//! Natural spawning (`NaturalSpawner`): mob caps per category, the spawn attempts per chunk
//! near players, the spawn rules of each mob, the animals of freshly generated chunks
//! (`spawnMobsForChunkGeneration`) and spawning a mob by command.

use std::sync::Arc;
use vanillite_base::hash::FxHashMap;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, ChunkPos, Vec3};

use vanillite_chunk::{HeightmapKind, ProtoChunk};
use vanillite_data::block::{BlockState, flags, ids};
use vanillite_data::entity::{EntityTypeId, ids as entity_ids};
use vanillite_world::entity::{EntityId, Rotation};

use super::fam::{ExtraMob, GroupData, Relation, SpawnContext, SpawnReason};
use super::kinds::{self, FarmClimate, Kind};
use super::nearby::{
    CloseForSpawning, NearbyPlayers, RADIUS, SPAWN_DISTANCE_SQR, chunk_center_distance_sqr,
};
use super::physics;
use super::{Environment, MobData, max_local_raw_brightness};
use crate::level::Level;
use crate::level::tiles::{Phase, Phases, TilePos, TileRun, sort_chunks, tile_seed};
use crate::survival::rules::Difficulty;

/// `MobCategory` without `MISC`, in Vanilla's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Monster,
    Creature,
    Ambient,
    Axolotls,
    UndergroundWaterCreature,
    WaterCreature,
    WaterAmbient,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Monster,
        Category::Creature,
        Category::Ambient,
        Category::Axolotls,
        Category::UndergroundWaterCreature,
        Category::WaterCreature,
        Category::WaterAmbient,
    ];

    /// The category of an entity type, `None` for `misc`.
    pub fn of(t: EntityTypeId) -> Option<Category> {
        Category::by_name(t.info().category)
    }

    /// The category by its name in data packs (`monster`, `creature`, …).
    pub fn by_name(name: &str) -> Option<Category> {
        Some(match name {
            "monster" => Category::Monster,
            "creature" => Category::Creature,
            "ambient" => Category::Ambient,
            "axolotls" => Category::Axolotls,
            "underground_water_creature" => Category::UndergroundWaterCreature,
            "water_creature" => Category::WaterCreature,
            "water_ambient" => Category::WaterAmbient,
            _ => return None,
        })
    }

    fn index(self) -> usize {
        self as usize
    }

    /// `getMaxInstancesPerChunk`.
    pub fn max_per_chunk(self) -> i32 {
        match self {
            Category::Monster => 70,
            Category::Creature => 10,
            Category::Ambient => 15,
            Category::Axolotls | Category::UndergroundWaterCreature | Category::WaterCreature => 5,
            Category::WaterAmbient => 20,
        }
    }

    /// `isFriendly`.
    pub fn is_friendly(self) -> bool {
        self != Category::Monster
    }

    /// `isPersistent`.
    pub fn is_persistent(self) -> bool {
        self == Category::Creature
    }

    /// `getDespawnDistance`.
    pub fn despawn_distance(self) -> i32 {
        if self == Category::WaterAmbient {
            64
        } else {
            128
        }
    }
}

/// Deviations from Vanilla's spawn numbers (`performance.spawning`). The default is Vanilla.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnLimits {
    /// Mob cap per category, in [`Category::ALL`] order (`getMaxInstancesPerChunk`).
    pub caps: [i32; 7],
    /// Despawn distance of every category; `None` keeps each category's own.
    pub despawn_distance: Option<i32>,
}

impl Default for SpawnLimits {
    fn default() -> Self {
        SpawnLimits {
            caps: Category::ALL.map(Category::max_per_chunk),
            despawn_distance: None,
        }
    }
}

impl SpawnLimits {
    fn cap(&self, c: Category) -> i32 {
        self.caps[c.index()]
    }

    fn despawn_distance(&self, c: Category) -> i32 {
        self.despawn_distance
            .unwrap_or_else(|| c.despawn_distance())
    }
}

/// One `SpawnerData` of a biome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnEntry {
    pub type_id: EntityTypeId,
    pub weight: i32,
    pub min: i32,
    pub max: i32,
}

/// The spawn settings of one biome.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BiomeSpawns {
    /// The biome id (`minecraft:plains`).
    pub name: Box<str>,
    /// The biome tags that contain the biome (`minecraft:is_forest`), sorted.
    pub tags: Vec<Box<str>>,
    pub by_category: [Vec<SpawnEntry>; 7],
    /// `spawn_costs` (`MobSpawnCost`): charge and energy budget by entity type.
    pub spawn_costs: Vec<(EntityTypeId, f64, f64)>,
    /// `creature_world_gen_spawn_probability`.
    pub creature_probability: f32,
    pub climate: FarmClimate,
    /// The base temperature of the biome.
    pub temperature: f32,
    /// `minecraft:gameplay/can_pillager_patrol_spawn`.
    pub patrol_spawn: bool,
    /// `minecraft:gameplay/snow_golem_melts` where the biome sets it (else the dimension's).
    pub snow_golem_melts: Option<bool>,
}

impl BiomeSpawns {
    /// `biome.is(tag)`, with or without the `minecraft:` namespace.
    pub fn is(&self, tag: &str) -> bool {
        let tag = tag.strip_prefix('#').unwrap_or(tag);
        if tag.contains(':') {
            self.tags.binary_search_by(|t| (**t).cmp(tag)).is_ok()
        } else {
            let full = format!("minecraft:{tag}");
            self.tags.binary_search_by(|t| (**t).cmp(&*full)).is_ok()
        }
    }

    /// The spawn cost of an entity type (`getMobSpawnCost`).
    pub fn spawn_cost(&self, t: EntityTypeId) -> Option<(f64, f64)> {
        self.spawn_costs
            .iter()
            .find(|c| c.0 == t)
            .map(|&(_, charge, budget)| (charge, budget))
    }
}

/// The spawn settings of every biome, by protocol id, and the structures' spawn overrides.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpawnTable {
    pub biomes: Vec<BiomeSpawns>,
    /// `Structure.spawnOverrides` by structure id and category.
    pub structures: FxHashMap<Box<str>, [Option<StructureOverride>; 7]>,
    /// Per category whether any structure overrides its spawns, worked out on first use (a
    /// table is not changed once spawning uses it).
    overridden: std::sync::OnceLock<[bool; 7]>,
}

/// `StructureSpawnOverride`: the spawns inside a structure (its pieces or its whole box).
#[derive(Clone, Debug, PartialEq)]
pub struct StructureOverride {
    /// `bounding_box: piece` (else `full`).
    pub piece: bool,
    pub spawns: Vec<SpawnEntry>,
}

impl SpawnTable {
    /// A table of the biomes' spawn settings and the structures' spawn overrides.
    pub fn new(
        biomes: Vec<BiomeSpawns>,
        structures: FxHashMap<Box<str>, [Option<StructureOverride>; 7]>,
    ) -> Self {
        SpawnTable {
            biomes,
            structures,
            overridden: std::sync::OnceLock::new(),
        }
    }

    /// Whether any structure overrides the spawns of `category`.
    fn any_override(&self, category: Category) -> bool {
        self.overridden.get_or_init(|| {
            let mut any = [false; 7];
            for o in self.structures.values() {
                for (a, o) in any.iter_mut().zip(o) {
                    *a |= o.is_some();
                }
            }
            any
        })[category.index()]
    }

    /// The spawn settings of a biome by protocol id.
    pub fn biome(&self, id: u16) -> Option<&BiomeSpawns> {
        self.biomes.get(usize::from(id))
    }
}

/// The spawn table of the loaded data packs for code without a world's table at hand (spawn
/// rules that check the biome); data packs are frozen for the process.
static INSTALLED: std::sync::OnceLock<std::sync::Arc<SpawnTable>> = std::sync::OnceLock::new();

/// Installs the process' spawn table (later calls are ignored).
pub fn install_table(table: std::sync::Arc<SpawnTable>) {
    let _ = INSTALLED.set(table);
}

/// The spawn settings of the biome at `pos`, `None` without data packs.
pub fn biome_at(level: &Level, pos: BlockPos) -> Option<&'static BiomeSpawns> {
    INSTALLED.get()?.biome(level.biome(pos))
}

/// `WeightedList.getRandom`.
fn pick(list: &[SpawnEntry], random: &mut LegacyRandom) -> Option<SpawnEntry> {
    let total: i32 = list.iter().map(|e| e.weight).sum();
    if total <= 0 {
        return None;
    }
    let mut roll = random.next_int_bounded(total);
    for e in list {
        roll -= e.weight;
        if roll < 0 {
            return Some(*e);
        }
    }
    None
}

/// What the spawner needs to know about the world besides the level.
#[derive(Clone, Copy, Debug)]
pub struct SpawnInput<'a> {
    /// Non-spectator players with their position.
    pub players: &'a [(EntityId, Vec3)],
    pub spawn_point: Option<BlockPos>,
    /// `spawnEnemies`: monsters may spawn (difficulty and `spawn_monsters`).
    pub spawn_enemies: bool,
    pub difficulty: Difficulty,
    pub env: Environment,
    pub limits: SpawnLimits,
    /// The `spawn_mobs` rule and the clock for lightning (`tickThunder` runs in every spawning
    /// chunk, whether mobs spawn or not).
    pub thunder: crate::lightning::StrikeInput,
    /// Whether this tick is a natural spawn round (`performance.spawning.spawn-interval`; always
    /// in the vanilla profile). Off: only the shuffled chunks and their thunder.
    pub natural: bool,
}

/// Per-player mob counts (`LocalMobCapCalculator.MobCounts`), by [`NearbyPlayers`] slot.
#[derive(Debug, Default)]
struct LocalCaps {
    counts: Vec<[i32; 7]>,
    /// The number of players below the cap per category (counts only grow during a tick).
    below: [usize; 7],
    /// The mob cap per category ([`SpawnLimits::caps`]).
    caps: [i32; 7],
}

impl LocalCaps {
    /// `addMob` for one player.
    fn add(&mut self, slot: u32, c: Category, n: i32) {
        let count = &mut self.counts[slot as usize][c.index()];
        let max = self.caps[c.index()];
        if *count < max && *count + n >= max {
            self.below[c.index()] -= 1;
        }
        *count += n;
    }

    /// `canSpawn` for the players close to a chunk.
    fn can_spawn(&self, c: Category, nearby: &NearbyPlayers, chunk: ChunkPos) -> bool {
        self.below[c.index()] > 0
            && nearby.around(chunk).iter().any(|&s| {
                self.counts[s as usize][c.index()] < self.caps[c.index()]
                    && nearby.is_close_for_spawning(s, chunk)
            })
    }

