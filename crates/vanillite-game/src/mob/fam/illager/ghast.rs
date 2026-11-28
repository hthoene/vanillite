//! Ghasts: floating around, shooting large fireballs, dying to reflected ones.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_chunk::HeightmapKind;
use vanillite_world::entity::Rotation;

use super::super::super::common::{self, CommonGoal};
use super::super::super::goals::Goal as MobGoal;
use super::super::super::kinds::Kind;
use super::super::super::living::has_line_of_sight;
use super::super::super::missile::{self, MissileData, MissileKind};
use super::super::super::nav::Operation;
use super::super::super::physics;
use super::super::super::{MobData, Tick, WorldAction, spawn};
use super::super::{Hurt, SpawnReason};
use super::{Goal, attr_default, dirty, fam, st, st_mut};
use crate::level::Level;

/// `Ghast.registerGoals`.
pub fn register_goals(m: &mut MobData) {
    let g = &mut m.goals;
    let tg = &mut m.targets;
    g.add(5, fam(Goal::GhastFloat));
    g.add(7, fam(Goal::GhastLook));
    g.add(7, fam(Goal::GhastShoot { charge_time: 0 }));
    tg.add(
        1,
        MobGoal::Common(CommonGoal::NearestAttackable {
            state: super::super::super::goals::TargetState::new(true),
            interval: super::super::super::goals::reduced(10),
            found: None,
            filter: |t, id| {
                common::any_player(t, id)
                    && t.ents
                        .store
                        .get(id)
                        .is_some_and(|e| (e.pos().y - t.pos().y).abs() <= 4.0)
            },
            kinds: None,
            players_only: true,
            precheck: None,
        }),
    );
}

/// `Ghast.checkGhastSpawnRules`.
pub fn check_spawn_rules(
    kind: Kind,
    level: &Level,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    // The difficulty check is the spawner's (`spawnEnemies`); the roll is always taken.
    random.next_int_bounded(20) == 0 && super::check_mob_spawn_rules(kind, level, pos, reason)
}

pub fn set_charging(t: &mut Tick<'_, '_>, on: bool) {
    if st(t.mob_ref()).ghast_charging != on {
        st_mut(t.mob()).ghast_charging = on;
        dirty(t);
    }
}

/// `Ghast.faceMovementDirection`.
pub fn face_movement_direction(t: &mut Tick<'_, '_>) {
    let p = t.pos();
    let y_rot = match t.mob_ref().target.and_then(|id| t.other(id)) {
        None => {
            let v = t.velocity();
            Some(-(vanillite_base::math::atan2(v.x, v.z) as f32) * (180.0 / std::f32::consts::PI))
        }
        Some(o) if o.pos.distance_to_sqr(p) < 4096.0 => {
            let (xd, zd) = (o.pos.x - p.x, o.pos.z - p.z);
            Some(-(vanillite_base::math::atan2(xd, zd) as f32) * (180.0 / std::f32::consts::PI))
        }
        Some(_) => None,
    };
    if let Some(y) = y_rot {
        let mut rot = t.core().rot();
        rot.y_rot = y;
        t.core_mut().set_rot(rot);
        t.mob().y_body_rot = y;
    }
}

/// `GhastMoveControl.tick` (not careful, never stopped).
pub fn move_control(t: &mut Tick<'_, '_>) {
    if t.mob_ref().move_ctl.operation != Operation::MoveTo {
        return;
    }
    let s = st_mut(t.mob());
    let due = s.float_duration <= 0;
    s.float_duration -= 1;
    if !due {
        return;
    }
    let extra = t.mob().random.next_int_bounded(5) + 2;
    st_mut(t.mob()).float_duration += extra;
    let travel = t.mob_ref().move_ctl.wanted - t.pos();
    if can_reach(t, travel) {
        let speed = attr_default(t.mob_ref().kind, "minecraft:flying_speed");
        let v = t.velocity() + travel.normalize() * (speed * 5.0 / 3.0);
        t.set_velocity(v);
    } else {
        t.mob().move_ctl.operation = Operation::Wait;
    }
}

/// `canReach`: no block collides with the box moving along `travel` (blocks the box already
/// touches do not count).
fn can_reach(t: &mut Tick<'_, '_>, travel: Vec3) -> bool {
    let aabb = t.aabb();
    let end = Aabb::new(
        aabb.min_x + travel.x,
        aabb.min_y + travel.y,
        aabb.min_z + travel.z,
        aabb.max_x + travel.x,
        aabb.max_y + travel.y,
        aabb.max_z + travel.z,
    );
    let swept = Aabb::new(
        aabb.min_x.min(end.min_x),
        aabb.min_y.min(end.min_y),
        aabb.min_z.min(end.min_z),
        aabb.max_x.max(end.max_x),
        aabb.max_y.max(end.max_y),
        aabb.max_z.max(end.max_z),
    );
    let mut boxes = std::mem::take(&mut t.cx.boxes);
    boxes.clear();
    physics::block_boxes(&t.level.view(), &swept, &mut boxes);
    let half = Vec3::new(
        (aabb.max_x - aabb.min_x) / 2.0,
        (aabb.max_y - aabb.min_y) / 2.0,
        (aabb.max_z - aabb.min_z) / 2.0,
    );
    let from = Vec3::new(
        aabb.min_x + half.x,
        aabb.min_y + half.y,
        aabb.min_z + half.z,
    );
    let to = from + travel;
    let blocked = boxes.iter().any(|b| {
        // A block the ghast already overlaps does not stop it.
        if b.intersects(&aabb) {
            return false;
        }
        let grown = Aabb::new(
            b.min_x - half.x,
            b.min_y - half.y,
            b.min_z - half.z,
            b.max_x + half.x,
            b.max_y + half.y,
            b.max_z + half.z,
        );
        super::super::super::projectile::segment_box(from, to, &grown).is_some()
    });
    t.cx.boxes = boxes;
    !blocked
}

