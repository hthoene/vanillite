//! What the commands do, with Vanilla's feedback (`net.minecraft.server.commands`).

use vanillite_base::Vec3;
use vanillite_command::coords::Coordinates;
use vanillite_command::error::CommandError;
use vanillite_command::values::GameMode as ArgGameMode;
use vanillite_config::GameMode;
use vanillite_game::entities::Entities;
use vanillite_game::player::chat::ChatMessage;
use vanillite_game::player::join::{self, display_name, game_mode_name, set_game_mode};
use vanillite_protocol::chat::ChatTypeBound;
use vanillite_protocol::packets::play::clientbound::{PlayerInfoUpdate, info_action};
use vanillite_protocol::text::{Arg, ClickEvent, Color, Component, HoverEvent, NamedColor};
use vanillite_world::entity::{EntityFlags, Rotation};

use super::select::Target;
use super::source::CommandSource;
use crate::global::Global;
use crate::player_list::Uuid;
use crate::world::WorldId;

/// The protocol's game mode as the configuration's.
pub(crate) fn config_game_mode(mode: ArgGameMode) -> GameMode {
    match mode {
        ArgGameMode::Survival => GameMode::Survival,
        ArgGameMode::Creative => GameMode::Creative,
        ArgGameMode::Adventure => GameMode::Adventure,
        ArgGameMode::Spectator => GameMode::Spectator,
    }
}

/// `gameMode.<name>`.
pub(crate) fn mode_component(mode: GameMode) -> Component {
    Component::translatable(format!("gameMode.{}", game_mode_name(mode)), vec![])
}

/// The player entity of `uuid` in this world.
pub(crate) fn find_player(
    entities: &Entities,
    uuid: u128,
) -> Option<vanillite_world::entity::EntityId> {
    entities
        .players
        .iter()
        .find_map(|(id, _)| (entities.store.get(id)?.uuid() == uuid).then_some(id))
}

/// The tab list part of a game mode change (`broadcastAll(UPDATE_GAME_MODE)`).
pub(crate) fn broadcast_game_mode(global: &Global, uuid: u128, mode: GameMode) {
    global
        .players
        .update_info(Uuid(uuid), |e| e.game_mode = mode);
    if let Some(entry) = global
        .players
        .info_entries()
        .into_iter()
        .find(|e| e.uuid == uuid)
    {
        global.players.broadcast(
            &PlayerInfoUpdate {
                actions: info_action::UPDATE_GAME_MODE,
                ..join::player_info_add([&entry])
            },
            None,
        );
    }
}

/// Changes the game mode of a player of this world; `broadcast` runs at Vanilla's point of the
/// tab list update.
fn apply_game_mode(
    entities: &mut Entities,
    uuid: u128,
    mode: GameMode,
    broadcast: impl FnOnce(),
) -> bool {
    let Some(player) = find_player(entities, uuid).and_then(|id| entities.player_mut(id)) else {
        return false;
    };
    set_game_mode(player, mode, broadcast)
}

/// `ServerPlayer.setGameMode` without feedback (`enforceGameTypeForPlayers`): whether the
/// mode changed. Players of other worlds change between ticks.
pub(crate) fn enforce_game_mode(
    entities: &mut Entities,
    global: &Global,
    target: &Target,
    mode: GameMode,
) -> bool {
    if target.game_mode == mode
        || !crate::plugins::game_mode_change(
            global,
            u32::from(target.world.0),
            target.uuid,
            target.game_mode,
            mode,
        )
    {
        return false;
    }
    if target.entity.is_some() {
        return apply_game_mode(entities, target.uuid, mode, || {
            broadcast_game_mode(global, target.uuid, mode);
        });
    }
    broadcast_game_mode(global, target.uuid, mode);
    let uuid = target.uuid;
    global.run_in_world(target.world, move |w| {
        apply_game_mode(w.entities_mut(), uuid, mode, || {});
    });
    true
}

