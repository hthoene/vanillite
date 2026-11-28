//! Status effects on players and mobs: the [`EffectHost`]s of both, adding effects from
//! anywhere (potions, arrows, enchantments, commands), the per-tick sync (own client packets,
//! invisibility and glowing flags, particles, attributes) and the death and hurt hooks of the
//! effects (`onMobHurt`, `onMobRemoved`).

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::entity::{EntityTypeId, ids};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{
    RemoveMobEffect, SoundSource, UpdateAttributes, UpdateMobEffect,
};
use vanillite_world::entity::{EntityFlags, EntityId, EntityMut};

use super::attribute::{self, Attribute};
use super::potion::PotionContents;
use super::{ActiveEffects, EffectEvent, EffectHost, EffectInstance, HurtKind, MobEffect, active};
use crate::entities::Entities;
use crate::level::Level;
use crate::mob::{Attrs, MobContext, Tick, WorldAction};
use crate::player::PlayerData;
use crate::survival::combat::{AttackerName, SourceNames};
use crate::survival::damage::DamageSource;
use crate::survival::player::{self, Env};
use crate::survival::{Effect, play_sound};

/// The damage source of an effect's damage.
fn source_of(env: &Env<'_>, kind: HurtKind) -> DamageSource {
    match kind {
        HurtKind::Magic => DamageSource::of(env.types.magic),
        HurtKind::Wither => DamageSource::of(env.types.wither),
        HurtKind::IndirectMagic { direct, owner } => DamageSource {
            cause: owner,
            direct,
            ..DamageSource::of(env.types.indirect_magic)
        },
    }
}

/// A player as the target of its effects.
pub struct PlayerHost<'a, 'b, 'c> {
    pub entity: &'a mut EntityMut<'b>,
    pub player: &'a mut PlayerData,
    pub env: &'a mut Env<'c>,
    /// The players of the world by UUID, for the pvp rule of player-caused damage; `None`
    /// where effects hurt without a causing entity (ticking effects).
    pub players: Option<&'a vanillite_base::hash::FxHashMap<u128, EntityId>>,
    /// The names of the entities that may cause the damage, resolved before the host took the
    /// player out of the entities (death messages).
    pub names: &'a [(EntityId, AttackerName)],
}

impl std::fmt::Debug for PlayerHost<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlayerHost").finish_non_exhaustive()
    }
}

impl EffectHost for PlayerHost<'_, '_, '_> {
    fn effects(&mut self) -> &mut ActiveEffects {
        &mut self.player.survival.effects
    }
    fn health(&self) -> f32 {
        self.player.survival.health
    }
    fn max_health(&self) -> f32 {
        self.player.survival.max_health()
    }
    fn heal(&mut self, amount: f32) {
        player::heal(self.entity, self.player, amount);
    }
    fn hurt(&mut self, kind: HurtKind, amount: f32) -> bool {
        let mut source = source_of(self.env, kind);
        // `resolvePlayerResponsibleForDamage`: a causing player.
        source.responsible_player = source
            .cause
            .filter(|c| self.players.is_some_and(|ps| ps.values().any(|p| p == c)));
        // `ServerPlayer.hurtServer`: `canHarmPlayer` of a causing player (the pvp rule).
        if !self.env.rules.bool(crate::survival::rules::Rule::PVP)
            && let (Some(cause), Some(players)) = (source.cause, self.players)
            && players.values().any(|&p| p == cause)
        {
            return false;
        }
        // The plugins' damage and death events see a causing or direct player (the UUIDs of other
        // entities are not at hand here).
        let attackers = match self.players {
            Some(players) if player::wants_attackers(self.env.hooks) => {
                let uuid_of = |id: Option<EntityId>| {
                    id.and_then(|id| players.iter().find(|(_, p)| **p == id).map(|(u, _)| *u))
                };
                (uuid_of(source.cause), uuid_of(source.direct))
            }
            _ => (None, None),
        };
        let own = self.entity.as_ref().id();
        let name = |id: Option<EntityId>| {
            let id = id?;
            if let Some((_, n)) = self.names.iter().find(|(i, _)| *i == id) {
                return Some(n.clone());
            }
            (id == own).then(|| AttackerName {
                name: (self.env.display_name)(
                    &self.player.profile.name,
                    self.entity.as_ref().uuid(),
                ),
                item: crate::mob::combat::custom_named_item(self.player.inventory.selected_item()),
            })
        };
        let names = SourceNames {
            cause: name(source.cause),
            direct: name(source.direct).map(|a| a.name),
            responsible: name(source.responsible_player).map(|a| a.name),
        };
        player::hurt_with(
            self.entity,
            self.player,
            self.env,
            source,
            amount,
            None,
            attackers,
            names,
        )
    }
    fn absorption(&self) -> f32 {
        self.player.survival.absorption
    }
    fn set_absorption(&mut self, amount: f32) {
        let max = self.player.survival.max_absorption();
        let amount = amount.clamp(0.0, max);
        if amount != self.player.survival.absorption {
            self.player.survival.absorption = amount;
            self.entity.mark_data_dirty();
        }
    }
    fn inverted_heal_and_harm(&self) -> bool {
        false
    }
    fn tick_count(&self) -> i32 {
        self.player.survival.tick_count
    }
    fn exhaust(&mut self, amount: f32) {
        player::cause_food_exhaustion(self.player, amount);
    }
    fn eat(&mut self, food: i32, saturation: f32) {
        self.player
            .survival
            .food
            .eat_with_modifier(food, saturation);
    }
    fn play_sound(&mut self, sound: &str) {
        let pos = self.entity.as_ref().pos();
        let seed = crate::survival::sound_seed();
        play_sound(
            self.env.out,
            None,
            pos,
            sound,
            SoundSource::Players,
            1.0,
            1.0,
            seed,
        );
    }
    fn bad_omen_tick(&mut self, amplifier: i32) -> bool {
        // `BadOmenMobEffect.applyEffectTick`: in a village whose raid can still grow the omen
        // becomes a Raid Omen at the player's position.
        let view = self.player.survival.raid_omen.view;
        if self.player.game_mode != vanillite_config::GameMode::Spectator
            && self.env.difficulty != crate::survival::rules::Difficulty::Peaceful
            && view.in_village
            && !view.raid_full
        {
            self.player
                .survival
                .effects
                .add_deferred(EffectInstance::new(MobEffect::RAID_OMEN, 600, amplifier));
            let p = self.entity.as_ref().pos();
            self.player.survival.raid_omen.position = Some(BlockPos::containing(p.x, p.y, p.z));
            return false;
        }
        true
    }
    fn raid_omen_tick(&mut self, amplifier: i32) -> bool {
        // `RaidOmenMobEffect.applyEffectTick` on its last tick: the world starts the raid.
        if self.player.game_mode == vanillite_config::GameMode::Spectator {
            return true;
        }
        let omen = &mut self.player.survival.raid_omen;
        if let Some(pos) = omen.position.take() {
            omen.start_raid = Some(pos);
            omen.start_amplifier = amplifier;
            return false;
        }
        true
    }
}

