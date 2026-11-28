//! The chunks a player sees and their sending: Vanilla's `ChunkTrackingView`,
//! `ChunkMap.updateChunkTracking` and `PlayerChunkSender` (batches paced by the client's
//! `chunk_batch_received` feedback).
//!
//! Per player the work is bounded: a view change touches only the chunks entering or leaving,
//! and one tick sends at most the client's desired batch size (at most 64 chunks).

use std::sync::Arc;

use vanillite_base::ChunkPos;
use vanillite_base::hash::FxHashSet;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{
    ChunkBatchFinished, ChunkBatchStart, ForgetLevelChunk, SetChunkCacheCenter,
};

use super::ChunkView;
use super::sink::PacketSink;

/// `PlayerChunkSender.MIN_CHUNKS_PER_TICK`.
const MIN_CHUNKS_PER_TICK: f32 = 0.01;
/// `PlayerChunkSender.MAX_CHUNKS_PER_TICK`.
const MAX_CHUNKS_PER_TICK: f32 = 64.0;
/// `PlayerChunkSender.START_CHUNKS_PER_TICK`.
const START_CHUNKS_PER_TICK: f32 = 9.0;
/// `PlayerChunkSender.MAX_UNACKNOWLEDGED_BATCHES`.
const MAX_UNACKNOWLEDGED_BATCHES: i32 = 10;
/// Ticks after its last centre change a view still counts as moving (a flying player crosses
/// a chunk every few ticks, a walking one about every 70).
const MOVING_TICKS: u32 = 40;
/// How far ahead of a moving view's centre its chunks are ordered from, in chunks.
const MAX_LEAD: i32 = 4;

/// Where chunk packets come from: the world runtime, or a placeholder generator until it
/// exists. Packets are encoded once per chunk and shared by all players that receive it.
pub trait ChunkSource {
    /// The `level_chunk_with_light` packet of a loaded, complete chunk; `None` while the chunk
    /// is not ready.
    fn chunk_packet(&self, pos: ChunkPos) -> Option<Arc<SharedPacket>>;

    /// Whether [`ChunkSource::chunk_packet`] would return a packet; sources override it when
    /// they can tell without encoding.
    fn is_ready(&self, pos: ChunkPos) -> bool {
        self.chunk_packet(pos).is_some()
    }
}

/// `ChunkTrackingView.Positioned`.
impl ChunkView {
    /// `ChunkTrackingView.isWithinDistance`.
    pub fn contains(&self, pos: ChunkPos, include_neighbors: bool) -> bool {
        let buffer = if include_neighbors { 2 } else { 1 };
        let dx = i64::from(
            pos.x
                .saturating_sub(self.center.x)
                .saturating_abs()
                .saturating_sub(buffer)
                .max(0),
        );
        let dz = i64::from(
            pos.z
                .saturating_sub(self.center.z)
                .saturating_abs()
                .saturating_sub(buffer)
                .max(0),
        );
        let r = i64::from(self.view_distance);
        dx.saturating_mul(dx).saturating_add(dz.saturating_mul(dz)) < r.saturating_mul(r)
    }

    fn min_x(&self) -> i32 {
        self.center
            .x
            .saturating_sub(self.view_distance)
            .saturating_sub(1)
    }

    fn max_x(&self) -> i32 {
        self.center
            .x
            .saturating_add(self.view_distance)
            .saturating_add(1)
    }

    fn min_z(&self) -> i32 {
        self.center
            .z
            .saturating_sub(self.view_distance)
            .saturating_sub(1)
    }

    fn max_z(&self) -> i32 {
        self.center
            .z
            .saturating_add(self.view_distance)
            .saturating_add(1)
    }

    fn square_intersects(&self, other: &ChunkView) -> bool {
        self.min_x() <= other.max_x()
            && self.max_x() >= other.min_x()
            && self.min_z() <= other.max_z()
            && self.max_z() >= other.min_z()
    }

    fn for_each(&self, mut f: impl FnMut(ChunkPos)) {
        for x in self.min_x()..=self.max_x() {
            for z in self.min_z()..=self.max_z() {
                let pos = ChunkPos::new(x, z);
                if self.contains(pos, true) {
                    f(pos);
                }
            }
        }
    }
}

/// `ChunkPos.distanceSquared` with wrapping like Java's int arithmetic.
fn distance_squared(a: ChunkPos, b: ChunkPos) -> i32 {
    let dx = a.x.wrapping_sub(b.x);
    let dz = a.z.wrapping_sub(b.z);
    dx.wrapping_mul(dx).wrapping_add(dz.wrapping_mul(dz))
}

