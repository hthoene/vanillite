//! Enchantment effects in combat (Vanilla `Enchantment` effect components and
//! `EnchantmentHelper`), read from the `minecraft:enchantment` entries of the data packs, so
//! data packs change and add them: `minecraft:damage`, `minecraft:knockback`,
//! `minecraft:damage_protection`, `minecraft:armor_effectiveness`,
//! `minecraft:smash_damage_per_fallen_block`, `minecraft:post_attack` (`ignite`,
//! `apply_mob_effect`, `damage_entity`, `change_item_damage`),
//! `minecraft:projectile_spawned` (`ignite`) and `minecraft:attributes`, and the ones of
//! projectile weapons: `minecraft:crossbow_charge_time`, `minecraft:crossbow_charging_sounds`,
//! `minecraft:projectile_count`, `minecraft:projectile_spread`, `minecraft:projectile_piercing`
//! and `minecraft:ammo_use`.
//!
//! Requirements are the loot conditions enchantments use: `entity_properties`,
//! `damage_source_properties`, `all_of`, `any_of`, `inverted`, `random_chance` and
//! `match_tool` (by item only, for the item effects); other ones never hold.

use serde_json::Value;
use vanillite_base::random::RandomSource;

use crate::item::{EquipmentSlot, ItemStack};
use crate::predicate::{
    DamageSourcePredicate, EntityFacts, EntityPredicate, EntityTags, SourceFacts,
};
use crate::survival::damage::DamageTypes;

/// `LevelBasedValue`.
#[derive(Clone, Debug, PartialEq)]
pub enum LevelBased {
    Constant(f32),
    Linear {
        base: f32,
        per_level: f32,
    },
    Clamped {
        value: Box<LevelBased>,
        min: f32,
        max: f32,
    },
    Fraction {
        numerator: Box<LevelBased>,
        denominator: Box<LevelBased>,
    },
    LevelsSquared {
        added: f32,
    },
    Lookup {
        values: Box<[f32]>,
        fallback: Box<LevelBased>,
    },
    Exponent {
        base: Box<LevelBased>,
        power: Box<LevelBased>,
    },
}

impl LevelBased {
    /// Parses a level based value (a number is a constant).
    pub fn parse(v: &Value) -> Option<LevelBased> {
        #[allow(clippy::cast_possible_truncation)]
        let f = |v: &Value| v.as_f64().map(|x| x as f32);
        if let Some(c) = f(v) {
            return Some(LevelBased::Constant(c));
        }
        let o = v.as_object()?;
        let sub = |k: &str| o.get(k).and_then(LevelBased::parse).map(Box::new);
        Some(
            match o.get("type")?.as_str()?.trim_start_matches("minecraft:") {
                "linear" => LevelBased::Linear {
                    base: f(o.get("base")?)?,
                    per_level: f(o.get("per_level_above_first")?)?,
                },
                "clamped" => LevelBased::Clamped {
                    value: sub("value")?,
                    min: f(o.get("min")?)?,
                    max: f(o.get("max")?)?,
                },
                "fraction" => LevelBased::Fraction {
                    numerator: sub("numerator")?,
                    denominator: sub("denominator")?,
                },
                "levels_squared" => LevelBased::LevelsSquared {
                    added: f(o.get("added")?)?,
                },
                "lookup" => LevelBased::Lookup {
                    values: o
                        .get("values")?
                        .as_array()?
                        .iter()
                        .map(f)
                        .collect::<Option<Vec<_>>>()?
                        .into(),
                    fallback: sub("fallback")?,
                },
                "exponent" => LevelBased::Exponent {
                    base: sub("base")?,
                    power: sub("power")?,
                },
                _ => return None,
            },
        )
    }

    /// `calculate(level)`.
    pub fn calculate(&self, level: i32) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let lf = level as f32;
        match self {
            LevelBased::Constant(c) => *c,
            LevelBased::Linear { base, per_level } => base + per_level * (lf - 1.0),
            LevelBased::Clamped { value, min, max } => value.calculate(level).clamp(*min, *max),
            LevelBased::Fraction {
                numerator,
                denominator,
            } => {
                let d = denominator.calculate(level);
                if d == 0.0 {
                    0.0
                } else {
                    numerator.calculate(level) / d
                }
            }
            LevelBased::LevelsSquared { added } => lf * lf + added,
            LevelBased::Lookup { values, fallback } => usize::try_from(level - 1)
                .ok()
                .and_then(|i| values.get(i).copied())
                .unwrap_or_else(|| fallback.calculate(level)),
            LevelBased::Exponent { base, power } => {
                #[allow(clippy::cast_possible_truncation)]
                let v =
                    f64::from(base.calculate(level)).powf(f64::from(power.calculate(level))) as f32;
                v
            }
        }
    }
}

/// `EnchantmentValueEffect`.
#[derive(Clone, Debug, PartialEq)]
pub enum ValueEffect {
    Add(LevelBased),
    Multiply(LevelBased),
    Set(LevelBased),
    RemoveBinomial(LevelBased),
    /// `exponential` (`ScaleExponentially`): `value · base^exponent`.
    Exponential {
        base: LevelBased,
        exponent: LevelBased,
    },
    AllOf(Vec<ValueEffect>),
}

impl ValueEffect {
    fn parse(v: &Value) -> Option<ValueEffect> {
        let o = v.as_object()?;
        let lb = |k: &str| o.get(k).and_then(LevelBased::parse);
        Some(
            match o.get("type")?.as_str()?.trim_start_matches("minecraft:") {
                "add" => ValueEffect::Add(lb("value")?),
                "multiply" => ValueEffect::Multiply(lb("factor")?),
                "set" => ValueEffect::Set(lb("value")?),
                "remove_binomial" => ValueEffect::RemoveBinomial(lb("chance")?),
                "exponential" => ValueEffect::Exponential {
                    base: lb("base")?,
                    exponent: lb("exponent")?,
                },
                "all_of" => ValueEffect::AllOf(
                    o.get("effects")?
                        .as_array()?
                        .iter()
                        .map(ValueEffect::parse)
                        .collect::<Option<Vec<_>>>()?,
                ),
                _ => return None,
            },
        )
    }

    /// `process(level, random, value)`.
    pub fn process(&self, level: i32, random: &mut dyn FloatRandom, value: f32) -> f32 {
        match self {
            ValueEffect::Add(v) => value + v.calculate(level),
            ValueEffect::Multiply(v) => value * v.calculate(level),
            ValueEffect::Set(v) => v.calculate(level),
            ValueEffect::RemoveBinomial(chance) => {
                remove_binomial(chance.calculate(level), random, value)
            }
            ValueEffect::Exponential { base, exponent } => {
                // `(float) (inputValue * Math.pow(base, exponent))` in double.
                let pow =
                    f64::from(base.calculate(level)).powf(f64::from(exponent.calculate(level)));
                #[allow(clippy::cast_possible_truncation)]
                let v = (f64::from(value) * pow) as f32;
                v
            }
            ValueEffect::AllOf(list) => list.iter().fold(value, |v, e| e.process(level, random, v)),
        }
    }
}

/// `RemoveBinomial.process(level, random, n)` with chance `p`: above 128 points (when both
/// sides of the split are at least 20) one Gaussian draw, else one float draw per point that
/// removes it below `p`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::as_conversions,
    clippy::neg_cmp_op_on_partial_ord
)]
pub fn remove_binomial(p: f32, random: &mut dyn FloatRandom, n: f32) -> f32 {
    let mut drop: i32 = 0;
    if !(n <= 128.0) && !(n * p < 20.0) && !(n * (1.0 - p) < 20.0) {
        let miu = f64::from(n * p).floor();
        let sigma = f64::from(n * p * (1.0 - p)).sqrt();
        // `(int) Math.round(double)`, then `Math.clamp(long, 0, (int) n)`.
        let rounded = (miu + random.next_gaussian() * sigma + 0.5).floor() as i64;
        drop = rounded.clamp(0, i64::from(n as i32)) as i32;
    } else {
        let mut y = 0;
        while (y as f32) < n {
            if random.next_float() < p {
                drop += 1;
            }
            y += 1;
        }
    }
    n - drop as f32
}

