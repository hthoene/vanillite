//! `Fox`: red and snow foxes, sleeping in the shade by day, stalking and pouncing on prey,
//! carrying an item in the mouth, eating berries, trusting the players that bred it and
//! defending them.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::{Compound, List, Tag};
use vanillite_world::entity::{EntityFlags, EntityId};

use super::{G, Goal as FamGoal, MeleeKind, SpawnContext, common, fam, st, st_mut};
use crate::item::{EquipmentSlot, ItemStack};
use crate::level::Level;
use crate::mob::common::{CommonGoal, MoveToBlock, TargetFilter};
use crate::mob::equipment;
use crate::mob::fam::{GroupData, Hurt, SpawnReason};
use crate::mob::goals::{Goal as BaseGoal, TargetState, flag, reduced};
use crate::mob::kinds::Kind;
use crate::mob::{MobData, Tick, nav};
use crate::player::encode::DataWriter;
use crate::survival::rules::Rule;

const SITTING: u8 = 1;
const CROUCHING: u8 = 4;
const INTERESTED: u8 = 8;
const POUNCING: u8 = 16;
const SLEEPING: u8 = 32;
const FACEPLANTED: u8 = 64;
const DEFENDING: u8 = 128;

/// The fox goals.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// `FoxFloatGoal` (clears the states).
    Float,
    /// `FaceplantGoal`.
    Faceplant { countdown: i32 },
    /// `FoxPanicGoal` (not while defending).
    Panic { pos: Vec3 },
    /// `AvoidEntityGoal` while not defending.
    Avoid(CommonGoal),
    /// `StalkPreyGoal`.
    StalkPrey,
    /// `FoxPounceGoal`.
    Pounce,
    /// `SeekShelterGoal`.
    SeekShelter { interval: i32, wanted: Vec3 },
    /// `FoxMeleeAttackGoal` guard (the melee itself is the shared goal).
    Sleep { countdown: i32 },
    /// `FoxStrollThroughVillageGoal`: the reduced interval and the wanted village spot.
    StrollThroughVillage {
        interval: i32,
        wanted: Option<BlockPos>,
    },
    /// `FoxEatBerriesGoal`.
    EatBerries { base: MoveToBlock, waited: i32 },
    /// `FoxSearchForItemsGoal`.
    SearchForItems,
    /// `PerchAndSearchGoal`.
    PerchAndSearch {
        rel_x: f64,
        rel_z: f64,
        look_time: i32,
        looks_remaining: i32,
    },
    /// `DefendTrustedTargetGoal`.
    DefendTrusted {
        state: TargetState,
        timestamp: i32,
        found: Option<EntityId>,
    },
}

pub fn flags(g: &Goal) -> u8 {
    use flag::*;
    match g {
        Goal::Float => JUMP,
        Goal::Faceplant { .. } => LOOK | JUMP | MOVE,
        Goal::Panic { .. }
        | Goal::Avoid(_)
        | Goal::SeekShelter { .. }
        | Goal::StrollThroughVillage { .. } => MOVE,
        Goal::StalkPrey | Goal::PerchAndSearch { .. } => MOVE | LOOK,
        Goal::Pounce => JUMP | MOVE,
        Goal::Sleep { .. } => MOVE | LOOK | JUMP,
        Goal::EatBerries { .. } => MOVE | JUMP,
        Goal::SearchForItems => MOVE,
        Goal::DefendTrusted { .. } => TARGET,
    }
}

pub fn every_tick(g: &Goal) -> bool {
    matches!(g, Goal::Float | Goal::Pounce | Goal::EatBerries { .. })
}

fn flag_of(m: &MobData, f: u8) -> bool {
    st(m).fox_flags & f != 0
}

fn set_flag(t: &mut Tick<'_, '_>, f: u8, on: bool) {
    let s = st_mut(t.mob());
    let before = s.fox_flags;
    if on {
        s.fox_flags |= f;
    } else {
        s.fox_flags &= !f;
    }
    if s.fox_flags != before {
        t.core_mut().mark_data_dirty();
    }
}

/// `clearStates`.
fn clear_states(t: &mut Tick<'_, '_>) {
    let s = st_mut(t.mob());
    if s.fox_flags != 0 {
        s.fox_flags = 0;
        t.core_mut().mark_data_dirty();
    }
}

/// `FoxPounceGoal.tick`'s pitch: back to level when it hardly moves up or down, else along its
/// motion (rising steeper while jumping).
fn pounce_pitch(t: &mut Tick<'_, '_>) {
    let v = t.velocity();
    let mut rot = t.core().rot();
    if v.y * v.y < f64::from(0.03_f32) && rot.x_rot != 0.0 {
        rot.x_rot += 0.2 * vanillite_base::math::wrap_degrees_f(0.0 - rot.x_rot);
    } else {
        let horizontal = (v.x * v.x + v.z * v.z).sqrt();
        let bias: f32 = if t.mob_ref().jumping && v.y > 0.0 {
            6.5
        } else {
            1.0
        };
        let y = v.y * f64::from(bias);
        let len = (horizontal * horizontal + y * y).sqrt();
        if len <= f64::from(1.0e-5_f32) {
            return;
        }
        // `Math.signum(-y)`: a zero keeps its sign.
        let sign = if y == 0.0 {
            -y
        } else if y < 0.0 {
            1.0
        } else {
            -1.0
        };
        let r = sign * (horizontal / len).acos() * 180.0 / f64::from(std::f32::consts::PI);
        #[allow(clippy::cast_possible_truncation)]
        {
            rot.x_rot = nav::clamp_x_rot(r as f32);
        }
    }
    t.core_mut().set_rot(rot);
}

/// `FoxMeleeAttackGoal.canUse`'s own part.
pub(crate) fn can_melee(m: &MobData) -> bool {
    !flag_of(m, SITTING)
        && !flag_of(m, SLEEPING)
        && !flag_of(m, CROUCHING)
        && !flag_of(m, FACEPLANTED)
}

/// `FoxMeleeAttackGoal.start`: no longer interested.
pub(crate) fn melee_start(t: &mut Tick<'_, '_>) {
    set_flag(t, INTERESTED, false);
}

