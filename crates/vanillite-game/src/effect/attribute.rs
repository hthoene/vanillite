//! Attribute values with modifiers (`AttributeInstance`, `AttributeMap`) and their client sync
//! (`ClientboundUpdateAttributesPacket`).
//!
//! An entity keeps its base values where it already has them (the mob `Attrs`, the player
//! defaults); [`Modifiers`] holds the modifiers on top (status effects today, equipment and
//! commands can add theirs) and computes `AttributeInstance.calculateValue`. Attributes whose
//! modifiers changed are marked dirty for `onAttributeUpdated` and the sync.

use vanillite_data::entity::{ATTRIBUTES, EntityTypeId};

use super::Operation;

/// A `minecraft:attribute` registry id (the index into `ATTRIBUTES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Attribute(pub u8);

impl Attribute {
    pub const AIR_DRAG_MODIFIER: Attribute = Attribute(0);
    pub const ARMOR: Attribute = Attribute(1);
    pub const ARMOR_TOUGHNESS: Attribute = Attribute(2);
    pub const ATTACK_DAMAGE: Attribute = Attribute(3);
    pub const ATTACK_KNOCKBACK: Attribute = Attribute(4);
    pub const ATTACK_SPEED: Attribute = Attribute(5);
    pub const EXPLOSION_KNOCKBACK_RESISTANCE: Attribute = Attribute(12);
    pub const FLYING_SPEED: Attribute = Attribute(15);
    pub const FRICTION_MODIFIER: Attribute = Attribute(17);
    pub const MOVEMENT_EFFICIENCY: Attribute = Attribute(25);
    pub const STEP_HEIGHT: Attribute = Attribute(33);
    pub const WATER_MOVEMENT_EFFICIENCY: Attribute = Attribute(37);
    pub const BLOCK_BREAK_SPEED: Attribute = Attribute(7);
    pub const FALL_DAMAGE_MULTIPLIER: Attribute = Attribute(14);
    pub const FOLLOW_RANGE: Attribute = Attribute(16);
    pub const GRAVITY: Attribute = Attribute(18);
    pub const JUMP_STRENGTH: Attribute = Attribute(19);
    pub const KNOCKBACK_RESISTANCE: Attribute = Attribute(20);
    pub const LUCK: Attribute = Attribute(21);
    pub const MAX_ABSORPTION: Attribute = Attribute(22);
    pub const MAX_HEALTH: Attribute = Attribute(23);
    pub const MOVEMENT_SPEED: Attribute = Attribute(26);
    pub const OXYGEN_BONUS: Attribute = Attribute(28);
    pub const SAFE_FALL_DISTANCE: Attribute = Attribute(29);
    pub const SCALE: Attribute = Attribute(30);
    pub const WAYPOINT_TRANSMIT_RANGE: Attribute = Attribute(38);
    pub const WAYPOINT_RECEIVE_RANGE: Attribute = Attribute(39);

    /// The registry name.
    pub fn name(self) -> &'static str {
        ATTRIBUTES
            .get(usize::from(self.0))
            .map_or("minecraft:luck", |a| a.name)
    }

    /// `Attribute.isClientSyncable`.
    pub fn syncable(self) -> bool {
        ATTRIBUTES
            .get(usize::from(self.0))
            .is_some_and(|a| a.syncable)
    }

    /// The attribute's own default (`Attribute.getDefaultValue`).
    pub fn default_value(self) -> f64 {
        ATTRIBUTES
            .get(usize::from(self.0))
            .map_or(0.0, |a| a.default)
    }

    /// The base value an entity type starts with (`DefaultAttributes.getSupplier(type)`).
    pub fn base_of(self, kind: EntityTypeId) -> f64 {
        kind.info()
            .attributes
            .iter()
            .find(|(a, _)| *a == u16::from(self.0))
            .map_or_else(|| self.default_value(), |&(_, v)| v)
    }

    /// `RangedAttribute.sanitizeValue`: NaN becomes the minimum, the rest is clamped.
    pub fn sanitize(self, value: f64) -> f64 {
        let (min, max) = RANGES
            .get(usize::from(self.0))
            .copied()
            .unwrap_or((f64::MIN, f64::MAX));
        if value.is_nan() {
            min
        } else {
            value.clamp(min, max)
        }
    }
}

