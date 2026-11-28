//! Java menus as Bedrock containers: chests, barrels, shulker boxes, ender chests, hoppers,
//! dispensers and droppers, furnaces, brewing stands, crafting tables, crafters, anvils,
//! enchanting tables, smithing tables, stonecutters, grindstones, cartography tables, beacons,
//! looms and lecterns open at the block the player used (chest minecarts, chest boats and hopper
//! minecarts at the entity; the inventories of horses, donkeys, mules, llamas and camels at the
//! mount). A lectern's book lives in its Bedrock block actor; crafter slots locked on Java are
//! shown through the crafter's block actor.
//! A menu with no block or entity behind it (a plugin menu) becomes a form whose buttons click
//! the slots. Bedrock computes anvil, smithing, stonecutter, grindstone and cartography results
//! itself; taking one clicks the Java result slot (after `rename_item` or the stonecutter button).
//!
//! The menu's own slots are mirrored in [`Open`]; its player inventory part maps onto the
//! inventory mirror. Bedrock requests on these slots become Java clicks in the open menu
//! ([`super::inventory`]).

use std::time::{Duration, Instant};

use vanillite_protocol::codec::Reader as JReader;
use vanillite_protocol::item::ItemStack;
use vanillite_protocol::packets::play::clientbound as pcb;
use vanillite_protocol::packets::play::serverbound::{self as psb, ContainerInput, HashedStack};

use super::Session;
use super::inventory::{Slot, Target};
use crate::forms::{Button, Form};
use crate::packets::{self as bp, container_type as ct, id, meta, window};

/// Form id of a menu shown as a form.
pub(super) const MENU_FORM: u32 = u32::MAX - 1;

/// How long a block or entity use may open a menu.
const USE_WINDOW: Duration = Duration::from_secs(1);

/// Slots of the player inventory part of a menu.
const INVENTORY_PART: usize = 36;

/// The kind of an open menu: how its slots map to Bedrock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// `generic_9xN` and `shulker_box`.
    Chest,
    /// `generic_3x3`.
    Dispenser,
    Hopper,
    /// `furnace`, `blast_furnace`, `smoker`: ingredient, fuel, result.
    Furnace,
    /// `brewing_stand`: three bottles, ingredient, fuel.
    Brewing,
    /// `crafting`: result, 3x3 grid.
    Crafting,
    /// `crafter`: the 3x3 grid (the result slot after the inventory is not shown).
    Crafter,
    /// `anvil`: input, material, result.
    Anvil,
    /// `enchantment`: item, lapis.
    Enchanting,
    /// `smithing`: template, base, addition, result.
    Smithing,
    /// `stonecutter`: input, result.
    Stonecutter,
    /// `grindstone`: input, additional, result.
    Grindstone,
    /// `cartography_table`: map, additional, result.
    Cartography,
    /// `beacon`: the payment item.
    Beacon,
    /// `loom`: banner, dye, pattern, result.
    Loom,
    /// `lectern`: the book.
    Lectern,
    /// `mount_screen_open`: saddle, body armour, then the chest.
    Mount(Mount),
}

/// How a mount's Java slots (saddle, body armour, chest) sit in the Bedrock horse window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mount {
    /// Saddle and armour (horses, camels, nautiluses): the same slots.
    Horse,
    /// Saddle then the chest (donkeys, mules): no armour slot.
    Chested,
    /// Carpet then the chest (llamas): no saddle slot.
    Llama,
}

impl Mount {
    fn of(java_type: &str) -> Option<Mount> {
        let name = java_type.strip_prefix("minecraft:")?;
        Some(match name {
            "llama" | "trader_llama" => Mount::Llama,
            "donkey" | "mule" => Mount::Chested,
            "horse" | "zombie_horse" | "skeleton_horse" | "camel" | "camel_husk" | "nautilus"
            | "zombie_nautilus" => Mount::Horse,
            _ => return None,
        })
    }

    /// The Bedrock slot of a Java menu slot.
    pub(super) fn bedrock(self, java: usize) -> Option<usize> {
        match (self, java) {
            (Mount::Horse, 0 | 1) | (Mount::Chested, 0) => Some(java),
            (Mount::Horse, _) | (Mount::Chested, 1) | (Mount::Llama, 0) => None,
            (Mount::Chested | Mount::Llama, j) => j.checked_sub(1),
        }
    }

