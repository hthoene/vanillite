//! `ProtoChunk`: a chunk while it is being generated.
//!
//! Block and biome data live in [`vanillite_chunk::Section`]s, so a finished proto chunk turns
//! into a [`vanillite_chunk::Chunk`] without copying. Heightmaps are kept as plain `i32`s: up to
//! the terrain stage the two world generation heightmaps (`WORLD_SURFACE_WG`, `OCEAN_FLOOR_WG`)
//! follow block changes; after it they are frozen and the four final heightmaps follow instead
//! (`ChunkStatus.heightmapsAfter`).

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use vanillite_base::{BlockPos, ChunkPos};
use vanillite_chunk::{
    BiomeId, Chunk, ChunkLight, HeightmapKind, PalettedContainer, Section, facts,
};
use vanillite_data::block::{BlockId, BlockState};
use vanillite_nbt::Compound;

use crate::structure::start::ChunkStarts;
use crate::structure::{LongOpenHashSet, StructureId, StructureStart};

/// A world generation heightmap (`Heightmap.Types.*_WG`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WgHeightmap {
    /// `WORLD_SURFACE_WG`: highest non-air block.
    WorldSurface,
    /// `OCEAN_FLOOR_WG`: highest block in `blocks_motion_in_heightmap`.
    OceanFloor,
}

impl WgHeightmap {
    /// The heightmap type.
    pub const fn kind(self) -> HeightmapKind {
        match self {
            Self::WorldSurface => HeightmapKind::WorldSurfaceWg,
            Self::OceanFloor => HeightmapKind::OceanFloorWg,
        }
    }
}

/// The heightmaps that follow block changes after the terrain stage (`Heightmap.Types` with
/// `Usage.LIVE_WORLD` and `WORLDGEN`, in `EnumSet` order).
const FINAL: [HeightmapKind; 4] = [
    HeightmapKind::WorldSurface,
    HeightmapKind::OceanFloor,
    HeightmapKind::MotionBlocking,
    HeightmapKind::MotionBlockingNoLeaves,
];

/// A tick scheduled during generation (`SavedTick` in `ProtoChunkTicks`): the delay is always 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProtoTick<T> {
    /// The block or fluid that ticks.
    pub kind: T,
    pub pos: BlockPos,
    /// `TickPriority` value (-3 extremely high .. 3 extremely low).
    pub priority: i8,
}

/// Ticks scheduled during generation, deduplicated by type and position (`ProtoChunkTicks`).
#[derive(Clone, Debug)]
pub struct ProtoTicks<T> {
    ticks: Vec<ProtoTick<T>>,
    unique: FxHashSet<(T, BlockPos)>,
}

impl<T> Default for ProtoTicks<T> {
    fn default() -> Self {
        Self {
            ticks: Vec::new(),
            unique: FxHashSet::default(),
        }
    }
}

impl<T: Copy + Eq + std::hash::Hash> ProtoTicks<T> {
    /// `schedule(tick)`: ignored if a tick of the same type is already at the position.
    pub fn schedule(&mut self, kind: T, pos: BlockPos, priority: i8) {
        if self.unique.insert((kind, pos)) {
            self.ticks.push(ProtoTick {
                kind,
                pos,
                priority,
            });
        }
    }

    /// `hasScheduledTick(pos, type)`.
    pub fn has(&self, kind: T, pos: BlockPos) -> bool {
        self.unique.contains(&(kind, pos))
    }

    /// The ticks in scheduling order.
    pub fn ticks(&self) -> &[ProtoTick<T>] {
        &self.ticks
    }
}

/// The type of a scheduled fluid tick: the `minecraft:fluid` id of the ticking fluid (flowing and
/// still fluids are different types, as in Vanilla).
pub type FluidTickKind = u16;

