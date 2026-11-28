//! Tile region mode (rule `performance.region-threading`): one world on several cores.
//!
//! The world is cut into fixed square tiles of `size × size` chunks. A tiled phase runs in two
//! steps:
//!
//! 1. **Propose**, in parallel: every busy tile gets a worker that only reads the level (the
//!    level is shared as `&Level` and nothing writes meanwhile) and its own random stream,
//!    seeded from the world seed, the game time and the tile. It returns what it wants to
//!    change.
//! 2. **Commit**, on the world's owner: the proposals are applied tile by tile in tile order
//!    with the whole world writable, like the serial phase applies them.
//!
//! No worker ever writes shared state, so there are no locks and no data races, and the result
//! depends only on the world state: not on the number of threads or on timing. What differs
//! from Vanilla is listed in `docs/architecture/region-threading.md`.
//!
//! **Owners.** A tile is worked out by its owner ([`OwnerId`]), never by "a thread": today the
//! owner is always this process and the job runs on the compute pool. What goes to the owner
//! (the tile, its chunks, the phase's inputs) and what comes back ([`TileReply`]: plain data
//! such as positions and new mobs, no references into the level) is all that crosses a tile's
//! border, and tiles share nothing with each other. An owner on another server would get the
//! same request and send the same reply; what it lacks today is its own copy of the chunks
//! and entities it reads (the jobs read the shared `&Level`).
//!
//! **Scheduling.** Which tiles share a job and in which order the jobs start is decided per
//! phase from the tiles' measured cost ([`TileCosts`], a moving average of their job times):
//! hot tiles run alone and start first, adjacent cold tiles of the same colour (tile x and z
//! parity) are batched into one job. Planning never changes what a tile computes or the order
//! its reply is applied in, so the result stays independent of timing; no data moves between
//! tiles.
//!
//! A phase falls back to the serial Vanilla order for a tick when the work lies in one tile,
//! when it is too small to be worth the fork, or when a synchronous chunk load's `unknown`
//! ticket exists (reads would touch the chunk cache, see [`Chunks::reads_are_pure`]).
//!
//! [`Chunks::reads_are_pure`]: vanillite_world::chunks::Chunks::reads_are_pure

use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use vanillite_base::hash::FxHashMap;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, ChunkPos};
use vanillite_data::block::BlockState;

use super::Level;
use crate::block;

/// Runs `job(0)`, …, `job(n - 1)`, on several threads if it likes, and returns when all are
/// done. The server passes its compute pool; tests run inline.
pub type Fork<'a> = dyn Fn(usize, &(dyn Fn(usize) + Sync)) + Sync + 'a;

/// Chunks of one player's simulation distance 10 are 441: below this, a phase is not worth
/// the fork and stays in Vanilla's exact serial order.
pub const MIN_CHUNKS: usize = 512;

/// The tile region mode of one tick.
#[derive(Clone, Copy)]
pub struct TileRun<'a> {
    /// Tile edge in chunks.
    pub size: i32,
    /// Chunks a phase needs to run tiled.
    pub min_chunks: usize,
    /// Where the tiles' proposals run.
    pub fork: &'a Fork<'a>,
    /// Threads [`TileRun::fork`] runs jobs on (the calling one included).
    pub threads: usize,
    /// The tiles' measured costs; without them every busy tile costs the same.
    pub costs: Option<&'a TileCosts>,
}

impl std::fmt::Debug for TileRun<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileRun")
            .field("size", &self.size)
            .field("min_chunks", &self.min_chunks)
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}

/// Jobs per thread the planner aims for: enough that uneven jobs balance out, few enough that
/// cold tiles share a job.
const JOBS_PER_THREAD: u64 = 4;

