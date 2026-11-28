//! The chunk runtime of one world (architecture 9.2): chunk map, tickets and levels, the chunk
//! scheduler, block access and saving. Sending chunks to players is the game's
//! (`vanillite_game::player::chunks`); players keep their chunks loaded with tickets
//! ([`Chunks::player_ticket_level`]).
//!
//! [`Chunks`] is owned by the world's thread. Everything slow runs as background jobs on the
//! [`Executor`] (the compute pool): decoding and encoding chunk records with the world's
//! [`ChunkCodec`], generation steps through `chunk-generator`, light through `light-engine`.
//! Jobs get snapshots (`Arc<ProtoChunk>`, sections are `Arc`s too) and send their results back
//! over a channel; [`Chunks::tick`] applies them on the world thread. A live (full) chunk is
//! changed only by the world thread, copy-on-write if a job still holds a snapshot.
//!
//! Records are read and written through the asynchronous `chunk-storage` slot
//! ([`ChunkStore`], #47): requests carry a token, results arrive in this world's inbox. The
//! store answers a load of a chunk whose save is still queued from the pending save
//! (read-your-writes); only the short window while a snapshot is encoded is covered here.
//!
//! Saving runs in rounds ([`Chunks::start_save_round`]): the changed chunks are captured after
//! the world tick from the tick's remaining time ([`Chunks::autosave_step`]), at least
//! `min_chunks_per_tick` per tick, encoded on the pool in jobs of about 1 ms, and committed once
//! all are submitted. Chunks that unload are saved at once and committed within the commit
//! interval.
//!
//! Scheduling follows Vanilla's generation pyramid: a chunk's level decides the status it must
//! reach; a step runs once every chunk in its requirement radius has the required status.
//! Tickets reach only the levels the game reads ([`MAX_TICKET_LEVEL`]); a chunk with a ticket
//! that must still be generated gets a generation ticket, whose levels up to [`MAX_LEVEL`] keep
//! the chunks its steps need. Those are loaded only once a step asks for them, so already
//! generated terrain keeps no pyramid in memory (#47 D3). Chunks waiting for a neighbour are parked on it and checked again only when it makes
//! progress. Steps with a block write radius (features) take their neighbours exclusively;
//! light results combine by maximum and need no exclusive access.
//!
//! Between ticks the owner may call [`Chunks::pump`] when the runtime's waker
//! ([`Chunks::set_waker`]) reports finished jobs: it applies them and starts the next steps right
//! away instead of at the next tick, so the workers never wait for the tick. Everything that
//! changes the live world (chunks becoming full, tickets, unloading) still happens in
//! [`Chunks::tick`].
//!
//! Full chunks carry their scheduled block and fluid ticks in the world's [`LevelTicks`] while
//! they are live; they go back into the chunk when it is saved or unloaded. Simulation tickets
//! (Vanilla's `SimulationChunkTracker`) decide which chunks tick.

mod holders;
pub mod levels;
pub mod simulation;

use std::collections::BTreeSet;

use holders::Holders;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use vanillite_base::hash::{FxHashMap, FxHashSet};
use vanillite_base::{BlockPos, ChunkPos};
use vanillite_chunk::light::{ChunkLight, LightNibbles};
use vanillite_chunk::packet::SectionMask;
use vanillite_chunk::slots::{
    ChunkCodec, ChunkGenerator, ChunkStore, Completion, CompletionTarget, DimensionId, LightEngine,
    Neighbours, Priority, RecordKey, RecordMeta, StoreOp, StoreResult, Token, WorldInfo,
};
use vanillite_chunk::{BlockStateId, ChunkStatus, ProtoChunk, SectionSummary};

pub use levels::{
    BLOCK_TICKING_LEVEL, ENTITY_TICKING_LEVEL, FULL_LEVEL, LevelChange, MAX_LEVEL,
    MAX_TICKET_LEVEL, TicketLevels, required_status,
};

use simulation::SimulationLevels;

use crate::ticks::{LevelTicks, ScheduledTick, TickPriority};

/// Most focus points a world orders its chunk work by ([`Chunks::set_focus`]).
pub const MAX_FOCUS: usize = 16;

/// An empty slot of the chunk cache (`ChunkPos.INVALID_CHUNK_POS`).
const NO_CHUNK: ChunkPos = ChunkPos::new(i32::MAX, i32::MAX);

/// Vanilla's `ServerChunkCache` cache of the last four chunks read, newest first. Atomics
/// (plain loads and stores) keep [`Chunks`] shareable between threads for read-only phases;
/// those only run while no `unknown` ticket exists, so nothing writes here then.
#[derive(Debug)]
struct ChunkCache([AtomicU64; 4]);

impl Default for ChunkCache {
    fn default() -> ChunkCache {
        ChunkCache(std::array::from_fn(|_| {
            AtomicU64::new(ChunkCache::key(NO_CHUNK))
        }))
    }
}

impl ChunkCache {
    #[allow(clippy::cast_sign_loss)]
    const fn key(pos: ChunkPos) -> u64 {
        ((pos.x as u32 as u64) << 32) | pos.z as u32 as u64
    }

    fn clear(&self) {
        for slot in &self.0 {
            slot.store(ChunkCache::key(NO_CHUNK), Ordering::Relaxed);
        }
    }

    /// Whether the cache holds `pos`; if not, it goes in as the newest entry.
    #[inline]
    fn note(&self, pos: ChunkPos) -> bool {
        let key = ChunkCache::key(pos);
        let mut cache = [0u64; 4];
        for (value, slot) in cache.iter_mut().zip(&self.0) {
            *value = slot.load(Ordering::Relaxed);
        }
        if cache.contains(&key) {
            return true;
        }
        cache.copy_within(0..3, 1);
        cache[0] = key;
        for (value, slot) in cache.iter().zip(&self.0) {
            slot.store(*value, Ordering::Relaxed);
        }
        false
    }
}

/// Runs background jobs; the server's compute pool, or inline in tests.
pub trait Executor: Send + Sync {
    /// Queues a job; lower `priority` runs first.
    fn spawn(&self, priority: u32, job: Box<dyn FnOnce() + Send>);

    /// The owner thread starts (`true`) or stops (`false`) waiting for jobs (a synchronous
    /// load): an executor that holds background work back during ticks must run it now.
    fn blocked(&self, _blocked: bool) {}
}

/// Runs every job at once on the calling thread (deterministic tests, `World::headless`).
#[derive(Debug, Default)]
pub struct InlineExecutor;

impl Executor for InlineExecutor {
    fn spawn(&self, _: u32, job: Box<dyn FnOnce() + Send>) {
        job();
    }
}

/// Called from a worker when a job of the runtime finished and no wake is pending; the owner
/// then calls [`Chunks::pump`] on its thread. Must not block.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// The pending-wake flag and the owner's waker, shared with the jobs.
#[derive(Default)]
struct WakeState {
    pending: AtomicBool,
    waker: OnceLock<Waker>,
}

/// The jobs' end of the result channel: sends and wakes the owner once per pump.
#[derive(Clone)]
struct DoneSender {
    tx: Sender<Done>,
    wake: Arc<WakeState>,
}

impl WakeState {
    /// Wakes the owner unless a wake is already pending.
    fn wake(&self) {
        if let Some(waker) = self.waker.get()
            && !self.pending.swap(true, Ordering::AcqRel)
        {
            waker();
        }
    }
}

impl DoneSender {
    fn send(&self, done: Done) {
        // A dropped runtime does not want results any more.
        if self.tx.send(done).is_ok() {
            self.wake.wake();
        }
    }
}

/// The slots one world's chunks run on.
#[derive(Clone)]
pub struct ChunkSlots {
    /// `chunk-storage`: the record store of this dimension.
    pub store: Arc<dyn ChunkStore>,
    /// Turns this world's records into chunks and back.
    pub codec: Arc<dyn ChunkCodec>,
    /// `chunk-generator`.
    pub generator: Arc<dyn ChunkGenerator>,
    /// `light-engine`.
    pub light: Arc<dyn LightEngine>,
    /// Where jobs run.
    pub executor: Arc<dyn Executor>,
}

impl fmt::Debug for ChunkSlots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkSlots").finish_non_exhaustive()
    }
}

/// Tuning of the chunk runtime.
#[derive(Clone, Copy, Debug)]
pub struct ChunkSettings {
    /// Server view distance in chunks (2..=32): players load and see chunks this far.
    pub view_distance: i32,
    /// Ticks a chunk stays loaded after no ticket needs it any more.
    pub unload_delay: u64,
    /// Background jobs (loads, generation steps, light) in flight at once.
    pub max_jobs: usize,
    /// Simulation distance in chunks (2..=32): chunks this far from a player tick.
    pub simulation_distance: i32,
    /// Chunks whose light is updated in one job after block changes; the rest waits for the
    /// next job.
    pub max_relights: usize,
    /// Saving.
    pub save: SaveSettings,
    /// Every ticket's full chunks are loaded before [`Chunks::add_ticket`] returns, like the
    /// synchronous `getChunk` with which Vanilla loads the destination of a teleport. Only
    /// for the parity harness (the exact Vanilla order): it blocks the tick.
    pub blocking_tickets: bool,
}

/// Tuning of saving (`storage.*`).
#[derive(Clone, Copy, Debug)]
pub struct SaveSettings {
    /// This world's dimension in record keys.
    pub dimension: DimensionId,
    /// Most time of one tick spent capturing chunks of a save round; only the tick's remaining
    /// time is used (`storage.autosave.tick-budget`).
    pub tick_budget: Duration,
    /// Chunks captured per tick even when the tick has no time left, so a round ends
    /// (`storage.autosave.min-chunks-per-tick`).
    pub min_chunks_per_tick: usize,
    /// Ticks after which saves of unloaded chunks are committed (`storage.commit-interval`).
    pub commit_interval: u64,
    /// Bytes of saves waiting for IO above which no more saves are queued: chunks stay loaded
    /// and rounds pause (`storage.max-pending-writes`).
    pub max_backlog: usize,
}

impl Default for SaveSettings {
    fn default() -> Self {
        SaveSettings {
            dimension: DimensionId(0),
            tick_budget: Duration::from_micros(500),
            min_chunks_per_tick: 16,
            commit_interval: 30 * 20,
            max_backlog: 64 << 20,
        }
    }
}

/// Runs once a save round is committed.
pub type SavedCallback = Box<dyn FnOnce() + Send + Sync>;

impl Default for ChunkSettings {
    fn default() -> Self {
        ChunkSettings {
            view_distance: 10,
            unload_delay: 100,
            max_jobs: 64,
            simulation_distance: 10,
            max_relights: 1024,
            save: SaveSettings::default(),
            blocking_tickets: false,
        }
    }
}

/// A ticket keeping chunks loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TicketId(u64);

/// One chunk slot of the map.
#[derive(Debug, Default)]
struct Holder {
    /// The chunk; `None` while it loads.
    chunk: Option<Arc<ProtoChunk>>,
    level: u8,
    /// A job writes this chunk (load, generation step).
    busy: bool,
    /// Blocks (everything but entities) changed since the last save; set through
    /// [`Holder::touch`].
    dirty: bool,
    /// Entities changed since the last save; set through [`Holder::touch_entities`].
    entities_dirty: bool,
    /// The chunk has an entities record in the store (an empty list deletes it).
    entities_stored: bool,
    /// The POI changed since the last save; set through [`Holder::touch_poi`].
    poi_dirty: bool,
    /// The load request while the chunk is read.
    loading: Option<Token>,
    /// Tick from which an unneeded chunk may be unloaded.
    unload_at: Option<u64>,
    /// Queued for a scheduling check.
    queued: bool,
    /// The order within its level it was queued with ([`Chunks::set_focus`]).
    rank: u8,
    /// Whether it was queued with its next step running inline ([`Holder::inline_next`]).
    queued_inline: bool,
    /// The [`ChunkStatus::reach`] of the next step it was queued with ([`urgency`]).
    queued_reach: u8,
    /// Set while a queued generation step of this chunk is not needed any more (its level left
    /// [`MAX_LEVEL`]); the step is then skipped when it starts.
    unneeded: Option<Arc<AtomicBool>>,
    /// The step whose missing neighbours were last added as dependencies.
    planned: Option<ChunkStatus>,
    /// Where the last check of the planned step stopped in its scan of the neighbours (index
    /// into the square around the chunk): the next check resumes there.
    scan_from: u16,
    /// The chunk's stored entities are live (`ChunkLoadStatus.LOADED`); set by the entity
    /// storage at the end of the level tick, required for scheduled ticks.
    entities_loaded: bool,
    /// Inhabited ticks (`incrementInhabitedTime`) not yet folded into the shared chunk: they
    /// are written into it when the chunk is saved, so counting never copies a chunk.
    inhabited: i64,
    /// Mirrors `Chunks::visibility` containing this position (the chunk is accessible), so
    /// [`Chunks::accessible_block`] needs one map lookup.
    accessible: bool,
    /// Mirrors `chunk` being a full chunk ([`Holder::sync_full`] after every change of it), so
    /// the ticking checks of the eight neighbours read the holder only, not the chunk.
    full: bool,
}

impl Holder {
    /// Whether the next step runs inline on the world thread (spawn and full): it needs no
    /// job slot and frees the chunks waiting for this one.
    fn inline_next(&self) -> bool {
        matches!(self.status(), Some(ChunkStatus::Light | ChunkStatus::Spawn))
    }

    /// The [`ChunkStatus::reach`] of the next step (a load: the first one).
    fn next_reach(&self) -> u8 {
        self.status()
            .and_then(ChunkStatus::next)
            .unwrap_or(ChunkStatus::Empty)
            .reach()
    }

    fn status(&self) -> Option<ChunkStatus> {
        self.chunk.as_ref().map(|c| c.status)
    }

    /// Follows a change of the chunk or its status in [`Holder::full`].
    #[inline]
    fn sync_full(&mut self) {
        self.full = self.status() == Some(ChunkStatus::Full);
    }

    /// Whether the chunk is full, from the mirror.
    #[inline]
    fn is_full(&self) -> bool {
        debug_assert_eq!(self.full, self.status() == Some(ChunkStatus::Full));
        self.full
    }

    /// Marks the chunk changed; the first change since the last save queues it for the next
    /// save round.
    fn touch(&mut self, pos: ChunkPos, changed: &mut Vec<ChunkPos>) {
        if !self.dirty {
            if !self.entities_dirty && !self.poi_dirty {
                changed.push(pos);
            }
            self.dirty = true;
        }
    }

    /// Marks the chunk's entities changed, like [`Holder::touch`].
    fn touch_entities(&mut self, pos: ChunkPos, changed: &mut Vec<ChunkPos>) {
        if !self.entities_dirty {
            if !self.dirty && !self.poi_dirty {
                changed.push(pos);
            }
            self.entities_dirty = true;
        }
    }

    /// Marks the chunk's POI changed, like [`Holder::touch`].
    fn touch_poi(&mut self, pos: ChunkPos, changed: &mut Vec<ChunkPos>) {
        if !self.poi_dirty {
            if !self.dirty && !self.entities_dirty {
                changed.push(pos);
            }
            self.poi_dirty = true;
        }
    }

