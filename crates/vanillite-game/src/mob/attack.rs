//! `Player.attack`: melee attacks of players on mobs and players with the held item's attack
//! damage and speed, the attack cooldown, critical hits, sweeping, knockback and the combat
//! effects of enchantments (damage, knockback, post attack effects like Fire Aspect), and the
//! weapon's durability.

use vanillite_base::Vec3;
use vanillite_base::random::RandomSource;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{Animate, SetEntityMotion, SoundSource};
use vanillite_protocol::types::LpVec3;
use vanillite_world::entity::{EntityFlags, EntityId};

use super::MobContext;
use super::combat::{self, entity_source};
use crate::enchant::{EnchantContext, Enchantments};
use crate::enchant::{EntityEffect, Party};
use crate::entities::Entities;
use crate::item::{self, EquipmentSlot, ItemStack};
use crate::level::Level;
use crate::predicate::{EntityFacts, EntityTags, SourceFacts};
use crate::survival::damage::DamageSource;
use crate::survival::player::{self, Env};
use crate::survival::{Effect, play_sound};

/// Entity type tags when no data packs are loaded.
/// The loaded game data with the enchantments and entity tags (empty without data packs).
pub fn combat_data() -> std::sync::Arc<crate::player::play::GameData> {
    static EMPTY: std::sync::OnceLock<std::sync::Arc<crate::player::play::GameData>> =
        std::sync::OnceLock::new();
    crate::player::play::game_data()
        .unwrap_or_else(|| std::sync::Arc::clone(EMPTY.get_or_init(Default::default)))
}

/// The wire id of a game mode (`GameType.getId`).
fn game_mode_id(mode: vanillite_config::GameMode) -> i32 {
    use vanillite_config::GameMode;
    match mode {
        GameMode::Survival => 0,
        GameMode::Creative => 1,
        GameMode::Adventure => 2,
        GameMode::Spectator => 3,
    }
}

/// The facts predicates test of a living entity.
pub fn facts(ents: &Entities, id: EntityId) -> Option<EntityFacts> {
    let e = ents.store.get(id)?;
    let flags = e.flags();
    let sheep = ents
        .mob(id)
        .filter(|m| m.kind == super::Kind::Sheep)
        .map(|m| (crate::mob::fam::classic::sheep(m).color & 15, m.sheared));
    let (on_fire, baby) = if let Some(m) = ents.mob(id) {
        (m.is_on_fire(), m.baby)
    } else if let Some(p) = ents.player(id) {
        (p.survival.fire_ticks > 0, false)
    } else {
        (flags.contains(EntityFlags::ON_FIRE), false)
    };
    Some(EntityFacts {
        kind: Some(e.kind()),
        on_fire,
        baby,
        sneaking: flags.contains(EntityFlags::CROUCHING),
        sprinting: flags.contains(EntityFlags::SPRINTING),
        swimming: flags.contains(EntityFlags::SWIMMING),
        on_ground: flags.contains(EntityFlags::ON_GROUND),
        smelts_loot: ents.player(id).is_some_and(|p| {
            crate::player::play::game_data().is_some_and(|d| {
                d.enchantment_level(p.inventory.selected_item(), "minecraft:fire_aspect") > 0
            })
        }),
        sheep_color: sheep.map(|s| s.0),
        sheared: sheep.is_some_and(|s| s.1),
        villager_variant: None,
        farm_variant: ents
            .mob(id)
            .filter(|m| {
                matches!(
                    m.kind,
                    super::Kind::Chicken | super::Kind::Cow | super::Kind::Pig
                )
            })
            .and_then(|m| u8::try_from(m.variant).ok()),
        // Creative flight is the client's: only gliding counts.
        flying: flags.contains(EntityFlags::FALL_FLYING),
        fall_distance: ents.player(id).map_or_else(
            || ents.mob(id).map_or(0.0, |m| m.fall_distance),
            |p| p.survival.fall_distance,
        ),
        can_see_sky: false,
        fishing_hook: None,
        vehicle: ents
            .vehicle_of(id)
            .and_then(|v| ents.store.get(v))
            .map(|v| v.kind()),
        cube_size: ents
            .mob(id)
            .filter(|m| {
                matches!(
                    m.kind,
                    super::Kind::Slime | super::Kind::MagmaCube | super::Kind::SulfurCube
                )
            })
            .map(crate::mob::fam::undead::cubes::size),
        raider: ents.mob(id).filter(|m| m.kind.is_raider()).map(|m| {
            use crate::mob::fam::illager::{raider, st};
            let captain = st(m).patrol_leader
                && raider::is_ominous_banner(m.equipment.get(crate::item::EquipmentSlot::Head));
            // `Raider.hasRaid`: its raid or a raid at its block position.
            let at = ents.store.get(id).map(|e| {
                let p = e.pos();
                vanillite_base::BlockPos::containing(p.x, p.y, p.z)
            });
            let has_raid = crate::raid::current_raid(ents, m).is_some()
                || at.is_some_and(|b| ents.raids.raid_at(b).is_some());
            (has_raid, captain)
        }),
        mooshroom_variant: ents
            .mob(id)
            .filter(|m| m.kind == super::Kind::Mooshroom)
            .and_then(|m| u8::try_from(crate::mob::fam::wildlife::st(m).variant).ok()),
        fall_flying: flags.contains(EntityFlags::FALL_FLYING),
        in_water: ents.player(id).map_or_else(
            || ents.mob(id).is_some_and(|m| m.fluids.in_water()),
            |p| p.survival.fluids.in_water(),
        ),
        player: ents
            .player(id)
            .map(|p| (game_mode_id(p.game_mode), p.survival.food.food_level)),
    })
}

/// `Enchantment.damageContext(victim, source)`.
pub fn damage_context<'a>(
    ents: &Entities,
    types: &'a crate::survival::damage::DamageTypes,
    tags: &'a dyn EntityTags,
    victim: EntityId,
    source: &DamageSource,
) -> EnchantContext<'a> {
    let attacker = source.cause.and_then(|c| facts(ents, c));
    let direct = source.direct.and_then(|c| facts(ents, c));
    let attacking_player = source
        .cause
        .filter(|&c| ents.player(c).is_some())
        .and_then(|c| facts(ents, c));
    EnchantContext {
        types,
        tags,
        this: facts(ents, victim),
        attacker,
        direct_attacker: direct,
        attacking_player,
        source: Some(SourceFacts {
            kind: source.kind,
            is_direct: source.cause.is_some() && source.cause == source.direct,
            direct,
            cause: attacker,
        }),
        raining: false,
        thundering: false,
        location: None,
    }
}

/// A player's attribute value from the base, the modifiers of its equipment and those of
/// its status effects (Strength, Weakness, Haste, Mining Fatigue, Luck, ...).
pub fn player_attribute(
    ents: &Entities,
    player: EntityId,
    name: &str,
    base: f64,
    min: f64,
    max: f64,
) -> f64 {
    let Some(p) = ents.player(player) else {
        return base;
    };
    let mut mods = Vec::new();
    for slot in crate::player::encode::EQUIPMENT_SLOTS {
        item::attribute_modifiers(p.inventory.equipment(slot), slot, name, &mut mods);
    }
    let effects = &p.survival.effects.modifiers;
    let mut base = base;
    if !effects.is_empty()
        && let Some(id) = item::attribute_id(name)
        && let Ok(id) = u8::try_from(id)
    {
        // A base value set by a command or plugin.
        let attribute = crate::effect::attribute::Attribute(id);
        base = effects.base(attribute).unwrap_or(base);
        for m in effects.of(attribute) {
            #[allow(clippy::as_conversions)]
            mods.push((m.operation as u8, m.amount));
        }
    }
    item::attribute_value(base, &mods, min, max)
}

/// `getCurrentItemAttackStrengthDelay`: ticks until a full-strength attack.
pub fn attack_strength_delay(ents: &Entities, player: EntityId) -> f32 {
    let speed = player_attribute(ents, player, "minecraft:attack_speed", 4.0, 0.0, 1024.0);
    #[allow(clippy::cast_possible_truncation)]
    let delay = (1.0 / speed * 20.0) as f32;
    delay
}

