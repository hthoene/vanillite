//! The dimensions of a server: Vanilla's three and those of the data packs and the world's
//! `world_gen_settings.dat` (`WorldDimensions.bake`), in tick order, then the extra worlds
//! (`[extra-worlds]`, plugins), which come and go while the server runs. A world's index here
//! is its [`WorldId`]; an unloaded extra world keeps its index, and loading it again reuses it.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde_json::Value;
use vanillite_datapack::LoadedPacks;

use crate::level::DimensionFacts;
use crate::world::WorldId;

/// Worlds per segment of the registry.
const SEGMENT: usize = 16;
/// Segments of the registry: at most `SEGMENT * SEGMENTS` worlds over a server's run.
const SEGMENTS: usize = 64;

/// One dimension: its key (`minecraft:the_nether`), the key of its dimension type and what the
/// server needs of that type.
#[derive(Debug)]
struct Dimension {
    id: String,
    kind: String,
    facts: DimensionFacts,
    /// `DimensionType.defaultClock`: the index of its world clock.
    default_clock: Option<usize>,
    /// The seed of its world (an extra world's is known once its folder is read).
    seed: AtomicI64,
    /// The folder of an extra world (relative to the server folder); `None` for the
    /// dimensions of the default world.
    folder: Option<String>,
    loaded: AtomicBool,
}

type Segment = [OnceLock<Dimension>; SEGMENT];

#[derive(Debug)]
struct Registry {
    segments: [OnceLock<Box<Segment>>; SEGMENTS],
    /// Registered slots.
    len: AtomicUsize,
    /// Serializes registrations.
    adding: Mutex<()>,
}

/// The dimensions of a server in tick order: the overworld, the nether and the end, then the
/// others, then the extra worlds. Cheap to clone; clones share the registry.
#[derive(Clone, Debug)]
pub struct Dimensions(Arc<Registry>);

/// The environment of an extra world: which of Vanilla's dimension types it has (Bukkit's
/// `World.Environment`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Environment {
    /// `minecraft:overworld`.
    Normal,
    /// `minecraft:the_nether`.
    Nether,
    /// `minecraft:the_end`.
    TheEnd,
}

impl Environment {
    /// The dimension type key.
    pub fn kind(self) -> &'static str {
        match self {
            Environment::Normal => "minecraft:overworld",
            Environment::Nether => "minecraft:the_nether",
            Environment::TheEnd => "minecraft:the_end",
        }
    }

    /// The environment of a dimension type key; `None` for other types.
    pub fn of_kind(kind: &str) -> Option<Environment> {
        match kind {
            "minecraft:overworld" => Some(Environment::Normal),
            "minecraft:the_nether" => Some(Environment::Nether),
            "minecraft:the_end" => Some(Environment::TheEnd),
            _ => None,
        }
    }

    /// From its name: `normal`, `nether` or `the_end` (Bukkit's names, any case).
    pub fn parse(name: &str) -> Option<Environment> {
        match name.trim().to_ascii_lowercase().as_str() {
            "normal" | "overworld" => Some(Environment::Normal),
            "nether" | "the_nether" => Some(Environment::Nether),
            "the_end" | "end" => Some(Environment::TheEnd),
            _ => None,
        }
    }

    /// Its name as [`Environment::parse`] reads it.
    pub fn name(self) -> &'static str {
        match self {
            Environment::Normal => "normal",
            Environment::Nether => "nether",
            Environment::TheEnd => "the_end",
        }
    }
}

impl Dimensions {
    /// The dimensions `(key, dimension type)` in tick order, with the facts of their types from
    /// `packs` (Vanilla's overworld values without packs).
    pub(crate) fn new(
        dimensions: Vec<(String, String)>,
        packs: Option<&LoadedPacks>,
        seed: i64,
    ) -> Self {
        let this = Dimensions(Arc::new(Registry {
            segments: std::array::from_fn(|_| OnceLock::new()),
            len: AtomicUsize::new(0),
            adding: Mutex::new(()),
        }));
        for (id, kind) in dimensions {
            let _ = this.register(&id, &kind, None, packs, seed);
        }
        this
    }

    /// Vanilla's three dimensions with their own dimension types.
    pub fn vanilla() -> Self {
        Self::new(
            crate::server::VANILLA_DIMENSIONS
                .iter()
                .map(|d| ((*d).to_owned(), (*d).to_owned()))
                .collect(),
            None,
            0,
        )
    }

    fn slot(&self, index: usize) -> Option<&Dimension> {
        self.0.segments.get(index / SEGMENT)?.get()?[index % SEGMENT].get()
    }

    fn get(&self, world: WorldId) -> Option<&Dimension> {
        self.slot(world.index())
    }