/// A chunk during generation.
#[derive(Clone, Debug)]
pub struct ProtoChunk {
    pos: ChunkPos,
    min_y: i32,
    sections: Vec<Section>,
    /// First free Y above the highest matching block per column (`x + z * 16`), per heightmap,
    /// indexed by [`HeightmapKind::id`].
    heightmaps: [[i32; 256]; 6],
    /// Whether the terrain stage is done: block changes update the final heightmaps from then on.
    terrain_done: bool,
    /// Whether the final heightmaps equal a fresh `primeHeightmaps` of the blocks: true from
    /// the terrain's end until a block changes without updating them.
    final_heightmaps_exact: bool,
    /// Positions (`BlockPos.asLong` layout is not needed: x, y, z) marked for post-processing.
    post_processing: Vec<(i32, i32, i32)>,
    /// Block entity data by position (`setBlockEntityNbt`, `DUMMY` placeholders included).
    block_entities: FxHashMap<BlockPos, Compound>,
    block_ticks: ProtoTicks<BlockId>,
    fluid_ticks: ProtoTicks<FluidTickKind>,
    /// `structureStarts`: the starts generated in this chunk, in insertion order.
    structure_starts: ChunkStarts,
    /// `structureReferences`: per structure, the start chunks (`ChunkPos.pack`) whose starts
    /// reach this chunk, in order of the first reference.
    structure_references: Vec<(StructureId, LongOpenHashSet)>,
    /// `entities`: entities added during generation (structure mobs, template entities), as
    /// their saved NBT (`id`, `Pos`, ...), in the order they were added.
    entities: Vec<Compound>,
    /// The blocks while the terrain stage builds them ([`Self::begin_dense`] to
    /// [`Self::end_dense`]); the sections' blocks are stale meanwhile.
    dense: Option<DenseBlocks>,
}

/// Block states of the whole chunk as one plain array, column after column (`x + z * 16`),
/// each bottom up, with the non-air count per section: the terrain stage writes nearly every
/// block of a chunk, column by column, which is much cheaper here than in paletted sections.
#[derive(Clone, Debug)]
struct DenseBlocks {
    states: Vec<u16>,
    non_air: Vec<u16>,
    /// Blocks per column.
    height: usize,
}

impl DenseBlocks {
    /// The index of a position: x/z (any, the low four bits count) and `row`, the Y above the
    /// chunk's bottom (below `height`).
    #[inline]
    fn index(&self, x: i32, row: usize, z: i32) -> usize {
        ((((z & 15) as usize) << 4) | (x & 15) as usize) * self.height + row
    }

    /// Sets the state at `index` of `section` and returns the previous one.
    #[inline]
    fn set(&mut self, section: usize, index: usize, state: u16) -> u16 {
        let old = std::mem::replace(&mut self.states[index], state);
        let air = BlockState::AIR.0;
        if old == air && state != air {
            self.non_air[section] += 1;
        } else if old != air && state == air {
            self.non_air[section] -= 1;
        }
        old
    }
}

impl ProtoChunk {
    /// Moves the blocks into a dense buffer until [`Self::end_dense`]: block reads and writes
    /// are much cheaper there. [`Self::sections`] must not be read for blocks meanwhile.
    /// `spare` is a reused allocation for the buffer (taken; [`Self::end_dense`] gives it back).
    pub fn begin_dense(&mut self, spare: &mut Vec<u16>) {
        if self.dense.is_some() {
            return;
        }
        let air = BlockState::AIR.0;
        let height = self.sections.len() << 4;
        let mut states = std::mem::take(spare);
        states.clear();
        states.resize(height << 8, air);
        let mut non_air = Vec::with_capacity(self.sections.len());
        let mut unpacked = [0u16; 4096];
        for (index, section) in self.sections.iter().enumerate() {
            let blocks = section.blocks();
            if blocks.single() == Some(air) {
                non_air.push(0);
                continue;
            }
            // Entry `(y << 8) | column` of the section.
            blocks.copy_to(&mut unpacked);
            for (column, states) in states.chunks_exact_mut(height).enumerate() {
                for (y, slot) in states[index << 4..][..16].iter_mut().enumerate() {
                    *slot = unpacked[(y << 8) | column];
                }
            }
            non_air.push(unpacked.iter().filter(|&&s| s != air).count() as u16);
        }
        self.dense = Some(DenseBlocks {
            states,
            non_air,
            height,
        });
    }

    /// Packs the dense buffer of [`Self::begin_dense`] back into the sections and leaves its
    /// allocation in `spare`.
    pub fn end_dense(&mut self, spare: &mut Vec<u16>) {
        let Some(dense) = self.dense.take() else {
            return;
        };
        let mut packed = [0u16; 4096];
        for (index, section) in self.sections.iter_mut().enumerate() {
            let blocks = if dense.non_air[index] == 0 {
                PalettedContainer::new(BlockState::AIR.0)
            } else {
                // Entry `(y << 8) | column` of the section.
                for (column, states) in dense.states.chunks_exact(dense.height).enumerate() {
                    for (y, &state) in states[index << 4..][..16].iter().enumerate() {
                        packed[(y << 8) | column] = state;
                    }
                }
                PalettedContainer::from_values(&packed)
            };
            let biomes = std::mem::replace(section.biomes_mut(), PalettedContainer::new(0));
            *section = Section::from_containers(blocks, biomes);
        }
        *spare = dense.states;
    }

