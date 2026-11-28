//! The live block entities of a level (`LevelChunk.blockEntities` and `Level.blockEntityTickers`).
//!
//! A block entity is plain data of its kind ([`BlockEntity`]); its behaviour are functions in
//! [`crate::block`] like every other block behaviour. Block entities are created and removed by
//! [`Level::set_block`] as in `LevelChunk.setBlockState`, and kinds with a ticker tick in
//! [`Level::tick_block_entities`] in Vanilla's order: in the order their tickers were added,
//! tickers added during a tick run from the next tick on.

use vanillite_base::hash::{FxHashMap, FxHashSet};
use vanillite_base::{BlockPos, ChunkPos, Direction};
use vanillite_chunk::slots::BlockEntityData;
use vanillite_data::block::BlockState;
use vanillite_nbt::{Compound, Tag};
use vanillite_worldgen::behaviour::StateExt;

use super::Level;
use super::game_event::{GameEventListeners, Listener};
use crate::block::bell::Bell;
use crate::block::lectern::Lectern;
use crate::block::piston::MovingPiston;
use crate::block::sculk::SculkSensor;
use crate::item::ItemStack;
use vanillite_protocol::components::ids as components;
use vanillite_protocol::text::Component;

/// The data of one block entity, by kind. Kinds whose behaviour does not exist yet have no
/// variant; their blocks simply have no live block entity.
#[derive(Clone, Debug, PartialEq)]
pub enum BlockEntity {
    /// `ComparatorBlockEntity`: the output signal.
    Comparator {
        /// `output`.
        output: i32,
    },
    /// `PistonMovingBlockEntity`.
    MovingPiston(Box<MovingPiston>),
    /// `DaylightDetectorBlockEntity` (no data; it ticks).
    DaylightDetector,
    /// `BellBlockEntity`.
    Bell(Bell),
    /// `LecternBlockEntity`.
    Lectern(Lectern),
    /// `SculkSensorBlockEntity` and `CalibratedSculkSensorBlockEntity`.
    SculkSensor(Box<SculkSensor>),
    /// `HopperBlockEntity`: its stacks and the transfer timing.
    Hopper(Box<crate::block::hopper::Hopper>),
    /// Chests, trapped chests, barrels, shulker boxes, dispensers and droppers.
    Container(Box<crate::block::container::Container>),
    /// `EnderChestBlockEntity`: the viewers of the lid (the stacks are the player's).
    EnderChest {
        /// Players that have it open.
        openers: i32,
    },
    /// Furnaces, blast furnaces and smokers.
    Furnace(Box<crate::block::furnace::Furnace>),
    /// Brewing stands.
    BrewingStand(Box<crate::block::brewing::BrewingStand>),
    /// Signs and hanging signs.
    Sign(Box<crate::block::sign::Sign>),
    /// Heads and skulls.
    Skull(Box<crate::block::decor::Skull>),
    /// Banners.
    Banner(Box<crate::block::decor::Banner>),
    /// Decorated pots.
    DecoratedPot(Box<crate::block::pot::DecoratedPot>),
    /// Chiseled bookshelves.
    ChiseledBookshelf(Box<crate::block::bookshelf::Bookshelf>),
    /// Campfires and soul campfires.
    Campfire(Box<crate::block::campfire::Campfire>),
    /// Suspicious sand and gravel.
    Brushable(Box<crate::block::brushable::Brushable>),
    /// Crafters.
    Crafter(Box<crate::block::crafter::Crafter>),
    /// Beacons.
    Beacon(Box<crate::block::beacon::Beacon>),
    /// Conduits.
    Conduit(Box<crate::block::beacon::Conduit>),
    /// `SpawnerBlockEntity` (`BaseSpawner`).
    Spawner(Box<crate::block::spawner::MobSpawner>),
    /// `TrialSpawnerBlockEntity`.
    TrialSpawner(Box<crate::block::trial_spawner::TrialSpawner>),
    /// `VaultBlockEntity`.
    Vault(Box<crate::block::vault::Vault>),
    /// `SculkShriekerBlockEntity`.
    SculkShrieker(Box<crate::block::sculk_shrieker::Shrieker>),
    /// `SculkCatalystBlockEntity` with its `SculkSpreader`.
    SculkCatalyst(Box<crate::block::sculk_catalyst::Catalyst>),
    /// `TheEndPortalBlockEntity` (no data; clients render the portal from it).
    EndPortal,
    /// `TheEndGatewayBlockEntity`.
    EndGateway(crate::block::portal::Gateway),
    /// `CommandBlockEntity` (impulse, chain and repeating command blocks).
    CommandBlock(Box<crate::block::technical::command_block::CommandBlock>),
    /// `StructureBlockEntity`.
    StructureBlock(Box<crate::block::technical::structure_block::StructureBlock>),
    /// `JigsawBlockEntity`.
    Jigsaw(Box<crate::block::technical::jigsaw::Jigsaw>),
    /// `TestBlockEntity`.
    TestBlock(Box<crate::block::technical::test_block::TestBlock>),
    /// `TestInstanceBlockEntity`.
    TestInstance(Box<crate::block::technical::test_instance::TestInstance>),
    /// `BeehiveBlockEntity` (beehives and bee nests).
    Beehive(Box<crate::block::beehive::Beehive>),
    /// `CreakingHeartBlockEntity`.
    CreakingHeart(Box<crate::block::creaking_heart::CreakingHeart>),
    /// `JukeboxBlockEntity`.
    Jukebox(Box<crate::block::jukebox::Jukebox>),
    /// `EnchantingTableBlockEntity`: its custom name.
    EnchantingTable(Option<Box<Component>>),
    /// `ShelfBlockEntity`.
    Shelf(Box<Shelf>),
    /// `CopperGolemStatueBlockEntity`: the components it keeps for the golem (its name).
    CopperGolemStatue(Option<Box<Compound>>),
    /// `PotentSulfurBlockEntity`: the eruption countdown.
    PotentSulfur {
        /// `countdown` (`waitingCountdown`).
        countdown: i32,
    },
}

/// `ShelfBlockEntity`: three stacks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shelf {
    /// `Items`.
    pub items: [ItemStack; 3],
    /// `align_items_to_bottom`.
    pub align_items_to_bottom: bool,
}

impl Shelf {
    fn load(&mut self, nbt: &Compound) {
        self.items = Default::default();
        crate::block::container::load_items(nbt, &mut self.items);
        self.align_items_to_bottom =
            matches!(nbt.get("align_items_to_bottom"), Some(Tag::Byte(v)) if *v != 0);
    }

    fn save(&self, nbt: &mut Compound) {
        crate::block::container::save_items_always(&self.items, nbt);
        nbt.insert(
            "align_items_to_bottom",
            Tag::Byte(i8::from(self.align_items_to_bottom)),
        );
    }
}

/// `BlockEntityTicker.tick(level, pos, state, blockEntity)`: the block entity is looked up at
/// `pos`.
pub type TickerFn = fn(&mut Level, BlockPos, BlockState);

#[derive(Debug)]
struct Entry {
    entity: BlockEntity,
    /// The ticker slot of this block entity, if it ticks.
    ticker: Option<u32>,
    /// `BlockEntity.components`: the item components the block entity keeps without using them
    /// (`item_name`, `lore`, `custom_data`, ...), in their saved form (`"components"`).
    components: Option<Box<Compound>>,
}

/// The block entities of a level, per chunk.
#[derive(Debug, Default)]
pub struct BlockEntities {
    chunks: FxHashMap<ChunkPos, FxHashMap<BlockPos, Entry>>,
    /// Chunks whose block entities changed in a way the stored list does not show yet
    /// (removed or added ones); written back on save.
    touched: FxHashSet<ChunkPos>,
    /// `blockEntityTickers`: positions and slot ids in tick order; a slot is live while the
    /// entry at its position still holds its id.
    tickers: Vec<(BlockPos, u32)>,
    /// `pendingBlockEntityTickers`.
    pending: Vec<(BlockPos, u32)>,
    next_ticker: u32,
    /// Game event listeners, of block entities and (later) entities.
    pub(super) listeners: GameEventListeners,
    /// The number of loaded End gateways: entities look for gateways around them only while
    /// there are any.
    gateways: usize,
}

impl BlockEntities {
    #[inline]
    fn entry(&self, pos: BlockPos) -> Option<&Entry> {
        self.chunks.get(&pos.chunk())?.get(&pos)
    }

    /// The block entity at `pos`.
    #[inline]
    pub fn get(&self, pos: BlockPos) -> Option<&BlockEntity> {
        self.entry(pos).map(|e| &e.entity)
    }

    /// The block entity at `pos`, mutably.
    #[inline]
    pub fn get_mut(&mut self, pos: BlockPos) -> Option<&mut BlockEntity> {
        self.chunks
            .get_mut(&pos.chunk())?
            .get_mut(&pos)
            .map(|e| &mut e.entity)
    }

    /// Number of live block entities.
    pub fn len(&self) -> usize {
        self.chunks.values().map(FxHashMap::len).sum()
    }

    /// Whether there are no live block entities.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Whether an End gateway is loaded.
    pub fn has_gateways(&self) -> bool {
        self.gateways > 0
    }

