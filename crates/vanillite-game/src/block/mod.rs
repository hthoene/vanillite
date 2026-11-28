//! Block behaviours (architecture 4.4): per block a table of plain functions that the level
//! calls (`onPlace`, `affectNeighborsAfterRemoval`, `neighborChanged`, scheduled `tick`,
//! `randomTick`, `triggerEvent`). Shape updates and survival checks are shared with world
//! generation (`vanillite_worldgen::behaviour`).
//!
//! [`Behaviours::vanilla`] fills the table from the Vanilla class of every block; a plugin may
//! replace entries before the world starts. Blocks whose class has no such behaviour have
//! empty entries and cost nothing.
//!
//! [`place`] has Vanilla-exact placement states, survival and replacement rules, and the
//! blocks that placing or breaking one block adds or removes.

pub mod beacon;
pub mod bed;
pub mod beehive;
pub mod bell;
pub mod bonemeal;
pub mod bookshelf;
pub mod break_hooks;
pub mod brewing;
pub mod brushable;
pub(crate) mod bubble;
pub mod cake;
pub mod campfire;
pub mod cauldron;
pub mod chest_loot;
pub mod composter;
pub mod contact;
pub mod container;
pub mod crafter;
pub mod creaking_heart;
pub mod decor;
pub mod dispenser;
mod dripleaf;
mod dripstone;
pub mod drops;
pub mod eggs;
mod eyeblossom;
pub mod falling;
pub mod fire;
pub mod fluid;
pub mod furnace;
pub mod geyser;
mod growth;
pub mod hook_events;
pub mod hopper;
pub mod interact;
pub mod jukebox;
pub mod lectern;
pub mod lock;
pub mod note;
pub mod piston;
pub mod place;
pub mod plate;
pub mod portal;
pub mod pot;
pub mod projectile_hit;
pub mod rail;
pub mod redstone;
pub mod sculk;
pub mod sculk_catalyst;
pub mod sculk_shrieker;
pub mod sculk_vibration;
pub mod sensor;
pub mod shelf;
pub mod sign;
pub mod spawner;
#[cfg(test)]
pub(crate) mod spawner_tests;
mod spread;
pub mod technical;
pub mod tnt;
pub mod tree;
pub mod trial_spawner;
pub mod tripwire;
pub mod vault;
pub mod weather;

use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_data::block::{BLOCK_COUNT, BlockId, BlockState, flags, ids};
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::StateExt;
use vanillite_worldgen::behaviour::{self as shared, Class, class_of_block, tags};

use crate::level::Level;

/// `Blocks.VOID_AIR`: what the level reads below and above the build height.
pub const VOID_AIR: BlockState = BlockState(ids_void_air());

const fn ids_void_air() -> u16 {
    // `minecraft:void_air` has a single state.
    vanillite_data::block::BLOCKS[ids::VOID_AIR.0 as usize].first_state
}

/// `onPlace(state, level, pos, oldState, movedByPiston)`.
pub type PlaceFn = fn(&mut Level, BlockState, BlockPos, BlockState, bool);
/// `affectNeighborsAfterRemoval(state, level, pos, movedByPiston)`.
pub type RemovalFn = fn(&mut Level, BlockState, BlockPos, bool);
/// `neighborChanged(state, level, pos, sourceBlock, orientation, movedByPiston)`.
pub type NeighborFn = fn(&mut Level, BlockState, BlockPos, BlockId, bool);
/// `tick` and `randomTick(state, level, pos, random)`; the random is the level's.
pub type TickFn = fn(&mut Level, BlockState, BlockPos);
/// `triggerEvent(state, level, pos, b0, b1)`: whether the event goes to clients.
pub type EventFn = fn(&mut Level, BlockState, BlockPos, i32, i32) -> bool;
/// `getSignal` and `getDirectSignal(state, level, pos, direction)`: the signal `state` at `pos`
/// emits towards the neighbour at `pos - direction`.
pub type SignalFn = fn(&Level, BlockState, BlockPos, Direction) -> i32;
/// `getAnalogOutputSignal(state, level, pos, direction)`: what a comparator facing away in
/// `direction` reads.
pub type AnalogFn = fn(&Level, BlockState, BlockPos, Direction) -> i32;
/// `useWithoutItem(state, level, pos, player, hitResult)`.
pub type UseFn = fn(&mut Level, BlockState, BlockPos, &UseContext) -> Interaction;
/// `useItemOn(stack, state, level, pos, player, hand, hitResult)`: the stack is the one in
/// the hand; a handler that uses items shrinks it (creative players keep theirs).
pub type UseItemFn = fn(
    &mut Level,
    BlockState,
    BlockPos,
    &UseContext,
    &mut crate::item::ItemStack,
) -> ItemInteraction;
/// `entityInside(state, level, pos, entity, effectApplier, isPrecise)`.
pub type EntityInsideFn = fn(&mut Level, BlockState, BlockPos, EntityId);
/// `attack(state, level, pos, player)`: a player starts to break the block.
pub type AttackFn = fn(&mut Level, BlockState, BlockPos, EntityId);

