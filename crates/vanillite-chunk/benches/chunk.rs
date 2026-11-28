//! Benchmarks for block access and chunk packet encoding.

use std::hint::black_box;

use microbench::{BatchSize, Bench, bench_group, bench_main};
use vanillite_chunk::facts::{self, Tables};
use vanillite_chunk::packet::encode_chunk_with_light;
use vanillite_chunk::{Chunk, LightNibbles, Section};

const STATE_COUNT: usize = 35_723;
static FLAGS: [u8; STATE_COUNT] = {
    let mut flags = [facts::BLOCKS_MOTION | facts::BLOCKS_MOTION_NO_LEAVES; STATE_COUNT];
    flags[0] = facts::AIR;
    flags
};

fn install() {
    facts::install(Tables {
        block_flags: &FLAGS,
        biome_count: 65,
    });
}

/// Xorshift, so the benchmark needs no RNG crate.
fn positions(count: usize) -> Vec<(u32, u32, u32)> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..count)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let v = state as u32;
            (v & 15, (v >> 4) & 15, (v >> 8) & 15)
        })
        .collect()
}

fn section_with(states: u16) -> Section {
    let mut section = Section::new(1, 0);
    for (i, (x, y, z)) in positions(4096).into_iter().enumerate() {
        section.set_block(x, y, z, 1 + (i as u16 % states));
    }
    section
}

/// Typical overworld terrain: 9 varied sections below the surface, empty sky above.
fn terrain_chunk() -> Chunk {
    let mut chunk = Chunk::new(-64, 384, 1, 0);
    for index in 0..9 {
        *chunk.section_mut(index) = section_with(12);
    }
    chunk.prime_heightmaps();
    let light = chunk.light_mut();
    for index in 0..26 {
        light.block_mut()[index] = LightNibbles::Uniform(0);
        light.sky_mut()[index] = if index < 9 {
            LightNibbles::Uniform(0)
        } else if index < 12 {
            let mut layer = LightNibbles::Uniform(15);
            layer.set(1, 1, 1, 3);
            layer
        } else {
            LightNibbles::Absent
        };
    }
    chunk
}

fn access(c: &mut Bench) {
    install();
    let positions = positions(4096);
    for (name, states) in [("indirect4", 12u16), ("indirect8", 200), ("direct", 1000)] {
        let section = section_with(states);
        c.bench_function(format!("section/get_4096/{name}"), |b| {
            b.iter(|| {
                let mut sum = 0u32;
                for &(x, y, z) in &positions {
                    sum += u32::from(section.block(x, y, z));
                }
                black_box(sum)
            })
        });
        c.bench_function(format!("section/set_4096/{name}"), |b| {
            b.iter_batched_ref(
                || section.clone(),
                |section| {
                    for (i, &(x, y, z)) in positions.iter().enumerate() {
                        section.set_block(x, y, z, 1 + (i as u16 % states));
                    }
                },
                BatchSize::SmallInput,
            )
        });
    }
    c.bench_function("chunk/set_block_4096", |b| {
        b.iter_batched_ref(
            terrain_chunk,
            |chunk| {
                for (i, &(x, y, z)) in positions.iter().enumerate() {
                    chunk.set_block(x, y as i32 * 4 - 64 + z as i32, z, i as u16 % 13);
                }
            },
            BatchSize::SmallInput,
        )
    });
}

fn encode(c: &mut Bench) {
    install();
    let chunk = terrain_chunk();
    let mut out = Vec::new();
    c.bench_function("packet/encode_chunk_with_light", |b| {
        b.iter(|| {
            out.clear();
            encode_chunk_with_light(&mut out, 3, -7, black_box(&chunk), &[]);
            black_box(out.len())
        })
    });
}

bench_group!(benches, access, encode);
bench_main!(benches);
