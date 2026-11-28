//! The zombie variants (`Zombie` subclasses): husks, drowned, zombie villagers, zombified
//! piglins, and giants. The classic zombie shares the finalization, the attributes, the
//! reinforcements and the drowning conversion from here.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::item::ids as items;
use vanillite_nbt::{Compound, Tag};
use vanillite_world::entity::{Dimensions, EntityFlags, EntityId};

use super::{Goal, HurtBy, Melee, State, goal, level_event, sound, state, state_mut};
use crate::effect as effects;
use crate::item::{EquipmentSlot, ItemStack};
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::common::{self, CommonGoal, MoveToBlock};
use crate::mob::equipment::{self, PickupRules};
use crate::mob::fam::{ExtraMob, GroupData, Hurt, Interact, Relation, SpawnContext, SpawnReason};
use crate::mob::goals::{self, MeleeKind};
use crate::mob::nav::{self, MoveKind, NavKind};
use crate::mob::{Environment, Kind, MobData, Tick, kinds, randompos};
use crate::player::encode::{DataWriter, serializer};
use crate::survival::rules::{Difficulty, Rule};

/// The state of a zombie (any `Zombie` subclass, the classic one included).
#[derive(Clone, Debug, Default)]
pub struct ZombieState {
    /// `SPAWN_REINFORCEMENTS_CHANCE` with its modifiers.
    pub reinforcement: f64,
    /// The drowning conversion (`ConversionTracker`): ticks with the eyes in water, the
    /// conversion countdown and whether it converts (`DATA_DROWNED_CONVERSION_ID`).
    pub in_water_time: i32,
    pub conversion_time: i32,
    pub converting: bool,
    /// `ZombieVillager`: the villager data (type and profession registry ids, level),
    /// whether it is final, the curing countdown, who started it and the villager's xp.
    pub villager_type: u16,
    pub profession: u16,
    pub villager_level: i32,
    pub data_finalized: bool,
    pub curing: bool,
    pub cure_time: i32,
    pub cure_starter: Option<u128>,
    pub villager_xp: i32,
    /// `ZombieVillager.gossips` and `tradeOffers` of the villager it was.
    pub gossips: Option<crate::mob::fam::village::gossip::Gossips>,
    pub offers: Option<crate::trading::MerchantOffers>,
    /// `ZombifiedPiglin`: the first anger sound, the alert interval and whether the
    /// attacking speed bonus is on.
    pub first_anger_sound_in: i32,
    pub next_alert: i32,
    pub speed_bonus: bool,
    /// `Drowned.searchingForLand`.
    pub searching_for_land: bool,
    /// `MoveThroughVillageGoal`: the village points of interest it reached (`visited`) and
    /// the path `canUse` made for `start` (boxed: only zombies near villages have it).
    pub village: Option<Box<VillageWalk>>,
}

/// The state of `MoveThroughVillageGoal`.
#[derive(Clone, Debug, Default)]
pub struct VillageWalk {
    pub visited: Vec<BlockPos>,
    pub path: Option<crate::mob::path::Path>,
}

fn village_mut(m: &mut MobData) -> &mut VillageWalk {
    zs_mut(m).village.get_or_insert_with(Box::default)
}

/// Every `Zombie` (the classic one too).
pub fn is_zombie(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Zombie | Kind::Husk | Kind::Drowned | Kind::ZombieVillager | Kind::ZombifiedPiglin
    )
}

/// The zombie state, created on first use.
pub fn zs_mut(m: &mut MobData) -> &mut ZombieState {
    let s = state_mut(m);
    if !matches!(s, State::Zombie(_)) {
        *s = State::Zombie(ZombieState::default());
    }
    match s {
        State::Zombie(z) => z,
        _ => unreachable!("the zombie state was just set"),
    }
}

/// The zombie state, read only.
pub fn zs(m: &MobData) -> Option<&ZombieState> {
    match state(m) {
        State::Zombie(z) => Some(z),
        _ => None,
    }
}

/// `SPEED_MODIFIER_BABY`: +50 % of the base speed.
pub fn baby_speed(m: &mut MobData, baby: bool) {
    let base = crate::mob::Attrs::of(m.kind.type_id()).movement_speed * 0.5;
    if baby {
        m.attrs.movement_speed += base;
    } else {
        m.attrs.movement_speed -= base;
    }
}

// --- Properties -------------------------------------------------------------------------

pub fn ambient_sound(m: &MobData) -> Option<&'static str> {
    Some(match m.kind {
        Kind::Husk => "minecraft:entity.husk.ambient",
        Kind::Drowned if m.fluids.in_water() => "minecraft:entity.drowned.ambient_water",
        Kind::Drowned => "minecraft:entity.drowned.ambient",
        Kind::ZombieVillager => "minecraft:entity.zombie_villager.ambient",
        // `isAngry`: an anger end time is set (it is reset when the anger ends).
        Kind::ZombifiedPiglin if m.anger_end > 0 => "minecraft:entity.zombified_piglin.angry",
        Kind::ZombifiedPiglin => "minecraft:entity.zombified_piglin.ambient",
        _ => return None,
    })
}

pub fn hurt_sound(m: &MobData) -> &'static str {
    match m.kind {
        Kind::Husk => "minecraft:entity.husk.hurt",
        Kind::Drowned if m.fluids.in_water() => "minecraft:entity.drowned.hurt_water",
        Kind::Drowned => "minecraft:entity.drowned.hurt",
        Kind::ZombieVillager => "minecraft:entity.zombie_villager.hurt",
        Kind::ZombifiedPiglin => "minecraft:entity.zombified_piglin.hurt",
        _ => "minecraft:entity.generic.hurt",
    }
}

pub fn death_sound(m: &MobData) -> &'static str {
    match m.kind {
        Kind::Husk => "minecraft:entity.husk.death",
        Kind::Drowned if m.fluids.in_water() => "minecraft:entity.drowned.death_water",
        Kind::Drowned => "minecraft:entity.drowned.death",
        Kind::ZombieVillager => "minecraft:entity.zombie_villager.death",
        Kind::ZombifiedPiglin => "minecraft:entity.zombified_piglin.death",
        _ => "minecraft:entity.generic.death",
    }
}

/// `Zombie.getBaseExperienceReward`: 5, babies 2.5 times, plus the equipment bonus.
pub fn experience_reward(m: &mut MobData) -> i32 {
    let mut xp = 5;
    if m.baby && is_zombie(m.kind) {
        #[allow(clippy::cast_possible_truncation)]
        {
            xp = (f64::from(xp) * 2.5) as i32;
        }
    }
    equipment_experience(m, xp)
}

/// `Mob.getBaseExperienceReward`'s equipment bonus: 1–3 per worn item that is not a
/// guaranteed drop (every slot but the saddle, `canIncreaseExperience`).
pub fn equipment_experience(m: &mut MobData, xp: i32) -> i32 {
    if xp <= 0 {
        return xp;
    }
    let mut result = xp;
    for slot in &equipment::SLOTS[..7] {
        let i = usize::from(slot.ordinal());
        if !m.equipment.items[i].is_empty() && m.equipment.drop_chances[i] <= 1.0 {
            result += 1 + m.random.next_int_bounded(3);
        }
    }
    result
}

/// `BABY_DIMENSIONS` of the zombies.
pub fn dimensions(m: &MobData) -> Option<Dimensions> {
    (m.baby && is_zombie(m.kind)).then_some(Dimensions {
        width: 0.49,
        height: 0.98,
    })
}

/// The eye height of `BABY_DIMENSIONS` of the zombies.
pub fn baby_eye_height(m: &MobData) -> Option<f32> {
    if !m.baby {
        return None;
    }
    match m.kind {
        Kind::Zombie | Kind::Drowned => Some(0.775),
        Kind::Husk => Some(0.825),
        Kind::ZombieVillager => Some(0.67),
        Kind::ZombifiedPiglin => Some(0.78),
        _ => None,
    }
}

// --- Spawn rules ------------------------------------------------------------------------

/// `Mob.checkMobSpawnRules`.
pub fn mob_rules(kind: Kind, level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    is_spawner(reason) || crate::mob::spawn::valid_spawn_floor(level.block(below), kind.type_id())
}

/// `EntitySpawnReason.isSpawner`.
pub fn is_spawner(reason: SpawnReason) -> bool {
    matches!(reason, SpawnReason::Spawner | SpawnReason::TrialSpawner)
}

/// `Monster.checkMonsterSpawnRules`.
pub fn monster_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    (reason == SpawnReason::TrialSpawner
        || crate::mob::spawn::is_dark_enough_to_spawn(level, env, pos, random))
        && mob_rules(kind, level, pos, reason)
}

/// `Monster.checkSurfaceMonstersSpawnRules`.
pub fn surface_monster_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    monster_rules(kind, level, env, pos, random, reason)
        && (is_spawner(reason) || crate::mob::can_see_sky(level, pos))
}

pub fn check_spawn_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    match kind {
        Kind::Husk => surface_monster_rules(kind, level, env, pos, random, reason),
        Kind::Drowned => drowned_spawn_rules(level, env, pos, random, reason),
        Kind::ZombifiedPiglin => {
            // `checkZombifiedPiglinSpawnRules`: anywhere but on nether wart blocks.
            level
                .block(BlockPos::new(pos.x, pos.y - 1, pos.z))
                .block()
                .name()
                != "minecraft:nether_wart_block"
        }
        _ => monster_rules(kind, level, env, pos, random, reason),
    }
}

/// `Drowned.checkDrownedSpawnRules`.
fn drowned_spawn_rules(
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    if !crate::mob::is_water(level.block(below)) && !is_spawner(reason) {
        return false;
    }
    let can_spawn = (reason == SpawnReason::TrialSpawner
        || crate::mob::spawn::is_dark_enough_to_spawn(level, env, pos, random))
        && (is_spawner(reason) || crate::mob::is_water(level.block(pos)));
    if can_spawn && (is_spawner(reason) || reason == SpawnReason::Reinforcement) {
        return true;
    }
    let frequent = biome_is(level, pos, "minecraft:more_frequent_drowned_spawns");
    if frequent {
        random.next_int_bounded(15) == 0 && can_spawn
    } else {
        random.next_int_bounded(40) == 0 && pos.y < env.sea_level - 5 && can_spawn
    }
}

/// Whether the biome at `pos` is in a tag (with the installed spawn table).
pub fn biome_is(level: &Level, pos: BlockPos, tag: &str) -> bool {
    crate::mob::spawn::biome_at(level, pos).is_some_and(|b| b.is(tag))
}

// --- Setup ------------------------------------------------------------------------------

