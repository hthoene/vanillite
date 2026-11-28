//! Enchantments as the anvil, grindstone and enchanting table use them: the `enchantment`
//! registry of the data packs (costs, levels, supported items, exclusive sets, tags) and the
//! default `enchantable` and `repairable` item components.

mod effects;
mod items;
pub mod location;
pub mod world;

pub use effects::*;

use std::collections::HashMap;

use serde_json::Value;
use vanillite_data::registry::ITEM;
use vanillite_protocol::codec::{Reader, write_varint};

use vanillite_base::random::RandomSource;

use crate::item::ItemStack;

/// `Enchantment.Cost`: `base + per_level_above_first * (level - 1)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    /// `base`.
    pub base: i32,
    /// `per_level_above_first`.
    pub per_level: i32,
}

impl Cost {
    /// `calculate(level)`.
    pub fn at(self, level: i32) -> i32 {
        self.base
            .saturating_add(self.per_level.saturating_mul(level.saturating_sub(1)))
    }
}

/// One enchantment of the registry.
#[derive(Clone, Debug, Default)]
pub struct Info {
    /// Its id (e.g. `minecraft:sharpness`).
    pub name: String,
    /// `description`.
    pub description: Option<vanillite_protocol::text::Component>,
    /// `max_level`.
    pub max_level: i32,
    /// `anvil_cost`.
    pub anvil_cost: i32,
    /// `weight`.
    pub weight: i32,
    /// `min_cost`.
    pub min_cost: Cost,
    /// `max_cost`.
    pub max_cost: Cost,
    /// `supported_items`, sorted.
    pub supported: Vec<u16>,
    /// `primary_items`, sorted (`None`: the supported items).
    pub primary: Option<Vec<u16>>,
    /// `exclusive_set`: enchantment ids.
    pub exclusive: Vec<u32>,
    /// In `#minecraft:curse`.
    pub curse: bool,
    /// In `#minecraft:in_enchanting_table`.
    pub in_table: bool,
    /// `slots`: the equipment slot groups it works in, as a bit per `EquipmentSlot` ordinal.
    pub slots: u8,
    /// The `minecraft:repair_with_xp` effects (Mending), or `None` without that component.
    pub repair_with_xp: Option<Vec<ValueEffect>>,
}

impl Info {
    /// Whether the enchantment has `minecraft:repair_with_xp` effects.
    pub fn repairs_with_xp(&self) -> bool {
        self.repair_with_xp.is_some()
    }

    /// `Enchantment.matchingSlot(slot)`.
    pub fn matches_slot(&self, slot: crate::item::EquipmentSlot) -> bool {
        self.slots & (1 << slot.ordinal()) != 0
    }
}

/// A `LevelBasedValue`.
#[derive(Clone, Debug, PartialEq)]
pub enum LevelValue {
    /// A number.
    Constant(f32),
    /// `linear`: `base + per_level_above_first * (level - 1)`.
    Linear { base: f32, per_level: f32 },
    /// `clamped`.
    Clamped {
        value: Box<LevelValue>,
        min: f32,
        max: f32,
    },
    /// `fraction`.
    Fraction(Box<LevelValue>, Box<LevelValue>),
    /// `levels_squared`: `level² + added`.
    LevelsSquared(f32),
    /// `lookup`: `values[level - 1]` or the fallback.
    Lookup(Vec<f32>, Box<LevelValue>),
    /// `exponent`: `base ^ power`.
    Exponent(Box<LevelValue>, Box<LevelValue>),
}

impl LevelValue {
    fn parse(v: &Value) -> Option<LevelValue> {
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let num = |v: &Value| v.as_f64().map(|n| n as f32);
        if let Some(n) = num(v) {
            return Some(LevelValue::Constant(n));
        }
        let field = |k: &str| v.get(k).and_then(LevelValue::parse).map(Box::new);
        let float = |k: &str| v.get(k).and_then(num);
        let kind = v.get("type")?.as_str()?;
        Some(match kind.strip_prefix("minecraft:").unwrap_or(kind) {
            "linear" => LevelValue::Linear {
                base: float("base")?,
                per_level: float("per_level_above_first")?,
            },
            "clamped" => LevelValue::Clamped {
                value: field("value")?,
                min: float("min")?,
                max: float("max")?,
            },
            "fraction" => LevelValue::Fraction(field("numerator")?, field("denominator")?),
            "levels_squared" => LevelValue::LevelsSquared(float("added")?),
            "lookup" => LevelValue::Lookup(
                v.get("values")?
                    .as_array()?
                    .iter()
                    .filter_map(num)
                    .collect(),
                field("fallback")?,
            ),
            "exponent" => LevelValue::Exponent(field("base")?, field("power")?),
            _ => return None,
        })
    }

