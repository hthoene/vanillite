//! The active effects of one living entity (`LivingEntity.activeEffects` and the methods around
//! it: `addEffect`, `forceAddEffect`, `removeEffect`, `removeAllEffects`, `tickEffects`,
//! `onEffectAdded` / `onEffectUpdated` / `onEffectsRemoved`, `updateDirtyEffects`).
//!
//! The entity itself is reached through an [`EffectHost`] (health, damage, food, sounds), so
//! players and mobs share the logic. What the entity's own client must hear is queued as
//! [`EffectEvent`]s; attribute changes go to [`Modifiers`] and are marked dirty.

use vanillite_nbt::Tag;
use vanillite_world::entity::EntityId;

use super::attribute::{Attribute, Modifiers};
use super::instance::{self, EffectInstance};
use super::{MobEffect, Particle};

/// The damage an effect deals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HurtKind {
    /// `damageSources().magic()` (poison, instant damage without a source).
    Magic,
    /// `damageSources().wither()`.
    Wither,
    /// `damageSources().indirectMagic(source, owner)` (splash and lingering potions,
    /// tipped arrows): the network ids of the direct entity and its owner.
    IndirectMagic {
        direct: Option<vanillite_world::entity::EntityId>,
        owner: Option<vanillite_world::entity::EntityId>,
    },
}

/// The living entity effects act on. Its [`ActiveEffects`] stay in place while the effects
/// tick, so damage an effect deals sees the entity's other effects (Resistance).
pub trait EffectHost {
    /// The entity's active effects.
    fn effects(&mut self) -> &mut ActiveEffects;
    /// `getHealth`.
    fn health(&self) -> f32;
    /// `getMaxHealth`.
    fn max_health(&self) -> f32;
    /// `heal`.
    fn heal(&mut self, amount: f32);
    /// `hurtServer` with the effect's damage source; returns whether damage was taken.
    fn hurt(&mut self, kind: HurtKind, amount: f32) -> bool;
    /// `getAbsorptionAmount`.
    fn absorption(&self) -> f32;
    /// `setAbsorptionAmount` (clamped to `[0, max_absorption]` by the host).
    fn set_absorption(&mut self, amount: f32);
    /// `isInvertedHealAndHarm` (`#minecraft:inverted_healing_and_harm`: the undead).
    fn inverted_heal_and_harm(&self) -> bool;
    /// `tickCount` (infinite effects tick on it).
    fn tick_count(&self) -> i32;
    /// `Player.causeFoodExhaustion`; nothing for mobs.
    fn exhaust(&mut self, _amount: f32) {}
    /// `FoodData.eat(food, saturation)`; nothing for mobs.
    fn eat(&mut self, _food: i32, _saturation: f32) {}
    /// Plays `sound` at the entity for everyone near (`MobEffect.onEffectAdded`'s sound).
    fn play_sound(&mut self, _sound: &str) {}
    /// `BadOmenMobEffect.applyEffectTick`: whether the effect stays (a player in a village
    /// turns it into Raid Omen).
    fn bad_omen_tick(&mut self, _amplifier: i32) -> bool {
        true
    }
    /// `RaidOmenMobEffect.applyEffectTick` on the last tick: whether the effect stays (a
    /// player with a raid omen position starts the raid).
    fn raid_omen_tick(&mut self, _amplifier: i32) -> bool {
        true
    }
}

/// What the entity's own client is told (`ServerPlayer.onEffectAdded` /
/// `onEffectUpdated` / `onEffectsRemoved`, and `sendEffectToPassengers` for riders).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectEvent {
    /// `update_mob_effect`; `blend` for a newly added effect; the entity that caused it
    /// (`addEffect(effect, source)`).
    Update {
        instance: EffectInstance,
        blend: bool,
        source: Option<EntityId>,
    },
    /// `remove_mob_effect`.
    Remove(MobEffect),
    /// `update_mob_effect` without a change (`sendActivePlayerEffects`): no `onEffectUpdated`.
    Resend(EffectInstance),
    /// The sound of an effect added outside the entity's tick (`onEffectAdded`), played by
    /// the owner at the entity.
    Sound(Box<str>),
}