/// The random numbers effects draw.
pub trait FloatRandom {
    fn next_float(&mut self) -> f32;
    fn next_gaussian(&mut self) -> f64;
}

impl<R: RandomSource> FloatRandom for R {
    fn next_float(&mut self) -> f32 {
        RandomSource::next_float(self)
    }
    fn next_gaussian(&mut self) -> f64 {
        RandomSource::next_gaussian(self)
    }
}

/// Which entity of the context a requirement looks at (`LootContext.EntityTarget`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    This,
    Attacker,
    DirectAttacker,
    AttackingPlayer,
}

impl Target {
    fn parse(s: &str) -> Option<Target> {
        Some(match s {
            "this" => Target::This,
            "attacker" => Target::Attacker,
            "direct_attacker" => Target::DirectAttacker,
            "attacking_player" => Target::AttackingPlayer,
            _ => return None,
        })
    }
}

/// A requirement of an effect (the loot conditions enchantments use).
#[derive(Clone, Debug, PartialEq)]
pub enum Requirement {
    Entity(Target, EntityPredicate),
    DamageSource(Box<DamageSourcePredicate>),
    AllOf(Vec<Requirement>),
    AnyOf(Vec<Requirement>),
    Inverted(Box<Requirement>),
    RandomChance(LevelBased),
    /// `match_tool` with an `items` holder set (`"ns:id"`, `"#ns:tag"` or a list); holds for
    /// the item of an item effect's context only.
    MatchTool(Vec<String>),
    /// `weather_check`.
    WeatherCheck {
        raining: Option<bool>,
        thundering: Option<bool>,
    },
    /// `location_check` with a block tag or id (`block.blocks`) and `can_see_sky`.
    LocationCheck {
        blocks: Option<String>,
        can_see_sky: Option<bool>,
    },
    /// `enchantment_active_check` (location based effects only).
    EnchantmentActive(bool),
    /// `match_block` on the hit block (`hit_block` only): the block holder set entries (any
    /// block without) and the property values (an exact value or integer bounds).
    MatchBlock(Option<Vec<String>>, Vec<(String, PropertyMatch)>),
    /// A predicate of the data packs by id (a `Holder<LootItemCondition>` reference), read
    /// when tested.
    Reference(String),
    /// Never holds.
    Unsupported,
}

/// A value matcher of `StatePropertiesPredicate`.
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyMatch {
    Exact(String),
    /// `{min, max}` of an integer property (or a string compared as text).
    Range(Option<String>, Option<String>),
}

impl PropertyMatch {
    fn matches(&self, value: &str) -> bool {
        match self {
            PropertyMatch::Exact(v) => v == value,
            PropertyMatch::Range(min, max) => {
                let Ok(v) = value.parse::<i64>() else {
                    return false;
                };
                let bound = |b: &Option<String>| b.as_ref().map(|b| b.parse::<i64>());
                match (bound(min), bound(max)) {
                    (Some(Err(_)), _) | (_, Some(Err(_))) => false,
                    (lo, hi) => {
                        lo.is_none_or(|l| l.is_ok_and(|l| l <= v))
                            && hi.is_none_or(|h| h.is_ok_and(|h| v <= h))
                    }
                }
            }
        }
    }
}

