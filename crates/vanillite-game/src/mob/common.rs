//! Goals many mob families share (`NearestAttackableTargetGoal` with a target filter,
//! `AvoidEntityGoal`, `RangedAttackGoal`, `MoveTowardsRestrictionGoal`,
//! `ResetUniversalAngerTargetGoal`) and the anger of neutral mobs (`NeutralMob`).
//!
//! Filters are plain function pointers so the goals stay `Copy`.

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_data::entity::ids;
use vanillite_world::entity::{EntityFlags, EntityId, KindSet};

use super::goals::{
    TargetState, can_attack, distance_sqr, flag, reduced, set_target, target_can_continue,
};
use super::living::has_line_of_sight;
use super::{Kind, MobData, Tick, nav, randompos};
use crate::survival::rules::{Difficulty, Rule};

/// Which entities a target or avoid goal considers (`Class<T>` plus the selector).
pub type TargetFilter = fn(&Tick<'_, '_>, EntityId) -> bool;

/// The entity types a [`TargetFilter`] can accept at all (its `Class<T>`): searches skip the
/// other entities of crowded sections without asking the filter, which would refuse them.
pub type TargetKinds = Option<&'static KindSet>;

/// `AbstractVillager`.
pub static VILLAGERS: KindSet = KindSet::of(&[ids::VILLAGER, ids::WANDERING_TRADER]);
/// `IronGolem`.
pub static IRON_GOLEMS: KindSet = KindSet::of(&[ids::IRON_GOLEM]);
/// `Turtle`.
pub static TURTLES: KindSet = KindSet::of(&[ids::TURTLE]);
/// `Axolotl`.
pub static AXOLOTLS: KindSet = KindSet::of(&[ids::AXOLOTL]);
/// `Wolf`.
pub static WOLVES: KindSet = KindSet::of(&[ids::WOLF]);
/// `Ocelot`.
pub static OCELOTS: KindSet = KindSet::of(&[ids::OCELOT]);
/// `Cat`.
pub static CATS: KindSet = KindSet::of(&[ids::CAT]);
/// `Armadillo`.
pub static ARMADILLOS: KindSet = KindSet::of(&[ids::ARMADILLO]);
/// `Endermite`.
pub static ENDERMITES: KindSet = KindSet::of(&[ids::ENDERMITE]);

/// A shared goal with its state.
#[derive(Clone, Copy, Debug)]
pub enum CommonGoal {
    /// `NearestAttackableTargetGoal(mob, type, interval, mustSee, mustReach, selector)`.
    /// `players_only` searches the players like `Player.class` does; else the living
    /// entities in the follow range box.
    NearestAttackable {
        state: TargetState,
        interval: i32,
        found: Option<EntityId>,
        filter: TargetFilter,
        /// The types `filter` accepts at most.
        kinds: TargetKinds,
        players_only: bool,
        /// What a subclass checks before `super.canUse` and `super.canContinueToUse`
        /// (`BeeBecomeAngryTargetGoal.beeCanTarget`), before the interval roll.
        precheck: Option<fn(&Tick<'_, '_>) -> bool>,
    },
    /// `AvoidEntityGoal(mob, type, maxDist, walkSpeed, sprintSpeed, predicate)`.
    Avoid {
        filter: TargetFilter,
        /// The types `filter` accepts at most.
        kinds: TargetKinds,
        max_dist: f32,
        walk: f64,
        sprint: f64,
        to_avoid: Option<EntityId>,
    },
    /// `RangedAttackGoal(mob, speed, intervalMin, intervalMax, radius)`; the attack is the
    /// family's `perform_ranged_attack`.
    Ranged {
        speed: f64,
        interval_min: i32,
        interval_max: i32,
        radius: f32,
        attack_time: i32,
        see_time: i32,
        target: Option<EntityId>,
    },
    /// `MoveTowardsRestrictionGoal(mob, speed)`.
    MoveTowardsRestriction { speed: f64, wanted: Vec3 },
    /// `ResetUniversalAngerTargetGoal(mob, alertOthersOfSameType)`.
    ResetUniversalAnger {
        alert_same_type: bool,
        last_hurt: i32,
    },
    /// The movement goals (strolling, swimming, flying, doors, ...) of `movement.rs`.
    Move(super::movement::MoveGoal),
}

