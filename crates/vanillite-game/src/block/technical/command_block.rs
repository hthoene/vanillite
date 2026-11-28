//! `CommandBlock`, `CommandBlockEntity` and `BaseCommandBlock`: impulse, chain and repeating
//! command blocks, and the command part of command block minecarts.
//!
//! The commands run on the server's dispatcher through the level's [`CommandRunner`]
//! (`level.getServer().getCommands()`), synchronously inside the block tick like Vanilla.

use std::sync::Arc;

use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockState, ids};
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::packets::play::serverbound::{SetCommandBlock, SetCommandMinecart};
use vanillite_protocol::text::Component;
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::StateExt;

use super::Editor;
use crate::block::BlockBehaviour;
use crate::level::{BlockEntity, Level};

/// `BaseCommandBlock`: the command, its last result and output.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseCommandBlock {
    /// `command`.
    pub command: String,
    /// `successCount`.
    pub success_count: i32,
    /// `customName`.
    pub custom_name: Option<Component>,
    /// `trackOutput`.
    pub track_output: bool,
    /// `lastOutput`.
    pub last_output: Option<Component>,
    /// `updateLastExecution`.
    pub update_last_execution: bool,
    /// `lastExecution` (-1: none).
    pub last_execution: i64,
}

impl Default for BaseCommandBlock {
    fn default() -> Self {
        Self {
            command: String::new(),
            success_count: 0,
            custom_name: None,
            track_output: true,
            last_output: None,
            update_last_execution: true,
            last_execution: -1,
        }
    }
}

fn flag(nbt: &Compound, key: &str, default: bool) -> bool {
    match nbt.get(key) {
        Some(Tag::Byte(v)) => *v != 0,
        _ => default,
    }
}

/// `BlockEntity.parseCustomNameSafe`.
fn component(nbt: &Compound, key: &str) -> Option<Component> {
    nbt.get(key).map(Component::from_nbt)
}

impl BaseCommandBlock {
    /// `load`.
    pub fn load(&mut self, nbt: &Compound) {
        self.command = nbt
            .get("Command")
            .and_then(Tag::as_str)
            .unwrap_or("")
            .to_owned();
        self.success_count = match nbt.get("SuccessCount") {
            Some(Tag::Int(v)) => *v,
            _ => 0,
        };
        self.custom_name = component(nbt, "CustomName");
        self.track_output = flag(nbt, "TrackOutput", true);
        self.last_output = if self.track_output {
            component(nbt, "LastOutput")
        } else {
            None
        };
        self.update_last_execution = flag(nbt, "UpdateLastExecution", true);
        self.last_execution = match nbt.get("LastExecution") {
            Some(Tag::Long(v)) if self.update_last_execution => *v,
            _ => -1,
        };
    }

    /// `save`.
    pub fn save(&self, nbt: &mut Compound) {
        nbt.insert("Command", Tag::String(self.command.clone()));
        nbt.insert("SuccessCount", Tag::Int(self.success_count));
        if let Some(name) = &self.custom_name {
            nbt.insert("CustomName", name.to_nbt());
        }
        nbt.insert("TrackOutput", Tag::Byte(i8::from(self.track_output)));
        if self.track_output
            && let Some(output) = &self.last_output
        {
            nbt.insert("LastOutput", output.to_nbt());
        }
        nbt.insert(
            "UpdateLastExecution",
            Tag::Byte(i8::from(self.update_last_execution)),
        );
        if self.update_last_execution && self.last_execution != -1 {
            nbt.insert("LastExecution", Tag::Long(self.last_execution));
        }
    }

    /// `setCommand`: a new command resets the success count.
    pub fn set_command(&mut self, command: String) {
        self.command = command;
        self.success_count = 0;
    }

    /// `getName`: the custom name or `@`.
    pub fn name(&self) -> Component {
        self.custom_name
            .clone()
            .unwrap_or_else(|| Component::text("@"))
    }
}

/// `CommandBlockEntity`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandBlock {
    /// The command and its results.
    pub base: BaseCommandBlock,
    /// `powered`.
    pub powered: bool,
    /// `auto`.
    pub auto: bool,
    /// `conditionMet`.
    pub condition_met: bool,
}

impl CommandBlock {
    /// `CommandBlock.newBlockEntity`: chain command blocks start automatic.
    pub fn new(block: BlockId) -> Self {
        Self {
            auto: block == ids::CHAIN_COMMAND_BLOCK,
            ..Self::default()
        }
    }

