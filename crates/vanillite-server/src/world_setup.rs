//! What the worlds need at startup: the block facts, each dimension's [`WorldInfo`], the
//! compute pool as the chunk runtime's executor, and chunk packets for the players.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use vanillite_base::ChunkPos;
use vanillite_base::hash::FxHashMap;
use vanillite_chunk::packet::BlockEntityWire;
use vanillite_game::player::ChunkSource;
use vanillite_protocol::packets::play::clientbound::LevelChunkWithLight;
use vanillite_protocol::{Packet, SharedPacket};
use vanillite_world::chunks::Chunks;

use vanillite_chunk::slots::{ChunkGenerator, ChunkGeneratorProvider, FlatGenerator, WorldInfo};
use vanillite_datapack::LoadedPacks;
use vanillite_world::chunks::Executor;
use vanillite_worldgen::{DimensionSpec, TerrainGenerator, VanillaChunkGenerator};

use crate::compute::Compute;
use crate::server::StartError;

/// Installs the per-state block facts the chunk format needs (once per process). Without data
/// packs (tests) the heightmap tags are empty and there is one biome.
pub(crate) fn install_facts(packs: Option<&LoadedPacks>) -> Result<(), StartError> {
    let Some(packs) = packs else {
        vanillite_chunk::facts::install_vanilla(&[], &[], 1);
        return Ok(());
    };
    let tag = |name: &str| {
        packs
            .tag_ids("minecraft:block", name)
            .map_err(|e| StartError::Io(format!("data packs: {e:?}")))
    };
    let biomes = packs
        .registry("minecraft:worldgen/biome")
        .map_or(0, |r| r.entries().len());
    vanillite_chunk::facts::install_vanilla(
        &tag("minecraft:blocks_motion_in_heightmap")?,
        &tag("minecraft:blocks_motion_in_heightmap_no_leaves")?,
        u32::try_from(biomes).unwrap_or(u32::MAX),
    );
    Ok(())
}

/// `level-seed`: a number, or any other text hashed like Java's `String.hashCode`; `None` if
/// empty.
pub(crate) fn parse_seed(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.parse::<i64>().unwrap_or_else(|_| {
        let hash = text
            .encode_utf16()
            .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(i32::from(c)));
        i64::from(hash)
    }))
}

/// A seed for a new world without `level-seed` (`WorldOptions.randomSeed`).
pub(crate) fn random_seed() -> i64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        hasher.write_u128(now.as_nanos());
    }
    hasher.finish() as i64
}

/// Saved data id of Vanilla's `WorldGenSettings`.
const WORLD_GEN_SETTINGS: &str = "minecraft:world_gen_settings";

/// Vanilla's `WorldGenSettings` of a world: the seed and the dimensions (`LevelStem`s) the server
/// loads, with each one's dimension type and generator.
#[derive(Debug, Clone)]
pub(crate) struct WorldGen {
    /// World seed.
    pub(crate) seed: i64,
    /// `generate_structures`.
    generate_structures: bool,
    /// The dimensions in Vanilla's order (`WorldDimensions.keysInOrder`).
    stems: Vec<Stem>,
}

/// One dimension (`LevelStem`): its key, its dimension type and its generator JSON.
#[derive(Debug, Clone)]
struct Stem {
    id: String,
    kind: String,
    generator: Option<serde_json::Value>,
}

impl Stem {
    /// A stem from its JSON (`{"type": ..., "generator": ...}`); a type given inline is taken
    /// as the dimension's own key.
    fn from_json(id: &str, json: &serde_json::Value) -> Stem {
        Stem {
            id: id.to_owned(),
            kind: json
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(id)
                .to_owned(),
            generator: json.get("generator").cloned(),
        }
    }
}

impl WorldGen {
    /// Reads the world's `data/minecraft/world_gen_settings.dat` (Paper keeps it in the
    /// overworld's folder); `None` if there is none.
    pub(crate) fn load(level_dir: &Path) -> Result<Option<Self>, StartError> {
        let overworld = vanillite_storage::dimension_dir(level_dir, "minecraft:overworld");
        for dir in [level_dir, overworld.as_path()] {
            let data = vanillite_storage::read_saved_data(dir, WORLD_GEN_SETTINGS)
                .map_err(|e| StartError::io(dir, e))?;
            if let Some(data) = data {
                return Ok(Some(Self::from_nbt(&data)));
            }
        }
        Ok(None)
    }

    /// No saved settings: seed 0, no saved dimensions.
    pub(crate) fn empty() -> Self {
        Self {
            seed: 0,
            generate_structures: true,
            stems: Vec::new(),
        }
    }

    /// Adds the dimensions of the data packs (`minecraft:dimension`), which take precedence
    /// over the saved ones, and orders them like `WorldDimensions.bake`: the overworld, the
    /// nether and the end first (always present here), then the others by key.
    pub(crate) fn bake(mut self, packs: Option<&LoadedPacks>) -> Self {
        if let Some(registry) = packs.and_then(|p| p.registry("minecraft:dimension")) {
            for entry in registry.entries() {
                let Ok(json) = serde_json::from_str::<serde_json::Value>(entry.json.get()) else {
                    log::warn!("dimension {}: invalid JSON, skipped", entry.id);
                    continue;
                };
                let stem = Stem::from_json(&entry.id, &json);
                match self.stems.iter_mut().find(|s| s.id == entry.id) {
                    Some(old) => *old = stem,
                    None => self.stems.push(stem),
                }
            }
        }
        for id in crate::server::VANILLA_DIMENSIONS {
            if !self.stems.iter().any(|s| s.id == id) {
                self.stems.push(Stem {
                    id: id.to_owned(),
                    kind: id.to_owned(),
                    generator: None,
                });
            }
        }
        let rank = |id: &str| {
            crate::server::VANILLA_DIMENSIONS
                .iter()
                .position(|d| *d == id)
                .unwrap_or(crate::server::VANILLA_DIMENSIONS.len())
        };
        self.stems
            .sort_by(|a, b| rank(&a.id).cmp(&rank(&b.id)).then_with(|| a.id.cmp(&b.id)));
        self
    }