    /// An empty chunk of air with every biome set to `biome`.
    pub fn new(pos: ChunkPos, min_y: i32, height: i32, biome: BiomeId) -> Self {
        assert!(
            min_y % 16 == 0 && height % 16 == 0 && height > 0,
            "unaligned chunk height"
        );
        let air = BlockState::AIR.0;
        Self {
            pos,
            min_y,
            sections: (0..height / 16).map(|_| Section::new(air, biome)).collect(),
            heightmaps: [[min_y; 256]; 6],
            terrain_done: false,
            final_heightmaps_exact: false,
            post_processing: Vec::new(),
            block_entities: FxHashMap::default(),
            block_ticks: ProtoTicks::default(),
            fluid_ticks: ProtoTicks::default(),
            structure_starts: Vec::new(),
            structure_references: Vec::new(),
            entities: Vec::new(),
            dense: None,
        }
    }

    /// A chunk taken over from the chunk runtime between two stages: its sections, with the
    /// world generation heightmaps computed from the blocks; with `terrain_done` (a chunk past
    /// the terrain stage) the final heightmaps are primed too.
    pub fn from_sections(
        pos: ChunkPos,
        min_y: i32,
        sections: Vec<Section>,
        terrain_done: bool,
    ) -> Self {
        assert!(min_y % 16 == 0 && !sections.is_empty(), "unaligned chunk");
        let mut chunk = Self {
            pos,
            min_y,
            sections,
            heightmaps: [[min_y; 256]; 6],
            terrain_done: false,
            final_heightmaps_exact: false,
            post_processing: Vec::new(),
            block_entities: FxHashMap::default(),
            block_ticks: ProtoTicks::default(),
            fluid_ticks: ProtoTicks::default(),
            structure_starts: Vec::new(),
            structure_references: Vec::new(),
            entities: Vec::new(),
            dense: None,
        };
        chunk.prime(&[
            WgHeightmap::WorldSurface.kind(),
            WgHeightmap::OceanFloor.kind(),
        ]);
        if terrain_done {
            chunk.finish_terrain();
        }
        chunk
    }

    /// Takes the sections out to hand them to the chunk runtime; the chunk keeps everything
    /// else (heightmaps, block entities, ticks) and must get sections back with
    /// [`Self::set_sections`] before it is used again.
    pub fn take_sections(&mut self) -> Vec<Section> {
        self.end_dense(&mut Vec::new());
        std::mem::take(&mut self.sections)
    }

    /// Puts the chunk's sections back (see [`Self::take_sections`]); they must hold the blocks
    /// they held when taken, as the heightmaps are kept.
    pub fn set_sections(&mut self, sections: Vec<Section>) {
        self.sections = sections;
    }

    /// The chunk position.
    pub fn pos(&self) -> ChunkPos {
        self.pos
    }

    /// Lowest block Y (`getMinY`).
    pub fn min_y(&self) -> i32 {
        self.min_y
    }

    /// Number of block rows (`getHeight`).
    pub fn height(&self) -> i32 {
        self.sections.len() as i32 * 16
    }

    /// Highest block Y (`getMaxY`).
    pub fn max_y(&self) -> i32 {
        self.min_y + self.height() - 1
    }

    /// `isOutsideBuildHeight(y)`.
    #[inline]
    pub fn is_outside(&self, y: i32) -> bool {
        y.wrapping_sub(self.min_y) as u32 >= (self.sections.len() << 4) as u32
    }

    /// `getSectionIndex(y)`.
    #[inline]
    pub fn section_index(&self, y: i32) -> usize {
        ((y >> 4) - (self.min_y >> 4)) as usize
    }

    /// Sections, bottom first.
    pub fn sections(&self) -> &[Section] {
        debug_assert!(self.dense.is_none(), "sections read in dense mode");
        &self.sections
    }

    /// Mutable section by index.
    pub fn section_mut(&mut self, index: usize) -> &mut Section {
        self.end_dense(&mut Vec::new());
        self.final_heightmaps_exact = false;
        &mut self.sections[index]
    }

    /// `getHighestFilledSectionIndex()`: the highest section with a non-air block, or -1.
    pub fn highest_filled_section_index(&self) -> i32 {
        if let Some(dense) = &self.dense {
            return dense
                .non_air
                .iter()
                .rposition(|&n| n > 0)
                .map_or(-1, |i| i as i32);
        }
        self.sections
            .iter()
            .rposition(|s| !s.summary().is_empty())
            .map_or(-1, |i| i as i32)
    }

