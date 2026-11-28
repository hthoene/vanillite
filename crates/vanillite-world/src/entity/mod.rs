//! The entity store (architecture 4.2 and 4.3).
//!
//! - [`EntityId`]: generational id within a world; stale ids fail lookups. [`NetworkId`] is
//!   Vanilla's separate server-wide `int` id.
//! - **Core columns** ([`CoreColumns`]): what every entity has, one dense column per field
//!   (SoA). Hot sweeps (tracking, physics broad phase, idle checks) read the columns directly.
//! - **Kind stores** ([`Dense`]): per-kind data, owned by the gameplay crate. The store keeps
//!   each entity's kind and its row in that kind store.
//! - **Tick order**: an insertion-ordered set (Vanilla's linked map); [`EntityStore::tick_cursor`]
//!   walks it in Vanilla's order.
//! - **Section index** ([`SectionIndex`]): entity ids per 16³ section in Vanilla's order,
//!   updated whenever an entity changes section.
//!
//! Mutations go through [`EntityMut`] so that the bounding box, the section index and the
//! dirty flags stay consistent with the columns.

mod dense;
mod sections;
mod tick_order;

use std::sync::atomic::{AtomicI32, Ordering};

use vanillite_base::hash::{FxHashMap, FxHashSet};
use vanillite_base::{Aabb, BlockPos, SectionPos, SlotMap, Vec3};
use vanillite_data::entity::{EntityTypeId, ids};

pub use dense::Dense;
use sections::Subset;
pub use sections::{EntityClass, KindSet, SectionIndex};
pub use tick_order::TickCursor;

use tick_order::TickOrder;

vanillite_base::define_slot_key!(
    /// A generational entity id within one world. Stale ids fail lookups; plugin handles map to
    /// it.
    pub struct EntityId;
);

/// Vanilla's network entity id: one server-wide `int` counter, separate from [`EntityId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NetworkId(pub i32);

/// Allocates network ids like Vanilla's `Entity.ENTITY_COUNTER` (the first id is 1). One per
/// server, shared by all worlds.
#[derive(Debug, Default)]
pub struct NetworkIds(AtomicI32);

impl NetworkIds {
    /// A fresh counter.
    pub const fn new() -> NetworkIds {
        NetworkIds(AtomicI32::new(0))
    }

    /// The next id (`incrementAndGet`, wrapping like Java).
    pub fn next(&self) -> NetworkId {
        NetworkId(self.0.fetch_add(1, Ordering::Relaxed).wrapping_add(1))
    }

    /// The first of `count` consecutive ids (an entity with parts: the ender dragon's parts are
    /// numbered after it on the client).
    pub fn next_block(&self, count: i32) -> NetworkId {
        NetworkId(self.0.fetch_add(count, Ordering::Relaxed).wrapping_add(1))
    }
}

/// Boolean state of an entity in one word: engine flags in the low byte, Vanilla's shared
/// flags (synched data byte 0) in the second byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EntityFlags(u32);

impl EntityFlags {
    /// No flags.
    pub const EMPTY: EntityFlags = EntityFlags(0);
    /// `onGround`.
    pub const ON_GROUND: EntityFlags = EntityFlags(1 << 0);
    /// `isNoGravity`.
    pub const NO_GRAVITY: EntityFlags = EntityFlags(1 << 1);
    /// `hasImpulse`: the velocity was changed from outside; the tracker sends it.
    pub const HAS_IMPULSE: EntityFlags = EntityFlags(1 << 2);
    /// `hurtMarked`: the velocity must be sent to viewers and the entity itself.
    pub const HURT_MARKED: EntityFlags = EntityFlags(1 << 3);
    /// Some synched entity data changed; the tracker sends it.
    pub const DATA_DIRTY: EntityFlags = EntityFlags(1 << 4);
    /// The entity rides another one.
    pub const PASSENGER: EntityFlags = EntityFlags(1 << 5);
    /// `getRequiresPrecisePosition`: movement is always sent as an absolute position.
    pub const PRECISE_POSITION: EntityFlags = EntityFlags(1 << 6);
    /// `Entity.permanentlyInvulnerable` (the `Invulnerable` tag).
    pub const INVULNERABLE: EntityFlags = EntityFlags(1 << 7);
    /// `Entity.hasGlowingTag` (the `Glowing` tag); [`EntityFlags::GLOWING`] is what clients
    /// see (the tag or the effect).
    pub const GLOWING_TAG: EntityFlags = EntityFlags(1 << 16);
    /// `isCustomNameVisible` (synched data 3).
    pub const NAME_VISIBLE: EntityFlags = EntityFlags(1 << 17);
    /// `Entity.hasVisualFire`: clients see the entity burn; it takes no damage from it.
    pub const VISUAL_FIRE: EntityFlags = EntityFlags(1 << 18);