pub fn init(m: &mut MobData) {
    zs_mut(m);
    match m.kind {
        Kind::Drowned => {
            m.nav.kind = NavKind::Amphibious;
            m.move_ctl.kind = MoveKind::Custom;
        }
        Kind::ZombieVillager => {
            // `initializeZombieVillagerData`: plains, a random profession, level 1. It runs in
            // `defineSynchedData`, inside the `Entity` constructor, before the entity's random
            // gets its seed: the draw is not from the mob's random.
            let professions = vanillite_data::registry::VILLAGER_PROFESSION.entries.len();
            let p = vanillite_base::random::LegacyRandom::new(crate::mob::new_unique_seed())
                .next_int_bounded(i32::try_from(professions).unwrap_or(1).max(1));
            let plains = vanillite_data::registry::VILLAGER_TYPE
                .id("minecraft:plains")
                .unwrap_or(0);
            let z = zs_mut(m);
            z.villager_type = plains;
            z.profession = u16::try_from(p).unwrap_or(0);
            z.villager_level = 1;
        }
        _ => {}
    }
}

/// `Zombie.registerGoals` / `addBehaviourGoals` of the variants (`MoveThroughVillageGoal`
/// comes with the villages).
pub fn register_goals(m: &mut MobData) {
    let g = &mut m.goals;
    let tg = &mut m.targets;
    if m.kind != Kind::Giant {
        // `Zombie.registerGoals` before `addBehaviourGoals`, for every zombie.
        g.add(4, crate::mob::movement::MoveGoal::zombie_turtle_egg());
    }
    match m.kind {
        Kind::Giant => {}
        Kind::Drowned => {
            g.add(8, goals::Goal::look_at_player(8.0));
            g.add(8, goals::Goal::random_look());
            g.add(
                1,
                goal(Goal::DrownedGoToWater {
                    speed: 1.0,
                    wanted: Vec3::ZERO,
                }),
            );
            g.add(
                2,
                goal(Goal::DrownedTrident(CommonGoal::ranged(1.0, 40, 40, 10.0))),
            );
            g.add(
                2,
                goal(Goal::DrownedMelee(Melee::new(
                    MeleeKind::Zombie,
                    1.0,
                    false,
                ))),
            );
            g.add(5, goal(Goal::DrownedGoToBeach(MoveToBlock::new(1.0, 8, 2))));
            g.add(
                6,
                goal(Goal::DrownedSwimUp {
                    speed: 1.0,
                    stuck: false,
                }),
            );
            g.add(7, crate::mob::movement::MoveGoal::random_stroll(1.0, 120));
            tg.add(1, goal(Goal::HurtByIgnoringDrowned(HurtBy::new(true))));
            tg.add(
                2,
                goals::Goal::Common(CommonGoal::nearest(10, true, drowned_ok_player)),
            );
            tg.add(
                3,
                goals::Goal::Common(
                    CommonGoal::nearest(10, false, common::villager).of_kinds(&common::VILLAGERS),
                ),
            );
            tg.add(
                3,
                goals::Goal::Common(
                    CommonGoal::nearest(10, true, common::iron_golem)
                        .of_kinds(&common::IRON_GOLEMS),
                ),
            );
            tg.add(
                3,
                goals::Goal::Common(
                    CommonGoal::nearest(10, true, axolotl).of_kinds(&common::AXOLOTLS),
                ),
            );
            tg.add(
                5,
                goals::Goal::Common(
                    CommonGoal::nearest(10, true, common::baby_turtle_on_land)
                        .of_kinds(&common::TURTLES),
                ),
            );
        }
        Kind::ZombifiedPiglin => {
            g.add(8, goals::Goal::look_at_player(8.0));
            g.add(8, goals::Goal::random_look());
            g.add(1, goal(Goal::SpearUse(SpearUse::new(1.0, 1.0, 10.0, 2.0))));
            g.add(2, goals::Goal::melee(MeleeKind::Zombie, 1.0, false));
            g.add(7, goals::Goal::stroll(1.0));
            tg.add(1, goals::Goal::hurt_by(true));
            tg.add(
                2,
                goals::Goal::Common(CommonGoal::nearest(10, true, common::angry_at)),
            );
            tg.add(
                3,
                goals::Goal::Common(CommonGoal::ResetUniversalAnger {
                    alert_same_type: true,
                    last_hurt: 0,
                }),
            );
        }
        _ => {
            g.add(8, goals::Goal::look_at_player(8.0));
            g.add(8, goals::Goal::random_look());
            g.add(2, goal(Goal::SpearUse(SpearUse::new(1.0, 1.0, 10.0, 2.0))));
            g.add(3, goals::Goal::melee(MeleeKind::Zombie, 1.0, false));
            g.add(
                6,
                goal(Goal::MoveThroughVillage {
                    poi: BlockPos::ZERO,
                }),
            );
            g.add(7, goals::Goal::stroll(1.0));
            tg.add(1, goals::Goal::hurt_by(true));
            tg.add(2, goals::Goal::nearest_player(true));
            tg.add(
                3,
                goals::Goal::Common(
                    CommonGoal::nearest(10, false, common::villager).of_kinds(&common::VILLAGERS),
                ),
            );
            tg.add(
                3,
                goals::Goal::Common(
                    CommonGoal::nearest(10, true, common::iron_golem)
                        .of_kinds(&common::IRON_GOLEMS),
                ),
            );
            tg.add(
                5,
                goals::Goal::Common(
                    CommonGoal::nearest(10, true, common::baby_turtle_on_land)
                        .of_kinds(&common::TURTLES),
                ),
            );
        }
    }
}

fn axolotl(t: &Tick<'_, '_>, id: crate::mob::EntityId) -> bool {
    common::mob_of(t, id, Kind::Axolotl)
}

/// `Drowned.okTarget` for players: at night, or the player is in water.
fn drowned_ok_player(t: &Tick<'_, '_>, id: crate::mob::EntityId) -> bool {
    t.ents.player(id).is_some() && drowned_ok_target(t, id)
}

/// `Drowned.okTarget`.
fn drowned_ok_target(t: &Tick<'_, '_>, id: crate::mob::EntityId) -> bool {
    let in_water = t.ents.mob(id).map_or_else(
        || {
            t.ents
                .player(id)
                .is_some_and(|p| p.survival.fluids.in_water())
        },
        |m| m.fluids.in_water(),
    );
    !t.cx.env.is_bright_outside() || in_water
}

/// `Zombie.finalizeSpawn` (after `Mob.finalizeSpawn`) of every zombie, the classic one too.
pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    if m.kind == Kind::Giant {
        return;
    }
    if m.kind == Kind::ZombieVillager {
        finalize_villager_type(m, ctx);
    }
    zombie_finalize(m, ctx);
    match m.kind {
        Kind::Husk => husk_finalize(m, ctx),
        Kind::Drowned => drowned_finalize(m, ctx),
        _ => {}
    }
}

/// `Zombie.finalizeSpawn`.
pub fn zombie_finalize(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    zombie_finalize_with(m, ctx, true);
}

/// `Zombie.finalizeSpawn`; `roll_baby` false for a given `ZombieGroupData(false, true)`.
fn zombie_finalize_with(m: &mut MobData, ctx: &mut SpawnContext<'_>, roll_baby: bool) {
    let special = ctx.special_multiplier;
    let conversion = ctx.reason == SpawnReason::Conversion;
    if !conversion {
        m.can_pick_up_loot = m.random.next_float() < 0.55 * special;
    }
    // `ZombieGroupData(getSpawnAsBabyOdds(random), true)` once per spawn group: the later
    // members take the first one's baby status; other group data skips the zombie part.
    let baby = if !roll_baby {
        Some(false)
    } else {
        match ctx.group {
            GroupData::None => {
                let baby = m.random.next_float() < 0.05;
                ctx.group = GroupData::Zombie(baby);
                Some(baby)
            }
            GroupData::Zombie(baby) => Some(baby),
            _ => None,
        }
    };
    if let Some(baby) = baby {
        if baby {
            super::set_baby(m, true);
            kinds::zombie_chicken_jockey(m, ctx);
        }
        let doors = m.random.next_float() < special * 0.1;
        set_can_break_doors(m, doors);
        if !conversion {
            populate_equipment(m, ctx.difficulty, special);
            equipment::populate_default_enchantments(m, special);
        }
    }
    kinds::halloween_head(m);
    // `handleAttributes` draws from the zombie's own random (`this.random`).
    let mut own = LegacyRandom::from_state(ctx.own_state);
    handle_attributes(m, special, ctx.reason, &mut own);
    ctx.own_state = own.state();
}

/// `finalizeSpawn(CONVERSION, new ZombieGroupData(false, true))` of a zombie villager made
/// from a villager: `Mob.finalizeSpawn` and the zombie part without a baby roll, drawing from
/// the level's `random`.
pub fn finalize_converted(m: &mut MobData, ctx: &mut SpawnContext<'_>, random: &mut LegacyRandom) {
    std::mem::swap(&mut m.random, random);
    ctx.own_state = random.state();
    let bonus = m.random.triangle(0.0, 0.114_850_000_000_000_01);
    m.attrs.follow_range *= 1.0 + bonus;
    m.left_handed = m.random.next_float() < 0.05;
    if m.kind == Kind::ZombieVillager {
        finalize_villager_type(m, ctx);
    }
    zombie_finalize_with(m, ctx, false);
    *random = LegacyRandom::from_state(ctx.own_state);
    std::mem::swap(&mut m.random, random);
}

/// `setVillagerData` and `setVillagerDataFinalized` of a zombie villager.
pub fn set_villager_data(m: &mut MobData, ty: u16, profession: u16, level: i32, finalized: bool) {
    let z = zs_mut(m);
    z.villager_type = ty;
    z.profession = profession;
    z.villager_level = level;
    z.data_finalized = finalized;
}

/// `setGossips`, `setTradeOffers` and `setVillagerXp` of a zombie villager.
pub fn set_villager_trades(
    m: &mut MobData,
    gossips: crate::mob::fam::village::gossip::Gossips,
    offers: crate::trading::MerchantOffers,
    xp: i32,
) {
    let z = zs_mut(m);
    z.gossips = Some(gossips);
    z.offers = Some(offers);
    z.villager_xp = xp;
}

/// `setCanBreakDoors`: ground navigation only.
fn set_can_break_doors(m: &mut MobData, on: bool) {
    crate::mob::movement::set_can_break_doors(m, on);
}

/// `populateDefaultEquipmentSlots` of each zombie kind.
fn populate_equipment(m: &mut MobData, difficulty: Difficulty, special: f32) {
    match m.kind {
        Kind::Drowned => {
            if m.random.next_float() > 0.9 {
                let item = if m.random.next_int_bounded(16) < 10 {
                    "minecraft:trident"
                } else {
                    "minecraft:fishing_rod"
                };
                m.equipment
                    .set(EquipmentSlot::MainHand, equipment::stack(item));
            }
        }
        Kind::ZombifiedPiglin => {
            let item = if m.random.next_int_bounded(20) == 0 {
                "minecraft:golden_spear"
            } else {
                "minecraft:golden_sword"
            };
            m.equipment
                .set(EquipmentSlot::MainHand, equipment::stack(item));
        }
        _ => {
            equipment::populate_default_armor(m, difficulty, special);
            let chance = if difficulty == Difficulty::Hard {
                0.05
            } else {
                0.01
            };
            if m.random.next_float() < chance {
                let weapon = match m.random.next_int_bounded(6) {
                    0 => "minecraft:iron_sword",
                    1 => "minecraft:iron_spear",
                    _ => "minecraft:iron_shovel",
                };
                m.equipment
                    .set(EquipmentSlot::MainHand, equipment::stack(weapon));
            }
        }
    }
}

