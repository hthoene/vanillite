//! Arrows (`AbstractArrow`, `Arrow` with its potion, `SpectralArrow`) shot by bows,
//! skeletons, strays and bogged.

use vanillite_base::math::ceil;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_data::entity::ids as entity_ids;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{EntityFlags, EntityId, NewEntity, Rotation};

use super::physics::{self, Blocks};
use super::{MobContext, Tick, atan2_deg, combat};
use crate::entities::Entities;
use crate::level::Level;
use crate::survival::play_sound;
use crate::survival::player::Env;

/// Whether `kind` is an arrow (`Arrow` or `SpectralArrow`).
pub fn is_arrow_kind(kind: vanillite_data::entity::EntityTypeId) -> bool {
    kind == entity_ids::ARROW || kind == entity_ids::SPECTRAL_ARROW
}

/// The kind data of an arrow or spectral arrow.
#[derive(Clone, Debug, PartialEq)]
pub struct ArrowData {
    pub owner: Option<EntityId>,
    /// The owner's UUID (`EntityReference`): the owner is looked up again by it every tick,
    /// also after a reload or the owner's respawn.
    pub owner_uuid: Option<u128>,
    pub base_damage: f64,
    pub in_ground: bool,
    pub life: i32,
    pub shake: i32,
    pub crit: bool,
    /// The arrow left its owner's box (`leftOwner`).
    pub left_owner: bool,
    pub last_state: Option<BlockState>,
    pub tick_count: i32,
    /// `pickup`: 0 disallowed, 1 allowed, 2 creative only.
    pub pickup: u8,
    /// The state of its own random ([`LegacyRandom::state`]).
    pub random_state: i64,
    /// The knockback of the weapon's enchantments (Punch).
    pub knockback: f32,
    /// `remainingFireTicks`: a burning arrow (Flame, wither skeletons, lava, fire) sets what
    /// it hits on fire for 5 seconds.
    pub fire: i32,
    /// `wasTouchingWater`: the water contact `baseTick` found at the end of the last flying
    /// tick (`isInWater`).
    pub touching_water: bool,
    /// The item it is picked up as (`pickupItemStack`): an arrow, a tipped arrow with its
    /// `potion_contents`, or a spectral arrow.
    pub item: crate::item::ItemStack,
    /// `inGroundTime`.
    pub in_ground_time: i32,
    /// The glowing ticks of a spectral arrow (`duration`).
    pub spectral_duration: i32,
    /// `hasBeenShot`: the `PROJECTILE_SHOOT` game event was posted.
    pub has_been_shot: bool,
    /// `PIERCE_LEVEL`: how many more entities it passes through (Piercing).
    pub pierce_level: u8,
    /// `piercingIgnoreEntityIds`: the entities it pierced (hits them no more).
    pub pierced: Vec<EntityId>,
    /// `piercedAndKilledEntities`: what a piercing arrow killed, with the type and the
    /// position at the kill (for `KILLED_BY_ARROW`).
    pub pierced_killed: Vec<(EntityId, &'static str, Vec3)>,
    /// `soundEvent` is `item.crossbow.hit` (shot from a crossbow) instead of
    /// `entity.arrow.hit`.
    pub crossbow_sound: bool,
    /// The bow or crossbow it was shot from (`firedFromWeapon`).
    pub weapon: Option<crate::item::ItemStack>,
    /// `Projectile.lastDeflectedBy`.
    pub last_deflected: Option<EntityId>,
}

impl ArrowData {
    /// An arrow that was just shot.
    pub fn shot(
        owner: Option<EntityId>,
        base_damage: f64,
        pickup: u8,
        random_state: i64,
        item: crate::item::ItemStack,
    ) -> ArrowData {
        ArrowData {
            owner,
            owner_uuid: None,
            base_damage,
            in_ground: false,
            life: 0,
            shake: 0,
            crit: false,
            left_owner: false,
            last_state: None,
            tick_count: 0,
            pickup,
            random_state,
            knockback: 0.0,
            fire: 0,
            touching_water: false,
            item,
            in_ground_time: 0,
            spectral_duration: 200,
            has_been_shot: false,
            pierce_level: 0,
            pierced: Vec::new(),
            pierced_killed: Vec::new(),
            crossbow_sound: false,
            weapon: None,
            last_deflected: None,
        }
    }

    /// The arrow's `potion_contents` (`Arrow.getPotionContents`).
    pub fn potion(&self) -> crate::effect::potion::PotionContents {
        crate::effect::potion::PotionContents::of_stack(&self.item)
    }

