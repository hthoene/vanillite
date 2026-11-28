//! Entity and damage source predicates (Vanilla `EntityPredicate`, `DamageSourcePredicate`):
//! parsed once from data pack JSON and tested against a small view of the world. Loot
//! conditions and enchantment requirements share them; unknown parts make a predicate fail
//! (they are reported by the parser).

use serde_json::Value;
use vanillite_data::entity::EntityTypeId;

use crate::survival::damage::{DamageKind, DamageTypes, tag};

/// A set of entity types: a tag (resolved against the loaded tags) or a list of types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeSet {
    Tag(Box<str>),
    Types(Box<[u16]>),
}

impl TypeSet {
    /// Parses `"#ns:tag"`, `"ns:type"` or a list of types.
    pub fn parse(v: &Value) -> Option<TypeSet> {
        match v {
            Value::String(s) => Some(match s.strip_prefix('#') {
                Some(t) => TypeSet::Tag(qualify(t).into()),
                None => TypeSet::Types(Box::new([type_id(s)?])),
            }),
            Value::Array(a) => Some(TypeSet::Types(
                a.iter()
                    .map(|x| x.as_str().and_then(type_id))
                    .collect::<Option<Vec<_>>>()?
                    .into(),
            )),
            _ => None,
        }
    }

    /// Whether `kind` is in the set.
    pub fn contains(&self, kind: EntityTypeId, tags: &dyn EntityTags) -> bool {
        match self {
            TypeSet::Tag(t) => tags.entity_in_tag(kind, t),
            TypeSet::Types(ids) => ids.contains(&kind.0),
        }
    }
}

/// Entity type tags as loaded from the data packs.
pub trait EntityTags {
    /// Whether an entity type is in a tag (`ns:path` without `#`).
    fn entity_in_tag(&self, kind: EntityTypeId, tag: &str) -> bool;
}

fn qualify(s: &str) -> String {
    if s.contains(':') {
        s.to_owned()
    } else {
        format!("minecraft:{s}")
    }
}

fn type_id(name: &str) -> Option<u16> {
    vanillite_data::registry::ENTITY_TYPE.id(&qualify(name))
}

/// The entity facts predicates test.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EntityFacts {
    pub kind: Option<EntityTypeId>,
    pub on_fire: bool,
    pub baby: bool,
    pub sneaking: bool,
    pub sprinting: bool,
    pub swimming: bool,
    pub on_ground: bool,
    /// The main hand item has an enchantment of `#minecraft:smelts_loot` (Fire Aspect).
    pub smelts_loot: bool,
    /// A sheep's colour (`DyeColor` id) and whether it is sheared.
    pub sheep_color: Option<u8>,
    pub sheared: bool,
    /// A villager's or zombie villager's type (`minecraft:villager_type` id).
    pub villager_variant: Option<u16>,
    /// The climate variant of a chicken, cow or pig: 0 cold, 1 temperate, 2 warm.
    pub farm_variant: Option<u8>,
    /// `is_flying`: gliding or flying with the creative abilities.
    pub flying: bool,
    /// `movement.fall_distance`.
    pub fall_distance: f64,
    /// `location.can_see_sky` at the entity's position (known where the level is).
    pub can_see_sky: bool,
    /// A fishing hook and whether it fishes in open water.
    pub fishing_hook: Option<bool>,
    /// The type of the entity this one rides.
    pub vehicle: Option<EntityTypeId>,
    /// The size of a slime, magma cube or sulfur cube.
    pub cube_size: Option<i32>,
    /// A raider: `hasRaid` (in a raid or near an active one) and `isCaptain` (a patrol or wave
    /// leader wearing the ominous banner).
    pub raider: Option<(bool, bool)>,
    /// A mooshroom's colour: 0 red, 1 brown.
    pub mooshroom_variant: Option<u8>,
    /// `is_fall_flying`: gliding with elytra.
    pub fall_flying: bool,
    /// `is_in_water`.
    pub in_water: bool,
    /// A player's game mode (wire id) and food level.
    pub player: Option<(i32, i32)>,
    /// `tickCount` (`periodic_tick`).
    pub tick_count: i32,
    /// `getKnownMovement()` in blocks per tick (`movement`).
    pub movement: vanillite_base::Vec3,
    /// The block below that affects the movement (`movement_affected_by`), where the level is
    /// known.
    pub movement_block: Option<vanillite_data::block::BlockState>,
}

