//! Crowd mode of the [`ChunkTracker`](super::ChunkTracker) (`performance.crowd`): distance tiers
//! of player movement and the cap on visible players. It exists only while a crowd rule is on;
//! without it the tracker runs Vanilla's code alone.
//!
//! Tiers: a viewer farther than the near radius from a player is in that player's far tier.
//! The near tier gets Vanilla's stream of relative moves (`ServerEntity.sendChanges`); the far
//! tier gets a second, coarser stream every `far-interval` ticks with its own base position. A
//! client's position codec follows one stream at a time, so a viewer changing streams first
//! gets that stream's base as an absolute position (`entity_position_sync`): one coming near at
//! once, one going far with the next far update.
//!
//! Cap: each viewer sees players only within its own radius, the distance of its
//! `max-visible-players`-th nearest player. The radius is fitted whenever the viewer re-checks
//! what it sees (section change, join, chunks sent) and when its count of visible players left
//! a band around the cap.

use vanillite_base::Vec3;
use vanillite_base::math::{java_round, pack_degrees};
use vanillite_data::entity::{EntityTypeId, ids};

use super::{MIN_MOVE_SQR, SlotSet, Tracked, ViewerState, index};
use crate::entity::{CoreColumns, EntityFlags, EntityId};
use crate::tracking::{CrowdSettings, EntityUpdate, TrackerOutput};

/// The far stream of one player: what its far tier last got.
#[derive(Debug)]
struct FarStream {
    id: EntityId,
    /// Viewer slots in the far tier.
    far: SlotSet,
    /// Far-tier viewer slots whose client follows this stream (they got its base).
    synced: SlotSet,
    base: Vec3,
    y_rot: i8,
    x_rot: i8,
    y_head_rot: i8,
}

/// The cap of one viewer.
#[derive(Clone, Copy, Debug)]
struct ViewerCap {
    /// Squared horizontal distance within which it sees players.
    range_sqr: f64,
    /// Players it sees.
    players: u32,
}

impl ViewerCap {
    /// A new viewer sees no player until its first fit, which comes in the same update.
    const NEW: ViewerCap = ViewerCap {
        range_sqr: 0.0,
        players: 0,
    };
}

/// The crowd state of one tracker.
#[derive(Debug)]
pub(super) struct Crowd {
    /// Squared near radius; `None` = no tiers.
    near_sqr: Option<f64>,
    far_interval: i32,
    cap: Option<u32>,
    /// Far streams by entity index (players only).
    streams: Vec<Option<FarStream>>,
    /// Caps by viewer slot.
    caps: Vec<ViewerCap>,
    /// Viewer slots whose count may have left the band around the cap.
    recheck: Vec<u32>,
    // The tiers of the player being updated.
    /// The viewers of Vanilla's stream.
    pub(super) near: Vec<EntityId>,
    /// Viewers that came near: they get the near base.
    promoted: Vec<EntityId>,
    /// Far viewers that get the far base on this far update.
    joining: Vec<EntityId>,
    /// Far viewers that follow the far stream.
    stream: Vec<EntityId>,
}

impl Crowd {
    /// The crowd state for `settings`, `None` while no crowd rule is on.
    pub(super) fn new(settings: &CrowdSettings) -> Option<Crowd> {
        settings.enabled().then(|| Crowd {
            near_sqr: settings.near_radius.map(|r| f64::from(r) * f64::from(r)),
            far_interval: i32::from(settings.far_interval.max(1)),
            cap: settings.max_visible_players.map(|cap| cap.max(1)),
            streams: Vec::new(),
            caps: Vec::new(),
            recheck: Vec::new(),
            near: Vec::new(),
            promoted: Vec::new(),
            joining: Vec::new(),
            stream: Vec::new(),
        })
    }

    /// Whether the movement of `kind` goes through the tiers.
    #[inline]
    pub(super) fn tiers(&self, kind: EntityTypeId) -> bool {
        self.near_sqr.is_some() && kind == ids::PLAYER
    }

    /// A viewer took `slot`.
    pub(super) fn new_viewer(&mut self, slot: u32) {
        let i = slot as usize;
        if self.caps.len() <= i {
            self.caps.resize(i + 1, ViewerCap::NEW);
        }
        self.caps[i] = ViewerCap::NEW;
    }

    /// Whether the cap lets the viewer in `slot` at `viewer` see an entity of `kind` at `pos`.
    #[inline]
    pub(super) fn sees(&self, slot: u32, viewer: Vec3, kind: EntityTypeId, pos: Vec3) -> bool {
        self.cap.is_none()
            || self
                .caps
                .get(slot as usize)
                .is_none_or(|cap| within(cap.range_sqr, viewer, kind, pos))
    }

