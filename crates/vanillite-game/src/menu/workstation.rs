//! The workstation menus without block entity: stonecutter (`StonecutterMenu`), anvil
//! (`AnvilMenu`), grindstone (`GrindstoneMenu`) and smithing table (`SmithingMenu`). Their
//! input and result slots belong to the menu ([`ContainerMenu::local`]); every change of an
//! input slot computes the result again, like their `SimpleContainer.setChanged`.

use vanillite_base::random::RandomSource;
use vanillite_data::item::ids as items;
use vanillite_protocol::components::ids as components;
use vanillite_protocol::text::Component;

use super::container::{ContainerView, MenuKind};
use super::{ClickContext, MenuSlots};
use crate::enchant::{self, Enchanting};
use crate::inventory::Drop;
use crate::item::{self, ItemStack};
use crate::recipe::{Recipes, Smithing};

/// `Mth.clamp`-free saturating `(int)Math.min(baseCost * 2L + 1L, MAX)`.
fn increased_repair_cost(base: i32) -> i32 {
    base.saturating_mul(2).saturating_add(1)
}

fn data() -> Option<std::sync::Arc<crate::player::play::GameData>> {
    crate::player::play::game_data()
}

/// The result slot index of a workstation.
pub(super) fn result_slot(kind: MenuKind) -> Option<usize> {
    match kind {
        MenuKind::Stonecutter => Some(1),
        MenuKind::Anvil | MenuKind::Grindstone => Some(2),
        MenuKind::Smithing | MenuKind::Loom => Some(3),
        MenuKind::CartographyTable => Some(2),
        _ => None,
    }
}

/// `Slot.mayPlace` of a workstation's own slot `i`.
pub(super) fn may_place(kind: MenuKind, i: usize, stack: &ItemStack) -> bool {
    if kind == MenuKind::Enchantment {
        return i == 0 || item::item_id(stack) == items::LAPIS_LAZULI;
    }
    if result_slot(kind) == Some(i) {
        return false;
    }
    if matches!(kind, MenuKind::Loom | MenuKind::CartographyTable) {
        return ContainerView::station_may_place(kind, i, stack);
    }
    let Some(d) = data() else {
        return matches!(kind, MenuKind::Stonecutter | MenuKind::Anvil);
    };
    match kind {
        MenuKind::Grindstone => item::is_damageable(stack) || has_any_enchantments(stack),
        MenuKind::Smithing => {
            let sets = d.recipes.smithing_sets();
            match i {
                0 => sets.0.binary_search(&stack.item).is_ok(),
                1 => sets.1.binary_search(&stack.item).is_ok(),
                _ => sets.2.binary_search(&stack.item).is_ok(),
            }
        }
        _ => true,
    }
}

/// `ItemCombinerMenu.mayPickup` of the result slot.
pub(super) fn may_pickup_result(view: &ContainerView<'_>) -> bool {
    match view.menu.kind {
        MenuKind::Anvil => {
            let cost = view.menu.data.first().copied().unwrap_or(0);
            let level = view.xp.as_ref().map_or(0, |x| x.level);
            (view.menu.state.creative || level >= cost) && cost > 0
        }
        _ => true,
    }
}

/// `EnchantmentHelper.getEnchantmentsForCrafting`: the stored enchantments of books.
pub fn crafting_enchantments(stack: &ItemStack) -> Vec<(u32, i32)> {
    enchant::read(stack.component(enchantment_component(stack)))
}

fn enchantment_component(stack: &ItemStack) -> u16 {
    if item::item_id(stack) == items::ENCHANTED_BOOK {
        components::STORED_ENCHANTMENTS
    } else {
        components::ENCHANTMENTS
    }
}

/// `EnchantmentHelper.setEnchantments` / `updateEnchantments`.
pub fn set_crafting_enchantments(stack: &mut ItemStack, list: &[(u32, i32)]) {
    let id = enchantment_component(stack);
    if list.is_empty() {
        stack.patch_mut().unset(id);
    } else {
        stack.set_component(id, enchant::write(list));
    }
}

/// `EnchantmentHelper.hasAnyEnchantments`.
pub fn has_any_enchantments(stack: &ItemStack) -> bool {
    !enchant::read(stack.component(components::ENCHANTMENTS)).is_empty()
        || !enchant::read(stack.component(components::STORED_ENCHANTMENTS)).is_empty()
}

fn repair_cost(stack: &ItemStack) -> i32 {
    enchant::read_int(stack.component(components::REPAIR_COST))
}

fn set_repair_cost(stack: &mut ItemStack, cost: i32) {
    if cost == 0 {
        stack.patch_mut().unset(components::REPAIR_COST);
    } else {
        stack.set_component(components::REPAIR_COST, enchant::write_int(cost));
    }
}

/// The plain text of a stack's custom name.
fn custom_name(stack: &ItemStack) -> Option<String> {
    let bytes = stack.component(components::CUSTOM_NAME)?;
    let mut r = vanillite_protocol::codec::Reader::new(bytes);
    <Component as vanillite_protocol::codec::DecodeLimited>::decode_limited(&mut r, 2_097_152)
        .ok()
        .map(|c| c.plain())
}

