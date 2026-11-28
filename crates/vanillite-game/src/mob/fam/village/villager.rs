//! Villagers (`Villager`): professions and levels, gossip and reputation, food and
//! breeding, sleeping, golem summoning and the brain set up of [`super::ai`].

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_data::item::ids as items;
use vanillite_nbt::{Compound, Tag};
use vanillite_world::entity::{Dimensions, EntityId, Pose};

use super::super::{FamilyHooks, Hurt, Interact, SpawnContext, SpawnReason};
use super::gossip::{GossipType, Gossips};
use super::{ai, merchant, state, state_mut};
use crate::effect::{EffectInstance, MobEffect};
use crate::item::ItemStack;
use crate::mob::animal::Interaction;
use crate::mob::brain::{self, Brain, MemValue, MemoryType as M, brain_ref};
use crate::mob::path::{MalusTable, PathType};
use crate::mob::{Kind, MobData, Tick, WorldAction};
use crate::player::encode::{DataWriter, serializer};
use crate::poi::PoiType;
use crate::survival::damage::DamageSource;
use crate::survival::rules::Difficulty;

/// The family's hooks for villagers.
#[derive(Debug)]
pub struct Hooks;

// --- Professions and types -------------------------------------------------------------

/// `VillagerProfession` registry ids (registration order).
pub mod profession {
    pub const NONE: u16 = 0;
    pub const ARMORER: u16 = 1;
    pub const FARMER: u16 = 5;
    pub const NITWIT: u16 = 11;
    pub const WEAPONSMITH: u16 = 14;
}

/// The profession name without namespace (`farmer`).
pub fn profession_name(p: u16) -> &'static str {
    let n = vanillite_data::registry::VILLAGER_PROFESSION
        .name(p)
        .unwrap_or("minecraft:none");
    n.strip_prefix("minecraft:").unwrap_or(n)
}

/// The job site of a working profession.
fn job_poi(p: u16) -> Option<PoiType> {
    match p {
        profession::NONE | profession::NITWIT => None,
        _ => PoiType::by_name(profession_name(p)),
    }
}

/// `VillagerProfession.heldJobSite().test(type)`.
pub fn held_job_site(p: u16, ty: PoiType) -> bool {
    job_poi(p) == Some(ty)
}

/// `VillagerProfession.acquirableJobSite().test(type)`: the unemployed take any job site.
pub fn acquirable_job_site(p: u16, ty: PoiType) -> bool {
    match p {
        profession::NONE => ty.is_job_site(),
        _ => held_job_site(p, ty),
    }
}

/// The first profession (registry order) whose job site is `ty`.
pub fn profession_of_job_site(ty: PoiType) -> Option<u16> {
    let n = u16::try_from(vanillite_data::registry::VILLAGER_PROFESSION.len()).unwrap_or(0);
    (0..n).find(|&p| held_job_site(p, ty))
}

/// `requestedItems`: farmers want wheat, seeds and bone meal.
pub fn requested_items(p: u16) -> &'static [u16] {
    const FARMER: &[u16] = &[
        items::WHEAT.0,
        items::WHEAT_SEEDS.0,
        items::BEETROOT_SEEDS.0,
        items::BONE_MEAL.0,
    ];
    if p == profession::FARMER { FARMER } else { &[] }
}

/// `secondaryPoi`: farmers work farmland.
pub fn secondary_poi(p: u16, state: BlockState) -> bool {
    p == profession::FARMER && state.block().name() == "minecraft:farmland"
}

/// `workSound`.
pub fn work_sound(p: u16) -> Option<&'static str> {
    Some(match profession_name(p) {
        "armorer" => "minecraft:entity.villager.work_armorer",
        "butcher" => "minecraft:entity.villager.work_butcher",
        "cartographer" => "minecraft:entity.villager.work_cartographer",
        "cleric" => "minecraft:entity.villager.work_cleric",
        "farmer" => "minecraft:entity.villager.work_farmer",
        "fisherman" => "minecraft:entity.villager.work_fisherman",
        "fletcher" => "minecraft:entity.villager.work_fletcher",
        "leatherworker" => "minecraft:entity.villager.work_leatherworker",
        "librarian" => "minecraft:entity.villager.work_librarian",
        "mason" => "minecraft:entity.villager.work_mason",
        "shepherd" => "minecraft:entity.villager.work_shepherd",
        "toolsmith" => "minecraft:entity.villager.work_toolsmith",
        "weaponsmith" => "minecraft:entity.villager.work_weaponsmith",
        _ => return None,
    })
}

/// `VillagerData.canLevelUp`.
pub fn can_level_up(level: i32) -> bool {
    (1..5).contains(&level)
}

/// `VillagerData.getMinXpPerLevel`.
pub fn min_xp_per_level(level: i32) -> i32 {
    const NEXT: [i32; 5] = [0, 10, 70, 150, 250];
    if can_level_up(level) {
        NEXT[usize::try_from(level - 1).unwrap_or(0)]
    } else {
        0
    }
}

/// `VillagerData.getMaxXpPerLevel`.
pub fn max_xp_per_level(level: i32) -> i32 {
    const NEXT: [i32; 5] = [0, 10, 70, 150, 250];
    if can_level_up(level) {
        NEXT[usize::try_from(level).unwrap_or(0)]
    } else {
        0
    }
}

fn type_id(name: &str) -> u16 {
    vanillite_data::registry::VILLAGER_TYPE
        .id(name)
        .unwrap_or(2)
}

/// The plains villager type id (`VillagerData.DEFAULT_TYPE`).
pub fn plains() -> u16 {
    type_id("minecraft:plains")
}

/// The biome name at `pos` (`level.getBiome(pos)`).
pub fn biome_at(level: &crate::level::Level, pos: BlockPos) -> String {
    level.climate().map_or_else(
        || "minecraft:plains".to_owned(),
        |c| c.biomes.get(level.biome(pos)).name.clone(),
    )
}

// --- State -------------------------------------------------------------------------------

/// `AcquirePoi.JitteredLinearRetry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    pub previous: i64,
    pub next: i64,
    pub delay: i32,
}

impl Retry {
    pub fn new(random: &mut impl RandomSource, at: i64) -> Retry {
        let mut r = Retry {
            previous: 0,
            next: 0,
            delay: 0,
        };
        r.mark(random, at);
        r
    }

    /// `markAttempt`.
    pub fn mark(&mut self, random: &mut impl RandomSource, at: i64) {
        self.previous = at;
        let suggested = self.delay + random.next_int_bounded(40) + 40;
        self.delay = suggested.min(400);
        self.next = at + i64::from(self.delay);
    }
}