    /// The Java menu slot of a Bedrock slot.
    pub(super) fn java(self, bedrock: usize) -> usize {
        match (self, bedrock) {
            (Mount::Horse, b) | (Mount::Chested, b @ 0) => b,
            (Mount::Chested | Mount::Llama, b) => b.saturating_add(1),
        }
    }
}

/// Whether a Java entity type has an inventory menu its rider opens (`HasCustomInventoryScreen`).
pub(super) fn has_inventory_screen(java_type: &str) -> bool {
    Mount::of(java_type).is_some()
        || java_type.ends_with("_chest_boat")
        || java_type.ends_with("_chest_raft")
}

/// Bedrock recipe network ids of the three enchanting options (above all crafting recipes).
pub(super) const ENCHANT_NET_ID: u32 = 0x4000_0000;

/// The open Java menu.
#[derive(Debug)]
pub(super) struct Open {
    pub container: i32,
    pub window: u8,
    pub kind: Kind,
    pub slots: Vec<Slot>,
    pub state_id: i32,
    /// Bedrock container type (for `container_close`).
    container_type: i8,
    /// The Java total cooking time of a furnace (Bedrock's progress bar is 200 ticks).
    cook: i32,
    /// Shown as a form (no block or entity to open it at).
    form: bool,
    title: String,
    /// Enchanting table data: costs, clue enchantments and clue levels of the three options.
    enchant: [[i32; 3]; 3],
    /// The block the menu belongs to (crafters and lecterns).
    pos: Option<[i32; 3]>,
    /// Crafter: bit per disabled slot. Lectern: the Java page.
    state: i32,
    /// The Bedrock window is not open yet (a lectern opens once its book is known).
    pending_open: Option<([i32; 3], i64)>,
}

/// What the player used last: a block or an entity (Java id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Used {
    Block([i32; 3]),
    Entity(i32),
}

/// The kind, size and Bedrock container type of a Java menu type.
fn menu_kind(menu: &str) -> Option<(Kind, usize, i8)> {
    Some(match menu {
        "minecraft:generic_9x1" => (Kind::Chest, 9, ct::CONTAINER),
        "minecraft:generic_9x2" => (Kind::Chest, 18, ct::CONTAINER),
        "minecraft:generic_9x3" | "minecraft:shulker_box" => (Kind::Chest, 27, ct::CONTAINER),
        "minecraft:generic_9x4" => (Kind::Chest, 36, ct::CONTAINER),
        "minecraft:generic_9x5" => (Kind::Chest, 45, ct::CONTAINER),
        "minecraft:generic_9x6" => (Kind::Chest, 54, ct::CONTAINER),
        "minecraft:generic_3x3" => (Kind::Dispenser, 9, ct::DISPENSER),
        "minecraft:hopper" => (Kind::Hopper, 5, ct::HOPPER),
        "minecraft:furnace" => (Kind::Furnace, 3, ct::FURNACE),
        "minecraft:blast_furnace" => (Kind::Furnace, 3, ct::BLAST_FURNACE),
        "minecraft:smoker" => (Kind::Furnace, 3, ct::SMOKER),
        "minecraft:brewing_stand" => (Kind::Brewing, 5, ct::BREWING_STAND),
        "minecraft:crafting" => (Kind::Crafting, 10, ct::WORKBENCH),
        "minecraft:crafter_3x3" => (Kind::Crafter, 9, ct::CRAFTER),
        "minecraft:anvil" => (Kind::Anvil, 3, ct::ANVIL),
        "minecraft:enchantment" => (Kind::Enchanting, 2, ct::ENCHANTMENT),
        "minecraft:smithing" => (Kind::Smithing, 4, ct::SMITHING_TABLE),
        "minecraft:stonecutter" => (Kind::Stonecutter, 2, ct::STONECUTTER),
        "minecraft:grindstone" => (Kind::Grindstone, 3, ct::GRINDSTONE),
        "minecraft:cartography_table" => (Kind::Cartography, 3, ct::CARTOGRAPHY),
        "minecraft:beacon" => (Kind::Beacon, 1, ct::BEACON),
        "minecraft:loom" => (Kind::Loom, 4, ct::LOOM),
        "minecraft:lectern" => (Kind::Lectern, 1, ct::LECTERN),
        _ => return None,
    })
}

