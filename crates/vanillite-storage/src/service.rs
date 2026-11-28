//! The storage service (#47): one asynchronous record store per dimension on a shared group of
//! IO threads.
//!
//! - [`IoPool`] is the disk IO thread group of the server (`storage.io-threads`, default 2). Its
//!   threads only make system calls; codec work runs on the compute pool.
//! - [`RegionStore`] is the built-in `chunk-storage` slot for region files of either world
//!   format ([`WorldFormat`]): Anvil (`.mca`) or the own format (`.vwr`, [`crate::vwr`]). It
//!   is owner-agnostic: any thread may submit, results go to the requester's inbox by token.
//!
//! Open files live in a table sharded by region file. Each file has one owner at a time: the IO
//! thread that works on it takes it out of the table, so no two instances of a file allocate
//! sectors (#50) and no lock is held during IO. Writes wait in the table until they are on disk:
//! a `Get` of a key with a pending write is answered from it (read-your-writes), and a second
//! `Put` before the write replaces the first (coalescing). Reads of one file are batched: records
//! in neighbouring sectors come in one `pread`, and with read-ahead the other records of the
//! chunk's 4 × 4 tile that lie next to them are read along into a bounded record cache.
//!
//! A commit writes every `Put` submitted before it and then syncs every touched file
//! (`sync-chunk-writes`); replaced sectors are reused only after that, so the last committed
//! version of a chunk survives a power failure. Own-format files write their index at the
//! commit (and before they are closed), so a crash leaves them exactly as of the last commit.

use std::collections::{BinaryHeap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use vanillite_base::ChunkPos;
use vanillite_base::hash::FxHashMap;
use vanillite_chunk::slots::WorldInfo;
use vanillite_chunk::store::{
    ChunkStore, ChunkStoreProvider, CompletionTarget, DimensionId, Priority, RecordKey, RecordMeta,
    StoreOp, StoreResult, Stream, Token,
};

use crate::format::WorldFormat;
use crate::region::{self, RegionFile};
use crate::vwr::VwrFile;

/// Shards of the file table of one dimension.
const SHARDS: usize = 16;

/// Settings of the stores.
#[derive(Clone, Copy, Debug)]
pub struct StoreSettings {
    /// Commits sync the touched files (`sync-chunk-writes`).
    pub sync: bool,
    /// Records next to a read one in its 4 × 4 tile are read along (`storage.read-ahead`).
    pub read_ahead: bool,
    /// Bytes of records read ahead kept per dimension (`storage.read-cache`).
    pub read_cache: usize,
    /// Region files kept open per dimension while unused (`storage.max-open-files`).
    pub max_open_files: usize,
}

impl Default for StoreSettings {
    fn default() -> Self {
        StoreSettings {
            sync: true,
            read_ahead: true,
            read_cache: 64 << 20,
            max_open_files: 256,
        }
    }
}

// --- IO threads --------------------------------------------------------------------------

/// The disk IO thread group, shared by every dimension. Cheap to clone.
#[derive(Clone)]
pub struct IoPool {
    inner: Arc<PoolInner>,
}

impl std::fmt::Debug for IoPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoPool").finish_non_exhaustive()
    }
}

struct PoolInner {
    queue: Mutex<PoolQueue>,
    wake: Condvar,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Runs on a thread that runs out of work ([`IoPool::set_idle_hook`]).
    idle: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct PoolQueue {
    jobs: BinaryHeap<Job>,
    seq: u64,
    stop: bool,
}

struct Job {
    priority: Priority,
    seq: u64,
    work: Work,
}

enum Work {
    /// Run the queued ops of a file.
    File(Arc<StoreInner>, FileKey),
    /// Write every put up to `upto` and sync.
    Commit {
        store: Arc<StoreInner>,
        round: u64,
        to: CompletionTarget,
        upto: u64,
    },
    /// Close the idle files and drop the read-ahead records ([`ChunkStore::release_memory`]).
    Release(Arc<StoreInner>),
}

impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        (self.priority, self.seq) == (other.priority, other.seq)
    }
}

impl Eq for Job {}

impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Job {
    /// The heap pops the most urgent, then the oldest job.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.priority, other.seq).cmp(&(self.priority, self.seq))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl IoPool {
    /// Starts `threads` IO threads (at least 1).
    pub fn new(threads: usize) -> io::Result<IoPool> {
        let inner = Arc::new(PoolInner {
            queue: Mutex::new(PoolQueue::default()),
            wake: Condvar::new(),
            threads: Mutex::new(Vec::new()),
            idle: std::sync::OnceLock::new(),
        });
        for index in 0..threads.max(1) {
            let worker = Arc::clone(&inner);
            let handle = std::thread::Builder::new()
                .name(format!("Storage IO #{index}"))
                .spawn(move || worker_loop(&worker))?;
            lock(&inner.threads).push(handle);
        }
        Ok(IoPool { inner })
    }

    fn push(&self, priority: Priority, work: Work) {
        let mut queue = lock(&self.inner.queue);
        queue.seq += 1;
        let seq = queue.seq;
        queue.jobs.push(Job {
            priority,
            seq,
            work,
        });
        drop(queue);
        self.inner.wake.notify_one();
    }

    /// Sets what a thread runs, without the queue lock, each time it runs out of work and
    /// after each [`IoPool::wake_idle`] (the server's memory hand-back of the thread's
    /// allocator heap).
    pub fn set_idle_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        let _ = self.inner.idle.set(hook);
    }

    /// Wakes the idle threads so they run the idle hook.
    pub fn wake_idle(&self) {
        let _queue = lock(&self.inner.queue);
        self.inner.wake.notify_all();
    }

    /// Runs the queued jobs to the end and stops the threads.
    pub fn shutdown(&self) {
        lock(&self.inner.queue).stop = true;
        self.inner.wake.notify_all();
        let threads = std::mem::take(&mut *lock(&self.inner.threads));
        for thread in threads {
            let _ = thread.join();
        }
    }
}

fn worker_loop(pool: &PoolInner) {
    // The idle hook ran since the last job or wake-up.
    let mut idled = false;
    loop {
        let job = {
            let mut queue = lock(&pool.queue);
            loop {
                if let Some(job) = queue.jobs.pop() {
                    break job;
                }
                if queue.stop {
                    return;
                }
                if !idled && let Some(idle) = pool.idle.get() {
                    drop(queue);
                    idle();
                    idled = true;
                    queue = lock(&pool.queue);
                    continue;
                }
                queue = pool
                    .wake
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
                idled = false;
            }
        };
        idled = false;
        match job.work {
            Work::File(store, key) => store.run_file(key),
            Work::Commit {
                store,
                round,
                to,
                upto,
            } => {
                let result = store.run_commit(upto);
                to.post_commit(round, result);
            }
            Work::Release(store) => store.release_memory(),
        }
    }
}

