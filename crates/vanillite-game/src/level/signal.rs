//! Redstone signals (`SignalGetter` and the signal methods of `BlockBehaviour`): what a block
//! emits towards a neighbour, strongly (direct) or weakly, and what a position receives.
//!
//! Blocks emit through their behaviour entries `signal`, `direct_signal` and `analog_output`;
//! blocks without them emit nothing. `isSignalSource` and `isRedstoneConductor` are state facts
//! from the data, except that redstone wire stops being a signal source while it computes its
//! own power ([`Level::wire_signals`], `RedstoneWireBlock.shouldSignal`).

use vanillite_base::{BlockPos, Direction};
use vanillite_data::block::{BlockState, flags, ids};
use vanillite_worldgen::behaviour::StateExt;

use super::Level;

/// `Direction.values()`: the order of `getBestNeighborSignal`.
pub const DIRECTIONS: [Direction; 6] = [
    Direction::Down,
    Direction::Up,
    Direction::North,
    Direction::South,
    Direction::West,
    Direction::East,
];

impl Level {
    /// `state.isSignalSource()`.
    #[inline]
    pub fn is_signal_source(&self, state: BlockState) -> bool {
        state.has(flags::SIGNAL_SOURCE) && (self.wire_signals || !state.is(ids::REDSTONE_WIRE))
    }

    /// `state.isRedstoneConductor(level, pos)`.
    #[inline]
    pub fn is_redstone_conductor(state: BlockState) -> bool {
        state.has(flags::REDSTONE_CONDUCTOR)
    }

    /// `state.getSignal(level, pos, direction)`: the weak signal `state` at `pos` emits towards
    /// the neighbour it sees in `-direction`.
    #[inline]
    pub fn state_signal(&self, state: BlockState, pos: BlockPos, direction: Direction) -> i32 {
        match self.behaviours.get(state.block()).signal {
            Some(f) => f(self, state, pos, direction),
            None => 0,
        }
    }

    /// `state.getDirectSignal(level, pos, direction)`: the strong signal.
    #[inline]
    pub fn state_direct_signal(
        &self,
        state: BlockState,
        pos: BlockPos,
        direction: Direction,
    ) -> i32 {
        match self.behaviours.get(state.block()).direct_signal {
            Some(f) => f(self, state, pos, direction),
            None => 0,
        }
    }

    /// `state.hasAnalogOutputSignal()`.
    #[inline]
    pub fn has_analog_output(state: BlockState) -> bool {
        state.has(flags::ANALOG_OUTPUT)
    }

    /// `state.getAnalogOutputSignal(level, pos, direction)`.
    pub fn analog_output(&self, state: BlockState, pos: BlockPos, direction: Direction) -> i32 {
        match self.behaviours.get(state.block()).analog_output {
            Some(f) => f(self, state, pos, direction),
            None => 0,
        }
    }

    /// `getDirectSignal(pos, direction)`.
    #[inline]
    pub fn direct_signal(&self, pos: BlockPos, direction: Direction) -> i32 {
        self.state_direct_signal(self.block(pos), pos, direction)
    }

    /// Whether `state` surely emits no signal, so the behaviour table is not needed: air, and
    /// wire while wires emit nothing. Most neighbours a wire reads are one of the two.
    #[inline]
    fn emits_nothing(&self, state: BlockState) -> bool {
        state == BlockState::AIR || (!self.wire_signals && state.is(ids::REDSTONE_WIRE))
    }

    /// `getDirectSignalTo(pos)`: the strongest strong signal into `pos`.
    pub fn direct_signal_to(&self, pos: BlockPos) -> i32 {
        let mut result = 0;
        for direction in DIRECTIONS {
            let neighbour = pos.relative(direction);
            let state = self.block(neighbour);
            if self.emits_nothing(state) {
                continue;
            }
            result = result.max(self.state_direct_signal(state, neighbour, direction));
            if result >= 15 {
                return result;
            }
        }
        result
    }

    /// `getControlInputSignal(pos, direction, onlyDiodes)`: a side input of a diode.
    pub fn control_input_signal(
        &self,
        pos: BlockPos,
        direction: Direction,
        only_diodes: bool,
    ) -> i32 {
        let state = self.block(pos);
        if only_diodes {
            if is_diode(state) {
                self.direct_signal(pos, direction)
            } else {
                0
            }
        } else if state.is(ids::REDSTONE_BLOCK) {
            15
        } else if state.is(ids::REDSTONE_WIRE) {
            wire_power(state)
        } else if self.is_signal_source(state) {
            self.direct_signal(pos, direction)
        } else {
            0
        }
    }

    /// `getSignal(pos, direction)`: the weak signal of the block at `pos` towards `-direction`,
    /// or for a conductor the strongest strong signal it receives.
    pub fn signal(&self, pos: BlockPos, direction: Direction) -> i32 {
        let state = self.block(pos);
        // Neither air nor wire is a conductor.
        if self.emits_nothing(state) {
            return 0;
        }
        let signal = self.state_signal(state, pos, direction);
        if Self::is_redstone_conductor(state) {
            signal.max(self.direct_signal_to(pos))
        } else {
            signal
        }
    }

