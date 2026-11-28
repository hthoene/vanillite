//! The location based enchantment effects of living entities, read from the enchantment
//! definitions: `minecraft:tick` (`EnchantmentHelper.tickEffects`, every tick) and
//! `minecraft:location_changed` (`runLocationChangedEffects`: a new block position, a landing,
//! new equipment), with the active effects of each slot (`activeLocationDependentEnchantments`)
//! and the attribute modifiers of the active `attribute` effects (Frost Walker's
//! `replace_disk`, Soul Speed's speed, particles, sound and durability).

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{EntityFlags, EntityMut};

use super::{EnchantContext, Enchantment, Enchantments, EntityEffect};
use crate::effect::Operation;
use crate::effect::attribute::Attribute;
use crate::item::{EquipmentSlot, ItemStack};
use crate::level::Level;
use crate::player::PlayerData;
use crate::predicate::EntityFacts;
use crate::survival::player::Env;

/// `EquipmentSlot.VALUES`.
const SLOTS: [EquipmentSlot; 8] = [
    EquipmentSlot::MainHand,
    EquipmentSlot::OffHand,
    EquipmentSlot::Feet,
    EquipmentSlot::Legs,
    EquipmentSlot::Chest,
    EquipmentSlot::Head,
    EquipmentSlot::Body,
    EquipmentSlot::Saddle,
];

/// `EquipmentSlot.getSerializedName`.
fn slot_name(slot: EquipmentSlot) -> &'static str {
    match slot {
        EquipmentSlot::MainHand => "mainhand",
        EquipmentSlot::OffHand => "offhand",
        EquipmentSlot::Feet => "feet",
        EquipmentSlot::Legs => "legs",
        EquipmentSlot::Chest => "chest",
        EquipmentSlot::Head => "head",
        EquipmentSlot::Body => "body",
        EquipmentSlot::Saddle => "saddle",
    }
}

/// An active location based effect: the slot, the enchantment (registry id) and the index of
/// the effect in its `location_changed` list, with the level it became active with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Active {
    pub slot: EquipmentSlot,
    pub enchantment: u32,
    pub effect: usize,
    pub level: i32,
}

/// The tracked state of a living entity's location based enchantments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocationState {
    /// `lastPos` of `LivingEntity.baseTick`.
    pub last_pos: Option<BlockPos>,
    /// The equipment with location based effects as last seen (`collectEquipmentChanges`),
    /// by slot ordinal.
    pub last: [Option<ItemStack>; 8],
    /// `activeLocationDependentEnchantments`.
    pub active: Vec<Active>,
}

impl LocationState {
    /// The attribute modifiers of the active `attribute` effects for `attribute`:
    /// `(id, operation, amount)`.
    pub fn modifiers(&self, attribute: &str, out: &mut Vec<(String, u8, f64)>) {
        if self.active.is_empty() {
            return;
        }
        let data = crate::mob::attack::combat_data();
        for a in &self.active {
            let Some(effect) = enchantment(&data.enchants, a.enchantment)
                .and_then(|e| e.location_changed.get(a.effect))
            else {
                continue;
            };
            attribute_effects(&effect.effect, &mut |m| {
                if m.attribute == attribute {
                    out.push((
                        format!("{}/{}", m.id, slot_name(a.slot)),
                        m.operation,
                        f64::from(m.amount.calculate(a.level)),
                    ));
                }
            });
        }
    }
}

/// The `attribute` effects of an effect (inside `all_of` too).
fn attribute_effects(effect: &EntityEffect, f: &mut dyn FnMut(&super::LocationAttribute)) {
    match effect {
        EntityEffect::Attribute(m) => f(m),
        EntityEffect::AllOf(list) => {
            for e in list {
                attribute_effects(e, f);
            }
        }
        _ => {}
    }
}

fn enchantment(enchants: &Enchantments, id: u32) -> Option<&Enchantment> {
    enchants.list.get(usize::try_from(id).ok()?)
}

/// The enchantments of a stack in a slot that work there (`runIterationOnItem` with a slot).
fn in_slot<'a>(
    enchants: &'a Enchantments,
    stack: &ItemStack,
    slot: EquipmentSlot,
) -> Vec<(u32, &'a Enchantment, i32)> {
    if stack.is_empty() {
        return Vec::new();
    }
    crate::item::enchantments(stack)
        .into_iter()
        .filter(|&(_, level)| level > 0)
        .filter_map(|(id, level)| Some((id, enchantment(enchants, id)?, level)))
        .filter(|(_, e, _)| e.works_in(slot))
        .collect()
}