    /// `loadAdditional`.
    pub fn load(&mut self, nbt: &Compound) {
        self.base.load(nbt);
        self.powered = flag(nbt, "powered", false);
        self.condition_met = flag(nbt, "conditionMet", false);
        // `setAutomatic` before the block entity is in a level schedules nothing.
        self.auto = flag(nbt, "auto", false);
    }

    /// `saveAdditional`.
    pub fn save(&self, nbt: &mut Compound) {
        self.base.save(nbt);
        nbt.insert("powered", Tag::Byte(i8::from(self.powered)));
        nbt.insert("conditionMet", Tag::Byte(i8::from(self.condition_met)));
        nbt.insert("auto", Tag::Byte(i8::from(self.auto)));
    }
}

/// `CommandBlockEntity.Mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Chain command blocks.
    Sequence,
    /// Repeating command blocks.
    Auto,
    /// Impulse command blocks.
    Redstone,
}

impl Mode {
    /// `getMode` of the block entity in a block of `state`.
    pub fn of(state: BlockState) -> Mode {
        if state.is(ids::REPEATING_COMMAND_BLOCK) {
            Mode::Auto
        } else if state.is(ids::CHAIN_COMMAND_BLOCK) {
            Mode::Sequence
        } else {
            Mode::Redstone
        }
    }

    /// The packet's mode id (`SEQUENCE`, `AUTO`, `REDSTONE`).
    fn from_id(id: u8) -> Mode {
        match id {
            0 => Mode::Sequence,
            1 => Mode::Auto,
            _ => Mode::Redstone,
        }
    }
}

/// What a command block or command block minecart runs (`createCommandSourceStack` of
/// `BaseCommandBlock.performCommand`): permission level 2 (`GAMEMASTER`).
#[derive(Clone, Debug)]
pub struct CommandRun<'a> {
    /// The command, with its optional `/`.
    pub command: &'a str,
    /// The position of the source.
    pub pos: Vec3,
    /// `y_rot`, `x_rot` of the source.
    pub rot: (f32, f32),
    /// The block's name (`getName`); a minecart's source is the minecart itself.
    pub name: Component,
    /// The command block minecart the command runs as.
    pub entity: Option<EntityId>,
    /// `trackOutput`: the source takes messages; else it is `CommandSource.NULL`.
    pub track_output: bool,
}

/// What a command run left (`withCallback` and `CloseableCommandBlockSource`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandOutcome {
    /// The successful results.
    pub successes: i32,
    /// The last message the source took, if any.
    pub last_message: Option<Component>,
}

/// The server's command dispatcher, for command blocks and command block minecarts
/// (`Commands.performPrefixedCommand`).
pub trait CommandRunner: Send + Sync {
    /// Runs `run` on `level` (a new execution) and reports its results.
    fn run(&self, level: &mut Level, run: &CommandRun<'_>) -> CommandOutcome;

    /// `ServerFunctionManager.execute` of the function `id` as the enchantment effect
    /// `run_function` runs it: the server at game master level with suppressed output, as
    /// `entity` at `pos` with the rotation `rot` (yaw, pitch). Returns `false` for an unknown
    /// function.
    fn run_function(
        &self,
        level: &mut Level,
        id: &str,
        entity: vanillite_world::entity::EntityId,
        pos: Vec3,
        rot: (f32, f32),
    ) -> bool {
        let _ = (level, id, entity, pos, rot);
        false
    }
}

/// The owner of a `BaseCommandBlock`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A command block entity.
    Block(BlockPos),
    /// A command block minecart.
    Minecart(EntityId),
}

fn base_mut(level: &mut Level, owner: Owner) -> Option<&mut BaseCommandBlock> {
    match owner {
        Owner::Block(pos) => match level.block_entity_mut(pos) {
            Some(BlockEntity::CommandBlock(c)) => Some(&mut c.base),
            _ => None,
        },
        Owner::Minecart(id) => level
            .entities
            .minecart_mut(id)
            .filter(|m| m.kind == crate::minecart::MinecartKind::CommandBlock)
            .map(|m| &mut m.command),
    }
}

