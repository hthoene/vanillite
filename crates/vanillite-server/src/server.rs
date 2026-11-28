//! Startup, the tick loop (a port of Vanilla's `MinecraftServer.runServer`), world threads,
//! global phases, autosave, config reload and shutdown.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Write as _;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::{fmt, io};

use vanillite_chunk::slots::{
    ChunkGeneratorProvider, ChunkStoreProvider, DimensionId, LightEngineProvider,
};
use vanillite_config::{Config, ConfigError, RuleSet, VANILLITE_FILE, VanillaOr};
use vanillite_light::BfsLightEngineProvider;
use vanillite_storage::backup::BackupSettings;
use vanillite_storage::player_data::{JsonFile, PlayerDataStore};
use vanillite_storage::writer::FileWriter;
use vanillite_storage::{IoPool, RegionStoreProvider, StoreSettings, WorldFormat, WorldLayout};
use vanillite_world::chunks::SaveSettings;
use vanillite_world::entity::NetworkIds;
use vanillite_world::tracking::{ChunkTrackerProvider, EntityTrackerProvider};

use crate::budget::{Budget, Limits};
use crate::clock::Clock;
use crate::compute::{Compute, ComputePool, Scheduler};
use crate::global::Global;
use crate::net::{NetState, VanillaPack};
use crate::player_list::{PlayerEntry, PlayerList, Uuid};
use crate::slot::{Origin, Slot, SlotError};
use crate::tick_rate::{NANOS_PER_SECOND, TickRateManager};
use crate::watchdog::{Heartbeat, WatchState, Watchdog};
use crate::world::{ServerWorld, TickContext, WorldId, WorldTask};
use vanillite_net::VirtualOpener;

/// Vanilla's built-in dimensions, in tick order; every server has them, data packs add more
/// ([`crate::dimensions::Dimensions`]).
pub const VANILLA_DIMENSIONS: [&str; 3] = [
    "minecraft:overworld",
    "minecraft:the_nether",
    "minecraft:the_end",
];

/// How far behind the server must be before "Can't keep up!" (plus 20 ticks).
const OVERLOADED_THRESHOLD: u64 = NANOS_PER_SECOND;
/// Ticks between the hand-backs of freed memory to the system ([`Budget::release_memory`]).
const MEMORY_RELEASE_TICKS: u64 = 600;
/// Ticks after an empty server released its memory until what that freed (also in jobs that
/// were still running) is handed back to the system.
const EMPTY_RELEASE_SETTLE_TICKS: u32 = 100;
/// Minimum time between two "Can't keep up!" warnings (plus 100 ticks).
const OVERLOADED_WARNING_INTERVAL: u64 = 10 * NANOS_PER_SECOND;
/// Vanilla's autosave period in seconds of game time.
const AUTOSAVE_SECONDS: f32 = 300.0;

/// Work that runs on the server thread in the global pre-phase, with `&mut` to everything.
pub type GlobalTask = Box<dyn FnOnce(&mut Server) + Send + 'static>;

/// Work that runs once a save of every world is done.
pub type AfterSave = Box<dyn FnOnce() + Send + 'static>;

/// Counts the worlds of a save down; the last one runs the follow-up.
struct SaveDone {
    remaining: AtomicUsize,
    /// Waited for before the follow-up when the save flushes.
    writer: Option<FileWriter>,
    then: Mutex<Option<AfterSave>>,
}

impl SaveDone {
    fn world_saved(&self) {
        if self.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        if let Some(writer) = &self.writer {
            writer.flush();
        }
        let then = self
            .then
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(then) = then {
            then();
        }
    }
}

enum ServerMsg {
    Console(String),
    Global(GlobalTask),
    Stop,
    /// A task for a world owned by the server thread (sequential mode).
    WorldTask(WorldId, WorldTask),
    /// A world thread finished its tick (parallel mode).
    WorldTicked(WorldId, Vec<(WorldId, WorldTask)>),
    /// A world thread panicked.
    WorldCrashed(WorldId),
}

enum WorldMsg {
    Tick {
        tick: u64,
        runs_normally: bool,
        /// When the tick should end (clock nanoseconds).
        deadline: u64,
    },
    Task(WorldTask),
    /// Saves (chunks only with `save`) and ends the thread.
    Stop {
        save: bool,
    },
}

#[derive(Clone)]
enum Route {
    Server,
    World(Sender<WorldMsg>),
}

/// How tasks reach each world, by world index; `None` for unloaded worlds.
type Routes = Arc<std::sync::RwLock<Vec<Option<Route>>>>;

/// Who owns the worlds, by world index; `None` for unloaded extra worlds.
enum Worlds {
    /// The server thread ticks all worlds one after another (exact Vanilla order).
    Sequential(Vec<Option<ServerWorld>>),
    /// Each world has its own thread; the server thread coordinates.
    Parallel(Vec<Option<WorldThread>>),
}

/// Puts a built world into the slot of its index: on its own thread in parallel mode, with a
/// chunk waker through the server thread in sequential mode.
fn attach_world(
    worlds: &mut Worlds,
    routes: &Routes,
    mut world: ServerWorld,
    compute: &Compute,
    global: &Arc<Global>,
    sender: &Sender<ServerMsg>,
    watch: &WatchState,
) -> Result<(), StartError> {
    let id = world.id();
    let route = match worlds {
        Worlds::Parallel(threads) => {
            let heartbeat = Arc::new(Heartbeat::new(format!("World {}", world.dimension())));
            watch.register(Arc::clone(&heartbeat));
            let thread = spawn_world_thread(world, compute, global, sender.clone(), heartbeat)?;
            let route = Route::World(thread.sender.clone());
            put(threads, id, thread);
            route
        }
        Worlds::Sequential(worlds) => {
            let wake = sender.clone();
            world.set_chunk_waker(Arc::new(move || {
                let _ = wake.send(ServerMsg::WorldTask(id, Box::new(ServerWorld::pump_chunks)));
            }));
            put(worlds, id, world);
            Route::Server
        }
    };
    put(
        &mut routes.write().unwrap_or_else(PoisonError::into_inner),
        id,
        route,
    );
    Ok(())
}

/// Sets the slot of a world, growing the list.
fn put<T>(slots: &mut Vec<Option<T>>, id: WorldId, value: T) {
    if slots.len() <= id.index() {
        slots.resize_with(id.index() + 1, || None);
    }
    slots[id.index()] = Some(value);
}

struct WorldThread {
    sender: Sender<WorldMsg>,
    handle: JoinHandle<()>,
}

/// Configures and starts a server. Slots are filled here before startup; whatever is not
/// filled gets the built-in implementation.
#[derive(Debug)]
pub struct ServerBuilder {
    dir: PathBuf,
    scheduler: Slot<dyn Scheduler>,
    entity_tracker: Slot<dyn EntityTrackerProvider>,
    chunk_storage: Slot<dyn ChunkStoreProvider>,
    chunk_generator: Slot<dyn ChunkGeneratorProvider>,
    light_engine: Slot<dyn LightEngineProvider>,
    vanilla_pack: VanillaPack,
    network: bool,
    version: String,
    /// The parity harness drives the server (`test-support`).
    harness: bool,
    /// Wraps whatever fills the `chunk-generator` slot (`test-support`).
    wrap_generator: Option<GeneratorWrap>,
    memory_release: Option<fn()>,
}

/// A wrapper around the provider that fills the `chunk-generator` slot.
struct GeneratorWrap(Box<dyn FnOnce(GeneratorBox) -> GeneratorBox + Send>);

type GeneratorBox = Box<dyn ChunkGeneratorProvider>;

impl std::fmt::Debug for GeneratorWrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GeneratorWrap")
    }
}

impl ServerBuilder {
    /// A server running in `dir` (config files and worlds).
    pub fn new(dir: impl Into<PathBuf>) -> ServerBuilder {
        ServerBuilder {
            dir: dir.into(),
            scheduler: Slot::new("scheduler"),
            entity_tracker: Slot::new("entity-tracker"),
            chunk_storage: Slot::new("chunk-storage"),
            chunk_generator: Slot::new("chunk-generator"),
            light_engine: Slot::new("light-engine"),
            vanilla_pack: VanillaPack::default(),
            network: true,
            harness: false,
            wrap_generator: None,
            memory_release: None,
            version: format!(
                "Vanillite {} (Minecraft {})",
                env!("CARGO_PKG_VERSION"),
                vanillite_data::version::VERSION_ID
            ),
        }
    }

    /// The allocator's way to return freed memory to the system (`malloc_trim`, `mi_collect`):
    /// the server calls it after bursts of work ([`crate::budget::Budget::release_memory`]).
    pub fn memory_release(&mut self, release: fn()) -> &mut Self {
        self.memory_release = Some(release);
        self
    }

    /// The version shown in the startup log (default: `Vanillite <version> (Minecraft <id>)`);
    /// the server binary adds its commit.
    pub fn version(&mut self, version: impl Into<String>) -> &mut Self {
        self.version = version.into();
        self
    }

    /// Where the Vanilla data pack comes from (default: downloaded once into `cache/`).
    pub fn vanilla_pack(&mut self, pack: VanillaPack) -> &mut Self {
        self.vanilla_pack = pack;
        self
    }

    /// Whether the network listens (default `true`). Without it the server only takes
    /// virtual connections ([`Server::open_virtual`]); for tests.
    pub fn network(&mut self, on: bool) -> &mut Self {
        self.network = on;
        self
    }

    /// Fills the `scheduler` slot with an embedded implementation.
    pub fn scheduler(&mut self, scheduler: Box<dyn Scheduler>) -> Result<&mut Self, SlotError> {
        self.scheduler.fill(scheduler, Origin::Embedded)?;
        Ok(self)
    }

    /// Fills the `entity-tracker` slot with an embedded implementation.
    pub fn entity_tracker(
        &mut self,
        provider: Box<dyn EntityTrackerProvider>,
    ) -> Result<&mut Self, SlotError> {
        self.entity_tracker.fill(provider, Origin::Embedded)?;
        Ok(self)
    }

    /// Fills the `chunk-storage` slot with an embedded implementation.
    pub fn chunk_storage(
        &mut self,
        provider: Box<dyn ChunkStoreProvider>,
    ) -> Result<&mut Self, SlotError> {
        self.chunk_storage.fill(provider, Origin::Embedded)?;
        Ok(self)
    }

    /// Fills the `chunk-generator` slot with an embedded implementation.
    pub fn chunk_generator(
        &mut self,
        provider: Box<dyn ChunkGeneratorProvider>,
    ) -> Result<&mut Self, SlotError> {
        self.chunk_generator.fill(provider, Origin::Embedded)?;
        Ok(self)
    }

    /// Fills the `light-engine` slot with an embedded implementation.
    pub fn light_engine(
        &mut self,
        provider: Box<dyn LightEngineProvider>,
    ) -> Result<&mut Self, SlotError> {
        self.light_engine.fill(provider, Origin::Embedded)?;
        Ok(self)
    }

