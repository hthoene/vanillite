//! Path finding like Vanilla's `PathFinder` with the `WalkNodeEvaluator`: A* over block
//! nodes with path type costs, jumping up one block, falling down to the ground and diagonal
//! moves.

use std::sync::OnceLock;

use vanillite_base::hash::FxHashMap;
use vanillite_base::math::floor;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::{BlockState, STATE_COUNT, flags, ids};
use vanillite_worldgen::behaviour::{Class, class_of_block};

use super::is_water;
use super::physics::{Blocks, no_collision};

/// `PathType` with its default malus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PathType {
    Blocked,
    Open,
    Walkable,
    WalkableDoor,
    Trapdoor,
    PowderSnow,
    OnTopOfPowderSnow,
    Fence,
    Lava,
    Water,
    WaterBorder,
    Rail,
    UnpassableRail,
    FireInNeighbor,
    Fire,
    DamagingInNeighbor,
    Damaging,
    DoorOpen,
    DoorWoodClosed,
    DoorIronClosed,
    Breach,
    Leaves,
    StickyHoney,
    Cocoa,
    DamageCautious,
    OnTopOfTrapdoor,
    BigMobsCloseToDanger,
}

impl PathType {
    /// `getMalus()`: the default cost.
    pub fn malus(self) -> f32 {
        use PathType::*;
        match self {
            Blocked | PowderSnow | Fence | Lava | UnpassableRail | Damaging | DoorWoodClosed
            | DoorIronClosed | Leaves => -1.0,
            Water | WaterBorder | FireInNeighbor | DamagingInNeighbor | StickyHoney => 8.0,
            Fire => 16.0,
            Breach | BigMobsCloseToDanger => 4.0,
            _ => 0.0,
        }
    }

    fn from_u8(v: u8) -> PathType {
        // Only values written by `state_type` are stored.
        use PathType::*;
        const ALL: [PathType; 27] = [
            Blocked,
            Open,
            Walkable,
            WalkableDoor,
            Trapdoor,
            PowderSnow,
            OnTopOfPowderSnow,
            Fence,
            Lava,
            Water,
            WaterBorder,
            Rail,
            UnpassableRail,
            FireInNeighbor,
            Fire,
            DamagingInNeighbor,
            Damaging,
            DoorOpen,
            DoorWoodClosed,
            DoorIronClosed,
            Breach,
            Leaves,
            StickyHoney,
            Cocoa,
            DamageCautious,
            OnTopOfTrapdoor,
            BigMobsCloseToDanger,
        ];
        ALL.get(usize::from(v)).copied().unwrap_or(Blocked)
    }
}

/// Blocks that are never pathfindable (`isPathfindable` returns `false` for every type).
fn never_pathfindable(state: BlockState) -> bool {
    let block = state.block();
    match class_of_block(block) {
        Class::AbstractBed
        | Class::BambooStalk
        | Class::Skull
        | Class::WallSkull
        | Class::Anvil
        | Class::CandleCake
        | Class::Cake
        | Class::Azalea
        | Class::Cocoa
        | Class::Campfire
        | Class::EndPortalFrame
        | Class::Bell
        | Class::Cactus
        | Class::EnderChest
        | Class::Farmland
        | Class::FlowerPot
        | Class::Grindstone
        | Class::ChorusPlant
        | Class::Chest
        | Class::CopperChest
        | Class::Lantern
        | Class::Fence
        | Class::IronBars
        | Class::Chain
        | Class::Conduit
        | Class::Hopper
        | Class::DecoratedPot
        | Class::Path
        | Class::HeavyCore
        | Class::Rod
        | Class::LightningRod
        | Class::SculkSensor
        | Class::CalibratedSculkSensor
        | Class::SeaPickle
        | Class::Stonecutter
        | Class::Lectern
        | Class::Wall
        | Class::ShelfMushroom
        | Class::WallHangingSign
        | Class::Stair
        | Class::Speleothem => true,
        _ => matches!(
            block.name(),
            "minecraft:brewing_stand"
                | "minecraft:enchanting_table"
                | "minecraft:cauldron"
                | "minecraft:water_cauldron"
                | "minecraft:lava_cauldron"
                | "minecraft:powder_snow_cauldron"
                | "minecraft:composter"
                | "minecraft:dragon_egg"
                | "minecraft:mud"
                | "minecraft:soul_sand"
                | "minecraft:respawn_anchor"
                | "minecraft:dried_ghast"
                | "minecraft:sniffer_egg"
        ),
    }
}

/// `BlockBehaviour.isPathfindable(state, LAND)`.
pub fn is_pathfindable_land(state: BlockState) -> bool {
    is_pathfindable(state, Computation::Land)
}

/// `PathComputationType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Computation {
    Land,
    Water,
    Air,
}

/// `BlockBehaviour.isPathfindable(state, type)`, uncached.
fn compute_pathfindable(state: BlockState, ty: Computation) -> bool {
    if never_pathfindable(state) {
        return false;
    }
    let block = state.block();
    let open = state.value("open") == Some("true");
    let water = super::is_water(state);
    let full = state.has(flags::COLLISION_FULL_BLOCK);
    match class_of_block(block) {
        Class::Door | Class::FenceGate => ty != Computation::Water && open,
        Class::TrapDoor => match ty {
            Computation::Water => state.value("waterlogged") == Some("true"),
            _ => open,
        },
        Class::Liquid => !block.name().ends_with("lava"),
        Class::Slab | Class::Shelf | Class::CopperGolemStatue => ty == Computation::Water && water,
        Class::SnowLayer => {
            ty == Computation::Land
                && state
                    .value("layers")
                    .and_then(|l| l.parse::<u8>().ok())
                    .is_some_and(|l| l < 5)
        }
        _ if block == ids::POWDER_SNOW => true,
        _ => match ty {
            Computation::Water => water,
            // `VegetationBlock`: without collision it is pathfindable through the air.
            Computation::Air => state.collision_shape().is_empty() || !full,
            Computation::Land => !full,
        },
    }
}

/// `isPathfindable(state, type)`, from a table of every state.
pub fn is_pathfindable(state: BlockState, ty: Computation) -> bool {
    static TABLE: OnceLock<Vec<u8>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        (0..STATE_COUNT)
            .map(|s| {
                let state = BlockState(u16::try_from(s).unwrap_or(0));
                u8::from(compute_pathfindable(state, Computation::Land))
                    | (u8::from(compute_pathfindable(state, Computation::Water)) << 1)
                    | (u8::from(compute_pathfindable(state, Computation::Air)) << 2)
            })
            .collect()
    });
    let bits = table.get(usize::from(state.0)).copied().unwrap_or(0);
    let bit = match ty {
        Computation::Land => 1,
        Computation::Water => 2,
        Computation::Air => 4,
    };
    bits & bit != 0
}

/// `WalkNodeEvaluator.getPathTypeFromState`, uncached.
fn compute_state_type(state: BlockState) -> PathType {
    if state.is_air() {
        return PathType::Open;
    }
    let block = state.block();
    let class = class_of_block(block);
    let name = block.name();
    if class == Class::TrapDoor || class == Class::LilyPad || class == Class::BigDripleaf {
        return PathType::Trapdoor;
    }
    if block == ids::POWDER_SNOW {
        return PathType::PowderSnow;
    }
    if block == ids::CACTUS || block == ids::SWEET_BERRY_BUSH {
        return PathType::Damaging;
    }
    if block == ids::HONEY_BLOCK {
        return PathType::StickyHoney;
    }
    if block == ids::COCOA {
        return PathType::Cocoa;
    }
    if block == ids::WITHER_ROSE || class == Class::Speleothem {
        return PathType::DamageCautious;
    }
    if super::is_lava(state) {
        return PathType::Lava;
    }
    let burning = block == ids::FIRE
        || block == ids::SOUL_FIRE
        || block == ids::MAGMA_BLOCK
        || block == ids::LAVA_CAULDRON
        || (class == Class::Campfire && state.value("lit") == Some("true"));
    if burning {
        return PathType::Fire;
    }
    if class == Class::Door {
        if state.value("open") == Some("true") {
            return PathType::DoorOpen;
        }
        return if name == "minecraft:iron_door" {
            PathType::DoorIronClosed
        } else {
            PathType::DoorWoodClosed
        };
    }
    if matches!(
        class,
        Class::Rail | Class::PoweredRail | Class::DetectorRail
    ) {
        return PathType::Rail;
    }
    if class == Class::Leaves {
        return PathType::Leaves;
    }
    if class == Class::Fence
        || class == Class::Wall
        || (class == Class::FenceGate && state.value("open") != Some("true"))
    {
        return PathType::Fence;
    }
    if !compute_pathfindable(state, Computation::Land) {
        return PathType::Blocked;
    }
    if super::is_water(state) {
        PathType::Water
    } else {
        PathType::Open
    }
}

/// The path type of every block state, computed once.
fn state_table() -> &'static [u8] {
    static TABLE: OnceLock<Vec<u8>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0..STATE_COUNT)
            .map(|s| {
                let state = BlockState(u16::try_from(s).unwrap_or(0));
                compute_state_type(state) as u8
            })
            .collect()
    })
}

/// `getPathTypeFromState` (Vanilla caches it in `PathTypeCache`; this table holds every state).
#[inline]
pub fn state_type(state: BlockState) -> PathType {
    state_table()
        .get(usize::from(state.0))
        .map_or(PathType::Blocked, |&v| PathType::from_u8(v))
}

