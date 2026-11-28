//! The entity commands: `summon`, `ride`, `rotate`, `damage`, `swing` (Vanilla's
//! `SummonCommand`, `RideCommand`, `RotateCommand`, `DamageCommand`, `SwingCommand`), and
//! `effect` and `bossbar`, whose syntax is complete but whose models do not exist yet.

use std::sync::Arc;

use vanillite_base::Vec3;
use vanillite_command::Args;
use vanillite_command::coords::Coordinates;
use vanillite_command::error::{Arg as ErrArg, CommandError};
use vanillite_command::selector::EntitySelector;
use vanillite_command::values::Anchor;
use vanillite_game::commands::CommandId;
use vanillite_game::effect::MobEffect;
use vanillite_game::effect::living::{add_queued, add_queued_from};
use vanillite_game::entities::Entities;
use vanillite_game::survival::damage::DamageSource;
use vanillite_nbt::Compound;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{
    LookAtEntity, PlayerLookAt, PlayerRotation, SwingAnimation,
};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_protocol::text::{Arg, Component};
use vanillite_world::entity::{EntityId, Rotation};
use vanillite_world::tracking::Viewer;

use super::Env;
use super::exec::{resolve_position, resolve_rotation};
use super::select::{self, Target};
use super::source::CommandSource;
use super::tracker::Tracker;
use crate::global::Global;
use crate::survival::WorldSurvival;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

fn selector<'a>(args: &'a Args, name: &str) -> Result<&'a EntitySelector, CommandError> {
    args.selector(name).ok_or_else(|| missing(name))
}

fn component(c: Component) -> ErrArg {
    ErrArg::Component(Box::new(c))
}

/// `getEntityOrException`: the source's own entity.
fn own_entity(source: &CommandSource, env: &Env<'_>) -> Result<Target, CommandError> {
    super::dispatch::self_target(env, source)
}

/// Runs `f` on a target's entity: at once when it is in this world, else on its world's
/// thread without waiting (Vanilla acts on entities of every world).
fn on_entity(
    env: &mut Env<'_>,
    target: &Target,
    f: impl FnOnce(&mut Entities, EntityId, &Global, &str) + Send + 'static,
) -> bool {
    let global = env.global;
    let dimension = global.dimensions.name_or_overworld(target.world);
    if let Some(id) = target.entity {
        f(&mut env.level.entities, id, env.global, dimension);
        return true;
    }
    let Some(global) = env.global.arc() else {
        return false;
    };
    let uuid = target.uuid;
    let dimension = dimension.to_owned();
    env.global.run_in_world(target.world, move |w| {
        let entities = w.entities_mut();
        if let Some(id) = select::find_uuid(entities, uuid) {
            f(entities, id, &global, &dimension);
        }
    });
    true
}

/// `ChunkMap.TrackedEntity.broadcast[AndSend]` without the tracker: the players whose view
/// holds the entity's chunk, within its tracking range (and the entity itself).
fn send_to_viewers(entities: &Entities, id: EntityId, with_self: bool, packet: &Arc<SharedPacket>) {
    let Some(entity) = entities.store.get(id) else {
        return;
    };
    let pos = entity.pos();
    let chunk = vanillite_base::ChunkPos::new(
        vanillite_base::math::floor(pos.x) >> 4,
        vanillite_base::math::floor(pos.z) >> 4,
    );
    let range = f64::from(entity.kind().info().tracking_range.saturating_mul(16));
    for (viewer, player) in entities.players.iter() {
        let sees = if viewer == id {
            with_self
        } else {
            let Some(at) = entities.store.get(viewer).map(|e| e.pos()) else {
                continue;
            };
            let view = player.chunk_view;
            let limit = range.min(f64::from(view.view_distance.saturating_mul(16)));
            let (dx, dz) = (at.x - pos.x, at.z - pos.z);
            Viewer {
                entity: viewer,
                view_center: view.center,
                view_distance: view.view_distance,
                sent_radius: 0,
            }
            .sees_chunk(chunk)
                && dx * dx + dz * dz <= limit * limit
        };
        if sees {
            player.connection.sink.send_shared(packet);
        }
    }
}

