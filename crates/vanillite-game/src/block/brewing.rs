//! Brewing stands (`BrewingStandBlockEntity`): slots 0–2 bottles, 3 the reagent, 4 the fuel.
//! A brew takes 400 ticks and one use of fuel (blaze powder gives 20 uses); the recipes are the
//! data driven `brewing` recipes of 26.3.

use vanillite_base::BlockPos;
use vanillite_data::block::BlockState;
use vanillite_data::item::ids as items;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::text::Component;
use vanillite_worldgen::behaviour::StateExt;

use crate::item::{self, ItemStack};
use crate::level::{BlockEntity, Level};

/// `LevelEvent.SOUND_BREWING_STAND_BREW`.
const BREW_FINISHED: i32 = 1035;
/// Uses of one blaze powder (`brewing/uses_default`).
const FUEL_USES: i32 = 20;

/// The data of a brewing stand block entity.
#[derive(Clone, Debug, PartialEq)]
pub struct BrewingStand {
    /// Bottles, reagent, fuel.
    pub items: [ItemStack; 5],
    /// `brewTime`: ticks left of the running brew.
    pub brew_time: i32,
    /// `totalBrewTime`.
    pub total_brew_time: i32,
    /// `fuel`: uses left.
    pub fuel: i32,
    /// `totalFuel`.
    pub total_fuel: i32,
    /// `speedMultiplier` of the fuel.
    pub speed: f32,
    /// The reagent item the running brew started with.
    pub ingredient: u16,
    /// `lastPotionCount`: which bottle slots were filled.
    pub last_bottles: [bool; 3],
    /// `CustomName`.
    pub custom_name: Option<Component>,
    /// `lock` (`LockCode`).
    pub lock: Option<vanillite_nbt::Tag>,
}

impl Default for BrewingStand {
    fn default() -> Self {
        BrewingStand {
            items: std::array::from_fn(|_| ItemStack::EMPTY),
            brew_time: 0,
            total_brew_time: 400,
            fuel: 0,
            total_fuel: FUEL_USES,
            speed: 1.0,
            ingredient: 0,
            last_bottles: [false; 3],
            custom_name: None,
            lock: None,
        }
    }
}

/// `PotionIngredient.isPotionInput`: a bottle a brewing stand takes.
pub fn is_potion_input(stack: &ItemStack) -> bool {
    crate::player::play::game_data().is_some_and(|d| {
        d.recipes.brewing.iter().any(|r| r.input.test(stack))
            || d.item_in_tag(stack.item, "minecraft:brewing_potion_inputs")
    })
}

/// `BrewingStandMenu.FuelSlot.mayPlaceItem`: the `brewing_fuel` component (patched, removed or
/// the item's: blaze powder).
pub fn is_fuel(stack: &ItemStack) -> bool {
    use vanillite_protocol::components::ids::BREWING_FUEL;
    match item::patched(stack, BREWING_FUEL) {
        item::Patched::Value(_) => !stack.is_empty(),
        item::Patched::Removed => false,
        item::Patched::Default => item::item_id(stack) == items::BLAZE_POWDER,
    }
}