/// `WalkNodeEvaluator.getPathTypeStatic`.
pub fn path_type_static(blocks: &dyn Blocks, min_y: i32, x: i32, y: i32, z: i32) -> PathType {
    let here = state_type(blocks.state(BlockPos::new(x, y, z)));
    if here == PathType::Open && y > min_y {
        match state_type(blocks.state(BlockPos::new(x, y - 1, z))) {
            PathType::Open | PathType::Water | PathType::Lava | PathType::Walkable => {
                PathType::Open
            }
            PathType::Fire => PathType::Fire,
            PathType::Damaging => PathType::Damaging,
            PathType::StickyHoney => PathType::StickyHoney,
            PathType::PowderSnow => PathType::OnTopOfPowderSnow,
            PathType::DamageCautious => PathType::DamageCautious,
            PathType::Trapdoor => PathType::OnTopOfTrapdoor,
            _ => check_neighbour_blocks(blocks, x, y, z, PathType::Walkable),
        }
    } else {
        here
    }
}

/// `WalkNodeEvaluator.checkNeighbourBlocks`.
fn check_neighbour_blocks(
    blocks: &dyn Blocks,
    x: i32,
    y: i32,
    z: i32,
    fallback: PathType,
) -> PathType {
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                if dx == 0 && dz == 0 {
                    continue;
                }
                match state_type(blocks.state(BlockPos::new(x + dx, y + dy, z + dz))) {
                    PathType::Damaging => return PathType::DamagingInNeighbor,
                    PathType::Fire | PathType::Lava => return PathType::FireInNeighbor,
                    PathType::Water => return PathType::WaterBorder,
                    PathType::DamageCautious => return PathType::DamageCautious,
                    _ => {}
                }
            }
        }
    }
    fallback
}

/// `WalkNodeEvaluator.getFloorLevel(level, pos)`.
pub fn floor_level(blocks: &dyn Blocks, pos: BlockPos) -> f64 {
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    f64::from(below.y) + shape_top(blocks.state(below))
}

/// The top of the collision shape of `state` (0 without one).
fn compute_shape_top(state: BlockState) -> f64 {
    state
        .collision_shape()
        .iter()
        .map(|b| b[4])
        .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
        .unwrap_or(0.0)
}

/// [`compute_shape_top`] of every block state, computed once: an index per state into the
/// distinct tops (`u8::MAX` = computed each time, if there were ever that many).
fn shape_tops() -> &'static (Vec<u8>, Vec<f64>) {
    static TABLE: OnceLock<(Vec<u8>, Vec<f64>)> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut tops: Vec<f64> = Vec::new();
        let index = (0..STATE_COUNT)
            .map(|s| {
                let top = compute_shape_top(BlockState(u16::try_from(s).unwrap_or(0)));
                match tops.iter().position(|&t| t.to_bits() == top.to_bits()) {
                    Some(i) => u8::try_from(i).unwrap_or(u8::MAX),
                    None if tops.len() < usize::from(u8::MAX) => {
                        tops.push(top);
                        u8::try_from(tops.len() - 1).unwrap_or(u8::MAX)
                    }
                    None => u8::MAX,
                }
            })
            .collect();
        (index, tops)
    })
}

/// [`compute_shape_top`] from the table.
#[inline]
fn shape_top(state: BlockState) -> f64 {
    let (index, tops) = shape_tops();
    match index.get(usize::from(state.0)) {
        Some(&i) if i != u8::MAX => tops[usize::from(i)],
        _ => compute_shape_top(state),
    }
}

/// The node evaluator of a search (`NodeEvaluator` subclasses).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Evaluator {
    /// `WalkNodeEvaluator`.
    #[default]
    Walk,
    /// `SwimNodeEvaluator(allowBreaching)`.
    Swim { breaching: bool },
    /// `FlyNodeEvaluator`.
    Fly,
    /// `AmphibiousNodeEvaluator(prefersShallowSwimming)`; `frog` is the frog's evaluator
    /// (`#frog_prefer_jump_to` blocks count as open, the start in water is the box bottom).
    Amphibious { prefers_shallow: bool, frog: bool },
}

/// What the path finder needs to know about the mob.
#[derive(Clone, Copy, Debug)]
pub struct PathMob {
    /// The node evaluator.
    pub evaluator: Evaluator,
    /// `canWalkOverFences`.
    pub can_walk_over_fences: bool,
    /// `canStandOnFluid(lava)` (striders).
    pub stands_on_lava: bool,
    /// `level.getSeaLevel()` (shallow swimming).
    pub sea_level: i32,
    pub pos: Vec3,
    pub aabb: Aabb,
    pub width: f32,
    pub height: f32,
    pub on_ground: bool,
    pub max_up_step: f32,
    pub max_fall_distance: i32,
    /// `isInFloatableFluid` (water).
    pub in_water: bool,
    pub can_float: bool,
    pub can_open_doors: bool,
    pub can_pass_doors: bool,
    /// The creaking heart a creaking keeps near (`Creaking.HomeNodeEvaluator`): blocks more
    /// than 32 blocks from it are blocked unless the mob is even farther.
    pub home: Option<BlockPos>,
}

/// The malus overrides a mob kind sets: `(type, cost)`.
pub type MalusTable = [(PathType, f32)];

impl PathMob {
    fn block_pos(&self) -> BlockPos {
        BlockPos::containing(self.pos.x, self.pos.y, self.pos.z)
    }

    /// A walking mob with the defaults of `Mob` (tests and simple callers).
    pub fn walking(pos: Vec3, aabb: Aabb, width: f32, height: f32) -> PathMob {
        PathMob {
            evaluator: Evaluator::Walk,
            can_walk_over_fences: false,
            stands_on_lava: false,
            sea_level: 63,
            pos,
            aabb,
            width,
            height,
            on_ground: true,
            max_up_step: 0.6,
            max_fall_distance: 3,
            in_water: false,
            can_float: false,
            can_open_doors: false,
            can_pass_doors: true,
            home: None,
        }
    }

    fn amphibious(&self) -> bool {
        matches!(self.evaluator, Evaluator::Amphibious { .. })
    }
}

/// `Mob.getPathfindingMalus`.
pub fn malus_of(table: &MalusTable, t: PathType) -> f32 {
    table
        .iter()
        .find(|(k, _)| *k == t)
        .map_or_else(|| t.malus(), |&(_, v)| v)
}

/// A node of a computed path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathNode {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub kind: PathType,
}

impl PathNode {
    /// The node's block position.
    pub fn pos(&self) -> BlockPos {
        BlockPos::new(self.x, self.y, self.z)
    }
}

/// `Path`: the nodes, the next node to walk to and the target.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    pub nodes: Vec<PathNode>,
    pub next: usize,
    pub target: BlockPos,
    pub dist_to_target: f32,
    pub reached: bool,
}

impl Path {
    /// `isDone`.
    pub fn is_done(&self) -> bool {
        self.next >= self.nodes.len()
    }

    /// `getEntityPosAtNode`.
    pub fn entity_pos_at(&self, width: f32, index: usize) -> Vec3 {
        let Some(n) = self.nodes.get(index) else {
            return Vec3::ZERO;
        };
        // `(int)(width + 1.0F) * 0.5`.
        #[allow(clippy::cast_possible_truncation)]
        let half = f64::from((width + 1.0) as i32) * 0.5;
        Vec3::new(f64::from(n.x) + half, f64::from(n.y), f64::from(n.z) + half)
    }

    /// `getNextEntityPos`.
    pub fn next_entity_pos(&self, width: f32) -> Vec3 {
        self.entity_pos_at(width, self.next)
    }

    /// `getEndNode`.
    pub fn end(&self) -> Option<&PathNode> {
        self.nodes.last()
    }

    /// `sameAs`: the same nodes.
    pub fn same_nodes(&self, other: &Path) -> bool {
        self.nodes == other.nodes
    }
}

#[derive(Clone, Copy, Debug)]
struct Node {
    x: i32,
    y: i32,
    z: i32,
    heap_idx: i32,
    g: f32,
    h: f32,
    f: f32,
    came_from: u32,
    closed: bool,
    walked: f32,
    cost_malus: f32,
    kind: PathType,
}

const NONE: u32 = u32::MAX;

impl Node {
    fn new(x: i32, y: i32, z: i32) -> Node {
        Node {
            x,
            y,
            z,
            heap_idx: -1,
            g: 0.0,
            h: 0.0,
            f: 0.0,
            came_from: NONE,
            closed: false,
            walked: 0.0,
            cost_malus: 0.0,
            kind: PathType::Blocked,
        }
    }

    fn distance_to(&self, x: i32, y: i32, z: i32) -> f32 {
        // `float xd = to.x - this.x`: int to float conversions.
        #[allow(clippy::cast_precision_loss)]
        let (dx, dy, dz) = (
            (x - self.x) as f32,
            (y - self.y) as f32,
            (z - self.z) as f32,
        );
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    fn manhattan(&self, x: i32, y: i32, z: i32) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let d = ((x - self.x).abs() + (y - self.y).abs() + (z - self.z).abs()) as f32;
        d
    }
}

fn key(x: i32, y: i32, z: i32) -> i64 {
    BlockPos::new(x, y, z).as_long()
}

