//! Sculk sensors and calibrated sculk sensors (`SculkSensorBlock`, `CalibratedSculkSensorBlock`,
//! their block entities) and the vibration system they listen with (`VibrationSystem`,
//! `VibrationSelector`, `VibrationInfo`).
//!
//! A sensor's block entity is a game event listener ([`crate::level::game_event`]). A game event
//! in range becomes a candidate (the nearest of a tick wins, ties go to the higher frequency);
//! one tick later the vibration starts travelling, one block per tick, and on arrival the sensor
//! activates with a power that falls with the distance. The vibration particle is not sent.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockState, ids};
use vanillite_data::entity::ids as entity_ids;
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::{PosExt, StateExt, tags};

use super::{BlockBehaviour, sound_id};
use crate::level::game_event::{
    self, GameEventContext, center, containing, dist_sqr, events, frequency,
};
use crate::level::signal::DIRECTIONS;
use crate::level::{BlockEntity, Level, SOUND_BLOCKS, update};

/// `SculkSensorBlock.getActiveTicks()`.
const ACTIVE_TICKS: i32 = 30;
/// `CalibratedSculkSensorBlock.getActiveTicks()`.
const CALIBRATED_ACTIVE_TICKS: i32 = 10;
/// `SculkSensorBlock.COOLDOWN_TICKS`.
const COOLDOWN_TICKS: i32 = 10;

pub use super::sculk_vibration::Vibration;
use super::sculk_vibration::{Listenable, VibrationData};

/// The data of a `SculkSensorBlockEntity` (`CalibratedSculkSensorBlockEntity` if `calibrated`)
/// with its `VibrationSystem.Data`.
#[derive(Clone, Debug, PartialEq)]
pub struct SculkSensor {
    /// A calibrated sensor (listener radius 16, filtered by the signal behind it).
    pub calibrated: bool,
    /// `lastVibrationFrequency`.
    pub last_vibration_frequency: i32,
    /// `VibrationSystem.Data`.
    pub vibration: VibrationData,
}

impl SculkSensor {
    /// A fresh block entity.
    pub fn new(calibrated: bool) -> SculkSensor {
        SculkSensor {
            calibrated,
            last_vibration_frequency: 0,
            vibration: VibrationData::default(),
        }
    }

    /// `VibrationUser.getListenerRadius()`.
    pub fn listener_radius(&self) -> i32 {
        if self.calibrated { 16 } else { 8 }
    }
}

/// Fills the sculk sensor behaviours.
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    if block == ids::SCULK_SENSOR || block == ids::CALIBRATED_SCULK_SENSOR {
        b.tick = Some(tick);
        b.on_place = Some(on_place);
        b.affect_neighbors_after_removal = Some(affect_neighbors_after_removal);
        b.signal = Some(signal);
        b.direct_signal = Some(direct_signal);
        b.analog_output = Some(analog_output);
    }
}

fn is_calibrated(state: BlockState) -> bool {
    state.is(ids::CALIBRATED_SCULK_SENSOR)
}

/// `SculkSensorBlock.canActivate(state)`: the phase is inactive.
fn can_activate(state: BlockState) -> bool {
    state.value("sculk_sensor_phase") == Some("inactive")
}

fn power(state: BlockState) -> i32 {
    i32::from(state.value_index("power").unwrap_or(0))
}

fn with_power(state: BlockState, power: i32) -> BlockState {
    state
        .with_index("power", power.clamp(0, 15) as u16)
        .unwrap_or(state)
}

/// `getSignal`: the power; a calibrated sensor gives none in its facing direction.
fn signal(_: &Level, state: BlockState, _: BlockPos, direction: Direction) -> i32 {
    if is_calibrated(state) && state.direction("facing") == Some(direction) {
        0
    } else {
        power(state)
    }
}

/// `getDirectSignal`: upwards only.
fn direct_signal(level: &Level, state: BlockState, pos: BlockPos, direction: Direction) -> i32 {
    if direction == Direction::Up {
        signal(level, state, pos, direction)
    } else {
        0
    }
}