/// `getUses` and `getSpeedMultiplier` of a fuel's `brewing_fuel`: numbers, or
/// `context_int_provider` ids (`brewing/uses_default`: 20); a speed provider id other than
/// `brewing/speed_default` counts as 1.
fn fuel_values(level: &mut Level, pos: BlockPos, stack: &ItemStack) -> (i32, f32) {
    use vanillite_protocol::components::ids::BREWING_FUEL;
    let item::Patched::Value(v) = item::patched(stack, BREWING_FUEL) else {
        return (FUEL_USES, 1.0);
    };
    let mut r = vanillite_protocol::codec::Reader::new(v);
    let uses = crate::recipe::read_resolvable(&mut r, false);
    let speed = crate::recipe::read_resolvable(&mut r, true);
    let uses = match uses {
        Some(serde_json::Value::String(id)) => {
            let provider =
                crate::player::play::game_data().and_then(|d| d.compostables.get(&id).cloned());
            match provider {
                Some(p) => {
                    let state = level.block(pos);
                    p.get(state, level.random())
                }
                None => 0,
            }
        }
        Some(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
        None => 0,
    };
    #[allow(clippy::cast_possible_truncation)]
    let speed = speed.and_then(|s| s.as_f64()).map_or(1.0, |s| s as f32);
    (uses, speed)
}

/// Whether the stack is a reagent of some brewing recipe.
pub fn is_reagent(stack: &ItemStack) -> bool {
    crate::player::play::game_data().is_some_and(|d| d.recipes.is_brewing_reagent(stack))
}

impl BrewingStand {
    /// The menu's data slots.
    pub fn data(&self, index: usize) -> i32 {
        match index {
            0 => self.brew_time,
            1 => self.fuel,
            2 => self.total_brew_time,
            3 => self.total_fuel,
            _ => 0,
        }
    }

    /// `canPlaceItem`.
    pub fn can_place_item(&self, slot: usize, stack: &ItemStack) -> bool {
        match slot {
            4 => is_fuel(stack),
            3 => is_reagent(stack),
            _ => is_potion_input(stack) && self.items.get(slot).is_some_and(ItemStack::is_empty),
        }
    }

    /// Loads the stored fields.
    pub fn load(&mut self, nbt: &Compound) {
        let int = |k: &str| {
            nbt.get(k)
                .and_then(Tag::as_i64)
                .and_then(|v| i32::try_from(v).ok())
        };
        self.lock = super::lock::load(nbt);
        self.custom_name = super::container::load_name(nbt);
        super::container::load_items(nbt, &mut self.items);
        self.brew_time = int("BrewTime").unwrap_or(0);
        self.total_brew_time = int("total_brew_time").unwrap_or(400);
        if self.brew_time > 0 {
            self.ingredient = self.items[3].item;
        }
        self.fuel = int("Fuel").unwrap_or(0);
        self.total_fuel = int("total_fuel").unwrap_or(FUEL_USES);
        self.speed = match nbt.get("speed_multiplier") {
            Some(Tag::Float(v)) => *v,
            _ => 1.0,
        };
    }

    /// Saves the stored fields.
    pub fn save(&self, out: &mut Compound) {
        super::lock::save(&self.lock, out);
        super::container::save_name(&self.custom_name, out);
        out.insert("BrewTime", Tag::Int(self.brew_time));
        out.insert("total_brew_time", Tag::Int(self.total_brew_time));
        super::container::save_items_always(&self.items, out);
        out.insert("Fuel", Tag::Int(self.fuel));
        out.insert("total_fuel", Tag::Int(self.total_fuel));
        out.insert("speed_multiplier", Tag::Float(self.speed));
    }
}

/// `BREW` before a brewing stand brews: `None` when cancelled, else the three bottle slots
/// as plugins replace them (`None`: unchanged). The results go to plugins as a list of the
/// three slots (`{}` for an empty one), and a replacement list fills them in that order.
fn brew_event(
    hooks: &dyn crate::hooks::GameHooks,
    pos: BlockPos,
    state: BlockState,
    items: &[ItemStack],
    reagent: &ItemStack,
    recipes: &crate::recipe::Recipes,
) -> Option<Option<[ItemStack; 3]>> {
    use vanillite_nbt::{List, Tag};
    let results: [ItemStack; 3] = std::array::from_fn(|slot| {
        let bottle = items.get(slot).cloned().unwrap_or_default();
        recipes
            .brewing_for(&bottle, reagent)
            .map_or(bottle, |r| r.output.clone())
    });
    let list = results
        .iter()
        .map(|s| {
            Tag::Compound(if s.is_empty() {
                vanillite_nbt::Compound::new()
            } else {
                crate::item_nbt::stack_to_nbt(s)
            })
        })
        .collect();
    let mut ev = crate::hooks::Event {
        pos,
        state: Some(state),
        item: vanillite_nbt::snbt::to_string(&Tag::List(List(list))),
        ..crate::hooks::Event::new(vanillite_abi::event::BREW)
    };
    hooks.event(crate::hooks::HookWorld::NONE, &mut ev);
    if ev.cancelled {
        return None;
    }
    let Some(Ok(Tag::List(list))) = ev.item_out.as_deref().map(vanillite_nbt::snbt::parse) else {
        return Some(None);
    };
    // A bottle slot holds one item.
    Some(Some(std::array::from_fn(|slot| match list.0.get(slot) {
        Some(Tag::Compound(c)) => {
            let stack = crate::item_nbt::stack_from_nbt(c);
            if stack.is_empty() {
                stack
            } else {
                stack.with_count(1)
            }
        }
        _ => ItemStack::EMPTY,
    })))
}

/// `BrewingStandBlockEntity.serverTick`.
pub fn server_tick(level: &mut Level, pos: BlockPos, state: BlockState) {
    let Some(data) = crate::player::play::game_data() else {
        return;
    };
    let recipes = &data.recipes;
    let hooks = level.hooks_for(vanillite_abi::event::BREW);
    let Some(BlockEntity::BrewingStand(stand)) = level.block_entity_mut(pos) else {
        return;
    };
    let mut changed = false;
    if stand.fuel <= 0 && is_fuel(&stand.items[4]) {
        let fuel = stand.items[4].clone();
        let (uses, speed) = fuel_values(level, pos, &fuel);
        let Some(BlockEntity::BrewingStand(stand)) = level.block_entity_mut(pos) else {
            return;
        };
        stand.fuel = uses;
        stand.total_fuel = uses;
        stand.speed = speed;
        stand.items[4].shrink(1);
        changed = true;
    }
    let Some(BlockEntity::BrewingStand(stand)) = level.block_entity_mut(pos) else {
        return;
    };
    let reagent = stand.items[3].clone();
    let brewable = !reagent.is_empty()
        && recipes.is_brewing_reagent(&reagent)
        && stand.items[..3]
            .iter()
            .any(|b| recipes.brewing_for(b, &reagent).is_some());
    let mut finished = false;
    let mut remainder = None;
    if stand.brew_time > 0 {
        stand.brew_time -= 1;
        // Bukkit's `BrewEvent`: plugins may cancel the brew or change its results.
        let plugin = match hooks.as_deref() {
            Some(h) if stand.brew_time == 0 && brewable => {
                brew_event(h, pos, state, &stand.items, &reagent, recipes)
            }
            _ => Some(None),
        };
        if let (0, true, Some(plugin_results)) = (stand.brew_time, brewable, plugin) {
            // `doBrew`.
            for slot in 0..3 {
                if let Some(r) = recipes.brewing_for(&stand.items[slot], &reagent) {
                    stand.items[slot] = r.output.clone();
                }
            }
            if let Some(results) = plugin_results {
                stand.items[..3].clone_from_slice(&results);
            }
            let rest = crate::recipe::crafting_remainder(reagent.item);
            stand.items[3].shrink(1);
            if let Some(r) = rest {
                if stand.items[3].is_empty() {
                    stand.items[3] = ItemStack::new(r, 1);
                } else {
                    remainder = Some(ItemStack::new(r, 1));
                }
            }
            finished = true;
        } else if !brewable || reagent.item != stand.ingredient {
            stand.brew_time = 0;
        }
        changed = true;
    } else if brewable && stand.fuel > 0 {
        let speed = if stand.speed > 0.0 { stand.speed } else { 1.0 };
        stand.fuel -= 1;
        #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
        let time = (400.0 / speed).ceil() as i32;
        stand.brew_time = time;
        stand.total_brew_time = time;
        stand.ingredient = reagent.item;
        changed = true;
    }
    let bottles = [0, 1, 2].map(|i| !stand.items[i].is_empty());
    let bottles_changed = bottles != stand.last_bottles;
    stand.last_bottles = bottles;
    if let Some(r) = remainder {
        super::container::drop_item_stack(level, pos, r);
    }
    if finished {
        level.level_event(None, BREW_FINISHED, pos, 0);
    }
    if changed {
        super::container::set_changed(level, pos);
    }
    if bottles_changed {
        let mut new = state;
        for (i, has) in bottles.into_iter().enumerate() {
            new = new.set_flag(&format!("has_bottle_{i}"), has);
        }
        level.set_block(pos, new, crate::level::update::CLIENTS);
    }
}
