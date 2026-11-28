//! The system and admin commands: `random`, `fetchprofile`, `version`, `spectate`,
//! `stopwatch`, `test`, `waypoint`, `jfr`, `perf`, `debug start|stop`, `setidletimeout` and
//! `transfer` (Vanilla's classes of the same names).

use vanillite_base::random::RandomSource;
use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, GameValue, Parsed};
use vanillite_game::commands::{
    CommandId, FetchProfileOp, RandomOp, StopwatchOp, TestOp, WaypointOp,
};
use vanillite_game::scoreboard::{COLOR_NAMES, named_color};
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::packets::play::clientbound::{SetCamera, Transfer};
use vanillite_protocol::text::{Arg, ClickEvent, Color, Component, HoverEvent, NamedColor};

use super::Env;
use super::select::{self, Target};
use super::source::CommandSource;
use super::tracker::Tracker;
use crate::command_state::Measurement;
use crate::player_list::Uuid;

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

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// `CommandSourceStack.getPlayerOrException`: the source player as a target.
fn source_player(env: &Env<'_>, source: &CommandSource) -> Result<Target, CommandError> {
    let uuid = source
        .uuid()
        .ok_or_else(|| err("permissions.requires.player"))?;
    select::online_players(&env.level.entities, env.global, env.world)
        .into_iter()
        .find(|t| t.uuid == uuid)
        .ok_or_else(|| err("permissions.requires.player"))
}

fn single(
    env: &Env<'_>,
    source: &CommandSource,
    parsed: &Parsed<CommandId>,
    name: &str,
) -> Result<Target, CommandError> {
    let selector = parsed.args.selector(name).ok_or_else(|| missing(name))?;
    select::single(env, source, selector)
}

/// Runs a system command.
pub(crate) fn run(
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    match parsed.command {
        CommandId::Random(op) => random(op, parsed, source, env),
        CommandId::FetchProfile(op) => fetch_profile(op, parsed, source, env),
        CommandId::Version => Ok(version(source, env)),
        CommandId::Plugins => Ok(super::plugins_cmd::plugins(source, env.global)),
        CommandId::BukkitVersion => Ok(super::plugins_cmd::version(
            parsed.args.string("plugin").map(String::as_str),
            source,
            env.global,
        )),
        CommandId::Spectate => spectate(parsed, source, env),
        CommandId::Stopwatch(op) => stopwatch(op, parsed, source, env),
        CommandId::Test(op) => test(op, parsed, source, env),
        CommandId::Waypoint(op) => waypoint(op, parsed, source, env),
        CommandId::JfrStart => Err(err("commands.jfr.start.failed")),
        CommandId::JfrStop => Err(CommandError::new(
            "commands.jfr.dump.failed",
            vec![ErrArg::Str("Not currently profiling".into())],
        )),
        CommandId::PerfStart
        | CommandId::PerfStop
        | CommandId::DebugStart
        | CommandId::DebugStop => profile(parsed.command, source, env),
        CommandId::SetIdleTimeout => {
            let minutes = *parsed
                .args
                .int("minutes")
                .ok_or_else(|| missing("minutes"))?;
            env.global.access.set_idle_timeout(minutes);
            let message = if minutes > 0 {
                tr("commands.setidletimeout.success", vec![Arg::Int(minutes)])
            } else {
                tr("commands.setidletimeout.success.disabled", vec![])
            };
            source.send_success(env.global, message, true);
            Ok(minutes)
        }
        CommandId::Transfer => transfer(parsed, source, env),
        _ => Err(err("command.unknown.command")),
    }
}