/// `Zombie.handleAttributes`: reinforcements, knockback resistance, follow range and the
/// leader bonus, drawn from the zombie's own random `random`.
pub fn handle_attributes(
    m: &mut MobData,
    special: f32,
    reason: SpawnReason,
    random: &mut LegacyRandom,
) {
    // `randomizeReinforcementsChance`.
    let base = if m.kind == Kind::ZombifiedPiglin {
        0.0
    } else {
        random.next_double() * f64::from(0.1_f32)
    };
    zs_mut(m).reinforcement = base;
    m.attrs.knockback_resistance += random.next_double() * f64::from(0.05_f32);
    let follow = random.next_double() * 1.5 * f64::from(special);
    if follow > 1.0 {
        m.attrs.follow_range *= 1.0 + follow;
    }
    if random.next_float() < special * 0.05 {
        let bonus = random.next_double() * 0.25 + 0.5;
        zs_mut(m).reinforcement += bonus;
        let health = random.next_double() * 3.0 + 1.0;
        m.attrs.max_health *= 1.0 + health;
        if !matches!(
            reason,
            SpawnReason::Conversion | SpawnReason::Load | SpawnReason::DimensionTravel
        ) {
            m.health = m.max_health();
        }
        set_can_break_doors(m, true);
    }
}

/// `ZombieVillager.finalizeVillagerType`: the villager type of the biome.
fn finalize_villager_type(m: &mut MobData, ctx: &SpawnContext<'_>) {
    if zs(m).is_some_and(|z| z.data_finalized) {
        return;
    }
    let name = ctx.biome.map_or("minecraft:plains", |b| &*b.name);
    let ty = villager_type_of_biome(name);
    let id = vanillite_data::registry::VILLAGER_TYPE.id(ty).unwrap_or(0);
    let z = zs_mut(m);
    z.villager_type = id;
    z.data_finalized = true;
}

/// `VillagerType.byBiome`.
pub fn villager_type_of_biome(biome: &str) -> &'static str {
    match biome.strip_prefix("minecraft:").unwrap_or(biome) {
        "badlands" | "desert" | "eroded_badlands" | "wooded_badlands" => "minecraft:desert",
        "bamboo_jungle" | "jungle" | "sparse_jungle" => "minecraft:jungle",
        "savanna_plateau" | "savanna" | "windswept_savanna" => "minecraft:savanna",
        "deep_frozen_ocean" | "frozen_ocean" | "frozen_river" | "ice_spikes" | "snowy_beach"
        | "snowy_taiga" | "snowy_plains" | "grove" | "snowy_slopes" | "frozen_peaks"
        | "jagged_peaks" => "minecraft:snow",
        "swamp" | "mangrove_swamp" => "minecraft:swamp",
        "old_growth_spruce_taiga"
        | "old_growth_pine_taiga"
        | "windswept_gravelly_hills"
        | "windswept_hills"
        | "taiga"
        | "windswept_forest" => "minecraft:taiga",
        _ => "minecraft:plains",
    }
}

/// `Husk.finalizeSpawn`: the loot roll again and the camel husk with its parched rider.
fn husk_finalize(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    if ctx.reason != SpawnReason::Conversion {
        m.can_pick_up_loot = m.random.next_float() < 0.55 * ctx.special_multiplier;
    }
    if ctx.reason != SpawnReason::Natural {
        return;
    }
    let Some(level) = ctx.level else {
        return;
    };
    let p = ctx.pos;
    let at = Vec3::new(f64::from(p.x) + 0.5, f64::from(p.y), f64::from(p.z) + 0.5);
    if !super::box_free(level, Kind::CamelHusk, at, true) {
        return;
    }
    if m.random.next_float() >= 0.1 {
        return;
    }
    m.equipment.set(
        EquipmentSlot::MainHand,
        equipment::stack("minecraft:iron_spear"),
    );
    let mut camel = MobData::new(Kind::CamelHusk, crate::mob::new_entity_seed());
    let mut parched = MobData::new(Kind::Parched, crate::mob::new_entity_seed());
    for mob in [&mut camel, &mut parched] {
        let mut inner = SpawnContext {
            extra: Vec::new(),
            ride: None,
            ..*ctx
        };
        kinds::finalize_spawn(mob, &mut inner, &mut m.random);
    }
    let base = ctx.extra.len();
    ctx.extra.push(ExtraMob {
        mob: camel,
        offset: Vec3::ZERO,
        relation: Relation::CarriesIt,
    });
    ctx.extra.push(ExtraMob {
        mob: parched,
        offset: Vec3::ZERO,
        relation: Relation::RidesExtra(base),
    });
}

/// `Drowned.finalizeSpawn`: the nautilus shell and the zombie nautilus jockey.
fn drowned_finalize(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    if m.equipment.get(EquipmentSlot::OffHand).is_empty() && m.random.next_float() < 0.03 {
        m.equipment.set(
            EquipmentSlot::OffHand,
            equipment::stack("minecraft:nautilus_shell"),
        );
        m.equipment.set_guaranteed_drop(EquipmentSlot::OffHand);
    }
    let natural = matches!(ctx.reason, SpawnReason::Natural | SpawnReason::Structure);
    if natural
        && m.equipment.holds(items::TRIDENT.0)
        && m.random.next_float() < 0.5
        && !m.baby
        && !ctx.biome_is("minecraft:more_frequent_drowned_spawns")
    {
        let mut nautilus = MobData::new(Kind::ZombieNautilus, crate::mob::new_entity_seed());
        if ctx.reason == SpawnReason::Structure {
            nautilus.persistent = true;
        }
        let mut inner = SpawnContext {
            extra: Vec::new(),
            ride: None,
            ..*ctx
        };
        kinds::finalize_spawn(&mut nautilus, &mut inner, &mut m.random);
        ctx.extra.push(ExtraMob {
            mob: nautilus,
            offset: Vec3::ZERO,
            relation: Relation::CarriesIt,
        });
    }
}

// --- Spears -----------------------------------------------------------------------------

/// `SpearUseGoal`: approach the target, charge it with the spear lowered for the weapon's
/// damage use duration, then back off and charge again (`SpearUseState`; `active` while the
/// goal has a state).
#[derive(Clone, Copy, Debug)]
pub struct SpearUse {
    pub charge_speed: f64,
    pub reposition_speed: f64,
    pub approach_sqr: f32,
    pub in_range_sqr: f32,
    pub active: bool,
    pub engage_time: i32,
    pub fleeing_time: i32,
    pub away: Option<Vec3>,
    pub done: bool,
}

impl SpearUse {
    /// `SpearUseGoal(mob, charging, repositioning, approachDistance, targetInRangeRadius)`.
    pub fn new(charge: f64, reposition: f64, approach: f32, in_range: f32) -> SpearUse {
        SpearUse {
            charge_speed: charge,
            reposition_speed: reposition,
            approach_sqr: approach * approach,
            in_range_sqr: in_range * in_range,
            active: false,
            engage_time: -1,
            fleeing_time: -1,
            away: None,
            done: false,
        }
    }
}

/// `SpearUseGoal.MAX_FLEEING_TIME` (`reducedTickDelay(100)`).
const SPEAR_MAX_FLEEING_TIME: i32 = 50;

/// `ableToAttack`: a target and a kinetic weapon in the main hand.
fn spear_able(t: &Tick<'_, '_>) -> bool {
    t.mob_ref().target.is_some() && crate::spear::mob_holds_kinetic(t.mob_ref())
}

fn spear_can_use(t: &Tick<'_, '_>) -> bool {
    spear_able(t) && t.mob_ref().using_item < 0
}

fn spear_can_continue(t: &Tick<'_, '_>, s: &SpearUse) -> bool {
    s.active && !s.done && spear_able(t)
}

fn spear_start(t: &mut Tick<'_, '_>, s: &mut SpearUse) {
    goals::set_aggressive(t, true);
    // `state = new SpearUseState()`.
    s.active = true;
    s.engage_time = -1;
    s.fleeing_time = -1;
    s.away = None;
    s.done = false;
}

fn spear_stop(t: &mut Tick<'_, '_>, s: &mut SpearUse) {
    t.nav().stop();
    goals::set_aggressive(t, false);
    s.active = false;
    crate::mob::projectile::stop_using_item(t);
}

fn spear_tick(t: &mut Tick<'_, '_>, s: &mut SpearUse) {
    if !s.active {
        return;
    }
    let Some(target) = t.mob_ref().target else {
        return;
    };
    let Some(o) = t.other(target) else {
        return;
    };
    let p = t.pos();
    let dist_sqr = p.distance_to_sqr(o.pos);
    let mut root = t.id;
    while let Some(v) = t.ents.vehicle_of(root) {
        root = v;
    }
    // `chargeSpeedModifier` of the mob at the bottom of the stack the zombie rides.
    let speed = match t.ents.mob(root).map(|m| m.kind) {
        Some(Kind::ZombieHorse) if root != t.id => 1.4_f32,
        Some(Kind::CamelHusk) if root != t.id => 4.0,
        _ => 1.0,
    };
    let speed = f64::from(speed);
    let mount = if t.ents.vehicle_of(t.id).is_some() {
        2.0
    } else {
        0.0
    };
    kinds::look_at_entity(t, o, 30.0, 30.0);
    t.mob()
        .look
        .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 30.0, 30.0);
    if s.engage_time < 0 {
        if dist_sqr > f64::from(s.approach_sqr) {
            nav::move_to_entity(t, o.pos, 1, speed * s.reposition_speed);
            return;
        }
        s.engage_time = goals::reduced(crate::spear::mob_use_duration(t.mob_ref()));
        crate::mob::projectile::start_using_item(t);
    }
    // `tickAndCheckEngagement`.
    if s.engage_time > 0 {
        s.engage_time -= 1;
        if s.engage_time == 0 {
            crate::mob::projectile::stop_using_item(t);
            let d = dist_sqr.sqrt();
            s.away = randompos::land_pos_away(
                t,
                (9.0 + mount - d).max(0.0),
                (11.0 + mount - d).max(1.0),
                7,
                o.pos,
            );
            s.fleeing_time = 1;
        }
    }
    // `tickAndCheckFleeing`.
    if s.fleeing_time > 0 {
        s.fleeing_time += 1;
        if s.fleeing_time > SPEAR_MAX_FLEEING_TIME {
            s.done = true;
            return;
        }
    }
    if let Some(away) = s.away {
        nav::move_to_pos(t, away, speed * s.reposition_speed);
        if t.nav_ref().is_done() {
            if s.fleeing_time > 0 {
                s.done = true;
                return;
            }
            s.away = None;
        }
    } else {
        nav::move_to_entity(t, o.pos, 1, speed * s.charge_speed);
        if dist_sqr < f64::from(s.in_range_sqr) || t.nav_ref().is_done() {
            let d = dist_sqr.sqrt();
            s.away = randompos::land_pos_away(t, 6.0 + mount - d, 7.0 + mount - d, 7, o.pos);
        }
    }
}