/// The reusable state of a path search (`PathFinder` + `WalkNodeEvaluator`). One per world;
/// a search leaves nothing behind but its result.
#[derive(Debug, Default)]
pub struct PathFinder {
    nodes: Vec<Node>,
    index: FxHashMap<i64, u32>,
    heap: Vec<u32>,
    types: FxHashMap<i64, PathType>,
    collisions: FxHashMap<[u64; 6], bool>,
    boxes: Vec<Aabb>,
    /// The block states the running search has read.
    states: StateCache,
    /// Nodes visited by searches since the last [`Self::take_visited`] (for budgets).
    visited: u64,
}

/// The block states one search has read, by position. A search asks for most blocks many
/// times (every walkable block looks at its 26 neighbours); the world does not change while
/// it runs. Vanilla's `PathTypeCache` plays this part.
#[derive(Debug, Default)]
struct StateCache {
    slots: Vec<std::cell::Cell<StateSlot>>,
    /// `getPathTypeStatic` by position: a walking search asks for most positions several
    /// times (each node checks the blocks of the mob's whole box, and every result looks at
    /// up to 24 neighbours).
    statics: Vec<std::cell::Cell<StaticSlot>>,
    /// `getCachedPathType` (the type of the mob's whole box) by position, in place of a map.
    mob_types: Vec<std::cell::Cell<StaticSlot>>,
    /// The running search; slots of earlier searches do not count.
    stamp: u32,
}

#[derive(Clone, Copy, Debug, Default)]
struct StateSlot {
    pos: BlockPos,
    stamp: u32,
    state: u16,
}

#[derive(Clone, Copy, Debug, Default)]
struct StaticSlot {
    pos: BlockPos,
    stamp: u32,
    kind: u8,
}

impl StateCache {
    /// Slots, a power of two (`PathTypeCache.SIZE`).
    const SIZE: usize = 4096;

    #[inline]
    fn slot(pos: BlockPos) -> usize {
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let index = ((pos.as_long() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 52) as usize;
        index
    }

    /// [`path_type_static`] once per position and search.
    #[inline]
    fn path_type_static(
        &self,
        blocks: &dyn Blocks,
        min_y: i32,
        x: i32,
        y: i32,
        z: i32,
    ) -> PathType {
        let pos = BlockPos::new(x, y, z);
        let slot = &self.statics[Self::slot(pos)];
        let cached = slot.get();
        if cached.stamp == self.stamp && cached.pos == pos {
            return PathType::from_u8(cached.kind);
        }
        let kind = path_type_static(blocks, min_y, x, y, z);
        slot.set(StaticSlot {
            pos,
            stamp: self.stamp,
            kind: kind as u8,
        });
        kind
    }

    /// Starts a search: everything cached so far is forgotten.
    fn begin(&mut self) {
        self.stamp = self.stamp.wrapping_add(1);
        if self.slots.is_empty() || self.stamp == 0 {
            self.slots.clear();
            self.slots.resize_with(Self::SIZE, Default::default);
            self.statics.clear();
            self.statics.resize_with(Self::SIZE, Default::default);
            self.mob_types.clear();
            self.mob_types.resize_with(Self::SIZE, Default::default);
            self.stamp = 1;
        }
    }
}

/// The blocks of a search: `inner` read once per position.
struct SearchBlocks<'a> {
    inner: &'a dyn Blocks,
    cache: &'a StateCache,
}

impl Blocks for SearchBlocks<'_> {
    #[inline]
    fn state(&self, pos: BlockPos) -> BlockState {
        let slot = &self.cache.slots[StateCache::slot(pos)];
        let cached = slot.get();
        if cached.stamp == self.cache.stamp && cached.pos == pos {
            return BlockState(cached.state);
        }
        let state = self.inner.state(pos);
        slot.set(StateSlot {
            pos,
            stamp: self.cache.stamp,
            state: state.0,
        });
        state
    }

    #[inline]
    fn border(&self) -> Option<&crate::level::border::WorldBorder> {
        self.inner.border()
    }
}

/// One target of a search (`Target`).
#[derive(Clone, Copy, Debug)]
struct Target {
    x: i32,
    y: i32,
    z: i32,
    pos: BlockPos,
    best_h: f32,
    best: u32,
    reached: bool,
}

/// The parameters of one search besides the mob.
#[derive(Clone, Copy, Debug)]
pub struct Search<'a> {
    pub max_path_length: f32,
    pub reach_range: i32,
    pub max_visited_nodes: i32,
    pub malus: &'a MalusTable,
    pub min_y: i32,
}

struct Eval<'a> {
    blocks: &'a dyn Blocks,
    /// The search's cache of [`path_type_static`] (none in tests).
    statics: Option<&'a StateCache>,
    mob: &'a PathMob,
    search: Search<'a>,
    entity_width: i32,
    entity_height: i32,
    entity_depth: i32,
}

impl PathFinder {
    /// A search state.
    pub fn new() -> PathFinder {
        PathFinder::default()
    }

    /// Nodes visited since the last call.
    pub fn take_visited(&mut self) -> u64 {
        std::mem::take(&mut self.visited)
    }

    fn node(&mut self, x: i32, y: i32, z: i32) -> u32 {
        let k = key(x, y, z);
        if let Some(&i) = self.index.get(&k) {
            return i;
        }
        let i = u32::try_from(self.nodes.len()).unwrap_or(NONE);
        self.nodes.push(Node::new(x, y, z));
        self.index.insert(k, i);
        i
    }

    fn n(&self, i: u32) -> &Node {
        &self.nodes[i as usize]
    }

    fn nm(&mut self, i: u32) -> &mut Node {
        &mut self.nodes[i as usize]
    }

    // --- Binary heap (`BinaryHeap`) -------------------------------------------------------

    fn heap_insert(&mut self, i: u32) {
        let idx = self.heap.len();
        self.heap.push(i);
        self.nm(i).heap_idx = i32::try_from(idx).unwrap_or(-1);
        self.up_heap(idx);
    }

    fn heap_pop(&mut self) -> u32 {
        let popped = self.heap[0];
        let last = self.heap.pop().unwrap_or(popped);
        if !self.heap.is_empty() {
            self.heap[0] = last;
            self.nm(last).heap_idx = 0;
            self.down_heap(0);
        }
        self.nm(popped).heap_idx = -1;
        popped
    }

    fn change_cost(&mut self, i: u32, cost: f32) {
        let old = self.n(i).f;
        self.nm(i).f = cost;
        let idx = self.n(i).heap_idx as usize;
        if cost < old {
            self.up_heap(idx);
        } else {
            self.down_heap(idx);
        }
    }

    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    fn up_heap(&mut self, mut idx: usize) {
        let node = self.heap[idx];
        let cost = self.n(node).f;
        while idx > 0 {
            let parent_idx = (idx - 1) >> 1;
            let parent = self.heap[parent_idx];
            if !(cost < self.n(parent).f) {
                break;
            }
            self.heap[idx] = parent;
            self.nm(parent).heap_idx = idx as i32;
            idx = parent_idx;
        }
        self.heap[idx] = node;
        self.nm(node).heap_idx = idx as i32;
    }

    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    fn down_heap(&mut self, mut idx: usize) {
        let node = self.heap[idx];
        let cost = self.n(node).f;
        let size = self.heap.len();
        loop {
            let left = 1 + (idx << 1);
            let right = left + 1;
            if left >= size {
                break;
            }
            let left_node = self.heap[left];
            let left_cost = self.n(left_node).f;
            let (right_node, right_cost) = if right >= size {
                (NONE, f32::INFINITY)
            } else {
                let r = self.heap[right];
                (r, self.n(r).f)
            };
            if left_cost < right_cost {
                if !(left_cost < cost) {
                    break;
                }
                self.heap[idx] = left_node;
                self.nm(left_node).heap_idx = idx as i32;
                idx = left;
            } else {
                if !(right_cost < cost) {
                    break;
                }
                self.heap[idx] = right_node;
                self.nm(right_node).heap_idx = idx as i32;
                idx = right;
            }
        }
        self.heap[idx] = node;
        self.nm(node).heap_idx = idx as i32;
    }

    // --- Search (`PathFinder.findPath`) ---------------------------------------------------

    /// `PathFinder.findPath` to the nearest of `targets`: `None` without a start node.
    /// `random` is the mob's random (the flying start node candidates of small mobs).
    ///
    /// `blocks` is read once per position, so it must be a read without side effects (the
    /// level's read view, as Vanilla's `PathNavigationRegion`), never the tick view.
    pub fn find_path(
        &mut self,
        blocks: &dyn Blocks,
        mob: &PathMob,
        targets: &[BlockPos],
        search: Search<'_>,
        random: &mut LegacyRandom,
    ) -> Option<Path> {
        let mut states = std::mem::take(&mut self.states);
        states.begin();
        let cached = SearchBlocks {
            inner: blocks,
            cache: &states,
        };
        let path = self.find_path_in(&cached, Some(&states), mob, targets, search, random);
        self.states = states;
        path
    }