    /// The dimensions the server loads, in tick order.
    pub(crate) fn dimensions(&self, packs: Option<&LoadedPacks>) -> crate::dimensions::Dimensions {
        crate::dimensions::Dimensions::new(
            self.stems
                .iter()
                .map(|s| (s.id.clone(), s.kind.clone()))
                .collect(),
            packs,
            self.seed,
        )
    }

    pub(crate) fn load_or_create(
        level_dir: &Path,
        properties: &vanillite_config::ServerProperties,
        packs: Option<&LoadedPacks>,
    ) -> Result<Self, StartError> {
        if let Some(world_gen) = Self::load(level_dir)? {
            return Ok(world_gen.bake(packs));
        }
        let seed = parse_seed(&properties.level_seed).unwrap_or_else(random_seed);
        let preset = match properties.level_type.trim().to_lowercase().as_str() {
            "" | "default" | "minecraft:default" => "minecraft:normal".to_owned(),
            name if name.contains(':') => name.to_owned(),
            name => format!("minecraft:{name}"),
        };
        let dimensions = packs
            .and_then(|p| p.registry("minecraft:worldgen/world_preset"))
            .and_then(|r| r.get(&preset))
            .and_then(|entry| serde_json::from_str::<serde_json::Value>(entry.json.get()).ok())
            .and_then(|json| json.get("dimensions").cloned());
        let Some(serde_json::Value::Object(dimensions)) = dimensions else {
            // Without data packs (tests) there are no presets: the flat stand-in everywhere.
            return Ok(Self {
                seed,
                generate_structures: properties.generate_structures,
                stems: Vec::new(),
            }
            .bake(packs));
        };
        let mut stems = vanillite_nbt::Compound::new();
        for (key, stem) in &dimensions {
            if let vanillite_nbt::Tag::Compound(stem) = json_to_nbt(stem) {
                stems.insert(key.clone(), stem);
            }
        }
        let data = vanillite_nbt::Compound::new()
            .with("generate_structures", properties.generate_structures)
            .with("bonus_chest", false)
            .with("seed", seed)
            .with("dimensions", stems);
        vanillite_storage::write_saved_data(level_dir, WORLD_GEN_SETTINGS, data.clone())
            .map_err(|e| StartError::io(level_dir, e))?;
        log::info!("New world: preset {preset}, seed {seed}");
        Ok(Self::from_nbt(&data).bake(packs))
    }

    /// The settings of an extra world: one dimension `id` of type `kind` with `generator`
    /// (`None`: the classic flat layers), written to `level_dir` like Vanilla's
    /// `world_gen_settings.dat`.
    pub(crate) fn create_single(
        level_dir: &Path,
        (id, kind): (&str, &str),
        generator: Option<serde_json::Value>,
        seed: i64,
        generate_structures: bool,
    ) -> Result<Self, StartError> {
        let mut stem = vanillite_nbt::Compound::new().with("type", kind);
        if let Some(generator) = &generator
            && let vanillite_nbt::Tag::Compound(generator) = json_to_nbt(generator)
        {
            stem.insert("generator", generator);
        }
        let data = vanillite_nbt::Compound::new()
            .with("generate_structures", generate_structures)
            .with("bonus_chest", false)
            .with("seed", seed)
            .with("dimensions", vanillite_nbt::Compound::new().with(id, stem));
        vanillite_storage::write_saved_data(level_dir, WORLD_GEN_SETTINGS, data.clone())
            .map_err(|e| StartError::io(level_dir, e))?;
        Ok(Self::from_nbt(&data))
    }

    /// The dimension type of a saved dimension.
    pub(crate) fn kind_of(&self, dimension: &str) -> Option<&str> {
        self.stems
            .iter()
            .find(|s| s.id == dimension)
            .map(|s| s.kind.as_str())
    }

    /// The saved dimension `from` under the key `to` (an extra world whose files another
    /// server wrote under Vanilla's key).
    pub(crate) fn renamed(mut self, from: &str, to: &str) -> Self {
        self.stems.retain(|s| s.id == from);
        for stem in &mut self.stems {
            stem.id = to.to_owned();
        }
        self
    }

    /// The generator JSON of a dimension, if it has one.
    pub(crate) fn generator_json(&self, dimension: &str) -> Option<serde_json::Value> {
        self.generator(dimension).cloned()
    }

    fn from_nbt(data: &vanillite_nbt::Compound) -> Self {
        let seed = data
            .get("seed")
            .and_then(vanillite_nbt::Tag::as_i64)
            .unwrap_or(0);
        let mut stems = Vec::new();
        if let Some(dimensions) = data
            .get("dimensions")
            .and_then(vanillite_nbt::Tag::as_compound)
        {
            for (key, stem) in dimensions.iter() {
                stems.push(Stem::from_json(key, &nbt_to_json(stem)));
            }
        }
        // Vanilla reads a missing or non-byte value as `true`.
        let generate_structures = !matches!(
            data.get("generate_structures"),
            Some(vanillite_nbt::Tag::Byte(0))
        );
        Self {
            seed,
            generate_structures,
            stems,
        }
    }

    /// The generator JSON of a dimension.
    fn generator(&self, dimension: &str) -> Option<&serde_json::Value> {
        self.stems
            .iter()
            .find(|s| s.id == dimension)?
            .generator
            .as_ref()
    }

    /// The total height of the overworld's flat layers (`FlatLevelSource.getSpawnHeight` takes
    /// it); `None` for other generators.
    pub(crate) fn flat_spawn_height(&self) -> Option<i32> {
        self.flat_spawn_height_of("minecraft:overworld")
    }