    /// The counts of every player in `nearby` (`addMob` for every counted mob): the mobs of
    /// the chunks close enough for spawning to the player. The chunks close to a player form
    /// one run of x per row of chunks, so the counts are prefix sums per row instead of every
    /// mob's chunk times every close player.
    fn count(
        nearby: &NearbyPlayers,
        mob_chunks: &FxHashMap<ChunkPos, [i32; 7]>,
        caps: [i32; 7],
    ) -> LocalCaps {
        let mut rows: FxHashMap<i32, Vec<ChunkCounts>> = FxHashMap::default();
        for (&chunk, &n) in mob_chunks {
            rows.entry(chunk.z).or_default().push((chunk.x, n));
        }
        let mut rows: Vec<(i32, Vec<ChunkCounts>)> = rows.into_iter().collect();
        rows.sort_unstable_by_key(|r| r.0);
        for (_, row) in &mut rows {
            row.sort_unstable_by_key(|e| e.0);
            let mut sum = [0; 7];
            for (_, n) in row.iter_mut() {
                for (s, v) in sum.iter_mut().zip(n.iter_mut()) {
                    *s += *v;
                    *v = *s;
                }
            }
        }
        let mut local = LocalCaps {
            counts: vec![[0; 7]; nearby.slot_count()],
            below: [0; 7],
            caps,
        };
        for (slot, pos, home) in nearby.players() {
            let mut total = [0; 7];
            let first = rows.partition_point(|r| r.0 < home.z - RADIUS);
            let last = rows.partition_point(|r| r.0 <= home.z + RADIUS);
            for (z, row) in rows.get(first..last).unwrap_or(&[]) {
                let z = *z;
                let close =
                    |x| chunk_center_distance_sqr(ChunkPos::new(x, z), pos) < SPAWN_DISTANCE_SQR;
                // The own column is the nearest one of the row.
                if !close(home.x) {
                    continue;
                }
                let dz = f64::from(z) * 16.0 + 8.0 - pos.z;
                let half = (SPAWN_DISTANCE_SQR - dz * dz).max(0.0).sqrt();
                #[allow(clippy::cast_possible_truncation)]
                let guess = |x: f64| ((x - 8.0) / 16.0).floor() as i32;
                let mut lo = guess(pos.x - half).clamp(home.x - RADIUS, home.x);
                while lo < home.x && !close(lo) {
                    lo += 1;
                }
                while lo > home.x - RADIUS && close(lo - 1) {
                    lo -= 1;
                }
                let mut hi = guess(pos.x + half).clamp(home.x, home.x + RADIUS);
                while hi > home.x && !close(hi) {
                    hi -= 1;
                }
                while hi < home.x + RADIUS && close(hi + 1) {
                    hi += 1;
                }
                let start = row.partition_point(|e| e.0 < lo);
                let end = row.partition_point(|e| e.0 <= hi);
                if end > start {
                    let below = start.checked_sub(1).map_or([0; 7], |i| row[i].1);
                    for ((t, a), b) in total.iter_mut().zip(row[end - 1].1).zip(below) {
                        *t += a - b;
                    }
                }
            }
            local.counts[slot as usize] = total;
            for c in Category::ALL {
                if total[c.index()] < caps[c.index()] {
                    local.below[c.index()] += 1;
                }
            }
        }
        local
    }
}

/// The chunk x of a row of chunks and the mob counts summed up to it.
type ChunkCounts = (i32, [i32; 7]);

/// `NaturalSpawner.SpawnState` of one tick.
#[derive(Debug, Default)]
struct SpawnState {
    spawnable_chunks: i32,
    counts: [i32; 7],
    local: LocalCaps,
}

impl SpawnState {
    fn can_spawn_global(&self, c: Category, limits: &SpawnLimits) -> bool {
        let max = limits.cap(c) * self.spawnable_chunks / 289;
        self.counts[c.index()] < max
    }
}

/// `PotentialCalculator`: the charges of the mobs with a spawn cost.
#[derive(Debug, Default)]
struct Potential {
    charges: Vec<(BlockPos, f64)>,
}

impl Potential {
    fn add(&mut self, pos: BlockPos, charge: f64) {
        if charge != 0.0 {
            self.charges.push((pos, charge));
        }
    }

    /// `getPotentialEnergyChange`.
    fn energy_change(&self, pos: BlockPos, charge: f64) -> f64 {
        if charge == 0.0 {
            return 0.0;
        }
        let mut sum = 0.0;
        for &(p, c) in &self.charges {
            let d = p.dist_sqr(pos);
            sum += if d == 0.0 {
                f64::INFINITY
            } else {
                c / d.sqrt()
            };
        }
        sum * charge
    }
}

/// The reusable state of a world's spawner.
#[derive(Debug, Default)]
pub struct Spawner {
    chunks: Vec<ChunkPos>,
    nearby: NearbyPlayers,
    close: CloseForSpawning,
    mob_chunks: FxHashMap<ChunkPos, [i32; 7]>,
    potential: Potential,
    /// Parsed structure data of chunks.
    pub structures: super::structures::StructureIndex,
    /// Chunks that passed the spawn step and wait for their animals.
    pub generated: Vec<ChunkPos>,
    /// What each busy tile keeps between ticks in tile region mode ([`tick_tiled`]).
    tiles: FxHashMap<TilePos, TileSpawner>,
    /// The chunks around the players and their flags ([`collect_spawning_chunks`]).
    candidates: Vec<ChunkPos>,
    flags: SpawnFlags,
}

/// `canSpawnEntitiesInChunk` (ticking and inside the world border) of the chunks around the
/// players, worked out for the tick as a parallel phase: a grid over their bounding box,
/// "unknown" outside.
#[derive(Debug, Default)]
struct SpawnFlags {
    min: ChunkPos,
    width: i32,
    height: i32,
    /// 0 = unknown, 1 = no, 2 = yes.
    cells: Vec<u8>,
}

/// Largest grid of [`SpawnFlags`] in chunks; larger boxes go without one.
const FLAGS_AREA: i64 = 1 << 22;

impl SpawnFlags {
    /// An empty grid over the bounding box of `chunks` (none when it is too large).
    fn reset(&mut self, chunks: &[ChunkPos]) {
        self.cells.clear();
        self.width = 0;
        self.height = 0;
        let Some(first) = chunks.first() else {
            return;
        };
        let (mut min, mut max) = (*first, *first);
        for c in chunks {
            min = ChunkPos::new(min.x.min(c.x), min.z.min(c.z));
            max = ChunkPos::new(max.x.max(c.x), max.z.max(c.z));
        }
        let width = i64::from(max.x) - i64::from(min.x) + 1;
        let height = i64::from(max.z) - i64::from(min.z) + 1;
        if width * height > FLAGS_AREA {
            return;
        }
        self.min = min;
        self.width = i32::try_from(width).unwrap_or(0);
        self.height = i32::try_from(height).unwrap_or(0);
        self.cells
            .resize(usize::try_from(width * height).unwrap_or(0), 0);
    }

    #[inline]
    fn cell(&self, c: ChunkPos) -> Option<usize> {
        let (x, z) = (c.x.wrapping_sub(self.min.x), c.z.wrapping_sub(self.min.z));
        ((0..self.width).contains(&x) && (0..self.height).contains(&z))
            .then(|| usize::try_from(z * self.width + x).unwrap_or(0))
    }

    fn set(&mut self, c: ChunkPos, yes: bool) {
        if let Some(i) = self.cell(c) {
            self.cells[i] = 1 + u8::from(yes);
        }
    }

    /// The flag of `c`, `None` when it was not worked out.
    #[inline]
    fn get(&self, c: ChunkPos) -> Option<bool> {
        match self.cells.get(self.cell(c)?)? {
            0 => None,
            v => Some(*v == 2),
        }
    }
}

/// A tile's own caches in tile region mode.
#[derive(Debug, Default)]
struct TileSpawner {
    structures: super::structures::StructureIndex,
    close: CloseForSpawning,
}

/// A mob a spawn round finished: what `addFreshEntityWithPassengers` and `afterSpawn` need.
struct NewMob {
    mob: MobData,
    pos: Vec3,
    y_rot: f32,
    extra: Vec<ExtraMob>,
    /// The block position, with the charge of the mob's spawn cost.
    block: BlockPos,
    charge: f64,
}

/// Where a spawn round reads, draws its random numbers and puts its mobs: the world itself in
/// Vanilla's order ([`WorldRound`]) or one tile of the tile region mode ([`TileRound`]). The
/// spawn logic ([`spawn_category_for_chunk`]) is the same for both.
trait Round {
    /// The level, for reads.
    fn level(&self) -> &Level;
    /// The round's random (`level.random`).
    fn random(&mut self) -> &mut LegacyRandom;
    /// Runs `f` with the level next to the round's random.
    fn with_level_random<T>(&mut self, f: impl FnOnce(&Level, &mut LegacyRandom) -> T) -> T;
    /// The level next to the structure index.
    fn level_structures(&mut self) -> (&Level, &mut super::structures::StructureIndex);
    /// The seed of a new mob's own random.
    fn entity_seed(&mut self) -> i64;
    /// `PotentialCalculator.getPotentialEnergyChange`.
    fn energy_change(&self, pos: BlockPos, charge: f64) -> f64;
    /// `canSpawnForCategoryLocal`.
    fn can_spawn_local(&self, c: Category, nearby: &NearbyPlayers, chunk: ChunkPos) -> bool;
    /// `addFreshEntityWithPassengers` and `afterSpawn`: the mob, its charge and the counts.
    fn spawned(&mut self, nearby: &NearbyPlayers, chunk: ChunkPos, c: Category, new: NewMob);
}

/// A spawn round on the world itself: mobs are added at once and the level's random is used
/// in place, as in Vanilla.
struct WorldRound<'a> {
    level: &'a mut Level,
    structures: &'a mut super::structures::StructureIndex,
    potential: &'a mut Potential,
    close: &'a mut CloseForSpawning,
    state: &'a mut SpawnState,
    /// Mobs added so far: the local caps only change with them.
    spawns: u32,
}

impl Round for WorldRound<'_> {
    #[inline]
    fn level(&self) -> &Level {
        self.level
    }

    #[inline]
    fn random(&mut self) -> &mut LegacyRandom {
        self.level.random()
    }

    #[inline]
    fn with_level_random<T>(&mut self, f: impl FnOnce(&Level, &mut LegacyRandom) -> T) -> T {
        with_level_random(self.level, f)
    }

    #[inline]
    fn level_structures(&mut self) -> (&Level, &mut super::structures::StructureIndex) {
        (self.level, self.structures)
    }

    #[inline]
    fn entity_seed(&mut self) -> i64 {
        crate::mob::new_entity_seed()
    }

    #[inline]
    fn energy_change(&self, pos: BlockPos, charge: f64) -> f64 {
        self.potential.energy_change(pos, charge)
    }

    #[inline]
    fn can_spawn_local(&self, c: Category, nearby: &NearbyPlayers, chunk: ChunkPos) -> bool {
        self.state.local.can_spawn(c, nearby, chunk)
    }

    fn spawned(&mut self, nearby: &NearbyPlayers, chunk: ChunkPos, c: Category, new: NewMob) {
        add_mob_with(
            self.level,
            new.mob,
            new.pos,
            new.y_rot,
            new.extra,
            SpawnReason::Natural,
        );
        self.potential.add(new.block, new.charge);
        self.spawns += 1;
        self.state.counts[c.index()] += 1;
        for &slot in self.close.get(nearby, chunk) {
            self.state.local.add(slot, c, 1);
        }
    }
}

/// A spawn round of one tile in tile region mode: it only reads the level, draws from the
/// tile's random and collects its mobs; [`tick_tiled`] adds them afterwards. The counts of
/// the tick's start come from the world, the tile's own spawns are added on top; what other
/// tiles spawn in the same tick is not seen.
struct TileRound<'a> {
    level: &'a Level,
    random: LegacyRandom,
    seeds: LegacyRandom,
    structures: &'a mut super::structures::StructureIndex,
    close: &'a mut CloseForSpawning,
    world_potential: &'a Potential,
    potential: Potential,
    caps: &'a LocalCaps,
    /// The mobs this tile spawned this tick, per player slot and category.
    added: FxHashMap<u32, [i32; 7]>,
    out: Vec<NewMob>,
}

impl Round for TileRound<'_> {
    #[inline]
    fn level(&self) -> &Level {
        self.level
    }

    #[inline]
    fn random(&mut self) -> &mut LegacyRandom {
        &mut self.random
    }

    #[inline]
    fn with_level_random<T>(&mut self, f: impl FnOnce(&Level, &mut LegacyRandom) -> T) -> T {
        f(self.level, &mut self.random)
    }

    #[inline]
    fn level_structures(&mut self) -> (&Level, &mut super::structures::StructureIndex) {
        (self.level, self.structures)
    }

    #[inline]
    fn entity_seed(&mut self) -> i64 {
        self.seeds.next_long()
    }

    fn energy_change(&self, pos: BlockPos, charge: f64) -> f64 {
        self.world_potential.energy_change(pos, charge) + self.potential.energy_change(pos, charge)
    }

    fn can_spawn_local(&self, c: Category, nearby: &NearbyPlayers, chunk: ChunkPos) -> bool {
        let caps = self.caps;
        caps.below[c.index()] > 0
            && nearby.around(chunk).iter().any(|&s| {
                let added = self.added.get(&s).map_or(0, |a| a[c.index()]);
                caps.counts[s as usize][c.index()] + added < caps.caps[c.index()]
                    && nearby.is_close_for_spawning(s, chunk)
            })
    }

    fn spawned(&mut self, nearby: &NearbyPlayers, chunk: ChunkPos, c: Category, new: NewMob) {
        self.potential.add(new.block, new.charge);
        for &slot in self.close.get(nearby, chunk) {
            self.added.entry(slot).or_insert([0; 7])[c.index()] += 1;
        }
        self.out.push(new);
    }
}