/// `getAnalogOutputSignal`: the frequency of the last vibration while active.
fn analog_output(level: &Level, state: BlockState, pos: BlockPos, _: Direction) -> i32 {
    match level.block_entity(pos) {
        Some(BlockEntity::SculkSensor(sensor))
            if state.value("sculk_sensor_phase") == Some("active") =>
        {
            sensor.last_vibration_frequency
        }
        _ => 0,
    }
}

/// `SculkSensorBlock.onPlace`: a sensor placed with power and no pending tick loses it.
fn on_place(level: &mut Level, state: BlockState, pos: BlockPos, old: BlockState, _: bool) {
    if !old.is(state.block())
        && power(state) > 0
        && !level.chunks.has_block_tick(pos, state.block().0)
    {
        level.set_block(
            pos,
            with_power(state, 0),
            update::KNOWN_SHAPE | update::CLIENTS,
        );
    }
}

/// `affectNeighborsAfterRemoval`.
fn affect_neighbors_after_removal(level: &mut Level, state: BlockState, pos: BlockPos, _: bool) {
    if state.value("sculk_sensor_phase") == Some("active") {
        update_neighbours(level, pos, state.block());
    }
}

/// `SculkSensorBlock.updateNeighbours`.
fn update_neighbours(level: &mut Level, pos: BlockPos, block: BlockId) {
    level.update_neighbors_at(pos, block);
    level.update_neighbors_at(pos.below(), block);
}

/// `SculkSensorBlock.tick`: active → cooldown, cooldown → inactive.
fn tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    match state.value("sculk_sensor_phase") {
        Some("active") => deactivate(level, pos, state),
        Some("cooldown") => {
            level.set_block_and_update(pos, state.set("sculk_sensor_phase", "inactive"));
            if !state.waterlogged() {
                let pitch = level.random().next_float() * 0.2 + 0.8;
                let sound = sound_id("block.sculk_sensor.clicking_stop");
                level.play_block_sound(None, pos, sound, SOUND_BLOCKS, 1.0, pitch);
            }
        }
        _ => {}
    }
}

/// `SculkSensorBlock.deactivate`.
pub fn deactivate(level: &mut Level, pos: BlockPos, state: BlockState) {
    let cooldown = with_power(state.set("sculk_sensor_phase", "cooldown"), 0);
    level.set_block_and_update(pos, cooldown);
    level.schedule_block_tick(pos, state.block(), COOLDOWN_TICKS);
    update_neighbours(level, pos, state.block());
}

/// `SculkSensorBlock.activate`.
fn activate(
    level: &mut Level,
    source: GameEventContext,
    pos: BlockPos,
    state: BlockState,
    power: i32,
    frequency: i32,
) {
    let active = with_power(state.set("sculk_sensor_phase", "active"), power);
    level.set_block_and_update(pos, active);
    let ticks = if is_calibrated(state) {
        CALIBRATED_ACTIVE_TICKS
    } else {
        ACTIVE_TICKS
    };
    level.schedule_block_tick(pos, state.block(), ticks);
    update_neighbours(level, pos, state.block());
    try_resonate_vibration(level, source, pos, frequency);
    level.game_event_block(events().sculk_sensor_tendrils_clicking, pos, source);
    if !state.waterlogged() {
        let pitch = level.random().next_float() * 0.2 + 0.8;
        let sound = sound_id("block.sculk_sensor.clicking");
        level.play_block_sound(None, pos, sound, SOUND_BLOCKS, 1.0, pitch);
    }
}

/// `SculkSensorBlock.tryResonateVibration`: amethyst next to an active sensor resonates.
pub fn try_resonate_vibration(
    level: &mut Level,
    source: GameEventContext,
    pos: BlockPos,
    frequency: i32,
) {
    /// `RESONANCE_PITCH_BEND`'s tone map.
    const TONES: [i32; 16] = [0, 0, 2, 4, 6, 7, 9, 10, 12, 14, 15, 18, 19, 21, 22, 24];
    for direction in DIRECTIONS {
        let neighbour = pos.relative(direction);
        let state = level.block(neighbour);
        if tags().blocks.vibration_resonators.contains(state) {
            level.game_event(
                game_event::resonance_event(frequency),
                center(neighbour),
                GameEventContext {
                    affected: Some(state),
                    ..source
                },
            );
            let pitch = super::note::pitch_from_note(TONES[frequency.clamp(0, 15) as usize]);
            let sound = sound_id("block.amethyst_block.resonate");
            level.play_block_sound(None, neighbour, sound, SOUND_BLOCKS, 1.0, pitch);
        }
    }
}

