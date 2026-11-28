//! The family's projectiles: small and large fireballs (`AbstractHurtingProjectile`), shulker
//! bullets, evoker fangs and thrown potions (`AbstractThrownPotion`).
//!
//! Field use of [`MissileData`]: `ints[0]` the bullet's move direction (face id, 6 = none) or
//! the fangs' warmup, `ints[1]` the bullet's flight steps or the fangs' life, `ints[2]` the
//! fangs' spike event flag; `vec` the bullet's target deltas.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_data::block::ids;
use vanillite_nbt::Compound;
use vanillite_world::entity::{EntityFlags, EntityId};

use super::super::super::kinds::Kind;
use super::super::super::missile::{MissileData, MissileKind, MissileTick};
use super::super::super::physics::{self, Blocks};
use super::super::super::projectile::{lerp_rotation, segment_box};
use super::super::super::{ExplosionRequest, WorldAction, atan2_deg, combat, is_water};
use super::potion::{self, Potion};
use super::{is_illager, st};
use crate::effect as effects;
use crate::effect::{EffectInstance, MobEffect};
use crate::entities::Entities;
use crate::level::Level;
use crate::player::encode::DataWriter;
use crate::survival::damage::{DamageKind, DamageSource};
use crate::survival::rules::{Difficulty, Rule};

/// No move direction.
const NO_DIR: i32 = 6;

/// What a projectile's move hits (`HitResult`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Miss,
    /// A block, the hit point and the face hit (face id).
    Block(BlockPos, Vec3, u8),
    Entity(EntityId, Vec3),
}

impl Hit {
    fn location(self) -> Option<Vec3> {
        match self {
            Hit::Miss => None,
            Hit::Block(_, at, _) | Hit::Entity(_, at) => Some(at),
        }
    }
}

/// The face of a block a ray entering it at `at` hits.
fn hit_face(pos: BlockPos, at: Vec3, velocity: Vec3) -> u8 {
    let fx = at.x - f64::from(pos.x);
    let fy = at.y - f64::from(pos.y);
    let fz = at.z - f64::from(pos.z);
    // Distance to the face the ray enters through on each moving axis.
    let mut best = (f64::MAX, 1u8);
    let mut consider = |v: f64, frac: f64, neg: u8, pos_face: u8| {
        if v > 0.0 {
            if frac.abs() < best.0 {
                best = (frac.abs(), neg);
            }
        } else if v < 0.0 && (1.0 - frac).abs() < best.0 {
            best = ((1.0 - frac).abs(), pos_face);
        }
    };
    consider(velocity.y, fy, 0, 1);
    consider(velocity.z, fz, 2, 3);
    consider(velocity.x, fx, 4, 5);
    best.1
}

use crate::mob::missile::margin;

/// Whether the projectile may hit `target` (`canHitEntity`): a live living entity, the owner
/// only once it left it.
fn can_hit(ents: &Entities, d: &MissileData, me: EntityId, target: EntityId) -> bool {
    if target == me {
        return false;
    }
    crate::effect::thrown::can_be_hit(ents, target) && (d.left_owner || d.owner != Some(target))
}

/// `ProjectileUtil.getHitResultOnMoveVector`: the first block along the move, or the nearest
/// entity the move passes through before it.
fn hit_on_move(
    level: &Level,
    ents: &Entities,
    me: EntityId,
    d: &MissileData,
    pos: Vec3,
    v: Vec3,
) -> Hit {
    let to = pos + v;
    let block = physics::clip(level as &dyn Blocks, pos, to);
    let end = block.map_or(to, |(_, at)| at);
    let Some(e) = ents.store.get(me) else {
        return Hit::Miss;
    };
    let area = e
        .aabb()
        .expand_towards(v.x, v.y, v.z)
        .inflate(1.0, 1.0, 1.0);
    let m = margin(d.life);
    let mut best: Option<(EntityId, f64, Vec3)> = None;
    ents.store.for_each_in(&area, false, |other| {
        if !can_hit(ents, d, me, other) {
            return;
        }
        let Some(oe) = ents.store.get(other) else {
            return;
        };
        let grown = oe.aabb().inflate(m, m, m);
        if let Some(t) = segment_box(pos, end, &grown) {
            let hit = pos + (end - pos) * t;
            let dist = pos.distance_to_sqr(hit);
            if best.is_none_or(|(_, b, _)| dist < b) {
                best = Some((other, dist, hit));
            }
        }
    });
    // Bukkit's `ProjectileHitEvent`: a cancelled hit is ignored and the projectile flies on.
    let shooter = owner(ents, d);
    let allowed = |hit| crate::mob::missile::hit_event(level, ents, me, shooter, hit);
    if let Some((id, _, at)) = best
        && allowed(crate::mob::missile::HitWhat::Entity(id))
    {
        return Hit::Entity(id, at);
    }
    match block {
        Some((p, at)) if allowed(crate::mob::missile::HitWhat::Block(p, at)) => {
            Hit::Block(p, at, hit_face(p, at, v))
        }
        _ => Hit::Miss,
    }
}

