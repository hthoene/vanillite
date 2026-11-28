//! The locator bar of a world (`ServerWaypointManager`, `WaypointTransmitter`): every player
//! transmits a waypoint the other players of the world receive as a block position, a chunk or,
//! when really far, an azimuth. Sneaking, invisibility and heads (the `waypoint_transmit_range`
//! modifiers) hide a player; spectators receive all.
//!
//! Vanilla updates the connections whenever a player moves; here they are updated once per
//! tick for the players that moved or changed, with the same connections and packets.

use std::sync::{Arc, OnceLock};

use vanillite_base::hash::{FxHashMap, FxHashSet};
use vanillite_base::{BlockPos, ChunkPos, Vec3};
use vanillite_config::GameMode;
use vanillite_data::entity::ids::PLAYER;
use vanillite_game::effect::attribute::Attribute;
use vanillite_game::entities::Entities;
use vanillite_game::item::EquipmentSlot;
use vanillite_game::player::ChunkView;
use vanillite_game::player::sink::PacketSink;
use vanillite_protocol::packets::play::clientbound::{Waypoint, WaypointOperation, WaypointTarget};
use vanillite_protocol::types::{Identifier, Uuid};
use vanillite_world::entity::{EntityId, Pose};

use vanillite_protocol::compress::Codec;
use vanillite_protocol::frame::encode_frame;
use vanillite_protocol::tap::{BatchMark, PacketTap, Stage, Verdict};
use vanillite_protocol::{Direction, Packet, SharedPacket, State};

use crate::compute::Compute;
use crate::global::Global;

/// `WaypointTransmitter.REALLY_FAR_DISTANCE`.
const REALLY_FAR: f64 = 332.0;
/// `AttributeModifier.Operation.ADD_MULTIPLIED_TOTAL`.
const ADD_MULTIPLIED_TOTAL: u8 = 2;
/// `TeamColor.BLACK` on the locator bar (`Waypoint.Icon.cloneAndAssignStyle`).
const BLACK: i32 = -13_619_152;
/// `TeamColor.rgb` by colour id.
const TEAM_RGB: [i32; 16] = [
    0x00_0000, 0x00_00AA, 0x00_AA00, 0x00_AAAA, 0xAA_0000, 0xAA_00AA, 0xFF_AA00, 0xAA_AAAA,
    0x55_5555, 0x55_55FF, 0x55_FF55, 0x55_FFFF, 0xFF_5555, 0xFF_55FF, 0xFF_FF55, 0xFF_FFFF,
];

/// `Waypoint.Icon` after `cloneAndAssignStyle`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Icon {
    style: Option<String>,
    color: Option<i32>,
}

/// What the locator bar needs of a player this tick.
struct Snapshot {
    uuid: u128,
    sink: Arc<dyn PacketSink>,
    pos: Vec3,
    block: BlockPos,
    chunk: ChunkPos,
    view: Option<ChunkView>,
    /// `isTransmittingWaypoint`: `waypoint_transmit_range` > 0.
    transmit_range: f64,
    receive_range: f64,
    spectator: bool,
    /// The vehicles it rides, innermost first (`hasIndirectPassenger`).
    vehicles: Vec<EntityId>,
    icon: Icon,
}

impl Snapshot {
    fn transmitting(&self) -> bool {
        self.transmit_range > 0.0
    }

    /// Whether anything a connection depends on changed since `before`.
    fn changed(&self, before: &Snapshot) -> bool {
        self.pos != before.pos
            || self.view != before.view
            || self.transmit_range.to_bits() != before.transmit_range.to_bits()
            || self.spectator != before.spectator
            || self.vehicles != before.vehicles
            || self.icon != before.icon
    }

    fn distance(&self, other: &Snapshot) -> f64 {
        let (dx, dy, dz) = (
            self.pos.x - other.pos.x,
            self.pos.y - other.pos.y,
            self.pos.z - other.pos.z,
        );
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    fn sees_chunk(&self, chunk: ChunkPos) -> bool {
        self.view.is_some_and(|v| v.contains(chunk, false))
    }
}

/// One `WaypointTransmitter.Connection`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Connection {
    Azimuth(f32),
    Block(BlockPos),
    Chunk(ChunkPos),
}

