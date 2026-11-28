//! `NoiseSpawnFinder`: the climate based search for a dimension's origin (the world spawn
//! area), used by `NoiseBasedChunkGenerator.getOrigin`.

use vanillite_base::ChunkPos;

use crate::biome::climate::{Parameter, quantize_coord};
use crate::density::{FnId, Program, SamplerContext};

/// A compiled `SpawnTargetPoint`: each density function with its target interval.
pub(crate) type SpawnTarget = Vec<(FnId, Parameter)>;

/// `NoiseSpawnFinder.findSpawnPosition(targetPoints, samplers)` as a chunk position
/// (`ChunkPos.containing`).
pub(crate) fn find_spawn_chunk(program: &Program, targets: &[SpawnTarget]) -> ChunkPos {
    // `SamplerContext.builder().enableCaches().build()`.
    let mut ctx = SamplerContext::new(true);
    let mut finder = Finder {
        program,
        targets,
        ctx: &mut ctx,
    };
    let mut result = finder.fitness_at(0, 0);
    result = finder.radial_search(result, 2048.0, 512.0);
    result = finder.radial_search(result, 512.0, 32.0);
    ChunkPos::new(result.x >> 4, result.z >> 4)
}

/// `NoiseSpawnFinder.Result`: a block column and its fitness (lower is better).
#[derive(Clone, Copy, Debug)]
struct Candidate {
    x: i32,
    z: i32,
    fitness: i64,
}

struct Finder<'a> {
    program: &'a Program,
    targets: &'a [SpawnTarget],
    ctx: &'a mut SamplerContext,
}

impl Finder<'_> {
    /// `radialSearch(samplers, targetPoints, maxRadius, radiusIncrement)`: rings around the
    /// best position so far, `float` angle and radius as in Vanilla.
    fn radial_search(&mut self, mut best: Candidate, max_radius: f32, increment: f32) -> Candidate {
        let mut angle = 0.0_f32;
        let mut radius = increment;
        let (origin_x, origin_z) = (best.x, best.z);
        while radius <= max_radius {
            let x = origin_x + (crate::mth::math_sin(f64::from(angle)) * f64::from(radius)) as i32;
            let z = origin_z + (crate::mth::math_cos(f64::from(angle)) * f64::from(radius)) as i32;
            let candidate = self.fitness_at(x, z);
            if candidate.fitness < best.fitness {
                best = candidate;
            }
            angle += increment / radius;
            if f64::from(angle) > std::f64::consts::PI * 2.0 {
                angle = 0.0;
                radius += increment;
            }
        }
        best
    }

    /// `getSpawnPositionAndFitness(samplers, targetPoints, blockX, blockZ)`.
    fn fitness_at(&mut self, x: i32, z: i32) -> Candidate {
        let (quart_x, quart_z) = ((x >> 2) << 2, (z >> 2) << 2);
        let mut min_fitness = i64::MAX;
        for target in self.targets {
            // `SpawnTargetPoint.sampleFitness(samplers, x, 0, z)`.
            let mut fitness = 0_i64;
            for (function, parameter) in target {
                let value = self
                    .program
                    .sample_value(self.ctx, *function, quart_x, 0, quart_z);
                let distance = parameter.distance(quantize_coord(value));
                fitness = fitness.wrapping_add(distance.wrapping_mul(distance));
            }
            min_fitness = min_fitness.min(fitness);
        }
        let bias = i64::from(x) * i64::from(x) + i64::from(z) * i64::from(z);
        Candidate {
            x,
            z,
            fitness: min_fitness.wrapping_mul(2048 * 2048).wrapping_add(bias),
        }
    }
}
