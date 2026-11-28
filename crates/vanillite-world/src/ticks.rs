//! Scheduled block and fluid ticks (Vanilla's `LevelTicks`, `LevelChunkTicks` and
//! `ScheduledTick`).
//!
//! Every full chunk has a container of its ticks, ordered by trigger time, priority and the
//! sub-tick counter (Vanilla's `DRAIN_ORDER`) and unique per type and position. Once per world
//! tick [`LevelTicks::collect`] drains the due ticks of all ticking chunks in Vanilla's order (at
//! most a budget per tick); the world then runs them one by one ([`LevelTicks::next_to_run`])
//! and new ticks scheduled meanwhile wait for a later tick. The type of a tick is a block id for
//! block ticks and a fluid id for fluid ticks.

mod java;

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};

use vanillite_base::hash::{FxHashMap, FxHashSet};
use vanillite_base::{BlockPos, ChunkPos};
use vanillite_chunk::slots::SavedTick;

/// `TickPriority`: lower values run first within a game tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TickPriority(pub i8);

impl TickPriority {
    /// `EXTREMELY_HIGH`.
    pub const EXTREMELY_HIGH: TickPriority = TickPriority(-3);
    /// `VERY_HIGH`.
    pub const VERY_HIGH: TickPriority = TickPriority(-2);
    /// `HIGH`.
    pub const HIGH: TickPriority = TickPriority(-1);
    /// `NORMAL`.
    pub const NORMAL: TickPriority = TickPriority(0);
    /// `LOW`.
    pub const LOW: TickPriority = TickPriority(1);
    /// `VERY_LOW`.
    pub const VERY_LOW: TickPriority = TickPriority(2);
    /// `EXTREMELY_LOW`.
    pub const EXTREMELY_LOW: TickPriority = TickPriority(3);
}

/// A scheduled tick (`ScheduledTick`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledTick {
    /// Block id or fluid id.
    pub kind: u16,
    /// Position.
    pub pos: BlockPos,
    /// Game time at which it runs.
    pub trigger: i64,
    /// Priority within the game tick.
    pub priority: TickPriority,
    /// The level's sub-tick counter when it was scheduled: the order among equal priorities.
    pub sub_tick: i64,
}

impl ScheduledTick {
    /// `DRAIN_ORDER`: trigger time, priority, sub-tick.
    fn drain_key(&self) -> (i64, TickPriority, i64) {
        (self.trigger, self.priority, self.sub_tick)
    }

    /// `INTRA_TICK_DRAIN_ORDER`: priority, sub-tick.
    fn intra_tick_key(&self) -> (TickPriority, i64) {
        (self.priority, self.sub_tick)
    }
}

/// A tick in a container's queue, ordered by `DRAIN_ORDER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Queued(ScheduledTick);

impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.drain_key().cmp(&other.0.drain_key())
    }
}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The ticks of one chunk (`LevelChunkTicks`).
#[derive(Debug, Default)]
pub struct ChunkTicks {
    queue: BinaryHeap<Reverse<Queued>>,
    unique: FxHashSet<(u16, BlockPos)>,
}

impl ChunkTicks {
    /// The next tick.
    fn peek(&self) -> Option<&ScheduledTick> {
        self.queue.peek().map(|Reverse(Queued(tick))| tick)
    }

    fn poll(&mut self) -> Option<ScheduledTick> {
        let Reverse(Queued(tick)) = self.queue.pop()?;
        self.unique.remove(&(tick.kind, tick.pos));
        Some(tick)
    }

    /// Adds a tick unless one of the same type is scheduled at its position.
    fn schedule(&mut self, tick: ScheduledTick) -> bool {
        if self.unique.insert((tick.kind, tick.pos)) {
            self.queue.push(Reverse(Queued(tick)));
            true
        } else {
            false
        }
    }

    /// Number of ticks.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether there are no ticks.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// `pack(gameTime)`: the ticks in sub-tick order with delays relative to `game_time`.
    fn pack(&self, game_time: i64) -> Vec<SavedTick> {
        let mut ticks: Vec<&ScheduledTick> =
            self.queue.iter().map(|Reverse(Queued(t))| t).collect();
        ticks.sort_unstable_by_key(|t| t.sub_tick);
        ticks
            .into_iter()
            .map(|t| SavedTick {
                kind: t.kind,
                pos: t.pos,
                delay: i32::try_from(t.trigger - game_time).unwrap_or(i32::MAX),
                priority: t.priority.0,
            })
            .collect()
    }
}

