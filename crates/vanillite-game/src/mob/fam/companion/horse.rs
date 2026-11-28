//! `AbstractHorse`, `AbstractChestedHorse`, `Horse`, `Donkey`, `Mule`, `SkeletonHorse` (with
//! `SkeletonTrapGoal`) and `ZombieHorse`; the shared mount parts (`handleEating`, saddles,
//! body armour, chests, taming by riding) are used by the llamas too.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_nbt::{Compound, List, Tag};
use vanillite_world::entity::{EntityId, Rotation};

use super::{
    Cond, G, Goal as FamGoal, Interact, SpawnContext, State, TemptItems, consume, dirty, fam, int,
    item_name, sound, st, st_mut,
};
use crate::item::{EquipmentSlot, ItemStack};
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::equipment;
use crate::mob::fam::{ExtraMob, GroupData, Hurt, Relation, SpawnReason};
use crate::mob::goals::flag;
use crate::mob::kinds::Kind;
use crate::mob::{MobData, Tick, nav, randompos};
use crate::player::encode::DataWriter;
use crate::survival::Effect;

const TAMED: u8 = 2;
const BRED: u8 = 8;
const EATING: u8 = 16;
const STANDING: u8 = 32;
const MOUTH: u8 = 64;

/// `AbstractHorse.generateSpeed` with a constant random (the offspring ranges).
fn gen_speed(r: f64) -> f64 {
    (f64::from(0.45_f32) + r * 0.3 + r * 0.3 + r * 0.3) * 0.25
}

/// `generateJumpStrength` with a constant random.
fn gen_jump(r: f64) -> f64 {
    f64::from(0.4_f32) + r * 0.2 + r * 0.2 + r * 0.2
}

/// The horse goals.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// `RunAroundLikeCrazyGoal`: an untamed horse with a rider bucks until tamed.
    RunAround { speed: f64, pos: Vec3 },
    /// `RandomStandGoal`.
    RandomStand { next_stand: i32 },
    /// `SkeletonTrapGoal`.
    SkeletonTrap,
}

pub fn flags(g: &Goal) -> u8 {
    match g {
        Goal::RunAround { .. } => flag::MOVE,
        Goal::RandomStand { .. } | Goal::SkeletonTrap => 0,
    }
}

pub fn every_tick(g: &Goal) -> bool {
    matches!(g, Goal::RandomStand { .. })
}

/// Whether the kind is a chested horse (`AbstractChestedHorse`).
pub fn is_chested(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Donkey | Kind::Mule | Kind::Llama | Kind::TraderLlama
    )
}

/// `canPerformRearing`.
fn can_rear(kind: Kind) -> bool {
    !matches!(kind, Kind::Llama | Kind::TraderLlama)
}

/// `getMaxTemper`.
pub fn max_temper(kind: Kind) -> i32 {
    if matches!(kind, Kind::Llama | Kind::TraderLlama) {
        30
    } else {
        100
    }
}

/// `isSaddled`.
pub fn is_saddled(m: &MobData) -> bool {
    !m.equipment.get(EquipmentSlot::Saddle).is_empty()
}

/// `isMobControlled` (a zombie horse with a mob rider): no player taming.
fn mob_controlled(t: &Tick<'_, '_>) -> bool {
    t.mob_ref().kind == Kind::ZombieHorse
        && t.ents
            .riding
            .passengers_of(t.id)
            .first()
            .is_some_and(|&p| t.ents.mob(p).is_some())
}

fn set_flag(m: &mut MobData, bit: u8, on: bool) {
    let s = st_mut(m);
    if on {
        s.horse_flags |= bit;
    } else {
        s.horse_flags &= !bit;
    }
}

/// `AbstractHorse.onElasticLeashPull`: a pulled horse stops eating (`setEating(false)`).
pub fn stop_eating(t: &mut Tick<'_, '_>) {
    if has_flag(t.mob_ref(), EATING) {
        set_flag(t.mob(), EATING, false);
        t.core_mut().mark_data_dirty();
    }
}

fn has_flag(m: &MobData, bit: u8) -> bool {
    st(m).horse_flags & bit != 0
}

/// `setStanding(ticks)`.
fn set_standing(m: &mut MobData, ticks: i32) {
    set_flag(m, EATING, false);
    set_flag(m, STANDING, true);
    st_mut(m).stand_counter = ticks;
}

/// `standIfPossible`.
fn stand_if_possible(m: &mut MobData) {
    if can_rear(m.kind) {
        set_standing(m, 20);
    }
}

/// `registerGoals` and `addBehaviourGoals` of the horses (not the llamas).
pub fn register_goals(m: &mut MobData) {
    let kind = m.kind;
    let g = &mut m.goals;
    g.add(
        1,
        fam(FamGoal::Horse(Goal::RunAround {
            speed: 1.2,
            pos: Vec3::ZERO,
        })),
    );
    g.add(2, G::breed(1.0));
    g.add(
        4,
        G::FollowParent {
            speed: 1.0,
            parent: None,
            recalc: 0,
        },
    );
    g.add(6, G::stroll(0.7));
    g.add(7, G::look_at_player(6.0));
    g.add(8, G::random_look());
    g.add(
        9,
        fam(FamGoal::Horse(Goal::RandomStand { next_stand: -400 })),
    );
    match kind {
        Kind::SkeletonHorse => {
            g.add(1, fam(FamGoal::Horse(Goal::SkeletonTrap)));
        }
        Kind::ZombieHorse => {
            g.add(0, G::Float);
            g.add(
                3,
                G::tempt(1.25, TemptItems::Tag("minecraft:zombie_horse_food")),
            );
        }
        _ => {
            g.add(0, G::Float);
            g.add(1, G::panic(1.2));
            g.add(
                3,
                G::tempt(1.25, TemptItems::Tag("minecraft:horse_tempt_items")),
            );
        }
    }
    m.nav.can_float = true;
    let _ = Cond::NotTame;
}

/// `AbstractHorse.generateMaxHealth`.
fn gen_health(r: &mut LegacyRandom) -> f64 {
    15.0 + f64::from(r.next_int_bounded(8)) + f64::from(r.next_int_bounded(9))
}

/// `generateSpeed` with a random.
fn rand_speed(r: &mut LegacyRandom) -> f64 {
    let (a, b, c) = (r.next_double(), r.next_double(), r.next_double());
    (f64::from(0.45_f32) + a * 0.3 + b * 0.3 + c * 0.3) * 0.25
}

