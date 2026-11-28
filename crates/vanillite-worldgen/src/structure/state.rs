//! `ChunkGeneratorStructureState`: the seed-dependent view of the structure sets a generator
//! places (possible sets, ring positions of concentric placements, the dimension origin).

use vanillite_base::ChunkPos;
use vanillite_base::random::{LegacyRandom, RandomSource};

use super::StructureSetId;
use super::placement::{ConcentricRings, java_round};
use crate::biome::BiomeResolver;
use crate::generator::TerrainGenerator;

/// `ChunkGeneratorStructureState` of a [`TerrainGenerator`] (`createForNormal` with the world
/// seed as level seed and concentric rings seed).
///
/// The possible structure sets are fixed when the generator is built; ring positions are
/// computed once per placement on first use (Vanilla computes all of them on the first query,
/// each from its own generator, so the result is the same).
#[derive(Clone, Copy, Debug)]
pub struct StructureState<'g> {
    generator: &'g TerrainGenerator,
}

impl<'g> StructureState<'g> {
    /// The state of a generator.
    pub fn new(generator: &'g TerrainGenerator) -> Self {
        Self { generator }
    }

    /// The generator.
    pub fn generator(&self) -> &'g TerrainGenerator {
        self.generator
    }

    /// `getLevelSeed()`.
    pub fn level_seed(&self) -> i64 {
        self.generator.seed()
    }

    /// `getDimensionOrigin()`: `ChunkGenerator.getOrigin(randomState)`.
    pub fn dimension_origin(&self) -> ChunkPos {
        self.generator.origin()
    }

    /// `possibleStructureSets()`: the sets with a structure whose biomes the biome source can
    /// return, in registry order.
    pub fn possible_structure_sets(&self) -> &'g [StructureSetId] {
        self.generator.structures().possible_sets()
    }

    /// `getRingPositionsFor(placement)`: `None` for placements of sets that cannot generate.
    pub fn ring_positions_for(&self, rings: &'g ConcentricRings) -> Option<&'g [ChunkPos]> {
        if !rings.placeable {
            return None;
        }
        Some(
            rings
                .positions
                .get_or_init(|| self.generate_ring_positions(rings)),
        )
    }

    /// `hasStructureChunkInRange(set, sourceX, sourceZ, range)`.
    pub fn has_structure_chunk_in_range(
        &self,
        set: StructureSetId,
        x: i32,
        z: i32,
        range: i32,
    ) -> bool {
        let placement = &self.generator.structures().set(set).placement;
        for test_x in x - range..=x + range {
            for test_z in z - range..=z + range {
                if placement.is_structure_chunk(self, test_x, test_z) {
                    return true;
                }
            }
        }
        false
    }

    /// `generateRingPositions(set, placement)`.
    fn generate_ring_positions(&self, rings: &ConcentricRings) -> Vec<ChunkPos> {
        if rings.count == 0 {
            return Vec::new();
        }
        let distance = rings.distance;
        let count = rings.count;
        let mut spread = rings.spread;
        // `RandomSource.create()` then `setSeed(concentricRingsSeed)`.
        let mut random = LegacyRandom::new(0);
        random.set_seed(self.generator.concentric_rings_seed());
        let mut angle = random.next_double() * std::f64::consts::PI * 2.0;
        let mut position_in_circle = 0;
        let mut circle = 0;
        let mut positions = Vec::with_capacity(count as usize);
        for i in 0..count {
            let dist = f64::from(4 * distance + distance * circle * 6)
                + (random.next_double() - 0.5) * (f64::from(distance) * 2.5);
            let initial_x = java_round(crate::mth::math_cos(angle) * dist);
            let initial_z = java_round(crate::mth::math_sin(angle) * dist);
            let mut search_random = random.fork();
            positions.push(
                self.find_ring_biome(rings, initial_x, initial_z, &mut search_random)
                    .unwrap_or(ChunkPos::new(initial_x, initial_z)),
            );
            angle += (std::f64::consts::PI * 2.0) / f64::from(spread);
            position_in_circle += 1;
            if position_in_circle == spread {
                circle += 1;
                position_in_circle = 0;
                spread += 2 * spread / (circle + 1);
                spread = spread.min(count - i);
                angle += random.next_double() * std::f64::consts::PI * 2.0;
            }
        }
        positions
    }

    /// `biomeSource.findBiomeHorizontal(sectionToBlockCoord(x, 8), 0, sectionToBlockCoord(z, 8),
    /// 112, preferredBiomes::contains, random, randomState)`, as a chunk position.
    fn find_ring_biome(
        &self,
        rings: &ConcentricRings,
        chunk_x: i32,
        chunk_z: i32,
        random: &mut LegacyRandom,
    ) -> Option<ChunkPos> {
        let generator = self.generator;
        let mut resolver = generator
            .biome_source()
            .create_caching_resolver(generator.program(), generator.climate());
        let (origin_x, origin_z) = ((chunk_x << 4) + 8, (chunk_z << 4) + 8);
        let (center_x, center_z) = (origin_x >> 2, origin_z >> 2);
        let radius = 112 >> 2;
        let noise_y = 0;
        let mut result = None;
        let mut found = 0;
        // `findClosest == false`: only the outermost radius, scanned row by row.
        for z in -radius..=radius {
            for x in -radius..=radius {
                let (noise_x, noise_z) = (center_x + x, center_z + z);
                let biome = resolver.noise_biome(noise_x, noise_y, noise_z);
                if rings.preferred_biomes.contains(biome) {
                    if result.is_none() || random.next_int_bounded(found + 1) == 0 {
                        result = Some((noise_x << 2, noise_z << 2));
                    }
                    found += 1;
                }
            }
        }
        result.map(|(x, z)| ChunkPos::new(x >> 4, z >> 4))
    }
}
