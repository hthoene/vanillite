//! What `data`, `execute store`/`if data` and `function ... with` read and write
//! (`DataAccessor`: `EntityDataAccessor`, `BlockDataAccessor`, `StorageDataAccessor`).

use vanillite_base::BlockPos;
use vanillite_command::coords::Coordinates;
use vanillite_command::error::CommandError;
use vanillite_command::nbt_path::NbtPath;
use vanillite_command::{Arg as ErrArg, ArgValue, Args};
use vanillite_data::entity::ids;
use vanillite_game::entities::Entities;
use vanillite_nbt::{Compound, List, Tag};
use vanillite_world::entity::EntityId;

use super::source::CommandSource;
use super::{Env, nbt_ops, select};
use crate::global::Global;

/// Where data lives.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DataTarget {
    /// An entity of the command's world.
    Entity { uuid: u128, id: EntityId },
    /// A block entity.
    Block(BlockPos),
    /// Command storage.
    Storage(String),
}

/// `NbtTagArgument`-style type names (`Tag.getType().getName()`).
pub(crate) fn type_name(tag: &Tag) -> &'static str {
    match tag {
        Tag::Byte(_) => "BYTE",
        Tag::Short(_) => "SHORT",
        Tag::Int(_) => "INT",
        Tag::Long(_) => "LONG",
        Tag::Float(_) => "FLOAT",
        Tag::Double(_) => "DOUBLE",
        Tag::ByteArray(_) => "BYTE[]",
        Tag::String(_) => "STRING",
        Tag::List(_) => "LIST",
        Tag::Compound(_) => "COMPOUND",
        Tag::IntArray(_) => "INT[]",
        Tag::LongArray(_) => "LONG[]",
    }
}