/// A living entity whose location based effects run.
trait Living {
    fn stack(&self, slot: EquipmentSlot) -> &ItemStack;
    fn state(&mut self) -> &mut LocationState;
    /// `this` of the loot context.
    fn facts(&self) -> Option<EntityFacts>;
    fn pos(&self) -> Vec3;
    fn level_random(&mut self) -> &mut LegacyRandom;
    fn context<'a>(
        &'a self,
        tags: &'a dyn crate::predicate::EntityTags,
        this: Option<EntityFacts>,
    ) -> EnchantContext<'a>;
    /// `EnchantmentEntityEffect.apply` (or `onChangedBlock`) of an effect at the position.
    fn run(&mut self, effect: &EntityEffect, level: i32, slot: EquipmentSlot, pos: Vec3);
}

/// `EnchantmentHelper.tickEffects`: the `tick` effects of every slot whose requirements
/// hold.
fn tick_effects(l: &mut dyn Living) {
    let data = crate::mob::attack::combat_data();
    for slot in SLOTS {
        let list = in_slot(&data.enchants, l.stack(slot), slot);
        for (_, e, level) in list {
            for c in &e.tick {
                let pos = l.pos();
                let this = l.facts();
                let holds = c.requirements.as_ref().is_none_or(|r| {
                    // The loot context's random is the level's.
                    let mut random = l.level_random().clone();
                    let holds = r.test(&l.context(&*data, this), level, &mut random);
                    *l.level_random() = random;
                    holds
                });
                if holds {
                    l.run(&c.effect, level, slot, pos);
                }
            }
        }
    }
}

/// `Enchantment.runLocationChangedEffects` of the enchantments of one slot.
fn location_changed_slot(l: &mut dyn Living, slot: EquipmentSlot) {
    let data = crate::mob::attack::combat_data();
    let list = in_slot(&data.enchants, l.stack(slot), slot);
    for (id, e, level) in list {
        for (i, c) in e.location_changed.iter().enumerate() {
            let was = l
                .state()
                .active
                .iter()
                .any(|a| a.slot == slot && a.enchantment == id && a.effect == i);
            let pos = l.pos();
            let this = l.facts();
            let holds = c.requirements.as_ref().is_none_or(|r| {
                let mut random = l.level_random().clone();
                let holds = {
                    let mut cx = l.context(&*data, this);
                    cx.enchantment_active = Some(was);
                    r.test(&cx, level, &mut random)
                };
                *l.level_random() = random;
                holds
            });
            if holds {
                if !was {
                    l.state().active.push(Active {
                        slot,
                        enchantment: id,
                        effect: i,
                        level,
                    });
                }
                // `onChangedBlock`: an attribute is added when it becomes active; the entity
                // effects run every time.
                l.run(&c.effect, level, slot, pos);
            } else if was {
                l.state()
                    .active
                    .retain(|a| !(a.slot == slot && a.enchantment == id && a.effect == i));
            }
        }
    }
}

/// `EnchantmentHelper.runLocationChangedEffects(level, entity)`: every slot.
fn location_changed(l: &mut dyn Living) {
    for slot in SLOTS {
        location_changed_slot(l, slot);
    }
}

/// `stopLocationBasedEffects(previous, slot)`: the effects of the slot end (their attribute
/// modifiers go).
fn stop_slot(l: &mut dyn Living, slot: EquipmentSlot) -> bool {
    let state = l.state();
    let before = state.active.len();
    state.active.retain(|a| a.slot != slot);
    state.active.len() != before
}

