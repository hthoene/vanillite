//! The two collections whose iteration and tie order decide Vanilla's tick order when containers
//! tie (stored ticks of different chunks share sub-tick values): fastutil's
//! `Long2LongOpenHashMap` (`nextTickForContainer`) and `java.util.PriorityQueue`
//! (`containersToTick`).

/// `Hash.DEFAULT_LOAD_FACTOR`.
const LOAD_FACTOR: f64 = 0.75;
/// `Hash.DEFAULT_INITIAL_SIZE`.
const INITIAL_SIZE: usize = 16;

/// `HashCommon.mix(long)`.
#[inline]
fn mix(x: i64) -> usize {
    let h = x.wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as i64);
    let h = h ^ ((h as u64) >> 32) as i64;
    (h ^ ((h as u64) >> 16) as i64) as u64 as usize
}

/// `HashCommon.arraySize(expected, f)`.
fn array_size(expected: usize) -> usize {
    ((expected as f64 / LOAD_FACTOR).ceil() as usize)
        .next_power_of_two()
        .max(2)
}

/// `HashCommon.maxFill(n, f)`.
fn max_fill(n: usize) -> usize {
    ((n as f64 * LOAD_FACTOR).ceil() as usize).min(n - 1)
}

/// fastutil's `Long2LongOpenHashMap` as `new Long2LongOpenHashMap()` builds it: the same slots,
/// so it iterates (null key first, then from the last slot down) in the same order.
#[derive(Debug, Clone)]
pub(super) struct Long2LongMap {
    /// `n + 1` slots; key `0` is free, the null key's value lives in slot `n`.
    key: Vec<i64>,
    value: Vec<i64>,
    contains_null_key: bool,
    n: usize,
    min_n: usize,
    max_fill: usize,
    size: usize,
}

impl Default for Long2LongMap {
    fn default() -> Self {
        let n = array_size(INITIAL_SIZE);
        Self {
            key: vec![0; n + 1],
            value: vec![0; n + 1],
            contains_null_key: false,
            n,
            min_n: n,
            max_fill: max_fill(n),
            size: 0,
        }
    }
}

impl Long2LongMap {
    fn mask(&self) -> usize {
        self.n - 1
    }

    /// `find(k)`: `Ok(slot)` or `Err(free slot)`.
    fn find(&self, k: i64) -> Result<usize, usize> {
        if k == 0 {
            return if self.contains_null_key {
                Ok(self.n)
            } else {
                Err(self.n)
            };
        }
        let mask = self.mask();
        let mut pos = mix(k) & mask;
        loop {
            let curr = self.key[pos];
            if curr == 0 {
                return Err(pos);
            }
            if curr == k {
                return Ok(pos);
            }
            pos = (pos + 1) & mask;
        }
    }

    /// `put(k, v)`.
    pub(super) fn put(&mut self, k: i64, v: i64) {
        match self.find(k) {
            Ok(pos) => self.value[pos] = v,
            Err(pos) => {
                if pos == self.n {
                    self.contains_null_key = true;
                }
                self.key[pos] = k;
                self.value[pos] = v;
                let before = self.size;
                self.size += 1;
                if before >= self.max_fill {
                    self.rehash(array_size(self.size + 1));
                }
            }
        }
    }

    /// `remove(k)`.
    pub(super) fn remove(&mut self, k: i64) {
        let Ok(pos) = self.find(k) else {
            return;
        };
        self.size -= 1;
        if pos == self.n {
            self.contains_null_key = false;
        } else {
            self.shift_keys(pos, None);
        }
        if self.n > self.min_n && self.size < self.max_fill / 4 && self.n > INITIAL_SIZE {
            self.rehash(self.n / 2);
        }
    }

    /// `shiftKeys(pos)`; the iterator's version also records the entries that wrap around
    /// behind its cursor.
    fn shift_keys(&mut self, mut pos: usize, mut wrapped: Option<&mut Vec<i64>>) {
        let mask = self.mask();
        loop {
            let last = pos;
            pos = (last + 1) & mask;
            let curr = loop {
                let curr = self.key[pos];
                if curr == 0 {
                    self.key[last] = 0;
                    return;
                }
                let slot = mix(curr) & mask;
                let stays = if last <= pos {
                    last >= slot || slot > pos
                } else {
                    last >= slot && slot > pos
                };
                if stays {
                    break curr;
                }
                pos = (pos + 1) & mask;
            };
            if pos < last
                && let Some(w) = wrapped.as_deref_mut()
            {
                w.push(curr);
            }
            self.key[last] = curr;
            self.value[last] = self.value[pos];
        }
    }

