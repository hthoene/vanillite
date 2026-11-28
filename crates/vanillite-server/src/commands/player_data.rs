//! `/experience`, `/recipe` and `/attribute` (Vanilla's `ExperienceCommand`, `RecipeCommand`,
//! `AttributeCommand`).

use vanillite_command::error::CommandError;
use vanillite_command::{Arg as ErrArg, Args};
use vanillite_data::registry::ATTRIBUTE;
use vanillite_game::commands::{AttributeOp, ExperienceOp};
use vanillite_game::level::Level;
use vanillite_protocol::text::{Arg, Component};
use vanillite_world::entity::EntityId;

use super::Env;
use super::items::on_player;
use super::select::{self, Target};
use super::source::CommandSource;
use super::tracker::Tracker;

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

fn error(key: &'static str, args: Vec<ErrArg>) -> CommandError {
    CommandError::new(key, args)
}

fn many(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Vec<Target>, CommandError> {
    let selector = args.selector(name).ok_or_else(|| missing(name))?;
    select::many(env, source, selector)
}

fn single(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Target, CommandError> {
    let selector = args.selector(name).ok_or_else(|| missing(name))?;
    select::single(env, source, selector)
}

// --- experience ---------------------------------------------------------------------------------

/// `giveExperiencePoints` / `giveExperienceLevels` of `ServerPlayer`.
pub(crate) fn add_experience(level: &mut Level, id: EntityId, amount: i32, levels: bool) {
    let Some(player) = level.entities.player_mut(id) else {
        return;
    };
    if amount == 0 {
        return;
    }
    let s = &mut player.survival;
    let volume = if levels {
        s.xp.give_levels(amount);
        // `giveExperienceLevels`: the level-up sound every fifth level, at most once per 100
        // ticks.
        #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
        let due = amount > 0
            && s.xp.level % 5 == 0
            && (s.last_level_up_time as f32) < s.tick_count as f32 - 100.0;
        due.then(|| {
            s.last_level_up_time = s.tick_count;
            #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
            let v = if s.xp.level > 30 {
                1.0
            } else {
                s.xp.level as f32 / 30.0
            };
            v * 0.75
        })
    } else {
        s.give_experience_points(amount)
    };
    s.resend_experience();
    if let Some(volume) = volume
        && let Some(pos) = level.entities.store.get(id).map(|e| e.pos())
    {
        level.play_sound(
            None,
            pos.x,
            pos.y,
            pos.z,
            vanillite_game::block::sound_id("entity.player.levelup"),
            vanillite_game::level::SOUND_PLAYERS,
            volume,
            1.0,
        );
    }
}

/// `ExperienceCommand.Type.set`: points must stay below the next level.
#[allow(clippy::as_conversions, clippy::cast_precision_loss)]
fn set_experience(level: &mut Level, id: EntityId, amount: i32, levels: bool) -> bool {
    let Some(player) = level.entities.player_mut(id) else {
        return false;
    };
    let s = &mut player.survival;
    if levels {
        if amount != s.xp.level {
            s.xp.level = amount;
            s.resend_experience();
        }
        return true;
    }
    let needed = s.xp.needed_for_next_level();
    if amount >= needed {
        return false;
    }
    let limit = needed as f32;
    let progress = (amount as f32 / limit).clamp(0.0, (limit - 1.0) / limit);
    if progress != s.xp.progress {
        s.xp.progress = progress;
        s.resend_experience();
    }
    true
}

/// `/experience (add|set|query) ...`. Players in other worlds are changed there and count as
/// changed; they cannot be queried.
#[allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
pub(crate) fn experience(
    op: ExperienceOp,
    levels: bool,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let unit = if levels { "levels" } else { "points" };
    if op == ExperienceOp::Query {
        let target = single(args, "target", source, env)?;
        let player = target
            .entity
            .and_then(|id| env.level.entities.player(id))
            .ok_or_else(|| error("command.failed", vec![]))?;
        let xp = &player.survival.xp;
        let result = if levels {
            xp.level
        } else {
            (xp.progress * xp.needed_for_next_level() as f32).floor() as i32
        };
        source.send_success(
            env.global,
            Component::translatable_args(
                format!("commands.experience.query.{unit}"),
                vec![Arg::from(target.display()), Arg::Int(result)],
            ),
            false,
        );
        return Ok(result);
    }
    let players = many(args, "target", source, env)?;
    let amount = *args.int("amount").ok_or_else(|| missing("amount"))?;
    let mut tracker = Tracker::<Component>::default();
    for player in &players {
        let changed = if op == ExperienceOp::Add {
            on_player(env, player, move |level, id| {
                add_experience(level, id, amount, levels);
            });
            true
        } else {
            match player.entity {
                Some(id) => set_experience(env.level, id, amount, levels),
                None => {
                    on_player(env, player, move |level, id| {
                        set_experience(level, id, amount, levels);
                    });
                    true
                }
            }
        };
        tracker.track(&player.display(), i32::from(changed));
    }
    let (action, on_zero) = match op {
        ExperienceOp::Add => ("add", None),
        _ => (
            "set",
            Some(error("commands.experience.set.points.invalid", vec![])),
        ),
    };
    tracker.feedback(source, env, true, false, on_zero, |one, n, _| match one {
        Some(player) => Component::translatable_args(
            format!("commands.experience.{action}.{unit}.success.single"),
            vec![Arg::Int(amount), Arg::from(player.clone())],
        ),
        None => Component::translatable_args(
            format!("commands.experience.{action}.{unit}.success.multiple"),
            vec![Arg::Int(amount), Arg::Int(n)],
        ),
    })
}

// --- recipe -------------------------------------------------------------------------------------

/// `/recipe (give|take) <targets> (<recipe>|*)`. Players have no recipe book yet, so nothing
/// is awarded or taken and the command fails like Vanilla when nothing changed.
pub(crate) fn recipe(
    give: bool,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let players = many(args, "targets", source, env)?;
    let game = env.global.game();
    let ids: Vec<String> = match args.identifier("recipe") {
        Some(id) => {
            if !game.recipes.book.contains(id) {
                return Err(error("recipe.notFound", vec![ErrArg::Str(id.clone())]));
            }
            vec![id.clone()]
        }
        // `*`: every recipe.
        None => game.recipes.book.recipe_ids().map(str::to_owned).collect(),
    };
    let mut tracker = Tracker::<Component>::default();
    for player in &players {
        let changed = match player
            .entity
            .and_then(|id| env.level.entities.player_mut(id))
        {
            Some(p) => {
                let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                if give {
                    p.unlock_recipes(&refs)
                } else {
                    p.lock_recipes(&refs)
                }
            }
            None => {
                let list = ids.clone();
                super::items::on_player(env, player, move |level, id| {
                    if let Some(p) = level.entities.player_mut(id) {
                        let refs: Vec<&str> = list.iter().map(String::as_str).collect();
                        if give {
                            p.unlock_recipes(&refs);
                        } else {
                            p.lock_recipes(&refs);
                        }
                    }
                });
                ids.len()
            }
        };
        tracker.track(
            &player.display(),
            i32::try_from(changed).unwrap_or(i32::MAX),
        );
    }
    let action = if give { "give" } else { "take" };
    let failed = if give {
        "commands.recipe.give.failed"
    } else {
        "commands.recipe.take.failed"
    };
    tracker.feedback(
        source,
        env,
        true,
        false,
        Some(error(failed, vec![])),
        |one, n, total| match one {
            Some(player) => Component::translatable_args(
                format!("commands.recipe.{action}.success.single"),
                vec![Arg::Int(total), Arg::from(player.clone())],
            ),
            None => Component::translatable_args(
                format!("commands.recipe.{action}.success.multiple"),
                vec![Arg::Int(total), Arg::Int(n)],
            ),
        },
    )
}

// --- attribute ----------------------------------------------------------------------------------

/// An attribute change run in the world of a target in another world.
type RemoteChange = Box<dyn FnOnce(&mut vanillite_game::entities::Entities, EntityId) + Send>;

/// `/attribute <target> <attribute> ...` (`AttributeCommand`) over the entity's attribute map
/// ([`vanillite_game::effect::attribute_map`]). A target in another world is changed there;
/// its values read as the type's defaults.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
pub(crate) fn attribute(
    op: AttributeOp,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    use vanillite_game::effect::Operation;
    use vanillite_game::effect::attribute::Attribute;
    use vanillite_game::effect::attribute_map::{self as map, AttributeError};
    let target = single(args, "target", source, env)?;
    let name = args
        .identifier("attribute")
        .ok_or_else(|| missing("attribute"))?;
    let display = target.display();
    let description = Component::translatable(
        format!("attribute.name.{}", name.rsplit(':').next().unwrap_or("")),
        vec![],
    );
    let id_arg = || args.identifier("id").cloned().ok_or_else(|| missing("id"));
    let fail = |e: AttributeError, id: Option<&str>| -> CommandError {
        let target = || ErrArg::Component(Box::new(display.clone()));
        let attr = || ErrArg::Component(Box::new(description.clone()));
        let id = || ErrArg::Str(id.unwrap_or_default().to_owned());
        match e {
            AttributeError::NotLiving => error("commands.attribute.failed.entity", vec![target()]),
            AttributeError::NoAttribute => error(
                "commands.attribute.failed.no_attribute",
                vec![target(), attr()],
            ),
            AttributeError::NoModifier => error(
                "commands.attribute.failed.no_modifier",
                vec![attr(), target(), id()],
            ),
            AttributeError::ModifierPresent => error(
                "commands.attribute.failed.modifier_already_present",
                vec![id(), attr(), target()],
            ),
            AttributeError::Refused => error("command.failed", vec![]),
        }
    };
    let living = !target.kind.info().attributes.is_empty();
    if !living {
        return Err(fail(AttributeError::NotLiving, None));
    }
    let Some(attribute) = ATTRIBUTE
        .id(name)
        .and_then(|i| u8::try_from(i).ok())
        .map(Attribute)
        .filter(|&a| map::has_attribute(target.kind, a))
    else {
        return Err(fail(AttributeError::NoAttribute, None));
    };
    let send = |env: &Env<'_>, key: &str, args: Vec<Arg>| {
        source.send_success(env.global, Component::translatable_args(key, args), false);
    };
    let on = || Arg::from(display.clone());
    let attr = || Arg::from(description.clone());
    let scale = args.double("scale").copied().unwrap_or(1.0);
    let local = target.entity;
    // A target in another world: changes run there, reads see the type's defaults.
    let remote = |env: &mut Env<'_>, f: RemoteChange| {
        on_player(env, &target, move |level, id| f(&mut level.entities, id));
    };
    match op {
        AttributeOp::Get | AttributeOp::BaseGet => {
            let value = match local {
                Some(id) if op == AttributeOp::Get => {
                    map::value(&env.level.entities, id, attribute)
                }
                Some(id) => map::base(&env.level.entities, id, attribute),
                None => Ok(attribute.base_of(target.kind)),
            }
            .map_err(|e| fail(e, None))?;
            let key = if op == AttributeOp::Get {
                "commands.attribute.value.get.success"
            } else {
                "commands.attribute.base_value.get.success"
            };
            send(env, key, vec![attr(), on(), Arg::Double(value)]);
            Ok((value * scale) as i32)
        }
        AttributeOp::BaseSet => {
            let value = args
                .double("value")
                .copied()
                .ok_or_else(|| missing("value"))?;
            match local {
                Some(id) => map::set_base(&mut env.level.entities, id, attribute, value, true)
                    .map_err(|e| fail(e, None))?,
                None => remote(
                    env,
                    Box::new(move |ents, id| {
                        let _ = map::set_base(ents, id, attribute, value, true);
                    }),
                ),
            }
            send(
                env,
                "commands.attribute.base_value.set.success",
                vec![attr(), on(), Arg::Double(value)],
            );
            Ok(1)
        }
        AttributeOp::BaseReset => {
            let value = match local {
                Some(id) => map::reset_base(&mut env.level.entities, id, attribute, true)
                    .map_err(|e| fail(e, None))?,
                None => {
                    remote(
                        env,
                        Box::new(move |ents, id| {
                            let _ = map::reset_base(ents, id, attribute, true);
                        }),
                    );
                    attribute.base_of(target.kind)
                }
            };
            send(
                env,
                "commands.attribute.base_value.reset.success",
                vec![attr(), on(), Arg::Double(value)],
            );
            Ok(1)
        }
        AttributeOp::ModifierAdd(operation) => {
            let id = id_arg()?;
            let amount = args
                .double("value")
                .copied()
                .ok_or_else(|| missing("value"))?;
            let operation = match operation {
                vanillite_game::commands::ModifierOperation::AddValue => Operation::AddValue,
                vanillite_game::commands::ModifierOperation::AddMultipliedBase => {
                    Operation::AddMultipliedBase
                }
                vanillite_game::commands::ModifierOperation::AddMultipliedTotal => {
                    Operation::AddMultipliedTotal
                }
            };
            match local {
                Some(e) => map::add_modifier(
                    &mut env.level.entities,
                    e,
                    attribute,
                    &id,
                    amount,
                    operation,
                    true,
                )
                .map_err(|err| fail(err, Some(&id)))?,
                None => {
                    let id = id.clone();
                    remote(
                        env,
                        Box::new(move |ents, e| {
                            let _ =
                                map::add_modifier(ents, e, attribute, &id, amount, operation, true);
                        }),
                    );
                }
            }
            send(
                env,
                "commands.attribute.modifier.add.success",
                vec![Arg::String(id.clone()), attr(), on()],
            );
            Ok(1)
        }
        AttributeOp::ModifierRemove => {
            let id = id_arg()?;
            match local {
                Some(e) => map::remove_modifier(&mut env.level.entities, e, attribute, &id, true)
                    .map_err(|err| fail(err, Some(&id)))?,
                None => return Err(fail(AttributeError::NoModifier, Some(&id))),
            }
            send(
                env,
                "commands.attribute.modifier.remove.success",
                vec![Arg::String(id.clone()), attr(), on()],
            );
            Ok(1)
        }
        AttributeOp::ModifierValueGet => {
            let id = id_arg()?;
            let value = match local {
                Some(e) => map::modifier(&env.level.entities, e, attribute, &id)
                    .map_err(|err| fail(err, Some(&id)))?,
                None => return Err(fail(AttributeError::NoModifier, Some(&id))),
            };
            send(
                env,
                "commands.attribute.modifier.value.get.success",
                vec![Arg::String(id.clone()), attr(), on(), Arg::Double(value)],
            );
            Ok((value * scale) as i32)
        }
    }
}