/// `Fox.tick` after `super.tick()`: the crouch amount.
pub fn after_tick(t: &mut Tick<'_, '_>) {
    let crouching = flag_of(t.mob_ref(), CROUCHING);
    let s = st_mut(t.mob());
    if crouching {
        s.crouch_amount = (s.crouch_amount + 0.2).min(5.0);
    } else {
        s.crouch_amount = 0.0;
    }
}

/// `FoxLookControl.tick`: a sleeping fox does not look around; a pouncing, crouching,
/// interested or faceplanted one keeps its pitch (`resetXRotOnTick`).
pub fn look_control(t: &mut Tick<'_, '_>) {
    let m = t.mob_ref();
    if flag_of(m, SLEEPING) {
        return;
    }
    let reset = !flag_of(m, POUNCING)
        && !flag_of(m, CROUCHING)
        && !flag_of(m, INTERESTED)
        && !flag_of(m, FACEPLANTED);
    nav::default_look_tick(t, reset);
}

fn can_move(m: &MobData) -> bool {
    !flag_of(m, SLEEPING) && !flag_of(m, SITTING) && !flag_of(m, FACEPLANTED)
}

/// Whether the fox trusts an entity.
fn trusts(t: &Tick<'_, '_>, id: EntityId) -> bool {
    let uuid = t.ents.store.get(id).map(|e| e.uuid());
    uuid.is_some() && st(t.mob_ref()).trusted.contains(&uuid)
}

fn avoid_player(t: &Tick<'_, '_>, id: EntityId) -> bool {
    // `AVOID_PLAYERS`: not sneaking, not creative or spectator.
    crate::mob::common::is_survival_player(t, id)
        && !crate::mob::common::is_crouching(t, id)
        && !trusts(t, id)
        && !flag_of(t.mob_ref(), DEFENDING)
}

fn avoid_wolf(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .mob(id)
        .is_some_and(|m| m.kind == Kind::Wolf && !m.tame)
        && !flag_of(t.mob_ref(), DEFENDING)
}

fn avoid_bear(t: &Tick<'_, '_>, id: EntityId) -> bool {
    crate::mob::common::mob_of(t, id, Kind::PolarBear) && !flag_of(t.mob_ref(), DEFENDING)
}

fn land_prey(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .mob(id)
        .is_some_and(|m| matches!(m.kind, Kind::Chicken | Kind::Rabbit))
}

fn schooling_fish(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .mob(id)
        .is_some_and(|m| matches!(m.kind, Kind::Cod | Kind::Salmon | Kind::TropicalFish))
}

/// `registerGoals` of the fox (the prey targets follow the variant: `setTargetGoals`).
pub fn register_goals(m: &mut MobData) {
    let g = &mut m.goals;
    g.add(0, fam(FamGoal::Fox(Goal::Float)));
    g.add(0, fam(FamGoal::ClimbPowderSnow));
    g.add(1, fam(FamGoal::Fox(Goal::Faceplant { countdown: 0 })));
    g.add(2, fam(FamGoal::Fox(Goal::Panic { pos: Vec3::ZERO })));
    g.add(3, G::breed(1.0));
    let players: TargetFilter = avoid_player;
    let wolves: TargetFilter = avoid_wolf;
    let bears: TargetFilter = avoid_bear;
    g.add(
        4,
        fam(FamGoal::Fox(Goal::Avoid(CommonGoal::avoid(
            players, 16.0, 1.6, 1.4,
        )))),
    );
    g.add(
        4,
        fam(FamGoal::Fox(Goal::Avoid(CommonGoal::avoid(
            wolves, 8.0, 1.6, 1.4,
        )))),
    );
    g.add(
        4,
        fam(FamGoal::Fox(Goal::Avoid(CommonGoal::avoid(
            bears, 8.0, 1.6, 1.4,
        )))),
    );
    g.add(5, fam(FamGoal::Fox(Goal::StalkPrey)));
    g.add(6, fam(FamGoal::Fox(Goal::Pounce)));
    g.add(
        6,
        fam(FamGoal::Fox(Goal::SeekShelter {
            interval: reduced(100),
            wanted: Vec3::ZERO,
        })),
    );
    g.add(7, G::melee(MeleeKind::Plain, f64::from(1.2_f32), true));
    let countdown = m.random.next_int_bounded(reduced(140));
    m.goals.add(7, fam(FamGoal::Fox(Goal::Sleep { countdown })));
    let g = &mut m.goals;
    g.add(
        8,
        G::FollowParent {
            speed: 1.25,
            parent: None,
            recalc: 0,
        },
    );
    g.add(
        9,
        fam(FamGoal::Fox(Goal::StrollThroughVillage {
            interval: reduced(200),
            wanted: None,
        })),
    );
    g.add(
        10,
        fam(FamGoal::Fox(Goal::EatBerries {
            base: MoveToBlock::new(f64::from(1.2_f32), 12, 1),
            waited: 0,
        })),
    );
    g.add(
        10,
        G::Leap {
            yd: 0.4,
            target: None,
        },
    );
    g.add(11, G::stroll(1.0));
    g.add(11, fam(FamGoal::Fox(Goal::SearchForItems)));
    g.add(12, G::look_at_player(24.0));
    g.add(
        13,
        fam(FamGoal::Fox(Goal::PerchAndSearch {
            rel_x: 0.0,
            rel_z: 0.0,
            look_time: 0,
            looks_remaining: 0,
        })),
    );
    m.targets.add(
        3,
        fam(FamGoal::Fox(Goal::DefendTrusted {
            state: TargetState::new(false),
            timestamp: 0,
            found: None,
        })),
    );
    m.can_pick_up_loot = true;
    m.look.kind = crate::mob::nav::LookKind::Custom;
    m.nav.can_float = true;
    let follow = m.attrs.follow_range;
    m.nav.set_required_path_length(32.0, follow);
}

