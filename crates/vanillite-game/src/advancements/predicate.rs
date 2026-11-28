//! The predicates of advancement criteria (`advancements.predicates`) and the loot conditions
//! they are wrapped in (`LootItemCondition`). Parts the server cannot evaluate yet (named
//! predicates, some entity and item sub-predicates, ...) parse into
//! [`Unsupported`](Condition) parts that never match, so such criteria are never granted by
//! triggers.

use std::borrow::Cow;

use serde_json::{Map, Value};
use vanillite_base::{BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_data::block::{BlockId, BlockState};
use vanillite_data::registry;

use crate::item::ItemStack;
use crate::stats::{Stat, StatType, StatsCounter};

/// A parse result; the error names what is wrong.
pub type Res<T> = Result<T, String>;

pub(crate) fn object(v: &Value) -> Res<&Map<String, Value>> {
    v.as_object().ok_or_else(|| format!("not an object: {v}"))
}

pub(crate) fn string(v: &Value) -> Res<&str> {
    v.as_str().ok_or_else(|| format!("not a string: {v}"))
}

/// `namespace:path` with the default namespace.
pub(crate) fn qualify(id: &str) -> String {
    if id.contains(':') {
        id.to_owned()
    } else {
        format!("minecraft:{id}")
    }
}

/// What the predicates read of the world.
pub trait World {
    /// The dimension key.
    fn dimension(&self) -> &str;
    /// The block at `pos`; `None` where the chunk is not loaded.
    fn block(&self, pos: BlockPos) -> Option<BlockState>;
    /// The biome at `pos` (`Level.getBiome`).
    fn biome(&self, pos: BlockPos) -> Option<String>;
    /// Whether `id` of `registry` is in `#tag`.
    fn in_tag(&self, registry: &str, tag: &str, id: &str) -> bool;
    /// `StructureManager.getStructureWithPieceAt(pos, structures).isValid()`: a start of one of
    /// `structures` has a piece at the loaded `pos`.
    fn structure_at(&self, _pos: BlockPos, _structures: &IdSet) -> bool {
        false
    }
    /// `CampfireBlock.isSmokeyPos`.
    fn smokey(&self, _pos: BlockPos) -> bool {
        false
    }
    /// `getMaxLocalRawBrightness(pos)`.
    fn light(&self, _pos: BlockPos) -> i32 {
        0
    }
    /// `canSeeSky(pos)`.
    fn can_see_sky(&self, _pos: BlockPos) -> bool {
        false
    }
    /// `BlockEntity.saveWithFullMetadata` of the block entity at a loaded `pos`.
    fn block_entity_nbt(&self, _pos: BlockPos) -> Option<vanillite_nbt::Compound> {
        None
    }
    /// `BlockEntity.collectComponents` of the block entity at a loaded `pos` (wire values by
    /// component type); `None` without a block entity.
    fn block_components(&self, _pos: BlockPos) -> Option<Vec<(u16, Box<[u8]>)>> {
        None
    }
    /// The progress of the player whose triggers run (`getAdvancements`), for the
    /// `advancements` of its player predicate.
    fn progress(&self) -> Option<&super::PlayerAdvancements> {
        None
    }
    /// The entity the player whose triggers run looks at (`PlayerPredicate.lookingAt`: the
    /// entity hit 100 blocks along its view, if it has a line of sight to it).
    fn looking_at(&self) -> Option<&EntityView<'static>> {
        None
    }
}

/// An active effect as `MobEffectsPredicate` sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectView {
    pub effect: crate::effect::MobEffect,
    pub amplifier: i32,
    pub duration: i32,
    pub ambient: bool,
    pub visible: bool,
}

/// The player part of an entity view (`PlayerPredicate`).
#[derive(Clone, Copy, Debug)]
pub struct PlayerFacts<'a> {
    pub game_mode: GameMode,
    pub level: i32,
    /// The statistics; `None` in snapshots of other players (stat conditions fail).
    pub stats: Option<&'a StatsCounter>,
    /// `getFoodData()`: the food level and the saturation.
    pub food: (i32, f32),
    /// The known recipes (`getRecipeBook`); `None` in snapshots of other players (recipe
    /// conditions fail).
    pub recipes: Option<&'a crate::recipe::book::PlayerRecipeBook>,
    /// `getLastClientInput()`.
    pub input: vanillite_protocol::packets::play::serverbound::Input,
    /// The player whose triggers run: [`World::progress`] and [`World::looking_at`] are its
    /// own (other players fail `advancements` and `looking_at` conditions).
    pub own: bool,
}

/// An entity as the entity predicates see it. With `'static` it is a snapshot that events
/// carry (`EntityPredicate.createContext` at the time of the event).
#[derive(Clone, Debug)]
pub struct EntityView<'a> {
    /// The type id, e.g. `minecraft:player`.
    pub kind: &'static str,
    pub pos: Vec3,
    pub on_ground: bool,
    pub on_fire: bool,
    pub crouching: bool,
    pub sprinting: bool,
    pub swimming: bool,
    pub flying: bool,
    pub fall_flying: bool,
    pub in_water: bool,
    pub baby: bool,
    /// A `LivingEntity` (players, mobs, armor stands, mannequins).
    pub living: bool,
    /// Head, chest, legs, feet, body, main hand, off hand.
    pub equipment: [Cow<'a, ItemStack>; 7],
    /// The team name.
    pub team: Option<String>,
    /// Game mode, level and statistics of a player.
    pub player: Option<PlayerFacts<'a>>,
    /// The active effects of a living entity.
    pub effects: Vec<EffectView>,
    /// The vehicle (with its passengers).
    pub vehicle: Option<Box<EntityView<'a>>>,
    /// The passengers (without their own vehicle or passengers).
    pub passengers: Vec<EntityView<'a>>,
    /// Exact entity components the predicates can test, e.g. `minecraft:cat/variant`.
    pub components: Vec<(&'static str, String)>,
    /// `LightningBolt.getBlocksSetOnFire` of a lightning bolt.
    pub lightning_fires: Option<i32>,
    /// `getKnownMovement().scale(20)`: blocks per second.
    pub movement: Vec3,
    /// `fallDistance`.
    pub fall_distance: f64,
    /// `tickCount`.
    pub tick_count: i32,
    /// `isSheared` of a sheep.
    pub sheared: Option<bool>,
    /// `getSize` of a slime, magma cube or sulfur cube.
    pub cube_size: Option<i32>,
    /// `hasRaid` and `isCaptain` of a raider.
    pub raider: Option<(bool, bool)>,
    /// `isOpenWaterFishing` of a fishing hook.
    pub open_water: Option<bool>,
    /// `Mob.getTarget` (without its own vehicle, passengers or target).
    pub target: Option<Box<EntityView<'a>>>,
    /// The player's inventory slots 0-35 (`inventory.*`, `hotbar.*`), where known.
    pub inventory: Option<Cow<'a, [ItemStack]>>,
    /// `NbtPredicate.getEntityTagToCompare`: kept only while a loaded predicate reads entity
    /// data ([`entity_nbt_wanted`]).
    pub nbt: Option<Box<vanillite_nbt::Compound>>,
    /// `entityTags()`.
    pub tags: Vec<String>,
    /// `LightningBolt.getHitEntities` of a lightning bolt: the alive entities a flash struck.
    pub struck: Vec<EntityView<'a>>,
}

/// Whether a parsed entity predicate reads entity data (`nbt`, data components): only then
/// views keep [`EntityView::nbt`]. It only ever turns on.
static ENTITY_NBT_WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether entity views must carry [`EntityView::nbt`].
pub fn entity_nbt_wanted() -> bool {
    ENTITY_NBT_WANTED.load(std::sync::atomic::Ordering::Relaxed)
}

fn want_entity_nbt() {
    ENTITY_NBT_WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

impl EntityView<'_> {
    /// `Entity.get(type)` in the persistent form, for the components the views know:
    /// `custom_data` (`CustomData.EMPTY` without data), `custom_name` and the variants.
    fn component(&self, name: &str) -> Option<vanillite_nbt::Tag> {
        use vanillite_nbt::{Compound, Tag};
        match name {
            "minecraft:custom_data" => Some(
                self.nbt
                    .as_ref()
                    .and_then(|n| n.get("data").cloned())
                    .unwrap_or_else(|| Tag::Compound(Compound::new())),
            ),
            "minecraft:custom_name" => self.nbt.as_ref().and_then(|n| n.get("CustomName").cloned()),
            _ => self
                .components
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| Tag::String(v.clone())),
        }
    }
}

/// A component value in the form a stack's wire value converts to (`None`: it does not
/// convert); types without a codec keep their value.
fn normalized_component(name: &str, tag: &vanillite_nbt::Tag) -> Option<vanillite_nbt::Tag> {
    use crate::item_nbt::{component_from_nbt, component_to_nbt};
    match vanillite_protocol::components_nbt::type_by_name(name) {
        Some(t) if !name.contains('/') => {
            component_from_nbt(t, tag).and_then(|w| component_to_nbt(t, &w))
        }
        _ => Some(tag.clone()),
    }
}

impl<'a> EntityView<'a> {
    /// A view of `kind` at `pos` with nothing else set.
    pub fn new(kind: &'static str, pos: Vec3) -> EntityView<'a> {
        EntityView {
            kind,
            pos,
            on_ground: false,
            on_fire: false,
            crouching: false,
            sprinting: false,
            swimming: false,
            flying: false,
            fall_flying: false,
            in_water: false,
            baby: false,
            living: false,
            equipment: std::array::from_fn(|_| Cow::Owned(ItemStack::EMPTY)),
            team: None,
            player: None,
            effects: Vec::new(),
            vehicle: None,
            passengers: Vec::new(),
            components: Vec::new(),
            lightning_fires: None,
            movement: Vec3::ZERO,
            fall_distance: 0.0,
            tick_count: 0,
            sheared: None,
            cube_size: None,
            raider: None,
            open_water: None,
            target: None,
            inventory: None,
            nbt: None,
            tags: Vec::new(),
            struck: Vec::new(),
        }
    }
}

/// `MinMaxBounds`: a number or `{min, max}`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bounds {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Bounds {
    /// Parses `n` or `{"min": a, "max": b}`.
    pub fn parse(v: &Value) -> Res<Bounds> {
        if let Some(n) = v.as_f64() {
            return Ok(Bounds {
                min: Some(n),
                max: Some(n),
            });
        }
        let o = object(v)?;
        let num = |k: &str| {
            o.get(k)
                .map(|n| n.as_f64().ok_or_else(|| format!("{k} is not a number")))
                .transpose()
        };
        Ok(Bounds {
            min: num("min")?,
            max: num("max")?,
        })
    }

    fn field(o: &Map<String, Value>, key: &str) -> Res<Bounds> {
        o.get(key)
            .map(Bounds::parse)
            .transpose()
            .map(Option::unwrap_or_default)
    }

    /// `matches`.
    pub fn matches(self, v: f64) -> bool {
        self.min.is_none_or(|m| m <= v) && self.max.is_none_or(|m| v <= m)
    }

    /// `matchesSqr`: against the squared bounds.
    pub fn matches_sqr(self, v: f64) -> bool {
        self.min.is_none_or(|m| m * m <= v) && self.max.is_none_or(|m| v <= m * m)
    }
}

/// A `HolderSet`: ids or a `#tag`.
#[derive(Clone, Debug, PartialEq)]
pub enum IdSet {
    Ids(Vec<String>),
    Tag(String),
}

impl IdSet {
    /// Parses `"id"`, `["a", "b"]` or `"#tag"`.
    pub fn parse(v: &Value) -> Res<IdSet> {
        if let Some(s) = v.as_str() {
            return Ok(match s.strip_prefix('#') {
                Some(tag) => IdSet::Tag(qualify(tag)),
                None => IdSet::Ids(vec![qualify(s)]),
            });
        }
        let list = v.as_array().ok_or_else(|| format!("not an id list: {v}"))?;
        list.iter()
            .map(|e| string(e).map(qualify))
            .collect::<Res<Vec<_>>>()
            .map(IdSet::Ids)
    }

    /// Whether `id` of `registry` is in the set.
    pub fn contains(&self, registry: &str, id: &str, world: &dyn World) -> bool {
        match self {
            IdSet::Ids(ids) => ids.iter().any(|i| i == id),
            IdSet::Tag(tag) => world.in_tag(registry, tag, id),
        }
    }
}

/// `EnchantmentPredicate` of the `minecraft:enchantments` and `minecraft:stored_enchantments`
/// item sub-predicates.
#[derive(Clone, Debug, PartialEq)]
struct EnchantmentMatcher {
    enchantments: Option<IdSet>,
    levels: Bounds,
}

