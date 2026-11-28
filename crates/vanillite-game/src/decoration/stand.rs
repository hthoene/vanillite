//! Armor stands (`ArmorStand`) and mannequins (`Mannequin`, 26.x): living entities without AI.
//! They are mob kinds of the decor family ([`crate::mob::fam::decor`]) so the living entity
//! tick (gravity, fluids, fire, effects, equipment) is the mobs'; this module has what they
//! do differently: poses and flags, equipment swapping, hits, storage and synched data.

use vanillite_base::random::RandomSource;
use vanillite_base::{Vec3, math::floor};
use vanillite_nbt::{Compound, List, Tag};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{EntityEvent, SetEquipment, SoundSource};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{Dimensions, EntityFlags, EntityId, Pose};

use crate::entities::Entities;
use crate::item::{EquipmentSlot, ItemStack};
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::{Kind, MobContext, MobData};
use crate::player::encode::{DataWriter, serializer};
use crate::survival::Effect;
use crate::survival::damage::{DamageSource, tag};
use crate::survival::player::Env;

/// `CLIENT_FLAG_SMALL`.
pub const SMALL: u8 = 1;
/// `CLIENT_FLAG_SHOW_ARMS`.
pub const SHOW_ARMS: u8 = 4;
/// `CLIENT_FLAG_NO_BASEPLATE`.
pub const NO_BASEPLATE: u8 = 8;
/// `CLIENT_FLAG_MARKER`.
pub const MARKER: u8 = 16;

/// The default pose: head, body, left arm, right arm, left leg, right leg.
pub const DEFAULT_POSE: [[f32; 3]; 6] = [
    [0.0, 0.0, 0.0],
    [0.0, 0.0, 0.0],
    [-10.0, 0.0, -10.0],
    [-15.0, 0.0, 10.0],
    [-1.0, 0.0, -1.0],
    [1.0, 0.0, 1.0],
];
const POSE_KEYS: [&str; 6] = ["Head", "Body", "LeftArm", "RightArm", "LeftLeg", "RightLeg"];
/// `PlayerModelPart` masks of all layers.
const ALL_LAYERS: u8 = 0x7F;
const LAYER_NAMES: [&str; 7] = [
    "cape",
    "jacket",
    "left_sleeve",
    "right_sleeve",
    "left_pants_leg",
    "right_pants_leg",
    "hat",
];

/// The kind state of an armor stand or mannequin (in [`crate::mob::fam::Ext::Decor`]).
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    /// `DATA_CLIENT_FLAGS`.
    pub flags: u8,
    /// `DATA_*_POSE`.
    pub pose: [[f32; 3]; 6],
    /// `disabledSlots`.
    pub disabled_slots: i32,
    /// `lastHit` (game time).
    pub last_hit: i64,
    /// `isCustomNameVisible`.
    pub name_visible: bool,
    /// `Invisible` as read (`ArmorStand.invisible`).
    pub invisible: bool,
    /// `Glowing` and `NoGravity` as read, for the entity's flags once it is spawned.
    pub glowing: bool,
    pub no_gravity: bool,
    /// Mannequin: `DATA_PROFILE` as NBT (`ResolvableProfile.CODEC`).
    pub profile: Tag,
    /// Mannequin: `DATA_PLAYER_MODE_CUSTOMISATION`.
    pub layers: u8,
    /// Mannequin: the main arm is the right one.
    pub right_handed: bool,
    /// Mannequin: `DATA_IMMOVABLE`.
    pub immovable: bool,
    /// Mannequin: `description` (`None`: the default label).
    pub description: Option<Tag>,
    /// Mannequin: `hide_description`.
    pub hide_description: bool,
    /// Mannequin: the stored pose.
    pub mannequin_pose: Pose,
}

impl Default for State {
    fn default() -> State {
        State {
            flags: 0,
            pose: DEFAULT_POSE,
            disabled_slots: 0,
            last_hit: 0,
            name_visible: false,
            invisible: false,
            glowing: false,
            no_gravity: false,
            profile: Tag::Compound(Compound::new()),
            layers: ALL_LAYERS,
            right_handed: true,
            immovable: false,
            description: None,
            hide_description: false,
            mannequin_pose: Pose::Standing,
        }
    }
}

/// Whether the kind is an armor stand or mannequin.
pub fn is_stand(kind: Kind) -> bool {
    matches!(kind, Kind::ArmorStand | Kind::Mannequin)
}

/// The state of a decor family mob.
pub fn state(m: &MobData) -> Option<&State> {
    match &m.ext {
        crate::mob::fam::Ext::Decor(s) => Some(s),
        _ => None,
    }
}

/// The state of a decor family mob, created on first use.
pub fn state_mut(m: &mut MobData) -> &mut State {
    if !matches!(m.ext, crate::mob::fam::Ext::Decor(_)) {
        m.ext = crate::mob::fam::Ext::Decor(Box::default());
    }
    match &mut m.ext {
        crate::mob::fam::Ext::Decor(s) => s,
        _ => unreachable!("set above"),
    }
}

fn flags(m: &MobData) -> u8 {
    state(m).map_or(0, |s| s.flags)
}

/// `isMarker`.
pub fn is_marker(m: &MobData) -> bool {
    m.kind == Kind::ArmorStand && flags(m) & MARKER != 0
}

