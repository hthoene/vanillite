//! The game state of one world and Vanilla's level operations on it (`Level`, `ServerLevel`).
//!
//! [`Level`] owns the world's chunks and entities plus everything the simulation keeps between
//! ticks: the neighbour updater, block events, the level random, the blocks changed for clients
//! and the level events to broadcast. It is owned by the world's thread; block behaviours are
//! plain functions over `&mut Level` ([`crate::block`]).
//!
//! - [`Level::set_block`] is Vanilla's `setBlock` with its update flags ([`update`]): the chunk
//!   write with `affectNeighborsAfterRemoval` and `onPlace`, the client update, neighbour
//!   updates and shape updates, all through Vanilla's chained [`neighbors`] updater.
//! - [`Level::tick`] runs the world's simulation phases in Vanilla's order (scheduled block and
//!   fluid ticks, random ticks, block events); entities tick in the world's entity phase.

pub mod block_entity;
pub mod border;
pub mod difficulty;
mod entity_storage;
pub mod explosion;
pub mod forced;
pub mod game_event;
pub mod locate;
pub mod mob_env;
mod neighbors;
pub mod particles;
pub mod place;
#[cfg(test)]
pub(crate) use neighbors::STEPS as NEIGHBOR_STEPS;
pub mod signal;
pub mod structure_refs;
mod tick;
pub mod tiles;
pub mod update;
pub mod view;

pub use crate::random_sequences::RandomSequences;

/// The loot random sequences the levels of a server share.
pub type SharedRandomSequences = Arc<std::sync::Mutex<RandomSequences>>;

#[cfg(test)]
pub(crate) mod tests;

use std::sync::Arc;

use vanillite_base::hash::{FxHashMap, FxHashSet};
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, ChunkPos, Direction, SectionPos};
use vanillite_chunk::ProtoChunk;
use vanillite_data::block::{BlockId, BlockState};
use vanillite_data::fluid::FluidState;
use vanillite_world::chunks::Chunks;
use vanillite_world::entity::{EntityId, NetworkIds};
use vanillite_world::ticks::TickPriority;
use vanillite_worldgen::behaviour::{self as shared, BlockLevel, StateExt};

use crate::block::weather::Climate;
use crate::block::{self, Behaviours};
use crate::entities::Entities;

pub use block_entity::{BlockEntities, BlockEntity};
pub use neighbors::NeighborUpdater;

/// Vanilla's update depth of `setBlock` (`Block.UPDATE_LIMIT`).
pub const UPDATE_LIMIT: i32 = 512;

/// The state the level random holds while lent out ([`Level::lend_random`]).
const LENT_RANDOM: i64 = 0x4C45_4E54;

/// Vanilla mechanics that are commonly called exploits (decision D1): `true` keeps Vanilla's
/// behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExploitSettings {
    /// TNT duplication with pistons and slime or honey blocks.
    pub tnt_duplication: bool,
    /// Carpet duplication with pistons.
    pub carpet_duplication: bool,
    /// Rail duplication with pistons.
    pub rail_duplication: bool,
    /// Pistons without a head.
    pub headless_pistons: bool,
    /// Bedrock and end portal frames broken with pistons and explosions.
    pub break_unbreakable_blocks: bool,
    /// String duplication with tripwire mechanics.
    pub string_duplication: bool,
}

impl Default for ExploitSettings {
    fn default() -> Self {
        ExploitSettings {
            tnt_duplication: true,
            carpet_duplication: true,
            rail_duplication: true,
            headless_pistons: true,
            break_unbreakable_blocks: true,
            string_duplication: true,
        }
    }
}

/// Rules and budgets of a world's simulation.
#[derive(Clone, Debug)]
pub struct LevelSettings {
    /// Scheduled block ticks run per tick (Vanilla: 65536).
    pub max_block_ticks: usize,
    /// Scheduled fluid ticks run per tick (Vanilla: 65536).
    pub max_fluid_ticks: usize,
    /// Chained neighbour updates before the rest is skipped (`max-chained-neighbor-updates`);
    /// negative means unlimited.
    pub max_chained_neighbor_updates: i32,
    /// `randomTickSpeed` game rule.
    pub random_tick_speed: i32,
    /// `waterSourceConversion` game rule.
    pub water_source_conversion: bool,
    /// `lavaSourceConversion` game rule.
    pub lava_source_conversion: bool,
    /// `doTileDrops` game rule (`BLOCK_DROPS`).
    pub block_drops: bool,
    /// `doEntityDrops` game rule (`ENTITY_DROPS`).
    pub entity_drops: bool,
    /// `max_snow_accumulation_height` game rule.
    pub max_snow_accumulation_height: i32,
    /// `fire_spread_radius_around_player` game rule (-1: everywhere).
    pub fire_spread_radius: i32,
    /// `spread_vines` game rule.
    pub spread_vines: bool,
    /// The server's difficulty (`level.getDifficulty()`).
    pub difficulty: crate::survival::rules::Difficulty,
    /// `getOverworldClockTime` (local difficulty), set by the server every tick.
    pub overworld_clock_time: i64,
    /// `getMoonBrightness` of this dimension (`visual/moon_phase`), set by the server every
    /// tick.
    pub moon_brightness: f32,
    /// The dimension's `infiniburn` tag.
    pub infiniburn: crate::block::fire::Infiniburn,
    /// The dimension's `fast_lava` attribute (the Nether).
    pub fast_lava: bool,
    /// `tntExplodes` game rule (`TNT_EXPLODES`).
    pub tnt_explodes: bool,
    /// `tnt_explosion_drop_decay` game rule.
    pub tnt_explosion_drop_decay: bool,
    /// `ServerLevel.isSpawningMonsters`: the `spawn_mobs` and `spawn_monsters` game rules.
    pub spawning_monsters: bool,
    /// The dimension has sky light.
    pub has_sky_light: bool,
    /// The dimension's `water_evaporates` attribute (the Nether).
    pub water_evaporates: bool,
    /// The dimension's `snow_golem_melts` attribute (the Nether); biomes override it.
    pub snow_golem_melts: bool,
    /// Fire lights nether portals here (`BaseFireBlock.inPortalDimension`: the Overworld and
    /// the Nether).
    pub portal_dimension: bool,
    /// Exploit switches (D1).
    pub exploits: ExploitSettings,
    /// Ticks a hopper waits after moving an item (`HopperBlockEntity.MOVE_ITEM_SPEED`, 8).
    pub hopper_transfer_cooldown: i32,
    /// Ticks a hopper waits after finding nothing to move (Vanilla 1: every tick).
    pub hopper_check_cooldown: i32,
    /// Radius of the experience orb merge scan (Vanilla 0.5).
    pub xp_merge_radius: f64,
    /// The difficulty is peaceful (set by the server every tick).
    pub peaceful: bool,
    /// `spawnWardens` game rule (set by the server every tick).
    pub spawn_wardens: bool,
    /// `command_blocks_work` game rule (`isCommandBlockEnabled`).
    pub command_blocks_work: bool,
    /// `max_command_sequence_length` game rule (chain command blocks).
    pub max_command_sequence_length: i32,
    /// `send_command_feedback` game rule (new command blocks track their output).
    pub send_command_feedback: bool,
    /// `mobGriefing` game rule (`MOB_GRIEFING`).
    pub mob_griefing: bool,
    /// `projectiles_can_break_blocks` game rule.
    pub projectiles_can_break_blocks: bool,
    /// `block_explosion_drop_decay` game rule.
    pub block_explosion_drop_decay: bool,
    /// `players_sleeping_percentage` game rule.
    pub players_sleeping_percentage: i32,
    /// The dimension's `gameplay/bed_rule` attribute.
    pub bed_rule: crate::block::bed::BedRule,
    /// The dimension's `gameplay/straw_bed_rule` attribute.
    pub straw_bed_rule: crate::block::bed::BedRule,
    /// The dimension's `gameplay/respawn_anchor_works` attribute (the Nether).
    pub respawn_anchor_works: bool,
    /// The dimension, e.g. `minecraft:overworld`.
    pub dimension: String,
}