    fn find_path_in(
        &mut self,
        blocks: &dyn Blocks,
        statics: Option<&StateCache>,
        mob: &PathMob,
        targets: &[BlockPos],
        search: Search<'_>,
        random: &mut LegacyRandom,
    ) -> Option<Path> {
        self.nodes.clear();
        self.index.clear();
        self.heap.clear();
        self.types.clear();
        self.collisions.clear();
        let w = floor(f64::from(mob.width) + 1.0);
        let h = floor(f64::from(mob.height) + 1.0);
        let eval = Eval {
            blocks,
            statics,
            mob,
            search,
            entity_width: w,
            entity_height: h,
            entity_depth: w,
        };
        let from = match mob.evaluator {
            Evaluator::Walk => self.start(&eval),
            Evaluator::Swim { .. } => {
                let b = mob.aabb;
                Some(self.node(floor(b.min_x), floor(b.min_y + 0.5), floor(b.min_z)))
            }
            Evaluator::Fly => Some(self.fly_start(&eval, random)),
            Evaluator::Amphibious { frog, .. } => {
                if mob.in_water {
                    let b = mob.aabb;
                    let y = if frog {
                        floor(b.min_y)
                    } else {
                        floor(b.min_y + 0.5)
                    };
                    Some(self.start_node(&eval, floor(b.min_x), y, floor(b.min_z)))
                } else {
                    self.start(&eval)
                }
            }
        }?;
        let mut tgts: Vec<Target> = targets
            .iter()
            .map(|&pos| Target {
                x: pos.x,
                y: pos.y,
                z: pos.z,
                pos,
                best_h: f32::MAX,
                best: NONE,
                reached: false,
            })
            .collect();
        let path = self.search(&eval, from, &mut tgts);
        Some(path)
    }

    fn best_h(&self, node: u32, targets: &mut [Target]) -> f32 {
        let mut best = f32::MAX;
        for t in targets.iter_mut() {
            let h = self.n(node).distance_to(t.x, t.y, t.z);
            if h < t.best_h {
                t.best_h = h;
                t.best = node;
            }
            best = best.min(h);
        }
        best
    }

    fn search(&mut self, eval: &Eval<'_>, from: u32, targets: &mut [Target]) -> Path {
        let max_len = eval.search.max_path_length;
        let h = self.best_h(from, targets);
        {
            let f = self.nm(from);
            f.g = 0.0;
            f.h = h;
            f.f = h;
        }
        self.heap.clear();
        self.heap_insert(from);
        let mut count = 0;
        let mut any_reached = false;
        let mut neighbors = [NONE; 32];
        let (fx, fy, fz) = {
            let f = self.n(from);
            (f.x, f.y, f.z)
        };
        while !self.heap.is_empty() {
            count += 1;
            if count >= eval.search.max_visited_nodes {
                break;
            }
            let current = self.heap_pop();
            self.nm(current).closed = true;
            self.visited += 1;
            let range = eval.search.reach_range;
            #[allow(clippy::cast_precision_loss)]
            let range = range as f32;
            for t in targets.iter_mut() {
                if self.n(current).manhattan(t.x, t.y, t.z) <= range {
                    t.reached = true;
                    any_reached = true;
                }
            }
            if any_reached {
                break;
            }
            if self.n(current).distance_to(fx, fy, fz) >= max_len {
                continue;
            }
            let count_n = self.neighbors(eval, &mut neighbors, current);
            for &neighbor in neighbors.iter().take(count_n) {
                let (cx, cy, cz) = {
                    let c = self.n(current);
                    (c.x, c.y, c.z)
                };
                let distance = self.n(neighbor).distance_to(cx, cy, cz);
                let walked = self.n(current).walked + distance;
                self.nm(neighbor).walked = walked;
                let tentative = self.n(current).g + distance + self.n(neighbor).cost_malus;
                let in_open = self.n(neighbor).heap_idx >= 0;
                if walked < max_len && (!in_open || tentative < self.n(neighbor).g) {
                    let h = self.best_h(neighbor, targets) * 1.5;
                    {
                        let n = self.nm(neighbor);
                        n.came_from = current;
                        n.g = tentative;
                        n.h = h;
                    }
                    if in_open {
                        let g = self.n(neighbor).g;
                        self.change_cost(neighbor, g + h);
                    } else {
                        let n = self.nm(neighbor);
                        n.f = n.g + n.h;
                        self.heap_insert(neighbor);
                    }
                }
            }
        }
        let mut best: Option<Path> = None;
        for t in targets.iter() {
            if any_reached && !t.reached {
                continue;
            }
            let path = self.reconstruct(t.best, t.pos, any_reached);
            let better = match &best {
                None => true,
                Some(b) if any_reached => path.nodes.len() < b.nodes.len(),
                Some(b) => {
                    path.dist_to_target < b.dist_to_target
                        || (path.dist_to_target == b.dist_to_target
                            && path.nodes.len() < b.nodes.len())
                }
            };
            if better {
                best = Some(path);
            }
        }
        best.unwrap_or(Path {
            nodes: Vec::new(),
            next: 0,
            target: BlockPos::ZERO,
            dist_to_target: f32::MAX,
            reached: false,
        })
    }

    fn reconstruct(&self, closest: u32, target: BlockPos, reached: bool) -> Path {
        let mut nodes = Vec::new();
        let mut i = closest;
        while i != NONE {
            let n = self.n(i);
            nodes.push(PathNode {
                x: n.x,
                y: n.y,
                z: n.z,
                kind: n.kind,
            });
            i = n.came_from;
        }
        nodes.reverse();
        let dist = nodes.last().map_or(f32::MAX, |n| {
            #[allow(clippy::cast_precision_loss)]
            let d =
                ((target.x - n.x).abs() + (target.y - n.y).abs() + (target.z - n.z).abs()) as f32;
            d
        });
        Path {
            nodes,
            next: 0,
            target,
            dist_to_target: dist,
            reached,
        }
    }

    // --- Node evaluators -------------------------------------------------------------------

    /// `getPathfindingMalus` with the amphibious evaluator's overrides (`prepare` sets water
    /// to 0, walkable to 6 and the water border to 4).
    fn malus(eval: &Eval<'_>, t: PathType) -> f32 {
        if eval.mob.amphibious() {
            match t {
                PathType::Water => return 0.0,
                PathType::Walkable => return 6.0,
                PathType::WaterBorder => return 4.0,
                _ => {}
            }
        }
        malus_of(eval.search.malus, t)
    }

    /// `getNeighbors` of the search's evaluator.
    fn neighbors(&mut self, eval: &Eval<'_>, out: &mut [u32; 32], pos: u32) -> usize {
        match eval.mob.evaluator {
            Evaluator::Walk => self.walk_neighbors(eval, out, pos),
            Evaluator::Swim { breaching } => self.swim_neighbors(eval, out, pos, breaching),
            Evaluator::Fly => self.fly_neighbors(eval, out, pos),
            Evaluator::Amphibious {
                prefers_shallow, ..
            } => self.amphibious_neighbors(eval, out, pos, prefers_shallow),
        }
    }

    // --- `WalkNodeEvaluator` ----------------------------------------------------------------

    fn start(&mut self, eval: &Eval<'_>) -> Option<u32> {
        let mob = eval.mob;
        let blocks = eval.blocks;
        let mut start_y = floor(mob.pos.y);
        let at = |y: i32| BlockPos::containing(mob.pos.x, f64::from(y), mob.pos.z);
        let stands = |state: BlockState| mob.stands_on_lava && super::is_lava(state);
        if stands(blocks.state(at(start_y))) {
            // `canStandOnFluid`: the start is on top of the fluid.
            while stands(blocks.state(at(start_y))) {
                start_y += 1;
            }
            start_y -= 1;
        } else if eval.mob.can_float && mob.in_water {
            while is_water(blocks.state(at(start_y))) {
                start_y += 1;
            }
            start_y -= 1;
        } else if mob.on_ground {
            start_y = floor(mob.pos.y + 0.5);
        } else {
            let mut y = floor(mob.pos.y + 1.0);
            while y > eval.search.min_y {
                start_y = y;
                y -= 1;
                let below = blocks.state(at(y));
                if !below.is_air() && !is_pathfindable_land(below) {
                    break;
                }
            }
        }
        let start = mob.block_pos();
        if !self.can_start_at(eval, start.x, start_y, start.z) {
            let b = mob.aabb;
            for (x, z) in [
                (b.min_x, b.min_z),
                (b.min_x, b.max_z),
                (b.max_x, b.min_z),
                (b.max_x, b.max_z),
            ] {
                let p = BlockPos::containing(x, f64::from(start_y), z);
                if self.can_start_at(eval, p.x, p.y, p.z) {
                    return Some(self.start_node(eval, p.x, p.y, p.z));
                }
            }
        }
        Some(self.start_node(eval, start.x, start_y, start.z))
    }

    fn start_node(&mut self, eval: &Eval<'_>, x: i32, y: i32, z: i32) -> u32 {
        let t = self.cached_type(eval, x, y, z);
        let i = self.node(x, y, z);
        let n = self.nm(i);
        n.kind = t;
        n.cost_malus = Self::malus(eval, t);
        i
    }

    fn can_start_at(&mut self, eval: &Eval<'_>, x: i32, y: i32, z: i32) -> bool {
        let t = self.cached_type(eval, x, y, z);
        if eval.mob.evaluator == Evaluator::Fly {
            return Self::malus(eval, t) >= 0.0;
        }
        t != PathType::Open && Self::malus(eval, t) >= 0.0
    }

    /// The jump size of `getNeighbors`: the step height when the block above is passable.
    fn jump_size(&mut self, eval: &Eval<'_>, px: i32, py: i32, pz: i32) -> i32 {
        let above = self.cached_type(eval, px, py + 1, pz);
        let current = self.cached_type(eval, px, py, pz);
        if Self::malus(eval, above) >= 0.0 && current != PathType::StickyHoney {
            floor(f64::from(eval.mob.max_up_step.max(1.0)))
        } else {
            0
        }
    }

