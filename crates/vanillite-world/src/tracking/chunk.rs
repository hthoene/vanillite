//! The built-in tracker: Vanilla's `ChunkMap.TrackedEntity` and `ServerEntity` over
//! chunk-indexed nearby-player sets.
//!
//! Scaling: every viewer is registered in the cells of 4×4 chunks that overlap a square
//! around it covering the largest tracking range, updated incrementally when that set of
//! cells changes. An entity that changes section checks only the viewers registered in its
//! chunk's cell and those that see it; a viewer that changes section checks only the entities
//! in its square and those it sees. No step iterates all pairs. The per-tick cost is one pass over the entities (Vanilla's
//! `sendChanges`) plus work proportional to the section changes.

mod crowd;

use rustc_hash::FxHashMap;
use vanillite_base::math::{java_round, pack_degrees};
use vanillite_base::slot_map::SlotKey;
use vanillite_base::{ChunkPos, SectionPos, Vec3};
use vanillite_data::entity::EntityTypeId;

use super::{
    EntityTracker, EntityTrackerProvider, EntityUpdate, TrackerInput, TrackerOutput,
    TrackerSettings, Viewer, ViewerSpan,
};
use crate::entity::{CoreColumns, EntityFlags, EntityId, EntityStore, NetworkId};
use crowd::Crowd;

/// `ServerEntity`: movements below this squared distance are not sent.
const MIN_MOVE_SQR: f64 = 7.629_394_5e-6_f32 as f64;
/// `ServerEntity`: ticks after which an absolute position is forced.
const TELEPORT_DELAY: i32 = 400;
/// `ServerEntity`: the position is resent at least this often (in update ticks).
const FORCE_POS_INTERVAL: i32 = 60;

/// Provides [`ChunkTracker`]s.
#[derive(Debug, Default)]
pub struct ChunkTrackerProvider;

impl EntityTrackerProvider for ChunkTrackerProvider {
    fn create(&self, settings: &TrackerSettings) -> Box<dyn EntityTracker> {
        Box::new(ChunkTracker::new(settings.clone()))
    }
}

/// What was last sent for an entity (`ServerEntity` fields).
#[derive(Debug)]
struct SyncState {
    tick_count: i32,
    teleport_delay: i32,
    base: Vec3,
    y_rot: i8,
    x_rot: i8,
    y_head_rot: i8,
    movement: Vec3,
    was_on_ground: bool,
    was_riding: bool,
}

impl SyncState {
    /// The state of an entity the tracker sees for the first time. Vanilla's `ServerEntity`
    /// starts from the position the entity was added at (`addEntity`): one added before its
    /// first tick this tick (the dragon of a fight) is still at its tick's start position.
    fn new(core: &CoreColumns, row: usize) -> SyncState {
        let rot = core.rot[row];
        SyncState {
            tick_count: 0,
            teleport_delay: 0,
            base: core.old_pos[row],
            y_rot: pack_degrees(rot.y_rot),
            x_rot: pack_degrees(rot.x_rot),
            y_head_rot: pack_degrees(rot.y_head_rot),
            movement: core.velocity[row],
            was_on_ground: core.flags[row].contains(EntityFlags::ON_GROUND),
            was_riding: false,
        }
    }
}

/// One tracked entity.
#[derive(Debug)]
struct Tracked {
    id: EntityId,
    /// Its row in the store at the last update ([`ChunkTracker::stamp`] = `stamp`).
    row: u32,
    stamp: u32,
    network_id: NetworkId,
    kind: EntityTypeId,
    /// Section at the last update; `None` before the first.
    section: Option<SectionPos>,
    /// Chunk it is registered under in `by_chunk`.
    chunk: Option<ChunkPos>,
    /// Viewers that see it.
    seen_by: Vec<EntityId>,
    /// The slots of the viewers that see it: the pair checks of an entity that moved test
    /// one bit set instead of each viewer's.
    seen_slots: SlotSet,
    sync: SyncState,
}

/// One viewer.
#[derive(Debug)]
struct ViewerState {
    entity: EntityId,
    /// Its row in the store at this update.
    row: u32,
    /// Its position at this update.
    pos: Vec3,
    view: Viewer,
    /// Centre of the square it is registered in (`nearby`).
    square: Option<ChunkPos>,
    /// Entity indices of the entities it sees.
    viewing: SlotSet,
    present: bool,
    /// The update in which it was queued in `moved`.
    queued: u32,
}

/// A set of entity indices as a bit set: one bit per index instead of a hashed entity id, so
/// a viewer that sees thousands of entities costs bytes, not tens of kilobytes, and a lookup
/// is one bit test.
#[derive(Debug, Default)]
struct SlotSet(Vec<u64>);

