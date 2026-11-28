//! The built-in tracker: Vanilla's `ChunkMap.TrackedEntity` and `ServerEntity` over
//! chunk-indexed nearby-player sets.
//!
//! Scaling: every viewer is registered in the cells of 4×4 chunks that overlap a square
//! around it covering the largest tracking range, updated incrementally when that set of
//! cells changes. An entity that changes section checks only the viewers registered in its
//! chunk's cell and those that see it; a viewer that changes section checks only the entities
//! in its square and those it sees. No step iterates all pairs. The per-tick cost is one pass over the entities (Vanilla's
//! `sendChanges`) plus work proportional to the section changes.

use std::sync::{Mutex, PoisonError};

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
    /// Its section at the last update (`TrackedEntity.lastSectionPos`).
    section: Option<SectionPos>,
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
    // Scratch buffers, kept for their capacity.
    moved: Vec<u32>,
    candidates: Vec<u32>,
    added: Vec<EntityId>,
    gone: Vec<EntityId>,
    gone_slots: Vec<u32>,
    /// Rows of the entities that changed section (or are new) this update.
    changed: Vec<u32>,
    /// Pairs whose visibility changes: (row, viewer slot) for the section changes, (viewer
    /// slot, entity index) for the moved viewers.
    changes: Vec<(u32, u32)>,
}

impl ChunkTracker {
    /// A tracker with the given ranges.
    pub fn new(settings: TrackerSettings) -> ChunkTracker {
        let radius = settings.max_range().div_euclid(16) + 2;
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
            moved: Vec::new(),
            candidates: Vec::new(),
            added: Vec::new(),
            gone: Vec::new(),
            gone_slots: Vec::new(),
            changed: Vec::new(),
            changes: Vec::new(),
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
                        section: None,
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
                    slot
                }
            };
            let Some(Some(state)) = self.viewers.get_mut(slot as usize) else {
                continue;
            };
            state.row = u32::try_from(row).unwrap_or(u32::MAX);
            state.pos = core.pos[row];
            // `ChunkMap.tick`: a player that changed sections is checked against every entity
            // again (so is one whose view changed); chunks sent to it alone pair nothing.
            let section = core.section[row];
            let recheck = state.square.is_none()
                || state.section != Some(section)
                || state.view.view_center != viewer.view_center
                || state.view.view_distance != viewer.view_distance;
            state.view = *viewer;
            state.section = Some(section);
            if recheck {
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
        }
    }
    let span = out.push_viewers(tracked.seen_by.iter().copied());
    out.push(tracked.id, tracked.network_id, EntityUpdate::Remove, span);
}

impl EntityTracker for ChunkTracker {
    fn release_memory(&mut self) {
        while matches!(self.tracked.last(), Some(None)) {
            self.tracked.pop();
        }
        self.tracked.shrink_to_fit();
        while self.viewer_slot.last() == Some(&NONE) {
            self.viewer_slot.pop();
        }
        self.viewer_slot.shrink_to_fit();
        self.viewers.shrink_to_fit();
        self.free_viewers.shrink_to_fit();
        self.by_chunk.shrink_to_fit();
        self.nearby.shrink_to_fit();
        self.moved = Vec::new();
        self.candidates = Vec::new();
        self.added = Vec::new();
        self.gone = Vec::new();
        self.gone_slots = Vec::new();
        self.changed = Vec::new();
        self.changes = Vec::new();
    }

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
        // Viewers the server moved for their client: check them again.
        for &viewer in input.moved_viewers {
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
            moved,
            candidates,
            added,
            gone,
            changed,
            changes,
            ..
        } = self;
        let radius = *radius;
        let stamp = *stamp;

        // The visibility checks of the section changes read only the last update's pairs and
        // this update's positions, so they run first, in parallel; the loop below applies the
        // pairs that change in Vanilla's order.
        changed.clear();
        changed.extend((0..core.id.len()).filter_map(|row| {
            let id = core.id[row];
            let same = tracked
                .get(index(id) as usize)
                .and_then(Option::as_ref)
                .is_some_and(|t| t.id == id && t.section == Some(core.section[row]));
            (!same).then(|| u32::try_from(row).unwrap_or(u32::MAX))
        }));
        changes.clear();
        {
            let (tracked, viewers, nearby) = (&*tracked, &*viewers, &*nearby);
            map_parts(
                input,
                changed.len() * viewers.len(),
                changed,
                changes,
                |rows, out| {
                    let mut near_set = SlotSet::default();
                    let mut candidates = Vec::new();
                    for &row in rows {
                        section_changes(
                            input,
                            settings,
                            tracked,
                            viewers,
                            nearby,
                            row as usize,
                            &mut near_set,
                            &mut candidates,
                            out,
                        );
                    }
                },
            );
        }
        let mut next_change = 0;