    /// `calculate(level)`.
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    pub fn calculate(&self, level: i32) -> f32 {
        match self {
            LevelValue::Constant(v) => *v,
            LevelValue::Linear { base, per_level } => base + per_level * (level - 1) as f32,
            LevelValue::Clamped { value, min, max } => value.calculate(level).clamp(*min, *max),
            LevelValue::Fraction(n, d) => {
                let d = d.calculate(level);
                if d == 0.0 {
                    0.0
                } else {
                    n.calculate(level) / d
                }
            }
            LevelValue::LevelsSquared(added) => (level as f32) * (level as f32) + added,
            LevelValue::Lookup(values, fallback) => usize::try_from(level - 1)
                .ok()
                .and_then(|i| values.get(i))
                .copied()
                .unwrap_or_else(|| fallback.calculate(level)),
            LevelValue::Exponent(base, power) => {
                f64::from(base.calculate(level)).powf(f64::from(power.calculate(level))) as f32
            }
        }
    }
}

/// An `EnchantmentValueEffect` without randomness.
#[derive(Clone, Debug, PartialEq)]
pub enum ValueEffect {
    /// `add`.
    Add(LevelValue),
    /// `multiply`.
    Multiply(LevelValue),
    /// `set`.
    Set(LevelValue),
    /// `all_of`.
    AllOf(Vec<ValueEffect>),
    /// `exponential`: `value * base ^ exponent`.
    Exponential(LevelValue, LevelValue),
}

impl ValueEffect {
    fn parse(v: &Value) -> Option<ValueEffect> {
        let field = |k: &str| v.get(k).and_then(LevelValue::parse);
        let kind = v.get("type")?.as_str()?;
        Some(match kind.strip_prefix("minecraft:").unwrap_or(kind) {
            "add" => ValueEffect::Add(field("value")?),
            "multiply" => ValueEffect::Multiply(field("factor")?),
            "set" => ValueEffect::Set(field("value")?),
            "all_of" => ValueEffect::AllOf(
                v.get("effects")?
                    .as_array()?
                    .iter()
                    .filter_map(ValueEffect::parse)
                    .collect(),
            ),
            "exponential" => ValueEffect::Exponential(field("base")?, field("exponent")?),
            _ => return None,
        })
    }

    /// `process(level, random, value)`.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    pub fn process(&self, level: i32, value: f32) -> f32 {
        match self {
            ValueEffect::Add(v) => value + v.calculate(level),
            ValueEffect::Multiply(v) => value * v.calculate(level),
            ValueEffect::Set(v) => v.calculate(level),
            ValueEffect::AllOf(list) => list.iter().fold(value, |acc, e| e.process(level, acc)),
            ValueEffect::Exponential(base, exponent) => {
                (f64::from(value)
                    * f64::from(base.calculate(level)).powf(f64::from(exponent.calculate(level))))
                    as f32
            }
        }
    }
}

/// The `minecraft:repair_with_xp` effects of an enchantment definition: the conditional
/// effects without requirements (their loot conditions are not evaluated).
fn repair_with_xp(json: &Value) -> Option<Vec<ValueEffect>> {
    let list = json
        .get("effects")?
        .get("minecraft:repair_with_xp")?
        .as_array()?;
    Some(
        list.iter()
            .filter(|e| e.get("requirements").is_none())
            .filter_map(|e| e.get("effect").and_then(ValueEffect::parse))
            .collect(),
    )
}