    /// The total height of a dimension's flat layers; `None` for other generators.
    pub(crate) fn flat_spawn_height_of(&self, dimension: &str) -> Option<i32> {
        let generator = self.generator(dimension)?;
        if generator.get("type").and_then(|t| t.as_str()) != Some("minecraft:flat") {
            return None;
        }
        let layers = generator.get("settings")?.get("layers")?.as_array()?;
        Some(
            layers
                .iter()
                .filter_map(|l| l.get("height")?.as_i64())
                .map(|h| i32::try_from(h).unwrap_or(0))
                .sum(),
        )
    }

    /// Whether the overworld uses the flat generator (`is_flat` of the login packet).
    pub(crate) fn is_flat(&self) -> bool {
        self.is_flat_of("minecraft:overworld")
    }

    /// Whether a dimension uses no noise generator (`is_flat` of the login packet).
    pub(crate) fn is_flat_of(&self, dimension: &str) -> bool {
        self.generator(dimension)
            .is_none_or(|g| g.get("type").and_then(|t| t.as_str()) != Some("minecraft:noise"))
    }
}

/// JSON as NBT the way Vanilla's codecs read it back: integers as `Int` (`Long` if too large),
/// other numbers as `Double`, booleans as `Byte`; `null` becomes an empty compound.
fn json_to_nbt(json: &serde_json::Value) -> vanillite_nbt::Tag {
    use serde_json::Value;
    use vanillite_nbt::Tag;
    match json {
        Value::Null => Tag::Compound(vanillite_nbt::Compound::new()),
        Value::Bool(b) => Tag::from(*b),
        Value::Number(n) => match n.as_i64() {
            Some(v) => i32::try_from(v).map_or(Tag::Long(v), Tag::Int),
            None => Tag::Double(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => Tag::String(s.clone()),
        Value::Array(items) => {
            Tag::List(vanillite_nbt::List(items.iter().map(json_to_nbt).collect()))
        }
        Value::Object(map) => {
            let mut c = vanillite_nbt::Compound::with_capacity(map.len());
            for (k, v) in map {
                c.insert(k.clone(), json_to_nbt(v));
            }
            Tag::Compound(c)
        }
    }
}

/// NBT as JSON for the data-driven parsers (numbers stay numbers, `Byte` too).
pub(crate) fn nbt_to_json(tag: &vanillite_nbt::Tag) -> serde_json::Value {
    use serde_json::Value;
    use vanillite_nbt::Tag;
    match tag {
        Tag::Byte(v) => Value::from(*v),
        Tag::Short(v) => Value::from(*v),
        Tag::Int(v) => Value::from(*v),
        Tag::Long(v) => Value::from(*v),
        Tag::Float(v) => Value::from(*v),
        Tag::Double(v) => Value::from(*v),
        Tag::String(s) => Value::String(s.clone()),
        Tag::ByteArray(v) => v.iter().map(|&b| Value::from(b)).collect(),
        Tag::IntArray(v) => v.iter().map(|&b| Value::from(b)).collect(),
        Tag::LongArray(v) => v.iter().map(|&b| Value::from(b)).collect(),
        Tag::List(list) => list.0.iter().map(nbt_to_json).collect(),
        Tag::Compound(c) => Value::Object(
            c.iter()
                .map(|(k, v)| (k.to_owned(), nbt_to_json(v)))
                .collect(),
        ),
    }
}

/// The built-in `chunk-generator` slot: Vanilla's noise generator for `minecraft:noise`
/// dimensions, the classic flat layers for everything else.
#[derive(Debug)]
pub(crate) struct BuiltInGenerators {
    pub(crate) packs: Option<Arc<LoadedPacks>>,
    pub(crate) world_gen: WorldGen,
    /// The noise generators made, by dimension, for the levels (`/locate`).
    pub(crate) created: GeneratorsByDimension,
    /// The settings of the extra worlds, by dimension key.
    pub(crate) extra: ExtraWorldGens,
    /// The server's structure template cache.
    pub(crate) templates: Arc<vanillite_worldgen::template::StructureTemplateManager>,
}

/// The generator settings of the extra worlds by dimension key, shared with the server.
pub(crate) type ExtraWorldGens =
    Arc<std::sync::Mutex<vanillite_base::hash::FxHashMap<String, WorldGen>>>;

/// Terrain generators by dimension, shared with the levels.
pub(crate) type GeneratorsByDimension = Arc<
    std::sync::Mutex<vanillite_base::hash::FxHashMap<String, vanillite_game::level::Generation>>,
>;

impl ChunkGeneratorProvider for BuiltInGenerators {
    fn create(&self, world: &WorldInfo) -> Result<Arc<dyn ChunkGenerator>, String> {
        let extra = self
            .extra
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&world.dimension)
            .cloned();
        let world_gen = extra.as_ref().unwrap_or(&self.world_gen);
        let generator = world_gen.generator(&world.dimension);
        let kind = generator
            .and_then(|g| g.get("type"))
            .and_then(|t| t.as_str());
        match kind {
            Some("minecraft:flat") if self.packs.is_none() => {
                return Ok(Arc::new(flat_generator(
                    world,
                    generator.and_then(|g| g.get("settings")),
                )));
            }
            Some("minecraft:debug") => return Ok(Arc::new(DebugGenerator::new(world))),
            _ => {}
        }
        // `FlatLevelSource`: layers, the flat biome's adjusted features and the structure
        // overrides, through the same pipeline as the noise generator.
        let flat = match kind {
            Some("minecraft:flat") => Some(match generator.and_then(|g| g.get("settings")) {
                Some(settings) => vanillite_worldgen::FlatSettings::from_json(settings)
                    .map_err(|e| format!("{}: {e}", world.dimension))?,
                None => vanillite_worldgen::FlatSettings::classic(),
            }),
            _ => None,
        };
        let (Some(packs), Some(generator), true) = (
            self.packs.as_ref(),
            generator,
            flat.is_some() || kind == Some("minecraft:noise"),
        ) else {
            if let Some(kind) = kind {
                log::warn!(
                    "{}: generator {kind} is not supported, using flat",
                    world.dimension
                );
            }
            return Ok(Arc::new(FlatGenerator::classic(world)));
        };
        let (noise_settings, biome_source) = if flat.is_some() {
            (String::new(), &serde_json::Value::Null)
        } else {
            (
                generator
                    .get("settings")
                    .and_then(|s| s.as_str())
                    .ok_or("inline noise settings are not supported")?
                    .to_owned(),
                generator.get("biome_source").ok_or("no biome source")?,
            )
        };
        let (min_y, has_sky_light) = (world.min_y, world.has_sky_light);
        let height = i32::try_from(world.height).map_err(|e| e.to_string())?;
        // Built now and again after an empty server released it (`performance.memory`).
        let terrain = {
            let (packs, biome_source) = (Arc::clone(packs), biome_source.clone());
            let (seed, dimension) = (world_gen.seed, world.dimension.clone());
            let templates = Arc::clone(&self.templates);
            vanillite_worldgen::SharedTerrain::rebuildable(move || {
                let started = std::time::Instant::now();
                let spec = DimensionSpec {
                    noise_settings: &noise_settings,
                    biome_source: &biome_source,
                    min_y,
                    height,
                    has_sky_light,
                };
                let terrain = match &flat {
                    Some(flat) => TerrainGenerator::flat_with_templates(
                        &packs,
                        flat,
                        min_y,
                        height,
                        has_sky_light,
                        seed,
                        Arc::clone(&templates),
                    ),
                    None => {
                        TerrainGenerator::with_templates(&packs, spec, seed, Arc::clone(&templates))
                    }
                };
                log::debug!(
                    "{dimension}: terrain generator built in {:.0?}",
                    started.elapsed()
                );
                terrain
            })
            .map_err(|e| e.to_string())?
        };
        let generator = VanillaChunkGenerator::new(terrain, world.clone())
            .with_structures(world_gen.generate_structures);
        self.created
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                world.dimension.clone(),
                vanillite_game::level::Generation {
                    shared: generator.shared(),
                    dimension: world.dimension.clone(),
                    structures: generator.generates_structures(),
                },
            );
        Ok(Arc::new(generator))
    }
}

