//! `/advancement (grant|revoke)` (Vanilla's `AdvancementCommands`). Players in other worlds
//! are changed there and count as changed for every advancement.

use std::sync::Arc;

use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, Args};
use vanillite_game::advancements::{Advancements, Grant};
use vanillite_game::commands::AdvancementMode;
use vanillite_game::level::Level;
use vanillite_protocol::text::{Arg, Component};
use vanillite_world::entity::EntityId;

use super::Env;
use super::select;
use super::source::CommandSource;
use super::tracker::Tracker;
use crate::world::ServerWorld;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

/// `AdvancementCommands.getAdvancements`: the advancement with its parents and/or descendants.
fn advancements(set: &Advancements, index: u32, mode: AdvancementMode) -> Vec<u32> {
    let in_tree = set.get(index).is_some_and(|a| a.in_tree);
    let (parents, children) = match mode {
        AdvancementMode::From => (false, true),
        AdvancementMode::Until => (true, false),
        AdvancementMode::Through => (true, true),
        _ => (false, false),
    };
    let mut out = Vec::new();
    if in_tree && parents {
        out.extend(set.parents(index));
    }
    out.push(index);
    if in_tree && children {
        set.descendants(index, &mut out);
    }
    out
}

/// `Action.perform`/`performCriterion` on one player of `level`: the advancements (or the one
/// criterion) that changed, then the rewards of the completed ones. `show` is false for
/// `grant everything`, which sends its changes without toasts.
pub(crate) fn perform_on(
    level: &mut Level,
    id: EntityId,
    action: Grant,
    list: &[u32],
    criterion: Option<u16>,
    show: bool,
) -> i32 {
    let Some(player) = level.entities.player_mut(id) else {
        return 0;
    };
    let sink = Arc::clone(&player.connection.sink);
    let advancements = &mut player.advancements;
    let flush = |advancements: &mut vanillite_game::advancements::PlayerAdvancements, show| {
        if let Some(packet) = advancements.flush(show) {
            sink.send(&packet);
        }
    };
    if !show {
        flush(advancements, true);
    }
    let mut count = 0i32;
    for &index in list {
        let changed = match (criterion, action) {
            (Some(c), Grant::Grant) => advancements.award(index, c),
            (Some(c), Grant::Revoke) => advancements.revoke(index, c),
            (None, action) => advancements.perform(action, index),
        };
        count = count.saturating_add(i32::from(changed));
    }
    if !show {
        flush(advancements, false);
    }
    count
}

