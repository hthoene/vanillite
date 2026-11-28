//! The scenario world on the real simulation: a headless [`Level`] over flat chunks.
//!
//! The world matches the Vanilla recorder (`ParityServer` and `ScenarioRecorder`): flat layers
//! from the bottom of the overworld, the scenario's game rules on top of the recorder's
//! defaults, and the region's chunks plus one chunk around them forced (entity ticking). A
//! step runs the simulation phases of `ServerLevel.tick` in Vanilla's order, then the
//! connection tick of the players (`ServerPlayer.doTick`). Chunks are saved to and loaded from
//! memory in the Anvil format, so unloading and loading them goes through the real chunk and
//! entity records.
//!
//! Entities: the mob, arrow and orb tick and what it queues (explosions, block changes, drops)
//! work like `vanillite-server`'s world (`mobs.rs`, `survival.rs`); keep them in step. The
//! survival random of the server world is the level random here, as in Vanilla (one
//! `Level.random`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value, json};
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, ChunkPos, Vec3};
use vanillite_chunk::ProtoChunk;
use vanillite_chunk::light::{ChunkLight, LightNibbles};
use vanillite_chunk::slots::{
    FlatGenerator, LightEngine, LightEngineProvider, MemoryStore, Neighbours, WorldInfo,
};
use vanillite_data::block::{BlockId, BlockState};
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_game::block::Behaviours;
use vanillite_game::block::weather::Climate;
use vanillite_game::entities::{Entities, is_mob_kind};
use vanillite_game::item_entity::ItemData;
use vanillite_game::level::explosion::BlockInteraction;
use vanillite_game::level::forced::SavedTickets;
use vanillite_game::level::{Level, LevelOutput, LevelSettings, update};
use vanillite_game::lightning;
use vanillite_game::minecart::{self, MinecartKind};
use vanillite_game::mob::common::CommonGoal;
use vanillite_game::mob::fam::FamGoal;
use vanillite_game::mob::goals::{Goal, MeleeKind, Slot};
use vanillite_game::mob::movement::{MoveGoal, RemoveSounds};
use vanillite_game::mob::{
    self, Environment, Kind, MobContext, MobData, MobSettings, WorldAction, combat, kinds,
    projectile,
};
use vanillite_game::player::chat::{ChatInbox, ChatSettings};
use vanillite_game::player::connection::Connection;
use vanillite_game::player::sink::PacketSink;
use vanillite_game::player::{ChunkView, ClientInformation, GameProfile, PlayerData};
use vanillite_game::survival::Effect;
use vanillite_game::survival::blocks::{BlockAccess, BlockFacts};
use vanillite_game::survival::damage::{DamageSource, DamageTypes};
use vanillite_game::survival::player::{self as survival_player, Env};
use vanillite_game::survival::rules::{Difficulty, GameRules, Rule};
use vanillite_game::survival::weather::{LevelWeather, WeatherData};
use vanillite_protocol::SharedPacket;
use vanillite_storage::{AnvilCodec, Compression};
use vanillite_world::chunks::{ChunkSettings, ChunkSlots, Chunks, InlineExecutor, Visibility};
use vanillite_world::entity::{EntityId, NetworkIds, Rotation};
use vanillite_world::ticks::LevelTicks;

use crate::scenario::{EntitySpec, Hurt, WorldSpec};
use crate::trace::{EntityObs, TickObs};
use crate::world::{Execution, ScenarioWorld};

/// Overworld height range.
const MIN_Y: i32 = -64;
const HEIGHT: u32 = 384;
/// Chunk runtime ticks [`GameWorld::set_forced`] runs at most to settle.
const SETTLE_LIMIT: usize = 1000;

/// No lighting, for scenarios that do not need it (see [`WorldSpec::light`]).
struct NoLight;

impl LightEngine for NoLight {
    fn initialize_light(&self, _: &mut [ProtoChunk]) {}
    fn light_chunks(&self, _: &mut [ProtoChunk], _: &mut Neighbours<'_>) {}
    fn relight(&self, _: &[BlockPos], _: &Neighbours<'_>) -> Vec<(ChunkPos, ChunkLight)> {
        Vec::new()
    }
}

/// A connection nobody reads.
struct NoSink;

impl PacketSink for NoSink {
    fn send_body(&self, _: Vec<u8>) -> bool {
        true
    }
    fn send_shared(&self, _: &Arc<SharedPacket>) -> bool {
        true
    }
    fn close(&self) {}
    fn is_closed(&self) -> bool {
        false
    }
    fn latency_ms(&self) -> u32 {
        0
    }
}

/// Blocks of the level's chunks (and the moving pistons' block entities) for survival code.
struct ChunkBlocks<'a>(
    &'a Chunks,
    &'a vanillite_game::level::block_entity::BlockEntities,
);

impl BlockAccess for ChunkBlocks<'_> {
    fn block_state(&self, pos: BlockPos) -> Option<BlockState> {
        self.0.block(pos).map(BlockState)
    }

    fn moving_piston_boxes(&self, pos: BlockPos, out: &mut Vec<[f64; 6]>) {
        vanillite_game::block::piston::collision_boxes_at(self.1, pos, out);
    }
}

/// Damage types and block facts of the Vanilla data pack.
static PACK: OnceLock<(DamageTypes, BlockFacts)> = OnceLock::new();
/// The biomes of the Vanilla data pack (temperatures, precipitation).
static BIOMES: OnceLock<vanillite_worldgen::biome::BiomeRegistry> = OnceLock::new();
/// The biome ids by protocol id.
static BIOME_NAMES: OnceLock<Arc<[String]>> = OnceLock::new();
/// The trees saplings grow into (they do not depend on the seed).
static TREES: OnceLock<Arc<vanillite_game::block::tree::TreeFeatures>> = OnceLock::new();

/// Sea level of Vanilla's flat worlds (`FlatLevelSource.getSeaLevel`).
pub(crate) const FLAT_SEA_LEVEL: i32 = -63;
/// Sea level of the normal overworld (snapshot worlds).
const OVERWORLD_SEA_LEVEL: i32 = 63;
/// The Vanilla data pack, for the noise generator of snapshot worlds.
static PACKS: OnceLock<vanillite_datapack::LoadedPacks> = OnceLock::new();

/// The sea level of a scenario world.
fn sea_level(spec: &WorldSpec) -> i32 {
    if spec.snapshot.is_some() {
        OVERWORLD_SEA_LEVEL
    } else {
        FLAT_SEA_LEVEL
    }
}

/// Chunks around the forced ones whose records a snapshot world takes from its files.
const SNAPSHOT_MARGIN: i32 = 4;

/// Puts the chunk and POI records of the snapshot's region files around the forced chunks
/// into `store` (the entities stay out, see the scenario documentation).
fn preload_snapshot(
    store: &MemoryStore,
    dir: &Path,
    forced: &[ChunkPos],
    entities: bool,
) -> Result<(), String> {
    use vanillite_chunk::store::{
        ChunkStore, CompletionTarget, DimensionId, Priority, RecordKey, RecordMeta, StoreOp, Token,
    };
    if !dir.join("region").is_dir() {
        return Err(format!("{}: no region folder", dir.display()));
    }
    let (Some(min_x), Some(max_x), Some(min_z), Some(max_z)) = (
        forced.iter().map(|c| c.x).min(),
        forced.iter().map(|c| c.x).max(),
        forced.iter().map(|c| c.z).min(),
        forced.iter().map(|c| c.z).max(),
    ) else {
        return Ok(());
    };
    let dimension = DimensionId(0);
    let mut ops = Vec::new();
    // `world.entities`: the snapshot's entities too.
    let folders: &[(&str, u8)] = if entities {
        &[("region", 0), ("poi", 1), ("entities", 2)]
    } else {
        &[("region", 0), ("poi", 1)]
    };
    for &(folder, kind) in folders {
        let mut files = std::collections::HashMap::new();
        for x in min_x - SNAPSHOT_MARGIN..=max_x + SNAPSHOT_MARGIN {
            for z in min_z - SNAPSHOT_MARGIN..=max_z + SNAPSHOT_MARGIN {
                let pos = ChunkPos::new(x, z);
                let region = (x >> 5, z >> 5);
                let file = match files.entry(region) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        let path = dir
                            .join(folder)
                            .join(format!("r.{}.{}.mca", region.0, region.1));
                        e.insert(
                            vanillite_storage::region::RegionFile::open(&path, false)
                                .map_err(|e| format!("{}: {e}", path.display()))?,
                        )
                    }
                };
                let Some(file) = file else {
                    continue;
                };
                let Some(record) = file
                    .read_record(pos)
                    .map_err(|e| format!("{folder} {pos:?}: {e}"))?
                else {
                    continue;
                };
                let key = match kind {
                    1 => RecordKey::poi(dimension, pos),
                    2 => RecordKey::entities(dimension, pos),
                    _ => RecordKey::chunk(dimension, pos),
                };
                ops.push(StoreOp::Put {
                    key,
                    meta: RecordMeta {
                        status: RecordMeta::STATUS_UNKNOWN,
                        ..RecordMeta::default()
                    },
                    bytes: record.into(),
                });
            }
        }
    }
    let (target, _done) = CompletionTarget::channel();
    store.submit(Token(0), ops, Priority::Background, target);
    Ok(())
}

/// Installs the chunk facts and the block tags behaviour reads from the Vanilla data pack
/// (extracted into `$VANILLITE_CACHE`, default `<workspace>/target/vanilla-cache`), once.
pub(crate) fn install() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let cache = std::env::var_os("VANILLITE_CACHE").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/vanilla-cache"),
            PathBuf::from,
        );
        let jar = std::env::var_os("VANILLITE_SERVER_JAR").map(PathBuf::from);
        let pack = vanillite_datapack::vanilla::ensure_vanilla_pack(&cache, jar.as_deref())
            .expect("Vanilla data pack (set VANILLITE_SERVER_JAR for an offline copy)");
        let packs = vanillite_datapack::load(&[pack.as_path()]).expect("load Vanilla data pack");
        // The heightmap tags and the biome count, as the server installs them.
        let tag = |name: &str| {
            packs
                .tag_ids("minecraft:block", name)
                .expect("Vanilla heightmap block tag")
        };
        let biomes = packs
            .registry("minecraft:worldgen/biome")
            .map_or(1, |r| r.entries().len());
        vanillite_chunk::facts::install_vanilla(
            &tag("minecraft:blocks_motion_in_heightmap"),
            &tag("minecraft:blocks_motion_in_heightmap_no_leaves"),
            u32::try_from(biomes).unwrap_or(u32::MAX),
        );
        vanillite_worldgen::behaviour::init(&packs);
        let _ = PACK.set((
            vanillite_server::load_damage_types(&packs),
            vanillite_server::load_block_facts(&packs),
        ));
        let _ = BIOMES
            .set(vanillite_worldgen::biome::BiomeRegistry::load(&packs).expect("Vanilla biomes"));
        let _ = TREES.set(Arc::new(
            vanillite_game::block::tree::TreeFeatures::load(
                &packs,
                BIOMES.get().expect("biomes first"),
                0,
                MIN_Y,
                HEIGHT as i32,
                FLAT_SEA_LEVEL,
            )
            .expect("Vanilla tree features"),
        ));
        let _ = BIOME_NAMES.set(
            packs
                .registry("minecraft:worldgen/biome")
                .map(|r| r.entries().iter().map(|e| e.id.clone()).collect())
                .expect("Vanilla biome registry"),
        );
        // The biomes' spawn settings and gameplay attributes, as the server installs them.
        if let Some(table) = vanillite_server::spawn_table(Some(&packs)) {
            vanillite_game::mob::spawn::install_table(Arc::new(table));
        }
        // Loot tables and item tags (block drops), as the server installs them.
        vanillite_game::player::play::install_game_data(Arc::new(vanillite_server::game_data(
            Some(&packs),
        )));
        let _ = PACKS.set(packs);
    });
}

/// The server of [`command_server`], once a scenario ran a command.
static COMMAND_SERVER: OnceLock<Arc<vanillite_server::Global>> = OnceLock::new();

/// The server whose command tree, scoreboard and storage `command` actions use: started once
/// without network in a temporary directory, with the Vanilla data pack of [`install`], and
/// never run (the commands run on the scenario's level).
fn command_server() -> &'static vanillite_server::Global {
    COMMAND_SERVER.get_or_init(|| {
        install();
        let dir = std::env::temp_dir().join(format!("vanillite-scenario-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scenario server directory");
        std::fs::write(dir.join("eula.txt"), "eula=true\n").expect("scenario server eula");
        let cache = std::env::var_os("VANILLITE_CACHE").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/vanilla-cache"),
            PathBuf::from,
        );
        let cache = std::fs::canonicalize(&cache).unwrap_or(cache);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&cache, dir.join("cache")).expect("link the pack cache");
        let mut builder = vanillite_server::ServerBuilder::new(&dir);
        builder
            .vanilla_pack(vanillite_server::VanillaPack::Download)
            .network(false);
        let server = builder.start().expect("scenario server starts");
        let global = server.global().arc().expect("server global");
        // Kept for the whole test run: its global outlives every scenario.
        std::mem::forget(server);
        global
    })
}

/// A headless level over flat chunks.
pub struct GameWorld {
    level: Level,
    /// The world's spec, to open it again on a restart.
    spec: WorldSpec,
    /// Where the chunks are saved, kept over restarts.
    store: Arc<MemoryStore>,
    /// Runs the light engine (scenarios that observe light).
    light: bool,
    network_ids: Arc<NetworkIds>,
    mobs: MobContext,
    rules: GameRules,
    difficulty: Difficulty,
    env: Environment,
    /// The server's weather timers (`WeatherData`).
    weather: WeatherData,
    effects: Vec<Effect>,
    /// Scenario names of entities.
    names: Vec<(EntityId, String)>,
    seed: i64,
    /// Frames observe the level random.
    level_rng: bool,
    /// The overworld clock (`time`), for the local difficulty of lightning strikes.
    clock_time: i64,
    /// The entities the ticking lightning bolt strikes.
    bolt_hits: Vec<EntityId>,
    item_scratch: vanillite_server::ItemScratch,
    /// The packet-level players, in join order.
    clients: Vec<Client>,
    /// Network ids of the scenario entities (they stay valid after an entity is gone).
    network_names: Vec<(String, i32)>,
}

/// A packet-level scenario player: its packets go into its connection's queue.
struct Client {
    id: EntityId,
    tx: vanillite_protocol::InboundSender,
    seq: u64,
    /// The client finished its join (accepted the first teleport, sent `player_loaded`).
    joined: bool,
}

/// Packets a client player may queue in one tick.
const CLIENT_QUEUE: usize = 4096;

/// Ticks until an item entity despawns (`ItemEntity.LIFETIME`).
const ITEM_LIFETIME: i16 = 6000;

impl std::fmt::Debug for GameWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameWorld").finish_non_exhaustive()
    }
}