/// The active effects of a living entity, sorted by effect id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActiveEffects {
    list: Vec<EffectInstance>,
    /// The attribute modifiers effects (and other sources) add.
    pub modifiers: Modifiers,
    /// `effectsDirty`: particles, invisibility and glowing must be recomputed.
    dirty: bool,
    /// Packets for the entity's own client (and riders), drained by the owner.
    events: Vec<EffectEvent>,
    /// Effects added while the effects tick (`addEffect` from inside an effect tick).
    deferred: Vec<EffectInstance>,
    /// Structural changes of the effect map (`HashMap.modCount`): the effects tick stops
    /// where Vanilla's iterator throws `ConcurrentModificationException`.
    mods: ModCount,
}

/// A change counter that never makes two effect sets differ.
#[derive(Clone, Copy, Debug, Default)]
struct ModCount(u32);

impl PartialEq for ModCount {
    fn eq(&self, _: &ModCount) -> bool {
        true
    }
}

impl ModCount {
    fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}

impl ActiveEffects {
    /// Whether no effect is active.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The active effects in id order.
    pub fn iter(&self) -> impl Iterator<Item = &EffectInstance> {
        self.list.iter()
    }

    /// `hasEffect`.
    pub fn has(&self, effect: MobEffect) -> bool {
        self.get(effect).is_some()
    }

    /// `getEffect`.
    pub fn get(&self, effect: MobEffect) -> Option<&EffectInstance> {
        self.list
            .binary_search_by_key(&effect, |e| e.effect)
            .ok()
            .and_then(|i| self.list.get(i))
    }

    /// The amplifier of an active effect.
    pub fn amplifier(&self, effect: MobEffect) -> Option<i32> {
        self.get(effect).map(EffectInstance::amplifier)
    }

    /// Takes the queued client events.
    pub fn take_events(&mut self) -> Vec<EffectEvent> {
        std::mem::take(&mut self.events)
    }