/// `generateJumpStrength` with a random.
fn rand_jump(r: &mut LegacyRandom) -> f64 {
    let (a, b, c) = (r.next_double(), r.next_double(), r.next_double());
    f64::from(0.4_f32) + a * 0.2 + b * 0.2 + c * 0.2
}

/// Sets the max health base; the health keeps its value until the next tick caps it
/// (`refreshDirtyAttributes`).
fn set_max_health(m: &mut MobData, v: f64) {
    m.attrs.max_health = v;
}

/// `randomizeAttributes` of the kind.
pub fn randomize_attributes(m: &mut MobData) {
    match m.kind {
        Kind::Horse => {
            let h = gen_health(&mut m.random);
            set_max_health(m, h);
            m.attrs.movement_speed = rand_speed(&mut m.random);
            m.attrs.jump_strength = rand_jump(&mut m.random);
        }
        Kind::Donkey | Kind::Mule | Kind::Llama | Kind::TraderLlama => {
            let h = gen_health(&mut m.random);
            set_max_health(m, h);
        }
        Kind::SkeletonHorse => m.attrs.jump_strength = rand_jump(&mut m.random),
        Kind::ZombieHorse => {
            let r = &mut m.random;
            let (a, b, c) = (r.next_double(), r.next_double(), r.next_double());
            m.attrs.jump_strength = 0.5
                + a * 0.066_666_666_666_666_67
                + b * 0.066_666_666_666_666_67
                + c * 0.066_666_666_666_666_67;
            let (a, b, c) = (r.next_double(), r.next_double(), r.next_double());
            m.attrs.movement_speed = (9.0 + a + b + c) / f64::from(42.16_f32);
        }
        _ => {}
    }
}

/// `finalizeSpawn` of the horses.
pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    let chance = match m.kind {
        Kind::Horse => {
            // `HorseGroupData(variant)`: the group's coat, own markings.
            let variant = if let GroupData::Horse(v) = ctx.group {
                v
            } else {
                let v = m.random.next_int_bounded(7);
                ctx.group = GroupData::Horse(v);
                v
            };
            let markings = m.random.next_int_bounded(5);
            m.variant = (variant & 0xFF) | ((markings << 8) & 0xFF00);
            0.05
        }
        Kind::ZombieHorse => {
            if ctx.reason == SpawnReason::Natural {
                // A zombie with an iron spear rides it.
                let seed = crate::mob::new_entity_seed();
                let mut zombie = MobData::new(Kind::Zombie, seed);
                let mut inner = SpawnContext {
                    reason: SpawnReason::Natural,
                    extra: Vec::new(),
                    ride: None,
                    ..*ctx
                };
                crate::mob::kinds::finalize_spawn(&mut zombie, &mut inner, &mut m.random);
                zombie.equipment.set(
                    EquipmentSlot::MainHand,
                    equipment::stack("minecraft:iron_spear"),
                );
                ctx.extra.push(ExtraMob {
                    mob: zombie,
                    offset: Vec3::ZERO,
                    relation: Relation::RidesIt,
                });
            }
            0.2
        }
        _ => 0.2,
    };
    randomize_attributes(m);
    super::ageable_group(m, ctx, true, chance);
}

/// `AbstractHorse.tick`: `standAnim` follows `isStanding` (after the stand counter of this
/// tick, which [`before_tick`] only applies at the start of the next one).
pub fn after_tick(m: &mut MobData) {
    let s = st_mut(m);
    let standing = s.horse_flags & STANDING != 0 && s.stand_counter != 1;
    s.stand_anim_o = s.stand_anim;
    let a = s.stand_anim;
    s.stand_anim = if standing {
        (a + ((1.0 - a) * 0.4 + 0.05)).min(1.0)
    } else {
        (a + ((0.8 * a * a * a - a) * 0.6 - 0.05)).max(0.0)
    };
}

/// `AbstractHorse.getPassengerAttachmentPoint`: the rearing offset (`standAnimO`) turned by
/// the horse's yaw, added to the passenger attachment.
pub fn rearing_offset(m: &MobData, y_rot: f32, scale: f32) -> Vec3 {
    let o = st(m).stand_anim_o;
    if o == 0.0 {
        return Vec3::ZERO;
    }
    let angle = -y_rot * (std::f32::consts::PI / 180.0);
    let (cos, sin) = (
        vanillite_base::math::cos(f64::from(angle)),
        vanillite_base::math::sin(f64::from(angle)),
    );
    let y = 0.15 * f64::from(o * scale);
    let z = -0.7 * f64::from(o * scale);
    Vec3::new(z * f64::from(sin), y, z * f64::from(cos))
}

/// `AbstractHorse.tick` counters (after `LivingEntity.tick`; here before the next one).
pub fn before_tick(t: &mut Tick<'_, '_>) {
    let rot = t.core().rot();
    let m = t.mob();
    st_mut(m).view_rot_o = [rot.y_head_rot, rot.x_rot];
    let mut changed = false;
    {
        let s = st_mut(m);
        if s.mouth_counter > 0 {
            s.mouth_counter += 1;
            if s.mouth_counter > 30 {
                s.mouth_counter = 0;
                s.horse_flags &= !MOUTH;
                changed = true;
            }
        }
        if s.stand_counter > 0 {
            s.stand_counter -= 1;
            if s.stand_counter <= 0 {
                s.horse_flags &= !STANDING;
                s.stand_counter = 0;
                changed = true;
            }
        }
        if s.sprint_counter > 0 {
            s.sprint_counter += 1;
            if s.sprint_counter > 300 {
                s.sprint_counter = 0;
            }
        }
    }
    if changed {
        t.core_mut().mark_data_dirty();
    }
}

/// `AbstractHorse.isImmobile`: eating or rearing (llamas: eating only); the dead are
/// immobile anyway.
pub fn is_immobile(m: &MobData) -> bool {
    let llama = matches!(m.kind, Kind::Llama | Kind::TraderLlama);
    has_flag(m, EATING) || !llama && has_flag(m, STANDING)
}

/// `AbstractHorse.aiStep` before `super.aiStep()`: the `moveTail` roll.
pub fn ai_step_start(t: &mut Tick<'_, '_>) {
    let _ = t.mob().random.next_int_bounded(200);
}

