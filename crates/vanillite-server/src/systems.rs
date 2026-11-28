//! Server-wide game systems around the simulation: the scoreboard, command storage, data pack
//! functions and advancements, scheduled functions, random sequences, stopwatches and the
//! profilers. Each sits behind its own lock, held only for one command's change. The
//! scoreboard, the command storage and the scheduled functions are the world's saved data.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use vanillite_base::Vec3;
use vanillite_game::advancements::Advancements;
use vanillite_game::scoreboard::Scoreboard;
use vanillite_nbt::{Compound, Tag};
use vanillite_storage::saved_data::SavedDataStore;

use crate::command_state::{Profiling, Stopwatches, TimerQueue, WaypointIcon};
use crate::commands::functions::FunctionLibrary;
use vanillite_game::level::{RandomSequences, SharedRandomSequences};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The saved data of the scoreboard (`ScoreboardSaveData.TYPE`).
const SCOREBOARD: &str = "minecraft:scoreboard";
/// The saved data of the scheduled functions (`TimerQueue.TYPE`).
const SCHEDULED_EVENTS: &str = "minecraft:scheduled_events";
/// The saved data of the stopwatches (`Stopwatches.TYPE`).
const STOPWATCHES: &str = "minecraft:stopwatches";
/// The path of a namespace's command storage (`CommandStorage.Container.type`).
const COMMAND_STORAGE: &str = "command_storage";

/// `CommandStorage`: compounds by id, saved per namespace as `<namespace>:command_storage`.
#[derive(Debug, Default)]
pub(crate) struct CommandStorage {
    entries: BTreeMap<String, Compound>,
    /// Namespaces changed since the last save.
    dirty: BTreeSet<String>,
}

fn split_id(id: &str) -> (&str, &str) {
    id.split_once(':').unwrap_or(("minecraft", id))
}

impl CommandStorage {
    /// `get`: the compound of an id, if any.
    pub(crate) fn get(&self, id: &str) -> Option<&Compound> {
        self.entries.get(id)
    }

    /// `set`: an empty compound removes the id.
    pub(crate) fn set(&mut self, id: String, data: Compound) {
        self.dirty.insert(split_id(&id).0.to_owned());
        if data.is_empty() {
            self.entries.remove(&id);
        } else {
            self.entries.insert(id, data);
        }
    }

    /// `keys`.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    /// Every `<namespace>:command_storage` below `data/` of the world.
    fn load(data: &SavedDataStore) -> CommandStorage {
        let mut storage = CommandStorage::default();
        let Ok(dirs) = std::fs::read_dir(data.dir().join("data")) else {
            return storage;
        };
        for dir in dirs.flatten() {
            let Ok(namespace) = dir.file_name().into_string() else {
                continue;
            };
            let id = format!("{namespace}:{COMMAND_STORAGE}");
            let contents = match data.read(&id) {
                Ok(Some(mut c)) => match c.remove("contents") {
                    Some(Tag::Compound(contents)) => contents,
                    _ => continue,
                },
                Ok(None) => continue,
                Err(e) => {
                    log::error!("Cannot read {id}: {e}");
                    continue;
                }
            };
            for (path, tag) in contents.iter() {
                if let Tag::Compound(c) = tag {
                    storage
                        .entries
                        .insert(format!("{namespace}:{path}"), c.clone());
                }
            }
        }
        storage
    }

    /// The changed namespaces' `Container.CODEC` data, as saved data ids.
    fn take_dirty(&mut self) -> Vec<(String, Compound)> {
        std::mem::take(&mut self.dirty)
            .into_iter()
            .map(|namespace| {
                let mut contents = Compound::new();
                for (id, data) in &self.entries {
                    let (ns, path) = split_id(id);
                    if ns == namespace {
                        contents.insert(path, data.clone());
                    }
                }
                let data = Compound::new().with("contents", contents.in_java_hash_order());
                (format!("{namespace}:{COMMAND_STORAGE}"), data)
            })
            .collect()
    }
}