    /// Whether anything is unsaved.
    fn unsaved(&self) -> bool {
        self.dirty || self.entities_dirty || self.poi_dirty
    }
}

/// A chunk captured for saving and the records to write.
#[derive(Clone)]
struct Snapshot {
    chunk: Arc<ProtoChunk>,
    /// Write the chunk record.
    blocks: bool,
    /// Write the entities record (delete it if the list is empty).
    entities: bool,
    /// Write the POI record (delete it if there is none).
    poi: bool,
}

/// A load waiting for the store: the chunk and the entities record.
#[derive(Default)]
struct PendingLoad {
    pos: ChunkPos,
    chunk: Option<StoreResult>,
    entities: Option<StoreResult>,
    poi: Option<StoreResult>,
}

/// The stored entities and POI of a loaded chunk.
type LoadedRecords = (
    io::Result<Option<Vec<Arc<[u8]>>>>,
    io::Result<Option<Arc<[u8]>>>,
);

/// A chunk read between ticks with its stored entities, waiting for the next tick.
type LoadedChunk = (ChunkPos, Box<ProtoChunk>, LoadedRecords);

/// A finished background job.
enum Done {
    /// A chunk was read (`None`: never saved) with its stored entities and POI (`None`: no
    /// record).
    Loaded(ChunkPos, io::Result<Option<Box<ProtoChunk>>>, LoadedRecords),
    Stepped {
        status: ChunkStatus,
        targets: Vec<ProtoChunk>,
        written: Vec<Arc<ProtoChunk>>,
    },
    Lit {
        targets: Vec<ProtoChunk>,
        neighbours: Vec<Arc<ProtoChunk>>,
    },
    /// A generation step that was not needed any more when it started: nothing changed.
    Skipped {
        target: ChunkPos,
        written: Vec<ChunkPos>,
    },
    /// A light update: the changed blocks per chunk, the chunks it read and the new light.
    Relit {
        changes: Vec<(ChunkPos, Vec<BlockPos>)>,
        read: Vec<Arc<ProtoChunk>>,
        light: Vec<(ChunkPos, ChunkLight)>,
    },
    /// An encode job submitted its records.
    Encoded {
        /// Captured for the save round.
        round: bool,
        chunks: Vec<Snapshot>,
        errors: Vec<(ChunkPos, io::Error)>,
    },
}

/// What a scheduling check of one chunk found.
enum Check {
    /// Nothing more to do.
    Done,
    /// Waits for this chunk to make progress.
    Blocked(ChunkPos),
    /// A job started or a step ran inline.
    Progress,
    /// The job budget is used up.
    Full,
}

/// A pure, read-only view of a world's full chunks (`getChunkNow` without the chunk cache):
/// its reads have no side effects, so it is `Send + Sync` and phases that only read (collision,
/// path finding, tracking, AI proposals) can share it across threads while the world thread
/// does not write. It does not touch tickets: a synchronous load's `unknown` ticket is kept
/// alive only by [`Chunks::block`].
#[derive(Clone, Copy)]
pub struct ChunkView<'a> {
    holders: &'a Holders,
}

const _: () = {
    const fn shareable<T: Send + Sync>() {}
    shareable::<ChunkView<'static>>();
};

const _: () = {
    const fn shareable<T: Sync>() {}
    shareable::<Chunks>();
};

impl ChunkView<'_> {
    /// The block state at `pos`, `None` outside full chunks and the build height.
    #[inline(always)]
    pub fn block(&self, pos: BlockPos) -> Option<BlockStateId> {
        self.holders.full_block(pos).map(|(state, _)| state)
    }

    /// [`ChunkView::block`] of every position in the box from `min` to `max` (inclusive), in
    /// X, then Z, then Y order (Y varies fastest), with one chunk lookup per chunk.
    #[inline]
    pub fn blocks_in(&self, min: BlockPos, max: BlockPos, f: impl FnMut(Option<BlockStateId>)) {
        full_blocks_in(self.holders, min, max, f);
    }

    /// The summary of section `section_y` of the full chunk at `chunk`, `None` outside full
    /// chunks and the build height.
    #[inline]
    pub fn section_summary(&self, chunk: ChunkPos, section_y: i32) -> Option<SectionSummary> {
        self.holders
            .full_section(chunk, section_y)
            .map(|section| section.summary())
    }

    /// Whether the chunk is loaded as a full chunk.
    #[inline]
    pub fn is_full(&self, pos: ChunkPos) -> bool {
        self.holders
            .get(&pos)
            .and_then(|h| h.chunk.as_ref())
            .is_some_and(|c| c.status == ChunkStatus::Full)
    }
}

/// The block states of the full chunks in the box from `min` to `max` (inclusive), in X, then
/// Z, then Y order, with one chunk lookup per chunk; `None` outside full chunks and the build
/// height.
fn full_blocks_in(
    holders: &Holders,
    min: BlockPos,
    max: BlockPos,
    mut f: impl FnMut(Option<BlockStateId>),
) {
    let mut last: Option<(ChunkPos, Option<&ProtoChunk>)> = None;
    for x in min.x..=max.x {
        for z in min.z..=max.z {
            let chunk_pos = ChunkPos::new(x >> 4, z >> 4);
            let chunk = match last {
                Some((pos, chunk)) if pos == chunk_pos => chunk,
                _ => {
                    let chunk = holders.get(&chunk_pos).and_then(|holder| {
                        let chunk = holder
                            .chunk
                            .as_ref()
                            .filter(|c| c.status == ChunkStatus::Full)?;
                        Some(&**chunk)
                    });
                    last = Some((chunk_pos, chunk));
                    chunk
                }
            };
            for y in min.y..=max.y {
                f(chunk.and_then(|c| c.chunk.block((x & 15) as u32, y, (z & 15) as u32)));
            }
        }
    }
}

impl fmt::Debug for ChunkView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkView")
            .field("chunks", &self.holders.len())
            .finish()
    }
}

/// The chunk runtime of one world.
pub struct Chunks {
    info: WorldInfo,
    settings: ChunkSettings,
    slots: ChunkSlots,
    holders: Holders,
    /// Levels of the tickets, up to [`MAX_TICKET_LEVEL`].
    levels: TicketLevels,
    tickets: FxHashMap<TicketId, (ChunkPos, u8)>,
    /// Levels of the generation tickets, up to [`MAX_LEVEL`]; a chunk's level is the lower of
    /// both.
    generation: TicketLevels,
    /// The generation ticket of each chunk with a ticket that is being generated.
    generation_tickets: FxHashMap<ChunkPos, u8>,
    next_ticket: u64,
    level_changes: Vec<LevelChange>,
    /// Chunks to check: those whose next step runs inline first (`false` sorts first), then
    /// the most urgent (lowest [`urgency`] of the next step), then the lowest level, then by
    /// rank (nearest to a focus).
    check: BTreeSet<(bool, u8, u8, u8, ChunkPos)>,
    /// Points the players' views move towards ([`Chunks::set_focus`]).
    focus: Vec<ChunkPos>,
    /// Chunks parked on a neighbour.
    waiters: FxHashMap<ChunkPos, Vec<ChunkPos>>,
    in_flight: usize,
    done_tx: DoneSender,
    done_rx: Mutex<Receiver<Done>>,
    /// This world's inbox for store results.
    inbox_tx: CompletionTarget,
    inbox: Mutex<Receiver<Completion>>,
    next_token: u64,
    /// Loads waiting for the store, by token.
    loads: FxHashMap<Token, PendingLoad>,
    /// Snapshots being encoded; a load of such a chunk takes this version. Once submitted,
    /// the store answers loads from the pending write.
    encoding: FxHashMap<ChunkPos, Arc<ProtoChunk>>,
    /// Encode jobs whose records are not submitted yet.
    encode_jobs: usize,
    /// Encode jobs of the save round whose records are not submitted yet.
    round_jobs: usize,
    /// Chunks changed since they were last captured, in change order.
    changed: Vec<ChunkPos>,
    /// The running save round.
    round: Option<SaveRound>,
    /// Number of the last commit started.
    commits: u64,
    /// Callbacks waiting for a commit, by commit number.
    commit_waiters: Vec<(u64, SavedCallback)>,
    /// A commit is running.
    committing: bool,
    /// Records were submitted since the last commit started.
    uncommitted: bool,
    /// Tick of the last commit.
    last_commit: u64,
    /// Inside [`Chunks::pump`]: nothing becomes full and no loaded full chunk goes live.
    between_ticks: bool,
    /// Loaded full chunks that arrived between ticks; they go live in the next tick.
    loaded_between_ticks: Vec<LoadedChunk>,
    /// Chunks whose next step (spawn, full) waits for the next tick.
    due_next_tick: FxHashSet<ChunkPos>,
    unload_candidates: FxHashSet<ChunkPos>,
    /// Changed blocks whose light is still to update, per chunk.
    relight: FxHashMap<ChunkPos, Vec<BlockPos>>,
    /// A light update is in flight. The next one starts when it is applied, so every update
    /// reads the light the one before left. Light updates do not count against the job budget
    /// of loading and generation.
    relighting: bool,
    /// Sections whose sky and block light changed, per full chunk.
    light_changes: FxHashMap<ChunkPos, LightChange>,
    block_changes: Vec<BlockPos>,
    tick: u64,
    /// Simulation tickets and their levels.
    simulation: SimulationLevels,
    simulation_tickets: FxHashMap<TicketId, (ChunkPos, u8)>,
    /// Scheduled block ticks of the live chunks.
    block_ticks: LevelTicks,
    /// Scheduled fluid ticks of the live chunks.
    fluid_ticks: LevelTicks,
    /// The world's game time, for scheduling and saving ticks.
    game_time: i64,
    /// The level's sub-tick counter (`nextSubTickCount`).
    sub_tick: i64,
    /// Live chunks with post-processing marks not handled yet.
    post_process: Vec<ChunkPos>,
    /// Chunks that passed the spawn step since the last [`Self::take_generated`].
    generated: Vec<ChunkPos>,
    /// Chunks that became live since the last [`Chunks::take_promoted`].
    promoted: Vec<ChunkPos>,
    /// Chunks whose generation finished since the start of the last [`Chunks::tick`]
    /// ([`Chunks::generated_now`]).
    generated_full: Vec<ChunkPos>,
    /// `ServerLevel.noSave` (`/save-off`): unneeded chunks are not unloaded.
    no_save: bool,
    /// The visibility of every chunk that is not hidden.
    visibility: FxHashMap<ChunkPos, Visibility>,
    /// Chunks to check for a visibility change at the end of the tick.
    visibility_check: Vec<ChunkPos>,
    /// Visibility changes since the last [`Chunks::take_visibility_changes`].
    visibility_changes: Vec<VisibilityChange>,
    /// The `unknown` tickets of synchronous loads ([`Chunks::load_now`]) with their ticks
    /// left (`TicketType.UNKNOWN`: level 33, timeout 1).
    unknown: FxHashMap<ChunkPos, (TicketId, AtomicI64)>,
    /// `ServerChunkCache.lastChunkPos`: the world thread's cache of the last four accessible
    /// chunks it read, newest first. A read that hits it does not reset an `unknown` ticket.
    /// Only kept while `unknown` tickets exist: a new one clears it.
    chunk_cache: ChunkCache,
    /// Synchronous loads that blocked the tick ([`Chunks::load_now`]).
    sync_loads: SyncLoads,
}

/// Synchronous chunk loads that blocked the world thread (`getChunk(x, z, FULL, true)` of a
/// chunk that was not accessible), counted since the world started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncLoads {
    /// How many loads waited.
    pub count: u64,
    /// How long they waited together.
    pub waited: Duration,
}

/// What the entities of a chunk do (Vanilla's `Visibility`, from the chunk's full status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// Not accessible (level above full, or not loaded): its entities are stored.
    Hidden,
    /// Full or block ticking: its entities are loaded but do not tick.
    Tracked,
    /// Entity ticking: its entities tick.
    Ticking,
}

/// A chunk's visibility changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibilityChange {
    /// The chunk.
    pub pos: ChunkPos,
    /// Before.
    pub old: Visibility,
    /// Now.
    pub new: Visibility,
}

/// A save round: chunks changed before it started, captured from the ticks' remaining time.
#[derive(Default)]
struct SaveRound {
    pending: std::collections::VecDeque<ChunkPos>,
    waiters: Vec<SavedCallback>,
}

impl fmt::Debug for Chunks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chunks")
            .field("dimension", &self.info.dimension)
            .field("holders", &self.holders.len())
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