/// `showArms`.
fn show_arms(m: &MobData) -> bool {
    flags(m) & SHOW_ARMS != 0
}

/// `hasPhysics` of an armor stand / `isEffectiveAi` of a mannequin: whether it moves.
pub fn moves(m: &MobData, no_gravity: bool) -> bool {
    match m.kind {
        Kind::ArmorStand => !is_marker(m) && !no_gravity,
        _ => !state(m).is_some_and(|s| s.immovable),
    }
}

/// `isAttackable` for targeting (`attackable()`): armor stands are no targets.
pub fn attackable(m: &MobData) -> bool {
    m.kind != Kind::ArmorStand
}

/// `isPushable`: armor stands are not pushed.
pub fn pushable(m: &MobData) -> bool {
    m.kind != Kind::ArmorStand
}

/// The dimensions of an armor stand (marker, small) or of a mannequin's pose.
pub fn dimensions(m: &MobData) -> Option<Dimensions> {
    let d = |width, height| Some(Dimensions { width, height });
    match m.kind {
        Kind::ArmorStand if is_marker(m) => d(0.0, 0.0),
        Kind::ArmorStand if flags(m) & SMALL != 0 => d(0.25, 0.9875),
        Kind::ArmorStand => None,
        _ => match state(m).map_or(Pose::Standing, |s| s.mannequin_pose) {
            Pose::Crouching => d(0.6, 1.5),
            Pose::Swimming | Pose::FallFlying => d(0.6, 0.6),
            Pose::Sleeping => d(0.2, 0.2),
            _ => None,
        },
    }
}

/// Applies what the state says to the entity once it exists (flags, pose, dimensions).
pub fn after_spawn(ents: &mut Entities, id: EntityId) {
    let Some(m) = ents.mob(id) else {
        return;
    };
    let Some(s) = state(m).cloned() else {
        return;
    };
    let dims = crate::mob::kinds::mob_dimensions(m);
    let kind = m.kind;
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_flags(EntityFlags::INVISIBLE, s.invisible);
        e.set_flags(EntityFlags::GLOWING, s.glowing);
        e.set_flags(EntityFlags::NO_GRAVITY, s.no_gravity);
        if kind == Kind::Mannequin {
            e.set_pose(s.mannequin_pose);
        }
        if e.as_ref().dims() != dims {
            // `ArmorStand.refreshDimensions` keeps the position.
            e.set_dims(dims);
        }
        e.mark_data_dirty();
    }
}

/// `ArmorStandItem.useOn`: an armor stand on the clicked face, facing away from the player
/// in steps of 45 degrees.
#[allow(clippy::too_many_arguments)]
pub fn use_on(
    level: &mut Level,
    pos: vanillite_base::BlockPos,
    face: vanillite_base::Direction,
    y_rot: f32,
    stack: &mut ItemStack,
    player: Option<EntityId>,
    infinite: bool,
    spectators: &[EntityId],
) -> bool {
    if face == vanillite_base::Direction::Down {
        return false;
    }
    let replace = level
        .block(pos)
        .has(vanillite_data::block::flags::REPLACEABLE);
    let at = if replace { pos } else { pos.relative(face) };
    let bottom = Vec3::new(
        f64::from(at.x) + 0.5,
        f64::from(at.y),
        f64::from(at.z) + 0.5,
    );
    let aabb = Dimensions::of(vanillite_data::entity::ids::ARMOR_STAND).bounding_box(bottom);
    let mut scratch = Vec::new();
    if !crate::mob::physics::no_collision(&level.view(), &aabb, &mut scratch) {
        return false;
    }
    // `getEntities(null, box)`: spectators do not count.
    let mut any = false;
    let ents = &level.entities;
    ents.store.for_each_in(&aabb, false, |o| {
        if ents
            .store
            .get(o)
            .is_some_and(|e| e.aabb().intersects(&aabb))
            && !spectators.contains(&o)
            && ents
                .player(o)
                .is_none_or(|p| p.game_mode != vanillite_config::GameMode::Spectator)
        {
            any = true;
        }
    });
    if any {
        return false;
    }
    // `EntityType.create(.., tryMoveDown, movedUp)`: down onto the collisions of the block
    // and the one below (`getYOffset`), and the level random's yaw (replaced below).
    let y_offset = crate::mob::animal::spawn_y_offset(level, at, true, Kind::ArmorStand);
    let spawn = Vec3::new(bottom.x, f64::from(at.y) + y_offset, bottom.z);
    let _ = level.random().next_float();
    let wrapped = vanillite_base::math::wrap_degrees_f(y_rot - 180.0);
    #[allow(clippy::cast_precision_loss)]
    let yaw = floor(f64::from((wrapped + 22.5) / 45.0)) as f32 * 45.0;
    let id = spawn_stand(
        level,
        spawn,
        yaw,
        stack,
        vanillite_abi::spawn_reason::SPAWN_ITEM_USE,
    );
    if !level.entities.store.contains(id) {
        return true;
    }
    if let Some(at) = level.entities.store.get(id).map(|e| e.pos()) {
        level.play_sound(
            None,
            at.x,
            at.y,
            at.z,
            super::hanging::sound_id("minecraft:entity.armor_stand.place"),
            crate::level::SOUND_BLOCKS,
            0.75,
            0.8,
        );
        let event = crate::level::game_event::events().entity_place;
        level.game_event(
            event,
            at,
            crate::level::game_event::GameEventContext::of(player),
        );
    }
    if !infinite {
        stack.shrink(1);
    }
    true
}

