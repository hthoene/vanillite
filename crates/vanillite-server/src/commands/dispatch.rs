//! Runs a parsed command: reads its typed arguments, resolves selectors and messages, and calls
//! the command's implementation in [`super::exec`].

use vanillite_command::error::CommandError;
use vanillite_command::selector::EntitySelector;
use vanillite_command::values::Anchor;
use vanillite_command::{GameProfileArg, Message, Parsed, Source as _};
use vanillite_game::commands::{CommandId, ExecuteOp};
use vanillite_game::player::chat::ChatMessage;
use vanillite_game::player::join::display_name;
use vanillite_protocol::packets::play::clientbound::{LookAtEntity, PlayerLookAt};
use vanillite_protocol::text::Component;

use super::exec::{self, Destination, Profile, config_game_mode, format_list};
use super::select::{self, Target};
use super::source::CommandSource;
use super::survival;
use super::{Env, SignedArguments};
use vanillite_game::survival::rules::Rule;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![vanillite_command::Arg::Str(format!(
            "missing argument {name}"
        ))],
    )
}

/// `EntityArgument.getEntities`/`getPlayers` and their single forms.
fn targets(
    env: &Env<'_>,
    source: &CommandSource,
    selector: &EntitySelector,
    _players_only: bool,
) -> Result<Vec<Target>, CommandError> {
    select::many(env, source, selector)
}

fn selector_arg<'a>(
    parsed: &'a Parsed<CommandId>,
    name: &str,
) -> Result<&'a EntitySelector, CommandError> {
    parsed.args.selector(name).ok_or_else(|| missing(name))
}

/// `GameProfileArgument.getGameProfiles`: selected players, or a name of an online player,
/// an operator or a whitelisted player. In offline mode an unknown name gets its offline UUID.
pub(super) fn profiles(
    env: &Env<'_>,
    source: &CommandSource,
    parsed: &Parsed<CommandId>,
) -> Result<Vec<Profile>, CommandError> {
    let arg = parsed
        .args
        .game_profile("targets")
        .ok_or_else(|| missing("targets"))?;
    match arg {
        GameProfileArg::Selector(selector) => Ok(targets(env, source, selector, true)?
            .into_iter()
            .map(|t| Profile {
                uuid: t.uuid,
                name: t.name,
            })
            .collect()),
        GameProfileArg::Name(name) => {
            let global = env.global;
            let online = global
                .players
                .snapshot()
                .into_iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
                .map(|p| Profile {
                    uuid: p.uuid.0,
                    name: p.name,
                });
            let known = || {
                global
                    .access
                    .ops()
                    .into_iter()
                    .find(|o| o.name.eq_ignore_ascii_case(name))
                    .map(|o| Profile {
                        uuid: o.uuid.0,
                        name: o.name,
                    })
            };
            let offline = || {
                (!global.online_mode).then(|| Profile {
                    uuid: vanillite_protocol::types::Uuid::offline(name).0,
                    name: name.clone(),
                })
            };
            online
                .or_else(known)
                .or_else(offline)
                .map(|p| vec![p])
                .ok_or_else(|| CommandError::new("argument.player.unknown", vec![]))
        }
    }
}

/// `MessageArgument.Message.resolveComponent`: the text with its selectors replaced by the
/// names they select.
pub(super) fn resolve_message(
    env: &Env<'_>,
    source: &CommandSource,
    message: &Message,
) -> Result<Component, CommandError> {
    if message.parts.is_empty() || !source.allows_selectors() {
        return Ok(Component::text(message.text.as_str()));
    }
    let text = message.text.as_str();
    let first = message.parts.first().map_or(0, |p| p.start);
    let mut out = Component::text(text.get(..first).unwrap_or(""));
    let mut previous = first;
    for part in &message.parts {
        let names = select::select(&part.selector, source, env)
            .into_iter()
            .map(|t| display_name(&t.name, t.uuid))
            .collect();
        if previous < part.start {
            out = out.append(Component::text(
                text.get(previous..part.start).unwrap_or(""),
            ));
        }
        out = out.append(format_list(names));
        previous = part.end;
    }
    if previous < text.len() {
        out = out.append(Component::text(text.get(previous..).unwrap_or("")));
    }
    Ok(out)
}

