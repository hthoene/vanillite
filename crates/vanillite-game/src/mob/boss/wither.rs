//! The wither (`WitherBoss`): summoning, the charge-up, its heads and skulls, block breaking
//! and the boss bar.
//!
//! Its navigation is `FlyingPathNavigation` reduced to a straight flight to the destination
//! (with Vanilla's stuck detection): the wither steers itself towards its target in `aiStep`,
//! the path only matters for idle wandering.

use std::sync::OnceLock;

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockState, ids as block_ids};
use vanillite_data::entity::ids as entity_ids;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::packets::play::clientbound::{BossBarColor, BossBarFlags, BossBarOverlay};
use vanillite_protocol::text::Component;
use vanillite_world::entity::{EntityFlags, EntityId, NewEntity, Rotation};
use vanillite_worldgen::behaviour::StateExt;

use super::data;
use super::special::{SpecialData, SpecialTick};
use crate::boss_bar::BossBar;
use crate::effect::{EffectInstance, MobEffect};
use crate::entities::Entities;
use crate::level::explosion::BlockResistance;
use crate::level::{Level, update};
use crate::mob::goals::{Goal, flag};
use crate::mob::nav::{self, Operation};
use crate::mob::path::{malus_of, path_type_static};
use crate::mob::physics::{self, Blocks};
use crate::mob::{
    ExplosionRequest, Kind, MobContext, MobData, Tick, WorldAction, atan2_deg, combat, kinds,
    living,
};
use crate::survival::damage::{DamageSource, tag};
use crate::survival::player::Env;
use crate::survival::rules::{Difficulty, Rule};

/// `INVULNERABLE_TICKS`: the charge-up after summoning.
pub const INVULNERABLE_TICKS: i32 = 220;

/// `LevelEvent` ids the wither uses.
mod event {
    pub const WITHER_BREAK_BLOCK: i32 = 1022;
    pub const WITHER_SPAWNED: i32 = 1023;
    pub const WITHER_SHOOT: i32 = 1024;
    pub const BLOCK_DESTROY: i32 = 2001;
}

/// The wither's flying navigation: where it flies and how fast (`FlyingPathNavigation`
/// reduced to the destination).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Flight {
    pub target: Vec3,
    pub speed: f64,
}

/// The kind state.
#[derive(Clone, Debug)]
pub struct State {
    /// `DATA_ID_INV`: ticks of the charge-up left.
    pub invulnerable_ticks: i32,
    /// `DATA_TARGET_A/B/C`: the heads' targets (entity and its network id).
    pub targets: [Option<(EntityId, i32)>; 3],
    /// `nextHeadUpdate`.
    pub next_head_update: [i32; 2],
    /// `idleHeadUpdates`.
    pub idle_head_updates: [i32; 2],
    /// `destroyBlocksTick`.
    pub destroy_blocks_tick: i32,
    /// The boss bar's UUID (`Mth.createInsecureUUID(random)`).
    pub bar_id: u128,
    /// The navigation's destination; `None` when done.
    pub flight: Option<Flight>,
    /// Stuck detection of the navigation (`lastStuckCheck`, `lastStuckCheckPos`).
    pub nav_tick: i32,
    pub last_stuck_check: i32,
    pub last_stuck_check_pos: Vec3,
}

impl Default for State {
    fn default() -> Self {
        State {
            invulnerable_ticks: 0,
            targets: [None; 3],
            next_head_update: [0; 2],
            idle_head_updates: [0; 2],
            destroy_blocks_tick: 0,
            bar_id: 0,
            flight: None,
            nav_tick: 0,
            last_stuck_check: 0,
            last_stuck_check_pos: Vec3::ZERO,
        }
    }
}

impl State {
    /// `isPowered` needs the health; see [`is_powered`].
    fn target(&self, head: usize) -> Option<EntityId> {
        self.targets.get(head).copied().flatten().map(|(id, _)| id)
    }
}

/// `isPowered`: at or below half health.
pub fn is_powered(m: &MobData) -> bool {
    m.health <= m.max_health() / 2.0
}

fn state(m: &MobData) -> Option<&State> {
    m.boss.wither()
}

fn state_mut(m: &mut MobData) -> Option<&mut State> {
    m.boss.wither_mut()
}

/// Whether an entity type is in `#wither_friends` (`#undead`: `#skeletons`, `#zombies`, the
/// wither and the phantom).
pub fn is_wither_friend(kind: vanillite_data::entity::EntityTypeId) -> bool {
    const FRIENDS: [&str; 17] = [
        "minecraft:skeleton",
        "minecraft:stray",
        "minecraft:wither_skeleton",
        "minecraft:skeleton_horse",
        "minecraft:bogged",
        "minecraft:parched",
        "minecraft:zombie_horse",
        "minecraft:camel_husk",
        "minecraft:zombie",
        "minecraft:zombie_villager",
        "minecraft:zombified_piglin",
        "minecraft:zoglin",
        "minecraft:drowned",
        "minecraft:husk",
        "minecraft:zombie_nautilus",
        "minecraft:wither",
        "minecraft:phantom",
    ];
    static IDS: OnceLock<Vec<u16>> = OnceLock::new();
    IDS.get_or_init(|| {
        FRIENDS
            .iter()
            .filter_map(|n| vanillite_data::registry::ENTITY_TYPE.id(n))
            .collect()
    })
    .contains(&kind.0)
}

/// `WitherBoss.canDestroy(state)`: not air and not `#wither_immune`.
pub fn can_destroy(state: BlockState) -> bool {
    !state.is_air() && !crate::block::block_tags().wither_immune.contains(state)
}

// --- Spawning -------------------------------------------------------------------------------

/// Whether `state` is a wither skeleton skull (floor or wall).
pub fn is_wither_skull(state: BlockState) -> bool {
    state.is(block_ids::WITHER_SKELETON_SKULL) || state.is(block_ids::WITHER_SKELETON_WALL_SKULL)
}

/// A cell of the wither pattern (`BlockPatternBuilder.aisle("^^^", "###", "~#~")`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    Skull,
    Base,
    Air,
    /// ` `: any block.
    Any,
}

const PATTERN: [[Cell; 3]; 3] = [
    [Cell::Skull, Cell::Skull, Cell::Skull],
    [Cell::Base, Cell::Base, Cell::Base],
    [Cell::Air, Cell::Base, Cell::Air],
];

/// A found pattern (`BlockPatternMatch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatternMatch {
    pub front_top_left: BlockPos,
    pub forwards: Direction,
    pub up: Direction,
}

fn step(d: Direction) -> (i32, i32, i32) {
    let [x, y, z] = d.normal();
    (x, y, z)
}

/// `BlockPattern.translateAndRotate`.
fn translate(
    origin: BlockPos,
    forwards: Direction,
    up: Direction,
    right: i32,
    down: i32,
) -> BlockPos {
    let f = step(forwards);
    let u = step(up);
    // `forwards.cross(up)`.
    let r = (
        f.1 * u.2 - f.2 * u.1,
        f.2 * u.0 - f.0 * u.2,
        f.0 * u.1 - f.1 * u.0,
    );
    BlockPos::new(
        origin.x + u.0 * -down + r.0 * right,
        origin.y + u.1 * -down + r.1 * right,
        origin.z + u.2 * -down + r.2 * right,
    )
}

impl PatternMatch {
    /// `getBlock(right, down, 0)`.
    pub fn block(&self, right: i32, down: i32) -> BlockPos {
        translate(self.front_top_left, self.forwards, self.up, right, down)
    }
}

fn cell_matches(level: &Level, cell: Cell, pos: BlockPos) -> bool {
    if !level.is_loaded(pos) {
        return false;
    }
    let state = level.block(pos);
    match cell {
        Cell::Skull => is_wither_skull(state),
        Cell::Base => crate::block::block_tags()
            .wither_summon_base_blocks
            .contains(state),
        Cell::Air => state.is_air(),
        Cell::Any => true,
    }
}

/// `getOrCreateWitherBase`: the pattern without its skulls.
const BASE_PATTERN: [[Cell; 3]; 3] = [
    [Cell::Any, Cell::Any, Cell::Any],
    [Cell::Base, Cell::Base, Cell::Base],
    [Cell::Air, Cell::Base, Cell::Air],
];