/// The measured cost of every busy tile per phase: an exponentially weighted moving average
/// (weight 1/4) of how long its job took, in nanoseconds. Kept by the world across ticks; a
/// tile that is not busy in a round of its phase is dropped, so it holds only busy tiles.
#[derive(Debug, Default)]
pub struct TileCosts(Mutex<FxHashMap<(Phase, TilePos), u64>>);

/// Who works a tile out. Every tile belongs to this process today ([`OwnerId::LOCAL`]);
/// later ids name other servers that own tiles of the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OwnerId(pub u32);

impl OwnerId {
    /// This process.
    pub const LOCAL: OwnerId = OwnerId(0);
}

/// What a tile's owner returns for a phase: its changes as plain data. The world's owner
/// applies the replies in tile order, wherever they were worked out.
#[derive(Clone, Debug)]
pub struct TileReply<T> {
    /// The tile.
    pub tile: TilePos,
    /// Who worked it out.
    pub owner: OwnerId,
    /// What the tile wants to change.
    pub changes: T,
}

/// A [`Fork`] that runs the jobs one after another on the calling thread.
pub fn inline_fork(n: usize, job: &(dyn Fn(usize) + Sync)) {
    for i in 0..n {
        job(i);
    }
}

/// Items per part of a parallel phase over chunks: below this, a part is not worth a thread.
pub(crate) const PHASE_PART: usize = 1024;

/// The parallel phases of Vanilla's serial order (`performance.threads.tick-phases`): work
/// that only reads the level is split into contiguous parts that run on up to `threads`
/// threads, and the results come back in the order of the input, so they do not depend on
/// the number of threads.
#[derive(Clone, Copy)]
pub struct Phases<'a> {
    /// Threads of a phase, the calling one included (1 = inline).
    pub threads: usize,
    /// Where the parts run.
    pub fork: &'a Fork<'a>,
}

impl std::fmt::Debug for Phases<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Phases")
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}

impl Phases<'static> {
    /// Every phase inline on the calling thread.
    pub const INLINE: Phases<'static> = Phases {
        threads: 1,
        fork: &inline_fork,
    };
}

impl Phases<'_> {
    /// Runs `job` over contiguous parts of `items` (at least `min_part` items each, at most
    /// [`Phases::threads`] parts) and returns what the parts appended to their output, in the
    /// order of `items`.
    pub fn map<T: Sync, R: Send>(
        &self,
        items: &[T],
        min_part: usize,
        job: impl Fn(&[T], &mut Vec<R>) + Sync,
    ) -> Vec<R> {
        let n = items.len();
        let parts = self.threads.min(n.div_ceil(min_part.max(1))).max(1);
        let mut out = Vec::new();
        if parts == 1 {
            job(items, &mut out);
            return out;
        }
        let per = n.div_ceil(parts);
        let slots: Vec<Mutex<Vec<R>>> = (0..parts).map(|_| Mutex::new(Vec::new())).collect();
        (self.fork)(parts, &|i| {
            let part = items.get(i * per..((i + 1) * per).min(n)).unwrap_or(&[]);
            let mut local = Vec::new();
            job(part, &mut local);
            *slots[i].lock().unwrap_or_else(PoisonError::into_inner) = local;
        });
        for slot in slots {
            out.append(&mut slot.into_inner().unwrap_or_else(PoisonError::into_inner));
        }
        out
    }
}

/// A tile: `size × size` chunks, aligned to the chunk grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TilePos {
    /// Tile x.
    pub x: i32,
    /// Tile z.
    pub z: i32,
}

impl TilePos {
    /// The tile of a chunk.
    #[inline]
    pub fn of(chunk: ChunkPos, size: i32) -> TilePos {
        TilePos {
            x: chunk.x.div_euclid(size),
            z: chunk.z.div_euclid(size),
        }
    }
}

/// The phases with their own random stream per tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Random ticks and precipitation.
    RandomTicks = 1,
    /// Natural spawning.
    Spawning = 2,
    /// The seeds of the mobs natural spawning adds.
    SpawnSeeds = 3,
}

