//! The scenario world across dimensions: the real server with its three worlds ticked one after
//! another in Vanilla's order (`performance.tick.parallel-dimensions = false`, the `vanilla`
//! profile).
//!
//! The server runs without network, its compute jobs inline, its chunks in memory and its
//! chunk tickets blocking ([`vanillite_server::ServerBuilder::harness`]); every dimension is
//! flat like the recorder's (`ParityServer` with `flatOthers`). A step is one iteration of the
//! server's tick loop without its clock; players join through the server's join code on a
//! connection nobody reads, and a `use_item` action is their `use_item` packet. Everything
//! between the worlds (portals, ender pearls, `execute in`, `/tp`) is the server's own code.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use vanillite_base::random::LegacyRandom;
use vanillite_base::{BlockPos, Vec3};
use vanillite_chunk::slots::{
    ChunkGenerator, ChunkGeneratorProvider, ChunkStore, ChunkStoreProvider, FlatGenerator,
    MemoryStore, WorldInfo,
};
use vanillite_data::block::{BlockId, BlockState};
use vanillite_data::entity::ids;
use vanillite_game::level::{Level, update};
use vanillite_game::mob::Environment;
use vanillite_game::survival::rules::Difficulty;
use vanillite_server::{Server, ServerBuilder, WorldId};
use vanillite_world::chunks::Visibility;
use vanillite_world::entity::EntityId;

use crate::game_world::{
    FLAT_SEA_LEVEL, PARITY_SHUFFLE_SEED, entity_detail, inside, install, observe_ticks, spawn_mob,
};
use crate::scenario::{EntitySpec, OVERWORLD, WorldSpec};
use crate::trace::{EntityObs, TickObs};
use crate::world::{Execution, ScenarioWorld};

const NETHER: &str = "minecraft:the_nether";
const END: &str = "minecraft:the_end";
/// Server ticks the chunks of the regions get to become ticking.
const SETTLE_LIMIT: usize = 200;
/// The chunk rate the scenario clients ask for (`ScenarioEntities.CHUNKS_PER_TICK`).
const CHUNKS_PER_TICK: f32 = 64.0;
/// View and simulation distance (`ScenarioRecorder.VIEW_DISTANCE`).
const VIEW_DISTANCE: u32 = 2;

/// The packets a scenario player's client receives (id + body each), until the frame takes
/// them.
#[derive(Default)]
struct Capture(std::sync::Mutex<Vec<Vec<u8>>>);

impl Capture {
    fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn push(&self, body: Vec<u8>) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(body);
        true
    }
}

impl vanillite_game::player::sink::PacketSink for Capture {
    fn send_body(&self, body: Vec<u8>) -> bool {
        self.push(body)
    }
    fn send_shared(&self, packet: &Arc<vanillite_protocol::SharedPacket>) -> bool {
        self.push(packet.body().to_vec())
    }
    fn close(&self) {}
    fn is_closed(&self) -> bool {
        false
    }
    fn latency_ms(&self) -> u32 {
        0
    }
}

/// Dimension keys with their flat layers (block, height), bottom first.
type PackDimensions = Vec<(String, Vec<(String, u32)>)>;

/// The flat layers of each dimension: the scenario's overworld, the recorder's Nether and End
/// (`ParityServer.NETHER_LAYERS`, `END_LAYERS`), the data pack's own.
struct ScenarioGenerators {
    overworld: Vec<(String, u32)>,
    /// The flat layers of the scenario data pack's dimensions, by dimension key.
    pack_dimensions: PackDimensions,
    /// The server's own generators, for the End of `"end": "normal"`.
    normal_end: Option<Box<dyn ChunkGeneratorProvider>>,
}

impl std::fmt::Debug for ScenarioGenerators {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScenarioGenerators")
            .field("overworld", &self.overworld)
            .field("pack_dimensions", &self.pack_dimensions)
            .finish_non_exhaustive()
    }
}

impl ChunkGeneratorProvider for ScenarioGenerators {
    fn create(&self, world: &WorldInfo) -> Result<Arc<dyn ChunkGenerator>, String> {
        if world.dimension == END
            && let Some(normal) = &self.normal_end
        {
            return normal.create(world);
        }
        let nether = [("minecraft:bedrock", 1), ("minecraft:netherrack", 3)];
        let end = [("minecraft:bedrock", 1), ("minecraft:end_stone", 3)];
        let layers: Vec<(&str, u32)> = match world.dimension.as_str() {
            NETHER => nether.to_vec(),
            END => end.to_vec(),
            other if self.pack_dimensions.iter().any(|(d, _)| d == other) => self
                .pack_dimensions
                .iter()
                .filter(|(d, _)| d == other)
                .flat_map(|(_, layers)| layers.iter().map(|(b, h)| (b.as_str(), *h)))
                .collect(),
            _ => self
                .overworld
                .iter()
                .map(|(b, h)| (b.as_str(), *h))
                .collect(),
        };
        let mut states = Vec::new();
        for (block, height) in layers {
            let state = BlockId::by_name(block)
                .ok_or_else(|| format!("unknown layer block `{block}`"))?
                .default_state();
            states.extend(std::iter::repeat_n(state.0, height as usize));
        }
        Ok(Arc::new(FlatGenerator::new(world, &states)))
    }
}

