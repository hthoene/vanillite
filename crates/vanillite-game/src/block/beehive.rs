//! Beehives and bee nests (`BeehiveBlock`, `BeehiveBlockEntity`): up to three bees live in a
//! hive, leave it after their time inside (bees with nectar raise the honey level), stay in at
//! night and in rain, and burst out angry when the hive is harvested without smoke, broken,
//! blown up or set on fire.
//!
//! A bee entering its hive is saved into the block entity (without the tags Vanilla drops,
//! `IGNORED_BEE_TAGS`) and leaves the world; leaving creates a new bee from that data in front
//! of the hive.

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Direction, Vec3};
use vanillite_data::block::{BlockId, BlockState, ids};
use vanillite_data::entity::{EntityTypeId, ids as entity_ids};
use vanillite_data::item::ids as items;
use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::components::ids as components;
use vanillite_world::entity::{EntityFlags, EntityId};
use vanillite_worldgen::behaviour::StateExt;

use super::{BlockBehaviour, Interaction, ItemInteraction, UseContext, block_tags, sound_id};
use crate::item::ItemStack;
use crate::level::game_event::{GameEventContext, events};
use crate::level::{BlockEntity, Level, SOUND_BLOCKS};
use crate::mob::Kind;
use crate::mob::fam::wildlife::bee;

/// `MAX_OCCUPANTS`.
pub const MAX_OCCUPANTS: usize = 3;
/// `MIN_OCCUPATION_TICKS_NECTAR`.
const MIN_OCCUPATION_TICKS_NECTAR: i32 = 2400;
/// `MIN_OCCUPATION_TICKS_NECTARLESS`.
const MIN_OCCUPATION_TICKS_NECTARLESS: i32 = 600;
/// `HONEY_LEVEL`'s maximum.
const MAX_HONEY_LEVELS: i32 = 5;

/// `IGNORED_BEE_TAGS`: what a bee does not keep while it is in a hive.
const IGNORED_BEE_TAGS: [&str; 25] = [
    "Air",
    "drop_chances",
    "equipment",
    "Brain",
    "CanPickUpLoot",
    "DeathTime",
    "fall_distance",
    "FallFlying",
    "Fire",
    "HurtTime",
    "LeftHanded",
    "Motion",
    "NoGravity",
    "OnGround",
    "PortalCooldown",
    "Pos",
    "Rotation",
    "sleeping_pos",
    "CannotEnterHiveTicks",
    "TicksSincePollination",
    "CropsGrownSincePollination",
    "hive_pos",
    "Passengers",
    "leash",
    "UUID",
];

/// `BeehiveBlockEntity.Occupant` with its live `ticksInHive` (`BeeData`).
#[derive(Clone, Debug, PartialEq)]
pub struct Occupant {
    /// `entity_data`: the saved entity, with its `id`.
    pub entity_data: Compound,
    pub ticks_in_hive: i32,
    pub min_ticks_in_hive: i32,
}

impl Occupant {
    /// `BeeData.hasNectar`.
    fn has_nectar(&self) -> bool {
        matches!(self.entity_data.get("HasNectar"), Some(Tag::Byte(b)) if *b != 0)
    }

    fn load(c: &Compound) -> Option<Occupant> {
        let int = |k: &str| match c.get(k) {
            Some(Tag::Int(v)) => Some(*v),
            _ => None,
        };
        let entity_data = c.get("entity_data")?.as_compound()?.clone();
        entity_data.get("id")?.as_str()?;
        Some(Occupant {
            entity_data,
            ticks_in_hive: int("ticks_in_hive")?,
            min_ticks_in_hive: int("min_ticks_in_hive")?,
        })
    }

    fn save(&self) -> Compound {
        let mut c = Compound::new();
        c.insert("entity_data", Tag::Compound(self.entity_data.clone()));
        c.insert("ticks_in_hive", Tag::Int(self.ticks_in_hive));
        c.insert("min_ticks_in_hive", Tag::Int(self.min_ticks_in_hive));
        c
    }
}

/// The data of a `BeehiveBlockEntity`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Beehive {
    /// `stored`, in the order the bees came in.
    pub stored: Vec<Occupant>,
    /// `savedFlowerPos`.
    pub flower_pos: Option<BlockPos>,
}

