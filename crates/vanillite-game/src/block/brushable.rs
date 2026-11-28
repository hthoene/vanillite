//! Suspicious sand and gravel (`BrushableBlock`, `BrushableBlockEntity`): brushing ten times
//! uncovers the item of their archaeology loot table.

use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockState, ids};
use vanillite_nbt::{Compound, Tag};
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::StateExt;

use crate::item::ItemStack;
use crate::level::{BlockEntity, Level};

/// `BRUSH_COOLDOWN_TICKS`.
const COOLDOWN: i64 = 10;
/// `BRUSH_RESET_TICKS`.
const RESET: i64 = 40;
/// `REQUIRED_BRUSHES_TO_BREAK`.
const REQUIRED: i32 = 10;

/// The data of a `BrushableBlockEntity`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Brushable {
    /// `brushCount`.
    pub brush_count: i32,
    /// `brushCountResetsAtTick`.
    pub resets_at: i64,
    /// `coolDownEndsAtTick`.
    pub cooldown_ends_at: i64,
    /// The uncovered item.
    pub item: ItemStack,
    /// `hit_direction`.
    pub hit_direction: Option<Direction>,
    /// `LootTable` not rolled yet.
    pub loot_table: Option<String>,
    /// `LootTableSeed`.
    pub loot_seed: i64,
}

impl Brushable {
    /// `loadAdditional`.
    pub fn load(&mut self, nbt: &Compound) {
        self.loot_table = match nbt.get("LootTable") {
            Some(Tag::String(t)) => Some(t.clone()),
            _ => None,
        };
        self.loot_seed = nbt.get("LootTableSeed").and_then(Tag::as_i64).unwrap_or(0);
        self.item = match (&self.loot_table, nbt.get("item")) {
            (None, Some(Tag::Compound(c))) => crate::inventory::stack_from_nbt(c),
            _ => ItemStack::EMPTY,
        };
        // `Direction.LEGACY_ID_CODEC`: a byte, read from any numeric tag.
        self.hit_direction = nbt
            .get("hit_direction")
            .and_then(Tag::as_i64)
            .map(|v| direction_from_3d(i32::from(v.to_le_bytes()[0].cast_signed())));
    }

    /// `saveAdditional`.
    pub fn save(&self, out: &mut Compound) {
        if let Some(table) = &self.loot_table {
            out.insert("LootTable", Tag::String(table.clone()));
            if self.loot_seed != 0 {
                out.insert("LootTableSeed", Tag::Long(self.loot_seed));
            }
        } else if !self.item.is_empty() {
            out.insert("item", crate::inventory::stack_to_nbt(&self.item));
        }
    }

    /// `getUpdateTag`: the hit direction and the item.
    pub fn update_tag(&self) -> Compound {
        let mut out = Compound::new();
        if let Some(d) = self.hit_direction {
            out.insert(
                "hit_direction",
                Tag::Byte(i8::try_from(direction_to_3d(d)).unwrap_or(0)),
            );
        }
        if !self.item.is_empty() {
            out.insert("item", crate::inventory::stack_to_nbt(&self.item));
        }
        out
    }

    /// `getCompletionState`.
    fn completion(&self) -> i32 {
        match self.brush_count {
            0 => 0,
            1..=2 => 1,
            3..=5 => 2,
            _ => 3,
        }
    }
}

fn direction_from_3d(v: i32) -> Direction {
    match v.rem_euclid(6) {
        0 => Direction::Down,
        1 => Direction::Up,
        2 => Direction::North,
        3 => Direction::South,
        4 => Direction::West,
        _ => Direction::East,
    }
}

fn direction_to_3d(d: Direction) -> i32 {
    match d {
        Direction::Down => 0,
        Direction::Up => 1,
        Direction::North => 2,
        Direction::South => 3,
        Direction::West => 4,
        Direction::East => 5,
    }
}

/// `getTurnsInto`.
fn turns_into(state: BlockState) -> BlockState {
    if state.is(ids::SUSPICIOUS_GRAVEL) {
        ids::GRAVEL.default_state()
    } else if state.is(ids::SUSPICIOUS_SAND) {
        ids::SAND.default_state()
    } else {
        BlockState::AIR
    }
}

/// `getBrushSound` of the block (`item.brush.brushing.*`).
pub fn brush_sound(state: BlockState) -> &'static str {
    if state.is(ids::SUSPICIOUS_GRAVEL) {
        "item.brush.brushing.gravel"
    } else if state.is(ids::SUSPICIOUS_SAND) {
        "item.brush.brushing.sand"
    } else {
        "item.brush.brushing.generic"
    }
}

/// `BrushItem.onUseTick` once the brushing stroke lands on the brushable block at `pos`
/// (hit face `face`): `BrushableBlockEntity.brush`, and one point of durability off the
/// brush when the brushing completed. Returns whether it completed.
pub fn brush(
    level: &mut Level,
    game_time: i64,
    player: EntityId,
    pos: BlockPos,
    face: Direction,
    stack: &mut ItemStack,
    slot: crate::item::EquipmentSlot,
) -> bool {
    // Player luck is 0 until the `luck` attribute exists.
    let done = brush_block(level, game_time, pos, Some(player), 0.0, face);
    if done {
        super::hurt_item(level, stack, 1, player, slot);
    }
    done
}