/// Copies the scenario data pack `name` into the world's `datapacks/` folder; returns the flat
/// layers of its dimensions.
fn install_datapack(world_dir: &Path, name: &str) -> Result<PackDimensions, String> {
    fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }
    let source = crate::golden::crate_dir().join("datapacks").join(name);
    copy(&source, &world_dir.join("datapacks").join(name))
        .map_err(|e| format!("data pack {}: {e}", source.display()))?;
    let mut dimensions = Vec::new();
    let data = source.join("data");
    for namespace in std::fs::read_dir(&data).map_err(|e| e.to_string())? {
        let namespace = namespace.map_err(|e| e.to_string())?;
        let Ok(files) = std::fs::read_dir(namespace.path().join("dimension")) else {
            continue;
        };
        for file in files {
            let path = file.map_err(|e| e.to_string())?.path();
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            let layers = json
                .pointer("/generator/settings/layers")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| format!("{}: only flat dimensions are supported", path.display()))?
                .iter()
                .map(|l| {
                    let block = l.get("block").and_then(serde_json::Value::as_str);
                    let height = l.get("height").and_then(serde_json::Value::as_u64);
                    match (block, height.and_then(|h| u32::try_from(h).ok())) {
                        (Some(b), Some(h)) => Ok((b.to_owned(), h)),
                        _ => Err(format!("{}: invalid layer {l}", path.display())),
                    }
                })
                .collect::<Result<Vec<_>, String>>()?;
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or("invalid dimension file name")?;
            dimensions.push((
                format!("{}:{stem}", namespace.file_name().to_string_lossy()),
                layers,
            ));
        }
    }
    Ok(dimensions)
}

/// Chunks in memory.
#[derive(Debug)]
struct MemoryStores;

impl ChunkStoreProvider for MemoryStores {
    fn create(
        &self,
        _: &WorldInfo,
        _: vanillite_chunk::store::DimensionId,
    ) -> std::io::Result<Arc<dyn ChunkStore>> {
        Ok(Arc::new(MemoryStore::default()))
    }
}

/// The server of a scenario across dimensions.
pub struct ServerWorld {
    server: Server,
    spec: WorldSpec,
    dir: PathBuf,
    /// The world block ops and observations work in.
    selected: WorldId,
    /// Scenario names by UUID: an entity that changes worlds is a new entity there.
    names: Vec<(u128, String)>,
    /// The packet queues of the scenario players.
    players: Vec<(String, vanillite_protocol::InboundSender)>,
    /// The world of the player [`ServerWorld::player`] found last.
    selected_player: WorldId,
    /// What the scenario players' clients receive (`"client_view"`), by name.
    captures: Vec<(String, Arc<Capture>)>,
    clients: crate::client_view::ClientViews,
    /// The packet-level players (`"client": true`) by name, and whether their client
    /// reported the world loaded.
    client_players: Vec<(String, bool)>,
}

/// A sink for packets nobody reads.
struct Discard;

impl vanillite_game::player::sink::PacketSink for Discard {
    fn send_body(&self, _: Vec<u8>) -> bool {
        true
    }
    fn send_shared(&self, _: &Arc<vanillite_protocol::SharedPacket>) -> bool {
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

impl std::fmt::Debug for ServerWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerWorld").finish_non_exhaustive()
    }
}

fn world_id(server: &Server, dimension: &str) -> Result<WorldId, String> {
    server
        .global()
        .dimensions
        .world(dimension)
        .ok_or_else(|| format!("unknown dimension {dimension}"))
}

