//! The server's data packs (`MinecraftServer.packRepository`): which packs are selected at
//! start (`configurePackRepository`) and the asynchronous `reloadResources` of `/reload` and
//! `/datapack`: packs are opened and loaded on a worker thread, and the results swapped in at
//! the next global pre-phase. Dynamic registries keep what was loaded at start, like Vanilla.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use vanillite_datapack::repository::{self, DataPackConfig, Pack, PackRepository};
use vanillite_datapack::{LoadedPacks, OpenedPack};
use vanillite_protocol::packets::common::RegistryTags;
use vanillite_protocol::text::Component;
use vanillite_storage::level_dat::LevelData;

use crate::commands::CommandSource;
use crate::commands::functions::{self, FunctionLibrary};
use crate::global::Global;
use crate::systems::RegistryTags as TagMap;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The pack repository and what reloads need.
#[derive(Debug)]
pub(crate) struct DataPacks {
    repository: Mutex<PackRepository>,
    /// `WorldData.enabledFeatures`: fixed when the server starts.
    pub(crate) features: Vec<String>,
    /// Where zip packs are extracted.
    cache: PathBuf,
    /// What was loaded at start; reloads keep its registries. `None` without the Vanilla pack.
    loaded: Option<Arc<LoadedPacks>>,
    /// Lets one reload load at a time, so they are applied in order.
    reloading: Mutex<()>,
    /// Counts reloads, so a failed one restores the selection only if no other followed.
    generation: AtomicU64,
    /// Reloads load on the calling thread (the parity harness, whose ticks must not depend on
    /// how fast a worker loads); they are still applied at the next global pre-phase.
    synchronous: bool,
}

/// The packs selected at start.
pub(crate) struct Selected {
    pub(crate) packs: DataPacks,
    /// `level.dat`'s new `DataPacks`.
    pub(crate) config: DataPackConfig,
    /// The selected packs, opened, in load order.
    pub(crate) opened: Vec<OpenedPack>,
}

/// Selects the packs at start (`Main.loadOrCreateConfig` and
/// `MinecraftServer.configurePackRepository`): `level.dat`'s `DataPacks` and
/// `enabled_features`, or `initial-enabled-packs`/`initial-disabled-packs` for a new world.
pub(crate) fn select(
    vanilla: Option<PathBuf>,
    level_dir: &Path,
    cache: &Path,
    initial: (&str, &str),
) -> Selected {
    let defaults = || -> Vec<String> {
        repository::DEFAULT_FEATURES
            .iter()
            .map(|f| (*f).to_owned())
            .collect()
    };
    let (config, features, init) = match LevelData::read(level_dir).ok().flatten() {
        Some(level) => {
            let config = level
                .data_packs()
                .map_or_else(DataPackConfig::vanilla, |(enabled, disabled)| {
                    DataPackConfig { enabled, disabled }
                });
            let features = level.enabled_features().map_or_else(defaults, |f| {
                f.into_iter()
                    .filter(|f| repository::FEATURE_FLAGS.contains(&f.as_str()))
                    .collect()
            });
            (config, features, false)
        }
        None => {
            // `Splitter.on(',').trimResults()`.
            let split = |s: &str| s.split(',').map(|p| p.trim().to_owned()).collect();
            let config = DataPackConfig {
                enabled: split(initial.0),
                disabled: split(initial.1),
            };
            (config, defaults(), true)
        }
    };
    let mut repo = PackRepository::new(vanilla, level_dir.join("datapacks"));
    let (config, features) = repository::configure(&mut repo, &config, &features, init);
    let cache = cache.join("datapacks");
    let opened = open(&repo.packs(repo.selected()), &cache);
    Selected {
        packs: DataPacks {
            repository: Mutex::new(repo),
            features,
            cache,
            loaded: None,
            reloading: Mutex::new(()),
            generation: AtomicU64::new(0),
            synchronous: false,
        },
        config,
        opened,
    }
}

/// Opens `packs`; a pack that cannot be opened is left out (logged).
fn open(packs: &[Pack], cache: &Path) -> Vec<OpenedPack> {
    packs
        .iter()
        .filter_map(|pack| match pack.open(cache) {
            Ok(opened) => Some(opened),
            Err(e) => {
                log::error!("Failed to open data pack {}: {e}", pack.id);
                None
            }
        })
        .collect()
}

/// The tags of all registries of `packs` (`GameSystems::set_tags`).
pub(crate) fn registry_tags(packs: &LoadedPacks) -> TagMap {
    packs
        .tagged_registries()
        .filter_map(|r| Some((r.to_owned(), packs.tags(r)?.clone())))
        .collect()
}

/// What a reload loaded, ready to be swapped in.
struct Reloaded {
    /// The pack directories of the structure templates.
    template_roots: Vec<std::path::PathBuf>,
    tags: Option<(TagMap, Vec<RegistryTags>)>,
    functions: FunctionLibrary,
    /// Loot tables, predicates, item modifiers, recipes and the item and block data read
    /// from tags.
    game: Option<vanillite_game::player::play::GameData>,
    advancements: vanillite_game::advancements::Advancements,
}

impl DataPacks {
    /// The packs as loaded at start; `synchronous` reloads load on the calling thread.
    pub(crate) fn with_loaded(
        self,
        loaded: Option<Arc<LoadedPacks>>,
        synchronous: bool,
    ) -> DataPacks {
        DataPacks {
            loaded,
            synchronous,
            ..self
        }
    }