/// What a block does to the player that used it; the player handlers apply it after the use
/// (the player's data is not part of the level).
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEffect {
    /// `player.getFoodData().eat(nutrition, saturation)`.
    Eat(i32, f32),
    /// `ItemUtils.createFilledResult` for a creative player: the stack goes into the inventory
    /// unless it already holds the same item and components.
    GiveIfMissing(crate::item::ItemStack),
    /// `player.setItemInHand(hand, stack)` of the used hand, also for creative players (whose
    /// held stack does not change otherwise).
    SetHand(crate::item::ItemStack),
}

/// What a block learns about a player that uses it with an empty hand (or an item without a
/// use of its own): `useWithoutItem`'s player and hit result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UseContext {
    /// The player.
    pub player: EntityId,
    /// `player.getAbilities().mayBuild`.
    pub may_build: bool,
    /// The player's yaw, for `player.getDirection()`.
    pub yaw: f32,
    /// The clicked face.
    pub face: Direction,
    /// The exact hit location.
    pub hit: Vec3,
    /// `player.hasInfiniteMaterials()` (creative).
    pub infinite: bool,
    /// `player.canEat(false)`: invulnerable or hungry.
    pub can_eat: bool,
    /// `player.canUseGameMasterBlocks()`: creative with operator permission level 2.
    pub game_master: bool,
    /// The click is with the main hand.
    pub main_hand: bool,
    /// The item id in the player's off hand.
    pub off_hand_item: u16,
    /// The player's respawn position if it is in this world (`getRespawnConfig`).
    pub respawn_here: Option<BlockPos>,
}

impl UseContext {
    /// The equipment slot of the clicking hand.
    pub fn hand_slot(&self) -> crate::item::EquipmentSlot {
        if self.main_hand {
            crate::item::EquipmentSlot::MainHand
        } else {
            crate::item::EquipmentSlot::OffHand
        }
    }
}

/// `InteractionResult` as far as block use goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interaction {
    /// `PASS`: the block does nothing; the item's own use runs.
    Pass,
    /// `SUCCESS`: the block reacted; the player swings their arm.
    Success,
    /// `CONSUME`: the block reacted without an arm swing.
    Consume,
    /// `FAIL`.
    Fail,
}

/// What `useItemOn` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemInteraction {
    /// A result of its own (consumes the click unless it is `Pass` or `Fail`).
    Done(Interaction),
    /// `TRY_WITH_EMPTY_HAND`: `useWithoutItem` runs for the main hand.
    TryWithEmptyHand,
    /// `PASS`: the item's own use runs.
    Pass,
}

/// The behaviour of one block.
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockBehaviour {
    /// `onPlace`.
    pub on_place: Option<PlaceFn>,
    /// `affectNeighborsAfterRemoval`.
    pub affect_neighbors_after_removal: Option<RemovalFn>,
    /// `neighborChanged`.
    pub neighbor_changed: Option<NeighborFn>,
    /// Scheduled `tick`.
    pub tick: Option<TickFn>,
    /// `randomTick`; only called for states that tick randomly.
    pub random_tick: Option<TickFn>,
    /// `triggerEvent`.
    pub trigger_event: Option<EventFn>,
    /// `getSignal`.
    pub signal: Option<SignalFn>,
    /// `getDirectSignal`.
    pub direct_signal: Option<SignalFn>,
    /// `getAnalogOutputSignal`.
    pub analog_output: Option<AnalogFn>,
    /// `useWithoutItem`.
    pub use_without_item: Option<UseFn>,
    /// `useItemOn` (`TRY_WITH_EMPTY_HAND` without an entry).
    pub use_item_on: Option<UseItemFn>,
    /// `entityInside`.
    pub entity_inside: Option<EntityInsideFn>,
    /// `attack`.
    pub attack: Option<AttackFn>,
    /// `onProjectileHit`.
    pub projectile_hit: Option<projectile_hit::ProjectileHitFn>,
}

/// The behaviour table of all blocks.
#[derive(Debug)]
pub struct Behaviours {
    blocks: Box<[BlockBehaviour]>,
}