/// `MessageArgument.resolveChatMessage` of the argument `name`: the signed message with the
/// resolved text as unsigned content, or a system message when the argument is not signed.
pub(super) fn chat_message(
    env: &Env<'_>,
    source: &CommandSource,
    parsed: &Parsed<CommandId>,
    signed: &SignedArguments,
    name: &str,
) -> Result<ChatMessage, CommandError> {
    let message = parsed.args.message(name).ok_or_else(|| missing(name))?;
    let formatted = resolve_message(env, source, message)?;
    let mut out = match signed.get(name) {
        Some(m) => m.clone(),
        None => ChatMessage::system(message.text.clone()),
    };
    // `withUnsignedContent`: kept only when it differs from the plain signed text.
    if formatted != Component::text(out.body.content.as_str()) {
        out.unsigned_content = Some(formatted);
    }
    Ok(out)
}

fn require_player(source: &CommandSource) -> Result<(), CommandError> {
    match source.uuid() {
        Some(_) => Ok(()),
        None => Err(CommandError::new("permissions.requires.player", vec![])),
    }
}

/// The source itself as a target.
pub(crate) fn self_target(env: &Env<'_>, source: &CommandSource) -> Result<Target, CommandError> {
    let own = source
        .entity
        .as_ref()
        .ok_or_else(|| CommandError::new("permissions.requires.entity", vec![]))?;
    let found = match own.id.filter(|_| own.world == env.world) {
        Some(id) => select::local(&env.level.entities, env.world, id),
        None => select::online_players(&env.level.entities, env.global, source.world)
            .into_iter()
            .find(|t| t.uuid == own.uuid)
            // An entity of another world (`execute in` moved the execution): known by the
            // source.
            .or_else(|| {
                (own.world != env.world && !own.is_player()).then(|| Target {
                    uuid: own.uuid,
                    name: own.name.clone(),
                    world: own.world,
                    entity: None,
                    place: None,
                    game_mode: vanillite_config::GameMode::Survival,
                    kind: own.kind,
                })
            }),
    };
    found.ok_or_else(|| CommandError::new("permissions.requires.entity", vec![]))
}

/// `LookAt.perform` for players: `player_look_at` from the eyes.
fn look_at(
    env: &Env<'_>,
    source: &CommandSource,
    targets: &[Target],
    parsed: &Parsed<CommandId>,
) -> Result<(), CommandError> {
    let packet = if let Some(location) = parsed.args.coordinates("facingLocation") {
        let p = exec::resolve_position(source, location);
        Some((p, None))
    } else if parsed.args.has("facingEntity") {
        let to = targets_single(env, source, selector_arg(parsed, "facingEntity")?)?;
        let anchor = match parsed.args.anchor("facingAnchor") {
            Some(Anchor::Eyes) => 1,
            _ => 0,
        };
        let network_id = to
            .entity
            .and_then(|id| env.level.entities.store.get(id))
            .map(|e| e.network_id().0);
        let pos = to.place.map_or(source.pos, |(p, _, _)| p);
        Some((
            pos,
            network_id.map(|entity_id| LookAtEntity { entity_id, anchor }),
        ))
    } else {
        None
    };
    let Some((pos, entity)) = packet else {
        return Ok(());
    };
    for target in targets {
        if let Some((sink, _)) = env
            .global
            .players
            .connection(crate::player_list::Uuid(target.uuid))
        {
            sink.send(&PlayerLookAt {
                from_anchor: 0,
                x: pos.x,
                y: pos.y,
                z: pos.z,
                entity,
            });
        }
    }
    Ok(())
}

fn targets_single(
    env: &Env<'_>,
    source: &CommandSource,
    selector: &EntitySelector,
) -> Result<Target, CommandError> {
    select::single(env, source, selector)
}

