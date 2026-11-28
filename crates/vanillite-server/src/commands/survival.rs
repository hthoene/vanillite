//! The survival commands: `time` on the world clocks, `weather`, `gamerule` and `kill`.

use vanillite_command::error::{Arg as ErrArg, CommandError};
use vanillite_game::survival::clock::MoveResult;
use vanillite_game::survival::rules::{Rule, RuleType};
use vanillite_game::survival::weather::{RAIN_DELAY, RAIN_DURATION, THUNDER_DURATION, sample};
use vanillite_protocol::text::{Arg, Component};

use super::Env;
use super::select::Target;
use super::source::CommandSource;
use crate::global::Global;
use crate::survival::{broadcast_all, clock_update, full_time_sync};

fn error(key: &'static str, args: Vec<ErrArg>) -> CommandError {
    CommandError::new(key, args)
}

/// `TimeCommand.wrapTime`.
fn wrap_time(ticks: i64) -> i32 {
    i32::try_from(ticks % i64::from(i32::MAX)).unwrap_or(0)
}

/// The clock a `time` command works on: `of <clock>` or the default clock of the source's
/// dimension (`getDefaultClock`).
pub(crate) fn clock(
    source: &CommandSource,
    global: &Global,
    named: Option<&str>,
) -> Result<(usize, String), CommandError> {
    let level = &global.level;
    match named {
        Some(name) => {
            let id = if name.contains(':') {
                name.to_owned()
            } else {
                format!("minecraft:{name}")
            };
            level
                .clocks()
                .index(&id)
                .map(|i| (i, id.clone()))
                .ok_or_else(|| {
                    error(
                        "argument.resource.not_found",
                        vec![
                            ErrArg::Str(id),
                            ErrArg::Str("minecraft:world_clock".to_owned()),
                        ],
                    )
                })
        }
        None => {
            let dimension = global.dimensions.name_or_overworld(source.world);
            let clock = global.dimensions.default_clock(source.world);
            match clock {
                Some(i) => {
                    let id = level
                        .clocks()
                        .clocks
                        .get(i)
                        .map(|c| c.id.clone())
                        .unwrap_or_default();
                    Ok((i, id))
                }
                None => Err(error(
                    "commands.time.no_default_clock",
                    vec![ErrArg::Str(dimension.to_owned())],
                )),
            }
        }
    }
}

/// Sends the changed clock to all players (`modifyClock`).
pub(crate) fn clock_changed(global: &Global, clock: usize) {
    broadcast_all(global, &clock_update(&global.level, clock));
}

/// `time set <time>`.
pub(crate) fn time_set(
    source: &CommandSource,
    global: &Global,
    (clock, name): (usize, String),
    ticks: i32,
) -> Result<i32, CommandError> {
    {
        let mut clocks = global.level.clocks();
        let current = clocks.clocks.get(clock).map_or(0, |c| c.total_ticks);
        if current == i64::from(ticks) {
            return Err(error(
                "commands.time.set.already_at_time",
                vec![ErrArg::Str(name), ErrArg::Int(ticks)],
            ));
        }
        clocks.set_total_ticks(clock, i64::from(ticks));
    }
    clock_changed(global, clock);
    source.send_success(
        global,
        Component::translatable_args(
            "commands.time.set.absolute",
            vec![Arg::String(name), Arg::Long(i64::from(ticks))],
        ),
        true,
    );
    Ok(ticks)
}

/// `time add <time>`.
pub(crate) fn time_add(
    source: &CommandSource,
    global: &Global,
    (clock, name): (usize, String),
    ticks: i32,
) -> i32 {
    let total = {
        let mut clocks = global.level.clocks();
        clocks.add_ticks(clock, ticks);
        clocks.clocks.get(clock).map_or(0, |c| c.total_ticks)
    };
    clock_changed(global, clock);
    source.send_success(
        global,
        Component::translatable_args(
            "commands.time.set.absolute",
            vec![Arg::String(name), Arg::Long(total)],
        ),
        true,
    );
    wrap_time(total)
}

/// `time set <timemarker>`.
pub(crate) fn time_set_marker(
    source: &CommandSource,
    global: &Global,
    (clock, name): (usize, String),
    marker: &str,
) -> Result<i32, CommandError> {
    let marker = if marker.contains(':') {
        marker.to_owned()
    } else {
        format!("minecraft:{marker}")
    };
    let (result, total) = {
        let mut clocks = global.level.clocks();
        let result = clocks.move_to_marker(clock, &marker);
        (
            result,
            clocks.clocks.get(clock).map_or(0, |c| c.total_ticks),
        )
    };
    match result {
        MoveResult::NoTimeMarkerFound => Err(error(
            "commands.time.no_time_marker_found",
            vec![ErrArg::Str(marker), ErrArg::Str(name)],
        )),
        MoveResult::NotMoved => Err(error(
            "commands.time.set.already_at_time_marker",
            vec![ErrArg::Str(name), ErrArg::Str(marker)],
        )),
        MoveResult::Moved => {
            clock_changed(global, clock);
            source.send_success(
                global,
                Component::translatable_args(
                    "commands.time.set.time_marker",
                    vec![Arg::String(name), Arg::String(marker)],
                ),
                true,
            );
            Ok(wrap_time(total))
        }
    }
}

