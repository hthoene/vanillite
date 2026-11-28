//! What breaking blocks drops (`Block.dropResources`, `Entity.spawnAtLocation`) and how item
//! entities enter the level (`new ItemEntity` + `addFreshEntity`).

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::{BlockId, BlockState};
use vanillite_world::entity::EntityId;

use crate::item::ItemStack;
use crate::item_entity::ItemData;
use crate::level::Level;
use crate::loot::BlockLootContext;

/// `Block.dropResources(state, level, pos, blockEntity, breaker, tool)` without a tool: the
/// block's loot at `pos`, if the `doTileDrops` game rule allows. Player breaks evaluate the
/// loot with their tool in [`crate::player::play`].
pub fn drop_resources(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    breaker: Option<EntityId>,
) {
    let Some(data) = crate::player::play::game_data() else {
        return;
    };
    let mut stacks = Vec::new();
    let tool = ItemStack::EMPTY;
    let Some(state) = loot_state(level, pos, state) else {
        return;
    };
    let entity = level.block_entity(pos).cloned();
    let kept = level.block_entities.kept_components(pos).cloned();
    let components = |id: u16| {
        entity
            .as_ref()
            .and_then(|e| crate::level::block_entity::collect_component_with(e, kept.as_ref(), id))
    };
    let item_in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let loot = BlockLootContext {
        state,
        tool: &tool,
        enchantment_level: &|_| 0,
        item_in_tag: &item_in_tag,
        explosion_radius: None,
        this_present: breaker.is_some(),
        entity: None,
        block_entity: Some(&components),
    };
    // `LootContext.Builder.create`: the table's random sequence, else the level random.
    let mut random = level.lend_random();
    data.loot
        .roll_block(level, pos, &loot, &mut random, &mut stacks);
    level.restore_random(random);
    if let Some(hooks) = level.hooks_for(vanillite_abi::event::BLOCK_DROP) {
        let player = breaker
            .filter(|&b| level.entities.player(b).is_some())
            .and_then(|b| level.entities.store.get(b).map(|e| e.uuid()))
            .unwrap_or(0);
        let xp = super::break_hooks::break_experience(level, state, &tool);
        let Some(xp) = block_drop_event(&*hooks, player, pos, state, &mut stacks, xp) else {
            return;
        };
        for stack in stacks {
            pop_resource(level, pos, stack);
        }
        super::break_hooks::spawn_after_break_with(level, state, pos, &tool, xp);
        return;
    }
    for stack in stacks {
        pop_resource(level, pos, stack);
    }
    super::break_hooks::spawn_after_break(level, state, pos, &tool, true);
}

/// The most stacks a plugin may make a broken block drop.
const MAX_PLUGIN_DROPS: usize = 64;

/// The most experience a plugin may make a broken block drop.
const MAX_PLUGIN_EXPERIENCE: i64 = 100_000;

/// Fires `BLOCK_DROP` for the block `state` at `pos` that `player` (0 for none) broke:
/// handlers may replace `stacks` and change the `experience`. `None` when cancelled (nothing
/// drops).
pub fn block_drop_event(
    hooks: &dyn crate::hooks::GameHooks,
    player: u128,
    pos: BlockPos,
    state: BlockState,
    stacks: &mut Vec<ItemStack>,
    experience: i32,
) -> Option<i32> {
    let mut ev = crate::hooks::Event {
        player,
        pos,
        state: Some(state),
        item: crate::hooks::items_snbt(stacks.iter()),
        value: i64::from(experience),
        ..crate::hooks::Event::new(vanillite_abi::event::BLOCK_DROP)
    };
    hooks.event(crate::hooks::HookWorld::NONE, &mut ev);
    if ev.cancelled {
        return None;
    }
    if let Some(mut items) = ev.item_out.as_deref().and_then(crate::hooks::parse_items) {
        items.truncate(MAX_PLUGIN_DROPS);
        *stacks = items;
    }
    Some(i32::try_from(ev.value.clamp(0, MAX_PLUGIN_EXPERIENCE)).unwrap_or(0))
}

/// The state whose loot `state` at `pos` drops: `MovingPistonBlock.getDrops` rolls the moved
/// state of its block entity (nothing without one).
fn loot_state(level: &Level, pos: BlockPos, state: BlockState) -> Option<BlockState> {
    if state.block() != vanillite_data::block::ids::MOVING_PISTON {
        return Some(state);
    }
    match level.block_entity(pos) {
        Some(crate::level::block_entity::BlockEntity::MovingPiston(piston)) => {
            Some(piston.moved_state)
        }
        _ => None,
    }
}

