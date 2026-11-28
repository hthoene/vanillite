//! Contracts for chunk generation, lighting and persistence: chunk statuses with Vanilla's
//! generation pyramid, proto-chunks and the world-scoped slots `chunk-storage`,
//! `chunk-generator` and `light-engine` (architecture 5.2).
//!
//! The chunk scheduler in `vanillite-world` drives the slots; the built-in implementations live
//! in `vanillite-storage`, `vanillite-worldgen` and `vanillite-light`. All slot calls are batch
//! shaped and run on the compute pool, so implementations are `Send + Sync` and shared per world.
//! The storage slot is the asynchronous record store of [`crate::store`].

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use vanillite_base::{BlockPos, ChunkPos};

use crate::chunk::Chunk;
use crate::facts::{BiomeId, BlockStateId};
use crate::light::ChunkLight;

pub use crate::store::{
    ChunkCodec, ChunkStore, ChunkStoreProvider, Completion, CompletionTarget, DimensionId,
    InboxWaker, MemoryStore, NullCodec, NullStore, Priority, RecordKey, RecordMeta, RoundView,
    StoreOp, StoreResult, Stream, Token,
};

/// Vanilla's `ChunkStatus`, in generation order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ChunkStatus {
    /// Nothing generated.
    Empty,
    /// Structure starts placed.
    StructureStarts,
    /// Structure references collected.
    StructureReferences,
    /// Biomes filled.
    Biomes,
    /// Noise, aquifers, ore veins, surface rules and carvers (one step since 26.x).
    Terrain,
    /// Features and structure pieces.
    Features,
    /// Light sources prepared.
    InitializeLight,
    /// Initial light propagated.
    Light,
    /// Initial mobs spawned.
    Spawn,
    /// A live chunk of the world.
    Full,
}

/// Distance from a full chunk up to which the generation pyramid reaches (Vanilla's
/// `ChunkLevel.RADIUS_AROUND_FULL_CHUNK`).
pub const RADIUS_AROUND_FULL: u32 = 11;

impl ChunkStatus {
    /// All statuses in order.
    pub const ALL: [Self; 10] = [
        Self::Empty,
        Self::StructureStarts,
        Self::StructureReferences,
        Self::Biomes,
        Self::Terrain,
        Self::Features,
        Self::InitializeLight,
        Self::Light,
        Self::Spawn,
        Self::Full,
    ];