/// `FlatLevelSource` of `settings` (`FlatLevelGeneratorSettings`) without data packs: its layers
/// from the bottom of the world up and its biome. Without settings, Vanilla's default layers.
/// Lakes, features and structures need the data packs (the terrain generator's flat mode).
fn flat_generator(world: &WorldInfo, settings: Option<&serde_json::Value>) -> FlatGenerator {
    let Some(settings) = settings else {
        return FlatGenerator::classic(world);
    };
    let mut states = Vec::new();
    for layer in settings
        .get("layers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let block = layer
            .get("block")
            .and_then(serde_json::Value::as_str)
            .and_then(vanillite_data::block::BlockId::by_name);
        let height = layer
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .and_then(|h| usize::try_from(h).ok())
            .unwrap_or(0);
        match block {
            Some(block) => states.extend(std::iter::repeat_n(block.default_state().0, height)),
            None => log::warn!("{}: unknown flat layer block {layer}", world.dimension),
        }
    }
    states.truncate(usize::try_from(world.height).unwrap_or(usize::MAX));
    let mut info = world.clone();
    if let Some(biome) = settings
        .get("biome")
        .and_then(serde_json::Value::as_str)
        .and_then(|b| world.biomes.iter().position(|id| id == b))
        .and_then(|i| u16::try_from(i).ok())
    {
        info.default_biome = biome;
    }
    FlatGenerator::new(&info, &states)
}

/// `DebugLevelSource`: every block state in a grid at y 70 over a barrier floor at y 60.
#[derive(Debug)]
struct DebugGenerator {
    world: WorldInfo,
}

impl DebugGenerator {
    /// `DebugLevelSource.GRID_WIDTH` and `GRID_HEIGHT`.
    fn grid() -> (i32, i32) {
        let count = vanillite_data::block::STATE_COUNT;
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let width = (count as f32).sqrt().ceil() as i32;
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let height = (count as f32 / width as f32).ceil() as i32;
        (width, height)
    }

    fn new(world: &WorldInfo) -> Self {
        DebugGenerator {
            world: world.clone(),
        }
    }

    /// `DebugLevelSource.getBlockStateFor`.
    fn state_at(x: i32, z: i32) -> Option<u16> {
        if x <= 0 || z <= 0 || x % 2 == 0 || z % 2 == 0 {
            return None;
        }
        let (x, z) = (x / 2, z / 2);
        let (width, height) = Self::grid();
        if x > width || z > height {
            return None;
        }
        let index = usize::try_from(x.wrapping_mul(width).wrapping_add(z).unsigned_abs()).ok()?;
        (index < vanillite_data::block::STATE_COUNT)
            .then(|| u16::try_from(index).ok())
            .flatten()
    }
}

impl ChunkGenerator for DebugGenerator {
    fn new_chunk(&self, pos: ChunkPos) -> vanillite_chunk::ProtoChunk {
        let mut proto = vanillite_chunk::ProtoChunk::empty(pos, &self.world);
        let barrier = vanillite_data::block::ids::BARRIER.default_state().0;
        for z in 0..16u32 {
            for x in 0..16u32 {
                proto.chunk.set_block(x, 60, z, barrier);
                let world_x = pos.x.wrapping_mul(16).wrapping_add_unsigned(x);
                let world_z = pos.z.wrapping_mul(16).wrapping_add_unsigned(z);
                if let Some(state) = Self::state_at(world_x, world_z) {
                    proto.chunk.set_block(x, 70, z, state);
                }
            }
        }
        proto
    }