    /// Loads the configuration, starts the threads and creates the worlds.
    pub fn start(self) -> Result<Server, StartError> {
        let started = Instant::now();
        log::info!("Starting {}", self.version);
        let dir = self.dir;
        fs::create_dir_all(&dir).map_err(|e| StartError::io(&dir, e))?;
        log::info!("Loading properties");
        let config = Config::load(&dir)?;
        log::info!("Using profile {}", config.profile);
        if !crate::eula::check(&dir).map_err(|e| StartError::io(&dir, e))? {
            return Err(StartError::Eula(crate::eula::file(&dir)));
        }

        // Plugins load first: they register commands, events and slot providers.
        let plugins = crate::plugins::load(&dir, &config);
        let parallel = config.server.tick.parallel_dimensions;
        // One worker per core at a lower OS priority: the tick threads take a core whenever they
        // need one, generation gets every core they leave.
        let budget = Arc::new(Budget::new(
            Limits::detect(config.server.budget.cpu, config.server.budget.memory),
            config.server.threads.compute_nice,
        ));
        if let Some(release) = self.memory_release {
            budget.set_memory_release(release);
        }
        log::info!("Hardware budget: {}", budget.describe());
        let workers = match config.server.threads.compute {
            0 => budget.limits().default_workers(),
            n => usize::from(n),
        };
        let (scheduler, origin) = match self.scheduler.take() {
            Some(filled) => filled,
            None => {
                // Tick helpers for the parallel tick phases: one per core besides the tick
                // thread, at the tick threads' priority (the workers run below it).
                let phase_threads = match config.server.threads.tick_phases {
                    0 => budget.limits().default_workers(),
                    n => usize::from(n),
                };
                let pool = ComputePool::with_budget(
                    workers,
                    phase_threads.saturating_sub(1),
                    Arc::clone(&budget),
                )
                .map_err(|e| StartError::io(&dir, e))?;
                (Box::new(pool) as Box<dyn Scheduler>, Origin::BuiltIn)
            }
        };
        log::info!(
            "Slot scheduler: {origin} ({} compute workers)",
            scheduler.workers()
        );
        let compute = Compute::new(Arc::from(scheduler))
            .with_phase_threads(usize::from(config.server.threads.tick_phases));
        let (tracker_provider, origin) = self
            .entity_tracker
            .take()
            .unwrap_or_else(|| (Box::new(ChunkTrackerProvider), Origin::BuiltIn));
        log::info!("Slot entity-tracker: {origin}");

        let level_name = config.properties.level_name.clone();
        let level_dir = dir.join(&level_name);
        let session_lock = lock_world(&level_dir)?;
        let layout = crate::world_format::layout(&config);
        let writer = FileWriter::start().map_err(|e| StartError::io(&level_dir, e))?;
        let player_store = PlayerDataStore::new(&level_dir).with_writer(writer.clone());
        // Mojang's profile key signers are fetched while the data packs load.
        let services_keys = (config.properties.online_mode && self.network).then(|| {
            std::thread::spawn(|| {
                vanillite_net::services::fetch_profile_keys(Duration::from_secs(10))
            })
        });
        let prepared =
            crate::net::prepare(&config, &dir, &level_dir, &self.vanilla_pack, &player_store)?;
        // The dimensions come from the data packs and `world_gen_settings.dat`; the layout and
        // the format are settled for all of them before a chunk is read.
        let dimensions = prepared.world_gen.dimensions(prepared.packs.as_ref());
        let names: Vec<&str> = dimensions.names().collect();
        let moved = vanillite_storage::layout::adopt_layout(&dir, &level_name, layout, &names)
            .map_err(|e| StartError::Io(format!("world layout of \"{level_name}\": {e}")))?;
        for line in moved {
            log::warn!("storage.layout = {layout:?}: {line}");
        }
        let format_plan = crate::world_format::plan(&config, &dir, &level_name, &dimensions)?;
        let chat = chat_settings(&config, services_keys);
        let level_setup = crate::level::LevelSetup {
            level_name: level_name.clone(),
            game_type: crate::players::game_mode_id(config.properties.gamemode),
            hardcore: config.properties.hardcore,
            spawn: prepared
                .worlds
                .first()
                .map_or((vanillite_base::Vec3::ZERO, 0.0), |w| w.level.spawn),
        };
        let mut world_players = prepared.worlds.into_iter();
        let packs = prepared.packs.map(Arc::new);
        let crate::packs::Selected {
            packs: data_packs,
            config: pack_config,
            opened: opened_packs,
        } = prepared.selected;
        // Block behaviour reads its tags in every world, also without the noise generator.
        if let Some(packs) = packs.as_deref() {
            vanillite_worldgen::behaviour::init(packs);
        }
        let spawn_known = prepared.spawn_known;
        let seed = prepared.world_gen.seed;
        let world_gen = prepared.world_gen.clone();
        let mut behaviours = vanillite_game::block::Behaviours::vanilla();
        if config.server.redstone.implementation
            == vanillite_config::RedstoneImplementation::AlternateCurrent
        {
            vanillite_game::block::redstone::alternate_current::install(&mut behaviours);
        }
        let behaviours = Arc::new(behaviours);
        let spawn_table = crate::world_setup::spawn_table(packs.as_deref()).map(Arc::new);
        if let Some(t) = &spawn_table {
            vanillite_game::mob::spawn::install_table(Arc::clone(t));
        }
        let slot = |name: &str, origin: &Origin| log::info!("Slot {name}: {origin}");
        let compression = crate::world_format::anvil_compression(&config);
        let world_format = if format_plan.convert_to.is_some() {
            let infos = dimensions
                .iter()
                .map(|(_, d, kind)| {
                    crate::world_setup::world_info(
                        packs.as_deref(),
                        layout.dimension_dir(&dir, &level_name, d),
                        d,
                        kind,
                        seed,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            crate::world_format::convert_on_start(
                format_plan,
                &config,
                &dir,
                &level_name,
                &infos,
                workers,
            )
        } else {
            format_plan.format
        };
        let own_level = i32::from(config.server.storage.compression_level);
        let upgrades = match world_format {
            WorldFormat::Vanillite => {
                vanillite_storage::upgrade::Upgrades::load(&dir.join(&level_name))
                    .map_err(|e| StartError::Io(format!("world \"{level_name}\": {e}")))?
            }
            WorldFormat::Anvil => vanillite_storage::upgrade::Upgrades::none(),
        };
        let storage_rules = &config.server.storage;
        let io = IoPool::new(usize::from(storage_rules.io_threads))
            .map_err(|e| StartError::Io(format!("cannot start the storage IO threads: {e}")))?;
        let store_settings = StoreSettings {
            sync: config.properties.sync_chunk_writes,
            read_ahead: storage_rules.read_ahead,
            // Under a memory limit at most 1/32 of it per dimension (32 MiB at 1 GiB).
            read_cache: budget.cache_bytes(
                1.0 / 32.0,
                0,
                (storage_rules.read_cache as usize) << 20,
            ),
            max_open_files: storage_rules.max_open_files as usize,
        };
        let (storage_provider, origin) = self.chunk_storage.take().unwrap_or_else(|| {
            (
                Box::new(RegionStoreProvider {
                    format: world_format,
                    pool: io.clone(),
                    settings: store_settings,
                }),
                Origin::BuiltIn,
            )
        });
        let built_in_storage =
            matches!(origin, Origin::BuiltIn).then(|| (io.clone(), store_settings));
        let save_settings = SaveSettings {
            dimension: DimensionId(0),
            tick_budget: Duration::from_micros(u64::from(
                config.server.storage_autosave.tick_budget,
            )),
            min_chunks_per_tick: config.server.storage_autosave.min_chunks_per_tick as usize,
            commit_interval: u64::from(storage_rules.commit_interval) * 20,
            max_backlog: (storage_rules.max_pending_writes as usize) << 20,
        };
        slot("chunk-storage", &origin);
        let terrains = crate::world_setup::GeneratorsByDimension::default();
        let extra_gens = crate::world_setup::ExtraWorldGens::default();
        log::info!("World format: {}", world_format.name());
        // `StructureTemplateManager`: one cache for world generation, structure blocks and
        // commands, like Vanilla's.
        let templates = Arc::new(
            vanillite_worldgen::template::StructureTemplateManager::for_world(
                packs
                    .as_deref()
                    .map(|p| p.roots().to_vec())
                    .unwrap_or_default(),
                level_dir.join("generated"),
            ),
        );
        let built_in_generators = {
            let packs = packs.clone();
            let world_gen = prepared.world_gen;
            let created = Arc::clone(&terrains);
            let extra = Arc::clone(&extra_gens);
            let templates = Arc::clone(&templates);
            move || -> Box<dyn ChunkGeneratorProvider> {
                Box::new(crate::world_setup::BuiltInGenerators {
                    packs,
                    world_gen,
                    created,
                    extra: Arc::clone(&extra),
                    templates: Arc::clone(&templates),
                })
            }
        };
        let (generator_provider, origin): (Box<dyn ChunkGeneratorProvider>, Origin) =
            match self.chunk_generator.take() {
                Some((embedded, origin)) => {
                    if let Some((id, _)) = plugins.take_chunk_generator(built_in_generators) {
                        return Err(StartError::Slot(SlotError {
                            slot: "chunk-generator",
                            existing: origin,
                            rejected: Origin::Plugin(id),
                        }));
                    }
                    (embedded, origin)
                }
                None => match plugins.take_chunk_generator(built_in_generators.clone()) {
                    Some((id, provider)) => (Box::new(provider), Origin::Plugin(id)),
                    None => (built_in_generators(), Origin::BuiltIn),
                },
            };
        let generator_provider = match self.wrap_generator {
            Some(GeneratorWrap(wrap)) => wrap(generator_provider),
            None => generator_provider,
        };
        // Plugin chunk populators run after the steps of whichever generator fills the slot.
        let generator_provider =
            crate::plugins::with_populators(generator_provider, plugins.chunk_populators());
        slot("chunk-generator", &origin);
        let (light_provider, origin) = self
            .light_engine
            .take()
            .unwrap_or_else(|| (Box::new(BfsLightEngineProvider), Origin::BuiltIn));
        slot("light-engine", &origin);
        let executor = Arc::new(crate::world_setup::ComputeExecutor(
            compute.clone(),
            Arc::clone(&budget),
        ));

        let clock = Clock::new();
        let max_players = usize::try_from(config.properties.max_players).unwrap_or(0);
        let mut commands = crate::commands::build_dispatcher(
            command_limits(&config),
            config.server.commands.bukkit_commands,
        );
        plugins.register_commands(&mut commands, vanillite_game::commands::CommandId::Plugin);
        let game = Arc::new(crate::blocks::game_data(packs.as_deref()));
        vanillite_game::player::play::install_game_data(Arc::clone(&game));
        let recipes_packet = vanillite_protocol::SharedPacket::of(&game.recipes.update_packet());
        let level = crate::level::LevelState::load(
            &level_dir,
            packs.as_deref(),
            crate::survival::difficulty(config.properties.difficulty),
            &level_setup,
            writer.clone(),
        );
        level.set_data_configuration(&pack_config, Some(&data_packs.features));
        // `MinecraftServer.randomSequences`: shared by all worlds and `/random`, restored from
        // the saved data with the game systems before the first tick.
        let random_sequences = Arc::new(std::sync::Mutex::new(
            vanillite_game::random_sequences::RandomSequences::new(seed),
        ));
        let server_icon = crate::admin::server_icon_png(&dir, &config.properties.level_name);
        let global = Arc::new_cyclic(|this| Global {
            clock,
            players: PlayerList::new(max_players),
            network_ids: Arc::new(NetworkIds::new()),
            chat,
            chat_types: prepared.chat_types,
            access: crate::access::Access::load(
                &dir,
                config.properties.white_list,
                config.properties.op_permission_level,
                config.properties.enforce_whitelist,
            ),
            handle: std::sync::OnceLock::new(),
            commands: std::sync::RwLock::new(Arc::new(commands)),
            online_mode: config.properties.online_mode,
            dimensions: dimensions.clone(),
            level,
            seed: level_seed(&config.properties.level_seed),
            registries: prepared.registries.clone(),
            game_data: std::sync::RwLock::new(game),
            recipes_packet: std::sync::RwLock::new(recipes_packet),
            spawn_protection: config.properties.spawn_protection,
            systems: crate::systems::GameSystems::new(
                level_dir.clone(),
                u8::try_from(config.properties.function_permission_level).unwrap_or(2),
                Arc::clone(&random_sequences),
            ),
            packs: data_packs.with_loaded(packs.clone(), self.harness),
            templates,
            tick_rate: std::sync::Mutex::new(TickRateManager::default()),
            tick_times: std::sync::Mutex::new(crate::tick_rate::TickTimes::default()),
            diagnostics: crate::diagnostics::Diagnostics::new(),
            force_game_mode: config.properties.force_gamemode,
            language: Arc::new(prepared.language),
            broadcast_console_to_ops: config.properties.broadcast_console_to_ops,
            broadcast_rcon_to_ops: config.properties.broadcast_rcon_to_ops,
            plugins,
            version: self.version.clone(),
            motd: std::sync::Mutex::new(None),
            server_data: std::sync::RwLock::new(crate::admin::server_data(
                vanillite_protocol::text::Component::text(config.properties.motd.clone()),
                server_icon.clone(),
            )),
            server_icon,
            this: this.clone(),
        });
        global
            .plugins
            .set_server(Arc::new(crate::plugins::Services::new(
                &global, &compute, &config, &dir,
            )));
        if let Some(packs) = packs.as_deref() {
            global.systems.set_tags(crate::packs::registry_tags(packs));
        }
        global.systems.set_spawn(level_setup.spawn.0);
        if let Some(table) = &spawn_table {
            let _ = global.systems.mob_spawns.set(Arc::clone(table));
        }
        crate::commands::load_game_systems(&global, &opened_packs);
        crate::commands::install_map_explorer(&global);
        let watch = Arc::new(WatchState::new(
            config.server.watchdog.early_warning_after.get(),
            config.server.watchdog.warning_interval,
        ));
        let heartbeat = Arc::new(Heartbeat::new("Server thread"));
        watch.register(Arc::clone(&heartbeat));
        let (sender, receiver) = mpsc::channel();

        log::info!("Preparing level \"{level_name}\"");
        let factory = crate::worlds::WorldFactory {
            trees: crate::world_setup::tree_features(packs.as_deref(), seed),
            packs: packs.clone(),
            tracker: tracker_provider,
            storage: storage_provider,
            built_in_storage,
            generator: generator_provider,
            light: light_provider,
            executor: executor as Arc<dyn vanillite_world::chunks::Executor>,
            compression,
            own_level,
            save: save_settings,
            scarce: budget.scarce(),
            harness: self.harness,
            behaviours,
            terrains,
            extra_gens,
            writer: writer.clone(),
            random_sequences,
            spawn_table,
            seed,
            format: world_format,
        };
        let mut worlds = Vec::with_capacity(dimensions.len());
        for (id, dimension, kind) in dimensions.iter() {
            let players = world_players
                .next()
                .ok_or_else(|| StartError::Io("world setup missing".to_owned()))?;
            let place = crate::worlds::WorldPlace {
                dir: layout.dimension_dir(&dir, &level_name, dimension),
                seed,
                format: world_format,
                upgrades: Arc::clone(&upgrades),
                world_gen: &world_gen,
            };
            worlds.push(factory.build(&global, &config, id, (dimension, kind), &place, players)?);
        }
        if !spawn_known {
            initial_spawn(&mut worlds, &global, world_gen.flat_spawn_height());
        }
        prepare_levels(&mut worlds);
        let mut owned = if parallel {
            log::info!("Dimensions tick in parallel");
            Worlds::Parallel(Vec::with_capacity(worlds.len()))
        } else {
            log::info!("Dimensions tick sequentially");
            Worlds::Sequential(Vec::with_capacity(worlds.len()))
        };
        let routes = Arc::new(std::sync::RwLock::new(Vec::with_capacity(worlds.len())));
        for world in worlds {
            attach_world(
                &mut owned, &routes, world, &compute, &global, &sender, &watch,
            )?;
        }
        let worlds = owned;

        let net = NetState::start(
            &config,
            &dir,
            prepared.configuration,
            self.network,
            global.chat.enforce_secure,
            Arc::clone(&global.language),
            Arc::clone(global.plugins.packet_tap()),
            match config.server.threads.network {
                0 => budget.limits().default_network_threads(),
                n => usize::from(n),
            },
        )?;
        if !config.properties.online_mode {
            log::warn!("**** SERVER IS RUNNING IN OFFLINE/INSECURE MODE!");
        }

        let watchdog = Watchdog::start(Arc::clone(&watch), clock, config.properties.max_tick_time)
            .map_err(|e| StartError::io(&dir, e))?;

        let mut server = Server {
            dir,
            config,
            compute,
            budget,
            global,
            worlds,
            routes,
            sender,
            receiver,
            global_queue: VecDeque::new(),
            tick_count: 0,
            next_tick: 0,
            empty_ticks: 0,
            memory_settle_ticks: 0,
            ticks_until_autosave: 0,
            average_tick_nanos: 0.0,
            last_tick_nanos: 0,
            tick_log: open_tick_log(),
            running: true,
            heartbeat,
            watch,
            watchdog: Some(watchdog),
            net: Some(net),
            test_bots: Vec::new(),
            dupe_waiting: Vec::new(),
            layout,
            player_store,
            factory,
            extra_locks: vanillite_base::hash::FxHashMap::default(),
            world_gen,
            next_backup: Instant::now(),
            backup: None,
            io,
            _session_lock: session_lock,
        };
        server.next_backup = server.backup_due_after();
        server.ticks_until_autosave = server.autosave_interval();
        let _ = server.global.handle.set(server.handle());
        if let Some(net) = &server.net {
            crate::admin::start_rcon(
                &server.config,
                &net.network,
                server.handle(),
                &server.global,
            );
            crate::admin::start_query(&server.config, &net.network, Arc::clone(&net.query));
        }
        for world in &server.config.extra_worlds.clone() {
            let opened = crate::worlds::WorldSpec::from_config(world)
                .and_then(|spec| server.open_world(&spec, true).map_err(|e| e.to_string()));
            if let Err(e) = opened {
                log::error!("Cannot load the extra world {:?}: {e}", world.name);
            }
        }
        server.global.plugins.enable_all();
        let bots = server.config.server.debug.test_bots;
        if bots > 0
            && let Some(opener) = server.virtual_opener()
        {
            log::warn!("Starting {bots} test bot(s) (debug.test-bots); not for production use");
            server.test_bots = crate::test_bot::uuids(bots);
            crate::test_bot::spawn(opener, bots);
        }
        log::info!(
            "Done ({:.3}s)! For help, type \"help\"",
            started.elapsed().as_secs_f64()
        );
        Ok(server)
    }
}

/// Holds `session.lock` of the world folder so no second server opens the same world.
/// The world seed from `level-seed`: a number, else Java's hash of the text, 0 when empty
/// (`WorldOptions.parseSeed`; the world runtime will store a random seed instead).
fn level_seed(text: &str) -> i64 {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    text.parse::<i64>()
        .unwrap_or_else(|_| i64::from(vanillite_base::math::java_string_hash(text)))
}

/// The command parser limits from `vanillite.toml` (`commands.*`).
fn command_limits(config: &Config) -> vanillite_command::limits::Limits {
    let c = &config.server.commands;
    let size = |v: u32| usize::try_from(v).unwrap_or(usize::MAX);
    vanillite_command::limits::Limits {
        max_command_length: size(c.max_command_length),
        max_suggestion_input: size(c.max_suggestion_input),
        max_suggestions: size(c.max_suggestions),
        max_message_length: size(c.max_message_length),
        max_nbt_depth: c.max_nbt_depth,
        max_nbt_path_nodes: size(c.max_nbt_path_nodes),
        max_selector_options: size(c.max_selector_options),
        max_message_selectors: size(c.max_message_selectors),
        max_redirect_depth: size(c.max_redirect_depth),
    }
}

/// The chat settings: secure chat is enforced when `enforce-secure-profile` is on, the server
/// runs in online mode and Mojang's keys could be fetched (`enforceSecureProfile`).
fn chat_settings(
    config: &Config,
    services_keys: Option<JoinHandle<Result<Vec<Vec<u8>>, String>>>,
) -> vanillite_game::player::chat::ChatSettings {
    use vanillite_game::player::chat::{ChatSettings, ServicesKeys};
    let keys = match services_keys.map(JoinHandle::join) {
        Some(Ok(Ok(keys))) => ServicesKeys::from_der(keys.iter().map(Vec::as_slice)),
        Some(Ok(Err(e))) => {
            log::warn!("Failed to fetch Mojang's services keys, chat cannot be verified: {e}");
            ServicesKeys::default()
        }
        Some(Err(_)) | None => ServicesKeys::default(),
    };
    let p = &config.properties;
    ChatSettings {
        enforce_secure: p.enforce_secure_profile && p.online_mode && !keys.is_empty(),
        keys: Arc::new(keys),
        chat_spam_threshold: p.chat_spam_threshold_seconds.saturating_mul(20),
        command_spam_threshold: p.command_spam_threshold_seconds.saturating_mul(20),
        max_pending_messages: usize::try_from(config.server.chat.max_pending_messages)
            .unwrap_or(4096),
    }
}

pub(crate) fn lock_world(level_dir: &Path) -> Result<File, StartError> {
    fs::create_dir_all(level_dir).map_err(|e| StartError::io(level_dir, e))?;
    let path = level_dir.join("session.lock");
    let mut file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| StartError::io(&path, e))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Err(StartError::WorldInUse(path)),
        Err(fs::TryLockError::Error(e)) => return Err(StartError::io(&path, e)),
    }
    // Vanilla writes a snowman into the lock file.
    file.set_len(0)
        .and_then(|()| file.write_all("\u{2603}".as_bytes()))
        .map_err(|e| StartError::io(&path, e))?;
    Ok(file)
}

fn spawn_world_thread(
    mut world: ServerWorld,
    compute: &Compute,
    global: &Arc<Global>,
    server: Sender<ServerMsg>,
    heartbeat: Arc<Heartbeat>,
) -> Result<WorldThread, StartError> {
    let (sender, receiver) = mpsc::channel::<WorldMsg>();
    let wake = sender.clone();
    world.set_chunk_waker(Arc::new(move || {
        let _ = wake.send(WorldMsg::Task(Box::new(ServerWorld::pump_chunks)));
    }));
    let name = format!("World {}", world.dimension());
    let id = world.id();
    let thread_compute = compute.clone();
    let global = Arc::clone(global);
    let body = move || {
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            world_loop(
                &mut world,
                &receiver,
                &server,
                &thread_compute,
                &global,
                &heartbeat,
            );
        }));
        if result.is_err() {
            let _ = server.send(ServerMsg::WorldCrashed(id));
        }
    };
    let handle = compute
        .scheduler()
        .spawn_thread(name, Box::new(body))
        .map_err(|e| StartError::Io(format!("cannot start a world thread: {e}")))?;
    Ok(WorldThread { sender, handle })
}

