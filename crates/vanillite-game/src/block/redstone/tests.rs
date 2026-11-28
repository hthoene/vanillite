//! Redstone behaviour on the flat test world (grass at Y -61).

use vanillite_base::BlockPos;
use vanillite_data::block::{BlockState, ids};
use vanillite_worldgen::behaviour::StateExt;

use crate::level::tests::{flat_level, tick};
use crate::level::{Level, update};

fn place(level: &mut Level, pos: BlockPos, state: BlockState) {
    assert!(level.set_block(pos, state, update::ALL), "{pos:?}");
}

fn power(level: &Level, pos: BlockPos) -> i32 {
    level.block(pos).int("power").unwrap_or(-1)
}

#[test]
fn wire_line_fades_and_lights_a_lamp() {
    let mut level = flat_level();
    let y = -60;
    for x in 1..=16 {
        place(
            &mut level,
            BlockPos::new(x, y, 0),
            ids::REDSTONE_WIRE.default_state(),
        );
    }
    place(
        &mut level,
        BlockPos::new(17, y, 0),
        ids::REDSTONE_LAMP.default_state(),
    );
    place(
        &mut level,
        BlockPos::new(0, y, 0),
        ids::REDSTONE_BLOCK.default_state(),
    );
    for x in 1..=15 {
        assert_eq!(power(&level, BlockPos::new(x, y, 0)), 16 - x, "x={x}");
    }
    assert_eq!(power(&level, BlockPos::new(16, y, 0)), 0);
    // The lamp after the 15th wire is not reached; one closer it is.
    place(
        &mut level,
        BlockPos::new(16, y, 0),
        ids::REDSTONE_LAMP.default_state(),
    );
    assert!(level.block(BlockPos::new(16, y, 0)).flag("lit"));
    place(&mut level, BlockPos::new(0, y, 0), BlockState::AIR);
    for x in 1..=15 {
        assert_eq!(power(&level, BlockPos::new(x, y, 0)), 0, "x={x}");
    }
    // Lamps turn off 4 ticks later.
    for _ in 0..3 {
        tick(&mut level);
    }
    assert!(level.block(BlockPos::new(16, y, 0)).flag("lit"));
    tick(&mut level);
    assert!(!level.block(BlockPos::new(16, y, 0)).flag("lit"));
}

#[test]
fn repeater_delays_and_torch_inverts() {
    let mut level = flat_level();
    let y = -60;
    // Block at x=0, repeater (delay 2 = 4 ticks) facing west (input from the west), lamp.
    let repeater = ids::REPEATER
        .default_state()
        .set("facing", "west")
        .set_int("delay", 2);
    place(&mut level, BlockPos::new(1, y, 0), repeater);
    place(
        &mut level,
        BlockPos::new(2, y, 0),
        ids::REDSTONE_LAMP.default_state(),
    );
    place(
        &mut level,
        BlockPos::new(0, y, 0),
        ids::REDSTONE_BLOCK.default_state(),
    );
    for t in 1..=4 {
        tick(&mut level);
        let lit = level.block(BlockPos::new(2, y, 0)).flag("lit");
        assert_eq!(lit, t == 4, "tick {t}");
    }
    // A torch on a block that becomes a power source turns off after 2 ticks.
    place(
        &mut level,
        BlockPos::new(5, y, 5),
        ids::STONE.default_state(),
    );
    place(
        &mut level,
        BlockPos::new(5, y + 1, 5),
        ids::REDSTONE_TORCH.default_state(),
    );
    place(
        &mut level,
        BlockPos::new(5, y, 5),
        ids::REDSTONE_BLOCK.default_state(),
    );
    tick(&mut level);
    assert!(level.block(BlockPos::new(5, y + 1, 5)).flag("lit"));
    tick(&mut level);
    assert!(!level.block(BlockPos::new(5, y + 1, 5)).flag("lit"));
}

#[test]
fn comparator_subtracts_and_reads_a_composter() {
    let mut level = flat_level();
    let y = -60;
    let composter = ids::COMPOSTER.default_state().set_int("level", 5);
    place(&mut level, BlockPos::new(0, y, 0), composter);
    let comparator = ids::COMPARATOR.default_state().set("facing", "west");
    place(&mut level, BlockPos::new(1, y, 0), comparator);
    place(
        &mut level,
        BlockPos::new(2, y, 0),
        ids::REDSTONE_WIRE.default_state(),
    );
    // The comparator was placed without `setPlacedBy`: a neighbour update makes it look.
    level.neighbor_changed(BlockPos::new(1, y, 0), ids::STONE);
    tick(&mut level);
    tick(&mut level);
    assert_eq!(level.comparator_output(BlockPos::new(1, y, 0)), 5);
    assert_eq!(power(&level, BlockPos::new(2, y, 0)), 5);
    // Subtract mode with a side input of 3.
    let state = level.block(BlockPos::new(1, y, 0)).set("mode", "subtract");
    level.set_block(BlockPos::new(1, y, 0), state, update::ALL);
    place(
        &mut level,
        BlockPos::new(1, y, 3),
        ids::REDSTONE_BLOCK.default_state(),
    );
    for z in 1..=2 {
        place(
            &mut level,
            BlockPos::new(1, y, z),
            ids::REDSTONE_WIRE.default_state(),
        );
    }
    // Wire at z=1 carries 14 > 5: the comparator turns off.
    for _ in 0..4 {
        tick(&mut level);
    }
    assert_eq!(level.comparator_output(BlockPos::new(1, y, 0)), 0);
}