impl Beehive {
    /// `isFull`.
    pub fn is_full(&self) -> bool {
        self.stored.len() >= MAX_OCCUPANTS
    }

    /// `loadAdditional`.
    pub fn load(&mut self, nbt: &Compound) {
        self.stored = match nbt.get("bees") {
            Some(Tag::List(list)) => list
                .0
                .iter()
                .filter_map(|t| t.as_compound().and_then(Occupant::load))
                .collect(),
            _ => Vec::new(),
        };
        self.flower_pos = match nbt.get("flower_pos") {
            Some(Tag::IntArray(v)) if v.len() == 3 => Some(BlockPos::new(v[0], v[1], v[2])),
            _ => None,
        };
    }

    /// `saveAdditional`.
    pub fn save(&self, out: &mut Compound) {
        let list: Vec<Tag> = self
            .stored
            .iter()
            .map(|o| Tag::Compound(o.save()))
            .collect();
        out.insert("bees", Tag::List(List(list)));
        if let Some(p) = self.flower_pos {
            out.insert("flower_pos", Tag::IntArray(vec![p.x, p.y, p.z]));
        }
    }

    /// `collectImplicitComponents`: the `minecraft:bees` value (always set).
    pub fn component(&self, id: u16) -> Option<Box<[u8]>> {
        (id == components::BEES).then(|| encode_bees(&self.stored))
    }

    /// `applyImplicitComponents`: the bees of a placed hive item.
    pub fn apply(&mut self, stack: &ItemStack) {
        self.stored.clear();
        if let Some(bees) = stack.component(components::BEES).and_then(decode_bees) {
            self.stored = bees;
        }
    }
}

/// The `minecraft:bees` wire value: per bee its entity type, entity data (without `id`), ticks
/// in the hive and the minimum ticks.
fn encode_bees(bees: &[Occupant]) -> Box<[u8]> {
    use vanillite_protocol::codec::{write_len, write_varint};
    let mut out = Vec::new();
    write_len(&mut out, bees.len());
    for b in bees {
        let kind = b
            .entity_data
            .get("id")
            .and_then(Tag::as_str)
            .and_then(EntityTypeId::by_name)
            .unwrap_or(entity_ids::BEE);
        write_varint(&mut out, i32::from(kind.0));
        let mut tag = b.entity_data.clone();
        tag.remove("id");
        let _ = vanillite_nbt::write::write_network_compound(&tag, &mut out);
        write_varint(&mut out, b.ticks_in_hive);
        write_varint(&mut out, b.min_ticks_in_hive);
    }
    out.into_boxed_slice()
}

/// Reads a `minecraft:bees` wire value.
fn decode_bees(mut data: &[u8]) -> Option<Vec<Occupant>> {
    fn varint(data: &mut &[u8]) -> Option<i32> {
        let mut value: u32 = 0;
        for i in 0..5 {
            let (&b, rest) = data.split_first()?;
            *data = rest;
            value |= u32::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Some(i32::from_ne_bytes(value.to_ne_bytes()));
            }
        }
        None
    }
    let count = usize::try_from(varint(&mut data)?).ok()?;
    let mut out = Vec::with_capacity(count.min(MAX_OCCUPANTS));
    for _ in 0..count {
        let kind = EntityTypeId(u16::try_from(varint(&mut data)?).ok()?);
        let mut budget = vanillite_nbt::Budget::network();
        let tag = vanillite_nbt::read::read_network(&mut data, &mut budget).ok()??;
        let Tag::Compound(mut entity_data) = tag else {
            return None;
        };
        entity_data.insert("id", Tag::String(kind.name().to_owned()));
        out.push(Occupant {
            entity_data,
            ticks_in_hive: varint(&mut data)?,
            min_ticks_in_hive: varint(&mut data)?,
        });
    }
    Some(out)
}

/// `BeehiveBlockEntity.BeeReleaseStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseStatus {
    HoneyDelivered,
    BeeReleased,
    Emergency,
}