    /// `ID_EFFECT_COLOR`: the potion colour, -1 without a potion.
    pub fn color(&self) -> i32 {
        let contents = self.potion();
        if contents == crate::effect::potion::PotionContents::default() {
            -1
        } else {
            contents.color()
        }
    }
}

/// The item an arrow item is shot as (`ArrowItem.createArrow`): its entity type and the
/// pickup stack (count 1).
pub fn arrow_of(
    ammo: &crate::item::ItemStack,
) -> (vanillite_data::entity::EntityTypeId, crate::item::ItemStack) {
    let mut item = ammo.clone();
    item.count = 1;
    let kind = if crate::item::item_id(ammo).name() == "minecraft:spectral_arrow" {
        entity_ids::SPECTRAL_ARROW
    } else {
        entity_ids::ARROW
    };
    (kind, item)
}

/// `startUsingItem` with the bow: the "using item" living flag is synched.
pub fn start_using_item(t: &mut Tick<'_, '_>) {
    let m = t.mob();
    m.using_item = 0;
    m.stabbed.clear();
    t.core_mut().mark_data_dirty();
}

/// `stopUsingItem`.
pub fn stop_using_item(t: &mut Tick<'_, '_>) {
    if t.mob_ref().using_item >= 0 {
        let m = t.mob();
        m.using_item = -1;
        m.stabbed = Vec::new();
        t.core_mut().mark_data_dirty();
    }
}

/// `Projectile.getMovementToShoot` and `shoot`: the velocity and rotation of a shot.
pub fn shoot(
    random: &mut LegacyRandom,
    dir: Vec3,
    power: f32,
    uncertainty: f32,
) -> (Vec3, Rotation) {
    let spread = 0.017_227_5 * f64::from(uncertainty);
    let n = dir.normalize();
    let v = Vec3::new(
        n.x + random.triangle(0.0, spread),
        n.y + random.triangle(0.0, spread),
        n.z + random.triangle(0.0, spread),
    ) * f64::from(power);
    let sd = (v.x * v.x + v.z * v.z).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let rot = Rotation {
        y_rot: atan2_deg(v.x, v.z) as f32,
        x_rot: atan2_deg(v.y, sd) as f32,
        y_head_rot: 0.0,
    };
    (v, rot)
}

/// `BowItem.releaseUsing` / `ProjectileWeaponItem.shoot` of one arrow: spawns it at `start`
/// flying along `dir` with `power` and `uncertainty`, fired from `weapon` (Flame sets it on
/// fire; Power and Punch apply at the hit). Dispensers and other shooters use it too.
#[allow(clippy::too_many_arguments)]
pub fn shoot_arrow(
    ents: &mut Entities,
    network_ids: &vanillite_world::entity::NetworkIds,
    start: Vec3,
    dir: Vec3,
    power: f32,
    uncertainty: f32,
    owner: Option<EntityId>,
    weapon: Option<&crate::item::ItemStack>,
    ammo: &crate::item::ItemStack,
    crit: bool,
    pickup: u8,
) -> EntityId {
    // The arrow's own random (its constructor), then `shoot` draws the spread from it.
    let mut own = LegacyRandom::new(crate::mob::new_entity_seed());
    let uuid = crate::mob::new_uuid();
    let (velocity, rot) = shoot(&mut own, dir, power, uncertainty);
    let (kind, item) = arrow_of(ammo);
    let mut data = ArrowData::shot(owner, 2.0, pickup, own.state(), item);
    data.crit = crit;
    data.weapon = weapon.cloned();
    if let Some(w) = weapon {
        // Flame (`onProjectileSpawned`); Power and Punch apply at the hit.
        if super::attack::combat_data()
            .enchants
            .projectile_ignite(w)
            .is_some()
        {
            // `igniteForSeconds(100)`.
            data.fire = 2000;
        }
    }
    let mut new = NewEntity::new(kind, network_ids.next(), uuid, start);
    new.velocity = velocity;
    new.rot = rot;
    new.flags = EntityFlags::HAS_IMPULSE;
    if data.fire > 0 {
        new.flags.set(EntityFlags::ON_FIRE, true);
    }
    let id = ents.spawn_arrow(new, data);
    ents.launched(id, owner);
    id
}

/// `AbstractSkeleton.performRangedAttack`.
pub fn skeleton_shoot(t: &mut Tick<'_, '_>, target: EntityId, power: f32) {
    let Some(o) = t.other(target) else {
        return;
    };
    let p = t.pos();
    // `getEyeY() - 0.1F`.
    let start = Vec3::new(p.x, t.eye_y() - f64::from(0.1_f32), p.z);
    let difficulty = t.env.difficulty;
    let id = t.id;
    // `getItemInHand(getWeaponHoldingHand(BOW))` and `getProjectile(bow)`: an arrow held in
    // a hand (off hand first), else a plain arrow.
    let (weapon, ammo) = {
        use crate::item::EquipmentSlot;
        let m = t.mob_ref();
        let bow = vanillite_data::item::ids::BOW.0;
        let main = m.equipment.get(EquipmentSlot::MainHand);
        let off = m.equipment.get(EquipmentSlot::OffHand);
        let weapon = if !main.is_empty() && main.item == bow {
            main.clone()
        } else {
            off.clone()
        };
        let is_arrow = |s: &crate::item::ItemStack| {
            !s.is_empty()
                && crate::player::play::game_data()
                    .is_some_and(|d| d.item_in_tag(s.item, "minecraft:arrows"))
        };
        let ammo = if is_arrow(off) {
            Some(off.clone())
        } else if is_arrow(main) {
            Some(main.clone())
        } else {
            None
        };
        (weapon, ammo)
    };
    let m = t.mob();
    // The arrow's own random (its constructor); `setBaseDamageFromMob` and `shoot` draw from
    // it, not from the skeleton's.
    let mut own = LegacyRandom::new(super::new_entity_seed());
    // `getArrow` of the skeleton kinds: tipped arrows and the wither skeleton's fire.
    let (effect, fire) = super::fam::undead::skeleton_arrow(m.kind);
    let (kind, item) = match &ammo {
        None => (entity_ids::ARROW, mob_arrow(effect)),
        Some(a) => {
            let (kind, mut item) = arrow_of(a);
            // `Arrow.addEffect` onto the held arrow's potion.
            if kind == entity_ids::ARROW
                && let Some(e) = effect
            {
                let mut contents = crate::effect::potion::PotionContents::of_stack(&item);
                contents.custom_effects.push(e);
                item.set_component(
                    vanillite_protocol::components::ids::POTION_CONTENTS,
                    contents.encode(),
                );
            }
            (kind, item)
        }
    };
    // The `AbstractArrow` constructor with the weapon: its Piercing level.
    let weapon = (!weapon.is_empty()).then_some(weapon);
    let pierce = match (&weapon, crate::player::play::game_data()) {
        (Some(w), Some(d)) => d.enchants.piercing_count(w, &item, t.env.random),
        _ => 0,
    };
    #[allow(clippy::cast_precision_loss)]
    let base_damage =
        f64::from(power) * 2.0 + own.triangle(f64::from(difficulty as i32) * 0.11, 0.574_25);
    let xd = o.pos.x - p.x;
    // `target.getY(0.3333333333333333)`.
    let target_y = o.pos.y + f64::from(o.height) * 0.333_333_333_333_333_3;
    let yd = target_y - start.y;
    let zd = o.pos.z - p.z;
    let dist = (xd * xd + zd * zd).sqrt();
    #[allow(clippy::cast_precision_loss)]
    let uncertainty = (14 - difficulty as i32 * 4) as f32;
    let (velocity, rot) = shoot(
        &mut own,
        Vec3::new(xd, yd + dist * f64::from(0.2_f32), zd),
        1.6,
        uncertainty,
    );
    let uuid = crate::mob::new_uuid();
    let mut data = ArrowData::shot(Some(id), base_damage, 0, own.state(), item);
    data.crit = false;
    // `igniteForSeconds(100)` of the wither skeleton's arrow.
    data.fire = if fire { 2000 } else { 0 };
    data.pierce_level = u8::try_from(pierce.clamp(0, 127)).unwrap_or(0);
    if let Some(w) = &weapon {
        // Flame (`onProjectileSpawned`); Power and Punch apply at the hit.
        if super::attack::combat_data()
            .enchants
            .projectile_ignite(w)
            .is_some()
        {
            // `igniteForSeconds(100)`.
            data.fire = 2000;
        }
    }
    data.weapon = weapon;
    let mut new = NewEntity::new(kind, t.level.network_ids().next(), uuid, start);
    new.velocity = velocity;
    new.rot = rot;
    new.flags = EntityFlags::HAS_IMPULSE;
    if data.fire > 0 {
        new.flags.set(EntityFlags::ON_FIRE, true);
    }
    let arrow = t.ents.spawn_arrow(new, data);
    t.ents.launched(arrow, Some(id));
    let m = t.mob();
    let pitch = 1.0 / (m.random.next_float() * 0.4 + 0.8);
    super::kinds::play_sound(t, "minecraft:entity.skeleton.shoot", 1.0, pitch);
}

/// The arrow a skeleton type shoots (`AbstractSkeleton.getArrow`): `Arrow.addEffect` puts the
/// effect of strays, bogged and parched into a plain arrow's potion.
pub(crate) fn mob_arrow(effect: Option<crate::effect::EffectInstance>) -> crate::item::ItemStack {
    use crate::effect::potion::PotionContents;
    let mut item = crate::item::ItemStack::new(
        vanillite_data::registry::ITEM
            .id("minecraft:arrow")
            .unwrap_or(0),
        1,
    );
    if let Some(e) = effect {
        let contents = PotionContents {
            custom_effects: vec![e],
            ..PotionContents::default()
        };
        item.set_component(
            vanillite_protocol::components::ids::POTION_CONTENTS,
            contents.encode(),
        );
    }
    item
}

/// `Entity.canBeHitByProjectile` (alive and `isPickable`): living entities but marker armor
/// stands and spectators, end crystals, shulker bullets, the `#redirectable_projectile`s,
/// item frames, paintings and cushions, boats, minecarts, primed TNT and falling blocks.
pub fn can_be_hit_by_projectile(ents: &Entities, id: EntityId) -> bool {
    use super::missile::MissileKind;
    if let Some(p) = ents.player(id) {
        return !p.survival.is_dead_or_dying()
            && p.game_mode != vanillite_config::GameMode::Spectator;
    }
    if let Some(m) = ents.mob(id) {
        return m.is_alive()
            && !(m.kind == super::Kind::ArmorStand && crate::decoration::stand::is_marker(m));
    }
    if let Some(d) = ents.missile(id) {
        return !d.removed
            && matches!(
                d.kind,
                MissileKind::Fireball
                    | MissileKind::WindCharge
                    | MissileKind::BreezeWindCharge
                    | MissileKind::ShulkerBullet
            );
    }
    if let Some(s) = ents.special(id) {
        return matches!(s, super::boss::special::SpecialData::EndCrystal(c) if !c.removed);
    }
    if let Some(d) = ents.decor(id) {
        use crate::decoration::DecorData;
        return matches!(
            d.data,
            DecorData::Frame(_) | DecorData::Painting(_) | DecorData::Cushion(_)
        );
    }
    ents.store.get(id).is_some_and(|e| {
        let kind = e.kind();
        crate::vehicle::BoatKind::of(kind).is_some()
            || crate::minecart::MinecartKind::of(kind).is_some()
            || kind == entity_ids::TNT
            || kind == entity_ids::FALLING_BLOCK
    })
}

/// `AbstractArrow.tick` (with `Projectile.tick`'s owner check) and `Arrow.tick`.
pub fn tick_arrow(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) {
    let Some(mut a) = ents.arrow(id).cloned() else {
        return;
    };
    // `getOwner`: the owner by its UUID.
    if a.owner_uuid.is_none() {
        a.owner_uuid = a.owner.and_then(|o| ents.store.get(o)).map(|e| e.uuid());
    }
    a.owner = super::missile::resolve_owner(ents, a.owner, a.owner_uuid);
    let Some(e) = ents.store.get(id) else {
        return;
    };
    // The synched values (`ID_FLAGS`, `PIERCE_LEVEL`, `IN_GROUND`): only a change of one marks
    // the data dirty. The client restarts its shake on every `IN_GROUND` it receives.
    let synched = (a.crit, a.pierce_level, a.in_ground);
    let mut color_changed = false;
    let mut pos = e.pos();
    let start = pos;
    let start_box = e.aabb();
    let mut velocity = e.velocity();
    let mut rot = e.rot();
    let (kind, network_id) = (e.kind(), e.network_id().0);
    let mut random = LegacyRandom::from_state(a.random_state);
    // `Projectile.tick`: the first tick posts `PROJECTILE_SHOOT` from the shooter.
    if !std::mem::replace(&mut a.has_been_shot, true) {
        super::missile::shoot_event(env.out, pos, a.owner);
    }
    a.tick_count += 1;
    let block_pos = BlockPos::containing(pos.x, pos.y, pos.z);
    let state = level.block(block_pos);
    if !state.is_air() {
        for b in state.collision_shape() {
            let shape = Aabb::new(
                f64::from(block_pos.x) + b[0],
                f64::from(block_pos.y) + b[1],
                f64::from(block_pos.z) + b[2],
                f64::from(block_pos.x) + b[3],
                f64::from(block_pos.y) + b[4],
                f64::from(block_pos.z) + b[5],
            );
            if shape.contains(pos.x, pos.y, pos.z) {
                velocity = Vec3::ZERO;
                a.in_ground = true;
                break;
            }
        }
    }
    if a.shake > 0 {
        a.shake -= 1;
    }
    // `isInWaterOrRain`: `clearFire`.
    if a.fire > 0 && (a.touching_water || in_rain(level, pos)) {
        a.fire = 0;
    }
    let mut discard = false;
    if a.in_ground {
        let falls = a.last_state != Some(state) && {
            let probe =
                Aabb::new(pos.x, pos.y, pos.z, pos.x, pos.y, pos.z).inflate(0.06, 0.06, 0.06);
            physics::no_collision(&level.view(), &probe, &mut cx.boxes)
        };
        if falls {
            a.in_ground = false;
            velocity = velocity.multiply(
                f64::from(random.next_float() * 0.2),
                f64::from(random.next_float() * 0.2),
                f64::from(random.next_float() * 0.2),
            );
            a.life = 0;
        } else {
            a.life += 1;
            if a.life >= cx.settings.arrow_despawn_ticks {
                discard = true;
            }
        }
        a.in_ground_time += 1;
        if !discard {
            inside_blocks(level, cx, id, start_box, start, pos);
            fire_effects(level, &mut a, &mut random, env, start_box, start, pos);
        }
    } else {
        a.in_ground_time = 0;
        // The step and the rotation use the motion from the start of the tick.
        let movement = velocity;
        let in_water = a.touching_water;
        if in_water {
            velocity = velocity * f64::from(0.6_f32);
        }
        #[allow(clippy::cast_possible_truncation)]
        let y_rot = atan2_deg(movement.x, movement.z) as f32;
        let hd = (movement.x * movement.x + movement.z * movement.z).sqrt();
        #[allow(clippy::cast_possible_truncation)]
        let x_rot = atan2_deg(movement.y, hd) as f32;
        rot.x_rot = lerp_rotation(rot.x_rot, x_rot);
        rot.y_rot = lerp_rotation(rot.y_rot, y_rot);
        // `checkLeftOwner`.
        if !a.left_owner {
            let aabb = e
                .aabb()
                .expand_towards(velocity.x, velocity.y, velocity.z)
                .inflate(1.0, 1.0, 1.0);
            let still_inside = a.owner.is_some_and(|o| {
                ents.store
                    .get(o)
                    .is_some_and(|oe| oe.aabb().intersects(&aabb))
            });
            a.left_owner = !still_inside;
        }
        let to = pos + movement;
        let block_hit = physics::clip(level as &dyn Blocks, pos, to);
        let end = block_hit.map_or(to, |(_, p)| p);
        // `findHitEntities`: the hit entity nearest to the arrow (`computeMargin`).
        #[allow(clippy::cast_precision_loss)]
        let margin = ((a.tick_count - 2) as f32 / 20.0).clamp(0.0, 0.3);
        let area = e
            .aabb()
            .expand_towards(velocity.x, velocity.y, velocity.z)
            .inflate(1.0, 1.0, 1.0);
        let mut hits: Vec<(EntityId, f64, Vec3, Option<usize>)> = Vec::new();
        let mut dragons: Vec<EntityId> = Vec::new();
        ents.store
            .for_each_in(&area.inflate(12.0, 12.0, 12.0), false, |other| {
                if ents
                    .mob(other)
                    .is_some_and(|m| m.kind == super::Kind::EnderDragon)
                {
                    dragons.push(other);
                }
            });
        ents.store.for_each_in(&area, false, |other| {
            if other == id || a.pierced.contains(&other) {
                return;
            }
            // `EnderDragon.isPickable` is false: arrows hit its parts.
            if dragons.contains(&other) {
                return;
            }
            if !a.left_owner && a.owner == Some(other) {
                return;
            }
            if !can_be_hit_by_projectile(ents, other) {
                return;
            }
            // `AbstractArrow.canHitEntity`: a player's arrow flies through the players it may
            // not hurt (`canHarmPlayer`).
            if ents.player(other).is_some()
                && let Some(owner) = a.owner.filter(|&o| ents.player(o).is_some())
                && !super::attack::can_harm_player(ents, env, owner, other)
            {
                return;
            }
            let Some(oe) = ents.store.get(other) else {
                return;
            };
            // `ProjectileUtil.getManyEntityHitResult`: the exact box, else the box grown by
            // the margin when the way from there to its centre is free.
            let bb = oe.aabb();
            let hit = aabb_clip(&bb, pos, end).or_else(|| {
                if margin <= 0.0 {
                    return None;
                }
                let m = f64::from(margin);
                let outside = aabb_clip(&bb.inflate(m, m, m), pos, end)?;
                let towards = physics::clip(level as &dyn Blocks, outside, bb.center())
                    .map_or(bb.center(), |(_, p)| p);
                aabb_clip(&bb, outside, towards).map(|_| outside)
            });
            if let Some(hit) = hit {
                hits.push((other, pos.distance_to_sqr(oe.pos()), hit, None));
            }
        });
        // `Level.getEntities` adds the dragon parts (`EnderDragonPart`) in the area last.
        for &dragon in &dragons {
            if a.pierced.contains(&dragon)
                || !crate::effect::thrown::can_be_hit(ents, dragon)
                || (!a.left_owner && a.owner == Some(dragon))
            {
                continue;
            }
            let Some(s) = ents.mob(dragon).and_then(|m| m.boss.dragon()) else {
                continue;
            };
            for i in 0..super::boss::dragon::PARTS.len() {
                let bb = s.part_box(i);
                if !bb.intersects(&area) {
                    continue;
                }
                let hit = aabb_clip(&bb, pos, end).or_else(|| {
                    if margin <= 0.0 {
                        return None;
                    }
                    let m = f64::from(margin);
                    let outside = aabb_clip(&bb.inflate(m, m, m), pos, end)?;
                    let towards = physics::clip(level as &dyn Blocks, outside, bb.center())
                        .map_or(bb.center(), |(_, p)| p);
                    aabb_clip(&bb, outside, towards).map(|_| outside)
                });
                if let Some(hit) = hit {
                    hits.push((dragon, pos.distance_to_sqr(s.parts[i]), hit, Some(i)));
                }
            }
        }
        // `stepMoveAndHit`: the entities on the way, nearest first, are hit one after the
        // other until one stops the arrow; a piercing arrow then flies on.
        hits.sort_by(|x, y| x.1.total_cmp(&y.1));
        let mut deflected = false;
        if let Some(&(_, _, first, _)) = hits.first() {
            pos = first;
        }
        // Whether an entity was hit: Bukkit's `ProjectileHitEvent` may make the arrow ignore
        // every one on its way.
        let mut hit_any = false;
        for &(target, _, hit, part) in &hits {
            if !super::missile::hit_event(
                level,
                ents,
                id,
                a.owner,
                super::missile::HitWhat::Entity(target),
            ) {
                continue;
            }
            hit_any = true;
            // `hitTargetOrDeflectSelf`: a breeze turns the arrow back, once in a row.
            if part.is_none() && super::missile::deflects(ents, target, kind) {
                if a.last_deflected != Some(target) {
                    (velocity, rot) =
                        super::missile::reverse(ents, env, target, velocity, rot, &mut random);
                    a.last_deflected = Some(target);
                }
                deflected = true;
                break;
            }
            // `Projectile.onHit`: a `#redirectable_projectile` it hits flies off along the
            // owner's look (`onRedirectProjectile`).
            super::attack::aim_deflect(ents, target, a.owner, a.owner);
            // `onHit` posts `PROJECTILE_LAND` at the hit location after `onHitEntity`.
            super::missile::land_event(env.out, level, id, a.owner, None, hit);
            // `onHitEntity`: the weapon's `modifyDamage` (Power) against this target.
            let pow = velocity.length_sqr().sqrt();
            let owner_for_source = a.owner.filter(|o| ents.store.contains(*o)).unwrap_or(id);
            let (arrow_damage, knockback) = match &a.weapon {
                Some(w) => {
                    weapon_hit_values(ents, env, w, kind, owner_for_source, target, a.base_damage)
                }
                None => (a.base_damage, a.knockback),
            };
            #[allow(clippy::cast_possible_truncation)]
            let mut damage = ceil((pow * arrow_damage).clamp(0.0, 2.147_483_647e9));
            if a.pierce_level > 0 {
                if a.pierced.len() > usize::from(a.pierce_level) {
                    discard = true;
                    break;
                }
                a.pierced.push(target);
            }
            if a.crit {
                let inc = random.next_int_bounded(damage / 2 + 2);
                damage = damage.saturating_add(inc);
            }
            let owner = a.owner.filter(|o| ents.store.contains(*o)).unwrap_or(id);
            let mut source = combat::entity_source(ents, env.types.arrow, owner, id);
            source.position = Some(pos);
            source.projectile_motion = Some(velocity);
            // A dragon part passes the hit on (`EnderDragonPart.hurtServer`).
            if let Some(i) = part
                && let Some(s) = ents.mob_mut(target).and_then(|m| m.boss.dragon_mut())
            {
                s.hit_part = Some(i);
            }
            // `owner.setLastHurtMob(entity)`: mobs and players (their wolves join in).
            if let Some(owner) = a.owner {
                if let Some(m) = ents.mob_mut(owner) {
                    m.last_hurt_mob = Some(target);
                } else if let Some(p) = ents.player_mut(owner) {
                    p.survival.last_hurt_mob = Some((target, p.survival.tick_count));
                }
            }
            // A burning arrow sets the target on fire for 5 seconds before the hurt.
            let fire_before = fire_ticks(ents, target);
            if a.fire > 0 {
                super::attack::ignite(ents, target, 100);
            }
            #[allow(clippy::cast_precision_loss)]
            let hurt = combat::hurt(level, ents, env, cx, target, source, damage as f32);
            // A perched dragon sets the arrow burning (`AbstractDragonSittingPhase.onHurt`).
            if let Some(d) = ents.arrow(id) {
                a.fire = a.fire.max(d.fire);
            }
            let living = part.is_none() && crate::effect::thrown::is_living(ents, target);
            if hurt && !living {
                // A vehicle, crystal or decoration: only the hit sound (`onHitEntity`).
                let pitch = 1.2 / (random.next_float() * 0.2 + 0.9);
                let seed = crate::survival::sound_seed();
                play_sound(
                    env.out,
                    None,
                    pos,
                    hit_sound(&a),
                    SoundSource::Neutral,
                    1.0,
                    pitch,
                    seed,
                );
                if a.pierce_level == 0 {
                    discard = true;
                    break;
                }
            } else if hurt {
                // `setArrowCount(+1)` of a non-piercing hit: the arrows stuck in the body.
                if a.pierce_level == 0 {
                    add_stuck_arrow(ents, target);
                }
                // `doKnockback`: Punch pushes along the arrow's flight.
                if knockback > 0.0 {
                    let resist = super::attack::knockback_resistance(ents, target);
                    let flat = Vec3::new(velocity.x, 0.0, velocity.z);
                    if flat.length_sqr() > 0.0 {
                        let push = flat.normalize()
                            * (f64::from(knockback) * 0.6 * (1.0 - resist).max(0.0));
                        if let Some(mut e) = ents.store.get_mut(target) {
                            let v = e.as_ref().velocity();
                            e.set_velocity(v + Vec3::new(push.x, 0.1, push.z));
                            e.set_flags(EntityFlags::HAS_IMPULSE, true);
                            e.set_flags(EntityFlags::HURT_MARKED, true);
                        }
                    }
                }
                // `doPostAttackEffectsWithItemSource` (the victim's equipment), then
                // `doPostHurtEffects`: the potion of an arrow, the glowing of a spectral one.
                super::attack::post_attack(level, ents, env, cx, target, &source, None);
                if kind == entity_ids::SPECTRAL_ARROW {
                    let glowing = crate::effect::EffectInstance::new(
                        crate::effect::MobEffect::GLOWING,
                        a.spectral_duration,
                        0,
                    );
                    // `getEffectSource()`: the owner, else the arrow.
                    crate::effect::living::add_to_entity_from(
                        level,
                        ents,
                        env,
                        cx,
                        target,
                        glowing,
                        Some(owner),
                    );
                } else {
                    crate::effect::cloud::arrow_hit(level, ents, env, cx, target, &a.item, owner);
                }
                // A piercing arrow keeps what it killed (`piercedAndKilledEntities`); a player's
                // arrow fires `KILLED_BY_ARROW` with that list on every hit, else with what it
                // hit if that died.
                let dead = ents.mob(target).map_or_else(
                    || {
                        ents.player(target)
                            .is_some_and(|p| p.survival.is_dead_or_dying())
                    },
                    |m| !m.is_alive(),
                );
                let victim = ents
                    .store
                    .get(target)
                    .map(|e| (target, e.kind().name(), e.pos()));
                if dead
                    && a.pierce_level > 0
                    && let Some(v) = victim
                {
                    a.pierced_killed.push(v);
                }
                let victims = if a.pierce_level > 0 {
                    Some(a.pierced_killed.clone())
                } else {
                    victim.filter(|_| dead).map(|v| vec![v])
                };
                if let Some(player) = a.owner.filter(|&o| ents.player(o).is_some())
                    && let Some(victims) = victims
                {
                    use crate::advancements::{predicate::EntityView, trigger, view};
                    view::fire(
                        ents,
                        player,
                        trigger::TriggerKind::KILLED_BY_ARROW,
                        |ents| {
                            let mut types: Vec<&str> = victims.iter().map(|v| v.1).collect();
                            types.sort_unstable();
                            types.dedup();
                            let views = victims
                                .iter()
                                .map(|&(id, kind, pos)| {
                                    view::snapshot(ents, id)
                                        .map_or_else(|| EntityView::new(kind, pos), |v| *v)
                                })
                                .collect();
                            trigger::Event::KilledByArrow(
                                views,
                                i32::try_from(types.len()).unwrap_or(i32::MAX),
                                a.weapon.clone(),
                            )
                        },
                    );
                }
                // `PLAY_ARROW_HIT_SOUND` to a player who shot another player.
                if let Some(owner) = a.owner
                    && owner != target
                    && ents.player(target).is_some()
                    && ents.player(owner).is_some()
                {
                    env.out.push(crate::survival::Effect::To {
                        player: owner,
                        packet: vanillite_protocol::SharedPacket::of(
                            &vanillite_protocol::packets::play::clientbound::GameEvent {
                                event: 6,
                                param: 0.0,
                            },
                        ),
                    });
                }
                let pitch = 1.2 / (random.next_float() * 0.2 + 0.9);
                let seed = crate::survival::sound_seed();
                play_sound(
                    env.out,
                    None,
                    pos,
                    hit_sound(&a),
                    SoundSource::Neutral,
                    1.0,
                    pitch,
                    seed,
                );
                if a.pierce_level == 0 {
                    discard = true;
                    break;
                }
            } else if !ents
                .mob(target)
                .is_some_and(|m| m.kind == super::Kind::Enderman)
            {
                // `projectileReceivesSideEffectsOnHit(false)` (an enderman's is false: the arrow
                // flies on): the target's fire is put back. Deflected (`deflect(ProjectileDeflection.REVERSE, ..., 0.2)`): back
                // at half the power, turned by 170° to 190°.
                if let Some(before) = fire_before {
                    if let Some(m) = ents.mob_mut(target) {
                        m.fire_ticks = before;
                    } else if let Some(p) = ents.player_mut(target) {
                        p.survival.fire_ticks = before;
                    }
                }
                let turn = 170.0 + random.next_float() * 20.0;
                velocity = velocity * -0.1;
                rot.y_rot += turn;
                if velocity.length_sqr() < 1.0e-7 {
                    // Stopped: it drops as an item if it may be picked up
                    // (`spawnAtLocation(pickupItem, 0.1F)`).
                    if a.pickup == 1 {
                        crate::decoration::spawn_item(
                            env,
                            Vec3::new(pos.x, pos.y + f64::from(0.1_f32), pos.z),
                            a.item.clone(),
                        );
                    }
                    discard = true;
                }
                deflected = true;
                break;
            }
        }
        // A piercing arrow that went through everything on its way: no entity it has not
        // pierced is left, so it goes on to the block or the end of its step.
        let flies_on = !hit_any || (!discard && !deflected && a.pierce_level > 0);
        let block_hit = block_hit.filter(|&(hit_pos, at)| {
            !flies_on
                || super::missile::hit_event(
                    level,
                    ents,
                    id,
                    a.owner,
                    super::missile::HitWhat::Block(hit_pos, at),
                )
        });
        if flies_on && let Some((hit_pos, at)) = block_hit {
            // `onHitBlock`.
            let sign = |v: f64| {
                if v > 0.0 {
                    1.0
                } else if v < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            };
            let offset = Vec3::new(sign(velocity.x), sign(velocity.y), sign(velocity.z))
                * f64::from(0.05_f32);
            pos = at - offset;
            velocity = Vec3::ZERO;
            a.last_state = Some(level.block(hit_pos));
            super::missile::hit_block(level, cx, id, hit_pos, at);
            let pitch = 1.2 / (random.next_float() * 0.2 + 0.9);
            let seed = crate::survival::sound_seed();
            play_sound(
                env.out,
                None,
                pos,
                hit_sound(&a),
                SoundSource::Neutral,
                1.0,
                pitch,
                seed,
            );
            super::missile::land_event(env.out, level, id, a.owner, Some(hit_pos), at);
            a.in_ground = true;
            a.shake = 7;
            a.crit = false;
            a.pierce_level = 0;
            a.crossbow_sound = false;
            // `resetPiercedEntities`.
            a.pierced.clear();
            a.pierced_killed.clear();
        } else if flies_on {
            pos = to;
        }
        inside_blocks(level, cx, id, start_box, start, pos);
        fire_effects(level, &mut a, &mut random, env, start_box, start, pos);
        if !in_water {
            // `applyInertia(0.99F)`.
            velocity = velocity * f64::from(0.99_f32);
        }
        if !a.in_ground {
            // `applyGravity`: `movement.add(0, -gravity, 0)` (turns -0.0 into 0.0).
            velocity = velocity + Vec3::new(0.0, -0.05, 0.0);
        }
        // `super.tick()` → `baseTick`: the water contact (`updateFluidInteraction`), then the
        // fire burns down while the arrow flies.
        let moved_box = start_box.offset(pos.x - start.x, pos.y - start.y, pos.z - start.z);
        a.touching_water = crate::survival::blocks::fluid_contact(
            &level.view(),
            crate::survival::blocks::fluid_facts(),
            moved_box,
            pos,
        )
        .in_water();
        if a.fire > 0 {
            a.fire -= 1;
        }
    }
    a.random_state = random.state();
    // `Arrow.tick`: a tipped arrow in the ground for 600 ticks loses its potion.
    if kind == entity_ids::ARROW
        && a.in_ground
        && a.in_ground_time >= 600
        && a.potion() != crate::effect::potion::PotionContents::default()
    {
        env.out.push(crate::survival::Effect::Tracking {
            entity: id,
            with_self: false,
            packet: vanillite_protocol::SharedPacket::of(
                &vanillite_protocol::packets::play::clientbound::EntityEvent {
                    entity_id: network_id,
                    event: 0,
                },
            ),
        });
        a.item = crate::item::ItemStack::new(
            vanillite_data::registry::ITEM
                .id("minecraft:arrow")
                .unwrap_or(0),
            1,
        );
        color_changed = true;
    }
    if discard || !ents.store.contains(id) {
        cx.discarded.push(id);
        return;
    }
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_pos(pos);
        e.set_velocity(velocity);
        e.set_rot(rot);
        // `setSharedFlagOnFire`.
        e.set_flags(EntityFlags::ON_FIRE, a.fire > 0);
        if color_changed || synched != (a.crit, a.pierce_level, a.in_ground) {
            e.mark_data_dirty();
        }
    }
    if let Some(d) = ents.arrow_mut(id) {
        *d = a;
    }
}