/// The seed of every shuffled behaviour list's random in a scenario
/// (`ParityHooks.SHUFFLE_SEED`).
pub(crate) const PARITY_SHUFFLE_SEED: i64 = 0x5348_5546;

impl GameWorld {
    /// `ServerPlayerGameMode.useItemOn` (through `handleUseItemOn`) or `START_DESTROY_BLOCK`
    /// of a scenario player, on the real packet handlers.
    /// [`ScenarioWorld::player_use`]; `true` when the `use_item` packet handler took the use
    /// of an `air` click.
    fn click(&mut self, click: &crate::scenario::PlayerUse) -> Result<bool, String> {
        use vanillite_game::player::play::{Actor, PlayContext};
        use vanillite_protocol::packets::play::serverbound::{DigAction, Hand, UseItemOn};
        let id = self.named(&click.entity)?;
        let face: u8 = match click.face.as_str() {
            "down" => 0,
            "up" => 1,
            "north" => 2,
            "south" => 3,
            "west" => 4,
            "east" => 5,
            other => return Err(format!("unknown face {other}")),
        };
        let stack = match &click.item {
            Some(item) if item == "minecraft:air" => Some(vanillite_game::item::ItemStack::EMPTY),
            Some(item) => {
                let mut c = vanillite_nbt::Compound::new();
                c.insert("id", vanillite_nbt::Tag::String(item.clone()));
                c.insert("count", vanillite_nbt::Tag::Int(click.count));
                if let Some(snbt) = &click.components {
                    let components =
                        vanillite_nbt::snbt::parse_compound(snbt).map_err(|e| e.to_string())?;
                    c.insert("components", vanillite_nbt::Tag::Compound(components));
                }
                Some(vanillite_game::item_nbt::stack_from_nbt(&c))
            }
            None => None,
        };
        let data = vanillite_game::player::play::game_data().ok_or("no game data")?;
        let (pos, rot, row, network_id, uuid, on_ground) = {
            let e = self
                .level
                .entities
                .store
                .get(id)
                .ok_or("the player is gone")?;
            (
                e.pos(),
                e.rot(),
                e.kind_row(),
                e.network_id(),
                e.uuid(),
                e.flags()
                    .contains(vanillite_world::entity::EntityFlags::ON_GROUND),
            )
        };
        let mut players = std::mem::take(&mut self.level.entities.players);
        let mut bind_fence = None;
        let mut start_using = None;
        let result = (|| {
            let player = players.get_mut(row).ok_or("no player data")?;
            if let Some(food) = click.food {
                player.survival.food.food_level = food;
            }
            if let Some(stack) = stack {
                if click.offhand {
                    player.inventory.offhand = stack;
                } else {
                    let slot = player.inventory.selected;
                    player.inventory.set(slot, stack);
                }
            }
            let actor = Actor {
                entity: Some(id),
                network_id: network_id.0,
                uuid,
                pos,
                eye_height: if click.sneak { 1.27 } else { 1.62 },
                y_rot: rot.y_rot,
                x_rot: rot.x_rot,
                on_ground,
                sneaking: click.sneak,
                game_mode: player.game_mode,
                permission_level: 0,
                eyes_in_water: false,
                dig_speed: player
                    .survival
                    .effects
                    .has_dig_speed()
                    .then(|| player.survival.effects.dig_speed_amplification()),
                mining_fatigue: player
                    .survival
                    .effects
                    .amplifier(vanillite_game::effect::MobEffect::MINING_FATIGUE),
                respawn_here: player
                    .survival
                    .respawn
                    .as_ref()
                    .filter(|r| r.dimension == "minecraft:overworld")
                    .map(|r| r.pos),
                leads_mobs: self.level.entities.leashes.values().any(|&h| h == id),
                ranges: {
                    let ranges = vanillite_game::player::play::interaction_ranges(
                        &player.inventory,
                        player.game_mode,
                    );
                    // The recorder calls `ServerPlayerGameMode.useItemOn` itself, without the
                    // packet handler's reach check: a clicked block is in reach.
                    if click.air || click.attack {
                        ranges
                    } else {
                        (10_000.0, ranges.1)
                    }
                },
            };
            let mut effects = vanillite_server::LevelEffects::default();
            let spectators = vanillite_base::hash::FxHashSet::default();
            let aabb = self
                .level
                .entities
                .store
                .get(id)
                .map(|e| e.aabb())
                .ok_or("the player is gone")?;
            let mut world = vanillite_server::WorldLevel {
                level: &mut self.level,
                effects: &mut effects,
                spectators: &spectators,
                actor: Some((id, aabb)),
                dimension: "minecraft:overworld",
            };
            let mut drops = Vec::new();
            let mut swings = Vec::new();
            let protected = |_: BlockPos| false;
            let hooks = vanillite_game::hooks::NoHooks;
            let mut ctx = PlayContext {
                level: &mut world,
                inventory: &mut player.inventory,
                xp: Some(&mut player.survival.xp),
                random: Some(&mut player.survival.random),
                food: Some(&mut player.survival.food),
                recipe_book: Some(&mut player.recipe_book),
                actor,
                data: &data,
                spawn_protected: &protected,
                drops: &mut drops,
                swings: &mut swings,
                hooks: &hooks,
                stats: Some(&mut player.stats),
                advancements: Some(&mut player.advancements),
            };
            let at = click.pos;
            let wire = vanillite_protocol::types::BlockPos {
                x: at.x,
                y: at.y,
                z: at.z,
            };
            if click.air {
                let hand = if click.offhand {
                    Hand::OffHand
                } else {
                    Hand::MainHand
                };
                return Ok(player
                    .connection
                    .play
                    .use_item(&mut ctx, hand, 0, rot.y_rot, rot.x_rot));
            } else if click.attack {
                player.connection.play.handle_action(
                    &mut ctx,
                    DigAction::StartDestroyBlock,
                    at,
                    face,
                    0,
                    &NoSink,
                );
            } else {
                let [hx, hy, hz] = click.hit.unwrap_or(match face {
                    0 => [0.5, 0.0, 0.5],
                    1 => [0.5, 1.0, 0.5],
                    2 => [0.5, 0.5, 0.0],
                    3 => [0.5, 0.5, 1.0],
                    4 => [0.0, 0.5, 0.5],
                    _ => [1.0, 0.5, 0.5],
                });
                let packet = UseItemOn {
                    hand: if click.offhand {
                        Hand::OffHand
                    } else {
                        Hand::MainHand
                    },
                    pos: wire,
                    face: i32::from(face),
                    cursor_x: hx as f32,
                    cursor_y: hy as f32,
                    cursor_z: hz as f32,
                    inside: false,
                    world_border_hit: false,
                    sequence: 0,
                };
                player
                    .connection
                    .play
                    .use_item_on(&mut ctx, &packet, &NoSink);
                bind_fence = player.connection.play.bind_fence.take();
                start_using = player.connection.play.start_using.take();
            }
            Ok::<bool, String>(false)
        })();
        self.level.entities.players = players;
        let handled = result?;
        vanillite_game::survival::sleep::run_block_uses(
            &mut self.level,
            &vanillite_game::hooks::NoHooks,
        );
        self.explosion_hurts();
        // `EntityAction::StartUsing`: the consumable that could not be placed is used.
        if let Some(hand) = start_using {
            let network_ids = Arc::clone(&self.network_ids);
            self.with_env(|_, ents, env, _| {
                vanillite_game::player::using::start(ents, env, &network_ids, id, hand);
            });
        }
        // `EntityAction::BindToFence`: the fence ties the player's leashed mobs.
        if let Some(fence) = bind_fence {
            let network_ids = Arc::clone(&self.network_ids);
            self.with_env(|_, ents, env, _| {
                vanillite_game::mob::tame::bind_player_mobs(ents, env, &network_ids, id, fence);
            });
        }
        // The entity side of `useItemOn` (`EntityAction::UseItemOn`): spawn eggs, leads,
        // minecarts and rockets.
        let hand = if click.offhand {
            Hand::OffHand
        } else {
            Hand::MainHand
        };
        let held = self.level.entities.player(id).map(|p| {
            if click.offhand {
                p.inventory.offhand.clone()
            } else {
                p.inventory.get(p.inventory.selected).clone()
            }
        });
        if !click.attack
            && !click.air
            && held.is_some_and(|s| vanillite_game::player::item_use::uses_entities_on_block(&s))
        {
            let [hx, hy, hz] = click.hit.unwrap_or(match face {
                0 => [0.5, 0.0, 0.5],
                1 => [0.5, 1.0, 0.5],
                2 => [0.5, 0.5, 0.0],
                3 => [0.5, 0.5, 1.0],
                4 => [0.0, 0.5, 0.5],
                _ => [1.0, 0.5, 0.5],
            });
            let at = click.pos;
            #[allow(clippy::cast_possible_truncation)]
            let location = vanillite_base::Vec3::new(
                f64::from(at.x) + f64::from(hx as f32),
                f64::from(at.y) + f64::from(hy as f32),
                f64::from(at.z) + f64::from(hz as f32),
            );
            let direction = vanillite_game::player::play::direction_wrapped(i32::from(face));
            let network_ids = Arc::clone(&self.network_ids);
            self.with_env(|level, ents, env, _| {
                vanillite_game::player::item_use::use_item_on(
                    level,
                    ents,
                    env,
                    &network_ids,
                    id,
                    hand,
                    at,
                    direction,
                    location,
                    false,
                );
            });
        }
        self.apply_actions();
        self.finish_removals();
        self.spawn_drops();
        Ok(handled)
    }

    /// The world of `spec`. The simulation runs on the world's thread only, so serial and
    /// parallel execution are the same.
    pub fn new(spec: &WorldSpec) -> Result<Self, String> {
        install();
        // The recorder seeds every shuffled behaviour list's random alike
        // (`ParityHooks.shuffleRandom`).
        vanillite_game::mob::set_parity_shuffle_seed(Some(PARITY_SHUFFLE_SEED));
        let store = Arc::new(MemoryStore::default());
        if let Some(snapshot) = &spec.snapshot {
            preload_snapshot(&store, snapshot, &spec.forced, spec.snapshot_entities)?;
        }
        let (mut level, network_ids) = open_level(spec, Arc::clone(&store))?;
        let mut rules = GameRules::default();
        apply_rules(&mut LevelSettings::default(), &mut rules, &spec.rules)?;
        let difficulty = match spec.difficulty.as_str() {
            "peaceful" => Difficulty::Peaceful,
            "easy" => Difficulty::Easy,
            "normal" => Difficulty::Normal,
            "hard" => Difficulty::Hard,
            d => return Err(format!("unknown difficulty `{d}`")),
        };
        level.settings.difficulty = difficulty;
        // The world starts at day time 0 unless the scenario sets one; time advances with
        // `advance_time` (off by default).
        let env = environment(spec, spec.time.unwrap_or(0), LevelWeather::default());
        // What spawner blocks and creaking hearts read when the block entities tick.
        level.spawner_env = vanillite_game::block::spawner::SpawnerEnv {
            env,
            difficulty,
            spawn_mobs: rules.bool(Rule::SPAWN_MOBS),
            spawn_monsters: rules.bool(Rule::SPAWN_MONSTERS),
            ..Default::default()
        };
        level.entities.day_time = env.day_time;
        let mut world = Self {
            level,
            spec: spec.clone(),
            store,
            light: spec.observe_light,
            network_ids,
            mobs: MobContext::default(),
            rules,
            difficulty,
            weather: weather_data(spec),
            env,
            effects: Vec::new(),
            names: Vec::new(),
            seed: spec.seed,
            level_rng: spec.level_rng,
            clock_time: spec.time.unwrap_or(0),
            bolt_hits: Vec::new(),
            item_scratch: vanillite_server::ItemScratch::default(),
            clients: Vec::new(),
            network_names: Vec::new(),
        };
        world
            .set_forced(&spec.forced, true)
            .map_err(|_| "the scenario region does not become ticking".to_owned())?;
        if spec.snapshot.is_some() && !spec.snapshot_entities {
            // What chunk generation spawned (see the scenario documentation).
            let spawned = world.level.entities.store.columns().id.clone();
            for id in spawned {
                world.level.entities.despawn(id);
            }
            world.level.tick_entity_manager();
        }
        Ok(world)
    }

    /// Whether the chunk system is settled around the forced chunks: every chunk is visible
    /// as its ticket level says, and nothing is loading, generating or waiting to unload.
    fn settled(&self) -> bool {
        let chunks = &self.level.chunks;
        chunks.is_idle()
            && self.level.forced.sorted().into_iter().all(|pos| {
                chunks.visibility(pos) == Visibility::Ticking && chunks.is_block_ticking(pos)
            })
    }

    /// [`crate::world::WorldFactory`] of the game world.
    pub fn factory(spec: &WorldSpec, _execution: Execution) -> Box<dyn ScenarioWorld> {
        Box::new(Self::new(spec).unwrap_or_else(|e| panic!("{e}")))
    }

    /// The level, for tests that look deeper than the trace.
    pub fn level(&mut self) -> &mut Level {
        &mut self.level
    }