/// A beehive or bee nest block (`instanceof BeehiveBlock`).
pub fn is_beehive(block: BlockId) -> bool {
    block == ids::BEEHIVE || block == ids::BEE_NEST
}

pub(super) fn fill(block: BlockId, b: &mut BlockBehaviour) {
    if is_beehive(block) {
        b.use_item_on = Some(use_item_on);
    }
}

/// `CampfireBlock.isLitCampfire`.
fn is_lit_campfire(state: BlockState) -> bool {
    block_tags().campfires.contains(state) && state.flag("lit")
}

/// `CampfireBlock.isSmokeyPos`: a lit campfire up to five blocks below, smoke stopped by the
/// first block whose collision reaches the virtual post in the middle of the column.
pub fn is_smokey_pos(level: &Level, pos: BlockPos) -> bool {
    // `SHAPE_VIRTUAL_POST`: `Block.column(4, 0, 16)`.
    const LO: f64 = 6.0 / 16.0;
    const HI: f64 = 10.0 / 16.0;
    for i in 1..=5 {
        let check = pos.offset(0, -i, 0);
        let state = level.block(check);
        if is_lit_campfire(state) {
            return true;
        }
        let blocked = state
            .collision_shape()
            .iter()
            .any(|b| b[0] < HI && b[3] > LO && b[1] < 1.0 && b[4] > 0.0 && b[2] < HI && b[5] > LO);
        if blocked {
            return is_lit_campfire(level.block(check.offset(0, -1, 0)));
        }
    }
    false
}

/// `isFireNearby`: fire in the 3×3×3 cube around the hive.
pub fn is_fire_nearby(level: &Level, pos: BlockPos) -> bool {
    for dy in -1..=1 {
        for dz in -1..=1 {
            for dx in -1..=1 {
                if level.block(pos.offset(dx, dy, dz)).is(ids::FIRE) {
                    return true;
                }
            }
        }
    }
    false
}

/// The hive at `pos`, if its block entity is one.
pub fn hive(level: &Level, pos: BlockPos) -> Option<&Beehive> {
    match level.block_entity(pos) {
        Some(BlockEntity::Beehive(h)) => Some(h),
        _ => None,
    }
}

/// `BlockEntity.setChanged(level, pos, state)`: saved, comparators read the hive again.
fn set_changed(level: &mut Level, pos: BlockPos, state: BlockState) {
    level.block_entities.touch(pos.chunk());
    if !state.is_air() {
        level.update_neighbour_for_output_signal(pos, state.block());
    }
}

/// `BeehiveBlockEntity.serverTick`: the bees' time inside, then the working sound.
pub fn tick(level: &mut Level, pos: BlockPos, state: BlockState) {
    let Some(BlockEntity::Beehive(h)) = level.block_entity_mut(pos) else {
        return;
    };
    let flower = h.flower_pos;
    let mut stored = std::mem::take(&mut h.stored);
    // `tickOccupants`.
    let mut changed = false;
    let mut i = 0;
    while i < stored.len() {
        let due = {
            let o = &mut stored[i];
            let due = o.ticks_in_hive > o.min_ticks_in_hive;
            o.ticks_in_hive += 1;
            due
        };
        if due {
            let status = if stored[i].has_nectar() {
                ReleaseStatus::HoneyDelivered
            } else {
                ReleaseStatus::BeeReleased
            };
            if release_occupant(level, pos, state, &stored[i], status, flower).is_some() {
                changed = true;
                stored.remove(i);
                continue;
            }
        }
        i += 1;
    }
    let empty = stored.is_empty();
    if let Some(BlockEntity::Beehive(h)) = level.block_entity_mut(pos) {
        h.stored = stored;
    }
    if changed {
        set_changed(level, pos, state);
    }
    if !empty && level.random().next_double() < 0.005 {
        let (x, y, z) = (
            f64::from(pos.x) + 0.5,
            f64::from(pos.y),
            f64::from(pos.z) + 0.5,
        );
        let sound = sound_id("block.beehive.work");
        level.play_sound(None, x, y, z, sound, SOUND_BLOCKS, 1.0, 1.0);
    }
}