/// `collectEquipmentChanges` for the location based effects: a slot whose stack with such
/// effects changed (or went) stops its effects, then the new stack runs its
/// `location_changed` effects. Returns whether active effects changed.
fn equipment_changes(l: &mut dyn Living) -> bool {
    let data = crate::mob::attack::combat_data();
    let mut changed = false;
    for slot in SLOTS {
        let i = usize::from(slot.ordinal());
        let stack = l.stack(slot);
        let current = data
            .enchants
            .has_location_effects(stack)
            .then(|| stack.clone());
        let Some(last) = l.state().last.get_mut(i) else {
            continue;
        };
        if *last == current {
            continue;
        }
        let had = last.is_some();
        *last = current.clone();
        if had {
            changed |= stop_slot(l, slot);
        }
        if current.is_some() {
            let before = l.state().active.clone();
            location_changed_slot(l, slot);
            changed |= l.state().active != before;
        }
    }
    changed
}

/// The movement block of `getBlockPosBelowThatAffectsMyMovement` (0.500001 below the feet).
fn below_movement(pos: Vec3) -> BlockPos {
    BlockPos::new(
        vanillite_base::math::floor(pos.x),
        vanillite_base::math::floor(pos.y - 0.500_001),
        vanillite_base::math::floor(pos.z),
    )
}

fn block_pos(pos: Vec3) -> BlockPos {
    BlockPos::new(
        vanillite_base::math::floor(pos.x),
        vanillite_base::math::floor(pos.y),
        vanillite_base::math::floor(pos.z),
    )
}

// ---------------------------------------------------------------------------------------------
// Players.

struct PlayerLiving<'a, 'b, 'c> {
    entity: &'a mut EntityMut<'b>,
    player: &'a mut PlayerData,
    env: &'a mut Env<'c>,
}

impl Living for PlayerLiving<'_, '_, '_> {
    fn stack(&self, slot: EquipmentSlot) -> &ItemStack {
        self.player.inventory.equipment(slot)
    }

    fn state(&mut self) -> &mut LocationState {
        &mut self.player.survival.location
    }

    fn facts(&self) -> Option<EntityFacts> {
        let e = self.entity.as_ref();
        let flags = e.flags();
        let s = &self.player.survival;
        Some(EntityFacts {
            kind: Some(e.kind()),
            on_fire: s.fire_ticks > 0,
            sneaking: flags.contains(EntityFlags::CROUCHING),
            sprinting: flags.contains(EntityFlags::SPRINTING),
            swimming: flags.contains(EntityFlags::SWIMMING),
            on_ground: flags.contains(EntityFlags::ON_GROUND),
            flying: s.flying || flags.contains(EntityFlags::FALL_FLYING),
            fall_flying: flags.contains(EntityFlags::FALL_FLYING),
            fall_distance: s.fall_distance,
            in_water: s.fluids.in_water(),
            // The type of the vehicle is not known here: riding at all is.
            vehicle: flags.contains(EntityFlags::PASSENGER).then(|| e.kind()),
            player: Some((
                crate::mob::attack::game_mode_id(self.player.game_mode),
                s.food.food_level,
            )),
            tick_count: s.tick_count,
            movement: self.player.connection.movement.known_movement,
            movement_block: self.env.blocks.block_state(below_movement(e.pos())),
            ..Default::default()
        })
    }

    fn pos(&self) -> Vec3 {
        self.entity.as_ref().pos()
    }

    fn level_random(&mut self) -> &mut LegacyRandom {
        self.env.random
    }

    fn context<'a>(
        &'a self,
        tags: &'a dyn crate::predicate::EntityTags,
        this: Option<EntityFacts>,
    ) -> EnchantContext<'a> {
        let pos = self.pos();
        let at = block_pos(pos);
        EnchantContext {
            types: self.env.types,
            tags,
            this,
            attacker: None,
            direct_attacker: None,
            attacking_player: None,
            source: None,
            // The level's weather is not known here; rain at the position stands in.
            raining: (self.env.raining_at)(at),
            thundering: false,
            location: self.env.blocks.block_state(at).map(|s| (s, false)),
            enchantment_active: None,
            block_state: None,
        }
    }

    fn run(&mut self, effect: &EntityEffect, level: i32, slot: EquipmentSlot, pos: Vec3) {
        run_player_effect(self, effect, level, slot, pos);
    }
}