/// Executes a parsed command; errors are shown to the source by the caller.
pub(crate) fn execute(
    source: &CommandSource,
    env: &mut Env<'_>,
    parsed: &Parsed<CommandId>,
    signed: &SignedArguments,
) -> Result<i32, CommandError> {
    let global = env.global;
    match parsed.command {
        CommandId::Plugin(command) => plugin_command(env, source, parsed, command),
        CommandId::Advancement { grant, mode } => {
            super::advancement_cmd::advancement(grant, mode, &parsed.args, source, env)
        }
        CommandId::GameModeSelf | CommandId::GameModeTargets => {
            let mode = parsed
                .args
                .game_mode("gamemode")
                .ok_or_else(|| missing("gamemode"))?;
            let targets = if parsed.command == CommandId::GameModeSelf {
                require_player(source)?;
                vec![self_target(env, source)?]
            } else {
                targets(env, source, selector_arg(parsed, "target")?, true)?
            };
            Ok(exec::game_mode(
                source,
                &mut env.level.entities,
                global,
                targets,
                config_game_mode(*mode),
            ))
        }
        CommandId::Help | CommandId::HelpCommand => {
            let command = parsed.args.string("command").map(String::as_str);
            let lines = global.commands().help_lines(command, source)?;
            for line in &lines {
                source.send_success(global, Component::text(line.as_str()), false);
            }
            Ok(i32::try_from(lines.len()).unwrap_or(i32::MAX))
        }
        CommandId::Kick => {
            let targets = targets(env, source, selector_arg(parsed, "targets")?, true)?;
            let reason = match parsed.args.message("reason") {
                Some(message) => resolve_message(env, source, message)?,
                None => Component::translatable("multiplayer.disconnect.kicked", vec![]),
            };
            Ok(exec::kick(source, global, &targets, reason))
        }
        CommandId::List => Ok(exec::list(source, global, false)),
        CommandId::ListUuids => Ok(exec::list(source, global, true)),
        CommandId::Msg => {
            let targets = targets(env, source, selector_arg(parsed, "targets")?, true)?;
            let message = chat_message(env, source, parsed, signed, "message")?;
            Ok(exec::msg(source, global, &targets, &message))
        }
        CommandId::Say => {
            let message = chat_message(env, source, parsed, signed, "message")?;
            exec::say(source, global, &message);
            Ok(1)
        }
        CommandId::Seed => Ok(exec::seed(source, global, global.seed)),
        CommandId::TeleportToLocation
        | CommandId::TeleportToEntity
        | CommandId::TeleportTargetsToLocation
        | CommandId::TeleportTargetsToEntity => {
            let victims = match parsed.command {
                CommandId::TeleportToLocation | CommandId::TeleportToEntity => {
                    vec![self_target(env, source)?]
                }
                _ => targets(env, source, selector_arg(parsed, "targets")?, false)?,
            };
            let local: Vec<Target> = victims
                .iter()
                .filter(|t| t.entity.is_some())
                .cloned()
                .collect();
            let destination = match parsed.command {
                CommandId::TeleportToEntity | CommandId::TeleportTargetsToEntity => {
                    Destination::Entity(targets_single(
                        env,
                        source,
                        selector_arg(parsed, "destination")?,
                    )?)
                }
                _ => Destination::Position {
                    coordinates: *parsed
                        .args
                        .coordinates("location")
                        .ok_or_else(|| missing("location"))?,
                    rotation: parsed.args.coordinates("rotation").copied(),
                },
            };
            let count = exec::teleport(source, env, victims, destination)?;
            look_at(env, source, &local, parsed)?;
            Ok(count)
        }
        CommandId::TimeQueryGameTime => Ok(exec::time_gametime(
            source,
            global,
            global.level.game_time(),
        )),
        CommandId::TimeQueryTime
        | CommandId::TimeQueryTimeline
        | CommandId::TimeQueryRepetition
        | CommandId::TimeSet
        | CommandId::TimeSetMarker
        | CommandId::TimeAdd
        | CommandId::TimePause
        | CommandId::TimeResume
        | CommandId::TimeRate => {
            let clock = survival::clock(
                source,
                global,
                parsed.args.identifier("clock").map(String::as_str),
            )?;
            match parsed.command {
                CommandId::TimeQueryTime => Ok(survival::time_query(source, global, clock)),
                CommandId::TimeQueryTimeline | CommandId::TimeQueryRepetition => {
                    let timeline = parsed
                        .args
                        .identifier("timeline")
                        .ok_or_else(|| missing("timeline"))?;
                    survival::time_timeline(
                        source,
                        global,
                        clock,
                        timeline,
                        parsed.command == CommandId::TimeQueryRepetition,
                    )
                }
                CommandId::TimeSet => {
                    let time = parsed.args.time("time").ok_or_else(|| missing("time"))?;
                    survival::time_set(source, global, clock, *time)
                }
                CommandId::TimeSetMarker => {
                    let marker = parsed
                        .args
                        .identifier("timemarker")
                        .ok_or_else(|| missing("timemarker"))?;
                    survival::time_set_marker(source, global, clock, marker)
                }
                CommandId::TimeAdd => {
                    let time = parsed.args.time("time").ok_or_else(|| missing("time"))?;
                    Ok(survival::time_add(source, global, clock, *time))
                }
                CommandId::TimePause => survival::time_pause(source, global, clock, true),
                CommandId::TimeResume => survival::time_pause(source, global, clock, false),
                _ => {
                    let rate = parsed.args.float("rate").ok_or_else(|| missing("rate"))?;
                    survival::time_rate(source, global, clock, *rate)
                }
            }
        }
        CommandId::GameRuleQuery(index) => Ok(survival::game_rule_query(
            source,
            global,
            Rule(usize::from(index)),
        )),
        CommandId::GameRuleSet(index) => {
            let rule = Rule(usize::from(index));
            let value = match (parsed.args.bool("value"), parsed.args.int("value")) {
                (Some(b), _) => i32::from(*b),
                (None, Some(i)) => *i,
                (None, None) => return Err(missing("value")),
            };
            survival::game_rule_set(source, global, rule, value)
        }
        CommandId::KillSelf | CommandId::KillTargets => {
            let victims = if parsed.command == CommandId::KillSelf {
                vec![self_target(env, source)?]
            } else {
                targets(env, source, selector_arg(parsed, "targets")?, false)?
            };
            Ok(survival::kill(source, env, &victims))
        }
        CommandId::WeatherClear | CommandId::WeatherRain | CommandId::WeatherThunder => {
            let kind = match parsed.command {
                CommandId::WeatherClear => survival::Weather::Clear,
                CommandId::WeatherRain => survival::Weather::Rain,
                _ => survival::Weather::Thunder,
            };
            let duration = parsed.args.time("duration").copied();
            Ok(survival::weather(source, env, kind, duration))
        }
        CommandId::Op => {
            let profiles = profiles(env, source, parsed)?;
            exec::op(source, global, &profiles)
        }
        CommandId::Deop => {
            let profiles = profiles(env, source, parsed)?;
            exec::deop(source, global, &profiles)
        }
        CommandId::SaveAll => Ok(exec::save_all(source, global, false)),
        CommandId::SaveAllFlush => Ok(exec::save_all(source, global, true)),
        CommandId::SaveOff => exec::save_switch(source, global, false),
        CommandId::SaveOn => exec::save_switch(source, global, true),
        CommandId::Stop => Ok(exec::stop(source, global)),
        CommandId::WhitelistOn => exec::whitelist_switch(source, global, true),
        CommandId::WhitelistOff => exec::whitelist_switch(source, global, false),
        CommandId::WhitelistList => Ok(exec::whitelist_list(source, global)),
        CommandId::WhitelistAdd => {
            let profiles = profiles(env, source, parsed)?;
            exec::whitelist_change(source, global, &profiles, true)
        }
        CommandId::WhitelistRemove => {
            let profiles = profiles(env, source, parsed)?;
            exec::whitelist_change(source, global, &profiles, false)
        }
        CommandId::WhitelistReload => Ok(exec::whitelist_reload(source, global)),
        CommandId::Execute(ExecuteOp::Test { condition, unless }) => {
            super::execute::run_condition(condition, unless, &parsed.args, source, env)
        }
        CommandId::Scoreboard(op) => super::scores::run(op, &parsed.args, source, env),
        CommandId::Summon
        | CommandId::RideMount
        | CommandId::RideDismount
        | CommandId::Rotate
        | CommandId::Damage
        | CommandId::Swing { .. }
        | CommandId::EffectClear
        | CommandId::EffectGive { .. } => {
            super::entities_cmd::run(parsed.command, &parsed.args, source, env)
        }
        CommandId::Loot {
            target,
            source: from,
        } => super::loot_cmd::run(target, from, &parsed.args, source, env),
        CommandId::Bossbar(op) => super::bossbar_cmd::run(op, &parsed.args, source, env),
        CommandId::Particle { force } => super::particle::run(force, &parsed.args, source, env),
        CommandId::PlaySound(index) => super::sound::play_sound(index, &parsed.args, source, env),
        CommandId::StopSound(index) => super::sound::stop_sound(index, &parsed.args, source, env),
        CommandId::PostEffect(op) => super::sound::post_effect(op, &parsed.args, source, env),
        CommandId::Attribute(op) => super::player_data::attribute(op, &parsed.args, source, env),
        CommandId::Clear => super::items::clear(&parsed.args, source, env),
        CommandId::Data(op) => super::data_cmd::run_data(op, &parsed.args, source, env),
        CommandId::DataPack(_) | CommandId::Reload => super::pack_cmd::run(parsed, source, env),
        CommandId::Enchant => super::items::enchant(&parsed.args, source, env),
        CommandId::Experience { op, levels } => {
            super::player_data::experience(op, levels, &parsed.args, source, env)
        }
        CommandId::Give => super::items::give(&parsed.args, source, env),
        CommandId::Item(op) => super::items::item(op, &parsed.args, source, env),
        CommandId::RecipeGive => super::player_data::recipe(true, &parsed.args, source, env),
        CommandId::RecipeTake => super::player_data::recipe(false, &parsed.args, source, env),
        CommandId::Tag(op) => super::data_cmd::run_tag(op, &parsed.args, source, env),
        CommandId::TellRaw
        | CommandId::Title(_)
        | CommandId::Me
        | CommandId::TeamMsg
        | CommandId::Trigger(_)
        | CommandId::DialogShow
        | CommandId::DialogClear => super::chat_cmd::run(parsed, signed, source, env),
        CommandId::Team(op) => super::teams::run(op, &parsed.args, source, env),
        CommandId::Ban
        | CommandId::BanIp
        | CommandId::BanList(_)
        | CommandId::Pardon
        | CommandId::PardonIp => super::bans::run(parsed, source, env),
        CommandId::ScheduleFunction(replace) => {
            super::schedule::schedule(&parsed.args, replace, source, env)
        }
        CommandId::ScheduleClear => super::schedule::clear(&parsed.args, source, env),
        CommandId::Random(_)
        | CommandId::FetchProfile(_)
        | CommandId::Version
        | CommandId::Plugins
        | CommandId::BukkitVersion
        | CommandId::Spectate
        | CommandId::Stopwatch(_)
        | CommandId::Test(_)
        | CommandId::Waypoint(_)
        | CommandId::JfrStart
        | CommandId::JfrStop
        | CommandId::PerfStart
        | CommandId::PerfStop
        | CommandId::DebugStart
        | CommandId::DebugStop
        | CommandId::SetIdleTimeout
        | CommandId::Transfer => super::system_cmd::run(parsed, source, env),
        CommandId::Vanillite(op) => super::vanillite_cmd::run(op, parsed, source, env),
        CommandId::SetBlock(mode) => super::blocks_cmd::set_block(mode, &parsed.args, source, env),
        CommandId::Fill(mode) => super::blocks_cmd::fill(mode, &parsed.args, source, env),
        CommandId::Clone {
            filter,
            mode,
            strict,
        } => super::blocks_cmd::clone((filter, mode, strict), &parsed.args, source, env),
        CommandId::FillBiome => super::blocks_cmd::fill_biome(&parsed.args, source, env),
        CommandId::ForceLoad(op) => super::world_cmd::force_load(op, &parsed.args, source, env),
        CommandId::SetWorldSpawn => super::world_cmd::set_world_spawn(&parsed.args, source, env),
        CommandId::SpawnPoint => {
            let targets = if parsed.args.has("targets") {
                targets(env, source, selector_arg(parsed, "targets")?, true)?
            } else {
                require_player(source)?;
                vec![self_target(env, source)?]
            };
            super::world_cmd::spawn_point(&targets, &parsed.args, source, env)
        }
        CommandId::DifficultyQuery => Ok(super::world_cmd::difficulty_query(source, global)),
        CommandId::DifficultySet(id) => super::world_cmd::difficulty_set(source, global, id),
        CommandId::DefaultGameMode => {
            super::world_cmd::default_game_mode(&parsed.args, source, env)
        }
        CommandId::SpreadPlayers => {
            let targets = targets(env, source, selector_arg(parsed, "targets")?, false)?;
            super::world_cmd::spread_players(targets, &parsed.args, source, env)
        }
        CommandId::Tick(op) => super::world_cmd::tick(op, &parsed.args, source, global),
        CommandId::WorldBorder(op) => super::border_cmd::run(op, &parsed.args, source, env),
        CommandId::Locate(op) => super::locate::locate(op, &parsed.args, source, env),
        CommandId::Place(op) => super::locate::place(op, &parsed.args, source, env),
        // Redirect modifiers never end a command; the engine runs `function` and `return`.
        CommandId::Execute(_)
        | CommandId::ExecuteIfFunction
        | CommandId::ExecuteUnlessFunction
        | CommandId::Function
        | CommandId::FunctionWithArguments
        | CommandId::FunctionWith
        | CommandId::FunctionWithPath
        | CommandId::ReturnValue
        | CommandId::ReturnFail
        | CommandId::ReturnRun
        | CommandId::DebugFunction => Err(CommandError::new("command.unknown.command", vec![])),
    }
}

