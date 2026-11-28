//! Wind charges (`AbstractWindCharge`, `WindCharge`, `BreezeWindCharge`): straight flying
//! projectiles without gravity or drag that burst into a knockback-only explosion where they
//! hit (radius 1.2 with 1.22 times the knockback for players' charges, 3 for breezes').

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_nbt::Compound;
use vanillite_world::entity::{EntityId, Rotation};

use crate::mob::missile::{MissileData, MissileKind, MissileTick};
use crate::mob::{WindBurst, WorldAction, combat};
use crate::survival::damage::DamageSource;

/// `MissileData.ints[0]`: `WindCharge.noDeflectTicks`.
const NO_DEFLECT: usize = 0;

/// `WindCharge.deflect` refuses while `noDeflectTicks` run (a fresh player's charge).
pub(crate) fn refuses_deflection(d: &MissileData) -> bool {
    d.kind == MissileKind::WindCharge && d.ints[NO_DEFLECT] > 0
}

/// `Projectile.getMovementToShoot(x, y, z, pow, uncertainty)` with a random.
pub fn movement_to_shoot(
    random: &mut impl RandomSource,
    dir: Vec3,
    pow: f32,
    uncertainty: f32,
) -> Vec3 {
    let n = dir.normalize();
    let spread = 0.017_227_5 * f64::from(uncertainty);
    let v = Vec3::new(
        n.x + random.triangle(0.0, spread),
        n.y + random.triangle(0.0, spread),
        n.z + random.triangle(0.0, spread),
    );
    v * f64::from(pow)
}

/// The rotation of a projectile flying along `v` (`Projectile.shoot`'s `setYRot`/`setXRot`).
pub fn rotation_of(v: Vec3) -> Rotation {
    let h = (v.x * v.x + v.z * v.z).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let y_rot = crate::mob::atan2_deg(v.x, v.z) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let x_rot = crate::mob::atan2_deg(v.y, h) as f32;
    Rotation {
        y_rot,
        x_rot,
        y_head_rot: 0.0,
    }
}

/// A new wind charge of `kind` from its owner.
pub fn new_charge(kind: MissileKind) -> MissileData {
    let mut d = MissileData::new(kind);
    // `accelerationPower = 0`.
    d.acceleration = 0.0;
    if kind == MissileKind::WindCharge {
        d.ints[NO_DEFLECT] = 5;
    }
    d
}

/// `WindChargeItem.use`: a wind charge from the player's eyes (`spawnProjectileFromRotation`
/// with power 1.5), the throw sound, the use statistic and one item less.
pub fn player_throw(
    ents: &mut crate::entities::Entities,
    env: &mut crate::survival::player::Env<'_>,
    network_ids: &vanillite_world::entity::NetworkIds,
    player: EntityId,
    stack: &mut crate::item::ItemStack,
) {
    let Some(pos) = ents.store.get(player).map(|e| e.pos()) else {
        return;
    };
    let eye = crate::mob::missile::eye_height(ents, player);
    crate::mob::missile::spawn_from_rotation(
        ents,
        network_ids,
        player,
        Vec3::new(pos.x, pos.y + eye, pos.z),
        0.0,
        1.5,
        1.0,
        new_charge(MissileKind::WindCharge),
    );
    let pitch = 0.4 / (env.random.next_float() * 0.4 + 0.8);
    crate::survival::play_sound(
        env.out,
        None,
        pos,
        "minecraft:entity.wind_charge.throw",
        vanillite_protocol::packets::play::clientbound::SoundSource::Neutral,
        0.5,
        pitch,
        crate::survival::sound_seed(),
    );
    let infinite = ents.player_mut(player).is_some_and(|p| {
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, stack.item),
            1,
        );
        p.game_mode == vanillite_config::GameMode::Creative
    });
    if !infinite {
        stack.shrink(1);
    }
}