impl CommonGoal {
    /// `NearestAttackableTargetGoal(mob, Player.class, mustSee)` with a filter.
    pub fn nearest_player(must_see: bool, filter: TargetFilter) -> CommonGoal {
        CommonGoal::NearestAttackable {
            state: TargetState::new(must_see),
            interval: reduced(10),
            found: None,
            filter,
            kinds: None,
            players_only: true,
            precheck: None,
        }
    }

    /// `NearestAttackableTargetGoal(mob, type, interval, mustSee, false, selector)` over the
    /// living entities.
    pub fn nearest(interval: i32, must_see: bool, filter: TargetFilter) -> CommonGoal {
        CommonGoal::NearestAttackable {
            state: TargetState::new(must_see),
            interval: reduced(interval),
            found: None,
            filter,
            kinds: None,
            players_only: false,
            precheck: None,
        }
    }

    /// With a check before `canUse` and `canContinueToUse` (before the interval roll).
    #[must_use]
    pub fn with_precheck(mut self, check: fn(&Tick<'_, '_>) -> bool) -> CommonGoal {
        if let CommonGoal::NearestAttackable { precheck, .. } = &mut self {
            *precheck = Some(check);
        }
        self
    }

    pub fn avoid(filter: TargetFilter, max_dist: f32, walk: f64, sprint: f64) -> CommonGoal {
        CommonGoal::Avoid {
            filter,
            kinds: None,
            max_dist,
            walk,
            sprint,
            to_avoid: None,
        }
    }

    /// Declares the only entity types the filter of a target or avoid goal accepts (a
    /// promise: the filter refuses every other type), so its searches skip the others.
    #[must_use]
    pub fn of_kinds(mut self, only: &'static KindSet) -> CommonGoal {
        if let CommonGoal::NearestAttackable { kinds, .. } | CommonGoal::Avoid { kinds, .. } =
            &mut self
        {
            *kinds = Some(only);
        }
        self
    }

    pub fn ranged(speed: f64, interval_min: i32, interval_max: i32, radius: f32) -> CommonGoal {
        CommonGoal::Ranged {
            speed,
            interval_min,
            interval_max,
            radius,
            attack_time: -1,
            see_time: 0,
            target: None,
        }
    }

    pub fn flags(&self) -> u8 {
        match self {
            CommonGoal::NearestAttackable { .. } => flag::TARGET,
            CommonGoal::Avoid { .. } | CommonGoal::MoveTowardsRestriction { .. } => flag::MOVE,
            CommonGoal::Ranged { .. } => flag::MOVE | flag::LOOK,
            CommonGoal::ResetUniversalAnger { .. } => 0,
            CommonGoal::Move(g) => g.flags(),
        }
    }

    pub fn every_tick(&self) -> bool {
        match self {
            CommonGoal::Move(g) => g.every_tick(),
            _ => matches!(self, CommonGoal::Ranged { .. }),
        }
    }

    /// `isInterruptable`.
    pub fn interruptable(&self) -> bool {
        match self {
            CommonGoal::Move(g) => g.interruptable(),
            _ => true,
        }
    }
}

// --- Filters ----------------------------------------------------------------------------

/// Any player.
pub fn any_player(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.player(id).is_some()
}

/// A player the neutral mob is angry at (`this::isAngryAt`).
pub fn angry_at(t: &Tick<'_, '_>, id: EntityId) -> bool {
    is_angry_at(t, id)
}

/// A mob of `kind`.
pub fn mob_of(t: &Tick<'_, '_>, id: EntityId, kind: Kind) -> bool {
    t.ents.mob(id).is_some_and(|m| m.kind == kind)
}

/// `Turtle.BABY_ON_LAND_SELECTOR`: a baby turtle out of water.
pub fn baby_turtle_on_land(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .mob(id)
        .is_some_and(|m| m.kind == Kind::Turtle && m.baby && !m.fluids.in_water())
}