/// `WitherSkullBlock.canSpawnMob` of a dispensed wither skeleton skull: high enough, not
/// Peaceful, and the base pattern around `pos`.
pub fn can_spawn_mob(level: &Level, pos: BlockPos, difficulty: Difficulty) -> bool {
    pos.y >= level.min_y() + 2
        && difficulty != Difficulty::Peaceful
        && find_with(level, pos, &BASE_PATTERN).is_some()
}

/// `BlockPattern.find(level, origin)` of the full wither pattern.
pub fn find_pattern(level: &Level, origin: BlockPos) -> Option<PatternMatch> {
    find_with(level, origin, &PATTERN)
}

fn find_with(level: &Level, origin: BlockPos, pattern: &[[Cell; 3]; 3]) -> Option<PatternMatch> {
    // `BlockPos.betweenClosed(origin, origin + 2)`: x fastest, then y, then z.
    for dz in 0..3 {
        for dy in 0..3 {
            for dx in 0..3 {
                let test = BlockPos::new(origin.x + dx, origin.y + dy, origin.z + dz);
                for forwards in Direction::ALL {
                    for up in Direction::ALL {
                        if up == forwards || up == forwards.opposite() {
                            continue;
                        }
                        let all = (0..3).all(|x| {
                            (0..3).all(|y| {
                                cell_matches(
                                    level,
                                    pattern[y as usize][x as usize],
                                    translate(test, forwards, up, x, y),
                                )
                            })
                        });
                        if all {
                            return Some(PatternMatch {
                                front_top_left: test,
                                forwards,
                                up,
                            });
                        }
                    }
                }
            }
        }
    }
    None
}

/// `WitherSkullBlock.checkSpawn`: a wither skeleton skull placed at `pos` that completes the
/// pattern clears it and summons a charging wither. Returns the new wither.
pub fn check_spawn(
    level: &mut Level,
    pos: BlockPos,
    difficulty: Difficulty,
    seed: i64,
) -> Option<EntityId> {
    if !is_wither_skull(level.block(pos))
        || pos.y < level.min_y()
        || difficulty == Difficulty::Peaceful
    {
        return None;
    }
    let found = find_pattern(level, pos)?;
    let mut m = MobData::new(Kind::Wither, seed);
    // `CarvedPumpkinBlock.clearPatternBlocks`.
    for x in 0..3 {
        for y in 0..3 {
            let p = found.block(x, y);
            let state = level.block(p);
            level.set_block(p, BlockState::AIR, update::CLIENTS);
            level.level_event(None, event::BLOCK_DESTROY, p, i32::from(state.0));
        }
    }
    let spawn = found.block(1, 2);
    let yaw = if matches!(found.forwards, Direction::East | Direction::West) {
        0.0
    } else {
        90.0
    };
    make_invulnerable(&mut m);
    m.y_body_rot = yaw;
    let uuid = crate::mob::new_uuid();
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    let id = crate::mob::spawn(
        &mut level.entities,
        &network_ids,
        uuid,
        Vec3::new(
            f64::from(spawn.x) + 0.5,
            f64::from(spawn.y) + 0.55,
            f64::from(spawn.z) + 0.5,
        ),
        // `snapTo` turns the wither only: the head keeps the constructor's yaw.
        Rotation {
            y_rot: yaw,
            x_rot: 0.0,
            y_head_rot: crate::mob::constructor_y_rot(seed),
        },
        m,
    );
    level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::TRIGGERED, 0);
    // `SUMMONED_ENTITY` for the players within 50 blocks of the wither's box.
    if let Some(b) = level.entities.store.get(id).map(|e| e.aabb()) {
        let near = crate::mob::animal::players_in(&level.entities, &b.inflate(50.0, 50.0, 50.0));
        crate::mob::animal::award_summoned(&mut level.entities, &near, id);
    }
    // `CarvedPumpkinBlock.updatePatternBlocks`.
    for x in 0..3 {
        for y in 0..3 {
            level.update_neighbors_at(found.block(x, y), block_ids::AIR);
        }
    }
    Some(id)
}

/// `makeInvulnerable`: the charge-up with a third of the health.
pub fn make_invulnerable(m: &mut MobData) {
    let max = m.max_health();
    m.health = max / 3.0;
    if let Some(s) = state_mut(m) {
        s.invulnerable_ticks = INVULNERABLE_TICKS;
    }
}

// --- Goals ----------------------------------------------------------------------------------

/// The wither's own goals.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WitherGoal {
    /// `WitherDoNothingGoal`: nothing during the charge-up.
    DoNothing,
    /// `RangedAttackGoal(this, 1.0, 40, 20.0F)`.
    RangedAttack {
        target: Option<EntityId>,
        attack_time: i32,
        see_time: i32,
    },
    /// `WaterAvoidingRandomFlyingGoal(this, 1.0)`.
    RandomFly { wanted: Vec3 },
    /// `NearestAttackableTargetGoal(LivingEntity, 0, false, false, LIVING_ENTITY_SELECTOR)`.
    NearestLiving {
        found: Option<EntityId>,
        target_mob: Option<EntityId>,
    },
}

const ATTACK_INTERVAL: i32 = 40;
const ATTACK_RADIUS: f32 = 20.0;

impl WitherGoal {
    /// The control flags.
    pub fn flags(&self) -> u8 {
        match self {
            WitherGoal::DoNothing => flag::MOVE | flag::JUMP | flag::LOOK,
            WitherGoal::RangedAttack { .. } => flag::MOVE | flag::LOOK,
            WitherGoal::RandomFly { .. } => flag::MOVE,
            WitherGoal::NearestLiving { .. } => flag::TARGET,
        }
    }

    /// `requiresUpdateEveryTick`.
    pub fn every_tick(&self) -> bool {
        matches!(self, WitherGoal::RangedAttack { .. })
    }
}

/// `registerGoals`.
pub fn register_goals(m: &mut MobData) {
    let bar = crate::mob::random_uuid(&mut m.random);
    if let Some(s) = state_mut(m) {
        s.bar_id = bar;
    }
    m.goals.add(0, Goal::Wither(WitherGoal::DoNothing));
    m.goals.add(
        2,
        Goal::Wither(WitherGoal::RangedAttack {
            target: None,
            attack_time: -1,
            see_time: 0,
        }),
    );
    m.goals.add(
        5,
        Goal::Wither(WitherGoal::RandomFly { wanted: Vec3::ZERO }),
    );
    m.goals.add(6, Goal::look_at_player(8.0));
    m.goals.add(7, Goal::random_look());
    m.targets.add(1, Goal::hurt_by(false));
    m.targets.add(
        2,
        Goal::Wither(WitherGoal::NearestLiving {
            found: None,
            target_mob: None,
        }),
    );
    // `FlyingPathNavigation.setCanFloat(true)`.
    m.nav.can_float = true;
}

fn invulnerable_ticks(t: &Tick<'_, '_>) -> i32 {
    state(t.mob_ref()).map_or(0, |s| s.invulnerable_ticks)
}

/// `LivingEntity.canAttack(target)` (players: not in peaceful, not creative or spectator).
fn can_attack(ents: &Entities, env: &Env<'_>, me: EntityId, target: EntityId) -> bool {
    if target == me || crate::mob::entity_nbt::is_invulnerable(ents, target) {
        return false;
    }
    if let Some(p) = ents.player(target) {
        if env.difficulty == Difficulty::Peaceful {
            return false;
        }
        return !p.survival.is_dead_or_dying()
            && matches!(
                p.game_mode,
                vanillite_config::GameMode::Survival | vanillite_config::GameMode::Adventure
            );
    }
    ents.mob(target)
        .is_some_and(|m| m.is_alive() && crate::decoration::stand::attackable(m))
}

/// `TargetingConditions.forCombat().range(range).selector(LIVING_ENTITY_SELECTOR)` with
/// `wither` as the targeter.
fn test_target(t: &mut Tick<'_, '_>, target: EntityId, range: f64) -> bool {
    let me = t.id;
    let Some(e) = t.ents.store.get(target) else {
        return false;
    };
    if is_wither_friend(e.kind()) {
        return false;
    }
    if let Some(p) = t.ents.player(target)
        && p.game_mode == vanillite_config::GameMode::Spectator
    {
        return false;
    }
    if !can_attack(t.ents, t.env, me, target) {
        return false;
    }
    let target_pos = e.pos();
    if range > 0.0 {
        // `getVisibilityPercent`: sneaking, invisibility with the armour cover, mob heads.
        let modifier = crate::mob::brain::sensing::visibility_percent(t, target);
        let visible = (range * modifier).max(2.0);
        if t.pos().distance_to_sqr(target_pos) > visible * visible {
            return false;
        }
    }
    living::has_line_of_sight(t, target)
}