/// `checkLeftOwner`: the projectile left its owner's box.
fn check_left_owner(ents: &Entities, me: EntityId, d: &mut MissileData) {
    if d.left_owner {
        return;
    }
    let Some(e) = ents.store.get(me) else {
        return;
    };
    let v = e.velocity();
    let aabb = e
        .aabb()
        .expand_towards(v.x, v.y, v.z)
        .inflate(1.0, 1.0, 1.0);
    let inside = d.owner.is_some_and(|o| {
        ents.store
            .get(o)
            .is_some_and(|oe| oe.aabb().intersects(&aabb))
    });
    d.left_owner = !inside;
}

/// The owner if it still exists.
fn owner(ents: &Entities, d: &MissileData) -> Option<EntityId> {
    d.owner
        .filter(|&o| ents.store.contains(o))
        .or_else(|| d.owner_uuid.and_then(|u| ents.player_by_uuid(u)))
}

/// A damage source of the projectile and its owner.
fn source(ents: &Entities, kind: DamageKind, me: EntityId, d: &MissileData) -> DamageSource {
    match owner(ents, d) {
        Some(o) => combat::entity_source(ents, kind, o, me),
        None => {
            let mut s = DamageSource::of(kind);
            s.direct = Some(me);
            s.position = ents.store.get(me).map(|e| e.pos());
            s
        }
    }
}

/// Moves the projectile and rotates it towards its motion (`rotateTowardsMovement`).
fn place(ents: &mut Entities, me: EntityId, pos: Vec3, v: Vec3) {
    let Some(mut e) = ents.store.get_mut(me) else {
        return;
    };
    let mut rot = e.as_ref().rot();
    #[allow(clippy::cast_possible_truncation)]
    let y_rot = atan2_deg(v.x, v.z) as f32;
    let hd = (v.x * v.x + v.z * v.z).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let x_rot = atan2_deg(v.y, hd) as f32;
    rot.x_rot = lerp_rotation(rot.x_rot, x_rot);
    rot.y_rot = lerp_rotation(rot.y_rot, y_rot);
    e.set_pos(pos);
    e.set_velocity(v);
    e.set_rot(rot);
}

/// `AbstractHurtingProjectile.applyInertia`: the push along the flight, then the drag (0.95,
/// 0.8 in water).
pub fn fireball_inertia(v: Vec3, acceleration: f64, in_water: bool) -> Vec3 {
    let inertia = f64::from(if in_water { 0.8_f32 } else { 0.95_f32 });
    let n = if v.length() < 1.0e-5 {
        Vec3::ZERO
    } else {
        v.normalize()
    };
    (v + n * acceleration) * inertia
}

/// One projectile tick of the family.
pub fn tick(t: &mut MissileTick<'_, '_>) {
    let Some(kind) = t.ents.missile(t.id).map(|d| d.kind) else {
        return;
    };
    match kind {
        MissileKind::SmallFireball | MissileKind::Fireball => fireball_tick(t),
        MissileKind::ShulkerBullet => bullet_tick(t),
        MissileKind::EvokerFangs => fangs_tick(t),
        MissileKind::SplashPotion | MissileKind::LingeringPotion => potion_tick(t),
        _ => {}
    }
}

fn discard(t: &mut MissileTick<'_, '_>) {
    if let Some(d) = t.ents.missile_mut(t.id) {
        d.removed = true;
    }
}

// --- Fireballs --------------------------------------------------------------------------------

/// `AbstractHurtingProjectile.tick`.
fn fireball_tick(t: &mut MissileTick<'_, '_>) {
    let me = t.id;
    let Some(mut d) = t.ents.missile(me).cloned() else {
        return;
    };
    let Some(e) = t.ents.store.get(me) else {
        return;
    };
    let (pos, mut v) = (e.pos(), e.velocity());
    // `applyInertia`.
    let in_water = is_water(t.level.block(BlockPos::containing(pos.x, pos.y, pos.z)));
    v = fireball_inertia(v, d.acceleration, in_water);
    let owner_gone = d.owner.is_some_and(|o| !t.ents.store.contains(o));
    if owner_gone || !t.level.is_loaded(BlockPos::containing(pos.x, pos.y, pos.z)) {
        discard(t);
        return;
    }
    let hit = hit_on_move(t.level, t.ents, me, &d, pos, v);
    let new_pos = hit.location().unwrap_or(pos + v);
    place(t.ents, me, new_pos, v);
    check_left_owner(t.ents, me, &mut d);
    if let Some(slot) = t.ents.missile_mut(me) {
        *slot = d.clone();
    }
    // `shouldBurn`: the fireball itself burns.
    if let Some(mut e) = t.ents.store.get_mut(me) {
        e.set_flags(EntityFlags::ON_FIRE, true);
    }
    if hit != Hit::Miss && !deflected(t, hit, true) {
        fireball_hit(t, &d, hit, new_pos);
    }
}