impl Default for LevelSettings {
    fn default() -> Self {
        LevelSettings {
            max_block_ticks: 65536,
            max_fluid_ticks: 65536,
            max_chained_neighbor_updates: 1_000_000,
            random_tick_speed: 3,
            water_source_conversion: true,
            lava_source_conversion: false,
            block_drops: true,
            entity_drops: true,
            max_snow_accumulation_height: 1,
            fire_spread_radius: 128,
            spread_vines: true,
            difficulty: crate::survival::rules::Difficulty::Normal,
            overworld_clock_time: 0,
            moon_brightness: 1.0,
            infiniburn: crate::block::fire::Infiniburn::Overworld,
            fast_lava: false,
            tnt_explodes: true,
            tnt_explosion_drop_decay: false,
            spawning_monsters: true,
            has_sky_light: true,
            water_evaporates: false,
            snow_golem_melts: false,
            portal_dimension: true,
            exploits: ExploitSettings::default(),
            hopper_transfer_cooldown: 8,
            hopper_check_cooldown: 1,
            xp_merge_radius: 0.5,
            peaceful: false,
            spawn_wardens: true,
            command_blocks_work: true,
            max_command_sequence_length: 65536,
            send_command_feedback: true,
            mob_griefing: true,
            projectiles_can_break_blocks: true,
            block_explosion_drop_decay: true,
            players_sleeping_percentage: 100,
            bed_rule: crate::block::bed::BedRule::can_sleep_when_dark(),
            straw_bed_rule: crate::block::bed::BedRule::destroy_on_leave(),
            respawn_anchor_works: false,
            dimension: "minecraft:overworld".to_owned(),
        }
    }
}

impl LevelSettings {
    /// Takes the simulation's game rules from `rules`.
    pub fn apply_game_rules(&mut self, rules: &crate::survival::rules::GameRules) {
        use crate::survival::rules::Rule;
        let get = |name: &str| Rule::by_name(name).map(|r| rules.get(r));
        if let Some(v) = get("random_tick_speed") {
            self.random_tick_speed = v;
        }
        if let Some(v) = get("water_source_conversion") {
            self.water_source_conversion = v != 0;
        }
        if let Some(v) = get("lava_source_conversion") {
            self.lava_source_conversion = v != 0;
        }
        if let Some(v) = get("block_drops") {
            self.block_drops = v != 0;
        }
        if let Some(v) = get("tnt_explodes") {
            self.tnt_explodes = v != 0;
        }
        if let Some(v) = get("tnt_explosion_drop_decay") {
            self.tnt_explosion_drop_decay = v != 0;
        }
        self.spawning_monsters = rules.bool(Rule::SPAWN_MOBS) && rules.bool(Rule::SPAWN_MONSTERS);
        if let Some(v) = get("entity_drops") {
            self.entity_drops = v != 0;
        }
        if let Some(v) = get("max_snow_accumulation_height") {
            self.max_snow_accumulation_height = v;
        }
        if let Some(v) = get("fire_spread_radius_around_player") {
            self.fire_spread_radius = v;
        }
        if let Some(v) = get("spread_vines") {
            self.spread_vines = v != 0;
        }
        self.command_blocks_work = rules.bool(Rule::COMMAND_BLOCKS_WORK);
        self.max_command_sequence_length = rules.get(Rule::MAX_COMMAND_SEQUENCE_LENGTH);
        self.send_command_feedback = rules.bool(Rule::SEND_COMMAND_FEEDBACK);
        if let Some(v) = get("mob_griefing") {
            self.mob_griefing = v != 0;
        }
        if let Some(v) = get("projectiles_can_break_blocks") {
            self.projectiles_can_break_blocks = v != 0;
        }
        self.block_explosion_drop_decay = rules.bool(Rule::BLOCK_EXPLOSION_DROP_DECAY);
        if let Some(v) = get("players_sleeping_percentage") {
            self.players_sleeping_percentage = v;
        }
    }
}

/// A block event (`BlockEventData`): run at the end of the tick, then sent to nearby players.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockEvent {
    /// Position.
    pub pos: BlockPos,
    /// The block that queued it; the event is dropped if the block changed.
    pub block: BlockId,
    /// First parameter.
    pub a: i32,
    /// Second parameter.
    pub b: i32,
}

/// A level event (`levelEvent`: sounds and particles of the client), sent to players within 64
/// blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelEvent {
    /// Event id (`LevelEvent`).
    pub event: i32,
    /// Position.
    pub pos: BlockPos,
    /// Event data.
    pub data: i32,
    /// The player that caused it and predicted it locally; it is not sent to them.
    pub source: Option<EntityId>,
}

/// A sound at a position (`playSound`, `ClientboundSoundPacket`), sent to players within its
/// range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sound {
    /// `minecraft:sound_event` id.
    pub sound: u16,
    /// `SoundSource` ordinal.
    pub source: u8,
    /// Position.
    pub x: f64,
    /// Position.
    pub y: f64,
    /// Position.
    pub z: f64,
    /// Volume; the range is `16 * max(volume, 1)` blocks.
    pub volume: f32,
    /// Pitch.
    pub pitch: f32,
    /// Seed of the client's random variation.
    pub seed: i64,
    /// The player that caused it and played it locally; it is not sent to them.
    pub except: Option<EntityId>,
}

/// `SoundSource.BLOCKS`.
pub const SOUND_BLOCKS: u8 = 4;
/// `SoundSource.PLAYERS`.
pub const SOUND_PLAYERS: u8 = 7;
/// `SoundSource.HOSTILE`.
pub const SOUND_HOSTILE: u8 = 5;
/// `SoundSource.NEUTRAL`.
pub const SOUND_NEUTRAL: u8 = 6;
/// `SoundSource.RECORDS`.
pub const SOUND_RECORDS: u8 = 2;

/// The changed positions of one section, each once, in change order. Long lists check for
/// duplicates with a bit per block instead of a scan (a `fill` changes whole sections).
#[derive(Debug, Default, Clone)]
pub struct SectionChanges {
    positions: Vec<BlockPos>,
    seen: Option<Box<[u64; 64]>>,
}

impl SectionChanges {
    /// Up to this many positions are checked by a scan.
    const SCAN: usize = 16;

    /// Adds `pos` unless it is already there.
    pub fn insert(&mut self, pos: BlockPos) {
        if let Some(seen) = &mut self.seen {
            if Self::mark(seen, pos) {
                self.positions.push(pos);
            }
        } else if !self.positions.contains(&pos) {
            self.positions.push(pos);
            if self.positions.len() > Self::SCAN {
                let mut seen = Box::new([0; 64]);
                for &p in &self.positions {
                    Self::mark(&mut seen, p);
                }
                self.seen = Some(seen);
            }
        }
    }

    /// Sets the bit of `pos`; whether it was clear.
    fn mark(seen: &mut [u64; 64], pos: BlockPos) -> bool {
        let index = ((pos.y & 15) << 8 | (pos.z & 15) << 4 | (pos.x & 15)) as usize;
        let (word, bit) = (index >> 6, 1u64 << (index & 63));
        let new = seen[word] & bit == 0;
        seen[word] |= bit;
        new
    }

    /// The positions in change order.
    pub fn positions(&self) -> &[BlockPos] {
        &self.positions
    }

    /// Whether no position changed.
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }
}

/// What the world sends after a tick: changed blocks per section, block events, level events
/// and sounds. Filled by the simulation, drained by the server.
#[derive(Debug, Default)]
pub struct LevelOutput {
    /// Changed positions (`ChunkHolder.blockChanged`), deduplicated per section, in change
    /// order.
    pub changed: FxHashMap<SectionPos, SectionChanges>,
    /// Block events that ran (`ClientboundBlockEventPacket`).
    pub block_events: Vec<BlockEvent>,
    /// Level events.
    pub level_events: Vec<LevelEvent>,
    /// Sounds.
    pub sounds: Vec<Sound>,
    /// Server particles (`sendParticles`).
    pub particles: Vec<particles::LevelParticles>,
    /// Explosions.
    pub explosions: Vec<explosion::ExplosionEvent>,
    /// The damage of the living entities explosions hit, for the mob system.
    pub explosion_hurts: Vec<explosion::ExplosionHurt>,
    /// The damage of the living entities falling blocks land on, for the mob system.
    pub falling_hurts: Vec<crate::block::falling::FallingHurt>,
    /// Magic damage block entities deal (conduits): target and amount.
    pub magic_hurts: Vec<(vanillite_world::entity::EntityId, f32)>,
    /// Experience to award at a position (`ExperienceOrb.award`; orbs are not simulated yet).
    pub experience: Vec<(vanillite_base::Vec3, i32)>,
    /// Block entities whose client data changed (`sendBlockUpdated` of a block entity).
    pub block_entity_updates: Vec<BlockPos>,
    /// Global level events (`globalLevelEvent`: event, position, data), heard by every player
    /// of the server.
    pub global_level_events: Vec<(i32, BlockPos, i32)>,
    /// Wither skeleton skulls players placed, for the wither's summon check
    /// (`WitherSkullBlock.setPlacedBy`).
    pub summon_checks: Vec<BlockPos>,
    /// `PiglinAi.angerNearbyPiglins` calls of the players' block uses and breaks (player,
    /// `onlyIfTheySeeThePlayer`), run before the entities tick.
    pub piglin_anger: Vec<(vanillite_world::entity::EntityId, bool)>,
    /// An end crystal was placed: the End fight tries to respawn the dragon (`tryRespawn`).
    pub try_dragon_respawn: bool,
    /// What the dragon and the crystals reported outside the entity tick (commands), handed
    /// to the End fight before its next tick.
    pub dragon_events: Vec<crate::mob::boss::dragon::DragonEvent>,
    /// Entity packets for the players near a position (position, range): equipment a
    /// dispenser put on an armor stand.
    pub nearby_packets: Vec<(
        vanillite_base::Vec3,
        f64,
        Arc<vanillite_protocol::SharedPacket>,
    )>,
}