    /// Runs `f` with the survival environment of this tick; the entities are taken out of the
    /// level meanwhile, the level random is the environment's random.
    fn with_env<R>(
        &mut self,
        f: impl FnOnce(&Level, &mut Entities, &mut Env<'_>, &mut MobContext) -> R,
    ) -> R {
        let mut random = std::mem::replace(self.level.random(), LegacyRandom::new(0));
        let mut ents = std::mem::take(&mut self.level.entities);
        let (types, facts) = PACK.get().expect("installed");
        let r = {
            let level = &self.level;
            let blocks = ChunkBlocks(&level.chunks, &level.block_entities);
            let raining = |pos| level.is_raining_at(pos);
            let mut no_gateway = |_| false;
            let names = |name: &str, _| vanillite_protocol::text::Component::text(name.to_owned());
            let mut env = Env {
                types,
                facts,
                blocks: &blocks,
                rules: &self.rules,
                difficulty: self.difficulty,
                game_time: level.game_time(),
                min_y: MIN_Y,
                logical_height: 384,
                dimension: "minecraft:overworld",
                raining_at: &raining,
                border: None,
                enter_gateway: &mut no_gateway,
                random: &mut random,
                out: &mut self.effects,
                hooks: &vanillite_game::hooks::NoHooks,
                hook_world: vanillite_game::hooks::HookWorld::NONE,
                display_name: &names,
                // Teams come from `team` commands, so only once the command server runs.
                entity_team: &|holder| {
                    COMMAND_SERVER
                        .get()
                        .and_then(|g| vanillite_server::entity_team(g, holder))
                },
            };
            f(level, &mut ents, &mut env, &mut self.mobs)
        };
        self.level.entities = ents;
        *self.level.random() = random;
        r
    }

    /// `entityTickList.forEach(tickNonPassenger)`: falling blocks, TNT, players
    /// (`ServerPlayer.tick`), mobs, arrows and orbs.
    fn tick_entities(&mut self) {
        let has_players = !self.level.entities.players.is_empty();
        self.mobs
            .begin_tick(self.env, MobSettings::default(), has_players);
        // `PiglinAi.angerNearbyPiglins` of the block uses and breaks before.
        let anger = std::mem::take(&mut self.level.output().piglin_anger);
        if !anger.is_empty() {
            self.with_env(|level, ents, env, cx| {
                for (player, only_if_seen) in anger {
                    vanillite_game::mob::fam::brainy::piglin::anger_nearby_piglins(
                        level,
                        ents,
                        env,
                        cx,
                        player,
                        only_if_seen,
                    );
                }
            });
        }
        // What block entities (beehives) read of the world after the entities.
        self.level.spawner_env.env = self.env;
        self.level.entities.day_time = self.env.day_time;
        let mut cursor = self.level.entities.store.tick_cursor();
        while let Some(id) = self.level.entities.store.next_to_tick(&mut cursor) {
            if !vanillite_game::entities::in_ticking_range(&self.level, id) {
                continue;
            }
            let Some(mut entity) = self.level.entities.store.get_mut(id) else {
                continue;
            };
            entity.set_old_pos();
            let kind = entity.as_ref().kind();
            let pos = entity.as_ref().pos();
            if kind == ids::LIGHTNING_BOLT {
                let mut hits = std::mem::take(&mut self.bolt_hits);
                lightning::tick(&mut self.level, id, &mut hits);
                if !hits.is_empty() {
                    self.with_env(|level, ents, env, cx| {
                        for &target in &hits {
                            lightning::thunder_hit(level, ents, env, cx, target, id);
                        }
                    });
                    self.finish_removals();
                }
                self.bolt_hits = hits;
                continue;
            }
            if kind == ids::OMINOUS_ITEM_SPAWNER {
                vanillite_game::block::trial_spawner::tick_item_spawner(&mut self.level, id);
                self.spawn_drops();
                continue;
            }
            if kind == ids::FALLING_BLOCK {
                // The entities a landing block hurts take the damage (and drop their loot)
                // before it lands.
                if let Some(resume) =
                    vanillite_game::block::falling::tick_until_hurt(&mut self.level, id)
                {
                    self.explosion_hurts();
                    self.spawn_drops();
                    vanillite_game::block::falling::resume_tick(&mut self.level, id, resume);
                }
                continue;
            }
            if kind == ids::TNT {
                vanillite_game::entities::tick_entity(&mut self.level, id, kind);
                self.explosion_hurts();
                continue;
            }
            if kind == ids::PLAYER {
                if let Some(p) = self.level.entities.player_mut(id) {
                    p.connection.movement.tick_client_load_timeout();
                    survival_player::tick_entity(p);
                }
                continue;
            }
            if kind == ids::ITEM {
                let level = &mut self.level;
                let (types, facts) = PACK.get().expect("installed");
                let mut ents = std::mem::take(&mut level.entities);
                let burn = {
                    let raining = |pos| level.is_raining_at(pos);
                    vanillite_server::tick_item(
                        &mut ents,
                        level,
                        id,
                        MIN_Y,
                        vanillite_server::ItemSettings {
                            lifetime: ITEM_LIFETIME,
                            merge_radius: 0.5,
                        },
                        &mut self.item_scratch,
                        &vanillite_game::item_entity::FireWorld {
                            blocks: &ChunkBlocks(&level.chunks, &level.block_entities),
                            facts,
                            types,
                            raining_at: &raining,
                            reacts: &|s| level.has_entity_inside(s),
                        },
                    )
                };
                level.entities = ents;
                if !burn.is_none() {
                    vanillite_game::item_entity::burn_effects(level, id, burn);
                }
                continue;
            }
            let is_mob = is_mob_kind(kind);
            let is_special = mob::boss::special::is_special_kind(kind);
            let is_minecart = MinecartKind::of(kind).is_some();
            let is_boat = vanillite_game::vehicle::BoatKind::of(kind).is_some();
            let is_missile = vanillite_game::mob::missile::is_missile_kind(kind);
            let is_decor = vanillite_game::decoration::ticks(kind);
            if !is_mob
                && !is_special
                && !is_minecart
                && !is_boat
                && !is_missile
                && !is_decor
                && !projectile::is_arrow_kind(kind)
                && kind != ids::EXPERIENCE_ORB
                && kind != ids::AREA_EFFECT_CLOUD
            {
                continue;
            }
            if !self
                .level
                .is_block_ticking(BlockPos::containing(pos.x, pos.y, pos.z).chunk())
            {
                continue;
            }
            if kind == ids::EXPERIENCE_ORB {
                vanillite_game::entities::tick_entity(&mut self.level, id, kind);
                continue;
            }
            self.with_env(|level, ents, env, cx| {
                if is_mob {
                    mob::tick(level, ents, env, cx, id);
                } else if is_special {
                    mob::boss::special::tick(level, ents, env, cx, id);
                } else if is_minecart {
                    minecart::tick(level, ents, env, cx, id);
                } else if is_boat {
                    vanillite_game::vehicle::tick_boat(level, ents, env, cx, id);
                } else if projectile::is_arrow_kind(kind) {
                    projectile::tick_arrow(level, ents, env, cx, id);
                } else if is_missile {
                    vanillite_game::mob::missile::tick(level, ents, env, cx, id);
                } else if is_decor {
                    vanillite_game::decoration::tick(level, ents, env, cx, id);
                } else {
                    vanillite_game::effect::cloud::tick(level, ents, env, cx, id);
                }
            });
            if !self.mobs.actions.is_empty() {
                self.apply_actions();
            }
            let ents = &mut self.level.entities;
            if ents.mob(id).is_some_and(|m| m.removed) {
                ents.despawn(id);
            }
            self.finish_removals();
        }
        // `rideTick`: sneaking players get off, passengers follow their vehicles (as the
        // server's `WorldMobs`).
        self.with_env(|_, ents, env, _| {
            vanillite_game::riding::stop_riding_sneaking_players(ents, env);
        });
        vanillite_game::riding::ride_tick(&mut self.level.entities);
        // Leads after the entities (`WorldMobs` of the server): orphaned leads, gone knots.
        let discarded = self.with_env(|level, ents, env, _| {
            let mut discarded = Vec::new();
            vanillite_game::mob::tame::tick_leashes(level, ents, env, &mut discarded);
            discarded
        });
        for d in discarded {
            self.level.entities.despawn(d);
        }
        // No chunk tickets or other worlds here: every chunk of the scenario is forced.
        self.mobs.travel.clear();
        self.mobs.gateways.clear();
        self.spawn_drops();
    }

    /// What the last mob tick queued (`WorldMobs::apply_actions` of the server).
    fn apply_actions(&mut self) {
        for action in std::mem::take(&mut self.mobs.actions) {
            match action {
                WorldAction::Explode(e) => {
                    e.run(&mut self.level, &self.rules);
                    self.explosion_hurts();
                }
                WorldAction::ExplodeWith {
                    source,
                    indirect,
                    pos,
                    radius,
                    blocks,
                } => {
                    let interaction = if !blocks {
                        BlockInteraction::Keep
                    } else if self.rules.bool(Rule::BLOCK_EXPLOSION_DROP_DECAY) {
                        BlockInteraction::DestroyWithDecay
                    } else {
                        BlockInteraction::Destroy
                    };
                    self.level
                        .explode(source, indirect, pos, radius, interaction);
                    self.explosion_hurts();
                }
                WorldAction::Level(action) => action.apply(&mut self.level),
                other => {
                    other.apply_to_level(&mut self.level);
                }
            }
        }
        self.finish_removals();
    }

    /// The explosion damage of the living entities the last explosions hit
    /// (`WorldMobs::apply_explosion_hurts` of the server).
    fn explosion_hurts(&mut self) {
        if self.level.output().explosion_hurts.is_empty()
            && self.level.output().falling_hurts.is_empty()
        {
            self.level.finish_explosion();
            return;
        }
        let hurts = std::mem::take(&mut self.level.output().explosion_hurts);
        let falling = std::mem::take(&mut self.level.output().falling_hurts);
        self.with_env(|level, ents, env, cx| {
            vanillite_game::block::falling::apply_falling_hurts(level, ents, env, cx, falling);
            for hurt in hurts {
                hurt.aim(ents);
                let mut source = match (hurt.source, hurt.indirect) {
                    _ if hurt.bad_respawn_point => DamageSource::of(env.types.bad_respawn_point),
                    (Some(direct), Some(cause)) => {
                        combat::entity_source(ents, env.types.player_explosion, cause, direct)
                    }
                    (direct, _) => {
                        let mut source = DamageSource::of(env.types.explosion);
                        source.direct = direct;
                        source.position = Some(
                            direct
                                .and_then(|d| ents.store.get(d))
                                .map_or(hurt.center, |e| e.pos()),
                        );
                        source
                    }
                };
                source.position.get_or_insert(hurt.center);
                if ents.minecart(hurt.target).is_some() {
                    minecart::hurt(ents, env, cx, hurt.target, source, hurt.amount);
                    continue;
                }
                combat::hurt(level, ents, env, cx, hurt.target, source, hurt.amount);
            }
        });
        // `interactWithBlocks` after `hurtEntities`.
        self.level.finish_explosion();
        self.finish_removals();
    }

    fn finish_removals(&mut self) {
        for d in std::mem::take(&mut self.mobs.discarded) {
            self.level.entities.despawn(d);
        }
    }

    /// The item entities of the drops since the last call (`spawn_drops` of the server), then
    /// the game events of the effects (as the server posts them after delivering); the other
    /// effects go to clients nobody has.
    fn spawn_drops(&mut self) {
        let mut events = Vec::new();
        for effect in std::mem::take(&mut self.effects) {
            if let Effect::GameEvent {
                event,
                pos,
                context,
            } = effect
            {
                events.push((event, pos, context));
                continue;
            }
            // `ServerPlayer.die`: `tellNeutralMobsThatIDied` (as the server does on this effect).
            if let Effect::Died { victim, .. } = effect
                && self.level.entities.player(victim).is_some()
                && self.rules.bool(Rule::FORGIVE_DEAD_PLAYERS)
            {
                vanillite_game::mob::common::tell_neutral_mobs_player_died(
                    &mut self.level.entities,
                    victim,
                );
            }
            if let Effect::Experience { pos, amount } = effect {
                // The merge group comes from the level random (`tryMergeToExisting`).
                let (entities, network_ids, random) = self.level.entity_parts();
                vanillite_game::experience_orb::award_in(
                    entities,
                    network_ids,
                    random,
                    pos,
                    vanillite_base::Vec3::default(),
                    amount,
                );
            }
            if let Effect::DropItem {
                uuid,
                random: item_random,
                pos,
                velocity,
                y_rot,
                pickup_delay,
                stack,
            } = effect
            {
                self.level.entities.spawn_item(
                    self.network_ids.next(),
                    uuid,
                    pos,
                    velocity,
                    y_rot,
                    ItemData::new(stack, pickup_delay, None, item_random),
                );
            }
        }
        for (event, pos, context) in events {
            self.level.game_event(event, pos, context);
        }
    }

    /// `ServerPlayer.doTick` of every player (the connection tick).
    fn tick_players(&mut self) {
        let players: Vec<EntityId> = self
            .level
            .entities
            .players
            .iter()
            .map(|(id, _)| id)
            .collect();
        if players.is_empty() {
            return;
        }
        // `Player.tick`'s sleep part.
        vanillite_game::survival::sleep::update_sleeping_player_list(&mut self.level);
        vanillite_game::survival::sleep::tick_players(
            &mut self.level,
            &vanillite_game::hooks::NoHooks,
        );
        self.with_env(|level, ents, env, cx| {
            for &id in &players {
                let result = {
                    let Entities { store, players, .. } = &mut *ents;
                    let Some(mut entity) = store.get_mut(id) else {
                        continue;
                    };
                    let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
                        continue;
                    };
                    let std::ops::ControlFlow::Continue(result) =
                        survival_player::do_tick_base(&mut entity, data, env)
                    else {
                        continue;
                    };
                    result
                };
                // `LivingEntity.tick`: `updatingUsingItem` after the base tick, before
                // `aiStep`.
                vanillite_game::player::using::tick(ents, env, id);
                let result = {
                    let Entities { store, players, .. } = &mut *ents;
                    let Some(mut entity) = store.get_mut(id) else {
                        continue;
                    };
                    let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
                        continue;
                    };
                    survival_player::do_tick_ai(&mut entity, data, env, result)
                };
                // `pushEntities` and `touch` at the end of the player's `aiStep`.
                vanillite_game::mob::living::player_ai_step_end(
                    level,
                    ents,
                    env,
                    cx,
                    id,
                    |ents, reach| {
                        // `touch` → `AbstractArrow.playerTouch`: arrows and tridents it picks
                        // up.
                        let mut near = Vec::new();
                        ents.store.for_each_non_player_in(reach, |e| near.push(e));
                        let mut players = std::mem::take(&mut ents.players);
                        let row = ents.store.get(id).map(|e| e.kind_row());
                        for p in near {
                            if let Some(data) = row.and_then(|r| players.get_mut(r))
                                && vanillite_game::mob::projectile::try_pickup(ents, p, data, id)
                            {
                                ents.despawn(p);
                            }
                        }
                        // Then one of the touched orbs (`Util.getRandom`):
                        // `ExperienceOrb.playerTouch`.
                        if let Some(data) = row.and_then(|r| players.get_mut(r)) {
                            touch_orb(ents, data, reach);
                        }
                        ents.players = players;
                    },
                );
                let Entities { store, players, .. } = &mut *ents;
                let Some(mut entity) = store.get_mut(id) else {
                    continue;
                };
                let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
                    continue;
                };
                survival_player::do_tick_end(&mut entity, data, env, result);
            }
            vanillite_game::spear::tick_players(level, ents, env, cx);
            vanillite_game::trident::tick_spin_attacks(level, ents, env, cx);
        });
        // The players' contact with blocks (`post_player_game_events` of the server).
        let mut actions = Vec::new();
        for (_, p) in self.level.entities.players.iter_mut() {
            actions.append(&mut p.survival.world_actions);
        }
        for action in actions {
            match action {
                vanillite_game::mob::WorldAction::Level(action) => action.apply(&mut self.level),
                other => {
                    other.apply_to_level(&mut self.level);
                }
            }
        }
        self.spawn_drops();
    }

    fn named(&self, name: &str) -> Result<EntityId, String> {
        self.names
            .iter()
            .find(|(_, n)| n == name)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("no scenario entity `{name}`"))
    }

    fn name_of(&self, id: EntityId) -> Option<&str> {
        self.names
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, n)| n.as_str())
    }

    /// Adds a player of the scenario's entities (`index` is its position there).
    pub fn add_player(&mut self, spec: &EntitySpec, index: usize) -> Result<(), String> {
        let pos = Vec3::new(spec.pos[0], spec.pos[1], spec.pos[2]);
        let (tx, inbound) = vanillite_protocol::inbound_queue(CLIENT_QUEUE);
        let chat = ChatSettings::default();
        let mut data = PlayerData {
            profile: GameProfile {
                name: spec.name.clone(),
                properties: Vec::new(),
            },
            game_mode: match spec.game_mode.as_deref() {
                None | Some("survival") => vanillite_config::GameMode::Survival,
                Some("creative") => vanillite_config::GameMode::Creative,
                Some("adventure") => vanillite_config::GameMode::Adventure,
                Some("spectator") => vanillite_config::GameMode::Spectator,
                Some(m) => return Err(format!("unknown game mode `{m}`")),
            },
            previous_game_mode: None,
            client: ClientInformation::default(),
            chunk_view: ChunkView {
                center: BlockPos::containing(pos.x, pos.y, pos.z).chunk(),
                view_distance: 2,
            },
            connection: Connection::new(
                Arc::new(NoSink),
                inbound,
                pos,
                true,
                true,
                &chat,
                Arc::new(Mutex::new(ChatInbox::default())),
            ),
            inventory: vanillite_game::inventory::PlayerInventory::default(),
            last_equipment: std::array::from_fn(|_| vanillite_game::item::ItemStack::EMPTY),
            survival: vanillite_game::survival::player::Survival::default(),
            post_effects: Vec::new(),
            stats: vanillite_game::stats::StatsCounter::default(),
            advancements: vanillite_game::advancements::PlayerAdvancements::load(
                Arc::new(vanillite_game::advancements::Advancements::default()),
                None,
            ),
            recipe_book: vanillite_game::recipe::book::PlayerRecipeBook::default(),
        };
        let rot = Rotation {
            y_rot: spec.rot[0],
            x_rot: spec.rot[1],
            y_head_rot: spec.rot[0],
        };
        for (slot, item, count) in &spec.inventory {
            let item = vanillite_data::item::ItemId::by_name(item)
                .ok_or_else(|| format!("unknown item {item}"))?;
            if let Some(s) = data.inventory.get_mut(*slot) {
                *s = vanillite_game::item::ItemStack::new(item.0, *count);
            }
        }
        if let Some(food) = spec.food {
            data.survival.food.food_level = food;
        }
        // Nobody moves the mock player: the server does, like Vanilla's. A client player
        // moves by its packets.
        data.survival.simulate_movement = !spec.client;
        // The recorder seeds the player's own random with the spec's seed
        // (`ParityHooks.seedNext`), then the `LivingEntity` constructor draws the yaw from it.
        let mut own = LegacyRandom::new(spec.seed);
        let _ = own.next_float();
        data.survival.own_random = own.state();
        data.survival.y_body_rot = spec.rot[0];
        // The recorder seeds the player's own random with its `seed` at the end of the
        // `Entity` constructor; the `LivingEntity` constructor then draws the yaw.
        data.survival.random = LegacyRandom::new(spec.seed);
        data.survival.random.next_float();
        let id =
            self.level
                .entities
                .spawn_player(&self.network_ids, spec.uuid(index), pos, rot, data);
        self.names.push((id, spec.name.clone()));
        if spec.client {
            self.clients.push(Client {
                id,
                tx,
                seq: 0,
                joined: false,
            });
            self.join_client(id)?;
        }
        Ok(())
    }

    fn add_mob(&mut self, spec: &EntitySpec, index: usize) -> Result<(), String> {
        let id = spawn_mob(
            &mut self.level,
            &self.network_ids,
            self.env,
            self.difficulty,
            spec,
            index,
        )?;
        self.names.push((id, spec.name.clone()));
        Ok(())
    }

    /// The trace detail of an entity (see [`crate::trace`]).
    fn detail(&self, row: usize) -> Option<Map<String, Value>> {
        let name_of = |id| self.name_of(id).map(str::to_owned);
        entity_detail(
            &self.level.entities,
            row,
            &name_of,
            &|id| self.clients.iter().any(|c| c.id == id),
            self.spec.player_state,
            self.spec.sleep_state,
        )
    }
}

