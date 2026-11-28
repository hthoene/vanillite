//! Built-in light engine: the `light-engine` slot with a breadth-first propagation that
//! reproduces Vanilla's final light levels.
//!
//! Light is the maximum over all sources of the source level minus the opacity along the
//! path, where a step into a block costs `max(1, dampening)` and faces closed by both blocks'
//! occlusion shapes stop it (Vanilla's `LightEngine`). Sky light sources are the cells at and
//! above each column's lowest sky source (`ChunkSkyLightSources`). Because light travels at
//! most 15 blocks, everything a chunk's sources reach lies within its 3 × 3 chunk area, and
//! everything that reaches a chunk comes from sources within its 5 × 5 area:
//!
//! - initial light propagates one chunk's own sources through its 3 × 3 area; results of
//!   different chunks combine by maximum, so neighbouring chunks light in parallel;
//! - after block changes, only the cells a change can reach are reset and propagated again
//!   from the sources among them and the light around them (`update`), with the same result as
//!   recomputing the 3 × 3 area around the changed chunk from all sources of its 5 × 5 area
//!   ([`BfsLightEngine::relight_area`], the reference).
//!
//! Levels are processed from 15 down in buckets, so every cell is finalised once.
//!
//! Like Vanilla, light also lives in the light sections below and above the world (layers 0 and
//! `sections + 1`), where a chunk keeps them: when a section next to them, also in a
//! neighbouring chunk, holds blocks (`LayerLightSectionStorage`). Their cells are air; a column
//! open down to the bottom of the world has sky light sources below it as well
//! (`ChunkSkyLightSources`: lowest source at negative infinity).
//! Not yet ported: Vanilla's exceptions for sections it keeps no light data for (huge empty
//! areas far from any block).

mod facts;
mod update;

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use vanillite_base::scratch::ScratchPool;
use vanillite_base::{BlockPos, ChunkPos, Direction};
use vanillite_chunk::light::{ChunkLight, LightNibbles, NIBBLE_BYTES};
use vanillite_chunk::slots::{LightEngine, LightEngineProvider, Neighbours, WorldInfo};
use vanillite_chunk::{Chunk, ProtoChunk};

use crate::facts::LightFacts;

/// The built-in light engine of one world.
#[derive(Debug)]
pub struct BfsLightEngine {
    has_sky: bool,
    /// The lowest section (from the bottom of the world, -1 below it) the world has held light
    /// data for since it started (`SkyLightSectionStorage.currentLowestY`, per world): sky
    /// light fills empty sections only down to it.
    lowest_section: AtomicI32,
    /// The light buffers of the jobs, reused from job to job.
    buffers: ScratchPool<GridBuffers>,
}

impl BfsLightEngine {
    /// The engine of a world.
    pub fn new(world: &WorldInfo) -> Self {
        BfsLightEngine {
            has_sky: world.has_sky_light,
            lowest_section: AtomicI32::new(i32::MAX),
            buffers: ScratchPool::default(),
        }
    }

    /// Lowers [`Self::lowest_section`] to the light data the chunks keep: a section with
    /// blocks keeps light in the section below it; with `lit`, light kept below the world
    /// counts as well. Returns the new value.
    fn note_lowest<'c>(&self, chunks: impl Iterator<Item = &'c Chunk>, lit: bool) -> i32 {
        let lowest = chunks
            .filter_map(|chunk| {
                if lit && chunk.light().block().first() != Some(&LightNibbles::Absent) {
                    return Some(-1);
                }
                let filled = chunk
                    .sections()
                    .iter()
                    .position(|section| !section.summary().is_empty())?;
                Some(filled as i32 - 1)
            })
            .min()
            .unwrap_or(i32::MAX);
        let before = self.lowest_section.fetch_min(lowest, Ordering::Relaxed);
        before.min(lowest)
    }
}

/// Provider of [`BfsLightEngine`].
#[derive(Debug, Default)]
pub struct BfsLightEngineProvider;

