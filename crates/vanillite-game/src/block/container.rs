//! Container blocks and their block entities (`BaseContainerBlockEntity` and its users):
//! chests (single and double, trapped, copper), barrels, shulker boxes, ender chests,
//! dispensers, droppers and the inventory of hoppers; furnaces are in [`super::furnace`].
//!
//! The stacks of a container live in its block entity ([`Container`], [`BlockEntity::Hopper`],
//! [`BlockEntity::Furnace`]); [`items`] and [`items_mut`] reach them by position. Using a
//! container block asks the level to open a menu ([`Level::request_menu`]); the player side
//! ([`crate::menu::container`]) opens it and counts the viewers back here
//! ([`start_open`], [`stop_open`]): lid block events, sounds, the barrel's `open` state and the
//! trapped chest's signal follow the viewer count like Vanilla's `ContainerOpenersCounter`.

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockKind, BlockState};
use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::text::Component;
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::{PosExt, StateExt};

use super::{BlockBehaviour, Interaction, UseContext, sound_id};
use crate::item::ItemStack;
use crate::level::{BlockEntity, Level};
use crate::menu::container::{MenuKind, MenuRequest, Storage};
use crate::recipe::CookingKind;

/// `SoundSource.BLOCKS`.
const BLOCKS: u8 = 4;

/// The stacks and name of a container block entity (`RandomizableContainerBlockEntity`).
#[derive(Clone, Debug, PartialEq)]
pub struct Container {
    /// The stacks.
    pub items: Box<[ItemStack]>,
    /// `CustomName`.
    pub custom_name: Option<Component>,
    /// `LootTable`: the loot table that fills the container when it is first opened.
    pub loot_table: Option<String>,
    /// `LootTableSeed`.
    pub loot_seed: i64,
    /// Players that have it open (`ContainerOpenersCounter.openCount`).
    pub openers: i32,
    /// `lock` (`LockCode`).
    pub lock: Option<vanillite_nbt::Tag>,
}

impl Container {
    /// An empty container with `size` slots.
    pub fn new(size: usize) -> Container {
        Container {
            items: vec![ItemStack::EMPTY; size].into_boxed_slice(),
            custom_name: None,
            loot_table: None,
            loot_seed: 0,
            openers: 0,
            lock: None,
        }
    }
}

/// What kind of container a block is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Chests and copper chests.
    Chest,
    /// Trapped chests.
    TrappedChest,
    /// Barrels.
    Barrel,
    /// Shulker boxes.
    ShulkerBox,
    /// Dispensers.
    Dispenser,
    /// Droppers.
    Dropper,
    /// Hoppers.
    Hopper,
    /// Ender chests (the stacks are the player's).
    EnderChest,
    /// Furnaces, blast furnaces, smokers.
    Furnace(CookingKind),
    /// Crafting tables (no block entity).
    CraftingTable,
    /// Stonecutters.
    Stonecutter,
    /// Anvils (all damage stages).
    Anvil,
    /// Grindstones.
    Grindstone,
    /// Smithing tables.
    SmithingTable,
    /// Brewing stands.
    BrewingStand,
    /// Enchanting tables (their block entity only animates the book).
    EnchantingTable,
    /// Looms (no block entity).
    Loom,
    /// Cartography tables (no block entity).
    CartographyTable,
    Beacon,
}

/// The container kind of a block.
pub fn kind_of(block: BlockId) -> Option<Kind> {
    Some(match block.info().kind {
        BlockKind::ChestBlock
        | BlockKind::CopperChestBlock
        | BlockKind::WeatheringCopperChestBlock => Kind::Chest,
        BlockKind::TrappedChestBlock => Kind::TrappedChest,
        BlockKind::BarrelBlock => Kind::Barrel,
        BlockKind::ShulkerBoxBlock => Kind::ShulkerBox,
        BlockKind::DispenserBlock => Kind::Dispenser,
        BlockKind::DropperBlock => Kind::Dropper,
        BlockKind::HopperBlock => Kind::Hopper,
        BlockKind::EnderChestBlock => Kind::EnderChest,
        BlockKind::FurnaceBlock => Kind::Furnace(CookingKind::Smelting),
        BlockKind::BlastFurnaceBlock => Kind::Furnace(CookingKind::Blasting),
        BlockKind::SmokerBlock => Kind::Furnace(CookingKind::Smoking),
        BlockKind::CraftingTableBlock => Kind::CraftingTable,
        BlockKind::StonecutterBlock => Kind::Stonecutter,
        BlockKind::AnvilBlock => Kind::Anvil,
        BlockKind::GrindstoneBlock => Kind::Grindstone,
        BlockKind::SmithingTableBlock => Kind::SmithingTable,
        BlockKind::BrewingStandBlock => Kind::BrewingStand,
        BlockKind::EnchantingTableBlock => Kind::EnchantingTable,
        BlockKind::LoomBlock => Kind::Loom,
        BlockKind::CartographyTableBlock => Kind::CartographyTable,
        BlockKind::BeaconBlock => Kind::Beacon,
        _ => return None,
    })
}

/// Slots of a [`Container`] block entity kind.
pub fn size_of(kind: Kind) -> usize {
    match kind {
        Kind::Chest | Kind::TrappedChest | Kind::Barrel | Kind::ShulkerBox | Kind::EnderChest => 27,
        Kind::Dispenser | Kind::Dropper => 9,
        Kind::Hopper => 5,
        Kind::Furnace(_) => 3,
        Kind::BrewingStand => 5,
        Kind::CraftingTable
        | Kind::Stonecutter
        | Kind::Anvil
        | Kind::Grindstone
        | Kind::SmithingTable
        | Kind::EnchantingTable
        | Kind::Loom
        | Kind::CartographyTable
        | Kind::Beacon => 0,
    }
}

/// The new block entity of a container block (`newBlockEntity`).
pub fn new_block_entity(block: BlockId) -> Option<BlockEntity> {
    Some(match kind_of(block)? {
        Kind::EnderChest => BlockEntity::EnderChest { openers: 0 },
        Kind::Furnace(kind) => BlockEntity::Furnace(Box::new(super::furnace::Furnace::new(kind))),
        Kind::BrewingStand => BlockEntity::BrewingStand(Box::default()),
        Kind::Hopper
        | Kind::CraftingTable
        | Kind::Stonecutter
        | Kind::Anvil
        | Kind::Grindstone
        | Kind::SmithingTable
        | Kind::EnchantingTable
        | Kind::Loom
        | Kind::CartographyTable
        | Kind::Beacon => return None,
        kind => BlockEntity::Container(Box::new(Container::new(size_of(kind)))),
    })
}

