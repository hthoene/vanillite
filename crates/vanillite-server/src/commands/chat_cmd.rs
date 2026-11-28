//! The chat commands: `tellraw`, `title`, `me`, `teammsg`, `trigger` and `dialog` (Vanilla's
//! `TellRawCommand`, `TitleCommand`, `EmoteCommands`, `TeamMsgCommand`, `TriggerCommand`,
//! `DialogCommand`).

use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, GameValue, InlineOrId, Parsed};
use vanillite_game::commands::{CommandId, TitleOp, TriggerOp};
use vanillite_game::player::chat::ChatMessage;
use vanillite_game::scoreboard::CriteriaKind;
use vanillite_protocol::chat::ChatTypeBound;
use vanillite_protocol::packets::play::clientbound::{
    ClearDialog, ClearTitles, DialogRef, SetActionBarText, SetSubtitleText, SetTitleText,
    SetTitlesAnimation, ShowDialog, SystemChat,
};
use vanillite_protocol::text::{Arg, ClickEvent, Component, HoverEvent};
use vanillite_protocol::types::Nbt;

use super::select::{self, Target};
use super::source::CommandSource;
use super::tracker::Tracker;
use super::{Env, SignedArguments};
use crate::player_list::Uuid;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

/// `EntityArgument.getPlayers`.
fn players(
    env: &Env<'_>,
    source: &CommandSource,
    parsed: &Parsed<CommandId>,
    name: &str,
) -> Result<Vec<Target>, CommandError> {
    let selector = parsed.args.selector(name).ok_or_else(|| missing(name))?;
    select::many(env, source, selector)
}

fn raw_component<'a>(
    parsed: &'a Parsed<CommandId>,
    name: &str,
) -> Result<&'a Component, CommandError> {
    match parsed.args.game(name) {
        Some(GameValue::Component(c)) => Ok(c),
        _ => Err(missing(name)),
    }
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Runs a chat command.
pub(crate) fn run(
    parsed: &Parsed<CommandId>,
    signed: &SignedArguments,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    match parsed.command {
        CommandId::TellRaw => tellraw(parsed, source, env),
        CommandId::Title(op) => title(op, parsed, source, env),
        CommandId::Me => {
            let message = super::dispatch::chat_message(env, source, parsed, signed, "action")?;
            me(source, env, &message);
            Ok(1)
        }
        CommandId::TeamMsg => team_msg(parsed, signed, source, env),
        CommandId::Trigger(op) => trigger(op, parsed, source, env),
        CommandId::DialogShow | CommandId::DialogClear => dialog(parsed, source, env),
        _ => Err(CommandError::new("command.unknown.command", vec![])),
    }
}

/// `TellRawCommand`: the component, resolved for each player, as a system message.
fn tellraw(
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let targets = players(env, source, parsed, "targets")?;
    let message = raw_component(parsed, "message")?;
    let mut result = 0i32;
    for target in &targets {
        let content = super::text::resolve(env, source, message, Some(target))?;
        // `ServerPlayer.sendSystemMessage`.
        if let Some((sink, inbox)) = env.global.players.connection(Uuid(target.uuid)) {
            let accepts = inbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .accepts_system(false);
            if accepts {
                sink.send(&SystemChat {
                    content,
                    overlay: false,
                });
            }
        }
        result = result.saturating_add(1);
    }
    Ok(result)
}

