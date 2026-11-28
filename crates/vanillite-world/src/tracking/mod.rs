//! Entity tracking (architecture 11): which player sees which entity, and what changed.
//!
//! The `EntityTracker` slot runs once per world tick after the entities ticked. It reads the
//! core columns and the viewers, and writes typed updates ([`TrackerOutput`]): each update
//! is one [`Message`] with the list of viewers that must receive it. The tracker knows no
//! packets. The game encodes each message exactly once and hands the same encoded packet to
//! all its viewers ([`TrackerOutput::fan_out`]).
//!
//! The built-in implementation is [`ChunkTracker`].

mod chunk;

use std::fmt;

use vanillite_base::{ChunkPos, Vec3};
use vanillite_config::{CrowdRules, TrackingRules, VanillaOr};
use vanillite_data::entity::{ENTITY_TYPES, EntityTypeId, ids};

use crate::entity::{EntityId, EntityStore, NetworkId};

pub use chunk::{ChunkTracker, ChunkTrackerProvider};

/// The `entity-tracker` slot: one instance per world, owned by the world's thread.
pub trait EntityTracker: Send {
    /// Computes this tick's visibility changes and movement updates. `out` is empty on entry.
    fn update(&mut self, input: &TrackerInput<'_>, out: &mut TrackerOutput);

    /// The players that saw `entity` at the last update (Vanilla's `ServerEntity.seenBy`), for
    /// packets about it (`sendToTrackingPlayers`): swings, item pickups, equipment.
    fn seen_by(&self, entity: EntityId) -> &[EntityId] {
        let _ = entity;
        &[]
    }

    /// Gives back what the tracker keeps for its capacity (the world has been empty for a
    /// while); the next update must work as before.
    fn release_memory(&mut self) {}
}

/// Creates the tracker of each world.
pub trait EntityTrackerProvider: Send + Sync {
    /// A tracker for one world.
    fn create(&self, settings: &TrackerSettings) -> Box<dyn EntityTracker>;
}

/// Tracking ranges of one world, resolved from the entity types and the rules.
#[derive(Clone, Debug)]
pub struct TrackerSettings {
    view_distance: i32,
    /// Effective range in blocks per entity type, already capped by the view distance.
    ranges: Box<[i32]>,
    max_range: i32,
    crowd: CrowdSettings,
}

/// Crowd mode (`performance.crowd`) of one world's tracker.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CrowdSettings {
    /// Blocks within which players get every movement update; `None` = no tiers.
    pub near_radius: Option<u16>,
    /// Ticks between two movement updates of a far player.
    pub far_interval: u8,
    /// Players each viewer sees at most; `None` = all within range.
    pub max_visible_players: Option<u32>,
}

impl CrowdSettings {
    /// The settings of the rules.
    pub fn new(rules: &CrowdRules) -> CrowdSettings {
        CrowdSettings {
            near_radius: rules.near_radius.get(),
            far_interval: rules.far_interval,
            max_visible_players: rules.max_visible_players.get(),
        }
    }

    /// Whether any crowd rule is on.
    pub fn enabled(&self) -> bool {
        self.near_radius.is_some() || self.max_visible_players.is_some()
    }
}

impl TrackerSettings {
    /// Settings for a world with the server's view distance (chunks), Vanilla's
    /// `entity-broadcast-range-percentage` and the world's tracking rules.
    pub fn new(
        view_distance: i32,
        range_percentage: i32,
        rules: &TrackingRules,
    ) -> TrackerSettings {
        let view_distance = view_distance.clamp(2, 32);
        let cap = view_distance * 16;
        let ranges: Box<[i32]> = ENTITY_TYPES
            .iter()
            .zip(0u16..)
            .map(|(info, id)| {
                let rule = if EntityTypeId(id) == ids::PLAYER {
                    rules.player_range
                } else {
                    match info.category {
                        "monster" => rules.monster_range,
                        "misc" => rules.misc_range,
                        _ => rules.animal_range,
                    }
                };
                // `getScaledTrackingDistance(clientTrackingRange * 16)`.
                let vanilla = || {
                    let blocks = i32::try_from(info.tracking_range).unwrap_or(i32::MAX / 16) * 16;
                    range_percentage.wrapping_mul(blocks) / 100
                };
                // `ChunkMap.addEntity`: types without a client tracking range (markers) are
                // never tracked.
                if info.tracking_range == 0 {
                    return 0;
                }
                match rule {
                    VanillaOr::Value(blocks) => i32::from(blocks),
                    VanillaOr::Vanilla => vanilla(),
                }
                .min(cap)
            })
            .collect();
        let max_range = ranges.iter().copied().max().unwrap_or(0).max(0);
        TrackerSettings {
            view_distance,
            ranges,
            max_range,
            crowd: CrowdSettings::default(),
        }
    }