        let mut pairing = Pairing {
            input,
            settings,
            viewers,
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
                forget(stale, by_chunk, viewer_slot, pairing.viewers, out);
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
                let row32 = u32::try_from(row).unwrap_or(u32::MAX);
                while let Some(&(r, slot)) = changes.get(next_change)
                    && r == row32
                {
                    candidates.push(slot);
                    next_change += 1;
                }
                if !candidates.is_empty() {
                    pairing.update(entry, row, candidates, out);
                }
                if let Some(slot) = own_slot
                    && let Some(Some(state)) = pairing.viewers.get_mut(slot as usize)
                {
                    queue(moved, state, slot, stamp);
                }
            }
            send_changes(
                entry,
                core,
                row,
                id,
                own_slot.is_some(),
                input.update_scale,
                out,
            );
        }

        // Viewers that moved to another section (or joined) re-check what they can see: the
        // checks in parallel (each reads its own viewer's pairs), the changes in their order.
        changes.clear();
        {
            let (tracked, viewers, by_chunk) = (&*tracked, &*pairing.viewers, &*by_chunk);
            let entities = core.id.len();
            map_parts(
                input,
                moved.len() * entities,
                moved,
                changes,
                |slots, out| {
                    let mut candidates = Vec::new();
                    for &slot in slots {
                        if let Some(Some(state)) = viewers.get(slot as usize) {
                            viewer_changes(
                                input,
                                settings,
                                tracked,
                                by_chunk,
                                radius,
                                stamp,
                                state,
                                slot,
                                &mut candidates,
                                out,
                            );
                        }
                    }
                },
            );
        }
        for &(viewer, i) in changes.iter() {
            let Some(Some(entry)) = tracked.get_mut(i as usize) else {
                continue;
            };
            let row = entry.row as usize;
            pairing.update_one(entry, row, viewer, out);
        }
    }
}

/// Pairs per part below which a parallel visibility check is not worth a thread.
const PAIRS_PER_PART: usize = if cfg!(test) { 1 } else { 16 * 1024 };

/// Runs `job` over contiguous parts of `items` (about `work` pair checks in all) on
/// [`TrackerInput::fork`] and appends what the parts produced to `out` in the order of
/// `items`; inline when the work is small or there is one thread.
fn map_parts<T: Sync, R: Send>(
    input: &TrackerInput<'_>,
    work: usize,
    items: &[T],
    out: &mut Vec<R>,
    job: impl Fn(&[T], &mut Vec<R>) + Sync,
) {
    // More parts than threads, so uneven parts balance out.
    let parts = if input.threads > 1 {
        (input.threads * 2)
            .min(work / PAIRS_PER_PART)
            .min(items.len())
            .max(1)
    } else {
        1
    };
    if parts == 1 {
        job(items, out);
        return;
    }
    let per = items.len().div_ceil(parts);
    let slots: Vec<Mutex<Vec<R>>> = (0..parts).map(|_| Mutex::new(Vec::new())).collect();
    (input.fork)(parts, &|p| {
        let part = items
            .get(p * per..((p + 1) * per).min(items.len()))
            .unwrap_or(&[]);
        let mut local = Vec::new();
        job(part, &mut local);
        if let Some(slot) = slots.get(p) {
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = local;
        }
    });
    for slot in slots {
        out.append(&mut slot.into_inner().unwrap_or_else(PoisonError::into_inner));
    }
}