/// `time pause` and `time resume`.
pub(crate) fn time_pause(
    source: &CommandSource,
    global: &Global,
    (clock, name): (usize, String),
    paused: bool,
) -> Result<i32, CommandError> {
    {
        let mut clocks = global.level.clocks();
        let Some(c) = clocks.clocks.get_mut(clock) else {
            return Ok(0);
        };
        if c.paused == paused {
            let key = if paused {
                "commands.time.pause.already_paused"
            } else {
                "commands.time.pause.already_running"
            };
            return Err(error(key, vec![ErrArg::Str(name)]));
        }
        c.paused = paused;
    }
    clock_changed(global, clock);
    let key = if paused {
        "commands.time.pause"
    } else {
        "commands.time.resume"
    };
    source.send_success(
        global,
        Component::translatable_args(key, vec![Arg::String(name)]),
        true,
    );
    Ok(1)
}

/// `time rate <rate>`.
pub(crate) fn time_rate(
    source: &CommandSource,
    global: &Global,
    (clock, name): (usize, String),
    rate: f32,
) -> Result<i32, CommandError> {
    {
        let mut clocks = global.level.clocks();
        let Some(c) = clocks.clocks.get_mut(clock) else {
            return Ok(0);
        };
        if c.rate == rate {
            return Err(error(
                "commands.time.rate.already_same",
                vec![ErrArg::Str(name), ErrArg::Float(rate)],
            ));
        }
        c.rate = rate;
    }
    clock_changed(global, clock);
    source.send_success(
        global,
        Component::translatable_args(
            "commands.time.rate",
            vec![Arg::String(name), Arg::Float(rate)],
        ),
        true,
    );
    Ok(1)
}

/// `time query time`.
pub(crate) fn time_query(
    source: &CommandSource,
    global: &Global,
    (clock, name): (usize, String),
) -> i32 {
    let total = global
        .level
        .clocks()
        .clocks
        .get(clock)
        .map_or(0, |c| c.total_ticks);
    source.send_success(
        global,
        Component::translatable_args(
            "commands.time.query.absolute",
            vec![Arg::String(name), Arg::Long(total)],
        ),
        false,
    );
    wrap_time(total)
}

/// `time query <timeline> [repetition]`.
pub(crate) fn time_timeline(
    source: &CommandSource,
    global: &Global,
    (clock, name): (usize, String),
    timeline: &str,
    repetition: bool,
) -> Result<i32, CommandError> {
    let timeline = if timeline.contains(':') {
        timeline.to_owned()
    } else {
        format!("minecraft:{timeline}")
    };
    let value = {
        let clocks = global.level.clocks();
        let Some(info) = clocks.timeline(&timeline) else {
            return Err(error(
                "argument.resource.not_found",
                vec![
                    ErrArg::Str(timeline),
                    ErrArg::Str("minecraft:timeline".to_owned()),
                ],
            ));
        };
        if info.clock != clock {
            return Err(error(
                "commands.time.wrong_timeline_for_clock",
                vec![ErrArg::Str(timeline), ErrArg::Str(name)],
            ));
        }
        let total = clocks.clocks.get(clock).map_or(0, |c| c.total_ticks);
        if repetition {
            info.period_count(total)
        } else {
            info.current_ticks(total)
        }
    };
    let key = if repetition {
        "commands.time.query.timeline.repetitions"
    } else {
        "commands.time.query.timeline"
    };
    source.send_success(
        global,
        Component::translatable_args(key, vec![Arg::String(timeline), Arg::Long(value)]),
        false,
    );
    Ok(wrap_time(value))
}

/// The kinds of `weather`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Weather {
    Clear,
    Rain,
    Thunder,
}

/// `weather clear|rain|thunder [<duration>]` (`setWeatherParameters`).
pub(crate) fn weather(
    source: &CommandSource,
    env: &mut super::Env<'_>,
    kind: Weather,
    duration: Option<i32>,
) -> i32 {
    let global = env.global;
    let key = match kind {
        Weather::Clear => "commands.weather.set.clear",
        Weather::Rain => "commands.weather.set.rain",
        Weather::Thunder => "commands.weather.set.thunder",
    };
    // `getDuration`: the default distribution draws from the source level's random.
    let range = match kind {
        Weather::Clear => RAIN_DELAY,
        Weather::Rain => RAIN_DURATION,
        Weather::Thunder => THUNDER_DURATION,
    };
    let ticks =
        duration.or_else(|| (source.world == env.world).then(|| sample(range, env.level.random())));
    set_weather(global, kind, ticks);
    source.send_success(global, Component::translatable(key, vec![]), true);
    duration.unwrap_or(-1)
}