/// The living entities (players and mobs) in `area`, in section order.
fn living_in(t: &mut Tick<'_, '_>, area: &Aabb) -> Vec<EntityId> {
    let mut out = std::mem::take(&mut t.cx.ids);
    out.clear();
    let ents = &*t.ents;
    ents.store.for_each_in(area, false, |id| {
        if ents.mob(id).is_some() || ents.player(id).is_some() {
            out.push(id);
        }
    });
    let found = out.clone();
    t.cx.ids = out;
    found
}

/// `canUse`.
pub fn goal_can_use(t: &mut Tick<'_, '_>, g: &mut WitherGoal) -> bool {
    match g {
        WitherGoal::DoNothing => invulnerable_ticks(t) > 0,
        WitherGoal::RangedAttack { target, .. } => {
            let best = t.mob_ref().target.filter(|&id| t.other(id).is_some());
            *target = best;
            best.is_some()
        }
        WitherGoal::RandomFly { wanted } => {
            if t.mob_ref().no_action_time >= 100 {
                return false;
            }
            if t.mob()
                .random
                .next_int_bounded(crate::mob::goals::reduced(120))
                != 0
            {
                return false;
            }
            match random_fly_position(t) {
                Some(p) => {
                    *wanted = p;
                    true
                }
                None => false,
            }
        }
        WitherGoal::NearestLiving { found, .. } => {
            let range = t.mob_ref().attrs.follow_range;
            let area = t.aabb().inflate(range, range, range);
            let candidates = living_in(t, &area);
            let p = t.pos();
            let at = Vec3::new(p.x, t.eye_y(), p.z);
            let mut best: Option<(EntityId, f64)> = None;
            for id in candidates {
                if !test_target(t, id, range) {
                    continue;
                }
                let Some(e) = t.ents.store.get(id) else {
                    continue;
                };
                let d = e.pos().distance_to_sqr(at);
                if best.is_none_or(|(_, b)| d < b) {
                    best = Some((id, d));
                }
            }
            *found = best.map(|(id, _)| id);
            found.is_some()
        }
    }
}

/// `canContinueToUse`.
pub fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut WitherGoal) -> bool {
    match g {
        WitherGoal::DoNothing => invulnerable_ticks(t) > 0,
        WitherGoal::RangedAttack { target, .. } => {
            let previous = *target;
            if goal_can_use(t, g) {
                return true;
            }
            let alive = previous.is_some_and(|id| t.other(id).is_some());
            if let WitherGoal::RangedAttack { target, .. } = g {
                *target = previous;
            }
            alive && !nav_is_done(t)
        }
        WitherGoal::RandomFly { .. } => !nav_is_done(t),
        WitherGoal::NearestLiving { target_mob, .. } => {
            // `TargetGoal.canContinueToUse` without `mustSee`.
            let Some(target) = t.mob_ref().target.or(*target_mob) else {
                return false;
            };
            if !can_attack(t.ents, t.env, t.id, target) {
                return false;
            }
            let Some(o) = t.other(target) else {
                return false;
            };
            let range = t.mob_ref().attrs.follow_range;
            if t.pos().distance_to_sqr(o.pos) > range * range {
                return false;
            }
            t.mob().target = Some(target);
            true
        }
    }
}

/// `start`.
pub fn goal_start(t: &mut Tick<'_, '_>, g: &mut WitherGoal) {
    match g {
        WitherGoal::DoNothing | WitherGoal::RangedAttack { .. } => {}
        WitherGoal::RandomFly { wanted } => {
            let w = *wanted;
            nav_move_to(t, w, 1.0);
        }
        WitherGoal::NearestLiving { found, .. } => {
            let reason = crate::mob::goals::closest_reason(t, *found);
            crate::mob::goals::set_target_why(t, *found, reason);
        }
    }
}

/// `stop`.
pub fn goal_stop(t: &mut Tick<'_, '_>, g: &mut WitherGoal) {
    match g {
        WitherGoal::DoNothing => {}
        WitherGoal::RangedAttack {
            target,
            attack_time,
            see_time,
        } => {
            *target = None;
            *see_time = 0;
            *attack_time = -1;
        }
        WitherGoal::RandomFly { .. } => nav_stop(t),
        WitherGoal::NearestLiving { target_mob, .. } => {
            t.mob().target = None;
            *target_mob = None;
        }
    }
}

/// `tick`.
pub fn goal_tick(t: &mut Tick<'_, '_>, g: &mut WitherGoal) {
    let WitherGoal::RangedAttack {
        target: Some(target),
        attack_time,
        see_time,
    } = g
    else {
        return;
    };
    let target = *target;
    let Some(o) = t.other(target) else {
        return;
    };
    let dist_sqr = t.pos().distance_to_sqr(o.pos);
    let sees = living::has_line_of_sight(t, target);
    if sees {
        *see_time += 1;
    } else {
        *see_time = 0;
    }
    let radius_sqr = f64::from(ATTACK_RADIUS * ATTACK_RADIUS);
    if dist_sqr <= radius_sqr && *see_time >= 5 {
        nav_stop(t);
    } else {
        nav_move_to(t, o.pos, 1.0);
    }
    t.mob()
        .look
        .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 30.0, 30.0);
    *attack_time -= 1;
    if *attack_time == 0 {
        if !sees {
            return;
        }
        // `performRangedAttack(target, power)`: the power does not matter to the wither.
        perform_ranged_attack_at(t, 0, target);
        // `Mth.floor(dist * (max - min) + min)` with min = max.
        *attack_time = ATTACK_INTERVAL;
    } else if *attack_time < 0 {
        // `Mth.floor(Mth.lerp(dist / radius, min, max))` with min = max.
        *attack_time = ATTACK_INTERVAL;
    }
}

// --- Navigation and movement ----------------------------------------------------------------

fn nav_is_done(t: &Tick<'_, '_>) -> bool {
    state(t.mob_ref()).is_none_or(|s| s.flight.is_none())
}

fn nav_stop(t: &mut Tick<'_, '_>) {
    if let Some(s) = state_mut(t.mob()) {
        s.flight = None;
    }
}

fn nav_move_to(t: &mut Tick<'_, '_>, target: Vec3, speed: f64) {
    let pos = t.pos();
    if let Some(s) = state_mut(t.mob()) {
        if s.flight.is_none() {
            s.last_stuck_check = s.nav_tick;
            s.last_stuck_check_pos = pos;
        }
        s.flight = Some(Flight { target, speed });
    }
}

/// `PathNavigation.tick` of the straight flight: reaching the destination ends it, being
/// stuck for 100 ticks stops it; otherwise the move control flies on.
fn nav_tick(t: &mut Tick<'_, '_>) {
    let pos = t.pos();
    let width = f64::from(t.core().dims().width);
    let speed = t.mob_ref().speed;
    let Some(s) = state_mut(t.mob()) else {
        return;
    };
    s.nav_tick += 1;
    let Some(flight) = s.flight else {
        return;
    };
    // `maxDistanceToWaypoint`.
    let reach = if width > 0.75 {
        width / 2.0
    } else {
        0.75 - width / 2.0
    };
    let d = flight.target - pos;
    if d.x.abs() < reach && d.z.abs() < reach && d.y.abs() < 1.0 {
        s.flight = None;
        return;
    }
    // `doStuckDetection`.
    if s.nav_tick - s.last_stuck_check > 100 {
        let effective = if speed >= 1.0 { speed } else { speed * speed };
        let threshold = f64::from(effective * 100.0 * 0.25);
        let stuck = pos.distance_to_sqr(s.last_stuck_check_pos) < threshold * threshold;
        s.last_stuck_check = s.nav_tick;
        s.last_stuck_check_pos = pos;
        if stuck {
            s.flight = None;
            return;
        }
    }
    t.mob()
        .move_ctl
        .set_wanted_position(flight.target, flight.speed);
}