    /// Shared flag `FLAG_ONFIRE`.
    pub const ON_FIRE: EntityFlags = EntityFlags(1 << 8);
    /// Shared flag `FLAG_SHIFT_KEY_DOWN` (sneaking).
    pub const CROUCHING: EntityFlags = EntityFlags(1 << 9);
    /// Shared flag `FLAG_SPRINTING`.
    pub const SPRINTING: EntityFlags = EntityFlags(1 << 11);
    /// Shared flag `FLAG_SWIMMING`.
    pub const SWIMMING: EntityFlags = EntityFlags(1 << 12);
    /// Shared flag `FLAG_INVISIBLE`.
    pub const INVISIBLE: EntityFlags = EntityFlags(1 << 13);
    /// Shared flag `FLAG_GLOWING`.
    pub const GLOWING: EntityFlags = EntityFlags(1 << 14);
    /// Shared flag `FLAG_FALL_FLYING`.
    pub const FALL_FLYING: EntityFlags = EntityFlags(1 << 15);

    const SHARED: u32 = 0xFF00;

    /// Whether all flags of `other` are set.
    #[inline]
    pub const fn contains(self, other: EntityFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Sets or clears `flags`.
    #[inline]
    pub fn set(&mut self, flags: EntityFlags, on: bool) {
        if on {
            self.0 |= flags.0;
        } else {
            self.0 &= !flags.0;
        }
    }

    /// Vanilla's shared flags byte (`DATA_SHARED_FLAGS_ID`).
    #[inline]
    pub const fn shared_byte(self) -> u8 {
        ((self.0 & Self::SHARED) >> 8) as u8
    }

    /// Whether `flags` contains shared flags.
    #[inline]
    const fn has_shared(flags: EntityFlags) -> bool {
        flags.0 & Self::SHARED != 0
    }
}

impl std::ops::BitOr for EntityFlags {
    type Output = EntityFlags;

    fn bitor(self, rhs: EntityFlags) -> EntityFlags {
        EntityFlags(self.0 | rhs.0)
    }
}

/// Vanilla's `Pose`, in protocol order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Pose {
    #[default]
    Standing,
    FallFlying,
    Sleeping,
    Swimming,
    SpinAttack,
    Crouching,
    LongJumping,
    Dying,
    Croaking,
    UsingTongue,
    Sitting,
    Roaring,
    Sniffing,
    Emerging,
    Digging,
    Sliding,
    Shooting,
    Inhaling,
}

/// Rotation in degrees: `yRot`, `xRot` and the head's `yHeadRot` (0 for entities without a
/// head, like Vanilla's `Entity.getYHeadRot`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rotation {
    /// Yaw.
    pub y_rot: f32,
    /// Pitch.
    pub x_rot: f32,
    /// Head yaw.
    pub y_head_rot: f32,
}

/// `EntityDimensions`: width and height of the bounding box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dimensions {
    /// Width (X and Z).
    pub width: f32,
    /// Height.
    pub height: f32,
}

impl Dimensions {
    /// The default dimensions of an entity type.
    pub fn of(kind: EntityTypeId) -> Dimensions {
        let info = kind.info();
        Dimensions {
            width: info.width,
            height: info.height,
        }
    }

    /// `makeBoundingBox(position)`: centred on X and Z, standing on `pos.y`.
    #[inline]
    pub fn bounding_box(self, pos: Vec3) -> Aabb {
        let half = f64::from(self.width / 2.0);
        let height = f64::from(self.height);
        Aabb::new(
            pos.x - half,
            pos.y,
            pos.z - half,
            pos.x + half,
            pos.y + height,
            pos.z + half,
        )
    }
}

/// The section of an entity at `pos` (`SectionPos.of(blockPosition())`).
#[inline]
fn section_of(pos: Vec3) -> SectionPos {
    BlockPos::containing(pos.x, pos.y, pos.z).section()
}