/// The locator bar state of one world. Players have slots; per pair of receiver and source
/// only the kind of connection is kept (2 bits): the icon and the block or chunk a connection
/// last sent are the source's of the last tick, since every connection of a source that
/// changed is updated in the same tick. Only azimuths (players more than 332 blocks apart)
/// keep their own angle.
#[derive(Default)]
pub(crate) struct Waypoints {
    /// Slot by player.
    slot_of: FxHashMap<EntityId, u32>,
    /// The players by slot as of the last tick.
    players: Vec<Option<(EntityId, Snapshot)>>,
    free: Vec<u32>,
    /// Per receiver slot: the connections to each source slot, 2 bits each ([`Kind`]).
    kinds: Vec<Vec<u64>>,
    /// Per receiver slot: the last angle of its azimuth connections by source slot.
    angles: Vec<FxHashMap<u32, f32>>,
}

/// The kind of a connection in [`Waypoints::kinds`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Kind {
    None = 0,
    Azimuth = 1,
    Block = 2,
    Chunk = 3,
}

fn kind_at(row: &[u64], slot: u32) -> Kind {
    let bits = row
        .get(slot as usize / 32)
        .map_or(0, |w| (w >> (slot % 32 * 2)) & 3);
    match bits {
        1 => Kind::Azimuth,
        2 => Kind::Block,
        3 => Kind::Chunk,
        _ => Kind::None,
    }
}

fn set_kind(row: &mut Vec<u64>, slot: u32, kind: Kind) {
    let word = slot as usize / 32;
    if row.len() <= word {
        if kind == Kind::None {
            return;
        }
        row.resize(word + 1, 0);
    }
    let shift = slot % 32 * 2;
    row[word] = (row[word] & !(3 << shift)) | (u64::from(kind as u8) << shift);
}

/// The packets of one source that are the same for every receiver, encoded on first use.
#[derive(Default)]
struct SourceFrames {
    untrack: OnceLock<Vec<u8>>,
    track_block: OnceLock<Vec<u8>>,
    track_chunk: OnceLock<Vec<u8>>,
    update_block: OnceLock<Vec<u8>>,
    update_chunk: OnceLock<Vec<u8>>,
}

/// The frame in `cell`, made by `make` on first use.
fn cached(cell: &OnceLock<Vec<u8>>, make: impl FnOnce() -> Vec<u8>) -> &[u8] {
    cell.get_or_init(make)
}

/// How this tick's packets are framed and tapped.
struct Framing<'a> {
    threshold: Option<usize>,
    tap: &'a PacketTap,
    tapped: bool,
}

impl Framing<'_> {
    /// The frame of `packet` after the broadcast stage of the packet listeners (empty when a
    /// listener drops it).
    fn frame(&self, packet: &Waypoint, codec: &mut Codec) -> Vec<u8> {
        let shared = SharedPacket::of(packet);
        let shared = if self.tapped {
            match self.tap.shared(State::Play, &shared) {
                Verdict::Pass => shared,
                Verdict::Drop => return Vec::new(),
                Verdict::Replace(other) => other,
            }
        } else {
            shared
        };
        let mut out = Vec::new();
        if encode_frame(shared.body(), self.threshold, codec, &mut out).is_err() {
            out.clear();
        }
        out
    }
}

/// `WaypointTransmitter.doesSourceIgnoreReceiver`.
fn ignores(source_id: EntityId, source: &Snapshot, receiver: &Snapshot) -> bool {
    if receiver.spectator {
        return false;
    }
    if source.spectator || receiver.vehicles.contains(&source_id) {
        return true;
    }
    source.distance(receiver) >= source.transmit_range.min(receiver.receive_range)
}

/// The azimuth of `source` seen from `receiver` (`EntityAzimuthConnection`).
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn azimuth(source: &Snapshot, receiver: &Snapshot) -> f32 {
    // `receiver.position().subtract(source.position()).rotateClockwise90()`.
    let (x, z) = (
        -(receiver.pos.z - source.pos.z),
        receiver.pos.x - source.pos.x,
    );
    z.atan2(x) as f32
}

/// `LivingEntity.makeWaypointConnectionWith`.
fn connect(source_id: EntityId, source: &Snapshot, receiver: &Snapshot) -> Option<Connection> {
    if ignores(source_id, source, receiver) {
        None
    } else if source.distance(receiver) > REALLY_FAR {
        Some(Connection::Azimuth(azimuth(source, receiver)))
    } else if !receiver.sees_chunk(source.chunk) {
        Some(Connection::Chunk(source.chunk))
    } else {
        Some(Connection::Block(source.block))
    }
}