impl Requirement {
    fn parse(v: &Value) -> Requirement {
        if let Some(id) = v.as_str() {
            return Requirement::Reference(super::world::qualify(id));
        }
        let Some(o) = v.as_object() else {
            return Requirement::Unsupported;
        };
        let list = |k: &str| {
            o.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().map(Requirement::parse).collect())
        };
        let kind = o
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_start_matches("minecraft:");
        match kind {
            "entity_properties" => {
                let target = o
                    .get("entity")
                    .and_then(Value::as_str)
                    .and_then(Target::parse);
                match target {
                    Some(t) => Requirement::Entity(
                        t,
                        EntityPredicate::parse(o.get("predicate").unwrap_or(&Value::Null)),
                    ),
                    None => Requirement::Unsupported,
                }
            }
            "damage_source_properties" => Requirement::DamageSource(Box::new(
                DamageSourcePredicate::parse(o.get("predicate").unwrap_or(&Value::Null)),
            )),
            "all_of" => list("terms").map_or(Requirement::Unsupported, Requirement::AllOf),
            "any_of" => list("terms").map_or(Requirement::Unsupported, Requirement::AnyOf),
            "inverted" => Requirement::Inverted(Box::new(
                o.get("term")
                    .map_or(Requirement::Unsupported, Requirement::parse),
            )),
            // `random_chance` with a number or an `enchantment_level` number provider.
            "random_chance" => {
                o.get("chance")
                    .and_then(|c| {
                        let level_based = c.get("type").and_then(Value::as_str).is_some_and(|t| {
                            t.trim_start_matches("minecraft:") == "enchantment_level"
                        });
                        if level_based {
                            c.get("amount").and_then(LevelBased::parse)
                        } else {
                            LevelBased::parse(c)
                        }
                    })
                    .map_or(Requirement::Unsupported, Requirement::RandomChance)
            }
            "weather_check" => Requirement::WeatherCheck {
                raining: o.get("raining").and_then(Value::as_bool),
                thundering: o.get("thundering").and_then(Value::as_bool),
            },
            "location_check" => {
                let p = o.get("predicate").and_then(Value::as_object);
                let known = p.is_some_and(|p| {
                    p.keys().all(|k| k == "block" || k == "can_see_sky")
                        && p.get("block").is_none_or(|b| {
                            b.as_object()
                                .is_some_and(|b| b.keys().all(|k| k == "blocks"))
                        })
                }) && o.get("offsetX").is_none()
                    && o.get("offsetY").is_none()
                    && o.get("offsetZ").is_none();
                if known {
                    Requirement::LocationCheck {
                        blocks: p
                            .and_then(|p| p.get("block"))
                            .and_then(|b| b.get("blocks"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        can_see_sky: p
                            .and_then(|p| p.get("can_see_sky"))
                            .and_then(Value::as_bool),
                    }
                } else {
                    Requirement::Unsupported
                }
            }
            "enchantment_active_check" => o
                .get("active")
                .and_then(Value::as_bool)
                .map_or(Requirement::Unsupported, Requirement::EnchantmentActive),
            "match_block" => {
                let known = o
                    .keys()
                    .all(|k| matches!(k.as_str(), "type" | "blocks" | "state"));
                let blocks = match o.get("blocks") {
                    None => Some(None),
                    Some(b) => super::world::holder_entries(b).map(Some),
                };
                let properties = match o.get("state") {
                    None => Some(Vec::new()),
                    Some(Value::Object(p)) => p
                        .iter()
                        .map(|(k, v)| {
                            let text = |v: &Value| match v {
                                Value::String(s) => Some(s.clone()),
                                Value::Number(n) => Some(n.to_string()),
                                Value::Bool(b) => Some(b.to_string()),
                                _ => None,
                            };
                            let m = match v {
                                Value::Object(r) => PropertyMatch::Range(
                                    r.get("min").and_then(text),
                                    r.get("max").and_then(text),
                                ),
                                other => PropertyMatch::Exact(text(other)?),
                            };
                            Some((k.clone(), m))
                        })
                        .collect(),
                    Some(_) => None,
                };
                match (known, blocks, properties) {
                    (true, Some(b), Some(p)) => Requirement::MatchBlock(b, p),
                    _ => Requirement::Unsupported,
                }
            }
            "match_tool" => {
                let predicate = o.get("predicate").and_then(Value::as_object);
                let items = predicate
                    .filter(|p| p.len() == 1)
                    .and_then(|p| p.get("items"));
                match items {
                    Some(Value::String(s)) => Requirement::MatchTool(vec![s.clone()]),
                    Some(Value::Array(a)) => Requirement::MatchTool(
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                    ),
                    _ => Requirement::Unsupported,
                }
            }
            _ => Requirement::Unsupported,
        }
    }

    pub(crate) fn test(
        &self,
        cx: &EnchantContext<'_>,
        level: i32,
        random: &mut dyn FloatRandom,
    ) -> bool {
        match self {
            Requirement::Entity(target, p) => {
                let e = match target {
                    Target::This => cx.this.as_ref(),
                    Target::Attacker => cx.attacker.as_ref(),
                    Target::DirectAttacker => cx.direct_attacker.as_ref(),
                    Target::AttackingPlayer => cx.attacking_player.as_ref(),
                };
                p.test(e, cx.tags)
            }
            Requirement::DamageSource(p) => cx
                .source
                .as_ref()
                .is_some_and(|s| p.test(s, cx.types, cx.tags)),
            Requirement::AllOf(l) => l.iter().all(|r| r.test(cx, level, random)),
            Requirement::AnyOf(l) => l.iter().any(|r| r.test(cx, level, random)),
            Requirement::Inverted(r) => !r.test(cx, level, random),
            Requirement::RandomChance(c) => random.next_float() < c.calculate(level),
            Requirement::WeatherCheck {
                raining,
                thundering,
            } => {
                raining.is_none_or(|r| r == cx.raining)
                    && thundering.is_none_or(|t| t == cx.thundering)
            }
            Requirement::LocationCheck {
                blocks,
                can_see_sky,
            } => cx.location.is_some_and(|(block, sky)| {
                can_see_sky.is_none_or(|s| s == sky)
                    && blocks.as_deref().is_none_or(|b| match b.strip_prefix('#') {
                        Some(tag) => crate::player::play::game_data()
                            .is_some_and(|d| d.block_in_tag(block.block().0, tag)),
                        None => block.block().name() == b,
                    })
            }),
            Requirement::EnchantmentActive(active) => cx.enchantment_active == Some(*active),
            Requirement::MatchBlock(blocks, properties) => cx.block_state.is_some_and(|s| {
                blocks.as_ref().is_none_or(|entries| {
                    crate::player::play::game_data().is_some_and(|d| {
                        entries.iter().any(|e| {
                            d.holder_set("minecraft:block", e)
                                .contains(&u32::from(s.block().0))
                        })
                    })
                }) && properties
                    .iter()
                    .all(|(k, m)| s.value(k).is_some_and(|v| m.matches(v)))
            }),
            Requirement::Reference(id) => {
                Self::referenced(id).is_some_and(|r| r.test(cx, level, random))
            }
            Requirement::MatchTool(_) | Requirement::Unsupported => false,
        }
    }

    /// The requirement of a data pack predicate (a list is `all_of`); `None` for an unknown
    /// one or one nested too deep (a cycle).
    fn referenced(id: &str) -> Option<Requirement> {
        thread_local! {
            static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        }
        if DEPTH.get() >= 64 {
            return None;
        }
        let data = crate::player::play::game_data()?;
        let json = data.loot.predicate_json(id)?;
        DEPTH.set(DEPTH.get().saturating_add(1));
        let r = match json {
            Value::Array(list) => Requirement::AllOf(list.iter().map(Requirement::parse).collect()),
            v => Requirement::parse(v),
        };
        DEPTH.set(DEPTH.get().saturating_sub(1));
        Some(r)
    }

    /// The test in an item context (`Enchantment.itemContext`: only the tool is known).
    fn test_item(
        &self,
        tool: Option<&ItemStack>,
        level: i32,
        random: &mut dyn FloatRandom,
    ) -> bool {
        match self {
            Requirement::MatchTool(items) => tool.is_some_and(|t| {
                !t.is_empty()
                    && items.iter().any(|entry| match entry.strip_prefix('#') {
                        Some(tag) => crate::player::play::game_data()
                            .is_some_and(|d| d.item_in_tag(t.item, tag)),
                        None => crate::item::item_id(t).name() == entry,
                    })
            }),
            Requirement::AllOf(l) => l.iter().all(|r| r.test_item(tool, level, random)),
            Requirement::AnyOf(l) => l.iter().any(|r| r.test_item(tool, level, random)),
            Requirement::Inverted(r) => !r.test_item(tool, level, random),
            Requirement::RandomChance(c) => random.next_float() < c.calculate(level),
            Requirement::Reference(id) => {
                Self::referenced(id).is_some_and(|r| r.test_item(tool, level, random))
            }
            Requirement::Entity(..)
            | Requirement::DamageSource(_)
            | Requirement::WeatherCheck { .. }
            | Requirement::LocationCheck { .. }
            | Requirement::EnchantmentActive(_)
            | Requirement::MatchBlock(..)
            | Requirement::Unsupported => false,
        }
    }
}

/// An effect with its requirements (`ConditionalEffect`).
#[derive(Clone, Debug, PartialEq)]
pub struct Conditional<T> {
    pub effect: T,
    pub requirements: Option<Requirement>,
}

/// `minecraft:post_attack` and `minecraft:projectile_spawned` entity effects the game runs.
#[derive(Clone, Debug, PartialEq)]
pub enum EntityEffect {
    /// `ignite`: sets the affected entity on fire for the given seconds.
    Ignite(LevelBased),
    /// `apply_mob_effect`: one of the effects with a duration (seconds) and amplifier between
    /// the bounds (Bane of Arthropods).
    ApplyMobEffect {
        effects: Vec<crate::effect::MobEffect>,
        min_duration: LevelBased,
        max_duration: LevelBased,
        min_amplifier: LevelBased,
        max_amplifier: LevelBased,
    },
    /// `damage_entity`: damage of a type between the bounds, caused by the enchanted item's
    /// owner (Thorns).
    DamageEntity {
        damage_type: String,
        min_damage: LevelBased,
        max_damage: LevelBased,
    },
    /// `change_item_damage`: the enchanted item loses durability.
    ChangeItemDamage(LevelBased),
    /// `explode` with the trigger block interaction and no damage (Wind Burst): a wind burst
    /// at the affected entity.
    WindBurst {
        radius: f32,
        knockback: LevelBased,
        sound: String,
    },
    /// `play_sound`: the sound of the enchantment level (the last one beyond the list), its
    /// volume and pitch drawn from the entity's random.
    PlaySound {
        sounds: Vec<String>,
        volume: super::world::FloatProvider,
        pitch: super::world::FloatProvider,
    },
    /// `spawn_particles`.
    SpawnParticles(Box<super::world::ParticleSpawn>),
    /// The effects that change the world (`replace_block`, `replace_disk`,
    /// `set_block_properties`, `run_function`, `summon_entity`, `explode`), run once the
    /// level is at hand ([`super::world::EnchantAction`]).
    World(Box<super::world::WorldEffect>),
    /// `attribute` (location based effects only): a modifier while the effect is active.
    Attribute(Box<LocationAttribute>),
    /// `apply_exhaustion`: a player's food exhaustion grows.
    ApplyExhaustion(LevelBased),
    /// `apply_impulse`: a push along `direction` turned by the entity's look, scaled per
    /// axis by `coordinate_scale` and by `magnitude` (Lunge).
    ApplyImpulse {
        direction: [f64; 3],
        coordinate_scale: [f64; 3],
        magnitude: LevelBased,
    },
    AllOf(Vec<EntityEffect>),
    /// Definitions that are not understood (unknown types, unsupported parts).
    Unsupported,
}

/// `EnchantmentAttributeEffect`: the modifier `id/<slot>` of an attribute.
#[derive(Clone, Debug, PartialEq)]
pub struct LocationAttribute {
    pub id: String,
    pub attribute: &'static str,
    pub amount: LevelBased,
    /// `AttributeModifier.Operation` ordinal.
    pub operation: u8,
}

impl EntityEffect {
    fn parse(v: &Value) -> EntityEffect {
        let Some(o) = v.as_object() else {
            return EntityEffect::Unsupported;
        };
        match o
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_start_matches("minecraft:")
        {
            "ignite" => o
                .get("duration")
                .and_then(LevelBased::parse)
                .map_or(EntityEffect::Unsupported, EntityEffect::Ignite),
            "apply_mob_effect" => {
                let names: Vec<&str> = match o.get("to_apply") {
                    Some(Value::String(s)) => vec![s.as_str()],
                    Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                };
                let effects = names
                    .iter()
                    .filter_map(|n| crate::effect::MobEffect::by_name(n))
                    .collect();
                let lb = |k: &str| o.get(k).and_then(LevelBased::parse);
                match (
                    lb("min_duration"),
                    lb("max_duration"),
                    lb("min_amplifier"),
                    lb("max_amplifier"),
                ) {
                    (
                        Some(min_duration),
                        Some(max_duration),
                        Some(min_amplifier),
                        Some(max_amplifier),
                    ) => EntityEffect::ApplyMobEffect {
                        effects,
                        min_duration,
                        max_duration,
                        min_amplifier,
                        max_amplifier,
                    },
                    _ => EntityEffect::Unsupported,
                }
            }
            "damage_entity" => {
                let lb = |k: &str| o.get(k).and_then(LevelBased::parse);
                match (
                    o.get("damage_type").and_then(Value::as_str),
                    lb("min_damage"),
                    lb("max_damage"),
                ) {
                    (Some(t), Some(min_damage), Some(max_damage)) => EntityEffect::DamageEntity {
                        damage_type: t.to_owned(),
                        min_damage,
                        max_damage,
                    },
                    _ => EntityEffect::Unsupported,
                }
            }
            "change_item_damage" => o
                .get("amount")
                .and_then(LevelBased::parse)
                .map_or(EntityEffect::Unsupported, EntityEffect::ChangeItemDamage),
            "explode" => {
                let trigger = o
                    .get("block_interaction")
                    .and_then(Value::as_str)
                    .is_some_and(|b| b == "trigger");
                // The look of a wind charge burst (gusts, no block particles).
                let wind_look = o.get("small_particle").and_then(|p| p.get("type"))
                    == Some(&Value::from("minecraft:gust_emitter_small"))
                    && o.get("large_particle").and_then(|p| p.get("type"))
                        == Some(&Value::from("minecraft:gust_emitter_large"))
                    && o.get("block_particles")
                        .is_none_or(|b| b.as_array().is_some_and(Vec::is_empty))
                    && o.get("offset").is_none()
                    && o.get("immune_blocks").is_none_or(|b| {
                        b.as_str() == Some("#minecraft:blocks_wind_charge_explosions")
                    });
                let harmless = o.get("attribute_to_user").is_none()
                    && o.get("damage_type").is_none()
                    && o.get("create_fire").is_none_or(|f| f == false);
                match (
                    o.get("radius").and_then(LevelBased::parse),
                    o.get("knockback_multiplier").and_then(LevelBased::parse),
                    o.get("sound").and_then(Value::as_str),
                ) {
                    (Some(radius), Some(knockback), Some(sound))
                        if trigger && harmless && wind_look =>
                    {
                        EntityEffect::WindBurst {
                            radius: radius.calculate(1),
                            knockback,
                            sound: sound.to_owned(),
                        }
                    }
                    _ => super::world::WorldEffect::parse("explode", o)
                        .map_or(EntityEffect::Unsupported, |w| {
                            EntityEffect::World(Box::new(w))
                        }),
                }
            }
            "play_sound" => {
                let f = |k: &str| o.get(k).map(super::world::FloatProvider::parse);
                let sounds: Vec<String> = match o.get("sound") {
                    Some(Value::String(s)) => vec![s.clone()],
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    _ => Vec::new(),
                };
                let one = super::world::FloatProvider::Constant(1.0);
                match (
                    f("volume").unwrap_or(Some(one.clone())),
                    f("pitch").unwrap_or(Some(one)),
                ) {
                    (Some(volume), Some(pitch)) if !sounds.is_empty() => EntityEffect::PlaySound {
                        sounds,
                        volume,
                        pitch,
                    },
                    _ => EntityEffect::Unsupported,
                }
            }
            "apply_exhaustion" => o
                .get("amount")
                .and_then(LevelBased::parse)
                .map_or(EntityEffect::Unsupported, EntityEffect::ApplyExhaustion),
            "apply_impulse" => {
                let vec3 = |k: &str| {
                    let a = o.get(k)?.as_array()?;
                    match a.as_slice() {
                        [x, y, z] => Some([x.as_f64()?, y.as_f64()?, z.as_f64()?]),
                        _ => None,
                    }
                };
                match (
                    vec3("direction"),
                    vec3("coordinate_scale"),
                    o.get("magnitude").and_then(LevelBased::parse),
                ) {
                    (Some(direction), Some(coordinate_scale), Some(magnitude)) => {
                        EntityEffect::ApplyImpulse {
                            direction,
                            coordinate_scale,
                            magnitude,
                        }
                    }
                    _ => EntityEffect::Unsupported,
                }
            }
            "all_of" => EntityEffect::AllOf(
                o.get("effects")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(EntityEffect::parse).collect())
                    .unwrap_or_default(),
            ),
            "spawn_particles" => super::world::ParticleSpawn::parse(o)
                .map_or(EntityEffect::Unsupported, |p| {
                    EntityEffect::SpawnParticles(Box::new(p))
                }),
            "attribute" => {
                let attribute = o.get("attribute").and_then(Value::as_str).and_then(|name| {
                    vanillite_data::entity::ATTRIBUTES
                        .iter()
                        .find(|a| a.name == super::world::qualify(name))
                        .map(|a| a.name)
                });
                let operation = o.get("operation").and_then(Value::as_str).and_then(|op| {
                    ["add_value", "add_multiplied_base", "add_multiplied_total"]
                        .iter()
                        .position(|x| *x == op)
                        .and_then(|i| u8::try_from(i).ok())
                });
                match (
                    o.get("id").and_then(Value::as_str),
                    attribute,
                    o.get("amount").and_then(LevelBased::parse),
                    operation,
                ) {
                    (Some(id), Some(attribute), Some(amount), Some(operation)) => {
                        EntityEffect::Attribute(Box::new(LocationAttribute {
                            id: super::world::qualify(id),
                            attribute,
                            amount,
                            operation,
                        }))
                    }
                    _ => EntityEffect::Unsupported,
                }
            }
            kind => super::world::WorldEffect::parse(kind, o)
                .map_or(EntityEffect::Unsupported, |w| {
                    EntityEffect::World(Box::new(w))
                }),
        }
    }