    /// `setBlockEntity` / `addAndRegisterBlockEntity`: replaces the block entity at `pos`; a
    /// ticking kind keeps the ticker slot of the one it replaces (`RebindableTickingBlockEntity
    /// Wrapper.rebind`) or gets a new one at the end of the tick order.
    pub fn insert(&mut self, pos: BlockPos, entity: BlockEntity) {
        let ticks = ticker_of(&entity).is_some();
        let chunk = pos.chunk();
        self.touched.insert(chunk);
        let entries = self.chunks.entry(chunk).or_default();
        let old_ticker = entries.get(&pos).and_then(|e| e.ticker);
        let ticker = match (ticks, old_ticker) {
            (true, Some(id)) => Some(id),
            (true, None) => {
                let id = self.next_ticker;
                self.next_ticker = self.next_ticker.wrapping_add(1);
                self.pending.push((pos, id));
                Some(id)
            }
            (false, _) => None,
        };
        let new_listener = listener_of(pos, &entity);
        let gateway = matches!(entity, BlockEntity::EndGateway(_));
        let old = entries.insert(
            pos,
            Entry {
                entity,
                ticker,
                components: None,
            },
        );
        let old_gateway = matches!(
            old,
            Some(Entry {
                entity: BlockEntity::EndGateway(_),
                ..
            })
        );
        self.gateways = self.gateways + usize::from(gateway) - usize::from(old_gateway);
        if let Some(old) = old
            && let Some(listener) = listener_of(pos, &old.entity)
            && Some(listener) != new_listener
        {
            self.listeners.unregister(listener);
        }
        // `addGameEventListener`.
        if let Some(listener) = new_listener {
            self.listeners.register(listener);
        }
    }

    /// `removeBlockEntity(pos)`: the block entity, its ticker and its game event listener go.
    pub fn remove(&mut self, pos: BlockPos) -> Option<BlockEntity> {
        let chunk = pos.chunk();
        let entries = self.chunks.get_mut(&chunk)?;
        let removed = entries.remove(&pos)?;
        if entries.is_empty() {
            self.chunks.remove(&chunk);
        }
        self.touched.insert(chunk);
        if matches!(removed.entity, BlockEntity::EndGateway(_)) {
            self.gateways -= 1;
        }
        if let Some(listener) = listener_of(pos, &removed.entity) {
            self.listeners.unregister(listener);
        }
        Some(removed.entity)
    }

    /// The components the block entity at `pos` keeps (`BlockEntity.components`).
    pub fn kept_components(&self, pos: BlockPos) -> Option<&Compound> {
        self.chunks
            .get(&pos.chunk())?
            .get(&pos)?
            .components
            .as_deref()
    }

    /// Sets the components the block entity at `pos` keeps.
    pub fn set_kept_components(&mut self, pos: BlockPos, components: Option<Compound>) {
        let chunk = pos.chunk();
        if let Some(e) = self.chunks.get_mut(&chunk).and_then(|c| c.get_mut(&pos)) {
            e.components = components.filter(|c| !c.is_empty()).map(Box::new);
            self.touched.insert(chunk);
        }
    }

    /// The number of block entities per chunk (`/vanillite chunks`).
    pub fn per_chunk(&self) -> impl Iterator<Item = (ChunkPos, usize)> + '_ {
        self.chunks.iter().map(|(pos, map)| (*pos, map.len()))
    }

    /// The block entities that tick.
    pub fn ticking(&self) -> usize {
        self.tickers.len()
    }

    /// Bytes of the block entities' entries (`/vanillite memory`; data behind them not
    /// counted).
    pub fn memory_bytes(&self) -> usize {
        self.len() * (size_of::<Entry>() + size_of::<BlockPos>() + 8)
            + self.tickers.capacity() * size_of::<(BlockPos, u32)>()
    }

    /// The block entities of a chunk.
    pub fn in_chunk(&self, chunk: ChunkPos) -> impl Iterator<Item = (BlockPos, &BlockEntity)> {
        self.chunks
            .get(&chunk)
            .into_iter()
            .flat_map(|entries| entries.iter().map(|(&pos, e)| (pos, &e.entity)))
    }

    /// Marks a chunk whose block entity data changed (written back on save).
    pub fn touch(&mut self, chunk: ChunkPos) {
        self.touched.insert(chunk);
    }

    /// Drops the block entities of an unloaded chunk.
    fn remove_chunk(&mut self, chunk: ChunkPos) {
        if let Some(entries) = self.chunks.remove(&chunk) {
            for (pos, entry) in entries {
                if matches!(entry.entity, BlockEntity::EndGateway(_)) {
                    self.gateways -= 1;
                }
                if let Some(listener) = listener_of(pos, &entry.entity) {
                    self.listeners.unregister(listener);
                }
            }
        }
        self.touched.remove(&chunk);
    }
}

/// The ticker of a block entity kind (`EntityBlock.getTicker`).
fn ticker_of(entity: &BlockEntity) -> Option<TickerFn> {
    match entity {
        BlockEntity::Comparator { .. }
        | BlockEntity::Lectern(_)
        | BlockEntity::Container(_)
        | BlockEntity::EnderChest { .. }
        | BlockEntity::EndPortal
        | BlockEntity::Skull(_)
        | BlockEntity::Banner(_)
        | BlockEntity::DecoratedPot(_)
        | BlockEntity::ChiseledBookshelf(_)
        | BlockEntity::Brushable(_)
        | BlockEntity::CommandBlock(_)
        | BlockEntity::StructureBlock(_)
        | BlockEntity::Jigsaw(_)
        | BlockEntity::TestBlock(_)
        | BlockEntity::TestInstance(_)
        | BlockEntity::EnchantingTable(_)
        | BlockEntity::Shelf(_)
        | BlockEntity::CopperGolemStatue(_) => None,
        BlockEntity::PotentSulfur { .. } => Some(crate::block::geyser::tick),
        BlockEntity::Campfire(_) => Some(crate::block::campfire::tick),
        BlockEntity::Beehive(_) => Some(crate::block::beehive::tick),
        BlockEntity::Crafter(_) => Some(crate::block::crafter::server_tick),
        BlockEntity::Beacon(_) => Some(crate::block::beacon::tick),
        BlockEntity::Conduit(_) => Some(crate::block::beacon::conduit_tick),
        BlockEntity::Furnace(_) => Some(crate::block::furnace::server_tick),
        BlockEntity::BrewingStand(_) => Some(crate::block::brewing::server_tick),
        BlockEntity::Sign(_) => Some(crate::block::sign::tick),
        BlockEntity::EndGateway(_) => Some(crate::block::portal::gateway_tick),
        BlockEntity::MovingPiston(_) => Some(crate::block::piston::moving_piston_tick),
        BlockEntity::Hopper(_) => Some(crate::block::hopper::push_items_tick),
        BlockEntity::DaylightDetector => Some(crate::block::sensor::daylight_detector_tick),
        BlockEntity::Bell(_) => Some(crate::block::bell::tick),
        BlockEntity::Jukebox(_) => Some(crate::block::jukebox::tick),
        BlockEntity::SculkSensor(_) => Some(crate::block::sculk::tick_block_entity),
        BlockEntity::Spawner(_) => Some(crate::block::spawner::tick),
        BlockEntity::TrialSpawner(_) => Some(crate::block::trial_spawner::tick),
        BlockEntity::Vault(_) => Some(crate::block::vault::tick),
        BlockEntity::SculkShrieker(_) => Some(crate::block::sculk_shrieker::tick_block_entity),
        BlockEntity::SculkCatalyst(_) => Some(crate::block::sculk_catalyst::tick_block_entity),
        BlockEntity::CreakingHeart(_) => Some(crate::block::creaking_heart::server_tick),
    }
}

/// The game event listener of a block entity kind (`EntityBlock.getListener`).
fn listener_of(pos: BlockPos, entity: &BlockEntity) -> Option<Listener> {
    match entity {
        BlockEntity::SculkSensor(sensor) => Some(Listener::Block {
            pos,
            radius: sensor.listener_radius(),
        }),
        BlockEntity::SculkShrieker(_) => Some(Listener::Block {
            pos,
            radius: crate::block::sculk_shrieker::LISTENER_RADIUS,
        }),
        BlockEntity::SculkCatalyst(_) => Some(Listener::Block {
            pos,
            radius: crate::block::sculk_catalyst::LISTENER_RADIUS,
        }),
        _ => None,
    }
}