/// The class an entity of `kind` is indexed under.
#[inline]
fn class_of(kind: EntityTypeId) -> EntityClass {
    if kind == ids::PLAYER {
        EntityClass::Player
    } else {
        EntityClass::Other
    }
}

/// The core columns: one dense vector per field, indexed by row. Rows are not stable (removal
/// swaps the last row in); use [`EntityId`] to keep a reference.
#[derive(Debug, Default)]
pub struct CoreColumns {
    /// The id of each row.
    pub id: Vec<EntityId>,
    /// Network id.
    pub network_id: Vec<NetworkId>,
    /// UUID.
    pub uuid: Vec<u128>,
    /// Entity type.
    pub kind: Vec<EntityTypeId>,
    /// Row in the kind store of the entity's type.
    pub kind_row: Vec<u32>,
    /// Position.
    pub pos: Vec<Vec3>,
    /// Position at the start of the tick (`xo`, `yo`, `zo`).
    pub old_pos: Vec<Vec3>,
    /// Velocity (`deltaMovement`).
    pub velocity: Vec<Vec3>,
    /// Rotation.
    pub rot: Vec<Rotation>,
    /// Bounding box size.
    pub dims: Vec<Dimensions>,
    /// Bounding box.
    pub aabb: Vec<Aabb>,
    /// Flags.
    pub flags: Vec<EntityFlags>,
    /// Pose.
    pub pose: Vec<Pose>,
    /// Current section.
    pub section: Vec<SectionPos>,
}

impl CoreColumns {
    fn swap_remove(&mut self, row: usize) {
        self.id.swap_remove(row);
        self.network_id.swap_remove(row);
        self.uuid.swap_remove(row);
        self.kind.swap_remove(row);
        self.kind_row.swap_remove(row);
        self.pos.swap_remove(row);
        self.old_pos.swap_remove(row);
        self.velocity.swap_remove(row);
        self.rot.swap_remove(row);
        self.dims.swap_remove(row);
        self.aabb.swap_remove(row);
        self.flags.swap_remove(row);
        self.pose.swap_remove(row);
        self.section.swap_remove(row);
    }
}

/// What a new entity starts with.
#[derive(Clone, Copy, Debug)]
pub struct NewEntity {
    /// Entity type.
    pub kind: EntityTypeId,
    /// Network id, from the server's [`NetworkIds`].
    pub network_id: NetworkId,
    /// UUID.
    pub uuid: u128,
    /// Position.
    pub pos: Vec3,
    /// Rotation.
    pub rot: Rotation,
    /// Velocity.
    pub velocity: Vec3,
    /// Flags.
    pub flags: EntityFlags,
}

impl NewEntity {
    /// An entity of `kind` at `pos` with everything else at its default.
    pub fn new(kind: EntityTypeId, network_id: NetworkId, uuid: u128, pos: Vec3) -> NewEntity {
        NewEntity {
            kind,
            network_id,
            uuid,
            pos,
            rot: Rotation::default(),
            velocity: Vec3::ZERO,
            flags: EntityFlags::EMPTY,
        }
    }
}

/// An entity that left the store: what its kind store and the tracker still need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removed {
    /// The (now stale) id.
    pub id: EntityId,
    /// Its network id.
    pub network_id: NetworkId,
    /// Its type.
    pub kind: EntityTypeId,
    /// Its row in the kind store; the caller removes that row.
    pub kind_row: u32,
}

/// The entities of one world.
#[derive(Debug, Default)]
pub struct EntityStore {
    rows: SlotMap<EntityId, u32>,
    core: CoreColumns,
    /// Slot in the tick order per row, [`tick_order::NONE`] while not ticking.
    order_slot: Vec<u32>,
    tick_order: TickOrder,
    sections: SectionIndex,
    removed: Vec<Removed>,
    /// Chunks whose entities tick (Vanilla's `Visibility.TICKING`), set by the world: only
    /// entities in these chunks are in the tick order.
    ticking_chunks: FxHashSet<(i32, i32)>,
    /// UUID → entity (`EntityLookup.byUuid`). With duplicate UUIDs it names one of them.
    by_uuid: FxHashMap<u128, EntityId>,
    /// Entities whose UUID another live entity had when they were added (not in `by_uuid`).
    duplicate_uuids: usize,
}

impl EntityStore {
    /// An empty store.
    pub fn new() -> EntityStore {
        EntityStore::default()
    }