/// Runs an entity command.
pub(crate) fn run(
    id: CommandId,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    match id {
        CommandId::Summon => summon(args, source, env),
        CommandId::RideMount => {
            let target = select::single(env, source, selector(args, "target")?)?;
            let vehicle = select::single(env, source, selector(args, "vehicle")?)?;
            ride_mount(source, env, &target, &vehicle)
        }
        CommandId::RideDismount => {
            let target = select::single(env, source, selector(args, "target")?)?;
            ride_dismount(source, env, &target)
        }
        CommandId::Rotate => rotate(args, source, env),
        CommandId::Damage => damage(args, source, env),
        CommandId::Swing { offhand } => swing(offhand, args, source, env),
        CommandId::EffectClear => effect_clear(args, source, env),
        CommandId::EffectGive { infinite } => effect_give(infinite, args, source, env),
        _ => Err(CommandError::new("command.unknown.command", vec![])),
    }
}

/// The `effect` argument: a `minecraft:mob_effect` id.
fn effect_arg(args: &Args) -> Result<(MobEffect, Component), CommandError> {
    let name = args.identifier("effect").ok_or_else(|| missing("effect"))?;
    let Some(effect) = vanillite_data::registry::MOB_EFFECT
        .id(name)
        .and_then(|id| MobEffect::from_id(i32::from(id)))
    else {
        return Err(CommandError::new(
            "argument.resource.not_found",
            vec![
                ErrArg::Str(name.clone()),
                ErrArg::Str("minecraft:mob_effect".to_owned()),
            ],
        ));
    };
    // `MobEffect.getDisplayName`.
    let key = format!("effect.{}", name.replace(':', "."));
    Ok((effect, Component::translatable(key, vec![])))
}

/// Whether a target is a `LivingEntity`.
fn is_living(target: &Target) -> bool {
    !target.kind.info().attributes.is_empty()
}

/// `EffectCommands.giveEffect`.
fn effect_give(
    infinite: bool,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let targets = select::many(env, source, selector(args, "targets")?)?;
    let (effect, effect_name) = effect_arg(args)?;
    let amplifier = args.int("amplifier").copied().unwrap_or(0);
    let particles = !args.bool("hideParticles").copied().unwrap_or(false);
    // `computeDurationInTicks`.
    let duration = if infinite {
        vanillite_game::effect::instance::INFINITE
    } else {
        match args.int("seconds").copied() {
            Some(s) if effect.is_instantaneous() => s,
            Some(s) => s.saturating_mul(20),
            None if effect.is_instantaneous() => 1,
            None => 600,
        }
    };
    let instance = vanillite_game::effect::EffectInstance::with_flags(
        effect, duration, amplifier, false, particles, particles,
    );
    // `addEffect(instance, source.getEntity())`: the source's entity of this world.
    let by = source
        .entity
        .as_ref()
        .filter(|e| e.world == env.world)
        .and_then(|e| e.id);
    let mut tracker = Tracker::<Component>::default();
    for target in &targets {
        if !is_living(target) {
            continue;
        }
        let changed = match target.entity {
            Some(id) => add_queued_from(&mut env.level.entities, id, instance.clone(), by),
            // An entity of another world: applied on its world's thread; counted as changed
            // when its kind can be affected.
            None => {
                let instance = instance.clone();
                on_entity(env, target, move |entities, id, _, _| {
                    add_queued(entities, id, instance);
                });
                vanillite_game::effect::can_be_affected(target.kind, effect)
                    && !vanillite_game::effect::refuses_all_effects(target.kind)
            }
        };
        tracker.track(&target.display(env.global), i32::from(changed));
    }
    tracker.messages(
        source,
        env.global,
        || CommandError::new("commands.effect.give.failed", vec![]),
        |entity| {
            Component::translatable_args(
                "commands.effect.give.success.single",
                vec![
                    Arg::from(effect_name.clone()),
                    Arg::from(entity.clone()),
                    Arg::Int(duration / 20),
                ],
            )
        },
        |count| {
            Component::translatable_args(
                "commands.effect.give.success.multiple",
                vec![
                    Arg::from(effect_name.clone()),
                    Arg::Int(count),
                    Arg::Int(duration / 20),
                ],
            )
        },
    )
}