/// A mob as the target of its effects.
pub struct MobHost<'t, 'a, 'e> {
    pub t: &'t mut Tick<'a, 'e>,
}

impl std::fmt::Debug for MobHost<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobHost").field("id", &self.t.id).finish()
    }
}

impl EffectHost for MobHost<'_, '_, '_> {
    fn effects(&mut self) -> &mut ActiveEffects {
        &mut self.t.mob().effects
    }
    fn health(&self) -> f32 {
        self.t.mob_ref().health
    }
    fn max_health(&self) -> f32 {
        #[allow(clippy::cast_possible_truncation)]
        let max = self.t.mob_ref().attrs.max_health as f32;
        max
    }
    fn heal(&mut self, amount: f32) {
        let max = self.max_health();
        let m = self.t.mob();
        if m.health > 0.0 {
            let health = (m.health + amount).clamp(0.0, max);
            if health != m.health {
                m.health = health;
                self.t.core_mut().mark_data_dirty();
            }
        }
    }
    fn hurt(&mut self, kind: HurtKind, amount: f32) -> bool {
        let source = source_of(self.t.env, kind);
        let id = self.t.id;
        crate::mob::combat::hurt(
            self.t.level,
            self.t.ents,
            self.t.env,
            self.t.cx,
            id,
            source,
            amount,
        )
    }
    fn absorption(&self) -> f32 {
        self.t.mob_ref().absorption
    }
    fn set_absorption(&mut self, amount: f32) {
        let m = self.t.mob();
        let max = m.effects.attribute(Attribute::MAX_ABSORPTION, 0.0);
        #[allow(clippy::cast_possible_truncation)]
        let amount = amount.clamp(0.0, max as f32);
        m.absorption = amount;
    }
    fn inverted_heal_and_harm(&self) -> bool {
        super::inverted_heal_and_harm(self.t.core().kind())
    }
    fn tick_count(&self) -> i32 {
        self.t.mob_ref().tick_count
    }
    fn play_sound(&mut self, sound: &str) {
        // `onEffectAdded`: the mob's own sound source (`getSoundSource`).
        let pos = self.t.pos();
        let seed = crate::survival::sound_seed();
        let source = self.t.mob_ref().kind.sound_source();
        play_sound(self.t.env.out, None, pos, sound, source, 1.0, 1.0, seed);
    }
}

/// `tickEffects` of a player, with the client sync.
pub fn tick_player(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    if !player.survival.effects.is_empty() {
        let mut host = PlayerHost {
            entity,
            player,
            env,
            players: None,
            names: &[],
        };
        active::tick(&mut host);
        sync_player(host.entity, host.player, host.env);
    } else {
        sync_player(entity, player, env);
    }
}

/// `tickEffects` of the ticking mob, with the sync of its data.
pub fn tick_mob(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().effects.is_empty() {
        active::tick(&mut MobHost { t });
    }
    if !t.exists() {
        return;
    }
    let id = t.id;
    sync_mob(t.ents, t.env.out, id);
}

/// `sendEffectToPassengers` / `onEffectsRemoved` of a mob: its effect events as packets to
/// the players riding it.
fn send_to_passengers(ents: &mut Entities, out: &mut Vec<Effect>, id: EntityId) {
    let riders: Vec<EntityId> = ents
        .riding
        .passengers_of(id)
        .iter()
        .copied()
        .filter(|&p| ents.player(p).is_some())
        .collect();
    let Some((network_id, pos)) = ents.store.get(id).map(|e| (e.network_id().0, e.pos())) else {
        return;
    };
    let Some(m) = ents.mob_mut(id) else {
        return;
    };
    let source = m.kind.sound_source();
    for event in m.effects.take_events() {
        let packet = match event {
            // `onEffectAdded` of an effect added outside the mob's tick.
            EffectEvent::Sound(sound) => {
                let seed = crate::survival::sound_seed();
                play_sound(out, None, pos, &sound, source, 1.0, 1.0, seed);
                continue;
            }
            EffectEvent::Update {
                instance, blend, ..
            } => SharedPacket::of(&update_packet(network_id, &instance, blend)),
            EffectEvent::Resend(instance) => {
                SharedPacket::of(&update_packet(network_id, &instance, false))
            }
            EffectEvent::Remove(effect) => SharedPacket::of(&RemoveMobEffect {
                entity_id: network_id,
                effect: i32::from(effect.0),
            }),
        };
        for &player in &riders {
            out.push(Effect::To {
                player,
                packet: std::sync::Arc::clone(&packet),
            });
        }
    }
}

/// `ServerPlayer.startRiding` (`sendActiveEffects` of a living vehicle) and `removeVehicle`
/// (a `remove_mob_effect` per effect): the vehicle's effects for its riding player.
pub fn send_vehicle_effects(ents: &mut Entities, player: EntityId, vehicle: EntityId, mount: bool) {
    let Some(network_id) = ents.store.get(vehicle).map(|e| e.network_id().0) else {
        return;
    };
    let Some(effects) = ents
        .mob(vehicle)
        .map(|m| m.effects.iter().cloned().collect::<Vec<_>>())
    else {
        return;
    };
    let Some(p) = ents.player_mut(player) else {
        return;
    };
    for e in &effects {
        if mount {
            p.connection.sink.send(&update_packet(network_id, e, false));
        } else {
            p.connection.sink.send(&RemoveMobEffect {
                entity_id: network_id,
                effect: i32::from(e.effect.0),
            });
        }
    }
}