impl EnchantmentMatcher {
    /// `containedIn(itemEnchantments)`.
    fn contained_in(&self, present: &[(u32, i32)], world: &dyn World) -> bool {
        match &self.enchantments {
            Some(set) => {
                let data = crate::player::play::game_data();
                let name = |id: u32| {
                    data.as_ref().and_then(|d| {
                        d.enchantments
                            .iter()
                            .find(|(_, v)| **v == id)
                            .map(|(n, _)| n.clone())
                    })
                };
                // `matchesEnchantment`: a level of 0 does not count.
                present.iter().any(|&(id, level)| {
                    level != 0
                        && self.levels.matches(f64::from(level))
                        && name(id)
                            .is_some_and(|n| set.contains("minecraft:enchantment", &n, world))
                })
            }
            None if self.levels != Bounds::default() => present
                .iter()
                .any(|&(_, level)| self.levels.matches(f64::from(level))),
            None => !present.is_empty(),
        }
    }
}

/// One item sub-predicate (`DataComponentPredicate`) of `predicates`.
#[derive(Clone, Debug, PartialEq)]
enum SubPredicate {
    /// `damage`: durability and damage bounds.
    Damage { durability: Bounds, damage: Bounds },
    /// `enchantments`, or `stored_enchantments`: every entry finds an enchantment.
    Enchantments {
        stored: bool,
        list: Vec<EnchantmentMatcher>,
    },
    /// `potion_contents`: the potions (`effects` is not evaluated).
    Potions(Option<IdSet>),
    /// `custom_data`: `NbtUtils.compareNbt` against the custom data.
    CustomData(vanillite_nbt::Tag),
    /// `jukebox_playable`: the songs.
    JukeboxPlayable(Option<IdSet>),
    /// `trim`: the materials and the patterns.
    Trim {
        material: Option<IdSet>,
        pattern: Option<IdSet>,
    },
    /// `villager/variant`: the villager types.
    VillagerVariant(IdSet),
    /// `AnyValue`: a component type without a predicate type is present.
    Present(u16),
    /// `potion_contents` with `effects`: the potions and the collection of all effects.
    PotionEffects(Option<IdSet>, CollectionPredicate<MobEffectsPredicate>),
    /// `firework_explosion`.
    FireworkExplosion(ExplosionTest),
    /// `fireworks`: the explosions and the flight duration.
    Fireworks(Option<CollectionPredicate<ExplosionTest>>, Bounds),
    /// `writable_book_content`: the raw page texts.
    WritableBook(Option<CollectionPredicate<String>>),
    /// `written_book_content`: pages (raw text components), author, title, generation,
    /// resolved.
    WrittenBook {
        pages: Option<CollectionPredicate<vanillite_nbt::Tag>>,
        author: Option<String>,
        title: Option<String>,
        generation: Bounds,
        resolved: Option<bool>,
    },
    /// `attribute_modifiers`: the modifier entries.
    AttributeModifiers(Option<CollectionPredicate<ModifierTest>>),
    /// `container` (`nonEmptyItems`) or `bundle_contents` (`items`).
    Items {
        bundle: bool,
        items: Option<CollectionPredicate<ItemPredicate>>,
    },
    /// The predicate types that are not evaluated (containers, bundles, fireworks, books,
    /// attribute modifiers, potion effects): never matches.
    Unsupported,
}

/// `NbtPredicate.CODEC` (`TagParser.LENIENT_CODEC`): SNBT or the compound itself.
fn lenient_nbt(v: &Value) -> Res<vanillite_nbt::Tag> {
    Ok(match v {
        Value::String(s) => vanillite_nbt::snbt::parse_compound(s)
            .map(vanillite_nbt::Tag::Compound)
            .map_err(|e| format!("bad nbt {s}: {e:?}"))?,
        other => crate::item_nbt::json_to_nbt(other),
    })
}

/// The holder of a component in its persistent form: a registry id string (else `None`, e.g.
/// an inline value).
fn holder_name(tag: Option<&vanillite_nbt::Tag>) -> Option<&str> {
    tag.and_then(vanillite_nbt::Tag::as_str)
}

impl SubPredicate {
    fn parse(name: &str, v: &Value) -> Res<SubPredicate> {
        let o = || object(v);
        let set = |o: &Map<String, Value>, k: &str| o.get(k).map(IdSet::parse).transpose();
        Ok(match name {
            "minecraft:damage" => {
                let o = o()?;
                SubPredicate::Damage {
                    durability: Bounds::field(o, "durability")?,
                    damage: Bounds::field(o, "damage")?,
                }
            }
            "minecraft:enchantments" | "minecraft:stored_enchantments" => {
                let Value::Array(entries) = v else {
                    return Err(format!("{name} is not a list"));
                };
                let mut list = Vec::new();
                for e in entries {
                    let e = object(e)?;
                    list.push(EnchantmentMatcher {
                        enchantments: set(e, "enchantments")?,
                        levels: Bounds::field(e, "levels")?,
                    });
                }
                SubPredicate::Enchantments {
                    stored: name == "minecraft:stored_enchantments",
                    list,
                }
            }
            "minecraft:potion_contents" => {
                let o = o()?;
                match o.get("effects") {
                    Some(e) => SubPredicate::PotionEffects(
                        set(o, "potions")?,
                        CollectionPredicate::parse(e, MobEffectsPredicate::parse)?,
                    ),
                    None => SubPredicate::Potions(set(o, "potions")?),
                }
            }
            "minecraft:container" | "minecraft:bundle_contents" => SubPredicate::Items {
                bundle: name == "minecraft:bundle_contents",
                items: o()?
                    .get("items")
                    .map(|v| CollectionPredicate::parse(v, ItemPredicate::parse))
                    .transpose()?,
            },
            "minecraft:custom_data" => SubPredicate::CustomData(lenient_nbt(v)?),
            "minecraft:jukebox_playable" => SubPredicate::JukeboxPlayable(set(o()?, "song")?),
            "minecraft:trim" => {
                let o = o()?;
                SubPredicate::Trim {
                    material: set(o, "material")?,
                    pattern: set(o, "pattern")?,
                }
            }
            "minecraft:villager/variant" => SubPredicate::VillagerVariant(IdSet::parse(v)?),
            "minecraft:firework_explosion" => {
                SubPredicate::FireworkExplosion(ExplosionTest::parse(v)?)
            }
            "minecraft:fireworks" => {
                let o = o()?;
                SubPredicate::Fireworks(
                    o.get("explosions")
                        .map(|e| CollectionPredicate::parse(e, ExplosionTest::parse))
                        .transpose()?,
                    Bounds::field(o, "flight_duration")?,
                )
            }
            "minecraft:writable_book_content" => {
                SubPredicate::WritableBook(
                    o()?.get("pages")
                        .map(|p| {
                            CollectionPredicate::parse(p, |e| {
                                Ok(string(object(e)?.get("contents").ok_or("no contents")?)?
                                    .to_owned())
                            })
                        })
                        .transpose()?,
                )
            }
            "minecraft:written_book_content" => {
                let o = o()?;
                SubPredicate::WrittenBook {
                    pages: o
                        .get("pages")
                        .map(|p| {
                            CollectionPredicate::parse(p, |e| {
                                Ok(text_nbt(&crate::item_nbt::json_to_nbt(
                                    object(e)?.get("contents").ok_or("no contents")?,
                                )))
                            })
                        })
                        .transpose()?,
                    author: o.get("author").map(string).transpose()?.map(str::to_owned),
                    title: o.get("title").map(string).transpose()?.map(str::to_owned),
                    generation: Bounds::field(o, "generation")?,
                    resolved: o.get("resolved").and_then(Value::as_bool),
                }
            }
            "minecraft:attribute_modifiers" => SubPredicate::AttributeModifiers(
                o()?.get("modifiers")
                    .map(|m| CollectionPredicate::parse(m, ModifierTest::parse))
                    .transpose()?,
            ),
            other => match vanillite_protocol::components_nbt::type_by_name(other) {
                Some(t) => SubPredicate::Present(t),
                None => return Err(format!("unknown item sub-predicate {other}")),
            },
        })
    }

    /// `matches(entity)`: an entity has only the components [`EntityView::component`] knows;
    /// item-only predicates need a component it never has.
    fn matches_entity(&self, e: &EntityView<'_>, world: &dyn World) -> bool {
        match self {
            SubPredicate::CustomData(expected) => e
                .component("minecraft:custom_data")
                .is_some_and(|actual| crate::item_nbt::compare_nbt(expected, &actual)),
            SubPredicate::VillagerVariant(types) => {
                holder_name(e.component("minecraft:villager/variant").as_ref())
                    .is_some_and(|n| types.contains("minecraft:villager_type", n, world))
            }
            SubPredicate::Present(t) => vanillite_data::registry::DATA_COMPONENT_TYPE
                .name(*t)
                .is_some_and(|n| e.component(n).is_some()),
            _ => false,
        }
    }