/// The villager state (`Villager` fields besides the merchant ones), with what its
/// behaviours keep between ticks that does not fit their `Copy` values.
#[derive(Clone, Debug)]
pub struct Villager {
    /// `VillagerData`: type and profession registry ids, level (1 to 5).
    pub villager_type: u16,
    pub profession: u16,
    pub level: i32,
    /// `DATA_VILLAGER_DATA_FINALIZED`.
    pub data_finalized: bool,
    /// `foodLevel`.
    pub food_level: i32,
    pub gossips: Gossips,
    /// `lastGossipTime` and `lastGossipDecayTime`.
    pub last_gossip_time: i64,
    pub last_gossip_decay: i64,
    /// `SLEEPING_POS`.
    pub sleeping_pos: Option<BlockPos>,
    /// Was a baby when the brain was built (`ageBoundaryReached` rebuilds it).
    pub brain_baby: bool,
    /// `AcquirePoi` batch caches of the job site, bed and meeting point behaviours.
    pub acquire_cache: [Vec<(BlockPos, Retry)>; 3],
    /// `NearestBedSensor` and `SetClosestHomeAsWalkTarget` caches (position, expiry).
    pub bed_cache: Vec<(BlockPos, i64)>,
    pub bed_tried: i32,
    pub bed_last_update: i64,
    pub home_cache: Vec<(BlockPos, i64)>,
    pub home_last_update: i64,
    /// `HarvestFarmland.validFarmlandAroundVillager`.
    pub farmland: Vec<BlockPos>,
    /// `ShowTradesToPlayer`: the player's stack and the offers shown.
    pub shown_player_stack: Option<ItemStack>,
    pub display_items: Vec<ItemStack>,
    /// `TradeWithVillager.trades`.
    pub willing_trades: Vec<u16>,
    /// `SetHiddenState.ticksHidden`.
    pub ticks_hidden: i32,
    /// A player used the bed this villager sleeps in (`kickVillagerOutOfBed`): it wakes at
    /// its next tick.
    pub kicked_out: bool,
}

impl Default for Villager {
    fn default() -> Self {
        Villager {
            villager_type: plains(),
            profession: profession::NONE,
            level: 1,
            data_finalized: false,
            food_level: 0,
            gossips: Gossips::default(),
            last_gossip_time: 0,
            last_gossip_decay: 0,
            sleeping_pos: None,
            brain_baby: false,
            acquire_cache: Default::default(),
            bed_cache: Vec::new(),
            bed_tried: 0,
            bed_last_update: 0,
            home_cache: Vec::new(),
            home_last_update: 0,
            farmland: Vec::new(),
            shown_player_stack: None,
            display_items: Vec::new(),
            willing_trades: Vec::new(),
            ticks_hidden: 0,
            kicked_out: false,
        }
    }
}

/// The villager state of a mob.
pub fn vs(m: &MobData) -> &Villager {
    &state(m).villager
}

/// The villager state of a mob, mutably.
pub fn vs_mut(m: &mut MobData) -> &mut Villager {
    &mut state_mut(m).villager
}

/// Villagers have no goals.
#[derive(Clone, Copy, Debug)]
pub enum NoGoal {}

/// `setPathfindingMalus` of `AbstractVillager`.
const MALUS: &MalusTable = &[(PathType::FireInNeighbor, 16.0), (PathType::Fire, -1.0)];

// --- Inventory (`SimpleContainer`) --------------------------------------------------------

fn same(a: &ItemStack, b: &ItemStack) -> bool {
    a.same_item_same_components(b)
}

/// `SimpleContainer.canAddItem`.
pub fn can_add_item(inv: &[ItemStack; 8], stack: &ItemStack) -> bool {
    inv.iter()
        .any(|s| s.is_empty() || (same(s, stack) && s.count < crate::item::max_stack_size(s)))
}

/// `SimpleContainer.addItem`: fills stacks of the same item, then empty slots; returns
/// what did not fit.
pub fn add_item(inv: &mut [ItemStack; 8], stack: &ItemStack) -> ItemStack {
    let mut rest = stack.clone();
    if rest.is_empty() {
        return rest;
    }
    for s in inv.iter_mut() {
        if !s.is_empty() && same(s, &rest) {
            let max = crate::item::max_stack_size(s);
            let moved = rest.count.min(max - s.count).max(0);
            if moved > 0 {
                s.grow(moved);
                rest.shrink(moved);
                if rest.is_empty() {
                    return rest;
                }
            }
        }
    }
    for s in inv.iter_mut() {
        if s.is_empty() {
            *s = rest;
            return ItemStack::EMPTY;
        }
    }
    rest
}

/// `SimpleContainer.countItem`.
pub fn count_item(inv: &[ItemStack; 8], item: u16) -> i32 {
    inv.iter()
        .filter(|s| s.item == item && !s.is_empty())
        .map(|s| s.count)
        .sum()
}

/// `SimpleContainer.removeItemType(item, count)`.
pub fn remove_item_type(inv: &mut [ItemStack; 8], item: u16, count: i32) -> i32 {
    let mut left = count;
    for s in inv.iter_mut().rev() {
        if s.is_empty() || s.item != item {
            continue;
        }
        let take = left.min(s.count);
        s.shrink(take);
        left -= take;
        if s.is_empty() {
            *s = ItemStack::EMPTY;
        }
        if left == 0 {
            break;
        }
    }
    count - left
}

/// `VILLAGER_FOOD` nutrition of a stack: its own (patched or removed) component, else the
/// item's default.
pub fn stack_villager_food(stack: &ItemStack) -> Option<i32> {
    use vanillite_protocol::components::ids::VILLAGER_FOOD;
    if stack.is_empty() {
        return None;
    }
    if let Some(p) = stack.patch() {
        if let Some(v) = p.get(VILLAGER_FOOD) {
            return vanillite_protocol::codec::Reader::new(v).varint().ok();
        }
        if p.is_removed(VILLAGER_FOOD) {
            return None;
        }
    }
    villager_food(stack.item)
}

/// `VILLAGER_FOOD` nutrition of an item (bread 4, potato, carrot and beetroot 1).
pub fn villager_food(item: u16) -> Option<i32> {
    match item {
        i if i == items::BREAD.0 => Some(4),
        i if i == items::POTATO.0 || i == items::CARROT.0 || i == items::BEETROOT.0 => Some(1),
        _ => None,
    }
}

fn item_in_tag(item: u16, tag: &str) -> bool {
    crate::player::play::game_data().is_some_and(|d| d.item_in_tag(item, tag))
}

/// `countFoodPointsInInventory`.
pub fn food_points(m: &MobData) -> i32 {
    state(m)
        .merchant
        .inventory
        .iter()
        .filter_map(|s| stack_villager_food(s).map(|n| n * s.count))
        .sum()
}

/// `hasExcessFood`.
pub fn has_excess_food(m: &MobData) -> bool {
    food_points(m) >= 24
}

/// `wantsMoreFood`.
pub fn wants_more_food(m: &MobData) -> bool {
    food_points(m) < 12
}