    /// `PlaySoundEffect`'s sound for an enchantment level: the level's entry, the last one
    /// beyond the list.
    pub fn sound_of_level(sounds: &[String], level: i32) -> Option<&str> {
        let i = usize::try_from(level.saturating_sub(1))
            .unwrap_or(0)
            .min(sounds.len().saturating_sub(1));
        sounds.get(i).map(String::as_str)
    }

    /// The seconds the affected entity burns (the longest `ignite`), if any.
    pub fn ignite_seconds(&self, level: i32) -> Option<f32> {
        match self {
            EntityEffect::Ignite(d) => Some(d.calculate(level)),
            EntityEffect::AllOf(l) => l
                .iter()
                .filter_map(|e| e.ignite_seconds(level))
                .reduce(f32::max),
            _ => None,
        }
    }
}

/// Who is enchanted / affected by a post attack effect (`EnchantmentTarget`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Party {
    Attacker,
    DamagingEntity,
    Victim,
}

impl Party {
    fn parse(v: Option<&Value>) -> Option<Party> {
        Some(match v?.as_str()? {
            "attacker" => Party::Attacker,
            "damaging_entity" => Party::DamagingEntity,
            "victim" => Party::Victim,
            _ => return None,
        })
    }
}

/// One `minecraft:post_attack` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct PostAttack {
    pub enchanted: Party,
    pub affected: Party,
    pub effect: EntityEffect,
    pub requirements: Option<Requirement>,
}