/// `releaseOccupant`: the bee leaves through the front unless bees stay inside now or the
/// front is blocked (an emergency ignores both). Returns the new bee.
fn release_occupant(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    occupant: &Occupant,
    status: ReleaseStatus,
    flower: Option<BlockPos>,
) -> Option<EntityId> {
    let emergency = status == ReleaseStatus::Emergency;
    if level.spawner_env.env.bees_stay_in_hive && !emergency {
        return None;
    }
    let facing = state.direction("facing").unwrap_or(Direction::North);
    let front = pos.relative(facing);
    let front_blocked = !level.block(front).collision_shape().is_empty();
    if front_blocked && !emergency {
        return None;
    }
    // `Occupant.createEntity`: only `#beehive_inhabitors` (bees) come back.
    let kind = occupant
        .entity_data
        .get("id")
        .and_then(Tag::as_str)
        .and_then(EntityTypeId::by_name)?;
    if kind != entity_ids::BEE {
        return None;
    }
    let mut tag = occupant.entity_data.clone();
    for key in IGNORED_BEE_TAGS {
        tag.remove(key);
    }
    tag.insert(
        "Pos",
        Tag::List(List(vec![
            Tag::Double(0.0),
            Tag::Double(0.0),
            Tag::Double(0.0),
        ])),
    );
    let mut mob = crate::mob::nbt::load_mob(&tag, crate::mob::new_entity_seed())?.data;
    if mob.kind != Kind::Bee {
        return None;
    }
    bee::set_hive_pos(&mut mob, Some(pos));
    // `setBeeReleaseData`: the time inside counts towards growing up and the love time.
    let ticks = occupant.ticks_in_hive;
    if mob.age < 0 {
        mob.age = (mob.age + ticks).min(0);
    } else if mob.age > 0 {
        mob.age = (mob.age - ticks).max(0);
    }
    mob.baby = mob.age < 0;
    mob.in_love = (mob.in_love - ticks).max(0);
    if let Some(f) = flower
        && !bee::has_saved_flower_pos(&mob)
        && level.random().next_float() < 0.9
    {
        bee::set_saved_flower_pos(&mut mob, f);
    }
    if status == ReleaseStatus::HoneyDelivered {
        bee::drop_off_nectar(&mut mob);
        if block_tags().beehives.contains(state)
            && let Some(honey) = state.int("honey_level")
            && honey < MAX_HONEY_LEVELS
        {
            let mut increase = if level.random().next_int_bounded(100) == 0 {
                2
            } else {
                1
            };
            if honey + increase > MAX_HONEY_LEVELS {
                increase -= 1;
            }
            level.set_block_and_update(pos, state.set_int("honey_level", honey + increase));
        }
    }
    let dims = crate::mob::kinds::mob_dimensions(&mob);
    let delta = if front_blocked {
        0.0
    } else {
        0.55 + f64::from(dims.width / 2.0)
    };
    let [sx, _, sz] = facing.normal();
    let at = Vec3::new(
        f64::from(pos.x) + 0.5 + delta * f64::from(sx),
        f64::from(pos.y) + 0.5 - f64::from(dims.height / 2.0),
        f64::from(pos.z) + 0.5 + delta * f64::from(sz),
    );
    level.play_block_sound(
        None,
        pos,
        sound_id("block.beehive.exit"),
        SOUND_BLOCKS,
        1.0,
        1.0,
    );
    // `EntityType.loadEntityRecursive(..., LOAD)`, then `addFreshEntity`.
    let id = crate::mob::spawn::add_mob(level, mob, at, 0.0, crate::mob::fam::SpawnReason::Load);
    if let Some(mut e) = level.entities.store.get_mut(id) {
        e.set_flags(EntityFlags::NO_GRAVITY, true);
    }
    let now = level.block(pos);
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext::with_state(Some(id), now),
    );
    Some(id)
}

/// `releaseAllOccupants` of a hive taken out of the level: the bees that left.
fn release_all(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    hive: &mut Beehive,
    status: ReleaseStatus,
) -> Vec<EntityId> {
    let mut spawned = Vec::new();
    let flower = hive.flower_pos;
    hive.stored.retain(
        |o| match release_occupant(level, pos, state, o, status, flower) {
            Some(id) => {
                spawned.push(id);
                false
            }
            None => true,
        },
    );
    if !spawned.is_empty() {
        // `super.setChanged()`: the block entity's own (cached) state.
        set_changed(level, pos, state);
    }
    spawned
}

