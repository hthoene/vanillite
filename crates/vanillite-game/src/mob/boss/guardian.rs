//! Guardians and elder guardians (`Guardian`, `ElderGuardian`): swimming, the beam attack,
//! thorns and the elders' mining fatigue.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_data::block::flags;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::GameEvent;
use vanillite_world::entity::{EntityFlags, EntityId};

use super::data;
use crate::effect::{EffectInstance, MobEffect};
use crate::entities::Entities;
use crate::level::Level;
use crate::mob::goals::{self, Goal, TargetState, flag, reduced};
use crate::mob::living::{self, has_line_of_sight};
use crate::mob::nav::{self, Operation, rotlerp};
use crate::mob::path::{malus_of, path_type_static};
use crate::mob::{Kind, MobContext, MobData, Tick, atan2_deg, combat, kinds};
use crate::survival::Effect;
use crate::survival::damage::{DamageSource, DamageTypes, tag};
use crate::survival::player::Env;
use crate::survival::rules::Difficulty;

/// `DATA_ID_MOVING`.
const DATA_MOVING: u8 = 16;
/// `DATA_ID_ATTACK_TARGET`.
const DATA_ATTACK_TARGET: u8 = 17;
/// Entity event of the beam's start (the client's attack sound).
const EVENT_ATTACK: i8 = 21;
/// `ClientboundGameEventPacket.GUARDIAN_ELDER_EFFECT`.
const GAME_EVENT_ELDER_EFFECT: u8 = 10;

/// The kind state.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// `DATA_ID_MOVING`: swimming under its move control (spikes retracted).
    pub moving: bool,
    /// `DATA_ID_ATTACK_TARGET`: network id of the beam's target, `0` without one.
    pub attack_target: i32,
    /// `Mob.homePosition` and `homeRadius` (elder guardians stay near where they started).
    pub home: Option<(BlockPos, i32)>,
}

/// Whether `kind` is a guardian or an elder guardian.
pub fn is_guardian(kind: Kind) -> bool {
    matches!(kind, Kind::Guardian | Kind::ElderGuardian)
}

/// `getAttackDuration`.
pub fn attack_duration(kind: Kind) -> i32 {
    if kind == Kind::ElderGuardian { 60 } else { 80 }
}

/// The beam's magic damage (`GuardianAttackGoal.tick`): 1, +2 on hard, +2 for elders.
pub fn beam_magic_damage(kind: Kind, difficulty: Difficulty) -> f32 {
    let mut damage = 1.0;
    if difficulty == Difficulty::Hard {
        damage += 2.0;
    }
    if kind == Kind::ElderGuardian {
        damage += 2.0;
    }
    damage
}

/// The land variant of a guardian's ambient, hurt and death sound (`isInWater` picks the
/// water one); other mobs keep `sound`.
pub fn sound_variant(m: &MobData, sound: &'static str) -> &'static str {
    if !is_guardian(m.kind) || m.fluids.in_water() {
        return sound;
    }
    match sound {
        "minecraft:entity.guardian.ambient" => "minecraft:entity.guardian.ambient_land",
        "minecraft:entity.guardian.hurt" => "minecraft:entity.guardian.hurt_land",
        "minecraft:entity.guardian.death" => "minecraft:entity.guardian.death_land",
        "minecraft:entity.elder_guardian.ambient" => "minecraft:entity.elder_guardian.ambient_land",
        "minecraft:entity.elder_guardian.hurt" => "minecraft:entity.elder_guardian.hurt_land",
        "minecraft:entity.elder_guardian.death" => "minecraft:entity.elder_guardian.death_land",
        other => other,
    }
}

// --- Goals --------------------------------------------------------------------------------

/// The guardian specific goals.
#[derive(Clone, Copy, Debug)]
pub enum GuardianGoal {
    /// `GuardianAttackGoal`.
    Attack { attack_time: i32 },
    /// `MoveTowardsRestrictionGoal(mob, 1.0)` (flags move and look).
    MoveTowardsRestriction { wanted: Vec3 },
    /// `RandomStrollGoal(mob, 1.0, interval)` (flags move and look).
    Stroll {
        interval: i32,
        wanted: Vec3,
        force_trigger: bool,
    },
    /// `LookAtPlayerGoal(mob, Guardian.class, 12.0F, 0.01F)`.
    LookAtGuardian {
        look_at: Option<EntityId>,
        time: i32,
    },
    /// `NearestAttackableTargetGoal(mob, LivingEntity.class, 10, true, false,
    /// GuardianAttackSelector)`.
    Target {
        state: TargetState,
        interval: i32,
        found: Option<EntityId>,
    },
}

impl GuardianGoal {
    /// The control flags the goal locks.
    pub fn flags(&self) -> u8 {
        match self {
            GuardianGoal::Attack { .. }
            | GuardianGoal::MoveTowardsRestriction { .. }
            | GuardianGoal::Stroll { .. } => flag::MOVE | flag::LOOK,
            GuardianGoal::LookAtGuardian { .. } => flag::LOOK,
            GuardianGoal::Target { .. } => flag::TARGET,
        }
    }

    /// `requiresUpdateEveryTick`.
    pub fn every_tick(&self) -> bool {
        matches!(self, GuardianGoal::Attack { .. })
    }
}

/// `Guardian.registerGoals` (and the elder's longer stroll interval), with the water-bound
/// navigation and the elder's persistence.
pub fn register_goals(m: &mut MobData) {
    let elder = m.kind == Kind::ElderGuardian;
    let g = &mut m.goals;
    g.add(4, Goal::Guardian(GuardianGoal::Attack { attack_time: 0 }));
    g.add(
        5,
        Goal::Guardian(GuardianGoal::MoveTowardsRestriction { wanted: Vec3::ZERO }),
    );
    g.add(
        7,
        Goal::Guardian(GuardianGoal::Stroll {
            interval: if elder { 400 } else { 80 },
            wanted: Vec3::ZERO,
            force_trigger: false,
        }),
    );
    g.add(8, Goal::look_at_player(8.0));
    g.add(
        8,
        Goal::Guardian(GuardianGoal::LookAtGuardian {
            look_at: None,
            time: 0,
        }),
    );
    g.add(9, Goal::random_look());
    m.targets.add(
        1,
        Goal::Guardian(GuardianGoal::Target {
            state: TargetState::new(true),
            interval: reduced(10),
            found: None,
        }),
    );
    m.nav.kind = crate::mob::nav::NavKind::Water { breaching: false };
    if elder {
        m.persistent = true;
    }
}

fn state(t: &Tick<'_, '_>) -> State {
    t.mob_ref().boss.guardian().cloned().unwrap_or_default()
}

fn state_mut<'a>(t: &'a mut Tick<'_, '_>) -> Option<&'a mut State> {
    t.mob().boss.guardian_mut()
}