/// `hitTargetOrDeflectSelf`: an entity hit of a breeze turns the projectile back instead
/// (`AbstractHurtingProjectile.onDeflection(false)` halves the acceleration).
fn deflected(t: &mut MissileTick<'_, '_>, hit: Hit, hurting: bool) -> bool {
    let Hit::Entity(target, _) = hit else {
        return false;
    };
    let Some(d) = t.ents.missile(t.id) else {
        return false;
    };
    if !crate::mob::missile::deflects(t.ents, target, d.kind.type_id()) {
        return false;
    }
    let first = d.last_deflected != Some(target);
    crate::mob::fam::wildlife::snowball::deflect(t, target);
    if first
        && hurting
        && let Some(d) = t.ents.missile_mut(t.id)
    {
        d.acceleration *= 0.5;
    }
    true
}

fn fireball_hit(t: &mut MissileTick<'_, '_>, d: &MissileData, hit: Hit, at: Vec3) {
    hit_block(t, hit);
    let me = t.id;
    let fireball = t.env.types.named("minecraft:fireball");
    match hit {
        Hit::Entity(target, _) => {
            let src = source(t.ents, fireball, me, d);
            if d.kind == MissileKind::SmallFireball {
                let before = fire_ticks(t.ents, target);
                super::super::super::attack::ignite(t.ents, target, 100);
                if combat::hurt(t.level, t.ents, t.env, t.cx, target, src, 5.0) {
                    crate::mob::attack::post_attack_effects(
                        t.level, t.ents, t.env, t.cx, target, &src,
                    );
                } else {
                    set_fire_ticks(t.ents, target, before);
                }
            } else {
                combat::hurt(t.level, t.ents, t.env, t.cx, target, src, 6.0);
                crate::mob::attack::post_attack_effects(t.level, t.ents, t.env, t.cx, target, &src);
            }
        }
        Hit::Block(pos, _, face) => {
            if d.kind == MissileKind::SmallFireball {
                let mob_owner = owner(t.ents, d).is_some_and(|o| t.ents.mob(o).is_some());
                if !mob_owner || t.env.rules.bool(Rule::MOB_GRIEFING) {
                    let (x, y, z) = super::shulker::step(face);
                    let fire_pos = BlockPos::new(pos.x + x, pos.y + y, pos.z + z);
                    if t.level.block(fire_pos).is_air() {
                        let below =
                            t.level
                                .block(BlockPos::new(fire_pos.x, fire_pos.y - 1, fire_pos.z));
                        let soul =
                            below.block() == ids::SOUL_SAND || below.block() == ids::SOUL_SOIL;
                        let fire = if soul { ids::SOUL_FIRE } else { ids::FIRE };
                        t.cx.actions
                            .push(WorldAction::SetBlock(fire_pos, fire.default_state(), 3));
                    }
                }
            }
        }
        Hit::Miss => return,
    }
    land(t, hit);
    if d.kind == MissileKind::Fireball {
        // `LargeFireball.onHit`: the explosion (`ExplosionInteraction.MOB`), with fire when
        // mobs may grief.
        #[allow(clippy::cast_precision_loss)]
        let radius = d.explosion_power as f32;
        let fire = t.env.rules.bool(crate::survival::rules::Rule::MOB_GRIEFING);
        t.cx.actions.push(WorldAction::Explode(ExplosionRequest {
            source: Some(me),
            indirect: d.owner,
            pos: at,
            radius,
            mob: true,
            resistance: crate::level::explosion::BlockResistance::Default,
            fire,
        }));
    }
    discard(t);
}

fn fire_ticks(ents: &Entities, id: EntityId) -> i32 {
    ents.mob(id)
        .map(|m| m.fire_ticks)
        .or_else(|| ents.player(id).map(|p| p.survival.fire_ticks))
        .unwrap_or(-1)
}

fn set_fire_ticks(ents: &mut Entities, id: EntityId, ticks: i32) {
    if let Some(m) = ents.mob_mut(id) {
        m.fire_ticks = ticks;
    } else if let Some(p) = ents.player_mut(id) {
        p.survival.fire_ticks = ticks;
    }
}

// --- Shulker bullets --------------------------------------------------------------------------

fn face_step(face: i32) -> (i32, i32, i32) {
    super::shulker::step(u8::try_from(face).unwrap_or(0))
}

