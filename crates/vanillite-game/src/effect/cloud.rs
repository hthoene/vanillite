//! Thrown potions hitting (`AbstractThrownPotion.onHit`, `ThrownSplashPotion.onHitAsPotion`,
//! `ThrownLingeringPotion.onHitAsPotion`), the area effect cloud they leave
//! (`AreaEffectCloud`), and the effects of tipped arrows (`Arrow.doPostHurtEffects`).
//!
//! The projectiles themselves (flight, owner, hit test) belong to the projectile code; it calls
//! [`potion_hit`] when a splash or lingering potion lands and [`arrow_hit`] after an arrow hurt
//! a living entity.

use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::entity::ids;
use vanillite_nbt::{Compound, Tag};
use vanillite_world::entity::{Dimensions, EntityId, NetworkIds, NewEntity};

use super::living::{add_to_entity_from, with_host_from};
use super::potion::{self, PotionContents};
use super::{EffectInstance, HurtKind, active};
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::mob::{MobContext, WorldAction};
use crate::survival::player::Env;

/// The kind data of an `area_effect_cloud` entity.
#[derive(Clone, Debug, PartialEq)]
pub struct CloudData {
    /// `DATA_RADIUS`.
    pub radius: f32,
    pub radius_on_use: f32,
    pub radius_per_tick: f32,
    /// Ticks after the wait time it lasts, `-1` forever.
    pub duration: i32,
    pub duration_on_use: i32,
    pub wait_time: i32,
    pub reapplication_delay: i32,
    /// `tickCount` (`Age`).
    pub age: i32,
    /// `DATA_WAITING`.
    pub waiting: bool,
    pub owner: Option<EntityId>,
    /// The owner's UUID (`Owner`), kept for saving.
    pub owner_uuid: Option<u128>,
    pub contents: PotionContents,
    pub duration_scale: f32,
    /// `customParticle` is the dragon's breath (the only custom particle the server sets).
    pub dragon_breath: bool,
    /// `victims`: entities and the tick they may be affected again.
    pub victims: Vec<(EntityId, i32)>,
    /// The custom name (`CustomName`, from the potion's `custom_name`), encoded.
    pub custom_name: Option<Box<[u8]>>,
}

impl Default for CloudData {
    fn default() -> CloudData {
        CloudData {
            radius: 3.0,
            radius_on_use: 0.0,
            radius_per_tick: 0.0,
            duration: -1,
            duration_on_use: 0,
            wait_time: 20,
            reapplication_delay: 20,
            age: 0,
            waiting: false,
            owner: None,
            owner_uuid: None,
            contents: PotionContents::default(),
            duration_scale: 1.0,
            dragon_breath: false,
            victims: Vec::new(),
            custom_name: None,
        }
    }
}

impl CloudData {
    /// `DATA_PARTICLE`: `entity_effect` in the opaque potion colour.
    pub fn particle_color(&self) -> i32 {
        self.contents.color() | -16_777_216
    }

    /// The `area_effect_cloud` entity NBT after the entity's own (`addAdditionalSaveData`).
    pub fn save(&self, out: &mut Compound) {
        out.insert("Age", Tag::Int(self.age));
        out.insert("Duration", Tag::Int(self.duration));
        out.insert("WaitTime", Tag::Int(self.wait_time));
        out.insert("ReapplicationDelay", Tag::Int(self.reapplication_delay));
        out.insert("DurationOnUse", Tag::Int(self.duration_on_use));
        out.insert("RadiusOnUse", Tag::Float(self.radius_on_use));
        out.insert("RadiusPerTick", Tag::Float(self.radius_per_tick));
        out.insert("Radius", Tag::Float(self.radius));
        if let Some(tag) = self
            .custom_name
            .as_deref()
            .and_then(crate::mob::nbt::component_tag)
        {
            out.insert("CustomName", tag);
        }
        if self.dragon_breath {
            let mut p = Compound::new();
            p.insert("type", Tag::String("minecraft:dragon_breath".into()));
            p.insert("power", Tag::Float(1.0));
            out.insert("custom_particle", Tag::Compound(p));
        }
        if let Some(uuid) = self.owner_uuid {
            out.insert("Owner", crate::mob::nbt::uuid_tag(uuid));
        }
        if self.contents != PotionContents::default() {
            out.insert("potion_contents", Tag::Compound(self.contents.to_nbt()));
        }
        if self.duration_scale != 1.0 {
            out.insert("potion_duration_scale", Tag::Float(self.duration_scale));
        }
    }