/// `setActiveAttackTarget(entityId)`.
fn set_active_attack_target(t: &mut Tick<'_, '_>, network_id: i32) {
    let changed = state_mut(t).is_some_and(|s| {
        let changed = s.attack_target != network_id;
        s.attack_target = network_id;
        changed
    });
    if changed {
        t.core_mut().mark_data_dirty();
    }
}

/// `setMoving(value)`.
fn set_moving(t: &mut Tick<'_, '_>, moving: bool) {
    let changed = state_mut(t).is_some_and(|s| {
        let changed = s.moving != moving;
        s.moving = moving;
        changed
    });
    if changed {
        t.core_mut().mark_data_dirty();
    }
}

/// `randomStrollGoal.trigger()` of a guardian's mob data.
fn trigger_stroll(m: &mut MobData) {
    for slot in &mut m.goals.slots {
        if let Goal::Guardian(GuardianGoal::Stroll { force_trigger, .. }) = &mut slot.goal {
            *force_trigger = true;
        }
    }
}

/// `LookControl.setLookAt(entity, yMax, xMax)`: at the entity's eyes.
fn look_at_entity(t: &mut Tick<'_, '_>, target: EntityId, y_max: f32, x_max: f32) {
    if let Some(o) = t.other(target) {
        t.mob()
            .look
            .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), y_max, x_max);
    }
}

/// `canUse`.
pub fn goal_can_use(t: &mut Tick<'_, '_>, g: &mut GuardianGoal) -> bool {
    match g {
        GuardianGoal::Attack { .. } => attack_can_use(t),
        GuardianGoal::MoveTowardsRestriction { wanted } => {
            let Some((home, _)) = state(t).home else {
                return false;
            };
            if is_within_home(t, t.block_pos()) {
                return false;
            }
            let towards = Vec3::new(
                f64::from(home.x) + 0.5,
                f64::from(home.y),
                f64::from(home.z) + 0.5,
            );
            match random_pos_towards(t, 16, 7, towards, f64::from(std::f32::consts::FRAC_PI_2)) {
                Some(p) => {
                    *wanted = p;
                    true
                }
                None => false,
            }
        }
        GuardianGoal::Stroll {
            interval,
            wanted,
            force_trigger,
        } => {
            if !*force_trigger {
                if t.mob_ref().no_action_time >= 100 {
                    return false;
                }
                let bound = reduced(*interval).max(1);
                if t.mob().random.next_int_bounded(bound) != 0 {
                    return false;
                }
            }
            match random_pos(t, 10, 7) {
                Some(p) => {
                    *wanted = p;
                    *force_trigger = false;
                    true
                }
                None => false,
            }
        }
        GuardianGoal::LookAtGuardian { look_at, .. } => {
            if t.mob().random.next_float() >= 0.01 {
                return false;
            }
            if t.mob_ref().target.is_some() {
                *look_at = t.mob_ref().target;
            }
            *look_at = nearest_guardian(t, 12.0);
            look_at.is_some()
        }
        GuardianGoal::Target {
            interval, found, ..
        } => {
            if *interval > 0 && t.mob().random.next_int_bounded(*interval) != 0 {
                return false;
            }
            *found = find_target(t);
            found.is_some()
        }
    }
}

fn attack_can_use(t: &Tick<'_, '_>) -> bool {
    t.mob_ref().target.is_some_and(|id| t.other(id).is_some())
}