/// `slots`: `EquipmentSlotGroup` names as a bit per `EquipmentSlot` ordinal (main hand 0,
/// off hand 1, feet 2, legs 3, chest 4, head 5, body 6, saddle 7).
fn slot_bits(json: &Value) -> u8 {
    let Some(list) = json.get("slots").and_then(Value::as_array) else {
        return 0;
    };
    list.iter()
        .filter_map(Value::as_str)
        .map(|g| match g {
            "any" => 0xFF,
            "mainhand" => 1,
            "offhand" => 2,
            "hand" => 3,
            "feet" => 1 << 2,
            "legs" => 1 << 3,
            "chest" => 1 << 4,
            "head" => 1 << 5,
            "armor" => 0b0111_1100,
            "body" => 1 << 6,
            "saddle" => 1 << 7,
            _ => 0,
        })
        .fold(0, |a, b| a | b)
}

/// The enchantment data of the loaded data packs.
#[derive(Clone, Debug, Default)]
pub struct Enchanting {
    /// By `minecraft:enchantment` id.
    pub list: Vec<Info>,
    /// `#minecraft:in_enchanting_table` in tag order.
    pub table: Vec<u32>,
    /// `#minecraft:on_mob_spawn_equipment` in tag order.
    pub mob_spawn: Vec<u32>,
    enchantable: HashMap<u16, i32>,
    repairable: HashMap<u16, Vec<u16>>,
    /// Item → `trim_material` registry id (`provides_trim_material`).
    trim_materials: HashMap<u16, u32>,
    /// `trim_pattern` id → registry id.
    trim_patterns: HashMap<String, u32>,
}

/// A holder set of a registry: `"id"`, `"#tag"` or a list, resolved with `tag` and `id`.
fn holder_set<T: Ord>(
    v: Option<&Value>,
    tag: &dyn Fn(&str) -> Vec<T>,
    id: &dyn Fn(&str) -> Option<T>,
) -> Vec<T> {
    let mut out: Vec<T> = match v {
        Some(Value::String(s)) => match s.strip_prefix('#') {
            Some(t) => tag(t),
            None => id(s).into_iter().collect(),
        },
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .filter_map(id)
            .collect(),
        _ => Vec::new(),
    };
    out.sort();
    out.dedup();
    out
}

fn cost(v: Option<&Value>) -> Cost {
    let int = |k: &str| {
        v.and_then(|v| v.get(k))
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(0)
    };
    Cost {
        base: int("base"),
        per_level: int("per_level_above_first"),
    }
}

impl Enchanting {
    /// Loads the registry entries (`(id, json)` in protocol id order), with item tags and
    /// enchantment tags resolved by the callbacks.
    pub fn load(
        entries: &[(String, Value)],
        item_tag: &dyn Fn(&str) -> Vec<u16>,
        enchantment_tag: &dyn Fn(&str) -> Vec<u32>,
    ) -> Enchanting {
        let ids: HashMap<&str, u32> = entries
            .iter()
            .zip(0u32..)
            .map(|((name, _), i)| (name.as_str(), i))
            .collect();
        let item = |s: &str| ITEM.id(s);
        let enchantment = |s: &str| ids.get(s).copied();
        let curses = enchantment_tag("minecraft:curse");
        let table = enchantment_tag("minecraft:in_enchanting_table");
        let list = entries
            .iter()
            .zip(0u32..)
            .map(|((name, json), i)| {
                let int = |k: &str| {
                    json.get(k)
                        .and_then(Value::as_i64)
                        .and_then(|n| i32::try_from(n).ok())
                        .unwrap_or(0)
                };
                Info {
                    name: name.clone(),
                    description: json
                        .get("description")
                        .and_then(|d| vanillite_protocol::text::Component::try_from_json(d).ok()),
                    max_level: int("max_level"),
                    anvil_cost: int("anvil_cost"),
                    weight: int("weight"),
                    min_cost: cost(json.get("min_cost")),
                    max_cost: cost(json.get("max_cost")),
                    supported: holder_set(json.get("supported_items"), item_tag, &item),
                    primary: json
                        .get("primary_items")
                        .map(|p| holder_set(Some(p), item_tag, &item)),
                    exclusive: holder_set(json.get("exclusive_set"), enchantment_tag, &enchantment),
                    curse: curses.contains(&i),
                    in_table: table.contains(&i),
                    slots: slot_bits(json),
                    repair_with_xp: repair_with_xp(json),
                }
            })
            .collect();
        let table = table.clone();
        let mob_spawn = enchantment_tag("minecraft:on_mob_spawn_equipment");
        let named = |n: &str| ITEM.id(&format!("minecraft:{n}"));
        let enchantable = items::ENCHANTABLE
            .iter()
            .filter_map(|(n, v)| Some((named(n)?, i32::from(*v))))
            .collect();
        let repairable = items::REPAIRABLE
            .iter()
            .filter_map(|(n, v)| {
                let set = holder_set(Some(&Value::String((*v).to_owned())), item_tag, &item);
                Some((named(n)?, set))
            })
            .collect();
        Enchanting {
            list,
            table,
            mob_spawn,
            enchantable,
            repairable,
            trim_materials: HashMap::new(),
            trim_patterns: HashMap::new(),
        }
    }

