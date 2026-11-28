//! The `chunk-storage` slot (#47, decision D1): an asynchronous record store.
//!
//! A store keeps bytes plus small metadata per key (dimension, chunk position, stream). It knows
//! nothing about chunks: the core turns records into [`ProtoChunk`]s and back with a
//! [`ChunkCodec`] on the compute pool, so a provider (a database, network storage, an in-memory
//! store for tests) stays small.
//!
//! Requests are batches with a [`Token`]. Any thread may submit; every result arrives as a
//! [`Completion`] in the requester's inbox ([`CompletionTarget`]). A store never runs owner code
//! on its own threads, so owners (a world thread today, a region thread later) install results
//! between their ticks.
//!
//! Contract rules for every provider:
//! - read-your-writes per key: a `Get` after a `Put` of the same key returns the new bytes, also
//!   while the write is still queued;
//! - `Put`s of one key take effect in submission order;
//! - every op of a submitted batch gets exactly one result (also [`StoreResult::Cancelled`]);
//! - `commit(round)` covers everything submitted before it, and repeating it does no harm.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use vanillite_base::ChunkPos;

use crate::slots::{ProtoChunk, WorldInfo};

/// A dimension of the server (process-local index). One store instance serves one dimension;
/// providers that keep several dimensions in one backend map it with the dimension name of the
/// [`WorldInfo`] they were created with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DimensionId(pub u16);

/// The kind of data of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Stream {
    /// Blocks, biomes, light, block entities, scheduled ticks, structures (`region/`).
    Chunk = 0,
    /// The entities of a chunk (`entities/`).
    Entities = 1,
    /// Points of interest (`poi/`).
    Poi = 2,
}

/// The key of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordKey {
    /// The dimension.
    pub dimension: DimensionId,
    /// The chunk.
    pub pos: ChunkPos,
    /// The kind of data.
    pub stream: Stream,
}

impl RecordKey {
    /// The chunk record of `pos`.
    pub const fn chunk(dimension: DimensionId, pos: ChunkPos) -> Self {
        RecordKey {
            dimension,
            pos,
            stream: Stream::Chunk,
        }
    }

    /// The entities record of `pos`.
    pub const fn entities(dimension: DimensionId, pos: ChunkPos) -> Self {
        RecordKey {
            dimension,
            pos,
            stream: Stream::Entities,
        }
    }

    /// The points of interest record of `pos`.
    pub const fn poi(dimension: DimensionId, pos: ChunkPos) -> Self {
        RecordKey {
            dimension,
            pos,
            stream: Stream::Poi,
        }
    }
}

/// Metadata kept next to a record. Fields a format does not store are 0
/// ([`RecordMeta::STATUS_UNKNOWN`] for the status).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct RecordMeta {
    /// Minecraft data version the record was written with.
    pub data_version: u32,
    /// Length of the record before compression.
    pub raw_len: u32,
    /// Checksum of the stored bytes; checked by the core, 0 = none.
    pub checksum: u32,
    /// [`ChunkStatus`](crate::ChunkStatus) of a chunk record.
    pub status: u8,
    /// Format flags.
    pub flags: u8,
}

impl RecordMeta {
    /// `status` of a record whose status the store does not know without reading it.
    pub const STATUS_UNKNOWN: u8 = u8::MAX;
}

/// Identifies a submitted batch; chosen by the requester.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Token(pub u64);

/// Request order within a dimension: class first, then ticket level, then age.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Priority {
    /// The chunk a player stands in after join, teleport or respawn.
    Urgent,
    /// Loads for player views, by ticket level (lower first).
    View(u8),
    /// Records read ahead of moving players.
    ReadAhead,
    /// Unload saves, save rounds, conversion, compaction, backups.
    Background,
}

/// One operation of a batch.
#[derive(Clone, Debug)]
pub enum StoreOp {
    /// Reads a record.
    Get(RecordKey),
    /// Reads the metadata of a record.
    Meta(RecordKey),
    /// Writes a record.
    Put {
        /// The record.
        key: RecordKey,
        /// Its metadata.
        meta: RecordMeta,
        /// Its bytes.
        bytes: Arc<[u8]>,
    },
    /// Removes a record.
    Delete(RecordKey),
}

impl StoreOp {
    /// The key the op works on.
    pub fn key(&self) -> RecordKey {
        match self {
            StoreOp::Get(key) | StoreOp::Meta(key) | StoreOp::Delete(key) => *key,
            StoreOp::Put { key, .. } => *key,
        }
    }
}

/// The result of one operation.
#[derive(Debug)]
pub enum StoreResult {
    /// A `Get` found the record.
    Record(RecordMeta, Arc<[u8]>),
    /// A `Meta` found the record.
    Meta(RecordMeta),
    /// A `Get` or `Meta` found nothing.
    Missing,
    /// A `Put` or `Delete` is done (written or superseded by a later `Put` of the same key).
    Done,
    /// The op was dropped by [`ChunkStore::cancel`] before it started.
    Cancelled,
    /// The op failed.
    Failed(io::Error),
}

