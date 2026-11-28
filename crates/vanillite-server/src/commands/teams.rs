//! `/team` (Vanilla's `TeamCommand`) and the team packets of `ServerScoreboard`; the changes
//! go out with [`flush`] right away.

use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, Args, GameValue};
use vanillite_game::commands::TeamOp;
use vanillite_game::scoreboard::{COLOR_NAMES, CollisionRule, Team, Visibility};
use vanillite_protocol::packets::play::clientbound::{SetPlayerTeam, TeamAction, TeamParameters};
use vanillite_protocol::text::{Arg, Color, Component, HoverEvent, NamedColor};

use super::Env;
use super::scores::{self, Holder};
use super::source::CommandSource;
use super::tracker::Tracker;
use crate::scoreboard::flush;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

/// `ClientboundSetPlayerTeamPacket.Parameters`.
pub(crate) fn parameters(team: &Team) -> Box<TeamParameters> {
    Box::new(TeamParameters {
        display_name: team.display_name.clone(),
        prefix: team.prefix.clone(),
        suffix: team.suffix.clone(),
        nametag_visibility: team.nametag_visibility.id(),
        collision_rule: team.collision_rule.id(),
        color: team.color.map(i32::from),
        options: team.pack_options(),
    })
}

/// `createAddOrModifyPacket(team, true)`.
pub(crate) fn add_packet(team: &Team) -> SetPlayerTeam {
    SetPlayerTeam {
        name: team.name.clone(),
        action: TeamAction::Add(parameters(team), team.players.clone()),
    }
}

/// Java's `String.hashCode` spread like `HashMap.hash`.
fn java_hash(s: &str) -> u32 {
    let h = s
        .encode_utf16()
        .fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(u32::from(c)));
    h ^ (h >> 16)
}

/// The iteration order of a Java `HashMap` with these string keys, inserted in this order:
/// by bucket of the smallest default-grown table that holds them, then insertion order.
pub(crate) fn java_hash_order<T>(items: Vec<T>, key: impl Fn(&T) -> &str) -> Vec<T> {
    let mut capacity: usize = 16;
    while items.len() > capacity / 4 * 3 {
        capacity = capacity.saturating_mul(2);
    }
    let mask = u32::try_from(capacity.saturating_sub(1)).unwrap_or(u32::MAX);
    let mut keyed: Vec<(u32, T)> = items
        .into_iter()
        .map(|t| (java_hash(key(&t)) & mask, t))
        .collect();
    keyed.sort_by_key(|(bucket, _)| *bucket);
    keyed.into_iter().map(|(_, t)| t).collect()
}

/// `TeamArgument.getTeam`.
fn team_arg(env: &Env<'_>, args: &Args) -> Result<Team, CommandError> {
    let name = args.string("team").ok_or_else(|| missing("team"))?;
    env.global
        .systems
        .scoreboard()
        .team(name)
        .cloned()
        .ok_or_else(|| CommandError::new("team.notFound", vec![ErrArg::Str(name.clone())]))
}

/// `ComponentArgument.getResolvedComponent`.
fn component(
    env: &Env<'_>,
    source: &CommandSource,
    args: &Args,
    name: &str,
) -> Result<Component, CommandError> {
    match args.game(name) {
        Some(GameValue::Component(c)) => super::text::resolve(env, source, c, None),
        _ => Err(missing(name)),
    }
}

fn t(key: &str, args: Vec<Arg>) -> Component {
    Component::translatable_args(key, args)
}