/// `VibrationSystem.Listener.handleGameEvent` of the sensor at `pos`.
pub(crate) fn handle_game_event(
    level: &mut Level,
    pos: BlockPos,
    event: u16,
    context: GameEventContext,
    source: Vec3,
) {
    match level.block_entity(pos) {
        Some(BlockEntity::SculkSensor(sensor)) if sensor.vibration.current.is_none() => {}
        _ => return,
    }
    if !super::sculk_vibration::is_valid_vibration(level, event, context, Listenable::Vibrations) {
        return;
    }
    let destination = center(pos);
    if !can_receive_vibration(level, pos, containing(source), event)
        || super::sculk_vibration::is_occluded(level, source, destination)
    {
        return;
    }
    schedule_vibration(level, pos, event, context, source, destination);
}

/// `Listener.scheduleVibration`: a candidate for the selector.
fn schedule_vibration(
    level: &mut Level,
    pos: BlockPos,
    event: u16,
    context: GameEventContext,
    origin: Vec3,
    destination: Vec3,
) {
    let time = level.game_time();
    let source = context.source;
    let projectile_owner = super::sculk_vibration::event_projectile_owner(level, context);
    if let Some(BlockEntity::SculkSensor(sensor)) = level.block_entity_mut(pos) {
        let distance = origin.distance_to_sqr(destination).sqrt() as f32;
        sensor.vibration.add_candidate(
            Vibration {
                event,
                distance,
                pos: origin,
                source,
                projectile_owner,
            },
            time,
        );
    }
}

/// `VibrationUser.canReceiveVibration(level, pos, event, context)` of the sensor at `listener`;
/// `pos` is the vibration's block.
fn can_receive_vibration(level: &Level, listener: BlockPos, pos: BlockPos, event: u16) -> bool {
    let state = level.block(listener);
    if is_calibrated(state) {
        // `getBackSignal`: the signal into the back selects one frequency.
        let back = state
            .direction("facing")
            .unwrap_or(Direction::North)
            .opposite();
        let wanted = level.signal(listener.relative(back), back);
        if wanted != 0 && frequency(event) != wanted {
            return false;
        }
    }
    let e = events();
    if pos == listener && (event == e.block_destroy || event == e.block_place) {
        return false;
    }
    frequency(event) != 0 && can_activate(state)
}

/// The block entity ticker (`VibrationSystem.Ticker.tick`): select the candidate of an earlier
/// tick, let it travel, and receive it on arrival.
pub fn tick_block_entity(level: &mut Level, pos: BlockPos, _: BlockState) {
    let time = level.game_time();
    let Some(BlockEntity::SculkSensor(sensor)) = level.block_entity_mut(pos) else {
        return;
    };
    // `trySelectAndScheduleVibration`.
    sensor.vibration.try_select(time);
    let Some(vibration) = sensor.vibration.current else {
        return;
    };
    sensor.vibration.travel_time = (sensor.vibration.travel_time - 1).max(0);
    if sensor.vibration.travel_time > 0 {
        return;
    }
    // `receiveVibration`: only with the chunks around the listener ticking.
    let origin = containing(vibration.pos);
    if !super::sculk_vibration::adjacent_chunks_ticking(level, pos) {
        return;
    }
    let distance = dist_sqr(origin, pos).sqrt() as f32;
    let source = GameEventContext {
        source: vibration.source,
        affected: None,
        owner: vibration.projectile_owner,
    };
    on_receive_vibration(level, pos, vibration.event, source, distance);
    if let Some(BlockEntity::SculkSensor(sensor)) = level.block_entity_mut(pos) {
        sensor.vibration.current = None;
    }
}