/// `AbstractHorse.aiStep` (healing, grazing, following the mother) and the kind additions
/// (skeleton trap lifetime, trader llama despawning).
pub fn ai_step(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().is_alive() {
        return;
    }
    if t.mob().random.next_int_bounded(900) == 0 && t.mob_ref().death_time == 0 {
        super::heal(t.mob(), 1.0);
        t.core_mut().mark_data_dirty();
    }
    let kind = t.mob_ref().kind;
    if !matches!(kind, Kind::Llama | Kind::TraderLlama) {
        let vehicle = super::is_vehicle(t);
        let eating = has_flag(t.mob_ref(), EATING);
        if !eating && !vehicle && t.mob().random.next_int_bounded(300) == 0 {
            let p = t.block_pos();
            let below = t.level.block(BlockPos::new(p.x, p.y - 1, p.z));
            if below.block() == vanillite_data::block::ids::GRASS_BLOCK {
                set_flag(t.mob(), EATING, true);
                t.core_mut().mark_data_dirty();
            }
        }
        if has_flag(t.mob_ref(), EATING) {
            let s = st_mut(t.mob());
            s.eating_counter += 1;
            if s.eating_counter > 50 {
                s.eating_counter = 0;
                s.horse_flags &= !EATING;
                t.core_mut().mark_data_dirty();
            }
        }
    }
    follow_mommy(t);
    match kind {
        Kind::SkeletonHorse => {
            let m = t.mob();
            if !m.persistent && st(m).trap {
                let s = st_mut(m);
                let time = s.trap_time;
                s.trap_time += 1;
                if time >= 18_000 {
                    m.removed = true;
                }
            }
        }
        Kind::TraderLlama => super::llama::trader_despawn(t),
        _ => {}
    }
}

/// `followMommy`: a bred foal walks to the nearest adult horse within 16 blocks.
fn follow_mommy(t: &mut Tick<'_, '_>) {
    let m = t.mob_ref();
    let llama = matches!(m.kind, Kind::Llama | Kind::TraderLlama);
    if llama && st(m).caravan_head.is_some() {
        return;
    }
    // `Llama.followMommy` adds only the caravan and baby checks.
    if !(has_flag(m, BRED) && m.baby && !has_flag(m, EATING)) {
        return;
    }
    let area = t.aabb().inflate(16.0, 16.0, 16.0);
    let pos = t.pos();
    let mut best: Option<(EntityId, f64)> = None;
    let me = t.id;
    let ents = &*t.ents;
    ents.store.for_each_in(&area, false, |id| {
        if id == me {
            return;
        }
        let Some(o) = ents.mob(id) else {
            return;
        };
        // `MOMMY_TARGETING`: a bred horse within 16 blocks.
        if !super::is_horse(o.kind) || !o.is_alive() || !has_flag(o, BRED) {
            return;
        }
        let Some(e) = ents.store.get(id) else {
            return;
        };
        let d = e.pos().distance_to_sqr(pos);
        if d > 256.0 {
            return;
        }
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((id, d));
        }
    });
    if let Some((id, d)) = best
        && d > 4.0
        && let Some(target) = t.other(id).map(|o| o.pos)
    {
        // `navigation.createPath(mommy, 0)` without moving: only the path search runs.
        let _ = nav::create_path_to(t, BlockPos::containing(target.x, target.y, target.z), 0);
    }
}

/// `causeFallDamage`: the landing sound.
pub fn on_fall_damage(t: &mut Tick<'_, '_>, distance: f64) {
    if distance > 1.0 && super::is_horse(t.mob_ref().kind) {
        let s = if t.mob_ref().baby {
            "minecraft:entity.horse.land_baby"
        } else {
            "minecraft:entity.horse.land"
        };
        if !matches!(t.mob_ref().kind, Kind::Llama | Kind::TraderLlama) {
            crate::mob::kinds::play_sound(t, s, 0.4, 1.0);
        }
    }
}

/// `hurtServer`: one in three hits makes it rear.
pub fn after_hurt(h: &mut Hurt<'_, '_>, _amount: f32) {
    let Some(m) = h.ents.mob_mut(h.target) else {
        return;
    };
    if m.random.next_int_bounded(3) == 0 {
        stand_if_possible(m);
        if let Some(mut e) = h.ents.store.get_mut(h.target) {
            e.mark_data_dirty();
        }
    }
}

/// `dropEquipment`: the inventory and the chest.
pub fn on_die(h: &mut Hurt<'_, '_>) {
    let Some(pos) = h.ents.store.get(h.target).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = h.ents.mob_mut(h.target) else {
        return;
    };
    let mut drops: Vec<ItemStack> = std::mem::take(&mut st_mut(m).inventory)
        .into_iter()
        .filter(|s| !s.is_empty() && !equipment::prevents_drop(s))
        .collect();
    if st(m).chest {
        st_mut(m).chest = false;
        drops.push(equipment::stack("minecraft:chest"));
    }
    for stack in drops {
        drop_item(h.env, pos, stack);
    }
}

/// An item entity at `pos` with the default random motion (`spawnAtLocation`).
pub fn drop_item(env: &mut crate::survival::player::Env<'_>, pos: Vec3, stack: ItemStack) {
    let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
    env.out.push(Effect::DropItem {
        random: item_random,
        uuid,
        pos,
        velocity,
        y_rot,
        pickup_delay: 10,
        stack,
    });
}

// --- Sounds -------------------------------------------------------------------------------

pub fn ambient_sound(m: &MobData) -> Option<&'static str> {
    Some(match m.kind {
        Kind::Horse if m.baby => "minecraft:entity.horse.ambient_baby",
        Kind::Horse => "minecraft:entity.horse.ambient",
        Kind::Donkey => "minecraft:entity.donkey.ambient",
        Kind::Mule => "minecraft:entity.mule.ambient",
        Kind::SkeletonHorse if m.fluids.eye_in_water => {
            "minecraft:entity.skeleton_horse.ambient_water"
        }
        Kind::SkeletonHorse => "minecraft:entity.skeleton_horse.ambient",
        Kind::ZombieHorse => "minecraft:entity.zombie_horse.ambient",
        _ => "minecraft:entity.llama.ambient",
    })
}

