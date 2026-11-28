//! Items players use on blocks (`Item.useOn`) and in the air (`Item.use`): the block
//! transformers of axes, shovels and hoes, bone meal, honeycomb, shears on growing plants, fire
//! charges, water bottles on dirt, the debug stick, buckets, glass bottles, lily pads and
//! frogspawn, and right-click equipping (`Equippable.swapWithEquipmentSlot`). Adventure mode
//! `can_place_on` / `can_break` predicates are evaluated here too.

use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockKind, BlockState, ids as blocks};
use vanillite_data::item::ids as items;
use vanillite_protocol::codec::Reader;
use vanillite_protocol::components::ids;
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_worldgen::behaviour::StateExt;

use super::{PlayContext, hurt_and_break};
use crate::advancements::trigger::{Event, TriggerKind};
use crate::block::sound_id;
use crate::inventory::Drop;
use crate::item::{self, EquipmentSlot, ItemStack};
use crate::level::game_event::{GameEventContext, events};
use crate::level::{Level, SOUND_BLOCKS, SOUND_NEUTRAL, SOUND_PLAYERS, update};
use crate::stats::{Stat, StatType};

/// `InteractionResult` of an item use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Used {
    /// `PASS`: the item has no use here.
    Pass,
    /// `FAIL`.
    Fail,
    /// `SUCCESS` (or `SUCCESS_SERVER`, `CONSUME`): the use happened.
    Success,
}

/// A click on a block (`UseOnContext`).
#[derive(Clone, Copy, Debug)]
pub(super) struct Click {
    pub hand: Hand,
    pub pos: BlockPos,
    pub face: Direction,
}

/// The stack in `hand`.
pub(super) fn held(ctx: &PlayContext<'_>, hand: Hand) -> ItemStack {
    match hand {
        Hand::MainHand => ctx.inventory.selected_item().clone(),
        Hand::OffHand => ctx.inventory.offhand.clone(),
    }
}

/// `player.setItemInHand(hand, stack)`.
pub(super) fn set_held(ctx: &mut PlayContext<'_>, hand: Hand, stack: ItemStack) {
    let slot = match hand {
        Hand::MainHand => ctx.inventory.selected_item_mut(),
        Hand::OffHand => Some(&mut ctx.inventory.offhand),
    };
    if let Some(slot) = slot {
        *slot = stack;
    }
}

/// `itemStack.consume(1, player)` of the held stack: creative players keep it.
fn consume_held(ctx: &mut PlayContext<'_>, hand: Hand) {
    if ctx.actor.creative() {
        return;
    }
    let mut stack = held(ctx, hand);
    stack.shrink(1);
    set_held(ctx, hand, stack);
}

/// `ItemUtils.createFilledResult(itemStack, player, filled)`: one of the held stack turns into
/// `filled` (creative players get `filled` once, unless they have it).
fn filled_result(ctx: &mut PlayContext<'_>, hand: Hand, mut filled: ItemStack) {
    if ctx.actor.creative() {
        if !ctx.inventory.contains(&filled) {
            ctx.inventory.add(None, &mut filled, true);
        }
        return;
    }
    let mut stack = held(ctx, hand);
    stack.shrink(1);
    if stack.is_empty() {
        set_held(ctx, hand, filled);
        return;
    }
    set_held(ctx, hand, stack);
    if !ctx.inventory.add(None, &mut filled, false) {
        ctx.drops.push(Drop {
            stack: filled,
            thrown: false,
        });
    }
}

fn used_stat(ctx: &mut PlayContext<'_>, item: u16) {
    ctx.award_stat(Stat::of(StatType::Used, item), 1);
}

// --- useOn -----------------------------------------------------------------------------------

/// `ItemStack.useOn` of the items with a use of their own on blocks (`None` for the others,
/// block items included). The `ITEM_USED` statistic of a successful item interaction is
/// awarded here; the caller fires `item_used_on_block` and `any_block_use`.
pub(super) fn use_on(
    ctx: &mut PlayContext<'_>,
    click: Click,
    sink: &dyn super::PacketSink,
) -> Option<Used> {
    let stack = held(ctx, click.hand);
    let id = item::item_id(&stack);
    let kind = if let Some(transformer) = item::block_transformer(&stack) {
        UseOn::Transform(transformer)
    } else if id == items::BONE_MEAL {
        UseOn::BoneMeal
    } else if id == items::HONEYCOMB {
        UseOn::Honeycomb
    } else if id == items::SHEARS {
        UseOn::Shears
    } else if id == items::FIRE_CHARGE {
        UseOn::FireCharge
    } else if id == items::POTION {
        UseOn::Potion
    } else if id == items::DEBUG_STICK {
        UseOn::DebugStick
    } else {
        return None;
    };
    if ctx.actor.game_mode == vanillite_config::GameMode::Adventure
        && !can_place_on(ctx, &stack, click.pos)
    {
        return Some(Used::Pass);
    }
    ctx.level.level_mut()?;
    let creative = ctx.actor.creative();
    let count = stack.count;
    let result = match kind {
        UseOn::Transform(name) => transform(ctx, click, name),
        UseOn::BoneMeal => bone_meal(ctx, click),
        UseOn::Honeycomb => honeycomb(ctx, click),
        UseOn::Shears => shears(ctx, click),
        UseOn::FireCharge => fire_charge(ctx, click),
        UseOn::Potion => water_bottle_on_mud(ctx, click),
        UseOn::DebugStick => debug_stick(ctx, click.pos, true, click.hand, sink),
    };
    if creative {
        // `ServerPlayerGameMode.useItemOn`: creative players keep the count.
        let mut now = held(ctx, click.hand);
        if now.item == stack.item && !now.is_empty() {
            now.count = count;
            set_held(ctx, click.hand, now);
        } else if now.is_empty() {
            set_held(ctx, click.hand, stack.clone());
        }
    }
    if result == Used::Success {
        used_stat(ctx, stack.item);
    }
    Some(result)
}