/// `DATA_ARROW_COUNT_ID` and `removeArrowTime`: the arrows stuck in a living entity's body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StuckArrows {
    pub count: u16,
    /// At most `20 * 30` ticks.
    pub remove_time: i16,
}

impl StuckArrows {
    /// The countdown of `LivingEntity.tick`: one arrow falls out after `20 * (30 - count)`
    /// ticks. Returns whether the count changed.
    pub fn tick(&mut self) -> bool {
        if self.count == 0 {
            return false;
        }
        if self.remove_time <= 0 {
            self.remove_time = 20 * (30 - i16::try_from(self.count).unwrap_or(i16::MAX).min(30));
        }
        self.remove_time -= 1;
        if self.remove_time <= 0 {
            self.count -= 1;
            return true;
        }
        false
    }
}

/// `setArrowCount(getArrowCount() + 1)` of a living entity.
fn add_stuck_arrow(ents: &mut Entities, id: EntityId) {
    if ents.mob(id).is_some() {
        let a = ents.stuck_arrows.entry(id).or_default();
        a.count = a.count.saturating_add(1);
    } else if let Some(p) = ents.player_mut(id) {
        p.survival.arrows.count = p.survival.arrows.count.saturating_add(1);
    } else {
        return;
    }
    if let Some(mut e) = ents.store.get_mut(id) {
        e.mark_data_dirty();
    }
}