/// `LivingEntity.updatingUsingItem` of the zombies (they only use spears): the use counts up
/// and the spear damages what it touches; another item in the hand stops the use.
pub fn spear_use_tick(t: &mut Tick<'_, '_>) {
    if t.mob_ref().using_item < 0 {
        return;
    }
    if !crate::spear::mob_holds_kinetic(t.mob_ref()) {
        crate::mob::projectile::stop_using_item(t);
        return;
    }
    let m = t.mob();
    m.using_item += 1;
    let used = m.using_item - 1;
    crate::spear::mob_use_tick(t, used);
}

// --- Tick -------------------------------------------------------------------------------

pub fn before_tick(t: &mut Tick<'_, '_>) {
    if t.mob_ref().kind == Kind::ZombieVillager {
        cure_tick(t);
    }
}

pub fn after_tick(t: &mut Tick<'_, '_>) {
    if converts_in_water(t.mob_ref().kind) {
        drowning_tick(t);
    }
}

pub fn ai_step(t: &mut Tick<'_, '_>) {
    if t.mob_ref().kind == Kind::Drowned {
        // `updateSwimming`.
        let swim = t.mob_ref().fluids.eye_in_water && wants_to_swim(t) && !t.mob_ref().no_ai;
        let flags = t.core().flags();
        if flags.contains(EntityFlags::SWIMMING) != swim {
            t.core_mut().set_flags(EntityFlags::SWIMMING, swim);
        }
    }
}

pub fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
    if t.mob_ref().kind == Kind::ZombifiedPiglin {
        piglin_server_ai_step(t);
    }
}

/// `convertsInWater`: zombies become drowned and husks zombies.
fn converts_in_water(kind: Kind) -> bool {
    matches!(kind, Kind::Zombie | Kind::Husk)
}

/// `ConversionTracker.tick` of the drowning conversion (after `Zombie.tick`).
pub fn drowning_tick(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().is_alive() || t.mob_ref().no_ai {
        return;
    }
    let afflicted = t.mob_ref().fluids.eye_in_water;
    let z = zs_mut(t.mob());
    if afflicted {
        if z.converting {
            z.conversion_time -= 1;
            if z.conversion_time < 0 {
                drown_convert(t);
            }
        } else {
            z.in_water_time += 1;
            if z.in_water_time >= 600 {
                z.conversion_time = 300;
                z.converting = true;
                t.core_mut().mark_data_dirty();
            }
        }
    } else {
        z.in_water_time = -1;
        if z.converting {
            z.converting = false;
            t.core_mut().mark_data_dirty();
        }
    }
}

/// The drowning conversion: zombie → drowned (event 1040), husk → zombie (1041).
fn drown_convert(t: &mut Tick<'_, '_>) {
    let (to, event) = match t.mob_ref().kind {
        Kind::Husk => (Kind::Zombie, 1041),
        _ => (Kind::Drowned, 1040),
    };
    // `getCurrentDifficultyAt(converted.blockPosition())`: the converted mob is where this
    // one is.
    let special = crate::level::difficulty::at(t.level, t.block_pos()).special_multiplier();
    let Some(id) = super::convert_to(
        t,
        to,
        true,
        true,
        vanillite_abi::transform_reason::DROWNED,
        |n| {
            let mut own = n.random.clone();
            handle_attributes(n, special, SpawnReason::Conversion, &mut own);
            n.random = own;
        },
    ) else {
        return;
    };
    let silent = t.ents.mob(id).is_some_and(|m| m.silent);
    if !silent && let Some(p) = t.ents.store.get(id).map(|e| e.pos()) {
        level_event(t.cx, event, BlockPos::containing(p.x, p.y, p.z), 0);
    }
}

/// `ZombieVillager.tick`'s curing countdown.
fn cure_tick(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().is_alive() || !zs(t.mob_ref()).is_some_and(|z| z.curing) {
        return;
    }
    let amount = conversion_progress(t);
    let z = zs_mut(t.mob());
    if z.cure_time > 0 {
        z.cure_time -= amount;
    }
    if z.cure_time <= 0 {
        finish_curing(t);
    }
}

/// `getConversionProgress`: beds and iron bars nearby speed curing up.
fn conversion_progress(t: &mut Tick<'_, '_>) -> i32 {
    let mut amount = 1;
    if t.mob().random.next_float() < 0.01 {
        let p = t.pos();
        #[allow(clippy::cast_possible_truncation)]
        let (x, y, z) = (p.x as i32, p.y as i32, p.z as i32);
        let mut count = 0;
        'out: for xx in x - 4..x + 4 {
            for yy in y - 4..y + 4 {
                for zz in z - 4..z + 4 {
                    if count >= 14 {
                        break 'out;
                    }
                    let state = t.level.block(BlockPos::new(xx, yy, zz));
                    let name = state.block().name();
                    if name == "minecraft:iron_bars" || name.ends_with("_bed") {
                        if t.mob().random.next_float() < 0.3 {
                            amount += 1;
                        }
                        count += 1;
                    }
                }
            }
        }
    }
    amount
}

/// `finishConversion`: the zombie villager becomes a villager with its data, gossip, offers
/// and experience; the player who cured it gains reputation.
fn finish_curing(t: &mut Tick<'_, '_>) {
    let Some(z) = zs(t.mob_ref()).cloned() else {
        return;
    };
    // `dropPreservedEquipment(!PREVENT_ARMOR_CHANGE)`: what the zombie villager picked up is
    // dropped; Curse of Binding items go into the villager's inventory slot of the slot's
    // index (`getIndex() + 300`) instead.
    let mut kept: Vec<(usize, ItemStack)> = Vec::new();
    let mut dropped: Vec<ItemStack> = Vec::new();
    {
        let m = t.mob();
        for slot in equipment::SLOTS {
            let i = usize::from(slot.ordinal());
            let stack = &m.equipment.items[i];
            if stack.is_empty() {
                continue;
            }
            let bound =
                crate::player::play::game_data().is_some_and(|d| d.prevents_armor_change(stack));
            if bound {
                let index = match slot {
                    EquipmentSlot::OffHand | EquipmentSlot::Legs => 1,
                    EquipmentSlot::Chest => 2,
                    EquipmentSlot::Head => 3,
                    _ => 0,
                };
                kept.push((index, stack.clone()));
            } else if m.equipment.drop_chances[i] > 1.0 {
                dropped.push(stack.clone());
            }
        }
    }
    let silent = t.mob_ref().silent;
    let pos = t.block_pos();
    let drop_at = t.pos();
    // The zombie as `CURED_ZOMBIE_VILLAGER` sees it, before it is replaced.
    let zombie = z
        .cure_starter
        .and_then(|starter| t.ents.player_by_uuid(starter))
        .filter(|&player| {
            crate::advancements::view::listens(
                t.ents,
                player,
                crate::advancements::trigger::TriggerKind::CURED_ZOMBIE_VILLAGER,
            )
        })
        .and_then(|_| crate::advancements::view::snapshot(t.ents, t.id));
    let Some(id) = super::convert_to(
        t,
        Kind::Villager,
        false,
        false,
        vanillite_abi::transform_reason::CURED,
        |v| {
            crate::mob::fam::village::villager::cured(
                v,
                z.villager_type,
                z.profession,
                z.villager_level,
                z.data_finalized,
                z.gossips.clone().unwrap_or_default(),
                z.offers.clone(),
                z.villager_xp,
            );
            let inv = &mut crate::mob::fam::village::state_mut(v).merchant.inventory;
            for (i, stack) in kept {
                if let Some(slot) = inv.get_mut(i) {
                    *slot = stack;
                }
            }
        },
    ) else {
        return;
    };
    // The dropped equipment (`spawnAtLocation`, created after the villager).
    for stack in dropped {
        let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
        t.env.out.push(crate::survival::Effect::DropItem {
            random: item_random,
            uuid,
            pos: drop_at,
            velocity,
            y_rot,
            pickup_delay: 10,
            stack,
        });
    }
    // `finalizeSpawn(CONVERSION)`: the villager type of the biome unless final.
    if let Some(v) = t.ents.mob_mut(id)
        && !crate::mob::fam::village::villager::vs(v).data_finalized
    {
        let biome = crate::mob::fam::village::villager::biome_at(t.level, pos);
        let ty = vanillite_data::registry::VILLAGER_TYPE
            .id(villager_type_of_biome(&biome))
            .unwrap_or(0);
        let s = crate::mob::fam::village::villager::vs_mut(v);
        s.villager_type = ty;
        s.data_finalized = true;
    }
    // `Mob.finalizeSpawn`: the follow range bonus and left-handedness from the level random.
    let bonus = t.env.random.triangle(0.0, 0.114_850_000_000_000_01);
    let left_handed = t.env.random.next_float() < 0.05;
    if let Some(m) = t.ents.mob_mut(id) {
        m.attrs.follow_range *= 1.0 + bonus;
        m.left_handed = left_handed;
    }
    if let Some(starter) = z.cure_starter
        && let Some(player) = t.ents.player_by_uuid(starter)
    {
        if let Some(zombie) = zombie
            && let Some(villager) = crate::advancements::view::snapshot(t.ents, id)
            && let Some(p) = t.ents.player_mut(player)
        {
            p.advancements
                .trigger(crate::advancements::trigger::Event::CuredZombieVillager(
                    zombie, villager,
                ));
        }
        crate::mob::fam::village::villager::reputation_event(
            t.ents,
            crate::mob::fam::village::villager::ReputationEvent::ZombieVillagerCured,
            player,
            id,
        );
    }
    if !silent {
        level_event(t.cx, 1027, pos, 0);
    }
}

// --- Drowned --------------------------------------------------------------------------------

/// `Drowned.wantsToSwim`.
fn wants_to_swim(t: &Tick<'_, '_>) -> bool {
    let m = t.mob_ref();
    if zs(m).is_some_and(|z| z.searching_for_land) {
        return true;
    }
    m.target.is_some_and(|id| {
        t.ents.mob(id).map_or_else(
            || {
                t.ents
                    .player(id)
                    .is_some_and(|p| p.survival.fluids.in_water())
            },
            |o| o.fluids.in_water(),
        )
    })
}