/// A new armor stand at `pos` with `yaw` and the stack's components (`createDefaultStackConfig`),
/// spawned for `reason` (`vanillite_abi::spawn_reason`; a plugin may cancel it, the returned
/// entity is then gone).
pub fn spawn_stand(
    level: &mut Level,
    pos: Vec3,
    yaw: f32,
    stack: &ItemStack,
    reason: u32,
) -> EntityId {
    let seed = crate::mob::new_entity_seed();
    let mut m = MobData::new(Kind::ArmorStand, seed);
    state_mut(&mut m);
    m.y_body_rot = yaw;
    m.custom_name = stack
        .component(vanillite_protocol::components::ids::CUSTOM_NAME)
        .map(Box::from);
    let network_ids = std::sync::Arc::clone(level.network_ids_arc());
    let uuid = crate::mob::new_uuid();
    let rot = vanillite_world::entity::Rotation {
        y_rot: yaw,
        x_rot: 0.0,
        // `snapTo` turns the yaw and the body: the head keeps the constructor's yaw.
        y_head_rot: crate::mob::constructor_y_rot(seed),
    };
    let id = crate::mob::spawn(&mut level.entities, &network_ids, uuid, pos, rot, m);
    apply_entity_data_component(&mut level.entities, id, stack);
    level.entities.spawned(id, reason, 0);
    id
}

/// `EntityType.updateCustomEntityTag`: the `entity_data` component of the item merged into the
/// new entity's data.
pub fn apply_entity_data_component(ents: &mut Entities, id: EntityId, stack: &ItemStack) {
    use vanillite_protocol::components::ids;
    let mut data = match stack.component(ids::ENTITY_DATA) {
        Some(wire) => match vanillite_protocol::components_nbt::to_nbt(
            ids::ENTITY_DATA,
            wire,
            &vanillite_protocol::item::InstalledRegistries,
        ) {
            Ok(Tag::Compound(data)) => data,
            _ => return,
        },
        // The default `entity_data` of a spawn egg: only its `id`.
        None if crate::item::item_id(stack).name().ends_with("_spawn_egg") => Compound::new(),
        None => return,
    };
    // `entity.getType() == entityData.type()`.
    let kind = ents.store.get(id).map(|e| e.kind());
    if let Some(Tag::String(t)) = data.remove("id")
        && kind.map(|k| k.name()) != Some(t.as_str())
        && kind.map(|k| k.name().trim_start_matches("minecraft:")) != Some(t.as_str())
    {
        return;
    }
    // Only the `id`: nothing changes for a decoration; a mob still loads its own data
    // again (`entity.load`), which makes its brain anew.
    if data.is_empty() && ents.mob(id).is_none() {
        return;
    }
    let Some(mut tag) = save_any(ents, id) else {
        return;
    };
    let uuid = tag.get("UUID").cloned();
    merge(&mut tag, &data);
    if let Some(u) = uuid {
        tag.insert("UUID", u);
    }
    let (game_time, day_time) = (ents.game_time, ents.day_time);
    if !super::apply_nbt(ents, id, &tag)
        && let Some(m) = ents.mob_mut(id)
    {
        crate::mob::nbt::apply_mob(m, &tag, game_time, day_time);
        if let Some(mut e) = ents.store.get_mut(id) {
            e.mark_data_dirty();
        }
    }
}

/// `CompoundTag.merge`: compounds merge, other values replace.
pub(crate) fn merge(into: &mut Compound, from: &Compound) {
    for (k, v) in from.iter() {
        match (into.get_mut(k), v) {
            (Some(Tag::Compound(a)), Tag::Compound(b)) => merge(a, b),
            _ => into.insert(k, v.clone()),
        }
    }
}

/// `PREVENT_EQUIPMENT_DROP` (Curse of Vanishing).
fn prevents_drop(stack: &ItemStack) -> bool {
    crate::player::play::game_data()
        .is_some_and(|d| d.enchantment_level(stack, "minecraft:vanishing_curse") > 0)
}

/// The stored data of a decoration or decor family mob (for merging).
fn save_any(ents: &Entities, id: EntityId) -> Option<Compound> {
    if let Some(c) = super::save(ents, id) {
        return Some(c);
    }
    let e = ents.store.get(id)?;
    let m = ents.mob(id)?;
    Some(crate::mob::nbt::save_mob(&e, m))
}