/// The chunks of one player: its view and the chunks waiting to be sent.
#[derive(Debug)]
pub struct PlayerChunks {
    view: Option<ChunkView>,
    pending: FxHashSet<ChunkPos>,
    memory_connection: bool,
    desired_chunks_per_tick: f32,
    batch_quota: f32,
    unacknowledged_batches: i32,
    max_unacknowledged_batches: i32,
    scratch: Vec<ChunkPos>,
    /// [`PlayerChunks::sent_radius`] by centre, until `pending` changes.
    sent_radius: Option<(ChunkPos, i32)>,
    /// The last step of the view centre (chunks; zero after a jump).
    motion: (i32, i32),
    /// Ticks since the view centre last moved.
    still_ticks: u32,
}

impl PlayerChunks {
    /// A player without a view yet. In-memory connections get all pending chunks at once
    /// (Vanilla's `memoryConnection`).
    pub fn new(memory_connection: bool) -> PlayerChunks {
        PlayerChunks {
            view: None,
            pending: FxHashSet::default(),
            memory_connection,
            desired_chunks_per_tick: START_CHUNKS_PER_TICK,
            batch_quota: 0.0,
            unacknowledged_batches: 0,
            max_unacknowledged_batches: 1,
            scratch: Vec::new(),
            sent_radius: None,
            motion: (0, 0),
            still_ticks: 0,
        }
    }

    /// Whether the player is an in-process client (test bots).
    pub fn is_memory_connection(&self) -> bool {
        self.memory_connection
    }

    /// Where a moving view heads: its centre moved ahead in the direction of its last step
    /// (by up to a third of the view distance). `None` for a view that stands still or jumped.
    /// Chunks nearer to it are sent (and generated) first; Vanilla orders by the centre only,
    /// the chunks themselves are the same.
    pub fn focus(&self) -> Option<ChunkPos> {
        let view = self.view?;
        if self.motion == (0, 0) || self.still_ticks > MOVING_TICKS {
            return None;
        }
        let lead = (view.view_distance / 3).clamp(1, MAX_LEAD);
        Some(ChunkPos::new(
            view.center
                .x
                .saturating_add(self.motion.0.saturating_mul(lead)),
            view.center
                .z
                .saturating_add(self.motion.1.saturating_mul(lead)),
        ))
    }

    /// The current view.
    pub fn view(&self) -> Option<ChunkView> {
        self.view
    }

    /// `isPending`: the chunk is in view but not sent yet.
    pub fn is_pending(&self, pos: ChunkPos) -> bool {
        self.pending.contains(&pos)
    }

    /// The Chebyshev distance from `center` of the nearest pending chunk (`i32::MAX` when
    /// none): no chunk closer to `center` is pending.
    pub fn sent_radius(&mut self, center: ChunkPos) -> i32 {
        if let Some((at, radius)) = self.sent_radius
            && at == center
        {
            return radius;
        }
        let radius = self
            .pending
            .iter()
            .map(|p| p.x.abs_diff(center.x).max(p.z.abs_diff(center.z)))
            .min()
            .map_or(i32::MAX, |d| i32::try_from(d).unwrap_or(i32::MAX));
        self.sent_radius = Some((center, radius));
        radius
    }

    /// `updateChunkTracking` / `applyChunkTrackingView`: moves the view to `center` with
    /// `distance`. Sends the new centre, queues entering chunks and forgets leaving ones.
    pub fn update_view(&mut self, center: ChunkPos, distance: i32, sink: &dyn PacketSink) {
        let next = ChunkView {
            center,
            view_distance: distance,
        };
        let previous = self.view;
        if previous == Some(next) {
            return;
        }
        self.sent_radius = None;
        if previous.is_none_or(|p| p.center != center) {
            sink.send(&SetChunkCacheCenter {
                x: center.x,
                z: center.z,
            });
            let step = previous.map(|p| {
                (
                    center.x.saturating_sub(p.center.x),
                    center.z.saturating_sub(p.center.z),
                )
            });
            self.motion = match step {
                // A step to a neighbouring chunk (or two, when fast): the view moves.
                Some((dx, dz)) if dx.abs() <= 2 && dz.abs() <= 2 => (dx.signum(), dz.signum()),
                _ => (0, 0),
            };
            self.still_ticks = 0;
        }
        match previous {
            Some(last) if last.square_intersects(&next) => {
                for x in last.min_x().min(next.min_x())..=last.max_x().max(next.max_x()) {
                    for z in last.min_z().min(next.min_z())..=last.max_z().max(next.max_z()) {
                        let pos = ChunkPos::new(x, z);
                        let saw = last.contains(pos, true);
                        let sees = next.contains(pos, true);
                        if saw != sees {
                            if sees {
                                self.pending.insert(pos);
                            } else {
                                self.drop_chunk(pos, sink);
                            }
                        }
                    }
                }
            }
            Some(last) => {
                last.for_each(|pos| self.drop_chunk(pos, sink));
                next.for_each(|pos| {
                    self.pending.insert(pos);
                });
            }
            None => next.for_each(|pos| {
                self.pending.insert(pos);
            }),
        }
        self.view = Some(next);
    }