impl SlotSet {
    #[inline]
    fn contains(&self, slot: u32) -> bool {
        self.0
            .get(slot as usize / 64)
            .is_some_and(|word| word & (1 << (slot % 64)) != 0)
    }

    #[inline]
    fn insert(&mut self, slot: u32) {
        let word = slot as usize / 64;
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << (slot % 64);
    }

    /// The word of the bits `64 * index ..`.
    #[inline]
    fn word(&self, index: usize) -> u64 {
        self.0.get(index).copied().unwrap_or(0)
    }

    #[inline]
    fn remove(&mut self, slot: u32) {
        if let Some(word) = self.0.get_mut(slot as usize / 64) {
            *word &= !(1 << (slot % 64));
        }
    }

    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().zip(0u32..).flat_map(|(&word, index)| {
            let mut bits = word;
            std::iter::from_fn(move || {
                (bits != 0).then(|| {
                    let bit = bits.trailing_zeros();
                    bits &= bits - 1;
                    index * 64 + bit
                })
            })
        })
    }
}

/// No viewer slot.
const NONE: u32 = u32::MAX;

/// The dense index of an entity in the store (reused after it is removed).
#[inline]
fn index(id: EntityId) -> u32 {
    id.raw().index()
}

/// The viewer slot of `id`, if it is a viewer.
#[inline]
fn viewer_of(viewer_slot: &[u32], viewers: &[Option<ViewerState>], id: EntityId) -> Option<u32> {
    let slot = *viewer_slot.get(index(id) as usize)?;
    viewers
        .get(slot as usize)?
        .as_ref()
        .is_some_and(|state| state.entity == id)
        .then_some(slot)
}

/// The built-in `entity-tracker`. Entities and viewers are kept in vectors by dense index
/// (entity index, viewer slot), so the per-pair work of the visibility checks does no hashing.
#[derive(Debug)]
pub struct ChunkTracker {
    settings: TrackerSettings,
    /// Chebyshev radius in chunks of a viewer's registration square.
    radius: i32,
    /// Tracked entities by entity index.
    tracked: Vec<Option<Tracked>>,
    /// Viewer slot by entity index ([`NONE`] = not a viewer).
    viewer_slot: Vec<u32>,
    viewers: Vec<Option<ViewerState>>,
    free_viewers: Vec<u32>,
    /// Entity indices of the tracked entities per chunk.
    by_chunk: FxHashMap<i64, Vec<u32>>,
    /// Slots of the viewers whose square overlaps a cell of chunks ([`cell_of`]), sorted.
    nearby: FxHashMap<i64, Vec<u32>>,
    /// Number of the current update.
    stamp: u32,
    /// Crowd mode (`performance.crowd`), `None` while it is off.
    crowd: Option<Box<Crowd>>,
    // Scratch buffers, kept for their capacity.
    moved: Vec<u32>,
    candidates: Vec<u32>,
    near_set: SlotSet,
    added: Vec<EntityId>,
    gone: Vec<EntityId>,
    gone_slots: Vec<u32>,
}

impl ChunkTracker {
    /// A tracker with the given ranges.
    pub fn new(settings: TrackerSettings) -> ChunkTracker {
        let radius = settings.max_range().div_euclid(16) + 2;
        let crowd = Crowd::new(settings.crowd()).map(Box::new);
        ChunkTracker {
            settings,
            radius,
            tracked: Vec::new(),
            viewer_slot: Vec::new(),
            viewers: Vec::new(),
            free_viewers: Vec::new(),
            by_chunk: FxHashMap::default(),
            nearby: FxHashMap::default(),
            stamp: 0,
            crowd,
            moved: Vec::new(),
            candidates: Vec::new(),
            near_set: SlotSet::default(),
            added: Vec::new(),
            gone: Vec::new(),
            gone_slots: Vec::new(),
        }
    }

    /// Sends `Remove` for despawned entities and forgets them.
    fn forget_removed(&mut self, store: &EntityStore, out: &mut TrackerOutput) {
        for removed in store.removed() {
            let i = index(removed.id);
            let Some(tracked) = self
                .tracked
                .get_mut(i as usize)
                .and_then(|t| t.take_if(|t| t.id == removed.id))
            else {
                continue;
            };
            forget(
                tracked,
                &mut self.by_chunk,
                &self.viewer_slot,
                &mut self.viewers,
                self.crowd.as_deref_mut(),
                out,
            );
        }
    }