/// `removeEffect` (with `only`) or `removeAllEffects` on a living entity; whether it had it.
fn remove_effects(
    entities: &mut vanillite_game::entities::Entities,
    id: vanillite_world::entity::EntityId,
    only: Option<MobEffect>,
) -> bool {
    entities.effects_mut(id).is_some_and(|fx| match only {
        Some(e) => fx.remove(e),
        None => fx.clear(),
    })
}

/// `EffectCommands.clearEffects` and `clearEffect`.
fn effect_clear(
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let targets = match args.selector("targets") {
        Some(s) => select::many(env, source, s)?,
        None => vec![own_entity(source, env)?],
    };
    let effect = if args.has("effect") {
        Some(effect_arg(args)?)
    } else {
        None
    };
    let mut tracker = Tracker::<Component>::default();
    for target in &targets {
        if !is_living(target) {
            continue;
        }
        let only = effect.as_ref().map(|(e, _)| *e);
        let changed = match target.entity {
            Some(id) => remove_effects(&mut env.level.entities, id, only),
            None => {
                on_entity(env, target, move |entities, id, _, _| {
                    remove_effects(entities, id, only);
                });
                true
            }
        };
        tracker.track(&target.display(env.global), i32::from(changed));
    }
    match effect {
        Some((_, name)) => tracker.messages(
            source,
            env.global,
            || CommandError::new("commands.effect.clear.specific.failed", vec![]),
            |entity| {
                Component::translatable_args(
                    "commands.effect.clear.specific.success.single",
                    vec![Arg::from(name.clone()), Arg::from(entity.clone())],
                )
            },
            |count| {
                Component::translatable_args(
                    "commands.effect.clear.specific.success.multiple",
                    vec![Arg::from(name.clone()), Arg::Int(count)],
                )
            },
        ),
        None => tracker.messages(
            source,
            env.global,
            || CommandError::new("commands.effect.clear.everything.failed", vec![]),
            |entity| {
                Component::translatable(
                    "commands.effect.clear.everything.success.single",
                    vec![entity.clone()],
                )
            },
            |count| {
                Component::translatable_args(
                    "commands.effect.clear.everything.success.multiple",
                    vec![Arg::Int(count)],
                )
            },
        ),
    }
}

/// `SummonCommand.spawnEntity`.
fn summon(args: &Args, source: &CommandSource, env: &mut Env<'_>) -> Result<i32, CommandError> {
    let id = args.identifier("entity").ok_or_else(|| missing("entity"))?;
    let pos = args
        .coordinates("pos")
        .map_or(source.pos, |c| resolve_position(source, c));
    let empty = Compound::new();
    let given = args.compound("nbt");
    let entity = super::execute::summon(
        env,
        source,
        id,
        pos,
        given.unwrap_or(&empty),
        given.is_none(),
    )?;
    source.send_success(
        env.global,
        Component::translatable("commands.summon.success", vec![entity.display(env.global)]),
        true,
    );
    Ok(1)
}

/// The dimension id of a world.
pub(crate) fn dimension_of(global: &Global, world: crate::world::WorldId) -> &str {
    global.dimensions.name_or_overworld(world)
}

/// The display name of the entity with this id (`getDisplayName`).
fn display_of(env: &Env<'_>, id: EntityId) -> Option<Component> {
    select::local(&env.level.entities, env.world, id).map(|t| t.display(env.global))
}