/// The spawn cost of a type at a position (`getRoughBiome(pos).getMobSpawnCost(type)`).
fn spawn_cost(
    level: &Level,
    table: &SpawnTable,
    t: EntityTypeId,
    pos: BlockPos,
) -> Option<(f64, f64)> {
    table
        .biome(rough_biome(level, pos))
        .and_then(|b| b.spawn_cost(t))
}

/// `getRoughBiome`: the noise biome of the quart position without the biome zoom.
fn rough_biome(level: &Level, pos: BlockPos) -> u16 {
    let default = level.chunks.info().default_biome;
    let Some(c) = level.chunks.chunk(pos.chunk()) else {
        return default;
    };
    let sections = c.chunk.sections();
    let min_quart = c.chunk.min_section() * 4;
    let len = i32::try_from(sections.len()).unwrap_or(0);
    let qy = (pos.y >> 2).clamp(min_quart, min_quart + len * 4 - 1);
    let index = usize::try_from((qy - min_quart) >> 2).unwrap_or(0);
    #[allow(clippy::cast_sign_loss)]
    sections[index].biome(
        ((pos.x >> 2) & 3) as u32,
        (qy & 3) as u32,
        ((pos.z >> 2) & 3) as u32,
    )
}

/// Whether a mob counts for the caps (`!isPersistenceRequired && !requiresCustomPersistence`).
fn counts_for_caps(level: &Level, id: EntityId, m: &MobData) -> bool {
    !m.persistent
        && !super::fam::requires_custom_persistence(m)
        && level.entities.riding.vehicle_of(id).is_none()
        && !level.entities.leashes.contains_key(&id)
}

/// `ServerChunkCache.tickChunks`' spawning chunks: counts, caps, and per shuffled chunk near
/// players the thunder (`tickThunder`) and one attempt per category.
pub fn tick(
    level: &mut Level,
    table: &SpawnTable,
    spawner: &mut Spawner,
    input: &SpawnInput<'_>,
    spawn_persistent: bool,
    phases: &Phases<'_>,
) {
    if input.players.is_empty() {
        return;
    }
    spawner.nearby.update(input.players);
    tick_world(level, table, spawner, input, spawn_persistent, phases);
}

/// [`tick`] after the players were taken over: the round on the world itself.
fn tick_world(
    level: &mut Level,
    table: &SpawnTable,
    spawner: &mut Spawner,
    input: &SpawnInput<'_>,
    spawn_persistent: bool,
    phases: &Phases<'_>,
) {
    spawner.close.clear();
    if !input.thunder.spawn_mobs || !input.natural {
        // No categories: only the shuffled chunks and their thunder.
        collect_spawning_chunks(level, spawner, phases);
        let chunks = std::mem::take(&mut spawner.chunks);
        for &chunk in &chunks {
            // `tickSpawningChunk`.
            level.chunks.increment_inhabited_time(chunk);
            if level.chunks.in_entity_ticking_range(chunk) {
                crate::lightning::tick_thunder(level, chunk, input.thunder);
            }
        }
        spawner.chunks = chunks;
        return;
    }
    let mut state = create_state(level, table, spawner, input);
    let categories = spawn_categories(&state, input, spawn_persistent);
    collect_spawning_chunks(level, spawner, phases);
    let chunks = std::mem::take(&mut spawner.chunks);
    let flags = &spawner.flags;
    let mut round = WorldRound {
        level,
        structures: &mut spawner.structures,
        potential: &mut spawner.potential,
        close: &mut spawner.close,
        state: &mut state,
        spawns: 0,
    };
    let mut wants = [false; 7];
    for (i, &chunk) in chunks.iter().enumerate() {
        // The chunks ahead start loading into the cache while this one spawns.
        for (ahead, stage) in [(3, 0), (2, 3)] {
            if let Some(&next) = chunks.get(i + ahead) {
                round.level.chunks.prefetch(next, stage);
            }
        }
        // `tickSpawningChunk`.
        round.level.chunks.increment_inhabited_time(chunk);
        if round.level.chunks.in_entity_ticking_range(chunk) {
            crate::lightning::tick_thunder(round.level, chunk, input.thunder);
        }
        // `canSpawnEntitiesInChunk`: the chunk is ticking and inside the world border.
        if !round.level.border.is_chunk_within_bounds(chunk) {
            continue;
        }
        // `canSpawnForCategoryLocal` of every category: the local caps only change when a mob
        // is added, so the answers hold until then.
        let mut attempts = 0;
        for (want, &c) in wants.iter_mut().zip(&categories) {
            *want = round.can_spawn_local(c, &spawner.nearby, chunk);
            attempts += usize::from(*want);
        }
        let mut here = round.level.chunks.chunk(chunk).cloned();
        if let Some(c) = &here {
            prefetch_starts(round.level, c, chunk, attempts);
        }
        let spawns = round.spawns;
        for (&want, &c) in wants.iter().zip(&categories) {
            let want = if round.spawns == spawns {
                want
            } else {
                round.can_spawn_local(c, &spawner.nearby, chunk)
            };
            if want {
                spawn_category_for_chunk(
                    &mut round,
                    table,
                    &spawner.nearby,
                    flags,
                    input,
                    c,
                    chunk,
                    &mut here,
                );
            }
        }
    }
    spawner.chunks = chunks;
}

/// Prefetch hint for the start blocks of a chunk's next `attempts` spawn attempts
/// (`getRandomPosWithin`), on a copy of the level's random and as if each attempt ended at its
/// start block, as most do: first their sections, then the blocks. Only reads.
fn prefetch_starts(level: &Level, chunk: &ProtoChunk, pos: ChunkPos, attempts: usize) {
    let mut random = level.random_ref().clone();
    let min_y = level.min_y();
    let mut starts = [(0u32, 0i32, 0u32); 7];
    let mut n = 0;
    for _ in 0..attempts.min(starts.len()) {
        let x = random.next_int_bounded(16);
        let z = random.next_int_bounded(16);
        let top = surface_y_of(
            level,
            Some(chunk),
            HeightmapKind::WorldSurface,
            pos.x * 16 + x,
            pos.z * 16 + z,
        ) + 1;
        let y = random.next_int_between_inclusive(min_y, top);
        let Ok(index) = usize::try_from((y >> 4) - chunk.chunk.min_section()) else {
            continue;
        };
        if y < min_y + 1 || chunk.chunk.section_all_conductors(index) {
            continue;
        }
        chunk.chunk.prefetch_section(index);
        #[allow(clippy::cast_sign_loss)]
        {
            starts[n] = (x as u32, y, z as u32);
        }
        n += 1;
    }
    for &(x, y, z) in &starts[..n] {
        chunk.chunk.prefetch_block(x, y, z);
    }
}

/// `NaturalSpawner.createState`: the non-persistent mobs per category, globally and per
/// player, and the charges of the mobs with a spawn cost.
fn create_state(
    level: &Level,
    table: &SpawnTable,
    spawner: &mut Spawner,
    input: &SpawnInput<'_>,
) -> SpawnState {
    let mut state = SpawnState {
        // `getNaturalSpawnChunkCount`: chunks within 8 of a player chunk.
        spawnable_chunks: i32::try_from(spawner.nearby.chunk_count()).unwrap_or(i32::MAX),
        counts: [0; 7],
        local: LocalCaps::default(),
    };
    spawner.potential.charges.clear();
    spawner.mob_chunks.clear();
    // `createState`: the non-persistent mobs per category, globally and per player, and the
    // charges of the mobs with a spawn cost.
    for (id, m) in level.entities.mobs.iter() {
        if !counts_for_caps(level, id, m) {
            continue;
        }
        let Some(e) = level.entities.store.get(id) else {
            continue;
        };
        let Some(c) = Category::of(e.kind()) else {
            continue;
        };
        let pos = e.pos();
        let block = BlockPos::containing(pos.x, pos.y, pos.z);
        let chunk = block.chunk();
        if level.chunks.chunk(chunk).is_none() {
            continue;
        }
        if let Some((charge, _)) = spawn_cost(level, table, e.kind(), block) {
            spawner.potential.add(block, charge);
        }
        state.counts[c.index()] += 1;
        spawner.mob_chunks.entry(chunk).or_insert([0; 7])[c.index()] += 1;
    }
    // The local counts: the sums do not depend on the order.
    state.local = LocalCaps::count(&spawner.nearby, &spawner.mob_chunks, input.limits.caps);
    state
}

/// The categories that spawn this tick (`getFilteredSpawningCategories`).
fn spawn_categories(
    state: &SpawnState,
    input: &SpawnInput<'_>,
    spawn_persistent: bool,
) -> Vec<Category> {
    Category::ALL
        .into_iter()
        .filter(|c| {
            (input.spawn_enemies || c.is_friendly())
                && (spawn_persistent || !c.is_persistent())
                && state.can_spawn_global(*c, &input.limits)
        })
        .collect()
}

/// [`tick`] in tile region mode ([`crate::level::tiles`]): the spawning chunks are grouped by
/// tile; every tile shuffles its chunks and runs its spawn attempts in parallel on the level
/// as it is at the start of the round, with its own random; then the tiles' chunks tick
/// (inhabited time, thunder) and their mobs are added, tile by tile. Falls back to [`tick`]'s
/// order when the round does not apply (see [`TileRun`]).
pub fn tick_tiled(
    level: &mut Level,
    table: &SpawnTable,
    spawner: &mut Spawner,
    input: &SpawnInput<'_>,
    spawn_persistent: bool,
    run: &TileRun<'_>,
    phases: &Phases<'_>,
) {
    if input.players.is_empty() {
        return;
    }
    spawner.nearby.update(input.players);
    let mut chunks = std::mem::take(&mut spawner.chunks);
    chunks.clear();
    chunks.extend(spawner.nearby.chunks());
    let chunks = chunks;
    if !run.applies(level, &chunks) {
        spawner.chunks = chunks;
        spawner.tiles.clear();
        tick_world(level, table, spawner, input, spawn_persistent, phases);
        return;
    }
    // Without categories only the chunks tick (their thunder).
    let (state, categories) = if input.thunder.spawn_mobs && input.natural {
        let state = create_state(level, table, spawner, input);
        let categories = spawn_categories(&state, input, spawn_persistent);
        (state, categories)
    } else {
        (SpawnState::default(), Vec::new())
    };
    let mut jobs = Vec::new();
    for (tile, chunks) in run.group(&chunks) {
        let own = spawner.tiles.remove(&tile).unwrap_or_default();
        jobs.push((tile, (chunks, own)));
    }
    // Tiles that are no longer busy drop their caches.
    spawner.tiles.clear();
    let shared: &Level = level;
    let nearby = &spawner.nearby;
    let world_potential = &spawner.potential;
    let (seed, time) = (shared.seed(), shared.game_time());
    let caps = &state.local;
    let categories = &categories;
    let no_flags = SpawnFlags::default();
    let replies = run.map(jobs, |tile, (mut spawning, mut own)| {
        let mut random = LegacyRandom::new(tile_seed(seed, time, tile, Phase::Spawning));
        // `collectSpawningChunks` of the tile, shuffled with its random.
        spawning.retain(|&c| shared.is_block_ticking(c) && nearby.any_close_for_spawning(c));
        sort_chunks(&mut spawning);
        shuffle(&mut spawning, &mut random);
        own.close.clear();
        let mut round = TileRound {
            level: shared,
            random,
            seeds: LegacyRandom::new(tile_seed(seed, time, tile, Phase::SpawnSeeds)),
            structures: &mut own.structures,
            close: &mut own.close,
            world_potential,
            potential: Potential::default(),
            caps,
            added: FxHashMap::default(),
            out: Vec::new(),
        };
        if !categories.is_empty() {
            for &chunk in &spawning {
                if !shared.border.is_chunk_within_bounds(chunk) {
                    continue;
                }
                let mut here = None;
                for &c in categories {
                    if round.can_spawn_local(c, nearby, chunk) {
                        spawn_category_for_chunk(
                            &mut round, table, nearby, &no_flags, input, c, chunk, &mut here,
                        );
                    }
                }
            }
        }
        let mobs = round.out;
        (own, spawning, mobs)
    });
    for reply in replies {
        // The tile's changes: its spawning chunks and its new mobs. Its caches stay with it.
        let (own, spawning, mobs) = reply.changes;
        spawner.tiles.insert(reply.tile, own);
        for chunk in spawning {
            // `tickSpawningChunk`.
            level.chunks.increment_inhabited_time(chunk);
            if level.chunks.in_entity_ticking_range(chunk) {
                crate::lightning::tick_thunder(level, chunk, input.thunder);
            }
        }
        for mut new in mobs {
            // A mount another mob of this tick took already is not taken twice.
            new.extra.retain(|e| match e.relation {
                Relation::Existing(vehicle) => {
                    level.entities.store.contains(vehicle)
                        && level.entities.riding.passengers_of(vehicle).is_empty()
                }
                _ => true,
            });
            add_mob_with(
                level,
                new.mob,
                new.pos,
                new.y_rot,
                new.extra,
                SpawnReason::Natural,
            );
        }
    }
    spawner.chunks = chunks;
}

