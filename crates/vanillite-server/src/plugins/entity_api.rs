//! The entity side of plugin ABI 1.6: typed entity properties, custom names, passengers
//! (`/ride`), attributes (`/attribute`), mob targets, navigation and plugin goals of the
//! world a plugin call runs on.
//!
//! Properties use the fields the game keeps for players and mobs; decorations and armor stands
//! take their flags through their entity data as `/data merge` does, and a change the entity
//! does not keep is refused rather than faked.

use super::ReadWorld;
use vanillite_abi::{self as abi, attribute_op, entity_prop, event, player_prop, status};
use vanillite_data::entity::ATTRIBUTES;
use vanillite_game::effect::Operation;
use vanillite_game::effect::attribute::Attribute;
use vanillite_game::entities::Entities;
use vanillite_game::hooks::{GoalAction, GoalMob, PluginGoal as GamePluginGoal};
use vanillite_game::mob::goals::plugin;
use vanillite_game::mob::{Family, Kind, Tick};
use vanillite_nbt::{Compound, Tag};
use vanillite_plugin_host::{PluginGoal, Text};
use vanillite_protocol::text::Component;
use vanillite_world::entity::{EntityFlags, EntityId};

use super::PluginWorld;
use super::world_api::{any_entity, float, position, qualified, whole};
use crate::survival::WorldSurvival;

type Result<T> = std::result::Result<T, i32>;

/// The longest custom name (its network NBT) a plugin may set: what a name reads back with.
const MAX_NAME_BYTES: usize = (1 << 16) - 1;

fn flag(on: bool) -> f64 {
    if on { 1.0 } else { 0.0 }
}

/// A decoration or an armor stand (mannequin): entities whose flags live in their own data.
fn decor_like(entities: &Entities, id: EntityId) -> bool {
    entities.decor(id).is_some()
        || entities
            .mob(id)
            .is_some_and(|m| vanillite_game::decoration::stand::is_stand(m.kind))
}

/// The entity data key of a flag decorations and armor stands keep.
fn decor_key(prop: u32) -> Option<&'static str> {
    Some(match prop {
        entity_prop::NO_GRAVITY => "NoGravity",
        entity_prop::SILENT => "Silent",
        entity_prop::GLOWING => "Glowing",
        entity_prop::INVULNERABLE => "Invulnerable",
        entity_prop::CUSTOM_NAME_VISIBLE => "CustomNameVisible",
        entity_prop::INVISIBLE => "Invisible",
        _ => return None,
    })
}

fn nbt_flag(data: &Compound, key: &str) -> bool {
    data.get(key).and_then(Tag::as_i64).is_some_and(|v| v != 0)
}

/// A flag of a decoration or armor stand from its entity data.
fn decor_flag(entities: &Entities, id: EntityId, key: &str) -> Result<f64> {
    let data = crate::commands::entity_nbt(entities, id).ok_or(status::NOT_FOUND)?;
    Ok(flag(nbt_flag(&data, key)))
}

/// Sets a flag of a decoration or armor stand as `/data merge entity` would; `UNSUPPORTED`
/// when the entity does not keep it.
fn set_decor_flag(entities: &mut Entities, id: EntityId, key: &str, on: bool) -> Result<()> {
    let old = crate::commands::entity_nbt(entities, id).ok_or(status::NOT_FOUND)?;
    if nbt_flag(&old, key) == on {
        return Ok(());
    }
    let mut new = old.clone();
    new.insert(key, on);
    crate::commands::apply_entity_nbt(entities, id, &new).map_err(|_| status::INVALID_ARGUMENT)?;
    let now = crate::commands::entity_nbt(entities, id).ok_or(status::NOT_FOUND)?;
    if nbt_flag(&now, key) == on {
        Ok(())
    } else {
        Err(status::UNSUPPORTED)
    }
}

/// The portal cooldown of a player or another entity.
fn portal_cooldown(entities: &Entities, id: EntityId) -> i32 {
    match entities.player(id) {
        Some(p) => p.survival.portal.cooldown,
        None => entities.portals.get(&id).map_or(0, |p| p.cooldown),
    }
}