/// A scenario entity besides players like `ScenarioEntities.mob` makes it: a minecart, or a
/// mob created with its seed, `finalizeSpawn` with the level random, made a baby if asked.
pub(crate) fn spawn_mob(
    level: &mut Level,
    network_ids: &NetworkIds,
    env: Environment,
    difficulty: Difficulty,
    spec: &EntitySpec,
    index: usize,
) -> Result<EntityId, String> {
    if let Some(kind) = EntityTypeId::by_name(&spec.kind).and_then(MinecartKind::of) {
        let rot = Rotation {
            y_rot: spec.rot[0],
            x_rot: spec.rot[1],
            y_head_rot: spec.rot[0],
        };
        let pos = Vec3::new(spec.pos[0], spec.pos[1], spec.pos[2]);
        let id = minecart::spawn(
            &mut level.entities,
            network_ids,
            spec.uuid(index),
            pos,
            rot,
            minecart::MinecartData::new(kind, spec.seed),
        );
        if let (Some(m), Some(mut e)) = (spec.motion, level.entities.store.get_mut(id)) {
            e.set_velocity(Vec3::new(m[0], m[1], m[2]));
        }
        return Ok(id);
    }
    let kind = EntityTypeId::by_name(&spec.kind)
        .and_then(Kind::of)
        .ok_or_else(|| format!("{}: no mob behaviour in Vanillite", spec.kind))?;
    let mut m = MobData::new(kind, spec.seed);
    // Only a `Mob` has `finalizeSpawn`.
    if spec.finalize && kind.family() != vanillite_game::mob::Family::Decor {
        // `finalizeSpawn(level, difficulty, COMMAND, null)` with the level random.
        let pos = BlockPos::containing(spec.pos[0], spec.pos[1], spec.pos[2]);
        let mut random = std::mem::replace(level.random(), LegacyRandom::new(0));
        {
            let mut ctx = vanillite_game::mob::fam::SpawnContext::simple(
                difficulty,
                vanillite_game::mob::fam::SpawnReason::Command,
            );
            ctx.pos = pos;
            ctx.level = Some(level);
            ctx.env = env;
            kinds::finalize_spawn(&mut m, &mut ctx, &mut random);
        }
        *level.random() = random;
    }
    if let Some(baby) = spec.baby {
        // `setBaby`: zombies also change their speed modifier.
        vanillite_game::mob::fam::undead::set_baby(&mut m, baby);
        m.baby = baby;
        m.age = if baby { -24000 } else { 0 };
    }
    if let Some(state) = &spec.weather_state {
        use vanillite_game::mob::fam::mount::copper_golem;
        copper_golem::golem_mut(&mut m).weather = copper_golem::Weather::of(state);
    }
    m.y_body_rot = spec.rot[0];
    let rot = Rotation {
        y_rot: spec.rot[0],
        x_rot: spec.rot[1],
        y_head_rot: spec.rot[0],
    };
    let pos = Vec3::new(spec.pos[0], spec.pos[1], spec.pos[2]);
    let id = mob::spawn(
        &mut level.entities,
        network_ids,
        spec.uuid(index),
        pos,
        rot,
        m,
    );
    Ok(id)
}

/// The trace detail of the entity in row `row` (see [`crate::trace`]); `name_of` gives the
/// scenario names, `is_client` tells the packet-level players.
pub(crate) fn entity_detail(
    ents: &Entities,
    row: usize,
    name_of: &dyn Fn(EntityId) -> Option<String>,
    is_client: &dyn Fn(EntityId) -> bool,
    player_state: bool,
    sleep_state: bool,
) -> Option<Map<String, Value>> {
    let core = ents.store.columns();
    let id = core.id[row];
    let mut d = Map::new();
    if let Some(name) = name_of(id) {
        d.insert("name".into(), json!(name));
    }
    if let Some(item) = ents.item(id) {
        d.insert(
            "item".into(),
            json!(vanillite_data::item::ItemId(item.stack.item).name()),
        );
        d.insert("count".into(), json!(item.stack.count));
    }
    let bits = |f: f32| json!(f.to_bits() as i32);
    let rot = core.rot[row];
    let client = is_client(id);
    let living = |d: &mut Map<String, Value>, body: f32, hp: f32, death: i32| {
        // The body rotation of a client's player is the client's own look.
        let rot = if client {
            json!([bits(rot.y_rot), bits(rot.x_rot), bits(rot.y_head_rot)])
        } else {
            json!([
                bits(rot.y_rot),
                bits(rot.x_rot),
                bits(rot.y_head_rot),
                bits(body)
            ])
        };
        d.insert("rot".into(), rot);
        d.insert("hp".into(), bits(hp));
        if death > 0 {
            d.insert("death".into(), json!(death));
        }
    };
    if let Some(p) = ents.player(id) {
        living(
            &mut d,
            p.survival.y_body_rot,
            p.survival.health,
            p.survival.death_time,
        );
        if client {
            let inv: Vec<Value> = (0..=vanillite_game::inventory::OFFHAND)
                .filter_map(|slot| {
                    let s = p.inventory.get(slot);
                    (!s.is_empty())
                        .then(|| json!([slot, vanillite_game::item::item_id(s).name(), s.count]))
                })
                .collect();
            d.insert("inv".into(), Value::Array(inv));
            d.insert(
                "food".into(),
                json!([p.survival.food.food_level, bits(p.survival.food.saturation)]),
            );
        }
        if player_state {
            let hand = |s: &vanillite_game::item::ItemStack| {
                json!([vanillite_game::item::item_id(s).name(), s.count])
            };
            d.insert(
                "food".into(),
                json!([p.survival.food.food_level, bits(p.survival.food.saturation)]),
            );
            d.insert(
                "hands".into(),
                json!([
                    hand(p.inventory.selected_item()),
                    hand(&p.inventory.offhand)
                ]),
            );
        }
        if sleep_state {
            let s = &p.survival;
            d.insert(
                "sleep".into(),
                json!({
                    "pos": s.sleeping_pos.map(|b| json!([b.x, b.y, b.z])),
                    "timer": s.sleep_timer,
                    "pose": vanillite_game::player::encode::pose_id(core.pose[row]),
                    "spawn": s.respawn.as_ref().map(|r| json!([r.pos.x, r.pos.y, r.pos.z, r.forced])),
                }),
            );
        }
    }
    if ents.minecart(id).is_some() {
        d.insert("rot".into(), json!([bits(rot.y_rot), bits(rot.x_rot)]));
    }
    if let Some(m) = ents.mob(id) {
        living(&mut d, m.y_body_rot, m.health, m.death_time);
    }
    // Armor stands and mannequins are living entities but no `Mob`.
    if let Some(m) = ents
        .mob(id)
        .filter(|m| m.kind.family() != vanillite_game::mob::Family::Decor)
    {
        d.insert("rng".into(), json!(m.random.state()));
        if let Some(t) = m.target {
            let name = name_of(t).unwrap_or_else(|| {
                ents.store
                    .get(t)
                    .map_or("?", |e| e.kind().name())
                    .to_owned()
            });
            d.insert("target".into(), json!(name));
        }
        d.insert("goals".into(), running(m.kind, &m.goals.slots));
        d.insert("targets".into(), running(m.kind, &m.targets.slots));
        // `getNavigation()`: a rider steering a mob vehicle reports the vehicle's.
        let nav = ents
            .mob(vanillite_game::mob::nav_owner(ents, id))
            .map_or(&m.nav, |o| &o.nav);
        if let Some(path) = nav.path.as_ref().filter(|p| !p.is_done()) {
            let nodes: Vec<Value> = path
                .nodes
                .iter()
                .map(|n| json!([n.x, n.y, n.z, path_type_name(n.kind)]))
                .collect();
            d.insert(
                "path".into(),
                json!({
                    "to": [path.target.x, path.target.y, path.target.z],
                    "next": path.next,
                    "reach": path.reached,
                    "nodes": nodes,
                }),
            );
        }
    }
    (!d.is_empty()).then_some(d)
}

/// The running goals by Vanilla class name, sorted.
fn running(kind: Kind, slots: &[Slot]) -> Value {
    let mut names: Vec<&str> = slots
        .iter()
        .filter(|s| s.running)
        .map(|s| goal_name(kind, &s.goal))
        .collect();
    names.sort_unstable();
    json!(names)
}

/// The Vanilla class of a goal of a mob of `kind`.
fn goal_name(kind: Kind, goal: &Goal) -> &'static str {
    match goal {
        Goal::Float => "FloatGoal",
        Goal::Panic { .. } if matches!(kind, Kind::Horse | Kind::Donkey | Kind::Mule) => {
            "MountPanicGoal"
        }
        Goal::Panic { .. } => "PanicGoal",
        Goal::Stroll { .. } => "WaterAvoidingRandomStrollGoal",
        Goal::LookAtPlayer { .. } if kind == Kind::Fox => "FoxLookAtPlayerGoal",
        Goal::LookAtPlayer { .. } => "LookAtPlayerGoal",
        Goal::RandomLook { .. } => "RandomLookAroundGoal",
        Goal::Melee { .. } if kind == Kind::Bee => "BeeAttackGoal",
        Goal::Melee { kind, .. } => match kind {
            MeleeKind::Plain => "MeleeAttackGoal",
            MeleeKind::Zombie => "ZombieAttackGoal",
            MeleeKind::Spider => "SpiderAttackGoal",
        },
        Goal::Swell => "SwellGoal",
        Goal::Leap { .. } => "LeapAtTargetGoal",
        Goal::FleeSun { .. } => "FleeSunGoal",
        Goal::RestrictSun => "RestrictSunGoal",
        Goal::Bow { .. } => "RangedBowAttackGoal",
        Goal::EatBlock { .. } => "EatBlockGoal",
        Goal::FollowParent { .. } => "FollowParentGoal",
        Goal::NearestTarget { spider: true, .. } => "SpiderTargetGoal",
        Goal::NearestTarget { .. } => "NearestAttackableTargetGoal",
        Goal::HurtBy { .. } if kind == Kind::Bee => "BeeHurtByOtherGoal",
        Goal::HurtBy { .. } if kind == Kind::Panda => "PandaHurtByTargetGoal",
        Goal::HurtBy { .. } => "HurtByTargetGoal",
        Goal::EndermanFreeze { .. } => "EndermanFreezeWhenLookedAt",
        Goal::EndermanStare { .. } => "EndermanLookForPlayerGoal",
        Goal::Breed { .. } if kind == Kind::Panda => "PandaBreedGoal",
        Goal::Breed { .. } => "BreedGoal",
        Goal::Tempt { .. } => "TemptGoal",
        Goal::SitWhenOrdered => "SitWhenOrderedToGoal",
        Goal::EndermanLeaveBlock => "EndermanLeaveBlockGoal",
        Goal::EndermanTakeBlock => "EndermanTakeBlockGoal",
        Goal::FollowOwner { .. } => "FollowOwnerGoal",
        Goal::OwnerHurtBy { .. } => "OwnerHurtByTargetGoal",
        Goal::OwnerHurt { .. } => "OwnerHurtTargetGoal",
        Goal::Common(CommonGoal::NearestAttackable { .. }) if kind == Kind::Bee => {
            "BeeBecomeAngryTargetGoal"
        }
        Goal::Common(c) => common_goal_name(c),
        Goal::Fam(f) => fam_goal_name(kind, f),
        Goal::Guardian(g) => {
            use vanillite_game::mob::boss::guardian::GuardianGoal as Gg;
            match g {
                Gg::Attack { .. } => "GuardianAttackGoal",
                Gg::MoveTowardsRestriction { .. } => "MoveTowardsRestrictionGoal",
                Gg::Stroll { .. } => "RandomStrollGoal",
                Gg::LookAtGuardian { .. } => "LookAtPlayerGoal",
                Gg::Target { .. } => "NearestAttackableTargetGoal",
            }
        }
        Goal::Wither(w) => {
            use vanillite_game::mob::boss::wither::WitherGoal as Wg;
            match w {
                Wg::DoNothing => "WitherDoNothingGoal",
                Wg::RangedAttack { .. } => "RangedAttackGoal",
                Wg::RandomFly { .. } => "WaterAvoidingRandomFlyingGoal",
                Wg::NearestLiving { .. } => "NearestAttackableTargetGoal",
            }
        }
    }
}