/// `Drowned.DrownedMoveControl.tick`.
pub fn move_control(t: &mut Tick<'_, '_>) -> bool {
    if t.mob_ref().kind != Kind::Drowned {
        return false;
    }
    let target_higher = t
        .mob_ref()
        .target
        .and_then(|id| t.other(id))
        .is_some_and(|o| o.pos.y > t.pos().y);
    if wants_to_swim(t) && t.mob_ref().fluids.in_water() {
        if target_higher || zs(t.mob_ref()).is_some_and(|z| z.searching_for_land) {
            let v = t.velocity();
            t.set_velocity(Vec3::new(v.x, v.y + 0.002, v.z));
        }
        let ctl = t.mob_ref().move_ctl;
        if ctl.operation != nav::Operation::MoveTo || t.nav_ref().is_done() {
            // `setSpeed(0)`: also the forward input.
            let m = t.mob();
            m.speed = 0.0;
            m.zza = 0.0;
            return true;
        }
        let p = t.pos();
        let (xd, mut yd, zd) = (ctl.wanted.x - p.x, ctl.wanted.y - p.y, ctl.wanted.z - p.z);
        let dd = (xd * xd + yd * yd + zd * zd).sqrt();
        yd /= dd;
        #[allow(clippy::cast_possible_truncation)]
        let y_rot_d = crate::mob::atan2_deg(zd, xd) as f32 - 90.0;
        let mut rot = t.core().rot();
        rot.y_rot = nav::rotlerp(rot.y_rot, y_rot_d, 90.0);
        t.core_mut().set_rot(rot);
        t.mob().y_body_rot = rot.y_rot;
        #[allow(clippy::cast_possible_truncation)]
        let target_speed = (ctl.speed_modifier * t.mob_ref().attrs.movement_speed) as f32;
        let speed = t.mob_ref().speed + 0.125 * (target_speed - t.mob_ref().speed);
        let m = t.mob();
        m.speed = speed;
        m.zza = speed;
        let s = f64::from(speed);
        let v = t.velocity();
        t.set_velocity(Vec3::new(
            v.x + s * xd * 0.005,
            v.y + s * yd * 0.1,
            v.z + s * zd * 0.005,
        ));
    } else {
        if !t.on_ground() {
            let v = t.velocity();
            t.set_velocity(Vec3::new(v.x, v.y - 0.008, v.z));
        }
        t.mob().move_ctl.kind = MoveKind::Default;
        nav::tick_move_control(t);
        if t.exists() {
            t.mob().move_ctl.kind = MoveKind::Custom;
        }
    }
    true
}

/// `Drowned.travelInWater`: swimming drowned glide.
pub fn travel(t: &mut Tick<'_, '_>, input: Vec3) -> bool {
    if t.mob_ref().kind != Kind::Drowned
        || !t.mob_ref().fluids.eye_in_water
        || !wants_to_swim(t)
        || !t.mob_ref().fluids.in_water()
    {
        return false;
    }
    crate::mob::living::move_relative(t, 0.01, input);
    let v = t.velocity();
    crate::mob::living::move_self(t, v);
    if t.exists() {
        let v = t.velocity();
        t.set_velocity(v * 0.9);
    }
    true
}

/// `Drowned.performRangedAttack`: throws a trident.
pub fn drowned_throw_trident(t: &mut Tick<'_, '_>, target: EntityId, _power: f32) {
    let Some(o) = t.other(target) else {
        return;
    };
    let main = t.mob_ref().equipment.get(EquipmentSlot::MainHand).clone();
    let item = if !main.is_empty() && main.item == items::TRIDENT.0 {
        main
    } else {
        ItemStack::new(items::TRIDENT.0, 1)
    };
    let p = t.pos();
    let start = Vec3::new(p.x, t.eye_y() - f64::from(0.1_f32), p.z);
    let xd = o.pos.x - p.x;
    let target_y = o.pos.y + f64::from(o.height) * 0.333_333_333_333_333_3;
    let yd = target_y - start.y;
    let zd = o.pos.z - p.z;
    let dist = (xd * xd + zd * zd).sqrt();
    #[allow(clippy::cast_precision_loss)]
    let uncertainty = (14 - t.env.difficulty as i32 * 4) as f32;
    super::trident::throw(
        t,
        start,
        Vec3::new(xd, yd + dist * f64::from(0.2_f32), zd),
        item,
        uncertainty,
    );
    let pitch = 1.0 / (t.mob().random.next_float() * 0.4 + 0.8);
    sound(t, "minecraft:entity.drowned.shoot", 1.0, pitch);
}

/// `closeToNextPos`: the path's target is within 2 blocks.
fn close_to_next_pos(t: &Tick<'_, '_>) -> bool {
    let Some(path) = t.nav_ref().path.as_ref() else {
        return false;
    };
    let target = path.target;
    let p = t.pos();
    let (dx, dy, dz) = (
        f64::from(target.x) - p.x,
        f64::from(target.y) - p.y,
        f64::from(target.z) - p.z,
    );
    dx * dx + dy * dy + dz * dz < 4.0
}

/// `DrownedGoToBeachGoal.isValidTarget`: two free blocks above a block to stand on.
fn beach_target(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    let above2 = BlockPos::new(pos.x, pos.y + 2, pos.z);
    if !t.level.block(above).is_air() || !t.level.block(above2).is_air() {
        return false;
    }
    // `entityCanStandOn`: a sturdy top face.
    t.level.block(pos).misc().sturdy_full & 2 != 0
}

// --- Zombified piglins ----------------------------------------------------------------------

/// `ZombifiedPiglin.customServerAiStep` (with `setTarget`'s anger sound and alert timers,
/// sampled when a target appears).
/// `ZombifiedPiglin.setTarget` for a first target: `FIRST_ANGER_SOUND_DELAY` and
/// `ALERT_INTERVAL`.
pub(crate) fn piglin_first_target(m: &mut MobData) {
    let first = m.random.next_int_between_inclusive(0, 20);
    let alert = m.random.next_int_between_inclusive(80, 120);
    let z = zs_mut(m);
    z.first_anger_sound_in = first;
    z.next_alert = alert;
}

fn piglin_server_ai_step(t: &mut Tick<'_, '_>) {
    let game_time = t.env.game_time;
    if common::is_angry(t.mob_ref(), game_time) {
        let m = t.mob();
        if !m.baby && !zs(m).is_some_and(|z| z.speed_bonus) {
            m.attrs.movement_speed += 0.05;
            zs_mut(m).speed_bonus = true;
        }
        let z = zs_mut(t.mob());
        if z.first_anger_sound_in > 0 {
            z.first_anger_sound_in -= 1;
            if z.first_anger_sound_in == 0 {
                let volume = 2.0;
                let pitch = voice_pitch(t) * 1.8;
                sound(t, "minecraft:entity.zombified_piglin.angry", volume, pitch);
            }
        }
    } else if zs(t.mob_ref()).is_some_and(|z| z.speed_bonus) {
        let m = t.mob();
        m.attrs.movement_speed -= 0.05;
        zs_mut(m).speed_bonus = false;
    }
    let range = crate::mob::fam::anger_range(Kind::ZombifiedPiglin);
    common::update_persistent_anger(t, true, range);
    if let Some(target) = t.mob_ref().target {
        let z = zs_mut(t.mob());
        if z.next_alert > 0 {
            z.next_alert -= 1;
        } else {
            if crate::mob::living::has_line_of_sight(t, target) {
                alert_others(t, target);
            }
            let next = t.mob().random.next_int_between_inclusive(80, 120);
            zs_mut(t.mob()).next_alert = next;
        }
    }
}

/// `getVoicePitch`.
fn voice_pitch(t: &mut Tick<'_, '_>) -> f32 {
    let baby = t.mob_ref().baby;
    let r = &mut t.mob().random;
    let (a, b) = (r.next_float(), r.next_float());
    if baby {
        (a - b) * 0.2 + 1.5
    } else {
        (a - b) * 0.2 + 1.0
    }
}

/// `ZombifiedPiglin.alertOthers`: the zombified piglins around without a target take it.
fn alert_others(t: &mut Tick<'_, '_>, target: EntityId) {
    let within = t.mob_ref().attrs.follow_range;
    let p = t.pos();
    let area = vanillite_base::Aabb::new(p.x, p.y, p.z, p.x + 1.0, p.y + 1.0, p.z + 1.0)
        .inflate(within, 10.0, within);
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&area, false, |id| ids.push(id));
    for &id in &ids {
        if id == t.id {
            continue;
        }
        if let Some(m) = t.ents.mob_mut(id)
            && m.kind == Kind::ZombifiedPiglin
            && m.target.is_none()
        {
            crate::mob::goals::assign_target(m, Some(target));
        }
    }
    t.cx.ids = ids;
}

// --- Combat ---------------------------------------------------------------------------------

pub fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
    let hit = crate::mob::combat::base_mob_attack(t, target);
    if !hit {
        return Some(false);
    }
    let kind = t.mob_ref().kind;
    if kind == Kind::Giant {
        return Some(true);
    }
    let diff = crate::level::difficulty::at(t.level, t.block_pos()).effective;
    let empty = t
        .mob_ref()
        .equipment
        .get(EquipmentSlot::MainHand)
        .is_empty();
    // `Zombie.doHurtTarget`: a burning zombie with an empty hand sets its target on fire.
    if empty && t.mob_ref().is_on_fire() && t.mob().random.next_float() < diff * 0.3 {
        #[allow(clippy::cast_possible_truncation)]
        crate::mob::attack::ignite(t.ents, target, 2 * diff as i32 * 20);
    }
    if kind == Kind::Husk && empty {
        #[allow(clippy::cast_possible_truncation)]
        let ticks = 140 * diff as i32;
        let me = t.id;
        super::add_effect(
            t.ents,
            target,
            effects::EffectInstance::new(effects::MobEffect::HUNGER, ticks, 0),
            Some(me),
        );
    }
    Some(true)
}

pub fn hurt_filter(_h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
    Some(amount)
}

pub fn after_hurt(h: &mut Hurt<'_, '_>, _amount: f32) {
    reinforcements(h);
}