    /// Reads the `area_effect_cloud` entity NBT (`readAdditionalSaveData`); custom particles
    /// other than the dragon's breath fall back to the potion colour.
    pub fn load(tag: &Compound) -> CloudData {
        let int = |key: &str, default: i32| {
            tag.get(key)
                .and_then(Tag::as_i64)
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(default)
        };
        let float = |key: &str, default: f32| match tag.get(key) {
            Some(Tag::Float(f)) if f.is_finite() => *f,
            _ => default,
        };
        let dragon_breath = matches!(
            tag.get("custom_particle"),
            Some(Tag::Compound(p))
                if matches!(p.get("type"), Some(Tag::String(s)) if s == "minecraft:dragon_breath")
        );
        let contents = match tag.get("potion_contents") {
            Some(Tag::Compound(c)) => PotionContents::from_nbt(c),
            _ => PotionContents::default(),
        };
        CloudData {
            radius: float("Radius", 3.0).clamp(0.0, 32.0),
            radius_on_use: float("RadiusOnUse", 0.0),
            radius_per_tick: float("RadiusPerTick", 0.0),
            duration: int("Duration", -1),
            duration_on_use: int("DurationOnUse", 0),
            wait_time: int("WaitTime", 20),
            reapplication_delay: int("ReapplicationDelay", 20),
            age: int("Age", 0),
            waiting: false,
            owner: None,
            owner_uuid: crate::mob::nbt::uuid_from_tag(tag.get("Owner")),
            custom_name: tag
                .get("CustomName")
                .and_then(crate::mob::nbt::tag_component),
            contents,
            duration_scale: float("potion_duration_scale", 1.0).max(0.0),
            dragon_breath,
            victims: Vec::new(),
        }
    }
}

/// `EntityDimensions.scalable(radius * 2, 0.5)`.
fn dims_of(radius: f32) -> Dimensions {
    Dimensions {
        width: radius * 2.0,
        height: 0.5,
    }
}

/// `AbstractThrownPotion.onHit` on the server: water effects around the impact, the potion's
/// effects (splash) or a cloud (lingering), the splash level events. `potion_box` is the
/// projectile's box moved to the hit location, `margin` `ProjectileUtil.computeMargin`,
/// `hit_entity` the entity a lingering potion hit (the cloud starts at its position).
#[allow(clippy::too_many_arguments)]
pub fn potion_hit(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    projectile: Option<EntityId>,
    owner: Option<EntityId>,
    stack: &ItemStack,
    potion_box: Aabb,
    margin: f32,
    hit_entity: Option<EntityId>,
) {
    let contents = PotionContents::of_stack(stack);
    let center = Vec3::new(
        (potion_box.min_x + potion_box.max_x) / 2.0,
        potion_box.min_y,
        (potion_box.min_z + potion_box.max_z) / 2.0,
    );
    affect_entities_around(
        level, ents, env, cx, projectile, owner, &contents, potion_box, center,
    );
    let lingering =
        vanillite_data::registry::ITEM.name(stack.item) == Some("minecraft:lingering_potion");
    if contents.has_effects() {
        if lingering {
            let at = hit_entity
                .and_then(|e| ents.store.get(e).map(|e| e.pos()))
                .unwrap_or(center);
            spawn_lingering(ents, level.network_ids(), at, owner, stack);
        } else {
            splash(
                level, ents, env, cx, projectile, owner, stack, &contents, potion_box, margin,
            );
        }
    }
    let pos = BlockPos::containing(center.x, center.y, center.z);
    let instant = contents.potion.is_some_and(|p| {
        potion::potion_effects(p)
            .iter()
            .any(|e| e.effect.is_instantaneous())
    });
    let (particles, sound) = if instant { (2007, 1054) } else { (2002, 1053) };
    cx.actions
        .push(WorldAction::LevelEvent(particles, pos, contents.color()));
    cx.actions.push(WorldAction::LevelEvent(sound, pos, 0));
}