/// Fills the behaviours of the container blocks.
pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    let Some(kind) = kind_of(block) else {
        return;
    };
    b.use_without_item = Some(use_without_item);
    match kind {
        Kind::Chest | Kind::TrappedChest | Kind::EnderChest | Kind::ShulkerBox => {
            b.trigger_event = Some(trigger_event);
        }
        _ => {}
    }
    if matches!(
        kind,
        Kind::Chest | Kind::TrappedChest | Kind::EnderChest | Kind::Barrel
    ) {
        b.tick = Some(recheck_openers);
    }
    match kind {
        Kind::Chest
        | Kind::TrappedChest
        | Kind::Barrel
        | Kind::ShulkerBox
        | Kind::Furnace(_)
        | Kind::BrewingStand => {
            b.affect_neighbors_after_removal = Some(super::dispenser::container_removed);
            b.analog_output = Some(super::dispenser::container_signal);
        }
        _ => {}
    }
    if matches!(
        kind,
        Kind::Chest | Kind::TrappedChest | Kind::Barrel | Kind::EnderChest
    ) {
        b.tick = Some(recheck_open);
    }
    if kind == Kind::TrappedChest {
        b.signal = Some(trapped_chest_signal);
        b.direct_signal = Some(trapped_chest_direct_signal);
    }
}

/// `TrappedChestBlock.getSignal`: the number of viewers, at most 15.
fn trapped_chest_signal(level: &Level, _: BlockState, pos: BlockPos, _: Direction) -> i32 {
    match level.block_entity(pos) {
        Some(BlockEntity::Container(c)) => c.openers.clamp(0, 15),
        _ => 0,
    }
}

/// `TrappedChestBlock.getDirectSignal`: only up.
fn trapped_chest_direct_signal(
    level: &Level,
    state: BlockState,
    pos: BlockPos,
    d: Direction,
) -> i32 {
    if d == Direction::Up {
        trapped_chest_signal(level, state, pos, d)
    } else {
        0
    }
}

/// `triggerEvent(1, count)` of chests, ender chests and shulker boxes: the lid animation goes
/// to the clients.
fn trigger_event(level: &mut Level, _: BlockState, pos: BlockPos, a: i32, _: i32) -> bool {
    a == 1 && level.block_entity(pos).is_some()
}

/// `useWithoutItem` of every container block: the menu opens (`player.openMenu`).
fn use_without_item(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    context: &UseContext,
) -> Interaction {
    if let Some(request) = menu_provider(level, state, pos) {
        level.request_menu(MenuRequest {
            player: Some(context.player),
            ..request
        });
        // `PiglinAi.angerNearbyPiglins(level, player, true)` of chests, barrels, ender chests
        // and shulker boxes.
        if matches!(
            kind_of(state.block()),
            Some(
                Kind::Chest
                    | Kind::TrappedChest
                    | Kind::Barrel
                    | Kind::EnderChest
                    | Kind::ShulkerBox
            )
        ) {
            level.output().piglin_anger.push((context.player, true));
        }
    }
    Interaction::Success
}

/// The text component of a translation key.
fn title(key: &str) -> Component {
    Component::translatable(key, vec![])
}

/// `getMenuProvider(state, level, pos)`: the menu a block opens, `None` if it cannot open
/// now (blocked chests and shulker boxes). Spectators open menus through this directly.
pub fn menu_provider(level: &Level, state: BlockState, pos: BlockPos) -> Option<MenuRequest> {
    let kind = kind_of(state.block())?;
    let named = |default: &str| -> Component {
        match level.block_entity(pos) {
            Some(BlockEntity::Container(c)) => c.custom_name.clone(),
            Some(BlockEntity::Hopper(h)) => h.custom_name.clone(),
            Some(BlockEntity::Furnace(f)) => f.custom_name.clone(),
            Some(BlockEntity::BrewingStand(b)) => b.custom_name.clone(),
            Some(BlockEntity::Beacon(b)) => b.custom_name.clone(),
            Some(BlockEntity::EnchantingTable(name)) => name.as_deref().cloned(),
            _ => None,
        }
        .unwrap_or_else(|| title(default))
    };
    let request = |menu: MenuKind, storage: Storage, title: Component| MenuRequest {
        player: None,
        menu,
        storage,
        title,
        pos,
        block: state.block(),
    };
    match kind {
        Kind::Chest | Kind::TrappedChest => {
            if !matches!(level.block_entity(pos), Some(BlockEntity::Container(_)))
                || chest_blocked(level, pos)
            {
                return None;
            }
            match double_chest_partner(level, state, pos) {
                Some(other) => {
                    if chest_blocked(level, other) {
                        return None;
                    }
                    // `FIRST` (the right half) holds slots 0–26.
                    let right_first = state.value("type") == Some("right");
                    let (first, second) = if right_first {
                        (pos, other)
                    } else {
                        (other, pos)
                    };
                    let name = |p: BlockPos| match level.block_entity(p) {
                        Some(BlockEntity::Container(c)) => c.custom_name.clone(),
                        _ => None,
                    };
                    let title = name(first)
                        .or_else(|| name(second))
                        .unwrap_or_else(|| self::title("container.chestDouble"));
                    Some(request(
                        MenuKind::Generic { rows: 6 },
                        Storage::Double(first, second),
                        title,
                    ))
                }
                None => Some(request(
                    MenuKind::Generic { rows: 3 },
                    Storage::Block(pos),
                    named("container.chest"),
                )),
            }
        }
        Kind::Barrel => Some(request(
            MenuKind::Generic { rows: 3 },
            Storage::Block(pos),
            named("container.barrel"),
        )),
        Kind::ShulkerBox => {
            let Some(BlockEntity::Container(c)) = level.block_entity(pos) else {
                return None;
            };
            if c.openers == 0 && shulker_lid_blocked(level, state, pos) {
                return None;
            }
            Some(request(
                MenuKind::ShulkerBox,
                Storage::Block(pos),
                named("container.shulkerBox"),
            ))
        }
        Kind::Dispenser => Some(request(
            MenuKind::Dispenser,
            Storage::Block(pos),
            named("container.dispenser"),
        )),
        Kind::Dropper => Some(request(
            MenuKind::Dispenser,
            Storage::Block(pos),
            named("container.dropper"),
        )),
        Kind::Hopper => Some(request(
            MenuKind::Hopper,
            Storage::Block(pos),
            named("container.hopper"),
        )),
        Kind::EnderChest => {
            if !matches!(
                level.block_entity(pos),
                Some(BlockEntity::EnderChest { .. })
            ) {
                return None;
            }
            let above = pos.above();
            if Level::is_redstone_conductor(level.block(above)) {
                return None;
            }
            Some(request(
                MenuKind::Generic { rows: 3 },
                Storage::Ender(pos),
                title("container.enderchest"),
            ))
        }
        Kind::Furnace(cooking) => {
            let key = match cooking {
                CookingKind::Blasting => "container.blast_furnace",
                CookingKind::Smoking => "container.smoker",
                _ => "container.furnace",
            };
            Some(request(
                MenuKind::Furnace(cooking),
                Storage::Block(pos),
                named(key),
            ))
        }
        Kind::CraftingTable => Some(request(
            MenuKind::Crafting,
            Storage::Local,
            title("container.crafting"),
        )),
        Kind::EnchantingTable => Some(request(
            MenuKind::Enchantment,
            Storage::Local,
            named("container.enchant"),
        )),
        Kind::BrewingStand => Some(request(
            MenuKind::BrewingStand,
            Storage::Block(pos),
            named("container.brewing"),
        )),
        Kind::Stonecutter => Some(request(
            MenuKind::Stonecutter,
            Storage::Local,
            title("container.stonecutter"),
        )),
        Kind::Anvil => Some(request(
            MenuKind::Anvil,
            Storage::Local,
            title("container.repair"),
        )),
        Kind::Grindstone => Some(request(
            MenuKind::Grindstone,
            Storage::Local,
            title("container.grindstone_title"),
        )),
        Kind::SmithingTable => Some(request(
            MenuKind::Smithing,
            Storage::Local,
            title("container.upgrade"),
        )),
        Kind::Loom => Some(request(
            MenuKind::Loom,
            Storage::Local,
            title("container.loom"),
        )),
        Kind::Beacon => Some(request(
            MenuKind::Beacon,
            Storage::Block(pos),
            named("container.beacon"),
        )),
        Kind::CartographyTable => Some(request(
            MenuKind::CartographyTable,
            Storage::Local,
            title("container.cartography_table"),
        )),
    }
}