/// The terrain generator of a world (`getChunkSource().getGenerator()`).
#[derive(Clone)]
pub struct Generation {
    /// Released while the server is empty; [`Generation::terrain`] builds it again.
    pub shared: vanillite_worldgen::SharedTerrain,
    /// The dimension it generates.
    pub dimension: String,
    /// `WorldOptions.generateStructures`.
    pub structures: bool,
}

impl Generation {
    /// The generator.
    pub fn terrain(&self) -> Arc<vanillite_worldgen::generator::TerrainGenerator> {
        self.shared.get()
    }
}

impl std::fmt::Debug for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generation")
            .field("structures", &self.structures)
            .finish_non_exhaustive()
    }
}

/// The game state of one world.
pub struct Level {
    /// Chunks, tickets, scheduled ticks.
    pub chunks: Chunks,
    /// Entities.
    pub entities: Entities,
    /// Rules and budgets.
    pub settings: LevelSettings,
    behaviours: Arc<Behaviours>,
    network_ids: Arc<NetworkIds>,
    climate: Option<Arc<Climate>>,
    /// The trees saplings grow into.
    tree_features: Option<Arc<crate::block::tree::TreeFeatures>>,
    updater: NeighborUpdater,
    /// Live block entities.
    pub block_entities: BlockEntities,
    /// The world border (`getWorldBorder`).
    pub border: border::WorldBorder,
    /// Force-loaded chunks (`getForceLoadedChunks`).
    pub forced: forced::ForcedChunks,
    /// The world's noise generator, for commands that ask it (`/locate`); `None` for other
    /// generators.
    pub generation: Option<Generation>,
    /// Reused buffers of the piston code.
    pub(crate) pistons: crate::block::piston::Scratch,
    /// `ServerLevel.handlingTick`: the simulation phases of the tick run (scheduled ticks to
    /// block events).
    handling_tick: bool,
    /// `RedstoneWireBlock.shouldSignal`: `false` while a wire computes its power.
    wire_signals: bool,
    /// `RedstoneTorchBlock.RECENT_TOGGLES` of this level: torch positions and when they turned
    /// off, oldest first.
    pub(crate) torch_toggles: Vec<(BlockPos, i64)>,
    /// Scratch list of [`Level::tick_chunk_runtime`].
    chunk_scratch: Vec<ChunkPos>,
    /// Scratch list of the visibility changes.
    visibility_scratch: Vec<vanillite_world::chunks::VisibilityChange>,
    /// Chunks that became visible and load their entities in [`Level::tick_entity_manager`]
    /// (`PersistentEntitySectionManager.loadingInbox`).
    pending_entity_loads: Vec<ChunkPos>,
    /// The stored data live entities were loaded from: written back with their modelled
    /// fields replaced, so stored fields the game does not model are kept.
    entity_nbt: FxHashMap<EntityId, vanillite_nbt::Compound>,
    /// Stored entities of kinds the game does not model, per live chunk; written back
    /// unchanged.
    parked_entities: FxHashMap<ChunkPos, Vec<Arc<[u8]>>>,
    /// Live chunks whose stored entities are not empty.
    entity_chunks: FxHashSet<ChunkPos>,
    /// `blockEvents`, a linked set: an event already queued is not queued again.
    block_events: Vec<BlockEvent>,
    block_event_set: FxHashSet<BlockEvent>,
    random: LegacyRandom,
    /// `soundSeedGenerator`: the seeds of sounds without an explicit seed; separate from the
    /// level random so sounds do not shift the simulation's random sequence.
    sound_seeds: LegacyRandom,
    /// The seed of the next entity [`Level::load_entity`] creates instead of one from its UUID
    /// (an entity arriving from another world, [`Level::load_travelled_entity`]).
    next_entity_seed: Option<i64>,
    /// `randValue` of `getBlockRandomPos`.
    rand_value: i32,
    /// The server's named random sequences (loot tables).
    random_sequences: Arc<std::sync::Mutex<crate::random_sequences::RandomSequences>>,
    /// Rain and thunder levels (`Level.rainLevel`, `thunderLevel`), set by the world's weather.
    weather: crate::survival::weather::LevelWeather,
    game_time: i64,
    output: LevelOutput,
    /// The block phase of the last explosion, waiting for the entities it hit to take their
    /// damage ([`Level::finish_explosion`]).
    pending_blast: Option<explosion::PendingBlast>,
    /// Menus blocks asked to open for the player that used them (`player.openMenu`); taken
    /// by the player handlers.
    menu_requests: Vec<crate::menu::container::MenuRequest>,
    /// Sign editors blocks asked to open: player, position, front side.
    sign_editors: Vec<(EntityId, BlockPos, bool)>,
    player_block_uses: Vec<(EntityId, BlockPos, crate::block::bed::PlayerBlockUse)>,
    /// `ServerLevel.sleepStatus`.
    pub sleep_status: crate::survival::sleep::SleepStatus,
    /// Stacks blocks hand to the player that used them (`player.getInventory().add`, else
    /// dropped); taken by the player handlers.
    player_gives: Vec<(EntityId, crate::item::ItemStack)>,
    /// What blocks did to the player that used them; taken by the player handlers.
    player_effects: Vec<(EntityId, block::PlayerEffect)>,
    /// Statistics and criterion triggers for players (`awardStat`, `CriteriaTriggers`) from
    /// block code; the server applies them after the players' tick.
    player_awards: Vec<(EntityId, crate::advancements::award::Award)>,
    /// `test_instance_block_action` packets of the tick; the server's game tests handle them
    /// after the players' packets.
    pub test_instance_actions: Vec<crate::block::technical::test_instance::Action>,
    /// The server's maps (shared by all worlds).
    maps: Option<Arc<crate::map::Maps>>,
    /// The references exploration maps added to structure starts.
    structure_refs: Arc<structure_refs::StructureReferences>,
    /// The server's structure templates (`getStructureTemplateManager`, shared by all worlds).
    templates: Option<Arc<vanillite_worldgen::template::manager::StructureTemplateManager>>,
    /// The data packs loaded at start (template pools of jigsaw blocks).
    packs: Option<Arc<vanillite_datapack::LoadedPacks>>,
    min_y: i32,
    max_y: i32,
    /// The seed the level was created with (`getSeed`).
    seed: i64,
    /// What spawner blocks read of this tick's rules and sky (set by the world each tick).
    pub spawner_env: crate::block::spawner::SpawnerEnv,
    /// The plugin hooks of this world; `None` without plugins, so hook sites cost one
    /// branch ([`Level::hooks_for`]).
    hooks: Option<Arc<dyn crate::hooks::GameHooks + Send>>,
    /// The server's dispatcher for command blocks and command block minecarts.
    pub(crate) command_runner:
        Option<Arc<dyn crate::block::technical::command_block::CommandRunner>>,
    /// POI changes of block changes made while the entities (with the POI manager) were out
    /// of the level; applied by [`Level::flush_poi_changes`].
    poi_changes: Vec<(
        BlockPos,
        Option<crate::poi::PoiType>,
        Option<crate::poi::PoiType>,
    )>,
}

// Read-only phases share the level between threads (tile region mode, `tiles`).
const _: () = {
    const fn shareable<T: Sync>() {}
    shareable::<Level>();
};

impl std::fmt::Debug for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Level")
            .field("chunks", &self.chunks)
            .field("game_time", &self.game_time)
            .finish_non_exhaustive()
    }
}

impl Level {
    /// Installs the plugin hooks of this world (only when plugins are loaded).
    pub fn set_hooks(&mut self, hooks: Arc<dyn crate::hooks::GameHooks + Send>) {
        self.entities.set_hooks(Arc::clone(&hooks));
        self.hooks = Some(hooks);
    }

    /// The plugin hooks when a plugin listens to `kind` (`vanillite_abi::event`): without
    /// plugins one branch, with plugins one bit of the subscription mask. Build the
    /// [`crate::hooks::Event`] only when this returns hooks.
    #[inline]
    pub fn hooks_for(&self, kind: u32) -> Option<Arc<dyn crate::hooks::GameHooks + Send>> {
        match &self.hooks {
            Some(h) if h.wants(kind) => Some(Arc::clone(h)),
            _ => None,
        }
    }