/// `RideCommand.mount`.
fn ride_mount(
    source: &CommandSource,
    env: &mut Env<'_>,
    target: &Target,
    vehicle: &Target,
) -> Result<i32, CommandError> {
    let (Some(t), Some(v)) = (target.entity, vehicle.entity) else {
        // Entities of another world: checked and mounted on their world's thread.
        if vehicle.is_player() {
            return Err(CommandError::new(
                "commands.ride.mount.failure.cant_ride_players",
                vec![],
            ));
        }
        if target.world != vehicle.world {
            return Err(CommandError::new(
                "commands.ride.mount.failure.wrong_dimension",
                vec![],
            ));
        }
        let (tu, vu) = (target.uuid, vehicle.uuid);
        on_level(env, target.world, move |level, global, dimension| {
            let (Some(t), Some(v)) = (
                select::find_uuid(&level.entities, tu),
                select::find_uuid(&level.entities, vu),
            ) else {
                return;
            };
            if level.entities.vehicle_of(t).is_none() {
                WorldSurvival::with_entities(level, global, dimension, |_, ents, e, _| {
                    vanillite_game::riding::start_riding(ents, e, t, v, true)
                });
            }
        });
        source.send_success(
            env.global,
            Component::translatable(
                "commands.ride.mount.success",
                vec![target.display(env.global), vehicle.display(env.global)],
            ),
            true,
        );
        return Ok(1);
    };
    if let Some(current) = env.level.entities.vehicle_of(t) {
        let current = display_of(env, current).unwrap_or_else(|| Component::text(""));
        return Err(CommandError::new(
            "commands.ride.already_riding",
            vec![component(target.display(env.global)), component(current)],
        ));
    }
    if let Some(key) = mount_refusal(&env.level.entities, t, v) {
        return Err(CommandError::new(key, vec![]));
    }
    if target.world != vehicle.world {
        return Err(CommandError::new(
            "commands.ride.mount.failure.wrong_dimension",
            vec![],
        ));
    }
    let dimension = dimension_of(env.global, env.world);
    if !mount(env.level, env.global, dimension, t, v) {
        return Err(CommandError::new(
            "commands.ride.mount.failure.generic",
            vec![
                component(target.display(env.global)),
                component(vehicle.display(env.global)),
            ],
        ));
    }
    source.send_success(
        env.global,
        Component::translatable(
            "commands.ride.mount.success",
            vec![target.display(env.global), vehicle.display(env.global)],
        ),
        true,
    );
    Ok(1)
}

/// Why `RideCommand.mount` refuses to put `target` on `vehicle` of the same world (the
/// translation key): players carry nobody and no entity rides itself or its passengers.
pub(crate) fn mount_refusal(
    entities: &Entities,
    target: EntityId,
    vehicle: EntityId,
) -> Option<&'static str> {
    if entities.player(vehicle).is_some() {
        return Some("commands.ride.mount.failure.cant_ride_players");
    }
    // `getSelfAndPassengers().anyMatch(e -> e == vehicle)`.
    let mut stack = vec![target];
    while let Some(e) = stack.pop() {
        if e == vehicle {
            return Some("commands.ride.mount.failure.loop");
        }
        stack.extend_from_slice(entities.riding.passengers_of(e));
    }
    None
}

/// `startRiding(vehicle, true)` of `RideCommand.mount` (it leaves an old vehicle first);
/// returns whether it got on.
pub(crate) fn mount(
    level: &mut vanillite_game::level::Level,
    global: &Global,
    dimension: &str,
    target: EntityId,
    vehicle: EntityId,
) -> bool {
    WorldSurvival::with_entities(level, global, dimension, |_, ents, e, _| {
        vanillite_game::riding::start_riding(ents, e, target, vehicle, true)
    })
}

/// `stopRiding` of `RideCommand.dismount`.
pub(crate) fn dismount(
    level: &mut vanillite_game::level::Level,
    global: &Global,
    dimension: &str,
    target: EntityId,
) {
    WorldSurvival::with_entities(level, global, dimension, |_, ents, e, _| {
        vanillite_game::riding::stop_riding(ents, e, target);
    });
}

/// `hurtServer` of a player, a mob, a boss part, a decoration or a vehicle; other entities
/// take no damage here.
pub(crate) fn hurt_any(
    level: &mut vanillite_game::level::Level,
    global: &Global,
    dimension: &str,
    id: EntityId,
    source: DamageSource,
    amount: f32,
) -> bool {
    if level.entities.player(id).is_none() {
        return WorldSurvival::with_entities(level, global, dimension, |level, ents, e, cx| {
            vanillite_game::mob::combat::hurt(level, ents, e, cx, id, source, amount)
        });
    }
    // `resolvePlayerResponsibleForDamage`: a causing player.
    let mut source = source;
    source.responsible_player = source.cause.filter(|&c| level.entities.player(c).is_some());
    let mut events = Vec::new();
    // The level random, as in the world's tick.
    let mut random = level.lend_random();
    let hurt = WorldSurvival::hurt(
        &mut level.entities,
        Some(&level.chunks),
        &mut random,
        global,
        dimension,
        id,
        source,
        amount,
        &mut events,
    );
    level.restore_random(random);
    for (event, pos, context) in events {
        level.game_event(event, pos, context);
    }
    hurt
}