pub fn hurt_sound(m: &MobData) -> &'static str {
    match m.kind {
        Kind::Horse if m.baby => "minecraft:entity.horse.hurt_baby",
        Kind::Horse => "minecraft:entity.horse.hurt",
        Kind::Donkey => "minecraft:entity.donkey.hurt",
        Kind::Mule => "minecraft:entity.mule.hurt",
        Kind::SkeletonHorse => "minecraft:entity.skeleton_horse.hurt",
        Kind::ZombieHorse => "minecraft:entity.zombie_horse.hurt",
        _ => "minecraft:entity.llama.hurt",
    }
}

pub fn death_sound(m: &MobData) -> &'static str {
    match m.kind {
        Kind::Horse if m.baby => "minecraft:entity.horse.death_baby",
        Kind::Horse => "minecraft:entity.horse.death",
        Kind::Donkey => "minecraft:entity.donkey.death",
        Kind::Mule => "minecraft:entity.mule.death",
        Kind::SkeletonHorse => "minecraft:entity.skeleton_horse.death",
        Kind::ZombieHorse => "minecraft:entity.zombie_horse.death",
        _ => "minecraft:entity.llama.death",
    }
}

fn angry_sound(m: &MobData) -> Option<&'static str> {
    Some(match m.kind {
        Kind::Horse if m.baby => "minecraft:entity.horse.angry_baby",
        Kind::Horse => "minecraft:entity.horse.angry",
        Kind::Donkey => "minecraft:entity.donkey.angry",
        Kind::Mule => "minecraft:entity.mule.angry",
        Kind::ZombieHorse => "minecraft:entity.zombie_horse.angry",
        Kind::Llama | Kind::TraderLlama => "minecraft:entity.llama.angry",
        _ => return None,
    })
}

fn eating_sound(m: &MobData) -> Option<&'static str> {
    Some(match m.kind {
        Kind::Horse if m.baby => "minecraft:entity.horse.eat_baby",
        Kind::Horse => "minecraft:entity.horse.eat",
        Kind::Donkey => "minecraft:entity.donkey.eat",
        Kind::Mule => "minecraft:entity.mule.eat",
        Kind::ZombieHorse => "minecraft:entity.zombie_horse.eat",
        Kind::Llama | Kind::TraderLlama => "minecraft:entity.llama.eat",
        _ => return None,
    })
}

// --- Interaction --------------------------------------------------------------------------

/// `makeMad`: rears and plays the angry sound.
fn make_mad(ix: &mut Interact<'_, '_>) {
    let standing = ix
        .ents
        .mob(ix.target)
        .is_some_and(|m| has_flag(m, STANDING));
    if standing {
        return;
    }
    let sound_name = ix.ents.mob_mut(ix.target).and_then(|m| {
        stand_if_possible(m);
        angry_sound(m)
    });
    dirty(ix);
    if let Some(s) = sound_name {
        crate::mob::combat::mob_sound(ix.ents, ix.env, ix.target, s);
    }
}

/// `handleEating` of the horses and the llamas; returns whether the food was used.
fn handle_eating(ix: &mut Interact<'_, '_>) -> bool {
    let name = item_name(ix.stack);
    let Some(m) = ix.ents.mob(ix.target) else {
        return false;
    };
    let llama = matches!(m.kind, Kind::Llama | Kind::TraderLlama);
    let love_ok = m.tame && m.age == 0 && m.in_love <= 0;
    let (heal, age_up, temper, love) = if llama {
        match name {
            "minecraft:wheat" => (2.0, 10, 3, false),
            "minecraft:hay_block" => (10.0, 90, 6, love_ok),
            _ => (0.0, 0, 0, false),
        }
    } else {
        match name {
            "minecraft:wheat" => (2.0, 20, 3, false),
            "minecraft:sugar" => (1.0, 30, 3, false),
            "minecraft:hay_block" => (20.0, 180, 0, false),
            "minecraft:apple" | "minecraft:carrot" => (3.0, 60, 3, false),
            "minecraft:red_mushroom" => (3.0, 0, 3, false),
            "minecraft:golden_carrot" => (4.0, 60, 5, love_ok),
            "minecraft:golden_apple" | "minecraft:enchanted_golden_apple" => {
                (10.0, 240, 10, love_ok)
            }
            _ => (0.0, 0, 0, false),
        }
    };
    let player_uuid = super::player_uuid(ix);
    let Some(m) = ix.ents.mob_mut(ix.target) else {
        return false;
    };
    let mut used = false;
    if love {
        used = true;
        m.in_love = 600;
        m.love_cause = player_uuid;
    }
    if m.health < m.max_health() && heal > 0.0 {
        super::heal(m, heal);
        used = true;
    }
    if m.baby && age_up > 0 && !m.age_locked {
        crate::mob::animal::age_up(m, age_up, false);
        used = true;
    }
    let max = max_temper(m.kind);
    if temper > 0 && (used || !m.tame) && st(m).temper < max {
        let s = st_mut(m);
        s.temper = (s.temper + temper).clamp(0, max);
        used = true;
    }
    if used {
        if love {
            crate::mob::animal::entity_event(ix.ents, ix.env, ix.target, 18);
        }
        let eat = ix.ents.mob_mut(ix.target).and_then(|m| {
            if !llama {
                // `eating`: the mouth opens.
                st_mut(m).mouth_counter = 1;
                set_flag(m, MOUTH, true);
            }
            eating_sound(m)
        });
        if let Some(s) = eat {
            let pitch = ix.ents.mob_mut(ix.target).map_or(1.0, |m| {
                1.0 + (m.random.next_float() - m.random.next_float()) * 0.2
            });
            sound(ix, s, 1.0, pitch);
        }
        // `AbstractHorse.handleEating`'s EAT game event; `Llama.handleEating` sends none.
        if !llama {
            let event = crate::level::game_event::events().eat;
            crate::mob::animal::entity_game_event(
                ix.ents,
                ix.env,
                ix.target,
                event,
                Some(ix.target),
            );
        }
        dirty(ix);
    }
    used
}

/// `fedFood`.
fn fed_food(ix: &mut Interact<'_, '_>) -> Interaction {
    if handle_eating(ix) {
        consume(ix);
        Interaction::Success
    } else {
        Interaction::Pass
    }
}