fn c(component: Component) -> Arg {
    Arg::from(component)
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// Changes a team, sends the change and reads it back for the feedback.
fn modify(env: &Env<'_>, name: &str, f: impl FnOnce(&mut Team)) -> Option<Team> {
    let team = {
        let mut board = env.global.systems.scoreboard();
        board.modify_team(name, f);
        board.team(name).cloned()
    };
    flush(env.global);
    team
}

/// `/team`.
pub(crate) fn run(
    op: TeamOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    match op {
        TeamOp::List => {
            let board = global.systems.scoreboard();
            let teams = java_hash_order(board.teams().collect(), |t| t.name.as_str());
            let n = count(teams.len());
            let message = if teams.is_empty() {
                t("commands.team.list.teams.empty", vec![])
            } else {
                t(
                    "commands.team.list.teams.success",
                    vec![
                        Arg::Int(n),
                        c(super::exec::format_list(
                            teams.iter().map(|t| t.formatted_display_name()).collect(),
                        )),
                    ],
                )
            };
            drop(board);
            source.send_success(global, message, false);
            Ok(n)
        }
        TeamOp::ListMembers => {
            let team = team_arg(env, args)?;
            let n = count(team.players.len());
            let message = if team.players.is_empty() {
                t(
                    "commands.team.list.members.empty",
                    vec![c(team.formatted_display_name())],
                )
            } else {
                // `ComponentUtils.formatList(Collection<String>)`: sorted, green.
                let mut names = team.players.clone();
                names.sort();
                let names = names
                    .iter()
                    .map(|n| Component::text(n.as_str()).color(Color::Named(NamedColor::Green)))
                    .collect();
                t(
                    "commands.team.list.members.success",
                    vec![
                        c(team.formatted_display_name()),
                        Arg::Int(n),
                        c(super::exec::format_list(names)),
                    ],
                )
            };
            source.send_success(global, message, false);
            Ok(n)
        }
        TeamOp::Add => {
            let name = args.string("team").ok_or_else(|| missing("team"))?;
            let display = if args.has("displayName") {
                component(env, source, args, "displayName")?
            } else {
                Component::text(name.as_str())
            };
            let (team, n) = {
                let mut board = global.systems.scoreboard();
                if !board.add_team(name) {
                    return Err(CommandError::new("commands.team.add.duplicate", vec![]));
                }
                board.modify_team(name, |t| t.display_name = display);
                (board.team(name).cloned(), board.teams().count())
            };
            flush(global);
            if let Some(team) = team {
                source.send_success(
                    global,
                    t(
                        "commands.team.add.success",
                        vec![c(team.formatted_display_name())],
                    ),
                    true,
                );
            }
            Ok(count(n))
        }
        TeamOp::Remove => {
            let team = team_arg(env, args)?;
            let n = {
                let mut board = global.systems.scoreboard();
                board.remove_team(&team.name);
                board.teams().count()
            };
            flush(global);
            source.send_success(
                global,
                t(
                    "commands.team.remove.success",
                    vec![c(team.formatted_display_name())],
                ),
                true,
            );
            Ok(count(n))
        }
        TeamOp::Empty => {
            let team = team_arg(env, args)?;
            if team.players.is_empty() {
                return Err(CommandError::new("commands.team.empty.unchanged", vec![]));
            }
            {
                let mut board = global.systems.scoreboard();
                for member in &team.players {
                    board.leave_team(member);
                }
            }
            flush(global);
            let n = count(team.players.len());
            source.send_success(
                global,
                t(
                    "commands.team.empty.success",
                    vec![Arg::Int(n), c(team.formatted_display_name())],
                ),
                true,
            );
            Ok(n)
        }
        TeamOp::JoinSelf | TeamOp::Join => {
            let team = team_arg(env, args)?;
            let members = if op == TeamOp::JoinSelf {
                vec![own_holder(source)?]
            } else {
                scores::holders(args, "members", source, env, true)?
            };
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = global.systems.scoreboard();
                for member in &members {
                    let joined = board.join_team(&member.name, &team.name);
                    tracker.track(member, i32::from(joined));
                }
            }
            flush(global);
            let team = team.formatted_display_name();
            tracker.feedback(source, env, true, false, None, |one, n, _| match one {
                Some(h) => t(
                    "commands.team.join.success.single",
                    vec![c(h.display.clone()), c(team)],
                ),
                None => t(
                    "commands.team.join.success.multiple",
                    vec![Arg::Int(n), c(team)],
                ),
            })
        }
        TeamOp::Leave => {
            let members = scores::holders(args, "members", source, env, true)?;
            let mut tracker = Tracker::<Holder>::default();
            {
                let mut board = global.systems.scoreboard();
                for member in &members {
                    let left = board.leave_team(&member.name);
                    tracker.track(member, i32::from(left));
                }
            }
            flush(global);
            tracker.feedback(source, env, true, false, None, |one, n, _| match one {
                Some(h) => t(
                    "commands.team.leave.success.single",
                    vec![c(h.display.clone())],
                ),
                None => t("commands.team.leave.success.multiple", vec![Arg::Int(n)]),
            })
        }
        _ => modify_option(op, args, source, env),
    }
}

/// The source entity as a score holder (`getEntityOrException`).
fn own_holder(source: &CommandSource) -> Result<Holder, CommandError> {
    let e = source
        .entity
        .as_ref()
        .ok_or_else(|| CommandError::new("permissions.requires.entity", vec![]))?;
    let name = if e.is_player() {
        e.name.clone()
    } else {
        vanillite_protocol::types::Uuid(e.uuid).to_string()
    };
    Ok(Holder {
        display: e
            .display
            .clone()
            .hover(HoverEvent::ShowText(Component::text(name.as_str()))),
        score_display: Some(e.display.clone()),
        name,
    })
}

/// A visibility or collision rule from its id.
fn visibility(id: u8) -> Visibility {
    Visibility::ALL
        .into_iter()
        .find(|v| v.id() == i32::from(id))
        .unwrap_or_default()
}

fn collision(id: u8) -> CollisionRule {
    CollisionRule::ALL
        .into_iter()
        .find(|v| v.id() == i32::from(id))
        .unwrap_or_default()
}