/// `Entity.kill`: living entities take `genericKill` damage, the others are removed; the
/// dragon and end crystals tell the End fight (their own `kill`).
pub(crate) fn kill_any(
    level: &mut vanillite_game::level::Level,
    global: &Global,
    dimension: &str,
    id: EntityId,
) {
    if level.entities.player(id).is_some() {
        let mut events = Vec::new();
        let mut random = level.lend_random();
        WorldSurvival::kill(
            &mut level.entities,
            Some(&level.chunks),
            &mut random,
            global,
            dimension,
            id,
            &mut events,
        );
        level.restore_random(random);
        for (event, pos, context) in events {
            level.game_event(event, pos, context);
        }
    } else if level
        .entities
        .mob(id)
        .is_some_and(|m| m.kind == vanillite_game::mob::Kind::EnderDragon)
    {
        WorldSurvival::with_entities(level, global, dimension, |_, ents, _, cx| {
            vanillite_game::mob::boss::dragon::kill(ents, cx, id)
        });
    } else if level.entities.mob(id).is_some() {
        let source = DamageSource::of(global.level.damage_types.generic_kill);
        hurt_any(level, global, dimension, id, source, f32::MAX);
    } else if !WorldSurvival::with_entities(level, global, dimension, |_, ents, e, cx| {
        vanillite_game::mob::boss::dragon::crystal::kill_crystal(ents, e, cx, id)
    }) {
        level.entities.despawn(id);
    }
}

/// Runs `f` on the level of another world on its thread, without waiting.
pub(crate) fn on_level(
    env: &Env<'_>,
    world: crate::world::WorldId,
    f: impl FnOnce(&mut vanillite_game::level::Level, &Global, &str) + Send + 'static,
) {
    let Some(global) = env.global.arc() else {
        return;
    };
    env.global.run_in_world(world, move |w| {
        let dimension = w.dimension().to_owned();
        f(w.level_mut(), &global, &dimension);
    });
}

/// `RideCommand.dismount`.
fn ride_dismount(
    source: &CommandSource,
    env: &mut Env<'_>,
    target: &Target,
) -> Result<i32, CommandError> {
    let Some(t) = target.entity else {
        return Err(CommandError::new(
            "commands.ride.not_riding",
            vec![component(target.display(env.global))],
        ));
    };
    let Some(vehicle) = env.level.entities.vehicle_of(t) else {
        return Err(CommandError::new(
            "commands.ride.not_riding",
            vec![component(target.display(env.global))],
        ));
    };
    let vehicle_name = display_of(env, vehicle).unwrap_or_else(|| Component::text(""));
    let dimension = dimension_of(env.global, env.world);
    dismount(env.level, env.global, dimension, t);
    source.send_success(
        env.global,
        Component::translatable(
            "commands.ride.dismount.success",
            vec![target.display(env.global), vehicle_name],
        ),
        true,
    );
    Ok(1)
}

/// How `rotate` turns an entity.
#[derive(Clone, Copy, Debug)]
enum Turn {
    /// `forceSetRotation` with the rotation from the source, each axis maybe relative.
    Angles {
        y_rot: f32,
        relative_y: bool,
        x_rot: f32,
        relative_x: bool,
    },
    /// `lookAt(anchor, pos)`; players also get `player_look_at` (towards an entity when
    /// given: its network id and anchor).
    Face {
        from: Anchor,
        at: Vec3,
        entity: Option<LookAtEntity>,
    },
}

/// `Mth.wrapDegrees(float)`.
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

fn anchor_id(anchor: Anchor) -> i32 {
    match anchor {
        Anchor::Feet => 0,
        Anchor::Eyes => 1,
    }
}

