//! Damage, knockback and death of mobs (`LivingEntity.hurtServer`, `actuallyHurt`,
//! `knockback`, `die`), melee attacks of mobs (`Mob.doHurtTarget`) and of players
//! (`Player.attack`), and experience drops (`ExperienceOrb.award`).

use vanillite_base::Vec3;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{DamageEvent, EntityEvent};
use vanillite_world::entity::{EntityFlags, EntityId, Pose};

use super::living::event;
use super::{Kind, MobContext, kinds};
use crate::advancements::predicate::EntityView;
use crate::advancements::trigger::{Event, TriggerKind};
use crate::advancements::view;
use crate::entities::Entities;
use crate::level::Level;
use crate::survival::combat::{AttackerName, SourceNames};
use crate::survival::damage::{DamageKind, DamageSource, tag};
use crate::survival::player::{self, Env};
use crate::survival::rules::Rule;
use crate::survival::{Effect, play_sound};
use vanillite_protocol::text::{Component, HoverEvent};

/// `LivingEntity.knockback(power, xd, zd)`: the new velocity, `None` when the resistance
/// absorbs it.
pub fn knockback(
    velocity: Vec3,
    on_ground: bool,
    power: f64,
    xd: f64,
    zd: f64,
    resistance: f64,
    random: &mut LegacyRandom,
) -> Option<Vec3> {
    let power = power * (1.0 - resistance);
    if power <= 0.0 {
        return None;
    }
    let (mut xd, mut zd) = (xd, zd);
    while xd * xd + zd * zd < f64::from(1.0e-5_f32) {
        xd = (random.next_double() - random.next_double()) * 0.01;
        zd = (random.next_double() - random.next_double()) * 0.01;
    }
    let d = Vec3::new(xd, 0.0, zd).normalize() * power;
    Some(Vec3::new(
        velocity.x / 2.0 - d.x,
        if on_ground {
            (velocity.y / 2.0 + power).min(0.4)
        } else {
            velocity.y
        },
        velocity.z / 2.0 - d.z,
    ))
}

/// A damage source caused by an entity (`damageSources().mobAttack(mob)` and friends): the
/// source position is the attacker's.
pub fn entity_source(
    ents: &Entities,
    kind: DamageKind,
    cause: EntityId,
    direct: EntityId,
) -> DamageSource {
    let mut source = DamageSource::of(kind);
    source.cause = Some(cause);
    source.direct = Some(direct);
    if let Some(e) = ents.store.get(direct) {
        source.position = Some(e.pos());
    }
    if let Some(m) = ents.mob(cause) {
        source.cause_is_mob = true;
        source.cause_name = Some(m.kind.translation_key());
    }
    source
}

/// `hurtServer` of any living entity, decoration or vehicle: players go through the survival
/// logic, mobs through [`hurt_mob`]. Returns whether the damage was taken.
pub fn hurt(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: DamageSource,
    amount: f32,
) -> bool {
    let source = with_projectile_motion(ents, source);
    if ents.mob(target).is_some() {
        return hurt_mob(level, ents, env, cx, target, source, amount);
    }
    if ents.special(target).is_some() {
        return super::boss::special::hurt(ents, env, cx, target, &source);
    }
    if ents.decor(target).is_some() {
        return crate::decoration::hurt(level, ents, env, cx, target, &source);
    }
    if ents
        .missile(target)
        .is_some_and(|d| d.kind == super::missile::MissileKind::ShulkerBullet && !d.removed)
    {
        return super::fam::illager::missiles::hurt_bullet(ents, env, cx, target);
    }
    // `VehicleEntity.hurtServer`.
    if ents.minecart(target).is_some() {
        return crate::minecart::hurt(ents, env, cx, target, source, amount);
    }
    if ents.boat(target).is_some() {
        return crate::vehicle::hurt_boat(ents, env, cx, target, source, amount);
    }
    if ents.player(target).is_none() {
        return false;
    }
    let mut source = with_blocking_facts(ents, source);
    let attacker_of_block = source.direct.filter(|_| source.own_blocked_by_item);
    // `resolvePlayerResponsibleForDamage`: the causing player, or a tame wolf's owner.
    source.responsible_player = source.cause.and_then(|c| {
        if ents.player(c).is_some() {
            return Some(c);
        }
        ents.mob(c)
            .filter(|m| m.kind == Kind::Wolf && m.tame)
            .and_then(|m| super::tame::owner_of(ents, m))
    });
    // `ServerPlayer.hurtServer`: a player-caused hit (also through projectiles, potions and
    // arrows of a player) fails `canHarmPlayer` (the pvp rule, teams without friendly fire).
    let harmer = source
        .cause
        .filter(|&c| ents.player(c).is_some())
        .or_else(|| {
            source
                .direct
                .and_then(|d| ents.arrow(d))
                .and_then(|a| a.owner)
                .filter(|&o| ents.player(o).is_some())
        });
    if harmer.is_some_and(|h| !super::attack::can_harm_player(ents, env, target, h)) {
        return false;
    }
    let armor = super::attack::armor_effectiveness(ents, env.types, target, &source);
    let attackers = if player::wants_attackers(env.hooks) {
        let uuid_of = |id: Option<EntityId>| id.and_then(|i| ents.store.get(i)).map(|e| e.uuid());
        (uuid_of(source.cause), uuid_of(source.direct))
    } else {
        (None, None)
    };
    let names = source_names(ents, env, &source);
    let Entities { store, players, .. } = ents;
    let Some(mut entity) = store.get_mut(target) else {
        return false;
    };
    let row = entity.as_ref().kind_row();
    let Some(data) = players.get_mut(row) else {
        return false;
    };
    let was_alive = !data.survival.is_dead_or_dying();
    let hurt = player::hurt_with(
        &mut entity,
        data,
        env,
        source,
        amount,
        armor.as_deref(),
        attackers,
        names,
    );
    let died = was_alive && data.survival.is_dead_or_dying();
    let blocked_by_item = data.survival.blocked_by_item.take();
    // The killer's type while it is still there (`ENTITY_KILLED_BY`).
    if died {
        let mut credit = None;
        for effect in env.out.iter_mut().rev() {
            if let Effect::Died {
                victim,
                killer,
                killer_kind,
                ..
            } = effect
                && *victim == target
            {
                *killer_kind = killer.and_then(|k| ents.store.get(k)).map(|e| e.kind());
                credit = *killer;
                break;
            }
        }
        // `ServerPlayer.die` → `createWitherRose(killer)`.
        if let Some(pos) = ents.store.get(target).map(|e| e.pos()) {
            create_wither_rose(level, ents, env, cx, pos, credit);
        }
    }
    view::resolve_combat(ents, env.types, target);
    // `blockUsingItem` → the attacker's own `blockedByItem`.
    if blocked_by_item.is_some()
        && let Some(attacker) = attacker_of_block
    {
        let mut t = super::Tick {
            level,
            ents,
            env,
            cx,
            id: attacker,
        };
        match t.mob_ref().kind {
            Kind::Ravager => super::fam::illager::ravager::blocked_by_item(&mut t, target),
            Kind::Hoglin | Kind::Zoglin if !t.mob_ref().baby => {
                super::fam::brainy::hoglin::throw_target(&mut t, target);
            }
            _ => {}
        }
    }
    // `ServerPlayer.die` → `tellNeutralMobsThatIDied`: at once, so an attacker calms down
    // within its own tick.
    if died && env.rules.bool(Rule::FORGIVE_DEAD_PLAYERS) {
        super::common::tell_neutral_mobs_player_died(ents, target);
    }
    hurt
}

