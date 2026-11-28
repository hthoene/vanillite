//! Blocks that spread or decay: grass and mycelium (`SpreadingSnowyBlock`), nylium, leaves,
//! mushrooms and corals.

use vanillite_abi::event::{BLOCK_FADE, BLOCK_SPREAD, LEAVES_DECAY};
use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Direction};
use vanillite_data::block::{BlockId, BlockKind, BlockState, flags, ids};
use vanillite_worldgen::behaviour::{PosExt, StateExt, merged_face_occludes};

use super::hook_events as he;
use super::{block_tags, drops, fluid};
use crate::level::{Level, update};

/// `LightEngine.getLightDampeningInto(from, to, direction, to.getLightDampening())`.
fn light_dampening_into(from: BlockState, to: BlockState, direction: Direction) -> i32 {
    let simple = i32::from(to.light_dampening());
    let empty =
        |s: BlockState| !s.has(flags::CAN_OCCLUDE) || !s.has(flags::USE_SHAPE_FOR_LIGHT_OCCLUSION);
    let (from_empty, to_empty) = (empty(from), empty(to));
    if from_empty && to_empty {
        return simple;
    }
    let from_shape = if from_empty {
        &[][..]
    } else {
        from.occlusion_shape()
    };
    let to_shape = if to_empty {
        &[][..]
    } else {
        to.occlusion_shape()
    };
    if merged_face_occludes(from_shape, to_shape, direction) {
        16
    } else {
        simple
    }
}

/// `SpreadingSnowyBlock.canStayAlive`.
fn can_stay_alive(level: &Level, state: BlockState, pos: BlockPos) -> bool {
    let above = level.block(pos.above());
    if above.is(ids::SNOW) && above.int("layers") == Some(1) {
        return true;
    }
    if above.fluid_state().info().amount == 8 {
        return false;
    }
    light_dampening_into(state, above, Direction::Up) < 15
}

/// `SpreadingSnowyBlock.canPropagate`.
fn can_propagate(level: &Level, state: BlockState, pos: BlockPos) -> bool {
    can_stay_alive(level, state, pos) && !fluid::is_water(level.fluid(pos.above()))
}

/// `SpreadingSnowyBlock.randomTick` of grass blocks and mycelium: dies to dirt when covered,
/// spreads to dirt nearby in the light.
pub(super) fn spreading_snowy_dirt_random_tick(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
) {
    let base = ids::DIRT;
    if !can_stay_alive(level, state, pos) {
        he::set_block_and_update(level, BLOCK_FADE, pos, base.default_state());
        return;
    }
    if level.max_local_raw_brightness(pos.above()) < 9 {
        return;
    }
    let spread = state.block().default_state();
    for _ in 0..4 {
        let random = level.random();
        let dx = random.next_int_bounded(3) - 1;
        let dy = random.next_int_bounded(5) - 3;
        let dz = random.next_int_bounded(3) - 1;
        let target = pos.offset(dx, dy, dz);
        if level.block(target).is(base) && can_propagate(level, spread, target) {
            let snowy = block_tags().snow.contains(level.block(target.above()));
            let grown = spread.set_flag("snowy", snowy);
            he::set_block_and_update(level, BLOCK_SPREAD, target, grown);
        }
    }
}

/// `NyliumBlock.randomTick`: covered nylium becomes netherrack.
pub(super) fn nylium_random_tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    let above = level.block(pos.above());
    if light_dampening_into(state, above, Direction::Up) >= 15 {
        he::set_block_and_update(level, BLOCK_FADE, pos, ids::NETHERRACK.default_state());
    }
}

/// `LeavesBlock.getOptionalDistanceAt`.
fn leaves_distance_at(state: BlockState) -> i32 {
    if block_tags().prevents_nearby_leaf_decay.contains(state) {
        0
    } else if matches!(
        super::place::util::StateExt::kind(state),
        BlockKind::LeavesBlock
            | BlockKind::MangroveLeavesBlock
            | BlockKind::TintedParticleLeavesBlock
            | BlockKind::UntintedParticleLeavesBlock
    ) {
        // `hasProperty(DISTANCE)`: only leaves; scaffolding's `distance` is another property.
        state.int("distance").unwrap_or(7)
    } else {
        7
    }
}