impl<'a> TileRun<'a> {
    /// A run on `fork` with one thread and no measured costs.
    pub fn new(size: i32, min_chunks: usize, fork: &'a Fork<'a>) -> Self {
        TileRun {
            size,
            min_chunks,
            fork,
            threads: 1,
            costs: None,
        }
    }

    /// Whether a phase over `chunks` runs tiled this tick: the work spans several tiles, is
    /// large enough and reads of the level are pure.
    pub(crate) fn applies(&self, level: &Level, chunks: &[ChunkPos]) -> bool {
        let Some(&first) = chunks.first() else {
            return false;
        };
        let size = self.size.max(1);
        let tile = TilePos::of(first, size);
        chunks.len() >= self.min_chunks
            && level.chunks.reads_are_pure()
            && chunks.iter().any(|&c| TilePos::of(c, size) != tile)
    }

    /// Every tile with its chunks, in tile order. The chunks of a tile are in the order of
    /// `chunks`; a tile's job sorts them ([`sort_chunks`]) so the result does not depend on it.
    pub(crate) fn group(&self, chunks: &[ChunkPos]) -> Vec<(TilePos, Vec<ChunkPos>)> {
        let size = self.size.max(1);
        let mut index: FxHashMap<TilePos, usize> = FxHashMap::default();
        let mut tiles: Vec<(TilePos, Vec<ChunkPos>)> = Vec::new();
        for &chunk in chunks {
            let tile = TilePos::of(chunk, size);
            let i = *index.entry(tile).or_insert_with(|| {
                tiles.push((tile, Vec::new()));
                tiles.len() - 1
            });
            tiles[i].1.push(chunk);
        }
        tiles.sort_unstable_by_key(|t| t.0);
        tiles
    }

    /// The owner of a tile: this process, for every tile, until tiles can live on other
    /// servers.
    #[inline]
    pub fn owner(&self, _tile: TilePos) -> OwnerId {
        OwnerId::LOCAL
    }

    /// Hands every tile with its input to its owner and returns the replies in the order of
    /// `tiles`. Local owners run `job` on the workers, in the jobs [`plan`] forms from the
    /// tiles' costs in `phase`; each tile's time updates its cost.
    pub(crate) fn map<T: Send, R: Send>(
        &self,
        phase: Phase,
        tiles: Vec<(TilePos, T)>,
        job: impl Fn(TilePos, T) -> R + Sync,
    ) -> Vec<TileReply<R>> {
        type Slot<V> = Mutex<Option<V>>;
        let lock = |costs: &'a TileCosts| costs.0.lock().unwrap_or_else(PoisonError::into_inner);
        let positions: Vec<TilePos> = tiles.iter().map(|t| t.0).collect();
        let cost: Vec<u64> = match self.costs {
            Some(costs) => {
                let map = lock(costs);
                let known: Vec<Option<u64>> = positions
                    .iter()
                    .map(|&tile| map.get(&(phase, tile)).copied())
                    .collect();
                // A tile without a measurement costs what the measured ones cost on average.
                let (sum, count) = known
                    .iter()
                    .flatten()
                    .fold((0u64, 0u64), |(s, c), &v| (s.saturating_add(v), c + 1));
                let unknown = sum.checked_div(count).map_or(1, |v| v.max(1));
                known.into_iter().map(|v| v.unwrap_or(unknown)).collect()
            }
            None => vec![1; positions.len()],
        };
        let jobs = plan(&positions, &cost, self.threads);
        let input: Vec<Slot<T>> = tiles.into_iter().map(|t| Mutex::new(Some(t.1))).collect();
        let output: Vec<Slot<(R, u64)>> = input.iter().map(|_| Mutex::new(None)).collect();
        (self.fork)(jobs.len(), &|j| {
            for &i in &jobs[j] {
                let item = input[i]
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(item) = item {
                    let start = Instant::now();
                    let changes = job(positions[i], item);
                    let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    *output[i].lock().unwrap_or_else(PoisonError::into_inner) =
                        Some((changes, nanos));
                }
            }
        });
        let replies: Vec<(TilePos, Option<(R, u64)>)> = positions
            .into_iter()
            .zip(output)
            .map(|(tile, slot)| {
                (
                    tile,
                    slot.into_inner().unwrap_or_else(PoisonError::into_inner),
                )
            })
            .collect();
        if let Some(costs) = self.costs {
            let mut map = lock(costs);
            let averaged: Vec<((Phase, TilePos), u64)> = replies
                .iter()
                .filter_map(|(tile, reply)| {
                    let nanos = reply.as_ref()?.1;
                    let cost = match map.get(&(phase, *tile)) {
                        Some(&old) => old - old / 4 + nanos / 4,
                        None => nanos,
                    };
                    Some(((phase, *tile), cost))
                })
                .collect();
            map.retain(|key, _| key.0 != phase);
            map.extend(averaged);
        }
        replies
            .into_iter()
            .filter_map(|(tile, reply)| {
                let (changes, _) = reply?;
                Some(TileReply {
                    tile,
                    owner: self.owner(tile),
                    changes,
                })
            })
            .collect()
    }
}