/// The body slot item a mount accepts (`isEquippableInSlot(stack, BODY)`).
pub fn accepts_body(kind: Kind, stack: &ItemStack) -> bool {
    let name = item_name(stack);
    match kind {
        Kind::Horse | Kind::ZombieHorse => name.ends_with("_horse_armor"),
        // The dyed carpets (`#wool_carpets`, their equippable); moss and pale moss carpets
        // are no llama decor.
        Kind::Llama | Kind::TraderLlama => {
            name.ends_with("_carpet") && !name.ends_with("moss_carpet")
        }
        _ => false,
    }
}

/// `Equippable.equipOnInteract` of a saddle: a tame adult without a saddle takes it.
fn try_saddle(ix: &mut Interact<'_, '_>) -> bool {
    if item_name(ix.stack) != "minecraft:saddle" {
        return false;
    }
    let Some(m) = ix.ents.mob(ix.target) else {
        return false;
    };
    let accepts = matches!(
        m.kind,
        Kind::Horse | Kind::Donkey | Kind::Mule | Kind::SkeletonHorse | Kind::ZombieHorse
    );
    if !accepts || !m.is_alive() || m.baby || !m.tame || is_saddled(m) {
        return false;
    }
    let one = ix.stack.with_count(1);
    consume(ix);
    if let Some(m) = ix.ents.mob_mut(ix.target) {
        m.equipment.set(EquipmentSlot::Saddle, one);
        m.equipment.set_guaranteed_drop(EquipmentSlot::Saddle);
    }
    send_equipment(ix);
    // `LivingEntity.onEquipItem`: the equip sound takes its seed from the mob's random.
    let pos = ix.ents.store.get(ix.target).map(|e| e.pos());
    if let (Some(pos), Some(m)) = (pos, ix.ents.mob_mut(ix.target))
        && !m.silent
    {
        let seed = m.random.next_long();
        let source = m.sound_source();
        crate::survival::play_sound(
            ix.env.out,
            None,
            pos,
            "minecraft:entity.horse.saddle",
            source,
            1.0,
            1.0,
            seed,
        );
    }
    let event = crate::level::game_event::events().equip;
    let target = ix.target;
    crate::mob::animal::entity_game_event(ix.ents, ix.env, target, event, Some(target));
    true
}

/// `set_equipment` of the target after an interaction.
pub fn send_equipment(ix: &mut Interact<'_, '_>) {
    let Some(e) = ix.ents.store.get(ix.target) else {
        return;
    };
    let entity_id = e.network_id().0;
    let Some(m) = ix.ents.mob(ix.target) else {
        return;
    };
    let slots: Vec<(u8, ItemStack)> = equipment::SLOTS
        .iter()
        .map(|&s| (s.ordinal(), m.equipment.get(s).clone()))
        .collect();
    let target = ix.target;
    ix.env.out.push(Effect::Tracking {
        entity: target,
        with_self: false,
        packet: vanillite_protocol::SharedPacket::of(
            &vanillite_protocol::packets::play::clientbound::SetEquipment { entity_id, slots },
        ),
    });
}

/// `doPlayerRide`.
fn ride(ix: &mut Interact<'_, '_>) -> Interaction {
    if let Some(m) = ix.ents.mob_mut(ix.target) {
        set_flag(m, EATING, false);
        set_flag(m, STANDING, false);
        st_mut(m).stand_counter = 0;
    }
    dirty(ix);
    let rot = ix.ents.store.get(ix.target).map(|e| e.rot());
    if let (Some(rot), Some(mut p)) = (rot, ix.ents.store.get_mut(ix.player)) {
        p.set_rot(Rotation {
            y_rot: rot.y_rot,
            x_rot: rot.x_rot,
            y_head_rot: rot.y_rot,
        });
    }
    crate::riding::start_riding(ix.ents, ix.env, ix.player, ix.target, false);
    // `AbstractHorse.addPassenger`: the rider looks where the horse looked at the start of its
    // last tick (`absSnapRotationTo(getViewYRot(0), getViewXRot(0))`); the mount teleport of
    // the player keeps that rotation.
    if ix.ents.vehicle_of(ix.player) == Some(ix.target) {
        let [y, x] = ix
            .ents
            .mob(ix.target)
            .map_or([0.0; 2], |m| st(m).view_rot_o);
        if let Some(mut p) = ix.ents.store.get_mut(ix.player) {
            let y_rot = y % 360.0;
            p.set_rot(Rotation {
                y_rot,
                x_rot: x.clamp(-90.0, 90.0),
                y_head_rot: y_rot,
            });
        }
    }
    Interaction::Success
}

/// `mobInteract` of the horses, donkeys, mules, skeleton and zombie horses and llamas.
pub fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
    let Some(m) = ix.ents.mob(ix.target) else {
        return Interaction::Pass;
    };
    let kind = m.kind;
    let (tame, baby) = (m.tame, m.baby);
    if kind == Kind::SkeletonHorse && !tame {
        return Interaction::Pass;
    }
    if kind == Kind::ZombieHorse {
        // `ZombieHorse.interact`: any interaction keeps it.
        if let Some(m) = ix.ents.mob_mut(ix.target) {
            m.persistent = true;
        }
    }
    let vehicle = !ix.ents.riding.passengers_of(ix.target).is_empty();
    let open_inventory = !baby && tame && ix.sneaking;
    let dandelion = item_name(ix.stack) == "minecraft:golden_dandelion";
    if kind != Kind::SkeletonHorse
        && !vehicle
        && !open_inventory
        && (!baby || !dandelion)
        && !ix.stack.is_empty()
    {
        {
            if crate::mob::fam::is_food(kind, ix.stack.item) {
                return fed_food(ix);
            }
            if !tame {
                make_mad(ix);
                return Interaction::Success;
            }
            if is_chested(kind)
                && !ix.ents.mob(ix.target).is_some_and(|m| st(m).chest)
                && item_name(ix.stack) == "minecraft:chest"
            {
                equip_chest(ix);
                return Interaction::Success;
            }
        }
    }
    // `AbstractHorse.mobInteract`.
    if vehicle || baby {
        return super::animal_interact(ix);
    }
    if tame && ix.sneaking {
        open_inventory_screen(ix);
        return Interaction::Success;
    }
    if !ix.stack.is_empty() {
        if try_saddle(ix) {
            return Interaction::Success;
        }
        let wearing = ix
            .ents
            .mob(ix.target)
            .is_some_and(|m| !m.equipment.get(EquipmentSlot::Body).is_empty());
        if accepts_body(kind, ix.stack) && !wearing {
            let one = ix.stack.with_count(1);
            consume(ix);
            if let Some(m) = ix.ents.mob_mut(ix.target) {
                m.equipment.set(EquipmentSlot::Body, one);
                m.equipment.set_guaranteed_drop(EquipmentSlot::Body);
            }
            send_equipment(ix);
            let s = if matches!(kind, Kind::Llama | Kind::TraderLlama) {
                "minecraft:entity.llama.swag"
            } else {
                "minecraft:entity.horse.armor"
            };
            sound(ix, s, 1.0, 1.0);
            return Interaction::Success;
        }
    }
    if kind == Kind::TraderLlama && super::llama::leashed_to_trader(ix.ents, ix.target) {
        return Interaction::Success;
    }
    ride(ix)
}