/// One `minecraft:attributes` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct AttributeEffect {
    pub attribute: &'static str,
    pub amount: LevelBased,
    /// `AttributeModifier.Operation` ordinal.
    pub operation: u8,
}

/// `CrossbowItem.ChargingSounds`: the sound ids of the loading stages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChargingSounds {
    pub start: Option<String>,
    pub mid: Option<String>,
    pub end: Option<String>,
}

impl ChargingSounds {
    fn parse(v: &Value) -> Option<ChargingSounds> {
        let o = v.as_object()?;
        let s = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_owned);
        Some(ChargingSounds {
            start: s("start"),
            mid: s("mid"),
            end: s("end"),
        })
    }
}

/// The parsed combat effects of one enchantment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Enchantment {
    pub id: String,
    /// `slots` as `EquipmentSlotGroup` ordinals.
    pub slots: Vec<u8>,
    pub damage: Vec<Conditional<ValueEffect>>,
    pub knockback: Vec<Conditional<ValueEffect>>,
    pub damage_protection: Vec<Conditional<ValueEffect>>,
    /// `minecraft:damage_immunity` (Frost Walker against `#burn_from_stepping`).
    pub damage_immunity: Vec<Conditional<()>>,
    pub armor_effectiveness: Vec<Conditional<ValueEffect>>,
    pub smash_damage_per_fallen_block: Vec<Conditional<ValueEffect>>,
    pub post_attack: Vec<PostAttack>,
    pub projectile_spawned: Vec<Conditional<EntityEffect>>,
    pub attributes: Vec<AttributeEffect>,
    /// `minecraft:crossbow_charge_time` (one effect without requirements).
    pub crossbow_charge_time: Option<ValueEffect>,
    /// `minecraft:crossbow_charging_sounds`, one entry per level.
    pub crossbow_charging_sounds: Vec<ChargingSounds>,
    pub projectile_count: Vec<Conditional<ValueEffect>>,
    pub projectile_spread: Vec<Conditional<ValueEffect>>,
    pub projectile_piercing: Vec<Conditional<ValueEffect>>,
    pub ammo_use: Vec<Conditional<ValueEffect>>,
    /// `minecraft:item_damage` (Unbreaking).
    pub item_damage: Vec<Conditional<ValueEffect>>,
    /// `minecraft:trident_spin_attack_strength` (Riptide).
    pub trident_spin_attack_strength: Option<ValueEffect>,
    /// `minecraft:trident_sound`, one sound per level.
    pub trident_sound: Vec<String>,
    /// `minecraft:trident_return_acceleration` (Loyalty).
    pub trident_return_acceleration: Vec<Conditional<ValueEffect>>,
    /// `minecraft:fishing_time_reduction` (Lure).
    pub fishing_time_reduction: Vec<Conditional<ValueEffect>>,
    /// `minecraft:fishing_luck_bonus` (Luck of the Sea).
    pub fishing_luck_bonus: Vec<Conditional<ValueEffect>>,
    /// `minecraft:hit_block` (Channeling on lightning rods).
    pub hit_block: Vec<Conditional<EntityEffect>>,
    /// `minecraft:post_piercing_attack` (Lunge).
    pub post_piercing_attack: Vec<Conditional<EntityEffect>>,
    pub block_experience: Vec<Conditional<ValueEffect>>,
    /// `minecraft:mob_experience` (no Vanilla enchantment has it; data packs can add it).
    pub mob_experience: Vec<Conditional<ValueEffect>>,
    /// `minecraft:prevent_armor_change` (Curse of Binding).
    pub prevent_armor_change: bool,
    /// `minecraft:prevent_equipment_drop` (Curse of Vanishing).
    pub prevent_equipment_drop: bool,
    /// `minecraft:tick` (Soul Speed's particles and sound).
    pub tick: Vec<Conditional<EntityEffect>>,
    /// `minecraft:location_changed` (Frost Walker, Soul Speed).
    pub location_changed: Vec<Conditional<EntityEffect>>,
}

/// `EquipmentSlotGroup` names in ordinal order.
const SLOT_GROUPS: [&str; 11] = [
    "any", "mainhand", "offhand", "hand", "feet", "legs", "chest", "head", "armor", "body",
    "saddle",
];

/// `EquipmentSlotGroup.test(slot)`.
pub fn slot_group_matches(group: u8, slot: EquipmentSlot) -> bool {
    use EquipmentSlot as S;
    match group {
        0 => true,
        1 => slot == S::MainHand,
        2 => slot == S::OffHand,
        3 => matches!(slot, S::MainHand | S::OffHand),
        4 => slot == S::Feet,
        5 => slot == S::Legs,
        6 => slot == S::Chest,
        7 => slot == S::Head,
        8 => slot.is_armor(),
        9 => slot == S::Body,
        10 => slot == S::Saddle,
        _ => false,
    }
}

fn conditionals<T>(v: Option<&Value>, parse: impl Fn(&Value) -> Option<T>) -> Vec<Conditional<T>> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(Conditional {
                effect: parse(e.get("effect")?)?,
                requirements: e.get("requirements").map(Requirement::parse),
            })
        })
        .collect()
}