/// `EntityPredicate`: the parts the game evaluates (`type`, `flags`); other parts are
/// unsupported and make it fail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntityPredicate {
    pub types: Option<TypeSet>,
    /// `flags`: `is_on_fire`, `is_baby`, `is_sneaking`, `is_sprinting`, `is_swimming`,
    /// `is_on_ground` (`None` = any).
    pub on_fire: Option<bool>,
    pub baby: Option<bool>,
    pub sneaking: Option<bool>,
    pub sprinting: Option<bool>,
    pub swimming: Option<bool>,
    pub on_ground: Option<bool>,
    /// `equipment.mainhand` with an enchantment of `#minecraft:smelts_loot`.
    pub smelts_loot: bool,
    /// `components` `minecraft:sheep/color`.
    pub sheep_color: Option<u8>,
    /// `type_specific/sheep` `sheared`.
    pub sheared: Option<bool>,
    /// `components` `minecraft:chicken/variant`, `minecraft:cow/variant` or
    /// `minecraft:pig/variant`: 0 cold, 1 temperate, 2 warm.
    pub farm_variant: Option<u8>,
    /// `predicates` `minecraft:villager/variant`: the villager types (`villager_type` ids).
    pub villager_variants: Option<Vec<u16>>,
    /// `flags.is_flying`.
    pub flying: Option<bool>,
    /// `movement`: the bounds of x, y, z, speed, horizontal speed, vertical speed (per
    /// second) and fall distance.
    pub movement: Option<Box<MovementBounds>>,
    /// `periodic_tick`.
    pub periodic_tick: Option<i32>,
    /// `movement_affected_by` with a `block.blocks` holder set entry (`"ns:id"`, `"#tag"`).
    pub movement_affected_by: Option<Vec<String>>,
    /// `location.can_see_sky`.
    pub can_see_sky: Option<bool>,
    /// `type_specific/fishing_hook`: a hook, with `in_open_water` if given.
    pub fishing_hook: Option<Option<bool>>,
    /// `vehicle`: the types of the ridden entity (other parts are not evaluated).
    pub vehicle: Option<TypeSet>,
    /// `type_specific/cube_mob` `size`: the inclusive bounds.
    pub cube_size: Option<(Option<i32>, Option<i32>)>,
    /// `type_specific/raider`: `has_raid` and `is_captain` (both default false).
    pub raider: Option<(bool, bool)>,
    /// `components` `minecraft:mooshroom/variant`: 0 red, 1 brown.
    pub mooshroom_variant: Option<u8>,
    /// `flags.is_fall_flying`.
    pub fall_flying: Option<bool>,
    /// `flags.is_in_water`.
    pub in_water: Option<bool>,
    /// `vehicle: {}`: rides anything.
    pub in_vehicle: bool,
    /// `type_specific/player`: a player, in one of the game modes (wire ids) if given, with
    /// at least / at most this food level if given.
    pub player: Option<PlayerPredicate>,
    /// Has parts that are not evaluated.
    pub unsupported: bool,
}