/// The Vanilla class of a goal many families share.
fn common_goal_name(goal: &CommonGoal) -> &'static str {
    match goal {
        CommonGoal::NearestAttackable { .. } => "NearestAttackableTargetGoal",
        CommonGoal::Avoid { .. } => "AvoidEntityGoal",
        CommonGoal::Ranged { .. } => "RangedAttackGoal",
        CommonGoal::MoveTowardsRestriction { .. } => "MoveTowardsRestrictionGoal",
        CommonGoal::ResetUniversalAnger { .. } => "ResetUniversalAngerTargetGoal",
        CommonGoal::Move(m) => match m {
            MoveGoal::RemoveBlock {
                sounds: RemoveSounds::ZombieEgg,
                ..
            } => "ZombieAttackTurtleEggGoal",
            MoveGoal::RemoveBlock { .. } => "RemoveBlockGoal",
            MoveGoal::OpenDoor { .. } => "OpenDoorGoal",
            MoveGoal::BreakDoor { .. } => "BreakDoorGoal",
            MoveGoal::BreathAir => "BreathAirGoal",
            MoveGoal::TryFindLiquid { .. } => "TryFindWaterGoal",
            MoveGoal::ClimbOnTopOfPowderSnow => "ClimbOnTopOfPowderSnowGoal",
            _ => "RandomStrollGoal",
        },
    }
}

/// The Vanilla class of a family goal of a mob of `kind`, read from the goal's `Debug` form
/// (`Illager(Spell(UseSpell { spell: 1, .. }))`): the family, its variant and what selects the
/// class inside it.
fn fam_goal_name(kind: Kind, goal: &FamGoal) -> &'static str {
    let debug = format!("{goal:?}");
    let words: Vec<&str> = debug
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .collect();
    let word = |i: usize| words.get(i).copied().unwrap_or("");
    let field = |name: &str| {
        words
            .iter()
            .position(|w| *w == name)
            .map_or("", |i| word(i + 1))
    };
    let (family, variant) = (word(0), word(1));
    match (family, variant) {
        ("Undead", "DrownedMelee") => "DrownedAttackGoal",
        ("Undead", "DrownedTrident") => "DrownedTridentAttackGoal",
        ("Undead", "HurtByIgnoringDrowned") => "HurtByTargetGoal",
        ("Undead", "SkeletonMelee") => "MeleeAttackGoal",
        ("Undead", "CubeFloat") => "CubeMobFloatGoal",
        ("Undead", "CubeAttack") => "CubeMobAttackGoal",
        ("Undead", "CubeRandomDirection") => "CubeMobRandomDirectionGoal",
        ("Undead", "CubeKeepOnJumping") => "CubeMobKeepOnJumpingGoal",
        ("Undead", "PhantomSweep") => "PhantomSweepAttackGoal",
        ("Undead", "PhantomCircle") => "PhantomCircleAroundAnchorGoal",
        ("Undead", "PhantomTargetPlayer") => "PhantomAttackPlayerTargetGoal",
        ("Illager", "Patrol") => "LongDistancePatrolGoal",
        ("Illager", "HoldGround") => "HoldGroundAttackGoal",
        ("Illager", "HurtBy") => "HurtByTargetGoal",
        ("Illager", "LookAt") => "LookAtPlayerGoal",
        ("Illager", "Stroll") => match kind {
            Kind::Pillager | Kind::Witch => "WaterAvoidingRandomStrollGoal",
            _ => "RandomStrollGoal",
        },
        ("Illager", "Locked") => common_name_of(word(2)),
        ("Illager", "Target") => match field("gate") {
            "WitchCanAttack" => "NearestAttackableWitchTargetGoal",
            "Johnny" => "VindicatorJohnnyAttackGoal",
            "NotPeaceful" => "ShulkerNearestAttackGoal",
            "InTeam" => "ShulkerDefenseAttackGoal",
            _ => "NearestAttackableTargetGoal",
        },
        ("Illager", "WitchHeal") => "NearestHealableRaiderTargetGoal",
        ("Illager", "Crossbow") => "RangedCrossbowAttackGoal",
        ("Illager", "SpellCasting") => match kind {
            Kind::Evoker => "EvokerCastingSpellGoal",
            _ => "SpellcasterCastingSpellGoal",
        },
        ("Illager", "Spell") => match field("spell") {
            "1" => "EvokerSummonSpellGoal",
            "2" => "EvokerAttackSpellGoal",
            "3" => "EvokerWololoSpellGoal",
            "4" => "IllusionerMirrorSpellGoal",
            "5" => "IllusionerBlindnessSpellGoal",
            _ => "SpellcasterUseSpellGoal",
        },
        ("Illager", "VexCharge") => "VexChargeAttackGoal",
        ("Illager", "VexCopyOwner") => "VexCopyOwnerTargetGoal",
        ("Illager", "GhastFloat") => "RandomFloatAroundGoal",
        ("Illager", "GhastShoot") => "GhastShootFireballGoal",
        ("Companion", "Guard") => match (word(3), kind) {
            ("Avoid", Kind::Cat) => "CatAvoidEntityGoal",
            ("Avoid", Kind::Ocelot) => "OcelotAvoidEntityGoal",
            ("NearestAttackable", _) if word(2) == "NotTame" => "NonTameRandomTargetGoal",
            (inner, _) => common_name_of(inner),
        },
        ("Companion", "TamePanic") => "TamableAnimalPanicGoal",
        ("Companion", "ClimbPowderSnow") => "ClimbOnTopOfPowderSnowGoal",
        ("Companion", _) => match (variant, word(2)) {
            ("Horse", "RunAround") => "RunAroundLikeCrazyGoal",
            ("Llama", "FollowCaravan") => "LlamaFollowCaravanGoal",
            ("Llama", "AttackWolf") => "LlamaAttackWolfGoal",
            ("Llama", "DefendTrader") => "TraderLlamaDefendWanderingTraderGoal",
            ("Wolf", "AvoidLlama") => "WolfAvoidEntityGoal",
            ("Feline", "RelaxOnOwner") => "CatRelaxOnOwnerGoal",
            ("Feline", "LieOnBlock") => "CatLieOnBedGoal",
            ("Feline", "SitOnBlock") => "CatSitOnBlockGoal",
            ("Feline", "Attack") => "OcelotAttackGoal",
            ("Parrot", "Wander") => "ParrotWanderGoal",
            ("Fox", "Float") => "FoxFloatGoal",
            ("Fox", "Panic") => "FoxPanicGoal",
            ("Fox", "Avoid") => "AvoidEntityGoal",
            ("Fox", "Pounce") => "FoxPounceGoal",
            ("Fox", "EatBerries") => "FoxEatBerriesGoal",
            ("Fox", "StrollThroughVillage") => "FoxStrollThroughVillageGoal",
            ("Fox", "SearchForItems") => "FoxSearchForItemsGoal",
            ("Fox", "DefendTrusted") => "DefendTrustedTargetGoal",
            ("Panda", "BreedCheck") => "PandaBreedGoal",
            ("Panda", inner) => leak(format!("Panda{inner}Goal")),
            ("Bear", "Panic") => "PolarBearPanicGoal",
            ("Bear", "AttackPlayers") => "PolarBearAttackPlayersGoal",
            ("Rabbit", "Panic") => "RabbitPanicGoal",
            ("Rabbit", "Avoid") => "RabbitAvoidEntityGoal",
            (_, inner) => leak(format!("{inner}Goal")),
        },
        ("Mount", "Ghast") => match word(2) {
            "Float" => "HappyGhastFloatGoal",
            "Tempt" => "ForNonPathfinders",
            _ => "RandomFloatAroundGoal",
        },
        ("Wildlife", "Swim") => match kind {
            Kind::Dolphin => "RandomSwimmingGoal",
            _ => "FishSwimGoal",
        },
        ("Wildlife", "Puff") => "PufferfishPuffGoal",
        ("Wildlife", "TurtleStroll") => "TurtleRandomStrollGoal",
        ("Wildlife", "GolemStroll") => "GolemRandomStrollInVillageGoal",
        ("Wildlife", "BeeValidateFlower") => "ValidateFlowerGoal",
        ("Wildlife", "BeeValidateHive") => "ValidateHiveGoal",
        ("Village", "Trader") => match word(2) {
            "LookAt" if field("players") == "true" => "InteractGoal",
            "LookAt" => "LookAtPlayerGoal",
            inner => leak(format!("{inner}Goal")),
        },
        (_, variant) => leak(if variant.ends_with("Goal") {
            variant.to_owned()
        } else {
            format!("{variant}Goal")
        }),
    }
}

/// The Vanilla class of a [`CommonGoal`] variant by name.
fn common_name_of(variant: &str) -> &'static str {
    match variant {
        "NearestAttackable" => "NearestAttackableTargetGoal",
        "Avoid" => "AvoidEntityGoal",
        "Ranged" => "RangedAttackGoal",
        "MoveTowardsRestriction" => "MoveTowardsRestrictionGoal",
        "ResetUniversalAnger" => "ResetUniversalAngerTargetGoal",
        other => leak(format!("{other}Goal")),
    }
}

/// A name for the trace, kept for the process (one per distinct name).
fn leak(name: String) -> &'static str {
    use std::collections::HashSet;
    static NAMES: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut names = NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let names = names.get_or_insert_with(HashSet::new);
    if let Some(n) = names.get(name.as_str()) {
        return n;
    }
    let n: &'static str = Box::leak(name.into_boxed_str());
    names.insert(n);
    n
}