/// `canContinueToUse`.
pub fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut GuardianGoal) -> bool {
    match g {
        GuardianGoal::Attack { .. } => {
            if !attack_can_use(t) {
                return false;
            }
            if t.mob_ref().kind == Kind::ElderGuardian {
                return true;
            }
            t.mob_ref()
                .target
                .and_then(|id| t.other(id))
                .is_some_and(|o| t.pos().distance_to_sqr(o.pos) > 9.0)
        }
        GuardianGoal::MoveTowardsRestriction { .. } | GuardianGoal::Stroll { .. } => {
            !t.nav_ref().is_done()
        }
        GuardianGoal::LookAtGuardian { look_at, time } => {
            let Some(o) = look_at.and_then(|id| t.other(id)) else {
                return false;
            };
            t.pos().distance_to_sqr(o.pos) <= 144.0 && *time > 0
        }
        GuardianGoal::Target { state, .. } => goals::target_can_continue(t, state),
    }
}

/// `start`.
pub fn goal_start(t: &mut Tick<'_, '_>, g: &mut GuardianGoal) {
    match g {
        GuardianGoal::Attack { attack_time } => {
            *attack_time = -10;
            t.nav().stop();
            if let Some(target) = t.mob_ref().target {
                look_at_entity(t, target, 90.0, 90.0);
            }
            // `needsSync`.
            t.core_mut().set_flags(EntityFlags::HAS_IMPULSE, true);
        }
        GuardianGoal::MoveTowardsRestriction { wanted } | GuardianGoal::Stroll { wanted, .. } => {
            nav::move_to_pos(t, *wanted, 1.0);
        }
        GuardianGoal::LookAtGuardian { time, .. } => {
            let r = t.mob().random.next_int_bounded(40);
            *time = reduced(40 + r);
        }
        GuardianGoal::Target { state, found, .. } => {
            goals::set_target(t, *found);
            state.unseen_ticks = 0;
        }
    }
}

/// `stop`.
pub fn goal_stop(t: &mut Tick<'_, '_>, g: &mut GuardianGoal) {
    match g {
        GuardianGoal::Attack { .. } => {
            set_active_attack_target(t, 0);
            goals::set_target(t, None);
            trigger_stroll(t.mob());
        }
        GuardianGoal::MoveTowardsRestriction { .. } => {}
        GuardianGoal::Stroll { .. } => t.nav().stop(),
        GuardianGoal::LookAtGuardian { look_at, .. } => *look_at = None,
        GuardianGoal::Target { state, .. } => {
            goals::set_target(t, None);
            state.target_mob = None;
        }
    }
}

/// `tick`.
pub fn goal_tick(t: &mut Tick<'_, '_>, g: &mut GuardianGoal) {
    match g {
        GuardianGoal::Attack { attack_time } => attack_tick(t, attack_time),
        GuardianGoal::LookAtGuardian { look_at, time } => {
            if let Some(o) = look_at.and_then(|id| t.other(id)) {
                nav::look_at(t, Vec3::new(o.pos.x, o.eye_y, o.pos.z));
                *time -= 1;
            }
        }
        _ => {}
    }
}

/// `GuardianAttackGoal.tick`: charges the beam, then hits with magic and a melee attack.
fn attack_tick(t: &mut Tick<'_, '_>, attack_time: &mut i32) {
    let Some(target) = t.mob_ref().target else {
        return;
    };
    t.nav().stop();
    look_at_entity(t, target, 90.0, 90.0);
    if !has_line_of_sight(t, target) {
        goals::set_target(t, None);
        return;
    }
    *attack_time += 1;
    let kind = t.mob_ref().kind;
    if *attack_time == 0 {
        let network_id = t.ents.store.get(target).map_or(0, |e| e.network_id().0);
        set_active_attack_target(t, network_id);
        if !t.mob_ref().silent {
            let id = t.id;
            living::entity_event(t, id, EVENT_ATTACK);
        }
    } else if *attack_time >= attack_duration(kind) {
        let damage = beam_magic_damage(kind, t.env.difficulty);
        let source = combat::entity_source(t.ents, t.env.types.indirect_magic, t.id, t.id);
        combat::hurt(t.level, t.ents, t.env, t.cx, target, source, damage);
        if t.exists() {
            combat::mob_attack(t, target);
        }
        if t.exists() {
            goals::set_target(t, None);
        }
    }
}

/// `GuardianAttackSelector` with `NearestAttackableTargetGoal.findTarget`: the nearest player,
/// squid, glow squid or axolotl more than 3 blocks away.
fn find_target(t: &mut Tick<'_, '_>) -> Option<EntityId> {
    let range = t.mob_ref().attrs.follow_range;
    let area = t.aabb().inflate(range, range, range);
    crate::mob::common::find_nearest(t, is_prey, range, true, false, area)
}

/// `GuardianAttackSelector.test`.
fn is_prey(t: &Tick<'_, '_>, id: EntityId) -> bool {
    let prey = t.ents.player(id).is_some()
        || t.ents.mob(id).is_some_and(|m| {
            matches!(
                m.kind,
                crate::mob::Kind::Squid | crate::mob::Kind::GlowSquid | crate::mob::Kind::Axolotl
            )
        });
    prey && t
        .ents
        .store
        .get(id)
        .is_some_and(|e| e.pos().distance_to_sqr(t.pos()) > 9.0)
}