    fn walk_neighbors(&mut self, eval: &Eval<'_>, out: &mut [u32; 32], pos: u32) -> usize {
        let (px, py, pz) = {
            let n = self.n(pos);
            (n.x, n.y, n.z)
        };
        let mut p = 0;
        let jump_size = self.jump_size(eval, px, py, pz);
        let current = self.cached_type(eval, px, py, pz);
        let height = self.floor_level(eval, BlockPos::new(px, py, pz));
        // `Direction.Plane.HORIZONTAL`: north, east, south, west; 2D data values are
        // south 0, west 1, north 2, east 3.
        const DIRS: [(i32, i32, usize); 4] = [(0, -1, 2), (1, 0, 3), (0, 1, 0), (-1, 0, 1)];
        let mut reusable = [NONE; 4];
        for &(dx, dz, d2) in &DIRS {
            let node = self.find_accepted_node(
                eval,
                px + dx,
                py,
                pz + dz,
                jump_size,
                height,
                (dx, dz),
                current,
            );
            reusable[d2] = node.unwrap_or(NONE);
            if let Some(node) = node
                && self.neighbor_valid(node, pos)
            {
                out[p] = node;
                p += 1;
            }
        }
        for &(dx, dz, d2) in &DIRS {
            // `getClockWise`: north → east → south → west.
            let (cx, cz, c2) = match d2 {
                2 => (1, 0, 3),
                3 => (0, 1, 0),
                0 => (-1, 0, 1),
                _ => (0, -1, 2),
            };
            if self.diagonal_valid(eval, pos, reusable[d2], reusable[c2]) {
                let diag = self.find_accepted_node(
                    eval,
                    px + dx + cx,
                    py,
                    pz + dz + cz,
                    jump_size,
                    height,
                    (dx, dz),
                    current,
                );
                if let Some(d) = diag
                    && self.diagonal_node_valid(d)
                {
                    out[p] = d;
                    p += 1;
                }
            }
        }
        p
    }

    fn neighbor_valid(&self, neighbor: u32, current: u32) -> bool {
        let n = self.n(neighbor);
        !n.closed && (n.cost_malus >= 0.0 || self.n(current).cost_malus < 0.0)
    }

    fn diagonal_valid(&self, eval: &Eval<'_>, pos: u32, ew: u32, ns: u32) -> bool {
        if ns == NONE || ew == NONE {
            return false;
        }
        let (p, e, n) = (self.n(pos), self.n(ew), self.n(ns));
        if n.y > p.y || e.y > p.y {
            return false;
        }
        if e.kind == PathType::WalkableDoor || n.kind == PathType::WalkableDoor {
            return false;
        }
        let width = eval.mob.width;
        if width > 1.0 && (e.cost_malus > 0.0 || n.cost_malus > 0.0) {
            return false;
        }
        let between_posts = n.kind == PathType::Fence && e.kind == PathType::Fence && width < 0.5;
        (n.y < p.y || n.cost_malus >= 0.0 || between_posts)
            && (e.y < p.y || e.cost_malus >= 0.0 || between_posts)
    }

    fn diagonal_node_valid(&self, d: u32) -> bool {
        let n = self.n(d);
        !n.closed && n.kind != PathType::WalkableDoor && n.cost_malus >= 0.0
    }