/// An iron golem.
pub fn iron_golem(t: &Tick<'_, '_>, id: EntityId) -> bool {
    mob_of(t, id, Kind::IronGolem)
}

/// `AbstractVillager` (villagers and wandering traders).
pub fn villager(t: &Tick<'_, '_>, id: EntityId) -> bool {
    use vanillite_data::entity::ids;
    t.ents
        .store
        .get(id)
        .is_some_and(|e| e.kind() == ids::VILLAGER || e.kind() == ids::WANDERING_TRADER)
}

/// An adult `AbstractVillager` (`Ravager`'s villager target: `!target.isBaby()`).
pub fn adult_villager(t: &Tick<'_, '_>, id: EntityId) -> bool {
    villager(t, id) && !t.ents.mob(id).is_some_and(|m| m.baby)
}

/// An `Enemy` (monsters, slimes, ghasts, phantoms, shulkers, hoglins).
pub fn enemy(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.mob(id).is_some_and(|m| m.kind.is_monster())
}

// --- Targeting ----------------------------------------------------------------------------

/// `TargetingConditions.forCombat().range(range)` (with the visibility percent) and line of
/// sight when `must_see`, for a player or a mob.
pub fn test_combat(t: &mut Tick<'_, '_>, target: EntityId, range: f64, must_see: bool) -> bool {
    if t.ents.player(target).is_some() {
        return super::goals::test_player(t, target, true, range, must_see);
    }
    if !can_attack(t, target) {
        return false;
    }
    let Some(e) = t.ents.store.get(target) else {
        return false;
    };
    if range > 0.0 {
        let invisible = t
            .ents
            .mob(target)
            .is_some_and(|m| m.effects.has(crate::effect::MobEffect::INVISIBILITY));
        let modifier = if invisible { 0.07 } else { 1.0 };
        let visible = (range * modifier).max(2.0);
        if t.pos().distance_to_sqr(e.pos()) > visible * visible {
            return false;
        }
    }
    !must_see || has_line_of_sight(t, target)
}

