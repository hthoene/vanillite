//! `GroundPathNavigation` and the mob controls (`MoveControl`, `LookControl`, `JumpControl`,
//! `BodyRotationControl`).

use vanillite_base::math::wrap_degrees_f;
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_worldgen::behaviour::{Class, class_of};

use super::path::{self, Evaluator, Path, PathMob, PathType, Search};
use super::{Tick, atan2_deg, can_see_sky, is_water};

/// `Mth.degreesDifference`.
pub fn degrees_difference(from: f32, to: f32) -> f32 {
    wrap_degrees_f(to - from)
}

/// `Mth.rotateIfNecessary`.
pub fn rotate_if_necessary(current: f32, target: f32, max: f32) -> f32 {
    let diff = degrees_difference(current, target);
    target - diff.clamp(-max, max)
}

/// `LookControl.rotateTowards`.
fn rotate_towards(from: f32, to: f32, max: f32) -> f32 {
    from + degrees_difference(from, to).clamp(-max, max)
}

/// Which `PathNavigation` a mob uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NavKind {
    /// `GroundPathNavigation`.
    #[default]
    Ground,
    /// `WallClimberNavigation` (spiders).
    WallClimber,
    /// `WaterBoundPathNavigation` (fish, squids, dolphins, guardians); `breaching` lets
    /// dolphins path through the surface.
    Water { breaching: bool },
    /// `AmphibiousPathNavigation` (turtles, axolotls, frogs, drowned).
    Amphibious,
    /// `FlyingPathNavigation` (bees, parrots, allays, vexes).
    Flying,
    /// No path navigation: the mob steers itself (ghasts, phantoms, slimes).
    None,
}

/// Which `MoveControl` a mob uses.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum MoveKind {
    /// `MoveControl`.
    #[default]
    Default,
    /// `FlyingMoveControl(mob, maxTurn, hoversInPlace)`.
    Flying { max_turn: i32, hovers: bool },
    /// `SmoothSwimmingMoveControl(mob, maxTurnX, maxTurnY, inWaterSpeedModifier,
    /// outsideWaterSpeedModifier, applyGravity)`.
    SmoothSwimming {
        max_turn_x: i32,
        max_turn_y: i32,
        in_water_speed: f32,
        outside_water_speed: f32,
        apply_gravity: bool,
    },
    /// The family's own control (`FamilyHooks::move_control`).
    Custom,
}

/// Which `LookControl` a mob uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LookKind {
    /// `LookControl`.
    #[default]
    Default,
    /// `SmoothSwimmingLookControl(mob, maxYRotFromCenter)`.
    SmoothSwimming { max_y_rot: i32 },
    /// The family's own control (`FamilyHooks::look_control`).
    Custom,
}

/// Kind specific differences of a navigation (the `PathNavigation` subclasses of single
/// mobs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NavVariant {
    #[default]
    Default,
    /// `TurtlePathNavigation`: while travelling to water (`travelPos`), only water is a
    /// stable destination.
    Turtle { to_water: bool },
    /// `FrogPathNavigation` with `FrogNodeEvaluator(true)`.
    Frog,
    /// `StriderPathNavigation`: lava is a stable destination.
    Strider,
    /// The bee's flying navigation: a block below is a stable destination.
    Bee,
    /// The baby happy ghast's `BabyFlyingPathNavigation`: fluids do not block a direct move.
    BabyHappyGhast,
}

/// The state of a mob's `PathNavigation` (ground by default, see [`NavKind`]).
#[derive(Clone, Debug)]
pub struct Navigation {
    /// The navigation type.
    pub kind: NavKind,
    /// Kind specific differences.
    pub variant: NavVariant,
    pub path: Option<Path>,
    pub speed_modifier: f64,
    tick: i32,
    last_stuck_check: i32,
    last_stuck_check_pos: Vec3,
    timeout_cached_node: BlockPos,
    timeout_timer: i64,
    last_timeout_check: i64,
    timeout_limit: f64,
    max_distance_to_waypoint: f32,
    has_delayed_recomputation: bool,
    time_last_recompute: i64,
    target_pos: Option<BlockPos>,
    reach_range: i32,
    max_visited_nodes: i32,
    required_path_length: f32,
    /// `maxVisitedNodesMultiplier` (bees search further for their hive).
    pub max_visited_nodes_multiplier: f32,
    /// `WallClimberNavigation.pathToPosition`.
    path_to_position: Option<BlockPos>,
    pub is_stuck: bool,
    pub avoid_sun: bool,
    pub can_float: bool,
    pub can_open_doors: bool,
    pub can_pass_doors: bool,
    /// `canWalkOverFences` (camels).
    pub can_walk_over_fences: bool,
    /// `canStandOnFluid(lava)` (striders).
    pub stands_on_lava: bool,
    /// `canPathToTargetsBelowSurface` (copper golems moving items).
    pub can_path_below_surface: bool,
    /// The navigation does not tick (a bee pollinating).
    pub paused: bool,
}

impl Navigation {
    /// The navigation of a mob with the base follow range `follow_range`.
    pub fn new(follow_range: f64) -> Navigation {
        Navigation {
            kind: NavKind::Ground,
            variant: NavVariant::Default,
            path: None,
            speed_modifier: 0.0,
            tick: 0,
            last_stuck_check: 0,
            last_stuck_check_pos: Vec3::ZERO,
            timeout_cached_node: BlockPos::ZERO,
            timeout_timer: 0,
            last_timeout_check: 0,
            timeout_limit: 0.0,
            max_distance_to_waypoint: 0.5,
            has_delayed_recomputation: false,
            time_last_recompute: 0,
            target_pos: None,
            reach_range: 0,
            max_visited_nodes: vanillite_base::math::floor(follow_range * 16.0),
            required_path_length: 16.0,
            max_visited_nodes_multiplier: 1.0,
            path_to_position: None,
            is_stuck: false,
            avoid_sun: false,
            can_float: false,
            can_open_doors: false,
            can_pass_doors: true,
            can_walk_over_fences: false,
            stands_on_lava: false,
            can_path_below_surface: false,
            paused: false,
        }
    }

    /// `setRequiredPathLength`: also recomputes the node limit from the larger of the follow
    /// range and the length (`updatePathfinderMaxVisitedNodes`).
    pub fn set_required_path_length(&mut self, length: f32, follow_range: f64) {
        self.required_path_length = length;
        #[allow(clippy::cast_possible_truncation)]
        let max = (follow_range as f32).max(length);
        self.max_visited_nodes = vanillite_base::math::floor(f64::from(max * 16.0));
    }

    /// `isDone`.
    pub fn is_done(&self) -> bool {
        self.path.as_ref().is_none_or(Path::is_done)
    }

    /// `isInProgress`.
    pub fn is_in_progress(&self) -> bool {
        !self.is_done()
    }