pub(super) fn entity_get(w: &impl ReadWorld, entity: u128, prop: u32) -> Result<f64> {
    use entity_prop as p;
    let entities = w.entities()?;
    let id = any_entity(entities, entity)?;
    let entity_ref = entities.store.get(id).ok_or(status::NOT_FOUND)?;
    if prop == p::NETWORK_ID {
        return Ok(f64::from(entity_ref.network_id().0));
    }
    let flags = entity_ref.flags();
    match prop {
        p::ON_GROUND => return Ok(flag(flags.contains(EntityFlags::ON_GROUND))),
        p::SNEAKING => return Ok(flag(flags.contains(EntityFlags::CROUCHING))),
        p::SPRINTING => return Ok(flag(flags.contains(EntityFlags::SPRINTING))),
        p::PORTAL_COOLDOWN => return Ok(f64::from(portal_cooldown(entities, id))),
        _ => {}
    }
    if let Some(key) = decor_key(prop)
        && decor_like(entities, id)
    {
        return decor_flag(entities, id, key);
    }
    match prop {
        p::NO_GRAVITY => return Ok(flag(flags.contains(EntityFlags::NO_GRAVITY))),
        p::INVISIBLE => return Ok(flag(flags.contains(EntityFlags::INVISIBLE))),
        p::GLOWING => return Ok(flag(flags.contains(EntityFlags::GLOWING))),
        _ => {}
    }
    if let Some(player) = entities.player(id) {
        let s = &player.survival;
        return Ok(match prop {
            p::HEALTH => super::world_api::player_get(w, entity, player_prop::HEALTH)?,
            p::MAX_HEALTH => super::world_api::player_get(w, entity, player_prop::MAX_HEALTH)?,
            p::AIR => super::world_api::player_get(w, entity, player_prop::AIR)?,
            p::FIRE_TICKS => super::world_api::player_get(w, entity, player_prop::FIRE_TICKS)?,
            p::ABSORPTION => f64::from(s.absorption),
            p::FREEZE_TICKS => f64::from(s.ticks_frozen),
            p::FALL_DISTANCE => s.fall_distance,
            p::TICKS_LIVED => f64::from(s.tick_count),
            // Players here are never silent, invulnerable as an entity or name-tagged.
            p::SILENT | p::INVULNERABLE | p::CUSTOM_NAME_VISIBLE => 0.0,
            _ => return Err(status::INVALID_ARGUMENT),
        });
    }
    if let Some(m) = entities.mob(id) {
        return Ok(match prop {
            p::HEALTH => f64::from(m.health),
            p::MAX_HEALTH => m.attrs.max_health,
            p::ABSORPTION => f64::from(m.absorption),
            p::FIRE_TICKS => f64::from(m.fire_ticks),
            p::FREEZE_TICKS => f64::from(m.ticks_frozen),
            p::AIR => f64::from(m.air),
            p::FALL_DISTANCE => m.fall_distance,
            p::SILENT => flag(m.silent),
            p::NO_AI => flag(m.no_ai),
            p::PERSISTENT => flag(m.persistent),
            p::AGE if m.kind.ageable() => f64::from(m.age),
            p::TICKS_LIVED => f64::from(m.tick_count),
            // Mobs here keep neither flag: none is invulnerable or shows its name always.
            p::INVULNERABLE | p::CUSTOM_NAME_VISIBLE => 0.0,
            _ => return Err(status::INVALID_ARGUMENT),
        });
    }
    match (prop, entities.decor(id)) {
        (p::TICKS_LIVED, Some(d)) => Ok(f64::from(d.tick_count)),
        _ => Err(status::INVALID_ARGUMENT),
    }
}