/// `Direction.getClockWise` (around Y).
fn clockwise(d: Direction) -> Direction {
    match d {
        Direction::North => Direction::East,
        Direction::East => Direction::South,
        Direction::South => Direction::West,
        Direction::West => Direction::North,
        other => other,
    }
}

/// `ChestBlock.isChestBlockedAt`: a redstone conductor or a sitting cat above.
pub(crate) fn chest_blocked(level: &Level, pos: BlockPos) -> bool {
    chest_blocked_with(level, &level.entities, pos)
}

/// [`chest_blocked`] with the entities `ents` (a mob's tick holds them).
pub(crate) fn chest_blocked_with(
    level: &Level,
    ents: &crate::entities::Entities,
    pos: BlockPos,
) -> bool {
    let above = pos.above();
    Level::is_redstone_conductor(level.block(above)) || cat_sitting_on(ents, pos)
}

/// `isCatSittingOnChest`: a cat in its sitting pose in the block above.
fn cat_sitting_on(ents: &crate::entities::Entities, pos: BlockPos) -> bool {
    let (x, y, z) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    let above = vanillite_base::Aabb::new(x, y + 1.0, z, x + 1.0, y + 2.0, z + 1.0);
    let mut sitting = false;
    ents.store.for_each_in(&above, false, |id| {
        sitting |= ents
            .mob(id)
            .is_some_and(|m| m.kind == crate::mob::Kind::Cat && m.in_sitting_pose);
    });
    sitting
}

/// The other half of a double chest (`ChestBlock.getConnectedBlockPos`), if it is there.
pub fn double_chest_partner(level: &Level, state: BlockState, pos: BlockPos) -> Option<BlockPos> {
    let kind = state.value("type")?;
    if kind == "single" {
        return None;
    }
    let facing = state.direction("facing")?;
    let connected = if kind == "left" {
        clockwise(facing)
    } else {
        clockwise(facing).opposite()
    };
    let other = pos.relative(connected);
    let other_state = level.block(other);
    (other_state.block() == state.block()
        && other_state.value("type") != Some("single")
        && other_state.value("type") != Some(kind)
        && other_state.value("facing") == state.value("facing")
        && matches!(level.block_entity(other), Some(BlockEntity::Container(_))))
    .then_some(other)
}

/// `ShulkerBoxBlock.canOpen` for a closed box: the half block the lid opens into must be
/// free of block collisions.
fn shulker_lid_blocked(level: &Level, state: BlockState, pos: BlockPos) -> bool {
    let facing = state.direction("facing").unwrap_or(Direction::Up);
    let [dx, dy, dz] = facing.normal();
    let (x, y, z) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    // `Shulker.getProgressDeltaAabb(1, facing, 0, 0.5)`: the block box moved 0–0.5 blocks
    // in `facing`, minus the box itself, deflated by 1e-6.
    let e = 1.0e-6;
    let span = |d: i32| match d {
        1 => (1.0, 1.5),
        -1 => (-0.5, 0.0),
        _ => (0.0, 1.0),
    };
    let (x0, x1) = span(dx);
    let (y0, y1) = span(dy);
    let (z0, z1) = span(dz);
    let min = (x0, y0, z0);
    let max = (x1, y1, z1);
    let lid = Aabb::new(
        x + min.0 + e,
        y + min.1 + e,
        z + min.2 + e,
        x + max.0 - e,
        y + max.1 - e,
        z + max.2 - e,
    );
    let other = pos.relative(facing);
    let other_state = level.block(other);
    let (ox, oy, oz) = (f64::from(other.x), f64::from(other.y), f64::from(other.z));
    other_state.collision_shape().iter().any(|b| {
        Aabb::new(
            b[0] + ox,
            b[1] + oy,
            b[2] + oz,
            b[3] + ox,
            b[4] + oy,
            b[5] + oz,
        )
        .intersects(&lid)
    })
}

// --- Stacks ----------------------------------------------------------------------------------

/// The stacks of the container block entity at `pos`.
pub fn items(level: &Level, pos: BlockPos) -> Option<&[ItemStack]> {
    match level.block_entity(pos)? {
        BlockEntity::Container(c) => Some(&c.items),
        BlockEntity::Hopper(h) => Some(&h.items),
        BlockEntity::Furnace(f) => Some(&f.items),
        BlockEntity::BrewingStand(b) => Some(&b.items),
        BlockEntity::DecoratedPot(p) => Some(std::slice::from_ref(&p.item)),
        BlockEntity::ChiseledBookshelf(b) => Some(&b.items),
        BlockEntity::Crafter(c) => Some(&c.items),
        BlockEntity::Jukebox(j) => Some(std::slice::from_ref(&j.item)),
        BlockEntity::Shelf(s) => Some(&s.items),
        _ => None,
    }
}

/// The stacks of the container block entity at `pos`, mutably.
pub fn items_mut(level: &mut Level, pos: BlockPos) -> Option<&mut [ItemStack]> {
    match level.block_entity_mut(pos)? {
        BlockEntity::Container(c) => Some(&mut c.items),
        BlockEntity::Hopper(h) => Some(&mut h.items),
        BlockEntity::Furnace(f) => Some(&mut f.items),
        BlockEntity::BrewingStand(b) => Some(&mut b.items),
        BlockEntity::DecoratedPot(p) => Some(std::slice::from_mut(&mut p.item)),
        BlockEntity::ChiseledBookshelf(b) => Some(&mut b.items),
        BlockEntity::Crafter(c) => Some(&mut c.items),
        BlockEntity::Jukebox(j) => Some(std::slice::from_mut(&mut j.item)),
        BlockEntity::Shelf(s) => Some(&mut s.items),
        _ => None,
    }
}

/// `BlockEntity.setChanged`: comparators next to the container read it again.
pub fn set_changed(level: &mut Level, pos: BlockPos) {
    let state = level.block(pos);
    if !state.is_air() {
        level.update_neighbour_for_output_signal(pos, state.block());
    }
}

