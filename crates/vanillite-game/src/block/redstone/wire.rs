//! Redstone wire (`RedstoneWireBlock` with `DefaultRedstoneWireEvaluator`).

use vanillite_base::{BlockPos, Direction};
use vanillite_data::block::{BlockState, ids};
use vanillite_worldgen::behaviour::{self as shared, HORIZONTAL, PosExt, StateExt};

use crate::block::{Interaction, UseContext, drops};
use crate::level::signal::{DIRECTIONS, wire_power, with_power};
use crate::level::{Level, update};

/// `Direction.Plane.VERTICAL`.
pub(super) const VERTICAL: [Direction; 2] = [Direction::Up, Direction::Down];

/// `getSignal`: the wire's power towards a side it points to, or up; nothing down. While a wire
/// computes its power, wires emit nothing.
pub(super) fn signal(level: &Level, state: BlockState, pos: BlockPos, direction: Direction) -> i32 {
    if !level.is_signal_source(state) || direction == Direction::Down {
        return 0;
    }
    let power = wire_power(state);
    if power == 0 {
        return 0;
    }
    if direction != Direction::Up {
        let connections = shared::wire_connection_state(level, state, pos);
        if !shared::wire_properties().connected(connections, direction.opposite()) {
            return 0;
        }
    }
    power
}

/// `getDirectSignal`: the same as the weak signal.
pub(super) fn direct_signal(
    level: &Level,
    state: BlockState,
    pos: BlockPos,
    direction: Direction,
) -> i32 {
    signal(level, state, pos, direction)
}

/// `onPlace`.
pub(super) fn on_place(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    old: BlockState,
    _: bool,
) {
    if old.block() == state.block() {
        return;
    }
    update_power_strength(level, pos, state);
    for direction in VERTICAL {
        level.update_neighbors_at(pos.relative(direction), ids::REDSTONE_WIRE);
    }
    update_neighbors_of_neighboring_wires(level, pos);
}

/// `affectNeighborsAfterRemoval`.
pub(super) fn affect_neighbors_after_removal(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    moved: bool,
) {
    if moved {
        return;
    }
    for direction in DIRECTIONS {
        level.update_neighbors_at(pos.relative(direction), ids::REDSTONE_WIRE);
    }
    update_power_strength(level, pos, state);
    update_neighbors_of_neighboring_wires(level, pos);
}

/// `neighborChanged`.
pub(super) fn neighbor_changed(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    _: vanillite_data::block::BlockId,
    _: bool,
) {
    if level.can_survive(state, pos) {
        update_power_strength(level, pos, state);
    } else {
        drops::drop_resources(level, pos, state, None);
        level.remove_block(pos, false);
    }
}

/// `useWithoutItem`: a cross becomes a dot and a dot a cross.
pub(super) fn use_without_item(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    context: &UseContext,
) -> Interaction {
    if !context.may_build {
        return Interaction::Pass;
    }
    let cross = is_cross(state);
    if cross || is_dot(state) {
        let side = if cross { "none" } else { "side" };
        let mut new = ids::REDSTONE_WIRE.default_state();
        for direction in HORIZONTAL {
            new = new.set(side_name(direction), side);
        }
        new = with_power(new, wire_power(state));
        new = shared::wire_connection_state(level, new, pos);
        if new != state {
            level.set_block_and_update(pos, new);
            // `updatesOnShapeChange`.
            for direction in HORIZONTAL {
                let side_pos = pos.relative(direction);
                if is_connected(state, direction) != is_connected(new, direction)
                    && Level::is_redstone_conductor(level.block(side_pos))
                {
                    level.update_neighbors_at_except(
                        side_pos,
                        ids::REDSTONE_WIRE,
                        Some(direction.opposite()),
                    );
                }
            }
            return Interaction::Success;
        }
    }
    Interaction::Pass
}

/// `DefaultRedstoneWireEvaluator.updatePowerStrength`: the wire takes its target power and, if
/// it changed, the wire and its six neighbours update their neighbours in the iteration order
/// of Java's `HashSet<BlockPos>` (the locational order of Vanilla redstone).
fn update_power_strength(level: &mut Level, pos: BlockPos, state: BlockState) {
    let target = calculate_target_strength(level, pos);
    if wire_power(state) == target {
        return;
    }
    if level.block(pos) == state {
        level.set_block(pos, with_power(state, target), update::CLIENTS);
    }
    let mut positions = [pos; 7];
    for (i, direction) in DIRECTIONS.into_iter().enumerate() {
        positions[i + 1] = pos.relative(direction);
    }
    for p in hash_set_order(positions) {
        level.update_neighbors_at(p, ids::REDSTONE_WIRE);
    }
}

/// `calculateTargetStrength`: the strongest signal from non-wire neighbours, or the strongest
/// neighbouring wire minus one.
fn calculate_target_strength(level: &mut Level, pos: BlockPos) -> i32 {
    let block_signal = level.wire_block_signal(pos);
    if block_signal == 15 {
        block_signal
    } else {
        block_signal.max(incoming_wire_signal(level, pos))
    }
}

