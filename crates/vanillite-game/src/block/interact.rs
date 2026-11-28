//! Smaller block interactions: flower pots (`FlowerPotBlock`), carving pumpkins
//! (`PumpkinBlock`), picking glow berries and sweet berries (`CaveVines`,
//! `SweetBerryBushBlock`), the dragon egg (`DragonEggBlock`), light blocks (`LightBlock`) and
//! copper golem statues (`CopperGolemStatueBlock`).

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockKind, BlockState, ids};
use vanillite_data::item::ids as items;
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::StateExt;

use super::{BlockBehaviour, Interaction, ItemInteraction, UseContext, sound_id};
use crate::item::{self, ItemStack};
use crate::level::game_event::{GameEventContext, events};
use crate::level::{Level, SOUND_BLOCKS, update};
use crate::stats::{Stat, StatType};

/// Fills the behaviours of this module's blocks.
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    match block.info().kind {
        BlockKind::FlowerPotBlock => {
            b.use_item_on = Some(flower_pot_use_item_on);
            b.use_without_item = Some(flower_pot_use_without_item);
        }
        BlockKind::PumpkinBlock => b.use_item_on = Some(pumpkin_use_item_on),
        BlockKind::CaveVinesBlock | BlockKind::CaveVinesPlantBlock => {
            b.use_without_item =
                Some(|level, state, pos, ctx| cave_vines_use(level, Some(ctx.player), state, pos));
        }
        BlockKind::SweetBerryBushBlock => {
            b.use_item_on = Some(sweet_berry_use_item_on);
            b.use_without_item = Some(sweet_berry_use_without_item);
        }
        BlockKind::DragonEggBlock => {
            b.use_without_item = Some(|level, state, pos, _| {
                dragon_egg_teleport(level, state, pos);
                Interaction::Success
            });
            b.attack = Some(|level, state, pos, _| dragon_egg_teleport(level, state, pos));
        }
        BlockKind::LightBlock => b.use_without_item = Some(light_use_without_item),
        BlockKind::CopperGolemStatueBlock | BlockKind::WeatheringCopperGolemStatueBlock => {
            b.use_item_on = Some(statue_use_item_on);
            // `affectNeighborsAfterRemoval`: comparators behind a block re-read the pose.
            b.affect_neighbors_after_removal = Some(|level, state, pos, _| {
                level.update_neighbour_for_output_signal(pos, state.block());
            });
        }
        _ => {}
    }
}

/// `CopperGolemStatueBlockEntity.removeStatue`: a copper golem stands where the statue was,
/// facing its way (`playSpawnSound`).
fn revive_statue(level: &mut Level, state: BlockState, pos: BlockPos) {
    use crate::mob::{Kind, MobData};
    let y_rot = match state.direction("facing").unwrap_or(Direction::South) {
        Direction::West => 90.0,
        Direction::North => 180.0,
        Direction::East => 270.0,
        _ => 0.0,
    };
    let mut golem = MobData::new(Kind::CopperGolem, crate::mob::new_entity_seed());
    golem.y_body_rot = y_rot;
    let at = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    );
    level.play_sound(
        None,
        at.x,
        at.y,
        at.z,
        sound_id("entity.copper_golem.spawn"),
        crate::level::SOUND_NEUTRAL,
        1.0,
        1.0,
    );
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    let rot = vanillite_world::entity::Rotation {
        y_rot,
        x_rot: 0.0,
        y_head_rot: y_rot,
    };
    let id = crate::mob::spawn(
        &mut level.entities,
        &network_ids,
        crate::mob::new_uuid(),
        at,
        rot,
        golem,
    );
    level
        .entities
        .spawned(id, vanillite_abi::spawn_reason::TRIGGERED, 0);
}

/// The plant of a flower pot (`FlowerPotBlock.potted`), `None` for the empty pot.
fn potted_content(pot: BlockId) -> Option<BlockId> {
    let name = pot.name().strip_prefix("minecraft:potted_")?;
    let content = match name {
        "azalea_bush" => "azalea",
        "flowering_azalea_bush" => "flowering_azalea",
        other => other,
    };
    BlockId::by_name(&format!("minecraft:{content}"))
}

/// `POTTED_BY_CONTENT.get(block)`.
fn pot_of(content: BlockId) -> Option<BlockId> {
    let name = content.name().strip_prefix("minecraft:")?;
    let pot = match name {
        "azalea" => "azalea_bush",
        "flowering_azalea" => "flowering_azalea_bush",
        other => other,
    };
    BlockId::by_name(&format!("minecraft:potted_{pot}"))
        .filter(|b| b.info().kind == BlockKind::FlowerPotBlock)
}