/// `setTargetGoals`: red foxes hunt land prey first, snow foxes fish.
pub fn set_target_goals(m: &mut MobData) {
    m.targets.slots.retain(|s| {
        !matches!(
            s.goal,
            BaseGoal::Common(CommonGoal::NearestAttackable { .. })
        )
    });
    let land: TargetFilter = land_prey;
    let turtles: TargetFilter = crate::mob::common::baby_turtle_on_land;
    let fish: TargetFilter = schooling_fish;
    let (land_p, fish_p) = if m.variant == 0 { (4, 6) } else { (6, 4) };
    if m.variant == 0 {
        m.targets
            .add(land_p, common(CommonGoal::nearest(10, false, land)));
        m.targets
            .add(land_p, common(CommonGoal::nearest(10, false, turtles)));
        m.targets
            .add(fish_p, common(CommonGoal::nearest(20, false, fish)));
    } else {
        m.targets
            .add(fish_p, common(CommonGoal::nearest(20, false, fish)));
        m.targets
            .add(land_p, common(CommonGoal::nearest(10, false, land)));
        m.targets
            .add(land_p, common(CommonGoal::nearest(10, false, turtles)));
    }
}

/// `finalizeSpawn`: the variant of the biome (`#spawns_snow_foxes`), kits from the third of
/// a group, the item in the mouth.
pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    let variant = i32::from(ctx.biome_is("minecraft:spawns_snow_foxes"));
    m.variant = if let GroupData::Fox(v) = ctx.group {
        v
    } else {
        ctx.group = GroupData::Fox(variant);
        variant
    };
    if ctx.group_index >= 2 {
        m.age = -24000;
        m.baby = true;
    }
    set_target_goals(m);
    // `populateDefaultEquipmentSlots`.
    if m.random.next_float() < 0.2 {
        let odds = m.random.next_float();
        let item = if odds < 0.05 {
            "minecraft:emerald"
        } else if odds < 0.2 {
            "minecraft:egg"
        } else if odds < 0.4 {
            if m.random.next_boolean() {
                "minecraft:rabbit_foot"
            } else {
                "minecraft:rabbit_hide"
            }
        } else if odds < 0.6 {
            "minecraft:wheat"
        } else if odds < 0.8 {
            "minecraft:leather"
        } else {
            "minecraft:feather"
        };
        m.equipment
            .set(EquipmentSlot::MainHand, equipment::stack(item));
    }
}

/// `getAmbientSound` (the night screech is played in `ai_step`).
pub fn ambient_sound(m: &MobData) -> Option<&'static str> {
    Some(if flag_of(m, SLEEPING) {
        "minecraft:entity.fox.sleep"
    } else {
        "minecraft:entity.fox.ambient"
    })
}

fn is_consumable_food(s: &ItemStack) -> bool {
    super::is_food_item(s)
}

/// `Fox.aiStep`: eating the food in the mouth, the aggro sound while defending.
pub fn ai_step(t: &mut Tick<'_, '_>) {
    if t.mob_ref().is_alive() {
        st_mut(t.mob()).ticks_since_eaten += 1;
        let held = t.mob_ref().equipment.get(EquipmentSlot::MainHand).clone();
        let can_eat = is_consumable_food(&held)
            && t.mob_ref().target.is_none()
            && t.on_ground()
            && !flag_of(t.mob_ref(), SLEEPING);
        if can_eat {
            let since = st(t.mob_ref()).ticks_since_eaten;
            if since > 600 {
                // `finishUsingItem`: the food is eaten and heals.
                let heal = super::food_nutrition(&held).unwrap_or(0.0);
                let m = t.mob();
                super::heal(m, heal);
                let mut rest = held;
                rest.shrink(1);
                m.equipment.set(EquipmentSlot::MainHand, rest);
                st_mut(m).ticks_since_eaten = 0;
                crate::mob::equipment::broadcast(t);
            } else if since > 560 && t.mob().random.next_float() < 0.1 {
                crate::mob::kinds::play_sound(t, "minecraft:entity.fox.eat", 1.0, 1.0);
                let id = t.id;
                crate::mob::living::entity_event(t, id, 45);
            }
        }
        let target_alive = t.mob_ref().target.and_then(|id| t.other(id)).is_some();
        if !target_alive {
            set_flag(t, CROUCHING, false);
            set_flag(t, INTERESTED, false);
        }
    }
    if flag_of(t.mob_ref(), DEFENDING) && t.mob().random.next_float() < 0.05 {
        crate::mob::kinds::play_sound(t, "minecraft:entity.fox.aggro", 1.0, 1.0);
    }
}

/// `Fox.tick`: wakes up in water, when targeting or in thunder; faceplant particles.
pub fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
    let in_water = t.mob_ref().fluids.in_water();
    if in_water || t.mob_ref().target.is_some() || t.cx.env.thundering {
        set_flag(t, SLEEPING, false);
    }
    if in_water || flag_of(t.mob_ref(), SLEEPING) {
        set_flag(t, SITTING, false);
    }
    if flag_of(t.mob_ref(), FACEPLANTED) && t.mob().random.next_float() < 0.2 {
        let pos = t.block_pos();
        let state = t.level.block(pos);
        t.cx.actions.push(crate::mob::WorldAction::LevelEvent(
            2001,
            pos,
            i32::from(state.0),
        ));
    }
    if flag_of(t.mob_ref(), SLEEPING) || !can_move(t.mob_ref()) {
        let m = t.mob();
        m.jumping = false;
        m.xxa = 0.0;
        m.zza = 0.0;
    }
}

/// `Fox.doHurtTarget` with the bite sound of its melee goal.
pub fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
    let r = crate::mob::combat::base_mob_attack(t, target);
    crate::mob::kinds::play_sound(t, "minecraft:entity.fox.bite", 1.0, 1.0);
    Some(r)
}

/// `hurtServer` wakes the fox (its target).
pub fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
    if let Some(m) = h.ents.mob_mut(h.target) {
        st_mut(m).fox_flags &= !(SLEEPING | SITTING);
    }
    Some(amount)
}

/// `dropAllDeathLoot`: the item in the mouth drops first (equipment drops handle it with a
/// guaranteed chance).
pub fn on_die(_h: &mut Hurt<'_, '_>) {}

