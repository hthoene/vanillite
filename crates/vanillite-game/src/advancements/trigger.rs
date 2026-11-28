//! Criterion triggers (`CriteriaTriggers`, `SimpleCriterionTrigger`): the trigger types, the
//! conditions of their instances, and the events the game fires them with.

use serde_json::{Map, Value};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::BlockState;

use super::predicate::{
    Bounds, Condition, Context, DamageEvent, DamagePredicate, DamageSourcePredicate, DamageView,
    DistancePredicate, EntityView, IdSet, ItemPredicate, LocationPredicate, MobEffectsPredicate,
    Res, StateMatcher, World, block_matches, object, qualify, string,
};
use crate::inventory::PlayerInventory;
use crate::item::ItemStack;

/// The trigger types in `CriteriaTriggers` registration order.
pub const TRIGGERS: [&str; 58] = [
    "impossible",
    "player_killed_entity",
    "entity_killed_player",
    "enter_block",
    "inventory_changed",
    "recipe_unlocked",
    "player_hurt_entity",
    "entity_hurt_player",
    "enchanted_item",
    "filled_bucket",
    "brewed_potion",
    "construct_beacon",
    "used_ender_eye",
    "summoned_entity",
    "bred_animals",
    "location",
    "slept_in_bed",
    "cured_zombie_villager",
    "villager_trade",
    "item_durability_changed",
    "levitation",
    "changed_dimension",
    "tick",
    "tame_animal",
    "placed_block",
    "consume_item",
    "effects_changed",
    "used_totem",
    "nether_travel",
    "fishing_rod_hooked",
    "channeled_lightning",
    "shot_crossbow",
    "spear_mobs",
    "killed_by_arrow",
    "hero_of_the_village",
    "voluntary_exile",
    "slide_down_block",
    "bee_nest_destroyed",
    "target_hit",
    "item_used_on_block",
    "default_block_use",
    "any_block_use",
    "player_generates_container_loot",
    "thrown_item_picked_up_by_entity",
    "thrown_item_picked_up_by_player",
    "player_interacted_with_entity",
    "player_sheared_equipment",
    "started_riding",
    "lightning_strike",
    "using_item",
    "fall_from_height",
    "ride_entity_in_lava",
    "kill_mob_near_sculk_catalyst",
    "allay_drop_item_on_block",
    "avoid_vibration",
    "recipe_crafted",
    "crafter_recipe_crafted",
    "fall_after_explosion",
];

/// A trigger type: its index in [`TRIGGERS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TriggerKind(pub u8);

impl TriggerKind {
    pub const IMPOSSIBLE: TriggerKind = TriggerKind(0);
    pub const PLAYER_KILLED_ENTITY: TriggerKind = TriggerKind(1);
    pub const ENTITY_KILLED_PLAYER: TriggerKind = TriggerKind(2);
    pub const ENTER_BLOCK: TriggerKind = TriggerKind(3);
    pub const INVENTORY_CHANGED: TriggerKind = TriggerKind(4);
    pub const RECIPE_UNLOCKED: TriggerKind = TriggerKind(5);
    pub const PLAYER_HURT_ENTITY: TriggerKind = TriggerKind(6);
    pub const ENTITY_HURT_PLAYER: TriggerKind = TriggerKind(7);
    pub const ENCHANTED_ITEM: TriggerKind = TriggerKind(8);
    pub const FILLED_BUCKET: TriggerKind = TriggerKind(9);
    pub const BREWED_POTION: TriggerKind = TriggerKind(10);
    pub const CONSTRUCT_BEACON: TriggerKind = TriggerKind(11);
    pub const USED_ENDER_EYE: TriggerKind = TriggerKind(12);
    pub const SUMMONED_ENTITY: TriggerKind = TriggerKind(13);
    pub const BRED_ANIMALS: TriggerKind = TriggerKind(14);
    pub const LOCATION: TriggerKind = TriggerKind(15);
    pub const SLEPT_IN_BED: TriggerKind = TriggerKind(16);
    pub const CURED_ZOMBIE_VILLAGER: TriggerKind = TriggerKind(17);
    pub const VILLAGER_TRADE: TriggerKind = TriggerKind(18);
    pub const ITEM_DURABILITY_CHANGED: TriggerKind = TriggerKind(19);
    pub const LEVITATION: TriggerKind = TriggerKind(20);
    pub const CHANGED_DIMENSION: TriggerKind = TriggerKind(21);
    pub const TICK: TriggerKind = TriggerKind(22);
    pub const TAME_ANIMAL: TriggerKind = TriggerKind(23);
    pub const PLACED_BLOCK: TriggerKind = TriggerKind(24);
    pub const CONSUME_ITEM: TriggerKind = TriggerKind(25);
    pub const EFFECTS_CHANGED: TriggerKind = TriggerKind(26);
    pub const USED_TOTEM: TriggerKind = TriggerKind(27);
    pub const NETHER_TRAVEL: TriggerKind = TriggerKind(28);
    pub const FISHING_ROD_HOOKED: TriggerKind = TriggerKind(29);
    pub const CHANNELED_LIGHTNING: TriggerKind = TriggerKind(30);
    pub const SHOT_CROSSBOW: TriggerKind = TriggerKind(31);
    pub const SPEAR_MOBS: TriggerKind = TriggerKind(32);
    pub const KILLED_BY_ARROW: TriggerKind = TriggerKind(33);
    pub const HERO_OF_THE_VILLAGE: TriggerKind = TriggerKind(34);
    pub const VOLUNTARY_EXILE: TriggerKind = TriggerKind(35);
    pub const SLIDE_DOWN_BLOCK: TriggerKind = TriggerKind(36);
    pub const BEE_NEST_DESTROYED: TriggerKind = TriggerKind(37);
    pub const TARGET_HIT: TriggerKind = TriggerKind(38);
    pub const ITEM_USED_ON_BLOCK: TriggerKind = TriggerKind(39);
    pub const DEFAULT_BLOCK_USE: TriggerKind = TriggerKind(40);
    pub const ANY_BLOCK_USE: TriggerKind = TriggerKind(41);
    pub const PLAYER_GENERATES_CONTAINER_LOOT: TriggerKind = TriggerKind(42);
    pub const THROWN_ITEM_PICKED_UP_BY_ENTITY: TriggerKind = TriggerKind(43);
    pub const THROWN_ITEM_PICKED_UP_BY_PLAYER: TriggerKind = TriggerKind(44);
    pub const PLAYER_INTERACTED_WITH_ENTITY: TriggerKind = TriggerKind(45);
    pub const PLAYER_SHEARED_EQUIPMENT: TriggerKind = TriggerKind(46);
    pub const STARTED_RIDING: TriggerKind = TriggerKind(47);
    pub const LIGHTNING_STRIKE: TriggerKind = TriggerKind(48);
    pub const USING_ITEM: TriggerKind = TriggerKind(49);
    pub const FALL_FROM_HEIGHT: TriggerKind = TriggerKind(50);
    pub const RIDE_ENTITY_IN_LAVA: TriggerKind = TriggerKind(51);
    pub const KILL_MOB_NEAR_SCULK_CATALYST: TriggerKind = TriggerKind(52);
    pub const ALLAY_DROP_ITEM_ON_BLOCK: TriggerKind = TriggerKind(53);
    pub const AVOID_VIBRATION: TriggerKind = TriggerKind(54);
    pub const RECIPE_CRAFTED: TriggerKind = TriggerKind(55);
    pub const CRAFTER_RECIPE_CRAFTED: TriggerKind = TriggerKind(56);
    pub const FALL_AFTER_EXPLOSION: TriggerKind = TriggerKind(57);

