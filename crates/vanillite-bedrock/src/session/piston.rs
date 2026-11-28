//! Pistons. The server moves blocks as `moving_piston` blocks it does not send: the Java client
//! works the move out from `block_event` and its own world. Bedrock draws the arm from the
//! piston's `PistonArm` block actor and moved blocks from `MovingBlock` actors, so the session
//! animates both over the two ticks of a move (the approach of Geyser's `PistonBlockEntity`, MIT,
//! Copyright (c) 2019-2026 GeyserMC).
//!
//! The moved blocks come from a small memory of Java block states: the blocks in front of the
//! pistons of each chunk and recent block updates. Only the row in front of the piston is
//! animated; blocks glued on sideways by slime or honey, and blocks the memory does not know,
//! jump to their place when the server sends the end of the move.

use rustc_hash::FxHashMap;
use vanillite_data::block::{BlockState, PushReaction, flags};

use super::Session;
use crate::nbt::Compound;
use crate::packets::{self as bp, id};

/// Most remembered block states (a full memory is cleared; misses only cost the animation).
const MEMORY_MAX: usize = 16_384;

/// Most blocks a piston moves (`PistonStructureResolver.MAX_PUSH_DEPTH`).
const PUSH_LIMIT: usize = 12;

/// Java `block_event` types of pistons.
const EXTEND: u8 = 0;
const CONTRACT: u8 = 1;

/// A piston moving on the client.
#[derive(Debug)]
struct Moving {
    pos: [i32; 3],
    sticky: bool,
    extending: bool,
    progress: f32,
    last: f32,
    /// Start positions of the moved blocks, flattened.
    attached: Vec<i32>,
}

/// The pistons of a session.
#[derive(Debug, Default)]
pub(super) struct Pistons {
    /// Java states of the blocks in front of pistons and of recent block updates.
    memory: FxHashMap<[i32; 3], u32>,
    active: Vec<Moving>,
}

impl Pistons {
    fn remember(&mut self, pos: [i32; 3], state: u32) {
        if self.memory.len() >= MEMORY_MAX && !self.memory.contains_key(&pos) {
            self.memory.clear();
        }
        self.memory.insert(pos, state);
    }

    /// Forgets everything (another dimension).
    pub(super) fn clear(&mut self) {
        self.memory.clear();
        self.active.clear();
    }

    /// Blocks in front of a chunk's pistons ([`crate::chunk::translate`]).
    pub(super) fn remember_chunk(&mut self, blocks: &[([i32; 3], u32)]) {
        for &(pos, state) in blocks {
            self.remember(pos, state);
        }
    }

    fn animating(&self, pos: [i32; 3]) -> bool {
        self.active.iter().any(|m| m.pos == pos)
    }
}

/// Whether a Java state is `moving_piston`.
fn is_moving_piston(state: u32) -> bool {
    u16::try_from(state).is_ok_and(|s| {
        let s = BlockState(s);
        s.is_valid() && s.block().name() == "minecraft:moving_piston"
    })
}

fn add(a: [i32; 3], b: [i32; 3]) -> [i32; 3] {
    [
        a[0].wrapping_add(b[0]),
        a[1].wrapping_add(b[1]),
        a[2].wrapping_add(b[2]),
    ]
}

fn sub(a: [i32; 3], b: [i32; 3]) -> [i32; 3] {
    [
        a[0].wrapping_sub(b[0]),
        a[1].wrapping_sub(b[1]),
        a[2].wrapping_sub(b[2]),
    ]
}