/// Forms the jobs of a round from the busy tiles and their costs: the indices of each job's
/// tiles in tile order, the jobs longest first. A tile costing at least a job's share of the
/// round (the total over [`JOBS_PER_THREAD`] jobs per thread) is hot and runs alone; cold tiles
/// of the same colour join a job while it stays under that share and the tile is adjacent to
/// one of the job's tiles. One thread gets one job.
fn plan(tiles: &[TilePos], cost: &[u64], threads: usize) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..tiles.len()).collect();
    order.sort_unstable_by_key(|&i| tiles[i]);
    if threads <= 1 || tiles.len() <= 1 {
        return if order.is_empty() {
            Vec::new()
        } else {
            vec![order]
        };
    }
    let total = cost.iter().fold(0u64, |s, &c| s.saturating_add(c));
    #[allow(clippy::cast_possible_truncation)]
    let share = (total / (threads as u64).saturating_mul(JOBS_PER_THREAD)).max(1);
    let colour = |t: TilePos| (t.x & 1, t.z & 1);
    // Neighbours of one colour lie two tiles apart.
    let adjacent = |a: TilePos, b: TilePos| (a.x - b.x).abs() <= 2 && (a.z - b.z).abs() <= 2;
    let mut jobs: Vec<(u64, Vec<usize>)> = order
        .iter()
        .filter(|&&i| cost[i] >= share)
        .map(|&i| (cost[i], vec![i]))
        .collect();
    for c in [(0, 0), (0, 1), (1, 0), (1, 1)] {
        let mut batch: (u64, Vec<usize>) = (0, Vec::new());
        for &i in &order {
            if cost[i] >= share || colour(tiles[i]) != c {
                continue;
            }
            let joins =
                batch.0 + cost[i] <= share && batch.1.iter().any(|&j| adjacent(tiles[j], tiles[i]));
            if !joins && !batch.1.is_empty() {
                jobs.push(std::mem::take(&mut batch));
            }
            batch.0 += cost[i];
            batch.1.push(i);
        }
        if !batch.1.is_empty() {
            jobs.push(batch);
        }
    }
    jobs.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| tiles[a.1[0]].cmp(&tiles[b.1[0]]))
    });
    jobs.into_iter().map(|(_, job)| job).collect()
}

/// Sorts a tile's chunks by x and z: the order its job works in.
pub(crate) fn sort_chunks(chunks: &mut [ChunkPos]) {
    chunks.sort_unstable_by_key(|c| (c.x, c.z));
}