    /// Sets the trim registries (`trim_material` and `trim_pattern` ids in protocol order).
    pub fn set_trims(&mut self, materials: &[String], patterns: &[String]) {
        let named = |n: &str| ITEM.id(&format!("minecraft:{n}"));
        self.trim_materials = items::TRIM_MATERIALS
            .iter()
            .filter_map(|(item, material)| {
                let id = materials.iter().position(|m| m == material)?;
                Some((named(item)?, u32::try_from(id).ok()?))
            })
            .collect();
        self.trim_patterns = patterns
            .iter()
            .zip(0u32..)
            .map(|(p, i)| (p.clone(), i))
            .collect();
    }

    /// `provides_trim_material` of an item: the `trim_material` id.
    pub fn trim_material(&self, stack: &ItemStack) -> Option<u32> {
        use crate::item::{Patched, patched};
        match patched(
            stack,
            vanillite_protocol::components::ids::PROVIDES_TRIM_MATERIAL,
        ) {
            // A registry holder (id + 1); an inline material has no registry id.
            Patched::Value(v) => {
                let id = vanillite_protocol::codec::Reader::new(v).varint().ok()?;
                u32::try_from(id.checked_sub(1)?).ok()
            }
            Patched::Removed => None,
            Patched::Default => self.trim_materials.get(&stack.item).copied(),
        }
    }

    /// The `trim_pattern` id of a pattern name.
    pub fn trim_pattern(&self, name: &str) -> Option<u32> {
        self.trim_patterns.get(name).copied()
    }

    /// The enchantment with id `id`.
    pub fn get(&self, id: u32) -> Option<&Info> {
        self.list.get(usize::try_from(id).ok()?)
    }

    /// `Enchantment.canEnchant(stack)`: the item is supported.
    pub fn can_enchant(&self, id: u32, stack: &ItemStack) -> bool {
        self.get(id)
            .is_some_and(|e| e.supported.binary_search(&stack.item).is_ok())
    }

    /// `Enchantment.isPrimaryItem`.
    pub fn is_primary(&self, id: u32, stack: &ItemStack) -> bool {
        self.get(id).is_some_and(|e| {
            e.primary
                .as_ref()
                .unwrap_or(&e.supported)
                .binary_search(&stack.item)
                .is_ok()
        })
    }

    /// `Enchantment.areCompatible(a, b)`.
    pub fn compatible(&self, a: u32, b: u32) -> bool {
        a != b
            && !self.get(a).is_some_and(|e| e.exclusive.contains(&b))
            && !self.get(b).is_some_and(|e| e.exclusive.contains(&a))
    }

    /// Whether the enchantment is a curse.
    pub fn is_curse(&self, id: u32) -> bool {
        self.get(id).is_some_and(|e| e.curse)
    }

    /// The `enchantable` value of an item (0: not enchantable at a table).
    pub fn enchantable(&self, stack: &ItemStack) -> i32 {
        if stack.is_empty() {
            return 0;
        }
        if let Some(patched) = crate::item::patched_enchantable(stack) {
            return patched.unwrap_or(0);
        }
        self.enchantable.get(&stack.item).copied().unwrap_or(0)
    }