/// `getRemainingFireTicks` of a living entity.
fn fire_ticks(ents: &Entities, id: EntityId) -> Option<i32> {
    ents.mob(id)
        .map(|m| m.fire_ticks)
        .or_else(|| ents.player(id).map(|p| p.survival.fire_ticks))
}

/// `Entity.applyEffectsFromBlocks(from, to)` of a projectile whose box was `start_box` at
/// `from`: `entityInside` (buttons, pressure plates, tripwires) of the blocks the box passes
/// on its way ([`crate::survival::inside`]), as world actions.
pub fn inside_blocks(
    level: &Level,
    cx: &mut MobContext,
    id: EntityId,
    start_box: Aabb,
    from: Vec3,
    to: Vec3,
) {
    #[allow(clippy::cast_possible_truncation)]
    let dims = level.entities.store.get(id).map_or(
        vanillite_world::entity::Dimensions {
            width: (start_box.max_x - start_box.min_x) as f32,
            height: (start_box.max_y - start_box.min_y) as f32,
        },
        |e| e.dims(),
    );
    let way = [crate::survival::inside::Movement::plain(from, to)];
    crate::block::plate::for_each_inside(level, dims, &way, |pos| {
        cx.actions.push(super::WorldAction::EntityInside(pos, id));
    });
}