/// The game systems of the server.
pub(crate) struct GameSystems {
    /// The mob spawn table of the data packs, for `/summon`'s biome variants.
    pub(crate) mob_spawns: std::sync::OnceLock<Arc<vanillite_game::mob::spawn::SpawnTable>>,
    scoreboard: Mutex<Scoreboard>,
    /// What the clients know of the scoreboard; locked before the scoreboard.
    scoreboard_sync: Mutex<crate::scoreboard::ScoreboardSync>,
    storage: Mutex<CommandStorage>,
    functions: RwLock<Arc<FunctionLibrary>>,
    /// `None` until first needed ([`GameSystems::advancements`]) and again after
    /// [`GameSystems::release_advancements`].
    advancements: RwLock<Option<Arc<Advancements>>>,
    /// The pack directories the advancements are read from.
    advancement_roots: RwLock<Vec<PathBuf>>,
    /// Held while the advancements are read, so they are read once.
    advancements_loading: Mutex<()>,
    post_reload: AtomicBool,
    /// `function-permission-level`.
    pub(crate) function_level: u8,
    /// The world directory.
    pub(crate) level_dir: PathBuf,
    spawn: Mutex<Vec3>,
    /// Tags of the static and dynamic registries: registry → tag → entries.
    tags: RwLock<Arc<RegistryTags>>,
    scheduled: Mutex<TimerQueue>,
    /// The server's random sequences, shared with every world's loot.
    random_sequences: SharedRandomSequences,
    stopwatches: Mutex<Stopwatches>,
    profiling: Mutex<Profiling>,
    waypoints: Mutex<std::collections::BTreeMap<u128, WaypointIcon>>,
    bossbars: Mutex<crate::bossbars::CustomBossEvents>,
    game_tests: Mutex<crate::commands::gametest::GameTests>,
}

/// Registry tags: registry name → tag id → resolved entry ids.
pub(crate) type RegistryTags =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, Vec<String>>>;

impl std::fmt::Debug for GameSystems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameSystems").finish_non_exhaustive()
    }
}

impl GameSystems {
    /// Empty systems; [`crate::commands::load_game_systems`] fills them once the server exists.
    pub(crate) fn new(
        level_dir: PathBuf,
        function_level: u8,
        random_sequences: SharedRandomSequences,
    ) -> Self {
        GameSystems {
            mob_spawns: std::sync::OnceLock::new(),
            scoreboard: Mutex::new(Scoreboard::new()),
            scoreboard_sync: Mutex::default(),
            storage: Mutex::default(),
            functions: RwLock::new(Arc::new(FunctionLibrary::default())),
            advancements: RwLock::default(),
            advancement_roots: RwLock::default(),
            advancements_loading: Mutex::new(()),
            post_reload: AtomicBool::new(false),
            function_level,
            level_dir,
            spawn: Mutex::new(Vec3::ZERO),
            tags: RwLock::new(Arc::new(RegistryTags::new())),
            scheduled: Mutex::default(),
            random_sequences,
            stopwatches: Mutex::default(),
            profiling: Mutex::default(),
            waypoints: Mutex::default(),
            bossbars: Mutex::default(),
            game_tests: Mutex::default(),
        }
    }