    /// `EnchantmentHelper.selectEnchantment(random, stack, cost, source)`.
    pub fn select(
        &self,
        random: &mut impl RandomSource,
        stack: &ItemStack,
        cost: i32,
        source: &[u32],
    ) -> Vec<(u32, i32)> {
        let mut results = Vec::new();
        let value = self.enchantable(stack);
        if value <= 0 {
            return results;
        }
        let mut cost = cost
            + 1
            + random.next_int_bounded(value / 4 + 1)
            + random.next_int_bounded(value / 4 + 1);
        let span = (random.next_float() + random.next_float() - 1.0) * 0.15;
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        {
            cost = ((cost as f32 + cost as f32 * span).round() as i32).max(1);
        }
        // `getAvailableEnchantmentResults`.
        let is_book = stack.item == vanillite_data::item::ids::BOOK.0;
        let mut available: Vec<(u32, i32, i32)> = Vec::new();
        for &id in source {
            if !(is_book || self.is_primary(id, stack)) {
                continue;
            }
            let Some(e) = self.get(id) else {
                continue;
            };
            for level in (1..=e.max_level).rev() {
                if cost >= e.min_cost.at(level) && cost <= e.max_cost.at(level) {
                    available.push((id, level, e.weight));
                    break;
                }
            }
        }
        let pick = |random: &mut dyn FnMut(i32) -> i32, list: &[(u32, i32, i32)]| {
            let total: i32 = list.iter().map(|e| e.2).sum();
            if total <= 0 {
                return None;
            }
            let mut roll = random(total);
            for e in list {
                roll -= e.2;
                if roll < 0 {
                    return Some((e.0, e.1));
                }
            }
            None
        };
        if available.is_empty() {
            return results;
        }
        if let Some(r) = pick(&mut |n| random.next_int_bounded(n), &available) {
            results.push(r);
        }
        while random.next_int_bounded(50) <= cost {
            if let Some(&(last, _)) = results.last() {
                available.retain(|e| self.compatible(last, e.0));
            }
            if available.is_empty() {
                break;
            }
            if let Some(r) = pick(&mut |n| random.next_int_bounded(n), &available) {
                results.push(r);
            }
            cost /= 2;
        }
        results
    }

    /// `enchantItemFromProvider(stack, MOB_SPAWN_EQUIPMENT, difficulty, random)`
    /// (`EnchantmentsByCostWithDifficulty(#on_mob_spawn_equipment, 5, 17)`).
    pub fn enchant_mob_spawn_equipment(
        &self,
        stack: &mut ItemStack,
        special_multiplier: f32,
        random: &mut impl RandomSource,
    ) {
        #[allow(clippy::cast_possible_truncation)]
        let span = (special_multiplier * 17.0) as i32;
        let cost = random.next_int_between_inclusive(5, 5 + span);
        let source = self.mob_spawn.clone();
        let chosen = self.select(random, stack, cost, &source);
        if chosen.is_empty() {
            return;
        }
        let id = vanillite_protocol::components::ids::ENCHANTMENTS;
        let mut list = read(stack.component(id));
        for (e, level) in chosen {
            match list.iter_mut().find(|x| x.0 == e) {
                Some(x) => x.1 = x.1.max(level),
                None => list.push((e, level)),
            }
        }
        stack.set_component(id, write(&list));
    }

    /// `EnchantmentHelper.modifyDurabilityToRepairFromXp(level, stack, durability)`: the
    /// `repair_with_xp` effects of every enchantment of the stack applied to the experience.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    pub fn durability_to_repair_from_xp(&self, stack: &ItemStack, xp: i32) -> i32 {
        #[allow(clippy::cast_precision_loss)]
        let mut value = xp as f32;
        for (id, level) in crate::item::enchantments(stack) {
            if let Some(effects) = self.get(id).and_then(|e| e.repair_with_xp.as_ref()) {
                for effect in effects {
                    value = effect.process(level, value);
                }
            }
        }
        // `MutableFloat.intValue()`: truncation; NaN gives 0.
        (value as i32).max(0)
    }

    /// `ItemStack.isValidRepairItem(addition)`: the `repairable` component.
    pub fn is_valid_repair_item(&self, stack: &ItemStack, addition: &ItemStack) -> bool {
        if addition.is_empty() {
            return false;
        }
        match crate::item::patched_repairable(stack) {
            Some(Some(crate::consumable::HolderSet::Ids(ids))) => ids.contains(&addition.item),
            Some(Some(crate::consumable::HolderSet::Tag(tag))) => {
                crate::player::play::game_data().is_some_and(|d| d.item_in_tag(addition.item, &tag))
            }
            Some(None) => false,
            None => self
                .repairable
                .get(&stack.item)
                .is_some_and(|items| items.binary_search(&addition.item).is_ok()),
        }
    }
}