    /// Takes over the current viewers: registers squares, notes new viewers and view changes
    /// in `moved`, and drops viewers that left (without packets: their client is gone or
    /// changes dimension, which clears its entities).
    fn sync_viewers(&mut self, input: &TrackerInput<'_>) {
        let core = input.entities.columns();
        for state in self.viewers.iter_mut().flatten() {
            state.present = false;
        }
        for viewer in input.viewers {
            let Some(row) = input.entities.row(viewer.entity) else {
                continue;
            };
            let chunk = core.section[row].chunk();
            let slot = match viewer_of(&self.viewer_slot, &self.viewers, viewer.entity) {
                Some(slot) => slot,
                None => {
                    let state = ViewerState {
                        entity: viewer.entity,
                        row: 0,
                        pos: Vec3::ZERO,
                        view: *viewer,
                        square: None,
                        viewing: SlotSet::default(),
                        present: false,
                        queued: self.stamp.wrapping_sub(1),
                    };
                    let slot = match self.free_viewers.pop() {
                        Some(slot) => {
                            self.viewers[slot as usize] = Some(state);
                            slot
                        }
                        None => {
                            self.viewers.push(Some(state));
                            u32::try_from(self.viewers.len() - 1).unwrap_or(NONE)
                        }
                    };
                    let i = index(viewer.entity) as usize;
                    if self.viewer_slot.len() <= i {
                        self.viewer_slot.resize(i + 1, NONE);
                    }
                    self.viewer_slot[i] = slot;
                    if let Some(crowd) = self.crowd.as_deref_mut() {
                        crowd.new_viewer(slot);
                    }
                    slot
                }
            };
            let Some(Some(state)) = self.viewers.get_mut(slot as usize) else {
                continue;
            };
            state.row = u32::try_from(row).unwrap_or(u32::MAX);
            state.pos = core.pos[row];
            let changed = state.square.is_none()
                || sees_more(&state.view, viewer, &self.by_chunk, self.radius);
            state.view = *viewer;
            if changed {
                queue(&mut self.moved, state, slot, self.stamp);
            }
            state.present = true;
            if state.square != Some(chunk) {
                move_square(
                    &mut self.nearby,
                    slot,
                    state.square,
                    Some(chunk),
                    self.radius,
                );
                state.square = Some(chunk);
            }
        }
        self.gone_slots.clear();
        self.gone_slots.extend(
            self.viewers
                .iter()
                .zip(0u32..)
                .filter(|(state, _)| state.as_ref().is_some_and(|state| !state.present))
                .map(|(_, slot)| slot),
        );
        for &slot in &self.gone_slots {
            let Some(state) = self.viewers.get_mut(slot as usize).and_then(Option::take) else {
                continue;
            };
            self.free_viewers.push(slot);
            if let Some(at) = self.viewer_slot.get_mut(index(state.entity) as usize)
                && *at == slot
            {
                *at = NONE;
            }
            move_square(&mut self.nearby, slot, state.square, None, self.radius);
            for i in state.viewing.iter() {
                if let Some(Some(tracked)) = self.tracked.get_mut(i as usize) {
                    remove_unordered(&mut tracked.seen_by, state.entity);
                    tracked.seen_slots.remove(slot);
                }
            }
        }
        self.gone_slots.clear();
    }
}

/// Whether a viewer's new view can change what it sees: another centre or distance, or a
/// changed `sent_radius` whose rings in between hold tracked entities. Rings beyond `radius`
/// (the largest tracking range in chunks plus a margin) hold none the viewer could see.
fn sees_more(old: &Viewer, new: &Viewer, by_chunk: &FxHashMap<i64, Vec<u32>>, radius: i32) -> bool {
    if old.entity != new.entity
        || old.view_center != new.view_center
        || old.view_distance != new.view_distance
    {
        return true;
    }
    if old.sent_radius == new.sent_radius {
        return false;
    }
    let from = old.sent_radius.min(new.sent_radius).max(0);
    let to = old.sent_radius.max(new.sent_radius).min(radius);
    (from..=to).any(|ring| {
        let c = new.view_center;
        let has =
            |x: i32, z: i32| by_chunk.contains_key(&ChunkPos::new(c.x + x, c.z + z).as_long());
        (-ring..=ring).any(|d| has(d, -ring) || has(d, ring) || has(-ring, d) || has(ring, d))
    })
}

/// Queues a viewer in `moved` once per update.
#[inline]
fn queue(moved: &mut Vec<u32>, state: &mut ViewerState, slot: u32, stamp: u32) {
    if state.queued != stamp {
        state.queued = stamp;
        moved.push(slot);
    }
}