impl Chunks {
    /// The chunk runtime of a world.
    pub fn new(info: WorldInfo, settings: ChunkSettings, slots: ChunkSlots) -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        let wake = Arc::<WakeState>::default();
        // Store results wake the owner like job results, batched by the same pending flag.
        let inbox_wake = Arc::clone(&wake);
        let (inbox_tx, inbox) = CompletionTarget::with_waker(Arc::new(move || inbox_wake.wake()));
        Chunks {
            info,
            settings: ChunkSettings {
                view_distance: settings.view_distance.clamp(2, 32),
                simulation_distance: settings.simulation_distance.clamp(2, 32),
                ..settings
            },
            slots,
            holders: Holders::new(),
            levels: TicketLevels::with_max(MAX_TICKET_LEVEL),
            tickets: FxHashMap::default(),
            generation: TicketLevels::default(),
            generation_tickets: FxHashMap::default(),
            next_ticket: 0,
            level_changes: Vec::new(),
            check: BTreeSet::new(),
            focus: Vec::new(),
            waiters: FxHashMap::default(),
            in_flight: 0,
            done_tx: DoneSender { tx: done_tx, wake },
            done_rx: Mutex::new(done_rx),
            inbox_tx,
            inbox: Mutex::new(inbox),
            next_token: 0,
            loads: FxHashMap::default(),
            encoding: FxHashMap::default(),
            encode_jobs: 0,
            round_jobs: 0,
            changed: Vec::new(),
            round: None,
            commits: 0,
            commit_waiters: Vec::new(),
            committing: false,
            uncommitted: false,
            last_commit: 0,
            between_ticks: false,
            loaded_between_ticks: Vec::new(),
            due_next_tick: FxHashSet::default(),
            unload_candidates: FxHashSet::default(),
            relight: FxHashMap::default(),
            relighting: false,
            light_changes: FxHashMap::default(),
            block_changes: Vec::new(),
            tick: 0,
            simulation: SimulationLevels::default(),
            simulation_tickets: FxHashMap::default(),
            block_ticks: LevelTicks::default(),
            fluid_ticks: LevelTicks::default(),
            game_time: 0,
            sub_tick: 0,
            post_process: Vec::new(),
            generated: Vec::new(),
            promoted: Vec::new(),
            generated_full: Vec::new(),
            no_save: false,
            visibility: FxHashMap::default(),
            visibility_check: Vec::new(),
            visibility_changes: Vec::new(),
            unknown: FxHashMap::default(),
            chunk_cache: ChunkCache::default(),
            sync_loads: SyncLoads::default(),
        }
    }

    /// Sets the waker that asks the owner to call [`Chunks::pump`] between ticks (once; later
    /// calls are ignored). Without a waker, finished jobs are applied in [`Chunks::tick`].
    pub fn set_waker(&mut self, waker: Waker) {
        let _ = self.done_tx.wake.waker.set(waker);
    }

    /// Applies finished jobs and starts the next steps between ticks. Chunks only make progress
    /// towards full here: steps to [`ChunkStatus::Spawn`] and [`ChunkStatus::Full`] and loaded
    /// full chunks wait for the next [`Chunks::tick`], so the live world only changes in ticks
    /// (light of full chunks excepted, it is sent with the next tick's light changes).
    pub fn pump(&mut self) {
        self.done_tx.wake.pending.store(false, Ordering::Release);
        self.between_ticks = true;
        for _ in 0..1024 {
            let progress = self.drain_done() | self.dispatch();
            if !progress {
                break;
            }
        }
        self.between_ticks = false;
    }

    /// What the slots know about this world.
    pub fn info(&self) -> &WorldInfo {
        &self.info
    }

    /// Chunks whose light is recomputed per tick (config reload).
    pub fn set_max_relights(&mut self, max: usize) {
        self.settings.max_relights = max;
    }

    /// The settings.
    pub fn settings(&self) -> &ChunkSettings {
        &self.settings
    }

    /// Number of chunks in the map (all statuses).
    pub fn loaded(&self) -> usize {
        self.holders.len()
    }

    /// Number of full chunks (`/vanillite status`).
    pub fn full(&self) -> usize {
        self.holders
            .values()
            .filter(|h| {
                h.chunk
                    .as_ref()
                    .is_some_and(|c| c.status == ChunkStatus::Full)
            })
            .count()
    }

    /// Heap bytes of the loaded chunks' block, light and heightmap data (`/vanillite
    /// memory`); walks every chunk, only on demand.
    pub fn memory_bytes(&self) -> usize {
        self.holders
            .values()
            .map(|h| {
                size_of::<Holder>()
                    + h.chunk
                        .as_ref()
                        .map_or(0, |c| size_of::<ProtoChunk>() + c.chunk.heap_bytes())
            })
            .sum()
    }

    // --- Tickets -------------------------------------------------------------------------

    /// Adds a ticket: chunks within `33 - level` of `pos` become full, one ring further at
    /// least lit; what is missing is generated (forced chunks: level 31, Vanilla's `ChunkLevel.byStatus(ENTITY_TICKING)`).
    pub fn add_ticket(&mut self, pos: ChunkPos, level: u8) -> TicketId {
        let id = self.insert_ticket(pos, level);
        if self.settings.blocking_tickets && level <= FULL_LEVEL {
            let radius = i32::from(FULL_LEVEL - level);
            self.run_blocking(|c| {
                (-radius..=radius).all(|dx| {
                    (-radius..=radius).all(|dz| c.is_full_at(ChunkPos::new(pos.x + dx, pos.z + dz)))
                })
            });
        }
        id
    }

    /// [`Chunks::add_ticket`] that never waits for its chunks, also with
    /// [`ChunkSettings::blocking_tickets`]: a ticket Vanilla adds without a synchronous
    /// `getChunk` (the dragon fight's), whose chunks load with the next run of the chunk
    /// system.
    pub fn add_ticket_deferred(&mut self, pos: ChunkPos, level: u8) -> TicketId {
        self.insert_ticket(pos, level)
    }

    fn insert_ticket(&mut self, pos: ChunkPos, level: u8) -> TicketId {
        let id = TicketId(self.next_ticket);
        self.next_ticket += 1;
        self.tickets.insert(id, (pos, level));
        self.levels.add(pos, level, &mut self.level_changes);
        id
    }

    /// Removes a ticket.
    pub fn remove_ticket(&mut self, id: TicketId) {
        if let Some((pos, level)) = self.tickets.remove(&id) {
            self.levels.remove(pos, level, &mut self.level_changes);
        }
    }

    /// Moves a ticket to `pos` (a player changing chunk): only the chunks whose level changes
    /// are touched.
    pub fn move_ticket(&mut self, id: TicketId, pos: ChunkPos) {
        if let Some((from, level)) = self.tickets.get_mut(&id) {
            self.levels
                .shift(*from, pos, *level, &mut self.level_changes);
            *from = pos;
        }
    }

    /// The ticket level of a player's load ticket at its centre chunk: Vanilla's
    /// `PlayerTicketTracker` puts a `PLAYER_LOADING` ticket at entity-ticking level (31) on
    /// every chunk within the view distance, so chunks one further block tick (and are spawn
    /// candidates) and two further are full.
    pub fn player_ticket_level(&self) -> u8 {
        ENTITY_TICKING_LEVEL.saturating_sub(self.settings.view_distance as u8)
    }

    /// The ticket level of a chunk (above [`MAX_TICKET_LEVEL`]: no ticket reaches it).
    pub fn level(&self, pos: ChunkPos) -> u8 {
        self.levels.level(pos)
    }

    /// Adds a simulation ticket: chunks within `31 - level` of `pos` tick entities and random
    /// ticks, one chunk further block ticks.
    pub fn add_simulation_ticket(&mut self, pos: ChunkPos, level: u8) -> TicketId {
        let id = TicketId(self.next_ticket);
        self.next_ticket += 1;
        self.simulation_tickets.insert(id, (pos, level));
        self.simulation.add_ticket(pos, level);
        self.simulation.run_updates();
        id
    }

    /// Removes a simulation ticket.
    pub fn remove_simulation_ticket(&mut self, id: TicketId) {
        if let Some((pos, level)) = self.simulation_tickets.remove(&id) {
            self.simulation.remove_ticket(pos, level);
            self.simulation.run_updates();
        }
    }

    /// Moves a simulation ticket to `pos`: like a player changing chunks in Vanilla
    /// (`ChunkMap.move`), the old ticket goes, the new one comes, then the levels update.
    pub fn move_simulation_ticket(&mut self, id: TicketId, pos: ChunkPos) {
        if let Some((from, level)) = self.simulation_tickets.get_mut(&id) {
            if *from == pos {
                return;
            }
            self.simulation.remove_ticket(*from, *level);
            self.simulation.add_ticket(pos, *level);
            self.simulation.run_updates();
            *from = pos;
        }
    }

    /// The simulation ticket level that lets a player's surroundings tick
    /// (`DistanceManager.getPlayerTicketLevel`).
    pub fn player_simulation_level(&self) -> u8 {
        (ENTITY_TICKING_LEVEL as i32 - self.settings.simulation_distance).max(0) as u8
    }

    /// Whether a chunk is full and loaded to block ticking (Vanilla's ticking chunk: its
    /// ticking future completes once the 3 × 3 chunks around it are full).
    fn is_ticking_chunk(&self, pos: ChunkPos) -> bool {
        self.holders
            .get(&pos)
            .is_some_and(|h| h.level <= BLOCK_TICKING_LEVEL && h.is_full())
            && neighbours_full(&self.holders, pos)
    }

    /// Whether scheduled ticks run in a chunk (`isPositionTickingWithEntitiesLoaded`).
    pub fn is_block_ticking(&self, pos: ChunkPos) -> bool {
        tick_check(&self.holders, &self.simulation, pos)
    }

    /// The ticket level of a chunk's holder (`ChunkHolder.getTicketLevel`), `None` without a
    /// holder. `LevelChunk.getFullStatus` follows it.
    pub fn holder_level(&self, pos: ChunkPos) -> Option<u8> {
        self.holders.get(&pos).map(|h| h.level)
    }

    /// Whether the stored entities of a chunk are live (`areEntitiesLoaded`).
    pub fn entities_loaded(&self, pos: ChunkPos) -> bool {
        self.holders.get(&pos).is_some_and(|h| h.entities_loaded)
    }

    /// Marks the stored entities of a loaded chunk live or not (`ChunkLoadStatus`).
    pub fn set_entities_loaded(&mut self, pos: ChunkPos, loaded: bool) {
        if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
            holder.entities_loaded = loaded;
        }
    }

    /// The chunks that get random ticks this tick (`forEachBlockTickingChunk`: entity ticking
    /// by simulation level and a ticking chunk), in Vanilla's order.
    pub fn random_ticking_chunks(&self, out: &mut Vec<ChunkPos>) {
        out.extend(
            self.simulation
                .iter()
                .filter(|&(pos, level)| level <= ENTITY_TICKING_LEVEL && self.is_ticking_chunk(pos))
                .map(|(pos, _)| pos),
        );
    }

    /// The chunks [`Self::random_ticking_chunks`] chooses from, by simulation level only;
    /// [`Self::is_random_ticking`] is the rest of its check. For phases that check the chunks
    /// on several threads (tile region mode).
    pub fn random_ticking_candidates(&self, out: &mut Vec<ChunkPos>) {
        out.extend(
            self.simulation
                .iter()
                .filter(|&(_, level)| level <= ENTITY_TICKING_LEVEL)
                .map(|(pos, _)| pos),
        );
    }

    /// Whether a chunk of [`Self::random_ticking_candidates`] ticks randomly.
    pub fn is_random_ticking(&self, pos: ChunkPos) -> bool {
        self.is_ticking_chunk(pos)
    }

    fn apply_level_changes(&mut self) {
        if !self.level_changes.is_empty() {
            // `runDistanceManagerUpdates` clears the chunk cache after level changes.
            self.chunk_cache.clear();
        }
        // Moving a generation ticket reports more changes; apply them too.
        while !self.level_changes.is_empty() {
            let mut changes = std::mem::take(&mut self.level_changes);
            for change in &changes {
                self.apply_level_change(change.pos);
            }
            changes.clear();
            if self.level_changes.is_empty() {
                self.level_changes = changes;
            }
        }
    }

    /// Brings a chunk's holder to its current level (the lower of ticket and generation level).
    fn apply_level_change(&mut self, pos: ChunkPos) {
        let ticket = self.levels.level(pos);
        if self.generation_tickets.contains_key(&pos) {
            // The pyramid of a chunk being generated follows its ticket level.
            self.set_generation_ticket(pos, generation_ticket_level(ticket));
        }
        let new = ticket.min(self.generation.level(pos));
        let old = self
            .holders
            .get(&pos)
            .map_or(levels::UNLOADED, |holder| holder.level);
        if old == new {
            return;
        }
        if visibility_of_level(old) != visibility_of_level(new) {
            self.visibility_check.push(pos);
        }
        if new <= MAX_LEVEL {
            // Chunks only the generation pyramid reaches are added once a step needs them.
            if ticket > MAX_TICKET_LEVEL && !self.holders.contains_key(&pos) {
                return;
            }
            let visibility = &self.visibility;
            let holder = self.holders.map_mut().entry(pos).or_insert_with(|| Holder {
                accessible: visibility.contains_key(&pos),
                ..Holder::default()
            });
            if holder.queued {
                let job = !holder.queued_inline;
                let reach = holder.queued_reach;
                let (level, rank) = (holder.level, holder.rank);
                self.check
                    .remove(&(job, urgency(level, reach), level, rank, pos));
                self.check
                    .insert((job, urgency(new, reach), new, rank, pos));
            }
            if let Some(unneeded) = &holder.unneeded {
                unneeded.store(false, Ordering::Relaxed);
            }
            holder.level = new;
            holder.unload_at = None;
            self.unload_candidates.remove(&pos);
            // Only a new required status gives the chunk work: a full chunk moving
            // closer to a player (level 25 to 24) needs no check.
            if new < old && required_status(new) != required_status(old) {
                self.enqueue(pos);
            }
        } else if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
            if holder.queued {
                let job = !holder.queued_inline;
                let reach = holder.queued_reach;
                let (level, rank) = (holder.level, holder.rank);
                self.check
                    .remove(&(job, urgency(level, reach), level, rank, pos));
                holder.queued = false;
            }
            // A generation step nobody needs any more is skipped if it did not start.
            if let Some(unneeded) = &holder.unneeded {
                unneeded.store(true, Ordering::Relaxed);
            }
            holder.level = new;
            // At least one tick later: the game writes back what the chunk holds first.
            holder.unload_at = Some(self.tick + self.settings.unload_delay.max(1));
            self.unload_candidates.insert(pos);
            // A queued load nobody needs any more is dropped before IO and decoding.
            if holder.chunk.is_none()
                && let Some(token) = holder.loading
            {
                self.slots.store.cancel(token);
            }
        }
    }

    /// Places, moves or (`None`) removes the generation ticket of a chunk.
    fn set_generation_ticket(&mut self, pos: ChunkPos, level: Option<u8>) {
        let current = self.generation_tickets.get(&pos).copied();
        if current == level {
            return;
        }
        if let Some(current) = current {
            self.generation
                .remove(pos, current, &mut self.level_changes);
            self.generation_tickets.remove(&pos);
        }
        if let Some(level) = level {
            self.generation.add(pos, level, &mut self.level_changes);
            self.generation_tickets.insert(pos, level);
        }
    }

    fn enqueue(&mut self, pos: ChunkPos) {
        let rank = self.rank(pos);
        if let Some(holder) = self.holders.map_mut().get_mut(&pos)
            && !holder.queued
            && holder.level <= MAX_LEVEL
        {
            holder.queued = true;
            holder.rank = rank;
            holder.queued_inline = holder.inline_next();
            holder.queued_reach = holder.next_reach();
            self.check.insert((
                !holder.queued_inline,
                urgency(holder.level, holder.queued_reach),
                holder.level,
                rank,
                pos,
            ));
        }
    }

    /// The order of a chunk within its level: its chessboard distance to the nearest focus
    /// point, 0 without any.
    fn rank(&self, pos: ChunkPos) -> u8 {
        self.focus
            .iter()
            .map(|focus| pos.chessboard_distance(*focus))
            .min()
            .map_or(0, |distance| u8::try_from(distance).unwrap_or(u8::MAX))
    }

    /// Sets the points the players' views move towards (a flying player's view centre ahead in
    /// its direction; at most [`MAX_FOCUS`]). Within one urgency, chunks nearer to a focus point
    /// are loaded and generated first, so a moving view fills ahead first. Only the order of
    /// the work changes, never its result.
    pub fn set_focus(&mut self, focus: &[ChunkPos]) {
        self.focus.clear();
        self.focus.extend(focus.iter().take(MAX_FOCUS).copied());
    }

    /// Re-checks the chunks parked on `pos`.
    fn wake(&mut self, pos: ChunkPos) {
        if let Some(waiters) = self.waiters.remove(&pos) {
            for waiter in waiters {
                self.enqueue(waiter);
            }
        }
        self.enqueue(pos);
    }

    // --- Synchronous loads ---------------------------------------------------------------

    /// Whether a chunk is accessible as a full chunk (ticket level up to full).
    #[inline]
    fn is_full_at(&self, pos: ChunkPos) -> bool {
        self.holders
            .get(&pos)
            .is_some_and(|h| h.level <= FULL_LEVEL && h.status() == Some(ChunkStatus::Full))
    }

    /// A read of an accessible chunk on the world thread (`ServerChunkCache.getChunk`): unless
    /// the chunk cache holds it, it goes into the cache and `reset` restarts its `unknown`
    /// ticket. Only tracked while `unknown` tickets exist.
    #[inline]
    fn note_read(&self, pos: ChunkPos, reset: bool) {
        if !self.unknown.is_empty() {
            self.note_tracked_read(pos, reset);
        }
    }

    /// [`Self::note_read`] while `unknown` tickets exist. Out of line: block reads are hot and
    /// almost never tracked.
    #[cold]
    #[inline(never)]
    fn note_tracked_read(&self, pos: ChunkPos, reset: bool) {
        if self.chunk_cache.note(pos) {
            return;
        }
        if reset && let Some((_, left)) = self.unknown.get(&pos) {
            left.store(1, Ordering::Relaxed);
        }
    }

    /// Whether reads through `&self` are pure right now: no `unknown` ticket of a synchronous
    /// load exists, so [`Chunks::block`] touches neither the chunk cache nor a ticket. Phases
    /// that share the chunks between threads (tile region mode) only run then.
    pub fn reads_are_pure(&self) -> bool {
        self.unknown.is_empty()
    }

    /// Whether a chunk is accessible as a full chunk, without touching the chunk cache.
    pub fn is_full(&self, pos: ChunkPos) -> bool {
        self.is_full_at(pos)
    }

    /// `ServerChunkCache.getChunkNow(x, z) != null`: whether the chunk is accessible, which
    /// puts it into the chunk cache without touching tickets.
    pub fn chunk_now(&self, pos: ChunkPos) -> bool {
        let accessible = self.is_full_at(pos);
        if accessible {
            self.note_read(pos, false);
        }
        accessible
    }

    /// `ServerChunkCache.getChunk(x, z, FULL, true)` on the world thread, as `getBlockState`
    /// and `setBlock` call it: a chunk that is not accessible gets an `unknown` ticket at
    /// level 33 (timeout 1 tick, [`Chunks::purge_stale_tickets`]) and is loaded or generated
    /// while the world thread waits (`managedBlock`). Returns whether it had to wait; the
    /// chunks that became live are in [`Chunks::take_promoted`].
    ///
    /// Every load that has to wait blocks the tick; it is counted in
    /// [`Chunks::sync_loads`] (see `docs/architecture/world-access.md` for where it is
    /// allowed).
    #[inline]
    pub fn load_now(&mut self, pos: ChunkPos) -> bool {
        if self.is_full_at(pos) {
            self.note_read(pos, true);
            return false;
        }
        let started = Instant::now();
        self.load_blocking(pos);
        let waited = started.elapsed();
        self.sync_loads.count += 1;
        self.sync_loads.waited += waited;
        log::debug!(
            "Synchronous load of chunk {}, {} in {} blocked the tick for {:.1} ms",
            pos.x,
            pos.z,
            self.info.dimension,
            waited.as_secs_f64() * 1000.0
        );
        true
    }

    /// The synchronous loads that blocked the tick since the world started.
    pub fn sync_loads(&self) -> SyncLoads {
        self.sync_loads
    }

    #[cold]
    fn load_blocking(&mut self, pos: ChunkPos) {
        match self.unknown.get(&pos) {
            Some((_, left)) => left.store(1, Ordering::Relaxed),
            None => {
                let id = self.insert_ticket(pos, FULL_LEVEL);
                self.unknown.insert(pos, (id, AtomicI64::new(1)));
            }
        }
        if !self.run_blocking(|c| c.is_full_at(pos)) {
            log::error!(
                "Couldn't load chunk {}, {} in {} synchronously",
                pos.x,
                pos.z,
                self.info.dimension
            );
        }
        self.note_read(pos, false);
    }

    /// Runs the chunk runtime on this thread until `done` (false if nothing can make progress
    /// any more), then applies the visibility changes and clears the chunk cache: the tickets
    /// changed levels.
    #[cold]
    fn run_blocking(&mut self, done: impl Fn(&Self) -> bool) -> bool {
        // What waits for the next tick is wanted now.
        for (at, chunk, entities) in std::mem::take(&mut self.loaded_between_ticks) {
            self.apply_loaded(at, Ok(Some(chunk)), entities);
        }
        for at in std::mem::take(&mut self.due_next_tick) {
            self.enqueue(at);
        }
        self.slots.executor.blocked(true);
        let done = loop {
            let mut progress = self.drain_done();
            self.apply_level_changes();
            if done(self) {
                break true;
            }
            progress |= self.dispatch();
            if progress {
                continue;
            }
            if self.in_flight == 0 && self.check.is_empty() {
                break false;
            }
            match self
                .done_rx
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(Duration::from_millis(1))
            {
                Ok(job) => self.apply_done(job),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break false,
            }
        };
        self.slots.executor.blocked(false);
        self.check_visibility();
        // The tickets changed levels, which cleared the cache.
        self.chunk_cache.clear();
        done
    }

    /// `TicketStorage.purgeStaleTickets` for the `unknown` tickets of synchronous loads: each
    /// counts down and goes once below zero, so a ticket lives through two purges. Runs once
    /// per world tick at the start of the chunk source tick while the game runs normally.
    pub fn purge_stale_tickets(&mut self) {
        if self.unknown.is_empty() {
            return;
        }
        let mut expired = Vec::new();
        self.unknown.retain(|_, (id, left)| {
            *left.get_mut() -= 1;
            if *left.get_mut() < 0 {
                expired.push(*id);
                false
            } else {
                true
            }
        });
        for id in expired {
            self.remove_ticket(id);
        }
    }

    // --- Tick ----------------------------------------------------------------------------

    /// Applies finished jobs and ticket changes, starts new jobs and unloads unneeded chunks.
    /// With an inline executor this runs until nothing can make progress.
    pub fn tick(&mut self) {
        self.tick += 1;
        self.generated_full.clear();
        for (pos, chunk, entities) in std::mem::take(&mut self.loaded_between_ticks) {
            self.apply_loaded(pos, Ok(Some(chunk)), entities);
        }
        for pos in std::mem::take(&mut self.due_next_tick) {
            self.enqueue(pos);
        }
        for _ in 0..1024 {
            let mut progress = self.drain_done();
            self.apply_level_changes();
            progress |= self.dispatch();
            if !progress {
                break;
            }
        }
        // What changed since the last tick ended (tasks between ticks).
        self.start_relights();
        self.unload();
        self.check_visibility();
        // `ServerChunkCache.tick` ends with `clearCache`.
        self.chunk_cache.clear();
    }

    /// The visibility of a chunk (`ChunkMap.onFullChunkStatusChange`): hidden unless it is
    /// full with a level up to full.
    pub fn visibility(&self, pos: ChunkPos) -> Visibility {
        self.visibility
            .get(&pos)
            .copied()
            .unwrap_or(Visibility::Hidden)
    }

    fn current_visibility(&self, pos: ChunkPos) -> Visibility {
        self.holders
            .get(&pos)
            .filter(|h| h.status() == Some(ChunkStatus::Full))
            .map_or(Visibility::Hidden, |h| visibility_of_level(h.level))
    }

    /// Records the visibility changes of the chunks touched this tick, in touch order.
    fn check_visibility(&mut self) {
        let check = std::mem::take(&mut self.visibility_check);
        for &pos in &check {
            let new = self.current_visibility(pos);
            let old = if new == Visibility::Hidden {
                self.visibility.remove(&pos)
            } else {
                self.visibility.insert(pos, new)
            }
            .unwrap_or(Visibility::Hidden);
            if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                holder.accessible = new != Visibility::Hidden;
            }
            if old != new {
                self.visibility_changes
                    .push(VisibilityChange { pos, old, new });
            }
        }
        self.visibility_check = check;
        self.visibility_check.clear();
    }

    /// Visibility changes since the last call, in order
    /// (`PersistentEntitySectionManager.updateChunkStatus`). A chunk becomes hidden before it
    /// unloads, at least one [`Chunks::tick`] earlier.
    pub fn take_visibility_changes(&mut self, out: &mut Vec<VisibilityChange>) {
        out.append(&mut self.visibility_changes);
    }

    /// Whether no job, load, save or unload is pending: the chunks are as their tickets say.
    pub fn is_idle(&self) -> bool {
        self.in_flight == 0
            && self.encode_jobs == 0
            && self.check.is_empty()
            && self.unload_candidates.is_empty()
            && self.visibility_changes.is_empty()
    }

    /// The chunks whose ticket level asks for a full chunk that is not full yet
    /// (`ChunkLoadCounter.pendingChunks`); ticket changes count from the next [`Chunks::tick`].
    pub fn pending_full(&self) -> usize {
        self.holders
            .values()
            .filter(|h| h.level <= FULL_LEVEL && h.status() != Some(ChunkStatus::Full))
            .count()
    }

    /// Whether a chunk is accessible as a full chunk (`hasChunk`: level up to full).
    pub fn is_accessible(&self, pos: ChunkPos) -> bool {
        self.visibility.contains_key(&pos)
    }

    /// `DistanceManager.inEntityTickingRange`: entities in this chunk tick by simulation
    /// distance.
    pub fn in_entity_ticking_range(&self, pos: ChunkPos) -> bool {
        self.simulation.level(pos) <= ENTITY_TICKING_LEVEL
    }

    fn drain_done(&mut self) -> bool {
        let mut any = false;
        while let Ok(done) = self
            .done_rx
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .try_recv()
        {
            any = true;
            match done {
                Done::Loaded(pos, Ok(Some(chunk)), entities)
                    if self.between_ticks
                        && chunk.status == ChunkStatus::Full
                        && chunk.light_on =>
                {
                    // The job is done; the chunk stays busy until it goes live.
                    self.in_flight -= 1;
                    self.loaded_between_ticks.push((pos, chunk, entities));
                }
                done => self.apply_done(done),
            }
        }
        while let Ok(completion) = self
            .inbox
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .try_recv()
        {
            any = true;
            self.apply_completion(completion);
        }
        any
    }

    /// Installs a result of the store.
    fn apply_completion(&mut self, completion: Completion) {
        match completion {
            Completion::Op {
                token,
                index,
                result,
            } => {
                let Some(load) = self.loads.get_mut(&token) else {
                    // A save.
                    if let StoreResult::Failed(e) = result {
                        log::error!("Couldn't save a chunk in {}: {e}", self.info.dimension);
                    }
                    return;
                };
                match index {
                    0 => load.chunk = Some(result),
                    1 => load.entities = Some(result),
                    _ => load.poi = Some(result),
                }
                if load.chunk.is_none() || load.entities.is_none() || load.poi.is_none() {
                    return;
                }
                let Some(PendingLoad {
                    pos,
                    chunk: Some(chunk),
                    entities: Some(entities),
                    poi: Some(poi),
                }) = self.loads.remove(&token)
                else {
                    return;
                };
                if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                    holder.loading = None;
                }
                if matches!(chunk, StoreResult::Cancelled)
                    || matches!(entities, StoreResult::Cancelled)
                    || matches!(poi, StoreResult::Cancelled)
                {
                    self.in_flight -= 1;
                    if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                        holder.busy = false;
                    }
                    self.wake(pos);
                    return;
                }
                let needs_decode = matches!(chunk, StoreResult::Record(..))
                    || matches!(entities, StoreResult::Record(..))
                    || matches!(poi, StoreResult::Record(..));
                if !needs_decode {
                    let chunk = load_result(chunk).map(|_| None);
                    let entities = load_result(entities).map(|_| None);
                    let poi = load_result(poi).map(|_| None);
                    self.apply_done(Done::Loaded(pos, chunk, (entities, poi)));
                    return;
                }
                let codec = Arc::clone(&self.slots.codec);
                let tx = self.done_tx.clone();
                let priority =
                    self.holders
                        .get(&pos)
                        .map_or(job_priority(MAX_LEVEL, MAX_LEVEL, 0), |h| {
                            let reach = ChunkStatus::Empty.reach();
                            job_priority(urgency(h.level, reach), h.level, h.rank)
                        });
                self.slots.executor.spawn(
                    priority,
                    Box::new(move || {
                        let chunk = load_result(chunk).and_then(|record| {
                            record
                                .map(|(meta, bytes)| codec.decode(pos, &meta, &bytes).map(Box::new))
                                .transpose()
                        });
                        let entities = load_result(entities).and_then(|record| {
                            record
                                .map(|(meta, bytes)| codec.decode_entities(pos, &meta, &bytes))
                                .transpose()
                        });
                        let poi = load_result(poi).and_then(|record| {
                            Ok(record
                                .map(|(meta, bytes)| codec.decode_poi(pos, &meta, &bytes))
                                .transpose()?
                                .flatten())
                        });
                        tx.send(Done::Loaded(pos, chunk, (entities, poi)));
                    }),
                );
            }
            Completion::Commit { round, result } => {
                self.committing = false;
                if let Err(e) = result {
                    log::error!(
                        "Couldn't commit saved chunks of {}: {e}",
                        self.info.dimension
                    );
                }
                let mut index = 0;
                while index < self.commit_waiters.len() {
                    if self.commit_waiters[index].0 <= round {
                        let (_, then) = self.commit_waiters.swap_remove(index);
                        then();
                    } else {
                        index += 1;
                    }
                }
            }
        }
    }

    /// A finished load: the chunk (or a new one) goes into its holder with its stored entities.
    fn apply_loaded(
        &mut self,
        pos: ChunkPos,
        result: io::Result<Option<Box<ProtoChunk>>>,
        (entities, poi): LoadedRecords,
    ) {
        let mut chunk = match result {
            Ok(Some(chunk)) => *chunk,
            Ok(None) => self.slots.generator.new_chunk(pos),
            Err(e) => {
                log::error!(
                    "Couldn't load chunk {}, {} in {}: {e}; generating it again",
                    pos.x,
                    pos.z,
                    self.info.dimension
                );
                self.slots.generator.new_chunk(pos)
            }
        };
        let entities_stored = match entities {
            Ok(Some(entities)) => {
                chunk.entities = entities;
                true
            }
            Ok(None) => false,
            Err(e) => {
                log::error!(
                    "Couldn't load the entities of chunk {}, {} in {}: {e}",
                    pos.x,
                    pos.z,
                    self.info.dimension
                );
                false
            }
        };
        match poi {
            Ok(poi) => chunk.poi = poi,
            Err(e) => log::error!(
                "Couldn't load the POI of chunk {}, {} in {}: {e}",
                pos.x,
                pos.z,
                self.info.dimension
            ),
        }
        let mut dirty = chunk.status == ChunkStatus::Empty;
        // Stored light that is not trusted is computed again (Vanilla: `isLightOn`).
        if chunk.status > ChunkStatus::Features && !chunk.light_on {
            chunk.status = ChunkStatus::Features;
            dirty = true;
        }
        let lit = chunk.status >= ChunkStatus::InitializeLight;
        if lit && self.info.has_sky_light {
            chunk.chunk.light_mut().reset_sky_sources();
        }
        if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
            holder.chunk = Some(Arc::new(chunk));
            holder.sync_full();
            holder.busy = false;
            holder.entities_stored = entities_stored;
            if dirty {
                holder.touch(pos, &mut self.changed);
            }
            if lit {
                self.restore_kept_light(pos);
            }
            self.promote(pos);
            self.wake(pos);
        }
    }

    fn apply_done(&mut self, done: Done) {
        match done {
            Done::Loaded(pos, result, entities) => {
                self.in_flight -= 1;
                self.apply_loaded(pos, result, entities);
            }
            Done::Stepped {
                status,
                targets,
                written,
            } => {
                self.in_flight -= 1;
                for mut target in targets {
                    target.status = status;
                    let pos = target.pos;
                    self.put_back(pos, Arc::new(target));
                    if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                        holder.busy = false;
                        holder.unneeded = None;
                    }
                    self.wake(pos);
                }
                for chunk in written {
                    let pos = chunk.pos;
                    self.put_back(pos, chunk);
                    if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                        holder.busy = false;
                    }
                    self.wake(pos);
                }
            }
            Done::Lit {
                targets,
                neighbours,
            } => {
                self.in_flight -= 1;
                for target in targets {
                    let pos = target.pos;
                    if let Some(holder) = self.holders.map_mut().get_mut(&pos)
                        && let Some(current) = holder.chunk.as_mut()
                    {
                        let current = Arc::make_mut(current);
                        current.chunk.light_mut().merge_max(target.chunk.light());
                        current.light_on = true;
                        current.status = ChunkStatus::Light;
                        holder.sync_full();
                        holder.busy = false;
                        holder.touch(pos, &mut self.changed);
                    }
                    self.wake(pos);
                }
                for neighbour in neighbours {
                    self.merge_light(&neighbour);
                }
            }
            Done::Skipped { target, written } => {
                self.in_flight -= 1;
                for pos in std::iter::once(target).chain(written) {
                    if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                        holder.busy = false;
                        if pos == target {
                            holder.unneeded = None;
                        }
                    }
                    self.wake(pos);
                }
            }
            Done::Relit {
                changes,
                read,
                light,
            } => {
                self.relighting = false;
                self.apply_relit(changes, &read, light);
                self.start_relights();
            }
            Done::Encoded {
                round,
                chunks,
                errors,
            } => {
                self.encode_jobs -= 1;
                self.round_jobs -= usize::from(round);
                self.uncommitted = true;
                for (pos, e) in errors {
                    log::error!(
                        "Couldn't save chunk {}, {} in {}: {e}",
                        pos.x,
                        pos.z,
                        self.info.dimension
                    );
                }
                for Snapshot { chunk, .. } in chunks {
                    if self
                        .encoding
                        .get(&chunk.pos)
                        .is_some_and(|encoding| Arc::ptr_eq(encoding, &chunk))
                    {
                        self.encoding.remove(&chunk.pos);
                    }
                }
            }
        }
    }

    /// Puts a chunk a job wrote back into its holder.
    fn put_back(&mut self, pos: ChunkPos, chunk: Arc<ProtoChunk>) {
        if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
            holder.chunk = Some(chunk);
            holder.sync_full();
            holder.touch(pos, &mut self.changed);
        }
    }

    /// Applies a light update, unless the light of a chunk it writes changed since the update
    /// read it (initial light of a neighbour arrived): then its changes are queued again.
    fn apply_relit(
        &mut self,
        changes: Vec<(ChunkPos, Vec<BlockPos>)>,
        read: &[Arc<ProtoChunk>],
        light: Vec<(ChunkPos, ChunkLight)>,
    ) {
        let current = |holders: &FxHashMap<ChunkPos, Holder>, pos: ChunkPos| {
            holders
                .get(&pos)
                .filter(|holder| !holder.busy)
                .and_then(|holder| holder.chunk.clone())
        };
        let unchanged = light.iter().all(|(pos, _)| {
            let before = read.iter().find(|chunk| chunk.pos == *pos);
            match (current(&self.holders, *pos), before) {
                (Some(now), Some(before)) => now.chunk.light() == before.chunk.light(),
                // Unloaded meanwhile: nothing to write.
                (None, _) => !self.holders.contains_key(pos),
                (Some(_), None) => false,
            }
        });
        if !unchanged {
            for (pos, blocks) in changes {
                self.relight.entry(pos).or_default().extend(blocks);
            }
            return;
        }
        for (pos, light) in light {
            let Some(holder) = self.holders.map_mut().get_mut(&pos) else {
                continue;
            };
            let Some(current) = holder.chunk.as_mut() else {
                continue;
            };
            let change = LightChange::between(current.chunk.light(), &light);
            if change.is_empty() {
                // Only the sky light sources' state changed: nothing to save or send.
                if *current.chunk.light() != light {
                    *Arc::make_mut(current).chunk.light_mut() = light;
                }
                continue;
            }
            let current = Arc::make_mut(current);
            *current.chunk.light_mut() = light;
            let full = current.status == ChunkStatus::Full;
            holder.touch(pos, &mut self.changed);
            if full {
                self.light_changes.entry(pos).or_default().add(&change);
            }
        }
    }

    fn merge_light(&mut self, from: &ProtoChunk) {
        let pos = from.pos;
        let Some(holder) = self.holders.map_mut().get_mut(&pos) else {
            return;
        };
        let Some(current) = holder.chunk.as_mut() else {
            return;
        };
        Arc::make_mut(current)
            .chunk
            .light_mut()
            .merge_max(from.chunk.light());
        let full = current.status == ChunkStatus::Full;
        holder.touch(pos, &mut self.changed);
        if full {
            self.light_changes.insert(pos, LightChange::ALL);
        }
    }

    fn dispatch(&mut self) -> bool {
        let mut progress = false;
        while let Some(&(job, urgency, level, rank, pos)) = self.check.first() {
            // Spawn and full steps (first in the queue) need no job slot: they must not wait
            // until the generation of the whole area is through, or every chunk of a new area
            // would become full in the same tick.
            if job && self.in_flight >= self.settings.max_jobs {
                break;
            }
            self.check.remove(&(job, urgency, level, rank, pos));
            if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                holder.queued = false;
            }
            match self.check_chunk(pos) {
                Check::Done => {}
                Check::Progress => progress = true,
                Check::Blocked(on) => self.waiters.entry(on).or_default().push(pos),
                Check::Full => {
                    self.enqueue(pos);
                    break;
                }
            }
        }
        progress
    }

    /// Advances a chunk by one step if it can.
    fn check_chunk(&mut self, pos: ChunkPos) -> Check {
        let Some(holder) = self.holders.get(&pos) else {
            return Check::Done;
        };
        let Some(required) = required_status(holder.level) else {
            return Check::Done;
        };
        if holder.busy {
            return Check::Blocked(pos);
        }
        let Some(status) = holder.status() else {
            return self.start_load(pos);
        };
        let next = status.next().filter(|_| status < required);
        // The first check of a step adds every missing neighbour it needs, so they load
        // together; later checks stop at the first one not ready.
        let plan = holder.planned != next;
        let Some(next) = next else {
            self.set_generation_ticket(pos, None);
            return Check::Done;
        };
        if self.between_ticks && matches!(next, ChunkStatus::Spawn | ChunkStatus::Full) {
            self.due_next_tick.insert(pos);
            return Check::Done;
        }
        // Requirements by radius. A later check resumes at the neighbour the last one was
        // blocked on and wraps around, so every neighbour is still checked once before the
        // step runs, but a chunk woken by each of its neighbours in turn does not scan the
        // whole square every time.
        let radius = next.requirement_radius() as i32;
        let side = 2 * radius + 1;
        let cells = (side * side) as usize;
        let resume = if plan {
            0
        } else {
            usize::from(holder.scan_from).min(cells)
        };
        let mut needs = [None; 16];
        for (distance, need) in needs.iter_mut().enumerate().take(radius as usize + 1) {
            *need = next.direct_requirement_at(distance as u32);
        }
        let mut blocked = None;
        for i in (resume..cells).chain(0..resume) {
            let (dz, dx) = (i as i32 / side - radius, i as i32 % side - radius);
            if dx == 0 && dz == 0 {
                continue;
            }
            let distance = dx.unsigned_abs().max(dz.unsigned_abs());
            let Some(needed) = needs[distance as usize] else {
                continue;
            };
            let other = ChunkPos::new(pos.x + dx, pos.z + dz);
            let other_holder = self.holders.get(&other);
            if other_holder
                .and_then(Holder::status)
                .is_some_and(|status| status >= needed)
            {
                continue;
            }
            match other_holder {
                None => self.add_dependency(pos, other),
                // Kept only by a generation ticket that is gone: this chunk's own
                // ticket keeps it.
                Some(other) if required_status(other.level) < Some(needed) => {
                    self.add_generation_ticket(pos);
                }
                Some(_) => {}
            }
            if !plan {
                if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                    holder.scan_from = i as u16;
                }
                return Check::Blocked(other);
            }
            blocked.get_or_insert((i, other));
        }
        if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
            holder.planned = Some(next);
            holder.scan_from = blocked.map_or(0, |(i, _)| i as u16);
        }
        let blocked = blocked.map(|(_, other)| other);
        if let Some(other) = blocked {
            return Check::Blocked(other);
        }
        let write = next.write_radius() as i32;
        for dz in -write..=write {
            for dx in -write..=write {
                let other = ChunkPos::new(pos.x + dx, pos.z + dz);
                if self.holders.get(&other).is_some_and(|h| h.busy) {
                    return Check::Blocked(other);
                }
            }
        }
        self.run_step(pos, next, radius, write);
        Check::Progress
    }

    /// Gives a chunk with a ticket that is being generated its generation ticket: it keeps the
    /// chunks its steps need while no ticket reaches them. Chunks without a ticket are needed
    /// only within such a pyramid, which already reaches everything they need.
    fn add_generation_ticket(&mut self, pos: ChunkPos) {
        let ticket = self.levels.level(pos);
        if ticket <= MAX_TICKET_LEVEL {
            self.set_generation_ticket(pos, generation_ticket_level(ticket));
        }
    }

    /// Adds the holder of a chunk that a step of `of` needs but no ticket reaches.
    fn add_dependency(&mut self, of: ChunkPos, pos: ChunkPos) {
        self.add_generation_ticket(of);
        let level = self.levels.level(pos).min(self.generation.level(pos));
        if level > MAX_LEVEL {
            return;
        }
        self.holders.map_mut().insert(
            pos,
            Holder {
                level,
                accessible: self.visibility.contains_key(&pos),
                ..Holder::default()
            },
        );
        self.enqueue(pos);
    }

    fn start_load(&mut self, pos: ChunkPos) -> Check {
        if self.in_flight >= self.settings.max_jobs {
            return Check::Full;
        }
        let Some(holder) = self.holders.map_mut().get_mut(&pos) else {
            return Check::Done;
        };
        if let Some(encoding) = self.encoding.get(&pos) {
            // A save being encoded answers the load.
            holder.chunk = Some(Arc::clone(encoding));
            holder.sync_full();
            holder.entities_stored = true;
            holder.touch(pos, &mut self.changed);
            holder.touch_entities(pos, &mut self.changed);
            self.promote(pos);
            self.wake(pos);
            return Check::Progress;
        }
        holder.busy = true;
        self.in_flight += 1;
        self.next_token += 1;
        let token = Token(self.next_token);
        holder.loading = Some(token);
        let priority = Priority::View(holder.level);
        self.loads.insert(
            token,
            PendingLoad {
                pos,
                ..PendingLoad::default()
            },
        );
        let dimension = self.settings.save.dimension;
        self.slots.store.submit(
            token,
            vec![
                StoreOp::Get(RecordKey::chunk(dimension, pos)),
                StoreOp::Get(RecordKey::entities(dimension, pos)),
                StoreOp::Get(RecordKey::poi(dimension, pos)),
            ],
            priority,
            self.inbox_tx.clone(),
        );
        Check::Progress
    }

    fn run_step(&mut self, pos: ChunkPos, next: ChunkStatus, radius: i32, write: i32) {
        match next {
            ChunkStatus::Spawn | ChunkStatus::Full => {
                if let Some(holder) = self.holders.map_mut().get_mut(&pos)
                    && let Some(chunk) = holder.chunk.as_mut()
                {
                    let chunk = Arc::make_mut(chunk);
                    chunk.status = next;
                    if next == ChunkStatus::Full {
                        chunk.generation = None;
                        self.generated_full.push(pos);
                    }
                    holder.sync_full();
                    holder.touch(pos, &mut self.changed);
                    if next == ChunkStatus::Spawn && self.slots.generator.spawns_original_mobs() {
                        self.generated.push(pos);
                    }
                }
                self.promote(pos);
                self.wake(pos);
                return;
            }
            _ => {}
        }
        let Some(holder) = self.holders.map_mut().get_mut(&pos) else {
            return;
        };
        let level = holder.level;
        let rank = holder.rank;
        // The holder keeps the current version for readers; the job works on a copy
        // (sections are shared until written).
        let Some(target) = holder.chunk.as_deref().cloned() else {
            return;
        };
        holder.busy = true;
        let mut neighbours = Vec::new();
        let mut written = Vec::new();
        for dz in -radius..=radius {
            for dx in -radius..=radius {
                if dx == 0 && dz == 0 {
                    continue;
                }
                let other = ChunkPos::new(pos.x + dx, pos.z + dz);
                let Some(holder) = self.holders.map_mut().get_mut(&other) else {
                    continue;
                };
                let Some(chunk) = holder.chunk.as_ref() else {
                    continue;
                };
                if dx.abs() <= write && dz.abs() <= write {
                    holder.busy = true;
                    written.push(other);
                }
                neighbours.push(Arc::clone(chunk));
            }
        }
        self.in_flight += 1;
        let tx = self.done_tx.clone();
        let priority = job_priority(urgency(level, next.reach()), level, rank);
        match next {
            ChunkStatus::InitializeLight => {
                let light = Arc::clone(&self.slots.light);
                self.slots.executor.spawn(
                    priority,
                    Box::new(move || {
                        let mut targets = vec![target];
                        light.initialize_light(&mut targets);
                        tx.send(Done::Stepped {
                            status: next,
                            targets,
                            written: Vec::new(),
                        });
                    }),
                );
            }
            ChunkStatus::Light => {
                let light = Arc::clone(&self.slots.light);
                self.slots.executor.spawn(
                    priority,
                    Box::new(move || {
                        let mut targets = vec![target];
                        let mut neighbours = neighbours;
                        light.light_chunks(&mut targets, &mut Neighbours::new(&mut neighbours));
                        tx.send(Done::Lit {
                            targets,
                            neighbours,
                        });
                    }),
                );
            }
            _ => {
                let generator = Arc::clone(&self.slots.generator);
                let unneeded = Arc::new(AtomicBool::new(false));
                if let Some(holder) = self.holders.map_mut().get_mut(&pos) {
                    holder.unneeded = Some(Arc::clone(&unneeded));
                }
                self.slots.executor.spawn(
                    priority,
                    Box::new(move || {
                        if unneeded.load(Ordering::Relaxed) {
                            tx.send(Done::Skipped {
                                target: pos,
                                written,
                            });
                            return;
                        }
                        let mut targets = vec![target];
                        let mut neighbours = neighbours;
                        generator.generate(
                            next,
                            &mut targets,
                            &mut Neighbours::new(&mut neighbours),
                        );
                        let written = neighbours
                            .into_iter()
                            .filter(|n| written.contains(&n.pos))
                            .collect();
                        tx.send(Done::Stepped {
                            status: next,
                            targets,
                            written,
                        });
                    }),
                );
            }
        }
    }

    /// Makes a chunk that just became full live: its stored ticks move into the world's tick
    /// queues (and the chunk is saved again for them) and its post-processing marks wait for
    /// its neighbours.
    fn promote(&mut self, pos: ChunkPos) {
        let Some(chunk) = self
            .holders
            .map_mut()
            .get_mut(&pos)
            .and_then(|h| h.chunk.as_mut())
        else {
            return;
        };
        if chunk.status != ChunkStatus::Full {
            return;
        }
        let chunk = Arc::make_mut(chunk);
        self.visibility_check.push(pos);
        let blocks = std::mem::take(&mut chunk.block_ticks);
        let fluids = std::mem::take(&mut chunk.fluid_ticks);
        if !chunk.post_processing.is_empty() {
            self.post_process.push(pos);
        }
        self.promoted.push(pos);
        // Stored ticks hold delays relative to the save; the next save writes them anew from
        // the game time then, even if nothing else changes (the new `LevelChunk` is unsaved).
        let ticks = !blocks.is_empty() || !fluids.is_empty();
        self.block_ticks.add_container(pos, &blocks, self.game_time);
        self.fluid_ticks.add_container(pos, &fluids, self.game_time);
        if ticks {
            self.mark_dirty(pos);
        }
    }

    /// Whether a chunk that became live since the start of the last [`Chunks::tick`] was
    /// generated then rather than loaded (plugins' `ChunkLoadEvent.isNewChunk`).
    pub fn generated_now(&self, pos: ChunkPos) -> bool {
        self.generated_full.contains(&pos)
    }

    /// Chunks that became live since the last call (the game loads their block entities).
    pub fn take_promoted(&mut self, out: &mut Vec<ChunkPos>) {
        out.append(&mut self.promoted);
    }

    /// Live chunks the next [`Chunks::tick`] unloads unless a ticket comes back meanwhile (the
    /// game writes their block entities back first).
    pub fn unloading_next_tick(&self, out: &mut Vec<ChunkPos>) {
        if self.no_save {
            return;
        }
        let next = self.tick + 1;
        out.extend(self.unload_candidates.iter().copied().filter(|pos| {
            self.holders.get(pos).is_some_and(|holder| {
                holder.level > MAX_LEVEL
                    && holder.unload_at.is_some_and(|at| at <= next)
                    && !holder.busy
                    && holder
                        .chunk
                        .as_ref()
                        .is_some_and(|c| c.status == ChunkStatus::Full)
            })
        }));
    }

    /// The stored block entities of a live chunk, for the game to write its live block
    /// entities back; the chunk counts as changed.
    pub fn block_entities_mut(
        &mut self,
        pos: ChunkPos,
    ) -> Option<&mut Vec<vanillite_chunk::slots::BlockEntityData>> {
        let holder = self.holders.map_mut().get_mut(&pos)?;
        if holder.busy {
            return None;
        }
        if holder.status() != Some(ChunkStatus::Full) {
            return None;
        }
        holder.touch(pos, &mut self.changed);
        Some(&mut Arc::make_mut(holder.chunk.as_mut()?).block_entities)
    }

    /// The stored entities of a full chunk ([`ProtoChunk::entities`]): what it was loaded
    /// with, or what the game last wrote back.
    pub fn stored_entities(&self, pos: ChunkPos) -> &[Arc<[u8]>] {
        self.chunk(pos).map_or(&[], |c| &c.entities[..])
    }

    /// The stored POI of a full chunk ([`ProtoChunk::poi`]).
    pub fn stored_poi(&self, pos: ChunkPos) -> Option<&Arc<[u8]>> {
        self.chunk(pos).and_then(|c| c.poi.as_ref())
    }

    /// Replaces the stored POI of a full chunk (the game writes its POI back before saving and
    /// unloading); the chunk's POI count as changed if they differ.
    pub fn set_stored_poi(&mut self, pos: ChunkPos, poi: Option<Arc<[u8]>>) {
        let Some(holder) = self.holders.map_mut().get_mut(&pos) else {
            return;
        };
        if holder.busy || holder.status() != Some(ChunkStatus::Full) {
            return;
        }
        let Some(chunk) = holder.chunk.as_mut() else {
            return;
        };
        if chunk.poi == poi {
            return;
        }
        Arc::make_mut(chunk).poi = poi;
        holder.touch_poi(pos, &mut self.changed);
    }

    /// Replaces the stored entities of a full chunk (the game writes its live entities back
    /// before saving and unloading); the chunk's entities count as changed if they differ.
    pub fn set_stored_entities(&mut self, pos: ChunkPos, entities: Vec<Arc<[u8]>>) {
        let Some(holder) = self.holders.map_mut().get_mut(&pos) else {
            return;
        };
        if holder.busy || holder.status() != Some(ChunkStatus::Full) {
            return;
        }
        let Some(chunk) = holder.chunk.as_mut() else {
            return;
        };
        if chunk.entities == entities {
            return;
        }
        Arc::make_mut(chunk).entities = entities;
        holder.touch_entities(pos, &mut self.changed);
    }

    /// Replaces the rest of the stored data of a full chunk ([`ProtoChunk::extra`]); the chunk
    /// counts as changed.
    pub fn set_stored_extra(&mut self, pos: ChunkPos, extra: Option<Arc<[u8]>>) {
        let Some(holder) = self.holders.map_mut().get_mut(&pos) else {
            return;
        };
        if holder.busy || holder.status() != Some(ChunkStatus::Full) {
            return;
        }
        let Some(chunk) = holder.chunk.as_mut() else {
            return;
        };
        Arc::make_mut(chunk).extra = extra;
        holder.touch(pos, &mut self.changed);
    }

    /// Sets `noSave` (`/save-off`): while set, chunks no ticket needs stay loaded instead of
    /// being saved and unloaded (`ChunkMap.tick` skips `processUnloads`).
    pub fn set_no_save(&mut self, no_save: bool) {
        self.no_save = no_save;
    }

    fn unload(&mut self) {
        if self.no_save {
            return;
        }
        let now = self.tick;
        // Above the backlog limit changed chunks stay loaded until the IO threads caught up.
        let save_allowed = self.slots.store.backlog() <= self.settings.save.max_backlog;
        let mut ready = Vec::new();
        for &pos in &self.unload_candidates {
            if let Some(holder) = self.holders.get(&pos)
                && holder.level > MAX_LEVEL
                && holder.unload_at.is_some_and(|at| at <= now)
                && !holder.busy
                && (save_allowed || !holder.unsaved())
            {
                ready.push(pos);
            }
        }
        let mut save = Vec::new();
        for pos in ready {
            self.unload_candidates.remove(&pos);
            let Some(mut holder) = self.holders.map_mut().remove(&pos) else {
                continue;
            };
            if let Some(chunk) = holder.chunk.as_ref() {
                self.forget_unkept_light(chunk);
            }
            if let Some(chunk) = holder
                .chunk
                .as_mut()
                .filter(|c| c.status == ChunkStatus::Full)
            {
                let blocks = self.block_ticks.remove_container(pos, self.game_time);
                let fluids = self.fluid_ticks.remove_container(pos, self.game_time);
                if holder.dirty {
                    let chunk = Arc::make_mut(chunk);
                    chunk.inhabited_time = chunk.inhabited_time.saturating_add(holder.inhabited);
                    chunk.block_ticks = blocks;
                    chunk.fluid_ticks = fluids;
                    chunk.last_update = self.game_time;
                }
                self.post_process.retain(|&p| p != pos);
                self.visibility_check.push(pos);
            }
            if let Some(waiters) = self.waiters.remove(&pos) {
                for waiter in waiters {
                    self.enqueue(waiter);
                }
            }
            if holder.unsaved()
                && let Some(chunk) = holder.chunk
            {
                save.push(Snapshot {
                    entities: holder.entities_dirty
                        && (holder.entities_stored || !chunk.entities.is_empty()),
                    poi: holder.poi_dirty,
                    blocks: holder.dirty,
                    chunk,
                });
            }
        }
        self.save_chunks(save, false);
    }

    /// Light the neighbours of an unloaded chunk only kept because of it is gone, as in
    /// Vanilla's light engine ([`vanillite_chunk::light::forget_unkept_light`]). Like Vanilla,
    /// clients are not told; they see it when the chunk is sent again.
    fn forget_unkept_light(&mut self, unloaded: &Arc<ProtoChunk>) {
        if unloaded.status < ChunkStatus::InitializeLight {
            return;
        }
        let pos = unloaded.pos;
        for dz in -1..=1 {
            for dx in -1..=1 {
                let center = ChunkPos::new(pos.x + dx, pos.z + dz);
                if center == pos {
                    continue;
                }
                let lit = |holder: &Holder| {
                    holder
                        .chunk
                        .as_ref()
                        .filter(|c| c.status >= ChunkStatus::InitializeLight)
                        .cloned()
                };
                if !self
                    .holders
                    .get(&center)
                    .is_some_and(|h| !h.busy && lit(h).is_some())
                {
                    continue;
                }
                let around: [Option<Arc<ProtoChunk>>; 9] = std::array::from_fn(|i| {
                    let at =
                        ChunkPos::new(center.x + i as i32 % 3 - 1, center.z + i as i32 / 3 - 1);
                    if at == pos {
                        Some(Arc::clone(unloaded))
                    } else {
                        self.holders.get(&at).and_then(lit)
                    }
                });
                let unloaded_index = ((pos.z - center.z + 1) * 3 + (pos.x - center.x + 1)) as usize;
                let area: [Option<&vanillite_chunk::Chunk>; 9] =
                    std::array::from_fn(|i| around[i].as_ref().map(|c| &c.chunk));
                let mut light = around[4].as_ref().map(|c| c.chunk.light().clone()).unwrap();
                if !vanillite_chunk::light::forget_unkept_light(&mut light, &area, unloaded_index) {
                    continue;
                }
                drop(around);
                if let Some(holder) = self.holders.map_mut().get_mut(&center)
                    && let Some(chunk) = holder.chunk.as_mut()
                {
                    *Arc::make_mut(chunk).chunk.light_mut() = light;
                    holder.touch(center, &mut self.changed);
                }
            }
        }
    }

    /// The light the neighbours of a loaded chunk keep again because of it, as in Vanilla's
    /// light engine ([`vanillite_chunk::light::restore_kept_light`]): like Vanilla, clients get
    /// the created sections.
    fn restore_kept_light(&mut self, pos: ChunkPos) {
        let lit = |holder: &Holder| {
            holder
                .chunk
                .as_ref()
                .filter(|c| c.status >= ChunkStatus::InitializeLight)
                .cloned()
        };
        for dz in -1..=1 {
            for dx in -1..=1 {
                let center = ChunkPos::new(pos.x + dx, pos.z + dz);
                if center == pos
                    || !self
                        .holders
                        .get(&center)
                        .is_some_and(|h| !h.busy && lit(h).is_some())
                {
                    continue;
                }
                let around: [Option<Arc<ProtoChunk>>; 9] = std::array::from_fn(|i| {
                    let at =
                        ChunkPos::new(center.x + i as i32 % 3 - 1, center.z + i as i32 / 3 - 1);
                    self.holders.get(&at).and_then(lit)
                });
                let loaded_index = ((pos.z - center.z + 1) * 3 + (pos.x - center.x + 1)) as usize;
                let area: [Option<&vanillite_chunk::Chunk>; 9] =
                    std::array::from_fn(|i| around[i].as_ref().map(|c| &c.chunk));
                let mut light = around[4].as_ref().map(|c| c.chunk.light().clone()).unwrap();
                let restored =
                    vanillite_chunk::light::restore_kept_light(&mut light, &area, loaded_index);
                drop(around);
                if restored.is_empty() {
                    continue;
                }
                if let Some(holder) = self.holders.map_mut().get_mut(&center)
                    && let Some(chunk) = holder.chunk.as_mut()
                {
                    let chunk = Arc::make_mut(chunk);
                    *chunk.chunk.light_mut() = light;
                    let full = chunk.status == ChunkStatus::Full;
                    holder.touch(center, &mut self.changed);
                    if full {
                        self.light_changes
                            .entry(center)
                            .or_default()
                            .add(&LightChange {
                                sky: restored,
                                block: restored,
                            });
                    }
                }
            }
        }
    }

    // --- Saving --------------------------------------------------------------------------

    /// Encodes snapshots on the pool in jobs of about 1 ms; each job submits its records.
    fn save_chunks(&mut self, chunks: Vec<Snapshot>, round: bool) {
        if chunks.is_empty() {
            return;
        }
        for snapshot in &chunks {
            self.encoding
                .insert(snapshot.chunk.pos, Arc::clone(&snapshot.chunk));
        }
        let per_job = self.slots.codec.chunks_per_job().max(1);
        let dimension = self.settings.save.dimension;
        for batch in chunks.chunks(per_job) {
            let batch = batch.to_vec();
            let codec = Arc::clone(&self.slots.codec);
            let store = Arc::clone(&self.slots.store);
            let tx = self.done_tx.clone();
            let inbox = self.inbox_tx.clone();
            self.next_token += 1;
            let token = Token(self.next_token);
            self.encode_jobs += 1;
            self.round_jobs += usize::from(round);
            self.slots.executor.spawn(
                job_priority(MAX_LEVEL + 1, MAX_LEVEL + 1, 0),
                Box::new(move || {
                    let mut ops = Vec::with_capacity(batch.len());
                    let mut errors = Vec::new();
                    for snapshot in &batch {
                        let chunk = &snapshot.chunk;
                        if snapshot.blocks {
                            match codec.encode(chunk) {
                                Ok((meta, bytes)) => ops.push(StoreOp::Put {
                                    key: RecordKey::chunk(dimension, chunk.pos),
                                    meta,
                                    bytes,
                                }),
                                Err(e) => errors.push((chunk.pos, e)),
                            }
                        }
                        if snapshot.entities {
                            let key = RecordKey::entities(dimension, chunk.pos);
                            if chunk.entities.is_empty() {
                                // `EntityStorage.storeEntities`: no entities, no record.
                                ops.push(StoreOp::Delete(key));
                            } else {
                                match codec.encode_entities(chunk) {
                                    Ok((meta, bytes)) => {
                                        ops.push(StoreOp::Put { key, meta, bytes });
                                    }
                                    Err(e) => errors.push((chunk.pos, e)),
                                }
                            }
                        }
                        if snapshot.poi {
                            let key = RecordKey::poi(dimension, chunk.pos);
                            if chunk.poi.is_none() {
                                ops.push(StoreOp::Delete(key));
                            } else {
                                match codec.encode_poi(chunk) {
                                    Ok((meta, bytes)) => {
                                        ops.push(StoreOp::Put { key, meta, bytes });
                                    }
                                    Err(e) => errors.push((chunk.pos, e)),
                                }
                            }
                        }
                    }
                    if !ops.is_empty() {
                        store.submit(token, ops, Priority::Background, inbox);
                    }
                    tx.send(Done::Encoded {
                        round,
                        chunks: batch,
                        errors,
                    });
                }),
            );
        }
    }

    /// Takes the snapshot of a changed chunk for saving; `None` if it is unchanged or a job
    /// works on it (it is then saved in the next round).
    fn capture(&mut self, pos: ChunkPos) -> Option<Snapshot> {
        let holder = self.holders.map_mut().get_mut(&pos)?;
        if !holder.unsaved() {
            return None;
        }
        if holder.busy {
            self.changed.push(pos);
            return None;
        }
        let chunk = holder.chunk.as_mut()?;
        let blocks = std::mem::take(&mut holder.dirty);
        let entities = std::mem::take(&mut holder.entities_dirty)
            && (holder.entities_stored || !chunk.entities.is_empty());
        if entities {
            holder.entities_stored = !chunk.entities.is_empty();
        }
        let poi = std::mem::take(&mut holder.poi_dirty);
        if blocks && chunk.status == ChunkStatus::Full {
            let chunk = Arc::make_mut(chunk);
            chunk.inhabited_time = chunk
                .inhabited_time
                .saturating_add(std::mem::take(&mut holder.inhabited));
            chunk.block_ticks = self.block_ticks.pack(pos, self.game_time);
            chunk.fluid_ticks = self.fluid_ticks.pack(pos, self.game_time);
            chunk.last_update = self.game_time;
        }
        Some(Snapshot {
            chunk: Arc::clone(chunk),
            blocks,
            entities,
            poi,
        })
    }

    /// Starts a save round (autosave, `/save-all`): every chunk changed so far is captured by
    /// [`Chunks::autosave_step`] from the remaining time of the next ticks. A round that is
    /// still running takes the new changes over. `then` runs once the round is committed.
    pub fn start_save_round(&mut self, then: Option<SavedCallback>) {
        let round = self.round.get_or_insert_with(SaveRound::default);
        round.pending.extend(self.changed.drain(..));
        round.waiters.extend(then);
    }

    /// Whether a save round is running.
    pub fn save_round_running(&self) -> bool {
        self.round.is_some()
    }

    /// Runs after the world tick, before the post-tick phase: captures chunks of the running
    /// save round for at most `budget` (the tick's remaining time, capped by the tick budget),
    /// but at least the minimum per tick; seals and commits the round once every chunk is
    /// captured and submitted. Saves of unloaded chunks are committed every commit interval.
    pub fn autosave_step(&mut self, budget: Duration) {
        if self.round.is_some() && self.slots.store.backlog() <= self.settings.save.max_backlog {
            let started = Instant::now();
            let budget = budget.min(self.settings.save.tick_budget);
            let min = self.settings.save.min_chunks_per_tick;
            let per_job = self.slots.codec.chunks_per_job().max(1);
            let mut batch = Vec::with_capacity(per_job);
            let (mut captured, mut visited) = (0usize, 0usize);
            while let Some(pos) = self.round.as_mut().and_then(|r| r.pending.pop_front()) {
                visited += 1;
                if let Some(chunk) = self.capture(pos) {
                    batch.push(chunk);
                    captured += 1;
                    // Jobs are queued as they fill, so the budget covers queueing them too.
                    if batch.len() >= per_job {
                        self.save_chunks(std::mem::take(&mut batch), true);
                    }
                }
                if captured >= min && visited.is_multiple_of(8) && started.elapsed() >= budget {
                    break;
                }
            }
            self.save_chunks(batch, true);
        }
        let sealed =
            self.round.as_ref().is_some_and(|r| r.pending.is_empty()) && self.round_jobs == 0;
        if sealed {
            let waiters = self.round.take().map(|r| r.waiters).unwrap_or_default();
            self.commit(waiters);
        } else if self.round.is_none()
            && self.uncommitted
            && !self.committing
            && self.tick.saturating_sub(self.last_commit) >= self.settings.save.commit_interval
        {
            self.commit(Vec::new());
        }
    }

    /// Commits everything submitted so far; `waiters` run when it is done.
    fn commit(&mut self, waiters: Vec<SavedCallback>) {
        self.commits += 1;
        let round = self.commits;
        self.committing = true;
        self.uncommitted = false;
        self.last_commit = self.tick;
        self.commit_waiters
            .extend(waiters.into_iter().map(|then| (round, then)));
        self.slots.store.commit(round, self.inbox_tx.clone());
    }

    /// Frees what can be built again: the generator's tables ([`ChunkGenerator::release_memory`])
    /// and the spare room of the chunk tables. For a server without players; the next
    /// generation step builds the generator again ([`Chunks::prepare_generator`] does it
    /// ahead).
    pub fn release_memory(&mut self) {
        self.slots.generator.release_memory();
        self.levels.shrink_to_fit();
        self.generation.shrink_to_fit();
        self.block_ticks.shrink_to_fit();
        self.fluid_ticks.shrink_to_fit();
        self.holders.map_mut().shrink_to_fit();
        self.tickets.shrink_to_fit();
        self.generation_tickets.shrink_to_fit();
        self.waiters.shrink_to_fit();
        self.loads.shrink_to_fit();
        self.changed.shrink_to_fit();
        self.unload_candidates.shrink_to_fit();
        self.light_changes.shrink_to_fit();
        self.block_changes.shrink_to_fit();
        self.level_changes.shrink_to_fit();
    }

    /// Runs [`ChunkGenerator::prepare`] as a background job.
    pub fn prepare_generator(&self) {
        let generator = Arc::clone(&self.slots.generator);
        self.slots
            .executor
            .spawn(0, Box::new(move || generator.prepare()));
    }

    /// Runs `then` once the running save round is committed, at once if none runs.
    pub fn when_saved(&mut self, then: SavedCallback) {
        match &mut self.round {
            Some(round) => round.waiters.push(then),
            None => then(),
        }
    }

    /// Saves every changed chunk. With `flush` everything is captured now, written and
    /// committed before this returns (shutdown, `save-all flush`); otherwise a save round
    /// starts and runs from the remaining time of the next ticks.
    pub fn save_all(&mut self, flush: bool) {
        self.start_save_round(None);
        if !flush {
            return;
        }
        // Jobs that write chunks finish first, so no changed chunk is busy.
        self.wait_until(|chunks| chunks.in_flight == 0 && !chunks.relighting);
        // Relights applied while waiting changed chunks too.
        self.start_save_round(None);
        let pending: Vec<ChunkPos> = self
            .round
            .as_mut()
            .map(|r| r.pending.drain(..).collect())
            .unwrap_or_default();
        let mut captured = Vec::with_capacity(pending.len());
        for pos in pending {
            captured.extend(self.capture(pos));
        }
        self.save_chunks(captured, true);
        // Every encode job submits its records first, so the commit covers them.
        self.wait_until(|chunks| chunks.encode_jobs == 0);
        let waiters = self.round.take().map(|r| r.waiters).unwrap_or_default();
        self.commit(waiters);
        let round = self.commits;
        let (tx, rx) = mpsc::channel();
        self.commit_waiters.push((
            round,
            Box::new(move || {
                let _ = tx.send(());
            }),
        ));
        self.wait_until(|_| rx.try_recv().is_ok());
    }

    /// Applies results until `done` holds (blocking; flush and shutdown only).
    fn wait_until(&mut self, mut done: impl FnMut(&Self) -> bool) {
        self.slots.executor.blocked(true);
        loop {
            self.drain_done();
            if done(self) {
                break;
            }
            match self
                .done_rx
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(Duration::from_millis(1))
            {
                Ok(job) => self.apply_done(job),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.slots.executor.blocked(false);
    }

    // --- Chunk and block access ----------------------------------------------------------

    /// A full chunk.
    #[inline]
    pub fn chunk(&self, pos: ChunkPos) -> Option<&Arc<ProtoChunk>> {
        self.holders
            .get(&pos)
            .filter(|h| h.is_full())
            .and_then(|h| h.chunk.as_ref())
    }

    /// Prefetch hint ahead of a loop over chunks, in stages a few chunks apart: stage 0 starts
    /// loading the chunk (its holder is read now), stage 1 its list of sections, stage 2 the
    /// sections that may tick randomly; stage 3 (natural spawning) the whole list of sections
    /// and the `WORLD_SURFACE` heightmap. No effect on anything but the caches.
    #[inline]
    pub fn prefetch(&self, pos: ChunkPos, stage: u8) {
        let Some(c) = self.holders.get(&pos).and_then(|h| h.chunk.as_ref()) else {
            return;
        };
        match stage {
            0 => vanillite_base::prefetch::read(&raw const c.chunk),
            1 => c.chunk.prefetch_sections(),
            2 => c.chunk.prefetch_ticking_sections(),
            _ => c
                .chunk
                .prefetch_sections_and(vanillite_chunk::heightmap::HeightmapKind::WorldSurface),
        }
    }

    /// `ChunkAccess.getInhabitedTime` of a full chunk.
    pub fn inhabited_time(&self, pos: ChunkPos) -> Option<i64> {
        let holder = self.holders.get(&pos)?;
        let chunk = holder.chunk.as_ref()?;
        (chunk.status == ChunkStatus::Full)
            .then(|| chunk.inhabited_time.saturating_add(holder.inhabited))
    }

    /// `ChunkAccess.incrementInhabitedTime` of a full chunk. Like Vanilla it does not mark the
    /// chunk unsaved.
    pub fn increment_inhabited_time(&mut self, pos: ChunkPos) {
        if let Some(holder) = self.holders.map_mut().get_mut(&pos)
            && holder
                .chunk
                .as_ref()
                .is_some_and(|c| c.status == ChunkStatus::Full)
        {
            holder.inhabited = holder.inhabited.saturating_add(1);
        }
    }

    /// The block state at `pos`, `None` outside full chunks and the build height. This is
    /// the world thread's read (`getBlockState`): it goes through Vanilla's chunk cache and
    /// keeps a synchronous load's `unknown` ticket alive. Reads that Vanilla does without
    /// tickets (collisions, path finding) and reads from other threads use [`Chunks::view`].
    #[inline(always)]
    pub fn block(&self, pos: BlockPos) -> Option<BlockStateId> {
        if self.unknown.is_empty() {
            return self.holders.full_block(pos).map(|(state, _)| state);
        }
        self.tracked_block(pos)
    }

    /// [`Chunks::block`] while `unknown` tickets exist: every read goes through the chunk
    /// cache.
    #[cold]
    #[inline(never)]
    fn tracked_block(&self, pos: BlockPos) -> Option<BlockStateId> {
        let chunk_pos = pos.chunk();
        let holder = self.holders.get(&chunk_pos)?;
        let chunk = holder.chunk.as_ref().filter(|_| holder.is_full())?;
        if holder.level <= FULL_LEVEL {
            self.note_read(chunk_pos, true);
        }
        chunk
            .chunk
            .block((pos.x & 15) as u32, pos.y, (pos.z & 15) as u32)
    }

    /// [`Chunks::load_now`] and [`Chunks::block`] in one read: the block state at `pos` if
    /// its chunk needs no load (full, level up to full) and no `unknown` ticket exists, so
    /// both have no side effects; `None` otherwise (also outside the build height).
    #[inline]
    pub fn loaded_now_block(&self, pos: BlockPos) -> Option<BlockStateId> {
        if !self.unknown.is_empty() {
            return None;
        }
        match self.holders.full_block(pos) {
            Some((state, true)) => Some(state),
            _ => None,
        }
    }

    /// [`Chunks::block`] of every position in the box from `min` to `max` (inclusive), in X,
    /// then Z, then Y order (Y varies fastest), passed to `f`: one chunk lookup per chunk
    /// instead of one per block (collisions read boxes of blocks, which span several
    /// sections and chunks).
    pub fn blocks_in(&self, min: BlockPos, max: BlockPos, mut f: impl FnMut(Option<BlockStateId>)) {
        if !self.unknown.is_empty() {
            for x in min.x..=max.x {
                for z in min.z..=max.z {
                    for y in min.y..=max.y {
                        f(self.tracked_block(BlockPos::new(x, y, z)));
                    }
                }
            }
            return;
        }
        full_blocks_in(&self.holders, min, max, f);
    }

    /// [`Chunks::block`] of an accessible chunk ([`Chunks::is_accessible`]) with one map
    /// lookup; `None` if the chunk is not accessible or `pos` is outside the build height.
    #[inline]
    pub fn accessible_block(&self, pos: BlockPos) -> Option<BlockStateId> {
        let chunk_pos = pos.chunk();
        let holder = self.holders.get(&chunk_pos)?;
        if !holder.accessible {
            return None;
        }
        let chunk = holder.chunk.as_ref().filter(|_| holder.is_full())?;
        if holder.level <= FULL_LEVEL {
            self.note_read(chunk_pos, true);
        }
        chunk
            .chunk
            .block((pos.x & 15) as u32, pos.y, (pos.z & 15) as u32)
    }

    /// The pure read view of the full chunks: reads without side effects, shareable with
    /// other threads while nothing writes.
    #[inline]
    pub fn view(&self) -> ChunkView<'_> {
        ChunkView {
            holders: &self.holders,
        }
    }

    /// Sets a block state in a full chunk like `LevelChunk.setBlockState` (heightmaps follow,
    /// light is recomputed after the tick). Returns the previous state, `None` if nothing
    /// changed or the chunk is not loaded. No neighbour or shape updates: those are gameplay.
    pub fn set_block(&mut self, pos: BlockPos, state: BlockStateId) -> Option<BlockStateId> {
        let chunk_pos = pos.chunk();
        let holder = self.holders.map_mut().get_mut(&chunk_pos)?;
        if holder.busy {
            return None;
        }
        let chunk = holder.chunk.as_mut()?;
        if chunk.status != ChunkStatus::Full {
            return None;
        }
        let old = Arc::make_mut(chunk).chunk.set_block(
            (pos.x & 15) as u32,
            pos.y,
            (pos.z & 15) as u32,
            state,
        )?;
        holder.touch(chunk_pos, &mut self.changed);
        self.block_changes.push(pos);
        if self.slots.light.changes_light(old, state) {
            self.relight.entry(chunk_pos).or_default().push(pos);
        }
        Some(old)
    }

    /// How many block changes are recorded since the last [`Self::take_block_changes`].
    pub fn block_change_count(&self) -> usize {
        self.block_changes.len()
    }

    /// The block changes recorded after the first `start` (see [`Self::block_change_count`]),
    /// without taking them.
    pub fn block_changes_from(&self, start: usize) -> &[BlockPos] {
        &self.block_changes[start.min(self.block_changes.len())..]
    }

    /// Blocks changed since the last call, in change order (for block update packets).
    pub fn take_block_changes(&mut self, out: &mut Vec<BlockPos>) {
        out.append(&mut self.block_changes);
    }

    /// Live chunks whose post-processing marks can be handled now (their 3 × 3 area is full),
    /// with the marks; the marks are removed from the chunks.
    pub fn take_post_processing(&mut self, out: &mut Vec<(ChunkPos, Vec<BlockPos>)>) {
        let mut index = 0;
        while index < self.post_process.len() {
            let pos = self.post_process[index];
            let ready = (-1..=1).all(|dz| {
                (-1..=1).all(|dx| self.chunk(ChunkPos::new(pos.x + dx, pos.z + dz)).is_some())
            });
            if !ready {
                index += 1;
                continue;
            }
            self.post_process.swap_remove(index);
            if let Some(holder) = self.holders.map_mut().get_mut(&pos)
                && let Some(chunk) = holder.chunk.as_mut()
            {
                let marks = std::mem::take(&mut Arc::make_mut(chunk).post_processing);
                holder.touch(pos, &mut self.changed);
                out.push((pos, marks));
            }
        }
    }

    // --- Scheduled ticks -----------------------------------------------------------------

    /// The game time ticks are scheduled and saved against.
    pub fn game_time(&self) -> i64 {
        self.game_time
    }

    /// Sets the game time (once per tick, by the world).
    pub fn set_game_time(&mut self, game_time: i64) {
        self.game_time = game_time;
    }

    fn new_tick(
        &mut self,
        pos: BlockPos,
        kind: u16,
        delay: i32,
        priority: TickPriority,
    ) -> ScheduledTick {
        let sub_tick = self.sub_tick;
        self.sub_tick += 1;
        ScheduledTick {
            kind,
            pos,
            trigger: self.game_time + i64::from(delay),
            priority,
            sub_tick,
        }
    }

    fn mark_dirty(&mut self, chunk: ChunkPos) {
        if let Some(holder) = self.holders.map_mut().get_mut(&chunk) {
            holder.touch(chunk, &mut self.changed);
        }
    }

    /// `getBlockTicks().copyAreaFrom(blockTicks, area, offset)` within this world: the block
    /// ticks scheduled in the box `min..=max` again at `pos + offset`, after all others.
    pub fn copy_block_ticks(&mut self, min: BlockPos, max: BlockPos, offset: BlockPos) {
        let inside = |p: BlockPos| {
            (min.x..=max.x).contains(&p.x)
                && (min.y..=max.y).contains(&p.y)
                && (min.z..=max.z).contains(&p.z)
        };
        let mut ticks: Vec<ScheduledTick> = Vec::new();
        for cz in (min.z >> 4)..=(max.z >> 4) {
            for cx in (min.x >> 4)..=(max.x >> 4) {
                ticks.extend(
                    self.block_ticks
                        .ticks_in(ChunkPos::new(cx, cz))
                        .filter(|t| inside(t.pos))
                        .copied(),
                );
            }
        }
        let min_sub = ticks.iter().map(|t| t.sub_tick).min().unwrap_or(0);
        let max_sub = ticks.iter().map(|t| t.sub_tick).max().unwrap_or(0);
        for tick in ticks {
            let pos = BlockPos::new(
                tick.pos.x.saturating_add(offset.x),
                tick.pos.y.saturating_add(offset.y),
                tick.pos.z.saturating_add(offset.z),
            );
            let moved = ScheduledTick {
                pos,
                sub_tick: tick
                    .sub_tick
                    .saturating_sub(min_sub)
                    .saturating_add(max_sub)
                    .saturating_add(1),
                ..tick
            };
            if self.block_ticks.schedule(moved) {
                self.mark_dirty(pos.chunk());
            }
        }
    }

    /// `getBlockTicks().clearArea(area)`: drops the block ticks inside `min..=max`.
    pub fn clear_block_ticks(&mut self, min: BlockPos, max: BlockPos) {
        for chunk in self.block_ticks.clear_area(min, max) {
            self.mark_dirty(chunk);
        }
    }

    /// Changes a full chunk in place (`/fillbiome`'s `fillBiomesFromNoise`): the chunk is saved
    /// again and its cached packets are dropped. `None` if the chunk is not full.
    pub fn modify_chunk<R>(
        &mut self,
        pos: ChunkPos,
        f: impl FnOnce(&mut ProtoChunk) -> R,
    ) -> Option<R> {
        let holder = self.holders.map_mut().get_mut(&pos)?;
        if holder.busy {
            return None;
        }
        let chunk = holder.chunk.as_mut()?;
        if chunk.status != ChunkStatus::Full {
            return None;
        }
        let result = f(Arc::make_mut(chunk));
        holder.sync_full();
        holder.dirty = true;
        // The packet cache drops chunks with block changes.
        self.block_changes
            .push(BlockPos::new(pos.x << 4, self.info.min_y, pos.z << 4));
        Some(result)
    }

    /// `scheduleTick(pos, block, delay, priority)`; `block` is a block id. Ignored outside live
    /// chunks or if the block already has a tick there. The sub-tick counter advances either
    /// way, as in Vanilla.
    pub fn schedule_block_tick(
        &mut self,
        pos: BlockPos,
        block: u16,
        delay: i32,
        priority: TickPriority,
    ) {
        let tick = self.new_tick(pos, block, delay, priority);
        if self.block_ticks.schedule(tick) {
            self.mark_dirty(pos.chunk());
        }
    }

    /// `scheduleTick(pos, fluid, delay, priority)`; `fluid` is a fluid id.
    pub fn schedule_fluid_tick(
        &mut self,
        pos: BlockPos,
        fluid: u16,
        delay: i32,
        priority: TickPriority,
    ) {
        let tick = self.new_tick(pos, fluid, delay, priority);
        if self.fluid_ticks.schedule(tick) {
            self.mark_dirty(pos.chunk());
        }
    }

    /// `getBlockTicks().hasScheduledTick(pos, block)`.
    pub fn has_block_tick(&self, pos: BlockPos, block: u16) -> bool {
        self.block_ticks.has_scheduled(pos, block)
    }

    /// `getFluidTicks().hasScheduledTick(pos, fluid)`.
    pub fn has_fluid_tick(&self, pos: BlockPos, fluid: u16) -> bool {
        self.fluid_ticks.has_scheduled(pos, fluid)
    }

    /// The scheduled block ticks (read only).
    pub fn block_ticks(&self) -> &LevelTicks {
        &self.block_ticks
    }

    /// The scheduled fluid ticks (read only).
    pub fn fluid_ticks(&self) -> &LevelTicks {
        &self.fluid_ticks
    }

    /// `getBlockTicks().willTickThisTick(pos, block)`.
    pub fn block_tick_runs_this_tick(&mut self, pos: BlockPos, block: u16) -> bool {
        self.block_ticks.will_tick_this_tick(pos, block)
    }

    /// Collects the block ticks due now (at most `max`) in Vanilla's order; run them with
    /// [`Self::next_block_tick`].
    pub fn collect_block_ticks(&mut self, max: usize) {
        let mut drained = Vec::new();
        let (holders, simulation) = (&self.holders, &self.simulation);
        let ticking = |pos: ChunkPos| tick_check(holders, simulation, pos);
        self.block_ticks
            .collect(self.game_time, max, ticking, |pos| drained.push(pos));
        for pos in drained {
            self.mark_dirty(pos);
        }
    }

    /// Collects the fluid ticks due now, like [`Self::collect_block_ticks`].
    pub fn collect_fluid_ticks(&mut self, max: usize) {
        let mut drained = Vec::new();
        let (holders, simulation) = (&self.holders, &self.simulation);
        let ticking = |pos: ChunkPos| tick_check(holders, simulation, pos);
        self.fluid_ticks
            .collect(self.game_time, max, ticking, |pos| drained.push(pos));
        for pos in drained {
            self.mark_dirty(pos);
        }
    }

    /// The next collected block tick: position and block id.
    pub fn next_block_tick(&mut self) -> Option<(BlockPos, u16)> {
        self.block_ticks.next_to_run()
    }

    /// The next collected fluid tick: position and fluid id.
    pub fn next_fluid_tick(&mut self) -> Option<(BlockPos, u16)> {
        self.fluid_ticks.next_to_run()
    }

    /// Scheduled block and fluid ticks of the live chunks.
    pub fn scheduled_ticks(&self) -> (usize, usize) {
        (self.block_ticks.count(), self.fluid_ticks.count())
    }

    /// Chunks that passed the spawn step (were generated, not loaded) since the last call, for
    /// the mobs a fresh chunk starts with (`spawnMobsForChunkGeneration`).
    pub fn take_generated(&mut self, out: &mut Vec<ChunkPos>) {
        out.append(&mut self.generated);
    }

    /// Full chunks whose light changed since the last call (for light update packets).
    pub fn take_light_changes(&mut self, out: &mut Vec<(ChunkPos, LightChange)>) {
        out.extend(self.light_changes.drain());
    }

    /// Recomputes the light around changed chunks now and applies the results that are done:
    /// with an inline executor, all of them (scenario worlds observe settled light).
    pub fn flush_light(&mut self) {
        // Relights still to apply would hold back new ones of the same centres.
        self.drain_done();
        self.start_relights();
        self.drain_done();
    }

    /// Starts the light recomputation around the chunks changed in a tick, at its end: the
    /// results are applied between ticks ([`Chunks::pump`]) or by the next [`Chunks::tick`],
    /// so the next tick sees the new light. Vanilla's light thread works the same way: the tick
    /// loop starts it while it waits for the next tick (`ServerChunkCache.pollTask`).
    pub fn end_tick(&mut self) {
        self.start_relights();
    }

    /// Starts the light update for the blocks changed since the last one, as one job: at most
    /// `max_relights` chunks, the rest follows when it is applied. A chunk waits while a job
    /// writes one of the chunks its light can reach.
    fn start_relights(&mut self) {
        if self.relighting || self.relight.is_empty() {
            return;
        }
        let max = self.settings.max_relights.max(1);
        let mut changes = Vec::new();
        let mut read: Vec<Arc<ProtoChunk>> = Vec::new();
        let pending: Vec<ChunkPos> = self.relight.keys().copied().collect();
        for center in pending {
            if changes.len() >= max {
                break;
            }
            if self.holders.get(&center).and_then(Holder::status) != Some(ChunkStatus::Full) {
                // Unloaded: its light was saved as it was.
                self.relight.remove(&center);
                continue;
            }
            let around = (-1..=1)
                .flat_map(|dz| (-1..=1).map(move |dx| ChunkPos::new(center.x + dx, center.z + dz)));
            let around: Vec<(ChunkPos, &Holder)> = around
                .filter_map(|pos| Some((pos, self.holders.get(&pos)?)))
                .filter(|(_, holder)| holder.chunk.is_some())
                .collect();
            if around.iter().any(|(_, holder)| holder.busy) {
                continue;
            }
            for (pos, holder) in around {
                if let Some(chunk) = &holder.chunk
                    && !read.iter().any(|other| other.pos == pos)
                {
                    read.push(Arc::clone(chunk));
                }
            }
            if let Some(blocks) = self.relight.remove(&center) {
                changes.push((center, blocks));
            }
        }
        if changes.is_empty() {
            return;
        }
        self.relighting = true;
        let light = Arc::clone(&self.slots.light);
        let tx = self.done_tx.clone();
        self.slots.executor.spawn(
            0,
            Box::new(move || {
                let mut read = read;
                let blocks: Vec<BlockPos> = changes
                    .iter()
                    .flat_map(|(_, blocks)| blocks.iter().copied())
                    .collect();
                let light = light.relight(&blocks, &Neighbours::new(&mut read));
                tx.send(Done::Relit {
                    changes,
                    read,
                    light,
                });
            }),
        );
    }
}