    /// Whether events wait.
    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    /// `updateDirtyEffects`: whether the synched effect data (particles, ambience,
    /// invisibility, glowing) must be refreshed; clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    fn add_modifiers(&mut self, instance: &EffectInstance) {
        let factor = f64::from(instance.amplifier()) + 1.0;
        for t in instance.effect.info().modifiers {
            self.modifiers
                .set(t.attribute, t.id, t.amount * factor, t.operation);
        }
    }

    fn remove_modifiers(&mut self, effect: MobEffect) {
        for t in effect.info().modifiers {
            self.modifiers.remove(t.attribute, t.id);
        }
    }

    /// `onEffectAdded`.
    fn on_added(&mut self, instance: &EffectInstance, source: Option<EntityId>) {
        self.dirty = true;
        self.add_modifiers(instance);
        self.events.push(EffectEvent::Update {
            instance: instance.clone(),
            blend: true,
            source,
        });
    }

    /// `onEffectUpdated`.
    fn on_updated(
        &mut self,
        instance: &EffectInstance,
        refresh_attributes: bool,
        source: Option<EntityId>,
    ) {
        self.dirty = true;
        if refresh_attributes {
            self.remove_modifiers(instance.effect);
            self.add_modifiers(instance);
        }
        self.events.push(EffectEvent::Update {
            instance: instance.clone(),
            blend: false,
            source,
        });
    }

    /// `onEffectsRemoved` for one effect.
    fn on_removed(&mut self, effect: MobEffect) {
        self.dirty = true;
        self.remove_modifiers(effect);
        self.events.push(EffectEvent::Remove(effect));
    }

    /// The merge part of `addEffect`: inserts or updates; returns whether anything changed and
    /// whether the effect is new.
    fn merge(&mut self, new: EffectInstance, source: Option<EntityId>) -> (bool, bool) {
        match self.list.binary_search_by_key(&new.effect, |e| e.effect) {
            Err(i) => {
                self.on_added(&new, source);
                self.list.insert(i, new);
                self.mods.bump();
                (true, true)
            }
            Ok(i) => {
                let Some(current) = self.list.get_mut(i) else {
                    return (false, false);
                };
                if current.update(&new) {
                    let updated = current.clone();
                    self.on_updated(&updated, true, source);
                    (true, false)
                } else {
                    (false, false)
                }
            }
        }
    }

    /// Queues the sound of an effect added outside the entity's tick ([`EffectEvent::Sound`]).
    pub fn push_sound(&mut self, sound: &str) {
        self.events.push(EffectEvent::Sound(sound.into()));
    }

    /// `addEffect` while the effects tick: applied after the tick.
    pub fn add_deferred(&mut self, new: EffectInstance) {
        self.deferred.push(new);
    }

    /// `forceAddEffect`: replaces an active instance of the effect.
    pub fn force_add(&mut self, new: EffectInstance) {
        match self.list.binary_search_by_key(&new.effect, |e| e.effect) {
            Err(i) => {
                self.on_added(&new, None);
                self.list.insert(i, new);
                self.mods.bump();
            }
            Ok(i) => {
                self.on_updated(&new, true, None);
                if let Some(slot) = self.list.get_mut(i) {
                    *slot = new;
                }
            }
        }
    }

    /// `removeEffect`; returns whether it was active.
    pub fn remove(&mut self, effect: MobEffect) -> bool {
        match self.list.binary_search_by_key(&effect, |e| e.effect) {
            Ok(i) => {
                self.list.remove(i);
                self.on_removed(effect);
                self.mods.bump();
                true
            }
            Err(_) => false,
        }
    }

    /// `removeAllEffects`; returns whether any was active.
    pub fn clear(&mut self) -> bool {
        if self.list.is_empty() {
            return false;
        }
        for e in std::mem::take(&mut self.list) {
            self.on_removed(e.effect);
        }
        self.mods.bump();
        true
    }

    /// Drops all effects without events (`triggerOnDeathMobEffects` clears the map after the
    /// death hooks ran); returns them.
    pub fn take_all(&mut self) -> Vec<EffectInstance> {
        for e in &self.list {
            for t in e.effect.info().modifiers {
                self.modifiers.remove(t.attribute, t.id);
            }
        }
        self.dirty = true;
        if !self.list.is_empty() {
            self.mods.bump();
        }
        std::mem::take(&mut self.list)
    }

    /// Restores saved effects (`readAdditionalSaveData`): no events, modifiers re-added.
    pub fn load(&mut self, tag: Option<&Tag>) {
        for e in instance::list_from_nbt(tag) {
            match self.list.binary_search_by_key(&e.effect, |x| x.effect) {
                Ok(i) => {
                    if let Some(slot) = self.list.get_mut(i) {
                        *slot = e;
                    }
                }
                Err(i) => self.list.insert(i, e),
            }
        }
        let list = std::mem::take(&mut self.list);
        for e in &list {
            self.add_modifiers(e);
        }
        self.list = list;
        self.dirty = true;
    }

    /// The `active_effects` tag, `None` without effects.
    pub fn save(&self) -> Option<Tag> {
        (!self.list.is_empty()).then(|| instance::list_to_nbt(self.list.iter()))
    }

    /// `update_mob_effect` events for every active effect (joining, respawning in another
    /// dimension, a rider mounting).
    pub fn resend_all(&mut self) {
        for e in &self.list {
            self.events.push(EffectEvent::Resend(e.clone()));
        }
    }

    /// `areAllEffectsAmbient`.
    pub fn all_ambient(&self) -> bool {
        self.list.iter().all(|e| !e.visible || e.ambient)
    }

    /// `DATA_EFFECT_PARTICLES`: the encoded particle options of the visible effects, in the
    /// entity data `PARTICLES` format (count, then per particle type id and options).
    pub fn encode_particles(&self, out: &mut Vec<u8>) {
        use vanillite_protocol::codec::write_varint;
        let visible = self.list.iter().filter(|e| e.visible);
        write_varint(out, i32::try_from(visible.clone().count()).unwrap_or(0));
        for e in visible {
            let info = e.effect.info();
            match info.particle {
                Particle::EntityEffect => {
                    write_varint(out, particle_id("minecraft:entity_effect"));
                    // `ColorParticleOption`: ARGB with alpha 38 when ambient.
                    let alpha: u32 = if e.ambient { 38 } else { 255 };
                    let rgb = u32::from_ne_bytes(info.color.to_ne_bytes()) & 0x00FF_FFFF;
                    out.extend_from_slice(&((alpha << 24) | rgb).to_be_bytes());
                }
                Particle::Simple(name) => write_varint(out, particle_id(name)),
            }
        }
    }

    /// The value of an attribute with the effect modifiers on `base`.
    pub fn attribute(&self, attribute: Attribute, base: f64) -> f64 {
        self.modifiers.value(attribute, base)
    }

    /// `MobEffectUtil.hasDigSpeed`.
    pub fn has_dig_speed(&self) -> bool {
        self.has(MobEffect::HASTE) || self.has(MobEffect::CONDUIT_POWER)
    }

    /// `MobEffectUtil.getDigSpeedAmplification`.
    pub fn dig_speed_amplification(&self) -> i32 {
        let haste = self.amplifier(MobEffect::HASTE).unwrap_or(0);
        let conduit = self.amplifier(MobEffect::CONDUIT_POWER).unwrap_or(0);
        haste.max(conduit)
    }

    /// `MobEffectUtil.hasWaterBreathing`.
    pub fn has_water_breathing(&self) -> bool {
        self.has(MobEffect::WATER_BREATHING)
            || self.has(MobEffect::CONDUIT_POWER)
            || self.has(MobEffect::BREATH_OF_THE_NAUTILUS)
    }

    /// `MobEffectUtil.shouldEffectsRefillAirsupply`: Breath of the Nautilus keeps the air from
    /// refilling on land.
    pub fn refills_air(&self) -> bool {
        !self.has(MobEffect::BREATH_OF_THE_NAUTILUS)
            || self.has(MobEffect::WATER_BREATHING)
            || self.has(MobEffect::CONDUIT_POWER)
    }

    /// `getJumpBoostPower`.
    pub fn jump_boost_power(&self) -> f32 {
        self.amplifier(MobEffect::JUMP_BOOST)
            .map_or(0.0, |a| 0.1 * (a as f32 + 1.0))
    }

    /// `getEffectiveGravity`: Slow Falling caps the gravity while falling.
    pub fn effective_gravity(&self, gravity: f64, falling: bool) -> f64 {
        if falling && self.has(MobEffect::SLOW_FALLING) {
            gravity.min(0.01)
        } else {
            gravity
        }
    }

    /// `getDamageAfterMagicAbsorb`'s Resistance part: the damage left after Resistance
    /// (the caller checks `#bypasses_resistance` and `#bypasses_effects`).
    pub fn resistance_damage(&self, damage: f32) -> f32 {
        match self.amplifier(MobEffect::RESISTANCE) {
            Some(a) => {
                let absorb = (a + 1) * 5;
                let resisted = 25 - absorb;
                let result = damage * resisted as f32;
                (result / 25.0).max(0.0)
            }
            None => damage,
        }
    }

    /// `getVisibilityPercent`'s invisibility factor (armour cover from the caller).
    pub fn invisible(&self) -> bool {
        self.has(MobEffect::INVISIBILITY)
    }
}

