//! Chunk ticket levels: a player ticket (view distance 10, level 22) changing chunk, as a
//! remove + add and as a move, alone and among other players.

use std::hint::black_box;

use microbench::{Bench, bench_group, bench_main};
use vanillite_base::ChunkPos;
use vanillite_world::chunks::{BLOCK_TICKING_LEVEL, LevelChange, MAX_TICKET_LEVEL, TicketLevels};

/// Player ticket level at view distance 10.
const PLAYER: u8 = 22;
/// Player simulation ticket level at simulation distance 10.
const SIMULATION: u8 = 21;

/// A walk along a 16 x 16 square, one chunk per step, diagonal on the corners' way back.
fn walk() -> Vec<ChunkPos> {
    let mut path = Vec::new();
    for i in 0..16 {
        path.push(ChunkPos::new(i, 0));
    }
    for i in 0..16 {
        path.push(ChunkPos::new(16, i));
    }
    for i in 0..16 {
        path.push(ChunkPos::new(16 - i, 16 - i));
    }
    path
}

fn levels(max: u8, others: usize, level: u8) -> TicketLevels {
    let mut levels = TicketLevels::with_max(max);
    let mut changes = Vec::new();
    for i in 0..others as i32 {
        levels.add(
            ChunkPos::new((i * 7) % 40 - 12, (i * 13) % 40 - 12),
            level,
            &mut changes,
        );
    }
    levels
}

fn bench(c: &mut Bench, name: &str, max: u8, others: usize, level: u8) {
    let path = walk();
    let mut changes: Vec<LevelChange> = Vec::new();
    let mut group = c.benchmark_group(name);
    let mut state = levels(max, others, level);
    state.add(path[0], level, &mut changes);
    let mut step = 0;
    group.bench_function("remove_add", |b| {
        b.iter(|| {
            let from = path[step % path.len()];
            step += 1;
            let to = path[step % path.len()];
            changes.clear();
            state.add(black_box(to), level, &mut changes);
            state.remove(black_box(from), level, &mut changes);
            black_box(changes.len());
        })
    });
    let mut state = levels(max, others, level);
    state.add(path[0], level, &mut changes);
    let mut step = 0;
    group.bench_function("move", |b| {
        b.iter(|| {
            let from = path[step % path.len()];
            step += 1;
            let to = path[step % path.len()];
            changes.clear();
            state.shift(black_box(from), black_box(to), level, &mut changes);
            black_box(changes.len());
        })
    });
    group.finish();
}

fn tickets(c: &mut Bench) {
    bench(c, "player_ticket_alone", MAX_TICKET_LEVEL, 0, PLAYER);
    bench(c, "player_ticket_among_50", MAX_TICKET_LEVEL, 50, PLAYER);
    bench(
        c,
        "simulation_ticket_alone",
        BLOCK_TICKING_LEVEL,
        0,
        SIMULATION,
    );
}

bench_group!(benches, tickets);
bench_main!(benches);