fn set_custom_name(stack: &mut ItemStack, name: Option<&str>) {
    use vanillite_protocol::codec::Encode;
    match name {
        Some(name) => {
            let mut out = Vec::new();
            Component::text(name).encode(&mut out);
            stack.set_component(components::CUSTOM_NAME, out.into_boxed_slice());
        }
        None => stack.patch_mut().unset(components::CUSTOM_NAME),
    }
}

impl ContainerView<'_> {
    fn local(&self, i: usize) -> ItemStack {
        self.menu.local.get(i).cloned().unwrap_or_default()
    }

    fn set_local(&mut self, i: usize, stack: ItemStack) {
        if let Some(s) = self.menu.local.get_mut(i) {
            *s = stack;
        }
    }

    fn set_data(&mut self, i: usize, value: i32) {
        if let Some(d) = self.menu.data.get_mut(i) {
            *d = value;
        }
    }

    /// `slotsChanged` of the workstation's input container.
    pub(super) fn workstation_changed(&mut self) {
        match self.menu.kind {
            MenuKind::Stonecutter => self.stonecutter_changed(),
            MenuKind::Anvil => self.anvil_result(),
            MenuKind::Grindstone => {
                let result = grindstone_result(&self.local(0), &self.local(1));
                self.set_local(2, result);
            }
            MenuKind::Enchantment => self.enchantment_changed(),
            MenuKind::Loom => self.loom_changed(),
            MenuKind::CartographyTable => self.cartography_changed(),
            MenuKind::Smithing => {
                self.smithing_result();
                let has_error = !self.local(0).is_empty()
                    && !self.local(1).is_empty()
                    && !self.local(2).is_empty()
                    && self.local(3).is_empty();
                self.set_data(0, i32::from(has_error));
            }
            _ => {}
        }
    }

    // --- Stonecutter -----------------------------------------------------------------------

    fn stonecutter_changed(&mut self) {
        let input = self.local(0);
        let same = if input.is_empty() {
            self.menu.last_input.is_empty()
        } else {
            !self.menu.last_input.is_empty() && self.menu.last_input.item == input.item
        };
        if same {
            return;
        }
        self.menu.last_input = input.clone();
        // `setupRecipeList`.
        self.set_data(0, -1);
        self.set_local(1, ItemStack::EMPTY);
        self.menu.recipes = match data() {
            Some(d) if !input.is_empty() => d
                .recipes
                .stonecutting
                .iter()
                .enumerate()
                .filter(|(_, r)| r.ingredient.test(&input))
                .map(|(i, _)| i)
                .collect(),
            _ => Vec::new(),
        };
    }

    fn stonecutter_setup_result(&mut self, index: i32) {
        let recipe = usize::try_from(index)
            .ok()
            .and_then(|i| self.menu.recipes.get(i))
            .copied();
        let result = match (recipe, data()) {
            (Some(r), Some(d)) => d
                .recipes
                .stonecutting
                .get(r)
                .map(|r| r.result.clone())
                .unwrap_or_default(),
            _ => ItemStack::EMPTY,
        };
        self.set_local(1, result);
    }

    /// `clickMenuButton`: whether the button did something.
    pub(super) fn workstation_button(&mut self, id: i32) -> bool {
        match self.menu.kind {
            MenuKind::Enchantment => self.enchant_button(id),
            MenuKind::Loom => self.loom_button(id),
            MenuKind::Stonecutter => {
                if self.menu.data.first() == Some(&id) {
                    return false;
                }
                let valid = usize::try_from(id).is_ok_and(|i| i < self.menu.recipes.len());
                if valid {
                    self.set_data(0, id);
                    self.stonecutter_setup_result(id);
                }
                true
            }
            _ => false,
        }
    }

    // --- Anvil -----------------------------------------------------------------------------

    /// `AnvilMenu.setItemName`.
    pub fn rename(&mut self, name: &str) -> bool {
        if self.menu.kind != MenuKind::Anvil {
            return false;
        }
        // `StringUtil.filterText`: no section signs or control characters; at most 50.
        let filtered: String = name
            .chars()
            .filter(|&c| c != '\u{a7}' && c >= ' ' && c != '\u{7f}')
            .collect();
        if filtered.chars().count() > 50 || self.menu.item_name.as_deref() == Some(&filtered) {
            return false;
        }
        let blank = filtered.trim().is_empty();
        let mut result = self.local(2);
        if !result.is_empty() {
            set_custom_name(&mut result, (!blank).then_some(filtered.as_str()));
            self.set_local(2, result);
        }
        self.menu.item_name = Some(filtered);
        self.anvil_result();
        true
    }

    /// `AnvilMenu.createResult` with `ANVIL_PREPARE` (Bukkit's `PrepareAnvilEvent`): plugins
    /// may replace the result and change the level cost.
    fn anvil_result(&mut self) {
        self.anvil_create_result();
        if self.menu.plugin {
            return;
        }
        let Some(hooks) = self.level.hooks_for(vanillite_abi::event::ANVIL_PREPARE) else {
            return;
        };
        /// The highest level cost a plugin may set.
        const MAX_COST: i64 = 32767;
        let mut ev = crate::hooks::Event {
            entity: self.menu.player,
            item: crate::hooks::item_snbt(&self.local(2)),
            value: i64::from(self.menu.data.first().copied().unwrap_or(0)),
            text: self.menu.item_name.clone().unwrap_or_default(),
            ..crate::hooks::Event::new(vanillite_abi::event::ANVIL_PREPARE)
        };
        hooks.event(crate::hooks::HookWorld::NONE, &mut ev);
        if let Some(result) = ev.item_out.as_deref().and_then(crate::hooks::parse_item) {
            self.set_local(2, result);
        }
        self.set_data(0, i32::try_from(ev.value.clamp(0, MAX_COST)).unwrap_or(0));
    }

    /// `AnvilMenu.createResult`.
    fn anvil_create_result(&mut self) {
        let Some(d) = data() else {
            return;
        };
        let e = &d.enchanting;
        let input = self.local(0);
        self.menu.only_renaming = false;
        self.set_data(0, 1);
        if input.is_empty() {
            self.set_local(2, ItemStack::EMPTY);
            self.set_data(0, 0);
            return;
        }
        let infinite = self.menu.state.creative;
        let mut result = input.clone();
        let addition = self.local(1);
        let mut enchantments = crafting_enchantments(&result);
        let tax = i64::from(repair_cost(&input)) + i64::from(repair_cost(&addition));
        let mut price: i32 = 0;
        let mut naming_cost = 0;
        self.menu.repair_item_count_cost = 0;
        let fail = |view: &mut ContainerView<'_>| {
            view.set_local(2, ItemStack::EMPTY);
            view.set_data(0, 0);
        };
        if !addition.is_empty() {
            let using_book = addition
                .component(components::STORED_ENCHANTMENTS)
                .is_some();
            if item::is_damageable(&result) && e.is_valid_repair_item(&input, &addition) {
                let mut repair = item::damage(&result).min(item::max_damage(&result) / 4);
                if repair <= 0 {
                    fail(self);
                    return;
                }
                let mut count = 0;
                while repair > 0 && count < addition.count {
                    let new_damage = item::damage(&result) - repair;
                    item::set_damage(&mut result, new_damage);
                    price += 1;
                    repair = item::damage(&result).min(item::max_damage(&result) / 4);
                    count += 1;
                }
                self.menu.repair_item_count_cost = count;
            } else {
                if !using_book && (result.item != addition.item || !item::is_damageable(&result)) {
                    fail(self);
                    return;
                }
                if item::is_damageable(&result) && !using_book {
                    let remaining1 = item::max_damage(&input) - item::damage(&input);
                    let remaining2 = item::max_damage(&addition) - item::damage(&addition);
                    let additional = remaining2 + item::max_damage(&result) * 12 / 100;
                    let remaining = remaining1 + additional;
                    let new_damage = (item::max_damage(&result) - remaining).max(0);
                    if new_damage < item::damage(&result) {
                        item::set_damage(&mut result, new_damage);
                        price += 2;
                    }
                }
                let mut any_compatible = false;
                let mut any_incompatible = false;
                for (id, level) in crafting_enchantments(&addition) {
                    let current = enchantments
                        .iter()
                        .find(|(e, _)| *e == id)
                        .map_or(0, |(_, l)| *l);
                    let mut level = if current == level {
                        level + 1
                    } else {
                        level.max(current)
                    };
                    let mut compatible = e.can_enchant(id, &input);
                    if infinite || item::item_id(&input) == items::ENCHANTED_BOOK {
                        compatible = true;
                    }
                    for &(other, _) in &enchantments {
                        if other != id && !e.compatible(id, other) {
                            compatible = false;
                            price += 1;
                        }
                    }
                    if !compatible {
                        any_incompatible = true;
                        continue;
                    }
                    any_compatible = true;
                    let info = e.get(id);
                    let max = info.map_or(1, |i| i.max_level);
                    if level > max {
                        level = max;
                    }
                    match enchantments.iter_mut().find(|(e, _)| *e == id) {
                        Some(entry) => entry.1 = level,
                        None => enchantments.push((id, level)),
                    }
                    let mut fee = info.map_or(1, |i| i.anvil_cost);
                    if using_book {
                        fee = (fee / 2).max(1);
                    }
                    price += fee * level;
                    if input.count > 1 {
                        price = 40;
                    }
                }
                if any_incompatible && !any_compatible {
                    fail(self);
                    return;
                }
            }
        }
        let name = self.menu.item_name.clone().filter(|n| !n.trim().is_empty());
        match name {
            Some(name) => {
                if Some(&name) != custom_name(&input).as_ref() {
                    naming_cost = 1;
                    price += naming_cost;
                    set_custom_name(&mut result, Some(&name));
                }
            }
            None => {
                if input.component(components::CUSTOM_NAME).is_some() {
                    naming_cost = 1;
                    price += naming_cost;
                    set_custom_name(&mut result, None);
                }
            }
        }
        let final_price = if price <= 0 {
            0
        } else {
            i32::try_from((tax + i64::from(price)).clamp(0, i64::from(i32::MAX))).unwrap_or(0)
        };
        self.set_data(0, final_price);
        if price <= 0 {
            result = ItemStack::EMPTY;
        }
        if naming_cost == price && naming_cost > 0 {
            if self.menu.data.first().is_some_and(|&c| c >= 40) {
                self.set_data(0, 39);
            }
            self.menu.only_renaming = true;
        }
        if self.menu.data.first().is_some_and(|&c| c >= 40) && !infinite {
            result = ItemStack::EMPTY;
        }
        if !result.is_empty() {
            let mut base = repair_cost(&result).max(repair_cost(&addition));
            if naming_cost != price || naming_cost == 0 {
                base = increased_repair_cost(base);
            }
            set_repair_cost(&mut result, base);
            set_crafting_enchantments(&mut result, &enchantments);
        }
        self.set_local(2, result);
    }

    /// `player.getRandom().nextFloat()`.
    fn player_random_float(&mut self) -> f32 {
        match self.random.as_deref_mut() {
            Some(r) => r.next_float(),
            None => self.level.random().next_float(),
        }
    }

    /// `AnvilMenu.onTake`.
    fn anvil_take(&mut self) {
        let cost = self.menu.data.first().copied().unwrap_or(0);
        if !self.menu.state.creative
            && let Some(xp) = self.xp.as_deref_mut()
        {
            xp.give_levels(-cost);
        }
        if self.menu.repair_item_count_cost > 0 {
            let mut addition = self.local(1);
            if !addition.is_empty() && addition.count > self.menu.repair_item_count_cost {
                addition.shrink(self.menu.repair_item_count_cost);
                self.set_local(1, addition);
            } else {
                self.set_local(1, ItemStack::EMPTY);
            }
            self.workstation_changed();
        } else if !self.menu.only_renaming {
            self.set_local(1, ItemStack::EMPTY);
            self.workstation_changed();
        }
        self.set_data(0, 0);
        self.set_local(0, ItemStack::EMPTY);
        self.workstation_changed();
        // The anvil wears (12 %, the player's own random) unless the player is in creative.
        let pos = self.menu.pos;
        let state = self.level.block(pos);
        let is_anvil = state.block().info().kind == vanillite_data::block::BlockKind::AnvilBlock;
        if !self.menu.state.creative && is_anvil && self.player_random_float() < 0.12 {
            let damaged = if state.block() == vanillite_data::block::ids::ANVIL {
                Some(vanillite_data::block::ids::CHIPPED_ANVIL)
            } else if state.block() == vanillite_data::block::ids::CHIPPED_ANVIL {
                Some(vanillite_data::block::ids::DAMAGED_ANVIL)
            } else {
                None
            };
            match damaged {
                Some(block) => {
                    let new = vanillite_worldgen::behaviour::with_properties_of(block, state);
                    self.level
                        .set_block(pos, new, crate::level::update::CLIENTS);
                    self.level.level_event(None, 1030, pos, 0);
                }
                None => {
                    self.level.remove_block(pos, false);
                    self.level.level_event(None, 1029, pos, 0);
                }
            }
        } else {
            self.level.level_event(None, 1030, pos, 0);
        }
    }

    // --- Smithing --------------------------------------------------------------------------

    fn smithing_result(&mut self) {
        let Some(d) = data() else {
            return;
        };
        let (template, base, addition) = (self.local(0), self.local(1), self.local(2));
        let result = smithing_result(&d.recipes, &d.enchanting, &template, &base, &addition);
        self.set_local(3, result);
    }

    /// The crafted part of the `onTake` of the smithing table's and the stonecutter's result
    /// slot: `carried.onCraftedBy(player, count)` and `awardUsedRecipes` with the inputs
    /// (`recipe_crafted`, the recipe unlocked).
    pub(super) fn workstation_crafted(&mut self, taken: &ItemStack) {
        let Some(d) = data() else {
            return;
        };
        let (recipe, inputs) = match self.menu.kind {
            MenuKind::Stonecutter => {
                let recipe = self
                    .menu
                    .data
                    .first()
                    .and_then(|&i| usize::try_from(i).ok())
                    .and_then(|i| self.menu.recipes.get(i))
                    .and_then(|&r| d.recipes.stonecutting.get(r))
                    .map(|r| r.id.clone());
                (recipe, vec![self.local(0)])
            }
            MenuKind::Smithing => {
                let inputs = vec![self.local(0), self.local(1), self.local(2)];
                let recipe = smithing_recipe(&d.recipes, &inputs[0], &inputs[1], &inputs[2]);
                (recipe, inputs)
            }
            _ => return,
        };
        use crate::advancements::award::Award;
        if !taken.is_empty() {
            let stat = crate::stats::Stat::of(crate::stats::StatType::Crafted, taken.item);
            self.inv.awards.push(Award::Stat(stat, taken.count));
        }
        if let Some(id) = recipe {
            self.inv.awards.push(Award::Trigger(
                crate::advancements::trigger::Event::RecipeCrafted(id.clone(), inputs),
            ));
            self.inv.awards.push(Award::Recipes(vec![id]));
        }
    }

    /// The `onTake` of a workstation's result slot.
    pub(super) fn workstation_take(&mut self, _ctx: ClickContext, _drops: &mut Vec<Drop>) {
        let pos = self.menu.pos;
        match self.menu.kind {
            MenuKind::Stonecutter => {
                let input = self.local(0);
                if !input.is_empty() {
                    let mut rest = input;
                    rest.shrink(1);
                    self.set_local(0, rest.clone());
                    self.workstation_changed();
                    if !rest.is_empty() {
                        let selected = self.menu.data.first().copied().unwrap_or(-1);
                        self.stonecutter_setup_result(selected);
                    }
                }
                let time = self.level.game_time();
                if self.menu.last_sound != time {
                    let sound = crate::block::sound_id("ui.stonecutter.take_result");
                    self.level.play_block_sound(None, pos, sound, 4, 1.0, 1.0);
                    self.menu.last_sound = time;
                }
            }
            MenuKind::Anvil => self.anvil_take(),
            MenuKind::Grindstone => {
                let xp = grindstone_experience(self);
                let at = crate::block::furnace::center(pos);
                self.level.award_experience(at, xp);
                self.level.level_event(None, 1042, pos, 0);
                self.set_local(0, ItemStack::EMPTY);
                self.set_local(1, ItemStack::EMPTY);
                self.workstation_changed();
            }
            MenuKind::Smithing => {
                for i in 0..3 {
                    let mut s = self.local(i);
                    if !s.is_empty() {
                        s.shrink(1);
                        self.set_local(i, s);
                    }
                }
                self.workstation_changed();
                self.level.level_event(None, 1044, pos, 0);
            }
            _ => {}
        }
    }

    /// `quickMoveStack` of the workstations.
    pub(super) fn workstation_quick_move(
        &mut self,
        index: usize,
        ctx: ClickContext,
        drops: &mut Vec<Drop>,
    ) -> ItemStack {
        let original = self.item(index).clone();
        if original.is_empty() {
            return ItemStack::EMPTY;
        }
        let kind = self.menu.kind;
        if kind == MenuKind::Loom {
            return self.loom_quick_move(index, ctx, drops);
        }
        if kind == MenuKind::CartographyTable {
            return self.cartography_quick_move(index, ctx, drops);
        }
        let mut stack = original.clone();
        if kind == MenuKind::Enchantment {
            let moved = if index < 2 {
                super::move_to(self, &mut stack, 2, 38, true)
            } else if item::item_id(&stack) == items::LAPIS_LAZULI {
                super::move_to(self, &mut stack, 1, 2, true)
            } else {
                if !self.local(0).is_empty() {
                    return ItemStack::EMPTY;
                }
                let single = stack.split(1);
                super::set_by_player(self, 0, single);
                true
            };
            if !moved {
                return ItemStack::EMPTY;
            }
            return super::finish_quick_move(self, index, stack, &original, ctx, drops);
        }
        let Some(result) = result_slot(kind) else {
            return ItemStack::EMPTY;
        };
        let inv_start = result + 1;
        let inv_end = inv_start + 27;
        let use_end = inv_end + 9;
        let moved = if index == result {
            let moved = super::move_to(self, &mut stack, inv_start, use_end, true);
            if moved && kind == MenuKind::Grindstone {
                // `onQuickCraft` has no effect for these result slots.
            }
            moved
        } else if index < result {
            super::move_to(self, &mut stack, inv_start, use_end, false)
        } else {
            match kind {
                MenuKind::Stonecutter => {
                    let accepted = data().is_some_and(|d| {
                        d.recipes
                            .stonecutting
                            .iter()
                            .any(|r| r.ingredient.test(&stack))
                    });
                    if accepted {
                        super::move_to(self, &mut stack, 0, 1, false)
                    } else if index < inv_end {
                        super::move_to(self, &mut stack, inv_end, use_end, false)
                    } else {
                        super::move_to(self, &mut stack, inv_start, inv_end, false)
                    }
                }
                MenuKind::Grindstone => {
                    if !self.local(0).is_empty() && !self.local(1).is_empty() {
                        if index < inv_end {
                            super::move_to(self, &mut stack, inv_end, use_end, false)
                        } else {
                            super::move_to(self, &mut stack, inv_start, inv_end, false)
                        }
                    } else {
                        super::move_to(self, &mut stack, 0, 2, false)
                    }
                }
                _ => {
                    // `ItemCombinerMenu`: into the inputs if they take it.
                    if self.can_move_into_inputs(&stack) {
                        super::move_to(self, &mut stack, 0, result, false)
                    } else if index < inv_end {
                        super::move_to(self, &mut stack, inv_end, use_end, false)
                    } else {
                        super::move_to(self, &mut stack, inv_start, inv_end, false)
                    }
                }
            }
        };
        if !moved {
            return ItemStack::EMPTY;
        }
        let out = super::finish_quick_move(self, index, stack.clone(), &original, ctx, drops);
        if kind == MenuKind::Stonecutter && index == result && !out.is_empty() && !stack.is_empty()
        {
            drops.push(Drop {
                stack,
                thrown: false,
            });
        }
        out
    }

    fn can_move_into_inputs(&self, stack: &ItemStack) -> bool {
        match self.menu.kind {
            MenuKind::Smithing => {
                (may_place(MenuKind::Smithing, 0, stack) && self.local(0).is_empty())
                    || (may_place(MenuKind::Smithing, 1, stack) && self.local(1).is_empty())
                    || (may_place(MenuKind::Smithing, 2, stack) && self.local(2).is_empty())
            }
            _ => true,
        }
    }
}