    /// `matches(components)`: single component predicates need their component.
    fn matches(&self, stack: &ItemStack, world: &dyn World) -> bool {
        use crate::item_nbt::{component_to_nbt, effective_component};
        use vanillite_protocol::components::ids;
        let nbt = |t: u16| effective_component(stack, t).and_then(|v| component_to_nbt(t, &v));
        match self {
            SubPredicate::Damage { durability, damage } => {
                let Some(d) = effective_component(stack, ids::DAMAGE)
                    .and_then(|v| vanillite_protocol::codec::Reader::new(&v).varint().ok())
                else {
                    return false;
                };
                let max = crate::item::max_damage(stack);
                durability.matches(f64::from(max) - f64::from(d)) && damage.matches(f64::from(d))
            }
            SubPredicate::Enchantments { stored, list } => {
                let t = if *stored {
                    ids::STORED_ENCHANTMENTS
                } else {
                    ids::ENCHANTMENTS
                };
                if effective_component(stack, t).is_none() {
                    return false;
                }
                let present = if *stored {
                    crate::item::stored_enchantments(stack)
                } else {
                    crate::item::enchantments(stack)
                };
                list.iter().all(|m| m.contained_in(&present, world))
            }
            SubPredicate::Potions(potions) => {
                let Some(contents) = effective_component(stack, ids::POTION_CONTENTS)
                    .and_then(|v| crate::effect::potion::PotionContents::decode(&v))
                else {
                    return false;
                };
                potions.as_ref().is_none_or(|set| {
                    contents
                        .potion
                        .and_then(|p| registry::POTION.name(p))
                        .is_some_and(|n| set.contains("minecraft:potion", n, world))
                })
            }
            SubPredicate::CustomData(expected) => {
                // `CustomData.EMPTY` without the component.
                let actual = registry::DATA_COMPONENT_TYPE
                    .id("minecraft:custom_data")
                    .and_then(nbt)
                    .unwrap_or_else(
                        || vanillite_nbt::Tag::Compound(vanillite_nbt::Compound::new()),
                    );
                crate::item_nbt::compare_nbt(expected, &actual)
            }
            SubPredicate::JukeboxPlayable(songs) => {
                if !crate::block::jukebox::is_playable(stack) {
                    return false;
                }
                songs.as_ref().is_none_or(|set| {
                    let data = crate::player::play::game_data();
                    crate::block::jukebox::song_of(stack)
                        .and_then(|id| {
                            let d = data.as_ref()?;
                            Some(d.jukebox_songs.get(usize::try_from(id).ok()?)?.name.clone())
                        })
                        .is_some_and(|n| set.contains("minecraft:jukebox_song", &n, world))
                })
            }
            SubPredicate::Trim { material, pattern } => {
                let Some(vanillite_nbt::Tag::Compound(trim)) = nbt(ids::TRIM) else {
                    return false;
                };
                let holder_in = |set: &Option<IdSet>, key: &str, registry: &str| {
                    set.as_ref().is_none_or(|set| {
                        holder_name(trim.get(key)).is_some_and(|n| set.contains(registry, n, world))
                    })
                };
                holder_in(material, "material", "minecraft:trim_material")
                    && holder_in(pattern, "pattern", "minecraft:trim_pattern")
            }
            SubPredicate::VillagerVariant(types) => {
                let Some(t) = registry::DATA_COMPONENT_TYPE.id("minecraft:villager/variant") else {
                    return false;
                };
                holder_name(nbt(t).as_ref())
                    .is_some_and(|n| types.contains("minecraft:villager_type", n, world))
            }
            SubPredicate::Present(t) => effective_component(stack, *t).is_some(),
            SubPredicate::PotionEffects(potions, effects) => {
                let Some(contents) = effective_component(stack, ids::POTION_CONTENTS)
                    .and_then(|v| crate::effect::potion::PotionContents::decode(&v))
                else {
                    return false;
                };
                // `PotionsPredicate.matches`: a set of potions needs a potion in it.
                let potion_ok = potions.as_ref().is_none_or(|set| {
                    contents
                        .potion
                        .and_then(|p| registry::POTION.name(p))
                        .is_some_and(|n| set.contains("minecraft:potion", n, world))
                });
                let all: Vec<EffectView> = contents
                    .all_effects()
                    .iter()
                    .map(|e| EffectView {
                        effect: e.effect,
                        amplifier: i32::from(e.amplifier),
                        duration: e.duration,
                        ambient: e.ambient,
                        visible: e.visible,
                    })
                    .collect();
                potion_ok && effects.test(&all, |p, e| p.test(e))
            }
            SubPredicate::FireworkExplosion(test) => {
                matches!(nbt(ids::FIREWORK_EXPLOSION), Some(vanillite_nbt::Tag::Compound(c)) if test.test(&c))
            }
            SubPredicate::Fireworks(explosions, flight) => {
                let Some(vanillite_nbt::Tag::Compound(c)) = nbt(ids::FIREWORKS) else {
                    return false;
                };
                let list = compounds(c.get("explosions"));
                let duration = c
                    .get("flight_duration")
                    .and_then(vanillite_nbt::Tag::as_i64)
                    .and_then(|v| i32::try_from(v).ok())
                    .map_or(0.0, f64::from);
                explosions
                    .as_ref()
                    .is_none_or(|e| e.test(&list, |t, c| t.test(c)))
                    && flight.matches(duration)
            }
            SubPredicate::WritableBook(pages) => {
                let Some(vanillite_nbt::Tag::Compound(c)) = nbt(ids::WRITABLE_BOOK_CONTENT) else {
                    return false;
                };
                let raw: Vec<String> = pages_of(c.get("pages"))
                    .into_iter()
                    .filter_map(|t| t.as_str().map(str::to_owned))
                    .collect();
                pages
                    .as_ref()
                    .is_none_or(|p| p.test(&raw, |want, have| want == have))
            }
            SubPredicate::WrittenBook {
                pages,
                author,
                title,
                generation,
                resolved,
            } => {
                let Some(vanillite_nbt::Tag::Compound(c)) = nbt(ids::WRITTEN_BOOK_CONTENT) else {
                    return false;
                };
                let str_of = |k: &str| c.get(k).and_then(vanillite_nbt::Tag::as_str);
                let title_raw = pages_of(
                    c.get("title")
                        .map(|t| vanillite_nbt::Tag::List(vanillite_nbt::List(vec![t.clone()])))
                        .as_ref(),
                )
                .into_iter()
                .next();
                let raw: Vec<vanillite_nbt::Tag> =
                    pages_of(c.get("pages")).iter().map(text_nbt).collect();
                author
                    .as_deref()
                    .is_none_or(|a| str_of("author") == Some(a))
                    && title.as_deref().is_none_or(|t| {
                        title_raw.as_ref().and_then(vanillite_nbt::Tag::as_str) == Some(t)
                    })
                    && generation.matches(
                        c.get("generation")
                            .and_then(vanillite_nbt::Tag::as_i64)
                            .and_then(|v| i32::try_from(v).ok())
                            .map_or(0.0, f64::from),
                    )
                    && resolved.is_none_or(|r| {
                        (c.get("resolved")
                            .and_then(vanillite_nbt::Tag::as_i64)
                            .unwrap_or(0)
                            != 0)
                            == r
                    })
                    && pages
                        .as_ref()
                        .is_none_or(|p| p.test(&raw, |want, have| want == have))
            }
            SubPredicate::AttributeModifiers(modifiers) => {
                let Some(list) = nbt(ids::ATTRIBUTE_MODIFIERS) else {
                    return false;
                };
                let entries = compounds(Some(&list));
                modifiers
                    .as_ref()
                    .is_none_or(|m| m.test(&entries, |t, e| t.test(e, world)))
            }
            SubPredicate::Items { bundle, items } => {
                let list = if *bundle {
                    if effective_component(stack, ids::BUNDLE_CONTENTS).is_none() {
                        return false;
                    }
                    crate::bundle::items(stack).unwrap_or_default()
                } else {
                    let Some(v) = effective_component(stack, ids::CONTAINER) else {
                        return false;
                    };
                    vanillite_protocol::item::decode_container_contents(&v)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect()
                };
                items
                    .as_ref()
                    .is_none_or(|c| c.test(&list, |p, s| p.test(s, world)))
            }
            SubPredicate::Unsupported => false,
        }
    }
}

/// `ItemPredicate`.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemPredicate {
    /// Item ids, or a tag.
    items: Option<Result<Vec<u16>, String>>,
    count: Bounds,
    /// `DataComponentExactPredicate`: the component types and their values in the persistent
    /// form the stack's values convert to (`None`: a value that does not convert never
    /// matches).
    exact: Vec<(u16, Option<vanillite_nbt::Tag>)>,
    /// The sub-predicates (`predicates`).
    predicates: Vec<SubPredicate>,
}

impl ItemPredicate {
    /// Parses an item predicate.
    pub fn parse(v: &Value) -> Res<ItemPredicate> {
        use crate::item_nbt::{component_from_nbt, component_to_nbt, json_to_nbt};
        let o = object(v)?;
        let items = match o.get("items").map(IdSet::parse).transpose()? {
            None => None,
            Some(IdSet::Tag(tag)) => Some(Err(tag)),
            Some(IdSet::Ids(ids)) => Some(Ok(ids
                .iter()
                .map(|n| {
                    registry::ITEM
                        .id(n)
                        .ok_or_else(|| format!("unknown item {n}"))
                })
                .collect::<Res<Vec<_>>>()?)),
        };
        let mut exact = Vec::new();
        for (k, v) in o
            .get("components")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let name = qualify(k);
            let t = vanillite_protocol::components_nbt::type_by_name(&name)
                .ok_or_else(|| format!("unknown component {name}"))?;
            // Both sides in the form the stack's wire value converts to.
            let value =
                component_from_nbt(t, &json_to_nbt(v)).and_then(|w| component_to_nbt(t, &w));
            exact.push((t, value));
        }
        let mut predicates = Vec::new();
        for (k, v) in o
            .get("predicates")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            predicates.push(SubPredicate::parse(&qualify(k), v)?);
        }
        Ok(ItemPredicate {
            items,
            count: Bounds::field(o, "count")?,
            exact,
            predicates,
        })
    }

    /// Whether every part is evaluated.
    pub fn is_supported(&self) -> bool {
        self.exact.iter().all(|(_, v)| v.is_some())
            && self.predicates.iter().all(|p| match p {
                SubPredicate::Unsupported => false,
                SubPredicate::Items { items: Some(c), .. } => {
                    c.all().all(ItemPredicate::is_supported)
                }
                _ => true,
            })
    }

    /// `test`.
    pub fn test(&self, stack: &ItemStack, world: &dyn World) -> bool {
        let item = if stack.is_empty() { 0 } else { stack.item };
        let items_ok = match &self.items {
            None => true,
            Some(Ok(ids)) => ids.contains(&item),
            Some(Err(tag)) => registry::ITEM
                .name(item)
                .is_some_and(|n| world.in_tag("minecraft:item", tag, n)),
        };
        let count = if stack.is_empty() { 0 } else { stack.count };
        items_ok
            && self.count.matches(f64::from(count))
            && self.exact.iter().all(|(t, expected)| {
                expected.as_ref().is_some_and(|expected| {
                    crate::item_nbt::effective_component(stack, *t)
                        .and_then(|v| crate::item_nbt::component_to_nbt(*t, &v))
                        .is_some_and(|actual| actual == *expected)
                })
            })
            && self.predicates.iter().all(|p| p.matches(stack, world))
    }
}

/// `CollectionPredicate`: `contains` (every test matched by some element), `count` (the
/// number of matching elements per test) and `size`.
#[derive(Clone, Debug, PartialEq)]
pub struct CollectionPredicate<P> {
    contains: Vec<P>,
    counts: Vec<(P, Bounds)>,
    size: Bounds,
}

impl<P> CollectionPredicate<P> {
    fn parse(v: &Value, element: impl Fn(&Value) -> Res<P>) -> Res<CollectionPredicate<P>> {
        let o = object(v)?;
        let list = |k: &str| -> Res<&[Value]> {
            match o.get(k) {
                None => Ok(&[]),
                Some(Value::Array(a)) => Ok(a),
                Some(other) => Err(format!("{k} is not a list: {other}")),
            }
        };
        let contains = list("contains")?.iter().map(&element).collect::<Res<_>>()?;
        let counts = list("count")?
            .iter()
            .map(|e| {
                let e = object(e)?;
                Ok((
                    element(e.get("test").ok_or("count entry without test")?)?,
                    Bounds::parse(e.get("count").ok_or("count entry without count")?)?,
                ))
            })
            .collect::<Res<_>>()?;
        Ok(CollectionPredicate {
            contains,
            counts,
            size: Bounds::field(o, "size")?,
        })
    }

    /// `test(values)`.
    fn test<T>(&self, values: &[T], matches: impl Fn(&P, &T) -> bool) -> bool {
        // `CollectionContentsPredicate.Multiple`: a test drops out once an element matches.
        let mut left: Vec<&P> = self.contains.iter().collect();
        let contains_ok = left.is_empty()
            || values.iter().any(|v| {
                left.retain(|p| !matches(p, v));
                left.is_empty()
            });
        let count = |p: &P| values.iter().filter(|v| matches(p, v)).count();
        contains_ok
            && self.counts.iter().all(|(p, bounds)| {
                bounds.matches(f64::from(u32::try_from(count(p)).unwrap_or(u32::MAX)))
            })
            && self
                .size
                .matches(f64::from(u32::try_from(values.len()).unwrap_or(u32::MAX)))
    }

    fn all(&self) -> impl Iterator<Item = &P> {
        self.contains
            .iter()
            .chain(self.counts.iter().map(|(p, _)| p))
    }
}

