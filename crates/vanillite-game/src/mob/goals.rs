//! Mob goals (`GoalSelector`, `WrappedGoal` and the goals of the common mobs).
//!
//! A goal is a small `Copy` value; the selector copies it out of its slot to run it with full
//! access to the world and writes it back, so goals can hurt other entities without aliasing
//! the mob's own data.

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_world::entity::EntityId;

use super::kinds::Kind;
use super::living::{self, has_line_of_sight};
use super::path::{PathType, path_type_static};
use super::{Tick, combat, kinds, nav, projectile};
use crate::survival::damage::tag;
use crate::survival::rules::{Difficulty, Rule};

pub mod plugin;

/// `Goal.Flag` bits.
pub mod flag {
    pub const MOVE: u8 = 1;
    pub const LOOK: u8 = 2;
    pub const JUMP: u8 = 4;
    pub const TARGET: u8 = 8;
}

/// `reducedTickDelay`.
pub fn reduced(ticks: i32) -> i32 {
    (ticks + 1) / 2
}

/// The shared state of target goals (`TargetGoal`).
#[derive(Clone, Copy, Debug)]
pub struct TargetState {
    pub must_see: bool,
    pub unseen_ticks: i32,
    pub unseen_memory: i32,
    pub target_mob: Option<EntityId>,
}

impl TargetState {
    /// The state of a new target goal.
    pub fn new(must_see: bool) -> TargetState {
        TargetState {
            must_see,
            unseen_ticks: 0,
            unseen_memory: 60,
            target_mob: None,
        }
    }
}

/// Which melee goal (`MeleeAttackGoal` and its subclasses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeleeKind {
    Plain,
    Zombie,
    Spider,
}

/// A goal with its state.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    Float,
    Panic {
        speed: f64,
        pos: Vec3,
    },
    Stroll {
        speed: f64,
        interval: i32,
        probability: f32,
        wanted: Vec3,
    },
    LookAtPlayer {
        distance: f32,
        probability: f32,
        look_at: Option<EntityId>,
        time: i32,
    },
    RandomLook {
        rel_x: f64,
        rel_z: f64,
        time: i32,
    },
    Melee {
        kind: MeleeKind,
        speed: f64,
        follow_unseen: bool,
        pathed: Vec3,
        recalc: i32,
        attack_in: i32,
        last_can_use: i64,
        raise_arm: i32,
    },
    Swell,
    Leap {
        yd: f32,
        target: Option<EntityId>,
    },
    FleeSun {
        speed: f64,
        wanted: Vec3,
    },
    RestrictSun,
    Bow {
        speed: f64,
        interval_min: i32,
        radius_sqr: f32,
        attack_time: i32,
        see_time: i32,
        clockwise: bool,
        backwards: bool,
        strafing_time: i32,
    },
    EatBlock {
        tick: i32,
    },
    FollowParent {
        speed: f64,
        parent: Option<EntityId>,
        recalc: i32,
    },
    NearestTarget {
        state: TargetState,
        interval: i32,
        found: Option<EntityId>,
        /// `SpiderTargetGoal`: only in the dark.
        spider: bool,
    },
    HurtBy {
        state: TargetState,
        timestamp: i32,
        alert: bool,
    },
    EndermanFreeze {
        target: Option<EntityId>,
    },
    EndermanStare {
        state: TargetState,
        pending: Option<EntityId>,
        aggro_time: i32,
        teleport_time: i32,
    },
    /// `BreedGoal`.
    Breed {
        speed: f64,
        partner: Option<EntityId>,
        love_time: i32,
    },
    /// `TemptGoal` with the items that tempt.
    Tempt {
        speed: f64,
        items: TemptItems,
        player: Option<EntityId>,
        calm_down: i32,
    },
    /// `SitWhenOrderedToGoal`.
    SitWhenOrdered,
    /// `EndermanLeaveBlockGoal`.
    EndermanLeaveBlock,
    /// `EndermanTakeBlockGoal`.
    EndermanTakeBlock,
    /// `FollowOwnerGoal`.
    FollowOwner {
        speed: f64,
        start_distance: f32,
        stop_distance: f32,
        recalc: i32,
    },
    /// `OwnerHurtByTargetGoal`: attacks what hurt its owner.
    OwnerHurtBy {
        state: TargetState,
        timestamp: i32,
        found: Option<EntityId>,
    },
    /// `OwnerHurtTargetGoal`: attacks what its owner hurt.
    OwnerHurt {
        state: TargetState,
        timestamp: i32,
        found: Option<EntityId>,
    },
    /// A goal of a mob family (`mob/fam`).
    Fam(super::fam::FamGoal),
    /// A goal many families share (`mob/common.rs`).
    Common(super::common::CommonGoal),
    /// The goals of guardians (`super::boss::guardian`).
    Guardian(super::boss::guardian::GuardianGoal),
    /// The wither's own goals.
    Wither(super::boss::wither::WitherGoal),
}

/// The items of a [`Goal::Tempt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemptItems {
    /// The animal's breeding food (`isFood`).
    Food,
    CarrotOnAStick,
    /// The items of an item tag (`minecraft:horse_tempt_items`).
    Tag(&'static str),
}

impl Goal {
    /// The control flags the goal locks.
    pub fn flags(&self) -> u8 {
        use flag::*;
        match self {
            Goal::Float => JUMP,
            Goal::Panic { .. } | Goal::Stroll { .. } | Goal::FleeSun { .. } | Goal::Swell => MOVE,
            Goal::LookAtPlayer { .. } => LOOK,
            Goal::RandomLook { .. } | Goal::Melee { .. } | Goal::Bow { .. } => MOVE | LOOK,
            Goal::Leap { .. } => JUMP | MOVE,
            Goal::EatBlock { .. } => MOVE | LOOK | JUMP,
            Goal::RestrictSun | Goal::FollowParent { .. } => 0,
            Goal::NearestTarget { .. } | Goal::HurtBy { .. } | Goal::EndermanStare { .. } => TARGET,
            Goal::EndermanFreeze { .. } => JUMP | MOVE,
            Goal::Breed { .. } | Goal::Tempt { .. } | Goal::FollowOwner { .. } => MOVE | LOOK,
            Goal::SitWhenOrdered => JUMP | MOVE,
            Goal::EndermanLeaveBlock | Goal::EndermanTakeBlock => 0,
            Goal::OwnerHurtBy { .. } | Goal::OwnerHurt { .. } => TARGET,
            Goal::Fam(g) => super::fam::goal_flags(g),
            Goal::Common(g) => g.flags(),
            Goal::Guardian(g) => g.flags(),
            Goal::Wither(g) => g.flags(),
        }
    }

    /// `requiresUpdateEveryTick`.
    pub fn every_tick(&self) -> bool {
        match self {
            Goal::Fam(g) => super::fam::goal_every_tick(g),
            Goal::Common(g) => g.every_tick(),
            Goal::Guardian(g) => g.every_tick(),
            Goal::Wither(g) => g.every_tick(),
            _ => matches!(
                self,
                Goal::Float
                    | Goal::RandomLook { .. }
                    | Goal::Melee { .. }
                    | Goal::Swell
                    | Goal::Bow { .. }
            ),
        }
    }

    /// `isInterruptable`.
    pub fn interruptable(&self) -> bool {
        match self {
            Goal::Common(g) => g.interruptable(),
            Goal::Fam(crate::mob::fam::FamGoal::Wildlife(g)) => {
                crate::mob::fam::wildlife::goal_interruptable(g)
            }
            _ => true,
        }
    }

    /// `adjustedTickDelay`.
    fn adjusted(&self, ticks: i32) -> i32 {
        if self.every_tick() {
            ticks
        } else {
            reduced(ticks)
        }
    }

    // Constructors with Vanilla's defaults.

    pub fn panic(speed: f64) -> Goal {
        Goal::Panic {
            speed,
            pos: Vec3::ZERO,
        }
    }

    /// `WaterAvoidingRandomStrollGoal(mob, speed)`.
    pub fn stroll(speed: f64) -> Goal {
        Goal::Stroll {
            speed,
            interval: 120,
            probability: 0.001,
            wanted: Vec3::ZERO,
        }
    }

    /// `WaterAvoidingRandomStrollGoal(mob, speed, probability)`.
    pub fn stroll_with(speed: f64, probability: f32) -> Goal {
        Goal::Stroll {
            speed,
            interval: 120,
            probability,
            wanted: Vec3::ZERO,
        }
    }