#[derive(Clone, Copy)]
enum UseOn {
    Transform(&'static str),
    BoneMeal,
    Honeycomb,
    Shears,
    FireCharge,
    Potion,
    DebugStick,
}

/// One `BlockTransformData` of the Vanilla `minecraft:block_transformer` entries.
struct Transform {
    target: BlockState,
    sound: &'static str,
    /// `TransformParticle`'s level event (0: none).
    particle: i32,
    update_from_neighbors: bool,
    loot: Option<&'static str>,
    /// `DropStrategy.CLICKED_FACE` (else `FROM_MIDDLE`).
    drop_from_face: bool,
    copper_chest: bool,
}

/// The first transform of `name` that applies to the block at `pos` clicked on `face`
/// (`minecraft:shovel`, `minecraft:hoe`, `minecraft:axe` of `BlockTransformers`).
fn find_transform(level: &Level, name: &str, pos: BlockPos, face: Direction) -> Option<Transform> {
    let state = level.block(pos);
    let block = state.block();
    let tags = crate::player::play::game_data()?;
    let air_above = || {
        tags.block_in_tag(
            level.block(pos.relative(Direction::Up)).block().0,
            "minecraft:air",
        )
    };
    let plain = |target: BlockState, sound: &'static str| Transform {
        target,
        sound,
        particle: 0,
        update_from_neighbors: true,
        loot: None,
        drop_from_face: false,
        copper_chest: false,
    };
    match name {
        "minecraft:shovel" => (face != Direction::Down
            && tags.block_in_tag(block.0, "minecraft:turns_into_dirt_path")
            && air_above())
        .then(|| plain(blocks::DIRT_PATH.default_state(), "item.shovel.flatten")),
        "minecraft:hoe" => {
            if face != Direction::Down {
                if tags.block_in_tag(block.0, "minecraft:turns_into_farmland") && air_above() {
                    return Some(plain(blocks::FARMLAND.default_state(), "item.hoe.till"));
                }
                if block == blocks::COARSE_DIRT && air_above() {
                    return Some(plain(blocks::DIRT.default_state(), "item.hoe.till"));
                }
            }
            (block == blocks::ROOTED_DIRT).then(|| Transform {
                loot: Some("minecraft:till/rooted_dirt"),
                drop_from_face: true,
                ..plain(blocks::DIRT.default_state(), "item.hoe.till")
            })
        }
        "minecraft:axe" => {
            let path = block.name().strip_prefix("minecraft:")?;
            if !path.starts_with("stripped_")
                && let Some(stripped) = BlockId::by_name(&format!("minecraft:stripped_{path}"))
            {
                return Some(plain(copy_properties(stripped, state), "item.axe.strip"));
            }
            let (target, sound, particle) =
                if let Some(previous) = crate::block::weather::previous_weathering(block) {
                    (previous, "item.axe.scrape", 3005)
                } else if crate::block::weather::is_waxed_copper(block) {
                    let unwaxed =
                        BlockId::by_name(&format!("minecraft:{}", path.strip_prefix("waxed_")?))?;
                    (unwaxed, "item.axe.wax_off", 3004)
                } else {
                    return None;
                };
            let copper_chest = is_copper_chest(state);
            // `instanceof DoorBlock`: the weathering copper doors are doors too.
            let door = matches!(
                block.info().kind,
                BlockKind::DoorBlock | BlockKind::WeatheringCopperDoorBlock
            );
            Some(Transform {
                particle,
                update_from_neighbors: !(copper_chest || door),
                copper_chest,
                ..plain(copy_properties(target, state), sound)
            })
        }
        _ => None,
    }
}

/// `instanceof CopperChestBlock` (the waxable and scrapable chests).
fn is_copper_chest(state: BlockState) -> bool {
    matches!(
        state.block().info().kind,
        BlockKind::CopperChestBlock | BlockKind::WeatheringCopperChestBlock
    )
}

/// `ChestBlock.getConnectedBlockPos`.
fn connected_chest_pos(pos: BlockPos, state: BlockState) -> BlockPos {
    use vanillite_worldgen::behaviour::clockwise;
    let facing = state.direction("facing").unwrap_or(Direction::North);
    let side = if state.value("type") == Some("left") {
        clockwise(facing)
    } else {
        clockwise(facing).opposite()
    };
    pos.relative(side)
}

/// `CopyPropertiesProvider`: `block`'s default state with the properties of `from`.
fn copy_properties(block: BlockId, from: BlockState) -> BlockState {
    let mut out = block.default_state();
    for (key, value) in from.properties() {
        out = out.with(key, value).unwrap_or(out);
    }
    out
}

/// `BlockTransformer.transformBlock`.
fn transform(ctx: &mut PlayContext<'_>, click: Click, name: &str) -> Used {
    // `playerHasBlockingItemUseIntent`: a shield in the off hand is raised instead.
    let offhand = &ctx.inventory.offhand;
    let shield = crate::blocking::has_blocks_attacks(offhand);
    if click.hand == Hand::MainHand && shield && !ctx.actor.sneaking {
        return Used::Pass;
    }
    let player = ctx.actor.entity;
    let stack = held(ctx, click.hand);
    let pos = click.pos;
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let Some(t) = find_transform(level, name, pos, click.face) else {
        return Used::Pass;
    };
    let updated = if t.update_from_neighbors {
        update_from_neighbour_shapes(level, t.target, pos)
    } else {
        t.target
    };
    ctx.trigger(Event::ItemUsedOnBlock(pos, stack.clone()));
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let old = level.block(pos);
    if let Some(table) = t.loot {
        let drops = crate::block::interact::interact_loot(level, table, old);
        for drop in drops {
            if t.drop_from_face {
                pop_resource_from_face(level, pos, click.face, drop);
            } else {
                crate::block::drops::pop_resource(level, pos, drop);
            }
        }
    }
    // Tools are not stackable: they lose durability.
    if item::is_stackable(&stack) {
        consume_held(ctx, click.hand);
    } else {
        hurt_and_break(ctx, click.hand, 1);
    }
    let Some(level) = ctx.level.level_mut() else {
        return Used::Success;
    };
    level.set_block(pos, updated, update::ALL_IMMEDIATE);
    level.play_block_sound(player, pos, sound_id(t.sound), SOUND_BLOCKS, 1.0, 1.0);
    if t.particle != 0 {
        level.level_event(player, t.particle, pos, 0);
    }
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext::with_state(player, updated),
    );
    if t.copper_chest && old.value("type").is_some_and(|v| v != "single") {
        let other = connected_chest_pos(pos, old);
        let other_state = level.block(other);
        level.game_event_block(
            events().block_change,
            other,
            GameEventContext::with_state(player, other_state),
        );
        if t.particle != 0 {
            level.level_event(player, t.particle, other, 0);
        }
    }
    Used::Success
}

/// `Block.updateFromNeighbourShapes`.
fn update_from_neighbour_shapes(level: &mut Level, state: BlockState, pos: BlockPos) -> BlockState {
    let mut out = state;
    for direction in crate::block::place::util::UPDATE_SHAPE_ORDER {
        let neighbour = pos.relative(direction);
        let neighbour_state = level.block(neighbour);
        out = level.update_shape(out, pos, direction, neighbour, neighbour_state);
    }
    out
}

/// `Block.popResourceFromFace`.
fn pop_resource_from_face(level: &mut Level, pos: BlockPos, face: Direction, stack: ItemStack) {
    use crate::block::place::util::DirExt;
    use vanillite_base::random::RandomSource;
    let (sx, sy, sz) = (face.step_x(), face.step_y(), face.step_z());
    let half = 0.125;
    let mut between = |a: f64, b: f64| a + level.random().next_double() * (b - a);
    let x = f64::from(pos.x)
        + 0.5
        + if sx == 0 {
            between(-0.25, 0.25)
        } else {
            f64::from(sx) * (0.5 + half)
        };
    let y = f64::from(pos.y)
        + 0.5
        + if sy == 0 {
            between(-0.25, 0.25)
        } else {
            f64::from(sy) * (0.5 + half)
        }
        - half;
    let z = f64::from(pos.z)
        + 0.5
        + if sz == 0 {
            between(-0.25, 0.25)
        } else {
            f64::from(sz) * (0.5 + half)
        };
    let dx = if sx == 0 {
        between(-0.1, 0.1)
    } else {
        f64::from(sx) * 0.1
    };
    let dy = if sy == 0 {
        between(0.0, 0.1)
    } else {
        f64::from(sy) * 0.1 + 0.1
    };
    let dz = if sz == 0 {
        between(-0.1, 0.1)
    } else {
        f64::from(sz) * 0.1
    };
    // `popResource`: the position and motion are drawn before the stack and the rule are seen.
    if stack.is_empty() || !level.settings.block_drops {
        return;
    }
    crate::block::drops::spawn_item_with_motion(
        level,
        Vec3::new(x, y, z),
        stack,
        Vec3::new(dx, dy, dz),
        10,
    );
}