/// A container in the drain queue, ordered by its next tick's `INTRA_TICK_DRAIN_ORDER`.
#[derive(Debug, PartialEq, Eq)]
struct Candidate {
    key: (TickPriority, i64),
    chunk: ChunkPos,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The scheduled ticks of one type (block or fluid) in a world (`LevelTicks`).
#[derive(Debug, Default)]
pub struct LevelTicks {
    containers: FxHashMap<ChunkPos, ChunkTicks>,
    /// The trigger time of every container's next tick (`nextTickForContainer`), by packed
    /// chunk position: its iteration order decides the order of tying containers.
    next_tick: java::Long2LongMap,
    /// Ticks collected for this game tick, in run order.
    to_run: VecDeque<ScheduledTick>,
    /// `toRunThisTickSet`, built on demand.
    to_run_set: FxHashSet<(u16, BlockPos)>,
}

impl LevelTicks {
    /// Gives back the room of ticks that ran and of containers that left.
    pub fn shrink_to_fit(&mut self) {
        self.containers.shrink_to_fit();
        self.next_tick.shrink_to_fit();
        self.to_run.shrink_to_fit();
        self.to_run_set.shrink_to_fit();
    }

    /// Adds the container of a chunk that became live, with the ticks it was stored with
    /// (`LevelChunkTicks.unpack`: stored ticks get sub-tick values below every new tick, in
    /// stored order). `sub_tick` is the level's counter.
    pub fn add_container(&mut self, chunk: ChunkPos, saved: &[SavedTick], game_time: i64) {
        let mut container = ChunkTicks::default();
        let first = -i64::try_from(saved.len()).unwrap_or(i64::MAX);
        for (sub_tick, tick) in (first..).zip(saved) {
            container.schedule(ScheduledTick {
                kind: tick.kind,
                pos: tick.pos,
                trigger: game_time + i64::from(tick.delay),
                priority: TickPriority(tick.priority),
                sub_tick,
            });
        }
        if let Some(next) = container.peek() {
            self.next_tick.put(chunk.as_long(), next.trigger);
        }
        self.containers.insert(chunk, container);
    }

    /// Removes the container of a chunk that unloads and returns its ticks relative to
    /// `game_time`.
    pub fn remove_container(&mut self, chunk: ChunkPos, game_time: i64) -> Vec<SavedTick> {
        self.next_tick.remove(chunk.as_long());
        self.containers
            .remove(&chunk)
            .map(|c| c.pack(game_time))
            .unwrap_or_default()
    }

    /// The ticks of a chunk relative to `game_time`, for saving.
    pub fn pack(&self, chunk: ChunkPos, game_time: i64) -> Vec<SavedTick> {
        self.containers
            .get(&chunk)
            .map(|c| c.pack(game_time))
            .unwrap_or_default()
    }

    /// `schedule(tick)`: ignored if its chunk has no container (not live) or a tick of the same
    /// type is scheduled at the position. Returns whether it was added.
    pub fn schedule(&mut self, tick: ScheduledTick) -> bool {
        let chunk = tick.pos.chunk();
        let Some(container) = self.containers.get_mut(&chunk) else {
            return false;
        };
        if !container.schedule(tick) {
            return false;
        }
        // `chunkScheduleUpdater`: the new tick is the container's next one.
        if container.peek() == Some(&tick) {
            self.next_tick.put(chunk.as_long(), tick.trigger);
        }
        true
    }

    /// `hasScheduledTick(pos, type)`.
    pub fn has_scheduled(&self, pos: BlockPos, kind: u16) -> bool {
        self.containers
            .get(&pos.chunk())
            .is_some_and(|c| c.unique.contains(&(kind, pos)))
    }

    /// `willTickThisTick(pos, type)`: the tick is collected and has not run yet.
    pub fn will_tick_this_tick(&mut self, pos: BlockPos, kind: u16) -> bool {
        if self.to_run_set.is_empty() && !self.to_run.is_empty() {
            self.to_run_set
                .extend(self.to_run.iter().map(|t| (t.kind, t.pos)));
        }
        self.to_run_set.contains(&(kind, pos))
    }