/// Sends what changed in a player's effects: `update_mob_effect` / `remove_mob_effect` to its
/// client, the invisibility and glowing flags and particles to everyone, the changed
/// attributes to everyone tracking it and itself (`onAttributeUpdated` clamps health and
/// absorption first).
pub fn sync_player(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let network_id = entity.as_ref().network_id().0;
    let fx = &mut player.survival.effects;
    if fx.has_events() {
        let kind = crate::advancements::trigger::TriggerKind::EFFECTS_CHANGED;
        for event in fx.take_events() {
            match event {
                EffectEvent::Update {
                    instance,
                    blend,
                    source,
                } => {
                    player
                        .connection
                        .sink
                        .send(&update_packet(network_id, &instance, blend));
                    // `onEffectAdded`: where Levitation started.
                    if blend && instance.effect == MobEffect::LEVITATION {
                        crate::advancements::tracking::levitation_started(
                            player,
                            entity.as_ref().pos(),
                        );
                    }
                    // `onEffectAdded` / `onEffectUpdated`: `EFFECTS_CHANGED` with the source;
                    // its view is taken with the world's entities (`view::resolve_combat`).
                    if player.advancements.listens(kind) {
                        match source {
                            Some(source) => player.advancements.combat.push(
                                crate::advancements::view::PendingCombat::EffectsChanged { source },
                            ),
                            None => player
                                .advancements
                                .trigger(crate::advancements::trigger::Event::EffectsChanged(None)),
                        }
                    }
                }
                EffectEvent::Resend(instance) => {
                    player
                        .connection
                        .sink
                        .send(&update_packet(network_id, &instance, false));
                }
                EffectEvent::Remove(effect) => {
                    player.connection.sink.send(&RemoveMobEffect {
                        entity_id: network_id,
                        effect: i32::from(effect.0),
                    });
                    if effect == MobEffect::LEVITATION {
                        player.survival.tracking.levitation = None;
                    }
                    // `onEffectsRemoved`: `EFFECTS_CHANGED` without a source.
                    player.advancements.trigger_with(kind, || {
                        crate::advancements::trigger::Event::EffectsChanged(None)
                    });
                }
                EffectEvent::Sound(sound) => {
                    // `Level.playSound`: the seed comes from `soundSeedGenerator`, not the
                    // level random.
                    let seed = crate::survival::sound_seed();
                    play_sound(
                        env.out,
                        None,
                        entity.as_ref().pos(),
                        &sound,
                        SoundSource::Players,
                        1.0,
                        1.0,
                        seed,
                    );
                }
            }
        }
    }
    let fx = &mut player.survival.effects;
    if fx.take_dirty() {
        update_flags(entity, fx);
    }
    if !fx.modifiers.is_dirty() {
        return;
    }
    let dirty = fx.modifiers.take_dirty();
    if dirty.contains(&Attribute::SCALE) {
        let scale = super::attribute_map::scale_of(&fx.modifiers);
        let pose = entity.as_ref().pose();
        entity.set_dims(crate::player::scaled_dimensions(pose, scale));
    }
    let s = &mut player.survival;
    for &a in &dirty {
        if a == Attribute::MAX_HEALTH {
            let max = s.max_health();
            if s.health > max {
                s.health = max;
                entity.mark_data_dirty();
            }
        } else if a == Attribute::MAX_ABSORPTION {
            let max = s.max_absorption();
            if s.absorption > max {
                s.absorption = max;
                entity.mark_data_dirty();
            }
        }
    }
    // The equipment's modifiers go along: the client replaces an attribute's modifiers.
    if s.equipment_modifiers.is_empty() {
        push_attributes(entity, &dirty, &s.effects, ids::PLAYER, None, env.out);
    } else {
        let mut fx = s.effects.clone();
        for m in s.equipment_modifiers.iter() {
            fx.modifiers.set(m.attribute, &m.id, m.amount, m.operation);
        }
        push_attributes(entity, &dirty, &fx, ids::PLAYER, None, env.out);
    }
}

/// `update_attributes` of the syncable attributes in `dirty` to everyone tracking the entity
/// and itself.
fn push_attributes(
    entity: &EntityMut<'_>,
    dirty: &[Attribute],
    fx: &ActiveEffects,
    kind: EntityTypeId,
    base: Option<&Attrs>,
    out: &mut Vec<Effect>,
) {
    let synced: Vec<(Attribute, f64)> = dirty
        .iter()
        .filter(|a| a.syncable())
        .map(|&a| {
            let value = base
                .and_then(|b| attr_field_ref(b, a))
                .or_else(|| fx.modifiers.base(a))
                .unwrap_or_else(|| a.base_of(kind));
            (a, value)
        })
        .collect();
    if synced.is_empty() {
        return;
    }
    let e = entity.as_ref();
    out.push(Effect::Tracking {
        entity: e.id(),
        with_self: true,
        packet: SharedPacket::of(&UpdateAttributes {
            body: attribute::encode_update(e.network_id().0, &synced, &fx.modifiers),
        }),
    });
}

/// `collectEquipmentChanges` → `ItemStack.forEachModifier`: the attribute modifiers of a
/// player's equipment (item and enchantment modifiers per slot) with those of the active
/// location-based enchantments (Soul Speed).
pub fn player_equipment_modifiers(player: &PlayerData) -> attribute::Modifiers {
    let mut out = attribute::Modifiers::default();
    let mut list = Vec::new();
    for slot in crate::player::encode::EQUIPMENT_SLOTS {
        let stack = player.inventory.equipment(slot);
        if stack.is_empty() {
            continue;
        }
        for (i, a) in vanillite_data::entity::ATTRIBUTES.iter().enumerate() {
            let Ok(id) = u8::try_from(i) else {
                break;
            };
            list.clear();
            crate::item::attribute_modifiers(stack, slot, a.name, &mut list);
            for (n, &(op, amount)) in list.iter().enumerate() {
                let operation = match op {
                    0 => super::Operation::AddValue,
                    1 => super::Operation::AddMultipliedBase,
                    _ => super::Operation::AddMultipliedTotal,
                };
                let key = format!("vanillite:equipment/{}/{n}", slot.ordinal());
                out.set(Attribute(id), &key, amount, operation);
            }
        }
    }
    crate::enchant::location::location_modifiers(player, &mut out);
    out.take_dirty();
    out
}

/// The `update_attributes` of a player's attributes whose equipment modifiers changed (all
/// of them when `full`, after a join or respawn), with the effect modifiers; `None` when
/// nothing changed.
pub fn player_equipment_update(
    network_id: i32,
    player: &mut PlayerData,
    full: bool,
) -> Option<UpdateAttributes> {
    let new = player_equipment_modifiers(player);
    let old = &player.survival.equipment_modifiers;
    let mut changed: Vec<Attribute> = Vec::new();
    if full {
        // The base values a command or plugin set (`getSyncableAttributes`).
        changed.extend(player.survival.effects.modifiers.bases().map(|(a, _)| a));
    }
    for m in old.iter().chain(new.iter()) {
        if changed.contains(&m.attribute) {
            continue;
        }
        let a: Vec<_> = old.of(m.attribute).collect();
        let b: Vec<_> = new.of(m.attribute).collect();
        if full || a != b {
            changed.push(m.attribute);
        }
    }
    player.survival.equipment_modifiers = new;
    player.survival.equipment_dirty = false;
    let own = &player.survival.effects.modifiers;
    let synced: Vec<(Attribute, f64)> = changed
        .iter()
        .filter(|a| a.syncable())
        .map(|&a| (a, own.base(a).unwrap_or_else(|| a.base_of(ids::PLAYER))))
        .collect();
    if synced.is_empty() {
        return None;
    }
    let mut all = player.survival.effects.modifiers.clone();
    for m in player.survival.equipment_modifiers.iter() {
        all.set(m.attribute, &m.id, m.amount, m.operation);
    }
    Some(UpdateAttributes {
        body: attribute::encode_update(network_id, &synced, &all),
    })
}