/// `TitleCommand`.
fn title(
    op: TitleOp,
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let targets = players(env, source, parsed, "targets")?;
    let mut tracker = Tracker::<Target>::default();
    for target in &targets {
        let Some((sink, _)) = env.global.players.connection(Uuid(target.uuid)) else {
            tracker.track(target, 1);
            continue;
        };
        match op {
            TitleOp::Clear | TitleOp::Reset => {
                sink.send(&ClearTitles {
                    reset: op == TitleOp::Reset,
                });
            }
            TitleOp::Times => {
                let time =
                    |name: &str| parsed.args.time(name).copied().ok_or_else(|| missing(name));
                sink.send(&SetTitlesAnimation {
                    fade_in: time("fadeIn")?,
                    stay: time("stay")?,
                    fade_out: time("fadeOut")?,
                });
            }
            TitleOp::Title | TitleOp::Subtitle | TitleOp::Actionbar => {
                let raw = raw_component(parsed, "title")?;
                let text = super::text::resolve(env, source, raw, Some(target))?;
                match op {
                    TitleOp::Title => sink.send(&SetTitleText { text }),
                    TitleOp::Subtitle => sink.send(&SetSubtitleText { text }),
                    _ => sink.send(&SetActionBarText { text }),
                };
            }
        }
        tracker.track(target, 1);
    }
    let base = match op {
        TitleOp::Clear => "commands.title.cleared",
        TitleOp::Reset => "commands.title.reset",
        TitleOp::Title => "commands.title.show.title",
        TitleOp::Subtitle => "commands.title.show.subtitle",
        TitleOp::Actionbar => "commands.title.show.actionbar",
        TitleOp::Times => "commands.title.times",
    };
    tracker.feedback(source, env, true, false, None, |one, n, _| match one {
        Some(player) => {
            Component::translatable(format!("{base}.single"), vec![player.display(env.global)])
        }
        None => Component::translatable_args(format!("{base}.multiple"), vec![Arg::Int(n)]),
    })
}

/// `EmoteCommands`: the action to everyone as `emote_command`.
fn me(source: &CommandSource, env: &Env<'_>, message: &ChatMessage) {
    let global = env.global;
    let chat_type = ChatTypeBound {
        chat_type: global.chat_types.emote_command,
        name: source.display_name(),
        target_name: None,
    };
    crate::chat::broadcast_chat(global, message, &chat_type, "chat.type.emote");
}

/// `TeamMsgCommand`: the message to the online members of the source's team.
fn team_msg(
    parsed: &Parsed<CommandId>,
    signed: &SignedArguments,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    let entity = source
        .entity
        .as_ref()
        .ok_or_else(|| CommandError::new("permissions.requires.entity", vec![]))?;
    let own_name = if entity.is_player() {
        entity.name.clone()
    } else {
        vanillite_protocol::types::Uuid(entity.uuid).to_string()
    };
    let (team, receivers) = {
        let board = global.systems.scoreboard();
        let Some(team) = board.team_of(&own_name).cloned() else {
            return Err(CommandError::new("commands.teammsg.failed.noteam", vec![]));
        };
        let receivers: Vec<_> = global
            .players
            .snapshot()
            .into_iter()
            .filter(|p| {
                p.uuid.0 == entity.uuid
                    || board.team_of(&p.name).is_some_and(|t| t.name == team.name)
            })
            .collect();
        (team, receivers)
    };
    if !receivers.is_empty() {
        let message = super::dispatch::chat_message(env, source, parsed, signed, "message")?;
        // `SUGGEST_STYLE`.
        let team_name = team
            .formatted_display_name()
            .hover(HoverEvent::ShowText(Component::translatable(
                "chat.type.team.hover",
                vec![],
            )))
            .click(ClickEvent::SuggestCommand("/teammsg ".into()));
        let bound = |chat_type: i32| ChatTypeBound {
            chat_type,
            name: source.display_name(),
            target_name: Some(team_name.clone()),
        };
        let incoming = bound(global.chat_types.team_msg_incoming);
        let outgoing = bound(global.chat_types.team_msg_outgoing);
        let max = global.chat.max_pending_messages;
        for receiver in &receivers {
            let chat_type = if receiver.uuid.0 == entity.uuid {
                &outgoing
            } else {
                &incoming
            };
            let sent = receiver
                .inbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(&message, chat_type, receiver.sink.as_ref(), max);
            if !sent {
                receiver.sink.kick(Component::translatable(
                    "multiplayer.disconnect.too_many_pending_chats",
                    vec![],
                ));
            }
        }
    }
    Ok(count(receivers.len()))
}