/// The loop of a world thread: ticks on command and runs the world's tasks in between.
fn world_loop(
    world: &mut ServerWorld,
    receiver: &Receiver<WorldMsg>,
    server: &Sender<ServerMsg>,
    compute: &Compute,
    global: &Global,
    heartbeat: &Heartbeat,
) {
    for msg in receiver {
        heartbeat.enter(&global.clock);
        match msg {
            WorldMsg::Tick {
                tick,
                runs_normally,
                deadline,
            } => {
                let mut outbox = Vec::new();
                let mut ctx = TickContext::new(
                    world.id(),
                    tick,
                    runs_normally,
                    deadline,
                    compute,
                    global,
                    &mut outbox,
                );
                world.tick(&mut ctx);
                let _ = server.send(ServerMsg::WorldTicked(world.id(), outbox));
            }
            WorldMsg::Task(task) => task(world),
            WorldMsg::Stop { save } => {
                world.save(true, !save);
                heartbeat.leave();
                return;
            }
        }
        heartbeat.leave();
    }
}

/// A handle to a running server, usable from any thread (console, network, signals).
#[derive(Clone)]
pub struct ServerHandle {
    sender: Sender<ServerMsg>,
    routes: Routes,
}

impl fmt::Debug for ServerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerHandle").finish_non_exhaustive()
    }
}