/// `Util.shuffle`.
fn shuffle(chunks: &mut [ChunkPos], random: &mut LegacyRandom) {
    let n = chunks.len();
    for i in (1..n).rev() {
        let j = usize::try_from(random.next_int_bounded(i32::try_from(i + 1).unwrap_or(i32::MAX)))
            .unwrap_or(0);
        chunks.swap(i, j);
    }
}

/// `collectSpawningChunks`: ticking chunks with a player within 128 blocks, shuffled with the
/// level random (`Util.shuffle`). The checks of the chunks around the players run as a
/// parallel phase; their `canSpawnEntitiesInChunk` is kept for the tick ([`SpawnFlags`]).
fn collect_spawning_chunks(level: &mut Level, spawner: &mut Spawner, phases: &Phases<'_>) {
    const TICKING: u8 = 1;
    const IN_BORDER: u8 = 2;
    const CLOSE: u8 = 4;
    spawner.chunks.clear();
    let mut candidates = std::mem::take(&mut spawner.candidates);
    candidates.clear();
    candidates.extend(spawner.nearby.chunks());
    {
        let shared: &Level = level;
        let nearby = &spawner.nearby;
        let flags = phases.map(&candidates, crate::level::tiles::PHASE_PART, |part, out| {
            out.extend(part.iter().map(|&c| {
                let ticking = shared.is_block_ticking(c);
                (if ticking { TICKING } else { 0 })
                    | (if ticking && shared.border.is_chunk_within_bounds(c) {
                        IN_BORDER
                    } else {
                        0
                    })
                    | (if ticking && nearby.any_close_for_spawning(c) {
                        CLOSE
                    } else {
                        0
                    })
            }));
        });
        spawner.flags.reset(&candidates);
        for (&c, f) in candidates.iter().zip(flags) {
            spawner.flags.set(c, f & IN_BORDER != 0);
            if f & CLOSE != 0 {
                spawner.chunks.push(c);
            }
        }
    }
    spawner.candidates = candidates;
    sort_chunks(&mut spawner.chunks);
    shuffle(&mut spawner.chunks, level.random());
}

/// The nether fortress enemies (`NetherFortressStructure.FORTRESS_ENEMIES`).
fn fortress_enemies() -> &'static [SpawnEntry] {
    use vanillite_data::entity::ids as e;
    static LIST: std::sync::OnceLock<[SpawnEntry; 5]> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        let entry = |type_id, weight, min, max| SpawnEntry {
            type_id,
            weight,
            min,
            max,
        };
        [
            entry(e::BLAZE, 10, 2, 3),
            entry(e::ZOMBIFIED_PIGLIN, 5, 4, 4),
            entry(e::WITHER_SKELETON, 8, 5, 5),
            entry(e::SKELETON, 2, 5, 5),
            entry(e::MAGMA_CUBE, 3, 4, 4),
        ]
    })
}

/// `NaturalSpawner.mobsAt`: the fortress enemies on nether bricks in a fortress, a
/// structure's spawn override, else the biome's list.
fn mobs_at<'t>(
    level: &Level,
    table: &'t SpawnTable,
    index: &mut super::structures::StructureIndex,
    category: Category,
    pos: BlockPos,
    near: Option<&ProtoChunk>,
) -> &'t [SpawnEntry] {
    let mut fortress = false;
    let mut overridden: Option<&'t [SpawnEntry]> = None;
    let below_bricks = category == Category::Monster
        && level.block(BlockPos::new(pos.x, pos.y - 1, pos.z)).block() == ids::NETHER_BRICKS;
    // Only structures that can change the list are looked up: the fortress on nether bricks
    // and those with an override for this category.
    let overrides = |name: &str| {
        table
            .structures
            .get(name)
            .is_some_and(|o| o[category.index()].is_some())
    };
    let wanted = |name: &str| (below_bricks && name == "minecraft:fortress") || overrides(name);
    if below_bricks || table.any_override(category) {
        index.structures_at_of(level, pos, wanted, |name, in_piece, in_box| {
            if below_bricks && name == "minecraft:fortress" && in_box {
                fortress = true;
            }
            if overridden.is_none()
                && let Some(o) = table
                    .structures
                    .get(name)
                    .and_then(|o| o[category.index()].as_ref())
                && (if o.piece { in_piece } else { in_box })
            {
                overridden = Some(&o.spawns);
            }
        });
    }
    if fortress {
        return fortress_enemies();
    }
    if let Some(list) = overridden {
        return list;
    }
    table
        .biome(level.biome_near(pos, near))
        .map_or(&[], |b| &b.by_category[category.index()])
}

/// `Mob.getMaxSpawnClusterSize`.
pub fn max_spawn_cluster_size(kind: Kind) -> i32 {
    match kind {
        Kind::Wolf => 8,
        Kind::Horse
        | Kind::Donkey
        | Kind::Mule
        | Kind::SkeletonHorse
        | Kind::ZombieHorse
        | Kind::Llama
        | Kind::TraderLlama
        | Kind::Camel
        | Kind::CamelHusk => 6,
        Kind::Ghast | Kind::HappyGhast | Kind::Pillager => 1,
        Kind::Cod | Kind::Pufferfish | Kind::TropicalFish => 8,
        Kind::Salmon => 5,
        _ => 4,
    }
}

/// `spawnCategoryForChunk` and `spawnCategoryForPosition`. `here` keeps the chunk for the
/// next category of the same chunk (Vanilla reads its height and start block from the chunk
/// itself); it is dropped before a mob is added, so it never outlives a change of the level.
#[allow(clippy::too_many_arguments)]
fn spawn_category_for_chunk<R: Round>(
    round: &mut R,
    table: &SpawnTable,
    nearby: &NearbyPlayers,
    flags: &SpawnFlags,
    input: &SpawnInput<'_>,
    category: Category,
    chunk: ChunkPos,
    here: &mut Option<Arc<ProtoChunk>>,
) {
    if here.is_none() {
        *here = round.level().chunks.chunk(chunk).cloned();
    }
    let x0 = chunk.x * 16 + round.random().next_int_bounded(16);
    let z0 = chunk.z * 16 + round.random().next_int_bounded(16);
    let top = surface_y_of(
        round.level(),
        here.as_deref(),
        HeightmapKind::WorldSurface,
        x0,
        z0,
    ) + 1;
    let min_y = round.level().min_y();
    let y = round.random().next_int_between_inclusive(min_y, top);
    if y < min_y + 1 {
        return;
    }
    let start = BlockPos::new(x0, y, z0);
    #[allow(clippy::cast_sign_loss)]
    let in_chunk = here.as_deref().and_then(|c| {
        let index = usize::try_from((y >> 4) - c.chunk.min_section()).ok()?;
        (index < c.chunk.sections().len()).then_some((c, index))
    });
    let conductor = match in_chunk {
        // A section of conductors only answers without visiting it.
        Some((c, index)) if c.chunk.section_all_conductors(index) => true,
        Some((c, index)) => BlockState(c.chunk.sections()[index].block(
            (x0 & 15) as u32,
            (y & 15) as u32,
            (z0 & 15) as u32,
        ))
        .has(flags::REDSTONE_CONDUCTOR),
        None => round.level().block(start).has(flags::REDSTONE_CONDUCTOR),
    };
    if conductor {
        return;
    }
    // `canSpawnEntitiesInChunk` of the chunks the positions fall into, and the spawn list of
    // the first position (`getRandomSpawnMobAt` and `canSpawnMobAt` ask for the same one).
    let mut ticking = TickingMemo::default();
    let mut around = AroundMemo::default();
    let mut listed: Option<(BlockPos, &[SpawnEntry])> = None;
    let mut cluster = 0;
    for _ in 0..3 {
        let (mut x, mut z) = (x0, z0);
        let mut current: Option<SpawnEntry> = None;
        #[allow(clippy::cast_possible_truncation)]
        let mut max = (round.random().next_float() * 4.0).ceil() as i32;
        let mut group_index = 0u32;
        let mut group = GroupData::None;
        let mut group_size = 0;
        let mut ll = 0;
        while ll < max {
            ll += 1;
            x += round.random().next_int_bounded(6) - round.random().next_int_bounded(6);
            z += round.random().next_int_bounded(6) - round.random().next_int_bounded(6);
            let pos = BlockPos::new(x, y, z);
            let (xx, zz) = (f64::from(x) + 0.5, f64::from(z) + 0.5);
            let at = Vec3::new(xx, f64::from(y), zz);
            // `getNearestPlayer`: only compared with 24 (`isRightDistanceToPlayerAndSpawnPoint`
            // rejects closer positions) and the despawn distances up to 128.
            let nearest = nearby.nearest_distance_sqr_around(at, 576.0, around.get(nearby, pos));
            if !right_distance(
                round.level(),
                input,
                chunk,
                pos,
                nearest,
                flags,
                &mut ticking,
            ) {
                continue;
            }
            if current.is_none() {
                // `getRandomSpawnMobAt`: most water ambient spawns fail in some biomes.
                if category == Category::WaterAmbient
                    && table
                        .biome(round.level().biome(pos))
                        .is_some_and(|b| b.is("minecraft:reduced_water_ambient_spawns"))
                    && round.random().next_float() < 0.98
                {
                    break;
                }
                let list = {
                    let (level, structures) = round.level_structures();
                    mobs_at(level, table, structures, category, pos, here.as_deref())
                };
                listed = Some((pos, list));
                let Some(entry) = pick(list, round.random()) else {
                    break;
                };
                max = round
                    .random()
                    .next_int_between_inclusive(entry.min, entry.max);
                current = Some(entry);
            }
            let Some(entry) = current else {
                break;
            };
            let valid = {
                let (level, structures) = round.level_structures();
                let known = listed.filter(|l| l.0 == pos).map(|l| l.1);
                valid_position_for_type(
                    level, table, structures, input, category, entry, pos, nearest, known,
                )
            };
            if !valid {
                continue;
            }
            // `SpawnState.canSpawn`: the spawn cost's energy budget.
            let cost = spawn_cost(round.level(), table, entry.type_id, pos);
            if let Some((charge, budget)) = cost
                && round.energy_change(pos, charge) > budget
            {
                continue;
            }
            let Some(kind) = Kind::of(entry.type_id) else {
                // Types without behaviour yet do not spawn; the attempt ends like a failed
                // creation in Vanilla.
                return;
            };
            let seed = round.entity_seed();
            let mut mob = MobData::new(kind, seed);
            let y_rot = round.random().next_float() * 360.0;
            let pos_v = Vec3::new(xx, f64::from(y), zz);
            if !valid_position_for_mob(round.level(), input, &mob, pos_v, nearest) {
                continue;
            }
            let extra = round.with_level_random(|level, random| {
                finalize_in_group(
                    level,
                    Some(table),
                    &input.env,
                    &mut mob,
                    pos,
                    input.difficulty,
                    group_index,
                    &mut group,
                    SpawnReason::Natural,
                    random,
                )
            });
            // `AbstractCubeMob.onSyncedDataUpdated(ID_SIZE)`: a cube whose spawn size changed
            // turns (and its body) to its head yaw, the constructor's.
            let y_rot = if matches!(kind, Kind::Slime | Kind::MagmaCube | Kind::SulfurCube)
                && super::fam::undead::cubes::size(&mob) != 1
            {
                super::constructor_y_rot(seed)
            } else {
                y_rot
            };
            group_index += 1;
            cluster += 1;
            group_size += 1;
            let group_reached = super::fam::is_max_group_size_reached(&mob, group_size);
            *here = None;
            // `afterSpawn`: the charge, the counts.
            round.spawned(
                nearby,
                chunk,
                category,
                NewMob {
                    mob,
                    pos: pos_v,
                    y_rot,
                    extra,
                    block: pos,
                    charge: cost.map_or(0.0, |c| c.0),
                },
            );
            if cluster >= max_spawn_cluster_size(kind) {
                return;
            }
            if group_reached {
                break;
            }
        }
    }
}