    /// `getBlockState(x, y, z)` with world coordinates; air outside the height.
    #[inline]
    pub fn block(&self, x: i32, y: i32, z: i32) -> BlockState {
        if self.is_outside(y) {
            return BlockState::AIR;
        }
        if let Some(dense) = &self.dense {
            return BlockState(dense.states[dense.index(x, (y - self.min_y) as usize, z)]);
        }
        BlockState(self.sections[self.section_index(y)].block(
            (x & 15) as u32,
            (y & 15) as u32,
            (z & 15) as u32,
        ))
    }

    /// Appends the states of the column at world `x`, `z` from `min_y` to `max_y` (inclusive,
    /// bottom up) to `out`; air outside the height.
    pub fn column_states(&self, x: i32, z: i32, min_y: i32, max_y: i32, out: &mut Vec<BlockState>) {
        let Some(dense) = &self.dense else {
            out.extend((min_y..=max_y).map(|y| self.block(x, y, z)));
            return;
        };
        let (bottom, top) = (self.min_y, self.max_y());
        let (from, to) = (min_y.max(bottom), max_y.min(top));
        out.extend((min_y..from.min(max_y + 1)).map(|_| BlockState::AIR));
        if from <= to {
            let start = dense.index(x, (from - bottom) as usize, z);
            let count = (to - from + 1) as usize;
            out.extend(
                dense.states[start..start + count]
                    .iter()
                    .map(|&state| BlockState(state)),
            );
        }
        out.extend((to.max(min_y - 1) + 1..=max_y).map(|_| BlockState::AIR));
    }

    /// Writes a whole column of a chunk that is still empty, while the terrain is built in the
    /// dense buffer: `states` are the blocks at world `x`, `z` from `min_y` up. Sets the two
    /// world generation heightmaps of the column from its highest matching blocks, as the
    /// block-by-block updates of an empty column do. Returns `false` (nothing done) if the
    /// chunk is not in that state or the column does not fit.
    pub(crate) fn write_empty_column(
        &mut self,
        x: i32,
        z: i32,
        min_y: i32,
        states: &[BlockState],
    ) -> bool {
        let Some(dense) = &mut self.dense else {
            return false;
        };
        let column = (((z & 15) as usize) << 4) | (x & 15) as usize;
        let bottom = min_y - self.min_y;
        if self.terrain_done
            || bottom < 0
            || bottom as usize + states.len() > dense.height
            || self.heightmaps[HeightmapKind::WorldSurfaceWg.id() as usize][column] != self.min_y
            || self.heightmaps[HeightmapKind::OceanFloorWg.id() as usize][column] != self.min_y
        {
            return false;
        }
        let air = BlockState::AIR;
        let bottom = bottom as usize;
        let start = dense.index(x, bottom, z);
        let target = &mut dense.states[start..start + states.len()];
        // The column must still be air (then the counts below are right).
        if target
            .iter()
            .fold(false, |any, &state| any | (state != air.0))
        {
            return false;
        }
        // Section by section: copied and counted in one pass.
        let mut row = 0;
        while row < states.len() {
            let section = (bottom + row) >> 4;
            let end = (((section + 1) << 4) - bottom).min(states.len());
            let mut count = 0u16;
            for (slot, state) in target[row..end].iter_mut().zip(&states[row..end]) {
                *slot = state.0;
                count += u16::from(state.0 != air.0);
            }
            dense.non_air[section] += count;
            row = end;
        }
        let (mut surface, mut ocean_floor) = (None, None);
        for (row, &state) in states.iter().enumerate().rev() {
            if state == air {
                continue;
            }
            let y = bottom + row;
            let flags = facts::block_flags(state.0);
            surface = surface.or(HeightmapKind::WorldSurfaceWg.matches(flags).then_some(y));
            ocean_floor = HeightmapKind::OceanFloorWg.matches(flags).then_some(y);
            if ocean_floor.is_some() {
                break;
            }
        }
        for (kind, top) in [
            (HeightmapKind::WorldSurfaceWg, surface),
            (HeightmapKind::OceanFloorWg, ocean_floor),
        ] {
            if let Some(y) = top {
                self.heightmaps[kind.id() as usize][column] = self.min_y + y as i32 + 1;
            }
        }
        true
    }