    fn generate(
        &self,
        _: vanillite_chunk::ChunkStatus,
        _: &mut [vanillite_chunk::ProtoChunk],
        _: &mut vanillite_chunk::slots::Neighbours<'_>,
    ) {
    }

    fn spawns_original_mobs(&self) -> bool {
        false
    }
}

/// The [`WorldInfo`] of a dimension stored in `dir`; its height and sky light come from its
/// dimension type `kind` (Vanilla's overworld values without data packs).
pub(crate) fn world_info(
    packs: Option<&LoadedPacks>,
    dir: PathBuf,
    dimension: &str,
    kind: &str,
    seed: i64,
) -> Result<WorldInfo, StartError> {
    let json: serde_json::Value = match packs {
        Some(packs) => {
            let entry = packs
                .registry("minecraft:dimension_type")
                .and_then(|r| r.get(kind))
                .ok_or_else(|| {
                    StartError::Io(format!("{dimension}: unknown dimension type {kind}"))
                })?;
            serde_json::from_str(entry.json.get())
                .map_err(|e| StartError::Io(format!("dimension type {kind}: {e}")))?
        }
        None => serde_json::json!({ "min_y": -64, "height": 384, "has_skylight": true }),
    };
    let int = |key: &str| json.get(key).and_then(serde_json::Value::as_i64);
    let min_y = int("min_y")
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(0);
    let height = int("height")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(256);
    let has_sky_light = json
        .get("has_skylight")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let biome_registry = packs.and_then(|p| p.registry("minecraft:worldgen/biome"));
    let biomes: Arc<[String]> = biome_registry
        .map(|r| r.entries().iter().map(|e| e.id.clone()).collect())
        .unwrap_or_else(|| Arc::from(["minecraft:plains".to_owned()]));
    let default_biome = biomes
        .iter()
        .position(|b| b == "minecraft:plains")
        .and_then(|i| u16::try_from(i).ok())
        .unwrap_or(0);
    Ok(WorldInfo {
        dimension: dimension.to_owned(),
        dir,
        min_y,
        height,
        seed,
        has_sky_light,
        biomes,
        default_biome,
        air: vanillite_data::block::BlockState::AIR.0,
    })
}

/// The compute pool as the chunk runtime's executor: background jobs by priority, with the
/// budget told when a world waits for them.
#[derive(Debug)]
pub(crate) struct ComputeExecutor(pub(crate) Compute, pub(crate) Arc<crate::budget::Budget>);

impl Executor for ComputeExecutor {
    fn spawn(&self, priority: u32, job: Box<dyn FnOnce() + Send>) {
        self.0.spawn_background(priority, job);
    }

    fn blocked(&self, blocked: bool) {
        self.1.set_blocked(blocked);
    }
}

/// Chunk packets for the players' chunk sending, encoded once per chunk and shared by every
/// player that receives it. Entries are dropped when a chunk's blocks or light change or it
/// unloads.
#[derive(Debug, Default)]
pub(crate) struct ChunkPackets {
    cache: RefCell<FxHashMap<ChunkPos, Arc<SharedPacket>>>,
    /// `anticheat.anti-xray`, `None` when disabled.
    anti_xray: Option<crate::anti_xray::AntiXray>,
    /// Block changes of the tick, for Anti-Xray.
    changes: Vec<vanillite_base::BlockPos>,
}

impl ChunkPackets {
    /// Drops the encoded chunk packets; they are encoded again when a player needs them.
    pub(crate) fn release_memory(&mut self) {
        *self.cache.get_mut() = FxHashMap::default();
        self.changes.shrink_to_fit();
    }

    /// Applies the world's `anticheat.anti-xray` rules; changed rules drop every packet.
    pub(crate) fn set_anti_xray(
        &mut self,
        rules: &vanillite_config::AntiXrayRules,
        dimension: &str,
    ) {
        let same = match &self.anti_xray {
            Some(current) => current.built_from(rules),
            None => rules.engine_mode == vanillite_config::AntiXrayMode::Disabled,
        };
        if !same {
            self.anti_xray = crate::anti_xray::AntiXray::new(rules, dimension);
            self.cache.get_mut().clear();
        }
    }

    /// Drops the packets of changed and unloaded chunks. With Anti-Xray, the masked blocks
    /// around changed blocks are added to `changed` (the tick's block updates), and changes at a
    /// chunk border also drop the neighbour's packet (its blocks may now be exposed).
    pub(crate) fn invalidate(
        &mut self,
        chunks: &mut Chunks,
        light: &[(ChunkPos, vanillite_world::chunks::LightChange)],
        changed: &mut FxHashMap<vanillite_base::SectionPos, vanillite_game::level::SectionChanges>,
    ) {
        let cache = self.cache.get_mut();
        for (pos, _) in light {
            cache.remove(pos);
        }
        let blocks = &mut self.changes;
        blocks.clear();
        chunks.take_block_changes(blocks);
        for pos in blocks.iter() {
            cache.remove(&pos.chunk());
        }
        if let Some(anti_xray) = &self.anti_xray {
            for pos in blocks.iter() {
                let chunk = pos.chunk();
                let dx = match pos.x & 15 {
                    0 => -1,
                    15 => 1,
                    _ => 0,
                };
                let dz = match pos.z & 15 {
                    0 => -1,
                    15 => 1,
                    _ => 0,
                };
                if dx != 0 {
                    cache.remove(&ChunkPos::new(chunk.x.wrapping_add(dx), chunk.z));
                }
                if dz != 0 {
                    cache.remove(&ChunkPos::new(chunk.x, chunk.z.wrapping_add(dz)));
                }
            }
            anti_xray.reveal(chunks, blocks, changed);
        }
        if cache.len() > 4096 {
            cache.retain(|pos, _| chunks.chunk(*pos).is_some());
        }
    }

    /// Drops the packet of one chunk (its block entity data changed).
    pub(crate) fn invalidate_chunk(&mut self, pos: ChunkPos) {
        self.cache.get_mut().remove(&pos);
    }