/// `/data` and `/summon` on a live armor stand or mannequin: its data is read again from the
/// whole compound.
pub fn apply_nbt(ents: &mut Entities, id: EntityId, tag: &Compound) -> bool {
    let Some(kind) = ents.mob(id).map(|m| m.kind) else {
        return false;
    };
    if !is_stand(kind) {
        return false;
    }
    let Some(e) = ents.store.get(id) else {
        return false;
    };
    let mut full = tag.clone();
    full.insert("id", kind.type_id().name());
    if full.get("Pos").is_none() {
        let p = e.pos();
        full.insert(
            "Pos",
            List(vec![Tag::Double(p.x), Tag::Double(p.y), Tag::Double(p.z)]),
        );
    }
    full.insert("UUID", crate::mob::nbt::uuid_tag(e.uuid()));
    let Some(loaded) = crate::mob::nbt::load_mob(&full, 0) else {
        return false;
    };
    let (pos, rot, velocity) = (loaded.pos, loaded.rot, loaded.velocity);
    let rotated = tag.get("Rotation").is_some();
    if let Some(m) = ents.mob_mut(id) {
        let random = std::mem::replace(&mut m.random, vanillite_base::random::LegacyRandom::new(0));
        let tick = m.tick_count;
        *m = loaded.data;
        m.random = random;
        m.tick_count = tick;
    }
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_pos(pos);
        e.set_velocity(velocity);
        if rotated {
            e.set_rot(rot);
        }
    }
    after_spawn(ents, id);
    true
}

// --- Armor stand interaction -------------------------------------------------------------

/// `EquipmentSlot.getFilterBit(0)`.
fn filter_bit(slot: EquipmentSlot) -> i32 {
    match slot {
        EquipmentSlot::MainHand => 0,
        EquipmentSlot::Feet => 1,
        EquipmentSlot::Legs => 2,
        EquipmentSlot::Chest => 3,
        EquipmentSlot::Head => 4,
        EquipmentSlot::OffHand => 5,
        EquipmentSlot::Body => 6,
        EquipmentSlot::Saddle => 7,
    }
}

fn is_hand(slot: EquipmentSlot) -> bool {
    matches!(slot, EquipmentSlot::MainHand | EquipmentSlot::OffHand)
}

/// `ArmorStand.isDisabled(slot)`.
fn is_disabled(m: &MobData, slot: EquipmentSlot) -> bool {
    let disabled = state(m).map_or(0, |s| s.disabled_slots);
    disabled & (1 << filter_bit(slot)) != 0 || is_hand(slot) && !show_arms(m)
}

/// `getEquipmentSlotForItem` with `canUseSlot`.
fn slot_for_item(m: &MobData, stack: &ItemStack) -> EquipmentSlot {
    let slot = crate::item::equipment_slot(stack);
    if matches!(slot, EquipmentSlot::Body | EquipmentSlot::Saddle) || is_disabled(m, slot) {
        EquipmentSlot::MainHand
    } else {
        slot
    }
}

/// `getClickedSlot(location)`.
fn clicked_slot(m: &MobData, location: Vec3) -> EquipmentSlot {
    let small = flags(m) & SMALL != 0;
    let scale = if small { 0.5 } else { 1.0 };
    let y = location.y / scale;
    let has = |s: EquipmentSlot| !m.equipment.get(s).is_empty();
    let (feet, chest, legs) = if small {
        (0.8, (0.3, 1.0), 1.0)
    } else {
        (0.45, (0.0, 0.7), 0.8)
    };
    if y >= 0.1 && y < 0.1 + feet && has(EquipmentSlot::Feet) {
        EquipmentSlot::Feet
    } else if y >= 0.9 + chest.0 && y < 0.9 + chest.1 && has(EquipmentSlot::Chest) {
        EquipmentSlot::Chest
    } else if y >= 0.4 && y < 0.4 + legs && has(EquipmentSlot::Legs) {
        EquipmentSlot::Legs
    } else if y >= 1.6 && has(EquipmentSlot::Head) {
        EquipmentSlot::Head
    } else if !has(EquipmentSlot::MainHand) && has(EquipmentSlot::OffHand) {
        EquipmentSlot::OffHand
    } else {
        EquipmentSlot::MainHand
    }
}

/// `ArmorStand.interact`: equipment goes on or comes off. `location` is relative to the
/// stand. `Pass` lets the item act (name tags).
pub fn interact(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    id: EntityId,
    hand: Hand,
    location: Vec3,
) -> Interaction {
    let Some(m) = ents.mob(id) else {
        return Interaction::Pass;
    };
    if m.kind != Kind::ArmorStand {
        return Interaction::Pass;
    }
    let stack = crate::mob::animal::held(ents, player, hand);
    if is_marker(m) || crate::item::item_id(&stack).name() == "minecraft:name_tag" {
        return Interaction::Pass;
    }
    if ents
        .player(player)
        .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Spectator)
    {
        return Interaction::Success;
    }
    let item_slot = slot_for_item(m, &stack);
    if stack.is_empty() {
        let clicked = clicked_slot(m, location);
        let target = if is_disabled(m, clicked) {
            item_slot
        } else {
            clicked
        };
        if !m.equipment.get(target).is_empty() && swap(ents, env, player, id, target, hand) {
            return Interaction::Success;
        }
    } else {
        // `InteractionResult.FAIL` does not consume the use: the item may still act.
        if is_disabled(m, item_slot) || is_hand(item_slot) && !show_arms(m) {
            return Interaction::Pass;
        }
        if swap(ents, env, player, id, item_slot, hand) {
            return Interaction::Success;
        }
    }
    Interaction::Pass
}