/// `Connection.isBroken`.
fn broken(c: Connection, source_id: EntityId, source: &Snapshot, receiver: &Snapshot) -> bool {
    match c {
        Connection::Azimuth(_) => {
            ignores(source_id, source, receiver)
                || receiver.sees_chunk(source.chunk)
                || source.distance(receiver) <= REALLY_FAR
        }
        Connection::Block(last) => {
            manhattan(last, source.block) > 1 || ignores(source_id, source, receiver)
        }
        Connection::Chunk(last) => {
            chessboard(last, source.chunk) > 1
                || ignores(source_id, source, receiver)
                || receiver.sees_chunk(last)
        }
    }
}

fn manhattan(a: BlockPos, b: BlockPos) -> i64 {
    let d = |a: i32, b: i32| (i64::from(a) - i64::from(b)).abs();
    d(a.x, b.x) + d(a.y, b.y) + d(a.z, b.z)
}

fn chessboard(a: ChunkPos, b: ChunkPos) -> i64 {
    let d = |a: i32, b: i32| (i64::from(a) - i64::from(b)).abs();
    d(a.x, b.x).max(d(a.z, b.z))
}

fn packet(
    op: WaypointOperation,
    uuid: u128,
    icon: Option<&Icon>,
    c: Option<Connection>,
) -> Waypoint {
    let style = icon
        .and_then(|i| i.style.as_deref())
        .and_then(|s| Identifier::new(s).ok());
    Waypoint {
        operation: op,
        id: Ok(Uuid(uuid)),
        style: style.unwrap_or_else(|| Identifier::vanilla("default")),
        color: icon.and_then(|i| i.color),
        target: match c {
            None => WaypointTarget::Empty,
            Some(Connection::Block(p)) => WaypointTarget::Position {
                x: p.x,
                y: p.y,
                z: p.z,
            },
            Some(Connection::Chunk(p)) => WaypointTarget::Chunk { x: p.x, z: p.z },
            Some(Connection::Azimuth(a)) => WaypointTarget::Azimuth(a),
        },
    }
}

fn untrack(sink: &dyn PacketSink, uuid: u128) {
    sink.send(&packet(WaypointOperation::Untrack, uuid, None, None));
}

/// `waypoint_transmit_range` of a player: the default with the modifiers of its equipment
/// (heads, carved pumpkins), sneaking and its effects (invisibility).
fn transmit_range(player: &vanillite_game::player::PlayerData, crouching: bool) -> f64 {
    let attribute = Attribute::WAYPOINT_TRANSMIT_RANGE;
    let mut modifiers = Vec::new();
    for slot in [
        EquipmentSlot::MainHand,
        EquipmentSlot::OffHand,
        EquipmentSlot::Feet,
        EquipmentSlot::Legs,
        EquipmentSlot::Chest,
        EquipmentSlot::Head,
    ] {
        vanillite_game::item::attribute_modifiers(
            player.inventory.equipment(slot),
            slot,
            attribute.name(),
            &mut modifiers,
        );
    }
    if crouching {
        // `ServerPlayer.WAYPOINT_TRANSMIT_RANGE_CROUCH_MODIFIER`.
        modifiers.push((ADD_MULTIPLIED_TOTAL, -1.0));
    }
    // The base value a command or plugin set and the effects' and commands' modifiers.
    let own = &player.survival.effects.modifiers;
    let base = own
        .base(attribute)
        .unwrap_or_else(|| attribute.base_of(PLAYER));
    #[allow(clippy::as_conversions)]
    modifiers.extend(own.of(attribute).map(|m| (m.operation as u8, m.amount)));
    let (min, max) = (0.0, attribute.sanitize(f64::MAX));
    vanillite_game::item::attribute_value(base, &modifiers, min, max)
}

/// What the skip check of a pair reads of its source, by slot.
#[derive(Clone, Copy)]
struct Source {
    id: EntityId,
    pos: Vec3,
    transmit_range: f64,
    spectator: bool,
}