/// `getAttackStrengthScale(a)`.
pub fn attack_strength_scale(ents: &Entities, player: EntityId, a: f32) -> f32 {
    let Some(p) = ents.player(player) else {
        return 1.0;
    };
    #[allow(clippy::cast_precision_loss)]
    let ticker = p.survival.attack_ticker as f32;
    ((ticker + a) / attack_strength_delay(ents, player)).clamp(0.0, 1.0)
}

/// Plays a player sound at the player's position, heard by everyone
/// (`playServerSideSound`).
fn player_sound(env: &mut Env<'_>, pos: Vec3, sound: &str) {
    let seed = crate::survival::sound_seed();
    play_sound(
        env.out,
        None,
        pos,
        sound,
        SoundSource::Players,
        1.0,
        1.0,
        seed,
    );
}

/// Whether `id` is a living entity that is alive.
fn is_alive(ents: &Entities, id: EntityId) -> bool {
    ents.mob(id).is_some_and(super::MobData::is_alive)
        || ents
            .player(id)
            .is_some_and(|p| !p.survival.is_dead_or_dying())
}

/// Whether the attacked entity is a `LivingEntity` (a mob, armor stand or player). The
/// dragon's body is hit through its parts, which are not.
pub(crate) fn is_living_target(ents: &Entities, id: EntityId) -> bool {
    ents.player(id).is_some()
        || ents
            .mob(id)
            .is_some_and(|m| m.kind != super::Kind::EnderDragon)
}

/// `isAttackable`: not items, experience orbs, arrows and tridents, falling blocks, eyes of
/// ender or firework rockets.
pub(crate) fn is_attackable(ents: &Entities, id: EntityId) -> bool {
    use vanillite_data::entity::ids;
    ents.store.get(id).is_some_and(|e| {
        ![
            ids::ITEM,
            ids::EXPERIENCE_ORB,
            ids::ARROW,
            ids::SPECTRAL_ARROW,
            ids::TRIDENT,
            ids::FALLING_BLOCK,
            ids::EYE_OF_ENDER,
            ids::FIREWORK_ROCKET,
        ]
        .contains(&e.kind())
    })
}

/// `Entity.getTeam`: the scoreboard team of a player (its name holds the score) or of another
/// entity (its UUID).
pub fn team_of(
    ents: &Entities,
    env: &Env<'_>,
    id: EntityId,
) -> Option<crate::scoreboard::TeamFacts> {
    let holder = match ents.player(id) {
        Some(p) => p.profile.name.clone(),
        None => vanillite_protocol::types::Uuid(ents.store.get(id)?.uuid()).to_string(),
    };
    (env.entity_team)(&holder)
}

/// `Entity.considersEntityAsAlly(other)`: the same team; a tamed wolf, cat, parrot or
/// nautilus stands by its owner and the owner's allies (`TamableAnimal`), an allay by the
/// player it likes.
fn considers_ally(ents: &Entities, env: &Env<'_>, id: EntityId, other: EntityId) -> bool {
    use super::Kind;
    if let Some(m) = ents.mob(id) {
        let tamable = matches!(
            m.kind,
            Kind::Wolf | Kind::Cat | Kind::Parrot | Kind::Nautilus | Kind::ZombieNautilus
        );
        if tamable
            && m.tame
            && let Some(owner) = super::tame::owner_of(ents, m)
        {
            return other == owner || considers_ally(ents, env, owner, other);
        }
        if m.kind == Kind::Allay
            && ents.player(other).is_some()
            && super::fam::mount::ai::liked_player(m)
                .is_some_and(|u| ents.store.get(other).is_some_and(|e| e.uuid() == u))
        {
            return true;
        }
    }
    team_of(ents, env, id)
        .is_some_and(|t| team_of(ents, env, other).is_some_and(|o| o.name == t.name))
}

/// `Entity.isAlliedTo(other)`.
pub fn is_allied(ents: &Entities, env: &Env<'_>, a: EntityId, b: EntityId) -> bool {
    a == b || considers_ally(ents, env, a, b) || considers_ally(ents, env, b, a)
}

/// `ServerPlayer.canHarmPlayer(target)`: the pvp rule, then `doTeamsAllowDamage` (members of
/// one team hurt each other only with friendly fire).
pub fn can_harm_player(
    ents: &Entities,
    env: &Env<'_>,
    attacker: EntityId,
    target: EntityId,
) -> bool {
    if !env.rules.bool(crate::survival::rules::Rule::PVP) {
        return false;
    }
    let Some(team) = team_of(ents, env, attacker) else {
        return true;
    };
    team.friendly_fire || team_of(ents, env, target).is_none_or(|t| t.name != team.name)
}

/// `Projectile.deflect(ProjectileDeflection.AIM_DEFLECT, by, owner, true, 1.0)` of a
/// `#redirectable_projectile` (ghast fireballs, wind charges): it flies on along the look of
/// `by` (keeps its motion without one), belongs to `owner` and accelerates again
/// (`onDeflection(true)`); a wind charge refuses while its `noDeflectTicks` run. `false` for
/// other entities.
pub(crate) fn aim_deflect(
    ents: &mut Entities,
    target: EntityId,
    by: Option<EntityId>,
    owner: Option<EntityId>,
) -> bool {
    use super::missile::MissileKind;
    let Some(d) = ents.missile(target) else {
        return false;
    };
    let redirectable = matches!(
        d.kind,
        MissileKind::Fireball | MissileKind::WindCharge | MissileKind::BreezeWindCharge
    );
    if !redirectable || d.removed || super::fam::mount::wind::refuses_deflection(d) {
        return false;
    }
    let look = by.and_then(|b| ents.store.get(b)).map(|e| {
        let r = e.rot();
        super::view_vector(r.x_rot, r.y_rot)
    });
    let owner_uuid = owner.and_then(|o| ents.store.get(o)).map(|e| e.uuid());
    if let Some(d) = ents.missile_mut(target) {
        d.owner = owner;
        d.owner_uuid = owner_uuid;
        d.acceleration = 0.1;
    }
    if let Some(look) = look
        && let Some(mut e) = ents.store.get_mut(target)
    {
        e.set_velocity(look);
        // `needsSync`.
        e.set_flags(EntityFlags::HAS_IMPULSE, true);
    }
    true
}

/// `Player.deflectProjectile`: a player's hit sends a `#redirectable_projectile` off along the
/// player's look as its new owner ([`aim_deflect`]), with the `attack.nodamage` sound.
pub(crate) fn deflect_projectile(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
) -> bool {
    if !aim_deflect(ents, target, Some(player), Some(player)) {
        return false;
    }
    if let Some(pos) = ents.store.get(player).map(|e| e.pos()) {
        player_sound(env, pos, "minecraft:entity.player.attack.nodamage");
    }
    true
}