fn piston_arm(m: &Moving, attached: &[i32]) -> Vec<u8> {
    // 0 retracted, 1 extending, 2 extended, 3 retracting.
    let state = if m.extending {
        if m.progress >= 1.0 && m.last >= 1.0 {
            2
        } else {
            1
        }
    } else if m.progress <= 0.0 && m.last <= 0.0 {
        0
    } else {
        3
    };
    let [x, y, z] = m.pos;
    let mut c = Compound::default();
    c.string("id", "PistonArm")
        .ints("AttachedBlocks", attached)
        .float("Progress", m.progress)
        .float("LastProgress", m.last)
        .byte("NewState", state)
        .byte("State", state)
        .byte("Sticky", u8::from(m.sticky))
        .int("x", x)
        .int("y", y)
        .int("z", z)
        .byte("isMovable", 0);
    bp::block_actor_data(m.pos, &c.root())
}

/// Whether a piston moves a block of this state (`PistonBaseBlock.isPushable`, without the
/// world height and border checks): `pull` for a sticky piston pulling it.
fn movable(state: u32, pull: bool) -> bool {
    let Ok(s) = u16::try_from(state) else {
        return false;
    };
    let s = BlockState(s);
    if !s.is_valid() || s.misc().destroy_speed < 0.0 || s.has(flags::HAS_BLOCK_ENTITY) {
        return false;
    }
    if crate::blockentity::piston_facing(state).is_some() {
        return s.value("extended") != Some("true");
    }
    match s.misc().push_reaction {
        PushReaction::PushPull => true,
        PushReaction::Push => !pull,
        PushReaction::Immoveable | PushReaction::Popped => false,
    }
}

impl Pistons {
    /// The blocks a piston moves, start positions and states (`PistonStructureResolver` for a
    /// row of blocks; blocks glued on by slime or honey and unknown blocks are left out).
    fn resolve(
        &self,
        pos: [i32; 3],
        facing: [i32; 3],
        extending: bool,
        pulls: bool,
    ) -> Vec<([i32; 3], u32)> {
        let known = |p: [i32; 3]| self.memory.get(&p).copied();
        let mut out = Vec::new();
        if extending {
            let mut p = add(pos, facing);
            while let Some(state) = known(p) {
                let air = u16::try_from(state).is_ok_and(|s| BlockState(s).is_air());
                if air || !movable(state, false) {
                    break;
                }
                if out.len() >= PUSH_LIMIT {
                    // The piston would not move: Java sent the event, so the memory is stale.
                    return Vec::new();
                }
                out.push((p, state));
                p = add(p, facing);
            }
        } else if pulls {
            let p = add(add(pos, facing), facing);
            if let Some(state) = known(p)
                && !u16::try_from(state).is_ok_and(|s| BlockState(s).is_air())
                && movable(state, true)
            {
                out.push((p, state));
            }
        }
        out
    }
}

impl Session {
    /// A Java block update: `moving_piston` is not shown (the move is animated instead; Vanilla
    /// does not send it either). Returns whether the update is done.
    pub(super) fn piston_block_update(&mut self, pos: [i32; 3], state: u32) -> bool {
        if is_moving_piston(state) {
            return true;
        }
        self.pistons.remember(pos, state);
        false
    }

    /// Whether the session animates the piston at `pos` (its block actor is sent per tick).
    pub(super) fn piston_animating(&self, pos: [i32; 3]) -> bool {
        self.pistons.animating(pos)
    }