/// `VibrationUser.onReceiveVibration`.
fn on_receive_vibration(
    level: &mut Level,
    pos: BlockPos,
    event: u16,
    source: GameEventContext,
    distance: f32,
) {
    let state = level.block(pos);
    if !can_activate(state) {
        return;
    }
    let frequency = frequency(event);
    let Some(BlockEntity::SculkSensor(sensor)) = level.block_entity_mut(pos) else {
        return;
    };
    sensor.last_vibration_frequency = frequency;
    // `VibrationSystem.getRedstoneStrengthForDistance`.
    let scale = 15.0 / f64::from(sensor.listener_radius());
    let power = (15 - (scale * f64::from(distance)).floor() as i32).max(1);
    activate(level, source, pos, state, power, frequency);
}

/// Whether `stepOn` of `block` does something: the sculk sensors and the shrieker.
pub fn has_step_on(block: BlockId) -> bool {
    block == ids::SCULK_SENSOR
        || block == ids::CALIBRATED_SCULK_SENSOR
        || block == ids::SCULK_SHRIEKER
}

/// `Block.stepOn` of the block at `pos` for an entity standing on it (`applyEffectsFromBlocks`
/// of an entity on the ground, the block at `getOnPosLegacy`).
pub fn step_on_block(level: &mut Level, pos: BlockPos, entity: EntityId) {
    let block = level.block(pos).block();
    if block == ids::SCULK_SHRIEKER {
        super::sculk_shrieker::step_on(level, pos, entity);
    } else if block == ids::SCULK_SENSOR || block == ids::CALIBRATED_SCULK_SENSOR {
        step_on(level, pos, entity);
    }
}

/// `SculkSensorBlock.stepOn`: an entity (not a warden) stepping on an inactive sensor triggers
/// it directly. Called by entity movement.
pub fn step_on(level: &mut Level, pos: BlockPos, entity: EntityId) {
    let state = level.block(pos);
    if !can_activate(state) {
        return;
    }
    let Some(entity_ref) = level.entities.store.get(entity) else {
        return;
    };
    if entity_ref.kind() == entity_ids::WARDEN {
        return;
    }
    let position = entity_ref.pos();
    let step = events().step;
    if !matches!(level.block_entity(pos), Some(BlockEntity::SculkSensor(_)))
        || !can_receive_vibration(level, pos, pos, step)
    {
        return;
    }
    // `forceScheduleVibration`.
    schedule_vibration(
        level,
        pos,
        step,
        GameEventContext::of(Some(entity)),
        position,
        center(pos),
    );
}

#[cfg(test)]
mod tests {
    use vanillite_world::entity::{EntityFlags, NewEntity};

    use super::*;
    use crate::level::tests::flat_level;

    /// An entity standing on a sculk sensor schedules a `step` vibration through the block step
    /// code; a warden does not (`SculkSensorBlock.stepOn`, #119).
    #[test]
    fn stepping_on_a_sensor_schedules_a_vibration() {
        for (kind, heard) in [(entity_ids::EGG, true), (entity_ids::WARDEN, false)] {
            let mut level = flat_level();
            let pos = BlockPos::new(0, -60, 0);
            assert!(level.set_block(pos, ids::SCULK_SENSOR.default_state(), update::ALL));
            let network_id = level.network_ids().next();
            let new = NewEntity::new(kind, network_id, 1, Vec3::new(0.5, -59.5, 0.5));
            let id = level.entities.store.spawn(new, |_| 0);
            if let Some(mut e) = level.entities.store.get_mut(id) {
                e.set_flags(EntityFlags::ON_GROUND, true);
            }
            crate::block::plate::check_inside_blocks(&mut level, id, None);
            let Some(BlockEntity::SculkSensor(sensor)) = level.block_entity(pos) else {
                panic!("no sensor block entity");
            };
            let candidate = sensor.vibration.candidate.as_ref();
            assert_eq!(candidate.is_some(), heard, "{}", kind.name());
            if let Some((vibration, _)) = candidate {
                assert_eq!(vibration.event, events().step);
                assert_eq!(vibration.source, Some(id));
            }
        }
    }
}