/// Runs `f` with the level and its random taken out of it (`finalizeSpawn` reads the level
/// and draws from its random).
pub fn with_level_random<R>(
    level: &mut Level,
    f: impl FnOnce(&Level, &mut LegacyRandom) -> R,
) -> R {
    let mut random = std::mem::replace(level.random(), LegacyRandom::new(0));
    let out = f(level, &mut random);
    *level.random() = random;
    out
}

/// Builds the spawn context at `pos` and runs `finalizeSpawn`; returns the mobs that spawn
/// with it (jockeys).
#[allow(clippy::too_many_arguments)]
pub fn finalize(
    level: &Level,
    table: Option<&SpawnTable>,
    env: &Environment,
    mob: &mut MobData,
    pos: BlockPos,
    difficulty: Difficulty,
    group_index: u32,
    reason: SpawnReason,
    random: &mut LegacyRandom,
) -> Vec<ExtraMob> {
    let mut group = GroupData::None;
    finalize_in_group(
        level,
        table,
        env,
        mob,
        pos,
        difficulty,
        group_index,
        &mut group,
        reason,
        random,
    )
}

/// [`finalize`] of a member of a spawn group: `group` is the `SpawnGroupData` the earlier
/// members left and is updated for the next one.
#[allow(clippy::too_many_arguments)]
pub fn finalize_in_group(
    level: &Level,
    table: Option<&SpawnTable>,
    env: &Environment,
    mob: &mut MobData,
    pos: BlockPos,
    difficulty: Difficulty,
    group_index: u32,
    group: &mut GroupData,
    reason: SpawnReason,
    random: &mut LegacyRandom,
) -> Vec<ExtraMob> {
    let biome = table.and_then(|t| t.biome(level.biome(pos)));
    // `level.getCurrentDifficultyAt(mob.blockPosition())`.
    let instance = crate::level::difficulty::DifficultyInstance {
        base: difficulty,
        ..crate::level::difficulty::at(level, pos)
    };
    let mut ctx = SpawnContext {
        difficulty,
        special_multiplier: instance.special_multiplier(),
        effective_difficulty: instance.effective,
        climate: biome.map_or(FarmClimate::Temperate, |b| b.climate),
        group_index,
        group: *group,
        reason,
        biome,
        pos,
        level: Some(level),
        env: *env,
        extra: Vec::new(),
        ride: None,
        own_state: 0,
    };
    kinds::finalize_spawn(mob, &mut ctx, random);
    *group = ctx.group;
    if let Some(vehicle) = ctx.ride {
        ctx.extra.push(ExtraMob {
            mob: MobData::new(Kind::Chicken, 0),
            offset: Vec3::ZERO,
            relation: Relation::Existing(vehicle),
        });
    }
    ctx.extra
}

/// Adds a finished mob and the mobs spawned with it (`addFreshEntityWithPassengers`) for
/// `reason`. A plugin may cancel the spawn ([`Entities::spawned`](crate::entities::Entities::spawned)):
/// the returned mob is then gone and nothing spawns with it.
pub fn add_mob_with(
    level: &mut Level,
    mob: MobData,
    pos: Vec3,
    y_rot: f32,
    extra: Vec<ExtraMob>,
    reason: SpawnReason,
) -> EntityId {
    add_mob_with_code(level, mob, pos, y_rot, extra, reason.abi())
}

/// [`add_mob_with`] with the plugins' `vanillite_abi::spawn_reason` (riders: `JOCKEY`).
fn add_mob_with_code(
    level: &mut Level,
    mob: MobData,
    pos: Vec3,
    y_rot: f32,
    extra: Vec<ExtraMob>,
    reason: u32,
) -> EntityId {
    let id = add_mob_raw(level, mob, pos, y_rot);
    level.entities.spawned(id, reason, 0);
    if extra.is_empty() || !level.entities.store.contains(id) {
        return id;
    }
    let parent = level.entities.store.get(id).map_or(0, |e| e.uuid());
    let mut spawned = Vec::with_capacity(extra.len());
    for e in extra {
        if let Relation::Existing(vehicle) = e.relation {
            // `Zombie.finalizeSpawn`: `chicken.setChickenJockey(true)` before riding it.
            if let Some(c) = level.entities.mob_mut(vehicle)
                && c.kind == Kind::Chicken
            {
                super::fam::classic::chicken_mut(c).jockey = true;
            }
            level.entities.riding.add(id, vehicle, false);
            spawned.push(vehicle);
            continue;
        }
        let rides = !matches!(e.relation, Relation::Beside);
        let other = add_mob_raw(level, e.mob, pos + e.offset, y_rot);
        spawned.push(other);
        match e.relation {
            Relation::RidesExtra(i) => {
                if let Some(&vehicle) = spawned.get(i) {
                    level.entities.riding.add(other, vehicle, false);
                }
            }
            Relation::RidesIt => level.entities.riding.add(other, id, false),
            Relation::CarriesIt => level.entities.riding.add(id, other, false),
            Relation::Beside | Relation::Existing(_) => {}
        }
        let reason = if rides {
            vanillite_abi::spawn_reason::JOCKEY
        } else {
            reason
        };
        level.entities.spawned(other, reason, parent);
    }
    id
}

/// Adds a finished mob to the level (`addFreshEntity`) for `reason`; a plugin may cancel it
/// (the returned mob is then gone).
pub fn add_mob(
    level: &mut Level,
    mob: MobData,
    pos: Vec3,
    y_rot: f32,
    reason: SpawnReason,
) -> EntityId {
    let id = add_mob_raw(level, mob, pos, y_rot);
    level.entities.spawned(id, reason.abi(), 0);
    id
}

fn add_mob_raw(level: &mut Level, mut mob: MobData, pos: Vec3, y_rot: f32) -> EntityId {
    let uuid = crate::mob::new_uuid();
    mob.y_body_rot = y_rot;
    let rot = Rotation {
        y_rot,
        x_rot: 0.0,
        y_head_rot: y_rot,
    };
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    super::spawn(&mut level.entities, &network_ids, uuid, pos, rot, mob)
}

/// `isRightDistanceToPlayerAndSpawnPoint`.
#[allow(clippy::too_many_arguments)]
fn right_distance(
    level: &Level,
    input: &SpawnInput<'_>,
    chunk: ChunkPos,
    pos: BlockPos,
    nearest: f64,
    flags: &SpawnFlags,
    ticking: &mut TickingMemo,
) -> bool {
    if nearest <= 576.0 {
        return false;
    }
    if let Some(spawn) = input.spawn_point {
        let c = Vec3::new(
            f64::from(pos.x) + 0.5,
            f64::from(pos.y),
            f64::from(pos.z) + 0.5,
        );
        let s = Vec3::new(
            f64::from(spawn.x) + 0.5,
            f64::from(spawn.y) + 0.5,
            f64::from(spawn.z) + 0.5,
        );
        if c.distance_to_sqr(s) < 24.0 * 24.0 {
            return false;
        }
    }
    let c = pos.chunk();
    c == chunk
        || flags.get(c).unwrap_or_else(|| {
            ticking.get(c, || {
                level.is_block_ticking(c) && level.border.is_chunk_within_bounds(c)
            })
        })
}

/// The players around the chunk of the last position of a spawn attempt
/// ([`NearbyPlayers::around`]): its positions mostly stay in one chunk.
#[derive(Default)]
struct AroundMemo<'a> {
    last: Option<(ChunkPos, &'a [u32])>,
}

impl<'a> AroundMemo<'a> {
    #[inline]
    fn get(&mut self, nearby: &'a NearbyPlayers, pos: BlockPos) -> &'a [u32] {
        let chunk = pos.chunk();
        match self.last {
            Some((c, around)) if c == chunk => around,
            _ => {
                let around = nearby.around(chunk);
                self.last = Some((chunk, around));
                around
            }
        }
    }
}

/// The last few answers of `canSpawnEntitiesInChunk` in one spawn attempt: its positions stay
/// within a few chunks, and nothing that decides it changes while the attempt reads.
#[derive(Default)]
struct TickingMemo {
    entries: [Option<(ChunkPos, bool)>; 4],
    next: usize,
}

impl TickingMemo {
    fn get(&mut self, chunk: ChunkPos, compute: impl FnOnce() -> bool) -> bool {
        if let Some((_, v)) = self.entries.iter().flatten().find(|e| e.0 == chunk) {
            return *v;
        }
        let v = compute();
        self.entries[self.next] = Some((chunk, v));
        self.next = (self.next + 1) % self.entries.len();
        v
    }
}

/// `isValidSpawnPostitionForType`.
#[allow(clippy::too_many_arguments)]
fn valid_position_for_type(
    level: &Level,
    table: &SpawnTable,
    structures: &mut super::structures::StructureIndex,
    input: &SpawnInput<'_>,
    category: Category,
    entry: SpawnEntry,
    pos: BlockPos,
    nearest: f64,
    known: Option<&[SpawnEntry]>,
) -> bool {
    let info = entry.type_id.info();
    let despawn = f64::from(input.limits.despawn_distance(category));
    if !info.can_spawn_far_from_player && nearest > despawn * despawn {
        return false;
    }
    if !info.summonable {
        return false;
    }
    // `canSpawnMobAt`: the entry is still in the list there.
    let list = match known {
        Some(list) => list,
        None => mobs_at(level, table, structures, category, pos, None),
    };
    if !list.contains(&entry) {
        return false;
    }
    if !spawn_position_ok(level, entry.type_id, pos) {
        return false;
    }
    let Some(kind) = Kind::of(entry.type_id) else {
        return true;
    };
    let mut random =
        LegacyRandom::new(i64::from(pos.x) ^ (i64::from(pos.z) << 20) ^ level.game_time());
    if !check_spawn_rules(
        level,
        &input.env,
        kind,
        pos,
        &mut random,
        SpawnReason::Natural,
    ) {
        return false;
    }
    let dims = super::Dimensions::of(entry.type_id);
    let aabb = dims.bounding_box(Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    ));
    let mut scratch = Vec::new();
    physics::no_collision(&level.view(), &aabb, &mut scratch)
}