/// The arguments of a plugin command: selectors resolved to players or entities,
/// coordinates to positions, rotations to angles; the argument types plugins get as text
/// keep the typed input. Argument types plugins cannot declare are left out.
pub(crate) fn plugin_args(
    env: Option<&Env<'_>>,
    source: &CommandSource,
    args: &vanillite_command::Args,
    input: &str,
) -> Result<Vec<vanillite_plugin_host::Arg>, CommandError> {
    use vanillite_abi::arg;
    use vanillite_command::{ArgValue as V, ArgumentType as T, NodeKind};
    use vanillite_plugin_host::{Arg, ArgValue};
    let mut out = Vec::new();
    let commands = source.global.commands();
    for a in args.iter() {
        let ty = match commands.node(a.node).map(|n| &n.kind) {
            Some(NodeKind::Argument { ty, .. }) => Some(ty),
            _ => None,
        };
        let raw = |kind: u32| {
            let text = input.get(a.range.start..a.range.end).unwrap_or_default();
            ArgValue::Raw(kind, text.to_owned())
        };
        let value = match (&a.value, ty) {
            (V::Bool(b), _) => ArgValue::Bool(*b),
            (V::Int(v), _) => ArgValue::Integer(i64::from(*v)),
            (V::Long(v), _) => ArgValue::Long(*v),
            (V::Float(v), _) => ArgValue::Float(f64::from(*v)),
            (V::Double(v), _) => ArgValue::Double(*v),
            (V::String(s), _) => ArgValue::Text(s.clone()),
            (V::Entity(selector), ty) => match env {
                Some(env) => {
                    let found = targets(env, source, selector, true)?
                        .into_iter()
                        .map(|t| t.uuid)
                        .collect();
                    match ty {
                        Some(T::Entity {
                            players_only: false,
                            ..
                        }) => ArgValue::Entities(found),
                        _ => ArgValue::Players(found),
                    }
                }
                None => continue,
            },
            (V::Coordinates(c), Some(T::Rotation)) => {
                let (yaw, pitch) = exec::resolve_rotation(source, c);
                ArgValue::Rotation([f64::from(yaw), f64::from(pitch)])
            }
            (V::Coordinates(c), Some(T::Vec3 { .. })) => {
                let pos = exec::resolve_position(source, c);
                ArgValue::Vec3([pos.x, pos.y, pos.z])
            }
            (V::Coordinates(coordinates), _) => {
                let pos = exec::resolve_position(source, coordinates);
                ArgValue::Pos([
                    vanillite_base::math::floor(pos.x),
                    vanillite_base::math::floor(pos.y),
                    vanillite_base::math::floor(pos.z),
                ])
            }
            (V::Identifier(id), _) => ArgValue::Raw(arg::IDENTIFIER, id.clone()),
            (V::Uuid(_), _) => raw(arg::UUID),
            (V::Compound(_), _) => raw(arg::NBT_COMPOUND),
            (V::GameMode(mode), _) => {
                use vanillite_abi::game_mode as g;
                use vanillite_command::values::GameMode as M;
                let mode = match mode {
                    M::Survival => g::SURVIVAL,
                    M::Creative => g::CREATIVE,
                    M::Adventure => g::ADVENTURE,
                    M::Spectator => g::SPECTATOR,
                };
                ArgValue::Int(arg::GAME_MODE, i64::from(mode))
            }
            (V::Time(ticks), _) => ArgValue::Int(arg::TIME, i64::from(*ticks)),
            (_, Some(T::BlockState)) => raw(arg::BLOCK_STATE),
            (_, Some(T::ItemStack)) => raw(arg::ITEM_STACK),
            (_, Some(T::Component)) => raw(arg::COMPONENT),
            _ => continue,
        };
        out.push(Arg {
            name: a.name.clone(),
            value,
        });
    }
    Ok(out)
}

/// Runs a plugin's command (no world access: the plugin schedules on the world for that).
fn plugin_command(
    env: &mut Env<'_>,
    source: &CommandSource,
    parsed: &Parsed<CommandId>,
    command: u32,
) -> Result<i32, CommandError> {
    let args = plugin_args(Some(env), source, &parsed.args, &parsed.input)?;
    env.global
        .plugins
        .execute_command(command, &source.plugin_sender(), &parsed.input, &args, None)
        .ok_or_else(|| CommandError::new("command.failed", vec![]))
}