/// Sets a property. Health 0 kills as `/kill` does (as `player_set`); flags take 0 or 1;
/// read-only properties and those the entity does not have are `INVALID_ARGUMENT`, those the
/// server does not keep for it `UNSUPPORTED`.
pub(super) fn entity_set(
    w: &mut PluginWorld<'_>,
    entity: u128,
    prop: u32,
    value: f64,
) -> Result<()> {
    use entity_prop as p;
    let id = any_entity(w.0.entities(), entity)?;
    if matches!(
        prop,
        p::MAX_HEALTH | p::ON_GROUND | p::SNEAKING | p::SPRINTING | p::NETWORK_ID
    ) {
        return Err(status::INVALID_ARGUMENT);
    }
    let is_flag = matches!(
        prop,
        p::NO_GRAVITY
            | p::SILENT
            | p::GLOWING
            | p::INVULNERABLE
            | p::NO_AI
            | p::PERSISTENT
            | p::CUSTOM_NAME_VISIBLE
            | p::INVISIBLE
    );
    let on = is_flag && whole(value, 0..=1)? == 1;
    if let Some(key) = decor_key(prop)
        && decor_like(w.0.entities(), id)
    {
        return set_decor_flag(w.0.entities_mut(), id, key, on);
    }
    let is_player = w.0.entities().player(id).is_some();
    let is_mob = w.0.entities().mob(id).is_some();
    match prop {
        // `setNoGravity`, `setInvisible`: the shared flags (an invisibility effect sets the
        // latter again when the effects change, as in Vanilla).
        p::NO_GRAVITY | p::INVISIBLE => {
            let bit = if prop == p::NO_GRAVITY {
                EntityFlags::NO_GRAVITY
            } else {
                EntityFlags::INVISIBLE
            };
            let mut e =
                w.0.entities_mut()
                    .store
                    .get_mut(id)
                    .ok_or(status::NOT_FOUND)?;
            if e.as_ref().flags().contains(bit) != on {
                e.set_flags(bit, on);
                e.mark_data_dirty();
            }
            return Ok(());
        }
        // Players and mobs keep no glowing tag apart from the effect's flag.
        p::GLOWING if is_player || is_mob => return Err(status::UNSUPPORTED),
        p::GLOWING => {
            let mut e =
                w.0.entities_mut()
                    .store
                    .get_mut(id)
                    .ok_or(status::NOT_FOUND)?;
            if e.as_ref().flags().contains(EntityFlags::GLOWING) != on {
                e.set_flags(EntityFlags::GLOWING, on);
                e.mark_data_dirty();
            }
            return Ok(());
        }
        p::PORTAL_COOLDOWN => {
            let cooldown = whole(value, 0..=i32::MAX)?;
            let entities = w.0.entities_mut();
            match entities.player_mut(id) {
                Some(player) => player.survival.portal.cooldown = cooldown,
                None => entities.portals.entry(id).or_default().cooldown = cooldown,
            }
            return Ok(());
        }
        _ => {}
    }
    if is_player {
        return set_player(w, entity, id, prop, value);
    }
    if is_mob {
        return set_mob(w, id, prop, value, on);
    }
    match (prop, w.0.entities_mut().decor_mut(id)) {
        (p::TICKS_LIVED, Some(d)) => {
            d.tick_count = whole(value, 0..=i32::MAX)?;
            Ok(())
        }
        _ => Err(status::INVALID_ARGUMENT),
    }
}

fn set_player(
    w: &mut PluginWorld<'_>,
    uuid: u128,
    id: EntityId,
    prop: u32,
    value: f64,
) -> Result<()> {
    use entity_prop as p;
    let same = match prop {
        p::HEALTH => Some(player_prop::HEALTH),
        p::AIR => Some(player_prop::AIR),
        p::FIRE_TICKS => Some(player_prop::FIRE_TICKS),
        _ => None,
    };
    if let Some(prop) = same {
        return super::world_api::player_set(w, uuid, prop, value);
    }
    let entities = w.0.entities_mut();
    let s = &mut entities.player_mut(id).ok_or(status::NOT_FOUND)?.survival;
    match prop {
        // `setAbsorptionAmount`: clamped to the maximum.
        p::ABSORPTION => {
            let amount = float(value, 0.0..=f32::MAX)?;
            s.absorption = amount.min(s.max_absorption());
        }
        p::FREEZE_TICKS => s.ticks_frozen = whole(value, 0..=i32::MAX)?,
        p::FALL_DISTANCE if value >= 0.0 => {
            s.fall_distance = value;
            return Ok(());
        }
        p::SILENT | p::INVULNERABLE | p::CUSTOM_NAME_VISIBLE => return Err(status::UNSUPPORTED),
        _ => return Err(status::INVALID_ARGUMENT),
    }
    if let Some(mut e) = entities.store.get_mut(id) {
        e.mark_data_dirty();
    }
    Ok(())
}