/// `RedstoneWireEvaluator.getIncomingWireSignal`.
fn incoming_wire_signal(level: &Level, pos: BlockPos) -> i32 {
    let mut wire = 0;
    let above_conducts = Level::is_redstone_conductor(level.block(pos.above()));
    for direction in HORIZONTAL {
        let neighbour = pos.relative(direction);
        let state = level.block(neighbour);
        wire = wire.max(wire_signal(state));
        let conducts = Level::is_redstone_conductor(state);
        if conducts && !above_conducts {
            wire = wire.max(wire_signal(level.block(neighbour.above())));
        } else if !conducts {
            wire = wire.max(wire_signal(level.block(neighbour.below())));
        }
    }
    (wire - 1).max(0)
}

/// `getWireSignal`.
#[inline]
fn wire_signal(state: BlockState) -> i32 {
    if state.is(ids::REDSTONE_WIRE) {
        wire_power(state)
    } else {
        0
    }
}

/// `updateNeighborsOfNeighboringWires`.
pub(super) fn update_neighbors_of_neighboring_wires(level: &mut Level, pos: BlockPos) {
    for direction in HORIZONTAL {
        check_corner_change_at(level, pos.relative(direction));
    }
    for direction in HORIZONTAL {
        let target = pos.relative(direction);
        if Level::is_redstone_conductor(level.block(target)) {
            check_corner_change_at(level, target.above());
        } else {
            check_corner_change_at(level, target.below());
        }
    }
}

/// `checkCornerChangeAt`.
fn check_corner_change_at(level: &mut Level, pos: BlockPos) {
    if level.block(pos).is(ids::REDSTONE_WIRE) {
        level.update_neighbors_at(pos, ids::REDSTONE_WIRE);
        for direction in DIRECTIONS {
            level.update_neighbors_at(pos.relative(direction), ids::REDSTONE_WIRE);
        }
    }
}

/// The iteration order of a `java.util.HashSet<BlockPos>` holding `positions` (inserted in
/// order, no duplicates): buckets of a 16-bucket table by the spread hash, each bucket in
/// insertion order. Seven entries stay below the resize threshold of 12.
fn hash_set_order(positions: [BlockPos; 7]) -> [BlockPos; 7] {
    let bucket = |p: BlockPos| {
        // `Vec3i.hashCode` and `HashMap.hash`.
        let h =
            p.y.wrapping_add(p.z.wrapping_mul(31))
                .wrapping_mul(31)
                .wrapping_add(p.x);
        (h ^ ((h as u32) >> 16) as i32) & 15
    };
    // A counting sort over the 16 buckets: each position is hashed once, and the insertion
    // order within a bucket stays.
    let buckets = positions.map(|p| bucket(p) as usize);
    let mut next = [0usize; 16];
    for &b in &buckets {
        next[b] += 1;
    }
    let mut start = 0;
    for slot in &mut next {
        start += std::mem::replace(slot, start);
    }
    let mut order = positions;
    for (p, b) in positions.into_iter().zip(buckets) {
        order[next[b]] = p;
        next[b] += 1;
    }
    order
}

/// The wire side property of a horizontal direction.
pub(super) fn side_name(direction: Direction) -> &'static str {
    match direction {
        Direction::North => "north",
        Direction::South => "south",
        Direction::West => "west",
        Direction::East => "east",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

/// `state.getValue(PROPERTY_BY_DIRECTION.get(direction)).isConnected()`.
fn is_connected(state: BlockState, direction: Direction) -> bool {
    shared::wire_properties().connected(state, direction)
}

/// `isCross`.
fn is_cross(state: BlockState) -> bool {
    HORIZONTAL.into_iter().all(|d| is_connected(state, d))
}

/// `isDot`.
fn is_dot(state: BlockState) -> bool {
    HORIZONTAL.into_iter().all(|d| !is_connected(state, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counting sort gives the order of a stable sort by bucket.
    #[test]
    fn hash_set_order_is_a_stable_sort_by_bucket() {
        let bucket = |p: &BlockPos| {
            let h =
                p.y.wrapping_add(p.z.wrapping_mul(31))
                    .wrapping_mul(31)
                    .wrapping_add(p.x);
            (h ^ ((h as u32) >> 16) as i32) & 15
        };
        let coordinates = [
            -30_000_000,
            -65_537,
            -17,
            -1,
            0,
            1,
            15,
            16,
            4_095,
            29_999_999,
        ];
        for x in coordinates {
            for y in -64..64 {
                for z in coordinates {
                    let pos = BlockPos::new(x, y, z);
                    let mut positions = [pos; 7];
                    for (i, direction) in DIRECTIONS.into_iter().enumerate() {
                        positions[i + 1] = pos.relative(direction);
                    }
                    let mut expected = positions;
                    expected.sort_by_key(bucket);
                    assert_eq!(hash_set_order(positions), expected, "{pos:?}");
                }
            }
        }
    }
}