/// Forgets a tracked entity and sends `Remove` to its viewers.
fn forget(
    tracked: Tracked,
    by_chunk: &mut FxHashMap<i64, Vec<u32>>,
    viewer_slot: &[u32],
    viewers: &mut [Option<ViewerState>],
    mut crowd: Option<&mut Crowd>,
    out: &mut TrackerOutput,
) {
    let i = index(tracked.id);
    if let Some(chunk) = tracked.chunk {
        remove_from(by_chunk, chunk.as_long(), i);
    }
    for &viewer in &tracked.seen_by {
        if let Some(slot) = viewer_of(viewer_slot, viewers, viewer)
            && let Some(Some(state)) = viewers.get_mut(slot as usize)
        {
            state.viewing.remove(i);
            if let Some(crowd) = crowd.as_deref_mut() {
                crowd.pair_changed(i, slot, tracked.kind, false);
            }
        }
    }
    let span = out.push_viewers(tracked.seen_by.iter().copied());
    out.push(tracked.id, tracked.network_id, EntityUpdate::Remove, span);
}

impl EntityTracker for ChunkTracker {
    fn seen_by(&self, entity: EntityId) -> &[EntityId] {
        self.tracked
            .get(index(entity) as usize)
            .and_then(Option::as_ref)
            .filter(|t| t.id == entity)
            .map_or(&[], |t| &t.seen_by)
    }

    fn update(&mut self, input: &TrackerInput<'_>, out: &mut TrackerOutput) {
        let store = input.entities;
        let core = store.columns();
        self.stamp = self.stamp.wrapping_add(1);
        self.moved.clear();
        self.forget_removed(store, out);
        self.sync_viewers(input);
        // Chunks that reached a client make its entities pairable: check that viewer again.
        for &(viewer, _) in input.chunks_sent {
            if let Some(slot) = viewer_of(&self.viewer_slot, &self.viewers, viewer)
                && let Some(Some(state)) = self.viewers.get_mut(slot as usize)
            {
                queue(&mut self.moved, state, slot, self.stamp);
            }
        }

        let ChunkTracker {
            settings,
            radius,
            tracked,
            viewer_slot,
            viewers,
            by_chunk,
            nearby,
            stamp,
            crowd,
            moved,
            candidates,
            near_set,
            added,
            gone,
            ..
        } = self;
        let radius = *radius;
        let stamp = *stamp;
        let mut pairing = Pairing {
            input,
            settings,
            viewers,
            crowd: crowd.as_deref_mut(),
            added,
            gone,
        };

        // `ChunkMap.tick`: section changes update the entity's viewers, then `sendChanges`.
        for row in 0..core.id.len() {
            let id = core.id[row];
            let section = core.section[row];
            let i = index(id);
            if tracked.len() <= i as usize {
                tracked.resize_with(i as usize + 1, || None);
            }
            let slot = &mut tracked[i as usize];
            // An entity that left without a removal record: forget it like a removed one.
            if let Some(stale) = slot.take_if(|t| t.id != id) {
                forget(
                    stale,
                    by_chunk,
                    viewer_slot,
                    pairing.viewers,
                    pairing.crowd.as_deref_mut(),
                    out,
                );
            }
            let entry = slot.get_or_insert_with(|| Tracked {
                id,
                row: 0,
                stamp: 0,
                network_id: core.network_id[row],
                kind: core.kind[row],
                section: None,
                chunk: None,
                seen_by: Vec::new(),
                seen_slots: SlotSet::default(),
                sync: SyncState::new(core, row),
            });
            entry.row = u32::try_from(row).unwrap_or(u32::MAX);
            entry.stamp = stamp;
            let own_slot = viewer_of(viewer_slot, pairing.viewers, id);
            if entry.section != Some(section) {
                let chunk = section.chunk();
                if entry.chunk != Some(chunk) {
                    if let Some(old) = entry.chunk {
                        remove_from(by_chunk, old.as_long(), i);
                    }
                    by_chunk.entry(chunk.as_long()).or_default().push(i);
                    entry.chunk = Some(chunk);
                }
                entry.section = Some(section);
                candidates.clear();
                if let Some(list) = nearby.get(&cell_of(chunk)) {
                    candidates.extend_from_slice(list);
                }
                // Viewers that see it but are no longer registered near its chunk.
                let near = candidates.len();
                for &slot in candidates.iter() {
                    near_set.insert(slot);
                }
                for (index, &seen) in (0u32..).zip(&entry.seen_slots.0) {
                    let mut bits = seen & !near_set.word(index as usize);
                    while bits != 0 {
                        candidates.push(index * 64 + bits.trailing_zeros());
                        bits &= bits - 1;
                    }
                }
                for &slot in &candidates[..near] {
                    near_set.remove(slot);
                }
                pairing.update(entry, row, candidates, out);
                if let Some(slot) = own_slot
                    && let Some(Some(state)) = pairing.viewers.get_mut(slot as usize)
                {
                    queue(moved, state, slot, stamp);
                }
            }
            let is_viewer = own_slot.is_some();
            let scale = input.update_scale;
            match pairing.crowd.as_deref_mut().filter(|c| c.tiers(entry.kind)) {
                None => send_changes(entry, None, core, row, is_viewer, scale, out),
                Some(crowd) => {
                    let far_due = crowd.far_due(entry.sync.tick_count, scale);
                    if far_due || sends(entry, core, row, scale) {
                        crowd.split(entry, core, row, pairing.viewers, far_due);
                    } else {
                        crowd.clear_tiers();
                    }
                    send_changes(entry, Some(&crowd.near), core, row, is_viewer, scale, out);
                    crowd.send_far(entry, core, row, out);
                }
            }
        }

        // Viewers whose count of visible players left the band around the cap.
        if let Some(crowd) = pairing.crowd.as_deref_mut() {
            crowd.take_rechecks(|slot| {
                if let Some(Some(state)) = pairing.viewers.get_mut(slot as usize) {
                    queue(moved, state, slot, stamp);
                }
            });
        }

        // Viewers that moved to another section (or joined) re-check what they can see.
        for &viewer in moved.iter() {
            let Some(Some(state)) = pairing.viewers.get(viewer as usize) else {
                continue;
            };
            let Some(square) = state.square else {
                continue;
            };
            candidates.clear();
            for_square(square, radius, |chunk| {
                if let Some(list) = by_chunk.get(&chunk.as_long()) {
                    candidates.extend_from_slice(list);
                }
            });
            candidates.extend(state.viewing.iter().filter(|&i| {
                tracked
                    .get(i as usize)
                    .and_then(Option::as_ref)
                    .and_then(|t| t.chunk)
                    .is_none_or(|chunk| !in_square(square, radius, chunk))
            }));
            if let Some(crowd) = pairing.crowd.as_deref_mut() {
                crowd.fit(viewer, state, candidates, tracked, core, stamp);
            }
            for &i in candidates.iter() {
                let Some(Some(entry)) = tracked.get_mut(i as usize) else {
                    continue;
                };
                if entry.stamp != stamp {
                    continue;
                }
                let row = entry.row as usize;
                pairing.update_one(entry, row, viewer, out);
            }
        }
    }
}