/// `getBreedOffspring`: a parent's variant; the players that fed the parents are trusted.
pub fn breed_offspring(
    parent: &MobData,
    partner: &MobData,
    child: &mut MobData,
    r: &mut LegacyRandom,
) {
    child.variant = if r.next_boolean() {
        parent.variant
    } else {
        partner.variant
    };
    let mut trusted = [None, None];
    if let Some(a) = parent.love_cause {
        trusted[0] = Some(a);
        if let Some(b) = partner.love_cause
            && b != a
        {
            trusted[1] = Some(b);
        }
    } else if let Some(b) = partner.love_cause {
        trusted[0] = Some(b);
    }
    st_mut(child).trusted = trusted;
    set_target_goals(child);
}

/// `addTrustedEntity`: the first free trusted slot, else the second.
pub fn add_trusted(m: &mut MobData, uuid: u128) {
    let s = st_mut(m);
    if s.trusted[0].is_none() {
        s.trusted[0] = Some(uuid);
    } else {
        s.trusted[1] = Some(uuid);
    }
    set_target_goals(m);
}

/// The fox's synched data.
pub fn encode(w: &mut DataWriter<'_>, m: &MobData) {
    let s = st(m);
    w.int(18, m.variant, 0);
    w.byte(19, s.fox_flags, 0);
    w.optional_uuid(20, s.trusted[0]);
    w.optional_uuid(21, s.trusted[1]);
}

/// `Fox.addAdditionalSaveData`.
pub fn save(m: &MobData, out: &mut Compound) {
    let s = st(m);
    let list: Vec<Tag> = s
        .trusted
        .iter()
        .flatten()
        .map(|&u| Tag::IntArray(super::uuid_ints(u)))
        .collect();
    out.insert("Trusted", List(list));
    out.insert("Sleeping", flag_of(m, SLEEPING));
    out.insert("Type", if m.variant == 1 { "snow" } else { "red" });
    out.insert("Sitting", flag_of(m, SITTING));
    out.insert("Crouching", flag_of(m, CROUCHING));
}

/// `Fox.readAdditionalSaveData`.
pub fn load(m: &mut MobData, tag: &Compound) {
    let mut trusted = [None, None];
    if let Some(Tag::List(l)) = tag.get("Trusted") {
        let mut i = 0;
        for t in &l.0 {
            if let Tag::IntArray(v) = t
                && v.len() == 4
                && i < 2
            {
                #[allow(clippy::cast_sign_loss)]
                let u = v
                    .iter()
                    .fold(0u128, |acc, &x| (acc << 32) | u128::from(x as u32));
                trusted[i] = Some(u);
                i += 1;
            }
        }
    }
    m.variant = i32::from(super::string(tag, "Type") == Some("snow"));
    let mut flags = 0;
    if super::boolean(tag, "Sleeping") {
        flags |= SLEEPING;
    }
    if super::boolean(tag, "Sitting") {
        flags |= SITTING;
    }
    if super::boolean(tag, "Crouching") {
        flags |= CROUCHING;
    }
    let s = st_mut(m);
    s.trusted = trusted;
    s.fox_flags = flags;
    // The generic loader reads `Sitting` as an order to sit; foxes are not tamable.
    m.order_to_sit = false;
    m.in_sitting_pose = false;
    set_target_goals(m);
}

/// `checkFoxSpawnRules`: `#foxes_spawnable_on` below and bright enough.
pub fn spawn_rules(level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    super::below_is(
        level,
        pos,
        &[
            "minecraft:grass_block",
            "minecraft:snow",
            "minecraft:snow_block",
            "minecraft:podzol",
            "minecraft:coarse_dirt",
        ],
    ) && super::bright_enough(level, pos, reason)
}

// --- Pickup -------------------------------------------------------------------------------

/// `canHoldItem`.
pub fn can_hold(m: &MobData, s: &ItemStack) -> bool {
    let held = m.equipment.get(EquipmentSlot::MainHand);
    held.is_empty()
        || (st(m).ticks_since_eaten > 0 && is_consumable_food(s) && !is_consumable_food(held))
}

/// `Fox.pickUpItem`: one item into the mouth, the rest and the old item thrown out.
pub fn pick_up(t: &mut Tick<'_, '_>, item: EntityId) -> bool {
    let Some(stack) = t.ents.item(item).map(|d| d.stack.clone()) else {
        return false;
    };
    if !can_hold(t.mob_ref(), &stack) {
        return false;
    }
    let count = stack.count;
    if count > 1 {
        let rest = stack.with_count(count - 1);
        crate::mob::equipment::spawn_at_location(t, rest, 0.0);
    }
    let old = t.mob_ref().equipment.get(EquipmentSlot::MainHand).clone();
    if !old.is_empty() {
        crate::mob::equipment::spawn_at_location(t, old, 1.0);
        crate::mob::kinds::play_sound(t, "minecraft:entity.fox.spit", 1.0, 1.0);
    }
    let m = t.mob();
    m.equipment
        .set(EquipmentSlot::MainHand, stack.with_count(1));
    m.equipment.set_guaranteed_drop(EquipmentSlot::MainHand);
    st_mut(m).ticks_since_eaten = 0;
    crate::mob::equipment::take(t, item, count);
    t.cx.discarded.push(item);
    crate::mob::equipment::broadcast(t);
    true
}

// --- Goals --------------------------------------------------------------------------------

/// `Fox.isPathClear`: nothing solid in the three blocks above the line to the target.
fn path_clear(t: &Tick<'_, '_>, target: Vec3) -> bool {
    let p = t.pos();
    let (zd, xd) = (target.z - p.z, target.x - p.x);
    let slope = zd / xd;
    for i in 0..6 {
        #[allow(clippy::cast_precision_loss)]
        let f = f64::from(i as f32 / 6.0);
        let z = if slope == 0.0 { 0.0 } else { zd * f };
        let x = if slope == 0.0 { xd * f } else { z / slope };
        for j in 1..4 {
            let pos = BlockPos::containing(p.x + x, p.y + f64::from(j), p.z + z);
            let state = t.level.block(pos);
            if !(state.is_air() || state.has(vanillite_data::block::flags::REPLACEABLE)) {
                return false;
            }
        }
    }
    true
}