/// `ParticleTypes` registry id.
fn particle_id(name: &str) -> i32 {
    vanillite_data::registry::PARTICLE_TYPE
        .id(name)
        .map_or(0, i32::from)
}

/// `addEffect(newEffect, source)` after `canBeAffected` passed: merges with an active
/// instance of the effect; returns whether anything changed. Runs `onEffectAdded` (the sound)
/// and `onEffectStarted` (absorption).
pub fn add(host: &mut dyn EffectHost, new: EffectInstance) -> bool {
    add_from(host, new, None)
}

/// [`add`] with the entity that applies the effect (`addEffect(newEffect, source)`).
pub fn add_from(host: &mut dyn EffectHost, new: EffectInstance, source: Option<EntityId>) -> bool {
    let effect = new.effect;
    let amplifier = new.amplifier();
    let (changed, added) = host.effects().merge(new, source);
    if added && let Some(sound) = effect.info().sound_on_added {
        host.play_sound(sound);
    }
    // `onEffectStarted` runs with the new instance's amplifier, changed or not.
    if effect == MobEffect::ABSORPTION {
        let amount = host.absorption().max(4.0 * (1.0 + amplifier as f32));
        host.set_absorption(amount);
    }
    changed
}

/// `tickEffects` on the server: every effect's `tickServer`; run-out effects are removed,
/// every 600 ticks an effect is resent. Effects added by an effect tick apply afterwards.
pub fn tick(host: &mut dyn EffectHost) {
    let mut next = 0usize;
    loop {
        let fx = host.effects();
        let Some(e) = fx.list.get(next) else {
            break;
        };
        let effect = e.effect;
        // `tickServer`.
        if !e.has_remaining_duration() {
            fx.list.remove(next);
            fx.on_removed(effect);
            continue;
        }
        let tick_count = if e.is_infinite() {
            None
        } else {
            Some(e.duration)
        };
        let amplifier = e.amplifier();
        let tick_count = match tick_count {
            Some(d) => d,
            None => host.tick_count(),
        };
        let fx = host.effects();
        let (mods, deferred) = (fx.mods.0, fx.deferred.len());
        let keep =
            !effect.should_apply_tick(tick_count, amplifier) || apply_tick(effect, host, amplifier);
        // A new key or a removal from inside the tick (Bad Omen adding Raid Omen, a totem
        // clearing the effects) makes the iterator throw at its next step: the current effect
        // is not removed and the remaining effects skip this tick.
        let fx = host.effects();
        let modified = fx.mods.0 != mods
            || fx.deferred.get(deferred..).is_some_and(|added| {
                added.iter().any(|d| {
                    fx.list
                        .binary_search_by_key(&d.effect, |e| e.effect)
                        .is_err()
                })
            });
        // The effect tick may have changed the list (death hooks): find the effect again.
        let Ok(i) = fx.list.binary_search_by_key(&effect, |e| e.effect) else {
            if modified {
                break;
            }
            continue;
        };
        next = i;
        let Some(e) = fx.list.get_mut(i) else {
            break;
        };
        if !keep {
            if modified {
                break;
            }
            fx.list.remove(i);
            fx.on_removed(effect);
            continue;
        }
        e.tick_down();
        let downgraded = e.downgrade_to_hidden();
        if !e.has_remaining_duration() {
            if modified {
                break;
            }
            fx.list.remove(i);
            fx.on_removed(effect);
            continue;
        }
        let updated = e.clone();
        if downgraded {
            fx.on_updated(&updated, true, None);
        } else if updated.duration % 600 == 0 {
            fx.on_updated(&updated, false, None);
        }
        if modified {
            break;
        }
        next = i + 1;
    }
    for new in std::mem::take(&mut host.effects().deferred) {
        add(host, new);
    }
}