/// What `applyItemBlocking` of a player needs of the entities: whether a piercing arrow hit
/// and, for a mob's melee hit, `getSecondsToDisableBlocking` of its main hand weapon.
fn with_blocking_facts(ents: &Entities, mut source: DamageSource) -> DamageSource {
    let Some(direct) = source.direct else {
        return source;
    };
    source.piercing_arrow = ents.arrow(direct).is_some_and(|a| a.pierce_level > 0);
    if source.cause != Some(direct) {
        return source;
    }
    // `getSecondsToDisableBlocking`: the `weapon` of the main hand item, unless the attacker
    // uses the item of its other hand (`getActiveItem`).
    let weapon = if let Some(m) = ents.mob(direct) {
        if m.kind == Kind::Warden {
            // `Warden.getSecondsToDisableBlocking`.
            source.disable_shield_seconds = 5.0;
            return source;
        }
        Some(m.equipment.get(crate::item::EquipmentSlot::MainHand))
    } else {
        ents.player(direct).and_then(|p| {
            let off_hand = p.survival.using_item.is_some_and(|u| {
                u.hand == vanillite_protocol::packets::play::serverbound::Hand::OffHand
            });
            (!off_hand).then(|| p.inventory.selected_item())
        })
    };
    source.own_blocked_by_item = ents
        .mob(direct)
        .is_some_and(|m| matches!(m.kind, Kind::Hoglin | Kind::Zoglin | Kind::Ravager));
    if let Some(weapon) = weapon {
        source.disable_shield_seconds =
            crate::item::weapon(weapon).map_or(0.0, |w| w.disable_blocking_for_seconds);
    }
    source
}

/// The motion of a projectile that hits (arrows, tridents, fireballs, thrown items, …: every
/// `Projectile` but potions and fireworks, which push away from their position), for the
/// knockback direction.
fn with_projectile_motion(ents: &Entities, mut source: DamageSource) -> DamageSource {
    if source.projectile_motion.is_some() {
        return source;
    }
    if let Some(e) = source.direct.and_then(|d| ents.store.get(d))
        && matches!(
            e.kind().name(),
            "minecraft:arrow"
                | "minecraft:spectral_arrow"
                | "minecraft:trident"
                | "minecraft:fireball"
                | "minecraft:small_fireball"
                | "minecraft:dragon_fireball"
                | "minecraft:wither_skull"
                | "minecraft:wind_charge"
                | "minecraft:breeze_wind_charge"
                | "minecraft:snowball"
                | "minecraft:egg"
                | "minecraft:ender_pearl"
                | "minecraft:experience_bottle"
                | "minecraft:llama_spit"
                | "minecraft:shulker_bullet"
                | "minecraft:fishing_bobber"
        )
    {
        source.projectile_motion = Some(e.velocity());
    }
    source
}

/// `isInvulnerableTo` of a mob: also the `Invulnerable` flag, unless the damage bypasses it
/// or comes from a creative player (`isCreativePlayer`).
fn invulnerable_to(
    ents: &Entities,
    env: &Env<'_>,
    target: EntityId,
    source: &DamageSource,
) -> bool {
    let types = env.types;
    let kind = source.kind;
    let Some(e) = ents.store.get(target) else {
        return true;
    };
    let Some(m) = ents.mob(target) else {
        return true;
    };
    let creative = source
        .cause
        .and_then(|c| ents.player(c))
        .is_some_and(|p| p.game_mode == vanillite_config::GameMode::Creative);
    m.removed
        || super::entity_nbt::is_invulnerable(ents, target)
            && !types.is(kind, tag::BYPASSES_INVULNERABILITY)
            && !creative
        || crate::enchant::location::frost_walker_immune(
            types,
            kind,
            m.equipment.get(crate::item::EquipmentSlot::Feet),
        )
        || types.is(kind, tag::IS_FIRE)
            && (e.kind().info().fire_immune
                || m.effects.has(crate::effect::MobEffect::FIRE_RESISTANCE))
        || types.is(kind, tag::IS_FALL) && m.kind.fall_damage_immune()
}

/// `LivingEntity.hurtServer` of a mob.
pub fn hurt_mob(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: DamageSource,
    amount: f32,
) -> bool {
    let source = with_projectile_motion(ents, source);
    // `EnderMan.hurtServer`: after `super.hurtServer`, whatever it returned (also while
    // invulnerable after a hit), damage not caused by a living entity makes it teleport.
    // A projectile (not while riding) makes it dodge instead; a thrown potion only hurts with
    // water and then always makes it try to teleport away (`repeatedlyTryToTeleport`).
    let enderman_after = ents.mob(target).is_some_and(|m| m.kind == Kind::Enderman)
        && !(env.types.is(source.kind, tag::IS_PROJECTILE) && ents.vehicle_of(target).is_none())
        && !invulnerable_to(ents, env, target, &source)
        && !immune_to_damage(ents, env, target, &source);
    let thrown_potion = if enderman_after {
        enderman_potion_hurts(ents, source.direct)
    } else {
        None
    };
    let took = hurt_mob_inner(level, ents, env, cx, target, source, amount);
    match (enderman_after, thrown_potion) {
        (true, None) => kinds::after_hurt(level, ents, env, cx, target, source),
        (true, Some(true)) => kinds::enderman_dodge(level, ents, env, cx, target),
        _ => {}
    }
    took
}