/// The Bedrock window and slot of a slot of the open menu.
fn bedrock_slot(open: &Open, i: usize) -> Option<(u32, u32)> {
    let w = u32::from(open.window);
    let s = u32::try_from(i).ok()?;
    Some(match open.kind {
        // Bedrock: ingredient, three bottles, fuel.
        Kind::Brewing => match i {
            0..=2 => (w, s.saturating_add(1)),
            3 => (w, 0),
            4 => (w, 4),
            _ => return None,
        },
        Kind::Crafting => match i {
            1..=9 => (window::UI, s.saturating_add(31)),
            _ => return None,
        },
        // Bedrock computes the results of these itself; the inputs live in the UI window.
        Kind::Anvil => (window::UI, *[1, 2].get(i)?),
        Kind::Enchanting => (window::UI, *[14, 15].get(i)?),
        Kind::Smithing => (window::UI, *[53, 51, 52].get(i)?),
        Kind::Stonecutter => (window::UI, *[3].get(i)?),
        Kind::Grindstone => (window::UI, *[16, 17].get(i)?),
        Kind::Cartography => (window::UI, *[12, 13].get(i)?),
        Kind::Beacon => (window::UI, *[27].get(i)?),
        Kind::Loom => (window::UI, *[9, 10, 11].get(i)?),
        Kind::Lectern => return None,
        Kind::Mount(m) => (w, u32::try_from(m.bedrock(i)?).ok()?),
        _ => (w, s),
    })
}

/// Whether the menu's slots are shown one by one (UI window or remapped slots).
fn per_slot(kind: Kind) -> bool {
    !matches!(
        kind,
        Kind::Chest | Kind::Dispenser | Kind::Hopper | Kind::Furnace | Kind::Crafter
    )
}

/// "Stone" or "16 Stone" or the custom name of a stack.
fn stack_label(stack: &ItemStack) -> String {
    let custom = [
        vanillite_protocol::components::ids::CUSTOM_NAME,
        vanillite_protocol::components::ids::ITEM_NAME,
    ]
    .into_iter()
    .find_map(|c| stack.component(c))
    .and_then(|raw| crate::entity::component(&mut JReader::new(raw)).ok())
    .map(|c| c.plain());
    let name = custom.unwrap_or_else(|| super::trade::item_name(stack.item));
    if stack.count > 1 {
        format!("{} {name}", stack.count)
    } else {
        name
    }
}

impl Session {
    /// Remembers what the player used: a menu opening next opens at it.
    pub(super) fn note_use(&mut self, used: Used) {
        self.used = Some((used, Instant::now()));
    }

    /// `open_screen` of a menu other than a merchant's: opens it on Bedrock.
    pub(super) fn open_menu(&mut self, container: i32, menu: &str, title: String) {
        self.close_open_window(true);
        let Some((kind, size, mut container_type)) = menu_kind(menu) else {
            // Not translated yet (anvil, enchanting, smithing, ...): close it again.
            self.java_send(&psb::ContainerClose {
                container_id: container,
            });
            self.send(
                id::TEXT,
                &bp::text_tip("This menu is not available on Bedrock yet"),
            );
            return;
        };
        let used = self
            .used
            .take()
            .filter(|(_, at)| at.elapsed() <= USE_WINDOW)
            .map(|(u, _)| u);
        let (pos, entity) = match used {
            Some(Used::Block(pos)) => (Some(pos), -1),
            Some(Used::Entity(java)) => {
                let target = self.world.as_ref().and_then(|w| {
                    let e = w.entities.get(&java)?;
                    let pos = e.pos.map(crate::num::floor_i32);
                    Some((pos, super::uid(java), e.java_type))
                });
                match target {
                    Some((pos, uid, java_type)) => {
                        container_type = match super::play::java_type_name(java_type) {
                            "minecraft:hopper_minecart" => ct::CART_HOPPER,
                            "minecraft:chest_minecart" => ct::CART_CHEST,
                            n if n.ends_with("_chest_boat") || n.ends_with("_chest_raft") => {
                                ct::CHEST_BOAT
                            }
                            _ => container_type,
                        };
                        (Some(pos), uid)
                    }
                    None => (None, -1),
                }
            }
            None => (None, -1),
        };
        self.open_window(
            container,
            kind,
            size,
            container_type,
            pos.map(|p| (p, entity)),
            title,
        );
    }