/// `updateInvisibilityStatus` + `updateGlowingStatus`; the particles are part of the entity
/// data, so the data is marked dirty.
fn update_flags(entity: &mut EntityMut<'_>, fx: &ActiveEffects) {
    entity.set_flags(EntityFlags::INVISIBLE, fx.has(MobEffect::INVISIBILITY));
    // `isCurrentlyGlowing`: the effect or the `Glowing` tag.
    let tagged = entity.as_ref().flags().contains(EntityFlags::GLOWING_TAG);
    entity.set_flags(EntityFlags::GLOWING, tagged || fx.has(MobEffect::GLOWING));
    entity.mark_data_dirty();
}

/// `ClientboundUpdateMobEffectPacket(entityId, effect, blend)`.
pub fn update_packet(network_id: i32, e: &EffectInstance, blend: bool) -> UpdateMobEffect {
    let flags = u8::from(e.ambient)
        | (u8::from(e.visible) << 1)
        | (u8::from(e.show_icon) << 2)
        | (u8::from(blend) << 3);
    UpdateMobEffect {
        entity_id: network_id,
        effect: i32::from(e.effect.0),
        amplifier: e.amplifier(),
        duration: e.duration,
        flags,
    }
}

/// The `Attrs` field of an attribute effects modify.
fn attr_field(attrs: &mut Attrs, a: Attribute) -> Option<&mut f64> {
    Some(match a {
        Attribute::AIR_DRAG_MODIFIER => &mut attrs.air_drag_modifier,
        Attribute::ARMOR_TOUGHNESS => &mut attrs.armor_toughness,
        Attribute::ATTACK_KNOCKBACK => &mut attrs.attack_knockback,
        Attribute::EXPLOSION_KNOCKBACK_RESISTANCE => &mut attrs.explosion_knockback_resistance,
        Attribute::FLYING_SPEED => &mut attrs.flying_speed,
        Attribute::FRICTION_MODIFIER => &mut attrs.friction_modifier,
        Attribute::MOVEMENT_EFFICIENCY => &mut attrs.movement_efficiency,
        Attribute::STEP_HEIGHT => &mut attrs.step_height,
        Attribute::WATER_MOVEMENT_EFFICIENCY => &mut attrs.water_movement_efficiency,
        Attribute::MOVEMENT_SPEED => &mut attrs.movement_speed,
        Attribute::ATTACK_DAMAGE => &mut attrs.attack_damage,
        Attribute::MAX_HEALTH => &mut attrs.max_health,
        Attribute::SAFE_FALL_DISTANCE => &mut attrs.safe_fall_distance,
        Attribute::FOLLOW_RANGE => &mut attrs.follow_range,
        Attribute::KNOCKBACK_RESISTANCE => &mut attrs.knockback_resistance,
        Attribute::ARMOR => &mut attrs.armor,
        Attribute::GRAVITY => &mut attrs.gravity,
        Attribute::JUMP_STRENGTH => &mut attrs.jump_strength,
        Attribute::FALL_DAMAGE_MULTIPLIER => &mut attrs.fall_damage_multiplier,
        _ => return None,
    })
}

fn attr_field_ref(attrs: &Attrs, a: Attribute) -> Option<f64> {
    let mut copy = *attrs;
    attr_field(&mut copy, a).map(|v| *v)
}

/// Sync of a mob after its effects changed: flags and particles, the attribute values
/// (`attrs` carries the modified values, the unmodified ones wait in `base_attrs`), health and
/// absorption clamped, and the synced attributes to the trackers. Riding players hear of the
/// changes (`sendEffectToPassengers`).
pub fn sync_mob(ents: &mut Entities, out: &mut Vec<Effect>, id: EntityId) {
    if ents.mob(id).is_some_and(|m| m.effects.has_events()) {
        send_to_passengers(ents, out, id);
    }
    let Entities { store, mobs, .. } = ents;
    let Some(mut entity) = store.get_mut(id) else {
        return;
    };
    let row = entity.as_ref().kind_row();
    let kind = entity.as_ref().kind();
    let Some(m) = mobs.get_mut(row) else {
        return;
    };
    if m.effects.has_events() {
        m.effects.take_events();
    }
    if m.effects.take_dirty() {
        update_flags(&mut entity, &m.effects);
        // `ArmorStand.updateInvisibilityStatus` keeps its own `Invisible`; the `Glowing` tag
        // stays (`isCurrentlyGlowing`: the effect or the tag).
        if let Some(s) = crate::decoration::stand::state(m) {
            entity.set_flags(EntityFlags::INVISIBLE, s.invisible);
            if s.glowing {
                entity.set_flags(EntityFlags::GLOWING, true);
            }
        }
    }
    if !m.effects.modifiers.is_dirty() {
        return;
    }
    let dirty = m.effects.modifiers.take_dirty();
    let base = apply_mob_modifiers(m, &dirty);
    if m.health > 0.0 {
        entity.mark_data_dirty();
    }
    // `onAttributeUpdated(SCALE)`: `refreshDimensions`.
    if dirty.contains(&Attribute::SCALE) {
        let dims = crate::mob::kinds::mob_dimensions(m);
        if entity.as_ref().dims() != dims {
            entity.set_dims(dims);
        }
    }
    push_attributes(&entity, &dirty, &m.effects, kind, Some(&base), out);
}

/// Folds the effect modifiers of `dirty` attributes into the mob's `attrs` (the unmodified
/// values wait in `base_attrs` while modifiers are active) and clamps health and absorption
/// (`onAttributeUpdated`). Returns the unmodified values.
pub fn apply_mob_modifiers(m: &mut crate::mob::MobData, dirty: &[Attribute]) -> Attrs {
    let base = **m.base_attrs.get_or_insert_with(|| Box::new(m.attrs));
    for &a in dirty {
        let (Some(b), Some(field)) = (attr_field_ref(&base, a), attr_field(&mut m.attrs, a)) else {
            continue;
        };
        *field = m.effects.attribute(a, b);
    }
    #[allow(clippy::cast_possible_truncation)]
    let max = m.attrs.max_health as f32;
    m.health = m.health.min(max);
    if m.effects.modifiers.is_empty()
        && let Some(b) = m.base_attrs.take()
    {
        m.attrs = *b;
    }
    #[allow(clippy::cast_possible_truncation)]
    let max_absorption = m.effects.attribute(Attribute::MAX_ABSORPTION, 0.0) as f32;
    m.absorption = m.absorption.min(max_absorption);
    base
}