    /// Index in [`Self::ALL`].
    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The registry name, e.g. `minecraft:terrain`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Empty => "minecraft:empty",
            Self::StructureStarts => "minecraft:structure_starts",
            Self::StructureReferences => "minecraft:structure_references",
            Self::Biomes => "minecraft:biomes",
            Self::Terrain => "minecraft:terrain",
            Self::Features => "minecraft:features",
            Self::InitializeLight => "minecraft:initialize_light",
            Self::Light => "minecraft:light",
            Self::Spawn => "minecraft:spawn",
            Self::Full => "minecraft:full",
        }
    }

    /// The status of a registry name, with or without the `minecraft:` namespace.
    pub fn by_name(name: &str) -> Option<Self> {
        let path = name.strip_prefix("minecraft:").unwrap_or(name);
        Self::ALL
            .into_iter()
            .find(|status| &status.name()["minecraft:".len()..] == path)
    }

    /// The previous status; `Empty` for `Empty`.
    pub const fn parent(self) -> Self {
        match self {
            Self::Empty | Self::StructureStarts => Self::Empty,
            Self::StructureReferences => Self::StructureStarts,
            Self::Biomes => Self::StructureReferences,
            Self::Terrain => Self::Biomes,
            Self::Features => Self::Terrain,
            Self::InitializeLight => Self::Features,
            Self::Light => Self::InitializeLight,
            Self::Spawn => Self::Light,
            Self::Full => Self::Spawn,
        }
    }

    /// The next status; `None` after `Full`.
    pub const fn next(self) -> Option<Self> {
        match self {
            Self::Empty => Some(Self::StructureStarts),
            Self::StructureStarts => Some(Self::StructureReferences),
            Self::StructureReferences => Some(Self::Biomes),
            Self::Biomes => Some(Self::Terrain),
            Self::Terrain => Some(Self::Features),
            Self::Features => Some(Self::InitializeLight),
            Self::InitializeLight => Some(Self::Light),
            Self::Light => Some(Self::Spawn),
            Self::Spawn => Some(Self::Full),
            Self::Full => None,
        }
    }

    /// What the step to this status needs besides the chunk itself at the parent status:
    /// `(status, radius)` pairs of Vanilla's `GENERATION_PYRAMID`.
    pub const fn requirements(self) -> &'static [(Self, u32)] {
        match self {
            Self::StructureReferences | Self::Biomes => &[(Self::StructureStarts, 8)],
            Self::Terrain => &[(Self::StructureStarts, 8), (Self::Biomes, 1)],
            Self::Features => &[(Self::StructureStarts, 8), (Self::Terrain, 1)],
            Self::Light => &[(Self::InitializeLight, 1)],
            Self::Spawn => &[(Self::Biomes, 1)],
            _ => &[],
        }
    }

    /// Radius around the chunk in which the step to this status writes blocks
    /// (`blockStateWriteRadius`; 0 for steps that write nothing outside the chunk).
    pub const fn write_radius(self) -> u32 {
        match self {
            Self::Features => 1,
            _ => 0,
        }
    }

    /// The status the step to this status needs at `distance` (direct requirements, the
    /// chunk itself at the parent status), `None` beyond its reach.
    pub fn direct_requirement_at(self, distance: u32) -> Option<Self> {
        let mut required = (distance == 0 && self != Self::Empty).then(|| self.parent());
        for &(status, radius) in self.requirements() {
            if distance <= radius {
                required = Some(required.map_or(status, |r| r.max(status)));
            }
        }
        required
    }

    /// Radius of the direct requirements.
    pub fn requirement_radius(self) -> u32 {
        self.requirements()
            .iter()
            .map(|&(_, radius)| radius)
            .max()
            .unwrap_or(0)
    }

    /// The accumulated dependencies of the step to this status by distance (Vanilla's
    /// `ChunkStep.accumulatedDependencies`): what every chunk at that distance must reach before
    /// the whole chain up to this status can finish.
    pub fn accumulated(self) -> &'static [Self] {
        static TABLE: OnceLock<[Vec<ChunkStatus>; 10]> = OnceLock::new();
        &TABLE.get_or_init(|| {
            let mut table: [Vec<ChunkStatus>; 10] = Default::default();
            for status in Self::ALL {
                let direct: Vec<Self> = (0..=status.requirement_radius())
                    .map_while(|d| status.direct_requirement_at(d))
                    .collect();
                if status == Self::Empty {
                    continue;
                }
                let parent = &table[status.parent().index()];
                let parent_radius = direct
                    .iter()
                    .rposition(|&s| s >= status.parent())
                    .unwrap_or(0);
                let len = (parent_radius + parent.len()).max(direct.len());
                let accumulated = (0..len)
                    .map(|distance| {
                        let in_parent = distance
                            .checked_sub(parent_radius)
                            .and_then(|d| parent.get(d).copied());
                        match (direct.get(distance).copied(), in_parent) {
                            (Some(a), Some(b)) => a.max(b),
                            (Some(a), None) => a,
                            (None, Some(b)) => b,
                            (None, None) => Self::Empty,
                        }
                    })
                    .collect();
                table[status.index()] = accumulated;
            }
            table
        })[self.index()]
    }

    /// The farthest distance at which a chunk on its way to full needs a chunk at this status
    /// (the largest `d` with [`ChunkStatus::around_full`]`(d)` at least this status): the step
    /// to this status is waited for by chunks up to this far away.
    pub const fn reach(self) -> u8 {
        match self {
            Self::Empty | Self::StructureStarts => RADIUS_AROUND_FULL as u8,
            Self::StructureReferences | Self::Biomes => 3,
            Self::Terrain => 2,
            Self::Features | Self::InitializeLight => 1,
            Self::Light | Self::Spawn | Self::Full => 0,
        }
    }

    /// The status a chunk at `distance` from a full chunk must reach
    /// (`ChunkStatus.getStatusAroundFullChunk`); `None` beyond [`RADIUS_AROUND_FULL`].
    pub fn around_full(distance: u32) -> Option<Self> {
        if distance == 0 {
            return Some(Self::Full);
        }
        Self::Full
            .accumulated()
            .get(usize::try_from(distance).ok()?)
            .copied()
    }
}