impl ServerHandle {
    /// Asks the server to stop after the current tick.
    pub fn stop(&self) {
        let _ = self.sender.send(ServerMsg::Stop);
    }

    /// Queues a console command for the next global pre-phase.
    pub fn console(&self, line: String) {
        let _ = self.sender.send(ServerMsg::Console(line));
    }

    /// Queues work for the next global pre-phase.
    pub fn run_global(&self, task: impl FnOnce(&mut Server) + Send + 'static) {
        let _ = self.sender.send(ServerMsg::Global(Box::new(task)));
    }

    /// Runs work on a world between its ticks. Returns `false` for an unknown world or a
    /// stopped server.
    pub fn run_in_world(
        &self,
        world: WorldId,
        task: impl FnOnce(&mut ServerWorld) + Send + 'static,
    ) -> bool {
        let task: WorldTask = Box::new(task);
        let route = self
            .routes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(world.index())
            .cloned()
            .flatten();
        match route {
            Some(Route::Server) => self.sender.send(ServerMsg::WorldTask(world, task)).is_ok(),
            Some(Route::World(sender)) => sender.send(WorldMsg::Task(task)).is_ok(),
            None => false,
        }
    }
}

/// A running server. [`Server::run`] runs the tick loop on the calling thread until stopped.
pub struct Server {
    dir: PathBuf,
    config: Config,
    compute: Compute,
    budget: Arc<Budget>,
    global: Arc<Global>,
    worlds: Worlds,
    routes: Routes,
    sender: Sender<ServerMsg>,
    receiver: Receiver<ServerMsg>,
    /// Console commands and global tasks waiting for the next global pre-phase.
    global_queue: VecDeque<ServerMsg>,
    tick_count: u64,
    next_tick: u64,
    empty_ticks: u64,
    /// Ticks until the memory an empty server released is handed back to the system; 0 = no
    /// hand-back is due.
    memory_settle_ticks: u32,
    ticks_until_autosave: i64,
    average_tick_nanos: f64,
    /// `lastTickNanos`: when the last full tick (tick and wait) ended.
    last_tick_nanos: u64,
    /// Load test tick log (`VANILLITE_TICK_LOG`): one line with the duration in nanoseconds per
    /// tick. A stand-in for the metrics endpoint of architecture 3.8.
    tick_log: Option<io::BufWriter<File>>,
    running: bool,
    heartbeat: Arc<Heartbeat>,
    watch: Arc<WatchState>,
    watchdog: Option<Watchdog>,
    net: Option<NetState>,
    /// UUIDs of the test bots this server started (`debug.test-bots`).
    test_bots: Vec<u128>,
    /// Joins whose UUID was still online, with the tick they arrived in: they wait until the
    /// kicked older session has left (Vanilla's `WAITING_FOR_DUPE_DISCONNECT`).
    dupe_waiting: Vec<(vanillite_net::Joined, u64)>,
    layout: WorldLayout,
    /// Player data files, written in the background.
    player_store: PlayerDataStore,
    /// Builds the worlds that load while the server runs.
    factory: crate::worlds::WorldFactory,
    /// The `session.lock` of each loaded extra world's folder.
    extra_locks: vanillite_base::hash::FxHashMap<WorldId, File>,
    /// The default world's generator settings (extra worlds may take a dimension's generator).
    world_gen: crate::world_setup::WorldGen,
    /// When the next automatic backup is due.
    next_backup: Instant,
    /// The thread of the running or last backup.
    backup: Option<JoinHandle<()>>,
    /// The disk IO threads of the storage service.
    io: IoPool,
    _session_lock: File,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("dir", &self.dir)
            .field("tick_count", &self.tick_count)
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Opens in-memory connections for vouched-for profiles (architecture 14.1), from any
    /// thread while the server runs; they log in like a client from the handshake on.
    pub fn virtual_opener(&self) -> Option<VirtualOpener> {
        self.net.as_ref().map(|n| n.network.virtual_opener())
    }

    /// The network, while it runs.
    pub(crate) fn network(&self) -> Option<&vanillite_net::Network> {
        self.net.as_ref().map(|n| &n.network)
    }

    /// The UDP address of the Bedrock frontend, if it runs.
    pub fn bedrock_addr(&self) -> Option<std::net::SocketAddr> {
        self.net
            .as_ref()?
            .bedrock
            .as_ref()
            .map(|(f, _)| f.local_addr())
    }