/// `emptyAllLivingFromHive(player, state, status)` of a hive (in the level or captured before
/// its block went): the bees near `player` attack them, or stay out a while when smoke
/// calms them.
fn empty_hive(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    hive: &mut Beehive,
    player: Option<EntityId>,
    status: ReleaseStatus,
) {
    let released = release_all(level, pos, state, hive, status);
    let Some(player) = player else {
        return;
    };
    let Some(player_pos) = level.entities.store.get(player).map(|e| e.pos()) else {
        return;
    };
    for id in released {
        let Some(bee_pos) = level.entities.store.get(id).map(|e| e.pos()) else {
            continue;
        };
        if player_pos.distance_to_sqr(bee_pos) > 16.0 {
            continue;
        }
        // `isSedated`.
        let sedated = is_smokey_pos(level, pos);
        if let Some(m) = level.entities.mob_mut(id) {
            if sedated {
                bee::set_stay_out_of_hive(m, 400);
            } else {
                m.target = Some(player);
            }
        }
    }
}

/// `emptyAllLivingFromHive` of the hive at `pos`.
pub fn empty_all_living(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    player: Option<EntityId>,
    status: ReleaseStatus,
) {
    let Some(BlockEntity::Beehive(h)) = level.block_entity_mut(pos) else {
        return;
    };
    let mut hive = std::mem::take(&mut **h);
    empty_hive(level, pos, state, &mut hive, player, status);
    if let Some(BlockEntity::Beehive(h)) = level.block_entity_mut(pos) {
        **h = hive;
    }
}

/// `addOccupant`: a bee whose saved data is `data` goes into the hive at `pos` (the bee
/// itself is discarded by its tick).
pub fn add_occupant(
    level: &mut Level,
    pos: BlockPos,
    bee: EntityId,
    data: &Compound,
    bee_flower: Option<BlockPos>,
) {
    let full = !matches!(level.block_entity(pos), Some(BlockEntity::Beehive(h)) if !h.is_full());
    if full {
        return;
    }
    // `Occupant.of`.
    let mut entity_data = data.clone();
    for key in IGNORED_BEE_TAGS {
        entity_data.remove(key);
    }
    let has_nectar = matches!(entity_data.get("HasNectar"), Some(Tag::Byte(b)) if *b != 0);
    let occupant = Occupant {
        entity_data,
        ticks_in_hive: 0,
        min_ticks_in_hive: if has_nectar {
            MIN_OCCUPATION_TICKS_NECTAR
        } else {
            MIN_OCCUPATION_TICKS_NECTARLESS
        },
    };
    let hive_flower = hive(level, pos).and_then(|h| h.flower_pos);
    let take_flower =
        bee_flower.is_some() && (hive_flower.is_none() || level.random().next_boolean());
    if let Some(BlockEntity::Beehive(h)) = level.block_entity_mut(pos) {
        h.stored.push(occupant);
        if take_flower {
            h.flower_pos = bee_flower;
        }
    }
    let sound = sound_id("block.beehive.enter");
    level.play_sound(
        None,
        f64::from(pos.x),
        f64::from(pos.y),
        f64::from(pos.z),
        sound,
        SOUND_BLOCKS,
        1.0,
        1.0,
    );
    let state = level.block(pos);
    level.game_event_block(
        events().block_change,
        pos,
        GameEventContext::with_state(Some(bee), state),
    );
    set_changed(level, pos, state);
}