    /// The game tests (`GameTestTicker.SINGLETON`, `FailedTestTracker`).
    pub(crate) fn game_tests(&self) -> MutexGuard<'_, crate::commands::gametest::GameTests> {
        lock(&self.game_tests)
    }

    /// Scheduled functions (`MinecraftServer.getScheduledEvents`).
    pub(crate) fn scheduled(&self) -> MutexGuard<'_, TimerQueue> {
        lock(&self.scheduled)
    }

    /// Random sequences (`MinecraftServer.getRandomSequences`).
    pub(crate) fn random_sequences(&self) -> MutexGuard<'_, RandomSequences> {
        lock(&self.random_sequences)
    }

    /// Stopwatches (`MinecraftServer.getStopwatches`).
    pub(crate) fn stopwatches(&self) -> MutexGuard<'_, Stopwatches> {
        lock(&self.stopwatches)
    }

    /// The profilers of `debug`, `perf` and `jfr`.
    pub(crate) fn profiling(&self) -> MutexGuard<'_, Profiling> {
        lock(&self.profiling)
    }

    /// Waypoint icons changed by `/waypoint`, by entity UUID.
    pub(crate) fn waypoints(
        &self,
    ) -> MutexGuard<'_, std::collections::BTreeMap<u128, WaypointIcon>> {
        lock(&self.waypoints)
    }

    /// The registry tags.
    pub(crate) fn tags(&self) -> Arc<RegistryTags> {
        Arc::clone(&self.tags.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replaces the registry tags.
    pub(crate) fn set_tags(&self, tags: RegistryTags) {
        *self.tags.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(tags);
    }

    /// Whether `entry` is in `#tag` of `registry`.
    pub(crate) fn in_tag(&self, registry: &str, tag: &str, entry: &str) -> bool {
        self.tags()
            .get(registry)
            .and_then(|t| t.get(tag))
            .is_some_and(|e| e.iter().any(|e| e == entry))
    }

    /// The scoreboard.
    pub(crate) fn scoreboard(&self) -> MutexGuard<'_, Scoreboard> {
        self.scoreboard
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The scoreboard as the clients know it ([`crate::scoreboard::flush`]).
    pub(crate) fn scoreboard_sync(&self) -> MutexGuard<'_, crate::scoreboard::ScoreboardSync> {
        lock(&self.scoreboard_sync)
    }

    /// Loads the scoreboard, the command storage, the scheduled functions, the random
    /// sequences and the stopwatches from the world's saved data (`ServerScoreboard.load`,
    /// `CommandStorage`, `TimerQueue`, `RandomSequences`, `Stopwatches`).
    /// `legacy` reads the `level.dat` fields of worlds saved before the split into saved data
    /// (`LevelDatToSavedDataPreparationFix`, `LevelDatToSavedDataFileFix`).
    pub(crate) fn load(&self, data: &SavedDataStore, legacy: &dyn Fn(&str) -> Option<Tag>) {
        let read = |id: &str| {
            data.read(id).unwrap_or_else(|e| {
                log::error!("Cannot read {id}: {e}");
                None
            })
        };
        if let Some(c) = read(SCOREBOARD) {
            *self.scoreboard() = Scoreboard::from_nbt(&c);
        }
        if let Some(c) = read(SCHEDULED_EVENTS).or_else(|| legacy_scheduled_events(legacy)) {
            *self.scheduled() = TimerQueue::from_nbt(&c);
        }
        if let Some(c) = read(STOPWATCHES) {
            *self.stopwatches() = Stopwatches::from_nbt(&c);
        }
        if let Some(c) = read(vanillite_game::random_sequences::SAVED_DATA) {
            let mut sequences = self.random_sequences();
            *sequences = RandomSequences::load(sequences.world_seed(), &c);
        }
        *self.storage() = CommandStorage::load(data);
        let bars = read(crate::bossbars::SAVED_DATA).or_else(|| match legacy("CustomBossEvents") {
            Some(Tag::Compound(c)) => Some(c),
            _ => None,
        });
        if let Some(c) = bars {
            let mut random = vanillite_base::random::LegacyRandom::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_nanos() % (1 << 62)).unwrap_or(0)),
            );
            *self.bossbars() = crate::bossbars::CustomBossEvents::load(&c, &mut || {
                vanillite_game::mob::random_uuid(&mut random)
            });
        }
    }

    /// Writes what changed since the last save (`storeToSaveDataIfDirty`, `setDirty`).
    pub(crate) fn save(&self, data: &SavedDataStore) {
        let scoreboard = {
            let mut board = self.scoreboard();
            board.take_dirty().then(|| board.to_nbt())
        };
        if let Some(c) = scoreboard {
            data.write(SCOREBOARD, c);
        }
        let scheduled = {
            let mut queue = self.scheduled();
            queue.take_dirty().then(|| queue.to_nbt())
        };
        if let Some(c) = scheduled {
            data.write(SCHEDULED_EVENTS, c);
        }
        let stopwatches = self.stopwatches().take_dirty_nbt();
        if let Some(c) = stopwatches {
            data.write(STOPWATCHES, c);
        }
        let sequences = {
            let mut sequences = self.random_sequences();
            sequences.take_dirty().then(|| sequences.to_nbt())
        };
        if let Some(c) = sequences {
            data.write(vanillite_game::random_sequences::SAVED_DATA, c);
        }
        for (id, c) in self.storage().take_dirty() {
            data.write(&id, c);
        }
        if let Some(c) = self.bossbars().take_dirty() {
            data.write(crate::bossbars::SAVED_DATA, c);
        }
    }

    /// Command storage.
    pub(crate) fn storage(&self) -> MutexGuard<'_, CommandStorage> {
        self.storage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The function library.
    pub(crate) fn functions(&self) -> Arc<FunctionLibrary> {
        Arc::clone(
            &self
                .functions
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Replaces the function library; the load functions run on the next tick.
    pub(crate) fn set_functions(&self, library: FunctionLibrary) {
        *self
            .functions
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(library);
        self.post_reload.store(true, Ordering::Release);
    }

    /// The advancements (`ServerAdvancementManager`), read from the packs on first use: only
    /// players need them, and a login reads them ahead ([`GameSystems::prefetch_advancements`]).
    pub(crate) fn advancements(&self) -> Arc<Advancements> {
        if let Some(set) = self.loaded_advancements() {
            return set;
        }
        let _loading = lock(&self.advancements_loading);
        if let Some(set) = self.loaded_advancements() {
            return set;
        }
        let roots = self
            .advancement_roots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let set = Arc::new(crate::commands::load_advancements_from(&roots));
        *self
            .advancements
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&set));
        set
    }

    fn loaded_advancements(&self) -> Option<Arc<Advancements>> {
        self.advancements
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Reads the advancements on a background thread if they are not loaded (a player logs in).
    pub(crate) fn prefetch_advancements(
        global: &Arc<crate::global::Global>,
        compute: &crate::compute::Compute,
    ) {
        if global.systems.loaded_advancements().is_none() {
            let global = Arc::clone(global);
            compute.spawn_background(0, move || {
                global.systems.advancements();
            });
        }
    }

    /// Where the advancements are read from on first use (the start).
    pub(crate) fn set_advancement_roots(&self, roots: Vec<PathBuf>) {
        *self
            .advancement_roots
            .write()
            .unwrap_or_else(PoisonError::into_inner) = roots;
        *self
            .advancements
            .write()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Replaces the advancements read from `roots` (`/reload`); players that join afterwards
    /// use them.
    pub(crate) fn set_advancements(&self, advancements: Advancements, roots: Vec<PathBuf>) {
        let _loading = lock(&self.advancements_loading);
        *self
            .advancement_roots
            .write()
            .unwrap_or_else(PoisonError::into_inner) = roots;
        *self
            .advancements
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(advancements));
    }

    /// Drops the advancements if nobody holds them (no player is online); the next use reads
    /// them again.
    pub(crate) fn release_advancements(&self) {
        let _loading = lock(&self.advancements_loading);
        let mut set = self
            .advancements
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if set.as_ref().is_some_and(|set| Arc::strong_count(set) == 1) {
            *set = None;
        }
    }

    /// Whether the load functions are due; clears the flag.
    pub(crate) fn take_post_reload(&self) -> bool {
        self.post_reload.swap(false, Ordering::AcqRel)
    }

    /// The command boss bars (`MinecraftServer.getCustomBossEvents`).
    pub(crate) fn bossbars(&self) -> MutexGuard<'_, crate::bossbars::CustomBossEvents> {
        lock(&self.bossbars)
    }

    /// Whether a command boss bar exists (`CustomBossEvents.get`).
    pub(crate) fn bossbar_exists(&self, id: &str) -> bool {
        self.bossbars().get(id).is_some()
    }

    /// Sets a command boss bar's value or maximum (`execute store bossbar`); the packets of the
    /// change go to `out`.
    pub(crate) fn set_bossbar_value(
        &self,
        id: &str,
        max: bool,
        value: i32,
        out: &mut crate::bossbars::Outbox,
    ) {
        if let Some(e) = self.bossbars().get_mut(id) {
            if max {
                e.set_max(value, out);
            } else {
                e.set_value(value, out);
            }
        }
    }

    /// The elapsed seconds of a stopwatch (`Stopwatches.get`, `Stopwatch.elapsedSeconds`).
    pub(crate) fn stopwatch_seconds(&self, id: &str) -> Option<f64> {
        let stopwatches = self.stopwatches();
        let now = stopwatches.now();
        stopwatches.get(id).map(|s| s.elapsed_seconds(now))
    }

    /// The overworld spawn (the server's command position).
    pub(crate) fn spawn(&self) -> Vec3 {
        *self.spawn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the overworld spawn.
    pub(crate) fn set_spawn(&self, pos: Vec3) {
        *self.spawn.lock().unwrap_or_else(PoisonError::into_inner) = pos;
    }
}

