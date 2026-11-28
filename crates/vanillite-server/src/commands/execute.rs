//! The subcommands of `execute` (Vanilla's `ExecuteCommand`): the redirect modifiers that
//! change or fork the source, `store` callbacks, and the conditions, both as modifiers and as
//! the command at the end of a chain.

use std::sync::Arc;

use vanillite_base::{BlockPos, Vec3};
use vanillite_command::error::CommandError;
use vanillite_command::nbt_path::NbtPath;
use vanillite_command::values::Anchor;
use vanillite_command::{Arg as ErrArg, Args, GameValue, HeightmapKind, ResourceOrTag};
use vanillite_data::block::BlockState;
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_game::commands::{
    CommandId, CompareOp, DataKind, ExecuteCondition, ExecuteOp, NumType, Relation, StoreTarget,
};
use vanillite_nbt::Tag;
use vanillite_protocol::text::{Arg, Component};
use vanillite_world::entity::NewEntity;

use super::data::{self, DataTarget};
use super::engine::Callback;
use super::scores;
use super::select::{self, Target};
use super::source::CommandSource;
use super::{Env, nbt_ops};
use crate::world::WorldId;

/// Where `execute store` writes the result.
pub(crate) enum StoreKind {
    /// Scores of holders.
    Score {
        holders: Vec<String>,
        objective: String,
    },
    /// A number at a path.
    Data {
        target: DataTarget,
        path: NbtPath,
        ty: NumType,
        scale: f64,
    },
    /// A boss bar's value or maximum.
    Bossbar { id: String, max: bool },
}

/// An `execute store` callback.
pub(crate) struct Store {
    kind: StoreKind,
    /// `result` (else `success`).
    result: bool,
}

/// `onResult` of a store callback.
pub(crate) fn store(store: &Store, env: &mut Env<'_>, success: bool, result: i32) {
    let value = if store.result {
        result
    } else {
        i32::from(success)
    };
    match &store.kind {
        StoreKind::Score { holders, objective } => {
            let mut board = env.global.systems.scoreboard();
            for holder in holders {
                board.set_score(holder, objective, value, None);
            }
        }
        StoreKind::Data {
            target,
            path,
            ty,
            scale,
        } => {
            let tag = number_tag(*ty, f64::from(value) * scale);
            // Errors are dropped like Vanilla's `catch (CommandSyntaxException)`.
            let _ = data::store_number(target, path, tag, env);
        }
        StoreKind::Bossbar { id, max } => {
            let mut out = Vec::new();
            env.global
                .systems
                .set_bossbar_value(id, *max, value, &mut out);
            crate::bossbars::send(env.global, out);
        }
    }
}

/// Java's narrowing of the scaled value: `(int)`, `(float)`, `(short)(int)`, `(long)`,
/// `(byte)(int)`.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn number_tag(ty: NumType, v: f64) -> Tag {
    match ty {
        NumType::Int => Tag::Int(v as i32),
        NumType::Float => Tag::Float(v as f32),
        NumType::Short => Tag::Short((v as i32) as i16),
        NumType::Long => Tag::Long(v as i64),
        NumType::Double => Tag::Double(v),
        NumType::Byte => Tag::Byte((v as i32) as i8),
    }
}

fn missing(name: &str) -> CommandError {
    CommandError::new(
        "command.exception",
        vec![ErrArg::Str(format!("missing argument {name}"))],
    )
}

fn selector<'a>(
    args: &'a Args,
    name: &str,
) -> Result<&'a vanillite_command::selector::EntitySelector, CommandError> {
    args.selector(name).ok_or_else(|| missing(name))
}

/// `CommandSourceStack.withEntity`.
fn with_entity(source: &CommandSource, target: &Target) -> CommandSource {
    let mut out = source.clone();
    out.entity = Some(target.source_entity());
    out
}

/// `withLevel(e.level).withPosition(e.position).withRotation(e.getRotationVector())`.
fn at_entity(source: &CommandSource, target: &Target) -> Option<CommandSource> {
    let (pos, y_rot, x_rot) = target.place?;
    let mut out = source.clone();
    out.world = target.world;
    out.pos = pos;
    out.rot = (y_rot, x_rot);
    Some(out)
}

/// `EntityAnchorArgument.Anchor.apply(entity)`.
fn anchor_of(target: &Target, anchor: Anchor) -> Option<Vec3> {
    let (pos, _, _) = target.place?;
    Some(match anchor {
        Anchor::Feet => pos,
        Anchor::Eyes => pos + Vec3::new(0.0, f64::from(target.kind.info().eye_height), 0.0),
    })
}

/// `Mth.wrapDegrees` for floats.
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

/// `CommandSourceStack.facing(Vec3)`.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
fn facing(source: &CommandSource, at: Vec3) -> CommandSource {
    let from = source.anchor_pos();
    let (xd, yd, zd) = (at.x - from.x, at.y - from.y, at.z - from.z);
    let sd = (xd * xd + zd * zd).sqrt();
    let x_rot = wrap_degrees(-(yd.atan2(sd).to_degrees() as f32));
    let y_rot = wrap_degrees(zd.atan2(xd).to_degrees() as f32 - 90.0);
    let mut out = source.clone();
    out.rot = (y_rot, x_rot);
    out
}