/// Visibility changes between entities and viewers (`TrackedEntity.updatePlayer`).
struct Pairing<'a, 'i> {
    input: &'a TrackerInput<'i>,
    settings: &'a TrackerSettings,
    viewers: &'a mut Vec<Option<ViewerState>>,
    crowd: Option<&'a mut Crowd>,
    added: &'a mut Vec<EntityId>,
    gone: &'a mut Vec<EntityId>,
}

impl Pairing<'_, '_> {
    /// [`Self::update`] for one viewer: tests the viewer's own bit set, which stays in cache
    /// while one viewer checks many entities.
    fn update_one(&mut self, entry: &mut Tracked, row: usize, slot: u32, out: &mut TrackerOutput) {
        let Some(Some(state)) = self.viewers.get(slot as usize) else {
            return;
        };
        if state.entity == entry.id {
            return;
        }
        let target = Target::new(self.input, self.settings, row);
        let visible = self
            .crowd
            .as_deref()
            .is_none_or(|crowd| crowd.sees(slot, state.pos, target.kind, target.pos))
            && is_visible(self.input, &target, state);
        if visible != state.viewing.contains(index(entry.id)) {
            self.update(entry, row, &[slot], out);
        }
    }

    /// Re-checks the viewers in the slots `candidates` and emits `Remove` and `Spawn`.
    fn update(
        &mut self,
        entry: &mut Tracked,
        row: usize,
        candidates: &[u32],
        out: &mut TrackerOutput,
    ) {
        self.added.clear();
        self.gone.clear();
        let id = entry.id;
        let i = index(id);
        let target = Target::new(self.input, self.settings, row);
        for &slot in candidates {
            let Some(Some(state)) = self.viewers.get_mut(slot as usize) else {
                continue;
            };
            if state.entity == id {
                continue;
            }
            let visible = self
                .crowd
                .as_deref()
                .is_none_or(|crowd| crowd.sees(slot, state.pos, target.kind, target.pos))
                && is_visible(self.input, &target, state);
            let seen = entry.seen_slots.contains(slot);
            if visible == seen {
                continue;
            }
            if visible {
                state.viewing.insert(i);
                entry.seen_slots.insert(slot);
                entry.seen_by.push(state.entity);
                self.added.push(state.entity);
            } else {
                state.viewing.remove(i);
                entry.seen_slots.remove(slot);
                remove_unordered(&mut entry.seen_by, state.entity);
                self.gone.push(state.entity);
            }
            if let Some(crowd) = self.crowd.as_deref_mut() {
                crowd.pair_changed(i, slot, entry.kind, visible);
            }
        }
        if !self.gone.is_empty() {
            let span = out.push_viewers(self.gone.iter().copied());
            out.push(id, entry.network_id, EntityUpdate::Remove, span);
        }
        if !self.added.is_empty() {
            let core = self.input.entities.columns();
            let sync = &entry.sync;
            let spawn = EntityUpdate::Spawn {
                kind: entry.kind,
                uuid: core.uuid[row],
                pos: sync.base,
                velocity: sync.movement,
                y_rot: sync.y_rot,
                x_rot: sync.x_rot,
                y_head_rot: sync.y_head_rot,
            };
            let span = out.push_viewers(self.added.iter().copied());
            out.push(id, entry.network_id, spawn, span);
        }
    }
}

