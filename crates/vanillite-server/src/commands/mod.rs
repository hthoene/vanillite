//! Commands of players and the console: parsing through the command tree with the source's
//! permission (SEC-11), signed message arguments, execution with Vanilla's feedback, tab
//! completion and the command tree packet. Permissions are checked centrally here: the tree
//! only offers and parses nodes the source may use, and every command node carries its
//! required level (SEC-12).

mod advancement_cmd;
mod bans;
mod blocks_cmd;
mod border_cmd;
mod bossbar_cmd;
mod chat_cmd;
mod data;
mod data_cmd;
mod dispatch;
mod engine;
mod entities_cmd;
pub(crate) mod exec;
mod execute;
pub(crate) mod functions;
pub(crate) mod gametest;
pub(crate) mod items;
mod locate;
pub(crate) mod loot_cmd;
mod nbt_ops;
mod operator;
mod pack_cmd;
mod particle;
mod player_data;
pub(crate) mod plugins_cmd;
mod schedule;
mod scores;
mod select;
mod sound;
mod source;
mod survival;
mod system_cmd;
pub(crate) mod teams;
mod text;
mod tracker;
mod vanillite_cmd;
mod world_cmd;

use std::collections::HashMap;
use std::sync::Arc;

use vanillite_command::error::CommandError;
use vanillite_game::commands::CommandId;
use vanillite_game::entities::Entities;
use vanillite_game::player::chat::{ChainError, ChatMessage, MessageBody};
use vanillite_game::player::{PacketSink, SignedCommand};
use vanillite_protocol::packets::play::clientbound::{Commands, SystemChat};
use vanillite_protocol::text::{Arg, ClickEvent, Color, Component, NamedColor};
use vanillite_world::entity::EntityId;

pub(crate) use advancement_cmd::perform_on as perform_advancement;
pub(crate) use bans::ban_reason;
pub(crate) use blocks_cmd::fill_biome_box;
pub(crate) use data::{apply_entity_nbt, entity_nbt};
pub(crate) use data_cmd::merge_compound;
pub(crate) use entities_cmd::{dismount, kill_any, mount, mount_refusal};
pub(crate) use exec::{find_player, permission_event};
pub(crate) use execute::summon_for;
pub(crate) use operator::handle as handle_admin;
pub(crate) use particle::particle;
pub(crate) use player_data::add_experience;
pub(crate) use select::{find_uuid, local as local_target};
pub(crate) use sound::{send_post_effects, sound_source};
pub(crate) use source::{CommandSource, SourceKind};
pub(crate) use survival::{Weather, clock_changed, on_game_rule_changed, set_weather};
pub(crate) use vanillite_cmd::profile_finished;
pub(crate) use world_cmd::sprint_finished;

use crate::global::Global;
use crate::world::{ServerWorld, WorldId};

/// The command tree of the server: built once, shared by all worlds.
pub(crate) type CommandDispatcher = vanillite_command::Dispatcher<CommandId>;

/// Builds the dispatcher with the configured limits.
pub(crate) fn build_dispatcher(
    limits: vanillite_command::limits::Limits,
    bukkit_commands: bool,
) -> CommandDispatcher {
    let mut dispatcher = CommandDispatcher::new();
    dispatcher.set_limits(limits);
    vanillite_game::commands::register_server(&mut dispatcher, bukkit_commands);
    dispatcher
}

/// What a command execution may touch: the world it runs on and the server.
pub(crate) struct Env<'a> {
    pub(crate) level: &'a mut vanillite_game::level::Level,
    pub(crate) global: &'a Global,
    /// The world of `level`.
    pub(crate) world: WorldId,
    /// The levels of the other worlds, when the execution has them at hand.
    pub(crate) others: OtherLevels<'a>,
}

/// The levels of the other worlds by world index, with `None` at the own world's index: what
/// a world's tick sees of the others when the dimensions tick one after another (Vanilla's
/// order). Empty when they are not at hand (parallel dimensions, work between the ticks):
/// what leaves the world then runs there later.
pub(crate) type OtherLevels<'a> = Vec<Option<&'a mut vanillite_game::level::Level>>;

impl<'a> Env<'a> {
    /// Whether `world` is this execution's world or one at hand.
    pub(crate) fn has(&self, world: WorldId) -> bool {
        world == self.world || self.others.get(world.index()).is_some_and(Option::is_some)
    }