    /// The address the network listens on.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.net.as_ref().map(|n| n.network.local_addr())
    }

    /// A handle for other threads.
    pub fn handle(&self) -> ServerHandle {
        ServerHandle {
            sender: self.sender.clone(),
            routes: Arc::clone(&self.routes),
        }
    }

    /// The loaded configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Server-wide state.
    pub fn global(&self) -> &Global {
        &self.global
    }

    /// The compute pool.
    pub fn compute(&self) -> &Compute {
        &self.compute
    }

    /// The tick rate state (`/tick`).
    pub fn tick_rate(&self) -> std::sync::MutexGuard<'_, TickRateManager> {
        self.global.tick_rate()
    }

    /// Ticks run since start.
    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }

    /// The smoothed time of a server tick in milliseconds (MSPT).
    pub fn average_tick_millis(&self) -> f64 {
        self.average_tick_nanos / 1e6
    }

    /// Stops the server after the current tick.
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Runs the tick loop until the server is stopped, then shuts down.
    pub fn run(mut self) {
        let clock = self.global.clock;
        self.next_tick = clock.now();
        self.last_tick_nanos = self.next_tick;
        let mut last_overload_warning = self.next_tick;
        while self.running {
            let now = clock.now();
            let (sprint_tick, finished, nanos_per_tick) = {
                let mut rate = self.global.tick_rate();
                let sprint = rate.is_sprinting() && rate.check_should_sprint_this_tick(now);
                (sprint, rate.take_finished_sprint(), rate.nanos_per_tick())
            };
            if let Some(report) = finished {
                crate::commands::sprint_finished(&self.global, &report);
            }
            let this_tick = if sprint_tick {
                self.next_tick = now;
                last_overload_warning = now;
                0
            } else {
                let this_tick = nanos_per_tick;
                let behind = now.saturating_sub(self.next_tick);
                if behind > OVERLOADED_THRESHOLD + 20 * this_tick
                    && self.next_tick.saturating_sub(last_overload_warning)
                        >= OVERLOADED_WARNING_INTERVAL + 100 * this_tick
                {
                    let missed = behind / this_tick;
                    log::warn!(
                        "Can't keep up! Is the server overloaded? Running {}ms or {missed} ticks behind",
                        behind / 1_000_000
                    );
                    self.next_tick += missed * this_tick;
                    last_overload_warning = self.next_tick;
                }
                this_tick
            };
            let sprinting = this_tick == 0;
            self.next_tick += this_tick;
            let tick_started = clock.now();
            self.tick_server();
            let wait_started = clock.now();
            let idle = self.wait_until_next_tick(this_tick, sprinting);
            let waited = clock.now().saturating_sub(wait_started);
            if sprinting {
                self.global.tick_rate().end_tick_work(clock.now());
            }
            self.log_full_tick_time([
                wait_started.saturating_sub(tick_started),
                waited.saturating_sub(idle),
                idle,
            ]);
        }
        self.shutdown();
    }

    /// Vanilla's `tickServer`: pause check, global pre-phase, world phase, global post-phase.
    fn tick_server(&mut self) {
        let clock = self.global.clock;
        let started = clock.now();
        self.budget.enter_tick();
        self.heartbeat.enter(&clock);
        self.watch.progress.store(started, Ordering::Relaxed);

        if self.global.players.count() == 0 && !self.global.tick_rate().is_sprinting() {
            self.empty_ticks += 1;
        } else {
            self.empty_ticks = 0;
        }
        self.release_memory_when_empty();
        let pause_seconds =
            u64::try_from(self.config.properties.pause_when_empty_seconds).unwrap_or(0);
        if pause_seconds > 0 {
            let threshold = pause_seconds * 20;
            if self.empty_ticks >= threshold {
                if self.empty_ticks == threshold {
                    log::info!("Server empty for {pause_seconds} seconds, pausing");
                    // Paused worlds run no save rounds: write everything now.
                    self.ticks_until_autosave = self.autosave_interval();
                    self.save_everything(true, false, None);
                }
                // The console stays responsive while paused, like Vanilla.
                self.global_pre_phase();
                self.accept_joins();
                if self.global.diagnostics.profiler.is_active() {
                    // A profile ends while paused too; the paused ticks run no world.
                    let spent = Duration::from_nanos(clock.now().saturating_sub(started));
                    self.profile_tick(crate::profiler::ServerLaps {
                        before: spent,
                        worlds: Duration::ZERO,
                        after: Duration::ZERO,
                        total: spent,
                    });
                }
                self.heartbeat.leave();
                self.budget.leave_tick(clock.now().saturating_sub(started));
                self.watch.progress.store(clock.now(), Ordering::Relaxed);
                return;
            }
        }

        self.tick_count += 1;
        self.global.tick_rate().tick();
        // `/vanillite profile`: the laps are taken only while a profile runs.
        let profiling = self.global.diagnostics.profiler.is_active();
        self.global_pre_phase();
        self.tick_clocks();
        // After the clocks: a join sends its login on the player's world, so no broadcast of
        // this thread (the time sync) may run between the player list entry and that.
        self.accept_joins();
        let before_worlds = profiling.then(|| clock.now());
        self.tick_worlds();
        let after_worlds = profiling.then(|| clock.now());
        self.global_post_phase();

        let elapsed_nanos = clock.now().saturating_sub(started);
        if let (Some(before), Some(after)) = (before_worlds, after_worlds) {
            let nanos = Duration::from_nanos;
            self.profile_tick(crate::profiler::ServerLaps {
                before: nanos(before.saturating_sub(started)),
                worlds: nanos(after.saturating_sub(before)),
                after: nanos(started.saturating_add(elapsed_nanos).saturating_sub(after)),
                total: nanos(elapsed_nanos),
            });
        }
        self.global
            .tick_times
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_tick(self.tick_count, started, elapsed_nanos);
        let elapsed = elapsed_nanos as f64;
        self.average_tick_nanos = self.average_tick_nanos * 0.8 + elapsed * 0.2;
        if let Some(log) = &mut self.tick_log {
            let written = writeln!(log, "{}", clock.now().saturating_sub(started));
            if written.is_err() || (self.tick_count.is_multiple_of(20) && log.flush().is_err()) {
                log::warn!("Cannot write the tick log, disabling it");
                self.tick_log = None;
            }
        }
        self.heartbeat.leave();
        self.budget.leave_tick(elapsed_nanos);
        if self.tick_count.is_multiple_of(MEMORY_RELEASE_TICKS) {
            let budget = Arc::clone(&self.budget);
            self.compute
                .spawn_background(u32::MAX, move || budget.release_memory());
        }
        self.watch.progress.store(clock.now(), Ordering::Relaxed);
    }

    /// `performance.memory.release-when-empty`: after that time without players (and again
    /// every such interval while the server stays empty) the worlds free what they can build
    /// again, and a little later the allocator hands the freed memory back to the system.
    fn release_memory_when_empty(&mut self) {
        if self.empty_ticks == 0 {
            self.memory_settle_ticks = 0;
            return;
        }
        if self.memory_settle_ticks > 0 {
            self.memory_settle_ticks -= 1;
            if self.memory_settle_ticks == 0 {
                // On this thread: nobody is online to notice, and the workers stay untouched.
                self.budget.release_idle_memory();
            }
        }
        let Some(seconds) = self.config.server.memory.release_when_empty.get() else {
            return;
        };
        if !self.empty_ticks.is_multiple_of(u64::from(seconds) * 20) {
            return;
        }
        log::debug!("Server empty: releasing memory");
        self.global.templates.trim();
        self.compute.release_memory();
        for id in self.loaded_worlds() {
            self.run_in_world(id, Box::new(ServerWorld::release_memory));
        }
        self.memory_settle_ticks = EMPTY_RELEASE_SETTLE_TICKS;
    }

    /// G1: console commands and global tasks, with `&mut` to everything because no world is
    /// ticking.
    fn global_pre_phase(&mut self) {
        while let Some(msg) = self.global_queue.pop_front() {
            match msg {
                ServerMsg::Console(line) => self.run_console_command(&line),
                ServerMsg::Global(task) => task(self),
                other => self.dispatch(other),
            }
        }
        self.global.plugins.run_global_tasks(self.tick_count);
        self.add_late_plugin_commands();
    }

    /// Commands plugins registered after the start (Bukkit plugins in `onEnable`), collected
    /// over the tick: a new tree with them appended, sent to every player again.
    fn add_late_plugin_commands(&self) {
        let current = self.global.commands();
        let Some(next) = self
            .global
            .plugins
            .add_late_commands(&current, vanillite_game::commands::CommandId::Plugin)
        else {
            return;
        };
        *self
            .global
            .commands
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(next);
        for player in self.global.players.snapshot() {
            crate::commands::permissions_changed(&self.global, player.uuid.0);
        }
    }

    /// `tickChildren`'s clocks and the game time sync every second (by default).
    fn tick_clocks(&mut self) {
        let level = &self.global.level;
        if self.global.tick_rate().runs_normally() {
            let advance = level
                .rules()
                .bool(vanillite_game::survival::rules::Rule::ADVANCE_TIME);
            level.clocks().tick(advance);
        }
        let interval = u64::from(self.config.server.survival.time_sync_interval.max(1));
        if self.tick_count.is_multiple_of(interval) {
            crate::survival::broadcast_all(&self.global, &crate::survival::game_time_sync(level));
        }
    }

    /// Decides the waiting logins, then takes joined connections into the game, at most
    /// `security.max-joins-per-tick`.
    fn accept_joins(&mut self) {
        while let Some(check) = self.net.as_ref().and_then(NetState::next_login) {
            self.verify_login(check);
        }
        for (joined, since) in std::mem::take(&mut self.dupe_waiting) {
            self.accept_waiting(joined, since);
        }
        let max = self.config.server.security.max_joins_per_tick;
        for _ in 0..max {
            let Some(joined) = self.net.as_ref().and_then(NetState::next_join) else {
                break;
            };
            if self.global.players.contains(Uuid(joined.profile.id.0)) {
                // `disconnectAllPlayersWithProfile`: the older session goes, the new one waits.
                if let Some((sink, _)) = self.global.players.connection(Uuid(joined.profile.id.0)) {
                    sink.kick(vanillite_protocol::text::Component::translatable(
                        "multiplayer.disconnect.duplicate_login",
                        vec![],
                    ));
                }
                self.accept_waiting(joined, self.tick_count);
            } else {
                self.accept(joined);
            }
        }
    }

    /// A join waiting for its older session to leave: accepted once it has left, refused with
    /// `slow_login` after Vanilla's login limit of 600 ticks.
    fn accept_waiting(&mut self, joined: vanillite_net::Joined, since: u64) {
        if !self.global.players.contains(Uuid(joined.profile.id.0)) {
            self.accept(joined);
        } else if self.tick_count.saturating_sub(since) >= 600 {
            let properties = &self.config.properties;
            let (_, sink) =
                crate::net::arrival(joined, None, properties.gamemode, properties.force_gamemode);
            sink.kick(vanillite_protocol::text::Component::translatable(
                "multiplayer.disconnect.slow_login",
                vec![],
            ));
        } else {
            self.dupe_waiting.push((joined, since));
        }
    }

    /// `PlayerList.placeNewPlayer`, global part: checks, player data, the player list; the
    /// world part runs on the player's world.
    fn accept(&mut self, joined: vanillite_net::Joined) {
        let uuid = joined.profile.id.0;
        // `ServerPlayer.getIpAddress`.
        let ip = joined.address.ip().to_string();
        let saved = match self.player_store.load(uuid) {
            Ok(saved) => saved,
            Err(e) => {
                log::error!("Failed to load player data of {}: {e}", joined.profile.name);
                None
            }
        };
        let properties = &self.config.properties;
        // `getDefaultGameType`: `level.dat`'s game type, which `/defaultgamemode` changes.
        let default_mode = crate::players::game_mode_by_id(self.global.level.default_game_type())
            .unwrap_or(properties.gamemode);
        let overridden = joined.login_overridden;
        let (mut arrival, sink) =
            crate::net::arrival(joined, saved, default_mode, properties.force_gamemode);
        // `handleConfigurationFinished` checks `canPlayerLogin` again (a ban or a full server
        // since the login); a plugin's earlier override stands.
        if !overridden
            && let (_, Some(reason)) = self.can_player_login(uuid, &ip, arrival.memory_connection)
        {
            sink.kick(reason);
            return;
        }
        // `PlayerList.getPlayerStats` and `getPlayerAdvancements`.
        let name = arrival.profile.name.clone();
        let json = |kind| match self.player_store.load_json(kind, uuid) {
            Ok(text) => text,
            Err(e) => {
                log::error!("Couldn't read {kind:?} of {name}: {e}");
                None
            }
        };
        arrival.stats = json(JsonFile::Stats);
        arrival.advancements = json(JsonFile::Advancements);
        self.place_new_player(arrival, ip);
    }

    /// `ServerLoginPacketListenerImpl.verifyLoginAndFinishConnectionSetup`: refuses the login
    /// (a login disconnect) or lets it finish; `PLAYER_LOGIN` plugins may change the result.
    fn verify_login(&self, check: vanillite_net::LoginCheck) {
        let uuid = check.profile.id.0;
        let ip = check.address.ip().to_string();
        let (reason, refusal) = self.can_player_login(uuid, &ip, check.memory);
        let decided = if self
            .global
            .plugins
            .wants(vanillite_abi::event::PLAYER_LOGIN)
        {
            let name = check.profile.name.clone();
            crate::plugins::login(&self.global, uuid, &name, &ip, reason, refusal.clone())
        } else {
            refusal.clone()
        };
        match decided {
            Some(reason) => check.refuse(reason),
            None => check.admit(refusal.is_some()),
        }
    }

    /// `PlayerList.canPlayerLogin`: why the player may not log in (a [`login_reason`] and the
    /// message), checked in Vanilla's order.
    ///
    /// [`login_reason`]: vanillite_abi::login_reason
    fn can_player_login(
        &self,
        uuid: u128,
        ip: &str,
        memory: bool,
    ) -> (u32, Option<vanillite_protocol::text::Component>) {
        use vanillite_abi::login_reason;
        use vanillite_protocol::text::Component;
        let players = &self.global.players;
        let access = &self.global.access;
        // The server's own test bots come over virtual connections it opened itself.
        let test_bot = memory && self.test_bots.contains(&uuid);
        if let Some(reason) = crate::commands::ban_reason(access, uuid, None) {
            (login_reason::BANNED, Some(reason))
        } else if !test_bot && !access.is_whitelisted(uuid) {
            (
                login_reason::NOT_WHITELISTED,
                Some(Component::translatable(
                    "multiplayer.disconnect.not_whitelisted",
                    vec![],
                )),
            )
        } else if let Some(reason) = crate::commands::ban_reason(access, uuid, Some(ip)) {
            (login_reason::BANNED, Some(reason))
        } else if players.count() >= players.max_players() && !access.bypasses_player_limit(uuid) {
            (
                login_reason::FULL,
                Some(Component::translatable(
                    "multiplayer.disconnect.server_full",
                    vec![],
                )),
            )
        } else {
            (login_reason::ALLOWED, None)
        }
    }

    /// The end of [`Server::accept`]: the player list entry, then the world part on the
    /// player's world.
    fn place_new_player(&mut self, arrival: crate::players::Arrival, ip: String) {
        let players = &self.global.players;
        let uuid = arrival.uuid;
        let sink = Arc::clone(&arrival.sink);
        let world = arrival
            .saved
            .as_ref()
            .and_then(|s| self.global.dimensions.world(&s.dimension))
            .unwrap_or(WorldId(0));
        players.add(PlayerEntry {
            uuid: Uuid(uuid),
            name: arrival.profile.name.clone(),
            world,
            info: crate::players::info_entry(&arrival),
            sink,
            inbox: Arc::clone(&arrival.inbox),
            ip,
            allows_listing: arrival.client.allows_listing,
            details: arrival.details.clone(),
            bedrock: arrival.virtual_connection && !arrival.memory_connection,
            debug_subscriptions: 0,
        });
        let global = Arc::clone(&self.global);
        self.run_in_world(
            world,
            Box::new(move |w: &mut ServerWorld| w.join_player(&global, arrival)),
        );
    }

    /// The world phase and the cross-world hand-offs of G2, applied in world order.
    fn tick_worlds(&mut self) {
        let tick = self.tick_count;
        let runs_normally = self.global.tick_rate().runs_normally();
        let deadline = self.next_tick;
        let slots = match &self.worlds {
            Worlds::Sequential(worlds) => worlds.len(),
            Worlds::Parallel(threads) => threads.len(),
        };
        let mut outboxes: Vec<Vec<(WorldId, WorldTask)>> =
            std::iter::repeat_with(Vec::new).take(slots).collect();
        match &mut self.worlds {
            Worlds::Sequential(worlds) => {
                for (index, outbox) in outboxes.iter_mut().enumerate() {
                    let (before, rest) = worlds.split_at_mut(index);
                    let Some((world, after)) = rest.split_first_mut() else {
                        break;
                    };
                    let Some(world) = world else {
                        continue;
                    };
                    let mut ctx = TickContext::new(
                        world.id(),
                        tick,
                        runs_normally,
                        deadline,
                        &self.compute,
                        &self.global,
                        outbox,
                    )
                    .with_others(before, after);
                    world.tick(&mut ctx);
                }
            }
            Worlds::Parallel(threads) => {
                for thread in threads.iter().flatten() {
                    let _ = thread.sender.send(WorldMsg::Tick {
                        tick,
                        runs_normally,
                        deadline,
                    });
                }
                let mut remaining = threads.iter().flatten().count();
                while remaining > 0 {
                    let Ok(msg) = self.receiver.recv() else {
                        break;
                    };
                    match msg {
                        ServerMsg::WorldTicked(id, outbox) => {
                            if let Some(slot) = outboxes.get_mut(id.index()) {
                                *slot = outbox;
                            }
                            remaining -= 1;
                        }
                        ServerMsg::WorldCrashed(id) => {
                            self.world_crashed(id);
                            remaining -= 1;
                        }
                        other => self.global_queue.push_back(other),
                    }
                }
            }
        }
        for (target, op) in outboxes.into_iter().flatten() {
            self.run_in_world(target, op);
        }
    }

    /// G2: player list and autosave.
    fn global_post_phase(&mut self) {
        let online = self.global.players.count();
        let properties = &self.config.properties;
        if let Some(net) = self.net.as_mut() {
            net.set_online(online);
            let motd = self
                .global
                .motd
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(motd) = motd {
                let data = crate::admin::server_data(motd.clone(), self.global.server_icon.clone());
                *self
                    .global
                    .server_data
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = data;
                net.set_motd(motd);
            }
            // `rebuildServerStatus` every 5 seconds; the Query names every second.
            let (sample_due, query_due) = (
                self.tick_count.is_multiple_of(100),
                self.tick_count.is_multiple_of(20),
            );
            if sample_due || (query_due && properties.enable_query) {
                let players = self.global.players.snapshot();
                if sample_due {
                    let listed: Vec<(String, u128, bool)> = players
                        .iter()
                        .map(|p| (p.name.clone(), p.uuid.0, p.allows_listing))
                        .collect();
                    net.update_sample(&listed, properties.hide_online_players);
                }
                if properties.enable_query {
                    net.query.set(players.into_iter().map(|p| p.name).collect());
                    net.query
                        .set_plugins(crate::commands::plugins_cmd::query_plugins(
                            &self.global.version,
                            &self.global.plugins.list(),
                        ));
                }
            }
        }
        // Player list maintenance (latency updates, idle kicks) comes with players.
        self.ticks_until_autosave -= 1;
        if self.ticks_until_autosave <= 0 {
            self.autosave();
        }
        if self.config.server.backup.interval.get().is_some() && Instant::now() >= self.next_backup
        {
            self.next_backup = self.backup_due_after();
            self.start_backup();
        }
    }

    /// Runs messages between ticks until the next tick is due (Vanilla's `waitUntilNextTick`:
    /// when the tick ran late, queued work may still use up to one tick length).
    /// Returns the nanoseconds spent idle (waiting for messages).
    fn wait_until_next_tick(&mut self, this_tick: u64, sprinting: bool) -> u64 {
        let clock = self.global.clock;
        let mut idle = 0u64;
        if sprinting {
            while let Ok(msg) = self.receiver.try_recv() {
                self.dispatch(msg);
            }
            return idle;
        }
        let task_deadline = (clock.now() + this_tick).max(self.next_tick);
        let mut may_have_delayed = true;
        while self.running {
            let now = clock.now();
            let limit = if may_have_delayed {
                task_deadline
            } else {
                self.next_tick
            };
            if now >= limit {
                break;
            }
            match self.receiver.try_recv() {
                Ok(msg) => self.dispatch(msg),
                Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {
                    may_have_delayed = false;
                    if now >= self.next_tick {
                        break;
                    }
                    let received = self
                        .receiver
                        .recv_timeout(Duration::from_nanos(self.next_tick - now));
                    idle = idle.saturating_add(clock.now().saturating_sub(now));
                    match received {
                        Ok(msg) => self.dispatch(msg),
                        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        }
        idle
    }

    /// `logFullTickTime` of the dedicated server's `RemoteSampleLogger`: the
    /// `TpsDebugDimensions` sample (full tick; `tick` = `tickServer`, scheduled tasks and idle
    /// time of the wait) goes to the operators that subscribed to
    /// `dedicated_server_tick_time` (the F3 tick time chart).
    fn log_full_tick_time(&mut self, [tick, tasks, idle]: [u64; 3]) {
        use vanillite_protocol::packets::play::clientbound::{
            DebugSample, DebugSampleType, debug_subscription,
        };
        let now = self.global.clock.now();
        let full = now.saturating_sub(self.last_tick_nanos);
        self.last_tick_nanos = now;
        let receivers = self
            .global
            .players
            .debug_subscribers(debug_subscription::DEDICATED_SERVER_TICK_TIME);
        if receivers.is_empty() {
            return;
        }
        let nanos = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        let packet = vanillite_protocol::SharedPacket::of(&DebugSample {
            sample: vec![nanos(full), nanos(tick), nanos(tasks), nanos(idle)],
            kind: DebugSampleType::TickTime,
        });
        for (uuid, sink) in receivers {
            // `ServerDebugSubscribers.hasRequiredPermissions`: operators only.
            if self.global.access.op_level(uuid.0).is_some() {
                sink.send_shared(&packet);
            }
        }
    }

    /// Handles a message outside the world phase.
    fn dispatch(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Stop => self.running = false,
            ServerMsg::Console(_) | ServerMsg::Global(_) => self.global_queue.push_back(msg),
            ServerMsg::WorldTask(id, task) => self.run_in_world(id, task),
            ServerMsg::WorldCrashed(id) => self.world_crashed(id),
            ServerMsg::WorldTicked(id, _) => {
                log::warn!("Unexpected tick completion of world {}", id.0);
            }
        }
    }

    fn world_crashed(&mut self, id: WorldId) {
        log::error!(
            "The thread of world {} crashed; stopping the server",
            self.global.dimensions.name(id).unwrap_or("?")
        );
        self.running = false;
    }

    /// Runs a task on a world: directly in sequential mode, between its ticks on its thread in
    /// parallel mode.
    fn run_in_world(&mut self, id: WorldId, task: WorldTask) {
        match &mut self.worlds {
            Worlds::Sequential(worlds) => match worlds.get_mut(id.index()).and_then(Option::as_mut)
            {
                Some(world) => task(world),
                None => log::warn!("Task for unknown world {} dropped", id.0),
            },
            Worlds::Parallel(threads) => match threads.get(id.index()).and_then(Option::as_ref) {
                Some(thread) => {
                    if thread.sender.send(WorldMsg::Task(task)).is_err() {
                        log::warn!("Task for stopped world {} dropped", id.0);
                    }
                }
                None => log::warn!("Task for unknown world {} dropped", id.0),
            },
        }
    }

    /// A console command: `vanillite plugins` and `vanillite plugin reload <id>` here,
    /// everything else (also `vanillite reload config`) through the command tree on the
    /// overworld.
    fn run_console_command(&mut self, line: &str) {
        let command = line.trim().trim_start_matches('/');
        if let Some(id) = command.strip_prefix("vanillite plugin reload ") {
            match self.global.plugins.reload(id.trim()) {
                Ok(()) => {}
                Err(e) => log::warn!("Reloading plugin {} failed: {e}", id.trim()),
            }
            return;
        }
        if command == "vanillite plugins" {
            let plugins = self.global.plugins.plugins();
            log::info!("Plugins ({}): {}", plugins.len(), plugins.join(", "));
            return;
        }
        let global = Arc::clone(&self.global);
        let line = line.to_owned();
        self.run_in_world(
            WorldId(0),
            Box::new(move |w: &mut ServerWorld| {
                crate::commands::run_console_command(w, &global, &line);
            }),
        );
    }

    /// Reloads `vanillite.toml` and swaps the rules at this tick boundary (`/vanillite reload
    /// config`); `server.properties` is read at startup only, like Vanilla. Returns the keys
    /// that take effect now and those that need a restart, or the error that kept the old
    /// values.
    pub(crate) fn reload_config_report(&mut self) -> Result<ReloadReport, String> {
        let mut new = match Config::load(&self.dir) {
            Ok(config) => config,
            Err(e) => {
                log::error!("Reloading {VANILLITE_FILE} failed, keeping the old values: {e}");
                return Err(e.to_string());
            }
        };
        new.properties = self.config.properties.clone();
        let mut report = ReloadReport::default();
        changed_rules(&self.config.server, &new.server, None, &mut report);
        for dimension in self.global.dimensions.names() {
            changed_rules(
                &self.config.world_rules(dimension),
                &new.world_rules(dimension),
                Some(dimension),
                &mut report,
            );
        }
        for key in &report.restart {
            log::warn!("{key} changed; the new value takes effect after a restart");
        }
        self.watch.configure(
            new.server.watchdog.early_warning_after.get(),
            new.server.watchdog.warning_interval,
        );
        let dimensions = self.global.dimensions.clone();
        for (world, dimension, _) in dimensions.iter() {
            let rules = Arc::new(new.world_rules(dimension));
            let max_chained = new.properties.max_chained_neighbor_updates;
            self.run_in_world(world, Box::new(move |w| w.set_rules(rules, max_chained)));
        }
        self.config = new;
        log::info!("Reloaded {VANILLITE_FILE}");
        Ok(report)
    }

    /// The CPU and memory limits the server runs under.
    pub(crate) fn limits(&self) -> &crate::budget::Limits {
        self.budget.limits()
    }

    /// Runs `each` on every loaded world between its ticks (on its thread), then `then` with
    /// the results in world order once every world ran it. Worlds that unload first never
    /// answer; `then` does not run then.
    pub(crate) fn gather_worlds<T: Send + 'static>(
        &mut self,
        each: impl Fn(&mut ServerWorld) -> T + Send + Sync + 'static,
        then: impl FnOnce(Vec<T>) + Send + 'static,
    ) {
        type Pending<T, F> = (Vec<Option<T>>, usize, Option<F>);
        let worlds = self.loaded_worlds();
        if worlds.is_empty() {
            then(Vec::new());
            return;
        }
        let each = Arc::new(each);
        let state: Arc<Mutex<Pending<T, _>>> = Arc::new(Mutex::new((
            std::iter::repeat_with(|| None).take(worlds.len()).collect(),
            worlds.len(),
            Some(then),
        )));
        for (index, id) in worlds.into_iter().enumerate() {
            let each = Arc::clone(&each);
            let state = Arc::clone(&state);
            self.run_in_world(
                id,
                Box::new(move |w: &mut ServerWorld| {
                    let result = each(w);
                    let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(slot) = s.0.get_mut(index) {
                        *slot = Some(result);
                    }
                    s.1 = s.1.saturating_sub(1);
                    if s.1 == 0
                        && let Some(then) = s.2.take()
                    {
                        let results = std::mem::take(&mut s.0).into_iter().flatten().collect();
                        drop(s);
                        then(results);
                    }
                }),
            );
        }
    }

    /// A server tick of a running profile ended: its laps go to the profile; a finished
    /// profile collects the worlds' parts and reports.
    fn profile_tick(&mut self, laps: crate::profiler::ServerLaps) {
        let Some(finished) = self.global.diagnostics.profiler.server_tick(laps) else {
            return;
        };
        let global = Arc::clone(&self.global);
        let dir = self.dir.join("profiles");
        self.gather_worlds(
            |w| w.take_profile(),
            move |worlds| {
                let worlds: Vec<_> = worlds.into_iter().flatten().collect();
                crate::commands::profile_finished(&global, &dir, &finished, &worlds);
                global.diagnostics.profiler.collected();
            },
        );
    }

    /// Ticks until the next autosave (Vanilla's `computeNextAutosaveInterval`).
    fn autosave_interval(&self) -> i64 {
        match self.config.server.autosave.interval {
            VanillaOr::Value(ticks) => i64::from(ticks),
            VanillaOr::Vanilla => {
                let tick_rate = self.global.tick_rate();
                let rate = if tick_rate.is_sprinting() {
                    1e9 / (self.average_tick_nanos as f32 + 1.0)
                } else {
                    tick_rate.tick_rate()
                };
                ((rate * AUTOSAVE_SECONDS) as i64).max(100)
            }
        }
    }

    fn autosave(&mut self) {
        self.ticks_until_autosave = self.autosave_interval();
        log::debug!("Autosave started");
        self.save_everything(false, false, None);
    }

    /// The ids of the loaded worlds.
    fn loaded_worlds(&self) -> Vec<WorldId> {
        match &self.worlds {
            Worlds::Sequential(worlds) => worlds.iter().flatten().map(ServerWorld::id).collect(),
            Worlds::Parallel(threads) => threads
                .iter()
                .enumerate()
                .filter(|(_, t)| t.is_some())
                .filter_map(|(i, _)| u16::try_from(i).ok().map(WorldId))
                .collect(),
        }
    }

    /// Vanilla's `saveEverything`: the players, `level.dat`, the saved data and the chunks of
    /// every world; after `/save-off` the chunks only when `force`d. Each world saves between
    /// its ticks; files are written in the background. `then` runs once every world has saved
    /// (with `flush`: once everything is on disk).
    pub(crate) fn save_everything(&mut self, flush: bool, force: bool, then: Option<AfterSave>) {
        let skip_chunks = !force && !self.global.level.auto_save.load(Ordering::Relaxed);
        self.global.systems.save(&self.global.level.data);
        self.global.level.save();
        let loaded = self.loaded_worlds();
        let done = Arc::new(SaveDone {
            remaining: AtomicUsize::new(loaded.len()),
            writer: flush.then(|| self.global.level.writer().clone()),
            then: Mutex::new(then),
        });
        for id in loaded {
            let done = Arc::clone(&done);
            // `then` (backups) waits until the world's save round is committed.
            self.run_in_world(
                id,
                Box::new(move |w| {
                    w.save(flush, skip_chunks);
                    w.when_saved(Box::new(move || done.world_saved()));
                }),
            );
        }
    }

    /// When the backup after one starting now is due.
    fn backup_due_after(&self) -> Instant {
        let minutes = self.config.server.backup.interval.get().unwrap_or(0);
        Instant::now() + Duration::from_secs(u64::from(minutes) * 60)
    }

    /// Saves everything and archives the world on a background thread once the saves are
    /// written (`backup.*`).
    fn start_backup(&mut self) {
        if self.backup.as_ref().is_some_and(|b| !b.is_finished()) {
            log::warn!("Skipping a backup: the previous one is still running");
            return;
        }
        let rules = &self.config.server.backup;
        let settings = BackupSettings {
            directory: self.dir.join(&rules.directory),
            keep: rules.keep,
            compression_level: rules.compression_level,
        };
        let level_name = self.config.properties.level_name.clone();
        let mut sources: Vec<PathBuf> = Vec::new();
        for world in self.global.dimensions.worlds() {
            let dir = match self.global.dimensions.folder(world) {
                Some(folder) => self.dir.join(folder),
                None => self.layout.world_dir(
                    &self.dir,
                    &level_name,
                    self.global.dimensions.name_or_overworld(world),
                ),
            };
            if !sources.contains(&dir) {
                sources.push(dir);
            }
        }
        let (saved, wait) = mpsc::channel::<()>();
        self.save_everything(
            false,
            false,
            Some(Box::new(move || {
                let _ = saved.send(());
            })),
        );
        let writer = self.global.level.writer().clone();
        let thread = std::thread::Builder::new()
            .name("Backup".to_owned())
            .spawn(move || {
                // A crashed world drops the sender; the backup is made anyway.
                let _ = wait.recv();
                writer.flush();
                log::info!("Backing up level '{level_name}'");
                let started = Instant::now();
                match vanillite_storage::backup::create_backup(&level_name, &sources, &settings) {
                    Ok(path) => log::info!(
                        "Backup saved to {} ({:.1}s)",
                        path.display(),
                        started.elapsed().as_secs_f64()
                    ),
                    Err(e) => log::error!("Backup of level '{level_name}' failed: {e}"),
                }
            });
        match thread {
            Ok(thread) => self.backup = Some(thread),
            Err(e) => log::error!("Cannot start the backup thread: {e}"),
        }
    }

    /// Loads the extra world of `spec` from its folder; with `create` a missing world is created
    /// from `spec` (an existing one keeps its seed and generator). Blocks the server thread
    /// while the world opens, like Bukkit's `createWorld`.
    pub(crate) fn open_world(
        &mut self,
        spec: &crate::worlds::WorldSpec,
        create: bool,
    ) -> Result<WorldId, StartError> {
        let name = spec.name.as_str();
        let key = crate::worlds::dimension_key(name).map_err(StartError::Io)?;
        let level_name = &self.config.properties.level_name;
        let dims = &self.global.dimensions;
        let taken = dims
            .worlds()
            .filter(|&w| dims.folder(w).is_none())
            .filter_map(|w| dims.name(w))
            .map(|d| self.layout.world_dir(&self.dir, level_name, d))
            .chain(std::iter::once(self.dir.join(level_name)))
            .any(|d| {
                d.file_name()
                    .is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case(name))
            });
        if taken {
            return Err(StartError::Io(format!(
                "{name} is a folder of the default world"
            )));
        }
        let folder = self.dir.join(name);
        let packs = self.factory.packs.clone();
        let (kind, world_gen, files) = match crate::worlds::saved_world(&folder, &key)? {
            Some(saved) => (saved.kind, saved.world_gen, saved.files),
            None if !create => {
                return Err(StartError::Io(format!("there is no world in {name}/")));
            }
            None => {
                let used = fs::read_dir(&folder).is_ok_and(|mut d| d.next().is_some());
                if used && !folder.join("level.dat").exists() {
                    return Err(StartError::Io(format!("{name}/ exists and holds no world")));
                }
                fs::create_dir_all(&folder).map_err(|e| StartError::io(&folder, e))?;
                let generator = spec
                    .generator_json(packs.as_deref(), &self.world_gen)
                    .map_err(StartError::Io)?;
                let seed = spec.seed.unwrap_or_else(crate::world_setup::random_seed);
                let kind = spec.environment.kind();
                let world_gen = crate::world_setup::WorldGen::create_single(
                    &folder,
                    (&key, kind),
                    generator,
                    seed,
                    spec.generate_structures,
                )?;
                log::info!(
                    "New world {name}: {}, generator {}, seed {seed}",
                    spec.environment.name(),
                    spec.generator
                );
                (kind.to_owned(), world_gen, key.clone())
            }
        };
        let id = dims
            .register(&key, &kind, Some(name), packs.as_deref(), world_gen.seed)
            .map_err(StartError::Io)?;
        let lock = lock_world(&folder)?;
        self.factory
            .extra_gens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), world_gen.clone());
        let built = self.build_extra(id, (&key, &kind), &folder, &files, &world_gen);
        let world = match built {
            Ok(world) => world,
            Err(e) => {
                self.factory
                    .extra_gens
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&key);
                return Err(e);
            }
        };
        attach_world(
            &mut self.worlds,
            &self.routes,
            world,
            &self.compute,
            &self.global,
            &self.sender,
            &self.watch,
        )?;
        self.global.dimensions.set_loaded(id, true);
        self.extra_locks.insert(id, lock);
        log::info!("Loaded world {name} ({key})");
        let mut ev = vanillite_game::hooks::Event {
            key,
            text: name.to_owned(),
            ..vanillite_game::hooks::Event::new(vanillite_abi::event::WORLD_LOAD)
        };
        crate::plugins::Hooks::of_world(&self.global, id).event(&mut ev);
        Ok(id)
    }

    /// Builds the extra world `id` stored in `folder` (chunks in the folder of the dimension
    /// `files`), with its spawn.
    fn build_extra(
        &mut self,
        id: WorldId,
        (key, kind): (&str, &str),
        folder: &Path,
        files: &str,
        world_gen: &crate::world_setup::WorldGen,
    ) -> Result<ServerWorld, StartError> {
        let name = folder
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let packs = self.factory.packs.clone();
        let dir = vanillite_storage::dimension_dir(folder, files);
        let plan = crate::world_format::plan_in(
            &self.config,
            &self.dir,
            &name,
            std::slice::from_ref(&dir),
        )?;
        let format = if plan.convert_to.is_some() {
            let info = crate::world_setup::world_info(
                packs.as_deref(),
                dir.clone(),
                key,
                kind,
                world_gen.seed,
            )?;
            crate::world_format::convert_on_start(
                plan,
                &self.config,
                &self.dir,
                &name,
                &[info],
                self.compute.scheduler().workers(),
            )
        } else {
            plan.format
        };
        let upgrades = match format {
            WorldFormat::Vanillite => vanillite_storage::upgrade::Upgrades::load(folder)
                .map_err(|e| StartError::Io(format!("world \"{name}\": {e}")))?,
            WorldFormat::Anvil => vanillite_storage::upgrade::Upgrades::none(),
        };
        let mut level_data = vanillite_storage::level_dat::LevelData::read(folder)
            .map_err(|e| StartError::io(folder, e))?;
        let saved_spawn = level_data.as_ref().and_then(|l| l.spawn.clone());
        let spawn = saved_spawn
            .as_ref()
            .map_or((vanillite_base::Vec3::ZERO, 0.0), |s| {
                (
                    vanillite_base::Vec3::new(
                        f64::from(s.pos[0]) + 0.5,
                        f64::from(s.pos[1]),
                        f64::from(s.pos[2]) + 0.5,
                    ),
                    s.yaw,
                )
            });
        let levels = self
            .global
            .dimensions
            .names()
            .chain(std::iter::once(key))
            .filter_map(|d| vanillite_protocol::types::Identifier::new(d).ok())
            .collect();
        let level = crate::net::level_info(
            &self.config,
            packs.as_deref(),
            levels,
            (key, kind),
            world_gen.seed,
            world_gen.is_flat_of(key),
            spawn,
        );
        let players = crate::players::WorldPlayers::new(level, self.player_store.clone());
        let place = crate::worlds::WorldPlace {
            dir,
            seed: world_gen.seed,
            format,
            upgrades,
            world_gen,
        };
        let mut world =
            self.factory
                .build(&self.global, &self.config, id, (key, kind), &place, players)?;
        if saved_spawn.is_none() {
            let pos = world.search_initial_spawn(world_gen.flat_spawn_height_of(key));
            world.players.level.spawn = (
                vanillite_base::Vec3::new(
                    f64::from(pos.x) + 0.5,
                    f64::from(pos.y),
                    f64::from(pos.z) + 0.5,
                ),
                0.0,
            );
            let data = level_data
                .get_or_insert_with(|| vanillite_storage::level_dat::LevelData::new(&name));
            data.spawn = Some(vanillite_storage::level_dat::Spawn {
                dimension: key.to_owned(),
                pos: [pos.x, pos.y, pos.z],
                yaw: 0.0,
                pitch: 0.0,
            });
            data.add_brand("vanillite", true);
            data.write(folder).map_err(|e| StartError::io(folder, e))?;
        }
        prepare_levels(std::slice::from_mut(&mut world));
        Ok(world)
    }

    /// Unloads the extra world `id`, its chunks saved first with `save` (Bukkit's
    /// `unloadWorld`). Fails for the default world's dimensions, a world with players in it
    /// and when a plugin cancels it.
    pub(crate) fn unload_world(&mut self, id: WorldId, save: bool) -> Result<(), String> {
        let dims = &self.global.dimensions;
        let Some(name) = dims.folder(id).map(str::to_owned) else {
            return Err("only extra worlds can be unloaded".to_owned());
        };
        if !dims.is_loaded(id) {
            return Err(format!("world {name} is not loaded"));
        }
        if self.global.players.snapshot().iter().any(|p| p.world == id) {
            return Err(format!("players are in world {name}"));
        }
        let key = dims.name_or_overworld(id).to_owned();
        let mut ev = vanillite_game::hooks::Event {
            key: key.clone(),
            text: name.clone(),
            flags: u32::from(save),
            ..vanillite_game::hooks::Event::new(vanillite_abi::event::WORLD_UNLOAD)
        };
        crate::plugins::Hooks::of_world(&self.global, id).event(&mut ev);
        if ev.cancelled {
            return Err(format!("a plugin keeps world {name} loaded"));
        }
        dims.set_loaded(id, false);
        if let Some(route) = self
            .routes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id.index())
        {
            *route = None;
        }
        match &mut self.worlds {
            Worlds::Sequential(worlds) => {
                if let Some(mut world) = worlds.get_mut(id.index()).and_then(Option::take) {
                    world.save(true, !save);
                }
            }
            Worlds::Parallel(threads) => {
                if let Some(thread) = threads.get_mut(id.index()).and_then(Option::take) {
                    let _ = thread.sender.send(WorldMsg::Stop { save });
                    let _ = thread.handle.join();
                }
            }
        }
        self.global.level.writer().flush();
        self.factory
            .extra_gens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        self.factory
            .terrains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        self.extra_locks.remove(&id);
        log::info!("Unloaded world {name} ({key})");
        Ok(())
    }

    /// Vanilla's `stopServer`: save everything with flush, stop the threads, release the world.
    fn shutdown(mut self) {
        // Saving may take long; the watchdog must not kill the process now.
        self.watch.progress.store(0, Ordering::Relaxed);
        log::info!("Stopping server");
        self.global.plugins.disable_all();
        for player in self.global.players.snapshot() {
            player
                .sink
                .kick(vanillite_protocol::text::Component::translatable(
                    "multiplayer.disconnect.server_shutdown",
                    vec![],
                ));
        }
        log::info!("Saving players");
        log::info!("Saving worlds");
        let level = &self.config.properties.level_name;
        match self.worlds {
            Worlds::Sequential(mut worlds) => {
                for world in worlds.iter_mut().flatten() {
                    log::info!("Saving chunks for level '{level}'/{}", world.dimension());
                    world.save(true, false);
                }
            }
            Worlds::Parallel(threads) => {
                for (index, thread) in threads.iter().enumerate() {
                    let Some(thread) = thread else {
                        continue;
                    };
                    let dimension = u16::try_from(index)
                        .ok()
                        .and_then(|i| self.global.dimensions.name(WorldId(i)))
                        .unwrap_or("?");
                    log::info!("Saving chunks for level '{level}'/{dimension}");
                    let _ = thread.sender.send(WorldMsg::Stop { save: true });
                }
                for thread in threads.into_iter().flatten() {
                    let _ = thread.handle.join();
                }
            }
        }
        self.global.systems.save(&self.global.level.data);
        self.global.level.save();
        self.global.level.writer().flush();
        self.io.shutdown();
        log::info!("All dimensions are saved");
        if let Some(backup) = self.backup.take()
            && !backup.is_finished()
        {
            log::info!("Waiting for the running backup to finish");
            let _ = backup.join();
        }
        if let Some(net) = self.net.take() {
            net.shutdown();
        }
        self.compute.scheduler().shutdown();
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.stop();
        }
    }
}