/// `BrushableBlockEntity.brush(gameTime, level, user, direction, brush)`: whether the
/// brushing completed.
pub fn brush_block(
    level: &mut Level,
    time: i64,
    pos: BlockPos,
    user: Option<EntityId>,
    luck: f32,
    direction: Direction,
) -> bool {
    let Some(BlockEntity::Brushable(b)) = level.block_entity_mut(pos) else {
        return false;
    };
    if b.hit_direction.is_none() {
        b.hit_direction = Some(direction);
    }
    b.resets_at = time + RESET;
    if time < b.cooldown_ends_at {
        return false;
    }
    b.cooldown_ends_at = time + COOLDOWN;
    unpack_loot(level, pos, user, luck);
    let Some(BlockEntity::Brushable(b)) = level.block_entity_mut(pos) else {
        return false;
    };
    let previous = b.completion();
    b.brush_count += 1;
    if b.brush_count >= REQUIRED {
        brushing_completed(level, pos, user, luck);
        return true;
    }
    let completion = b.completion();
    let state = level.block(pos);
    level.schedule_block_tick(pos, state.block(), 2);
    if previous != completion {
        level.set_block_and_update(pos, state.set_int("dusted", completion));
    }
    level.block_entities.touch(pos.chunk());
    false
}

/// `unpackLootTable`: the archaeology table gives the item.
fn unpack_loot(level: &mut Level, pos: BlockPos, user: Option<EntityId>, luck: f32) {
    let Some(BlockEntity::Brushable(b)) = level.block_entity_mut(pos) else {
        return;
    };
    let Some(table) = b.loot_table.take() else {
        return;
    };
    let seed = b.loot_seed;
    let origin = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    super::chest_loot::generated_loot(level, user, &table);
    let loot = super::chest_loot::roll(level, &table, seed, origin, luck, user);
    if loot.len() > 1 {
        log::warn!(
            "Expected max 1 loot from loot table {table}, but got {}",
            loot.len()
        );
    }
    if let Some(BlockEntity::Brushable(b)) = level.block_entity_mut(pos) {
        b.item = loot.into_iter().next().unwrap_or_default();
    }
    level.block_entities.touch(pos.chunk());
}

/// `brushingCompleted`.
fn brushing_completed(level: &mut Level, pos: BlockPos, user: Option<EntityId>, luck: f32) {
    drop_content(level, pos, user, luck);
    let state = level.block(pos);
    level.level_event(None, 3008, pos, i32::from(state.0));
    level.set_block_and_update(pos, turns_into(state));
}

/// `dropContent`.
fn drop_content(level: &mut Level, pos: BlockPos, user: Option<EntityId>, luck: f32) {
    unpack_loot(level, pos, user, luck);
    let Some(BlockEntity::Brushable(b)) = level.block_entity_mut(pos) else {
        return;
    };
    if b.item.is_empty() {
        return;
    }
    let mut item = std::mem::take(&mut b.item);
    let direction = b.hit_direction.unwrap_or(Direction::Up);
    // `EntityTypes.ITEM`: 0.25 × 0.25.
    let (width, height) = (0.25f64, 0.25f64);
    let center_range = 1.0 - width;
    let half = width / 2.0;
    let drop = pos.relative(direction);
    let at = Vec3::new(
        f64::from(drop.x) + 0.5 * center_range + half,
        f64::from(drop.y) + 0.5 + height / 2.0,
        f64::from(drop.z) + 0.5 * center_range + half,
    );
    let n = vanillite_base::random::RandomSource::next_int_bounded(level.random(), 21) + 10;
    let part = item.split(n);
    // `new ItemEntity(level, x, y, z, stack)` draws a velocity from the item's own random,
    // replaced by zero.
    super::drops::spawn_item(level, at, part, Vec3::default(), 10, None);
}

/// `checkReset` (from the block's scheduled tick).
pub fn check_reset(level: &mut Level, pos: BlockPos) {
    let time = level.game_time();
    let state = level.block(pos);
    let Some(BlockEntity::Brushable(b)) = level.block_entity_mut(pos) else {
        return;
    };
    let mut new_state = None;
    if b.brush_count != 0 && time >= b.resets_at {
        let previous = b.completion();
        b.brush_count = (b.brush_count - 2).max(0);
        let completion = b.completion();
        if previous != completion {
            new_state = Some(state.set_int("dusted", completion));
        }
        b.resets_at = time + 4;
    }
    let reschedule = if b.brush_count == 0 {
        b.hit_direction = None;
        b.resets_at = 0;
        b.cooldown_ends_at = 0;
        false
    } else {
        true
    };
    if let Some(s) = new_state {
        level.set_block_and_update(pos, s);
    }
    if reschedule {
        level.schedule_block_tick(pos, state.block(), 2);
    }
}

/// Whether the block is brushable.
pub fn is_brushable(state: BlockState) -> bool {
    state.is(ids::SUSPICIOUS_SAND) || state.is(ids::SUSPICIOUS_GRAVEL)
}