    /// `stop`.
    pub fn stop(&mut self) {
        self.path = None;
    }

    /// `getTargetPos`.
    pub fn target_pos(&self) -> Option<BlockPos> {
        self.target_pos
    }

    /// `canNavigateGround` (ground, wall climber and amphibious navigation).
    pub fn can_navigate_ground(&self) -> bool {
        matches!(
            self.kind,
            NavKind::Ground | NavKind::WallClimber | NavKind::Amphibious
        )
    }

    /// `GoalUtils.hasGroundPathNavigation`: a `GroundPathNavigation` (doors).
    pub fn is_ground(&self) -> bool {
        matches!(self.kind, NavKind::Ground | NavKind::WallClimber)
    }

    /// `canFloat` of the node evaluator: water and amphibious navigation ignore `setCanFloat`.
    pub fn floats(&self) -> bool {
        self.can_float && !matches!(self.kind, NavKind::Water { .. } | NavKind::Amphibious)
    }

    fn reset_stuck_timeout(&mut self) {
        self.timeout_cached_node = BlockPos::ZERO;
        self.timeout_timer = 0;
        self.timeout_limit = 0.0;
        self.is_stuck = false;
    }
}

/// `PathNavigation.isStableDestination(pos)` of the mob's navigation.
pub fn is_stable_destination(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    let nav = &t.mob_ref().nav;
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let solid_below = || {
        t.level
            .block(below)
            .has(vanillite_data::block::flags::SOLID_RENDER)
    };
    match (nav.kind, nav.variant) {
        (NavKind::Water { .. }, _) => !t
            .level
            .block(pos)
            .has(vanillite_data::block::flags::SOLID_RENDER),
        (NavKind::Amphibious, NavVariant::Turtle { to_water: true }) => {
            t.level.block(pos).block() == vanillite_data::block::ids::WATER
        }
        (NavKind::Amphibious, _) | (NavKind::Flying, NavVariant::Bee) => {
            !t.level.block(below).is_air()
        }
        // `entityCanStandOn`: a full top face.
        (NavKind::Flying, _) => t.level.block(pos).misc().sturdy_full & 2 != 0,
        (_, NavVariant::Strider) => {
            t.level.block(pos).block() == vanillite_data::block::ids::LAVA || solid_below()
        }
        _ => solid_below(),
    }
}

/// `canUpdatePath` of the navigation kind.
fn can_update_path(t: &Tick<'_, '_>) -> bool {
    let m = t.mob_ref();
    let in_liquid = m.fluids.in_water() || m.fluids.in_lava();
    let passenger = t.ents.vehicle_of(t.id).is_some();
    match m.nav.kind {
        NavKind::Ground | NavKind::WallClimber => t.on_ground() || in_liquid || passenger,
        NavKind::Water { breaching } => breaching || in_liquid,
        NavKind::Amphibious => true,
        NavKind::Flying => m.nav.floats() && in_liquid || !passenger,
        NavKind::None => false,
    }
}

/// `getTempMobPos` of the navigation kind.
fn temp_mob_pos(t: &Tick<'_, '_>) -> Vec3 {
    let p = t.pos();
    let m = t.mob_ref();
    match m.nav.kind {
        NavKind::Water { .. } | NavKind::Amphibious => {
            let height = f64::from(t.core().dims().height);
            return Vec3::new(p.x, p.y + height * 0.5, p.z);
        }
        NavKind::Flying | NavKind::None => return p,
        NavKind::Ground | NavKind::WallClimber => {}
    }
    let y = if m.fluids.in_water() && m.nav.floats() {
        let mut surface = vanillite_base::math::floor(p.y);
        let mut steps = 0;
        loop {
            let state = t
                .level
                .block(BlockPos::containing(p.x, f64::from(surface), p.z));
            if !is_water(state) {
                break;
            }
            surface += 1;
            steps += 1;
            if steps > 16 {
                surface = vanillite_base::math::floor(p.y);
                break;
            }
        }
        surface
    } else {
        vanillite_base::math::floor(p.y + 0.5)
    };
    Vec3::new(p.x, f64::from(y), p.z)
}

/// The mob as the path finder sees it.
fn path_mob(t: &Tick<'_, '_>) -> PathMob {
    let core = t.core();
    let dims = core.dims();
    let m = t.mob_ref();
    let nav = &m.nav;
    let frog = nav.variant == NavVariant::Frog;
    let evaluator = match nav.kind {
        NavKind::Water { breaching } => Evaluator::Swim { breaching },
        NavKind::Amphibious => Evaluator::Amphibious {
            prefers_shallow: frog,
            frog,
        },
        NavKind::Flying => Evaluator::Fly,
        NavKind::Ground | NavKind::WallClimber | NavKind::None => Evaluator::Walk,
    };
    PathMob {
        evaluator,
        can_walk_over_fences: nav.can_walk_over_fences,
        stands_on_lava: nav.stands_on_lava,
        sea_level: t.cx.env.sea_level,
        pos: core.pos(),
        aabb: core.aabb(),
        width: dims.width,
        height: dims.height,
        on_ground: t.on_ground(),
        #[allow(clippy::cast_possible_truncation)]
        max_up_step: m.attrs.step_height as f32,
        max_fall_distance: super::living::max_fall_distance(t),
        in_water: m.fluids.in_water(),
        can_float: nav.floats(),
        can_open_doors: nav.can_open_doors,
        // `WaterBoundPathNavigation` cannot pass doors.
        can_pass_doors: nav.can_pass_doors && !matches!(nav.kind, NavKind::Water { .. }),
        home: super::fam::mount::creaking::creaking_home(m),
    }
}

/// `PathNavigation.createPath(targets, radiusOffset, above, reachRange, maxPathLength)`.
fn create_path(t: &mut Tick<'_, '_>, target: BlockPos, reach_range: i32) -> Option<Path> {
    create_path_limited(t, &[target], reach_range, None)
}

/// `createPath(targets, reachRange)`: a path to the closest reachable of `targets` (no
/// surface adjustment, like Vanilla's set variant).
pub fn create_path_to_any(
    t: &mut Tick<'_, '_>,
    targets: &[BlockPos],
    reach_range: i32,
) -> Option<Path> {
    t.as_nav_owner(|t| create_path_limited(t, targets, reach_range, None))
}

/// `createPath(pos, reachRange, maxPathLength)`: a path to `target` of at most
/// `max_path_length` (no surface adjustment).
pub fn create_path_within(
    t: &mut Tick<'_, '_>,
    target: BlockPos,
    reach_range: i32,
    max_path_length: f32,
) -> Option<Path> {
    t.as_nav_owner(|t| create_path_limited(t, &[target], reach_range, Some(max_path_length)))
}