/// `hasFarmSeeds`.
pub fn has_farm_seeds(m: &MobData) -> bool {
    state(m)
        .merchant
        .inventory
        .iter()
        .any(|s| !s.is_empty() && item_in_tag(s.item, "minecraft:villager_plantable_seeds"))
}

/// `canBreed`: enough food, awake and an adult without breeding cooldown.
pub fn can_breed(m: &MobData) -> bool {
    vs(m).food_level + food_points(m) >= 12 && vs(m).sleeping_pos.is_none() && m.age == 0
}

/// `eatAndDigestFood`: eat until full (12), then digest 12.
pub fn eat_and_digest(m: &mut MobData) {
    let s = state_mut(m);
    if s.villager.food_level < 12 {
        'slots: for slot in 0..8 {
            let stack = &s.merchant.inventory[slot];
            let Some(n) = stack_villager_food(stack) else {
                continue;
            };
            let count = stack.count;
            let mut to_remove = 0;
            for _ in 0..count {
                s.villager.food_level += n;
                to_remove += 1;
                if s.villager.food_level >= 12 {
                    s.merchant.inventory[slot].shrink(to_remove);
                    break 'slots;
                }
            }
            s.merchant.inventory[slot].shrink(to_remove);
        }
    }
    s.villager.food_level -= 12;
}

/// `wantsToPickUp`.
pub fn wants_to_pick_up(m: &MobData, stack: &ItemStack) -> bool {
    let item = stack.item;
    (item_in_tag(item, "minecraft:villager_picks_up")
        || stack_villager_food(stack).is_some()
        || requested_items(vs(m).profession).contains(&item))
        && can_add_item(&state(m).merchant.inventory, stack)
}

/// `InventoryCarrier.pickUpItem`.
fn pick_up(t: &mut Tick<'_, '_>, item: EntityId) -> bool {
    let Some(stack) = t.ents.item(item).map(|d| d.stack.clone()) else {
        return false;
    };
    if !wants_to_pick_up(t.mob_ref(), &stack) {
        return false;
    }
    let rest = add_item(&mut state_mut(t.mob()).merchant.inventory, &stack);
    crate::mob::equipment::take(t, item, stack.count - rest.count);
    true
}

// --- Brain --------------------------------------------------------------------------------

/// Builds the brain for the profession and age (`BRAIN_PROVIDER.makeBrain(body, packed)` and
/// `registerBrainGoals`), keeping the serializable memories of the old one (`pack`).
pub fn rebuild_brain(m: &mut MobData, time: Option<(i64, i64)>) {
    let mut packed = Compound::new();
    if let Some(b) = m.brain.as_deref() {
        brain::save(b, &mut packed);
    }
    let baby = m.baby;
    let profession = vs(m).profession;
    let mut b = Brain::new(ai::brain_spec(profession, baby), &mut m.random);
    brain::load(&mut b, &packed);
    b.schedule = Some(if baby {
        brain::Schedule::baby_villager()
    } else {
        brain::Schedule::villager()
    });
    if let Some((game_time, day_time)) = time {
        b.update_activity_from_schedule(game_time, day_time);
    }
    m.brain = Some(Box::new(b));
    vs_mut(m).brain_baby = baby;
}

/// `refreshBrain(level)`: stops every behaviour, then builds the brain again.
pub fn refresh_brain(t: &mut Tick<'_, '_>) {
    brain::stop_all(t);
    if !t.exists() {
        return;
    }
    let time = (t.game_time(), t.cx.env.day_time);
    rebuild_brain(t.mob(), Some(time));
}

/// `Brain.updateActivityFromSchedule` with the world's day.
pub fn update_activity_from_schedule(t: &mut Tick<'_, '_>) {
    let (game, day) = (t.game_time(), t.cx.env.day_time);
    brain::brain(t).update_activity_from_schedule(game, day);
}

/// `setVillagerData` with a profession change: the offers start over.
pub fn set_profession(m: &mut MobData, p: u16) {
    let s = state_mut(m);
    if s.villager.profession != p {
        s.merchant.offers = None;
    }
    s.villager.profession = p;
}

// --- Sounds, events -----------------------------------------------------------------------

/// `level.broadcastEntityEvent(entity, event)`.
pub fn entity_event(t: &mut Tick<'_, '_>, id: EntityId, event: i8) {
    crate::mob::living::entity_event(t, id, event);
}

/// `makeSound(sound)`.
pub fn make_sound(t: &mut Tick<'_, '_>, sound: &str) {
    let id = t.id;
    crate::mob::combat::mob_sound(t.ents, t.env, id, sound);
}

/// `playWorkSound`.
pub fn play_work_sound(t: &mut Tick<'_, '_>) {
    if let Some(s) = work_sound(vs(t.mob_ref()).profession) {
        make_sound(t, s);
    }
}

// --- Reputation ---------------------------------------------------------------------------

/// `ReputationEventType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReputationEvent {
    ZombieVillagerCured,
    GolemKilled,
    VillagerHurt,
    VillagerKilled,
    Trade,
}

/// `Villager.onReputationEventFrom(type, source)`.
pub fn on_reputation_event(m: &mut MobData, ev: ReputationEvent, source: u128) {
    if m.kind != Kind::Villager {
        return;
    }
    let g = &mut vs_mut(m).gossips;
    match ev {
        ReputationEvent::ZombieVillagerCured => {
            g.add(source, GossipType::MajorPositive, 20);
            g.add(source, GossipType::MinorPositive, 25);
        }
        ReputationEvent::Trade => g.add(source, GossipType::Trading, 2),
        ReputationEvent::VillagerHurt => g.add(source, GossipType::MinorNegative, 25),
        ReputationEvent::VillagerKilled => g.add(source, GossipType::MajorNegative, 25),
        ReputationEvent::GolemKilled => {}
    }
}

/// `ServerLevel.onReputationEvent(type, source, target)` for a target entity.
pub fn reputation_event(
    ents: &mut crate::entities::Entities,
    ev: ReputationEvent,
    source: EntityId,
    target: EntityId,
) {
    let Some(uuid) = ents.store.get(source).map(|e| e.uuid()) else {
        return;
    };
    if let Some(m) = ents.mob_mut(target) {
        on_reputation_event(m, ev, uuid);
    }
}

/// `Villager.getPlayerReputation`: the gossip about a player (`GossipContainer.getReputation`
/// over every type).
pub fn player_reputation(m: &MobData, player: u128) -> i32 {
    vs(m).gossips.reputation(player, |_| true)
}