impl LightEngineProvider for BfsLightEngineProvider {
    fn create(&self, world: &WorldInfo) -> Arc<dyn LightEngine> {
        Arc::new(BfsLightEngine::new(world))
    }
}

impl LightEngine for BfsLightEngine {
    fn initialize_light(&self, targets: &mut [ProtoChunk]) {
        for target in targets {
            let chunk = &mut target.chunk;
            let highest = highest_filled_section(chunk);
            let light = chunk.light_mut();
            // The layers below and above the world start empty and take light from the
            // neighbours; the chunk's own `light_chunks` drops them where it keeps no light.
            for layer in light.block_mut().iter_mut() {
                *layer = LightNibbles::Uniform(0);
            }
            let has_sky = self.has_sky;
            for (index, layer) in light.sky_mut().iter_mut().enumerate() {
                // Light layer `index` is section `index - 1`.
                let below_top = highest.is_some_and(|h| index >= 1 && index - 1 <= h);
                *layer = if has_sky && (below_top || index == 0) {
                    LightNibbles::Uniform(0)
                } else {
                    LightNibbles::Absent
                };
            }
        }
    }

    fn light_chunks(&self, targets: &mut [ProtoChunk], neighbours: &mut Neighbours<'_>) {
        for target in targets {
            let center = target.pos;
            let origin = ChunkPos::new(center.x - 1, center.z - 1);
            let result = {
                let mut chunks = [None; 9];
                for (i, slot) in chunks.iter_mut().enumerate() {
                    let pos = grid_pos(origin, 3, i);
                    *slot = if pos == center {
                        Some(&target.chunk)
                    } else {
                        neighbours.get(pos).map(|c| &c.chunk)
                    };
                }
                self.note_lowest(chunks.iter().flatten().copied(), false);
                let buffers = self.buffers.take_or(GridBuffers::default);
                let mut grid = Grid::new(origin, 3, &chunks, buffers);
                grid.fill_sky_sources(self.has_sky, |i| i == 4);
                grid.seed_block_sources(|i| i == 4);
                grid.run();
                let store = grid.store[4];
                let light = grid.light();
                self.buffers.give(grid.buffers());
                (light, store)
            };
            let (result, store) = result;
            for (i, light) in result.into_iter().enumerate() {
                let Some(light) = light else { continue };
                let pos = grid_pos(origin, 3, i);
                if pos == center {
                    target.chunk.light_mut().merge_max(&light);
                } else if let Some(chunk) = neighbours.get_mut(pos) {
                    chunk.chunk.light_mut().merge_max(&light);
                }
            }
            // The whole 3 × 3 area is known here: the layers below and above the world the
            // chunk keeps no light in go (later merges keep them absent).
            let light = target.chunk.light_mut();
            let top = light.block().len() - 1;
            if !store[0] {
                light.block_mut()[0] = LightNibbles::Absent;
                light.sky_mut()[0] = LightNibbles::Absent;
            }
            if !store[1] {
                light.block_mut()[top] = LightNibbles::Absent;
            }
            target.light_on = true;
        }
    }

    /// Light is a function of each state's emission, dampening and occlusion shape and of
    /// which sections are empty, so a change that keeps all of them keeps every light level.
    fn changes_light(&self, old: u16, new: u16) -> bool {
        let air =
            |state| vanillite_chunk::facts::block_flags(state) & vanillite_chunk::facts::AIR != 0;
        let facts = LightFacts::get();
        LightFacts::light(old) != LightFacts::light(new)
            || air(old) != air(new)
            || facts.shape_of(old) != facts.shape_of(new)
    }

    fn release_memory(&self) {
        self.buffers.release();
    }

    fn relight(&self, changes: &[BlockPos], area: &Neighbours<'_>) -> Vec<(ChunkPos, ChunkLight)> {
        let lowest = if self.has_sky {
            let lit = area
                .iter()
                .filter(|chunk| chunk.status >= vanillite_chunk::ChunkStatus::InitializeLight);
            self.note_lowest(lit.map(|chunk| &chunk.chunk), true)
        } else {
            i32::MAX
        };
        update::update(self.has_sky, lowest <= -1, changes, area)
    }
}