/// A message in an owner's inbox.
#[derive(Debug)]
pub enum Completion {
    /// The result of op `index` of batch `token`.
    Op {
        /// The batch.
        token: Token,
        /// The op's index in the batch.
        index: usize,
        /// Its result.
        result: StoreResult,
    },
    /// A commit finished.
    Commit {
        /// The round.
        round: u64,
        /// Whether everything submitted before it is durable.
        result: io::Result<()>,
    },
}

/// Wakes an inbox's owner after a result was posted; must not block. Owners batch it (one wake
/// per pump), so providers may call it per result.
pub type InboxWaker = Arc<dyn Fn() + Send + Sync>;

/// The requesting owner's inbox. Posting only enqueues a message (and wakes the owner if it set
/// a waker); no owner code runs on the provider's thread.
#[derive(Clone)]
pub struct CompletionTarget {
    tx: Sender<Completion>,
    waker: Option<InboxWaker>,
}

impl std::fmt::Debug for CompletionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionTarget")
            .field("waker", &self.waker.is_some())
            .finish_non_exhaustive()
    }
}

impl CompletionTarget {
    /// A new inbox and the receiver its owner drains.
    pub fn channel() -> (CompletionTarget, Receiver<Completion>) {
        let (tx, rx) = mpsc::channel();
        (CompletionTarget { tx, waker: None }, rx)
    }

    /// A new inbox whose owner is woken by `waker` after every posted result, so it installs
    /// results between its ticks instead of at the next one.
    pub fn with_waker(waker: InboxWaker) -> (CompletionTarget, Receiver<Completion>) {
        let (tx, rx) = mpsc::channel();
        (
            CompletionTarget {
                tx,
                waker: Some(waker),
            },
            rx,
        )
    }

    fn send(&self, completion: Completion) {
        // A dropped inbox does not want results any more.
        if self.tx.send(completion).is_ok()
            && let Some(waker) = &self.waker
        {
            waker();
        }
    }

    /// Posts the result of op `index` of batch `token`. A dropped inbox is ignored.
    pub fn post(&self, token: Token, index: usize, result: StoreResult) {
        self.send(Completion::Op {
            token,
            index,
            result,
        });
    }

    /// Posts the end of a commit.
    pub fn post_commit(&self, round: u64, result: io::Result<()>) {
        self.send(Completion::Commit { round, result });
    }
}

/// The `chunk-storage` slot: an asynchronous record store of one dimension.
pub trait ChunkStore: Send + Sync {
    /// Queues a batch. Every op gets one result in `to`. Later `Get`s see earlier `Put`s of the
    /// same key (read-your-writes).
    fn submit(&self, token: Token, ops: Vec<StoreOp>, priority: Priority, to: CompletionTarget);

    /// Drops the queued ops of batch `token` that did not start; they report
    /// [`StoreResult::Cancelled`]. Ops that started finish normally.
    fn cancel(&self, token: Token);

    /// Makes every `Put` submitted before the call durable as round `round`. Default: reports
    /// success at once; durability is then the provider's own (a database's transactions, an
    /// in-memory store).
    fn commit(&self, round: u64, to: CompletionTarget) {
        to.post_commit(round, Ok(()));
    }

    /// A consistent read view of a committed round (backup, export). Default: unsupported; such
    /// worlds get backups and exports only after `save-all flush`.
    fn pin(&self, _round: u64) -> io::Result<Box<dyn RoundView>> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// Bytes of `Put`s waiting for IO. Owners stop queueing saves while it is above their
    /// limit, so RAM stays bounded.
    fn backlog(&self) -> usize {
        0
    }

    /// Gives back what the store keeps for speed (open files, records read ahead); nobody
    /// plays in the dimension for a while. Default: nothing.
    fn release_memory(&self) {}
}

/// A pinned read view of a committed round.
pub trait RoundView: Send {
    /// Every record of a stream with its metadata.
    fn keys(&self, stream: Stream, out: &mut Vec<(ChunkPos, RecordMeta)>);
    /// Reads records; `None` for missing ones.
    fn read(&self, keys: &[RecordKey], out: &mut Vec<io::Result<Option<Arc<[u8]>>>>);
}

/// Opens the store of each dimension.
pub trait ChunkStoreProvider: Send + Sync {
    /// The store of one dimension.
    fn create(&self, world: &WorldInfo, dimension: DimensionId) -> io::Result<Arc<dyn ChunkStore>>;
}