/// `DimensionType.getTeleportationScale` between two worlds.
fn teleport_scale(global: &crate::global::Global, from: WorldId, to: WorldId) -> f64 {
    let scale = |w: WorldId| {
        global
            .dimensions
            .facts(w)
            .map_or(1.0, |t| t.coordinate_scale)
    };
    scale(from) / scale(to)
}

/// The world of a dimension argument (`DimensionArgument.getDimension`).
pub(crate) fn dimension(
    global: &crate::global::Global,
    args: &Args,
    name: &str,
) -> Result<WorldId, CommandError> {
    let id = args.identifier(name).ok_or_else(|| missing(name))?;
    global.dimensions.world(id).ok_or_else(|| {
        CommandError::new("argument.dimension.invalid", vec![ErrArg::Str(id.clone())])
    })
}

/// `BlockPosArgument.getLoadedBlockPos`.
pub(crate) fn loaded_block_pos(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<BlockPos, CommandError> {
    let c = args.coordinates(name).ok_or_else(|| missing(name))?;
    let pos = data::block_pos(source, c);
    if !env.level.in_height(pos.y) {
        return Err(CommandError::new("argument.pos.outofworld", vec![]));
    }
    if source.world != env.world || !env.level.is_loaded(pos) {
        return Err(CommandError::new("argument.pos.unloaded", vec![]));
    }
    Ok(pos)
}

/// The block predicate of `execute if block`, `clone filtered` and `fill replace`.
pub(crate) fn block_matches(
    env: &Env<'_>,
    pred: &vanillite_command::BlockPredicateArg,
    pos: BlockPos,
    state: BlockState,
) -> bool {
    let name = state.block().name();
    let block_ok = match &pred.block {
        ResourceOrTag::Resource(id) => id == name,
        ResourceOrTag::Tag(tag) => env.global.systems.in_tag("minecraft:block", tag, name),
    };
    block_ok
        && pred
            .properties
            .iter()
            .all(|(k, v)| state.value(k) == Some(v.as_str()))
        && pred.nbt.as_ref().is_none_or(|nbt| {
            env.level.block_entity_nbt(pos).is_some_and(|actual| {
                super::nbt_ops::matches(&Tag::Compound(nbt.clone()), &Tag::Compound(actual))
            })
        })
}

/// Score holder names of an argument, `*` for all tracked holders.
fn holder_names(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Vec<String>, CommandError> {
    Ok(scores::holders(args, name, source, env, true)?
        .into_iter()
        .map(|h| h.name)
        .collect())
}

/// `ObjectiveArgument.getObjective`.
fn objective(env: &Env<'_>, args: &Args, name: &str) -> Result<String, CommandError> {
    scores::objective(env, args, name)
}

/// Runs a redirect modifier for one source (`RedirectModifier.apply`).
pub(crate) fn modify(
    modifier: CommandId,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<Vec<CommandSource>, CommandError> {
    let CommandId::Execute(op) = modifier else {
        return Ok(vec![source.clone()]);
    };
    let one = |s: CommandSource| Ok(vec![s]);
    match op {
        ExecuteOp::Test { condition, unless } => {
            let count = test(condition, args, source, env)?;
            Ok(if (count > 0) != unless {
                vec![source.clone()]
            } else {
                Vec::new()
            })
        }
        ExecuteOp::As => Ok(select::optional(env, source, selector(args, "targets")?)?
            .iter()
            .map(|t| with_entity(source, t))
            .collect()),
        ExecuteOp::At => Ok(select::optional(env, source, selector(args, "targets")?)?
            .iter()
            .filter_map(|t| at_entity(source, t))
            .collect()),
        ExecuteOp::Positioned => {
            let c = args.coordinates("pos").ok_or_else(|| missing("pos"))?;
            let mut out = source.clone();
            out.pos = super::exec::resolve_position(source, c);
            out.anchor = Anchor::Feet;
            one(out)
        }
        ExecuteOp::PositionedAs => Ok(select::optional(env, source, selector(args, "targets")?)?
            .iter()
            .filter_map(|t| {
                let (pos, _, _) = t.place?;
                let mut out = source.clone();
                out.pos = pos;
                Some(out)
            })
            .collect()),
        ExecuteOp::PositionedOver => {
            let Some(GameValue::Heightmap(kind)) = args.game("heightmap") else {
                return Err(missing("heightmap"));
            };
            let pos = source.pos;
            let block = BlockPos::new(floor(pos.x), 0, floor(pos.z));
            if source.world != env.world || !env.level.is_loaded(block) {
                return Err(CommandError::new("argument.pos.unloaded", vec![]));
            }
            let height = height(env, *kind, block);
            let mut out = source.clone();
            out.pos = Vec3::new(pos.x, f64::from(height), pos.z);
            one(out)
        }
        ExecuteOp::Rotated => {
            let c = args.coordinates("rot").ok_or_else(|| missing("rot"))?;
            let mut out = source.clone();
            out.rot = super::exec::resolve_rotation(source, c);
            one(out)
        }
        ExecuteOp::RotatedAs => Ok(select::optional(env, source, selector(args, "targets")?)?
            .iter()
            .filter_map(|t| {
                let (_, y, x) = t.place?;
                let mut out = source.clone();
                out.rot = (y, x);
                Some(out)
            })
            .collect()),
        ExecuteOp::Facing => {
            let c = args.coordinates("pos").ok_or_else(|| missing("pos"))?;
            let at = super::exec::resolve_position(source, c);
            one(facing(source, at))
        }
        ExecuteOp::FacingEntity => {
            let anchor = args.anchor("anchor").copied().unwrap_or(Anchor::Feet);
            Ok(select::optional(env, source, selector(args, "targets")?)?
                .iter()
                .filter_map(|t| Some(facing(source, anchor_of(t, anchor)?)))
                .collect())
        }
        ExecuteOp::Align => {
            let Some(GameValue::Swizzle(axes)) = args.game("axes") else {
                return Err(missing("axes"));
            };
            let p = source.pos;
            let mut out = source.clone();
            out.pos = Vec3::new(
                if axes.x { p.x.floor() } else { p.x },
                if axes.y { p.y.floor() } else { p.y },
                if axes.z { p.z.floor() } else { p.z },
            );
            one(out)
        }
        ExecuteOp::Anchored => {
            let mut out = source.clone();
            out.anchor = args.anchor("anchor").copied().unwrap_or(Anchor::Feet);
            one(out)
        }
        ExecuteOp::In => {
            let world = dimension(env.global, args, "dimension")?;
            let mut out = source.clone();
            if world != source.world {
                let scale = teleport_scale(env.global, source.world, world);
                out.pos = Vec3::new(source.pos.x * scale, source.pos.y, source.pos.z * scale);
                out.world = world;
            }
            one(out)
        }
        ExecuteOp::Summon => {
            let id = args.identifier("entity").ok_or_else(|| missing("entity"))?;
            let empty = vanillite_nbt::Compound::new();
            let target = summon(env, source, id, source.pos, &empty, true)?;
            one(with_entity(source, &target))
        }
        ExecuteOp::On(relation) => {
            let Some(own) = source.entity.as_ref().and_then(|e| e.id) else {
                return Ok(Vec::new());
            };
            if source.entity.as_ref().is_some_and(|e| e.world != env.world) {
                return Ok(Vec::new());
            }
            let related = match relation {
                // `TraceableEntity.getOwner` of item entities: the thrower.
                Relation::Origin => env
                    .level
                    .entities
                    .item(own)
                    .and_then(|i| i.thrower)
                    .and_then(|u| select::find_uuid(&env.level.entities, u))
                    .into_iter()
                    .collect(),
                // `OwnableEntity.getOwner`: tamed animals.
                Relation::Owner => {
                    let ents = &env.level.entities;
                    ents.mob(own)
                        .and_then(|m| m.owner)
                        .and_then(|u| select::find_uuid(ents, u))
                        .into_iter()
                        .collect()
                }
                Relation::Leasher => env
                    .level
                    .entities
                    .leashes
                    .get(&own)
                    .copied()
                    .into_iter()
                    .collect(),
                // `Targeting.getTarget`.
                Relation::Target => env
                    .level
                    .entities
                    .mob(own)
                    .and_then(|m| m.target)
                    .or_else(|| {
                        vanillite_game::decoration::display::target(&env.level.entities, own)
                    })
                    .into_iter()
                    .collect(),
                // `LivingEntity.getLastAttacker`.
                Relation::Attacker => {
                    let ents = &env.level.entities;
                    ents.mob(own)
                        .and_then(|m| m.last_hurt_by_mob)
                        .or_else(|| {
                            ents.player(own)
                                .and_then(|p| p.survival.last_hurt_by_mob)
                                .map(|(id, _)| id)
                        })
                        .or_else(|| vanillite_game::decoration::display::last_attacker(ents, own))
                        .into_iter()
                        .collect()
                }
                Relation::Vehicle => env.level.entities.vehicle_of(own).into_iter().collect(),
                // `getControllingPassenger`.
                Relation::Controller => {
                    let ents = &env.level.entities;
                    ents.riding
                        .passengers_of(own)
                        .first()
                        .copied()
                        .filter(|&p| vanillite_game::riding::controls(ents, p, own))
                        .into_iter()
                        .collect()
                }
                Relation::Passengers => env.level.entities.riding.passengers_of(own).to_vec(),
            };
            Ok(related
                .into_iter()
                .filter_map(|id| select::local(&env.level.entities, env.world, id))
                .map(|t| with_entity(source, &t))
                .collect())
        }
        ExecuteOp::Store { target, result } => {
            let kind = match target {
                StoreTarget::Score => StoreKind::Score {
                    holders: holder_names(args, "targets", source, env)?,
                    objective: objective(env, args, "objective")?,
                },
                StoreTarget::BossbarValue | StoreTarget::BossbarMax => {
                    let id = args.identifier("id").ok_or_else(|| missing("id"))?;
                    if !env.global.systems.bossbar_exists(id) {
                        return Err(CommandError::new(
                            "commands.bossbar.unknown",
                            vec![ErrArg::Str(id.clone())],
                        ));
                    }
                    StoreKind::Bossbar {
                        id: id.clone(),
                        max: target == StoreTarget::BossbarMax,
                    }
                }
                StoreTarget::Data(_, ty) => StoreKind::Data {
                    target: data::target(args, "target", source, env)?,
                    path: args
                        .nbt_path("path")
                        .ok_or_else(|| missing("path"))?
                        .clone(),
                    ty,
                    scale: args.double("scale").copied().unwrap_or(1.0),
                },
            };
            let store = Callback::Store(Arc::new(Store { kind, result }));
            one(source.with_callback(Callback::chain(&source.callback, &store)))
        }
    }
}

fn floor(v: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let f = v.floor().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    f
}

/// `Level.getHeight(heightmap, x, z)`.
fn height(env: &Env<'_>, kind: HeightmapKind, pos: BlockPos) -> i32 {
    use vanillite_chunk::HeightmapKind as H;
    let kind = match kind {
        HeightmapKind::WorldSurface => H::WorldSurface,
        HeightmapKind::OceanFloor => H::OceanFloor,
        HeightmapKind::MotionBlocking => H::MotionBlocking,
        HeightmapKind::MotionBlockingNoLeaves => H::MotionBlockingNoLeaves,
    };
    let min = env.level.min_y();
    let Some(chunk) = env.level.chunks.chunk(pos.chunk()) else {
        return min;
    };
    let Some(heights) = chunk.chunk.heightmaps().get(kind) else {
        return min;
    };
    let (x, z) = (
        u32::try_from(pos.x & 15).unwrap_or(0),
        u32::try_from(pos.z & 15).unwrap_or(0),
    );
    i32::try_from(heights.get(x, z))
        .unwrap_or(0)
        .saturating_add(chunk.chunk.min_y())
}

/// `ResourceArgument.getSummonableEntityType` and `SummonCommand.createEntity`: the entity
/// loaded from `nbt` and moved to `pos`. Items, falling blocks and TNT get their kind data from
/// the NBT like their `readAdditionalSaveData`; other kinds keep what
/// [`data::apply_entity_nbt`] reads (mobs have no data of their own yet).
pub(crate) fn summon(
    env: &mut Env<'_>,
    source: &CommandSource,
    id: &str,
    pos: Vec3,
    nbt: &vanillite_nbt::Compound,
    finalize: bool,
) -> Result<Target, CommandError> {
    summon_for(
        env,
        source,
        id,
        pos,
        nbt,
        finalize,
        vanillite_abi::spawn_reason::COMMAND,
    )
}

/// [`summon`] for a plugin's spawn reason (`vanillite_abi::spawn_reason`: `COMMAND`, or
/// `PLUGIN` for `entity_spawn`); a plugin that cancels the spawn makes it fail. `finalize`:
/// mobs get `finalizeSpawn` (`/summon` without NBT).
pub(crate) fn summon_for(
    env: &mut Env<'_>,
    source: &CommandSource,
    id: &str,
    pos: Vec3,
    nbt: &vanillite_nbt::Compound,
    finalize: bool,
    reason: u32,
) -> Result<Target, CommandError> {
    let target = summon_one(env, source, id, pos, nbt, finalize, reason)?;
    super::data::join_team(env.global, target.uuid, nbt);
    // `EntityType.loadEntityRecursive`: the `Passengers` (each with its own `Passengers`)
    // are made at the same position and ride the entity (`startRiding(vehicle, true)`); one
    // that cannot be made is left out.
    if let (Some(Tag::List(passengers)), Some(vehicle)) = (nbt.get("Passengers"), target.entity) {
        let dimension = super::entities_cmd::dimension_of(env.global, env.world);
        for passenger in &passengers.0 {
            let Tag::Compound(tag) = passenger else {
                continue;
            };
            let Some(Tag::String(kind)) = tag.get("id") else {
                continue;
            };
            let kind = if kind.contains(':') {
                kind.clone()
            } else {
                format!("minecraft:{kind}")
            };
            let Ok(rider) = summon_for(env, source, &kind, pos, tag, false, reason) else {
                continue;
            };
            if let Some(rider) = rider.entity {
                super::entities_cmd::mount(env.level, env.global, dimension, rider, vehicle);
                // `startRiding` leaves the passenger where it was made: its first `rideTick`
                // places it on the vehicle.
                if let Some(mut e) = env.level.entities.store.get_mut(rider) {
                    e.set_pos(pos);
                }
            }
        }
    }
    Ok(target)
}

/// [`summon_for`] of one entity, without its passengers.
fn summon_one(
    env: &mut Env<'_>,
    source: &CommandSource,
    id: &str,
    pos: Vec3,
    nbt: &vanillite_nbt::Compound,
    finalize: bool,
    reason: u32,
) -> Result<Target, CommandError> {
    use vanillite_game::entities::{FallingBlockData, PrimedTntData};
    let failed = || CommandError::new("commands.summon.failed", vec![]);
    let duplicate = || CommandError::new("commands.summon.failed.uuid", vec![]);
    let kind = EntityTypeId::by_name(id)
        .filter(|k| k.info().summonable)
        .ok_or_else(|| CommandError::new("entity.not_summonable", vec![ErrArg::from(id)]))?;
    let block = BlockPos::new(floor(pos.x), floor(pos.y), floor(pos.z));
    let in_bounds = (-30_000_000..30_000_000).contains(&block.x)
        && (-30_000_000..30_000_000).contains(&block.z)
        && (-20_000_000..20_000_000).contains(&block.y);
    if !in_bounds {
        return Err(CommandError::new("commands.summon.invalidPosition", vec![]));
    }
    let path = id.strip_prefix("minecraft:").unwrap_or(id);
    if env.global.level.difficulty() == vanillite_game::survival::rules::Difficulty::Peaceful
        && !vanillite_game::mob::spawn::allowed_in_peaceful(path)
    {
        return Err(CommandError::new("commands.summon.failed.peaceful", vec![]));
    }
    // Only the level of the command's world is reachable here.
    if source.world != env.world || kind == ids::PLAYER {
        return Err(failed());
    }
    // Decorations, armor stands, mannequins and mobs take their seed when their data is
    // created.
    let decor = vanillite_game::decoration::is_decor_kind(kind)
        || vanillite_game::mob::kinds::Kind::by_name(id)
            .is_some_and(|k| k.family() == vanillite_game::mob::Family::Decor);
    let own_seed = decor || vanillite_game::mob::kinds::Kind::by_name(id).is_some();
    // The seed a fresh UUID's new entity took (its own random).
    let mut fresh_seed = None;
    let mut fresh_uuid = || {
        if own_seed {
            vanillite_game::mob::new_uuid()
        } else {
            let (uuid, seed) = env.level.new_uuid_and_seed();
            fresh_seed = Some(seed);
            uuid
        }
    };
    let uuid = match nbt.get("UUID") {
        Some(Tag::IntArray(ints)) => match ints.as_slice() {
            [a, b, c, d] => {
                // The entity's constructor still creates its random (and a UUID from it)
                // before the NBT replaces the UUID.
                if !own_seed {
                    let _ = fresh_uuid();
                }
                [*a, *b, *c, *d]
                    .iter()
                    .fold(0u128, |acc, v| (acc << 32) | u128::from(v.cast_unsigned()))
            }
            _ => fresh_uuid(),
        },
        _ => fresh_uuid(),
    };
    if select::find_uuid(&env.level.entities, uuid).is_some() {
        return Err(duplicate());
    }
    let short = |key: &str, default: i32| {
        nbt.get(key)
            .and_then(Tag::as_i64)
            .and_then(|v| i16::try_from(v).ok())
            .map_or(default, i32::from)
    };
    // `BlockState.CODEC`: a block id or `{id, properties}` (`{Name, Properties}` too).
    let block_state = |key: &str, default: BlockState| {
        nbt.get(key)
            .and_then(vanillite_game::level::block_entity::read_state_codec)
            .unwrap_or(default)
    };
    if let Some(mob) = vanillite_game::mob::kinds::Kind::by_name(id)
        .filter(|k| k.family() != vanillite_game::mob::Family::Decor)
    {
        // Mobs with behaviour: `finalizeSpawn` with the command reason.
        let table = env.global.systems.mob_spawns.get().map(Arc::as_ref);
        let difficulty = env.global.level.difficulty();
        // The constructor's seed is the one the fresh UUID's entity took.
        let id = vanillite_game::mob::spawn::summon_seeded(
            env.level, table, mob, pos, difficulty, reason, finalize, fresh_seed,
        );
        if !env.level.entities.store.contains(id) {
            // A plugin cancelled the spawn (`tryAddFreshEntityWithPassengers` failed).
            return Err(failed());
        }
        // `Entity.load` of the given data: its `UUID` replaces the constructor's.
        if matches!(nbt.get("UUID"), Some(Tag::IntArray(i)) if i.len() == 4) {
            env.level.entities.store.set_uuid(id, uuid);
        }
        // The given tags on top of what the mob has (like `data merge`).
        let mut loaded = data::entity_nbt(&env.level.entities, id).unwrap_or_default();
        // `Entity.load` of the given tags: without `Health` the mob has its full health.
        if !finalize {
            loaded.remove("Health");
        }
        super::data_cmd::merge_compound(&mut loaded, nbt);
        loaded.remove("Pos");
        data::apply_entity_nbt(&mut env.level.entities, id, &loaded)?;
        return select::local(&env.level.entities, env.world, id).ok_or_else(failed);
    }
    if vanillite_game::mob::kinds::Kind::by_name(id)
        .is_some_and(|k| k.family() == vanillite_game::mob::Family::Decor)
    {
        // Armor stands and mannequins are no `Mob`: loaded without `finalizeSpawn`.
        let mut full = nbt.clone();
        full.insert("id", kind.name());
        full.insert(
            "Pos",
            vanillite_nbt::List(vec![
                Tag::Double(pos.x),
                Tag::Double(pos.y),
                Tag::Double(pos.z),
            ]),
        );
        full.insert("UUID", vanillite_game::mob::nbt::uuid_tag(uuid));
        let loaded =
            vanillite_game::mob::nbt::load_mob(&full, vanillite_game::mob::new_entity_seed())
                .ok_or_else(failed)?;
        let network_ids = Arc::clone(env.level.network_ids_arc());
        let id = vanillite_game::mob::spawn(
            &mut env.level.entities,
            &network_ids,
            loaded.uuid,
            loaded.pos,
            loaded.rot,
            loaded.data,
        );
        let tags: Vec<String> = match nbt.get("Tags") {
            Some(Tag::List(l)) => {
                l.0.iter()
                    .filter_map(|t| t.as_str().map(str::to_owned))
                    .collect()
            }
            _ => Vec::new(),
        };
        if let Some(mut e) = env.level.entities.store.get_mut(id) {
            e.set_velocity(loaded.velocity);
        }
        env.level.entities.set_tags(id, tags);
        vanillite_game::mob::entity_nbt::load_base(&mut env.level.entities, id, &full, true);
        if !env.level.entities.spawned(id, reason, 0) {
            return Err(failed());
        }
        return select::local(&env.level.entities, env.world, id).ok_or_else(failed);
    }
    if kind == ids::LIGHTNING_BOLT {
        let id = vanillite_game::lightning::spawn(env.level, pos, false);
        if !env.level.entities.spawned(id, reason, 0) {
            return Err(failed());
        }
        return select::local(&env.level.entities, env.world, id).ok_or_else(failed);
    }
    if kind != ids::ITEM
        && kind != ids::FALLING_BLOCK
        && kind != ids::TNT
        && vanillite_game::entities::Entities::has_kind_store(kind)
    {
        // `EntityType.loadEntityRecursive` of the kinds with data: the NBT with the command's
        // type, position and UUID.
        let mut full = nbt.clone();
        full.insert("id", kind.name());
        full.insert(
            "Pos",
            vanillite_nbt::List(vec![
                Tag::Double(pos.x),
                Tag::Double(pos.y),
                Tag::Double(pos.z),
            ]),
        );
        full.insert("UUID", vanillite_game::mob::nbt::uuid_tag(uuid));
        // An arrow's own random starts from the seed its constructor took.
        let seed = fresh_seed.filter(|_| vanillite_game::mob::projectile::is_arrow_kind(kind));
        if !env.level.load_entity_seeded(full, seed) {
            return Err(failed());
        }
        let entities = &mut env.level.entities;
        let id = *entities.store.columns().id.last().ok_or_else(failed)?;
        if entities.store.get(id).is_none_or(|e| e.uuid() != uuid) {
            return Err(failed());
        }
        // The orb's own random: the seed its constructor took.
        if let (Some(seed), Some(orb)) = (fresh_seed, entities.experience_orb_mut(id)) {
            orb.random_state = vanillite_base::random::LegacyRandom::new(seed).state();
        }
        if !entities.spawned(id, reason, 0) {
            return Err(failed());
        }
        return select::local(entities, env.world, id).ok_or_else(failed);
    }
    // The level's allocator (the server's shared one).
    let network_id = env.level.network_ids_arc().next();
    let new = NewEntity::new(kind, network_id, uuid, pos);
    let entities = &mut env.level.entities;
    let id = if kind == ids::ITEM {
        let stack = match nbt.get("Item") {
            Some(Tag::Compound(c)) => vanillite_game::inventory::stack_from_nbt(c),
            _ => vanillite_protocol::item::ItemStack::EMPTY,
        };
        // An item entity without an item discards itself while loading, and
        // `tryAddFreshEntityWithPassengers` refuses removed entities.
        if stack.is_empty() {
            return Err(duplicate());
        }
        #[allow(clippy::cast_possible_truncation)]
        let short = |name: &str, default: i16| {
            nbt.get(name)
                .and_then(Tag::as_i64)
                .map_or(default, |v| v as i16)
        };
        let mut data = vanillite_game::item_entity::ItemData::new(
            stack,
            short("PickupDelay", 0),
            None,
            vanillite_game::item_entity::loaded_random(
                fresh_seed.unwrap_or_else(vanillite_game::mob::new_entity_seed),
            ),
        );
        data.age = short("Age", 0);
        data.health = short("Health", 5);
        data.fire_ticks = i32::from(short("Fire", 0)).max(0);
        entities.spawn_item(network_id, uuid, pos, Vec3::new(0.0, 0.0, 0.0), 0.0, data)
    } else if kind == ids::FALLING_BLOCK {
        let mut data = FallingBlockData::new(
            block_state(
                "BlockState",
                vanillite_data::block::ids::SAND.default_state(),
            ),
            nbt.get("DropItem")
                .and_then(Tag::as_i64)
                .is_none_or(|v| v != 0),
            BlockPos::new(0, 0, 0),
            // The seed the fresh UUID's entity took: one draw of the seed stream per entity.
            fresh_seed.unwrap_or_else(vanillite_game::mob::new_entity_seed),
        );
        data.time = nbt
            .get("Time")
            .and_then(Tag::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0);
        data.hurt_entities = nbt.get("HurtEntities").and_then(Tag::as_i64).map_or_else(
            || vanillite_game::block::falling::is_anvil(data.state.block()),
            |v| v != 0,
        );
        if let Some(Tag::Float(v)) = nbt.get("FallHurtAmount") {
            data.fall_hurt_amount = *v;
        }
        if let Some(v) = nbt.get("FallHurtMax").and_then(Tag::as_i64) {
            data.fall_hurt_max = i32::try_from(v).unwrap_or(40);
        }
        data.cancel_drop = nbt
            .get("CancelDrop")
            .and_then(Tag::as_i64)
            .is_some_and(|v| v != 0);
        entities.spawn_falling_block(new, data)
    } else if kind == ids::TNT {
        let power = match nbt.get("explosion_power") {
            Some(Tag::Float(v)) => v.clamp(0.0, 128.0),
            _ => 4.0,
        };
        let data = PrimedTntData {
            fuse: short("fuse", 80),
            state: block_state(
                "block_state",
                vanillite_data::block::ids::TNT.default_state(),
            ),
            owner: None,
            explosion_power: power,
            fall_distance: match nbt.get("fall_distance") {
                Some(Tag::Double(v)) => *v,
                _ => 0.0,
            },
        };
        entities.spawn_primed_tnt(new, data)
    } else {
        entities.store.spawn(new, |_| 0)
    };
    // `snapTo(pos)` after loading: the position is the command's, the rest the NBT's.
    let mut loaded = nbt.clone();
    loaded.remove("Pos");
    data::apply_entity_nbt(entities, id, &loaded)?;
    if !entities.spawned(id, reason, 0) {
        return Err(failed());
    }
    select::local(entities, env.world, id).ok_or_else(failed)
}

/// A condition's count: 1/0 for the boolean conditions, the number of matches for `entity`,
/// `data`, `blocks` and `items` (`CommandPredicate`, `CommandNumericPredicate`).
fn test(
    condition: ExecuteCondition,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let count = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
    Ok(match condition {
        ExecuteCondition::Block => {
            let pos = loaded_block_pos(args, "pos", source, env)?;
            let Some(GameValue::BlockPredicate(pred)) = args.game("block") else {
                return Err(missing("block"));
            };
            i32::from(block_matches(env, pred, pos, env.level.block(pos)))
        }
        ExecuteCondition::Biome => {
            let pos = loaded_block_pos(args, "pos", source, env)?;
            let biome = env.level.biome(pos);
            let name = env
                .global
                .registries
                .get("minecraft:worldgen/biome")
                .and_then(|b| b.get(usize::from(biome)))
                .cloned()
                .unwrap_or_default();
            let Some(GameValue::ResourceOrTag(pred)) = args.game("biome") else {
                return Err(missing("biome"));
            };
            i32::from(match pred {
                ResourceOrTag::Resource(id) => *id == name,
                ResourceOrTag::Tag(tag) => {
                    env.global
                        .systems
                        .in_tag("minecraft:worldgen/biome", tag, &name)
                }
            })
        }
        ExecuteCondition::Loaded => {
            let c = args.coordinates("pos").ok_or_else(|| missing("pos"))?;
            let pos = data::block_pos(source, c);
            if !env.level.in_height(pos.y) {
                return Err(CommandError::new("argument.pos.outofworld", vec![]));
            }
            let loaded = source.world == env.world
                && env.level.is_loaded(pos)
                && env.level.is_block_ticking(pos.chunk());
            i32::from(loaded)
        }
        ExecuteCondition::Dimension => {
            i32::from(dimension(env.global, args, "dimension")? == source.world)
        }
        ExecuteCondition::Score(op) => {
            let board_check = |holder: &str, objective: &str| {
                env.global
                    .systems
                    .scoreboard()
                    .score(holder, objective)
                    .map(|s| s.value)
            };
            let target = scores::single_holder(args, "target", source, env)?;
            let target_objective = objective(env, args, "targetObjective")?;
            let other = scores::single_holder(args, "source", source, env)?;
            let source_objective = objective(env, args, "sourceObjective")?;
            let a = board_check(&target, &target_objective);
            let b = board_check(&other, &source_objective);
            i32::from(match (a, b) {
                (Some(a), Some(b)) => match op {
                    CompareOp::Eq => a == b,
                    CompareOp::Lt => a < b,
                    CompareOp::Le => a <= b,
                    CompareOp::Gt => a > b,
                    CompareOp::Ge => a >= b,
                },
                _ => false,
            })
        }
        ExecuteCondition::ScoreMatches => {
            let target = scores::single_holder(args, "target", source, env)?;
            let target_objective = objective(env, args, "targetObjective")?;
            let range = args.int_range("range").ok_or_else(|| missing("range"))?;
            let value = env
                .global
                .systems
                .scoreboard()
                .score(&target, &target_objective)
                .map(|s| s.value);
            i32::from(value.is_some_and(|v| range.matches(v)))
        }
        ExecuteCondition::Blocks(masked) => regions(args, source, env, masked)?.unwrap_or(0),
        ExecuteCondition::Entity => {
            count(select::optional(env, source, selector(args, "entities")?)?.len())
        }
        // `checkCustomPredicate` with the level's random.
        ExecuteCondition::Predicate => {
            let predicate = super::loot_cmd::resource_or_inline(args, "predicate")?;
            let this = source
                .entity
                .as_ref()
                .filter(|e| e.world == env.world)
                .and_then(|e| e.id);
            let level = &mut *env.level;
            let weather = (level.is_raining(), level.is_thundering());
            let entities = std::mem::take(&mut level.entities);
            let mut random = level.lend_random();
            let result = super::loot_cmd::test_predicate(
                &entities,
                Some(&level.chunks),
                env.global,
                this,
                source.pos,
                weather,
                &predicate,
                &mut random,
            );
            level.restore_random(random);
            level.entities = entities;
            i32::from(
                result.map_err(|e| CommandError::new("command.exception", vec![ErrArg::Str(e)]))?,
            )
        }
        ExecuteCondition::Stopwatch => {
            let id = args.identifier("id").ok_or_else(|| missing("id"))?;
            let range = args.float_range("range").ok_or_else(|| missing("range"))?;
            let elapsed = env.global.systems.stopwatch_seconds(id).ok_or_else(|| {
                CommandError::new(
                    "commands.stopwatch.does_not_exist",
                    vec![ErrArg::Str(id.clone())],
                )
            })?;
            i32::from(range.matches(elapsed))
        }
        ExecuteCondition::Data(_) => {
            let target = data::target(args, "source", source, env)?;
            let path = args.nbt_path("path").ok_or_else(|| missing("path"))?;
            let root = Tag::Compound(data::read(&target, env)?);
            count(nbt_ops::count(path, &root))
        }
        // `countItems`: the count of the matching stacks.
        ExecuteCondition::Items(kind) => {
            let stacks = super::items::selected_stacks(kind == DataKind::Block, args, source, env)?;
            let accepted = super::items::accepted_items_of(args, "item_predicate", env)?;
            stacks
                .iter()
                .filter(|s| accepted.matches(s))
                .map(|s| s.count)
                .sum()
        }
        // `countSlots`: the number of selected slots.
        ExecuteCondition::Slots(kind) => {
            count(super::items::selected_stacks(kind == DataKind::Block, args, source, env)?.len())
        }
    })
}

/// `checkRegions`: the number of compared blocks, `None` when a block differs.
fn regions(
    args: &Args,
    source: &CommandSource,
    env: &Env<'_>,
    masked: bool,
) -> Result<Option<i32>, CommandError> {
    let start = loaded_block_pos(args, "start", source, env)?;
    let end = loaded_block_pos(args, "end", source, env)?;
    let dest = loaded_block_pos(args, "destination", source, env)?;
    let (min, max) = (
        BlockPos::new(start.x.min(end.x), start.y.min(end.y), start.z.min(end.z)),
        BlockPos::new(start.x.max(end.x), start.y.max(end.y), start.z.max(end.z)),
    );
    let span = |a: i32, b: i32| i64::from(b) - i64::from(a) + 1;
    let area = span(min.x, max.x)
        .saturating_mul(span(min.y, max.y))
        .saturating_mul(span(min.z, max.z));
    if area > 32768 {
        return Err(CommandError::new(
            "commands.execute.blocks.toobig",
            vec![ErrArg::Int(32768), ErrArg::Long(area)],
        ));
    }
    let offset = (
        dest.x.saturating_sub(min.x),
        dest.y.saturating_sub(min.y),
        dest.z.saturating_sub(min.z),
    );
    let mut count = 0i32;
    for z in min.z..=max.z {
        for y in min.y..=max.y {
            for x in min.x..=max.x {
                let from = env.level.block(BlockPos::new(x, y, z));
                if masked && from.is_air() {
                    continue;
                }
                let to = env.level.block(BlockPos::new(
                    x.saturating_add(offset.0),
                    y.saturating_add(offset.1),
                    z.saturating_add(offset.2),
                ));
                if from != to {
                    return Ok(None);
                }
                count = count.saturating_add(1);
            }
        }
    }
    Ok(Some(count))
}

/// A condition as the last command of a chain: the pass message and its count, or the
/// failure (`addConditional`'s `executes`, `addIfBlocksConditional`).
pub(crate) fn run_condition(
    condition: ExecuteCondition,
    unless: bool,
    args: &Args,
    source: &CommandSource,
    env: &mut Env<'_>,
) -> Result<i32, CommandError> {
    let counted = matches!(
        condition,
        ExecuteCondition::Entity
            | ExecuteCondition::Data(_)
            | ExecuteCondition::Items(_)
            | ExecuteCondition::Slots(_)
    );
    let fail = || CommandError::new("commands.execute.conditional.fail", vec![]);
    let pass = |source: &CommandSource, env: &Env<'_>| {
        source.send_success(
            env.global,
            Component::translatable("commands.execute.conditional.pass", vec![]),
            false,
        );
    };
    if let ExecuteCondition::Blocks(masked) = condition {
        let found = regions(args, source, env, masked)?;
        return match (found, unless) {
            (Some(count), false) => {
                pass_count(source, env, count);
                Ok(count)
            }
            (None, false) => Err(fail()),
            (Some(count), true) => Err(fail_count(count)),
            (None, true) => {
                pass(source, env);
                Ok(1)
            }
        };
    }
    let count = test(condition, args, source, env)?;
    if !counted {
        return if (count > 0) != unless {
            pass(source, env);
            Ok(1)
        } else {
            Err(fail())
        };
    }
    match (count > 0, unless) {
        (true, false) => {
            pass_count(source, env, count);
            Ok(count)
        }
        (false, false) => Err(fail()),
        (false, true) => {
            pass(source, env);
            Ok(1)
        }
        (true, true) => Err(fail_count(count)),
    }
}

fn pass_count(source: &CommandSource, env: &Env<'_>, count: i32) {
    source.send_success(
        env.global,
        Component::translatable_args(
            "commands.execute.conditional.pass_count",
            vec![Arg::Int(count)],
        ),
        false,
    );
}

fn fail_count(count: i32) -> CommandError {
    CommandError::new(
        "commands.execute.conditional.fail_count",
        vec![ErrArg::Int(count)],
    )
}