/// `GameModeCommand.setMode`.
pub(crate) fn game_mode(
    source: &CommandSource,
    entities: &mut Entities,
    global: &Global,
    targets: Vec<Target>,
    mode: GameMode,
) -> i32 {
    let mut count = 0;
    for target in targets {
        if target.game_mode == mode
            || !crate::plugins::game_mode_change(
                global,
                u32::from(target.world.0),
                target.uuid,
                target.game_mode,
                mode,
            )
        {
            continue;
        }
        if target.entity.is_some() {
            let changed = apply_game_mode(entities, target.uuid, mode, || {
                broadcast_game_mode(global, target.uuid, mode);
            });
            if !changed {
                continue;
            }
        } else {
            // On another world: the tab list changes now, the player on its world.
            broadcast_game_mode(global, target.uuid, mode);
            let uuid = target.uuid;
            global.run_in_world(target.world, move |w| {
                apply_game_mode(w.entities_mut(), uuid, mode, || {});
            });
        }
        count += 1;
        if source.uuid() == Some(target.uuid) {
            source.send_success(
                global,
                Component::translatable(
                    "commands.gamemode.success.self",
                    vec![mode_component(mode)],
                ),
                true,
            );
        } else {
            let feedback = global
                .level
                .rules()
                .bool(vanillite_game::survival::rules::Rule::SEND_COMMAND_FEEDBACK);
            if feedback && let Some((sink, _)) = global.players.connection(Uuid(target.uuid)) {
                sink.send(
                    &vanillite_protocol::packets::play::clientbound::SystemChat {
                        content: Component::translatable(
                            "gameMode.changed",
                            vec![mode_component(mode)],
                        ),
                        overlay: false,
                    },
                );
            }
            source.send_success(
                global,
                Component::translatable(
                    "commands.gamemode.success.other",
                    vec![
                        display_name(&target.name, target.uuid),
                        mode_component(mode),
                    ],
                ),
                true,
            );
        }
    }
    count
}

/// `Level.isInSpawnableBounds`.
pub(crate) fn in_spawnable_bounds(pos: Vec3) -> bool {
    let (x, y, z) = (pos.x.floor(), pos.y.floor(), pos.z.floor());
    (-30_000_000.0..30_000_000.0).contains(&x)
        && (-30_000_000.0..30_000_000.0).contains(&z)
        && (-20_000_000.0..20_000_000.0).contains(&y)
}

/// `Mth.wrapDegrees`.
fn wrap_degrees(v: f32) -> f32 {
    let w = v % 360.0;
    if w >= 180.0 {
        w - 360.0
    } else if w < -180.0 {
        w + 360.0
    } else {
        w
    }
}