/// `BoneMealItem.useOn`.
fn bone_meal(ctx: &mut PlayContext<'_>, click: Click) -> Used {
    let player = ctx.actor.entity;
    let at = ctx.actor.pos;
    let mut stack = held(ctx, click.hand);
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let pos = click.pos;
    let relative = pos.relative(click.face);
    let grown = if crate::block::bonemeal::fertilize(level, pos, &mut stack, ctx.actor.uuid) {
        Some(pos)
    } else if crate::block::is_face_sturdy(level.block(pos), click.face)
        && crate::block::bonemeal::grow_water_plant_from(
            level,
            relative,
            Some(click.face),
            &mut stack,
        )
    {
        Some(relative)
    } else {
        None
    };
    let Some(at_pos) = grown else {
        return Used::Pass;
    };
    // `causeUseVibration`: the stack's `use_effects` (an emptied stack has none).
    if !stack.is_empty() && crate::consumable::use_vibrations(&stack) {
        level.game_event(
            events().item_interact_finish,
            at,
            GameEventContext::of(player),
        );
    }
    level.level_event(None, 1505, at_pos, 15);
    set_held(ctx, click.hand, stack);
    Used::Success
}

/// `HoneycombItem.useOn`: waxes a copper block.
fn honeycomb(ctx: &mut PlayContext<'_>, click: Click) -> Used {
    let player = ctx.actor.entity;
    let pos = click.pos;
    let stack = held(ctx, click.hand);
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let old = level.block(pos);
    let Some(waxed) = crate::block::dispenser::waxed(old) else {
        return Used::Pass;
    };
    ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
    let mut stack = held(ctx, click.hand);
    stack.shrink(1);
    set_held(ctx, click.hand, stack);
    let Some(level) = ctx.level.level_mut() else {
        return Used::Success;
    };
    level.set_block(pos, waxed, update::ALL_IMMEDIATE);
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext::with_state(player, waxed),
    );
    level.level_event(player, 3003, pos, 0);
    let sound = sound_id("item.honeycomb.wax_on");
    level.play_block_sound(player, pos, sound, SOUND_BLOCKS, 1.0, 1.0);
    if is_copper_chest(old) && old.value("type").is_some_and(|v| v != "single") {
        let other = connected_chest_pos(pos, old);
        let other_state = level.block(other);
        level.game_event_block(
            events().block_change,
            other,
            GameEventContext::with_state(player, other_state),
        );
        level.level_event(player, 3003, other, 0);
        level.play_block_sound(player, pos, sound, SOUND_BLOCKS, 1.0, 1.0);
    }
    Used::Success
}

/// `ShearsItem.useOn`: a growing plant (kelp, vines, …) stops growing.
fn shears(ctx: &mut PlayContext<'_>, click: Click) -> Used {
    let player = ctx.actor.entity;
    let pos = click.pos;
    let stack = held(ctx, click.hand);
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let state = level.block(pos);
    // `GrowingPlantHeadBlock` that is not at its maximum age.
    let head = matches!(
        state.block().info().kind,
        BlockKind::KelpBlock
            | BlockKind::WeepingVinesBlock
            | BlockKind::TwistingVinesBlock
            | BlockKind::CaveVinesBlock
    );
    if !head || state.int("age").is_none_or(|age| age >= 25) {
        return Used::Pass;
    }
    let max_age = state.set_int("age", 25);
    ctx.trigger(Event::ItemUsedOnBlock(pos, stack));
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let sound = sound_id("block.growing_plant.crop");
    level.play_block_sound(player, pos, sound, SOUND_BLOCKS, 1.0, 1.0);
    level.set_block_and_update(pos, max_age);
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext::with_state(player, max_age),
    );
    hurt_and_break(ctx, click.hand, 1);
    Used::Success
}

/// `FireChargeItem.useOn`: lights a campfire or candle, or places fire on the clicked face.
fn fire_charge(ctx: &mut PlayContext<'_>, click: Click) -> Used {
    let player = ctx.actor.entity;
    let uuid = ctx.actor.uuid;
    let forward = crate::block::place::horizontal_direction_of(ctx.actor.y_rot);
    let lightable = super::can_light(ctx.level.block(click.pos));
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    // Bukkit's `BlockIgniteEvent` (`FIREBALL`): a cancelled use does nothing.
    let target = if lightable {
        click.pos
    } else {
        click.pos.relative(click.face)
    };
    if !crate::block::hook_events::may_ignite(
        level,
        target,
        vanillite_abi::ignite_cause::FIREBALL,
        uuid,
        uuid,
    ) {
        return Used::Fail;
    }
    let used = if lightable {
        let state = level.block(click.pos);
        play_fire_charge_sound(level, click.pos);
        level.set_block_and_update(click.pos, state.set_flag("lit", true));
        level.game_event_at(player, events().block_change, click.pos);
        true
    } else {
        let target = click.pos.relative(click.face);
        let fire = crate::block::place::base_fire_state(level, target);
        let placeable = level.block(target).is_air()
            && (level.can_survive(fire, target)
                || crate::block::portal::lights_portal(level, target, forward));
        if placeable {
            play_fire_charge_sound(level, target);
            level.set_block_and_update(target, fire);
            level.game_event_at(player, events().block_place, target);
        }
        placeable
    };
    if !used {
        return Used::Fail;
    }
    let mut stack = held(ctx, click.hand);
    stack.shrink(1);
    set_held(ctx, click.hand, stack);
    Used::Success
}

/// `FireChargeItem.playSound`.
fn play_fire_charge_sound(level: &mut Level, pos: BlockPos) {
    use vanillite_base::random::RandomSource;
    let a = level.random().next_float();
    let b = level.random().next_float();
    let pitch = (a - b) * 0.2 + 1.0;
    let sound = sound_id("item.firecharge.use");
    level.play_block_sound(None, pos, sound, SOUND_BLOCKS, 1.0, pitch);
}

/// `PotionItem.useOn`: a water bottle turns dirt into mud.
fn water_bottle_on_mud(ctx: &mut PlayContext<'_>, click: Click) -> Used {
    let stack = held(ctx, click.hand);
    let water = vanillite_data::registry::POTION.id("minecraft:water");
    if click.face == Direction::Down || water.is_none() || crate::recipe::potion_of(&stack) != water
    {
        return Used::Pass;
    }
    let pos = click.pos;
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    if !crate::block::block_tags()
        .convertible_to_mud
        .contains(level.block(pos))
    {
        return Used::Pass;
    }
    let splash = sound_id("entity.generic.splash");
    level.play_block_sound(None, pos, splash, SOUND_BLOCKS, 1.0, 1.0);
    filled_result(ctx, click.hand, ItemStack::new(items::GLASS_BOTTLE.0, 1));
    let Some(level) = ctx.level.level_mut() else {
        return Used::Success;
    };
    // `sendParticles(SPLASH, x + nextDouble, y + 1, z + nextDouble, 1, 0, 0, 0, 1)` five
    // times.
    for _ in 0..5 {
        use vanillite_base::random::RandomSource;
        let x = f64::from(pos.x) + level.random().next_double();
        let z = f64::from(pos.z) + level.random().next_double();
        level.send_particles(
            crate::level::particles::simple("minecraft:splash"),
            Vec3::new(x, f64::from(pos.y) + 1.0, z),
            1,
            Vec3::ZERO,
            1.0,
        );
    }
    let empty = sound_id("item.bottle.empty");
    level.play_block_sound(None, pos, empty, SOUND_BLOCKS, 1.0, 1.0);
    level.game_event_at(None, events().fluid_place, pos);
    level.set_block_and_update(pos, blocks::MUD.default_state());
    Used::Success
}