/// `GrindstoneMenu.computeResult`.
fn grindstone_result(input: &ItemStack, additional: &ItemStack) -> ItemStack {
    let Some(d) = data() else {
        return ItemStack::EMPTY;
    };
    let e = &d.enchanting;
    if input.is_empty() && additional.is_empty() {
        return ItemStack::EMPTY;
    }
    if input.count > 1 || additional.count > 1 {
        return ItemStack::EMPTY;
    }
    if input.is_empty() || additional.is_empty() {
        let only = if input.is_empty() { additional } else { input };
        if !has_any_enchantments(only) {
            return ItemStack::EMPTY;
        }
        return remove_non_curses(e, only.clone());
    }
    if input.item != additional.item {
        return ItemStack::EMPTY;
    }
    let durability = item::max_damage(input).max(item::max_damage(additional));
    let remaining1 = item::max_damage(input) - item::damage(input);
    let remaining2 = item::max_damage(additional) - item::damage(additional);
    let remaining = remaining1 + remaining2 + durability * 5 / 100;
    let mut count = 1;
    if !item::is_damageable(input) {
        if item::max_stack_size(input) < 2 || !item::matches(input, additional) {
            return ItemStack::EMPTY;
        }
        count = 2;
    }
    let mut out = input.with_count(count);
    if item::is_damageable(&out) {
        if durability != item::max_damage(&out) {
            out.set_component(components::MAX_DAMAGE, enchant::write_int(durability));
        }
        item::set_damage(&mut out, (durability - remaining).max(0));
    }
    // `mergeEnchantsFrom`: curses only if new, others upgrade.
    let mut list = crafting_enchantments(&out);
    for (id, level) in crafting_enchantments(additional) {
        let current = list.iter().find(|(e, _)| *e == id).map_or(0, |(_, l)| *l);
        if !e.is_curse(id) || current == 0 {
            match list.iter_mut().find(|(e, _)| *e == id) {
                Some(entry) => entry.1 = entry.1.max(level),
                None => list.push((id, level)),
            }
        }
    }
    set_crafting_enchantments(&mut out, &list);
    remove_non_curses(e, out)
}