/// `Villager.gossip(level, target, timestamp)`.
pub fn gossip(t: &mut Tick<'_, '_>, target: EntityId, now: i64) {
    let ok = |last: i64| now < last || now >= last + 1200;
    let mine = vs(t.mob_ref()).last_gossip_time;
    let Some(theirs) = t.ents.mob(target).map(|m| vs(m).last_gossip_time) else {
        return;
    };
    if !(ok(mine) && ok(theirs)) {
        return;
    }
    let Some(source) = t.ents.mob(target).map(|m| vs(m).gossips.clone()) else {
        return;
    };
    let m = t.mob();
    let mut random = std::mem::replace(&mut m.random, LegacyRandom::new(0));
    vs_mut(m).gossips.transfer_from(&source, &mut random, 10);
    m.random = random;
    vs_mut(m).last_gossip_time = now;
    if let Some(o) = t.ents.mob_mut(target) {
        vs_mut(o).last_gossip_time = now;
    }
    spawn_golem_if_needed(t, now, 5);
}

/// `maybeDecayGossip`.
fn maybe_decay_gossip(t: &mut Tick<'_, '_>) {
    let now = t.game_time();
    let v = vs_mut(t.mob());
    if v.last_gossip_decay == 0 {
        v.last_gossip_decay = now;
    } else if now >= v.last_gossip_decay + 24000 {
        v.gossips.decay();
        v.last_gossip_decay = now;
    }
}

// --- Golems -------------------------------------------------------------------------------

/// `golemSpawnConditionsMet` and no golem seen recently (`wantsToSpawnGolem`).
fn wants_to_spawn_golem(m: &MobData, game_time: i64) -> bool {
    let Some(b) = m.brain.as_deref() else {
        return false;
    };
    let slept = b.long(M::LastSlept).is_some_and(|s| game_time - s < 24000);
    slept && !b.has(M::GolemDetectedRecently)
}

/// `GolemSensor.golemDetected`.
pub fn golem_detected(m: &mut MobData) {
    if let Some(b) = m.brain.as_deref_mut() {
        b.set_with_expiry(M::GolemDetectedRecently, MemValue::Bool(true), 599);
    }
}

/// `spawnGolemIfNeeded(level, timestamp, villagersNeededToAgree)`.
pub fn spawn_golem_if_needed(t: &mut Tick<'_, '_>, _now: i64, needed: usize) {
    let game_time = t.game_time();
    if !wants_to_spawn_golem(t.mob_ref(), game_time) {
        return;
    }
    let area = t.aabb().inflate(10.0, 10.0, 10.0);
    let mut nearby = Vec::new();
    let ents = &*t.ents;
    ents.store.for_each_in(&area, false, |id| {
        if ents.mob(id).is_some_and(|m| m.kind == Kind::Villager) {
            nearby.push(id);
        }
    });
    let agreeing = nearby
        .iter()
        .filter(|&&id| {
            t.ents
                .mob(id)
                .is_some_and(|m| wants_to_spawn_golem(m, game_time))
        })
        .take(5)
        .count();
    if agreeing < needed {
        return;
    }
    if try_spawn_golem(t) {
        for id in nearby {
            if let Some(m) = t.ents.mob_mut(id) {
                golem_detected(m);
            }
        }
    }
}

/// `SpawnUtil.Strategy.LEGACY_IRON_GOLEM`.
fn legacy_golem_can_spawn_on(state: BlockState, above: BlockState) -> bool {
    let name = state.block().name();
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    let excluded = matches!(
        name,
        "cobweb"
            | "cactus"
            | "glass_pane"
            | "conduit"
            | "ice"
            | "tnt"
            | "glowstone"
            | "beacon"
            | "sea_lantern"
            | "frosted_ice"
            | "tinted_glass"
            | "glass"
    ) || name.ends_with("stained_glass_pane")
        || name.ends_with("stained_glass")
        || name.ends_with("leaves");
    if excluded {
        return false;
    }
    (above.is_air() || !above.fluid_state().is_empty())
        && (crate::mob::nav::is_solid(state) || name == "powder_snow")
}

/// `SpawnUtil.trySpawnMob(IRON_GOLEM, MOB_SUMMONED, level, pos, 10, 8, 6, LEGACY_IRON_GOLEM,
/// false)`.
fn try_spawn_golem(t: &mut Tick<'_, '_>) -> bool {
    let start = t.block_pos();
    for _ in 0..10 {
        let dx = t.env.random.next_int_between_inclusive(-8, 8);
        let dz = t.env.random.next_int_between_inclusive(-8, 8);
        let mut pos = start.offset(dx, 6, dz);
        let mut above = t.level.block(pos);
        let mut found = false;
        for _ in (-6..=6).rev() {
            pos = pos.offset(0, -1, 0);
            let current = t.level.block(pos);
            if legacy_golem_can_spawn_on(current, above) {
                pos = pos.offset(0, 1, 0);
                found = true;
                break;
            }
            above = current;
        }
        if !found {
            continue;
        }
        let mut mob = MobData::new(Kind::IronGolem, crate::mob::new_entity_seed());
        // `EntityType.create`: `snapTo` with a random yaw before `finalizeSpawn`.
        let yaw = vanillite_base::math::wrap_degrees_f(t.env.random.next_float() * 360.0);
        mob.y_body_rot = yaw;
        let at = Vec3::new(
            f64::from(pos.x) + 0.5,
            f64::from(pos.y),
            f64::from(pos.z) + 0.5,
        );
        let env = t.cx.env;
        let difficulty = t.env.difficulty;
        let extra = crate::mob::spawn::finalize(
            t.level,
            None,
            &env,
            &mut mob,
            pos,
            difficulty,
            0,
            SpawnReason::MobSummoned,
            t.env.random,
        );
        let rules_ok = crate::mob::fam::check_spawn_rules(
            Kind::IronGolem,
            t.level,
            &env,
            pos,
            t.env.random,
            SpawnReason::MobSummoned,
        );
        if !rules_ok || !crate::mob::spawn::check_spawn_obstruction(t.level, &env, &mob, at) {
            continue;
        }
        let uuid = crate::mob::new_uuid();
        let y_rot = mob.y_body_rot;
        let id = crate::mob::spawn(
            t.ents,
            t.level.network_ids(),
            uuid,
            at,
            vanillite_world::entity::Rotation {
                y_rot,
                x_rot: 0.0,
                y_head_rot: y_rot,
            },
            mob,
        );
        if !t
            .ents
            .spawned(id, vanillite_abi::spawn_reason::MOB_SUMMONED, 0)
        {
            return true;
        }
        crate::mob::fam::undead::spawn_extras(
            t.ents,
            t.level,
            id,
            at,
            extra,
            SpawnReason::MobSummoned,
        );
        if let Some(sound) = t
            .ents
            .mob_mut(id)
            .and_then(crate::mob::kinds::ambient_sound)
        {
            crate::mob::combat::mob_sound(t.ents, t.env, id, sound);
        }
        return true;
    }
    false
}

// --- Sleeping -----------------------------------------------------------------------------