/// `angerNearbyBees`: the bees within 8 blocks (6 up and down) without a target attack a
/// random player there.
pub fn anger_nearby_bees(level: &mut Level, pos: BlockPos) {
    let area = Aabb::new(
        f64::from(pos.x) - 8.0,
        f64::from(pos.y) - 6.0,
        f64::from(pos.z) - 8.0,
        f64::from(pos.x) + 9.0,
        f64::from(pos.y) + 7.0,
        f64::from(pos.z) + 9.0,
    );
    let ents = &level.entities;
    let mut bees = Vec::new();
    ents.store.for_each_in(&area, false, |id| {
        if ents.mob(id).is_some_and(|m| m.kind == Kind::Bee) {
            bees.push(id);
        }
    });
    if bees.is_empty() {
        return;
    }
    let mut players = Vec::new();
    ents.store.for_each_in(&area, true, |id| {
        if ents
            .player(id)
            .is_some_and(|p| p.game_mode != vanillite_config::GameMode::Spectator)
        {
            players.push(id);
        }
    });
    if players.is_empty() {
        return;
    }
    for id in bees {
        if level.entities.mob(id).is_some_and(|m| m.target.is_none()) {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let i = level.random().next_int_bounded(players.len() as i32);
            let target = players[usize::try_from(i).unwrap_or(0)];
            if let Some(m) = level.entities.mob_mut(id) {
                m.target = Some(target);
            }
        }
    }
}

/// `resetHoneyLevel`.
fn reset_honey_level(level: &mut Level, state: BlockState, pos: BlockPos) {
    level.set_block_and_update(pos, state.set_int("honey_level", 0));
}

/// `releaseBeesAndResetHoneyLevel`.
pub fn release_bees_and_reset_honey_level(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    player: Option<EntityId>,
    status: ReleaseStatus,
) {
    reset_honey_level(level, state, pos);
    empty_all_living(level, pos, state, player, status);
}

/// `dropHoneycomb`: `minecraft:harvest/beehive` gives three honeycombs.
pub fn drop_honeycomb(level: &mut Level, pos: BlockPos) {
    super::drops::pop_resource(level, pos, ItemStack::new(items::HONEYCOMB.0, 3));
}

/// `BeehiveBlock.useItemOn`: shears take honeycombs, glass bottles honey from a full hive;
/// without smoke below the bees burst out at the player.
fn use_item_on(
    level: &mut Level,
    state: BlockState,
    pos: BlockPos,
    ctx: &UseContext,
    stack: &mut ItemStack,
) -> ItemInteraction {
    let honey = state.int("honey_level").unwrap_or(0);
    let mut emptied = false;
    if honey >= MAX_HONEY_LEVELS {
        let player_pos = level
            .entities
            .store
            .get(ctx.player)
            .map_or(Vec3::ZERO, |e| e.pos());
        let item = crate::item::item_id(stack);
        if item == items::SHEARS {
            drop_honeycomb(level, pos);
            let sound = sound_id("block.beehive.shear");
            level.play_sound(
                None,
                player_pos.x,
                player_pos.y,
                player_pos.z,
                sound,
                SOUND_BLOCKS,
                1.0,
                1.0,
            );
            super::hurt_item(level, stack, 1, ctx.player, ctx.hand_slot());
            emptied = true;
            level.game_event_at(Some(ctx.player), events().shear, pos);
        } else if item == items::GLASS_BOTTLE {
            stack.shrink(1);
            let sound = sound_id("item.bottle.fill");
            level.play_sound(
                Some(ctx.player),
                player_pos.x,
                player_pos.y,
                player_pos.z,
                sound,
                SOUND_BLOCKS,
                1.0,
                1.0,
            );
            let bottle = ItemStack::new(items::HONEY_BOTTLE.0, 1);
            if stack.is_empty() {
                *stack = bottle;
            } else {
                level.give_to_player(ctx.player, bottle);
            }
            emptied = true;
            level.game_event_at(Some(ctx.player), events().fluid_pickup, pos);
        }
        if emptied {
            use crate::stats::{Stat, StatType};
            level.award_stat(ctx.player, Stat::of(StatType::Used, item.0), 1);
        }
    }
    if !emptied {
        return ItemInteraction::TryWithEmptyHand;
    }
    if is_smokey_pos(level, pos) {
        reset_honey_level(level, state, pos);
    } else {
        if hive(level, pos).is_some_and(|h| !h.stored.is_empty()) {
            anger_nearby_bees(level, pos);
        }
        release_bees_and_reset_honey_level(
            level,
            state,
            pos,
            Some(ctx.player),
            ReleaseStatus::Emergency,
        );
    }
    ItemInteraction::Done(Interaction::Success)
}