/// The order of a chunk's next step (lower first): the level of the most urgent chunk that
/// may wait for it, its own level less the step's [`ChunkStatus::reach`]. The chunk under a
/// player and everything its way to full needs (the structure starts eleven chunks away
/// included) then come before the chunks around it, whose own levels are higher. Steps of the
/// same urgency go by level: where a view moves, the chunks entering it finish before the
/// work for the chunks after them.
fn urgency(level: u8, reach: u8) -> u8 {
    level.saturating_sub(reach)
}

/// The compute-pool priority of a chunk job: by urgency ([`urgency`]), then by level, then by
/// rank ([`Chunks::set_focus`]). Below `1 << 20`, where the plugins' background jobs start.
fn job_priority(urgency: u8, level: u8, rank: u8) -> u32 {
    u32::from(urgency.min(63)) << 14 | u32::from(level.min(63)) << 8 | u32::from(rank)
}

/// The level of the generation ticket of a chunk with this ticket level: the pyramid of a
/// full chunk (or of the ring around full chunks); `None` above [`MAX_TICKET_LEVEL`].
fn generation_ticket_level(ticket: u8) -> Option<u8> {
    (ticket <= MAX_TICKET_LEVEL).then_some(ticket.max(FULL_LEVEL))
}

/// The visibility of a full chunk at `level`.
fn visibility_of_level(level: u8) -> Visibility {
    if level <= ENTITY_TICKING_LEVEL {
        Visibility::Ticking
    } else if level <= FULL_LEVEL {
        Visibility::Tracked
    } else {
        Visibility::Hidden
    }
}