/// The nearest living entity passing `filter` and the combat conditions
/// (`getNearestEntity` / `getNearestPlayer` at the mob's eyes).
pub fn find_nearest(
    t: &mut Tick<'_, '_>,
    filter: TargetFilter,
    kinds: TargetKinds,
    range: f64,
    must_see: bool,
    players_only: bool,
    area: Aabb,
) -> Option<EntityId> {
    let p = t.pos();
    let at = Vec3::new(p.x, t.eye_y(), p.z);
    let mut candidates = std::mem::take(&mut t.cx.ids);
    candidates.clear();
    match kinds {
        Some(kinds) if !players_only => {
            t.ents
                .store
                .for_each_of_kinds_in(&area, kinds, |id| candidates.push(id));
        }
        _ => t
            .ents
            .store
            .for_each_in(&area, players_only, |id| candidates.push(id)),
    }
    let mut best: Option<(EntityId, f64)> = None;
    for &id in &candidates {
        if id == t.id {
            continue;
        }
        let living = t.ents.player(id).is_some() || t.ents.mob(id).is_some();
        if !living || (players_only && t.ents.player(id).is_none()) || !filter(t, id) {
            continue;
        }
        if !test_combat(t, id, range, must_see) {
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

// --- Neutral mobs -------------------------------------------------------------------------

/// `isAngry`: the anger end time is in the future.
pub fn is_angry(m: &MobData, game_time: i64) -> bool {
    m.anger_end > 0 && m.anger_end - game_time > 0
}

/// `startPersistentAngerTimer` with the kind's range in ticks (inclusive).
pub fn start_anger_timer(m: &mut MobData, game_time: i64, range: (i32, i32)) {
    let ticks = m.random.next_int_between_inclusive(range.0, range.1);
    m.anger_end = game_time + i64::from(ticks);
}

/// `stopBeingAngry`.
pub fn stop_being_angry(m: &mut MobData) {
    m.last_hurt_by_mob = None;
    m.anger_target = None;
    m.target = None;
    m.anger_end = -1;
}

/// `ServerPlayer.tellNeutralMobsThatIDied` → `NeutralMob.playerDied`: with
/// `forgive_dead_players`, the neutral mobs within 32 blocks (10 vertically) of the dead
/// player's block that are angry at it calm down.
pub fn tell_neutral_mobs_player_died(ents: &mut crate::entities::Entities, player: EntityId) {
    let Some(e) = ents.store.get(player) else {
        return;
    };
    let uuid = e.uuid();
    let p = e.pos();
    let (x, y, z) = (p.x.floor(), p.y.floor(), p.z.floor());
    let area = Aabb::new(x - 32.0, y - 10.0, z - 32.0, x + 33.0, y + 11.0, z + 33.0);
    let angry: Vec<EntityId> = ents
        .mobs
        .iter()
        .filter(|(_, m)| m.anger_target == Some(uuid))
        .map(|(id, _)| id)
        .filter(|&id| {
            ents.store
                .get(id)
                .is_some_and(|e| e.aabb().intersects(&area))
        })
        .collect();
    for id in angry {
        if let Some(m) = ents.mob_mut(id) {
            stop_being_angry(m);
        }
    }
}

/// `isValidPlayerTarget`.
fn valid_player_target(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.player(id).is_some_and(|p| {
        !matches!(p.game_mode, GameMode::Creative | GameMode::Spectator)
            && t.env.difficulty != Difficulty::Peaceful
    })
}

/// `isAngryAtAllPlayers`.
pub fn is_angry_at_all_players(t: &Tick<'_, '_>) -> bool {
    let m = t.mob_ref();
    t.env.rules.bool(Rule::UNIVERSAL_ANGER)
        && is_angry(m, t.env.game_time)
        && m.anger_target.is_none()
}

/// `isAngryAt(entity)`.
pub fn is_angry_at(t: &Tick<'_, '_>, id: EntityId) -> bool {
    if !can_attack(t, id) {
        return false;
    }
    if valid_player_target(t, id) && is_angry_at_all_players(t) {
        return true;
    }
    let uuid = t.ents.store.get(id).map(|e| e.uuid());
    t.mob_ref().anger_target.is_some() && t.mob_ref().anger_target == uuid
}

/// `updatePersistentAnger(level, stayAngryIfTargetPresent)` with the kind's anger range.
pub fn update_persistent_anger(t: &mut Tick<'_, '_>, stay_angry: bool, range: (i32, i32)) {
    let game_time = t.env.game_time;
    let previous = t.mob_ref().target;
    let anger_target = t.mob_ref().anger_target;
    let uuid_of = |t: &Tick<'_, '_>, id: EntityId| t.ents.store.get(id).map(|e| e.uuid());
    if let Some(prev) = previous
        && let Some(m) = t.ents.mob(prev)
        && m.is_dead_or_dying()
        && anger_target.is_some()
        && anger_target == uuid_of(t, prev)
    {
        stop_being_angry(t.mob());
        return;
    }
    // `getTarget` drops dead or removed targets.
    let target = previous.filter(|&id| t.other(id).is_some());
    if let Some(target) = target {
        let uuid = uuid_of(t, target);
        let new = anger_target.is_none() || anger_target != uuid;
        if new {
            t.mob().anger_target = uuid;
        }
        if new || stay_angry {
            start_anger_timer(t.mob(), game_time, range);
        }
    }
    if anger_target.is_some()
        && !is_angry(t.mob_ref(), game_time)
        && (target.is_none() || !target.is_some_and(|id| valid_player_target(t, id)) || !stay_angry)
    {
        stop_being_angry(t.mob());
    }
    if let Some(uuid) = t.mob_ref().anger_target
        && let Some(id) = t.ents.player_by_uuid(uuid)
        && let Some(p) = t.ents.player(id)
        && (matches!(p.game_mode, GameMode::Creative | GameMode::Spectator)
            || t.env.difficulty == Difficulty::Peaceful)
    {
        stop_being_angry(t.mob());
    }
}

/// `forgetCurrentTargetAndRefreshUniversalAnger`.
pub fn forget_and_refresh_universal_anger(m: &mut MobData, game_time: i64, range: (i32, i32)) {
    stop_being_angry(m);
    start_anger_timer(m, game_time, range);
}

/// Writes `anger_end_time` and `angry_at`.
pub fn save_anger(m: &MobData, out: &mut vanillite_nbt::Compound) {
    out.insert("anger_end_time", m.anger_end);
    if let Some(u) = m.anger_target {
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let ints = vec![
            (u >> 96) as i32,
            (u >> 64) as i32,
            (u >> 32) as i32,
            u as i32,
        ];
        out.insert("angry_at", ints);
    }
}

/// Reads `anger_end_time` (or the old `AngerTime`) and `angry_at`.
pub fn load_anger(m: &mut MobData, tag: &vanillite_nbt::Compound, game_time: i64) {
    use vanillite_nbt::Tag;
    m.anger_end = match (tag.get("anger_end_time"), tag.get("AngerTime")) {
        (Some(Tag::Long(v)), _) => *v,
        (_, Some(Tag::Int(v))) => game_time + i64::from(*v),
        _ => -1,
    };
    m.anger_target = match tag.get("angry_at") {
        Some(Tag::IntArray(v)) if v.len() == 4 => {
            #[allow(clippy::cast_sign_loss)]
            let u = v
                .iter()
                .fold(0u128, |acc, &x| (acc << 32) | u128::from(x as u32));
            Some(u)
        }
        _ => None,
    };
}

// --- Goal behaviour -------------------------------------------------------------------------

/// The anger range of the ticking mob's kind.
fn anger_range(t: &Tick<'_, '_>) -> (i32, i32) {
    super::fam::anger_range(t.mob_ref().kind)
}

pub fn can_use(t: &mut Tick<'_, '_>, g: &mut CommonGoal) -> bool {
    match g {
        CommonGoal::Move(m) => super::movement::can_use(t, m),
        CommonGoal::NearestAttackable {
            state,
            interval,
            found,
            filter,
            kinds,
            players_only,
            precheck,
        } => {
            if precheck.is_some_and(|g| !g(t)) {
                return false;
            }
            if *interval > 0 && t.mob().random.next_int_bounded(*interval) != 0 {
                return false;
            }
            let range = t.mob_ref().attrs.follow_range;
            let area = if t.mob_ref().kind == Kind::Shulker {
                super::fam::illager::shulker::target_search_area(t, range)
            } else {
                t.aabb().inflate(range, range, range)
            };
            *found = find_nearest(
                t,
                *filter,
                *kinds,
                range,
                state.must_see,
                *players_only,
                area,
            );
            found.is_some()
        }
        CommonGoal::Avoid {
            filter,
            kinds,
            max_dist,
            to_avoid,
            ..
        } => {
            let d = f64::from(*max_dist);
            let area = t.aabb().inflate(d, 3.0, d);
            // `getNearestEntity(..., x, y, z)`: measured from the feet.
            let p = t.pos();
            let mut candidates = std::mem::take(&mut t.cx.ids);
            candidates.clear();
            match kinds {
                Some(kinds) => {
                    t.ents
                        .store
                        .for_each_of_kinds_in(&area, kinds, |id| candidates.push(id));
                }
                None => t
                    .ents
                    .store
                    .for_each_in(&area, false, |id| candidates.push(id)),
            }
            let mut best: Option<(EntityId, f64)> = None;
            for &id in &candidates {
                if id == t.id
                    || !(t.ents.player(id).is_some() || t.ents.mob(id).is_some())
                    || !filter(t, id)
                    || !test_combat(t, id, d, true)
                {
                    continue;
                }
                let Some(e) = t.ents.store.get(id) else {
                    continue;
                };
                let dist = e.pos().distance_to_sqr(p);
                if best.is_none_or(|(_, b)| dist < b) {
                    best = Some((id, dist));
                }
            }
            t.cx.ids = candidates;
            let Some((avoid, _)) = best else {
                return false;
            };
            *to_avoid = Some(avoid);
            let Some(avoid_pos) = t.other(avoid).map(|o| o.pos) else {
                return false;
            };
            let Some(pos) = randompos::default_pos_away(t, 16, 7, avoid_pos) else {
                return false;
            };
            if avoid_pos.distance_to_sqr(pos) < avoid_pos.distance_to_sqr(t.pos()) {
                return false;
            }
            let path = nav::create_path_to(t, BlockPos::containing(pos.x, pos.y, pos.z), 0);
            let ok = path.is_some();
            t.mob().goal_path = path;
            ok
        }
        CommonGoal::Ranged { target, .. } => {
            let best = t.mob_ref().target.filter(|&id| t.other(id).is_some());
            if best.is_some() {
                *target = best;
            }
            best.is_some()
        }
        CommonGoal::MoveTowardsRestriction { wanted, .. } => {
            let here = t.block_pos();
            if t.mob_ref().is_within_restriction(here) {
                return false;
            }
            let home = randompos::bottom_center(t.mob_ref().restrict_center);
            match randompos::default_pos_towards(t, 16, 7, home, randompos::half_pi()) {
                Some(p) => {
                    *wanted = p;
                    true
                }
                None => false,
            }
        }
        CommonGoal::ResetUniversalAnger { last_hurt, .. } => {
            let m = t.mob_ref();
            t.env.rules.bool(Rule::UNIVERSAL_ANGER)
                && m.last_hurt_by_mob
                    .is_some_and(|by| t.ents.player(by).is_some())
                && m.last_hurt_by_mob_time > *last_hurt
        }
    }
}

pub fn can_continue(t: &mut Tick<'_, '_>, g: &mut CommonGoal) -> bool {
    match g {
        CommonGoal::Move(m) => super::movement::can_continue(t, m),
        CommonGoal::NearestAttackable {
            state, precheck, ..
        } => precheck.is_none_or(|g| g(t)) && target_can_continue(t, state),
        CommonGoal::Avoid { .. } | CommonGoal::MoveTowardsRestriction { .. } => {
            !t.nav_ref().is_done()
        }
        CommonGoal::Ranged { target, .. } => {
            let alive = target.is_some_and(|id| t.other(id).is_some());
            can_use(t, g) || alive && !t.nav_ref().is_done()
        }
        CommonGoal::ResetUniversalAnger { .. } => false,
    }
}

pub fn start(t: &mut Tick<'_, '_>, g: &mut CommonGoal) {
    match g {
        CommonGoal::Move(m) => super::movement::start(t, m),
        CommonGoal::NearestAttackable { state, found, .. } => {
            let reason = super::goals::closest_reason(t, *found);
            super::goals::set_target_why(t, *found, reason);
            state.unseen_ticks = 0;
        }
        CommonGoal::Avoid { walk, .. } => {
            let path = t.mob().goal_path.take();
            nav::move_to_path(t, path, *walk);
        }
        CommonGoal::MoveTowardsRestriction { speed, wanted } => {
            nav::move_to_pos(t, *wanted, *speed);
        }
        CommonGoal::ResetUniversalAnger {
            alert_same_type,
            last_hurt,
        } => {
            *last_hurt = t.mob_ref().last_hurt_by_mob_time;
            let range = anger_range(t);
            let game_time = t.env.game_time;
            forget_and_refresh_universal_anger(t.mob(), game_time, range);
            if *alert_same_type {
                let within = t.mob_ref().attrs.follow_range;
                let p = t.pos();
                let area = Aabb::new(p.x, p.y, p.z, p.x + 1.0, p.y + 1.0, p.z + 1.0)
                    .inflate(within, 10.0, within);
                let kind = t.mob_ref().kind;
                let mut ids = std::mem::take(&mut t.cx.ids);
                ids.clear();
                t.ents.store.for_each_in(&area, false, |id| ids.push(id));
                for &id in &ids {
                    if id == t.id {
                        continue;
                    }
                    if let Some(m) = t.ents.mob_mut(id)
                        && m.kind == kind
                    {
                        forget_and_refresh_universal_anger(m, game_time, range);
                    }
                }
                t.cx.ids = ids;
            }
        }
        CommonGoal::Ranged { .. } => {}
    }
}

pub fn stop(t: &mut Tick<'_, '_>, g: &mut CommonGoal) {
    match g {
        CommonGoal::Move(m) => super::movement::stop(t, m),
        CommonGoal::NearestAttackable { state, .. } => {
            set_target(t, None);
            state.target_mob = None;
        }
        CommonGoal::Avoid { to_avoid, .. } => *to_avoid = None,
        CommonGoal::Ranged {
            target,
            see_time,
            attack_time,
            ..
        } => {
            *target = None;
            *see_time = 0;
            *attack_time = -1;
        }
        CommonGoal::MoveTowardsRestriction { .. } | CommonGoal::ResetUniversalAnger { .. } => {}
    }
}

pub fn tick(t: &mut Tick<'_, '_>, g: &mut CommonGoal) {
    match g {
        CommonGoal::Move(m) => super::movement::tick(t, m),
        CommonGoal::Avoid {
            to_avoid,
            walk,
            sprint,
            ..
        } => {
            let Some(pos) = to_avoid.and_then(|id| t.other(id)).map(|o| o.pos) else {
                return;
            };
            let speed = if distance_sqr(t, pos) < 49.0 {
                *sprint
            } else {
                *walk
            };
            t.nav().speed_modifier = speed;
        }
        CommonGoal::Ranged {
            speed,
            interval_min,
            interval_max,
            radius,
            attack_time,
            see_time,
            target,
        } => {
            let Some(id) = *target else {
                return;
            };
            let Some(o) = t.other(id) else {
                return;
            };
            let dist_sqr = distance_sqr(t, o.pos);
            let sees = has_line_of_sight(t, id);
            if sees {
                *see_time += 1;
            } else {
                *see_time = 0;
            }
            let radius_sqr = f64::from(*radius * *radius);
            if dist_sqr <= radius_sqr && *see_time >= 5 {
                t.nav().stop();
            } else {
                nav::move_to_entity(t, o.pos, 1, *speed);
            }
            let look = Vec3::new(o.pos.x, o.eye_y, o.pos.z);
            t.mob().look.set_look_at(look, 30.0, 30.0);
            *attack_time -= 1;
            if *attack_time == 0 {
                if !sees {
                    return;
                }
                #[allow(clippy::cast_possible_truncation)]
                let dist = dist_sqr.sqrt() as f32 / *radius;
                let power = dist.clamp(0.1, 1.0);
                super::fam::perform_ranged_attack(t, id, power);
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                {
                    *attack_time = (dist * (*interval_max - *interval_min) as f32
                        + *interval_min as f32)
                        .floor() as i32;
                }
            } else if *attack_time < 0 {
                let d = dist_sqr.sqrt() / f64::from(*radius);
                let lo = f64::from(*interval_min);
                let hi = f64::from(*interval_max);
                *attack_time = vanillite_base::math::floor(lo + d * (hi - lo));
            }
        }
        _ => {}
    }
}

/// Whether the entity is a player in survival or adventure (helper for filters).
pub fn is_survival_player(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .player(id)
        .is_some_and(|p| matches!(p.game_mode, GameMode::Survival | GameMode::Adventure))
}

/// Whether the entity is crouching (helper for filters).
pub fn is_crouching(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .store
        .get(id)
        .is_some_and(|e| e.flags().contains(EntityFlags::CROUCHING))
}

// --- MoveToBlockGoal ------------------------------------------------------------------------

/// Whether a block is a target of a [`MoveToBlock`] goal (`isValidTarget(level, pos)`).
pub type BlockFilter = fn(&Tick<'_, '_>, BlockPos) -> bool;

/// The state of a `MoveToBlockGoal` subclass; family goals embed it and call its methods
/// from their `canUse`, `start`, `tick` (`requiresUpdateEveryTick`, flags `MOVE | JUMP`).
#[derive(Clone, Copy, Debug)]
pub struct MoveToBlock {
    pub speed: f64,
    pub next_start_tick: i32,
    pub try_ticks: i32,
    pub max_stay_ticks: i32,
    pub block_pos: BlockPos,
    pub reached_target: bool,
    pub search_range: i32,
    pub vertical_search_range: i32,
    pub vertical_search_start: i32,
}

impl MoveToBlock {
    /// `MoveToBlockGoal(mob, speed, searchRange, verticalSearchRange)`.
    pub fn new(speed: f64, search_range: i32, vertical_search_range: i32) -> MoveToBlock {
        MoveToBlock {
            speed,
            next_start_tick: 0,
            try_ticks: 0,
            max_stay_ticks: 0,
            block_pos: BlockPos::new(0, 0, 0),
            reached_target: false,
            search_range,
            vertical_search_range,
            vertical_search_start: 0,
        }
    }

    /// `nextStartTick`: `reducedTickDelay(200 + random(200))`.
    pub fn default_next_start_tick(t: &mut Tick<'_, '_>) -> i32 {
        reduced(200 + t.mob().random.next_int_bounded(200))
    }

    /// `canUse` with the kind's next start delay.
    pub fn can_use(
        &mut self,
        t: &mut Tick<'_, '_>,
        valid: BlockFilter,
        next_start: fn(&mut Tick<'_, '_>) -> i32,
    ) -> bool {
        if self.next_start_tick > 0 {
            self.next_start_tick -= 1;
            return false;
        }
        self.next_start_tick = next_start(t);
        self.find_nearest_block(t, valid)
    }

    /// `canContinueToUse`.
    pub fn can_continue(&self, t: &Tick<'_, '_>, valid: BlockFilter) -> bool {
        self.try_ticks >= -self.max_stay_ticks && self.try_ticks <= 1200 && valid(t, self.block_pos)
    }

    /// `start`: moves to the block (`moveMobToBlock`) and rolls the stay time.
    pub fn start(&mut self, t: &mut Tick<'_, '_>) {
        let p = self.block_pos;
        let at = Vec3::new(
            f64::from(p.x) + 0.5,
            f64::from(p.y) + 1.0,
            f64::from(p.z) + 0.5,
        );
        nav::move_to_pos(t, at, self.speed);
        self.try_ticks = 0;
        let r = &mut t.mob().random;
        let inner = r.next_int_bounded(1200) + 1200;
        self.max_stay_ticks = r.next_int_bounded(inner) + 1200;
    }

    /// `tick` with the move-to target (`getMoveToTarget`, the block above by default) and
    /// the accepted distance (`acceptedDistance`, 1 by default).
    pub fn tick(&mut self, t: &mut Tick<'_, '_>, target: BlockPos, accepted: f64) {
        self.tick_with(t, target, accepted, 40);
    }

    /// `tick` with the kind's `shouldRecalculatePath` interval (`tryTicks % every == 0`).
    pub fn tick_with(&mut self, t: &mut Tick<'_, '_>, target: BlockPos, accepted: f64, every: i32) {
        let center = Vec3::new(
            f64::from(target.x) + 0.5,
            f64::from(target.y) + 0.5,
            f64::from(target.z) + 0.5,
        );
        if center.distance_to_sqr(t.pos()) < accepted * accepted {
            self.reached_target = true;
            self.try_ticks -= 1;
        } else {
            self.reached_target = false;
            self.try_ticks += 1;
            if self.try_ticks % every == 0 {
                let at = Vec3::new(
                    f64::from(target.x) + 0.5,
                    f64::from(target.y),
                    f64::from(target.z) + 0.5,
                );
                nav::move_to_pos(t, at, self.speed);
            }
        }
    }

    /// `findNearestBlock`: rings around the mob, the closest layers first.
    pub fn find_nearest_block(&mut self, t: &Tick<'_, '_>, valid: BlockFilter) -> bool {
        let mob = t.block_pos();
        let (h, v) = (self.search_range, self.vertical_search_range);
        let mut y = self.vertical_search_start;
        while y <= v {
            for r in 0..h {
                let mut x = 0;
                while x <= r {
                    let mut z = if x < r && x > -r { r } else { 0 };
                    while z <= r {
                        let pos = BlockPos::new(mob.x + x, mob.y + y - 1, mob.z + z);
                        if t.mob_ref().is_within_restriction(pos) && valid(t, pos) {
                            self.block_pos = pos;
                            return true;
                        }
                        z = if z > 0 { -z } else { 1 - z };
                    }
                    x = if x > 0 { -x } else { 1 - x };
                }
            }
            y = if y > 0 { -y } else { 1 - y };
        }
        false
    }
}