/// `TeleportCommand.performTeleport` for an entity in this world. Players fire the plugins'
/// teleport event with `cause` (`vanillite_abi::teleport_cause`): cancelled, they stay; a
/// destination in another world sends them there.
#[allow(clippy::too_many_arguments)]
pub(crate) fn teleport_local(
    level: &mut vanillite_game::level::Level,
    global: &Global,
    target: &Target,
    pos: Vec3,
    relatives: i32,
    y_rot: f32,
    x_rot: f32,
    cause: u32,
) -> Result<(), CommandError> {
    use vanillite_protocol::packets::play::clientbound::relative;
    if !in_spawnable_bounds(pos) {
        return Err(CommandError::new(
            "commands.teleport.invalidPosition",
            vec![],
        ));
    }
    let Some(id) = target.entity else {
        return Ok(());
    };
    let (mut pos, mut y_rot, mut x_rot) = (pos, y_rot, x_rot);
    if level.entities.player(id).is_some()
        && let Some(hooks) = level.hooks_for(vanillite_abi::event::PLAYER_TELEPORT)
    {
        let to = vanillite_game::hooks::Place::looking(pos, y_rot, x_rot);
        let hooks: &dyn vanillite_game::hooks::GameHooks = &*hooks;
        let Some(place) = vanillite_game::player::movement::teleport_event(
            Some(hooks),
            &level.entities,
            id,
            cause,
            to,
        ) else {
            return Ok(());
        };
        if let Some(world) = place.world {
            if let Some(world) = plugin_world(global, world) {
                move_to_world(
                    global,
                    target,
                    world,
                    place.vec(),
                    Some((place.yaw, place.pitch)),
                    None,
                );
                return Ok(());
            }
        } else {
            (pos, y_rot, x_rot) = (place.vec(), place.yaw, place.pitch);
        }
    }
    // `ServerPlayer.teleportTo`: a sleeper leaves its bed first.
    vanillite_game::survival::sleep::stop_sleep_in_bed(
        level,
        id,
        true,
        true,
        &vanillite_game::hooks::NoHooks,
    );
    let entities = &mut level.entities;
    let Some(entity) = entities.store.get(id) else {
        return Ok(());
    };
    let current = entity.pos();
    let rot = entity.rot();
    let flags = entity.flags();
    let rel = |flag: i32, cur: f64, v: f64| if relatives & flag != 0 { v - cur } else { v };
    let wire = Vec3::new(
        rel(relative::X, current.x, pos.x),
        rel(relative::Y, current.y, pos.y),
        rel(relative::Z, current.z, pos.z),
    );
    let y = wrap_degrees(if relatives & relative::Y_ROT != 0 {
        y_rot - rot.y_rot
    } else {
        y_rot
    });
    let x = wrap_degrees(if relatives & relative::X_ROT != 0 {
        x_rot - rot.x_rot
    } else {
        x_rot
    });
    let mut body = vanillite_game::player::Body {
        pos: current,
        velocity: entity.velocity(),
        y_rot: rot.y_rot,
        x_rot: rot.x_rot,
        on_ground: flags.contains(EntityFlags::ON_GROUND),
        horizontal_collision: false,
        fall_flying: flags.contains(EntityFlags::FALL_FLYING),
    };
    let Some(player) = entities.player_mut(id) else {
        // `Entity.teleportTo` in the same level, then `performTeleport`'s motion and ground.
        let y_rot = if relatives & relative::Y_ROT != 0 {
            rot.y_rot + y
        } else {
            y
        };
        let x_rot = if relatives & relative::X_ROT != 0 {
            rot.x_rot + x
        } else {
            x
        };
        if let Some(mut entity) = entities.store.get_mut(id) {
            entity.set_pos(pos);
            entity.set_rot(Rotation {
                y_rot,
                x_rot,
                y_head_rot: y_rot,
            });
            if !flags.contains(EntityFlags::FALL_FLYING) {
                let v = entity.as_ref().velocity();
                entity.set_velocity(Vec3::new(v.x, 0.0, v.z));
                entity.set_flags(EntityFlags::ON_GROUND, true);
            }
        }
        if let Some(mob) = entities.mob_mut(id) {
            mob.nav.stop();
        }
        vanillite_game::riding::send_teleport_transition(
            level,
            id,
            vanillite_protocol::packets::play::clientbound::PositionMoveRotation {
                position: vanillite_game::riding::wire_vec(wire),
                delta: vanillite_game::riding::wire_vec(Vec3::ZERO),
                y_rot: y,
                x_rot: x,
            },
            relatives,
            current,
            flags.contains(EntityFlags::ON_GROUND),
        );
        return Ok(());
    };
    let sink = std::sync::Arc::clone(&player.connection.sink);
    player
        .connection
        .movement
        .teleport_relative(&mut body, wire, y, x, relatives, sink.as_ref());
    // `ServerPlayer.teleport` resets the movement checks to the new position.
    player.connection.movement.reset_position(body.pos);
    player.survival.teleported();
    if let Some(mut entity) = entities.store.get_mut(id) {
        entity.set_pos(body.pos);
        entity.set_rot(Rotation {
            y_rot: body.y_rot,
            x_rot: body.x_rot,
            y_head_rot: body.y_rot,
        });
        // `teleportSetPosition`: `setOldPosAndRot`.
        entity.set_old_pos();
        if !body.fall_flying {
            let v = body.velocity;
            entity.set_velocity(Vec3::new(v.x, 0.0, v.z));
            entity.set_flags(EntityFlags::ON_GROUND, true);
        }
    }
    Ok(())
}

/// `%f` of `String.format(Locale.ROOT, ...)`.
fn format_double(v: f64) -> String {
    format!("{v:.6}")
}

/// Where `tp` sends its targets.
pub(crate) enum Destination {
    /// Another entity (its position and rotation).
    Entity(Target),
    /// Coordinates with an optional rotation (`[y_rot, x_rot]` as unresolved coordinates).
    Position {
        coordinates: Coordinates,
        rotation: Option<Coordinates>,
    },
}