/// `RandomCommand`.
fn random(
    op: RandomOp,
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    let args = &parsed.args;
    let seed_options = || {
        (
            args.int("seed").copied().unwrap_or(0),
            args.bool("includeWorldSeed").copied().unwrap_or(true),
            args.bool("includeSequenceId").copied().unwrap_or(true),
        )
    };
    match op {
        RandomOp::Value | RandomOp::Roll => {
            let range = args.int_range("range").ok_or_else(|| missing("range"))?;
            let min = range.min.unwrap_or(i32::MIN);
            let max = range.max.unwrap_or(i32::MAX);
            let span = i64::from(max).saturating_sub(i64::from(min));
            if span == 0 {
                return Err(err("commands.random.error.range_too_small"));
            }
            if span >= i64::from(i32::MAX) {
                return Err(err("commands.random.error.range_too_large"));
            }
            // `Mth.randomBetweenInclusive`: the span is in 1..i32::MAX here.
            let bound = i32::try_from(span.saturating_add(1)).unwrap_or(i32::MAX);
            let offset = match args.identifier("sequence") {
                Some(id) => global
                    .systems
                    .random_sequences()
                    .get(id)
                    .next_int_bounded(bound),
                None => env.level.random().next_int_bounded(bound),
            };
            let value = offset.wrapping_add(min);
            if op == RandomOp::Roll {
                global.players.broadcast_system(
                    tr(
                        "commands.random.roll",
                        vec![
                            Arg::from(source.display_name()),
                            Arg::Int(value),
                            Arg::Int(min),
                            Arg::Int(max),
                        ],
                    ),
                    false,
                    None,
                );
            } else {
                source.send_success(
                    global,
                    tr("commands.random.sample.success", vec![Arg::Int(value)]),
                    false,
                );
            }
            Ok(value)
        }
        RandomOp::Reset | RandomOp::ResetWithSeed => {
            let id = args
                .identifier("sequence")
                .ok_or_else(|| missing("sequence"))?;
            let options = (op == RandomOp::ResetWithSeed).then(seed_options);
            global.systems.random_sequences().reset(id, options);
            source.send_success(
                global,
                tr(
                    "commands.random.reset.success",
                    vec![Arg::String(id.clone())],
                ),
                false,
            );
            Ok(1)
        }
        RandomOp::ResetAll | RandomOp::ResetAllWithDefaults => {
            let n = {
                let mut sequences = global.systems.random_sequences();
                if op == RandomOp::ResetAllWithDefaults {
                    sequences.set_defaults(seed_options());
                }
                count(sequences.clear())
            };
            source.send_success(
                global,
                tr("commands.random.reset.all.success", vec![Arg::Int(n)]),
                false,
            );
            Ok(n)
        }
    }
}

/// The profile of an online player as `ResolvableProfile` NBT (`id`, `name`, `properties`).
fn profile_nbt(env: &Env<'_>, uuid: u128) -> Option<Tag> {
    let entry = env
        .global
        .players
        .snapshot()
        .into_iter()
        .find(|p| p.uuid.0 == uuid)?;
    let mut profile = Compound::default();
    profile.insert(
        "id",
        Tag::IntArray(vanillite_nbt::snbt::uuid_to_ints(uuid).to_vec()),
    );
    profile.insert("name", Tag::String(entry.name.clone()));
    let properties: Vec<Tag> = entry
        .info
        .profile
        .properties
        .iter()
        .map(|p| {
            let mut c = Compound::default();
            c.insert("name", Tag::String(p.name.clone()));
            c.insert("value", Tag::String(p.value.clone()));
            if let Some(s) = &p.signature {
                c.insert("signature", Tag::String(s.clone()));
            }
            Tag::Compound(c)
        })
        .collect();
    if !properties.is_empty() {
        profile.insert("properties", Tag::List(vanillite_nbt::List(properties)));
    }
    Some(Tag::Compound(profile))
}