/// `openCustomInventoryScreen`: the mount inventory menu.
pub fn open_inventory_screen(ix: &mut Interact<'_, '_>) {
    if let Some(request) = crate::menu::mount::request(ix.ents, ix.target, ix.player) {
        ix.cx.menus.push(request);
    }
}

/// `AbstractChestedHorse.getSlot(499).set(chest)` of a dispenser: a tame, living chested
/// horse without a chest takes it (no sound). Whether it did.
pub fn dispense_chest(ents: &mut crate::entities::Entities, id: EntityId) -> bool {
    let Some(m) = ents.mob_mut(id) else {
        return false;
    };
    if !is_chested(m.kind) || !m.is_alive() || st(m).chest || !m.tame {
        return false;
    }
    st_mut(m).chest = true;
    let size = inventory_size(m);
    st_mut(m).inventory.resize(size, ItemStack::default());
    if let Some(mut e) = ents.store.get_mut(id) {
        e.mark_data_dirty();
    }
    true
}

/// `equipChest`.
fn equip_chest(ix: &mut Interact<'_, '_>) {
    let kind = ix.ents.mob(ix.target).map(|m| m.kind);
    if let Some(m) = ix.ents.mob_mut(ix.target) {
        st_mut(m).chest = true;
        let size = inventory_size(m);
        st_mut(m).inventory.resize(size, ItemStack::default());
    }
    let s = match kind {
        Some(Kind::Mule) => "minecraft:entity.mule.chest",
        Some(Kind::Llama | Kind::TraderLlama) => "minecraft:entity.llama.chest",
        _ => "minecraft:entity.donkey.chest",
    };
    let pitch = ix.ents.mob_mut(ix.target).map_or(1.0, |m| {
        (m.random.next_float() - m.random.next_float()) * 0.2 + 1.0
    });
    sound(ix, s, 1.0, pitch);
    consume(ix);
    dirty(ix);
}

/// `getInventorySize`: the columns times three rows.
pub fn inventory_size(m: &MobData) -> usize {
    let columns = if !st(m).chest {
        0
    } else if matches!(m.kind, Kind::Llama | Kind::TraderLlama) {
        st(m).strength
    } else {
        5
    };
    usize::try_from(columns * 3).unwrap_or(0)
}

// --- Breeding -----------------------------------------------------------------------------

/// `canParent`.
fn can_parent(m: &MobData) -> bool {
    m.tame && !m.baby && m.health >= m.max_health() && m.in_love > 0
}

/// `canMate` of the horses.
pub fn can_mate(me: &MobData, partner: &MobData) -> bool {
    let ok = match me.kind {
        Kind::Horse | Kind::Donkey => matches!(partner.kind, Kind::Horse | Kind::Donkey),
        Kind::Llama | Kind::TraderLlama => {
            matches!(partner.kind, Kind::Llama | Kind::TraderLlama)
        }
        _ => false,
    };
    ok && can_parent(me) && can_parent(partner)
}

/// The kind of the foal.
pub fn offspring_kind(parent: &MobData, partner: &MobData) -> Kind {
    match (parent.kind, partner.kind) {
        (Kind::Horse, Kind::Donkey) | (Kind::Donkey, Kind::Horse) => Kind::Mule,
        (k, _) => k,
    }
}

/// `createOffspringAttribute`.
pub fn offspring_attribute(a: f64, b: f64, min: f64, max: f64, r: &mut LegacyRandom) -> f64 {
    let a = a.clamp(min, max);
    let b = b.clamp(min, max);
    let margin = 0.15 * (max - min);
    let range = (a - b).abs() + margin * 2.0;
    let average = (a + b) / 2.0;
    let quality = (r.next_double() + r.next_double() + r.next_double()) / 3.0 - 0.5;
    let v = average + range * quality;
    if v > max {
        max - (v - max)
    } else if v < min {
        min + (min - v)
    } else {
        v
    }
}

/// `setOffspringAttributes`: health, jump strength and speed from both parents.
pub fn offspring_attributes(
    parent: &MobData,
    partner: &MobData,
    child: &mut MobData,
    r: &mut LegacyRandom,
) {
    let (pa, pb) = (parent.attrs, partner.attrs);
    let h = offspring_attribute(pa.max_health, pb.max_health, 15.0, 30.0, r);
    let j = offspring_attribute(
        pa.jump_strength,
        pb.jump_strength,
        f64::from(gen_jump(0.0) as f32),
        f64::from(gen_jump(1.0) as f32),
        r,
    );
    let s = offspring_attribute(
        pa.movement_speed,
        pb.movement_speed,
        f64::from(gen_speed(0.0) as f32),
        f64::from(gen_speed(1.0) as f32),
        r,
    );
    set_max_health(child, h);
    child.attrs.jump_strength = j;
    child.attrs.movement_speed = s;
}

/// `getBreedOffspring` of the horses: markings and variants of a foal.
pub fn breed_offspring(
    parent: &MobData,
    partner: &MobData,
    child: &mut MobData,
    r: &mut LegacyRandom,
) {
    if child.kind == Kind::Horse && partner.kind == Kind::Horse {
        let skin = r.next_int_bounded(9);
        let variant = if skin < 4 {
            parent.variant & 0xFF
        } else if skin < 8 {
            partner.variant & 0xFF
        } else {
            r.next_int_bounded(7)
        };
        let mark = r.next_int_bounded(5);
        let markings = if mark < 2 {
            (parent.variant >> 8) & 0xFF
        } else if mark < 4 {
            (partner.variant >> 8) & 0xFF
        } else {
            r.next_int_bounded(5)
        };
        child.variant = (variant & 0xFF) | ((markings << 8) & 0xFF00);
    }
    if matches!(child.kind, Kind::Horse | Kind::Donkey | Kind::Mule) && parent.kind != Kind::Mule {
        offspring_attributes(parent, partner, child, r);
    }
    // Vanilla never sets `Bred` (only `Bred` from NBT does), so foals stay unbred.
}