/// `FlyingMoveControl(this, 10, false).tick()`; replaces the ground move control.
pub fn move_control(t: &mut Tick<'_, '_>) {
    nav_tick(t);
    let ctl = t.mob_ref().move_ctl;
    if ctl.operation == Operation::MoveTo {
        t.mob().move_ctl.operation = Operation::Wait;
        t.core_mut().set_flags(EntityFlags::NO_GRAVITY, true);
        let p = t.pos();
        let xd = ctl.wanted.x - p.x;
        let yd = ctl.wanted.y - p.y;
        let zd = ctl.wanted.z - p.z;
        let dd = xd * xd + yd * yd + zd * zd;
        if dd < f64::from(2.500_000_3e-7_f32) {
            let m = t.mob();
            m.yya = 0.0;
            m.zza = 0.0;
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        let y_rot_d = atan2_deg(zd, xd) as f32 - 90.0;
        let mut rot = t.core().rot();
        rot.y_rot = nav::rotlerp(rot.y_rot, y_rot_d, 90.0);
        // Flying speed and movement speed are both 0.6 for the wither.
        #[allow(clippy::cast_possible_truncation)]
        let speed = (ctl.speed_modifier * t.mob_ref().attrs.movement_speed) as f32;
        t.mob().speed = speed;
        let sd = (xd * xd + zd * zd).sqrt();
        if yd.abs() > f64::from(1.0e-5_f32) || sd.abs() > f64::from(1.0e-5_f32) {
            #[allow(clippy::cast_possible_truncation)]
            let x_rot_d = -(atan2_deg(yd, sd) as f32);
            rot.x_rot = nav::clamp_x_rot(nav::rotlerp(rot.x_rot, x_rot_d, 10.0));
            t.mob().yya = if yd > 0.0 { speed } else { -speed };
        }
        t.core_mut().set_rot(rot);
        t.mob().zza = speed;
    } else {
        t.core_mut().set_flags(EntityFlags::NO_GRAVITY, false);
        let m = t.mob();
        m.yya = 0.0;
        m.zza = 0.0;
    }
}

/// `getViewVector(0)` from the rotation.
fn view_vector(t: &Tick<'_, '_>) -> Vec3 {
    let rot = t.core().rot();
    let x = f64::from(rot.x_rot) * std::f64::consts::PI / 180.0;
    let y = -f64::from(rot.y_rot) * std::f64::consts::PI / 180.0;
    let (ycos, ysin) = (
        f64::from(vanillite_base::math::cos(y)),
        f64::from(vanillite_base::math::sin(y)),
    );
    let (xcos, xsin) = (
        f64::from(vanillite_base::math::cos(x)),
        f64::from(vanillite_base::math::sin(x)),
    );
    Vec3::new(ysin * xcos, -xsin, ycos * xcos)
}

/// `RandomPos.generateRandomDirectionWithinRadians`.
fn direction_within_radians(
    random: &mut LegacyRandom,
    max_dist: f64,
    vertical: i32,
    flying_height: i32,
    xd: f64,
    zd: f64,
    max_radians: f64,
) -> Option<(i32, i32, i32)> {
    let center = vanillite_base::math::atan2(zd, xd) - f64::from(std::f32::consts::FRAC_PI_2);
    let y_radians = center + f64::from(2.0 * random.next_float() - 1.0) * max_radians;
    let dist = vanillite_base::math::lerp(random.next_double().sqrt(), 0.0, max_dist)
        * std::f64::consts::SQRT_2;
    let xt = -dist * y_radians.sin();
    let zt = dist * y_radians.cos();
    if xt.abs() > max_dist || zt.abs() > max_dist {
        return None;
    }
    let yt = random.next_int_bounded(2 * vertical + 1) - vertical + flying_height;
    let p = BlockPos::containing(xt, f64::from(yt), zt);
    Some((p.x, p.y, p.z))
}

fn is_solid(level: &Level, pos: BlockPos) -> bool {
    nav::is_solid(level.block(pos))
}

fn has_malus(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    let kind = path_type_static(t.level, t.level.min_y(), pos.x, pos.y, pos.z);
    malus_of(t.mob_ref().kind.malus(), kind) != 0.0
}

/// `entityCanStandOn`: the block's top face is full.
fn can_stand_on(level: &Level, pos: BlockPos) -> bool {
    level
        .block(pos)
        .collision_shape()
        .iter()
        .any(|b| b[0] <= 0.0 && b[2] <= 0.0 && b[3] >= 1.0 && b[5] >= 1.0 && b[4] >= 1.0)
}

/// `WaterAvoidingRandomFlyingGoal.getPosition`: `HoverRandomPos`, else `AirAndWaterRandomPos`.
fn random_fly_position(t: &mut Tick<'_, '_>) -> Option<Vec3> {
    let dir = view_vector(t);
    let half_pi = f64::from(std::f32::consts::FRAC_PI_2);
    let p = t.pos();
    let level = t.level;
    let max_y = level.max_y();
    let min_y = level.min_y();
    let best = |t: &mut Tick<'_, '_>,
                produce: &mut dyn FnMut(&mut Tick<'_, '_>) -> Option<BlockPos>| {
        let mut best: Option<(BlockPos, f64)> = None;
        for _ in 0..10 {
            if let Some(pos) = produce(t) {
                let value = kinds::walk_target_value(t.mob_ref().kind, t.level, &t.cx.env, pos);
                if best.is_none_or(|(_, b)| value > b) {
                    best = Some((pos, value));
                }
            }
        }
        best.map(|(b, _)| Vec3::new(f64::from(b.x) + 0.5, f64::from(b.y), f64::from(b.z) + 0.5))
    };
    // `HoverRandomPos.getPos(mob, 8, 7, x, z, PI / 2, 3, 1)`.
    let hover = best(t, &mut |t| {
        let (x, y, z) =
            direction_within_radians(&mut t.mob().random, 8.0, 7, 0, dir.x, dir.z, half_pi)?;
        let pos = BlockPos::containing(f64::from(x) + p.x, f64::from(y) + p.y, f64::from(z) + p.z);
        if pos.y < min_y || pos.y > max_y || !can_stand_on(t.level, pos) {
            return None;
        }
        let amount = t.mob().random.next_int_bounded(3) + 1;
        let pos = move_up_to_above_solid(t.level, pos, amount, max_y);
        let water = crate::mob::is_water(t.level.block(pos));
        (!water && !has_malus(t, pos)).then_some(pos)
    });
    if hover.is_some() {
        return hover;
    }
    // `AirAndWaterRandomPos.getPos(mob, 8, 4, -2, x, z, PI / 2)`.
    best(t, &mut |t| {
        let (x, y, z) =
            direction_within_radians(&mut t.mob().random, 8.0, 4, -2, dir.x, dir.z, half_pi)?;
        let pos = BlockPos::containing(f64::from(x) + p.x, f64::from(y) + p.y, f64::from(z) + p.z);
        if pos.y < min_y || pos.y > max_y {
            return None;
        }
        let pos = move_up_out_of_solid(t.level, pos, max_y);
        (!has_malus(t, pos)).then_some(pos)
    })
}

/// `RandomPos.moveUpOutOfSolid`.
fn move_up_out_of_solid(level: &Level, pos: BlockPos, max_y: i32) -> BlockPos {
    if !is_solid(level, pos) {
        return pos;
    }
    let mut y = pos.y + 1;
    while y <= max_y && is_solid(level, BlockPos::new(pos.x, y, pos.z)) {
        y += 1;
    }
    BlockPos::new(pos.x, y, pos.z)
}

/// `RandomPos.moveUpToAboveSolid`.
fn move_up_to_above_solid(level: &Level, pos: BlockPos, amount: i32, max_y: i32) -> BlockPos {
    if !is_solid(level, pos) {
        return pos;
    }
    let mut y = pos.y + 1;
    while y <= max_y && is_solid(level, BlockPos::new(pos.x, y, pos.z)) {
        y += 1;
    }
    let first = y;
    while y <= max_y && y - first < amount {
        y += 1;
        if is_solid(level, BlockPos::new(pos.x, y, pos.z)) {
            y -= 1;
            break;
        }
    }
    BlockPos::new(pos.x, y, pos.z)
}

// --- Tick -----------------------------------------------------------------------------------

/// Before `LivingEntity.tick`: `checkDespawn` keeps `noActionTime` at 0.
pub fn before_tick(t: &mut Tick<'_, '_>) {
    t.mob().no_action_time = 0;
}

/// After `LivingEntity.tick`: the boss bar follows the players that track the wither.
pub fn after_tick(t: &mut Tick<'_, '_>) {
    sync_bar(t);
}

/// `WitherBoss.aiStep` before `super.aiStep()`: hovering above the target and turning to the
/// flight direction.
pub fn ai_step_before(t: &mut Tick<'_, '_>) {
    let mut v = t.velocity().multiply(1.0, 0.6, 1.0);
    let target = state(t.mob_ref()).and_then(|s| s.target(0));
    if let Some(target) = target
        && let Some(e) = t.ents.store.get(target)
    {
        let target_pos = e.pos();
        let p = t.pos();
        let mut yd = v.y;
        if p.y < target_pos.y || !is_powered(t.mob_ref()) && p.y < target_pos.y + 5.0 {
            yd = yd.max(0.0);
            yd += 0.3 - yd * f64::from(0.6_f32);
        }
        v = Vec3::new(v.x, yd, v.z);
        let delta = Vec3::new(target_pos.x - p.x, 0.0, target_pos.z - p.z);
        if delta.x * delta.x + delta.z * delta.z > 9.0 {
            let scale = delta.normalize();
            v = v + Vec3::new(scale.x * 0.3 - v.x * 0.6, 0.0, scale.z * 0.3 - v.z * 0.6);
        }
    }
    t.set_velocity(v);
    if v.x * v.x + v.z * v.z > 0.05 {
        #[allow(clippy::cast_possible_truncation)]
        let y_rot = atan2_deg(v.z, v.x) as f32 - 90.0;
        let mut rot = t.core().rot();
        rot.y_rot = y_rot;
        t.core_mut().set_rot(rot);
    }
}

/// The parts of `aiStep` after `super.aiStep()`: the head turns are for clients, and the
/// particles (`addParticle` does nothing on the server) still draw their numbers: three
/// gaussians from the wither's random per head, while powered a `nextInt(4)` from the level
/// random with three more gaussians on 0, and while invulnerable three times a gaussian, a
/// float and a gaussian.
pub fn ai_step(t: &mut Tick<'_, '_>) {
    let powered = is_powered(t.mob_ref());
    for _ in 0..3 {
        let r = &mut t.mob().random;
        r.next_gaussian();
        r.next_gaussian();
        r.next_gaussian();
        if powered && t.env.random.next_int_bounded(4) == 0 {
            let r = &mut t.mob().random;
            r.next_gaussian();
            r.next_gaussian();
            r.next_gaussian();
        }
    }
    if state(t.mob_ref()).is_some_and(|s| s.invulnerable_ticks > 0) {
        let r = &mut t.mob().random;
        for _ in 0..3 {
            r.next_gaussian();
            r.next_float();
            r.next_gaussian();
        }
    }
}

/// `getHeadX/Y/Z(index)`.
fn head_pos(t: &Tick<'_, '_>, index: usize) -> Vec3 {
    let p = t.pos();
    let y = p.y + if index == 0 { 3.0 } else { f64::from(2.2_f32) };
    if index == 0 {
        return Vec3::new(p.x, y, p.z);
    }
    #[allow(clippy::cast_precision_loss)]
    let angle =
        (t.mob_ref().y_body_rot + 180.0 * (index as f32 - 1.0)) * (std::f32::consts::PI / 180.0);
    let cos = f64::from(vanillite_base::math::cos(f64::from(angle)));
    let sin = f64::from(vanillite_base::math::sin(f64::from(angle)));
    Vec3::new(p.x + cos * 1.3, y, p.z + sin * 1.3)
}

/// `performRangedAttack(head, target)`.
fn perform_ranged_attack_at(t: &mut Tick<'_, '_>, head: usize, target: EntityId) {
    let Some(o) = t.other(target) else {
        return;
    };
    let eye = o.eye_y - o.pos.y;
    let dangerous = head == 0 && t.mob().random.next_float() < 0.001;
    perform_ranged_attack(
        t,
        head,
        Vec3::new(o.pos.x, o.pos.y + eye * 0.5, o.pos.z),
        dangerous,
    );
}

/// `performRangedAttack(head, x, y, z, dangerous)`: a wither skull from the head.
fn perform_ranged_attack(t: &mut Tick<'_, '_>, head: usize, at: Vec3, dangerous: bool) {
    if !t.mob_ref().silent {
        let p = t.block_pos();
        t.cx.actions
            .push(WorldAction::LevelEvent(event::WITHER_SHOOT, p, 0));
    }
    let from = head_pos(t, head);
    let direction = (at - from).normalize();
    let rot = t.core().rot();
    let owner = t.id;
    let uuid = crate::mob::new_entity_uuid();
    spawn_skull(
        t.level, t.ents, owner, from, direction, rot, dangerous, uuid,
    );
}

/// `new WitherSkull(level, mob, direction)` at `pos`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_skull(
    level: &Level,
    ents: &mut Entities,
    owner: EntityId,
    pos: Vec3,
    direction: Vec3,
    rot: Rotation,
    dangerous: bool,
    uuid: u128,
) -> EntityId {
    let owner_uuid = ents.store.get(owner).map(|e| e.uuid());
    let mut new = NewEntity::new(
        entity_ids::WITHER_SKULL,
        level.network_ids().next(),
        uuid,
        pos,
    );
    new.velocity = direction.normalize() * 0.1;
    new.rot = Rotation {
        y_rot: rot.y_rot,
        x_rot: rot.x_rot,
        y_head_rot: rot.y_rot,
    };
    let id = ents.spawn_special(
        new,
        SpecialData::WitherSkull(Box::new(Skull {
            owner: Some(owner),
            owner_uuid,
            dangerous,
            acceleration_power: 0.1,
            left_owner: false,
            has_been_shot: false,
            tick_count: 0,
        })),
    );
    ents.launched(id, Some(owner));
    id
}