/// `Entity.forceSetRotation` / `Entity.lookAt`, with `ServerPlayer`'s packets.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn turn(entities: &mut Entities, id: EntityId, turn: Turn) {
    let Some(entity) = entities.store.get(id) else {
        return;
    };
    let current = entity.rot();
    let pos = entity.pos();
    let eye_height = f64::from(entity.kind().info().eye_height);
    let (y_rot, x_rot, packet) = match turn {
        Turn::Angles {
            y_rot,
            relative_y,
            x_rot,
            relative_x,
        } => {
            let y = if relative_y {
                y_rot - current.y_rot
            } else {
                y_rot
            };
            let x = if relative_x {
                x_rot - current.x_rot
            } else {
                x_rot
            };
            let abs_y = if relative_y { current.y_rot + y } else { y };
            let abs_x = (if relative_x { current.x_rot + x } else { x }).clamp(-90.0, 90.0);
            let packet = SharedPacket::of(&PlayerRotation {
                y_rot: y,
                relative_y,
                x_rot: x,
                relative_x,
            });
            (abs_y, abs_x, packet)
        }
        Turn::Face { from, at, entity } => {
            let origin = match from {
                Anchor::Eyes => Vec3::new(pos.x, pos.y + eye_height, pos.z),
                Anchor::Feet => pos,
            };
            let d = at - origin;
            let horizontal = (d.x * d.x + d.z * d.z).sqrt();
            let x = wrap_degrees((-(d.y.atan2(horizontal) * 180.0 / std::f64::consts::PI)) as f32);
            let y = wrap_degrees((d.z.atan2(d.x) * 180.0 / std::f64::consts::PI) as f32 - 90.0);
            let packet = SharedPacket::of(&PlayerLookAt {
                from_anchor: anchor_id(from),
                x: at.x,
                y: at.y,
                z: at.z,
                entity,
            });
            (y, x, packet)
        }
    };
    if !(y_rot.is_finite() && x_rot.is_finite()) {
        return;
    }
    if let Some(mut e) = entities.store.get_mut(id) {
        e.set_rot(Rotation {
            y_rot,
            // `setXRot`.
            x_rot: (x_rot % 360.0).clamp(-90.0, 90.0),
            y_head_rot: y_rot,
        });
    }
    if let Some(player) = entities.player(id) {
        player.connection.sink.send_shared(&packet);
    }
}

/// `RotateCommand.rotate`.
fn rotate(args: &Args, source: &CommandSource, env: &mut Env<'_>) -> Result<i32, CommandError> {
    let target = select::single(env, source, selector(args, "target")?)?;
    let how = if let Some(rotation) = args.coordinates("rotation") {
        let (y_rot, x_rot) = resolve_rotation(source, rotation);
        let (relative_x, relative_y) = match rotation {
            Coordinates::World([x, y, _]) => (x.relative, y.relative),
            Coordinates::Local(_) => (false, false),
        };
        Turn::Angles {
            y_rot,
            relative_y,
            x_rot,
            relative_x,
        }
    } else if let Some(location) = args.coordinates("facingLocation") {
        Turn::Face {
            from: source.anchor,
            at: resolve_position(source, location),
            entity: None,
        }
    } else {
        let facing = select::single(env, source, selector(args, "facingEntity")?)?;
        let anchor = args.anchor("facingAnchor").copied().unwrap_or(Anchor::Feet);
        let (pos, _, _) = facing
            .place
            .ok_or_else(|| CommandError::new("argument.entity.notfound.entity", vec![]))?;
        let at = match anchor {
            Anchor::Eyes => Vec3::new(
                pos.x,
                pos.y + f64::from(facing.kind.info().eye_height),
                pos.z,
            ),
            Anchor::Feet => pos,
        };
        let entity = facing
            .entity
            .and_then(|id| env.level.entities.store.get(id))
            .map(|e| LookAtEntity {
                entity_id: e.network_id().0,
                anchor: anchor_id(anchor),
            });
        Turn::Face {
            from: source.anchor,
            at,
            entity,
        }
    };
    on_entity(env, &target, move |entities, id, _, _| {
        turn(entities, id, how)
    });
    source.send_success(
        env.global,
        Component::translatable("commands.rotate.success", vec![target.display(env.global)]),
        true,
    );
    Ok(1)
}