/// `Zombie.hurtServer`'s reinforcements on Hard.
pub fn reinforcements(h: &mut Hurt<'_, '_>) {
    if h.env.difficulty != Difficulty::Hard {
        return;
    }
    let source = h.source;
    let Some(m) = h.ents.mob(h.target) else {
        return;
    };
    // Also on the killing blow: `super.hurtServer` returned true.
    if !is_zombie(m.kind) {
        return;
    }
    let kind = m.kind;
    let Some(target) = m.target.or_else(|| super::living_cause(h, &source)) else {
        return;
    };
    let chance = zs(m).map_or(0.0, |z| z.reinforcement);
    let Some(pos) = h.ents.store.get(h.target).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = h.ents.mob_mut(h.target) else {
        return;
    };
    // `level.isSpawningMonsters()` after the chance roll.
    if f64::from(m.random.next_float()) >= chance
        || !h.env.rules.bool(Rule::SPAWN_MOBS)
        || !h.env.rules.bool(Rule::SPAWN_MONSTERS)
    {
        return;
    }
    let seed = crate::mob::new_entity_seed();
    let mut new = MobData::new(kind, seed);
    let (x, y, z) = (
        vanillite_base::math::floor(pos.x),
        vanillite_base::math::floor(pos.y),
        vanillite_base::math::floor(pos.z),
    );
    for _ in 0..50 {
        let r = &mut h.ents.mob_mut(h.target).expect("checked above").random;
        let mut off = || {
            let d = r.next_int_between_inclusive(7, 40);
            d * r.next_int_between_inclusive(-1, 1)
        };
        let (xt, yt, zt) = (x + off(), y + off(), z + off());
        let spawn = BlockPos::new(xt, yt, zt);
        if !placement_ok(h.level, kind, spawn) {
            continue;
        }
        // `checkSpawnRules(..., level.getRandom())`.
        // The real level: at night the open surface is dark enough.
        let level_env = h.level.spawner_env.env;
        if !crate::mob::fam::check_spawn_rules(
            kind,
            h.level,
            &level_env,
            spawn,
            h.env.random,
            SpawnReason::Reinforcement,
        ) {
            continue;
        }
        let at = Vec3::new(f64::from(xt), f64::from(yt), f64::from(zt));
        if player_within(h, at, 7.0)
            || !unobstructed(h, kind, at)
            || !super::box_free(h.level, kind, at, kind == Kind::Drowned)
        {
            continue;
        }
        crate::mob::goals::assign_target(&mut new, Some(target));
        let mut ctx = SpawnContext::at(h.level, spawn, SpawnReason::Reinforcement);
        kinds::finalize_spawn(&mut new, &mut ctx, h.env.random);
        zs_mut(&mut new).reinforcement -= 0.05;
        let uuid = crate::mob::new_uuid();
        // `setPos` only: the yaw of `LivingEntity`'s constructor, the head and body at 0.
        let rot = vanillite_world::entity::Rotation {
            y_rot: crate::mob::constructor_y_rot(seed),
            x_rot: 0.0,
            y_head_rot: 0.0,
        };
        let id = crate::mob::spawn(h.ents, h.level.network_ids(), uuid, at, rot, new);
        let parent = h.ents.store.get(h.target).map_or(0, |e| e.uuid());
        if !h
            .ents
            .spawned(id, vanillite_abi::spawn_reason::REINFORCEMENT, parent)
        {
            break;
        }
        super::spawn_extras(
            h.ents,
            h.level,
            id,
            at,
            ctx.extra,
            SpawnReason::Reinforcement,
        );
        if let Some(m) = h.ents.mob_mut(h.target) {
            zs_mut(m).reinforcement -= 0.05;
        }
        break;
    }
}

/// `SpawnPlacements.isSpawnPositionOk` of a zombie kind (in water for drowned).
fn placement_ok(level: &Level, kind: Kind, pos: BlockPos) -> bool {
    if kind == Kind::Drowned {
        let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
        crate::mob::is_water(level.block(pos))
            && !level
                .block(above)
                .has(vanillite_data::block::flags::REDSTONE_CONDUCTOR)
    } else {
        crate::mob::spawn::spawn_position_ok(level, kind.type_id(), pos)
    }
}

/// `level.isUnobstructed(reinforcement)`: no entity that blocks building in its box.
fn unobstructed(h: &Hurt<'_, '_>, kind: Kind, at: Vec3) -> bool {
    let aabb = Dimensions::of(kind.type_id()).bounding_box(at);
    let mut blocked = false;
    h.ents.store.for_each_in(&aabb, false, |id| {
        blocked |= crate::block::dispenser::entity_blocks_building(h.ents, id);
    });
    !blocked
}

/// `hasNearbyAlivePlayer(x, y, z, range)`.
fn player_within(h: &Hurt<'_, '_>, at: Vec3, range: f64) -> bool {
    let area = vanillite_base::Aabb::new(
        at.x - range,
        at.y - range,
        at.z - range,
        at.x + range,
        at.y + range,
        at.z + range,
    );
    let mut found = false;
    h.ents.store.for_each_in(&area, true, |id| {
        if let (Some(p), Some(e)) = (h.ents.player(id), h.ents.store.get(id))
            && !p.survival.is_dead_or_dying()
            && p.game_mode != vanillite_config::GameMode::Spectator
            && e.pos().distance_to_sqr(at) < range * range
        {
            found = true;
        }
    });
    found
}

pub fn on_die(_h: &mut Hurt<'_, '_>) {}

// --- Players ----------------------------------------------------------------------------

pub fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
    let Some(m) = ix.ents.mob(ix.target) else {
        return Interaction::Pass;
    };
    if m.kind != Kind::ZombieVillager
        || crate::item::item_id(ix.stack).name() != "minecraft:golden_apple"
    {
        return Interaction::Pass;
    }
    if !m.effects.has(effects::MobEffect::WEAKNESS) {
        return Interaction::Consume;
    }
    crate::mob::animal::use_item(ix.ents, ix.player, ix.hand);
    let starter = ix.ents.store.get(ix.player).map(|e| e.uuid());
    let difficulty = ix.env.difficulty;
    let Some(m) = ix.ents.mob_mut(ix.target) else {
        return Interaction::Success;
    };
    let time = m.random.next_int_bounded(2401) + 3600;
    start_curing(m, starter, time, difficulty);
    crate::mob::animal::entity_event(ix.ents, ix.env, ix.target, 16);
    if let Some(mut e) = ix.ents.store.get_mut(ix.target) {
        e.mark_data_dirty();
    }
    Interaction::Success
}

/// `startConverting`: weakness goes, strength for the cure time.
fn start_curing(m: &mut MobData, starter: Option<u128>, time: i32, difficulty: Difficulty) {
    let z = zs_mut(m);
    z.cure_starter = starter;
    z.cure_time = time;
    z.curing = true;
    m.effects.remove(effects::MobEffect::WEAKNESS);
    // `Math.min(difficulty.getId() - 1, 0)`: level 0 (no zombie villagers in peaceful).
    let _ = difficulty;
    let amplifier = 0;
    effects::living::add_to_mob(
        m,
        effects::EffectInstance::new(effects::MobEffect::STRENGTH, time, amplifier),
    );
}

/// `ZombieVillager.removeWhenFarAway`: not while curing or with trade experience.
pub fn zombie_villager_removes_when_far(m: &MobData) -> bool {
    zs(m).is_none_or(|z| !z.curing && z.villager_xp == 0)
}

/// What zombies pick up (`canHoldItem`, `wantsToPickUp`, preferred weapons).
pub fn pick_up_items(t: &mut Tick<'_, '_>) {
    let rules = match t.mob_ref().kind {
        Kind::Giant => return,
        Kind::Drowned => PickupRules {
            wants: |m, s| zombie_wants(m, s) && !in_tag(s, "minecraft:spears"),
            preferred_weapon: Some("minecraft:drowned_preferred_weapons"),
        },
        Kind::ZombifiedPiglin => PickupRules {
            wants: |m, s| zombie_can_hold(m, s),
            preferred_weapon: None,
        },
        _ => PickupRules {
            wants: zombie_wants,
            preferred_weapon: None,
        },
    };
    equipment::pick_up_items(t, &rules, None, (1.0, 0.0, 1.0));
}

fn in_tag(s: &ItemStack, tag: &str) -> bool {
    crate::player::play::game_data().is_some_and(|d| d.item_in_tag(s.item, tag))
}

/// `Zombie.canHoldItem`: babies riding something do not take eggs.
fn zombie_can_hold(m: &MobData, s: &ItemStack) -> bool {
    !(m.baby && in_tag(s, "minecraft:eggs"))
}

/// `Zombie.wantsToPickUp`: never glow ink sacs.
fn zombie_wants(m: &MobData, s: &ItemStack) -> bool {
    crate::item::item_id(s).name() != "minecraft:glow_ink_sac" && zombie_can_hold(m, s)
}

// --- Client data and storage ------------------------------------------------------------

pub fn encode_data(w: &mut DataWriter<'_>, m: &MobData) {
    if m.kind == Kind::Giant {
        return;
    }
    let z = zs(m);
    w.boolean(16, m.baby, false);
    w.int(17, 0, 0);
    w.boolean(18, z.is_some_and(|z| z.converting), false);
    if m.kind == Kind::ZombieVillager {
        let z = z.cloned().unwrap_or_default();
        w.boolean(19, z.curing, false);
        w.raw(20, serializer::VILLAGER_DATA, false, |o| {
            vanillite_protocol::codec::write_varint(o, i32::from(z.villager_type));
            vanillite_protocol::codec::write_varint(o, i32::from(z.profession));
            vanillite_protocol::codec::write_varint(o, z.villager_level.max(1));
        });
        w.boolean(21, z.data_finalized, false);
    }
}

/// The conversion data of a zombie (`DrownedConversionTime`, `InWaterTime`).
pub fn save_conversion(m: &MobData, out: &mut Compound) {
    let z = zs(m).cloned().unwrap_or_default();
    out.insert(
        "DrownedConversionTime",
        if z.converting { z.conversion_time } else { -1 },
    );
    out.insert(
        "InWaterTime",
        if m.fluids.eye_in_water {
            z.in_water_time
        } else {
            -1
        },
    );
}

/// Reads the conversion data of a zombie.
pub fn load_conversion(m: &mut MobData, tag: &Compound) {
    let int = |k: &str| crate::mob::entity_nbt::get_int(tag, k);
    let z = zs_mut(m);
    z.in_water_time = int("InWaterTime").unwrap_or(0);
    match int("DrownedConversionTime") {
        Some(t) if t != -1 => {
            z.conversion_time = t;
            z.converting = true;
        }
        _ => z.converting = false,
    }
}

pub fn save(m: &MobData, out: &mut Compound) {
    if m.kind == Kind::Giant {
        return;
    }
    out.insert("IsBaby", m.baby);
    out.insert("CanBreakDoors", m.can_break_doors);
    save_conversion(m, out);
    save_attributes(m, out);
    let z = zs(m).cloned().unwrap_or_default();
    match m.kind {
        Kind::ZombieVillager => {
            let mut data = Compound::new();
            let name = |r: &vanillite_data::registry::Registry, i: u16| {
                r.entries
                    .get(usize::from(i))
                    .copied()
                    .unwrap_or("minecraft:none")
            };
            data.insert(
                "type",
                name(&vanillite_data::registry::VILLAGER_TYPE, z.villager_type),
            );
            data.insert(
                "profession",
                name(&vanillite_data::registry::VILLAGER_PROFESSION, z.profession),
            );
            data.insert("level", z.villager_level.max(1));
            out.insert("VillagerData", data);
            out.insert("VillagerDataFinalized", z.data_finalized);
            out.insert("ConversionTime", if z.curing { z.cure_time } else { -1 });
            if let Some(u) = z.cure_starter {
                out.insert("ConversionPlayer", uuid_ints(u));
            }
            out.insert("Xp", z.villager_xp);
            if let Some(g) = &z.gossips {
                out.insert("Gossips", g.save());
            }
            if let Some(o) = &z.offers {
                crate::mob::fam::village::merchant::save_offers(o, out);
            }
        }
        Kind::ZombifiedPiglin => common::save_anger(m, out),
        _ => {}
    }
}

