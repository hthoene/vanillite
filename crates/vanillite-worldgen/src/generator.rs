//! `NoiseBasedChunkGenerator`: the generation stages of one dimension.
//!
//! A [`TerrainGenerator`] is built once per dimension from the data packs and the seed. It holds
//! the compiled density program and everything derived from the seed, is `Send + Sync` and is
//! shared by all generation tasks. The stages are pure functions of the seed and the chunk
//! position (plus, for [`Stage::Terrain`], the biomes of the neighbouring chunks), so any
//! scheduler and any number of threads produce Vanilla's result.
//!
//! The entry points are batch-shaped: [`TerrainGenerator::generate_biomes`] and
//! [`TerrainGenerator::generate_terrain`] take many chunks per call and write into
//! caller-owned chunks, so a scheduler (or a future accelerated backend) can amortise setup over
//! a batch and complete batches asynchronously on its own threads.

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use vanillite_base::ChunkPos;
use vanillite_base::random::RandomSource;
use vanillite_base::scratch::ScratchPool;
use vanillite_chunk::{BiomeId, Biomes, HeightmapKind, PalettedContainer, facts};
use vanillite_data::block::{BlockId, BlockState};
use vanillite_datapack::LoadedPacks;

use crate::Error;
use crate::anchor::WorldGenerationContext;
use crate::aquifer::{Aquifer, AquiferFunctions, GlobalFluidPicker, SurfaceLevelCache};
use crate::biome::climate::{ClimateCache, ClimateSampler};
use crate::biome::{BiomeManager, BiomeRegistry, BiomeSource};
use crate::blocks::BlockSet;
use crate::carver::{CarverId, CarverPlans, CarverRegistry, carve_chunk};
use crate::density::{ContextSampler, DensityCompiler, FnId, Program, SamplerContext};
use crate::feature::{FeatureSet, WorldGenRegion};
use crate::material::MaterialSystem;
use crate::proto::{ProtoChunk, WgHeightmap};
use crate::random::WorldgenRandom;
use crate::random_state::RandomState;
use crate::settings::NoiseGeneratorSettings;
use crate::spawn_finder::{SpawnTarget, find_spawn_chunk};
use crate::structure::{
    Beardifier, GenerationContext, LevelHeight, NeighbourStarts, PieceContext, ReferencedStarts,
    StructureId, StructurePlacement, StructureRegistry, StructureStart, StructureState,
};
use crate::template::{BoundingBox, StructureTemplateManager};
use crate::volume::DensityVolume;

/// A generation step of Vanilla's chunk pyramid that this crate implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    /// `structure_starts`: the structure starts whose start chunk this is.
    StructureStarts,
    /// `structure_references`: the starts of the 17×17 chunks around that reach this chunk.
    StructureReferences,
    /// `biomes`: noise biomes from the climate functions.
    Biomes,
    /// `terrain`: noise fill with aquifers, material rules (surface, bedrock, ore veins) and
    /// carvers.
    Terrain,
    /// `features`.
    Features,
}

impl Stage {
    /// All stages in pyramid order.
    pub const ALL: [Self; 5] = [
        Self::StructureStarts,
        Self::StructureReferences,
        Self::Biomes,
        Self::Terrain,
        Self::Features,
    ];

    /// Chebyshev radius of neighbours that must have reached the previous stage
    /// (`ChunkStep.addRequirement` of the previous status in `GENERATION_PYRAMID`). See
    /// [`Self::requirements`] for all requirements, including the structure starts.
    pub const fn dependency_radius(self) -> u32 {
        match self {
            Self::StructureStarts | Self::Biomes => 0,
            Self::StructureReferences => 8,
            Self::Terrain | Self::Features => 1,
        }
    }

    /// Every requirement of the stage (`ChunkStep.addRequirement(status, radius)` in Vanilla's
    /// `GENERATION_PYRAMID`): the neighbours within the Chebyshev radius must have reached the
    /// stage. Every stage after the starts needs the starts of radius 8: the references read
    /// them, and the terrain (`Beardifier`) and features (structure placement) stages read the
    /// starts the chunk references.
    pub const fn requirements(self) -> &'static [(Stage, u32)] {
        match self {
            Self::StructureStarts => &[],
            Self::StructureReferences | Self::Biomes => &[(Self::StructureStarts, 8)],
            Self::Terrain => &[(Self::StructureStarts, 8), (Self::Biomes, 1)],
            Self::Features => &[(Self::StructureStarts, 8), (Self::Terrain, 1)],
        }
    }

    /// Chebyshev radius of chunks the stage writes blocks into (`blockStateWriteRadius`).
    pub const fn write_radius(self) -> u32 {
        match self {
            Self::Features => 1,
            _ => 0,
        }
    }
}

/// The dimension a generator is built for: its `worldgen/noise_settings` id, its biome source
/// (the `biome_source` object of the dimension's generator) and the height of its levels.
#[derive(Clone, Copy, Debug)]
pub struct DimensionSpec<'a> {
    /// `worldgen/noise_settings` id, e.g. `minecraft:overworld`.
    pub noise_settings: &'a str,
    /// The generator's `biome_source` JSON.
    pub biome_source: &'a Value,
    /// Lowest block Y of the level (dimension type `min_y`).
    pub min_y: i32,
    /// Number of block rows of the level (dimension type `height`).
    pub height: i32,
    /// Whether the dimension has sky light (dimension type `has_skylight`).
    pub has_sky_light: bool,
}

/// The noise biomes of a 3×3 chunk area around a chunk (row-major, `dz` outer, `dx` inner, the
/// centre at index 4), as the terrain stage reads them from its neighbours.
#[derive(Clone, Debug)]
pub struct NeighbourBiomes {
    min_section: i32,
    chunks: [Vec<PalettedContainer<Biomes>>; 9],
}