/// `BeehiveBlock.playerDestroy` after the drops: without Silk Touch (`#prevents_bee_spawns_
/// when_mining`) the bees of the broken hive burst out at the player and the bees around
/// get angry.
pub fn player_destroy(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    entity: Option<&BlockEntity>,
    player: EntityId,
    tool: &ItemStack,
) {
    let Some(BlockEntity::Beehive(h)) = entity else {
        return;
    };
    let prevents = crate::player::play::game_data().is_some_and(|d| {
        let set = d.holder_set(
            "minecraft:enchantment",
            "#minecraft:prevents_bee_spawns_when_mining",
        );
        crate::item::enchantments(tool)
            .iter()
            .any(|(e, _)| set.contains(e))
    });
    let mut hive = (**h).clone();
    if !prevents {
        empty_hive(
            level,
            pos,
            state,
            &mut hive,
            Some(player),
            ReleaseStatus::Emergency,
        );
        // `Containers.updateNeighboursAfterDestroy`.
        level.update_neighbour_for_output_signal(pos, state.block());
        anger_nearby_bees(level, pos);
    }
    // `BEE_NEST_DESTROYED` with the bees still inside.
    let bees = i32::try_from(hive.stored.len()).unwrap_or(i32::MAX);
    level.trigger(
        player,
        crate::advancements::trigger::Event::BeeNestDestroyed(state, tool.clone(), bees),
    );
}

/// `BeehiveBlock.playerWillDestroy` of a player who gets no drops (creative): a hive with bees
/// or honey drops as an item that keeps both.
pub fn creative_drop(level: &mut Level, pos: BlockPos, state: BlockState) {
    if !level.settings.block_drops {
        return;
    }
    let Some(h) = hive(level, pos) else {
        return;
    };
    let honey = state.int("honey_level").unwrap_or(0);
    if h.stored.is_empty() && honey <= 0 {
        return;
    }
    let item = state.block().info().item;
    if item == u16::MAX {
        return;
    }
    let mut stack = ItemStack::new(item, 1);
    stack.set_component(components::BEES, encode_bees(&h.stored));
    let mut block_state = Vec::new();
    vanillite_protocol::codec::write_len(&mut block_state, 1);
    vanillite_protocol::codec::write_str(&mut block_state, "honey_level");
    vanillite_protocol::codec::write_str(&mut block_state, &honey.to_string());
    stack.set_component(components::BLOCK_STATE, block_state.into_boxed_slice());
    let at = Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    super::drops::spawn_item_moving(level, at, stack, None, 10, None);
}

/// `BeehiveBlock.getDrops` in an explosion: TNT, creepers, wither skulls, the wither and TNT
/// minecarts drive the bees out first.
pub fn exploded(level: &mut Level, pos: BlockPos, state: BlockState, source: Option<EntityId>) {
    let kind = source.and_then(|s| level.entities.store.get(s).map(|e| e.kind()));
    if kind.is_some_and(|k| {
        k == entity_ids::TNT
            || k == entity_ids::CREEPER
            || k == entity_ids::WITHER_SKULL
            || k == entity_ids::WITHER
            || k == entity_ids::TNT_MINECART
    }) {
        empty_all_living(level, pos, state, None, ReleaseStatus::Emergency);
    }
}

/// `BeehiveBlock.updateShape`: fire next to the hive drives the bees out.
pub fn neighbour_changed_shape(
    level: &mut Level,
    pos: BlockPos,
    state: BlockState,
    neighbour: BlockPos,
) {
    if level.block(neighbour).is(ids::FIRE) {
        empty_all_living(level, pos, state, None, ReleaseStatus::Emergency);
    }
}

/// `BeehiveBlockEntity.setChanged` after a placed item filled the hive: bees next to fire
/// leave at once.
pub fn placed_changed(level: &mut Level, pos: BlockPos) {
    if is_fire_nearby(level, pos) {
        let state = level.block(pos);
        empty_all_living(level, pos, state, None, ReleaseStatus::Emergency);
    }
}