/// The viewers whose pairing with the entity in `row` changes because it changed section
/// (or is new): those registered near its chunk and those that see it. Appends
/// `(row, slot)` in Vanilla's candidate order. Reads only.
#[allow(clippy::too_many_arguments)]
fn section_changes(
    input: &TrackerInput<'_>,
    settings: &TrackerSettings,
    tracked: &[Option<Tracked>],
    viewers: &[Option<ViewerState>],
    nearby: &FxHashMap<i64, Vec<u32>>,
    row: usize,
    near_set: &mut SlotSet,
    candidates: &mut Vec<u32>,
    out: &mut Vec<(u32, u32)>,
) {
    let core = input.entities.columns();
    let id = core.id[row];
    let chunk = core.section[row].chunk();
    let seen = tracked
        .get(index(id) as usize)
        .and_then(Option::as_ref)
        .filter(|t| t.id == id)
        .map(|t| &t.seen_slots);
    candidates.clear();
    if let Some(list) = nearby.get(&cell_of(chunk)) {
        candidates.extend_from_slice(list);
    }
    // Viewers that see it but are no longer registered near its chunk.
    if let Some(seen) = seen {
        let near = candidates.len();
        for &slot in candidates.iter() {
            near_set.insert(slot);
        }
        for (index, &word) in (0u32..).zip(&seen.0) {
            let mut bits = word & !near_set.word(index as usize);
            while bits != 0 {
                candidates.push(index * 64 + bits.trailing_zeros());
                bits &= bits - 1;
            }
        }
        for &slot in &candidates[..near] {
            near_set.remove(slot);
        }
    }
    let target = Target::new(input, settings, row);
    let row = u32::try_from(row).unwrap_or(u32::MAX);
    for &slot in candidates.iter() {
        let Some(Some(state)) = viewers.get(slot as usize) else {
            continue;
        };
        if state.entity == id {
            continue;
        }
        let was = seen.is_some_and(|s| s.contains(slot));
        if is_visible(input, &target, state) != was {
            out.push((row, slot));
        }
    }
}