fn create_path_limited(
    t: &mut Tick<'_, '_>,
    targets: &[BlockPos],
    reach_range: i32,
    max_path_length: Option<f32>,
) -> Option<Path> {
    if targets.is_empty() || t.mob_ref().nav.kind == NavKind::None {
        return None;
    }
    if t.pos().y < f64::from(t.level.min_y()) || !can_update_path(t) {
        return None;
    }
    {
        let nav = &t.mob_ref().nav;
        if let Some(p) = &nav.path
            && !p.is_done()
            && nav.target_pos.is_some_and(|tp| targets.contains(&tp))
        {
            return Some(p.clone());
        }
    }
    if t.cx.path_budget == 0 {
        return None;
    }
    let mob = path_mob(t);
    let m = t.mob_ref();
    #[allow(clippy::cast_possible_truncation)]
    let max_len = max_path_length
        .unwrap_or_else(|| (m.attrs.follow_range as f32).max(m.nav.required_path_length));
    let budget = i32::try_from(t.cx.path_budget).unwrap_or(i32::MAX);
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let max_nodes = (m.nav.max_visited_nodes as f32 * m.nav.max_visited_nodes_multiplier) as i32;
    let search = Search {
        max_path_length: max_len,
        reach_range,
        max_visited_nodes: max_nodes.min(budget),
        malus: super::kinds::mob_malus(m),
        min_y: t.level.min_y(),
    };
    let mut random = m.random.clone();
    let path =
        t.cx.finder
            .find_path(&t.level.view(), &mob, targets, search, &mut random);
    t.mob().random = random;
    let visited = t.cx.finder.take_visited();
    if t.cx.path_budget != u64::MAX {
        t.cx.path_budget = t.cx.path_budget.saturating_sub(visited);
    }
    if let Some(p) = &path {
        let nav = &mut t.mob().nav;
        nav.target_pos = Some(p.target);
        nav.reach_range = reach_range;
        nav.reset_stuck_timeout();
    }
    path
}

/// `GroundPathNavigation.findSurfacePosition`.
fn surface_position(t: &Tick<'_, '_>, mut pos: BlockPos) -> BlockPos {
    let level = t.level;
    if level.block(pos).is_air() {
        let mut y = pos.y - 1;
        while y >= level.min_y() && level.block(BlockPos::new(pos.x, y, pos.z)).is_air() {
            y -= 1;
        }
        if y >= level.min_y() {
            return BlockPos::new(pos.x, y + 1, pos.z);
        }
        y = pos.y + 1;
        while y <= level.max_y() && level.block(BlockPos::new(pos.x, y, pos.z)).is_air() {
            y += 1;
        }
        pos = BlockPos::new(pos.x, y, pos.z);
    }
    if !is_solid(level.block(pos)) {
        return pos;
    }
    let mut y = pos.y + 1;
    while y <= level.max_y() && is_solid(level.block(BlockPos::new(pos.x, y, pos.z))) {
        y += 1;
    }
    BlockPos::new(pos.x, y, pos.z)
}

/// `BlockState.isSolid` (legacy solidity).
pub fn is_solid(state: BlockState) -> bool {
    state.has(vanillite_data::block::flags::SOLID)
}

/// `getNavigation().createPath(pos, reachRange)`: ground navigation paths to the surface of
/// the column.
pub fn create_path_to(t: &mut Tick<'_, '_>, pos: BlockPos, reach_range: i32) -> Option<Path> {
    t.as_nav_owner(|t| create_path_to_here(t, pos, reach_range))
}

/// `createPath(pos, reachRange)` of the mob's own navigation.
fn create_path_to_here(t: &mut Tick<'_, '_>, pos: BlockPos, reach_range: i32) -> Option<Path> {
    let nav = &t.mob_ref().nav;
    if nav.kind == NavKind::None {
        return None;
    }
    let target = if nav.is_ground() {
        if !t.level.is_loaded(pos) {
            return None;
        }
        if nav.kind == NavKind::WallClimber {
            t.mob().nav.path_to_position = Some(pos);
        }
        if t.mob_ref().nav.can_path_below_surface {
            pos
        } else {
            surface_position(t, pos)
        }
    } else {
        pos
    };
    create_path(t, target, reach_range)
}

/// `getNavigation().moveTo(path, speedModifier)`.
pub fn move_to_path(t: &mut Tick<'_, '_>, path: Option<Path>, speed: f64) -> bool {
    t.as_nav_owner(|t| move_to_path_here(t, path, speed))
}

/// `moveTo(path, speedModifier)` of the mob's own navigation.
fn move_to_path_here(t: &mut Tick<'_, '_>, path: Option<Path>, speed: f64) -> bool {
    let Some(new) = path else {
        t.mob().nav.path = None;
        return false;
    };
    {
        let nav = &mut t.mob().nav;
        let same = nav.path.as_ref().is_some_and(|p| new.same_nodes(p));
        if !same {
            nav.path = Some(new);
        }
        if nav.is_done() {
            return false;
        }
    }
    trim_path(t);
    let pos = temp_mob_pos(t);
    let nav = &mut t.mob().nav;
    if nav.path.as_ref().is_none_or(|p| p.nodes.is_empty()) {
        return false;
    }
    nav.speed_modifier = speed;
    nav.last_stuck_check = nav.tick;
    nav.last_stuck_check_pos = pos;
    true
}

/// `getNavigation().moveTo(x, y, z, speedModifier)`.
pub fn move_to_pos(t: &mut Tick<'_, '_>, pos: Vec3, speed: f64) -> bool {
    t.as_nav_owner(|t| {
        let path = create_path_to_here(t, BlockPos::containing(pos.x, pos.y, pos.z), 1);
        move_to_path_here(t, path, speed)
    })
}

/// `moveTo(entity, reachRange, speedModifier)` (`moveTo(entity, speed)` uses reach range 1); a
/// wall climber without a path climbs straight towards the entity.
pub fn move_to_entity(t: &mut Tick<'_, '_>, target: Vec3, reach: i32, speed: f64) -> bool {
    t.as_nav_owner(|t| move_to_entity_here(t, target, reach, speed))
}

fn move_to_entity_here(t: &mut Tick<'_, '_>, target: Vec3, reach: i32, speed: f64) -> bool {
    let block = BlockPos::containing(target.x, target.y, target.z);
    let path = create_path_to_here(t, block, reach);
    if path.is_none() && t.mob_ref().nav.kind == NavKind::WallClimber {
        let nav = &mut t.mob().nav;
        nav.path_to_position = Some(block);
        nav.speed_modifier = speed;
        return true;
    }
    path.is_some() && move_to_path_here(t, path, speed)
}