/// `Player.attack(entity)`.
pub fn player_attack(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    attacker: EntityId,
    target: EntityId,
) {
    // `cannotAttack`.
    if attacker == target || !is_attackable(ents, target) {
        return;
    }
    let Some(e) = ents.store.get(attacker) else {
        return;
    };
    let ppos = e.pos();
    let flags = e.flags();
    let y_rot = e.rot().y_rot;
    let Some(p) = ents.player(attacker) else {
        return;
    };
    // `getWeaponItem`: the riptide trident while spinning.
    let spin = p
        .survival
        .spin_attack
        .as_ref()
        .map(|s| (s.damage, s.item.clone()));
    let weapon = spin
        .as_ref()
        .map_or_else(|| p.inventory.selected_item().clone(), |s| s.1.clone());
    let fall_distance = p.survival.fall_distance;
    let in_water = p.survival.fluids.in_water();
    // `onClimbable` and `isMobilityRestricted` (blindness) of `canCriticalAttack`.
    let climbing = p.game_mode != vanillite_config::GameMode::Spectator
        && env
            .blocks
            .block_state(player::block_pos(ppos))
            .is_some_and(|s| env.facts.is_climbable(s.block()));
    let blind = p.survival.effects.has(crate::effect::MobEffect::BLINDNESS);
    let combat = combat_data();
    let (enchants, tags): (&Enchantments, &dyn EntityTags) = (&combat.enchants, &*combat);

    let base = player_attribute(ents, attacker, "minecraft:attack_damage", 1.0, 0.0, 2048.0);
    #[allow(clippy::cast_possible_truncation)]
    let mut base_damage = spin.as_ref().map_or(base as f32, |s| s.0);
    // `ItemStack.getDamageSource`: the stack's `damage_type`, else `getItemDamageSource` (a
    // smash attack is `mace` damage), else `player_attack`.
    let smash = can_smash_attack(ents, attacker, &weapon);
    let kind = if let Some(kind) = item::damage_type(&weapon).and_then(|t| env.types.kind(t)) {
        kind
    } else if smash {
        env.types.named("minecraft:mace")
    } else {
        env.types.player_attack
    };
    let mut source = entity_source(ents, kind, attacker, attacker);
    source.cause_is_mob = false;
    source.disable_shield_seconds =
        item::weapon(&weapon).map_or(0.0, |w| w.disable_blocking_for_seconds);
    let scale = attack_strength_scale(ents, attacker, 0.5);
    let enchanted = {
        let ecx = damage_context(ents, env.types, tags, target, &source);
        enchants.modify_damage(&weapon, &ecx, env.random, base_damage)
    };
    let magic_boost = scale * (enchanted - base_damage);
    base_damage *= 0.2 + scale * scale * 0.8;
    // `onAttack`: `resetOnlyAttackStrengthTicker`.
    if let Some(p) = ents.player_mut(attacker) {
        p.survival.attack_ticker = 0;
    }
    if deflect_projectile(ents, env, attacker, target) {
        return;
    }
    if base_damage > 0.0 || magic_boost > 0.0 {
        let hit = Hit {
            weapon: &weapon,
            source,
            base_damage,
            magic_boost,
            scale,
            smash,
            fall_distance,
            in_water,
            climbing,
            blind,
            ppos,
            flags,
            y_rot,
        };
        attack_hit(level, ents, env, cx, attacker, target, hit);
    }
    post_piercing_attack(level, ents, env, cx, attacker);
}

/// `LivingEntity.postPiercingAttack` of a player after a melee attack or a jab
/// (`EnchantmentHelper.doPostPiercingAttackEffects`): the `post_piercing_attack` effects of
/// its weapon in the main hand (Lunge) on the player itself.
pub fn post_piercing_attack(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    user: EntityId,
) {
    let Some(p) = ents.player(user) else {
        return;
    };
    // `getWeaponItem`.
    let weapon = p
        .survival
        .spin_attack
        .as_ref()
        .map_or_else(|| p.inventory.selected_item().clone(), |s| s.item.clone());
    if weapon.is_empty() {
        return;
    }
    let combat = combat_data();
    let (enchants, tags): (&Enchantments, &dyn EntityTags) = (&combat.enchants, &*combat);
    let effects = {
        let ecx = EnchantContext {
            types: env.types,
            tags,
            this: facts(ents, user),
            attacker: None,
            direct_attacker: None,
            attacking_player: None,
            source: None,
            raining: false,
            thundering: false,
            location: None,
        };
        enchants.post_piercing_attack_effects(&weapon, &ecx, env.random)
    };
    for (effect, ench_level) in effects {
        run_entity_effect(
            level,
            ents,
            env,
            cx,
            &effect,
            ench_level,
            user,
            Some((user, EquipmentSlot::MainHand)),
        );
    }
}

/// The attack values `Player.attack` computed before the hit.
struct Hit<'a> {
    weapon: &'a ItemStack,
    source: DamageSource,
    base_damage: f32,
    magic_boost: f32,
    scale: f32,
    smash: bool,
    fall_distance: f64,
    in_water: bool,
    climbing: bool,
    blind: bool,
    ppos: Vec3,
    flags: EntityFlags,
    y_rot: f32,
}

/// The hit of `Player.attack` with damage to deal: knockback, critical and sweep attacks,
/// the hurt and what follows it.
fn attack_hit(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    attacker: EntityId,
    target: EntityId,
    hit: Hit<'_>,
) {
    let Hit {
        weapon,
        source,
        mut base_damage,
        magic_boost,
        scale,
        smash,
        fall_distance,
        in_water,
        climbing,
        blind,
        ppos,
        flags,
        y_rot,
    } = hit;
    let combat = combat_data();
    let (enchants, tags): (&Enchantments, &dyn EntityTags) = (&combat.enchants, &*combat);
    let living = is_living_target(ents, target);
    let full = scale > 0.9;
    let sprinting = flags.contains(EntityFlags::SPRINTING);
    let on_ground = flags.contains(EntityFlags::ON_GROUND);
    let knockback_attack = sprinting && full;
    if knockback_attack {
        player_sound(env, ppos, "minecraft:entity.player.attack.knockback");
    }
    base_damage += mace_bonus(ents, env, attacker, target, weapon, &source, fall_distance);
    let passenger = ents.vehicle_of(attacker).is_some();
    // `canCriticalAttack`: only living entities.
    let crit = full
        && fall_distance > 0.0
        && !on_ground
        && !climbing
        && !in_water
        && !blind
        && !passenger
        && !sprinting
        && living;
    if crit {
        base_damage *= 1.5;
    }
    let total = base_damage + magic_boost;
    // `isSweepAttack`: a full strength sword hit on the ground while moving slower than 2.5
    // times the speed (`getSpeed`: the movement speed attribute; the sprint modifier never
    // counts, a sprinting full strength hit is a knockback attack).
    let sweep = full && !crit && !knockback_attack && on_ground && {
        let known = crate::spear::known_movement(ents, attacker);
        #[allow(clippy::cast_possible_truncation)]
        let speed =
            player_attribute(ents, attacker, "minecraft:movement_speed", 0.1, 0.0, 1024.0) as f32;
        let max = f64::from(speed) * 2.5;
        known.x * known.x + known.z * known.z < max * max
            && crate::player::play::game_data()
                .is_some_and(|d| d.item_in_tag(weapon.item, "minecraft:swords"))
    };
    let old_movement = ents.store.get(target).map(|e| e.velocity());
    let old_health = living_health(ents, target);
    let hurt = combat::hurt(level, ents, env, cx, target, source, total);
    if !hurt {
        player_sound(env, ppos, "minecraft:entity.player.attack.nodamage");
        return;
    }
    // `getKnockback`: the attack knockback attribute through the enchantments, halved.
    let kb_base = player_attribute(ents, attacker, "minecraft:attack_knockback", 0.0, 0.0, 5.0);
    #[allow(clippy::cast_possible_truncation)]
    let kb = {
        let ecx = damage_context(ents, env.types, tags, target, &source);
        enchants.modify_knockback(weapon, &ecx, env.random, kb_base as f32) / 2.0
    };
    let kb = kb + if knockback_attack { 0.5 } else { 0.0 };
    let by = Some((attacker, total, true));
    extra_knockback(ents, env, attacker, target, kb, y_rot, old_movement, by);
    if sweep {
        sweep_attack(
            level,
            ents,
            env,
            cx,
            attacker,
            target,
            base_damage,
            &source,
            scale,
        );
    }
    let network_id = ents.store.get(target).map_or(0, |e| e.network_id().0);
    if crit {
        player_sound(env, ppos, "minecraft:entity.player.attack.crit");
        animate(env, target, network_id, CRITICAL_HIT);
    }
    if !crit && !sweep {
        player_sound(
            env,
            ppos,
            if full {
                "minecraft:entity.player.attack.strong"
            } else {
                "minecraft:entity.player.attack.weak"
            },
        );
    }
    if magic_boost > 0.0 {
        animate(env, target, network_id, MAGIC_CRITICAL_HIT);
    }
    // `setLastHurtMob`: no mob for entities that are not living.
    if let Some(p) = ents.player_mut(attacker) {
        p.survival.last_hurt_mob = living.then_some((target, p.survival.tick_count));
    }
    // `itemAttackInteraction`: `hurtEnemy`, the post attack effects, `postHurtEnemy`; the item
    // only hurts living entities (the dragon through its parts).
    let enemy = ents.mob(target).is_some() || ents.player(target).is_some();
    if enemy && smash {
        mace_hurt_enemy(ents, env, cx, attacker, target, fall_distance);
    }
    weapon_used_stat(ents, attacker, weapon, target);
    post_attack(level, ents, env, cx, target, &source, Some(weapon));
    if enemy {
        hurt_weapon(ents, env, attacker, weapon);
        if smash && let Some(p) = ents.player_mut(attacker) {
            // `MaceItem.postHurtEnemy`: `resetFallDistance` (`FALL_FROM_HEIGHT`).
            crate::advancements::tracking::reset_fall_distance(p);
        }
    }
    damage_stats_and_hearts(ents, cx, attacker, target, old_health);
    if let Some(p) = ents.player_mut(attacker) {
        player::cause_food_exhaustion(p, 0.1);
    }
}