/// `RangedAttribute` minimum and maximum, by registry id.
const RANGES: [(f64, f64); 40] = [
    (0.0, 2048.0),     // air_drag_modifier
    (0.0, 30.0),       // armor
    (0.0, 20.0),       // armor_toughness
    (0.0, 2048.0),     // attack_damage
    (0.0, 5.0),        // attack_knockback
    (0.0, 1024.0),     // attack_speed
    (0.0, 512.0),      // below_name_distance
    (0.0, 1024.0),     // block_break_speed
    (0.0, 64.0),       // block_interaction_range
    (0.0, 1.0),        // bounciness
    (0.0, 1024.0),     // burning_time
    (0.0, 32.0),       // camera_distance
    (0.0, 1.0),        // explosion_knockback_resistance
    (0.0, 64.0),       // entity_interaction_range
    (0.0, 100.0),      // fall_damage_multiplier
    (0.0, 1024.0),     // flying_speed
    (0.0, 2048.0),     // follow_range
    (0.0, 2048.0),     // friction_modifier
    (-1.0, 1.0),       // gravity
    (0.0, 32.0),       // jump_strength
    (-2.0, 1.0),       // knockback_resistance
    (-1024.0, 1024.0), // luck
    (0.0, 2048.0),     // max_absorption
    (1.0, 1024.0),     // max_health
    (0.0, 1024.0),     // mining_efficiency
    (0.0, 1.0),        // movement_efficiency
    (0.0, 1024.0),     // movement_speed
    (0.0, 512.0),      // name_tag_distance
    (0.0, 1024.0),     // oxygen_bonus
    (-1024.0, 1024.0), // safe_fall_distance
    (0.0625, 16.0),    // scale
    (0.0, 1.0),        // sneaking_speed
    (0.0, 1.0),        // spawn_reinforcements
    (0.0, 10.0),       // step_height
    (0.0, 20.0),       // submerged_mining_speed
    (0.0, 1.0),        // sweeping_damage_ratio
    (0.0, 2048.0),     // tempt_range
    (0.0, 1.0),        // water_movement_efficiency
    (0.0, 6.0e7),      // waypoint_transmit_range
    (0.0, 6.0e7),      // waypoint_receive_range
];

/// One `AttributeModifier` on an attribute.
#[derive(Clone, Debug, PartialEq)]
pub struct Modifier {
    pub attribute: Attribute,
    /// The modifier id (`minecraft:effect.speed`, …); one modifier per id and attribute.
    pub id: Box<str>,
    pub amount: f64,
    pub operation: Operation,
}

/// The modifiers of one entity with a dirty mask per attribute
/// (`AttributeMap.getAttributesToUpdate` / `getAttributesToSync`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Modifiers {
    list: Vec<Modifier>,
    /// Base values set by a command or plugin (`AttributeInstance.setBaseValue`) for the
    /// attributes the entity keeps no field of: all of a player's, a mob's beyond its `Attrs`
    /// (`minecraft:scale`, ...). `None` for nearly every entity: no allocation.
    bases: Option<Box<[(Attribute, f64)]>>,
    /// Attributes whose value may have changed since the last [`Self::take_dirty`].
    dirty: u64,
}

impl Modifiers {
    /// Whether no modifier and no own base value is set (every value is the default's).
    pub fn is_empty(&self) -> bool {
        self.list.is_empty() && self.bases.is_none()
    }

    /// The own base value of `attribute`, if one is set.
    pub fn base(&self, attribute: Attribute) -> Option<f64> {
        self.bases
            .as_ref()?
            .iter()
            .find(|(a, _)| *a == attribute)
            .map(|&(_, v)| v)
    }