fn set_mob(w: &mut PluginWorld<'_>, id: EntityId, prop: u32, value: f64, on: bool) -> Result<()> {
    use entity_prop as p;
    if prop == p::HEALTH && value == 0.0 {
        let dimension = w.0.dimension().to_owned();
        crate::commands::kill_any(w.0.level_mut(), w.1, &dimension, id);
        return Ok(());
    }
    let entities = w.0.entities_mut();
    let m = entities.mob_mut(id).ok_or(status::NOT_FOUND)?;
    // Whether the synched entity data changed.
    let mut data_changed = true;
    match prop {
        p::HEALTH => {
            if m.is_dead_or_dying() {
                return Err(status::INVALID_ARGUMENT);
            }
            #[allow(clippy::cast_possible_truncation)]
            let max = m.attrs.max_health as f32;
            m.health = float(value, 0.0..=max)?;
        }
        // `setAbsorptionAmount`: clamped to the maximum.
        p::ABSORPTION => {
            let amount = float(value, 0.0..=f32::MAX)?;
            let max = m.effects.attribute(Attribute::MAX_ABSORPTION, 0.0);
            #[allow(clippy::cast_possible_truncation)]
            let max = max as f32;
            m.absorption = amount.min(max);
            data_changed = false;
        }
        p::FIRE_TICKS => {
            m.fire_ticks = whole(value, i32::MIN..=i32::MAX)?;
            data_changed = false;
        }
        p::FREEZE_TICKS => m.ticks_frozen = whole(value, 0..=i32::MAX)?,
        p::AIR => m.air = whole(value, i32::MIN..=i32::MAX)?,
        p::FALL_DISTANCE if value >= 0.0 => {
            m.fall_distance = value;
            data_changed = false;
        }
        p::SILENT => m.silent = on,
        p::NO_AI => m.no_ai = on,
        p::PERSISTENT => {
            m.persistent = on;
            data_changed = false;
        }
        // `AgeableMob.setAge`: the baby flag and size follow with its next tick.
        p::AGE if m.kind.ageable() => {
            m.age = whole(value, i32::MIN..=i32::MAX)?;
            data_changed = false;
        }
        p::TICKS_LIVED => {
            m.tick_count = whole(value, 0..=i32::MAX)?;
            data_changed = false;
        }
        p::INVULNERABLE | p::CUSTOM_NAME_VISIBLE => return Err(status::UNSUPPORTED),
        _ => return Err(status::INVALID_ARGUMENT),
    }
    if data_changed && let Some(mut e) = entities.store.get_mut(id) {
        e.mark_data_dirty();
    }
    Ok(())
}

/// Where an entity keeps its custom name (network NBT of the `custom_name` component).
fn name_slot(entities: &mut Entities, id: EntityId) -> Option<&mut Option<Box<[u8]>>> {
    if entities.mob(id).is_some() {
        return entities.mob_mut(id).map(|m| &mut m.custom_name);
    }
    if entities.decor(id).is_some() {
        return entities.decor_mut(id).map(|d| &mut d.name);
    }
    if entities.boat(id).is_some() {
        return entities.boat_mut(id).map(|b| &mut b.custom_name);
    }
    entities.minecart_mut(id).map(|c| &mut c.custom_name)
}

fn name_of(entities: &Entities, id: EntityId) -> Option<&[u8]> {
    if let Some(m) = entities.mob(id) {
        return m.custom_name.as_deref();
    }
    if let Some(d) = entities.decor(id) {
        return d.name.as_deref();
    }
    if let Some(b) = entities.boat(id) {
        return b.custom_name.as_deref();
    }
    entities.minecart(id)?.custom_name.as_deref()
}

/// `getCustomName` as JSON.
pub(super) fn entity_name(w: &impl ReadWorld, entity: u128) -> Result<Option<String>> {
    let entities = w.entities()?;
    let id = any_entity(entities, entity)?;
    Ok(name_of(entities, id)
        .and_then(vanillite_game::mob::nbt::component_tag)
        .map(|tag| Component::from_nbt(&tag).to_json_string()))
}

/// `setCustomName`: mobs, decorations, boats and minecarts keep one.
pub(super) fn set_entity_name(
    w: &mut PluginWorld<'_>,
    entity: u128,
    name: Option<Text<'_>>,
) -> Result<()> {
    let encoded = match name {
        Some(text) => {
            let tag = super::component(text).to_nbt();
            let bytes =
                vanillite_game::mob::nbt::tag_component(&tag).ok_or(status::INVALID_ARGUMENT)?;
            if bytes.len() > MAX_NAME_BYTES {
                return Err(status::INVALID_ARGUMENT);
            }
            Some(bytes)
        }
        None => None,
    };
    let entities = w.0.entities_mut();
    let id = any_entity(entities, entity)?;
    let slot = name_slot(entities, id).ok_or(status::UNSUPPORTED)?;
    if *slot != encoded {
        *slot = encoded;
        if let Some(mut e) = entities.store.get_mut(id) {
            e.mark_data_dirty();
        }
    }
    Ok(())
}