impl NeighbourBiomes {
    /// Collects the biomes of the 3×3 chunks around `center`; `chunk(dx, dz)` returns the
    /// neighbour at that offset (offsets in `-1..=1`), which must have finished [`Stage::Biomes`].
    pub fn collect<'a>(mut chunk: impl FnMut(i32, i32) -> &'a ProtoChunk) -> Self {
        let mut min_section = 0;
        let chunks = std::array::from_fn(|i| {
            let (dx, dz) = (i as i32 % 3 - 1, i as i32 / 3 - 1);
            let c = chunk(dx, dz);
            min_section = c.min_y() >> 4;
            c.sections().iter().map(|s| s.biomes().clone()).collect()
        });
        Self {
            min_section,
            chunks,
        }
    }

    /// The biomes of the 3×3 chunks around a chunk: each chunk's biome containers, bottom
    /// section first, in the order of [`Self::collect`]; `min_section` is the lowest section Y.
    pub fn new(min_section: i32, chunks: [Vec<PalettedContainer<Biomes>>; 9]) -> Self {
        Self {
            min_section,
            chunks,
        }
    }

    /// `ChunkAccess.getNoiseBiome` of the chunk containing the quart, for quarts of the 3×3
    /// area around `center`.
    fn noise_biome(&self, center: ChunkPos, quart_x: i32, quart_y: i32, quart_z: i32) -> BiomeId {
        let dx = (quart_x >> 2) - center.x;
        let dz = (quart_z >> 2) - center.z;
        debug_assert!((-1..=1).contains(&dx) && (-1..=1).contains(&dz));
        let sections = &self.chunks[((dz + 1) * 3 + dx + 1) as usize];
        let min_quart = self.min_section << 2;
        let max_quart = min_quart + (sections.len() as i32) * 4 - 1;
        let qy = quart_y.clamp(min_quart, max_quart);
        let section = ((qy >> 2) - self.min_section) as usize;
        sections[section].get_at((quart_x & 3) as u32, (qy & 3) as u32, (quart_z & 3) as u32)
    }

    /// `collectPossibleBiomes(region, 1)`: every biome in the 3×3 area, sorted.
    fn possible_biomes(&self) -> Vec<BiomeId> {
        let mut out = Vec::new();
        for container in self.chunks.iter().flatten() {
            container.for_each_count(|biome, count| {
                if count > 0 && !out.contains(&biome) {
                    out.push(biome);
                }
            });
        }
        out.sort_unstable();
        out
    }
}

/// What [`TerrainGenerator::generate_terrain_observed`] calls after each terrain step.
type Observer<'a> = &'a mut dyn FnMut(TerrainStep, &ProtoChunk);

/// The three steps of [`Stage::Terrain`], in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerrainStep {
    /// `doFill`: final density and aquifers.
    Noise,
    /// `buildSurface`: material rules (surface, bedrock, ore veins, underground bands).
    Surface,
    /// `generateCarvers` and `applyCarvingMask`.
    Carvers,
}

/// One chunk of a terrain batch: the chunk (at [`Stage::Biomes`]), its neighbours' biomes and
/// the structure starts it references (for the `Beardifier`; [`ReferencedStarts::empty`]
/// without structures).
#[derive(Debug)]
pub struct TerrainJob {
    pub chunk: ProtoChunk,
    pub neighbours: NeighbourBiomes,
    pub structures: ReferencedStarts,
}

/// Vanilla's `NoiseBasedChunkGenerator` with its `RandomState` for one dimension and seed.
#[derive(Debug)]
pub struct TerrainGenerator {
    seed: i64,
    level_min_y: i32,
    level_height: i32,
    has_sky_light: bool,
    settings: NoiseGeneratorSettings,
    random_state: RandomState,
    program: Program,
    final_density: FnId,
    climate: ClimateSampler,
    aquifer: Option<AquiferFunctions>,
    /// The surface levels the aquifers of all chunks share.
    surface_levels: Arc<SurfaceLevelCache>,
    fluid_picker: GlobalFluidPicker,
    material: MaterialSystem,
    biomes: BiomeRegistry,
    biome_source: BiomeSource,
    biome_manager: BiomeManager,
    carvers: CarverRegistry,
    /// The carvers' ellipsoids per source chunk, shared by the chunks they carve.
    carver_plans: CarverPlans,
    /// The buffers of the terrain jobs, reused from job to job ([`Self::release_scratch`]).
    terrain_scratch: ScratchPool<TerrainScratch>,
    /// Carvers per biome id.
    biome_carvers: Vec<Vec<CarverId>>,
    /// Climates of the columns the carvers look up their biomes at.
    carver_climates: ClimateCache,
    uncarvable: BlockSet,
    features: FeatureSet,
    structures: StructureRegistry,
    /// The template cache the features and structures read through.
    templates: Arc<StructureTemplateManager>,
    grass_block: BlockId,
    mycelium: BlockId,
    dirt: BlockId,
    /// The compiled `spawn_target` of the settings.
    spawn_target: Vec<SpawnTarget>,
    /// `getOrigin(randomState)`, computed on first use.
    origin: OnceLock<ChunkPos>,
    /// The layers of a flat generator (`FlatLevelSource`); `None` for the noise generator.
    flat: Option<FlatTerrain>,
}

/// `FlatLevelGeneratorSettings`: the `settings` of a `minecraft:flat` generator.
#[derive(Clone, Debug)]
pub struct FlatSettings {
    /// `layers`, one state per block row from the bottom of the level (`getLayers()`).
    pub layers: Vec<BlockState>,
    /// `biome` (default `minecraft:plains`).
    pub biome: String,
    /// `lakes`.
    pub lakes: bool,
    /// `features`.
    pub features: bool,
    /// `structure_overrides`: structure set ids and `#` tags; `None` for every set.
    pub structure_overrides: Option<Vec<String>>,
}

impl FlatSettings {
    /// `FlatLevelGeneratorSettings.getDefault`: bedrock, two dirt, grass block in plains, with
    /// villages and strongholds.
    pub fn classic() -> Self {
        let state = |id: BlockId| id.default_state();
        use vanillite_data::block::ids;
        Self {
            layers: vec![
                state(ids::BEDROCK),
                state(ids::DIRT),
                state(ids::DIRT),
                state(ids::GRASS_BLOCK),
            ],
            biome: "minecraft:plains".to_owned(),
            lakes: false,
            features: false,
            structure_overrides: Some(vec![
                "minecraft:strongholds".to_owned(),
                "minecraft:villages".to_owned(),
            ]),
        }
    }

    /// Parses the settings as `FlatLevelGeneratorSettings.CODEC` reads them.
    pub fn from_json(json: &Value) -> Result<Self, Error> {
        let mut layers = Vec::new();
        for layer in json
            .get("layers")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::new("flat settings: no layers"))?
        {
            let name = layer
                .get("block")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::new(format!("flat layer without block: {layer}")))?;
            let block = BlockId::by_name(&crate::blocks::qualify(name))
                .ok_or_else(|| Error::new(format!("flat layer: unknown block {name}")))?;
            let height = layer
                .get("height")
                .and_then(Value::as_u64)
                .filter(|h| *h <= 4064)
                .ok_or_else(|| Error::new(format!("flat layer: bad height in {layer}")))?;
            layers.extend(std::iter::repeat_n(block.default_state(), height as usize));
        }
        // `validateHeight`: the layers fit `DimensionType.Y_SIZE`.
        if layers.len() > 4064 {
            return Err(Error::new("flat settings: layers higher than 4064"));
        }
        let flag = |name: &str| json.get(name).and_then(Value::as_bool).unwrap_or(false);
        let structure_overrides = match json.get("structure_overrides") {
            None => None,
            Some(Value::String(name)) => Some(vec![name.clone()]),
            Some(Value::Array(names)) => Some(
                names
                    .iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect(),
            ),
            Some(other) => {
                return Err(Error::new(format!(
                    "flat settings: unsupported structure_overrides {other}"
                )));
            }
        };
        Ok(Self {
            layers,
            biome: json
                .get("biome")
                .and_then(Value::as_str)
                .map_or_else(|| "minecraft:plains".to_owned(), crate::blocks::qualify),
            lakes: flag("lakes"),
            features: flag("features"),
            structure_overrides,
        })
    }
}

