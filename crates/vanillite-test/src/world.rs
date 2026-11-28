//! The observation API of a scenario world, and the flat stand-in world.
//!
//! [`ScenarioWorld`] is what the runner needs from a world: set blocks with and without
//! updates, step whole ticks and observe a region (blocks, pending ticks, entities). The world
//! runtime's `World::headless` implements it once it exists; until then [`FlatWorld`] stands in:
//! flat chunks on `vanillite-chunk` without any game mechanics, enough for the scenarios of what
//! exists today (chunk loading, block set and get).

use std::collections::HashMap;

use vanillite_base::{BlockPos, ChunkPos};
use vanillite_chunk::{Chunk, facts};
use vanillite_data::block::{BlockId, BlockState};

use crate::scenario::{EntitySpec, Hurt, WorldSpec};
use crate::trace::{EntityObs, TickObs};

/// How the world runs the parallel phases of a tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Execution {
    /// Compute pool jobs run inline in submission order (test mode, section 3.7).
    Serial,
    /// Compute pool jobs run on worker threads.
    Parallel,
}

/// A world a scenario runs in.
pub trait ScenarioWorld {
    /// Sets a block without neighbour or shape updates (Vanilla flags `UPDATE_CLIENTS |
    /// UPDATE_KNOWN_SHAPE`).
    fn set_block_raw(&mut self, pos: BlockPos, state: BlockState);
    /// Sets a block with neighbour and shape updates (Vanilla `Level#setBlock` flags 3).
    fn place_block(&mut self, pos: BlockPos, state: BlockState);
    /// The block state at `pos`.
    fn block(&self, pos: BlockPos) -> BlockState;
    /// Runs one full world tick.
    fn step(&mut self);
    /// Pending block ticks inside `min..=max` in drain order (trigger tick, priority, sub-tick
    /// order), delays relative to the current game time.
    fn block_ticks(&self, min: BlockPos, max: BlockPos, out: &mut Vec<TickObs>);
    /// Pending fluid ticks inside `min..=max`, like [`ScenarioWorld::block_ticks`].
    fn fluid_ticks(&self, min: BlockPos, max: BlockPos, out: &mut Vec<TickObs>);
    /// Entities whose block position is inside `min..=max`.
    fn entities(&self, min: BlockPos, max: BlockPos, out: &mut Vec<EntityObs>);

    /// Makes `dimension` the one the block operations and observations work in (scenarios
    /// across dimensions); worlds of the overworld only refuse the others.
    fn select_dimension(&mut self, dimension: &str) -> Result<(), String> {
        if dimension == crate::scenario::OVERWORLD {
            Ok(())
        } else {
            Err(format!("this world has no dimension {dimension}"))
        }
    }

    /// Whether `pos` is in an accessible chunk; other positions are traced as
    /// [`crate::scenario::UNLOADED`].
    fn is_loaded(&self, _pos: BlockPos) -> bool {
        true
    }

    /// Adds (`forced`) or removes the forced tickets of chunks, then runs the chunk system
    /// without a game tick until it is settled.
    fn set_forced(&mut self, _chunks: &[ChunkPos], _forced: bool) -> Result<(), String> {
        Err("this world has no chunk tickets".to_owned())
    }

    /// Saves the world, drops everything in memory and loads it again like a server restart:
    /// the saved chunk tickets come back and their chunks load before the next tick.
    fn restart(&mut self) -> Result<(), String> {
        Err("this world cannot restart".to_owned())
    }

    /// Uses bone meal on the block at `pos` (`BoneMealItem.growCrop`).
    fn bonemeal(&mut self, _pos: BlockPos) -> Result<(), String> {
        Err("this world has no bone meal".to_owned())
    }

    /// Recomputes the light of the block changes so far. The runner calls it wherever the
    /// recorder waits for Vanilla's light thread (`ParityServer.settleLight`): after the setup,
    /// the entities and the actions of a tick; [`ScenarioWorld::step`] does it for the changes
    /// of its tick. Not across dimensions: there the recorder does not wait either (chunks
    /// load meanwhile).
    fn settle_light(&mut self) {}