// --- Open files ---------------------------------------------------------------------------

/// An open region file of either format.
pub(crate) enum OpenFile {
    Anvil(Box<RegionFile>),
    Own(VwrFile),
}

impl OpenFile {
    pub(crate) fn open(
        format: WorldFormat,
        path: &std::path::Path,
        create: bool,
    ) -> io::Result<Option<Self>> {
        Ok(match format {
            WorldFormat::Anvil => {
                RegionFile::open(path, create)?.map(|f| OpenFile::Anvil(Box::new(f)))
            }
            WorldFormat::Vanillite => match VwrFile::open(path, create) {
                // A damaged own-format file (#167): it is kept aside for recovery and a new
                // one takes the writes, so the region's chunks are saved again.
                Err(e) if create && e.kind() == io::ErrorKind::InvalidData => {
                    let millis = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis());
                    let aside = crate::files::with_suffix(path, &format!(".damaged-{millis}"));
                    std::fs::rename(path, &aside)?;
                    log::error!(
                        "{e}; the file is kept as {} and a new one is started",
                        aside.display()
                    );
                    VwrFile::open(path, true)?.map(OpenFile::Own)
                }
                result => result?.map(OpenFile::Own),
            },
        })
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            OpenFile::Anvil(file) => file.name(),
            OpenFile::Own(file) => file.name(),
        }
    }

    pub(crate) fn write(
        &mut self,
        pos: ChunkPos,
        meta: RecordMeta,
        record: &[u8],
    ) -> io::Result<()> {
        match self {
            OpenFile::Anvil(file) => file.write_record(pos, record),
            OpenFile::Own(file) => file.write_record(pos, meta, record),
        }
    }

    pub(crate) fn delete(&mut self, pos: ChunkPos) -> io::Result<()> {
        match self {
            OpenFile::Anvil(file) => file.delete(pos),
            OpenFile::Own(file) => file.delete(pos),
        }
    }

    pub(crate) fn read_many(
        &self,
        region: (i32, i32),
        wanted: &[usize],
        ahead: &[usize],
        out: &mut Vec<region::RecordRead>,
    ) -> (usize, usize) {
        match self {
            OpenFile::Anvil(file) => file.read_many(region, wanted, ahead, out),
            OpenFile::Own(file) => file.read_many(region, wanted, ahead, out),
        }
    }

    /// Makes the writes durable: Anvil syncs (with `sync`) and frees replaced sectors, the
    /// own format writes its index.
    pub(crate) fn commit(&mut self, sync: bool) -> io::Result<()> {
        match self {
            OpenFile::Anvil(file) if sync => file.sync(),
            OpenFile::Anvil(file) => {
                file.release_freed();
                Ok(())
            }
            OpenFile::Own(file) => file.flush(sync),
        }
    }
}

// --- The store of one dimension ----------------------------------------------------------

/// A region file of a stream: (stream, region x, region z).
type FileKey = (Stream, i32, i32);

fn file_key(key: &RecordKey) -> FileKey {
    let (x, z) = key.pos.region();
    (key.stream, x, z)
}

/// A record with its metadata.
type Record = (RecordMeta, Arc<[u8]>);

/// A write taken by an IO thread: slot, sequence number, record (`None` deletes).
type WriteJob = (usize, u64, Option<Record>);

/// What a read found; errors are rebuilt for every op that wanted the record.
type ReadOutcome = Result<Option<Record>, (io::ErrorKind, String)>;

/// A batch's reply address.
struct Batch {
    token: Token,
    to: CompletionTarget,
}

struct PendingRead {
    slot: usize,
    meta_only: bool,
    priority: Priority,
    batch: Arc<Batch>,
    index: usize,
}

struct PendingWrite {
    seq: u64,
    /// `None` deletes.
    record: Option<Record>,
    /// The last attempt failed (disk full, read-only): the write stays queued, out of the
    /// backlog, and is tried again by the next commit or a later put of the key.
    failed: bool,
    /// Ops answered once this key is written.
    waiters: Vec<(Arc<Batch>, usize)>,
}

impl PendingWrite {
    fn len(&self) -> usize {
        self.record.as_ref().map_or(0, |r| r.1.len())
    }
}

#[derive(Default)]
struct FileSlot {
    /// The open file; taken out while an IO thread owns it.
    region: Option<OpenFile>,
    /// An IO thread works on this file.
    busy: bool,
    /// The file does not exist (found by the last read).
    missing: bool,
    /// Priority of the job queued for it, if any.
    queued: Option<Priority>,
    reads: Vec<PendingRead>,
    writes: FxHashMap<usize, PendingWrite>,
    /// Written since the last sync.
    dirty: bool,
    last_use: u64,
}

impl FileSlot {
    fn has_work(&self) -> bool {
        !self.reads.is_empty() || self.writes.values().any(|w| !w.failed)
    }

    fn best_priority(&self) -> Priority {
        self.reads
            .iter()
            .map(|r| r.priority)
            .min()
            .unwrap_or(Priority::Background)
    }

    fn idle(&self) -> bool {
        !self.busy && self.reads.is_empty() && self.writes.is_empty() && self.queued.is_none()
    }
}

/// Records read ahead but not asked for within this time are dropped: the chunk they were
/// read ahead for went the other way.
const READ_AHEAD_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// Records read ahead, first in first out under a byte budget and [`READ_AHEAD_AGE`].
#[derive(Default)]
struct ReadCache {
    map: FxHashMap<(Stream, ChunkPos), Record>,
    order: VecDeque<((Stream, ChunkPos), std::time::Instant)>,
    bytes: usize,
    budget: usize,
}

impl ReadCache {
    fn take(&mut self, key: (Stream, ChunkPos)) -> Option<Record> {
        let record = self.map.remove(&key)?;
        self.bytes -= record.1.len();
        Some(record)
    }

    fn insert(&mut self, key: (Stream, ChunkPos), record: Record) {
        if record.1.len() > self.budget {
            return;
        }
        self.bytes += record.1.len();
        if let Some(old) = self.map.insert(key, record) {
            self.bytes -= old.1.len();
        }
        let now = std::time::Instant::now();
        self.order.push_back((key, now));
        while let Some(&(oldest, at)) = self.order.front()
            && (self.bytes > self.budget || now.duration_since(at) > READ_AHEAD_AGE)
        {
            self.order.pop_front();
            self.take(oldest);
        }
        if self.order.len() > self.map.len() * 4 + 64 {
            let map = &self.map;
            self.order.retain(|(k, _)| map.contains_key(k));
        }
    }

    /// Drops every record and gives back the room.
    fn clear(&mut self) {
        self.map = FxHashMap::default();
        self.order = VecDeque::new();
        self.bytes = 0;
    }
}