    /// `hasSignal(pos, direction)`.
    #[inline]
    pub fn has_signal(&self, pos: BlockPos, direction: Direction) -> bool {
        self.signal(pos, direction) > 0
    }

    /// `hasNeighborSignal(pos)`: any neighbour powers `pos` (down, up, north, south, west,
    /// east).
    pub fn has_neighbor_signal(&self, pos: BlockPos) -> bool {
        DIRECTIONS
            .into_iter()
            .any(|direction| self.signal(pos.relative(direction), direction) > 0)
    }

    /// `getBestNeighborSignal(pos)`.
    pub fn best_neighbor_signal(&self, pos: BlockPos) -> i32 {
        let mut best = 0;
        for direction in DIRECTIONS {
            let signal = self.signal(pos.relative(direction), direction);
            if signal >= 15 {
                return 15;
            }
            best = best.max(signal);
        }
        best
    }

    /// `RedstoneWireBlock.getBlockSignal(level, pos)`: the best neighbour signal while wires
    /// emit nothing.
    pub fn wire_block_signal(&mut self, pos: BlockPos) -> i32 {
        self.wire_signals = false;
        let signal = self.best_neighbor_signal(pos);
        self.wire_signals = true;
        signal
    }

    /// `updateNeighbourForOutputSignal(pos, block)`: comparators next to `pos`, or behind a
    /// conductor next to it, re-read their input.
    pub fn update_neighbour_for_output_signal(
        &mut self,
        pos: BlockPos,
        source: vanillite_data::block::BlockId,
    ) {
        for direction in vanillite_worldgen::behaviour::HORIZONTAL {
            let mut neighbour = pos.relative(direction);
            if !self.is_loaded(neighbour) {
                continue;
            }
            let mut state = self.block(neighbour);
            if state.is(ids::COMPARATOR) {
                self.neighbor_changed_state(state, neighbour, source);
            } else if Self::is_redstone_conductor(state) {
                neighbour = neighbour.relative(direction);
                state = self.block(neighbour);
                if state.is(ids::COMPARATOR) {
                    self.neighbor_changed_state(state, neighbour, source);
                }
            }
        }
    }

    /// `neighborChanged(state, pos, block, orientation, movedByPiston)` of `Level`: a neighbour
    /// update of `state` as given, not as read when the update runs (`FullNeighborUpdate`).
    pub fn neighbor_changed_state(
        &mut self,
        state: BlockState,
        pos: BlockPos,
        source: vanillite_data::block::BlockId,
    ) {
        self.add_and_run(super::neighbors::Update::Full {
            state,
            pos,
            source,
            moved: false,
        });
    }

    /// [`Self::neighbor_changed_state`] with the `movedByPiston` flag.
    pub fn neighbor_changed_moved(
        &mut self,
        state: BlockState,
        pos: BlockPos,
        source: vanillite_data::block::BlockId,
        moved: bool,
    ) {
        self.add_and_run(super::neighbors::Update::Full {
            state,
            pos,
            source,
            moved,
        });
    }

    /// `ComparatorBlockEntity.getOutputSignal()` of the comparator at `pos`.
    pub fn comparator_output(&self, pos: BlockPos) -> i32 {
        match self.block_entity(pos) {
            Some(super::block_entity::BlockEntity::Comparator { output }) => *output,
            _ => 0,
        }
    }
}

/// `DiodeBlock.isDiode(state)`: repeaters and comparators.
#[inline]
pub fn is_diode(state: BlockState) -> bool {
    state.is(ids::REPEATER) || state.is(ids::COMPARATOR)
}

/// `state.getValue(RedstoneWireBlock.POWER)`.
#[inline]
pub fn wire_power(state: BlockState) -> i32 {
    if state.is(ids::REDSTONE_WIRE) {
        return i32::from(
            vanillite_worldgen::behaviour::wire_properties()
                .power
                .index(state),
        );
    }
    i32::from(state.value_index("power").unwrap_or(0))
}

/// `state.getValue(POWER)` of any block with a `power` property (wire, daylight detectors,
/// weighted plates, sculk sensors, target blocks).
#[inline]
pub fn power(state: BlockState) -> i32 {
    i32::from(state.value_index("power").unwrap_or(0))
}

/// `state.setValue(POWER, power)`.
#[inline]
pub fn with_power(state: BlockState, power: i32) -> BlockState {
    if state.is(ids::REDSTONE_WIRE) {
        let property = vanillite_worldgen::behaviour::wire_properties().power;
        return property.with_index(state, power.clamp(0, 15) as u16);
    }
    state
        .with_index("power", power.clamp(0, 15) as u16)
        .unwrap_or(state)
}

/// `state.getValue(POWERED)`.
#[inline]
pub fn powered(state: BlockState) -> bool {
    state.flag("powered")
}
