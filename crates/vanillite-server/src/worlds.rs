//! Building worlds: [`WorldFactory`] makes a [`ServerWorld`] from the slots filled at startup,
//! for the dimensions of the default world and for extra worlds.
//!
//! An extra world (`[extra-worlds.<name>]`, a plugin's `world_create`) is one dimension with
//! its own folder `<name>/` like a Bukkit world: its own `level.dat`, `session.lock`,
//! `data/minecraft/world_gen_settings.dat` (seed and generator) and chunks in
//! `<name>/dimensions/minecraft/<name>/`, in the world format it was found in (new ones in
//! `storage.world-format`). Its dimension key is `minecraft:<name in lower case>` and its
//! dimension type that of its [`Environment`]. Time, weather, game rules, difficulty and the
//! player data are those of the server, as in Vanilla for all dimensions.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vanillite_chunk::slots::{
    ChunkGeneratorProvider, ChunkStoreProvider, DimensionId, LightEngineProvider,
};
use vanillite_config::Config;
use vanillite_datapack::LoadedPacks;
use vanillite_game::level::Level;
use vanillite_storage::saved_data::SavedDataStore;
use vanillite_storage::upgrade::Upgrades;
use vanillite_storage::writer::FileWriter;
use vanillite_storage::{
    AnvilCodec, Compression, IoPool, KeepUnreadable, OwnCodec, RegionStoreProvider, StoreSettings,
    WorldFormat,
};
use vanillite_world::chunks::{ChunkSettings, ChunkSlots, Chunks, Executor, SaveSettings};
use vanillite_world::tracking::{EntityTrackerProvider, TrackerSettings};

use crate::dimensions::Environment;
use crate::global::Global;
use crate::players::WorldPlayers;
use crate::server::StartError;
use crate::world::{ServerWorld, WorldId};
use crate::world_setup::WorldGen;

/// `performance.chunks.max-jobs` at most on scarce hardware ([`crate::budget::Budget::scarce`]).
const SCARCE_MAX_JOBS: usize = 16;

/// The dimension key of the extra world `name`: `minecraft:<name in lower case>`. Names are
/// folder names of letters, digits, `_`, `-` and `.`.
pub(crate) fn dimension_key(name: &str) -> Result<String, String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !valid {
        return Err(format!(
            "invalid world name {name:?}: use letters, digits, _, - and . (at most 64)"
        ));
    }
    Ok(format!("minecraft:{}", name.to_ascii_lowercase()))
}

/// What a new extra world is made of (Bukkit's `WorldCreator`).
#[derive(Clone, Debug)]
pub(crate) struct WorldSpec {
    /// The folder and name.
    pub(crate) name: String,
    pub(crate) environment: Environment,
    /// `normal`, `flat`, `void`, `amplified`, `large_biomes` or a dimension key whose
    /// generator it takes.
    pub(crate) generator: String,
    /// `None`: a random one.
    pub(crate) seed: Option<i64>,
    pub(crate) generate_structures: bool,
}

impl WorldSpec {
    /// A world of `[extra-worlds.<name>]`.
    pub(crate) fn from_config(world: &vanillite_config::ExtraWorld) -> Result<Self, String> {
        Ok(WorldSpec {
            name: world.name.clone(),
            environment: Environment::parse(&world.environment)
                .ok_or_else(|| format!("unknown environment {:?}", world.environment))?,
            generator: world.generator.clone(),
            seed: crate::world_setup::parse_seed(&world.seed),
            generate_structures: world.generate_structures,
        })
    }

    /// The generator JSON of the new world (`None`: the classic flat layers, only without
    /// data packs). `saved` has the dimensions of the default world.
    pub(crate) fn generator_json(
        &self,
        packs: Option<&LoadedPacks>,
        saved: &WorldGen,
    ) -> Result<Option<serde_json::Value>, String> {
        let kind = self.environment.kind();
        let preset = |preset: &str| -> Option<serde_json::Value> {
            let entry = packs?
                .registry("minecraft:worldgen/world_preset")?
                .get(preset)?;
            let json: serde_json::Value = serde_json::from_str(entry.json.get()).ok()?;
            json.get("dimensions")?.get(kind)?.get("generator").cloned()
        };
        let generator = match self.generator.as_str() {
            "normal" | "" => preset("minecraft:normal"),
            "amplified" => preset("minecraft:amplified"),
            "large_biomes" => preset("minecraft:large_biomes"),
            "flat" => {
                let flat = packs
                    .and_then(|p| p.registry("minecraft:worldgen/world_preset"))
                    .and_then(|r| r.get("minecraft:flat"))
                    .and_then(|e| serde_json::from_str::<serde_json::Value>(e.json.get()).ok())
                    .and_then(|j| {
                        j.pointer("/dimensions/minecraft:overworld/generator")
                            .cloned()
                    });
                Some(flat.unwrap_or_else(|| serde_json::json!({ "type": "minecraft:flat" })))
            }
            "void" => Some(serde_json::json!({
                "type": "minecraft:flat",
                "settings": {
                    "layers": [],
                    "biome": "minecraft:the_void",
                    "lakes": false,
                    "features": false,
                    "structure_overrides": [],
                },
            })),
            key if key.contains(':') => Some(
                saved
                    .generator_json(key)
                    .ok_or_else(|| format!("no dimension {key} to take the generator from"))?,
            ),
            other => return Err(format!("unknown generator {other:?}")),
        };
        if packs.is_some() && generator.is_none() {
            return Err(format!(
                "generator {} has no {} dimension",
                self.generator,
                self.environment.name()
            ));
        }
        Ok(generator)
    }
}