    pub fn look_at_player(distance: f32) -> Goal {
        Goal::LookAtPlayer {
            distance,
            probability: 0.02,
            look_at: None,
            time: 0,
        }
    }

    pub fn random_look() -> Goal {
        Goal::RandomLook {
            rel_x: 0.0,
            rel_z: 0.0,
            time: 0,
        }
    }

    pub fn melee(kind: MeleeKind, speed: f64, follow_unseen: bool) -> Goal {
        Goal::Melee {
            kind,
            speed,
            follow_unseen,
            pathed: Vec3::ZERO,
            recalc: 0,
            attack_in: 0,
            // `lastCanUseCheck` starts at 0: no attack in a world's first 20 ticks.
            last_can_use: 0,
            raise_arm: 0,
        }
    }

    pub fn bow(speed: f64, interval_min: i32, radius: f32) -> Goal {
        Goal::Bow {
            speed,
            interval_min,
            radius_sqr: radius * radius,
            attack_time: -1,
            see_time: 0,
            clockwise: false,
            backwards: false,
            strafing_time: -1,
        }
    }

    pub fn nearest_player(must_see: bool) -> Goal {
        Goal::NearestTarget {
            state: TargetState::new(must_see),
            interval: reduced(10),
            found: None,
            spider: false,
        }
    }

    pub fn spider_target() -> Goal {
        Goal::NearestTarget {
            state: TargetState::new(true),
            interval: reduced(10),
            found: None,
            spider: true,
        }
    }

    pub fn breed(speed: f64) -> Goal {
        Goal::Breed {
            speed,
            partner: None,
            love_time: 0,
        }
    }

    pub fn tempt(speed: f64, items: TemptItems) -> Goal {
        Goal::Tempt {
            speed,
            items,
            player: None,
            calm_down: 0,
        }
    }

    /// `FollowOwnerGoal(mob, speed, startDistance, stopDistance)`.
    pub fn follow_owner(speed: f64, start_distance: f32, stop_distance: f32) -> Goal {
        Goal::FollowOwner {
            speed,
            start_distance,
            stop_distance,
            recalc: 0,
        }
    }

    pub fn owner_hurt_by() -> Goal {
        Goal::OwnerHurtBy {
            state: TargetState::new(false),
            timestamp: 0,
            found: None,
        }
    }

    pub fn owner_hurt() -> Goal {
        Goal::OwnerHurt {
            state: TargetState::new(false),
            timestamp: 0,
            found: None,
        }
    }

    pub fn hurt_by(alert: bool) -> Goal {
        Goal::HurtBy {
            state: TargetState::new(true),
            timestamp: 0,
            alert,
        }
    }
}

/// `WrappedGoal`: a goal with its priority and whether it runs.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub priority: u8,
    pub running: bool,
    pub goal: Goal,
}

const NO_SLOT: u8 = u8::MAX;

/// `GoalSelector`.
#[derive(Clone, Debug)]
pub struct GoalSelector {
    pub slots: Vec<Slot>,
    locked: [u8; 4],
    disabled: u8,
}

impl Default for GoalSelector {
    fn default() -> Self {
        GoalSelector {
            slots: Vec::new(),
            locked: [NO_SLOT; 4],
            disabled: 0,
        }
    }
}

impl GoalSelector {
    /// `setControlFlag(flag, enabled)` for every flag in `flags`.
    pub fn set_control_flags(&mut self, flags: u8, enabled: bool) {
        if enabled {
            self.disabled &= !flags;
        } else {
            self.disabled |= flags;
        }
    }

    /// `addGoal`.
    pub fn add(&mut self, priority: u8, goal: Goal) {
        self.slots.push(Slot {
            priority,
            running: false,
            goal,
        });
    }
}

/// `GoalSelector.removeAllGoals(g -> true)`: stops the running goals and removes them all.
pub fn remove_all_goals(t: &mut Tick<'_, '_>) {
    for i in 0..t.mob_ref().goals.slots.len() {
        stop_slot(t, Which::Goals, i);
    }
    let g = &mut t.mob().goals;
    g.slots.clear();
    g.locked = [NO_SLOT; 4];
}

/// Which selector of a mob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Goals,
    Targets,
}

fn selector<'a>(t: &'a mut Tick<'_, '_>, which: Which) -> &'a mut GoalSelector {
    let m = t.mob();
    match which {
        Which::Goals => &mut m.goals,
        Which::Targets => &mut m.targets,
    }
}

fn selector_ref<'a>(t: &'a Tick<'_, '_>, which: Which) -> &'a GoalSelector {
    let m = t.mob_ref();
    match which {
        Which::Goals => &m.goals,
        Which::Targets => &m.targets,
    }
}

/// Runs `f` on the goal of slot `i` (copied out and written back).
fn with_goal<R>(
    t: &mut Tick<'_, '_>,
    which: Which,
    i: usize,
    f: impl FnOnce(&mut Tick<'_, '_>, &mut Goal) -> R,
) -> Option<R> {
    let mut goal = selector_ref(t, which).slots.get(i)?.goal;
    let r = f(t, &mut goal);
    if t.exists()
        && let Some(slot) = selector(t, which).slots.get_mut(i)
    {
        slot.goal = goal;
    }
    Some(r)
}

fn stop_slot(t: &mut Tick<'_, '_>, which: Which, i: usize) {
    let running = selector_ref(t, which)
        .slots
        .get(i)
        .is_some_and(|s| s.running);
    if !running {
        return;
    }
    if let Some(s) = selector(t, which).slots.get_mut(i) {
        s.running = false;
    }
    with_goal(t, which, i, stop);
}

fn start_slot(t: &mut Tick<'_, '_>, which: Which, i: usize) {
    let running = selector_ref(t, which)
        .slots
        .get(i)
        .is_some_and(|s| s.running);
    if running {
        return;
    }
    if let Some(s) = selector(t, which).slots.get_mut(i) {
        s.running = true;
    }
    with_goal(t, which, i, start);
}

/// One goal of a selector as the selector sees it: the Vanilla slots first, then the mob's
/// plugin goals ([`plugin`]), as Paper appends custom goals to `availableGoals`.
#[derive(Clone, Copy, Debug)]
struct Unit {
    running: bool,
    flags: u8,
    priority: i32,
    interruptable: bool,
    every_tick: bool,
}

/// Goal `i` of a selector whose first `n` goals are its Vanilla slots.
fn unit(t: &Tick<'_, '_>, which: Which, n: usize, i: usize) -> Option<Unit> {
    if i >= n {
        return plugin::unit(t, which, i - n);
    }
    let slot = selector_ref(t, which).slots.get(i)?;
    Some(Unit {
        running: slot.running,
        flags: slot.goal.flags(),
        priority: i32::from(slot.priority),
        interruptable: slot.goal.interruptable(),
        every_tick: slot.goal.every_tick(),
    })
}

fn unit_can_use(t: &mut Tick<'_, '_>, which: Which, n: usize, i: usize) -> bool {
    if i >= n {
        return plugin::can_use(t, which, i - n);
    }
    with_goal(t, which, i, can_use).unwrap_or(false)
}

fn unit_can_continue(t: &mut Tick<'_, '_>, which: Which, n: usize, i: usize) -> bool {
    if i >= n {
        return plugin::can_continue(t, which, i - n);
    }
    with_goal(t, which, i, can_continue).unwrap_or(false)
}

fn start_unit(t: &mut Tick<'_, '_>, which: Which, n: usize, i: usize) {
    if i >= n {
        plugin::start(t, which, i - n);
    } else {
        start_slot(t, which, i);
    }
}

fn stop_unit(t: &mut Tick<'_, '_>, which: Which, n: usize, i: usize) {
    if i >= n {
        plugin::stop(t, which, i - n);
    } else {
        stop_slot(t, which, i);
    }
}

fn tick_unit(t: &mut Tick<'_, '_>, which: Which, n: usize, i: usize) {
    if i >= n {
        plugin::tick(t, which, i - n);
    } else {
        with_goal(t, which, i, tick);
    }
}