    /// By id (`minecraft:tick` or `tick`).
    pub fn by_name(name: &str) -> Option<TriggerKind> {
        let path = qualify(name);
        let path = path.strip_prefix("minecraft:")?;
        TRIGGERS
            .iter()
            .position(|t| *t == path)
            .and_then(|i| u8::try_from(i).ok())
            .map(TriggerKind)
    }

    /// The index into per-trigger tables.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// The trigger-specific conditions of a criterion (`TriggerInstance` without `player`).
#[derive(Clone, Debug, PartialEq)]
enum Data {
    /// No conditions besides `player` (`PlayerTrigger`, `StartRidingTrigger`, `impossible`).
    None,
    /// `inventory_changed`: occupied, full and empty slot bounds and the items.
    Inventory {
        slots: [Bounds; 3],
        items: Vec<ItemPredicate>,
    },
    /// `location` of the block triggers (`ItemUsedOnLocationTrigger`, ...).
    Location(Option<Box<Condition>>),
    /// `enter_block`, `slide_down_block`.
    EnterBlock {
        blocks: Option<IdSet>,
        state: Option<StateMatcher>,
    },
    /// `recipe_unlocked`, `recipe_crafted`, `crafter_recipe_crafted`.
    Recipes {
        recipes: IdSet,
        ingredients: Vec<ItemPredicate>,
    },
    /// `changed_dimension`.
    Dimension {
        from: Option<String>,
        to: Option<String>,
    },
    /// `consume_item`, `filled_bucket`, `used_totem`, `shot_crossbow`, `using_item`.
    Item(Option<ItemPredicate>),
    /// `KilledTrigger`: `player_killed_entity`, `entity_killed_player`,
    /// `kill_mob_near_sculk_catalyst`.
    Killed {
        entity: Option<Box<Condition>>,
        killing_blow: Option<DamageSourcePredicate>,
    },
    /// `player_hurt_entity`, `entity_hurt_player` (without `entity`).
    Hurt {
        damage: Option<DamagePredicate>,
        entity: Option<Box<Condition>>,
    },
    /// `enchanted_item`.
    Enchanted {
        item: Option<ItemPredicate>,
        levels: Bounds,
    },
    /// `brewed_potion`: the potion ids (an `effects` collection predicate never matches).
    Potion {
        potions: Option<IdSet>,
        supported: bool,
    },
    /// `construct_beacon` levels, `used_ender_eye` distance.
    Bounds(Bounds),
    /// `summoned_entity`, `tame_animal`: the entity.
    Entity(Option<Box<Condition>>),
    /// `bred_animals`.
    Bred {
        parent: Option<Box<Condition>>,
        partner: Option<Box<Condition>>,
        child: Option<Box<Condition>>,
    },
    /// `cured_zombie_villager`.
    Cured {
        zombie: Option<Box<Condition>>,
        villager: Option<Box<Condition>>,
    },
    /// `villager_trade`, `player_interacted_with_entity`, `player_sheared_equipment`,
    /// `thrown_item_picked_up_by_entity`, `thrown_item_picked_up_by_player`: an entity and an
    /// item.
    EntityItem {
        entity: Option<Box<Condition>>,
        item: Option<ItemPredicate>,
    },
    /// `item_durability_changed`.
    Durability {
        item: Option<ItemPredicate>,
        durability: Bounds,
        delta: Bounds,
    },
    /// `levitation`.
    Levitation {
        distance: Option<DistancePredicate>,
        duration: Bounds,
    },
    /// `effects_changed`.
    Effects {
        effects: Option<MobEffectsPredicate>,
        source: Option<Box<Condition>>,
    },
    /// `DistanceTrigger` and `fall_after_explosion` (with `cause`).
    Distance {
        start: Option<LocationPredicate>,
        distance: Option<DistancePredicate>,
        cause: Option<Box<Condition>>,
    },
    /// `fishing_rod_hooked`.
    Fishing {
        rod: Option<ItemPredicate>,
        entity: Option<Box<Condition>>,
        item: Option<ItemPredicate>,
    },
    /// `channeled_lightning`, `killed_by_arrow`.
    Victims {
        victims: Vec<Condition>,
        unique_types: Bounds,
        weapon: Option<ItemPredicate>,
    },
    /// `spear_mobs`.
    Count(Option<i64>),
    /// `bee_nest_destroyed`.
    BeeNest {
        blocks: Option<IdSet>,
        state: Option<StateMatcher>,
        item: Option<ItemPredicate>,
        bees: Bounds,
    },
    /// `target_hit`.
    Target {
        signal: Bounds,
        projectile: Option<Box<Condition>>,
    },
    /// `player_generates_container_loot`.
    Loot(IdSet),
    /// `lightning_strike`.
    Lightning {
        lightning: Option<Box<Condition>>,
        bystander: Option<Box<Condition>>,
    },
}

/// The conditions of one criterion.
#[derive(Clone, Debug, PartialEq)]
pub struct Conditions {
    player: Option<Box<Condition>>,
    data: Data,
}

/// Boxed: conditions are large and most criteria have none.
fn condition(o: &Map<String, Value>, key: &str) -> Res<Option<Box<Condition>>> {
    Ok(o.get(key).map(Condition::parse).transpose()?.map(Box::new))
}

fn item(o: &Map<String, Value>, key: &str) -> Res<Option<ItemPredicate>> {
    o.get(key).map(ItemPredicate::parse).transpose()
}

fn bounds(o: &Map<String, Value>, key: &str) -> Res<Bounds> {
    o.get(key)
        .map(Bounds::parse)
        .transpose()
        .map(Option::unwrap_or_default)
}

fn item_list(o: &Map<String, Value>, key: &str) -> Res<Vec<ItemPredicate>> {
    match o.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(ItemPredicate::parse).collect(),
        Some(other) => Err(format!("{key} is not a list: {other}")),
    }
}