/// `EntityBlock.newBlockEntity(pos, state)` for the kinds that exist.
pub fn new_block_entity(state: BlockState) -> Option<BlockEntity> {
    use vanillite_data::block::ids;
    let block = state.block();
    if block == ids::COMPARATOR {
        Some(BlockEntity::Comparator { output: 0 })
    } else if block == ids::DAYLIGHT_DETECTOR {
        Some(crate::block::sensor::new_daylight_detector())
    } else if block == ids::BELL {
        Some(BlockEntity::Bell(Bell::default()))
    } else if block == ids::LECTERN {
        Some(BlockEntity::Lectern(Lectern::default()))
    } else if block == ids::JUKEBOX {
        Some(BlockEntity::Jukebox(Box::default()))
    } else if block == ids::SCULK_SENSOR || block == ids::CALIBRATED_SCULK_SENSOR {
        Some(BlockEntity::SculkSensor(Box::new(SculkSensor::new(
            block == ids::CALIBRATED_SCULK_SENSOR,
        ))))
    } else if block == ids::HOPPER {
        Some(BlockEntity::Hopper(Box::default()))
    } else if block == ids::SPAWNER {
        Some(BlockEntity::Spawner(Box::default()))
    } else if block == ids::TRIAL_SPAWNER {
        Some(BlockEntity::TrialSpawner(Box::default()))
    } else if block == ids::VAULT {
        Some(BlockEntity::Vault(Box::default()))
    } else if block == ids::SCULK_SHRIEKER {
        Some(BlockEntity::SculkShrieker(Box::default()))
    } else if block == ids::SCULK_CATALYST {
        Some(BlockEntity::SculkCatalyst(Box::default()))
    } else if crate::block::sign::is_sign(block) {
        Some(BlockEntity::Sign(Box::default()))
    } else if let Some(entity) = crate::block::technical::new_block_entity(state) {
        Some(entity)
    } else if block == ids::END_PORTAL {
        Some(BlockEntity::EndPortal)
    } else if block == ids::END_GATEWAY {
        Some(BlockEntity::EndGateway(
            crate::block::portal::Gateway::default(),
        ))
    } else if block == ids::DECORATED_POT {
        Some(BlockEntity::DecoratedPot(Box::default()))
    } else if block == ids::ENCHANTING_TABLE {
        Some(BlockEntity::EnchantingTable(None))
    } else if block == ids::POTENT_SULFUR {
        Some(BlockEntity::PotentSulfur { countdown: -1 })
    } else if block.info().kind == vanillite_data::block::BlockKind::ShelfBlock {
        Some(BlockEntity::Shelf(Box::default()))
    } else if is_golem_statue(block) {
        Some(BlockEntity::CopperGolemStatue(None))
    } else if block == ids::CHISELED_BOOKSHELF {
        Some(BlockEntity::ChiseledBookshelf(Box::default()))
    } else if block == ids::BEACON {
        Some(BlockEntity::Beacon(Box::default()))
    } else if block == ids::CONDUIT {
        Some(BlockEntity::Conduit(Box::default()))
    } else if block == ids::CRAFTER {
        Some(BlockEntity::Crafter(Box::new(
            crate::block::crafter::Crafter {
                triggered: state.value("triggered") == Some("true"),
                ..Default::default()
            },
        )))
    } else if crate::block::brushable::is_brushable(state) {
        Some(BlockEntity::Brushable(Box::default()))
    } else if crate::block::campfire::is_campfire(block) {
        Some(BlockEntity::Campfire(Box::default()))
    } else if crate::block::beehive::is_beehive(block) {
        Some(BlockEntity::Beehive(Box::default()))
    } else if crate::block::decor::is_skull(block) {
        Some(BlockEntity::Skull(Box::default()))
    } else if crate::block::decor::is_banner(block) {
        Some(BlockEntity::Banner(Box::default()))
    } else if block == ids::CREAKING_HEART {
        Some(BlockEntity::CreakingHeart(Box::default()))
    } else {
        crate::block::container::new_block_entity(block)
    }
}

/// The copper golem statue blocks (all weathering and waxed variants).
pub fn is_golem_statue(block: vanillite_data::block::BlockId) -> bool {
    use vanillite_data::block::BlockKind;
    matches!(
        block.info().kind,
        BlockKind::CopperGolemStatueBlock | BlockKind::WeatheringCopperGolemStatueBlock
    )
}

/// Whether `entity` belongs to a block of `state` (`BlockEntityType.isValid(state)`).
fn is_valid(entity: &BlockEntity, state: BlockState) -> bool {
    use vanillite_data::block::ids;
    match entity {
        BlockEntity::Comparator { .. } => state.block() == ids::COMPARATOR,
        BlockEntity::MovingPiston(_) => state.block() == ids::MOVING_PISTON,
        BlockEntity::DaylightDetector => state.block() == ids::DAYLIGHT_DETECTOR,
        BlockEntity::Bell(_) => state.block() == ids::BELL,
        BlockEntity::Lectern(_) => state.block() == ids::LECTERN,
        BlockEntity::Jukebox(_) => state.block() == ids::JUKEBOX,
        BlockEntity::SculkSensor(sensor) => {
            state.block()
                == if sensor.calibrated {
                    ids::CALIBRATED_SCULK_SENSOR
                } else {
                    ids::SCULK_SENSOR
                }
        }
        BlockEntity::Hopper(_) => state.block() == ids::HOPPER,
        BlockEntity::Spawner(_) => state.block() == ids::SPAWNER,
        BlockEntity::TrialSpawner(_) => state.block() == ids::TRIAL_SPAWNER,
        BlockEntity::Vault(_) => state.block() == ids::VAULT,
        BlockEntity::SculkShrieker(_) => state.block() == ids::SCULK_SHRIEKER,
        BlockEntity::SculkCatalyst(_) => state.block() == ids::SCULK_CATALYST,
        BlockEntity::Container(c) => {
            use crate::block::container::{Kind, kind_of, size_of};
            matches!(
                kind_of(state.block()),
                Some(
                    k @ (Kind::Chest
                    | Kind::TrappedChest
                    | Kind::Barrel
                    | Kind::ShulkerBox
                    | Kind::Dispenser
                    | Kind::Dropper)
                ) if size_of(k) == c.items.len()
            )
        }
        BlockEntity::EnderChest { .. } => state.block() == ids::ENDER_CHEST,
        BlockEntity::Furnace(f) => {
            crate::block::container::kind_of(state.block())
                == Some(crate::block::container::Kind::Furnace(f.kind))
        }
        BlockEntity::BrewingStand(_) => state.block() == ids::BREWING_STAND,
        BlockEntity::Sign(_) => crate::block::sign::is_sign(state.block()),
        BlockEntity::EndPortal => state.block() == ids::END_PORTAL,
        BlockEntity::EndGateway(_) => state.block() == ids::END_GATEWAY,
        BlockEntity::Skull(_) => crate::block::decor::is_skull(state.block()),
        BlockEntity::Banner(_) => crate::block::decor::is_banner(state.block()),
        BlockEntity::DecoratedPot(_) => state.block() == ids::DECORATED_POT,
        BlockEntity::ChiseledBookshelf(_) => state.block() == ids::CHISELED_BOOKSHELF,
        BlockEntity::Campfire(_) => crate::block::campfire::is_campfire(state.block()),
        BlockEntity::Beehive(_) => crate::block::beehive::is_beehive(state.block()),
        BlockEntity::Brushable(_) => crate::block::brushable::is_brushable(state),
        BlockEntity::Crafter(_) => state.block() == ids::CRAFTER,
        BlockEntity::Beacon(_) => state.block() == ids::BEACON,
        BlockEntity::Conduit(_) => state.block() == ids::CONDUIT,
        BlockEntity::CommandBlock(_) => {
            state.block().info().kind == vanillite_data::block::BlockKind::CommandBlock
        }
        BlockEntity::StructureBlock(_) => state.block() == ids::STRUCTURE_BLOCK,
        BlockEntity::Jigsaw(_) => state.block() == ids::JIGSAW,
        BlockEntity::TestBlock(_) => state.block() == ids::TEST_BLOCK,
        BlockEntity::TestInstance(_) => state.block() == ids::TEST_INSTANCE_BLOCK,
        BlockEntity::EnchantingTable(_) => state.block() == ids::ENCHANTING_TABLE,
        BlockEntity::PotentSulfur { .. } => state.block() == ids::POTENT_SULFUR,
        BlockEntity::Shelf(_) => {
            state.block().info().kind == vanillite_data::block::BlockKind::ShelfBlock
        }
        BlockEntity::CopperGolemStatue(_) => is_golem_statue(state.block()),
        BlockEntity::CreakingHeart(_) => state.block() == ids::CREAKING_HEART,
    }
}

/// `shouldChangedStateKeepBlockEntity`: copper chests keep their block entity (and stacks) when
/// they weather or get waxed.
fn keeps_block_entity(old: BlockState, new: BlockState) -> bool {
    let copper_chests = &vanillite_worldgen::behaviour::tags().blocks.copper_chests;
    copper_chests.contains(old) && copper_chests.contains(new)
}

impl Level {
    /// `getBlockEntity(pos)`.
    #[inline]
    pub fn block_entity(&self, pos: BlockPos) -> Option<&BlockEntity> {
        self.block_entities.get(pos)
    }

    /// `getBlockEntity(pos)`, mutably.
    #[inline]
    pub fn block_entity_mut(&mut self, pos: BlockPos) -> Option<&mut BlockEntity> {
        self.block_entities.get_mut(pos)
    }

    /// `setBlockEntity(blockEntity)`: only where the block fits the block entity.
    pub fn set_block_entity(&mut self, pos: BlockPos, entity: BlockEntity) {
        if is_valid(&entity, self.block(pos)) {
            self.block_entities.insert(pos, entity);
        }
    }

    /// `removeBlockEntity(pos)`.
    pub fn remove_block_entity(&mut self, pos: BlockPos) -> Option<BlockEntity> {
        self.block_entities.remove(pos)
    }

    /// The block entity part of `LevelChunk.setBlockState` before `affectNeighborsAfterRemoval`:
    /// the old block's block entity goes when the block changes.
    pub(super) fn block_entity_removed(
        &mut self,
        old: BlockState,
        new: BlockState,
        pos: BlockPos,
        flags: u32,
    ) {
        if old.block() != new.block()
            && old.block().info().block_entity != u16::MAX
            && !keeps_block_entity(old, new)
        {
            let side_effects = flags & super::update::SKIP_BLOCK_ENTITY_SIDEEFFECTS == 0;
            if side_effects {
                // `preRemoveSideEffects`, while the block entity is still there:
                // `Containers.dropContents` reads the stacks, which rolls a loot table first
                // (shulker boxes drop nothing here).
                if crate::block::container::kind_of(old.block())
                    != Some(crate::block::container::Kind::ShulkerBox)
                {
                    crate::block::chest_loot::unpack(self, pos, None);
                }
                if let Some(entity) = self.block_entities.get(pos).cloned() {
                    crate::block::container::pre_remove_side_effects(self, pos, old, &entity);
                    if let BlockEntity::Lectern(lectern) = &entity {
                        crate::block::lectern::drop_book(self, pos, old, lectern);
                    }
                    if let BlockEntity::Jukebox(_) = entity {
                        crate::block::jukebox::pop_out_the_item(self, pos);
                    }
                    if let BlockEntity::Beacon(_) = entity {
                        crate::block::beacon::removed(self, pos);
                    }
                    if let BlockEntity::SculkShrieker(_) = entity {
                        crate::block::sculk_shrieker::pre_remove(self, pos, old);
                    }
                    if let BlockEntity::CreakingHeart(_) = entity {
                        crate::block::creaking_heart::remove_protector(self, pos, None);
                    }
                    if let BlockEntity::Furnace(_) = entity {
                        let xp = crate::block::furnace::pop_experience(self, pos);
                        self.award_experience(crate::block::furnace::center(pos), xp);
                    }
                }
            }
            if let Some(BlockEntity::Jukebox(_)) = self.block_entities.remove(pos) {
                // `setRemoved`.
                crate::block::jukebox::removed(self, pos, old);
            }
        }
    }