impl Enchantment {
    /// Parses an enchantment definition.
    pub fn parse(id: &str, v: &Value) -> Enchantment {
        let slots = match v.get("slots") {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        }
        .into_iter()
        .filter_map(|s| SLOT_GROUPS.iter().position(|g| *g == s))
        .filter_map(|i| u8::try_from(i).ok())
        .collect();
        let empty = Value::Null;
        let effects = v.get("effects").unwrap_or(&empty);
        let get = |k: &str| effects.get(k);
        let values = |k: &str| conditionals(get(k), ValueEffect::parse);
        let post_attack = get("minecraft:post_attack")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|e| {
                Some(PostAttack {
                    enchanted: Party::parse(e.get("enchanted"))?,
                    affected: Party::parse(e.get("affected"))?,
                    effect: EntityEffect::parse(e.get("effect")?),
                    requirements: e.get("requirements").map(Requirement::parse),
                })
            })
            .collect();
        let attributes = get("minecraft:attributes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let name = e.get("attribute")?.as_str()?;
                let attribute = vanillite_data::entity::ATTRIBUTES
                    .iter()
                    .find(|a| a.name == name)?
                    .name;
                let op = e.get("operation")?.as_str()?;
                let operation = ["add_value", "add_multiplied_base", "add_multiplied_total"]
                    .iter()
                    .position(|x| *x == op)?;
                Some(AttributeEffect {
                    attribute,
                    amount: LevelBased::parse(e.get("amount")?)?,
                    operation: u8::try_from(operation).ok()?,
                })
            })
            .collect();
        Enchantment {
            id: id.to_owned(),
            slots,
            damage: values("minecraft:damage"),
            knockback: values("minecraft:knockback"),
            damage_protection: values("minecraft:damage_protection"),
            damage_immunity: conditionals(get("minecraft:damage_immunity"), |_| Some(())),
            armor_effectiveness: values("minecraft:armor_effectiveness"),
            smash_damage_per_fallen_block: values("minecraft:smash_damage_per_fallen_block"),
            post_attack,
            projectile_spawned: conditionals(get("minecraft:projectile_spawned"), |e| {
                Some(EntityEffect::parse(e))
            }),
            attributes,
            crossbow_charge_time: get("minecraft:crossbow_charge_time")
                .and_then(ValueEffect::parse),
            crossbow_charging_sounds: get("minecraft:crossbow_charging_sounds")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(ChargingSounds::parse).collect())
                .unwrap_or_default(),
            projectile_count: values("minecraft:projectile_count"),
            projectile_spread: values("minecraft:projectile_spread"),
            projectile_piercing: values("minecraft:projectile_piercing"),
            ammo_use: values("minecraft:ammo_use"),
            item_damage: values("minecraft:item_damage"),
            trident_spin_attack_strength: get("minecraft:trident_spin_attack_strength")
                .and_then(ValueEffect::parse),
            trident_sound: get("minecraft:trident_sound")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            trident_return_acceleration: values("minecraft:trident_return_acceleration"),
            fishing_time_reduction: values("minecraft:fishing_time_reduction"),
            fishing_luck_bonus: values("minecraft:fishing_luck_bonus"),
            hit_block: conditionals(get("minecraft:hit_block"), |e| Some(EntityEffect::parse(e))),
            post_piercing_attack: conditionals(get("minecraft:post_piercing_attack"), |e| {
                Some(EntityEffect::parse(e))
            }),
            block_experience: values("minecraft:block_experience"),
            mob_experience: values("minecraft:mob_experience"),
            prevent_armor_change: get("minecraft:prevent_armor_change").is_some(),
            prevent_equipment_drop: get("minecraft:prevent_equipment_drop").is_some(),
            tick: conditionals(get("minecraft:tick"), |e| Some(EntityEffect::parse(e))),
            location_changed: conditionals(get("minecraft:location_changed"), |e| {
                Some(EntityEffect::parse(e))
            }),
        }
    }

    /// Whether the enchantment works in an equipment slot (`matchingSlot`).
    pub fn works_in(&self, slot: EquipmentSlot) -> bool {
        self.slots.iter().any(|&g| slot_group_matches(g, slot))
    }
}

/// The entities and damage of one enchantment evaluation (`Enchantment.damageContext`).
pub struct EnchantContext<'a> {
    pub types: &'a DamageTypes,
    pub tags: &'a dyn EntityTags,
    /// The victim (`this`).
    pub this: Option<EntityFacts>,
    pub attacker: Option<EntityFacts>,
    pub direct_attacker: Option<EntityFacts>,
    pub attacking_player: Option<EntityFacts>,
    pub source: Option<SourceFacts>,
    /// `weather_check`: the level rains / thunders.
    pub raining: bool,
    pub thundering: bool,
    /// `location_check`: the block at the origin and whether it sees the sky.
    pub location: Option<(vanillite_data::block::BlockState, bool)>,
    /// `enchantment_active` of the location based effects.
    pub enchantment_active: Option<bool>,
    /// `block_state`: the hit block of `hit_block`.
    pub block_state: Option<vanillite_data::block::BlockState>,
}

impl std::fmt::Debug for EnchantContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnchantContext")
            .field("this", &self.this)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// All enchantments of the data packs by registry id.
#[derive(Clone, Debug, Default)]
pub struct Enchantments {
    pub list: Vec<Enchantment>,
}