    /// The installed plugin hooks, if any (for code that passes hooks on).
    pub fn hooks(&self) -> Option<&Arc<dyn crate::hooks::GameHooks + Send>> {
        self.hooks.as_ref()
    }

    /// The game state of a world over its chunk runtime.
    pub fn new(
        chunks: Chunks,
        settings: LevelSettings,
        behaviours: Arc<Behaviours>,
        network_ids: Arc<NetworkIds>,
        seed: i64,
    ) -> Level {
        let info = chunks.info();
        let min_y = info.min_y;
        let max_y = info.min_y + info.height as i32 - 1;
        let mut level = Level {
            chunks,
            entities: Entities::new(),
            updater: NeighborUpdater::new(settings.max_chained_neighbor_updates),
            settings,
            behaviours,
            network_ids,
            climate: None,
            tree_features: None,
            block_entities: BlockEntities::default(),
            border: border::WorldBorder::default(),
            forced: forced::ForcedChunks::default(),
            generation: None,
            pistons: crate::block::piston::Scratch::default(),
            handling_tick: false,
            wire_signals: true,
            torch_toggles: Vec::new(),
            chunk_scratch: Vec::new(),
            visibility_scratch: Vec::new(),
            pending_entity_loads: Vec::new(),
            entity_nbt: FxHashMap::default(),
            parked_entities: FxHashMap::default(),
            entity_chunks: FxHashSet::default(),
            block_events: Vec::new(),
            block_event_set: FxHashSet::default(),
            random: LegacyRandom::new(seed),
            sound_seeds: LegacyRandom::new(seed ^ 0x50_4E_44_53),
            next_entity_seed: None,
            rand_value: LegacyRandom::new(seed ^ 0x5DEE_CE66).next_int(),
            random_sequences: Arc::new(std::sync::Mutex::new(
                crate::random_sequences::RandomSequences::new(seed),
            )),
            weather: crate::survival::weather::LevelWeather::default(),
            game_time: 0,
            output: LevelOutput::default(),
            pending_blast: None,
            menu_requests: Vec::new(),
            sign_editors: Vec::new(),
            player_block_uses: Vec::new(),
            sleep_status: crate::survival::sleep::SleepStatus::default(),
            player_gives: Vec::new(),
            player_effects: Vec::new(),
            player_awards: Vec::new(),
            test_instance_actions: Vec::new(),
            maps: None,
            structure_refs: Arc::default(),
            templates: None,
            packs: None,
            min_y,
            max_y,
            seed,
            spawner_env: crate::block::spawner::SpawnerEnv::default(),
            command_runner: None,
            hooks: None,
            poi_changes: Vec::new(),
        };
        level.entities.poi = crate::poi::PoiManager::new_live();
        // Chunks that are visible already queue their entities for the end of the first tick.
        level.apply_visibility();
        level
    }

    /// Sets the server's structure templates.
    pub fn set_structure_templates(
        &mut self,
        templates: Arc<vanillite_worldgen::template::manager::StructureTemplateManager>,
    ) {
        self.templates = Some(templates);
    }

    /// Sets the data packs loaded at start.
    pub fn set_data_packs(&mut self, packs: Arc<vanillite_datapack::LoadedPacks>) {
        self.packs = Some(packs);
    }

    /// The data packs loaded at start (`registryAccess`), if the server has them.
    pub fn data_packs(&self) -> Option<&Arc<vanillite_datapack::LoadedPacks>> {
        self.packs.as_ref()
    }

    /// `getStructureTemplateManager`: the server's structure templates, if it has any.
    pub fn structure_templates(
        &self,
    ) -> Option<&Arc<vanillite_worldgen::template::manager::StructureTemplateManager>> {
        self.templates.as_ref()
    }

    /// `getSeed`.
    pub fn seed(&self) -> i64 {
        self.seed
    }

    /// `sendBlockUpdated` of a block entity whose client data changed: the block and a
    /// `block_entity_data` packet with its update tag go to the clients that see its chunk.
    pub fn send_block_entity_updated(&mut self, pos: BlockPos) {
        self.send_block_updated(pos);
        if !self.output.block_entity_updates.contains(&pos) {
            self.output.block_entity_updates.push(pos);
        }
    }

    /// `player.openMenu(provider)` from block code: the player handlers open it.
    pub fn request_menu(&mut self, request: crate::menu::container::MenuRequest) {
        self.menu_requests.push(request);
    }

    /// The menus blocks asked to open since the last call.
    pub fn take_menu_requests(&mut self) -> Vec<crate::menu::container::MenuRequest> {
        std::mem::take(&mut self.menu_requests)
    }

    /// `player.openTextEdit(sign, side)` from block code.
    pub fn request_sign_editor(&mut self, player: EntityId, pos: BlockPos, front: bool) {
        self.sign_editors.push((player, pos, front));
    }

    /// Uses the server's map store.
    pub fn set_maps(&mut self, maps: Arc<crate::map::Maps>) {
        self.maps = Some(maps);
    }

    /// Uses saved structure start references.
    pub fn set_structure_refs(&mut self, refs: structure_refs::StructureReferences) {
        self.structure_refs = Arc::new(refs);
    }

    /// The references exploration maps added to structure starts.
    pub fn structure_refs(&self) -> &Arc<structure_refs::StructureReferences> {
        &self.structure_refs
    }

    /// The server's map store.
    pub fn maps_shared(&self) -> Option<Arc<crate::map::Maps>> {
        self.maps.clone()
    }

    /// The maps of the server.
    pub fn maps(&self) -> Option<&crate::map::Maps> {
        self.maps.as_deref()
    }

    /// `player.getInventory().add(stack)` (or drop) from block code.
    pub fn give_to_player(&mut self, player: EntityId, stack: crate::item::ItemStack) {
        if !stack.is_empty() {
            self.player_gives.push((player, stack));
        }
    }

    /// Takes the stacks blocks handed to players.
    pub fn take_player_gives(&mut self) -> Vec<(EntityId, crate::item::ItemStack)> {
        std::mem::take(&mut self.player_gives)
    }

    /// A block's effect on the player that used it ([`block::PlayerEffect`]).
    pub fn player_effect(&mut self, player: EntityId, effect: block::PlayerEffect) {
        self.player_effects.push((player, effect));
    }

    /// Takes the effects blocks had on players.
    pub fn take_player_effects(&mut self) -> Vec<(EntityId, block::PlayerEffect)> {
        std::mem::take(&mut self.player_effects)
    }

    /// `player.awardStat(stat, count)` from block code.
    pub fn award_stat(&mut self, player: EntityId, stat: crate::stats::Stat, count: i32) {
        self.player_awards
            .push((player, crate::advancements::award::Award::Stat(stat, count)));
    }

    /// `CriteriaTriggers.X.trigger(player, ...)` from block code.
    pub fn trigger(&mut self, player: EntityId, event: crate::advancements::trigger::Event) {
        self.player_awards
            .push((player, crate::advancements::award::Award::Trigger(event)));
    }

    /// `clearBlockEvents(bb)`: drops the queued block events inside `min..=max`.
    pub fn clear_block_events(&mut self, min: BlockPos, max: BlockPos) {
        let inside = |p: BlockPos| {
            (min.x..=max.x).contains(&p.x)
                && (min.y..=max.y).contains(&p.y)
                && (min.z..=max.z).contains(&p.z)
        };
        self.block_events.retain(|e| !inside(e.pos));
        self.block_event_set.retain(|e| !inside(e.pos));
    }

    /// Takes the statistics and triggers block code awarded since the last call.
    pub fn take_player_awards(&mut self) -> Vec<(EntityId, crate::advancements::award::Award)> {
        std::mem::take(&mut self.player_awards)
    }

    /// Takes the sign editors blocks asked to open.
    pub fn take_sign_editors(&mut self) -> Vec<(EntityId, BlockPos, bool)> {
        std::mem::take(&mut self.sign_editors)
    }

    /// A bed or respawn anchor a player used: the player's side runs after its packets
    /// ([`crate::survival::sleep::run_block_uses`]).
    pub fn player_block_use(
        &mut self,
        player: EntityId,
        pos: BlockPos,
        kind: crate::block::bed::PlayerBlockUse,
    ) {
        self.player_block_uses.push((player, pos, kind));
    }

    /// Takes the bed and anchor uses since the last call.
    pub fn take_player_block_uses(
        &mut self,
    ) -> Vec<(EntityId, BlockPos, crate::block::bed::PlayerBlockUse)> {
        std::mem::take(&mut self.player_block_uses)
    }