    /// `ExperienceOrb.award(level, pos, amount)`: experience orbs at `pos`.
    pub fn award_experience(&mut self, pos: vanillite_base::Vec3, amount: i32) {
        crate::experience_orb::award(self, pos, amount);
    }

    /// The block entity part of `LevelChunk.setBlockState` after `onPlace`: a block with a
    /// block entity gets a fresh one unless a fitting one is there.
    pub(super) fn block_entity_placed(&mut self, state: BlockState, pos: BlockPos) {
        if state.block().info().block_entity == u16::MAX {
            return;
        }
        if let Some(entity) = self.block_entities.get(pos) {
            if is_valid(entity, state) {
                return;
            }
            self.block_entities.remove(pos);
        }
        if let Some(entity) = new_block_entity(state) {
            // `ChunkHolder.broadcastBlockEntityIfNeeded`: a block entity with client data goes
            // out with the block change.
            if update_tag(&entity, state).is_some() {
                self.output().block_entity_updates.push(pos);
            }
            self.block_entities.insert(pos, entity);
        }
    }

    /// `tickBlockEntities`: pending tickers join, then every live ticker in a block ticking
    /// chunk runs if its block still fits its block entity; removed ones leave the list.
    pub fn tick_block_entities(&mut self) {
        let store = &mut self.block_entities;
        let pending = std::mem::take(&mut store.pending);
        store.tickers.extend(pending);
        let mut index = 0;
        let mut kept = 0;
        while index < self.block_entities.tickers.len() {
            let (pos, id) = self.block_entities.tickers[index];
            index += 1;
            let ticker = match self.block_entities.entry(pos) {
                Some(entry) if entry.ticker == Some(id) => ticker_of(&entry.entity),
                _ => continue,
            };
            let tickers = &mut self.block_entities.tickers;
            tickers[kept] = (pos, id);
            kept += 1;
            let Some(ticker) = ticker else {
                continue;
            };
            // `LevelChunk.isTicking`: inside the world border and in a ticking chunk.
            if !self.border.is_block_within_bounds(pos) || !self.is_block_ticking(pos.chunk()) {
                continue;
            }
            let state = self.block(pos);
            if self
                .block_entities
                .get(pos)
                .is_some_and(|entity| is_valid(entity, state))
            {
                ticker(self, pos, state);
            }
        }
        self.block_entities.tickers.truncate(kept);
    }
}

// --- Persistence -----------------------------------------------------------------------------

/// Whether the live world models the block entity of `state` (its stored data is replaced by
/// the live one on save).
fn modelled(state: BlockState) -> bool {
    use vanillite_data::block::ids;
    state.is(ids::MOVING_PISTON) || new_block_entity(state).is_some()
}

/// `BlockEntity.loadWithComponents`: the live block entity of `state` from its stored data.
fn load(state: BlockState, nbt: &Compound) -> Option<BlockEntity> {
    use vanillite_data::block::ids;
    let int = |name| match nbt.get(name) {
        Some(Tag::Int(v)) => Some(*v),
        _ => None,
    };
    if state.is(ids::MOVING_PISTON) {
        let moved = nbt
            .get("blockState")
            .and_then(read_state_codec)
            .unwrap_or(BlockState::AIR);
        let flag = |name| matches!(nbt.get(name), Some(Tag::Byte(v)) if *v != 0);
        let progress = match nbt.get("progress") {
            Some(Tag::Float(v)) => *v,
            _ => 0.0,
        };
        // `Direction.LEGACY_ID_CODEC`: a byte (older saves of Vanillite: an int).
        let facing = match nbt.get("facing") {
            Some(Tag::Byte(v)) => i32::from(*v),
            _ => int("facing").unwrap_or(0),
        };
        let direction = direction_from_3d(facing);
        let mut piston = MovingPiston::new(moved, direction, flag("extending"), flag("source"));
        piston.progress = progress;
        piston.progress_o = progress;
        return Some(BlockEntity::MovingPiston(Box::new(piston)));
    }
    let mut entity = new_block_entity(state)?;
    match &mut entity {
        BlockEntity::Comparator { output } => *output = int("OutputSignal").unwrap_or(0),
        BlockEntity::Spawner(s) => **s = crate::block::spawner::MobSpawner::load(nbt),
        BlockEntity::TrialSpawner(s) => {
            **s = crate::block::trial_spawner::TrialSpawner::load(nbt);
        }
        BlockEntity::Vault(v) => **v = crate::block::vault::Vault::load(nbt),
        BlockEntity::SculkSensor(sensor) => {
            sensor.last_vibration_frequency = int("last_vibration_frequency").unwrap_or(0);
            if let Some(Tag::Compound(l)) = nbt.get("listener") {
                sensor.vibration = crate::block::sculk_vibration::VibrationData::load(l, |_| None);
            }
        }
        BlockEntity::SculkShrieker(shrieker) => shrieker.load(nbt),
        BlockEntity::SculkCatalyst(catalyst) => catalyst.load(nbt),
        BlockEntity::Container(c) => c.load(nbt),
        BlockEntity::Furnace(f) => f.load(nbt),
        BlockEntity::BrewingStand(b) => b.load(nbt),
        BlockEntity::Sign(sign) => sign.load(nbt),
        BlockEntity::Skull(skull) => skull.load(nbt),
        BlockEntity::Banner(banner) => banner.load(nbt),
        BlockEntity::DecoratedPot(pot) => pot.load(nbt),
        BlockEntity::ChiseledBookshelf(shelf) => shelf.load(nbt),
        BlockEntity::Campfire(campfire) => campfire.load(nbt),
        BlockEntity::Beehive(hive) => hive.load(nbt),
        BlockEntity::Brushable(b) => b.load(nbt),
        BlockEntity::Crafter(c) => c.load(nbt),
        BlockEntity::Beacon(b) => b.load(nbt),
        BlockEntity::Conduit(c) => c.load(nbt),
        BlockEntity::Lectern(lectern) => lectern.load(nbt),
        BlockEntity::Jukebox(j) => j.load(nbt),
        BlockEntity::Hopper(h) => {
            h.lock = crate::block::lock::load(nbt);
            h.custom_name = crate::block::container::load_name(nbt);
            (h.loot_table, h.loot_seed) = crate::block::chest_loot::load(nbt);
            if h.loot_table.is_none() {
                crate::block::container::load_items(nbt, &mut h.items);
            }
            h.cooldown_time = int("TransferCooldown").unwrap_or(-1);
        }
        BlockEntity::EndGateway(gateway) => *gateway = crate::block::portal::Gateway::load(nbt),
        BlockEntity::CommandBlock(c) => c.load(nbt),
        BlockEntity::StructureBlock(b) => b.load(nbt),
        BlockEntity::Jigsaw(j) => j.load(nbt),
        BlockEntity::TestBlock(t) => t.load(nbt),
        BlockEntity::TestInstance(t) => t.load(nbt),
        BlockEntity::EnchantingTable(name) => {
            *name = crate::block::container::load_name(nbt).map(Box::new);
        }
        BlockEntity::Shelf(shelf) => shelf.load(nbt),
        BlockEntity::CopperGolemStatue(components) => {
            *components = match nbt.get("components") {
                Some(Tag::Compound(c)) => Some(Box::new(c.clone())),
                _ => None,
            };
        }
        BlockEntity::PotentSulfur { countdown } => {
            if let Some(v) = int("countdown") {
                *countdown = v;
            }
        }
        BlockEntity::CreakingHeart(h) => h.load(nbt),
        _ => {}
    }
    Some(entity)
}