/// `GrindstoneMenu.removeNonCursesFrom`.
fn remove_non_curses(e: &Enchanting, mut stack: ItemStack) -> ItemStack {
    let mut list = crafting_enchantments(&stack);
    list.retain(|(id, _)| e.is_curse(*id));
    set_crafting_enchantments(&mut stack, &list);
    if item::item_id(&stack) == items::ENCHANTED_BOOK && list.is_empty() {
        // `transmuteCopy(Items.BOOK)`.
        stack.item = items::BOOK.0;
        stack.patch_mut().unset(components::STORED_ENCHANTMENTS);
    }
    let mut cost = 0;
    for _ in 0..list.len() {
        cost = increased_repair_cost(cost);
    }
    set_repair_cost(&mut stack, cost);
    stack
}

/// The experience the grindstone gives for its inputs.
fn grindstone_experience(view: &mut ContainerView<'_>) -> i32 {
    let Some(d) = data() else {
        return 0;
    };
    let mut amount = 0;
    for i in 0..2 {
        for (id, level) in crafting_enchantments(&view.local(i)) {
            if !d.enchanting.is_curse(id) {
                amount += d.enchanting.get(id).map_or(0, |e| e.min_cost.at(level));
            }
        }
    }
    if amount <= 0 {
        return 0;
    }
    let half = (amount + 1) / 2;
    half + view.level.random().next_int_bounded(half)
}