/// `FoxBehaviorGoal.alertable`: a living entity within 12 that would wake the fox.
fn alertable(t: &Tick<'_, '_>) -> bool {
    let area = t.aabb().inflate(12.0, 6.0, 12.0);
    let p = t.pos();
    let me = t.id;
    let ents = &*t.ents;
    let mut found = false;
    ents.store.for_each_in(&area, false, |id| {
        if found || id == me {
            return;
        }
        let Some(e) = ents.store.get(id) else {
            return;
        };
        if e.pos().distance_to_sqr(p) > 144.0 {
            return;
        }
        if let Some(m) = ents.mob(id) {
            if !m.is_alive() || m.kind == Kind::Fox {
                return;
            }
            if matches!(m.kind, Kind::Chicken | Kind::Rabbit) || m.kind.is_monster() {
                found = true;
                return;
            }
            if super::is_tamable(m.kind) {
                found = !m.tame;
                return;
            }
            found = !e.flags().contains(EntityFlags::CROUCHING);
        } else if let Some(pd) = ents.player(id) {
            if pd.survival.is_dead_or_dying()
                || matches!(
                    pd.game_mode,
                    vanillite_config::GameMode::Creative | vanillite_config::GameMode::Spectator
                )
            {
                return;
            }
            let uuid = e.uuid();
            if st(t.mob_ref()).trusted.contains(&Some(uuid)) {
                return;
            }
            found = !e.flags().contains(EntityFlags::CROUCHING)
                && e.pose() != vanillite_world::entity::Pose::Sleeping;
        }
    });
    found
}

/// `FoxBehaviorGoal.hasShelter`: no sky above and a walkable spot.
fn has_shelter(t: &Tick<'_, '_>) -> bool {
    let p = t.pos();
    let pos = BlockPos::containing(p.x, t.aabb().max_y, p.z);
    !crate::mob::can_see_sky(t.level, pos)
        && crate::mob::kinds::walk_target_value(Kind::Fox, t.level, &t.cx.env, pos) >= 0.0
}

/// `FleeSunGoal.getHidePos`: a covered spot within 10 blocks.
fn hide_pos(t: &mut Tick<'_, '_>) -> Option<Vec3> {
    let base = t.block_pos();
    for _ in 0..10 {
        let r = &mut t.mob().random;
        let pos = BlockPos::new(
            base.x + r.next_int_bounded(20) - 10,
            base.y + r.next_int_bounded(6) - 3,
            base.z + r.next_int_bounded(20) - 10,
        );
        if !crate::mob::can_see_sky(t.level, pos)
            && crate::mob::kinds::walk_target_value(Kind::Fox, t.level, &t.cx.env, pos) < 0.0
        {
            return Some(crate::mob::randompos::bottom_center(pos));
        }
    }
    None
}

/// `FoxEatBerriesGoal.isValidTarget`: a berry bush of age 2 or more, or glow berries.
fn berry_target(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    let state = t.level.block(pos);
    match state.block().name() {
        "minecraft:sweet_berry_bush" => state
            .value("age")
            .and_then(|a| a.parse::<i32>().ok())
            .is_some_and(|a| a >= 2),
        "minecraft:cave_vines" | "minecraft:cave_vines_plant" => {
            state.value("berries") == Some("true")
        }
        _ => false,
    }
}

/// The nearest item entity within 8 blocks without pickup delay.
fn nearest_item(t: &Tick<'_, '_>) -> Option<Vec3> {
    let area = t.aabb().inflate(8.0, 8.0, 8.0);
    let ents = &*t.ents;
    let mut first = None;
    ents.store.for_each_in(&area, false, |id| {
        if first.is_some() {
            return;
        }
        if let Some(d) = ents.item(id)
            && d.pickup_delay <= 0
            && !d.stack.is_empty()
        {
            first = ents.store.get(id).map(|e| e.pos());
        }
    });
    first
}

/// `FoxStrollThroughVillageGoal.canFoxMove`.
fn can_fox_move(m: &MobData) -> bool {
    !flag_of(m, SLEEPING) && !flag_of(m, SITTING) && !flag_of(m, DEFENDING) && m.target.is_none()
}

/// `StrollThroughVillageGoal.canUse`: at night, now and then, a land spot nearer to the village
/// centre of a village within 6 sections.
fn stroll_through_village(t: &mut Tick<'_, '_>, interval: i32) -> Option<BlockPos> {
    if crate::mob::has_controlling_passenger(t.ents, t.id) || t.cx.env.is_bright_outside() {
        return None;
    }
    if t.mob().random.next_int_bounded(interval) != 0 {
        return None;
    }
    if !t.ents.poi.is_close_to_village(t.block_pos(), 6) {
        return None;
    }
    let land = crate::mob::randompos::land_pos_weighted(t, 15, 7, |t, p| {
        -f64::from(t.ents.poi.sections_to_village(p.section()))
    })?;
    Some(BlockPos::containing(land.x, land.y, land.z))
}

/// `StrollThroughVillageGoal.tick`: towards the wanted spot in steps of 10 blocks, else
/// somewhere near (`moveRandomly`).
fn stroll_through_village_tick(t: &mut Tick<'_, '_>, wanted: BlockPos) {
    let pos = t.pos();
    let center = Vec3::new(
        f64::from(wanted.x) + 0.5,
        f64::from(wanted.y) + 0.5,
        f64::from(wanted.z) + 0.5,
    );
    if !t.nav_ref().is_done() || center.distance_to_sqr(pos) < 100.0 {
        return;
    }
    let target = crate::mob::randompos::bottom_center(wanted);
    let far = Vec3::new(
        (pos.x - target.x) * 0.4 + target.x,
        (pos.y - target.y) * 0.4 + target.y,
        (pos.z - target.z) * 0.4 + target.z,
    );
    let d = Vec3::new(far.x - pos.x, far.y - pos.y, far.z - pos.z);
    let len = d.length_sqr().sqrt();
    let n = if len < f64::from(1.0e-5_f32) {
        Vec3::ZERO
    } else {
        Vec3::new(d.x / len, d.y / len, d.z / len)
    };
    let to = Vec3::new(pos.x + n.x * 10.0, pos.y + n.y * 10.0, pos.z + n.z * 10.0);
    let to = BlockPos::containing(to.x, to.y, to.z);
    let to = surface(t, to.x, to.z);
    if !nav::move_to_pos(t, to_vec(to), 1.0) {
        let p = t.block_pos();
        let r = &mut t.mob().random;
        let dx = -8 + r.next_int_bounded(16);
        let dz = -8 + r.next_int_bounded(16);
        let to = surface(t, p.x + dx, p.z + dz);
        nav::move_to_pos(t, to_vec(to), 1.0);
    }
}