/// `#villagers_can_sleep_on_bed` (the beds).
pub fn is_villager_bed(state: BlockState) -> bool {
    state.block().name().ends_with("_bed")
}

/// `LivingEntity.startSleeping(bedPos)`.
pub fn start_sleeping(t: &mut Tick<'_, '_>, bed: BlockPos) -> bool {
    let state = t.level.block(bed);
    if !is_villager_bed(state) {
        return false;
    }
    let Some(h) = crate::block::bed::sleep_height(t.level, state, bed) else {
        return false;
    };
    if t.ents.riding.vehicle_of(t.id).is_some() {
        let id = t.id;
        crate::riding::stop_riding(t.ents, t.env, id);
    }
    let pos = Vec3::new(
        f64::from(bed.x) + 0.5,
        f64::from(bed.y) + h + 0.125,
        f64::from(bed.z) + 0.5,
    );
    if let Some(occupied) = state.with("occupied", "true") {
        t.cx.actions.push(WorldAction::SetBlock(
            bed,
            occupied,
            crate::level::update::ALL,
        ));
    }
    vs_mut(t.mob()).sleeping_pos = Some(bed);
    let dims = crate::mob::kinds::mob_dimensions(t.mob_ref());
    let mut e = t.core_mut();
    e.set_pos(pos);
    e.set_pose(Pose::Sleeping);
    e.set_dims(dims);
    e.set_velocity(Vec3::ZERO);
    e.mark_data_dirty();
    true
}

/// `LivingEntity.stopSleeping`.
pub fn stop_sleeping(t: &mut Tick<'_, '_>) {
    let Some(bed) = vs(t.mob_ref()).sleeping_pos else {
        return;
    };
    vs_mut(t.mob()).sleeping_pos = None;
    if t.level.is_loaded(bed) {
        let state = t.level.block(bed);
        if is_villager_bed(state) {
            if let Some(free) = state.with("occupied", "false") {
                t.cx.actions
                    .push(WorldAction::SetBlock(bed, free, crate::level::update::ALL));
            }
            let forward =
                <BlockState as vanillite_worldgen::behaviour::StateExt>::direction(state, "facing")
                    .unwrap_or(vanillite_base::Direction::North);
            let yaw = t.core().rot().y_rot;
            let dims = crate::mob::kinds::mob_dimensions(t.mob_ref());
            let stand =
                crate::block::bed::stand_up_position(t.level, bed, forward, yaw, dims, false)
                    .unwrap_or(Vec3::new(
                        f64::from(bed.x) + 0.5,
                        f64::from(bed.y) + 1.1,
                        f64::from(bed.z) + 0.5,
                    ));
            let look = Vec3::new(
                f64::from(bed.x) + 0.5 - stand.x,
                f64::from(bed.y) - stand.y,
                f64::from(bed.z) + 0.5 - stand.z,
            )
            .normalize();
            #[allow(clippy::cast_possible_truncation)]
            let yaw = vanillite_base::math::wrap_degrees_f(
                (crate::mob::atan2_deg(look.z, look.x) - 90.0) as f32,
            );
            let mut e = t.core_mut();
            e.set_pos(stand);
            let mut rot = e.as_ref().rot();
            rot.y_rot = yaw;
            rot.x_rot = 0.0;
            e.set_rot(rot);
        }
    }
    let dims = crate::mob::kinds::mob_dimensions(t.mob_ref());
    let mut e = t.core_mut();
    e.set_pose(Pose::Standing);
    e.set_dims(dims);
    e.mark_data_dirty();
    // `Villager.stopSleeping`.
    let now = t.game_time();
    brain::brain(t).set(M::LastWoken, MemValue::Long(now));
}

/// `isSleeping`.
pub fn is_sleeping(m: &MobData) -> bool {
    vs(m).sleeping_pos.is_some()
}

/// A villager asleep (`Villager` with `isSleeping`).
pub fn is_villager_sleeping(m: &MobData) -> bool {
    m.kind == Kind::Villager && is_sleeping(m)
}

/// `kickVillagerOutOfBed`: the villager wakes at its next tick.
pub fn kick_out(m: &mut MobData) {
    if m.kind == Kind::Villager {
        vs_mut(m).kicked_out = true;
    }
}

// --- POI ----------------------------------------------------------------------------------

/// `POI_MEMORIES` test of a memory and POI type.
fn poi_memory_matches(m: &MobData, memory: M, ty: PoiType) -> bool {
    match memory {
        M::Home => ty == PoiType::Home,
        M::JobSite => held_job_site(vs(m).profession, ty),
        M::PotentialJobSite => ty.is_job_site(),
        M::MeetingPoint => ty == PoiType::Meeting,
        _ => false,
    }
}

/// `releasePoi(memoryType)`: frees the ticket of the remembered POI.
pub fn release_poi(t: &mut Tick<'_, '_>, memory: M) {
    let Some(MemValue::GlobalPos(g)) = brain_ref(t).get(memory).cloned() else {
        return;
    };
    if *g.dimension != *t.env.dimension {
        return;
    }
    if let Some(ty) = t.ents.poi.get_type(g.pos)
        && poi_memory_matches(t.mob_ref(), memory, ty)
    {
        t.ents.poi.release(g.pos);
    }
}

/// `releaseAllPois`.
pub fn release_all_pois(t: &mut Tick<'_, '_>) {
    for m in [M::Home, M::JobSite, M::PotentialJobSite, M::MeetingPoint] {
        release_poi(t, m);
    }
}

// --- Conversions ----------------------------------------------------------------------------