/// An effect on a player from its own equipment.
fn run_player_effect(
    l: &mut PlayerLiving<'_, '_, '_>,
    effect: &EntityEffect,
    level: i32,
    slot: EquipmentSlot,
    pos: Vec3,
) {
    let id = l.entity.as_ref().id();
    match effect {
        EntityEffect::Ignite(seconds) => {
            #[allow(clippy::cast_possible_truncation)]
            let ticks = (seconds.calculate(level) * 20.0).floor() as i32;
            let burning = crate::mob::attack::player_data_attribute(
                l.player,
                "minecraft:burning_time",
                1.0,
                0.0,
                1024.0,
            );
            let ticks = crate::mob::attack::burning_ticks(ticks, burning);
            let s = &mut l.player.survival;
            if s.fire_ticks < ticks {
                s.fire_ticks = ticks;
            }
            s.ticks_frozen = 0;
        }
        EntityEffect::ApplyMobEffect {
            effects,
            min_duration,
            max_duration,
            min_amplifier,
            max_amplifier,
        } => {
            let Some(n) = i32::try_from(effects.len()).ok().filter(|&n| n > 0) else {
                return;
            };
            let r = &mut l.player.survival.random;
            let Some(&kind) = usize::try_from(r.next_int_bounded(n))
                .ok()
                .and_then(|i| effects.get(i))
            else {
                return;
            };
            let between =
                |r: &mut LegacyRandom, min: f32, max: f32| r.next_float() * (max - min) + min;
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let ticks = (between(
                r,
                min_duration.calculate(level),
                max_duration.calculate(level),
            ) * 20.0
                + 0.5)
                .floor() as i32;
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let amp = ((between(
                r,
                min_amplifier.calculate(level),
                max_amplifier.calculate(level),
            ) + 0.5)
                .floor() as i32)
                .max(0);
            let instance = crate::effect::EffectInstance::new(kind, ticks, amp);
            let entity_kind = l.entity.as_ref().kind();
            let mut host = crate::effect::living::PlayerHost {
                entity: &mut *l.entity,
                player: l.player,
                env: l.env,
                players: None,
                names: &[],
            };
            crate::effect::consume::add_effect(entity_kind, instance, &mut host);
        }
        EntityEffect::DamageEntity {
            damage_type,
            min_damage,
            max_damage,
        } => {
            let (min, max) = (min_damage.calculate(level), max_damage.calculate(level));
            let amount = l.player.survival.random.next_float() * (max - min) + min;
            let Some(kind) = l.env.types.kind(damage_type) else {
                return;
            };
            let source = crate::survival::damage::DamageSource {
                cause: Some(id),
                direct: Some(id),
                ..crate::survival::damage::DamageSource::of(kind)
            };
            crate::survival::player::hurt(l.entity, l.player, l.env, source, amount);
        }
        EntityEffect::ChangeItemDamage(amount) => {
            #[allow(clippy::cast_possible_truncation)]
            let amount = amount.calculate(level) as i32;
            for _ in 0..amount.max(0) {
                crate::elytra::hurt_slot(l.entity, l.player, l.env, slot);
            }
        }
        EntityEffect::ApplyExhaustion(amount) => {
            crate::survival::player::cause_food_exhaustion(l.player, amount.calculate(level));
        }
        EntityEffect::PlaySound {
            sounds,
            volume,
            pitch,
        } => {
            let Some(sound) = EntityEffect::sound_of_level(sounds, level) else {
                return;
            };
            let r = &mut l.player.survival.random;
            let (volume, pitch) = (volume.sample(r), pitch.sample(r));
            crate::survival::play_sound(
                l.env.out,
                None,
                pos,
                sound,
                SoundSource::Players,
                volume,
                pitch,
                crate::survival::sound_seed(),
            );
        }
        EntityEffect::SpawnParticles(p) => {
            let dims = l.entity.as_ref().dims();
            let movement = l.player.connection.movement.known_movement;
            let packet = p.packet(
                pos,
                movement,
                (dims.width, dims.height),
                &mut l.player.survival.random,
            );
            l.player
                .survival
                .world_actions
                .push(crate::mob::WorldAction::Particles(Box::new(packet)));
        }
        EntityEffect::World(w) => {
            let action = super::world::action(w, level, id, pos, Some(id), l.env.types);
            l.player
                .survival
                .world_actions
                .push(crate::mob::WorldAction::Enchant(Box::new(action)));
        }
        EntityEffect::WindBurst {
            radius,
            knockback,
            sound,
        } => {
            l.player
                .survival
                .world_actions
                .push(crate::mob::WorldAction::WindBurst(crate::mob::WindBurst {
                    source: None,
                    pos,
                    radius: *radius,
                    knockback: f64::from(knockback.calculate(level)),
                    sound: crate::mob::attack::static_sound(sound),
                }));
        }
        EntityEffect::AllOf(list) => {
            for e in list {
                run_player_effect(l, e, level, slot, pos);
            }
        }
        // The impulse of a player's own equipment is not applied here.
        EntityEffect::ApplyImpulse { .. }
        | EntityEffect::Attribute(_)
        | EntityEffect::Unsupported => {}
    }
}