impl Enchantments {
    /// The enchantments of `stack` with their definitions.
    pub fn of<'a>(
        &'a self,
        stack: &ItemStack,
    ) -> impl Iterator<Item = (&'a Enchantment, i32)> + use<'a> {
        crate::item::enchantments(stack)
            .into_iter()
            .filter(|&(_, level)| level > 0)
            .filter_map(|(id, level)| Some((self.list.get(usize::try_from(id).ok()?)?, level)))
    }

    fn run(
        &self,
        stack: &ItemStack,
        pick: impl Fn(&Enchantment) -> &[Conditional<ValueEffect>],
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
        value: f32,
    ) -> f32 {
        let mut value = value;
        for (e, level) in self.of(stack) {
            for c in pick(e) {
                if c.requirements
                    .as_ref()
                    .is_none_or(|r| r.test(cx, level, random))
                {
                    value = c.effect.process(level, random, value);
                }
            }
        }
        value
    }

    /// The value effects of `stack` in an item context (`itemContext(level, enchantmentLevel,
    /// tool)`): requirements see only `tool`.
    fn run_item(
        &self,
        stack: &ItemStack,
        pick: impl Fn(&Enchantment) -> &[Conditional<ValueEffect>],
        tool: Option<&ItemStack>,
        random: &mut dyn FloatRandom,
        value: f32,
    ) -> f32 {
        let mut value = value;
        for (e, level) in self.of(stack) {
            for c in pick(e) {
                if c.requirements
                    .as_ref()
                    .is_none_or(|r| r.test_item(tool, level, random))
                {
                    value = c.effect.process(level, random, value);
                }
            }
        }
        value
    }

    /// `EnchantmentHelper.modifyCrossbowChargingTime`: at least 0.
    pub fn crossbow_charge_time(
        &self,
        crossbow: &ItemStack,
        random: &mut dyn FloatRandom,
        time: f32,
    ) -> f32 {
        let mut time = time;
        for (e, level) in self.of(crossbow) {
            if let Some(effect) = &e.crossbow_charge_time {
                time = effect.process(level, random, time);
            }
        }
        time.max(0.0)
    }

    /// `EnchantmentHelper.getTridentSpinAttackStrength`: 0 without Riptide.
    pub fn trident_spin_attack_strength(
        &self,
        stack: &ItemStack,
        random: &mut dyn FloatRandom,
    ) -> f32 {
        let mut v = 0.0;
        for (e, level) in self.of(stack) {
            if let Some(effect) = &e.trident_spin_attack_strength {
                v = effect.process(level, random, v);
            }
        }
        v
    }

    /// `pickHighestLevel(TRIDENT_SOUND)`: the sound of the highest level, if any.
    pub fn trident_sound(&self, stack: &ItemStack) -> Option<&str> {
        let mut found: Option<(&Enchantment, i32)> = None;
        for (e, level) in self.of(stack) {
            if !e.trident_sound.is_empty() && found.is_none_or(|(_, l)| level > l) {
                found = Some((e, level));
            }
        }
        let (e, level) = found?;
        let i = usize::try_from(level)
            .unwrap_or(1)
            .min(e.trident_sound.len())
            .saturating_sub(1);
        e.trident_sound.get(i).map(String::as_str)
    }

    /// `EnchantmentHelper.getTridentReturnToOwnerAcceleration` (Loyalty), the requirements in
    /// an entity context without one (`None` holds only requirement-free effects).
    pub fn trident_return_acceleration(
        &self,
        stack: &ItemStack,
        random: &mut dyn FloatRandom,
    ) -> i32 {
        #[allow(clippy::cast_possible_truncation)]
        let v = self.run_item(stack, |e| &e.trident_return_acceleration, None, random, 0.0) as i32;
        v
    }

    /// `EnchantmentHelper.getFishingTimeReduction` (Lure), in seconds.
    pub fn fishing_time_reduction(&self, rod: &ItemStack, random: &mut dyn FloatRandom) -> f32 {
        self.run_item(rod, |e| &e.fishing_time_reduction, None, random, 0.0)
            .max(0.0)
    }

    /// `EnchantmentHelper.getFishingLuckBonus` (Luck of the Sea).
    pub fn fishing_luck_bonus(&self, rod: &ItemStack, random: &mut dyn FloatRandom) -> i32 {
        #[allow(clippy::cast_possible_truncation)]
        let v = self.run_item(rod, |e| &e.fishing_luck_bonus, None, random, 0.0) as i32;
        v.max(0)
    }

    /// `EnchantmentHelper.onHitBlock`: the `hit_block` effects whose requirements hold, with
    /// their level.
    pub fn hit_block_effects(
        &self,
        weapon: &ItemStack,
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
    ) -> Vec<(EntityEffect, i32)> {
        let mut out = Vec::new();
        for (e, level) in self.of(weapon) {
            for c in &e.hit_block {
                if c.requirements
                    .as_ref()
                    .is_none_or(|r| r.test(cx, level, random))
                {
                    out.push((c.effect.clone(), level));
                }
            }
        }
        out
    }

    /// `EnchantmentHelper.doPostPiercingAttackEffects`: the `post_piercing_attack` effects of
    /// the main hand `weapon` whose requirements hold (the user is `this`), with their level.
    pub fn post_piercing_attack_effects(
        &self,
        weapon: &ItemStack,
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
    ) -> Vec<(EntityEffect, i32)> {
        let mut out = Vec::new();
        for (e, level) in self.of(weapon) {
            if !e.works_in(EquipmentSlot::MainHand) {
                continue;
            }
            for c in &e.post_piercing_attack {
                if c.requirements
                    .as_ref()
                    .is_none_or(|r| r.test(cx, level, random))
                {
                    out.push((c.effect.clone(), level));
                }
            }
        }
        out
    }

    /// `EnchantmentHelper.pickHighestLevel(CROSSBOW_CHARGING_SOUNDS)`: the entry for the
    /// level of the highest enchantment that has the effect.
    pub fn crossbow_charging_sounds(&self, crossbow: &ItemStack) -> Option<&ChargingSounds> {
        let mut found: Option<(&Enchantment, i32)> = None;
        for (e, level) in self.of(crossbow) {
            if !e.crossbow_charging_sounds.is_empty() && found.is_none_or(|(_, l)| level > l) {
                found = Some((e, level));
            }
        }
        let (e, level) = found?;
        let i = usize::try_from(level)
            .unwrap_or(1)
            .min(e.crossbow_charging_sounds.len())
            .saturating_sub(1);
        e.crossbow_charging_sounds.get(i)
    }

    /// `EnchantmentHelper.processProjectileCount`: at least 0 (the entity requirements of the
    /// shooter are not evaluated; Vanilla's data has none).
    pub fn projectile_count(
        &self,
        weapon: &ItemStack,
        random: &mut dyn FloatRandom,
        count: i32,
    ) -> i32 {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let v = self.run_item(weapon, |e| &e.projectile_count, None, random, count as f32) as i32;
        v.max(0)
    }

    /// `EnchantmentHelper.processBlockExperience` (`MutableFloat.intValue`).
    pub fn block_experience(
        &self,
        tool: &ItemStack,
        random: &mut dyn FloatRandom,
        amount: i32,
    ) -> i32 {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let v = self.run_item(tool, |e| &e.block_experience, None, random, amount as f32) as i32;
        v
    }

    /// Whether any enchantment has a `mob_experience` effect (skips the killer's equipment
    /// walk when none does).
    pub fn has_mob_experience(&self) -> bool {
        self.list.iter().any(|e| !e.mob_experience.is_empty())
    }

    /// `EnchantmentHelper.processMobExperience`: the killer's equipment, in the slots its
    /// enchantments work in, changes the experience of a killed mob (`cx.this` is the killed
    /// mob, `random` its random; `MutableFloat.intValue`).
    pub fn mob_experience(
        &self,
        equipment: &[(EquipmentSlot, &ItemStack)],
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
        amount: i32,
    ) -> i32 {
        #[allow(clippy::cast_precision_loss)]
        let mut value = amount as f32;
        for &(slot, stack) in equipment {
            for (e, level) in self.of(stack) {
                if !e.works_in(slot) {
                    continue;
                }
                for c in &e.mob_experience {
                    if c.requirements
                        .as_ref()
                        .is_none_or(|r| r.test(cx, level, random))
                    {
                        value = c.effect.process(level, random, value);
                    }
                }
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        let v = value as i32;
        v
    }

    /// `EnchantmentHelper.processProjectileSpread`: at least 0.
    pub fn projectile_spread(
        &self,
        weapon: &ItemStack,
        random: &mut dyn FloatRandom,
        angle: f32,
    ) -> f32 {
        self.run_item(weapon, |e| &e.projectile_spread, None, random, angle)
            .max(0.0)
    }

    /// `EnchantmentHelper.getPiercingCount(weapon, ammo)`: at least 0.
    pub fn piercing_count(
        &self,
        weapon: &ItemStack,
        ammo: &ItemStack,
        random: &mut dyn FloatRandom,
    ) -> i32 {
        #[allow(clippy::cast_possible_truncation)]
        let v = self.run_item(weapon, |e| &e.projectile_piercing, Some(ammo), random, 0.0) as i32;
        v.max(0)
    }

    /// `EnchantmentHelper.processDurabilityChange(stack, amount)` (`MutableFloat.intValue`):
    /// the `item_damage` effects in the item's context (the stack is the tool).
    pub fn durability_change(
        &self,
        stack: &ItemStack,
        random: &mut dyn FloatRandom,
        amount: i32,
    ) -> i32 {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let v = self.run_item(
            stack,
            |e| &e.item_damage,
            Some(stack),
            random,
            amount as f32,
        ) as i32;
        v
    }

    /// `EnchantmentHelper.processAmmoUse(weapon, ammo, amount)`.
    pub fn ammo_use(
        &self,
        weapon: &ItemStack,
        ammo: &ItemStack,
        random: &mut dyn FloatRandom,
        amount: i32,
    ) -> i32 {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let v = self.run_item(weapon, |e| &e.ammo_use, Some(ammo), random, amount as f32) as i32;
        v
    }

    /// `EnchantmentHelper.modifyDamage`.
    pub fn modify_damage(
        &self,
        weapon: &ItemStack,
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
        damage: f32,
    ) -> f32 {
        self.run(weapon, |e| &e.damage, cx, random, damage)
    }

    /// `EnchantmentHelper.modifyKnockback`.
    pub fn modify_knockback(
        &self,
        weapon: &ItemStack,
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
        knockback: f32,
    ) -> f32 {
        self.run(weapon, |e| &e.knockback, cx, random, knockback)
    }

    /// `EnchantmentHelper.modifyArmorEffectiveness`.
    pub fn modify_armor_effectiveness(
        &self,
        weapon: &ItemStack,
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
        value: f32,
    ) -> f32 {
        self.run(weapon, |e| &e.armor_effectiveness, cx, random, value)
    }

    /// `EnchantmentHelper.modifyFallBasedDamage` (mace smash attacks).
    pub fn modify_fall_based_damage(
        &self,
        weapon: &ItemStack,
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
        value: f32,
    ) -> f32 {
        self.run(
            weapon,
            |e| &e.smash_damage_per_fallen_block,
            cx,
            random,
            value,
        )
    }

    /// `EnchantmentHelper.getDamageProtection`: the protection of the equipment in the slots
    /// its enchantments work in.
    pub fn damage_protection(
        &self,
        equipment: &[(EquipmentSlot, &ItemStack)],
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
    ) -> f32 {
        let mut value = 0.0;
        for &(slot, stack) in equipment {
            for (e, level) in self.of(stack) {
                if !e.works_in(slot) {
                    continue;
                }
                for c in &e.damage_protection {
                    if c.requirements
                        .as_ref()
                        .is_none_or(|r| r.test(cx, level, random))
                    {
                        value = c.effect.process(level, random, value);
                    }
                }
            }
        }
        value
    }

    /// `EnchantmentHelper.isImmuneToDamage`: an enchantment of the equipment, in a slot it
    /// works in, has a `damage_immunity` effect whose requirements hold.
    pub fn immune_to_damage(
        &self,
        equipment: &[(EquipmentSlot, &ItemStack)],
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
    ) -> bool {
        for &(slot, stack) in equipment {
            for (e, level) in self.of(stack) {
                if !e.works_in(slot) {
                    continue;
                }
                for c in &e.damage_immunity {
                    if c.requirements
                        .as_ref()
                        .is_none_or(|r| r.test(cx, level, random))
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// `Enchantment.doPostAttack` of every enchantment of `stack` for `party`: the post
    /// attack effects whose requirements hold, with their enchantment level and who they
    /// affect, in order.
    pub fn post_attack_effects(
        &self,
        stack: &ItemStack,
        slot: Option<EquipmentSlot>,
        party: Party,
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
    ) -> Vec<(EntityEffect, i32, Party)> {
        let mut out = Vec::new();
        for (e, level) in self.of(stack) {
            if slot.is_some_and(|s| !e.works_in(s)) {
                continue;
            }
            for p in &e.post_attack {
                if p.enchanted == party
                    && p.requirements
                        .as_ref()
                        .is_none_or(|r| r.test(cx, level, random))
                {
                    out.push((p.effect.clone(), level, p.affected));
                }
            }
        }
        out
    }

    /// `Enchantment.onProjectileSpawned` of every enchantment of `weapon`: the
    /// `projectile_spawned` effects whose requirements hold (the projectile is `this`), with
    /// their level.
    pub fn projectile_spawned_effects(
        &self,
        weapon: &ItemStack,
        cx: &EnchantContext<'_>,
        random: &mut dyn FloatRandom,
    ) -> Vec<(EntityEffect, i32)> {
        let mut out = Vec::new();
        for (e, level) in self.of(weapon) {
            for c in &e.projectile_spawned {
                if c.requirements
                    .as_ref()
                    .is_none_or(|r| r.test(cx, level, random))
                {
                    out.push((c.effect.clone(), level));
                }
            }
        }
        out
    }

    /// Whether any enchantment of `stack` has `tick` or `location_changed` effects.
    pub fn has_location_effects(&self, stack: &ItemStack) -> bool {
        !stack.is_empty()
            && self
                .of(stack)
                .any(|(e, _)| !e.tick.is_empty() || !e.location_changed.is_empty())
    }

    /// The seconds `onProjectileSpawned` sets a shot projectile on fire for (Flame).
    pub fn projectile_ignite(&self, weapon: &ItemStack) -> Option<f32> {
        self.of(weapon)
            .flat_map(|(e, level)| {
                e.projectile_spawned
                    .iter()
                    .filter(|c| c.requirements.is_none())
                    .filter_map(move |c| c.effect.ignite_seconds(level))
            })
            .reduce(f32::max)
    }

    /// The `minecraft:attributes` modifiers of a stack's enchantments for `attribute` in
    /// `slot`: `(operation, amount)`.
    pub fn attribute_modifiers(
        &self,
        stack: &ItemStack,
        slot: EquipmentSlot,
        attribute: &str,
        out: &mut Vec<(u8, f64)>,
    ) {
        for (e, level) in self.of(stack) {
            if !e.works_in(slot) {
                continue;
            }
            for a in &e.attributes {
                if a.attribute == attribute {
                    out.push((a.operation, f64::from(a.amount.calculate(level))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoTags;
    impl EntityTags for NoTags {
        fn entity_in_tag(&self, _: vanillite_data::entity::EntityTypeId, t: &str) -> bool {
            t == "minecraft:sensitive_to_smite"
        }
    }

    #[test]
    fn level_based_values() {
        let v: Value = serde_json::json!({"type": "minecraft:fraction",
            "numerator": {"type": "minecraft:linear", "base": 1.0, "per_level_above_first": 1.0},
            "denominator": {"type": "minecraft:linear", "base": 2.0, "per_level_above_first": 1.0}});
        let lb = LevelBased::parse(&v).expect("parses");
        assert!((lb.calculate(3) - 0.75).abs() < 1e-6);
        let lin = LevelBased::Linear {
            base: 1.0,
            per_level: 0.5,
        };
        assert!((lin.calculate(5) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn smite_only_hits_undead() {
        let json = serde_json::json!({"slots": ["mainhand"], "effects": {"minecraft:damage": [{
            "effect": {"type": "minecraft:add", "value": {"type": "minecraft:linear", "base": 2.5, "per_level_above_first": 2.5}},
            "requirements": {"type": "minecraft:entity_properties", "entity": "this",
                "predicate": {"minecraft:entity_type": "#minecraft:sensitive_to_smite"}}}]}});
        let e = Enchantment::parse("minecraft:smite", &json);
        let types = DamageTypes::built_in();
        let cx = EnchantContext {
            types: &types,
            tags: &NoTags,
            this: Some(EntityFacts {
                kind: Some(vanillite_data::entity::ids::ZOMBIE),
                ..EntityFacts::default()
            }),
            attacker: None,
            direct_attacker: None,
            attacking_player: None,
            source: None,
            raining: false,
            thundering: false,
            location: None,
            enchantment_active: None,
            block_state: None,
        };
        let mut r = vanillite_base::random::LegacyRandom::new(1);
        let c = &e.damage[0];
        assert!(
            c.requirements
                .as_ref()
                .is_some_and(|q| q.test(&cx, 2, &mut r))
        );
        assert!((c.effect.process(2, &mut r, 7.0) - 12.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod value_tests {
    use super::*;

    /// `ScaleExponentially.process`: 10 · 2^3 = 80 (I4-11 of #272).
    #[test]
    fn world_effects_parse() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../vanillite-test/datapacks/enchant_effects/data/vanillite_test/enchantment/"
        );
        for name in ["blast", "stomp", "pulse", "shatter"] {
            let text = std::fs::read_to_string(format!("{dir}{name}.json")).unwrap_or_default();
            let json: Value = serde_json::from_str(&text).unwrap_or_default();
            let e = Enchantment::parse(name, &json);
            let effects: Vec<&EntityEffect> = e
                .post_attack
                .iter()
                .map(|p| &p.effect)
                .chain(e.tick.iter().map(|c| &c.effect))
                .chain(e.location_changed.iter().map(|c| &c.effect))
                .chain(e.hit_block.iter().map(|c| &c.effect))
                .collect();
            assert!(!effects.is_empty(), "{name}");
            fn unsupported(e: &EntityEffect) -> bool {
                match e {
                    EntityEffect::AllOf(l) => l.iter().any(unsupported),
                    EntityEffect::Unsupported => true,
                    _ => false,
                }
            }
            assert!(
                !effects.iter().any(|e| unsupported(e)),
                "{name}: {effects:?}"
            );
        }
    }

    #[test]
    fn exponential_value_effect() {
        let e = serde_json::json!({"type": "minecraft:exponential", "base": 2.0, "exponent": 3.0});
        let parsed = ValueEffect::parse(&e).expect("exponential");
        let mut r = vanillite_base::random::LegacyRandom::new(0);
        assert!((parsed.process(1, &mut r, 10.0) - 80.0).abs() < 1e-6);
    }
}