/// `PathType.name()`: `WalkableDoor` → `WALKABLE_DOOR`.
fn path_type_name(t: mob::path::PathType) -> String {
    let mut out = String::new();
    for (i, c) in format!("{t:?}").chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

/// The overworld's environment at `day` ticks of the day in `weather`.
fn environment(spec: &WorldSpec, day: i64, weather: LevelWeather) -> Environment {
    Environment {
        sea_level: sea_level(spec),
        day_time: day,
        ..Environment::overworld(day, weather.rain(), weather.thunder())
    }
}

/// The recorder's default game rules (`ScenarioRecorder.DEFAULT_RULES`: no random ticks, no
/// mobs, no time or weather) with the scenario's rules on top.
/// A new level of `spec` over the chunks in `store`.
fn open_level(
    spec: &WorldSpec,
    store: Arc<MemoryStore>,
) -> Result<(Level, Arc<NetworkIds>), String> {
    let biomes = Arc::clone(BIOME_NAMES.get().expect("installed"));
    let default_biome = biomes
        .iter()
        .position(|b| *b == spec.biome)
        .and_then(|i| u16::try_from(i).ok())
        .ok_or_else(|| format!("unknown biome `{}`", spec.biome))?;
    let info = WorldInfo {
        dimension: "minecraft:overworld".to_owned(),
        dir: std::env::temp_dir(),
        min_y: MIN_Y,
        height: HEIGHT,
        seed: spec.seed,
        has_sky_light: true,
        biomes,
        default_biome,
        air: BlockState::AIR.0,
    };
    let mut layers = Vec::new();
    for (block, height) in &spec.layers {
        let state = BlockId::by_name(block)
            .ok_or_else(|| format!("unknown layer block `{block}`"))?
            .default_state();
        layers.extend(std::iter::repeat_n(state.0, *height as usize));
    }
    let codec = AnvilCodec::new(&info, Compression::None);
    let generator: Arc<dyn vanillite_chunk::slots::ChunkGenerator> = if spec.snapshot.is_some() {
        let biome_source =
            json!({"type": "minecraft:multi_noise", "preset": "minecraft:overworld"});
        let terrain = vanillite_worldgen::TerrainGenerator::new(
            PACKS.get().expect("installed"),
            vanillite_worldgen::DimensionSpec {
                noise_settings: "minecraft:overworld",
                biome_source: &biome_source,
                min_y: MIN_Y,
                height: HEIGHT as i32,
                has_sky_light: true,
            },
            spec.seed,
        )
        .map_err(|e| format!("overworld generator: {e}"))?;
        Arc::new(
            vanillite_worldgen::VanillaChunkGenerator::new(terrain, info.clone())
                .with_structures(false),
        )
    } else {
        Arc::new(FlatGenerator::new(&info, &layers))
    };
    let slots = ChunkSlots {
        store,
        codec: Arc::new(codec),
        generator,
        light: if spec.light {
            vanillite_light::BfsLightEngineProvider.create(&info)
        } else {
            Arc::new(NoLight)
        },
        executor: Arc::new(InlineExecutor),
    };
    let chunks = Chunks::new(info, ChunkSettings::default(), slots);
    let mut settings = LevelSettings::default();
    apply_rules(&mut settings, &mut GameRules::default(), &spec.rules)?;
    let network_ids = Arc::new(NetworkIds::new());
    let mut level = Level::new(
        chunks,
        settings,
        Arc::new(Behaviours::vanilla()),
        Arc::clone(&network_ids),
        spec.seed,
    );
    level.set_tree_features(Arc::clone(TREES.get().expect("installed")));
    // Structure blocks and `/place template`: the data packs' templates and what the scenario
    // saves (in memory, its own per world).
    level.set_structure_templates(Arc::new(
        vanillite_worldgen::template::StructureTemplateManager::from_roots(
            PACKS.get().map(|p| p.roots().to_vec()).unwrap_or_default(),
        ),
    ));
    level.set_climate(Arc::new(Climate {
        biomes: BIOMES.get().expect("installed").clone(),
        manager: vanillite_worldgen::biome::BiomeManager::for_world_seed(spec.seed),
        sea_level: sea_level(spec),
    }));
    let (rain, thunder) = match spec.weather.as_str() {
        "clear" => (0.0, 0.0),
        "rain" => (1.0, 0.0),
        "thunder" => (1.0, 1.0),
        w => return Err(format!("unknown weather `{w}`")),
    };
    level.set_weather(LevelWeather {
        rain_level: rain,
        o_rain_level: rain,
        thunder_level: thunder,
        o_thunder_level: thunder,
    });
    Ok((level, network_ids))
}

/// The weather timers the recorder sets for the scenario's weather (`setWeatherParameters`):
/// rain and thunder last, clear is the cycle of a new world.
fn weather_data(spec: &WorldSpec) -> WeatherData {
    let mut data = WeatherData::default();
    match spec.weather.as_str() {
        "rain" => data.set(0, 1_000_000, true, false),
        "thunder" => data.set(0, 1_000_000, true, true),
        _ => {}
    }
    data
}

fn apply_rules(
    settings: &mut LevelSettings,
    game_rules: &mut GameRules,
    rules: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    settings.random_tick_speed = 0;
    for name in [
        "random_tick_speed",
        "spawn_mobs",
        "advance_time",
        "advance_weather",
    ] {
        if let Some(rule) = Rule::by_name(name) {
            game_rules.set(rule, 0);
        }
    }
    for (name, value) in rules {
        let bad = || format!("game rule {name}: bad value `{value}`");
        let supported = matches!(
            name.as_str(),
            "random_tick_speed"
                | "water_source_conversion"
                | "lava_source_conversion"
                | "block_drops"
                | "entity_drops"
                | "max_snow_accumulation_height"
                | "fire_spread_radius_around_player"
                | "spread_vines"
                | "spawn_mobs"
                | "spawn_monsters"
                | "ender_pearls_vanish_on_death"
                | "advance_time"
                | "advance_weather"
                | "mob_griefing"
                | "mob_drops"
                | "tnt_explodes"
                | "tnt_explosion_drop_decay"
                | "players_sleeping_percentage"
                | "natural_health_regeneration"
        );
        let rule = Rule::by_name(name)
            .filter(|_| supported)
            .ok_or_else(|| format!("game rule {name} is not supported by the game world"))?;
        let v = match value.as_str() {
            "true" => 1,
            "false" => 0,
            v => v.parse().map_err(|_| bad())?,
        };
        game_rules.set(rule, v);
    }
    settings.apply_game_rules(game_rules);
    Ok(())
}

impl ScenarioWorld for GameWorld {
    /// Like the recorder's `getChunkNow`, which puts the chunk into the chunk cache.
    fn is_loaded(&self, pos: BlockPos) -> bool {
        self.level.is_loaded(pos) && self.level.chunks.chunk_now(pos.chunk())
    }

    fn set_forced(&mut self, chunks: &[ChunkPos], forced: bool) -> Result<(), String> {
        for &pos in chunks {
            let Level {
                chunks: runtime,
                forced: set,
                ..
            } = &mut self.level;
            set.set(runtime, pos, forced);
        }
        for _ in 0..SETTLE_LIMIT {
            // The chunk system and the entity manager only: no game tick.
            self.level.tick_chunk_runtime();
            self.level.tick_entity_manager();
            if self.settled() {
                return Ok(());
            }
        }
        Err("the chunk system does not settle".to_owned())
    }

    /// `MinecraftServer.stopServer` (every chunk saved with its entities, block entities and
    /// scheduled ticks, the chunk tickets and the random sequences), then a new level on the saved chunks: the saved
    /// tickets come back and `prepareLevels` runs the chunk system until their chunks are full.
    /// The game time is kept like `level.dat` keeps it.
    fn restart(&mut self) -> Result<(), String> {
        if !self.names.is_empty() || !self.level.entities.players.is_empty() {
            return Err("scenarios with entities cannot restart".to_owned());
        }
        self.level.save(true);
        let tickets = self.level.forced.save_tickets(&SavedTickets::default());
        // Through the saved file's bytes, like the saved data store.
        let mut bytes = Vec::new();
        vanillite_nbt::write::write_named("", &tickets, &mut bytes).map_err(|e| e.to_string())?;
        let budget = &mut vanillite_nbt::Budget::new(
            vanillite_nbt::read::FILE_QUOTA,
            vanillite_nbt::read::MAX_DEPTH,
        );
        let tickets = vanillite_nbt::read::read_named(&mut bytes.as_slice(), budget)
            .map_err(|e| e.to_string())?;
        // The server's random sequences (`random_sequences.dat`) the same way.
        let sequences = vanillite_game::random_sequences::SAVED_DATA;
        let saved = self
            .level
            .random_sequences()
            .lock()
            .map_err(|e| e.to_string())?
            .to_nbt();
        let mut bytes = Vec::new();
        vanillite_nbt::write::write_named(sequences, &saved, &mut bytes)
            .map_err(|e| e.to_string())?;
        let budget = &mut vanillite_nbt::Budget::new(
            vanillite_nbt::read::FILE_QUOTA,
            vanillite_nbt::read::MAX_DEPTH,
        );
        let saved = vanillite_nbt::read::read_named(&mut bytes.as_slice(), budget)
            .map_err(|e| e.to_string())?;
        let game_time = self.level.game_time();
        let (mut level, network_ids) = open_level(&self.spec, Arc::clone(&self.store))?;
        level.spawner_env.env = self.env;
        level.set_random_sequences(Arc::new(std::sync::Mutex::new(
            vanillite_game::random_sequences::RandomSequences::load(self.spec.seed, &saved),
        )));
        level.set_game_time(game_time);
        let Level { chunks, forced, .. } = &mut level;
        forced.load_tickets(chunks, &tickets);
        self.level = level;
        self.weather = weather_data(&self.spec);
        self.network_ids = network_ids;
        self.mobs = MobContext::default();
        self.effects.clear();
        for _ in 0..SETTLE_LIMIT {
            self.level.tick_chunk_runtime();
            if self.level.chunks.pending_full() == 0 {
                return Ok(());
            }
        }
        Err("the saved forced chunks do not load".to_owned())
    }

    fn bonemeal(&mut self, pos: BlockPos) -> Result<(), String> {
        let bone_meal = vanillite_data::item::ItemId::by_name("minecraft:bone_meal")
            .ok_or("no bone meal item")?;
        let mut stack = vanillite_game::item::ItemStack::new(bone_meal.0, 1);
        vanillite_game::block::bonemeal::grow_crop(&mut self.level, pos, &mut stack);
        Ok(())
    }

    fn set_block_raw(&mut self, pos: BlockPos, state: BlockState) {
        self.level
            .set_block(pos, state, update::CLIENTS | update::KNOWN_SHAPE);
    }

    fn place_block(&mut self, pos: BlockPos, state: BlockState) {
        self.level.set_block(pos, state, update::ALL);
    }

    fn block(&self, pos: BlockPos) -> BlockState {
        self.level.block(pos)
    }

    /// The simulation phases of `ServerLevel.tick`: game time, scheduled block then fluid
    /// ticks, the chunk source (post-processing, random ticks), block events, entities, block
    /// entities, entity management (pending entity loads).
    fn step(&mut self) {
        // `MinecraftServer.tickChildren`: the clocks advance before the levels tick.
        if self.rules.bool(Rule::ADVANCE_TIME) {
            self.clock_time += 1;
        }
        // `ServerLevel.tick`: `advanceWeatherCycle` with the level random.
        let advance_weather = self.rules.bool(Rule::ADVANCE_WEATHER);
        let mut weather = self.level.weather();
        weather.advance(
            Some(&mut self.weather),
            advance_weather,
            self.level.random(),
            &mut Vec::new(),
        );
        self.level.set_weather(weather);
        // The night skip; rain stops with `advance_weather` (`resetWeatherCycle`).
        if vanillite_game::survival::sleep::skip_night(
            &mut self.level,
            &vanillite_game::hooks::NoHooks,
        ) && advance_weather
            && weather.is_raining()
        {
            self.weather.reset_cycle();
        }
        // `updateSkyBrightness`: what this tick's block ticks read of the sky.
        if self.env.day_time != self.clock_time || advance_weather {
            self.env = environment(&self.spec, self.clock_time, weather);
            self.level.spawner_env.env = self.env;
            self.level.entities.day_time = self.env.day_time;
        }
        let level = &mut self.level;
        level.tick_time();
        level.run_scheduled_ticks();
        level.chunks.purge_stale_tickets();
        level.tick_chunk_runtime();
        level.post_process();
        let mut scratch = Vec::new();
        level.tick_chunks(&mut scratch, &vanillite_game::level::tiles::Phases::INLINE);
        level.run_block_events();
        self.tick_entities();
        let level = &mut self.level;
        level.tick_block_entities();
        level.tick_entity_manager();
        // Nobody watches: drop what would go to clients.
        *level.output() = LevelOutput::default();
        let mut changes = Vec::new();
        level.chunks.take_block_changes(&mut changes);
        // The light of this tick's changes is there when the next tick starts.
        level.chunks.flush_light();
        let mut light = Vec::new();
        level.chunks.take_light_changes(&mut light);
        self.tick_players();
        self.tick_client_connections();
    }

    fn settle_light(&mut self) {
        self.level.chunks.flush_light();
    }

    fn block_ticks(&self, min: BlockPos, max: BlockPos, out: &mut Vec<TickObs>) {
        observe_ticks(
            &self.level,
            self.level.chunks.block_ticks(),
            min,
            max,
            out,
            |kind| BlockId(kind).name().to_owned(),
        );
    }

    fn fluid_ticks(&self, min: BlockPos, max: BlockPos, out: &mut Vec<TickObs>) {
        observe_ticks(
            &self.level,
            self.level.chunks.fluid_ticks(),
            min,
            max,
            out,
            |kind| {
                vanillite_data::registry::FLUID
                    .name(kind)
                    .unwrap_or("?")
                    .to_owned()
            },
        );
    }

    fn block_entities(&self, min: BlockPos, max: BlockPos, out: &mut Vec<String>) {
        observe_block_entities(&self.level, |p| self.is_loaded(p), min, max, out);
    }

    fn light(&mut self, min: BlockPos, max: BlockPos, out: &mut Vec<u8>) {
        if !self.light {
            return;
        }
        let chunks = &mut self.level.chunks;
        chunks.flush_light();
        let layers = HEIGHT as usize / 16 + 2;
        let mut storing = std::collections::HashMap::new();
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                for x in min.x..=max.x {
                    let pos = BlockPos::new(x, y, z);
                    let storing = storing
                        .entry(pos.chunk())
                        .or_insert_with(|| storing_sections(chunks, pos.chunk(), layers));
                    out.push(visible_light(chunks, storing, pos));
                }
            }
        }
    }

    fn entities(&self, min: BlockPos, max: BlockPos, out: &mut Vec<EntityObs>) {
        let core = self.level.entities.store.columns();
        for row in 0..core.id.len() {
            let pos = core.pos[row];
            let block = BlockPos::new(
                pos.x.floor() as i32,
                pos.y.floor() as i32,
                pos.z.floor() as i32,
            );
            if !inside(block, min, max) {
                continue;
            }
            // A player moves by its client: its server-side motion is not part of the trace.
            let v = if core.kind[row] == ids::PLAYER {
                Vec3::ZERO
            } else {
                core.velocity[row]
            };
            out.push(EntityObs {
                kind: core.kind[row].name().to_owned(),
                pos: [pos.x, pos.y, pos.z],
                motion: [v.x, v.y, v.z],
                detail: self.detail(row),
            });
        }
    }

    /// Players, the level random reseeded with the world seed, then the other entities.
    fn add_entities(&mut self, entities: &[EntitySpec], entity_seed: i64) -> Result<(), String> {
        for (i, spec) in entities.iter().enumerate() {
            if spec.is_player() && spec.join.is_none() {
                self.add_player(spec, i)?;
            }
        }
        *self.level.random() = LegacyRandom::new(self.seed);
        self.level.set_rand_value(self.seed as i32);
        for (i, spec) in entities.iter().enumerate() {
            if !spec.is_player() {
                self.add_mob(spec, i)?;
            }
        }
        self.level.set_entity_seed(entity_seed);
        for (id, name) in &self.names {
            if let Some(e) = self.level.entities.store.get(*id) {
                self.network_names.push((name.clone(), e.network_id().0));
            }
        }
        vanillite_game::experience_orb::seed_entity_random(entity_seed);
        Ok(())
    }

    fn join(&mut self, spec: &EntitySpec, index: usize) -> Result<(), String> {
        self.add_player(spec, index)?;
        let id = self.named(&spec.name)?;
        if let Some(e) = self.level.entities.store.get(id) {
            self.network_names
                .push((spec.name.clone(), e.network_id().0));
        }
        Ok(())
    }

    fn lightning(&mut self, column: BlockPos) -> Result<(), String> {
        let input = lightning::StrikeInput {
            spawn_mobs: self.rules.bool(Rule::SPAWN_MOBS),
            clock_time: self.clock_time,
        };
        lightning::strike(&mut self.level, column, input);
        Ok(())
    }

    fn level_rng(&mut self) -> Option<i64> {
        (self.level_rng || !self.names.is_empty()).then(|| self.level.random().state())
    }

    fn hurt(&mut self, hurt: &Hurt) -> Result<(), String> {
        let target = self.named(&hurt.entity)?;
        let by = hurt.by.as_deref().map(|b| self.named(b)).transpose()?;
        let is_player = by.is_some_and(|b| self.level.entities.player(b).is_some());
        self.with_env(|level, ents, env, cx| {
            let source = match by {
                Some(b) if is_player => combat::entity_source(ents, env.types.player_attack, b, b),
                Some(b) => combat::entity_source(ents, env.types.mob_attack, b, b),
                None => DamageSource::of(env.types.generic),
            };
            combat::hurt(level, ents, env, cx, target, source, hurt.amount);
        });
        // What the hurt queued (the death's game event reaches a sculk catalyst at once, like
        // Vanilla's `gameEvent`).
        self.apply_actions();
        self.finish_removals();
        self.spawn_drops();
        Ok(())
    }

    fn command(&mut self, line: &str) -> Result<(), String> {
        // Command blocks run on the same server's dispatcher.
        if self.level.command_runner().is_none() {
            vanillite_server::install_command_runner(command_server(), &mut self.level);
        }
        vanillite_server::run_command_on_level(
            command_server(),
            &mut self.level,
            Vec3::ZERO,
            line,
        )?;
        self.finish_removals();
        self.spawn_drops();
        Ok(())
    }

    fn use_item(&mut self, op: &crate::scenario::UseItemOp) -> Result<(), String> {
        use crate::scenario::UseMode;
        use vanillite_protocol::packets::play::serverbound::Hand;
        let player = self.named(&op.entity)?;
        let stack_of = |item: &str, count: i32, components: Option<&str>| {
            let mut c = vanillite_nbt::Compound::new();
            c.insert("id", vanillite_nbt::Tag::String(item.to_owned()));
            c.insert("count", vanillite_nbt::Tag::Int(count));
            if let Some(snbt) = components {
                let components =
                    vanillite_nbt::snbt::parse_compound(snbt).map_err(|e| e.to_string())?;
                c.insert("components", vanillite_nbt::Tag::Compound(components));
            }
            let stack = vanillite_game::item_nbt::stack_from_nbt(&c);
            if stack.is_empty() {
                return Err(format!("unknown item {item}"));
            }
            Ok(stack)
        };
        if let Some(item) = &op.item {
            let stack = stack_of(item, op.count, op.components.as_deref())?;
            if let Some(p) = self.level.entities.player_mut(player) {
                let slot = p.inventory.selected;
                p.inventory.set(slot, stack);
            }
        }
        for entry in &op.inventory {
            let stack = stack_of(&entry.item, entry.count, entry.components.as_deref())?;
            if let Some(p) = self.level.entities.player_mut(player) {
                p.inventory.set(entry.slot, stack);
            }
        }
        if let Some(food) = op.food
            && let Some(p) = self.level.entities.player_mut(player)
        {
            p.survival.food.food_level = food;
        }
        if let Some(target) = &op.target {
            // `Player.interactOn` with the stack in the main hand.
            let target = self.named(target)?;
            self.with_env(|level, ents, env, cx| {
                mob::animal::interact_on(
                    level,
                    ents,
                    env,
                    cx,
                    player,
                    target,
                    Hand::MainHand,
                    false,
                );
            });
            self.spawn_drops();
            return Ok(());
        }
        if op.mode == UseMode::Use {
            // The `use_item` packet handler first (buckets, bottles, books, ...); what it
            // leaves to the entities follows (`EntityAction::UseItem`).
            let handled = self.click(&crate::scenario::PlayerUse {
                entity: op.entity.clone(),
                attack: false,
                air: true,
                pos: BlockPos::new(0, 0, 0),
                face: "up".to_owned(),
                hit: None,
                item: None,
                count: op.count,
                components: None,
                offhand: false,
                sneak: false,
                food: None,
            })?;
            if handled {
                return Ok(());
            }
        }
        if let Some(distance) = op.fall_distance
            && let Some(p) = self.level.entities.player_mut(player)
        {
            p.survival.fall_distance = distance;
        }
        let target = match &op.mode {
            UseMode::Attack(name) => Some(self.named(name)?),
            _ => None,
        };
        let network_ids = Arc::clone(&self.network_ids);
        self.with_env(|level, ents, env, cx| match (&op.mode, target) {
            (UseMode::Use, _) => vanillite_game::player::item_use::use_item(
                level,
                ents,
                env,
                cx,
                &network_ids,
                player,
                Hand::MainHand,
            ),
            (UseMode::Release, _) => {
                vanillite_game::player::using::release(ents, env, &network_ids, player);
            }
            (UseMode::Attack(_), Some(target)) => {
                vanillite_game::player::item_use::attack_entity(
                    level, ents, env, cx, player, target,
                );
            }
            (UseMode::Attack(_), None) => {}
            (UseMode::Stab, _) => {
                vanillite_game::spear::player_stab(level, ents, env, cx, player);
            }
        });
        self.apply_actions();
        self.finish_removals();
        self.spawn_drops();
        Ok(())
    }

    /// `ServerPlayerGameMode.useItemOn` (through `handleUseItemOn`) or `START_DESTROY_BLOCK`
    /// of a scenario player, on the real packet handlers.
    fn player_use(&mut self, click: &crate::scenario::PlayerUse) -> Result<(), String> {
        self.click(click).map(drop)
    }

    fn packet(&mut self, op: &crate::scenario::PacketOp) -> Result<(), String> {
        let id = self.named(&op.entity)?;
        let target = match &op.target {
            Some(t) => Some(self.target(t)?),
            None => None,
        };
        let packet_id = vanillite_protocol::ids::play::serverbound::NAMES
            .iter()
            .position(|n| n.strip_prefix("minecraft:").unwrap_or(n) == op.packet)
            .ok_or_else(|| format!("unknown serverbound play packet `{}`", op.packet))?;
        let mut bytes = Vec::new();
        vanillite_protocol::codec::write_varint(
            &mut bytes,
            i32::try_from(packet_id).map_err(|e| e.to_string())?,
        );
        bytes.extend(op.body(target)?);
        let packet = vanillite_protocol::sets::ServerboundPlay::decode(&bytes)
            .map_err(|e| format!("`{}` does not decode: {e}", op.packet))?;
        self.send(id, packet)
    }

    fn client_begin(&mut self) -> Result<(), String> {
        use vanillite_protocol::packets::play::serverbound::{AcceptTeleportation, PlayerLoaded};
        use vanillite_protocol::sets::ServerboundPlay;
        let ids: Vec<EntityId> = self.clients.iter().map(|c| c.id).collect();
        let joining: Vec<EntityId> = self
            .clients
            .iter_mut()
            .filter_map(|c| (!std::mem::replace(&mut c.joined, true)).then_some(c.id))
            .collect();
        for id in ids {
            let Some(p) = self.level.entities.player(id) else {
                continue;
            };
            let Some((teleport, to)) = p.connection.movement.pending_teleport() else {
                continue;
            };
            let rot = self
                .level
                .entities
                .store
                .get(id)
                .map(|e| e.rot())
                .ok_or("the player is gone")?;
            self.send(
                id,
                ServerboundPlay::AcceptTeleportation(AcceptTeleportation {
                    id: teleport,
                    x: to.x,
                    y: to.y,
                    z: to.z,
                    y_rot: rot.y_rot,
                    x_rot: rot.x_rot,
                }),
            )?;
        }
        for id in joining {
            self.send(id, ServerboundPlay::PlayerLoaded(PlayerLoaded))?;
        }
        Ok(())
    }

    fn client_end(&mut self) -> Result<(), String> {
        use vanillite_protocol::packets::play::serverbound::ClientTickEnd;
        use vanillite_protocol::sets::ServerboundPlay;
        let ids: Vec<EntityId> = self.clients.iter().map(|c| c.id).collect();
        if self.spec.client_tick_end {
            for id in ids {
                self.send(id, ServerboundPlay::ClientTickEnd(ClientTickEnd))?;
            }
        }
        self.handle_clients()
    }

    fn throw_potion(&mut self, op: &crate::scenario::ThrowPotionOp) -> Result<(), String> {
        use vanillite_game::effect::potion::PotionContents;
        use vanillite_game::effect::thrown::{self, ThrownPotion};
        let by = op.by.as_deref().map(|b| self.named(b)).transpose()?;
        let item = vanillite_data::registry::ITEM
            .id(&op.item)
            .ok_or_else(|| format!("unknown item {}", op.item))?;
        let potion = vanillite_data::registry::POTION
            .id(&op.potion)
            .ok_or_else(|| format!("unknown potion {}", op.potion))?;
        let mut stack = vanillite_game::item::ItemStack::new(item, 1);
        stack.set_component(
            vanillite_protocol::components::ids::POTION_CONTENTS,
            PotionContents::of(potion).encode(),
        );
        let kind = thrown::kind_of(&stack).ok_or("throw_potion needs a throwable potion")?;
        let ents = &mut self.level.entities;
        let owner_uuid = by.and_then(|b| ents.store.get(b)).map(|e| e.uuid());
        let uuid = 0x7072_6f6a_0000_0000_0000_0000_0000_0000_u128 | u128::from(potion);
        let [x, y, z] = op.pos;
        let [dx, dy, dz] = op.motion;
        let mut new = vanillite_world::entity::NewEntity::new(
            kind,
            self.network_ids.next(),
            uuid,
            Vec3::new(x, y, z),
        );
        new.velocity = Vec3::new(dx, dy, dz);
        self.level.entities.spawn_special(
            new,
            vanillite_game::mob::boss::special::SpecialData::ThrownPotion(Box::new(ThrownPotion {
                owner: by,
                owner_uuid,
                stack,
                left_owner: false,
                tick_count: 0,
                has_been_shot: false,
            })),
        );
        Ok(())
    }

    fn effect(&mut self, effect: &crate::scenario::EffectOp) -> Result<(), String> {
        let target = self.named(&effect.entity)?;
        let kind = vanillite_game::effect::MobEffect::by_name(&effect.effect)
            .ok_or_else(|| format!("unknown effect {}", effect.effect))?;
        let instance =
            vanillite_game::effect::EffectInstance::new(kind, effect.duration, effect.amplifier);
        self.with_env(|level, ents, env, cx| {
            vanillite_game::effect::living::add_to_entity(level, ents, env, cx, target, instance);
        });
        self.finish_removals();
        self.spawn_drops();
        Ok(())
    }
}