/// `BlockEntity.saveWithFullMetadata`: the stored data of a live block entity.
fn save(entity: &BlockEntity, state: BlockState, pos: BlockPos) -> Compound {
    let kind = if let BlockEntity::MovingPiston(_) = entity {
        "minecraft:piston"
    } else {
        vanillite_data::registry::BLOCK_ENTITY_TYPE
            .name(state.block().info().block_entity)
            .unwrap_or("minecraft:empty")
    };
    let mut nbt = Compound::new();
    nbt.insert("id", Tag::String(kind.to_owned()));
    nbt.insert("x", Tag::Int(pos.x));
    nbt.insert("y", Tag::Int(pos.y));
    nbt.insert("z", Tag::Int(pos.z));
    match entity {
        BlockEntity::Comparator { output } => nbt.insert("OutputSignal", Tag::Int(*output)),
        BlockEntity::MovingPiston(piston) => {
            nbt.insert("blockState", write_state_codec(piston.moved_state));
            nbt.insert(
                "facing",
                Tag::Byte(i8::try_from(direction_to_3d(piston.direction)).unwrap_or(0)),
            );
            nbt.insert("progress", Tag::Float(piston.progress_o));
            nbt.insert("extending", Tag::Byte(i8::from(piston.extending)));
            nbt.insert("source", Tag::Byte(i8::from(piston.source)));
        }
        BlockEntity::Spawner(s) => s.save(&mut nbt, true),
        BlockEntity::TrialSpawner(s) => s.save(&mut nbt),
        BlockEntity::Vault(v) => v.save(&mut nbt),
        BlockEntity::SculkSensor(sensor) => {
            nbt.insert(
                "last_vibration_frequency",
                Tag::Int(sensor.last_vibration_frequency),
            );
            nbt.insert("listener", Tag::Compound(sensor.vibration.save(|_| None)));
        }
        BlockEntity::SculkShrieker(shrieker) => shrieker.save(&mut nbt),
        BlockEntity::SculkCatalyst(catalyst) => catalyst.save(&mut nbt),
        BlockEntity::Container(c) => c.save(
            &mut nbt,
            crate::block::container::kind_of(state.block())
                != Some(crate::block::container::Kind::ShulkerBox),
        ),
        BlockEntity::Furnace(f) => f.save(&mut nbt),
        BlockEntity::BrewingStand(b) => b.save(&mut nbt),
        BlockEntity::Sign(sign) => sign.save(&mut nbt),
        BlockEntity::Skull(skull) => skull.save(&mut nbt),
        BlockEntity::Banner(banner) => banner.save(&mut nbt),
        BlockEntity::DecoratedPot(pot) => pot.save(&mut nbt),
        BlockEntity::ChiseledBookshelf(shelf) => shelf.save(&mut nbt),
        BlockEntity::Campfire(campfire) => campfire.save(&mut nbt),
        BlockEntity::Beehive(hive) => hive.save(&mut nbt),
        BlockEntity::Brushable(b) => b.save(&mut nbt),
        BlockEntity::Crafter(c) => c.save(&mut nbt),
        BlockEntity::Beacon(b) => b.save(&mut nbt),
        BlockEntity::Conduit(c) => c.save(&mut nbt),
        BlockEntity::Lectern(lectern) => lectern.save(&mut nbt),
        BlockEntity::Jukebox(j) => j.save(&mut nbt),
        BlockEntity::Hopper(h) => {
            crate::block::lock::save(&h.lock, &mut nbt);
            crate::block::container::save_name(&h.custom_name, &mut nbt);
            if !crate::block::chest_loot::save(h.loot_table.as_deref(), h.loot_seed, &mut nbt) {
                crate::block::container::save_items_always(&h.items, &mut nbt);
            }
            nbt.insert("TransferCooldown", Tag::Int(h.cooldown_time));
        }
        BlockEntity::EndGateway(gateway) => gateway.save(&mut nbt),
        BlockEntity::CommandBlock(c) => c.save(&mut nbt),
        BlockEntity::StructureBlock(b) => b.save(&mut nbt),
        BlockEntity::Jigsaw(j) => j.save(&mut nbt),
        BlockEntity::TestBlock(t) => t.save(&mut nbt),
        BlockEntity::TestInstance(t) => t.save(&mut nbt),
        BlockEntity::EnchantingTable(name) => {
            crate::block::container::save_name(&name.as_deref().cloned(), &mut nbt);
        }
        BlockEntity::Shelf(shelf) => shelf.save(&mut nbt),
        BlockEntity::CopperGolemStatue(Some(c)) => {
            nbt.insert("components", Tag::Compound((**c).clone()));
        }
        BlockEntity::PotentSulfur { countdown } => nbt.insert("countdown", Tag::Int(*countdown)),
        BlockEntity::CreakingHeart(h) => h.save(&mut nbt),
        _ => {}
    }
    // `saveWithoutMetadata` always stores the components, empty or not.
    if nbt.get("components").is_none() {
        nbt.insert("components", Tag::Compound(Compound::new()));
    }
    nbt
}

/// `Direction.from3DDataValue`.
fn direction_from_3d(value: i32) -> Direction {
    match value.rem_euclid(6) {
        0 => Direction::Down,
        1 => Direction::Up,
        2 => Direction::North,
        3 => Direction::South,
        4 => Direction::West,
        _ => Direction::East,
    }
}

/// `Direction.get3DDataValue`.
fn direction_to_3d(direction: Direction) -> i32 {
    match direction {
        Direction::Down => 0,
        Direction::Up => 1,
        Direction::North => 2,
        Direction::South => 3,
        Direction::West => 4,
        Direction::East => 5,
    }
}

/// `NbtUtils.readBlockState`.
pub fn read_block_state(c: &Compound) -> BlockState {
    let Some(Tag::String(name)) = c.get("Name") else {
        return BlockState::AIR;
    };
    let Some(block) = vanillite_data::block::BlockId::by_name(name) else {
        return BlockState::AIR;
    };
    let mut state = block.default_state();
    if let Some(Tag::Compound(props)) = c.get("Properties") {
        for (key, value) in props.iter() {
            if let Tag::String(value) = value {
                state = state.with(key, value).unwrap_or(state);
            }
        }
    }
    state
}

/// `BlockState.CODEC` (`CONSTANT_OR_DISPATCH_CODEC`): the block id of a default state, else
/// `{id, properties}`; the `{Name, Properties}` of `NbtUtils` reads too. `None` for anything
/// else.
pub fn read_state_codec(tag: &Tag) -> Option<BlockState> {
    match tag {
        Tag::String(name) => vanillite_data::block::BlockId::by_name(name)
            .map(vanillite_data::block::BlockId::default_state),
        Tag::Compound(c) => match c.get("id") {
            Some(Tag::String(name)) => {
                let block = vanillite_data::block::BlockId::by_name(name)?;
                let mut state = block.default_state();
                if let Some(Tag::Compound(props)) = c.get("properties") {
                    for (key, value) in props.iter() {
                        if let Tag::String(value) = value {
                            state = state.with(key, value).unwrap_or(state);
                        }
                    }
                }
                Some(state)
            }
            _ => c.get("Name").is_some().then(|| read_block_state(c)),
        },
        _ => None,
    }
}

/// `BlockState.CODEC` of `state`: the block id for its default state, else
/// `{id, properties}` with every property.
pub fn write_state_codec(state: BlockState) -> Tag {
    let name = state.block().name().to_owned();
    if state == state.block().default_state() {
        return Tag::String(name);
    }
    let mut c = Compound::new();
    c.insert("id", Tag::String(name));
    let mut props = Compound::new();
    for (key, value) in state.properties() {
        props.insert(key, Tag::String(value.to_owned()));
    }
    c.insert("properties", Tag::Compound(props));
    Tag::Compound(c)
}

impl Level {
    /// `BlockEntity.saveWithFullMetadata` of the block entity at `pos` (`id`, `x`, `y`, `z`
    /// and its data): the live one, else the stored data of a kind the world does not model
    /// yet. `None` when the block has no block entity.
    pub fn block_entity_nbt(&self, pos: BlockPos) -> Option<Compound> {
        let state = self.block(pos);
        if let Some(entity) = self.block_entities.get(pos) {
            let mut nbt = save(entity, state, pos);
            if let Some(kept) = self.block_entities.kept_components(pos) {
                nbt.insert("components", Tag::Compound(kept.clone()));
            }
            return Some(nbt);
        }
        let kind = state.block().info().block_entity;
        if kind == u16::MAX || modelled(state) {
            return None;
        }
        let mut nbt = self
            .chunks
            .chunk(pos.chunk())
            .and_then(|c| c.block_entities.iter().find(|d| d.pos == pos).cloned())
            .filter(|d| d.kind == Some(kind))
            .and_then(|d| {
                let mut input = &d.nbt[..];
                let mut budget = vanillite_nbt::Budget::file();
                match vanillite_nbt::read::read_network(&mut input, &mut budget) {
                    Ok(Some(Tag::Compound(c))) => Some(c),
                    _ => None,
                }
            })
            .unwrap_or_default();
        let id = vanillite_data::registry::BLOCK_ENTITY_TYPE
            .name(kind)
            .unwrap_or("minecraft:empty");
        nbt.insert("id", Tag::String(id.to_owned()));
        nbt.insert("x", Tag::Int(pos.x));
        nbt.insert("y", Tag::Int(pos.y));
        nbt.insert("z", Tag::Int(pos.z));
        Some(nbt)
    }

    /// `BlockEntity.loadWithComponents(nbt)`, `setChanged` and `sendBlockUpdated` of the block
    /// entity at `pos` (the data commands, `setblock` and `fill` with block entity data,
    /// `clone`): the live one is rebuilt from `nbt`, a kind the world does not model keeps
    /// `nbt` as its stored data. `id`, `x`, `y` and `z` in `nbt` are ignored. Returns whether
    /// the block has a block entity.
    pub fn load_block_entity_nbt(&mut self, pos: BlockPos, nbt: &Compound) -> bool {
        let state = self.block(pos);
        let kind = state.block().info().block_entity;
        if kind == u16::MAX && !state.is(vanillite_data::block::ids::MOVING_PISTON) {
            return false;
        }
        if modelled(state) {
            let Some(entity) = load(state, nbt) else {
                return false;
            };
            let previous_auto = match self.block_entities.get(pos) {
                Some(BlockEntity::CommandBlock(c)) => Some(c.auto),
                _ => None,
            };
            let statue = matches!(entity, BlockEntity::CopperGolemStatue(_));
            self.block_entities.insert(pos, entity);
            if !statue {
                self.block_entities
                    .set_kept_components(pos, saved_components(nbt));
            }
            crate::block::technical::command_block::loaded(self, pos, previous_auto);
        } else {
            let mut data = nbt.clone();
            for key in ["id", "x", "y", "z"] {
                data.remove(key);
            }
            let id = vanillite_data::registry::BLOCK_ENTITY_TYPE
                .name(kind)
                .unwrap_or("minecraft:empty");
            data.insert("id", Tag::String(id.to_owned()));
            data.insert("x", Tag::Int(pos.x));
            data.insert("y", Tag::Int(pos.y));
            data.insert("z", Tag::Int(pos.z));
            let mut bytes = Vec::new();
            if vanillite_nbt::write::write_network_compound(&data, &mut bytes).is_err() {
                return false;
            }
            let Some(list) = self.chunks.block_entities_mut(pos.chunk()) else {
                return false;
            };
            let entry = BlockEntityData {
                pos,
                kind: Some(kind),
                nbt: bytes.into(),
            };
            match list.iter_mut().find(|d| d.pos == pos) {
                Some(old) => *old = entry,
                None => list.push(entry),
            }
        }
        self.block_entities.touch(pos.chunk());
        crate::block::technical::structure_block::loaded(self, pos);
        self.send_block_entity_updated(pos);
        true
    }