/// The compounds of a list tag.
fn compounds(tag: Option<&vanillite_nbt::Tag>) -> Vec<vanillite_nbt::Compound> {
    match tag {
        Some(vanillite_nbt::Tag::List(l)) => {
            l.0.iter()
                .filter_map(|t| match t {
                    vanillite_nbt::Tag::Compound(c) => Some(c.clone()),
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The raw values of a list of `Filterable`s (a value, or `{raw, filtered}`).
fn pages_of(tag: Option<&vanillite_nbt::Tag>) -> Vec<vanillite_nbt::Tag> {
    match tag {
        Some(vanillite_nbt::Tag::List(l)) => l
            .0
            .iter()
            .filter_map(|t| match t {
                vanillite_nbt::Tag::Compound(c) if c.get("raw").is_some() => c.get("raw").cloned(),
                other => Some(other.clone()),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A text component in one comparable form: a plain string becomes `{text: ...}`.
fn text_nbt(tag: &vanillite_nbt::Tag) -> vanillite_nbt::Tag {
    match tag {
        vanillite_nbt::Tag::String(s) => {
            vanillite_nbt::Tag::Compound(vanillite_nbt::Compound::new().with("text", s.as_str()))
        }
        other => other.clone(),
    }
}

/// `FireworkExplosionPredicate.FireworkPredicate` on an explosion in its persistent form.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplosionTest {
    shape: Option<String>,
    twinkle: Option<bool>,
    trail: Option<bool>,
}

impl ExplosionTest {
    fn parse(v: &Value) -> Res<ExplosionTest> {
        let o = object(v)?;
        Ok(ExplosionTest {
            shape: o.get("shape").map(string).transpose()?.map(str::to_owned),
            twinkle: o.get("has_twinkle").and_then(Value::as_bool),
            trail: o.get("has_trail").and_then(Value::as_bool),
        })
    }

    fn test(&self, c: &vanillite_nbt::Compound) -> bool {
        let flag = |k: &str| c.get(k).and_then(vanillite_nbt::Tag::as_i64).unwrap_or(0) != 0;
        self.shape
            .as_deref()
            .is_none_or(|s| c.get("shape").and_then(vanillite_nbt::Tag::as_str) == Some(s))
            && self.twinkle.is_none_or(|t| flag("has_twinkle") == t)
            && self.trail.is_none_or(|t| flag("has_trail") == t)
    }
}

/// `AttributeModifiersPredicate.EntryPredicate` on a modifier entry in its persistent form.
#[derive(Clone, Debug, PartialEq)]
pub struct ModifierTest {
    attribute: Option<IdSet>,
    id: Option<String>,
    amount: Bounds,
    operation: Option<String>,
    slot: Option<String>,
}

impl ModifierTest {
    fn parse(v: &Value) -> Res<ModifierTest> {
        let o = object(v)?;
        let s = |k: &str| -> Res<Option<String>> {
            Ok(o.get(k).map(string).transpose()?.map(str::to_owned))
        };
        Ok(ModifierTest {
            attribute: o.get("attribute").map(IdSet::parse).transpose()?,
            id: s("id")?.map(|i| qualify(&i)),
            amount: Bounds::field(o, "amount")?,
            operation: s("operation")?,
            slot: s("slot")?,
        })
    }

    fn test(&self, e: &vanillite_nbt::Compound, world: &dyn World) -> bool {
        let s = |k: &str| e.get(k).and_then(vanillite_nbt::Tag::as_str);
        self.attribute.as_ref().is_none_or(|set| {
            s("type").is_some_and(|a| set.contains("minecraft:attribute", &qualify(a), world))
        }) && self
            .id
            .as_deref()
            .is_none_or(|i| s("id").map(qualify).as_deref() == Some(i))
            && self.amount.matches(match e.get("amount") {
                Some(vanillite_nbt::Tag::Double(d)) => *d,
                _ => 0.0,
            })
            && self
                .operation
                .as_deref()
                .is_none_or(|o| s("operation") == Some(o))
            && self
                .slot
                .as_deref()
                .is_none_or(|slot| s("slot").unwrap_or("any") == slot)
    }
}

/// `MobEffectsPredicate`: every listed effect is active and matches.
#[derive(Clone, Debug, PartialEq)]
pub struct MobEffectsPredicate(Vec<(crate::effect::MobEffect, [Bounds; 2], [Option<bool>; 2])>);

impl MobEffectsPredicate {
    /// Parses `{"minecraft:speed": {"amplifier": .., "duration": .., "ambient": .., "visible": ..}}`.
    pub fn parse(v: &Value) -> Res<MobEffectsPredicate> {
        let mut out = Vec::new();
        for (name, p) in object(v)? {
            let effect = crate::effect::MobEffect::by_name(&qualify(name))
                .ok_or_else(|| format!("unknown effect {name}"))?;
            let p = object(p)?;
            out.push((
                effect,
                [
                    Bounds::field(p, "amplifier")?,
                    Bounds::field(p, "duration")?,
                ],
                [
                    p.get("ambient").and_then(Value::as_bool),
                    p.get("visible").and_then(Value::as_bool),
                ],
            ));
        }
        Ok(MobEffectsPredicate(out))
    }

    /// `test(instance)` (`Predicate<MobEffectInstance>`): the effect is listed and matches.
    fn test(&self, e: &EffectView) -> bool {
        self.0
            .iter()
            .find(|(effect, ..)| *effect == e.effect)
            .is_some_and(|(_, [amp, dur], [ambient, visible])| {
                amp.matches(f64::from(e.amplifier))
                    && dur.matches(f64::from(e.duration))
                    && ambient.is_none_or(|a| a == e.ambient)
                    && visible.is_none_or(|v| v == e.visible)
            })
    }

    /// `matches(activeEffects)`.
    pub fn matches(&self, effects: &[EffectView]) -> bool {
        self.0
            .iter()
            .all(|(effect, [amp, dur], [ambient, visible])| {
                effects
                    .iter()
                    .find(|e| e.effect == *effect)
                    .is_some_and(|e| {
                        amp.matches(f64::from(e.amplifier))
                            && dur.matches(f64::from(e.duration))
                            && ambient.is_none_or(|a| a == e.ambient)
                            && visible.is_none_or(|v| v == e.visible)
                    })
            })
    }
}

/// A damage source as `DamageSourcePredicate` sees it.
#[derive(Clone, Debug)]
pub struct DamageView {
    /// The damage type id.
    pub kind: String,
    /// The built-in damage type tag bits (`survival::damage::tag`).
    pub tags: u32,
    /// `getDirectEntity`.
    pub direct: Option<Box<EntityView<'static>>>,
    /// `getEntity`: the causing entity.
    pub cause: Option<Box<EntityView<'static>>>,
    /// `isDirect`: the direct entity is the causing entity.
    pub is_direct: bool,
    /// `getSourcePosition`.
    pub position: Option<Vec3>,
}

/// `DamageSourcePredicate`.
#[derive(Clone, Debug, PartialEq)]
pub struct DamageSourcePredicate {
    tags: Vec<(String, bool)>,
    direct: Option<EntityPredicate>,
    cause: Option<EntityPredicate>,
    is_direct: Option<bool>,
}

impl DamageSourcePredicate {
    /// Parses a damage source predicate.
    pub fn parse(v: &Value) -> Res<DamageSourcePredicate> {
        let o = object(v)?;
        let mut tags = Vec::new();
        for t in o
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let t = object(t)?;
            let id = string(t.get("id").ok_or("tag without id")?)?;
            let expected = t
                .get("expected")
                .and_then(Value::as_bool)
                .ok_or("tag without expected")?;
            tags.push((qualify(id.trim_start_matches('#')), expected));
        }
        Ok(DamageSourcePredicate {
            tags,
            direct: o
                .get("direct_entity")
                .map(EntityPredicate::parse)
                .transpose()?,
            cause: o
                .get("source_entity")
                .map(EntityPredicate::parse)
                .transpose()?,
            is_direct: o.get("is_direct").and_then(Value::as_bool),
        })
    }

    /// `matches(level, position, source)`.
    pub fn matches(&self, source: &DamageView, world: &dyn World, origin: Vec3) -> bool {
        let entity = |p: &Option<EntityPredicate>, e: &Option<Box<EntityView<'static>>>| {
            p.as_ref().is_none_or(|p| {
                e.as_ref()
                    .is_some_and(|e| p.matches(e, world, Some(origin)))
            })
        };
        self.tags.iter().all(|(tag, expected)| {
            let has = match crate::survival::damage::tag::NAMES
                .iter()
                .find(|(n, _)| *n == tag.as_str())
            {
                Some(&(_, bit)) => source.tags & bit != 0,
                None => world.in_tag("minecraft:damage_type", tag, &source.kind),
            };
            has == *expected
        }) && entity(&self.direct, &source.direct)
            && entity(&self.cause, &source.cause)
            && self.is_direct.is_none_or(|d| d == source.is_direct)
    }
}

/// `DamagePredicate`.
#[derive(Clone, Debug, PartialEq)]
pub struct DamagePredicate {
    dealt: Bounds,
    taken: Bounds,
    source_entity: Option<EntityPredicate>,
    blocked: Option<bool>,
    kind: Option<DamageSourcePredicate>,
}

/// A damage event: the source, the original and the actual damage, and whether it was
/// blocked.
#[derive(Clone, Debug)]
pub struct DamageEvent {
    pub source: DamageView,
    pub dealt: f32,
    pub taken: f32,
    pub blocked: bool,
}

impl DamagePredicate {
    /// Parses a damage predicate.
    pub fn parse(v: &Value) -> Res<DamagePredicate> {
        let o = object(v)?;
        Ok(DamagePredicate {
            dealt: Bounds::field(o, "dealt")?,
            taken: Bounds::field(o, "taken")?,
            source_entity: o
                .get("source_entity")
                .map(EntityPredicate::parse)
                .transpose()?,
            blocked: o.get("blocked").and_then(Value::as_bool),
            kind: o
                .get("type")
                .map(DamageSourcePredicate::parse)
                .transpose()?,
        })
    }

    /// `matches(player, source, originalDamage, actualDamage, blocked)`.
    pub fn matches(&self, d: &DamageEvent, world: &dyn World, origin: Vec3) -> bool {
        self.dealt.matches(f64::from(d.dealt))
            && self.taken.matches(f64::from(d.taken))
            && self.source_entity.as_ref().is_none_or(|p| {
                d.source
                    .cause
                    .as_ref()
                    .is_some_and(|e| p.matches(e, world, Some(origin)))
            })
            && self.blocked.is_none_or(|b| b == d.blocked)
            && self
                .kind
                .as_ref()
                .is_none_or(|k| k.matches(&d.source, world, origin))
    }
}

/// A property value matcher of `StatePropertiesPredicate`.
#[derive(Clone, Debug, PartialEq)]
enum ValueMatcher {
    Exact(String),
    Range(Option<String>, Option<String>),
}

/// `StatePropertiesPredicate`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StateMatcher(Vec<(String, ValueMatcher)>);

/// The type of a fluid state property (`BooleanProperty`, `IntegerProperty`).
#[derive(Clone, Copy, Debug)]
enum PropertyKind {
    Bool,
    Int,
}

impl StateMatcher {
    /// Parses `{"name": "value" | {"min": .., "max": ..}}`.
    pub fn parse(v: &Value) -> Res<StateMatcher> {
        let text = |v: &Value| -> Res<String> {
            Ok(match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                other => return Err(format!("bad state value {other}")),
            })
        };
        let mut out = Vec::new();
        for (name, value) in object(v)? {
            let matcher = match value {
                Value::Object(range) => ValueMatcher::Range(
                    range.get("min").map(text).transpose()?,
                    range.get("max").map(text).transpose()?,
                ),
                other => ValueMatcher::Exact(text(other)?),
            };
            out.push((name.clone(), matcher));
        }
        Ok(StateMatcher(out))
    }

    /// `matches(FluidState)`: `value` gives the value of a property of the state and its type
    /// (`None`: the state has no such property).
    fn matches_values(&self, value: impl Fn(&str) -> Option<(String, PropertyKind)>) -> bool {
        self.0.iter().all(|(name, matcher)| {
            let Some((actual, kind)) = value(name) else {
                return false;
            };
            // `Property.getValue(string)` and the values' order.
            let typed = |v: &str| -> Option<i64> {
                match kind {
                    PropertyKind::Bool => match v {
                        "false" => Some(0),
                        "true" => Some(1),
                        _ => None,
                    },
                    PropertyKind::Int => v.parse().ok(),
                }
            };
            let Some(actual) = typed(&actual) else {
                return false;
            };
            match matcher {
                ValueMatcher::Exact(expected) => typed(expected) == Some(actual),
                ValueMatcher::Range(min, max) => {
                    min.as_ref()
                        .is_none_or(|m| typed(m).is_some_and(|m| actual >= m))
                        && max
                            .as_ref()
                            .is_none_or(|m| typed(m).is_some_and(|m| actual <= m))
                }
            }
        })
    }

    /// `matches(BlockState)`.
    pub fn matches(&self, state: BlockState) -> bool {
        use crate::loot::{property_key, property_values};
        let block = state.block();
        self.0.iter().all(|(name, matcher)| {
            let (Some(values), Some(actual)) = (property_values(block, name), state.value(name))
            else {
                return false;
            };
            let Some(actual) = property_key(values, actual) else {
                return false;
            };
            match matcher {
                ValueMatcher::Exact(expected) => property_key(values, expected) == Some(actual),
                ValueMatcher::Range(min, max) => {
                    min.as_ref()
                        .is_none_or(|m| property_key(values, m).is_some_and(|m| actual >= m))
                        && max
                            .as_ref()
                            .is_none_or(|m| property_key(values, m).is_some_and(|m| actual <= m))
                }
            }
        })
    }
}

/// `BlockPredicate`.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockPredicate {
    blocks: Option<IdSet>,
    state: Option<StateMatcher>,
    /// `nbt`: compared with the block entity's full data (`NbtPredicate.matches`).
    nbt: Option<vanillite_nbt::Compound>,
    /// `components` and `predicates` (`DataComponentMatchers`) against the block entity's
    /// collected components, as an item predicate over them; `None` when both are empty.
    components: Option<ItemPredicate>,
    /// Every part is evaluated.
    supported: bool,
}

impl BlockPredicate {
    /// Parses the fields of a block predicate.
    pub fn parse(o: &Map<String, Value>) -> Res<BlockPredicate> {
        let non_empty = |k: &str| {
            o.get(k)
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty())
        };
        let nbt = match o.get("nbt") {
            None => None,
            Some(Value::String(s)) => Some(
                vanillite_nbt::snbt::parse_compound(s)
                    .map_err(|e| format!("bad nbt {s}: {e:?}"))?,
            ),
            Some(other) => return Err(format!("bad nbt {other}")),
        };
        let components = (non_empty("components") || non_empty("predicates"))
            .then(|| {
                let mut m = Map::new();
                for k in ["components", "predicates"] {
                    if let Some(v) = o.get(k) {
                        m.insert(k.to_owned(), v.clone());
                    }
                }
                ItemPredicate::parse(&Value::Object(m))
            })
            .transpose()?;
        Ok(BlockPredicate {
            blocks: o.get("blocks").map(IdSet::parse).transpose()?,
            state: o.get("state").map(StateMatcher::parse).transpose()?,
            nbt,
            supported: components.as_ref().is_none_or(ItemPredicate::is_supported),
            components,
        })
    }

    /// `matchesState` (a predicate with `nbt` or components needs the block entity:
    /// [`Self::matches`]).
    pub fn matches_state(&self, state: BlockState, world: &dyn World) -> bool {
        self.supported
            && self.nbt.is_none()
            && self.components.is_none()
            && self.matches_state_only(state, world)
    }

    fn matches_state_only(&self, state: BlockState, world: &dyn World) -> bool {
        self.supported
            && self
                .blocks
                .as_ref()
                .is_none_or(|b| b.contains("minecraft:block", state.block().name(), world))
            && self.state.as_ref().is_none_or(|s| s.matches(state))
    }

    /// `matches(level, pos)`: the block must be loaded.
    pub fn matches(&self, world: &dyn World, pos: BlockPos) -> bool {
        world.block(pos).is_some_and(|state| {
            self.matches_state_only(state, world)
                && self.nbt.as_ref().is_none_or(|nbt| {
                    world
                        .block_entity_nbt(pos)
                        .is_some_and(|actual| crate::item_nbt::compare_compounds(nbt, &actual))
                })
                && self.components.as_ref().is_none_or(|p| {
                    world
                        .block_components(pos)
                        .is_some_and(|c| p.test(&component_carrier(&c), world))
                })
        })
    }
}

/// A stack whose effective components are exactly `components` (every default of the
/// carrier item removed), so item predicates test a block entity's `collectComponents`.
fn component_carrier(components: &[(u16, Box<[u8]>)]) -> ItemStack {
    let item = registry::ITEM.id("minecraft:stone").unwrap_or(1);
    let mut stack = ItemStack::new(item, 1);
    let types = u16::try_from(registry::DATA_COMPONENT_TYPE.len()).unwrap_or(u16::MAX);
    for t in 0..types {
        stack.remove_component(t);
    }
    for (t, v) in components {
        stack.patch_mut().set(*t, v.clone());
    }
    stack
}

/// `FluidPredicate`: the fluids and the state properties of the fluid state.
#[derive(Clone, Debug, PartialEq)]
pub struct FluidPredicate {
    fluids: Option<IdSet>,
    state: Option<StateMatcher>,
}

impl FluidPredicate {
    fn parse(v: &Value) -> Res<FluidPredicate> {
        let o = object(v)?;
        Ok(FluidPredicate {
            fluids: o.get("fluids").map(IdSet::parse).transpose()?,
            state: o.get("state").map(StateMatcher::parse).transpose()?,
        })
    }

    /// `matches(level, pos)` of a loaded position.
    fn matches(&self, world: &dyn World, pos: BlockPos) -> bool {
        let Some(state) = world.block(pos) else {
            return false;
        };
        let fluid = state.fluid_state();
        let info = fluid.info();
        let name = registry::FLUID
            .name(info.fluid)
            .unwrap_or("minecraft:empty");
        self.fluids
            .as_ref()
            .is_none_or(|f| f.contains("minecraft:fluid", name, world))
            && self.state.as_ref().is_none_or(|s| {
                // `FlowingFluid.FALLING` and `LEVEL` (flowing fluids only).
                let flowing = !fluid.is_empty() && !info.source;
                s.matches_values(|property| match property {
                    "falling" if !fluid.is_empty() => {
                        Some((info.falling.to_string(), PropertyKind::Bool))
                    }
                    "level" if flowing => Some((info.amount.to_string(), PropertyKind::Int)),
                    _ => None,
                })
            })
    }
}

/// `LocationPredicate`.
#[derive(Clone, Debug, PartialEq)]
pub struct LocationPredicate {
    position: [Bounds; 3],
    biomes: Option<IdSet>,
    structures: Option<Box<IdSet>>,
    dimension: Option<String>,
    smokey: Option<bool>,
    /// `LightPredicate`: the `light` bounds.
    light: Option<Bounds>,
    block: Option<Box<BlockPredicate>>,
    fluid: Option<Box<FluidPredicate>>,
    can_see_sky: Option<bool>,
}

impl LocationPredicate {
    /// Parses a location predicate.
    pub fn parse(v: &Value) -> Res<LocationPredicate> {
        let o = object(v)?;
        let position = match o.get("position") {
            Some(p) => {
                let p = object(p)?;
                [
                    Bounds::field(p, "x")?,
                    Bounds::field(p, "y")?,
                    Bounds::field(p, "z")?,
                ]
            }
            None => [Bounds::default(); 3],
        };
        let flag = |k: &str| {
            o.get(k)
                .map(|v| v.as_bool().ok_or_else(|| format!("{k} is not a boolean")))
                .transpose()
        };
        Ok(LocationPredicate {
            position,
            biomes: o.get("biomes").map(IdSet::parse).transpose()?,
            structures: o
                .get("structures")
                .map(IdSet::parse)
                .transpose()?
                .map(Box::new),
            dimension: o.get("dimension").map(string).transpose()?.map(qualify),
            smokey: flag("smokey")?,
            light: o
                .get("light")
                .map(|l| object(l).and_then(|l| Bounds::field(l, "light")))
                .transpose()?,
            block: o
                .get("block")
                .map(|b| object(b).and_then(BlockPredicate::parse))
                .transpose()?
                .map(Box::new),
            fluid: o
                .get("fluid")
                .map(FluidPredicate::parse)
                .transpose()?
                .map(Box::new),
            can_see_sky: flag("can_see_sky")?,
        })
    }

    /// Whether every part is evaluated.
    fn is_supported(&self) -> bool {
        self.block.as_ref().is_none_or(|b| b.supported)
    }

    /// `matches(level, x, y, z)`.
    pub fn matches(&self, world: &dyn World, at: Vec3) -> bool {
        if !(self.position[0].matches(at.x)
            && self.position[1].matches(at.y)
            && self.position[2].matches(at.z))
        {
            return false;
        }
        if self
            .dimension
            .as_ref()
            .is_some_and(|d| d != world.dimension())
        {
            return false;
        }
        let pos = crate::player::play::block_pos_of(at);
        let loaded = world.block(pos).is_some();
        if let Some(biomes) = &self.biomes {
            let inside = loaded
                && world
                    .biome(pos)
                    .is_some_and(|b| biomes.contains("minecraft:worldgen/biome", &b, world));
            if !inside {
                return false;
            }
        }
        if self
            .structures
            .as_ref()
            .is_some_and(|s| !(loaded && world.structure_at(pos, s)))
        {
            return false;
        }
        if self
            .smokey
            .is_some_and(|s| !(loaded && s == world.smokey(pos)))
        {
            return false;
        }
        // `LightPredicate.matches`: loaded, with the light in the bounds.
        if self
            .light
            .is_some_and(|l| !(loaded && l.matches(f64::from(world.light(pos)))))
        {
            return false;
        }
        if self.block.as_ref().is_some_and(|b| !b.matches(world, pos)) {
            return false;
        }
        if self.fluid.as_ref().is_some_and(|f| !f.matches(world, pos)) {
            return false;
        }
        self.can_see_sky.is_none_or(|s| s == world.can_see_sky(pos))
    }
}

/// `DistancePredicate`.
#[derive(Clone, Debug, PartialEq)]
pub struct DistancePredicate {
    x: Bounds,
    y: Bounds,
    z: Bounds,
    horizontal: Bounds,
    absolute: Bounds,
}

impl DistancePredicate {
    pub fn parse(v: &Value) -> Res<DistancePredicate> {
        let o = object(v)?;
        Ok(DistancePredicate {
            x: Bounds::field(o, "x")?,
            y: Bounds::field(o, "y")?,
            z: Bounds::field(o, "z")?,
            horizontal: Bounds::field(o, "horizontal")?,
            absolute: Bounds::field(o, "absolute")?,
        })
    }

    /// `matches(x0, y0, z0, x1, y1, z1)` with float differences.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    pub fn matches(&self, a: Vec3, b: Vec3) -> bool {
        let (dx, dy, dz) = ((a.x - b.x) as f32, (a.y - b.y) as f32, (a.z - b.z) as f32);
        self.x.matches(f64::from(dx.abs()))
            && self.y.matches(f64::from(dy.abs()))
            && self.z.matches(f64::from(dz.abs()))
            && self.horizontal.matches_sqr(f64::from(dx * dx + dz * dz))
            && self
                .absolute
                .matches_sqr(f64::from(dx * dx + dy * dy + dz * dz))
    }
}

/// One part of an `EntityPredicate`.
#[derive(Clone, Debug, PartialEq)]
enum EntityPart {
    Type(IdSet),
    Location(LocationPredicate),
    SteppingOn(LocationPredicate),
    Distance(DistancePredicate),
    Flags(Vec<(String, bool)>),
    /// Slot index into [`EntityView::equipment`] and the predicate.
    Equipment(Vec<(usize, ItemPredicate)>),
    Team(String),
    Player(Box<PlayerPart>),
    Vehicle(Box<EntityPredicate>),
    Passenger(Box<EntityPredicate>),
    Effects(MobEffectsPredicate),
    /// Exact entity components by name and expected value (normalised; `None`: a value that
    /// does not convert never matches).
    Components(Vec<(String, Option<vanillite_nbt::Tag>)>),
    /// `predicates`: partial component predicates.
    Predicates(Vec<SubPredicate>),
    /// `nbt`.
    Nbt(vanillite_nbt::Tag),
    /// `entity_tags`: any of, all of, none of.
    Tags([Option<Vec<String>>; 3]),
    /// `type_specific/lightning`: blocks set on fire, `entity_struck`.
    Lightning(Bounds, Option<Box<EntityPredicate>>),
    /// `movement`: x, y, z, speed, horizontal speed, vertical speed, fall distance.
    Movement([Bounds; 7]),
    /// `periodic_tick`.
    PeriodicTick(i32),
    /// `movement_affected_by`: the location of the block below that affects movement.
    MovementAffectedBy(LocationPredicate),
    /// `targeted_entity`.
    TargetedEntity(Box<EntityPredicate>),
    /// `type_specific/sheep`.
    Sheep(Option<bool>),
    /// `type_specific/slime` (`cube_mob`).
    CubeMob(Bounds),
    /// `type_specific/raider`: `has_raid`, `is_captain`.
    Raider(bool, bool),
    /// `type_specific/fishing_hook`.
    FishingHook(Option<bool>),
    /// `slots`: slot ranges with their item predicate.
    Slots(Vec<(Vec<SlotRef>, ItemPredicate)>),
    Unsupported,
}

/// A slot of `SlotRange`s an entity has.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SlotRef {
    /// Index into [`EntityView::equipment`].
    Equipment(usize),
    /// Player inventory index.
    Inventory(usize),
}

/// `SlotRanges`: the slots of a slot range name.
fn slot_range(name: &str) -> Option<Vec<SlotRef>> {
    let eq = |i: usize| vec![SlotRef::Equipment(i)];
    let inv = |r: std::ops::Range<usize>| r.map(SlotRef::Inventory).collect::<Vec<_>>();
    Some(match name {
        "weapon" | "weapon.mainhand" => eq(5),
        "weapon.offhand" => eq(6),
        "weapon.*" => vec![SlotRef::Equipment(5), SlotRef::Equipment(6)],
        "armor.head" => eq(0),
        "armor.chest" => eq(1),
        "armor.legs" => eq(2),
        "armor.feet" => eq(3),
        "armor.body" => eq(4),
        "armor.*" => (0..4).map(SlotRef::Equipment).collect(),
        "hotbar.*" => inv(0..9),
        "inventory.*" => inv(9..36),
        "container.*" => inv(0..36),
        _ => {
            let (prefix, n) = name.rsplit_once('.')?;
            let n: usize = n.parse().ok()?;
            match prefix {
                "hotbar" if n < 9 => vec![SlotRef::Inventory(n)],
                "inventory" if n < 27 => vec![SlotRef::Inventory(n.checked_add(9)?)],
                "container" if n < 36 => vec![SlotRef::Inventory(n)],
                _ => return None,
            }
        }
    })
}

/// `EntityPredicate`: all parts must match.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityPredicate(Vec<EntityPart>);

/// `PlayerPredicate.AdvancementPredicate`.
#[derive(Clone, Debug, PartialEq)]
enum AdvancementCheck {
    /// `AdvancementDonePredicate`.
    Done(bool),
    /// `AdvancementCriterionsPredicate`.
    Criteria(Vec<(String, bool)>),
}

/// The keys of `InputPredicate` in [`PlayerPart::input`] order.
const INPUT_KEYS: [&str; 7] = [
    "forward", "backward", "left", "right", "jump", "sneak", "sprint",
];

/// `PlayerPredicate` (`type_specific/player`).
#[derive(Clone, Debug, PartialEq)]
struct PlayerPart {
    game_modes: Option<Vec<GameMode>>,
    level: Bounds,
    /// `FoodPredicate`: the food level and the saturation.
    food: Option<(Bounds, Bounds)>,
    stats: Vec<(Stat, Bounds)>,
    /// Recipe ids and whether the player must know them.
    recipes: Vec<(String, bool)>,
    advancements: Vec<(String, AdvancementCheck)>,
    looking_at: Option<EntityPredicate>,
    /// `InputPredicate` in [`INPUT_KEYS`] order.
    input: [Option<bool>; 7],
}

impl PlayerPart {
    /// `PlayerPredicate.matches(player, level, position)`.
    fn matches(&self, p: &PlayerFacts<'_>, pos: Vec3, world: &dyn World) -> bool {
        let level_ok = self.level.matches(f64::from(p.level));
        let food_ok = self.food.is_none_or(|(level, saturation)| {
            level.matches(f64::from(p.food.0)) && saturation.matches(f64::from(p.food.1))
        });
        let mode_ok = self
            .game_modes
            .as_ref()
            .is_none_or(|m| m.contains(&p.game_mode));
        let stats_ok = self.stats.is_empty()
            || p.stats.is_some_and(|counter| {
                self.stats
                    .iter()
                    .all(|(stat, range)| range.matches(f64::from(counter.get(*stat))))
            });
        let recipes_ok = self.recipes.is_empty()
            || p.recipes.is_some_and(|book| {
                self.recipes
                    .iter()
                    .all(|(id, known)| book.contains(id) == *known)
            });
        level_ok
            && food_ok
            && mode_ok
            && stats_ok
            && recipes_ok
            && (self.advancements.is_empty() || p.own && self.advancements_match(world))
            && self.looking_at.as_ref().is_none_or(|l| {
                p.own
                    && world
                        .looking_at()
                        .is_some_and(|target| l.matches(target, world, Some(pos)))
            })
            && self.input_matches(p.input)
    }

    /// The `advancements` of `PlayerPredicate.matches`: each advancement exists and its
    /// progress passes.
    fn advancements_match(&self, world: &dyn World) -> bool {
        let Some(progress) = world.progress() else {
            return false;
        };
        let set = progress.advancements();
        self.advancements.iter().all(|(id, check)| {
            let Some(index) = set.index(id) else {
                return false;
            };
            match check {
                AdvancementCheck::Done(done) => progress.is_done(index) == *done,
                AdvancementCheck::Criteria(criteria) => criteria.iter().all(|(name, done)| {
                    set.get(index)
                        .and_then(|a| a.criterion(name))
                        .is_some_and(|c| progress.criterion_done(index, c) == *done)
                }),
            }
        })
    }

    /// `InputPredicate.matches(input)`.
    fn input_matches(&self, input: vanillite_protocol::packets::play::serverbound::Input) -> bool {
        let pressed = [
            input.forward,
            input.backward,
            input.left,
            input.right,
            input.jump,
            input.shift,
            input.sprint,
        ];
        self.input
            .iter()
            .zip(pressed)
            .all(|(want, have)| want.is_none_or(|w| w == have))
    }
}

const EQUIPMENT: [&str; 7] = [
    "head", "chest", "legs", "feet", "body", "mainhand", "offhand",
];

fn game_mode(name: &str) -> Option<GameMode> {
    Some(match name {
        "survival" => GameMode::Survival,
        "creative" => GameMode::Creative,
        "adventure" => GameMode::Adventure,
        "spectator" => GameMode::Spectator,
        _ => return None,
    })
}

impl EntityPredicate {
    /// Parses the map of entity sub-predicates.
    pub fn parse(v: &Value) -> Res<EntityPredicate> {
        let mut parts = Vec::new();
        for (key, value) in object(v)? {
            parts.push(match qualify(key).as_str() {
                "minecraft:entity_type" => EntityPart::Type(IdSet::parse(value)?),
                "minecraft:location" => EntityPart::Location(LocationPredicate::parse(value)?),
                "minecraft:stepping_on" => EntityPart::SteppingOn(LocationPredicate::parse(value)?),
                "minecraft:distance" => EntityPart::Distance(DistancePredicate::parse(value)?),
                "minecraft:flags" => EntityPart::Flags(
                    object(value)?
                        .iter()
                        .map(|(k, v)| {
                            v.as_bool()
                                .map(|b| (k.clone(), b))
                                .ok_or_else(|| format!("flag {k} is not a boolean"))
                        })
                        .collect::<Res<_>>()?,
                ),
                "minecraft:equipment" => {
                    let o = object(value)?;
                    let mut slots = Vec::new();
                    for (slot, name) in EQUIPMENT.iter().enumerate() {
                        if let Some(p) = o.get(*name) {
                            slots.push((slot, ItemPredicate::parse(p)?));
                        }
                    }
                    EntityPart::Equipment(slots)
                }
                "minecraft:team" => EntityPart::Team(string(value)?.to_owned()),
                "minecraft:type_specific/player" => Self::player(object(value)?)?,
                "minecraft:vehicle" => {
                    EntityPart::Vehicle(Box::new(EntityPredicate::parse(value)?))
                }
                "minecraft:passenger" => {
                    EntityPart::Passenger(Box::new(EntityPredicate::parse(value)?))
                }
                "minecraft:effects" => EntityPart::Effects(MobEffectsPredicate::parse(value)?),
                "minecraft:components" => {
                    want_entity_nbt();
                    let mut out = Vec::new();
                    for (k, v) in object(value)? {
                        let name = qualify(k);
                        let expected = match v {
                            // Registry holders (variants) by id.
                            Value::String(s) if name.contains('/') => {
                                Some(vanillite_nbt::Tag::String(qualify(s)))
                            }
                            other => {
                                normalized_component(&name, &crate::item_nbt::json_to_nbt(other))
                            }
                        };
                        out.push((name, expected));
                    }
                    EntityPart::Components(out)
                }
                "minecraft:predicates" => {
                    want_entity_nbt();
                    let mut out = Vec::new();
                    for (k, v) in object(value)? {
                        out.push(SubPredicate::parse(&qualify(k), v)?);
                    }
                    EntityPart::Predicates(out)
                }
                "minecraft:nbt" => {
                    want_entity_nbt();
                    EntityPart::Nbt(lenient_nbt(value)?)
                }
                "minecraft:entity_tags" => {
                    let o = object(value)?;
                    let list = |k: &str| -> Res<Option<Vec<String>>> {
                        o.get(k)
                            .map(|l| {
                                l.as_array()
                                    .ok_or_else(|| format!("{k} is not a list"))?
                                    .iter()
                                    .map(|t| string(t).map(str::to_owned))
                                    .collect()
                            })
                            .transpose()
                    };
                    EntityPart::Tags([list("any_of")?, list("all_of")?, list("none_of")?])
                }
                "minecraft:movement" => {
                    let o = object(value)?;
                    let mut b = [Bounds::default(); 7];
                    for (slot, key) in b.iter_mut().zip([
                        "x",
                        "y",
                        "z",
                        "speed",
                        "horizontal_speed",
                        "vertical_speed",
                        "fall_distance",
                    ]) {
                        *slot = Bounds::field(o, key)?;
                    }
                    EntityPart::Movement(b)
                }
                "minecraft:periodic_tick" => EntityPart::PeriodicTick(
                    value
                        .as_i64()
                        .and_then(|n| i32::try_from(n).ok())
                        .filter(|&n| n > 0)
                        .ok_or("periodic_tick must be a positive int")?,
                ),
                "minecraft:movement_affected_by" => {
                    EntityPart::MovementAffectedBy(LocationPredicate::parse(value)?)
                }
                "minecraft:targeted_entity" => {
                    EntityPart::TargetedEntity(Box::new(EntityPredicate::parse(value)?))
                }
                "minecraft:type_specific/sheep" => {
                    EntityPart::Sheep(object(value)?.get("sheared").and_then(Value::as_bool))
                }
                "minecraft:type_specific/slime" | "minecraft:type_specific/cube_mob" => {
                    EntityPart::CubeMob(Bounds::field(object(value)?, "size")?)
                }
                "minecraft:type_specific/raider" => {
                    let o = object(value)?;
                    let flag = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
                    EntityPart::Raider(flag("has_raid"), flag("is_captain"))
                }
                "minecraft:type_specific/fishing_hook" => EntityPart::FishingHook(
                    object(value)?.get("in_open_water").and_then(Value::as_bool),
                ),
                "minecraft:slots" => {
                    let mut out = Vec::new();
                    let mut known = true;
                    for (range, p) in object(value)? {
                        match slot_range(range) {
                            Some(slots) => out.push((slots, ItemPredicate::parse(p)?)),
                            None => known = false,
                        }
                    }
                    if known {
                        EntityPart::Slots(out)
                    } else {
                        EntityPart::Unsupported
                    }
                }
                "minecraft:type_specific/lightning" => {
                    let o = object(value)?;
                    EntityPart::Lightning(
                        Bounds::field(o, "blocks_set_on_fire")?,
                        o.get("entity_struck")
                            .map(|p| EntityPredicate::parse(p).map(Box::new))
                            .transpose()?,
                    )
                }
                _ => EntityPart::Unsupported,
            });
        }
        Ok(EntityPredicate(parts))
    }

    /// Whether every part is evaluated, also of the nested predicates.
    fn is_supported(&self) -> bool {
        self.0.iter().all(|part| match part {
            EntityPart::Unsupported => false,
            EntityPart::Location(l) | EntityPart::SteppingOn(l) => l.is_supported(),
            EntityPart::Equipment(slots) => slots.iter().all(|(_, p)| p.is_supported()),
            EntityPart::Vehicle(p) | EntityPart::Passenger(p) | EntityPart::TargetedEntity(p) => {
                p.is_supported()
            }
            EntityPart::Lightning(_, p) => p.as_deref().is_none_or(EntityPredicate::is_supported),
            EntityPart::Predicates(p) => !p.contains(&SubPredicate::Unsupported),
            EntityPart::Components(c) => c.iter().all(|(_, v)| v.is_some()),
            EntityPart::MovementAffectedBy(l) => l.is_supported(),
            EntityPart::Slots(slots) => slots.iter().all(|(_, p)| p.is_supported()),
            EntityPart::Player(p) => p
                .looking_at
                .as_ref()
                .is_none_or(EntityPredicate::is_supported),
            _ => true,
        })
    }

    /// `PlayerPredicate`.
    fn player(o: &Map<String, Value>) -> Res<EntityPart> {
        let game_modes = match o.get("gamemode") {
            None => None,
            Some(Value::String(s)) => Some(vec![game_mode(s).ok_or("unknown game mode")?]),
            Some(Value::Array(list)) => Some(
                list.iter()
                    .map(|m| {
                        string(m)
                            .ok()
                            .and_then(game_mode)
                            .ok_or("unknown game mode")
                    })
                    .collect::<Result<_, _>>()?,
            ),
            Some(other) => return Err(format!("bad gamemode {other}")),
        };
        let mut stats = Vec::new();
        for s in o
            .get("stats")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let s = object(s)?;
            let kind = StatType::by_name(&qualify(string(s.get("type").ok_or("stat type")?)?))
                .ok_or("unknown stat type")?;
            let name = qualify(string(s.get("stat").ok_or("stat")?)?);
            let value = kind.registry().id(&name).ok_or("unknown stat")?;
            stats.push((Stat { kind, value }, Bounds::field(s, "value")?));
        }
        let bool_map = |key: &str| -> Res<Vec<(String, bool)>> {
            o.get(key)
                .map(object)
                .transpose()?
                .into_iter()
                .flatten()
                .map(|(k, v)| {
                    v.as_bool()
                        .map(|b| (k.clone(), b))
                        .ok_or_else(|| format!("{key}: {k} is not a boolean"))
                })
                .collect()
        };
        let recipes = bool_map("recipes")?
            .into_iter()
            .map(|(k, b)| (qualify(&k), b))
            .collect();
        let mut advancements = Vec::new();
        for (id, check) in o
            .get("advancements")
            .map(object)
            .transpose()?
            .into_iter()
            .flatten()
        {
            let check = match check {
                Value::Bool(done) => AdvancementCheck::Done(*done),
                Value::Object(criteria) => AdvancementCheck::Criteria(
                    criteria
                        .iter()
                        .map(|(k, v)| {
                            v.as_bool()
                                .map(|b| (k.clone(), b))
                                .ok_or_else(|| format!("criterion {k} is not a boolean"))
                        })
                        .collect::<Res<_>>()?,
                ),
                other => return Err(format!("bad advancement predicate {other}")),
            };
            advancements.push((qualify(id), check));
        }
        let food = o
            .get("food")
            .map(|f| {
                let f = object(f)?;
                Ok::<_, String>((Bounds::field(f, "level")?, Bounds::field(f, "saturation")?))
            })
            .transpose()?;
        let mut input = [None; 7];
        if let Some(i) = o.get("input") {
            let i = object(i)?;
            for (slot, key) in input.iter_mut().zip(INPUT_KEYS) {
                *slot = i.get(key).and_then(Value::as_bool);
            }
        }
        Ok(EntityPart::Player(Box::new(PlayerPart {
            game_modes,
            level: Bounds::field(o, "level")?,
            food,
            stats,
            recipes,
            advancements,
            looking_at: o
                .get("looking_at")
                .map(EntityPredicate::parse)
                .transpose()?,
            input,
        })))
    }

    /// `matches(entity, level, position)`; `origin` is the position of the context.
    pub fn matches(&self, e: &EntityView<'_>, world: &dyn World, origin: Option<Vec3>) -> bool {
        self.0.iter().all(|part| match part {
            EntityPart::Type(types) => types.contains("minecraft:entity_type", e.kind, world),
            EntityPart::Location(l) => l.matches(world, e.pos),
            EntityPart::SteppingOn(l) => {
                if !e.on_ground {
                    return false;
                }
                let below =
                    crate::player::play::block_pos_of(Vec3::new(e.pos.x, e.pos.y - 0.2, e.pos.z));
                let center = Vec3::new(
                    f64::from(below.x) + 0.5,
                    f64::from(below.y) + 0.5,
                    f64::from(below.z) + 0.5,
                );
                l.matches(world, center)
            }
            EntityPart::Distance(d) => origin.is_some_and(|o| d.matches(o, e.pos)),
            EntityPart::Flags(flags) => flags.iter().all(|(name, want)| {
                let have = match name.as_str() {
                    "is_on_ground" => e.on_ground,
                    "is_on_fire" => e.on_fire,
                    "is_sneaking" => e.crouching,
                    "is_sprinting" => e.sprinting,
                    "is_swimming" => e.swimming,
                    // `LivingEntity.isFallFlying` or `abilities.flying`.
                    "is_flying" => e.living && (e.flying || e.fall_flying),
                    // Not tested on entities that are not living.
                    "is_fall_flying" if !e.living => return true,
                    "is_baby" if !e.living => return true,
                    "is_fall_flying" => e.fall_flying,
                    "is_in_water" => e.in_water,
                    "is_baby" => e.baby,
                    _ => return false,
                };
                have == *want
            }),
            EntityPart::Equipment(slots) => {
                e.living
                    && slots.iter().all(|(slot, p)| {
                        e.equipment
                            .get(*slot)
                            .is_some_and(|stack| p.test(stack, world))
                    })
            }
            EntityPart::Team(team) => e.team.as_deref() == Some(team.as_str()),
            EntityPart::Player(part) => e.player.is_some_and(|p| part.matches(&p, e.pos, world)),
            EntityPart::Vehicle(p) => e
                .vehicle
                .as_ref()
                .is_some_and(|v| p.matches(v, world, origin)),
            EntityPart::Passenger(p) => e.passengers.iter().any(|v| p.matches(v, world, origin)),
            EntityPart::Effects(p) => p.matches(&e.effects),
            EntityPart::Components(expected) => expected.iter().all(|(name, value)| {
                value.as_ref().is_some_and(|value| {
                    e.component(name)
                        .and_then(|actual| normalized_component(name, &actual))
                        .is_some_and(|actual| actual == *value)
                })
            }),
            EntityPart::Predicates(predicates) => {
                predicates.iter().all(|p| p.matches_entity(e, world))
            }
            EntityPart::Nbt(expected) => e.nbt.as_ref().is_some_and(|n| {
                crate::item_nbt::compare_nbt(expected, &vanillite_nbt::Tag::Compound((**n).clone()))
            }),
            EntityPart::Tags([any, all, none]) => {
                let has = |t: &String| e.tags.contains(t);
                any.as_ref().is_none_or(|l| l.iter().any(has))
                    && none.as_ref().is_none_or(|l| !l.iter().any(has))
                    && all.as_ref().is_none_or(|l| l.iter().all(has))
            }
            EntityPart::Lightning(fires, struck) => {
                e.lightning_fires
                    .is_some_and(|n| fires.matches(f64::from(n)))
                    && struck
                        .as_ref()
                        .is_none_or(|p| e.struck.iter().any(|s| p.matches(s, world, origin)))
            }
            EntityPart::Movement([x, y, z, speed, horizontal, vertical, fall]) => {
                let m = e.movement;
                x.matches(m.x)
                    && y.matches(m.y)
                    && z.matches(m.z)
                    && speed.matches_sqr(m.x * m.x + m.y * m.y + m.z * m.z)
                    && horizontal.matches_sqr(m.x * m.x + m.z * m.z)
                    && vertical.matches(m.y.abs())
                    && fall.matches(e.fall_distance)
            }
            EntityPart::PeriodicTick(n) => e.tick_count.checked_rem(*n) == Some(0),
            EntityPart::MovementAffectedBy(l) => {
                // `getBlockPosBelowThatAffectsMyMovement`: 0.500001 below the feet.
                let below = crate::player::play::block_pos_of(Vec3::new(
                    e.pos.x,
                    e.pos.y - 0.500_001,
                    e.pos.z,
                ));
                l.matches(
                    world,
                    Vec3::new(
                        f64::from(below.x) + 0.5,
                        f64::from(below.y) + 0.5,
                        f64::from(below.z) + 0.5,
                    ),
                )
            }
            EntityPart::TargetedEntity(p) => e
                .target
                .as_ref()
                .is_some_and(|t| p.matches(t, world, origin)),
            EntityPart::Sheep(sheared) => e
                .sheared
                .is_some_and(|s| sheared.is_none_or(|want| want == s)),
            EntityPart::CubeMob(size) => e.cube_size.is_some_and(|s| size.matches(f64::from(s))),
            EntityPart::Raider(has_raid, captain) => e
                .raider
                .is_some_and(|(r, c)| r == *has_raid && c == *captain),
            EntityPart::FishingHook(open) => {
                open.is_none_or(|want| e.open_water.is_some_and(|o| o == want))
            }
            EntityPart::Slots(slots) => slots.iter().all(|(refs, p)| {
                refs.iter().any(|r| match r {
                    SlotRef::Equipment(i) => e.equipment.get(*i).is_some_and(|s| p.test(s, world)),
                    SlotRef::Inventory(i) => e
                        .inventory
                        .as_ref()
                        .and_then(|inv| inv.get(*i))
                        .is_some_and(|s| p.test(s, world)),
                })
            }),
            EntityPart::Unsupported => false,
        })
    }
}