/// `Mob.checkSpawnRules` of a fresh mob: pathfinder mobs need a walk target value of at
/// least 0 (`PathfinderMob`), the others always pass.
pub fn mob_check_spawn_rules(
    level: &Level,
    env: &Environment,
    mob: &MobData,
    pos: BlockPos,
) -> bool {
    let pathfinder = !matches!(
        mob.kind,
        Kind::Slime | Kind::MagmaCube | Kind::SulfurCube | Kind::Ghast | Kind::Phantom | Kind::Bat
    );
    !pathfinder || kinds::walk_target_value(mob.kind, level, env, pos) >= 0.0
}

/// `Mob.checkSpawnObstruction` with the kinds' overrides (water mobs only need room).
pub fn check_spawn_obstruction(level: &Level, env: &Environment, mob: &MobData, pos: Vec3) -> bool {
    let aabb = kinds::mob_dimensions(mob).bounding_box(pos);
    let liquid = || physics::contains_any_liquid(level, &aabb);
    // `level.isUnobstructed(mob)`: no entity that blocks building in the box.
    let unobstructed = || {
        let mut blocked = false;
        level.entities.store.for_each_in(&aabb, false, |id| {
            blocked |= crate::block::dispenser::blocks_building(level, id);
        });
        !blocked
    };
    match mob.kind {
        Kind::Squid
        | Kind::GlowSquid
        | Kind::Dolphin
        | Kind::Cod
        | Kind::Salmon
        | Kind::Pufferfish
        | Kind::TropicalFish
        | Kind::Nautilus
        | Kind::ZombieNautilus
        | Kind::Axolotl
        | Kind::Guardian
        | Kind::Strider
        | Kind::Drowned => unobstructed(),
        Kind::Ravager | Kind::ZombifiedPiglin => !liquid(),
        Kind::Ocelot => {
            if !unobstructed() || liquid() {
                return false;
            }
            let block = BlockPos::containing(pos.x, pos.y, pos.z);
            if block.y < env.sea_level {
                return false;
            }
            let below = level.block(BlockPos::new(block.x, block.y - 1, block.z));
            below.block() == ids::GRASS_BLOCK || in_block_tag(below, "minecraft:leaves")
        }
        Kind::IronGolem => {
            let block = BlockPos::containing(pos.x, pos.y, pos.z);
            let below = level.block(BlockPos::new(block.x, block.y - 1, block.z));
            if !below.has(flags::COLLISION_FULL_BLOCK) && below.collision_shape().is_empty() {
                return false;
            }
            let t = mob.kind.type_id();
            (1..3).all(|i| {
                let s = level.block(BlockPos::new(block.x, block.y + i, block.z));
                valid_empty_spawn_block(s, t)
            }) && {
                let s = level.block(block);
                !s.has(flags::COLLISION_FULL_BLOCK)
                    && !s.has(flags::SIGNAL_SOURCE)
                    && valid_empty_spawn_block_ignoring_fluid(s, t)
            }
        }
        _ => !liquid() && unobstructed(),
    }
}

/// `EntityType.immuneTo`: the block tag of the type's builder (`isBlockDangerous` is false
/// for those blocks).
fn immune_to(type_id: EntityTypeId, state: BlockState) -> bool {
    use vanillite_data::entity::ids as e;
    let tags = super::fam::wildlife::block_tags();
    let tag = match type_id {
        t if t == e::FOX => &tags.fox_immune_to,
        t if t == e::POLAR_BEAR => &tags.polar_bear_immune_to,
        t if t == e::SNOW_GOLEM => &tags.snow_golem_immune_to,
        t if t == e::STRAY => &tags.stray_immune_to,
        t if t == e::WITHER => &tags.wither_immune_to,
        t if t == e::WITHER_SKELETON => &tags.wither_skeleton_immune_to,
        _ => return false,
    };
    tag.contains(state)
}

/// `isValidEmptySpawnBlock` with an empty fluid state.
fn valid_empty_spawn_block_ignoring_fluid(state: BlockState, type_id: EntityTypeId) -> bool {
    let name = state.block().name();
    if name.contains("rail") {
        return false;
    }
    let fire_immune = type_id.info().fire_immune;
    let dangerous = (!fire_immune
        && (state.block() == ids::FIRE
            || state.block() == ids::SOUL_FIRE
            || state.block() == ids::MAGMA_BLOCK
            || state.block() == ids::LAVA_CAULDRON
            || name.ends_with("campfire")))
        || state.block() == ids::WITHER_ROSE
        || state.block() == ids::SWEET_BERRY_BUSH
        || state.block() == ids::CACTUS
        || state.block() == ids::POWDER_SNOW;
    !dangerous || immune_to(type_id, state)
}

/// `isValidPositionForMob`.
fn valid_position_for_mob(
    level: &Level,
    input: &SpawnInput<'_>,
    mob: &MobData,
    pos: Vec3,
    nearest: f64,
) -> bool {
    let category = Category::of(mob.kind.type_id()).unwrap_or(Category::Monster);
    let despawn = f64::from(input.limits.despawn_distance(category));
    if nearest > despawn * despawn && super::fam::remove_when_far_away_mob(mob, nearest) {
        return false;
    }
    let block = BlockPos::containing(pos.x, pos.y, pos.z);
    mob_check_spawn_rules(level, &input.env, mob, block)
        && check_spawn_obstruction(level, &input.env, mob, pos)
}
/// `SpawnPlacementType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    OnGround,
    InWater,
    InLava,
    NoRestrictions,
}

/// `SpawnPlacements`: the placement type and heightmap of an entity type (unregistered types
/// have no restrictions).
pub fn placement(t: EntityTypeId) -> (Placement, HeightmapKind) {
    use vanillite_data::entity::ids as e;
    let no_leaves = HeightmapKind::MotionBlockingNoLeaves;
    let p = if [
        e::AXOLOTL,
        e::COD,
        e::DOLPHIN,
        e::DROWNED,
        e::GUARDIAN,
        e::PUFFERFISH,
        e::SALMON,
        e::SQUID,
        e::TROPICAL_FISH,
        e::GLOW_SQUID,
        e::NAUTILUS,
        e::ELDER_GUARDIAN,
    ]
    .contains(&t)
    {
        Placement::InWater
    } else if t == e::STRIDER {
        Placement::InLava
    } else if [
        e::EVOKER,
        e::FOX,
        e::ILLUSIONER,
        e::PANDA,
        e::PHANTOM,
        e::SHULKER,
        e::TRADER_LLAMA,
        e::VEX,
        e::VINDICATOR,
        e::WARDEN,
    ]
    .contains(&t)
        || !Kind::of(t).is_some_and(registered_placement)
    {
        Placement::NoRestrictions
    } else {
        Placement::OnGround
    };
    let heightmap = if t == e::OCELOT || t == e::PARROT {
        HeightmapKind::MotionBlocking
    } else {
        no_leaves
    };
    (p, heightmap)
}

/// Whether a kind has a `SpawnPlacements` entry (the others have no restrictions).
fn registered_placement(k: Kind) -> bool {
    !matches!(
        k,
        Kind::ZombieNautilus | Kind::CopperGolem | Kind::Allay | Kind::Sniffer | Kind::Tadpole
    )
}

/// `SpawnPlacements.isSpawnPositionOk` by the type's placement.
pub fn spawn_position_ok(level: &Level, type_id: EntityTypeId, pos: BlockPos) -> bool {
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    match placement(type_id).0 {
        Placement::NoRestrictions => true,
        Placement::InWater => {
            super::is_water(level.block(pos)) && !level.block(above).has(flags::REDSTONE_CONDUCTOR)
        }
        Placement::InLava => super::is_lava(level.block(pos)),
        Placement::OnGround => {
            let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
            if !is_valid_spawn(level, level.block(below), type_id) {
                return false;
            }
            valid_empty_spawn_block(level.block(pos), type_id)
                && valid_empty_spawn_block(level.block(above), type_id)
        }
    }
}

/// `BlockState.isValidSpawn(level, pos, type)` with the light check of the block below.
pub fn is_valid_spawn(_level: &Level, state: BlockState, type_id: EntityTypeId) -> bool {
    valid_spawn_floor(state, type_id)
}

/// `BlockState.isValidSpawn` for a mob: a sturdy top face that is not too bright, except the
/// blocks with their own rule in `Blocks` (`never`, `always`, `ocelotOrParrot`, ice for polar
/// bears, magma for fire immune mobs).
pub fn valid_spawn_floor(state: BlockState, type_id: EntityTypeId) -> bool {
    let name = state.block().name();
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    // `Blocks::never`.
    if matches!(
        name,
        "bedrock"
            | "glass"
            | "tinted_glass"
            | "moving_piston"
            | "barrier"
            | "chorus_flower"
            | "scaffolding"
    ) || name.ends_with("_stained_glass")
        || name.ends_with("trapdoor")
        || name.ends_with("copper_grate")
    {
        return false;
    }
    // `Blocks::always`.
    if matches!(
        name,
        "soul_sand" | "carved_pumpkin" | "jack_o_lantern" | "redstone_lamp" | "mud"
    ) {
        return true;
    }
    // `Blocks::ocelotOrParrot` (every leaves block).
    if name.ends_with("leaves") {
        return type_id == entity_ids::OCELOT || type_id == entity_ids::PARROT;
    }
    if name == "ice" || name == "frosted_ice" {
        return type_id == entity_ids::POLAR_BEAR;
    }
    if state.block() == ids::MAGMA_BLOCK {
        return type_id.info().fire_immune;
    }
    // Bit 1 of `sturdy_full` is the up face.
    state.misc().sturdy_full & 2 != 0 && state.light_emission() < 14
}

/// `NaturalSpawner.isValidEmptySpawnBlock`.
pub fn valid_empty_spawn_block(state: BlockState, type_id: EntityTypeId) -> bool {
    if state.has(flags::COLLISION_FULL_BLOCK) || state.has(flags::SIGNAL_SOURCE) {
        return false;
    }
    if !state.fluid_state().is_empty() {
        return false;
    }
    let name = state.block().name();
    if name.contains("rail") {
        return false;
    }
    // `EntityType.isBlockDangerous`.
    let fire_immune = type_id.info().fire_immune;
    let dangerous = (!fire_immune
        && (state.block() == ids::FIRE
            || state.block() == ids::SOUL_FIRE
            || state.block() == ids::MAGMA_BLOCK
            || state.block() == ids::LAVA_CAULDRON
            || name.ends_with("campfire")))
        || state.block() == ids::WITHER_ROSE
        || state.block() == ids::SWEET_BERRY_BUSH
        || state.block() == ids::CACTUS
        || state.block() == ids::POWDER_SNOW;
    !dangerous || immune_to(type_id, state)
}

/// The spawn rules (`SpawnPlacements.checkSpawnRules`) of a kind.
pub fn check_spawn_rules(
    level: &Level,
    env: &Environment,
    kind: Kind,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    super::fam::check_spawn_rules(kind, level, env, pos, random, reason)
}

/// `Monster.checkMonsterSpawnRules` for monsters, `Animal.checkAnimalSpawnRules` for the
/// others.
pub fn default_spawn_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    if kind.is_monster() {
        check_monster_spawn_rules(kind, level, env, pos, random, reason)
    } else {
        check_animal_spawn_rules(kind, level, env, pos, reason)
    }
}

/// The entity types built with `notInPeaceful()` (`EntityType.isAllowedInPeaceful`).
pub const NOT_IN_PEACEFUL: [&str; 38] = [
    "blaze",
    "bogged",
    "breeze",
    "cave_spider",
    "creaking",
    "creeper",
    "drowned",
    "elder_guardian",
    "enderman",
    "endermite",
    "evoker",
    "ghast",
    "giant",
    "guardian",
    "hoglin",
    "husk",
    "illusioner",
    "magma_cube",
    "parched",
    "phantom",
    "piglin_brute",
    "pillager",
    "ravager",
    "silverfish",
    "skeleton",
    "slime",
    "spider",
    "stray",
    "vex",
    "vindicator",
    "warden",
    "witch",
    "wither",
    "wither_skeleton",
    "zoglin",
    "zombie",
    "zombie_villager",
    "zombified_piglin",
];