/// `BlockState.getDrops` of a block an explosion destroys (`onExplosionHit`): its loot without
/// a tool, with the explosion radius for `explosion_decay` when the blocks decay. The loot
/// is rolled whether or not blocks drop (`popResource` checks that).
pub fn explosion_loot(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    radius: Option<f32>,
    source: Option<EntityId>,
    out: &mut Vec<ItemStack>,
) {
    let Some(data) = crate::player::play::game_data() else {
        return;
    };
    let tool = ItemStack::EMPTY;
    let Some(state) = loot_state(level, pos, state) else {
        return;
    };
    let entity = level.block_entity(pos).cloned();
    let components = |id: u16| {
        entity
            .as_ref()
            .and_then(|e| super::container::component(e, id))
    };
    let item_in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let loot = BlockLootContext {
        state,
        tool: &tool,
        enchantment_level: &|_| 0,
        item_in_tag: &item_in_tag,
        explosion_radius: radius,
        this_present: source.is_some(),
        entity: None,
        block_entity: Some(&components),
    };
    let mut random = std::mem::replace(level.random(), LegacyRandom::new(0));
    data.loot.roll_block(level, pos, &loot, &mut random, out);
    *level.random() = random;
}

/// `Block.popResource(level, pos, stack)`: the stack as an item entity around the block's
/// centre.
pub fn pop_resource(level: &mut Level, pos: BlockPos, stack: ItemStack) {
    let half_height = 0.125;
    let x = f64::from(pos.x) + 0.5 + (level.random().next_double() * 0.5 - 0.25);
    let y = f64::from(pos.y) + 0.5 + (level.random().next_double() * 0.5 - 0.25) - half_height;
    let z = f64::from(pos.z) + 0.5 + (level.random().next_double() * 0.5 - 0.25);
    // The position is drawn either way; empty stacks and the `block_drops` rule only stop the
    // item entity.
    if stack.is_empty() || !level.settings.block_drops {
        return;
    }
    spawn_item_moving(level, Vec3::new(x, y, z), stack, None, 10, None);
}

/// `spawnAtLocation(level, block)`: the item of `block` as an item entity at `pos`.
pub fn spawn_block_item(level: &mut Level, pos: Vec3, block: BlockId) {
    let item = block.info().item;
    if item == u16::MAX || item == 0 {
        return;
    }
    spawn_item_moving(level, pos, ItemStack::new(item, 1), None, 10, None);
}

/// Adds an item entity with `pickup_delay` ticks before it can be picked up and the UUID of
/// its thrower.
pub fn spawn_item(
    level: &mut Level,
    pos: Vec3,
    stack: ItemStack,
    velocity: Vec3,
    pickup_delay: i16,
    thrower: Option<u128>,
) -> Option<EntityId> {
    spawn_item_moving(level, pos, stack, Some(velocity), pickup_delay, thrower)
}

/// [`spawn_item`] with `new ItemEntity(level, x, y, z, stack)`'s random velocity if `velocity`
/// is `None`. The item draws its yaw, then the velocity, from its own random.
pub fn spawn_item_moving(
    level: &mut Level,
    pos: Vec3,
    stack: ItemStack,
    velocity: Option<Vec3>,
    pickup_delay: i16,
    thrower: Option<u128>,
) -> Option<EntityId> {
    if stack.is_empty() {
        return None;
    }
    // The `ItemEntity` constructor draws `bobOffs`, the yaw and the motion from the item's own
    // random (seeded from the entity seed stream).
    let (uuid, y_rot, motion, item_random) = crate::mob::new_item_entity();
    let velocity = velocity.unwrap_or(motion);
    add_item(
        level,
        (uuid, y_rot, item_random),
        pos,
        stack,
        velocity,
        pickup_delay,
        thrower,
    )
}

/// [`spawn_item`] with `new ItemEntity(level, x, y, z, stack, dx, dy, dz)`, which draws no
/// motion from the item's own random (`Block.popResourceFromFace`, a goat's horn).
pub fn spawn_item_with_motion(
    level: &mut Level,
    pos: Vec3,
    stack: ItemStack,
    velocity: Vec3,
    pickup_delay: i16,
) -> Option<EntityId> {
    if stack.is_empty() {
        return None;
    }
    let new = crate::mob::new_item_entity_with_motion();
    add_item(level, new, pos, stack, velocity, pickup_delay, None)
}

/// Adds the item entity `(uuid, y_rot, random state)`.
fn add_item(
    level: &mut Level,
    (uuid, y_rot, item_random): (u128, f32, i64),
    pos: Vec3,
    stack: ItemStack,
    velocity: Vec3,
    pickup_delay: i16,
    thrower: Option<u128>,
) -> Option<EntityId> {
    let network_id = level.network_ids().next();
    let id = level.entities.spawn_item(
        network_id,
        uuid,
        pos,
        velocity,
        y_rot,
        ItemData::new(stack, pickup_delay, thrower, item_random),
    );
    level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0)
        .then_some(id)
}