/// `getHeightmapPos(MOTION_BLOCKING_NO_LEAVES, (x, z))`.
fn surface(t: &Tick<'_, '_>, x: i32, z: i32) -> BlockPos {
    let y = crate::mob::spawn::surface_y(
        t.level,
        vanillite_chunk::HeightmapKind::MotionBlockingNoLeaves,
        x,
        z,
    );
    BlockPos::new(x, y, z)
}

/// `moveTo(x, y, z, speed)` of whole block coordinates.
fn to_vec(p: BlockPos) -> Vec3 {
    Vec3::new(f64::from(p.x), f64::from(p.y), f64::from(p.z))
}

pub fn can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::StrollThroughVillage { interval, wanted } => {
            *wanted = stroll_through_village(t, *interval);
            wanted.is_some() && can_fox_move(t.mob_ref())
        }
        Goal::Float => {
            let f = t.mob_ref().fluids;
            f.water_height > 0.25 || f.in_lava()
        }
        Goal::Faceplant { .. } => flag_of(t.mob_ref(), FACEPLANTED),
        Goal::Panic { pos } => {
            if flag_of(t.mob_ref(), DEFENDING) {
                return false;
            }
            let panics = t
                .mob_ref()
                .last_damage_source(t.game_time())
                .is_some_and(|k| {
                    t.env
                        .types
                        .is(k, crate::survival::damage::tag::PANIC_CAUSES)
                });
            if !panics {
                return false;
            }
            match crate::mob::randompos::default_pos(t, 5, 4) {
                Some(p) => {
                    *pos = p;
                    true
                }
                None => false,
            }
        }
        Goal::Avoid(cg) => crate::mob::common::can_use(t, cg),
        Goal::StalkPrey => {
            let m = t.mob_ref();
            if flag_of(m, SLEEPING) || flag_of(m, CROUCHING) || flag_of(m, INTERESTED) || m.jumping
            {
                return false;
            }
            let Some(target) = m.target else {
                return false;
            };
            let stalkable = t
                .ents
                .mob(target)
                .is_some_and(|tm| matches!(tm.kind, Kind::Chicken | Kind::Rabbit))
                || t.ents.player(target).is_some();
            let Some(o) = t.other(target) else {
                return false;
            };
            stalkable && o.pos.distance_to_sqr(t.pos()) > 36.0
        }
        Goal::Pounce => {
            let m = t.mob_ref();
            // `isFullyCrouched`.
            #[allow(clippy::float_cmp)]
            if st(m).crouch_amount != 5.0 {
                return false;
            }
            let Some(target) = m.target else {
                return false;
            };
            let Some(o) = t.other(target) else {
                return false;
            };
            let clear = path_clear(t, o.pos);
            if !clear {
                let _ = nav::create_path_to(t, BlockPos::containing(o.pos.x, o.pos.y, o.pos.z), 0);
                set_flag(t, CROUCHING, false);
                set_flag(t, INTERESTED, false);
            }
            clear
        }
        Goal::SeekShelter { interval, wanted } => {
            if flag_of(t.mob_ref(), SLEEPING) || t.mob_ref().target.is_some() {
                return false;
            }
            let pos = t.block_pos();
            let sky = crate::mob::can_see_sky(t.level, pos);
            if t.cx.env.thundering && sky {
                if let Some(p) = hide_pos(t) {
                    *wanted = p;
                    return true;
                }
                return false;
            }
            if *interval > 0 {
                *interval -= 1;
                return false;
            }
            *interval = 100;
            if t.cx.env.is_bright_outside()
                && sky
                && let Some(p) = hide_pos(t)
            {
                *wanted = p;
                return true;
            }
            false
        }
        Goal::Sleep { countdown } => {
            let m = t.mob_ref();
            if m.xxa != 0.0 || m.yya != 0.0 || m.zza != 0.0 {
                return false;
            }
            let sleeping = flag_of(m, SLEEPING);
            can_sleep(t, countdown) || sleeping
        }
        Goal::EatBerries { base, .. } => {
            !flag_of(t.mob_ref(), SLEEPING)
                && base.can_use(t, berry_target, MoveToBlock::default_next_start_tick)
        }
        Goal::SearchForItems => {
            let m = t.mob_ref();
            if !m.equipment.get(EquipmentSlot::MainHand).is_empty()
                || m.target.is_some()
                || m.last_hurt_by_mob.is_some()
                || !can_move(m)
            {
                return false;
            }
            if t.mob().random.next_int_bounded(reduced(10)) != 0 {
                return false;
            }
            nearest_item(t).is_some()
        }
        Goal::PerchAndSearch { .. } => {
            let m = t.mob_ref();
            m.last_hurt_by_mob.is_none()
                && t.mob().random.next_float() < 0.02
                && !flag_of(t.mob_ref(), SLEEPING)
                && t.mob_ref().target.is_none()
                && t.nav_ref().is_done()
                && !alertable(t)
                && !flag_of(t.mob_ref(), POUNCING)
                && !flag_of(t.mob_ref(), CROUCHING)
        }
        Goal::DefendTrusted {
            timestamp, found, ..
        } => {
            if t.mob().random.next_int_bounded(reduced(10)) != 0 {
                return false;
            }
            for u in st(t.mob_ref()).trusted.into_iter().flatten() {
                let Some(id) = t.ents.player_by_uuid(u) else {
                    continue;
                };
                let Some(pd) = t.ents.player(id) else {
                    continue;
                };
                let by = pd.survival.last_hurt_by_mob.map(|(id, _)| id);
                let stamp = pd.survival.last_hurt_by_mob.map_or(0, |(_, at)| at);
                *found = by;
                return stamp != *timestamp
                    && by.is_some_and(|b| crate::mob::common::test_combat(t, b, -1.0, false));
            }
            false
        }
    }
}