/// `swapItem(player, slot, playerItemStack, hand)`.
fn swap(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    id: EntityId,
    slot: EquipmentSlot,
    hand: Hand,
) -> bool {
    let Some(m) = ents.mob(id) else {
        return false;
    };
    let current = m.equipment.get(slot).clone();
    let disabled = state(m).map_or(0, |s| s.disabled_slots);
    let bit = filter_bit(slot);
    if !current.is_empty() && disabled & (1 << (bit + 8)) != 0 {
        return false;
    }
    if current.is_empty() && disabled & (1 << (bit + 16)) != 0 {
        return false;
    }
    let held = crate::mob::animal::held(ents, player, hand);
    let infinite = crate::mob::animal::infinite(ents, player);
    let i = crate::mob::animal::hand_index(ents, player, hand);
    let (new, back) = if infinite && current.is_empty() && !held.is_empty() {
        (held.with_count(1), None)
    } else if held.is_empty() || held.count <= 1 {
        (held, Some(current))
    } else if !current.is_empty() {
        return false;
    } else {
        let mut rest = held;
        let one = rest.split(1);
        (one, Some(rest))
    };
    if let Some(back) = back
        && let Some(p) = ents.player_mut(player)
    {
        p.inventory.set(i, back);
    }
    set_slot(ents, env, id, slot, new);
    true
}

/// `setItemSlot` with the equip sound and the equipment packet.
pub fn set_slot(
    ents: &mut Entities,
    env: &mut Env<'_>,
    id: EntityId,
    slot: EquipmentSlot,
    stack: ItemStack,
) {
    let Some(m) = ents.mob_mut(id) else {
        return;
    };
    let equip = !stack.is_empty() && !is_hand(slot);
    let sound = equip.then(|| crate::block::dispenser::equip_sound(&stack));
    m.equipment.set(slot, stack);
    let silent = m.silent;
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let (network_id, pos) = (e.network_id().0, e.pos());
    if let Some(sound) = sound
        && !silent
    {
        crate::survival::play_sound(
            env.out,
            None,
            pos,
            &format!("minecraft:{sound}"),
            SoundSource::Neutral,
            1.0,
            1.0,
            crate::survival::sound_seed(),
        );
    }
    let slots = crate::mob::equipment::SLOTS
        .iter()
        .map(|&s| {
            (
                s.ordinal(),
                ents.mob(id)
                    .map(|m| m.equipment.get(s).clone())
                    .unwrap_or_default(),
            )
        })
        .collect();
    env.out.push(Effect::Tracking {
        entity: id,
        with_self: false,
        packet: SharedPacket::of(&SetEquipment {
            entity_id: network_id,
            slots,
        }),
    });
}

/// `EquipmentDispenseItemBehavior` on an armor stand: the item goes on when its slot is free.
pub fn can_dispenser_equip(m: &MobData, stack: &ItemStack) -> Option<EquipmentSlot> {
    if m.kind != Kind::ArmorStand || !m.is_alive() {
        return None;
    }
    let slot = slot_for_item(m, stack);
    let equippable = crate::item::equipment_slot(stack);
    (slot == equippable && !is_disabled(m, slot) && m.equipment.get(slot).is_empty())
        .then_some(slot)
}

// --- Armor stand hits --------------------------------------------------------------------

/// `ArmorStand.hurtServer`. Returns whether it was hit.
pub fn hurt(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    source: &DamageSource,
) -> bool {
    let _ = level;
    let Some(m) = ents.mob(id) else {
        return false;
    };
    if m.removed {
        return false;
    }
    let types = env.types;
    let is = |t: u32| types.is(source.kind, t);
    let mob_cause = source.cause.is_some_and(|c| ents.mob(c).is_some());
    if mob_cause && !env.rules.bool(crate::survival::rules::Rule::MOB_GRIEFING) {
        return false;
    }
    if is(tag::BYPASSES_INVULNERABILITY) {
        kill(ents, env, cx, id, source.cause);
        return false;
    }
    let invisible = state(m).is_some_and(|s| s.invisible);
    let fire_immune = is(tag::IS_FIRE) && m.kind.type_id().info().fire_immune;
    if fire_immune
        || invisible
        || is_marker(m)
        || crate::mob::entity_nbt::invulnerable_to(ents, types, id, source)
    {
        return false;
    }
    if is(tag::IS_EXPLOSION) {
        broken_by_anything(ents, env, id);
        kill(ents, env, cx, id, source.cause);
        return false;
    }
    if is(tag::IGNITES_ARMOR_STANDS) {
        if m.fire_ticks > 0 {
            cause_damage(ents, env, cx, id, source, 0.15);
        } else if let Some(m) = ents.mob_mut(id) {
            m.fire_ticks = m.fire_ticks.max(100);
        }
        return false;
    }
    if is(tag::BURNS_ARMOR_STANDS) && m.health > 0.5 {
        cause_damage(ents, env, cx, id, source, 4.0);
        return false;
    }
    let incremental = is(tag::CAN_BREAK_ARMOR_STAND);
    let always = is(tag::ALWAYS_KILLS_ARMOR_STANDS);
    if !incremental && !always {
        return false;
    }
    if let Some(p) = source.cause.and_then(|c| ents.player(c))
        && matches!(
            p.game_mode,
            vanillite_config::GameMode::Adventure | vanillite_config::GameMode::Spectator
        )
    {
        return false;
    }
    if super::creative(ents, source.cause) {
        play_broken_sound(ents, env, id);
        breaking_particles(ents, env, id);
        kill(ents, env, cx, id, source.cause);
        return true;
    }
    let time = env.game_time;
    let last = ents.mob(id).and_then(state).map_or(0, |s| s.last_hit);
    if time - last > 5 && !always {
        if let Some(e) = ents.store.get(id) {
            env.out.push(Effect::Tracking {
                entity: id,
                with_self: false,
                packet: SharedPacket::of(&EntityEvent {
                    entity_id: e.network_id().0,
                    event: 32,
                }),
            });
        }
        game_event(
            ents,
            cx,
            id,
            crate::level::game_event::events().entity_damage,
            source.cause,
        );
        if let Some(m) = ents.mob_mut(id) {
            state_mut(m).last_hit = time;
        }
    } else {
        broken_by_player(ents, env, id);
        breaking_particles(ents, env, id);
        kill(ents, env, cx, id, source.cause);
    }
    true
}