/// `TriggerCommand`.
fn trigger(
    op: TriggerOp,
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let player = source
        .entity
        .as_ref()
        .filter(|e| e.is_player())
        .ok_or_else(|| CommandError::new("permissions.requires.player", vec![]))?;
    let objective = super::scores::objective(env, &parsed.args, "objective")?;
    let global = env.global;
    let (value, formatted) = {
        let mut board = global.systems.scoreboard();
        let Some(o) = board.objective(&objective) else {
            return Err(CommandError::new("commands.trigger.failed.invalid", vec![]));
        };
        if o.criteria != CriteriaKind::Trigger {
            return Err(CommandError::new("commands.trigger.failed.invalid", vec![]));
        }
        let formatted = o.formatted_display_name();
        // `getScore`: a primed (unlocked) score, locked again.
        let current = match board.score(&player.name, &objective) {
            Some(s) if !s.locked => s.value,
            _ => {
                return Err(CommandError::new(
                    "commands.trigger.failed.unprimed",
                    vec![],
                ));
            }
        };
        board.set_locked(&player.name, &objective, true);
        let value = match op {
            TriggerOp::Simple => current.wrapping_add(1),
            TriggerOp::Add => {
                current.wrapping_add(*parsed.args.int("value").ok_or_else(|| missing("value"))?)
            }
            TriggerOp::Set => *parsed.args.int("value").ok_or_else(|| missing("value"))?,
        };
        board.set_score(&player.name, &objective, value, Some(&player.display));
        (value, formatted)
    };
    let message = match op {
        TriggerOp::Simple => {
            Component::translatable("commands.trigger.simple.success", vec![formatted])
        }
        TriggerOp::Add => Component::translatable_args(
            "commands.trigger.add.success",
            vec![
                Arg::from(formatted),
                Arg::Int(*parsed.args.int("value").unwrap_or(&0)),
            ],
        ),
        TriggerOp::Set => Component::translatable_args(
            "commands.trigger.set.success",
            vec![Arg::from(formatted), Arg::Int(value)],
        ),
    };
    source.send_success(global, message, true);
    Ok(value)
}

/// `DialogCommand`.
fn dialog(
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let targets = players(env, source, parsed, "targets")?;
    let show = parsed.command == CommandId::DialogShow;
    let packet = if show {
        let dialog = match parsed.args.game("dialog") {
            Some(GameValue::InlineOrId(InlineOrId::Id(id))) => {
                let index = env
                    .global
                    .registries
                    .get("minecraft:dialog")
                    .and_then(|entries| entries.iter().position(|e| e == id))
                    .and_then(|i| i32::try_from(i).ok())
                    .ok_or_else(|| {
                        CommandError::new(
                            "argument.resource_or_id.failed_to_parse",
                            vec![ErrArg::Str(id.clone())],
                        )
                    })?;
                DialogRef::Registered(index)
            }
            Some(GameValue::InlineOrId(InlineOrId::Inline(tag))) => {
                DialogRef::Direct(Nbt(Some(tag.clone())))
            }
            _ => return Err(missing("dialog")),
        };
        Some(ShowDialog { dialog })
    } else {
        None
    };
    let mut tracker = Tracker::<Target>::default();
    for target in &targets {
        if let Some((sink, _)) = env.global.players.connection(Uuid(target.uuid)) {
            match &packet {
                Some(p) => sink.send(p),
                None => sink.send(&ClearDialog),
            };
        }
        tracker.track(target, 1);
    }
    let base = if show {
        "commands.dialog.show"
    } else {
        "commands.dialog.clear"
    };
    tracker.feedback(source, env, true, false, None, |one, n, _| match one {
        Some(player) => {
            Component::translatable(format!("{base}.single"), vec![player.display(env.global)])
        }
        None => Component::translatable_args(format!("{base}.multiple"), vec![Arg::Int(n)]),
    })
}