impl Source {
    /// [`ignores`] (with `isTransmittingWaypoint`) on the columns: the same comparisons.
    fn ignores(&self, receiver: &Snapshot) -> bool {
        if self.transmit_range <= 0.0 {
            return true;
        }
        if receiver.spectator {
            return false;
        }
        if self.spectator || receiver.vehicles.contains(&self.id) {
            return true;
        }
        let (dx, dy, dz) = (
            self.pos.x - receiver.pos.x,
            self.pos.y - receiver.pos.y,
            self.pos.z - receiver.pos.z,
        );
        (dx * dx + dy * dy + dz * dz).sqrt() >= self.transmit_range.min(receiver.receive_range)
    }
}

/// The high bit of every 2-bit entry of a [`Waypoints::kinds`] word; the slot masks of a tick
/// use these positions.
const HI: u64 = 0xAAAA_AAAA_AAAA_AAAA;
/// The low bit of every 2-bit entry.
const LO: u64 = 0x5555_5555_5555_5555;
/// The least transmit range of a source whose pairs [`Fast`] may skip.
const FAST_RANGE: f64 = 4096.0;

/// The sources whose pairs a receiver may skip by their connection kind alone, as masks over
/// slots (bit `2 * slot + 1` of word `slot / 32`). A source is *fast* when, apart from its
/// position, nothing a pair reads of it changed (same block, chunk, icon, game mode and
/// transmit range) and it is either not transmitting or not a spectator with a transmit range
/// of at least [`FAST_RANGE`]. Where every fast source is surely within range of a receiver
/// (see [`Fast::covers`]), none of their pairs starts or stops being ignored, so a block or
/// chunk connection to it sends nothing and stays: only azimuths and missing connections to
/// transmitting sources are updated. The work then follows the packets sent.
struct Fast {
    /// Fast sources that changed (moved) this tick.
    changed: Vec<u64>,
    /// Fast sources that did not change.
    unchanged: Vec<u64>,
    /// The other sources that changed.
    slow_changed: Vec<u64>,
    /// The other sources that did not change.
    slow_unchanged: Vec<u64>,
    /// Fast sources that transmit.
    transmitting: Vec<u64>,
    /// The box around the transmitting fast sources and their least transmit range.
    bounds: Option<(Vec3, Vec3, f64)>,
}

impl Fast {
    fn new(
        now: &[Option<(EntityId, Snapshot)>],
        before: &[Option<(EntityId, Snapshot)>],
        changed: &[bool],
    ) -> Fast {
        let words = now.len().div_ceil(32);
        let mut fast = Fast {
            changed: vec![0; words],
            unchanged: vec![0; words],
            slow_changed: vec![0; words],
            slow_unchanged: vec![0; words],
            transmitting: vec![0; words],
            bounds: None,
        };
        for (slot, entry) in now.iter().enumerate() {
            let Some((_, s)) = entry else {
                continue;
            };
            let (word, bit) = (slot / 32, 1u64 << (slot % 32 * 2 + 1));
            let same = before
                .get(slot)
                .and_then(Option::as_ref)
                .is_some_and(|(_, b)| {
                    b.uuid == s.uuid
                        && b.block == s.block
                        && b.chunk == s.chunk
                        && b.spectator == s.spectator
                        && b.transmit_range.to_bits() == s.transmit_range.to_bits()
                        && b.icon == s.icon
                });
            let eligible =
                same && (!s.transmitting() || (!s.spectator && s.transmit_range >= FAST_RANGE));
            let mask = match (eligible, changed[slot]) {
                (true, true) => &mut fast.changed,
                (true, false) => &mut fast.unchanged,
                (false, true) => &mut fast.slow_changed,
                (false, false) => &mut fast.slow_unchanged,
            };
            mask[word] |= bit;
            if eligible && s.transmitting() {
                fast.transmitting[word] |= bit;
                let (lo, hi, range) = fast.bounds.get_or_insert((s.pos, s.pos, s.transmit_range));
                *lo = Vec3::new(lo.x.min(s.pos.x), lo.y.min(s.pos.y), lo.z.min(s.pos.z));
                *hi = Vec3::new(hi.x.max(s.pos.x), hi.y.max(s.pos.y), hi.z.max(s.pos.z));
                *range = range.min(s.transmit_range);
            }
        }
        fast
    }