/// `GoalSelector.removeAllGoals` of the Vanilla goals a plugin cleared (`mob_goals_clear`):
/// the running ones stop, the locks of the plugin goals move with them.
fn clear_vanilla(t: &mut Tick<'_, '_>, which: Which) {
    if !plugin::take_clear(t, which) {
        return;
    }
    let n = selector_ref(t, which).slots.len();
    for i in 0..n {
        if !t.exists() {
            return;
        }
        stop_slot(t, which, i);
    }
    if !t.exists() {
        return;
    }
    let s = selector(t, which);
    s.slots.clear();
    for l in &mut s.locked {
        if *l != NO_SLOT {
            let i = usize::from(*l);
            *l = if i < n {
                NO_SLOT
            } else {
                u8::try_from(i - n).unwrap_or(NO_SLOT)
            };
        }
    }
}

/// `GoalSelector.tick`.
pub fn tick_selector(t: &mut Tick<'_, '_>, which: Which) {
    clear_vanilla(t, which);
    let n = selector_ref(t, which).slots.len();
    let total = n + plugin::count(t, which);
    for i in 0..total {
        if !t.exists() {
            return;
        }
        let disabled = selector_ref(t, which).disabled;
        let Some(u) = unit(t, which, n, i) else {
            continue;
        };
        if u.running {
            let stop_it = u.flags & disabled != 0 || !unit_can_continue(t, which, n, i);
            if stop_it {
                stop_unit(t, which, n, i);
            }
        }
    }
    if !t.exists() {
        return;
    }
    for b in 0..4 {
        let l = selector_ref(t, which).locked[b];
        if l != NO_SLOT && !unit(t, which, n, usize::from(l)).is_some_and(|x| x.running) {
            selector(t, which).locked[b] = NO_SLOT;
        }
    }
    for i in 0..total {
        if !t.exists() {
            return;
        }
        let Some(u) = unit(t, which, n, i) else {
            continue;
        };
        let (disabled, replaceable) = {
            let view: &Tick<'_, '_> = t;
            let s = selector_ref(view, which);
            let replaceable = (0..4).all(|b| {
                if u.flags & (1 << b) == 0 {
                    return true;
                }
                let l = s.locked[b];
                // `canBeReplacedBy`: interruptable and a lower priority.
                l == NO_SLOT
                    || unit(view, which, n, usize::from(l))
                        .is_none_or(|o| o.interruptable && u.priority < o.priority)
            });
            (s.disabled, replaceable)
        };
        if u.running || u.flags & disabled != 0 || !replaceable {
            continue;
        }
        if !unit_can_use(t, which, n, i) {
            continue;
        }
        for b in 0..4 {
            if u.flags & (1 << b) == 0 {
                continue;
            }
            let current = selector_ref(t, which).locked[b];
            if current != NO_SLOT {
                stop_unit(t, which, n, usize::from(current));
            }
            selector(t, which).locked[b] = u8::try_from(i).unwrap_or(NO_SLOT);
        }
        start_unit(t, which, n, i);
    }
    tick_running(t, which, true);
}

/// `GoalSelector.tickRunningGoals`.
pub fn tick_running(t: &mut Tick<'_, '_>, which: Which, force: bool) {
    let n = selector_ref(t, which).slots.len();
    let total = n + plugin::count(t, which);
    for i in 0..total {
        if !t.exists() || t.mob_ref().removed {
            return;
        }
        let Some(u) = unit(t, which, n, i) else {
            return;
        };
        if u.running && (force || u.every_tick) {
            tick_unit(t, which, n, i);
        }
    }
}

// --- Helpers ----------------------------------------------------------------------------

fn random_float(t: &mut Tick<'_, '_>) -> f32 {
    t.mob().random.next_float()
}

fn random_int(t: &mut Tick<'_, '_>, bound: i32) -> i32 {
    t.mob().random.next_int_bounded(bound.max(1))
}

pub(super) fn distance_sqr(t: &Tick<'_, '_>, other: Vec3) -> f64 {
    t.pos().distance_to_sqr(other)
}

/// `LivingEntity.canAttack(target)` and the combat part of `TargetingConditions.test`.
pub(crate) fn can_attack(t: &Tick<'_, '_>, target: EntityId) -> bool {
    // `canBeSeenAsEnemy`: invulnerable entities are no targets.
    if target == t.id || crate::mob::entity_nbt::is_invulnerable(t.ents, target) {
        return false;
    }
    let me = t.mob_ref();
    // `Dolphin.canAttack`: babies never attack.
    if me.kind == Kind::Dolphin && me.baby {
        return false;
    }
    // `TamableAnimal.canAttack`: never the owner.
    if me.tame
        && me.owner.is_some()
        && (crate::mob::fam::companion::is_tamable(me.kind)
            || matches!(me.kind, Kind::Nautilus | Kind::ZombieNautilus))
        && t.ents.store.get(target).map(|e| e.uuid()) == me.owner
    {
        return false;
    }
    // `IronGolem.canAttack`: no creepers, no players when it was built by one.
    if me.kind == Kind::IronGolem {
        if t.ents.mob(target).is_some_and(|o| o.kind == Kind::Creeper) {
            return false;
        }
        if t.ents.player(target).is_some() && crate::mob::fam::wildlife::st(me).player_created {
            return false;
        }
    }
    // `AbstractIllager.canAttack`: never a baby villager.
    if matches!(
        me.kind,
        Kind::Pillager | Kind::Vindicator | Kind::Evoker | Kind::Illusioner
    ) && t
        .ents
        .mob(target)
        .is_some_and(|o| o.kind == Kind::Villager && o.baby)
    {
        return false;
    }
    // `Mob.canAttack`: never a ghast.
    if t.ents.mob(target).is_some_and(|o| o.kind == Kind::Ghast) {
        return false;
    }
    if let Some(p) = t.ents.player(target) {
        if t.env.difficulty == Difficulty::Peaceful {
            return false;
        }
        return !p.survival.is_dead_or_dying()
            && matches!(p.game_mode, GameMode::Survival | GameMode::Adventure);
    }
    t.ents
        .mob(target)
        .is_some_and(|m| m.is_alive() && crate::decoration::stand::attackable(m))
}

/// `TargetingConditions.test` for a player target.
pub(crate) fn test_player(
    t: &mut Tick<'_, '_>,
    target: EntityId,
    combat: bool,
    range: f64,
    line_of_sight: bool,
) -> bool {
    let Some(p) = t.ents.player(target) else {
        return false;
    };
    if p.survival.is_dead_or_dying() || p.game_mode == GameMode::Spectator {
        return false;
    }
    if combat && !can_attack(t, target) {
        return false;
    }
    let Some(e) = t.ents.store.get(target) else {
        return false;
    };
    if range > 0.0 {
        // `getVisibilityPercent`: sneaking, Invisibility with the armour cover, mob heads.
        let target_pos = e.pos();
        let modifier = super::brain::sensing::visibility_percent(t, target);
        let visible = (range * modifier).max(2.0);
        if t.pos().distance_to_sqr(target_pos) > visible * visible {
            return false;
        }
    }
    !line_of_sight || has_line_of_sight(t, target)
}