    /// Number of entities.
    #[inline]
    pub fn len(&self) -> usize {
        self.core.id.len()
    }

    /// Whether the store is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.core.id.is_empty()
    }

    /// Whether `id` refers to a live entity.
    #[inline]
    pub fn contains(&self, id: EntityId) -> bool {
        self.rows.contains(id)
    }

    /// The current row of `id`.
    #[inline]
    pub fn row(&self, id: EntityId) -> Option<usize> {
        self.rows.get(id).map(|&row| row as usize)
    }

    /// The entity with a UUID (`getEntity(UUID)`), through the index.
    #[inline]
    pub fn by_uuid(&self, uuid: u128) -> Option<EntityId> {
        self.by_uuid.get(&uuid).copied()
    }

    /// The core columns for sweeps. Change entities through [`Self::get_mut`].
    #[inline]
    pub fn columns(&self) -> &CoreColumns {
        &self.core
    }

    /// The section index.
    #[inline]
    pub fn sections(&self) -> &SectionIndex {
        &self.sections
    }

    /// Read access to one entity.
    #[inline]
    pub fn get(&self, id: EntityId) -> Option<EntityRef<'_>> {
        let row = self.row(id)?;
        Some(EntityRef {
            core: &self.core,
            row,
        })
    }

    /// Write access to one entity.
    #[inline]
    pub fn get_mut(&mut self, id: EntityId) -> Option<EntityMut<'_>> {
        let row = self.row(id)?;
        Some(EntityMut { store: self, row })
    }

    /// Adds an entity and appends it to the tick order. `kind_row` gets the new id and returns
    /// the entity's row in its kind store (e.g. `|id| players.push(id, data)`).
    pub fn spawn(&mut self, new: NewEntity, kind_row: impl FnOnce(EntityId) -> u32) -> EntityId {
        let row = u32::try_from(self.len()).expect("entity store full");
        let id = self.rows.insert(row);
        let dims = Dimensions::of(new.kind);
        let section = section_of(new.pos);
        let core = &mut self.core;
        core.id.push(id);
        core.network_id.push(new.network_id);
        core.uuid.push(new.uuid);
        core.kind.push(new.kind);
        core.kind_row.push(kind_row(id));
        core.pos.push(new.pos);
        core.old_pos.push(new.pos);
        core.velocity.push(new.velocity);
        core.rot.push(new.rot);
        core.dims.push(dims);
        let aabb = dims.bounding_box(new.pos);
        core.aabb.push(aabb);
        core.flags.push(new.flags);
        core.pose.push(Pose::Standing);
        core.section.push(section);
        // `addEntity`: it starts ticking if its section ticks.
        let slot = if self.ticking_chunks.contains(&(section.x, section.z)) {
            self.tick_order.push(id)
        } else {
            tick_order::NONE
        };
        self.order_slot.push(slot);
        self.sections
            .insert(section, class_of(new.kind), new.kind, id, aabb);
        match self.by_uuid.entry(new.uuid) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(id);
            }
            std::collections::hash_map::Entry::Occupied(_) => self.duplicate_uuids += 1,
        }
        id
    }

    /// Gives `id` another UUID (`Entity.setUUID` while loading a summoned entity's data).
    pub fn set_uuid(&mut self, id: EntityId, uuid: u128) {
        let Some(row) = self.row(id) else {
            return;
        };
        let old = self.core.uuid[row];
        if old == uuid {
            return;
        }
        self.unindex_uuid(id, old);
        self.core.uuid[row] = uuid;
        match self.by_uuid.entry(uuid) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(id);
            }
            std::collections::hash_map::Entry::Occupied(_) => self.duplicate_uuids += 1,
        }
    }

    /// Removes an entity. The caller removes the returned kind row from its kind store and
    /// reports the entity that moved into that row with [`Self::set_kind_row`]. The removal is
    /// also logged for the tracker until [`Self::clear_removed`].
    pub fn despawn(&mut self, id: EntityId) -> Option<Removed> {
        let row = self.rows.remove(id)? as usize;
        self.unindex_uuid(id, self.core.uuid[row]);
        let core = &mut self.core;
        let removed = Removed {
            id,
            network_id: core.network_id[row],
            kind: core.kind[row],
            kind_row: core.kind_row[row],
        };
        self.sections
            .remove(core.section[row], class_of(removed.kind), id);
        self.tick_order.remove(self.order_slot[row]);
        core.swap_remove(row);
        self.order_slot.swap_remove(row);
        if let Some(&moved) = core.id.get(row)
            && let Some(moved_row) = self.rows.get_mut(moved)
        {
            *moved_row = row as u32;
        }
        self.removed.push(removed);
        Some(removed)
    }

    /// Removes `id` from the UUID index before it leaves the columns; another entity with the
    /// same UUID takes its place.
    fn unindex_uuid(&mut self, id: EntityId, uuid: u128) {
        if self.by_uuid.get(&uuid) != Some(&id) {
            self.duplicate_uuids = self.duplicate_uuids.saturating_sub(1);
            return;
        }
        self.by_uuid.remove(&uuid);
        if self.duplicate_uuids > 0 {
            self.index_duplicate(id, uuid);
        }
    }

    /// Indexes another live entity with `uuid` than `id`, if there is one (rare: duplicates).
    #[cold]
    fn index_duplicate(&mut self, id: EntityId, uuid: u128) {
        let core = &self.core;
        if let Some(other) = (0..core.id.len()).find(|&r| core.uuid[r] == uuid && core.id[r] != id)
        {
            self.by_uuid.insert(uuid, core.id[other]);
            self.duplicate_uuids -= 1;
        }
    }

    /// Records that the kind store moved `id` to `kind_row`.
    pub fn set_kind_row(&mut self, id: EntityId, kind_row: u32) {
        if let Some(row) = self.row(id) {
            self.core.kind_row[row] = kind_row;
        }
    }

    /// Entities removed since the last [`Self::clear_removed`], in removal order.
    #[inline]
    pub fn removed(&self) -> &[Removed] {
        &self.removed
    }

    /// Clears the removal log; called after the tracker ran.
    pub fn clear_removed(&mut self) {
        self.removed.clear();
    }

    /// Clears the per-tick sync flags (`hasImpulse`, `hurtMarked`, dirty data) of all entities;
    /// called after the tracker ran and the updates were encoded.
    pub fn clear_sync_flags(&mut self) {
        let sync = EntityFlags::HAS_IMPULSE | EntityFlags::HURT_MARKED | EntityFlags::DATA_DIRTY;
        for flags in &mut self.core.flags {
            flags.set(sync, false);
        }
    }

    /// Whether an entity is in the tick order.
    pub fn is_ticking(&self, id: EntityId) -> bool {
        self.row(id)
            .is_some_and(|row| self.order_slot[row] != tick_order::NONE)
    }

    /// Whether the entities of a chunk tick.
    pub fn is_chunk_ticking(&self, x: i32, z: i32) -> bool {
        self.ticking_chunks.contains(&(x, z))
    }

    /// Sets whether the entities of a chunk tick (`updateChunkStatus`): its entities start
    /// ticking at the end of the tick order, section by section from the bottom, or stop.
    pub fn set_chunk_ticking(&mut self, x: i32, z: i32, ticking: bool) {
        let changed = if ticking {
            self.ticking_chunks.insert((x, z))
        } else {
            self.ticking_chunks.remove(&(x, z))
        };
        if !changed {
            return;
        }
        let ids: Vec<EntityId> = self.sections.in_chunk(x, z).collect();
        for id in ids {
            if ticking {
                self.start_ticking(id);
            } else {
                self.stop_ticking(id);
            }
        }
    }

    /// Adds an entity back to the end of the tick order (e.g. when its chunk starts ticking).
    pub fn start_ticking(&mut self, id: EntityId) {
        if let Some(row) = self.row(id)
            && self.order_slot[row] == tick_order::NONE
        {
            self.order_slot[row] = self.tick_order.push(id);
        }
    }

    /// Takes an entity out of the tick order. The others keep their order.
    pub fn stop_ticking(&mut self, id: EntityId) {
        if let Some(row) = self.row(id) {
            let slot = std::mem::replace(&mut self.order_slot[row], tick_order::NONE);
            self.tick_order.remove(slot);
        }
    }

    /// Starts a walk over the tick order (Vanilla's `entityTickList.forEach`): entities added
    /// during the walk are not visited, removed ones are skipped. One walk at a time.
    pub fn tick_cursor(&mut self) -> TickCursor {
        let rows = &self.rows;
        let order_slot = &mut self.order_slot;
        self.tick_order.compact(|id, slot| {
            if let Some(&row) = rows.get(id) {
                order_slot[row as usize] = slot;
            }
        });
        TickCursor::new(self.tick_order.len())
    }

    /// The next entity of a walk started with [`Self::tick_cursor`].
    #[inline]
    pub fn next_to_tick(&self, cursor: &mut TickCursor) -> Option<EntityId> {
        cursor.next(&self.tick_order)
    }

    /// Calls `f` for every entity (or only every player) whose bounding box intersects `aabb`,
    /// in Vanilla's order (`EntitySectionStorage.getEntities`).
    pub fn for_each_in(&self, aabb: &Aabb, players_only: bool, f: impl FnMut(EntityId)) {
        let subset = if players_only {
            Subset::Players
        } else {
            Subset::All
        };
        self.for_each_in_subset(aabb, subset, f);
    }

    /// Like [`Self::for_each_in`] for the entities of the types in `kinds` (players too when
    /// the set has them): the order of [`Self::for_each_in`] without the other entities.
    pub fn for_each_of_kinds_in(&self, aabb: &Aabb, kinds: &KindSet, f: impl FnMut(EntityId)) {
        self.sections.for_each_intersecting_of(aabb, kinds, f);
    }

    /// Like [`Self::for_each_in`] for every entity that is not a player: queries for other
    /// kinds (items a player picks up) skip the players of crowded sections.
    pub fn for_each_non_player_in(&self, aabb: &Aabb, f: impl FnMut(EntityId)) {
        self.for_each_in_subset(aabb, Subset::Others, f);
    }

    /// Calls `f` for every player whose position is within `range` blocks of `pos` on the X
    /// and Z axes, and for some farther ones (the caller measures), in no particular order,
    /// until `f` returns `true`. For questions about positions without an order, like the
    /// distance to the nearest player; not for bounding boxes, which reach past a position.
    pub fn players_around(&self, pos: Vec3, range: f64, f: impl FnMut(EntityId) -> bool) {
        self.sections.players_around(pos.x, pos.z, range, f);
    }

    fn for_each_in_subset(&self, aabb: &Aabb, subset: Subset, mut f: impl FnMut(EntityId)) {
        if subset == Subset::All {
            self.sections.for_each_intersecting(aabb, f);
            return;
        }
        self.sections.for_each_near(aabb, subset, |id| {
            if let Some(row) = self.row(id)
                && self.core.aabb[row].intersects(aabb)
            {
                f(id);
            }
        });
    }
}