/// `DebugStickItem.handleInteraction`: `cycle` changes the selected property of the block
/// (`useOn`), else the next property is selected (`canDestroyBlock`). Only creative operators
/// can use it.
pub(super) fn debug_stick(
    ctx: &mut PlayContext<'_>,
    pos: BlockPos,
    cycle: bool,
    hand: Hand,
    sink: &dyn super::PacketSink,
) -> Used {
    use vanillite_protocol::packets::play::clientbound::SystemChat;
    use vanillite_protocol::text::Component;
    if !(ctx.actor.creative() && ctx.actor.permission_level >= 2) {
        return Used::Fail;
    }
    let state = ctx.level.block(pos);
    let block = state.block();
    let properties = block.info().properties;
    let message = |_: &mut PlayContext<'_>, key: &str, args: Vec<Component>| {
        sink.send(&SystemChat {
            content: Component::translatable(key, args),
            overlay: true,
        });
    };
    if properties.is_empty() {
        message(
            ctx,
            "item.minecraft.debug_stick.empty",
            vec![Component::text(block.name())],
        );
        return Used::Fail;
    }
    let infos: Vec<&'static vanillite_data::block::PropertyInfo> = properties
        .iter()
        .filter_map(|&p| vanillite_data::block::PROPERTIES.get(usize::from(p)))
        .collect();
    let mut stack = held(ctx, hand);
    let Some(mut selected) = debug_stick_state(&stack) else {
        return Used::Fail;
    };
    let current = selected
        .iter()
        .find(|(b, _)| *b == block.name())
        .and_then(|(_, p)| infos.iter().position(|i| i.name == p));
    let backward = ctx.actor.sneaking;
    let relative = |len: usize, at: Option<usize>| -> usize {
        match at {
            // `Util.findPreviousInIterable` without a current value ends on the last one.
            None if backward => len.saturating_sub(1),
            None => 0,
            Some(i) if backward => i.checked_sub(1).unwrap_or(len.saturating_sub(1)),
            Some(i) => i.saturating_add(1).checked_rem(len).unwrap_or(0),
        }
    };
    if cycle {
        let Some(&property) = infos.get(current.unwrap_or(0)) else {
            return Used::Fail;
        };
        let value_at = property
            .values
            .iter()
            .position(|v| state.value(property.name) == Some(*v));
        let Some(&next) = property
            .values
            .get(relative(property.values.len(), value_at))
        else {
            return Used::Fail;
        };
        let new_state = state.with(property.name, next).unwrap_or(state);
        ctx.level
            .set_block(pos, new_state, update::CLIENTS | update::KNOWN_SHAPE);
        message(
            ctx,
            "item.minecraft.debug_stick.update",
            vec![Component::text(property.name), Component::text(next)],
        );
    } else {
        let Some(&property) = infos.get(relative(infos.len(), current)) else {
            return Used::Fail;
        };
        selected.retain(|(b, _)| *b != block.name());
        selected.push((block.name().to_owned(), property.name.to_owned()));
        stack.set_component(ids::DEBUG_STICK_STATE, encode_debug_stick_state(&selected));
        set_held(ctx, hand, stack);
        let value = state.value(property.name).unwrap_or_default();
        message(
            ctx,
            "item.minecraft.debug_stick.select",
            vec![Component::text(property.name), Component::text(value)],
        );
    }
    Used::Success
}

/// The `minecraft:debug_stick_state` component: block name to selected property name. It has
/// only a persistent codec, so the wire carries it as one NBT compound
/// (`{"minecraft:<block>": "<property>"}`).
fn debug_stick_state(stack: &ItemStack) -> Option<Vec<(String, String)>> {
    let Some(data) = stack.component(ids::DEBUG_STICK_STATE) else {
        // The debug stick's default component is an empty map.
        return (item::item_id(stack) == items::DEBUG_STICK).then(Vec::new);
    };
    let mut input = data;
    let mut budget = vanillite_nbt::Budget::file();
    let Ok(Some(vanillite_nbt::Tag::Compound(c))) =
        vanillite_nbt::read::read_network(&mut input, &mut budget)
    else {
        return None;
    };
    Some(
        c.iter()
            .filter_map(|(block, property)| match property {
                vanillite_nbt::Tag::String(p) => Some((block.to_owned(), p.clone())),
                _ => None,
            })
            .collect(),
    )
}

fn encode_debug_stick_state(selected: &[(String, String)]) -> Box<[u8]> {
    let mut c = vanillite_nbt::Compound::new();
    for (block, property) in selected {
        if BlockId::by_name(block).is_some() {
            c.insert(block.clone(), vanillite_nbt::Tag::String(property.clone()));
        }
    }
    let mut out = Vec::new();
    let _ = vanillite_nbt::write::write_network_compound(&c, &mut out);
    out.into()
}

// --- Adventure mode --------------------------------------------------------------------------

/// `ItemStack.canPlaceOnBlockInAdventureMode`.
pub(super) fn can_place_on(ctx: &PlayContext<'_>, stack: &ItemStack, pos: BlockPos) -> bool {
    adventure_allows_state(
        ctx.data,
        ctx.level.level(),
        ctx.level.block(pos),
        pos,
        stack.component(ids::CAN_PLACE_ON),
    )
}

/// `ItemStack.canBreakBlockInAdventureMode`.
pub(super) fn can_break(ctx: &PlayContext<'_>, stack: &ItemStack, pos: BlockPos) -> bool {
    adventure_allows_state(
        ctx.data,
        ctx.level.level(),
        ctx.level.block(pos),
        pos,
        stack.component(ids::CAN_BREAK),
    )
}

/// `AdventureModePredicate.test` of an encoded `can_place_on` / `can_break` component: any of
/// its block predicates matches the block at `pos`.
pub fn adventure_allows(
    data: &super::GameData,
    level: Option<&Level>,
    component: Option<&[u8]>,
    pos: BlockPos,
) -> bool {
    let Some(level) = level else {
        return false;
    };
    adventure_allows_state(data, Some(level), level.block(pos), pos, component)
}

fn adventure_allows_state(
    data: &super::GameData,
    level: Option<&Level>,
    state: BlockState,
    pos: BlockPos,
    component: Option<&[u8]>,
) -> bool {
    let Some(component) = component else {
        return false;
    };
    let mut r = Reader::new(component);
    let Ok(n) = r.varint() else {
        return false;
    };
    for _ in 0..n.clamp(0, 256) {
        match block_predicate(data, level, &mut r, state, pos) {
            Some(true) => return true,
            Some(false) => {}
            None => return false,
        }
    }
    false
}