pub(super) fn passengers(w: &impl ReadWorld, entity: u128) -> Result<Vec<u128>> {
    let entities = w.entities()?;
    let id = any_entity(entities, entity)?;
    Ok(entities
        .riding
        .passengers_of(id)
        .iter()
        .filter_map(|&p| entities.store.get(p).map(|e| e.uuid()))
        .collect())
}

pub(super) fn vehicle(w: &impl ReadWorld, entity: u128) -> Result<Option<u128>> {
    let entities = w.entities()?;
    let id = any_entity(entities, entity)?;
    Ok(entities
        .vehicle_of(id)
        .and_then(|v| entities.store.get(v))
        .map(|e| e.uuid()))
}

/// `/ride <passenger> mount <vehicle>`, leaving an old vehicle first: `INVALID_ARGUMENT` for a
/// player vehicle or a loop, `ERROR` when the vehicle does not take it.
pub(super) fn mount(w: &mut PluginWorld<'_>, passenger: u128, vehicle: u128) -> Result<()> {
    let entities = w.0.entities();
    let t = any_entity(entities, passenger)?;
    let v = any_entity(entities, vehicle)?;
    if crate::commands::mount_refusal(entities, t, v).is_some() {
        return Err(status::INVALID_ARGUMENT);
    }
    let dimension = w.0.dimension().to_owned();
    if crate::commands::mount(w.0.level_mut(), w.1, &dimension, t, v) {
        Ok(())
    } else {
        Err(status::ERROR)
    }
}

/// `/ride <passenger> dismount`: `NOT_FOUND` when it rides nothing.
pub(super) fn dismount(w: &mut PluginWorld<'_>, passenger: u128) -> Result<()> {
    let entities = w.0.entities();
    let t = any_entity(entities, passenger)?;
    if entities.vehicle_of(t).is_none() {
        return Err(status::NOT_FOUND);
    }
    let dimension = w.0.dimension().to_owned();
    crate::commands::dismount(w.0.level_mut(), w.1, &dimension, t);
    Ok(())
}

/// An attribute of a living entity as `AttributeArgument` reads it (`speed` is
/// `minecraft:speed`): `INVALID_ARGUMENT` when the entity does not have it
/// (`commands.attribute.failed.entity`, `...no_attribute`).
fn attribute_of(entities: &Entities, id: EntityId, name: &str) -> Result<Attribute> {
    let kind = entities.store.get(id).ok_or(status::NOT_FOUND)?.kind();
    let name = qualified(name);
    let index = ATTRIBUTES
        .iter()
        .position(|a| a.name == name)
        .and_then(|i| u8::try_from(i).ok())
        .ok_or(status::INVALID_ARGUMENT)?;
    let living = entities.player(id).is_some() || entities.mob(id).is_some();
    let has = kind
        .info()
        .attributes
        .iter()
        .any(|(a, _)| *a == u16::from(index));
    if living && has {
        Ok(Attribute(index))
    } else {
        Err(status::INVALID_ARGUMENT)
    }
}

/// `/attribute ... base get` and `/attribute ... get`.
pub(super) fn attribute(w: &PluginWorld<'_>, entity: u128, name: &str) -> Result<(f64, f64)> {
    use vanillite_game::effect::attribute_map as map;
    let entities = w.0.entities();
    let id = any_entity(entities, entity)?;
    let a = attribute_of(entities, id, name)?;
    let base = map::base(entities, id, a).map_err(|_| status::INVALID_ARGUMENT)?;
    let value = map::value(entities, id, a).map_err(|_| status::INVALID_ARGUMENT)?;
    Ok((base, value))
}

/// `/attribute ... base set`; a base outside the attribute's range is `INVALID_ARGUMENT`.
pub(super) fn set_attribute_base(
    w: &mut PluginWorld<'_>,
    entity: u128,
    name: &str,
    value: f64,
) -> Result<()> {
    let entities = w.0.entities_mut();
    let id = any_entity(entities, entity)?;
    let a = attribute_of(entities, id, name)?;
    if a.sanitize(value) != value {
        return Err(status::INVALID_ARGUMENT);
    }
    vanillite_game::effect::attribute_map::set_base(entities, id, a, value, false)
        .map_err(|_| status::INVALID_ARGUMENT)
}