    /// The source over a world's chunks and their live block entities.
    pub(crate) fn source<'a>(
        &'a self,
        chunks: &'a Chunks,
        block_entities: &'a vanillite_game::level::BlockEntities,
    ) -> WorldChunkSource<'a> {
        WorldChunkSource {
            chunks,
            block_entities,
            cache: &self.cache,
            anti_xray: self.anti_xray.as_ref(),
        }
    }
}

/// [`ChunkSource`] over the full chunks of a world.
pub(crate) struct WorldChunkSource<'a> {
    chunks: &'a Chunks,
    block_entities: &'a vanillite_game::level::BlockEntities,
    cache: &'a RefCell<FxHashMap<ChunkPos, Arc<SharedPacket>>>,
    anti_xray: Option<&'a crate::anti_xray::AntiXray>,
}

impl ChunkSource for WorldChunkSource<'_> {
    fn is_ready(&self, pos: ChunkPos) -> bool {
        self.chunks.chunk(pos).is_some()
    }

    fn chunk_packet(&self, pos: ChunkPos) -> Option<Arc<SharedPacket>> {
        if let Some(packet) = self.cache.borrow().get(&pos) {
            return Some(Arc::clone(packet));
        }
        let chunk = self.chunks.chunk(pos)?;
        // Block entities go out with their update tag (`getUpdateTag`): live ones from the
        // level, stored ones of kinds the level does not model with their type only.
        let mut tags: Vec<(vanillite_base::BlockPos, u32, Vec<u8>)> = Vec::new();
        for (bpos, entity) in self.block_entities.in_chunk(pos) {
            let Some(id) = self.chunks.block(bpos) else {
                continue;
            };
            let state = vanillite_data::block::BlockState(id);
            let kind = state.block().info().block_entity;
            if kind == u16::MAX {
                continue;
            }
            let mut nbt = Vec::new();
            match vanillite_game::level::block_entity::update_tag(entity, state) {
                Some(tag) => {
                    if vanillite_nbt::write::write_network_compound(&tag, &mut nbt).is_err() {
                        nbt = vec![0];
                    }
                }
                None => nbt.push(0),
            }
            tags.push((bpos, u32::from(kind), nbt));
        }
        for be in &chunk.block_entities {
            if tags.iter().any(|(p, _, _)| *p == be.pos) {
                continue;
            }
            if let Some(kind) = be.kind {
                tags.push((be.pos, u32::from(kind), vec![0]));
            }
        }
        // Anti-Xray: one check per chunk; masked sections are copies.
        let masked = self
            .anti_xray
            .and_then(|anti_xray| anti_xray.mask(self.chunks, pos, &chunk.chunk));
        if let Some(sections) = &masked {
            // Block entities of masked blocks would give them away.
            let min_section = chunk.chunk.min_section();
            tags.retain(|(bpos, _, _)| {
                let index = usize::try_from((bpos.y >> 4).wrapping_sub(min_section)).ok();
                match (
                    index.and_then(|i| sections.get(i)),
                    index.and_then(|i| chunk.chunk.sections().get(i)),
                ) {
                    (Some(sent), Some(real)) => {
                        Arc::ptr_eq(sent, real) || {
                            let (x, y, z) = (
                                (bpos.x & 15) as u32,
                                (bpos.y & 15) as u32,
                                (bpos.z & 15) as u32,
                            );
                            sent.block(x, y, z) == real.block(x, y, z)
                        }
                    }
                    _ => true,
                }
            });
        }
        let block_entities: Vec<BlockEntityWire<'_>> = tags
            .iter()
            .filter_map(|(bpos, kind, nbt)| {
                Some(BlockEntityWire {
                    packed_xz: (((bpos.x & 15) << 4) | (bpos.z & 15)) as u8,
                    y: i16::try_from(bpos.y).ok()?,
                    type_id: *kind,
                    nbt,
                })
            })
            .collect();
        let mut body = Vec::with_capacity(16 * 1024);
        vanillite_protocol::codec::write_varint(&mut body, LevelChunkWithLight::ID);
        vanillite_chunk::packet::encode_chunk_with_sections(
            &mut body,
            pos.x,
            pos.z,
            &chunk.chunk,
            masked.as_deref().unwrap_or(chunk.chunk.sections()),
            &block_entities,
        );
        let packet = Arc::new(SharedPacket::new(body));
        self.cache.borrow_mut().insert(pos, Arc::clone(&packet));
        Some(packet)
    }
}