impl ServerWorld {
    /// The server of `spec`, with the chunks of the region and the views ticking.
    pub fn new(spec: &WorldSpec) -> Result<Self, String> {
        install();
        vanillite_game::mob::set_parity_shuffle_seed(Some(PARITY_SHUFFLE_SEED));
        if spec.observe_light {
            return Err("scenarios across dimensions observe no light".to_owned());
        }
        if spec.weather != "clear" {
            return Err("scenarios across dimensions have clear weather".to_owned());
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vanillite-dimensions-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let io = |e: std::io::Error| e.to_string();
        std::fs::write(dir.join("eula.txt"), "eula=true\n").map_err(io)?;
        std::fs::write(
            dir.join("server.properties"),
            format!(
                "level-seed={}\nview-distance={VIEW_DISTANCE}\nsimulation-distance={VIEW_DISTANCE}\n\
                 online-mode=false\nmax-tick-time=-1\npause-when-empty-seconds=0\n\
                 spawn-protection=0\ngamemode=survival\n",
                spec.seed
            ),
        )
        .map_err(io)?;
        let cache = std::env::var_os("VANILLITE_CACHE").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/vanilla-cache"),
            PathBuf::from,
        );
        let cache = std::fs::canonicalize(&cache).unwrap_or(cache);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&cache, dir.join("cache")).map_err(io)?;
        let pack_dimensions = match &spec.datapack {
            Some(name) => install_datapack(&dir.join("world"), name)?,
            None => Vec::new(),
        };
        let mut builder = ServerBuilder::new(&dir);
        builder
            .vanilla_pack(vanillite_server::VanillaPack::Download)
            .network(false)
            .harness();
        builder
            .scheduler(Box::new(vanillite_server::InlineScheduler))
            .and_then(|b| b.chunk_storage(Box::new(MemoryStores)))
            .map_err(|e| e.to_string())?;
        let overworld = spec.layers.clone();
        if spec.normal_end {
            // The built-in generators fill the slot; the scenario's serve all but the End.
            builder.wrap_chunk_generator(move |built_in| {
                Box::new(ScenarioGenerators {
                    overworld,
                    pack_dimensions,
                    normal_end: Some(built_in),
                })
            });
        } else {
            builder
                .chunk_generator(Box::new(ScenarioGenerators {
                    overworld,
                    pack_dimensions,
                    normal_end: None,
                }))
                .map_err(|e| e.to_string())?;
        }
        let server = builder.start().map_err(|e| e.to_string())?;
        let selected = world_id(&server, &spec.dimension)?;
        let mut world = Self {
            server,
            spec: spec.clone(),
            dir,
            selected,
            names: Vec::new(),
            players: Vec::new(),
            selected_player: WorldId(0),
            captures: Vec::new(),
            clients: crate::client_view::ClientViews::with_attributes(spec.client_attributes),
            client_players: Vec::new(),
        };
        // The recorder's rules (`ScenarioRecorder.rules`).
        let mut rules: Vec<(String, String)> = [
            ("random_tick_speed", "0"),
            ("spawn_mobs", "false"),
            ("advance_time", "false"),
            ("advance_weather", "false"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        for (k, v) in &spec.rules {
            rules.retain(|(name, _)| name != k);
            rules.push((k.clone(), v.clone()));
        }
        for (k, v) in rules {
            world.server.command(&format!("gamerule {k} {v}"));
        }
        // The recorder's time of day and difficulty (`ScenarioRecorder.timeAndDifficulty`).
        if let Some(time) = spec.time {
            world.server.command(&format!("time set {time}"));
        }
        world
            .server
            .command(&format!("difficulty {}", spec.difficulty));
        // The regions' chunks, forced like `/forceload`, until they tick with their entities.
        let mut areas = vec![(world.selected, spec.forced.clone())];
        for view in &spec.views {
            areas.push((
                world_id(&world.server, &view.dimension)?,
                view.forced.clone(),
            ));
        }
        for (id, chunks) in &areas {
            let level = world.server.world(*id).level_mut();
            for &pos in chunks {
                let Level {
                    chunks: runtime,
                    forced,
                    ..
                } = level;
                forced.set(runtime, pos, true);
            }
        }
        let mut settled = false;
        for _ in 0..SETTLE_LIMIT {
            world.server.step();
            settled = areas.iter().all(|(id, chunks)| {
                let level = world.server.world_ref(*id).level();
                chunks.iter().all(|&pos| {
                    level.chunks.visibility(pos) == Visibility::Ticking
                        && level.is_block_ticking(pos)
                        && level.chunks.entities_loaded(pos)
                })
            });
            if settled {
                break;
            }
        }
        if !settled {
            let mut why = String::new();
            for (id, chunks) in &areas {
                let level = world.server.world_ref(*id).level();
                for &pos in chunks {
                    let state = (
                        level.chunks.visibility(pos),
                        level.is_block_ticking(pos),
                        level.chunks.entities_loaded(pos),
                    );
                    if state != (Visibility::Ticking, true, true) {
                        why = format!("world {} chunk {pos:?}: {state:?}", id.0);
                        break;
                    }
                }
            }
            return Err(format!("the scenario regions do not become ticking: {why}"));
        }
        // The scenario starts at game time 0 like the recorder's.
        world.server.set_game_time(0);
        world.seed_levels(false);
        Ok(world)
    }

    /// [`crate::world::WorldFactory`] of scenarios across dimensions.
    pub fn factory(spec: &WorldSpec, _execution: Execution) -> Box<dyn ScenarioWorld> {
        Box::new(Self::new(spec).unwrap_or_else(|e| panic!("{e}")))
    }

    /// The level random of every world (and with `rand_value` the LCG of random tick
    /// positions) starts from the world seed (`ScenarioRecorder.seedLevels`).
    fn seed_levels(&mut self, rand_value: bool) {
        let seed = self.spec.seed;
        let worlds: Vec<WorldId> = self.server.global().dimensions.worlds().collect();
        for id in worlds {
            let level = self.server.world(id).level_mut();
            *level.random() = LegacyRandom::new(seed);
            if rand_value {
                level.set_rand_value(seed as i32);
            }
        }
    }

    fn level(&self) -> &Level {
        self.server.world_ref(self.selected).level()
    }

    fn level_mut(&mut self) -> &mut Level {
        self.server.world(self.selected).level_mut()
    }

    fn name_of(&self, uuid: u128) -> Option<String> {
        self.names
            .iter()
            .find(|(u, _)| *u == uuid)
            .map(|(_, n)| n.clone())
    }

    /// The overworld entity of a scenario player.
    fn player(&mut self, name: &str) -> Result<EntityId, String> {
        let uuid = self
            .names
            .iter()
            .find(|(_, n)| n == name)
            .map(|(u, _)| *u)
            .ok_or_else(|| format!("no scenario entity `{name}`"))?;
        let worlds: Vec<WorldId> = self.server.global().dimensions.worlds().collect();
        for id in worlds {
            let index = id.0;
            if let Some(id) = self
                .server
                .world_ref(WorldId(index))
                .level()
                .entities
                .player_by_uuid(uuid)
            {
                self.selected_player = WorldId(index);
                return Ok(id);
            }
        }
        Err(format!("the player `{name}` is in no world"))
    }

    fn add_player(&mut self, spec: &EntitySpec, index: usize) -> Result<(), String> {
        let uuid = spec.uuid(index);
        let tx = if self.spec.client_view {
            let capture = Arc::new(Capture::default());
            self.captures
                .push((spec.name.clone(), Arc::clone(&capture)));
            self.server.join_into_sink(&spec.name, uuid, capture)
        } else {
            self.server.join_without_network(&spec.name, uuid)
        };
        self.players.push((spec.name.clone(), tx));
        if spec.client {
            self.client_players.push((spec.name.clone(), false));
        }
        self.names.push((uuid, spec.name.clone()));
        let overworld = self.server.world(WorldId(0)).level_mut();
        let id = overworld
            .entities
            .player_by_uuid(uuid)
            .ok_or("the scenario player did not join")?;
        let pos = Vec3::new(spec.pos[0], spec.pos[1], spec.pos[2]);
        // `snapTo` with head and body facing the yaw; the server moves the player (it has no
        // client).
        vanillite_game::player::movement::teleport_within_world(
            &mut overworld.entities,
            id,
            pos,
            spec.rot[0],
            spec.rot[1],
            0,
        );
        if let Some(mut e) = overworld.entities.store.get_mut(id) {
            e.set_rot(vanillite_world::entity::Rotation {
                y_rot: spec.rot[0],
                x_rot: spec.rot[1],
                y_head_rot: spec.rot[0],
            });
            e.set_old_pos();
            // A new player is not on the ground before its first move.
            e.set_flags(vanillite_world::entity::EntityFlags::ON_GROUND, false);
        }
        // The join of a client: the server teleports the player where it stands
        // (`placeNewPlayer`); the client accepts at the start of its first tick.
        let body = spec.client.then(|| {
            overworld
                .entities
                .store
                .get(id)
                .map(|e| vanillite_game::player::movement::Body {
                    pos: e.pos(),
                    velocity: e.velocity(),
                    y_rot: e.rot().y_rot,
                    x_rot: e.rot().x_rot,
                    on_ground: false,
                    horizontal_collision: false,
                    fall_flying: false,
                })
        });
        if let Some(p) = overworld.entities.player_mut(id) {
            // A client player moves by its packets.
            p.survival.simulate_movement = !spec.client;
            for (slot, item, count) in &spec.inventory {
                let item = vanillite_data::item::ItemId::by_name(item)
                    .ok_or_else(|| format!("unknown item {item}"))?;
                if let Some(s) = p.inventory.get_mut(*slot) {
                    *s = vanillite_game::item::ItemStack::new(item.0, *count);
                }
            }
            if let Some(food) = spec.food {
                p.survival.food.food_level = food;
            }
            if let Some(Some(mut body)) = body {
                let (pos, y_rot, x_rot) = (body.pos, body.y_rot, body.x_rot);
                p.connection
                    .movement
                    .teleport(&mut body, pos, y_rot, x_rot, &Discard);
            }
            p.survival.y_body_rot = spec.rot[0];
            // The recorder seeds the player's own random with its `seed` at the end of the
            // `Entity` constructor; the `LivingEntity` constructor then draws the yaw.
            p.survival.random = vanillite_base::random::LegacyRandom::new(spec.seed);
            vanillite_base::random::RandomSource::next_float(&mut p.survival.random);
        }
        Ok(())
    }
}

impl ServerWorld {
    /// The network id of the scenario entity `name` in any world.
    fn network_id(&self, name: &str) -> Result<i32, String> {
        let uuid = self
            .names
            .iter()
            .find(|(_, n)| n == name)
            .map(|(u, _)| *u)
            .ok_or_else(|| format!("no scenario entity `{name}`"))?;
        self.network_id_of(uuid)
            .ok_or_else(|| format!("the entity `{name}` is in no world"))
    }