/// `FlowerPotBlock.useItemOn`: a plant goes into an empty pot.
fn flower_pot_use_item_on(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    let Some(pot) = item::item_id(stack).block().and_then(pot_of) else {
        return ItemInteraction::TryWithEmptyHand;
    };
    if potted_content(state.block()).is_some() {
        return ItemInteraction::Done(Interaction::Consume);
    }
    level.set_block_and_update(pos, pot.default_state());
    level.game_event_at(Some(ctx.player), events().block_change, pos);
    level.award_stat(ctx.player, Stat::custom_named("minecraft:pot_flower"), 1);
    if !ctx.infinite {
        stack.shrink(1);
    }
    ItemInteraction::Done(Interaction::Success)
}

/// `FlowerPotBlock.useWithoutItem`: the plant goes to the player.
fn flower_pot_use_without_item(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
) -> Interaction {
    let Some(content) = potted_content(state.block()) else {
        return Interaction::Consume;
    };
    let item = content.info().item;
    if item != u16::MAX {
        level.give_to_player(ctx.player, ItemStack::new(item, 1));
    }
    level.set_block_and_update(pos, ids::FLOWER_POT.default_state());
    level.game_event_at(Some(ctx.player), events().block_change, pos);
    Interaction::Success
}

/// `Block.dropFromBlockInteractLootTable`: the stacks of a `BLOCK_INTERACT` loot table with
/// the table's random sequence, else the level random.
pub(crate) fn interact_loot(level: &mut Level, table: &str, state: BlockState) -> Vec<ItemStack> {
    let Some(data) = crate::player::play::game_data() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let tool = ItemStack::EMPTY;
    let in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let ctx = crate::loot::BlockLootContext {
        state,
        tool: &tool,
        enchantment_level: &|_| 0,
        item_in_tag: &in_tag,
        explosion_radius: None,
        this_present: false,
        entity: None,
        block_entity: None,
    };
    match data.loot.random_sequence(table) {
        Some(key) => {
            let sequences = std::sync::Arc::clone(level.random_sequences());
            let mut sequences = sequences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            data.loot
                .table_drops(table, &ctx, sequences.get(key), &mut out);
        }
        None => data.loot.table_drops(table, &ctx, level.random(), &mut out),
    }
    out
}

/// `PumpkinBlock.useItemOn`: shears carve the pumpkin and drop its seeds.
fn pumpkin_use_item_on(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    if item::item_id(stack) != items::SHEARS {
        return ItemInteraction::TryWithEmptyHand;
    }
    let direction = if matches!(ctx.face, Direction::Up | Direction::Down) {
        super::redstone::direction_from_yaw(ctx.yaw).opposite()
    } else {
        ctx.face
    };
    let (sx, sz) = match direction {
        Direction::East => (1, 0),
        Direction::West => (-1, 0),
        Direction::South => (0, 1),
        Direction::North => (0, -1),
        _ => (0, 0),
    };
    for seeds in interact_loot(level, "minecraft:carve/pumpkin", state) {
        let at = Vec3::new(
            f64::from(pos.x) + 0.5 + f64::from(sx) * 0.65,
            f64::from(pos.y) + 0.1,
            f64::from(pos.z) + 0.5 + f64::from(sz) * 0.65,
        );
        // `new ItemEntity(...)` draws its own motion, which is replaced right away.
        let id = super::drops::spawn_item_moving(level, at, seeds, None, 0, None);
        let random = level.random();
        let vx = 0.05 * f64::from(sx) + random.next_double() * 0.02;
        let vz = 0.05 * f64::from(sz) + random.next_double() * 0.02;
        if let Some(mut e) = id.and_then(|id| level.entities.store.get_mut(id)) {
            e.set_velocity(Vec3::new(vx, 0.05, vz));
        }
    }
    level.play_block_sound(
        None,
        pos,
        sound_id("block.pumpkin.carve"),
        SOUND_BLOCKS,
        1.0,
        1.0,
    );
    let carved = ids::CARVED_PUMPKIN
        .default_state()
        .set_direction("facing", direction);
    level.set_block(pos, carved, update::ALL_IMMEDIATE);
    if !ctx.infinite {
        super::hurt_item(level, stack, 1, ctx.player, ctx.hand_slot());
    }
    level.game_event_at(Some(ctx.player), events().shear, pos);
    level.award_stat(ctx.player, Stat::of(StatType::Used, items::SHEARS.0), 1);
    ItemInteraction::Done(Interaction::Success)
}

/// `CaveVines.use`: ripe glow berries are picked.
pub fn cave_vines_use(
    level: &mut Level,
    source: Option<EntityId>,
    state: BlockState,
    pos: BlockPos,
) -> Interaction {
    if !state.flag("berries") {
        return Interaction::Pass;
    }
    for stack in interact_loot(level, "minecraft:harvest/cave_vine", state) {
        super::drops::pop_resource(level, pos, stack);
    }
    // `Mth.randomBetween(random, 0.8F, 1.2F)`: `(1.2F - 0.8F)` is 0.40000004F.
    let pitch = level.random().next_float() * (1.2_f32 - 0.8_f32) + 0.8;
    level.play_block_sound(
        None,
        pos,
        sound_id("block.cave_vines.pick_berries"),
        SOUND_BLOCKS,
        1.0,
        pitch,
    );
    let new_state = state.set_flag("berries", false);
    level.set_block(pos, new_state, update::CLIENTS);
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext {
            source,
            affected: Some(new_state),
            owner: None,
        },
    );
    Interaction::Success
}