/// `BlockPredicate.matches` of one encoded predicate (`None` if it cannot be read).
fn block_predicate(
    data: &super::GameData,
    level: Option<&Level>,
    r: &mut Reader<'_>,
    state: BlockState,
    pos: BlockPos,
) -> Option<bool> {
    let mut matches = true;
    if r.u8().ok()? != 0 {
        let n = r.varint().ok()?;
        if n == 0 {
            let tag = r.string(32767).ok()?;
            matches &= data.block_in_tag(state.block().0, tag);
        } else {
            let mut any = false;
            for _ in 0..n.saturating_sub(1).clamp(0, 4096) {
                any |= r.varint().ok()? == i32::from(state.block().0);
            }
            matches &= any;
        }
    }
    if r.u8().ok()? != 0 {
        let n = r.varint().ok()?;
        for _ in 0..n.clamp(0, 256) {
            let name = r.string(32767).ok()?.to_owned();
            let value = state.value(&name);
            if r.u8().ok()? != 0 {
                let exact = r.string(32767).ok()?;
                matches &= value == Some(exact);
            } else {
                let min = if r.u8().ok()? != 0 {
                    Some(r.string(32767).ok()?.to_owned())
                } else {
                    None
                };
                let max = if r.u8().ok()? != 0 {
                    Some(r.string(32767).ok()?.to_owned())
                } else {
                    None
                };
                matches &= value.is_some_and(|v| in_range(state, &name, v, min, max));
            }
        }
    }
    if r.u8().ok()? != 0 {
        let mut input = r.rest();
        let mut budget = vanillite_nbt::read::Budget::new(1 << 16, 64);
        let tag = vanillite_nbt::read::read_network(&mut input, &mut budget).ok()??;
        let consumed = r.rest().len().saturating_sub(input.len());
        r.skip(consumed).ok()?;
        let entity = level.and_then(|level| level.block_entity_nbt(pos));
        matches &= entity.is_some_and(|e| nbt_contains(&Tag::Compound(e), &tag));
    }
    // Component matchers: `matches(BlockInWorld)` tests only the state and the NBT, so
    // `AdventureModePredicate` ignores them.
    let exact = r.varint().ok()?;
    for _ in 0..exact.clamp(0, 256) {
        let type_id = u16::try_from(r.varint().ok()?).ok()?;
        let len = vanillite_protocol::components::value_len(type_id, r.rest()).ok()?;
        r.skip(len).ok()?;
    }
    let partial = r.varint().ok()?;
    for _ in 0..partial.clamp(0, 64) {
        // A predicate type or a component type, then the value as NBT.
        r.u8().ok()?;
        r.varint().ok()?;
        let mut input = r.rest();
        let mut budget = vanillite_nbt::read::Budget::new(1 << 16, 64);
        vanillite_nbt::read::read_network(&mut input, &mut budget).ok()?;
        let consumed = r.rest().len().saturating_sub(input.len());
        r.skip(consumed).ok()?;
    }
    Some(matches)
}

use vanillite_nbt::Tag;

/// `StatePropertiesPredicate.RangedMatcher`: the value lies between the bounds (by the
/// property's value order, as `Property.getValue(name)` comparables).
fn in_range(
    state: BlockState,
    name: &str,
    value: &str,
    min: Option<String>,
    max: Option<String>,
) -> bool {
    let Some(info) = state
        .block()
        .info()
        .properties
        .iter()
        .filter_map(|&p| vanillite_data::block::PROPERTIES.get(usize::from(p)))
        .find(|p| p.name == name)
    else {
        return false;
    };
    let order = |v: &str| -> Option<i64> {
        match v.parse::<i64>() {
            Ok(n) => Some(n),
            Err(_) => info
                .values
                .iter()
                .position(|x| *x == v)
                .and_then(|i| i64::try_from(i).ok()),
        }
    };
    let Some(v) = order(value) else {
        return false;
    };
    if let Some(min) = min
        && order(&min).is_none_or(|m| v < m)
    {
        return false;
    }
    if let Some(max) = max
        && order(&max).is_none_or(|m| v > m)
    {
        return false;
    }
    true
}

/// `NbtUtils.compareNbt(expected, actual, true)`: `actual` holds everything in `expected`.
fn nbt_contains(actual: &Tag, expected: &Tag) -> bool {
    match (expected, actual) {
        (Tag::Compound(e), Tag::Compound(a)) => e
            .iter()
            .all(|(k, v)| a.get(k).is_some_and(|av| nbt_contains(av, v))),
        (Tag::List(e), Tag::List(a)) => {
            e.0.is_empty() && a.0.is_empty()
                || e.0
                    .iter()
                    .all(|ev| a.0.iter().any(|av| nbt_contains(av, ev)))
        }
        (e, a) => e == a,
    }
}

// --- use -------------------------------------------------------------------------------------

/// The first block the player looks at within reach (`Item.getPlayerPOVHitResult`): its
/// position, the face and the hit location.
pub(super) fn pov_hit(
    ctx: &PlayContext<'_>,
    y_rot: f32,
    x_rot: f32,
    source_fluids: bool,
) -> Option<(BlockPos, Direction, Vec3)> {
    let from = ctx.actor.eye();
    let view = view_vector(x_rot, y_rot);
    let range = ctx.actor.block_range();
    let to = Vec3::new(
        from.x + view.x * range,
        from.y + view.y * range,
        from.z + view.z * range,
    );
    clip_outline(&*ctx.level, from, to, source_fluids)
}

/// `Entity.calculateViewVector`.
fn view_vector(x_rot: f32, y_rot: f32) -> Vec3 {
    use vanillite_base::math::{cos, sin};
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let degrees = (std::f64::consts::PI / 180.0) as f32;
    let x = x_rot * degrees;
    let y = -y_rot * degrees;
    let ycos = cos(f64::from(y));
    let ysin = sin(f64::from(y));
    let xcos = cos(f64::from(x));
    let xsin = sin(f64::from(x));
    Vec3::new(
        f64::from(ysin * xcos),
        f64::from(-xsin),
        f64::from(ycos * xcos),
    )
}

/// `Level.clip` with `ClipContext.Block.OUTLINE` and `Fluid.NONE` or `SOURCE_ONLY`.
fn clip_outline(
    level: &dyn super::PlayLevel,
    from: Vec3,
    to: Vec3,
    source_fluids: bool,
) -> Option<(BlockPos, Direction, Vec3)> {
    use vanillite_base::math::floor;
    if from == to {
        return None;
    }
    let lerp = |t: f64, a: f64, b: f64| a + t * (b - a);
    let to_x = lerp(-1.0e-7, to.x, from.x);
    let to_y = lerp(-1.0e-7, to.y, from.y);
    let to_z = lerp(-1.0e-7, to.z, from.z);
    let from_x = lerp(-1.0e-7, from.x, to.x);
    let from_y = lerp(-1.0e-7, from.y, to.y);
    let from_z = lerp(-1.0e-7, from.z, to.z);
    let hit = |pos: BlockPos| -> Option<(BlockPos, Direction, Vec3)> {
        let state = level.block(pos);
        let (ox, oy, oz) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
        let boxes: Vec<Aabb> = state
            .outline_shape()
            .iter()
            .map(|b| {
                Aabb::new(
                    ox + b[0],
                    oy + b[1],
                    oz + b[2],
                    ox + b[3],
                    oy + b[4],
                    oz + b[5],
                )
            })
            .collect();
        let block_hit = clip_boxes(&boxes, from, to);
        let fluid = state.fluid_state();
        let fluid_hit = if source_fluids && !fluid.is_empty() && fluid.info().source {
            let above = level.block(pos.relative(Direction::Up)).fluid_state();
            let height = if !above.is_empty() && above.info().fluid == fluid.info().fluid {
                1.0
            } else {
                f64::from(fluid.info().own_height)
            };
            clip_boxes(
                &[Aabb::new(ox, oy, oz, ox + 1.0, oy + height, oz + 1.0)],
                from,
                to,
            )
        } else {
            None
        };
        let dist = |h: &Option<(Direction, Vec3)>| {
            h.map_or(f64::MAX, |(_, p)| {
                let (dx, dy, dz) = (p.x - from.x, p.y - from.y, p.z - from.z);
                dx * dx + dy * dy + dz * dz
            })
        };
        let best = if dist(&block_hit) <= dist(&fluid_hit) {
            block_hit
        } else {
            fluid_hit
        };
        best.map(|(d, p)| (pos, d, p))
    };
    let mut bx = floor(from_x);
    let mut by = floor(from_y);
    let mut bz = floor(from_z);
    if let Some(h) = hit(BlockPos::new(bx, by, bz)) {
        return Some(h);
    }
    let (dx, dy, dz) = (to_x - from_x, to_y - from_y, to_z - from_z);
    let sign = |d: f64| -> i32 {
        if d > 0.0 {
            1
        } else if d < 0.0 {
            -1
        } else {
            0
        }
    };
    let frac = |v: f64| v - v.floor();
    let (sx, sy, sz) = (sign(dx), sign(dy), sign(dz));
    let delta = |s: i32, d: f64| {
        if s == 0 { f64::MAX } else { f64::from(s) / d }
    };
    let (tdx, tdy, tdz) = (delta(sx, dx), delta(sy, dy), delta(sz, dz));
    let start = |s: i32, td: f64, f: f64| td * if s > 0 { 1.0 - frac(f) } else { frac(f) };
    let mut tx = start(sx, tdx, from_x);
    let mut ty = start(sy, tdy, from_y);
    let mut tz = start(sz, tdz, from_z);
    while tx <= 1.0 || ty <= 1.0 || tz <= 1.0 {
        if tx < ty {
            if tx < tz {
                bx = bx.wrapping_add(sx);
                tx += tdx;
            } else {
                bz = bz.wrapping_add(sz);
                tz += tdz;
            }
        } else if ty < tz {
            by = by.wrapping_add(sy);
            ty += tdy;
        } else {
            bz = bz.wrapping_add(sz);
            tz += tdz;
        }
        if let Some(h) = hit(BlockPos::new(bx, by, bz)) {
            return Some(h);
        }
    }
    None
}