/// `Coordinates.getPosition`: world coordinates relative to the source, local ones along its
/// view (`LocalCoordinates`).
pub(crate) fn resolve_position(source: &CommandSource, coordinates: &Coordinates) -> Vec3 {
    match coordinates {
        Coordinates::World([x, y, z]) => Vec3::new(
            x.get(source.pos.x),
            y.get(source.pos.y),
            z.get(source.pos.z),
        ),
        Coordinates::Local([left, up, forwards]) => {
            let (y_rot, x_rot) = source.rot;
            let (yaw, pitch) = (
                f64::from(y_rot + 90.0).to_radians(),
                f64::from(-x_rot).to_radians(),
            );
            let (yaw2, pitch2) = (yaw, f64::from(-x_rot + 90.0).to_radians());
            let forward = Vec3::new(
                yaw.cos() * pitch.cos(),
                pitch.sin(),
                yaw.sin() * pitch.cos(),
            );
            let upv = Vec3::new(
                yaw2.cos() * pitch2.cos(),
                pitch2.sin(),
                yaw2.sin() * pitch2.cos(),
            );
            let leftv = Vec3::new(
                forward.y * upv.z - forward.z * upv.y,
                forward.z * upv.x - forward.x * upv.z,
                forward.x * upv.y - forward.y * upv.x,
            );
            let leftv = Vec3::new(-leftv.x, -leftv.y, -leftv.z);
            let origin = source.anchor_pos();
            Vec3::new(
                origin.x + forward.x * forwards + upv.x * up + leftv.x * left,
                origin.y + forward.y * forwards + upv.y * up + leftv.y * left,
                origin.z + forward.z * forwards + upv.z * up + leftv.z * left,
            )
        }
    }
}

/// `Coordinates.getRotation` as (`y_rot`, `x_rot`); local rotations are zero.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
pub(crate) fn resolve_rotation(source: &CommandSource, rotation: &Coordinates) -> (f32, f32) {
    match rotation {
        Coordinates::World([x, y, _]) => (
            y.get(f64::from(source.rot.0)) as f32,
            x.get(f64::from(source.rot.1)) as f32,
        ),
        Coordinates::Local(_) => (0.0, 0.0),
    }
}

/// `TeleportCommand.getRelatives`.
fn relatives(coordinates: &Coordinates, rotation: Option<&Coordinates>) -> i32 {
    use vanillite_protocol::packets::play::clientbound::relative;
    let mut bits = 0;
    if let Coordinates::World([x, y, z]) = coordinates {
        for (c, pos, delta) in [
            (x, relative::X, relative::DELTA_X),
            (y, relative::Y, relative::DELTA_Y),
            (z, relative::Z, relative::DELTA_Z),
        ] {
            if c.relative {
                bits |= pos | delta;
            }
        }
    }
    match rotation {
        None => bits |= relative::Y_ROT | relative::X_ROT,
        Some(Coordinates::World([x_rot, y_rot, _])) => {
            if y_rot.relative {
                bits |= relative::Y_ROT;
            }
            if x_rot.relative {
                bits |= relative::X_ROT;
            }
        }
        Some(Coordinates::Local(_)) => {}
    }
    bits
}