/// `Mth.nextDouble(random, min, max)`.
fn next_double_between(random: &mut LegacyRandom, min: f64, max: f64) -> f64 {
    if min >= max {
        min
    } else {
        random.next_double() * (max - min) + min
    }
}

/// `customServerAiStep`.
pub fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
    let invul = invulnerable_ticks(t);
    if invul > 0 {
        let new_count = invul - 1;
        set_bar_progress(t, 1.0 - new_count as f32 / 220.0);
        if new_count <= 0 {
            let p = t.pos();
            let eye = t.eye_y();
            t.cx.actions.push(WorldAction::Explode(ExplosionRequest {
                source: Some(t.id),
                indirect: None,
                pos: Vec3::new(p.x, eye, p.z),
                radius: 7.0,
                mob: true,
                resistance: BlockResistance::Default,
                fire: false,
            }));
            if !t.mob_ref().silent {
                global_spawn_event(t);
            }
        }
        if let Some(s) = state_mut(t.mob()) {
            s.invulnerable_ticks = new_count;
        }
        t.core_mut().mark_data_dirty();
        if t.mob_ref().tick_count % 10 == 0 {
            living::heal(t, 10.0);
        }
        return;
    }
    for i in 1..3usize {
        let tick_count = t.mob_ref().tick_count;
        let next = state(t.mob_ref()).map_or(0, |s| s.next_head_update[i - 1]);
        if tick_count < next {
            continue;
        }
        let delay = 10 + t.mob().random.next_int_bounded(10);
        let hard_enough = matches!(t.env.difficulty, Difficulty::Normal | Difficulty::Hard);
        let idle = {
            let Some(s) = state_mut(t.mob()) else {
                return;
            };
            s.next_head_update[i - 1] = tick_count + delay;
            let idle = s.idle_head_updates[i - 1];
            if hard_enough {
                s.idle_head_updates[i - 1] += 1;
            }
            idle
        };
        if hard_enough && idle > 15 {
            let p = t.pos();
            let r = &mut t.mob().random;
            let xt = next_double_between(r, p.x - 10.0, p.x + 10.0);
            let yt = next_double_between(r, p.y - 5.0, p.y + 5.0);
            let zt = next_double_between(r, p.z - 10.0, p.z + 10.0);
            perform_ranged_attack(t, i + 1, Vec3::new(xt, yt, zt), true);
            if let Some(s) = state_mut(t.mob()) {
                s.idle_head_updates[i - 1] = 0;
            }
        }
        let head_target = state(t.mob_ref()).and_then(|s| s.target(i));
        if let Some(current) = head_target {
            let ok = t.other(current).is_some()
                && can_attack(t.ents, t.env, t.id, current)
                && t.other(current)
                    .is_some_and(|o| t.pos().distance_to_sqr(o.pos) <= 900.0)
                && living::has_line_of_sight(t, current);
            if ok {
                perform_ranged_attack_at(t, i + 1, current);
                let delay = 40 + t.mob().random.next_int_bounded(20);
                let tick_count = t.mob_ref().tick_count;
                if let Some(s) = state_mut(t.mob()) {
                    s.next_head_update[i - 1] = tick_count + delay;
                    s.idle_head_updates[i - 1] = 0;
                }
            } else {
                set_alternative_target(t, i, None);
            }
        } else {
            let area = t.aabb().inflate(20.0, 8.0, 20.0);
            let candidates = living_in(t, &area);
            let mut entities = Vec::new();
            for id in candidates {
                if test_target(t, id, 20.0) {
                    entities.push(id);
                }
            }
            if !entities.is_empty() {
                let n = i32::try_from(entities.len()).unwrap_or(i32::MAX);
                let pick = t.mob().random.next_int_bounded(n);
                let selected = entities[usize::try_from(pick).unwrap_or(0)];
                set_alternative_target(t, i, Some(selected));
            }
        }
    }
    let target = t.mob_ref().target;
    set_alternative_target(t, 0, target);
    let destroy = state(t.mob_ref()).map_or(0, |s| s.destroy_blocks_tick);
    if destroy > 0 {
        let destroy = destroy - 1;
        if let Some(s) = state_mut(t.mob()) {
            s.destroy_blocks_tick = destroy;
        }
        if destroy == 0 && t.env.rules.bool(Rule::MOB_GRIEFING) {
            destroy_blocks(t);
        }
    }
    if t.mob_ref().tick_count % 20 == 0 {
        living::heal(t, 1.0);
    }
    let progress = {
        let m = t.mob_ref();
        m.health / m.max_health()
    };
    set_bar_progress(t, progress);
}