/// `BlockEntity.preRemoveSideEffects` of containers: the stacks drop (`Containers
/// .dropContents`); shulker boxes keep theirs (their item carries them); furnaces also pop
/// the experience of their recipes.
pub fn pre_remove_side_effects(
    level: &mut Level,
    pos: BlockPos,
    old: BlockState,
    entity: &BlockEntity,
) {
    let items: &[ItemStack] = match entity {
        BlockEntity::Container(c) => {
            // The removed block: the level may hold its replacement already.
            if kind_of(old.block()) == Some(Kind::ShulkerBox) {
                return;
            }
            &c.items
        }
        BlockEntity::Hopper(h) => &h.items,
        BlockEntity::Furnace(f) => &f.items,
        BlockEntity::BrewingStand(b) => &b.items,
        BlockEntity::Crafter(c) => &c.items,
        // `CampfireBlockEntity.preRemoveSideEffects`; bookshelves and pots are containers.
        BlockEntity::Campfire(c) => &c.items,
        BlockEntity::ChiseledBookshelf(b) => &b.items,
        BlockEntity::DecoratedPot(p) => std::slice::from_ref(&p.item),
        BlockEntity::Shelf(s) => &s.items,
        _ => return,
    };
    // Every slot draws its drop position, empty ones too.
    let items: Vec<ItemStack> = items.to_vec();
    for stack in items {
        drop_item_stack(level, pos, stack);
    }
}

/// `Containers.dropItemStack` at the block: the stack splits into item entities of 10–30
/// items.
pub fn drop_item_stack(level: &mut Level, pos: BlockPos, mut stack: ItemStack) {
    // `EntityTypes.ITEM` is 0.25 wide.
    let size = 0.25;
    let center_range = 1.0 - size;
    let half = size / 2.0;
    let xo = f64::from(pos.x) + level.random().next_double() * center_range + half;
    let yo = f64::from(pos.y) + level.random().next_double() * center_range;
    let zo = f64::from(pos.z) + level.random().next_double() * center_range + half;
    while !stack.is_empty() {
        let n = level.random().next_int_bounded(21) + 10;
        let part = stack.split(n);
        // `new ItemEntity(level, x, y, z, stack)` draws its first velocity from the entity's
        // own random, not the level's; it is replaced right after.
        let triangle = |level: &mut Level, mode: f64| {
            let a = level.random().next_double();
            let b = level.random().next_double();
            mode + 0.114_850_001_711_398_36 * (a - b)
        };
        let vx = triangle(level, 0.0);
        let vy = triangle(level, 0.2);
        let vz = triangle(level, 0.0);
        super::drops::spawn_item(
            level,
            Vec3::new(xo, yo, zo),
            part,
            Vec3::new(vx, vy, vz),
            0,
            None,
        );
    }
}

// --- Container access (hoppers, droppers) ---------------------------------------------------

/// A block container as `HopperBlockEntity.getContainerAt` sees it: one block entity or a
/// double chest (`CompoundContainer`, first half first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerRef {
    /// One block entity.
    Block(BlockPos),
    /// Both halves of a double chest.
    Double(BlockPos, BlockPos),
    /// A container entity (chest and hopper minecarts, chest boats).
    Entity(vanillite_world::entity::EntityId),
    /// `ComposterBlock.getContainer`: the composter at the position with the state its
    /// container was made for (input below level 7, bone meal output at level 8, none at 7).
    Composter(BlockPos, BlockState),
}

impl ContainerRef {
    /// `getBlockContainer`: the container of the block at `pos` (a chest with its other
    /// half, ignoring whether it is blocked).
    pub fn at(level: &Level, pos: BlockPos) -> Option<ContainerRef> {
        let state = level.block(pos);
        items(level, pos)?;
        if matches!(
            kind_of(state.block()),
            Some(Kind::Chest | Kind::TrappedChest)
        ) && let Some(other) = double_chest_partner(level, state, pos)
        {
            return Some(if state.value("type") == Some("right") {
                ContainerRef::Double(pos, other)
            } else {
                ContainerRef::Double(other, pos)
            });
        }
        Some(ContainerRef::Block(pos))
    }

    /// `HopperBlockEntity.getBlockContainer`: a composter's container (`WorldlyContainerHolder`)
    /// or [`ContainerRef::at`].
    pub fn at_block(level: &Level, pos: BlockPos) -> Option<ContainerRef> {
        let state = level.block(pos);
        if state.block() == vanillite_data::block::ids::COMPOSTER {
            return Some(ContainerRef::Composter(pos, state));
        }
        ContainerRef::at(level, pos)
    }

    /// `HopperBlockEntity.getContainerAt(level, x, y, z)`: the block container at the point,
    /// else a random container entity within half a block of it.
    pub fn at_point(level: &mut Level, point: Vec3) -> Option<ContainerRef> {
        let pos = BlockPos::containing(point.x, point.y, point.z);
        if let Some(c) = ContainerRef::at_block(level, pos) {
            return Some(c);
        }
        let area = vanillite_base::Aabb::new(
            point.x - 0.5,
            point.y - 0.5,
            point.z - 0.5,
            point.x + 0.5,
            point.y + 0.5,
            point.z + 0.5,
        );
        let mut found = Vec::new();
        let ents = &level.entities;
        ents.store.for_each_in(&area, false, |id| {
            if ents.container_items(id).is_some() {
                found.push(id);
            }
        });
        if found.is_empty() {
            return None;
        }
        let n = i32::try_from(found.len()).unwrap_or(i32::MAX);
        let i = usize::try_from(level.random().next_int_bounded(n)).unwrap_or(0);
        found.get(i).copied().map(ContainerRef::Entity)
    }

    fn locate(self, level: &Level, slot: usize) -> Option<(BlockPos, usize)> {
        match self {
            ContainerRef::Entity(_) | ContainerRef::Composter(..) => None,
            ContainerRef::Block(p) => Some((p, slot)),
            ContainerRef::Double(a, b) => {
                let first = items(level, a).map_or(0, <[ItemStack]>::len);
                if slot < first {
                    Some((a, slot))
                } else {
                    Some((b, slot - first))
                }
            }
        }
    }

    /// `getContainerSize`.
    pub fn size(self, level: &Level) -> usize {
        match self {
            ContainerRef::Entity(id) => level
                .entities
                .container_items(id)
                .map_or(0, <[ItemStack]>::len),
            ContainerRef::Block(p) => items(level, p).map_or(0, <[ItemStack]>::len),
            ContainerRef::Double(a, b) => {
                items(level, a).map_or(0, <[ItemStack]>::len)
                    + items(level, b).map_or(0, <[ItemStack]>::len)
            }
            ContainerRef::Composter(_, state) => usize::from(composter_level(state) != 7),
        }
    }