/// `trimPath`: a node in a cauldron moves up (and the next with it); ground navigation with
/// `avoidSun` ends the path before the first node under the open sky.
fn trim_path(t: &mut Tick<'_, '_>) {
    {
        let level = t.level;
        let nav = &mut t.mob().nav;
        if let Some(path) = &mut nav.path {
            let n = path.nodes.len();
            for i in 0..n {
                let node = path.nodes[i];
                let state = level.block(node.pos());
                if is_cauldron(state) {
                    path.nodes[i].y = node.y + 1;
                    if i + 1 < n && node.y >= path.nodes[i + 1].y {
                        path.nodes[i + 1].y = node.y + 1;
                    }
                }
            }
        }
    }
    if !t.mob_ref().nav.avoid_sun || !t.mob_ref().nav.is_ground() {
        return;
    }
    let p = t.pos();
    if can_see_sky(t.level, BlockPos::containing(p.x, p.y + 0.5, p.z)) {
        return;
    }
    let level = t.level;
    let nav = &mut t.mob().nav;
    if let Some(path) = &mut nav.path
        && let Some(i) = path.nodes.iter().position(|n| can_see_sky(level, n.pos()))
    {
        path.nodes.truncate(i);
    }
}

/// `#minecraft:cauldrons`.
fn is_cauldron(state: BlockState) -> bool {
    matches!(
        state.block().name(),
        "minecraft:cauldron"
            | "minecraft:water_cauldron"
            | "minecraft:lava_cauldron"
            | "minecraft:powder_snow_cauldron"
    )
}

/// `recomputePath`.
fn recompute_path(t: &mut Tick<'_, '_>) {
    let time = t.game_time();
    if time - t.mob_ref().nav.time_last_recompute <= 20 || !can_update_path(t) {
        t.mob().nav.has_delayed_recomputation = true;
        return;
    }
    let Some(target) = t.mob_ref().nav.target_pos else {
        return;
    };
    let reach = t.mob_ref().nav.reach_range;
    t.mob().nav.path = None;
    let path = create_path_to_here(t, target, reach);
    let nav = &mut t.mob().nav;
    nav.path = path;
    nav.time_last_recompute = time;
    nav.has_delayed_recomputation = false;
}

/// `ServerLevel.sendBlockUpdated` for the navigating mobs after the collision shape at `pos`
/// changed: every mob whose remaining path passes near it (`shouldRecomputePath`) recomputes
/// its path (`recomputePath`): at once if its last computation is more than 20 ticks ago,
/// else on its next navigation tick.
pub fn block_shape_changed(level: &mut crate::level::Level, pos: BlockPos) {
    let mut due: Vec<vanillite_world::entity::EntityId> = Vec::new();
    {
        let ents = &level.entities;
        for (id, m) in ents.mobs.iter() {
            let nav = &m.nav;
            if nav.has_delayed_recomputation {
                continue;
            }
            let Some(path) = &nav.path else {
                continue;
            };
            let Some(end) = path.nodes.last() else {
                continue;
            };
            if path.is_done() {
                continue;
            }
            let Some(p) = ents.store.get(id).map(|e| e.pos()) else {
                continue;
            };
            let middle = Vec3::new(
                (f64::from(end.x) + p.x) / 2.0,
                (f64::from(end.y) + p.y) / 2.0,
                (f64::from(end.z) + p.z) / 2.0,
            );
            #[allow(clippy::cast_precision_loss)]
            let distance = (path.nodes.len() - path.next) as f64;
            let dx = f64::from(pos.x) + 0.5 - middle.x;
            let dy = f64::from(pos.y) + 0.5 - middle.y;
            let dz = f64::from(pos.z) + 0.5 - middle.z;
            if dx * dx + dy * dy + dz * dz < distance * distance {
                due.push(id);
            }
        }
    }
    if due.is_empty() {
        return;
    }
    let time = level.game_time();
    // `recomputePath` within 20 ticks of the last one only marks it delayed.
    due.retain(|&id| {
        let Some(m) = level.entities.mob_mut(id) else {
            return false;
        };
        if time - m.nav.time_last_recompute > 20 {
            return m.nav.target_pos.is_some();
        }
        m.nav.has_delayed_recomputation = true;
        false
    });
    if due.is_empty() {
        return;
    }
    crate::level::mob_env::with_mob_env(level, |level, ents, env, cx| {
        cx.path_budget = u64::MAX;
        for id in due {
            let mut t = Tick {
                level,
                ents: &mut *ents,
                env: &mut *env,
                cx: &mut *cx,
                id,
            };
            let Some(target) = t.mob_ref().nav.target_pos else {
                continue;
            };
            let reach = t.mob_ref().nav.reach_range;
            t.mob().nav.path = None;
            let path = create_path_to_here(&mut t, target, reach);
            let nav = &mut t.mob().nav;
            nav.path = path;
            nav.time_last_recompute = time;
            nav.has_delayed_recomputation = false;
        }
    });
}

/// `PathNavigation.tick` of the mob's navigation kind.
pub fn tick(t: &mut Tick<'_, '_>) {
    let kind = t.mob_ref().nav.kind;
    if kind == NavKind::None || t.mob_ref().nav.paused {
        return;
    }
    // `CreakingPathNavigation.tick`: nothing while it is frozen.
    if t.mob_ref().kind == super::Kind::Creaking
        && !super::fam::mount::creaking::can_move(t.mob_ref())
    {
        return;
    }
    if kind == NavKind::WallClimber && t.mob_ref().nav.is_done() {
        wall_climber_idle(t);
        return;
    }
    t.mob().nav.tick += 1;
    if t.mob_ref().nav.has_delayed_recomputation {
        recompute_path(t);
    }
    if t.mob_ref().nav.is_done() {
        return;
    }
    let width = t.core().dims().width;
    if can_update_path(t) {
        follow_the_path(t);
    } else if kind == NavKind::Flying {
        // `FlyingPathNavigation.tick`: advance when inside the next node's block.
        let b = t.block_pos();
        let nav = &mut t.mob().nav;
        if let Some(path) = &mut nav.path
            && !path.is_done()
        {
            let pos = path.next_entity_pos(width);
            if b.x == vanillite_base::math::floor(pos.x)
                && b.y == vanillite_base::math::floor(pos.y)
                && b.z == vanillite_base::math::floor(pos.z)
            {
                path.next += 1;
            }
        }
    } else {
        let mob_pos = temp_mob_pos(t);
        let on_ground = t.on_ground();
        let nav = &mut t.mob().nav;
        if let Some(path) = &mut nav.path
            && !path.is_done()
        {
            let pos = path.next_entity_pos(width);
            if mob_pos.y > pos.y
                && !on_ground
                && vanillite_base::math::floor(mob_pos.x) == vanillite_base::math::floor(pos.x)
                && vanillite_base::math::floor(mob_pos.z) == vanillite_base::math::floor(pos.z)
            {
                path.next += 1;
            }
        }
    }
    let target = {
        let nav = &t.mob_ref().nav;
        match &nav.path {
            Some(p) if !p.is_done() => Some(p.next_entity_pos(width)),
            _ => None,
        }
    };
    if let Some(target) = target {
        let y = match kind {
            NavKind::Ground | NavKind::WallClimber => ground_y(t, target),
            _ => target.y,
        };
        let speed = t.mob_ref().nav.speed_modifier;
        t.mob()
            .move_ctl
            .set_wanted_position(Vec3::new(target.x, y, target.z), speed);
    }
}