fn condition_list(o: &Map<String, Value>, key: &str) -> Res<Vec<Condition>> {
    match o.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(Condition::parse).collect(),
        Some(other) => Err(format!("{key} is not a list: {other}")),
    }
}

impl Conditions {
    /// Parses the `conditions` of a criterion of `kind`.
    pub fn parse(kind: TriggerKind, v: Option<&Value>) -> Res<Conditions> {
        let empty = Map::new();
        let o = match v {
            Some(v) => object(v)?,
            None => &empty,
        };
        type K = TriggerKind;
        let data = match kind {
            K::IMPOSSIBLE
            | K::TICK
            | K::LOCATION
            | K::SLEPT_IN_BED
            | K::HERO_OF_THE_VILLAGE
            | K::VOLUNTARY_EXILE
            | K::AVOID_VIBRATION
            | K::STARTED_RIDING => Data::None,
            K::INVENTORY_CHANGED => {
                let slots = match o.get("slots") {
                    Some(s) => {
                        let s = object(s)?;
                        [
                            bounds(s, "occupied")?,
                            bounds(s, "full")?,
                            bounds(s, "empty")?,
                        ]
                    }
                    None => [Bounds::default(); 3],
                };
                Data::Inventory {
                    slots,
                    items: item_list(o, "items")?,
                }
            }
            K::PLACED_BLOCK
            | K::ITEM_USED_ON_BLOCK
            | K::DEFAULT_BLOCK_USE
            | K::ANY_BLOCK_USE
            | K::ALLAY_DROP_ITEM_ON_BLOCK => Data::Location(condition(o, "location")?),
            K::ENTER_BLOCK | K::SLIDE_DOWN_BLOCK => Data::EnterBlock {
                blocks: o.get("blocks").map(IdSet::parse).transpose()?,
                state: o.get("state").map(StateMatcher::parse).transpose()?,
            },
            K::RECIPE_UNLOCKED | K::RECIPE_CRAFTED | K::CRAFTER_RECIPE_CRAFTED => Data::Recipes {
                recipes: IdSet::parse(o.get("recipes").ok_or("missing recipes")?)?,
                ingredients: item_list(o, "ingredients")?,
            },
            K::CHANGED_DIMENSION => Data::Dimension {
                from: o.get("from").map(string).transpose()?.map(qualify),
                to: o.get("to").map(string).transpose()?.map(qualify),
            },
            K::CONSUME_ITEM
            | K::FILLED_BUCKET
            | K::USED_TOTEM
            | K::SHOT_CROSSBOW
            | K::USING_ITEM => Data::Item(item(o, "item")?),
            K::PLAYER_KILLED_ENTITY | K::ENTITY_KILLED_PLAYER | K::KILL_MOB_NEAR_SCULK_CATALYST => {
                Data::Killed {
                    entity: condition(o, "entity")?,
                    killing_blow: o
                        .get("killing_blow")
                        .map(DamageSourcePredicate::parse)
                        .transpose()?,
                }
            }
            K::PLAYER_HURT_ENTITY | K::ENTITY_HURT_PLAYER => Data::Hurt {
                damage: o.get("damage").map(DamagePredicate::parse).transpose()?,
                entity: condition(o, "entity")?,
            },
            K::ENCHANTED_ITEM => Data::Enchanted {
                item: item(o, "item")?,
                levels: bounds(o, "levels")?,
            },
            K::BREWED_POTION => match o.get("potion") {
                None => Data::Potion {
                    potions: None,
                    supported: true,
                },
                Some(p) => {
                    let p = object(p)?;
                    Data::Potion {
                        potions: p.get("potions").map(IdSet::parse).transpose()?,
                        supported: !p.contains_key("effects"),
                    }
                }
            },
            K::CONSTRUCT_BEACON => Data::Bounds(bounds(o, "level")?),
            K::USED_ENDER_EYE => Data::Bounds(bounds(o, "distance")?),
            K::SUMMONED_ENTITY | K::TAME_ANIMAL => Data::Entity(condition(o, "entity")?),
            K::BRED_ANIMALS => Data::Bred {
                parent: condition(o, "parent")?,
                partner: condition(o, "partner")?,
                child: condition(o, "child")?,
            },
            K::CURED_ZOMBIE_VILLAGER => Data::Cured {
                zombie: condition(o, "zombie")?,
                villager: condition(o, "villager")?,
            },
            K::VILLAGER_TRADE => Data::EntityItem {
                entity: condition(o, "villager")?,
                item: item(o, "item")?,
            },
            K::PLAYER_INTERACTED_WITH_ENTITY
            | K::PLAYER_SHEARED_EQUIPMENT
            | K::THROWN_ITEM_PICKED_UP_BY_ENTITY
            | K::THROWN_ITEM_PICKED_UP_BY_PLAYER => Data::EntityItem {
                entity: condition(o, "entity")?,
                item: item(o, "item")?,
            },
            K::ITEM_DURABILITY_CHANGED => Data::Durability {
                item: item(o, "item")?,
                durability: bounds(o, "durability")?,
                delta: bounds(o, "delta")?,
            },
            K::LEVITATION => Data::Levitation {
                distance: o
                    .get("distance")
                    .map(DistancePredicate::parse)
                    .transpose()?,
                duration: bounds(o, "duration")?,
            },
            K::EFFECTS_CHANGED => Data::Effects {
                effects: o
                    .get("effects")
                    .map(MobEffectsPredicate::parse)
                    .transpose()?,
                source: condition(o, "source")?,
            },
            K::NETHER_TRAVEL
            | K::FALL_FROM_HEIGHT
            | K::RIDE_ENTITY_IN_LAVA
            | K::FALL_AFTER_EXPLOSION => Data::Distance {
                start: o
                    .get("start_position")
                    .map(LocationPredicate::parse)
                    .transpose()?,
                distance: o
                    .get("distance")
                    .map(DistancePredicate::parse)
                    .transpose()?,
                cause: condition(o, "cause")?,
            },
            K::FISHING_ROD_HOOKED => Data::Fishing {
                rod: item(o, "rod")?,
                entity: condition(o, "entity")?,
                item: item(o, "item")?,
            },
            K::CHANNELED_LIGHTNING | K::KILLED_BY_ARROW => Data::Victims {
                victims: condition_list(o, "victims")?,
                unique_types: bounds(o, "unique_entity_types")?,
                weapon: item(o, "fired_from_weapon")?,
            },
            K::SPEAR_MOBS => Data::Count(o.get("count").and_then(Value::as_i64)),
            K::BEE_NEST_DESTROYED => Data::BeeNest {
                blocks: o.get("blocks").map(IdSet::parse).transpose()?,
                state: o.get("state").map(StateMatcher::parse).transpose()?,
                item: item(o, "item")?,
                bees: bounds(o, "num_bees_inside")?,
            },
            K::TARGET_HIT => Data::Target {
                signal: bounds(o, "signal_strength")?,
                projectile: condition(o, "projectile")?,
            },
            K::PLAYER_GENERATES_CONTAINER_LOOT => Data::Loot(IdSet::parse(
                o.get("loot_tables").ok_or("missing loot_tables")?,
            )?),
            K::LIGHTNING_STRIKE => Data::Lightning {
                lightning: condition(o, "lightning")?,
                bystander: condition(o, "bystander")?,
            },
            _ => return Err(format!("unknown trigger {}", kind.0)),
        };
        Ok(Conditions {
            player: condition(o, "player")?,
            data,
        })
    }
}