/// `AttributeInstance.getBaseValue` of an attribute a mob keeps a value of (`None` for the
/// others: their base is the type's default).
pub fn mob_attribute_base(m: &crate::mob::MobData, a: Attribute) -> Option<f64> {
    attr_field_ref(m.base_attrs.as_deref().unwrap_or(&m.attrs), a)
}

/// The `update_attributes` of the attributes a mob was given (`ServerEntity.sendPairingData`);
/// `None` without any the client knows and the game keeps a value of.
pub fn mob_pairing_attributes(
    network_id: i32,
    m: &crate::mob::MobData,
    given: &crate::mob::entity_nbt::NbtAttributes,
) -> Option<UpdateAttributes> {
    let kind = m.kind.type_id();
    let synced: Vec<(Attribute, f64)> = given
        .0
        .iter()
        .filter(|a| a.attribute.syncable())
        .map(|a| {
            let base = mob_attribute_base(m, a.attribute)
                .or_else(|| m.effects.modifiers.base(a.attribute))
                .unwrap_or_else(|| a.attribute.base_of(kind));
            (a.attribute, base)
        })
        .collect();
    (!synced.is_empty()).then(|| UpdateAttributes {
        body: attribute::encode_update(network_id, &synced, &m.effects.modifiers),
    })
}

/// The `update_attributes` of the attributes a command or plugin changed on a player, for the
/// players that start to see it (`ServerEntity.sendPairingData`).
pub fn player_pairing_attributes(network_id: i32, p: &PlayerData) -> Option<UpdateAttributes> {
    let own = &p.survival.effects.modifiers;
    let synced: Vec<(Attribute, f64)> = p
        .survival
        .attributes
        .0
        .iter()
        .filter(|a| a.attribute.syncable())
        .map(|a| {
            let base = own
                .base(a.attribute)
                .unwrap_or_else(|| a.attribute.base_of(ids::PLAYER));
            (a.attribute, base)
        })
        .collect();
    (!synced.is_empty()).then(|| UpdateAttributes {
        body: attribute::encode_update(network_id, &synced, own),
    })
}

/// `AttributeInstance.setBaseValue` of an attribute a mob keeps a value of; the value with
/// the modifiers follows with the mob's next effect sync (`onAttributeUpdated`, the update to
/// the trackers). Returns `false` for an attribute the mob keeps no value of.
pub fn set_mob_attribute_base(m: &mut crate::mob::MobData, a: Attribute, value: f64) -> bool {
    let base = match m.base_attrs.as_deref_mut() {
        Some(b) => b,
        None => &mut m.attrs,
    };
    let Some(field) = attr_field(base, a) else {
        return false;
    };
    *field = value;
    m.effects.modifiers.touch(a);
    true
}

/// A player as the target of effects added outside its tick and without the world's
/// environment (beacons, conduits, bosses): only effects that need no damage or food; the
/// sound is queued for the player's next sync.
struct BareHost<'a> {
    player: &'a mut PlayerData,
}

impl EffectHost for BareHost<'_> {
    fn effects(&mut self) -> &mut ActiveEffects {
        &mut self.player.survival.effects
    }
    fn health(&self) -> f32 {
        self.player.survival.health
    }
    fn max_health(&self) -> f32 {
        self.player.survival.max_health()
    }
    fn heal(&mut self, _amount: f32) {}
    fn hurt(&mut self, _kind: HurtKind, _amount: f32) -> bool {
        false
    }
    fn absorption(&self) -> f32 {
        self.player.survival.absorption
    }
    fn set_absorption(&mut self, amount: f32) {
        let max = self.player.survival.max_absorption();
        self.player.survival.absorption = amount.clamp(0.0, max);
    }
    fn inverted_heal_and_harm(&self) -> bool {
        false
    }
    fn tick_count(&self) -> i32 {
        self.player.survival.tick_count
    }
    fn play_sound(&mut self, sound: &str) {
        self.player.survival.effects.push_sound(sound);
    }
}

/// A mob as the target of effects added outside its tick (see [`BareHost`]); the sync follows
/// with the mob's next tick.
struct BareMobHost<'a> {
    m: &'a mut crate::mob::MobData,
    kind: EntityTypeId,
}

impl EffectHost for BareMobHost<'_> {
    fn effects(&mut self) -> &mut ActiveEffects {
        &mut self.m.effects
    }
    fn health(&self) -> f32 {
        self.m.health
    }
    fn max_health(&self) -> f32 {
        #[allow(clippy::cast_possible_truncation)]
        let max = self.m.attrs.max_health as f32;
        max
    }
    fn heal(&mut self, _amount: f32) {}
    fn hurt(&mut self, _kind: HurtKind, _amount: f32) -> bool {
        false
    }
    fn absorption(&self) -> f32 {
        self.m.absorption
    }
    fn set_absorption(&mut self, amount: f32) {
        let max = self.m.effects.attribute(Attribute::MAX_ABSORPTION, 0.0);
        #[allow(clippy::cast_possible_truncation)]
        let amount = amount.clamp(0.0, max as f32);
        self.m.absorption = amount;
    }
    fn inverted_heal_and_harm(&self) -> bool {
        super::inverted_heal_and_harm(self.kind)
    }
    fn tick_count(&self) -> i32 {
        self.m.tick_count
    }
    fn play_sound(&mut self, sound: &str) {
        // Played with the mob's next tick ([`send_to_passengers`]).
        self.m.effects.push_sound(sound);
    }
}

/// `player.addEffect` from a block entity (beacon powers, conduit power): see
/// [`add_detached`].
pub fn add_to_player(ents: &mut Entities, player: EntityId, new: EffectInstance) -> bool {
    add_detached(ents, player, new)
}

/// `addEffect` on a player or mob outside its tick and without the world's environment
/// (beacons, conduits, bosses, trial spawners), with `canBeAffected`: non-instant effects; the
/// packets, flags, attributes and the sound follow with the entity's next tick. Returns
/// whether the effect changed.
pub fn add_detached(ents: &mut Entities, id: EntityId, new: EffectInstance) -> bool {
    add_detached_from(ents, id, new, None)
}

/// [`add_detached`] with the entity that applies the effect (`addEffect(effect, source)`).
pub fn add_detached_from(
    ents: &mut Entities,
    id: EntityId,
    new: EffectInstance,
    source: Option<EntityId>,
) -> bool {
    !new.effect.is_instantaneous() && add_queued_from(ents, id, new, source)
}

/// `addEffect` from a command (`/effect give`) on a player or mob of this world, with
/// `canBeAffected`: like [`add_detached`], but instantaneous effects are kept too and apply
/// with the entity's next effect tick (`InstantenousMobEffect.shouldApplyEffectTickThisTick`).
/// Returns whether the effect changed.
pub fn add_queued(ents: &mut Entities, id: EntityId, new: EffectInstance) -> bool {
    add_queued_from(ents, id, new, None)
}