    /// Opens the Bedrock window of a Java menu at a block or entity (`None`: as a form).
    fn open_window(
        &mut self,
        container: i32,
        kind: Kind,
        size: usize,
        container_type: i8,
        at: Option<([i32; 3], i64)>,
        title: String,
    ) {
        if kind == Kind::Lectern && std::mem::take(&mut self.taking_book) {
            // The player hit the lectern to take its book.
            self.java_send(&psb::ContainerButtonClick {
                container_id: container,
                button_id: 3,
            });
            self.java_send(&psb::ContainerClose {
                container_id: container,
            });
            return;
        }
        self.next_window = if self.next_window >= 99 {
            1
        } else {
            self.next_window.saturating_add(1)
        };
        let window = self.next_window;
        let java_total = if container_type == ct::FURNACE {
            200
        } else {
            100
        };
        let deferred = kind == Kind::Lectern;
        self.inv.open = Some(Open {
            container,
            window,
            kind,
            slots: vec![Slot::default(); size],
            state_id: 0,
            container_type,
            cook: java_total,
            form: at.is_none(),
            title,
            enchant: [[0, -1, -1]; 3],
            pos: at.filter(|(_, e)| *e == -1).map(|(p, _)| p),
            state: 0,
            pending_open: at.filter(|_| deferred),
        });
        if let Some((pos, entity)) = at.filter(|_| !deferred) {
            self.send(
                id::CONTAINER_OPEN,
                &bp::container_open(window, container_type, pos, entity),
            );
        }
    }

    /// `mount_screen_open`: the inventory of a horse, donkey, mule, llama, camel or nautilus.
    pub(super) fn java_mount_screen(&mut self, container: i32, columns: i32, java: i32) {
        self.close_open_window(true);
        let columns = usize::try_from(columns.clamp(0, 5)).unwrap_or(0);
        let target = self.world.as_ref().and_then(|w| {
            let e = w.entities.get(&java)?;
            Some((e.pos.map(crate::num::floor_i32), e.java_type))
        });
        let Some((pos, java_type)) = target else {
            self.java_send(&psb::ContainerClose {
                container_id: container,
            });
            return;
        };
        let name = super::play::java_type_name(java_type);
        let mount = Mount::of(name).unwrap_or(Mount::Horse);
        let chest = columns.saturating_mul(3);
        let size = chest.saturating_add(2);
        // The Bedrock horse window takes its layout from the mount's entity data.
        let bedrock_size = match mount {
            Mount::Horse => 2,
            Mount::Chested | Mount::Llama => chest.saturating_add(1),
        };
        if let Some(e) = self.world.as_mut().and_then(|w| w.entities.get_mut(&java)) {
            e.tamed = true;
            e.chested = chest > 0;
        }
        let runtime = self.actor(java);
        let strength = i32::try_from(columns).unwrap_or(0);
        let flags = self.actor_flags_of(java);
        let mut m = bp::Metadata::default()
            .byte(meta::CONTAINER_TYPE, ct::HORSE.to_le_bytes()[0])
            .int(
                meta::CONTAINER_SIZE,
                i32::try_from(bedrock_size).unwrap_or(0),
            )
            .int(meta::CONTAINER_STRENGTH_MODIFIER, 3);
        if mount == Mount::Llama {
            m = m.int(meta::STRENGTH, strength).int(meta::STRENGTH_MAX, 5);
        }
        if let Some(f) = flags {
            m = m.long(meta::FLAGS, f);
        }
        self.send(id::SET_ACTOR_DATA, &bp::set_actor_data(runtime, &m));
        self.open_window(
            container,
            Kind::Mount(mount),
            size,
            ct::HORSE,
            Some((pos, super::uid(java))),
            String::new(),
        );
    }