    /// `setChanged` + `sendBlockUpdated` of a block entity whose client data changed: the
    /// clients near it get its data again.
    pub fn block_entity_changed(&mut self, pos: BlockPos) {
        self.block_entities.touch(pos.chunk());
        self.output.block_entity_updates.push(pos);
    }

    /// `state.attack(level, pos, player)` of the block at `pos`.
    pub fn attack_block(&mut self, pos: BlockPos, player: EntityId) {
        let state = self.block(pos);
        if let Some(f) = self.behaviours.get(state.block()).attack {
            f(self, state, pos, player);
        }
    }

    /// `state.useItemOn(stack, level, player, hand, hit)` of the block at `pos`.
    pub fn use_item_on(
        &mut self,
        pos: BlockPos,
        context: &block::UseContext,
        stack: &mut crate::item::ItemStack,
    ) -> block::ItemInteraction {
        let state = self.block(pos);
        match self.behaviours.get(state.block()).use_item_on {
            Some(f) => f(self, state, pos, context, stack),
            None => block::ItemInteraction::TryWithEmptyHand,
        }
    }

    /// Replaces the rules and budgets (config reload between ticks).
    pub fn set_settings(&mut self, settings: LevelSettings) {
        self.updater
            .set_max_chained(settings.max_chained_neighbor_updates);
        self.settings = settings;
    }

    /// Sets the world's biomes, for the biome dependent simulation (freezing).
    pub fn set_climate(&mut self, climate: Arc<Climate>) {
        self.climate = Some(climate);
    }

    /// The world's biomes, if known.
    pub fn climate(&self) -> Option<&Climate> {
        self.climate.as_deref()
    }

    /// Sets the tree features saplings grow into (none: saplings do not grow into trees).
    pub fn set_tree_features(&mut self, trees: Arc<crate::block::tree::TreeFeatures>) {
        self.tree_features = Some(trees);
    }

    /// The tree features saplings grow into.
    pub fn tree_features(&self) -> Option<&Arc<crate::block::tree::TreeFeatures>> {
        self.tree_features.as_ref()
    }

    /// `getBiome(pos)`: the noise biome the biome manager picks for `pos`; the default biome
    /// outside live chunks.
    pub fn biome(&self, pos: BlockPos) -> u16 {
        self.biome_near(pos, None)
    }

    /// [`Level::biome`] with a full chunk already looked up (`near`), used when the biome
    /// lies in it.
    pub fn biome_near(&self, pos: BlockPos, near: Option<&ProtoChunk>) -> u16 {
        let default = self.chunks.info().default_biome;
        let Some(climate) = &self.climate else {
            return default;
        };
        climate
            .manager
            .get_biome(pos.x, pos.y, pos.z, &mut |qx: i32, qy: i32, qz: i32| {
                let at = ChunkPos::new(qx >> 2, qz >> 2);
                let found = match near {
                    Some(c) if c.pos == at => Some(c),
                    _ => self.chunks.chunk(at).map(|c| &**c),
                };
                let Some(chunk) = found else {
                    return default;
                };
                // `LevelChunk.getNoiseBiome`: the quart Y is clamped to the chunk.
                let sections = chunk.chunk.sections();
                let min_quart = chunk.chunk.min_section() * 4;
                let qy = qy.clamp(min_quart, min_quart + sections.len() as i32 * 4 - 1);
                let index = (qy - min_quart) >> 2;
                sections[index as usize].biome((qx & 3) as u32, (qy & 3) as u32, (qz & 3) as u32)
            })
    }

    /// The block behaviours.
    pub fn behaviours(&self) -> &Arc<Behaviours> {
        &self.behaviours
    }

    /// The server-wide network id counter.
    pub fn network_ids(&self) -> &NetworkIds {
        &self.network_ids
    }

    /// The entities, the network ids and the level random at once.
    pub fn entity_parts(&mut self) -> (&mut Entities, &NetworkIds, &mut LegacyRandom) {
        (&mut self.entities, &self.network_ids, &mut self.random)
    }

    /// The server-wide network id counter, shared.
    pub fn network_ids_arc(&self) -> &Arc<NetworkIds> {
        &self.network_ids
    }

    /// The level random (`level.getRandom()`).
    pub fn random(&mut self) -> &mut LegacyRandom {
        &mut self.random
    }

    /// The level random, to read (e.g. to draw ahead on a copy).
    pub fn random_ref(&self) -> &LegacyRandom {
        &self.random
    }

    /// Lends the level random out for an [`Env`](crate::survival::player::Env) while the level is
    /// borrowed elsewhere (entity ticks take `&Level` and draw `level.getRandom()` through the
    /// `Env`); [`Level::restore_random`] gives it back. Nothing may draw from the level's own
    /// random in between: it holds a marker that `restore_random` checks.
    pub fn lend_random(&mut self) -> LegacyRandom {
        std::mem::replace(&mut self.random, LegacyRandom::from_state(LENT_RANDOM))
    }

    /// Gives back the random of [`Level::lend_random`].
    pub fn restore_random(&mut self, random: LegacyRandom) {
        debug_assert_eq!(
            self.random.state(),
            LENT_RANDOM,
            "the level random was drawn from while lent out"
        );
        self.random = random;
    }

    /// Shares the server's random sequences (every world of a server draws from the same).
    pub fn set_random_sequences(
        &mut self,
        sequences: Arc<std::sync::Mutex<crate::random_sequences::RandomSequences>>,
    ) {
        self.random_sequences = sequences;
    }

    /// The server's random sequences.
    pub fn random_sequences(
        &self,
    ) -> &Arc<std::sync::Mutex<crate::random_sequences::RandomSequences>> {
        &self.random_sequences
    }

    /// Sets `randValue`, the LCG of random tick positions (parity scenarios start it from a
    /// known value).
    pub fn set_rand_value(&mut self, value: i32) {
        self.rand_value = value;
    }

    /// Sets the rain and thunder levels (the world's weather after `advanceWeatherCycle`).
    pub fn set_weather(&mut self, weather: crate::survival::weather::LevelWeather) {
        self.weather = weather;
    }

    /// The rain and thunder levels.
    pub fn weather(&self) -> crate::survival::weather::LevelWeather {
        self.weather
    }

    /// `isRaining`.
    pub fn is_raining(&self) -> bool {
        self.weather.is_raining()
    }

    /// `isThundering`.
    pub fn is_thundering(&self) -> bool {
        self.weather.is_thundering()
    }

    /// `Level.precipitationAt(pos)`: what falls at `pos` now.
    pub fn precipitation_at(&self, pos: BlockPos) -> crate::block::weather::Precipitation {
        use crate::block::weather::Precipitation;
        if !self.is_raining() || self.light(pos).0 < 15 {
            return Precipitation::None;
        }
        if self.heightmap_y(vanillite_chunk::HeightmapKind::MotionBlocking, pos) > pos.y {
            return Precipitation::None;
        }
        crate::block::weather::biome_precipitation(self, pos, pos)
    }

    /// `Level.isRainingAt(pos)`.
    pub fn is_raining_at(&self, pos: BlockPos) -> bool {
        self.precipitation_at(pos) == crate::block::weather::Precipitation::Rain
    }

    /// `getHeightmapPos(kind, pos).getY()`: the first free y of the column; the world's bottom
    /// outside live chunks.
    pub fn heightmap_y(&self, kind: vanillite_chunk::HeightmapKind, pos: BlockPos) -> i32 {
        self.chunks
            .chunk(pos.chunk())
            .and_then(|c| {
                let map = c.chunk.heightmaps().get(kind)?;
                let relative = map.get((pos.x & 15) as u32, (pos.z & 15) as u32);
                Some(c.chunk.min_y() + relative as i32)
            })
            .unwrap_or(self.min_y)
    }

    /// `getRandomSequence(key)`: runs `f` with the named random of loot tables.
    pub fn with_random_sequence<R>(
        &self,
        key: &str,
        f: impl FnOnce(&mut vanillite_base::random::XoroshiroRandom) -> R,
    ) -> R {
        let mut sequences = self
            .random_sequences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(sequences.get(key))
    }

    /// The game time.
    pub fn game_time(&self) -> i64 {
        self.game_time
    }

    /// `ServerLevel.isHandlingTick()`: between the start of the tick and the end of the block
    /// events.
    pub fn is_handling_tick(&self) -> bool {
        self.handling_tick
    }

    /// Sets `handlingTick`: `true` from the start of the world tick until its block events ran
    /// (`ServerLevel.tick`), `false` while players' packets are handled.
    pub fn set_handling_tick(&mut self, handling: bool) {
        self.handling_tick = handling;
    }

    /// Lowest block Y.
    pub fn min_y(&self) -> i32 {
        self.min_y
    }