    /// Replaces `old`, the block at a position inside the height, by `state` if that cannot
    /// change a heightmap or a block count: while the terrain is built in the dense buffer and
    /// both are non-air blocks matching the same world generation heightmaps (the heightmaps
    /// already cover `old`). Returns whether it did; otherwise use [`Self::set_block`].
    #[inline]
    pub(crate) fn replace_solid(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        old: BlockState,
        state: BlockState,
    ) -> bool {
        const HEIGHTMAPS: u8 = facts::AIR | facts::BLOCKS_MOTION | facts::FLUID;
        let (old_flags, flags) = (facts::block_flags(old.0), facts::block_flags(state.0));
        let Some(dense) = &mut self.dense else {
            return false;
        };
        if self.terrain_done
            || old_flags & (facts::AIR | facts::FLUID) != 0
            || (old_flags ^ flags) & HEIGHTMAPS != 0
            || y < self.min_y
            || (y - self.min_y) as usize >= dense.height
        {
            return false;
        }
        let index = dense.index(x, (y - self.min_y) as usize, z);
        match dense.states.get_mut(index) {
            Some(slot) if *slot == old.0 => {
                *slot = state.0;
                true
            }
            _ => false,
        }
    }

    /// [`Self::replace_solid`] for the run of blocks of column `x`, `z` from `from_y` up, whose
    /// current blocks are `olds`: replaces all of them by `state` if it can replace each one,
    /// otherwise none. Returns whether it did.
    pub(crate) fn replace_solid_run(
        &mut self,
        x: i32,
        z: i32,
        from_y: i32,
        olds: &[BlockState],
        state: BlockState,
    ) -> bool {
        const HEIGHTMAPS: u8 = facts::AIR | facts::BLOCKS_MOTION | facts::FLUID;
        let flags = facts::block_flags(state.0);
        let Some(dense) = &mut self.dense else {
            return false;
        };
        if self.terrain_done || from_y < self.min_y {
            return false;
        }
        let row = (from_y - self.min_y) as usize;
        if row + olds.len() > dense.height {
            return false;
        }
        let start = dense.index(x, row, z);
        let slots = &mut dense.states[start..start + olds.len()];
        let mut checked = None;
        for (&slot, old) in slots.iter().zip(olds) {
            if slot != old.0 {
                return false;
            }
            if checked != Some(slot) {
                let old_flags = facts::block_flags(slot);
                if old_flags & (facts::AIR | facts::FLUID) != 0
                    || (old_flags ^ flags) & HEIGHTMAPS != 0
                {
                    return false;
                }
                checked = Some(slot);
            }
        }
        slots.fill(state.0);
        true
    }

    /// `getBlockState(pos)` with Vanilla's `VOID_AIR` outside the build height.
    #[inline]
    pub fn block_or_void(&self, x: i32, y: i32, z: i32) -> BlockState {
        if self.is_outside(y) {
            return void_air();
        }
        self.block(x, y, z)
    }

    /// `ProtoChunk.setBlockState(pos, state)` before `INITIALIZE_LIGHT`: sets the block and
    /// updates the heightmaps of the current status. Returns the old state (`VOID_AIR` outside
    /// the build height).
    #[inline]
    pub fn set_block(&mut self, x: i32, y: i32, z: i32, state: BlockState) -> BlockState {
        if self.is_outside(y) {
            return void_air();
        }
        let index = self.section_index(y);
        let (lx, lz) = ((x & 15) as u32, (z & 15) as u32);
        let old = if let Some(dense) = &mut self.dense {
            if dense.non_air[index] == 0 && state == BlockState::AIR {
                return state;
            }
            let at = dense.index(x, (y - self.min_y) as usize, z);
            BlockState(dense.set(index, at, state.0))
        } else {
            let section = &mut self.sections[index];
            if section.summary().is_empty() && state == BlockState::AIR {
                return state;
            }
            BlockState(section.set_block(lx, (y & 15) as u32, lz, state.0))
        };
        let (lx, lz) = (lx as usize, lz as usize);
        if self.terrain_done {
            for kind in FINAL {
                self.update_kind(kind, lx, y, lz, state);
            }
        } else {
            self.update_kind(HeightmapKind::WorldSurfaceWg, lx, y, lz, state);
            self.update_kind(HeightmapKind::OceanFloorWg, lx, y, lz, state);
        }
        old
    }

    /// Ends the terrain stage: primes the final heightmaps from the blocks
    /// (`Heightmap.primeHeightmaps`); from now on block changes update them instead of the
    /// world generation heightmaps.
    pub fn finish_terrain(&mut self) {
        self.terrain_done = true;
        self.prime(&FINAL);
        self.final_heightmaps_exact = true;
    }

    /// The final heightmap `kind` (first free Y per column `x + z * 16`) when it is known to
    /// equal a fresh prime of the blocks (`Heightmap.primeHeightmaps`).
    pub fn exact_final_heightmap(&self, kind: HeightmapKind) -> Option<&[i32; 256]> {
        (self.terrain_done && self.final_heightmaps_exact && FINAL.contains(&kind))
            .then(|| &self.heightmaps[kind.id() as usize])
    }