/// The smithing result of the first recipe that matches.
/// The id of the smithing recipe the inputs match (`getRecipeFor`).
fn smithing_recipe(
    recipes: &Recipes,
    template: &ItemStack,
    base: &ItemStack,
    addition: &ItemStack,
) -> Option<String> {
    let fits = |ing: &Option<crate::recipe::Ingredient>, s: &ItemStack| match ing {
        Some(i) => i.test(s),
        None => s.is_empty(),
    };
    recipes
        .smithing
        .iter()
        .find(|(_, recipe)| match recipe {
            Smithing::Transform {
                template: t,
                base: b,
                addition: a,
                ..
            } => fits(t, template) && fits(b, base) && fits(a, addition),
            Smithing::Trim {
                template: t,
                base: b,
                addition: a,
                ..
            } => t.test(template) && b.test(base) && a.test(addition),
        })
        .map(|(id, _)| id.clone())
}

fn smithing_result(
    recipes: &Recipes,
    e: &Enchanting,
    template: &ItemStack,
    base: &ItemStack,
    addition: &ItemStack,
) -> ItemStack {
    let fits = |ing: &Option<crate::recipe::Ingredient>, s: &ItemStack| match ing {
        Some(i) => i.test(s),
        None => s.is_empty(),
    };
    for (_, recipe) in &recipes.smithing {
        match recipe {
            Smithing::Transform {
                template: t,
                base: b,
                addition: a,
                result,
            } => {
                if fits(t, template) && fits(b, base) && fits(a, addition) {
                    // `createWithOriginalComponents(result, base)`.
                    let mut out = base.clone();
                    out.item = result.item;
                    out.count = result.count;
                    return out;
                }
            }
            Smithing::Trim {
                template: t,
                base: b,
                addition: a,
                pattern,
            } => {
                if t.test(template) && b.test(base) && a.test(addition) {
                    let (Some(material), Some(pattern)) =
                        (e.trim_material(addition), e.trim_pattern(pattern))
                    else {
                        return ItemStack::EMPTY;
                    };
                    // `ArmorTrim`: material and pattern holders (registry id + 1).
                    let mut value = Vec::new();
                    vanillite_protocol::codec::write_varint(
                        &mut value,
                        i32::try_from(material).unwrap_or(0) + 1,
                    );
                    vanillite_protocol::codec::write_varint(
                        &mut value,
                        i32::try_from(pattern).unwrap_or(0) + 1,
                    );
                    if base.component(components::TRIM) == Some(&value[..]) {
                        return ItemStack::EMPTY;
                    }
                    let mut out = base.with_count(1);
                    out.set_component(components::TRIM, value.into_boxed_slice());
                    return out;
                }
            }
        }
    }
    ItemStack::EMPTY
}