/// `MobEffect.applyEffectTick`: whether the effect stays.
pub fn apply_tick(effect: MobEffect, host: &mut dyn EffectHost, amplifier: i32) -> bool {
    match effect {
        MobEffect::REGENERATION => {
            if host.health() < host.max_health() {
                host.heal(1.0);
            }
            true
        }
        MobEffect::POISON => {
            if host.health() > 1.0 {
                host.hurt(HurtKind::Magic, 1.0);
            }
            true
        }
        MobEffect::WITHER => {
            host.hurt(HurtKind::Wither, 1.0);
            true
        }
        MobEffect::HUNGER => {
            host.exhaust(0.005 * (amplifier as f32 + 1.0));
            true
        }
        MobEffect::ABSORPTION => host.absorption() > 0.0,
        MobEffect::SATURATION => {
            host.eat(amplifier.wrapping_add(1), 1.0);
            true
        }
        MobEffect::INSTANT_HEALTH | MobEffect::INSTANT_DAMAGE => {
            heal_or_harm(effect, host, amplifier, None, HurtKind::Magic);
            true
        }
        MobEffect::BAD_OMEN => host.bad_omen_tick(amplifier),
        MobEffect::RAID_OMEN => host.raid_omen_tick(amplifier),
        _ => true,
    }
}

