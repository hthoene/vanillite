//! The simulation phases of the world tick (`ServerLevel.tick`): game time, scheduled block and
//! fluid ticks, post-processing of new chunks, random ticks and block events.

use std::collections::VecDeque;

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, ChunkPos};
use vanillite_worldgen::behaviour as shared;

use super::tiles::{PHASE_PART, Phases};
use super::{BlockEvent, Level, update};
use crate::block::{self, fluid};
use vanillite_data::block::BlockState;

impl Level {
    /// `tickTime`: the game time advances.
    pub fn tick_time(&mut self) {
        self.handling_tick = true;
        self.game_time += 1;
        self.chunks.set_game_time(self.game_time);
        self.entities.game_time = self.game_time;
    }

    /// Sets the game time: all worlds run on the overworld's (`DerivedLevelData`).
    pub fn set_game_time(&mut self, game_time: i64) {
        self.game_time = game_time;
        self.chunks.set_game_time(game_time);
        self.entities.game_time = game_time;
    }

    /// `blockTicks.tick(...)` and `fluidTicks.tick(...)`: the scheduled ticks due now, block
    /// ticks first.
    pub fn run_scheduled_ticks(&mut self) {
        self.chunks
            .collect_block_ticks(self.settings.max_block_ticks);
        while let Some((pos, kind)) = self.chunks.next_block_tick() {
            // `tickBlock`: only if the block is still there.
            let state = self.block(pos);
            if state.block().0 == kind
                && let Some(tick) = self.behaviours.get(state.block()).tick
            {
                tick(self, state, pos);
            }
        }
        self.chunks
            .collect_fluid_ticks(self.settings.max_fluid_ticks);
        while let Some((pos, kind)) = self.chunks.next_fluid_tick() {
            // `tickFluid`: only if the fluid is still there.
            let state = self.block(pos);
            let fluid = state.fluid_state();
            if !fluid.is_empty() && fluid.info().fluid == kind {
                fluid::tick(self, pos, state, fluid);
            }
        }
    }

    /// `LevelChunk.postProcessGeneration` of chunks that became live and whose neighbours are
    /// live: fluids generation left behind tick, other marked blocks update their shape.
    pub fn post_process(&mut self) {
        let mut ready = Vec::new();
        self.chunks.take_post_processing(&mut ready);
        for (_, marks) in ready {
            for pos in marks {
                let state = self.block(pos);
                let fluid = state.fluid_state();
                if !fluid.is_empty() {
                    fluid::tick(self, pos, state, fluid);
                }
                if block::is_liquid(state) {
                    if let Some(tick) = self.behaviours.get(state.block()).tick {
                        tick(self, state, pos);
                    }
                } else {
                    let new = shared::update_from_neighbour_shapes(self, state, pos);
                    if new != state {
                        self.set_block(
                            pos,
                            new,
                            update::INVISIBLE
                                | update::KNOWN_SHAPE
                                | update::SKIP_BLOCK_ENTITY_SIDEEFFECTS,
                        );
                    }
                }
            }
        }
    }

    /// `ServerChunkCache.tickChunks` as far as blocks go: random ticks of every chunk in
    /// simulation distance (`ServerLevel.tickChunk`).
    pub fn tick_chunks(&mut self, scratch: &mut Vec<ChunkPos>, phases: &Phases<'_>) {
        let speed = self.settings.random_tick_speed;
        scratch.clear();
        self.random_ticking_chunks(scratch, phases);
        for (i, &chunk) in scratch.iter().enumerate() {
            // The chunks ahead start loading into the cache while this one ticks.
            for (ahead, stage) in [(4, 0), (3, 1), (2, 2)] {
                if let Some(&next) = scratch.get(i + ahead) {
                    self.chunks.prefetch(next, stage);
                }
            }
            self.tick_chunk(chunk, speed, scratch.get(i + 1).copied());
        }
    }

    /// The chunks that get random ticks this tick, in Vanilla's order
    /// ([`Chunks::random_ticking_chunks`](vanillite_world::chunks::Chunks::random_ticking_chunks)):
    /// the ticking checks run as a parallel phase.
    fn random_ticking_chunks(&self, out: &mut Vec<ChunkPos>, phases: &Phases<'_>) {
        if phases.threads <= 1 {
            self.chunks.random_ticking_chunks(out);
            return;
        }
        let mut candidates = Vec::new();
        self.chunks.random_ticking_candidates(&mut candidates);
        out.extend(phases.map(&candidates, PHASE_PART, |part, out| {
            out.extend(
                part.iter()
                    .copied()
                    .filter(|&c| self.chunks.is_random_ticking(c)),
            );
        }));
    }