#[allow(clippy::too_many_lines)]
fn hurt_mob_inner(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: DamageSource,
    amount: f32,
) -> bool {
    let kind = source.kind;
    let types = env.types;
    if ents.mob(target).is_some_and(|m| m.kind == Kind::ArmorStand) {
        return crate::decoration::stand::hurt(level, ents, env, cx, target, &source);
    }
    if super::entity_nbt::invulnerable_to(ents, types, target, &source) {
        return false;
    }
    if let Some(took) =
        super::fam::undead::cubes::sulfur_hurt(level, ents, env, target, &source, amount)
    {
        return took;
    }
    if invulnerable_to(ents, env, target, &source) || immune_to_damage(ents, env, target, &source) {
        return false;
    }
    if ents.mob(target).is_some_and(|m| m.is_dead_or_dying()) {
        return false;
    }
    let amount = match super::boss::filter_hurt(level, ents, env, cx, target, &source, amount) {
        Ok(amount) => amount,
        Err(took) => return took,
    };
    let Some(m) = ents.mob_mut(target) else {
        return false;
    };
    if m.is_dead_or_dying() {
        return false;
    }
    if types.is(kind, tag::IS_FIRE) && m.effects.has(crate::effect::MobEffect::FIRE_RESISTANCE) {
        return false;
    }
    let mob_kind = m.kind;
    let mut amount = amount;
    // `EnderMan.hurtServer`: a thrown potion only hurts with water and always makes it try to
    // teleport away.
    let thrown_potion = if mob_kind == Kind::Enderman {
        enderman_potion_hurts(ents, source.direct)
    } else {
        None
    };
    if mob_kind == Kind::Enderman
        && types.is(kind, tag::IS_PROJECTILE)
        && ents.vehicle_of(target).is_none()
    {
        // `EnderMan.hurtServer`: projectiles make it teleport instead (not while riding).
        kinds::enderman_dodge(level, ents, env, cx, target);
        return false;
    }
    if thrown_potion == Some(false) {
        kinds::enderman_dodge(level, ents, env, cx, target);
        return false;
    }
    if mob_kind.family() != super::Family::Classic {
        let mut h = super::fam::Hurt {
            level,
            ents,
            env,
            cx,
            target,
            source,
        };
        match super::fam::hurt_filter(&mut h, mob_kind, amount) {
            Some(a) => amount = a,
            None => return false,
        }
    }
    // Plugins see the damage after the kind's own filters.
    if let Some(e) = ents.store.get(target) {
        let uuid_of = |id: Option<EntityId>| id.and_then(|i| ents.store.get(i)).map(|e| e.uuid());
        match env.hooks.entity_damage(
            crate::hooks::HookWorld::split(level, ents),
            e.uuid(),
            e.kind().name(),
            &types.get(kind).id,
            amount,
            uuid_of(source.cause),
            uuid_of(source.direct),
        ) {
            Some(changed) => amount = changed,
            None => return false,
        }
    }
    let Some(m) = ents.mob_mut(target) else {
        return false;
    };
    m.no_action_time = 0;
    let mut damage = amount.max(0.0);
    // `#damages_helmet` with a helmet on: three quarters (`hurtHelmet` wears only players').
    if types.is(kind, tag::DAMAGES_HELMET)
        && !m.equipment.get(crate::item::EquipmentSlot::Head).is_empty()
    {
        damage *= 0.75;
    }
    if !damage.is_finite() {
        damage = f32::MAX;
    }

    let took_full = if m.damage_cooldown > 10 && !types.is(kind, tag::BYPASSES_COOLDOWN) {
        if damage <= m.last_hurt {
            return false;
        }
        let delta = damage - m.last_hurt;
        actually_hurt(ents, env, target, source, delta);
        if let Some(m) = ents.mob_mut(target) {
            m.last_hurt = damage;
        }
        false
    } else {
        m.last_hurt = damage;
        m.damage_cooldown = 20;
        actually_hurt(ents, env, target, source, damage);
        if let Some(m) = ents.mob_mut(target) {
            m.hurt_time = 10;
        }
        true
    };
    // What the kinds' `actuallyHurt` overrides do after `super.actuallyHurt`.
    if mob_kind.family() != super::Family::Classic {
        let mut h = super::fam::Hurt {
            level,
            ents,
            env,
            cx,
            target,
            source,
        };
        super::fam::actually_hurt(&mut h, mob_kind);
    }
    // `resolveMobResponsibleForDamage` / `resolvePlayerResponsibleForDamage`.
    if let Some(cause) = source.cause {
        let is_player = ents.player(cause).is_some();
        let living =
            (is_player || ents.mob(cause).is_some()) && angers(ents, env.types, target, &source);
        // `resolvePlayerResponsibleForDamage`: a tame wolf's hit counts for its owner (none
        // without an owner).
        let wolf_owner = ents
            .mob(cause)
            .filter(|m| m.kind == Kind::Wolf && m.tame)
            .map(|m| super::tame::owner_of(ents, m));
        if let Some(owner) = wolf_owner
            && let Some(m) = ents.mob_mut(target)
        {
            m.last_hurt_by_player = owner;
            m.last_hurt_by_player_time = if owner.is_some() { 100 } else { 0 };
        }
        if let Some(m) = ents.mob_mut(target) {
            if living {
                m.last_hurt_by_mob = Some(cause);
                m.last_hurt_by_mob_time = m.tick_count;
            }
            if is_player {
                m.last_hurt_by_player = Some(cause);
                m.last_hurt_by_player_time = 100;
            }
        }
    }
    let Some(e) = ents.store.get(target) else {
        return false;
    };
    let network_id = e.network_id().0;
    let pos = e.pos();
    if took_full {
        let cause_id = source
            .cause
            .and_then(|c| ents.store.get(c))
            .map_or(-1, |c| c.network_id().0);
        let direct_id = source
            .direct
            .and_then(|c| ents.store.get(c))
            .map_or(-1, |c| c.network_id().0);
        env.out.push(Effect::Tracking {
            entity: target,
            with_self: false,
            packet: SharedPacket::of(&DamageEvent {
                entity_id: network_id,
                source_type: i32::from(kind.0),
                source_cause_id: cause_id,
                source_direct_id: direct_id,
                source_position: None,
            }),
        });
        if let Some(mut e) = ents.store.get_mut(target)
            && !types.is(kind, tag::NO_IMPACT)
        {
            e.set_flags(EntityFlags::HURT_MARKED, true);
        }
        if !types.is(kind, tag::NO_KNOCKBACK) {
            // Against the projectile's motion, else away from `getSourcePosition`: the explicit
            // position, else the direct entity's (a thrown potion pushes away from where it
            // splashed).
            let (xd, zd) = default_knockback_direction(ents, &source, pos);
            let by = source.cause.map(|c| (c, damage, false));
            knock_mob(ents, env, target, f64::from(0.4_f32), xd, zd, by);
        }
    }
    let dead = ents
        .mob(target)
        .is_some_and(super::MobData::is_dead_or_dying)
        && !mob_totem(level, ents, env, cx, target, source.kind);
    if dead {
        if took_full {
            let sound = ents.mob(target).map(kinds::death_sound);
            if let Some(sound) = sound {
                mob_sound(ents, env, target, sound);
            }
        }
        die(level, ents, env, cx, target, source);
    } else if took_full {
        let sound = ents.mob_mut(target).map(|m| {
            m.ambient_sound_time = -m.kind.ambient_sound_interval();
            kinds::hurt_sound(m)
        });
        if let Some(sound) = sound {
            mob_sound(ents, env, target, sound);
        }
    }
    if let Some(m) = ents.mob_mut(target) {
        m.last_damage = Some((kind, env.game_time));
        m.last_damage_cause = source.cause;
    }
    // `onMobHurt` of the effects (Infested).
    let view = ents.store.get(target).map(|e| (e.rot(), e.dims().height));
    if let Some((rot, height)) = view
        && let Some(m) = ents.mob_mut(target).filter(|m| !m.effects.is_empty())
    {
        let look = crate::effect::living::view_vector(rot.x_rot, rot.y_rot);
        let fx = std::mem::take(&mut m.effects);
        crate::effect::living::on_hurt(&fx, &mut m.random, env.random, pos, height, look, env.out);
        m.effects = fx;
    }
    // What the kinds' `hurtServer` overrides do after `super.hurtServer` (even when it died).
    if mob_kind.family() == super::Family::Classic {
        if mob_kind == Kind::Zombie {
            // `Zombie.hurtServer`: reinforcements on Hard.
            let mut h = super::fam::Hurt {
                level,
                ents,
                env,
                cx,
                target,
                source,
            };
            super::fam::undead::zombies::reinforcements(&mut h);
        }
    } else {
        let mut h = super::fam::Hurt {
            level,
            ents,
            env,
            cx,
            target,
            source,
        };
        super::fam::after_hurt(&mut h, mob_kind, damage);
    }
    // `PLAYER_HURT_ENTITY` of the player that caused it.
    if let Some(player) = source.cause.filter(|&c| ents.player(c).is_some()) {
        let types = env.types;
        view::fire(ents, player, TriggerKind::PLAYER_HURT_ENTITY, |ents| {
            let victim = view::snapshot(ents, target)
                .unwrap_or_else(|| Box::new(EntityView::new(mob_kind.name(), pos)));
            Event::PlayerHurtEntity(
                victim,
                view::damage_event(ents, types, &source, damage, damage, false),
            )
        });
    }
    true
}

/// The direction of `dealDefaultKnockback` for a mob at `pos`: against the projectile's
/// motion, else away from `getSourcePosition` (the explicit position, else the direct
/// entity's).
pub fn default_knockback_direction(
    ents: &Entities,
    source: &DamageSource,
    pos: Vec3,
) -> (f64, f64) {
    match source.projectile_motion {
        Some(m) => (-m.x, -m.z),
        None => source
            .position
            .or_else(|| {
                source
                    .direct
                    .and_then(|d| ents.store.get(d))
                    .map(|e| e.pos())
            })
            .map_or((0.0, 0.0), |p| (p.x - pos.x, p.z - pos.z)),
    }
}

/// `knockback(power, xd, zd, source, damage, comesFromEffect)` of a mob; `by` is the
/// source's causing entity, the damage and `comesFromEffect` (a sulfur cube holding a block
/// takes its own knockback from an entity).
pub fn knock_mob(
    ents: &mut Entities,
    env: &mut Env<'_>,
    target: EntityId,
    power: f64,
    xd: f64,
    zd: f64,
    by: Option<(EntityId, f32, bool)>,
) {
    if let Some((cause, damage, from_effect)) = by
        && super::fam::undead::cubes::knockback(
            ents,
            env,
            target,
            power,
            xd,
            zd,
            cause,
            damage,
            from_effect,
        )
    {
        return;
    }
    apply_knockback(ents, target, power, xd, zd);
}

