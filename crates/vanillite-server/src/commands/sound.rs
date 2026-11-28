//! The client effect commands: `playsound`, `stopsound` and `posteffect` (Vanilla's
//! `PlaySoundCommand`, `StopSoundCommand` and `PostEffectCommand`).

use vanillite_base::RandomSource as _;
use vanillite_command::Args;
use vanillite_command::error::CommandError;
use vanillite_game::commands::{PostEffectOp, SOUND_SOURCES};
use vanillite_game::player::PlayerData;
use vanillite_protocol::packets::play::clientbound::{
    PostEffects, SoundDirect, SoundSource, StopSound,
};
use vanillite_protocol::text::{Arg, Component};
use vanillite_protocol::types::Identifier;

use super::Env;
use super::exec::resolve_position;
use super::select::{self, Target};
use super::source::CommandSource;
use super::tracker::Tracker;
use crate::player_list::Uuid;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![vanillite_command::Arg::Str(format!(
            "missing argument {name}"
        ))],
    )
}

/// The `SoundSource` of an index into [`SOUND_SOURCES`].
pub(crate) fn sound_source(index: u8) -> SoundSource {
    match index {
        1 => SoundSource::Music,
        2 => SoundSource::Records,
        3 => SoundSource::Weather,
        4 => SoundSource::Blocks,
        5 => SoundSource::Hostile,
        6 => SoundSource::Neutral,
        7 => SoundSource::Players,
        8 => SoundSource::Ambient,
        9 => SoundSource::Voice,
        10 => SoundSource::Ui,
        _ => SoundSource::Master,
    }
}

/// An identifier argument as a protocol identifier.
fn identifier(args: &Args, name: &str) -> Result<Option<Identifier>, CommandError> {
    args.identifier(name)
        .map(|id| Identifier::new(id).map_err(|_| CommandError::new("argument.id.invalid", vec![])))
        .transpose()
}

/// `Component.translationArg(Identifier)`: the id as a literal.
fn literal(id: &Identifier) -> Arg {
    Arg::from(Component::text(id.to_string()))
}