/// `Zombie.killedEntity` → `convertVillagerToZombieVillager`: a villager killed by a zombie
/// on Normal (half of the time) or Hard. Returns whether it converted.
fn killed_by_zombie(t: &mut Tick<'_, '_>, killer: EntityId) -> bool {
    let difficulty = t.env.difficulty;
    if !matches!(difficulty, Difficulty::Normal | Difficulty::Hard) {
        return false;
    }
    let Some(km) = t.ents.mob(killer) else {
        return false;
    };
    if !crate::mob::fam::undead::zombies::is_zombie(km.kind) {
        return false;
    }
    if difficulty != Difficulty::Hard
        && t.ents
            .mob_mut(killer)
            .is_some_and(|m| m.random.next_boolean())
    {
        return false;
    }
    let killer_silent = t.ents.mob(killer).is_some_and(|m| m.silent);
    let killer_pos = t.ents.store.get(killer).map(|e| e.pos());
    let v = vs(t.mob_ref()).clone();
    let xp = state(t.mob_ref()).merchant.villager_xp;
    // `villager.getOffers().copy()`: offers nobody looked at yet are generated first.
    let id = t.id;
    super::merchant::ensure_offers_in(t.level, t.ents, id);
    let offers = state(t.mob_ref())
        .merchant
        .offers
        .clone()
        .unwrap_or_default();
    let env = t.cx.env;
    let pos = t.block_pos();
    let mut random = std::mem::replace(t.env.random, LegacyRandom::new(0));
    let level = t.level;
    let converted = crate::mob::fam::undead::convert_to(
        t,
        Kind::ZombieVillager,
        true,
        true,
        vanillite_abi::transform_reason::INFECTION,
        |z| {
            crate::mob::fam::undead::zombies::set_villager_data(
                z,
                v.villager_type,
                v.profession,
                v.level,
                v.data_finalized,
            );
            let mut ctx = SpawnContext::simple(difficulty, SpawnReason::Conversion);
            ctx.env = env;
            ctx.pos = pos;
            ctx.level = Some(level);
            crate::mob::fam::undead::zombies::finalize_converted(z, &mut ctx, &mut random);
            crate::mob::fam::undead::zombies::set_villager_data(
                z,
                v.villager_type,
                v.profession,
                v.level,
                v.data_finalized,
            );
            crate::mob::fam::undead::zombies::set_villager_trades(z, v.gossips.clone(), offers, xp);
        },
    );
    *t.env.random = random;
    if converted.is_some()
        && !killer_silent
        && let Some(p) = killer_pos
    {
        t.cx.actions.push(WorldAction::LevelEvent(
            1026,
            BlockPos::containing(p.x, p.y, p.z),
            0,
        ));
    }
    converted.is_some()
}

/// `thunderHit`: a witch outside peaceful.
fn struck_by_lightning(t: &mut Tick<'_, '_>) -> bool {
    let difficulty = t.env.difficulty;
    if difficulty == Difficulty::Peaceful {
        return false;
    }
    let env = t.cx.env;
    let pos = t.block_pos();
    let mut random = std::mem::replace(t.env.random, LegacyRandom::new(0));
    let level = t.level;
    let id = crate::mob::fam::undead::convert_to(
        t,
        Kind::Witch,
        false,
        false,
        vanillite_abi::transform_reason::LIGHTNING,
        |w| {
            crate::mob::spawn::finalize(
                level,
                None,
                &env,
                w,
                pos,
                difficulty,
                0,
                SpawnReason::Conversion,
                &mut random,
            );
            w.persistent = true;
        },
    );
    *t.env.random = random;
    if id.is_some() {
        // `releaseAllPois` in the conversion callback: not when a plugin cancelled it.
        release_all_pois(t);
    }
    id.is_some()
}

// --- Hooks --------------------------------------------------------------------------------

impl FamilyHooks for Hooks {
    type Goal = NoGoal;