fn game_event(
    ents: &Entities,
    cx: &mut MobContext,
    id: EntityId,
    event: u16,
    source: Option<EntityId>,
) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    cx.actions.push(crate::mob::WorldAction::Level(
        crate::mob::boss::warden::LevelAction::GameEvent {
            event,
            pos,
            context: crate::level::game_event::GameEventContext::of(source.or(Some(id))),
        },
    ));
}

/// `ArmorStand.kill`: removed at once (no death animation).
fn kill(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    by: Option<EntityId>,
) {
    if let Some(m) = ents.mob_mut(id) {
        if m.removed {
            return;
        }
        m.removed = true;
    }
    game_event(
        ents,
        cx,
        id,
        crate::level::game_event::events().entity_die,
        by,
    );
    // `CatalystListener` takes the source as the dying entity: a player's experience reward
    // becomes charge at the stand (`Player.getBaseExperienceReward`) once
    // (`wasExperienceConsumed`), the stand's is 0; the nearest catalyst blooms. A source that
    // is not living reaches no catalyst.
    if let Some(pos) = ents.store.get(id).map(|e| e.pos()) {
        let action = match by {
            Some(p) if ents.player(p).is_some() => ents
                .player(p)
                .filter(|d| !d.survival.experience_consumed)
                .map(|d| {
                    let experience = if d.game_mode == vanillite_config::GameMode::Spectator
                        || env.rules.bool(crate::survival::rules::Rule::KEEP_INVENTORY)
                    {
                        0
                    } else {
                        d.survival.xp.level.saturating_mul(7).min(100)
                    };
                    (experience, Some(p))
                }),
            Some(m) if ents.mob(m).is_none() => None,
            _ => Some((0, None)),
        };
        if let Some((experience, player)) = action {
            cx.actions.push(crate::mob::WorldAction::Level(
                crate::mob::boss::warden::LevelAction::SculkDeath {
                    pos,
                    experience,
                    player,
                },
            ));
        }
    }
    cx.discarded.push(id);
}

/// `causeDamage`.
fn cause_damage(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    source: &DamageSource,
    amount: f32,
) {
    let Some(m) = ents.mob_mut(id) else {
        return;
    };
    let health = m.health - amount;
    if health <= 0.5 {
        broken_by_anything(ents, env, id);
        kill(ents, env, cx, id, source.cause);
    } else {
        m.health = health;
        if let Some(mut e) = ents.store.get_mut(id) {
            e.mark_data_dirty();
        }
        game_event(
            ents,
            cx,
            id,
            crate::level::game_event::events().entity_damage,
            source.cause,
        );
    }
}

fn play_broken_sound(ents: &Entities, env: &mut Env<'_>, id: EntityId) {
    if let Some(pos) = ents.store.get(id).map(|e| e.pos()) {
        crate::survival::play_sound(
            env.out,
            None,
            pos,
            "minecraft:entity.armor_stand.break",
            SoundSource::Neutral,
            1.0,
            1.0,
            crate::survival::sound_seed(),
        );
    }
}

fn breaking_particles(ents: &Entities, env: &mut Env<'_>, id: EntityId) {
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let (pos, dims) = (e.pos(), e.dims());
    let kind = vanillite_data::registry::PARTICLE_TYPE
        .id("minecraft:block")
        .map_or(0, i32::from);
    let mut options = Vec::new();
    vanillite_protocol::codec::write_varint(
        &mut options,
        i32::from(vanillite_data::block::ids::OAK_PLANKS.default_state().0),
    );
    env.out.push(Effect::Nearby {
        pos,
        range: 32.0,
        except: None,
        packet: SharedPacket::of(
            &vanillite_protocol::packets::play::clientbound::LevelParticles {
                particle: vanillite_protocol::packets::play::clientbound::Particle {
                    kind,
                    options,
                },
                override_limiter: false,
                always_show: false,
                x: pos.x,
                y: pos.y + f64::from(dims.height) * 0.666_666_666_666_666_6,
                z: pos.z,
                x_dist: dims.width / 4.0,
                y_dist: dims.height / 4.0,
                z_dist: dims.width / 4.0,
                x_max_speed: 0.05,
                y_max_speed: 0.05,
                z_max_speed: 0.05,
                count: 10,
                randomization: 0,
            },
        ),
    });
}

