//! Container loot (`RandomizableContainer`): the loot table of a chest, barrel, shulker box,
//! dispenser, dropper, hopper, crafter or decorated pot is rolled the first time the container
//! is opened, read or broken (`unpackLootTable`); brushable blocks roll their archaeology
//! table when brushed. Chest minecarts, hopper minecarts and chest boats (`ContainerEntity`)
//! roll theirs the same way ([`unpack_entity`]).
//!
//! The roll uses `RandomSource.create(seed)` for a non-zero `LootTableSeed`, else the level
//! random (Vanilla takes the table's random sequence there; there are no random sequences
//! yet). The opening player's luck is 0 (no `luck` attribute yet).

use vanillite_base::random::LegacyRandom;
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::{Compound, Tag};
use vanillite_world::entity::EntityId;

use crate::item::ItemStack;
use crate::level::{BlockEntity, Level};
use crate::loot::{LootContext, LootRandom};

/// `tryLoadLootTable`: `LootTable` and `LootTableSeed`.
pub fn load(nbt: &Compound) -> (Option<String>, i64) {
    match nbt.get("LootTable") {
        Some(Tag::String(table)) => (
            Some(table.clone()),
            nbt.get("LootTableSeed").and_then(Tag::as_i64).unwrap_or(0),
        ),
        _ => (None, 0),
    }
}

/// `trySaveLootTable`: whether there is a table (then the stacks are not saved).
pub fn save(table: Option<&str>, seed: i64, out: &mut Compound) -> bool {
    let Some(table) = table else {
        return false;
    };
    out.insert("LootTable", Tag::String(table.to_owned()));
    if seed != 0 {
        out.insert("LootTableSeed", Tag::Long(seed));
    }
    true
}

/// A `container_loot` value (`SeededContainerLoot`; no stream codec, so one network NBT tag).
pub fn component(table: &str, seed: i64) -> Box<[u8]> {
    let mut c = Compound::new();
    c.insert("loot_table", Tag::String(table.to_owned()));
    if seed != 0 {
        c.insert("seed", Tag::Long(seed));
    }
    let mut out = Vec::new();
    // Writing a compound to a vector cannot fail.
    let _ = vanillite_nbt::write::write_network_compound(&c, &mut out);
    out.into_boxed_slice()
}

/// Reads a `container_loot` value: table and seed.
pub fn read_component(bytes: &[u8]) -> Option<(String, i64)> {
    let mut input = bytes;
    let tag = vanillite_nbt::read::read_network(&mut input, &mut vanillite_nbt::Budget::network())
        .ok()??;
    let c = tag.as_compound()?;
    let table = c.get("loot_table")?.as_str()?.to_owned();
    let seed = c.get("seed").and_then(Tag::as_i64).unwrap_or(0);
    Some((table, seed))
}

/// The loot table parts of a block entity that has them: table, seed and stacks.
fn loot_parts(entity: &mut BlockEntity) -> Option<(&mut Option<String>, i64, &mut [ItemStack])> {
    match entity {
        BlockEntity::Container(c) => Some((&mut c.loot_table, c.loot_seed, &mut c.items[..])),
        BlockEntity::Hopper(h) => Some((&mut h.loot_table, h.loot_seed, &mut h.items[..])),
        BlockEntity::Crafter(c) => Some((&mut c.loot_table, c.loot_seed, &mut c.items[..])),
        BlockEntity::DecoratedPot(p) => Some((
            &mut p.loot_table,
            p.loot_seed,
            std::slice::from_mut(&mut p.item),
        )),
        _ => None,
    }
}

/// Whether the block entity at `pos` still has a loot table to roll.
pub fn has_loot(level: &Level, pos: BlockPos) -> bool {
    match level.block_entity(pos) {
        Some(BlockEntity::Container(c)) => c.loot_table.is_some(),
        Some(BlockEntity::Hopper(h)) => h.loot_table.is_some(),
        Some(BlockEntity::Crafter(c)) => c.loot_table.is_some(),
        Some(BlockEntity::DecoratedPot(p)) => p.loot_table.is_some(),
        _ => false,
    }
}

/// `RandomizableContainer.unpackLootTable(player)`: fills the container at `pos` from its
/// loot table (once; the table is cleared first). `player` is the opening or breaking player
/// (`this_entity`); `None` for every other access.
pub fn unpack(level: &mut Level, pos: BlockPos, player: Option<EntityId>) {
    let Some((table, seed, items)) = level.block_entity_mut(pos).and_then(|e| {
        let (table, seed, items) = loot_parts(e)?;
        Some((table.take()?, seed, items.to_vec()))
    }) else {
        return;
    };
    let mut items = items;
    generated_loot(level, player, &table);
    let origin = super::furnace::center(pos);
    let luck = player_luck(level, player);
    fill(level, &table, seed, origin, luck, player, &mut items);
    if let Some((_, _, slots)) = level.block_entity_mut(pos).and_then(loot_parts) {
        for (slot, stack) in slots.iter_mut().zip(items) {
            *slot = stack;
        }
    }
    level.block_entities.touch(pos.chunk());
}

