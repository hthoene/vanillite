//! `/particle` (Vanilla's `ParticleCommand`) and the particle options: the SNBT options of the
//! `particle` argument read like the types' map codecs and written like their stream codecs.

use vanillite_base::Vec3;
use vanillite_command::error::{Arg as ErrArg, CommandError};
use vanillite_command::{Args, GameValue};
use vanillite_nbt::Compound;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{LevelParticles, Particle};
use vanillite_protocol::text::{Arg, Component};

use super::Env;
use super::exec::resolve_position;
use super::select;
use super::source::CommandSource;
use crate::player_list::Uuid;

/// `ParticleArgument.readParticle` ([`vanillite_game::level::particle_options::from_options`]).
pub(crate) fn particle(id: &str, given: Option<&Compound>) -> Result<Particle, CommandError> {
    use vanillite_game::level::particle_options::{OptionsError, from_options};
    from_options(id, given).map_err(|e| match e {
        OptionsError::NotFound => CommandError::new("particle.notFound", vec![ErrArg::from(id)]),
        OptionsError::Invalid(message) => {
            CommandError::new("particle.invalidOptions", vec![ErrArg::Str(message)])
        }
    })
}

/// `ParticleCommand.sendParticles` with `ServerLevel.sendParticles(player, ...)`: to each
/// viewer in the source's world whose block centre is closer than 32 blocks (512 with
/// `force`); the count of viewers reached.
pub(crate) fn run(
    force: bool,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let Some(GameValue::Particle(arg)) = args.game("name") else {
        return Err(CommandError::new("particle.notFound", vec![]));
    };
    let particle = particle(&arg.id, arg.options.as_ref())?;
    let pos = args
        .coordinates("pos")
        .map_or(source.pos, |c| resolve_position(source, c));
    let delta = args
        .coordinates("delta")
        .map_or(Vec3::new(0.0, 0.0, 0.0), |c| resolve_position(source, c));
    let speed = args.float("speed").copied().unwrap_or(0.0);
    let count = args.int("count").copied().unwrap_or(0);
    let viewers = match args.selector("viewers") {
        Some(selector) => select::many(env, source, selector)?,
        None => select::online_players(&env.level.entities, env.global, source.world),
    };
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let packet = SharedPacket::of(&LevelParticles {
        particle,
        override_limiter: force,
        always_show: false,
        x: pos.x,
        y: pos.y,
        z: pos.z,
        x_dist: delta.x as f32,
        y_dist: delta.y as f32,
        z_dist: delta.z as f32,
        x_max_speed: speed,
        y_max_speed: speed,
        z_max_speed: speed,
        count,
        randomization: 0,
    });
    let range: f64 = if force { 512.0 } else { 32.0 };
    let mut reached = 0i32;
    for viewer in &viewers {
        let Some((at, _, _)) = viewer.place.filter(|_| viewer.world == source.world) else {
            continue;
        };
        // `BlockPos.closerToCenterThan`.
        let center = Vec3::new(at.x.floor() + 0.5, at.y.floor() + 0.5, at.z.floor() + 0.5);
        let d = center - pos;
        if d.x * d.x + d.y * d.y + d.z * d.z >= range * range {
            continue;
        }
        if let Some((sink, _)) = env.global.players.connection(Uuid(viewer.uuid)) {
            sink.send_shared(&packet);
            reached = reached.saturating_add(1);
        }
    }
    if reached == 0 {
        return Err(CommandError::new("commands.particle.failed", vec![]));
    }
    source.send_success(
        env.global,
        Component::translatable_args(
            "commands.particle.success",
            vec![Arg::String(arg.id.clone())],
        ),
        true,
    );
    Ok(reached)
}