/// `getNearestEntity(getEntitiesOfClass(Guardian.class, box.inflate(d, 3, d)), forNonCombat
/// .range(d), ...)`.
fn nearest_guardian(t: &mut Tick<'_, '_>, distance: f64) -> Option<EntityId> {
    let area = t.aabb().inflate(distance, 3.0, distance);
    let p = t.pos();
    let at = Vec3::new(p.x, t.eye_y(), p.z);
    let mut candidates = std::mem::take(&mut t.cx.ids);
    candidates.clear();
    t.ents
        .store
        .for_each_in(&area, false, |id| candidates.push(id));
    let mut best: Option<(EntityId, f64)> = None;
    for &id in &candidates {
        if id == t.id
            || !t
                .ents
                .mob(id)
                .is_some_and(|m| is_guardian(m.kind) && m.is_alive())
        {
            continue;
        }
        let Some(pos) = t.ents.store.get(id).map(|e| e.pos()) else {
            continue;
        };
        let visible = distance.max(2.0);
        if p.distance_to_sqr(pos) > visible * visible || !has_line_of_sight(t, id) {
            continue;
        }
        let d = pos.distance_to_sqr(at);
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((id, d));
        }
    }
    t.cx.ids = candidates;
    best.map(|(id, _)| id)
}

// --- Random positions (`DefaultRandomPos` with the home restriction) ---------------------

/// `isWithinHome(pos)`.
fn is_within_home(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    match state(t).home {
        None => true,
        Some((home, radius)) => home.dist_sqr(pos) < f64::from(radius * radius),
    }
}

/// `GoalUtils.mobRestricted(mob, horizontalDist)`.
fn mob_restricted(t: &Tick<'_, '_>, horizontal: i32) -> bool {
    let Some((home, radius)) = state(t).home else {
        return false;
    };
    let p = t.pos();
    let c = Vec3::new(
        f64::from(home.x) + 0.5,
        f64::from(home.y) + 0.5,
        f64::from(home.z) + 0.5,
    );
    let dist = f64::from(radius) + f64::from(horizontal) + 1.0;
    c.distance_to_sqr(p) < dist * dist
}

/// `RandomPos.generateRandomPosTowardDirection`.
fn toward_direction(t: &mut Tick<'_, '_>, xz_dist: f64, dir: (i32, i32, i32)) -> BlockPos {
    let (mut xt, mut zt) = (f64::from(dir.0), f64::from(dir.2));
    let p = t.pos();
    if let Some((home, _)) = state(t).home
        && xz_dist > 1.0
    {
        let r = &mut t.mob().random;
        if p.x > f64::from(home.x) {
            xt -= r.next_double() * xz_dist / 2.0;
        } else {
            xt += r.next_double() * xz_dist / 2.0;
        }
        if p.z > f64::from(home.z) {
            zt -= r.next_double() * xz_dist / 2.0;
        } else {
            zt += r.next_double() * xz_dist / 2.0;
        }
    }
    BlockPos::containing(xt + p.x, f64::from(dir.1) + p.y, zt + p.z)
}

/// `DefaultRandomPos.generateRandomPosTowardDirection`: the position, if usable for a
/// water-bound guardian.
fn accept(
    t: &mut Tick<'_, '_>,
    horizontal: i32,
    restrict: bool,
    dir: (i32, i32, i32),
) -> Option<BlockPos> {
    let pos = toward_direction(t, f64::from(horizontal), dir);
    if !t.level.in_height(pos.y) {
        return None;
    }
    if restrict && !is_within_home(t, pos) {
        return None;
    }
    // `WaterBoundPathNavigation.isStableDestination`.
    if t.level.block(pos).has(flags::SOLID_RENDER) {
        return None;
    }
    let kind = path_type_static(t.level, t.level.min_y(), pos.x, pos.y, pos.z);
    if malus_of(t.mob_ref().kind.malus(), kind) != 0.0 {
        return None;
    }
    Some(pos)
}

/// `RandomPos.generateRandomPos`: the best of ten candidates by walk target value.
fn best_of(
    t: &mut Tick<'_, '_>,
    mut candidate: impl FnMut(&mut Tick<'_, '_>) -> Option<BlockPos>,
) -> Option<Vec3> {
    let mut best: Option<(BlockPos, f64)> = None;
    for _ in 0..10 {
        let Some(pos) = candidate(t) else {
            continue;
        };
        let value = kinds::walk_target_value(t.mob_ref().kind, t.level, &t.cx.env, pos);
        if best.is_none_or(|(_, b)| value > b) {
            best = Some((pos, value));
        }
    }
    best.map(|(p, _)| Vec3::new(f64::from(p.x) + 0.5, f64::from(p.y), f64::from(p.z) + 0.5))
}

/// `DefaultRandomPos.getPos(mob, h, v)`.
fn random_pos(t: &mut Tick<'_, '_>, h: i32, v: i32) -> Option<Vec3> {
    let restrict = mob_restricted(t, h);
    best_of(t, |t| {
        let r = &mut t.mob().random;
        let x = r.next_int_bounded(2 * h + 1) - h;
        let y = r.next_int_bounded(2 * v + 1) - v;
        let z = r.next_int_bounded(2 * h + 1) - h;
        accept(t, h, restrict, (x, y, z))
    })
}