/// What the visibility checks of one entity read.
struct Target {
    kind: EntityTypeId,
    pos: Vec3,
    /// Tracking range in blocks.
    range: f64,
    chunk: ChunkPos,
}

impl Target {
    #[inline]
    fn new(input: &TrackerInput<'_>, settings: &TrackerSettings, row: usize) -> Target {
        let core = input.entities.columns();
        Target {
            kind: core.kind[row],
            pos: core.pos[row],
            range: f64::from(settings.range(core.kind[row])),
            chunk: core.section[row].chunk(),
        }
    }
}

/// `TrackedEntity.updatePlayer`'s condition: horizontal distance within the range (capped by
/// the view distance), the entity's chunk inside the viewer's view and already sent.
#[inline]
fn is_visible(input: &TrackerInput<'_>, target: &Target, viewer: &ViewerState) -> bool {
    let delta = viewer.pos - target.pos;
    let distance = delta.x * delta.x + delta.z * delta.z;
    let view = &viewer.view;
    let ring = (target.chunk.x - view.view_center.x)
        .abs()
        .max((target.chunk.z - view.view_center.z).abs());
    target.range > 0.0
        && distance <= target.range * target.range
        && view.sees_chunk(target.chunk)
        && (ring < view.sent_radius || !(input.chunk_pending)(viewer.entity, target.chunk))
}

/// Lazily stored viewer lists of one entity for one tick.
#[derive(Default)]
struct Spans {
    seen: Option<ViewerSpan>,
    moves: Option<ViewerSpan>,
    with_self: Option<ViewerSpan>,
}

impl Spans {
    /// The viewers of movement: all viewers, or in crowd mode the near tier (`movers`).
    fn moves(
        &mut self,
        out: &mut TrackerOutput,
        seen_by: &[EntityId],
        movers: Option<&[EntityId]>,
    ) -> ViewerSpan {
        match movers {
            None => self.seen(out, seen_by),
            Some(movers) => *self
                .moves
                .get_or_insert_with(|| out.push_viewer_list(movers, None)),
        }
    }

    /// `broadcast`: the viewers.
    fn seen(&mut self, out: &mut TrackerOutput, seen_by: &[EntityId]) -> ViewerSpan {
        *self
            .seen
            .get_or_insert_with(|| out.push_viewer_list(seen_by, None))
    }

    /// `broadcastAndSend`: the viewers and, for players, the entity itself.
    fn with_self(
        &mut self,
        out: &mut TrackerOutput,
        seen_by: &[EntityId],
        id: EntityId,
        is_viewer: bool,
    ) -> ViewerSpan {
        if !is_viewer {
            return self.seen(out, seen_by);
        }
        *self
            .with_self
            .get_or_insert_with(|| out.push_viewer_list(seen_by, Some(id)))
    }
}

/// Whether [`send_changes`] sends movement or data of the entity this tick.
#[inline]
fn sends(entry: &Tracked, core: &CoreColumns, row: usize, update_scale: i32) -> bool {
    let flags = core.flags[row];
    let interval = entry
        .kind
        .info()
        .update_interval
        .max(1)
        .saturating_mul(update_scale.max(1));
    entry.sync.tick_count.wrapping_rem(interval) == 0
        || flags.contains(EntityFlags::HAS_IMPULSE)
        || flags.contains(EntityFlags::DATA_DIRTY)
}