    /// Notes a block change that bypassed the heightmaps.
    #[inline]
    fn note_unmapped_change(&mut self, old: u16, new: u16) {
        if self.terrain_done && old != new {
            let (old, new) = (facts::block_flags(old), facts::block_flags(new));
            if FINAL
                .iter()
                .any(|kind| kind.matches(old) != kind.matches(new))
            {
                self.final_heightmaps_exact = false;
            }
        }
    }

    /// `Heightmap.primeHeightmaps` for `kinds`: each column's highest matching block.
    fn prime(&mut self, kinds: &[HeightmapKind]) {
        let top = (self.highest_filled_section_index() + 1) * 16 + self.min_y;
        for column in 0..256 {
            let (x, z) = ((column & 15) as i32, (column >> 4) as i32);
            for kind in kinds {
                self.heightmaps[kind.id() as usize][column] = self.min_y;
            }
            let mut remaining = kinds.len();
            let mut done = [false; 6];
            for y in (self.min_y..top).rev() {
                let state = self.block(x, y, z);
                if state == BlockState::AIR {
                    continue;
                }
                let flags = facts::block_flags(state.0);
                for (i, kind) in kinds.iter().enumerate() {
                    if !done[i] && kind.matches(flags) {
                        done[i] = true;
                        remaining -= 1;
                        self.heightmaps[kind.id() as usize][column] = y + 1;
                    }
                }
                if remaining == 0 {
                    break;
                }
            }
        }
    }

    /// Whether [`Self::finish_terrain`] ran.
    pub fn terrain_done(&self) -> bool {
        self.terrain_done
    }

    /// `LevelChunkSection.setBlockState` without heightmap updates (as `doFill` writes).
    #[inline]
    pub fn set_block_raw(&mut self, x: i32, y: i32, z: i32, state: BlockState) {
        let index = self.section_index(y);
        let old = if let Some(dense) = &mut self.dense {
            let at = dense.index(x, (y - self.min_y) as usize, z);
            dense.set(index, at, state.0)
        } else {
            self.sections[index].set_block(
                (x & 15) as u32,
                (y & 15) as u32,
                (z & 15) as u32,
                state.0,
            )
        };
        self.note_unmapped_change(old, state.0);
    }

    /// `getHeight(type, x, z)` of a world generation heightmap: the highest matching block Y,
    /// or `min_y - 1`. Local x/z.
    #[inline]
    pub fn height_at(&self, map: WgHeightmap, x: usize, z: usize) -> i32 {
        self.heightmap(map.kind(), x, z)
    }

    /// `getHeight(type, x, z)`: the highest matching block Y, or `min_y - 1`. Local x/z. The
    /// final heightmaps are only valid after [`Self::finish_terrain`].
    #[inline]
    pub fn heightmap(&self, kind: HeightmapKind, x: usize, z: usize) -> i32 {
        self.heightmaps[kind.id() as usize][x + z * 16] - 1
    }

    /// `Heightmap.update(x, y, z, state)` of a world generation heightmap with local x/z and
    /// world y.
    pub fn update_heightmap(
        &mut self,
        map: WgHeightmap,
        x: usize,
        y: i32,
        z: usize,
        state: BlockState,
    ) {
        self.update_kind(map.kind(), x, y, z, state);
    }

    #[inline]
    fn update_kind(&mut self, kind: HeightmapKind, x: usize, y: i32, z: usize, state: BlockState) {
        // Below the top block of the column nothing changes: the common case.
        if y <= self.heightmaps[kind.id() as usize][x + z * 16] - 2 {
            return;
        }
        self.update_kind_at_top(kind, x, y, z, state);
    }

    /// [`Self::update_kind`] at or above the column's top block.
    #[inline(never)]
    fn update_kind_at_top(
        &mut self,
        kind: HeightmapKind,
        x: usize,
        y: i32,
        z: usize,
        state: BlockState,
    ) {
        let index = kind.id() as usize;
        let column = x + z * 16;
        let first_available = self.heightmaps[index][column];
        if kind.matches(facts::block_flags(state.0)) {
            if y >= first_available {
                self.heightmaps[index][column] = y + 1;
            }
        } else if first_available - 1 == y {
            let mut found = self.min_y;
            for below in (self.min_y..y).rev() {
                if kind.matches(facts::block_flags(self.block(x as i32, below, z as i32).0)) {
                    found = below + 1;
                    break;
                }
            }
            self.heightmaps[index][column] = found;
        }
    }