/// `LeavesBlock.tick`: the distance to the nearest log is recomputed.
pub(super) fn leaves_tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    let mut distance = 7;
    for direction in Direction::ALL {
        distance = distance.min(leaves_distance_at(level.block(pos.relative(direction))) + 1);
        if distance == 1 {
            break;
        }
    }
    level.set_block_and_update(pos, state.set_int("distance", distance));
}

/// `LeavesBlock.randomTick`: leaves too far from logs decay.
pub(super) fn leaves_random_tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    if !state.flag("persistent") && state.int("distance") == Some(7) {
        if let Some(hooks) = level.hooks_for(LEAVES_DECAY) {
            let mut ev = crate::hooks::Event {
                pos,
                key: he::state_text(state),
                state: Some(state),
                ..crate::hooks::Event::new(LEAVES_DECAY)
            };
            hooks.event(crate::hooks::HookWorld::of(level), &mut ev);
            if ev.cancelled {
                return;
            }
        }
        drops::drop_resources(level, pos, state, None);
        level.remove_block(pos, false);
    }
}

/// `MushroomBlock.randomTick`: spreads while fewer than five mushrooms are around.
pub(super) fn mushroom_random_tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    if level.random().next_int_bounded(25) != 0 {
        return;
    }
    // `canSpread`: at most four of the same mushroom in the 9 × 3 × 9 area.
    let mut count = 0;
    for y in -1..=1 {
        for z in -4..=4 {
            for x in -4..=4 {
                if level.block(pos.offset(x, y, z)).is(state.block()) {
                    count += 1;
                    if count > 4 {
                        return;
                    }
                }
            }
        }
    }
    let mut pos = pos;
    let mut offset = random_offset(level, pos);
    for _ in 0..4 {
        if level.block(offset).is_air() && level.can_survive(state, offset) {
            pos = offset;
        }
        offset = random_offset(level, pos);
    }
    if level.block(offset).is_air() && level.can_survive(state, offset) {
        he::set_block(level, BLOCK_SPREAD, offset, state, update::CLIENTS);
    }
}

fn random_offset(level: &mut Level, pos: BlockPos) -> BlockPos {
    let random = level.random();
    let dx = random.next_int_bounded(3) - 1;
    let dy = random.next_int_bounded(2) - random.next_int_bounded(2);
    let dz = random.next_int_bounded(3) - 1;
    pos.offset(dx, dy, dz)
}

/// The dead variant of a living coral block, plant or fan.
fn dead_coral(block: BlockId) -> Option<BlockId> {
    let name = block.name().strip_prefix("minecraft:")?;
    if name.starts_with("dead_") {
        return None;
    }
    BlockId::by_name(&format!("minecraft:dead_{name}"))
}

/// `scanForWater`: corals live next to water (coral plants and fans also when waterlogged).
fn scan_for_water(level: &Level, state: BlockState, pos: BlockPos) -> bool {
    fluid::waterlogged(state)
        || Direction::ALL
            .into_iter()
            .any(|d| fluid::is_water(level.fluid(pos.relative(d))))
}

/// `BaseCoralPlantTypeBlock.tryScheduleDieTick` on placement of coral plants and fans.
pub(super) fn coral_on_place(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    _old: BlockState,
    _moved: bool,
) {
    if dead_coral(state.block()).is_some() && !scan_for_water(level, state, pos) {
        let delay = 60 + level.random().next_int_bounded(40);
        level.schedule_block_tick(pos, state.block(), delay);
    }
}

/// The die tick of corals: without water they become their dead variant.
pub(super) fn coral_tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    let Some(dead) = dead_coral(state.block()) else {
        return;
    };
    if scan_for_water(level, state, pos) {
        return;
    }
    let mut dead_state = dead.default_state();
    if dead_state.value("waterlogged").is_some() {
        dead_state = dead_state.set_flag("waterlogged", false);
    }
    if let Some(facing) = state.direction("facing") {
        dead_state = dead_state.set_direction("facing", facing);
    }
    he::set_block(level, BLOCK_FADE, pos, dead_state, update::CLIENTS);
}