    /// The pair of the entity with index `i` and the viewer in `slot` started (`added`) or
    /// ended: the pair starts over in the near stream (a spawn carries the near base).
    pub(super) fn pair_changed(&mut self, i: u32, slot: u32, kind: EntityTypeId, added: bool) {
        if let Some(Some(stream)) = self.streams.get_mut(i as usize) {
            stream.far.remove(slot);
            stream.synced.remove(slot);
        }
        if kind == ids::PLAYER
            && let Some(cap) = self.cap
            && let Some(state) = self.caps.get_mut(slot as usize)
        {
            let slack = slack(cap);
            if added {
                state.players += 1;
                if state.players > cap + slack {
                    self.recheck.push(slot);
                }
            } else {
                state.players = state.players.saturating_sub(1);
                if state.players + slack < cap && state.range_sqr < f64::INFINITY {
                    self.recheck.push(slot);
                }
            }
        }
    }

    /// Calls `refit` for each viewer slot whose count of visible players is outside the band
    /// around the cap.
    pub(super) fn take_rechecks(&mut self, mut refit: impl FnMut(u32)) {
        let Some(cap) = self.cap else {
            return;
        };
        let slack = slack(cap);
        for slot in self.recheck.drain(..) {
            if self.caps.get(slot as usize).is_some_and(|state| {
                state.players > cap + slack
                    || (state.players + slack < cap && state.range_sqr < f64::INFINITY)
            }) {
                refit(slot);
            }
        }
    }

    /// The squared radius of the viewer's cap: the distance of its cap-th nearest player among
    /// `candidates` (entity indices of this update's tracked entities); `None` without a cap.
    /// Reads only, so the viewers' checks can run in parallel; [`Crowd::set_range`] applies it.
    pub(super) fn fit_range(
        &self,
        viewer: &ViewerState,
        candidates: &[u32],
        tracked: &[Option<Tracked>],
        core: &CoreColumns,
        stamp: u32,
        distances: &mut Vec<f64>,
    ) -> Option<f64> {
        let cap = self.cap?;
        distances.clear();
        for &i in candidates {
            let Some(Some(entry)) = tracked.get(i as usize) else {
                continue;
            };
            if entry.stamp == stamp && entry.kind == ids::PLAYER && entry.id != viewer.entity {
                let pos = core.pos[entry.row as usize];
                distances.push(horizontal_sqr(viewer.pos, pos));
            }
        }
        let nth = cap as usize - 1;
        Some(if distances.len() > nth + 1 {
            *distances.select_nth_unstable_by(nth, f64::total_cmp).1
        } else {
            f64::INFINITY
        })
    }

    /// Sets the squared cap radius of the viewer in `slot` (from [`Crowd::fit_range`]).
    pub(super) fn set_range(&mut self, slot: u32, range_sqr: f64) {
        if let Some(state) = self.caps.get_mut(slot as usize) {
            state.range_sqr = range_sqr;
        }
    }

    /// Whether the far stream of a player with this tick count sends this tick.
    #[inline]
    pub(super) fn far_due(&self, tick_count: i32, update_scale: i32) -> bool {
        tick_count.wrapping_rem(self.far_interval.saturating_mul(update_scale.max(1))) == 0
    }

    /// Empties the tier lists: nothing of the player is sent this tick.
    pub(super) fn clear_tiers(&mut self) {
        self.near.clear();
        self.promoted.clear();
        self.joining.clear();
        self.stream.clear();
    }

    /// Sorts the viewers of the player `entry` into the tiers for this tick. `far_due`: the
    /// far stream sends this tick.
    pub(super) fn split(
        &mut self,
        entry: &Tracked,
        core: &CoreColumns,
        row: usize,
        viewers: &[Option<ViewerState>],
        far_due: bool,
    ) {
        self.clear_tiers();
        let near_sqr = self.near_sqr.unwrap_or(f64::INFINITY);
        let i = index(entry.id) as usize;
        if self.streams.len() <= i {
            self.streams.resize_with(i + 1, || None);
        }
        let stream = &mut self.streams[i];
        if stream.as_ref().is_none_or(|s| s.id != entry.id) {
            let rot = core.rot[row];
            *stream = Some(FarStream {
                id: entry.id,
                far: SlotSet::default(),
                synced: SlotSet::default(),
                base: core.pos[row],
                y_rot: pack_degrees(rot.y_rot),
                x_rot: pack_degrees(rot.x_rot),
                y_head_rot: pack_degrees(rot.y_head_rot),
            });
        }
        let Some(stream) = stream.as_mut() else {
            return;
        };
        let pos = core.pos[row];
        for slot in entry.seen_slots.iter() {
            let Some(Some(viewer)) = viewers.get(slot as usize) else {
                continue;
            };
            if horizontal_sqr(viewer.pos, pos) <= near_sqr {
                if stream.far.contains(slot) {
                    stream.far.remove(slot);
                    stream.synced.remove(slot);
                    self.promoted.push(viewer.entity);
                } else {
                    self.near.push(viewer.entity);
                }
            } else {
                stream.far.insert(slot);
                if far_due {
                    if stream.synced.contains(slot) {
                        self.stream.push(viewer.entity);
                    } else {
                        stream.synced.insert(slot);
                        self.joining.push(viewer.entity);
                    }
                }
            }
        }
    }