/// The `baseTick` part of a living player: `EnchantmentHelper.tickEffects`, then
/// `onChangedBlock` when the block position changed, then the equipment changes. Returns
/// whether the attributes must be synced again.
pub fn base_tick(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) -> bool {
    let has_any = crate::player::play::game_data().is_some_and(|d| {
        SLOTS.iter().any(|&s| {
            d.enchants
                .has_location_effects(player.inventory.equipment(s))
        })
    });
    let pos = entity.as_ref().pos();
    let block = block_pos(pos);
    let moved = player.survival.location.last_pos != Some(block);
    player.survival.location.last_pos = Some(block);
    let state = &player.survival.location;
    if !has_any && state.active.is_empty() && state.last.iter().all(Option::is_none) {
        return false;
    }
    let mut l = PlayerLiving {
        entity,
        player,
        env,
    };
    tick_effects(&mut l);
    let before = l.player.survival.location.active.clone();
    // `ServerPlayer.onChangedBlock`: not for spectators.
    if moved && l.player.game_mode != vanillite_config::GameMode::Spectator {
        location_changed(&mut l);
    }
    equipment_changes(&mut l);
    l.player.survival.location.active != before
}

/// `LivingEntity.onChangedBlock` of a landing (`checkFallDamage`). Returns whether the
/// active effects changed.
pub fn on_changed_block(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
) -> bool {
    if player.game_mode == vanillite_config::GameMode::Spectator {
        return false;
    }
    let before = player.survival.location.active.clone();
    let mut l = PlayerLiving {
        entity,
        player,
        env,
    };
    location_changed(&mut l);
    l.player.survival.location.active != before
}

/// The modifiers of the active location based `attribute` effects of a player (Soul Speed:
/// movement speed and movement efficiency).
pub fn location_modifiers(player: &PlayerData, out: &mut crate::effect::attribute::Modifiers) {
    let state = &player.survival.location;
    if state.active.is_empty() {
        return;
    }
    let mut list = Vec::new();
    for a in &vanillite_data::entity::ATTRIBUTES {
        list.clear();
        state.modifiers(a.name, &mut list);
        let Some(id) = crate::item::attribute_id(a.name).and_then(|i| u8::try_from(i).ok()) else {
            continue;
        };
        for (key, op, amount) in &list {
            let operation = match op {
                0 => Operation::AddValue,
                1 => Operation::AddMultipliedBase,
                _ => Operation::AddMultipliedTotal,
            };
            out.set(Attribute(id), key, *amount, operation);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Mobs.

struct MobLiving<'t, 'a, 'e> {
    t: &'t mut crate::mob::Tick<'a, 'e>,
}

impl Living for MobLiving<'_, '_, '_> {
    fn stack(&self, slot: EquipmentSlot) -> &ItemStack {
        self.t
            .ents
            .mob(self.t.id)
            .map_or(&ItemStack::EMPTY, |m| m.equipment.get(slot))
    }

    fn state(&mut self) -> &mut LocationState {
        let m = self.t.mob();
        m.location.get_or_insert_with(Box::default)
    }

    fn facts(&self) -> Option<EntityFacts> {
        let mut f = crate::mob::attack::facts(self.t.ents, self.t.id)?;
        let below = below_movement(self.pos());
        f.movement_block = Some(self.t.level.block(below));
        Some(f)
    }

    fn pos(&self) -> Vec3 {
        self.t
            .ents
            .store
            .get(self.t.id)
            .map_or(Vec3::ZERO, |e| e.pos())
    }

    fn level_random(&mut self) -> &mut LegacyRandom {
        self.t.env.random
    }

    fn context<'a>(
        &'a self,
        tags: &'a dyn crate::predicate::EntityTags,
        this: Option<EntityFacts>,
    ) -> EnchantContext<'a> {
        let at = block_pos(self.pos());
        let level = self.t.level;
        EnchantContext {
            types: self.t.env.types,
            tags,
            this,
            attacker: None,
            direct_attacker: None,
            attacking_player: None,
            source: None,
            raining: level.is_raining(),
            thundering: level.is_thundering(),
            location: Some((level.block(at), crate::mob::can_see_sky(level, at))),
            enchantment_active: None,
            block_state: None,
        }
    }

    fn run(&mut self, effect: &EntityEffect, level: i32, slot: EquipmentSlot, pos: Vec3) {
        let t = &mut *self.t;
        crate::mob::attack::run_entity_effect_at(
            t.level,
            t.ents,
            t.env,
            t.cx,
            effect,
            level,
            t.id,
            pos,
            Some(t.id),
            Some(slot),
        );
    }
}