/// `SleepGoal.canSleep`.
fn can_sleep(t: &mut Tick<'_, '_>, countdown: &mut i32) -> bool {
    if *countdown > 0 {
        *countdown -= 1;
        return false;
    }
    t.cx.env.is_bright_outside() && has_shelter(t) && !alertable(t)
}

pub fn can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::StrollThroughVillage { wanted, .. } => {
            wanted.is_some()
                && !t.nav_ref().is_done()
                && t.nav_ref().target_pos() == *wanted
                && can_fox_move(t.mob_ref())
        }
        Goal::Faceplant { countdown } => flag_of(t.mob_ref(), FACEPLANTED) && *countdown > 0,
        Goal::Panic { .. } | Goal::SeekShelter { .. } => !t.nav_ref().is_done(),
        Goal::Avoid(cg) => crate::mob::common::can_continue(t, cg),
        Goal::Pounce => {
            let Some(target) = t.mob_ref().target else {
                return false;
            };
            if t.other(target).is_none() {
                return false;
            }
            let yd = t.velocity().y;
            let x_rot = t.core().rot().x_rot;
            (yd * yd >= f64::from(0.05_f32) || x_rot.abs() >= 15.0 || !t.on_ground())
                && !flag_of(t.mob_ref(), FACEPLANTED)
        }
        Goal::Sleep { countdown } => can_sleep(t, countdown),
        Goal::EatBerries { base, .. } => base.can_continue(t, berry_target),
        Goal::PerchAndSearch {
            looks_remaining, ..
        } => *looks_remaining > 0,
        Goal::DefendTrusted { state, .. } => crate::mob::goals::target_can_continue(t, state),
        _ => can_use(t, g),
    }
}

pub fn start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::StrollThroughVillage { .. } => clear_states(t),
        Goal::Float => clear_states(t),
        Goal::Faceplant { countdown } => *countdown = reduced(40),
        Goal::Panic { pos } => {
            nav::move_to_pos(t, *pos, 2.2);
        }
        Goal::Avoid(cg) => crate::mob::common::start(t, cg),
        Goal::StalkPrey => {
            set_flag(t, SITTING, false);
            set_flag(t, FACEPLANTED, false);
        }
        Goal::Pounce => {
            t.mob().jumping = true;
            set_flag(t, POUNCING, true);
            set_flag(t, INTERESTED, false);
            if let Some(o) = t.mob_ref().target.and_then(|id| t.other(id)) {
                t.mob()
                    .look
                    .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 60.0, 30.0);
                let p = t.pos();
                let uv = (o.pos - p).normalize();
                let v = t.velocity();
                t.set_velocity(Vec3::new(v.x + uv.x * 0.8, v.y + 0.9, v.z + uv.z * 0.8));
            }
            t.nav().stop();
        }
        Goal::SeekShelter { wanted, .. } => {
            clear_states(t);
            nav::move_to_pos(t, *wanted, 1.25);
        }
        Goal::Sleep { .. } => {
            set_flag(t, SITTING, false);
            set_flag(t, CROUCHING, false);
            set_flag(t, INTERESTED, false);
            t.mob().jumping = false;
            set_flag(t, SLEEPING, true);
            t.nav().stop();
            let p = t.pos();
            t.move_ctl().set_wanted_position(p, 0.0);
        }
        Goal::EatBerries { base, waited } => {
            *waited = 0;
            set_flag(t, SITTING, false);
            base.start(t);
        }
        Goal::SearchForItems => {
            if let Some(p) = nearest_item(t) {
                nav::move_to_pos(t, p, f64::from(1.2_f32));
            }
        }
        Goal::PerchAndSearch {
            rel_x,
            rel_z,
            look_time,
            looks_remaining,
        } => {
            reset_look(t, rel_x, rel_z, look_time);
            *looks_remaining = 2 + t.mob().random.next_int_bounded(3);
            set_flag(t, SITTING, true);
            t.nav().stop();
        }
        Goal::DefendTrusted {
            state,
            timestamp,
            found,
        } => {
            crate::mob::goals::set_target(t, *found);
            for u in st(t.mob_ref()).trusted.into_iter().flatten() {
                if let Some(pd) = t.ents.player_by_uuid(u).and_then(|id| t.ents.player(id)) {
                    *timestamp = pd.survival.last_hurt_by_mob.map_or(0, |(_, at)| at);
                    break;
                }
            }
            crate::mob::kinds::play_sound(t, "minecraft:entity.fox.aggro", 1.0, 1.0);
            set_flag(t, DEFENDING, true);
            set_flag(t, SLEEPING, false);
            state.unseen_ticks = 0;
        }
    }
}

fn reset_look(t: &mut Tick<'_, '_>, rel_x: &mut f64, rel_z: &mut f64, look_time: &mut i32) {
    let r = &mut t.mob().random;
    let a = std::f64::consts::TAU * r.next_double();
    *rel_x = a.cos();
    *rel_z = a.sin();
    *look_time = reduced(80 + r.next_int_bounded(20));
}

pub fn stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::Faceplant { .. } => set_flag(t, FACEPLANTED, false),
        Goal::Avoid(cg) => crate::mob::common::stop(t, cg),
        Goal::StalkPrey => {
            let target = t.mob_ref().target.and_then(|id| t.other(id));
            if let Some(o) = target
                && path_clear(t, o.pos)
            {
                set_flag(t, INTERESTED, true);
                set_flag(t, CROUCHING, true);
                t.nav().stop();
                // `getMaxHeadYRot`, `getMaxHeadXRot`.
                t.mob()
                    .look
                    .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 75.0, 40.0);
            } else {
                set_flag(t, INTERESTED, false);
                set_flag(t, CROUCHING, false);
            }
        }
        Goal::Pounce => {
            set_flag(t, CROUCHING, false);
            st_mut(t.mob()).crouch_amount = 0.0;
            set_flag(t, INTERESTED, false);
            set_flag(t, POUNCING, false);
        }
        Goal::Sleep { countdown } => {
            *countdown = t.mob().random.next_int_bounded(reduced(140));
            clear_states(t);
        }
        Goal::PerchAndSearch { .. } => set_flag(t, SITTING, false),
        Goal::DefendTrusted { state, .. } => {
            crate::mob::goals::set_target(t, None);
            set_flag(t, DEFENDING, false);
            state.target_mob = None;
        }
        Goal::Panic { .. } | Goal::SeekShelter { .. } | Goal::SearchForItems => {
            t.nav().stop();
        }
        _ => {}
    }
}