/// `WallClimberNavigation.tick` without a path: climbs straight to `pathToPosition`.
fn wall_climber_idle(t: &mut Tick<'_, '_>) {
    let Some(to) = t.mob_ref().nav.path_to_position else {
        return;
    };
    let p = t.pos();
    let width = f64::from(t.core().dims().width);
    let near = |b: BlockPos| {
        let c = Vec3::new(
            f64::from(b.x) + 0.5,
            f64::from(b.y) + 0.5,
            f64::from(b.z) + 0.5,
        );
        c.distance_to_sqr(p) < width * width
    };
    let above_column = BlockPos::containing(f64::from(to.x), p.y, f64::from(to.z));
    if !near(to) && (p.y <= f64::from(to.y) || !near(above_column)) {
        let speed = t.mob_ref().nav.speed_modifier;
        let at = Vec3::new(f64::from(to.x), f64::from(to.y), f64::from(to.z));
        t.mob().move_ctl.set_wanted_position(at, speed);
    } else {
        t.mob().nav.path_to_position = None;
    }
}

/// `getGroundY`.
fn ground_y(t: &Tick<'_, '_>, target: Vec3) -> f64 {
    let pos = BlockPos::containing(target.x, target.y, target.z);
    if t.level
        .block(BlockPos::new(pos.x, pos.y - 1, pos.z))
        .is_air()
    {
        target.y
    } else {
        path::floor_level(t.level, pos)
    }
}

/// `canCutCorner`.
fn can_cut_corner(nav: &Navigation, kind: PathType) -> bool {
    if nav.variant == NavVariant::Frog && kind == PathType::WaterBorder {
        return false;
    }
    kind != PathType::FireInNeighbor
        && kind != PathType::DamagingInNeighbor
        && kind != PathType::WalkableDoor
}

/// `canMoveDirectly(start, stop)`: water, amphibious (in a liquid) and flying navigation move
/// straight when nothing collides (`isClearForMovementBetween`).
fn can_move_directly(t: &Tick<'_, '_>, start: Vec3, stop: Vec3) -> bool {
    let m = t.mob_ref();
    let fluids = match m.nav.kind {
        NavKind::Water { .. } => false,
        NavKind::Amphibious if m.fluids.in_water() || m.fluids.in_lava() => false,
        NavKind::Flying => m.nav.variant != NavVariant::BabyHappyGhast,
        _ => return false,
    };
    let height = f64::from(t.core().dims().height);
    let to = Vec3::new(stop.x, stop.y + height * 0.5, stop.z);
    let blocks = t.level as &dyn super::physics::Blocks;
    if fluids {
        super::physics::clip_fluids(blocks, start, to).is_none()
    } else {
        super::physics::clip(blocks, start, to).is_none()
    }
}

/// `followThePath`.
fn follow_the_path(t: &mut Tick<'_, '_>) {
    let mob_pos = temp_mob_pos(t);
    let width = t.core().dims().width;
    let pos = t.pos();
    let max_vertical = if matches!(t.mob_ref().nav.kind, NavKind::Water { .. }) {
        0.5
    } else {
        1.0
    };
    let (close, cut, direct_to) = {
        let nav = &mut t.mob().nav;
        nav.max_distance_to_waypoint = if width > 0.75 {
            width / 2.0
        } else {
            0.75 - width / 2.0
        };
        let Some(path) = &nav.path else {
            return;
        };
        let Some(node) = path.nodes.get(path.next) else {
            return;
        };
        let max = f64::from(nav.max_distance_to_waypoint);
        let close = (pos.x - (f64::from(node.x) + 0.5)).abs() < max
            && (pos.z - (f64::from(node.z) + 0.5)).abs() < max
            && (pos.y - f64::from(node.y)).abs() < max_vertical;
        (
            close,
            can_cut_corner(nav, node.kind),
            path.next_entity_pos(width),
        )
    };
    let advance = close || cut && should_target_next_node(t, mob_pos, direct_to);
    if advance && let Some(path) = &mut t.mob().nav.path {
        path.next += 1;
    }
    do_stuck_detection(t, mob_pos);
}

/// `shouldTargetNextNodeInDirection`.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn should_target_next_node(t: &Tick<'_, '_>, mob: Vec3, next_entity_pos: Vec3) -> bool {
    let Some(path) = &t.mob_ref().nav.path else {
        return false;
    };
    if path.next + 1 >= path.nodes.len() {
        return false;
    }
    let bottom =
        |n: &path::PathNode| Vec3::new(f64::from(n.x) + 0.5, f64::from(n.y), f64::from(n.z) + 0.5);
    let current = bottom(&path.nodes[path.next]);
    if mob.distance_to_sqr(current) >= 4.0 {
        return false;
    }
    if can_move_directly(t, mob, next_entity_pos) {
        return true;
    }
    let next = bottom(&path.nodes[path.next + 1]);
    let to_current = current - mob;
    let to_next = next - mob;
    let cur_sqr = to_current.length_sqr();
    let next_sqr = to_next.length_sqr();
    if !(next_sqr < cur_sqr) && !(cur_sqr < 0.5) {
        return false;
    }
    to_next.normalize().dot(to_current.normalize()) < 0.0
}

/// `doStuckDetection`.
fn do_stuck_detection(t: &mut Tick<'_, '_>, mob_pos: Vec3) {
    let time = t.game_time();
    let m = t.mob();
    let speed = m.speed;
    let nav = &mut m.nav;
    if nav.tick - nav.last_stuck_check > 100 {
        let effective = if speed >= 1.0 { speed } else { speed * speed };
        let threshold = f64::from(effective * 100.0 * 0.25);
        if mob_pos.distance_to_sqr(nav.last_stuck_check_pos) < threshold * threshold {
            nav.is_stuck = true;
            nav.stop();
        } else {
            nav.is_stuck = false;
        }
        nav.last_stuck_check = nav.tick;
        nav.last_stuck_check_pos = mob_pos;
    }
    let node = match &nav.path {
        Some(p) if !p.is_done() => p.nodes[p.next].pos(),
        _ => return,
    };
    if node == nav.timeout_cached_node {
        nav.timeout_timer += time - nav.last_timeout_check;
    } else {
        nav.timeout_cached_node = node;
        let center = Vec3::new(
            f64::from(node.x) + 0.5,
            f64::from(node.y),
            f64::from(node.z) + 0.5,
        );
        let dist = mob_pos.distance_to_sqr(center).sqrt();
        nav.timeout_limit = if speed > 0.0 {
            dist / f64::from(speed) * 20.0
        } else {
            0.0
        };
    }
    #[allow(clippy::cast_precision_loss)]
    let timer = nav.timeout_timer as f64;
    if nav.timeout_limit > 0.0 && timer > nav.timeout_limit * 3.0 {
        nav.reset_stuck_timeout();
        nav.stop();
    }
    nav.last_timeout_check = time;
}