/// `Player.damageStatsAndHearts`: `DAMAGE_DEALT` of the health the living target lost, then
/// dark hearts for more than one heart of damage.
pub(crate) fn damage_stats_and_hearts(
    ents: &mut Entities,
    cx: &mut MobContext,
    attacker: EntityId,
    target: EntityId,
    old_health: Option<f32>,
) {
    if !is_living_target(ents, target) {
        return;
    }
    if let (Some(old), Some(now)) = (old_health, living_health(ents, target)) {
        let damage = old - now;
        if let Some(p) = ents.player_mut(attacker) {
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let v = (damage * 10.0).round() as i32;
            p.stats.award(crate::stats::custom::DAMAGE_DEALT, v);
        }
        if damage > 2.0
            && let Some(e) = ents.store.get(target)
        {
            let (pos, height) = (e.pos(), f64::from(e.dims().height));
            #[allow(clippy::cast_possible_truncation)]
            let count = (f64::from(damage) * 0.5) as i32;
            cx.send_particles(
                crate::level::particles::simple("minecraft:damage_indicator"),
                Vec3::new(pos.x, pos.y + height * 0.5, pos.z),
                count,
                Vec3::new(0.1, 0.0, 0.1),
                0.2,
            );
        }
    }
}

/// `LivingEntity.getHealth` of a mob or player.
pub(crate) fn living_health(ents: &Entities, id: EntityId) -> Option<f32> {
    ents.mob(id)
        .map(|m| m.health)
        .or_else(|| ents.player(id).map(|p| p.survival.health))
}

/// `ClientboundAnimatePacket.CRITICAL_HIT` (`ServerPlayer.crit`).
const CRITICAL_HIT: u8 = 1;
/// `ClientboundAnimatePacket.MAGIC_CRITICAL_HIT` (`ServerPlayer.magicCrit`).
pub(crate) const MAGIC_CRITICAL_HIT: u8 = 2;

/// `sendToTrackingPlayersAndSelf(new ClientboundAnimatePacket(entity, action))`.
pub(crate) fn animate(env: &mut Env<'_>, target: EntityId, network_id: i32, action: u8) {
    env.out.push(Effect::Tracking {
        entity: target,
        with_self: true,
        packet: SharedPacket::of(&Animate {
            entity_id: network_id,
            action,
        }),
    });
}

/// `MaceItem.getAttackDamageBonus`: smash attacks add damage per fallen block.
fn mace_bonus(
    ents: &Entities,
    env: &mut Env<'_>,
    attacker: EntityId,
    target: EntityId,
    weapon: &ItemStack,
    source: &DamageSource,
    fall_distance: f64,
) -> f32 {
    if !can_smash_attack(ents, attacker, weapon) {
        return 0.0;
    }
    let f = fall_distance;
    let bonus = if f <= 3.0 {
        4.0 * f
    } else if f <= 8.0 {
        12.0 + 2.0 * (f - 3.0)
    } else {
        22.0 + f - 8.0
    };
    let combat = combat_data();
    let (enchants, tags): (&Enchantments, &dyn EntityTags) = (&combat.enchants, &*combat);
    let ecx = damage_context(ents, env.types, tags, target, source);
    #[allow(clippy::cast_possible_truncation)]
    let per_block = enchants.modify_fall_based_damage(weapon, &ecx, env.random, 0.0) * f as f32;
    #[allow(clippy::cast_possible_truncation)]
    let b = bonus as f32 + per_block;
    b
}

/// `MaceItem.canSmashAttack` with a mace: falling more than 1.5 blocks, not gliding.
pub fn can_smash_attack(ents: &Entities, attacker: EntityId, weapon: &ItemStack) -> bool {
    if weapon.is_empty() || item::item_id(weapon).name() != "minecraft:mace" {
        return false;
    }
    let gliding = ents
        .store
        .get(attacker)
        .is_some_and(|e| e.flags().contains(EntityFlags::FALL_FLYING));
    let fall = ents.player(attacker).map_or_else(
        || ents.mob(attacker).map_or(0.0, |m| m.fall_distance),
        |p| p.survival.fall_distance,
    );
    fall > 1.5 && !gliding
}

/// `MaceItem.hurtEnemy` of a smash attack: the attacker stops falling (its later fall from the
/// impact point does not hurt), the smash sound, and the living entities within 3.5 blocks of
/// the target are knocked away.
fn mace_hurt_enemy(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    attacker: EntityId,
    target: EntityId,
    fall_distance: f64,
) {
    let Some((apos, network_id)) = ents
        .store
        .get(attacker)
        .map(|e| (e.pos(), e.network_id().0))
    else {
        return;
    };
    if let Some(mut e) = ents.store.get_mut(attacker) {
        let v = e.as_ref().velocity();
        e.set_velocity(Vec3::new(v.x, f64::from(0.01_f32), v.z));
    }
    if let Some(p) = ents.player_mut(attacker) {
        // `setIgnoreFallDamageFromCurrentImpulse(true, calculateImpactPosition)`.
        let s = &mut p.survival;
        let impact = match s.impulse_impact {
            Some(i) if i.y <= apos.y => i,
            _ => apos,
        };
        s.impulse_grace = s.impulse_grace.max(40);
        s.impulse_impact = Some(impact);
    }
    if let Some(v) = ents.store.get(attacker).map(|e| e.velocity()) {
        env.out.push(Effect::To {
            player: attacker,
            packet: SharedPacket::of(&SetEntityMotion {
                entity_id: network_id,
                movement: LpVec3(vanillite_protocol::types::Vec3 {
                    x: v.x,
                    y: v.y,
                    z: v.z,
                }),
            }),
        });
    }
    let Some((tpos, t_on_ground, tbox)) = ents.store.get(target).map(|e| {
        (
            e.pos(),
            e.flags().contains(EntityFlags::ON_GROUND),
            e.aabb(),
        )
    }) else {
        return;
    };
    if t_on_ground && let Some(p) = ents.player_mut(attacker) {
        p.survival.spawn_extra_particles_on_fall = true;
    }
    let sound = if t_on_ground {
        if fall_distance > 5.0 {
            "minecraft:item.mace.smash_ground_heavy"
        } else {
            "minecraft:item.mace.smash_ground"
        }
    } else {
        "minecraft:item.mace.smash_air"
    };
    player_sound(env, apos, sound);
    // `knockback`: the dust (level event 2013) and the push.
    let on_pos = vanillite_base::BlockPos::new(
        vanillite_base::math::floor(tpos.x),
        vanillite_base::math::floor(tpos.y - 0.2),
        vanillite_base::math::floor(tpos.z),
    );
    cx.actions
        .push(super::WorldAction::LevelEvent(2013, on_pos, 750));
    let mut near = Vec::new();
    ents.store
        .for_each_in(&tbox.inflate(3.5, 3.5, 3.5), false, |e| near.push(e));
    let target_uuid = ents.store.get(target).map(|e| e.uuid());
    for nearby in near {
        // `knockbackPredicate`: living, no spectator, neither attacker nor target, not allied
        // to the attacker, no tamed animal of the target, no marker armor stand, no flying
        // creative player.
        if nearby == attacker || nearby == target {
            continue;
        }
        let living = ents.mob(nearby).is_some()
            || ents.player(nearby).is_some_and(|p| {
                p.game_mode != vanillite_config::GameMode::Spectator
                    && !(p.game_mode == vanillite_config::GameMode::Creative && p.survival.flying)
            });
        if !living || is_allied(ents, env, attacker, nearby) {
            continue;
        }
        if ents.mob(nearby).is_some_and(|m| {
            (m.tame && m.owner.is_some() && m.owner == target_uuid)
                || (crate::decoration::stand::is_stand(m.kind)
                    && crate::decoration::stand::is_marker(m))
        }) {
            continue;
        }
        let Some(npos) = ents.store.get(nearby).map(|e| e.pos()) else {
            continue;
        };
        let direction = npos - tpos;
        if direction.length_sqr() > 3.5 * 3.5 {
            continue;
        }
        let resistance = knockback_resistance(ents, nearby);
        let power = (3.5 - direction.length())
            * f64::from(0.7_f32)
            * if fall_distance > 5.0 { 2.0 } else { 1.0 }
            * (1.0 - resistance);
        if power <= 0.0 {
            continue;
        }
        let k = direction.normalize() * power;
        if let Some(mut e) = ents.store.get_mut(nearby) {
            let v = e.as_ref().velocity();
            e.set_velocity(v + Vec3::new(k.x, f64::from(0.7_f32), k.z));
            e.set_flags(EntityFlags::HAS_IMPULSE, true);
            if ents_is_player(&e) {
                e.set_flags(EntityFlags::HURT_MARKED, true);
            }
        }
    }
}