    /// The own base values.
    pub fn bases(&self) -> impl Iterator<Item = (Attribute, f64)> + '_ {
        self.bases.iter().flat_map(|b| b.iter().copied())
    }

    /// `setBaseValue` of an attribute the entity keeps no field of.
    pub fn set_base(&mut self, attribute: Attribute, value: f64) {
        self.mark(attribute);
        if let Some(b) = self
            .bases
            .as_mut()
            .and_then(|b| b.iter_mut().find(|(a, _)| *a == attribute))
        {
            b.1 = value;
            return;
        }
        let mut bases = self.bases.take().map(Vec::from).unwrap_or_default();
        bases.push((attribute, value));
        self.bases = Some(bases.into_boxed_slice());
    }

    /// Drops the own base value of `attribute` (back to the type's default).
    pub fn clear_base(&mut self, attribute: Attribute) {
        let Some(bases) = self.bases.take() else {
            return;
        };
        let mut bases = Vec::from(bases);
        bases.retain(|(a, _)| *a != attribute);
        self.bases = (!bases.is_empty()).then(|| bases.into_boxed_slice());
        self.mark(attribute);
    }

    /// Whether only own base values are set, no modifier.
    pub fn has_modifiers(&self) -> bool {
        !self.list.is_empty()
    }

    /// All modifiers.
    pub fn iter(&self) -> impl Iterator<Item = &Modifier> {
        self.list.iter()
    }

    /// The modifiers of one attribute.
    pub fn of(&self, attribute: Attribute) -> impl Iterator<Item = &Modifier> {
        self.list.iter().filter(move |m| m.attribute == attribute)
    }

    fn mark(&mut self, attribute: Attribute) {
        self.dirty |= 1u64.checked_shl(u32::from(attribute.0)).unwrap_or(0);
    }

    /// Marks an attribute whose base value changed for `onAttributeUpdated` and the sync.
    pub fn touch(&mut self, attribute: Attribute) {
        self.mark(attribute);
    }

    /// `removeModifier(id)` + `addPermanentModifier`: sets the modifier `id` of `attribute`.
    pub fn set(&mut self, attribute: Attribute, id: &str, amount: f64, operation: Operation) {
        self.mark(attribute);
        if let Some(m) = self
            .list
            .iter_mut()
            .find(|m| m.attribute == attribute && &*m.id == id)
        {
            m.amount = amount;
            m.operation = operation;
            return;
        }
        self.list.push(Modifier {
            attribute,
            id: id.into(),
            amount,
            operation,
        });
    }

    /// `removeModifier(id)`; returns whether it was set.
    pub fn remove(&mut self, attribute: Attribute, id: &str) -> bool {
        let before = self.list.len();
        self.list
            .retain(|m| !(m.attribute == attribute && &*m.id == id));
        let removed = self.list.len() != before;
        if removed {
            self.mark(attribute);
        }
        removed
    }

    /// `AttributeInstance.calculateValue` with `base` as the base value (an own base value
    /// set by [`Self::set_base`] takes its place).
    pub fn value(&self, attribute: Attribute, base: f64) -> f64 {
        let base = match &self.bases {
            None => base,
            Some(_) => self.base(attribute).unwrap_or(base),
        };
        if self.list.is_empty() {
            return attribute.sanitize(base);
        }
        let mut base = base;
        for m in self.of(attribute) {
            if m.operation == Operation::AddValue {
                base += m.amount;
            }
        }
        let mut result = base;
        for m in self.of(attribute) {
            if m.operation == Operation::AddMultipliedBase {
                result += base * m.amount;
            }
        }
        for m in self.of(attribute) {
            if m.operation == Operation::AddMultipliedTotal {
                result *= 1.0 + m.amount;
            }
        }
        attribute.sanitize(result)
    }

    /// The attributes changed since the last call, cleared.
    pub fn take_dirty(&mut self) -> Vec<Attribute> {
        let mut out = Vec::new();
        let mut bits = std::mem::take(&mut self.dirty);
        while bits != 0 {
            let i = bits.trailing_zeros();
            bits &= bits.wrapping_sub(1);
            if let Ok(i) = u8::try_from(i) {
                out.push(Attribute(i));
            }
        }
        out
    }

    /// Whether any attribute changed since the last [`Self::take_dirty`].
    pub fn is_dirty(&self) -> bool {
        self.dirty != 0
    }
}