/// A store result of a load: the record, `None` if it is missing.
fn load_result(result: StoreResult) -> io::Result<Option<(RecordMeta, Arc<[u8]>)>> {
    match result {
        StoreResult::Record(meta, bytes) => Ok(Some((meta, bytes))),
        StoreResult::Missing => Ok(None),
        StoreResult::Failed(e) => Err(e),
        StoreResult::Meta(_) | StoreResult::Done | StoreResult::Cancelled => {
            Err(io::Error::other("unexpected store result for a load"))
        }
    }
}

/// [`Chunks::is_block_ticking`] over the fields, for use while the tick queues are borrowed.
fn tick_check(
    holders: &FxHashMap<ChunkPos, Holder>,
    simulation: &SimulationLevels,
    pos: ChunkPos,
) -> bool {
    simulation.level(pos) <= BLOCK_TICKING_LEVEL
        && holders
            .get(&pos)
            .is_some_and(|h| h.entities_loaded && h.level <= BLOCK_TICKING_LEVEL && h.is_full())
        && neighbours_full(holders, pos)
}

/// Whether the eight chunks around `pos` are full (`ChunkMap.prepareTickingChunk` waits for
/// the 3 × 3 area at `FULL` before a chunk ticks). A ticking chunk's neighbours are within the
/// accessible level, so a block change next to it never has to load a chunk synchronously.
fn neighbours_full(holders: &FxHashMap<ChunkPos, Holder>, pos: ChunkPos) -> bool {
    (-1..=1).all(|dz| {
        (-1..=1).all(|dx| {
            (dx == 0 && dz == 0)
                || holders
                    .get(&ChunkPos::new(pos.x + dx, pos.z + dz))
                    .is_some_and(Holder::is_full)
        })
    })
}