fn ents_is_player(e: &vanillite_world::entity::EntityMut<'_>) -> bool {
    e.as_ref().kind() == vanillite_data::entity::ids::PLAYER
}

/// The `knockback_resistance` attribute of a living entity.
pub(crate) fn knockback_resistance(ents: &Entities, id: EntityId) -> f64 {
    if let Some(m) = ents.mob(id) {
        return m.attrs.knockback_resistance;
    }
    player_attribute(ents, id, "minecraft:knockback_resistance", 0.0, 0.0, 1.0)
}

/// `Entity.push(x, y, z)` of an entity that is not living (`needsSync`); wind charges keep
/// their course.
fn push_entity(ents: &mut Entities, id: EntityId, push: Vec3) {
    use super::missile::MissileKind;
    if ents.missile(id).is_some_and(|d| {
        matches!(
            d.kind,
            MissileKind::WindCharge | MissileKind::BreezeWindCharge
        )
    }) {
        return;
    }
    if let Some(mut e) = ents.store.get_mut(id) {
        let v = e.as_ref().velocity();
        e.set_velocity(v + push);
        e.set_flags(EntityFlags::HAS_IMPULSE, true);
    }
}

/// `causeExtraKnockback`: knocks the target back in the attacker's facing and slows the
/// attacker; a hit player gets its new motion at once.
#[allow(clippy::too_many_arguments)]
pub(crate) fn extra_knockback(
    ents: &mut Entities,
    env: &mut Env<'_>,
    attacker: EntityId,
    target: EntityId,
    amount: f32,
    y_rot: f32,
    old_movement: Option<Vec3>,
    by: Option<(EntityId, f32, bool)>,
) {
    if amount > 0.0 {
        let rad = f64::from(y_rot * super::DEG_TO_RAD);
        let (sin, cos) = (
            f64::from(vanillite_base::math::sin(rad)),
            f64::from(vanillite_base::math::cos(rad)),
        );
        if is_living_target(ents, target) {
            combat::knock_living(ents, env, target, f64::from(amount), sin, -cos, by);
        } else if ents.mob(target).is_none() {
            // Other entities are pushed (`Entity.push`); the dragon's parts take only the hit.
            let (s, c) = (
                vanillite_base::math::sin(rad),
                vanillite_base::math::cos(rad),
            );
            let push = Vec3::new(f64::from(-s * amount), 0.1, f64::from(c * amount));
            push_entity(ents, target, push);
        }
        if let Some(mut e) = ents.store.get_mut(attacker) {
            let v = e.as_ref().velocity();
            e.set_velocity(v.multiply(0.6, 1.0, 0.6));
            e.set_flags(EntityFlags::SPRINTING, false);
        }
    }
    // A hit player learns its knockback right away (`syncVelocity`).
    if ents.player(target).is_some()
        && let Some(mut e) = ents.store.get_mut(target)
        && e.as_ref().flags().contains(EntityFlags::HURT_MARKED)
    {
        let v = e.as_ref().velocity();
        let id = e.as_ref().network_id().0;
        e.set_flags(EntityFlags::HURT_MARKED, false);
        if let Some(old) = old_movement {
            e.set_velocity(old);
        }
        env.out.push(Effect::To {
            player: target,
            packet: SharedPacket::of(&SetEntityMotion {
                entity_id: id,
                movement: LpVec3(vanillite_protocol::types::Vec3 {
                    x: v.x,
                    y: v.y,
                    z: v.z,
                }),
            }),
        });
    }
}

/// `doSweepAttack`: the living entities around the target take sweeping damage.
#[allow(clippy::too_many_arguments)]
fn sweep_attack(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    attacker: EntityId,
    target: EntityId,
    base_damage: f32,
    source: &DamageSource,
    scale: f32,
) {
    let Some(ppos) = ents.store.get(attacker).map(|e| e.pos()) else {
        return;
    };
    let y_rot = ents.store.get(attacker).map_or(0.0, |e| e.rot().y_rot);
    player_sound(env, ppos, "minecraft:entity.player.attack.sweep");
    let ratio = player_attribute(
        ents,
        attacker,
        "minecraft:sweeping_damage_ratio",
        0.0,
        0.0,
        1.0,
    );
    #[allow(clippy::cast_possible_truncation)]
    let sweep_damage = 1.0 + ratio as f32 * base_damage;
    let Some(area) = ents
        .store
        .get(target)
        .map(|e| e.aabb().inflate(1.0, 0.25, 1.0))
    else {
        return;
    };
    let mut near = Vec::new();
    ents.store.for_each_in(&area, false, |e| near.push(e));
    let weapon = ents
        .player(attacker)
        .map(|p| p.inventory.selected_item().clone())
        .unwrap_or_default();
    let combat = combat_data();
    let (enchants, tags): (&Enchantments, &dyn EntityTags) = (&combat.enchants, &*combat);
    for id in near {
        if id == attacker || id == target || !is_alive(ents, id) {
            continue;
        }
        // Allies (teammates, the player's tamed animals, an allay that likes it) and marker
        // armor stands are spared.
        let marker = ents.mob(id).is_some_and(|m| {
            m.kind == super::Kind::ArmorStand && crate::decoration::stand::is_marker(m)
        });
        if marker || is_allied(ents, env, attacker, id) {
            continue;
        }
        let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
            continue;
        };
        if pos.distance_to_sqr(ppos) >= 9.0 {
            continue;
        }
        let damage = {
            let ecx = damage_context(ents, env.types, tags, id, source);
            enchants.modify_damage(&weapon, &ecx, env.random, sweep_damage) * scale
        };
        if combat::hurt(level, ents, env, cx, id, *source, damage) {
            let rad = f64::from(y_rot * super::DEG_TO_RAD);
            combat::knock_living(
                ents,
                env,
                id,
                f64::from(0.4_f32),
                f64::from(vanillite_base::math::sin(rad)),
                -f64::from(vanillite_base::math::cos(rad)),
                Some((attacker, damage, false)),
            );
            post_attack(level, ents, env, cx, id, source, Some(&weapon));
        }
    }
    let rad = f64::from(y_rot * super::DEG_TO_RAD);
    let dx = -f64::from(vanillite_base::math::sin(rad));
    let dz = f64::from(vanillite_base::math::cos(rad));
    let height = ents
        .store
        .get(attacker)
        .map_or(0.0, |e| f64::from(e.dims().height));
    cx.send_particles(
        crate::level::particles::simple("minecraft:sweep_attack"),
        Vec3::new(ppos.x + dx, ppos.y + height * 0.5, ppos.z + dz),
        0,
        Vec3::new(dx, 0.0, dz),
        0.0,
    );
}