/// `EntityType.isAllowedInPeaceful` of the type `path` (without namespace).
pub fn allowed_in_peaceful(path: &str) -> bool {
    !NOT_IN_PEACEFUL.contains(&path)
}

/// `EntitySpawnReason.isSpawner`.
pub fn is_spawner(reason: SpawnReason) -> bool {
    matches!(reason, SpawnReason::Spawner | SpawnReason::TrialSpawner)
}

/// `Mob.checkMobSpawnRules`: a valid spawn block below (spawners ignore it).
pub fn check_mob_spawn_rules(
    kind: Kind,
    level: &Level,
    pos: BlockPos,
    reason: SpawnReason,
) -> bool {
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    is_spawner(reason) || is_valid_spawn(level, level.block(below), kind.type_id())
}

/// `Monster.checkMonsterSpawnRules`: dark enough (trial spawners ignore light) and the mob
/// rules.
pub fn check_monster_spawn_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    (reason == SpawnReason::TrialSpawner || is_dark_enough_to_spawn(level, env, pos, random))
        && check_mob_spawn_rules(kind, level, pos, reason)
}

/// `Monster.checkAnyLightMonsterSpawnRules`.
pub fn check_any_light_monster_spawn_rules(
    kind: Kind,
    level: &Level,
    pos: BlockPos,
    reason: SpawnReason,
) -> bool {
    check_mob_spawn_rules(kind, level, pos, reason)
}

/// `Monster.checkSurfaceMonstersSpawnRules`: the monster rules under the open sky.
pub fn check_surface_monsters_spawn_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    check_monster_spawn_rules(kind, level, env, pos, random, reason)
        && (is_spawner(reason) || super::can_see_sky(level, pos))
}

/// `Animal.checkAnimalSpawnRules`: `#animals_spawnable_on` below and bright enough
/// (`isBrightEnoughToSpawn`; chunk generation ignores the light).
pub fn check_animal_spawn_rules(
    _kind: Kind,
    level: &Level,
    _env: &Environment,
    pos: BlockPos,
    reason: SpawnReason,
) -> bool {
    let below = level.block(BlockPos::new(pos.x, pos.y - 1, pos.z));
    let bright = reason == SpawnReason::ChunkGeneration || {
        let (sky, block) = level.light(pos);
        sky.max(block) > 8
    };
    in_block_tag(below, "minecraft:animals_spawnable_on") && bright
}

/// Whether a block is in a block tag of the data packs (`false` without data packs, except
/// the grass block for `#animals_spawnable_on`).
pub fn in_block_tag(state: BlockState, tag: &str) -> bool {
    match crate::player::play::game_data() {
        Some(d) if d.block_tags.contains_key(tag) => d.block_in_tag(state.block().0, tag),
        _ => tag == "minecraft:animals_spawnable_on" && state.block() == ids::GRASS_BLOCK,
    }
}

/// `Monster.isDarkEnoughToSpawn`.
pub fn is_dark_enough_to_spawn(
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
) -> bool {
    let (sky, block) = level.light(pos);
    if sky > random.next_int_bounded(32) {
        return false;
    }
    if env.monster_block_light_limit < 15 && block > env.monster_block_light_limit {
        return false;
    }
    let brightness = if env.thundering {
        let darkened = Environment {
            sky_darken: 10,
            ..*env
        };
        max_local_raw_brightness(level, &darkened, pos)
    } else {
        max_local_raw_brightness(level, env, pos)
    };
    // `monsterSpawnLightTest.sample`: a constant (the Nether's 7) draws nothing.
    let (lo, hi) = env.monster_light_test;
    let limit = if lo == hi {
        lo
    } else {
        random.next_int_between_inclusive(lo, hi)
    };
    brightness <= limit
}

/// `getHeight(heightmap, x, z)`: the first free y of a column.
pub fn heightmap_y(level: &Level, kind: HeightmapKind, x: i32, z: i32) -> i32 {
    surface_y(level, kind, x, z)
}

pub fn surface_y(level: &Level, kind: HeightmapKind, x: i32, z: i32) -> i32 {
    surface_y_of(
        level,
        level
            .chunks
            .chunk(ChunkPos::new(x >> 4, z >> 4))
            .map(|c| &**c),
        kind,
        x,
        z,
    )
}

/// [`surface_y`] with the chunk of `x`/`z` already looked up.
fn surface_y_of(
    level: &Level,
    chunk: Option<&ProtoChunk>,
    kind: HeightmapKind,
    x: i32,
    z: i32,
) -> i32 {
    let Some(c) = chunk else {
        return level.min_y();
    };
    let Some(h) = c.chunk.heightmaps().get(kind) else {
        return level.min_y();
    };
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    let top = h.get((x & 15) as u32, (z & 15) as u32) as i32 + c.chunk.min_y();
    top
}

/// `getTopNonCollidingPos` for ground mobs.
fn top_non_colliding(
    level: &Level,
    env: &Environment,
    type_id: EntityTypeId,
    x: i32,
    z: i32,
) -> BlockPos {
    let (placement, heightmap) = placement(type_id);
    let y = surface_y(level, heightmap, x, z);
    let mut pos = BlockPos::new(x, y, z);
    if env.has_ceiling {
        loop {
            pos = BlockPos::new(x, pos.y - 1, z);
            if level.block(pos).is_air() || pos.y <= level.min_y() {
                break;
            }
        }
        loop {
            pos = BlockPos::new(x, pos.y - 1, z);
            if !(level.block(pos).is_air() && pos.y > level.min_y()) {
                break;
            }
        }
    }
    // `adjustSpawnPosition` of `ON_GROUND`: onto a pathfindable block below.
    if placement == Placement::OnGround {
        let below = BlockPos::new(x, pos.y - 1, z);
        if super::path::is_pathfindable_land(level.block(below)) {
            return below;
        }
    }
    pos
}

/// `NaturalSpawner.spawnMobsForChunkGeneration`: the animals of a freshly generated chunk.
pub fn spawn_for_chunk_generation(
    level: &mut Level,
    table: &SpawnTable,
    env: &Environment,
    chunk: ChunkPos,
    seed: i64,
    difficulty: Difficulty,
) {
    let source = BlockPos::new(chunk.x * 16 + 8, 0, chunk.z * 16 + 8);
    let biome = level.biome(BlockPos::new(
        source.x,
        surface_y(level, HeightmapKind::WorldSurface, source.x, source.z),
        source.z,
    ));
    let Some(b) = table.biome(biome) else {
        return;
    };
    let mobs = b.by_category[Category::Creature.index()].clone();
    if mobs.is_empty() {
        return;
    }
    let probability = b.creature_probability;
    // `WorldgenRandom.setDecorationSeed(seed, minX, minZ)` on a fresh random.
    let mut random = LegacyRandom::new(seed);
    let a = random.next_long() | 1;
    let c = random.next_long() | 1;
    let decoration = i64::from(chunk.x * 16)
        .wrapping_mul(a)
        .wrapping_add(i64::from(chunk.z * 16).wrapping_mul(c))
        ^ seed;
    random.set_seed(decoration);
    let (xo, zo) = (chunk.x * 16, chunk.z * 16);
    while random.next_float() < probability {
        let Some(entry) = pick(&mobs, &mut random) else {
            continue;
        };
        let count = random.next_int_between_inclusive(entry.min, entry.max);
        let mut x = xo + random.next_int_bounded(16);
        let mut z = zo + random.next_int_bounded(16);
        let (start_x, start_z) = (x, z);
        let mut group_index = 0;
        let mut group = GroupData::None;
        for _ in 0..count {
            let mut success = false;
            let mut attempts = 0;
            while !success && attempts < 4 {
                attempts += 1;
                let pos = top_non_colliding(level, env, entry.type_id, x, z);
                if entry.type_id.info().summonable && spawn_position_ok(level, entry.type_id, pos) {
                    let width = f64::from(entry.type_id.info().width);
                    let fx =
                        f64::from(x).clamp(f64::from(xo) + width, f64::from(xo) + 16.0 - width);
                    let fz =
                        f64::from(z).clamp(f64::from(zo) + width, f64::from(zo) + 16.0 - width);
                    let at = Vec3::new(fx, f64::from(pos.y), fz);
                    let aabb = super::Dimensions::of(entry.type_id).bounding_box(at);
                    let mut scratch = Vec::new();
                    let block = BlockPos::containing(fx, f64::from(pos.y), fz);
                    let rules = Kind::of(entry.type_id).is_some_and(|k| {
                        check_spawn_rules(
                            level,
                            env,
                            k,
                            block,
                            &mut random,
                            SpawnReason::ChunkGeneration,
                        )
                    });
                    if physics::no_collision(&level.view(), &aabb, &mut scratch) && rules {
                        let kind = Kind::of(entry.type_id);
                        let y_rot = random.next_float() * 360.0;
                        if let Some(kind) = kind {
                            let mut mob = MobData::new(kind, crate::mob::new_entity_seed());
                            if mob_check_spawn_rules(level, env, &mob, block)
                                && check_spawn_obstruction(level, env, &mob, at)
                            {
                                let extra = finalize_in_group(
                                    level,
                                    Some(table),
                                    env,
                                    &mut mob,
                                    block,
                                    difficulty,
                                    group_index,
                                    &mut group,
                                    SpawnReason::ChunkGeneration,
                                    // `WorldGenRegion.getRandom()` is a positional random of
                                    // the region; this one is distribution equal.
                                    &mut random,
                                );
                                group_index += 1;
                                add_mob_with(
                                    level,
                                    mob,
                                    at,
                                    y_rot,
                                    extra,
                                    SpawnReason::ChunkGeneration,
                                );
                                success = true;
                            }
                        }
                    }
                }
                x += random.next_int_bounded(5) - random.next_int_bounded(5);
                z += random.next_int_bounded(5) - random.next_int_bounded(5);
                while x < xo || x >= xo + 16 || z < zo || z >= zo + 16 {
                    x = start_x + random.next_int_bounded(5) - random.next_int_bounded(5);
                    z = start_z + random.next_int_bounded(5) - random.next_int_bounded(5);
                }
            }
        }
    }
}

/// Spawns a mob by command (`/summon`): loaded facing south (`Rotation` defaults to zero),
/// then `finalizeSpawn` with `COMMAND` when `finalize` (`/summon` without NBT). `reason` is the
/// plugins' `vanillite_abi::spawn_reason` (`COMMAND`, or `PLUGIN` for `entity_spawn`); a
/// plugin may cancel the spawn (the returned mob is then gone).
pub fn summon(
    level: &mut Level,
    table: Option<&SpawnTable>,
    kind: Kind,
    pos: Vec3,
    difficulty: Difficulty,
    reason: u32,
    finalize_spawn: bool,
) -> EntityId {
    summon_seeded(
        level,
        table,
        kind,
        pos,
        difficulty,
        reason,
        finalize_spawn,
        None,
    )
}

/// [`summon`] of a mob whose constructor already took its seed from the entity seed stream
/// (`seed`), else it takes the next one.
#[allow(clippy::too_many_arguments)]
pub fn summon_seeded(
    level: &mut Level,
    table: Option<&SpawnTable>,
    kind: Kind,
    pos: Vec3,
    difficulty: Difficulty,
    reason: u32,
    finalize_spawn: bool,
    seed: Option<i64>,
) -> EntityId {
    let seed = seed.unwrap_or_else(crate::mob::new_entity_seed);
    if kind == Kind::EnderDragon {
        crate::mob::boss::dragon::part_seeds();
    }
    let mut mob = MobData::new(kind, seed);
    let block = BlockPos::containing(pos.x, pos.y, pos.z);
    let env = Environment::default();
    let extra = if finalize_spawn {
        with_level_random(level, |level, random| {
            finalize(
                level,
                table,
                &env,
                &mut mob,
                block,
                difficulty,
                0,
                SpawnReason::Command,
                random,
            )
        })
    } else {
        Vec::new()
    };
    // `EntityType.loadEntityRecursive` of `{id}` reads no `Rotation`: yaw, head and body 0;
    // `snapTo` keeps it (no draw from the level random).
    add_mob_with_code(level, mob, pos, 0.0, extra, reason)
}