/// A block entity as stored with its chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockEntityData {
    /// Position in the world.
    pub pos: BlockPos,
    /// Block entity type id (`minecraft:block_entity_type`), `None` if unknown.
    pub kind: Option<u16>,
    /// The stored compound as binary network NBT, passed through unchanged.
    pub nbt: Arc<[u8]>,
}

/// A scheduled block or fluid tick as stored with its chunk (Vanilla's `SavedTick`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedTick {
    /// Block id (`minecraft:block`) of a block tick, fluid id (`minecraft:fluid`) of a fluid
    /// tick.
    pub kind: u16,
    /// Position in the world.
    pub pos: BlockPos,
    /// Ticks until it runs, relative to the game time it was saved at.
    pub delay: i32,
    /// `TickPriority` value (-3 extremely high ..= 3 extremely low).
    pub priority: i8,
}

/// A chunk with everything stored with it: blocks, light, heightmaps, its generation status and
/// the data the runtime does not interpret yet. One type serves generation (statuses before
/// `Full`), storage and the live world (`Full`).
#[derive(Clone, Debug)]
pub struct ProtoChunk {
    /// Position.
    pub pos: ChunkPos,
    /// Generation status reached.
    pub status: ChunkStatus,
    /// Blocks, biomes, light and heightmaps.
    pub chunk: Chunk,
    /// Block entities.
    pub block_entities: Vec<BlockEntityData>,
    /// Ticks players spent near this chunk (`InhabitedTime`).
    pub inhabited_time: i64,
    /// The game time of the last save (`LastUpdate`).
    pub last_update: i64,
    /// Whether the stored light is complete and trusted (`isLightOn`).
    pub light_on: bool,
    /// Scheduled block ticks of a chunk that is not live: a live chunk's ticks are run by the
    /// world and put back here when it is saved.
    pub block_ticks: Vec<SavedTick>,
    /// Scheduled fluid ticks, like [`Self::block_ticks`].
    pub fluid_ticks: Vec<SavedTick>,
    /// Positions generation marked for post-processing (`PostProcessing`); the world handles
    /// them when the chunk becomes live.
    pub post_processing: Vec<BlockPos>,
    /// The entities of a chunk that is not live, as stored in its entities record (Vanilla's
    /// `entities/`): one binary network-NBT compound per entity, in stored order. A live
    /// chunk's entities are run by the world and put back here when it is saved.
    pub entities: Vec<Arc<[u8]>>,
    /// The points of interest record of the chunk (Vanilla's `poi/`) as binary network NBT of
    /// its root compound without `DataVersion` (`Sections` and unknown tags); `None` without
    /// a record. A live chunk's POI are run by the world and put back here when it is saved.
    pub poi: Option<Arc<[u8]>>,
    /// The rest of the stored chunk (structures, …) as binary network NBT of a compound,
    /// written back unchanged. `None` for new chunks.
    pub extra: Option<Arc<[u8]>>,
    /// What the chunk generator keeps between its steps while the chunk is generated; not
    /// stored, dropped when the chunk becomes `Full`.
    pub generation: Option<GenerationState>,
}

/// Generator-private data of a chunk being generated (e.g. Vanilla's world generation
/// heightmaps, block entity data and scheduled ticks), shared between snapshots.
#[derive(Clone)]
pub struct GenerationState(pub Arc<dyn std::any::Any + Send + Sync>);

impl std::fmt::Debug for GenerationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GenerationState")
    }
}

impl ProtoChunk {
    /// An empty chunk at status `Empty`.
    pub fn empty(pos: ChunkPos, world: &WorldInfo) -> Self {
        ProtoChunk {
            pos,
            status: ChunkStatus::Empty,
            chunk: Chunk::new(world.min_y, world.height, world.default_biome, world.air),
            block_entities: Vec::new(),
            inhabited_time: 0,
            last_update: 0,
            light_on: false,
            block_ticks: Vec::new(),
            fluid_ticks: Vec::new(),
            post_processing: Vec::new(),
            entities: Vec::new(),
            poi: None,
            extra: None,
            generation: None,
        }
    }
}