/// Where a world's files are and what it was generated with.
pub(crate) struct WorldPlace<'a> {
    /// The dimension folder (`region/`, `entities/`, `poi/`, `data/`).
    pub(crate) dir: PathBuf,
    pub(crate) seed: i64,
    pub(crate) format: WorldFormat,
    pub(crate) upgrades: Arc<Upgrades>,
    /// Its generator settings.
    pub(crate) world_gen: &'a WorldGen,
}

/// The slots and shared parts every world is built from, kept for worlds that load later.
pub(crate) struct WorldFactory {
    pub(crate) packs: Option<Arc<LoadedPacks>>,
    pub(crate) tracker: Box<dyn EntityTrackerProvider>,
    pub(crate) storage: Box<dyn ChunkStoreProvider>,
    /// The built-in storage's IO threads and settings when no plugin or embedder filled the
    /// slot: extra worlds in another format get a store of their own format.
    pub(crate) built_in_storage: Option<(IoPool, StoreSettings)>,
    pub(crate) generator: Box<dyn ChunkGeneratorProvider>,
    pub(crate) light: Box<dyn LightEngineProvider>,
    pub(crate) executor: Arc<dyn Executor>,
    pub(crate) compression: Compression,
    pub(crate) own_level: i32,
    pub(crate) save: SaveSettings,
    pub(crate) scarce: bool,
    pub(crate) harness: bool,
    pub(crate) behaviours: Arc<vanillite_game::block::Behaviours>,
    pub(crate) terrains: crate::world_setup::GeneratorsByDimension,
    pub(crate) extra_gens: crate::world_setup::ExtraWorldGens,
    pub(crate) writer: FileWriter,
    pub(crate) random_sequences:
        Arc<std::sync::Mutex<vanillite_game::random_sequences::RandomSequences>>,
    pub(crate) spawn_table: Option<Arc<vanillite_game::mob::spawn::SpawnTable>>,
    /// The tree features of the default world's seed.
    pub(crate) trees: Option<Arc<vanillite_game::block::tree::TreeFeatures>>,
    pub(crate) seed: i64,
    /// The format of the default world.
    pub(crate) format: WorldFormat,
}