    /// Closes the Bedrock window of the open menu (`server_side`: the server closed it).
    pub(super) fn close_open_window(&mut self, server_side: bool) {
        if let Some(open) = self.inv.open.take()
            && !open.form
        {
            self.send(
                id::CONTAINER_CLOSE,
                &bp::container_close(open.window, open.container_type, server_side),
            );
        }
    }

    /// The client closed a window: closes the Java menu. Returns whether it was the open menu.
    pub(super) fn client_closed(&mut self, window: u8) -> bool {
        let Some(open) = self.inv.open.as_ref().filter(|o| o.window == window) else {
            return false;
        };
        let (container, container_type) = (open.container, open.container_type);
        self.inv.open = None;
        self.java_send(&psb::ContainerClose {
            container_id: container,
        });
        self.send(
            id::CONTAINER_CLOSE,
            &bp::container_close(window, container_type, false),
        );
        true
    }

    /// The server closed a menu.
    pub(super) fn java_menu_closed(&mut self, container: i32) {
        if self
            .inv
            .open
            .as_ref()
            .is_some_and(|o| o.container == container)
        {
            self.close_open_window(true);
        }
    }

    /// Sends one slot of the open menu.
    fn send_open_slot(&mut self, i: usize) {
        let Some(open) = &self.inv.open else {
            return;
        };
        if open.form {
            return;
        }
        let (Some((w, s)), Some(slot)) = (bedrock_slot(open, i), open.slots.get(i)) else {
            return;
        };
        let item = self.instance(slot);
        self.send(id::INVENTORY_SLOT, &bp::inventory_slot(w, s, &item));
    }

    /// Sends all slots of the open menu.
    fn send_open_content(&mut self) {
        let Some(open) = &self.inv.open else {
            return;
        };
        if open.form {
            self.show_menu_form();
            return;
        }
        if per_slot(open.kind) {
            for i in 0..open.slots.len() {
                self.send_open_slot(i);
            }
            return;
        }
        let items = self.version.palette.items();
        let mut body = Vec::with_capacity(open.slots.len().saturating_mul(12));
        for s in &open.slots {
            items.write_instance(&mut body, &s.stack, s.net, &self.enchantments);
        }
        let packet = bp::inventory_content(u32::from(open.window), open.slots.len(), &body);
        self.send(id::INVENTORY_CONTENT, &packet);
    }

    /// `container_set_content` of the open menu.
    pub(super) fn open_content(&mut self, c: pcb::ContainerSetContent) {
        let Some(open) = self
            .inv
            .open
            .as_mut()
            .filter(|o| o.container == c.container_id)
        else {
            return;
        };
        open.state_id = c.state_id;
        let size = open.slots.len();
        let mut items = c.items.into_iter();
        for i in 0..size {
            let stack = items.next().unwrap_or_default();
            self.inv.put(Target::Open(i), stack);
        }
        for k in 0..INVENTORY_PART {
            let stack = items.next().unwrap_or_default();
            self.inv.put(Target::Menu(k.saturating_add(9)), stack);
        }
        self.inv.put(Target::Cursor, c.carried);
        self.send_open_content();
        self.send_inventory();
        self.send_lectern();
        self.send_crafter();
    }

    /// `container_set_slot` of the open menu.
    pub(super) fn open_slot(&mut self, c: pcb::ContainerSetSlot) {
        let Some(open) = self
            .inv
            .open
            .as_mut()
            .filter(|o| o.container == c.container_id)
        else {
            return;
        };
        open.state_id = c.state_id;
        let size = open.slots.len();
        let form = open.form;
        let Ok(i) = usize::try_from(c.slot) else {
            return;
        };
        if i < size {
            if self.inv.put(Target::Open(i), c.item) {
                if form {
                    self.show_menu_form();
                } else {
                    self.send_open_slot(i);
                    self.send_lectern();
                }
            }
        } else if let Some(k) = i.checked_sub(size).filter(|&k| k < INVENTORY_PART) {
            let menu = k.saturating_add(9);
            if self.inv.put(Target::Menu(menu), c.item) {
                self.send_slot(menu);
            }
        }
    }