/// What a loot condition is tested against (`LootContext`).
#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub world: &'a dyn World,
    /// `this`.
    pub this: Option<&'a EntityView<'a>>,
    /// `origin`.
    pub origin: Option<Vec3>,
    /// `block_state`.
    pub block_state: Option<BlockState>,
    /// `tool`.
    pub tool: Option<&'a ItemStack>,
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("this", &self.this)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// A loot condition (`LootItemCondition`).
#[derive(Clone, Debug, PartialEq)]
pub enum Condition {
    /// `entity_properties` of `this`; without a predicate it needs the entity only.
    EntityProperties(Option<EntityPredicate>),
    /// `location_check`: the predicate at the origin moved by the offset.
    LocationCheck(Option<LocationPredicate>, [i32; 3]),
    /// `match_block` on the block state.
    MatchBlock(BlockPredicate),
    /// `match_tool`.
    MatchTool(Option<ItemPredicate>),
    AllOf(Vec<Condition>),
    AnyOf(Vec<Condition>),
    Inverted(Box<Condition>),
    /// Named predicates, other entity targets and other condition types: never true.
    Unsupported,
}

impl Condition {
    /// Parses a condition holder: an inline condition (named predicates are unsupported).
    pub fn parse(v: &Value) -> Res<Condition> {
        if v.is_string() {
            return Ok(Condition::Unsupported);
        }
        let o = object(v)?;
        let kind = qualify(string(o.get("type").ok_or("condition without type")?)?);
        let list = |key: &str| -> Res<Vec<Condition>> {
            match o.get(key) {
                Some(Value::Array(items)) => items.iter().map(Condition::parse).collect(),
                _ => Err(format!("{key} is not a list")),
            }
        };
        Ok(match kind.as_str() {
            "minecraft:entity_properties" => {
                if o.get("entity").and_then(Value::as_str) != Some("this") {
                    return Ok(Condition::Unsupported);
                }
                Condition::EntityProperties(
                    o.get("predicate").map(EntityPredicate::parse).transpose()?,
                )
            }
            "minecraft:location_check" => {
                let offset = |k: &str| {
                    o.get(k)
                        .map_or(Some(0), |v| v.as_i64().and_then(|v| i32::try_from(v).ok()))
                        .ok_or_else(|| format!("{k} is not an int"))
                };
                Condition::LocationCheck(
                    o.get("predicate")
                        .map(LocationPredicate::parse)
                        .transpose()?,
                    [offset("offsetX")?, offset("offsetY")?, offset("offsetZ")?],
                )
            }
            "minecraft:match_block" => Condition::MatchBlock(BlockPredicate::parse(o)?),
            "minecraft:match_tool" => {
                Condition::MatchTool(o.get("predicate").map(ItemPredicate::parse).transpose()?)
            }
            "minecraft:all_of" => Condition::AllOf(list("terms")?),
            "minecraft:any_of" => Condition::AnyOf(list("terms")?),
            // The inversion of what is not evaluated is not evaluated either.
            "minecraft:inverted" => {
                let term = Condition::parse(o.get("term").ok_or("no term")?)?;
                if term.is_supported() {
                    Condition::Inverted(Box::new(term))
                } else {
                    Condition::Unsupported
                }
            }
            _ => Condition::Unsupported,
        })
    }