/// What the slots of one world know about it.
#[derive(Clone, Debug)]
pub struct WorldInfo {
    /// Dimension key, e.g. `minecraft:overworld`.
    pub dimension: String,
    /// The dimension's folder (contains `region/`); the overworld's is the world folder.
    pub dir: PathBuf,
    /// Lowest block Y; a multiple of 16.
    pub min_y: i32,
    /// Height in blocks; a multiple of 16.
    pub height: u32,
    /// World seed.
    pub seed: i64,
    /// Whether the dimension has sky light.
    pub has_sky_light: bool,
    /// Biome names by biome id (registry order).
    pub biomes: Arc<[String]>,
    /// Biome of new, empty chunks (`minecraft:plains`).
    pub default_biome: BiomeId,
    /// The air block state.
    pub air: BlockStateId,
}

/// Snapshots of the chunks around a batch, sorted by position. Chunks within the write radius
/// of the step may be changed; the scheduler takes those changes over, all others are dropped.
#[derive(Debug)]
pub struct Neighbours<'a> {
    chunks: &'a mut [Arc<ProtoChunk>],
}

impl<'a> Neighbours<'a> {
    /// Wraps snapshots; sorts them by position.
    pub fn new(chunks: &'a mut [Arc<ProtoChunk>]) -> Self {
        chunks.sort_unstable_by_key(|chunk| chunk.pos);
        Neighbours { chunks }
    }

    #[inline]
    fn find(&self, pos: ChunkPos) -> Option<usize> {
        self.chunks
            .binary_search_by_key(&pos, |chunk| chunk.pos)
            .ok()
    }

    /// The chunk at `pos`, if it is part of the area.
    #[inline]
    pub fn get(&self, pos: ChunkPos) -> Option<&ProtoChunk> {
        self.find(pos).map(|index| &*self.chunks[index])
    }

    /// The chunk at `pos` for writing (copied on first write if the snapshot is shared).
    #[inline]
    pub fn get_mut(&mut self, pos: ChunkPos) -> Option<&mut ProtoChunk> {
        let index = self.find(pos)?;
        Some(Arc::make_mut(&mut self.chunks[index]))
    }

    /// All chunks, sorted by position.
    pub fn iter(&self) -> impl Iterator<Item = &ProtoChunk> {
        self.chunks.iter().map(|chunk| &**chunk)
    }

    /// Number of chunks.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// No chunks.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// The `chunk-generator` slot: produces the generation statuses up to `Features`.
pub trait ChunkGenerator: Send + Sync {
    /// A new chunk at status `Empty`.
    fn new_chunk(&self, pos: ChunkPos) -> ProtoChunk;

    /// Runs the step to `status` for every target; all targets are at `status.parent()`, the
    /// scheduler sets their status afterwards. `neighbours` holds every chunk within the
    /// step's requirement radius at its required status, except the targets; targets are never
    /// within each other's requirement radius.
    fn generate(
        &self,
        status: ChunkStatus,
        targets: &mut [ProtoChunk],
        neighbours: &mut Neighbours<'_>,
    );

    /// Precomputes what the first steps would otherwise compute on demand (Vanilla's
    /// `ensureStructuresGenerated` when a level loads). Runs once as a background job when the
    /// world starts; `generate` must work without it.
    fn prepare(&self) {}

    /// Frees what the generator can make again when it is needed (tables, caches). Called
    /// when the server has been empty for a while; what `generate` produces must not change.
    fn release_memory(&self) {}

    /// Whether chunks get their first animals when they pass the spawn step
    /// (`ChunkGenerator.spawnOriginalMobs`): Vanilla's noise generator does unless its settings
    /// disable mob generation; flat and debug worlds do not.
    fn spawns_original_mobs(&self) -> bool {
        true
    }
}

/// Creates the generator of each world.
pub trait ChunkGeneratorProvider: Send + Sync {
    /// The generator of one world.
    fn create(&self, world: &WorldInfo) -> Result<Arc<dyn ChunkGenerator>, String>;
}

/// The built-in stand-in generator until Vanilla's noise generator fills the slot: flat
/// layers from the bottom of the world (no layers: a void world), the default biome.
#[derive(Debug)]
pub struct FlatGenerator {
    world: WorldInfo,
    template: Chunk,
}

impl FlatGenerator {
    /// Vanilla's classic superflat layers: bedrock, two dirt, grass block.
    pub fn classic(world: &WorldInfo) -> Self {
        use vanillite_data::block::ids;
        Self::new(
            world,
            &[
                ids::BEDROCK.default_state().0,
                ids::DIRT.default_state().0,
                ids::DIRT.default_state().0,
                ids::GRASS_BLOCK.default_state().0,
            ],
        )
    }