/// `LivingEntity.addAdditionalSaveData`'s `attributes` of the zombies: the spawn bonuses of
/// `handleAttributes` (leader health and reinforcements, follow range, knockback resistance)
/// as the values they make, without effect modifiers.
pub fn save_attributes(m: &MobData, out: &mut Compound) {
    let a = m.base_attrs.as_deref().unwrap_or(&m.attrs);
    let entries = [
        ("minecraft:follow_range", a.follow_range),
        ("minecraft:knockback_resistance", a.knockback_resistance),
        ("minecraft:max_health", a.max_health),
        (
            "minecraft:spawn_reinforcements",
            zs(m).map_or(0.0, |z| z.reinforcement),
        ),
    ];
    let list = entries
        .iter()
        .map(|&(id, base)| {
            let mut c = Compound::new();
            c.insert("id", id);
            c.insert("base", base);
            Tag::Compound(c)
        })
        .collect();
    out.insert("attributes", vanillite_nbt::List(list));
}

/// The value of a saved attribute: its base with the modifiers applied (`AttributeInstance`
/// order: add, multiply base, multiply total).
fn attribute_value(c: &Compound) -> Option<f64> {
    let base = crate::mob::entity_nbt::get_double(c, "base")?;
    let mut mods = Vec::new();
    if let Some(Tag::List(l)) = c.get("modifiers") {
        for t in &l.0 {
            if let Tag::Compound(m) = t
                && let Some(amount) = crate::mob::entity_nbt::get_double(m, "amount")
            {
                mods.push((
                    m.get("operation").and_then(Tag::as_str).unwrap_or(""),
                    amount,
                ));
            }
        }
    }
    let mut v = base;
    for (op, a) in &mods {
        if *op == "add_value" {
            v += a;
        }
    }
    let mut r = v;
    for (op, a) in &mods {
        if *op == "add_multiplied_base" {
            r += v * a;
        }
    }
    for (op, a) in &mods {
        if *op == "add_multiplied_total" {
            r *= 1.0 + a;
        }
    }
    Some(r)
}

/// Reads the zombie attributes [`save_attributes`] writes (before the health, like
/// `LivingEntity.readAdditionalSaveData`): the shared attributes, and the reinforcement
/// chance, which only zombies keep.
pub fn load_attributes(m: &mut MobData, tag: &Compound) {
    crate::mob::entity_nbt::load_attributes(m, tag.get("attributes"));
    let Some(Tag::List(l)) = tag.get("attributes") else {
        return;
    };
    for t in &l.0 {
        if let Tag::Compound(c) = t
            && c.get("id").and_then(Tag::as_str) == Some("minecraft:spawn_reinforcements")
            && let Some(v) = attribute_value(c)
        {
            zs_mut(m).reinforcement = v;
        }
    }
}

fn uuid_ints(u: u128) -> Vec<i32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let v = vec![
        (u >> 96) as i32,
        (u >> 64) as i32,
        (u >> 32) as i32,
        u as i32,
    ];
    v
}

pub fn load(m: &mut MobData, tag: &Compound) {
    if m.kind == Kind::Giant {
        return;
    }
    let boolean = |k: &str| crate::mob::entity_nbt::get_bool(tag, k).unwrap_or(false);
    super::set_baby(m, boolean("IsBaby"));
    set_can_break_doors(m, boolean("CanBreakDoors"));
    load_conversion(m, tag);
    match m.kind {
        Kind::ZombieVillager => {
            let int = |k: &str| crate::mob::entity_nbt::get_int(tag, k);
            let finalized = boolean("VillagerDataFinalized");
            let data = match tag.get("VillagerData") {
                Some(Tag::Compound(c)) => Some(c.clone()),
                _ => None,
            };
            let z = zs_mut(m);
            if finalized || data.is_some() {
                z.data_finalized = true;
                if let Some(d) = data {
                    let id = |r: &vanillite_data::registry::Registry, k: &str, def: &str| {
                        let name = d.get(k).and_then(Tag::as_str).unwrap_or(def);
                        r.id(name).unwrap_or(0)
                    };
                    z.villager_type = id(
                        &vanillite_data::registry::VILLAGER_TYPE,
                        "type",
                        "minecraft:plains",
                    );
                    z.profession = id(
                        &vanillite_data::registry::VILLAGER_PROFESSION,
                        "profession",
                        "minecraft:none",
                    );
                    z.villager_level =
                        crate::mob::entity_nbt::get_int(&d, "level").map_or(1, |v| v.max(1));
                }
            }
            z.villager_xp = int("Xp").unwrap_or(0);
            if tag.get("Gossips").is_some() {
                z.gossips = Some(crate::mob::fam::village::gossip::Gossips::load(
                    tag.get("Gossips"),
                ));
            }
            z.offers = crate::mob::fam::village::merchant::load_offers(tag);
            match int("ConversionTime") {
                Some(t) if t != -1 => {
                    z.curing = true;
                    z.cure_time = t;
                    z.cure_starter = crate::mob::nbt::uuid_key_of(tag, "ConversionPlayer");
                }
                _ => {
                    z.curing = false;
                    z.cure_time = -1;
                }
            }
        }
        Kind::ZombifiedPiglin => common::load_anger(m, tag, 0),
        _ => {}
    }
}

// --- Goals --------------------------------------------------------------------------------

/// `BlockPos.closerToCenterThan(pos, dist)`.
fn closer_to_center(p: BlockPos, pos: Vec3, dist: f64) -> bool {
    let c = Vec3::new(
        f64::from(p.x) + 0.5,
        f64::from(p.y) + 0.5,
        f64::from(p.z) + 0.5,
    );
    c.distance_to_sqr(pos) < dist * dist
}

/// `MoveThroughVillageGoal(mob, 1.0, true, 4, this::canBreakDoors).canUse`: at night near a
/// village, a path to an occupied village point of interest it has not visited yet.
fn village_can_use(t: &mut Tick<'_, '_>, poi: &mut BlockPos) -> bool {
    if !t.mob_ref().nav.is_ground() {
        return false;
    }
    // `updateVisited`.
    if let Some(v) = zs_mut(t.mob()).village.as_deref_mut()
        && v.visited.len() > 15
    {
        v.visited.remove(0);
    }
    if t.cx.env.is_bright_outside() {
        return false;
    }
    let pos = t.block_pos();
    if !t.ents.poi.is_close_to_village(pos, 6) {
        return false;
    }
    let visited = zs(t.mob_ref())
        .and_then(|z| z.village.as_ref())
        .map(|v| v.visited.clone())
        .unwrap_or_default();
    let find = |t: &Tick<'_, '_>, at: BlockPos| {
        t.ents.poi.find(
            crate::poi::PoiType::is_village,
            |p| !visited.contains(&p),
            at,
            10,
            crate::poi::Occupancy::IsOccupied,
        )
    };
    let land = randompos::land_pos_weighted(t, 15, 7, |t, p| {
        if !t.ents.poi.is_village(p) {
            return f64::NEG_INFINITY;
        }
        find(t, p).map_or(f64::NEG_INFINITY, |q| -q.dist_sqr(pos))
    });
    let Some(land) = land else {
        return false;
    };
    let Some(target) = find(t, BlockPos::containing(land.x, land.y, land.z)) else {
        return false;
    };
    *poi = target;
    let doors = t.mob_ref().can_break_doors;
    t.mob().nav.can_open_doors = doors;
    let mut path = nav::create_path_to(t, target, 0);
    t.mob().nav.can_open_doors = true;
    if path.is_none() {
        let towards = randompos::bottom_center(target);
        let Some(step) = randompos::default_pos_towards(t, 10, 7, towards, randompos::half_pi())
        else {
            return false;
        };
        t.mob().nav.can_open_doors = doors;
        path = nav::create_path_to(t, BlockPos::containing(step.x, step.y, step.z), 0);
        t.mob().nav.can_open_doors = true;
        if path.is_none() {
            return false;
        }
    }
    if let Some(p) = &path {
        let door = p.nodes.iter().find(|n| {
            crate::mob::movement::is_wooden_door(t.level.block(BlockPos::new(n.x, n.y + 1, n.z)))
        });
        if let Some(n) = door {
            let at = BlockPos::new(n.x, n.y, n.z);
            path = nav::create_path_to(t, at, 0);
        }
    }
    let found = path.is_some();
    village_mut(t.mob()).path = path;
    found
}

pub fn goal_flags(g: &Goal) -> u8 {
    use crate::mob::goals::flag::{JUMP, LOOK, MOVE, TARGET};
    match g {
        Goal::DrownedMelee(_) => MOVE | LOOK,
        Goal::DrownedTrident(c) => c.flags(),
        Goal::DrownedGoToWater { .. } => MOVE,
        Goal::DrownedGoToBeach(_) => MOVE | JUMP,
        Goal::HurtByIgnoringDrowned(_) => TARGET,
        Goal::SpearUse(_) => MOVE | LOOK,
        Goal::MoveThroughVillage { .. } => MOVE,
        _ => 0,
    }
}

pub fn goal_every_tick(g: &Goal) -> bool {
    matches!(
        g,
        Goal::DrownedMelee(_) | Goal::DrownedTrident(_) | Goal::DrownedGoToBeach(_)
    )
}

pub fn goal_can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::MoveThroughVillage { poi } => village_can_use(t, poi),
        Goal::SpearUse(_) => spear_can_use(t),
        Goal::DrownedMelee(melee) => {
            melee.with(t, goals::can_use)
                && t.mob_ref()
                    .target
                    .is_some_and(|id| drowned_ok_target(t, id))
        }
        Goal::DrownedTrident(c) => {
            common::can_use(t, c) && t.mob_ref().equipment.holds(items::TRIDENT.0)
        }
        Goal::DrownedGoToWater { wanted, .. } => {
            if !t.cx.env.is_bright_outside() || t.mob_ref().fluids.in_water() {
                return false;
            }
            let pos = t.block_pos();
            for _ in 0..10 {
                let r = &mut t.mob().random;
                let dx = r.next_int_bounded(20) - 10;
                let dy = 2 - r.next_int_bounded(8);
                let dz = r.next_int_bounded(20) - 10;
                let p = BlockPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
                if t.level.block(p).block() == vanillite_data::block::ids::WATER {
                    *wanted = randompos::bottom_center(p);
                    return true;
                }
            }
            false
        }
        Goal::DrownedGoToBeach(mtb) => {
            mtb.can_use(t, beach_target, MoveToBlock::default_next_start_tick)
                && !t.cx.env.is_bright_outside()
                && t.mob_ref().fluids.in_water()
                && t.pos().y >= f64::from(t.cx.env.sea_level - 3)
        }
        Goal::DrownedSwimUp { .. } => {
            !t.cx.env.is_bright_outside()
                && t.mob_ref().fluids.in_water()
                && t.pos().y < f64::from(t.cx.env.sea_level - 2)
        }
        Goal::HurtByIgnoringDrowned(h) => {
            let by = t.mob_ref().last_hurt_by_mob;
            if by.is_some_and(|b| t.ents.mob(b).is_some_and(|m| m.kind == Kind::Drowned)) {
                return false;
            }
            h.with(t, goals::can_use)
        }
        _ => false,
    }
}