/// The layers of a flat generator after `adjustGenerationSettings`: layers that do not block
/// motion are `None` (the `fill_layer` feature places them).
#[derive(Debug)]
struct FlatTerrain {
    layers: Vec<Option<BlockState>>,
}

impl FlatTerrain {
    /// `FlatLevelSource.getBaseHeight`.
    fn base_height(&self, heightmap: HeightmapKind, level: LevelHeight) -> i32 {
        let top = (self.layers.len() as i32 - 1).min(level.max_y());
        for index in (0..=top).rev() {
            if let Some(state) = self.layers[index as usize]
                && heightmap.matches(facts::block_flags(state.0))
            {
                return level.min_y + index + 1;
            }
        }
        level.min_y
    }

    /// `FlatLevelSource.buildTerrain`.
    fn build(&self, chunk: &mut ProtoChunk) {
        let rows = self.layers.len().min(chunk.height() as usize);
        for (index, layer) in self.layers[..rows].iter().enumerate() {
            let Some(state) = *layer else {
                continue;
            };
            let y = chunk.min_y() + index as i32;
            for z in 0..16 {
                for x in 0..16 {
                    chunk.set_block(x, y, z, state);
                }
            }
        }
        chunk.finish_terrain();
    }
}

/// What a generator searched for on first use. A generator built again for the same world
/// takes it over ([`TerrainGenerator::restore`]) instead of searching again.
#[derive(Clone, Debug, Default)]
pub struct TerrainMemo {
    origin: Option<ChunkPos>,
    /// The ring positions of concentric rings placements, by structure set.
    rings: Vec<(usize, Vec<ChunkPos>)>,
}

impl TerrainGenerator {
    /// What this generator has searched for so far.
    pub fn memo(&self) -> TerrainMemo {
        let rings = self.structures.sets().iter().enumerate();
        TerrainMemo {
            origin: self.origin.get().copied(),
            rings: rings
                .filter_map(|(index, set)| match &set.placement {
                    StructurePlacement::ConcentricRings { rings, .. } => {
                        Some((index, rings.positions.get()?.clone()))
                    }
                    _ => None,
                })
                .collect(),
        }
    }

    /// Takes over what a generator of the same packs, dimension and seed has searched for.
    pub fn restore(&self, memo: &TerrainMemo) {
        if let Some(origin) = memo.origin {
            let _ = self.origin.set(origin);
        }
        for (index, positions) in &memo.rings {
            if let Some(StructurePlacement::ConcentricRings { rings, .. }) =
                self.structures.sets().get(*index).map(|set| &set.placement)
            {
                let _ = rings.positions.set(positions.clone());
            }
        }
    }

    /// Builds the generator: parses the dimension's settings, derives the random state from
    /// `seed` and compiles every density function the stages use into one program.
    pub fn new(
        packs: &LoadedPacks,
        dimension: DimensionSpec<'_>,
        seed: i64,
    ) -> Result<Self, Error> {
        let templates = Arc::new(StructureTemplateManager::new(packs));
        Self::with_templates(packs, dimension, seed, templates)
    }

    /// [`Self::new`] reading structure templates through `templates`: the server's one cache
    /// (Vanilla's `StructureTemplateManager`, which also sees the world's `generated`
    /// templates) instead of an own one over the packs.
    pub fn with_templates(
        packs: &LoadedPacks,
        dimension: DimensionSpec<'_>,
        seed: i64,
        templates: Arc<StructureTemplateManager>,
    ) -> Result<Self, Error> {
        let settings = NoiseGeneratorSettings::load(packs, dimension.noise_settings)?;
        Self::build(packs, dimension, settings, seed, templates, None)
    }

    /// A flat generator (`FlatLevelSource`) of `flat` for a level of that height, reading
    /// structure templates through `templates`.
    pub fn flat_with_templates(
        packs: &LoadedPacks,
        flat: &FlatSettings,
        min_y: i32,
        height: i32,
        has_sky_light: bool,
        seed: i64,
        templates: Arc<StructureTemplateManager>,
    ) -> Result<Self, Error> {
        // What `FlatLevelSource` reports instead of noise settings: sea level -63, min Y 0,
        // generation depth 384, origin chunk 0, 0, no mobs at generation. The rest (no noise,
        // no aquifers, no surface) is never sampled.
        let mut json: Value = packs
            .registry("minecraft:worldgen/noise_settings")
            .and_then(|r| r.get("minecraft:overworld"))
            .and_then(|e| serde_json::from_str(e.json.get()).ok())
            .ok_or_else(|| Error::new("flat generator: no overworld noise settings"))?;
        if let Some(router) = json.get_mut("noise_router").and_then(Value::as_object_mut) {
            for function in router.values_mut() {
                *function = Value::from(0.0);
            }
        }
        if let Some(object) = json.as_object_mut() {
            object.remove("aquifers");
            object.insert("spawn_target".into(), Value::Array(Vec::new()));
            object.insert("sea_level".into(), Value::from(-63));
            object.insert("disable_mob_generation".into(), Value::Bool(true));
            object.insert(
                "noise".into(),
                serde_json::json!({"min_y": 0, "height": 384}),
            );
            object.insert(
                "material_rule".into(),
                serde_json::json!({"type": "minecraft:block", "result_state": "minecraft:air"}),
            );
        }
        let settings = NoiseGeneratorSettings::from_json(
            &json,
            &mut crate::density::DensityParser::new(packs),
        )?;
        let biome_source = serde_json::json!({"type": "minecraft:fixed", "biome": flat.biome});
        let dimension = DimensionSpec {
            noise_settings: "minecraft:overworld",
            biome_source: &biome_source,
            min_y,
            height,
            has_sky_light,
        };
        Self::build(packs, dimension, settings, seed, templates, Some(flat))
    }