    /// Highest block Y.
    pub fn max_y(&self) -> i32 {
        self.max_y
    }

    /// `isInsideBuildHeight` / `isInValidBounds` for Y.
    #[inline]
    pub fn in_height(&self, y: i32) -> bool {
        y >= self.min_y && y <= self.max_y
    }

    /// What to send to clients; the server drains it after the tick.
    pub fn output(&mut self) -> &mut LevelOutput {
        &mut self.output
    }

    // --- Reading -------------------------------------------------------------------------

    /// `getBlockState(pos)` of the world thread (the tick view, see [`view`]): void air below
    /// and above the world, air in chunks that are not live.
    #[inline(always)]
    pub fn block(&self, pos: BlockPos) -> BlockState {
        match self.chunks.block(pos) {
            Some(state) => BlockState(state),
            None if self.in_height(pos.y) => BlockState::AIR,
            None => block::VOID_AIR,
        }
    }

    /// [`Level::block`] if the position is loaded ([`Level::is_loaded`]), with one chunk
    /// lookup inside the build height.
    #[inline]
    pub fn loaded_block(&self, pos: BlockPos) -> Option<BlockState> {
        if self.in_height(pos.y) {
            self.chunks.accessible_block(pos).map(BlockState)
        } else {
            self.is_loaded(pos).then_some(block::VOID_AIR)
        }
    }

