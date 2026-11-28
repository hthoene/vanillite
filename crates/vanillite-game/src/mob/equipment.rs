//! Mob equipment (`Mob` equipment slots and `DropChances`): what a mob wears and holds, the
//! armour it spawns with (`populateDefaultEquipmentSlots`) and its enchantments, picking up
//! items (`aiStep` looting, `equipItemIfPossible`, `canReplaceCurrentItem`), the equipment it
//! drops when it dies (`dropCustomDeathLoot`) and its entity NBT (`equipment`,
//! `drop_chances`).

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, Vec3};
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::TakeItemEntity;
use vanillite_world::entity::EntityId;

use super::{MobData, Tick};
use crate::item::{self, EquipmentSlot, ItemStack};
use crate::survival::Effect;
use crate::survival::rules::{Difficulty, Rule};

/// `Mob.DEFAULT_EQUIPMENT_DROP_CHANCE`.
pub const DEFAULT_DROP_CHANCE: f32 = 0.085;
/// A drop chance above 1: always drops, undamaged (`DropChances.isPreserved`).
pub const PRESERVED: f32 = 2.0;

/// The slots in `EquipmentSlot` order.
pub const SLOTS: [EquipmentSlot; 8] = [
    EquipmentSlot::MainHand,
    EquipmentSlot::OffHand,
    EquipmentSlot::Feet,
    EquipmentSlot::Legs,
    EquipmentSlot::Chest,
    EquipmentSlot::Head,
    EquipmentSlot::Body,
    EquipmentSlot::Saddle,
];

/// The NBT name of each slot (`EquipmentSlot.getSerializedName`).
const NAMES: [&str; 8] = [
    "mainhand", "offhand", "feet", "legs", "chest", "head", "body", "saddle",
];

/// The equipment of a mob.
#[derive(Clone, Debug, PartialEq)]
pub struct Equipment {
    pub items: [ItemStack; 8],
    pub drop_chances: [f32; 8],
}

impl Default for Equipment {
    fn default() -> Self {
        Equipment {
            items: Default::default(),
            drop_chances: [DEFAULT_DROP_CHANCE; 8],
        }
    }
}

impl Equipment {
    /// `getItemBySlot`.
    pub fn get(&self, slot: EquipmentSlot) -> &ItemStack {
        &self.items[usize::from(slot.ordinal())]
    }

    /// `setItemSlot`.
    pub fn set(&mut self, slot: EquipmentSlot, stack: ItemStack) {
        self.items[usize::from(slot.ordinal())] = stack;
    }

    /// The drop chance of a slot.
    pub fn drop_chance(&self, slot: EquipmentSlot) -> f32 {
        self.drop_chances[usize::from(slot.ordinal())]
    }

    /// `setDropChance(slot, chance)`.
    pub fn set_drop_chance(&mut self, slot: EquipmentSlot, chance: f32) {
        self.drop_chances[usize::from(slot.ordinal())] = chance;
    }

    /// `setGuaranteedDrop`.
    pub fn set_guaranteed_drop(&mut self, slot: EquipmentSlot) {
        self.set_drop_chance(slot, PRESERVED);
    }

    /// Whether any slot holds an item.
    pub fn is_empty(&self) -> bool {
        self.items.iter().all(ItemStack::is_empty)
    }

    /// Whether the main hand holds `item`.
    pub fn holds(&self, item: u16) -> bool {
        let s = self.get(EquipmentSlot::MainHand);
        !s.is_empty() && s.item == item
    }
}

/// An item by name; empty for unknown names.
pub fn stack(name: &str) -> ItemStack {
    vanillite_data::registry::ITEM
        .id(name)
        .map_or_else(ItemStack::default, |id| ItemStack::new(id, 1))
}