// --- Enchanting table ------------------------------------------------------------------------

impl ContainerView<'_> {
    /// The player's `enchantmentSeed`.
    fn enchantment_seed(&self) -> i32 {
        self.xp.as_ref().map_or(0, |x| x.seed)
    }

    /// Bookshelves around the table (`EnchantingTableBlock.BOOKSHELF_OFFSETS`).
    fn bookshelves(&self) -> i32 {
        let pos = self.menu.pos;
        let mut count = 0;
        for y in 0..=1 {
            for x in -2i32..=2 {
                for z in -2i32..=2 {
                    if x.abs() != 2 && z.abs() != 2 {
                        continue;
                    }
                    let shelf = vanillite_base::BlockPos::new(pos.x + x, pos.y + y, pos.z + z);
                    let between =
                        vanillite_base::BlockPos::new(pos.x + x / 2, pos.y + y, pos.z + z / 2);
                    // `enchantment_power_provider` (bookshelves) with a
                    // `enchantment_power_transmitter` (replaceable) block between.
                    let tags = &vanillite_worldgen::behaviour::tags().blocks;
                    if tags
                        .enchantment_power_provider
                        .contains(self.level.block(shelf))
                        && tags
                            .enchantment_power_transmitter
                            .contains(self.level.block(between))
                    {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// `EnchantmentMenu.slotsChanged`: the costs and clues of the three options.
    fn enchantment_changed(&mut self) {
        let Some(d) = data() else {
            return;
        };
        let item = self.local(0);
        let enchantable = d.enchanting.enchantable(&item);
        let unenchanted = enchant::read(item.component(components::ENCHANTMENTS)).is_empty();
        if item.is_empty() || enchantable == 0 || !unenchanted {
            for i in 0..3 {
                self.set_data(i, 0);
                self.set_data(4 + i, -1);
                self.set_data(7 + i, -1);
            }
            return;
        }
        let shelves = self.bookshelves().min(15);
        let seed = self.enchantment_seed();
        let mut random = vanillite_base::random::LegacyRandom::new(i64::from(seed));
        let mut costs = [0i32; 3];
        for (slot, cost) in costs.iter_mut().enumerate() {
            // `EnchantmentHelper.getEnchantmentCost`.
            let selected = random.next_int_bounded(8)
                + 1
                + (shelves >> 1)
                + random.next_int_bounded(shelves + 1);
            let c = match slot {
                0 => (selected / 3).max(1),
                1 => selected * 2 / 3 + 1,
                _ => selected.max(shelves * 2),
            };
            *cost = if c < i32::try_from(slot).unwrap_or(0) + 1 {
                0
            } else {
                c
            };
        }
        let mut clues = [(-1i32, -1i32); 3];
        for slot in 0..3 {
            let cost = costs[slot];
            if cost > 0 {
                // `getEnchantmentList` reseeds the menu random (seed + slot) and advances it;
                // the clue is the next draw of that same random.
                let list = enchantment_list(
                    &d.enchanting,
                    &mut random,
                    &item,
                    seed,
                    slot,
                    cost,
                    enchantable,
                );
                if !list.is_empty() {
                    let len = i32::try_from(list.len()).unwrap_or(1);
                    let pick = usize::try_from(random.next_int_bounded(len)).unwrap_or(0);
                    if let Some(&(id, level)) = list.get(pick) {
                        clues[slot] = (i32::try_from(id).unwrap_or(-1), level);
                    }
                }
            }
        }
        for slot in 0..3 {
            self.set_data(slot, costs[slot]);
            self.set_data(4 + slot, clues[slot].0);
            self.set_data(7 + slot, clues[slot].1);
        }
        self.set_data(3, seed);
    }

    /// `EnchantmentMenu.clickMenuButton`.
    fn enchant_button(&mut self, id: i32) -> bool {
        let Some(d) = data() else {
            return false;
        };
        let Ok(slot) = usize::try_from(id) else {
            return false;
        };
        if slot >= 3 {
            log::debug!("A player pressed invalid enchanting button id {id}");
            return false;
        }
        let infinite = self.menu.state.creative;
        let item = self.local(0);
        let currency = self.local(1);
        let cost = id + 1;
        let level = self.xp.as_ref().map_or(0, |x| x.level);
        let option_cost = self.menu.data.get(slot).copied().unwrap_or(0);
        if (currency.is_empty() || currency.count < cost) && !infinite {
            return false;
        }
        if option_cost <= 0
            || item.is_empty()
            || ((level < cost || level < option_cost) && !infinite)
        {
            return false;
        }
        let enchantable = d.enchanting.enchantable(&item);
        let list = enchantment_list(
            &d.enchanting,
            &mut vanillite_base::random::LegacyRandom::new(0),
            &item,
            self.enchantment_seed(),
            slot,
            option_cost,
            enchantable,
        );
        if list.is_empty() {
            return true;
        }
        // Bukkit's `EnchantItemEvent`: a cancelled enchantment takes nothing.
        if !self.menu.plugin
            && let Some(hooks) = self.level.hooks_for(vanillite_abi::event::ENCHANT_ITEM)
        {
            let mut chosen = vanillite_nbt::Compound::new();
            for &(id, level) in &list {
                if let Some(e) = d.enchanting.get(id) {
                    chosen.insert(e.name.clone(), vanillite_nbt::Tag::Int(level));
                }
            }
            let mut ev = crate::hooks::Event {
                entity: self.menu.player,
                pos: self.menu.pos,
                item: crate::hooks::item_snbt(&item),
                value: i64::from(option_cost),
                text: vanillite_nbt::snbt::to_string(&vanillite_nbt::Tag::Compound(chosen)),
                ..crate::hooks::Event::new(vanillite_abi::event::ENCHANT_ITEM)
            };
            hooks.event(crate::hooks::HookWorld::NONE, &mut ev);
            if ev.cancelled {
                return false;
            }
        }
        // `onEnchantmentPerformed`: levels and a new seed.
        let new_seed = match self.random.as_deref_mut() {
            Some(r) => r.next_int(),
            None => self.level.random().next_int(),
        };
        if let Some(xp) = self.xp.as_deref_mut() {
            xp.level -= cost;
            if xp.level < 0 {
                xp.level = 0;
                xp.progress = 0.0;
                xp.total = 0;
            }
            xp.seed = new_seed;
        }
        let mut result = item;
        if item::item_id(&result) == items::BOOK {
            result.item = items::ENCHANTED_BOOK.0;
        }
        let mut current = crafting_enchantments(&result);
        for (id, level) in list {
            match current.iter_mut().find(|(e, _)| *e == id) {
                Some(entry) => entry.1 = entry.1.max(level),
                None => current.push((id, level)),
            }
        }
        set_crafting_enchantments(&mut result, &current);
        let enchanted = result.clone();
        if let Some(s) = self.menu.local.get_mut(0) {
            *s = result;
        }
        let mut currency = currency;
        if !infinite {
            currency.shrink(cost);
        }
        if let Some(s) = self.menu.local.get_mut(1) {
            *s = if currency.is_empty() {
                ItemStack::EMPTY
            } else {
                currency
            };
        }
        use crate::advancements::award::Award;
        self.inv
            .awards
            .push(Award::Stat(crate::stats::custom::ENCHANT_ITEM, 1));
        self.inv.awards.push(Award::Trigger(
            crate::advancements::trigger::Event::EnchantedItem(enchanted, cost),
        ));
        self.enchantment_changed();
        let pos = self.menu.pos;
        let pitch = self.level.random().next_float() * 0.1 + 0.9;
        let sound = crate::block::sound_id("block.enchantment_table.use");
        self.level.play_block_sound(None, pos, sound, 4, 1.0, pitch);
        true
    }
}

/// `EnchantmentMenu.getEnchantmentList` with `EnchantmentHelper.selectEnchantment`.
/// `random` is the menu's, reseeded with `seed + slot` (int arithmetic).
fn enchantment_list(
    e: &Enchanting,
    random: &mut vanillite_base::random::LegacyRandom,
    item: &ItemStack,
    seed: i32,
    slot: usize,
    cost: i32,
    enchantable: i32,
) -> Vec<(u32, i32)> {
    vanillite_base::random::RandomSource::set_seed(
        random,
        i64::from(seed.wrapping_add(i32::try_from(slot).unwrap_or(0))),
    );
    if enchantable <= 0 {
        return Vec::new();
    }
    let mut results = crate::enchant::select_enchantment(e, random, item, cost, &e.table);
    let is_book = item::item_id(item) == items::BOOK;
    if is_book && results.len() > 1 {
        let len = i32::try_from(results.len()).unwrap_or(1);
        let remove = usize::try_from(random.next_int_bounded(len)).unwrap_or(0);
        results.remove(remove);
    }
    results
}