/// `DefaultRandomPos.getPosTowards(mob, h, v, towards, maxXzRadiansFromDir)`.
fn random_pos_towards(
    t: &mut Tick<'_, '_>,
    h: i32,
    v: i32,
    towards: Vec3,
    max_radians: f64,
) -> Option<Vec3> {
    let p = t.pos();
    let dir = towards - p;
    let restrict = mob_restricted(t, h);
    best_of(t, |t| {
        let d = direction_within_radians(
            &mut t.mob().random,
            f64::from(h),
            v,
            dir.x,
            dir.z,
            max_radians,
        )?;
        accept(t, h, restrict, d)
    })
}

/// `RandomPos.generateRandomDirectionWithinRadians` with a minimum distance and flying
/// height of 0.
fn direction_within_radians(
    random: &mut impl RandomSource,
    max_horizontal: f64,
    vertical: i32,
    x_dir: f64,
    z_dir: f64,
    max_radians: f64,
) -> Option<(i32, i32, i32)> {
    let center = vanillite_base::math::atan2(z_dir, x_dir) - f64::from(std::f32::consts::FRAC_PI_2);
    let y_radians = center + f64::from(2.0 * random.next_float() - 1.0) * max_radians;
    let dist = vanillite_base::math::lerp(random.next_double().sqrt(), 0.0, max_horizontal)
        * f64::from(std::f32::consts::SQRT_2);
    let xt = -dist * y_radians.sin();
    let zt = dist * y_radians.cos();
    if xt.abs() > max_horizontal || zt.abs() > max_horizontal {
        return None;
    }
    let yt = random.next_int_bounded(2 * vertical + 1) - vertical;
    let pos = BlockPos::containing(xt, f64::from(yt), zt);
    Some((pos.x, pos.y, pos.z))
}

// --- Tick hooks ---------------------------------------------------------------------------

pub fn before_tick(t: &mut Tick<'_, '_>) {
    let _ = t;
}

pub fn after_tick(t: &mut Tick<'_, '_>) {
    let _ = t;
}

/// `Guardian.aiStep` before `super.aiStep()`: air in water, flopping on land, facing the
/// beam's target.
pub fn ai_step_pre(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().is_alive() {
        return;
    }
    if t.mob_ref().fluids.in_water() {
        t.mob().air = 300;
    } else if t.on_ground() {
        let r = &mut t.mob().random;
        let x = f64::from((r.next_float() * 2.0 - 1.0) * 0.4);
        let z = f64::from((r.next_float() * 2.0 - 1.0) * 0.4);
        let y_rot = r.next_float() * 360.0;
        let v = t.velocity();
        t.set_velocity(v + Vec3::new(x, 0.5, z));
        let mut rot = t.core().rot();
        rot.y_rot = y_rot;
        let mut core = t.core_mut();
        core.set_rot(rot);
        core.set_flags(EntityFlags::ON_GROUND, false);
        // `needsSync`.
        core.set_flags(EntityFlags::HAS_IMPULSE, true);
    }
    if state(t).attack_target != 0 {
        let mut rot = t.core().rot();
        rot.y_rot = rot.y_head_rot;
        t.core_mut().set_rot(rot);
    }
}

pub fn ai_step(t: &mut Tick<'_, '_>) {
    let _ = t;
}

/// `ElderGuardian.customServerAiStep`: the mining fatigue curse every minute and the home.
pub fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
    if t.mob_ref().kind != Kind::ElderGuardian {
        return;
    }
    let network_id = t.core().network_id().0;
    if (t.mob_ref().tick_count + network_id) % 1200 == 0 {
        let pos = t.pos();
        let silent = t.mob_ref().silent;
        let cursed = curse_targets(t.ents, pos);
        let packet = SharedPacket::of(&GameEvent {
            event: GAME_EVENT_ELDER_EFFECT,
            param: if silent { 0.0 } else { 1.0 },
        });
        for player in cursed {
            crate::effect::living::add_detached_from(t.ents, player, curse_effect(), Some(t.id));
            t.env.out.push(Effect::To {
                player,
                packet: std::sync::Arc::clone(&packet),
            });
        }
    }
    if state(t).home.is_none() {
        let pos = t.block_pos();
        if let Some(s) = state_mut(t) {
            s.home = Some((pos, 16));
        }
    }
}

/// The curse: mining fatigue III for 5 minutes.
pub fn curse_effect() -> EffectInstance {
    EffectInstance::new(MobEffect::MINING_FATIGUE, 6000, 2)
}

/// Whether a player with `current` mining fatigue gets the curse (again): without it, with a
/// weaker one or one that ends within the display limit of 1200 ticks.
pub fn needs_curse(current: Option<&EffectInstance>) -> bool {
    let curse = curse_effect();
    match current {
        None => true,
        Some(e) => e.amplifier < curse.amplifier || e.ends_within(1200 - 1),
    }
}