    /// `getFloorLevel`: the middle of floatable water for floating or amphibious mobs.
    fn floor_level(&self, eval: &Eval<'_>, pos: BlockPos) -> f64 {
        if (eval.mob.can_float || eval.mob.amphibious()) && is_water(eval.blocks.state(pos)) {
            f64::from(pos.y) + 0.5
        } else {
            floor_level(eval.blocks, pos)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn find_accepted_node(
        &mut self,
        eval: &Eval<'_>,
        x: i32,
        y: i32,
        z: i32,
        jump_size: i32,
        node_height: f64,
        dir: (i32, i32),
        current: PathType,
    ) -> Option<u32> {
        let max_y_target = self.floor_level(eval, BlockPos::new(x, y, z));
        let jump_height = f64::from(eval.mob.max_up_step).max(1.125);
        if max_y_target - node_height > jump_height {
            return None;
        }
        let t = self.cached_type(eval, x, y, z);
        let cost = Self::malus(eval, t);
        let mut best = None;
        if cost >= 0.0 {
            best = Some(self.node_cost_to_max(x, y, z, t, cost));
        }
        let partial = |t: PathType| {
            matches!(
                t,
                PathType::Fence | PathType::DoorWoodClosed | PathType::DoorIronClosed
            )
        };
        if partial(current)
            && let Some(b) = best
            && self.n(b).cost_malus >= 0.0
            && !self.can_reach_without_collision(eval, b)
        {
            best = None;
        }
        let amphibious = eval.mob.amphibious();
        if t == PathType::Walkable || (amphibious && t == PathType::Water) {
            return best;
        }
        let best_blocked = best.is_none_or(|b| self.n(b).cost_malus < 0.0);
        if best_blocked
            && jump_size > 0
            && (t != PathType::Fence || eval.mob.can_walk_over_fences)
            && t != PathType::UnpassableRail
            && t != PathType::Trapdoor
            && t != PathType::PowderSnow
        {
            best = self.try_jump_on(eval, x, y, z, jump_size, node_height, dir, current);
        } else if !amphibious && t == PathType::Water && !eval.mob.can_float {
            best = self.try_first_non_water_below(eval, x, y, z, best);
        } else if t == PathType::Open {
            best = Some(self.try_first_ground_below(eval, x, y, z));
        } else if partial(t) && best.is_none() {
            best = Some(self.closed_node(x, y, z, t));
        }
        best
    }

    fn node_cost_to_max(&mut self, x: i32, y: i32, z: i32, t: PathType, cost: f32) -> u32 {
        let i = self.node(x, y, z);
        let n = self.nm(i);
        n.kind = t;
        n.cost_malus = n.cost_malus.max(cost);
        i
    }

    fn blocked_node(&mut self, x: i32, y: i32, z: i32) -> u32 {
        let i = self.node(x, y, z);
        let n = self.nm(i);
        n.kind = PathType::Blocked;
        n.cost_malus = -1.0;
        i
    }

    fn closed_node(&mut self, x: i32, y: i32, z: i32, t: PathType) -> u32 {
        let i = self.node(x, y, z);
        let n = self.nm(i);
        n.closed = true;
        n.kind = t;
        n.cost_malus = t.malus();
        i
    }

    #[allow(clippy::too_many_arguments)]
    fn try_jump_on(
        &mut self,
        eval: &Eval<'_>,
        x: i32,
        y: i32,
        z: i32,
        jump_size: i32,
        node_height: f64,
        dir: (i32, i32),
        current: PathType,
    ) -> Option<u32> {
        let above =
            self.find_accepted_node(eval, x, y + 1, z, jump_size - 1, node_height, dir, current)?;
        let mob = eval.mob;
        if mob.width >= 1.0 {
            return Some(above);
        }
        let kind = self.n(above).kind;
        if kind != PathType::Open && kind != PathType::Walkable {
            return Some(above);
        }
        let center_x = f64::from(x - dir.0) + 0.5;
        let center_z = f64::from(z - dir.1) + 0.5;
        let half = f64::from(mob.width) / 2.0;
        let (ax, ay, az) = {
            let n = self.n(above);
            (n.x, n.y, n.z)
        };
        let grow = Aabb::new(
            center_x - half,
            self.floor_level(
                eval,
                BlockPos::containing(center_x, f64::from(y + 1), center_z),
            ) + 0.001,
            center_z - half,
            center_x + half,
            f64::from(mob.height) + self.floor_level(eval, BlockPos::new(ax, ay, az)) - 0.002,
            center_z + half,
        );
        if self.has_collisions(eval, grow) {
            None
        } else {
            Some(above)
        }
    }

    fn try_first_non_water_below(
        &mut self,
        eval: &Eval<'_>,
        x: i32,
        mut y: i32,
        z: i32,
        mut best: Option<u32>,
    ) -> Option<u32> {
        y -= 1;
        while y > eval.search.min_y {
            let t = self.cached_type(eval, x, y, z);
            if t != PathType::Water {
                return best;
            }
            best = Some(self.node_cost_to_max(x, y, z, t, Self::malus(eval, t)));
            y -= 1;
        }
        best
    }

    fn try_first_ground_below(&mut self, eval: &Eval<'_>, x: i32, y: i32, z: i32) -> u32 {
        let mut cy = y - 1;
        while cy >= eval.search.min_y {
            if y - cy > eval.mob.max_fall_distance {
                return self.blocked_node(x, cy, z);
            }
            let t = self.cached_type(eval, x, cy, z);
            let cost = Self::malus(eval, t);
            if t != PathType::Open {
                if cost >= 0.0 {
                    return self.node_cost_to_max(x, cy, z, t, cost);
                }
                return self.blocked_node(x, cy, z);
            }
            cy -= 1;
        }
        self.blocked_node(x, y, z)
    }

    fn can_reach_without_collision(&mut self, eval: &Eval<'_>, to: u32) -> bool {
        let mob = eval.mob;
        let mut bb = mob.aabb;
        let (tx, ty, tz) = {
            let n = self.n(to);
            (f64::from(n.x), f64::from(n.y), f64::from(n.z))
        };
        let xs = bb.max_x - bb.min_x;
        let ys = bb.max_y - bb.min_y;
        let zs = bb.max_z - bb.min_z;
        let mut delta = Vec3::new(
            tx - mob.pos.x + xs / 2.0,
            ty - mob.pos.y + ys / 2.0,
            tz - mob.pos.z + zs / 2.0,
        );
        let size = (xs + ys + zs) / 3.0;
        let steps = vanillite_base::math::ceil(delta.length() / size).max(1);
        // `delta.scale(1.0F / steps)`: a float factor.
        #[allow(clippy::cast_precision_loss)]
        let factor = f64::from(1.0_f32 / steps as f32);
        delta = delta * factor;
        for _ in 1..=steps {
            bb = bb.offset(delta.x, delta.y, delta.z);
            if self.has_collisions(eval, bb) {
                return false;
            }
        }
        true
    }

    fn has_collisions(&mut self, eval: &Eval<'_>, aabb: Aabb) -> bool {
        let k = [
            aabb.min_x.to_bits(),
            aabb.min_y.to_bits(),
            aabb.min_z.to_bits(),
            aabb.max_x.to_bits(),
            aabb.max_y.to_bits(),
            aabb.max_z.to_bits(),
        ];
        if let Some(&c) = self.collisions.get(&k) {
            return c;
        }
        let c = !no_collision(eval.blocks, &aabb, &mut self.boxes);
        self.collisions.insert(k, c);
        c
    }

    fn cached_type(&mut self, eval: &Eval<'_>, x: i32, y: i32, z: i32) -> PathType {
        if let Some(cache) = eval.statics {
            let pos = BlockPos::new(x, y, z);
            let slot = &cache.mob_types[StateCache::slot(pos)];
            let cached = slot.get();
            if cached.stamp == cache.stamp && cached.pos == pos {
                return PathType::from_u8(cached.kind);
            }
            let t = match eval.mob.evaluator {
                Evaluator::Swim { .. } => Self::swim_type_of_mob(eval, x, y, z),
                _ => Self::type_of_mob(eval, x, y, z),
            };
            slot.set(StaticSlot {
                pos,
                stamp: cache.stamp,
                kind: t as u8,
            });
            return t;
        }
        let k = key(x, y, z);
        if let Some(&t) = self.types.get(&k) {
            return t;
        }
        let t = match eval.mob.evaluator {
            Evaluator::Swim { .. } => Self::swim_type_of_mob(eval, x, y, z),
            _ => Self::type_of_mob(eval, x, y, z),
        };
        self.types.insert(k, t);
        t
    }

    /// `getPathType(context, x, y, z)` of one block for the search's evaluator.
    fn block_type(eval: &Eval<'_>, x: i32, y: i32, z: i32) -> PathType {
        let blocks = eval.blocks;
        let min_y = eval.search.min_y;
        match eval.mob.evaluator {
            Evaluator::Fly => fly_path_type(blocks, min_y, eval.mob.block_pos(), x, y, z),
            Evaluator::Amphibious { frog, .. } => {
                if frog && is_frog_jump_target(blocks.state(BlockPos::new(x, y - 1, z))) {
                    PathType::Open
                } else {
                    amphibious_path_type(blocks, min_y, x, y, z)
                }
            }
            _ => {
                if let Some(home) = eval.mob.home {
                    let at = BlockPos::new(x, y, z);
                    let distance = home.dist_sqr(at);
                    if distance > 1024.0 && distance >= home.dist_sqr(eval.mob.block_pos()) {
                        return PathType::Blocked;
                    }
                }
                match eval.statics {
                    Some(cache) => cache.path_type_static(blocks, min_y, x, y, z),
                    None => path_type_static(blocks, min_y, x, y, z),
                }
            }
        }
    }

    /// `getPathTypeOfMob`.
    fn type_of_mob(eval: &Eval<'_>, x: i32, y: i32, z: i32) -> PathType {
        // An `EnumSet`: a bit per type, iterated in declaration order.
        let mut set: u32 = 0;
        let mob_pos = eval.mob.block_pos();
        for dx in 0..eval.entity_width {
            for dy in 0..eval.entity_height {
                for dz in 0..eval.entity_depth {
                    let mut t = Self::block_type(eval, x + dx, y + dy, z + dz);
                    if t == PathType::DoorWoodClosed
                        && eval.mob.can_open_doors
                        && eval.mob.can_pass_doors
                    {
                        t = PathType::WalkableDoor;
                    }
                    if t == PathType::DoorOpen && !eval.mob.can_pass_doors {
                        t = PathType::Blocked;
                    }
                    if t == PathType::Rail
                        && Self::block_type(eval, mob_pos.x, mob_pos.y, mob_pos.z) != PathType::Rail
                        && Self::block_type(eval, mob_pos.x, mob_pos.y - 1, mob_pos.z)
                            != PathType::Rail
                    {
                        t = PathType::UnpassableRail;
                    }
                    set |= 1 << (t as u32);
                }
            }
        }
        if set.count_ones() == 1 {
            return PathType::from_u8(u8::try_from(set.trailing_zeros()).unwrap_or(0));
        }
        let has = |t: PathType| set & (1 << (t as u32)) != 0;
        if has(PathType::Fence) {
            return PathType::Fence;
        }
        if has(PathType::UnpassableRail) {
            return PathType::UnpassableRail;
        }
        let mut highest_type = PathType::Blocked;
        let mut highest = Self::malus(eval, highest_type);
        let mut rest = set;
        while rest != 0 {
            let bit = rest.trailing_zeros();
            rest &= rest - 1;
            let t = PathType::from_u8(u8::try_from(bit).unwrap_or(0));
            let m = Self::malus(eval, t);
            if m < 0.0 {
                return t;
            }
            if m >= highest {
                highest = m;
                highest_type = t;
            }
        }
        let current = Self::block_type(eval, x, y, z);
        if eval.entity_width > 1 {
            let cheaper = Self::malus(eval, current) < highest;
            let cap = cheaper && Self::malus(eval, PathType::BigMobsCloseToDanger) < highest;
            if cap {
                PathType::BigMobsCloseToDanger
            } else {
                highest_type
            }
        } else if current == PathType::Open && highest_type != PathType::Open && highest == 0.0 {
            PathType::Open
        } else {
            highest_type
        }
    }

    // --- `AmphibiousNodeEvaluator` ----------------------------------------------------------

    fn amphibious_neighbors(
        &mut self,
        eval: &Eval<'_>,
        out: &mut [u32; 32],
        pos: u32,
        prefers_shallow: bool,
    ) -> usize {
        let mut count = self.walk_neighbors(eval, out, pos);
        let (px, py, pz) = {
            let n = self.n(pos);
            (n.x, n.y, n.z)
        };
        let jump_size = self.jump_size(eval, px, py, pz);
        let current = self.cached_type(eval, px, py, pz);
        let height = self.floor_level(eval, BlockPos::new(px, py, pz));
        let up = self.find_accepted_node(
            eval,
            px,
            py + 1,
            pz,
            (jump_size - 1).max(0),
            height,
            (0, 0),
            current,
        );
        let down =
            self.find_accepted_node(eval, px, py - 1, pz, jump_size, height, (0, 0), current);
        let vertical_valid = |s: &Self, n: Option<u32>| {
            n.is_some_and(|n| s.neighbor_valid(n, pos) && s.n(n).kind == PathType::Water)
        };
        if vertical_valid(self, up)
            && let Some(up) = up
        {
            out[count] = up;
            count += 1;
        }
        if vertical_valid(self, down)
            && current != PathType::Trapdoor
            && let Some(down) = down
        {
            out[count] = down;
            count += 1;
        }
        if prefers_shallow {
            let limit = eval.mob.sea_level - 10;
            for &n in out.iter().take(count) {
                let node = self.nm(n);
                if node.kind == PathType::Water && node.y < limit {
                    node.cost_malus += 1.0;
                }
            }
        }
        count
    }

    // --- `SwimNodeEvaluator` ----------------------------------------------------------------

    fn swim_neighbors(
        &mut self,
        eval: &Eval<'_>,
        out: &mut [u32; 32],
        pos: u32,
        breaching: bool,
    ) -> usize {
        let (px, py, pz) = {
            let n = self.n(pos);
            (n.x, n.y, n.z)
        };
        // `Direction.values()`: down, up, north, south, west, east.
        const DIRS: [(i32, i32, i32); 6] = [
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
            (-1, 0, 0),
            (1, 0, 0),
        ];
        let mut nodes = [NONE; 6];
        let mut count = 0;
        for (i, &(dx, dy, dz)) in DIRS.iter().enumerate() {
            let node = self.swim_accepted(eval, px + dx, py + dy, pz + dz, breaching);
            nodes[i] = node.unwrap_or(NONE);
            if let Some(n) = node
                && !self.n(n).closed
            {
                out[count] = n;
                count += 1;
            }
        }
        let has_malus = |s: &Self, n: u32| n != NONE && s.n(n).cost_malus >= 0.0;
        // `Direction.Plane.HORIZONTAL` (north, east, south, west) with the clockwise next.
        const HORIZONTAL: [(usize, usize); 4] = [(2, 5), (5, 3), (3, 4), (4, 2)];
        for &(a, b) in &HORIZONTAL {
            if has_malus(self, nodes[a]) && has_malus(self, nodes[b]) {
                let (ax, az) = (DIRS[a].0 + DIRS[b].0, DIRS[a].2 + DIRS[b].2);
                if let Some(d) = self.swim_accepted(eval, px + ax, py, pz + az, breaching)
                    && !self.n(d).closed
                {
                    out[count] = d;
                    count += 1;
                }
            }
        }
        count
    }

    /// `SwimNodeEvaluator.findAcceptedNode`.
    fn swim_accepted(
        &mut self,
        eval: &Eval<'_>,
        x: i32,
        y: i32,
        z: i32,
        breaching: bool,
    ) -> Option<u32> {
        let t = self.cached_type(eval, x, y, z);
        if !(breaching && t == PathType::Breach || t == PathType::Water) {
            return None;
        }
        let cost = Self::malus(eval, t);
        if cost < 0.0 {
            return None;
        }
        let i = self.node_cost_to_max(x, y, z, t, cost);
        if eval
            .blocks
            .state(BlockPos::new(x, y, z))
            .fluid_state()
            .is_empty()
        {
            self.nm(i).cost_malus += 8.0;
        }
        Some(i)
    }

    /// `SwimNodeEvaluator.getPathTypeOfMob`.
    fn swim_type_of_mob(eval: &Eval<'_>, x: i32, y: i32, z: i32) -> PathType {
        let blocks = eval.blocks;
        let mut last = BlockPos::new(x, y, z);
        for xx in x..x + eval.entity_width {
            for yy in y..y + eval.entity_height {
                for zz in z..z + eval.entity_depth {
                    let pos = BlockPos::new(xx, yy, zz);
                    last = pos;
                    let state = blocks.state(pos);
                    let fluid = state.fluid_state();
                    let below = blocks.state(BlockPos::new(xx, yy - 1, zz));
                    if fluid.is_empty()
                        && is_pathfindable(below, Computation::Water)
                        && state.is_air()
                    {
                        return PathType::Breach;
                    }
                    if !is_water(state) {
                        return PathType::Blocked;
                    }
                }
            }
        }
        if is_pathfindable(blocks.state(last), Computation::Water) {
            PathType::Water
        } else {
            PathType::Blocked
        }
    }

    // --- `FlyNodeEvaluator` -----------------------------------------------------------------

    /// `FlyNodeEvaluator.getStart`.
    fn fly_start(&mut self, eval: &Eval<'_>, random: &mut LegacyRandom) -> u32 {
        let mob = eval.mob;
        let start_y = if mob.can_float && mob.in_water {
            let mut y = floor(mob.pos.y);
            while eval
                .blocks
                .state(BlockPos::containing(mob.pos.x, f64::from(y), mob.pos.z))
                .block()
                == ids::WATER
            {
                y += 1;
            }
            y
        } else {
            floor(mob.pos.y + 0.5)
        };
        let start = BlockPos::containing(mob.pos.x, f64::from(start_y), mob.pos.z);
        if !self.can_start_at(eval, start.x, start.y, start.z) {
            let b = mob.aabb;
            let (xs, ys, zs) = (b.max_x - b.min_x, b.max_y - b.min_y, b.max_z - b.min_z);
            let block_y = floor(mob.pos.y);
            if (xs + ys + zs) / 3.0 >= 1.0 {
                for (x, z) in [
                    (b.min_x, b.min_z),
                    (b.min_x, b.max_z),
                    (b.max_x, b.min_z),
                    (b.max_x, b.max_z),
                ] {
                    let p = BlockPos::containing(x, f64::from(block_y), z);
                    if self.can_start_at(eval, p.x, p.y, p.z) {
                        return self.start_node(eval, p.x, p.y, p.z);
                    }
                }
            } else {
                // `BlockPos.randomBetweenClosed(random, 10, ...)` over the box grown to 1.1.
                let pad = |size: f64| (f64::from(1.1_f32) - size).max(0.0);
                let grown = b.inflate(pad(xs), pad(ys), pad(zs));
                let (x0, y0, z0) = (floor(grown.min_x), floor(grown.min_y), floor(grown.min_z));
                let (x1, y1, z1) = (floor(grown.max_x), floor(grown.max_y), floor(grown.max_z));
                for _ in 0..10 {
                    let x = x0 + random.next_int_bounded(x1 - x0 + 1);
                    let y = y0 + random.next_int_bounded(y1 - y0 + 1);
                    let z = z0 + random.next_int_bounded(z1 - z0 + 1);
                    if self.can_start_at(eval, x, y, z) {
                        return self.start_node(eval, x, y, z);
                    }
                }
            }
        }
        self.start_node(eval, start.x, start.y, start.z)
    }

    /// `FlyNodeEvaluator.findAcceptedNode`.
    fn fly_accepted(&mut self, eval: &Eval<'_>, x: i32, y: i32, z: i32) -> u32 {
        let t = self.cached_type(eval, x, y, z);
        let cost = Self::malus(eval, t);
        if cost < 0.0 {
            return NONE;
        }
        let i = self.node_cost_to_max(x, y, z, t, cost);
        if t == PathType::Walkable {
            self.nm(i).cost_malus += 1.0;
        }
        i
    }

    fn fly_neighbors(&mut self, eval: &Eval<'_>, out: &mut [u32; 32], pos: u32) -> usize {
        let (px, py, pz) = {
            let n = self.n(pos);
            (n.x, n.y, n.z)
        };
        let mut count = 0;
        let open = |s: &Self, n: u32| n != NONE && !s.n(n).closed;
        let malus = |s: &Self, n: u32| n != NONE && s.n(n).cost_malus >= 0.0;
        let a = |s: &mut Self, dx: i32, dy: i32, dz: i32| {
            s.fly_accepted(eval, px + dx, py + dy, pz + dz)
        };
        let south = a(self, 0, 0, 1);
        let west = a(self, -1, 0, 0);
        let east = a(self, 1, 0, 0);
        let north = a(self, 0, 0, -1);
        let up = a(self, 0, 1, 0);
        let down = a(self, 0, -1, 0);
        for n in [south, west, east, north, up, down] {
            if open(self, n) {
                out[count] = n;
                count += 1;
            }
        }
        let south_up = a(self, 0, 1, 1);
        let west_up = a(self, -1, 1, 0);
        let east_up = a(self, 1, 1, 0);
        let north_up = a(self, 0, 1, -1);
        let south_down = a(self, 0, -1, 1);
        let west_down = a(self, -1, -1, 0);
        let east_down = a(self, 1, -1, 0);
        let north_down = a(self, 0, -1, -1);
        // The candidates are accepted in Vanilla's order, interleaved with the next lookup.
        let push = |s: &Self, n: u32, ok: bool, out: &mut [u32; 32], count: &mut usize| {
            if open(s, n) && ok {
                out[*count] = n;
                *count += 1;
            }
        };
        // Vanilla looks each node up right before testing it; a lookup only touches its own
        // node, so looking them up first gives the same nodes.
        let checks = [
            (south_up, malus(self, south) && malus(self, up)),
            (west_up, malus(self, west) && malus(self, up)),
            (east_up, malus(self, east) && malus(self, up)),
            (north_up, malus(self, north) && malus(self, up)),
            (south_down, malus(self, south) && malus(self, down)),
            (west_down, malus(self, west) && malus(self, down)),
            (east_down, malus(self, east) && malus(self, down)),
            (north_down, malus(self, north) && malus(self, down)),
        ];
        for (n, ok) in checks {
            push(self, n, ok, out, &mut count);
        }
        let north_east = a(self, 1, 0, -1);
        let south_east = a(self, 1, 0, 1);
        let north_west = a(self, -1, 0, -1);
        let south_west = a(self, -1, 0, 1);
        let checks = [
            (north_east, malus(self, north) && malus(self, east)),
            (south_east, malus(self, south) && malus(self, east)),
            (north_west, malus(self, north) && malus(self, west)),
            (south_west, malus(self, south) && malus(self, west)),
        ];
        for (n, ok) in checks {
            push(self, n, ok, out, &mut count);
        }
        let corners = [
            (1, 1, -1, north_east, north, east, up, north_up, east_up),
            (1, 1, 1, south_east, south, east, up, south_up, east_up),
            (-1, 1, -1, north_west, north, west, up, north_up, west_up),
            (-1, 1, 1, south_west, south, west, up, south_up, west_up),
            (
                1, -1, -1, north_east, north, east, down, north_down, east_down,
            ),
            (
                1, -1, 1, south_east, south, east, down, south_down, east_down,
            ),
            (
                -1, -1, -1, north_west, north, west, down, north_down, west_down,
            ),
            (
                -1, -1, 1, south_west, south, west, down, south_down, west_down,
            ),
        ];
        for (dx, dy, dz, diag, h1, h2, v, v1, v2) in corners {
            let n = a(self, dx, dy, dz);
            let ok = malus(self, diag)
                && malus(self, h1)
                && malus(self, h2)
                && malus(self, v)
                && malus(self, v1)
                && malus(self, v2);
            push(self, n, ok, out, &mut count);
        }
        count
    }
}

/// `FlyNodeEvaluator.getPathType` of one block.
pub fn fly_path_type(
    blocks: &dyn Blocks,
    min_y: i32,
    mob_pos: BlockPos,
    x: i32,
    y: i32,
    z: i32,
) -> PathType {
    let mut t = state_type(blocks.state(BlockPos::new(x, y, z)));
    if t == PathType::Open && y > min_y {
        let below = BlockPos::new(x, y - 1, z);
        t = match state_type(blocks.state(below)) {
            PathType::Fire | PathType::Lava => PathType::Fire,
            PathType::Damaging => PathType::Damaging,
            PathType::Cocoa => PathType::Cocoa,
            PathType::Fence => {
                if below == mob_pos {
                    PathType::Open
                } else {
                    PathType::Fence
                }
            }
            PathType::Walkable | PathType::Open | PathType::Water => PathType::Open,
            _ => PathType::Walkable,
        };
    }
    if t == PathType::Walkable || t == PathType::Open {
        t = check_neighbour_blocks(blocks, x, y, z, t);
    }
    t
}

/// `AmphibiousNodeEvaluator.getPathType` of one block: water next to a blocked block is its
/// border.
pub fn amphibious_path_type(blocks: &dyn Blocks, min_y: i32, x: i32, y: i32, z: i32) -> PathType {
    if state_type(blocks.state(BlockPos::new(x, y, z))) == PathType::Water {
        const DIRS: [(i32, i32, i32); 6] = [
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
            (-1, 0, 0),
            (1, 0, 0),
        ];
        for (dx, dy, dz) in DIRS {
            if state_type(blocks.state(BlockPos::new(x + dx, y + dy, z + dz))) == PathType::Blocked
            {
                return PathType::WaterBorder;
            }
        }
        return PathType::Water;
    }
    path_type_static(blocks, min_y, x, y, z)
}

/// `#minecraft:frog_prefer_jump_to`.
fn is_frog_jump_target(state: BlockState) -> bool {
    matches!(
        state.block().name(),
        "minecraft:lily_pad" | "minecraft:big_dripleaf"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flat;
    impl Blocks for Flat {
        fn state(&self, pos: BlockPos) -> BlockState {
            if pos.y < 64 || (pos.y == 64 && pos.x == 3 && pos.z <= 2) {
                ids::STONE.default_state()
            } else {
                BlockState::AIR
            }
        }
    }

    /// A pool: water from y 60 to 63 for x in 0..10, z in -3..=3, stone around and below.
    struct Pool;
    impl Blocks for Pool {
        fn state(&self, pos: BlockPos) -> BlockState {
            let inside = (0..10).contains(&pos.x) && (-3..=3).contains(&pos.z);
            if inside && (60..=63).contains(&pos.y) {
                ids::WATER.default_state()
            } else if pos.y < 64 {
                ids::STONE.default_state()
            } else {
                BlockState::AIR
            }
        }
    }

    /// A gap: ground at y 63 for x <= 2 and x >= 8, nothing in between down to the void.
    struct Gap;
    impl Blocks for Gap {
        fn state(&self, pos: BlockPos) -> BlockState {
            if pos.y == 63 && (pos.x <= 2 || pos.x >= 8) {
                ids::STONE.default_state()
            } else {
                BlockState::AIR
            }
        }
    }

    /// A wall at x = 3 with a closed oak door at z = 0.
    struct Door;
    impl Blocks for Door {
        fn state(&self, pos: BlockPos) -> BlockState {
            if pos.y < 64 {
                return ids::STONE.default_state();
            }
            if pos.x == 3 && pos.y <= 66 {
                if pos.z == 0 && pos.y <= 65 {
                    let door = ids::OAK_DOOR.default_state();
                    let half = if pos.y == 64 { "lower" } else { "upper" };
                    return door.with("half", half).unwrap_or(door);
                }
                return ids::STONE.default_state();
            }
            BlockState::AIR
        }
    }

    fn mob(pos: Vec3) -> PathMob {
        let aabb = Aabb::new(
            pos.x - 0.3,
            pos.y,
            pos.z - 0.3,
            pos.x + 0.3,
            pos.y + 1.95,
            pos.z + 0.3,
        );
        PathMob::walking(pos, aabb, 0.6, 1.95)
    }

    fn search(malus: &MalusTable) -> Search<'_> {
        Search {
            max_path_length: 16.0,
            reach_range: 0,
            max_visited_nodes: 512,
            malus,
            min_y: -64,
        }
    }

    fn find(blocks: &dyn Blocks, mob: &PathMob, to: BlockPos, malus: &MalusTable) -> Path {
        let mut finder = PathFinder::new();
        let mut random = LegacyRandom::new(1);
        finder
            .find_path(blocks, mob, &[to], search(malus), &mut random)
            .unwrap_or_else(|| panic!("no start"))
    }

    #[test]
    fn finds_a_path_over_a_step() {
        let path = find(
            &Flat,
            &mob(Vec3::new(0.5, 64.0, 0.5)),
            BlockPos::new(6, 64, 0),
            &[],
        );
        assert!(path.reached);
        assert_eq!(
            path.nodes.first().map(PathNode::pos),
            Some(BlockPos::new(0, 64, 0))
        );
        assert_eq!(
            path.nodes.last().map(PathNode::pos),
            Some(BlockPos::new(6, 64, 0))
        );
        // The wall at x = 3 is one block high: the path jumps on it.
        assert!(path.nodes.iter().any(|n| n.y == 65));
    }

    #[test]
    fn walkable_ground_and_open_air() {
        assert_eq!(path_type_static(&Flat, -64, 0, 64, 0), PathType::Walkable);
        assert_eq!(path_type_static(&Flat, -64, 0, 66, 0), PathType::Open);
        assert_eq!(path_type_static(&Flat, -64, 0, 63, 0), PathType::Blocked);
    }

    #[test]
    fn fish_swim_through_water_only() {
        let pos = Vec3::new(1.5, 61.0, 0.5);
        let aabb = Aabb::new(
            pos.x - 0.25,
            pos.y,
            pos.z - 0.25,
            pos.x + 0.25,
            pos.y + 0.3,
            pos.z + 0.25,
        );
        let mut fish = PathMob::walking(pos, aabb, 0.5, 0.3);
        fish.evaluator = Evaluator::Swim { breaching: false };
        fish.in_water = true;
        fish.can_pass_doors = false;
        let path = find(&Pool, &fish, BlockPos::new(8, 61, 0), &[]);
        assert!(path.reached);
        assert!(
            path.nodes
                .iter()
                .all(|n| (60..=63).contains(&n.y) && is_water(Pool.state(n.pos())))
        );
        // Out of the water is never a node.
        let out = find(&Pool, &fish, BlockPos::new(8, 66, 0), &[]);
        assert!(!out.reached);
        assert!(out.nodes.iter().all(|n| n.y <= 63));
    }

    #[test]
    fn dolphins_breach_the_surface() {
        assert_eq!(
            PathFinder::swim_type_of_mob(
                &Eval {
                    blocks: &Pool,
                    statics: None,
                    mob: &mob(Vec3::new(1.5, 61.0, 0.5)),
                    search: search(&[]),
                    entity_width: 1,
                    entity_height: 1,
                    entity_depth: 1,
                },
                1,
                64,
                0
            ),
            PathType::Breach
        );
    }

    #[test]
    fn flyers_cross_a_gap() {
        let pos = Vec3::new(1.5, 64.0, 0.5);
        let aabb = Aabb::new(
            pos.x - 0.25,
            pos.y,
            pos.z - 0.25,
            pos.x + 0.25,
            pos.y + 0.9,
            pos.z + 0.25,
        );
        let mut parrot = PathMob::walking(pos, aabb, 0.5, 0.9);
        parrot.evaluator = Evaluator::Fly;
        let path = find(&Gap, &parrot, BlockPos::new(9, 64, 0), &[]);
        assert!(path.reached);
        // A walker cannot: the gap is deeper than its fall distance.
        let walker = find(&Gap, &mob(pos), BlockPos::new(9, 64, 0), &[]);
        assert!(!walker.reached);
    }

    #[test]
    fn amphibious_mobs_walk_into_the_water_and_swim() {
        let pos = Vec3::new(12.5, 64.0, 0.5);
        let aabb = Aabb::new(
            pos.x - 0.6,
            pos.y,
            pos.z - 0.6,
            pos.x + 0.6,
            pos.y + 0.4,
            pos.z + 0.6,
        );
        let mut turtle = PathMob::walking(pos, aabb, 1.2, 0.4);
        turtle.evaluator = Evaluator::Amphibious {
            prefers_shallow: false,
            frog: false,
        };
        let path = find(&Pool, &turtle, BlockPos::new(3, 61, 0), &[]);
        assert!(path.reached);
        assert!(path.nodes.iter().any(|n| n.y < 63));
        // The amphibious evaluator treats water as free.
        let walker = find(&Pool, &mob(pos), BlockPos::new(3, 61, 0), &[]);
        assert!(!walker.reached);
    }

    #[test]
    fn doors_need_opening() {
        let pos = Vec3::new(1.5, 64.0, 0.5);
        let closed = find(&Door, &mob(pos), BlockPos::new(5, 64, 0), &[]);
        assert!(!closed.reached);
        let mut villager = mob(pos);
        villager.can_open_doors = true;
        let open = find(&Door, &villager, BlockPos::new(5, 64, 0), &[]);
        assert!(open.reached);
        assert!(
            open.nodes
                .iter()
                .any(|n| n.pos() == BlockPos::new(3, 64, 0) && n.kind == PathType::WalkableDoor)
        );
    }

    #[test]
    fn pathfindable_by_type() {
        let water = ids::WATER.default_state();
        assert!(is_pathfindable(water, Computation::Water));
        assert!(is_pathfindable(BlockState::AIR, Computation::Air));
        assert!(!is_pathfindable(
            ids::STONE.default_state(),
            Computation::Air
        ));
        assert!(!is_pathfindable(
            ids::OAK_DOOR.default_state(),
            Computation::Water
        ));
    }
}