/// `knockback(power, xd, zd)` of a mob.
pub fn apply_knockback(ents: &mut Entities, target: EntityId, power: f64, xd: f64, zd: f64) {
    if super::boss::dragon::ignores_knockback(ents, target) {
        return;
    }
    let Some(resistance) = ents.mob(target).map(|m| {
        let base = m.attrs.knockback_resistance;
        super::equipment::attribute(m, "minecraft:knockback_resistance", base, 0.0, 1.0)
    }) else {
        return;
    };
    let Entities { store, mobs, .. } = ents;
    let Some(mut e) = store.get_mut(target) else {
        return;
    };
    let row = e.as_ref().kind_row();
    let Some(m) = mobs.get_mut(row) else {
        return;
    };
    let on_ground = e.as_ref().flags().contains(EntityFlags::ON_GROUND);
    if let Some(v) = knockback(
        e.as_ref().velocity(),
        on_ground,
        power,
        xd,
        zd,
        resistance,
        &mut m.random,
    ) {
        e.set_velocity(v);
        e.set_flags(EntityFlags::HAS_IMPULSE, true);
    }
}

/// The armour and armour toughness of a mob: its attributes with the modifiers of its
/// equipment.
fn mob_armor_values(m: &super::MobData) -> (f32, f32) {
    let mut armor = Vec::new();
    let mut toughness = Vec::new();
    for slot in (0..8).filter_map(crate::item::EquipmentSlot::from_ordinal) {
        let stack = m.equipment.get(slot);
        if stack.is_empty() {
            continue;
        }
        crate::item::attribute_modifiers(stack, slot, "minecraft:armor", &mut armor);
        crate::item::attribute_modifiers(stack, slot, "minecraft:armor_toughness", &mut toughness);
    }
    let value = |base: f64, mods: &[_], max: f64| {
        if mods.is_empty() {
            base
        } else {
            crate::item::attribute_value(base, mods, 0.0, max)
        }
    };
    #[allow(clippy::cast_possible_truncation)]
    (
        value(m.attrs.armor, &armor, 30.0) as f32,
        value(m.attrs.armor_toughness, &toughness, 20.0) as f32,
    )
}

/// `EnchantmentHelper.getDamageProtection` of a mob's equipment (Protection, Fire Protection,
/// ...).
fn mob_protection(
    ents: &Entities,
    env: &mut Env<'_>,
    target: EntityId,
    source: &DamageSource,
) -> f32 {
    let Some(m) = ents.mob(target) else {
        return 0.0;
    };
    let equipment: Vec<(crate::item::EquipmentSlot, &crate::item::ItemStack)> = (0..8)
        .filter_map(crate::item::EquipmentSlot::from_ordinal)
        .map(|s| (s, m.equipment.get(s)))
        .filter(|(_, s)| !s.is_empty() && !crate::item::enchantments(s).is_empty())
        .collect();
    if equipment.is_empty() {
        return 0.0;
    }
    let combat = super::attack::combat_data();
    let tags: &dyn crate::predicate::EntityTags = &*combat;
    let cx = super::attack::damage_context(ents, env.types, tags, target, source);
    combat
        .enchants
        .damage_protection(&equipment, &cx, env.random)
}

/// `EnchantmentHelper.isImmuneToDamage` of a mob's equipment (Frost Walker on magma).
fn immune_to_damage(
    ents: &Entities,
    env: &mut Env<'_>,
    target: EntityId,
    source: &DamageSource,
) -> bool {
    let Some(m) = ents.mob(target) else {
        return false;
    };
    let equipment: Vec<(crate::item::EquipmentSlot, &crate::item::ItemStack)> = (0..8)
        .filter_map(crate::item::EquipmentSlot::from_ordinal)
        .map(|s| (s, m.equipment.get(s)))
        .filter(|(_, s)| !s.is_empty() && !crate::item::enchantments(s).is_empty())
        .collect();
    if equipment.is_empty() {
        return false;
    }
    let combat = super::attack::combat_data();
    let tags: &dyn crate::predicate::EntityTags = &*combat;
    let cx = super::attack::damage_context(ents, env.types, tags, target, source);
    combat
        .enchants
        .immune_to_damage(&equipment, &cx, env.random)
}

/// `LivingEntity.actuallyHurt` of a mob: armour, then health.
fn actually_hurt(
    ents: &mut Entities,
    env: &mut Env<'_>,
    target: EntityId,
    source: DamageSource,
    damage: f32,
) {
    let types = env.types;
    let effectiveness = super::attack::armor_effectiveness(ents, types, target, &source);
    // `EnchantmentHelper.getDamageProtection` of the mob's equipment.
    let protection = if types.is(source.kind, tag::BYPASSES_EFFECTS)
        || types.is(source.kind, tag::BYPASSES_ENCHANTMENTS)
    {
        0.0
    } else {
        mob_protection(ents, env, target, &source)
    };
    let Some(m) = ents.mob_mut(target) else {
        return;
    };
    // `Animal.actuallyHurt`: `resetLove()` (only animals are ever in love).
    m.in_love = 0;
    let mut damage = damage;
    if !types.is(source.kind, tag::BYPASSES_ARMOR) {
        // `getArmorValue` and `ARMOR_TOUGHNESS` with the equipment's modifiers.
        let (armor, toughness) = mob_armor_values(m);
        damage = crate::survival::combat::damage_after_absorb_with(
            damage,
            armor,
            toughness,
            effectiveness.as_deref(),
        );
    }
    // `getDamageAfterMagicAbsorb`: Resistance.
    let mut resisted = 0.0;
    if !types.is(source.kind, tag::BYPASSES_EFFECTS) {
        if !types.is(source.kind, tag::BYPASSES_RESISTANCE) {
            let before = damage;
            damage = m.effects.resistance_damage(damage);
            resisted = before - damage;
        }
        if damage <= 0.0 {
            damage = 0.0;
        } else if protection > 0.0 {
            damage = crate::survival::combat::damage_after_magic_absorb(damage, protection);
        }
    }
    damage = super::fam::absorb(m, &source, env, damage);
    // Absorption takes the damage first.
    let remaining = (damage - m.absorption).max(0.0);
    m.absorption -= damage - remaining;
    let absorbed = damage - remaining;
    let damage = remaining;
    if damage != 0.0 {
        m.health = (m.health - damage).max(0.0);
        if let Some(mut e) = ents.store.get_mut(target) {
            e.mark_data_dirty();
        }
        let event = crate::level::game_event::events().entity_damage;
        super::animal::entity_game_event(ents, env, target, event, Some(target));
    }
    // `DAMAGE_DEALT_RESISTED` and `DAMAGE_DEALT_ABSORBED` of the player that caused it.
    if let Some(p) = source.cause.and_then(|c| ents.player_mut(c)) {
        if let Some(v) = crate::survival::player::damage_stat(resisted) {
            p.stats
                .award(crate::stats::custom::DAMAGE_DEALT_RESISTED, v);
        }
        if let Some(v) = crate::survival::player::damage_stat(absorbed) {
            p.stats
                .award(crate::stats::custom::DAMAGE_DEALT_ABSORBED, v);
        }
    }
}

/// `resolveMobResponsibleForDamage` / `resolvePlayerResponsibleForDamage` without damage
/// (`Creaking.blameSourceForDamage`): the responsible player, if any.
pub fn blame(
    ents: &mut Entities,
    types: &crate::survival::damage::DamageTypes,
    target: EntityId,
    source: &DamageSource,
) -> Option<EntityId> {
    let cause = source.cause?;
    let is_player = ents.player(cause).is_some();
    let living = (is_player || ents.mob(cause).is_some()) && angers(ents, types, target, source);
    let m = ents.mob_mut(target)?;
    if living {
        m.last_hurt_by_mob = Some(cause);
        m.last_hurt_by_mob_time = m.tick_count;
    }
    if is_player {
        m.last_hurt_by_player = Some(cause);
        m.last_hurt_by_player_time = 100;
    }
    is_player.then_some(cause)
}