/// An entity of an event (`EntityPredicate.createContext(player, entity)`), as it was then.
pub type Snapshot = Box<EntityView<'static>>;

/// A trigger with its data (the arguments of `CriterionTrigger.trigger`).
#[derive(Clone, Debug)]
pub enum Event {
    /// `tick`: every player tick.
    Tick,
    /// `location`: every 20 ticks.
    Location,
    /// A `PlayerTrigger` or `started_riding` without data: `slept_in_bed`,
    /// `hero_of_the_village`, `voluntary_exile`, `avoid_vibration`, `started_riding`.
    Player(TriggerKind),
    /// `inventory_changed` with the stack of the changed slot.
    InventoryChanged(ItemStack),
    /// `placed_block` at `pos` with the used stack.
    PlacedBlock(BlockPos, ItemStack),
    /// `item_used_on_block` at `pos` with the stack before its use.
    ItemUsedOnBlock(BlockPos, ItemStack),
    /// `allay_drop_item_on_block` at `pos` with the dropped stack.
    AllayDropItemOnBlock(BlockPos, ItemStack),
    /// `default_block_use` at `pos`.
    DefaultBlockUse(BlockPos),
    /// `any_block_use` at `pos` with the held stack.
    AnyBlockUse(BlockPos, ItemStack),
    /// `enter_block` with the state the player is inside.
    EnterBlock(BlockState),
    /// `slide_down_block` with the honey block state.
    SlideDownBlock(BlockState),
    /// `recipe_crafted` with the recipe id and the used ingredients.
    RecipeCrafted(String, Vec<ItemStack>),
    /// `crafter_recipe_crafted` with the recipe id and the used ingredients.
    CrafterRecipeCrafted(String, Vec<ItemStack>),
    /// `recipe_unlocked` with the recipe id.
    RecipeUnlocked(String),
    /// `changed_dimension` from, to.
    ChangedDimension(String, String),
    /// `consume_item`, `filled_bucket`, `used_totem`, `shot_crossbow`, `using_item` with the
    /// stack.
    Item(TriggerKind, ItemStack),
    /// `KilledTrigger` (`player_killed_entity`, `entity_killed_player`,
    /// `kill_mob_near_sculk_catalyst`): the other entity and the killing blow.
    Killed(TriggerKind, Snapshot, Box<DamageView>),
    /// `player_hurt_entity`: the victim and the damage.
    PlayerHurtEntity(Snapshot, Box<DamageEvent>),
    /// `entity_hurt_player`: the damage.
    EntityHurtPlayer(Box<DamageEvent>),
    /// `enchanted_item`: the stack and the spent levels.
    EnchantedItem(ItemStack, i32),
    /// `brewed_potion`: the potion id of the taken stack (`None` without one).
    BrewedPotion(Option<String>),
    /// `construct_beacon` with the beacon levels.
    ConstructBeacon(i32),
    /// `used_ender_eye` with the squared horizontal distance to the found structure.
    UsedEnderEye(f64),
    /// `summoned_entity`, `tame_animal`: the entity.
    Entity(TriggerKind, Snapshot),
    /// `bred_animals`: parent, partner and the child (if any).
    BredAnimals(Snapshot, Snapshot, Option<Snapshot>),
    /// `cured_zombie_villager`: the zombie and the villager.
    CuredZombieVillager(Snapshot, Snapshot),
    /// `villager_trade`, `player_interacted_with_entity`, `player_sheared_equipment`,
    /// `thrown_item_picked_up_by_entity`, `thrown_item_picked_up_by_player`: the entity (none
    /// for an item picked up by nobody known) and the stack.
    EntityItem(TriggerKind, Option<Snapshot>, ItemStack),
    /// `item_durability_changed`: the stack (before) and the new damage value.
    ItemDurabilityChanged(ItemStack, i32),
    /// `levitation`: where it started and how long it lasts.
    Levitation(Vec3, i32),
    /// `effects_changed` with the entity that caused the change.
    EffectsChanged(Option<Snapshot>),
    /// `nether_travel`, `fall_from_height`, `ride_entity_in_lava`: the start position.
    Distance(TriggerKind, Vec3),
    /// `fall_after_explosion`: the start position and the cause.
    FallAfterExplosion(Vec3, Option<Snapshot>),
    /// `fishing_rod_hooked`: the rod, the hooked entity (or the hook), the stack of a hooked
    /// item entity and the caught items.
    FishingRodHooked(ItemStack, Snapshot, Option<ItemStack>, Vec<ItemStack>),
    /// `channeled_lightning`: the struck entities.
    ChanneledLightning(Vec<EntityView<'static>>),
    /// `killed_by_arrow`: the victims, the number of their types and the weapon.
    KilledByArrow(Vec<EntityView<'static>>, i32, Option<ItemStack>),
    /// `spear_mobs`: how many mobs a spear hit.
    SpearMobs(i32),
    /// `bee_nest_destroyed`: the block, the tool and the bees inside.
    BeeNestDestroyed(BlockState, ItemStack, i32),
    /// `target_hit`: the projectile and the signal strength.
    TargetHit(Snapshot, i32),
    /// `player_generates_container_loot` with the loot table id.
    GenerateLoot(String),
    /// `lightning_strike`: the bolt and the entities around it.
    LightningStrike(Snapshot, Vec<EntityView<'static>>),
}

impl Event {
    /// The trigger type of the event.
    pub fn kind(&self) -> TriggerKind {
        type K = TriggerKind;
        match self {
            Event::Tick => K::TICK,
            Event::Location => K::LOCATION,
            Event::Player(k)
            | Event::Item(k, _)
            | Event::Killed(k, ..)
            | Event::Entity(k, _)
            | Event::EntityItem(k, ..)
            | Event::Distance(k, _) => *k,
            Event::InventoryChanged(_) => K::INVENTORY_CHANGED,
            Event::PlacedBlock(..) => K::PLACED_BLOCK,
            Event::ItemUsedOnBlock(..) => K::ITEM_USED_ON_BLOCK,
            Event::AllayDropItemOnBlock(..) => K::ALLAY_DROP_ITEM_ON_BLOCK,
            Event::DefaultBlockUse(_) => K::DEFAULT_BLOCK_USE,
            Event::AnyBlockUse(..) => K::ANY_BLOCK_USE,
            Event::EnterBlock(_) => K::ENTER_BLOCK,
            Event::SlideDownBlock(_) => K::SLIDE_DOWN_BLOCK,
            Event::RecipeCrafted(..) => K::RECIPE_CRAFTED,
            Event::CrafterRecipeCrafted(..) => K::CRAFTER_RECIPE_CRAFTED,
            Event::RecipeUnlocked(_) => K::RECIPE_UNLOCKED,
            Event::ChangedDimension(..) => K::CHANGED_DIMENSION,
            Event::PlayerHurtEntity(..) => K::PLAYER_HURT_ENTITY,
            Event::EntityHurtPlayer(_) => K::ENTITY_HURT_PLAYER,
            Event::EnchantedItem(..) => K::ENCHANTED_ITEM,
            Event::BrewedPotion(_) => K::BREWED_POTION,
            Event::ConstructBeacon(_) => K::CONSTRUCT_BEACON,
            Event::UsedEnderEye(_) => K::USED_ENDER_EYE,
            Event::BredAnimals(..) => K::BRED_ANIMALS,
            Event::CuredZombieVillager(..) => K::CURED_ZOMBIE_VILLAGER,
            Event::ItemDurabilityChanged(..) => K::ITEM_DURABILITY_CHANGED,
            Event::Levitation(..) => K::LEVITATION,
            Event::EffectsChanged(_) => K::EFFECTS_CHANGED,
            Event::FallAfterExplosion(..) => K::FALL_AFTER_EXPLOSION,
            Event::FishingRodHooked(..) => K::FISHING_ROD_HOOKED,
            Event::ChanneledLightning(_) => K::CHANNELED_LIGHTNING,
            Event::KilledByArrow(..) => K::KILLED_BY_ARROW,
            Event::SpearMobs(_) => K::SPEAR_MOBS,
            Event::BeeNestDestroyed(..) => K::BEE_NEST_DESTROYED,
            Event::TargetHit(..) => K::TARGET_HIT,
            Event::GenerateLoot(_) => K::PLAYER_GENERATES_CONTAINER_LOOT,
            Event::LightningStrike(..) => K::LIGHTNING_STRIKE,
        }
    }
}

/// What triggers are evaluated against: the world and the player.
pub struct TriggerContext<'a> {
    pub world: &'a dyn World,
    pub player: &'a EntityView<'a>,
    pub inventory: &'a PlayerInventory,
    /// Occupied, full and empty inventory slots, counted once.
    counts: std::cell::OnceCell<[u32; 3]>,
}

impl<'a> TriggerContext<'a> {
    /// A context over the world and the player.
    pub fn new(
        world: &'a dyn World,
        player: &'a EntityView<'a>,
        inventory: &'a PlayerInventory,
    ) -> TriggerContext<'a> {
        TriggerContext {
            world,
            player,
            inventory,
            counts: std::cell::OnceCell::new(),
        }
    }

    /// `InventoryChangeTrigger.trigger`'s slot counts.
    fn counts(&self) -> [u32; 3] {
        *self.counts.get_or_init(|| {
            let (mut occupied, mut full, mut empty) = (0u32, 0u32, 0u32);
            for stack in inventory_slots(self.inventory) {
                if stack.is_empty() {
                    empty = empty.saturating_add(1);
                } else {
                    occupied = occupied.saturating_add(1);
                    if stack.count >= crate::item::max_stack_size(stack) {
                        full = full.saturating_add(1);
                    }
                }
            }
            [occupied, full, empty]
        })
    }
}

impl<'a> TriggerContext<'a> {
    /// The same context over another view of the world.
    pub fn with_world<'b>(&self, world: &'b dyn World) -> TriggerContext<'b>
    where
        'a: 'b,
    {
        TriggerContext {
            world,
            player: self.player,
            inventory: self.inventory,
            counts: self.counts.clone(),
        }
    }
}