/// Turns chunk records into chunks and back (one per world format). Runs on the compute pool;
/// version upgrades live here.
pub trait ChunkCodec: Send + Sync {
    /// Decodes a chunk record.
    fn decode(&self, pos: ChunkPos, meta: &RecordMeta, bytes: &[u8]) -> io::Result<ProtoChunk>;

    /// Encodes a chunk into a record.
    fn encode(&self, chunk: &ProtoChunk) -> io::Result<(RecordMeta, Arc<[u8]>)>;

    /// Decodes an entities record: one binary network-NBT compound per entity, in stored
    /// order. Default: the format keeps no entities.
    fn decode_entities(
        &self,
        _pos: ChunkPos,
        _meta: &RecordMeta,
        _bytes: &[u8],
    ) -> io::Result<Vec<Arc<[u8]>>> {
        Ok(Vec::new())
    }

    /// Encodes the entities of a chunk ([`ProtoChunk::entities`]) into a record. Default: the
    /// format keeps no entities.
    fn encode_entities(&self, chunk: &ProtoChunk) -> io::Result<(RecordMeta, Arc<[u8]>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("chunk {:?}: this world format keeps no entities", chunk.pos),
        ))
    }

    /// Decodes a points of interest record into [`ProtoChunk::poi`]. Default: the format
    /// keeps no POI.
    fn decode_poi(
        &self,
        _pos: ChunkPos,
        _meta: &RecordMeta,
        _bytes: &[u8],
    ) -> io::Result<Option<Arc<[u8]>>> {
        Ok(None)
    }

    /// Encodes the POI of a chunk ([`ProtoChunk::poi`], which is `Some`) into a record.
    /// Default: the format keeps no POI.
    fn encode_poi(&self, chunk: &ProtoChunk) -> io::Result<(RecordMeta, Arc<[u8]>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("chunk {:?}: this world format keeps no POI", chunk.pos),
        ))
    }

    /// Chunks per encode job, so one job takes about 1 ms (compute-pool rule).
    fn chunks_per_job(&self) -> usize {
        4
    }
}

/// A store that keeps nothing: every record is missing, every write succeeds. For tests and
/// worlds that are never saved.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullStore;

impl ChunkStore for NullStore {
    fn submit(&self, token: Token, ops: Vec<StoreOp>, _: Priority, to: CompletionTarget) {
        for (index, op) in ops.iter().enumerate() {
            let result = match op {
                StoreOp::Get(_) | StoreOp::Meta(_) => StoreResult::Missing,
                StoreOp::Put { .. } | StoreOp::Delete(_) => StoreResult::Done,
            };
            to.post(token, index, result);
        }
    }

    fn cancel(&self, _: Token) {}
}

/// A store that keeps its records in memory (tests and scenario worlds): writes take effect
/// at once, every commit succeeds.
#[derive(Debug, Default)]
pub struct MemoryStore {
    records: std::sync::Mutex<std::collections::HashMap<RecordKey, Record>>,
}

/// A stored record: its metadata and bytes.
type Record = (RecordMeta, Arc<[u8]>);

impl ChunkStore for MemoryStore {
    fn submit(&self, token: Token, ops: Vec<StoreOp>, _: Priority, to: CompletionTarget) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (index, op) in ops.into_iter().enumerate() {
            let result = match op {
                StoreOp::Get(key) => match records.get(&key) {
                    Some((meta, bytes)) => StoreResult::Record(*meta, Arc::clone(bytes)),
                    None => StoreResult::Missing,
                },
                StoreOp::Meta(key) => match records.get(&key) {
                    Some((meta, _)) => StoreResult::Meta(*meta),
                    None => StoreResult::Missing,
                },
                StoreOp::Put { key, meta, bytes } => {
                    records.insert(key, (meta, bytes));
                    StoreResult::Done
                }
                StoreOp::Delete(key) => {
                    records.remove(&key);
                    StoreResult::Done
                }
            };
            to.post(token, index, result);
        }
    }

    fn cancel(&self, _: Token) {}
}

/// The codec of [`NullStore`] worlds: encodes nothing, decodes nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullCodec;

impl ChunkCodec for NullCodec {
    fn decode(&self, pos: ChunkPos, _: &RecordMeta, _: &[u8]) -> io::Result<ProtoChunk> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("chunk {pos:?}: no codec"),
        ))
    }

    fn encode(&self, chunk: &ProtoChunk) -> io::Result<(RecordMeta, Arc<[u8]>)> {
        Ok((
            RecordMeta {
                status: chunk.status as u8,
                ..RecordMeta::default()
            },
            Arc::from([]),
        ))
    }

    fn encode_entities(&self, _: &ProtoChunk) -> io::Result<(RecordMeta, Arc<[u8]>)> {
        Ok((RecordMeta::default(), Arc::from([])))
    }

    fn chunks_per_job(&self) -> usize {
        64
    }
}