/// `VoxelShape.clip`: the face and point where the segment enters the boxes (an inside start
/// hits the face facing the ray).
fn clip_boxes(boxes: &[Aabb], from: Vec3, to: Vec3) -> Option<(Direction, Vec3)> {
    if boxes.is_empty() {
        return None;
    }
    let d = Vec3::new(to.x - from.x, to.y - from.y, to.z - from.z);
    if d.x * d.x + d.y * d.y + d.z * d.z < 1.0e-7 {
        return None;
    }
    let test = Vec3::new(
        from.x + d.x * 0.001,
        from.y + d.y * 0.001,
        from.z + d.z * 0.001,
    );
    let inside = boxes.iter().any(|b| {
        test.x >= b.min_x
            && test.x < b.max_x
            && test.y >= b.min_y
            && test.y < b.max_y
            && test.z >= b.min_z
            && test.z < b.max_z
    });
    if inside {
        return Some((nearest_direction(d).opposite(), test));
    }
    let mut scale = 1.0;
    let mut direction = None;
    for b in boxes {
        if let Some(found) = aabb_direction(b, from, &mut scale, d) {
            direction = Some(found);
        }
    }
    direction.map(|dir| {
        (
            dir,
            Vec3::new(
                from.x + d.x * scale,
                from.y + d.y * scale,
                from.z + d.z * scale,
            ),
        )
    })
}

/// `Direction.getApproximateNearest`.
fn nearest_direction(d: Vec3) -> Direction {
    let mut best = Direction::North;
    let mut max = f64::MIN;
    for direction in Direction::ALL {
        let [x, y, z] = direction.normal();
        let dot = d.x * f64::from(x) + d.y * f64::from(y) + d.z * f64::from(z);
        if dot > max {
            max = dot;
            best = direction;
        }
    }
    best
}

/// `AABB.getDirection`: the face of `b` the ray enters first, if earlier than `scale`.
fn aabb_direction(b: &Aabb, from: Vec3, scale: &mut f64, d: Vec3) -> Option<Direction> {
    let mut dir = None;
    let mut clip = |da: f64,
                    db: f64,
                    dc: f64,
                    point: f64,
                    (min_b, max_b): (f64, f64),
                    (min_c, max_c): (f64, f64),
                    new: Direction,
                    (a, b0, c): (f64, f64, f64)| {
        let s = (point - a) / da;
        let pb = b0 + s * db;
        let pc = c + s * dc;
        if 0.0 < s
            && s < *scale
            && min_b - 1.0e-7 < pb
            && pb < max_b + 1.0e-7
            && min_c - 1.0e-7 < pc
            && pc < max_c + 1.0e-7
        {
            *scale = s;
            dir = Some(new);
        }
    };
    let (fx, fy, fz) = (from.x, from.y, from.z);
    if d.x > 1.0e-7 {
        clip(
            d.x,
            d.y,
            d.z,
            b.min_x,
            (b.min_y, b.max_y),
            (b.min_z, b.max_z),
            Direction::West,
            (fx, fy, fz),
        );
    } else if d.x < -1.0e-7 {
        clip(
            d.x,
            d.y,
            d.z,
            b.max_x,
            (b.min_y, b.max_y),
            (b.min_z, b.max_z),
            Direction::East,
            (fx, fy, fz),
        );
    }
    if d.y > 1.0e-7 {
        clip(
            d.y,
            d.z,
            d.x,
            b.min_y,
            (b.min_z, b.max_z),
            (b.min_x, b.max_x),
            Direction::Down,
            (fy, fz, fx),
        );
    } else if d.y < -1.0e-7 {
        clip(
            d.y,
            d.z,
            d.x,
            b.max_y,
            (b.min_z, b.max_z),
            (b.min_x, b.max_x),
            Direction::Up,
            (fy, fz, fx),
        );
    }
    if d.z > 1.0e-7 {
        clip(
            d.z,
            d.x,
            d.y,
            b.min_z,
            (b.min_x, b.max_x),
            (b.min_y, b.max_y),
            Direction::North,
            (fz, fx, fy),
        );
    } else if d.z < -1.0e-7 {
        clip(
            d.z,
            d.x,
            d.y,
            b.max_z,
            (b.min_x, b.max_x),
            (b.min_y, b.max_y),
            Direction::South,
            (fz, fx, fy),
        );
    }
    dir
}

/// What `Item.use` of a handled item did; `Place` asks the caller to place the held block item
/// at the position above the hit water (`PlaceOnWaterBlockItem.use`).
pub(super) enum UseAir {
    Done(Used),
    PlaceOnWater(BlockPos, Direction, Vec3),
}

/// `Item.use` of buckets, glass bottles, lily pads and frogspawn, and swappable equipment
/// (`None` for items handled elsewhere).
pub(super) fn use_item(
    ctx: &mut PlayContext<'_>,
    hand: Hand,
    y_rot: f32,
    x_rot: f32,
) -> Option<UseAir> {
    let stack = held(ctx, hand);
    if stack.is_empty() {
        return None;
    }
    let id = item::item_id(&stack);
    ctx.level.level_mut()?;
    if let Some(content) = bucket_content(id) {
        return Some(UseAir::Done(bucket(ctx, hand, y_rot, x_rot, content)));
    }
    if id == items::GLASS_BOTTLE {
        return Some(UseAir::Done(glass_bottle(ctx, hand, y_rot, x_rot)));
    }
    if let Some(kind) = crate::mob::animal::spawn_egg_kind(&stack) {
        return Some(UseAir::Done(spawn_egg_in_liquid(
            ctx, hand, y_rot, x_rot, kind,
        )));
    }
    if id == items::LILY_PAD || id == items::FROGSPAWN {
        let (pos, face, hit) = pov_hit(ctx, y_rot, x_rot, true)?;
        return Some(UseAir::PlaceOnWater(pos.relative(Direction::Up), face, hit));
    }
    if item::equippable(&stack).is_some_and(|e| e.swappable)
        && stack.component(ids::CONSUMABLE).is_none()
    {
        return Some(UseAir::Done(swap_with_equipment_slot(ctx, hand)));
    }
    None
}

/// What a bucket item holds (`BucketItem.content`, mob buckets with their mob).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Content {
    Empty,
    Water,
    Lava,
    Mob(crate::mob::Kind),
}