/// `TeleportCommand.teleportToEntity` / `teleportToPos`. The destination is in the source's
/// world, or the destination entity's. Targets in another world move there on their world's
/// thread (players travel like through a portal); a destination entity of another world is
/// looked up on its thread, and the feedback follows from there.
pub(crate) fn teleport(
    source: &CommandSource,
    env: &mut super::Env<'_>,
    targets: Vec<Target>,
    destination: Destination,
) -> Result<i32, CommandError> {
    let global = env.global;
    let count = i32::try_from(targets.len()).unwrap_or(i32::MAX);
    match &destination {
        Destination::Entity(to) => {
            let message = match targets.as_slice() {
                [one] => Component::translatable(
                    "commands.teleport.success.entity.single",
                    vec![one.display(global), to.display(global)],
                ),
                _ => Component::translatable_args(
                    "commands.teleport.success.entity.multiple",
                    vec![Arg::Int(count), Arg::from(to.display(global))],
                ),
            };
            let Some((pos, y_rot, x_rot)) = to.place else {
                // The destination is in another world: its place is read there.
                let (uuid, world, source) = (to.uuid, to.world, source.clone());
                super::entities_cmd::on_level(env, world, move |level, global, _| {
                    let Some(place) = super::select::find_uuid(&level.entities, uuid)
                        .and_then(|id| level.entities.store.get(id))
                        .map(|e| (e.pos(), e.rot()))
                    else {
                        source.send_failure(
                            global,
                            Component::translatable("commands.teleport.invalidPosition", vec![]),
                        );
                        return;
                    };
                    let (pos, rot) = place;
                    if !in_spawnable_bounds(pos) {
                        source.send_failure(
                            global,
                            Component::translatable("commands.teleport.invalidPosition", vec![]),
                        );
                        return;
                    }
                    for target in &targets {
                        move_to_world(
                            global,
                            target,
                            world,
                            pos,
                            Some((rot.y_rot, rot.x_rot)),
                            Some(vanillite_abi::teleport_cause::COMMAND),
                        );
                    }
                    source.send_success(global, message, true);
                });
                return Ok(count);
            };
            for target in &targets {
                if target.entity.is_some() {
                    teleport_local(
                        env.level,
                        global,
                        target,
                        pos,
                        0,
                        y_rot,
                        x_rot,
                        vanillite_abi::teleport_cause::COMMAND,
                    )?;
                } else {
                    if !in_spawnable_bounds(pos) {
                        return Err(CommandError::new(
                            "commands.teleport.invalidPosition",
                            vec![],
                        ));
                    }
                    let world = env.world;
                    if !move_now(env, target, world, pos, Some((y_rot, x_rot))) {
                        move_to_world(
                            global,
                            target,
                            world,
                            pos,
                            Some((y_rot, x_rot)),
                            Some(vanillite_abi::teleport_cause::COMMAND),
                        );
                    }
                }
            }
            source.send_success(global, message, true);
        }
        Destination::Position {
            coordinates,
            rotation,
        } => {
            let pos = resolve_position(source, coordinates);
            let rot = rotation.as_ref().map(|r| match r {
                Coordinates::World([x, y, _]) => (
                    y.get(f64::from(source.rot.0)) as f32,
                    x.get(f64::from(source.rot.1)) as f32,
                ),
                Coordinates::Local(_) => source.rot,
            });
            let bits = relatives(coordinates, rotation.as_ref());
            for target in &targets {
                if target.entity.is_none() {
                    if !in_spawnable_bounds(pos) {
                        return Err(CommandError::new(
                            "commands.teleport.invalidPosition",
                            vec![],
                        ));
                    }
                    if !move_now(env, target, source.world, pos, rot) {
                        move_to_world(
                            global,
                            target,
                            source.world,
                            pos,
                            rot,
                            Some(vanillite_abi::teleport_cause::COMMAND),
                        );
                    }
                    continue;
                }
                let (y_rot, x_rot) = match rot {
                    Some(r) => r,
                    None => target.place.map_or((0.0, 0.0), |(_, y, x)| (y, x)),
                };
                teleport_local(
                    env.level,
                    global,
                    target,
                    pos,
                    bits,
                    y_rot,
                    x_rot,
                    vanillite_abi::teleport_cause::COMMAND,
                )?;
            }
            let coords = [pos.x, pos.y, pos.z].map(|v| Arg::String(format_double(v)));
            let message = match targets.as_slice() {
                [one] => {
                    let mut args = vec![Arg::from(one.display(global))];
                    args.extend(coords);
                    Component::translatable_args("commands.teleport.success.location.single", args)
                }
                _ => {
                    let mut args = vec![Arg::Int(count)];
                    args.extend(coords);
                    Component::translatable_args(
                        "commands.teleport.success.location.multiple",
                        args,
                    )
                }
            };
            source.send_success(global, message, true);
        }
    }
    Ok(count)
}

/// Moves a target of another world than the command's to `world` at once
/// (`Entity.teleportTo(level, ...)`), when it is not a player and both levels are at hand:
/// whether it did.
fn move_now(
    env: &mut super::Env<'_>,
    target: &Target,
    world: WorldId,
    pos: Vec3,
    rotation: Option<(f32, f32)>,
) -> bool {
    if target.is_player() || target.world == world || !env.has(world) {
        return false;
    }
    let Some(level) = env.level_of(target.world) else {
        return false;
    };
    let Some(id) = super::select::find_uuid(&level.entities, target.uuid) else {
        return true;
    };
    let Some(rot) = level.entities.store.get(id).map(|e| e.rot()) else {
        return true;
    };
    let (y_rot, x_rot) = rotation.unwrap_or((rot.y_rot, rot.x_rot));
    let nbt = crate::world::ServerWorld::take_entity(
        level,
        id,
        pos,
        vanillite_base::math::wrap_degrees_f(y_rot),
        vanillite_base::math::wrap_degrees_f(x_rot),
    );
    if let (Some(nbt), Some(level)) = (nbt, env.level_of(world)) {
        crate::world::ServerWorld::load_moved(level, nbt);
    }
    true
}

