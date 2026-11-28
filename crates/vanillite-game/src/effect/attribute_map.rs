//! The attribute map of a living entity as `/attribute` and the plugins see it
//! (`LivingEntity.getAttributes()`, `AttributeMap`, `AttributeInstance`).
//!
//! The values stay where the hot paths read them: a mob's `Attrs` (base values in
//! `base_attrs` while modifiers are active), every other base value in the modifiers' own base
//! values ([`Modifiers::set_base`], `None` for nearly every entity), the modifiers in the
//! effects' [`Modifiers`]. The instances a command or plugin touched (`getInstance`) with their
//! base values and permanent modifiers are kept for the save: a mob's in its
//! [`crate::entities::EntityExtra::attributes`], a player's in
//! [`crate::survival::player::Survival::attributes`]. Changes are marked dirty and reach the
//! entity's value fields and the clients with its next effect sync.

use vanillite_abi::attribute_change;
use vanillite_data::entity::EntityTypeId;
use vanillite_world::entity::EntityId;

use super::Operation;
use super::attribute::{Attribute, Modifier, Modifiers};
use crate::entities::Entities;
use crate::mob::entity_nbt::NbtAttributes;

/// The most modifiers an entity keeps (SEC: a command loop cannot grow it without bound).
pub const MAX_MODIFIERS: usize = 256;

/// Why an attribute operation failed (`AttributeCommand`'s errors).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeError {
    /// `commands.attribute.failed.entity`: no living entity.
    NotLiving,
    /// `commands.attribute.failed.no_attribute`: the type has no such attribute.
    NoAttribute,
    /// `commands.attribute.failed.no_modifier`.
    NoModifier,
    /// `commands.attribute.failed.modifier_already_present`.
    ModifierPresent,
    /// Too many modifiers ([`MAX_MODIFIERS`]) or a plugin cancelled the change.
    Refused,
}

type Result<T> = std::result::Result<T, AttributeError>;

/// The type of a living entity that has `attribute` (`AttributeMap.hasAttribute`).
fn living_with(ents: &Entities, id: EntityId, attribute: Attribute) -> Result<EntityTypeId> {
    let kind = ents.store.get(id).ok_or(AttributeError::NotLiving)?.kind();
    if ents.player(id).is_none() && ents.mob(id).is_none() {
        return Err(AttributeError::NotLiving);
    }
    if has_attribute(kind, attribute) {
        Ok(kind)
    } else {
        Err(AttributeError::NoAttribute)
    }
}

/// `AttributeSupplier.hasAttribute`.
pub fn has_attribute(kind: EntityTypeId, attribute: Attribute) -> bool {
    kind.info()
        .attributes
        .iter()
        .any(|&(a, _)| a == u16::from(attribute.0))
}

/// The modifiers holding the entity's values.
fn modifiers(ents: &Entities, id: EntityId) -> Option<&Modifiers> {
    match ents.mob(id) {
        Some(m) => Some(&m.effects.modifiers),
        None => Some(&ents.player(id)?.survival.effects.modifiers),
    }
}

/// `getAttributeBaseValue`.
pub fn base(ents: &Entities, id: EntityId, attribute: Attribute) -> Result<f64> {
    let kind = living_with(ents, id, attribute)?;
    Ok(base_of(ents, id, kind, attribute))
}

fn base_of(ents: &Entities, id: EntityId, kind: EntityTypeId, attribute: Attribute) -> f64 {
    if let Some(m) = ents.mob(id)
        && let Some(v) = super::living::mob_attribute_base(m, attribute)
    {
        return v;
    }
    modifiers(ents, id)
        .and_then(|m| m.base(attribute))
        .unwrap_or_else(|| attribute.base_of(kind))
}

/// `getAttributeValue`: the base with every modifier (a player's equipment's too).
pub fn value(ents: &Entities, id: EntityId, attribute: Attribute) -> Result<f64> {
    let kind = living_with(ents, id, attribute)?;
    let base = base_of(ents, id, kind, attribute);
    if let Some(m) = ents.mob(id) {
        return Ok(m.effects.modifiers.value(attribute, base));
    }
    let Some(p) = ents.player(id) else {
        return Err(AttributeError::NotLiving);
    };
    let own = &p.survival.effects.modifiers;
    let equipment = &p.survival.equipment_modifiers;
    if equipment.of(attribute).next().is_none() {
        return Ok(own.value(attribute, base));
    }
    let mut all = own.clone();
    for m in equipment.of(attribute) {
        all.set(attribute, &m.id, m.amount, m.operation);
    }
    Ok(all.value(attribute, base))
}