/// `FetchProfileCommand.reportResolvedProfile`.
fn report_profile(source: &CommandSource, env: &Env<'_>, key: &str, arg: Component, profile: Tag) {
    use vanillite_protocol::text::{Content, ObjectInfo, OpaqueNbt};
    let encoded = vanillite_nbt::snbt::to_string(&profile);
    let head = Component::new(Content::Object {
        info: ObjectInfo::Player {
            player: OpaqueNbt(profile),
            hat: true,
        },
        fallback: None,
    });
    let encoded_head = vanillite_nbt::snbt::to_string(&head.to_nbt());
    let green = Color::Named(NamedColor::Green);
    let button =
        |c: Component| Component::translatable("chat.square_brackets", vec![c.color(green)]);
    let buttons = [
        Component::translatable("commands.fetchprofile.copy_component", vec![])
            .click(ClickEvent::CopyToClipboard(encoded.clone())),
        Component::translatable("commands.fetchprofile.give_item", vec![]).click(
            ClickEvent::RunCommand(format!("give @s minecraft:player_head[profile={encoded}]")),
        ),
        Component::translatable("commands.fetchprofile.summon_mannequin", vec![]).click(
            ClickEvent::RunCommand(format!(
                "summon minecraft:mannequin ~ ~ ~ {{profile:{encoded}}}"
            )),
        ),
        Component::translatable(
            "commands.fetchprofile.copy_text",
            vec![head.color(Color::Named(NamedColor::White))],
        )
        .click(ClickEvent::CopyToClipboard(encoded_head)),
    ];
    let mut clickable = Component::empty();
    for (i, b) in buttons.into_iter().enumerate() {
        if i > 0 {
            clickable = clickable.append(Component::text(" "));
        }
        clickable = clickable.append(button(b));
    }
    source.send_success(
        env.global,
        Component::translatable(key, vec![arg, clickable]),
        false,
    );
}

/// `FetchProfileCommand`: profiles of known (online) players; there is no profile service
/// lookup, so other names and ids are not found.
fn fetch_profile(
    op: FetchProfileOp,
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let players = env.global.players.snapshot();
    match op {
        FetchProfileOp::Name => {
            let name = parsed.args.string("name").ok_or_else(|| missing("name"))?;
            let arg = Component::text(name.as_str());
            let found = players
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name))
                .and_then(|p| profile_nbt(env, p.uuid.0));
            match found {
                Some(profile) => {
                    report_profile(
                        source,
                        env,
                        "commands.fetchprofile.name.success",
                        arg,
                        profile,
                    );
                }
                None => source.send_failure(
                    env.global,
                    Component::translatable("commands.fetchprofile.name.failure", vec![arg]),
                ),
            }
            Ok(1)
        }
        FetchProfileOp::Id => {
            let id = *parsed.args.uuid("id").ok_or_else(|| missing("id"))?;
            let arg = Component::text(vanillite_protocol::types::Uuid(id).to_string());
            match profile_nbt(env, id) {
                Some(profile) => {
                    report_profile(
                        source,
                        env,
                        "commands.fetchprofile.id.success",
                        arg,
                        profile,
                    );
                }
                None => source.send_failure(
                    env.global,
                    Component::translatable("commands.fetchprofile.id.failure", vec![arg]),
                ),
            }
            Ok(1)
        }
        FetchProfileOp::Entity => {
            let target = single(env, source, parsed, "entity")?;
            let profile = target
                .is_player()
                .then(|| profile_nbt(env, target.uuid))
                .flatten()
                .ok_or_else(|| {
                    CommandError::new(
                        "commands.fetchprofile.no_profile",
                        vec![ErrArg::Component(Box::new(target.display(env.global)))],
                    )
                })?;
            report_profile(
                source,
                env,
                "commands.fetchprofile.entity.success",
                target.display(env.global),
                profile,
            );
            Ok(1)
        }
    }
}

/// Java's `Date.toString` in UTC.
fn java_date(secs: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, mo, d, h, mi, s) = crate::access::utc(secs);
    let weekday = DAYS
        .get(usize::try_from((secs / 86_400) % 7).unwrap_or(0))
        .copied()
        .unwrap_or("Thu");
    let month = MONTHS
        .get(usize::from(mo.saturating_sub(1)))
        .copied()
        .unwrap_or("Jan");
    format!("{weekday} {month} {d:02} {h:02}:{mi:02}:{s:02} UTC {y}")
}

