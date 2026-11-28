//! Campfires (`CampfireBlock`, `CampfireBlockEntity`): four slots of food that cook while the
//! fire is lit and cool down while it is not; shovels douse the fire.

use vanillite_base::BlockPos;
use vanillite_data::block::{BlockId, BlockState};
use vanillite_nbt::{Compound, Tag};
use vanillite_worldgen::behaviour::StateExt;

use super::{BlockBehaviour, Interaction, ItemInteraction, UseContext};
use crate::item::ItemStack;
use crate::level::game_event::{GameEventContext, events};
use crate::level::{BlockEntity, Level};
use crate::recipe::CookingKind;

/// `BURN_COOL_SPEED`.
const BURN_COOL_SPEED: i32 = 2;

/// The data of a `CampfireBlockEntity`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Campfire {
    /// The four slots.
    pub items: [ItemStack; 4],
    /// `cookingProgress`.
    pub progress: [i32; 4],
    /// `cookingTime`.
    pub time: [i32; 4],
}

impl Campfire {
    /// `loadAdditional`.
    pub fn load(&mut self, nbt: &Compound) {
        self.items = Default::default();
        super::container::load_items(nbt, &mut self.items);
        read_ints(nbt, "CookingTimes", &mut self.progress);
        read_ints(nbt, "CookingTotalTimes", &mut self.time);
    }

    /// `saveAdditional`.
    pub fn save(&self, out: &mut Compound) {
        super::container::save_items_always(&self.items, out);
        out.insert("CookingTimes", Tag::IntArray(self.progress.to_vec()));
        out.insert("CookingTotalTimes", Tag::IntArray(self.time.to_vec()));
    }

    /// `getUpdateTag`: the items only.
    pub fn update_tag(&self) -> Compound {
        let mut out = Compound::new();
        super::container::save_items_always(&self.items, &mut out);
        out
    }
}

fn read_ints(nbt: &Compound, key: &str, target: &mut [i32; 4]) {
    *target = [0; 4];
    if let Some(Tag::IntArray(values)) = nbt.get(key) {
        for (t, v) in target.iter_mut().zip(values) {
            *t = *v;
        }
    }
}

/// Whether a block is a campfire (`#campfires` with `lit` and `waterlogged`).
pub fn is_campfire(block: BlockId) -> bool {
    super::decor::block_entity_type(block) == "minecraft:campfire"
}

/// Fills the campfire behaviours.
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    if is_campfire(block) {
        b.use_item_on = Some(use_item_on);
    }
}

fn recipes() -> Option<std::sync::Arc<crate::player::play::GameData>> {
    crate::player::play::game_data()
}

/// The ticker: `cookTick` while lit, `cooldownTick` otherwise (Vanilla picks the ticker by the
/// `lit` state).
pub fn tick(level: &mut Level, pos: BlockPos, state: BlockState) {
    if state.flag("lit") {
        cook_tick(level, pos, state);
    } else {
        cooldown_tick(level, pos);
    }
}

/// `cookTick`.
fn cook_tick(level: &mut Level, pos: BlockPos, state: BlockState) {
    let mut changed = false;
    for slot in 0..4 {
        let Some(BlockEntity::Campfire(c)) = level.block_entity_mut(pos) else {
            return;
        };
        let (Some(stack), Some(progress), Some(time)) = (
            c.items.get(slot),
            c.progress.get_mut(slot),
            c.time.get(slot).copied(),
        ) else {
            continue;
        };
        if stack.is_empty() {
            continue;
        }
        changed = true;
        *progress += 1;
        if *progress < time {
            continue;
        }
        let input = stack.clone();
        let result = recipes()
            .and_then(|r| {
                r.recipes
                    .cooking_for(CookingKind::Campfire, &input)
                    .map(|r| r.result.clone())
            })
            .unwrap_or(input);
        if let Some(target) = c.items.get_mut(slot) {
            *target = ItemStack::EMPTY;
        }
        super::container::drop_item_stack(level, pos, result);
        level.block_entity_changed(pos);
        level.send_block_updated(pos);
        level.game_event_block(
            events().block_change,
            pos,
            GameEventContext::with_state(None, state),
        );
    }
    if changed {
        super::container::set_changed(level, pos);
        level.block_entities.touch(pos.chunk());
    }
}

/// `cooldownTick`.
fn cooldown_tick(level: &mut Level, pos: BlockPos) {
    let Some(BlockEntity::Campfire(c)) = level.block_entity_mut(pos) else {
        return;
    };
    let mut changed = false;
    for (progress, time) in c.progress.iter_mut().zip(c.time) {
        if *progress > 0 {
            changed = true;
            *progress = (*progress - BURN_COOL_SPEED).clamp(0, time.max(0));
        }
    }
    if changed {
        super::container::set_changed(level, pos);
        level.block_entities.touch(pos.chunk());
    }
}

/// `useItemOn`: food goes on a free slot; shovels douse a lit fire.
fn use_item_on(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    if matches!(level.block_entity(pos), Some(BlockEntity::Campfire(_)))
        && recipes().is_some_and(|r| r.recipes.is_cooking_input(CookingKind::Campfire, stack))
    {
        if place_food(level, pos, Some(ctx.player), stack) {
            level.award_stat(ctx.player, crate::stats::custom::INTERACT_WITH_CAMPFIRE, 1);
            return ItemInteraction::Done(Interaction::Success);
        }
        return ItemInteraction::Done(Interaction::Consume);
    }
    let douses = crate::player::play::game_data()
        .is_some_and(|d| d.item_in_tag(stack.item, "minecraft:douses_campfires"));
    if douses && state.flag("lit") {
        level.level_event(None, 1009, pos, 0);
        level.game_event_block(
            events().block_change,
            pos,
            GameEventContext::of(Some(ctx.player)),
        );
        level.set_block_and_update(pos, state.set_flag("lit", false));
        super::hurt_item(level, stack, 1, ctx.player, ctx.hand_slot());
        return ItemInteraction::Done(Interaction::Success);
    }
    ItemInteraction::TryWithEmptyHand
}

/// `placeFood`: one item of `stack` goes on the first free slot.
pub fn place_food(
    level: &mut Level,
    pos: BlockPos,
    source: Option<vanillite_world::entity::EntityId>,
    stack: &mut ItemStack,
) -> bool {
    let Some(time) = recipes().and_then(|r| {
        r.recipes
            .cooking_for(CookingKind::Campfire, stack)
            .map(|r| r.cooking_time)
    }) else {
        return false;
    };
    let state = level.block(pos);
    let Some(BlockEntity::Campfire(c)) = level.block_entity_mut(pos) else {
        return false;
    };
    let Some(slot) = c.items.iter().position(ItemStack::is_empty) else {
        return false;
    };
    if let (Some(item), Some(t), Some(p)) = (
        c.items.get_mut(slot),
        c.time.get_mut(slot),
        c.progress.get_mut(slot),
    ) {
        *t = time;
        *p = 0;
        *item = stack.split(1);
    }
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext::with_state(source, state),
    );
    level.block_entity_changed(pos);
    level.send_block_updated(pos);
    true
}