/// `EnchantmentHelper.doPostAttackEffectsWithItemSource`: the post attack effects of the
/// victim's equipment (Thorns hurts the attacker), then those of the attacker's `weapon`
/// (Fire Aspect sets the victim on fire, Bane of Arthropods slows arthropods). The victim's
/// gear counts for players and mobs alike.
#[allow(clippy::too_many_arguments)]
pub fn post_attack(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    victim: EntityId,
    source: &DamageSource,
    weapon: Option<&ItemStack>,
) {
    let combat = combat_data();
    let (enchants, tags): (&Enchantments, &dyn EntityTags) = (&combat.enchants, &*combat);
    // `runIterationOnEquipment` of the victim (`EquipmentSlot` order).
    for (slot, stack) in equipment(ents, victim) {
        let effects = {
            let mut ecx = damage_context(ents, env.types, tags, victim, source);
            with_world(&mut ecx, level, ents, victim, source);
            enchants.post_attack_effects(&stack, Some(slot), Party::Victim, &ecx, env.random)
        };
        for (effect, ench_level, affected) in effects {
            let target = affected_entity(affected, victim, source);
            if let Some(target) = target {
                run_entity_effect(
                    level,
                    ents,
                    env,
                    cx,
                    &effect,
                    ench_level,
                    target,
                    Some((victim, slot)),
                );
            }
        }
    }
    // The weapon of a living attacker (`runIterationOnItem(source, MAINHAND, attacker)`).
    let Some(weapon) = weapon else {
        return;
    };
    let Some(attacker) = source
        .cause
        .filter(|&c| ents.mob(c).is_some() || ents.player(c).is_some())
    else {
        return;
    };
    let effects = {
        let mut ecx = damage_context(ents, env.types, tags, victim, source);
        with_world(&mut ecx, level, ents, victim, source);
        enchants.post_attack_effects(
            weapon,
            Some(EquipmentSlot::MainHand),
            Party::Attacker,
            &ecx,
            env.random,
        )
    };
    for (effect, ench_level, affected) in effects {
        if let Some(target) = affected_entity(affected, victim, source) {
            run_entity_effect(
                level,
                ents,
                env,
                cx,
                &effect,
                ench_level,
                target,
                Some((attacker, EquipmentSlot::MainHand)),
            );
        }
    }
}

/// `EnchantmentHelper.doPostAttackEffects(level, victim, source)`: [`post_attack`] with the
/// weapon of the source's living cause (`DamageSource.getWeaponItem`, its main hand).
pub fn post_attack_effects(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    victim: EntityId,
    source: &DamageSource,
) {
    let weapon = source.cause.and_then(|c| {
        ents.mob(c)
            .map(|m| m.equipment.get(EquipmentSlot::MainHand).clone())
            .or_else(|| ents.player(c).map(|p| p.inventory.selected_item().clone()))
    });
    post_attack(level, ents, env, cx, victim, source, weapon.as_ref());
}

/// The entity a post attack effect affects (`TargetedConditionalEffect.affected`).
fn affected_entity(affected: Party, victim: EntityId, source: &DamageSource) -> Option<EntityId> {
    match affected {
        Party::Attacker => source.cause,
        Party::DamagingEntity => source.direct,
        Party::Victim => Some(victim),
    }
}

/// `EnchantmentEntityEffect.apply` of a post attack effect on `target`; `item` is the owner
/// and slot of the enchanted item (`EnchantedItemInUse`).
#[allow(clippy::too_many_arguments)]
fn run_entity_effect(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    effect: &EntityEffect,
    ench_level: i32,
    target: EntityId,
    item: Option<(EntityId, EquipmentSlot)>,
) {
    match effect {
        EntityEffect::Ignite(seconds) => {
            // `igniteForSeconds`: `Mth.floor(seconds * 20)` ticks.
            #[allow(clippy::cast_possible_truncation)]
            let ticks = (seconds.calculate(ench_level) * 20.0).floor() as i32;
            ignite(ents, target, ticks);
        }
        EntityEffect::ApplyMobEffect {
            effects,
            min_duration,
            max_duration,
            min_amplifier,
            max_amplifier,
        } => {
            crate::effect::living::apply_mob_effect(
                level,
                ents,
                env,
                cx,
                target,
                effects,
                (
                    min_duration.calculate(ench_level),
                    max_duration.calculate(ench_level),
                ),
                (
                    min_amplifier.calculate(ench_level),
                    max_amplifier.calculate(ench_level),
                ),
            );
        }
        EntityEffect::DamageEntity {
            damage_type,
            min_damage,
            max_damage,
        } => {
            let (min, max) = (
                min_damage.calculate(ench_level),
                max_damage.calculate(ench_level),
            );
            // `Mth.randomBetween(entity.getRandom(), min, max)`.
            let roll = if let Some(m) = ents.mob_mut(target) {
                m.random.next_float()
            } else if let Some(p) = ents.player_mut(target) {
                p.survival.random.next_float()
            } else {
                crate::experience_orb::with_entity_random(|r| r.next_float())
            };
            let amount = roll * (max - min) + min;
            let Some(kind) = env.types.kind(damage_type) else {
                return;
            };
            // `new DamageSource(type, item.owner())`.
            let owner = item.map(|(o, _)| o);
            let source = match owner {
                Some(o) => entity_source(ents, kind, o, o),
                None => DamageSource::of(kind),
            };
            combat::hurt(level, ents, env, cx, target, source, amount);
        }
        EntityEffect::ChangeItemDamage(amount) => {
            #[allow(clippy::cast_possible_truncation)]
            let amount = amount.calculate(ench_level) as i32;
            if let Some((owner, slot)) = item {
                hurt_equipment(ents, env, owner, slot, amount);
            }
        }
        EntityEffect::AllOf(list) => {
            for e in list {
                run_entity_effect(level, ents, env, cx, e, ench_level, target, item);
            }
        }
        EntityEffect::WindBurst {
            radius,
            knockback,
            sound,
        } => {
            let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
                return;
            };
            cx.actions
                .push(super::WorldAction::WindBurst(super::WindBurst {
                    source: None,
                    pos,
                    radius: *radius,
                    knockback: f64::from(knockback.calculate(ench_level)),
                    sound: static_sound(sound),
                }));
        }
        EntityEffect::SummonLightning => {
            if let Some(pos) = ents.store.get(target).map(|e| e.pos()) {
                let cause = item
                    .map(|(owner, _)| owner)
                    .filter(|&o| ents.player(o).is_some());
                cx.actions.push(super::WorldAction::Level(
                    super::boss::warden::LevelAction::Lightning { pos, cause },
                ));
            }
        }
        EntityEffect::PlaySound {
            sounds,
            volume,
            pitch,
        } => {
            let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
                return;
            };
            let Some(sound) = EntityEffect::sound_of_level(sounds, ench_level) else {
                return;
            };
            let source = ents.mob(target).map_or(
                vanillite_protocol::packets::play::clientbound::SoundSource::Players,
                |m| m.sound_source(),
            );
            crate::survival::play_sound(
                env.out,
                None,
                pos,
                sound,
                source,
                *volume,
                *pitch,
                crate::survival::sound_seed(),
            );
        }
        EntityEffect::ApplyExhaustion(amount) => {
            if let Some(p) = ents.player_mut(target) {
                player::cause_food_exhaustion(p, amount.calculate(ench_level));
            }
        }
        EntityEffect::ApplyImpulse {
            direction,
            coordinate_scale,
            magnitude,
        } => apply_impulse(
            ents,
            env,
            target,
            *direction,
            *coordinate_scale,
            magnitude.calculate(ench_level),
        ),
        EntityEffect::Unsupported => {}
    }
}