    /// Block entity data at a position.
    pub fn block_entity(&self, pos: BlockPos) -> Option<&Compound> {
        self.block_entities.get(&pos)
    }

    /// Mutable block entity data at a position.
    pub fn block_entity_mut(&mut self, pos: BlockPos) -> Option<&mut Compound> {
        self.block_entities.get_mut(&pos)
    }

    /// `setBlockEntityNbt`: replaces the block entity data at a position.
    pub fn set_block_entity(&mut self, pos: BlockPos, data: Compound) {
        self.block_entities.insert(pos, data);
    }

    /// `removeBlockEntity(pos)`.
    pub fn remove_block_entity(&mut self, pos: BlockPos) {
        self.block_entities.remove(&pos);
    }

    /// All block entities.
    pub fn block_entities(&self) -> &FxHashMap<BlockPos, Compound> {
        &self.block_entities
    }

    /// `addEntity(tag)`: stores an entity added during generation.
    pub fn add_entity(&mut self, entity: Compound) {
        self.entities.push(entity);
    }

    /// The entities added during generation, in order.
    pub fn entities(&self) -> &[Compound] {
        &self.entities
    }

    /// Scheduled block ticks.
    pub fn block_ticks(&self) -> &ProtoTicks<BlockId> {
        &self.block_ticks
    }

    /// Scheduled block ticks, mutable.
    pub fn block_ticks_mut(&mut self) -> &mut ProtoTicks<BlockId> {
        &mut self.block_ticks
    }

    /// Scheduled fluid ticks.
    pub fn fluid_ticks(&self) -> &ProtoTicks<FluidTickKind> {
        &self.fluid_ticks
    }

    /// Scheduled fluid ticks, mutable.
    pub fn fluid_ticks_mut(&mut self) -> &mut ProtoTicks<FluidTickKind> {
        &mut self.fluid_ticks
    }

    /// The block at world `x`, `z` and `y` inside section `section` (`y` within the build
    /// height), for bulk writers that look the section up once per row.
    #[inline]
    pub(crate) fn section_block(&self, section: usize, x: i32, y: i32, z: i32) -> BlockState {
        if let Some(dense) = &self.dense {
            return BlockState(dense.states[dense.index(x, (y - self.min_y) as usize, z)]);
        }
        BlockState(self.sections[section].block((x & 15) as u32, (y & 15) as u32, (z & 15) as u32))
    }

    /// [`Self::set_block_in_section`] with the section index of `y` already known.
    #[inline]
    pub(crate) fn set_section_block(
        &mut self,
        section: usize,
        x: i32,
        y: i32,
        z: i32,
        state: BlockState,
    ) {
        let old = if let Some(dense) = &mut self.dense {
            let at = dense.index(x, (y - self.min_y) as usize, z);
            dense.set(section, at, state.0)
        } else {
            self.sections[section].set_block(
                (x & 15) as u32,
                (y & 15) as u32,
                (z & 15) as u32,
                state.0,
            )
        };
        self.note_unmapped_change(old, state.0);
    }

    /// Sets a block in its section only, as `BulkSectionAccess` writes (no heightmap updates).
    #[inline]
    pub fn set_block_in_section(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        state: BlockState,
    ) -> BlockState {
        let index = self.section_index(y);
        let old = if let Some(dense) = &mut self.dense {
            let at = dense.index(x, (y - self.min_y) as usize, z);
            dense.set(index, at, state.0)
        } else {
            self.sections[index].set_block(
                (x & 15) as u32,
                (y & 15) as u32,
                (z & 15) as u32,
                state.0,
            )
        };
        self.note_unmapped_change(old, state.0);
        BlockState(old)
    }

    /// `markPosForPostProcessing(pos)`; positions outside the build height are ignored.
    pub fn mark_post_processing(&mut self, x: i32, y: i32, z: i32) {
        if !self.is_outside(y) {
            self.post_processing.push((x, y, z));
        }
    }

    /// Positions marked for post-processing, in marking order (duplicates possible).
    pub fn post_processing(&self) -> &[(i32, i32, i32)] {
        &self.post_processing
    }

    /// Noise biome at quart coordinates of this chunk (`ChunkAccess.getNoiseBiome`): y is clamped
    /// to the chunk, x and z are taken modulo the chunk.
    pub fn noise_biome(&self, quart_x: i32, quart_y: i32, quart_z: i32) -> BiomeId {
        let min_quart = self.min_y >> 2;
        let max_quart = min_quart + (self.height() >> 2) - 1;
        let qy = quart_y.clamp(min_quart, max_quart);
        let section = self.section_index(qy << 2);
        self.sections[section].biome((quart_x & 3) as u32, (qy & 3) as u32, (quart_z & 3) as u32)
    }