/// `ShulkerBullet.selectNextMoveDirection(avoidAxis, target)` (axes: 0 y, 1 z, 2 x).
pub fn select_next_move_direction(
    level: &Level,
    ents: &mut Entities,
    me: EntityId,
    avoid_axis: Option<u8>,
) {
    if ents.store.get(me).is_none() {
        return;
    }
    // The bullet's own random, put back below.
    let Some(mut random) = ents
        .missile(me)
        .map(|d| LegacyRandom::from_state(d.random_state))
    else {
        return;
    };
    let random = &mut random;
    let Some(e) = ents.store.get(me) else {
        return;
    };
    let pos = e.pos();
    let current = BlockPos::containing(pos.x, pos.y, pos.z);
    let target = ents.missile(me).and_then(|d| d.target);
    let (target_pos, y_offset) = match target.and_then(|t| ents.store.get(t)) {
        Some(te) => {
            let off = f64::from(te.dims().height) * 0.5;
            let p = te.pos();
            (BlockPos::containing(p.x, p.y + off, p.z), off)
        }
        None => (BlockPos::new(current.x, current.y - 1, current.z), 0.5),
    };
    let mut tx = f64::from(target_pos.x) + 0.5;
    let mut ty = f64::from(target_pos.y) + y_offset;
    let mut tz = f64::from(target_pos.z) + 0.5;
    let mut selection = NO_DIR;
    let center = Vec3::new(
        f64::from(target_pos.x) + 0.5,
        f64::from(target_pos.y) + 0.5,
        f64::from(target_pos.z) + 0.5,
    );
    if center.distance_to_sqr(pos) >= 4.0 {
        let empty = |p: BlockPos| level.block(p).is_air();
        let rel = |f: i32| {
            let (x, y, z) = face_step(f);
            BlockPos::new(current.x + x, current.y + y, current.z + z)
        };
        let mut options: Vec<i32> = Vec::with_capacity(3);
        if avoid_axis != Some(2) {
            if current.x < target_pos.x && empty(rel(5)) {
                options.push(5);
            } else if current.x > target_pos.x && empty(rel(4)) {
                options.push(4);
            }
        }
        if avoid_axis != Some(0) {
            if current.y < target_pos.y && empty(rel(1)) {
                options.push(1);
            } else if current.y > target_pos.y && empty(rel(0)) {
                options.push(0);
            }
        }
        if avoid_axis != Some(1) {
            if current.z < target_pos.z && empty(rel(3)) {
                options.push(3);
            } else if current.z > target_pos.z && empty(rel(2)) {
                options.push(2);
            }
        }
        selection = random.next_int_bounded(6);
        if options.is_empty() {
            let mut attempts = 5;
            while !empty(rel(selection)) && attempts > 0 {
                selection = random.next_int_bounded(6);
                attempts -= 1;
            }
        } else {
            let n = i32::try_from(options.len()).unwrap_or(1);
            selection = options[usize::try_from(random.next_int_bounded(n)).unwrap_or(0)];
        }
        let (x, y, z) = face_step(selection);
        tx = pos.x + f64::from(x);
        ty = pos.y + f64::from(y);
        tz = pos.z + f64::from(z);
    }
    let (xa, ya, za) = (tx - pos.x, ty - pos.y, tz - pos.z);
    let dist = (xa * xa + ya * ya + za * za).sqrt();
    let deltas = if dist == 0.0 {
        Vec3::ZERO
    } else {
        Vec3::new(xa / dist * 0.15, ya / dist * 0.15, za / dist * 0.15)
    };
    let steps = 10 + random.next_int_bounded(5) * 10;
    if let Some(d) = ents.missile_mut(me) {
        d.ints[0] = selection;
        d.vec = deltas;
        d.ints[1] = steps;
        d.random_state = random.state();
    }
}

/// `ShulkerBullet.tick`.
fn bullet_tick(t: &mut MissileTick<'_, '_>) {
    let me = t.id;
    if t.env.difficulty == Difficulty::Peaceful {
        discard(t);
        return;
    }
    let Some(mut d) = t.ents.missile(me).cloned() else {
        return;
    };
    let Some(e) = t.ents.store.get(me) else {
        return;
    };
    let pos = e.pos();
    let mut v = e.velocity();
    check_left_owner(t.ents, me, &mut d);
    let target = d
        .target
        .filter(|&id| t.ents.store.contains(id))
        .or_else(|| d.target_uuid.and_then(|u| t.ents.player_by_uuid(u)));
    if target.is_none() {
        d.target = None;
    }
    let target_alive = target.is_some_and(|id| {
        t.ents
            .mob(id)
            .is_some_and(super::super::super::MobData::is_alive)
            || t.ents.player(id).is_some_and(|p| {
                !p.survival.is_dead_or_dying() && p.game_mode != GameMode::Spectator
            })
    });
    if target_alive {
        let c = |x: f64| (x * 1.025).clamp(-1.0, 1.0);
        d.vec = Vec3::new(c(d.vec.x), c(d.vec.y), c(d.vec.z));
        v = v + Vec3::new(
            (d.vec.x - v.x) * 0.2,
            (d.vec.y - v.y) * 0.2,
            (d.vec.z - v.z) * 0.2,
        );
    } else if !e.flags().contains(EntityFlags::NO_GRAVITY) {
        v = v + Vec3::new(0.0, -0.04, 0.0);
    }
    let hit = hit_on_move(t.level, t.ents, me, &d, pos, v);
    let new_pos = pos + v;
    if let Some(slot) = t.ents.missile_mut(me) {
        *slot = d.clone();
    }
    if let Some(mut e) = t.ents.store.get_mut(me) {
        e.set_pos(new_pos);
        e.set_velocity(v);
    }
    if hit != Hit::Miss && !deflected(t, hit, false) {
        bullet_hit(t, &d, hit);
        if t.ents.missile(me).is_none_or(|x| x.removed) {
            return;
        }
    }
    // `rotateTowardsMovement(0.5)` is the client's; the direction logic follows.
    let Some(target) = target else {
        return;
    };
    let mut steps = d.ints[1];
    if steps > 0 {
        steps -= 1;
        if let Some(x) = t.ents.missile_mut(me) {
            x.ints[1] = steps;
        }
        if steps == 0 {
            let axis = (d.ints[0] != NO_DIR).then(|| u8::try_from(d.ints[0] / 2).unwrap_or(0));
            select_next_move_direction(t.level, t.ents, me, axis);
        }
    }
    let dir = t.ents.missile(me).map_or(NO_DIR, |x| x.ints[0]);
    if dir != NO_DIR {
        let current = BlockPos::containing(new_pos.x, new_pos.y, new_pos.z);
        let axis = u8::try_from(dir / 2).unwrap_or(0);
        let (x, y, z) = face_step(dir);
        let next = BlockPos::new(current.x + x, current.y + y, current.z + z);
        // `loadedAndEntityCanStandOn`: a full top face ahead.
        let solid = t.level.is_loaded(next) && t.level.block(next).misc().sturdy_full & 2 != 0;
        let tp = t.ents.store.get(target).map(|e| {
            let p = e.pos();
            BlockPos::containing(p.x, p.y, p.z)
        });
        let aligned = tp.is_some_and(|tp| match axis {
            2 => current.x == tp.x,
            1 => current.z == tp.z,
            _ => current.y == tp.y,
        });
        if solid || aligned {
            select_next_move_direction(t.level, t.ents, me, Some(axis));
        }
    }
}