/// `getNearestPlayer(conditions, mob, x, eyeY, z)` over the players within `range`.
pub(crate) fn nearest_player(
    t: &mut Tick<'_, '_>,
    combat: bool,
    range: f64,
    line_of_sight: bool,
) -> Option<EntityId> {
    let p = t.pos();
    let at = Vec3::new(p.x, t.eye_y(), p.z);
    let r = range.max(2.0);
    let area = Aabb::new(p.x - r, p.y - r, p.z - r, p.x + r, p.y + r, p.z + r);
    let mut candidates = std::mem::take(&mut t.cx.ids);
    candidates.clear();
    t.ents
        .store
        .for_each_in(&area, true, |id| candidates.push(id));
    let mut best: Option<(EntityId, f64)> = None;
    for &id in &candidates {
        if !test_player(t, id, combat, range, line_of_sight) {
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
    t.cx.ids = candidates;
    best.map(|(id, _)| id)
}

/// `TemptGoal.canUse`: the nearest player (not a spectator) within the tempt range holding a
/// tempting item in either hand.
pub(crate) fn tempting_player(t: &mut Tick<'_, '_>, items: TemptItems) -> Option<EntityId> {
    // `minecraft:tempt_range` defaults to 10.
    let range = 10.0;
    let p = t.pos();
    let at = Vec3::new(p.x, t.eye_y(), p.z);
    let area = Aabb::new(
        p.x - range,
        p.y - range,
        p.z - range,
        p.x + range,
        p.y + range,
        p.z + range,
    );
    let mut candidates = std::mem::take(&mut t.cx.ids);
    candidates.clear();
    t.ents
        .store
        .for_each_in(&area, true, |id| candidates.push(id));
    let kind = t.mob_ref().kind;
    let mut best: Option<(EntityId, f64)> = None;
    for &id in &candidates {
        let Some(pd) = t.ents.player(id) else {
            continue;
        };
        if pd.survival.is_dead_or_dying() || pd.game_mode == GameMode::Spectator {
            continue;
        }
        let holds = [
            crate::item::EquipmentSlot::MainHand,
            crate::item::EquipmentSlot::OffHand,
        ]
        .iter()
        .any(|&s| {
            let stack = pd.inventory.equipment(s);
            !stack.is_empty()
                && match items {
                    TemptItems::Food => super::animal::is_food(kind, stack.item),
                    TemptItems::CarrotOnAStick => {
                        stack.item == vanillite_data::item::ids::CARROT_ON_A_STICK.0
                    }
                    TemptItems::Tag(tag) => crate::player::play::game_data()
                        .is_some_and(|d| d.item_in_tag(stack.item, tag)),
                }
        });
        if !holds {
            continue;
        }
        let Some(e) = t.ents.store.get(id) else {
            continue;
        };
        let d = e.pos().distance_to_sqr(at);
        if d > range * range {
            continue;
        }
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((id, d));
        }
    }
    t.cx.ids = candidates;
    best.map(|(id, _)| id)
}

fn other_pos(t: &Tick<'_, '_>, id: EntityId) -> Option<Vec3> {
    t.other(id).map(|o| o.pos)
}

/// `Mob.isWithinMeleeAttackRange`.
pub(super) fn within_melee_range(t: &Tick<'_, '_>, target: EntityId) -> bool {
    let Some(o) = t.other(target) else {
        return false;
    };
    let reach = f64::from(2.04_f32).sqrt() - f64::from(0.6_f32);
    let mut b = attack_bounding_box(t).inflate(reach, 0.0, reach);
    // `Ravager.getAttackBoundingBox`.
    if t.mob_ref().kind == Kind::Ravager {
        b = b.inflate(-0.05, 0.0, -0.05);
    }
    b.intersects(&o.aabb)
}

/// `Mob.getAttackBoundingBox`: a rider reaches as far as its mount does sideways.
pub(super) fn attack_bounding_box(t: &Tick<'_, '_>) -> Aabb {
    let own = t.aabb();
    let Some(mount) = t
        .ents
        .vehicle_of(t.id)
        .and_then(|v| t.ents.store.get(v))
        .map(|e| e.aabb())
    else {
        return own;
    };
    Aabb::new(
        own.min_x.min(mount.min_x),
        own.min_y,
        own.min_z.min(mount.min_z),
        own.max_x.max(mount.max_x),
        own.max_y,
        own.max_z.max(mount.max_z),
    )
}

fn is_creative_or_spectator(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .player(id)
        .is_some_and(|p| matches!(p.game_mode, GameMode::Creative | GameMode::Spectator))
}

/// `getWalkTargetValue(pos)`.
fn walk_target_value(t: &Tick<'_, '_>, pos: BlockPos) -> f64 {
    kinds::walk_target_value(t.mob_ref().kind, t.level, &t.cx.env, pos)
}

/// `DefaultRandomPos.getPos(mob, h, v)`.
fn default_random_pos(t: &mut Tick<'_, '_>, h: i32, v: i32) -> Option<Vec3> {
    super::randompos::default_pos(t, h, v)
}

/// `LandRandomPos.getPos(mob, h, v)`.
pub(crate) fn land_random_pos(t: &mut Tick<'_, '_>, h: i32, v: i32) -> Option<Vec3> {
    super::randompos::land_pos(t, h, v)
}

fn bottom_center(p: BlockPos) -> Vec3 {
    Vec3::new(f64::from(p.x) + 0.5, f64::from(p.y), f64::from(p.z) + 0.5)
}

pub(super) fn set_aggressive(t: &mut Tick<'_, '_>, on: bool) {
    if t.mob_ref().aggressive != on {
        t.mob().aggressive = on;
        t.core_mut().mark_data_dirty();
    }
}

/// `Mob.setTarget` without a known reason (forgetting it: `FORGET`).
pub(crate) fn set_target(t: &mut Tick<'_, '_>, target: Option<EntityId>) {
    let reason = if target.is_none() {
        vanillite_abi::target_reason::FORGET
    } else {
        vanillite_abi::target_reason::UNKNOWN
    };
    set_target_why(t, target, reason);
}

/// `Mob.setTarget` for a reason of `vanillite_abi::target_reason`; a plugin may cancel the
/// change (the mob keeps its target). Returns whether the target is `target` now.
pub(crate) fn set_target_why(t: &mut Tick<'_, '_>, target: Option<EntityId>, reason: u32) -> bool {
    let old = t.mob_ref().target;
    if !target_event(t.ents, t.id, old, target, reason) {
        return false;
    }
    assign_target(t.mob(), target);
    if t.mob_ref().kind == Kind::Enderman {
        enderman_set_target(t, target);
    }
    true
}

/// `EnderMan.setTarget`: a target makes it creepy and faster (+0.15) and remembers the time;
/// none takes both back.
pub(crate) fn enderman_set_target(t: &mut Tick<'_, '_>, target: Option<EntityId>) {
    let tick = t.mob_ref().tick_count;
    let m = t.mob();
    let had = super::fam::classic::enderman(m).attack_speed;
    let e = super::fam::classic::enderman_mut(m);
    e.target_change_time = if target.is_some() { tick } else { 0 };
    e.attack_speed = target.is_some();
    match (target.is_some(), had) {
        (true, false) => m.attrs.movement_speed += 0.15,
        (false, true) => m.attrs.movement_speed -= 0.15,
        _ => {}
    }
    kinds::set_creepy(t, target.is_some());
}

/// The plugins' target event (`vanillite_abi::event::ENTITY_TARGET`) of the mob `id` about
/// to change its target from `old` to `new` for `reason`: whether it may. Without listeners
/// (or without a change) one branch.
pub(crate) fn target_event(
    ents: &crate::entities::Entities,
    id: EntityId,
    old: Option<EntityId>,
    new: Option<EntityId>,
    reason: u32,
) -> bool {
    if old == new {
        return true;
    }
    let Some(hooks) = ents.hooks_for(vanillite_abi::event::ENTITY_TARGET) else {
        return true;
    };
    let Some(e) = ents.store.get(id) else {
        return true;
    };
    let target = new.and_then(|n| ents.store.get(n));
    let mut ev = crate::hooks::Event {
        entity: e.uuid(),
        entity_type: e.kind().name().to_owned(),
        other: target.as_ref().map_or(0, |t| t.uuid()),
        other_type: target
            .as_ref()
            .map_or_else(String::new, |t| t.kind().name().to_owned()),
        cause: reason,
        ..crate::hooks::Event::new(vanillite_abi::event::ENTITY_TARGET)
    };
    hooks.event(crate::hooks::HookWorld::entities(ents), &mut ev);
    !ev.cancelled
}

/// `NearestAttackableTargetGoal`'s reason: the closest player or the closest other entity.
pub(crate) fn closest_reason(t: &Tick<'_, '_>, found: Option<EntityId>) -> u32 {
    if found.is_some_and(|f| t.ents.player(f).is_some()) {
        vanillite_abi::target_reason::CLOSEST_PLAYER
    } else {
        vanillite_abi::target_reason::CLOSEST_ENTITY
    }
}

/// `Mob.setTarget` with the kinds' overrides: a zombified piglin rolls its anger sound delay
/// and alert interval when it takes its first target.
pub(crate) fn assign_target(m: &mut super::MobData, target: Option<EntityId>) {
    if m.kind == super::Kind::ZombifiedPiglin && m.target.is_none() && target.is_some() {
        super::fam::undead::zombies::piglin_first_target(m);
    }
    m.target = target;
}

fn fluid_jump_threshold(t: &Tick<'_, '_>) -> f64 {
    if let Some(v) =
        super::fam::undead::cubes::fluid_jump_threshold(t.mob_ref(), t.core().dims().height)
    {
        return v;
    }
    if super::eye_height(t.mob_ref()) < 0.4 {
        0.0
    } else {
        0.4
    }
}

// --- Goal behaviour -------------------------------------------------------------------------

/// `canUse`.
pub(crate) fn can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    if matches!(g, Goal::Melee { .. }) {
        match t.mob_ref().kind {
            // `BeeAttackGoal`: only an angry bee that has not stung yet.
            Kind::Bee => return can_use_goal(t, g) && super::fam::wildlife::bee::can_attack(t),
            // `FoxMeleeAttackGoal`: not while sitting, sleeping, crouching or faceplanted.
            Kind::Fox => {
                return super::fam::companion::fox::can_melee(t.mob_ref()) && can_use_goal(t, g);
            }
            _ => {}
        }
    }
    can_use_goal(t, g)
}