/// `onUpdated`: a block sends its block update, a minecart its command and output data.
fn on_updated(level: &mut Level, owner: Owner) {
    match owner {
        Owner::Block(pos) => level.send_block_updated(pos),
        Owner::Minecart(id) => {
            if let Some(mut e) = level.entities.store.get_mut(id) {
                e.mark_data_dirty();
            }
        }
    }
}

/// `[HH:mm:ss] ` of now (UTC) before a message (`CloseableCommandBlockSource`).
fn timestamped(message: Component) -> Component {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let day = secs % 86_400;
    Component::text(format!(
        "[{:02}:{:02}:{:02}] ",
        day / 3600,
        day / 60 % 60,
        day % 60
    ))
    .append(message)
}

/// `Direction.toYRot`.
fn y_rot(direction: Direction) -> f32 {
    match direction {
        Direction::South => 0.0,
        Direction::West => 90.0,
        Direction::North => 180.0,
        _ => 270.0,
    }
}

/// The source of a command block: the block's centre, facing its direction.
fn block_source(level: &Level, pos: BlockPos) -> (Vec3, (f32, f32), Option<EntityId>) {
    let facing = level
        .block(pos)
        .direction("facing")
        .unwrap_or(Direction::North);
    let center = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    (center, (y_rot(facing), 0.0), None)
}

/// The source of a command block minecart: its position and rotation.
fn minecart_source(level: &Level, id: EntityId) -> (Vec3, (f32, f32), Option<EntityId>) {
    level
        .entities
        .store
        .get(id)
        .map_or((Vec3::new(0.0, 0.0, 0.0), (0.0, 0.0), Some(id)), |e| {
            let rot = e.rot();
            (e.pos(), (rot.y_rot, rot.x_rot), Some(id))
        })
}

/// `BaseCommandBlock.performCommand`: runs the command once per game tick; `false` if it
/// already ran this tick.
pub fn perform_command(level: &mut Level, owner: Owner) -> bool {
    let game_time = level.game_time();
    let Some(base) = base_mut(level, owner) else {
        return false;
    };
    if base.last_execution == game_time {
        return false;
    }
    if base.command.eq_ignore_ascii_case("Searge") {
        base.last_output = Some(Component::text("#itzlipofutzli"));
        base.success_count = 1;
        return true;
    }
    base.success_count = 0;
    let command = base.command.clone();
    let track_output = base.track_output;
    let name = base.name();
    if level.settings.command_blocks_work && !command.is_empty() {
        if let Some(base) = base_mut(level, owner) {
            base.last_output = None;
        }
        let (pos, rot, entity) = match owner {
            Owner::Block(pos) => block_source(level, pos),
            Owner::Minecart(id) => minecart_source(level, id),
        };
        let outcome = level.command_runner().map(|runner| {
            runner.run(
                level,
                &CommandRun {
                    command: &command,
                    pos,
                    rot,
                    name,
                    entity,
                    track_output,
                },
            )
        });
        if let Some(outcome) = outcome
            && let Some(base) = base_mut(level, owner)
        {
            base.success_count = base.success_count.wrapping_add(outcome.successes);
            if let Some(message) = outcome.last_message {
                base.last_output = Some(timestamped(message));
                on_updated(level, owner);
            }
        }
    }
    if let Some(base) = base_mut(level, owner) {
        base.last_execution = if base.update_last_execution {
            game_time
        } else {
            -1
        };
    }
    true
}

fn command_block(level: &Level, pos: BlockPos) -> Option<&CommandBlock> {
    match level.block_entity(pos) {
        Some(BlockEntity::CommandBlock(c)) => Some(c),
        _ => None,
    }
}

fn command_block_mut(level: &mut Level, pos: BlockPos) -> Option<&mut CommandBlock> {
    match level.block_entity_mut(pos) {
        Some(BlockEntity::CommandBlock(c)) => Some(c),
        _ => None,
    }
}

/// `CommandBlockEntity.isConditional`.
fn is_conditional(level: &Level, pos: BlockPos) -> bool {
    let state = level.block(pos);
    is_command_block(state) && state.value("conditional") == Some("true")
}

fn is_command_block(state: BlockState) -> bool {
    state.block().info().kind == vanillite_data::block::BlockKind::CommandBlock
}