/// `VersionCommand.dumpVersion`; the build time is the server's start.
fn version(source: &CommandSource, env: &Env<'_>) -> i32 {
    use vanillite_data::version as v;
    let global = env.global;
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(global.clock.now() / 1_000_000_000);
    let lines = [
        tr("commands.version.header", vec![]),
        tr(
            "commands.version.id",
            vec![Arg::String(v::VERSION_ID.into())],
        ),
        tr(
            "commands.version.name",
            vec![Arg::String(v::VERSION_ID.into())],
        ),
        tr("commands.version.data", vec![Arg::Int(v::DATA_VERSION)]),
        tr("commands.version.series", vec![Arg::String("main".into())]),
        tr(
            "commands.version.protocol",
            vec![
                Arg::Int(v::PROTOCOL_VERSION),
                Arg::String(format!("0x{:x}", v::PROTOCOL_VERSION)),
            ],
        ),
        tr(
            "commands.version.build_time",
            vec![Arg::String(java_date(started))],
        ),
        tr(
            "commands.version.pack.resource",
            vec![Arg::String(format!(
                "{}.{}",
                v::RESOURCE_PACK_VERSION.0,
                v::RESOURCE_PACK_VERSION.1
            ))],
        ),
        tr(
            "commands.version.pack.data",
            vec![Arg::String(format!(
                "{}.{}",
                v::DATA_PACK_VERSION.0,
                v::DATA_PACK_VERSION.1
            ))],
        ),
        tr("commands.version.stable.yes", vec![]),
    ];
    for line in lines {
        source.send_system(global, line);
    }
    1
}

/// `SpectateCommand`: sets the spectator's camera.
fn spectate(
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let target = if parsed.args.has("target") {
        Some(single(env, source, parsed, "target")?)
    } else {
        None
    };
    let player = if parsed.args.has("player") {
        single(env, source, parsed, "player")?
    } else {
        source_player(env, source)?
    };
    if target.as_ref().is_some_and(|t| t.uuid == player.uuid) {
        return Err(err("commands.spectate.self"));
    }
    if player.game_mode != vanillite_config::GameMode::Spectator {
        return Err(CommandError::new(
            "commands.spectate.not_spectator",
            vec![ErrArg::Component(Box::new(player.display(env.global)))],
        ));
    }
    if let Some(t) = &target
        && t.kind.info().tracking_range == 0
    {
        return Err(CommandError::new(
            "commands.spectate.cannot_spectate",
            vec![ErrArg::Component(Box::new(t.display(env.global)))],
        ));
    }
    // `ServerPlayer.setCamera`: the spectator moves along with its camera.
    if let Some(id) = player.entity {
        let camera = target.as_ref().and_then(|t| t.entity);
        crate::spectator::set_camera(&mut env.level.entities, id, camera);
    } else {
        let camera = target.as_ref().map_or(player.entity, |t| t.entity);
        let network_id = camera
            .and_then(|id| env.level.entities.store.get(id))
            .map(|e| e.network_id().0);
        if let (Some(camera_id), Some((sink, _))) =
            (network_id, env.global.players.connection(Uuid(player.uuid)))
        {
            sink.send(&SetCamera { camera_id });
        }
    }
    let message = match &target {
        Some(t) => Component::translatable(
            "commands.spectate.success.started",
            vec![t.display(env.global)],
        ),
        None => Component::translatable("commands.spectate.success.stopped", vec![]),
    };
    source.send_success(env.global, message, false);
    Ok(1)
}