    /// `container_set_data`: furnace and brewing stand progress.
    pub(super) fn java_container_data(&mut self, r: &mut JReader<'_>) {
        let Ok(d) = <pcb::ContainerSetData as vanillite_protocol::codec::Decode>::decode(r) else {
            return;
        };
        let Some(open) = self
            .inv
            .open
            .as_mut()
            .filter(|o| o.container == d.container_id && !o.form)
        else {
            return;
        };
        let window = open.window;
        let value = i32::from(d.value);
        let data: &[(i32, i32)] = match (open.kind, d.id) {
            // Java: lit time, lit duration, cooking progress, total cooking time.
            (Kind::Furnace, 0) => &[(1, value)],
            (Kind::Furnace, 1) => &[(2, value)],
            (Kind::Furnace, 2) => {
                let ticks = i64::from(value)
                    .saturating_mul(200)
                    .checked_div(i64::from(open.cook.max(1)))
                    .unwrap_or(0);
                let ticks = i32::try_from(ticks).unwrap_or(0);
                self.send(
                    id::CONTAINER_SET_DATA,
                    &bp::container_set_data(window, 0, ticks),
                );
                return;
            }
            (Kind::Furnace, 3) => {
                open.cook = value.max(1);
                return;
            }
            // Java: disabled slots 0-8 (1 = disabled), shown in the crafter's block actor.
            (Kind::Crafter, k @ 0..=8) => {
                let bit = 1i32.wrapping_shl(u32::try_from(k).unwrap_or(0));
                let state = if value == 1 {
                    open.state | bit
                } else {
                    open.state & !bit
                };
                if state != open.state {
                    open.state = state;
                    self.send_crafter();
                }
                return;
            }
            // Java: the page of the lectern's book.
            (Kind::Lectern, 0) => {
                if open.state != value {
                    open.state = value;
                    self.send_lectern();
                }
                return;
            }
            // Java: brew time, fuel; Bedrock also wants the full fuel.
            (Kind::Brewing, 0) => &[(0, value)],
            (Kind::Brewing, 1) => &[(1, value), (2, 20)],
            // Java: costs 0-2, seed 3, clue enchantments 4-6, clue levels 7-9.
            (Kind::Enchanting, k @ (0..=2 | 4..=9)) => {
                let k = usize::try_from(k).unwrap_or(0);
                let (row, option) = match k {
                    0..=2 => (0, k),
                    4..=6 => (1, k.saturating_sub(4)),
                    _ => (2, k.saturating_sub(7)),
                };
                if let Some(v) = open.enchant.get_mut(row).and_then(|r| r.get_mut(option)) {
                    *v = value;
                }
                self.send_enchant_options();
                return;
            }
            _ => return,
        };
        for &(key, v) in data {
            self.send(
                id::CONTAINER_SET_DATA,
                &bp::container_set_data(window, key, v),
            );
        }
    }

    /// The crafter's block actor with the slots disabled on Java.
    fn send_crafter(&mut self) {
        let Some((pos, disabled)) = self
            .inv
            .open
            .as_ref()
            .filter(|o| o.kind == Kind::Crafter)
            .and_then(|o| Some((o.pos?, o.state)))
        else {
            return;
        };
        let [x, y, z] = pos;
        let mut c = crate::nbt::Compound::default();
        c.string("id", "Crafter")
            .short(
                "disabled_slots",
                i16::try_from(disabled & 0x1ff).unwrap_or(0),
            )
            .int("x", x)
            .int("y", y)
            .int("z", z)
            .byte("isMovable", 1);
        self.send(id::BLOCK_ACTOR_DATA, &bp::block_actor_data(pos, &c.root()));
    }