    /// `getItem(slot)`.
    pub fn get(self, level: &Level, slot: usize) -> ItemStack {
        if let ContainerRef::Entity(id) = self {
            return level
                .entities
                .container_items(id)
                .and_then(|items| items.get(slot).cloned())
                .unwrap_or_default();
        }
        if let ContainerRef::Composter(pos, state) = self {
            // The output holds its bone meal until it empties the composter.
            return if slot == 0 && composter_level(state) == 8 && level.block(pos) == state {
                ItemStack::new(vanillite_data::item::ids::BONE_MEAL.0, 1)
            } else {
                ItemStack::EMPTY
            };
        }
        self.locate(level, slot)
            .and_then(|(p, i)| items(level, p)?.get(i).cloned())
            .unwrap_or_default()
    }

    /// The stack of a slot for in-place changes.
    pub fn get_mut(self, level: &mut Level, slot: usize) -> Option<&mut ItemStack> {
        if let ContainerRef::Entity(id) = self {
            return level.entities.container_items_mut(id)?.get_mut(slot);
        }
        let (p, i) = self.locate(level, slot)?;
        items_mut(level, p)?.get_mut(i)
    }

    /// `setItem(slot, stack)` (a furnace's input restarts its cooking).
    pub fn set(self, level: &mut Level, slot: usize, stack: ItemStack) {
        if let ContainerRef::Entity(id) = self {
            if let Some(target) = level
                .entities
                .container_items_mut(id)
                .and_then(|items| items.get_mut(slot))
            {
                *target = stack;
            }
            return;
        }
        if let ContainerRef::Composter(pos, state) = self {
            // `InputContainer.setItem` → `setChanged`: the item becomes a layer at once.
            let mut stack = stack;
            if composter_level(state) < 7
                && let Some(provider) = super::composter::compostable(&stack)
            {
                let new_state = super::composter::add_layer(level, None, state, pos, &provider);
                level.level_event(None, 1500, pos, i32::from(new_state != state));
                stack.shrink(1);
            }
            return;
        }
        let Some((p, i)) = self.locate(level, slot) else {
            return;
        };
        if let Some(BlockEntity::Furnace(f)) = level.block_entity_mut(p) {
            f.set_item(i, stack);
        } else if let Some(BlockEntity::ChiseledBookshelf(_)) = level.block_entity(p) {
            super::bookshelf::set_item(level, p, i, stack);
        } else if let Some(BlockEntity::Crafter(c)) = level.block_entity_mut(p) {
            c.set_item(i, stack);
        } else if let Some(BlockEntity::Jukebox(_)) = level.block_entity(p) {
            // `JukeboxBlockEntity.setTheItem`: the disc plays.
            super::jukebox::set_the_item(level, p, stack);
        } else if let Some(target) = items_mut(level, p).and_then(|items| items.get_mut(i)) {
            *target = stack;
        }
    }

    /// `removeItem(slot, 1)`: one item of a slot (a bookshelf updates its state).
    pub fn remove_one(self, level: &mut Level, slot: usize) {
        if let ContainerRef::Composter(..) = self {
            // `SimpleContainer.removeItem` → `OutputContainer.setChanged`.
            self.set_changed(level);
            return;
        }
        let Some((p, i)) = self.locate(level, slot) else {
            return;
        };
        if let Some(BlockEntity::ChiseledBookshelf(_)) = level.block_entity(p) {
            super::bookshelf::remove_item(level, p, i);
        } else if let Some(BlockEntity::Jukebox(_)) = level.block_entity(p) {
            // `JukeboxBlockEntity.splitTheItem`: the whole disc leaves, the song stops.
            super::jukebox::set_the_item(level, p, ItemStack::EMPTY);
        } else if let Some(s) = items_mut(level, p).and_then(|items| items.get_mut(i)) {
            s.shrink(1);
        }
    }

    /// `JukeboxBlockEntity.canTakeItem(into, …)`: a jukebox only gives its disc to a
    /// container with an empty slot.
    pub fn takes_only_into_empty(self, level: &Level) -> bool {
        matches!(self, ContainerRef::Block(p) if matches!(level.block_entity(p), Some(BlockEntity::Jukebox(_))))
    }

    /// `setChanged`.
    pub fn set_changed(self, level: &mut Level) {
        match self {
            // `ShelfBlockEntity.setChanged`: a block update and `block_activate`.
            ContainerRef::Block(p)
                if matches!(level.block_entity(p), Some(BlockEntity::Shelf(_))) =>
            {
                super::shelf::set_changed(
                    level,
                    p,
                    Some(crate::level::game_event::events().block_activate),
                );
            }
            ContainerRef::Block(p) => set_changed(level, p),
            ContainerRef::Double(a, b) => {
                set_changed(level, a);
                set_changed(level, b);
            }
            ContainerRef::Entity(_) => {}
            // `OutputContainer.setChanged` empties the composter (again); the input turned its
            // item into a layer when it was set.
            ContainerRef::Composter(pos, state) => {
                if composter_level(state) == 8 {
                    super::composter::empty(level, None, state, pos);
                }
            }
        }
    }

    /// `getItem(slot)` of a `RandomizableContainer` first rolls the loot table of the block
    /// entity holding the slot.
    pub fn unpack(self, level: &mut Level, slot: usize) {
        if let ContainerRef::Entity(id) = self {
            super::chest_loot::unpack_entity(level, id, None);
        } else if let Some((p, _)) = self.locate(level, slot) {
            super::chest_loot::unpack(level, p, None);
        }
    }

    /// `isEmpty` with the loot tables it rolls (a double chest's second half only if the
    /// first is empty).
    pub fn is_empty_unpacking(self, level: &mut Level) -> bool {
        match self {
            ContainerRef::Block(p) => {
                super::chest_loot::unpack(level, p, None);
                ContainerRef::Block(p).is_empty(level)
            }
            ContainerRef::Double(a, b) => {
                super::chest_loot::unpack(level, a, None);
                if !ContainerRef::Block(a).is_empty(level) {
                    return false;
                }
                super::chest_loot::unpack(level, b, None);
                ContainerRef::Block(b).is_empty(level)
            }
            // `isChestVehicleEmpty` reads the stacks as they are.
            ContainerRef::Entity(_) | ContainerRef::Composter(..) => self.is_empty(level),
        }
    }

    /// `isEmpty`.
    pub fn is_empty(self, level: &Level) -> bool {
        (0..self.size(level)).all(|i| self.get(level, i).is_empty())
    }

    /// The hopper at the position, if this is one.
    pub fn hopper(self) -> Option<BlockPos> {
        match self {
            ContainerRef::Block(p) => Some(p),
            ContainerRef::Double(..) | ContainerRef::Entity(_) | ContainerRef::Composter(..) => {
                None
            }
        }
    }