    /// Whether no fast source is ignored by `receiver`: it is no spectator, rides nothing and
    /// the farthest corner of the box is (with a margin for rounding) within both ranges.
    fn covers(&self, receiver: &Snapshot) -> bool {
        let Some((lo, hi, range)) = self.bounds else {
            return true;
        };
        if receiver.spectator || !receiver.vehicles.is_empty() {
            return false;
        }
        let p = receiver.pos;
        let (dx, dy, dz) = (
            (p.x - lo.x).abs().max((p.x - hi.x).abs()),
            (p.y - lo.y).abs().max((p.y - hi.y).abs()),
            (p.z - lo.z).abs().max((p.z - hi.z).abs()),
        );
        (dx * dx + dy * dy + dz * dz).sqrt() + 1.0 < range.min(receiver.receive_range)
    }
}

/// Receivers per part of the parallel pair updates.
const RECEIVERS_PER_PART: usize = 64;

/// What the pair updates of one tick read.
struct PairCtx<'a> {
    /// The players by slot now.
    now: &'a [Option<(EntityId, Snapshot)>],
    /// The players by slot as of the last tick.
    before: &'a [Option<(EntityId, Snapshot)>],
    changed: &'a [bool],
    /// The slots of the players that changed, ascending.
    changed_slots: &'a [u32],
    sources: &'a [Option<Source>],
    fast: &'a Fast,
    frames: &'a [SourceFrames],
    framing: Framing<'a>,
}

impl Waypoints {
    /// The tick's update: the players that left get their connections removed, then the
    /// connections of the players that moved or changed are made, updated or broken. Each
    /// receiver gets its packets as one batch, built in parallel over the receivers.
    pub(crate) fn tick(
        &mut self,
        entities: &Entities,
        global: &Global,
        compute: &Compute,
        tap: &PacketTap,
    ) {
        let enabled = global
            .level
            .rules()
            .bool(vanillite_game::survival::rules::Rule::LOCATOR_BAR);
        if !enabled {
            // `breakAllConnections`.
            for (receiver, row) in self.players.iter().zip(&self.kinds) {
                let Some((_, receiver)) = receiver else {
                    continue;
                };
                for (slot, entry) in (0u32..).zip(&self.players) {
                    if let Some((_, source)) = entry
                        && kind_at(row, slot) != Kind::None
                    {
                        untrack(receiver.sink.as_ref(), source.uuid);
                    }
                }
            }
            *self = Waypoints::default();
            return;
        }
        let mut now_list = snapshots(entities, global);
        // `removePlayer`: the leaving player's own connections and those to it break.
        let present: FxHashSet<EntityId> = now_list.iter().map(|(id, _)| *id).collect();
        let gone: Vec<u32> = self
            .players
            .iter()
            .enumerate()
            .filter_map(|(slot, p)| {
                p.as_ref()
                    .filter(|(id, _)| !present.contains(id))
                    .map(|_| u32::try_from(slot).unwrap_or(u32::MAX))
            })
            .collect();
        for &g in &gone {
            self.forget(g);
        }

        // Slots for the players of this tick; who changed.
        let mut now: Vec<Option<(EntityId, Snapshot)>> = Vec::new();
        now.resize_with(self.players.len(), || None);
        for (id, snap) in now_list.drain(..) {
            let slot = match self.slot_of.get(&id) {
                Some(&slot) => slot,
                None => {
                    let slot = self
                        .free
                        .pop()
                        .unwrap_or_else(|| u32::try_from(self.players.len()).unwrap_or(u32::MAX));
                    self.slot_of.insert(id, slot);
                    slot
                }
            };
            let i = slot as usize;
            if self.players.len() <= i {
                self.players.resize_with(i + 1, || None);
                self.kinds.resize_with(i + 1, Vec::new);
                self.angles.resize_with(i + 1, FxHashMap::default);
            }
            if now.len() <= i {
                now.resize_with(i + 1, || None);
            }
            now[i] = Some((id, snap));
        }
        let changed: Vec<bool> = now
            .iter()
            .zip(&self.players)
            .map(|(now, before)| match (now, before) {
                (Some((_, now)), Some((_, before))) => now.changed(before),
                (Some(_), None) => true,
                _ => false,
            })
            .collect();
        let changed_slots: Vec<u32> = (0u32..)
            .zip(&changed)
            .filter(|(_, c)| **c)
            .map(|(slot, _)| slot)
            .collect();
        if changed_slots.is_empty() {
            self.players = now;
            return;
        }

        let threshold = entities
            .players
            .iter()
            .next()
            .and_then(|(_, player)| player.connection.sink.frame_threshold());
        let tapped = tap.active(Direction::Clientbound);
        let mut mark = tap.batch_mark();
        if tapped
            && tap.listens(Stage::Receiver)
            && tap.wants_at(Stage::Receiver, State::Play, Waypoint::ID)
        {
            mark = BatchMark::UNKNOWN;
        }
        let mut frames = Vec::new();
        frames.resize_with(now.len(), SourceFrames::default);
        let sources: Vec<Option<Source>> = now
            .iter()
            .map(|entry| {
                entry.as_ref().map(|(id, s)| Source {
                    id: *id,
                    pos: s.pos,
                    transmit_range: s.transmit_range,
                    spectator: s.spectator,
                })
            })
            .collect();
        let fast = Fast::new(&now, &self.players, &changed);
        let ctx = PairCtx {
            now: &now,
            before: &self.players,
            changed: &changed,
            changed_slots: &changed_slots,
            sources: &sources,
            fast: &fast,
            frames: &frames,
            framing: Framing {
                threshold,
                tap,
                tapped,
            },
        };
        let mut rows: Vec<(u32, &mut Vec<u64>, &mut FxHashMap<u32, f32>)> = (0u32..)
            .zip(self.kinds.iter_mut().zip(self.angles.iter_mut()))
            .filter(|(slot, _)| now.get(*slot as usize).is_some_and(Option::is_some))
            .map(|(slot, (kinds, angles))| (slot, kinds, angles))
            .collect();
        compute.parts(&mut rows, RECEIVERS_PER_PART, |_, part| {
            let mut codec = Codec::default();
            let mut batch = Vec::new();
            // Receivers in a crowd get batches of similar size: each starts with the capacity
            // of the last one instead of growing from empty.
            for (receiver, kinds, angles) in part.iter_mut() {
                ctx.receiver(*receiver, kinds, angles, &mut codec, &mut batch);
                if !batch.is_empty()
                    && let Some((_, r)) = &ctx.now[*receiver as usize]
                {
                    let size = batch.len();
                    r.sink.send_batch(
                        std::mem::replace(&mut batch, Vec::with_capacity(size)),
                        threshold,
                        mark,
                    );
                }
            }
        });
        drop(rows);
        self.players = now;
    }