/// How a projectile takes a bubble column's drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BubbleDrag {
    /// `Projectile.onAbove/InsideBubbleColumn`: the drag adds to the motion, uncapped.
    Projectile,
    /// `Entity.handleOnAbove/InsideBubbleColumn` (ender pearls): capped like any entity's.
    Entity,
}

/// `BubbleColumnBlock.entityInside` (only `isPrecise`) of the bubble columns a throwable
/// passes: on the way `from` to where it is now (`applyEffectsFromBlocks`), or, with `from` =
/// `None`, those in its box (`ThrowableProjectile.handleFirstTickBubbleColumn`). Each pushes the
/// motion up or down, more on the column's top (`onAboveBubbleColumn`, with splash and bubble
/// particles drawn from the level's `random`) than inside it (`onInsideBubbleColumn`).
pub fn bubble_columns(
    level: &Level,
    ents: &mut Entities,
    random: &mut LegacyRandom,
    cx: &mut MobContext,
    id: EntityId,
    from: Option<Vec3>,
    drag: BubbleDrag,
) {
    use vanillite_base::math::floor;
    use vanillite_data::block::ids::BUBBLE_COLUMN;
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let (pos, dims, aabb) = (e.pos(), e.dims(), e.aabb());
    let mut columns: Vec<(BlockPos, bool)> = Vec::new();
    let drag_down = |state: BlockState| state.value("drag") == Some("true");
    match from {
        Some(from) => {
            let way = [crate::survival::inside::Movement::plain(from, pos)];
            let view = level.view();
            let walker = crate::survival::inside::Walker {
                blocks: &view,
                facts: None,
                dims,
                fast_fall: false,
            };
            walker.walk(&way, |cell| {
                if cell.in_block && cell.precise && cell.state.block() == BUBBLE_COLUMN {
                    columns.push((cell.pos, drag_down(cell.state)));
                }
                true
            });
        }
        None => {
            // `findBlocksIn(getBoundingBox())`: Y, then Z, then X.
            for y in floor(aabb.min_y)..=floor(aabb.max_y) {
                for z in floor(aabb.min_z)..=floor(aabb.max_z) {
                    for x in floor(aabb.min_x)..=floor(aabb.max_x) {
                        let at = BlockPos::new(x, y, z);
                        let state = level.block(at);
                        if state.block() == BUBBLE_COLUMN {
                            columns.push((at, drag_down(state)));
                        }
                    }
                }
            }
        }
    }
    for (at, down) in columns {
        let above = level.block(BlockPos::new(at.x, at.y + 1, at.z));
        let nothing_above = above.collision_shape().is_empty() && above.fluid_state().is_empty();
        let Some(mut e) = ents.store.get_mut(id) else {
            return;
        };
        let v = e.as_ref().velocity();
        let y = match (drag, nothing_above, down) {
            (BubbleDrag::Projectile, _, true) => v.y - 0.03,
            (BubbleDrag::Projectile, true, false) => v.y + 0.1,
            (BubbleDrag::Projectile, false, false) => v.y + 0.06,
            (BubbleDrag::Entity, true, true) => (v.y - 0.03).max(-0.9),
            (BubbleDrag::Entity, true, false) => (v.y + 0.1).min(1.8),
            (BubbleDrag::Entity, false, true) => (v.y - 0.03).max(-0.3),
            (BubbleDrag::Entity, false, false) => (v.y + 0.06).min(0.7),
        };
        e.set_velocity(Vec3::new(v.x, y, v.z));
        if nothing_above {
            bubble_column_particles(random, cx, at);
        }
    }
}

