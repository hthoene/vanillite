//! Block behaviour that world generation and the live world share: survival checks,
//! post-processing marks, shape updates and face sturdiness (`BlockBehaviour`, `BlockState` and
//! the `Block` subclasses of Vanilla 26.3).
//!
//! Every block maps to the Vanilla class that gives it its behaviour (a table derived from
//! `Blocks.java`); the functions here dispatch on that class and read the level through
//! [`BlockLevel`], which the generation region ([`WorldGenRegion`]) and the game's live level
//! implement. Block and fluid tags come from the data packs ([`init`]).
//!
//! Not ported (the state is kept unchanged): `BeehiveBlock.updateShape` (releases bees next to fire) and the block entity side
//! effect of `PotentSulfurBlock.updateShape` (geyser countdown reset). Shapes are the
//! precomputed ones of an empty level without the random offset of offset blocks (bamboo,
//! pointed dripstone, …), as everywhere in generation.

mod class;
mod geometry;
mod portal;
mod props;
mod survive;
mod tables;
mod update;

#[cfg(test)]
mod tests;

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Direction};
use vanillite_data::block::{BlockId, BlockState, flags, ids};
use vanillite_data::fluid::FluidState;

use crate::feature::region::WorldGenRegion;

pub use class::{Class, class_of, class_of_block};
pub use geometry::{is_full_block, merged_face_occludes};
pub use portal::{HorizontalAxis, MAX_PORTAL_SIZE, PortalShape, is_frame, relative_n};
pub use props::{
    HORIZONTAL, IntProperty, PosExt, StateExt, UPDATE_SHAPE_ORDER, WireProperties, clockwise,
    counter_clockwise, is_horizontal, wire_properties, with_properties_of,
};
pub use survive::{redstone_wire_can_survive_on, scaffolding_distance};
pub use tables::{BlockTags, FluidTags, Tags, can_burn, init, tags};
pub use update::{
    concrete_of, fire_state_with_age, should_wire_connect_to, touches_liquid, wire_connecting_side,
    wire_connection_state,
};

/// What block behaviour reads from and schedules in a level (Vanilla's `LevelReader` plus
/// `ScheduledTickAccess` and the level random, as far as shape updates and survival checks use
/// them): the generation region or the live world.
pub trait BlockLevel {
    /// `getBlockState(pos)`.
    fn block(&self, pos: BlockPos) -> BlockState;

    /// `getMinY()`: the lowest block row. Without bounds, reads below the level are void air,
    /// which stops every search there as well.
    fn min_y(&self) -> i32 {
        i32::MIN
    }

    /// `getFluidState(pos)`.
    #[inline]
    fn fluid(&self, pos: BlockPos) -> FluidState {
        self.block(pos).fluid_state()
    }

    /// `isEmptyBlock(pos)`: the block is air (any air).
    #[inline]
    fn is_empty_block(&self, pos: BlockPos) -> bool {
        self.block(pos).is_air()
    }

    /// `getRawBrightness(pos, 0)`: the higher of sky and block light.
    fn raw_brightness(&self, pos: BlockPos) -> i32;

    /// `random.nextInt(bound)` of the level's random.
    fn random_int(&mut self, bound: i32) -> i32;

    /// `scheduleTick(pos, block, delay)`.
    fn schedule_block_tick(&mut self, pos: BlockPos, block: BlockId, delay: i32);

    /// `scheduleTick(pos, fluid, fluid.getTickDelay(level))`; `fluid` is a `minecraft:fluid`
    /// id.
    fn schedule_fluid_tick(&mut self, pos: BlockPos, fluid: u16);

    /// `getBlockTicks().hasScheduledTick(pos, block)`.
    fn has_block_tick(&self, pos: BlockPos, block: BlockId) -> bool;

    /// The collision boxes (relative to `pos`) of the moving piston at `pos`, from its block
    /// entity (`PistonMovingBlockEntity.getCollisionShape`); none without one.
    fn moving_piston_boxes(&self, _pos: BlockPos, _out: &mut Vec<[f64; 6]>) {}

    /// `ComparatorBlockEntity.getOutputSignal()` of the comparator at `pos`.
    fn comparator_output(&self, _pos: BlockPos) -> i32 {
        0
    }
}

impl BlockLevel for WorldGenRegion<'_> {
    #[inline]
    fn block(&self, pos: BlockPos) -> BlockState {
        WorldGenRegion::block(self, pos)
    }

    /// Chunks in the features stage are not lit yet: the block light engine has no data (0) and
    /// the sky light engine reports full light (15) for columns it has not seen. Dimensions
    /// without sky light have no sky engine (0).
    fn raw_brightness(&self, _pos: BlockPos) -> i32 {
        if self.has_sky_light() { 15 } else { 0 }
    }

    fn random_int(&mut self, bound: i32) -> i32 {
        self.random().next_int_bounded(bound)
    }

    /// Generation ticks are stored with delay 0 (`WorldGenTickAccess`); a live world gets the
    /// delay.
    fn schedule_block_tick(&mut self, pos: BlockPos, block: BlockId, delay: i32) {
        if let Some(live) = self.live_level() {
            return live.schedule_block_tick(pos, block, delay);
        }
        WorldGenRegion::schedule_block_tick(self, pos, block);
    }

    fn schedule_fluid_tick(&mut self, pos: BlockPos, fluid: u16) {
        if let Some(live) = self.live_level() {
            // `Fluid.getTickDelay` of the overworld: water 5, lava 30.
            let delay = if matches!(fluid, 3 | 4) { 30 } else { 5 };
            return live.schedule_fluid_tick(pos, fluid, delay);
        }
        WorldGenRegion::schedule_fluid_tick(self, pos, fluid);
    }

    fn has_block_tick(&self, pos: BlockPos, block: BlockId) -> bool {
        self.chunk(pos).block_ticks().has(block, pos)
    }

    /// From the block entity data.
    fn comparator_output(&self, pos: BlockPos) -> i32 {
        match self
            .chunk(pos)
            .block_entity(pos)
            .and_then(|tag| tag.get("OutputSignal"))
        {
            Some(vanillite_nbt::Tag::Int(value)) => *value,
            _ => 0,
        }
    }
}