/// `Mob.getEquipmentForSlot(slot, type)`: leather, copper, gold, chain, iron, diamond.
pub fn equipment_for_slot(slot: EquipmentSlot, armor_type: i32) -> Option<&'static str> {
    const HEAD: [&str; 6] = [
        "minecraft:leather_helmet",
        "minecraft:copper_helmet",
        "minecraft:golden_helmet",
        "minecraft:chainmail_helmet",
        "minecraft:iron_helmet",
        "minecraft:diamond_helmet",
    ];
    const CHEST: [&str; 6] = [
        "minecraft:leather_chestplate",
        "minecraft:copper_chestplate",
        "minecraft:golden_chestplate",
        "minecraft:chainmail_chestplate",
        "minecraft:iron_chestplate",
        "minecraft:diamond_chestplate",
    ];
    const LEGS: [&str; 6] = [
        "minecraft:leather_leggings",
        "minecraft:copper_leggings",
        "minecraft:golden_leggings",
        "minecraft:chainmail_leggings",
        "minecraft:iron_leggings",
        "minecraft:diamond_leggings",
    ];
    const FEET: [&str; 6] = [
        "minecraft:leather_boots",
        "minecraft:copper_boots",
        "minecraft:golden_boots",
        "minecraft:chainmail_boots",
        "minecraft:iron_boots",
        "minecraft:diamond_boots",
    ];
    let i = usize::try_from(armor_type).ok()?;
    // The switch falls through to the next slot for unknown types.
    match slot {
        EquipmentSlot::Head => HEAD.get(i).or_else(|| CHEST.get(i)),
        EquipmentSlot::Chest => CHEST.get(i).or_else(|| LEGS.get(i)),
        EquipmentSlot::Legs => LEGS.get(i).or_else(|| FEET.get(i)),
        EquipmentSlot::Feet => FEET.get(i),
        _ => None,
    }
    .copied()
}

/// `Mob.populateDefaultEquipmentSlots`: random armour, more likely on higher difficulties.
pub fn populate_default_armor(m: &mut MobData, difficulty: Difficulty, special_multiplier: f32) {
    let random = &mut m.random;
    if random.next_float() >= 0.15 * special_multiplier {
        return;
    }
    let mut armor_type = random.next_int_bounded(3);
    for _ in 1..=3 {
        if random.next_float() < 0.1087 {
            armor_type += 1;
        }
    }
    let partial = if difficulty == Difficulty::Hard {
        0.1
    } else {
        0.25
    };
    let mut first = true;
    for slot in [
        EquipmentSlot::Head,
        EquipmentSlot::Chest,
        EquipmentSlot::Legs,
        EquipmentSlot::Feet,
    ] {
        if !first && m.random.next_float() < partial {
            break;
        }
        first = false;
        if m.equipment.get(slot).is_empty()
            && let Some(name) = equipment_for_slot(slot, armor_type)
        {
            m.equipment.set(slot, stack(name));
        }
    }
}

/// `populateDefaultEquipmentEnchantments`: the weapon (25 %) and each armour piece (50 %),
/// scaled by the special multiplier.
pub fn populate_default_enchantments(m: &mut MobData, special_multiplier: f32) {
    enchant_spawned(m, EquipmentSlot::MainHand, 0.25, special_multiplier);
    for slot in [
        EquipmentSlot::Feet,
        EquipmentSlot::Legs,
        EquipmentSlot::Chest,
        EquipmentSlot::Head,
    ] {
        enchant_spawned(m, slot, 0.5, special_multiplier);
    }
}

/// `enchantSpawnedEquipment`.
pub fn enchant_spawned(m: &mut MobData, slot: EquipmentSlot, chance: f32, special_multiplier: f32) {
    if m.equipment.get(slot).is_empty() {
        return;
    }
    if m.random.next_float() >= chance * special_multiplier {
        return;
    }
    let Some(data) = crate::player::play::game_data() else {
        return;
    };
    let mut s = m.equipment.get(slot).clone();
    data.enchanting
        .enchant_mob_spawn_equipment(&mut s, special_multiplier, &mut m.random);
    m.equipment.set(slot, s);
}

/// The value of an attribute with the item's modifiers on a mob's base value
/// (`getApproximateAttributeWith`).
fn approximate(base: f64, stack: &ItemStack, slot: EquipmentSlot, attribute: &str) -> f64 {
    let mut mods = Vec::new();
    item::attribute_modifiers(stack, slot, attribute, &mut mods);
    item::attribute_value(base, &mods, f64::MIN, f64::MAX)
}

/// `getAttributeValue(name)` of a mob: `base` with the attribute modifiers of its equipment
/// (and their enchantments), clamped to `min..=max`.
pub fn attribute(m: &crate::mob::MobData, name: &str, base: f64, min: f64, max: f64) -> f64 {
    let mut mods = Vec::new();
    for slot in (0..8).filter_map(EquipmentSlot::from_ordinal) {
        item::attribute_modifiers(m.equipment.get(slot), slot, name, &mut mods);
    }
    // The active location based `attribute` effects (Soul Speed).
    if let Some(location) = &m.location {
        let mut list = Vec::new();
        location.modifiers(name, &mut list);
        mods.extend(list.into_iter().map(|(_, op, amount)| (op, amount)));
    }
    if mods.is_empty() {
        return base;
    }
    item::attribute_value(base, &mods, min, max)
}