/// The entities whose pairing with the viewer in `slot` changes because it moved: those in
/// its square and those it sees. Appends `(slot, entity index)` in Vanilla's candidate order.
/// Reads only.
#[allow(clippy::too_many_arguments)]
fn viewer_changes(
    input: &TrackerInput<'_>,
    settings: &TrackerSettings,
    tracked: &[Option<Tracked>],
    by_chunk: &FxHashMap<i64, Vec<u32>>,
    radius: i32,
    stamp: u32,
    state: &ViewerState,
    slot: u32,
    candidates: &mut Vec<u32>,
    out: &mut Vec<(u32, u32)>,
) {
    let Some(square) = state.square else {
        return;
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
    for &i in candidates.iter() {
        let Some(Some(entry)) = tracked.get(i as usize) else {
            continue;
        };
        if entry.stamp != stamp || state.entity == entry.id {
            continue;
        }
        let target = Target::new(input, settings, entry.row as usize);
        if is_visible(input, &target, state) != state.viewing.contains(i) {
            out.push((slot, i));
        }
    }
}

/// Visibility changes between entities and viewers (`TrackedEntity.updatePlayer`).
struct Pairing<'a, 'i> {
    input: &'a TrackerInput<'i>,
    settings: &'a TrackerSettings,
    viewers: &'a mut Vec<Option<ViewerState>>,
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
        if is_visible(self.input, &target, state) != state.viewing.contains(index(entry.id)) {
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
            let visible = is_visible(self.input, &target, state);
            let seen = entry.seen_slots.contains(slot);
            if visible && !seen {
                state.viewing.insert(i);
                entry.seen_slots.insert(slot);
                entry.seen_by.push(state.entity);
                self.added.push(state.entity);
            } else if !visible && seen {
                state.viewing.remove(i);
                entry.seen_slots.remove(slot);
                remove_unordered(&mut entry.seen_by, state.entity);
                self.gone.push(state.entity);
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
    with_self: Option<ViewerSpan>,
}

impl Spans {
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

/// `ServerEntity.sendChanges` (passengers' list and maps aside).
fn send_changes(
    entry: &mut Tracked,
    core: &CoreColumns,
    row: usize,
    id: EntityId,
    is_viewer: bool,
    update_scale: i32,
    out: &mut TrackerOutput,
) {
    let info = entry.kind.info();
    let flags = core.flags[row];
    let network_id = entry.network_id;
    let seen_by = entry.seen_by.as_slice();
    let sync = &mut entry.sync;
    let mut spans = Spans::default();
    let mut broadcast = |out: &mut TrackerOutput, update: EntityUpdate| {
        let span = spans.seen(out, seen_by);
        out.push(id, network_id, update, span);
    };

    let interval = info
        .update_interval
        .max(1)
        .saturating_mul(update_scale.max(1));
    if sync.tick_count.wrapping_rem(interval) == 0
        || flags.contains(EntityFlags::HAS_IMPULSE)
        || flags.contains(EntityFlags::DATA_DIRTY)
    {
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
            let span = spans.seen(out, seen_by);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::NewEntity;
    use crate::tracking::{Fork, inline_fork};
    use vanillite_config::{Profile, RuleSet, WorldRules};
    use vanillite_data::entity::ids;

    /// Runs the jobs of a fork in reverse order, as threads might finish them.
    fn reverse_fork(n: usize, job: &(dyn Fn(usize) + Sync)) {
        for i in (0..n).rev() {
            job(i);
        }
    }

    /// A small deterministic generator (no draws from any Vanilla random).
    fn next(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        f64::from(u32::try_from(*seed >> 40).unwrap_or(0)) / f64::from(1u32 << 24)
    }

    /// Stream identity (#269): the parallel visibility checks give the same messages in the
    /// same order as the serial ones, update after update.
    #[test]
    fn parallel_checks_match_serial_order() {
        let settings =
            TrackerSettings::new(4, 100, &WorldRules::for_profile(Profile::Vanilla).tracking);
        let mut store = EntityStore::new();
        let mut seed = 7u64;
        let ids: Vec<EntityId> = (0..48u16)
            .map(|n| {
                let pos = Vec3::new(
                    next(&mut seed) * 160.0 - 80.0,
                    64.0,
                    next(&mut seed) * 160.0 - 80.0,
                );
                let new =
                    NewEntity::new(ids::PLAYER, NetworkId(i32::from(n) + 1), u128::from(n), pos);
                store.spawn(new, |_| u32::from(n))
            })
            .collect();
        let mut serial = ChunkTracker::new(settings.clone());
        let mut parallel = ChunkTracker::new(settings);
        for round in 0..12 {
            for &id in &ids {
                if next(&mut seed) < 0.5 {
                    let pos = store.columns().pos[store.row(id).unwrap_or(0)];
                    let step = Vec3::new(
                        next(&mut seed) * 40.0 - 20.0,
                        0.0,
                        next(&mut seed) * 40.0 - 20.0,
                    );
                    if let Some(mut e) = store.get_mut(id) {
                        e.set_pos(pos + step);
                    }
                }
            }
            let viewers: Vec<Viewer> = ids
                .iter()
                .enumerate()
                .filter(|(n, _)| (n + round) % 5 != 0)
                .map(|(_, &entity)| {
                    let pos = store.columns().pos[store.row(entity).unwrap_or(0)];
                    let view_center =
                        ChunkPos::new((pos.x.floor() as i32) >> 4, (pos.z.floor() as i32) >> 4);
                    Viewer {
                        entity,
                        view_center,
                        view_distance: 4,
                        sent_radius: 2,
                    }
                })
                .collect();
            let moved: Vec<EntityId> = ids.iter().copied().skip(round % 3).step_by(3).collect();
            let pending = |player: EntityId, chunk: ChunkPos| {
                (index(player) + chunk.x.unsigned_abs()).is_multiple_of(4)
            };
            let mut outputs = [TrackerOutput::default(), TrackerOutput::default()];
            for (tracker, (threads, out)) in [&mut serial, &mut parallel]
                .into_iter()
                .zip([1usize, 4].into_iter().zip(outputs.iter_mut()))
            {
                let fork: &Fork<'_> = if threads == 1 {
                    &inline_fork
                } else {
                    &reverse_fork
                };
                let input = TrackerInput {
                    entities: &store,
                    viewers: &viewers,
                    chunk_pending: &pending,
                    moved_viewers: &moved,
                    update_scale: 1,
                    threads,
                    fork,
                };
                tracker.update(&input, out);
            }
            let [a, b] = &outputs;
            assert!(!a.messages.is_empty(), "round {round} sends nothing");
            assert_eq!(format!("{a:?}"), format!("{b:?}"), "round {round}");
        }
    }
}