impl Behaviours {
    /// Vanilla's behaviours.
    pub fn vanilla() -> Behaviours {
        let blocks = (0..BLOCK_COUNT)
            .map(|id| vanilla_behaviour(BlockId(id as u16)))
            .collect();
        Behaviours { blocks }
    }

    /// The behaviour of a block.
    #[inline]
    pub fn get(&self, block: BlockId) -> &BlockBehaviour {
        &self.blocks[usize::from(block.0)]
    }

    /// Replaces the behaviour of a block (plugins, before the world starts).
    pub fn set(&mut self, block: BlockId, behaviour: BlockBehaviour) {
        self.blocks[usize::from(block.0)] = behaviour;
    }
}

fn vanilla_behaviour(block: BlockId) -> BlockBehaviour {
    let mut b = BlockBehaviour::default();
    match class_of_block(block) {
        Class::Liquid => {
            b.on_place = Some(fluid::liquid_on_place);
            b.neighbor_changed = Some(fluid::liquid_neighbor_changed);
            b.random_tick = Some(fluid::liquid_random_tick);
            b.tick = Some(bubble::liquid_tick);
        }
        Class::BubbleColumn => b.tick = Some(bubble::column_tick),
        Class::ChorusFlower => {
            b.tick = Some(destroy_unless_survives);
            b.random_tick = Some(growth::chorus_flower_random_tick);
        }
        Class::Vine => b.random_tick = Some(growth::vine_random_tick),
        Class::PitcherCrop => b.random_tick = Some(growth::pitcher_crop_random_tick),
        Class::Speleothem => {
            b.tick = Some(dripstone::tick);
            b.random_tick = Some(dripstone::random_tick);
        }
        Class::Falling | Class::Anvil | Class::ConcretePowder => {
            b.on_place = Some(falling::on_place);
            b.tick = Some(falling::tick);
        }
        Class::Scaffolding => {
            b.on_place = Some(falling::scaffolding_on_place);
            b.tick = Some(falling::scaffolding_tick);
        }
        Class::Brushable => {
            b.on_place = Some(falling::brushable_on_place);
            b.tick = Some(falling::brushable_tick);
        }
        Class::Cactus => {
            b.tick = Some(destroy_unless_survives);
            b.random_tick = Some(growth::cactus_random_tick);
        }
        Class::SugarCane => {
            b.tick = Some(destroy_unless_survives);
            b.random_tick = Some(growth::sugar_cane_random_tick);
        }
        Class::BambooStalk => {
            b.tick = Some(destroy_unless_survives);
            b.random_tick = Some(growth::bamboo_random_tick);
        }
        Class::BambooSapling => b.random_tick = Some(growth::bamboo_sapling_random_tick),
        Class::ChorusPlant | Class::HangingMoss | Class::BigDripleafStem => {
            b.tick = Some(destroy_unless_survives);
        }
        Class::Kelp | Class::WeepingVines | Class::TwistingVines | Class::CaveVines => {
            b.tick = Some(destroy_unless_survives);
            b.random_tick = Some(growth::growing_plant_head_random_tick);
        }
        Class::KelpPlant
        | Class::WeepingVinesPlant
        | Class::TwistingVinesPlant
        | Class::CaveVinesPlant => b.tick = Some(destroy_unless_survives),
        Class::Leaves => {
            b.tick = Some(spread::leaves_tick);
            b.random_tick = Some(spread::leaves_random_tick);
        }
        Class::Crop => b.random_tick = Some(growth::crop_random_tick),
        Class::Stem => b.random_tick = Some(growth::stem_random_tick),
        Class::NetherWart => b.random_tick = Some(growth::nether_wart_random_tick),
        Class::Cocoa => b.random_tick = Some(growth::cocoa_random_tick),
        Class::Farmland => {
            b.tick = Some(growth::farmland_tick);
            b.random_tick = Some(growth::farmland_random_tick);
        }
        Class::Path => b.tick = Some(growth::path_tick),
        Class::Mushroom => b.random_tick = Some(spread::mushroom_random_tick),
        Class::SnowLayer => b.random_tick = Some(weather::snow_random_tick),
        Class::Coral => b.tick = Some(spread::coral_tick),
        Class::CoralPlant | Class::CoralFan | Class::CoralWallFan => {
            b.on_place = Some(spread::coral_on_place);
            b.tick = Some(spread::coral_tick);
        }
        _ => {}
    }
    if block == ids::FIRE {
        b.tick = Some(fire::tick);
    }
    if block == ids::CARVED_PUMPKIN || block == ids::JACK_O_LANTERN {
        b.on_place = Some(crate::mob::golem_build::pumpkin_on_place);
    }
    if block == ids::GRASS_BLOCK || block == ids::MYCELIUM {
        b.random_tick = Some(spread::spreading_snowy_dirt_random_tick);
    } else if block == ids::CRIMSON_NYLIUM || block == ids::WARPED_NYLIUM {
        b.random_tick = Some(spread::nylium_random_tick);
    } else if block == ids::ICE {
        b.random_tick = Some(weather::ice_random_tick);
    } else if block == ids::FROSTED_ICE {
        b.on_place = Some(weather::frosted_ice_on_place);
        b.tick = Some(weather::frosted_ice_tick);
        b.neighbor_changed = Some(weather::frosted_ice_neighbor_changed);
        b.random_tick = Some(weather::ice_random_tick);
    } else if block == ids::CAULDRON || block == ids::WATER_CAULDRON {
        // `AbstractCauldronBlock.tick`; lava and powder snow cauldrons take no drips.
        b.tick = Some(dripstone::cauldron_tick);
    } else if block == ids::SWEET_BERRY_BUSH {
        b.random_tick = Some(growth::sweet_berry_random_tick);
    } else if growth::is_sapling(block) {
        b.random_tick = Some(growth::sapling_random_tick);
    } else if block == ids::MANGROVE_PROPAGULE {
        b.random_tick = Some(growth::propagule_random_tick);
    } else if block == ids::BUDDING_AMETHYST {
        b.random_tick = Some(growth::budding_amethyst_random_tick);
    } else if block == ids::REDSTONE_ORE || block == ids::DEEPSLATE_REDSTONE_ORE {
        b.random_tick = Some(weather::redstone_ore_random_tick);
        b.attack = Some(weather::redstone_ore_attack);
        b.use_item_on = Some(weather::redstone_ore_use_item_on);
    } else if block == ids::SPONGE {
        b.on_place = Some(fluid::sponge_on_place);
        b.neighbor_changed = Some(fluid::sponge_neighbor_changed);
    } else if block == ids::WET_SPONGE {
        b.on_place = Some(fluid::wet_sponge_on_place);
    } else if weather::weathers(block) {
        b.random_tick = Some(weather::copper_random_tick);
    }
    if block == ids::LILY_PAD {
        b.entity_inside = Some(contact::lily_pad_entity_inside);
    } else if block == ids::FROGSPAWN {
        b.entity_inside = Some(falling::frogspawn_entity_inside);
    }
    if decor::is_skull(block) {
        b.neighbor_changed = Some(decor::skull_neighbor_changed);
    }
    redstone::fill(block, &mut b);
    piston::fill(block, &mut b);
    rail::fill(block, &mut b);
    plate::fill(block, &mut b);
    tripwire::fill(block, &mut b);
    sensor::fill(block, &mut b);
    note::fill(block, &mut b);
    bell::fill(block, &mut b);
    lectern::fill(block, &mut b);
    jukebox::fill(block, &mut b);
    sculk::fill(block, &mut b);
    sculk_shrieker::fill(block, &mut b);
    sculk_catalyst::fill(block, &mut b);
    tnt::fill(block, &mut b);
    dispenser::fill(block, &mut b);
    hopper::fill(block, &mut b);
    spawner::fill(block, &mut b);
    container::fill(block, &mut b);
    sign::fill(block, &mut b);
    portal::fill(block, &mut b);
    pot::fill(block, &mut b);
    bookshelf::fill(block, &mut b);
    shelf::fill(block, &mut b);
    campfire::fill(block, &mut b);
    cake::fill(block, &mut b);
    bed::fill(block, &mut b);
    cauldron::fill(block, &mut b);
    composter::fill(block, &mut b);
    interact::fill(block, &mut b);
    beehive::fill(block, &mut b);
    crafter::fill(block, &mut b);
    technical::fill(block, &mut b);
    creaking_heart::fill(block, &mut b);
    portal::fill(block, &mut b);
    eggs::fill(block, &mut b);
    eyeblossom::fill(block, &mut b);
    geyser::fill(block, &mut b);
    dripleaf::fill(block, &mut b);
    projectile_hit::fill(block, &mut b);
    b
}