/// `CommandBlockEntity.markConditionMet`: a conditional block's condition is the success of
/// the command block behind it.
fn mark_condition_met(level: &mut Level, pos: BlockPos) -> bool {
    let mut met = true;
    if is_conditional(level, pos) {
        let facing = level
            .block(pos)
            .direction("facing")
            .unwrap_or(Direction::North);
        let behind = pos.relative(facing.opposite());
        met = is_command_block(level.block(behind))
            && command_block(level, behind).is_some_and(|c| c.base.success_count > 0);
    }
    if let Some(c) = command_block_mut(level, pos) {
        c.condition_met = met;
    }
    met
}

/// `CommandBlockEntity.scheduleTick`.
fn schedule_tick(level: &mut Level, pos: BlockPos) {
    let state = level.block(pos);
    if is_command_block(state) {
        mark_condition_met(level, pos);
        level.schedule_block_tick(pos, state.block(), 1);
    }
}

/// `CommandBlockEntity.setAutomatic` in a level.
fn set_automatic(level: &mut Level, pos: BlockPos, auto: bool) {
    let mode = Mode::of(level.block(pos));
    let Some(c) = command_block_mut(level, pos) else {
        return;
    };
    let previous = c.auto;
    c.auto = auto;
    if !previous && auto && !c.powered && mode != Mode::Sequence {
        schedule_tick(level, pos);
    }
}

/// `loadWithComponents` of a command block entity in a level (`setblock`, `data`):
/// `setAutomatic` with the loaded flag schedules a tick when it turns automatic.
/// `previous_auto` is the flag of the block entity that was there.
pub(crate) fn loaded(level: &mut Level, pos: BlockPos, previous_auto: Option<bool>) {
    let state = level.block(pos);
    let Some(c) = command_block_mut(level, pos) else {
        return;
    };
    let auto = c.auto;
    c.auto = previous_auto.unwrap_or(state.is(ids::CHAIN_COMMAND_BLOCK));
    set_automatic(level, pos, auto);
}

/// `CommandBlockEntity.onModeSwitch`.
fn on_mode_switch(level: &mut Level, pos: BlockPos) {
    let mode = Mode::of(level.block(pos));
    if mode == Mode::Auto && command_block(level, pos).is_some_and(|c| c.powered || c.auto) {
        schedule_tick(level, pos);
    }
}

/// `CommandBlock.setPoweredAndUpdate`.
fn set_powered_and_update(level: &mut Level, pos: BlockPos, state: BlockState, powered: bool) {
    let mode = Mode::of(state);
    let Some(c) = command_block_mut(level, pos) else {
        return;
    };
    if powered == c.powered {
        return;
    }
    c.powered = powered;
    if powered {
        if c.auto || mode == Mode::Sequence {
            return;
        }
        mark_condition_met(level, pos);
        level.schedule_block_tick(pos, state.block(), 1);
    }
}

/// `CommandBlock.neighborChanged`.
fn neighbor_changed(level: &mut Level, state: BlockState, pos: BlockPos, _: BlockId, _: bool) {
    if command_block(level, pos).is_some() {
        let powered = level.has_neighbor_signal(pos);
        set_powered_and_update(level, pos, state, powered);
    }
}

/// `CommandBlock.tick`.
fn tick(level: &mut Level, state: BlockState, pos: BlockPos) {
    let Some(c) = command_block(level, pos) else {
        return;
    };
    let command_set = !c.base.command.is_empty();
    let was_condition_met = c.condition_met;
    let mode = Mode::of(state);
    match mode {
        Mode::Auto => {
            mark_condition_met(level, pos);
            if was_condition_met {
                execute(level, state, pos, command_set);
            } else if is_conditional(level, pos)
                && let Some(c) = command_block_mut(level, pos)
            {
                c.base.success_count = 0;
            }
            if command_block(level, pos).is_some_and(|c| c.powered || c.auto) {
                level.schedule_block_tick(pos, state.block(), 1);
            }
        }
        Mode::Redstone => {
            if was_condition_met {
                execute(level, state, pos, command_set);
            } else if is_conditional(level, pos)
                && let Some(c) = command_block_mut(level, pos)
            {
                c.base.success_count = 0;
            }
        }
        Mode::Sequence => {}
    }
    level.update_neighbour_for_output_signal(pos, state.block());
}