/// `canReplaceEqualItem`: more enchantments, less damage, a custom name.
pub fn can_replace_equal_item(new: &ItemStack, current: &ItemStack) -> bool {
    let enchants = |s: &ItemStack| item::enchantments(s).len();
    let (n, c) = (enchants(new), enchants(current));
    if n != c {
        return n > c;
    }
    let (nd, cd) = (item::damage(new), item::damage(current));
    if nd != cd {
        return nd < cd;
    }
    let named = |s: &ItemStack| {
        s.component(vanillite_protocol::components::ids::CUSTOM_NAME)
            .is_some()
    };
    named(new) && !named(current)
}

/// `canReplaceCurrentItem` with the preferred weapon tag of the mob (`getPreferredWeaponType`).
pub fn can_replace_current_item(
    m: &MobData,
    new: &ItemStack,
    current: &ItemStack,
    slot: EquipmentSlot,
    preferred_weapon: Option<&str>,
) -> bool {
    if current.is_empty() {
        return true;
    }
    if slot.is_armor() {
        let curse =
            crate::player::play::game_data().is_some_and(|d| d.prevents_armor_change(current));
        if curse {
            return false;
        }
        let armor = |s| approximate(m.attrs.armor, s, slot, "minecraft:armor");
        let tough = |s| {
            approximate(
                m.attrs.armor_toughness,
                s,
                slot,
                "minecraft:armor_toughness",
            )
        };
        let (na, ca) = (armor(new), armor(current));
        #[allow(clippy::float_cmp)]
        if na != ca {
            return na > ca;
        }
        let (nt, ct) = (tough(new), tough(current));
        #[allow(clippy::float_cmp)]
        if nt != ct {
            return nt > ct;
        }
        return can_replace_equal_item(new, current);
    }
    if slot != EquipmentSlot::MainHand {
        return false;
    }
    if let Some(tag) = preferred_weapon {
        let in_tag = |s: &ItemStack| {
            crate::player::play::game_data().is_some_and(|d| d.item_in_tag(s.item, tag))
        };
        let (n, c) = (in_tag(new), in_tag(current));
        if c && !n {
            return false;
        }
        if !c && n {
            return true;
        }
    }
    let damage = |s| approximate(m.attrs.attack_damage, s, slot, "minecraft:attack_damage");
    let (nd, cd) = (damage(new), damage(current));
    #[allow(clippy::float_cmp)]
    if nd != cd {
        return nd > cd;
    }
    can_replace_equal_item(new, current)
}

/// What a family decides about picking up items.
#[derive(Clone, Copy, Debug)]
pub struct PickupRules {
    /// `canHoldItem` / `wantsToPickUp` of the stack.
    pub wants: fn(&MobData, &ItemStack) -> bool,
    /// `getPreferredWeaponType`.
    pub preferred_weapon: Option<&'static str>,
}

impl Default for PickupRules {
    fn default() -> Self {
        PickupRules {
            wants: |_, _| true,
            preferred_weapon: None,
        }
    }
}

/// `equipItemIfPossible`: equips a copy of `stack` and returns what was equipped (empty if
/// nothing). The replaced item may drop.
pub fn equip_item_if_possible(
    t: &mut Tick<'_, '_>,
    stack: &ItemStack,
    rules: &PickupRules,
) -> ItemStack {
    let mut slot = item::equipment_slot(stack);
    let m = t.mob_ref();
    let mut current = m.equipment.get(slot).clone();
    let mut can_replace =
        can_replace_current_item(m, stack, &current, slot, rules.preferred_weapon);
    if slot.is_armor() && !can_replace {
        slot = EquipmentSlot::MainHand;
        current = m.equipment.get(slot).clone();
        can_replace = current.is_empty();
    }
    if !can_replace || !(rules.wants)(m, stack) {
        return ItemStack::default();
    }
    let chance = t.mob_ref().equipment.drop_chance(slot);
    if !current.is_empty() && (t.mob().random.next_float() - 0.1).max(0.0) < chance {
        spawn_at_location(t, current, 0.0);
    }
    let equipped = if slot.is_armor() || matches!(slot, EquipmentSlot::Body | EquipmentSlot::Saddle)
    {
        stack.with_count(1)
    } else {
        stack.clone()
    };
    let m = t.mob();
    m.equipment.set(slot, equipped.clone());
    m.equipment.set_guaranteed_drop(slot);
    m.persistent = true;
    broadcast(t);
    equipped
}