/// `ApplyEntityImpulse.apply`: `direction` turned by the entity's look
/// (`getLookQuaternion().transform`), scaled per axis and by `magnitude`, is added to its
/// motion; a player learns it at once, a living entity gets 10 ticks of impulse grace.
fn apply_impulse(
    ents: &mut Entities,
    env: &mut Env<'_>,
    target: EntityId,
    direction: [f64; 3],
    scale: [f64; 3],
    magnitude: f32,
) {
    let Some(rot) = ents.store.get(target).map(|e| e.rot()) else {
        return;
    };
    let d = look_transform(rot.x_rot, rot.y_rot, direction);
    let m = f64::from(magnitude);
    let impulse = Vec3::new(d.x * scale[0] * m, d.y * scale[1] * m, d.z * scale[2] * m);
    if !(impulse.x.is_finite() && impulse.y.is_finite() && impulse.z.is_finite()) {
        return;
    }
    let player = ents.player(target).is_some();
    let Some(mut e) = ents.store.get_mut(target) else {
        return;
    };
    let v = e.as_ref().velocity() + impulse;
    let network_id = e.as_ref().network_id().0;
    e.set_velocity(v);
    if player {
        env.out.push(Effect::To {
            player: target,
            packet: SharedPacket::of(&SetEntityMotion {
                entity_id: network_id,
                movement: LpVec3(vanillite_protocol::types::Vec3 {
                    x: v.x,
                    y: v.y,
                    z: v.z,
                }),
            }),
        });
    } else {
        // `syncVelocity`.
        e.set_flags(EntityFlags::HAS_IMPULSE, true);
    }
    // `applyPostImpulseGraceTime(10)`.
    if let Some(p) = ents.player_mut(target) {
        p.survival.impulse_grace = p.survival.impulse_grace.max(10);
    }
}

/// `Entity.calculateViewQuaternion(xRot, yRot).transform(v)`: JOML's
/// `Quaternionf().rotationY(-yRot°).rotateX(xRot°)` in floats applied to a double vector.
fn look_transform(x_rot: f32, y_rot: f32, v: [f64; 3]) -> Vec3 {
    // `org.joml.Math.sin` and `cosFromSin` without fast math.
    #[allow(clippy::cast_possible_truncation)]
    let sin = |a: f32| f64::from(a).sin() as f32;
    let cos_from_sin = |sin: f32, angle: f32| {
        #[allow(clippy::cast_possible_truncation)]
        let cos = f64::from(1.0 - sin * sin).sqrt() as f32;
        const PI2: f32 = 6.283_185_5;
        let a = angle + 1.570_796_4;
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let mut b = a - (a / PI2) as i32 as f32 * PI2;
        if b < 0.0 {
            b += PI2;
        }
        if b >= std::f32::consts::PI { -cos } else { cos }
    };
    let half = |a: f32| {
        let s = sin(a * 0.5);
        (s, cos_from_sin(s, a * 0.5))
    };
    // `rotationY`: (0, s, 0, c).
    let (sy, cy) = half(-y_rot * super::DEG_TO_RAD);
    let (x, y, z, w) = (0.0_f32, sy, 0.0_f32, cy);
    // `rotateX`.
    let (sx, cx) = half(x_rot * super::DEG_TO_RAD);
    let (x, y, z, w) = (
        w * sx + x * cx,
        y * cx + z * sx,
        z * cx - y * sx,
        w * cx - x * sx,
    );
    // `transform(double, double, double, Vector3d)`.
    let (xx, yy, zz, ww) = (x * x, y * y, z * z, w * w);
    let (xy, xz, yz, xw, zw, yw) = (x * y, x * z, y * z, x * w, z * w, y * w);
    let k = 1.0 / (xx + yy + zz + ww);
    let d = |f: f32| f64::from(f);
    let row = |a: f32, b: f32, c: f32| d(a) * v[0] + (d(b) * v[1] + d(c) * v[2]);
    Vec3::new(
        row(
            (xx - yy - zz + ww) * k,
            2.0 * (xy - zw) * k,
            2.0 * (xz + yw) * k,
        ),
        row(
            2.0 * (xy + zw) * k,
            (yy - xx - zz + ww) * k,
            2.0 * (yz - xw) * k,
        ),
        row(
            2.0 * (xz - yw) * k,
            2.0 * (yz + xw) * k,
            (zz - xx - yy + ww) * k,
        ),
    )
}

/// The burst sounds of the `explode` effects as static names.
fn static_sound(sound: &str) -> &'static str {
    match sound {
        "minecraft:entity.wind_charge.wind_burst" => "minecraft:entity.wind_charge.wind_burst",
        "minecraft:entity.generic.explode" => "minecraft:entity.generic.explode",
        _ => "minecraft:entity.wind_charge.wind_burst",
    }
}

/// The world facts of an enchantment context: the weather and whether the entities of the
/// context see the sky.
pub fn with_world(
    ecx: &mut crate::enchant::EnchantContext<'_>,
    level: &Level,
    ents: &Entities,
    victim: EntityId,
    source: &DamageSource,
) {
    ecx.raining = level.is_raining();
    ecx.thundering = level.is_thundering();
    let sky = |id: Option<EntityId>| {
        id.and_then(|i| ents.store.get(i)).is_some_and(|e| {
            let p = e.pos();
            super::can_see_sky(level, vanillite_base::BlockPos::containing(p.x, p.y, p.z))
        })
    };
    if let Some(f) = ecx.this.as_mut() {
        f.can_see_sky = sky(Some(victim));
    }
    if let Some(f) = ecx.attacker.as_mut() {
        f.can_see_sky = sky(source.cause);
    }
    if let Some(f) = ecx.direct_attacker.as_mut() {
        f.can_see_sky = sky(source.direct);
    }
}

/// `ItemStack.hurtAndBreak(amount, level, player)` of a player's equipment piece: Unbreaking
/// saves points (armour: 2·level / (5·level + 5) each, other items level / (level + 1));
/// creative players' items take no damage.
pub(crate) fn hurt_equipment(
    ents: &mut Entities,
    env: &mut Env<'_>,
    owner: EntityId,
    slot: EquipmentSlot,
    amount: i32,
) {
    if ents.mob(owner).is_some() {
        hurt_mob_equipment(ents, env, owner, slot, amount);
        return;
    }
    let Some(p) = ents.player(owner) else {
        return;
    };
    let index = match slot {
        EquipmentSlot::MainHand => p.inventory.selected,
        EquipmentSlot::OffHand => crate::inventory::OFFHAND,
        EquipmentSlot::Feet => 36,
        EquipmentSlot::Legs => 37,
        EquipmentSlot::Chest => 38,
        EquipmentSlot::Head => 39,
        EquipmentSlot::Body | EquipmentSlot::Saddle => return,
    };
    hurt_inventory_item(ents, env, owner, index, amount);
}

/// `ItemStack.hurtAndBreak(amount, mob, slot)` of a mob's equipment: the `item_damage`
/// effects (Unbreaking) from the level random, then `onEquippedItemBroken` (the slot's break
/// entity event) when it breaks.
fn hurt_mob_equipment(
    ents: &mut Entities,
    env: &mut Env<'_>,
    owner: EntityId,
    slot: EquipmentSlot,
    amount: i32,
) {
    let Some(m) = ents.mob(owner) else {
        return;
    };
    let mut stack = m.equipment.get(slot).clone();
    let damage = crate::durability::change(&stack, amount, false, env.random);
    if damage == 0 {
        return;
    }
    let d = item::damage(&stack).saturating_add(damage);
    let broken = crate::durability::apply(&mut stack, d);
    let Some(m) = ents.mob_mut(owner) else {
        return;
    };
    m.equipment.set(slot, stack);
    ents.equipment_updates.push(owner);
    if broken {
        crate::mob::animal::entity_event(ents, env, owner, crate::durability::break_event(slot));
    }
}