/// The common `tick` of plants that break when they lose their support: `if
/// (!state.canSurvive(level, pos)) level.destroyBlock(pos, true)`.
fn destroy_unless_survives(level: &mut Level, state: BlockState, pos: BlockPos) {
    if !level.can_survive(state, pos) {
        level.destroy_block(pos, true, None);
    }
}

/// `block instanceof BaseRailBlock`.
pub fn is_rail(block: BlockId) -> bool {
    matches!(
        class_of_block(block),
        Class::Rail | Class::PoweredRail | Class::DetectorRail
    )
}

/// `state.is(BlockTags.FIRE)`.
pub fn is_fire(state: BlockState) -> bool {
    state.is(ids::FIRE) || state.is(ids::SOUL_FIRE)
}

/// `state.getBlock() instanceof LiquidBlock`.
pub fn is_liquid(state: BlockState) -> bool {
    class_of_block(state.block()) == Class::Liquid
}

/// `state.canBeReplaced()`.
pub fn can_be_replaced(state: BlockState) -> bool {
    state.has(flags::REPLACEABLE)
}

/// `state.isSolid()`.
pub fn is_solid(state: BlockState) -> bool {
    state.has(flags::SOLID)
}

/// `state.isFaceSturdy(level, pos, direction)` (full support).
pub fn is_face_sturdy(state: BlockState, direction: Direction) -> bool {
    shared::is_face_sturdy(state, direction, shared::SupportType::Full)
}