/// Encodes the `update_attributes` body for `attributes` of the entity `network_id`: per
/// attribute its registry id, base value and modifiers (`AttributeSnapshot`).
pub fn encode_update(
    network_id: i32,
    attributes: &[(Attribute, f64)],
    modifiers: &Modifiers,
) -> Vec<u8> {
    use vanillite_protocol::codec::write_varint;
    let mut out = Vec::with_capacity(16 + attributes.len() * 24);
    write_varint(&mut out, network_id);
    write_varint(&mut out, i32::try_from(attributes.len()).unwrap_or(0));
    for &(attribute, base) in attributes {
        write_varint(&mut out, i32::from(attribute.0));
        out.extend_from_slice(&base.to_be_bytes());
        let list: Vec<&Modifier> = modifiers.of(attribute).collect();
        write_varint(&mut out, i32::try_from(list.len()).unwrap_or(0));
        for m in list {
            let id = m.id.as_bytes();
            write_varint(&mut out, i32::try_from(id.len()).unwrap_or(0));
            out.extend_from_slice(id);
            out.extend_from_slice(&m.amount.to_be_bytes());
            #[allow(clippy::as_conversions)]
            write_varint(&mut out, m.operation as i32);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_match_the_registry() {
        for (a, name) in [
            (Attribute::MOVEMENT_SPEED, "minecraft:movement_speed"),
            (Attribute::ATTACK_SPEED, "minecraft:attack_speed"),
            (Attribute::ATTACK_DAMAGE, "minecraft:attack_damage"),
            (Attribute::MAX_HEALTH, "minecraft:max_health"),
            (Attribute::AIR_DRAG_MODIFIER, "minecraft:air_drag_modifier"),
            (Attribute::ARMOR_TOUGHNESS, "minecraft:armor_toughness"),
            (Attribute::ATTACK_KNOCKBACK, "minecraft:attack_knockback"),
            (
                Attribute::EXPLOSION_KNOCKBACK_RESISTANCE,
                "minecraft:explosion_knockback_resistance",
            ),
            (Attribute::FLYING_SPEED, "minecraft:flying_speed"),
            (Attribute::FRICTION_MODIFIER, "minecraft:friction_modifier"),
            (
                Attribute::MOVEMENT_EFFICIENCY,
                "minecraft:movement_efficiency",
            ),
            (Attribute::STEP_HEIGHT, "minecraft:step_height"),
            (
                Attribute::WATER_MOVEMENT_EFFICIENCY,
                "minecraft:water_movement_efficiency",
            ),
            (Attribute::MAX_ABSORPTION, "minecraft:max_absorption"),
            (
                Attribute::SAFE_FALL_DISTANCE,
                "minecraft:safe_fall_distance",
            ),
            (Attribute::LUCK, "minecraft:luck"),
            (Attribute::GRAVITY, "minecraft:gravity"),
            (Attribute::JUMP_STRENGTH, "minecraft:jump_strength"),
            (Attribute::OXYGEN_BONUS, "minecraft:oxygen_bonus"),
            (Attribute::SCALE, "minecraft:scale"),
            (Attribute::BLOCK_BREAK_SPEED, "minecraft:block_break_speed"),
            (
                Attribute::FALL_DAMAGE_MULTIPLIER,
                "minecraft:fall_damage_multiplier",
            ),
            (Attribute::FOLLOW_RANGE, "minecraft:follow_range"),
            (
                Attribute::KNOCKBACK_RESISTANCE,
                "minecraft:knockback_resistance",
            ),
            (
                Attribute::WAYPOINT_TRANSMIT_RANGE,
                "minecraft:waypoint_transmit_range",
            ),
        ] {
            assert_eq!(a.name(), name);
            assert_eq!(
                vanillite_data::registry::ATTRIBUTE.id(name),
                Some(u16::from(a.0))
            );
        }
    }

    #[test]
    fn modifiers_compute_like_attribute_instance() {
        let mut m = Modifiers::default();
        m.set(
            Attribute::MOVEMENT_SPEED,
            "a",
            0.4,
            Operation::AddMultipliedTotal,
        );
        m.set(
            Attribute::MOVEMENT_SPEED,
            "b",
            -0.15,
            Operation::AddMultipliedTotal,
        );
        let v = m.value(Attribute::MOVEMENT_SPEED, 0.1);
        assert!((v - 0.1 * 1.4 * 0.85).abs() < 1e-12);
        m.set(Attribute::MAX_HEALTH, "h", 8.0, Operation::AddValue);
        assert_eq!(m.value(Attribute::MAX_HEALTH, 20.0), 28.0);
        assert_eq!(
            m.take_dirty(),
            vec![Attribute::MAX_HEALTH, Attribute::MOVEMENT_SPEED]
        );
        assert!(m.remove(Attribute::MAX_HEALTH, "h"));
        assert_eq!(m.value(Attribute::MAX_HEALTH, 20.0), 20.0);
        assert_eq!(m.value(Attribute::MAX_HEALTH, 0.0), 1.0);
    }
}
