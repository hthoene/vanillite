//! The pure read view of a level.
//!
//! A [`Level`](super::Level) has two read paths:
//!
//! - The tick view, [`Level::block`](super::Level::block) and the other `&Level` reads: the
//!   world thread's `getBlockState`. It goes through Vanilla's chunk cache, which keeps the
//!   `unknown` ticket of a synchronously loaded chunk alive, so it has a side effect and stays
//!   on the world thread.
//! - The read view, [`LevelView`]: reads without side effects, as Vanilla's collision
//!   (`getChunkForCollisions`) and path finding (`PathNavigationRegion`, `getChunkNow`) read.
//!   It is `Copy + Send + Sync`, so phases that only read the world can run on other threads
//!   while the world thread does not write.
//!
//! Code that only reads blocks and models a Vanilla read without tickets takes a
//! [`LevelView`] (or `&dyn` [`Blocks`]), never `&Level`.

use vanillite_base::{BlockPos, ChunkPos};
use vanillite_data::block::BlockState;
use vanillite_data::fluid::FluidState;
use vanillite_world::chunks::ChunkView;

use super::border::WorldBorder;
use crate::block;
use crate::mob::physics::Blocks;
use crate::survival::blocks::BlockAccess;

/// A pure, read-only view of a level's blocks and world border.
#[derive(Clone, Copy, Debug)]
pub struct LevelView<'a> {
    chunks: ChunkView<'a>,
    border: &'a WorldBorder,
    block_entities: &'a super::block_entity::BlockEntities,
    min_y: i32,
    max_y: i32,
}

const _: () = {
    const fn shareable<T: Send + Sync + Copy>() {}
    shareable::<LevelView<'static>>();
};

impl<'a> LevelView<'a> {
    pub(super) fn new(
        chunks: ChunkView<'a>,
        border: &'a WorldBorder,
        block_entities: &'a super::block_entity::BlockEntities,
        min_y: i32,
        max_y: i32,
    ) -> LevelView<'a> {
        LevelView {
            chunks,
            border,
            block_entities,
            min_y,
            max_y,
        }
    }

    /// The block at `pos` like [`Level::block`](super::Level::block): void air below and above
    /// the world, air in chunks that are not live.
    #[inline]
    pub fn block(&self, pos: BlockPos) -> BlockState {
        match self.chunks.block(pos) {
            Some(state) => BlockState(state),
            None if pos.y >= self.min_y && pos.y <= self.max_y => BlockState::AIR,
            None => block::VOID_AIR,
        }
    }

    /// `getFluidState(pos)`.
    #[inline]
    pub fn fluid(&self, pos: BlockPos) -> FluidState {
        self.block(pos).fluid_state()
    }

    /// The chunks.
    #[inline]
    pub fn chunks(&self) -> ChunkView<'a> {
        self.chunks
    }

    /// The world border.
    #[inline]
    pub fn border(&self) -> &'a WorldBorder {
        self.border
    }
}

impl Blocks for LevelView<'_> {
    #[inline]
    fn state(&self, pos: BlockPos) -> BlockState {
        self.block(pos)
    }

    fn states_in(&self, min: BlockPos, max: BlockPos, out: &mut [BlockState]) {
        let mut slots = out.iter_mut();
        let mut y = min.y;
        self.chunks.blocks_in(min, max, |state| {
            if let Some(slot) = slots.next() {
                *slot = match state {
                    Some(state) => BlockState(state),
                    None if y >= self.min_y && y <= self.max_y => BlockState::AIR,
                    None => block::VOID_AIR,
                };
            }
            y = if y == max.y { min.y } else { y + 1 };
        });
    }

    #[inline]
    fn border(&self) -> Option<&WorldBorder> {
        Some(self.border)
    }

    fn moving_piston_boxes(&self, pos: BlockPos, out: &mut Vec<[f64; 6]>) {
        crate::block::piston::collision_boxes_at(self.block_entities, pos, out);
    }

    #[inline]
    fn plain_collision(&self, x: i32, y: i32, z: i32) -> bool {
        // Outside full chunks and the build height every read is (void) air.
        self.chunks
            .section_summary(ChunkPos::new(x, z), y)
            .is_none_or(|summary| summary.special_collision == 0)
    }
}

impl BlockAccess for LevelView<'_> {
    #[inline]
    fn block_state(&self, pos: BlockPos) -> Option<BlockState> {
        self.chunks.block(pos).map(BlockState)
    }

    fn moving_piston_boxes(&self, pos: BlockPos, out: &mut Vec<[f64; 6]>) {
        crate::block::piston::collision_boxes_at(self.block_entities, pos, out);
    }
}