impl BfsLightEngine {
    /// Recomputes the light of the chunks within 1 of `center` from all light sources of the
    /// chunks within 2 (`area`), whatever light they hold. The reference [`LightEngine::relight`]
    /// is checked against, and the way to light chunks whose stored light is not trusted.
    pub fn relight_area(&self, center: ChunkPos, area: &mut Neighbours<'_>) {
        let origin = ChunkPos::new(center.x - 2, center.z - 2);
        let result = {
            let mut chunks = [None; 25];
            for (i, slot) in chunks.iter_mut().enumerate() {
                *slot = area.get(grid_pos(origin, 5, i)).map(|c| &c.chunk);
            }
            let buffers = self.buffers.take_or(GridBuffers::default);
            let mut grid = Grid::new(origin, 5, &chunks, buffers);
            grid.fill_sky_sources(self.has_sky, |_| true);
            grid.seed_block_sources(|_| true);
            grid.run();
            let store = grid.store.clone();
            let light = grid.light();
            self.buffers.give(grid.buffers());
            (light, store)
        };
        let (result, store) = result;
        for (i, light) in result.into_iter().enumerate() {
            let pos = grid_pos(origin, 5, i);
            if pos.chessboard_distance(center) > 1 {
                continue;
            }
            let (Some(light), Some(chunk)) = (light, area.get_mut(pos)) else {
                continue;
            };
            let highest = highest_filled_section(&chunk.chunk);
            let target = chunk.chunk.light_mut();
            let top = target.block().len() - 1;
            // The layers below and above the world: as computed where the chunk keeps light
            // there (empty if no light got there), else absent.
            let border = |index: usize, layer: &LightNibbles| {
                let kept = store[i][usize::from(index != 0)];
                match layer {
                    _ if !kept => LightNibbles::Absent,
                    LightNibbles::Absent => LightNibbles::Uniform(0),
                    layer => layer.clone(),
                }
            };
            for (index, layer) in light.block().iter().enumerate() {
                target.block_mut()[index] = if index == 0 || index == top {
                    border(index, layer)
                } else {
                    layer.clone()
                };
            }
            if self.has_sky {
                for (index, layer) in light.sky().iter().enumerate() {
                    let above_top = index > 0 && highest.is_none_or(|h| index - 1 > h);
                    target.sky_mut()[index] = if index == 0 {
                        border(index, layer)
                    } else if above_top {
                        LightNibbles::Absent
                    } else {
                        layer.clone()
                    };
                }
            }
        }
    }
}

/// Per chunk of a square of `size` chunks: whether it keeps light in its layers below and above
/// the world, a bottom (top) section with blocks in its 3 × 3 chunk area (Vanilla keeps light
/// data for sections within one section of a non-empty one). Missing chunks count as empty;
/// chunks outside the square as filled, so light can pass the border layers of the square's
/// outer chunks (each chunk drops the layers it does not keep when it is lit itself).
fn border_storage(chunks: &[Option<&Chunk>], size: usize) -> Vec<[bool; 2]> {
    let filled: Vec<[bool; 2]> = chunks
        .iter()
        .map(|chunk| {
            chunk.map_or([false; 2], |c| {
                let sections = c.sections();
                [
                    sections.first().is_some_and(|s| !s.summary().is_empty()),
                    sections.last().is_some_and(|s| !s.summary().is_empty()),
                ]
            })
        })
        .collect();
    (0..chunks.len())
        .map(|i| {
            let (x, z) = ((i % size) as isize, (i / size) as isize);
            let mut store = [false; 2];
            for dz in -1..=1 {
                for dx in -1..=1 {
                    let (nx, nz) = (x + dx, z + dz);
                    if (0..size as isize).contains(&nx) && (0..size as isize).contains(&nz) {
                        let f = filled[nz as usize * size + nx as usize];
                        store[0] |= f[0];
                        store[1] |= f[1];
                    } else {
                        store = [true; 2];
                    }
                }
            }
            store
        })
        .collect()
}