/// [`add_queued`] with the entity that applies the effect (`addEffect(effect, source)`, the
/// command source's entity of `/effect give`).
pub fn add_queued_from(
    ents: &mut Entities,
    id: EntityId,
    new: EffectInstance,
    source: Option<EntityId>,
) -> bool {
    let Some(kind) = ents.store.get(id).map(|e| e.kind()) else {
        return false;
    };
    if !super::can_be_affected(kind, new.effect) || super::refuses_all_effects(kind) {
        return false;
    }
    if let Some(m) = ents.mob_mut(id) {
        return active::add_from(&mut BareMobHost { m, kind }, new, source);
    }
    let Some(data) = ents.player_mut(id) else {
        return false;
    };
    active::add_from(&mut BareHost { player: data }, new, source)
}

/// `addEffect(effect, source)` on any living entity: players and mobs, with `canBeAffected`.
/// Returns whether the effect changed.
pub fn add_to_entity(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    new: EffectInstance,
) -> bool {
    add_to_entity_from(level, ents, env, cx, target, new, None)
}

/// [`add_to_entity`] with the entity that applies the effect (`addEffect(effect, source)`).
pub fn add_to_entity_from(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    new: EffectInstance,
    source: Option<EntityId>,
) -> bool {
    let Some(kind) = ents.store.get(target).map(|e| e.kind()) else {
        return false;
    };
    if !super::can_be_affected(kind, new.effect) || super::refuses_all_effects(kind) {
        return false;
    }
    with_host(level, ents, env, cx, target, |host| {
        active::add_from(host, new, source)
    })
    .unwrap_or(false)
}

/// Runs `f` with the effect host of a living entity and syncs it afterwards; `None` when it
/// is no living entity.
pub fn with_host<R>(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    f: impl FnOnce(&mut dyn EffectHost) -> R,
) -> Option<R> {
    with_host_from(level, ents, env, cx, target, HurtKind::Magic, f)
}

/// [`with_host`] where `f` may hurt with `source`: a player knows the names of its entities
/// for the death message.
pub fn with_host_from<R>(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: HurtKind,
    f: impl FnOnce(&mut dyn EffectHost) -> R,
) -> Option<R> {
    if ents.mob(target).is_some() {
        let r = {
            let mut t = Tick {
                level,
                ents,
                env,
                cx,
                id: target,
            };
            f(&mut MobHost { t: &mut t })
        };
        sync_mob(ents, env.out, target);
        return Some(r);
    }
    let mut names = Vec::new();
    if let HurtKind::IndirectMagic { direct, owner } = source {
        for id in [direct, owner].into_iter().flatten() {
            if let Some(n) = crate::mob::combat::attacker_name(ents, env, id) {
                names.push((id, n));
            }
        }
    }
    let Entities {
        store,
        players,
        players_by_uuid,
        ..
    } = ents;
    let mut entity = store.get_mut(target)?;
    let row = entity.as_ref().kind_row();
    let data = players.get_mut(row)?;
    let r = {
        let mut host = PlayerHost {
            entity: &mut entity,
            player: data,
            env,
            players: Some(players_by_uuid),
            names: &names,
        };
        f(&mut host)
    };
    // A player's packets go out with its next tick's sync.
    Some(r)
}

/// `PotionContents.applyToLivingEntity` on any living entity (the potion was drunk).
pub fn apply_potion_to_entity(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    contents: &PotionContents,
    scale: f32,
) {
    let Some(kind) = ents.store.get(target).map(|e| e.kind()) else {
        return;
    };
    let drinker = (kind == ids::PLAYER).then_some(target);
    let source = HurtKind::IndirectMagic {
        direct: drinker,
        owner: drinker,
    };
    with_host_from(level, ents, env, cx, target, source, |host| {
        super::consume::apply_potion(kind, contents, scale, host, source);
    });
}

/// `LivingEntity.remove(KILLED)` → `triggerOnDeathMobEffects`: Oozing spawns slimes, Weaving
/// spreads cobwebs, Wind Charged bursts (`WindChargedMobEffect.onMobRemoved`), then the
/// effects are gone.
pub fn on_mob_killed(t: &mut Tick<'_, '_>) {
    if t.mob_ref().effects.is_empty() {
        return;
    }
    let effects = t.mob().effects.take_all();
    let pos = t.pos();
    for e in &effects {
        match e.effect {
            MobEffect::OOZING => {
                // `numberOfSlimesToSpawn` with `maxEntityCramming`: slimes within 2 blocks count.
                let requested = 2;
                let cramming = t
                    .env
                    .rules
                    .get(crate::survival::rules::Rule::MAX_ENTITY_CRAMMING);
                let area = t.aabb().inflate(2.0, 2.0, 2.0);
                let me = t.id;
                let mut nearby = 0;
                t.ents.store.for_each_in(&area, false, |other| {
                    if other != me
                        && t.ents
                            .store
                            .get(other)
                            .is_some_and(|o| o.kind() == ids::SLIME)
                    {
                        nearby += 1;
                    }
                });
                let count = if cramming < 1 {
                    requested
                } else {
                    (cramming - nearby.min(cramming)).clamp(0, requested)
                };
                // `spawnSlimeOffspring`: a size-2 slime (`setSize(2, true)`) with the level
                // random's yaw, added at once.
                let network_ids = std::sync::Arc::clone(t.level.network_ids_arc());
                for _ in 0..count {
                    let seed = crate::mob::new_entity_seed();
                    let mut slime = crate::mob::MobData::new(crate::mob::Kind::Slime, seed);
                    crate::mob::fam::undead::cubes::set_size(&mut slime, 2, true);
                    // `setSize` turns head and body to the constructor's yaw, `snapTo` only
                    // the body's yaw to the level random's.
                    let seed_yaw = crate::mob::constructor_y_rot(seed);
                    slime.y_body_rot = seed_yaw;
                    let y_rot = t.env.random.next_float() * 360.0;
                    let id = crate::mob::spawn(
                        t.ents,
                        &network_ids,
                        crate::mob::new_uuid(),
                        Vec3::new(pos.x, pos.y + 0.5, pos.z),
                        vanillite_world::entity::Rotation {
                            y_rot,
                            x_rot: 0.0,
                            y_head_rot: seed_yaw,
                        },
                        slime,
                    );
                    t.ents
                        .spawned(id, vanillite_abi::spawn_reason::TRIGGERED, 0);
                }
            }
            MobEffect::WEAVING if t.env.rules.bool(crate::survival::rules::Rule::MOB_GRIEFING) => {
                spread_cobwebs(t, pos);
            }
            MobEffect::WIND_CHARGED => {
                let (center, radius) = wind_burst(pos, t.core().dims().height, &mut t.mob().random);
                t.cx.actions
                    .push(WorldAction::WindBurst(crate::mob::WindBurst {
                        source: Some(t.id),
                        pos: center,
                        radius,
                        knockback: WIND_CHARGED_KNOCKBACK,
                        sound: WIND_CHARGED_SOUND,
                    }));
            }
            _ => {}
        }
    }
}