/// `/advancement (grant|revoke) <targets> (only|from|until|through|everything) ...`.
pub(crate) fn advancement(
    grant: bool,
    mode: AdvancementMode,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let set = env.global.systems.advancements();
    let selector = args.selector("targets").ok_or_else(|| missing("targets"))?;
    let players = select::many(env, source, selector)?;
    let action = if grant { Grant::Grant } else { Grant::Revoke };
    let (list, criterion) = if mode == AdvancementMode::Everything {
        (set.iter().map(|(i, _)| i).collect(), None)
    } else {
        let id = args
            .identifier("advancement")
            .ok_or_else(|| missing("advancement"))?;
        let index = set.index(id).ok_or_else(|| {
            CommandError::new(
                "advancement.advancementNotFound",
                vec![ErrArg::Str(id.clone())],
            )
        })?;
        let a = set.get(index).ok_or_else(|| missing("advancement"))?;
        let criterion = if mode == AdvancementMode::Criterion {
            let name = args
                .string("criterion")
                .ok_or_else(|| missing("criterion"))?;
            let c = a.criterion(name).ok_or_else(|| {
                CommandError::new(
                    "commands.advancement.criterionNotFound",
                    vec![
                        ErrArg::Component(Box::new(a.name.clone())),
                        ErrArg::Str(name.clone()),
                    ],
                )
            })?;
            Some((c, name.clone()))
        } else {
            None
        };
        (advancements(&set, index, mode), criterion)
    };
    let show = !(grant && mode == AdvancementMode::Everything);
    let criterion_index = criterion.as_ref().map(|(c, _)| *c);
    let mut tracker = Tracker::<Component>::default();
    for player in &players {
        let count = match player.entity {
            Some(id) => {
                let count = perform_on(env.level, id, action, &list, criterion_index, show);
                crate::progress::grant_rewards(env, id);
                count
            }
            None => {
                let (uuid, targets) = (player.uuid, list.clone());
                let global = env.global.arc();
                env.global
                    .run_in_world(player.world, move |w: &mut ServerWorld| {
                        let world = w.id();
                        let level = w.level_mut();
                        let (Some(id), Some(global)) =
                            (select::find_uuid(&level.entities, uuid), global)
                        else {
                            return;
                        };
                        perform_on(level, id, action, &targets, criterion_index, show);
                        let mut env = Env {
                            level,
                            global: &global,
                            world,
                            others: Vec::new(),
                        };
                        crate::progress::grant_rewards(&mut env, id);
                    });
                if criterion_index.is_some() {
                    1
                } else {
                    i32::try_from(list.len()).unwrap_or(i32::MAX)
                }
            }
        };
        tracker.track(&player.display(env.global), count);
    }
    // The triggers the rewards fired (unlocked recipes) run at once, like Vanilla's.
    let global = env.global;
    crate::progress::settle(env, super::entities_cmd::dimension_of(global, env.world));
    // The messages name the criterion and/or the advancement (one) or count them (many).
    let only = (list.len() == 1)
        .then(|| list.first().and_then(|&i| set.get(i)))
        .flatten()
        .map(|a| a.name.clone());
    let (what, mut err_args, mut args) = match (criterion, only) {
        (Some((_, name)), Some(a)) => (
            "criterion",
            vec![
                ErrArg::Str(name.clone()),
                ErrArg::Component(Box::new(a.clone())),
            ],
            vec![Arg::String(name), Arg::from(a)],
        ),
        (_, Some(a)) => (
            "one",
            vec![ErrArg::Component(Box::new(a.clone()))],
            vec![Arg::from(a)],
        ),
        _ => {
            let count = i32::try_from(list.len()).unwrap_or(i32::MAX);
            ("many", vec![ErrArg::Int(count)], vec![Arg::Int(count)])
        }
    };
    if tracker.total() == 0 {
        let (one, n) = tracker.pick(true);
        err_args.push(match one {
            Some(player) => ErrArg::Component(Box::new(player.clone())),
            None => ErrArg::Int(n),
        });
        return Err(CommandError::new(
            feedback_key(grant, what, one.is_some(), false),
            err_args,
        ));
    }
    tracker.feedback(source, env, true, false, None, |one, n, _| {
        args.push(match one {
            Some(player) => Arg::from(player.clone()),
            None => Arg::Int(n),
        });
        Component::translatable_args(feedback_key(grant, what, one.is_some(), true), args)
    })
}

/// `commands.advancement.<grant|revoke>.<one|many|criterion>.to.<one|many>.<success|failure>`.
fn feedback_key(grant: bool, what: &str, one_player: bool, success: bool) -> &'static str {
    macro_rules! keys {
        ($action:literal) => {
            match (what, one_player, success) {
                ("criterion", true, true) => concat!(
                    "commands.advancement.",
                    $action,
                    ".criterion.to.one.success"
                ),
                ("criterion", true, false) => concat!(
                    "commands.advancement.",
                    $action,
                    ".criterion.to.one.failure"
                ),
                ("criterion", false, true) => concat!(
                    "commands.advancement.",
                    $action,
                    ".criterion.to.many.success"
                ),
                ("criterion", false, false) => concat!(
                    "commands.advancement.",
                    $action,
                    ".criterion.to.many.failure"
                ),
                ("one", true, true) => {
                    concat!("commands.advancement.", $action, ".one.to.one.success")
                }
                ("one", true, false) => {
                    concat!("commands.advancement.", $action, ".one.to.one.failure")
                }
                ("one", false, true) => {
                    concat!("commands.advancement.", $action, ".one.to.many.success")
                }
                ("one", false, false) => {
                    concat!("commands.advancement.", $action, ".one.to.many.failure")
                }
                (_, true, true) => {
                    concat!("commands.advancement.", $action, ".many.to.one.success")
                }
                (_, true, false) => {
                    concat!("commands.advancement.", $action, ".many.to.one.failure")
                }
                (_, false, true) => {
                    concat!("commands.advancement.", $action, ".many.to.many.success")
                }
                (_, false, false) => {
                    concat!("commands.advancement.", $action, ".many.to.many.failure")
                }
            }
        };
    }
    if grant {
        keys!("grant")
    } else {
        keys!("revoke")
    }
}