    /// The pure read view ([`view`]): reads without side effects for collision, path finding
    /// and code on other threads.
    #[inline]
    pub fn view(&self) -> view::LevelView<'_> {
        view::LevelView::new(
            self.chunks.view(),
            &self.border,
            &self.block_entities,
            self.min_y,
            self.max_y,
        )
    }

    /// `getBlockState(pos)` with the chunk loaded first like Vanilla: a chunk that is not
    /// accessible is loaded or generated synchronously and stays accessible for a tick
    /// ([`Level::load_chunk`]). [`Level::block`] reads without loading.
    pub fn block_loading(&mut self, pos: BlockPos) -> BlockState {
        if self.in_height(pos.y) {
            if let Some(state) = self.chunks.loaded_now_block(pos) {
                return BlockState(state);
            }
            self.load_chunk(pos.chunk());
        }
        self.block(pos)
    }

    /// `getChunk(x, z)` (`ServerChunkCache.getChunk(x, z, FULL, true)`): a chunk that is not
    /// accessible gets a temporary `unknown` ticket and is loaded or generated while the
    /// world thread waits; chunks that became live get their block entities.
    #[inline]
    pub fn load_chunk(&mut self, chunk: ChunkPos) {
        if self.chunks.load_now(chunk) {
            let mut promoted = Vec::new();
            self.chunks.take_promoted(&mut promoted);
            for &chunk in &promoted {
                self.take_generation_entities(chunk);
                self.load_block_entities(chunk);
            }
            self.chunks_loaded_event(&promoted);
        }
    }

    /// `getFluidState(pos)`.
    #[inline]
    pub fn fluid(&self, pos: BlockPos) -> FluidState {
        self.block(pos).fluid_state()
    }

    /// `isLoaded(pos)` / `hasChunkAt`: the position is in an accessible full chunk (level up
    /// to full).
    #[inline]
    pub fn is_loaded(&self, pos: BlockPos) -> bool {
        self.chunks.is_accessible(pos.chunk())
    }

    /// `getBrightness(LightLayer.SKY / BLOCK, pos)` as `(sky, block)`.
    pub fn light(&self, pos: BlockPos) -> (i32, i32) {
        let Some(chunk) = self.chunks.chunk(pos.chunk()) else {
            return (if self.settings.has_sky_light { 15 } else { 0 }, 0);
        };
        let light = chunk.chunk.light();
        let index = (pos.y >> 4) - (chunk.chunk.min_section() - 1);
        let (x, y, z) = (
            (pos.x & 15) as u32,
            (pos.y & 15) as u32,
            (pos.z & 15) as u32,
        );
        let Ok(index) = usize::try_from(index) else {
            return (0, 0);
        };
        let block = light
            .block()
            .get(index)
            .map_or(0, |l| i32::from(l.get(x, y, z)));
        let sky = if !self.settings.has_sky_light {
            0
        } else if index >= light.len() {
            15
        } else {
            // Sections without sky data read like the data above them: full light above the
            // highest data, as the client does.
            light.sky()[index..]
                .iter()
                .find(|l| !matches!(l, vanillite_chunk::LightNibbles::Absent))
                .map_or(15, |l| {
                    if std::ptr::eq(l, &light.sky()[index]) {
                        i32::from(l.get(x, y, z))
                    } else {
                        i32::from(l.get(x, 0, z))
                    }
                })
        };
        (sky, block)
    }

    /// `getRawBrightness(pos, 0)`.
    pub fn raw_brightness(&self, pos: BlockPos) -> i32 {
        let (sky, block) = self.light(pos);
        sky.max(block)
    }

    /// `getMaxLocalRawBrightness(pos)`: like [`Self::raw_brightness`] with the sky darkened by
    /// this tick's `skyDarken` (the time of day and the weather).
    pub fn max_local_raw_brightness(&self, pos: BlockPos) -> i32 {
        let (sky, block) = self.light(pos);
        (sky - self.spawner_env.env.sky_darken).max(block)
    }

    // --- Scheduled ticks and events --------------------------------------------------------

    /// `scheduleTick(pos, block, delay)`.
    pub fn schedule_block_tick(&mut self, pos: BlockPos, block: BlockId, delay: i32) {
        self.chunks
            .schedule_block_tick(pos, block.0, delay, TickPriority::NORMAL);
    }

    /// `scheduleTick(pos, block, delay, priority)`.
    pub fn schedule_block_tick_with(
        &mut self,
        pos: BlockPos,
        block: BlockId,
        delay: i32,
        priority: TickPriority,
    ) {
        self.chunks
            .schedule_block_tick(pos, block.0, delay, priority);
    }

    /// `scheduleTick(pos, fluid, delay)`; `fluid` is a fluid id.
    pub fn schedule_fluid_tick(&mut self, pos: BlockPos, fluid: u16, delay: i32) {
        self.chunks
            .schedule_fluid_tick(pos, fluid, delay, TickPriority::NORMAL);
    }

    /// `blockEvent(pos, block, a, b)`: queued unless the same event is queued already.
    pub fn block_event(&mut self, pos: BlockPos, block: BlockId, a: i32, b: i32) {
        let event = BlockEvent { pos, block, a, b };
        if self.block_event_set.insert(event) {
            self.block_events.push(event);
        }
    }

    /// `playSound(except, pos, sound, source, volume, pitch)` at the block's centre.
    pub fn play_block_sound(
        &mut self,
        except: Option<EntityId>,
        pos: BlockPos,
        sound: u16,
        source: u8,
        volume: f32,
        pitch: f32,
    ) {
        let (x, y, z) = (
            f64::from(pos.x) + 0.5,
            f64::from(pos.y) + 0.5,
            f64::from(pos.z) + 0.5,
        );
        self.play_sound(except, x, y, z, sound, source, volume, pitch);
    }

    /// `playSound(except, x, y, z, sound, source, volume, pitch)`.
    #[allow(clippy::too_many_arguments)]
    pub fn play_sound(
        &mut self,
        except: Option<EntityId>,
        x: f64,
        y: f64,
        z: f64,
        sound: u16,
        source: u8,
        volume: f32,
        pitch: f32,
    ) {
        let seed = self.sound_seeds.next_long();
        self.play_seeded_sound(except, x, y, z, sound, source, volume, pitch, seed);
    }

    /// `playSeededSound(except, x, y, z, sound, source, volume, pitch, seed)`.
    #[allow(clippy::too_many_arguments)]
    pub fn play_seeded_sound(
        &mut self,
        except: Option<EntityId>,
        x: f64,
        y: f64,
        z: f64,
        sound: u16,
        source: u8,
        volume: f32,
        pitch: f32,
        seed: i64,
    ) {
        self.output.sounds.push(Sound {
            sound,
            source,
            x,
            y,
            z,
            volume,
            pitch,
            seed,
            except,
        });
    }

    /// `playSound(null, entity, sound, source, volume, pitch)`: a sound that follows the entity
    /// on the client (`ClientboundSoundEntityPacket`).
    pub fn play_entity_sound(
        &mut self,
        entity: EntityId,
        sound: &str,
        source: vanillite_protocol::packets::play::clientbound::SoundSource,
        volume: f32,
        pitch: f32,
    ) {
        let Some((pos, entity_id)) = self
            .entities
            .store
            .get(entity)
            .map(|e| (e.pos(), e.network_id().0))
        else {
            return;
        };
        let packet = vanillite_protocol::SharedPacket::of(
            &vanillite_protocol::packets::play::clientbound::SoundEntity {
                sound: crate::survival::sound_event(sound),
                source,
                entity_id,
                volume,
                pitch,
                seed: self.sound_seeds.next_long(),
            },
        );
        let range = f64::from(if volume > 1.0 { 16.0 * volume } else { 16.0 });
        self.output().nearby_packets.push((pos, range, packet));
    }

    /// `globalLevelEvent(event, pos, data)`.
    pub fn global_level_event(&mut self, event: i32, pos: BlockPos, data: i32) {
        self.output.global_level_events.push((event, pos, data));
    }

    /// `levelEvent(source, event, pos, data)`.
    pub fn level_event(&mut self, source: Option<EntityId>, event: i32, pos: BlockPos, data: i32) {
        self.output.level_events.push(LevelEvent {
            event,
            pos,
            data,
            source,
        });
    }

    // --- Writing -------------------------------------------------------------------------

    /// `setBlock(pos, state, flags)` with Vanilla's update depth.
    pub fn set_block(&mut self, pos: BlockPos, state: BlockState, flags: u32) -> bool {
        self.set_block_depth(pos, state, flags, UPDATE_LIMIT)
    }

    /// `setBlockAndUpdate(pos, state)`: [`update::ALL`].
    pub fn set_block_and_update(&mut self, pos: BlockPos, state: BlockState) -> bool {
        self.set_block(pos, state, update::ALL)
    }

    /// `setBlock(pos, state, flags, updateLimit)` (`Level.setBlock` with
    /// `LevelChunk.setBlockState`). Returns whether the block was set.
    pub fn set_block_depth(
        &mut self,
        pos: BlockPos,
        state: BlockState,
        flags: u32,
        limit: i32,
    ) -> bool {
        if !self.in_height(pos.y) {
            return false;
        }
        // `getChunkAt(pos)` loads the chunk.
        self.load_chunk(pos.chunk());
        // `LevelChunk.setBlockState`.
        let Some(old) = self.chunks.set_block(pos, state.0).map(BlockState) else {
            return false;
        };
        let new_block = state.block();
        let block_changed = old.block() != new_block;
        let moved = flags & update::MOVE_BY_PISTON != 0;
        self.block_entity_removed(old, state, pos, flags);
        if (block_changed || block::is_rail(new_block)) && (flags & update::NEIGHBORS != 0 || moved)
        {
            self.affect_neighbors_after_removal(old, pos, moved);
        }
        if self.block(pos).block() != new_block {
            return false;
        }
        if flags & update::SKIP_ON_PLACE == 0 {
            self.on_place(state, pos, old, moved);
        }
        if self.block(pos).block() == new_block {
            self.block_entity_placed(state, pos);
        }
        // `Level.setBlock`.
        if self.block(pos) == state {
            if flags & update::CLIENTS != 0 {
                self.send_block_updated(pos);
                if old.collision_shape() != state.collision_shape() {
                    crate::mob::nav::block_shape_changed(self, pos);
                }
            }
            if flags & update::NEIGHBORS != 0 {
                self.update_neighbors_at(pos, old.block());
                if Self::has_analog_output(state) {
                    self.update_neighbour_for_output_signal(pos, new_block);
                }
            }
            if flags & update::KNOWN_SHAPE == 0 && limit > 0 {
                let neighbour_flags = flags & !(update::NEIGHBORS | update::SUPPRESS_DROPS);
                self.update_indirect_neighbour_shapes(old, pos, neighbour_flags, limit - 1);
                self.update_neighbour_shapes(state, pos, neighbour_flags, limit - 1);
                self.update_indirect_neighbour_shapes(state, pos, neighbour_flags, limit - 1);
            }
            // `updatePOIOnBlockStateChange`.
            let (old_poi, new_poi) = (
                crate::poi::PoiType::of_state(old),
                crate::poi::PoiType::of_state(state),
            );
            if old_poi != new_poi {
                self.poi_changes.push((pos, old_poi, new_poi));
                self.flush_poi_changes();
            }
        }
        true
    }

    /// Applies the POI changes of block changes to the POI manager (`remove`, then `add`, as
    /// the server tasks of `updatePOIOnBlockStateChange` run); kept while the entities are out
    /// of the level.
    pub fn flush_poi_changes(&mut self) {
        if self.poi_changes.is_empty() || !self.entities.poi.live {
            return;
        }
        for (pos, old, new) in std::mem::take(&mut self.poi_changes) {
            if old.is_some() {
                self.entities.poi.remove(pos);
            }
            if let Some(ty) = new {
                self.entities.poi.add(pos, ty);
            }
        }
    }

    /// `removeBlock(pos, movedByPiston)`: the block becomes the fluid it holds.
    pub fn remove_block(&mut self, pos: BlockPos, moved: bool) -> bool {
        let fluid = self.fluid(pos);
        let flags = update::ALL | if moved { update::MOVE_BY_PISTON } else { 0 };
        self.set_block(pos, fluid.block_state(), flags)
    }

    /// `destroyBlock(pos, drop, breaker)`.
    pub fn destroy_block(&mut self, pos: BlockPos, drop: bool, breaker: Option<EntityId>) -> bool {
        self.destroy_block_depth(pos, drop, breaker, UPDATE_LIMIT)
    }

    /// `destroyBlock(pos, drop, breaker, updateLimit)`: break particles and sound, drops, then
    /// the block becomes the fluid it holds.
    pub fn destroy_block_depth(
        &mut self,
        pos: BlockPos,
        drop: bool,
        breaker: Option<EntityId>,
        limit: i32,
    ) -> bool {
        let state = self.block(pos);
        if state.is_air() {
            return false;
        }
        // `spawnDestroyParticles`: `LevelEvent.PARTICLES_DESTROY_BLOCK` (none for fire; the
        // straw bed sends its particles without the break sound).
        if state.is(vanillite_data::block::ids::STRAW_BED) {
            self.level_event(None, 2014, pos, i32::from(state.0));
        } else if !block::is_fire(state) {
            self.level_event(None, 2001, pos, i32::from(state.0));
        }
        if drop {
            block::drops::drop_resources(self, pos, state, breaker);
        }
        let fluid = self.fluid(pos);
        let destroyed = self.set_block_depth(pos, fluid.block_state(), update::ALL, limit);
        if destroyed {
            let context = game_event::GameEventContext::with_state(breaker, state);
            self.game_event_block(game_event::events().block_destroy, pos, context);
        }
        destroyed
    }

    /// `sendBlockUpdated`: the position goes to the clients that see its chunk.
    pub fn send_block_updated(&mut self, pos: BlockPos) {
        self.output
            .changed
            .entry(pos.section())
            .or_default()
            .insert(pos);
    }

    // --- Updates -------------------------------------------------------------------------

    /// `updateNeighborsAt(pos, block)`: neighbour updates of the six neighbours in
    /// `NeighborUpdater.UPDATE_ORDER`.
    pub fn update_neighbors_at(&mut self, pos: BlockPos, source: BlockId) {
        self.update_neighbors_at_except(pos, source, None);
    }

    /// `updateNeighborsAtExceptFromFacing(pos, block, skip)`.
    pub fn update_neighbors_at_except(
        &mut self,
        pos: BlockPos,
        source: BlockId,
        skip: Option<Direction>,
    ) {
        self.add_and_run(neighbors::Update::multi(pos, source, skip));
    }

    /// `neighborChanged(pos, block)`.
    pub fn neighbor_changed(&mut self, pos: BlockPos, source: BlockId) {
        self.add_and_run(neighbors::Update::Simple { pos, source });
    }

    /// `neighborShapeChanged(direction, pos, neighbourPos, neighbourState, flags, limit)`.
    pub fn neighbor_shape_changed(
        &mut self,
        direction: Direction,
        pos: BlockPos,
        neighbour_pos: BlockPos,
        neighbour_state: BlockState,
        flags: u32,
        limit: i32,
    ) {
        self.add_and_run(neighbors::Update::Shape {
            direction,
            neighbour_state,
            pos,
            neighbour_pos,
            flags,
            limit,
        });
    }

    /// `state.updateNeighbourShapes(level, pos, flags, limit)`: every neighbour in
    /// `UPDATE_SHAPE_ORDER` learns that `state` is now at `pos`.
    pub fn update_neighbour_shapes(
        &mut self,
        state: BlockState,
        pos: BlockPos,
        flags: u32,
        limit: i32,
    ) {
        for direction in shared::UPDATE_SHAPE_ORDER {
            let neighbour = pos.relative(direction);
            self.neighbor_shape_changed(direction.opposite(), neighbour, pos, state, flags, limit);
        }
    }

    /// `state.updateIndirectNeighbourShapes(level, pos, flags, limit)`: only redstone wire has
    /// them (`RedstoneWireBlock.updateIndirectNeighbourShapes`): wire diagonally above and below
    /// a connected side learns about the block next to it.
    pub fn update_indirect_neighbour_shapes(
        &mut self,
        state: BlockState,
        pos: BlockPos,
        flags: u32,
        limit: i32,
    ) {
        if !state.is(vanillite_data::block::ids::REDSTONE_WIRE) {
            return;
        }
        let wire = shared::wire_properties();
        for direction in shared::HORIZONTAL {
            if !wire.connected(state, direction) {
                continue;
            }
            let side = pos.relative(direction);
            if self
                .block(side)
                .is(vanillite_data::block::ids::REDSTONE_WIRE)
            {
                continue;
            }
            for wire in [side.offset(0, -1, 0), side.offset(0, 1, 0)] {
                if self
                    .block(wire)
                    .is(vanillite_data::block::ids::REDSTONE_WIRE)
                {
                    let neighbour = wire.relative(direction.opposite());
                    let neighbour_state = self.block(neighbour);
                    self.neighbor_shape_changed(
                        direction.opposite(),
                        wire,
                        neighbour,
                        neighbour_state,
                        flags,
                        limit,
                    );
                }
            }
        }
    }

    /// `Block.updateOrDestroy(old, new, level, pos, flags, limit)`.
    pub fn update_or_destroy(
        &mut self,
        old: BlockState,
        new: BlockState,
        pos: BlockPos,
        flags: u32,
        limit: i32,
    ) {
        if new == old {
            return;
        }
        if new.is_air() {
            self.destroy_block_depth(pos, flags & update::SUPPRESS_DROPS == 0, None, limit);
        } else {
            self.set_block_depth(pos, new, flags & !update::SUPPRESS_DROPS, limit);
        }
    }

    /// `state.updateShape(...)` against this level.
    pub fn update_shape(
        &mut self,
        state: BlockState,
        pos: BlockPos,
        direction: Direction,
        neighbour_pos: BlockPos,
        neighbour_state: BlockState,
    ) -> BlockState {
        if block::beehive::is_beehive(state.block()) {
            block::beehive::neighbour_changed_shape(self, pos, state, neighbour_pos);
        }
        let new = shared::update_shape(self, state, pos, direction, neighbour_pos, neighbour_state);
        // `PotentSulfurBlock.validBlockState`: a block that becomes a periodic geyser starts a
        // new countdown.
        if state.block() == vanillite_data::block::ids::POTENT_SULFUR
            && !matches!(
                state.value("potent_sulfur_state"),
                Some("erupting" | "dormant")
            )
            && new.value("potent_sulfur_state") == Some("dormant")
            && let Some(BlockEntity::PotentSulfur { countdown }) = self.block_entity_mut(pos)
        {
            *countdown = -1;
        }
        new
    }

    /// `state.canSurvive(level, pos)`.
    pub fn can_survive(&self, state: BlockState, pos: BlockPos) -> bool {
        shared::can_survive(self, state, pos)
    }

    // --- Behaviour dispatch --------------------------------------------------------------

    fn on_place(&mut self, state: BlockState, pos: BlockPos, old: BlockState, moved: bool) {
        if let Some(f) = self.behaviours.get(state.block()).on_place {
            f(self, state, pos, old, moved);
        }
    }

    /// `state.useWithoutItem(level, pos, player, hitResult)` of the block at `pos`.
    pub fn use_without_item(
        &mut self,
        pos: BlockPos,
        context: &block::UseContext,
    ) -> block::Interaction {
        let state = self.block(pos);
        match self.behaviours.get(state.block()).use_without_item {
            Some(f) => f(self, state, pos, context),
            None => block::Interaction::Pass,
        }
    }

    /// Whether the block of `state` reacts to entities inside it (`entityInside`).
    pub fn has_entity_inside(&self, state: BlockState) -> bool {
        self.behaviours.get(state.block()).entity_inside.is_some()
    }

    /// `state.entityInside(level, pos, entity, ...)` of the block at `pos`.
    pub fn entity_inside(&mut self, pos: BlockPos, entity: EntityId) {
        let state = self.block(pos);
        if let Some(f) = self.behaviours.get(state.block()).entity_inside {
            f(self, state, pos, entity);
        }
    }

    /// `ServerLevel.updateNeighboursOnBlockSet(pos, oldState)`: the neighbour updates a command
    /// skipped while placing (`setblock`, `fill`, `clone`).
    pub fn update_neighbours_on_block_set(&mut self, pos: BlockPos, old: BlockState) {
        let state = self.block(pos);
        let block = state.block();
        if old.block() != block {
            self.affect_neighbors_after_removal(old, pos, false);
        }
        self.update_neighbors_at(pos, block);
        if Self::has_analog_output(state) {
            self.update_neighbour_for_output_signal(pos, block);
        }
    }

    /// `state.affectNeighborsAfterRemoval(level, pos, movedByPiston)`.
    pub(crate) fn affect_neighbors_after_removal(
        &mut self,
        state: BlockState,
        pos: BlockPos,
        moved: bool,
    ) {
        if let Some(f) = self
            .behaviours
            .get(state.block())
            .affect_neighbors_after_removal
        {
            f(self, state, pos, moved);
        }
    }

    /// `state.handleNeighborChanged(level, pos, block, orientation, movedByPiston)`.
    fn handle_neighbor_changed(
        &mut self,
        state: BlockState,
        pos: BlockPos,
        source: BlockId,
        moved: bool,
    ) {
        if let Some(f) = self.behaviours.get(state.block()).neighbor_changed {
            f(self, state, pos, source, moved);
        }
    }

    /// Restarts the seeds of new entities' randoms from `seed` (on this thread, see
    /// [`crate::mob::set_entity_seed_stream`]).
    pub fn set_entity_seed(&mut self, seed: i64) {
        crate::mob::set_entity_seed_stream(Some(seed));
    }

    /// The own random of a new entity (`Entity.random`); every entity the level creates
    /// takes one, in creation order.
    pub fn new_entity_random(&mut self) -> LegacyRandom {
        LegacyRandom::new(crate::mob::new_entity_seed())
    }

    /// A random UUID for a new entity (`Mth.createInsecureUUID(entity.random)`): drawn from a
    /// random of its own, as Vanilla's entities draw it from theirs, so spawning entities does
    /// not shift the level random.
    pub fn new_uuid(&mut self) -> u128 {
        self.new_uuid_and_seed().0
    }

    /// [`Self::new_uuid`] with the seed of the new entity's own random.
    pub fn new_uuid_and_seed(&mut self) -> (u128, i64) {
        // A new entity: it takes its seed like Vanilla's constructor.
        let seed = crate::mob::new_entity_seed();
        let most = self.sound_seeds.next_long() as u64;
        let least = self.sound_seeds.next_long() as u64;
        // Version 4, IETF variant.
        let most = (most & !0xF000) | 0x4000;
        let least = (least & !(0xC000_0000_0000_0000)) | 0x8000_0000_0000_0000;
        ((u128::from(most) << 64) | u128::from(least), seed)
    }

    /// Whether `pos` lies in a chunk where blocks tick.
    pub fn is_block_ticking(&self, chunk: ChunkPos) -> bool {
        self.chunks.is_block_ticking(chunk)
    }
}