fn can_use_goal(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::Fam(f) => super::fam::goal_can_use(t, f),
        Goal::Common(c) => super::common::can_use(t, c),
        Goal::Wither(w) => super::boss::wither::goal_can_use(t, w),
        Goal::Float => {
            let f = t.mob_ref().fluids;
            f.water_height > fluid_jump_threshold(t) || f.in_lava()
        }
        Goal::Panic { pos, .. } => {
            let panics = t
                .mob_ref()
                .last_damage_source(t.game_time())
                .is_some_and(|k| t.env.types.is(k, tag::PANIC_CAUSES));
            // `AbstractHorse.MountPanicGoal`: not while a mob steers the horse.
            let mount_panic = matches!(t.mob_ref().kind, Kind::Horse | Kind::Donkey | Kind::Mule);
            if !panics || mount_panic && super::controlling_mob_passenger(t.ents, t.id).is_some() {
                return false;
            }
            match super::animal::panic_pos(t) {
                Some(p) => {
                    *pos = p;
                    true
                }
                None => false,
            }
        }
        Goal::Stroll {
            interval,
            probability,
            wanted,
            ..
        } => {
            if super::has_controlling_passenger(t.ents, t.id) || t.mob_ref().no_action_time >= 100 {
                return false;
            }
            if random_int(t, reduced(*interval)) != 0 {
                return false;
            }
            let pos = if t.mob_ref().fluids.in_water() {
                land_random_pos(t, 15, 7).or_else(|| default_random_pos(t, 10, 7))
            } else if random_float(t) >= *probability {
                land_random_pos(t, 10, 7)
            } else {
                default_random_pos(t, 10, 7)
            };
            match pos {
                Some(p) => {
                    *wanted = p;
                    true
                }
                None => false,
            }
        }
        Goal::LookAtPlayer {
            distance,
            probability,
            look_at,
            ..
        } => {
            if random_float(t) >= *probability {
                return false;
            }
            if t.mob_ref().target.is_some() {
                *look_at = t.mob_ref().target;
            }
            *look_at = nearest_player(t, false, f64::from(*distance), true);
            look_at.is_some()
        }
        Goal::RandomLook { .. } => random_float(t) < 0.02,
        Goal::Melee {
            kind, last_can_use, ..
        } => {
            let time = t.game_time();
            if time - *last_can_use < 20 {
                return false;
            }
            *last_can_use = time;
            let Some(target) = t.mob_ref().target else {
                return false;
            };
            let Some(pos) = other_pos(t, target) else {
                return false;
            };
            let _ = kind;
            let path = nav::create_path_to(t, BlockPos::containing(pos.x, pos.y, pos.z), 0);
            let ok = path.is_some() || within_melee_range(t, target);
            t.mob().goal_path = path;
            ok
        }
        Goal::Swell => {
            let m = t.mob_ref();
            super::fam::classic::creeper(m).swell_dir > 0
                || m.target.is_some_and(|target| {
                    other_pos(t, target).is_some_and(|p| distance_sqr(t, p) < 9.0)
                })
        }
        Goal::Leap { target, .. } => {
            if super::has_controlling_passenger(t.ents, t.id) {
                return false;
            }
            *target = t.mob_ref().target;
            let Some(pos) = target.and_then(|id| other_pos(t, id)) else {
                return false;
            };
            let d = distance_sqr(t, pos);
            if !(4.0..=16.0).contains(&d) || !t.on_ground() {
                return false;
            }
            random_int(t, reduced(5)) == 0
        }
        Goal::FleeSun { wanted, .. } => {
            if t.mob_ref().target.is_some()
                || !t.cx.env.is_bright_outside()
                || !t.mob_ref().is_on_fire()
                || !super::can_see_sky(t.level, t.block_pos())
            {
                return false;
            }
            let base = t.block_pos();
            for _ in 0..10 {
                let x = random_int(t, 20) - 10;
                let y = random_int(t, 6) - 3;
                let z = random_int(t, 20) - 10;
                let pos = base.offset(x, y, z);
                if !super::can_see_sky(t.level, pos) && walk_target_value(t, pos) < 0.0 {
                    *wanted = bottom_center(pos);
                    return true;
                }
            }
            false
        }
        // `RestrictSunGoal.canUse`: not with a helmet (ground navigation: every user walks).
        Goal::RestrictSun => {
            t.cx.env.is_bright_outside()
                && t.mob_ref()
                    .equipment
                    .get(crate::item::EquipmentSlot::Head)
                    .is_empty()
        }
        Goal::Bow { .. } => t.mob_ref().target.is_some() && holds_bow(t.mob_ref()),
        Goal::EatBlock { .. } => {
            let chance = if t.mob_ref().baby { 50 } else { 1000 };
            if random_int(t, g.adjusted(chance)) != 0 {
                return false;
            }
            let pos = t.block_pos();
            kinds::edible_for_sheep(t.level.block(pos))
                || t.level
                    .block(BlockPos::new(pos.x, pos.y - 1, pos.z))
                    .block()
                    == vanillite_data::block::ids::GRASS_BLOCK
        }
        Goal::FollowParent { parent, .. } => {
            if t.mob_ref().age >= 0 {
                return false;
            }
            let kind = t.mob_ref().kind;
            let area = t.aabb().inflate(8.0, 4.0, 8.0);
            let mut ids = std::mem::take(&mut t.cx.ids);
            ids.clear();
            t.ents.store.for_each_in(&area, false, |id| ids.push(id));
            let mut closest: Option<(EntityId, f64)> = None;
            for &id in &ids {
                let Some(m) = t.ents.mob(id) else {
                    continue;
                };
                if m.kind != kind || m.age < 0 || id == t.id {
                    continue;
                }
                let Some(e) = t.ents.store.get(id) else {
                    continue;
                };
                let d = distance_sqr(t, e.pos());
                if closest.is_none_or(|(_, c)| d <= c) {
                    closest = Some((id, d));
                }
            }
            t.cx.ids = ids;
            match closest {
                Some((id, d)) if d >= 9.0 => {
                    *parent = Some(id);
                    true
                }
                _ => false,
            }
        }
        Goal::Breed { partner, .. } => {
            if t.mob_ref().in_love <= 0 {
                return false;
            }
            *partner = super::animal::free_partner(t);
            // `PandaBreedGoal`: only without unhappiness and with bamboo near.
            partner.is_some()
                && (t.mob_ref().kind != Kind::Panda
                    || super::fam::companion::panda::breed_allowed(t))
        }
        Goal::Tempt {
            items,
            player,
            calm_down,
            ..
        } => {
            if *calm_down > 0 {
                *calm_down -= 1;
                return false;
            }
            *player = tempting_player(t, *items);
            player.is_some()
        }
        Goal::EndermanLeaveBlock => {
            super::fam::classic::enderman(t.mob_ref()).carried.is_some()
                && t.env.rules.bool(Rule::MOB_GRIEFING)
                && random_int(t, reduced(2000)) == 0
        }
        Goal::EndermanTakeBlock => {
            super::fam::classic::enderman(t.mob_ref()).carried.is_none()
                && t.env.rules.bool(Rule::MOB_GRIEFING)
                && random_int(t, reduced(20)) == 0
        }
        Goal::SitWhenOrdered => super::tame::sit_can_use(t),
        Goal::FollowOwner { start_distance, .. } => super::tame::follow_can_use(t, *start_distance),
        Goal::OwnerHurtBy {
            timestamp, found, ..
        } => super::tame::owner_hurt_by_can_use(t, *timestamp, found, true),
        Goal::OwnerHurt {
            timestamp, found, ..
        } => super::tame::owner_hurt_by_can_use(t, *timestamp, found, false),
        Goal::NearestTarget {
            interval,
            found,
            spider,
            state,
        } => {
            if *spider && t.light_magic_value() >= 0.5 {
                return false;
            }
            if *interval > 0 && random_int(t, *interval) != 0 {
                return false;
            }
            let range = t.mob_ref().attrs.follow_range;
            *found = nearest_player(t, true, range, state.must_see);
            found.is_some()
        }
        Goal::HurtBy { timestamp, .. } => {
            let m = t.mob_ref();
            let (Some(by), stamp) = (m.last_hurt_by_mob, m.last_hurt_by_mob_time) else {
                return false;
            };
            if stamp == *timestamp {
                return false;
            }
            if t.ents.player(by).is_some() && t.env.rules.bool(Rule::UNIVERSAL_ANGER) {
                return false;
            }
            // `HurtByTargetGoal(this, Guardian.class)` of the dolphin.
            if m.kind == Kind::Dolphin
                && t.ents
                    .mob(by)
                    .is_some_and(|o| matches!(o.kind, Kind::Guardian | Kind::ElderGuardian))
            {
                return false;
            }
            // `HURT_BY_TARGETING` ignores line of sight and invisibility.
            if t.ents.player(by).is_some() {
                test_player(t, by, true, -1.0, false)
            } else {
                can_attack(t, by)
            }
        }
        Goal::EndermanFreeze { target } => {
            *target = t.mob_ref().target;
            let Some(id) = *target else {
                return false;
            };
            if t.ents.player(id).is_none() {
                return false;
            }
            let Some(pos) = other_pos(t, id) else {
                return false;
            };
            distance_sqr(t, pos) <= 256.0 && kinds::is_stared_at_by(t, id)
        }
        Goal::EndermanStare { pending, state, .. } => {
            let range = t.mob_ref().attrs.follow_range;
            let p = nearest_player(t, true, range, false);
            *pending = p.filter(|&id| kinds::is_stared_at_by(t, id));
            let _ = state;
            pending.is_some()
        }
        Goal::Guardian(g) => super::boss::guardian::goal_can_use(t, g),
    }
}