    /// Drops the player in `slot`: untracks its connections both ways and frees the slot.
    fn forget(&mut self, slot: u32) {
        let Some((id, gone)) = self.players.get_mut(slot as usize).and_then(Option::take) else {
            return;
        };
        self.slot_of.remove(&id);
        for (source, entry) in (0u32..).zip(&self.players) {
            if let Some((_, other)) = entry {
                if kind_at(&self.kinds[slot as usize], source) != Kind::None {
                    untrack(gone.sink.as_ref(), other.uuid);
                }
                if let Some(row) = self.kinds.get_mut(source as usize) {
                    if kind_at(row, slot) != Kind::None {
                        untrack(other.sink.as_ref(), gone.uuid);
                    }
                    set_kind(row, slot, Kind::None);
                }
                if let Some(angles) = self.angles.get_mut(source as usize) {
                    angles.remove(&slot);
                }
            }
        }
        self.kinds[slot as usize].clear();
        self.angles[slot as usize].clear();
        self.free.push(slot);
    }
}

impl PairCtx<'_> {
    /// `updateConnection` / `createConnection` of one receiver with every source it has to
    /// look at: all if it changed, else the sources that changed. Its packets go into `batch`.
    fn receiver(
        &self,
        r: u32,
        kinds: &mut Vec<u64>,
        angles: &mut FxHashMap<u32, f32>,
        codec: &mut Codec,
        batch: &mut Vec<u8>,
    ) {
        let Some((_, receiver)) = &self.now[r as usize] else {
            return;
        };
        let Some((_, before)) = self.before.get(r as usize).and_then(Option::as_ref) else {
            for s in 0..u32::try_from(self.now.len()).unwrap_or(u32::MAX) {
                if s != r {
                    self.pair(receiver, s, kinds, angles, codec, batch);
                }
            }
            return;
        };
        let moved_only = before.view == receiver.view
            && before.spectator == receiver.spectator
            && before.vehicles == receiver.vehicles
            && before.receive_range.to_bits() == receiver.receive_range.to_bits();
        let unchanged = !self.changed[r as usize];
        if (unchanged || moved_only) && self.fast.covers(receiver) {
            self.receiver_fast(r, receiver, unchanged, kinds, angles, codec, batch);
            return;
        }
        if unchanged {
            for &s in self.changed_slots {
                if s != r {
                    self.pair(receiver, s, kinds, angles, codec, batch);
                }
            }
            return;
        }
        // A receiver that only moved changes its pairs with unchanged sources only where the
        // source starts or stops being ignored (a distance threshold) and in azimuths: the
        // other pairs are skipped after a check on the compact source columns.
        for (s, source) in (0u32..).zip(self.sources) {
            let Some(source) = source.filter(|_| s != r) else {
                continue;
            };
            if moved_only && !self.changed[s as usize] {
                let kind = kind_at(kinds, s);
                if kind != Kind::Azimuth && (kind == Kind::None) == source.ignores(receiver) {
                    continue;
                }
            }
            self.pair(receiver, s, kinds, angles, codec, batch);
        }
    }

    /// [`Self::receiver`] for a receiver that is unchanged or only moved and that
    /// [`Fast::covers`]: block and chunk connections to fast sources are skipped by their kind.
    #[allow(clippy::too_many_arguments)]
    fn receiver_fast(
        &self,
        r: u32,
        receiver: &Snapshot,
        unchanged: bool,
        kinds: &mut Vec<u64>,
        angles: &mut FxHashMap<u32, f32>,
        codec: &mut Codec,
        batch: &mut Vec<u8>,
    ) {
        let fast = self.fast;
        for (w, word) in (0u32..).zip(0..fast.changed.len()) {
            let k = kinds.get(word).copied().unwrap_or(0);
            let lo_up = (k & LO) << 1;
            let azimuth = lo_up & !k & HI;
            let none = !(k | lo_up) & HI;
            let (fast_sources, checked) = if unchanged {
                (fast.changed[word], 0)
            } else {
                (
                    fast.changed[word] | fast.unchanged[word],
                    fast.slow_unchanged[word],
                )
            };
            let mut todo = fast.slow_changed[word]
                | checked
                | (fast_sources & (azimuth | (none & fast.transmitting[word])));
            while todo != 0 {
                let bit = todo.trailing_zeros();
                todo &= todo - 1;
                let s = w * 32 + bit / 2;
                if s == r {
                    continue;
                }
                if checked & (1 << bit) != 0
                    && let Some(source) = self.sources[s as usize]
                {
                    let kind = kind_at(kinds, s);
                    if kind != Kind::Azimuth && (kind == Kind::None) == source.ignores(receiver) {
                        continue;
                    }
                }
                self.pair(receiver, s, kinds, angles, codec, batch);
            }
        }
    }

    /// `ServerWaypointManager.updateConnection` / `createConnection` for one pair.
    #[allow(clippy::too_many_arguments)]
    fn pair(
        &self,
        receiver: &Snapshot,
        s: u32,
        kinds: &mut Vec<u64>,
        angles: &mut FxHashMap<u32, f32>,
        codec: &mut Codec,
        batch: &mut Vec<u8>,
    ) {
        let Some((source_id, source)) = &self.now[s as usize] else {
            return;
        };
        let frames = &self.frames[s as usize];
        let kind = kind_at(kinds, s);
        if !source.transmitting() {
            // `untrackWaypoint`.
            if kind != Kind::None {
                set_kind(kinds, s, Kind::None);
                angles.remove(&s);
                batch.extend_from_slice(cached(&frames.untrack, || {
                    self.framing.frame(
                        &packet(WaypointOperation::Untrack, source.uuid, None, None),
                        codec,
                    )
                }));
            }
            return;
        }
        // An existing connection was made or updated last tick with the source as it was.
        let before = self.before.get(s as usize).and_then(Option::as_ref);
        if kind != Kind::None
            && let Some((_, last)) = before
        {
            let existing = match kind {
                Kind::Azimuth => Connection::Azimuth(angles.get(&s).copied().unwrap_or(0.0)),
                Kind::Block => Connection::Block(last.block),
                _ => Connection::Chunk(last.chunk),
            };
            let remake = last.icon != source.icon || broken(existing, *source_id, source, receiver);
            if !remake {
                match existing {
                    Connection::Azimuth(angle) => {
                        let now = azimuth(source, receiver);
                        if (now - angle).abs() > 0.008_726_646 {
                            angles.insert(s, now);
                            let p = packet(
                                WaypointOperation::Update,
                                source.uuid,
                                Some(&source.icon),
                                Some(Connection::Azimuth(now)),
                            );
                            batch.extend_from_slice(&self.framing.frame(&p, codec));
                        }
                    }
                    Connection::Block(last) => {
                        if manhattan(last, source.block) > 0 {
                            batch.extend_from_slice(cached(&frames.update_block, || {
                                self.framing.frame(
                                    &packet(
                                        WaypointOperation::Update,
                                        source.uuid,
                                        Some(&source.icon),
                                        Some(Connection::Block(source.block)),
                                    ),
                                    codec,
                                )
                            }));
                        }
                    }
                    Connection::Chunk(last) => {
                        if chessboard(last, source.chunk) > 0 {
                            batch.extend_from_slice(cached(&frames.update_chunk, || {
                                self.framing.frame(
                                    &packet(
                                        WaypointOperation::Update,
                                        source.uuid,
                                        Some(&source.icon),
                                        Some(Connection::Chunk(source.chunk)),
                                    ),
                                    codec,
                                )
                            }));
                        }
                    }
                }
                return;
            }
        }
        match connect(*source_id, source, receiver) {
            Some(c) => {
                let track = |c| {
                    packet(
                        WaypointOperation::Track,
                        source.uuid,
                        Some(&source.icon),
                        Some(c),
                    )
                };
                match c {
                    Connection::Azimuth(angle) => {
                        set_kind(kinds, s, Kind::Azimuth);
                        angles.insert(s, angle);
                        batch.extend_from_slice(&self.framing.frame(&track(c), codec));
                    }
                    Connection::Block(_) => {
                        set_kind(kinds, s, Kind::Block);
                        angles.remove(&s);
                        batch.extend_from_slice(cached(&frames.track_block, || {
                            self.framing.frame(&track(c), codec)
                        }));
                    }
                    Connection::Chunk(_) => {
                        set_kind(kinds, s, Kind::Chunk);
                        angles.remove(&s);
                        batch.extend_from_slice(cached(&frames.track_chunk, || {
                            self.framing.frame(&track(c), codec)
                        }));
                    }
                }
            }
            None => {
                if kind != Kind::None {
                    set_kind(kinds, s, Kind::None);
                    angles.remove(&s);
                    batch.extend_from_slice(cached(&frames.untrack, || {
                        self.framing.frame(
                            &packet(WaypointOperation::Untrack, source.uuid, None, None),
                            codec,
                        )
                    }));
                }
            }
        }
    }
}