/// The seed of a tile's random stream for one phase of one tick: a mix of the world seed, the
/// game time and the tile, the same on every run and for every thread count.
pub(crate) fn tile_seed(level_seed: i64, game_time: i64, tile: TilePos, phase: Phase) -> i64 {
    #[allow(clippy::cast_sign_loss)]
    let mut h = (level_seed as u64) ^ 0x9E37_79B9_7F4A_7C15;
    #[allow(clippy::cast_sign_loss)]
    for value in [
        game_time as u64,
        u64::from(tile.x as u32),
        u64::from(tile.z as u32),
        phase as u64,
    ] {
        // SplitMix64's finaliser over the running hash and the next value.
        h = (h ^ value).wrapping_add(0x9E37_79B9_7F4A_7C15);
        h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 31;
    }
    #[allow(clippy::cast_possible_wrap)]
    let seed = h as i64;
    seed
}

/// What a tile's random tick round picked: the changes of its [`TileReply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// `tickPrecipitation` at this column.
    Precipitation(BlockPos),
    /// A random tick of the block or fluid at this position.
    Block(BlockPos),
}

impl Level {
    /// [`Level::tick_chunks`] in tile region mode: the tiles pick their random tick positions
    /// in parallel, each with its own random stream, and keep the positions whose block or
    /// fluid ticks randomly (most do not); the picks then run serially in tile order. Falls
    /// back to [`Level::tick_chunks`]' order when [`TileRun`] does not apply.
    pub fn tick_chunks_tiled(
        &mut self,
        scratch: &mut Vec<ChunkPos>,
        run: &TileRun<'_>,
        phases: &Phases<'_>,
    ) {
        let speed = self.settings.random_tick_speed;
        scratch.clear();
        self.chunks.random_ticking_candidates(scratch);
        if !run.applies(self, scratch) {
            self.tick_chunks(scratch, phases);
            return;
        }
        let tiles = run.group(scratch);
        let level: &Level = self;
        let replies = run.map(Phase::RandomTicks, tiles, |tile, mut chunks| {
            let mut random = LegacyRandom::new(tile_seed(
                level.seed,
                level.game_time,
                tile,
                Phase::RandomTicks,
            ));
            let mut rand_value = random.next_int();
            let mut out = Vec::new();
            sort_chunks(&mut chunks);
            for chunk in chunks {
                if level.chunks.is_random_ticking(chunk) {
                    level.pick_random_ticks(chunk, speed, &mut random, &mut rand_value, &mut out);
                }
            }
            out
        });
        for pick in replies.into_iter().flat_map(|reply| reply.changes) {
            match pick {
                Pick::Precipitation(pos) => block::weather::tick_precipitation(self, pos),
                Pick::Block(pos) => self.random_tick_at(pos),
            }
        }
    }