/// The equipment slot of a player's inventory index (selected hotbar slot, off hand, armour).
pub(crate) fn inventory_slot(selected: usize, index: usize) -> EquipmentSlot {
    match index {
        crate::inventory::OFFHAND => EquipmentSlot::OffHand,
        36 => EquipmentSlot::Feet,
        37 => EquipmentSlot::Legs,
        38 => EquipmentSlot::Chest,
        39 => EquipmentSlot::Head,
        i if i == selected => EquipmentSlot::MainHand,
        _ => EquipmentSlot::MainHand,
    }
}

/// `ItemStack.hurtAndBreak(amount, player, slot)` of the stack at `index` of a player's
/// inventory.
pub(crate) fn hurt_inventory_item(
    ents: &mut Entities,
    env: &mut Env<'_>,
    owner: EntityId,
    index: usize,
    amount: i32,
) {
    let Some(p) = ents.player(owner) else {
        return;
    };
    let creative = p.game_mode == vanillite_config::GameMode::Creative;
    let slot = inventory_slot(p.inventory.selected, index);
    let stack = p.inventory.get(index).clone();
    let damage = crate::durability::change(&stack, amount, creative, env.random);
    if damage == 0 {
        return;
    }
    let Some(p) = ents.player_mut(owner) else {
        return;
    };
    let crate::player::PlayerData {
        inventory,
        advancements,
        stats,
        ..
    } = p;
    let Some(s) = inventory.get_mut(index) else {
        return;
    };
    let d = item::damage(s).saturating_add(damage);
    if crate::advancements::view::apply_damage(Some(advancements), Some(stats), s, d) {
        crate::durability::broken_in_env(ents, env, owner, slot);
    }
}

/// The armour effectiveness of the damage source's weapon (`DamageSource.getWeaponItem`, the
/// held item of a player that hits directly) for `CombatRules.getDamageAfterAbsorb`
/// (`EnchantmentHelper.modifyArmorEffectiveness`: Breach); `None` when its enchantments do
/// not change it.
pub fn armor_effectiveness<'t>(
    ents: &Entities,
    types: &'t crate::survival::damage::DamageTypes,
    victim: EntityId,
    source: &DamageSource,
) -> Option<Box<dyn Fn(f32) -> f32 + 't>> {
    let weapon = source
        .direct
        .and_then(|d| ents.player(d))?
        .inventory
        .selected_item()
        .clone();
    let combat = combat_data();
    let (enchants, tags): (&Enchantments, &dyn EntityTags) = (&combat.enchants, &*combat);
    if !enchants
        .of(&weapon)
        .any(|(e, _)| !e.armor_effectiveness.is_empty())
    {
        return None;
    }
    let ecx = damage_context(ents, types, tags, victim, source);
    let facts = (
        ecx.this,
        ecx.attacker,
        ecx.direct_attacker,
        ecx.attacking_player,
        ecx.source,
    );
    Some(Box::new(move |fraction| {
        let (this, attacker, direct_attacker, attacking_player, source) = facts;
        let ecx = crate::enchant::EnchantContext {
            types,
            tags: &*combat,
            this,
            attacker,
            direct_attacker,
            attacking_player,
            source,
            raining: false,
            thundering: false,
            location: None,
        };
        crate::experience_orb::with_entity_random(|r| {
            combat
                .enchants
                .modify_armor_effectiveness(&weapon, &ecx, r, fraction)
        })
    }))
}

/// `igniteForTicks`: burns at least `ticks` more ticks.
pub fn ignite(ents: &mut Entities, id: EntityId, ticks: i32) {
    if let Some(m) = ents.mob_mut(id) {
        if ignite_mob(m, ticks)
            && let Some(mut e) = ents.store.get_mut(id)
        {
            e.mark_data_dirty();
        }
    } else if ents.player(id).is_some() {
        let burning = player_attribute(ents, id, "minecraft:burning_time", 1.0, 0.0, 1024.0);
        let ticks = burning_ticks(ticks, burning);
        if let Some(p) = ents.player_mut(id) {
            if p.survival.fire_ticks < ticks {
                p.survival.fire_ticks = ticks;
            }
            p.survival.ticks_frozen = 0;
        }
    }
}

/// `LivingEntity.igniteForTicks`: `ceil(ticks * burning_time)` (Fire Protection shortens it).
pub fn burning_ticks(ticks: i32, burning_time: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let scaled = (f64::from(ticks) * burning_time).ceil() as i32;
    scaled
}

/// `LivingEntity.igniteForTicks` of a mob: scaled by its `burning_time`, then
/// `Entity.igniteForTicks` with `clearFreeze`. Returns whether the synched frozen ticks
/// changed.
pub fn ignite_mob(m: &mut crate::mob::MobData, ticks: i32) -> bool {
    let burning = super::equipment::attribute(m, "minecraft:burning_time", 1.0, 0.0, 1024.0);
    let ticks = burning_ticks(ticks, burning);
    if m.fire_ticks < ticks {
        m.fire_ticks = ticks;
    }
    std::mem::replace(&mut m.ticks_frozen, 0) != 0
}

/// `ItemStack.hurtEnemy` on a living target: a player's weapon (an item with the `weapon`
/// component) counts as used.
pub fn weapon_used_stat(
    ents: &mut Entities,
    attacker: EntityId,
    weapon: &ItemStack,
    target: EntityId,
) {
    let living = ents.mob(target).is_some() || ents.player(target).is_some();
    if living
        && item::weapon(weapon).is_some()
        && let Some(p) = ents.player_mut(attacker)
    {
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, weapon.item),
            1,
        );
    }
}

/// `ItemStack.postHurtEnemy` of the main hand weapon: it loses `item_damage_per_attack`
/// durability ([`hurt_equipment`]: Unbreaking, not in creative mode).
fn hurt_weapon(ents: &mut Entities, env: &mut Env<'_>, attacker: EntityId, weapon: &ItemStack) {
    // While spinning the riptide trident is the weapon, in whichever hand it was used.
    if let Some(index) = ents
        .player(attacker)
        .and_then(|p| p.survival.spin_attack.as_ref())
        .map(|s| s.index)
    {
        if let Some(w) = item::weapon(weapon) {
            let amount = i32::try_from(w.item_damage_per_attack).unwrap_or(i32::MAX);
            hurt_inventory_item(ents, env, attacker, index, amount);
        }
        return;
    }
    hurt_weapon_in(ents, env, attacker, weapon, EquipmentSlot::MainHand);
}

/// `ItemStack.postHurtEnemy` of the weapon in `slot`.
pub(crate) fn hurt_weapon_in(
    ents: &mut Entities,
    env: &mut Env<'_>,
    attacker: EntityId,
    weapon: &ItemStack,
    slot: EquipmentSlot,
) {
    let Some(w) = item::weapon(weapon) else {
        return;
    };
    let amount = i32::try_from(w.item_damage_per_attack).unwrap_or(i32::MAX);
    hurt_equipment(ents, env, attacker, slot, amount);
}

/// The equipment of a player for armour and protection (`getArmorSlots` plus the hands).
pub fn equipment(ents: &Entities, player: EntityId) -> Vec<(EquipmentSlot, ItemStack)> {
    if let Some(m) = ents.mob(player) {
        // A mob's equipment in `EquipmentSlot` order.
        return (0..8)
            .filter_map(EquipmentSlot::from_ordinal)
            .map(|s| (s, m.equipment.get(s).clone()))
            .filter(|(_, s)| !s.is_empty())
            .collect();
    }
    let Some(p) = ents.player(player) else {
        return Vec::new();
    };
    crate::player::encode::EQUIPMENT_SLOTS
        .iter()
        .map(|&s| (s, p.inventory.equipment(s).clone()))
        .filter(|(_, s)| !s.is_empty())
        .collect()
}