/// `LivingEntity.remove(KILLED)` → `triggerOnDeathMobEffects` of a dead player (at death time
/// 20, or when it respawns before): Oozing spawns slimes (the player's slime count against
/// `cramming`), Weaving spreads cobwebs (for players without `mobGriefing`), Wind Charged
/// bursts, all from the player's own random; then the effects are gone.
pub fn player_death_effects(level: &mut Level, id: EntityId, cramming: i32) {
    let Some((pos, height, aabb)) = level
        .entities
        .store
        .get(id)
        .map(|e| (e.pos(), e.dims().height, e.aabb()))
    else {
        return;
    };
    let Some(p) = level.entities.player_mut(id) else {
        return;
    };
    if p.survival.effects.is_empty() {
        return;
    }
    let effects = p.survival.effects.take_all();
    for e in &effects {
        match e.effect {
            MobEffect::OOZING => {
                let area = aabb.inflate(2.0, 2.0, 2.0);
                let mut nearby = 0;
                level.entities.store.for_each_in(&area, false, |other| {
                    if other != id
                        && level
                            .entities
                            .store
                            .get(other)
                            .is_some_and(|o| o.kind() == ids::SLIME)
                    {
                        nearby += 1;
                    }
                });
                let count = if cramming < 1 {
                    2
                } else {
                    (cramming - nearby.min(cramming)).clamp(0, 2)
                };
                let network_ids = std::sync::Arc::clone(level.network_ids_arc());
                for _ in 0..count {
                    let seed = crate::mob::new_entity_seed();
                    let mut slime = crate::mob::MobData::new(crate::mob::Kind::Slime, seed);
                    crate::mob::fam::undead::cubes::set_size(&mut slime, 2, true);
                    let seed_yaw = crate::mob::constructor_y_rot(seed);
                    slime.y_body_rot = seed_yaw;
                    let y_rot = level.random().next_float() * 360.0;
                    let slime_id = crate::mob::spawn(
                        &mut level.entities,
                        &network_ids,
                        crate::mob::new_uuid(),
                        Vec3::new(pos.x, pos.y + 0.5, pos.z),
                        vanillite_world::entity::Rotation {
                            y_rot,
                            x_rot: 0.0,
                            y_head_rot: seed_yaw,
                        },
                        slime,
                    );
                    level
                        .entities
                        .spawned(slime_id, vanillite_abi::spawn_reason::TRIGGERED, 0);
                }
            }
            MobEffect::WEAVING => {
                let center = BlockPos::containing(pos.x, pos.y, pos.z);
                let Some(p) = level.entities.player_mut(id) else {
                    continue;
                };
                let max = p.survival.random.next_int_bounded(2) + 2;
                // `BlockPos.randomInCube(random, 15, pos, 1)`: drawn while iterating.
                let mut chosen: Vec<BlockPos> = Vec::new();
                for _ in 0..15 {
                    let Some(p) = level.entities.player_mut(id) else {
                        break;
                    };
                    let r = &mut p.survival.random;
                    let at = BlockPos::new(
                        center.x - 1 + r.next_int_bounded(3),
                        center.y - 1 + r.next_int_bounded(3),
                        center.z - 1 + r.next_int_bounded(3),
                    );
                    let below = BlockPos::new(at.x, at.y - 1, at.z);
                    if !chosen.contains(&at)
                        && crate::block::can_be_replaced(level.block(at))
                        && crate::block::is_face_sturdy(
                            level.block(below),
                            vanillite_base::Direction::Up,
                        )
                    {
                        chosen.push(at);
                        if i32::try_from(chosen.len()).unwrap_or(i32::MAX) >= max {
                            break;
                        }
                    }
                }
                let cobweb = vanillite_data::block::ids::COBWEB.default_state();
                for at in chosen {
                    level.set_block(at, cobweb, 3);
                    level.level_event(None, 3018, at, 0);
                }
            }
            MobEffect::WIND_CHARGED => {
                let Some(p) = level.entities.player_mut(id) else {
                    continue;
                };
                let (center, radius) = wind_burst(pos, height, &mut p.survival.random);
                level.explode_with(
                    Some(id),
                    None,
                    center,
                    radius,
                    crate::level::explosion::BlockInteraction::TriggerBlock,
                    crate::level::explosion::ExplosionStyle {
                        damages_entities: false,
                        knockback: WIND_CHARGED_KNOCKBACK,
                        wind_sound: Some(WIND_CHARGED_SOUND),
                        resistance: crate::level::explosion::BlockResistance::WindCharge,
                        fire: false,
                        bad_respawn_point: false,
                    },
                );
            }
            _ => {}
        }
    }
}

/// The knockback multiplier of a Wind Charged burst (`AbstractWindCharge.EXPLOSION_DAMAGE_CALCULATOR`).
pub const WIND_CHARGED_KNOCKBACK: f64 = 1.22;
/// The sound of a Wind Charged burst (`SoundEvents.BREEZE_WIND_CHARGE_BURST`).
pub const WIND_CHARGED_SOUND: &str = "minecraft:entity.breeze.wind_burst";

/// The centre (half the entity's height up) and strength (3 to 5, from the entity's random)
/// of a Wind Charged burst (`WindChargedMobEffect.onMobRemoved`).
pub fn wind_burst(pos: Vec3, height: f32, random: &mut LegacyRandom) -> (Vec3, f32) {
    let strength = 3.0 + random.next_float() * 2.0;
    (
        Vec3::new(pos.x, pos.y + f64::from(height / 2.0), pos.z),
        strength,
    )
}

/// `WeavingMobEffect.spawnCobwebsRandomlyAround`.
fn spread_cobwebs(t: &mut Tick<'_, '_>, pos: Vec3) {
    let center = BlockPos::containing(pos.x, pos.y, pos.z);
    let m = t.mob();
    let max = m.random.next_int_bounded(2) + 2;
    let mut chosen: Vec<BlockPos> = Vec::new();
    for _ in 0..15 {
        let m = t.mob();
        let p = BlockPos::new(
            center.x - 1 + m.random.next_int_bounded(3),
            center.y - 1 + m.random.next_int_bounded(3),
            center.z - 1 + m.random.next_int_bounded(3),
        );
        let below = BlockPos::new(p.x, p.y - 1, p.z);
        if !chosen.contains(&p)
            && crate::block::can_be_replaced(t.level.block(p))
            && crate::block::is_face_sturdy(t.level.block(below), vanillite_base::Direction::Up)
        {
            chosen.push(p);
            if i32::try_from(chosen.len()).unwrap_or(i32::MAX) >= max {
                break;
            }
        }
    }
    let cobweb = vanillite_data::block::ids::COBWEB.default_state();
    for p in chosen {
        t.cx.actions.push(WorldAction::SetBlock(p, cobweb, 3));
        t.cx.actions.push(WorldAction::LevelEvent(3018, p, 0));
    }
}