fn bucket_content(id: vanillite_data::item::ItemId) -> Option<Content> {
    use crate::mob::Kind;
    Some(match id {
        _ if id == items::BUCKET => Content::Empty,
        _ if id == items::WATER_BUCKET => Content::Water,
        _ if id == items::LAVA_BUCKET => Content::Lava,
        _ if id == items::COD_BUCKET => Content::Mob(Kind::Cod),
        _ if id == items::SALMON_BUCKET => Content::Mob(Kind::Salmon),
        _ if id == items::PUFFERFISH_BUCKET => Content::Mob(Kind::Pufferfish),
        _ if id == items::TROPICAL_FISH_BUCKET => Content::Mob(Kind::TropicalFish),
        _ if id == items::AXOLOTL_BUCKET => Content::Mob(Kind::Axolotl),
        _ if id == items::TADPOLE_BUCKET => Content::Mob(Kind::Tadpole),
        _ if id == items::SULFUR_CUBE_BUCKET => Content::Mob(Kind::SulfurCube),
        _ => return None,
    })
}

/// `SpawnEggItem.use`: looking at a water or lava source (`SOURCE_ONLY`), the egg's mob
/// spawns inside it without moving down.
fn spawn_egg_in_liquid(
    ctx: &mut PlayContext<'_>,
    hand: Hand,
    y_rot: f32,
    x_rot: f32,
    kind: crate::mob::Kind,
) -> Used {
    let Some((pos, face, _)) = pov_hit(ctx, y_rot, x_rot, true) else {
        return Used::Pass;
    };
    // `EntityType.canSpawn`: no monsters in Peaceful (the egg is kept).
    let peaceful = ctx
        .level
        .level()
        .is_some_and(|l| l.spawner_env.difficulty == crate::survival::rules::Difficulty::Peaceful);
    if peaceful && !kind.allowed_in_peaceful() {
        return Used::Fail;
    }
    let state = ctx.level.block(pos);
    if !state.is(blocks::WATER) && !state.is(blocks::LAVA) {
        return Used::Pass;
    }
    let stack = held(ctx, hand);
    // `level.mayInteract` and `player.mayUseItemAt(pos, face, stack)`.
    let may_build = ctx.actor.game_mode != vanillite_config::GameMode::Adventure
        || can_place_on(ctx, &stack, pos.relative(face.opposite()));
    if !may_interact(ctx, pos) || !may_build {
        return Used::Fail;
    }
    let player = ctx.actor.entity;
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let reason = crate::mob::fam::SpawnReason::SpawnItemUse;
    if crate::mob::animal::spawn_from_item(level, kind, &stack, pos, reason, false, false).is_none()
    {
        return Used::Fail;
    }
    level.game_event_at(player, events().entity_place, pos);
    if !ctx.actor.creative() {
        let mut left = stack.clone();
        left.shrink(1);
        set_held(ctx, hand, left);
    }
    used_stat(ctx, stack.item);
    Used::Success
}

/// `ServerLevel.mayInteract`: not spawn protected and inside the world border.
fn may_interact(ctx: &PlayContext<'_>, pos: BlockPos) -> bool {
    !(ctx.spawn_protected)(pos) && ctx.level.within_border(pos)
}

/// The mob of a mob bucket item.
pub(crate) fn mob_bucket_kind(id: vanillite_data::item::ItemId) -> Option<crate::mob::Kind> {
    match bucket_content(id)? {
        Content::Mob(kind) => Some(kind),
        _ => None,
    }
}

/// `BucketItem.use` (and `MobBucketItem`).
fn bucket(ctx: &mut PlayContext<'_>, hand: Hand, y_rot: f32, x_rot: f32, content: Content) -> Used {
    let Some((pos, face, _)) = pov_hit(ctx, y_rot, x_rot, content == Content::Empty) else {
        return Used::Pass;
    };
    let player = ctx.actor.entity;
    let stack = held(ctx, hand);
    let relative = pos.relative(face);
    // `level.mayInteract` (spawn protection, world border) and `player.mayUseItemAt`, which
    // tests `can_place_on` against the clicked block.
    let may_build = ctx.actor.game_mode != vanillite_config::GameMode::Adventure
        || can_place_on(ctx, &stack, pos);
    if !may_interact(ctx, pos) || !may_build || !ctx.level.is_loaded(pos) {
        return Used::Fail;
    }
    let sneaking = ctx.actor.sneaking;
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let clicked = level.block(pos);
    let fluid = match content {
        Content::Lava => Some(blocks::LAVA),
        // `MobBucketItem` of `Fluids.EMPTY`: nothing is placed.
        Content::Mob(crate::mob::Kind::SulfurCube) | Content::Empty => None,
        Content::Water | Content::Mob(_) => Some(blocks::WATER),
    };
    let place_pos =
        if fluid == Some(blocks::WATER) && crate::block::dispenser::is_liquid_container(clicked) {
            pos
        } else {
            relative
        };
    let emptied = if let Content::Mob(kind) = content
        && fluid.is_none()
    {
        // `MobBucketItem.emptyContents` without a fluid: only the empty sound.
        let sound = sound_id(mob_bucket_sound(kind));
        level.play_block_sound(player, place_pos, sound, SOUND_NEUTRAL, 1.0, 1.0);
        true
    } else {
        fluid.is_some_and(|fluid| {
            crate::block::dispenser::empty_bucket_contents_by(
                level,
                player,
                fluid,
                place_pos,
                Some((pos, face)),
                sneaking,
                match content {
                    Content::Mob(kind) => Some(mob_bucket_sound(kind)),
                    _ => None,
                },
            )
        })
    };
    if emptied {
        if let Content::Mob(kind) = content {
            release_mob(level, kind, &stack, place_pos);
            level.game_event_at(player, events().entity_place, place_pos);
        }
        if fluid.is_some() {
            ctx.trigger(Event::PlacedBlock(place_pos, stack.clone()));
        }
        used_stat(ctx, stack.item);
        let empty = if ctx.actor.creative() {
            stack
        } else {
            ItemStack::new(items::BUCKET.0, 1)
        };
        filled_result(ctx, hand, empty);
        return Used::Success;
    }
    if content == Content::Empty {
        let state = level.block(pos);
        let creative = ctx.actor.creative();
        if let Some(filled) = crate::block::dispenser::pickup_block(level, pos, creative) {
            let taken = ItemStack::new(filled.0, 1);
            used_stat(ctx, stack.item);
            let sound = pickup_sound(state);
            let at = ctx.actor.pos;
            if let Some(level) = ctx.level.level_mut() {
                level.play_sound(
                    player,
                    at.x,
                    at.y,
                    at.z,
                    sound_id(sound),
                    SOUND_PLAYERS,
                    1.0,
                    1.0,
                );
                level.game_event_at(player, events().fluid_pickup, pos);
            }
            filled_result(ctx, hand, taken.clone());
            ctx.trigger(Event::Item(TriggerKind::FILLED_BUCKET, taken));
            return Used::Success;
        }
    }
    Used::Fail
}

/// `BucketPickup.getPickupSound`.
fn pickup_sound(state: BlockState) -> &'static str {
    if state.is(blocks::LAVA) {
        "item.bucket.fill_lava"
    } else if state.is(blocks::POWDER_SNOW) {
        "item.bucket.fill_powder_snow"
    } else {
        "item.bucket.fill"
    }
}

/// `MobBucketItem.emptySound`.
pub(crate) fn mob_bucket_sound(kind: crate::mob::Kind) -> &'static str {
    match kind {
        crate::mob::Kind::Axolotl => "item.bucket.empty_axolotl",
        crate::mob::Kind::Tadpole => "item.bucket.empty_tadpole",
        crate::mob::Kind::SulfurCube => "item.bucket.empty_sulfur_cube",
        _ => "item.bucket.empty_fish",
    }
}