    /// The lectern's block actor with the book of the open lectern menu (Bedrock shows the book
    /// from it), then the lectern window once the book is known.
    fn send_lectern(&mut self) {
        let Some(open) = self.inv.open.as_ref().filter(|o| o.kind == Kind::Lectern) else {
            return;
        };
        let Some(pos) = open.pos.or(open.pending_open.map(|(p, _)| p)) else {
            return;
        };
        let book = open
            .slots
            .first()
            .map(|s| s.stack.clone())
            .unwrap_or_default();
        let page = open.state;
        let [x, y, z] = pos;
        let mut c = crate::nbt::Compound::default();
        c.string("id", "Lectern");
        let content = crate::items::book_content(&book);
        let name = self.version.palette.items().name(book.item);
        match (content, name) {
            (Some((pages, written)), Some((name, meta))) if !book.is_empty() => {
                let total = i32::try_from(pages.len()).unwrap_or(0);
                let mut tag = crate::nbt::Compound::default();
                let pages: Vec<crate::nbt::Compound> = pages
                    .iter()
                    .map(|p| {
                        let mut c = crate::nbt::Compound::default();
                        c.string("text", p).string("photoname", "");
                        c
                    })
                    .collect();
                tag.compounds("pages", &pages);
                if let Some((title, author, generation)) = &written {
                    tag.string("title", title)
                        .string("author", author)
                        .int("generation", *generation)
                        .string("xuid", "");
                }
                let mut item = crate::nbt::Compound::default();
                item.string("Name", name)
                    .byte("Count", 1)
                    .short("Damage", i16::try_from(meta).unwrap_or(0))
                    .byte("WasPickedUp", 0)
                    .compound("tag", &tag);
                // Bedrock pages come in spreads of two.
                c.byte("hasBook", 1)
                    .int("page", page.max(0) / 2)
                    .int("totalPages", total)
                    .compound("book", &item);
            }
            _ => {
                c.byte("hasBook", 0);
            }
        }
        c.int("x", x).int("y", y).int("z", z).byte("isMovable", 1);
        self.send(id::BLOCK_ACTOR_DATA, &bp::block_actor_data(pos, &c.root()));
        let pending = self.inv.open.as_mut().and_then(|o| {
            let (window, kind) = (o.window, o.container_type);
            o.pending_open.take().map(|(p, e)| (window, kind, p, e))
        });
        if let Some((window, kind, pos, entity)) = pending {
            self.send(
                id::CONTAINER_OPEN,
                &bp::container_open(window, kind, pos, entity),
            );
        }
    }

    /// `lectern_update`: the reader turned to another spread of the book.
    pub(super) fn lectern_update(&mut self, body: &[u8]) -> crate::io::Result<()> {
        let (page, pos) = bp::read_lectern_update(body)?;
        if let Some(open) = self
            .inv
            .open
            .as_ref()
            .filter(|o| o.kind == Kind::Lectern && o.pos == Some(pos))
        {
            // `LecternMenu.clickMenuButton`: 100 + page sets the page.
            let container_id = open.container;
            self.java_send(&psb::ContainerButtonClick {
                container_id,
                button_id: 100i32.saturating_add(i32::from(page).saturating_mul(2)),
            });
        }
        Ok(())
    }

    /// `player_toggle_crafter_slot_request`: a crafter slot locked or unlocked.
    pub(super) fn toggle_crafter_slot(&mut self, body: &[u8]) -> crate::io::Result<()> {
        let (pos, slot, disabled) = bp::read_toggle_crafter_slot(body)?;
        if slot < 9
            && let Some(open) = self
                .inv
                .open
                .as_ref()
                .filter(|o| o.kind == Kind::Crafter && o.pos.is_none_or(|p| p == pos))
        {
            let container_id = open.container;
            self.java_send(&psb::ContainerSlotStateChanged {
                slot_id: i32::from(slot),
                container_id,
                new_state: !disabled,
            });
        }
        Ok(())
    }