/// The players a command acts on: `<targets>` or, without it, the source's own player
/// (`getCallingPlayerAsCollection`).
fn players(
    args: &Args,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Vec<Target>, CommandError> {
    if let Some(selector) = args.selector("targets") {
        return select::many(env, source, selector);
    }
    let Some(uuid) = source.uuid() else {
        return Ok(Vec::new());
    };
    Ok(
        select::online_players(&env.level.entities, env.global, source.world)
            .into_iter()
            .filter(|p| p.uuid == uuid)
            .collect(),
    )
}

/// `PlaySoundCommand.playSound`: a variable range sound event by id to each target in the
/// source's world within the sound's range, or at `minVolume` two blocks away from the
/// targets beyond it.
pub(crate) fn play_sound(
    index: u8,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let sound = identifier(args, "sound")?.ok_or_else(|| missing("sound"))?;
    let targets = players(args, source, env)?;
    let pos = args
        .coordinates("pos")
        .map_or(source.pos, |c| resolve_position(source, c));
    let volume = args.float("volume").copied().unwrap_or(1.0);
    let pitch = args.float("pitch").copied().unwrap_or(1.0);
    let min_volume = args.float("minVolume").copied().unwrap_or(0.0);
    // `SoundEvent.getRange(volume)` of a variable range event, squared as a float.
    let range = if volume > 1.0 { 16.0 * volume } else { 16.0 };
    let max_dist_sqr = f64::from(range * range);
    let seed = env.level.random().next_long();
    let mut tracker = Tracker::default();
    for player in targets {
        let Some((at, _, _)) = player.place.filter(|_| player.world == source.world) else {
            continue;
        };
        let (dx, dy, dz) = (pos.x - at.x, pos.y - at.y, pos.z - at.z);
        let dist_sqr = dx * dx + dy * dy + dz * dz;
        let (mut local, mut local_volume) = (pos, volume);
        if dist_sqr > max_dist_sqr {
            if min_volume <= 0.0 {
                continue;
            }
            let distance = dist_sqr.sqrt();
            local = vanillite_base::Vec3::new(
                at.x + dx / distance * 2.0,
                at.y + dy / distance * 2.0,
                at.z + dz / distance * 2.0,
            );
            local_volume = min_volume;
        }
        // `(int)(x * 8.0)`: Java's saturating cast.
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let eighths = |v: f64| (v * 8.0) as i32;
        let packet = SoundDirect {
            sound: sound.clone(),
            fixed_range: None,
            source: sound_source(index),
            x: eighths(local.x),
            y: eighths(local.y),
            z: eighths(local.z),
            volume: local_volume,
            pitch,
            seed,
        };
        if let Some((sink, _)) = env.global.players.connection(Uuid(player.uuid)) {
            sink.send(&packet);
        }
        tracker.track(&player, i32::from(true));
    }
    tracker.messages(
        source,
        env.global,
        || CommandError::new("commands.playsound.failed", vec![]),
        |player| {
            Component::translatable_args(
                "commands.playsound.success.single",
                vec![literal(&sound), Arg::from(player.display(env.global))],
            )
        },
        |count| {
            Component::translatable_args(
                "commands.playsound.success.multiple",
                vec![literal(&sound), Arg::Int(count)],
            )
        },
    )
}

/// `StopSoundCommand.stopSound`: the packet to every target, wherever it is.
pub(crate) fn stop_sound(
    index: Option<u8>,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let selector = args.selector("targets").ok_or_else(|| missing("targets"))?;
    let targets = select::many(env, source, selector)?;
    let sound = identifier(args, "sound")?;
    let packet = StopSound {
        source: index.map(sound_source),
        sound: sound.clone(),
    };
    for target in &targets {
        if let Some((sink, _)) = env.global.players.connection(Uuid(target.uuid)) {
            sink.send(&packet);
        }
    }
    let name = |i: u8| {
        Arg::String(
            SOUND_SOURCES
                .get(usize::from(i))
                .copied()
                .unwrap_or("master")
                .to_owned(),
        )
    };
    let message = match (index, &sound) {
        (Some(i), Some(sound)) => Component::translatable_args(
            "commands.stopsound.success.source.sound",
            vec![literal(sound), name(i)],
        ),
        (Some(i), None) => {
            Component::translatable_args("commands.stopsound.success.source.any", vec![name(i)])
        }
        (None, Some(sound)) => Component::translatable_args(
            "commands.stopsound.success.sourceless.sound",
            vec![literal(sound)],
        ),
        (None, None) => {
            Component::translatable("commands.stopsound.success.sourceless.any", vec![])
        }
    };
    source.send_success(env.global, message, true);
    Ok(i32::try_from(targets.len()).unwrap_or(i32::MAX))
}

/// `ServerPlayer.sendPostEffects`.
pub(crate) fn send_post_effects(player: &PlayerData) {
    player.connection.sink.send(&PostEffects {
        effects: player
            .post_effects
            .iter()
            .filter_map(|e| Identifier::new(e).ok())
            .collect(),
    });
}

/// A change of a player's post effects (`addPostEffect`, `clearPostEffects`,
/// `removePostEffect`); whether it changed anything. The client gets the new list.
fn change_post_effects(player: &mut PlayerData, op: PostEffectOp, effect: &str) -> bool {
    let effects = &mut player.post_effects;
    let changed = match op {
        PostEffectOp::Add => {
            let new = !effects.iter().any(|e| e == effect);
            if new {
                effects.push(effect.to_owned());
            }
            new
        }
        PostEffectOp::Clear => {
            let any = !effects.is_empty();
            effects.clear();
            any
        }
        PostEffectOp::Remove | PostEffectOp::List => {
            match effects.iter().position(|e| e == effect) {
                Some(i) => {
                    effects.remove(i);
                    true
                }
                None => false,
            }
        }
    };
    if changed {
        send_post_effects(player);
    }
    changed
}

/// `PostEffectCommand`. Players of other worlds change on their world's thread and count as
/// changed; their list cannot be read from here.
pub(crate) fn post_effect(
    op: PostEffectOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let global = env.global;
    if op == PostEffectOp::List {
        let selector = args.selector("target").ok_or_else(|| missing("target"))?;
        let target = select::single(env, source, selector)?;
        let effects = target
            .entity
            .and_then(|id| env.level.entities.player(id))
            .map(|p| p.post_effects.clone())
            .ok_or_else(|| CommandError::new("argument.entity.notfound.player", vec![]))?;
        let message = if effects.is_empty() {
            Component::translatable(
                "commands.posteffect.list.empty",
                vec![target.display(env.global)],
            )
        } else {
            Component::translatable_args(
                "commands.posteffect.list.success",
                vec![
                    Arg::from(target.display(env.global)),
                    Arg::Int(i32::try_from(effects.len()).unwrap_or(i32::MAX)),
                    Arg::String(effects.join(", ")),
                ],
            )
        };
        source.send_success(global, message, false);
        return Ok(i32::try_from(effects.len()).unwrap_or(i32::MAX));
    }
    let selector = args.selector("targets").ok_or_else(|| missing("targets"))?;
    let targets = select::many(env, source, selector)?;
    let effect = identifier(args, "posteffect")?;
    let effect_id = effect.as_ref().map(ToString::to_string).unwrap_or_default();
    let mut tracker = Tracker::default();
    for target in targets {
        let changed = match target.entity {
            Some(id) => env
                .level
                .entities
                .player_mut(id)
                .is_some_and(|p| change_post_effects(p, op, &effect_id)),
            None => {
                let (uuid, effect) = (target.uuid, effect_id.clone());
                global.run_in_world(target.world, move |w| {
                    let entities = w.entities_mut();
                    if let Some(p) =
                        select::find_uuid(entities, uuid).and_then(|id| entities.player_mut(id))
                    {
                        change_post_effects(p, op, &effect);
                    }
                });
                true
            }
        };
        tracker.track(&target, i32::from(changed));
    }
    let effect_arg = || {
        effect
            .as_ref()
            .map(literal)
            .unwrap_or(Arg::String(String::new()))
    };
    let (failed, single, multiple) = match op {
        PostEffectOp::Add => (
            "commands.posteffect.add.failed",
            "commands.posteffect.add.success.single",
            "commands.posteffect.add.success.multiple",
        ),
        PostEffectOp::Clear => (
            "commands.posteffect.clear.failed",
            "commands.posteffect.clear.success.single",
            "commands.posteffect.clear.success.multiple",
        ),
        PostEffectOp::Remove | PostEffectOp::List => (
            "commands.posteffect.remove.failed",
            "commands.posteffect.remove.success.single",
            "commands.posteffect.remove.success.multiple",
        ),
    };
    let with_effect = op != PostEffectOp::Clear;
    tracker.messages(
        source,
        global,
        || CommandError::new(failed, vec![]),
        |player| {
            let mut args = Vec::new();
            if with_effect {
                args.push(effect_arg());
            }
            args.push(Arg::from(player.display(env.global)));
            Component::translatable_args(single, args)
        },
        |count| {
            let mut args = Vec::new();
            if with_effect {
                args.push(effect_arg());
            }
            args.push(Arg::Int(count));
            Component::translatable_args(multiple, args)
        },
    )
}