impl EntityPredicate {
    /// Parses an entity predicate object. Both `type` and `minecraft:entity_type` (the
    /// component form of 26.3 enchantment requirements) name the types.
    pub fn parse(v: &Value) -> EntityPredicate {
        let mut p = EntityPredicate::default();
        let Some(o) = v.as_object() else {
            p.unsupported = !v.is_null();
            return p;
        };
        for (k, v) in o {
            match k.trim_start_matches("minecraft:") {
                "type" | "entity_type" => {
                    p.types = TypeSet::parse(v);
                    p.unsupported |= p.types.is_none();
                }
                "flags" => {
                    let flag = |name: &str| v.get(name).and_then(Value::as_bool);
                    p.on_fire = flag("is_on_fire");
                    p.baby = flag("is_baby");
                    p.sneaking = flag("is_sneaking");
                    p.sprinting = flag("is_sprinting");
                    p.swimming = flag("is_swimming");
                    p.on_ground = flag("is_on_ground");
                    p.flying = flag("is_flying");
                    p.fall_flying = flag("is_fall_flying");
                    p.in_water = flag("is_in_water");
                    let known = [
                        "is_on_fire",
                        "is_baby",
                        "is_sneaking",
                        "is_sprinting",
                        "is_swimming",
                        "is_on_ground",
                        "is_flying",
                        "is_fall_flying",
                        "is_in_water",
                    ];
                    p.unsupported |= v
                        .as_object()
                        .is_some_and(|f| f.keys().any(|k| !known.contains(&k.as_str())));
                }
                "components" => {
                    const FARM: [&str; 3] = [
                        "minecraft:chicken/variant",
                        "minecraft:cow/variant",
                        "minecraft:pig/variant",
                    ];
                    const MOOSHROOM: &str = "minecraft:mooshroom/variant";
                    let known = v.as_object().is_some_and(|c| {
                        c.keys().all(|k| {
                            k == "minecraft:sheep/color"
                                || k == MOOSHROOM
                                || FARM.contains(&k.as_str())
                        })
                    });
                    let mooshroom = v.get(MOOSHROOM);
                    p.mooshroom_variant = mooshroom.and_then(Value::as_str).and_then(|name| {
                        match name.strip_prefix("minecraft:").unwrap_or(name) {
                            "red" => Some(0),
                            "brown" => Some(1),
                            _ => None,
                        }
                    });
                    p.unsupported |= mooshroom.is_some() && p.mooshroom_variant.is_none();
                    p.sheep_color = v
                        .get("minecraft:sheep/color")
                        .and_then(Value::as_str)
                        .and_then(dye_id);
                    let farm = FARM.iter().find_map(|k| v.get(*k));
                    p.farm_variant = farm.and_then(Value::as_str).and_then(|name| {
                        match name.strip_prefix("minecraft:").unwrap_or(name) {
                            "cold" => Some(0),
                            "temperate" => Some(1),
                            "warm" => Some(2),
                            _ => None,
                        }
                    });
                    p.unsupported |= !known
                        || (v.get("minecraft:sheep/color").is_some() && p.sheep_color.is_none())
                        || (farm.is_some() && p.farm_variant.is_none());
                }
                "vehicle" if v.as_object().is_some_and(serde_json::Map::is_empty) => {
                    p.in_vehicle = true;
                }
                "type_specific/player" => {
                    p.player = PlayerPredicate::parse(v);
                    p.unsupported |= p.player.is_none();
                }
                "vehicle" => {
                    // Only the type of the vehicle (the jockey drops).
                    p.vehicle = v
                        .as_object()
                        .filter(|o| o.len() == 1)
                        .and_then(|o| o.iter().next())
                        .filter(|(k, _)| {
                            matches!(k.trim_start_matches("minecraft:"), "type" | "entity_type")
                        })
                        .and_then(|(_, t)| TypeSet::parse(t));
                    p.unsupported |= p.vehicle.is_none();
                }
                "type_specific/cube_mob" => {
                    let only = v.as_object().is_some_and(|o| o.len() == 1);
                    p.cube_size = v.get("size").and_then(int_bounds);
                    p.unsupported |= !only || p.cube_size.is_none();
                }
                "type_specific/raider" => {
                    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
                    let known = v.as_object().is_some_and(|o| {
                        o.iter().all(|(k, f)| {
                            matches!(k.as_str(), "has_raid" | "is_captain") && f.is_boolean()
                        })
                    });
                    p.raider = Some((flag("has_raid"), flag("is_captain")));
                    p.unsupported |= !known;
                }
                "movement" => {
                    p.movement = movement_bounds(v).map(Box::new);
                    p.unsupported |= p.movement.is_none();
                }
                "periodic_tick" => {
                    p.periodic_tick = v
                        .as_i64()
                        .and_then(|n| i32::try_from(n).ok())
                        .filter(|&n| n > 0);
                    p.unsupported |= p.periodic_tick.is_none();
                }
                "movement_affected_by" => {
                    // Only a block set (Soul Speed).
                    let block = v.get("block").filter(|_| {
                        v.as_object()
                            .is_some_and(|l| l.keys().all(|k| k == "block"))
                    });
                    let blocks = block
                        .filter(|b| {
                            b.as_object()
                                .is_some_and(|b| b.keys().all(|k| k == "blocks"))
                        })
                        .and_then(|b| b.get("blocks"));
                    p.movement_affected_by = match blocks {
                        Some(Value::String(s)) => Some(vec![s.clone()]),
                        Some(Value::Array(a)) => a
                            .iter()
                            .map(|e| e.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>(),
                        _ => None,
                    };
                    p.unsupported |= p.movement_affected_by.is_none();
                }
                "location" => {
                    // Only `can_see_sky` (Channeling).
                    let only = v
                        .as_object()
                        .is_some_and(|l| l.keys().all(|k| k == "can_see_sky"));
                    p.can_see_sky = v.get("can_see_sky").and_then(Value::as_bool);
                    p.unsupported |= !only || p.can_see_sky.is_none();
                }
                "type_specific/fishing_hook" => {
                    p.fishing_hook = Some(v.get("in_open_water").and_then(Value::as_bool));
                }
                "type_specific/sheep" => {
                    p.sheared = v.get("sheared").and_then(Value::as_bool);
                }
                "predicates" => {
                    // Only the villager type of the villager trades.
                    let only = v
                        .as_object()
                        .is_some_and(|c| c.keys().all(|k| k == "minecraft:villager/variant"));
                    let variants = v.get("minecraft:villager/variant").and_then(villager_types);
                    p.unsupported |= !only || variants.is_none();
                    p.villager_variants = variants;
                }
                "equipment" => {
                    // Only the smelting check of the mob loot tables.
                    let tag = v
                        .pointer("/mainhand/predicates/minecraft:enchantments/0/enchantments")
                        .and_then(Value::as_str);
                    let only = v.as_object().is_some_and(|e| e.len() == 1);
                    if only && tag == Some("#minecraft:smelts_loot") {
                        p.smelts_loot = true;
                    } else {
                        p.unsupported = true;
                    }
                }
                _ => p.unsupported = true,
            }
        }
        p
    }

    /// Whether `entity` matches; a missing entity never does.
    pub fn test(&self, entity: Option<&EntityFacts>, tags: &dyn EntityTags) -> bool {
        let Some(e) = entity else {
            return false;
        };
        if self.unsupported {
            return false;
        }
        if let Some(t) = &self.types
            && !e.kind.is_some_and(|k| t.contains(k, tags))
        {
            return false;
        }
        if self.smelts_loot && !e.smelts_loot {
            return false;
        }
        if self.sheep_color.is_some() && self.sheep_color != e.sheep_color {
            return false;
        }
        if self.farm_variant.is_some() && self.farm_variant != e.farm_variant {
            return false;
        }
        if let Some(types) = &self.villager_variants
            && !e.villager_variant.is_some_and(|v| types.contains(&v))
        {
            return false;
        }
        if self
            .sheared
            .is_some_and(|s| e.sheep_color.is_none() || s != e.sheared)
        {
            return false;
        }
        let flag = |want: Option<bool>, have: bool| want.is_none_or(|w| w == have);
        flag(self.on_fire, e.on_fire)
            && flag(self.baby, e.baby)
            && flag(self.sneaking, e.sneaking)
            && flag(self.sprinting, e.sprinting)
            && flag(self.swimming, e.swimming)
            && flag(self.on_ground, e.on_ground)
            && flag(self.flying, e.flying)
            && flag(self.fall_flying, e.fall_flying)
            && flag(self.in_water, e.in_water)
            && (!self.in_vehicle || e.vehicle.is_some())
            && self
                .player
                .as_ref()
                .is_none_or(|p| e.player.is_some_and(|(mode, food)| p.test(mode, food)))
            && flag(self.can_see_sky, e.can_see_sky)
            && self
                .vehicle
                .as_ref()
                .is_none_or(|t| e.vehicle.is_some_and(|k| t.contains(k, tags)))
            && self.cube_size.is_none_or(|(min, max)| {
                e.cube_size
                    .is_some_and(|s| min.is_none_or(|m| m <= s) && max.is_none_or(|m| s <= m))
            })
            && self.raider.is_none_or(|r| e.raider == Some(r))
            && self
                .mooshroom_variant
                .is_none_or(|v| e.mooshroom_variant == Some(v))
            && self
                .movement
                .as_ref()
                .is_none_or(|b| movement_matches(b, e))
            && self
                .periodic_tick
                .is_none_or(|n| e.tick_count.checked_rem(n) == Some(0))
            && self.movement_affected_by.as_ref().is_none_or(|blocks| {
                e.movement_block.is_some_and(|state| {
                    crate::player::play::game_data().is_some_and(|d| {
                        blocks.iter().any(|entry| {
                            d.holder_set("minecraft:block", entry)
                                .contains(&u32::from(state.block().0))
                        })
                    })
                })
            })
            && self.fishing_hook.is_none_or(|want| {
                e.fishing_hook
                    .is_some_and(|open| want.is_none_or(|w| w == open))
            })
    }
}

/// `PlayerPredicate`: the parts the game evaluates (`gamemode`, `food.level`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerPredicate {
    /// The game modes (wire ids); `None` = any.
    pub game_modes: Option<Vec<i32>>,
    /// `food.level`: the inclusive bounds.
    pub food_level: Option<(Option<i32>, Option<i32>)>,
}