/// The world with the progress of the player whose triggers run ([`World::progress`]).
pub struct WithProgress<'a> {
    pub world: &'a dyn World,
    pub progress: &'a super::PlayerAdvancements,
}

impl std::fmt::Debug for WithProgress<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WithProgress").finish_non_exhaustive()
    }
}

impl World for WithProgress<'_> {
    fn dimension(&self) -> &str {
        self.world.dimension()
    }
    fn block(&self, pos: BlockPos) -> Option<BlockState> {
        self.world.block(pos)
    }
    fn biome(&self, pos: BlockPos) -> Option<String> {
        self.world.biome(pos)
    }
    fn in_tag(&self, registry: &str, tag: &str, id: &str) -> bool {
        self.world.in_tag(registry, tag, id)
    }
    fn structure_at(&self, pos: BlockPos, structures: &IdSet) -> bool {
        self.world.structure_at(pos, structures)
    }
    fn block_entity_nbt(&self, pos: BlockPos) -> Option<vanillite_nbt::Compound> {
        self.world.block_entity_nbt(pos)
    }
    fn block_components(&self, pos: BlockPos) -> Option<Vec<(u16, Box<[u8]>)>> {
        self.world.block_components(pos)
    }
    fn smokey(&self, pos: BlockPos) -> bool {
        self.world.smokey(pos)
    }
    fn light(&self, pos: BlockPos) -> i32 {
        self.world.light(pos)
    }
    fn can_see_sky(&self, pos: BlockPos) -> bool {
        self.world.can_see_sky(pos)
    }
    fn progress(&self) -> Option<&super::PlayerAdvancements> {
        Some(self.progress)
    }
    fn looking_at(&self) -> Option<&EntityView<'static>> {
        self.world.looking_at()
    }
}