    /// `player_enchant_options`: the enchanting table's options with a cost and a clue.
    fn send_enchant_options(&mut self) {
        let Some(open) = self
            .inv
            .open
            .as_ref()
            .filter(|o| o.kind == Kind::Enchanting)
        else {
            return;
        };
        let [costs, clues, levels] = open.enchant;
        let options: Vec<bp::EnchantOption> = (0..3usize)
            .filter_map(|i| {
                let cost = u8::try_from(*costs.get(i)?).ok().filter(|&c| c > 0)?;
                Some(bp::EnchantOption {
                    cost,
                    enchantment: self.enchantments.bedrock(*clues.get(i)?).unwrap_or(0),
                    level: u8::try_from(*levels.get(i)?).unwrap_or(1).max(1),
                    net_id: ENCHANT_NET_ID.saturating_add(u32::try_from(i).ok()?),
                })
            })
            .collect();
        self.send(
            id::PLAYER_ENCHANT_OPTIONS,
            &bp::player_enchant_options(&options),
        );
    }

    /// Shows a menu without block or entity as a form: one button per filled slot.
    fn show_menu_form(&mut self) {
        let Some(open) = self.inv.open.as_ref().filter(|o| o.form) else {
            return;
        };
        let buttons = open
            .slots
            .iter()
            .filter(|s| !s.stack.is_empty())
            .map(|s| Button {
                text: stack_label(&s.stack),
                image: None,
            })
            .collect();
        let form = Form::Simple {
            title: open.title.clone(),
            content: String::new(),
            buttons,
        };
        self.send(
            id::MODAL_FORM_REQUEST,
            &bp::modal_form_request(MENU_FORM, &form.to_json()),
        );
    }

    /// The answer to a menu form: a click on the slot of the button, or the menu closed.
    pub(super) fn menu_form_answer(&mut self, data: Option<&str>) {
        let Some(open) = self.inv.open.as_ref().filter(|o| o.form) else {
            return;
        };
        let choice = data
            .and_then(|d| d.trim().parse::<usize>().ok())
            .and_then(|b| {
                open.slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| !s.stack.is_empty())
                    .nth(b)
                    .map(|(i, _)| i)
            });
        let (container, state_id) = (open.container, open.state_id);
        let Some(slot) = choice.and_then(|i| i16::try_from(i).ok()) else {
            self.inv.open = None;
            self.java_send(&psb::ContainerClose {
                container_id: container,
            });
            return;
        };
        self.java_send(&psb::ContainerClick {
            container_id: container,
            state_id,
            slot,
            button: 0,
            input: ContainerInput::Pickup,
            changed: Vec::new(),
            carried: HashedStack::default(),
        });
        self.show_menu_form();
    }

    /// `block_event`: chest and shulker box lids, pistons.
    pub(super) fn java_block_event(&mut self, r: &mut JReader<'_>) {
        let (Ok(pos), Ok(action), Ok(param), Ok(block)) = (
            <vanillite_protocol::types::BlockPos as vanillite_protocol::codec::Decode>::decode(r),
            r.u8(),
            r.u8(),
            r.varint(),
        ) else {
            return;
        };
        let name = u16::try_from(block)
            .ok()
            .and_then(|b| vanillite_data::registry::BLOCK.name(b))
            .unwrap_or("");
        if matches!(name, "minecraft:piston" | "minecraft:sticky_piston") {
            self.piston_event(
                [pos.x, pos.y, pos.z],
                action,
                param,
                name == "minecraft:sticky_piston",
            );
        } else if action == 1 && (name.ends_with("chest") || name.ends_with("shulker_box")) {
            self.send(
                id::BLOCK_EVENT,
                &bp::block_event([pos.x, pos.y, pos.z], 1, i32::from(param > 0)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_slots_round_trip() {
        for (mount, java_slots) in [
            (Mount::Horse, vec![0, 1]),
            (Mount::Chested, vec![0, 2, 3, 16]),
            (Mount::Llama, vec![1, 2, 16]),
        ] {
            for j in java_slots {
                let b = mount.bedrock(j);
                assert_eq!(b.map(|b| mount.java(b)), Some(j), "{mount:?} {j}");
            }
        }
        assert_eq!(Mount::Chested.bedrock(1), None);
        assert_eq!(Mount::Llama.bedrock(0), None);
        assert_eq!(Mount::Llama.bedrock(1), Some(0));
        assert!(has_inventory_screen("minecraft:trader_llama"));
        assert!(has_inventory_screen("minecraft:oak_chest_boat"));
        assert!(!has_inventory_screen("minecraft:pig"));
    }
}