/// What a config reload changed (`/vanillite reload config`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ReloadReport {
    /// Keys that take effect now (`world/key` for a world's rules).
    pub(crate) applied: Vec<String>,
    /// Keys that take effect after a restart.
    pub(crate) restart: Vec<String>,
}

/// Adds the rules that differ between `old` and `new` to `report`.
fn changed_rules<R: RuleSet>(old: &R, new: &R, world: Option<&str>, report: &mut ReloadReport) {
    for info in R::RULES {
        if old.get(info.key) == new.get(info.key) {
            continue;
        }
        let key = match world {
            Some(w) => format!("{w}/{}", info.key),
            None => info.key.to_owned(),
        };
        let list = if info.restart {
            &mut report.restart
        } else {
            &mut report.applied
        };
        if !list.contains(&key) {
            list.push(key);
        }
    }
}

/// The server could not start.
#[derive(Debug)]
pub enum StartError {
    /// A configuration file is missing or invalid.
    Config(ConfigError),
    /// A file or thread operation failed.
    Io(String),
    /// Another server holds the world's `session.lock`.
    WorldInUse(PathBuf),
    /// A slot was filled twice.
    Slot(SlotError),
    /// The EULA is not accepted in `eula.txt` (its full path).
    Eula(PathBuf),
}