/// `canContinueToUse`.
pub(crate) fn can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    if matches!(g, Goal::Melee { .. }) && t.mob_ref().kind == Kind::Bee {
        // `BeeAttackGoal`: only an angry bee that has not stung yet.
        return can_continue_goal(t, g) && super::fam::wildlife::bee::can_attack(t);
    }
    can_continue_goal(t, g)
}

fn can_continue_goal(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::Fam(f) => super::fam::goal_can_continue(t, f),
        Goal::Common(c) => super::common::can_continue(t, c),
        Goal::Wither(w) => super::boss::wither::goal_can_continue(t, w),
        Goal::Float | Goal::RestrictSun => can_use(t, g),
        Goal::Stroll { .. } => {
            !t.nav_ref().is_done() && !super::has_controlling_passenger(t.ents, t.id)
        }
        Goal::Panic { .. } | Goal::FleeSun { .. } => !t.nav_ref().is_done(),
        Goal::LookAtPlayer {
            distance,
            look_at,
            time,
            ..
        } => {
            let Some(pos) = look_at.and_then(|id| other_pos(t, id)) else {
                return false;
            };
            let d = f64::from(*distance);
            distance_sqr(t, pos) <= d * d && *time > 0
        }
        Goal::RandomLook { time, .. } => *time >= 0,
        Goal::Melee {
            kind,
            follow_unseen,
            ..
        } => {
            if *kind == MeleeKind::Spider && t.light_magic_value() >= 0.5 && random_int(t, 100) == 0
            {
                set_target(t, None);
                return false;
            }
            let Some(target) = t.mob_ref().target else {
                return false;
            };
            if t.other(target).is_none() {
                return false;
            }
            if !*follow_unseen {
                return !t.nav_ref().is_done();
            }
            !is_creative_or_spectator(t, target)
        }
        Goal::Swell => can_use(t, g),
        Goal::Leap { .. } => !t.on_ground(),
        Goal::Bow { .. } => {
            (t.mob_ref().target.is_some() || !t.nav_ref().is_done()) && holds_bow(t.mob_ref())
        }
        Goal::EatBlock { tick } => *tick > 0,
        Goal::FollowParent { parent, .. } => {
            if t.mob_ref().age >= 0 {
                return false;
            }
            let Some(pos) = parent.and_then(|id| other_pos(t, id)) else {
                return false;
            };
            let d = distance_sqr(t, pos);
            (9.0..=256.0).contains(&d)
        }
        Goal::Breed {
            partner, love_time, ..
        } => {
            partner.is_some_and(|p| {
                t.ents.mob(p).is_some_and(|m| {
                    m.is_alive() && m.in_love > 0 && !super::animal::is_panicking(m)
                })
            }) && *love_time < 60
        }
        Goal::Tempt { .. } => can_use(t, g),
        Goal::EndermanLeaveBlock | Goal::EndermanTakeBlock => can_use(t, g),
        Goal::SitWhenOrdered => t.mob_ref().order_to_sit,
        Goal::FollowOwner { stop_distance, .. } => {
            super::tame::follow_can_continue(t, *stop_distance)
        }
        Goal::OwnerHurtBy { state, .. } | Goal::OwnerHurt { state, .. } => {
            target_can_continue(t, state)
        }
        Goal::NearestTarget { state, .. } | Goal::HurtBy { state, .. } => {
            target_can_continue(t, state)
        }
        Goal::EndermanFreeze { .. } => can_use(t, g),
        Goal::EndermanStare {
            state,
            pending,
            aggro_time,
            ..
        } => {
            if pending.is_some() {
                // `EndermanLookForPlayerGoal`: still waiting to scream while the player keeps
                // staring (or it is angry at it).
                if let Some(id) = *pending
                    && t.ents.player(id).is_some()
                    && *aggro_time > 0
                    && (kinds::is_stared_at_by(t, id) || super::common::is_angry_at(t, id))
                {
                    // `enderman.lookAt(pendingTarget, 10, 10)`.
                    if let Some(o) = t.other(id) {
                        kinds::look_at_entity(t, o, 10.0, 10.0);
                    }
                    return true;
                }
                return false;
            }
            target_can_continue(t, state)
        }
        Goal::Guardian(g) => super::boss::guardian::goal_can_continue(t, g),
    }
}

/// `RangedBowAttackGoal.isHoldingBow`: a bow in either hand.
fn holds_bow(m: &super::MobData) -> bool {
    let bow = vanillite_data::item::ids::BOW.0;
    let off = m.equipment.get(crate::item::EquipmentSlot::OffHand);
    m.equipment.holds(bow) || (!off.is_empty() && off.item == bow)
}

/// `TargetGoal.canContinueToUse`.
pub(crate) fn target_can_continue(t: &mut Tick<'_, '_>, state: &mut TargetState) -> bool {
    let Some(target) = t.mob_ref().target.or(state.target_mob) else {
        return false;
    };
    if !can_attack(t, target) {
        return false;
    }
    let Some(pos) = other_pos(t, target) else {
        return false;
    };
    let range = t.mob_ref().attrs.follow_range;
    if distance_sqr(t, pos) > range * range {
        return false;
    }
    if state.must_see {
        if has_line_of_sight(t, target) {
            state.unseen_ticks = 0;
        } else {
            state.unseen_ticks += 1;
            if state.unseen_ticks > reduced(state.unseen_memory) {
                return false;
            }
        }
    }
    set_target(t, Some(target));
    true
}