/// Read access to one entity's core fields.
#[derive(Clone, Copy, Debug)]
pub struct EntityRef<'a> {
    core: &'a CoreColumns,
    row: usize,
}

impl EntityRef<'_> {
    /// Row in the columns (valid until the next removal).
    #[inline]
    pub fn row(&self) -> usize {
        self.row
    }

    /// Id.
    #[inline]
    pub fn id(&self) -> EntityId {
        self.core.id[self.row]
    }

    /// Network id.
    #[inline]
    pub fn network_id(&self) -> NetworkId {
        self.core.network_id[self.row]
    }

    /// UUID.
    #[inline]
    pub fn uuid(&self) -> u128 {
        self.core.uuid[self.row]
    }

    /// Entity type.
    #[inline]
    pub fn kind(&self) -> EntityTypeId {
        self.core.kind[self.row]
    }

    /// Row in the kind store.
    #[inline]
    pub fn kind_row(&self) -> u32 {
        self.core.kind_row[self.row]
    }

    /// Position.
    #[inline]
    pub fn pos(&self) -> Vec3 {
        self.core.pos[self.row]
    }

    /// Position at the start of the tick.
    #[inline]
    pub fn old_pos(&self) -> Vec3 {
        self.core.old_pos[self.row]
    }

    /// Velocity.
    #[inline]
    pub fn velocity(&self) -> Vec3 {
        self.core.velocity[self.row]
    }

    /// Rotation.
    #[inline]
    pub fn rot(&self) -> Rotation {
        self.core.rot[self.row]
    }

    /// Bounding box size.
    #[inline]
    pub fn dims(&self) -> Dimensions {
        self.core.dims[self.row]
    }

    /// Bounding box.
    #[inline]
    pub fn aabb(&self) -> Aabb {
        self.core.aabb[self.row]
    }

    /// Flags.
    #[inline]
    pub fn flags(&self) -> EntityFlags {
        self.core.flags[self.row]
    }

    /// Pose.
    #[inline]
    pub fn pose(&self) -> Pose {
        self.core.pose[self.row]
    }

    /// Current section.
    #[inline]
    pub fn section(&self) -> SectionPos {
        self.core.section[self.row]
    }
}