/// The conditions of `resolveMobResponsibleForDamage` on the damage: not `#no_anger`
/// (`mob_attack_no_aggro`), and no wind charge damage to a `#no_anger_from_wind_charge` mob.
fn angers(
    ents: &Entities,
    types: &crate::survival::damage::DamageTypes,
    target: EntityId,
    source: &DamageSource,
) -> bool {
    let id = types.get(source.kind).id.as_str();
    if id == "minecraft:mob_attack_no_aggro" {
        return false;
    }
    !(id == "minecraft:wind_charge"
        && ents.store.get(target).is_some_and(|e| {
            crate::effect::entity_in_tag(e.kind(), "minecraft:no_anger_from_wind_charge")
        }))
}

/// `checkTotemDeathProtection` of a dying mob: a death protection item in a hand is used up
/// and the mob lives on with one health point.
fn mob_totem(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    kind: crate::survival::damage::DamageKind,
) -> bool {
    use crate::item::EquipmentSlot;
    if !crate::totem::can_protect(env.types, kind) {
        return false;
    }
    let Some(m) = ents.mob(target) else {
        return false;
    };
    let Some((slot, effects)) = crate::totem::protecting_hand(
        m.equipment.get(EquipmentSlot::MainHand),
        m.equipment.get(EquipmentSlot::OffHand),
    ) else {
        return false;
    };
    let kind_name = m.kind.type_id().name();
    if let Some(uuid) = ents.store.get(target).map(|e| e.uuid())
        && !env
            .hooks
            .entity_resurrect(crate::hooks::HookWorld::split(level, ents), uuid, kind_name)
    {
        return false;
    }
    let mut rolls = Vec::new();
    if let Some(m) = ents.mob_mut(target) {
        let mut stack = m.equipment.get(slot).clone();
        stack.shrink(1);
        m.equipment.set(slot, stack);
        m.health = 1.0;
        // `ApplyStatusEffectsConsumeEffect`: one roll of the mob's random per entry.
        for e in &effects {
            if matches!(e, crate::effect::consume::ConsumeEffect::Apply(..)) {
                rolls.push(m.random.next_float());
            }
        }
    }
    let mob_kind = ents
        .mob(target)
        .map(|m| (m.kind.type_id(), m.sound_source()));
    if let Some((mob_kind, source)) = mob_kind {
        let mut sounds = Vec::new();
        crate::effect::living::with_host(level, ents, env, cx, target, |host| {
            let mut rolls = rolls.into_iter();
            let mut roll = || rolls.next().unwrap_or(0.0);
            crate::totem::apply_effects(&effects, mob_kind, host, &mut roll, &mut |s| {
                sounds.push(s.to_owned());
            });
        });
        if let Some(at) = ents.store.get(target).map(|e| e.pos()) {
            for s in sounds {
                crate::totem::play_consume_sound(env.out, at, &s, source);
            }
        }
    }
    if let Some(mut e) = ents.store.get_mut(target) {
        e.mark_data_dirty();
    }
    if let Some(network_id) = ents.store.get(target).map(|e| e.network_id().0) {
        crate::totem::animation(env.out, target, network_id);
    }
    ents.equipment_updates.push(target);
    true
}

/// `blameSourceForDamage(source)` and `die(source)` of a mob killed without damage (a
/// creaking whose heart was broken).
pub fn blame_and_die(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: DamageSource,
) {
    blame(ents, env.types, target, &source);
    die(level, ents, env, cx, target, source);
}