/// `StopwatchCommand`.
fn stopwatch(
    op: StopwatchOp,
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    let id = parsed.args.identifier("id").ok_or_else(|| missing("id"))?;
    let not_found = || {
        CommandError::new(
            "commands.stopwatch.does_not_exist",
            vec![ErrArg::Str(id.clone())],
        )
    };
    let id_arg = Arg::String(id.clone());
    match op {
        StopwatchOp::Create => {
            if !global.systems.stopwatches().add(id) {
                return Err(CommandError::new(
                    "commands.stopwatch.already_exists",
                    vec![ErrArg::Str(id.clone())],
                ));
            }
            source.send_success(
                global,
                tr("commands.stopwatch.create.success", vec![id_arg]),
                true,
            );
            Ok(1)
        }
        StopwatchOp::Query => {
            let elapsed = global.systems.stopwatch_seconds(id).ok_or_else(not_found)?;
            let scale = parsed.args.double("scale").copied().unwrap_or(1.0);
            source.send_success(
                global,
                tr(
                    "commands.stopwatch.query",
                    vec![id_arg, Arg::Double(elapsed)],
                ),
                true,
            );
            // Java's `(int)` cast: saturating, NaN is 0.
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            Ok((elapsed * scale) as i32)
        }
        StopwatchOp::Restart => {
            if !global.systems.stopwatches().restart(id) {
                return Err(not_found());
            }
            source.send_success(
                global,
                tr("commands.stopwatch.restart.success", vec![id_arg]),
                true,
            );
            Ok(1)
        }
        StopwatchOp::Remove => {
            if !global.systems.stopwatches().remove(id) {
                return Err(not_found());
            }
            source.send_success(
                global,
                tr("commands.stopwatch.remove.success", vec![id_arg]),
                true,
            );
            Ok(1)
        }
    }
}

/// `TestCommand`: there are no test instances or test blocks, so every run finds no tests.
fn test(
    op: TestOp,
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    use super::gametest::{self, Finder, Retry};
    let args = &parsed.args;
    let retry = match args.int("numberOfTimes") {
        Some(n) => Retry::new(*n, args.bool("untilFailed").copied().unwrap_or(false)),
        None => Retry::NONE,
    };
    let rotation = args.int("rotationSteps").copied().unwrap_or(0);
    let per_row = args
        .int("testsPerRow")
        .copied()
        .unwrap_or(gametest::DEFAULT_TESTS_PER_ROW);
    let selection = |copies| Finder::Selection {
        pattern: args.string("tests").cloned().unwrap_or_default(),
        copies,
    };
    match op {
        TestOp::Run => gametest::run(env, source, &selection(1), retry, rotation, per_row),
        TestOp::RunMultiple => {
            let copies = args
                .int("amount")
                .map_or(1, |n| usize::try_from(*n).unwrap_or(0));
            gametest::run(env, source, &selection(copies), Retry::NONE, 0, 8)
        }
        TestOp::RunThese => gametest::run(env, source, &Finder::AllNearby, retry, 0, 8),
        TestOp::RunClosest => gametest::run(env, source, &Finder::Nearest, retry, 0, 8),
        TestOp::RunThat => gametest::run(env, source, &Finder::LookedAt, retry, 0, 8),
        TestOp::RunFailed => {
            let only_required = args.bool("onlyRequiredTests").copied().unwrap_or(false);
            gametest::run(
                env,
                source,
                &Finder::Failed { only_required },
                retry,
                rotation,
                per_row,
            )
        }
        TestOp::Verify => gametest::verify(env, source, &selection(1)),
        TestOp::Locate => gametest::locate(env, source, &selection(1)),
        TestOp::ResetClosest => gametest::reset(env, source, &Finder::Nearest),
        TestOp::ResetThese => gametest::reset(env, source, &Finder::AllNearby),
        TestOp::ResetThat => gametest::reset(env, source, &Finder::LookedAt),
        TestOp::ClearThat => gametest::clear(env, source, &Finder::LookedAt),
        TestOp::ClearThese => gametest::clear(env, source, &Finder::AllNearby),
        TestOp::ClearAll => {
            let radius = args.int("radius").map_or(250, |r| (*r).clamp(0, 1024));
            gametest::clear(env, source, &Finder::Radius(radius))
        }
        TestOp::Stop => Ok(gametest::stop(env)),
        TestOp::Pos => {
            source_player(env, source)?;
            let var = args
                .string("var")
                .cloned()
                .unwrap_or_else(|| "pos".to_owned());
            gametest::pos(env, source, &var)
        }
        TestOp::Create => {
            let id = args.identifier("id").ok_or_else(|| missing("id"))?;
            let width = args.int("width").copied().unwrap_or(5);
            let height = args.int("height").copied().unwrap_or(width);
            let depth = args.int("depth").copied().unwrap_or(width);
            gametest::create(env, source, id, [width, height, depth])
        }
    }
}