/// Write access to one entity. Keeps the bounding box, the section index and the dirty flags
/// consistent.
#[derive(Debug)]
pub struct EntityMut<'a> {
    store: &'a mut EntityStore,
    row: usize,
}

impl EntityMut<'_> {
    /// Read access.
    #[inline]
    pub fn as_ref(&self) -> EntityRef<'_> {
        EntityRef {
            core: &self.store.core,
            row: self.row,
        }
    }

    /// `setPos`: moves the entity, updates its bounding box and, on a section change, the
    /// section index.
    pub fn set_pos(&mut self, pos: Vec3) {
        let row = self.row;
        let core = &mut self.store.core;
        core.pos[row] = pos;
        let aabb = core.dims[row].bounding_box(pos);
        core.aabb[row] = aabb;
        let section = section_of(pos);
        let old = core.section[row];
        let id = core.id[row];
        if section == old {
            self.store.sections.set_box(section, id, aabb);
        } else {
            core.section[row] = section;
            let kind = core.kind[row];
            let class = class_of(kind);
            self.store.sections.remove(old, class, id);
            self.store.sections.insert(section, class, kind, id, aabb);
            // `onMove`: ticking follows the status of the new section.
            let ticking = &self.store.ticking_chunks;
            let was = ticking.contains(&(old.x, old.z));
            let is = ticking.contains(&(section.x, section.z));
            if is && !was {
                self.store.start_ticking(id);
            } else if was && !is {
                self.store.stop_ticking(id);
            }
        }
    }

    /// `setOldPos`: remembers the current position as the start of the tick.
    #[inline]
    pub fn set_old_pos(&mut self) {
        let core = &mut self.store.core;
        core.old_pos[self.row] = core.pos[self.row];
    }

    /// Sets the velocity.
    #[inline]
    pub fn set_velocity(&mut self, velocity: Vec3) {
        self.store.core.velocity[self.row] = velocity;
    }

    /// Sets the rotation.
    #[inline]
    pub fn set_rot(&mut self, rot: Rotation) {
        self.store.core.rot[self.row] = rot;
    }

    /// Sets the bounding box size (`refreshDimensions`) and recomputes the box.
    pub fn set_dims(&mut self, dims: Dimensions) {
        let core = &mut self.store.core;
        core.dims[self.row] = dims;
        let aabb = dims.bounding_box(core.pos[self.row]);
        core.aabb[self.row] = aabb;
        let (section, id) = (core.section[self.row], core.id[self.row]);
        self.store.sections.set_box(section, id, aabb);
    }

    /// Sets a bounding box that is not centred on the position (`setBoundingBox` of
    /// block-attached entities); the next [`EntityMut::set_pos`] or
    /// [`EntityMut::set_dims`] recomputes it from the dimensions.
    #[inline]
    pub fn set_aabb(&mut self, aabb: Aabb) {
        let core = &mut self.store.core;
        core.aabb[self.row] = aabb;
        let (section, id) = (core.section[self.row], core.id[self.row]);
        self.store.sections.set_box(section, id, aabb);
    }

    /// Sets or clears flags. Changing a shared flag marks the synched data dirty.
    pub fn set_flags(&mut self, flags: EntityFlags, on: bool) {
        let current = &mut self.store.core.flags[self.row];
        let before = *current;
        current.set(flags, on);
        if EntityFlags::has_shared(flags) && current.shared_byte() != before.shared_byte() {
            current.set(EntityFlags::DATA_DIRTY, true);
        }
    }

    /// Sets the pose; a change marks the synched data dirty.
    pub fn set_pose(&mut self, pose: Pose) {
        let core = &mut self.store.core;
        if core.pose[self.row] != pose {
            core.pose[self.row] = pose;
            core.flags[self.row].set(EntityFlags::DATA_DIRTY, true);
        }
    }

    /// Marks synched data of the kind store as changed.
    #[inline]
    pub fn mark_data_dirty(&mut self) {
        self.store.core.flags[self.row].set(EntityFlags::DATA_DIRTY, true);
    }
}