    /// The block entities inside `min..=max` as `x,y,z <canonical nbt>` (`"block_entities"`
    /// scenarios, [`crate::trace::canonical_nbt`] of `saveWithFullMetadata`), in any order.
    fn block_entities(&self, min: BlockPos, max: BlockPos, out: &mut Vec<String>) {
        let _ = (min, max, out);
    }

    /// The settled light inside `min..=max` in y, z, x order as `sky << 4 | block`: the levels
    /// a client sees. Worlds without a light engine leave `out` empty.
    fn light(&mut self, min: BlockPos, max: BlockPos, out: &mut Vec<u8>) {
        let _ = (min, max, out);
    }
    /// Adds the scenario's entities after the setup (order and seeds in
    /// [`crate::scenario`]); `entity_seed` seeds the entities created later.
    fn add_entities(&mut self, entities: &[EntitySpec], entity_seed: i64) -> Result<(), String> {
        let _ = entity_seed;
        if entities.is_empty() {
            Ok(())
        } else {
            Err("this world has no entities".to_owned())
        }
    }
    /// The state of the level random, for scenarios with entities.
    fn level_rng(&mut self) -> Option<i64> {
        None
    }
    /// What the players' clients received since the last call, normalized
    /// (`"client_view"` scenarios, see [`crate::client_view`]).
    fn client_view(&mut self) -> Result<Vec<String>, String> {
        Err("this world has no clients to observe".to_owned())
    }
    /// Runs a `hurt` action.
    fn hurt(&mut self, hurt: &Hurt) -> Result<(), String> {
        Err(format!("this world cannot hurt `{}`", hurt.entity))
    }
    /// Runs a `use_item` action.
    fn use_item(&mut self, op: &crate::scenario::UseItemOp) -> Result<(), String> {
        Err(format!("this world cannot use items for `{}`", op.entity))
    }
    /// Runs a `lightning` action in the column of `column`.
    fn lightning(&mut self, column: BlockPos) -> Result<(), String> {
        Err(format!("this world has no lightning ({column:?})"))
    }
    /// Runs an `effect` action.
    fn effect(&mut self, effect: &crate::scenario::EffectOp) -> Result<(), String> {
        Err(format!("this world has no effects for `{}`", effect.entity))
    }
    /// Runs a `command` action: the line as the server at the origin of the overworld.
    /// Returns the plain text of the messages the server got (its `System chat` lines).
    fn command(&mut self, line: &str) -> Result<Vec<String>, String> {
        Err(format!("this world runs no commands (`{line}`)"))
    }
    /// Runs a `throw_potion` action.
    fn player_use(&mut self, _click: &crate::scenario::PlayerUse) -> Result<(), String> {
        Err("this world has no players".to_owned())
    }

    /// Sends a serverbound play packet of a client player (handled before the next tick).
    fn packet(&mut self, op: &crate::scenario::PacketOp) -> Result<(), String> {
        Err(format!(
            "this world has no client players (`{}`)",
            op.entity
        ))
    }
    /// The start of a client tick of every client player: accepts pending server teleports.
    /// A player that joins while the scenario runs (`"join"`); `index` is its index in the
    /// scenario's entities.
    fn join(&mut self, _spec: &EntitySpec, _index: usize) -> Result<(), String> {
        Err("this world has no players".to_owned())
    }

    fn client_begin(&mut self) -> Result<(), String> {
        Ok(())
    }
    /// The end of a client tick of every client player (`client_tick_end`); the packets of
    /// the tick are handled.
    fn client_end(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn throw_potion(&mut self, potion: &crate::scenario::ThrowPotionOp) -> Result<(), String> {
        Err(format!("this world cannot throw `{}`", potion.potion))
    }
}

/// Builds the world for a scenario.
pub type WorldFactory = fn(&WorldSpec, Execution) -> Box<dyn ScenarioWorld>;

/// Parses `name[key=value,...]`.
pub fn parse_state(s: &str) -> Result<BlockState, String> {
    let (name, props) = match s.split_once('[') {
        Some((n, rest)) => (
            n,
            rest.strip_suffix(']')
                .ok_or_else(|| format!("`{s}`: missing `]`"))?,
        ),
        None => (s, ""),
    };
    let mut state = BlockId::by_name(name)
        .ok_or_else(|| format!("unknown block `{name}`"))?
        .default_state();
    for kv in props.split(',').filter(|p| !p.is_empty()) {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("`{s}`: bad property `{kv}`"))?;
        state = state
            .with(k.trim(), v.trim())
            .ok_or_else(|| format!("`{s}`: no property {kv}"))?;
    }
    Ok(state)
}