/// `LivingEntity.die` of a mob.
fn die(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: DamageSource,
) {
    let Some(m) = ents.mob_mut(target) else {
        return;
    };
    if m.removed || m.dead {
        return;
    }
    // `TamableAnimal.die`: the death message goes to the owner first.
    if let Some(m) = ents.mob(target)
        && m.tame
        && matches!(
            m.kind,
            Kind::Wolf | Kind::Cat | Kind::Parrot | Kind::Nautilus | Kind::ZombieNautilus
        )
        && env.rules.bool(Rule::SHOW_DEATH_MESSAGES)
    {
        let name = attacker_name(ents, env, target).map_or_else(|| mob_name(m), |a| a.name);
        // `getKillCredit`.
        let credit = m
            .last_hurt_by_player
            .or(m.last_hurt_by_mob)
            .and_then(|c| attacker_name(ents, env, c))
            .map(|a| a.name);
        if let Some(owner) = super::tame::owner_of(ents, m).filter(|&o| ents.player(o).is_some()) {
            let mut tracker = crate::survival::combat::CombatTracker::default();
            tracker.record_damage(
                crate::survival::combat::CombatEntry {
                    source,
                    damage: 0.0,
                    fall_location: None,
                    fall_distance: 0.0,
                    names: source_names(ents, env, &source),
                },
                0,
                false,
                false,
            );
            env.out.push(Effect::To {
                player: owner,
                packet: SharedPacket::of(
                    &vanillite_protocol::packets::play::clientbound::SystemChat {
                        content: tracker.death_message(env.types, name, credit),
                        overlay: false,
                    },
                ),
            });
        }
    }
    let Some(m) = ents.mob_mut(target) else {
        return;
    };
    m.dead = true;
    let mob_kind = m.kind;
    if let Some(e) = ents.store.get(target) {
        let killer = source
            .cause
            .and_then(|c| ents.store.get(c))
            .map(|c| c.uuid());
        env.hooks.entity_death(
            e.uuid(),
            e.kind().name(),
            &env.types.get(source.kind).id,
            killer,
        );
    }
    if mob_kind.family() != super::Family::Classic {
        let mut h = super::fam::Hurt {
            level,
            ents,
            env,
            cx,
            target,
            source,
        };
        super::fam::on_die(&mut h, mob_kind);
    }
    let Some(m) = ents.mob_mut(target) else {
        return;
    };
    let killed_by_player = m.last_hurt_by_player_time > 0;
    // `shouldDropLoot`: `Monster` drops as a baby too (the hoglin is an animal);
    // `shouldDropExperience`: `Monster` and `Hoglin` give experience as babies too.
    let monster = m.kind.is_monster();
    let loot_baby = m.baby && !(monster && m.kind != Kind::Hoglin);
    let baby = m.baby && !monster;
    // `getExperienceReward` (it draws from the mob's random) only where Vanilla asks for it:
    // a sculk catalyst at the death, else `dropExperience` after the loot.
    let reward_of = |ents: &mut Entities| {
        let reward = ents.mob_mut(target).map_or(0, kinds::experience_reward);
        if baby { 0 } else { reward }
    };
    if let Some(cause) = source.cause
        && let Some(p) = ents.player_mut(cause)
    {
        // `awardKillScore`.
        p.survival.score = p.survival.score.saturating_add(1);
    }
    let Some(e) = ents.store.get(target) else {
        return;
    };
    let pos = e.pos();
    let network_id = e.network_id().0;
    // `getKillCredit().awardKillScore(this, source)`: the player that hurt it last, else the
    // last attacking mob.
    let credit = ents.mob(target).and_then(|m| {
        m.last_hurt_by_player
            .filter(|_| m.last_hurt_by_player_time > 0)
            .or(m.last_hurt_by_mob)
    });
    // `ServerPlayer.awardKillScore`: `PLAYER_KILLED_ENTITY` of the player with the credit,
    // before the death loot (the victim still wears its equipment).
    if let Some(player) = credit
        .or(source.cause)
        .filter(|&k| k != target && ents.player(k).is_some())
    {
        let types = env.types;
        view::fire(ents, player, TriggerKind::PLAYER_KILLED_ENTITY, |ents| {
            let victim = view::snapshot(ents, target)
                .unwrap_or_else(|| Box::new(EntityView::new(mob_kind.name(), pos)));
            Event::Killed(
                TriggerKind::PLAYER_KILLED_ENTITY,
                victim,
                Box::new(view::damage_view(ents, types, &source)),
            )
        });
    }
    // `Creeper.killedEntity`: a charged creeper's kill drops `charged_creeper/root` (the
    // mob heads) once.
    if let Some(cause) = source.cause
        && env.rules.bool(Rule::MOB_DROPS)
        && ents.mob(cause).is_some_and(|c| {
            c.kind == Kind::Creeper && {
                let s = super::fam::classic::creeper(c);
                s.powered && !s.dropped_skulls
            }
        })
    {
        let heads = named_table_loot(
            level,
            ents,
            env,
            target,
            &source,
            "minecraft:charged_creeper/root",
            0,
        )
        .unwrap_or_default();
        for stack in heads.into_iter().filter(|s| !s.is_empty()) {
            if let Some(c) = ents.mob_mut(cause) {
                super::fam::classic::creeper_mut(c).dropped_skulls = true;
            }
            let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
            env.out.push(Effect::DropItem {
                random: item_random,
                uuid,
                pos,
                velocity,
                y_rot,
                pickup_delay: 10,
                stack,
            });
        }
    }
    // `gameEvent(ENTITY_DIE)`: a sculk catalyst nearby takes the experience
    // (`CatalystListener`, `skipDropExperience`); vibration listeners hear the death.
    let consumed = crate::block::sculk_catalyst::catalyst_near(level, pos);
    let reward = if consumed { reward_of(ents) } else { 0 };
    if consumed && reward > 0 {
        // `CatalystListener.tryAwardItSpreadsAdvancement`: the player that hurt it last, with
        // the damage source before the killing blow (else a player attack).
        let last = ents.mob(target).and_then(|m| {
            m.last_hurt_by_mob
                .filter(|&p| ents.player(p).is_some())
                .map(|p| (p, m.last_damage_source(env.game_time)))
        });
        if let Some((player, last_kind)) = last {
            let types = env.types;
            let kind = TriggerKind::KILL_MOB_NEAR_SCULK_CATALYST;
            view::fire(ents, player, kind, |ents| {
                let blow = match last_kind {
                    Some(k) => DamageSource::of(k),
                    None => entity_source(ents, types.player_attack, player, player),
                };
                let victim = view::snapshot(ents, target)
                    .unwrap_or_else(|| Box::new(EntityView::new(mob_kind.name(), pos)));
                Event::Killed(
                    kind,
                    victim,
                    Box::new(view::damage_view(ents, types, &blow)),
                )
            });
        }
    }
    if consumed {
        cx.actions.push(super::WorldAction::Level(
            super::boss::warden::LevelAction::SculkDeath {
                pos,
                experience: reward,
                player: None,
            },
        ));
    }
    cx.actions.push(super::WorldAction::Level(
        super::boss::warden::LevelAction::GameEvent {
            event: crate::level::game_event::events().entity_die,
            pos,
            context: crate::level::game_event::GameEventContext::of(Some(target)),
        },
    ));
    // `dropAllDeathLoot`: the loot table, then the experience.
    let mut drops = Vec::new();
    if !loot_baby && env.rules.bool(Rule::MOB_DROPS) {
        let mut loot = table_loot(level, ents, env, target, &source)
            .or_else(|| {
                ents.mob_mut(target)
                    .map(|m| kinds::death_loot(m, killed_by_player))
            })
            .unwrap_or_default();
        // `looting.json` `equipment_drops`: +0.01 per level for a killing player.
        let looting = source
            .cause
            .and_then(|c| ents.player(c))
            .and_then(|p| {
                let d = crate::player::play::game_data()?;
                let hand = p.inventory.equipment(crate::item::EquipmentSlot::MainHand);
                Some(d.enchantment_level(hand, "minecraft:looting"))
            })
            .filter(|&l| l > 0)
            .map_or(0.0, |l| {
                #[allow(clippy::cast_precision_loss)]
                let above = (l - 1) as f32;
                0.01_f32 + 0.01 * above
            });
        // `dropCustomDeathLoot` / `dropEquipment`: the equipment, the saddle and the
        // enderman's block.
        // `LivingEntity.dropEquipment` drops nothing; `Mob` and the players drop theirs.
        if let Some(m) = ents
            .mob_mut(target)
            .filter(|m| m.kind.family() != super::Family::Decor)
        {
            super::equipment::death_drops(m, killed_by_player, looting, &mut loot);
            if m.saddled
                && let Some(saddle) = vanillite_data::registry::ITEM.id("minecraft:saddle")
            {
                m.saddled = false;
                loot.push(crate::item::ItemStack::new(saddle, 1));
            }
            if let Some(block) =
                super::fam::classic::try_enderman_mut(m).and_then(|e| e.carried.take())
                && let Some(item) = vanillite_data::registry::ITEM.id(block.block().info().name)
            {
                loot.push(crate::item::ItemStack::new(item, 1));
            }
        }
        drops = loot;
    }
    // `dropExperience`: the reward is drawn after the loot (item entities use their own random).
    let mut experience =
        if !consumed && killed_by_player && !baby && env.rules.bool(Rule::MOB_DROPS) {
            reward_of(ents)
        } else {
            0
        };
    if let Some(hooks) = ents.hooks_for(vanillite_abi::event::ENTITY_DROPS)
        && let Some(e) = ents.store.get(target)
    {
        let mut ev = crate::hooks::Event {
            entity: e.uuid(),
            entity_type: e.kind().name().to_owned(),
            other: source
                .cause
                .and_then(|c| ents.store.get(c))
                .map_or(0, |c| c.uuid()),
            key: env.types.get(source.kind).id.clone(),
            item: crate::hooks::items_snbt(&drops),
            value: i64::from(experience),
            to: crate::hooks::Place::at(pos),
            ..crate::hooks::Event::new(vanillite_abi::event::ENTITY_DROPS)
        };
        hooks.event(crate::hooks::HookWorld::split(level, ents), &mut ev);
        if let Some(new) = crate::hooks::drops_out(&ev) {
            drops = new;
        }
        experience = crate::hooks::experience_value(ev.value);
    }
    for stack in drops {
        // `spawnAtLocation`: an item entity with the default random motion.
        let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
        env.out.push(Effect::DropItem {
            random: item_random,
            uuid,
            pos,
            velocity,
            y_rot,
            pickup_delay: 10,
            stack,
        });
    }
    if experience > 0 {
        award_experience(env, pos, experience);
    }
    create_wither_rose(level, ents, env, cx, pos, credit);
    env.out.push(Effect::Died {
        victim: target,
        killer: credit.or(source.cause),
        killer_kind: None,
        message: None,
    });
    env.out.push(Effect::Tracking {
        entity: target,
        with_self: false,
        packet: SharedPacket::of(&EntityEvent {
            entity_id: network_id,
            event: event::DEATH,
        }),
    });
    if let Some(mut e) = ents.store.get_mut(target) {
        e.set_pose(Pose::Dying);
    }
}