    /// `getSlotsForFace` (`WorldlyContainer`; all slots otherwise).
    pub fn slots_for_face(self, level: &Level, face: Direction) -> Vec<usize> {
        if let ContainerRef::Composter(_, state) = self {
            let open = match composter_level(state) {
                8 => Direction::Down,
                7 => return Vec::new(),
                _ => Direction::Up,
            };
            return if face == open { vec![0] } else { Vec::new() };
        }
        if let ContainerRef::Block(p) = self {
            match level.block_entity(p) {
                Some(BlockEntity::Furnace(_)) => {
                    return match face {
                        Direction::Up => vec![0],
                        Direction::Down => vec![2, 1],
                        _ => vec![1],
                    };
                }
                Some(BlockEntity::BrewingStand(_)) => {
                    return match face {
                        Direction::Up => vec![3],
                        Direction::Down => vec![0, 1, 2, 3],
                        _ => vec![0, 1, 2, 4],
                    };
                }
                _ => {}
            }
        }
        (0..self.size(level)).collect()
    }

    /// `canPlaceItem` and `canPlaceItemThroughFace`.
    pub fn can_place(self, level: &Level, slot: usize, stack: &ItemStack) -> bool {
        if let ContainerRef::Entity(_) = self {
            return slot < self.size(level);
        }
        if let ContainerRef::Composter(_, state) = self {
            return composter_level(state) < 7 && super::composter::compostable(stack).is_some();
        }
        let Some((p, i)) = self.locate(level, slot) else {
            return false;
        };
        match level.block_entity(p) {
            Some(BlockEntity::Furnace(f)) => crate::player::play::game_data()
                .is_some_and(|d| f.can_place_item(i, stack, &d.recipes)),
            Some(BlockEntity::BrewingStand(b)) => b.can_place_item(i, stack),
            Some(BlockEntity::ChiseledBookshelf(b)) => {
                super::bookshelf::accepts(stack) && b.items.get(i).is_some_and(ItemStack::is_empty)
            }
            Some(BlockEntity::Crafter(c)) => c.can_place_item(i, stack),
            // `JukeboxBlockEntity.canPlaceItem`: a playable disc into the empty slot.
            Some(BlockEntity::Jukebox(j)) => {
                super::jukebox::is_playable(stack) && j.item.is_empty()
            }
            Some(BlockEntity::Container(_))
                if kind_of(level.block(p).block()) == Some(Kind::ShulkerBox) =>
            {
                !crate::menu::container::is_shulker_box(stack)
            }
            _ => true,
        }
    }

    /// `canTakeItem` and `canTakeItemThroughFace`.
    pub fn can_take(self, level: &Level, slot: usize, stack: &ItemStack, face: Direction) -> bool {
        if let ContainerRef::Entity(_) = self {
            return slot < self.size(level);
        }
        if let ContainerRef::Composter(_, state) = self {
            return composter_level(state) == 8
                && face == Direction::Down
                && crate::item::item_id(stack) == vanillite_data::item::ids::BONE_MEAL;
        }
        let Some((p, i)) = self.locate(level, slot) else {
            return false;
        };
        match level.block_entity(p) {
            Some(BlockEntity::Furnace(_)) => {
                let id = crate::item::item_id(stack);
                face != Direction::Down
                    || i != 1
                    || id == vanillite_data::item::ids::WATER_BUCKET
                    || id == vanillite_data::item::ids::BUCKET
            }
            Some(BlockEntity::BrewingStand(_)) => {
                i != 3 || crate::item::item_id(stack) == vanillite_data::item::ids::GLASS_BOTTLE
            }
            _ => true,
        }
    }
}

/// The `level` of a composter state.
fn composter_level(state: BlockState) -> i32 {
    state.int("level").unwrap_or(0)
}

// --- Viewers ---------------------------------------------------------------------------------

/// `startOpen(player)`: one more viewer (spectators are not counted); `source` is the opening
/// entity of the `container_open` game event.
pub fn start_open(level: &mut Level, pos: BlockPos, spectator: bool, source: Option<EntityId>) {
    if spectator {
        return;
    }
    let state = level.block(pos);
    match level.block_entity_mut(pos) {
        Some(BlockEntity::Container(c)) => {
            let previous = c.openers;
            c.openers += 1;
            let current = c.openers;
            openers_changed(level, state, pos, previous, current, source);
            if previous == 0 {
                schedule_recheck(level, state, pos);
            }
        }
        Some(BlockEntity::EnderChest { openers }) => {
            let previous = *openers;
            *openers += 1;
            let current = *openers;
            openers_changed(level, state, pos, previous, current, source);
            if previous == 0 {
                schedule_recheck(level, state, pos);
            }
        }
        _ => {}
    }
}

/// `ContainerOpenersCounter.scheduleRecheck`: chests, barrels and ender chests count their
/// viewers again in 5 ticks.
fn schedule_recheck(level: &mut Level, state: BlockState, pos: BlockPos) {
    if matches!(
        kind_of(state.block()),
        Some(Kind::Chest | Kind::TrappedChest | Kind::Barrel | Kind::EnderChest)
    ) {
        level.schedule_block_tick(pos, state.block(), 5);
    }
}

/// `tick` of chests, barrels and ender chests: `recheckOpen` (`recheckOpeners`): the viewers
/// are counted again from the players and copper golems that have the container open near it
/// (a viewer that left without closing no longer counts); while any remain, again in 5 ticks.
fn recheck_open(level: &mut Level, state: BlockState, pos: BlockPos) {
    let previous = match level.block_entity(pos) {
        Some(BlockEntity::Container(c)) => c.openers,
        Some(BlockEntity::EnderChest { openers }) => *openers,
        _ => return,
    };
    // `getEntitiesWithContainerOpen`: the box of the block inflated by the interaction range
    // (a player's 4.5) plus 4.
    let range = 4.5 + 4.0;
    let (min, max) = (
        Vec3::new(
            f64::from(pos.x) - range,
            f64::from(pos.y) - range,
            f64::from(pos.z) - range,
        ),
        Vec3::new(
            f64::from(pos.x) + 1.0 + range,
            f64::from(pos.y) + 1.0 + range,
            f64::from(pos.z) + 1.0 + range,
        ),
    );
    let near = |level: &Level, id: EntityId| {
        level.entities.store.get(id).is_some_and(|e| {
            let b = e.aabb();
            b.max_x > min.x
                && b.min_x < max.x
                && b.max_y > min.y
                && b.min_y < max.y
                && b.max_z > min.z
                && b.min_z < max.z
        })
    };
    let mut count = 0;
    for (id, p) in level.entities.players.iter() {
        if p.game_mode == vanillite_config::GameMode::Spectator || !near(level, id) {
            continue;
        }
        let viewing = p.inventory.container.as_ref().is_some_and(|m| {
            m.storage_positions()
                .into_iter()
                .flatten()
                .any(|v| v == pos)
        });
        if viewing {
            count += 1;
        }
    }
    for (id, m) in level.entities.mobs.iter() {
        if m.kind != crate::mob::Kind::CopperGolem || !near(level, id) {
            continue;
        }
        let golem = crate::mob::fam::mount::copper_golem::golem(m);
        let open = golem.opened_chest.is_some_and(|(opened, c)| {
            opened == pos || matches!(c, ContainerRef::Double(a, b) if a == pos || b == pos)
        });
        if open {
            count += 1;
        }
    }
    match level.block_entity_mut(pos) {
        Some(BlockEntity::Container(c)) => c.openers = count,
        Some(BlockEntity::EnderChest { openers }) => *openers = count,
        _ => return,
    }
    openers_changed(level, state, pos, previous, count, None);
    if count > 0 {
        schedule_recheck(level, state, pos);
    }
}