    /// A piston's `block_event` (the server moves the blocks without telling the client):
    /// start the arm and the moved blocks.
    pub(super) fn piston_event(&mut self, pos: [i32; 3], action: u8, direction: u8, sticky: bool) {
        let extending = action == EXTEND;
        let facing = crate::blockentity::direction_step(direction);
        // Extending pushes along the facing; a sticky piston contracting pulls a block back.
        let pulls = action == CONTRACT && sticky;
        let movement = if extending {
            facing
        } else {
            sub([0; 3], facing)
        };
        let moved = self.pistons.resolve(pos, facing, extending, pulls);
        let palette = self.version.palette;
        let blocks = palette.blocks();
        let states = palette.states();
        let mut attached = Vec::with_capacity(moved.len().saturating_mul(3));
        if let Some(moving_block) = states.moving_block {
            let ends: Vec<[i32; 3]> = moved.iter().map(|&(p, _)| add(p, movement)).collect();
            for &(origin, _) in &moved {
                // The start of one block is the end of the next in a row.
                if !ends.contains(&origin) {
                    self.send(id::UPDATE_BLOCK, &bp::update_block(origin, blocks.air, 0));
                    self.send(id::UPDATE_BLOCK, &bp::update_block(origin, blocks.air, 1));
                }
            }
            for &(origin, state) in &moved {
                let rid = u32::from(blocks.entry(state) & !crate::data::WATERLOGGED);
                let Some(block) = states.get(rid) else {
                    continue;
                };
                attached.extend_from_slice(&origin);
                let d = add(origin, movement);
                let [x, y, z] = d;
                let mut c = Compound::default();
                c.string("id", "MovingBlock")
                    .raw_compound("movingBlock", block)
                    .byte("expanding", u8::from(extending))
                    .int("pistonPosX", pos[0])
                    .int("pistonPosY", pos[1])
                    .int("pistonPosZ", pos[2])
                    .int("x", x)
                    .int("y", y)
                    .int("z", z)
                    .byte("isMovable", 1);
                self.send(id::UPDATE_BLOCK, &bp::update_block(d, moving_block, 0));
                self.send(id::BLOCK_ACTOR_DATA, &bp::block_actor_data(d, &c.root()));
            }
        }
        // A new event replaces a running move of the same piston.
        self.pistons.active.retain(|m| m.pos != pos);
        let start = if extending { 0.0 } else { 1.0 };
        let m = Moving {
            pos,
            sticky,
            extending,
            progress: start,
            last: start,
            attached,
        };
        let body = piston_arm(&m, &m.attached);
        self.send(id::BLOCK_ACTOR_DATA, &body);
        self.pistons.active.push(m);
    }

    /// One client tick: advance the moving pistons (half a block per tick, like Java).
    pub(super) fn piston_tick(&mut self) {
        let p = &mut self.pistons;
        if p.active.is_empty() {
            return;
        }
        let mut frames = Vec::with_capacity(p.active.len());
        p.active.retain_mut(|m| {
            m.last = m.progress;
            m.progress = if m.extending {
                (m.progress + 0.5).min(1.0)
            } else {
                (m.progress - 0.5).max(0.0)
            };
            let done = if m.extending {
                m.last >= 1.0
            } else {
                m.last <= 0.0
            };
            frames.push(piston_arm(m, if done { &[] } else { &m.attached }));
            !done
        });
        for f in frames {
            self.send(id::BLOCK_ACTOR_DATA, &f);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn state(name: &str) -> u32 {
        let id = vanillite_data::registry::BLOCK.id(name).unwrap();
        u32::from(vanillite_data::block::BlockId(id).default_state().0)
    }

    #[test]
    fn resolves_a_row_and_a_pulled_block() {
        let mut p = Pistons::default();
        let (planks, stone, air) = (
            state("minecraft:oak_planks"),
            state("minecraft:stone"),
            state("minecraft:air"),
        );
        p.remember_chunk(&[
            ([1, 0, 0], planks),
            ([2, 0, 0], stone),
            ([3, 0, 0], air),
            ([0, 0, 5], state("minecraft:obsidian")),
        ]);
        let east = [1, 0, 0];
        assert_eq!(
            p.resolve([0, 0, 0], east, true, false),
            vec![([1, 0, 0], planks), ([2, 0, 0], stone)]
        );
        // Contracting: a sticky piston pulls the block two in front back.
        assert_eq!(
            p.resolve([0, 0, 0], east, false, true),
            vec![([2, 0, 0], stone)]
        );
        assert!(p.resolve([0, 0, 0], east, false, false).is_empty());
        // Obsidian does not move.
        assert!(p.resolve([0, 0, 4], [0, 0, 1], true, false).is_empty());
    }
}