/// `MobEffectUtil.addEffectToPlayersAround(level, elder, pos, 50, curse, 1200)`'s selection:
/// survival and adventure players within 50 blocks without a curse at least as strong that
/// lasts longer than a minute.
pub fn curse_targets(ents: &Entities, pos: Vec3) -> Vec<EntityId> {
    let curse = curse_effect();
    let mut out = Vec::new();
    for (id, p) in ents.players.iter() {
        if !matches!(p.game_mode, GameMode::Survival | GameMode::Adventure) {
            continue;
        }
        let Some(e) = ents.store.get(id) else {
            continue;
        };
        if e.pos().distance_to_sqr(pos) >= 50.0 * 50.0 {
            continue;
        }
        if needs_curse(p.survival.effects.get(curse.effect)) {
            out.push(id);
        }
    }
    out
}

/// `GuardianMoveControl.tick`.
pub fn tick_move_control(t: &mut Tick<'_, '_>) {
    let ctl = t.mob_ref().move_ctl;
    if ctl.operation != Operation::MoveTo || t.nav_ref().is_done() {
        let m = t.mob();
        m.speed = 0.0;
        m.zza = 0.0;
        set_moving(t, false);
        return;
    }
    let p = t.pos();
    let delta = ctl.wanted - p;
    let length = delta.length();
    let (xd, yd, zd) = (delta.x / length, delta.y / length, delta.z / length);
    #[allow(clippy::cast_possible_truncation)]
    let y_rot_d = atan2_deg(delta.z, delta.x) as f32 - 90.0;
    let mut rot = t.core().rot();
    rot.y_rot = rotlerp(rot.y_rot, y_rot_d, 90.0);
    t.core_mut().set_rot(rot);
    t.mob().y_body_rot = rot.y_rot;
    #[allow(clippy::cast_possible_truncation)]
    let target_speed = (ctl.speed_modifier * t.mob_ref().attrs.movement_speed) as f32;
    let new_speed = {
        let m = t.mob();
        let s = m.speed + 0.125 * (target_speed - m.speed);
        // `Mob.setSpeed`: the forward input follows the speed.
        m.speed = s;
        m.zza = s;
        s
    };
    let phase = f64::from(t.mob_ref().tick_count + t.core().network_id().0);
    let push = (phase * 0.5).sin() * 0.05;
    let (sin, cos) = java_sin_cos(rot.y_rot);
    let y_push = (phase * 0.75).sin() * 0.05;
    let v = t.velocity();
    t.set_velocity(
        v + Vec3::new(
            push * cos,
            y_push * (sin + cos) * 0.25 + f64::from(new_speed) * yd * 0.1,
            push * sin,
        ),
    );
    let eye = t.eye_y();
    let new_look = Vec3::new(p.x + xd * 2.0, eye + yd / length, p.z + zd * 2.0);
    let look = t.mob_ref().look;
    let old = if look.is_looking() {
        look.wanted
    } else {
        new_look
    };
    let lerp = |a: f64, b: f64| a + 0.125 * (b - a);
    t.mob().look.set_look_at(
        Vec3::new(
            lerp(old.x, new_look.x),
            lerp(old.y, new_look.y),
            lerp(old.z, new_look.z),
        ),
        10.0,
        40.0,
    );
    set_moving(t, true);
}

/// `Guardian.travelInWater`; other fluids and the air move as usual.
pub fn travel(t: &mut Tick<'_, '_>, input: Vec3) -> bool {
    if !t.mob_ref().fluids.in_water() {
        return false;
    }
    living::move_relative(t, 0.1, input);
    let v = t.velocity();
    living::move_self(t, v);
    if !t.exists() {
        return true;
    }
    let mut v = t.velocity() * 0.9;
    if !state(t).moving && t.mob_ref().target.is_none() {
        v = v + Vec3::new(0.0, -0.005, 0.0);
    }
    t.set_velocity(v);
    true
}

/// `Guardian.hurtServer` before the damage: thorns against a living attacker while the spikes
/// are out, and a stroll away.
pub fn filter_hurt(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: &DamageSource,
    amount: f32,
) -> Option<f32> {
    let moving = ents
        .mob(target)
        .and_then(|m| m.boss.guardian())
        .is_some_and(|s| s.moving);
    if let Some(attacker) = source.direct
        && thorns_apply(
            env.types,
            moving,
            source,
            ents.mob(attacker).is_some() || ents.player(attacker).is_some(),
        )
    {
        let thorns = combat::entity_source(ents, env.types.thorns, target, target);
        combat::hurt(level, ents, env, cx, attacker, thorns, 2.0);
    }
    if let Some(m) = ents.mob_mut(target) {
        trigger_stroll(m);
    }
    Some(amount)
}

/// Whether a hit makes the spikes hurt the attacker: the spikes are out (not moving), the
/// damage is not in `#avoids_guardian_thorns` (magic, thorns and explosions) and not thorns,
/// and the direct attacker is a living entity.
pub fn thorns_apply(
    types: &DamageTypes,
    moving: bool,
    source: &DamageSource,
    direct_is_living: bool,
) -> bool {
    let avoids = source.kind == types.magic
        || source.kind == types.thorns
        || types.is(source.kind, tag::IS_EXPLOSION);
    !moving && !avoids && direct_is_living
}

pub fn after_hurt(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: &DamageSource,
    took: bool,
) {
    let _ = (level, ents, env, cx, target, source, took);
}