    /// The packs as loaded at start (`None` without the Vanilla pack).
    pub(crate) fn loaded(&self) -> Option<&Arc<LoadedPacks>> {
        self.loaded.as_ref()
    }

    /// The repository, locked for one command.
    pub(crate) fn repository(&self) -> MutexGuard<'_, PackRepository> {
        lock(&self.repository)
    }

    /// Sets the selection and `level.dat`'s `DataPacks`; returns the previous selection.
    fn select(&self, global: &Global, ids: &[String]) -> Vec<String> {
        let (previous, config) = {
            let mut repo = self.repository();
            let previous = repo.selected().to_vec();
            repo.set_selected(ids);
            (previous, repo.config(true))
        };
        global.level.set_data_configuration(&config, None);
        previous
    }

    /// Loads `packs` (off the tick thread): opens them, loads the tags of the existing
    /// registries and the functions.
    fn load(&self, global: &Global, packs: &[Pack]) -> Result<Reloaded, String> {
        let opened = open(packs, &self.cache);
        let (tags, game) = match &self.loaded {
            Some(loaded) => {
                let reloaded = loaded.reload(&opened).map_err(|e| e.to_string())?;
                for warning in &reloaded.warnings {
                    log::warn!("{warning}");
                }
                let game = crate::blocks::game_data(Some(&reloaded));
                (
                    Some((
                        registry_tags(&reloaded),
                        vanillite_net::network_tags(&reloaded),
                    )),
                    Some(game),
                )
            }
            None => (None, None),
        };
        let functions = functions::load(global, &opened);
        let advancements = crate::commands::load_advancements(&opened);
        Ok(Reloaded {
            template_roots: opened
                .iter()
                .flat_map(|p| p.layers.iter().cloned())
                .collect(),
            tags,
            functions,
            game,
            advancements,
        })
    }

    /// The end of `reloadResources` on the server thread: the new tags and functions
    /// (`#minecraft:load` runs next tick); players and joining clients get the tags.
    fn apply(&self, global: &Global, reloaded: Reloaded, network: Option<&vanillite_net::Network>) {
        if let Some((tags, network_tags)) = reloaded.tags {
            global.systems.set_tags(tags);
            // `PlayerList.reloadResources`.
            global.players.broadcast(
                &vanillite_protocol::packets::play::clientbound::UpdateTags {
                    registries: network_tags.clone(),
                },
                None,
            );
            if let Some(network) = network {
                network.set_tags(
                    &vanillite_protocol::packets::configuration::clientbound::UpdateTags {
                        registries: network_tags,
                    },
                );
            }
        }
        global.systems.set_functions(reloaded.functions);
        // `StructureTemplateManager.onResourceManagerReload`.
        global.templates.reload(reloaded.template_roots);
        if let Some(game) = reloaded.game {
            // `PlayerList.reloadResources`: the recipes go to every player.
            let packet = vanillite_protocol::SharedPacket::of(&game.recipes.update_packet());
            global.set_game(Arc::new(game));
            *global
                .recipes_packet
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::clone(&packet);
            global.players.broadcast_shared(&packet, None);
        }
        // `ServerAdvancementManager` reload; `PlayerAdvancements.reload` per online player.
        global.systems.set_advancements(reloaded.advancements);
        crate::progress::reload_advancements(global);
    }
}

/// `ReloadCommand.reloadPacks` (`MinecraftServer.reloadResources`): selects `ids` at once (so
/// the next commands see the selection, as after Vanilla's blocking reload), loads them on a
/// worker thread and swaps the results in at the next global pre-phase. A failure restores
/// the previous selection, unless another reload followed, and is reported to `source` with
/// `commands.reload.failure`.
pub(crate) fn reload(global: &Global, ids: Vec<String>, source: CommandSource) {
    let Some(global) = global.arc() else {
        return;
    };
    let packs = global.packs.repository().packs(&ids);
    let previous = global.packs.select(&global, &ids);
    let generation = global
        .packs
        .generation
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    let failed = move |global: &Global, source: &CommandSource, e: &dyn std::fmt::Display| {
        log::warn!("Failed to execute reload: {e}");
        if global.packs.generation.load(Ordering::Acquire) == generation {
            global.packs.select(global, &previous);
        }
        source.send_failure(
            global,
            Component::translatable("commands.reload.failure", vec![]),
        );
    };
    let worker = Arc::clone(&global);
    let worker_source = source.clone();
    let worker_failed = failed.clone();
    let work = move || {
        let global = worker;
        // Reloads load one at a time, so their results are applied in order.
        let _serial = lock(&global.packs.reloading);
        match global.packs.load(&global, &packs) {
            Ok(reloaded) => {
                if let Some(handle) = global.handle.get() {
                    let applied = Arc::clone(&global);
                    handle.run_global(move |server| {
                        applied.packs.apply(&applied, reloaded, server.network());
                    });
                }
            }
            Err(e) => worker_failed(&global, &worker_source, &e),
        }
    };
    if global.packs.synchronous {
        work();
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("Reload".to_owned())
        .spawn(work);
    if let Err(e) = spawned {
        failed(&global, &source, &e);
    }
}