    /// Adds a dimension (not yet loaded unless it belongs to the default world, `folder`
    /// `None`) or finds the unloaded slot of an extra world with this key again. Fails when
    /// the key is loaded, its slot has another dimension type, or the registry is full.
    pub(crate) fn register(
        &self,
        id: &str,
        kind: &str,
        folder: Option<&str>,
        packs: Option<&LoadedPacks>,
        seed: i64,
    ) -> Result<WorldId, String> {
        let _adding = self.0.adding.lock().unwrap_or_else(PoisonError::into_inner);
        let len = self.0.len.load(Ordering::Acquire);
        if let Some(index) = (0..len).find(|&i| self.slot(i).is_some_and(|d| d.id == id)) {
            let world = WorldId(u16::try_from(index).map_err(|e| e.to_string())?);
            let existing = self.get(world).ok_or("missing slot")?;
            if existing.loaded.load(Ordering::Acquire) || existing.folder.is_none() {
                return Err(format!("world {id} is already loaded"));
            }
            if existing.kind != kind || existing.folder.as_deref() != folder {
                return Err(format!(
                    "world {id} was loaded before as {} in {}; restart the server to load it \
                     differently",
                    existing.kind,
                    existing.folder.as_deref().unwrap_or("?")
                ));
            }
            existing.seed.store(seed, Ordering::Relaxed);
            return Ok(world);
        }
        if len >= SEGMENT * SEGMENTS {
            return Err(format!("no more than {} worlds", SEGMENT * SEGMENTS));
        }
        let world = WorldId(u16::try_from(len).map_err(|e| e.to_string())?);
        let (facts, default_clock) = type_facts(packs, kind);
        let segment = self.0.segments[len / SEGMENT]
            .get_or_init(|| Box::new(std::array::from_fn(|_| OnceLock::new())));
        let _ = segment[len % SEGMENT].set(Dimension {
            id: id.to_owned(),
            kind: kind.to_owned(),
            facts,
            default_clock,
            seed: AtomicI64::new(seed),
            folder: folder.map(str::to_owned),
            loaded: AtomicBool::new(folder.is_none()),
        });
        self.0.len.store(len + 1, Ordering::Release);
        Ok(world)
    }

    /// Marks an extra world loaded or unloaded.
    pub(crate) fn set_loaded(&self, world: WorldId, loaded: bool) {
        if let Some(d) = self.get(world) {
            d.loaded.store(loaded, Ordering::Release);
        }
    }

    /// The number of slots, loaded or not: every [`WorldId`] is below it.
    pub fn len(&self) -> usize {
        self.0.len.load(Ordering::Acquire)
    }

    /// Whether there are none (never on a running server).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a world is loaded.
    pub fn is_loaded(&self, world: WorldId) -> bool {
        self.get(world)
            .is_some_and(|d| d.loaded.load(Ordering::Acquire))
    }

    /// The key of a world's dimension (also of an unloaded extra world).
    pub fn name(&self, world: WorldId) -> Option<&str> {
        self.get(world).map(|d| d.id.as_str())
    }

    /// The key of a world's dimension, the overworld's for an unknown world.
    pub fn name_or_overworld(&self, world: WorldId) -> &str {
        self.name(world).unwrap_or("minecraft:overworld")
    }

    /// The key of a world's dimension type.
    pub fn kind(&self, world: WorldId) -> Option<&str> {
        self.get(world).map(|d| d.kind.as_str())
    }

    /// `logicalHeight` of the dimension type of the world with dimension key `dimension`
    /// (384, the overworld's, for an unknown key).
    pub fn logical_height_of(&self, dimension: &str) -> i32 {
        (0..self.len())
            .filter_map(|i| self.slot(i))
            .find(|d| d.id == dimension)
            .map_or(384, |d| d.facts.logical_height)
    }

    /// What the server needs of a world's dimension type.
    pub fn facts(&self, world: WorldId) -> Option<DimensionFacts> {
        self.get(world).map(|d| d.facts)
    }

    /// The default clock of a world's dimension type (`getDefaultClock`).
    pub fn default_clock(&self, world: WorldId) -> Option<usize> {
        self.get(world).and_then(|d| d.default_clock)
    }

    /// The seed of a world.
    pub fn seed(&self, world: WorldId) -> Option<i64> {
        self.get(world).map(|d| d.seed.load(Ordering::Relaxed))
    }

    /// The folder of an extra world; `None` for the default world's dimensions.
    pub fn folder(&self, world: WorldId) -> Option<&str> {
        self.get(world).and_then(|d| d.folder.as_deref())
    }

    /// The environment of an extra world (Bukkit portals and respawns go by it); `None` for
    /// the default world's dimensions.
    pub fn extra_environment(&self, world: WorldId) -> Option<Environment> {
        let d = self.get(world)?;
        d.folder.as_ref()?;
        Environment::of_kind(&d.kind)
    }

    /// The loaded world of a dimension key.
    pub fn world(&self, dimension: &str) -> Option<WorldId> {
        self.all()
            .find(|&w| self.name(w) == Some(dimension))
            .filter(|&w| self.is_loaded(w))
    }