/// `entities/guardian` and `entities/elder_guardian` of 26.3 without looting.
pub fn death_loot(m: &mut MobData, killed_by_player: bool) -> Vec<crate::item::ItemStack> {
    let elder = m.kind == Kind::ElderGuardian;
    let on_fire = m.is_on_fire();
    let r = &mut m.random;
    let mut names: Vec<&'static str> = Vec::new();
    let mut counts: Vec<i32> = Vec::new();
    let mut push = |name: &'static str, count: i32| {
        names.push(name);
        counts.push(count);
    };
    push(
        "minecraft:prismarine_shard",
        r.next_int_between_inclusive(0, 2),
    );
    // Cod (smelted when burning), prismarine crystals or nothing.
    let cod = if elder { 3 } else { 2 };
    let roll = r.next_int_bounded(cod + 2 + 1);
    if roll < cod {
        push(
            if on_fire {
                "minecraft:cooked_cod"
            } else {
                "minecraft:cod"
            },
            1,
        );
    } else if roll < cod + 2 {
        push("minecraft:prismarine_crystals", 1);
    }
    if elder && killed_by_player {
        push("minecraft:wet_sponge", 1);
    }
    // A fish of `gameplay/fishing/fish` (2.5 % when killed by a player).
    if killed_by_player && r.next_float() < 0.025 {
        let roll = r.next_int_bounded(100);
        let fish = if roll < 60 {
            if on_fire {
                "minecraft:cooked_cod"
            } else {
                "minecraft:cod"
            }
        } else if roll < 85 {
            if on_fire {
                "minecraft:cooked_salmon"
            } else {
                "minecraft:salmon"
            }
        } else if roll < 87 {
            "minecraft:tropical_fish"
        } else {
            "minecraft:pufferfish"
        };
        push(fish, 1);
    }
    if elder && r.next_int_bounded(5) >= 4 {
        push("minecraft:tide_armor_trim_smithing_template", 1);
    }
    names
        .into_iter()
        .zip(counts)
        .filter(|&(_, c)| c > 0)
        .filter_map(|(n, c)| {
            vanillite_data::registry::ITEM
                .id(n)
                .map(|id| crate::item::ItemStack::new(id, c))
        })
        .collect()
}

/// `DATA_ID_MOVING` and `DATA_ID_ATTACK_TARGET`.
pub fn entity_data(out: &mut Vec<u8>, s: &State, all: bool) {
    data::boolean(out, DATA_MOVING, s.moving, false, all);
    data::int(out, DATA_ATTACK_TARGET, s.attack_target, 0, all);
}

/// `home_radius` and `home_pos` (`Mob.addAdditionalSaveData`).
pub fn save(s: &State, out: &mut Compound) {
    if let Some((pos, radius)) = s.home {
        out.insert("home_radius", Tag::Int(radius));
        out.insert("home_pos", Tag::IntArray(vec![pos.x, pos.y, pos.z]));
    }
}