impl BlockLevel for Level {
    #[inline]
    fn block(&self, pos: BlockPos) -> BlockState {
        Level::block(self, pos)
    }

    fn moving_piston_boxes(&self, pos: BlockPos, out: &mut Vec<[f64; 6]>) {
        block::piston::collision_boxes_at(&self.block_entities, pos, out);
    }

    fn min_y(&self) -> i32 {
        Level::min_y(self)
    }

    fn raw_brightness(&self, pos: BlockPos) -> i32 {
        Level::raw_brightness(self, pos)
    }

    fn random_int(&mut self, bound: i32) -> i32 {
        self.random.next_int_bounded(bound)
    }

    fn schedule_block_tick(&mut self, pos: BlockPos, block: BlockId, delay: i32) {
        Level::schedule_block_tick(self, pos, block, delay);
    }

    fn schedule_fluid_tick(&mut self, pos: BlockPos, fluid: u16) {
        let delay = block::fluid::tick_delay(self, fluid);
        Level::schedule_fluid_tick(self, pos, fluid, delay);
    }

    fn has_block_tick(&self, pos: BlockPos, block: BlockId) -> bool {
        self.chunks.has_block_tick(pos, block.0)
    }

    fn comparator_output(&self, pos: BlockPos) -> i32 {
        Level::comparator_output(self, pos)
    }
}