/// The light sections of a chunk that changed: bit `i` is light section `i` (one below the
/// lowest block section first).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightChange {
    /// Changed sky light sections.
    pub sky: SectionMask,
    /// Changed block light sections.
    pub block: SectionMask,
}

impl LightChange {
    /// Every section changed.
    pub const ALL: LightChange = LightChange {
        sky: SectionMask([u64::MAX; 4]),
        block: SectionMask([u64::MAX; 4]),
    };

    /// The sections that differ between two light states of a chunk.
    pub fn between(old: &ChunkLight, new: &ChunkLight) -> LightChange {
        let diff = |a: &[LightNibbles], b: &[LightNibbles]| {
            let mut mask = SectionMask::default();
            for index in 0..a.len().max(b.len()).min(256) {
                if a.get(index) != b.get(index) {
                    mask.set(index);
                }
            }
            mask
        };
        LightChange {
            sky: diff(old.sky(), new.sky()),
            block: diff(old.block(), new.block()),
        }
    }

    /// No section changed.
    pub fn is_empty(&self) -> bool {
        self.sky.is_empty() && self.block.is_empty()
    }

    /// Adds the sections of `other`.
    pub fn add(&mut self, other: &LightChange) {
        for i in 0..4 {
            self.sky.0[i] |= other.sky.0[i];
            self.block.0[i] |= other.block.0[i];
        }
    }
}