/// `SupportType` of `isFaceSturdy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupportType {
    Full,
    Center,
    Rigid,
}

/// `state.canSurvive(level, pos)`.
pub fn can_survive(level: &impl BlockLevel, state: BlockState, pos: BlockPos) -> bool {
    survive::can_survive(level, state, pos)
}

/// `state.getPostProcessPos(level, pos)`: the position to mark for post-processing after
/// `setBlock`, for the blocks with a `postProcess` predicate in `Blocks` (mushrooms mark
/// themselves, soul sand and magma blocks the block above).
pub fn post_process_pos(
    _level: &impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
) -> Option<BlockPos> {
    let block = state.block();
    if block == ids::BROWN_MUSHROOM || block == ids::RED_MUSHROOM {
        Some(pos)
    } else if block == ids::SOUL_SAND || block == ids::MAGMA_BLOCK {
        Some(pos.above())
    } else {
        None
    }
}

/// `state.updateShape(level, ticks, pos, direction, neighbourPos, neighbourState, random)`: the
/// state after its neighbour in `direction` changed. Ticks it schedules go to the region, the
/// random it uses is the region's (`level.getRandom()`, as all generation callers pass).
pub fn update_shape(
    level: &mut impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
    direction: Direction,
    neighbour_pos: BlockPos,
    neighbour_state: BlockState,
) -> BlockState {
    update::update_shape(level, state, pos, direction, neighbour_pos, neighbour_state)
}

/// `Block.updateFromNeighbourShapes(state, level, pos)`: [`update_shape`] for all six
/// directions in `UPDATE_SHAPE_ORDER` (west, east, north, south, down, up).
pub fn update_from_neighbour_shapes(
    level: &mut impl BlockLevel,
    state: BlockState,
    pos: BlockPos,
) -> BlockState {
    let mut state = state;
    for direction in props::UPDATE_SHAPE_ORDER {
        let neighbour_pos = pos.relative(direction);
        let neighbour = level.block(neighbour_pos);
        state = update_shape(level, state, pos, direction, neighbour_pos, neighbour);
    }
    state
}

/// `state.isFaceSturdy(level, pos, direction, support)` from the state's support shape.
pub fn is_face_sturdy(state: BlockState, direction: Direction, support: SupportType) -> bool {
    let misc = state.misc();
    let faces = match support {
        SupportType::Full => misc.sturdy_full,
        SupportType::Center => misc.sturdy_center,
        SupportType::Rigid => misc.sturdy_rigid,
    };
    faces & (1 << direction as u8) != 0
}

/// `state.isFaceSturdy(level, pos, direction, support)` of `state` at `pos`: like
/// [`is_face_sturdy`], but a moving piston's support shape is the collision shape of its block
/// entity (`MovingPistonBlock` has a dynamic shape).
pub fn face_sturdy_at(
    level: &impl BlockLevel,
    pos: BlockPos,
    state: BlockState,
    direction: Direction,
    support: SupportType,
) -> bool {
    if !state.is(ids::MOVING_PISTON) {
        return is_face_sturdy(state, direction, support);
    }
    let mut boxes = Vec::new();
    level.moving_piston_boxes(pos, &mut boxes);
    geometry::boxes_support(&boxes, direction, support)
}

/// `Block.canSupportRigidBlock(level, pos)`: the top of the block at `pos` is rigid.
pub fn can_support_rigid_block(level: &impl BlockLevel, pos: BlockPos) -> bool {
    face_sturdy_at(
        level,
        pos,
        level.block(pos),
        Direction::Up,
        SupportType::Rigid,
    )
}

/// `Block.canSupportCenter(level, pos, direction)`.
pub fn can_support_center(level: &impl BlockLevel, pos: BlockPos, direction: Direction) -> bool {
    let state = level.block(pos);
    if direction == Direction::Down && tables::tags().blocks.unstable_bottom_center.contains(state)
    {
        return false;
    }
    face_sturdy_at(level, pos, state, direction, SupportType::Center)
}

/// `MultifaceBlock.canAttachTo(level, direction, neighbourPos, neighbourState)`: whether a
/// face towards `direction` can attach to `neighbour` (its opposite face is full in the support
/// or the collision shape).
pub fn can_attach_multiface(neighbour: BlockState, direction: Direction) -> bool {
    let face = direction.opposite();
    is_face_sturdy(neighbour, face, SupportType::Full)
        || geometry::is_face_full(neighbour.collision_shape(), face)
}

/// `Block.isFaceFull(state.getCollisionShape(level, pos), direction)`.
pub fn is_collision_face_full(state: BlockState, direction: Direction) -> bool {
    geometry::is_face_full(state.collision_shape(), direction)
}

/// `level.getRawBrightness(pos, 0)`.
pub fn raw_brightness(level: &impl BlockLevel, pos: BlockPos) -> i32 {
    level.raw_brightness(pos)
}

/// `state.isSolid()` (the legacy "solid" flag).
pub fn is_solid(state: BlockState) -> bool {
    state.has(flags::SOLID)
}

/// `state.canBeReplaced()`.
pub fn can_be_replaced(state: BlockState) -> bool {
    state.has(flags::REPLACEABLE)
}

/// `state.isSolidRender()`.
pub fn is_solid_render(state: BlockState) -> bool {
    state.has(flags::SOLID_RENDER)
}