/// `SweetBerryBushBlock.useItemOn`: bone meal grows a bush that is not ripe.
fn sweet_berry_use_item_on(
    _: &mut Level,
    state: BlockState,
    _: BlockPos,
    _: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    if state.int("age") != Some(3) && item::item_id(stack) == items::BONE_MEAL {
        ItemInteraction::Pass
    } else {
        ItemInteraction::TryWithEmptyHand
    }
}

/// `SweetBerryBushBlock.useWithoutItem`: berries are picked from an older bush.
fn sweet_berry_use_without_item(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
) -> Interaction {
    if state.int("age").unwrap_or(0) <= 1 {
        return Interaction::Pass;
    }
    for stack in interact_loot(level, "minecraft:harvest/sweet_berry_bush", state) {
        super::drops::pop_resource(level, pos, stack);
    }
    let pitch = 0.8 + level.random().next_float() * 0.4;
    level.play_block_sound(
        None,
        pos,
        sound_id("block.sweet_berry_bush.pick_berries"),
        SOUND_BLOCKS,
        1.0,
        pitch,
    );
    let new_state = state.set_int("age", 1);
    level.set_block(pos, new_state, update::CLIENTS);
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext {
            source: Some(ctx.player),
            affected: Some(new_state),
            owner: None,
        },
    );
    Interaction::Success
}

/// `DragonEggBlock.teleport`: the egg jumps to a free spot nearby.
fn dragon_egg_teleport(level: &mut Level, state: BlockState, pos: BlockPos) {
    for _ in 0..1000 {
        let random = level.random();
        let dx = random.next_int_bounded(16) - random.next_int_bounded(16);
        let dy = random.next_int_bounded(8) - random.next_int_bounded(8);
        let dz = random.next_int_bounded(16) - random.next_int_bounded(16);
        let test = BlockPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
        if level.block(test).is_air()
            && !level
                .block(BlockPos::new(test.x, test.y - 1, test.z))
                .is_air()
            && level
                .border
                .is_within_bounds(f64::from(test.x), f64::from(test.z))
            && level.in_height(test.y)
        {
            // `BlockUtil.packDifferenceInPosition(pos, testPos, 16, 8, 16)`.
            let packed = ((dx + 16) & 0xFF) << 16 | ((dy + 8) & 0xFF) << 8 | ((dz + 16) & 0xFF);
            level.level_event(None, 2015, pos, packed);
            level.set_block(test, state, update::CLIENTS);
            level.remove_block(pos, false);
            return;
        }
    }
}

/// `LightBlock.useWithoutItem`: operators in creative cycle the light level.
fn light_use_without_item(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
) -> Interaction {
    if !ctx.game_master {
        return Interaction::Consume;
    }
    let next = (state.int("level").unwrap_or(0) + 1) % 16;
    level.set_block(pos, state.set_int("level", next), update::CLIENTS);
    Interaction::Success
}

/// `CopperGolemStatueBlock.useItemOn`: anything but an axe turns the statue's pose. On an
/// unwaxed statue (`WeatheringCopperGolemStatueBlock`) honeycomb waxes it instead and an axe
/// brings an unaffected statue back to life.
fn statue_use_item_on(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    let is_axe = crate::player::play::game_data()
        .is_some_and(|d| d.item_in_tag(stack.item, "minecraft:axes"));
    let weathering = state.block().info().kind == BlockKind::WeatheringCopperGolemStatueBlock;
    if weathering && !is_axe && item::item_id(stack) == items::HONEYCOMB {
        return ItemInteraction::Pass;
    }
    if is_axe {
        if weathering && state.is(ids::COPPER_GOLEM_STATUE) {
            revive_statue(level, state, pos);
            super::hurt_item(level, stack, 1, ctx.player, ctx.hand_slot());
            level.remove_block(pos, false);
            return ItemInteraction::Done(Interaction::Success);
        }
        return ItemInteraction::Pass;
    }
    level.play_block_sound(
        None,
        pos,
        sound_id("entity.copper_golem_become_statue"),
        SOUND_BLOCKS,
        1.0,
        1.0,
    );
    // `Pose.getNextPose`: standing, sitting, running, star, then standing again.
    let pose = state.value_index("copper_golem_pose").unwrap_or(0);
    let next = state
        .with_index("copper_golem_pose", (pose + 1) % 4)
        .unwrap_or(state);
    level.set_block_and_update(pos, next);
    level.game_event_at(Some(ctx.player), events().block_change, pos);
    ItemInteraction::Done(Interaction::Success)
}