/// Moves a target of another world than the command's to `world` on the target's world
/// thread (`Entity.teleportTo(level, ...)`). A player fires the plugins' teleport event with
/// `cause` there (`None`: it already did).
pub(crate) fn move_to_world(
    global: &Global,
    target: &Target,
    world: WorldId,
    pos: Vec3,
    rotation: Option<(f32, f32)>,
    cause: Option<u32>,
) {
    let Some(global_arc) = global.arc() else {
        return;
    };
    let uuid = target.uuid;
    global.run_in_world(target.world, move |w| {
        w.teleport_to_world(&global_arc, uuid, world, pos, rotation, cause);
    });
}

/// The world of a plugin's world index, if there is one.
pub(crate) fn plugin_world(global: &crate::global::Global, index: u32) -> Option<WorldId> {
    u16::try_from(index)
        .ok()
        .map(WorldId)
        .filter(|&w| global.dimensions.is_loaded(w))
}

/// `ComponentUtils.formatList` with `, ` in gray.
pub(crate) fn format_list(items: Vec<Component>) -> Component {
    let mut items = items.into_iter();
    let Some(first) = items.next() else {
        return Component::empty();
    };
    let mut rest = items.peekable();
    if rest.peek().is_none() {
        return first;
    }
    let mut out = Component::empty().append(first);
    for item in rest {
        out = out
            .append(Component::text(", ").color(Color::Named(NamedColor::Gray)))
            .append(item);
    }
    out
}

/// `ListPlayersCommand`.
pub(crate) fn list(source: &CommandSource, global: &Global, uuids: bool) -> i32 {
    let players = global.players.snapshot();
    let names: Vec<Component> = players
        .iter()
        .map(|p| {
            if uuids {
                Component::translatable_args(
                    "commands.list.nameAndId",
                    vec![Arg::String(p.name.clone()), Arg::String(p.uuid.to_string())],
                )
            } else {
                display_name(&p.name, p.uuid.0)
            }
        })
        .collect();
    let count = i32::try_from(players.len()).unwrap_or(i32::MAX);
    let max = i32::try_from(global.players.max_players()).unwrap_or(i32::MAX);
    source.send_success(
        global,
        Component::translatable_args(
            "commands.list.players",
            vec![
                Arg::Int(count),
                Arg::Int(max),
                Arg::from(format_list(names)),
            ],
        ),
        false,
    );
    count
}

/// `SeedCommand`.
pub(crate) fn seed(source: &CommandSource, global: &Global, seed: i64) -> i32 {
    let text = seed.to_string();
    let copy = Component::text(text.as_str())
        .color(Color::Named(NamedColor::Green))
        .click(ClickEvent::CopyToClipboard(text.clone()))
        .hover(HoverEvent::ShowText(Component::translatable(
            "chat.copy.click",
            vec![],
        )))
        .insertion(text.as_str());
    let bracketed = Component::translatable("chat.square_brackets", vec![copy]);
    source.send_success(
        global,
        Component::translatable("commands.seed.success", vec![bracketed]),
        false,
    );
    i32::try_from(seed & i64::from(i32::MAX)).unwrap_or(0)
}

/// `TimeCommand.wrapTime`.
fn wrap_time(ticks: i64) -> i32 {
    i32::try_from(ticks % i64::from(i32::MAX)).unwrap_or(0)
}

/// `time query gametime`.
pub(crate) fn time_gametime(source: &CommandSource, global: &Global, game_time: i64) -> i32 {
    source.send_success(
        global,
        Component::translatable_args("commands.time.query.gametime", vec![Arg::Long(game_time)]),
        false,
    );
    wrap_time(game_time)
}

/// `say`: broadcast as `say_command`.
pub(crate) fn say(source: &CommandSource, global: &Global, message: &ChatMessage) {
    let chat_type = ChatTypeBound {
        chat_type: global.chat_types.say_command,
        name: source.display_name(),
        target_name: None,
    };
    crate::chat::broadcast_chat(global, message, &chat_type, "chat.type.announcement");
}

