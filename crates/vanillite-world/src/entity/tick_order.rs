//! The tick order: an insertion-ordered set with Vanilla's linked-map semantics.

use super::EntityId;

/// Marks an entity that is not in the tick order.
pub(super) const NONE: u32 = u32::MAX;

/// Entity ids in insertion order. Removal leaves a hole so that the others keep their order
/// and a running walk stays valid; holes are compacted when the next walk starts.
#[derive(Debug, Default)]
pub(super) struct TickOrder {
    list: Vec<Option<EntityId>>,
    holes: usize,
}

impl TickOrder {
    /// Gives back the spare room of the list.
    pub(super) fn shrink_to_fit(&mut self) {
        self.list.shrink_to_fit();
    }

    /// Appends an id and returns its slot.
    pub(super) fn push(&mut self, id: EntityId) -> u32 {
        let slot = u32::try_from(self.list.len()).expect("tick order full");
        self.list.push(Some(id));
        slot
    }

    /// Removes the id in `slot` (no-op for [`NONE`]).
    pub(super) fn remove(&mut self, slot: u32) {
        if let Some(entry) = self.list.get_mut(slot as usize)
            && entry.take().is_some()
        {
            self.holes += 1;
        }
    }

    /// Number of slots, holes included.
    pub(super) fn len(&self) -> usize {
        self.list.len()
    }

    /// Closes all holes and reports the new slot of every moved id.
    pub(super) fn compact(&mut self, mut moved: impl FnMut(EntityId, u32)) {
        if self.holes == 0 {
            return;
        }
        let mut write = 0u32;
        for read in 0..self.list.len() {
            if let Some(id) = self.list[read] {
                if write as usize != read {
                    self.list[write as usize] = Some(id);
                    moved(id, write);
                }
                write += 1;
            }
        }
        self.list.truncate(write as usize);
        self.holes = 0;
    }
}

/// A walk over the tick order, see [`EntityStore::tick_cursor`](super::EntityStore::tick_cursor).
#[derive(Debug)]
pub struct TickCursor {
    next: usize,
    end: usize,
}

impl TickCursor {
    pub(super) fn new(end: usize) -> TickCursor {
        TickCursor { next: 0, end }
    }

    pub(super) fn next(&mut self, order: &TickOrder) -> Option<EntityId> {
        while self.next < self.end {
            let entry = order.list.get(self.next).copied().flatten();
            self.next += 1;
            if entry.is_some() {
                return entry;
            }
        }
        None
    }
}