impl std::fmt::Debug for TriggerContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TriggerContext")
            .field("player", &self.player)
            .finish_non_exhaustive()
    }
}

/// The slots of `Inventory` (main, armour, off hand, body, saddle) for `inventory_changed`.
fn inventory_slots(inv: &PlayerInventory) -> impl Iterator<Item = &ItemStack> {
    static EMPTY: ItemStack = ItemStack::EMPTY;
    inv.items
        .iter()
        .chain(inv.armor.iter())
        .chain(std::iter::once(&inv.offhand))
        .chain([&EMPTY, &EMPTY])
}

fn center(pos: BlockPos) -> Vec3 {
    Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    )
}

impl Conditions {
    /// The `player` condition (`EntityPredicate.createContext(player, player)`).
    fn player_matches(&self, ctx: &TriggerContext<'_>) -> bool {
        self.player.as_ref().is_none_or(|c| {
            c.test(&Context {
                world: ctx.world,
                this: Some(ctx.player),
                origin: Some(ctx.player.pos),
                block_state: None,
                tool: None,
            })
        })
    }

    /// Whether the criterion is met by `event` (`TriggerInstance.matches` and `player`).
    pub fn matches(&self, event: &Event, ctx: &TriggerContext<'_>) -> bool {
        self.data_matches(event, ctx) && self.player_matches(ctx)
    }