/// `AttributeMap.getModifierValue`: the amount of the modifier `modifier`.
pub fn modifier(
    ents: &Entities,
    id: EntityId,
    attribute: Attribute,
    modifier: &str,
) -> Result<f64> {
    living_with(ents, id, attribute)?;
    let find = |m: &Modifiers| {
        m.of(attribute)
            .find(|x| &*x.id == modifier)
            .map(|x| x.amount)
    };
    let own = modifiers(ents, id).and_then(find);
    let equipment = ents
        .player(id)
        .and_then(|p| find(&p.survival.equipment_modifiers));
    own.or(equipment).ok_or(AttributeError::NoModifier)
}

/// The modifier ids of an attribute (the suggestions of `modifier remove`).
pub fn modifier_ids(ents: &Entities, id: EntityId, attribute: Attribute) -> Vec<String> {
    modifiers(ents, id)
        .map(|m| m.of(attribute).map(|x| x.id.to_string()).collect())
        .unwrap_or_default()
}

/// The plugins' [`vanillite_abi::event::ENTITY_ATTRIBUTE_CHANGE`]: whether the change may
/// happen. Without listeners one branch.
fn allowed(
    ents: &Entities,
    id: EntityId,
    attribute: Attribute,
    cause: u32,
    modifier: &str,
    amount: f64,
    operation: Option<Operation>,
) -> bool {
    let Some(hooks) = ents.hooks_for(vanillite_abi::event::ENTITY_ATTRIBUTE_CHANGE) else {
        return true;
    };
    let Some(e) = ents.store.get(id) else {
        return true;
    };
    #[allow(clippy::as_conversions)]
    let mut ev = crate::hooks::Event {
        entity: e.uuid(),
        entity_type: e.kind().name().to_owned(),
        key: attribute.name().to_owned(),
        cause,
        text: modifier.to_owned(),
        amount,
        value: operation.map_or(0, |o| o as i64),
        ..crate::hooks::Event::new(vanillite_abi::event::ENTITY_ATTRIBUTE_CHANGE)
    };
    hooks.event(crate::hooks::HookWorld::entities(ents), &mut ev);
    !ev.cancelled
}

/// The saved instances of the entity, changed by `f` (`getInstance` creates the instance of
/// `attribute` with its current base value first).
fn with_instances<R>(
    ents: &mut Entities,
    id: EntityId,
    attribute: Attribute,
    base: f64,
    f: impl FnOnce(&mut NbtAttributes) -> R,
) -> R {
    if let Some(p) = ents.player_mut(id) {
        let saved = &mut p.survival.attributes;
        saved.entry(attribute, base);
        return f(saved);
    }
    let mut extra = ents.extra(id).cloned().unwrap_or_default();
    extra.attributes.entry(attribute, base);
    let out = f(&mut extra.attributes);
    ents.set_extra(id, extra);
    out
}

/// Sets the base value where the entity keeps it.
fn store_base(
    ents: &mut Entities,
    id: EntityId,
    kind: EntityTypeId,
    attribute: Attribute,
    value: f64,
) {
    if let Some(m) = ents.mob_mut(id) {
        if !super::living::set_mob_attribute_base(m, attribute, value) {
            set_own_base(&mut m.effects.modifiers, kind, attribute, value);
        }
    } else if let Some(p) = ents.player_mut(id) {
        set_own_base(&mut p.survival.effects.modifiers, kind, attribute, value);
    }
}

fn set_own_base(m: &mut Modifiers, kind: EntityTypeId, attribute: Attribute, value: f64) {
    if value.to_bits() == attribute.base_of(kind).to_bits() {
        m.clear_base(attribute);
        m.touch(attribute);
    } else {
        m.set_base(attribute, value);
    }
}

/// `getInstance(attribute).setBaseValue(value)` (`/attribute ... base set`); `event` fires the
/// plugins' event (commands).
pub fn set_base(
    ents: &mut Entities,
    id: EntityId,
    attribute: Attribute,
    value: f64,
    event: bool,
) -> Result<()> {
    let kind = living_with(ents, id, attribute)?;
    let base = base_of(ents, id, kind, attribute);
    with_instances(ents, id, attribute, base, |_| ());
    if event && !allowed(ents, id, attribute, attribute_change::BASE, "", value, None) {
        return Err(AttributeError::Refused);
    }
    store_base(ents, id, kind, attribute, value);
    with_instances(ents, id, attribute, base, |s| {
        if let Some(e) = s.get_mut(attribute) {
            e.base = value;
        }
    });
    Ok(())
}