    /// Whether every part is evaluated (no [`Condition::Unsupported`] or unsupported
    /// predicate part inside).
    fn is_supported(&self) -> bool {
        match self {
            Condition::EntityProperties(p) => p.as_ref().is_none_or(EntityPredicate::is_supported),
            Condition::LocationCheck(p, _) => {
                p.as_ref().is_none_or(LocationPredicate::is_supported)
            }
            Condition::MatchBlock(p) => p.supported,
            Condition::MatchTool(p) => p.as_ref().is_none_or(ItemPredicate::is_supported),
            Condition::AllOf(terms) | Condition::AnyOf(terms) => {
                terms.iter().all(Condition::is_supported)
            }
            Condition::Inverted(term) => term.is_supported(),
            Condition::Unsupported => false,
        }
    }

    /// `test(context)`: conditions that need a missing parameter are false.
    pub fn test(&self, ctx: &Context<'_>) -> bool {
        match self {
            Condition::EntityProperties(p) => ctx.this.is_some_and(|e| {
                p.as_ref()
                    .is_none_or(|p| p.matches(e, ctx.world, ctx.origin))
            }),
            Condition::LocationCheck(p, [dx, dy, dz]) => ctx.origin.is_some_and(|o| {
                p.as_ref().is_none_or(|p| {
                    let at = Vec3::new(
                        o.x + f64::from(*dx),
                        o.y + f64::from(*dy),
                        o.z + f64::from(*dz),
                    );
                    p.matches(ctx.world, at)
                })
            }),
            Condition::MatchBlock(p) => ctx
                .block_state
                .is_some_and(|s| p.matches_state(s, ctx.world)),
            Condition::MatchTool(p) => ctx
                .tool
                .is_some_and(|t| p.as_ref().is_none_or(|p| p.test(t, ctx.world))),
            Condition::AllOf(terms) => terms.iter().all(|t| t.test(ctx)),
            Condition::AnyOf(terms) => terms.iter().any(|t| t.test(ctx)),
            Condition::Inverted(term) => !term.test(ctx),
            Condition::Unsupported => false,
        }
    }
}