/// `Entity.sendBubbleColumnParticles`: splashes and bubbles at random spots on the column's
/// top, drawn from the level's random.
fn bubble_column_particles(random: &mut LegacyRandom, cx: &mut MobContext, pos: BlockPos) {
    use crate::level::particles::simple;
    let (x, y, z) = (f64::from(pos.x), f64::from(pos.y + 1), f64::from(pos.z));
    for _ in 0..2 {
        let at = Vec3::new(x + random.next_double(), y, z + random.next_double());
        cx.send_particles(simple("minecraft:splash"), at, 1, Vec3::ZERO, 1.0);
        let at = Vec3::new(x + random.next_double(), y, z + random.next_double());
        cx.send_particles(
            simple("minecraft:bubble"),
            at,
            1,
            Vec3::new(0.0, 0.01, 0.0),
            0.2,
        );
    }
}

/// `soundEvent`: what the arrow sounds like when it hits.
/// `isInRain` of an arrow at `pos` (its box is 0.5 high).
fn in_rain(level: &Level, pos: Vec3) -> bool {
    let feet = BlockPos::containing(pos.x, pos.y, pos.z);
    level.is_raining_at(feet)
        || level.is_raining_at(BlockPos::new(
            feet.x,
            vanillite_base::math::floor(pos.y + 0.5),
            feet.z,
        ))
}