fn bullet_hit(t: &mut MissileTick<'_, '_>, d: &MissileData, hit: Hit) {
    hit_block(t, hit);
    let me = t.id;
    match hit {
        Hit::Entity(target, _) => {
            let src = source(t.ents, t.env.types.mob_projectile, me, d);
            if combat::hurt(t.level, t.ents, t.env, t.cx, target, src, 4.0) {
                crate::mob::attack::post_attack_effects(t.level, t.ents, t.env, t.cx, target, &src);
                // `firstNonNull(owner, this)`.
                let by = owner(t.ents, d).or(Some(me));
                effects::living::add_to_entity_from(
                    t.level,
                    t.ents,
                    t.env,
                    t.cx,
                    target,
                    EffectInstance::new(MobEffect::LEVITATION, 200, 0),
                    by,
                );
            }
        }
        Hit::Block(..) => {
            let pos = t.ents.store.get(me).map_or(Vec3::ZERO, |e| e.pos());
            let explosion = crate::level::particles::simple("minecraft:explosion");
            t.cx.send_particles(explosion, pos, 2, Vec3::new(0.2, 0.2, 0.2), 0.0);
            let seed = crate::survival::sound_seed();
            crate::survival::play_sound(
                t.env.out,
                None,
                pos,
                "minecraft:entity.shulker_bullet.hit",
                vanillite_protocol::packets::play::clientbound::SoundSource::Hostile,
                1.0,
                1.0,
                seed,
            );
        }
        Hit::Miss => return,
    }
    land(t, hit);
    discard(t);
}

/// `ShulkerBullet.hurtServer`: any hit makes the hurt sound and crit particles and destroys it
/// (`destroy`: gone, with the `ENTITY_DAMAGE` game event).
pub fn hurt_bullet(
    ents: &mut Entities,
    env: &mut crate::survival::player::Env<'_>,
    cx: &mut super::super::super::MobContext,
    id: EntityId,
) -> bool {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return false;
    };
    crate::survival::play_sound(
        env.out,
        None,
        pos,
        "minecraft:entity.shulker_bullet.hurt",
        vanillite_protocol::packets::play::clientbound::SoundSource::Hostile,
        1.0,
        1.0,
        crate::survival::sound_seed(),
    );
    let crit = crate::level::particles::simple("minecraft:crit");
    cx.send_particles(crit, pos, 15, Vec3::new(0.2, 0.2, 0.2), 0.0);
    if let Some(d) = ents.missile_mut(id) {
        d.removed = true;
    }
    cx.discarded.push(id);
    crate::survival::game_event(
        env.out,
        crate::level::game_event::events().entity_damage,
        pos,
        Some(id),
    );
    true
}

/// `Projectile.onHitBlock`'s `onProjectileHit` of a block hit (comes first in `onHitBlock`).
fn hit_block(t: &mut MissileTick<'_, '_>, hit: Hit) {
    if let Hit::Block(pos, at, _) = hit {
        crate::mob::missile::hit_block(t.level, t.cx, t.id, pos, at);
    }
}

/// `Projectile.onHit`'s `PROJECTILE_LAND` of this projectile's hit.
fn land(t: &mut MissileTick<'_, '_>, hit: Hit) {
    let (block, at) = match hit {
        Hit::Entity(_, at) => (None, at),
        Hit::Block(pos, at, _) => (Some(pos), at),
        Hit::Miss => return,
    };
    let owner = t.ents.missile(t.id).and_then(|d| d.owner);
    crate::mob::missile::land_event(t.env.out, t.level, t.id, owner, block, at);
}

// --- Evoker fangs ---------------------------------------------------------------------------