    /// The network id of the entity `uuid` in any world.
    fn network_id_of(&self, uuid: u128) -> Option<i32> {
        self.server.global().dimensions.worlds().find_map(|id| {
            let columns = self.server.world_ref(id).level().entities.store.columns();
            let row = columns.uuid.iter().position(|&u| u == uuid)?;
            columns.network_id.get(row).map(|n| n.0)
        })
    }

    /// The network id and UUID of a packet target: a scenario entity, `uuid:<hex>` or `none`
    /// (a target that does not exist is the id -1).
    fn packet_target(&self, name: &str) -> Result<(i32, u128), String> {
        if name == "none" {
            return Ok((-1, 0));
        }
        if let Some(hex) = name.strip_prefix("uuid:") {
            let uuid = u128::from_str_radix(hex, 16).map_err(|_| format!("bad target `{name}`"))?;
            return Ok((self.network_id_of(uuid).unwrap_or(-1), uuid));
        }
        let uuid = self
            .names
            .iter()
            .find(|(_, n)| n == name)
            .map(|(u, _)| *u)
            .ok_or_else(|| format!("no scenario entity `{name}`"))?;
        Ok((self.network_id(name)?, uuid))
    }

    /// Queues a packet the client of the scenario player `name` sends by itself (it does not
    /// mark the player loaded); the server handles it in its next tick.
    fn push(
        &self,
        name: &str,
        packet: vanillite_protocol::sets::ServerboundPlay,
    ) -> Result<(), String> {
        let tx = self
            .players
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, tx)| tx)
            .ok_or("no packet queue")?;
        if tx.try_send(vanillite_protocol::Inbound { seq: 0, packet }) {
            Ok(())
        } else {
            Err(format!("the packet of `{name}` was dropped"))
        }
    }

    /// Queues a packet of the scenario player `name`; the server handles it in its next tick.
    /// The player counts as loaded from then on: the recorder calls the game code of an
    /// action directly, without `hasClientLoaded`.
    fn send(
        &mut self,
        name: &str,
        packet: vanillite_protocol::sets::ServerboundPlay,
    ) -> Result<(), String> {
        let id = self.player(name)?;
        if let Some(p) = self
            .server
            .world(self.selected_player)
            .level_mut()
            .entities
            .player_mut(id)
        {
            p.connection.movement.mark_client_loaded();
        }
        let tx = self
            .players
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, tx)| tx)
            .ok_or("no packet queue")?;
        if tx.try_send(vanillite_protocol::Inbound { seq: 0, packet }) {
            Ok(())
        } else {
            Err(format!("the packet of `{name}` was dropped"))
        }
    }
}