/// `Block.popResource(level, pos, stack)` from the entity context.
fn pop_resource(env: &mut Env<'_>, pos: vanillite_base::BlockPos, stack: ItemStack) {
    if stack.is_empty() {
        return;
    }
    let r = &mut *env.random;
    use vanillite_base::random::RandomSource;
    let x = f64::from(pos.x) + 0.5 + (r.next_double() * 0.5 - 0.25);
    let y = f64::from(pos.y) + 0.5 + (r.next_double() * 0.5 - 0.25) - 0.125;
    let z = f64::from(pos.z) + 0.5 + (r.next_double() * 0.5 - 0.25);
    super::spawn_item(env, Vec3::new(x, y, z), stack);
}

/// `brokenByPlayer`: the armor stand item (with its name), then [`broken_by_anything`].
fn broken_by_player(ents: &mut Entities, env: &mut Env<'_>, id: EntityId) {
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let p = e.pos();
    let block = vanillite_base::BlockPos::containing(p.x, p.y, p.z);
    let name = ents.mob(id).and_then(|m| m.custom_name.clone());
    if let Some(item) = super::stack_of("minecraft:armor_stand") {
        pop_resource(env, block, super::with_name(item, name.as_deref()));
    }
    broken_by_anything(ents, env, id);
}

/// `brokenByAnything`: the break sound and the equipment above the stand.
fn broken_by_anything(ents: &mut Entities, env: &mut Env<'_>, id: EntityId) {
    play_broken_sound(ents, env, id);
    let Some(p) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let above = vanillite_base::BlockPos::containing(p.x, p.y, p.z).offset(0, 1, 0);
    let Some(m) = ents.mob_mut(id) else {
        return;
    };
    let items: Vec<ItemStack> = crate::mob::equipment::SLOTS
        .iter()
        .map(|&s| std::mem::take(&mut m.equipment.items[usize::from(s.ordinal())]))
        .collect();
    for stack in items {
        if !stack.is_empty() && !prevents_drop(&stack) {
            pop_resource(env, above, stack);
        }
    }
}

// --- Client data and storage -------------------------------------------------------------

/// The synched data of the decor family (ids 15 and up).
pub fn encode_data(w: &mut DataWriter<'_>, m: &MobData) {
    let default = State::default();
    let s = state(m).unwrap_or(&default);
    if m.kind == Kind::ArmorStand {
        w.byte(15, s.flags, 0);
        for (i, rot) in s.pose.iter().enumerate() {
            let def = DEFAULT_POSE[i];
            let same = rot.iter().zip(def).all(|(a, b)| a.to_bits() == b.to_bits());
            w.raw(
                16 + u8::try_from(i).unwrap_or(0),
                serializer::ROTATIONS,
                same,
                |o| {
                    for f in rot {
                        o.extend_from_slice(&f.to_be_bytes());
                    }
                },
            );
        }
        return;
    }
    w.varint(15, serializer::HUMANOID_ARM, i32::from(s.right_handed), 1);
    w.byte(16, s.layers, 0);
    let profile = profile_wire(&s.profile);
    let empty = profile_wire(&Tag::Compound(Compound::new()));
    w.raw(17, serializer::RESOLVABLE_PROFILE, profile == empty, |o| {
        o.extend_from_slice(&profile);
    });
    w.boolean(18, s.immovable, false);
    let description = if s.hide_description {
        None
    } else {
        Some(s.description.clone().unwrap_or_else(default_label))
    };
    let is_default = !s.hide_description && s.description.is_none();
    w.raw(19, serializer::OPTIONAL_COMPONENT, is_default, |o| {
        o.push(u8::from(description.is_some()));
        if let Some(d) = &description {
            let _ = vanillite_nbt::write::write_network(d, o);
        }
    });
}

fn default_label() -> Tag {
    let mut c = Compound::new();
    c.insert("translate", "entity.minecraft.mannequin.label");
    Tag::Compound(c)
}

fn profile_wire(profile: &Tag) -> Vec<u8> {
    vanillite_protocol::components_nbt::from_nbt(
        vanillite_protocol::components::ids::PROFILE,
        profile,
        &vanillite_protocol::item::InstalledRegistries,
    )
    .map_or_else(|_| vec![0, 0, 0, 0, 0, 0], Vec::from)
}

/// `addAdditionalSaveData` of an armor stand or mannequin (after the mob's keys, of which
/// those `Mob` adds are removed).
pub fn save(m: &MobData, out: &mut Compound) {
    for k in [
        "CanPickUpLoot",
        "PersistenceRequired",
        "LeftHanded",
        "NoAI",
        "drop_chances",
        "home_radius",
        "home_pos",
        "leash",
    ] {
        out.remove(k);
    }
    let default = State::default();
    let s = state(m).unwrap_or(&default);
    if s.name_visible {
        out.insert("CustomNameVisible", true);
    }
    if s.glowing {
        out.insert("Glowing", true);
    }
    if m.kind == Kind::ArmorStand {
        out.insert("Invisible", s.invisible);
        out.insert("Small", s.flags & SMALL != 0);
        out.insert("ShowArms", s.flags & SHOW_ARMS != 0);
        out.insert("DisabledSlots", s.disabled_slots);
        out.insert("NoBasePlate", s.flags & NO_BASEPLATE != 0);
        if s.flags & MARKER != 0 {
            out.insert("Marker", true);
        }
        let mut pose = Compound::new();
        for (i, key) in POSE_KEYS.iter().enumerate() {
            pose.insert(
                *key,
                List(s.pose[i].iter().map(|&f| Tag::Float(f)).collect()),
            );
        }
        out.insert("Pose", pose);
        return;
    }
    out.insert("profile", s.profile.clone());
    let hidden: Vec<Tag> = LAYER_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| s.layers & (1 << i) == 0)
        .map(|(_, n)| Tag::String((*n).to_owned()))
        .collect();
    out.insert("hidden_layers", List(hidden));
    out.insert("main_hand", if s.right_handed { "right" } else { "left" });
    out.insert("pose", pose_name(s.mannequin_pose));
    out.insert("immovable", s.immovable);
    if s.hide_description {
        out.insert("hide_description", true);
    } else if let Some(d) = &s.description {
        out.insert("description", d.clone());
    }
}