impl WorldFactory {
    /// Builds the world `id` of `(dimension, dimension type)` stored at `place`.
    pub(crate) fn build(
        &self,
        global: &Arc<Global>,
        config: &Config,
        id: WorldId,
        (dimension, kind): (&str, &str),
        place: &WorldPlace<'_>,
        players: WorldPlayers,
    ) -> Result<ServerWorld, StartError> {
        let packs = self.packs.as_deref();
        let seed = place.seed;
        let rules = Arc::new(config.world_rules(dimension));
        let view_distance = rules
            .distances
            .view_distance
            .or(u8::try_from(config.properties.view_distance.clamp(2, 32)).unwrap_or(10));
        let tracker_settings = TrackerSettings::new(
            i32::from(view_distance),
            config.properties.entity_broadcast_range_percentage,
            &rules.tracking,
        );
        let tracker = self.tracker.create(&tracker_settings);
        let player_range = tracker_settings.range(vanillite_data::entity::ids::PLAYER);
        let info = crate::world_setup::world_info(packs, place.dir.clone(), dimension, kind, seed)?;
        let index = id.0;
        let store = match &self.built_in_storage {
            Some((pool, settings)) if place.format != self.format => RegionStoreProvider {
                format: place.format,
                pool: pool.clone(),
                settings: *settings,
            }
            .create(&info, DimensionId(index)),
            _ => self.storage.create(&info, DimensionId(index)),
        };
        let slots = ChunkSlots {
            store: store.map_err(|e| StartError::io(&info.dir, e))?,
            // Records that fail to decode are copied aside before the chunk is generated
            // again and saved over them (#142).
            codec: match place.format {
                WorldFormat::Anvil => Arc::new(KeepUnreadable::new(
                    AnvilCodec::new(&info, self.compression),
                    info.dir.join("unreadable"),
                )),
                WorldFormat::Vanillite => Arc::new(KeepUnreadable::new(
                    OwnCodec::new(&info, self.own_level).with_upgrades(Arc::clone(&place.upgrades)),
                    info.dir.join("unreadable"),
                )),
            },
            generator: self
                .generator
                .create(&info)
                .map_err(|e| StartError::Io(format!("chunk generator of {dimension}: {e}")))?,
            light: self.light.create(&info),
            executor: Arc::clone(&self.executor),
        };
        let generator = Arc::clone(&slots.generator);
        slots
            .executor
            .spawn(0, Box::new(move || generator.prepare()));
        let settings = ChunkSettings {
            view_distance: i32::from(view_distance),
            unload_delay: u64::from(rules.chunks.unload_delay),
            // On scarce hardware fewer jobs wait in the queue, so the newest priorities
            // (the chunks around the players now) come first.
            max_jobs: if self.scarce {
                usize::from(rules.chunks.max_jobs).min(SCARCE_MAX_JOBS)
            } else {
                usize::from(rules.chunks.max_jobs)
            },
            simulation_distance: i32::from(rules.distances.simulation_distance.or(
                u8::try_from(config.properties.simulation_distance.clamp(2, 32)).unwrap_or(10),
            )),
            max_relights: usize::try_from(rules.simulation.max_relights_per_tick)
                .unwrap_or(usize::MAX),
            save: SaveSettings {
                dimension: DimensionId(index),
                ..self.save
            },
            blocking_tickets: self.harness,
        };
        let level_settings =
            crate::world_setup::level_settings(packs, &info, kind, &rules, &config.properties);
        let climate = crate::world_setup::climate(packs, place.world_gen, dimension);
        let bounds = (info.min_y, info.height);
        let data = SavedDataStore::new(info.dir.clone(), self.writer.clone());
        let mut level = Level::new(
            Chunks::new(info, settings, slots),
            level_settings,
            Arc::clone(&self.behaviours),
            Arc::clone(&global.network_ids),
            seed,
        );
        level.set_game_time(global.level.game_time());
        level.generation = self
            .terrains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(dimension)
            .cloned();
        level.border = crate::border::load(
            &data,
            global.level.game_time(),
            config.properties.max_world_size,
        );
        level.set_random_sequences(Arc::clone(&self.random_sequences));
        let trees = if seed == self.seed {
            self.trees.clone()
        } else {
            crate::world_setup::tree_features(packs, seed)
        };
        if let Some(trees) = &trees {
            level.set_tree_features(Arc::clone(trees));
        }
        level.set_maps(Arc::clone(&global.level.maps));
        let block_commands = crate::commands::BlockCommands::new(global, id);
        let lent_levels = block_commands.lent();
        level.set_command_runner(Arc::new(block_commands));
        if !global.plugins.plugins().is_empty() {
            level.set_hooks(Arc::new(crate::plugins::WorldHooks::new(global, id)));
        }
        level.set_structure_templates(Arc::clone(&global.templates));
        if let Some(packs) = &self.packs {
            level.set_data_packs(Arc::clone(packs));
        }
        let survival = crate::survival::WorldSurvival::new(
            dimension,
            &global.level,
            bounds,
            global
                .dimensions
                .facts(id)
                .map_or(i32::try_from(bounds.1).unwrap_or(384), |f| f.logical_height),
            player_range,
            climate,
        );
        let saved_tickets = crate::chunk_tickets::load(&data, &mut level);
        match data.read(vanillite_game::level::structure_refs::SAVED_DATA) {
            Ok(Some(tag)) => level.set_structure_refs(
                vanillite_game::level::structure_refs::StructureReferences::load(&tag),
            ),
            Ok(None) => {}
            Err(e) => log::error!(
                "Cannot read {}: {e}",
                vanillite_game::level::structure_refs::SAVED_DATA
            ),
        }
        let mobs = crate::mobs::WorldMobs::new(self.spawn_table.clone(), seed, &rules);
        let mut world = ServerWorld::new(
            id,
            dimension.to_owned(),
            rules,
            tracker,
            players,
            level,
            survival,
            mobs,
            data,
            global
                .dimensions
                .facts(id)
                .is_some_and(|t| t.has_ender_dragon_fight),
        );
        world.set_lent_levels(lent_levels);
        world.restore_tickets(saved_tickets);
        Ok(world)
    }
}

/// An extra world found in its folder.
pub(crate) struct SavedWorld {
    /// Its dimension type.
    pub(crate) kind: String,
    /// Its seed and generator, under the world's key.
    pub(crate) world_gen: WorldGen,
    /// The dimension whose folder holds its chunks: the world's key, or Vanilla's key in a
    /// world another server created.
    pub(crate) files: String,
}

/// The extra world with the key `key` stored in `folder`, if it was created before: its own
/// dimension, else the first of Vanilla's dimensions saved there (a world a Bukkit server
/// created keeps Vanilla's keys).
pub(crate) fn saved_world(folder: &Path, key: &str) -> Result<Option<SavedWorld>, StartError> {
    let Some(saved) = WorldGen::load(folder)? else {
        return Ok(None);
    };
    let files = std::iter::once(key)
        .chain(crate::server::VANILLA_DIMENSIONS)
        .find(|d| saved.kind_of(d).is_some());
    let Some(files) = files else {
        return Ok(None);
    };
    let kind = saved.kind_of(files).unwrap_or_default().to_owned();
    Ok(Some(SavedWorld {
        kind,
        world_gen: saved.renamed(files, key),
        files: files.to_owned(),
    }))
}
