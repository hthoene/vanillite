//! `/bossbar` (Vanilla's `BossBarCommands`) on the server's command boss bars.

use crate::player_list::Uuid;
use vanillite_command::Args;
use vanillite_command::error::{Arg as ErrArg, CommandError};
use vanillite_command::game_values::GameValue;
use vanillite_game::commands::BossbarOp;
use vanillite_protocol::text::{Arg, Component};

use super::Env;
use super::exec::format_list;
use super::select;
use super::source::CommandSource;
use crate::bossbars::{self, CustomBossEvent};
use crate::global::Global;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

fn err(key: &'static str) -> CommandError {
    CommandError::new(key, vec![])
}

fn tr(key: &str, args: Vec<Arg>) -> Component {
    Component::translatable_args(key, args)
}

/// `getPlayers` as display names (with the team formatting).
fn player_names(global: &Global, uuids: &[u128]) -> Vec<Component> {
    let online = global.players.snapshot();
    uuids
        .iter()
        .filter_map(|&u| {
            let p = online.iter().find(|p| p.uuid == Uuid(u))?;
            Some(crate::scoreboard::display_name(global, &p.name, u))
        })
        .collect()
}

/// `ComponentArgument.getResolvedComponent` of `name`.
fn name_arg(env: &Env<'_>, source: &CommandSource, args: &Args) -> Result<Component, CommandError> {
    match args.game("name") {
        Some(GameValue::Component(c)) => super::text::resolve(env, source, c, None),
        _ => Err(missing("name")),
    }
}