/// Whether light can get from the bottom row of the world into the layer below it: a cell
/// there that lets light through or emits it.
fn bottom_open(chunk: &Chunk) -> bool {
    let Some(section) = chunk.sections().first() else {
        return false;
    };
    let passes = |state: u16| LightFacts::opacity(state) < 15 || LightFacts::light(state).0 > 0;
    if let Some(state) = section.blocks().single() {
        return passes(state);
    }
    (0..16).any(|z| (0..16).any(|x| passes(section.block(x, 0, z))))
}

/// Position of grid cell `i` (row-major, z then x).
#[inline]
fn grid_pos(origin: ChunkPos, size: i32, i: usize) -> ChunkPos {
    let i = i as i32;
    ChunkPos::new(origin.x + i % size, origin.z + i / size)
}

/// Index of the highest section with a non-air block.
pub(crate) fn highest_filled_section(chunk: &Chunk) -> Option<usize> {
    chunk
        .sections()
        .iter()
        .rposition(|section| !section.summary().is_empty())
}

/// `ChunkSkyLightSources.findLowestSourceY` of column x/z of a chunk, in cells above the bottom
/// of the world: cells at and above it are sky sources. 0 means the whole column.
pub(crate) fn lowest_source(facts: &LightFacts, chunk: &Chunk, lx: u32, lz: u32) -> usize {
    let Some(top) = highest_filled_section(chunk) else {
        return 0;
    };
    let air = 0u16;
    let mut top_state = air;
    let mut top_y = (top + 1) * 16;
    for index in (0..=top).rev() {
        let section = &chunk.sections()[index];
        if section.summary().is_empty() {
            top_state = air;
            top_y = index * 16;
            continue;
        }
        for local in (0..16u32).rev() {
            let bottom = section.block(lx, local, lz);
            if facts.edge_occluded(top_state, bottom) {
                return top_y;
            }
            top_state = bottom;
            top_y = index * 16 + local as usize;
        }
    }
    0
}

/// The allocations of a [`Grid`], reused between grids.
#[derive(Debug, Default)]
struct GridBuffers {
    sky: Vec<u8>,
    block: Vec<u8>,
    sky_queue: [Vec<u32>; 16],
    block_queue: [Vec<u32>; 16],
}

/// Light buffers of a square of chunk columns; one byte per cell.
struct Grid<'a> {
    origin: ChunkPos,
    size: usize,
    width: usize,
    /// Cells of the grid in y: from the bottom of the world (or of the layer below it,
    /// [`Self::bottom`]) to the section above the highest non-air section of the area (at most
    /// the layer above the world).
    height: usize,
    /// Cells of the layer below the world in the grid: 16 when light can get there, else 0.
    bottom: usize,
    /// Per chunk: whether it keeps light below and above the world ([`border_storage`]).
    store: Vec<[bool; 2]>,
    /// Sections of the world.
    sections: usize,
    min_y: i32,
    chunks: &'a [Option<&'a Chunk>],
    facts: &'static LightFacts,
    sky: Vec<u8>,
    block: Vec<u8>,
    sky_queue: [Vec<u32>; 16],
    block_queue: [Vec<u32>; 16],
    /// The box the buffers came in, given back by [`Self::buffers`].
    spare: Box<GridBuffers>,
}

impl<'a> Grid<'a> {
    /// The grid's allocations, for the next grid.
    fn buffers(self) -> Box<GridBuffers> {
        let mut spare = self.spare;
        spare.sky = self.sky;
        spare.block = self.block;
        spare.sky_queue = self.sky_queue;
        spare.block_queue = self.block_queue;
        spare
    }