impl PlayerPredicate {
    /// Parses a `type_specific/player` object; `None` when it has parts that are not
    /// evaluated.
    fn parse(v: &Value) -> Option<PlayerPredicate> {
        let o = v.as_object()?;
        let mut p = PlayerPredicate::default();
        for (k, v) in o {
            match k.as_str() {
                "gamemode" => {
                    let names: Vec<&str> = match v {
                        Value::String(s) => vec![s.as_str()],
                        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                        _ => return None,
                    };
                    let modes = names
                        .into_iter()
                        .map(|n| {
                            ["survival", "creative", "adventure", "spectator"]
                                .iter()
                                .position(|m| *m == n)
                                .and_then(|i| i32::try_from(i).ok())
                        })
                        .collect::<Option<Vec<i32>>>()?;
                    p.game_modes = Some(modes);
                }
                "food" => {
                    let food = v.as_object()?;
                    if food.keys().any(|k| k != "level") {
                        return None;
                    }
                    p.food_level = Some(int_bounds(food.get("level")?)?);
                }
                _ => return None,
            }
        }
        Some(p)
    }

    fn test(&self, mode: i32, food: i32) -> bool {
        self.game_modes.as_ref().is_none_or(|m| m.contains(&mode))
            && self.food_level.is_none_or(|(min, max)| {
                min.is_none_or(|m| m <= food) && max.is_none_or(|m| food <= m)
            })
    }
}