/// One tick of a wind charge (`AbstractWindCharge.tick`, `AbstractHurtingProjectile.tick`).
pub fn tick(t: &mut MissileTick<'_, '_>) {
    let Some(kind) = t.ents.missile(t.id).map(|d| d.kind) else {
        return;
    };
    if !matches!(
        kind,
        MissileKind::WindCharge | MissileKind::BreezeWindCharge
    ) {
        return;
    }
    let Some((pos, mut velocity)) = t.core().map(|e| (e.pos(), e.velocity())) else {
        return;
    };
    // Above the build height plus 30: it bursts.
    if pos.y > f64::from(t.level.max_y() + 30) {
        burst(t, pos);
        discard(t);
        return;
    }
    let owner = t.ents.missile(t.id).and_then(|d| d.owner);
    if owner.is_some_and(|o| t.ents.store.get(o).is_none()) {
        discard(t);
        return;
    }
    // `applyInertia` with inertia 1: thrown charges keep their speed, a deflected one
    // (`accelerationPower` 0.1) speeds up.
    let acceleration = t.ents.missile(t.id).map_or(0.0, |d| d.acceleration);
    if acceleration != 0.0 {
        let n = if velocity.length() < 1.0e-5 {
            Vec3::ZERO
        } else {
            velocity.normalize()
        };
        velocity = velocity + n * acceleration;
        if let Some(mut e) = t.ents.store.get_mut(t.id) {
            e.set_velocity(velocity);
        }
    }
    let to = pos + velocity;
    let block_hit = crate::mob::physics::clip(t.level, pos, to);
    let end = block_hit.map_or(to, |(_, at)| at);
    let entity_hit = hit_entity(t, pos, end, kind, owner);
    // Bukkit's `ProjectileHitEvent`: a cancelled hit is ignored, the charge flies on.
    let hit_event = |what| crate::mob::missile::hit_event(t.level, t.ents, t.id, owner, what);
    let entity_hit =
        entity_hit.filter(|&(target, _)| hit_event(crate::mob::missile::HitWhat::Entity(target)));
    let block_hit = block_hit.filter(|&(block, at)| {
        entity_hit.is_some() || hit_event(crate::mob::missile::HitWhat::Block(block, at))
    });
    let new_pos = match (entity_hit, block_hit) {
        (Some((_, at)), _) => at,
        (None, Some((_, at))) => at,
        (None, None) => to,
    };
    // `rotateTowardsMovement(this, 0.2)`.
    if let Some(mut e) = t.ents.store.get_mut(t.id) {
        let mut rot = e.as_ref().rot();
        let target = rotation_of(velocity);
        rot.y_rot += (target.y_rot - rot.y_rot) * 0.2;
        rot.x_rot += (target.x_rot - rot.x_rot) * 0.2;
        e.set_rot(rot);
        e.set_pos(new_pos);
    }
    update_left_owner(t, owner, new_pos);
    if let Some(d) = t.data()
        && d.ints[NO_DEFLECT] > 0
    {
        d.ints[NO_DEFLECT] -= 1;
    }
    if let Some((target, hit)) = entity_hit {
        on_hit_entity(t, target, owner, new_pos);
        crate::mob::missile::land_event(t.env.out, t.level, t.id, owner, None, hit);
        discard(t);
    } else if let Some((block, at)) = block_hit {
        // `onHitBlock`: `super.onHitBlock`, then the burst a quarter block out of the hit face.
        crate::mob::missile::hit_block(t.level, t.cx, t.id, block, at);
        let normal = face_normal(block, at);
        burst(t, at + normal * 0.25);
        crate::mob::missile::land_event(t.env.out, t.level, t.id, owner, Some(block), at);
        discard(t);
    }
}

/// The normal of the face of `block` the point `at` lies on.
fn face_normal(block: BlockPos, at: Vec3) -> Vec3 {
    let local = Vec3::new(
        at.x - f64::from(block.x),
        at.y - f64::from(block.y),
        at.z - f64::from(block.z),
    );
    let candidates = [
        (local.x.abs(), Vec3::new(-1.0, 0.0, 0.0)),
        ((1.0 - local.x).abs(), Vec3::new(1.0, 0.0, 0.0)),
        (local.y.abs(), Vec3::new(0.0, -1.0, 0.0)),
        ((1.0 - local.y).abs(), Vec3::new(0.0, 1.0, 0.0)),
        (local.z.abs(), Vec3::new(0.0, 0.0, -1.0)),
        ((1.0 - local.z).abs(), Vec3::new(0.0, 0.0, 1.0)),
    ];
    candidates
        .iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(Vec3::ZERO, |c| c.1)
}

/// `ProjectileUtil.getEntityHitResult` along the move: the closest living entity whose box
/// (grown by the margin) the segment crosses, not the owner before it left, no other wind
/// charges.
fn hit_entity(
    t: &MissileTick<'_, '_>,
    from: Vec3,
    to: Vec3,
    _kind: MissileKind,
    owner: Option<EntityId>,
) -> Option<(EntityId, Vec3)> {
    let (life, left) = t
        .ents
        .missile(t.id)
        .map_or((0, true), |d| (d.life, d.left_owner));
    // `computeMargin`: grows from 0 to 0.3 over the first ticks.
    #[allow(clippy::cast_precision_loss)]
    let margin = ((life - 2) as f32 / 20.0).clamp(0.0, 0.3);
    let swept = Aabb::new(
        from.x.min(to.x),
        from.y.min(to.y),
        from.z.min(to.z),
        from.x.max(to.x),
        from.y.max(to.y),
        from.z.max(to.z),
    )
    .inflate(1.0, 1.0, 1.0);
    let mut best: Option<(EntityId, Vec3, f64)> = None;
    t.ents.store.for_each_in(&swept, false, |id| {
        if id == t.id || (!left && Some(id) == owner) {
            return;
        }
        // `AbstractWindCharge.canHitEntity`: no end crystals (no other wind charges either).
        let crystal = matches!(
            t.ents.special(id),
            Some(crate::mob::boss::special::SpecialData::EndCrystal(_))
        );
        if crystal || !crate::effect::thrown::can_be_hit(t.ents, id) {
            return;
        }
        let Some(e) = t.ents.store.get(id) else {
            return;
        };
        let m = f64::from(margin);
        let b = e.aabb().inflate(m, m, m);
        if let Some(f) = segment_box(from, to, &b) {
            let at = from + (to - from) * f;
            let d = from.distance_to_sqr(at);
            if best.is_none_or(|(_, _, bd)| d < bd) {
                best = Some((id, at, d));
            }
        }
    });
    best.map(|(id, at, _)| (id, at))
}