pub fn tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::StrollThroughVillage {
            wanted: Some(w), ..
        } => {
            let w = *w;
            stroll_through_village_tick(t, w);
        }
        Goal::Float => {
            if t.mob().random.next_float() < 0.8 {
                t.mob().jumping = true;
            }
        }
        Goal::Faceplant { countdown } => *countdown -= 1,
        Goal::Avoid(cg) => crate::mob::common::tick(t, cg),
        Goal::StalkPrey => {
            let Some(o) = t.mob_ref().target.and_then(|id| t.other(id)) else {
                return;
            };
            // `getMaxHeadYRot`, `getMaxHeadXRot`.
            t.mob()
                .look
                .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 75.0, 40.0);
            if o.pos.distance_to_sqr(t.pos()) <= 36.0 {
                set_flag(t, INTERESTED, true);
                set_flag(t, CROUCHING, true);
                t.nav().stop();
            } else {
                nav::move_to_entity(t, o.pos, 1, 1.5);
            }
        }
        Goal::Pounce => {
            let target = t.mob_ref().target;
            let o = target.and_then(|id| t.other(id));
            if let Some(o) = o {
                t.mob()
                    .look
                    .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 60.0, 30.0);
            }
            if !flag_of(t.mob_ref(), FACEPLANTED) {
                pounce_pitch(t);
            }
            #[allow(clippy::cast_possible_truncation)]
            if let (Some(id), Some(o)) = (target, o)
                && (o.pos.distance_to_sqr(t.pos()).sqrt() as f32) <= 2.0
            {
                crate::mob::combat::base_mob_attack(t, id);
            } else if t.core().rot().x_rot > 0.0
                && t.on_ground()
                && t.velocity().y != 0.0
                && t.level.block(t.block_pos()).block().name() == "minecraft:snow"
            {
                let mut rot = t.core().rot();
                rot.x_rot = 60.0;
                t.core_mut().set_rot(rot);
                t.mob().target = None;
                set_flag(t, FACEPLANTED, true);
            }
        }
        Goal::EatBerries { base, waited } => {
            if base.reached_target {
                if *waited >= 40 {
                    eat_berries(t, base.block_pos);
                } else {
                    *waited += 1;
                }
            } else if t.mob().random.next_float() < 0.05 {
                crate::mob::kinds::play_sound(t, "minecraft:entity.fox.sniff", 1.0, 1.0);
            }
            let target = BlockPos::new(base.block_pos.x, base.block_pos.y + 1, base.block_pos.z);
            base.tick(t, target, 2.0);
        }
        Goal::SearchForItems => {
            if t.mob_ref()
                .equipment
                .get(EquipmentSlot::MainHand)
                .is_empty()
                && let Some(p) = nearest_item(t)
            {
                nav::move_to_pos(t, p, f64::from(1.2_f32));
            }
        }
        Goal::PerchAndSearch {
            rel_x,
            rel_z,
            look_time,
            looks_remaining,
        } => {
            *look_time -= 1;
            if *look_time <= 0 {
                *looks_remaining -= 1;
                reset_look(t, rel_x, rel_z, look_time);
            }
            let p = t.pos();
            let at = Vec3::new(p.x + *rel_x, t.eye_y(), p.z + *rel_z);
            // `getMaxHeadYRot`, `getMaxHeadXRot`.
            t.mob().look.set_look_at(at, 75.0, 40.0);
        }
        _ => {}
    }
}

/// `FoxEatBerriesGoal.onReachedTarget`: picks the berries (mob griefing).
fn eat_berries(t: &mut Tick<'_, '_>, pos: BlockPos) {
    if !t.env.rules.bool(Rule::MOB_GRIEFING) {
        return;
    }
    let state = t.level.block(pos);
    match state.block().name() {
        "minecraft:sweet_berry_bush" => {
            let age: i32 = state.value("age").and_then(|a| a.parse().ok()).unwrap_or(0);
            let mut count = 1 + t.mob().random.next_int_bounded(2) + i32::from(age == 3);
            if t.mob_ref()
                .equipment
                .get(EquipmentSlot::MainHand)
                .is_empty()
            {
                t.mob().equipment.set(
                    EquipmentSlot::MainHand,
                    equipment::stack("minecraft:sweet_berries"),
                );
                crate::mob::equipment::broadcast(t);
                count -= 1;
            }
            if count > 0 {
                let berries = equipment::stack("minecraft:sweet_berries").with_count(count);
                crate::mob::equipment::spawn_at_location(t, berries, 0.0);
            }
            crate::mob::kinds::play_sound(
                t,
                "minecraft:block.sweet_berry_bush.pick_berries",
                1.0,
                1.0,
            );
            if let Some(s) = state.with("age", "1") {
                t.cx.actions
                    .push(crate::mob::WorldAction::SetBlock(pos, s, 2));
            }
        }
        "minecraft:cave_vines" | "minecraft:cave_vines_plant" => {
            // `CaveVines.use`: the glow berries pop out.
            let berries = equipment::stack("minecraft:glow_berries");
            crate::mob::equipment::spawn_at_location(t, berries, 0.0);
            crate::mob::kinds::play_sound(t, "minecraft:block.cave_vines.pick_berries", 1.0, 1.0);
            if let Some(s) = state.with("berries", "false") {
                t.cx.actions
                    .push(crate::mob::WorldAction::SetBlock(pos, s, 2));
            }
        }
        _ => {}
    }
}