    /// `clearArea(area)`: drops the ticks inside `min..=max` (scheduled and collected).
    /// Returns the chunks whose ticks changed.
    pub fn clear_area(&mut self, min: BlockPos, max: BlockPos) -> Vec<ChunkPos> {
        let inside = |p: BlockPos| {
            (min.x..=max.x).contains(&p.x)
                && (min.y..=max.y).contains(&p.y)
                && (min.z..=max.z).contains(&p.z)
        };
        let mut changed = Vec::new();
        for cz in (min.z >> 4)..=(max.z >> 4) {
            for cx in (min.x >> 4)..=(max.x >> 4) {
                let chunk = ChunkPos::new(cx, cz);
                let Some(container) = self.containers.get_mut(&chunk) else {
                    continue;
                };
                let before = container.queue.len();
                let kept: Vec<Reverse<Queued>> = std::mem::take(&mut container.queue)
                    .into_iter()
                    .filter(|Reverse(Queued(t))| !inside(t.pos))
                    .collect();
                container.queue = kept.into_iter().collect();
                if container.queue.len() == before {
                    continue;
                }
                container.unique.retain(|(_, p)| !inside(*p));
                changed.push(chunk);
                match container.peek() {
                    Some(next) => {
                        self.next_tick.put(chunk.as_long(), next.trigger);
                    }
                    None => {
                        self.next_tick.remove(chunk.as_long());
                    }
                }
            }
        }
        self.to_run.retain(|t| !inside(t.pos));
        self.to_run_set.retain(|(_, p)| !inside(*p));
        changed
    }

    /// The ticks scheduled in a chunk, unordered (observation, e.g. parity traces).
    pub fn ticks_in(&self, chunk: ChunkPos) -> impl Iterator<Item = &ScheduledTick> {
        self.containers
            .get(&chunk)
            .into_iter()
            .flat_map(|c| c.queue.iter().map(|Reverse(Queued(t))| t))
    }

    /// `DRAIN_ORDER` key of a tick (trigger time, priority, sub-tick).
    pub fn drain_order(tick: &ScheduledTick) -> (i64, TickPriority, i64) {
        tick.drain_key()
    }

    /// Number of scheduled ticks.
    pub fn count(&self) -> usize {
        self.containers.values().map(ChunkTicks::len).sum()
    }

    /// Collects the ticks due at `now` of chunks where `ticking` holds, at most `max`, in
    /// Vanilla's order (`collectTicks`). Calls `drained` for every chunk that lost ticks (its
    /// stored data changed). Run them with [`Self::next_to_run`].
    pub fn collect(
        &mut self,
        now: i64,
        max: usize,
        ticking: impl Fn(ChunkPos) -> bool,
        mut drained: impl FnMut(ChunkPos),
    ) {
        self.to_run.clear();
        self.to_run_set.clear();
        // `sortContainersToTick`.
        let mut heap: java::PriorityQueue<Candidate> = java::PriorityQueue::new();
        let containers = &self.containers;
        self.next_tick.retain(|packed, next| {
            let chunk = ChunkPos::from_long(packed);
            if *next > now {
                return true;
            }
            let Some(peek) = containers.get(&chunk).and_then(ChunkTicks::peek) else {
                return false;
            };
            if peek.trigger > now {
                *next = peek.trigger;
                true
            } else if ticking(chunk) {
                heap.offer(Candidate {
                    key: peek.intra_tick_key(),
                    chunk,
                });
                false
            } else {
                true
            }
        });
        // `drainContainers`.
        while self.to_run.len() < max {
            let Some(top) = heap.poll() else {
                break;
            };
            let Some(container) = self.containers.get_mut(&top.chunk) else {
                continue;
            };
            drained(top.chunk);
            if let Some(tick) = container.poll() {
                self.to_run.push_back(tick);
            }
            // `drainFromCurrentContainer`.
            let next_best = heap.peek().map(|c| c.key);
            while self.to_run.len() < max {
                let Some(next) = container.peek() else {
                    break;
                };
                if next.trigger > now || next_best.is_some_and(|best| next.intra_tick_key() > best)
                {
                    break;
                }
                if let Some(tick) = container.poll() {
                    self.to_run.push_back(tick);
                }
            }
            if let Some(next) = container.peek() {
                if next.trigger <= now && self.to_run.len() < max {
                    heap.offer(Candidate {
                        key: next.intra_tick_key(),
                        chunk: top.chunk,
                    });
                } else {
                    self.next_tick.put(top.chunk.as_long(), next.trigger);
                }
            }
        }
        // `rescheduleLeftoverContainers`.
        for left in heap.into_vec() {
            if let Some(next) = self.containers.get(&left.chunk).and_then(ChunkTicks::peek) {
                self.next_tick.put(left.chunk.as_long(), next.trigger);
            }
        }
    }