    fn is_monster(_kind: Kind) -> bool {
        false
    }
    fn ageable(_kind: Kind) -> bool {
        true
    }
    fn ambient_sound_interval(_kind: Kind) -> i32 {
        80
    }
    fn remove_when_far_away(_kind: Kind) -> bool {
        false
    }
    fn remove_when_far_away_mob(_m: &MobData, _dist_sqr: f64) -> bool {
        false
    }
    fn malus(_kind: Kind) -> &'static MalusTable {
        MALUS
    }
    fn ambient_sound(m: &mut MobData) -> Option<&'static str> {
        if is_sleeping(m) {
            None
        } else if state(m).merchant.trading_player.is_some() {
            Some("minecraft:entity.villager.trade")
        } else {
            Some("minecraft:entity.villager.ambient")
        }
    }
    fn experience_reward(_m: &mut MobData) -> i32 {
        0
    }
    fn dimensions(m: &MobData) -> Option<Dimensions> {
        if is_sleeping(m) {
            return Some(Dimensions {
                width: 0.2,
                height: 0.2,
            });
        }
        m.baby.then_some(Dimensions {
            width: 0.49,
            height: 0.98,
        })
    }

    fn init(m: &mut MobData) {
        state_mut(m);
        m.nav.can_open_doors = true;
        m.nav.can_float = true;
        let follow = m.attrs.follow_range;
        m.nav.set_required_path_length(48.0, follow);
        m.can_pick_up_loot = true;
        rebuild_brain(m, None);
    }

    /// `registerBrainGoals`: the brain made in the constructor (or from the saved one) takes
    /// the scheduled activity at once.
    fn created(m: &mut MobData, game_time: i64, day_time: i64) {
        if let Some(b) = m.brain.as_deref_mut() {
            b.update_activity_from_schedule(game_time, day_time);
        }
    }

    fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
        if ctx.reason == SpawnReason::Breeding {
            set_profession(m, profession::NONE);
        }
        // `finalizeVillagerType`.
        if !vs(m).data_finalized {
            let biome = ctx.biome.map_or("minecraft:plains", |b| &*b.name);
            let ty = crate::mob::fam::undead::zombies::villager_type_of_biome(biome);
            let v = vs_mut(m);
            v.villager_type = type_id(ty);
            v.data_finalized = true;
        }
    }

    fn walk_target_value(
        _kind: Kind,
        _level: &crate::level::Level,
        _env: &crate::mob::Environment,
        _pos: BlockPos,
    ) -> f64 {
        // `PathfinderMob.getWalkTargetValue`: villagers are no animals.
        0.0
    }

    fn before_tick(t: &mut Tick<'_, '_>) {
        // `LivingEntity.tick`: out of a bed that is gone; sleepers look straight.
        if let Some(bed) = vs(t.mob_ref()).sleeping_pos {
            let kicked = std::mem::take(&mut vs_mut(t.mob()).kicked_out);
            let state = t.level.block(bed);
            if kicked
                || !is_villager_bed(state)
                || crate::block::bed::sleep_height(t.level, state, bed).is_none()
            {
                stop_sleeping(t);
            } else {
                let mut e = t.core_mut();
                let mut rot = e.as_ref().rot();
                rot.x_rot = 0.0;
                e.set_rot(rot);
            }
        }
    }

    fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
        // `hurtServer`: a hit wakes a sleeper.
        if h.ents.mob(h.target).is_some_and(is_sleeping) {
            let mut t = crate::mob::fam::brainy::hurt_tick(h);
            stop_sleeping(&mut t);
        }
        Some(amount)
    }

    fn after_tick(t: &mut Tick<'_, '_>) {
        // `Villager.tick` after `super.tick()`.
        let s = state_mut(t.mob());
        if s.merchant.unhappy_counter > 0 {
            s.merchant.unhappy_counter -= 1;
            t.core_mut().mark_data_dirty();
        }
        maybe_decay_gossip(t);
    }

    fn ai_step(t: &mut Tick<'_, '_>) {
        // `ageBoundaryReached` → `refreshBrain`.
        if t.mob_ref().baby != vs(t.mob_ref()).brain_baby {
            refresh_brain(t);
        }
    }

    fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
        brain::tick(t);
        if !t.exists() || t.mob_ref().removed {
            return;
        }
        if let Some(player) = state_mut(t.mob()).merchant.last_traded_player.take() {
            let me = t.id;
            reputation_event(t.ents, ReputationEvent::Trade, player, me);
            entity_event(t, me, 14);
        }
        if !t.mob_ref().no_ai && t.mob().random.next_int_bounded(100) == 0 {
            let pos = t.block_pos();
            if t.ents
                .raids
                .raid_at(pos)
                .is_some_and(|r| r.is_active() && !r.is_over())
            {
                let me = t.id;
                entity_event(t, me, 42);
            }
        }
        let s = &state(t.mob_ref()).merchant;
        if s.offers.as_ref().is_none_or(|o| o.0.is_empty()) && s.trading_player.is_some() {
            merchant::stop_trading(t);
        }
    }

    fn thunder_hit(h: &mut Hurt<'_, '_>) -> bool {
        let mut t = crate::mob::fam::brainy::hurt_tick(h);
        struck_by_lightning(&mut t)
    }

    fn after_hurt(h: &mut Hurt<'_, '_>, _amount: f32) {
        // `setLastHurtByMob`: the gossip about the attacker, angry particles for players.
        let Some(cause) = h.source.cause else {
            return;
        };
        let living = h.ents.mob(cause).is_some() || h.ents.player(cause).is_some();
        if !living {
            return;
        }
        let target = h.target;
        reputation_event(h.ents, ReputationEvent::VillagerHurt, cause, target);
        let alive = h.ents.mob(target).is_some_and(MobData::is_alive);
        if alive && h.ents.player(cause).is_some() {
            crate::mob::animal::entity_event(h.ents, h.env, target, 13);
        }
    }

    fn on_die(h: &mut Hurt<'_, '_>) {
        let source: DamageSource = h.source;
        let mut t = crate::mob::fam::brainy::hurt_tick(h);
        // `tellWitnessesThatIWasMurdered`.
        if let Some(murderer) = source.cause {
            let mut witnesses = Vec::new();
            crate::mob::brain::sensing::visible_find_all(
                &mut t,
                |t, id| t.ents.mob(id).is_some_and(|m| m.kind == Kind::Villager),
                &mut witnesses,
            );
            for w in witnesses {
                reputation_event(t.ents, ReputationEvent::VillagerKilled, murderer, w);
            }
        }
        release_all_pois(&mut t);
        stop_sleeping(&mut t);
        merchant::stop_trading(&mut t);
        if let Some(killer) = source.cause {
            killed_by_zombie(&mut t, killer);
        }
    }

    fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
        merchant::interact(ix)
    }

    fn encode_data(w: &mut DataWriter<'_>, m: &MobData) {
        let s = state(m);
        let v = &s.villager;
        w.boolean(16, m.baby, false);
        w.boolean(17, m.age_locked, false);
        w.int(18, s.merchant.unhappy_counter, 0);
        let default =
            v.villager_type == plains() && v.profession == profession::NONE && v.level == 1;
        w.raw(19, serializer::VILLAGER_DATA, default, |o| {
            vanillite_protocol::codec::write_varint(o, i32::from(v.villager_type));
            vanillite_protocol::codec::write_varint(o, i32::from(v.profession));
            vanillite_protocol::codec::write_varint(o, v.level);
        });
        w.boolean(20, v.data_finalized, false);
        w.optional_block_pos(14, v.sleeping_pos);
    }

    fn pick_up_items(t: &mut Tick<'_, '_>) {
        let rules = crate::mob::equipment::PickupRules {
            wants: wants_to_pick_up,
            preferred_weapon: None,
        };
        crate::mob::equipment::pick_up_items(t, &rules, Some(pick_up), (1.0, 0.0, 1.0));
    }

    fn wants_to_pick_up(t: &Tick<'_, '_>, stack: &ItemStack) -> bool {
        wants_to_pick_up(t.mob_ref(), stack)
    }

    fn save(m: &MobData, out: &mut Compound) {
        out.insert("Age", m.age);
        out.insert("ForcedAge", m.forced_age);
        out.insert("AgeLocked", m.age_locked);
        merchant::save(&state(m).merchant, out);
        let v = vs(m);
        let name = |r: &vanillite_data::registry::Registry, id: u16| {
            r.name(id).unwrap_or("minecraft:none").to_owned()
        };
        out.insert(
            "VillagerData",
            Compound::new()
                .with(
                    "type",
                    name(&vanillite_data::registry::VILLAGER_TYPE, v.villager_type),
                )
                .with(
                    "profession",
                    name(&vanillite_data::registry::VILLAGER_PROFESSION, v.profession),
                )
                .with("level", v.level),
        );
        out.insert("VillagerDataFinalized", v.data_finalized);
        #[allow(clippy::cast_possible_truncation)]
        out.insert("FoodLevel", Tag::Byte(v.food_level as i8));
        out.insert("Gossips", v.gossips.save());
        let merchant = &state(m).merchant;
        out.insert("Xp", merchant.villager_xp);
        out.insert("LastRestock", merchant.last_restock);
        out.insert("LastGossipDecay", v.last_gossip_decay);
        out.insert("RestocksToday", merchant.restocks_today);
        if let Some(p) = v.sleeping_pos {
            out.insert("sleeping_pos", Tag::IntArray(vec![p.x, p.y, p.z]));
        }
        if let Some(b) = m.brain.as_deref() {
            brain::save(b, out);
        }
    }

    fn load(m: &mut MobData, tag: &Compound) {
        let int = |k: &str| crate::mob::entity_nbt::get_int(tag, k);
        let long = |k: &str| tag.get(k).and_then(crate::mob::entity_nbt::long_value);
        if let Some(age) = int("Age") {
            m.age = age;
            m.baby = age < 0;
        }
        m.age_locked = crate::mob::entity_nbt::get_bool(tag, "AgeLocked").unwrap_or(false);
        if let Some(f) = int("ForcedAge") {
            m.forced_age = f;
        }
        merchant::load(&mut state_mut(m).merchant, tag);
        let finalized =
            crate::mob::entity_nbt::get_bool(tag, "VillagerDataFinalized").unwrap_or(false);
        let data = tag.get("VillagerData").and_then(Tag::as_compound);
        if finalized || data.is_some() {
            let mut ty = plains();
            let mut prof = profession::NONE;
            let mut level = 1;
            if let Some(d) = data {
                if let Some(t) = d
                    .get("type")
                    .and_then(Tag::as_str)
                    .and_then(|n| vanillite_data::registry::VILLAGER_TYPE.id(n))
                {
                    ty = t;
                }
                if let Some(p) = d
                    .get("profession")
                    .and_then(Tag::as_str)
                    .and_then(|n| vanillite_data::registry::VILLAGER_PROFESSION.id(n))
                {
                    prof = p;
                }
                if let Some(l) = crate::mob::entity_nbt::get_int(d, "level") {
                    level = l.max(1);
                }
            }
            let v = vs_mut(m);
            v.data_finalized = true;
            v.villager_type = ty;
            v.profession = prof;
            v.level = level;
        }
        let v = vs_mut(m);
        v.food_level = int("FoodLevel").unwrap_or(0);
        v.gossips = Gossips::load(tag.get("Gossips"));
        v.last_gossip_decay = long("LastGossipDecay").unwrap_or(0);
        if let Some(Tag::IntArray(p)) = tag.get("sleeping_pos")
            && p.len() == 3
        {
            v.sleeping_pos = Some(BlockPos::new(p[0], p[1], p[2]));
        }
        let merchant = &mut state_mut(m).merchant;
        merchant.villager_xp = int("Xp").unwrap_or(0);
        merchant.last_restock = long("LastRestock").unwrap_or(0);
        merchant.restocks_today = int("RestocksToday").unwrap_or(0);
        rebuild_brain(m, None);
        if let Some(b) = m.brain.as_deref_mut() {
            brain::load(b, tag);
        }
    }
}