/// `CatSpawner`'s state.
#[derive(Debug, Default)]
pub struct CatSpawner {
    next_tick: i32,
}

/// `CatSpawner.tick`: every minute a cat may spawn 8–24 blocks from a random player: in a
/// village (`isCloseToVillage(pos, 2)`, more than four occupied homes within 48 blocks and fewer
/// than five cats) or in a swamp hut (`#cats_spawn_in`, a black persistent cat when none is
/// within 16 blocks).
pub fn cat_spawner_tick(
    level: &mut Level,
    table: Option<&SpawnTable>,
    spawner: &mut CatSpawner,
    structures: &mut super::structures::StructureIndex,
    env: &Environment,
    difficulty: Difficulty,
    players: &[EntityId],
) {
    spawner.next_tick -= 1;
    if spawner.next_tick > 0 {
        return;
    }
    spawner.next_tick = 1200;
    if players.is_empty() {
        return;
    }
    // `getRandomPlayer`.
    let i = level
        .random()
        .next_int_bounded(i32::try_from(players.len()).unwrap_or(i32::MAX));
    let Some(p) = usize::try_from(i)
        .ok()
        .and_then(|i| players.get(i))
        .and_then(|&id| level.entities.store.get(id))
        .map(|e| e.pos())
    else {
        return;
    };
    let r = level.random();
    let x = (8 + r.next_int_bounded(24)) * if r.next_boolean() { -1 } else { 1 };
    let z = (8 + r.next_int_bounded(24)) * if r.next_boolean() { -1 } else { 1 };
    let base = BlockPos::containing(p.x, p.y, p.z);
    let pos = BlockPos::new(base.x + x, base.y, base.z + z);
    let loaded = (-10..=10).step_by(10).all(|dx| {
        (-10..=10)
            .step_by(10)
            .all(|dz| level.is_loaded(BlockPos::new(pos.x + dx, pos.y, pos.z + dz)))
    });
    if !loaded || !spawn_position_ok(level, vanillite_data::entity::ids::CAT, pos) {
        return;
    }
    if level.entities.poi.is_close_to_village(pos, 2) {
        // `spawnInVillage`.
        let homes = level.entities.poi.count_in_range(
            |t| t == crate::poi::PoiType::Home,
            pos,
            48,
            crate::poi::Occupancy::IsOccupied,
        );
        if homes <= 4 {
            return;
        }
        let area = vanillite_base::Aabb::new(
            f64::from(pos.x) - 48.0,
            f64::from(pos.y) - 8.0,
            f64::from(pos.z) - 48.0,
            f64::from(pos.x) + 49.0,
            f64::from(pos.y) + 9.0,
            f64::from(pos.z) + 49.0,
        );
        let mut cats = 0;
        let ents = &level.entities;
        ents.store.for_each_in(&area, false, |id| {
            cats += usize::from(ents.mob(id).is_some_and(|m| m.kind == Kind::Cat));
        });
        if cats < 5 {
            spawn_cat(level, table, env, difficulty, pos, false);
        }
        return;
    }
    let mut in_hut = false;
    structures.structures_at(level, pos, |name, in_piece, _| {
        in_hut |= in_piece && name == "minecraft:swamp_hut";
    });
    if !in_hut {
        return;
    }
    // `spawnInHut`: no cat within 16 × 8 × 16.
    let area = vanillite_base::Aabb::new(
        f64::from(pos.x) - 16.0,
        f64::from(pos.y) - 8.0,
        f64::from(pos.z) - 16.0,
        f64::from(pos.x) + 17.0,
        f64::from(pos.y) + 9.0,
        f64::from(pos.z) + 17.0,
    );
    let mut cats = false;
    let ents = &level.entities;
    ents.store.for_each_in(&area, false, |id| {
        cats |= ents.mob(id).is_some_and(|m| m.kind == Kind::Cat);
    });
    if cats {
        return;
    }
    spawn_cat(level, table, env, difficulty, pos, true);
}

/// `CatSpawner.spawnCat(pos, level, makePersistent)`: black in swamp huts, else a random
/// variant (all black only under a full moon).
fn spawn_cat(
    level: &mut Level,
    table: Option<&SpawnTable>,
    env: &Environment,
    difficulty: Difficulty,
    pos: BlockPos,
    in_hut: bool,
) {
    let seed = crate::mob::new_entity_seed();
    let mut cat = MobData::new(Kind::Cat, seed);
    // The moon's brightness is at least 0.9 at full moon only (surface slime chance 0.5).
    let full_moon = env.surface_slime_chance >= 0.45;
    let extra = with_level_random(level, |level, random| {
        let extra = finalize(
            level,
            table,
            env,
            &mut cat,
            pos,
            difficulty,
            0,
            SpawnReason::Natural,
            random,
        );
        // The variant's spawn conditions with the level random.
        cat.variant = super::fam::companion::feline::pick_cat_variant(random, in_hut, full_moon);
        extra
    });
    if in_hut {
        cat.persistent = true;
    }
    let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    add_mob_with(level, cat, at, 0.0, extra, SpawnReason::Event);
}

/// Counts the mobs per kind (for `/vanillite` style statistics and tests).
pub fn count_by_kind(level: &Level) -> FxHashMap<Kind, usize> {
    let mut out = FxHashMap::default();
    for (_, m) in level.entities.mobs.iter() {
        *out.entry(m.kind).or_insert(0) += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level::tests::flat_level;
    use crate::level::tiles::{Fork, inline_fork};
    use vanillite_data::entity::ids as e;

    /// A table whose first biome (the flat test world's plains) spawns zombies and spiders.
    fn table() -> SpawnTable {
        let mut plains = BiomeSpawns {
            name: "minecraft:plains".into(),
            ..BiomeSpawns::default()
        };
        let entry = |type_id, weight, min, max| SpawnEntry {
            type_id,
            weight,
            min,
            max,
        };
        plains.by_category[Category::Monster.index()] =
            vec![entry(e::ZOMBIE, 100, 4, 4), entry(e::SPIDER, 100, 4, 4)];
        plains.by_category[Category::Creature.index()] =
            vec![entry(e::COW, 8, 4, 4), entry(e::SHEEP, 12, 4, 4)];
        SpawnTable::new(vec![plains], FxHashMap::default())
    }

    /// Runs `ticks` spawn rounds with one player at the origin and returns every mob's type
    /// and position in spawn order, and the next value of the level's random.
    fn spawned(ticks: usize, run: Option<&TileRun<'_>>) -> (Vec<(EntityTypeId, [i64; 3])>, i32) {
        let mut level = flat_level();
        let mut ids: vanillite_base::SlotMap<EntityId, ()> = vanillite_base::SlotMap::default();
        let players = [(ids.insert(()), Vec3::new(8.5, -60.0, 8.5))];
        let table = table();
        let mut spawner = Spawner::default();
        let input = SpawnInput {
            players: &players,
            spawn_point: None,
            spawn_enemies: true,
            difficulty: Difficulty::Normal,
            // Night: dark enough for monsters under the open sky.
            env: Environment {
                sky_darken: 11,
                ..Environment::default()
            },
            limits: SpawnLimits::default(),
            thunder: crate::lightning::StrikeInput {
                spawn_mobs: true,
                clock_time: 0,
            },
            natural: true,
        };
        crate::mob::set_entity_seed_stream(Some(7));
        for _ in 0..ticks {
            level.tick_time();
            match run {
                Some(run) => tick_tiled(
                    &mut level,
                    &table,
                    &mut spawner,
                    &input,
                    true,
                    run,
                    &Phases::INLINE,
                ),
                None => tick(
                    &mut level,
                    &table,
                    &mut spawner,
                    &input,
                    true,
                    &Phases::INLINE,
                ),
            }
        }
        crate::mob::set_entity_seed_stream(None);
        #[allow(clippy::cast_possible_truncation)]
        let mobs = level
            .entities
            .mobs
            .iter()
            .filter_map(|(id, _)| level.entities.store.get(id))
            .map(|entity| {
                let p = entity.pos();
                let fixed = |v: f64| (v * 16.0).round() as i64;
                (entity.kind(), [fixed(p.x), fixed(p.y), fixed(p.z)])
            })
            .collect();
        (mobs, level.random().next_int())
    }

    fn tiles<'a>(fork: &'a Fork<'a>) -> TileRun<'a> {
        // The test world ticks the chunks around the origin: tiles of 2 × 2 chunks.
        TileRun {
            size: 2,
            min_chunks: 0,
            fork,
        }
    }

    /// The round on the world itself draws from the level's random in Vanilla's order: the
    /// spawns of a seeded world are those of the code without the tile region mode (the values
    /// were taken from it; they change only when the spawn logic itself does).
    #[test]
    fn the_world_round_keeps_its_spawns() {
        let (mobs, next) = spawned(60, None);
        let digest = mobs.iter().fold(0u64, |h, (kind, p)| {
            let mut h = h.wrapping_mul(0x100_0000_01B3) ^ u64::from(kind.0);
            for v in p {
                #[allow(clippy::cast_sign_loss)]
                let v = *v as u64;
                h = h.wrapping_mul(0x100_0000_01B3) ^ v;
            }
            h
        });
        assert_eq!((mobs.len(), digest, next), EXPECTED_WORLD_ROUND);
    }

    /// The number of mobs, the digest of their types and positions and the level random's
    /// next value after 60 rounds.
    /// (#217: `checkSpawnObstruction` tests entities that block building, pack members no
    /// longer spawn inside each other; #257: the player ticket makes one more chunk ring block
    /// ticking and a spawn candidate.)
    const EXPECTED_WORLD_ROUND: (usize, u64, i32) =
        (85, 15_489_036_134_221_526_530, -2_082_463_062);

    /// Packs wander up to 20 blocks from their start, across the 32-block tiles of this test:
    /// the mobs land in several tiles, only in ticking chunks, and the tiles together respect
    /// the mob cap up to one tick's spawns.
    #[test]
    fn tiled_spawns_cross_tile_borders_and_stay_in_ticking_chunks() {
        let fork: &Fork<'_> = &inline_fork;
        let (mobs, _) = spawned(60, Some(&tiles(fork)));
        assert!(mobs.len() > 8, "{} mobs", mobs.len());
        let level = flat_level();
        let mut seen = std::collections::BTreeSet::new();
        for (_, p) in &mobs {
            #[allow(clippy::cast_possible_truncation)]
            let chunk = ChunkPos::new((p[0] >> 8) as i32, (p[2] >> 8) as i32);
            assert!(level.is_block_ticking(chunk), "mob in {chunk:?}");
            seen.insert(TilePos::of(chunk, 2));
        }
        assert!(seen.len() >= 3, "tiles {seen:?}");
        // The monster cap is 70; the tiles that spawn in the same tick can overshoot it by
        // their packs (a few of the tiles over the 9 × 9 block-ticking chunks of view 3).
        assert!(mobs.len() <= 70 + 6 * 12, "{} mobs", mobs.len());
    }

    /// One thread or many: the same mobs at the same places.
    #[test]
    fn tiled_spawns_do_not_depend_on_threads() {
        let threaded = |n: usize, job: &(dyn Fn(usize) + Sync)| {
            std::thread::scope(|scope| {
                for i in 0..n {
                    scope.spawn(move || job(i));
                }
            });
        };
        let inline: &Fork<'_> = &inline_fork;
        let (a, _) = spawned(60, Some(&tiles(inline)));
        let (b, _) = spawned(60, Some(&tiles(&threaded)));
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    /// With everything in one tile or too few chunks the tiled round is the world round.
    #[test]
    fn a_tiled_round_that_does_not_apply_is_the_world_round() {
        let fork: &Fork<'_> = &inline_fork;
        let small = TileRun {
            size: 2,
            min_chunks: 1_000_000,
            fork,
        };
        assert_eq!(spawned(60, Some(&small)), spawned(60, None));
    }
}