/// `EvokerFangs.tick`.
fn fangs_tick(t: &mut MissileTick<'_, '_>) {
    let me = t.id;
    let Some(mut d) = t.ents.missile(me).cloned() else {
        return;
    };
    d.ints[0] -= 1;
    if d.ints[0] < 0 {
        if d.ints[0] == -8 {
            fangs_bite(t, &d);
        }
        if d.ints[2] == 0 {
            if let Some(e) = t.ents.store.get(me) {
                let net = e.network_id().0;
                t.env.out.push(crate::survival::Effect::Tracking {
                    entity: me,
                    with_self: false,
                    packet: vanillite_protocol::SharedPacket::of(
                        &vanillite_protocol::packets::play::clientbound::EntityEvent {
                            entity_id: net,
                            event: 4,
                        },
                    ),
                });
            }
            d.ints[2] = 1;
        }
        d.ints[1] -= 1;
        if d.ints[1] < 0 {
            d.removed = true;
        }
    }
    if let Some(slot) = t.ents.missile_mut(me) {
        let removed = slot.removed || d.removed;
        *slot = d;
        slot.removed = removed;
    }
}

/// `dealDamageTo` of everything living in the fangs.
fn fangs_bite(t: &mut MissileTick<'_, '_>, d: &MissileData) {
    let me = t.id;
    let Some(area) = t
        .ents
        .store
        .get(me)
        .map(|e| e.aabb().inflate(0.2, 0.0, 0.2))
    else {
        return;
    };
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&area, false, |id| ids.push(id));
    let owner_id = owner(t.ents, d);
    let owner_kind = owner_id.and_then(|o| t.ents.mob(o)).map(|m| m.kind);
    for &target in &ids {
        if Some(target) == owner_id || target == me {
            continue;
        }
        let alive = t
            .ents
            .mob(target)
            .is_some_and(super::super::super::MobData::is_alive)
            || t.ents.player(target).is_some_and(|p| {
                !p.survival.is_dead_or_dying() && p.game_mode != GameMode::Spectator
            });
        if !alive {
            continue;
        }
        let src = if owner_id.is_none() {
            DamageSource::of(t.env.types.magic)
        } else {
            // `isAlliedTo`: illagers spare illagers and the evoker's own vexes.
            if let Some(ok) = owner_kind
                && is_illager(ok)
                && let Some(m) = t.ents.mob(target)
                && (is_illager(m.kind)
                    || (m.kind == Kind::Vex
                        && st(m).owner.is_some()
                        && st(m).owner == d.owner_uuid))
            {
                continue;
            }
            source(t.ents, t.env.types.indirect_magic, me, d)
        };
        if combat::hurt(t.level, t.ents, t.env, t.cx, target, src, 6.0) && src.cause.is_some() {
            crate::mob::attack::post_attack_effects(t.level, t.ents, t.env, t.cx, target, &src);
        }
    }
    t.cx.ids = ids;
}

// --- Thrown potions ---------------------------------------------------------------------------

/// `ThrowableProjectile.tick` of a thrown potion.
fn potion_tick(t: &mut MissileTick<'_, '_>) {
    let me = t.id;
    let Some(mut d) = t.ents.missile(me).cloned() else {
        return;
    };
    let Some(e) = t.ents.store.get(me) else {
        return;
    };
    let pos = e.pos();
    let mut v = e.velocity();
    v = v + Vec3::new(0.0, -0.05, 0.0);
    let in_water = is_water(t.level.block(BlockPos::containing(pos.x, pos.y, pos.z)));
    v = v * f64::from(if in_water { 0.8_f32 } else { 0.99_f32 });
    let hit = hit_on_move(t.level, t.ents, me, &d, pos, v);
    let new_pos = hit.location().unwrap_or(pos + v);
    place(t.ents, me, new_pos, v);
    check_left_owner(t.ents, me, &mut d);
    if let Some(slot) = t.ents.missile_mut(me) {
        *slot = d.clone();
    }
    if hit != Hit::Miss && !deflected(t, hit, false) {
        potion_hit(t, &d, hit, new_pos);
    }
}

/// `AbstractThrownPotion.onHit` and `ThrownSplashPotion.onHitAsPotion`.
fn potion_hit(t: &mut MissileTick<'_, '_>, d: &MissileData, hit: Hit, at: Vec3) {
    hit_block(t, hit);
    let me = t.id;
    // `super.onHit` first: the land game event.
    land(t, hit);
    let potion = potion::of_stack(&d.item);
    if let Some(p) = potion
        && d.kind == MissileKind::SplashPotion
    {
        splash(t, d, p, at);
    }
    let block = BlockPos::containing(at.x, at.y, at.z);
    let (event, sound) = if potion.is_some_and(Potion::is_instant) {
        (2007, 1054)
    } else {
        (2002, 1053)
    };
    let color = potion.map_or(0x38_5D_C6, Potion::color);
    t.cx.actions
        .push(WorldAction::LevelEvent(event, block, color));
    t.cx.actions.push(WorldAction::LevelEvent(sound, block, 0));
    let _ = me;
    discard(t);
}