/// `DamageCommand.damage`: `hurtServer` of the target. Targets of other worlds are hurt on
/// their world's thread and count as hurt.
fn damage(args: &Args, source: &CommandSource, env: &mut Env<'_>) -> Result<i32, CommandError> {
    let target = select::single(env, source, selector(args, "target")?)?;
    let amount = *args.float("amount").ok_or_else(|| missing("amount"))?;
    let types = &env.global.level.damage_types;
    let kind = match args.identifier("damageType") {
        Some(id) => types.kind(id).ok_or_else(|| {
            CommandError::new(
                "argument.resource.not_found",
                vec![
                    ErrArg::from(id.as_str()),
                    ErrArg::from("minecraft:damage_type"),
                ],
            )
        })?,
        None => types.generic,
    };
    let entity = match args.selector("entity") {
        Some(s) => Some(select::single(env, source, s)?),
        None => None,
    };
    let cause = match args.selector("cause") {
        Some(s) => Some(select::single(env, source, s)?),
        None => None,
    };
    let local = |t: &Option<Target>| t.as_ref().and_then(|t| t.entity);
    // The causing mob's kind (difficulty scaling, the death message's killer).
    let cause_kind = cause
        .as_ref()
        .or(entity.as_ref())
        .and_then(|t| vanillite_game::mob::Kind::by_name(t.kind.name()));
    let damage_source = DamageSource {
        kind,
        // `new DamageSource(type, direct)` makes the direct entity the cause too.
        cause: local(&cause).or(local(&entity)),
        direct: local(&entity),
        // `getSourcePosition`: the location, else the direct entity's position.
        position: args
            .coordinates("location")
            .map(|c| resolve_position(source, c))
            .or_else(|| entity.as_ref().and_then(|t| t.place).map(|(pos, _, _)| pos)),
        cause_is_mob: cause_kind.is_some(),
        cause_name: cause_kind.map(vanillite_game::mob::Kind::translation_key),
        disable_shield_seconds: 0.0,
        piercing_arrow: false,
        projectile_motion: None,
        responsible_player: None,
        own_blocked_by_item: false,
    };
    let hurt = match target.entity {
        Some(id) => {
            let dimension = env.global.dimensions.name_or_overworld(target.world);
            hurt_any(env.level, env.global, dimension, id, damage_source, amount)
        }
        None => {
            // Entity ids of this world mean nothing in the target's world.
            let remote = DamageSource {
                cause: None,
                direct: None,
                ..damage_source
            };
            let uuid = target.uuid;
            on_level(env, target.world, move |level, global, dimension| {
                if let Some(id) = select::find_uuid(&level.entities, uuid) {
                    hurt_any(level, global, dimension, id, remote, amount);
                }
            });
            true
        }
    };
    if !hurt {
        return Err(CommandError::new("commands.damage.invulnerable", vec![]));
    }
    source.send_success(
        env.global,
        Component::translatable_args(
            "commands.damage.success",
            vec![Arg::Float(amount), Arg::from(target.display(env.global))],
        ),
        true,
    );
    Ok(1)
}

/// `SwingCommand.swing`: living entities swing (`LivingEntity.swing(hand, animation, true)`).
fn swing(
    offhand: bool,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let targets = match args.selector("targets") {
        Some(s) => select::many(env, source, s)?,
        None => vec![own_entity(source, env)?],
    };
    // `SwingAnimation.DEFAULT`: whack for 6 ticks.
    let kind = match args.string("animation").map(String::as_str) {
        Some("none") => 0,
        Some("stab") => 2,
        _ => 1,
    };
    let duration = args.time("duration").copied().unwrap_or(6);
    let hand = if offhand {
        Hand::OffHand
    } else {
        Hand::MainHand
    };
    let mut tracker = Tracker::default();
    for target in targets {
        // Living entities are the kinds with attributes.
        if target.kind.info().attributes.is_empty() {
            continue;
        }
        on_entity(env, &target, move |entities, id, _, _| {
            let Some(entity_id) = entities.store.get(id).map(|e| e.network_id().0) else {
                return;
            };
            let packet = SharedPacket::of(&SwingAnimation {
                entity_id,
                hand,
                kind,
                duration,
            });
            send_to_viewers(entities, id, true, &packet);
        });
        tracker.track(&target, i32::from(true));
    }
    tracker.messages(
        source,
        env.global,
        || CommandError::new("commands.swing.failed.notliving", vec![]),
        |entity| {
            Component::translatable(
                "commands.swing.success.single",
                vec![entity.display(env.global)],
            )
        },
        |count| {
            Component::translatable_args("commands.swing.success.multiple", vec![Arg::Int(count)])
        },
    )
}