/// Runs a `bossbar` subcommand.
pub(crate) fn run(
    op: BossbarOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    match op {
        BossbarOp::List => {
            let bars = global.systems.bossbars();
            let message = if bars.len() == 0 {
                tr("commands.bossbar.list.bars.none", vec![])
            } else {
                let names = bars.events().map(CustomBossEvent::display_name).collect();
                tr(
                    "commands.bossbar.list.bars.some",
                    vec![
                        Arg::Int(i32::try_from(bars.len()).unwrap_or(i32::MAX)),
                        Arg::from(format_list(names)),
                    ],
                )
            };
            let count = bars.len();
            drop(bars);
            source.send_success(global, message, false);
            return Ok(i32::try_from(count).unwrap_or(i32::MAX));
        }
        BossbarOp::Add => {
            let id = args.identifier("id").ok_or_else(|| missing("id"))?.clone();
            if global.systems.bossbar_exists(&id) {
                return Err(CommandError::new(
                    "commands.bossbar.create.failed",
                    vec![ErrArg::Str(id)],
                ));
            }
            let name = name_arg(env, source, args)?;
            // `Mth.createInsecureUUID(source.getLevel().getRandom())`.
            let uuid = vanillite_game::mob::random_uuid(env.level.random());
            let (display, count) = {
                let mut bars = global.systems.bossbars();
                let display = bars.create(uuid, id, name).display_name();
                (display, bars.len())
            };
            source.send_success(
                global,
                tr("commands.bossbar.create.success", vec![Arg::from(display)]),
                true,
            );
            return Ok(i32::try_from(count).unwrap_or(i32::MAX));
        }
        _ => {}
    }
    let id = args.identifier("id").ok_or_else(|| missing("id"))?.clone();
    let unknown = || CommandError::new("commands.bossbar.unknown", vec![ErrArg::Str(id.clone())]);
    // The players of `set players` resolve before the bar is locked.
    let targets = if op == BossbarOp::SetPlayers {
        match args.selector("targets") {
            Some(s) => select::many(env, source, s)?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let name = if op == BossbarOp::SetName {
        Some(name_arg(env, source, args)?)
    } else {
        None
    };
    let mut out = Vec::new();
    let result = {
        let mut bars = global.systems.bossbars();
        let bar = bars.get(&id).cloned().ok_or_else(unknown)?;
        let display = bar.display_name();
        let d = || Arg::from(display.clone());
        match op {
            BossbarOp::Remove => {
                bars.remove(&id, &mut out);
                let n = i32::try_from(bars.len()).unwrap_or(i32::MAX);
                Ok((tr("commands.bossbar.remove.success", vec![d()]), n))
            }
            BossbarOp::GetValue => Ok((
                tr(
                    "commands.bossbar.get.value",
                    vec![d(), Arg::Int(bar.value())],
                ),
                bar.value(),
            )),
            BossbarOp::GetMax => Ok((
                tr("commands.bossbar.get.max", vec![d(), Arg::Int(bar.max())]),
                bar.max(),
            )),
            BossbarOp::GetVisible => Ok(if bar.visible() {
                (tr("commands.bossbar.get.visible.visible", vec![d()]), 1)
            } else {
                (tr("commands.bossbar.get.visible.hidden", vec![d()]), 0)
            }),
            BossbarOp::GetPlayers => {
                let n = i32::try_from(bar.shown().len()).unwrap_or(i32::MAX);
                Ok(if n == 0 {
                    (tr("commands.bossbar.get.players.none", vec![d()]), 0)
                } else {
                    let names = player_names(global, bar.shown());
                    (
                        tr(
                            "commands.bossbar.get.players.some",
                            vec![d(), Arg::Int(n), Arg::from(format_list(names))],
                        ),
                        n,
                    )
                })
            }
            _ => {
                let Some(bar) = bars.get_mut(&id) else {
                    return Err(unknown());
                };
                set(op, args, bar, name, &targets, global, &mut out)
            }
        }
    };
    bossbars::send(global, out);
    let (message, value) = result?;
    source.send_success(global, message, true);
    Ok(value)
}

/// The `set` subcommands on a bar; the message and the result.
fn set(
    op: BossbarOp,
    args: &Args,
    bar: &mut CustomBossEvent,
    name: Option<Component>,
    targets: &[select::Target],
    global: &Global,
    out: &mut bossbars::Outbox,
) -> Result<(Component, i32), CommandError> {
    match op {
        BossbarOp::SetName => {
            let name = name.ok_or_else(|| missing("name"))?;
            if *bar.name() == name {
                return Err(err("commands.bossbar.set.name.unchanged"));
            }
            bar.set_name(name, out);
            Ok((
                tr(
                    "commands.bossbar.set.name.success",
                    vec![Arg::from(bar.display_name())],
                ),
                0,
            ))
        }
        BossbarOp::SetColor(id) => {
            let color = bossbars::color_of(id);
            if bar.color() == color {
                return Err(err("commands.bossbar.set.color.unchanged"));
            }
            bar.set_color(color, out);
            Ok((
                tr(
                    "commands.bossbar.set.color.success",
                    vec![Arg::from(bar.display_name())],
                ),
                0,
            ))
        }
        BossbarOp::SetStyle(id) => {
            let overlay = bossbars::overlay_of(id);
            if bar.overlay() == overlay {
                return Err(err("commands.bossbar.set.style.unchanged"));
            }
            bar.set_overlay(overlay, out);
            Ok((
                tr(
                    "commands.bossbar.set.style.success",
                    vec![Arg::from(bar.display_name())],
                ),
                0,
            ))
        }
        BossbarOp::SetValue => {
            let value = *args.int("value").ok_or_else(|| missing("value"))?;
            if bar.value() == value {
                return Err(err("commands.bossbar.set.value.unchanged"));
            }
            bar.set_value(value, out);
            Ok((
                tr(
                    "commands.bossbar.set.value.success",
                    vec![Arg::from(bar.display_name()), Arg::Int(value)],
                ),
                value,
            ))
        }
        BossbarOp::SetMax => {
            let max = *args.int("max").ok_or_else(|| missing("max"))?;
            if bar.max() == max {
                return Err(err("commands.bossbar.set.max.unchanged"));
            }
            bar.set_max(max, out);
            Ok((
                tr(
                    "commands.bossbar.set.max.success",
                    vec![Arg::from(bar.display_name()), Arg::Int(max)],
                ),
                max,
            ))
        }
        BossbarOp::SetVisible => {
            let visible = *args.bool("visible").ok_or_else(|| missing("visible"))?;
            if bar.visible() == visible {
                return Err(err(if visible {
                    "commands.bossbar.set.visibility.unchanged.visible"
                } else {
                    "commands.bossbar.set.visibility.unchanged.hidden"
                }));
            }
            bar.set_visible(visible, out);
            let key = if visible {
                "commands.bossbar.set.visible.success.visible"
            } else {
                "commands.bossbar.set.visible.success.hidden"
            };
            Ok((tr(key, vec![Arg::from(bar.display_name())]), 0))
        }
        BossbarOp::SetPlayers => {
            let uuids: Vec<u128> = targets.iter().map(|t| t.uuid).collect();
            if !bar.set_players(&uuids, out) {
                return Err(err("commands.bossbar.set.players.unchanged"));
            }
            let n = i32::try_from(bar.shown().len()).unwrap_or(i32::MAX);
            if n == 0 {
                return Ok((
                    tr(
                        "commands.bossbar.set.players.success.none",
                        vec![Arg::from(bar.display_name())],
                    ),
                    0,
                ));
            }
            let names = targets
                .iter()
                .map(|t| crate::scoreboard::display_name(global, &t.name, t.uuid))
                .collect();
            Ok((
                tr(
                    "commands.bossbar.set.players.success.some",
                    vec![
                        Arg::from(bar.display_name()),
                        Arg::Int(i32::try_from(targets.len()).unwrap_or(i32::MAX)),
                        Arg::from(format_list(names)),
                    ],
                ),
                n,
            ))
        }
        _ => Err(CommandError::new("command.unknown.command", vec![])),
    }
}