    /// Forgets the view and all pending chunks: the client dropped its level (respawn); the
    /// next [`update_view`](Self::update_view) queues every chunk again.
    pub fn reset(&mut self) {
        self.view = None;
        self.motion = (0, 0);
        self.pending.clear();
        self.sent_radius = None;
    }

    /// `dropChunk`.
    fn drop_chunk(&mut self, pos: ChunkPos, sink: &dyn PacketSink) {
        self.sent_radius = None;
        if !self.pending.remove(&pos) {
            sink.send(&ForgetLevelChunk { pos: pos.as_long() });
        }
    }

    /// `sendNextChunks`: one batch per tick, sized by the client's feedback. The batch takes
    /// the pending chunks that are ready, nearest to the [`focus`](Self::focus) (the centre
    /// while standing) first; Vanilla takes the nearest pending ones and sends those that are
    /// ready, so a batch stays small while the nearest still generate.
    pub fn send_next_chunks(&mut self, source: &dyn ChunkSource, sink: &dyn PacketSink) {
        self.still_ticks = self.still_ticks.saturating_add(1);
        if self.unacknowledged_batches >= self.max_unacknowledged_batches {
            return;
        }
        let max_batch_size = self.desired_chunks_per_tick.max(1.0);
        self.batch_quota = (self.batch_quota + self.desired_chunks_per_tick).min(max_batch_size);
        if self.batch_quota < 1.0 || self.pending.is_empty() {
            return;
        }
        let Some(view) = self.view else {
            return;
        };
        let center = self.focus().unwrap_or(view.center);
        // The quota is at most 64 here.
        let max = usize::try_from(vanillite_base::math::floor_f(self.batch_quota)).unwrap_or(0);
        let mut chosen = std::mem::take(&mut self.scratch);
        chosen.clear();
        chosen.extend(
            self.pending
                .iter()
                .copied()
                .filter(|&pos| source.is_ready(pos)),
        );
        let key = |pos: &ChunkPos| distance_squared(center, *pos);
        if !self.memory_connection && chosen.len() > max {
            // The `max` nearest in linear time, then only those sorted.
            chosen.select_nth_unstable_by_key(max, key);
            chosen.truncate(max);
        }
        chosen.sort_unstable_by_key(key);
        let mut packets = Vec::with_capacity(chosen.len());
        for &pos in &chosen {
            if let Some(packet) = source.chunk_packet(pos) {
                self.pending.remove(&pos);
                self.sent_radius = None;
                packets.push(packet);
            }
        }
        self.scratch = chosen;
        if packets.is_empty() {
            return;
        }
        self.unacknowledged_batches = self.unacknowledged_batches.saturating_add(1);
        sink.send(&ChunkBatchStart);
        for packet in &packets {
            sink.send_shared(packet);
        }
        let count = i32::try_from(packets.len()).unwrap_or(i32::MAX);
        sink.send(&ChunkBatchFinished { batch_size: count });
        self.batch_quota -= f32::from(u16::try_from(count).unwrap_or(u16::MAX));
    }

    /// `onChunkBatchReceivedByClient`.
    pub fn batch_received(&mut self, desired_chunks_per_tick: f32) {
        self.unacknowledged_batches = self.unacknowledged_batches.saturating_sub(1).max(0);
        self.desired_chunks_per_tick = if desired_chunks_per_tick.is_nan() {
            MIN_CHUNKS_PER_TICK
        } else {
            desired_chunks_per_tick.clamp(MIN_CHUNKS_PER_TICK, MAX_CHUNKS_PER_TICK)
        };
        if self.unacknowledged_batches == 0 {
            self.batch_quota = 1.0;
        }
        self.max_unacknowledged_batches = MAX_UNACKNOWLEDGED_BATCHES;
    }
}