/// `setWeatherParameters` as `/weather` sets them: `None` samples Vanilla's duration.
pub(crate) fn set_weather(global: &Global, kind: Weather, duration: Option<i32>) {
    let mut random =
        vanillite_base::random::LegacyRandom::new(global.level.game_time() ^ 0x5DEE_CE66);
    let mut sampled = |range| duration.unwrap_or_else(|| sample(range, &mut random));
    let mut data = global.level.weather();
    match kind {
        Weather::Clear => data.set(sampled(RAIN_DELAY), 0, false, false),
        Weather::Rain => data.set(0, sampled(RAIN_DURATION), true, false),
        Weather::Thunder => data.set(0, sampled(THUNDER_DURATION), true, true),
    }
}

/// `gamerule <rule>`.
pub(crate) fn game_rule_query(source: &CommandSource, global: &Global, rule: Rule) -> i32 {
    let value = global.level.rules().get(rule);
    let info = rule.info();
    source.send_success(
        global,
        Component::translatable_args(
            "commands.gamerule.query",
            vec![
                Arg::String(info.name.to_owned()),
                Arg::String(rule.serialize(value)),
            ],
        ),
        false,
    );
    value
}

/// `gamerule <rule> <value>` and the rule's side effects (`onGameRuleChanged`).
pub(crate) fn game_rule_set(
    source: &CommandSource,
    global: &Global,
    rule: Rule,
    value: i32,
) -> Result<i32, CommandError> {
    let info = rule.info();
    let value = match info.kind {
        RuleType::Bool => i32::from(value != 0),
        RuleType::Int { .. } => value,
    };
    {
        let mut rules = global.level.rules_mut();
        if rules.get(rule) == value {
            return Err(error(
                "commands.gamerule.not_set",
                vec![
                    ErrArg::Str(info.name.to_owned()),
                    ErrArg::Str(rule.serialize(value)),
                ],
            ));
        }
        rules.set(rule, value);
    }
    on_game_rule_changed(global, rule, value);
    source.send_success(
        global,
        Component::translatable_args(
            "commands.gamerule.set",
            vec![
                Arg::String(info.name.to_owned()),
                Arg::String(rule.serialize(value)),
            ],
        ),
        true,
    );
    Ok(value)
}

/// `MinecraftServer.onGameRuleChanged`: what the clients learn of a changed rule.
pub(crate) fn on_game_rule_changed(global: &Global, rule: Rule, value: i32) {
    if rule == Rule::REDUCED_DEBUG_INFO {
        // Entity event 22 or 23 to every player about itself.
        let event: i8 = if value != 0 { 22 } else { 23 };
        for index in global.dimensions.worlds().map(|w| w.0) {
            global.run_in_world(crate::world::WorldId(index), move |w| {
                let entities = w.entities();
                for (id, player) in entities.players.iter() {
                    if let Some(e) = entities.store.get(id) {
                        player.connection.sink.send(
                            &vanillite_protocol::packets::play::clientbound::EntityEvent {
                                entity_id: e.network_id().0,
                                event,
                            },
                        );
                    }
                }
            });
        }
    } else if rule == Rule::LIMITED_CRAFTING || rule == Rule::IMMEDIATE_RESPAWN {
        let event = if rule == Rule::LIMITED_CRAFTING {
            vanillite_game::survival::weather::event::LIMITED_CRAFTING
        } else {
            vanillite_game::survival::weather::event::IMMEDIATE_RESPAWN
        };
        broadcast_all(
            global,
            &vanillite_protocol::packets::play::clientbound::GameEvent {
                event,
                param: if value != 0 { 1.0 } else { 0.0 },
            },
        );
    } else if rule == Rule::ADVANCE_TIME {
        broadcast_all(global, &full_time_sync(&global.level));
    }
}

/// `kill [<targets>]`: `kill` on every victim; players of other worlds die between ticks.
pub(crate) fn kill(source: &CommandSource, env: &mut Env<'_>, victims: &[Target]) -> i32 {
    let global = env.global;
    for victim in victims {
        match victim.entity {
            Some(id) if victim.world == env.world => {
                let dimension = global.dimensions.name_or_overworld(victim.world);
                super::entities_cmd::kill_any(env.level, global, dimension, id);
            }
            _ => {
                let uuid = victim.uuid;
                super::entities_cmd::on_level(
                    env,
                    victim.world,
                    move |level, global, dimension| {
                        if let Some(id) = super::select::find_uuid(&level.entities, uuid) {
                            super::entities_cmd::kill_any(level, global, dimension, id);
                        }
                    },
                );
            }
        }
    }
    let message = match victims {
        [one] => Component::translatable_args(
            "commands.kill.success.single",
            vec![Arg::Component(Box::new(
                vanillite_game::player::join::display_name(&one.name, one.uuid),
            ))],
        ),
        _ => Component::translatable_args(
            "commands.kill.success.multiple",
            vec![Arg::Int(i32::try_from(victims.len()).unwrap_or(i32::MAX))],
        ),
    };
    source.send_success(global, message, true);
    i32::try_from(victims.len()).unwrap_or(i32::MAX)
}