/// `MinMaxBounds.Ints`: an exact number or `{min, max}`.
/// The bounds of a `MovementPredicate`: `x`, `y`, `z`, `speed`, `horizontal_speed`,
/// `vertical_speed` and `fall_distance` (`MinMaxBounds.Doubles`).
pub type MovementBounds = [(Option<f64>, Option<f64>); 7];

/// Parses a `MovementPredicate`.
fn movement_bounds(v: &Value) -> Option<MovementBounds> {
    const KEYS: [&str; 7] = [
        "x",
        "y",
        "z",
        "speed",
        "horizontal_speed",
        "vertical_speed",
        "fall_distance",
    ];
    let o = v.as_object()?;
    if !o.keys().all(|k| KEYS.contains(&k.as_str())) {
        return None;
    }
    let mut out = [(None, None); 7];
    for (slot, key) in out.iter_mut().zip(KEYS) {
        *slot = match o.get(key) {
            None => (None, None),
            Some(Value::Object(b)) => {
                if !b.keys().all(|k| k == "min" || k == "max") {
                    return None;
                }
                let bound = |k: &str| match b.get(k) {
                    None => Some(None),
                    Some(n) => n.as_f64().map(Some),
                };
                (bound("min")?, bound("max")?)
            }
            Some(n) => {
                let n = n.as_f64()?;
                (Some(n), Some(n))
            }
        };
    }
    Some(out)
}