/// The block breaking after the wither was hurt (`destroyBlocksTick` reaching 0).
fn destroy_blocks(t: &mut Tick<'_, '_>) {
    let dims = t.core().dims();
    let width = (dims.width / 2.0 + 1.0).floor() as i32;
    let height = dims.height.floor() as i32;
    let b = t.block_pos();
    let mut destroyed = false;
    // `BlockPos.betweenClosed`: x fastest, then y, then z.
    for z in b.z - width..=b.z + width {
        for y in b.y..=b.y + height {
            for x in b.x - width..=b.x + width {
                let pos = BlockPos::new(x, y, z);
                let state = t.level.block(pos);
                if can_destroy(state) {
                    // `destroyBlock` changes nothing for a fluid's own block.
                    let fluid_block = !state.fluid_state().is_empty()
                        && (state.is(block_ids::WATER) || state.is(block_ids::LAVA));
                    destroyed |= !fluid_block;
                    t.cx.actions.push(WorldAction::DestroyBlock(pos, true));
                }
            }
        }
    }
    if destroyed {
        t.cx.actions
            .push(WorldAction::LevelEvent(event::WITHER_BREAK_BLOCK, b, 0));
    }
}

/// `setAlternativeTarget(head, id)`.
fn set_alternative_target(t: &mut Tick<'_, '_>, head: usize, target: Option<EntityId>) {
    let value = target.and_then(|id| t.ents.store.get(id).map(|e| (id, e.network_id().0)));
    let changed = match state_mut(t.mob()) {
        Some(s) if s.targets[head] != value => {
            s.targets[head] = value;
            true
        }
        _ => false,
    };
    if changed {
        t.core_mut().mark_data_dirty();
    }
}

/// `level.globalLevelEvent(1023, blockPosition(), 0)`: with `globalSoundEvents` every player
/// of the dimension hears it from at most 32 blocks away in its direction.
fn global_spawn_event(t: &mut Tick<'_, '_>) {
    static RULE: OnceLock<Option<Rule>> = OnceLock::new();
    let rule = *RULE.get_or_init(|| Rule::by_name("global_sound_events"));
    let pos = t.block_pos();
    if !rule.is_some_and(|r| t.env.rules.bool(r)) {
        t.cx.actions
            .push(WorldAction::LevelEvent(event::WITHER_SPAWNED, pos, 0));
        return;
    }
    let center = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    let players: Vec<(EntityId, Vec3)> = t
        .ents
        .players
        .iter()
        .filter_map(|(id, _)| t.ents.store.get(id).map(|e| (id, e.pos())))
        .collect();
    for (player, p) in players {
        let sound_pos = if p.distance_to_sqr(center) < 32.0 * 32.0 {
            center
        } else {
            p + (center - p).normalize() * 32.0
        };
        let packet = vanillite_protocol::SharedPacket::of(
            &vanillite_protocol::packets::play::clientbound::LevelEvent {
                event: event::WITHER_SPAWNED,
                pos: wire_pos(BlockPos::containing(sound_pos.x, sound_pos.y, sound_pos.z)),
                data: 0,
                global: true,
            },
        );
        t.env
            .out
            .push(crate::survival::Effect::To { player, packet });
    }
}

fn wire_pos(p: BlockPos) -> vanillite_protocol::types::BlockPos {
    vanillite_protocol::types::BlockPos {
        x: p.x,
        y: p.y,
        z: p.z,
    }
}

// --- Boss bar -------------------------------------------------------------------------------

fn new_bar(id: u128) -> BossBar {
    BossBar::new(
        id,
        Component::translatable(Kind::Wither.translation_key(), Vec::new()),
        BossBarColor::Purple,
        BossBarOverlay::Progress,
    )
    .with_flags(BossBarFlags {
        darken_screen: true,
        play_music: false,
        create_fog: false,
    })
}

/// `bossEvent.setProgress`.
fn set_bar_progress(t: &mut Tick<'_, '_>, progress: f32) {
    let id = t.id;
    let bar_id = state(t.mob_ref()).map_or(0, |s| s.bar_id);
    let bar = t.cx.boss_bars.entry(id).or_insert_with(|| new_bar(bar_id));
    bar.set_progress(progress, t.env.out);
}

/// `startSeenByPlayer` / `stopSeenByPlayer`, approximated by the players whose view holds the
/// wither's chunk within its tracking range.
fn sync_bar(t: &mut Tick<'_, '_>) {
    let id = t.id;
    let pos = t.pos();
    let chunk = t.block_pos().chunk();
    let range = f64::from(t.core().kind().info().tracking_range) * 16.0;
    let mut wanted = Vec::new();
    for (player, p) in t.ents.players.iter() {
        let Some(e) = t.ents.store.get(player) else {
            continue;
        };
        let viewer = vanillite_world::tracking::Viewer {
            entity: player,
            view_center: p.chunk_view.center,
            view_distance: p.chunk_view.view_distance,
            sent_radius: 0,
        };
        let r = range.min(f64::from(p.chunk_view.view_distance) * 16.0);
        let d = e.pos() - pos;
        if d.x * d.x + d.z * d.z <= r * r && viewer.sees_chunk(chunk) {
            wanted.push(player);
        }
    }
    let bar_id = state(t.mob_ref()).map_or(0, |s| s.bar_id);
    let invul = invulnerable_ticks(t);
    t.cx.boss_bars.entry(id).or_insert_with(|| {
        let mut bar = new_bar(bar_id);
        if invul > 0 {
            bar.set_progress(0.0, &mut Vec::new());
        }
        bar
    });
    // `setCustomName` / `readAdditionalSaveData`: the bar shows `getDisplayName`.
    let name = crate::mob::combat::mob_name(t.mob_ref());
    let bar = t.cx.boss_bars.get_mut(&id).expect("inserted above");
    bar.set_name(name, t.env.out);
    bar.sync_players(&wanted, t.env.out);
}