// --- Data and storage ---------------------------------------------------------------------

/// The flags byte with the tamed bit from [`MobData::tame`].
fn flags_byte(m: &MobData) -> u8 {
    (st(m).horse_flags & !TAMED) | if m.tame { TAMED } else { 0 }
}

/// The synched data of the horses and llamas.
pub fn encode(w: &mut DataWriter<'_>, m: &MobData) {
    w.byte(18, flags_byte(m), 0);
    match m.kind {
        Kind::Horse => w.int(19, m.variant, 0),
        Kind::Donkey | Kind::Mule => w.boolean(19, st(m).chest, false),
        Kind::Llama | Kind::TraderLlama => {
            w.boolean(19, st(m).chest, false);
            w.int(20, st(m).strength, 0);
            w.int(21, m.variant, 0);
        }
        _ => {}
    }
}

/// `addAdditionalSaveData` of the horses and llamas.
pub fn save(m: &MobData, out: &mut Compound) {
    let s: &State = st(m);
    out.insert("EatingHaystack", has_flag(m, EATING));
    out.insert("Bred", has_flag(m, BRED));
    out.insert("Temper", s.temper);
    out.insert("Tame", m.tame);
    match m.kind {
        Kind::Horse => out.insert("Variant", m.variant),
        Kind::SkeletonHorse => {
            out.insert("SkeletonTrap", s.trap);
            out.insert("SkeletonTrapTime", s.trap_time);
        }
        _ => {}
    }
    if is_chested(m.kind) {
        out.insert("ChestedHorse", s.chest);
        if s.chest {
            let mut items = Vec::new();
            for (i, stack) in s.inventory.iter().enumerate() {
                if stack.is_empty() {
                    continue;
                }
                let mut c = crate::inventory::stack_to_nbt(stack);
                c.insert("Slot", i8::try_from(i).unwrap_or(i8::MAX));
                items.push(Tag::Compound(c));
            }
            out.insert("Items", List(items));
        }
    }
    if matches!(m.kind, Kind::Llama | Kind::TraderLlama) {
        out.insert("Variant", m.variant);
        out.insert("Strength", s.strength);
    }
    if m.kind == Kind::TraderLlama {
        out.insert("DespawnDelay", s.despawn_delay);
    }
}

/// `readAdditionalSaveData` of the horses and llamas.
pub fn load(m: &mut MobData, tag: &Compound) {
    m.tame = super::boolean(tag, "Tame");
    if matches!(m.kind, Kind::Llama | Kind::TraderLlama) {
        st_mut(m).strength = int(tag, "Strength").unwrap_or(0).clamp(1, 5);
        m.variant = int(tag, "Variant").unwrap_or(0).clamp(0, 3);
    }
    let eating = super::boolean(tag, "EatingHaystack");
    let bred = super::boolean(tag, "Bred");
    set_flag(m, EATING, eating);
    set_flag(m, BRED, bred);
    st_mut(m).temper = int(tag, "Temper").unwrap_or(0);
    match m.kind {
        Kind::Horse => m.variant = int(tag, "Variant").unwrap_or(0),
        Kind::SkeletonHorse => {
            st_mut(m).trap = super::boolean(tag, "SkeletonTrap");
            st_mut(m).trap_time = int(tag, "SkeletonTrapTime").unwrap_or(0);
        }
        Kind::TraderLlama => {
            st_mut(m).despawn_delay = int(tag, "DespawnDelay").unwrap_or(47_999);
        }
        _ => {}
    }
    if is_chested(m.kind) {
        st_mut(m).chest = super::boolean(tag, "ChestedHorse");
        let size = inventory_size(m);
        let mut inv = vec![ItemStack::default(); size];
        if let Some(Tag::List(items)) = tag.get("Items") {
            for t in &items.0 {
                let Tag::Compound(c) = t else {
                    continue;
                };
                let slot = int(c, "Slot").unwrap_or(-1);
                if let Ok(i) = usize::try_from(slot)
                    && i < size
                {
                    inv[i] = crate::inventory::stack_from_nbt(c);
                }
            }
        }
        st_mut(m).inventory = inv;
    }
}

// --- Spawn rules --------------------------------------------------------------------------

/// `checkSkeletonHorseSpawnRules`.
pub fn skeleton_spawn_rules(level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    if matches!(reason, SpawnReason::Spawner | SpawnReason::TrialSpawner) {
        super::bright_enough(level, pos, reason)
    } else {
        super::animal_spawn_rules(level, pos, reason)
    }
}

// --- Goals --------------------------------------------------------------------------------

pub fn can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::RunAround { pos, .. } => {
            if mob_controlled(t) || t.mob_ref().tame || !super::is_vehicle(t) {
                return false;
            }
            match randompos::default_pos(t, 5, 4) {
                Some(p) => {
                    *pos = p;
                    true
                }
                None => false,
            }
        }
        Goal::RandomStand { next_stand } => {
            *next_stand += 1;
            if *next_stand > 0 && t.mob().random.next_int_bounded(1000) < *next_stand {
                *next_stand = -400;
                let immobile = {
                    let m = t.mob_ref();
                    m.is_dead_or_dying()
                        || has_flag(m, EATING)
                        || has_flag(m, STANDING)
                        || (super::is_vehicle(t) && is_saddled(m))
                };
                !immobile && t.mob().random.next_int_bounded(10) == 0
            } else {
                false
            }
        }
        Goal::SkeletonTrap => {
            if !st(t.mob_ref()).trap {
                return false;
            }
            let p = t.pos();
            let area = Aabb::new(
                p.x - 10.0,
                p.y - 10.0,
                p.z - 10.0,
                p.x + 10.0,
                p.y + 10.0,
                p.z + 10.0,
            );
            let mut near = false;
            let ents = &*t.ents;
            ents.store.for_each_in(&area, true, |id| {
                if let (Some(pd), Some(e)) = (ents.player(id), ents.store.get(id))
                    && !pd.survival.is_dead_or_dying()
                    && pd.game_mode != vanillite_config::GameMode::Spectator
                    && e.pos().distance_to_sqr(p) < 100.0
                {
                    near = true;
                }
            });
            near
        }
    }
}