/// The fire part of an arrow's `applyEffectsFromBlocks` on the way `from` → `to`: fire blocks
/// (`BaseFireBlock.fireIgnite`) and lava (`lavaIgnite`) set it burning, water and powder snow
/// (`EXTINGUISH`) and rain put it out with the extinguish sound (from the arrow's random).
fn fire_effects(
    level: &Level,
    a: &mut ArrowData,
    random: &mut LegacyRandom,
    env: &mut Env<'_>,
    start_box: Aabb,
    from: Vec3,
    to: Vec3,
) {
    let facts = crate::survival::blocks::fluid_facts();
    #[allow(clippy::cast_possible_truncation)]
    let dims = vanillite_world::entity::Dimensions {
        width: (start_box.max_x - start_box.min_x) as f32,
        height: (start_box.max_y - start_box.min_y) as f32,
    };
    let way = [crate::survival::inside::Movement::plain(from, to)];
    let inside =
        crate::survival::blocks::inside_blocks(&level.view(), facts, dims, false, &way, |_| {});
    if !inside.burns() && a.fire == 0 {
        return;
    }
    let previous = a.fire;
    for step in inside.steps() {
        if step.fire.is_some() {
            if a.fire < 0 {
                a.fire += 1;
            }
            if a.fire >= 0 {
                a.fire = a.fire.max(160);
            }
        }
        if step.lava_blocks > 0 {
            a.fire = a.fire.max(300);
        }
        if step.extinguish {
            a.fire = a.fire.min(0);
        }
    }
    if a.fire > 0 && in_rain(level, to) {
        a.fire = 0;
    }
    if previous > 0 && a.fire <= 0 {
        // `playEntityOnFireExtinguishedSound`.
        let pitch = 1.6 + (random.next_float() - random.next_float()) * 0.4;
        play_sound(
            env.out,
            None,
            to,
            "minecraft:entity.generic.extinguish_fire",
            SoundSource::Neutral,
            0.7,
            pitch,
            crate::survival::sound_seed(),
        );
    }
    if a.fire <= 0 && a.fire <= previous {
        a.fire = 0;
    }
}