/// `start`.
pub(crate) fn start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    if matches!(g, Goal::Melee { .. }) && t.mob_ref().kind == Kind::Fox {
        // `FoxMeleeAttackGoal.start`.
        super::fam::companion::fox::melee_start(t);
    }
    match g {
        Goal::Fam(f) => super::fam::goal_start(t, f),
        Goal::Common(c) => super::common::start(t, c),
        Goal::Wither(w) => super::boss::wither::goal_start(t, w),
        Goal::Panic { speed, pos } => {
            nav::move_to_pos(t, *pos, *speed);
        }
        Goal::Stroll { speed, wanted, .. } => {
            nav::move_to_pos(t, *wanted, *speed);
        }
        Goal::FleeSun { speed, wanted } => {
            nav::move_to_pos(t, *wanted, *speed);
        }
        Goal::LookAtPlayer { time, .. } => {
            let r = random_int(t, 40);
            *time = reduced(40 + r);
        }
        Goal::RandomLook { rel_x, rel_z, time } => {
            let rnd = std::f64::consts::TAU * t.mob().random.next_double();
            *rel_x = rnd.cos();
            *rel_z = rnd.sin();
            *time = 20 + random_int(t, 20);
        }
        Goal::Melee {
            speed,
            recalc,
            attack_in,
            raise_arm,
            ..
        } => {
            let path = t.mob().goal_path.take();
            nav::move_to_path(t, path, *speed);
            set_aggressive(t, true);
            *recalc = 0;
            *attack_in = 0;
            *raise_arm = 0;
        }
        Goal::Swell => {
            t.nav().stop();
        }
        Goal::Leap { target, .. } => {
            let Some(pos) = target.and_then(|id| other_pos(t, id)) else {
                return;
            };
            let Goal::Leap { yd, .. } = *g else {
                return;
            };
            let v = t.velocity();
            let p = t.pos();
            let mut d = Vec3::new(pos.x - p.x, 0.0, pos.z - p.z);
            if d.length_sqr() > 1.0e-7 {
                d = d.normalize() * 0.4 + v * 0.2;
            }
            t.set_velocity(Vec3::new(d.x, f64::from(yd), d.z));
        }
        Goal::RestrictSun => {
            t.mob().nav.avoid_sun = true;
        }
        Goal::Bow { .. } => set_aggressive(t, true),
        Goal::EatBlock { tick } => {
            *tick = reduced(40);
            let id = t.id;
            living::entity_event(t, id, living::event::EAT_GRASS);
            t.nav().stop();
        }
        Goal::FollowParent { recalc, .. } => *recalc = 0,
        Goal::Breed { .. }
        | Goal::Tempt { .. }
        | Goal::EndermanLeaveBlock
        | Goal::EndermanTakeBlock => {}
        Goal::SitWhenOrdered => {
            t.nav().stop();
            super::tame::set_in_sitting_pose(t, true);
        }
        Goal::FollowOwner { recalc, .. } => *recalc = 0,
        Goal::OwnerHurtBy {
            state,
            timestamp,
            found,
        } => {
            set_target_why(t, *found, vanillite_abi::target_reason::OWNER);
            state.unseen_ticks = 0;
            *timestamp = super::tame::owner_hurt_stamp(t, true);
        }
        Goal::OwnerHurt {
            state,
            timestamp,
            found,
        } => {
            set_target_why(t, *found, vanillite_abi::target_reason::OWNER);
            state.unseen_ticks = 0;
            *timestamp = super::tame::owner_hurt_stamp(t, false);
        }
        Goal::NearestTarget { state, found, .. } => {
            let reason = closest_reason(t, *found);
            set_target_why(t, *found, reason);
            state.unseen_ticks = 0;
        }
        Goal::HurtBy {
            state,
            timestamp,
            alert,
        } => {
            let by = t.mob_ref().last_hurt_by_mob;
            set_target_why(t, by, vanillite_abi::target_reason::RETALIATE);
            state.target_mob = by;
            *timestamp = t.mob_ref().last_hurt_by_mob_time;
            state.unseen_memory = 300;
            if *alert && let Some(by) = by {
                alert_others(t, by);
            }
            state.unseen_ticks = 0;
        }
        Goal::EndermanFreeze { .. } => {
            t.nav().stop();
        }
        Goal::EndermanStare {
            aggro_time,
            teleport_time,
            ..
        } => {
            *aggro_time = reduced(5);
            *teleport_time = 0;
            kinds::set_creepy(t, true);
        }
        Goal::Float => {}
        Goal::Guardian(g) => super::boss::guardian::goal_start(t, g),
    }
}

/// `HurtByTargetGoal.alertOthers` for mobs of the same kind.
fn alert_others(t: &mut Tick<'_, '_>, by: EntityId) {
    let range = t.mob_ref().attrs.follow_range;
    let p = t.pos();
    let area =
        Aabb::new(p.x, p.y, p.z, p.x + 1.0, p.y + 1.0, p.z + 1.0).inflate(range, 10.0, range);
    let kind = t.mob_ref().kind;
    let owner = t.mob_ref().owner;
    let tamable = crate::mob::fam::companion::is_tamable(kind)
        || matches!(kind, Kind::Nautilus | Kind::ZombieNautilus);
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&area, false, |id| ids.push(id));
    for &id in &ids {
        if id == t.id {
            continue;
        }
        let alerted = t
            .ents
            .mob(id)
            .is_some_and(|m| m.kind == kind && m.target.is_none() && m.is_alive())
            // `alertOthers` of a pet: only the pets of the same owner.
            && (!tamable || t.ents.mob(id).is_some_and(|o| o.owner == owner))
            // `PandaHurtByTargetGoal.alertOther`: only aggressive pandas join in.
            && (kind != Kind::Panda
                || t.ents
                    .mob(id)
                    .is_some_and(crate::mob::fam::companion::panda::is_aggressive));
        if alerted
            && target_event(
                t.ents,
                id,
                None,
                Some(by),
                vanillite_abi::target_reason::RETALIATE,
            )
            && let Some(m) = t.ents.mob_mut(id)
        {
            assign_target(m, Some(by));
        }
    }
    t.cx.ids = ids;
}

/// `stop`.
pub(crate) fn stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::Fam(f) => super::fam::goal_stop(t, f),
        Goal::Common(c) => super::common::stop(t, c),
        Goal::Wither(w) => super::boss::wither::goal_stop(t, w),
        Goal::Stroll { .. } => t.nav().stop(),
        Goal::LookAtPlayer { look_at, .. } => *look_at = None,
        Goal::Melee { .. } => {
            if let Some(target) = t.mob_ref().target
                && is_creative_or_spectator(t, target)
            {
                set_target(t, None);
            }
            set_aggressive(t, false);
            t.nav().stop();
        }
        Goal::Swell => {}
        Goal::RestrictSun => t.mob().nav.avoid_sun = false,
        Goal::Bow {
            see_time,
            attack_time,
            ..
        } => {
            set_aggressive(t, false);
            *see_time = 0;
            *attack_time = -1;
            projectile::stop_using_item(t);
        }
        Goal::EatBlock { tick } => *tick = 0,
        Goal::Breed {
            partner, love_time, ..
        } => {
            *partner = None;
            *love_time = 0;
        }
        Goal::Tempt {
            player, calm_down, ..
        } => {
            *player = None;
            t.nav().stop();
            *calm_down = reduced(100);
        }
        Goal::SitWhenOrdered => super::tame::set_in_sitting_pose(t, false),
        Goal::FollowOwner { .. } => t.nav().stop(),
        Goal::OwnerHurtBy { state, .. } | Goal::OwnerHurt { state, .. } => {
            set_target(t, None);
            state.target_mob = None;
        }
        Goal::FollowParent { parent, .. } => *parent = None,
        Goal::NearestTarget { state, .. } | Goal::HurtBy { state, .. } => {
            set_target(t, None);
            state.target_mob = None;
        }
        Goal::EndermanStare { state, pending, .. } => {
            *pending = None;
            set_target(t, None);
            state.target_mob = None;
            kinds::set_creepy(t, false);
        }
        Goal::Guardian(g) => super::boss::guardian::goal_stop(t, g),
        _ => {}
    }
}