/// The players of the world as the locator bar sees them.
fn snapshots(entities: &Entities, global: &Global) -> Vec<(EntityId, Snapshot)> {
    let icons = global.systems.waypoints().clone();
    let board = global.systems.scoreboard();
    let receive = Attribute::WAYPOINT_RECEIVE_RANGE.base_of(PLAYER);
    let mut out = Vec::with_capacity(entities.players.len());
    for (id, player) in entities.players.iter() {
        let Some(entity) = entities.store.get(id) else {
            continue;
        };
        let pos = entity.pos();
        let block = vanillite_game::player::play::block_pos_of(pos);
        let mut vehicles = Vec::new();
        let mut at = id;
        while let Some(v) = entities.vehicle_of(at) {
            if vehicles.contains(&v) {
                break;
            }
            vehicles.push(v);
            at = v;
        }
        let uuid = entity.uuid();
        let own = icons.get(&uuid);
        // `cloneAndAssignStyle`: the icon's colour, else the team's.
        let color = own.and_then(|i| i.color).or_else(|| {
            let color = board.team_of(&player.profile.name)?.color?;
            Some(if color == 0 {
                BLACK
            } else {
                TEAM_RGB.get(usize::from(color)).copied()?
            })
        });
        out.push((
            id,
            Snapshot {
                uuid,
                sink: Arc::clone(&player.connection.sink),
                pos,
                block,
                chunk: ChunkPos::new(block.x >> 4, block.z >> 4),
                view: player.connection.chunks.view(),
                transmit_range: transmit_range(player, entity.pose() == Pose::Crouching),
                receive_range: receive,
                spectator: player.game_mode == GameMode::Spectator,
                vehicles,
                icon: Icon {
                    style: own.and_then(|i| i.style.clone()),
                    color,
                },
            },
        ));
    }
    out
}