/// Reads `home_radius` and `home_pos`.
pub fn load(s: &mut State, tag: &Compound) {
    let radius = tag
        .get("home_radius")
        .and_then(Tag::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(-1);
    if radius >= 0 {
        let pos = match tag.get("home_pos") {
            Some(Tag::IntArray(v)) if v.len() == 3 => BlockPos::new(v[0], v[1], v[2]),
            _ => BlockPos::ZERO,
        };
        s.home = Some((pos, radius));
    }
}

// --- Spawning -----------------------------------------------------------------------------

/// `Guardian.checkGuardianSpawnRules` for natural spawning.
pub fn check_spawn_rules(
    level: &Level,
    pos: BlockPos,
    difficulty: Difficulty,
    random: &mut impl RandomSource,
) -> bool {
    (random.next_int_bounded(20) == 0 || !can_see_sky_from_below_water(level, pos))
        && difficulty != Difficulty::Peaceful
        && crate::mob::is_water(level.block(pos))
        && crate::mob::is_water(level.block(BlockPos::new(pos.x, pos.y - 1, pos.z)))
}

/// `LevelReader.canSeeSkyFromBelowWater`.
pub fn can_see_sky_from_below_water(level: &Level, pos: BlockPos) -> bool {
    let sea = level.climate().map_or(63, |c| c.sea_level);
    if pos.y >= sea {
        return crate::mob::can_see_sky(level, pos);
    }
    let top = BlockPos::new(pos.x, sea, pos.z);
    if !crate::mob::can_see_sky(level, top) {
        return false;
    }
    let mut y = sea - 1;
    while y > pos.y {
        let state = level.block(BlockPos::new(pos.x, y, pos.z));
        if state.light_dampening() > 0 && state.fluid_state().is_empty() {
            return false;
        }
        y -= 1;
    }
    true
}

/// `SpawnPlacementTypes.IN_WATER.isSpawnPositionOk`.
pub fn in_water_spawn_position_ok(level: &Level, pos: BlockPos) -> bool {
    crate::mob::is_water(level.block(pos))
        && !level
            .block(BlockPos::new(pos.x, pos.y + 1, pos.z))
            .has(flags::REDSTONE_CONDUCTOR)
}

/// `Math.sin` and `Math.cos` of `yRot * (float)(Math.PI / 180.0)`.
fn java_sin_cos(y_rot: f32) -> (f64, f64) {
    #[allow(clippy::cast_possible_truncation)]
    let rad = f64::from(y_rot * (std::f64::consts::PI / 180.0) as f32);
    (rad.sin(), rad.cos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::survival::damage::{DamageKind, DamageType, DeathMessageType, Effects, Scaling};

    #[test]
    fn beam_charges_and_hurts_like_vanilla() {
        assert_eq!(attack_duration(Kind::Guardian), 80);
        assert_eq!(attack_duration(Kind::ElderGuardian), 60);
        assert_eq!(beam_magic_damage(Kind::Guardian, Difficulty::Normal), 1.0);
        assert_eq!(beam_magic_damage(Kind::Guardian, Difficulty::Hard), 3.0);
        assert_eq!(
            beam_magic_damage(Kind::ElderGuardian, Difficulty::Easy),
            3.0
        );
        assert_eq!(
            beam_magic_damage(Kind::ElderGuardian, Difficulty::Hard),
            5.0
        );
    }

    fn types() -> DamageTypes {
        let ty = |id: &str, tags: u32| DamageType {
            id: id.to_owned(),
            msg_id: String::new(),
            scaling: Scaling::Never,
            exhaustion: 0.0,
            effects: Effects::Hurt,
            death_message_type: DeathMessageType::Default,
            tags,
        };
        DamageTypes::new(vec![
            ty("minecraft:generic", 0),
            ty("minecraft:player_attack", 0),
            ty("minecraft:magic", 0),
            ty("minecraft:thorns", 0),
            ty("minecraft:explosion", tag::IS_EXPLOSION),
            ty("minecraft:arrow", tag::IS_PROJECTILE),
        ])
    }

    #[test]
    fn thorns_hurt_living_attackers_while_the_spikes_are_out() {
        let types = types();
        let hit = |kind: DamageKind| DamageSource::of(kind);
        assert!(thorns_apply(&types, false, &hit(types.player_attack), true));
        assert!(!thorns_apply(&types, true, &hit(types.player_attack), true));
        assert!(!thorns_apply(&types, false, &hit(types.magic), true));
        assert!(!thorns_apply(&types, false, &hit(types.thorns), true));
        assert!(!thorns_apply(&types, false, &hit(types.explosion), true));
        // An arrow is the direct entity: not living.
        assert!(!thorns_apply(&types, false, &hit(types.arrow), false));
    }

    #[test]
    fn elder_curse_selection() {
        let fatigue = |amplifier, duration| {
            EffectInstance::new(MobEffect::MINING_FATIGUE, duration, amplifier)
        };
        assert!(needs_curse(None));
        assert!(needs_curse(Some(&fatigue(1, 6000))));
        assert!(!needs_curse(Some(&fatigue(2, 6000))));
        assert!(!needs_curse(Some(&fatigue(2, 1200))));
        assert!(needs_curse(Some(&fatigue(2, 1199))));
        assert!(!needs_curse(Some(&fatigue(
            2,
            crate::effect::instance::INFINITE
        ))));
        let c = curse_effect();
        assert_eq!((c.amplifier, c.duration), (2, 6000));
    }

    #[test]
    fn mining_fatigue_and_haste_change_the_dig_speed() {
        use crate::player::play::dig_effects;
        assert!((dig_effects(1.0, None, Some(2)) - 0.027).abs() < 1.0e-6);
        assert!((dig_effects(10.0, Some(1), None) - 14.0).abs() < 1.0e-5);
        assert_eq!(dig_effects(3.0, None, None), 3.0);
        let mut effects = crate::effect::ActiveEffects::default();
        effects.force_add(curse_effect());
        assert_eq!(effects.amplifier(MobEffect::MINING_FATIGUE), Some(2));
    }

    #[test]
    fn elders_start_persistent_and_water_bound() {
        let m = MobData::new(Kind::ElderGuardian, 1);
        assert_eq!(
            sound_variant(&m, "minecraft:entity.elder_guardian.hurt"),
            "minecraft:entity.elder_guardian.hurt_land"
        );
        assert!(m.persistent);
        assert!(matches!(m.nav.kind, crate::mob::nav::NavKind::Water { .. }));
        let g = MobData::new(Kind::Guardian, 1);
        assert!(!g.persistent);
    }

    #[test]
    fn home_round_trips_through_nbt() {
        let s = State {
            home: Some((BlockPos::new(1, -2, 3), 16)),
            ..State::default()
        };
        let mut tag = Compound::new();
        save(&s, &mut tag);
        let mut back = State::default();
        load(&mut back, &tag);
        assert_eq!(back.home, s.home);
    }

    #[test]
    fn loot_without_a_player_kill() {
        for seed in 0..50 {
            let mut m = MobData::new(Kind::Guardian, seed);
            for stack in death_loot(&mut m, false) {
                let name = vanillite_data::registry::ITEM
                    .name(stack.item)
                    .unwrap_or("");
                assert!(
                    matches!(
                        name,
                        "minecraft:prismarine_shard"
                            | "minecraft:cod"
                            | "minecraft:prismarine_crystals"
                    ),
                    "{name}"
                );
            }
        }
    }
}