/// The waypoints of a world: the players in it (living entities transmit by their
/// `waypoint_transmit_range`, which only players have by default).
fn waypoint(
    op: WaypointOp,
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    if op == WaypointOp::List {
        let dimension = global.dimensions.name_or_overworld(source.world);
        let transmitters: Vec<Target> =
            select::online_players(&env.level.entities, global, source.world)
                .into_iter()
                .filter(|t| t.world == source.world)
                .collect();
        if transmitters.is_empty() {
            source.send_success(
                global,
                tr(
                    "commands.waypoint.list.empty",
                    vec![Arg::String(dimension.into())],
                ),
                false,
            );
            return Ok(0);
        }
        let icons = global.systems.waypoints().clone();
        let names = transmitters
            .iter()
            .map(|t| {
                let pos = t.place.map_or(source.pos, |(p, _, _)| p);
                #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
                let (x, y, z) = (
                    pos.x.floor() as i32,
                    pos.y.floor() as i32,
                    pos.z.floor() as i32,
                );
                let color = icons
                    .get(&t.uuid)
                    .and_then(|i| i.color)
                    .map_or(0xFF_FFFF, |c| {
                        u32::from_ne_bytes(c.to_ne_bytes()) & 0xFF_FFFF
                    });
                t.display(env.global)
                    .click(ClickEvent::SuggestCommand(format!(
                        "/execute in {dimension} run tp @s {x} {y} {z}"
                    )))
                    .hover(HoverEvent::ShowText(Component::translatable(
                        "chat.coordinates.tooltip",
                        vec![],
                    )))
                    .color(Color::Rgb(color))
            })
            .collect();
        let n = count(transmitters.len());
        source.send_success(
            global,
            tr(
                "commands.waypoint.list.success",
                vec![
                    Arg::Int(n),
                    Arg::String(dimension.into()),
                    Arg::from(super::exec::format_list(names)),
                ],
            ),
            false,
        );
        return Ok(n);
    }
    // `WaypointArgument.getWaypoint`: a living entity.
    let target = single(env, source, parsed, "waypoint")?;
    if target.kind.info().attributes.is_empty() {
        return Err(err("argument.waypoint.invalid"));
    }
    let mut icons = global.systems.waypoints();
    let icon = icons.entry(target.uuid).or_default();
    let message = match op {
        WaypointOp::Color => {
            let Some(GameValue::TeamColor(color)) = parsed.args.game("color") else {
                return Err(missing("color"));
            };
            let name = COLOR_NAMES
                .get(usize::from(*color))
                .copied()
                .unwrap_or("white");
            let named = named_color(*color).unwrap_or(NamedColor::White);
            icon.color = TEAM_RGB.get(usize::from(*color)).copied();
            tr(
                "commands.waypoint.modify.color",
                vec![Arg::from(Component::text(name).color(Color::Named(named)))],
            )
        }
        WaypointOp::ColorHex => {
            let Some(GameValue::HexColor(color)) = parsed.args.game("color") else {
                return Err(missing("color"));
            };
            let rgb = u32::from_ne_bytes(color.to_ne_bytes()) & 0xFF_FFFF;
            icon.color = Some(*color);
            tr(
                "commands.waypoint.modify.color",
                vec![Arg::from(
                    Component::text(format!("{rgb:06X}")).color(Color::Rgb(rgb)),
                )],
            )
        }
        WaypointOp::ColorReset => {
            icon.color = None;
            tr("commands.waypoint.modify.color.reset", vec![])
        }
        WaypointOp::StyleReset | WaypointOp::StyleSet => {
            icon.style = parsed.args.identifier("style").cloned();
            tr("commands.waypoint.modify.style", vec![])
        }
        WaypointOp::List => return Err(err("command.unknown.command")),
    };
    drop(icons);
    source.send_success(global, message, false);
    Ok(0)
}