/// Measures a 48 × 48 wire plane powered from four redstone blocks below it, switched on and
/// off, with Vanilla's wire and with Alternate Current, and checks that both end with the same
/// powers. `cargo test -p vanillite-game --release -- --ignored alternate_current_bench
/// --nocapture`.
#[test]
#[ignore]
fn alternate_current_bench() {
    use crate::block::Behaviours;
    use crate::level::NEIGHBOR_STEPS;
    use crate::level::tests::flat_level_with;

    const SIZE: i32 = 48;
    const ROUNDS: usize = 10;
    let y = -60;
    let sources = [(12, 12), (12, 36), (36, 12), (36, 36)];
    let run = |alternate: bool| {
        let mut behaviours = Behaviours::vanilla();
        if alternate {
            super::alternate_current::install(&mut behaviours);
        }
        let mut level = flat_level_with(behaviours);
        for x in 0..SIZE {
            for z in 0..SIZE {
                place(
                    &mut level,
                    BlockPos::new(x, y, z),
                    ids::REDSTONE_WIRE.default_state(),
                );
            }
        }
        let ground = level.block(BlockPos::new(0, y - 1, 0));
        let mut snapshots = Vec::new();
        let steps = NEIGHBOR_STEPS.with(|s| s.get());
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            for on in [true, false] {
                for &(x, z) in &sources {
                    let state = if on {
                        ids::REDSTONE_BLOCK.default_state()
                    } else {
                        ground
                    };
                    place(&mut level, BlockPos::new(x, y - 1, z), state);
                }
                let mut powers = Vec::with_capacity((SIZE * SIZE) as usize);
                for x in 0..SIZE {
                    for z in 0..SIZE {
                        powers.push(power(&level, BlockPos::new(x, y, z)));
                    }
                }
                snapshots.push(powers);
            }
        }
        let elapsed = start.elapsed();
        let steps = NEIGHBOR_STEPS.with(|s| s.get()) - steps;
        (snapshots, elapsed, steps)
    };
    let (vanilla, vanilla_time, vanilla_steps) = run(false);
    let (alternate, alternate_time, alternate_steps) = run(true);
    let switches = (ROUNDS * 2) as u32;
    println!(
        "vanilla: {:?} and {} neighbour/shape update steps per switch",
        vanilla_time / switches,
        vanilla_steps / u64::from(switches)
    );
    println!(
        "alternate-current: {:?} and {} neighbour/shape update steps per switch",
        alternate_time / switches,
        alternate_steps / u64::from(switches)
    );
    assert!(vanilla.iter().flatten().any(|&p| p > 0));
    assert_eq!(vanilla, alternate);
}

/// Measures the dust part of the `redstone` server benchmark: 93 lines of 15 dust, each
/// switched on and off at one end. Prints the time and update steps per switch of all lines.
/// `cargo test -p vanillite-game --release -- --ignored wire_lines_bench --nocapture`.
#[test]
#[ignore]
fn wire_lines_bench() {
    use crate::level::NEIGHBOR_STEPS;

    const LINES: i32 = 93;
    const ROUNDS: u32 = 200;
    let y = -60;
    let mut level = flat_level();
    for line in 0..LINES {
        for x in 1..=15 {
            place(
                &mut level,
                BlockPos::new(x, y, line * 2),
                ids::REDSTONE_WIRE.default_state(),
            );
        }
    }
    let steps = NEIGHBOR_STEPS.with(|s| s.get());
    let start = std::time::Instant::now();
    for _ in 0..ROUNDS {
        for (source, first) in [
            (ids::REDSTONE_BLOCK.default_state(), 15),
            (BlockState::AIR, 0),
        ] {
            for line in 0..LINES {
                place(&mut level, BlockPos::new(0, y, line * 2), source);
            }
            assert_eq!(power(&level, BlockPos::new(1, y, 0)), first);
        }
    }
    let switches = ROUNDS * 2;
    println!(
        "{:?} and {} neighbour/shape update steps per switch",
        start.elapsed() / switches,
        (NEIGHBOR_STEPS.with(|s| s.get()) - steps) / u64::from(switches)
    );
}