/// `onHitAsPotion` of a splash potion: the effects on the living around the impact.
fn splash(t: &mut MissileTick<'_, '_>, d: &MissileData, p: Potion, at: Vec3) {
    let me = t.id;
    let Some(e) = t.ents.store.get(me) else {
        return;
    };
    let old = e.pos();
    let bb = e.aabb();
    let off = at - old;
    let potion_aabb = Aabb::new(
        bb.min_x + off.x,
        bb.min_y + off.y,
        bb.min_z + off.z,
        bb.max_x + off.x,
        bb.max_y + off.y,
        bb.max_z + off.z,
    );
    let area = potion_aabb.inflate(4.0, 2.0, 4.0);
    let m = margin(d.life);
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&area, false, |id| ids.push(id));
    let owner_id = owner(t.ents, d);
    for &target in &ids {
        let Some(te) = t.ents.store.get(target) else {
            continue;
        };
        let living = t
            .ents
            .mob(target)
            .is_some_and(super::super::super::MobData::is_alive)
            || t.ents.player(target).is_some();
        if !living {
            continue;
        }
        let dist = aabb_distance_sqr(&potion_aabb, &te.aabb().inflate(m, m, m));
        if dist >= 16.0 {
            continue;
        }
        let scale = 1.0 - dist.sqrt() / 4.0;
        for effect in p.effects() {
            if matches!(
                effect.effect,
                MobEffect::INSTANT_HEALTH | MobEffect::INSTANT_DAMAGE
            ) {
                instant(t, d, owner_id, target, effect, scale);
            } else {
                #[allow(clippy::cast_possible_truncation)]
                let duration = (scale * f64::from(effect.duration) + 0.5) as i32;
                if duration > 20 {
                    // `getEffectSource()`: the owner, else the potion.
                    let by = owner_id.or(Some(t.id));
                    effects::living::add_to_entity_from(
                        t.level,
                        t.ents,
                        t.env,
                        t.cx,
                        target,
                        EffectInstance { duration, ..effect },
                        by,
                    );
                }
            }
        }
    }
    t.cx.ids = ids;
}