#[derive(Default)]
struct ShardState {
    files: FxHashMap<FileKey, FileSlot>,
    cache: ReadCache,
}

struct Shard {
    state: Mutex<ShardState>,
    /// Signalled when a file stops being busy.
    idle: Condvar,
}

/// Counters of a store, for benchmarks and metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Read system calls on region files.
    pub reads: u64,
    /// Bytes those reads returned.
    pub bytes_read: u64,
    /// Records returned by those reads (wanted and read ahead).
    pub records_read: u64,
    /// Gets answered from the read-ahead cache.
    pub cache_hits: u64,
    /// Gets answered from a pending write.
    pub pending_hits: u64,
    /// Records written.
    pub records_written: u64,
    /// Puts that replaced a pending one before it was written.
    pub coalesced: u64,
}

#[derive(Default)]
struct Counters {
    reads: AtomicU64,
    bytes_read: AtomicU64,
    records_read: AtomicU64,
    cache_hits: AtomicU64,
    pending_hits: AtomicU64,
    records_written: AtomicU64,
    coalesced: AtomicU64,
}

struct StoreInner {
    dir: PathBuf,
    format: WorldFormat,
    settings: StoreSettings,
    shards: Box<[Shard]>,
    pool: IoPool,
    /// Bytes of puts waiting for IO.
    backlog: AtomicUsize,
    put_seq: AtomicU64,
    use_clock: AtomicU64,
    open: AtomicUsize,
    counters: Counters,
}

/// The built-in `chunk-storage` slot: the region files of one dimension (`region/`,
/// `entities/`, `poi/`) in one world format, on the shared IO threads.
#[derive(Clone)]
pub struct RegionStore {
    inner: Arc<StoreInner>,
}

impl std::fmt::Debug for RegionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegionStore")
            .field("dir", &self.inner.dir)
            .field("format", &self.inner.format)
            .finish_non_exhaustive()
    }
}

impl RegionStore {
    /// The store of the dimension folder `dir` in `format`.
    pub fn new(
        dir: PathBuf,
        format: WorldFormat,
        settings: StoreSettings,
        pool: IoPool,
    ) -> RegionStore {
        let budget = settings.read_cache / SHARDS;
        let shards = (0..SHARDS)
            .map(|_| Shard {
                state: Mutex::new(ShardState {
                    files: FxHashMap::default(),
                    cache: ReadCache {
                        budget,
                        ..ReadCache::default()
                    },
                }),
                idle: Condvar::new(),
            })
            .collect();
        RegionStore {
            inner: Arc::new(StoreInner {
                dir,
                format,
                settings,
                shards,
                pool,
                backlog: AtomicUsize::new(0),
                put_seq: AtomicU64::new(0),
                use_clock: AtomicU64::new(0),
                open: AtomicUsize::new(0),
                counters: Counters::default(),
            }),
        }
    }

    /// The counters so far.
    pub fn stats(&self) -> StoreStats {
        let c = &self.inner.counters;
        StoreStats {
            reads: c.reads.load(Ordering::Relaxed),
            bytes_read: c.bytes_read.load(Ordering::Relaxed),
            records_read: c.records_read.load(Ordering::Relaxed),
            cache_hits: c.cache_hits.load(Ordering::Relaxed),
            pending_hits: c.pending_hits.load(Ordering::Relaxed),
            records_written: c.records_written.load(Ordering::Relaxed),
            coalesced: c.coalesced.load(Ordering::Relaxed),
        }
    }

    /// Region files open right now.
    pub fn open_files(&self) -> usize {
        self.inner.open.load(Ordering::Relaxed)
    }
}

impl StoreInner {
    fn shard(&self, key: FileKey) -> &Shard {
        let hash = (key.1 as u32).wrapping_mul(0x9E37_79B9)
            ^ (key.2 as u32).wrapping_mul(0x85EB_CA6B)
            ^ key.0 as u32;
        &self.shards[(hash >> 7) as usize % SHARDS]
    }

    fn path(&self, key: FileKey) -> PathBuf {
        let folder = match key.0 {
            Stream::Chunk => "region",
            Stream::Entities => "entities",
            Stream::Poi => "poi",
        };
        self.dir
            .join(folder)
            .join(format!("r.{}.{}.{}", key.1, key.2, self.format.extension()))
    }