    /// A generator with these layers, bottom first.
    pub fn new(world: &WorldInfo, layers: &[BlockStateId]) -> Self {
        let mut template = Chunk::new(world.min_y, world.height, world.default_biome, world.air);
        for (y, &state) in (world.min_y..).zip(layers) {
            for z in 0..16 {
                for x in 0..16 {
                    template.set_block(x, y, z, state);
                }
            }
        }
        FlatGenerator {
            world: world.clone(),
            template,
        }
    }
}

impl ChunkGenerator for FlatGenerator {
    fn new_chunk(&self, pos: ChunkPos) -> ProtoChunk {
        ProtoChunk {
            chunk: self.template.clone(),
            ..ProtoChunk::empty(pos, &self.world)
        }
    }

    fn generate(&self, _: ChunkStatus, _: &mut [ProtoChunk], _: &mut Neighbours<'_>) {}

    fn spawns_original_mobs(&self) -> bool {
        false
    }
}

/// Provider of [`FlatGenerator`] with the classic layers.
#[derive(Debug, Default)]
pub struct FlatGeneratorProvider;

impl ChunkGeneratorProvider for FlatGeneratorProvider {
    fn create(&self, world: &WorldInfo) -> Result<Arc<dyn ChunkGenerator>, String> {
        Ok(Arc::new(FlatGenerator::classic(world)))
    }
}

/// The `light-engine` slot. Results of initial light are combined by taking the maximum of
/// every light level, so jobs of neighbouring chunks may run at the same time.
pub trait LightEngine: Send + Sync {
    /// Prepares the light layers of the targets (the step to `InitializeLight`): sky light
    /// layers at and below the highest non-empty section and all block light layers start at
    /// 0; sky light layers above stay absent, which means full sky light.
    fn initialize_light(&self, targets: &mut [ProtoChunk]);

    /// Initial light of the targets (the step to `Light`): propagates the targets' own light
    /// sources through them and their neighbours within 1 chunk. Neighbours are at
    /// `InitializeLight` or later; light written into them is kept.
    fn light_chunks(&self, targets: &mut [ProtoChunk], neighbours: &mut Neighbours<'_>);

    /// Updates the light after the block changes at `changes`. `area` holds the chunks within 1
    /// of every changed chunk (light travels 15 blocks) with the blocks after the changes and
    /// the light before them, which was right for the blocks before. Chunks before
    /// `InitializeLight` have no light layers and take no light. Returns the new light of every
    /// chunk whose light changes; the runtime applies it unless the light of one of them
    /// changed meanwhile, in which case it asks again.
    fn relight(&self, changes: &[BlockPos], area: &Neighbours<'_>) -> Vec<(ChunkPos, ChunkLight)>;

    /// Whether changing a block from `old` to `new` can change any light level. The runtime
    /// only recomputes the light around changes where this is `true` (the default).
    fn changes_light(&self, old: BlockStateId, new: BlockStateId) -> bool {
        let _ = (old, new);
        true
    }
}

/// Creates the light engine of each world.
pub trait LightEngineProvider: Send + Sync {
    /// The light engine of one world.
    fn create(&self, world: &WorldInfo) -> Arc<dyn LightEngine>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyramid_matches_vanilla() {
        use ChunkStatus::*;
        // `GENERATION_PYRAMID.getStepTo(FULL).accumulatedDependencies()`.
        let full = Full.accumulated();
        assert_eq!(full.len(), RADIUS_AROUND_FULL as usize + 1);
        assert_eq!(&full[..4], [Spawn, InitializeLight, Terrain, Biomes]);
        assert!(full[4..].iter().all(|&s| s == StructureStarts));
        assert_eq!(ChunkStatus::around_full(1), Some(InitializeLight));
        assert_eq!(ChunkStatus::around_full(12), None);
        for status in ChunkStatus::ALL {
            let reach = (0..=RADIUS_AROUND_FULL)
                .rev()
                .find(|&d| ChunkStatus::around_full(d).is_some_and(|s| s >= status));
            assert_eq!(reach, Some(u32::from(status.reach())), "{status:?}");
        }
        assert_eq!(ChunkStatus::by_name("terrain"), Some(Terrain));
    }
}