/// The tags block behaviour resolved from the data packs.
pub fn block_tags() -> &'static shared::BlockTags {
    &tags().blocks
}

/// The `minecraft:sound_event` id of `name` (without namespace, e.g. `block.lever.click`);
/// 0 for unknown names.
pub fn sound_id(name: &str) -> u16 {
    let mut full = [0u8; 96];
    let prefix = b"minecraft:";
    let len = prefix.len() + name.len();
    if len > full.len() {
        return 0;
    }
    full[..prefix.len()].copy_from_slice(prefix);
    full[prefix.len()..len].copy_from_slice(name.as_bytes());
    let full = std::str::from_utf8(&full[..len]).unwrap_or_default();
    vanillite_data::registry::SOUND_EVENT.id(full).unwrap_or(0)
}

/// `ItemStack.hurtAndBreak(amount, player, slot)` of a held tool: the `item_damage` effects
/// (Unbreaking) from the level random, `ITEM_DURABILITY_CHANGED`, and when it breaks
/// `ITEM_BROKEN` and the break event (`onEquippedItemBroken`).
pub fn hurt_item(
    level: &mut Level,
    stack: &mut crate::item::ItemStack,
    amount: i32,
    player: EntityId,
    slot: crate::item::EquipmentSlot,
) {
    use crate::item;
    // `hurtAndBreak`: players with infinite materials (creative) take no damage.
    let creative = level
        .entities
        .player(player)
        .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Creative);
    let taken = crate::durability::change(stack, amount, creative, level.random());
    if taken == 0 {
        return;
    }
    let damage = item::damage(stack).saturating_add(taken);
    // `applyDamage`: `ITEM_DURABILITY_CHANGED`, and `ITEM_BROKEN` when it breaks.
    level.trigger(
        player,
        crate::advancements::trigger::Event::ItemDurabilityChanged(stack.clone(), damage),
    );
    let item = stack.item;
    if crate::durability::apply(stack, damage) {
        level.award_stat(
            player,
            crate::stats::Stat::of(crate::stats::StatType::Broken, item),
            1,
        );
        crate::durability::broken_on_level(level, player, slot);
    }
}

/// `level.findBlocksIn(from, to).filterState(predicate).forEach`: the matching positions in
/// `BlockScanUtils` order (sections by x, z, y; inside a section by y, z, x). Positions in
/// unloaded chunks and outside the build height are skipped.
pub fn find_blocks_in(
    level: &Level,
    from: BlockPos,
    to: BlockPos,
    mut predicate: impl FnMut(BlockState) -> bool,
) -> Vec<BlockPos> {
    let (min_x, max_x) = (from.x.min(to.x), from.x.max(to.x));
    let (min_y, max_y) = (
        from.y.min(to.y).max(level.min_y()),
        from.y.max(to.y).min(level.max_y()),
    );
    let (min_z, max_z) = (from.z.min(to.z), from.z.max(to.z));
    let mut out = Vec::new();
    if min_y > max_y {
        return out;
    }
    for sx in (min_x >> 4)..=(max_x >> 4) {
        for sz in (min_z >> 4)..=(max_z >> 4) {
            if level
                .chunks
                .chunk(vanillite_base::ChunkPos::new(sx, sz))
                .is_none()
            {
                continue;
            }
            for sy in (min_y >> 4)..=(max_y >> 4) {
                for y in (sy * 16).max(min_y)..=(sy * 16 + 15).min(max_y) {
                    for z in (sz * 16).max(min_z)..=(sz * 16 + 15).min(max_z) {
                        for x in (sx * 16).max(min_x)..=(sx * 16 + 15).min(max_x) {
                            let pos = BlockPos::new(x, y, z);
                            if predicate(level.block(pos)) {
                                out.push(pos);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}