    /// Every slot, loaded or not.
    fn all(&self) -> impl Iterator<Item = WorldId> + '_ {
        (0..self.len())
            .filter_map(|i| u16::try_from(i).ok())
            .map(WorldId)
    }

    /// The loaded worlds in tick order.
    pub fn worlds(&self) -> impl Iterator<Item = WorldId> + '_ {
        self.all().filter(|&w| self.is_loaded(w))
    }

    /// The dimension keys of the loaded worlds in tick order.
    pub fn names(&self) -> impl Iterator<Item = &str> + '_ {
        self.worlds().filter_map(|w| self.name(w))
    }

    /// `(world, key, dimension type)` of every loaded world in tick order.
    pub fn iter(&self) -> impl Iterator<Item = (WorldId, &str, &str)> + '_ {
        self.worlds()
            .filter_map(|w| self.get(w).map(|d| (w, d.id.as_str(), d.kind.as_str())))
    }
}

/// What the server needs of the dimension type `kind` and the index of its default clock.
fn type_facts(packs: Option<&LoadedPacks>, kind: &str) -> (DimensionFacts, Option<usize>) {
    let json: Option<Value> = packs
        .and_then(|p| p.registry("minecraft:dimension_type"))
        .and_then(|r| r.get(kind))
        .and_then(|e| serde_json::from_str(e.json.get()).ok());
    let field = |key: &str| json.as_ref().and_then(|v| v.get(key));
    let flag = |key: &str| field(key).and_then(Value::as_bool).unwrap_or(false);
    let facts = DimensionFacts {
        coordinate_scale: field("coordinate_scale")
            .and_then(Value::as_f64)
            .unwrap_or(1.0),
        logical_height: field("logical_height")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            // Without the registry: the Vanilla dimension types' heights.
            .unwrap_or(match kind {
                "minecraft:the_nether" => 128,
                "minecraft:the_end" => 256,
                _ => 384,
            }),
        has_ceiling: flag("has_ceiling"),
        has_ender_dragon_fight: flag("has_ender_dragon_fight"),
    };
    // A clock's index is its position in the world clock registry (`Clocks::index`).
    let clock = field("default_clock")
        .and_then(Value::as_str)
        .and_then(|c| {
            packs?
                .registry("minecraft:world_clock")?
                .entries()
                .iter()
                .position(|e| e.id == c)
        });
    (facts, clock)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `logicalHeight` comes from the dimension type (chorus fruit clamp, I1-04 of #272).
    #[test]
    fn logical_height_by_dimension_type() {
        let dims = Dimensions::vanilla();
        assert_eq!(dims.logical_height_of("minecraft:overworld"), 384);
        assert_eq!(dims.logical_height_of("minecraft:the_nether"), 128);
        assert_eq!(dims.logical_height_of("minecraft:the_end"), 256);
        dims.register(
            "minecraft:caves",
            "minecraft:the_nether",
            Some("caves"),
            None,
            0,
        )
        .unwrap();
        assert_eq!(dims.logical_height_of("minecraft:caves"), 128);
    }

    #[test]
    fn extra_worlds_keep_their_index() {
        let dims = Dimensions::vanilla();
        assert_eq!(dims.worlds().count(), 3);
        let lobby = dims
            .register(
                "minecraft:lobby",
                "minecraft:overworld",
                Some("lobby"),
                None,
                0,
            )
            .unwrap();
        assert_eq!(lobby, WorldId(3));
        assert!(!dims.is_loaded(lobby));
        assert_eq!(dims.world("minecraft:lobby"), None);
        dims.set_loaded(lobby, true);
        assert_eq!(dims.world("minecraft:lobby"), Some(lobby));
        assert_eq!(dims.extra_environment(lobby), Some(Environment::Normal));
        assert!(
            dims.register(
                "minecraft:lobby",
                "minecraft:overworld",
                Some("lobby"),
                None,
                0
            )
            .is_err()
        );
        dims.set_loaded(lobby, false);
        assert_eq!(dims.names().count(), 3);
        assert_eq!(
            dims.register(
                "minecraft:lobby",
                "minecraft:overworld",
                Some("lobby"),
                None,
                0
            ),
            Ok(lobby)
        );
        assert!(
            dims.register(
                "minecraft:lobby",
                "minecraft:the_nether",
                Some("lobby"),
                None,
                0
            )
            .is_err()
        );
        assert!(
            dims.register("minecraft:overworld", "minecraft:overworld", None, None, 0)
                .is_err()
        );
        let more: Vec<WorldId> = (0..40)
            .map(|i| {
                dims.register(
                    &format!("minecraft:w{i}"),
                    "minecraft:the_end",
                    Some("w"),
                    None,
                    0,
                )
                .unwrap()
            })
            .collect();
        assert_eq!(more.last(), Some(&WorldId(43)));
        assert_eq!(dims.name(WorldId(43)), Some("minecraft:w39"));
    }
}