    /// Creates the live block entities of a chunk that became live from its stored data.
    ///
    /// `runPostLoad` puts them into the chunk's `Object2ObjectOpenHashMap<BlockPos,
    /// BlockEntity>` in stored order, then `registerAllBlockEntitiesAfterLevelLoad` adds their tickers in the map's
    /// iteration order: that is the order they tick in.
    pub(super) fn load_block_entities(&mut self, chunk: ChunkPos) {
        let Some(stored) = self.chunks.chunk(chunk).map(|c| c.block_entities.clone()) else {
            return;
        };
        let order = fastutil_map_order(&stored.iter().map(|d| d.pos).collect::<Vec<_>>());
        for data in order.into_iter().map(|i| &stored[i]) {
            let state = self.block(data.pos);
            if data.kind != Some(state.block().info().block_entity)
                && !state.is(vanillite_data::block::ids::MOVING_PISTON)
            {
                continue;
            }
            let mut input = &data.nbt[..];
            let mut budget = vanillite_nbt::Budget::file();
            let Ok(Some(Tag::Compound(nbt))) =
                vanillite_nbt::read::read_network(&mut input, &mut budget)
            else {
                continue;
            };
            if let Some(entity) = load(state, &nbt) {
                let statue = matches!(entity, BlockEntity::CopperGolemStatue(_));
                self.block_entities.insert(data.pos, entity);
                if !statue && let Some(kept) = saved_components(&nbt) {
                    self.block_entities
                        .set_kept_components(data.pos, Some(kept));
                    self.block_entities.touched.remove(&chunk);
                }
            }
        }
        self.block_entities.touched.remove(&chunk);
    }

    /// Writes the live block entities of a chunk into its stored list: live ones replace
    /// their stored data, stored data of modelled kinds without a live block entity and of
    /// blocks that no longer have that block entity goes.
    fn write_block_entities(&mut self, chunk: ChunkPos) {
        let Some(stored) = self.chunks.chunk(chunk).map(|c| c.block_entities.clone()) else {
            return;
        };
        let mut list: Vec<BlockEntityData> = stored
            .into_iter()
            .filter(|data| {
                let state = self.block(data.pos);
                data.kind == Some(state.block().info().block_entity) && !modelled(state)
            })
            .collect();
        for (pos, entity) in self.block_entities.in_chunk(chunk) {
            let state = self.block(pos);
            let mut nbt = save(entity, state, pos);
            if let Some(kept) = self.block_entities.kept_components(pos) {
                nbt.insert("components", Tag::Compound(kept.clone()));
            }
            let mut bytes = Vec::new();
            if vanillite_nbt::write::write_network_compound(&nbt, &mut bytes).is_err() {
                continue;
            }
            let kind = match nbt.get("id") {
                Some(Tag::String(id)) => vanillite_data::registry::BLOCK_ENTITY_TYPE.id(id),
                _ => None,
            };
            list.push(BlockEntityData {
                pos,
                kind,
                nbt: bytes.into(),
            });
        }
        let unchanged = self.chunks.chunk(chunk).is_some_and(|c| {
            c.block_entities.len() == list.len()
                && c.block_entities
                    .iter()
                    .zip(&list)
                    .all(|(a, b)| a.pos == b.pos && a.kind == b.kind && a.nbt == b.nbt)
        });
        if !unchanged && let Some(target) = self.chunks.block_entities_mut(chunk) {
            *target = list;
        }
        self.block_entities.touched.remove(&chunk);
    }

    /// The chunk runtime's tick (`Chunks::tick`) with the block entities of chunks that unload
    /// written back first and those of chunks that became live loaded.
    pub fn tick_chunk_runtime(&mut self) {
        let mut chunks = std::mem::take(&mut self.chunk_scratch);
        chunks.clear();
        self.chunks.unloading_next_tick(&mut chunks);
        if let Some(hooks) = self.hooks_for(vanillite_abi::event::CHUNK_UNLOAD) {
            for &chunk in &chunks {
                let mut ev = crate::hooks::Event {
                    pos: BlockPos::new(chunk.x, 0, chunk.z),
                    ..crate::hooks::Event::new(vanillite_abi::event::CHUNK_UNLOAD)
                };
                hooks.event(crate::hooks::HookWorld::of(self), &mut ev);
            }
        }
        for &chunk in &chunks {
            self.write_block_entities(chunk);
        }
        self.chunks.tick();
        for &chunk in &chunks {
            if self.chunks.chunk(chunk).is_none() {
                self.block_entities.remove_chunk(chunk);
            }
        }
        chunks.clear();
        self.chunks.take_promoted(&mut chunks);
        for &chunk in &chunks {
            self.take_generation_entities(chunk);
            self.load_block_entities(chunk);
        }
        self.chunks_loaded_event(&chunks);
        self.apply_visibility();
        self.chunk_scratch = chunks;
    }

    /// Fires `CHUNK_LOAD` for chunks that became live (`flags` 1: generated now).
    pub(super) fn chunks_loaded_event(&self, chunks: &[ChunkPos]) {
        let Some(hooks) = self.hooks_for(vanillite_abi::event::CHUNK_LOAD) else {
            return;
        };
        for &chunk in chunks {
            let mut ev = crate::hooks::Event {
                pos: BlockPos::new(chunk.x, 0, chunk.z),
                flags: u32::from(self.chunks.generated_now(chunk)),
                ..crate::hooks::Event::new(vanillite_abi::event::CHUNK_LOAD)
            };
            hooks.event(crate::hooks::HookWorld::of(self), &mut ev);
        }
    }

    /// The chunks that became live in the last [`Level::tick_chunk_runtime`].
    pub fn promoted_chunks(&self) -> &[ChunkPos] {
        &self.chunk_scratch
    }

    /// Saves the world's chunks with their live block entities written back. `flush` waits
    /// until everything is on disk and first completes the pending entity loads
    /// (`PersistentEntitySectionManager.saveAll`).
    pub fn save(&mut self, flush: bool) {
        if flush {
            self.tick_entity_manager();
        }
        let mut chunks: Vec<ChunkPos> = self.block_entities.chunks.keys().copied().collect();
        chunks.extend(self.block_entities.touched.iter().copied());
        chunks.sort_unstable_by_key(|c| (c.x, c.z));
        chunks.dedup();
        for chunk in chunks {
            self.write_block_entities(chunk);
        }
        self.write_all_entities();
        self.write_all_poi();
        self.chunks.save_all(flush);
    }
}

/// `BlockEntity.getUpdateTag`: what clients see of a block entity, `None` for kinds whose
/// client data is empty.
pub fn update_tag(entity: &BlockEntity, state: BlockState) -> Option<Compound> {
    match entity {
        BlockEntity::Sign(sign) => {
            let mut nbt = Compound::new();
            sign.save(&mut nbt);
            nbt.sort_java_hash_order();
            Some(nbt)
        }
        BlockEntity::Skull(skull) => {
            let mut nbt = Compound::new();
            skull.save(&mut nbt);
            Some(nbt)
        }
        BlockEntity::Banner(banner) => {
            let mut nbt = Compound::new();
            banner.save(&mut nbt);
            Some(nbt)
        }
        BlockEntity::DecoratedPot(pot) => {
            let mut nbt = Compound::new();
            pot.save(&mut nbt);
            Some(nbt)
        }
        BlockEntity::Campfire(campfire) => Some(campfire.update_tag()),
        BlockEntity::Brushable(b) => Some(b.update_tag()),
        // `saveCustomOnly`.
        BlockEntity::Beacon(b) => {
            let mut nbt = Compound::new();
            b.save(&mut nbt);
            Some(nbt)
        }
        BlockEntity::Conduit(c) => {
            let mut nbt = Compound::new();
            c.save(&mut nbt);
            Some(nbt)
        }
        BlockEntity::Spawner(s) => Some(crate::block::spawner::update_tag(s)),
        BlockEntity::TrialSpawner(s) => {
            Some(s.update_tag(crate::block::trial_spawner::TrialState::of(state)))
        }
        BlockEntity::Vault(v) => Some(v.update_tag()),
        BlockEntity::Shelf(shelf) => {
            let mut nbt = Compound::new();
            shelf.save(&mut nbt);
            Some(nbt)
        }
        BlockEntity::CopperGolemStatue(_) => Some(Compound::new()),
        // `saveCustomOnly`: the age for the beam and the exit.
        BlockEntity::EndGateway(gateway) => {
            let mut nbt = Compound::new();
            gateway.save(&mut nbt);
            nbt.sort_java_hash_order();
            Some(nbt)
        }
        BlockEntity::StructureBlock(b) => b.update_tag(),
        BlockEntity::Jigsaw(j) => j.update_tag(),
        BlockEntity::TestBlock(t) => t.update_tag(),
        BlockEntity::TestInstance(t) => t.update_tag(),
        _ => None,
    }
}

// --- Components ------------------------------------------------------------------------------

/// `BlockEntity.collectComponents`: the value of component `id` the block entity puts on the
/// item it drops (loot `copy_components`), as a wire value: its own (implicit) value, else the
/// one it keeps (`kept`, [`BlockEntities::kept_components`]).
pub fn collect_component_with(
    entity: &BlockEntity,
    kept: Option<&Compound>,
    id: u16,
) -> Option<Box<[u8]>> {
    if let Some(v) = collect_component(entity, id) {
        return Some(v);
    }
    let kept = kept?;
    let name = crate::item_nbt::component_type_name(id)?;
    crate::item_nbt::component_from_nbt(id, kept.get(name)?)
}