impl Drop for ServerWorld {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl ScenarioWorld for ServerWorld {
    fn select_dimension(&mut self, dimension: &str) -> Result<(), String> {
        self.selected = world_id(&self.server, dimension)?;
        Ok(())
    }

    fn is_loaded(&self, pos: BlockPos) -> bool {
        let level = self.level();
        level.is_loaded(pos) && level.chunks.chunk_now(pos.chunk())
    }

    fn set_block_raw(&mut self, pos: BlockPos, state: BlockState) {
        self.level_mut()
            .set_block(pos, state, update::CLIENTS | update::KNOWN_SHAPE);
    }

    fn place_block(&mut self, pos: BlockPos, state: BlockState) {
        self.level_mut().set_block(pos, state, update::ALL);
    }

    fn block(&self, pos: BlockPos) -> BlockState {
        self.level().block(pos)
    }

    fn step(&mut self) {
        self.server.step();
    }

    fn block_entities(&self, min: BlockPos, max: BlockPos, out: &mut Vec<String>) {
        crate::game_world::observe_block_entities(
            self.level(),
            |p| self.is_loaded(p),
            min,
            max,
            out,
        );
    }

    fn block_ticks(&self, min: BlockPos, max: BlockPos, out: &mut Vec<TickObs>) {
        let level = self.level();
        observe_ticks(level, level.chunks.block_ticks(), min, max, out, |kind| {
            BlockId(kind).name().to_owned()
        });
    }

    fn fluid_ticks(&self, min: BlockPos, max: BlockPos, out: &mut Vec<TickObs>) {
        let level = self.level();
        observe_ticks(level, level.chunks.fluid_ticks(), min, max, out, |kind| {
            vanillite_data::registry::FLUID
                .name(kind)
                .unwrap_or("?")
                .to_owned()
        });
    }