impl StartError {
    pub(crate) fn io(path: &Path, error: io::Error) -> StartError {
        StartError::Io(format!("{}: {error}", path.display()))
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Config(e) => write!(f, "invalid configuration: {e}"),
            StartError::Io(e) => f.write_str(e),
            StartError::WorldInUse(path) => write!(
                f,
                "the world is in use by another server ({} is locked)",
                path.display()
            ),
            StartError::Slot(e) => e.fmt(f),
            // The first sentences are Vanilla's: panels such as Pterodactyl detect them.
            StartError::Eula(file) => write!(
                f,
                "You need to agree to the EULA in order to run the server. Go to eula.txt for \
                 more info.\n\
                 \x20 Read the Minecraft EULA: https://aka.ms/MinecraftEULA\n\
                 \x20 To accept it, open {} and change eula=false to eula=true,\n\
                 \x20 or set the environment variable VANILLITE_EULA=true. Then start the server \
                 again.",
                file.display()
            ),
        }
    }
}

impl std::error::Error for StartError {}

impl From<ConfigError> for StartError {
    fn from(e: ConfigError) -> Self {
        StartError::Config(e)
    }
}

impl From<SlotError> for StartError {
    fn from(e: SlotError) -> Self {
        StartError::Slot(e)
    }
}