    /// After Vanilla's update of the player `entry` (`send_changes` to `near`): the near base
    /// to the viewers that came near, and on a far update the movement since the last one.
    pub(super) fn send_far(
        &mut self,
        entry: &Tracked,
        core: &CoreColumns,
        row: usize,
        out: &mut TrackerOutput,
    ) {
        let (id, network_id) = (entry.id, entry.network_id);
        let flags = core.flags[row];
        let on_ground = flags.contains(EntityFlags::ON_GROUND);
        if !self.promoted.is_empty() {
            let sync = &entry.sync;
            let span = out.push_viewer_list(&self.promoted, None);
            let update = EntityUpdate::Sync {
                pos: sync.base,
                velocity: sync.movement,
                y_rot: unpack_degrees(sync.y_rot),
                x_rot: unpack_degrees(sync.x_rot),
                on_ground,
            };
            out.push(id, network_id, update, span);
            out.push(id, network_id, EntityUpdate::HeadRot(sync.y_head_rot), span);
        }
        if self.stream.is_empty() && self.joining.is_empty() {
            return;
        }
        let Some(Some(stream)) = self.streams.get_mut(index(id) as usize) else {
            return;
        };
        let pos = core.pos[row];
        let rot = core.rot[row];
        let y_rot = pack_degrees(rot.y_rot);
        let x_rot = pack_degrees(rot.x_rot);
        let y_head_rot = pack_degrees(rot.y_head_rot);
        let pos_changed = (pos - stream.base).length_sqr() >= MIN_MOVE_SQR;
        if !self.stream.is_empty() {
            let rot_changed = y_rot != stream.y_rot || x_rot != stream.x_rot;
            let delta = |now: f64, base: f64| {
                i16::try_from(java_round(now * 4096.0) - java_round(base * 4096.0)).ok()
            };
            let deltas = if flags.contains(EntityFlags::PASSENGER) {
                None
            } else {
                delta(pos.x, stream.base.x)
                    .zip(delta(pos.y, stream.base.y))
                    .zip(delta(pos.z, stream.base.z))
            };
            let update = match deltas {
                _ if !pos_changed && !rot_changed => None,
                _ if !pos_changed => Some(EntityUpdate::Rot {
                    y_rot,
                    x_rot,
                    on_ground,
                }),
                Some(((dx, dy), dz)) if rot_changed => Some(EntityUpdate::PosRot {
                    dx,
                    dy,
                    dz,
                    y_rot,
                    x_rot,
                    on_ground,
                }),
                Some(((dx, dy), dz)) => Some(EntityUpdate::Pos {
                    dx,
                    dy,
                    dz,
                    on_ground,
                }),
                None => Some(EntityUpdate::Sync {
                    pos,
                    velocity: core.velocity[row],
                    y_rot: rot.y_rot,
                    x_rot: rot.x_rot,
                    on_ground,
                }),
            };
            let head =
                (y_head_rot != stream.y_head_rot).then_some(EntityUpdate::HeadRot(y_head_rot));
            if update.is_some() || head.is_some() {
                let span = out.push_viewer_list(&self.stream, None);
                for update in update.into_iter().chain(head) {
                    out.push(id, network_id, update, span);
                }
            }
        }
        if pos_changed {
            stream.base = pos;
        }
        stream.y_rot = y_rot;
        stream.x_rot = x_rot;
        stream.y_head_rot = y_head_rot;
        if !self.joining.is_empty() {
            let span = out.push_viewer_list(&self.joining, None);
            let update = EntityUpdate::Sync {
                pos: stream.base,
                velocity: core.velocity[row],
                y_rot: unpack_degrees(y_rot),
                x_rot: unpack_degrees(x_rot),
                on_ground,
            };
            out.push(id, network_id, update, span);
            out.push(id, network_id, EntityUpdate::HeadRot(y_head_rot), span);
        }
    }
}

/// How far the count of visible players may leave the cap before the radius is fitted again.
fn slack(cap: u32) -> u32 {
    (cap / 8).max(1)
}

/// Whether a cap radius of `range_sqr` lets a viewer at `viewer` see an entity of `kind` at
/// `pos` (the cap counts players only).
#[inline]
pub(super) fn within(range_sqr: f64, viewer: Vec3, kind: EntityTypeId, pos: Vec3) -> bool {
    kind != ids::PLAYER || horizontal_sqr(viewer, pos) <= range_sqr
}

/// The squared horizontal distance, as the tracker measures ranges.
#[inline]
fn horizontal_sqr(a: Vec3, b: Vec3) -> f64 {
    let (dx, dz) = (a.x - b.x, a.z - b.z);
    dx * dx + dz * dz
}

/// The degrees a client reads from a protocol angle byte: they pack to the same byte again.
fn unpack_degrees(packed: i8) -> f32 {
    f32::from(packed) * 360.0 / 256.0
}