/// `blocks`/`state` of `enter_block`.
pub fn block_matches(
    blocks: Option<&IdSet>,
    state_matcher: Option<&StateMatcher>,
    state: BlockState,
    world: &dyn World,
) -> bool {
    let block: BlockId = state.block();
    blocks.is_none_or(|b| b.contains("minecraft:block", block.name(), world))
        && state_matcher.is_none_or(|s| s.matches(state))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Flat;

    impl World for Flat {
        fn dimension(&self) -> &str {
            "minecraft:overworld"
        }
        fn block(&self, pos: BlockPos) -> Option<BlockState> {
            (pos.y < 0).then(|| BlockId::by_name("minecraft:stone").unwrap().default_state())
        }
        fn biome(&self, _pos: BlockPos) -> Option<String> {
            Some("minecraft:plains".to_owned())
        }
        fn in_tag(&self, _registry: &str, tag: &str, id: &str) -> bool {
            tag == "minecraft:base_stone_overworld" && id == "minecraft:stone"
        }
    }

    #[test]
    fn movement_tick_and_type_specific_sub_predicates() {
        let p = EntityPredicate::parse(&json!({
            "movement": {"horizontal_speed": {"min": 4.0}, "fall_distance": {"max": 1.0}},
            "periodic_tick": 10,
            "type_specific/sheep": {"sheared": false},
            "movement_affected_by": {"block": {"blocks": "minecraft:stone"}}
        }));
        let p = p.unwrap();
        assert!(p.is_supported());
        let mut v = EntityView::new("minecraft:sheep", Vec3::new(0.5, 0.0, 0.5));
        v.movement = Vec3::new(3.0, 0.0, 3.0);
        v.tick_count = 20;
        v.sheared = Some(false);
        assert!(p.matches(&v, &Flat, None));
        v.tick_count = 21;
        assert!(!p.matches(&v, &Flat, None));
        v.tick_count = 30;
        v.sheared = Some(true);
        assert!(!p.matches(&v, &Flat, None));
        v.sheared = Some(false);
        v.movement = Vec3::new(1.0, 0.0, 1.0);
        assert!(!p.matches(&v, &Flat, None));
        // On a block above the stone, the block below that affects movement is air.
        v.movement = Vec3::new(3.0, 0.0, 3.0);
        v.pos = Vec3::new(0.5, 5.0, 0.5);
        assert!(!p.matches(&v, &Flat, None));
    }

    #[test]
    fn location_conditions() {
        let c = Condition::parse(&json!({
            "type": "minecraft:location_check",
            "offsetY": -1,
            "predicate": {"block": {"blocks": "#minecraft:base_stone_overworld"}, "biomes": "minecraft:plains"}
        }))
        .unwrap();
        let ctx = |y: f64| Context {
            world: &Flat,
            this: None,
            origin: Some(Vec3::new(0.5, y, 0.5)),
            block_state: None,
            tool: None,
        };
        assert!(c.test(&ctx(0.5)));
        assert!(!c.test(&ctx(1.5)));
        let structures = Condition::parse(&json!({
            "type": "minecraft:location_check",
            "predicate": {"structures": "minecraft:fortress"}
        }))
        .unwrap();
        assert!(!structures.test(&ctx(-3.0)));
        // The inversion of a condition that is not evaluated never passes either.
        let inverted = Condition::parse(&json!({"type": "minecraft:inverted", "term": {"type": "minecraft:random_chance", "chance": 1.0}})).unwrap();
        assert!(!inverted.test(&ctx(0.0)));
        let inverted = Condition::parse(&json!({"type": "minecraft:inverted", "term": {"type": "minecraft:location_check", "predicate": {"block": {"blocks": "minecraft:dirt"}}}})).unwrap();
        assert!(inverted.test(&ctx(0.5)));
    }

    #[test]
    fn item_predicates() {
        let stack = ItemStack::new(registry::ITEM.id("minecraft:crafting_table").unwrap(), 2);
        let p = ItemPredicate::parse(
            &json!({"items": "minecraft:crafting_table", "count": {"min": 2}}),
        )
        .unwrap();
        assert!(p.test(&stack, &Flat));
        let p = ItemPredicate::parse(&json!({"items": ["minecraft:stone"]})).unwrap();
        assert!(!p.test(&stack, &Flat));
        let p = ItemPredicate::parse(&json!({"components": {"minecraft:damage": 0}})).unwrap();
        assert!(!p.test(&stack, &Flat));
        assert!(ItemPredicate::parse(&json!({"items": "minecraft:nope"})).is_err());
    }

    /// `EntityPredicate` `nbt`, `entity_tags`, `components`/`predicates` and lightning
    /// `entity_struck`; item `potion_contents.effects` and `container` collection predicates
    /// (codecs of `EntitySubPredicates`, `PotionsPredicate`, `CollectionPredicate`).
    #[test]
    fn entity_data_and_collection_sub_predicates() {
        use vanillite_nbt::{Compound, Tag};
        let p = EntityPredicate::parse(&json!({
            "nbt": "{data:{quest:1},Tags:[\"a\"]}",
            "entity_tags": {"all_of": ["a"], "none_of": ["b"]},
            "predicates": {"minecraft:custom_data": {"quest": 1}},
            "components": {"minecraft:custom_data": {"quest": 1}}
        }))
        .unwrap();
        assert!(p.is_supported() && entity_nbt_wanted());
        let mut v = EntityView::new("minecraft:pig", Vec3::ZERO);
        let mut data = Compound::new();
        data.insert("quest", Tag::Int(1));
        let mut nbt = Compound::new();
        nbt.insert("data", Tag::Compound(data));
        nbt.insert(
            "Tags",
            Tag::List(vanillite_nbt::List(vec![Tag::String("a".into())])),
        );
        v.nbt = Some(Box::new(nbt));
        v.tags = vec!["a".into()];
        assert!(p.matches(&v, &Flat, None));
        v.tags.push("b".into());
        assert!(!p.matches(&v, &Flat, None));
        v.tags.pop();
        v.nbt = Some(Box::new(Compound::new()));
        assert!(!p.matches(&v, &Flat, None));
        // `custom_data` of an entity without data is `CustomData.EMPTY`.
        let empty = EntityPredicate::parse(&json!({"components": {"custom_data": {}}})).unwrap();
        assert!(empty.matches(&v, &Flat, None));

        let bolt = EntityPredicate::parse(&json!({
            "type_specific/lightning": {"entity_struck": {"entity_type": "minecraft:pig"}}
        }))
        .unwrap();
        assert!(bolt.is_supported());
        let mut b = EntityView::new("minecraft:lightning_bolt", Vec3::ZERO);
        b.lightning_fires = Some(0);
        assert!(!bolt.matches(&b, &Flat, None));
        b.struck.push(EntityView::new("minecraft:pig", Vec3::ZERO));
        assert!(bolt.matches(&b, &Flat, None));
        assert!(!bolt.matches(&v, &Flat, None));

        let potion = registry::POTION.id("minecraft:strong_swiftness").unwrap();
        let mut stack = ItemStack::new(registry::ITEM.id("minecraft:potion").unwrap(), 1);
        stack.set_component(
            vanillite_protocol::components::ids::POTION_CONTENTS,
            crate::effect::potion::PotionContents::of(potion).encode(),
        );
        let fx = |amp: i32, size: i32| {
            ItemPredicate::parse(&json!({"predicates": {"potion_contents": {"effects": {
                "contains": [{"minecraft:speed": {"amplifier": amp}}], "size": size
            }}}}))
            .unwrap()
        };
        assert!(fx(1, 1).is_supported());
        assert!(fx(1, 1).test(&stack, &Flat));
        assert!(!fx(0, 1).test(&stack, &Flat));
        assert!(!fx(1, 2).test(&stack, &Flat));

        let mut chest = ItemStack::new(registry::ITEM.id("minecraft:chest").unwrap(), 1);
        let items = [
            ItemStack::new(registry::ITEM.id("minecraft:stone").unwrap(), 3),
            ItemStack::EMPTY,
            ItemStack::new(registry::ITEM.id("minecraft:stone").unwrap(), 1),
        ];
        chest.set_component(
            vanillite_protocol::components::ids::CONTAINER,
            vanillite_protocol::item::encode_container_contents(&items).unwrap(),
        );
        let c = |count: i32, size: i32| {
            ItemPredicate::parse(&json!({"predicates": {"container": {"items": {
                "count": [{"test": {"items": "minecraft:stone"}, "count": count}], "size": size
            }}}}))
            .unwrap()
        };
        assert!(c(2, 2).test(&chest, &Flat));
        assert!(!c(1, 2).test(&chest, &Flat));
        assert!(!c(2, 3).test(&chest, &Flat));
    }

    /// `BlockPredicate` `components` and `predicates` against `collectComponents`.
    #[test]
    fn block_component_matchers() {
        struct Named;
        impl World for Named {
            fn dimension(&self) -> &str {
                "minecraft:overworld"
            }
            fn block(&self, pos: BlockPos) -> Option<BlockState> {
                Flat.block(pos)
            }
            fn biome(&self, _pos: BlockPos) -> Option<String> {
                None
            }
            fn in_tag(&self, _registry: &str, _tag: &str, _id: &str) -> bool {
                false
            }
            fn block_components(&self, pos: BlockPos) -> Option<Vec<(u16, Box<[u8]>)>> {
                use vanillite_protocol::components::ids::CUSTOM_DATA;
                let mut c = vanillite_nbt::Compound::new();
                c.insert("k", vanillite_nbt::Tag::Int(2));
                let v = crate::item_nbt::component_from_nbt(
                    CUSTOM_DATA,
                    &vanillite_nbt::Tag::Compound(c),
                )?;
                (pos.y == -1).then(|| vec![(CUSTOM_DATA, v)])
            }
        }
        let parse = |v: Value| BlockPredicate::parse(object(&v).unwrap()).unwrap();
        let exact = parse(json!({"components": {"minecraft:custom_data": {"k": 2}}}));
        let partial = parse(json!({"predicates": {"minecraft:custom_data": {"k": 2}}}));
        let absent = parse(json!({"predicates": {"minecraft:damage": {}}}));
        let at = BlockPos::new(0, -1, 0);
        assert!(exact.supported && exact.matches(&Named, at));
        assert!(partial.matches(&Named, at));
        assert!(!absent.matches(&Named, at));
        // No block entity.
        assert!(!partial.matches(&Named, BlockPos::new(0, -2, 0)));
        assert!(!partial.matches_state(Flat.block(at).unwrap(), &Named));
    }

    /// `EntityFlagsPredicate`, `EntityEquipmentPredicate` and `SteppingOnPredicate` on living and
    /// other entities (A-18 of #272).
    #[test]
    fn flags_equipment_and_stepping_on_by_entity_kind() {
        let flags =
            EntityPredicate::parse(&json!({"flags": {"is_baby": true, "is_flying": false}}))
                .unwrap();
        let mut boat = EntityView::new("minecraft:oak_boat", Vec3::new(0.5, 0.0, 0.5));
        // Not living: `is_baby` is not tested, and it never flies.
        assert!(flags.matches(&boat, &Flat, None));
        boat.flying = true;
        assert!(flags.matches(&boat, &Flat, None));
        let mut player = EntityView::new("minecraft:player", Vec3::ZERO);
        player.living = true;
        player.baby = true;
        player.flying = true;
        assert!(!flags.matches(&player, &Flat, None));
        let equipment =
            EntityPredicate::parse(&json!({"equipment": {"mainhand": {"count": 0}}})).unwrap();
        assert!(equipment.matches(&player, &Flat, None));
        assert!(!equipment.matches(&boat, &Flat, None));
        let stepping = EntityPredicate::parse(
            &json!({"stepping_on": {"block": {"blocks": "minecraft:stone"}}}),
        )
        .unwrap();
        assert!(!stepping.matches(&player, &Flat, None));
        player.on_ground = true;
        assert!(stepping.matches(&player, &Flat, None));
    }

    /// The item sub-predicates `fireworks`, `firework_explosion`, `writable_book_content` and
    /// `attribute_modifiers` (the rest of A-05 of #272).
    #[test]
    fn firework_book_and_modifier_sub_predicates() {
        use vanillite_protocol::components::ids;
        let with = |item: &str, t: u16, snbt: &str| {
            let mut stack = ItemStack::new(registry::ITEM.id(item).unwrap(), 1);
            let tag = vanillite_nbt::snbt::parse(snbt).unwrap();
            stack.set_component(t, crate::item_nbt::component_from_nbt(t, &tag).unwrap());
            stack
        };
        let rocket = with(
            "minecraft:firework_rocket",
            ids::FIREWORKS,
            "{flight_duration:2b,explosions:[{shape:\"star\",has_trail:1b}]}",
        );
        let p = |v: Value| ItemPredicate::parse(&json!({"predicates": v})).unwrap();
        let fw = p(
            json!({"fireworks": {"flight_duration": 2, "explosions": {"contains": [{"shape": "star", "has_trail": true}]}}}),
        );
        assert!(fw.is_supported() && fw.test(&rocket, &Flat));
        assert!(!p(json!({"fireworks": {"flight_duration": 1}})).test(&rocket, &Flat));
        assert!(
            !p(json!({"fireworks": {"explosions": {"contains": [{"has_twinkle": true}]}}}))
                .test(&rocket, &Flat)
        );
        let book = with(
            "minecraft:writable_book",
            ids::WRITABLE_BOOK_CONTENT,
            "{pages:[\"hello\",{raw:\"world\"}]}",
        );
        assert!(p(json!({"writable_book_content": {"pages": {"contains": [{"contents": "world"}], "size": 2}}})).test(&book, &Flat));
        assert!(
            !p(json!({"writable_book_content": {"pages": {"contains": [{"contents": "nope"}]}}}))
                .test(&book, &Flat)
        );
        let sword = with(
            "minecraft:stick",
            ids::ATTRIBUTE_MODIFIERS,
            "[{type:\"minecraft:attack_damage\",id:\"minecraft:test\",amount:3.0d,operation:\"add_value\",slot:\"mainhand\"}]",
        );
        let m = |v: Value| p(json!({"attribute_modifiers": {"modifiers": {"contains": [v]}}}));
        assert!(m(json!({"attribute": "minecraft:attack_damage", "amount": {"min": 2.0}, "slot": "mainhand"})).test(&sword, &Flat));
        assert!(!m(json!({"operation": "add_multiplied_base"})).test(&sword, &Flat));
        assert!(!m(json!({"id": "other"})).test(&sword, &Flat));
    }
}