/// `LivingEntity.createWitherRose`: a victim of the wither leaves a wither rose, planted
/// where it died if `mob_griefing` allows and the rose survives there, else as an item.
pub fn create_wither_rose(
    level: &Level,
    ents: &Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    pos: Vec3,
    killer: Option<EntityId>,
) {
    if !killer.is_some_and(|k| ents.mob(k).is_some_and(|m| m.kind == Kind::Wither)) {
        return;
    }
    use vanillite_data::block::ids;
    let block_pos = vanillite_base::BlockPos::new(
        vanillite_base::math::floor(pos.x),
        vanillite_base::math::floor(pos.y),
        vanillite_base::math::floor(pos.z),
    );
    let rose = ids::WITHER_ROSE.default_state();
    if env.rules.bool(Rule::MOB_GRIEFING)
        && level.block(block_pos).is_air()
        && crate::block::place::can_survive(rose, block_pos, level)
    {
        cx.actions.push(super::WorldAction::SetBlock(
            block_pos,
            rose,
            crate::level::update::ALL,
        ));
        return;
    }
    let Some(item) = vanillite_data::registry::ITEM.id("minecraft:wither_rose") else {
        return;
    };
    let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
    env.out.push(Effect::DropItem {
        random: item_random,
        uuid,
        pos,
        velocity,
        y_rot,
        pickup_delay: 0,
        stack: crate::item::ItemStack::new(item, 1),
    });
}

/// `getDisplayName` of a mob without team: its custom name, else its type's name.
pub(crate) fn mob_name(m: &super::MobData) -> Component {
    if let Some(name) = m.custom_name.as_deref().and_then(nbt_name) {
        return name;
    }
    Component::translatable(m.kind.translation_key(), vec![])
}

/// A custom name as network NBT.
fn nbt_name(bytes: &[u8]) -> Option<Component> {
    let mut input = bytes;
    let mut budget = vanillite_nbt::Budget::file();
    match vanillite_nbt::read::read_network(&mut input, &mut budget) {
        Ok(Some(tag)) => Some(Component::from_nbt(&tag)),
        _ => None,
    }
}

/// `Entity.getDisplayName` (entities that are no players without their team) with the
/// custom-named item in a living entity's main hand, for death messages.
pub fn attacker_name(ents: &Entities, env: &Env<'_>, id: EntityId) -> Option<AttackerName> {
    let e = ents.store.get(id)?;
    if let Some(p) = ents.player(id) {
        return Some(AttackerName {
            name: (env.display_name)(&p.profile.name, e.uuid()),
            item: custom_named_item(p.inventory.selected_item()),
        });
    }
    let type_name = e.kind().name();
    let custom = ents
        .mob(id)
        .and_then(|m| m.custom_name.as_deref())
        .or_else(|| crate::decoration::custom_name(ents, id))
        .and_then(nbt_name);
    // `getName`: the custom name, else the type's name.
    let name = custom.unwrap_or_else(|| {
        Component::translatable(format!("entity.{}", type_name.replace(':', ".")), vec![])
    });
    let display = name
        .clone()
        .hover(HoverEvent::ShowEntity {
            id: type_name.to_owned(),
            uuid: e.uuid(),
            name: Some(name),
        })
        .insertion(vanillite_protocol::types::Uuid(e.uuid()).to_string());
    let item = ents
        .mob(id)
        .and_then(|m| custom_named_item(m.equipment.get(crate::item::EquipmentSlot::MainHand)));
    Some(AttackerName {
        name: display,
        item,
    })
}

/// `ItemStack.getDisplayName` of a stack with a custom name: the italic name in brackets in
/// the rarity colour, showing the item on hover.
pub(crate) fn custom_named_item(stack: &crate::item::ItemStack) -> Option<Component> {
    use vanillite_data::item::Rarity;
    use vanillite_protocol::codec::DecodeLimited as _;
    use vanillite_protocol::text::{Color, NamedColor};
    if stack.is_empty() {
        return None;
    }
    let bytes = stack.component(vanillite_protocol::components::ids::CUSTOM_NAME)?;
    let name = Component::decode_limited(
        &mut vanillite_protocol::codec::Reader::new(bytes),
        bytes.len(),
    )
    .ok()?;
    let enchanted = !crate::item::enchantments(stack).is_empty();
    let color = match (crate::item::item_id(stack).info().rarity, enchanted) {
        (Rarity::Common, false) => NamedColor::White,
        (Rarity::Uncommon, false) => NamedColor::Yellow,
        (Rarity::Common | Rarity::Uncommon | Rarity::Rare, true) | (Rarity::Rare, false) => {
            NamedColor::Aqua
        }
        (Rarity::Epic, _) => NamedColor::LightPurple,
    };
    let name = Component::empty().append(name).italic(true);
    Some(
        Component::translatable("chat.square_brackets", vec![name])
            .color(Color::Named(color))
            .hover(HoverEvent::ShowItem {
                id: vanillite_data::registry::ITEM
                    .name(stack.item)
                    .unwrap_or("minecraft:air")
                    .to_owned(),
                count: stack.count,
                components: None,
            }),
    )
}

/// The names of a damage source's entities for the death message of a player.
pub fn source_names(ents: &Entities, env: &Env<'_>, source: &DamageSource) -> SourceNames {
    let name = |id: Option<EntityId>| id.and_then(|id| attacker_name(ents, env, id));
    SourceNames {
        cause: name(source.cause),
        direct: name(source.direct).map(|a| a.name),
        responsible: name(source.responsible_player).map(|a| a.name),
    }
}

/// `ExperienceOrb.award`: the world splits `amount` into orbs from the [`Effect::Experience`].
pub fn award_experience(env: &mut Env<'_>, pos: Vec3, amount: i32) {
    env.out.push(Effect::Experience { pos, amount });
}

/// Plays a mob's sound at its position with its volume and voice pitch (`makeSound`).
pub fn mob_sound(ents: &mut Entities, env: &mut Env<'_>, id: EntityId, sound: &str) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = ents.mob_mut(id) else {
        return;
    };
    let volume = super::fam::mob_sound_volume(m);
    // `getVoicePitch`: the happy ghast's is constant (no draws), the bat's a little lower.
    let pitch = if m.kind == Kind::HappyGhast {
        1.0
    } else {
        let a = m.random.next_float();
        let b = m.random.next_float();
        let pitch = if m.baby {
            (a - b) * 0.2 + 1.5
        } else {
            (a - b) * 0.2 + 1.0
        };
        if m.kind == Kind::Bat {
            pitch * 0.95
        } else {
            pitch
        }
    };
    // `playSound` checks `isSilent` after the pitch is drawn.
    if m.silent {
        return;
    }
    let source = m.sound_source();
    let seed = crate::survival::sound_seed();
    play_sound(env.out, None, pos, sound, source, volume, pitch, seed);
}

/// `Mob.doHurtTarget`: a melee hit with the mob's attack damage.
pub fn mob_attack(t: &mut super::Tick<'_, '_>, target: EntityId) -> bool {
    if t.mob_ref().kind.family() != super::Family::Classic
        && let Some(r) = super::fam::do_hurt_target(t, target)
    {
        return r;
    }
    base_mob_attack(t, target)
}

/// `Mob.doHurtTarget` itself (families call it from their override).
/// `getAttributeValue(ATTACK_DAMAGE)`: the mob's attack damage with the attribute modifiers of
/// its equipment (a vex's iron sword, a vindicator's axe).
pub fn attack_damage(m: &super::MobData) -> f32 {
    let mut mods = Vec::new();
    for slot in (0..8).filter_map(crate::item::EquipmentSlot::from_ordinal) {
        crate::item::attribute_modifiers(
            m.equipment.get(slot),
            slot,
            "minecraft:attack_damage",
            &mut mods,
        );
    }
    let value = if mods.is_empty() {
        m.attrs.attack_damage
    } else {
        crate::item::attribute_value(m.attrs.attack_damage, &mods, 0.0, 2048.0)
    };
    #[allow(clippy::cast_possible_truncation)]
    let value = value as f32;
    value
}