/// The simulation settings of a world from its rules, `server.properties` and the
/// `gameplay` attributes of its dimension type. Game rules keep Vanilla's defaults until
/// game rules are stored.
pub(crate) fn level_settings(
    packs: Option<&LoadedPacks>,
    info: &WorldInfo,
    kind: &str,
    rules: &vanillite_config::WorldRules,
    properties: &vanillite_config::ServerProperties,
) -> vanillite_game::level::LevelSettings {
    let attribute = |name: &str| {
        packs
            .and_then(|p| p.registry("minecraft:dimension_type"))
            .and_then(|r| r.get(kind))
            .and_then(|entry| serde_json::from_str::<serde_json::Value>(entry.json.get()).ok())
            .and_then(|json| json.get("attributes")?.get(name)?.as_bool())
            .unwrap_or(false)
    };
    let infiniburn = packs
        .and_then(|p| p.registry("minecraft:dimension_type"))
        .and_then(|r| r.get(kind))
        .and_then(|entry| serde_json::from_str::<serde_json::Value>(entry.json.get()).ok())
        .and_then(|json| Some(json.get("infiniburn")?.as_str()?.to_owned()))
        .map_or_else(Default::default, |tag| {
            vanillite_game::block::fire::Infiniburn::from_tag(&tag)
        });
    let attribute_value = |name: &str| {
        packs
            .and_then(|p| p.registry("minecraft:dimension_type"))
            .and_then(|r| r.get(kind))
            .and_then(|entry| serde_json::from_str::<serde_json::Value>(entry.json.get()).ok())
            .and_then(|json| json.get("attributes")?.get(name).cloned())
    };
    let bed_rule = |name: &str, default: vanillite_game::block::bed::BedRule| {
        attribute_value(name)
            .and_then(|v| vanillite_game::block::bed::BedRule::from_json(&v))
            .unwrap_or(default)
    };
    let mut settings = vanillite_game::level::LevelSettings {
        bed_rule: bed_rule(
            "minecraft:gameplay/bed_rule",
            vanillite_game::block::bed::BedRule::can_sleep_when_dark(),
        ),
        straw_bed_rule: bed_rule(
            "minecraft:gameplay/straw_bed_rule",
            vanillite_game::block::bed::BedRule::destroy_on_leave(),
        ),
        respawn_anchor_works: attribute("minecraft:gameplay/respawn_anchor_works"),
        dimension: info.dimension.clone(),
        infiniburn,
        fast_lava: attribute("minecraft:gameplay/fast_lava"),
        water_evaporates: attribute("minecraft:gameplay/water_evaporates"),
        snow_golem_melts: attribute("minecraft:gameplay/snow_golem_melts"),
        has_sky_light: info.has_sky_light,
        // `BaseFireBlock.inPortalDimension`.
        portal_dimension: matches!(
            info.dimension.as_str(),
            "minecraft:overworld" | "minecraft:the_nether"
        ),
        ..vanillite_game::level::LevelSettings::default()
    };
    apply_simulation_rules(
        &mut settings,
        rules,
        properties.max_chained_neighbor_updates,
    );
    settings
}

/// The exploit switches of `exploits` (decision D1).
pub(crate) fn exploit_settings(
    rules: &vanillite_config::WorldRules,
) -> vanillite_game::level::ExploitSettings {
    let e = &rules.exploits;
    vanillite_game::level::ExploitSettings {
        tnt_duplication: e.tnt_duplication,
        carpet_duplication: e.carpet_duplication,
        rail_duplication: e.rail_duplication,
        headless_pistons: e.headless_pistons,
        break_unbreakable_blocks: e.break_unbreakable_blocks,
        string_duplication: e.string_duplication,
    }
}

/// Sets the budgets of `performance.simulation`; `vanilla_max_chained` is
/// `max-chained-neighbor-updates` of `server.properties`.
pub(crate) fn apply_simulation_rules(
    settings: &mut vanillite_game::level::LevelSettings,
    rules: &vanillite_config::WorldRules,
    vanilla_max_chained: i32,
) {
    let simulation = &rules.simulation;
    settings.max_block_ticks = usize::try_from(simulation.max_block_ticks).unwrap_or(usize::MAX);
    settings.max_fluid_ticks = usize::try_from(simulation.max_fluid_ticks).unwrap_or(usize::MAX);
    settings.max_chained_neighbor_updates = match simulation.max_chained_neighbor_updates {
        vanillite_config::VanillaOr::Value(v) => i32::try_from(v).unwrap_or(i32::MAX),
        vanillite_config::VanillaOr::Vanilla => vanilla_max_chained,
    };
    settings.exploits = exploit_settings(rules);
    settings.hopper_transfer_cooldown = i32::from(rules.hoppers.transfer_cooldown);
    settings.hopper_check_cooldown = i32::from(rules.hoppers.check_cooldown);
    settings.xp_merge_radius = rules.items.xp_merge_radius;
}

/// The biomes of a world for the simulation: the biome registry, the seed's biome manager and
/// the sea level of the dimension's generator (Vanilla's -63 for flat worlds). `None` without
/// data packs.
pub(crate) fn climate(
    packs: Option<&LoadedPacks>,
    world_gen: &WorldGen,
    dimension: &str,
) -> Option<vanillite_game::block::weather::Climate> {
    let packs = packs?;
    let biomes = match vanillite_worldgen::biome::BiomeRegistry::load(packs) {
        Ok(biomes) => biomes,
        Err(e) => {
            log::warn!("{dimension}: biomes for the simulation: {e}");
            return None;
        }
    };
    let generator = world_gen.generator(dimension);
    let sea_level = generator
        .and_then(|g| g.get("settings")?.as_str())
        .and_then(|id| packs.registry("minecraft:worldgen/noise_settings")?.get(id))
        .and_then(|entry| serde_json::from_str::<serde_json::Value>(entry.json.get()).ok())
        .and_then(|json| json.get("sea_level")?.as_i64())
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(-63);
    Some(vanillite_game::block::weather::Climate {
        biomes,
        manager: vanillite_worldgen::biome::BiomeManager::for_world_seed(world_gen.seed),
        sea_level,
    })
}