/// `ServerEntity.sendChanges` (passengers' list and maps aside). Movement goes to `movers`
/// instead of all viewers if given (the near tier of crowd mode).
fn send_changes(
    entry: &mut Tracked,
    movers: Option<&[EntityId]>,
    core: &CoreColumns,
    row: usize,
    is_viewer: bool,
    update_scale: i32,
    out: &mut TrackerOutput,
) {
    let info = entry.kind.info();
    let flags = core.flags[row];
    let sends = sends(entry, core, row, update_scale);
    let id = entry.id;
    let network_id = entry.network_id;
    let seen_by = entry.seen_by.as_slice();
    let sync = &mut entry.sync;
    let mut spans = Spans::default();
    let mut broadcast = |out: &mut TrackerOutput, update: EntityUpdate| {
        let span = spans.moves(out, seen_by, movers);
        out.push(id, network_id, update, span);
    };

    if sends {
        let rot = core.rot[row];
        let pos = core.pos[row];
        let on_ground = flags.contains(EntityFlags::ON_GROUND);
        let y_rot = pack_degrees(rot.y_rot);
        let x_rot = pack_degrees(rot.x_rot);
        let rot_changed = y_rot != sync.y_rot || x_rot != sync.x_rot;
        let mut send_data = flags.contains(EntityFlags::DATA_DIRTY);

        if flags.contains(EntityFlags::PASSENGER) {
            if rot_changed {
                broadcast(
                    out,
                    EntityUpdate::Rot {
                        y_rot,
                        x_rot,
                        on_ground,
                    },
                );
                sync.y_rot = y_rot;
                sync.x_rot = x_rot;
            }
            sync.base = pos;
            sync.was_riding = true;
        } else {
            sync.teleport_delay = sync.teleport_delay.wrapping_add(1);
            let pos_changed = (pos - sync.base).length_sqr() >= MIN_MOVE_SQR;
            let send_pos = pos_changed || sync.tick_count.wrapping_rem(FORCE_POS_INTERVAL) == 0;
            let mut sent_pos = false;
            let mut sent_rot = false;
            let mut packet = None;
            if sync.tick_count > 0 {
                let encode =
                    |now: f64, base: f64| java_round(now * 4096.0) - java_round(base * 4096.0);
                let dx = encode(pos.x, sync.base.x);
                let dy = encode(pos.y, sync.base.y);
                let dz = encode(pos.z, sync.base.z);
                let short = |d: i64| i16::try_from(d).ok();
                let deltas = short(dx).zip(short(dy)).zip(short(dz));
                match deltas {
                    Some(((dx, dy), dz))
                        if !flags.contains(EntityFlags::PRECISE_POSITION)
                            && sync.teleport_delay <= TELEPORT_DELAY
                            && !sync.was_riding
                            && sync.was_on_ground == on_ground =>
                    {
                        if send_pos && rot_changed {
                            packet = Some(EntityUpdate::PosRot {
                                dx,
                                dy,
                                dz,
                                y_rot,
                                x_rot,
                                on_ground,
                            });
                            sent_pos = true;
                            sent_rot = true;
                        } else if send_pos {
                            packet = Some(EntityUpdate::Pos {
                                dx,
                                dy,
                                dz,
                                on_ground,
                            });
                            sent_pos = true;
                        } else if rot_changed {
                            packet = Some(EntityUpdate::Rot {
                                y_rot,
                                x_rot,
                                on_ground,
                            });
                            sent_rot = true;
                        }
                    }
                    _ => {
                        sync.was_on_ground = on_ground;
                        sync.teleport_delay = 0;
                        packet = Some(EntityUpdate::Sync {
                            pos,
                            velocity: core.velocity[row],
                            y_rot: rot.y_rot,
                            x_rot: rot.x_rot,
                            on_ground,
                        });
                        sent_pos = true;
                        sent_rot = true;
                    }
                }
            }
            if (flags.contains(EntityFlags::HAS_IMPULSE)
                || info.track_deltas
                || flags.contains(EntityFlags::FALL_FLYING))
                && sync.tick_count > 0
            {
                let velocity = core.velocity[row];
                let change = velocity.distance_to_sqr(sync.movement);
                if change > 1.0e-7 || (change > 0.0 && velocity.length_sqr() == 0.0) {
                    sync.movement = velocity;
                    broadcast(out, EntityUpdate::TrackedMotion(velocity));
                }
            }
            if let Some(packet) = packet {
                broadcast(out, packet);
            }
            if send_data {
                let span = spans.with_self(out, seen_by, id, is_viewer);
                out.push(id, network_id, EntityUpdate::Data, span);
                send_data = false;
            }
            if sent_pos {
                sync.base = pos;
            }
            if sent_rot {
                sync.y_rot = y_rot;
                sync.x_rot = x_rot;
            }
            sync.was_riding = false;
        }
        if send_data {
            let span = spans.with_self(out, seen_by, id, is_viewer);
            out.push(id, network_id, EntityUpdate::Data, span);
        }

        let y_head_rot = pack_degrees(rot.y_head_rot);
        if y_head_rot != sync.y_head_rot {
            let span = spans.moves(out, seen_by, movers);
            out.push(id, network_id, EntityUpdate::HeadRot(y_head_rot), span);
            sync.y_head_rot = y_head_rot;
        }
    }
    sync.tick_count = sync.tick_count.wrapping_add(1);
    if flags.contains(EntityFlags::HURT_MARKED) {
        let span = spans.with_self(out, seen_by, id, is_viewer);
        out.push(
            id,
            network_id,
            EntityUpdate::Motion(core.velocity[row]),
            span,
        );
    }
}

