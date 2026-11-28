//! Dense per-kind storage.

use super::EntityId;

/// The data of one entity kind in dense rows (`Dense<PlayerData>`). Each row remembers its
/// entity so a removal can report which entity moved into the freed row.
#[derive(Debug)]
pub struct Dense<T> {
    ids: Vec<EntityId>,
    values: Vec<T>,
}

impl<T> Default for Dense<T> {
    fn default() -> Self {
        Dense {
            ids: Vec::new(),
            values: Vec::new(),
        }
    }
}

impl<T> Dense<T> {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Gives back the room of removed rows.
    pub fn shrink_to_fit(&mut self) {
        self.ids.shrink_to_fit();
        self.values.shrink_to_fit();
    }

    /// Number of rows.
    #[inline]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether there are no rows.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Appends a row for `id` and returns its index.
    pub fn push(&mut self, id: EntityId, value: T) -> u32 {
        let row = u32::try_from(self.values.len()).expect("kind store full");
        self.ids.push(id);
        self.values.push(value);
        row
    }

    /// Removes a row by swapping the last one in. Returns the value and the entity that moved
    /// into `row`, if any; report it to [`EntityStore::set_kind_row`](super::EntityStore::set_kind_row).
    pub fn swap_remove(&mut self, row: u32) -> Option<(T, Option<EntityId>)> {
        let row = row as usize;
        if row >= self.values.len() {
            return None;
        }
        self.ids.swap_remove(row);
        let value = self.values.swap_remove(row);
        Some((value, self.ids.get(row).copied()))
    }

    /// The value in `row`.
    #[inline]
    pub fn get(&self, row: u32) -> Option<&T> {
        self.values.get(row as usize)
    }

    /// The value in `row`, mutably.
    #[inline]
    pub fn get_mut(&mut self, row: u32) -> Option<&mut T> {
        self.values.get_mut(row as usize)
    }

    /// All rows with their entities.
    pub fn iter(&self) -> impl Iterator<Item = (EntityId, &T)> {
        self.ids.iter().copied().zip(&self.values)
    }

    /// All rows with their entities, mutably.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (EntityId, &mut T)> {
        self.ids.iter().copied().zip(&mut self.values)
    }

    /// The entities and their rows side by side (row `i` belongs to entity `i`): for work
    /// that splits the rows between threads.
    pub fn rows(&self) -> (&[EntityId], &[T]) {
        (&self.ids, &self.values)
    }

    /// [`Dense::rows`] with the rows mutably.
    pub fn rows_mut(&mut self) -> (&[EntityId], &mut [T]) {
        (&self.ids, &mut self.values)
    }
}