/// `MovementPredicate.matches` with `getKnownMovement().scale(20)`; the speeds compare their
/// squares (`matchesSqr`).
fn movement_matches(b: &MovementBounds, e: &EntityFacts) -> bool {
    let plain = |(min, max): (Option<f64>, Option<f64>), v: f64| {
        min.is_none_or(|m| m <= v) && max.is_none_or(|m| v <= m)
    };
    let sqr = |(min, max): (Option<f64>, Option<f64>), v: f64| {
        min.is_none_or(|m| m * m <= v) && max.is_none_or(|m| v <= m * m)
    };
    let (x, y, z) = (
        e.movement.x * 20.0,
        e.movement.y * 20.0,
        e.movement.z * 20.0,
    );
    plain(b[0], x)
        && plain(b[1], y)
        && plain(b[2], z)
        && sqr(b[3], x * x + y * y + z * z)
        && sqr(b[4], x * x + z * z)
        && plain(b[5], y.abs())
        && plain(b[6], e.fall_distance)
}

fn int_bounds(v: &Value) -> Option<(Option<i32>, Option<i32>)> {
    let int = |v: &Value| v.as_i64().and_then(|n| i32::try_from(n).ok());
    match v {
        Value::Object(o) if o.keys().all(|k| k == "min" || k == "max") => {
            let bound = |k: &str| match o.get(k) {
                None => Some(None),
                Some(v) => int(v).map(Some),
            };
            Some((bound("min")?, bound("max")?))
        }
        other => int(other).map(|n| (Some(n), Some(n))),
    }
}

/// A `villager_type` holder set of ids (a single id or a list; tags are not supported).
fn villager_types(v: &Value) -> Option<Vec<u16>> {
    let one = |s: &str| vanillite_data::registry::VILLAGER_TYPE.id(&qualify(s));
    match v {
        Value::String(s) if !s.starts_with('#') => Some(vec![one(s)?]),
        Value::Array(list) => list.iter().map(|e| e.as_str().and_then(one)).collect(),
        _ => None,
    }
}

/// The damage source facts predicates test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceFacts {
    pub kind: DamageKind,
    /// `isDirect`: the direct entity is the causing entity.
    pub is_direct: bool,
    pub direct: Option<EntityFacts>,
    pub cause: Option<EntityFacts>,
}

/// `DamageSourcePredicate`: `tags`, `is_direct`, `direct_entity`, `source_entity`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DamageSourcePredicate {
    /// Tag bits with their expected value.
    pub tags: Vec<(u32, bool)>,
    pub is_direct: Option<bool>,
    pub direct_entity: Option<EntityPredicate>,
    pub source_entity: Option<EntityPredicate>,
    pub unsupported: bool,
}

impl DamageSourcePredicate {
    /// Parses a damage source predicate object.
    pub fn parse(v: &Value) -> DamageSourcePredicate {
        let mut p = DamageSourcePredicate::default();
        let Some(o) = v.as_object() else {
            p.unsupported = true;
            return p;
        };
        for (k, v) in o {
            match k.as_str() {
                "tags" => {
                    for t in v.as_array().into_iter().flatten() {
                        let id = t
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim_start_matches('#');
                        let id = qualify(id);
                        let expected = t.get("expected").and_then(Value::as_bool).unwrap_or(true);
                        match tag::NAMES.iter().find(|(n, _)| *n == id) {
                            Some(&(_, bit)) => p.tags.push((bit, expected)),
                            None => p.unsupported = true,
                        }
                    }
                }
                "is_direct" => p.is_direct = v.as_bool(),
                "direct_entity" => p.direct_entity = Some(EntityPredicate::parse(v)),
                "source_entity" => p.source_entity = Some(EntityPredicate::parse(v)),
                _ => p.unsupported = true,
            }
        }
        p
    }

    /// Whether `source` matches.
    pub fn test(&self, source: &SourceFacts, types: &DamageTypes, tags: &dyn EntityTags) -> bool {
        if self.unsupported {
            return false;
        }
        self.tags
            .iter()
            .all(|&(bit, expected)| types.is(source.kind, bit) == expected)
            && self.is_direct.is_none_or(|d| d == source.is_direct)
            && self
                .direct_entity
                .as_ref()
                .is_none_or(|p| p.test(source.direct.as_ref(), tags))
            && self
                .source_entity
                .as_ref()
                .is_none_or(|p| p.test(source.cause.as_ref(), tags))
    }
}