/// `affectEntitiesAround`: water hurts water sensitive entities and puts out burning ones
/// within 4 blocks.
#[allow(clippy::too_many_arguments)]
fn affect_entities_around(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    projectile: Option<EntityId>,
    owner: Option<EntityId>,
    contents: &PotionContents,
    potion_box: Aabb,
    center: Vec3,
) {
    // `#hurts_water_sensitive_entities` and `#extinguishes_entities`: the water potion.
    if !contents.is_water() {
        return;
    }
    let area = potion_box.inflate(4.0, 2.0, 4.0);
    let mut targets = Vec::new();
    ents.store.for_each_in(&area, false, |id| targets.push(id));
    for id in targets {
        let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
            continue;
        };
        if pos.distance_to_sqr(center) >= 16.0 {
            continue;
        }
        if ents.mob(id).is_some_and(|m| m.kind.sensitive_to_water()) {
            let source = crate::survival::damage::DamageSource {
                cause: owner,
                direct: projectile,
                ..crate::survival::damage::DamageSource::of(env.types.indirect_magic)
            };
            crate::mob::combat::hurt(level, ents, env, cx, id, source, 1.0);
        }
        // `extinguishFire`: `playEntityOnFireExtinguishedSound`, then `clearFire`.
        use vanillite_base::random::RandomSource;
        let extinguished = if let Some(m) = ents.mob_mut(id)
            && m.fire_ticks > 0
            && m.is_alive()
        {
            m.fire_ticks = 0;
            let r = &mut m.random;
            Some((m.kind.sound_source(), r.next_float() - r.next_float()))
        } else if let Some(p) = ents.player_mut(id)
            && p.survival.fire_ticks > 0
            && !p.survival.is_dead_or_dying()
        {
            p.survival.fire_ticks = 0;
            let r = &mut p.survival.random;
            Some((
                vanillite_protocol::packets::play::clientbound::SoundSource::Players,
                r.next_float() - r.next_float(),
            ))
        } else {
            None
        };
        if let Some((source, d)) = extinguished {
            crate::survival::play_sound(
                env.out,
                None,
                pos,
                "minecraft:entity.generic.extinguish_fire",
                source,
                0.7,
                1.6 + d * 0.4,
                crate::survival::sound_seed(),
            );
        }
    }
    // `#rehydrates_axolotls`: every axolotl in the box gets 1800 air (`rehydrate`).
    let mut axolotls = Vec::new();
    ents.store.for_each_in(&area, false, |id| axolotls.push(id));
    for id in axolotls {
        if let Some(m) = ents.mob_mut(id)
            && m.kind == crate::mob::Kind::Axolotl
        {
            m.air = (m.air + 1800).min(6000);
        }
    }
}

/// `ThrownSplashPotion.onHitAsPotion`.
#[allow(clippy::too_many_arguments)]
fn splash(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    projectile: Option<EntityId>,
    owner: Option<EntityId>,
    stack: &ItemStack,
    contents: &PotionContents,
    potion_box: Aabb,
    margin: f32,
) {
    let scale_all = potion::duration_scale(stack);
    let effects = contents.all_effects();
    let area = potion_box.inflate(4.0, 2.0, 4.0);
    let mut targets = Vec::new();
    ents.store.for_each_in(&area, false, |id| targets.push(id));
    let margin = f64::from(margin);
    for id in targets {
        let Some(e) = ents.store.get(id) else {
            continue;
        };
        let target_box = e.aabb().inflate(margin, margin, margin);
        if !is_affected_by_potions(ents, id) {
            continue;
        }
        let dist = box_distance_sqr(&potion_box, &target_box);
        if dist >= 16.0 {
            continue;
        }
        let scale = 1.0 - dist.sqrt() / 4.0;
        for effect in &effects {
            if effect.effect.is_instantaneous() {
                let source = HurtKind::IndirectMagic {
                    direct: projectile,
                    owner,
                };
                with_host_from(level, ents, env, cx, id, source, |host| {
                    active::apply_instantaneous(
                        effect.effect,
                        host,
                        effect.amplifier(),
                        scale,
                        source,
                    );
                });
            } else {
                #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
                let duration = effect
                    .map_duration(|d| (scale * f64::from(d) * f64::from(scale_all) + 0.5) as i32);
                let new = EffectInstance::with_flags(
                    effect.effect,
                    duration,
                    effect.amplifier(),
                    effect.ambient,
                    effect.visible,
                    effect.visible,
                );
                // `getEffectSource()`: the owner, else the potion.
                if !new.ends_within(20) {
                    add_to_entity_from(level, ents, env, cx, id, new, owner.or(projectile));
                }
            }
        }
    }
}