fn pose_name(p: Pose) -> &'static str {
    match p {
        Pose::Crouching => "crouching",
        Pose::Swimming => "swimming",
        Pose::FallFlying => "fall_flying",
        Pose::Sleeping => "sleeping",
        _ => "standing",
    }
}

/// `readAdditionalSaveData` of an armor stand or mannequin.
pub fn load(m: &mut MobData, tag: &Compound) {
    let flag = |k: &str| super::flag(tag, k);
    let kind = m.kind;
    let s = state_mut(m);
    s.name_visible = flag("CustomNameVisible");
    s.glowing = flag("Glowing");
    s.no_gravity = flag("NoGravity");
    s.invisible = flag("Invisible");
    if kind == Kind::ArmorStand {
        let mut f = 0;
        for (key, bit) in [
            ("Small", SMALL),
            ("ShowArms", SHOW_ARMS),
            ("NoBasePlate", NO_BASEPLATE),
            ("Marker", MARKER),
        ] {
            if flag(key) {
                f |= bit;
            }
        }
        s.flags = f;
        s.disabled_slots = super::int(tag, "DisabledSlots").unwrap_or(0);
        s.pose = DEFAULT_POSE;
        if let Some(Tag::Compound(p)) = tag.get("Pose") {
            for (i, key) in POSE_KEYS.iter().enumerate() {
                if let Some(Tag::List(List(v))) = p.get(key)
                    && v.len() == 3
                {
                    let f = |t: &Tag| match t {
                        Tag::Float(f) => *f,
                        #[allow(clippy::cast_possible_truncation)]
                        Tag::Double(d) => *d as f32,
                        _ => 0.0,
                    };
                    // `Rotations`: NaN reads as 0, the angles wrap to [-180, 180).
                    let r = |v: f32| {
                        let v = if v.is_finite() { v } else { 0.0 };
                        v % 360.0
                    };
                    s.pose[i] = [r(f(&v[0])), r(f(&v[1])), r(f(&v[2]))];
                }
            }
        }
        return;
    }
    if let Some(p) = tag.get("profile") {
        s.profile = p.clone();
    }
    s.layers = match tag.get("hidden_layers") {
        Some(Tag::List(List(v))) => {
            let mut mask = ALL_LAYERS;
            for n in v.iter().filter_map(Tag::as_str) {
                if let Some(i) = LAYER_NAMES.iter().position(|l| *l == n) {
                    mask &= !(1 << i);
                }
            }
            mask
        }
        _ => ALL_LAYERS,
    };
    s.right_handed = tag.get("main_hand").and_then(Tag::as_str) != Some("left");
    s.mannequin_pose = match tag.get("pose").and_then(Tag::as_str) {
        Some("crouching") => Pose::Crouching,
        Some("swimming") => Pose::Swimming,
        Some("fall_flying") => Pose::FallFlying,
        Some("sleeping") => Pose::Sleeping,
        _ => Pose::Standing,
    };
    s.immovable = flag("immovable");
    s.hide_description = flag("hide_description");
    s.description = tag.get("description").cloned();
}

/// `ArmorStand.tickHeadTurn` and `setYBodyRot`: the body and head follow the yaw.
pub fn tick_head_turn(m: &mut MobData, y_rot: f32) -> f32 {
    m.y_body_rot = y_rot;
    y_rot
}

/// The fall sounds of an armor stand.
pub fn fall_sound(m: &MobData) -> Option<&'static str> {
    (m.kind == Kind::ArmorStand).then_some("minecraft:entity.armor_stand.fall")
}

/// `ArmorStand.pushEntities`: only rideable minecarts close to its centre are pushed.
pub fn push_minecarts(ents: &mut Entities, id: EntityId) {
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let (aabb, pos) = (e.aabb(), e.pos());
    let mut carts = Vec::new();
    ents.store.for_each_non_player_in(&aabb, |o| {
        if o != id
            && ents
                .minecart(o)
                .is_some_and(|m| m.kind == crate::minecart::MinecartKind::Plain)
            && ents
                .store
                .get(o)
                .is_some_and(|c| c.aabb().intersects(&aabb) && c.pos().distance_to_sqr(pos) <= 0.2)
        {
            carts.push(o);
        }
    });
    for c in carts {
        crate::minecart::push_by(ents, c, id);
    }
}