fn in_square(center: ChunkPos, radius: i32, chunk: ChunkPos) -> bool {
    (chunk.x - center.x).abs() <= radius && (chunk.z - center.z).abs() <= radius
}

fn for_square(center: ChunkPos, radius: i32, mut f: impl FnMut(ChunkPos)) {
    for x in center.x - radius..=center.x + radius {
        for z in center.z - radius..=center.z + radius {
            f(ChunkPos::new(x, z));
        }
    }
}

/// Chunks per side of a cell of `nearby`, as a shift: a viewer registers in the cells its
/// square overlaps, so a crowd moving between chunks rewrites few, rarely changing lists.
const CELL_SHIFT: i32 = 2;

/// The cells `[min x, min z, max x, max z]` the square around `center` overlaps.
fn cells_of(center: ChunkPos, radius: i32) -> [i32; 4] {
    [
        (center.x - radius) >> CELL_SHIFT,
        (center.z - radius) >> CELL_SHIFT,
        (center.x + radius) >> CELL_SHIFT,
        (center.z + radius) >> CELL_SHIFT,
    ]
}

/// The key in `nearby` of the cell holding `chunk`.
fn cell_of(chunk: ChunkPos) -> i64 {
    ChunkPos::new(chunk.x >> CELL_SHIFT, chunk.z >> CELL_SHIFT).as_long()
}

/// Moves a viewer's registration from the cells of the square around `old` to those around
/// `new`, touching only the cells that differ. The lists stay sorted by slot: a removal is a
/// binary search in a list of every viewer of a crowd, and the pair checks visit the viewers
/// in memory order.
fn move_square(
    nearby: &mut FxHashMap<i64, Vec<u32>>,
    viewer: u32,
    old: Option<ChunkPos>,
    new: Option<ChunkPos>,
    radius: i32,
) {
    let old = old.map(|center| cells_of(center, radius));
    let new = new.map(|center| cells_of(center, radius));
    if old == new {
        return;
    }
    let inside = |cells: Option<[i32; 4]>, x: i32, z: i32| {
        cells.is_some_and(|[x0, z0, x1, z1]| (x0..=x1).contains(&x) && (z0..=z1).contains(&z))
    };
    if let Some([x0, z0, x1, z1]) = old {
        for x in x0..=x1 {
            for z in z0..=z1 {
                let key = ChunkPos::new(x, z).as_long();
                if !inside(new, x, z)
                    && let Some(list) = nearby.get_mut(&key)
                {
                    if let Ok(index) = list.binary_search(&viewer) {
                        list.remove(index);
                    }
                    if list.is_empty() {
                        nearby.remove(&key);
                    }
                }
            }
        }
    }
    if let Some([x0, z0, x1, z1]) = new {
        for x in x0..=x1 {
            for z in z0..=z1 {
                if !inside(old, x, z) {
                    let list = nearby.entry(ChunkPos::new(x, z).as_long()).or_default();
                    if let Err(index) = list.binary_search(&viewer) {
                        list.insert(index, viewer);
                    }
                }
            }
        }
    }
}

fn remove_from(map: &mut FxHashMap<i64, Vec<u32>>, key: i64, id: u32) {
    if let Some(list) = map.get_mut(&key) {
        remove_unordered(list, id);
        if list.is_empty() {
            map.remove(&key);
        }
    }
}

fn remove_unordered<T: PartialEq>(list: &mut Vec<T>, id: T) {
    if let Some(index) = list.iter().position(|e| *e == id) {
        list.swap_remove(index);
    }
}