/// `AABB.distanceToSqr(AABB)`.
fn box_distance_sqr(a: &Aabb, b: &Aabb) -> f64 {
    let dx = (a.min_x - b.max_x).max(0.0).max(b.min_x - a.max_x);
    let dy = (a.min_y - b.max_y).max(0.0).max(b.min_y - a.max_y);
    let dz = (a.min_z - b.max_z).max(0.0).max(b.min_z - a.max_z);
    dx * dx + dy * dy + dz * dz
}

/// `LivingEntity.isAffectedByPotions` (living and not dying; never armor stands) of the living
/// entities `getEntitiesOfClass` finds (no spectators).
fn is_affected_by_potions(ents: &Entities, id: EntityId) -> bool {
    if let Some(m) = ents.mob(id) {
        return !m.is_dead_or_dying() && m.kind != crate::mob::Kind::ArmorStand;
    }
    ents.player(id).is_some_and(|p| {
        !p.survival.is_dead_or_dying() && p.game_mode != vanillite_config::GameMode::Spectator
    })
}

/// `ThrownLingeringPotion.onHitAsPotion`: a cloud of radius 3 that shrinks to nothing in 600
/// ticks and by 0.5 per use, after a wait of 10 ticks.
pub fn spawn_lingering(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    pos: Vec3,
    owner: Option<EntityId>,
    stack: &ItemStack,
) -> EntityId {
    let radius = 3.0_f32;
    let duration = 600;
    let data = CloudData {
        radius,
        radius_on_use: -0.5,
        duration,
        wait_time: 10,
        radius_per_tick: -radius / duration as f32,
        owner,
        contents: PotionContents::of_stack(stack),
        duration_scale: potion::duration_scale(stack),
        // `applyComponentsFromItemStack`: the name and the custom data of the potion.
        custom_name: crate::item_nbt::effective_component(
            stack,
            vanillite_protocol::components::ids::CUSTOM_NAME,
        )
        .map(|v| v.into_owned().into_boxed_slice()),
        ..CloudData::default()
    };
    let id = spawn_cloud(ents, network_ids, pos, data);
    let custom_data = crate::item_nbt::effective_component(
        stack,
        vanillite_protocol::components::ids::CUSTOM_DATA,
    )
    .and_then(|v| {
        crate::item_nbt::component_to_nbt(vanillite_protocol::components::ids::CUSTOM_DATA, &v)
    })
    .and_then(|t| match t {
        Tag::Compound(c) => Some(c),
        _ => None,
    });
    if custom_data.is_some() {
        ents.set_custom_data(id, custom_data);
    }
    id
}

/// `getOwner()`: the owner, looked up again by its UUID once it is not known (after a reload,
/// `EntityReference.getEntity`).
pub fn owner(ents: &mut Entities, id: EntityId) -> Option<EntityId> {
    let c = ents.cloud(id)?;
    if let Some(o) = c.owner.filter(|&o| ents.store.contains(o)) {
        return Some(o);
    }
    let found = c.owner_uuid.and_then(|u| ents.store.by_uuid(u));
    if let Some(c) = ents.cloud_mut(id) {
        c.owner = found;
    }
    found
}

/// Adds an area effect cloud.
pub fn spawn_cloud(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    pos: Vec3,
    data: CloudData,
) -> EntityId {
    let uuid = crate::experience_orb::with_entity_random(crate::mob::random_uuid);
    let mut data = data;
    data.owner_uuid = data.owner.and_then(|o| ents.store.get(o)).map(|e| e.uuid());
    let radius = data.radius;
    let new = NewEntity::new(ids::AREA_EFFECT_CLOUD, network_ids.next(), uuid, pos);
    let parent = data.owner_uuid.unwrap_or(0);
    let id = ents.spawn_cloud(new, data);
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_dims(dims_of(radius));
    }
    ents.spawned(id, vanillite_abi::spawn_reason::DEFAULT, parent);
    id
}