/// The scheduled functions of an old `level.dat` (`ScheduledEvents`, renamed by
/// `LevelDatToSavedDataPreparationFix`), in the saved data format.
fn legacy_scheduled_events(legacy: &dyn Fn(&str) -> Option<Tag>) -> Option<Compound> {
    if let Some(Tag::Compound(c)) = legacy("scheduled_events") {
        return Some(c);
    }
    let Some(Tag::List(list)) = legacy("ScheduledEvents") else {
        return None;
    };
    let rename = |c: &Compound, from: &str, to: &str, out: &mut Compound| {
        if let Some(v) = c.get(from) {
            out.insert(to, v.clone());
        }
    };
    let events = list
        .0
        .iter()
        .filter_map(|e| {
            let Tag::Compound(e) = e else {
                return None;
            };
            let mut out = Compound::new();
            rename(e, "Name", "id", &mut out);
            rename(e, "TriggerTime", "trigger_time", &mut out);
            if let Some(Tag::Compound(cb)) = e.get("Callback") {
                let mut callback = Compound::new();
                rename(cb, "Type", "type", &mut callback);
                rename(cb, "Name", "id", &mut callback);
                out.insert("callback", Tag::Compound(callback));
            }
            Some(Tag::Compound(out))
        })
        .collect();
    let mut c = Compound::new();
    c.insert("events", Tag::List(vanillite_nbt::List(events)));
    Some(c)
}