/// `RandomFloatAroundGoal.canUse`.
pub fn float_can_use(t: &mut Tick<'_, '_>) -> bool {
    let ctl = t.mob_ref().move_ctl;
    if !ctl.has_wanted() {
        return true;
    }
    let d = ctl.wanted - t.pos();
    let dd = d.length_sqr();
    !(1.0..=3600.0).contains(&dd)
}

/// `RandomFloatAroundGoal.start`: `getSuitableFlyToPosition(ghast, 0)`.
pub fn float_start(t: &mut Tick<'_, '_>) {
    let center = t.pos();
    let pick = |r: &mut LegacyRandom| {
        let x = center.x + f64::from((r.next_float() * 2.0 - 1.0) * 16.0);
        let y = center.y + f64::from((r.next_float() * 2.0 - 1.0) * 16.0);
        let z = center.z + f64::from((r.next_float() * 2.0 - 1.0) * 16.0);
        Vec3::new(x, y, z)
    };
    for _ in 0..64 {
        let c = pick(&mut t.mob().random);
        let m = t.mob_ref();
        if !m.has_restriction() || m.is_within_restriction(BlockPos::containing(c.x, c.y, c.z)) {
            t.move_ctl().set_wanted_position(c, 1.0);
            return;
        }
    }
    let mut result = pick(&mut t.mob().random);
    let b = BlockPos::containing(result.x, result.y, result.z);
    let height = spawn::surface_y(t.level, HeightmapKind::MotionBlocking, b.x, b.z);
    if height < b.y && height > t.level.min_y() {
        let y = t.pos().y;
        result = Vec3::new(result.x, y - (y - result.y).abs(), result.z);
    }
    t.move_ctl().set_wanted_position(result, 1.0);
}

/// `GhastShootFireballGoal.tick`.
pub fn shoot_tick(t: &mut Tick<'_, '_>, charge_time: &mut i32) {
    let Some(target) = t.mob_ref().target else {
        return;
    };
    let Some(o) = t.other(target) else {
        return;
    };
    let p = t.pos();
    if o.pos.distance_to_sqr(p) < 4096.0 && has_line_of_sight(t, target) {
        *charge_time += 1;
        let here = t.block_pos();
        if *charge_time == 10 && !t.mob_ref().silent {
            t.cx.actions.push(WorldAction::LevelEvent(1015, here, 0));
        }
        if *charge_time == 20 {
            let view = view_vector(t.core().rot());
            let half_h = f64::from(t.core().dims().height) * 0.5;
            let xd = o.pos.x - (p.x + view.x * 4.0);
            let yd = (o.aabb.min_y + (o.aabb.max_y - o.aabb.min_y) * 0.5) - (0.5 + p.y + half_h);
            let zd = o.pos.z - (p.z + view.z * 4.0);
            if !t.mob_ref().silent {
                t.cx.actions.push(WorldAction::LevelEvent(1016, here, 0));
            }
            let mut d = MissileData::new(MissileKind::Fireball);
            d.explosion_power = st(t.mob_ref()).explosion_power;
            d.item = super::super::super::equipment::stack("minecraft:fire_charge");
            // `new LargeFireball(level, ghast, direction.normalize(), power)` normalizes again.
            let dir = Vec3::new(xd, yd, zd).normalize().normalize();
            let velocity = dir * d.acceleration;
            let start = Vec3::new(p.x + view.x * 4.0, p.y + half_h + 0.5, p.z + view.z * 4.0);
            let rot = t.core().rot();
            missile::spawn_from(
                t,
                start,
                velocity,
                Rotation {
                    y_head_rot: 0.0,
                    ..rot
                },
                d,
            );
            *charge_time = -40;
        }
    } else if *charge_time > 0 {
        *charge_time -= 1;
    }
    let charging = *charge_time > 10;
    set_charging(t, charging);
}

/// `getViewVector(1.0F)`.
pub fn view_vector(rot: Rotation) -> Vec3 {
    let pitch = f64::from(rot.x_rot) * std::f64::consts::PI / 180.0;
    let yaw = f64::from(-rot.y_rot) * std::f64::consts::PI / 180.0;
    let (yc, ys) = (
        f64::from(vanillite_base::math::cos(yaw)),
        f64::from(vanillite_base::math::sin(yaw)),
    );
    let (xc, xs) = (
        f64::from(vanillite_base::math::cos(pitch)),
        f64::from(vanillite_base::math::sin(pitch)),
    );
    Vec3::new(ys * xc, -xs, yc * xc)
}

/// `Ghast.hurtServer`: a reflected fireball from a player kills it, other damage as usual.
pub fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
    let reflected = h.source.direct.is_some_and(|d| {
        h.ents
            .missile(d)
            .is_some_and(|m| m.kind == MissileKind::Fireball)
    }) && h.source.cause.is_some_and(|c| h.ents.player(c).is_some());
    if reflected {
        return Some(1000.0);
    }
    Some(amount)
}