    /// The biome of the eight quarts `quart_x..=quart_x + 1`, `quart_y..=quart_y + 1`,
    /// `quart_z..=quart_z + 1` of this chunk (Y clamped like [`Self::noise_biome`]; X and Z
    /// must not leave the chunk) if they all hold the same one.
    pub fn uniform_noise_biome(&self, quart_x: i32, quart_y: i32, quart_z: i32) -> Option<BiomeId> {
        let min_quart = self.min_y >> 2;
        let max_quart = min_quart + (self.height() >> 2) - 1;
        let (x, z) = ((quart_x & 3) as u32, (quart_z & 3) as u32);
        let mut first = None;
        for qy in [quart_y, quart_y + 1] {
            let qy = qy.clamp(min_quart, max_quart);
            let biomes = self.sections[self.section_index(qy << 2)].biomes();
            let y = (qy & 3) as u32;
            for (dx, dz) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let biome = biomes.get_at(x + dx, y, z + dz);
                if *first.get_or_insert(biome) != biome {
                    return None;
                }
            }
        }
        first
    }

    /// Sets the noise biome at quart coordinates inside the chunk.
    pub fn set_noise_biome(&mut self, quart_x: i32, quart_y: i32, quart_z: i32, biome: BiomeId) {
        let section = self.section_index(quart_y << 2);
        self.sections[section].biomes_mut().set_at(
            (quart_x & 3) as u32,
            (quart_y & 3) as u32,
            (quart_z & 3) as u32,
            biome,
        );
    }

    /// `getStartForStructure(structure)`.
    pub fn start_for_structure(&self, structure: StructureId) -> Option<&Arc<StructureStart>> {
        self.structure_starts
            .iter()
            .find(|(s, _)| *s == structure)
            .map(|(_, start)| start)
    }

    /// `setStartForStructure(structure, start)`: replaces an earlier start of the structure.
    pub fn set_start_for_structure(&mut self, structure: StructureId, start: Arc<StructureStart>) {
        match self
            .structure_starts
            .iter_mut()
            .find(|(s, _)| *s == structure)
        {
            Some(entry) => entry.1 = start,
            None => self.structure_starts.push((structure, start)),
        }
    }

    /// `getAllStarts()`.
    pub fn structure_starts(&self) -> &ChunkStarts {
        &self.structure_starts
    }

    /// `addReferenceForStructure(structure, reference)`: `reference` is the start chunk's
    /// `ChunkPos.pack`.
    pub fn add_reference(&mut self, structure: StructureId, reference: i64) {
        let index = match self
            .structure_references
            .iter()
            .position(|(s, _)| *s == structure)
        {
            Some(index) => index,
            None => {
                self.structure_references
                    .push((structure, LongOpenHashSet::new()));
                self.structure_references.len() - 1
            }
        };
        self.structure_references[index].1.insert(reference);
    }

    /// `setReferencesForStructure(structure, references)` (references read from storage).
    pub fn set_references(&mut self, structure: StructureId, references: LongOpenHashSet) {
        match self
            .structure_references
            .iter_mut()
            .find(|(s, _)| *s == structure)
        {
            Some(entry) => entry.1 = references,
            None => self.structure_references.push((structure, references)),
        }
    }

    /// `getReferencesForStructure(structure)`.
    pub fn references_for_structure(&self, structure: StructureId) -> Option<&LongOpenHashSet> {
        self.structure_references
            .iter()
            .find(|(s, _)| *s == structure)
            .map(|(_, references)| references)
    }

    /// `getAllReferences()`.
    pub fn structure_references(&self) -> &[(StructureId, LongOpenHashSet)] {
        &self.structure_references
    }

    /// The finished chunk; its heightmaps are primed from the blocks.
    pub fn into_chunk(mut self) -> Chunk {
        self.end_dense(&mut Vec::new());
        let count = self.sections.len();
        Chunk::from_sections(
            self.min_y >> 4,
            self.sections.into_iter().map(Arc::new).collect(),
            ChunkLight::new(count),
        )
    }
}

/// `Blocks.VOID_AIR.defaultBlockState()`.
pub fn void_air() -> BlockState {
    use std::sync::OnceLock;
    static VOID_AIR: OnceLock<BlockState> = OnceLock::new();
    *VOID_AIR.get_or_init(|| {
        BlockId::by_name("minecraft:void_air")
            .expect("void_air")
            .default_state()
    })
}
