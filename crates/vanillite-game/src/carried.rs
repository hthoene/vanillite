//! Items that tick in a player's inventory (`Inventory.tick` → `Item.inventoryTick`): filled
//! maps (`MapItem.inventoryTick`) and lodestone compasses (`CompassItem.inventoryTick`), then
//! the map updates for the client (`ServerPlayer.synchronizeSpecialItemUpdates`).

use vanillite_base::{BlockPos, Vec3};
use vanillite_protocol::codec::Reader;
use vanillite_protocol::components::ids;

use crate::item::{EquipmentSlot, ItemStack};
use crate::level::Level;
use crate::map::{self, Carrier};
use crate::player::PlayerData;

/// The armour slots (`EquipmentSlot` values that are no hands).
const ARMOR: [EquipmentSlot; 4] = [
    EquipmentSlot::Feet,
    EquipmentSlot::Legs,
    EquipmentSlot::Chest,
    EquipmentSlot::Head,
];

/// The ticking player.
#[derive(Clone, Copy, Debug)]
pub struct Owner<'a> {
    pub uuid: u128,
    pub pos: Vec3,
    pub y_rot: f32,
    /// The player's dimension.
    pub dimension: &'a str,
    /// `dimensionType().hasCeiling()`.
    pub has_ceiling: bool,
}

/// `hasMapInvisibilityItemEquipped`.
pub(crate) fn map_invisible(inventory: &crate::inventory::PlayerInventory) -> bool {
    let Some(data) = crate::player::play::game_data() else {
        return false;
    };
    ARMOR.iter().any(|&slot| {
        let s = inventory.equipment(slot);
        !s.is_empty() && data.item_in_tag(s.item, "minecraft:map_invisibility_equipment")
    })
}

/// `LodestoneTracker.tick`: a tracked lodestone in this dimension that is gone (outside the
/// world, or another block in a loaded chunk) leaves an empty target.
fn tick_lodestone(level: &Level, dimension: &str, stack: &mut ItemStack) {
    let Some(value) = stack.component(ids::LODESTONE_TRACKER) else {
        return;
    };
    let mut r = Reader::new(value);
    let target = match r.u8() {
        Ok(0) => None,
        Ok(_) => {
            let (Ok(dim), Ok(pos)) = (r.string(32767).map(str::to_owned), r.i64()) else {
                return;
            };
            Some((dim, BlockPos::from_long(pos)))
        }
        Err(_) => return,
    };
    let tracked = r.u8().is_ok_and(|b| b != 0);
    let Some((dim, pos)) = target else {
        return;
    };
    if !tracked || dim != dimension {
        return;
    }
    let in_world = pos.y >= level.min_y()
        && pos.y < level.max_y()
        && (-30_000_000..30_000_000).contains(&pos.x)
        && (-30_000_000..30_000_000).contains(&pos.z);
    // Without a point-of-interest store, a lodestone is checked where its chunk is loaded.
    let gone = !in_world
        || (level.is_loaded(pos)
            && level.block(pos).block() != vanillite_data::block::ids::LODESTONE);
    if gone {
        stack.set_component(ids::LODESTONE_TRACKER, Box::new([0, 1]));
    }
}

/// `CompassItem.useOn` on a lodestone: the compass (or a copy of one from a stack) points to
/// it. The new compass to give the player, if one was split off.
pub fn bind_lodestone(
    stack: &mut ItemStack,
    dimension: &str,
    pos: BlockPos,
    infinite: bool,
) -> Option<ItemStack> {
    use vanillite_protocol::codec::Encode;
    let mut value = vec![1];
    dimension.encode(&mut value);
    pos.as_long().encode(&mut value);
    value.push(1);
    if !infinite && stack.count == 1 {
        stack.set_component(ids::LODESTONE_TRACKER, value.into_boxed_slice());
        return None;
    }
    // `transmuteCopy(COMPASS, 1)`.
    let mut compass = stack.with_count(1);
    compass.item = vanillite_data::item::ids::COMPASS.0;
    if !infinite {
        stack.shrink(1);
    }
    compass.set_component(ids::LODESTONE_TRACKER, value.into_boxed_slice());
    Some(compass)
}

/// One tick of the player's ticking items and its map updates.
pub fn tick(level: &Level, owner: Owner<'_>, player: &mut PlayerData) {
    // The carrier is built at the first map (no allocation for players without maps).
    let mut carrier: Option<Carrier> = None;
    let game_time = level.game_time();
    let mut carried: Vec<i32> = Vec::new();
    // `Inventory.tick`: the items (the selected one in the main hand), then the equipment.
    let selected = player.inventory.selected;
    let order = (0..36)
        .map(|i| (i, i == selected))
        .chain(std::iter::once((crate::inventory::OFFHAND, true)))
        .chain((36..40).map(|i| (i, false)));
    for (i, hand) in order {
        let Some(stack) = player.inventory.get_mut(i) else {
            continue;
        };
        if stack.is_empty() {
            continue;
        }
        // `CompassItem.inventoryTick`: only compasses tick their tracker.
        if stack.item == vanillite_data::item::ids::COMPASS.0
            && stack.component(ids::LODESTONE_TRACKER).is_some()
        {
            tick_lodestone(level, owner.dimension, stack);
        }
        if !map::is_map_item(stack.item) {
            continue;
        }
        let Some(id) = map::map_id(stack) else {
            continue;
        };
        let Some(maps) = level.maps() else {
            continue;
        };
        let statics = map::static_decorations(stack);
        let carrier = carrier.get_or_insert_with(|| Carrier {
            uuid: owner.uuid,
            name: player.profile.name.clone(),
            dimension: owner.dimension.to_owned(),
            x: owner.pos.x,
            z: owner.pos.z,
            y_rot: owner.y_rot,
            has_map: true,
            invisible: map_invisible(&player.inventory),
        });
        let found = maps.with(id, |data| {
            data.tick_carried_by(carrier, &statics, game_time);
            if !data.locked && hand {
                map::update(data, level, owner.dimension, carrier, owner.has_ceiling);
            }
        });
        if found.is_some() && !carried.contains(&id) {
            carried.push(id);
        }
    }
    if carried.is_empty() && player.survival.carried_maps.is_empty() {
        return;
    }
    let Some(maps) = level.maps() else {
        return;
    };
    for id in &player.survival.carried_maps {
        if !carried.contains(id) {
            maps.with(*id, |d| d.carrier_gone(owner.uuid));
        }
    }
    player.survival.carried_maps = carried;
    // `synchronizeSpecialItemUpdates` for every slot.
    for i in 0..=crate::inventory::OFFHAND {
        let stack = player.inventory.get(i);
        if stack.is_empty() || !map::is_map_item(stack.item) {
            continue;
        }
        let Some(id) = map::map_id(stack) else {
            continue;
        };
        if let Some(Some(packet)) = maps.with(id, |d| d.next_update_packet(owner.uuid, id)) {
            player.connection.sink.send(&packet);
        }
    }
}