/// `CommandBlock.execute`.
fn execute(level: &mut Level, state: BlockState, pos: BlockPos, command_set: bool) {
    if command_set {
        perform_command(level, Owner::Block(pos));
    } else if let Some(c) = command_block_mut(level, pos) {
        c.base.success_count = 0;
    }
    let facing = state.direction("facing").unwrap_or(Direction::North);
    execute_chain(level, pos, facing);
}

/// `CommandBlock.executeChain`: the chain command blocks in front, up to
/// `max_command_sequence_length` of them.
fn execute_chain(level: &mut Level, start: BlockPos, mut direction: Direction) {
    let mut pos = start;
    let max = level.settings.max_command_sequence_length;
    let mut iterations = max;
    while iterations > 0 {
        iterations -= 1;
        pos = pos.relative(direction);
        let state = level.block(pos);
        if !state.is(ids::CHAIN_COMMAND_BLOCK) || Mode::of(state) != Mode::Sequence {
            break;
        }
        let Some(c) = command_block(level, pos) else {
            break;
        };
        if c.powered || c.auto {
            if mark_condition_met(level, pos) {
                if !perform_command(level, Owner::Block(pos)) {
                    break;
                }
                level.update_neighbour_for_output_signal(pos, state.block());
            } else if is_conditional(level, pos)
                && let Some(c) = command_block_mut(level, pos)
            {
                c.base.success_count = 0;
            }
        }
        direction = state.direction("facing").unwrap_or(Direction::North);
    }
    if iterations <= 0 {
        log::warn!(
            "Command Block chain tried to execute more than {} steps!",
            max.max(0)
        );
    }
}

/// `CommandBlock.getAnalogOutputSignal`: the success count.
fn analog_output(level: &Level, _: BlockState, pos: BlockPos, _: Direction) -> i32 {
    command_block(level, pos).map_or(0, |c| c.base.success_count)
}

/// The behaviour of the block.
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    if block == ids::COMMAND_BLOCK
        || block == ids::CHAIN_COMMAND_BLOCK
        || block == ids::REPEATING_COMMAND_BLOCK
    {
        b.neighbor_changed = Some(neighbor_changed);
        b.tick = Some(tick);
        b.analog_output = Some(analog_output);
    }
}

/// `CommandBlock.setPlacedBy`: without block entity data on the item the block tracks its
/// output by `send_command_feedback` and takes its block's automatic default; it then reads
/// its power.
pub fn set_placed_by(level: &mut Level, state: BlockState, pos: BlockPos, has_data: bool) {
    if !is_command_block(state) || command_block(level, pos).is_none() {
        return;
    }
    if !has_data {
        let track = level.settings.send_command_feedback;
        if let Some(c) = command_block_mut(level, pos) {
            c.base.track_output = track;
        }
        set_automatic(level, pos, state.is(ids::CHAIN_COMMAND_BLOCK));
    }
    let powered = level.has_neighbor_signal(pos);
    set_powered_and_update(level, pos, state, powered);
}

/// `ServerPlayer.openCommandBlock`: the block entity's data (`saveCustomOnly`) goes to the
/// player, whose client opens the editor with it.
pub(super) fn open(level: &mut Level, pos: BlockPos, editor: &Editor<'_>) {
    let Some(c) = command_block(level, pos) else {
        return;
    };
    let mut nbt = Compound::new();
    c.save(&mut nbt);
    let mut bytes = Vec::new();
    if vanillite_nbt::write::write_network_compound(&nbt, &mut bytes).is_err() {
        return;
    }
    let kind = level.block(pos).block().info().block_entity;
    editor.sink.send(
        &vanillite_protocol::packets::play::clientbound::BlockEntityData {
            pos: vanillite_protocol::types::BlockPos {
                x: pos.x,
                y: pos.y,
                z: pos.z,
            },
            kind: i32::from(kind),
            nbt: bytes,
        },
    );
}

/// The feedback of an edit: `advMode.setCommand.success` or `.disabled`.
fn set_command_feedback(level: &Level, editor: &Editor<'_>, command: &str) {
    if command.is_empty() {
        return;
    }
    let key = if level.settings.command_blocks_work {
        "advMode.setCommand.success"
    } else {
        "advMode.setCommand.disabled"
    };
    editor.message(Component::translatable(key, vec![Component::text(command)]));
}