/// Whether Vanilla keeps light data for each light section of a chunk (index 0 is the layer
/// below the world): those within one section in every direction of a non-air section
/// (`LayerLightSectionStorage.updateSectionStatus`).
fn storing_sections(chunks: &Chunks, pos: ChunkPos, layers: usize) -> Vec<bool> {
    let mut storing = vec![false; layers];
    for dz in -1..=1 {
        for dx in -1..=1 {
            let Some(chunk) = chunks.chunk(ChunkPos::new(pos.x + dx, pos.z + dz)) else {
                continue;
            };
            for (section, s) in chunk.chunk.sections().iter().enumerate() {
                if !s.summary().is_empty() {
                    // Section `section` is light layer `section + 1`.
                    let end = (section + 3).min(layers);
                    storing[section..end].fill(true);
                }
            }
        }
    }
    storing
}

/// The light a client sees at `pos` as `sky << 4 | block`: the stored levels in sections with
/// light data; elsewhere no block light and the sky light of the bottom row of the next section
/// with data above, or full sky light (`SkyLightSectionStorage.getLightValue`).
fn visible_light(chunks: &Chunks, storing: &[bool], pos: BlockPos) -> u8 {
    let Some(chunk) = chunks.chunk(pos.chunk()) else {
        return 0;
    };
    let light = chunk.chunk.light();
    let (x, y, z) = (
        (pos.x & 15) as u32,
        (pos.y & 15) as u32,
        (pos.z & 15) as u32,
    );
    let sky_at = |layer: usize, y: u32| match &light.sky()[layer] {
        LightNibbles::Absent => 15,
        data => data.get(x, y, z),
    };
    let layer = ((pos.y - MIN_Y) >> 4) as usize + 1;
    if storing[layer] {
        return (sky_at(layer, y) << 4) | light.block()[layer].get(x, y, z);
    }
    let sky = (layer + 1..storing.len())
        .find(|&l| storing[l])
        .map_or(15, |l| sky_at(l, 0));
    sky << 4
}

pub(crate) fn inside(p: BlockPos, min: BlockPos, max: BlockPos) -> bool {
    p.x >= min.x && p.y >= min.y && p.z >= min.z && p.x <= max.x && p.y <= max.y && p.z <= max.z
}

/// Pending ticks inside `min..=max` in drain order, delays relative to the game time.
/// The block entities of the loaded positions inside `min..=max` as `x,y,z <canonical nbt>`
/// ([`crate::world::ScenarioWorld::block_entities`]).
pub(crate) fn observe_block_entities(
    level: &Level,
    is_loaded: impl Fn(BlockPos) -> bool,
    min: BlockPos,
    max: BlockPos,
    out: &mut Vec<String>,
) {
    for y in min.y..=max.y {
        for z in min.z..=max.z {
            for x in min.x..=max.x {
                let pos = BlockPos::new(x, y, z);
                if !is_loaded(pos) || level.block(pos).block().info().block_entity == u16::MAX {
                    continue;
                }
                if let Some(nbt) = level.block_entity_nbt(pos) {
                    let mut s = format!("{x},{y},{z} ");
                    crate::trace::canonical_nbt(&mut s, &vanillite_nbt::Tag::Compound(nbt));
                    out.push(s);
                }
            }
        }
    }
}

pub(crate) fn observe_ticks(
    level: &Level,
    ticks: &LevelTicks,
    min: BlockPos,
    max: BlockPos,
    out: &mut Vec<TickObs>,
    name: impl Fn(u16) -> String,
) {
    let mut all = Vec::new();
    for cx in min.x >> 4..=max.x >> 4 {
        for cz in min.z >> 4..=max.z >> 4 {
            all.extend(
                ticks
                    .ticks_in(ChunkPos::new(cx, cz))
                    .filter(|t| inside(t.pos, min, max)),
            );
        }
    }
    all.sort_by_key(LevelTicks::drain_order);
    let now = level.game_time();
    out.extend(all.into_iter().map(|t| TickObs {
        pos: t.pos,
        kind: name(t.kind),
        delay: t.trigger - now,
        priority: i32::from(t.priority.0),
    }));
}