/// Removes the boss bars of entities that are gone (removed, died or unloaded) from their
/// players. Run once per tick after the entity tick.
pub fn sweep_boss_bars(
    cx: &mut MobContext,
    ents: &Entities,
    out: &mut Vec<crate::survival::Effect>,
) {
    if cx.boss_bars.is_empty() {
        return;
    }
    cx.boss_bars.retain(|&id, bar| {
        let alive = ents.mob(id).is_some_and(|m| !m.removed);
        if !alive {
            bar.remove_all_players(out);
        }
        alive
    });
}

// --- Damage and death -----------------------------------------------------------------------

/// `WitherBoss.hurtServer`'s own rules before `LivingEntity.hurtServer`.
pub fn filter_hurt(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: &DamageSource,
    amount: f32,
) -> Option<f32> {
    let _ = (level, cx);
    // `#wither_immune_to`.
    if source.kind == env.types.drown {
        return None;
    }
    if source
        .cause
        .is_some_and(|c| ents.mob(c).is_some_and(|m| m.kind == Kind::Wither))
    {
        return None;
    }
    let m = ents.mob(target)?;
    let s = state(m)?;
    if s.invulnerable_ticks > 0 && !env.types.is(source.kind, tag::BYPASSES_INVULNERABILITY) {
        return None;
    }
    if is_powered(m)
        && source
            .direct
            .is_some_and(|d| crate::mob::projectile::is_arrow_or_wind_charge(ents, d))
    {
        return None;
    }
    if let Some(cause) = source.cause
        && ents
            .store
            .get(cause)
            .is_some_and(|e| is_wither_friend(e.kind()))
    {
        return None;
    }
    let s = state_mut(ents.mob_mut(target)?)?;
    if s.destroy_blocks_tick <= 0 {
        s.destroy_blocks_tick = 20;
    }
    for idle in &mut s.idle_head_updates {
        *idle += 3;
    }
    Some(amount)
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

/// `dropCustomDeathLoot`: the nether star with an extended lifetime.
pub fn on_death(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
) {
    let _ = cx;
    if !env.rules.bool(Rule::MOB_DROPS) {
        return;
    }
    let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
        return;
    };
    let Some(item) = vanillite_data::registry::ITEM.id("minecraft:nether_star") else {
        return;
    };
    // `spawnAtLocation`: `new ItemEntity(level, x, y, z, stack)` draws from its own random.
    let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
    let mut data = crate::item_entity::ItemData::new(
        crate::item::ItemStack::new(item, 1),
        10,
        None,
        item_random,
    );
    // `setExtendedLifetime`.
    data.age = -6000;
    let id = ents.spawn_item(level.network_ids().next(), uuid, pos, velocity, y_rot, data);
    let parent = ents.store.get(target).map_or(0, |e| e.uuid());
    ents.spawned(id, vanillite_abi::spawn_reason::DEFAULT, parent);
}

/// `tickDeath`: the bar leaves the players when the wither is removed.
pub fn tick_death(t: &mut Tick<'_, '_>) -> bool {
    if t.mob_ref().death_time + 1 >= 20 {
        let id = t.id;
        if let Some(mut bar) = t.cx.boss_bars.remove(&id) {
            bar.remove_all_players(t.env.out);
        }
    }
    false
}

/// `entities/wither` has no entries; the nether star comes from [`on_death`].
pub fn death_loot(m: &mut MobData, killed_by_player: bool) -> Vec<crate::item::ItemStack> {
    let _ = (m, killed_by_player);
    Vec::new()
}

/// The synched data: `DATA_TARGET_A/B/C` (16–18) and `DATA_ID_INV` (19).
pub fn entity_data(out: &mut Vec<u8>, s: &State, all: bool) {
    for (i, t) in s.targets.iter().enumerate() {
        let id = 16 + u8::try_from(i).unwrap_or(0);
        data::int(out, id, t.map_or(0, |(_, n)| n), 0, all);
    }
    data::int(out, 19, s.invulnerable_ticks, 0, all);
}

pub fn save(s: &State, out: &mut Compound) {
    out.insert("Invul", Tag::Int(s.invulnerable_ticks));
}

pub fn load(s: &mut State, tag: &Compound) {
    s.invulnerable_ticks = tag
        .get("Invul")
        .and_then(Tag::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(0);
}

pub fn travel(t: &mut Tick<'_, '_>, input: Vec3) -> bool {
    let _ = (t, input);
    false
}

// --- Wither skulls --------------------------------------------------------------------------

/// The kind data of a `wither_skull` (`WitherSkull`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Skull {
    /// The shooter (`Projectile.owner`).
    pub owner: Option<EntityId>,
    /// The shooter's UUID as stored (`Owner`), when it was not resolved.
    pub owner_uuid: Option<u128>,
    /// `DATA_DANGEROUS`: the blue skull.
    pub dangerous: bool,
    /// `accelerationPower`.
    pub acceleration_power: f64,
    /// `leftOwner`.
    pub left_owner: bool,
    /// `hasBeenShot`: the `projectile_shoot` game event is posted.
    pub has_been_shot: bool,
    /// `tickCount`.
    pub tick_count: i32,
}

/// `ProjectileUtil.rotateTowardsMovement(this, 0.2F)`.
fn rotate_towards_movement(rot: &mut Rotation, v: Vec3) {
    let hd = (v.x * v.x + v.z * v.z).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let x_target = (atan2_deg(v.y, hd) as f32) - 90.0;
    #[allow(clippy::cast_possible_truncation)]
    let y_target = (atan2_deg(v.z, v.x) as f32) + 90.0;
    let lerp = |old: f32, new: f32| {
        let mut old = old;
        while new - old < -180.0 {
            old -= 360.0;
        }
        while new - old >= 180.0 {
            old += 360.0;
        }
        old + (new - old) * 0.2
    };
    rot.x_rot = lerp(rot.x_rot, x_target);
    rot.y_rot = lerp(rot.y_rot, y_target);
}

/// Whether a projectile can hit `other` (`Projectile.canHitEntity`): living, pickable entities.
fn can_hit(ents: &Entities, skull: &Skull, id: EntityId, other: EntityId) -> bool {
    if other == id {
        return false;
    }
    if !skull.left_owner && skull.owner == Some(other) {
        return false;
    }
    ents.mob(other).is_some_and(MobData::is_alive)
        || ents.player(other).is_some_and(|p| {
            !p.survival.is_dead_or_dying() && p.game_mode != vanillite_config::GameMode::Spectator
        })
        || ents
            .special(other)
            .is_some_and(|s| matches!(s, SpecialData::EndCrystal(_)))
}

/// Where the segment enters `b`, as a fraction (`AABB.clip`).
fn segment_box(from: Vec3, to: Vec3, b: &Aabb) -> Option<f64> {
    crate::mob::projectile::segment_box(from, to, b)
}