    /// Entities in the region of the selected world; outside the overworld with their `dim`.
    fn entities(&self, min: BlockPos, max: BlockPos, out: &mut Vec<EntityObs>) {
        let level = self.level();
        let dimension = self
            .server
            .global()
            .dimensions
            .name_or_overworld(self.selected);
        let ents = &level.entities;
        let core = ents.store.columns();
        let name_of = |id: EntityId| ents.store.get(id).and_then(|e| self.name_of(e.uuid()));
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
            // `tickDeath` removed a dead player from the level (its connection still ticks it).
            if ents
                .player(core.id[row])
                .is_some_and(|p| p.survival.removed)
            {
                continue;
            }
            // A player moves by its client: its server-side motion is not part of the trace.
            let v = if core.kind[row] == ids::PLAYER {
                Vec3::ZERO
            } else {
                core.velocity[row]
            };
            let is_client = |id: EntityId| {
                core.kind[row] == ids::PLAYER
                    && name_of(id).is_some_and(|n| self.client_players.iter().any(|(c, _)| *c == n))
            };
            let mut detail = entity_detail(
                ents,
                row,
                &name_of,
                &is_client,
                self.spec.player_state,
                self.spec.sleep_state,
            );
            if dimension != OVERWORLD {
                detail
                    .get_or_insert_with(serde_json::Map::new)
                    .insert("dim".into(), json!(dimension));
            }
            if self.spec.progress_state
                && let Some(p) = ents.player(core.id[row])
            {
                let d = detail.get_or_insert_with(serde_json::Map::new);
                progress_detail(p, d);
            }
            out.push(EntityObs {
                kind: core.kind[row].name().to_owned(),
                pos: [pos.x, pos.y, pos.z],
                motion: [v.x, v.y, v.z],
                detail,
            });
        }
    }

    /// Players (through the join), every level random reseeded with the world seed, then the
    /// other entities in the overworld.
    fn add_entities(&mut self, entities: &[EntitySpec], entity_seed: i64) -> Result<(), String> {
        for (i, spec) in entities.iter().enumerate() {
            if spec.is_player() {
                self.add_player(spec, i)?;
            }
        }
        self.seed_levels(true);
        let env = Environment {
            sea_level: FLAT_SEA_LEVEL,
            ..Environment::overworld(self.spec.time.unwrap_or(0), 0.0, 0.0)
        };
        let difficulty = match self.spec.difficulty.as_str() {
            "peaceful" => Difficulty::Peaceful,
            "easy" => Difficulty::Easy,
            "hard" => Difficulty::Hard,
            _ => Difficulty::Normal,
        };
        for (i, spec) in entities.iter().enumerate() {
            if spec.is_player() {
                continue;
            }
            let level = self.server.world(WorldId(0)).level_mut();
            let network_ids = Arc::clone(level.network_ids_arc());
            let id = spawn_mob(level, &network_ids, env, difficulty, spec, i)?;
            let uuid = level.entities.store.get(id).map(|e| e.uuid());
            if let Some(uuid) = uuid {
                self.names.push((uuid, spec.name.clone()));
            }
        }
        self.level_mut().set_entity_seed(entity_seed);
        Ok(())
    }

    fn level_rng(&mut self) -> Option<i64> {
        (self.spec.level_rng || !self.names.is_empty()).then(|| self.level_mut().random().state())
    }