    fn data_matches(&self, event: &Event, ctx: &TriggerContext<'_>) -> bool {
        let world = ctx.world;
        let origin = ctx.player.pos;
        let location = |c: &Option<Box<Condition>>, pos: BlockPos, tool: Option<&ItemStack>| {
            c.as_ref().is_none_or(|c| {
                c.test(&Context {
                    world,
                    this: Some(ctx.player),
                    origin: Some(center(pos)),
                    block_state: world.block(pos),
                    tool,
                })
            })
        };
        // `EntityPredicate.createContext(player, entity)`: the entity at the player's position.
        let entity_test = |c: &Condition, e: Option<&EntityView<'_>>| {
            c.test(&Context {
                world,
                this: e,
                origin: Some(origin),
                block_state: None,
                tool: None,
            })
        };
        let entity = |c: &Option<Box<Condition>>, e: Option<&EntityView<'_>>| {
            c.as_ref().is_none_or(|c| entity_test(c, e))
        };
        let item = |p: &Option<ItemPredicate>, stack: &ItemStack| {
            p.as_ref().is_none_or(|p| p.test(stack, world))
        };
        let ingredients_match = |ingredients: &[ItemPredicate], used: &[ItemStack]| {
            let mut remaining: Vec<&ItemStack> = used.iter().collect();
            ingredients
                .iter()
                .all(|p| match remaining.iter().position(|s| p.test(s, world)) {
                    Some(i) => {
                        remaining.remove(i);
                        true
                    }
                    None => false,
                })
        };
        match (&self.data, event) {
            (Data::None, Event::Tick | Event::Location | Event::Player(_)) => true,
            (Data::Inventory { slots, items }, Event::InventoryChanged(changed)) => {
                inventory_matches(slots, items, changed, ctx)
            }
            (
                Data::Location(c),
                Event::PlacedBlock(pos, tool)
                | Event::ItemUsedOnBlock(pos, tool)
                | Event::AllayDropItemOnBlock(pos, tool)
                | Event::AnyBlockUse(pos, tool),
            ) => location(c, *pos, Some(tool)),
            (Data::Location(c), Event::DefaultBlockUse(pos)) => location(c, *pos, None),
            (
                Data::EnterBlock { blocks, state },
                Event::EnterBlock(s) | Event::SlideDownBlock(s),
            ) => block_matches(blocks.as_ref(), state.as_ref(), *s, world),
            (Data::Recipes { recipes, .. }, Event::RecipeUnlocked(id)) => {
                recipes.contains("minecraft:recipe", id, world)
            }
            (
                Data::Recipes {
                    recipes,
                    ingredients,
                },
                Event::RecipeCrafted(id, used) | Event::CrafterRecipeCrafted(id, used),
            ) => {
                recipes.contains("minecraft:recipe", id, world)
                    && ingredients_match(ingredients, used)
            }
            (Data::Dimension { from, to }, Event::ChangedDimension(f, t)) => {
                from.as_ref().is_none_or(|d| d == f) && to.as_ref().is_none_or(|d| d == t)
            }
            (Data::Item(p), Event::Item(_, stack)) => item(p, stack),
            (
                Data::Killed {
                    entity: c,
                    killing_blow,
                },
                Event::Killed(_, victim, source),
            ) => {
                killing_blow
                    .as_ref()
                    .is_none_or(|k| k.matches(source, world, origin))
                    && entity(c, Some(victim))
            }
            (Data::Hurt { damage, entity: c }, Event::PlayerHurtEntity(victim, d)) => {
                damage.as_ref().is_none_or(|p| p.matches(d, world, origin))
                    && entity(c, Some(victim))
            }
            (Data::Hurt { damage, .. }, Event::EntityHurtPlayer(d)) => {
                damage.as_ref().is_none_or(|p| p.matches(d, world, origin))
            }
            (Data::Enchanted { item: p, levels }, Event::EnchantedItem(stack, spent)) => {
                item(p, stack) && levels.matches(f64::from(*spent))
            }
            (Data::Potion { potions, supported }, Event::BrewedPotion(potion)) => {
                *supported
                    && potions.as_ref().is_none_or(|set| {
                        potion
                            .as_ref()
                            .is_some_and(|p| set.contains("minecraft:potion", p, world))
                    })
            }
            (Data::Bounds(levels), Event::ConstructBeacon(n)) => levels.matches(f64::from(*n)),
            (Data::Bounds(distance), Event::UsedEnderEye(sqr)) => distance.matches_sqr(*sqr),
            (Data::Entity(c), Event::Entity(_, e)) => entity(c, Some(e)),
            (
                Data::Bred {
                    parent,
                    partner,
                    child,
                },
                Event::BredAnimals(a, b, c),
            ) => {
                let child_ok = child
                    .as_ref()
                    .is_none_or(|p| c.as_ref().is_some_and(|c| entity_test(p, Some(c))));
                child_ok
                    && (entity(parent, Some(a)) && entity(partner, Some(b))
                        || entity(parent, Some(b)) && entity(partner, Some(a)))
            }
            (Data::Cured { zombie, villager }, Event::CuredZombieVillager(z, v)) => {
                entity(zombie, Some(z)) && entity(villager, Some(v))
            }
            (Data::EntityItem { entity: c, item: p }, Event::EntityItem(_, e, stack)) => {
                item(p, stack) && entity(c, e.as_deref())
            }
            (
                Data::Durability {
                    item: p,
                    durability,
                    delta,
                },
                Event::ItemDurabilityChanged(stack, new_damage),
            ) => {
                item(p, stack)
                    && durability.matches(f64::from(
                        crate::item::max_damage(stack).saturating_sub(*new_damage),
                    ))
                    && delta.matches(f64::from(
                        crate::item::damage(stack).saturating_sub(*new_damage),
                    ))
            }
            (Data::Levitation { distance, duration }, Event::Levitation(start, ticks)) => {
                distance.as_ref().is_none_or(|d| d.matches(*start, origin))
                    && duration.matches(f64::from(*ticks))
            }
            (Data::Effects { effects, source }, Event::EffectsChanged(cause)) => {
                effects
                    .as_ref()
                    .is_none_or(|p| p.matches(&ctx.player.effects))
                    && source
                        .as_ref()
                        .is_none_or(|c| cause.as_ref().is_some_and(|e| entity_test(c, Some(e))))
            }
            (
                Data::Distance {
                    start,
                    distance,
                    cause,
                },
                Event::Distance(_, at) | Event::FallAfterExplosion(at, _),
            ) => {
                let by = match event {
                    Event::FallAfterExplosion(_, by) => by.as_deref(),
                    _ => None,
                };
                start.as_ref().is_none_or(|l| l.matches(world, *at))
                    && distance.as_ref().is_none_or(|d| d.matches(*at, origin))
                    && cause
                        .as_ref()
                        .is_none_or(|c| by.is_some_and(|e| entity_test(c, Some(e))))
            }
            (
                Data::Fishing {
                    rod,
                    entity: c,
                    item: p,
                },
                Event::FishingRodHooked(used, hooked, hooked_item, caught),
            ) => {
                item(rod, used)
                    && entity(c, Some(hooked))
                    && p.as_ref().is_none_or(|p| {
                        hooked_item.as_ref().is_some_and(|s| p.test(s, world))
                            || caught.iter().any(|s| p.test(s, world))
                    })
            }
            (Data::Victims { victims, .. }, Event::ChanneledLightning(struck)) => victims
                .iter()
                .all(|c| struck.iter().any(|e| entity_test(c, Some(e)))),
            (
                Data::Victims {
                    victims,
                    unique_types,
                    weapon,
                },
                Event::KilledByArrow(killed, types, fired_from),
            ) => {
                weapon
                    .as_ref()
                    .is_none_or(|w| fired_from.as_ref().is_some_and(|s| w.test(s, world)))
                    && {
                        let mut left: Vec<&EntityView<'_>> = killed.iter().collect();
                        victims.iter().all(|c| {
                            match left.iter().position(|e| entity_test(c, Some(e))) {
                                Some(i) => {
                                    left.remove(i);
                                    true
                                }
                                None => false,
                            }
                        })
                    }
                    && unique_types.matches(f64::from(*types))
            }
            (Data::Count(required), Event::SpearMobs(n)) => {
                required.is_none_or(|r| i64::from(*n) >= r)
            }
            (
                Data::BeeNest {
                    blocks,
                    state,
                    item: p,
                    bees,
                },
                Event::BeeNestDestroyed(s, tool, inside),
            ) => {
                block_matches(blocks.as_ref(), state.as_ref(), *s, world)
                    && item(p, tool)
                    && bees.matches(f64::from(*inside))
            }
            (Data::Target { signal, projectile }, Event::TargetHit(p, strength)) => {
                signal.matches(f64::from(*strength)) && entity(projectile, Some(p))
            }
            (Data::Loot(tables), Event::GenerateLoot(id)) => {
                tables.contains("minecraft:loot_table", id, world)
            }
            (
                Data::Lightning {
                    lightning,
                    bystander,
                },
                Event::LightningStrike(bolt, around),
            ) => {
                entity(lightning, Some(bolt))
                    && bystander
                        .as_ref()
                        .is_none_or(|c| around.iter().any(|e| entity_test(c, Some(e))))
            }
            _ => false,
        }
    }
}

/// `InventoryChangeTrigger.TriggerInstance.matches`.
fn inventory_matches(
    slots: &[Bounds; 3],
    items: &[ItemPredicate],
    changed: &ItemStack,
    ctx: &TriggerContext<'_>,
) -> bool {
    let [occupied, full, empty] = ctx.counts();
    let [occupied_b, full_b, empty_b] = slots;
    if !(full_b.matches(f64::from(full))
        && empty_b.matches(f64::from(empty))
        && occupied_b.matches(f64::from(occupied)))
    {
        return false;
    }
    match items {
        [] => true,
        [single] => !changed.is_empty() && single.test(changed, ctx.world),
        _ => {
            let mut left: Vec<&ItemPredicate> = items.iter().collect();
            for stack in inventory_slots(ctx.inventory) {
                if left.is_empty() {
                    return true;
                }
                if !stack.is_empty() {
                    left.retain(|p| !p.test(stack, ctx.world));
                }
            }
            left.is_empty()
        }
    }
}