/// `AttributeMap.resetBaseValue` (`/attribute ... base reset`): an existing instance gets the
/// type's default base value. Returns the base value after.
pub fn reset_base(
    ents: &mut Entities,
    id: EntityId,
    attribute: Attribute,
    event: bool,
) -> Result<f64> {
    let kind = living_with(ents, id, attribute)?;
    let default = attribute.base_of(kind);
    let has_instance = match ents.player(id) {
        Some(p) => p
            .survival
            .attributes
            .0
            .iter()
            .any(|a| a.attribute == attribute),
        None => ents
            .extra(id)
            .is_some_and(|x| x.attributes.0.iter().any(|a| a.attribute == attribute)),
    };
    if has_instance {
        if event
            && !allowed(
                ents,
                id,
                attribute,
                attribute_change::BASE,
                "",
                default,
                None,
            )
        {
            return Err(AttributeError::Refused);
        }
        store_base(ents, id, kind, attribute, default);
        with_instances(ents, id, attribute, default, |s| {
            if let Some(e) = s.get_mut(attribute) {
                e.base = default;
            }
        });
    }
    Ok(base_of(ents, id, kind, attribute))
}

fn modifiers_mut(ents: &mut Entities, id: EntityId) -> Option<&mut Modifiers> {
    if ents.mob(id).is_some() {
        return ents.mob_mut(id).map(|m| &mut m.effects.modifiers);
    }
    ents.player_mut(id)
        .map(|p| &mut p.survival.effects.modifiers)
}

/// `getInstance(attribute).addPermanentModifier` (`/attribute ... modifier add`): fails when
/// the id is there already.
pub fn add_modifier(
    ents: &mut Entities,
    id: EntityId,
    attribute: Attribute,
    modifier: &str,
    amount: f64,
    operation: Operation,
    event: bool,
) -> Result<()> {
    let kind = living_with(ents, id, attribute)?;
    let base = base_of(ents, id, kind, attribute);
    with_instances(ents, id, attribute, base, |_| ());
    let Some(list) = modifiers(ents, id) else {
        return Err(AttributeError::NotLiving);
    };
    if list.of(attribute).any(|m| &*m.id == modifier) {
        return Err(AttributeError::ModifierPresent);
    }
    if list.iter().count() >= MAX_MODIFIERS {
        return Err(AttributeError::Refused);
    }
    if event
        && !allowed(
            ents,
            id,
            attribute,
            attribute_change::MODIFIER_ADD,
            modifier,
            amount,
            Some(operation),
        )
    {
        return Err(AttributeError::Refused);
    }
    if let Some(list) = modifiers_mut(ents, id) {
        list.set(attribute, modifier, amount, operation);
    }
    with_instances(ents, id, attribute, base, |s| {
        if let Some(e) = s.get_mut(attribute) {
            e.modifiers.retain(|m| &*m.id != modifier);
            e.modifiers.push(Modifier {
                attribute,
                id: modifier.into(),
                amount,
                operation,
            });
        }
    });
    Ok(())
}

/// `getInstance(attribute).removeModifier(id)` (`/attribute ... modifier remove`): fails when
/// there is no such modifier.
pub fn remove_modifier(
    ents: &mut Entities,
    id: EntityId,
    attribute: Attribute,
    modifier: &str,
    event: bool,
) -> Result<()> {
    let kind = living_with(ents, id, attribute)?;
    let base = base_of(ents, id, kind, attribute);
    with_instances(ents, id, attribute, base, |_| ());
    let Some(amount) = modifiers(ents, id).and_then(|m| {
        m.of(attribute)
            .find(|x| &*x.id == modifier)
            .map(|x| x.amount)
    }) else {
        return Err(AttributeError::NoModifier);
    };
    if event
        && !allowed(
            ents,
            id,
            attribute,
            attribute_change::MODIFIER_REMOVE,
            modifier,
            amount,
            None,
        )
    {
        return Err(AttributeError::Refused);
    }
    if let Some(list) = modifiers_mut(ents, id) {
        list.remove(attribute, modifier);
    }
    with_instances(ents, id, attribute, base, |s| {
        if let Some(e) = s.get_mut(attribute) {
            e.modifiers.retain(|m| &*m.id != modifier);
        }
    });
    Ok(())
}

/// `LivingEntity.getScale` without the age: the `minecraft:scale` value of a mob or player
/// (1 for nearly every entity: one branch).
pub fn scale_of(m: &Modifiers) -> f64 {
    if m.is_empty() {
        return 1.0;
    }
    m.value(Attribute::SCALE, 1.0)
}