/// The `"components"` of saved block entity data.
fn saved_components(nbt: &Compound) -> Option<Compound> {
    match nbt.get("components") {
        Some(Tag::Compound(c)) if !c.is_empty() => Some(c.clone()),
        _ => None,
    }
}

/// The components a block entity uses itself (`applyImplicitComponents`); the others of the
/// placed stack it keeps.
const IMPLICIT_COMPONENTS: [u16; 11] = [
    components::BLOCK_ENTITY_DATA,
    components::BLOCK_STATE,
    components::CUSTOM_NAME,
    components::LOCK,
    components::CONTAINER,
    components::CONTAINER_LOOT,
    components::BANNER_PATTERNS,
    components::PROFILE,
    components::NOTE_BLOCK_SOUND,
    components::POT_DECORATIONS,
    components::BEES,
];

/// What signs use themselves besides [`IMPLICIT_COMPONENTS`].
const SIGN_COMPONENTS: [u16; 3] = [
    components::SIGN_TEXT_FRONT,
    components::SIGN_TEXT_BACK,
    components::WAXED,
];

/// `BlockEntity.applyComponents`: the stack's component patch without the implicit
/// components (of a sign also its texts and `waxed`), in its saved form.
fn kept_of(stack: &ItemStack, sign: bool) -> Option<Compound> {
    let patch = stack.patch()?;
    let mut kept = patch.clone();
    let own: &[u16] = if sign { &SIGN_COMPONENTS } else { &[] };
    for &id in IMPLICIT_COMPONENTS.iter().chain(own) {
        kept.unset(id);
        if let Ok(i) = kept.removed.binary_search(&id) {
            kept.removed.remove(i);
        }
    }
    let (nbt, _) = crate::item_nbt::patch_to_nbt(&kept);
    (!nbt.is_empty()).then_some(nbt)
}

/// [`collect_component_with`] without kept components.
pub fn collect_component(entity: &BlockEntity, id: u16) -> Option<Box<[u8]>> {
    if id == components::LOCK {
        let lock = crate::block::lock::slot(entity)?.as_ref()?;
        return crate::block::lock::component(lock);
    }
    match entity {
        BlockEntity::Skull(skull) => skull.component(id),
        BlockEntity::Banner(banner) => banner.component(id),
        BlockEntity::DecoratedPot(pot) => pot.component(id),
        BlockEntity::ChiseledBookshelf(shelf) => {
            (id == components::CONTAINER).then(|| container_value(&shelf.items))
        }
        BlockEntity::Campfire(campfire) => {
            (id == components::CONTAINER).then(|| container_value(&campfire.items))
        }
        BlockEntity::Shelf(shelf) => {
            (id == components::CONTAINER).then(|| container_value(&shelf.items))
        }
        BlockEntity::EnchantingTable(name) => {
            if id == components::CUSTOM_NAME {
                encode_name(name.as_deref())
            } else {
                None
            }
        }
        BlockEntity::Beehive(hive) => hive.component(id),
        BlockEntity::Sign(sign) => match id {
            components::SIGN_TEXT_FRONT => sign.front.component(id),
            components::SIGN_TEXT_BACK => sign.back.component(id),
            components::WAXED => sign.waxed.then(|| Box::from([].as_slice())),
            _ => None,
        },
        BlockEntity::CommandBlock(block) => {
            if id == components::CUSTOM_NAME {
                encode_name(block.base.custom_name.as_ref())
            } else {
                None
            }
        }
        BlockEntity::Crafter(crafter) => {
            if id == components::CONTAINER {
                Some(container_value(&crafter.items))
            } else if id == components::CUSTOM_NAME {
                encode_name(crafter.custom_name.as_ref())
            } else if id == components::CONTAINER_LOOT {
                let table = crafter.loot_table.as_deref()?;
                Some(crate::block::chest_loot::component(
                    table,
                    crafter.loot_seed,
                ))
            } else {
                None
            }
        }
        _ => crate::block::container::component(entity, id),
    }
}

/// `ItemContainerContents.fromItems`: always a value (empty contents are an empty list).
fn container_value(items: &[ItemStack]) -> Box<[u8]> {
    vanillite_protocol::item::encode_container_contents(items).unwrap_or_else(|| Box::new([0]))
}

/// `CompoundTag.merge`: compounds merge key by key, everything else is replaced.
fn merge_compound(target: &mut Compound, other: &Compound) {
    for (key, value) in other.iter() {
        match (target.get_mut(key), value) {
            (Some(Tag::Compound(old)), Tag::Compound(new)) => merge_compound(old, new),
            _ => target.insert(key, value.clone()),
        }
    }
}

/// Whether the stack's `block_entity_data` holds `key` (`TypedEntityData.contains`).
pub fn block_entity_data_contains(stack: &ItemStack, key: &str) -> bool {
    let Some(wire) = stack.component(components::BLOCK_ENTITY_DATA) else {
        return false;
    };
    let mut reader = vanillite_protocol::codec::Reader::new(wire);
    if reader.varint().is_err() {
        return false;
    }
    let mut rest = reader.rest();
    let mut budget = vanillite_nbt::Budget::file();
    matches!(
        vanillite_nbt::read::read_network(&mut rest, &mut budget),
        Ok(Some(Tag::Compound(data))) if data.get(key).is_some()
    )
}

/// `BlockItem.updateCustomBlockEntityTag`: the item's `block_entity_data` merged into the
/// placed block entity (`TypedEntityData.loadInto`), if the types match; the operator-only
/// kinds (command blocks, lecterns, signs, spawners and trial spawners) only for players
/// that may use game master blocks.
pub fn apply_block_entity_data(
    level: &mut Level,
    pos: BlockPos,
    stack: &ItemStack,
    game_master: bool,
) -> bool {
    use vanillite_data::block::{BlockKind, ids};
    let Some(wire) = stack.component(components::BLOCK_ENTITY_DATA) else {
        return false;
    };
    let mut reader = vanillite_protocol::codec::Reader::new(wire);
    let Ok(type_id) = reader.varint() else {
        return false;
    };
    let mut rest = reader.rest();
    let mut budget = vanillite_nbt::Budget::file();
    let Ok(Some(Tag::Compound(data))) = vanillite_nbt::read::read_network(&mut rest, &mut budget)
    else {
        return false;
    };
    let state = level.block(pos);
    let kind = state.block().info().block_entity;
    if kind == u16::MAX || i32::from(kind) != type_id {
        return false;
    }
    let op_only = matches!(state.block().info().kind, BlockKind::CommandBlock)
        || state.is(ids::LECTERN)
        || state.is(ids::SPAWNER)
        || state.is(ids::TRIAL_SPAWNER)
        || crate::block::sign::is_sign(state.block());
    if op_only && !game_master {
        return false;
    }
    let Some(mut tag) = level.block_entity_nbt(pos) else {
        return false;
    };
    for key in ["id", "x", "y", "z"] {
        tag.remove(key);
    }
    let old = tag.clone();
    merge_compound(&mut tag, &data);
    tag != old && level.load_block_entity_nbt(pos, &tag)
}

/// `BlockEntity.applyComponentsFromItemStack` of a placed block item.
pub fn apply_components(level: &mut Level, pos: BlockPos, stack: &ItemStack) {
    if !matches!(
        level.block_entity(pos),
        None | Some(BlockEntity::CopperGolemStatue(_))
    ) {
        let sign = matches!(level.block_entity(pos), Some(BlockEntity::Sign(_)));
        level
            .block_entities
            .set_kept_components(pos, kept_of(stack, sign));
    }
    let contents = || {
        stack
            .component(components::CONTAINER)
            .and_then(|v| vanillite_protocol::item::decode_container_contents(v).ok())
    };
    if let Some(lock) = level
        .block_entity_mut(pos)
        .and_then(crate::block::lock::slot_mut)
    {
        *lock = crate::block::lock::from_stack(stack);
    }
    match level.block_entity_mut(pos) {
        Some(BlockEntity::Skull(skull)) => skull.apply(stack),
        Some(BlockEntity::Banner(banner)) => banner.apply(stack),
        Some(BlockEntity::DecoratedPot(pot)) => pot.apply(stack),
        Some(BlockEntity::ChiseledBookshelf(shelf)) => {
            if let Some(items) = contents() {
                copy_into(&items, &mut shelf.items);
            }
        }
        Some(BlockEntity::Campfire(campfire)) => {
            if let Some(items) = contents() {
                copy_into(&items, &mut campfire.items);
            }
        }
        Some(BlockEntity::Shelf(shelf)) => {
            if let Some(items) = contents() {
                copy_into(&items, &mut shelf.items);
            }
        }
        Some(BlockEntity::EnchantingTable(name)) => {
            *name = decode_name(stack).map(Box::new);
        }
        // `SignBlockEntity.applyImplicitComponents`: both texts (`SignText.EMPTY` without
        // them) and `waxed`.
        Some(BlockEntity::Sign(sign)) => {
            use crate::block::sign::SignText;
            sign.front = SignText::of_component(stack, components::SIGN_TEXT_FRONT);
            sign.back = SignText::of_component(stack, components::SIGN_TEXT_BACK);
            sign.waxed = stack.component(components::WAXED).is_some();
        }
        // `CommandBlockEntity.applyImplicitComponents`: the custom name.
        Some(BlockEntity::CommandBlock(block)) => {
            block.base.custom_name = decode_name(stack);
        }
        Some(BlockEntity::Beehive(hive)) => {
            hive.apply(stack);
            level.block_entities.touch(pos.chunk());
            crate::block::beehive::placed_changed(level, pos);
            return;
        }
        Some(BlockEntity::Crafter(crafter)) => {
            if let Some(name) = decode_name(stack) {
                crafter.custom_name = Some(name);
            }
            if let Some(items) = contents() {
                copy_into(&items, &mut crafter.items);
            }
            if let Some((table, seed)) = stack
                .component(components::CONTAINER_LOOT)
                .and_then(crate::block::chest_loot::read_component)
            {
                crafter.loot_table = Some(table);
                crafter.loot_seed = seed;
            }
        }
        _ => {
            crate::block::container::apply_item_components(level, pos, stack);
            return;
        }
    }
    level.block_entities.touch(pos.chunk());
    let state = level.block(pos);
    if level
        .block_entity(pos)
        .is_some_and(|e| update_tag(e, state).is_some())
    {
        level.output().block_entity_updates.push(pos);
    }
}