/// The target named `name` in `args`: a single entity, a block position or a storage id.
pub(crate) fn target(
    args: &Args,
    name: &str,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<DataTarget, CommandError> {
    match args.get(name) {
        Some(ArgValue::Entity(selector)) => {
            let t = select::single(env, source, selector)?;
            let id = t
                .entity
                .ok_or_else(|| CommandError::new("argument.entity.notfound.entity", vec![]))?;
            Ok(DataTarget::Entity { uuid: t.uuid, id })
        }
        Some(ArgValue::Coordinates(c)) => {
            // `BlockPosArgument.getLoadedBlockPos`, then the block entity.
            let pos = block_pos(source, c);
            if !env.level.in_height(pos.y) {
                return Err(CommandError::new("argument.pos.outofworld", vec![]));
            }
            if source.world != env.world || !env.level.is_loaded(pos) {
                return Err(CommandError::new("argument.pos.unloaded", vec![]));
            }
            if env.level.block(pos).block().info().block_entity == u16::MAX {
                return Err(CommandError::new("commands.data.block.invalid", vec![]));
            }
            Ok(DataTarget::Block(pos))
        }
        Some(ArgValue::Identifier(id)) => Ok(DataTarget::Storage(id.clone())),
        _ => Err(CommandError::new("command.exception", vec![])),
    }
}

/// The block position of block coordinates for a source.
pub(crate) fn block_pos(source: &CommandSource, c: &Coordinates) -> BlockPos {
    let p = super::exec::resolve_position(source, c);
    BlockPos::new(floor(p.x), floor(p.y), floor(p.z))
}

fn floor(v: f64) -> i32 {
    let f = v.floor();
    if f >= f64::from(i32::MAX) {
        i32::MAX
    } else if f <= f64::from(i32::MIN) {
        i32::MIN
    } else {
        // In range after the checks above.
        format!("{f:.0}").parse().unwrap_or(0)
    }
}

/// `DataAccessor.getData`.
pub(crate) fn read(target: &DataTarget, env: &Env<'_>) -> Result<Compound, CommandError> {
    match target {
        DataTarget::Storage(id) => Ok(env
            .global
            .systems
            .storage()
            .get(id)
            .cloned()
            .unwrap_or_default()),
        DataTarget::Entity { id, .. } => entity_nbt(&env.level.entities, *id)
            .ok_or_else(|| CommandError::new("argument.entity.notfound.entity", vec![])),
        DataTarget::Block(pos) => env
            .level
            .block_entity_nbt(*pos)
            .ok_or_else(|| CommandError::new("commands.data.block.invalid", vec![])),
    }
    .map(|mut c| {
        c.positive_zeros();
        c
    })
}

/// `DataAccessor.setData`.
pub(crate) fn write(
    target: &DataTarget,
    env: &mut Env<'_>,
    data: Compound,
) -> Result<(), CommandError> {
    match target {
        DataTarget::Storage(id) => {
            env.global.systems.storage().set(id.clone(), data);
            Ok(())
        }
        DataTarget::Entity { uuid, id } => {
            apply_entity_nbt(&mut env.level.entities, *id, &data)?;
            join_team(env.global, *uuid, &data);
            Ok(())
        }
        DataTarget::Block(pos) => {
            if env.level.load_block_entity_nbt(*pos, &data) {
                Ok(())
            } else {
                Err(CommandError::new("commands.data.block.invalid", vec![]))
            }
        }
    }
}

/// `Entity.load`'s `Team`: the entity joins the scoreboard team, if there is one.
pub(super) fn join_team(global: &Global, uuid: u128, data: &Compound) {
    if let Some(team) = data.get("Team").and_then(Tag::as_str) {
        let holder = vanillite_protocol::types::Uuid(uuid).to_string();
        let joined = global.systems.scoreboard().join_team(&holder, team);
        if joined {
            crate::scoreboard::flush(global);
        } else {
            log::warn!("Unable to add entity to team {team:?} (that team probably doesn't exist)");
        }
    }
}

/// The compound of the `source` argument of `function ... with`.
pub(crate) fn read_source(
    args: &Args,
    source: &CommandSource,
    env: &Env<'_>,
) -> Result<Compound, CommandError> {
    let t = target(args, "source", source, env)?;
    read(&t, env)
}

/// `DataCommands.getSingleTag`.
pub(crate) fn single_tag(path: &NbtPath, root: &Tag) -> Result<Tag, CommandError> {
    let mut found = nbt_ops::get(path, root)?;
    if found.len() != 1 {
        return Err(CommandError::new("commands.data.get.multiple", vec![]));
    }
    found
        .pop()
        .ok_or_else(|| CommandError::new("commands.data.get.multiple", vec![]))
}

/// `Entity.saveWithoutId` for the data the server keeps for every entity, with the kind's own
/// data for players and items.
pub(crate) fn entity_nbt(entities: &Entities, id: EntityId) -> Option<Compound> {
    vanillite_game::mob::entity_nbt::save_full(entities, id)
}

/// `Entity.load` of a whole entity compound: position, motion, rotation, tags, the common
/// keys ([`vanillite_game::mob::entity_nbt::load_base`]) and the kind's data. Players
/// cannot be changed (`commands.data.entity.invalid`).
pub(crate) fn apply_entity_nbt(
    entities: &mut Entities,
    id: EntityId,
    data: &Compound,
) -> Result<(), CommandError> {
    let Some(e) = entities.store.get(id) else {
        return Err(CommandError::new("argument.entity.notfound.entity", vec![]));
    };
    if e.kind() == ids::PLAYER {
        return Err(CommandError::new("commands.data.entity.invalid", vec![]));
    }
    let tags: Vec<String> = match data.get("Tags") {
        Some(Tag::List(List(l))) => l
            .iter()
            .filter_map(|t| t.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    entities.set_tags(id, tags);
    if vanillite_game::decoration::apply_nbt(entities, id, data) {
        return Ok(());
    }
    // `readAdditionalSaveData` of mobs and minecarts.
    let (game_time, day_time) = (entities.game_time, entities.day_time);
    if let Some(m) = entities.mob_mut(id) {
        vanillite_game::mob::nbt::apply_mob(m, data, game_time, day_time);
    } else if let Some(m) = entities.minecart_mut(id) {
        vanillite_game::mob::nbt::apply_minecart(m, data);
    }
    vanillite_game::mob::entity_nbt::load_base(entities, id, data, true);
    // `Entity.load`: head and body face the yaw.
    let rotation = vanillite_game::mob::entity_nbt::rotation(data.get("Rotation"));
    if let Some((yaw, _)) = rotation
        && let Some(m) = entities.mob_mut(id)
    {
        m.y_body_rot = yaw;
    }
    if let Some(mut e) = entities.store.get_mut(id) {
        if let Some(pos) = vanillite_game::mob::entity_nbt::vec3(data.get("Pos")) {
            e.set_pos(pos);
        }
        if let Some(motion) = vanillite_game::mob::entity_nbt::motion(data.get("Motion")) {
            e.set_velocity(motion);
        }
        if let Some((y, x)) = rotation {
            // `setRot` keeps yaw and pitch modulo 360; the head keeps the stored yaw.
            let mut rot = e.as_ref().rot();
            rot.y_rot = y % 360.0;
            rot.x_rot = x % 360.0;
            rot.y_head_rot = y;
            e.set_rot(rot);
        }
        if let Some(on_ground) = vanillite_game::mob::entity_nbt::get_bool(data, "OnGround") {
            e.set_flags(vanillite_world::entity::EntityFlags::ON_GROUND, on_ground);
        }
        e.mark_data_dirty();
    }
    if e_kind_is_item(entities, id)
        && let Some(item) = entities.item_mut(id)
    {
        // `getShortOr`: `NumericTag.shortValue()`, the low 16 bits.
        #[allow(clippy::cast_possible_truncation)]
        let short =
            |key: &str| vanillite_game::mob::entity_nbt::get_int(data, key).map(|v| v as i16);
        if let Some(age) = short("Age") {
            item.age = age;
        }
        if let Some(delay) = short("PickupDelay") {
            item.pickup_delay = delay;
        }
        if let Some(Tag::Compound(c)) = data.get("Item") {
            item.stack = vanillite_game::inventory::stack_from_nbt(c);
        }
    }
    Ok(())
}

fn e_kind_is_item(entities: &Entities, id: EntityId) -> bool {
    entities
        .store
        .get(id)
        .is_some_and(|e| e.kind() == ids::ITEM)
}

/// Writes a number at a path of a target (`execute store ... <path> <type> <scale>`).
pub(crate) fn store_number(
    target: &DataTarget,
    path: &NbtPath,
    tag: Tag,
    env: &mut Env<'_>,
) -> Result<(), CommandError> {
    let data = read(target, env)?;
    let mut root = Tag::Compound(data);
    nbt_ops::set(path, &mut root, &tag)?;
    if let Tag::Compound(c) = root {
        write(target, env, c)?;
    }
    Ok(())
}

/// The error of a value that is no number (`commands.data.get.invalid`).
pub(crate) fn not_numeric(path: &NbtPath) -> CommandError {
    CommandError::new(
        "commands.data.get.invalid",
        vec![ErrArg::Str(path.text.clone())],
    )
}