/// A `SpawnerData` (`type`, `weight`, `count`) of biome or structure spawns.
fn spawn_entry(e: &serde_json::Value) -> Option<vanillite_game::mob::spawn::SpawnEntry> {
    let type_id = e
        .get("type")
        .and_then(serde_json::Value::as_str)
        .and_then(vanillite_data::entity::EntityTypeId::by_name)?;
    let int = |v: Option<&serde_json::Value>| {
        v.and_then(serde_json::Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
    };
    let weight = int(e.get("weight")).unwrap_or(1);
    let (min, max) = match e.get("count") {
        Some(serde_json::Value::Object(o)) => (
            int(o.get("min_inclusive")).unwrap_or(1),
            int(o.get("max_inclusive")).unwrap_or(1),
        ),
        other => {
            let n = int(other).unwrap_or(1);
            (n, n)
        }
    };
    Some(vanillite_game::mob::spawn::SpawnEntry {
        type_id,
        weight,
        min,
        max,
    })
}

/// The trees saplings grow into, from the data packs; `None` without them.
pub(crate) fn tree_features(
    packs: Option<&LoadedPacks>,
    seed: i64,
) -> Option<Arc<vanillite_game::block::tree::TreeFeatures>> {
    let packs = packs?;
    let biomes = vanillite_worldgen::biome::BiomeRegistry::load(packs).ok()?;
    match vanillite_game::block::tree::TreeFeatures::load(packs, &biomes, seed, -64, 384, 63) {
        Ok(trees) => Some(Arc::new(trees)),
        Err(e) => {
            log::warn!("tree features: {e}");
            None
        }
    }
}

/// The spawn settings of every biome from the data packs: `gameplay/natural_mob_spawns`,
/// `gameplay/creature_world_gen_spawn_probability` and the farm animal climate tags. `None`
/// without data packs.
pub fn spawn_table(packs: Option<&LoadedPacks>) -> Option<vanillite_game::mob::spawn::SpawnTable> {
    use vanillite_game::mob::kinds::FarmClimate;
    use vanillite_game::mob::spawn::{BiomeSpawns, Category, SpawnTable};
    let packs = packs?;
    let registry = packs.registry("minecraft:worldgen/biome")?;
    let tag = |name: &str| {
        packs
            .tag_ids("minecraft:worldgen/biome", name)
            .unwrap_or_default()
    };
    // The tags of every biome, by protocol id.
    let mut biome_tags: Vec<Vec<Box<str>>> = vec![Vec::new(); registry.entries().len()];
    if let Some(all) = packs.tags("minecraft:worldgen/biome") {
        for name in all.keys() {
            for id in tag(name) {
                if let Some(list) = usize::try_from(id).ok().and_then(|i| biome_tags.get_mut(i)) {
                    list.push(name.as_str().into());
                }
            }
        }
    }
    for list in &mut biome_tags {
        list.sort();
        list.dedup();
    }
    let warm = tag("minecraft:spawns_warm_variant_farm_animals");
    let cold = tag("minecraft:spawns_cold_variant_farm_animals");
    // An attribute value, directly or as the `argument` of a modifier.
    fn attribute<'a>(attrs: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
        let v = attrs.get(key)?;
        Some(v.get("argument").unwrap_or(v))
    }
    let mut biomes = Vec::with_capacity(registry.entries().len());
    for (index, entry) in registry.entries().iter().enumerate() {
        let id = u32::try_from(index).unwrap_or(u32::MAX);
        let json: serde_json::Value = serde_json::from_str(entry.json.get()).unwrap_or_default();
        let attrs = json.get("attributes").cloned().unwrap_or_default();
        let mut b = BiomeSpawns {
            name: entry.id.as_str().into(),
            tags: biome_tags.get(index).cloned().unwrap_or_default(),
            #[allow(clippy::cast_possible_truncation)]
            temperature: json
                .get("temperature")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.5) as f32,
            creature_probability: 0.1,
            patrol_spawn: attribute(&attrs, "minecraft:gameplay/can_pillager_patrol_spawn")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            snow_golem_melts: attribute(&attrs, "minecraft:gameplay/snow_golem_melts")
                .and_then(serde_json::Value::as_bool),
            climate: if warm.contains(&id) {
                FarmClimate::Warm
            } else if cold.contains(&id) {
                FarmClimate::Cold
            } else {
                FarmClimate::Temperate
            },
            ..BiomeSpawns::default()
        };
        if let Some(p) = attribute(
            &attrs,
            "minecraft:gameplay/creature_world_gen_spawn_probability",
        )
        .and_then(serde_json::Value::as_f64)
        {
            #[allow(clippy::cast_possible_truncation)]
            {
                b.creature_probability = p as f32;
            }
        }
        if let Some(costs) = attribute(&attrs, "minecraft:gameplay/natural_mob_spawns")
            .and_then(|s| s.get("spawn_costs"))
            .and_then(serde_json::Value::as_object)
        {
            for (name, cost) in costs {
                let Some(type_id) = vanillite_data::entity::EntityTypeId::by_name(name) else {
                    continue;
                };
                let num = |k: &str| cost.get(k).and_then(serde_json::Value::as_f64);
                if let (Some(charge), Some(budget)) = (num("charge"), num("energy_budget")) {
                    b.spawn_costs.push((type_id, charge, budget));
                }
            }
        }
        if let Some(by) = attribute(&attrs, "minecraft:gameplay/natural_mob_spawns")
            .and_then(|s| s.get("spawns_by_category"))
            .and_then(serde_json::Value::as_object)
        {
            for (category, list) in by {
                let Some(c) = Category::by_name(category) else {
                    continue;
                };
                let Some(list) = list.as_array() else {
                    continue;
                };
                let slot = &mut b.by_category[c as usize];
                slot.extend(list.iter().filter_map(spawn_entry));
            }
        }
        biomes.push(b);
    }
    let mut structures = vanillite_base::hash::FxHashMap::default();
    if let Some(registry) = packs.registry("minecraft:worldgen/structure") {
        for entry in registry.entries() {
            let json: serde_json::Value =
                serde_json::from_str(entry.json.get()).unwrap_or_default();
            let Some(overrides) = json
                .get("spawn_overrides")
                .and_then(serde_json::Value::as_object)
            else {
                continue;
            };
            let mut by_category: [Option<vanillite_game::mob::spawn::StructureOverride>; 7] =
                Default::default();
            for (category, o) in overrides {
                let Some(c) = Category::by_name(category) else {
                    continue;
                };
                let piece =
                    o.get("bounding_box").and_then(serde_json::Value::as_str) == Some("piece");
                let spawns = o
                    .get("spawns")
                    .and_then(serde_json::Value::as_array)
                    .map(|l| l.iter().filter_map(spawn_entry).collect())
                    .unwrap_or_default();
                by_category[c as usize] =
                    Some(vanillite_game::mob::spawn::StructureOverride { piece, spawns });
            }
            structures.insert(entry.id.as_str().into(), by_category);
        }
    }
    Some(SpawnTable::new(biomes, structures))
}