    /// These settings with crowd mode.
    pub fn with_crowd(mut self, crowd: CrowdSettings) -> TrackerSettings {
        self.crowd = crowd;
        self
    }

    /// Crowd mode.
    pub fn crowd(&self) -> &CrowdSettings {
        &self.crowd
    }

    /// The server view distance in chunks.
    pub fn view_distance(&self) -> i32 {
        self.view_distance
    }

    /// The effective tracking range of `kind` in blocks.
    #[inline]
    pub fn range(&self, kind: EntityTypeId) -> i32 {
        self.ranges.get(usize::from(kind.0)).copied().unwrap_or(0)
    }

    /// The largest range of any entity type in blocks.
    pub fn max_range(&self) -> i32 {
        self.max_range
    }
}

/// A player that can see entities: an entity of this world with a chunk view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewer {
    /// The player's entity.
    pub entity: EntityId,
    /// Centre of the player's chunk view (`ChunkTrackingView`).
    pub view_center: ChunkPos,
    /// The player's effective view distance in chunks.
    pub view_distance: i32,
    /// Chebyshev distance from `view_center` below which every chunk was sent: the
    /// tracker asks `chunk_pending` only from there (0 = ask always).
    pub sent_radius: i32,
}

impl Viewer {
    /// `ChunkTrackingView.contains`: whether the chunk is inside the player's view.
    #[inline]
    pub fn sees_chunk(&self, chunk: ChunkPos) -> bool {
        let dx = i64::from(((chunk.x - self.view_center.x).abs() - 1).max(0));
        let dz = i64::from(((chunk.z - self.view_center.z).abs() - 1).max(0));
        let distance = i64::from(self.view_distance);
        dx * dx + dz * dz < distance * distance
    }
}

/// Runs `job(0)`, …, `job(n - 1)`, on several threads if it likes, and returns when all are
/// done. The server passes its compute pool; tests run inline.
pub type Fork<'a> = dyn Fn(usize, &(dyn Fn(usize) + Sync)) + Sync + 'a;

/// A [`Fork`] that runs the jobs one after another on the calling thread.
pub fn inline_fork(n: usize, job: &(dyn Fn(usize) + Sync)) {
    for i in 0..n {
        job(i);
    }
}

/// What a tracker update reads.
pub struct TrackerInput<'a> {
    /// The world's entities.
    pub entities: &'a EntityStore,
    /// The players of this world that see entities. Each must be an entity of `entities`.
    pub viewers: &'a [Viewer],
    /// Whether a chunk inside a viewer's view has not been sent to that viewer yet
    /// (Vanilla's `chunkSender.isPending`); entities there stay invisible.
    pub chunk_pending: &'a (dyn Fn(EntityId, ChunkPos) -> bool + Sync),
    /// Viewers whose client moves the server took since the last update (`ChunkMap.move`):
    /// every entity is checked against them again (`updatePlayer`), so entities in chunks
    /// sent since become visible. Only then: Vanilla pairs nothing when a chunk is sent.
    pub moved_viewers: &'a [EntityId],
    /// Factor on every entity type's movement update interval (1 = Vanilla). Larger under
    /// overload (`performance.entity-tracking.overload-tick-millis`).
    pub update_scale: i32,
    /// Threads the read-only visibility checks may use, the calling one included
    /// (`performance.threads.tick-phases`; 1 = inline). The result does not depend on it.
    pub threads: usize,
    /// Where those checks run.
    pub fork: &'a Fork<'a>,
}

impl fmt::Debug for TrackerInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrackerInput")
            .field("entities", &self.entities.len())
            .field("viewers", &self.viewers.len())
            .finish_non_exhaustive()
    }
}