/// `AreaEffectCloud.serverTick` (the entity tick of a cloud in a ticking chunk).
pub fn tick(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) {
    let Some(c) = ents.cloud_mut(id) else {
        return;
    };
    c.age += 1;
    let age = c.age;
    if c.duration != -1 && age - c.wait_time >= c.duration {
        ents.despawn(id);
        return;
    }
    let should_wait = age < c.wait_time;
    if c.waiting != should_wait {
        c.waiting = should_wait;
        mark_dirty(ents, id);
    }
    if should_wait {
        return;
    }
    let Some(c) = ents.cloud_mut(id) else {
        return;
    };
    let mut radius = c.radius;
    if c.radius_per_tick != 0.0 {
        radius += c.radius_per_tick;
        if radius < 0.5 {
            ents.despawn(id);
            return;
        }
        set_radius(ents, id, radius);
    }
    if age % 5 != 0 {
        return;
    }
    let Some(c) = ents.cloud_mut(id) else {
        return;
    };
    c.victims.retain(|&(_, until)| age < until);
    if !c.contents.has_effects() {
        c.victims.clear();
        return;
    }
    let effects = c.contents.scaled_effects(c.duration_scale);
    let delay = c.reapplication_delay;
    let owner = owner(ents, id);
    let Some((pos, area)) = ents.store.get(id).map(|e| (e.pos(), e.aabb())) else {
        return;
    };
    let mut targets = Vec::new();
    ents.store.for_each_in(&area, false, |t| targets.push(t));
    for target in targets {
        let Some(kind) = ents.store.get(target).map(|e| e.kind()) else {
            continue;
        };
        let known = ents
            .cloud(id)
            .is_some_and(|c| c.victims.iter().any(|&(v, _)| v == target));
        if known
            || !is_affected_by_potions(ents, target)
            || !effects
                .iter()
                .any(|e| super::can_be_affected(kind, e.effect))
        {
            continue;
        }
        let Some(tp) = ents.store.get(target).map(|e| e.pos()) else {
            continue;
        };
        let (xd, zd) = (tp.x - pos.x, tp.z - pos.z);
        if xd * xd + zd * zd > f64::from(radius * radius) {
            continue;
        }
        if let Some(c) = ents.cloud_mut(id) {
            c.victims.push((target, age + delay));
        }
        for e in &effects {
            if e.effect.is_instantaneous() {
                let source = HurtKind::IndirectMagic {
                    direct: Some(id),
                    owner,
                };
                with_host_from(level, ents, env, cx, target, source, |host| {
                    active::apply_instantaneous(e.effect, host, e.amplifier(), 0.5, source);
                });
            } else {
                // The cloud is the source (`addEffect(effect, this)`).
                add_to_entity_from(level, ents, env, cx, target, e.clone(), Some(id));
            }
        }
        let Some(c) = ents.cloud_mut(id) else {
            return;
        };
        if c.radius_on_use != 0.0 {
            radius += c.radius_on_use;
            if radius < 0.5 {
                ents.despawn(id);
                return;
            }
            set_radius(ents, id, radius);
        }
        let Some(c) = ents.cloud_mut(id) else {
            return;
        };
        if c.duration_on_use != 0 && c.duration != -1 {
            c.duration += c.duration_on_use;
            if c.duration <= 0 {
                ents.despawn(id);
                return;
            }
        }
    }
}

fn mark_dirty(ents: &mut Entities, id: EntityId) {
    if let Some(mut e) = ents.store.get_mut(id) {
        e.mark_data_dirty();
    }
}

/// `setRadius`: the synched radius, clamped to 0..=32, and the box (`refreshDimensions`).
pub(crate) fn set_radius(ents: &mut Entities, id: EntityId, radius: f32) {
    let radius = radius.clamp(0.0, 32.0);
    if let Some(c) = ents.cloud_mut(id) {
        c.radius = radius;
    }
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_dims(dims_of(radius));
        e.mark_data_dirty();
    }
}

/// `Arrow.doPostHurtEffects`: the effects of a tipped arrow (`potion_contents` of the arrow
/// item with its `potion_duration_scale`, 1/8 by default) on the entity it hurt.
pub fn arrow_hit(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    arrow_stack: &ItemStack,
    effect_source: EntityId,
) {
    let contents = PotionContents::of_stack(arrow_stack);
    let scale = potion::duration_scale(arrow_stack);
    // `forEachEffect(mob.addEffect(effect, effectSource))`: instant effects too, as one-tick
    // effects.
    for e in contents.scaled_effects(scale) {
        add_to_entity_from(level, ents, env, cx, target, e, Some(effect_source));
    }
}