/// `TeamColor.rgb` by colour id.
const TEAM_RGB: [i32; 16] = [
    0x00_0000, 0x00_00AA, 0x00_AA00, 0x00_AAAA, 0xAA_0000, 0xAA_00AA, 0xFF_AA00, 0xAA_AAAA,
    0x55_5555, 0x55_55FF, 0x55_FF55, 0x55_FFFF, 0xFF_5555, 0xFF_55FF, 0xFF_FF55, 0xFF_FFFF,
];

/// `PerfCommand` and `DebugCommand` start/stop: the duration and ticks between them.
fn profile(
    command: CommandId,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    let debug = matches!(command, CommandId::DebugStart | CommandId::DebugStop);
    let start = matches!(command, CommandId::DebugStart | CommandId::PerfStart);
    let now = Measurement {
        nanos: global.clock.now(),
        game_time: global.level.game_time(),
    };
    let previous = {
        let mut profiling = global.systems.profiling();
        let slot = if debug {
            &mut profiling.debug
        } else {
            &mut profiling.perf
        };
        match (start, slot.is_some()) {
            (true, true) => {
                return Err(err(if debug {
                    "commands.debug.alreadyRunning"
                } else {
                    "commands.perf.alreadyRunning"
                }));
            }
            (false, false) => {
                return Err(err(if debug {
                    "commands.debug.notRunning"
                } else {
                    "commands.perf.notRunning"
                }));
            }
            (true, false) => {
                *slot = Some(now);
                None
            }
            (false, true) => slot.take(),
        }
    };
    let Some(started) = previous else {
        let key = if debug {
            "commands.debug.started"
        } else {
            "commands.perf.started"
        };
        source.send_success(global, tr(key, vec![]), debug);
        return Ok(0);
    };
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    let seconds = now.nanos.saturating_sub(started.nanos) as f64 / 1e9;
    let ticks = i32::try_from(now.game_time.saturating_sub(started.game_time)).unwrap_or(i32::MAX);
    let tps = f64::from(ticks) / seconds;
    let key = if debug {
        "commands.debug.stopped"
    } else {
        "commands.perf.stopped"
    };
    source.send_success(
        global,
        tr(
            key,
            vec![
                Arg::String(format!("{seconds:.2}")),
                Arg::Int(ticks),
                Arg::String(format!("{tps:.2}")),
            ],
        ),
        debug,
    );
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    Ok(if debug { tps as i32 } else { 0 })
}

/// `TransferCommand`.
fn transfer(
    parsed: &Parsed<CommandId>,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let host = parsed
        .args
        .string("hostname")
        .ok_or_else(|| missing("hostname"))?;
    let port = parsed.args.int("port").copied().unwrap_or(25565);
    let players = match parsed.args.selector("players") {
        Some(selector) => select::many(env, source, selector)?,
        None => vec![source_player(env, source)?],
    };
    let mut tracker = Tracker::<Target>::default();
    for player in &players {
        if let Some((sink, _)) = env.global.players.connection(Uuid(player.uuid)) {
            sink.send(&Transfer {
                host: host.clone(),
                port,
            });
        }
        tracker.track(player, 1);
    }
    let host = Arg::String(host.clone());
    tracker.feedback(
        source,
        env,
        true,
        false,
        Some(CommandError::new(
            "commands.transfer.error.no_players",
            vec![],
        )),
        |one, n, _| match one {
            Some(p) => tr(
                "commands.transfer.success.single",
                vec![Arg::from(p.display(env.global)), host, Arg::Int(port)],
            ),
            None => tr(
                "commands.transfer.success.multiple",
                vec![Arg::Int(n), host, Arg::Int(port)],
            ),
        },
    )
}