    /// The next collected tick to run: its position and type.
    pub fn next_to_run(&mut self) -> Option<(BlockPos, u16)> {
        let tick = self.to_run.pop_front()?;
        if !self.to_run_set.is_empty() {
            self.to_run_set.remove(&(tick.kind, tick.pos));
        }
        Some((tick.pos, tick.kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(kind: u16, x: i32, trigger: i64, priority: i8, sub_tick: i64) -> ScheduledTick {
        ScheduledTick {
            kind,
            pos: BlockPos::new(x, 0, 0),
            trigger,
            priority: TickPriority(priority),
            sub_tick,
        }
    }

    fn run(ticks: &mut LevelTicks, now: i64, max: usize) -> Vec<i32> {
        ticks.collect(now, max, |_| true, |_| {});
        std::iter::from_fn(|| ticks.next_to_run())
            .map(|(pos, _)| pos.x)
            .collect()
    }

    #[test]
    fn drains_by_priority_then_schedule_order_across_chunks() {
        let mut ticks = LevelTicks::default();
        ticks.add_container(ChunkPos::new(0, 0), &[], 0);
        ticks.add_container(ChunkPos::new(1, 0), &[], 0);
        assert!(ticks.schedule(tick(1, 1, 5, 0, 1)));
        assert!(ticks.schedule(tick(1, 17, 5, 0, 2)));
        assert!(ticks.schedule(tick(1, 2, 5, 0, 3)));
        assert!(ticks.schedule(tick(1, 18, 5, -1, 4)));
        assert!(ticks.schedule(tick(1, 3, 6, -3, 5)));
        // Same type at the same position: ignored.
        assert!(!ticks.schedule(tick(1, 2, 5, 0, 6)));
        assert!(ticks.has_scheduled(BlockPos::new(2, 0, 0), 1));
        assert_eq!(run(&mut ticks, 4, 100), Vec::<i32>::new());
        assert_eq!(run(&mut ticks, 5, 100), [18, 1, 17, 2]);
        assert!(!ticks.has_scheduled(BlockPos::new(2, 0, 0), 1));
        assert_eq!(run(&mut ticks, 6, 100), [3]);
    }

    #[test]
    fn budget_leaves_the_rest_for_later() {
        let mut ticks = LevelTicks::default();
        ticks.add_container(ChunkPos::new(0, 0), &[], 0);
        for i in 0..5 {
            ticks.schedule(tick(1, i, 1, 0, i64::from(i)));
        }
        assert_eq!(run(&mut ticks, 1, 2), [0, 1]);
        assert_eq!(run(&mut ticks, 2, 10), [2, 3, 4]);
    }

    #[test]
    fn saved_ticks_round_trip_and_run_before_new_ones() {
        let mut ticks = LevelTicks::default();
        let chunk = ChunkPos::new(0, 0);
        ticks.add_container(chunk, &[], 100);
        ticks.schedule(tick(7, 4, 103, 1, 10));
        ticks.schedule(tick(7, 5, 101, 0, 11));
        let saved = ticks.remove_container(chunk, 100);
        assert_eq!(saved.iter().map(|t| t.delay).collect::<Vec<_>>(), [3, 1]);
        ticks.add_container(chunk, &saved, 50);
        ticks.schedule(tick(7, 6, 53, 1, 0));
        assert_eq!(run(&mut ticks, 53, 10), [5, 4, 6]);
    }
}