    fn build(
        packs: &LoadedPacks,
        dimension: DimensionSpec<'_>,
        settings: NoiseGeneratorSettings,
        seed: i64,
        templates: Arc<StructureTemplateManager>,
        flat: Option<&FlatSettings>,
    ) -> Result<Self, Error> {
        let random_state = RandomState::new(packs, seed, settings.legacy_random_source)?;
        let biomes = BiomeRegistry::load(packs)?;
        let biome_source = BiomeSource::from_json(dimension.biome_source, packs, &biomes)?;
        let carvers = CarverRegistry::load(packs)?;
        let biome_carvers = biomes
            .all()
            .iter()
            .map(|b| carvers.ids(&b.carvers))
            .collect::<Result<Vec<_>, Error>>()?;

        let mut compiler = DensityCompiler::new(&random_state);
        let router = &settings.router;
        let climate = ClimateSampler {
            temperature: compiler.compile(&router.temperature),
            humidity: compiler.compile(&router.vegetation),
            continentalness: compiler.compile(&router.continents),
            erosion: compiler.compile(&router.erosion),
            depth: compiler.compile(&router.depth),
            weirdness: compiler.compile(&router.ridges),
        };
        let final_density = compiler.compile(&router.final_density);
        let spawn_target = settings
            .spawn_target
            .iter()
            .map(|point| {
                point
                    .parameters
                    .iter()
                    .map(|(function, parameter)| (compiler.compile(function), *parameter))
                    .collect()
            })
            .collect();
        let aquifer = settings
            .aquifers
            .as_ref()
            .map(|config| AquiferFunctions::compile(config, &mut compiler));
        let material =
            MaterialSystem::new(packs, &settings, &random_state, &biomes, &mut compiler)?;
        let mut program = compiler.finish();
        // The final density is only sampled as whole volumes.
        program.set_masked_cache_root(final_density);

        // `adjustGenerationSettings`: layers that do not block motion become `fill_layer`
        // features (the layer list itself changes, as Vanilla's `validate()` does at start).
        let flat_terrain = flat.map(|flat| FlatTerrain {
            layers: flat
                .layers
                .iter()
                .map(|&state| {
                    HeightmapKind::MotionBlocking
                        .matches(facts::block_flags(state.0))
                        .then_some(state)
                })
                .collect(),
        });
        let flat_features = match (flat, &biome_source) {
            (Some(flat), BiomeSource::Fixed(biome)) => Some(crate::feature::FlatFeatures {
                biome: *biome,
                lakes: flat.lakes,
                decoration: flat.features
                    && (!flat.layers.iter().all(|s| s.is_air())
                        || biomes.get(*biome).name == "minecraft:the_void"),
                fill_layers: flat
                    .layers
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        !HeightmapKind::MotionBlocking.matches(facts::block_flags(s.0))
                    })
                    .map(|(i, &s)| (i as i32, s))
                    .collect(),
            }),
            _ => None,
        };
        let features = FeatureSet::load_with(
            packs,
            &biomes,
            &random_state,
            &biome_source.possible_biomes(),
            WorldGenerationContext::for_settings(&settings, dimension.min_y, dimension.height),
            &templates,
            flat_features.as_ref(),
        )?;

        let mut structures = StructureRegistry::load(
            packs,
            &biomes,
            &features,
            &biome_source.possible_biomes(),
            &templates,
        )?;
        if let Some(names) = flat.and_then(|f| f.structure_overrides.as_ref()) {
            structures.restrict_possible_sets(
                names,
                packs.tags("minecraft:worldgen/structure_set"),
                &biome_source.possible_biomes(),
            );
        }

        let block = |name: &str| {
            BlockId::by_name(name).ok_or_else(|| Error::new(format!("unknown block {name}")))
        };
        Ok(Self {
            seed,
            level_min_y: dimension.min_y,
            level_height: dimension.height,
            has_sky_light: dimension.has_sky_light,
            fluid_picker: GlobalFluidPicker::from_settings(&settings),
            settings,
            program,
            final_density,
            climate,
            aquifer,
            surface_levels: Arc::default(),
            material,
            biome_manager: BiomeManager::for_world_seed(seed),
            biomes,
            biome_source,
            carvers,
            carver_plans: CarverPlans::default(),
            terrain_scratch: ScratchPool::default(),
            biome_carvers,
            carver_climates: ClimateCache::new(),
            uncarvable: BlockSet::from_tag(packs, "minecraft:uncarvable"),
            features,
            structures,
            templates,
            grass_block: block("minecraft:grass_block")?,
            mycelium: block("minecraft:mycelium")?,
            dirt: block("minecraft:dirt")?,
            spawn_target,
            origin: OnceLock::new(),
            random_state,
            flat: flat_terrain,
        })
    }

    /// The concentric rings seed of `ChunkGeneratorStructureState`: the level seed, 0 for flat
    /// generators (`createForFlat`).
    pub fn concentric_rings_seed(&self) -> i64 {
        if self.flat.is_some() { 0 } else { self.seed }
    }

    /// Whether this is a flat generator (`FlatLevelSource`).
    pub fn is_flat(&self) -> bool {
        self.flat.is_some()
    }

    /// `getOrigin(randomState)`: the chunk `NoiseSpawnFinder` picks for the settings'
    /// `spawn_target`, or chunk 0, 0 without one.
    pub fn origin(&self) -> ChunkPos {
        *self.origin.get_or_init(|| {
            if self.spawn_target.is_empty() {
                ChunkPos::new(0, 0)
            } else {
                find_spawn_chunk(&self.program, &self.spawn_target)
            }
        })
    }

    /// The world seed.
    pub fn seed(&self) -> i64 {
        self.seed
    }

    /// The noise settings.
    pub fn settings(&self) -> &NoiseGeneratorSettings {
        &self.settings
    }

    /// The biome registry the generator writes ids of.
    pub fn biomes(&self) -> &BiomeRegistry {
        &self.biomes
    }

    /// The compiled density program (for other backends and debugging).
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// The seed-derived random state.
    pub fn random_state(&self) -> &RandomState {
        &self.random_state
    }

    /// A new, empty chunk with the level's height (`ProtoChunk` at status `empty`).
    pub fn new_chunk(&self, pos: ChunkPos) -> ProtoChunk {
        let plains = self.biomes.id_of("minecraft:plains").unwrap_or(0);
        ProtoChunk::new(pos, self.level_min_y, self.level_height, plains)
    }

    /// The structure template cache of this generator.
    pub fn templates(&self) -> &Arc<StructureTemplateManager> {
        &self.templates
    }

    /// The parsed features and their global order.
    pub fn features(&self) -> &FeatureSet {
        &self.features
    }

    /// The parsed structures and structure sets.
    pub fn structures(&self) -> &StructureRegistry {
        &self.structures
    }

    /// The biome source.
    pub fn biome_source(&self) -> &BiomeSource {
        &self.biome_source
    }

    /// The climate functions of the biome source (`Climate.Sampler`).
    pub fn climate(&self) -> &ClimateSampler {
        &self.climate
    }

    /// `getSeaLevel()`.
    pub fn sea_level(&self) -> i32 {
        self.settings.sea_level
    }

    /// `getMinY()`: the noise settings' lowest Y.
    pub fn min_y(&self) -> i32 {
        self.settings.min_y
    }

    /// `getGenDepth()`: the noise settings' height.
    pub fn gen_depth(&self) -> i32 {
        self.settings.height
    }

    /// The `WorldGenerationContext` of the generator for a level height range.
    pub fn generation_context(&self, level: LevelHeight) -> WorldGenerationContext {
        WorldGenerationContext::for_settings(&self.settings, level.min_y, level.height)
    }

    /// Runs `stage` on every chunk of `batch` that needs no neighbours. Panics for the stages
    /// that need neighbours (use [`Self::generate_structure_references`],
    /// [`Self::generate_terrain`] and [`Self::generate_features`]).
    pub fn generate(&self, stage: Stage, batch: &mut [ProtoChunk]) {
        match stage {
            Stage::StructureStarts => self.generate_structure_starts(batch),
            Stage::StructureReferences => {
                panic!(
                    "the structure references stage needs neighbour starts: use generate_structure_references"
                )
            }
            Stage::Biomes => self.generate_biomes(batch),
            Stage::Terrain => {
                panic!("the terrain stage needs neighbour biomes: use generate_terrain")
            }
            Stage::Features => {
                panic!("the features stage needs neighbour chunks: use generate_features")
            }
        }
    }

    /// [`Stage::Features`] for one chunk (`applyBiomeDecoration`): `area` holds the 3×3 chunks
    /// around it (row-major, `dz` outer, `dx` inner, the chunk to decorate in the middle), all
    /// past [`Stage::Terrain`]. Features write into the neighbours too, so two chunks whose
    /// areas overlap must not be decorated at the same time, and the order in which neighbouring
    /// chunks are decorated changes the result at chunk borders, exactly as in Vanilla.
    ///
    /// `structures` are the starts the middle chunk references ([`ReferencedStarts::collect`]);
    /// their pieces are placed before the features of their decoration step. With
    /// [`ReferencedStarts::empty`] this is decoration with structures disabled.
    pub fn generate_features(&self, area: [&mut ProtoChunk; 9], structures: &ReferencedStarts) {
        let mut region = self.feature_region(area.into());
        region.set_structure_starts(structures.clone());
        let center = region.center();
        let (min_y, height) = (region.min_y(), region.height());
        self.features
            .decorate(&mut region, &mut |region, random, decoration_seed, step| {
                for (index, &structure) in self.structures.by_step(step).iter().enumerate() {
                    random.set_feature_seed(decoration_seed, index as i32, step as i32);
                    for start in structures.for_structure(structure) {
                        // `getWritableArea(chunk)`, fresh for every start.
                        let mut chunk_bb = writable_area(center, min_y, height);
                        let mut ctx = PieceContext {
                            level: region,
                            random,
                            generator: self,
                            keep_jigsaws: false,
                        };
                        start.place_in_chunk(&self.structures, &mut ctx, &mut chunk_bb, center);
                    }
                }
            });
    }

    /// [`Stage::StructureStarts`] for a batch (`ChunkGenerator.createStructures` of fresh
    /// chunks: no chunk has a start yet, so every start is generated).
    pub fn generate_structure_starts(&self, batch: &mut [ProtoChunk]) {
        for chunk in batch {
            self.create_structures(chunk);
        }
    }

    /// `createStructures(registryAccess, state, structureManager, chunk, templates, level)`.
    fn create_structures(&self, chunk: &mut ProtoChunk) {
        let state = StructureState::new(self);
        let pos = chunk.pos();
        let level = LevelHeight {
            min_y: chunk.min_y(),
            height: chunk.height(),
        };
        let seed = state.level_seed();
        for &set_id in state.possible_structure_sets() {
            let set = self.structures.set(set_id);
            if set
                .structures
                .iter()
                .any(|(id, _)| chunk.start_for_structure(*id).is_some())
            {
                continue;
            }
            if !set.placement.is_structure_chunk(&state, pos.x, pos.z) {
                continue;
            }
            if let [(structure, _)] = set.structures.as_slice() {
                self.try_generate_structure(*structure, chunk, level);
                continue;
            }
            let mut options = set.structures.clone();
            let mut random = WorldgenRandom::new(0);
            random.set_large_feature_seed(seed, pos.x, pos.z);
            let mut total: i32 = options.iter().map(|(_, weight)| weight).sum();
            while !options.is_empty() {
                let mut choice = random.random.next_int_bounded(total);
                let mut index = 0;
                for (_, weight) in &options {
                    choice -= weight;
                    if choice < 0 {
                        break;
                    }
                    index += 1;
                }
                let (structure, weight) = options[index];
                if self.try_generate_structure(structure, chunk, level) {
                    break;
                }
                options.remove(index);
                total -= weight;
            }
        }
    }

    /// The structure of `set_id` that `createStructures` starts in the chunk at `pos`, if any:
    /// the set's placement, then its weighted pick, each pick tried with a full generation of
    /// its pieces (`/locate`'s `getStructureGeneratingAt` without stored chunks).
    pub fn structure_start_in(
        &self,
        state: &StructureState<'_>,
        set_id: crate::structure::StructureSetId,
        pos: ChunkPos,
        level: LevelHeight,
    ) -> Option<StructureId> {
        let set = self.structures.set(set_id);
        if !set.placement.is_structure_chunk(state, pos.x, pos.z) {
            return None;
        }
        let valid = |id: StructureId| {
            let structure = self.structures.structure(id);
            let mut ctx = GenerationContext::new(self, pos, level, &structure.settings.biomes);
            structure.generate_pieces(&mut ctx).is_some_and(|pieces| {
                StructureStart::new(id, pos, 0, pieces, |bb| structure.adjust_bounding_box(bb))
                    .is_some()
            })
        };
        if let [(structure, _)] = set.structures.as_slice() {
            return valid(*structure).then_some(*structure);
        }
        let mut options = set.structures.clone();
        let mut random = WorldgenRandom::new(0);
        random.set_large_feature_seed(state.level_seed(), pos.x, pos.z);
        let mut total: i32 = options.iter().map(|(_, weight)| weight).sum();
        while !options.is_empty() {
            let mut choice = random.random.next_int_bounded(total);
            let mut index = 0;
            for (_, weight) in &options {
                choice -= weight;
                if choice < 0 {
                    break;
                }
                index += 1;
            }
            let (structure, weight) = options[index];
            if valid(structure) {
                return Some(structure);
            }
            options.remove(index);
            total -= weight;
        }
        None
    }

    /// `tryGenerateStructure(...)`: whether a valid start was generated and stored.
    fn try_generate_structure(
        &self,
        id: StructureId,
        chunk: &mut ProtoChunk,
        level: LevelHeight,
    ) -> bool {
        let structure = self.structures.structure(id);
        let pos = chunk.pos();
        let references = chunk
            .start_for_structure(id)
            .map_or(0, |start| start.references());
        let mut ctx = GenerationContext::new(self, pos, level, &structure.settings.biomes);
        let Some(pieces) = structure.generate_pieces(&mut ctx) else {
            return false;
        };
        let Some(start) = StructureStart::new(id, pos, references, pieces, |bb| {
            structure.adjust_bounding_box(bb)
        }) else {
            return false;
        };
        chunk.set_start_for_structure(id, Arc::new(start));
        true
    }

    /// [`Stage::StructureReferences`] for one chunk (`createReferences`): records every start
    /// of the 17×17 chunks around it whose box reaches the chunk.
    pub fn generate_structure_references(&self, chunk: &mut ProtoChunk, starts: &NeighbourStarts) {
        let pos = chunk.pos();
        debug_assert_eq!(starts.center(), pos, "starts of another chunk");
        let (min_x, min_z) = (pos.min_block_x(), pos.min_block_z());
        for (source, source_starts) in starts.iter() {
            let key = source.as_long();
            for (structure, start) in source_starts {
                if start
                    .bounding_box()
                    .intersects_xz(min_x, min_z, min_x + 15, min_z + 15)
                {
                    chunk.add_reference(*structure, key);
                }
            }
        }
    }

    /// `getBaseHeight(x, z, heightmap, heightAccessor, randomState)`: the first free Y above the
    /// highest block of the noise column that matches the heightmap, or the level's lowest Y.
    pub fn base_height(&self, x: i32, z: i32, heightmap: HeightmapKind, level: LevelHeight) -> i32 {
        if let Some(flat) = &self.flat {
            return flat.base_height(heightmap, level);
        }
        self.iterate_noise_column(level, x, z, None, Some(heightmap))
            .unwrap_or(level.min_y)
    }

    /// `getFirstFreeHeight(...)`.
    pub fn first_free_height(
        &self,
        x: i32,
        z: i32,
        heightmap: HeightmapKind,
        level: LevelHeight,
    ) -> i32 {
        self.base_height(x, z, heightmap, level)
    }

    /// `getFirstOccupiedHeight(...)`.
    pub fn first_occupied_height(
        &self,
        x: i32,
        z: i32,
        heightmap: HeightmapKind,
        level: LevelHeight,
    ) -> i32 {
        self.base_height(x, z, heightmap, level) - 1
    }

    /// `getBaseColumn(x, z, heightAccessor, randomState)`: the column's blocks as the noise
    /// step fills them (no surface, no carvers).
    pub fn base_column(&self, x: i32, z: i32, level: LevelHeight) -> NoiseColumn {
        if let Some(flat) = &self.flat {
            let states = flat
                .layers
                .iter()
                .take(level.height.max(0) as usize)
                .map(|layer| layer.unwrap_or(BlockState::AIR))
                .collect();
            return NoiseColumn {
                min_y: level.min_y,
                states,
            };
        }
        let mut states = Vec::new();
        let min_y = self.iterate_noise_column_into(level, x, z, &mut states);
        NoiseColumn { min_y, states }
    }

    fn iterate_noise_column_into(
        &self,
        level: LevelHeight,
        x: i32,
        z: i32,
        states: &mut Vec<BlockState>,
    ) -> i32 {
        self.iterate_noise_column(level, x, z, Some(states), None);
        self.settings.min_y.max(level.min_y)
    }

    /// `iterateNoiseColumn(heightAccessor, randomState, x, z, column, tester)`: samples the
    /// final density and the aquifer of one column top-down (a fresh `NoiseChunk` without
    /// structures); fills `column` bottom-up and returns the first Y above the highest state
    /// matching `tester`.
    fn iterate_noise_column(
        &self,
        level: LevelHeight,
        x: i32,
        z: i32,
        mut column: Option<&mut Vec<BlockState>>,
        tester: Option<HeightmapKind>,
    ) -> Option<i32> {
        let min_y = self.settings.min_y.max(level.min_y);
        let height = (self.settings.min_y + self.settings.height).min(level.max_y() + 1) - min_y;
        if height <= 0 {
            return None;
        }
        let volume = DensityVolume::new(1, height, 1, x, min_y, z);
        if let Some(column) = column.as_deref_mut() {
            column.clear();
            column.resize(height as usize, BlockState::AIR);
        }
        let mut ctx = SamplerContext::new(true);
        let mut aquifer = Aquifer::for_chunk(
            self.aquifer.as_ref(),
            &self.random_state,
            &volume,
            self.fluid_picker,
            &self.program,
            &mut ctx,
            Some(&self.surface_levels),
        );
        let mut density = vec![0.0_f32; volume.len()];
        self.program
            .sample_volume(&mut ctx, self.final_density, &mut density, &volume);
        for y in (0..height).rev() {
            let block_y = volume.block_y(y);
            let value = density[volume.index(0, y, 0)];
            let state = aquifer
                .compute_substance(&self.program, &mut ctx, x, block_y, z, f64::from(value))
                .unwrap_or(self.settings.default_block);
            if let Some(column) = column.as_deref_mut() {
                column[y as usize] = state;
            }
            if let Some(kind) = tester
                && kind.matches(facts::block_flags(state.0))
            {
                return Some(block_y + 1);
            }
        }
        None
    }

    /// The `WorldGenRegion` of the features stage over `chunks` (3×3, row-major, `dz` outer),
    /// set up for this dimension.
    pub fn feature_region<'a>(&self, chunks: Vec<&'a mut ProtoChunk>) -> WorldGenRegion<'a> {
        self.region(chunks, 1)
    }

    /// A `WorldGenRegion` over `chunks` (`2 * radius + 1` square, row-major, `dz` outer) that
    /// may write anywhere in it, set up for this dimension.
    pub fn region<'a>(&self, chunks: Vec<&'a mut ProtoChunk>, radius: i32) -> WorldGenRegion<'a> {
        let mut region = WorldGenRegion::new(
            chunks,
            radius,
            radius,
            &self.random_state,
            self.settings.sea_level,
        );
        region.set_generation_context(self.features.env.context);
        region.set_has_sky_light(self.has_sky_light);
        region
    }

    /// [`Stage::Biomes`] for a batch (`ChunkGenerator.createBiomes`).
    pub fn generate_biomes(&self, batch: &mut [ProtoChunk]) {
        for chunk in batch {
            self.biome_source
                .create_biomes(chunk, &self.program, &self.climate);
        }
    }

    /// [`Stage::Terrain`] for a batch (`NoiseBasedChunkGenerator.buildTerrain`). One sampler
    /// context is reused across the batch; it is reset per chunk, as Vanilla creates a fresh
    /// `NoiseChunk` per chunk.
    pub fn generate_terrain(&self, batch: &mut [TerrainJob]) {
        if let Some(flat) = &self.flat {
            for job in batch {
                flat.build(&mut job.chunk);
            }
            return;
        }
        let mut scratch = self.terrain_scratch.take_or(TerrainScratch::default);
        for job in batch {
            scratch.ctx.reset();
            self.bind_beardifier(&mut scratch.ctx, job.chunk.pos(), &job.structures);
            self.build_terrain(&mut job.chunk, &job.neighbours, &mut scratch, None);
        }
        scratch.ctx.set_beardifier(None);
        self.terrain_scratch.give(scratch);
    }

    /// Drops the reused job buffers (they are made again by the next job): for idle times.
    pub fn release_scratch(&self) {
        self.terrain_scratch.release();
        self.material.release_scratch();
    }

    /// `ChunkGeneratorStructureState.ensureStructuresGenerated`: computes the ring positions of
    /// every concentric placement the generator can place (strongholds) now, so the first
    /// structure starts do not wait for them. Vanilla starts this when the level loads.
    pub fn prepare_structures(&self) {
        let state = StructureState::new(self);
        for &set in self.structures.possible_sets() {
            if let StructurePlacement::ConcentricRings { rings, .. } =
                &self.structures.set(set).placement
            {
                state.ring_positions_for(rings);
            }
        }
    }

    /// `Beardifier.forStructuresInChunk(structureManager, chunkPos)` bound to the context.
    fn bind_beardifier(&self, ctx: &mut SamplerContext, pos: ChunkPos, starts: &ReferencedStarts) {
        let beardifier = Beardifier::for_structures_in_chunk(&self.structures, starts, pos);
        ctx.set_beardifier(beardifier.map(|b| Arc::new(b) as Arc<dyn ContextSampler>));
    }

    /// [`Stage::Terrain`] for one chunk, calling `observe` with the chunk after each step (for
    /// parity tests and debugging).
    pub fn generate_terrain_observed(
        &self,
        job: &mut TerrainJob,
        mut observe: impl FnMut(TerrainStep, &ProtoChunk),
    ) {
        let mut scratch = TerrainScratch::default();
        self.bind_beardifier(&mut scratch.ctx, job.chunk.pos(), &job.structures);
        self.build_terrain(
            &mut job.chunk,
            &job.neighbours,
            &mut scratch,
            Some(&mut observe),
        );
    }

    /// `buildTerrain` for one chunk with a fresh caching context (`scratch.ctx`, reset);
    /// `observe` sees the chunk after each step. The blocks are built in the chunk's dense
    /// buffer.
    fn build_terrain(
        &self,
        chunk: &mut ProtoChunk,
        neighbours: &NeighbourBiomes,
        scratch: &mut TerrainScratch,
        mut observe: Option<Observer<'_>>,
    ) {
        let TerrainScratch {
            ctx,
            bounds,
            dense,
            density,
            states,
        } = scratch;
        let mut observe = |step: TerrainStep, chunk: &mut ProtoChunk, dense: &mut Vec<u16>| {
            if let Some(observe) = observe.as_mut() {
                chunk.end_dense(dense);
                observe(step, chunk);
                chunk.begin_dense(dense);
            }
        };
        // `NoiseSettings.clampToHeightAccessor`.
        let min_y = self.settings.min_y.max(chunk.min_y());
        let height = (self.settings.min_y + self.settings.height).min(chunk.max_y() + 1) - min_y;
        if height <= 0 {
            return;
        }
        let pos = chunk.pos();
        let volume = DensityVolume::new(16, height, 16, pos.x << 4, min_y, pos.z << 4);

        // `new NoiseChunk(...)`: the aquifer samples the surface level while it is built.
        let mut aquifer = Aquifer::for_chunk(
            self.aquifer.as_ref(),
            &self.random_state,
            &volume,
            self.fluid_picker,
            &self.program,
            ctx,
            Some(&self.surface_levels),
        );
        chunk.begin_dense(dense);
        self.fill_from_noise(chunk, &volume, &mut aquifer, ctx, bounds, density, states);
        observe(TerrainStep::Noise, chunk, dense);

        let biome_at = |x: i32, y: i32, z: i32| {
            self.biome_manager.get_biome(x, y, z, &mut |qx, qy, qz| {
                neighbours.noise_biome(pos, qx, qy, qz)
            })
        };
        self.material.build_surface(
            &self.program,
            ctx,
            chunk,
            &self.biomes,
            &volume,
            &biome_at,
            &neighbours.possible_biomes(),
        );
        observe(TerrainStep::Surface, chunk, dense);
        self.carve(chunk, &mut aquifer, ctx);
        chunk.finish_terrain();
        chunk.end_dense(dense);
        observe(TerrainStep::Carvers, chunk, dense);
        chunk.end_dense(dense);
    }

    /// `doFill`: the final density over the chunk volume, turned into blocks by the aquifer.
    /// `bounds` is the context the density bounds are computed in; `density` and `states` are
    /// reused buffers.
    #[allow(clippy::too_many_arguments)]
    fn fill_from_noise(
        &self,
        chunk: &mut ProtoChunk,
        volume: &DensityVolume,
        aquifer: &mut Aquifer,
        ctx: &mut SamplerContext,
        bounds: &mut SamplerContext,
        density: &mut Vec<f32>,
        states: &mut Vec<BlockState>,
    ) {
        // From this Y up a block that is not solid is air, without asking the aquifer.
        let air_from_y = aquifer.air_from_y();
        // And from `top` up the density is proven not to be positive: it is only sampled
        // below, which gives the same values there.
        let top = self.program.not_positive_from(
            bounds,
            self.final_density,
            volume,
            air_from_y,
            ctx.has_beardifier(),
        );
        let whole = *volume;
        let volume = &if top > whole.min_y && top <= whole.max_y() {
            DensityVolume::new(
                whole.size_x,
                top - whole.min_y,
                whole.size_z,
                whole.min_x,
                whole.min_y,
                whole.min_z,
            )
        } else {
            whole
        };
        // Every value is written by the sampling (the buffer only grows, never cleared).
        if density.len() < volume.len() {
            density.resize(volume.len(), 0.0);
        }
        let density = &mut density[..volume.len()];
        self.program
            .sample_volume(ctx, self.final_density, density, volume);
        let default_block = self.settings.default_block;
        // Every row is written per column before the column is read.
        if states.len() < volume.size_y as usize {
            states.resize(volume.size_y as usize, BlockState::AIR);
        }
        let states = &mut states[..volume.size_y as usize];
        for z in 0..volume.size_z {
            let block_z = volume.block_z(z);
            for x in 0..volume.size_x {
                let block_x = volume.block_x(x);
                let start = volume.index(x, 0, z);
                let column = &density[start..start + volume.size_y as usize];
                // The column's blocks, top down as Vanilla fills it (the aquifer and the
                // post-processing marks follow that order).
                for y in (0..volume.size_y).rev() {
                    let block_y = volume.block_y(y);
                    let value = column[y as usize];
                    states[y as usize] = if value > 0.0 {
                        default_block
                    } else if block_y >= air_from_y {
                        // (Also for a NaN density, which is not solid in the aquifer either.)
                        BlockState::AIR
                    } else {
                        let state = aquifer
                            .compute_substance(
                                &self.program,
                                ctx,
                                block_x,
                                block_y,
                                block_z,
                                f64::from(value),
                            )
                            .unwrap_or(default_block);
                        if !state.fluid_state().is_empty() && aquifer.should_schedule_fluid_update()
                        {
                            chunk.mark_post_processing(block_x, block_y, block_z);
                        }
                        state
                    };
                }
                if !chunk.write_empty_column(block_x, block_z, volume.min_y, states) {
                    self.write_column(chunk, block_x, block_z, volume.min_y, states);
                }
            }
        }
    }

    /// Sets the blocks of a column block by block, top down, with the heightmap updates of
    /// `doFill` (for a chunk that already holds blocks).
    fn write_column(
        &self,
        chunk: &mut ProtoChunk,
        block_x: i32,
        block_z: i32,
        min_y: i32,
        states: &[BlockState],
    ) {
        let (lx, lz) = ((block_x & 15) as usize, (block_z & 15) as usize);
        // Below the first block a heightmap matches, its updates change nothing.
        let (mut surface_done, mut ocean_floor_done) = (false, false);
        for (row, &state) in states.iter().enumerate().rev() {
            if state == BlockState::AIR {
                continue;
            }
            let block_y = min_y + row as i32;
            chunk.set_block_raw(block_x, block_y, block_z, state);
            if !(surface_done && ocean_floor_done) {
                let flags = facts::block_flags(state.0);
                if !ocean_floor_done {
                    chunk.update_heightmap(WgHeightmap::OceanFloor, lx, block_y, lz, state);
                    ocean_floor_done = WgHeightmap::OceanFloor.kind().matches(flags);
                }
                if !surface_done {
                    chunk.update_heightmap(WgHeightmap::WorldSurface, lx, block_y, lz, state);
                    surface_done = WgHeightmap::WorldSurface.kind().matches(flags);
                }
            }
        }
    }

    /// `generateCarvers` and `applyCarvingMask`.
    fn carve(&self, chunk: &mut ProtoChunk, aquifer: &mut Aquifer, ctx: &mut SamplerContext) {
        let pos = chunk.pos();
        let context =
            WorldGenerationContext::for_settings(&self.settings, chunk.min_y(), chunk.height());
        // The carvers' biomes come from an uncached resolver at each source chunk's corner,
        // queried in Vanilla's loop order.
        let mut resolver = self
            .biome_source
            .create_uncached_resolver(&self.program, &self.climate);
        const RANGE: i32 = 8;
        let side = (2 * RANGE + 1) as usize;
        let mut source_carvers: Vec<&[CarverId]> = Vec::with_capacity(side * side);
        for dx in -RANGE..=RANGE {
            for dz in -RANGE..=RANGE {
                let biome = resolver.noise_biome_at_zero(
                    (pos.x + dx) << 2,
                    (pos.z + dz) << 2,
                    &self.carver_climates,
                );
                source_carvers.push(&self.biome_carvers[usize::from(biome)]);
            }
        }
        let lookup = |source: ChunkPos| {
            let dx = (source.x - pos.x + RANGE) as usize;
            let dz = (source.z - pos.z + RANGE) as usize;
            source_carvers[dx * side + dz]
        };
        let mask = carve_chunk(
            &self.carvers,
            &lookup,
            self.seed,
            pos,
            &context,
            Some(&self.carver_plans),
        );
        if mask.is_empty() {
            return;
        }

        // `biomeManager.withDifferentSource(biomeResolver)`.
        let resolver = RefCell::new(resolver);
        let biome_at = |x: i32, y: i32, z: i32| {
            self.biome_manager
                .get_biome(x, y, z, &mut *resolver.borrow_mut())
        };
        let (min_x, min_z) = (pos.x << 4, pos.z << 4);
        mask.visit(|x, z, bottom, top| {
            let mut has_grass = false;
            let (world_x, world_z) = (min_x + x, min_z + z);
            for world_y in (bottom..=top).rev() {
                let current = chunk.block(world_x, world_y, world_z);
                if self.uncarvable.contains(current) {
                    continue;
                }
                let block = current.block();
                if block == self.grass_block || block == self.mycelium {
                    has_grass = true;
                }
                let Some(state) =
                    aquifer.compute_substance(&self.program, ctx, world_x, world_y, world_z, 0.0)
                else {
                    continue;
                };
                chunk.set_block(world_x, world_y, world_z, state);
                let fluid = !state.fluid_state().is_empty();
                if fluid && aquifer.should_schedule_fluid_update() {
                    chunk.mark_post_processing(world_x, world_y, world_z);
                }
                if has_grass && chunk.block(world_x, world_y - 1, world_z).block() == self.dirt {
                    let top_material = self.material.top_material(
                        &self.program,
                        ctx,
                        chunk,
                        &self.biomes,
                        &biome_at,
                        world_x,
                        world_y - 1,
                        world_z,
                        fluid,
                    );
                    if let Some(top_material) = top_material {
                        chunk.set_block(world_x, world_y - 1, world_z, top_material);
                        if !top_material.fluid_state().is_empty() {
                            chunk.mark_post_processing(world_x, world_y - 1, world_z);
                        }
                    }
                }
            }
        });
    }
}

