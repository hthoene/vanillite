//! Score holders, objectives and `/scoreboard` (Vanilla's `ScoreHolderArgument`,
//! `ObjectiveArgument`, `ScoreboardCommand` with its `CommandResponseTracker` feedback).

use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, Args, GameValue, ScoreHolderArg, ScoreOperation};
use vanillite_game::commands::ScoreboardOp;
use vanillite_game::scoreboard::{CriteriaKind, NumberFormat, Objective, RenderType};
use vanillite_protocol::text::{Arg, Component, HoverEvent};

use super::Env;
use super::exec::format_list;
use super::select;
use super::source::CommandSource;
use super::tracker::Tracker;

/// A resolved score holder (`ScoreHolder`).
#[derive(Clone, Debug)]
pub(crate) struct Holder {
    /// `getScoreboardName`.
    pub(crate) name: String,
    /// `getFeedbackDisplayName`.
    pub(crate) display: Component,
    /// `getDisplayName`: what a `displayAutoUpdate` objective shows (none for a plain name).
    pub(crate) score_display: Option<Component>,
}

impl Holder {
    fn name_only(name: &str) -> Holder {
        Holder {
            name: name.to_owned(),
            display: Component::text(name),
            score_display: None,
        }
    }

    fn of(target: &select::Target) -> Holder {
        let name = target.score_name();
        let shown = target.display();
        Holder {
            display: shown
                .clone()
                .hover(HoverEvent::ShowText(Component::text(name.as_str()))),
            score_display: Some(shown),
            name,
        }
    }
}

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

/// `ScoreHolderArgument.getNames`, with the tracked holders for `*` when `wildcard`.
pub(crate) fn holders(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
    wildcard: bool,
) -> Result<Vec<Holder>, CommandError> {
    let Some(GameValue::ScoreHolder(arg)) = args.game(name) else {
        return Err(missing(name));
    };
    let found = match arg {
        ScoreHolderArg::Selector(selector) => select::optional(env, source, selector)?
            .iter()
            .map(Holder::of)
            .collect(),
        ScoreHolderArg::Wildcard => {
            let tracked: Vec<Holder> = if wildcard {
                env.global
                    .systems
                    .scoreboard()
                    .holders()
                    .map(Holder::name_only)
                    .collect()
            } else {
                Vec::new()
            };
            if tracked.is_empty() {
                return Err(CommandError::new("argument.scoreHolder.empty", vec![]));
            }
            tracked
        }
        ScoreHolderArg::Name(text) => vec![named(env, source, text)],
    };
    if found.is_empty() {
        return Err(CommandError::new("argument.entity.notfound.entity", vec![]));
    }
    Ok(found)
}

/// A plain name: `#fake` stays a name, a UUID finds the entity, a name finds the online player.
fn named(env: &Env<'_>, source: &CommandSource, text: &str) -> Holder {
    if text.starts_with('#') {
        return Holder::name_only(text);
    }
    if let Some(uuid) = vanillite_protocol::types::Uuid::parse(text) {
        if let Some(t) = select::find_uuid(&env.level.entities, uuid.0)
            .and_then(|id| select::local(&env.level.entities, env.world, id))
        {
            return Holder::of(&t);
        }
        return Holder::name_only(text);
    }
    select::online_players(&env.level.entities, env.global, source.world)
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(text))
        .map_or_else(|| Holder::name_only(text), Holder::of)
}