/// `stopOpen(player)`: one viewer less; `source` is the closing entity of the
/// `container_close` game event.
pub fn stop_open(level: &mut Level, pos: BlockPos, spectator: bool, source: Option<EntityId>) {
    if spectator {
        return;
    }
    let state = level.block(pos);
    match level.block_entity_mut(pos) {
        Some(BlockEntity::Container(c)) if c.openers > 0 => {
            let previous = c.openers;
            c.openers -= 1;
            let current = c.openers;
            openers_changed(level, state, pos, previous, current, source);
        }
        Some(BlockEntity::EnderChest { openers }) if *openers > 0 => {
            let previous = *openers;
            *openers -= 1;
            let current = *openers;
            openers_changed(level, state, pos, previous, current, source);
        }
        _ => {}
    }
}

/// The delay of `ContainerOpenersCounter.scheduleRecheck`.
const OPENERS_RECHECK_DELAY: i32 = 5;

/// The scheduled tick of chests, ender chests and barrels (`recheckOpen`): an open container
/// checks its viewers again every five ticks. The viewer count here is kept exact by
/// [`start_open`] and [`stop_open`], so only the next check is scheduled.
fn recheck_openers(level: &mut Level, state: BlockState, pos: BlockPos) {
    let open = match level.block_entity(pos) {
        Some(BlockEntity::Container(c)) => c.openers > 0,
        Some(BlockEntity::EnderChest { openers }) => *openers > 0,
        _ => false,
    };
    if open {
        level.schedule_block_tick(pos, state.block(), OPENERS_RECHECK_DELAY);
    }
}

/// `onOpen` / `onClose` / `openerCountChanged` of the kinds that count viewers.
fn openers_changed(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    previous: i32,
    current: i32,
    source: Option<EntityId>,
) {
    let Some(kind) = kind_of(state.block()) else {
        return;
    };
    let opened = previous == 0 && current > 0;
    let closed = previous > 0 && current == 0;
    let events = crate::level::game_event::events();
    if opened {
        level.game_event_at(source, events.container_open, pos);
        // `ContainerOpenersCounter.scheduleRecheck`.
        if matches!(
            kind,
            Kind::Chest | Kind::TrappedChest | Kind::EnderChest | Kind::Barrel
        ) {
            level.schedule_block_tick(pos, state.block(), OPENERS_RECHECK_DELAY);
        }
    } else if closed {
        level.game_event_at(source, events.container_close, pos);
    }
    match kind {
        Kind::Chest | Kind::TrappedChest => {
            if opened || closed {
                chest_sound(level, state, pos, opened);
            }
            level.block_event(pos, state.block(), 1, current);
            if kind == Kind::TrappedChest && previous != current {
                level.update_neighbors_at(pos, state.block());
                level.update_neighbors_at(pos.below(), state.block());
            }
        }
        Kind::EnderChest => {
            if opened || closed {
                let sound = if opened {
                    "block.ender_chest.open"
                } else {
                    "block.ender_chest.close"
                };
                let pitch = level.random().next_float() * 0.1 + 0.9;
                level.play_block_sound(None, pos, sound_id(sound), BLOCKS, 0.5, pitch);
            }
            level.block_event(pos, state.block(), 1, current);
        }
        Kind::Barrel => {
            if opened || closed {
                let facing = state.direction("facing").unwrap_or(Direction::North);
                let [dx, dy, dz] = facing.normal();
                let sound = if opened {
                    "block.barrel.open"
                } else {
                    "block.barrel.close"
                };
                let pitch = level.random().next_float() * 0.1 + 0.9;
                level.play_sound(
                    None,
                    f64::from(pos.x) + 0.5 + f64::from(dx) / 2.0,
                    f64::from(pos.y) + 0.5 + f64::from(dy) / 2.0,
                    f64::from(pos.z) + 0.5 + f64::from(dz) / 2.0,
                    sound_id(sound),
                    BLOCKS,
                    0.5,
                    pitch,
                );
                level.set_block_and_update(pos, state.set_flag("open", opened));
            }
        }
        Kind::ShulkerBox => {
            level.block_event(pos, state.block(), 1, current);
            if opened || closed {
                let sound = if opened {
                    "block.shulker_box.open"
                } else {
                    "block.shulker_box.close"
                };
                let pitch = level.random().next_float() * 0.1 + 0.9;
                level.play_block_sound(None, pos, sound_id(sound), BLOCKS, 0.5, pitch);
            }
            if closed {
                level.update_neighbors_at(pos, state.block());
            }
        }
        _ => {}
    }
}

/// `ChestBlockEntity.playSound`: once per double chest, between the halves.
fn chest_sound(level: &mut Level, state: BlockState, pos: BlockPos, open: bool) {
    let kind = state.value("type").unwrap_or("single");
    if kind == "left" {
        return;
    }
    let mut x = f64::from(pos.x) + 0.5;
    let y = f64::from(pos.y) + 0.5;
    let mut z = f64::from(pos.z) + 0.5;
    if kind == "right"
        && let Some(facing) = state.direction("facing")
    {
        let [dx, _, dz] = clockwise(facing).opposite().normal();
        x += f64::from(dx) * 0.5;
        z += f64::from(dz) * 0.5;
    }
    let name = state.block().name();
    let base = if name.contains("copper_chest") {
        if name.contains("weathered") {
            "block.copper_chest_weathered"
        } else if name.contains("oxidized") {
            "block.copper_chest_oxidized"
        } else {
            "block.copper_chest"
        }
    } else {
        "block.chest"
    };
    let sound = format!("{base}.{}", if open { "open" } else { "close" });
    let pitch = level.random().next_float() * 0.1 + 0.9;
    level.play_sound(None, x, y, z, sound_id(&sound), BLOCKS, 0.5, pitch);
}

// --- Components ----------------------------------------------------------------------------

/// `BlockEntity.collectComponents` of containers: the component `id` the block entity puts on
/// its item (`custom_name`, `container`), as a wire value.
pub fn component(entity: &BlockEntity, id: u16) -> Option<Box<[u8]>> {
    use vanillite_protocol::codec::Encode;
    use vanillite_protocol::components::ids;
    let (name, items): (&Option<Component>, &[ItemStack]) = match entity {
        BlockEntity::Container(c) => (&c.custom_name, &c.items),
        BlockEntity::Hopper(h) => (&h.custom_name, &h.items),
        BlockEntity::Furnace(f) => (&f.custom_name, &f.items),
        BlockEntity::BrewingStand(b) => (&b.custom_name, &b.items),
        // `BeaconBlockEntity.collectImplicitComponents`: its custom name.
        BlockEntity::Beacon(b) => (&b.custom_name, &[]),
        _ => return None,
    };
    if id == ids::CONTAINER_LOOT {
        // `RandomizableContainerBlockEntity.collectImplicitComponents`.
        let (table, seed) = match entity {
            BlockEntity::Container(c) => (c.loot_table.as_deref(), c.loot_seed),
            BlockEntity::Hopper(h) => (h.loot_table.as_deref(), h.loot_seed),
            _ => (None, 0),
        };
        return Some(super::chest_loot::component(table?, seed));
    }
    if id == ids::CUSTOM_NAME {
        let mut out = Vec::new();
        name.as_ref()?.encode(&mut out);
        return Some(out.into_boxed_slice());
    }
    if id == ids::CONTAINER {
        // `ItemContainerContents.fromItems`: up to the last non-empty slot; nothing if empty
        // (the item's default).
        return vanillite_protocol::item::encode_container_contents(items);
    }
    None
}