    /// `ServerLevel.tickChunk` without its effects: the positions it would tick.
    fn pick_random_ticks(
        &self,
        chunk: ChunkPos,
        speed: i32,
        random: &mut LegacyRandom,
        rand_value: &mut i32,
        out: &mut Vec<Pick>,
    ) {
        let min_x = chunk.x * 16;
        let min_z = chunk.z * 16;
        for _ in 0..speed {
            if random.next_int_bounded(48) == 0 {
                out.push(Pick::Precipitation(super::tick::random_pos(
                    rand_value, min_x, 0, min_z, 15,
                )));
            }
        }
        let Some(full) = self.chunks.chunk(chunk) else {
            return;
        };
        let min_section = self.min_y >> 4;
        for (index, section) in full.chunk.sections().iter().enumerate() {
            if !section.summary().is_randomly_ticking() {
                continue;
            }
            let min_y = (min_section + i32::try_from(index).unwrap_or(0)) * 16;
            for _ in 0..speed {
                let pos = super::tick::random_pos(rand_value, min_x, min_y, min_z, 15);
                #[allow(clippy::cast_sign_loss)]
                let state = full
                    .chunk
                    .block((pos.x & 15) as u32, pos.y, (pos.z & 15) as u32);
                let Some(state) = state.map(BlockState) else {
                    continue;
                };
                if state.random_ticks() || state.fluid_state().info().random_ticks {
                    out.push(Pick::Block(pos));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level::tests::flat_level;
    use crate::level::update;
    use vanillite_data::block::ids;
    use vanillite_worldgen::behaviour::StateExt;

    /// A fork on real threads: every job on its own thread.
    pub(crate) fn threaded_fork(n: usize, job: &(dyn Fn(usize) + Sync)) {
        std::thread::scope(|scope| {
            for i in 0..n {
                scope.spawn(move || job(i));
            }
        });
    }

    fn run<'a>(fork: &'a Fork<'a>) -> TileRun<'a> {
        // The flat test world ticks the chunks around the origin: four tiles of 2 × 2.
        TileRun {
            threads: 4,
            ..TileRun::new(2, 0, fork)
        }
    }

    /// The blocks of the test world's grass layer and the one above.
    fn surface(level: &Level) -> Vec<BlockState> {
        let mut out = Vec::new();
        for x in -40..40 {
            for z in -40..40 {
                out.push(level.block(BlockPos::new(x, -61, z)));
                out.push(level.block(BlockPos::new(x, -60, z)));
            }
        }
        out
    }

    #[test]
    fn tiles_group_chunks_and_seeds_differ() {
        let fork: &Fork<'_> = &inline_fork;
        let run = TileRun::new(16, 0, fork);
        let chunks = vec![
            ChunkPos::new(17, 0),
            ChunkPos::new(-1, -1),
            ChunkPos::new(0, 0),
            ChunkPos::new(15, 15),
            ChunkPos::new(-16, -1),
            ChunkPos::new(-17, 0),
        ];
        let mut tiles = run.group(&chunks);
        for (_, chunks) in &mut tiles {
            sort_chunks(chunks);
        }
        assert_eq!(
            tiles,
            vec![
                (TilePos { x: -2, z: 0 }, vec![ChunkPos::new(-17, 0)]),
                (
                    TilePos { x: -1, z: -1 },
                    vec![ChunkPos::new(-16, -1), ChunkPos::new(-1, -1)]
                ),
                (
                    TilePos { x: 0, z: 0 },
                    vec![ChunkPos::new(0, 0), ChunkPos::new(15, 15)]
                ),
                (TilePos { x: 1, z: 0 }, vec![ChunkPos::new(17, 0)]),
            ]
        );
        let a = tile_seed(1, 5, TilePos { x: 0, z: 0 }, Phase::RandomTicks);
        assert_eq!(
            a,
            tile_seed(1, 5, TilePos { x: 0, z: 0 }, Phase::RandomTicks)
        );
        assert_ne!(
            a,
            tile_seed(1, 5, TilePos { x: 0, z: 1 }, Phase::RandomTicks)
        );
        assert_ne!(
            a,
            tile_seed(1, 5, TilePos { x: 1, z: 0 }, Phase::RandomTicks)
        );
        assert_ne!(
            a,
            tile_seed(1, 6, TilePos { x: 0, z: 0 }, Phase::RandomTicks)
        );
        assert_ne!(a, tile_seed(1, 5, TilePos { x: 0, z: 0 }, Phase::Spawning));
    }

    /// Dirt next to grass gets grass by random ticks. With the dirt in one tile and the only
    /// grass in the next, the spread crosses the tile border: the commit writes the whole world.
    #[test]
    fn random_ticks_spread_across_a_tile_border() {
        let mut level = flat_level();
        // Tile size 2: chunks 0..2 are one tile, chunks -2..0 the next; x = -1 | 0 is a border.
        for x in -48..48 {
            for z in -48..48 {
                let pos = BlockPos::new(x, -61, z);
                if level.block(pos).is(ids::GRASS_BLOCK) && x < 0 {
                    level.set_block(pos, ids::DIRT.default_state(), update::ALL);
                }
            }
        }
        let fork: &Fork<'_> = &inline_fork;
        let mut scratch = Vec::new();
        let dirt_left = |level: &Level| {
            (0..16)
                .filter(|&z| level.block(BlockPos::new(-1, -61, z)).is(ids::DIRT))
                .count()
        };
        assert_eq!(dirt_left(&level), 16);
        for _ in 0..4000 {
            level.tick_time();
            level.tick_chunks_tiled(&mut scratch, &run(fork), &Phases::INLINE);
        }
        assert!(
            dirt_left(&level) < 16,
            "grass spreads from the tile at x >= 0 into the tile at x < 0"
        );
    }

    /// The same world ticked on one thread and on many gives the same blocks: the result does
    /// not depend on threads or timing.
    #[test]
    fn tiled_random_ticks_do_not_depend_on_threads() {
        let ticked = |fork: &Fork<'_>| {
            let mut level = flat_level();
            for x in -40..40 {
                for z in -40..40 {
                    if (x + z) % 3 == 0 {
                        level.set_block(
                            BlockPos::new(x, -61, z),
                            ids::DIRT.default_state(),
                            update::ALL,
                        );
                    }
                }
            }
            let mut scratch = Vec::new();
            for _ in 0..300 {
                level.tick_time();
                level.tick_chunks_tiled(&mut scratch, &run(fork), &Phases::INLINE);
            }
            surface(&level)
        };
        let inline = ticked(&inline_fork);
        assert_eq!(inline, ticked(&threaded_fork));
        // And the ticks did something.
        let untouched = {
            let mut level = flat_level();
            for x in -40..40 {
                for z in -40..40 {
                    if (x + z) % 3 == 0 {
                        level.set_block(
                            BlockPos::new(x, -61, z),
                            ids::DIRT.default_state(),
                            update::ALL,
                        );
                    }
                }
            }
            surface(&level)
        };
        assert_ne!(inline, untouched);
    }

    /// Below the chunk threshold, in one tile, or while a synchronous load's ticket exists the
    /// phase runs in Vanilla's serial order with the level's own random.
    #[test]
    fn small_or_impure_rounds_fall_back_to_the_serial_order() {
        let serial = {
            let mut level = flat_level();
            let mut scratch = Vec::new();
            for _ in 0..200 {
                level.tick_time();
                level.tick_chunks(&mut scratch, &Phases::INLINE);
            }
            (surface(&level), level.random().next_int())
        };
        let fork: &Fork<'_> = &inline_fork;
        let with = |run: TileRun<'_>, load: bool| {
            let mut level = flat_level();
            if load {
                // An `unknown` ticket of a synchronous load at the border of the loaded area.
                level.load_chunk(ChunkPos::new(40, 40));
                assert!(!level.chunks.reads_are_pure());
            }
            let mut scratch = Vec::new();
            for _ in 0..200 {
                level.tick_time();
                level.tick_chunks_tiled(&mut scratch, &run, &Phases::INLINE);
                if load {
                    level.load_chunk(ChunkPos::new(40, 40));
                }
            }
            (surface(&level), level.random().next_int())
        };
        // Too few chunks.
        let small = TileRun::new(2, 1_000_000, fork);
        assert_eq!(with(small, false), serial);
        // One tile holds everything.
        let level = flat_level();
        let one_tile = [ChunkPos::new(0, 0), ChunkPos::new(1, 1)];
        assert!(!run(fork).applies(&level, &one_tile));
        assert!(run(fork).applies(&level, &[ChunkPos::new(0, 0), ChunkPos::new(2, 1)]));
        // The tiled round draws from other streams.
        assert!(with(run(fork), false) != serial);
        // A synchronous load at the border of the loaded chunks: serial again.
        assert!(with(run(fork), true).0 == serial.0);
    }
}