/// `msg`/`tell`/`w`: the outgoing message to the sender, the incoming one to each target.
pub(crate) fn msg(
    source: &CommandSource,
    global: &Global,
    targets: &[Target],
    message: &ChatMessage,
) -> i32 {
    let incoming = ChatTypeBound {
        chat_type: global.chat_types.msg_incoming,
        name: source.display_name(),
        target_name: None,
    };
    let max = global.chat.max_pending_messages;
    for target in targets {
        let outgoing = ChatTypeBound {
            chat_type: global.chat_types.msg_outgoing,
            name: source.display_name(),
            target_name: Some(display_name(&target.name, target.uuid)),
        };
        match source
            .uuid()
            .and_then(|u| global.players.connection(Uuid(u)))
        {
            Some((sink, inbox)) => {
                let sent = inbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .send(message, &outgoing, sink.as_ref(), max);
                if !sent {
                    sink.kick(Component::translatable(
                        "multiplayer.disconnect.too_many_pending_chats",
                        vec![],
                    ));
                }
            }
            None => log::info!(
                "You whisper to {}: {}",
                target.name,
                message.decorated_content().plain()
            ),
        }
        if let Some((sink, inbox)) = global.players.connection(Uuid(target.uuid)) {
            let sent = inbox
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(message, &incoming, sink.as_ref(), max);
            if !sent {
                sink.kick(Component::translatable(
                    "multiplayer.disconnect.too_many_pending_chats",
                    vec![],
                ));
            }
        }
    }
    i32::try_from(targets.len()).unwrap_or(i32::MAX)
}

/// `KickCommand`.
pub(crate) fn kick(
    source: &CommandSource,
    global: &Global,
    targets: &[Target],
    reason: Component,
) -> i32 {
    for target in targets {
        if let Some((sink, _)) = global.players.connection(Uuid(target.uuid)) {
            let world = u32::from(target.world.0);
            match crate::plugins::player_kick(&global.plugins, world, target.uuid, reason.clone()) {
                Some(reason) => sink.kick(reason),
                // A plugin kept the player (Bukkit's cancelled `PlayerKickEvent`).
                None => continue,
            }
        }
        source.send_success(
            global,
            Component::translatable(
                "commands.kick.success",
                vec![display_name(&target.name, target.uuid), reason.clone()],
            ),
            true,
        );
    }
    i32::try_from(targets.len()).unwrap_or(i32::MAX)
}

/// The `entity_event` that tells a client its permission level (`sendPlayerPermissionLevel`).
pub(crate) fn permission_event(
    entity_id: i32,
    level: u8,
) -> vanillite_protocol::packets::play::clientbound::EntityEvent {
    vanillite_protocol::packets::play::clientbound::EntityEvent {
        entity_id,
        event: 24i8.saturating_add(i8::try_from(level.min(4)).unwrap_or(0)),
    }
}

/// The world a player is in, from the player list.
pub(crate) fn world_of(global: &Global, uuid: u128) -> Option<WorldId> {
    global
        .players
        .snapshot()
        .into_iter()
        .find(|p| p.uuid.0 == uuid)
        .map(|p| p.world)
}

/// A player profile named by a `game_profile` argument.
#[derive(Clone, Debug)]
pub(crate) struct Profile {
    pub(crate) uuid: u128,
    pub(crate) name: String,
}

/// `OpCommand.opPlayers`.
pub(crate) fn op(
    source: &CommandSource,
    global: &Global,
    profiles: &[Profile],
) -> Result<i32, CommandError> {
    let mut count = 0;
    for p in profiles {
        if global.access.op(p.uuid, &p.name) {
            count += 1;
            super::permissions_changed(global, p.uuid);
            source.send_success(
                global,
                Component::translatable_args(
                    "commands.op.success",
                    vec![Arg::String(p.name.clone())],
                ),
                true,
            );
        }
    }
    if count == 0 {
        return Err(CommandError::new("commands.op.failed", vec![]));
    }
    Ok(count)
}

/// `DeOpCommand.deopPlayers`.
pub(crate) fn deop(
    source: &CommandSource,
    global: &Global,
    profiles: &[Profile],
) -> Result<i32, CommandError> {
    let mut count = 0;
    for p in profiles {
        if global.access.deop(p.uuid) {
            count += 1;
            super::permissions_changed(global, p.uuid);
            source.send_success(
                global,
                Component::translatable_args(
                    "commands.deop.success",
                    vec![Arg::String(p.name.clone())],
                ),
                true,
            );
        }
    }
    if count == 0 {
        return Err(CommandError::new("commands.deop.failed", vec![]));
    }
    Ok(count)
}

/// `PlayerList.kickUnlistedPlayers` when `enforce-whitelist` is on.
pub(crate) fn kick_unlisted(global: &Global) {
    if !global.access.enforce_whitelist || !global.access.whitelist_enabled() {
        return;
    }
    for p in global.players.snapshot() {
        if !global.access.is_whitelisted(p.uuid.0) {
            p.sink.kick(Component::translatable(
                "multiplayer.disconnect.not_whitelisted",
                vec![],
            ));
        }
    }
}