/// `MoveControl.Operation`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Operation {
    #[default]
    Wait,
    MoveTo,
    Strafe,
    Jumping,
}

/// `MoveControl`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MoveControl {
    /// The control type.
    pub kind: MoveKind,
    pub wanted: Vec3,
    pub speed_modifier: f64,
    pub strafe_forwards: f32,
    pub strafe_right: f32,
    pub operation: Operation,
    /// `RabbitMoveControl`'s state; `None` for other controls.
    pub rabbit: Option<RabbitMove>,
}

/// `RabbitMoveControl`: the speed of the next hop and whether the rabbit is in water.
#[derive(Clone, Copy, Debug, Default)]
pub struct RabbitMove {
    /// `nextJumpSpeed`: the last positive wanted speed.
    pub next_jump_speed: f64,
    /// `isInWater()`, kept current by the fluid update.
    pub in_water: bool,
}

impl MoveControl {
    /// `setWantedPosition` (`RabbitMoveControl`: 1.5 in water, a positive speed is the next
    /// hop's).
    pub fn set_wanted_position(&mut self, pos: Vec3, mut speed: f64) {
        if let Some(r) = &mut self.rabbit {
            if r.in_water {
                speed = 1.5;
            }
            if speed > 0.0 {
                r.next_jump_speed = speed;
            }
        }
        self.wanted = pos;
        self.speed_modifier = speed;
        if self.operation != Operation::Jumping {
            self.operation = Operation::MoveTo;
        }
    }

    /// `strafe`.
    pub fn strafe(&mut self, forwards: f32, right: f32) {
        self.operation = Operation::Strafe;
        self.strafe_forwards = forwards;
        self.strafe_right = right;
        self.speed_modifier = 0.25;
    }

    /// `hasWanted`.
    pub fn has_wanted(&self) -> bool {
        self.operation == Operation::MoveTo
    }
}

/// `MoveControl.rotlerp`: turns at most `max` degrees, wrapped into `[0, 360]`.
pub fn rotlerp(a: f32, b: f32, max: f32) -> f32 {
    let diff = wrap_degrees_f(b - a).clamp(-max, max);
    let mut r = a + diff;
    if r < 0.0 {
        r += 360.0;
    } else if r > 360.0 {
        r -= 360.0;
    }
    r
}

/// `Entity.setXRot`: the pitch modulo 360 clamped to `[-90, 90]` (a pitch `rotlerp` wrapped
/// to 350 becomes 90).
pub fn clamp_x_rot(x_rot: f32) -> f32 {
    (x_rot % 360.0).clamp(-90.0, 90.0)
}

/// `getAttributeValue(MOVEMENT_SPEED)`: sprinting mobs (cats and ocelots after their prey or
/// fleeing) move 30% faster (`SPEED_MODIFIER_SPRINTING`).
pub fn movement_speed(t: &Tick<'_, '_>) -> f64 {
    let base = t.mob_ref().attrs.movement_speed;
    if t.core()
        .flags()
        .contains(vanillite_world::entity::EntityFlags::SPRINTING)
    {
        base * (1.0 + f64::from(0.3_f32))
    } else {
        base
    }
}

/// `MoveControl.tick`.
/// `Mob.setSpeed`: the speed is also the forward input.
fn set_speed(m: &mut super::MobData, speed: f32) {
    m.speed = speed;
    m.zza = speed;
}

pub fn tick_move_control(t: &mut Tick<'_, '_>) {
    match t.mob_ref().move_ctl.kind {
        MoveKind::Custom => {
            if super::fam::move_control(t) {
                return;
            }
        }
        MoveKind::Flying { max_turn, hovers } => {
            tick_flying_move_control(t, max_turn, hovers);
            return;
        }
        MoveKind::SmoothSwimming {
            max_turn_x,
            max_turn_y,
            in_water_speed,
            outside_water_speed,
            apply_gravity,
        } => {
            tick_smooth_swimming_move_control(
                t,
                max_turn_x,
                max_turn_y,
                in_water_speed,
                outside_water_speed,
                apply_gravity,
            );
            return;
        }
        MoveKind::Default => {}
    }
    let speed_attr = movement_speed(t);
    let ctl = t.mob_ref().move_ctl;
    match ctl.operation {
        Operation::Strafe => {
            #[allow(clippy::cast_possible_truncation)]
            let speed = speed_attr as f32;
            #[allow(clippy::cast_possible_truncation)]
            let modified = ctl.speed_modifier as f32 * speed;
            let mut xa = ctl.strafe_forwards;
            let mut za = ctl.strafe_right;
            let mut dist = (xa * xa + za * za).sqrt();
            if dist < 1.0 {
                dist = 1.0;
            }
            dist = modified / dist;
            xa *= dist;
            za *= dist;
            let rad = f64::from(t.core().rot().y_rot * super::DEG_TO_RAD);
            let sin = vanillite_base::math::sin(rad);
            let cos = vanillite_base::math::cos(rad);
            let dx = xa * cos - za * sin;
            let dz = za * cos + xa * sin;
            let p = t.pos();
            let at = BlockPos::containing(p.x + f64::from(dx), p.y.floor(), p.z + f64::from(dz));
            let walkable = path::path_type_static(t.level, t.level.min_y(), at.x, at.y, at.z)
                == PathType::Walkable;
            let m = t.mob();
            if !walkable {
                m.move_ctl.strafe_forwards = 1.0;
                m.move_ctl.strafe_right = 0.0;
            }
            m.speed = modified;
            m.zza = m.move_ctl.strafe_forwards;
            m.xxa = m.move_ctl.strafe_right;
            m.move_ctl.operation = Operation::Wait;
        }
        Operation::MoveTo => {
            t.mob().move_ctl.operation = Operation::Wait;
            let p = t.pos();
            let xd = ctl.wanted.x - p.x;
            let zd = ctl.wanted.z - p.z;
            let yd = ctl.wanted.y - p.y;
            let dd = xd * xd + yd * yd + zd * zd;
            if dd < f64::from(2.500_000_3e-7_f32) {
                t.mob().zza = 0.0;
                return;
            }
            #[allow(clippy::cast_possible_truncation)]
            let y_rot_d = atan2_deg(zd, xd) as f32 - 90.0;
            let mut rot = t.core().rot();
            rot.y_rot = rotlerp(rot.y_rot, y_rot_d, 90.0);
            t.core_mut().set_rot(rot);
            let width = t.core().dims().width;
            let step = t.mob_ref().attrs.step_height;
            #[allow(clippy::cast_possible_truncation)]
            set_speed(t.mob(), (ctl.speed_modifier * speed_attr) as f32);
            let pos = t.block_pos();
            let state = t.level.block(pos);
            let shape = state.collision_shape();
            let top = shape.iter().map(|b| b[4]).fold(f64::NEG_INFINITY, f64::max);
            let class = class_of(state);
            let blocked = !shape.is_empty()
                && p.y < top + f64::from(pos.y)
                && class != Class::Door
                && class != Class::Fence;
            if yd > step && xd * xd + zd * zd < f64::from(width.max(1.0)) || blocked {
                t.mob().jump = true;
                t.mob().move_ctl.operation = Operation::Jumping;
            }
        }
        Operation::Jumping => {
            #[allow(clippy::cast_possible_truncation)]
            set_speed(t.mob(), (ctl.speed_modifier * speed_attr) as f32);
            let fluid = {
                let m = t.mob_ref();
                m.fluids.in_water() || m.fluids.in_lava()
            };
            if t.on_ground() || fluid {
                t.mob().move_ctl.operation = Operation::Wait;
            }
        }
        Operation::Wait => {
            t.mob().zza = 0.0;
        }
    }
}

