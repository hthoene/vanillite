//! Menu tests: crafting in the inventory and at a crafting table, furnace timing, container
//! clicks, and dupe tests (random click sequences never create or destroy items).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use vanillite_base::random::{RandomSource, XoroshiroRandom};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::ids as blocks;
use vanillite_data::item::ids as items;
use vanillite_data::registry::ITEM;
use vanillite_protocol::packets::play::serverbound::ContainerInput;

use super::container::{MenuKind, MenuRequest, Storage};
use super::{ClickContext, MenuSlots};
use crate::inventory::{Drop, PlayerInventory};
use crate::item::ItemStack;
use crate::level::{BlockEntity, Level, update};
use crate::player::play::GameData;
use crate::recipe::{CookingKind, CraftingInput, Recipes};

/// Installs gameplay data with the Vanilla recipes and item tags (once per test binary).
pub(crate) fn install_data() -> std::sync::Arc<GameData> {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let cache = std::env::var_os("VANILLITE_CACHE").map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/vanilla-cache"),
            PathBuf::from,
        );
        let jar = std::env::var_os("VANILLITE_SERVER_JAR").map(PathBuf::from);
        let pack = vanillite_datapack::vanilla::ensure_vanilla_pack(&cache, jar.as_deref())
            .expect("Vanilla data pack (set VANILLITE_SERVER_JAR for an offline copy)");
        let packs = vanillite_datapack::load(&[pack.as_path()]).expect("load Vanilla data pack");
        let tag = |t: &str| -> Vec<u16> {
            packs
                .tag_ids("minecraft:item", t)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|i| u16::try_from(i).ok())
                .collect()
        };
        let recipes = Recipes::load(packs.roots(), &tag);
        let registry = |name: &str| -> Vec<(String, serde_json::Value)> {
            packs
                .registry(name)
                .map(|r| {
                    r.entries()
                        .iter()
                        .map(|e| {
                            (
                                e.id.clone(),
                                serde_json::from_str(e.json.get()).unwrap_or_default(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let entries = registry("minecraft:enchantment");
        let mut enchanting = crate::enchant::Enchanting::load(&entries, &tag, &|t| {
            packs
                .tag_ids("minecraft:enchantment", t)
                .unwrap_or_default()
        });
        let names = |r: &str| registry(r).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        enchanting.set_trims(
            &names("minecraft:trim_material"),
            &names("minecraft:trim_pattern"),
        );
        let enchantments = entries
            .iter()
            .zip(0u32..)
            .map(|((n, _), i)| (n.clone(), i))
            .collect();
        let banner_patterns = crate::menu::BannerPatterns {
            names: names("minecraft:banner_pattern"),
            tags: packs
                .tags("minecraft:banner_pattern")
                .map(|tags| {
                    tags.keys()
                        .map(|t| {
                            let ids = packs
                                .tag_ids("minecraft:banner_pattern", t)
                                .unwrap_or_default();
                            (t.clone(), ids)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        let item_tags = packs
            .tags("minecraft:item")
            .map(|tags| tags.keys().map(|t| (t.clone(), tag(t))).collect())
            .unwrap_or_default();
        let registry_tags = packs
            .tagged_registries()
            .map(|registry| {
                let tags = packs
                    .tags(registry)
                    .map(|tags| {
                        tags.keys()
                            .map(|t| (t.clone(), packs.tag_ids(registry, t).unwrap_or_default()))
                            .collect()
                    })
                    .unwrap_or_default();
                (registry.to_owned(), tags)
            })
            .collect();
        let trades = crate::trading::data::TradeData::load(
            &registry("minecraft:villager_trade"),
            &registry("minecraft:trade_set"),
            &|t| {
                packs
                    .tag_ids("minecraft:villager_trade", t)
                    .unwrap_or_default()
            },
            &|id| {
                packs
                    .registry("minecraft:villager_trade")
                    .and_then(|r| r.id_of(id))
                    .and_then(|i| u32::try_from(i).ok())
            },
        );
        crate::player::play::install_game_data(Arc::new(GameData {
            trades,
            loot: crate::loot::LootTables::empty(),
            recipes,
            enchanting,
            enchantments,
            banner_patterns,
            item_tags,
            registry_tags,
            ..GameData::default()
        }));
    });
    crate::player::play::game_data().expect("game data installed")
}

pub(super) fn item(name: &str) -> u16 {
    ITEM.id(&format!("minecraft:{name}")).expect("item")
}

pub(super) fn stack(name: &str, count: i32) -> ItemStack {
    ItemStack::new(item(name), count)
}

pub(super) const SURVIVAL: ClickContext = ClickContext { infinite: false };

// --- Recipes ---------------------------------------------------------------------------------

#[test]
fn recipes_load_every_type() {
    let data = install_data();
    let r = &data.recipes;
    assert!(r.broken.is_empty(), "{:?}", r.broken);
    assert_eq!(r.crafting.len(), 1266);
    assert!(r.cooking.len() > 100);
    assert!(r.stonecutting.len() > 300);
    assert!(r.smithing.len() > 25);
    assert_eq!(r.brewing.len(), 279);
}

#[test]
fn shaped_recipes_match_anywhere_and_mirrored() {
    let r = &install_data().recipes;
    // Four planks make a crafting table in any corner of the 3×3 grid.
    let p = stack("oak_planks", 1);
    let e = ItemStack::EMPTY;
    let grid = [
        e.clone(),
        e.clone(),
        e.clone(),
        e.clone(),
        p.clone(),
        p.clone(),
        e.clone(),
        p.clone(),
        p.clone(),
    ];
    let input = CraftingInput::of(3, 3, &grid);
    assert_eq!(
        (input.left, input.top, input.width, input.height),
        (1, 1, 2, 2)
    );
    let found = r.crafting_for(&input).expect("crafting table");
    assert_eq!(r.assemble(found, &input).item, items::CRAFTING_TABLE.0);
    // An axe pattern works mirrored.
    let s = stack("stick", 1);
    let c = stack("cobblestone", 1);
    let axe = [
        c.clone(),
        c.clone(),
        e.clone(),
        c.clone(),
        s.clone(),
        e.clone(),
        e.clone(),
        s.clone(),
        e.clone(),
    ];
    let mirrored = [
        e.clone(),
        c.clone(),
        c.clone(),
        e.clone(),
        s.clone(),
        c.clone(),
        e.clone(),
        s.clone(),
        e.clone(),
    ];
    for grid in [axe, mirrored] {
        let input = CraftingInput::of(3, 3, &grid);
        let found = r.crafting_for(&input).expect("stone axe");
        assert_eq!(r.assemble(found, &input).item, item("stone_axe"));
    }
    // Sticks need the planks above each other.
    let sideways = [p.clone(), p.clone(), e.clone(), e.clone()];
    let input = CraftingInput::of(2, 2, &sideways);
    let result = r.crafting_for(&input).map(|i| r.assemble(i, &input));
    assert!(result.is_none_or(|s| s.item != item("stick")));
}

#[test]
fn shapeless_and_transmute_recipes() {
    let r = &install_data().recipes;
    let log = [
        stack("oak_log", 1),
        ItemStack::EMPTY,
        ItemStack::EMPTY,
        ItemStack::EMPTY,
    ];
    let input = CraftingInput::of(2, 2, &log);
    let planks = r.assemble(r.crafting_for(&input).expect("planks"), &input);
    assert_eq!((planks.item, planks.count), (item("oak_planks"), 4));
    // Dyeing a shulker box keeps its components.
    let mut boxed = stack("shulker_box", 1);
    boxed.patch_mut().set(
        vanillite_protocol::components::ids::CONTAINER,
        vec![0].into_boxed_slice(),
    );
    let grid = [
        boxed.clone(),
        stack("red_dye", 1),
        ItemStack::EMPTY,
        ItemStack::EMPTY,
    ];
    let input = CraftingInput::of(2, 2, &grid);
    let dyed = r.assemble(r.crafting_for(&input).expect("dyed box"), &input);
    assert_eq!(dyed.item, item("red_shulker_box"));
    assert_eq!(dyed.patch(), boxed.patch());
}

#[test]
fn fuel_and_cooking_times() {
    let r = &install_data().recipes;
    let coal = stack("coal", 1);
    assert_eq!(r.fuel(&coal, false).map(|f| f.burn_time), Some(1600));
    assert_eq!(r.fuel(&coal, true).map(|f| f.burn_time), Some(800));
    assert_eq!(
        r.fuel(&stack("lava_bucket", 1), false).map(|f| f.burn_time),
        Some(20000)
    );
    assert!(r.fuel(&stack("diamond", 1), false).is_none());
    let iron = stack("raw_iron", 1);
    assert_eq!(
        r.cooking_for(CookingKind::Smelting, &iron)
            .map(|c| c.cooking_time),
        Some(200)
    );
    // 26.3 blasting recipes take 200 ticks; blast furnaces run at fuel speed 2.
    assert_eq!(
        r.cooking_for(CookingKind::Blasting, &iron)
            .map(|c| c.cooking_time),
        Some(200)
    );
    assert_eq!(r.fuel(&coal, true).map(|f| f.speed), Some(2.0));
    assert!(r.cooking_for(CookingKind::Smoking, &iron).is_none());
}

// --- Inventory crafting ----------------------------------------------------------------------

#[test]
fn inventory_crafting_takes_ingredients_once() {
    install_data();
    let mut inv = PlayerInventory::default();
    let mut drops = Vec::new();
    // A log into the 2×2 grid (menu slot 1).
    inv.menu.carried = stack("oak_log", 2);
    inv.click(1, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
    assert_eq!(inv.craft[0].count, 2);
    assert_eq!(inv.craft_result.item, item("oak_planks"));
    // Taking the result uses one log.
    inv.click(0, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
    assert_eq!(
        (inv.menu.carried.item, inv.menu.carried.count),
        (item("oak_planks"), 4)
    );
    assert_eq!(inv.craft[0].count, 1);
    // A second click stacks onto the carried planks.
    inv.click(0, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
    assert_eq!(inv.menu.carried.count, 8);
    assert!(inv.craft[0].is_empty());
    assert!(inv.craft_result.is_empty());
    // Nothing can be put into the result slot.
    inv.click(0, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
    assert_eq!(inv.menu.carried.count, 8);
    assert!(drops.is_empty());
}

#[test]
fn inventory_shift_crafting_crafts_everything() {
    install_data();
    let mut inv = PlayerInventory::default();
    let mut drops = Vec::new();
    inv.menu.carried = stack("oak_log", 3);
    inv.click(1, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
    inv.click(0, 0, ContainerInput::QuickMove, SURVIVAL, &mut drops);
    let planks: i32 = inv
        .items
        .iter()
        .filter(|s| s.item == item("oak_planks"))
        .map(|s| s.count)
        .sum();
    assert_eq!(planks, 12);
    assert!(inv.craft.iter().all(ItemStack::is_empty));
    assert!(drops.is_empty());
}

// --- Dupe tests ------------------------------------------------------------------------------

/// Item counts by item id of everything a player holds or dropped.
fn census(inv: &PlayerInventory, extra: &[ItemStack], drops: &[Drop]) -> Vec<(u16, i32)> {
    let mut all: Vec<&ItemStack> = inv.items.iter().collect();
    all.extend(inv.armor.iter());
    all.push(&inv.offhand);
    all.extend(inv.craft.iter());
    all.push(&inv.menu.carried);
    all.extend(extra.iter());
    all.extend(drops.iter().map(|d| &d.stack));
    let mut counts: Vec<(u16, i32)> = Vec::new();
    for s in all.into_iter().filter(|s| !s.is_empty()) {
        match counts.iter_mut().find(|(i, _)| *i == s.item) {
            Some((_, n)) => *n += s.count,
            None => counts.push((s.item, s.count)),
        }
    }
    counts.sort_unstable();
    counts
}

pub(super) fn random_click(rng: &mut XoroshiroRandom, slots: i32) -> (i16, i8, ContainerInput) {
    let inputs = [
        ContainerInput::Pickup,
        ContainerInput::QuickMove,
        ContainerInput::Swap,
        ContainerInput::Clone,
        ContainerInput::Throw,
        ContainerInput::QuickCraft,
        ContainerInput::PickupAll,
    ];
    let input = inputs[usize::try_from(rng.next_int_bounded(7)).unwrap_or(0)];
    let slot = match rng.next_int_bounded(10) {
        0 => -999,
        1 => -1,
        _ => i16::try_from(rng.next_int_bounded(slots)).unwrap_or(0),
    };
    let button = match input {
        ContainerInput::Swap => {
            [0, 3, 8, 40, 9, 41][usize::try_from(rng.next_int_bounded(6)).unwrap_or(0)]
        }
        ContainerInput::QuickCraft => {
            let header = rng.next_int_bounded(4);
            let kind = rng.next_int_bounded(4);
            i8::try_from(header | (kind << 2)).unwrap_or(0)
        }
        _ => i8::try_from(rng.next_int_bounded(3)).unwrap_or(0),
    };
    (slot, button, input)
}

#[test]
fn random_inventory_clicks_conserve_items() {
    install_data();
    let mut rng = XoroshiroRandom::new(40);
    for round in 0..200 {
        let mut inv = PlayerInventory::default();
        // Items without 2×2 recipes, so crafting cannot change counts.
        inv.items[0] = stack("diamond", 37);
        inv.items[5] = stack("emerald", 64);
        inv.items[12] = stack("diamond", 20);
        inv.items[30] = stack("diamond_sword", 1);
        inv.armor[3] = stack("diamond_helmet", 1);
        let mut drops = Vec::new();
        let before = census(&inv, &[], &drops);
        for _ in 0..200 {
            let (slot, button, input) = random_click(&mut rng, 46);
            if !super::is_valid_slot_index(&inv, slot) {
                continue;
            }
            inv.click(slot, button, input, SURVIVAL, &mut drops);
        }
        assert_eq!(census(&inv, &[], &drops), before, "round {round}");
    }
}

/// A flat level with a block at `pos`.
pub(super) fn level_with(pos: BlockPos, state: vanillite_data::block::BlockState) -> Level {
    let mut level = crate::level::tests::flat_level();
    level.set_block(pos, state, update::ALL);
    level
}

pub(super) fn request(
    kind: MenuKind,
    storage: Storage,
    pos: BlockPos,
    block: vanillite_data::block::BlockId,
) -> MenuRequest {
    MenuRequest {
        player: None,
        menu: kind,
        storage,
        title: vanillite_protocol::text::Component::text("test"),
        pos,
        block,
    }
}

/// A client that drops every packet.
pub(super) struct NullSink;

impl crate::player::PacketSink for NullSink {
    fn send_body(&self, _: Vec<u8>) -> bool {
        true
    }
    fn send_shared(&self, _: &Arc<vanillite_protocol::SharedPacket>) -> bool {
        true
    }
    fn close(&self) {}
    fn is_closed(&self) -> bool {
        false
    }
    fn latency_ms(&self) -> u32 {
        0
    }
}

/// Opens a menu without a client.
pub(super) fn open(inv: &mut PlayerInventory, level: &mut Level, request: MenuRequest) {
    let sink = NullSink;
    let who = super::container::Opener {
        spectator: false,
        infinite: false,
        disconnected: false,
    };
    let mut drops = Vec::new();
    super::container::open(inv, level, request, who, &mut drops, &sink);
}

#[test]
fn random_chest_clicks_conserve_items() {
    install_data();
    let pos = BlockPos::new(0, 0, 0);
    let mut rng = XoroshiroRandom::new(7);
    for round in 0..60 {
        let mut level = level_with(pos, blocks::CHEST.default_state());
        if let Some(items) = crate::block::container::items_mut(&mut level, pos) {
            items[0] = stack("diamond", 50);
            items[26] = stack("emerald", 3);
        }
        let mut inv = PlayerInventory::default();
        inv.items[0] = stack("diamond", 64);
        inv.items[20] = stack("emerald", 12);
        open(
            &mut inv,
            &mut level,
            request(
                MenuKind::Generic { rows: 3 },
                Storage::Block(pos),
                pos,
                blocks::CHEST,
            ),
        );
        let chest = |level: &Level| {
            crate::block::container::items(level, pos)
                .unwrap_or_default()
                .to_vec()
        };
        let mut drops = Vec::new();
        let before = census(&inv, &chest(&level), &drops);
        for _ in 0..200 {
            let (slot, button, input) = random_click(&mut rng, 63);
            super::container::with_view(
                &mut inv,
                &mut level,
                Vec3::default(),
                None,
                None,
                |view| {
                    if super::is_valid_slot_index(view, slot) {
                        super::click(view, slot, button, input, SURVIVAL, &mut drops);
                    }
                },
            );
        }
        let carried = inv
            .container
            .as_ref()
            .map(|m| m.state.carried.clone())
            .unwrap_or_default();
        let mut extra = chest(&level);
        extra.push(carried);
        assert_eq!(census(&inv, &extra, &drops), before, "round {round}");
    }
}

#[test]
fn result_and_shulker_slots_refuse_items() {
    install_data();
    let pos = BlockPos::new(0, 0, 0);
    // Furnace: nothing goes into the result slot (2), fuel only into slot 1.
    let mut level = level_with(pos, blocks::FURNACE.default_state());
    let mut inv = PlayerInventory::default();
    open(
        &mut inv,
        &mut level,
        request(
            MenuKind::Furnace(CookingKind::Smelting),
            Storage::Block(pos),
            pos,
            blocks::FURNACE,
        ),
    );
    let mut drops = Vec::new();
    super::container::with_view(&mut inv, &mut level, Vec3::default(), None, None, |view| {
        view.menu.state.carried = stack("diamond", 5);
        super::click(view, 2, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
        super::click(view, 1, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
        assert!(view.item(2).is_empty());
        assert!(view.item(1).is_empty());
        assert_eq!(view.menu.state.carried.count, 5);
    });
    // Shulker box menu: no shulker boxes inside.
    let mut level = level_with(pos, blocks::SHULKER_BOX.default_state());
    let mut inv = PlayerInventory::default();
    inv.items[0] = stack("shulker_box", 1);
    open(
        &mut inv,
        &mut level,
        request(
            MenuKind::ShulkerBox,
            Storage::Block(pos),
            pos,
            blocks::SHULKER_BOX,
        ),
    );
    super::container::with_view(&mut inv, &mut level, Vec3::default(), None, None, |view| {
        // Shift click from the hotbar (slot 27 + 27 = 54).
        super::click(view, 54, 0, ContainerInput::QuickMove, SURVIVAL, &mut drops);
        // Number key 1 onto container slot 0.
        super::click(view, 0, 0, ContainerInput::Swap, SURVIVAL, &mut drops);
        assert!((0..27).all(|i| view.item(i).is_empty()));
    });
    assert_eq!(inv.items[0].item, item("shulker_box"));
}

#[test]
fn breaking_a_chest_drops_its_contents_and_closing_returns_the_carried_stack() {
    install_data();
    let pos = BlockPos::new(0, 0, 0);
    let mut level = level_with(pos, blocks::CHEST.default_state());
    if let Some(items) = crate::block::container::items_mut(&mut level, pos) {
        items[3] = stack("diamond", 40);
    }
    let mut inv = PlayerInventory::default();
    open(
        &mut inv,
        &mut level,
        request(
            MenuKind::Generic { rows: 3 },
            Storage::Block(pos),
            pos,
            blocks::CHEST,
        ),
    );
    assert!(matches!(level.block_entity(pos), Some(BlockEntity::Container(c)) if c.openers == 1));
    let mut drops = Vec::new();
    super::container::with_view(&mut inv, &mut level, Vec3::default(), None, None, |view| {
        super::click(view, 3, 1, ContainerInput::Pickup, SURVIVAL, &mut drops);
    });
    level.set_block(pos, vanillite_data::block::BlockState::AIR, update::ALL);
    let dropped: i32 = level
        .entities
        .items
        .iter()
        .map(|(_, d)| d.stack.count)
        .sum();
    assert_eq!(dropped, 20);
    // The menu is no longer valid; closing it puts the carried half back.
    assert!(!super::container::still_valid(
        &inv,
        &level,
        &|_| true,
        &|_| true
    ));
    let who = super::container::Opener {
        spectator: false,
        infinite: false,
        disconnected: false,
    };
    super::container::close(&mut inv, &mut level, who, &mut drops, None);
    assert_eq!(inv.items[0].count, 20);
    assert!(drops.is_empty());
}

#[test]
fn removing_a_dead_player_drops_carried_grid_and_open_menu_cursor() {
    install_data();
    let pos = BlockPos::new(0, 0, 0);
    let mut level = level_with(pos, blocks::CHEST.default_state());
    let mut inv = PlayerInventory::default();
    inv.craft[0] = stack("oak_planks", 1);
    inv.craft[3] = stack("stick", 2);
    inv.menu.carried = stack("bread", 3);
    open(
        &mut inv,
        &mut level,
        request(
            MenuKind::Generic { rows: 3 },
            Storage::Block(pos),
            pos,
            blocks::CHEST,
        ),
    );
    if let Some(menu) = inv.container.as_deref_mut() {
        menu.state.carried = stack("diamond", 5);
    }
    let mut drops = Vec::new();
    super::container::close_on_leave(&mut inv, &mut level, false, &mut drops);
    // Nothing goes back into the (already emptied) inventory: everything drops.
    assert!(inv.container.is_none());
    assert!(inv.menu.carried.is_empty());
    assert!(inv.craft.iter().all(ItemStack::is_empty));
    assert!(inv.items.iter().all(ItemStack::is_empty));
    let dropped: Vec<(u16, i32)> = drops
        .iter()
        .map(|d| (d.stack.item, d.stack.count))
        .collect();
    assert_eq!(
        dropped,
        vec![
            (item("diamond"), 5),
            (item("bread"), 3),
            (item("oak_planks"), 1),
            (item("stick"), 2),
        ]
    );
    assert!(matches!(level.block_entity(pos), Some(BlockEntity::Container(c)) if c.openers == 0));
}

#[test]
fn furnace_smelts_in_200_ticks_and_burns_coal() {
    install_data();
    let pos = BlockPos::new(0, 0, 0);
    let mut level = level_with(pos, blocks::FURNACE.default_state());
    if let Some(BlockEntity::Furnace(f)) = level.block_entity_mut(pos) {
        f.set_item(0, stack("raw_iron", 2));
        f.items[1] = stack("coal", 1);
    }
    // The ticker joins at the next tick.
    for tick in 1..=200 {
        level.tick_block_entities();
        let Some(BlockEntity::Furnace(f)) = level.block_entity(pos) else {
            panic!("furnace gone");
        };
        if tick < 200 {
            assert!(f.items[2].is_empty(), "tick {tick}");
        }
    }
    let Some(BlockEntity::Furnace(f)) = level.block_entity(pos) else {
        panic!("furnace gone");
    };
    assert_eq!(f.items[2].item, item("iron_ingot"));
    assert_eq!(f.items[0].count, 1);
    assert!(f.items[1].is_empty(), "the coal burns");
    assert_eq!(f.lit_total_time, 1600);
    assert!(level.block(pos).value("lit") == Some("true"));
}

/// Opens a workstation menu at a block.
fn workstation(kind: MenuKind, block: vanillite_data::block::BlockId) -> (PlayerInventory, Level) {
    install_data();
    let pos = BlockPos::new(0, 0, 0);
    let mut level = level_with(pos, block.default_state());
    let mut inv = PlayerInventory::default();
    open(
        &mut inv,
        &mut level,
        request(kind, Storage::Local, pos, block),
    );
    (inv, level)
}

fn with_enchantments(name: &str, list: &[(&str, i32)]) -> ItemStack {
    let data = install_data();
    let mut s = stack(name, 1);
    let ids: Vec<(u32, i32)> = list
        .iter()
        .map(|(e, l)| (data.enchantments[&format!("minecraft:{e}")], *l))
        .collect();
    super::workstation::set_crafting_enchantments(&mut s, &ids);
    s
}

#[test]
fn stonecutter_selects_and_cuts() {
    let (mut inv, mut level) = workstation(MenuKind::Stonecutter, blocks::STONECUTTER);
    let mut drops = Vec::new();
    super::container::with_view(&mut inv, &mut level, Vec3::default(), None, None, |view| {
        view.menu.state.carried = stack("stone", 3);
        super::click(view, 0, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
        assert!(
            view.menu.recipes.len() > 5,
            "{} stone recipes",
            view.menu.recipes.len()
        );
        assert!(view.item(1).is_empty());
        // Pick the stone bricks entry.
        let data = install_data();
        let index = view
            .menu
            .recipes
            .iter()
            .position(|&r| data.recipes.stonecutting[r].result.item == item("stone_bricks"))
            .expect("stone bricks");
        assert!(view.button(i32::try_from(index).unwrap(), &mut Vec::new()));
        assert_eq!(view.item(1).item, item("stone_bricks"));
        super::click(view, 1, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
        assert_eq!(view.menu.state.carried.item, item("stone_bricks"));
        assert_eq!(view.item(0).count, 2);
        assert_eq!(
            view.item(1).item,
            item("stone_bricks"),
            "the selection stays"
        );
    });
}

#[test]
fn anvil_repairs_renames_and_combines() {
    let (mut inv, mut level) = workstation(MenuKind::Anvil, blocks::ANVIL);
    let mut xp = crate::survival::player::Experience {
        level: 30,
        ..Default::default()
    };
    let mut drops = Vec::new();
    super::container::with_view(
        &mut inv,
        &mut level,
        Vec3::default(),
        Some(&mut xp),
        None,
        |view| {
            // A damaged diamond sword and two diamonds: two repair steps of a quarter each.
            let mut sword = stack("diamond_sword", 1);
            crate::item::set_damage(&mut sword, 1500);
            view.menu.state.carried = sword;
            super::click(view, 0, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
            view.menu.state.carried = stack("diamond", 2);
            super::click(view, 1, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
            let result = view.item(2).clone();
            assert_eq!(crate::item::damage(&result), 1500 - 390 - 390);
            assert_eq!(view.menu.data[0], 2, "two repair steps");
            assert_eq!(view.menu.repair_item_count_cost, 2);
            // Renaming adds one level.
            assert!(view.rename("Blade"));
            assert_eq!(view.menu.data[0], 3);
            super::click(view, 2, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
            assert_eq!(view.menu.state.carried.item, item("diamond_sword"));
            assert!(view.item(0).is_empty() && view.item(1).is_empty());
        },
    );
    assert_eq!(xp.level, 27);
    // Two Sharpness IV books make Sharpness V.
    let (mut inv, mut level) = workstation(MenuKind::Anvil, blocks::ANVIL);
    super::container::with_view(
        &mut inv,
        &mut level,
        Vec3::default(),
        Some(&mut xp),
        None,
        |view| {
            let book = with_enchantments("enchanted_book", &[("sharpness", 4)]);
            view.menu.state.carried = book.clone();
            super::click(view, 0, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
            view.menu.state.carried = book;
            super::click(view, 1, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
            let result = super::workstation::crafting_enchantments(view.item(2));
            let sharpness = install_data().enchantments["minecraft:sharpness"];
            assert_eq!(result, vec![(sharpness, 5)]);
            // Book fee: max(1, 1 / 2) * 5.
            assert_eq!(view.menu.data[0], 5);
        },
    );
}

#[test]
fn grindstone_removes_enchantments_but_curses() {
    let (mut inv, mut level) = workstation(MenuKind::Grindstone, blocks::GRINDSTONE);
    let mut drops = Vec::new();
    super::container::with_view(&mut inv, &mut level, Vec3::default(), None, None, |view| {
        let sword = with_enchantments("iron_sword", &[("sharpness", 2), ("vanishing_curse", 1)]);
        view.menu.state.carried = sword;
        super::click(view, 0, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
        let result = super::workstation::crafting_enchantments(view.item(2));
        let curse = install_data().enchantments["minecraft:vanishing_curse"];
        assert_eq!(result, vec![(curse, 1)]);
        super::click(view, 2, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
        assert!(view.item(0).is_empty());
    });
}

#[test]
fn smithing_upgrades_to_netherite() {
    let (mut inv, mut level) = workstation(MenuKind::Smithing, blocks::SMITHING_TABLE);
    let mut drops = Vec::new();
    super::container::with_view(&mut inv, &mut level, Vec3::default(), None, None, |view| {
        let sword = with_enchantments("diamond_sword", &[("sharpness", 3)]);
        for (slot, s) in [
            (0, stack("netherite_upgrade_smithing_template", 1)),
            (1, sword),
            (2, stack("netherite_ingot", 2)),
        ] {
            view.menu.state.carried = s;
            super::click(view, slot, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
        }
        let result = view.item(3).clone();
        assert_eq!(result.item, item("netherite_sword"));
        assert_eq!(super::workstation::crafting_enchantments(&result).len(), 1);
        super::click(view, 3, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
        assert!(view.item(0).is_empty() && view.item(1).is_empty());
        assert_eq!(view.item(2).count, 1);
    });
}

#[test]
fn brewing_stand_brews_awkward_potions_with_blaze_powder() {
    install_data();
    let pos = BlockPos::new(0, 0, 0);
    let mut level = level_with(pos, blocks::BREWING_STAND.default_state());
    let data = install_data();
    let water = data
        .recipes
        .brewing
        .iter()
        .find(|r| r.id == "minecraft:brewing/potion_water_nether_wart")
        .expect("recipe");
    let mut bottle = stack("potion", 1);
    let water_id = vanillite_data::registry::POTION
        .id("minecraft:water")
        .expect("water");
    let mut contents = vec![1u8];
    vanillite_protocol::codec::write_varint(&mut contents, i32::from(water_id));
    contents.extend_from_slice(&[0, 0, 0]);
    bottle.patch_mut().set(
        vanillite_protocol::components::ids::POTION_CONTENTS,
        contents.into_boxed_slice(),
    );
    if let Some(BlockEntity::BrewingStand(b)) = level.block_entity_mut(pos) {
        b.items[0] = bottle.clone();
        b.items[2] = bottle;
        b.items[3] = stack("nether_wart", 2);
        b.items[4] = stack("blaze_powder", 1);
    }
    for _ in 0..401 {
        level.tick_block_entities();
    }
    let Some(BlockEntity::BrewingStand(b)) = level.block_entity(pos) else {
        panic!("brewing stand gone");
    };
    assert_eq!(b.items[0], water.output);
    assert_eq!(b.items[2], water.output);
    assert_eq!(b.items[3].count, 1);
    assert!(b.items[4].is_empty());
    assert_eq!(b.fuel, 19);
    assert_eq!(level.block(pos).value("has_bottle_0"), Some("true"));
}

#[test]
fn enchanting_table_enchants_with_lapis_and_levels() {
    let (mut inv, mut level) = workstation(MenuKind::Enchantment, blocks::ENCHANTING_TABLE);
    // Fifteen bookshelves around the table.
    for x in -2i32..=2 {
        for z in -2i32..=2 {
            if x.abs() == 2 || z.abs() == 2 {
                level.set_block(
                    BlockPos::new(x, 0, z),
                    blocks::BOOKSHELF.default_state(),
                    update::ALL,
                );
            }
        }
    }
    let mut xp = crate::survival::player::Experience {
        level: 30,
        seed: 12345,
        ..Default::default()
    };
    let mut drops = Vec::new();
    super::container::with_view(
        &mut inv,
        &mut level,
        Vec3::default(),
        Some(&mut xp),
        None,
        |view| {
            view.menu.state.carried = stack("diamond_sword", 1);
            super::click(view, 0, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
            view.menu.state.carried = stack("lapis_lazuli", 5);
            super::click(view, 1, 0, ContainerInput::Pickup, SURVIVAL, &mut drops);
            assert_eq!(
                view.menu.data[2], 30,
                "16 shelves cap the third option at 30"
            );
            assert!(
                view.menu.data[4..7].iter().all(|&c| c >= 0),
                "clues {:?}",
                view.menu.data
            );
            // `slotsChanged` (I4-09 of #272): each clue is the next draw of the menu random
            // after `getEnchantmentList` reseeded it with seed + slot and selected from it.
            let d = crate::player::play::game_data().expect("game data");
            let seed = view.menu.data[3];
            let item = view.item(0).clone();
            for slot in 0..3usize {
                let mut r =
                    vanillite_base::random::LegacyRandom::new(i64::from(seed) + slot as i64);
                let cost = view.menu.data[slot];
                let list = crate::enchant::select_enchantment(
                    &d.enchanting,
                    &mut r,
                    &item,
                    cost,
                    &d.enchanting.table,
                );
                let len = i32::try_from(list.len()).unwrap_or(1);
                let pick = vanillite_base::random::RandomSource::next_int_bounded(&mut r, len);
                let (id, level) = list[usize::try_from(pick).unwrap_or(0)];
                assert_eq!(
                    (view.menu.data[4 + slot], view.menu.data[7 + slot]),
                    (i32::try_from(id).unwrap_or(-1), level),
                    "clue of option {slot}"
                );
            }
            assert!(view.button(2, &mut Vec::new()));
            assert!(!super::workstation::crafting_enchantments(view.item(0)).is_empty());
            assert_eq!(view.item(1).count, 2);
        },
    );
    assert_eq!(xp.level, 27);
    assert_ne!(xp.seed, 12345);
}

#[test]
fn only_the_editor_writes_signs_and_wax_locks_them() {
    install_data();
    let pos = BlockPos::new(0, 0, 0);
    let mut level = level_with(pos, blocks::OAK_SIGN.default_state());
    let lines = [
        "a\u{a7}cb".to_owned(),
        String::new(),
        String::new(),
        String::new(),
    ];
    // Nobody may edit until the editor opened for a player.
    crate::block::sign::update_text(&mut level, pos, 7, true, &lines);
    let text = |level: &Level| match level.block_entity(pos) {
        Some(BlockEntity::Sign(s)) => s.front.messages[0].plain(),
        _ => panic!("no sign"),
    };
    assert_eq!(text(&level), "");
    if let Some(BlockEntity::Sign(s)) = level.block_entity_mut(pos) {
        s.editor = Some(7);
    }
    crate::block::sign::update_text(&mut level, pos, 8, true, &lines);
    assert_eq!(text(&level), "", "another player");
    crate::block::sign::update_text(&mut level, pos, 7, true, &lines);
    assert_eq!(text(&level), "ab", "formatting codes are stripped");
    // The lock is released; waxed signs stay as they are.
    if let Some(BlockEntity::Sign(s)) = level.block_entity_mut(pos) {
        assert_eq!(s.editor, None);
        s.editor = Some(7);
        s.waxed = true;
    }
    let other = ["c".to_owned(), String::new(), String::new(), String::new()];
    crate::block::sign::update_text(&mut level, pos, 7, true, &other);
    assert_eq!(text(&level), "ab");
}