/// `Villager.getBreedOffspring(level, this)` of a spawn egg used on `parent` at `pos`: the
/// biome's type, else the parent's.
pub fn egg_offspring(
    level: &crate::level::Level,
    parent: &mut MobData,
    pos: BlockPos,
    seed: i64,
) -> MobData {
    let roll = parent.random.next_double();
    let ty = if roll < 0.5 {
        type_id(crate::mob::fam::undead::zombies::villager_type_of_biome(
            &biome_at(level, pos),
        ))
    } else {
        vs(parent).villager_type
    };
    let mut child = MobData::new(Kind::Villager, seed);
    {
        let v = vs_mut(&mut child);
        v.villager_type = ty;
        v.profession = profession::NONE;
        v.data_finalized = true;
    }
    child.age = -24000;
    child.baby = true;
    rebuild_brain(&mut child, None);
    child
}

/// `getBreedOffspring` and `VillagerMakeLove.breed`: a baby villager of the biome's or a
/// parent's type next to `source`; returns it.
pub fn breed(t: &mut Tick<'_, '_>, partner: EntityId) -> Option<EntityId> {
    let roll = t.mob().random.next_double();
    let ty = if roll < 0.5 {
        type_id(crate::mob::fam::undead::zombies::villager_type_of_biome(
            &biome_at(t.level, t.block_pos()),
        ))
    } else if roll < 0.75 {
        vs(t.mob_ref()).villager_type
    } else {
        t.ents.mob(partner).map(|m| vs(m).villager_type)?
    };
    let mut child = MobData::new(Kind::Villager, crate::mob::new_entity_seed());
    {
        let v = vs_mut(&mut child);
        v.villager_type = ty;
        v.profession = profession::NONE;
        v.data_finalized = true;
    }
    t.mob().age = 6000;
    if let Some(p) = t.ents.mob_mut(partner) {
        p.age = 6000;
    }
    child.age = -24000;
    child.baby = true;
    rebuild_brain(&mut child, Some((t.game_time(), t.cx.env.day_time)));
    let pos = t.pos();
    let uuid = crate::mob::new_uuid();
    if let Some(hooks) = t.ents.hooks_for(vanillite_abi::event::ENTITY_BREED)
        && crate::mob::animal::breed_event(
            &*hooks,
            t.ents,
            (uuid, Kind::Villager),
            t.id,
            partner,
            0,
            0,
        )
        .is_none()
    {
        return None;
    }
    let id = crate::mob::spawn(
        t.ents,
        t.level.network_ids(),
        uuid,
        pos,
        vanillite_world::entity::Rotation::default(),
        child,
    );
    let parent = t.ents.store.get(t.id).map_or(0, |e| e.uuid());
    if !t
        .ents
        .spawned(id, vanillite_abi::spawn_reason::BREEDING, parent)
    {
        return None;
    }
    entity_event(t, id, 12);
    Some(id)
}

/// `BehaviorUtils.throwItem(villager, stack, target)`.
pub fn throw_item(t: &mut Tick<'_, '_>, stack: ItemStack, target: Vec3) {
    if stack.is_empty() {
        return;
    }
    crate::mob::brain::behavior::swing(t, false);
    let p = t.pos();
    let start = Vec3::new(p.x, t.eye_y() - 0.3, p.z);
    let aim = Vec3::new(target.x, target.y + 1.0, target.z);
    let d = Vec3::new(aim.x - p.x, aim.y - p.y, aim.z - p.z).normalize();
    let v = f64::from(0.3_f32);
    let velocity = Vec3::new(d.x * v, d.y * v, d.z * v);
    let (uuid, y_rot, _, item_random) = crate::mob::new_item_entity();
    t.env.out.push(crate::survival::Effect::DropItem {
        random: item_random,
        uuid,
        pos: start,
        velocity,
        y_rot,
        pickup_delay: 10,
        stack,
    });
}

/// The villager after a cure (`ZombieVillager.finishConversion`'s set up of the villager):
/// data, gossip, offers, experience, nausea.
#[allow(clippy::too_many_arguments)]
pub fn cured(
    m: &mut MobData,
    villager_type: u16,
    profession: u16,
    level: i32,
    finalized: bool,
    gossips: Gossips,
    offers: Option<crate::trading::MerchantOffers>,
    xp: i32,
) {
    let s = state_mut(m);
    s.villager.villager_type = villager_type;
    s.villager.profession = profession;
    s.villager.level = level;
    s.villager.data_finalized = finalized;
    s.villager.gossips.put_all(&gossips);
    s.merchant.offers = offers;
    s.merchant.villager_xp = xp;
    rebuild_brain(m, None);
    crate::effect::living::add_to_mob(m, EffectInstance::new(MobEffect::NAUSEA, 200, 0));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod villager_food_tests {
    use super::*;

    /// `itemStack.get(VILLAGER_FOOD)` (I6-35 of #272): the stack's own value counts.
    #[test]
    fn patched_villager_food() {
        use vanillite_protocol::components::ids::VILLAGER_FOOD;
        let mut bread = ItemStack::new(items::BREAD.0, 1);
        assert_eq!(stack_villager_food(&bread), Some(4));
        bread.patch_mut().set(VILLAGER_FOOD, vec![7u8].into());
        assert_eq!(stack_villager_food(&bread), Some(7));
        let mut potato = ItemStack::new(items::POTATO.0, 1);
        potato.remove_component(VILLAGER_FOOD);
        assert_eq!(stack_villager_food(&potato), None);
        let mut stick = ItemStack::new(items::STICK.0, 1);
        assert_eq!(stack_villager_food(&stick), None);
        stick.patch_mut().set(VILLAGER_FOOD, vec![2u8].into());
        assert_eq!(stack_villager_food(&stick), Some(2));
    }
}