pub fn base_mob_attack(t: &mut super::Tick<'_, '_>, target: EntityId) -> bool {
    use crate::predicate::EntityTags;
    let weapon = t
        .mob_ref()
        .equipment
        .get(crate::item::EquipmentSlot::MainHand)
        .clone();
    // `weapon.getDamageSource(this)`: the weapon's `damage_type`, else `mob_attack`.
    let kind = crate::item::damage_type(&weapon)
        .and_then(|n| t.env.types.kind(n))
        .unwrap_or(t.env.types.mob_attack);
    let source = entity_source(t.ents, kind, t.id, t.id);
    let combat = super::attack::combat_data();
    let tags: &dyn EntityTags = &*combat;
    // `EnchantmentHelper.modifyDamage` of the weapon.
    let damage = attack_damage(t.mob_ref());
    let damage = if weapon.is_empty() {
        damage
    } else {
        let ecx = super::attack::damage_context(t.ents, t.env.types, tags, target, &source);
        combat
            .enchants
            .modify_damage(&weapon, &ecx, t.env.random, damage)
    };
    let hurt_ok = hurt(t.level, t.ents, t.env, t.cx, target, source, damage);
    if hurt_ok {
        // `getKnockback`: the attribute through the weapon's enchantments, halved.
        #[allow(clippy::cast_possible_truncation)]
        let base = t.mob_ref().attrs.attack_knockback as f32;
        let kb = if weapon.is_empty() {
            base
        } else {
            let ecx = super::attack::damage_context(t.ents, t.env.types, tags, target, &source);
            combat
                .enchants
                .modify_knockback(&weapon, &ecx, t.env.random, base)
        } / 2.0;
        let living = t.ents.mob(target).is_some() || t.ents.player(target).is_some();
        if kb > 0.0 && living {
            let rad = f64::from(t.core().rot().y_rot * super::DEG_TO_RAD);
            let (sin, cos) = (
                f64::from(vanillite_base::math::sin(rad)),
                f64::from(vanillite_base::math::cos(rad)),
            );
            let by = Some((t.id, damage, true));
            knock_living(t.ents, t.env, target, f64::from(kb), sin, -cos, by);
            let v = t.velocity();
            t.set_velocity(v.multiply(0.6, 1.0, 0.6));
        }
        // `weaponItem.hurtEnemy` wears nothing: only `Player.attack` runs `postHurtEnemy`.
        // `EnchantmentHelper.doPostAttackEffects`: the target's equipment (Thorns), then the
        // weapon (Fire Aspect).
        let weapon = (!weapon.is_empty()).then_some(&weapon);
        super::attack::post_attack(t.level, t.ents, t.env, t.cx, target, &source, weapon);
        t.mob().last_hurt_mob = Some(target);
        kinds::after_attack(t, target);
    }
    hurt_ok
}

/// Knocks a player or a mob back (`LivingEntity.knockback`).
pub fn knock_living(
    ents: &mut Entities,
    env: &mut Env<'_>,
    target: EntityId,
    power: f64,
    xd: f64,
    zd: f64,
    by: Option<(EntityId, f32, bool)>,
) {
    if ents.mob(target).is_some() {
        knock_mob(ents, env, target, power, xd, zd, by);
        return;
    }
    // `KNOCKBACK_RESISTANCE` of the player's equipment (netherite armour).
    let resistance = ents.player(target).map_or(0.0, |p| {
        crate::player::play::equipment_attribute(
            &p.inventory,
            "minecraft:knockback_resistance",
            0.0,
        )
    });
    if let Some(mut e) = ents.store.get_mut(target) {
        let on_ground = e.as_ref().flags().contains(EntityFlags::ON_GROUND);
        if let Some(v) = knockback(
            e.as_ref().velocity(),
            on_ground,
            power,
            xd,
            zd,
            resistance,
            env.random,
        ) {
            e.set_velocity(v);
            e.set_flags(EntityFlags::HURT_MARKED, true);
        }
    }
}

/// The drops of the mob's `entities/<type>` loot table from the data packs, `None` without
/// that table.
fn table_loot(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    target: EntityId,
    source: &DamageSource,
) -> Option<Vec<crate::item::ItemStack>> {
    let name = ents.store.get(target)?.kind().info().name;
    let (ns, path) = name.split_once(':').unwrap_or(("minecraft", name));
    // `Mob.getLootTable`: `DeathLootTable` replaces the type's table.
    let (replaced, seed) = ents.extra(target).map_or((None, 0), |x| {
        (x.death_loot_table.clone(), x.death_loot_seed)
    });
    let table = replaced
        .clone()
        .unwrap_or_else(|| format!("{ns}:entities/{path}"));
    let drops = named_table_loot(level, ents, env, target, source, &table, seed);
    if replaced.is_none() {
        return drops;
    }
    // `Mob.dropFromLootTable` forgets its `DeathLootTable`.
    if let Some(x) = ents.extra_mut(target) {
        x.death_loot_table = None;
    }
    // An unknown table is the empty one: nothing drops.
    Some(drops.unwrap_or_default())
}

/// `dropFromLootTable` of the loot table `table` for the dying `target`; a `seed` of 0 means
/// none.
fn named_table_loot(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    target: EntityId,
    source: &DamageSource,
    table: &str,
    seed: i64,
) -> Option<Vec<crate::item::ItemStack>> {
    let data = crate::player::play::game_data()?;
    if !data.loot.contains(table) {
        return None;
    }
    let this = super::attack::facts(ents, target)?;
    let attacker = source.cause.and_then(|c| super::attack::facts(ents, c));
    let direct = source.direct.and_then(|c| super::attack::facts(ents, c));
    let m = ents.mob(target)?;
    let last_player = m
        .last_hurt_by_player
        .filter(|_| m.last_hurt_by_player_time > 0)
        .and_then(|p| super::attack::facts(ents, p));
    let weapon = source
        .cause
        .and_then(|c| ents.player(c))
        .map(|p| p.inventory.selected_item().clone())
        .unwrap_or_default();
    let ench_level = |name: &str| data.enchantment_level(&weapon, name);
    let entity = crate::loot::EntityLootContext {
        this,
        attacker,
        direct_attacker: direct,
        last_damage_player: last_player,
        source: Some(crate::predicate::SourceFacts {
            kind: source.kind,
            is_direct: source.cause.is_some() && source.cause == source.direct,
            direct,
            cause: attacker,
        }),
        types: env.types,
        tags: &*data,
        attacker_enchantment_level: &ench_level,
    };
    let none = |_: &str| 0;
    let in_tag = |item: u16, tag: &str| data.item_in_tag(item, tag);
    let empty = crate::item::ItemStack::default();
    let ctx = crate::loot::BlockLootContext {
        state: vanillite_data::block::BlockState::AIR,
        tool: &empty,
        enchantment_level: &none,
        item_in_tag: &in_tag,
        explosion_radius: None,
        this_present: true,
        entity: Some(&entity),
        block_entity: None,
    };
    let mut out = Vec::new();
    if seed == 0 {
        // `dropFromLootTable` without a loot table seed: the table's random sequence, else
        // the level random.
        data.loot
            .roll_table(level, table, &ctx, env.random, &mut out);
    } else {
        let mut random = vanillite_base::random::LegacyRandom::new(seed);
        data.loot.table_drops(table, &ctx, &mut random, &mut out);
    }
    Some(out)
}

/// `EnderMan.hurtServer`'s thrown potion (`AbstractThrownPotion` as the direct entity):
/// `Some(hurts)`, true for a potion in `#hurts_water_sensitive_entities` (water); `None` for
/// any other damage.
fn enderman_potion_hurts(ents: &Entities, direct: Option<EntityId>) -> Option<bool> {
    let id = direct?;
    let kind = ents.store.get(id)?.kind();
    if !crate::effect::thrown::is_thrown_potion(kind) {
        return None;
    }
    let water = crate::effect::potion::water_id();
    let hurts = match ents.special(id) {
        Some(crate::mob::boss::special::SpecialData::ThrownPotion(p)) => {
            crate::effect::potion::PotionContents::of_stack(&p.stack).potion == water
        }
        _ => false,
    };
    Some(hurts)
}