/// `name[key=value,...]` with the properties sorted by name: the trace's state names.
pub fn state_name(state: BlockState) -> String {
    let mut props: Vec<_> = state.properties().collect();
    let name = state.block().name();
    if props.is_empty() {
        return name.to_owned();
    }
    props.sort_unstable_by_key(|&(k, _)| k);
    let inner: Vec<String> = props.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{name}[{}]", inner.join(","))
}

/// Installs the per-state chunk facts (Vanilla's, without the heightmap tags), once. The
/// same tables as the game's tests, so a test binary gets one set whichever installs first.
fn install_facts() {
    facts::install_vanilla(&[], &[], 1);
}

/// Overworld height range.
const MIN_Y: i32 = -64;
const HEIGHT: u32 = 384;

/// Flat chunks generated on first access, without mechanics: blocks never change by
/// themselves, nothing is scheduled, there are no entities. Stands in for `World::headless`.
#[derive(Debug)]
pub struct FlatWorld {
    layers: Vec<BlockState>,
    chunks: HashMap<(i32, i32), Chunk>,
}

impl FlatWorld {
    /// A flat world of `spec`'s layers. The execution mode does not matter: nothing runs.
    pub fn new(spec: &WorldSpec) -> Result<Self, String> {
        install_facts();
        let mut layers = Vec::new();
        for (block, height) in &spec.layers {
            let state = BlockId::by_name(block)
                .ok_or_else(|| format!("unknown layer block `{block}`"))?
                .default_state();
            layers.extend(std::iter::repeat_n(state, *height as usize));
        }
        Ok(Self {
            layers,
            chunks: HashMap::new(),
        })
    }

    /// [`WorldFactory`] of the stand-in.
    pub fn factory(spec: &WorldSpec, _execution: Execution) -> Box<dyn ScenarioWorld> {
        Box::new(Self::new(spec).unwrap_or_else(|e| panic!("{e}")))
    }

    fn chunk(&mut self, pos: BlockPos) -> &mut Chunk {
        let layers = &self.layers;
        self.chunks
            .entry((pos.x >> 4, pos.z >> 4))
            .or_insert_with(|| {
                let mut chunk = Chunk::new(MIN_Y, HEIGHT, 0, BlockState::AIR.0);
                for (i, state) in layers.iter().enumerate() {
                    for z in 0..16 {
                        for x in 0..16 {
                            chunk.set_block(x, MIN_Y + i as i32, z, state.0);
                        }
                    }
                }
                chunk
            })
    }
}

impl ScenarioWorld for FlatWorld {
    fn set_block_raw(&mut self, pos: BlockPos, state: BlockState) {
        let chunk = self.chunk(pos);
        chunk.set_block((pos.x & 15) as u32, pos.y, (pos.z & 15) as u32, state.0);
    }

    fn place_block(&mut self, pos: BlockPos, state: BlockState) {
        // No block has behaviour here, so there is nothing to update.
        self.set_block_raw(pos, state);
    }

    fn block(&self, pos: BlockPos) -> BlockState {
        if let Some(chunk) = self.chunks.get(&(pos.x >> 4, pos.z >> 4)) {
            return BlockState(
                chunk
                    .block((pos.x & 15) as u32, pos.y, (pos.z & 15) as u32)
                    .unwrap_or(BlockState::AIR.0),
            );
        }
        let layer = usize::try_from(pos.y - MIN_Y).ok();
        layer
            .and_then(|i| self.layers.get(i).copied())
            .unwrap_or(BlockState::AIR)
    }

    fn step(&mut self) {}

    fn block_ticks(&self, _: BlockPos, _: BlockPos, _: &mut Vec<TickObs>) {}

    fn fluid_ticks(&self, _: BlockPos, _: BlockPos, _: &mut Vec<TickObs>) {}

    fn entities(&self, _: BlockPos, _: BlockPos, _: &mut Vec<EntityObs>) {}
}