/// `HealOrHarmMobEffect.applyInstantenousEffect`.
fn instant(
    t: &mut MissileTick<'_, '_>,
    d: &MissileData,
    owner_id: Option<EntityId>,
    target: EntityId,
    effect: EffectInstance,
    scale: f64,
) {
    let harm = effect.effect == MobEffect::INSTANT_DAMAGE;
    let undead = t
        .ents
        .mob(target)
        .is_some_and(|m| effects::inverted_heal_and_harm(m.kind.type_id()));
    let amp = u32::from(effect.amplifier.min(20));
    if harm == undead {
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let amount = (scale * f64::from(4u32 << amp) + 0.5) as i32 as f32;
        if let Some(m) = t.ents.mob_mut(target) {
            if m.health > 0.0 {
                m.health = (m.health + amount).min(m.max_health());
            }
        } else if t.ents.player(target).is_some() {
            let Entities { store, players, .. } = &mut *t.ents;
            if let Some(mut e) = store.get_mut(target) {
                let row = e.as_ref().kind_row();
                if let Some(p) = players.get_mut(row) {
                    crate::survival::player::heal(&mut e, p, amount);
                }
            }
        }
        if let Some(mut e) = t.ents.store.get_mut(target) {
            e.mark_data_dirty();
        }
    } else {
        // A witch's own potion does nothing to it (`getDamageAfterMagicAbsorb`).
        if owner_id == Some(target) {
            return;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let amount = (scale * f64::from(6u32 << amp) + 0.5) as i32 as f32;
        let src = source(t.ents, t.env.types.indirect_magic, t.id, d);
        combat::hurt(t.level, t.ents, t.env, t.cx, target, src, amount);
    }
}

/// `AABB.distanceToSqr(other)`: the squared gap between two boxes.
fn aabb_distance_sqr(a: &Aabb, b: &Aabb) -> f64 {
    let gap = |amin: f64, amax: f64, bmin: f64, bmax: f64| (amin - bmax).max(0.0).max(bmin - amax);
    let dx = gap(a.min_x, a.max_x, b.min_x, b.max_x);
    let dy = gap(a.min_y, a.max_y, b.min_y, b.max_y);
    let dz = gap(a.min_z, a.max_z, b.min_z, b.max_z);
    dx * dx + dy * dy + dz * dz
}

// --- Storage and client data ------------------------------------------------------------------

/// The projectile's own NBT.
pub fn save(d: &MissileData, out: &mut Compound) {
    match d.kind {
        MissileKind::SmallFireball | MissileKind::Fireball => {
            out.insert("acceleration_power", d.acceleration);
            if d.kind == MissileKind::Fireball {
                out.insert(
                    "ExplosionPower",
                    i8::try_from(d.explosion_power).unwrap_or(1),
                );
            }
        }
        MissileKind::ShulkerBullet => {
            if let Some(u) = d.target_uuid {
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let ints = vec![
                    (u >> 96) as i32,
                    (u >> 64) as i32,
                    (u >> 32) as i32,
                    u as i32,
                ];
                out.insert("Target", ints);
            }
            if d.ints[0] != NO_DIR {
                out.insert("Dir", d.ints[0]);
            }
            out.insert("Steps", d.ints[1]);
            out.insert("TXD", d.vec.x);
            out.insert("TYD", d.vec.y);
            out.insert("TZD", d.vec.z);
        }
        MissileKind::EvokerFangs => out.insert("Warmup", d.ints[0]),
        _ => {}
    }
}

/// Reads the projectile's own NBT.
pub fn load(d: &mut MissileData, tag: &Compound) {
    let double = |k: &str| crate::mob::entity_nbt::get_double(tag, k);
    let int = |k: &str| crate::mob::entity_nbt::get_int(tag, k);
    match d.kind {
        MissileKind::SmallFireball | MissileKind::Fireball => {
            d.acceleration = double("acceleration_power").unwrap_or(0.1);
            d.explosion_power = int("ExplosionPower").unwrap_or(1);
            if d.item.is_empty() {
                d.item = super::super::super::equipment::stack("minecraft:fire_charge");
            }
        }
        MissileKind::ShulkerBullet => {
            d.target_uuid = super::super::super::nbt::uuid_key_of(tag, "Target");
            d.ints[0] = int("Dir").filter(|v| (0..6).contains(v)).unwrap_or(NO_DIR);
            d.ints[1] = int("Steps").unwrap_or(0);
            d.vec = Vec3::new(
                double("TXD").unwrap_or(0.0),
                double("TYD").unwrap_or(0.0),
                double("TZD").unwrap_or(0.0),
            );
        }
        MissileKind::EvokerFangs => {
            d.ints[0] = int("Warmup").unwrap_or(0);
            d.ints[1] = 22;
        }
        _ => {}
    }
}

/// The projectile's synched data: the shown item of fireballs and potions (id 8).
pub fn encode(w: &mut DataWriter<'_>, d: &MissileData) {
    match d.kind {
        MissileKind::SmallFireball
        | MissileKind::Fireball
        | MissileKind::SplashPotion
        | MissileKind::LingeringPotion => w.stack(8, &d.item),
        _ => {}
    }
}

// --- Player attacks -----------------------------------------------------------------------

/// `Player.attack` of a projectile: a fireball or wind charge (`#redirectable_projectile`) is
/// sent along the player's look (`deflectProjectile`, `AIM_DEFLECT`) and becomes the player's;
/// a shulker bullet breaks (`ShulkerBullet.hurtServer`); others take no damage.
pub fn attacked_by_player(t: &mut MissileTick<'_, '_>, player: EntityId) {
    let Some(kind) = t.ents.missile(t.id).map(|d| d.kind) else {
        return;
    };
    // Only these are attackable and take part (`AbstractArrow.isAttackable` is false).
    if !matches!(
        kind,
        MissileKind::Fireball
            | MissileKind::SmallFireball
            | MissileKind::WindCharge
            | MissileKind::BreezeWindCharge
            | MissileKind::ShulkerBullet
    ) {
        return;
    }
    // `onAttack`: `resetOnlyAttackStrengthTicker`.
    if let Some(p) = t.ents.player_mut(player) {
        p.survival.attack_ticker = 0;
    }
    let Some((ppos, rot, uuid)) = t
        .ents
        .store
        .get(player)
        .map(|e| (e.pos(), e.rot(), e.uuid()))
    else {
        return;
    };
    let nodamage = |t: &mut MissileTick<'_, '_>| {
        let seed = crate::survival::sound_seed();
        crate::survival::play_sound(
            t.env.out,
            None,
            ppos,
            "minecraft:entity.player.attack.nodamage",
            vanillite_protocol::packets::play::clientbound::SoundSource::Players,
            1.0,
            1.0,
            seed,
        );
    };
    if matches!(
        kind,
        MissileKind::Fireball | MissileKind::WindCharge | MissileKind::BreezeWindCharge
    ) {
        let look = crate::mob::randompos::view_vector(rot.x_rot, rot.y_rot);
        if let Some(mut e) = t.ents.store.get_mut(t.id) {
            e.set_velocity(look);
        }
        if let Some(d) = t.ents.missile_mut(t.id) {
            d.owner = Some(player);
            d.owner_uuid = Some(uuid);
            // `AbstractHurtingProjectile.onDeflection(true)`.
            d.acceleration = 0.1;
        }
        nodamage(t);
        return;
    }
    if kind != MissileKind::ShulkerBullet {
        // A small fireball: `AbstractHurtingProjectile.hurtServer` is false.
        nodamage(t);
        return;
    }
    let pos = t.ents.store.get(t.id).map_or(Vec3::ZERO, |e| e.pos());
    let seed = crate::survival::sound_seed();
    crate::survival::play_sound(
        t.env.out,
        None,
        pos,
        "minecraft:entity.shulker_bullet.hurt",
        vanillite_protocol::packets::play::clientbound::SoundSource::Hostile,
        1.0,
        1.0,
        seed,
    );
    let crit = crate::level::particles::simple("minecraft:crit");
    t.cx.send_particles(crit, pos, 15, Vec3::new(0.2, 0.2, 0.2), 0.0);
    // `destroy`: discarded with an `ENTITY_DAMAGE` game event.
    let event = crate::level::game_event::events().entity_damage;
    crate::survival::game_event(t.env.out, event, pos, Some(t.id));
    discard(t);
    t.cx.discarded.push(t.id);
    // The hit counts (`causeFoodExhaustion(0.1F)`).
    if let Some(p) = t.ents.player_mut(player) {
        crate::survival::player::cause_food_exhaustion(p, 0.1);
    }
}