/// Sends the mob's equipment to the players tracking it (`set_equipment` after a change).
pub fn broadcast(t: &mut Tick<'_, '_>) {
    if let Some(effect) = tracking_update(t.ents, t.id) {
        t.env.out.push(effect);
    }
}

/// The `set_equipment` of a mob for the players tracking it.
pub fn tracking_update(ents: &crate::entities::Entities, id: EntityId) -> Option<Effect> {
    let entity_id = ents.store.get(id)?.network_id().0;
    let m = ents.mob(id)?;
    let mut slots: Vec<(u8, ItemStack)> = SLOTS
        .iter()
        .map(|&s| (s.ordinal(), m.equipment.get(s).clone()))
        .collect();
    if m.saddled {
        slots[usize::from(EquipmentSlot::Saddle.ordinal())].1 = stack("minecraft:saddle");
    }
    Some(Effect::Tracking {
        entity: id,
        with_self: false,
        packet: SharedPacket::of(
            &vanillite_protocol::packets::play::clientbound::SetEquipment { entity_id, slots },
        ),
    })
}

/// `spawnAtLocation(stack, yOffset)`: an item entity at the mob with the default motion.
pub fn spawn_at_location(t: &mut Tick<'_, '_>, stack: ItemStack, y_offset: f64) {
    if stack.is_empty() {
        return;
    }
    let p = t.pos();
    let pos = Vec3::new(p.x, p.y + y_offset, p.z);
    let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
    t.env.out.push(Effect::DropItem {
        random: item_random,
        uuid,
        pos,
        velocity,
        y_rot,
        pickup_delay: 10,
        stack,
    });
}

/// `Mob.aiStep`'s looting: picks up the items in reach (`ITEM_PICKUP_REACH` 1, 0, 1) when the
/// mob can pick up loot and mob griefing is on. `pick_up` is the kind's `pickUpItem`
/// (`None`: equip it).
pub fn pick_up_items(
    t: &mut Tick<'_, '_>,
    rules: &PickupRules,
    pick_up: Option<fn(&mut Tick<'_, '_>, EntityId) -> bool>,
    reach: (f64, f64, f64),
) {
    let m = t.mob_ref();
    if !m.can_pick_up_loot || !m.is_alive() || m.dead {
        return;
    }
    if !t.env.rules.bool(Rule::MOB_GRIEFING) {
        return;
    }
    let area: Aabb = t.aabb().inflate(reach.0, reach.1, reach.2);
    let mut found = std::mem::take(&mut t.cx.ids);
    found.clear();
    t.ents.store.for_each_in(&area, false, |id| found.push(id));
    for &id in &found {
        let Some(data) = t.ents.item(id) else {
            continue;
        };
        if data.stack.is_empty() || data.pickup_delay > 0 || t.cx.discarded.contains(&id) {
            continue;
        }
        if !(rules.wants)(t.mob_ref(), &data.stack) {
            continue;
        }
        // Bukkit's `EntityPickupItemEvent`: cancelled, the mob leaves the item.
        if let Some(hooks) = t.level.hooks_for(vanillite_abi::event::ENTITY_PICKUP_ITEM) {
            let (Some(me), Some(it)) = (t.ents.store.get(t.id), t.ents.store.get(id)) else {
                continue;
            };
            let mut ev = crate::hooks::Event {
                entity: me.uuid(),
                entity_type: me.kind().name().to_owned(),
                other: it.uuid(),
                other_type: it.kind().name().to_owned(),
                item: crate::hooks::item_snbt(&data.stack),
                value: i64::from(data.stack.count),
                ..crate::hooks::Event::new(vanillite_abi::event::ENTITY_PICKUP_ITEM)
            };
            hooks.event(crate::hooks::HookWorld::split(t.level, t.ents), &mut ev);
            if ev.cancelled {
                continue;
            }
        }
        if let Some(f) = pick_up {
            f(t, id);
            continue;
        }
        let stack = data.stack.clone();
        let equipped = equip_item_if_possible(t, &stack, rules);
        if equipped.is_empty() {
            continue;
        }
        take(t, id, equipped.count);
    }
    t.cx.ids = found;
}