/// `ContainerEntity.unpackChestVehicleLootTable(player)`: fills the chest minecart, hopper
/// minecart or chest boat `id` from its loot table (once; the table is cleared first).
pub fn unpack_entity(level: &mut Level, id: EntityId, player: Option<EntityId>) {
    let Some((table, seed)) = level.entities.container_loot_mut(id).and_then(Option::take) else {
        return;
    };
    let Some(origin) = level.entities.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let Some(mut items) = level
        .entities
        .container_items(id)
        .map(<[ItemStack]>::to_vec)
    else {
        return;
    };
    generated_loot(level, player, &table);
    let luck = player_luck(level, player);
    fill(level, &table, seed, origin, luck, player, &mut items);
    if let Some(slots) = level.entities.container_items_mut(id) {
        for (slot, stack) in slots.iter_mut().zip(items) {
            *slot = stack;
        }
    }
}

/// Whether the container entity `id` still has a loot table to roll.
pub fn entity_has_loot(level: &mut Level, id: EntityId) -> bool {
    level
        .entities
        .container_loot_mut(id)
        .is_some_and(|l| l.is_some())
}

/// `CriteriaTriggers.GENERATE_LOOT` when a player unpacks the loot table `table`.
pub fn generated_loot(level: &mut Level, player: Option<EntityId>, table: &str) {
    let is_player = |p: &EntityId| {
        level
            .entities
            .store
            .get(*p)
            .is_some_and(|e| e.kind() == vanillite_data::entity::ids::PLAYER)
    };
    if let Some(player) = player.filter(is_player) {
        let event = crate::advancements::trigger::Event::GenerateLoot(table.to_owned());
        level.trigger(player, event);
    }
}

/// `withLuck(player.getLuck())`: the player's `luck` attribute (Luck and Bad Luck effects).
fn player_luck(level: &Level, player: Option<EntityId>) -> f32 {
    player
        .and_then(|p| level.entities.player(p))
        .map_or(0.0, |p| {
            #[allow(clippy::cast_possible_truncation)]
            let luck = p
                .survival
                .effects
                .attribute(crate::effect::attribute::Attribute::LUCK, 0.0)
                as f32;
            luck
        })
}

/// `LootTable.fill(container, params, seed)` of the table `table` for a container at `origin`.
pub fn fill(
    level: &mut Level,
    table: &str,
    seed: i64,
    origin: Vec3,
    luck: f32,
    player: Option<EntityId>,
    items: &mut [ItemStack],
) {
    with_context(
        level,
        table,
        seed,
        origin,
        luck,
        player,
        |data, ctx, rng| {
            data.loot.fill(table, ctx, rng, items);
        },
    );
}

/// `LootTable.getRandomItems(params, seed)` of the table `table` for a container at `origin`
/// (brushable blocks).
pub fn roll(
    level: &mut Level,
    table: &str,
    seed: i64,
    origin: Vec3,
    luck: f32,
    player: Option<EntityId>,
) -> Vec<ItemStack> {
    let mut out = Vec::new();
    with_context(
        level,
        table,
        seed,
        origin,
        luck,
        player,
        |data, ctx, rng| {
            data.loot.roll(table, ctx, rng, &mut out);
        },
    );
    out
}

/// Runs `f` with the loot context of a container at `origin` and the roll's random
/// (`withOptionalRandomSeed(seed)`, else the table's random sequence, else the level random).
fn with_context(
    level: &mut Level,
    table: &str,
    seed: i64,
    origin: Vec3,
    luck: f32,
    player: Option<EntityId>,
    f: impl FnOnce(&crate::player::play::GameData, &LootContext<'_>, &mut dyn LootRandom),
) {
    let Some(data) = crate::player::play::game_data() else {
        return;
    };
    // `BlockPos.containing(origin)`.
    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
    let at = BlockPos::new(
        origin.x.floor() as i32,
        origin.y.floor() as i32,
        origin.z.floor() as i32,
    );
    let biome: Option<String> = level
        .climate()
        .map(|c| c.biomes.get(level.biome(at)).name.clone());
    let biome_matches = |entry: &str| biome.as_deref() == Some(entry);
    let item_in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let holder_set = |registry: &str, entry: &str| data.holder_set(registry, entry);
    let generation = level.generation.clone();
    let maps = level.maps_shared();
    let refs = std::sync::Arc::clone(level.structure_refs());
    let (min_y, height) = (level.min_y(), level.max_y() - level.min_y() + 1);
    let sea_level = generation.as_ref().map_or(63, |g| g.terrain().sea_level());
    let explore = |request: &crate::loot::ExplorationMap, origin: Vec3| {
        crate::map::exploration_map(
            generation.as_ref()?,
            maps.as_deref()?,
            min_y,
            height,
            sea_level,
            request,
            origin,
            &refs,
        )
    };
    let ctx = LootContext {
        origin: Some(origin),
        this_entity: player,
        luck,
        item_in_tag: &item_in_tag,
        enchanting: Some(&data.enchanting),
        holder_set: &holder_set,
        biome: Some(&biome_matches),
        exploration_map: Some(&explore),
        ..LootContext::empty()
    };
    if seed != 0 {
        let mut rng = LegacyRandom::new(seed);
        f(&data, &ctx, &mut rng);
    } else if let Some(key) = data.loot.random_sequence(table) {
        level.with_random_sequence(key, |r| f(&data, &ctx, r));
    } else {
        f(&data, &ctx, level.random());
    }
}