/// `instanceof AbstractArrow || instanceof WindCharge`: arrows, spectral arrows, tridents and
/// the players' wind charges (not the breeze's).
pub fn is_arrow_or_wind_charge(ents: &Entities, id: EntityId) -> bool {
    ents.store.get(id).is_some_and(|e| {
        let k = e.kind();
        is_arrow_kind(k) || k == entity_ids::TRIDENT || k == entity_ids::WIND_CHARGE
    })
}

fn hit_sound(a: &ArrowData) -> &'static str {
    if a.crossbow_sound {
        "minecraft:item.crossbow.hit"
    } else {
        "minecraft:entity.arrow.hit"
    }
}

/// `Projectile.lerpRotation`.
pub fn lerp_rotation(old: f32, new: f32) -> f32 {
    let mut old = old;
    while new - old < -180.0 {
        old -= 360.0;
    }
    while new - old >= 180.0 {
        old += 360.0;
    }
    old + (new - old) * 0.2
}

/// Where the segment enters `b`, as a fraction of the segment (`AABB.clip`: nothing when it
/// starts inside).
pub fn segment_box(from: Vec3, to: Vec3, b: &Aabb) -> Option<f64> {
    aabb_clip_scale(b, from, to)
}

/// `AABB.clip(from, to)`: where the way from `from` to `to` enters the box; nothing when it
/// starts inside.
pub fn aabb_clip(b: &Aabb, from: Vec3, to: Vec3) -> Option<Vec3> {
    let (dx, dy, dz) = (to.x - from.x, to.y - from.y, to.z - from.z);
    aabb_clip_scale(b, from, to).map(|scale| {
        Vec3::new(
            from.x + scale * dx,
            from.y + scale * dy,
            from.z + scale * dz,
        )
    })
}

/// [`aabb_clip`] as the fraction of the way.
fn aabb_clip_scale(b: &Aabb, from: Vec3, to: Vec3) -> Option<f64> {
    let (dx, dy, dz) = (to.x - from.x, to.y - from.y, to.z - from.z);
    let mut scale = 1.0;
    let mut hit = false;
    #[allow(clippy::too_many_arguments)]
    let mut clip_point = |da: f64,
                          db: f64,
                          dc: f64,
                          point: f64,
                          (min_b, max_b): (f64, f64),
                          (min_c, max_c): (f64, f64),
                          (fa, fb, fc): (f64, f64, f64)| {
        let s = (point - fa) / da;
        let pb = fb + s * db;
        let pc = fc + s * dc;
        if 0.0 < s
            && s < scale
            && min_b - 1.0e-7 < pb
            && pb < max_b + 1.0e-7
            && min_c - 1.0e-7 < pc
            && pc < max_c + 1.0e-7
        {
            scale = s;
            hit = true;
        }
    };
    let yz = (from.x, from.y, from.z);
    if dx > 1.0e-7 {
        clip_point(
            dx,
            dy,
            dz,
            b.min_x,
            (b.min_y, b.max_y),
            (b.min_z, b.max_z),
            yz,
        );
    } else if dx < -1.0e-7 {
        clip_point(
            dx,
            dy,
            dz,
            b.max_x,
            (b.min_y, b.max_y),
            (b.min_z, b.max_z),
            yz,
        );
    }
    let zx = (from.y, from.z, from.x);
    if dy > 1.0e-7 {
        clip_point(
            dy,
            dz,
            dx,
            b.min_y,
            (b.min_z, b.max_z),
            (b.min_x, b.max_x),
            zx,
        );
    } else if dy < -1.0e-7 {
        clip_point(
            dy,
            dz,
            dx,
            b.max_y,
            (b.min_z, b.max_z),
            (b.min_x, b.max_x),
            zx,
        );
    }
    let xy = (from.z, from.x, from.y);
    if dz > 1.0e-7 {
        clip_point(
            dz,
            dx,
            dy,
            b.min_z,
            (b.min_x, b.max_x),
            (b.min_y, b.max_y),
            xy,
        );
    } else if dz < -1.0e-7 {
        clip_point(
            dz,
            dx,
            dy,
            b.max_z,
            (b.min_x, b.max_x),
            (b.min_y, b.max_y),
            xy,
        );
    }
    hit.then_some(scale)
}

/// `AbstractArrow.playerTouch` and `tryPickup` of arrows and tridents: a player takes a
/// projectile in the ground (a returning trident too) once it no longer shakes. Returns whether
/// it was taken; the caller sends `take` and removes it.
pub fn try_pickup(
    ents: &mut crate::entities::Entities,
    projectile: EntityId,
    player: &mut crate::player::PlayerData,
    player_id: EntityId,
) -> bool {
    if ents.missile(projectile).is_some() {
        return super::fam::undead::trident::try_pickup(ents, projectile, player, player_id);
    }
    let Some(a) = ents.arrow(projectile) else {
        return false;
    };
    if !a.in_ground || a.shake > 0 {
        return false;
    }
    let infinite = player.game_mode == vanillite_config::GameMode::Creative;
    match a.pickup {
        1 => {
            let mut item = a.item.clone();
            player.inventory.add(None, &mut item, infinite)
        }
        2 => infinite,
        _ => false,
    }
}

/// `AbstractArrow.onHitEntity`'s `modifyDamage(weapon, target, source, baseDamage)` (Power)
/// and `doKnockback`'s `modifyKnockback(weapon, target, source, 0)` (Punch) of an arrow shot
/// from `weapon`.
fn weapon_hit_values(
    ents: &crate::entities::Entities,
    env: &mut crate::survival::player::Env<'_>,
    weapon: &crate::item::ItemStack,
    kind: vanillite_data::entity::EntityTypeId,
    owner: EntityId,
    target: EntityId,
    base_damage: f64,
) -> (f64, f32) {
    let combat = super::attack::combat_data();
    let (enchants, tags): (
        &crate::enchant::Enchantments,
        &dyn crate::predicate::EntityTags,
    ) = (&combat.enchants, &*combat);
    let arrow = crate::predicate::EntityFacts {
        kind: Some(kind),
        ..crate::predicate::EntityFacts::default()
    };
    let cx = crate::enchant::EnchantContext {
        types: env.types,
        tags,
        this: super::attack::facts(ents, target),
        attacker: super::attack::facts(ents, owner),
        direct_attacker: Some(arrow),
        attacking_player: None,
        source: None,
        raining: false,
        thundering: false,
        location: None,
    };
    #[allow(clippy::cast_possible_truncation)]
    let damage = enchants.modify_damage(weapon, &cx, env.random, base_damage as f32);
    let knockback = enchants.modify_knockback(weapon, &cx, env.random, 0.0);
    (f64::from(damage), knockback)
}