    /// `ServerLevel.tickChunk`: precipitation checks and `randomTickSpeed` random ticks per
    /// section that has randomly ticking blocks or fluids. `next` is the chunk that ticks
    /// next: the blocks its random ticks will probably pick start loading into the cache.
    pub(super) fn tick_chunk(&mut self, chunk: ChunkPos, speed: i32, next: Option<ChunkPos>) {
        let min_x = chunk.x * 16;
        let min_z = chunk.z * 16;
        for _ in 0..speed {
            if self.random.next_int_bounded(48) == 0 {
                let pos = self.block_random_pos(min_x, 0, min_z, 15);
                block::weather::tick_precipitation(self, pos);
            }
        }
        if speed <= 0 {
            return;
        }
        if let Some(next) = next {
            self.prefetch_picks(chunk, next, speed);
        }
        let min_section = self.min_y >> 4;
        // The section being ticked and the picks it has left. The next randomly ticking
        // section is found when the loop gets there (ticks of lower sections may change it);
        // each pick reads the section in place (`section.getBlockState`) until one ticks, and
        // the chunk is looked up again after a tick, which may change it.
        let mut next = 0;
        let mut index = 0;
        let mut left = 0;
        loop {
            let mut hit = None;
            let rand_value = &mut self.rand_value;
            let Some(c) = self.chunks.chunk(chunk) else {
                // Not loaded any more: air, nothing ticks; the picks are still drawn.
                let min_y = (min_section + index as i32) * 16;
                for _ in 0..left {
                    random_pos(rand_value, min_x, min_y, min_z, 15);
                }
                break;
            };
            let sections = c.chunk.sections();
            if left == 0 {
                let Some(found) = c.chunk.next_randomly_ticking(next) else {
                    break;
                };
                index = found;
                next = index + 1;
                left = speed;
            }
            let section = &sections[index];
            let min_y = (min_section + index as i32) * 16;
            while left > 0 {
                left -= 1;
                let pos = random_pos(rand_value, min_x, min_y, min_z, 15);
                #[allow(clippy::cast_sign_loss)]
                let state = BlockState(section.block(
                    (pos.x & 15) as u32,
                    (pos.y & 15) as u32,
                    (pos.z & 15) as u32,
                ));
                if state.random_ticks() || state.fluid_state().info().random_ticks {
                    hit = Some((pos, state));
                    break;
                }
            }
            if let Some((pos, state)) = hit {
                self.random_tick_state(pos, state);
            }
        }
    }

    /// Prefetch hint for the random ticks of `next` after `chunk`: the blocks they pick if the
    /// randomly ticking sections of both stay as they are and `next` draws no precipitation
    /// position, or one (each section's picks plus one). Only reads; a wrong guess only costs
    /// the cache lines.
    fn prefetch_picks(&self, chunk: ChunkPos, next: ChunkPos, speed: i32) {
        if speed > MAX_PREFETCH_SPEED {
            return;
        }
        let (Some(here), Some(there)) = (self.chunks.chunk(chunk), self.chunks.chunk(next)) else {
            return;
        };
        let mut value = self.rand_value;
        let mut index = 0;
        while let Some(found) = here.chunk.next_randomly_ticking(index) {
            for _ in 0..speed {
                random_pos(&mut value, 0, 0, 0, 0);
            }
            index = found + 1;
        }
        let sections = there.chunk.sections();
        let mut index = 0;
        while let Some(found) = there.chunk.next_randomly_ticking(index) {
            let section = &sections[found];
            let mut pick = value;
            for _ in 0..=speed {
                let pos = random_pos(&mut pick, 0, 0, 0, 15);
                #[allow(clippy::cast_sign_loss)]
                section.prefetch_block(pos.x as u32, pos.y as u32, pos.z as u32);
            }
            for _ in 0..speed {
                random_pos(&mut value, 0, 0, 0, 0);
            }
            index = found + 1;
        }
    }

    /// The random tick of the block and of the fluid at `pos`, if they tick randomly.
    #[inline]
    pub(super) fn random_tick_at(&mut self, pos: BlockPos) {
        let state = self.block(pos);
        self.random_tick_state(pos, state);
    }

    /// [`Level::random_tick_at`] with the block state at `pos` already read.
    #[inline]
    fn random_tick_state(&mut self, pos: BlockPos, state: BlockState) {
        if state.random_ticks()
            && let Some(random_tick) = self.behaviours.get(state.block()).random_tick
        {
            random_tick(self, state, pos);
        }
        let fluid = state.fluid_state();
        if fluid.info().random_ticks {
            fluid::random_tick(self, pos, fluid);
        }
    }

    /// `getBlockRandomPos(x, y, z, mask)`: Vanilla's cheap LCG over `randValue`.
    pub fn block_random_pos(&mut self, x: i32, y: i32, z: i32, mask: i32) -> BlockPos {
        random_pos(&mut self.rand_value, x, y, z, mask)
    }

    /// `runBlockEvents`: every queued block event whose chunk ticks runs (events added meanwhile
    /// too); events in chunks that do not tick wait for a later tick.
    pub fn run_block_events(&mut self) {
        let mut queue: VecDeque<BlockEvent> = std::mem::take(&mut self.block_events).into();
        let mut reschedule = Vec::new();
        loop {
            // Events queued by running events run in the same loop.
            queue.extend(self.block_events.drain(..));
            let Some(event) = queue.pop_front() else {
                break;
            };
            // `removeFirst` of the linked set: the event may be queued again from now on.
            self.block_event_set.remove(&event);
            if !self.is_block_ticking(event.pos.chunk()) {
                reschedule.push(event);
                continue;
            }
            let state = self.block(event.pos);
            if state.block() != event.block {
                continue;
            }
            let Some(trigger) = self.behaviours.get(state.block()).trigger_event else {
                continue;
            };
            if trigger(self, state, event.pos, event.a, event.b) {
                self.output.block_events.push(event);
            }
        }
        // `blockEvents.addAll(blockEventsToReschedule)`.
        for event in reschedule {
            self.block_event(event.pos, event.block, event.a, event.b);
        }
        self.handling_tick = false;
    }
}

/// Above this `randomTickSpeed`, the picks of the next chunk are not prefetched.
const MAX_PREFETCH_SPEED: i32 = 16;

/// `getBlockRandomPos` over a `randValue`: the level's, or a tile's in tile region mode.
#[inline]
pub(super) fn random_pos(rand_value: &mut i32, x: i32, y: i32, z: i32, mask: i32) -> BlockPos {
    *rand_value = rand_value.wrapping_mul(3).wrapping_add(1_013_904_223);
    let value = *rand_value >> 2;
    BlockPos::new(
        x + (value & mask),
        y + (value >> 16 & mask),
        z + (value >> 8 & mask),
    )
}