/// `EnchantmentHelper.selectEnchantment(random, stack, cost, candidates)`: enchantments for an
/// item at an enchanting cost, drawn by weight from the candidates (in their order) that fit
/// the cost; nothing for items without `enchantable`.
pub fn select_enchantment(
    e: &Enchanting,
    random: &mut dyn crate::loot::LootRandom,
    stack: &ItemStack,
    cost: i32,
    candidates: &[u32],
) -> Vec<(u32, i32)> {
    let enchantable = e.enchantable(stack);
    if enchantable <= 0 {
        return Vec::new();
    }
    let mut cost = cost
        .saturating_add(1)
        .saturating_add(random.next_int(enchantable / 4 + 1))
        .saturating_add(random.next_int(enchantable / 4 + 1));
    let span = (random.next_float() + random.next_float() - 1.0) * 0.15;
    // `Mth.clamp(Math.round(cost + cost * span), 1, MAX_VALUE)`.
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss
    )]
    let rounded = (cost as f32 + cost as f32 * span + 0.5).floor() as i32;
    cost = rounded.max(1);
    let is_book = stack.item == vanillite_data::item::ids::BOOK.0;
    // `getAvailableEnchantmentResults`.
    let mut available: Vec<(u32, i32, i32)> = Vec::new();
    for &id in candidates {
        if !(is_book || e.is_primary(id, stack)) {
            continue;
        }
        let Some(info) = e.get(id) else {
            continue;
        };
        let mut level = info.max_level;
        while level >= 1 {
            if cost >= info.min_cost.at(level) && cost <= info.max_cost.at(level) {
                available.push((id, level, info.weight));
                break;
            }
            level -= 1;
        }
    }
    // `WeightedRandom.getRandomItem`.
    let pick = |random: &mut dyn crate::loot::LootRandom, list: &[(u32, i32, i32)]| {
        let total = list
            .iter()
            .fold(0i32, |sum, &(_, _, w)| sum.saturating_add(w));
        if total <= 0 {
            return None;
        }
        let mut selection = random.next_int(total);
        for &(id, level, weight) in list {
            selection = selection.saturating_sub(weight);
            if selection < 0 {
                return Some((id, level));
            }
        }
        None
    };
    let mut results: Vec<(u32, i32)> = Vec::new();
    if !available.is_empty() {
        if let Some(first) = pick(random, &available) {
            results.push(first);
        }
        while random.next_int(50) <= cost {
            if let Some(&(last, _)) = results.last() {
                available.retain(|&(id, _, _)| e.compatible(last, id));
            }
            if available.is_empty() {
                break;
            }
            if let Some(next) = pick(random, &available) {
                results.push(next);
            }
            cost /= 2;
        }
    }
    results
}

/// An `ItemEnchantments` value: `(enchantment id, level)` in insertion order.
pub fn read(data: Option<&[u8]>) -> Vec<(u32, i32)> {
    let Some(data) = data else {
        return Vec::new();
    };
    let mut r = Reader::new(data);
    let Ok(n) = r.varint() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for _ in 0..n.clamp(0, 256) {
        let (Ok(id), Ok(level)) = (r.varint(), r.varint()) else {
            break;
        };
        if let Ok(id) = u32::try_from(id) {
            out.push((id, level));
        }
    }
    out
}

/// Encodes an `ItemEnchantments` value.
pub fn write(list: &[(u32, i32)]) -> Box<[u8]> {
    let mut out = Vec::new();
    write_varint(&mut out, i32::try_from(list.len()).unwrap_or(0));
    for &(id, level) in list {
        write_varint(&mut out, i32::try_from(id).unwrap_or(0));
        write_varint(&mut out, level);
    }
    out.into_boxed_slice()
}

/// An integer component (`repair_cost`).
pub fn read_int(data: Option<&[u8]>) -> i32 {
    data.and_then(|d| Reader::new(d).varint().ok()).unwrap_or(0)
}

/// Encodes an integer component.
pub fn write_int(value: i32) -> Box<[u8]> {
    let mut out = Vec::new();
    write_varint(&mut out, value);
    out.into_boxed_slice()
}