#[cfg(test)]
mod legacy_tests {
    use vanillite_nbt::{Compound, List, Tag};

    #[test]
    fn scheduled_events_of_old_level_dat() {
        let mut callback = Compound::new();
        callback.insert("Type", "minecraft:function");
        callback.insert("Name", "test:tick");
        let mut event = Compound::new();
        event.insert("Name", "test:tick");
        event.insert("TriggerTime", Tag::Long(40));
        event.insert("Callback", Tag::Compound(callback));
        let list = Tag::List(List(vec![Tag::Compound(event)]));
        let converted =
            super::legacy_scheduled_events(&|k| (k == "ScheduledEvents").then(|| list.clone()))
                .expect("converted");
        let Some(Tag::List(events)) = converted.get("events") else {
            panic!("no events");
        };
        let Some(Tag::Compound(e)) = events.0.first() else {
            panic!("no event");
        };
        assert_eq!(e.get("id").and_then(Tag::as_str), Some("test:tick"));
        assert_eq!(e.get("trigger_time").and_then(Tag::as_i64), Some(40));
        let Some(Tag::Compound(cb)) = e.get("callback") else {
            panic!("no callback");
        };
        assert_eq!(
            cb.get("type").and_then(Tag::as_str),
            Some("minecraft:function")
        );
        assert!(super::legacy_scheduled_events(&|_| None).is_none());
    }
}