/// Opens the tick log named by `VANILLITE_TICK_LOG` (relative to the working directory), if set.
fn open_tick_log() -> Option<io::BufWriter<File>> {
    let path = std::env::var_os("VANILLITE_TICK_LOG")?;
    match File::create(&path) {
        Ok(file) => Some(io::BufWriter::new(file)),
        Err(e) => {
            log::warn!(
                "Cannot create the tick log {}: {e}",
                Path::new(&path).display()
            );
            None
        }
    }
}

/// `MinecraftServer.setInitialSpawn` of a new world: the generator's origin chunk (the noise
/// spawn search), then the first chunk with a respawn column in a spiral of 11 × 11 chunks
/// around it. `flat_height` is the flat generator's layer height (`getSpawnHeight`).
fn initial_spawn(worlds: &mut [ServerWorld], global: &Global, flat_height: Option<i32>) {
    let Some(world) = worlds.first_mut() else {
        return;
    };
    let spawn = world.search_initial_spawn(flat_height);
    log::info!(
        "Spawn of the new world: {}, {}, {}",
        spawn.x,
        spawn.y,
        spawn.z
    );
    global.level.set_spawn(vanillite_storage::level_dat::Spawn {
        dimension: "minecraft:overworld".to_owned(),
        pos: [spawn.x, spawn.y, spawn.z],
        yaw: 0.0,
        pitch: 0.0,
    });
    let at = vanillite_base::Vec3::new(
        f64::from(spawn.x) + 0.5,
        f64::from(spawn.y),
        f64::from(spawn.z) + 0.5,
    );
    global.systems.set_spawn(at);
    for world in worlds.iter_mut() {
        world.players.level.spawn = (at, 0.0);
    }
}

/// `MinecraftServer.prepareLevels`: the chunks of the saved tickets (forced chunks, portal
/// tickets) become full before the first tick, so forced chunks tick from the first tick on.
fn prepare_levels(worlds: &mut [ServerWorld]) {
    let mut waiting: Vec<&mut ServerWorld> = worlds
        .iter_mut()
        .filter(|w| w.has_saved_tickets())
        .collect();
    if waiting.is_empty() {
        return;
    }
    let started = Instant::now();
    let mut next_report = started + Duration::from_secs(5);
    loop {
        let pending: usize = waiting.iter_mut().map(|w| w.prepare_step()).sum();
        if pending == 0 {
            break;
        }
        if Instant::now() >= next_report {
            log::info!("Loading the chunks of the saved chunk tickets: {pending} left");
            next_report += Duration::from_secs(5);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    log::info!(
        "Loaded the chunks of the saved chunk tickets in {} ms",
        started.elapsed().as_millis()
    );
}

/// What the parity harness of `vanillite-test` drives (feature `test-support`): the tick loop
/// without its clock, the worlds, and players without a network.
#[cfg(feature = "test-support")]
impl ServerBuilder {
    /// Parity harness mode: every chunk ticket loads its chunks before the tick goes on
    /// ([`ChunkSettings::blocking_tickets`]), like Vanilla's synchronous loads.
    pub fn harness(&mut self) -> &mut Self {
        self.harness = true;
        self
    }

    /// Wraps the provider that fills the `chunk-generator` slot (the built-in one unless a
    /// plugin fills it), e.g. to serve some dimensions from it and others from a test
    /// generator.
    pub fn wrap_chunk_generator(
        &mut self,
        wrap: impl FnOnce(Box<dyn ChunkGeneratorProvider>) -> Box<dyn ChunkGeneratorProvider>
        + Send
        + 'static,
    ) -> &mut Self {
        self.wrap_generator = Some(GeneratorWrap(Box::new(wrap)));
        self
    }
}

#[cfg(feature = "test-support")]
impl Server {
    /// One iteration of [`Server::run`] without the clock: the messages that arrived while it
    /// waited, the tick, then the work queued during the tick.
    pub fn step(&mut self) {
        self.wait_until_next_tick(0, true);
        self.tick_server();
        self.wait_until_next_tick(0, true);
    }

    /// A world (sequential dimensions only).
    ///
    /// # Panics
    ///
    /// If the dimensions tick in parallel or the world does not exist.
    pub fn world(&mut self, id: WorldId) -> &mut ServerWorld {
        match &mut self.worlds {
            Worlds::Sequential(worlds) => worlds[id.index()].as_mut().expect("world not loaded"),
            Worlds::Parallel(_) => panic!("the harness needs sequential dimensions"),
        }
    }

    /// A world for reading (sequential dimensions only).
    ///
    /// # Panics
    ///
    /// If the dimensions tick in parallel or the world does not exist.
    pub fn world_ref(&self, id: WorldId) -> &ServerWorld {
        match &self.worlds {
            Worlds::Sequential(worlds) => worlds[id.index()].as_ref().expect("world not loaded"),
            Worlds::Parallel(_) => panic!("the harness needs sequential dimensions"),
        }
    }

    /// Runs a console command now, as the global pre-phase runs one.
    pub fn command(&mut self, line: &str) {
        self.run_console_command(line);
    }

    /// Runs a command as the console at `pos` of the overworld now (parity scenarios).
    pub fn command_at(&mut self, line: &str, pos: vanillite_base::Vec3) {
        let global = Arc::clone(&self.global);
        let line = line.to_owned();
        self.run_in_world(
            WorldId(0),
            Box::new(move |w: &mut ServerWorld| {
                crate::commands::run_console_command_at(w, &global, &line, pos);
            }),
        );
    }

    /// Sets the game time of the server and every world (`setGameTime` of the overworld's
    /// data).
    pub fn set_game_time(&mut self, time: i64) {
        self.global
            .level
            .game_time
            .store(time, std::sync::atomic::Ordering::Relaxed);
        if let Worlds::Sequential(worlds) = &mut self.worlds {
            for world in worlds.iter_mut().flatten() {
                world.level_mut().set_game_time(time);
            }
        }
    }

    /// A survival player without saved data joins like [`Server::accept`] admits one, on a
    /// connection nobody reads. Its packets go through the returned sender.
    pub fn join_without_network(
        &mut self,
        name: &str,
        uuid: u128,
    ) -> vanillite_protocol::InboundSender {
        self.join_unread(name, uuid, None)
    }

    /// [`Server::join_without_network`] with the packets the player gets going to `sink`
    /// (what its client would receive).
    pub fn join_into_sink(
        &mut self,
        name: &str,
        uuid: u128,
        sink: Arc<dyn vanillite_game::player::sink::PacketSink>,
    ) -> vanillite_protocol::InboundSender {
        self.join_unread(name, uuid, Some(sink))
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Server {
    /// [`Server::join_without_network`].
    fn join_unread(
        &mut self,
        name: &str,
        uuid: u128,
        sink: Option<Arc<dyn vanillite_game::player::sink::PacketSink>>,
    ) -> vanillite_protocol::InboundSender {
        /// A connection nobody reads.
        struct Unread;
        impl vanillite_game::player::sink::PacketSink for Unread {
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
        let (tx, inbound) = vanillite_protocol::inbound_queue(64);
        let arrival = crate::players::Arrival {
            uuid,
            profile: vanillite_game::player::GameProfile {
                name: name.to_owned(),
                properties: Vec::new(),
            },
            client: vanillite_game::player::ClientInformation::default(),
            game_mode: vanillite_config::GameMode::Survival,
            sink: sink.unwrap_or_else(|| Arc::new(Unread)),
            inbound,
            memory_connection: true,
            virtual_connection: true,
            saved: None,
            stats: None,
            advancements: None,
            address: "harness".to_owned(),
            inbox: Arc::new(Mutex::new(
                vanillite_game::player::chat::ChatInbox::default(),
            )),
            transferred: false,
            details: None,
        };
        self.place_new_player(arrival, "127.0.0.1".to_owned());
        tx
    }
}

#[cfg(test)]
#[path = "server/extra_worlds_tests.rs"]
mod extra_worlds_tests;

#[cfg(test)]
#[path = "server/vanillite_cmd_tests.rs"]
mod vanillite_cmd_tests;