/// The id of a dye colour name (`DyeColor`).
pub fn dye_id(name: &str) -> Option<u8> {
    const NAMES: [&str; 16] = [
        "white",
        "orange",
        "magenta",
        "light_blue",
        "yellow",
        "lime",
        "pink",
        "gray",
        "light_gray",
        "cyan",
        "purple",
        "blue",
        "brown",
        "green",
        "red",
        "black",
    ];
    NAMES
        .iter()
        .position(|n| *n == name)
        .and_then(|i| u8::try_from(i).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoTags;
    impl EntityTags for NoTags {
        fn entity_in_tag(&self, _: EntityTypeId, _: &str) -> bool {
            false
        }
    }

    #[test]
    fn sheep_loot_predicates() {
        let p = EntityPredicate::parse(&serde_json::json!({
            "minecraft:components": {"minecraft:sheep/color": "lime"},
            "minecraft:type_specific/sheep": {"sheared": false}
        }));
        assert!(!p.unsupported);
        let mut sheep = EntityFacts {
            sheep_color: Some(5),
            ..EntityFacts::default()
        };
        assert!(p.test(Some(&sheep), &NoTags));
        sheep.sheared = true;
        assert!(!p.test(Some(&sheep), &NoTags));
        sheep.sheared = false;
        sheep.sheep_color = Some(0);
        assert!(!p.test(Some(&sheep), &NoTags));
    }

    #[test]
    fn vanilla_mob_loot_predicates() {
        let parse = |v: serde_json::Value| {
            let p = EntityPredicate::parse(&v);
            assert!(!p.unsupported, "{v}");
            p
        };
        let jockey = parse(serde_json::json!({"minecraft:flags": {"is_baby": true},
            "minecraft:vehicle": {"minecraft:entity_type": "minecraft:chicken"}}));
        let mut e = EntityFacts {
            baby: true,
            ..EntityFacts::default()
        };
        assert!(!jockey.test(Some(&e), &NoTags));
        e.vehicle = Some(vanillite_data::entity::ids::CHICKEN);
        assert!(jockey.test(Some(&e), &NoTags));

        let big = parse(serde_json::json!(
            {"minecraft:type_specific/cube_mob": {"size": {"min": 2}}}));
        let tiny = parse(serde_json::json!({"minecraft:type_specific/cube_mob": {"size": 1}}));
        let mut e = EntityFacts::default();
        assert!(!big.test(Some(&e), &NoTags) && !tiny.test(Some(&e), &NoTags));
        e.cube_size = Some(1);
        assert!(!big.test(Some(&e), &NoTags) && tiny.test(Some(&e), &NoTags));
        e.cube_size = Some(4);
        assert!(big.test(Some(&e), &NoTags) && !tiny.test(Some(&e), &NoTags));

        let captain = parse(serde_json::json!(
            {"minecraft:type_specific/raider": {"is_captain": true}}));
        let mut e = EntityFacts {
            raider: Some((false, false)),
            ..EntityFacts::default()
        };
        assert!(!captain.test(Some(&e), &NoTags));
        e.raider = Some((false, true));
        assert!(captain.test(Some(&e), &NoTags));
        // `RaiderPredicate.hasRaid` defaults to false: a wave captain drops nothing.
        e.raider = Some((true, true));
        assert!(!captain.test(Some(&e), &NoTags));

        let brown = parse(serde_json::json!(
            {"minecraft:components": {"minecraft:mooshroom/variant": "brown"}}));
        let mut e = EntityFacts {
            mooshroom_variant: Some(0),
            ..EntityFacts::default()
        };
        assert!(!brown.test(Some(&e), &NoTags));
        e.mooshroom_variant = Some(1);
        assert!(brown.test(Some(&e), &NoTags));
    }

    #[test]
    fn fire_aspect_smelts() {
        let p = EntityPredicate::parse(&serde_json::json!({"minecraft:equipment": {"mainhand": {
            "predicates": {"minecraft:enchantments": [{"enchantments": "#minecraft:smelts_loot"}]}}}}));
        assert!(!p.unsupported);
        let mut killer = EntityFacts::default();
        assert!(!p.test(Some(&killer), &NoTags));
        killer.smelts_loot = true;
        assert!(p.test(Some(&killer), &NoTags));
    }
}