pub fn can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::RunAround { .. } => {
            !t.mob_ref().tame && !t.nav_ref().is_done() && super::is_vehicle(t)
        }
        Goal::RandomStand { .. } => false,
        Goal::SkeletonTrap => can_use(t, g),
    }
}

pub fn start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::RunAround { speed, pos } => {
            nav::move_to_pos(t, *pos, *speed);
        }
        Goal::RandomStand { .. } => {
            stand_if_possible(t.mob());
            t.core_mut().mark_data_dirty();
            if let Some(s) = ambient_sound(t.mob_ref()) {
                let id = t.id;
                crate::mob::combat::mob_sound(t.ents, t.env, id, s);
            }
        }
        Goal::SkeletonTrap => {}
    }
}

pub fn stop(_t: &mut Tick<'_, '_>, _g: &mut Goal) {}

pub fn tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::RunAround { .. } => {
            if t.mob_ref().tame {
                return;
            }
            let interval = crate::mob::goals::reduced(50);
            if t.mob().random.next_int_bounded(interval.max(1)) != 0 {
                return;
            }
            let Some(&passenger) = t.ents.riding.passengers_of(t.id).first() else {
                return;
            };
            if t.ents.player(passenger).is_some() {
                let temper = st(t.mob_ref()).temper;
                let max = max_temper(t.mob_ref().kind);
                let owner = t.ents.store.get(passenger).map(|e| e.uuid());
                if max > 0
                    && t.mob().random.next_int_bounded(max) < temper
                    && owner.is_none_or(|o| crate::mob::tame::tame_event(t.ents, t.id, o))
                {
                    // `tameWithName`.
                    if let Some(o) = owner {
                        let m = t.mob();
                        m.tame = true;
                        m.owner = Some(o);
                        let id = t.id;
                        crate::mob::tame::award_tame(t.ents, id, passenger);
                    }
                    let id = t.id;
                    crate::mob::living::entity_event(t, id, 7);
                    t.core_mut().mark_data_dirty();
                    return;
                }
                let m = t.mob();
                let max = max_temper(m.kind);
                let s = st_mut(m);
                s.temper = (s.temper + 5).clamp(0, max);
            }
            crate::riding::eject_passengers(t.ents, t.env, t.id);
            // `makeMad`.
            let standing = has_flag(t.mob_ref(), STANDING);
            if !standing {
                stand_if_possible(t.mob());
                t.core_mut().mark_data_dirty();
                if let Some(s) = angry_sound(t.mob_ref()) {
                    let id = t.id;
                    crate::mob::combat::mob_sound(t.ents, t.env, id, s);
                }
            }
            let id = t.id;
            crate::mob::living::entity_event(t, id, 6);
        }
        Goal::SkeletonTrap => skeleton_trap(t),
        Goal::RandomStand { .. } => {}
    }
}

/// `SkeletonTrapGoal.tick`: the trap springs: four skeleton riders on skeleton horses.
fn skeleton_trap(t: &mut Tick<'_, '_>) {
    {
        let m = t.mob();
        st_mut(m).trap = false;
        m.tame = true;
        m.age = 0;
        m.baby = false;
    }
    t.core_mut().mark_data_dirty();
    let pos = t.pos();
    // `SkeletonTrapGoal.tick`: `getCurrentDifficultyAt(horse.blockPosition())`.
    let block = t.block_pos();
    let network_ids = std::sync::Arc::clone(t.level.network_ids_arc());
    let spawn_skeleton = |t: &mut Tick<'_, '_>, horse: EntityId| {
        let seed = crate::mob::new_entity_seed();
        let mut sk = MobData::new(Kind::Skeleton, seed);
        let mut ctx = SpawnContext::at(t.level, block, SpawnReason::Triggered);
        crate::mob::kinds::finalize_spawn(&mut sk, &mut ctx, t.env.random);
        sk.persistent = true;
        sk.damage_cooldown = 60;
        if sk.equipment.get(EquipmentSlot::Head).is_empty() {
            sk.equipment.set(
                EquipmentSlot::Head,
                equipment::stack("minecraft:iron_helmet"),
            );
        }
        let special = ctx.special_multiplier;
        for slot in [EquipmentSlot::MainHand, EquipmentSlot::Head] {
            let mut s = sk.equipment.get(slot).clone();
            s.patch_mut()
                .unset(vanillite_protocol::components::ids::ENCHANTMENTS);
            if let Some(d) = crate::player::play::game_data() {
                d.enchanting
                    .enchant_mob_spawn_equipment(&mut s, special, &mut sk.random);
            }
            sk.equipment.set(slot, s);
        }
        let hp = t.ents.store.get(horse).map_or(pos, |e| e.pos());
        let uuid = crate::mob::new_uuid();
        let id = crate::mob::spawn(t.ents, &network_ids, uuid, hp, Rotation::default(), sk);
        t.ents.riding.add(id, horse, false);
        t.ents
            .spawned(id, vanillite_abi::spawn_reason::TRIGGERED, 0);
    };
    let me = t.id;
    spawn_skeleton(t, me);
    for _ in 0..3 {
        let seed = crate::mob::new_entity_seed();
        let mut horse = MobData::new(Kind::SkeletonHorse, seed);
        let mut ctx = SpawnContext::at(t.level, block, SpawnReason::Triggered);
        crate::mob::kinds::finalize_spawn(&mut horse, &mut ctx, t.env.random);
        horse.damage_cooldown = 60;
        horse.persistent = true;
        horse.tame = true;
        horse.age = 0;
        horse.baby = false;
        let dx = t.mob().random.triangle(0.0, 1.1485);
        let dz = t.mob().random.triangle(0.0, 1.1485);
        let uuid = crate::mob::new_uuid();
        let id = crate::mob::spawn(t.ents, &network_ids, uuid, pos, Rotation::default(), horse);
        if !t
            .ents
            .spawned(id, vanillite_abi::spawn_reason::TRIGGERED, 0)
        {
            continue;
        }
        if let Some(mut e) = t.ents.store.get_mut(id) {
            e.set_velocity(Vec3::new(dx, 0.0, dz));
        }
        spawn_skeleton(t, id);
    }
    crate::riding::position_all_riders(t.ents);
}