    fn new(
        origin: ChunkPos,
        size: usize,
        chunks: &'a [Option<&'a Chunk>],
        mut buffers: Box<GridBuffers>,
    ) -> Self {
        let any = chunks.iter().flatten().next();
        let (min_y, sections) = any.map_or((0, 0), |c| (c.min_y(), c.sections().len()));
        // Above the highest non-air section of the area every cell is a sky source and no block
        // light reaches past the section above it: the grid ends there.
        let top = chunks
            .iter()
            .flatten()
            .filter_map(|c| highest_filled_section(c))
            .max();
        let store = border_storage(chunks, size);
        let bottom = if chunks
            .iter()
            .zip(&store)
            .any(|(chunk, store)| store[0] && chunk.is_some_and(bottom_open))
        {
            16
        } else {
            0
        };
        let height = bottom + top.map_or(0, |t| (t + 2).min(sections + 1)) * 16;
        let width = size * 16;
        let cells = width * width * height;
        // Every cell starts dark; the queues are empty after every run.
        for light in [&mut buffers.sky, &mut buffers.block] {
            light.clear();
            light.resize(cells, 0);
        }
        Grid {
            origin,
            size,
            width,
            height,
            bottom,
            store,
            sections,
            min_y,
            chunks,
            facts: LightFacts::get(),
            sky: std::mem::take(&mut buffers.sky),
            block: std::mem::take(&mut buffers.block),
            sky_queue: std::mem::take(&mut buffers.sky_queue),
            block_queue: std::mem::take(&mut buffers.block_queue),
            spare: buffers,
        }
    }

    #[inline]
    fn index(&self, x: usize, y: usize, z: usize) -> usize {
        (y * self.width + z) * self.width + x
    }

    #[inline]
    fn coords(&self, index: usize) -> (usize, usize, usize) {
        let x = index % self.width;
        let z = (index / self.width) % self.width;
        (x, index / (self.width * self.width), z)
    }

    /// The chunk containing grid column x/z.
    #[inline]
    fn chunk(&self, x: usize, z: usize) -> Option<&'a Chunk> {
        self.chunks[(z >> 4) * self.size + (x >> 4)]
    }

    /// Block state at grid coordinates; `None` in missing chunks (they block light like
    /// Vanilla's bedrock stand-in) and in border layers a chunk keeps no light in. Border layers
    /// are air.
    #[inline]
    fn state(&self, x: usize, y: usize, z: usize) -> Option<u16> {
        let chunk = self.chunk(x, z)?;
        let Some(y) = y.checked_sub(self.bottom) else {
            return self.store[(z >> 4) * self.size + (x >> 4)][0].then_some(0);
        };
        let Some(section) = chunk.sections().get(y >> 4) else {
            return self.store[(z >> 4) * self.size + (x >> 4)][1].then_some(0);
        };
        Some(section.block((x & 15) as u32, (y & 15) as u32, (z & 15) as u32))
    }

    /// `ChunkSkyLightSources.findLowestSourceY` of every column of `chunk` (`x + z * 16`),
    /// relative to the grid's bottom: cells at and above it are sky sources. 0 means the whole
    /// column, `height` none.
    ///
    /// Every column is walked down from the highest non-air section until sky light cannot
    /// pass from one block into the next; the walk goes section by section over all columns
    /// at once, so each section's blocks are unpacked once.
    fn lowest_sources(&self, chunk: &Chunk) -> [usize; 256] {
        let mut lowest = [0; 256];
        let Some(top) = highest_filled_section(chunk) else {
            return lowest;
        };
        let air = 0u16;
        // Per column still open: the block above the next one and the Y it would return.
        let mut top_state = [air; 256];
        let mut top_y = [(top + 1) * 16; 256];
        let mut open: Vec<u8> = (0..=255).collect();
        // The last pair of states tested and its result: columns are long runs of one state.
        let mut tested = (air, air, self.facts.edge_occluded(air, air));
        let mut occluded = |above: u16, below: u16| {
            if (above, below) != (tested.0, tested.1) {
                tested = (above, below, self.facts.edge_occluded(above, below));
            }
            tested.2
        };
        let mut blocks = [0u16; 4096];
        for index in (0..=top).rev() {
            if open.is_empty() {
                break;
            }
            let section = &chunk.sections()[index];
            if section.summary().is_empty() {
                for &column in &open {
                    top_state[usize::from(column)] = air;
                    top_y[usize::from(column)] = index * 16;
                }
                continue;
            }
            if let Some(state) = section.blocks().single() {
                // A section of one state: its top block below the column's last block, then
                // 15 times the state below itself.
                let within = occluded(state, state);
                open.retain(|&column| {
                    let column = usize::from(column);
                    if occluded(top_state[column], state) {
                        lowest[column] = top_y[column];
                        false
                    } else if within {
                        lowest[column] = index * 16 + 15;
                        false
                    } else {
                        top_state[column] = state;
                        top_y[column] = index * 16;
                        true
                    }
                });
                continue;
            }
            section.blocks().copy_to(&mut blocks);
            open.retain(|&column| {
                let column = usize::from(column);
                for local in (0..16).rev() {
                    // Entry `(y << 8) | (z << 4) | x`.
                    let below = blocks[(local << 8) | column];
                    if occluded(top_state[column], below) {
                        lowest[column] = top_y[column];
                        return false;
                    }
                    top_state[column] = below;
                    top_y[column] = index * 16 + local;
                }
                true
            });
        }
        // Columns open down to the bottom are sources all the way: 0.
        lowest
    }

    /// Sets the sky sources of all present chunks to 15 and queues those of the chunks
    /// `seed(i)` selects, where light can leave them (Vanilla's `propagateLightSources`).
    fn fill_sky_sources(&mut self, has_sky: bool, seed: impl Fn(usize) -> bool) {
        if !has_sky || self.height == 0 {
            return;
        }
        let width = self.width;
        let mut lowest = vec![self.height; width * width];
        for (i, chunk) in self.chunks.iter().enumerate() {
            let Some(chunk) = chunk else { continue };
            let (base_x, base_z) = ((i % self.size) * 16, (i / self.size) * 16);
            let mut sources = self.lowest_sources(chunk);
            // In grid cells; a column open to the bottom also has sources below the world where
            // the chunk keeps light there.
            let below = if self.store[i][0] { 0 } else { self.bottom };
            for low in &mut sources {
                *low = if *low == 0 { below } else { *low + self.bottom };
            }
            for z in 0..16 {
                let row = (base_z + z) * width + base_x;
                lowest[row..row + 16].copy_from_slice(&sources[z * 16..z * 16 + 16]);
            }
        }
        for z in 0..width {
            for x in 0..width {
                let low = lowest[z * width + x];
                let seeded = seed((z >> 4) * self.size + (x >> 4));
                // A missing neighbour column counts as having sources everywhere
                // below the world (no sideways propagation into it).
                let side = |dx: isize, dz: isize| {
                    let nx = x.checked_add_signed(dx).filter(|&v| v < width);
                    let nz = z.checked_add_signed(dz).filter(|&v| v < width);
                    match (nx, nz) {
                        (Some(nx), Some(nz)) if self.chunk(nx, nz).is_some() => {
                            lowest[nz * width + nx]
                        }
                        _ => 0,
                    }
                };
                if !seeded {
                    continue;
                }
                let neighbour_low = side(-1, 0).max(side(1, 0)).max(side(0, -1)).max(side(0, 1));
                // Queued: the lowest source and those a neighbour column has none beside, bottom
                // up (the order the levels are set in below does not matter).
                for y in low..self.height.min(neighbour_low.max(low + 1)) {
                    let index = self.index(x, y, z);
                    self.sky_queue[15].push(index as u32);
                }
            }
        }
        // Every cell at or above its column's lowest source is 15, layer by layer.
        let lowest: Vec<u32> = lowest.iter().map(|&low| low as u32).collect();
        for (y, layer) in self.sky.chunks_exact_mut(width * width).enumerate() {
            let y = y as u32;
            for (cell, &low) in layer.iter_mut().zip(&lowest) {
                *cell = if y >= low { 15 } else { *cell };
            }
        }
    }

    /// Sets and queues the block light sources of the chunks `seed(i)` selects.
    fn seed_block_sources(&mut self, seed: impl Fn(usize) -> bool) {
        for i in 0..self.chunks.len() {
            let Some(chunk) = self.chunks[i] else {
                continue;
            };
            if !seed(i) {
                continue;
            }
            let base_x = (i % self.size) * 16;
            let base_z = (i / self.size) * 16;
            for (index, section) in chunk.sections().iter().enumerate() {
                if section.summary().is_empty() {
                    continue;
                }
                if !section
                    .blocks()
                    .may_contain(|state| LightFacts::light(state).0 > 0)
                {
                    continue;
                }
                for y in 0..16u32 {
                    for z in 0..16u32 {
                        for x in 0..16u32 {
                            let emission = LightFacts::light(section.block(x, y, z)).0;
                            if emission == 0 {
                                continue;
                            }
                            let cell = self.index(
                                base_x + x as usize,
                                self.bottom + index * 16 + y as usize,
                                base_z + z as usize,
                            );
                            if self.block[cell] < emission {
                                self.block[cell] = emission;
                                self.block_queue[usize::from(emission)].push(cell as u32);
                            }
                        }
                    }
                }
            }
        }
    }

    fn run(&mut self) {
        let mut sky = std::mem::take(&mut self.sky);
        let mut queue = std::mem::take(&mut self.sky_queue);
        self.propagate(&mut sky, &mut queue, true);
        self.sky = sky;
        let mut block = std::mem::take(&mut self.block);
        let mut queue = std::mem::take(&mut self.block_queue);
        self.propagate(&mut block, &mut queue, false);
        self.block = block;
    }

    /// Propagates queued levels, highest first (`propagateIncrease`).
    ///
    /// Sky light that crosses into another chunk from the bottom row of the world, where the
    /// chunk it leaves keeps no light below the world, also fills the column below the world of
    /// the chunk it enters if that one keeps light there (`SkyLightEngine
    /// .propagateFromEmptySections`: the missing layer reads as the bottom row above it).
    fn propagate(&self, light: &mut [u8], queue: &mut [Vec<u32>; 16], sky: bool) {
        let (width, height) = (self.width, self.height);
        let fill_below = sky && self.bottom > 0;
        for level in (2..=15u8).rev() {
            while let Some(cell) = queue[usize::from(level)].pop() {
                let cell = cell as usize;
                if light[cell] != level {
                    continue;
                }
                let (x, y, z) = self.coords(cell);
                let Some(from) = self.state(x, y, z) else {
                    continue;
                };
                for dir in Direction::ALL {
                    let [dx, dy, dz] = dir.normal();
                    let (Some(tx), Some(ty), Some(tz)) = (
                        x.checked_add_signed(dx as isize).filter(|&v| v < width),
                        y.checked_add_signed(dy as isize).filter(|&v| v < height),
                        z.checked_add_signed(dz as isize).filter(|&v| v < width),
                    ) else {
                        continue;
                    };
                    let to_cell = self.index(tx, ty, tz);
                    let current = light[to_cell];
                    if level - 1 <= current {
                        continue;
                    }
                    let Some(to) = self.state(tx, ty, tz) else {
                        continue;
                    };
                    let new = level.saturating_sub(LightFacts::opacity(to));
                    if new <= current || self.facts.occludes(from, to, dir) {
                        continue;
                    }
                    light[to_cell] = new;
                    if new > 1 {
                        queue[usize::from(new)].push(to_cell as u32);
                    }
                    if fill_below && y == self.bottom && dy == 0 {
                        let (from_chunk, to_chunk) = (
                            (z >> 4) * self.size + (x >> 4),
                            (tz >> 4) * self.size + (tx >> 4),
                        );
                        if from_chunk != to_chunk
                            && !self.store[from_chunk][0]
                            && self.store[to_chunk][0]
                        {
                            for by in 0..self.bottom {
                                let below = self.index(tx, by, tz);
                                if light[below] < new {
                                    light[below] = new;
                                    if new > 1 {
                                        queue[usize::from(new)].push(below as u32);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The computed light per present chunk, in the chunk's layout (layer `i + 1` is section
    /// `i`). Sections without light are `Uniform(0)`; above the grid sky light is absent (full)
    /// and block light 0. The border layers are absent unless the chunk keeps light there and
    /// the grid holds them; the sky layer above the world stays absent.
    fn light(&self) -> Vec<Option<ChunkLight>> {
        let top = self.bottom + self.sections * 16;
        (0..self.chunks.len())
            .map(|i| {
                self.chunks[i]?;
                let base_x = (i % self.size) * 16;
                let base_z = (i / self.size) * 16;
                let mut light = ChunkLight::new(self.sections);
                for section in 0..self.sections {
                    let y = self.bottom + section * 16;
                    if y < self.height {
                        light.sky_mut()[section + 1] = self.layer(&self.sky, base_x, base_z, y);
                        light.block_mut()[section + 1] = self.layer(&self.block, base_x, base_z, y);
                    } else {
                        light.block_mut()[section + 1] = LightNibbles::Uniform(0);
                    }
                }
                if self.bottom > 0 && self.store[i][0] {
                    light.sky_mut()[0] = self.layer(&self.sky, base_x, base_z, 0);
                    light.block_mut()[0] = self.layer(&self.block, base_x, base_z, 0);
                }
                if top < self.height && self.store[i][1] {
                    light.block_mut()[self.sections + 1] =
                        self.layer(&self.block, base_x, base_z, top);
                }
                Some(light)
            })
            .collect()
    }

    /// The layer of a chunk's column whose bottom cell is at grid `y`.
    fn layer(&self, buffer: &[u8], base_x: usize, base_z: usize, y0: usize) -> LightNibbles {
        // The 16 levels of a row as two words.
        let row = |y: usize, z: usize| -> [u64; 2] {
            let start = self.index(base_x, y0 + y, base_z + z);
            let (halves, _) = buffer[start..start + 16].as_chunks::<8>();
            [u64::from_le_bytes(halves[0]), u64::from_le_bytes(halves[1])]
        };
        // Most layers hold one level (full sky light above the terrain, none inside it).
        let first = buffer[self.index(base_x, y0, base_z)];
        let uniform = u64::from_le_bytes([first; 8]);
        if (0..16).all(|y| (0..16).all(|z| row(y, z) == [uniform; 2])) {
            return LightNibbles::Uniform(first);
        }
        let mut bytes = [0u8; NIBBLE_BYTES];
        for y in 0..16 {
            for z in 0..16 {
                // Nibble `(y << 8) | (z << 4) | x`: even x in the low half of a byte.
                let (out, _) = bytes[((y << 8) | (z << 4)) >> 1..][..8].as_chunks_mut::<4>();
                for (out, levels) in out.iter_mut().zip(row(y, z)) {
                    // Each pair of levels (at most 15 each) into one byte, then the four
                    // bytes of the pairs together.
                    let pairs = (levels | (levels >> 4)) & 0x00FF_00FF_00FF_00FF;
                    let pairs = (pairs | (pairs >> 8)) & 0x0000_FFFF_0000_FFFF;
                    *out = ((pairs | (pairs >> 16)) as u32).to_le_bytes();
                }
            }
        }
        LightNibbles::Data(Arc::new(bytes))
    }
}

impl std::fmt::Debug for Grid<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grid")
            .field("origin", &self.origin)
            .field("size", &self.size)
            .field("min_y", &self.min_y)
            .finish_non_exhaustive()
    }
}