/// `HealOrHarmMobEffect.applyEffectTick` (`scale` `None`) and `applyInstantaneousEffect`.
fn heal_or_harm(
    effect: MobEffect,
    host: &mut dyn EffectHost,
    amplifier: i32,
    scale: Option<f64>,
    source: HurtKind,
) {
    let harm = effect == MobEffect::INSTANT_DAMAGE;
    let shift = u32::try_from(amplifier).unwrap_or(0);
    let heal_base = 4_i32.wrapping_shl(shift);
    let harm_base = 6_i32.wrapping_shl(shift);
    if harm == host.inverted_heal_and_harm() {
        let amount = match scale {
            None => heal_base.max(0) as f32,
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            Some(s) => (s * f64::from(heal_base) + 0.5) as i32 as f32,
        };
        host.heal(amount);
    } else {
        let amount = match scale {
            None => harm_base as f32,
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            Some(s) => (s * f64::from(harm_base) + 0.5) as i32 as f32,
        };
        host.hurt(source, amount);
    }
}

/// `MobEffect.applyInstantaneousEffect(level, source, owner, mob, amplifier, scale)`: instant
/// health and damage scale with the distance; other effects tick once.
pub fn apply_instantaneous(
    effect: MobEffect,
    host: &mut dyn EffectHost,
    amplifier: i32,
    scale: f64,
    source: HurtKind,
) {
    match effect {
        MobEffect::INSTANT_HEALTH | MobEffect::INSTANT_DAMAGE => {
            // A missing source deals plain magic damage.
            let source = match source {
                HurtKind::IndirectMagic {
                    direct: None,
                    owner: None,
                } => HurtKind::Magic,
                other => other,
            };
            heal_or_harm(effect, host, amplifier, Some(scale), source);
        }
        _ => {
            apply_tick(effect, host, amplifier);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Host {
        fx: ActiveEffects,
        health: f32,
        absorption: f32,
        hurt: Vec<(HurtKind, f32)>,
        undead: bool,
        ticks: i32,
        exhaustion: f32,
        village: bool,
    }

    impl EffectHost for Host {
        fn effects(&mut self) -> &mut ActiveEffects {
            &mut self.fx
        }
        fn bad_omen_tick(&mut self, amplifier: i32) -> bool {
            if self.village {
                self.fx
                    .add_deferred(EffectInstance::new(MobEffect::RAID_OMEN, 600, amplifier));
            }
            !self.village
        }
        fn health(&self) -> f32 {
            self.health
        }
        fn max_health(&self) -> f32 {
            20.0
        }
        fn heal(&mut self, amount: f32) {
            self.health = (self.health + amount).min(20.0);
        }
        fn hurt(&mut self, kind: HurtKind, amount: f32) -> bool {
            self.hurt.push((kind, amount));
            self.health -= amount;
            true
        }
        fn absorption(&self) -> f32 {
            self.absorption
        }
        fn set_absorption(&mut self, amount: f32) {
            self.absorption = amount;
        }
        fn inverted_heal_and_harm(&self) -> bool {
            self.undead
        }
        fn tick_count(&self) -> i32 {
            self.ticks
        }
        fn exhaust(&mut self, amount: f32) {
            self.exhaustion += amount;
        }
    }

    #[test]
    fn poison_hits_every_25_ticks_and_never_kills() {
        let mut host = Host {
            health: 3.0,
            ..Host::default()
        };
        add(&mut host, EffectInstance::new(MobEffect::POISON, 100, 0));
        for _ in 0..100 {
            tick(&mut host);
        }
        // Damage at remaining durations 100, 75 (then health is 1 and stays).
        assert_eq!(host.hurt.len(), 2);
        assert_eq!(host.health, 1.0);
        assert!(host.fx.is_empty());
        let events = host.fx.take_events();
        assert!(matches!(
            events.first(),
            Some(EffectEvent::Update { blend: true, .. })
        ));
        assert_eq!(events.last(), Some(&EffectEvent::Remove(MobEffect::POISON)));
    }

    #[test]
    fn instant_health_harms_the_undead() {
        let mut host = Host {
            health: 20.0,
            undead: true,
            ..Host::default()
        };
        add(
            &mut host,
            EffectInstance::new(MobEffect::INSTANT_HEALTH, 1, 1),
        );
        tick(&mut host);
        assert_eq!(host.hurt, vec![(HurtKind::Magic, 12.0)]);
        assert!(host.fx.is_empty());
    }

    #[test]
    fn absorption_and_health_boost_modifiers() {
        let mut host = Host::default();
        add(
            &mut host,
            EffectInstance::new(MobEffect::ABSORPTION, 2400, 3),
        );
        assert_eq!(host.absorption, 16.0);
        assert_eq!(host.fx.attribute(Attribute::MAX_ABSORPTION, 0.0), 16.0);
        add(
            &mut host,
            EffectInstance::new(MobEffect::HEALTH_BOOST, 100, 1),
        );
        assert_eq!(host.fx.attribute(Attribute::MAX_HEALTH, 20.0), 28.0);
        host.fx.clear();
        assert_eq!(host.fx.attribute(Attribute::MAX_HEALTH, 20.0), 20.0);
    }

    #[test]
    fn resistance_reduces_damage() {
        let mut host = Host::default();
        add(
            &mut host,
            EffectInstance::new(MobEffect::RESISTANCE, 100, 1),
        );
        assert_eq!(host.fx.resistance_damage(10.0), 6.0);
        add(
            &mut host,
            EffectInstance::new(MobEffect::RESISTANCE, 100, 4),
        );
        assert_eq!(host.fx.resistance_damage(10.0), 0.0);
    }

    #[test]
    fn hunger_exhausts_every_tick() {
        let mut host = Host::default();
        add(&mut host, EffectInstance::new(MobEffect::HUNGER, 20, 2));
        for _ in 0..20 {
            tick(&mut host);
        }
        assert!((host.exhaustion - 0.3).abs() < 1e-5);
    }

    /// `MobEffectUtil.shouldEffectsRefillAirsupply`: Breath of the Nautilus stops the refill
    /// unless Water Breathing or Conduit Power is active too (S-11 of #272).
    #[test]
    fn breath_of_the_nautilus_refill() {
        let mut host = Host {
            health: 20.0,
            ..Host::default()
        };
        assert!(host.fx.refills_air());
        add(
            &mut host,
            EffectInstance::new(MobEffect::BREATH_OF_THE_NAUTILUS, 100, 0),
        );
        assert!(!host.fx.refills_air());
        add(
            &mut host,
            EffectInstance::new(MobEffect::CONDUIT_POWER, 100, 0),
        );
        assert!(host.fx.refills_air());
    }

    /// `tickEffects` and its `ConcurrentModificationException` (S-13 of #272): Bad Omen adding
    /// Raid Omen stops the tick, so Bad Omen stays a tick longer and the later effects skip
    /// it; the next tick removes Bad Omen and sets Raid Omen back to 600.
    #[test]
    fn effect_added_by_a_tick_stops_the_tick() {
        let mut host = Host {
            health: 20.0,
            village: true,
            ..Host::default()
        };
        add(&mut host, EffectInstance::new(MobEffect::BAD_OMEN, 100, 0));
        add(&mut host, EffectInstance::new(MobEffect::INFESTED, 100, 0));
        let duration = |h: &Host, e| h.fx.get(e).map(|i| i.duration);
        tick(&mut host);
        assert_eq!(duration(&host, MobEffect::BAD_OMEN), Some(100));
        assert_eq!(duration(&host, MobEffect::INFESTED), Some(100));
        assert_eq!(duration(&host, MobEffect::RAID_OMEN), Some(600));
        tick(&mut host);
        assert_eq!(duration(&host, MobEffect::BAD_OMEN), None);
        assert_eq!(duration(&host, MobEffect::INFESTED), Some(99));
        assert_eq!(duration(&host, MobEffect::RAID_OMEN), Some(600));
    }
}