/// `BlockEntity.applyComponentsFromItemStack` of containers (a placed block item): its
/// `custom_name` and `container`.
pub fn apply_item_components(level: &mut Level, pos: BlockPos, stack: &ItemStack) {
    use vanillite_protocol::components::ids;
    let name = stack.component(ids::CUSTOM_NAME).and_then(|bytes| {
        let mut r = vanillite_protocol::codec::Reader::new(bytes);
        <Component as vanillite_protocol::codec::DecodeLimited>::decode_limited(&mut r, 2_097_152)
            .ok()
    });
    let contents: Option<Vec<ItemStack>> = stack
        .component(ids::CONTAINER)
        .and_then(|bytes| vanillite_protocol::item::decode_container_contents(bytes).ok());
    let loot = stack
        .component(ids::CONTAINER_LOOT)
        .and_then(super::chest_loot::read_component);
    if let Some((table, seed)) = loot {
        // `RandomizableContainerBlockEntity.applyImplicitComponents`.
        match level.block_entity_mut(pos) {
            Some(BlockEntity::Container(c)) => (c.loot_table, c.loot_seed) = (Some(table), seed),
            Some(BlockEntity::Hopper(h)) => (h.loot_table, h.loot_seed) = (Some(table), seed),
            _ => {}
        }
    }
    let (target_name, target_items): (&mut Option<Component>, &mut [ItemStack]) =
        match level.block_entity_mut(pos) {
            Some(BlockEntity::Container(c)) => (&mut c.custom_name, &mut c.items),
            Some(BlockEntity::Hopper(h)) => (&mut h.custom_name, &mut h.items),
            Some(BlockEntity::Furnace(f)) => (&mut f.custom_name, &mut f.items),
            Some(BlockEntity::BrewingStand(b)) => (&mut b.custom_name, &mut b.items),
            Some(BlockEntity::Beacon(b)) => (&mut b.custom_name, &mut []),
            _ => return,
        };
    if name.is_some() {
        *target_name = name;
    }
    if let Some(contents) = contents {
        for (slot, stack) in target_items.iter_mut().zip(contents) {
            *slot = stack;
        }
    }
}

/// `ShulkerBoxBlock.playerWillDestroy` for a creative player: a box with contents drops as
/// an item with them.
pub fn creative_shulker_drop(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    entity: &BlockEntity,
) {
    use vanillite_protocol::components::ids;
    let BlockEntity::Container(c) = entity else {
        return;
    };
    if kind_of(state.block()) != Some(Kind::ShulkerBox) || c.items.iter().all(ItemStack::is_empty) {
        return;
    }
    let item = state.block().info().item;
    if item == u16::MAX {
        return;
    }
    let mut stack = ItemStack::new(item, 1);
    // `collectComponents`: the kept components, then the implicit ones.
    if let Some(kept) = level.block_entities.kept_components(pos) {
        let patch = crate::item_nbt::patch_from_nbt(kept);
        for (id, value) in patch.added {
            stack.set_component(id, value);
        }
    }
    for id in [ids::CUSTOM_NAME, ids::LOCK, ids::CONTAINER] {
        if let Some(value) = crate::level::block_entity::collect_component(entity, id) {
            stack.set_component(id, value);
        }
    }
    super::drops::spawn_item_moving(level, super::furnace::center(pos), stack, None, 10, None);
}

// --- Persistence -----------------------------------------------------------------------------

/// `ContainerHelper.saveAllItems`: the non-empty stacks with their slot.
pub fn save_items(items: &[ItemStack], out: &mut Compound) {
    let mut list = List::new();
    for (i, stack) in items.iter().enumerate() {
        if stack.is_empty() {
            continue;
        }
        let mut c = crate::inventory::stack_to_nbt(stack);
        c.insert("Slot", i8::try_from(i).unwrap_or(0));
        list.0.push(Tag::Compound(c));
    }
    if !list.0.is_empty() {
        out.insert("Items", list);
    }
}

/// `ContainerHelper.saveAllItems(output, items, true)`: `Items` even when empty.
pub fn save_items_always(items: &[ItemStack], out: &mut Compound) {
    save_items(items, out);
    if out.get("Items").is_none() {
        out.insert("Items", List::new());
    }
}

/// `ContainerHelper.loadAllItems`.
pub fn load_items(nbt: &Compound, items: &mut [ItemStack]) {
    let Some(Tag::List(list)) = nbt.get("Items") else {
        return;
    };
    for tag in &list.0 {
        let Some(c) = tag.as_compound() else {
            continue;
        };
        let slot = c.get("Slot").and_then(Tag::as_i64).unwrap_or(-1);
        // `Slot` is an unsigned byte.
        let slot = slot & 255;
        if let Some(target) = usize::try_from(slot).ok().and_then(|s| items.get_mut(s)) {
            *target = crate::inventory::stack_from_nbt(c);
        }
    }
}

/// Reads `CustomName`.
pub fn load_name(nbt: &Compound) -> Option<Component> {
    nbt.get("CustomName").map(Component::from_nbt)
}

/// Writes `CustomName`.
pub fn save_name(name: &Option<Component>, out: &mut Compound) {
    if let Some(name) = name {
        out.insert("CustomName", name.to_nbt());
    }
}

impl Container {
    /// Loads the stored fields: a container whose loot table was not rolled yet has no
    /// stacks (`tryLoadLootTable`, else `loadAllItems`).
    pub fn load(&mut self, nbt: &Compound) {
        self.custom_name = load_name(nbt);
        self.lock = super::lock::load(nbt);
        (self.loot_table, self.loot_seed) = super::chest_loot::load(nbt);
        if self.loot_table.is_none() {
            load_items(nbt, &mut self.items);
        }
    }

    /// Saves the stored fields (`trySaveLootTable`, else `saveAllItems`): `Items` also when
    /// empty, except for shulker boxes (`also_when_empty` false).
    pub fn save(&self, out: &mut Compound, also_when_empty: bool) {
        super::lock::save(&self.lock, out);
        save_name(&self.custom_name, out);
        if !super::chest_loot::save(self.loot_table.as_deref(), self.loot_seed, out) {
            if also_when_empty {
                save_items_always(&self.items, out);
            } else {
                save_items(&self.items, out);
            }
        }
    }
}