/// `WitherSkull.tick` (`AbstractHurtingProjectile.tick`).
pub fn tick_skull(t: &mut SpecialTick<'_, '_>) {
    let id = t.id;
    let Some(SpecialData::WitherSkull(skull)) = t.ents.special(id) else {
        return;
    };
    let mut skull = (**skull).clone();
    let Some(e) = t.ents.store.get(id) else {
        return;
    };
    let mut pos = e.pos();
    let mut velocity = e.velocity();
    let mut rot = e.rot();
    let aabb = e.aabb();
    skull.tick_count += 1;
    let block = BlockPos::containing(pos.x, pos.y, pos.z);
    // `applyInertia`.
    let in_water = crate::mob::is_water(t.level.block(block));
    let inertia = if in_water {
        0.8
    } else if skull.dangerous {
        f64::from(0.73_f32)
    } else {
        f64::from(0.95_f32)
    };
    velocity = (velocity + velocity.normalize() * skull.acceleration_power) * inertia;
    let owner_removed = skull
        .owner
        .is_some_and(|o| !t.ents.store.contains(o) || t.ents.mob(o).is_some_and(|m| m.removed));
    if owner_removed || !t.level.is_loaded(block) {
        t.cx.discarded.push(id);
        return;
    }
    // `checkLeftOwner`.
    if !skull.left_owner {
        let area = aabb
            .expand_towards(velocity.x, velocity.y, velocity.z)
            .inflate(1.0, 1.0, 1.0);
        let inside = skull.owner.is_some_and(|o| {
            t.ents
                .store
                .get(o)
                .is_some_and(|oe| oe.aabb().intersects(&area))
        });
        skull.left_owner = !inside;
    }
    // `ProjectileUtil.getHitResultOnMoveVector` with the collider clip.
    let to = pos + velocity;
    let block_hit = physics::clip(t.level as &dyn Blocks, pos, to);
    let end = block_hit.map_or(to, |(_, p)| p);
    #[allow(clippy::cast_precision_loss)]
    let margin = f64::from(((skull.tick_count - 2) as f32 / 20.0).clamp(0.0, 0.3));
    let area = aabb
        .expand_towards(velocity.x, velocity.y, velocity.z)
        .inflate(1.0, 1.0, 1.0);
    let mut best: Option<(EntityId, f64, Vec3)> = None;
    {
        let ents = &*t.ents;
        ents.store.for_each_in(&area, false, |other| {
            if !can_hit(ents, &skull, id, other) {
                return;
            }
            let Some(oe) = ents.store.get(other) else {
                return;
            };
            let grown = oe.aabb().inflate(margin, margin, margin);
            if let Some(f) = segment_box(pos, end, &grown) {
                let hit = pos + (end - pos) * f;
                let d = pos.distance_to_sqr(hit);
                if best.is_none_or(|(_, bd, _)| d < bd) {
                    best = Some((other, d, hit));
                }
            }
        });
    }
    let hit_entity = best.map(|(other, _, at)| (other, at));
    // Bukkit's `ProjectileHitEvent`: a cancelled hit is ignored, the skull flies on.
    let hit_event = |what| crate::mob::missile::hit_event(t.level, t.ents, id, skull.owner, what);
    let hit_entity =
        hit_entity.filter(|&(target, _)| hit_event(crate::mob::missile::HitWhat::Entity(target)));
    let block_hit = block_hit.filter(|&(block, at)| {
        hit_entity.is_some() || hit_event(crate::mob::missile::HitWhat::Block(block, at))
    });
    let new_pos = match (hit_entity, block_hit) {
        (Some((_, at)), _) => at,
        (None, Some((_, at))) => at,
        (None, None) => to,
    };
    rotate_towards_movement(&mut rot, velocity);
    pos = new_pos;
    if let Some(mut e) = t.ents.store.get_mut(id) {
        e.set_pos(pos);
        e.set_velocity(velocity);
        e.set_rot(rot);
    }
    // `Projectile.tick`: the first tick posts `PROJECTILE_SHOOT` from the shooter.
    if !std::mem::replace(&mut skull.has_been_shot, true) {
        crate::mob::missile::shoot_event(t.env.out, pos, skull.owner);
    }
    if let Some(SpecialData::WitherSkull(s)) = t.ents.special_mut(id) {
        **s = skull.clone();
    }
    let hit = hit_entity.is_some() || block_hit.is_some();
    if !hit {
        return;
    }
    if let Some((target, _)) = hit_entity {
        skull_hit_entity(t, &skull, target);
    }
    // `onHit`: the explosion, then the skull is gone.
    // `getIndirectSourceEntity`: the living owner.
    let indirect = skull
        .owner
        .filter(|&o| t.ents.mob(o).is_some() || t.ents.player(o).is_some());
    t.cx.actions.push(WorldAction::Explode(ExplosionRequest {
        source: Some(id),
        indirect,
        pos,
        radius: 1.0,
        mob: true,
        resistance: if skull.dangerous {
            BlockResistance::DangerousWitherSkull
        } else {
            BlockResistance::Default
        },
        fire: false,
    }));
    t.cx.discarded.push(id);
}

/// `WitherSkull.onHitEntity`.
fn skull_hit_entity(t: &mut SpecialTick<'_, '_>, skull: &Skull, target: EntityId) {
    let id = t.id;
    let owner = skull
        .owner
        .filter(|&o| t.ents.mob(o).is_some() || t.ents.player(o).is_some());
    let hurt = if let Some(owner) = owner {
        let source = combat::entity_source(t.ents, t.env.types.wither_skull, owner, id);
        let hurt = combat::hurt(t.level, t.ents, t.env, t.cx, target, source, 8.0);
        if hurt {
            let alive = t.ents.mob(target).is_some_and(MobData::is_alive)
                || t.ents
                    .player(target)
                    .is_some_and(|p| !p.survival.is_dead_or_dying());
            if alive {
                crate::mob::attack::post_attack_effects(
                    t.level, t.ents, t.env, t.cx, target, &source,
                );
            } else if let Some(m) = t.ents.mob_mut(owner) {
                // `livingOwner.heal(5.0F)`.
                if m.health > 0.0 {
                    let max = m.max_health();
                    m.health = (m.health + 5.0).clamp(0.0, max);
                    if let Some(mut e) = t.ents.store.get_mut(owner) {
                        e.mark_data_dirty();
                    }
                }
            }
        }
        hurt
    } else {
        let mut source = DamageSource::of(t.env.types.magic);
        source.direct = Some(id);
        combat::hurt(t.level, t.ents, t.env, t.cx, target, source, 5.0)
    };
    let living = t.ents.mob(target).is_some() || t.ents.player(target).is_some();
    if hurt && living {
        let seconds = wither_seconds(t.env.difficulty);
        if seconds > 0 {
            let effect = EffectInstance::new(MobEffect::WITHER, 20 * seconds, 1);
            // `getEffectSource()`: the owner, else the skull.
            let by = skull
                .owner
                .filter(|&o| t.ents.store.contains(o))
                .or(Some(id));
            crate::effect::living::add_detached_from(t.ents, target, effect, by);
        }
    }
}

/// The wither effect of a skull hit by difficulty.
pub fn wither_seconds(difficulty: Difficulty) -> i32 {
    match difficulty {
        Difficulty::Normal => 10,
        Difficulty::Hard => 40,
        _ => 0,
    }
}

/// `WitherSkull.getBlockExplosionResistance`: dangerous skulls blow through blocks the wither
/// can destroy as if their resistance were at most 0.8.
pub fn skull_block_resistance(dangerous: bool, state: BlockState, resistance: f32) -> f32 {
    if dangerous && can_destroy(state) {
        resistance.min(0.8)
    } else {
        resistance
    }
}

/// The synched data of a wither skull: `DATA_DANGEROUS` (8).
pub fn skull_entity_data(out: &mut Vec<u8>, s: &Skull, all: bool) {
    data::boolean(out, 8, s.dangerous, false, all);
}

/// The skull's NBT (`Projectile` and `AbstractHurtingProjectile` fields and `dangerous`).
pub fn save_skull(s: &Skull, out: &mut Compound) {
    if let Some(uuid) = s.owner_uuid {
        let ints = vec![
            (uuid >> 96) as u32 as i32,
            (uuid >> 64) as u32 as i32,
            (uuid >> 32) as u32 as i32,
            uuid as u32 as i32,
        ];
        out.insert("Owner", Tag::IntArray(ints));
    }
    out.insert("LeftOwner", s.left_owner);
    out.insert("HasBeenShot", s.has_been_shot);
    out.insert("acceleration_power", s.acceleration_power);
    out.insert("dangerous", s.dangerous);
}

/// Reads a skull's NBT.
pub fn load_skull(tag: &Compound) -> Skull {
    let owner_uuid = match tag.get("Owner") {
        Some(Tag::IntArray(a)) if a.len() == 4 => Some(
            a.iter()
                .fold(0u128, |acc, &v| (acc << 32) | u128::from(v as u32)),
        ),
        _ => None,
    };
    Skull {
        owner: None,
        owner_uuid,
        dangerous: tag
            .get("dangerous")
            .and_then(Tag::as_i64)
            .is_some_and(|v| v != 0),
        acceleration_power: match tag.get("acceleration_power") {
            Some(Tag::Double(d)) if d.is_finite() => *d,
            _ => 0.1,
        },
        left_owner: tag
            .get("LeftOwner")
            .and_then(Tag::as_i64)
            .is_some_and(|v| v != 0),
        has_been_shot: tag
            .get("HasBeenShot")
            .and_then(Tag::as_i64)
            .is_some_and(|v| v != 0),
        tick_count: 0,
    }
}

#[cfg(test)]
mod tests;