/// `team modify <team> ...`.
fn modify_option(
    op: TeamOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    let team = team_arg(env, args)?;
    let fail = |key: &'static str| Err(CommandError::new(key, vec![]));
    let success = |team: &Option<Team>, key: &str, mut extra: Vec<Arg>| {
        if let Some(team) = team {
            let mut args = vec![c(team.formatted_display_name())];
            args.append(&mut extra);
            source.send_success(global, t(key, args), true);
        }
    };
    match op {
        TeamOp::DisplayName => {
            let name = component(env, source, args, "displayName")?;
            if team.display_name == name {
                return fail("commands.team.option.name.unchanged");
            }
            let changed = modify(env, &team.name, |t| t.display_name = name);
            success(&changed, "commands.team.option.name.success", vec![]);
            Ok(0)
        }
        TeamOp::Color => {
            let Some(GameValue::TeamColor(color)) = args.game("value") else {
                return Err(missing("value"));
            };
            let color = *color;
            if team.color == Some(color) {
                return fail("commands.team.option.color.unchanged");
            }
            let changed = modify(env, &team.name, |t| t.color = Some(color));
            let name = COLOR_NAMES
                .get(usize::from(color))
                .copied()
                .unwrap_or("white");
            success(
                &changed,
                "commands.team.option.color.success",
                vec![Arg::String(name.to_owned())],
            );
            Ok(0)
        }
        TeamOp::ColorReset => {
            if team.color.is_none() {
                return fail("commands.team.option.color.unchanged");
            }
            let changed = modify(env, &team.name, |t| t.color = None);
            success(&changed, "commands.team.option.color.clear.success", vec![]);
            Ok(0)
        }
        TeamOp::FriendlyFire => {
            let allowed = *args.bool("allowed").ok_or_else(|| missing("allowed"))?;
            if team.allow_friendly_fire == allowed {
                return fail(if allowed {
                    "commands.team.option.friendlyfire.alreadyEnabled"
                } else {
                    "commands.team.option.friendlyfire.alreadyDisabled"
                });
            }
            let changed = modify(env, &team.name, |t| t.allow_friendly_fire = allowed);
            let key = if allowed {
                "commands.team.option.friendlyfire.enabled"
            } else {
                "commands.team.option.friendlyfire.disabled"
            };
            success(&changed, key, vec![]);
            Ok(0)
        }
        TeamOp::SeeFriendlyInvisibles => {
            let allowed = *args.bool("allowed").ok_or_else(|| missing("allowed"))?;
            if team.see_friendly_invisibles == allowed {
                return fail(if allowed {
                    "commands.team.option.seeFriendlyInvisibles.alreadyEnabled"
                } else {
                    "commands.team.option.seeFriendlyInvisibles.alreadyDisabled"
                });
            }
            let changed = modify(env, &team.name, |t| t.see_friendly_invisibles = allowed);
            let key = if allowed {
                "commands.team.option.seeFriendlyInvisibles.enabled"
            } else {
                "commands.team.option.seeFriendlyInvisibles.disabled"
            };
            success(&changed, key, vec![]);
            Ok(0)
        }
        TeamOp::NametagVisibility(id) | TeamOp::DeathMessageVisibility(id) => {
            let value = visibility(id);
            let nametag = matches!(op, TeamOp::NametagVisibility(_));
            let (current, unchanged, key) = if nametag {
                (
                    team.nametag_visibility,
                    "commands.team.option.nametagVisibility.unchanged",
                    "commands.team.option.nametagVisibility.success",
                )
            } else {
                (
                    team.death_message_visibility,
                    "commands.team.option.deathMessageVisibility.unchanged",
                    "commands.team.option.deathMessageVisibility.success",
                )
            };
            if current == value {
                return fail(unchanged);
            }
            let changed = modify(env, &team.name, |t| {
                if nametag {
                    t.nametag_visibility = value;
                } else {
                    t.death_message_visibility = value;
                }
            });
            let name = Component::translatable(format!("team.visibility.{}", value.name()), vec![]);
            success(&changed, key, vec![c(name)]);
            Ok(0)
        }
        TeamOp::CollisionRule(id) => {
            let value = collision(id);
            if team.collision_rule == value {
                return fail("commands.team.option.collisionRule.unchanged");
            }
            let changed = modify(env, &team.name, |t| t.collision_rule = value);
            let name = Component::translatable(format!("team.collision.{}", value.name()), vec![]);
            success(
                &changed,
                "commands.team.option.collisionRule.success",
                vec![c(name)],
            );
            Ok(0)
        }
        TeamOp::Prefix | TeamOp::Suffix => {
            let prefix = op == TeamOp::Prefix;
            let name = if prefix { "prefix" } else { "suffix" };
            let value = component(env, source, args, name)?;
            modify(env, &team.name, |t| {
                if prefix {
                    t.prefix = value.clone();
                } else {
                    t.suffix = value.clone();
                }
            });
            let key = if prefix {
                "commands.team.option.prefix.success"
            } else {
                "commands.team.option.suffix.success"
            };
            source.send_success(global, t(key, vec![c(value)]), false);
            Ok(1)
        }
        _ => Err(CommandError::new("command.unknown.command", vec![])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_hash_order_of_strings() {
        // "Aa" and "BB" share Java's hash; insertion order decides within a bucket.
        assert_eq!(java_hash("Aa"), java_hash("BB"));
        let order = java_hash_order(vec!["b", "a", "BB", "Aa"], |s| s);
        assert_eq!(order, ["BB", "Aa", "a", "b"]);
    }
}