/// `ScoreHolderArgument.getName`: the first holder's name.
pub(crate) fn single_holder(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<String, CommandError> {
    holders(args, name, source, env, false)?
        .into_iter()
        .next()
        .map(|h| h.name)
        .ok_or_else(|| CommandError::new("argument.entity.notfound.entity", vec![]))
}

/// `ObjectiveArgument.getObjective`.
pub(crate) fn objective(env: &Env<'_>, args: &Args, name: &str) -> Result<String, CommandError> {
    let id = args.string(name).ok_or_else(|| missing(name))?;
    if env.global.systems.scoreboard().objective(id).is_none() {
        return Err(CommandError::new(
            "arguments.objective.notFound",
            vec![ErrArg::Str(id.clone())],
        ));
    }
    Ok(id.clone())
}

/// `ObjectiveArgument.getWritableObjective`.
fn writable(env: &Env<'_>, args: &Args, name: &str) -> Result<String, CommandError> {
    let id = objective(env, args, name)?;
    let read_only = env
        .global
        .systems
        .scoreboard()
        .objective(&id)
        .is_some_and(|o| o.criteria.is_read_only());
    if read_only {
        return Err(CommandError::new(
            "arguments.objective.readonly",
            vec![ErrArg::Str(id)],
        ));
    }
    Ok(id)
}

fn t(key: &str, args: Vec<Arg>) -> Component {
    Component::translatable_args(key, args)
}

fn c(component: Component) -> Arg {
    Arg::from(component)
}

/// `Objective.getFormattedDisplayName` of an existing objective.
fn formatted(env: &Env<'_>, objective: &str) -> Component {
    env.global
        .systems
        .scoreboard()
        .objective(objective)
        .map_or_else(
            || Component::text(objective),
            Objective::formatted_display_name,
        )
}

fn number_format(op: ScoreboardOp, args: &Args) -> Option<NumberFormat> {
    use ScoreboardOp as S;
    match op {
        S::ObjectiveFormatBlank | S::PlayerFormatBlank => Some(NumberFormat::Blank),
        S::ObjectiveFormatFixed | S::PlayerFormatFixed => match args.game("contents") {
            Some(GameValue::Component(c)) => Some(NumberFormat::Fixed(c.clone())),
            _ => None,
        },
        S::ObjectiveFormatStyled | S::PlayerFormatStyled => match args.game("style") {
            Some(GameValue::Style(s)) => Some(NumberFormat::Styled(s.clone())),
            _ => None,
        },
        _ => None,
    }
}

fn component(args: &Args, name: &str) -> Option<Component> {
    match args.game(name) {
        Some(GameValue::Component(c)) => Some((**c).clone()),
        _ => None,
    }
}

/// `DisplaySlot.getSerializedName`.
fn slot_name(slot: vanillite_command::DisplaySlot) -> String {
    use vanillite_command::DisplaySlot as D;
    match slot {
        D::List => "list".into(),
        D::Sidebar => "sidebar".into(),
        D::BelowName => "below_name".into(),
        D::SidebarTeam(color) => format!(
            "sidebar.team.{}",
            vanillite_game::scoreboard::COLOR_NAMES
                .get(usize::from(color))
                .copied()
                .unwrap_or("white")
        ),
    }
}

/// `Mth.floorDiv` and `Mth.positiveModulo` of `/=` and `%=`.
fn operate(op: ScoreOperation, a: i32, b: i32) -> Result<(i32, i32), CommandError> {
    let div0 = || CommandError::new("arguments.operation.div0", vec![]);
    Ok(match op {
        ScoreOperation::Assign => (b, b),
        ScoreOperation::Add => (a.wrapping_add(b), b),
        ScoreOperation::Sub => (a.wrapping_sub(b), b),
        ScoreOperation::Mul => (a.wrapping_mul(b), b),
        ScoreOperation::Div => {
            if b == 0 {
                return Err(div0());
            }
            let q = a.wrapping_div(b);
            let q = if (a.wrapping_rem(b) != 0) && ((a < 0) != (b < 0)) {
                q.wrapping_sub(1)
            } else {
                q
            };
            (q, b)
        }
        ScoreOperation::Mod => {
            if b == 0 {
                return Err(div0());
            }
            let r = a.wrapping_rem(b);
            let r = if r != 0 && ((r < 0) != (b < 0)) {
                r.wrapping_add(b)
            } else {
                r
            };
            (r, b)
        }
        ScoreOperation::Min => (a.min(b), b),
        ScoreOperation::Max => (a.max(b), b),
        ScoreOperation::Swap => (b, a),
    })
}

/// Runs a `/scoreboard` subcommand.
#[allow(clippy::too_many_lines)]
pub(crate) fn run(
    op: ScoreboardOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    use ScoreboardOp as S;
    let systems = &env.global.systems;
    match op {
        S::ObjectivesList => {
            let board = systems.scoreboard();
            let list: Vec<Component> = board
                .objectives()
                .map(Objective::formatted_display_name)
                .collect();
            drop(board);
            let n = i32::try_from(list.len()).unwrap_or(i32::MAX);
            let message = if list.is_empty() {
                t("commands.scoreboard.objectives.list.empty", vec![])
            } else {
                t(
                    "commands.scoreboard.objectives.list.success",
                    vec![Arg::Int(n), c(format_list(list))],
                )
            };
            source.send_success(env.global, message, false);
            Ok(n)
        }
        S::ObjectivesAdd => {
            let name = args
                .string("objective")
                .ok_or_else(|| missing("objective"))?;
            let criteria_name = args.string("criteria").ok_or_else(|| missing("criteria"))?;
            let criteria = CriteriaKind::by_name(criteria_name).ok_or_else(|| {
                CommandError::new(
                    "argument.criteria.invalid",
                    vec![ErrArg::Str(criteria_name.clone())],
                )
            })?;
            let display =
                component(args, "displayName").unwrap_or_else(|| Component::text(name.as_str()));
            let mut board = systems.scoreboard();
            if board.objective(name).is_some() {
                return Err(CommandError::new(
                    "commands.scoreboard.objectives.add.duplicate",
                    vec![],
                ));
            }
            let objective = Objective {
                name: name.clone(),
                render_type: criteria.default_render_type(),
                criteria,
                display_name: display,
                display_auto_update: false,
                number_format: None,
            };
            let formatted = objective.formatted_display_name();
            board.add_objective(objective);
            let n = i32::try_from(board.objectives().count()).unwrap_or(i32::MAX);
            drop(board);
            source.send_success(
                env.global,
                t(
                    "commands.scoreboard.objectives.add.success",
                    vec![c(formatted)],
                ),
                true,
            );
            Ok(n)
        }
        S::ObjectivesRemove => {
            let name = objective(env, args, "objective")?;
            let formatted = formatted(env, &name);
            let mut board = systems.scoreboard();
            board.remove_objective(&name);
            let n = i32::try_from(board.objectives().count()).unwrap_or(i32::MAX);
            drop(board);
            source.send_success(
                env.global,
                t(
                    "commands.scoreboard.objectives.remove.success",
                    vec![c(formatted)],
                ),
                true,
            );
            Ok(n)
        }
        S::ObjectiveDisplayName => {
            let name = objective(env, args, "objective")?;
            let display = component(args, "displayName").ok_or_else(|| missing("displayName"))?;
            let changed = {
                let mut board = systems.scoreboard();
                let same = board
                    .objective(&name)
                    .is_some_and(|o| o.display_name == display);
                !same && board.modify_objective(&name, |o| o.display_name = display)
            };
            if changed {
                source.send_success(
                    env.global,
                    t(
                        "commands.scoreboard.objectives.modify.displayname",
                        vec![Arg::String(name.clone()), c(formatted(env, &name))],
                    ),
                    true,
                );
            }
            Ok(0)
        }
        S::RenderInteger | S::RenderHearts => {
            let name = objective(env, args, "objective")?;
            let render = if op == S::RenderHearts {
                RenderType::Hearts
            } else {
                RenderType::Integer
            };
            let changed = {
                let mut board = systems.scoreboard();
                let same = board
                    .objective(&name)
                    .is_some_and(|o| o.render_type == render);
                !same && board.modify_objective(&name, |o| o.render_type = render)
            };
            if changed {
                source.send_success(
                    env.global,
                    t(
                        "commands.scoreboard.objectives.modify.rendertype",
                        vec![c(formatted(env, &name))],
                    ),
                    true,
                );
            }
            Ok(0)
        }
        S::DisplayAutoUpdate => {
            let name = objective(env, args, "objective")?;
            let value = args.bool("value").copied().unwrap_or(false);
            let changed = {
                let mut board = systems.scoreboard();
                let same = board
                    .objective(&name)
                    .is_some_and(|o| o.display_auto_update == value);
                !same && board.modify_objective(&name, |o| o.display_auto_update = value)
            };
            if changed {
                let key = if value {
                    "commands.scoreboard.objectives.modify.displayAutoUpdate.enable"
                } else {
                    "commands.scoreboard.objectives.modify.displayAutoUpdate.disable"
                };
                source.send_success(
                    env.global,
                    t(
                        key,
                        vec![Arg::String(name.clone()), c(formatted(env, &name))],
                    ),
                    true,
                );
            }
            Ok(0)
        }
        S::ObjectiveFormatClear
        | S::ObjectiveFormatBlank
        | S::ObjectiveFormatFixed
        | S::ObjectiveFormatStyled => {
            let name = objective(env, args, "objective")?;
            let format = number_format(op, args);
            let set = format.is_some();
            systems
                .scoreboard()
                .modify_objective(&name, |o| o.number_format = format);
            let key = if set {
                "commands.scoreboard.objectives.modify.objectiveFormat.set"
            } else {
                "commands.scoreboard.objectives.modify.objectiveFormat.clear"
            };
            source.send_success(env.global, t(key, vec![Arg::String(name)]), true);
            Ok(0)
        }
        S::SetDisplayClear | S::SetDisplay => {
            let Some(GameValue::DisplaySlot(slot)) = args.game("slot") else {
                return Err(missing("slot"));
            };
            let slot_id = u8::try_from(slot.id()).unwrap_or(0);
            if op == S::SetDisplayClear {
                let mut board = systems.scoreboard();
                if board.display(slot_id).is_none() {
                    return Err(CommandError::new(
                        "commands.scoreboard.objectives.display.alreadyEmpty",
                        vec![],
                    ));
                }
                board.set_display(slot_id, None);
                drop(board);
                source.send_success(
                    env.global,
                    t(
                        "commands.scoreboard.objectives.display.cleared",
                        vec![Arg::String(slot_name(*slot))],
                    ),
                    true,
                );
                return Ok(0);
            }
            let name = objective(env, args, "objective")?;
            let mut board = systems.scoreboard();
            if board.display(slot_id) == Some(name.as_str()) {
                return Err(CommandError::new(
                    "commands.scoreboard.objectives.display.alreadySet",
                    vec![],
                ));
            }
            board.set_display(slot_id, Some(&name));
            let display = board
                .objective(&name)
                .map(|o| o.display_name.clone())
                .unwrap_or_else(Component::empty);
            drop(board);
            source.send_success(
                env.global,
                t(
                    "commands.scoreboard.objectives.display.set",
                    vec![Arg::String(slot_name(*slot)), c(display)],
                ),
                true,
            );
            Ok(0)
        }
        S::PlayersList => {
            let holders: Vec<String> = systems.scoreboard().holders().map(str::to_owned).collect();
            let n = i32::try_from(holders.len()).unwrap_or(i32::MAX);
            let message = if holders.is_empty() {
                t("commands.scoreboard.players.list.empty", vec![])
            } else {
                t(
                    "commands.scoreboard.players.list.success",
                    vec![
                        Arg::Int(n),
                        c(format_list(
                            holders
                                .iter()
                                .map(|h| Component::text(h.as_str()))
                                .collect(),
                        )),
                    ],
                )
            };
            source.send_success(env.global, message, false);
            Ok(n)
        }
        S::PlayersListTarget => {
            let holder = holders(args, "target", source, env, false)?
                .into_iter()
                .next()
                .ok_or_else(|| CommandError::new("argument.entity.notfound.entity", vec![]))?;
            let board = systems.scoreboard();
            let entries: Vec<(Component, i32)> = board
                .scores_of(&holder.name)
                .into_iter()
                .flatten()
                .filter_map(|(o, s)| Some((board.objective(o)?.formatted_display_name(), s.value)))
                .collect();
            drop(board);
            let n = i32::try_from(entries.len()).unwrap_or(i32::MAX);
            if entries.is_empty() {
                source.send_success(
                    env.global,
                    t(
                        "commands.scoreboard.players.list.entity.empty",
                        vec![c(holder.display)],
                    ),
                    false,
                );
            } else {
                source.send_success(
                    env.global,
                    t(
                        "commands.scoreboard.players.list.entity.success",
                        vec![c(holder.display), Arg::Int(n)],
                    ),
                    false,
                );
                for (objective, value) in entries {
                    source.send_success(
                        env.global,
                        t(
                            "commands.scoreboard.players.list.entity.entry",
                            vec![c(objective), Arg::Int(value)],
                        ),
                        false,
                    );
                }
            }
            Ok(n)
        }
        S::Get => {
            let holder = holders(args, "target", source, env, false)?
                .into_iter()
                .next()
                .ok_or_else(|| CommandError::new("argument.entity.notfound.entity", vec![]))?;
            let name = objective(env, args, "objective")?;
            let value = systems
                .scoreboard()
                .score(&holder.name, &name)
                .map(|s| s.value);
            let Some(value) = value else {
                return Err(CommandError::new(
                    "commands.scoreboard.players.get.null",
                    vec![
                        ErrArg::Str(name),
                        ErrArg::Component(Box::new(holder.display)),
                    ],
                ));
            };
            source.send_success(
                env.global,
                t(
                    "commands.scoreboard.players.get.success",
                    vec![c(holder.display), Arg::Int(value), c(formatted(env, &name))],
                ),
                false,
            );
            Ok(value)
        }
        S::Set | S::Add | S::Remove => {
            let targets = holders(args, "targets", source, env, true)?;
            let name = writable(env, args, "objective")?;
            let amount = args.int("score").copied().unwrap_or(0);
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = systems.scoreboard();
                for holder in &targets {
                    let before = board.score(&holder.name, &name).map_or(0, |s| s.value);
                    let value = match op {
                        S::Set => amount,
                        S::Add => before.wrapping_add(amount),
                        _ => before.wrapping_sub(amount),
                    };
                    board.set_score(&holder.name, &name, value, holder.score_display.as_ref());
                    tracker.track(holder, value);
                }
            }
            let objective = formatted(env, &name);
            match op {
                S::Set => tracker.feedback(source, env, true, false, None, |one, n, _| match one {
                    Some(h) => t(
                        "commands.scoreboard.players.set.success.single",
                        vec![c(objective), c(h.display.clone()), Arg::Int(amount)],
                    ),
                    None => t(
                        "commands.scoreboard.players.set.success.multiple",
                        vec![c(objective), Arg::Int(n), Arg::Int(amount)],
                    ),
                }),
                _ => {
                    let key = if op == S::Add { "add" } else { "remove" };
                    tracker.feedback(source, env, true, true, None, |one, n, total| match one {
                        Some(h) => t(
                            &format!("commands.scoreboard.players.{key}.success.single"),
                            vec![
                                Arg::Int(amount),
                                c(objective),
                                c(h.display.clone()),
                                Arg::Int(total),
                            ],
                        ),
                        None => t(
                            &format!("commands.scoreboard.players.{key}.success.multiple"),
                            vec![Arg::Int(amount), c(objective), Arg::Int(n)],
                        ),
                    })
                }
            }
        }
        S::ResetAll => {
            let targets = holders(args, "targets", source, env, true)?;
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = systems.scoreboard();
                for holder in &targets {
                    board.reset_all(&holder.name);
                    tracker.track(holder, 1);
                }
            }
            tracker.feedback(source, env, true, false, None, |one, n, _| match one {
                Some(h) => t(
                    "commands.scoreboard.players.reset.all.single",
                    vec![c(h.display.clone())],
                ),
                None => t(
                    "commands.scoreboard.players.reset.all.multiple",
                    vec![Arg::Int(n)],
                ),
            })
        }
        S::Reset => {
            let targets = holders(args, "targets", source, env, true)?;
            let name = objective(env, args, "objective")?;
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = systems.scoreboard();
                for holder in &targets {
                    board.reset_score(&holder.name, &name);
                    tracker.track(holder, 1);
                }
            }
            let objective = formatted(env, &name);
            tracker.feedback(source, env, true, false, None, |one, n, _| match one {
                Some(h) => t(
                    "commands.scoreboard.players.reset.specific.single",
                    vec![c(objective), c(h.display.clone())],
                ),
                None => t(
                    "commands.scoreboard.players.reset.specific.multiple",
                    vec![c(objective), Arg::Int(n)],
                ),
            })
        }
        S::Enable => {
            let targets = holders(args, "targets", source, env, true)?;
            let name = objective(env, args, "objective")?;
            let is_trigger = systems
                .scoreboard()
                .objective(&name)
                .is_some_and(|o| o.criteria == CriteriaKind::Trigger);
            if !is_trigger {
                return Err(CommandError::new(
                    "commands.scoreboard.players.enable.invalid",
                    vec![],
                ));
            }
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = systems.scoreboard();
                for holder in &targets {
                    let locked = board.score(&holder.name, &name).is_none_or(|s| s.locked);
                    if locked {
                        board.set_locked(&holder.name, &name, false);
                        tracker.track(holder, 1);
                    }
                }
            }
            let objective = formatted(env, &name);
            tracker.feedback(
                source,
                env,
                true,
                false,
                Some(CommandError::new(
                    "commands.scoreboard.players.enable.failed",
                    vec![],
                )),
                |one, n, _| match one {
                    Some(h) => t(
                        "commands.scoreboard.players.enable.success.single",
                        vec![c(objective), c(h.display.clone())],
                    ),
                    None => t(
                        "commands.scoreboard.players.enable.success.multiple",
                        vec![c(objective), Arg::Int(n)],
                    ),
                },
            )
        }
        S::DisplayNameClear | S::DisplayName => {
            let targets = holders(args, "targets", source, env, true)?;
            let name = objective(env, args, "objective")?;
            let display = if op == S::DisplayName {
                component(args, "name")
            } else {
                None
            };
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = systems.scoreboard();
                for holder in &targets {
                    board.set_score_display(&holder.name, &name, display.clone());
                    tracker.track(holder, 1);
                }
            }
            let objective = formatted(env, &name);
            match display {
                None => tracker.feedback(source, env, true, false, None, |one, n, _| match one {
                    Some(h) => t(
                        "commands.scoreboard.players.display.name.clear.success.single",
                        vec![c(h.display.clone()), c(objective)],
                    ),
                    None => t(
                        "commands.scoreboard.players.display.name.clear.success.multiple",
                        vec![Arg::Int(n), c(objective)],
                    ),
                }),
                Some(d) => {
                    tracker.feedback(source, env, true, false, None, |one, n, _| match one {
                        Some(h) => t(
                            "commands.scoreboard.players.display.name.set.success.single",
                            vec![c(d), c(h.display.clone()), c(objective)],
                        ),
                        None => t(
                            "commands.scoreboard.players.display.name.set.success.multiple",
                            vec![c(d), Arg::Int(n), c(objective)],
                        ),
                    })
                }
            }
        }
        S::PlayerFormatClear
        | S::PlayerFormatBlank
        | S::PlayerFormatFixed
        | S::PlayerFormatStyled => {
            let targets = holders(args, "targets", source, env, true)?;
            let name = objective(env, args, "objective")?;
            let format = number_format(op, args);
            let set = format.is_some();
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = systems.scoreboard();
                for holder in &targets {
                    board.set_score_format(&holder.name, &name, format.clone());
                    tracker.track(holder, 1);
                }
            }
            let objective = formatted(env, &name);
            let kind = if set { "set" } else { "clear" };
            tracker.feedback(source, env, true, false, None, |one, n, _| match one {
                Some(h) => t(
                    &format!(
                        "commands.scoreboard.players.display.numberFormat.{kind}.success.single"
                    ),
                    vec![c(h.display.clone()), c(objective)],
                ),
                None => t(
                    &format!(
                        "commands.scoreboard.players.display.numberFormat.{kind}.success.multiple"
                    ),
                    vec![Arg::Int(n), c(objective)],
                ),
            })
        }
        S::Operation => {
            let targets = holders(args, "targets", source, env, true)?;
            let target_objective = writable(env, args, "targetObjective")?;
            let Some(GameValue::Operation(operation)) = args.game("operation") else {
                return Err(missing("operation"));
            };
            let sources = holders(args, "source", source, env, true)?;
            let source_objective = objective(env, args, "sourceObjective")?;
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = systems.scoreboard();
                for target in &targets {
                    board.touch_score(&target.name, &target_objective);
                    for from in &sources {
                        board.touch_score(&from.name, &source_objective);
                        let a = board
                            .score(&target.name, &target_objective)
                            .map_or(0, |s| s.value);
                        let b = board
                            .score(&from.name, &source_objective)
                            .map_or(0, |s| s.value);
                        let (new_a, new_b) = operate(*operation, a, b)?;
                        board.set_score(
                            &target.name,
                            &target_objective,
                            new_a,
                            target.score_display.as_ref(),
                        );
                        if *operation == ScoreOperation::Swap {
                            board.set_score(
                                &from.name,
                                &source_objective,
                                new_b,
                                from.score_display.as_ref(),
                            );
                        }
                    }
                    let value = board
                        .score(&target.name, &target_objective)
                        .map_or(0, |s| s.value);
                    tracker.track(target, value);
                }
            }
            let objective = formatted(env, &target_objective);
            tracker.feedback(source, env, true, true, None, |one, n, total| match one {
                Some(h) => t(
                    "commands.scoreboard.players.operation.success.single",
                    vec![c(objective), c(h.display.clone()), Arg::Int(total)],
                ),
                None => t(
                    "commands.scoreboard.players.operation.success.multiple",
                    vec![c(objective), Arg::Int(n)],
                ),
            })
        }
    }
}