/// `handleSetCommandBlock`: an operator in creative sets the command, mode and flags.
pub fn handle(level: &mut Level, packet: SetCommandBlock, editor: &Editor<'_>) {
    if !editor.allowed {
        editor.message(Component::translatable("advMode.notAllowed", vec![]));
        return;
    }
    let pos = BlockPos::new(packet.pos.x, packet.pos.y, packet.pos.z);
    if command_block(level, pos).is_none() {
        return;
    }
    let old_mode = Mode::of(level.block(pos));
    let current = level.block(pos);
    let facing = current.direction("facing").unwrap_or(Direction::North);
    let mode = Mode::from_id(packet.mode);
    let block = match mode {
        Mode::Sequence => ids::CHAIN_COMMAND_BLOCK,
        Mode::Auto => ids::REPEATING_COMMAND_BLOCK,
        Mode::Redstone => ids::COMMAND_BLOCK,
    };
    let state = block
        .default_state()
        .set_direction("facing", facing)
        .set_flag("conditional", packet.conditional);
    if state != current {
        // `setBlock(pos, state, UPDATE_CLIENTS)`, then the same block entity again.
        let entity = level.block_entity(pos).cloned();
        level.set_block(pos, state, crate::level::update::CLIENTS);
        if let Some(entity) = entity {
            level.set_block_entity(pos, entity);
        }
    }
    let command = packet.command;
    if let Some(c) = command_block_mut(level, pos) {
        c.base.set_command(command.clone());
        c.base.track_output = packet.track_output;
        if !packet.track_output {
            c.base.last_output = None;
        }
    }
    set_automatic(level, pos, packet.automatic);
    if old_mode != mode {
        on_mode_switch(level, pos);
    }
    if level.settings.command_blocks_work {
        on_updated(level, Owner::Block(pos));
    }
    set_command_feedback(level, editor, &command);
}

/// `handleSetCommandMinecart`.
pub fn handle_minecart(level: &mut Level, packet: SetCommandMinecart, editor: &Editor<'_>) {
    if !editor.allowed {
        editor.message(Component::translatable("advMode.notAllowed", vec![]));
        return;
    }
    let Some(id) = level.entities.by_network_id(packet.entity) else {
        return;
    };
    let owner = Owner::Minecart(id);
    let Some(base) = base_mut(level, owner) else {
        return;
    };
    base.set_command(packet.command.clone());
    base.track_output = packet.track_output;
    if !packet.track_output {
        base.last_output = None;
    }
    if level.settings.command_blocks_work {
        on_updated(level, owner);
    }
    set_command_feedback(level, editor, &packet.command);
}

/// The level's command dispatcher for command blocks.
impl Level {
    /// The dispatcher command blocks run their commands on (`None`: they run none).
    pub fn command_runner(&self) -> Option<Arc<dyn CommandRunner>> {
        self.command_runner.clone()
    }

    /// Installs the server's dispatcher for command blocks.
    pub fn set_command_runner(&mut self, runner: Arc<dyn CommandRunner>) {
        self.command_runner = Some(runner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nbt_round_trips_like_vanilla() {
        let mut c = CommandBlock::new(ids::CHAIN_COMMAND_BLOCK);
        assert!(c.auto);
        c.base.set_command("say hi".to_owned());
        c.base.success_count = 3;
        c.base.last_output = Some(Component::text("[00:00:00] hi"));
        c.base.last_execution = 42;
        c.powered = true;
        c.condition_met = true;
        let mut nbt = Compound::new();
        c.save(&mut nbt);
        let mut loaded = CommandBlock::default();
        loaded.load(&nbt);
        assert_eq!(loaded, c);
        let keys: Vec<&str> = nbt.iter().map(|(k, _)| k).collect();
        for key in [
            "Command",
            "SuccessCount",
            "TrackOutput",
            "LastOutput",
            "UpdateLastExecution",
            "LastExecution",
            "powered",
            "conditionMet",
            "auto",
        ] {
            assert!(keys.contains(&key), "{key}");
        }
        // Without output tracking neither the output nor (when not updated) the execution
        // time is kept.
        c.base.track_output = false;
        c.base.update_last_execution = false;
        let mut nbt = Compound::new();
        c.save(&mut nbt);
        assert!(nbt.get("LastOutput").is_none() && nbt.get("LastExecution").is_none());
        let mut loaded = CommandBlock::default();
        loaded.load(&nbt);
        assert_eq!(
            (loaded.base.last_output, loaded.base.last_execution),
            (None, -1)
        );
    }
}