/// One change of an entity, as Vanilla's `ServerEntity` would send it. Angles are protocol
/// bytes (`Mth.packDegrees`), position deltas are in 1/4096 blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EntityUpdate {
    /// The viewers start seeing the entity: `add_entity` with these values plus the entity's
    /// pairing data (non-default synched data, equipment, attributes, passengers).
    Spawn {
        /// Entity type.
        kind: EntityTypeId,
        /// UUID.
        uuid: u128,
        /// Last sent position.
        pos: Vec3,
        /// Last sent velocity.
        velocity: Vec3,
        /// Last sent yaw.
        y_rot: i8,
        /// Last sent pitch.
        x_rot: i8,
        /// Last sent head yaw.
        y_head_rot: i8,
    },
    /// The viewers stop seeing the entity (`remove_entities`).
    Remove,
    /// Absolute position (`entity_position_sync`).
    Sync {
        /// Position.
        pos: Vec3,
        /// Velocity.
        velocity: Vec3,
        /// Yaw in degrees.
        y_rot: f32,
        /// Pitch in degrees.
        x_rot: f32,
        /// On ground.
        on_ground: bool,
    },
    /// Relative move (`move_entity_pos`).
    Pos {
        /// X delta.
        dx: i16,
        /// Y delta.
        dy: i16,
        /// Z delta.
        dz: i16,
        /// On ground.
        on_ground: bool,
    },
    /// Relative move and rotation (`move_entity_pos_rot`).
    PosRot {
        /// X delta.
        dx: i16,
        /// Y delta.
        dy: i16,
        /// Z delta.
        dz: i16,
        /// Yaw.
        y_rot: i8,
        /// Pitch.
        x_rot: i8,
        /// On ground.
        on_ground: bool,
    },
    /// Rotation (`move_entity_rot`).
    Rot {
        /// Yaw.
        y_rot: i8,
        /// Pitch.
        x_rot: i8,
        /// On ground.
        on_ground: bool,
    },
    /// Head yaw (`rotate_head`).
    HeadRot(i8),
    /// Velocity (`set_entity_motion`) of `hurtMarked`.
    Motion(Vec3),
    /// Velocity of the movement tracking (`set_entity_motion`; bundled with
    /// `projectile_power` for `AbstractHurtingProjectile`s).
    TrackedMotion(Vec3),
    /// The dirty synched data (`set_entity_data`; attributes too for living entities). The
    /// encoder packs it from the entity's current state.
    Data,
}

/// One update of one entity and who receives it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Message {
    /// The entity (stale for [`EntityUpdate::Remove`] after a despawn).
    pub entity: EntityId,
    /// Its network id.
    pub network_id: NetworkId,
    /// What changed.
    pub update: EntityUpdate,
    viewers_start: u32,
    viewers_len: u32,
}

/// A range of viewers in a [`TrackerOutput`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewerSpan {
    start: u32,
    len: u32,
}

impl ViewerSpan {
    /// Whether the span holds no viewer.
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// The result of one tracker update: messages in send order, with shared viewer lists.
#[derive(Debug, Default)]
pub struct TrackerOutput {
    messages: Vec<Message>,
    viewers: Vec<EntityId>,
}

impl TrackerOutput {
    /// Empties the output and gives back its capacity.
    pub fn release_memory(&mut self) {
        self.messages = Vec::new();
        self.viewers = Vec::new();
    }

    /// Empties the output, keeping its capacity.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.viewers.clear();
    }

    /// Stores a viewer list; several messages can share it.
    pub fn push_viewers(&mut self, viewers: impl IntoIterator<Item = EntityId>) -> ViewerSpan {
        let start = self.viewers.len();
        self.viewers.extend(viewers);
        ViewerSpan {
            start: u32::try_from(start).expect("tracker output full"),
            len: u32::try_from(self.viewers.len() - start).expect("tracker output full"),
        }
    }

    /// [`Self::push_viewers`] for a list and, if given, one more viewer: one copy of the
    /// list's memory instead of one write per viewer.
    pub fn push_viewer_list(&mut self, viewers: &[EntityId], and: Option<EntityId>) -> ViewerSpan {
        let start = self.viewers.len();
        self.viewers.extend_from_slice(viewers);
        self.viewers.extend(and);
        ViewerSpan {
            start: u32::try_from(start).expect("tracker output full"),
            len: u32::try_from(self.viewers.len() - start).expect("tracker output full"),
        }
    }

    /// Adds a message for the viewers of `span`. Messages without viewers are dropped.
    pub fn push(
        &mut self,
        entity: EntityId,
        network_id: NetworkId,
        update: EntityUpdate,
        span: ViewerSpan,
    ) {
        if span.is_empty() {
            return;
        }
        self.messages.push(Message {
            entity,
            network_id,
            update,
            viewers_start: span.start,
            viewers_len: span.len,
        });
    }

    /// The messages in send order.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The viewers of a message.
    pub fn viewers_of(&self, message: &Message) -> &[EntityId] {
        let start = message.viewers_start as usize;
        self.viewers
            .get(start..start + message.viewers_len as usize)
            .unwrap_or(&[])
    }

    /// Encode once, fan out: encodes every message exactly once with `encode` and passes the
    /// result to `send` for each of its viewers. Each viewer gets its packets in message order.
    /// `encode` returns `None` to skip a message.
    pub fn fan_out<P>(
        &self,
        mut encode: impl FnMut(&Message) -> Option<P>,
        mut send: impl FnMut(EntityId, &P),
    ) {
        for message in &self.messages {
            if let Some(packet) = encode(message) {
                for &viewer in self.viewers_of(message) {
                    send(viewer, &packet);
                }
            }
        }
    }
}