/// Where the segment enters `b`, as a fraction (`AABB.clip`).
fn segment_box(from: Vec3, to: Vec3, b: &Aabb) -> Option<f64> {
    crate::mob::projectile::segment_box(from, to, b)
}

/// `Projectile.checkLeftOwner`: once the owner's box no longer touches the charge's.
fn update_left_owner(t: &mut MissileTick<'_, '_>, owner: Option<EntityId>, pos: Vec3) {
    let left = t.ents.missile(t.id).is_none_or(|d| d.left_owner);
    if left {
        return;
    }
    let Some(o) = owner.and_then(|o| t.ents.store.get(o)) else {
        if let Some(d) = t.data() {
            d.left_owner = true;
        }
        return;
    };
    let own = o.aabb().inflate(1.0, 1.0, 1.0);
    let me = Aabb::new(
        pos.x - 0.15,
        pos.y - 0.15,
        pos.z - 0.15,
        pos.x + 0.15,
        pos.y + 0.15,
        pos.z + 0.15,
    );
    if !own.intersects(&me)
        && let Some(d) = t.data()
    {
        d.left_owner = true;
    }
}

/// `AbstractWindCharge.onHitEntity`: 1 wind charge damage, then the burst.
fn on_hit_entity(t: &mut MissileTick<'_, '_>, target: EntityId, owner: Option<EntityId>, at: Vec3) {
    // `owner.setLastHurtMob(target)` of any living owner, players too.
    let living = crate::effect::thrown::is_living(t.ents, target);
    if let Some(o) = owner {
        if let Some(m) = t.ents.mob_mut(o) {
            m.last_hurt_mob = living.then_some(target);
        } else if let Some(p) = t.ents.player_mut(o) {
            p.survival.last_hurt_mob = living.then_some((target, p.survival.tick_count));
        }
    }
    let kind = t.env.types.named("minecraft:wind_charge");
    let mut source = DamageSource::of(kind);
    source.direct = Some(t.id);
    source.cause = owner;
    source.position = Some(at);
    combat::hurt(t.level, t.ents, t.env, t.cx, target, source, 1.0);
    let pos = t.core().map_or(at, |e| e.pos());
    burst(t, pos);
}

/// `explode(position)`: the knockback-only burst.
fn burst(t: &mut MissileTick<'_, '_>, pos: Vec3) {
    let breeze = t
        .ents
        .missile(t.id)
        .is_some_and(|d| d.kind == MissileKind::BreezeWindCharge);
    let (radius, knockback, sound) = if breeze {
        (3.0, 1.0, "minecraft:entity.breeze.wind_burst")
    } else {
        // `Optional.of(1.22F)`: a float.
        (
            1.2,
            f64::from(1.22_f32),
            "minecraft:entity.wind_charge.wind_burst",
        )
    };
    let id = t.id;
    t.cx.actions.push(WorldAction::WindBurst(WindBurst {
        source: Some(id),
        pos,
        radius,
        knockback,
        sound,
    }));
}

fn discard(t: &mut MissileTick<'_, '_>) {
    if let Some(d) = t.data() {
        d.removed = true;
    }
}

/// `WindCharge` keeps no extra NBT; the deflection timer is not saved.
pub fn save(_d: &MissileData, _out: &mut Compound) {}

/// Loaded charges keep flying without acceleration.
pub fn load(d: &mut MissileData, _tag: &Compound) {
    d.acceleration = 0.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_enters_box() {
        let b = Aabb::new(1.0, -1.0, -1.0, 2.0, 1.0, 1.0);
        let f = segment_box(Vec3::ZERO, Vec3::new(4.0, 0.0, 0.0), &b).expect("hit");
        assert!((f - 0.25).abs() < 1e-9);
        assert!(segment_box(Vec3::ZERO, Vec3::new(0.0, 4.0, 0.0), &b).is_none());
    }

    #[test]
    fn shot_keeps_direction_without_spread() {
        let mut r = vanillite_base::random::LegacyRandom::new(1);
        let v = movement_to_shoot(&mut r, Vec3::new(3.0, 0.0, 4.0), 0.7, 0.0);
        assert!((v.length() - 0.7).abs() < 1e-6);
        let rot = rotation_of(Vec3::new(0.0, 0.0, 1.0));
        assert!(rot.y_rot.abs() < 1e-4);
        assert_eq!(
            face_normal(BlockPos::new(0, 0, 0), Vec3::new(0.5, 1.0, 0.5)),
            Vec3::new(0.0, 1.0, 0.0)
        );
    }

    #[test]
    fn new_charges_do_not_accelerate() {
        let d = new_charge(MissileKind::WindCharge);
        assert_eq!(d.acceleration, 0.0);
        assert_eq!(d.ints[NO_DEFLECT], 5);
    }
}