/// `NoiseColumn`: the blocks of one column as `getBaseColumn` returns them.
#[derive(Clone, Debug)]
pub struct NoiseColumn {
    min_y: i32,
    /// States bottom-up from `min_y`.
    states: Vec<BlockState>,
}

impl NoiseColumn {
    /// `getBlock(pos)`: air outside the column.
    pub fn block(&self, y: i32) -> BlockState {
        usize::try_from(y - self.min_y)
            .ok()
            .and_then(|i| self.states.get(i))
            .copied()
            .unwrap_or(BlockState::AIR)
    }
}

/// `ChunkGenerator.getWritableArea(chunk)`: the chunk's columns from one above the lowest Y.
fn writable_area(pos: ChunkPos, min_y: i32, height: i32) -> BoundingBox {
    let (x, z) = (pos.min_block_x(), pos.min_block_z());
    BoundingBox::new(x, min_y + 1, z, x + 15, min_y + height - 1, z + 15)
}

/// The reused buffers of one terrain job.
#[derive(Debug)]
struct TerrainScratch {
    /// The caching sampler context, reset per chunk (Vanilla's fresh `NoiseChunk`).
    ctx: SamplerContext,
    /// The context the density bounds are computed in.
    bounds: SamplerContext,
    /// The dense block buffer of the chunk being built ([`ProtoChunk::begin_dense`]).
    dense: Vec<u16>,
    /// The final density of the chunk volume.
    density: Vec<f32>,
    /// The states of one column.
    states: Vec<BlockState>,
}

impl Default for TerrainScratch {
    fn default() -> Self {
        Self {
            ctx: SamplerContext::new(true),
            bounds: SamplerContext::new(true),
            dense: Vec::new(),
            density: Vec::new(),
            states: Vec::new(),
        }
    }
}
