//! Reusable job buffers: a job takes one at its start and gives it back at its end, so the
//! next job skips the allocation (and the page faults of a fresh large buffer). Unlike a
//! thread-local, the owner can drop all of them when nothing runs ([`ScratchPool::release`]).

use std::sync::{Mutex, PoisonError};

/// A pool of reusable buffers of type `T`. It never holds more than `keep` of them, so a
/// burst of parallel jobs does not grow it beyond the number of threads that run them.
#[derive(Debug)]
pub struct ScratchPool<T> {
    free: Mutex<Vec<Box<T>>>,
    keep: usize,
}

impl<T> Default for ScratchPool<T> {
    /// A pool that keeps one buffer per hardware thread.
    fn default() -> Self {
        Self::new(std::thread::available_parallelism().map_or(4, |n| n.get()))
    }
}

impl<T> ScratchPool<T> {
    /// An empty pool that keeps at most `keep` buffers.
    pub const fn new(keep: usize) -> Self {
        Self {
            free: Mutex::new(Vec::new()),
            keep,
        }
    }

    /// A buffer from the pool, or `make()` when it is empty.
    pub fn take_or(&self, make: impl FnOnce() -> T) -> Box<T> {
        let free = self
            .free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        free.unwrap_or_else(|| Box::new(make()))
    }

    /// Returns a buffer to the pool (dropped when the pool is full).
    pub fn give(&self, buffer: Box<T>) {
        let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
        if free.len() < self.keep {
            free.push(buffer);
            return;
        }
        drop(free);
        drop(buffer);
    }

    /// Drops every buffer in the pool; buffers in use come back later as usual.
    pub fn release(&self) {
        let free = std::mem::take(&mut *self.free.lock().unwrap_or_else(PoisonError::into_inner));
        drop(free);
    }

    /// The number of buffers in the pool.
    pub fn len(&self) -> usize {
        self.free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether the pool holds no buffer.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::ScratchPool;

    #[test]
    fn reuses_keeps_at_most_keep_and_releases() {
        let pool = ScratchPool::new(1);
        let a = pool.take_or(|| vec![1u8; 4]);
        let b = pool.take_or(|| vec![2u8; 4]);
        pool.give(a);
        pool.give(b);
        assert_eq!(pool.len(), 1);
        assert_eq!(*pool.take_or(Vec::new), vec![1u8; 4]);
        pool.give(Box::new(vec![3]));
        pool.release();
        assert!(pool.is_empty());
    }
}