/// `/attribute ... modifier add` (`INVALID_ARGUMENT` when the id is already there) and
/// `... modifier remove` (`NOT_FOUND` when it is not); the value and the trackers follow
/// with the entity's next effect sync.
pub(super) fn attribute_modifier(
    w: &mut PluginWorld<'_>,
    entity: u128,
    name: &str,
    modifier: &str,
    change: Option<(f64, u32)>,
) -> Result<()> {
    use vanillite_game::effect::attribute_map as map;
    let entities = w.0.entities_mut();
    let id = any_entity(entities, entity)?;
    let a = attribute_of(entities, id, name)?;
    let Some((amount, operation)) = change else {
        return map::remove_modifier(entities, id, a, modifier, false)
            .map_err(|_| status::NOT_FOUND);
    };
    let operation = match operation {
        attribute_op::ADD_VALUE => Operation::AddValue,
        attribute_op::ADD_MULTIPLIED_BASE => Operation::AddMultipliedBase,
        attribute_op::ADD_MULTIPLIED_TOTAL => Operation::AddMultipliedTotal,
        _ => return Err(status::INVALID_ARGUMENT),
    };
    if !amount.is_finite() {
        return Err(status::INVALID_ARGUMENT);
    }
    map::add_modifier(entities, id, a, modifier, amount, operation, false)
        .map_err(|_| status::INVALID_ARGUMENT)
}

/// The mob of a UUID: `INVALID_ARGUMENT` for another entity.
fn mob_id(entities: &Entities, uuid: u128) -> Result<EntityId> {
    let id = any_entity(entities, uuid)?;
    if entities.mob(id).is_some() {
        Ok(id)
    } else {
        Err(status::INVALID_ARGUMENT)
    }
}

/// `Mob.getTarget` (the attack target memory of brain mobs).
pub(super) fn mob_target(w: &PluginWorld<'_>, mob: u128) -> Result<Option<u128>> {
    let entities = w.0.entities();
    let id = mob_id(entities, mob)?;
    Ok(plugin::target_of(entities.mob(id))
        .and_then(|t| entities.store.get(t))
        .map(|e| e.uuid()))
}

/// `Mob.setTarget` with the target event (reason `PLUGIN`): `INVALID_ARGUMENT` for a target
/// it cannot attack (`asValidTarget`), `ERROR` when a plugin cancelled it.
pub(super) fn set_mob_target(
    w: &mut PluginWorld<'_>,
    mob: u128,
    target: Option<u128>,
) -> Result<()> {
    let level = w.0.level_mut();
    let id = mob_id(&level.entities, mob)?;
    let target = match target {
        None => None,
        Some(uuid) => {
            let t = any_entity(&level.entities, uuid)?;
            if !plugin::valid_target(&level.entities, id, t) {
                return Err(status::INVALID_ARGUMENT);
            }
            Some(t)
        }
    };
    if plugin::target_of(level.entities.mob(id)) == target {
        return Ok(());
    }
    let hooks = level.hooks_for(event::ENTITY_TARGET);
    if plugin::change_target(
        hooks.as_deref(),
        &mut level.entities,
        id,
        target,
        abi::target_reason::PLUGIN,
    ) {
        Ok(())
    } else {
        Err(status::ERROR)
    }
}

/// `PathNavigation.moveTo(x, y, z, speed)` of the mob's own navigation; a speed of 0 or less
/// stops it (`navigation.stop()`). `NOT_FOUND` when it finds no path.
pub(super) fn navigate(w: &mut PluginWorld<'_>, mob: u128, to: [f64; 3], speed: f64) -> Result<()> {
    let pos = position(to)?;
    let id = mob_id(w.0.entities(), mob)?;
    if speed <= 0.0 {
        if let Some(m) = w.0.entities_mut().mob_mut(id) {
            m.nav.stop();
        }
        return Ok(());
    }
    let dimension = w.0.dimension().to_owned();
    let found =
        WorldSurvival::with_entities(w.0.level_mut(), w.1, &dimension, |level, ents, env, cx| {
            let mut t = Tick {
                level,
                ents,
                env,
                cx,
                id,
            };
            t.exists() && vanillite_game::mob::nav::move_to_pos(&mut t, pos, speed)
        });
    if found {
        Ok(())
    } else {
        Err(status::NOT_FOUND)
    }
}