/// `onItemPickup`, `take(entity, amount)` and the stack shrinking of `pickUpItem`: the
/// thrown item triggers, the pickup animation, then the item entity loses `amount` (and is
/// discarded when empty).
pub fn take(t: &mut Tick<'_, '_>, item: EntityId, amount: i32) {
    let thrown = crate::advancements::trigger::TriggerKind::THROWN_ITEM_PICKED_UP_BY_ENTITY;
    let ents = &*t.ents;
    if let Some((thrower, stack)) = ents
        .item(item)
        .and_then(|d| d.thrower.map(|o| (o, d)))
        .filter(|(o, _)| {
            ents.players_by_uuid
                .get(o)
                .is_some_and(|&p| crate::advancements::view::listens(ents, p, thrown))
        })
        .map(|(o, d)| (o, d.stack.clone()))
    {
        let me = t.id;
        super::animal::item_picked_up(t.ents, me, thrower, &stack);
    }
    let (Some(item_net), Some(me)) = (
        t.ents.store.get(item).map(|e| e.network_id().0),
        t.ents.store.get(t.id).map(|e| e.network_id().0),
    ) else {
        return;
    };
    t.env.out.push(Effect::Tracking {
        entity: item,
        with_self: false,
        packet: SharedPacket::of(&TakeItemEntity {
            item_id: item_net,
            player_id: me,
            amount,
        }),
    });
    let empty = match t.ents.item_mut(item) {
        Some(d) => {
            d.stack.shrink(amount);
            d.stack.is_empty()
        }
        None => return,
    };
    if empty {
        t.cx.discarded.push(item);
    } else if let Some(mut e) = t.ents.store.get_mut(item) {
        e.mark_data_dirty();
    }
}

/// `EnchantmentHelper.has(stack, PREVENT_EQUIPMENT_DROP)`: Curse of Vanishing.
pub fn prevents_drop(stack: &ItemStack) -> bool {
    crate::player::play::game_data().is_some_and(|d| d.prevents_equipment_drop(stack))
}

/// `Mob.dropCustomDeathLoot`'s equipment part: each slot drops with its chance when a player
/// killed the mob (preserved slots always), damaged unless preserved.
pub fn death_drops(
    m: &mut MobData,
    killed_by_player: bool,
    looting: f32,
    out: &mut Vec<ItemStack>,
) {
    for slot in SLOTS {
        let i = usize::from(slot.ordinal());
        let chance = m.equipment.drop_chances[i];
        if chance == 0.0 {
            continue;
        }
        let preserve = chance > 1.0;
        // `EnchantmentHelper.processEquipmentDropChance`: Looting of the killing player.
        let chance = chance + looting;
        // `PREVENT_EQUIPMENT_DROP` (Curse of Vanishing).
        let vanishes = prevents_drop(&m.equipment.items[i]);
        if m.equipment.items[i].is_empty()
            || vanishes
            || !(killed_by_player || preserve)
            || m.random.next_float() >= chance
        {
            continue;
        }
        let mut s = std::mem::take(&mut m.equipment.items[i]);
        if !preserve && item::is_damageable(&s) {
            let max = item::max_damage(&s);
            let inner = m.random.next_int_bounded((max - 3).max(1));
            let d = max - m.random.next_int_bounded(1 + inner);
            item::set_damage(&mut s, d);
        }
        out.push(s);
    }
}

/// The non-empty slots for `set_equipment`.
pub fn visible(m: &MobData) -> Vec<(u8, ItemStack)> {
    SLOTS
        .iter()
        .filter(|s| !m.equipment.get(**s).is_empty())
        .map(|&s| (s.ordinal(), m.equipment.get(s).clone()))
        .collect()
}

/// Writes `equipment` and `drop_chances`.
pub fn save(m: &MobData, out: &mut Compound) {
    save_equipment(&m.equipment, out);
}

/// Writes `equipment` and `drop_chances` of an equipment.
pub fn save_equipment(equipment: &Equipment, out: &mut Compound) {
    let mut eq = Compound::new();
    for (i, name) in NAMES.iter().enumerate() {
        let s = &equipment.items[i];
        if !s.is_empty() {
            eq.insert(*name, crate::inventory::stack_to_nbt(s));
        }
    }
    if !eq.is_empty() {
        out.insert("equipment", eq);
    }
    let mut chances = Compound::new();
    for (i, name) in NAMES.iter().enumerate() {
        let c = equipment.drop_chances[i];
        #[allow(clippy::float_cmp)]
        if c != DEFAULT_DROP_CHANCE {
            chances.insert(*name, c);
        }
    }
    if !chances.is_empty() {
        out.insert("drop_chances", chances);
    }
}