/// `tick`.
pub(crate) fn tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::Fam(f) => super::fam::goal_tick(t, f),
        Goal::Common(c) => super::common::tick(t, c),
        Goal::Wither(w) => super::boss::wither::goal_tick(t, w),
        Goal::Float => {
            if random_float(t) < 0.8 {
                t.mob().jump = true;
            }
        }
        Goal::LookAtPlayer { look_at, time, .. } => {
            if let Some(o) = look_at.and_then(|id| t.other(id)) {
                nav::look_at(t, Vec3::new(o.pos.x, o.eye_y, o.pos.z));
                *time -= 1;
            }
        }
        Goal::RandomLook { rel_x, rel_z, time } => {
            *time -= 1;
            let p = t.pos();
            let at = Vec3::new(p.x + *rel_x, t.eye_y(), p.z + *rel_z);
            nav::look_at(t, at);
        }
        Goal::Melee { .. } => melee_tick(t, g),
        Goal::Swell => {
            let target = t.mob_ref().target;
            let dir = match target.and_then(|id| t.other(id).map(|o| (id, o))) {
                Some((id, o)) => {
                    if distance_sqr(t, o.pos) > 49.0 || !has_line_of_sight(t, id) {
                        -1
                    } else {
                        1
                    }
                }
                None => -1,
            };
            kinds::set_swell_dir(t, dir);
        }
        Goal::Bow { .. } => bow_tick(t, g),
        Goal::EatBlock { tick } => {
            *tick = (*tick - 1).max(0);
            if let Some(s) = super::fam::classic::try_sheep_mut(t.mob()) {
                s.eat_animation = *tick;
            }
            if *tick == reduced(4) {
                kinds::eat_block(t);
            }
        }
        Goal::FollowParent {
            speed,
            parent,
            recalc,
        } => {
            *recalc -= 1;
            if *recalc <= 0 {
                *recalc = reduced(10);
                if let Some(pos) = parent.and_then(|id| other_pos(t, id)) {
                    nav::move_to_entity(t, pos, 1, *speed);
                }
            }
        }
        Goal::Breed {
            speed,
            partner,
            love_time,
        } => {
            let Some(p) = *partner else {
                return;
            };
            let Some(o) = t.other(p) else {
                return;
            };
            let x_max = super::kinds::max_head_x_rot(t.mob_ref());
            t.mob()
                .look
                .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 10.0, x_max);
            nav::move_to_entity(t, o.pos, 1, *speed);
            *love_time += 1;
            if *love_time >= reduced(60) && distance_sqr(t, o.pos) < 9.0 {
                let id = t.id;
                super::animal::breed(t.level, t.ents, t.env, id, p);
            }
        }
        Goal::Tempt { speed, player, .. } => {
            let Some(o) = player.and_then(|p| t.other(p)) else {
                return;
            };
            // `getMaxHeadYRot() + 20`, `getMaxHeadXRot()`.
            let y_max = super::kinds::max_head_y_rot(t.mob_ref()) + 20.0;
            let x_max = super::kinds::max_head_x_rot(t.mob_ref());
            t.mob()
                .look
                .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), y_max, x_max);
            if distance_sqr(t, o.pos) < 6.25 {
                t.nav().stop();
            } else {
                nav::move_to_entity(t, o.pos, 1, *speed);
            }
        }
        Goal::EndermanLeaveBlock => kinds::enderman_leave_block(t),
        Goal::EndermanTakeBlock => kinds::enderman_take_block(t),
        Goal::FollowOwner { speed, recalc, .. } => super::tame::follow_tick(t, *speed, recalc),
        Goal::EndermanFreeze { target } => {
            if let Some(o) = target.and_then(|id| t.other(id)) {
                nav::look_at(t, Vec3::new(o.pos.x, o.eye_y, o.pos.z));
            }
        }
        Goal::EndermanStare {
            pending,
            aggro_time,
            teleport_time,
            state,
        } => {
            if t.mob_ref().target.is_none() {
                state.target_mob = None;
            }
            if let Some(id) = *pending {
                *aggro_time -= 1;
                if *aggro_time <= 0 {
                    // Screams and takes the player as its target.
                    set_target_why(t, Some(id), vanillite_abi::target_reason::CLOSEST_PLAYER);
                    state.target_mob = Some(id);
                    *pending = None;
                    kinds::play_sound(t, "minecraft:entity.enderman.stare", 2.5, 1.0);
                }
            } else if let Some(target) = state.target_mob
                && t.ents.vehicle_of(t.id).is_none()
                && let Some(o) = t.other(target)
            {
                // Stared at up close it teleports away; far off it follows every 30 ticks.
                let dist = o.pos.distance_to_sqr(t.pos());
                if kinds::is_stared_at_by(t, target) {
                    if dist < 16.0 {
                        let id = t.id;
                        kinds::teleport_randomly(t.level, t.ents, t.env, t.cx, id);
                    }
                    *teleport_time = 0;
                } else if dist > 256.0 {
                    let due = *teleport_time >= reduced(30);
                    *teleport_time += 1;
                    if due && kinds::teleport_towards(t, o.pos, o.eye_y) {
                        *teleport_time = 0;
                    }
                }
            }
        }
        Goal::Guardian(g) => super::boss::guardian::goal_tick(t, g),
        _ => {}
    }
}

fn melee_tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    let Goal::Melee {
        kind,
        speed,
        follow_unseen,
        pathed,
        recalc,
        attack_in,
        raise_arm,
        ..
    } = g
    else {
        return;
    };
    let Some(target) = t.mob_ref().target else {
        return;
    };
    let Some(o) = t.other(target) else {
        return;
    };
    t.mob()
        .look
        .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 30.0, 30.0);
    *recalc = (*recalc - 1).max(0);
    let sees = *follow_unseen || has_line_of_sight(t, target);
    if sees
        && *recalc <= 0
        && (*pathed == Vec3::ZERO
            || o.pos.distance_to_sqr(*pathed) >= 1.0
            || random_float(t) < 0.05)
    {
        *pathed = o.pos;
        *recalc = 4 + random_int(t, 7);
        let d = distance_sqr(t, o.pos);
        if d > 1024.0 {
            *recalc += 10;
        } else if d > 256.0 {
            *recalc += 5;
        }
        if !nav::move_to_entity(t, o.pos, 0, *speed) {
            *recalc += 15;
        }
        // `adjustedTickDelay` of a goal that updates every tick.
    }
    *attack_in = (*attack_in - 1).max(0);
    if *attack_in <= 0 && within_melee_range(t, target) && has_line_of_sight(t, target) {
        *attack_in = 20;
        combat::mob_attack(t, target);
    }
    if !t.exists() {
        return;
    }
    if *kind == MeleeKind::Zombie {
        *raise_arm += 1;
        let up = *raise_arm >= 5 && *attack_in < 10;
        set_aggressive(t, up);
    }
}

fn bow_tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    let Goal::Bow {
        speed,
        interval_min,
        radius_sqr,
        attack_time,
        see_time,
        clockwise,
        backwards,
        strafing_time,
    } = g
    else {
        return;
    };
    let Some(target) = t.mob_ref().target else {
        return;
    };
    let Some(o) = t.other(target) else {
        return;
    };
    let d = distance_sqr(t, o.pos);
    let sees = has_line_of_sight(t, target);
    if sees != (*see_time > 0) {
        *see_time = 0;
    }
    if sees {
        *see_time += 1;
    } else {
        *see_time -= 1;
    }
    let radius = f64::from(*radius_sqr);
    if d <= radius && *see_time >= 20 {
        t.nav().stop();
        *strafing_time += 1;
    } else {
        nav::move_to_entity(t, o.pos, 1, *speed);
        *strafing_time = -1;
    }
    if *strafing_time >= 20 {
        if random_float(t) < 0.3 {
            *clockwise = !*clockwise;
        }
        if random_float(t) < 0.3 {
            *backwards = !*backwards;
        }
        *strafing_time = 0;
    }
    if *strafing_time > -1 {
        if d > radius * f64::from(0.75_f32) {
            *backwards = false;
        } else if d < radius * f64::from(0.25_f32) {
            *backwards = true;
        }
        let fwd = if *backwards { -0.5 } else { 0.5 };
        let right = if *clockwise { 0.5 } else { -0.5 };
        t.move_ctl().strafe(fwd, right);
        kinds::look_at_entity(t, o, 30.0, 30.0);
    } else {
        t.mob()
            .look
            .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 30.0, 30.0);
    }
    let using = t.mob_ref().using_item;
    if using >= 0 {
        if !sees && *see_time < -60 {
            projectile::stop_using_item(t);
        } else if sees && using >= 20 {
            projectile::stop_using_item(t);
            projectile::skeleton_shoot(t, target, bow_power(using));
            *attack_time = *interval_min;
        }
    } else {
        *attack_time -= 1;
        if *attack_time <= 0 && *see_time >= -60 {
            projectile::start_using_item(t);
        }
    }
}

/// `BowItem.getPowerForTime`.
pub fn bow_power(ticks: i32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let mut pow = ticks as f32 / 20.0;
    pow = (pow * pow + pow * 2.0) / 3.0;
    pow.min(1.0)
}

/// Whether the mob's path type at `pos` is walkable (`MoveControl.isWalkable`).
pub fn walkable(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    path_type_static(t.level, t.level.min_y(), pos.x, pos.y, pos.z) == PathType::Walkable
}