/// `Mth.atan2(y, x) * Mth.RAD_TO_DEG` as a float (the controls' angle maths; decompilers
/// print the constant as `180.0F / (float) Math.PI`).
#[allow(clippy::cast_possible_truncation)]
fn atan2_deg_f(y: f64, x: f64) -> f32 {
    atan2_deg(y, x) as f32
}

/// `FlyingMoveControl.tick`.
fn tick_flying_move_control(t: &mut Tick<'_, '_>, max_turn: i32, hovers: bool) {
    use vanillite_world::entity::EntityFlags;
    let ctl = t.mob_ref().move_ctl;
    if ctl.operation != Operation::MoveTo {
        if !hovers {
            t.core_mut().set_flags(EntityFlags::NO_GRAVITY, false);
        }
        let m = t.mob();
        m.yya = 0.0;
        m.zza = 0.0;
        return;
    }
    t.mob().move_ctl.operation = Operation::Wait;
    t.core_mut().set_flags(EntityFlags::NO_GRAVITY, true);
    let p = t.pos();
    let (xd, yd, zd) = (ctl.wanted.x - p.x, ctl.wanted.y - p.y, ctl.wanted.z - p.z);
    let dd = xd * xd + yd * yd + zd * zd;
    if dd < f64::from(2.500_000_3e-7_f32) {
        let m = t.mob();
        m.yya = 0.0;
        m.zza = 0.0;
        return;
    }
    let y_rot_d = atan2_deg_f(zd, xd) - 90.0;
    let mut rot = t.core().rot();
    rot.y_rot = rotlerp(rot.y_rot, y_rot_d, 90.0);
    let attrs = t.mob_ref().attrs;
    let base = if t.on_ground() {
        attrs.movement_speed
    } else {
        attrs.flying_speed
    };
    #[allow(clippy::cast_possible_truncation)]
    let speed = (ctl.speed_modifier * base) as f32;
    // `Mob.setSpeed` also sets the forward input.
    {
        let m = t.mob();
        m.speed = speed;
        m.zza = speed;
    }
    let sd = (xd * xd + zd * zd).sqrt();
    if yd.abs() > f64::from(1.0e-5_f32) || sd.abs() > f64::from(1.0e-5_f32) {
        let x_rot_d = -atan2_deg_f(yd, sd);
        #[allow(clippy::cast_precision_loss)]
        let max = max_turn as f32;
        rot.x_rot = clamp_x_rot(rotlerp(rot.x_rot, x_rot_d, max));
        t.mob().yya = if yd > 0.0 { speed } else { -speed };
    }
    t.core_mut().set_rot(rot);
}

/// `SmoothSwimmingMoveControl.tick`.
fn tick_smooth_swimming_move_control(
    t: &mut Tick<'_, '_>,
    max_turn_x: i32,
    max_turn_y: i32,
    in_water_speed: f32,
    outside_water_speed: f32,
    apply_gravity: bool,
) {
    let in_water = t.mob_ref().fluids.in_water();
    if apply_gravity && in_water {
        let v = t.velocity();
        t.set_velocity(Vec3::new(v.x, v.y + 0.005, v.z));
    }
    let ctl = t.mob_ref().move_ctl;
    if ctl.operation != Operation::MoveTo || t.nav_ref().is_done() {
        let m = t.mob();
        m.speed = 0.0;
        m.xxa = 0.0;
        m.yya = 0.0;
        m.zza = 0.0;
        return;
    }
    let p = t.pos();
    let (xd, yd, zd) = (ctl.wanted.x - p.x, ctl.wanted.y - p.y, ctl.wanted.z - p.z);
    let dd = xd * xd + yd * yd + zd * zd;
    if dd < f64::from(2.500_000_3e-7_f32) {
        t.mob().zza = 0.0;
        return;
    }
    let y_rot_d = atan2_deg_f(zd, xd) - 90.0;
    let mut rot = t.core().rot();
    #[allow(clippy::cast_precision_loss)]
    {
        rot.y_rot = rotlerp(rot.y_rot, y_rot_d, max_turn_y as f32);
    }
    rot.y_head_rot = rot.y_rot;
    t.mob().y_body_rot = rot.y_rot;
    #[allow(clippy::cast_possible_truncation)]
    let speed = (ctl.speed_modifier * movement_speed(t)) as f32;
    if in_water {
        // `setSpeed` sets the forward input; it is overwritten below.
        t.mob().speed = speed * in_water_speed;
        let horizontal = (xd * xd + zd * zd).sqrt();
        if yd.abs() > f64::from(1.0e-5_f32) || horizontal.abs() > f64::from(1.0e-5_f32) {
            #[allow(clippy::cast_precision_loss)]
            let max = max_turn_x as f32;
            let x_rot_d = wrap_degrees_f(-atan2_deg_f(yd, horizontal)).clamp(-max, max);
            rot.x_rot = rotate_towards(rot.x_rot, x_rot_d, 5.0);
        }
        let rad = rot.x_rot * (std::f32::consts::PI / 180.0);
        let cos = vanillite_base::math::cos(f64::from(rad));
        let sin = vanillite_base::math::sin(f64::from(rad));
        let m = t.mob();
        m.zza = cos * speed;
        m.yya = -sin * speed;
    } else {
        let left = wrap_degrees_f(rot.y_rot - y_rot_d).abs();
        let factor = 1.0 - ((left - 10.0) / 50.0).clamp(0.0, 1.0);
        let m = t.mob();
        let s = speed * outside_water_speed * factor;
        m.speed = s;
        m.zza = s;
    }
    t.core_mut().set_rot(rot);
}

/// `LookControl`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LookControl {
    /// The control type.
    pub kind: LookKind,
    pub wanted: Vec3,
    pub y_max_rot_speed: f32,
    pub x_max_rot_angle: f32,
    pub cooldown: i32,
}