/// The packet-level players: their packets go through the real handlers
/// (`Connection::handle_packets`), then the moves and entity actions run like in the server
/// world's tick (`WorldPlayers::handle_packets`, `WorldSurvival::moves`,
/// `WorldMobs::player_actions`); keep them in step.
impl GameWorld {
    /// The network id and UUID of a packet target: a scenario entity, `uuid:<hex>` or `none`
    /// (-1 and the nil UUID when there is no such entity).
    fn target(&self, name: &str) -> Result<(i32, u128), String> {
        let core = self.level.entities.store.columns();
        let by_uuid = |uuid: u128| {
            core.uuid
                .iter()
                .position(|u| *u == uuid)
                .map_or((-1, uuid), |row| (core.network_id[row].0, uuid))
        };
        if name == "none" {
            return Ok((-1, 0));
        }
        if let Some(hex) = name.strip_prefix("uuid:") {
            let uuid = u128::from_str_radix(hex, 16).map_err(|_| format!("bad target `{name}`"))?;
            return Ok(by_uuid(uuid));
        }
        let network_id = self
            .network_names
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| *id)
            .ok_or_else(|| format!("no scenario entity `{name}`"))?;
        let uuid = core
            .network_id
            .iter()
            .position(|n| n.0 == network_id)
            .map_or(0, |row| core.uuid[row]);
        Ok((network_id, uuid))
    }

    /// Queues a packet of the client player `id`.
    fn send(
        &mut self,
        id: EntityId,
        packet: vanillite_protocol::sets::ServerboundPlay,
    ) -> Result<(), String> {
        let client = self
            .clients
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or("packets need a client player (`\"client\": true`)")?;
        client.seq += 1;
        let inbound = vanillite_protocol::Inbound {
            seq: client.seq,
            packet,
        };
        if client.tx.try_send(inbound) {
            Ok(())
        } else {
            Err("the client's packet queue is full".to_owned())
        }
    }

    /// The join of a client player: the server teleports it to where it stands
    /// (`placeNewPlayer`); the client accepts and reports the world loaded at the start of its
    /// first tick ([`ScenarioWorld::client_begin`]).
    fn join_client(&mut self, id: EntityId) -> Result<(), String> {
        let Entities { store, players, .. } = &mut self.level.entities;
        let entity = store.get(id).ok_or("the player is gone")?;
        let player = players.get_mut(entity.kind_row()).ok_or("no player data")?;
        let rot = entity.rot();
        let mut body = vanillite_game::player::movement::Body {
            pos: entity.pos(),
            velocity: entity.velocity(),
            y_rot: rot.y_rot,
            x_rot: rot.x_rot,
            on_ground: false,
            horizontal_collision: false,
            fall_flying: false,
        };
        let pos = body.pos;
        player
            .connection
            .movement
            .teleport(&mut body, pos, rot.y_rot, rot.x_rot, &NoSink);
        Ok(())
    }

    /// Handles the queued packets of the client players, then their moves and entity actions.
    fn handle_clients(&mut self) -> Result<(), String> {
        use vanillite_config::GameMode;
        use vanillite_game::player::connection::{ChatContext, Handled, PlayerState};
        use vanillite_game::player::movement::{Body, MoveContext};
        use vanillite_game::player::play::PlayContext;
        use vanillite_game::survival::rules::Rule;
        use vanillite_world::entity::{EntityFlags, Pose};
        if self.clients.is_empty() {
            return Ok(());
        }
        let data = vanillite_game::player::play::game_data().ok_or("no game data")?;
        let chat = ChatSettings::default();
        // The scenario world runs the `vanilla` profile.
        let movement_rules =
            <vanillite_config::WorldRules as vanillite_config::RuleSet>::for_profile(
                vanillite_config::Profile::Vanilla,
            )
            .movement;
        let ctx = MoveContext {
            runs_normally: true,
            rules: &movement_rules,
            no_wrong_move_check: false,
            no_physics: false,
            dims: vanillite_world::entity::Dimensions {
                width: 0.6,
                height: 1.8,
            },
            blocks: None,
            movement_check: self.rules.bool(Rule::PLAYER_MOVEMENT_CHECK),
            elytra_movement_check: self.rules.bool(Rule::ELYTRA_MOVEMENT_CHECK),
        };
        let ids: Vec<EntityId> = self.clients.iter().map(|c| c.id).collect();
        let mut players = std::mem::take(&mut self.level.entities.players);
        let spectators = vanillite_base::hash::FxHashSet::default();
        let mut kicked = None;
        for &id in &ids {
            let Some(entity) = self.level.entities.store.get(id) else {
                continue;
            };
            let Some(player) = players.get_mut(entity.kind_row()) else {
                continue;
            };
            let flags = entity.flags();
            let rot = entity.rot();
            let before = PlayerState {
                body: Body {
                    pos: entity.pos(),
                    velocity: entity.velocity(),
                    y_rot: rot.y_rot,
                    x_rot: rot.x_rot,
                    on_ground: flags.contains(EntityFlags::ON_GROUND),
                    horizontal_collision: false,
                    fall_flying: flags.contains(EntityFlags::FALL_FLYING),
                },
                shift_key_down: flags.contains(EntityFlags::CROUCHING),
                sprinting: flags.contains(EntityFlags::SPRINTING),
                client: None,
                respawn: false,
                flying: player.survival.flying,
            };
            let mut state = before.clone();
            let actor = vanillite_server::player_actor(
                &entity,
                player.game_mode,
                &player.survival.effects,
                &player.inventory,
                0,
                &self.level.chunks,
            );
            let (aabb, uuid, dims) = (entity.aabb(), entity.uuid(), entity.dims());
            let mut effects = vanillite_server::LevelEffects::default();
            let mut drops = Vec::new();
            let mut swings = Vec::new();
            let protected = |_: BlockPos| false;
            let hooks = vanillite_game::hooks::NoHooks;
            let handled = {
                let mut world = vanillite_server::WorldLevel {
                    level: &mut self.level,
                    effects: &mut effects,
                    spectators: &spectators,
                    actor: Some((id, aabb)),
                    dimension: "minecraft:overworld",
                };
                let mut play = PlayContext {
                    level: &mut world,
                    inventory: &mut player.inventory,
                    xp: Some(&mut player.survival.xp),
                    random: Some(&mut player.survival.random),
                    food: Some(&mut player.survival.food),
                    recipe_book: Some(&mut player.recipe_book),
                    actor,
                    data: &data,
                    spawn_protected: &protected,
                    drops: &mut drops,
                    swings: &mut swings,
                    hooks: &hooks,
                    stats: Some(&mut player.stats),
                    advancements: Some(&mut player.advancements),
                };
                let handled = player.connection.handle_packets(
                    &mut state,
                    MoveContext {
                        no_wrong_move_check: matches!(
                            player.game_mode,
                            GameMode::Spectator | GameMode::Creative
                        ),
                        no_physics: player.game_mode == GameMode::Spectator,
                        dims,
                        ..ctx
                    },
                    ChatContext {
                        uuid,
                        name: &player.profile.name,
                        settings: &chat,
                    },
                    &mut play,
                    usize::MAX,
                );
                let actor = play.actor;
                for drop in drops {
                    vanillite_game::player::play::throw_item(
                        &mut world,
                        &actor,
                        drop,
                        &mut player.survival.own_random,
                    );
                }
                handled
            };
            match handled {
                Handled::Ok => {}
                Handled::Disconnected => kicked = Some(format!("{id:?} disconnected")),
                Handled::Kick(reason) => kicked = Some(format!("{id:?} kicked: {reason:?}")),
            }
            let Some(mut entity) = self.level.entities.store.get_mut(id) else {
                continue;
            };
            // `WorldPlayers::handle_packets`: the state the packets changed.
            let body = state.body;
            if body.pos != before.body.pos {
                entity.set_pos(body.pos);
            }
            if body.velocity != before.body.velocity {
                entity.set_velocity(body.velocity);
            }
            if body.y_rot != before.body.y_rot || body.x_rot != before.body.x_rot {
                entity.set_rot(Rotation {
                    y_rot: body.y_rot,
                    x_rot: body.x_rot,
                    y_head_rot: body.y_rot,
                });
            }
            entity.set_flags(EntityFlags::ON_GROUND, body.on_ground);
            entity.set_flags(EntityFlags::SPRINTING, state.sprinting);
            player.survival.flying = state.flying;
            if state.shift_key_down != before.shift_key_down || state.flying != before.flying {
                entity.set_flags(EntityFlags::CROUCHING, state.shift_key_down);
                let pose = if state.shift_key_down && !state.flying {
                    Pose::Crouching
                } else {
                    Pose::Standing
                };
                entity.set_pose(pose);
                entity.set_dims(vanillite_game::player::scaled_dimensions(
                    pose,
                    vanillite_game::effect::attribute_map::scale_of(
                        &player.survival.effects.modifiers,
                    ),
                ));
            }
        }
        self.level.entities.players = players;
        if let Some(reason) = kicked {
            return Err(format!("a client player left: {reason}"));
        }
        // `WorldSurvival::moves`: fall damage and exhaustion of the accepted moves.
        self.with_env(|_, ents, env, _| {
            let Entities { store, players, .. } = ents;
            for &id in &ids {
                let Some(mut entity) = store.get_mut(id) else {
                    continue;
                };
                let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
                    continue;
                };
                let moves = std::mem::take(&mut data.connection.movement.moves);
                for mv in &moves {
                    survival_player::on_move(&mut entity, data, env, *mv);
                }
            }
        });
        self.post_player_game_events();
        // `WorldMobs::player_actions`.
        let mut pending = Vec::new();
        let mut inputs = Vec::new();
        let riding = !self.level.entities.riding.is_empty();
        for &id in &ids {
            let Some(p) = self.level.entities.player_mut(id) else {
                continue;
            };
            pending.extend(p.connection.entity_actions.drain(..).map(|a| (id, a)));
            let v = &mut p.connection.vehicle;
            if riding || v.moved.is_some() {
                inputs.push((id, *v));
                v.moved = None;
                v.jump = None;
            }
        }
        let network_ids = Arc::clone(&self.network_ids);
        self.with_env(|level, ents, env, cx| {
            for (player, action) in pending {
                vanillite_server::player_entity_action(
                    level,
                    ents,
                    env,
                    cx,
                    &network_ids,
                    player,
                    action,
                );
            }
            for (player, input) in inputs {
                vanillite_server::player_vehicle_input(level, ents, env, cx, player, input);
            }
        });
        self.apply_actions();
        vanillite_game::menu::container::open_requests(
            &mut self.level,
            std::mem::take(&mut self.mobs.menus),
        );
        self.finish_removals();
        self.spawn_drops();
        Ok(())
    }

    /// The game events of the players (steps, item use) at the level.
    fn post_player_game_events(&mut self) {
        let mut pending = Vec::new();
        for (id, p) in self.level.entities.players.iter_mut() {
            pending.extend(
                p.survival
                    .game_events
                    .drain(..)
                    .map(|(event, pos, affected)| (id, event, pos, affected)),
            );
        }
        for (id, event, pos, affected) in pending {
            let context = vanillite_game::level::game_event::GameEventContext {
                source: Some(id),
                affected,
                owner: None,
            };
            self.level.game_event(event, pos, context);
        }
    }

    /// The connection tick of the client players (`WorldPlayers::tick_connections`): the
    /// digging progress, item pickup, then the movement checks start from where the player is.
    fn tick_client_connections(&mut self) {
        if self.clients.is_empty() {
            return;
        }
        let Some(data) = vanillite_game::player::play::game_data() else {
            return;
        };
        let ids: Vec<EntityId> = self.clients.iter().map(|c| c.id).collect();
        let mut players = std::mem::take(&mut self.level.entities.players);
        let spectators = vanillite_base::hash::FxHashSet::default();
        let mut taken = Vec::new();
        let mut thrown = Vec::new();
        for &id in &ids {
            let Some(entity) = self.level.entities.store.get(id) else {
                continue;
            };
            let Some(player) = players.get_mut(entity.kind_row()) else {
                continue;
            };
            player.connection.movement.tick_client_load_timeout();
            let actor = vanillite_server::player_actor(
                &entity,
                player.game_mode,
                &player.survival.effects,
                &player.inventory,
                0,
                &self.level.chunks,
            );
            let (aabb, uuid) = (entity.aabb(), entity.uuid());
            {
                let mut effects = vanillite_server::LevelEffects::default();
                let mut world = vanillite_server::WorldLevel {
                    level: &mut self.level,
                    effects: &mut effects,
                    spectators: &spectators,
                    actor: Some((id, aabb)),
                    dimension: "minecraft:overworld",
                };
                let mut drops = Vec::new();
                let mut swings = Vec::new();
                let protected = |_: BlockPos| false;
                let hooks = vanillite_game::hooks::NoHooks;
                let mut play = vanillite_game::player::play::PlayContext {
                    level: &mut world,
                    inventory: &mut player.inventory,
                    xp: Some(&mut player.survival.xp),
                    random: Some(&mut player.survival.random),
                    food: Some(&mut player.survival.food),
                    recipe_book: Some(&mut player.recipe_book),
                    actor,
                    data: &data,
                    spawn_protected: &protected,
                    drops: &mut drops,
                    swings: &mut swings,
                    hooks: &hooks,
                    stats: Some(&mut player.stats),
                    advancements: Some(&mut player.advancements),
                };
                player.connection.play.tick(&mut play, &NoSink);
            }
            // `Player.aiStep` → `touch`: item entities in the box grown by (1, 0.5, 1).
            if player.game_mode != vanillite_config::GameMode::Spectator {
                let Entities { store, items, .. } = &mut self.level.entities;
                let reach = aabb.inflate(1.0, 0.5, 1.0);
                let mut near = Vec::new();
                store.for_each_non_player_in(&reach, |e| near.push(e));
                let infinite = player.game_mode == vanillite_config::GameMode::Creative;
                for item_id in near {
                    let Some(row) = store
                        .get(item_id)
                        .filter(|e| e.kind() == ids::ITEM)
                        .map(|e| e.kind_row())
                    else {
                        continue;
                    };
                    let Some(item) = items.get_mut(row) else {
                        continue;
                    };
                    let target_ok = item.target.is_none_or(|t| t == uuid);
                    if item.pickup_delay != 0 || !target_ok || item.stack.is_empty() {
                        continue;
                    }
                    let before = item.thrower.map(|t| (t, item.stack.clone()));
                    if player.inventory.add(None, &mut item.stack, infinite) {
                        taken.push(item_id);
                        if let Some((thrower, mut stack)) = before {
                            if !item.stack.is_empty() {
                                stack = item.stack.clone();
                            }
                            thrown.push((id, thrower, stack));
                        }
                    }
                }
            }
        }
        self.level.entities.players = players;
        let entities = &mut self.level.entities;
        for (picker, thrower, stack) in thrown {
            mob::animal::item_picked_up(entities, picker, thrower, &stack);
        }
        for item_id in taken {
            if entities.item(item_id).is_none_or(|i| i.stack.is_empty()) {
                entities.despawn(item_id);
            }
        }
        // `resetPosition` for the moves of the next tick.
        let Entities { store, players, .. } = &mut self.level.entities;
        for &id in &ids {
            let Some(entity) = store.get(id) else {
                continue;
            };
            let Some(player) = players.get_mut(entity.kind_row()) else {
                continue;
            };
            let rot = entity.rot();
            let body = vanillite_game::player::movement::Body {
                pos: entity.pos(),
                velocity: entity.velocity(),
                y_rot: rot.y_rot,
                x_rot: rot.x_rot,
                on_ground: entity
                    .flags()
                    .contains(vanillite_world::entity::EntityFlags::ON_GROUND),
                horizontal_collision: false,
                fall_flying: false,
            };
            player.connection.movement.tick(&body);
        }
    }
}

/// `ExperienceOrb.playerTouch` of the orb `Util.getRandom` picks among those `reach` touches
/// (as the server's player tick does it).
fn touch_orb(
    ents: &mut Entities,
    player: &mut vanillite_game::player::PlayerData,
    reach: &vanillite_base::Aabb,
) {
    use vanillite_base::random::RandomSource;
    use vanillite_game::experience_orb as orbs;
    let mut near = Vec::new();
    orbs::touching(ents, reach, &mut near);
    if near.is_empty() {
        return;
    }
    let n = i32::try_from(near.len()).unwrap_or(i32::MAX);
    let pick = player.survival.random.next_int_bounded(n);
    let Some(&orb) = usize::try_from(pick).ok().and_then(|i| near.get(i)) else {
        return;
    };
    if player.survival.take_xp_delay != 0 {
        return;
    }
    player.survival.take_xp_delay = orbs::TAKE_XP_DELAY;
    let Some(data) = vanillite_game::player::play::game_data() else {
        return;
    };
    let random = &mut player.survival.random;
    let points = orbs::take_one(
        ents,
        orb,
        &mut player.inventory,
        &data.enchanting,
        &mut |n| random.next_int_bounded(n),
    );
    let Some(points) = points else {
        return;
    };
    if points > 0 {
        let _ = player.survival.give_experience_points(points);
    }
    // The orb goes when its last merged orb was taken.
    if ents.experience_orb(orb).is_some_and(|d| d.count <= 0) {
        ents.despawn(orb);
    }
}