/// `onMobHurt` of the active effects after a living entity took damage: Infested may release
/// silverfish (`InfestedMobEffect`). `random` is the entity's random, `look` its view vector.
pub fn on_hurt(
    fx: &ActiveEffects,
    random: &mut LegacyRandom,
    level_random: &mut LegacyRandom,
    pos: Vec3,
    height: f32,
    look: Vec3,
    out: &mut Vec<Effect>,
) {
    if !fx.has(MobEffect::INFESTED) {
        return;
    }
    if random.next_float() > 0.1 {
        return;
    }
    let count = random.next_int_bounded(2) + 1;
    for _ in 0..count {
        // `viewDirection = look * 0.3 * (1, 1.5, 1)` rotated around Y by a random angle.
        let angle = random.next_float() * std::f32::consts::PI - std::f32::consts::FRAC_PI_2;
        #[allow(clippy::cast_possible_truncation)]
        let (x, y, z) = (
            (look.x as f32) * 0.3,
            (look.y as f32) * 0.3 * 1.5,
            (look.z as f32) * 0.3,
        );
        let (sin, cos) = angle.sin_cos();
        // JOML `rotateY`: x' = cos*x + sin*z, z' = -sin*x + cos*z.
        let velocity = Vec3::new(
            f64::from(cos * x + sin * z),
            f64::from(y),
            f64::from(-sin * x + cos * z),
        );
        let y_rot = level_random.next_float() * 360.0;
        let at = Vec3::new(pos.x, pos.y + f64::from(height) / 2.0, pos.z);
        out.push(Effect::SpawnMob {
            kind: ids::SILVERFISH,
            pos: at,
            y_rot,
            velocity,
        });
        // `silverfish.playSound(SILVERFISH_HURT)` once it is added.
        crate::survival::play_sound(
            out,
            None,
            at,
            "minecraft:entity.silverfish.hurt",
            vanillite_protocol::packets::play::clientbound::SoundSource::Hostile,
            1.0,
            1.0,
            crate::survival::sound_seed(),
        );
    }
}

/// `Entity.calculateViewVector(xRot, yRot)`.
pub fn view_vector(x_rot: f32, y_rot: f32) -> Vec3 {
    use vanillite_base::math::{cos, sin};
    let rx = f64::from(x_rot * (std::f32::consts::PI / 180.0));
    let ry = f64::from(-y_rot * (std::f32::consts::PI / 180.0));
    let (ys, yc) = (sin(ry), cos(ry));
    let (xs, xc) = (sin(rx), cos(rx));
    Vec3::new(f64::from(ys * xc), f64::from(-xs), f64::from(yc * xc))
}

/// The enchantment entity effect `minecraft:apply_mob_effect` (`ApplyMobEffect.apply`, e.g.
/// Bane of Arthropods' Slowness): one of `candidates` with a duration (seconds) and amplifier
/// between the bounds already calculated for the enchantment level, drawn from the target's
/// own random. Returns whether the effect changed.
#[allow(clippy::too_many_arguments)]
pub fn apply_mob_effect(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    candidates: &[MobEffect],
    duration: (f32, f32),
    amplifier: (f32, f32),
) -> bool {
    let draw = |random: &mut LegacyRandom| -> Option<EffectInstance> {
        // `HolderSet.getRandomElement`: `Util.getRandomSafe`.
        if candidates.is_empty() {
            return None;
        }
        let n = i32::try_from(candidates.len()).unwrap_or(i32::MAX);
        let effect = *candidates.get(usize::try_from(random.next_int_bounded(n)).ok()?)?;
        let between =
            |r: &mut LegacyRandom, (min, max): (f32, f32)| r.next_float() * (max - min) + min;
        // `Math.round(float)`.
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let ticks = (between(random, duration) * 20.0 + 0.5).floor() as i32;
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let amp = ((between(random, amplifier) + 0.5).floor() as i32).max(0);
        Some(EffectInstance::new(effect, ticks, amp))
    };
    let instance = if let Some(m) = ents.mob_mut(target) {
        draw(&mut m.random)
    } else if let Some(p) = ents.player_mut(target) {
        draw(&mut p.survival.random)
    } else {
        None
    };
    match instance {
        Some(e) => add_to_entity(level, ents, env, cx, target, e),
        None => false,
    }
}

/// `addEffect` on a mob when only its data is at hand (another mob's attack, its own
/// conversion), with `canBeAffected`; like [`add_detached`], the packets, attributes and the
/// sound follow with the mob's next tick. Returns whether the effect changed.
pub fn add_to_mob(m: &mut crate::mob::MobData, new: EffectInstance) -> bool {
    let kind = m.kind.type_id();
    if new.effect.is_instantaneous()
        || !super::can_be_affected(kind, new.effect)
        || super::refuses_all_effects(kind)
    {
        return false;
    }
    active::add(&mut BareMobHost { m, kind }, new)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `InfestedMobEffect.spawnSilverfish`: each silverfish comes with its hurt sound (S-12 of
    /// #272).
    #[test]
    fn infested_silverfish_play_their_hurt_sound() {
        let mut fx = ActiveEffects::default();
        fx.force_add(crate::effect::EffectInstance::new(
            MobEffect::INFESTED,
            100,
            0,
        ));
        let mut out = Vec::new();
        // A seed whose first float is at most 0.1 (the 10 % chance).
        let seed = (0..10_000_i64)
            .find(|&s| LegacyRandom::new(s).next_float() <= 0.1)
            .unwrap_or(0);
        let mut random = LegacyRandom::new(seed);
        let mut level_random = LegacyRandom::new(1);
        on_hurt(
            &fx,
            &mut random,
            &mut level_random,
            Vec3::ZERO,
            1.8,
            Vec3::new(0.0, 0.0, 1.0),
            &mut out,
        );
        let spawned = out
            .iter()
            .filter(|e| matches!(e, Effect::SpawnMob { .. }))
            .count();
        let sounds = out
            .iter()
            .filter(|e| matches!(e, Effect::Nearby { .. }))
            .count();
        assert!(spawned >= 1);
        assert_eq!(spawned, sounds);
    }
}