/// `MobBucketItem.spawn`: the mob of the bucket, with the bucket's data, marked as from a
/// bucket, plays its ambient sound.
pub(crate) fn release_mob(
    level: &mut Level,
    kind: crate::mob::Kind,
    stack: &ItemStack,
    pos: BlockPos,
) {
    crate::mob::animal::spawn_from_item(
        level,
        kind,
        stack,
        pos,
        crate::mob::fam::SpawnReason::Bucket,
        true,
        false,
    );
}

/// `BottleItem.use`: dragon's breath from a dragon's cloud around the player, else water.
fn glass_bottle(ctx: &mut PlayContext<'_>, hand: Hand, y_rot: f32, x_rot: f32) -> Used {
    let player = ctx.actor.entity;
    let at = ctx.actor.pos;
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    let own = player
        .and_then(|p| level.entities.store.get(p).map(|e| e.aabb()))
        .unwrap_or_else(|| {
            Aabb::new(
                at.x - 0.3,
                at.y,
                at.z - 0.3,
                at.x + 0.3,
                at.y + 1.8,
                at.z + 0.3,
            )
        });
    let area = Aabb::new(
        own.min_x - 2.0,
        own.min_y - 2.0,
        own.min_z - 2.0,
        own.max_x + 2.0,
        own.max_y + 2.0,
        own.max_z + 2.0,
    );
    let mut clouds = Vec::new();
    level
        .entities
        .store
        .for_each_in(&area, false, |id| clouds.push(id));
    let cloud = clouds.into_iter().find(|&id| {
        crate::effect::cloud::owner(&mut level.entities, id).is_some_and(|o| {
            level
                .entities
                .mob(o)
                .is_some_and(|m| m.kind == crate::mob::Kind::EnderDragon)
        })
    });
    if let Some(cloud) = cloud {
        let radius = level.entities.cloud(cloud).map_or(0.0, |c| c.radius);
        crate::effect::cloud::set_radius(&mut level.entities, cloud, radius - 0.5);
        let sound = sound_id("item.bottle.fill_dragonbreath");
        level.play_sound(None, at.x, at.y, at.z, sound, SOUND_NEUTRAL, 1.0, 1.0);
        level.game_event(events().fluid_pickup, at, GameEventContext::of(player));
        let target = crate::advancements::view::snapshot(&level.entities, cloud);
        let stack = held(ctx, hand);
        ctx.trigger(Event::EntityItem(
            TriggerKind::PLAYER_INTERACTED_WITH_ENTITY,
            target,
            stack.clone(),
        ));
        used_stat(ctx, stack.item);
        filled_result(ctx, hand, ItemStack::new(items::DRAGON_BREATH.0, 1));
        return Used::Success;
    }
    let Some((pos, _, _)) = pov_hit(ctx, y_rot, x_rot, true) else {
        return Used::Pass;
    };
    if !may_interact(ctx, pos) {
        return Used::Pass;
    }
    let Some(level) = ctx.level.level_mut() else {
        return Used::Pass;
    };
    if !crate::block::fluid::is_water(level.fluid(pos)) {
        return Used::Pass;
    }
    let sound = sound_id("item.bottle.fill");
    level.play_sound(player, at.x, at.y, at.z, sound, SOUND_NEUTRAL, 1.0, 1.0);
    level.game_event_at(player, events().fluid_pickup, pos);
    let stack = held(ctx, hand);
    used_stat(ctx, stack.item);
    filled_result(ctx, hand, crate::block::dispenser::water_potion());
    Used::Success
}

/// `Equippable.swapWithEquipmentSlot`: armour and elytra go on the player, the worn item into
/// the hand.
fn swap_with_equipment_slot(ctx: &mut PlayContext<'_>, hand: Hand) -> Used {
    let in_hand = held(ctx, hand);
    // `canBeEquippedBy(player)`.
    if !item::equippable(&in_hand)
        .is_some_and(|e| e.can_be_equipped_by(vanillite_data::entity::ids::PLAYER.0))
    {
        return Used::Pass;
    }
    let slot = item::equipment_slot(&in_hand);
    let index = match slot {
        EquipmentSlot::Feet => 36,
        EquipmentSlot::Legs => 37,
        EquipmentSlot::Chest => 38,
        EquipmentSlot::Head => 39,
        _ => return Used::Pass,
    };
    let worn = ctx.inventory.get(index).clone();
    let creative = ctx.actor.creative();
    let bound = ctx.data.enchantment_level(&worn, "minecraft:binding_curse") > 0;
    if (bound && !creative) || in_hand.same_item_same_components(&worn) {
        return Used::Fail;
    }
    used_stat(ctx, in_hand.item);
    let equipped;
    if in_hand.count <= 1 {
        let to_hand = if worn.is_empty() {
            in_hand.clone()
        } else {
            worn.clone()
        };
        equipped = in_hand.clone();
        ctx.inventory.set(index, equipped.clone());
        if creative {
            // `inHand.copy()`: the hand keeps its stack.
            if !worn.is_empty() {
                set_held(ctx, hand, worn.clone());
            }
        } else if worn.is_empty() {
            set_held(ctx, hand, ItemStack::EMPTY);
        } else {
            set_held(ctx, hand, to_hand);
        }
    } else {
        let mut rest = in_hand.clone();
        equipped = rest.split(1);
        if creative {
            rest = in_hand.clone();
        }
        ctx.inventory.set(index, equipped.clone());
        set_held(ctx, hand, rest);
        let mut to_inventory = worn.clone();
        if !to_inventory.is_empty() && !ctx.inventory.add(None, &mut to_inventory, false) {
            ctx.drops.push(Drop {
                stack: to_inventory,
                thrown: false,
            });
        }
    }
    on_equip_item(ctx, slot, &worn, &equipped);
    Used::Success
}

/// `LivingEntity.onEquipItem` of a player's armour slot: the equip sound of an item made for
/// the slot, and `equip` (or `unequip` for an item that is not equippable).
fn on_equip_item(ctx: &mut PlayContext<'_>, slot: EquipmentSlot, old: &ItemStack, new: &ItemStack) {
    if ctx.actor.game_mode == vanillite_config::GameMode::Spectator
        || old.same_item_same_components(new)
    {
        return;
    }
    let player = ctx.actor.entity;
    let at = ctx.actor.pos;
    let Some(level) = ctx.level.level_mut() else {
        return;
    };
    let equippable = item::equippable(new);
    if let Some(e) = equippable.as_ref().filter(|e| e.slot == slot) {
        let id = vanillite_data::registry::SOUND_EVENT
            .id(&e.equip_sound)
            .unwrap_or(0);
        level.play_sound(None, at.x, at.y, at.z, id, SOUND_PLAYERS, 1.0, 1.0);
    }
    let equippable = equippable.is_some();
    // `Player.doesEmitEquipEvent`: armour slots only, which these all are.
    let event = if equippable {
        events().equip
    } else {
        events().unequip
    };
    level.game_event(event, at, GameEventContext::of(player));
}

/// `onEquipItem` of the armour slots the player's menu clicks set (`ArmorSlot.setByPlayer`).
pub(crate) fn flush_equips(ctx: &mut PlayContext<'_>) {
    if ctx.inventory.equips.is_empty() {
        return;
    }
    for (slot, old, new) in std::mem::take(&mut ctx.inventory.equips) {
        on_equip_item(ctx, slot, &old, &new);
    }
}