    /// `rehash(newN)`.
    fn rehash(&mut self, new_n: usize) {
        let mask = new_n - 1;
        let mut new_key = vec![0; new_n + 1];
        let mut new_value = vec![0; new_n + 1];
        let mut i = self.n;
        let real = self.size - usize::from(self.contains_null_key);
        for _ in 0..real {
            loop {
                i -= 1;
                if self.key[i] != 0 {
                    break;
                }
            }
            let mut pos = mix(self.key[i]) & mask;
            while new_key[pos] != 0 {
                pos = (pos + 1) & mask;
            }
            new_key[pos] = self.key[i];
            new_value[pos] = self.value[i];
        }
        new_value[new_n] = self.value[self.n];
        self.n = new_n;
        self.max_fill = max_fill(new_n);
        self.key = new_key;
        self.value = new_value;
    }

    /// One pass of `Long2LongMaps.fastIterator` over the entries: `f` may change the value and
    /// returns whether the entry stays (`false`: `iterator.remove()`).
    pub(super) fn retain(&mut self, mut f: impl FnMut(i64, &mut i64) -> bool) {
        let mut pos = self.n as isize;
        let mut c = self.size;
        let mut must_return_null_key = self.contains_null_key;
        let mut wrapped: Vec<i64> = Vec::new();
        while c != 0 {
            c -= 1;
            // `nextEntry`: the slot, and whether it is one of the wrapped entries.
            let (slot, from_wrapped) = if must_return_null_key {
                must_return_null_key = false;
                (self.n, false)
            } else {
                loop {
                    pos -= 1;
                    if pos < 0 {
                        let k = wrapped[(-pos - 1) as usize];
                        let mask = self.mask();
                        let mut p = mix(k) & mask;
                        while self.key[p] != k {
                            p = (p + 1) & mask;
                        }
                        break (p, true);
                    }
                    if self.key[pos as usize] != 0 {
                        break (pos as usize, false);
                    }
                }
            };
            let k = if slot == self.n { 0 } else { self.key[slot] };
            let mut v = self.value[slot];
            let keep = f(k, &mut v);
            self.value[slot] = v;
            if keep {
                continue;
            }
            // `remove()`.
            if from_wrapped {
                self.remove(k);
                continue;
            }
            if slot == self.n {
                self.contains_null_key = false;
            } else {
                self.shift_keys(slot, Some(&mut wrapped));
            }
            self.size -= 1;
        }
    }

    pub(super) fn shrink_to_fit(&mut self) {
        self.key.shrink_to_fit();
        self.value.shrink_to_fit();
    }
}

/// `java.util.PriorityQueue` of naturally ordered elements: Java's sift order, so equal
/// elements come out in the same order as there.
#[derive(Debug)]
pub(super) struct PriorityQueue<T> {
    queue: Vec<T>,
}

impl<T: Ord> PriorityQueue<T> {
    pub(super) fn new() -> Self {
        Self { queue: Vec::new() }
    }

    /// `offer(e)`: `siftUp`.
    pub(super) fn offer(&mut self, e: T) {
        let mut k = self.queue.len();
        self.queue.push(e);
        while k > 0 {
            let parent = (k - 1) >> 1;
            if self.queue[k] >= self.queue[parent] {
                break;
            }
            self.queue.swap(k, parent);
            k = parent;
        }
    }

    pub(super) fn peek(&self) -> Option<&T> {
        self.queue.first()
    }

    /// `poll()`: the last element sifts down from the root.
    pub(super) fn poll(&mut self) -> Option<T> {
        let last = self.queue.pop()?;
        if self.queue.is_empty() {
            return Some(last);
        }
        let result = std::mem::replace(&mut self.queue[0], last);
        let n = self.queue.len();
        let half = n >> 1;
        let mut k = 0;
        while k < half {
            let mut child = 2 * k + 1;
            let right = child + 1;
            if right < n && self.queue[child] > self.queue[right] {
                child = right;
            }
            if self.queue[k] <= self.queue[child] {
                break;
            }
            self.queue.swap(k, child);
            k = child;
        }
        Some(result)
    }

    /// The elements in array order (Java's iteration order).
    pub(super) fn into_vec(self) -> Vec<T> {
        self.queue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_survives_removal_while_iterating_and_resizes() {
        let mut map = Long2LongMap::default();
        for k in 0..200_i64 {
            map.put(k * 0x1_0000_0001, k);
        }
        let mut seen = 0;
        map.retain(|k, v| {
            seen += 1;
            *v += 1;
            k % 3 != 0
        });
        assert_eq!(seen, 200);
        let mut left = Vec::new();
        map.retain(|k, v| {
            assert_eq!(*v, k / 0x1_0000_0001 + 1);
            left.push(k);
            true
        });
        assert_eq!(left.len(), 200 - 67);
        for k in left {
            map.remove(k);
        }
        assert_eq!(map.size, 0);
    }
}
