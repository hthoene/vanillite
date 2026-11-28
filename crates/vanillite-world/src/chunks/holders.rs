//! The chunk holders of a world, with a per-thread cache of the section the last block was
//! read from: block reads (redstone, explosions, collisions, path finding) mostly hit the
//! section of the read before, and the cache saves the map lookup and the pointer chain to the
//! section.

use std::cell::Cell;
use std::ops::Deref;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use vanillite_base::hash::FxHashMap;
use vanillite_base::{BlockPos, ChunkPos};
use vanillite_chunk::{BlockStateId, ChunkStatus, Section};

use super::{FULL_LEVEL, Holder};

/// Source of the table ids; 0 is never handed out, so the empty cache entry matches nothing.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The section a thread read a block from last.
#[derive(Clone, Copy)]
struct LastSection {
    /// The table ([`Holders::id`]) and its state ([`Holders::epoch`]) the entry was taken from.
    id: u64,
    epoch: u64,
    chunk: ChunkPos,
    /// Section y (block y >> 4).
    section_y: i32,
    /// The chunk's level is at most full (`load_now` needs no load).
    loaded_now: bool,
    section: *const Section,
}

thread_local! {
    static LAST: Cell<LastSection> = const {
        Cell::new(LastSection {
            id: 0,
            epoch: 0,
            chunk: ChunkPos { x: 0, z: 0 },
            section_y: 0,
            loaded_now: false,
            section: ptr::null(),
        })
    };
}

/// The holders by chunk position. Reads go through `Deref`; every mutable access goes through
/// [`Holders::map_mut`], which moves the table to a new epoch.
pub(super) struct Holders {
    map: FxHashMap<ChunkPos, Holder>,
    id: u64,
    epoch: u64,
}

impl Holders {
    pub(super) fn new() -> Self {
        Self {
            map: FxHashMap::default(),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            epoch: 0,
        }
    }

    /// The epoch: it moves on with every mutable access ([`Holders::map_mut`]), so equal
    /// epochs see equal chunks.
    #[inline]
    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The map for changes. Every section pointer a thread cached before is stale from now on.
    #[inline]
    pub(super) fn map_mut(&mut self) -> &mut FxHashMap<ChunkPos, Holder> {
        self.epoch += 1;
        &mut self.map
    }

    /// The block state at `pos` in a full chunk and whether the chunk's level is at most full;
    /// `None` outside full chunks and the build height. No side effects.
    #[inline(always)]
    pub(super) fn full_block(&self, pos: BlockPos) -> Option<(BlockStateId, bool)> {
        let chunk = pos.chunk();
        let section_y = pos.y >> 4;
        let last = LAST.get();
        let (section, loaded_now) = if last.id == self.id
            && last.epoch == self.epoch
            && last.chunk == chunk
            && last.section_y == section_y
        {
            // SAFETY: the pointer was taken from a section of a chunk in this table (same id)
            // in the same epoch. Every mutable access to the table, and so to its chunks and
            // sections, starts a new epoch ([`Holders::map_mut`]), and the table owns the
            // chunk (`Arc`) the section is in for as long as the epoch lasts. `&self` keeps
            // the table from changing while the reference lives.
            (unsafe { &*last.section }, last.loaded_now)
        } else {
            self.lookup(chunk, section_y)?
        };
        Some((
            section.block(
                (pos.x & 15) as u32,
                (pos.y & 15) as u32,
                (pos.z & 15) as u32,
            ),
            loaded_now,
        ))
    }

    /// The section at `section_y` of a full chunk; `None` outside full chunks and the build
    /// height. Shares the per-thread cache of [`Holders::full_block`].
    #[inline]
    pub(super) fn full_section(&self, chunk: ChunkPos, section_y: i32) -> Option<&Section> {
        let last = LAST.get();
        if last.id == self.id
            && last.epoch == self.epoch
            && last.chunk == chunk
            && last.section_y == section_y
        {
            // SAFETY: as in [`Holders::full_block`].
            return Some(unsafe { &*last.section });
        }
        self.lookup(chunk, section_y).map(|(section, _)| section)
    }

    /// [`Holders::full_block`] through the map; caches the section for the next read.
    #[inline(never)]
    fn lookup(&self, chunk: ChunkPos, section_y: i32) -> Option<(&Section, bool)> {
        let holder = self.map.get(&chunk)?;
        let proto = holder
            .chunk
            .as_ref()
            .filter(|c| c.status == ChunkStatus::Full)?;
        let index = usize::try_from(section_y - proto.chunk.min_section()).ok()?;
        let section: &Section = proto.chunk.sections().get(index)?;
        let loaded_now = holder.level <= FULL_LEVEL;
        LAST.set(LastSection {
            id: self.id,
            epoch: self.epoch,
            chunk,
            section_y,
            loaded_now,
            section,
        });
        Some((section, loaded_now))
    }
}

impl Deref for Holders {
    type Target = FxHashMap<ChunkPos, Holder>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}