/// Whether a mob may have location based effects (equipment with them or effects still
/// active).
fn mob_has_any(t: &crate::mob::Tick<'_, '_>) -> bool {
    let Some(m) = t.ents.mob(t.id) else {
        return false;
    };
    if m.location
        .as_ref()
        .is_some_and(|l| !l.active.is_empty() || l.last.iter().any(Option::is_some))
    {
        return true;
    }
    // Most mobs carry nothing enchanted: no look at the data packs then.
    if SLOTS.iter().all(|&s| {
        let stack = m.equipment.get(s);
        stack.is_empty() || crate::item::enchantments(stack).is_empty()
    }) {
        return false;
    }
    crate::player::play::game_data().is_some_and(|d| {
        SLOTS
            .iter()
            .any(|&s| d.enchants.has_location_effects(m.equipment.get(s)))
    })
}

/// `EnchantmentHelper.tickEffects` of a mob (the start of `LivingEntity.baseTick`).
pub fn mob_tick_effects(t: &mut crate::mob::Tick<'_, '_>) {
    if mob_has_any(t) {
        tick_effects(&mut MobLiving { t });
    }
}

/// `onChangedBlock` of a mob (a new block position or a landing).
pub fn mob_changed_block(t: &mut crate::mob::Tick<'_, '_>) {
    if mob_has_any(t) {
        location_changed(&mut MobLiving { t });
    }
}

/// The location based part of a mob's `detectEquipmentUpdates`.
pub fn mob_equipment_changes(t: &mut crate::mob::Tick<'_, '_>) {
    if mob_has_any(t) {
        equipment_changes(&mut MobLiving { t });
    }
}

// ---------------------------------------------------------------------------------------------

/// `Level.isUnobstructed(state, pos, CollisionContext.empty())`.
pub(crate) fn unobstructed(level: &Level, state: BlockState, pos: BlockPos) -> bool {
    let (dx, dy, dz) = (f64::from(pos.x), f64::from(pos.y), f64::from(pos.z));
    state.collision_shape().iter().all(|b| {
        let shape = Aabb::new(
            b[0] + dx,
            b[1] + dy,
            b[2] + dz,
            b[3] + dx,
            b[4] + dy,
            b[5] + dz,
        );
        let mut blocked = false;
        level.entities.store.for_each_in(&shape, false, |id| {
            blocked |= crate::block::dispenser::blocks_building(level, id);
        });
        !blocked
    })
}

/// Frost Walker's `damage_immunity` (`EnchantmentHelper.isImmuneToDamage`): boots with Frost
/// Walker ignore `#burn_from_stepping` damage (magma blocks, campfires, hot sulfur cubes)
/// that does not bypass invulnerability.
pub fn frost_walker_immune(
    types: &crate::survival::damage::DamageTypes,
    kind: crate::survival::damage::DamageKind,
    boots: &crate::item::ItemStack,
) -> bool {
    use crate::survival::damage::tag;
    if boots.is_empty() || types.is(kind, tag::BYPASSES_INVULNERABILITY) {
        return false;
    }
    let stepping = kind == types.hot_floor
        || kind == types.campfire
        || types.kind("minecraft:sulfur_cube_hot") == Some(kind);
    stepping
        && crate::player::play::game_data()
            .is_some_and(|d| d.enchantment_level(boots, "minecraft:frost_walker") > 0)
}