pub fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::MoveThroughVillage { poi } => {
            !t.mob_ref().nav.is_done() && {
                let width = f64::from(t.core().dims().width);
                !closer_to_center(*poi, t.pos(), width + 4.0)
            }
        }
        Goal::SpearUse(s) => spear_can_continue(t, s),
        Goal::DrownedMelee(melee) => {
            melee.with(t, goals::can_continue)
                && t.mob_ref()
                    .target
                    .is_some_and(|id| drowned_ok_target(t, id))
        }
        Goal::DrownedTrident(c) => common::can_continue(t, c),
        Goal::DrownedGoToWater { .. } => !t.nav_ref().is_done(),
        Goal::DrownedGoToBeach(mtb) => mtb.can_continue(t, beach_target),
        Goal::DrownedSwimUp { stuck, .. } => {
            !*stuck
                && !t.cx.env.is_bright_outside()
                && t.mob_ref().fluids.in_water()
                && t.pos().y < f64::from(t.cx.env.sea_level - 2)
        }
        Goal::HurtByIgnoringDrowned(h) => h.with(t, goals::can_continue),
        _ => false,
    }
}

pub fn goal_start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::MoveThroughVillage { .. } => {
            let path = village_mut(t.mob()).path.take();
            nav::move_to_path(t, path, 1.0);
        }
        Goal::SpearUse(s) => spear_start(t, s),
        Goal::DrownedMelee(melee) => melee.with(t, goals::start),
        Goal::DrownedTrident(c) => {
            common::start(t, c);
            goals::set_aggressive(t, true);
            crate::mob::projectile::start_using_item(t);
        }
        Goal::DrownedGoToWater { speed, wanted } => {
            nav::move_to_pos(t, *wanted, *speed);
        }
        Goal::DrownedGoToBeach(mtb) => {
            zs_mut(t.mob()).searching_for_land = false;
            mtb.start(t);
        }
        Goal::DrownedSwimUp { stuck, .. } => {
            zs_mut(t.mob()).searching_for_land = true;
            *stuck = false;
        }
        Goal::HurtByIgnoringDrowned(h) => h.with(t, goals::start),
        _ => {}
    }
}

pub fn goal_stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::MoveThroughVillage { poi } => {
            if t.mob_ref().nav.is_done() || closer_to_center(*poi, t.pos(), 4.0) {
                let poi = *poi;
                village_mut(t.mob()).visited.push(poi);
            }
        }
        Goal::SpearUse(s) => spear_stop(t, s),
        Goal::DrownedMelee(melee) => melee.with(t, goals::stop),
        Goal::DrownedTrident(c) => {
            common::stop(t, c);
            crate::mob::projectile::stop_using_item(t);
            goals::set_aggressive(t, false);
        }
        Goal::DrownedSwimUp { .. } => zs_mut(t.mob()).searching_for_land = false,
        Goal::HurtByIgnoringDrowned(h) => h.with(t, goals::stop),
        _ => {}
    }
}

pub fn goal_tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::SpearUse(s) => spear_tick(t, s),
        Goal::DrownedMelee(melee) => melee.with(t, goals::tick),
        Goal::DrownedTrident(c) => common::tick(t, c),
        Goal::DrownedGoToBeach(mtb) => {
            let b = mtb.block_pos;
            mtb.tick(t, BlockPos::new(b.x, b.y + 1, b.z), 1.0);
        }
        Goal::DrownedSwimUp { speed, stuck } => {
            let sea = t.cx.env.sea_level;
            if t.pos().y < f64::from(sea - 1) && (t.nav_ref().is_done() || close_to_next_pos(t)) {
                let p = t.pos();
                let towards = Vec3::new(p.x, f64::from(sea - 1), p.z);
                match randompos::default_pos_towards(t, 4, 8, towards, randompos::half_pi()) {
                    Some(next) => {
                        nav::move_to_pos(t, next, *speed);
                    }
                    None => *stuck = true,
                }
            }
        }
        Goal::HurtByIgnoringDrowned(h) => h.with(t, goals::tick),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_construct_with_goals() {
        for kind in [
            Kind::Husk,
            Kind::Drowned,
            Kind::ZombieVillager,
            Kind::ZombifiedPiglin,
        ] {
            let m = MobData::new(kind, 7);
            assert!(!m.goals.slots.is_empty(), "{kind:?}");
            assert!(!m.targets.slots.is_empty(), "{kind:?}");
        }
        assert!(MobData::new(Kind::Giant, 1).goals.slots.is_empty());
    }

    #[test]
    fn finalize_is_deterministic() {
        let run = || {
            let mut m = MobData::new(Kind::Husk, 42);
            let mut ctx = SpawnContext::simple(Difficulty::Hard, SpawnReason::Command);
            kinds::finalize_spawn(
                &mut m,
                &mut ctx,
                &mut vanillite_base::random::LegacyRandom::new(1),
            );
            (
                m.baby,
                m.equipment.clone(),
                m.attrs.follow_range,
                zs(&m).map(|z| z.reinforcement),
            )
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn spawn_group_shares_baby_status() {
        // `ZombieGroupData`: the first member rolls, the others take it over.
        for seed in 0..40 {
            let mut ctx = SpawnContext::simple(Difficulty::Normal, SpawnReason::Natural);
            let mut random = vanillite_base::random::LegacyRandom::new(seed);
            let mut first = MobData::new(Kind::Zombie, seed);
            kinds::finalize_spawn(&mut first, &mut ctx, &mut random);
            assert_eq!(ctx.group, GroupData::Zombie(first.baby));
            for i in 1..4 {
                let mut m = MobData::new(Kind::Zombie, seed * 10 + i);
                kinds::finalize_spawn(&mut m, &mut ctx, &mut random);
                assert_eq!(m.baby, first.baby);
            }
        }
    }

    #[test]
    fn zombified_piglins_hold_gold() {
        let mut m = MobData::new(Kind::ZombifiedPiglin, 3);
        let mut ctx = SpawnContext::simple(Difficulty::Normal, SpawnReason::Command);
        kinds::finalize_spawn(
            &mut m,
            &mut ctx,
            &mut vanillite_base::random::LegacyRandom::new(1),
        );
        let name = crate::item::item_id(m.equipment.get(EquipmentSlot::MainHand)).name();
        assert!(name.starts_with("minecraft:golden_"), "{name}");
        assert_eq!(zs(&m).map(|z| z.reinforcement), Some(0.0));
    }

    #[test]
    fn nbt_round_trip() {
        let mut m = MobData::new(Kind::ZombieVillager, 9);
        start_curing(&mut m, Some(5), 4000, Difficulty::Normal);
        super::super::set_baby(&mut m, true);
        let mut out = Compound::new();
        save(&m, &mut out);
        let mut n = MobData::new(Kind::ZombieVillager, 10);
        load(&mut n, &out);
        let (a, b) = (zs(&m).cloned().unwrap(), zs(&n).cloned().unwrap());
        assert_eq!(
            (a.curing, a.cure_time, a.cure_starter),
            (b.curing, b.cure_time, b.cure_starter)
        );
        assert_eq!(
            (a.villager_type, a.profession),
            (b.villager_type, b.profession)
        );
        assert!(n.baby);
    }

    #[test]
    fn babies_are_faster_and_small() {
        let mut m = MobData::new(Kind::Husk, 1);
        let speed = m.attrs.movement_speed;
        super::super::set_baby(&mut m, true);
        assert!((m.attrs.movement_speed - speed * 1.5).abs() < 1e-9);
        assert_eq!(dimensions(&m).map(|d| d.height), Some(0.98));
    }

    #[test]
    fn villager_types_by_biome() {
        assert_eq!(
            villager_type_of_biome("minecraft:desert"),
            "minecraft:desert"
        );
        assert_eq!(villager_type_of_biome("minecraft:grove"), "minecraft:snow");
        assert_eq!(
            villager_type_of_biome("minecraft:forest"),
            "minecraft:plains"
        );
    }

    /// The ids of the synched data items written.
    fn data_ids(m: &MobData) -> Vec<u8> {
        let mut out = Vec::new();
        encode_data(&mut DataWriter::new(&mut out, true), m);
        let mut ids = Vec::new();
        let mut i = 0;
        while i < out.len() {
            ids.push(out[i]);
            let serializer = out[i + 1];
            i += 2;
            i += match i32::from(serializer) {
                serializer::BOOLEAN => 1,
                serializer::INT => 1,
                serializer::VILLAGER_DATA => 3,
                _ => unreachable!("unexpected serializer"),
            };
        }
        ids
    }

    #[test]
    fn synched_data_ids() {
        let husk = MobData::new(Kind::Husk, 1);
        assert_eq!(data_ids(&husk), vec![16, 17, 18]);
        let zv = MobData::new(Kind::ZombieVillager, 1);
        assert_eq!(data_ids(&zv), vec![16, 17, 18, 19, 20, 21]);
    }

    #[test]
    fn drowning_conversion_round_trip() {
        let mut m = MobData::new(Kind::Zombie, 1);
        let z = zs_mut(&mut m);
        z.converting = true;
        z.conversion_time = 123;
        let mut out = Compound::new();
        save_conversion(&m, &mut out);
        let mut n = MobData::new(Kind::Zombie, 2);
        load_conversion(&mut n, &out);
        assert!(zs(&n).is_some_and(|z| z.converting && z.conversion_time == 123));
    }

    #[test]
    fn attributes_round_trip() {
        let mut m = MobData::new(Kind::Zombie, 1);
        m.attrs.max_health = 50.0;
        m.attrs.follow_range = 70.0;
        m.attrs.knockback_resistance = 0.03;
        zs_mut(&mut m).reinforcement = 0.6;
        let mut out = Compound::new();
        save_attributes(&m, &mut out);
        let mut n = MobData::new(Kind::Zombie, 2);
        load_attributes(&mut n, &out);
        let same = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(same(n.attrs.max_health, 50.0));
        assert!(same(n.attrs.follow_range, 70.0));
        assert!(same(n.attrs.knockback_resistance, 0.03));
        assert!(zs(&n).is_some_and(|z| same(z.reinforcement, 0.6)));
    }
}