/// `ItemContainerContents.copyInto`: slots in order, the rest emptied.
fn copy_into(items: &[ItemStack], target: &mut [ItemStack]) {
    for (i, slot) in target.iter_mut().enumerate() {
        *slot = items.get(i).cloned().unwrap_or_default();
    }
}

/// The `custom_name` component of a stack.
pub fn decode_name(stack: &ItemStack) -> Option<Component> {
    let bytes = stack.component(components::CUSTOM_NAME)?;
    let mut r = vanillite_protocol::codec::Reader::new(bytes);
    <Component as vanillite_protocol::codec::DecodeLimited>::decode_limited(&mut r, 2_097_152).ok()
}

/// A `custom_name` component value.
pub fn encode_name(name: Option<&Component>) -> Option<Box<[u8]>> {
    use vanillite_protocol::codec::Encode;
    let mut out = Vec::new();
    name?.encode(&mut out);
    Some(out.into_boxed_slice())
}

/// Prefix of stored component values that have no persistent form (yet): their wire bytes.
const WIRE_PREFIX: &str = "vanillite:";

/// Stores a component value under `key` in its persistent form (or its wire bytes).
pub fn save_component(out: &mut Compound, key: &str, type_id: u16, wire: &[u8]) {
    match crate::item_nbt::component_to_nbt(type_id, wire) {
        Some(tag) => out.insert(key, tag),
        None => {
            let bytes: Vec<i8> = wire.iter().map(|b| i8::from_ne_bytes([*b])).collect();
            out.insert(format!("{WIRE_PREFIX}{key}"), Tag::ByteArray(bytes));
        }
    }
}

/// Reads what [`save_component`] stored; invalid values read as none.
pub fn load_component(nbt: &Compound, key: &str, type_id: u16) -> Option<Box<[u8]>> {
    if let Some(tag) = nbt.get(key) {
        return crate::item_nbt::component_from_nbt(type_id, tag);
    }
    let Some(Tag::ByteArray(bytes)) = nbt.get(&format!("{WIRE_PREFIX}{key}")) else {
        return None;
    };
    let wire: Vec<u8> = bytes.iter().map(|b| b.to_ne_bytes()[0]).collect();
    vanillite_protocol::components::validate(
        type_id,
        &wire,
        &vanillite_protocol::item::InstalledRegistries,
    )
    .ok()?;
    Some(wire.into_boxed_slice())
}

/// The indices of `keys`, put one after the other into a new fastutil
/// `Object2ObjectOpenHashMap<BlockPos, _>` (a key already present keeps its slot), in the order
/// the map iterates them: open addressing on `HashCommon.mix(Vec3i.hashCode)`, growing at three
/// quarters full, iterated from the last slot down.
pub(crate) fn fastutil_map_order(keys: &[BlockPos]) -> Vec<usize> {
    let mix = |pos: BlockPos| {
        let h = pos
            .y
            .wrapping_add(pos.z.wrapping_mul(31))
            .wrapping_mul(31)
            .wrapping_add(pos.x)
            .wrapping_mul(0x9E37_79B9_u32 as i32);
        (h ^ ((h as u32) >> 16) as i32) as u32 as usize
    };
    // `HashCommon.arraySize(expected, 0.75)` and `maxFill(n, 0.75)`.
    let array_size = |expected: usize| (expected * 4).div_ceil(3).next_power_of_two().max(2);
    let max_fill = |n: usize| (n * 3).div_ceil(4).min(n - 1);
    let mut slots: Vec<Option<usize>> = vec![None; array_size(16)];
    let mut size = 0;
    for (i, &key) in keys.iter().enumerate() {
        let mask = slots.len() - 1;
        let mut pos = mix(key) & mask;
        let mut present = false;
        while let Some(j) = slots[pos] {
            if keys[j] == key {
                present = true;
                break;
            }
            pos = (pos + 1) & mask;
        }
        if present {
            continue;
        }
        slots[pos] = Some(i);
        let before = size;
        size += 1;
        if before >= max_fill(slots.len()) {
            // `rehash`: the old slots from the last down.
            let n = array_size(size + 1);
            let mut grown = vec![None; n];
            for j in slots.iter().rev().flatten() {
                let mut pos = mix(keys[*j]) & (n - 1);
                while grown[pos].is_some() {
                    pos = (pos + 1) & (n - 1);
                }
                grown[pos] = Some(*j);
            }
            slots = grown;
        }
    }
    slots.into_iter().rev().flatten().collect()
}

// --- Pick block ------------------------------------------------------------------------------

/// A `block_state` component value with one property.
fn block_state_property(key: &str, value: &str) -> Box<[u8]> {
    let mut out = Vec::new();
    vanillite_protocol::codec::write_varint(&mut out, 1);
    vanillite_protocol::codec::write_str(&mut out, key);
    vanillite_protocol::codec::write_str(&mut out, value);
    out.into_boxed_slice()
}

/// The saved keys of the components a block entity collects (`removeComponentsFromTag`).
const COMPONENT_KEYS: [(u16, &[&str]); 10] = [
    (components::CUSTOM_NAME, &["CustomName"]),
    (components::LOCK, &["lock"]),
    (components::CONTAINER, &["Items", "item"]),
    (components::CONTAINER_LOOT, &["LootTable", "LootTableSeed"]),
    (components::BANNER_PATTERNS, &["patterns"]),
    (components::PROFILE, &["profile"]),
    (components::NOTE_BLOCK_SOUND, &["note_block_sound"]),
    (components::POT_DECORATIONS, &["sherds"]),
    (components::BEES, &["bees"]),
    (components::BLOCK_STATE, &[]),
];

/// `BlockState.getCloneItemStack(level, pos, includeData)` and, with `include_data`,
/// `ServerGamePacketListenerImpl.addBlockDataToItem`: the item a middle click picks, with the
/// components of banners, decorated pots, copper golem statues, light blocks and beehives;
/// with data, also the block entity's saved data and every component it collects.
pub fn pick_stack(level: &Level, pos: BlockPos, item: u16, include_data: bool) -> ItemStack {
    let state = level.block(pos);
    let entity = level.block_entities.get(pos);
    let kept = level.block_entities.kept_components(pos);
    let mut stack = ItemStack::new(item, 1);
    let collect_all = |stack: &mut ItemStack| {
        if let Some(entity) = entity {
            for (id, _) in COMPONENT_KEYS {
                if let Some(v) = collect_component(entity, id) {
                    stack.set_component(id, v);
                }
            }
        }
        if let Some(kept) = kept {
            let patch = crate::item_nbt::patch_from_nbt(kept);
            for (id, v) in patch.added {
                stack.set_component(id, v);
            }
            for id in patch.removed {
                stack.remove_component(id);
            }
        }
    };
    let block = state.block();
    let name = block.name();
    match entity {
        // `BannerBlockEntity.getItem` and `CopperGolemStatueBlockEntity.getItem`.
        Some(BlockEntity::Banner(_)) => collect_all(&mut stack),
        Some(BlockEntity::CopperGolemStatue(_)) => {
            collect_all(&mut stack);
            if let Some(pose) = state.value("copper_golem_pose") {
                stack.set_component(
                    components::BLOCK_STATE,
                    block_state_property("copper_golem_pose", pose),
                );
            }
        }
        // `createDecoratedPotInstance(decorations)`.
        Some(BlockEntity::DecoratedPot(pot)) => {
            if let Some(v) = pot.component(components::POT_DECORATIONS) {
                stack.set_component(components::POT_DECORATIONS, v);
            }
        }
        _ => {}
    }
    if name == "minecraft:light" {
        if let Some(l) = state.value("level") {
            stack.set_component(components::BLOCK_STATE, block_state_property("level", l));
        }
    } else if include_data
        && matches!(name, "minecraft:beehive" | "minecraft:bee_nest")
        && let Some(h) = state.value("honey_level")
    {
        stack.set_component(
            components::BLOCK_STATE,
            block_state_property("honey_level", h),
        );
    }
    if include_data && let Some(entity) = entity {
        // `saveCustomOnly` without the collected components, then `collectComponents`.
        if let Some(mut nbt) = level.block_entity_nbt(pos) {
            for key in ["id", "x", "y", "z", "components"] {
                nbt.remove(key);
            }
            for (id, keys) in COMPONENT_KEYS {
                if collect_component(entity, id).is_some() {
                    for key in keys {
                        nbt.remove(key);
                    }
                }
            }
            let kind = block.info().block_entity;
            if nbt.is_empty() || kind == u16::MAX {
                stack.remove_component(components::BLOCK_ENTITY_DATA);
            } else {
                nbt.sort_java_hash_order();
                let mut wire = Vec::new();
                vanillite_protocol::codec::write_varint(&mut wire, i32::from(kind));
                if vanillite_nbt::write::write_network(&Tag::Compound(nbt), &mut wire).is_ok() {
                    stack.set_component(components::BLOCK_ENTITY_DATA, wire.into_boxed_slice());
                }
            }
        }
        collect_all(&mut stack);
    }
    stack
}