/// A plugin's goal as the game's goal selector runs it, with the world it belongs to.
#[derive(Debug)]
struct GoalAdapter {
    goal: PluginGoal,
    world: u32,
}

impl GoalAdapter {
    fn view(&self, m: &GoalMob) -> abi::GoalMob {
        let vec = |p: [f64; 3]| abi::Vec3 {
            x: p[0],
            y: p[1],
            z: p[2],
        };
        abi::GoalMob {
            size: abi::size_of::<abi::GoalMob>(),
            mob: abi::Uuid::from_u128(m.mob),
            world: self.world,
            pos: vec(m.pos),
            yaw: m.yaw,
            pitch: m.pitch,
            health: m.health,
            on_ground: u32::from(m.on_ground),
            navigating: u32::from(m.navigating),
            target: abi::Uuid::from_u128(m.target),
            target_pos: vec(m.target_pos),
            game_time: m.game_time,
        }
    }
}

impl GamePluginGoal for GoalAdapter {
    fn priority(&self) -> i32 {
        self.goal.priority()
    }

    fn flags(&self) -> u32 {
        self.goal.flags()
    }

    fn is_target(&self) -> bool {
        self.goal.is_target()
    }

    fn can_use(&self, mob: &GoalMob) -> bool {
        self.goal.can_use(&self.view(mob))
    }

    fn can_continue(&self, mob: &GoalMob) -> bool {
        self.goal.can_continue(&self.view(mob))
    }

    fn start(&self, mob: &GoalMob) {
        self.goal.start(&self.view(mob));
    }

    fn stop(&self, mob: &GoalMob) {
        self.goal.stop(&self.view(mob));
    }

    fn tick(&self, mob: &GoalMob) -> GoalAction {
        let a = self.goal.tick(&self.view(mob));
        GoalAction {
            actions: a.actions,
            move_to: [a.move_to.x, a.move_to.y, a.move_to.z],
            speed: a.speed,
            look_at: [a.look_at.x, a.look_at.y, a.look_at.z],
            target: a.target.to_u128(),
        }
    }
}

/// Mobs whose AI runs no goal selector: armor stands and mannequins, the ender dragon.
fn runs_goals(kind: Kind) -> bool {
    kind.family() != Family::Decor && kind != Kind::EnderDragon
}

/// `GoalSelector.addGoal` of a plugin goal (after the mob's Vanilla goals): `UNSUPPORTED` for
/// a mob without a goal selector, `ERROR` when its selector is full.
pub(super) fn add_goal(w: &mut PluginWorld<'_>, mob: u128, goal: PluginGoal) -> Result<u64> {
    let world = u32::from(w.0.id().0);
    let entities = w.0.entities_mut();
    let id = mob_id(entities, mob)?;
    if !entities.mob(id).is_some_and(|m| runs_goals(m.kind)) {
        return Err(status::UNSUPPORTED);
    }
    plugin::add(entities, id, Box::new(GoalAdapter { goal, world })).ok_or(status::ERROR)
}

/// `GoalSelector.removeGoal` of a plugin goal: `NOT_FOUND` when the mob does not have it.
pub(super) fn remove_goal(w: &mut PluginWorld<'_>, mob: u128, goal: u64) -> Result<()> {
    let game_time = w.1.level.game_time();
    let level = w.0.level_mut();
    let id = mob_id(&level.entities, mob)?;
    if plugin::remove(level, id, goal, game_time) {
        Ok(())
    } else {
        Err(status::NOT_FOUND)
    }
}

/// `GoalSelector.removeAllGoals` of the mob's Vanilla goals (`goal_clear` bits), done with its
/// next AI tick. Brain mobs (villagers, piglins, ...) act through their brain, not goals:
/// `UNSUPPORTED`.
pub(super) fn clear_goals(w: &mut PluginWorld<'_>, mob: u128, which: u32) -> Result<()> {
    let entities = w.0.entities_mut();
    let id = mob_id(entities, mob)?;
    let m = entities.mob(id).ok_or(status::NOT_FOUND)?;
    if m.brain.is_some() || !runs_goals(m.kind) {
        return Err(status::UNSUPPORTED);
    }
    if plugin::request_clear(entities, id, which) {
        Ok(())
    } else {
        Err(status::NOT_FOUND)
    }
}