/// `whitelist on|off`.
pub(crate) fn whitelist_switch(
    source: &CommandSource,
    global: &Global,
    on: bool,
) -> Result<i32, CommandError> {
    if global.access.whitelist_enabled() == on {
        let key = if on {
            "commands.whitelist.alreadyOn"
        } else {
            "commands.whitelist.alreadyOff"
        };
        return Err(CommandError::new(key, vec![]));
    }
    global.access.set_whitelist_enabled(on);
    let key = if on {
        "commands.whitelist.enabled"
    } else {
        "commands.whitelist.disabled"
    };
    source.send_success(global, Component::translatable(key, vec![]), true);
    if on {
        kick_unlisted(global);
    }
    Ok(1)
}

/// `whitelist list`.
pub(crate) fn whitelist_list(source: &CommandSource, global: &Global) -> i32 {
    let names = global.access.whitelisted_names();
    if names.is_empty() {
        source.send_success(
            global,
            Component::translatable("commands.whitelist.none", vec![]),
            false,
        );
    } else {
        source.send_success(
            global,
            Component::translatable_args(
                "commands.whitelist.list",
                vec![
                    Arg::Int(i32::try_from(names.len()).unwrap_or(i32::MAX)),
                    Arg::String(names.join(", ")),
                ],
            ),
            false,
        );
    }
    i32::try_from(names.len()).unwrap_or(i32::MAX)
}

/// `whitelist add|remove`.
pub(crate) fn whitelist_change(
    source: &CommandSource,
    global: &Global,
    profiles: &[Profile],
    add: bool,
) -> Result<i32, CommandError> {
    let mut count = 0;
    for p in profiles {
        let changed = if add {
            global.access.whitelist_add(p.uuid, &p.name)
        } else {
            global.access.whitelist_remove(p.uuid)
        };
        if changed {
            count += 1;
            let key = if add {
                "commands.whitelist.add.success"
            } else {
                "commands.whitelist.remove.success"
            };
            source.send_success(
                global,
                Component::translatable_args(key, vec![Arg::String(p.name.clone())]),
                true,
            );
        }
    }
    if count == 0 {
        let key = if add {
            "commands.whitelist.add.failed"
        } else {
            "commands.whitelist.remove.failed"
        };
        return Err(CommandError::new(key, vec![]));
    }
    if !add {
        kick_unlisted(global);
    }
    Ok(count)
}

/// `whitelist reload`.
pub(crate) fn whitelist_reload(source: &CommandSource, global: &Global) -> i32 {
    global.access.reload();
    source.send_success(
        global,
        Component::translatable("commands.whitelist.reloaded", vec![]),
        true,
    );
    kick_unlisted(global);
    1
}

/// `StopCommand`.
/// `SaveAllCommand`: saves everything, chunks also after `/save-off`; with `flush` the
/// success message comes once everything is on disk.
pub(crate) fn save_all(source: &CommandSource, global: &Global, flush: bool) -> i32 {
    source.send_success(
        global,
        Component::translatable("commands.save.saving", vec![]),
        false,
    );
    let Some(handle) = global.handle.get() else {
        return 0;
    };
    let source = source.clone();
    handle.run_global(move |server| {
        server.save_everything(
            flush,
            true,
            Some(Box::new(move || {
                source.send_success(
                    &source.global,
                    Component::translatable("commands.save.success", vec![]),
                    true,
                );
            })),
        );
    });
    1
}

/// `SaveOnCommand` and `SaveOffCommand`.
pub(crate) fn save_switch(
    source: &CommandSource,
    global: &Global,
    on: bool,
) -> Result<i32, CommandError> {
    if global
        .level
        .auto_save
        .swap(on, std::sync::atomic::Ordering::Relaxed)
        == on
    {
        let key = if on {
            "commands.save.alreadyOn"
        } else {
            "commands.save.alreadyOff"
        };
        return Err(CommandError::new(key, vec![]));
    }
    let key = if on {
        "commands.save.enabled"
    } else {
        "commands.save.disabled"
    };
    source.send_success(global, Component::translatable(key, vec![]), true);
    Ok(1)
}

pub(crate) fn stop(source: &CommandSource, global: &Global) -> i32 {
    source.send_success(
        global,
        Component::translatable("commands.stop.stopping", vec![]),
        true,
    );
    global.stop();
    1
}