    /// The level of `world`, when it is this one or at hand.
    pub(crate) fn level_of(&mut self, world: WorldId) -> Option<&mut vanillite_game::level::Level> {
        if world == self.world {
            return Some(&mut *self.level);
        }
        self.others.get_mut(world.index())?.as_deref_mut()
    }

    /// The other levels at hand with their worlds.
    pub(crate) fn other_levels(
        &self,
    ) -> impl Iterator<Item = (WorldId, &vanillite_game::level::Level)> {
        self.others.iter().enumerate().filter_map(|(index, level)| {
            Some((WorldId(u16::try_from(index).ok()?), &**level.as_ref()?))
        })
    }

    /// Runs `f` as an execution on `world` with this world among its others (a source that
    /// left its world, `execute in`), or here when `world` is not at hand.
    pub(crate) fn at<R>(&mut self, world: WorldId, f: impl FnOnce(&mut Env<'a>) -> R) -> R {
        let home = self.world;
        let other = if world != home && home.index() < self.others.len() {
            self.others.get_mut(world.index()).and_then(Option::take)
        } else {
            None
        };
        let Some(other) = other else {
            return f(self);
        };
        let own = std::mem::replace(&mut self.level, other);
        if let Some(slot) = self.others.get_mut(home.index()) {
            *slot = Some(own);
        }
        self.world = world;
        let result = f(self);
        if let Some(own) = self.others.get_mut(home.index()).and_then(Option::take) {
            let other = std::mem::replace(&mut self.level, own);
            if let Some(slot) = self.others.get_mut(world.index()) {
                *slot = Some(other);
            }
            self.world = home;
        }
        result
    }
}

/// The other worlds' levels lent to a world's command blocks while a phase of its tick runs:
/// the blocks run inside the level, which cannot carry the borrows itself.
///
/// The slot holds the levels only inside [`LentLevels::with`], and a command reads them only
/// under the lock, which the lender takes again before it has the levels back.
#[derive(Default)]
pub(crate) struct LentLevels(std::sync::Mutex<Vec<Option<LevelPtr>>>);

struct LevelPtr(std::ptr::NonNull<vanillite_game::level::Level>);

// SAFETY: a `LevelPtr` stands for a `&mut Level` (see `LentLevels::with`), which is `Send`.
unsafe impl Send for LevelPtr {}

impl LentLevels {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Option<LevelPtr>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lends `levels` (as [`OtherLevels`]) to the command blocks that run inside `f`.
    pub(crate) fn with<'a, R>(
        &self,
        levels: impl Iterator<Item = Option<&'a mut vanillite_game::level::Level>>,
        f: impl FnOnce() -> R,
    ) -> R {
        struct TakeBack<'s>(&'s LentLevels);
        impl Drop for TakeBack<'_> {
            fn drop(&mut self) {
                // Waits for a command that still runs on the levels.
                self.0.lock().clear();
            }
        }
        let take_back = TakeBack(self);
        {
            let mut slot = self.lock();
            slot.clear();
            slot.extend(levels.map(|l| l.map(|l| LevelPtr(std::ptr::NonNull::from(l)))));
        }
        let result = f();
        drop(take_back);
        result
    }

    /// The slot for one command, or nothing while another command has it (a command block
    /// run from inside a command sees no other worlds).
    fn hold(&self) -> Option<std::sync::MutexGuard<'_, Vec<Option<LevelPtr>>>> {
        match self.0.try_lock() {
            Ok(slot) => Some(slot),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

/// Loads the functions and advancements of the selected data packs once the server exists
/// (`ServerFunctionLibrary.reload`, `ServerAdvancementManager`).
pub(crate) fn load_game_systems(global: &Global, packs: &[vanillite_datapack::OpenedPack]) {
    global.systems.set_functions(functions::load(global, packs));
    global
        .systems
        .load(&global.level.data, &|key| global.level.legacy(key));
    // Read when the first player logs in.
    global
        .systems
        .set_advancement_roots(packs.iter().flat_map(|p| p.layers.clone()).collect());
}

/// The advancements of the packs (`ServerAdvancementManager.apply`).
pub(crate) fn load_advancements(
    packs: &[vanillite_datapack::OpenedPack],
) -> vanillite_game::advancements::Advancements {
    let roots: Vec<std::path::PathBuf> = packs.iter().flat_map(|p| p.layers.clone()).collect();
    load_advancements_from(&roots)
}

/// The advancements of the pack directories `roots`, in load order.
pub(crate) fn load_advancements_from(
    roots: &[std::path::PathBuf],
) -> vanillite_game::advancements::Advancements {
    let files = functions::list(roots, "advancement", ".json")
        .into_iter()
        .filter_map(|(id, file)| match std::fs::read_to_string(&file) {
            Ok(text) => Some((id, text)),
            Err(e) => {
                log::error!("Couldn't read advancement {id}: {e}");
                None
            }
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    vanillite_game::advancements::Advancements::load(&files)
}

/// `ServerFunctionManager.tick` on the overworld.
pub(crate) fn tick_functions<'a>(
    level: &'a mut vanillite_game::level::Level,
    global: &'a Global,
    world: WorldId,
    others: OtherLevels<'a>,
) {
    let mut env = Env {
        level,
        global,
        world,
        others,
    };
    functions::tick(&mut env);
}

/// The scheduled functions due now (`TimerQueue.tick` in the overworld's `tickTime`).
pub(crate) fn tick_scheduled<'a>(
    level: &'a mut vanillite_game::level::Level,
    global: &'a Global,
    world: WorldId,
    others: OtherLevels<'a>,
) {
    let mut env = Env {
        level,
        global,
        world,
        others,
    };
    schedule::tick(&mut env);
}

/// Runs a parsed line for a source in a new execution (`Commands.executeCommandInContext`).
fn run_line(env: &mut Env<'_>, line: engine::Line, source: CommandSource) {
    let mut engine = functions::new_engine(env.global);
    engine.queue_command(line, source, engine::Callback::Empty);
    engine.run(env);
}

/// Installs the structure search of exploration maps with the current registry tags.
pub(crate) fn install_map_explorer(global: &Global) {
    vanillite_game::map::install_explorer(Box::new(locate::MapExplorer {
        tags: global.systems.tags(),
    }));
}

/// Where a written book to resolve is.
#[derive(Clone, Copy, Debug)]
pub(crate) enum BookAt {
    /// In a hand of the player; it opens afterwards (`openItemGui`).
    Hand(vanillite_protocol::packets::play::serverbound::Hand),
    /// On the lectern at the position (`LecternBlockEntity.resolveBook`).
    Lectern(vanillite_base::BlockPos),
}

/// `WrittenBookContent.resolveForItem` with the player's command source.
pub(crate) fn resolve_book(
    level: &mut vanillite_game::level::Level,
    global: &Global,
    id: EntityId,
    world: WorldId,
    at: BookAt,
) {
    use vanillite_protocol::packets::play::serverbound::Hand;
    let Some(source) = player_source(&level.entities, global, id, world) else {
        return;
    };
    let slot = |level: &vanillite_game::level::Level, hand: Hand| match hand {
        Hand::MainHand => level
            .entities
            .player(id)
            .map_or(0, |p| p.inventory.selected),
        Hand::OffHand => vanillite_game::inventory::OFFHAND,
    };
    let mut stack = match at {
        BookAt::Hand(hand) => {
            let i = slot(level, hand);
            match level.entities.player(id) {
                Some(p) => p.inventory.get(i).clone(),
                None => return,
            }
        }
        BookAt::Lectern(pos) => match level.block_entity(pos) {
            Some(vanillite_game::level::BlockEntity::Lectern(l)) => l.book.clone(),
            _ => return,
        },
    };
    if vanillite_game::book::needs_resolving(&stack) {
        let env = Env {
            level: &mut *level,
            global,
            world,
            others: Vec::new(),
        };
        vanillite_game::book::resolve_for_item(&mut stack, &mut |c| {
            text::resolve(&env, &source, c, None).ok()
        });
        match at {
            BookAt::Hand(hand) => {
                let i = slot(level, hand);
                if let Some(p) = level.entities.player_mut(id) {
                    p.inventory.set(i, stack);
                }
            }
            BookAt::Lectern(pos) => {
                if let Some(vanillite_game::level::BlockEntity::Lectern(l)) =
                    level.block_entity_mut(pos)
                {
                    l.book = stack;
                }
                level.block_entity_changed(pos);
            }
        }
    }
    if let BookAt::Hand(hand) = at
        && let Some(p) = level.entities.player_mut(id)
    {
        // `containerMenu.broadcastChanges()` first: the client opens the resolved book.
        let sink = Arc::clone(&p.connection.sink);
        if p.inventory.container.is_none() {
            p.inventory.broadcast_changes(sink.as_ref());
        }
        sink.send(&vanillite_protocol::packets::play::clientbound::OpenBook { hand });
    }
}

/// The permission level of a player: its operator level or 0.
pub(crate) fn permission_level(global: &Global, uuid: u128) -> u8 {
    global.access.op_level(uuid).unwrap_or(0)
}

/// The source of a player of this world.
pub(crate) fn player_source(
    entities: &Entities,
    global: &Global,
    id: EntityId,
    world: WorldId,
) -> Option<CommandSource> {
    let entity = entities.store.get(id)?;
    let player = entities.player(id)?;
    let uuid = entity.uuid();
    let name = player.profile.name.clone();
    let target = select::local(entities, world, id)?;
    Some(CommandSource::new(
        global.arc()?,
        SourceKind::Player {
            uuid,
            name,
            sink: Arc::clone(&player.connection.sink),
        },
        permission_level(global, uuid),
        world,
        entity.pos(),
        (entity.rot().y_rot, entity.rot().x_rot),
        Some(target.source_entity()),
    ))
}

/// The tree packet for a source (`Commands.sendCommands`).
pub(crate) fn tree_packet(global: &Global, source: &CommandSource) -> Commands {
    Commands(global.commands().to_tree(source))
}

/// Sends a joining player its permission level and command tree.
pub(crate) fn send_permissions(
    global: &Global,
    sink: &dyn PacketSink,
    entity_id: i32,
    source: &CommandSource,
) {
    sink.send(&permission_event(entity_id, source.level));
    sink.send(&tree_packet(global, source));
}

/// `PlayerList.sendPlayerPermissionLevel` after `op`/`deop`: on the player's world.
pub(crate) fn permissions_changed(global: &Global, uuid: u128) {
    let Some(world) = exec::world_of(global, uuid) else {
        return;
    };
    let Some(global_arc) = global.arc() else {
        return;
    };
    global.run_in_world(world, move |w: &mut ServerWorld| {
        let entities = w.entities();
        let Some(id) = exec::find_player(entities, uuid) else {
            return;
        };
        let (Some(source), Some(entity)) = (
            player_source(entities, &global_arc, id, w.id()),
            entities.store.get(id),
        ) else {
            return;
        };
        if let SourceKind::Player { sink, .. } = &source.kind {
            send_permissions(&global_arc, sink.as_ref(), entity.network_id().0, &source);
        }
    });
}

/// Vanilla's two error lines: the message and, for syntax errors, the context with a click
/// that suggests the command again (`Commands.performCommand`).
fn send_error(source: &CommandSource, global: &Global, error: &CommandError, command: &str) {
    source.send_failure(global, error_message(error));
    let Some(context) = error.context() else {
        return;
    };
    let mut line = Component::empty()
        .color(Color::Named(NamedColor::Gray))
        .click(ClickEvent::SuggestCommand(format!("/{command}")));
    if context.ellipsis {
        line = line.append(Component::text("..."));
    }
    line = line.append(Component::text(context.before));
    if !context.after.is_empty() {
        line = line.append(
            Component::text(context.after)
                .color(Color::Named(NamedColor::Red))
                .underlined(true),
        );
    }
    line = line.append(
        Component::translatable("command.context.here", vec![])
            .color(Color::Named(NamedColor::Red))
            .italic(true),
    );
    source.send_failure(global, line);
}

/// The error message with Vanilla's primitive translation arguments.
pub(crate) fn error_message(error: &CommandError) -> Component {
    use vanillite_command::error::Arg as E;
    let args = error
        .args
        .iter()
        .map(|a| match a {
            E::Str(s) => Arg::String(s.clone()),
            E::Int(v) => Arg::Int(*v),
            E::Long(v) => Arg::Long(*v),
            E::Float(v) => Arg::Float(*v),
            E::Double(v) => Arg::Double(*v),
            E::Component(c) => Arg::from((**c).clone()),
        })
        .collect();
    Component::translatable_args(error.key, args)
}

/// Signed arguments of a command, by name (`CommandSigningContext.SignedArguments`).
pub(crate) type SignedArguments = HashMap<String, ChatMessage>;

/// A player runs a command line (without the slash): `performUnsignedChatCommand` or
/// `performSignedChatCommand`, then the spam check. Returns a kick reason.
pub(crate) fn run_player_command(
    level: &mut vanillite_game::level::Level,
    global: &Global,
    id: EntityId,
    world: WorldId,
    command: &str,
    signed: Option<SignedCommand>,
) -> Option<Component> {
    let entities = &mut level.entities;
    let source = player_source(entities, global, id, world)?;
    let uuid = source.uuid()?;
    let name = source.text_name().to_owned();
    let commands = global.commands();
    let parse = commands.parse(command, &source);
    let signable = parse.signable_arguments(&commands);
    let signed_arguments = {
        let player = entities.player_mut(id)?;
        let sink = Arc::clone(&player.connection.sink);
        let chain = &mut player.connection.chat.chain;
        let enforce = global.chat.enforce_secure;
        let result = match &signed {
            None if enforce && !signable.is_empty() => {
                log::error!(
                    "Received unsigned command packet from {name}, but the command requires signable arguments: {command}"
                );
                Err(ChainError::InvalidCommandSignature)
            }
            None => Ok(SignedArguments::new()),
            Some(signed) => {
                collect_signed_arguments(chain, uuid, signed, &signable, enforce, command)
            }
        };
        match result {
            Ok(arguments) => arguments,
            Err(e) => {
                if e == ChainError::InvalidCommandSignature {
                    sink.send(&SystemChat {
                        content: Component::translatable(e.key(), vec![])
                            .color(Color::Named(NamedColor::Red)),
                        overlay: false,
                    });
                } else {
                    crate::chat::decode_failure(sink.as_ref(), &name, e);
                }
                return None;
            }
        }
    };
    let decision = global.plugins.player_command(uuid, &name, command);
    if decision.cancelled {
        return None;
    }
    let replaced = decision.text.map(|t| match t {
        vanillite_plugin_host::NewText::Plain(s) | vanillite_plugin_host::NewText::Json(s) => s,
    });
    let command = replaced.as_deref().unwrap_or(command);
    log::info!("{name} issued server command: /{command}");
    let mut env = Env {
        level,
        global,
        world,
        others: Vec::new(),
    };
    match parse.finish(&commands) {
        Ok(parsed) => {
            let line = engine::Line {
                parsed: Arc::new(parsed),
                signed: Some(Arc::new(signed_arguments)),
            };
            run_line(&mut env, line, source);
        }
        Err(e) => send_error(&source, global, &e, command),
    }
    // `detectCommandRateSpam`.
    let player = env.level.entities.player_mut(id)?;
    let spam = &mut player.connection.chat.command_spam;
    spam.increment();
    (!spam.is_under_threshold() && global.access.op_level(uuid).is_none())
        .then(|| Component::translatable("disconnect.spam", vec![]))
}

/// `collectSignedArguments`: every signed argument must be one the command has, and every
/// signable argument must be signed.
fn collect_signed_arguments(
    chain: &mut vanillite_game::player::chat::MessageChain,
    uuid: u128,
    signed: &SignedCommand,
    signable: &[(String, String)],
    enforce: bool,
    command: &str,
) -> Result<SignedArguments, ChainError> {
    let mut out = SignedArguments::new();
    if signed.signatures.is_empty() {
        for (name, value) in signable {
            let message =
                chain.unpack(uuid, None, MessageBody::unsigned(value.clone()), enforce)?;
            out.insert(name.clone(), message);
        }
        return Ok(out);
    }
    let mismatch = || {
        log::error!("Signed command mismatch between server and client ('{command}')");
        ChainError::InvalidCommandSignature
    };
    for argument in &signed.signatures {
        let Some((name, value)) = signable.iter().find(|(n, _)| *n == argument.name) else {
            chain.set_broken();
            return Err(mismatch());
        };
        let body = MessageBody {
            content: value.clone(),
            timestamp: signed.timestamp,
            salt: signed.salt,
            last_seen: signed.last_seen.clone(),
        };
        let message = chain.unpack(uuid, Some(argument.signature.clone()), body, enforce)?;
        out.insert(name.clone(), message);
    }
    if signable.iter().any(|(n, _)| !out.contains_key(n)) {
        return Err(mismatch());
    }
    Ok(out)
}

/// A console command, run on the overworld.
/// Runs a command line as the server (permission level 4) at `pos` in the overworld on
/// `level`, outside a running server (parity scenarios): the execution is the console's;
/// a line that does not parse is an error.
pub fn run_command_on_level(
    global: &Global,
    level: &mut vanillite_game::level::Level,
    pos: vanillite_base::Vec3,
    line: &str,
) -> Result<(), String> {
    let arc = global.arc().ok_or("the server is gone")?;
    let world = WorldId(0);
    let source = CommandSource::new(arc, SourceKind::Console, 4, world, pos, (0.0, 0.0), None);
    let parsed = global
        .commands()
        .parse_command(line.trim().trim_start_matches('/'), &source)
        .map_err(|e| format!("`{line}`: {}", error_message(&e).plain()))?;
    let mut env = Env {
        level,
        global,
        world,
        others: Vec::new(),
    };
    run_line(
        &mut env,
        engine::Line {
            parsed: Arc::new(parsed),
            signed: None,
        },
        source,
    );
    Ok(())
}

/// Installs the overworld's command block dispatcher on `level`, outside a running server
/// (parity scenarios).
pub fn install_command_runner(global: &Global, level: &mut vanillite_game::level::Level) {
    if let Some(arc) = global.arc() {
        level.set_command_runner(Arc::new(BlockCommands::new(&arc, WorldId(0))));
    }
}

pub(crate) fn run_console_command(world: &mut ServerWorld, global: &Global, line: &str) {
    run_server_command(world, global, line, SourceKind::Console);
}

/// An RCON command (`DedicatedServer.runCommand`), run on the overworld: its feedback.
pub(crate) fn run_rcon_command(world: &mut ServerWorld, global: &Global, line: &str) -> String {
    let buffer = Arc::new(std::sync::Mutex::new(String::new()));
    run_server_command(world, global, line, SourceKind::Rcon(Arc::clone(&buffer)));
    std::mem::take(
        &mut *buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// The dispatcher of one world's command blocks and command block minecarts
/// (`BaseCommandBlock.performCommand`).
pub(crate) struct BlockCommands {
    global: std::sync::Weak<Global>,
    world: WorldId,
    lent: Arc<LentLevels>,
}

impl BlockCommands {
    pub(crate) fn new(global: &Arc<Global>, world: WorldId) -> Self {
        Self {
            global: Arc::downgrade(global),
            world,
            lent: Arc::default(),
        }
    }

    /// Where the world lends the other worlds' levels to its command blocks.
    pub(crate) fn lent(&self) -> Arc<LentLevels> {
        Arc::clone(&self.lent)
    }
}

impl vanillite_game::block::technical::command_block::CommandRunner for BlockCommands {
    fn run(
        &self,
        level: &mut vanillite_game::level::Level,
        run: &vanillite_game::block::technical::command_block::CommandRun<'_>,
    ) -> vanillite_game::block::technical::command_block::CommandOutcome {
        let Some(global) = self.global.upgrade() else {
            return Default::default();
        };
        let output = Arc::new(source::BlockOutput::new(run.track_output, run.name.clone()));
        let entity = run
            .entity
            .and_then(|id| select::local(&level.entities, self.world, id))
            .map(|t| t.source_entity());
        let successes = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let source = CommandSource::new(
            Arc::clone(&global),
            SourceKind::CommandBlock(Arc::clone(&output)),
            2,
            self.world,
            run.pos,
            run.rot,
            entity,
        )
        .with_callback(engine::Callback::Count(Arc::clone(&successes)));
        // `performPrefixedCommand`: one leading `/` is optional.
        let command = run.command.strip_prefix('/').unwrap_or(run.command);
        // Held while the command runs: nothing else reaches the levels meanwhile, and the
        // lender waits for the command before it takes them back.
        let lent = self.lent.hold();
        let mut env = Env {
            level,
            global: &global,
            world: self.world,
            others: lent
                .iter()
                .flat_map(|slot| slot.iter())
                .map(|l| {
                    // SAFETY: the slot holds a level only inside `LentLevels::with`, which
                    // has the exclusive borrow of it and empties the slot under the lock
                    // before it returns, so the pointer is valid while `lent` is held; `env`
                    // is dropped before `lent`, and holding the lock makes this the only
                    // reference made of it.
                    l.as_ref().map(|l| unsafe { &mut *l.0.as_ptr() })
                })
                .collect(),
        };
        match global.commands().parse_command(command, &source) {
            Ok(parsed) => run_line(
                &mut env,
                engine::Line {
                    parsed: Arc::new(parsed),
                    signed: None,
                },
                source,
            ),
            Err(e) => send_error(&source, &global, &e, command),
        }
        drop(env);
        drop(lent);
        vanillite_game::block::technical::command_block::CommandOutcome {
            successes: successes.load(std::sync::atomic::Ordering::Relaxed),
            last_message: output.take(),
        }
    }
}

/// A command of the console or RCON with the owner permission level.
/// A command whose feedback goes to `buffer`, also when it arrives after the command
/// (`/vanillite` answers between the ticks).
#[cfg(test)]
pub(crate) fn run_collected(
    world: &mut ServerWorld,
    global: &Global,
    line: &str,
    buffer: &Arc<std::sync::Mutex<String>>,
) {
    run_server_command(world, global, line, SourceKind::Rcon(Arc::clone(buffer)));
}

fn run_server_command(world: &mut ServerWorld, global: &Global, line: &str, kind: SourceKind) {
    let pos = global.systems.spawn();
    run_server_command_at(world, global, line, kind, pos);
}

/// A server command as the console at `pos` of the world (parity scenarios run theirs at the
/// origin, like the Vanilla recorder).
#[cfg(feature = "test-support")]
pub(crate) fn run_console_command_at(
    world: &mut ServerWorld,
    global: &Global,
    line: &str,
    pos: vanillite_base::Vec3,
) {
    run_server_command_at(world, global, line, SourceKind::Console, pos);
}

fn run_server_command_at(
    world: &mut ServerWorld,
    global: &Global,
    line: &str,
    kind: SourceKind,
    pos: vanillite_base::Vec3,
) {
    let command = line.trim().trim_start_matches('/');
    if command.is_empty() {
        return;
    }
    let Some(arc) = global.arc() else {
        return;
    };
    let source = CommandSource::new(arc, kind, 4, world.id(), pos, (0.0, 0.0), None);
    let id = world.id();
    let mut env = Env {
        level: world.level_mut(),
        global,
        world: id,
        others: Vec::new(),
    };
    match global.commands().parse_command(command, &source) {
        Ok(parsed) => {
            let line = engine::Line {
                parsed: Arc::new(parsed),
                signed: None,
            };
            run_line(&mut env, line, source);
        }
        Err(e) => {
            let context = e.context().map(|context| {
                format!(
                    "{}{}{}<--[HERE]",
                    if context.ellipsis { "..." } else { "" },
                    context.before,
                    context.after
                )
            });
            if matches!(source.kind, SourceKind::Rcon(_)) {
                // `Commands.performCommand`: both lines as failures of the source.
                source.send_failure(global, error_message(&e));
                if let Some(context) = context {
                    source.send_failure(global, vanillite_protocol::text::Component::text(context));
                }
                return;
            }
            log::warn!("{}", error_message(&e).plain());
            if let Some(context) = context {
                log::warn!("{context}");
            }
        }
    }
}

/// Answers the latest tab completion request of each player of this world, once per tick
/// (`ServerCommandSuggestionsProvider`).
pub(crate) fn answer_suggestions(entities: &mut Entities, global: &Global, world: WorldId) {
    let pending: Vec<(EntityId, i32, String)> = entities
        .players
        .iter_mut()
        .filter_map(|(id, p)| {
            p.connection
                .chat
                .suggestion
                .take()
                .map(|(rid, cmd)| (id, rid, cmd))
        })
        .collect();
    for (id, request, line) in pending {
        let Some(source) = player_source(entities, global, id, world) else {
            continue;
        };
        let max = global.commands().limits().max_suggestions;
        let packet = global
            .commands()
            .suggest(&line, &source)
            .to_packet(request, &line, max);
        if let SourceKind::Player { sink, .. } = &source.kind {
            sink.send(&packet);
        }
    }
}