    fn submit(
        self: &Arc<Self>,
        token: Token,
        ops: Vec<StoreOp>,
        priority: Priority,
        to: CompletionTarget,
    ) {
        let batch = Arc::new(Batch { token, to });
        let mut answers: Vec<(usize, StoreResult)> = Vec::new();
        let mut schedule: Vec<(FileKey, Priority)> = Vec::new();
        for (index, op) in ops.into_iter().enumerate() {
            let key = op.key();
            let fk = file_key(&key);
            let slot_index = region::slot(key.pos);
            let mut state = lock(&self.shard(fk).state);
            let state = &mut *state;
            match op {
                StoreOp::Get(_) | StoreOp::Meta(_) => {
                    let meta_only = matches!(op, StoreOp::Meta(_));
                    let pending = state.files.get(&fk).and_then(|f| f.writes.get(&slot_index));
                    if let Some(write) = pending {
                        self.counters.pending_hits.fetch_add(1, Ordering::Relaxed);
                        answers.push((index, answer(write.record.clone(), meta_only)));
                        continue;
                    }
                    if !meta_only && let Some(record) = state.cache.take((key.stream, key.pos)) {
                        self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
                        answers.push((index, answer(Some(record), false)));
                        continue;
                    }
                    let file = state.files.entry(fk).or_default();
                    if file.missing && !file.busy {
                        answers.push((index, StoreResult::Missing));
                        continue;
                    }
                    file.reads.push(PendingRead {
                        slot: slot_index,
                        meta_only,
                        priority,
                        batch: Arc::clone(&batch),
                        index,
                    });
                    if needs_job(file, priority) {
                        schedule.push((fk, priority));
                    }
                }
                StoreOp::Put { meta, bytes, .. } => {
                    let record = Some((meta, bytes));
                    self.queue_write(state, fk, &key, slot_index, record, &batch, index);
                    let file = state.files.entry(fk).or_default();
                    if needs_job(file, priority) {
                        schedule.push((fk, priority));
                    }
                }
                StoreOp::Delete(_) => {
                    self.queue_write(state, fk, &key, slot_index, None, &batch, index);
                    let file = state.files.entry(fk).or_default();
                    if needs_job(file, priority) {
                        schedule.push((fk, priority));
                    }
                }
            }
        }
        for (index, result) in answers {
            batch.to.post(token, index, result);
        }
        for (fk, priority) in schedule {
            self.pool.push(priority, Work::File(Arc::clone(self), fk));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_write(
        &self,
        state: &mut ShardState,
        fk: FileKey,
        key: &RecordKey,
        slot_index: usize,
        record: Option<Record>,
        batch: &Arc<Batch>,
        index: usize,
    ) {
        let seq = self.put_seq.fetch_add(1, Ordering::Relaxed) + 1;
        state.cache.take((key.stream, key.pos));
        let file = state.files.entry(fk).or_default();
        if record.is_some() {
            file.missing = false;
        }
        let added = record.as_ref().map_or(0, |r| r.1.len());
        self.backlog.fetch_add(added, Ordering::Relaxed);
        match file.writes.get_mut(&slot_index) {
            Some(write) => {
                self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
                if !write.failed {
                    self.backlog.fetch_sub(write.len(), Ordering::Relaxed);
                }
                write.seq = seq;
                write.record = record;
                write.failed = false;
                write.waiters.push((Arc::clone(batch), index));
            }
            None => {
                file.writes.insert(
                    slot_index,
                    PendingWrite {
                        seq,
                        record,
                        failed: false,
                        waiters: vec![(Arc::clone(batch), index)],
                    },
                );
            }
        }
    }

    fn cancel(&self, token: Token) {
        let mut cancelled = Vec::new();
        for shard in &self.shards {
            let mut state = lock(&shard.state);
            for file in state.files.values_mut() {
                file.reads.retain(|read| {
                    if read.batch.token == token {
                        cancelled.push((Arc::clone(&read.batch), read.index));
                        false
                    } else {
                        true
                    }
                });
            }
        }
        for (batch, index) in cancelled {
            batch.to.post(batch.token, index, StoreResult::Cancelled);
        }
    }

    /// Runs the queued ops of one file on this IO thread.
    fn run_file(self: &Arc<Self>, fk: FileKey) {
        let shard = self.shard(fk);
        let mut state = lock(&shard.state);
        let Some(file) = state.files.get_mut(&fk) else {
            return;
        };
        file.queued = None;
        if file.busy || !file.has_work() {
            return;
        }
        file.busy = true;
        file.last_use = self.use_clock.fetch_add(1, Ordering::Relaxed);
        let reads = std::mem::take(&mut file.reads);
        let writes: Vec<WriteJob> = file
            .writes
            .iter()
            .map(|(&slot, w)| (slot, w.seq, w.record.clone()))
            .collect();
        let region = file.region.take();
        drop(state);

        let path = self.path(fk);
        let opened = region.is_none();
        let region = match region {
            Some(region) => Ok(Some(region)),
            None => {
                let create = writes.iter().any(|w| w.2.is_some());
                if create && let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                OpenFile::open(self.format, &path, create)
            }
        };
        let (mut region, open_error) = match region {
            Ok(region) => (region, None),
            Err(e) => (None, Some(e)),
        };
        if opened && region.is_some() {
            self.open.fetch_add(1, Ordering::Relaxed);
        }
        let error = |e: &io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
        let base = ChunkPos::new(fk.1 * 32, fk.2 * 32);
        let pos_of =
            |slot: usize| ChunkPos::new(base.x + (slot & 31) as i32, base.z + (slot / 32) as i32);

        let mut written: Vec<(usize, u64, io::Result<()>)> = Vec::with_capacity(writes.len());
        for (slot, seq, record) in writes {
            let result = match (&mut region, &record, &open_error) {
                (_, _, Some(e)) => Err(error(e)),
                (Some(region), Some((meta, record)), None) => {
                    region.write(pos_of(slot), *meta, record)
                }
                (Some(region), None, None) => region.delete(pos_of(slot)),
                (None, _, None) => Ok(()),
            };
            written.push((slot, seq, result));
        }
        let mut read: Vec<region::RecordRead> = Vec::new();
        if !reads.is_empty() {
            match (&region, &open_error) {
                (_, Some(e)) => {
                    for r in &reads {
                        read.push(region::RecordRead {
                            slot: r.slot,
                            ahead: false,
                            meta: RecordMeta::default(),
                            record: Err(error(e)),
                        });
                    }
                }
                (None, None) => {}
                (Some(region), None) => {
                    let mut wanted: Vec<usize> = reads.iter().map(|r| r.slot).collect();
                    wanted.sort_unstable();
                    wanted.dedup();
                    let mut ahead = Vec::new();
                    if self.settings.read_ahead && self.settings.read_cache > 0 {
                        for &slot in &wanted {
                            let (tx, tz) = ((slot & 31) & !3, (slot / 32) & !3);
                            for dz in 0..4 {
                                for dx in 0..4 {
                                    let other = tx + dx + (tz + dz) * 32;
                                    if wanted.binary_search(&other).is_err() {
                                        ahead.push(other);
                                    }
                                }
                            }
                        }
                        ahead.sort_unstable();
                        ahead.dedup();
                    }
                    let (syscalls, bytes) =
                        region.read_many((fk.1, fk.2), &wanted, &ahead, &mut read);
                    self.counters
                        .reads
                        .fetch_add(syscalls as u64, Ordering::Relaxed);
                    self.counters
                        .bytes_read
                        .fetch_add(bytes as u64, Ordering::Relaxed);
                    let records = read
                        .iter()
                        .filter(|r| matches!(r.record, Ok(Some(_))))
                        .count();
                    self.counters
                        .records_read
                        .fetch_add(records as u64, Ordering::Relaxed);
                }
            }
        }

        let mut answers: Vec<(Arc<Batch>, usize, StoreResult)> = Vec::new();
        let mut guard = lock(&shard.state);
        let state = &mut *guard;
        let file = state
            .files
            .get_mut(&fk)
            .expect("a busy file stays in the table");
        file.busy = false;
        file.missing = region.is_none() && open_error.is_none();
        file.region = region;
        let mut failed: Option<(usize, io::Error)> = None;
        for (slot, seq, result) in written {
            if !file.writes.get(&slot).is_some_and(|w| w.seq == seq) {
                // A newer put of the key arrived meanwhile; it is written next.
                file.dirty |= result.is_ok();
                continue;
            }
            if let Err(e) = &result {
                // Kept (#167): the record is the only copy of the chunk once it unloads.
                let write = file.writes.get_mut(&slot).expect("checked above");
                if !write.failed {
                    write.failed = true;
                    self.backlog.fetch_sub(write.len(), Ordering::Relaxed);
                }
                for (batch, index) in write.waiters.drain(..) {
                    let error = io::Error::new(e.kind(), e.to_string());
                    answers.push((batch, index, StoreResult::Failed(error)));
                }
                let first =
                    failed.get_or_insert_with(|| (0, io::Error::new(e.kind(), e.to_string())));
                first.0 += 1;
                continue;
            }
            let Some(write) = file.writes.remove(&slot) else {
                continue;
            };
            self.backlog.fetch_sub(write.len(), Ordering::Relaxed);
            if result.is_ok() {
                file.dirty = true;
                self.counters
                    .records_written
                    .fetch_add(1, Ordering::Relaxed);
            }
            for (batch, index) in write.waiters {
                let result = match &result {
                    Ok(()) => StoreResult::Done,
                    Err(e) => StoreResult::Failed(io::Error::new(e.kind(), e.to_string())),
                };
                answers.push((batch, index, result));
            }
        }
        let mut found: FxHashMap<usize, ReadOutcome> = FxHashMap::default();
        for r in read {
            let meta = r.meta;
            let record = r.record.map(|r| r.map(|bytes| (meta, Arc::from(bytes))));
            if r.ahead {
                if let Ok(Some(record)) = record
                    && !file.writes.contains_key(&r.slot)
                {
                    state.cache.insert((fk.0, pos_of(r.slot)), record);
                }
                continue;
            }
            found.insert(r.slot, record.map_err(|e| (e.kind(), e.to_string())));
        }
        for r in reads {
            let result = match found.get(&r.slot) {
                None | Some(Ok(None)) => StoreResult::Missing,
                Some(Ok(Some((meta, bytes)))) => {
                    answer(Some((*meta, Arc::clone(bytes))), r.meta_only)
                }
                Some(Err((kind, message))) => {
                    StoreResult::Failed(io::Error::new(*kind, message.clone()))
                }
            };
            answers.push((r.batch, r.index, result));
        }
        let requeue = file.has_work().then(|| file.best_priority());
        if let Some(priority) = requeue {
            file.queued = Some(priority);
        }
        drop(guard);
        shard.idle.notify_all();
        if let Some((count, e)) = failed {
            log::error!(
                "Couldn't write {count} records: {e}; they stay in memory and are written again \
                 with the next save"
            );
        }
        for (batch, index, result) in answers {
            batch.to.post(batch.token, index, result);
        }
        if let Some(priority) = requeue {
            self.pool.push(priority, Work::File(Arc::clone(self), fk));
        }
        if opened && self.open.load(Ordering::Relaxed) > self.settings.max_open_files {
            self.evict();
        }
    }

    /// Closes the least recently used idle files until a quarter of the limit is free again.
    fn evict(self: &Arc<Self>) {
        self.evict_to(self.settings.max_open_files * 3 / 4);
    }

    /// Closes every idle file and drops the records read ahead (nobody plays in the
    /// dimension for a while).
    fn release_memory(self: &Arc<Self>) {
        self.evict_to(0);
        for shard in &self.shards {
            let mut state = lock(&shard.state);
            state.cache.clear();
            state
                .files
                .retain(|_, file| !(file.idle() && file.region.is_none() && !file.dirty));
            state.files.shrink_to_fit();
        }
    }

    /// Closes the least recently used idle files until at most `target` are open. Files
    /// with queued work or an IO thread on them stay open. A file is made durable before it
    /// leaves the table, so the next instance of it sees every write.
    fn evict_to(self: &Arc<Self>, target: usize) {
        let open = self.open.load(Ordering::Relaxed);
        if open <= target {
            return;
        }
        let mut candidates: Vec<(u64, usize, FileKey)> = Vec::new();
        for (index, shard) in self.shards.iter().enumerate() {
            let state = lock(&shard.state);
            for (&fk, file) in &state.files {
                if file.idle() && file.region.is_some() {
                    candidates.push((file.last_use, index, fk));
                }
            }
        }
        candidates.sort_unstable();
        for (_, index, fk) in candidates.into_iter().take(open - target) {
            let shard = &self.shards[index];
            let taken = {
                let mut state = lock(&shard.state);
                match state.files.get_mut(&fk) {
                    Some(file) if file.idle() => {
                        // `dirty` stays set until the commit is done: a commit round that
                        // meets the busy file waits for it instead of finishing early.
                        file.busy = true;
                        file.region.take().map(|r| (r, file.dirty))
                    }
                    _ => None,
                }
            };
            let Some((mut region, dirty)) = taken else {
                continue;
            };
            let result = if dirty {
                region.commit(self.settings.sync)
            } else {
                Ok(())
            };
            if let Err(e) = &result {
                log::error!("Couldn't sync {}: {e}", region.name());
            }
            let mut state = lock(&shard.state);
            let file = state
                .files
                .get_mut(&fk)
                .expect("a busy file stays in the table");
            file.busy = false;
            file.dirty = result.is_err();
            let requeue = if file.has_work() || result.is_err() {
                // Work arrived meanwhile, or the file must stay open to be synced again.
                file.region = Some(region);
                (file.has_work() && file.queued.is_none()).then(|| file.best_priority())
            } else {
                state.files.remove(&fk);
                self.open.fetch_sub(1, Ordering::Relaxed);
                drop(region);
                None
            };
            if let Some(priority) = requeue
                && let Some(file) = state.files.get_mut(&fk)
            {
                file.queued = Some(priority);
            }
            drop(state);
            shard.idle.notify_all();
            if let Some(priority) = requeue {
                self.pool.push(priority, Work::File(Arc::clone(self), fk));
            }
        }
    }

    /// Writes every put with a sequence number up to `upto`, then syncs the touched files.
    fn run_commit(self: &Arc<Self>, upto: u64) -> io::Result<()> {
        let mut first_error = None;
        // Files whose commit failed in this round: they stay dirty for the next round.
        let mut failed: Vec<FileKey> = Vec::new();
        // Writes that failed before are tried once more in this round.
        for shard in &self.shards {
            let mut state = lock(&shard.state);
            for file in state.files.values_mut() {
                for write in file.writes.values_mut() {
                    if write.failed && write.seq <= upto {
                        write.failed = false;
                        self.backlog.fetch_add(write.len(), Ordering::Relaxed);
                    }
                }
            }
        }
        for shard in &self.shards {
            loop {
                let mut state = lock(&shard.state);
                let pending = state.files.iter().find_map(|(&fk, f)| {
                    f.writes
                        .values()
                        .any(|w| w.seq <= upto && !w.failed)
                        .then_some((fk, f.busy))
                });
                if let Some((fk, busy)) = pending {
                    if busy {
                        drop(
                            shard
                                .idle
                                .wait(state)
                                .unwrap_or_else(PoisonError::into_inner),
                        );
                    } else {
                        drop(state);
                        self.run_file(fk);
                    }
                    continue;
                }
                let dirty = state
                    .files
                    .iter()
                    .find(|(fk, f)| {
                        f.dirty && (f.region.is_some() || f.busy) && !failed.contains(fk)
                    })
                    .map(|(&fk, f)| (fk, f.busy));
                let Some((fk, busy)) = dirty else {
                    break;
                };
                if busy {
                    drop(
                        shard
                            .idle
                            .wait(state)
                            .unwrap_or_else(PoisonError::into_inner),
                    );
                    continue;
                }
                // `dirty` stays set until the commit is done, so a concurrent round waits.
                let file = state.files.get_mut(&fk).expect("found above");
                file.busy = true;
                let mut region = file.region.take().expect("dirty files are open");
                drop(state);
                let result = region.commit(self.settings.sync);
                let ok = result.is_ok();
                if let Err(e) = result {
                    log::error!("Couldn't sync {}: {e}", self.path(fk).display());
                    first_error.get_or_insert(e);
                    failed.push(fk);
                }
                let mut state = lock(&shard.state);
                let file = state
                    .files
                    .get_mut(&fk)
                    .expect("a busy file stays in the table");
                file.busy = false;
                file.dirty = !ok;
                file.region = Some(region);
                let requeue =
                    (file.has_work() && file.queued.is_none()).then(|| file.best_priority());
                if let Some(priority) = requeue {
                    file.queued = Some(priority);
                }
                drop(state);
                shard.idle.notify_all();
                if let Some(priority) = requeue {
                    self.pool.push(priority, Work::File(Arc::clone(self), fk));
                }
            }
        }
        let unwritten: usize = self
            .shards
            .iter()
            .map(|shard| {
                lock(&shard.state)
                    .files
                    .values()
                    .map(|f| f.writes.values().filter(|w| w.failed).count())
                    .sum::<usize>()
            })
            .sum();
        if unwritten > 0 && first_error.is_none() {
            first_error = Some(io::Error::other(format!(
                "{unwritten} records of {} could not be written; they stay in memory and are \
                 written again with the next save",
                self.dir.display()
            )));
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Whether a file needs a new job for an op of `priority`; marks it queued.
fn needs_job(file: &mut FileSlot, priority: Priority) -> bool {
    if file.busy {
        // The IO thread requeues the file when it is done.
        return false;
    }
    match file.queued {
        Some(queued) if queued <= priority => false,
        _ => {
            file.queued = Some(priority);
            true
        }
    }
}

fn answer(record: Option<Record>, meta_only: bool) -> StoreResult {
    match record {
        None => StoreResult::Missing,
        Some((meta, bytes)) => {
            if meta_only {
                StoreResult::Meta(meta)
            } else {
                StoreResult::Record(meta, bytes)
            }
        }
    }
}

impl ChunkStore for RegionStore {
    fn submit(&self, token: Token, ops: Vec<StoreOp>, priority: Priority, to: CompletionTarget) {
        self.inner.submit(token, ops, priority, to);
    }

    fn cancel(&self, token: Token) {
        self.inner.cancel(token);
    }

    fn commit(&self, round: u64, to: CompletionTarget) {
        let upto = self.inner.put_seq.load(Ordering::Relaxed);
        self.inner.pool.push(
            Priority::Background,
            Work::Commit {
                store: Arc::clone(&self.inner),
                round,
                to,
                upto,
            },
        );
    }

    fn backlog(&self) -> usize {
        self.inner.backlog.load(Ordering::Relaxed)
    }

    fn release_memory(&self) {
        self.inner
            .pool
            .push(Priority::Background, Work::Release(Arc::clone(&self.inner)));
    }
}

/// Provider of [`RegionStore`]s on one [`IoPool`].
#[derive(Clone, Debug)]
pub struct RegionStoreProvider {
    /// The world format of the files.
    pub format: WorldFormat,
    /// The IO threads.
    pub pool: IoPool,
    /// Settings of every store.
    pub settings: StoreSettings,
}

impl ChunkStoreProvider for RegionStoreProvider {
    fn create(&self, world: &WorldInfo, _: DimensionId) -> io::Result<Arc<dyn ChunkStore>> {
        Ok(Arc::new(RegionStore::new(
            world.dir.clone(),
            self.format,
            self.settings,
            self.pool.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocking;
    use crate::compression::Compression;
    use vanillite_chunk::store::Completion;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vanillite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key(pos: ChunkPos) -> RecordKey {
        RecordKey::chunk(DimensionId(0), pos)
    }

    /// A record whose bytes depend on the chunk and a version.
    fn record(pos: ChunkPos, version: u8) -> Arc<[u8]> {
        let tag = (pos.x as u8).wrapping_mul(31) ^ (pos.z as u8) ^ version.wrapping_mul(17);
        let mut bytes = vec![tag; 3000 + (pos.x as usize & 7) * 1000 + usize::from(version)];
        bytes[0] = Compression::None.id();
        Arc::from(bytes)
    }

    /// A get right after a put returns the new bytes, whether the write is still queued or
    /// already on disk; a commit makes them durable.
    #[test]
    fn read_your_writes() {
        read_your_writes_in(WorldFormat::Anvil);
        read_your_writes_in(WorldFormat::Vanillite);
    }

    fn read_your_writes_in(format: WorldFormat) {
        let dir = temp_dir(&format!("store-ryw-{}", format.name()));
        let pool = IoPool::new(2).unwrap();
        let store = RegionStore::new(dir.clone(), format, StoreSettings::default(), pool.clone());
        let pos = ChunkPos::new(3, -7);
        for version in 0..200u8 {
            let (to, inbox) = CompletionTarget::channel();
            store.submit(
                Token(1),
                vec![StoreOp::Put {
                    key: key(pos),
                    meta: RecordMeta::default(),
                    bytes: record(pos, version),
                }],
                Priority::Background,
                to.clone(),
            );
            store.submit(Token(2), vec![StoreOp::Get(key(pos))], Priority::Urgent, to);
            let mut got = None;
            for _ in 0..2 {
                if let Completion::Op {
                    token: Token(2),
                    result,
                    ..
                } = inbox.recv().unwrap()
                {
                    got = Some(result);
                }
            }
            match got {
                Some(StoreResult::Record(_, bytes)) => assert_eq!(bytes, record(pos, version)),
                other => panic!("version {version}: {other:?}"),
            }
        }
        assert!(store.stats().pending_hits > 0, "{:?}", store.stats());
        blocking::commit(&store).unwrap();
        let reopened =
            RegionStore::new(dir.clone(), format, StoreSettings::default(), pool.clone());
        assert_eq!(
            blocking::get(&reopened, &[key(pos)])
                .pop()
                .unwrap()
                .unwrap(),
            Some(record(pos, 199))
        );
        pool.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A write that fails (disk full, read-only, here a file where the folder must be) is
    /// answered as failed but kept: reads get it, it stays out of the backlog, and a later
    /// commit writes it once the disk takes it again (#167).
    #[test]
    fn failed_writes_are_kept_and_retried() {
        for format in [WorldFormat::Anvil, WorldFormat::Vanillite] {
            let dir = temp_dir(&format!("store-failed-{}", format.name()));
            std::fs::write(dir.join("region"), b"in the way").unwrap();
            let pool = IoPool::new(2).unwrap();
            let store =
                RegionStore::new(dir.clone(), format, StoreSettings::default(), pool.clone());
            let pos = ChunkPos::new(-40, 17);
            let (to, inbox) = CompletionTarget::channel();
            store.submit(
                Token(1),
                vec![StoreOp::Put {
                    key: key(pos),
                    meta: RecordMeta::default(),
                    bytes: record(pos, 1),
                }],
                Priority::Background,
                to,
            );
            match inbox.recv().unwrap() {
                Completion::Op {
                    result: StoreResult::Failed(_),
                    ..
                } => {}
                other => panic!("{format:?}: {other:?}"),
            }
            assert_eq!(store.backlog(), 0);
            assert_eq!(
                blocking::get(&store, &[key(pos)]).pop().unwrap().unwrap(),
                Some(record(pos, 1))
            );
            assert!(blocking::commit(&store).is_err());
            // The disk takes writes again: the next commit writes the kept record.
            std::fs::remove_file(dir.join("region")).unwrap();
            blocking::commit(&store).unwrap();
            let reopened =
                RegionStore::new(dir.clone(), format, StoreSettings::default(), pool.clone());
            assert_eq!(
                blocking::get(&reopened, &[key(pos)])
                    .pop()
                    .unwrap()
                    .unwrap(),
                Some(record(pos, 1))
            );
            pool.shutdown();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Writes that fail on the disk itself (run under a file size limit, `ulimit -f 512` with
    /// `SIGXFSZ` ignored, and `VANILLITE_FSIZE_LIMITED` set; skipped otherwise): region files
    /// cannot grow, writes fail midway, every record stays readable from memory, and a new
    /// store finds the last version that was written of every chunk undamaged (#167).
    #[test]
    fn writes_failing_on_the_disk_damage_nothing() {
        if std::env::var_os("VANILLITE_FSIZE_LIMITED").is_none() {
            return;
        }
        for format in [WorldFormat::Anvil, WorldFormat::Vanillite] {
            let dir = temp_dir(&format!("store-fsize-{}", format.name()));
            let pool = IoPool::new(2).unwrap();
            let store =
                RegionStore::new(dir.clone(), format, StoreSettings::default(), pool.clone());
            let mut latest: FxHashMap<ChunkPos, u8> = FxHashMap::default();
            let mut written: FxHashMap<ChunkPos, u8> = FxHashMap::default();
            let mut failures = 0;
            for i in 0..600 {
                let pos = ChunkPos::new(i % 24, (i / 24) % 8);
                let version = (i / 24) as u8;
                let result = blocking::put(&store, vec![(key(pos), record(pos, version))]).pop();
                latest.insert(pos, version);
                match result.unwrap() {
                    Ok(()) => {
                        written.insert(pos, version);
                    }
                    Err(e) => {
                        assert_eq!(e.kind(), io::ErrorKind::FileTooLarge, "{format:?}: {e}");
                        failures += 1;
                    }
                }
                if i % 16 == 15 {
                    let _ = blocking::commit(&store);
                }
            }
            assert!(failures > 0, "{format:?}: no write failed");
            for (&pos, &version) in &latest {
                assert_eq!(
                    blocking::get(&store, &[key(pos)]).pop().unwrap().unwrap(),
                    Some(record(pos, version)),
                    "{format:?} {pos:?} from memory"
                );
            }
            assert!(blocking::commit(&store).is_err());
            let reopened =
                RegionStore::new(dir.clone(), format, StoreSettings::default(), pool.clone());
            for (&pos, &version) in &written {
                assert_eq!(
                    blocking::get(&reopened, &[key(pos)])
                        .pop()
                        .unwrap()
                        .unwrap(),
                    Some(record(pos, version)),
                    "{format:?} {pos:?} on disk"
                );
            }
            pool.shutdown();
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// A process killed while it saves (`VANILLITE_KILL_TEST=<dir>`; skipped otherwise): with
    /// `VANILLITE_KILL_MODE=write` it saves and commits until it is killed (`kill -9`), with
    /// `check` every chunk in the files must be one complete version (#167).
    #[test]
    fn killed_while_saving() {
        let Some(root) = std::env::var_os("VANILLITE_KILL_TEST").map(PathBuf::from) else {
            return;
        };
        let write = std::env::var("VANILLITE_KILL_MODE").as_deref() == Ok("write");
        for format in [WorldFormat::Anvil, WorldFormat::Vanillite] {
            let dir = root.join(format.name());
            let pool = IoPool::new(2).unwrap();
            let store =
                RegionStore::new(dir.clone(), format, StoreSettings::default(), pool.clone());
            let positions: Vec<ChunkPos> = (0..256)
                .map(|i| ChunkPos::new(i % 40 - 20, i / 40 - 3))
                .collect();
            if write {
                std::thread::spawn(move || {
                    for version in (0..200u8).cycle() {
                        let ops = positions
                            .iter()
                            .map(|&pos| (key(pos), record(pos, version)))
                            .collect();
                        let _ = blocking::put(&store, ops);
                        if version % 3 == 0 {
                            let _ = blocking::commit(&store);
                        }
                    }
                });
                continue;
            }
            let mut found = 0;
            for (pos, got) in positions.iter().zip(blocking::get(
                &store,
                &positions.iter().map(|&p| key(p)).collect::<Vec<_>>(),
            )) {
                let Some(bytes) = got.unwrap() else {
                    continue;
                };
                found += 1;
                let version = bytes.len() - 3000 - (pos.x as usize & 7) * 1000;
                assert_eq!(bytes, record(*pos, version as u8), "{format:?} {pos:?}");
            }
            assert!(found > 0, "{format:?}: nothing was saved");
            pool.shutdown();
        }
        if write {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    /// A damaged own-format file is kept aside when it is written to; a new one takes the
    /// writes (#167).
    #[test]
    fn damaged_own_files_are_kept_aside() {
        let dir = temp_dir("store-damaged-vwr");
        std::fs::create_dir_all(dir.join("region")).unwrap();
        let path = dir.join("region").join("r.0.0.vwr");
        std::fs::write(&path, vec![7u8; 100_000]).unwrap();
        let pool = IoPool::new(1).unwrap();
        let store = RegionStore::new(
            dir.clone(),
            WorldFormat::Vanillite,
            StoreSettings::default(),
            pool.clone(),
        );
        let pos = ChunkPos::new(2, 3);
        assert!(blocking::get(&store, &[key(pos)]).pop().unwrap().is_err());
        blocking::put(&store, vec![(key(pos), record(pos, 1))])
            .pop()
            .unwrap()
            .unwrap();
        blocking::commit(&store).unwrap();
        let kept: Vec<_> = std::fs::read_dir(dir.join("region"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("r.0.0.vwr.damaged-"))
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            std::fs::read(dir.join("region").join(&kept[0])).unwrap(),
            vec![7u8; 100_000]
        );
        assert_eq!(
            blocking::get(&store, &[key(pos)]).pop().unwrap().unwrap(),
            Some(record(pos, 1))
        );
        pool.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A write that never reached the header (a crash between the record and its header
    /// entry) leaves the old record readable: replaced sectors are not reused before a sync.
    #[test]
    fn crash_keeps_the_old_record() {
        let dir = temp_dir("store-crash");
        let path = dir.join("r.0.0.mca");
        let pos = ChunkPos::new(5, 9);
        let mut region = RegionFile::open(&path, true).unwrap().unwrap();
        region.write_record(pos, &record(pos, 1)).unwrap();
        region.sync().unwrap();
        let header = std::fs::read(&path).unwrap()[..8192].to_vec();
        // Two more versions before the next sync: neither may land on the synced sectors.
        region.write_record(pos, &record(pos, 2)).unwrap();
        region.write_record(pos, &record(pos, 3)).unwrap();
        drop(region);
        // The crash: the new header entry is lost, the records were written.
        let mut file = std::fs::read(&path).unwrap();
        file[..8192].copy_from_slice(&header);
        std::fs::write(&path, &file).unwrap();
        let region = RegionFile::open(&path, false).unwrap().unwrap();
        assert_eq!(
            region.read_record(pos).unwrap().as_deref(),
            Some(&record(pos, 1)[..])
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Many threads writing into far more region files than stay open, then reading them back
    /// from many threads and from a fresh store: every chunk is intact (#50).
    #[test]
    fn many_threads_many_regions() {
        many_threads_many_regions_in(WorldFormat::Anvil);
        many_threads_many_regions_in(WorldFormat::Vanillite);
    }

    /// Own-format chunks with many biomes (and unused biome palette entries) written from many
    /// threads into more regions than stay open, committed, and read back by a fresh store:
    /// every chunk decodes to what was saved (#142, #133).
    #[test]
    fn own_format_chunks_reload_from_many_threads() {
        use crate::chunk_nbt::tests::{codec, golden};
        use crate::own_codec::tests::with_unused_biomes;
        use crate::own_codec::{DEFAULT_LEVEL, OwnCodec};

        let chunks = golden();
        let anvil = codec(&chunks);
        let own = OwnCodec::new(anvil.info(), DEFAULT_LEVEL);
        let biomes = anvil.info().biomes.len();
        let base: Vec<_> = chunks
            .iter()
            .map(|(pos, bytes, _)| anvil.read_bytes(*pos, bytes).unwrap())
            .collect();
        let (regions, per_region, threads) = (12, 6, 8);
        let pos = |n: i32| ChunkPos::new((n % regions - 6) * 32 + n / regions * 5, n % 3 * 32 - 40);
        let chunk_at = |n: i32| {
            let mut chunk = base[n as usize % base.len()].clone();
            chunk.pos = pos(n);
            with_unused_biomes(&mut chunk, biomes, n as usize);
            chunk
        };
        let count = regions * per_region;
        let dir = temp_dir("store-own-biomes");
        let pool = IoPool::new(4).unwrap();
        let settings = StoreSettings {
            max_open_files: 4,
            ..StoreSettings::default()
        };
        let store = RegionStore::new(dir.clone(), WorldFormat::Vanillite, settings, pool.clone());
        std::thread::scope(|scope| {
            for t in 0..threads {
                let (store, own) = (&store, &own);
                scope.spawn(move || {
                    for n in (t..count).step_by(threads as usize) {
                        let chunk = chunk_at(n);
                        let results = blocking::save(store, own, DimensionId(0), &[&chunk]);
                        assert!(results.iter().all(Result::is_ok), "{results:?}");
                    }
                });
            }
        });
        blocking::commit(&store).unwrap();
        let fresh = RegionStore::new(dir.clone(), WorldFormat::Vanillite, settings, pool.clone());
        let positions: Vec<ChunkPos> = (0..count).map(pos).collect();
        for (n, got) in (0..count).zip(blocking::load(&fresh, &own, DimensionId(0), &positions)) {
            let back = got.unwrap().expect("a saved chunk");
            assert_eq!(
                own.encode_raw(&back).1,
                own.encode_raw(&chunk_at(n)).1,
                "{n}"
            );
        }
        pool.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn many_threads_many_regions_in(format: WorldFormat) {
        let dir = temp_dir(&format!("store-many-{}", format.name()));
        let pool = IoPool::new(4).unwrap();
        let settings = StoreSettings {
            max_open_files: 4,
            read_cache: 1 << 20,
            ..StoreSettings::default()
        };
        let store = RegionStore::new(dir.clone(), format, settings, pool.clone());
        let (files, threads, per_file) = (64, 16, 24);
        let pos =
            |file: i32, i: i32| ChunkPos::new((file % 8 - 4) * 32 + i, (file / 8 - 4) * 32 + i);
        std::thread::scope(|scope| {
            for t in 0..threads {
                let store = &store;
                scope.spawn(move || {
                    // Each file gets chunks from several threads, interleaved with other files;
                    // every chunk is written twice, the second version must win.
                    for version in 0..2 {
                        for i in 0..per_file {
                            let records = (0..files)
                                .filter(|f| (f + i) % threads == t)
                                .map(|file| (key(pos(file, i)), record(pos(file, i), version)))
                                .collect();
                            let results = blocking::put(store, records);
                            assert!(results.iter().all(Result::is_ok), "{results:?}");
                        }
                    }
                });
            }
        });
        let check = |store: &RegionStore| {
            std::thread::scope(|scope| {
                for t in 0..threads {
                    scope.spawn(move || {
                        let keys: Vec<RecordKey> = (0..files)
                            .flat_map(|file| (0..per_file).map(move |i| pos(file, i)))
                            .filter(|p| (p.x + p.z).rem_euclid(threads) == t)
                            .map(key)
                            .collect();
                        for (key, got) in keys.iter().zip(blocking::get(store, &keys)) {
                            assert_eq!(
                                got.unwrap().as_deref(),
                                Some(&record(key.pos, 1)[..]),
                                "chunk {:?}",
                                key.pos
                            );
                        }
                    });
                }
            });
        };
        check(&store);
        blocking::commit(&store).unwrap();
        assert!(store.open_files() <= 64, "{}", store.open_files());
        check(&RegionStore::new(
            dir.clone(),
            format,
            settings,
            pool.clone(),
        ));
        pool.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