/// Reads `equipment` and `drop_chances`.
pub fn load(m: &mut MobData, tag: &Compound) {
    if let Some(Tag::Compound(eq)) = tag.get("equipment") {
        for (i, name) in NAMES.iter().enumerate() {
            if let Some(Tag::Compound(c)) = eq.get(name) {
                m.equipment.items[i] = crate::inventory::stack_from_nbt(c);
            }
        }
    }
    if let Some(Tag::Compound(c)) = tag.get("drop_chances") {
        for (i, name) in NAMES.iter().enumerate() {
            // `ExtraCodecs.NON_NEGATIVE_FLOAT`: any number (`2.0` in SNBT is a double).
            if let Some(v) = c
                .get(name)
                .and_then(as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0)
            {
                m.equipment.drop_chances[i] = v as f32;
            }
        }
    }
}

/// A numeric tag as a double (vanilla `getFloat` accepts any number tag).
#[allow(clippy::cast_precision_loss, clippy::as_conversions)]
fn as_f64(tag: &Tag) -> Option<f64> {
    Some(match *tag {
        Tag::Byte(v) => f64::from(v),
        Tag::Short(v) => f64::from(v),
        Tag::Int(v) => f64::from(v),
        Tag::Long(v) => v as f64,
        Tag::Float(v) => f64::from(v),
        Tag::Double(v) => v,
        _ => return None,
    })
}

/// A random for equipment rolls in tests.
#[cfg(test)]
pub(crate) fn test_random(seed: i64) -> vanillite_base::random::LegacyRandom {
    vanillite_base::random::LegacyRandom::new(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mob::Kind;

    #[test]
    fn armor_types_fall_through() {
        assert_eq!(
            equipment_for_slot(EquipmentSlot::Head, 5),
            Some("minecraft:diamond_helmet")
        );
        assert_eq!(
            equipment_for_slot(EquipmentSlot::Feet, 0),
            Some("minecraft:leather_boots")
        );
        assert_eq!(equipment_for_slot(EquipmentSlot::MainHand, 0), None);
    }

    #[test]
    fn save_and_load_round_trip() {
        let mut m = MobData::new(Kind::Zombie, 1);
        m.equipment
            .set(EquipmentSlot::Head, stack("minecraft:iron_helmet"));
        m.equipment.set_guaranteed_drop(EquipmentSlot::Head);
        let mut out = Compound::new();
        save(&m, &mut out);
        let mut n = MobData::new(Kind::Zombie, 2);
        load(&mut n, &out);
        assert_eq!(n.equipment, m.equipment);
    }

    #[test]
    fn drop_chances_accept_any_number_tag() {
        let mut chances = Compound::new();
        chances.insert("head", Tag::Int(1));
        chances.insert("chest", Tag::Double(0.5));
        chances.insert("legs", Tag::Byte(1));
        chances.insert("feet", Tag::Short(0));
        chances.insert("mainhand", Tag::Long(1));
        chances.insert("offhand", Tag::Float(0.25));
        chances.insert("body", Tag::Double(f64::NAN));
        chances.insert("saddle", Tag::String("0.5".into()));
        let mut tag = Compound::new();
        tag.insert("drop_chances", chances);
        let mut m = MobData::new(Kind::Zombie, 1);
        load(&mut m, &tag);
        let d = m.equipment.drop_chances;
        assert_eq!(d[5], 1.0);
        assert_eq!(d[4], 0.5);
        assert_eq!(d[3], 1.0);
        assert_eq!(d[2], 0.0);
        assert_eq!(d[0], 1.0);
        assert_eq!(d[1], 0.25);
        // Non-finite and non-numeric values keep the default.
        assert_eq!(d[6], DEFAULT_DROP_CHANCE);
        assert_eq!(d[7], DEFAULT_DROP_CHANCE);
    }

    #[test]
    fn peaceful_never_populates() {
        let mut m = MobData::new(Kind::Zombie, 3);
        m.random = test_random(3);
        populate_default_armor(&mut m, Difficulty::Easy, 0.0);
        assert!(m.equipment.is_empty());
    }
}