    /// Every client player accepts the teleport the server waits for, and reports the world
    /// loaded in its first tick (`ScenarioEntities.clientBegin`).
    fn client_begin(&mut self) -> Result<(), String> {
        use vanillite_protocol::packets::play::serverbound::{AcceptTeleportation, PlayerLoaded};
        use vanillite_protocol::sets::ServerboundPlay;
        let names: Vec<String> = self.client_players.iter().map(|(n, _)| n.clone()).collect();
        for name in &names {
            let id = self.player(name)?;
            let level = self.server.world_ref(self.selected_player).level();
            let pending = level
                .entities
                .player(id)
                .and_then(|p| p.connection.movement.pending_teleport());
            let rot = level.entities.store.get(id).map(|e| e.rot());
            if let (Some((teleport, to)), Some(rot)) = (pending, rot) {
                self.push(
                    name,
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
        }
        for i in 0..self.client_players.len() {
            if !std::mem::replace(&mut self.client_players[i].1, true) {
                let name = self.client_players[i].0.clone();
                self.push(&name, ServerboundPlay::PlayerLoaded(PlayerLoaded))?;
            }
        }
        Ok(())
    }

    /// `client_tick_end` of every client player.
    fn client_end(&mut self) -> Result<(), String> {
        use vanillite_protocol::packets::play::serverbound::ClientTickEnd;
        use vanillite_protocol::sets::ServerboundPlay;
        if self.spec.client_tick_end {
            for (name, _) in &self.client_players {
                self.push(name, ServerboundPlay::ClientTickEnd(ClientTickEnd))?;
            }
        }
        Ok(())
    }

    /// The client of every player also acknowledges each chunk batch it got
    /// (`chunk_batch_received`, like `ScenarioEntities.packets`), so chunks keep coming.
    fn client_view(&mut self) -> Result<Vec<String>, String> {
        use vanillite_protocol::Packet;
        use vanillite_protocol::packets::play::clientbound::ChunkBatchFinished;
        use vanillite_protocol::packets::play::serverbound::ChunkBatchReceived;
        use vanillite_protocol::sets::ServerboundPlay;
        let packets: Vec<(String, Vec<Vec<u8>>)> = self
            .captures
            .iter()
            .map(|(name, capture)| (name.clone(), capture.take()))
            .collect();
        for (name, bodies) in &packets {
            let finished = bodies.iter().any(|b| {
                vanillite_protocol::codec::Reader::new(b)
                    .varint()
                    .is_ok_and(|id| id == ChunkBatchFinished::ID)
            });
            if finished {
                self.push(
                    name,
                    ServerboundPlay::ChunkBatchReceived(ChunkBatchReceived {
                        desired_chunks_per_tick: CHUNKS_PER_TICK,
                    }),
                )?;
            }
        }
        Ok(self.clients.frame(
            packets
                .iter()
                .map(|(n, p)| (n.as_str(), p.iter().map(Vec::as_slice).collect())),
        ))
    }

    /// As the server at the origin of the overworld, before the tick.
    fn command(&mut self, line: &str) -> Result<(), String> {
        self.server.command_at(line, vanillite_base::Vec3::ZERO);
        Ok(())
    }

    /// The packet a client sends for the action: `use_item` (or `interact` on `target`,
    /// `attack`, the `RELEASE_USE_ITEM` and `STAB` player actions) after the stack, the
    /// inventory, the food and the fall distance are set.
    fn use_item(&mut self, op: &crate::scenario::UseItemOp) -> Result<(), String> {
        use crate::scenario::UseMode;
        use vanillite_protocol::packets::play::serverbound::{
            DigAction, Hand, PlayerActionPacket, UseItem,
        };
        use vanillite_protocol::sets::ServerboundPlay;
        let id = self.player(&op.entity)?;
        let stack = op
            .item
            .as_deref()
            .map(|item| scenario_stack(item, op.count, op.components.as_deref()))
            .transpose()?;
        let slots = op
            .inventory
            .iter()
            .map(|s| {
                Ok((
                    s.slot,
                    scenario_stack(&s.item, s.count, s.components.as_deref())?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let target = match (&op.target, &op.mode) {
            (Some(t), _) | (None, UseMode::Attack(t)) => Some(self.network_id(t)?),
            _ => None,
        };
        let level = self.server.world(self.selected_player).level_mut();
        let rot = level
            .entities
            .store
            .get(id)
            .map(|e| e.rot())
            .ok_or("the player is gone")?;
        let p = level.entities.player_mut(id).ok_or("the player is gone")?;
        if let Some(stack) = stack {
            let slot = p.inventory.selected;
            p.inventory.set(slot, stack);
        }
        for (slot, stack) in slots {
            p.inventory.set(slot, stack);
        }
        if let Some(food) = op.food {
            p.survival.food.food_level = food;
        }
        if let Some(fall) = op.fall_distance {
            p.survival.fall_distance = fall;
        }
        let action = |action| {
            ServerboundPlay::PlayerAction(PlayerActionPacket {
                action,
                pos: vanillite_protocol::types::BlockPos { x: 0, y: 0, z: 0 },
                direction: 0,
                sequence: 0,
            })
        };
        let packet =
            match (&op.mode, target) {
                (UseMode::Release, _) => action(DigAction::ReleaseUseItem),
                (UseMode::Stab, _) => action(DigAction::Stab),
                (UseMode::Attack(_), Some(entity_id)) => ServerboundPlay::Attack(
                    vanillite_protocol::packets::play::serverbound::Attack { entity_id },
                ),
                (UseMode::Use, Some(entity_id)) => ServerboundPlay::Interact(
                    vanillite_protocol::packets::play::serverbound::Interact {
                        entity_id,
                        hand: Hand::MainHand,
                        location: Default::default(),
                        using_secondary_action: false,
                    },
                ),
                _ => ServerboundPlay::UseItem(UseItem {
                    hand: Hand::MainHand,
                    sequence: 0,
                    y_rot: rot.y_rot,
                    x_rot: rot.x_rot,
                }),
            };
        self.send(&op.entity, packet)
    }

    /// The `use_item_on` packet of a block click (`attack`: `START_DESTROY_BLOCK`).
    fn player_use(&mut self, click: &crate::scenario::PlayerUse) -> Result<(), String> {
        use vanillite_protocol::packets::play::serverbound::{
            DigAction, Hand, PlayerActionPacket, UseItemOn,
        };
        use vanillite_protocol::sets::ServerboundPlay;
        let id = self.player(&click.entity)?;
        let face: u8 = match click.face.as_str() {
            "down" => 0,
            "up" => 1,
            "north" => 2,
            "south" => 3,
            "west" => 4,
            "east" => 5,
            other => return Err(format!("unknown face {other}")),
        };
        let stack = click
            .item
            .as_deref()
            .map(|item| scenario_stack(item, click.count, click.components.as_deref()))
            .transpose()?;
        let level = self.server.world(self.selected_player).level_mut();
        let p = level.entities.player_mut(id).ok_or("the player is gone")?;
        if let Some(food) = click.food {
            p.survival.food.food_level = food;
        }
        if let Some(stack) = stack {
            if click.offhand {
                p.inventory.offhand = stack;
            } else {
                let slot = p.inventory.selected;
                p.inventory.set(slot, stack);
            }
        }
        if click.sneak {
            return Err("sneaking clicks are not sent by the server harness".to_owned());
        }
        let packet = if click.attack {
            ServerboundPlay::PlayerAction(PlayerActionPacket {
                action: DigAction::StartDestroyBlock,
                pos: vanillite_protocol::types::BlockPos {
                    x: click.pos.x,
                    y: click.pos.y,
                    z: click.pos.z,
                },
                direction: face,
                sequence: 0,
            })
        } else {
            // The face's centre unless `hit` says otherwise.
            let [x, y, z] = click.hit.unwrap_or(match face {
                0 => [0.5, 0.0, 0.5],
                1 => [0.5, 1.0, 0.5],
                2 => [0.5, 0.5, 0.0],
                3 => [0.5, 0.5, 1.0],
                4 => [0.0, 0.5, 0.5],
                _ => [1.0, 0.5, 0.5],
            });
            #[allow(clippy::cast_possible_truncation)]
            ServerboundPlay::UseItemOn(UseItemOn {
                hand: if click.offhand {
                    Hand::OffHand
                } else {
                    Hand::MainHand
                },
                pos: vanillite_protocol::types::BlockPos {
                    x: click.pos.x,
                    y: click.pos.y,
                    z: click.pos.z,
                },
                face: i32::from(face),
                cursor_x: x as f32,
                cursor_y: y as f32,
                cursor_z: z as f32,
                inside: false,
                world_border_hit: false,
                sequence: 0,
            })
        };
        self.send(&click.entity, packet)
    }

    /// A raw serverbound play packet of a scenario player.
    fn packet(&mut self, op: &crate::scenario::PacketOp) -> Result<(), String> {
        let target = op
            .target
            .as_deref()
            .map(|t| self.packet_target(t))
            .transpose()?;
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
        self.send(&op.entity, packet)
    }
}

/// Time statistics and distances `progress_state` leaves out.
fn traced_stat(name: &str) -> bool {
    const TIMES: [&str; 5] = [
        "minecraft.custom:minecraft.play_time",
        "minecraft.custom:minecraft.total_world_time",
        "minecraft.custom:minecraft.time_since_death",
        "minecraft.custom:minecraft.time_since_rest",
        "minecraft.custom:minecraft.crouch_time",
    ];
    !TIMES.contains(&name) && !name.ends_with("_one_cm")
}

/// `adv` (the completed criteria as `advancement#criterion`, sorted) and `stats` (the non-zero
/// statistics by name) of a player (`"progress_state": true`).
fn progress_detail(
    p: &vanillite_game::player::PlayerData,
    d: &mut serde_json::Map<String, serde_json::Value>,
) {
    let progress = &p.advancements;
    let mut adv = Vec::new();
    for (index, a) in progress.advancements().iter() {
        for (c, criterion) in (0u16..).zip(&a.criteria) {
            if progress.criterion_done(index, c) {
                adv.push(format!("{}#{}", a.id, criterion.name));
            }
        }
    }
    adv.sort();
    let stats: std::collections::BTreeMap<String, i32> = p
        .stats
        .iter()
        .filter(|(_, v)| *v != 0)
        .map(|(s, v)| (s.name(), v))
        .filter(|(n, _)| traced_stat(n))
        .collect();
    d.insert("adv".into(), json!(adv));
    d.insert("stats".into(), json!(stats));
}

/// A stack like the recorder's `ScenarioEntities.stack`: item, count, SNBT components.
fn scenario_stack(
    item: &str,
    count: i32,
    components: Option<&str>,
) -> Result<vanillite_game::item::ItemStack, String> {
    if item == "minecraft:air" {
        return Ok(vanillite_game::item::ItemStack::EMPTY);
    }
    let mut c = vanillite_nbt::Compound::new();
    c.insert("id", vanillite_nbt::Tag::String(item.to_owned()));
    c.insert("count", vanillite_nbt::Tag::Int(count));
    if let Some(snbt) = components {
        let components = vanillite_nbt::snbt::parse_compound(snbt).map_err(|e| e.to_string())?;
        c.insert("components", vanillite_nbt::Tag::Compound(components));
    }
    Ok(vanillite_game::item_nbt::stack_from_nbt(&c))
}

/// The scenario world of a scenario: across dimensions on the server, else the headless
/// level.
pub fn factory(spec: &WorldSpec, execution: Execution) -> Box<dyn ScenarioWorld> {
    if spec.dimensions {
        ServerWorld::factory(spec, execution)
    } else {
        crate::game_world::GameWorld::factory(spec, execution)
    }
}