impl LookControl {
    /// `setLookAt(x, y, z, yMaxRotSpeed, xMaxRotAngle)`.
    pub fn set_look_at(&mut self, at: Vec3, y_max: f32, x_max: f32) {
        self.wanted = at;
        self.y_max_rot_speed = y_max;
        self.x_max_rot_angle = x_max;
        self.cooldown = 2;
    }

    /// `isLookingAtTarget`.
    pub fn is_looking(&self) -> bool {
        self.cooldown > 0
    }
}

/// `setLookAt(x, y, z)` with the mob's head speed (`getHeadRotSpeed`) and pitch limit
/// (`getMaxHeadXRot`).
pub fn look_at(t: &mut Tick<'_, '_>, at: Vec3) {
    let y_max = t.mob_ref().kind.head_rot_speed();
    let x_max = super::kinds::max_head_x_rot(t.mob_ref());
    t.mob().look.set_look_at(at, y_max, x_max);
}

/// `LookControl.tick`.
pub fn tick_look_control(t: &mut Tick<'_, '_>) {
    match t.mob_ref().look.kind {
        LookKind::Custom => {
            if super::fam::look_control(t) {
                return;
            }
        }
        LookKind::SmoothSwimming { max_y_rot } => {
            tick_smooth_swimming_look_control(t, max_y_rot);
            return;
        }
        LookKind::Default => {}
    }
    default_look_tick(t, true);
}

/// The body of `LookControl.tick`; `reset_x` is `resetXRotOnTick` (the pitch drops to 0 before
/// the mob looks at anything).
pub fn default_look_tick(t: &mut Tick<'_, '_>, reset_x: bool) {
    let p = t.pos();
    let eye = t.eye_y();
    let mut rot = t.core().rot();
    if reset_x {
        rot.x_rot = 0.0;
    }
    let body = t.mob_ref().y_body_rot;
    let look = t.mob_ref().look;
    if look.cooldown > 0 {
        t.mob().look.cooldown -= 1;
        let xd = look.wanted.x - p.x;
        let yd = look.wanted.y - eye;
        let zd = look.wanted.z - p.z;
        let tiny = f64::from(1.0e-5_f32);
        if zd.abs() > tiny || xd.abs() > tiny {
            let y_rot_d = atan2_deg_f(zd, xd) - 90.0;
            rot.y_head_rot = rotate_towards(rot.y_head_rot, y_rot_d, look.y_max_rot_speed);
        }
        let sd = (xd * xd + zd * zd).sqrt();
        if yd.abs() > tiny || sd.abs() > tiny {
            let x_rot_d = -atan2_deg_f(yd, sd);
            rot.x_rot = rotate_towards(rot.x_rot, x_rot_d, look.x_max_rot_angle);
        }
    } else {
        rot.y_head_rot = rotate_towards(rot.y_head_rot, body, 10.0);
    }
    if !t.nav_ref().is_done() {
        let max = super::kinds::max_head_y_rot(t.mob_ref());
        rot.y_head_rot = rotate_if_necessary(rot.y_head_rot, body, max);
    }
    t.core_mut().set_rot(rot);
}

/// `SmoothSwimmingLookControl.tick`: the head tilts 20° sideways and 10° down towards the
/// target; the body follows the head beyond `max_y_rot`.
fn tick_smooth_swimming_look_control(t: &mut Tick<'_, '_>, max_y_rot: i32) {
    let p = t.pos();
    let eye = t.eye_y();
    let mut rot = t.core().rot();
    let look = t.mob_ref().look;
    if look.cooldown > 0 {
        t.mob().look.cooldown -= 1;
        let xd = look.wanted.x - p.x;
        let yd = look.wanted.y - eye;
        let zd = look.wanted.z - p.z;
        let tiny = f64::from(1.0e-5_f32);
        if zd.abs() > tiny || xd.abs() > tiny {
            let y_rot_d = atan2_deg_f(zd, xd) - 90.0;
            rot.y_head_rot = rotate_towards(rot.y_head_rot, y_rot_d + 20.0, look.y_max_rot_speed);
        }
        let sd = (xd * xd + zd * zd).sqrt();
        if yd.abs() > tiny || sd.abs() > tiny {
            let x_rot_d = -atan2_deg_f(yd, sd);
            rot.x_rot = rotate_towards(rot.x_rot, x_rot_d + 10.0, look.x_max_rot_angle);
        }
    } else {
        if t.nav_ref().is_done() {
            rot.x_rot = rotate_towards(rot.x_rot, 0.0, 5.0);
        }
        let body = t.mob_ref().y_body_rot;
        rot.y_head_rot = rotate_towards(rot.y_head_rot, body, look.y_max_rot_speed);
    }
    let body = t.mob_ref().y_body_rot;
    let diff = wrap_degrees_f(rot.y_head_rot - body);
    #[allow(clippy::cast_precision_loss)]
    let max = max_y_rot as f32;
    if diff < -max {
        t.mob().y_body_rot -= 4.0;
    } else if diff > max {
        t.mob().y_body_rot += 4.0;
    }
    t.core_mut().set_rot(rot);
}

/// `BodyRotationControl`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BodyRotation {
    head_stable_time: i32,
    last_stable_y_head_rot: f32,
}

/// `BodyRotationControl.clientTick` (run by `Mob.tickHeadTurn` on the server too).
pub fn tick_body_rotation(t: &mut Tick<'_, '_>) {
    let core = t.core();
    let moved = {
        let d = core.pos() - core.old_pos();
        d.x * d.x + d.z * d.z > f64::from(2.500_000_3e-7_f32)
    };
    let mut rot = core.rot();
    let max = super::kinds::max_head_y_rot(t.mob_ref());
    let m = t.mob();
    if moved {
        m.y_body_rot = rot.y_rot;
        rot.y_head_rot = rotate_if_necessary(rot.y_head_rot, m.y_body_rot, max);
        m.body.last_stable_y_head_rot = rot.y_head_rot;
        m.body.head_stable_time = 0;
    } else if (rot.y_head_rot - m.body.last_stable_y_head_rot).abs() > 15.0 {
        m.body.head_stable_time = 0;
        m.body.last_stable_y_head_rot = rot.y_head_rot;
        m.y_body_rot = rotate_if_necessary(m.y_body_rot, rot.y_head_rot, max);
    } else {
        m.body.head_stable_time += 1;
        if m.body.head_stable_time > 10 {
            #[allow(clippy::cast_precision_loss)]
            let fraction = ((m.body.head_stable_time - 10) as f32 / 10.0).clamp(0.0, 1.0);
            let remaining = max * (1.0 - fraction);
            m.y_body_rot = rotate_if_necessary(m.y_body_rot, rot.y_head_rot, remaining);
        }
    }
    t.core_mut().set_rot(rot);
}
