//! `Llama` and `TraderLlama`: caravans, spitting (`LlamaSpit`), wolf hostility, strength and
//! the trader llama's despawning.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_data::entity::ids;
use vanillite_world::entity::{EntityId, Rotation};

use super::{G, Goal as FamGoal, SpawnContext, TemptItems, common, fam, st, st_mut};
use crate::entities::Entities;
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::common::{CommonGoal, TargetFilter};
use crate::mob::fam::{GroupData, Interact, SpawnReason};
use crate::mob::goals::{TargetState, flag, reduced};
use crate::mob::kinds::Kind;
use crate::mob::missile::{MissileData, MissileKind, MissileTick};
use crate::mob::physics::{self, Blocks};
use crate::mob::{MobData, Tick, nav};

/// The llama goals.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// `LlamaFollowCaravanGoal`.
    FollowCaravan { speed: f64, dist_check: i32 },
    /// `LlamaAttackWolfGoal`: untamed wolves within a quarter of the follow range.
    AttackWolf {
        state: TargetState,
        found: Option<EntityId>,
    },
    /// `TraderLlamaDefendWanderingTraderGoal`.
    DefendTrader {
        state: TargetState,
        timestamp: i32,
        found: Option<EntityId>,
    },
}

pub fn flags(g: &Goal) -> u8 {
    match g {
        Goal::FollowCaravan { .. } => flag::MOVE,
        Goal::AttackWolf { .. } | Goal::DefendTrader { .. } => flag::TARGET,
    }
}

fn is_llama(ents: &Entities, id: EntityId) -> bool {
    ents.mob(id)
        .is_some_and(|m| matches!(m.kind, Kind::Llama | Kind::TraderLlama) && m.is_alive())
}

fn wolf_not_tame(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .mob(id)
        .is_some_and(|m| m.kind == Kind::Wolf && !m.tame)
}

fn zombie_not_piglin(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.mob(id).is_some_and(|m| {
        matches!(
            m.kind,
            Kind::Zombie | Kind::Husk | Kind::Drowned | Kind::ZombieVillager
        )
    })
}

fn illager(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.mob(id).is_some_and(|m| {
        matches!(
            m.kind,
            Kind::Pillager | Kind::Vindicator | Kind::Evoker | Kind::Illusioner
        )
    })
}

/// `registerGoals` of the llamas.
pub fn register_goals(m: &mut MobData) {
    let trader = m.kind == Kind::TraderLlama;
    let g = &mut m.goals;
    g.add(0, G::Float);
    g.add(
        1,
        fam(FamGoal::Horse(super::horse::Goal::RunAround {
            speed: 1.2,
            pos: Vec3::ZERO,
        })),
    );
    g.add(
        2,
        fam(FamGoal::Llama(Goal::FollowCaravan {
            speed: 2.1,
            dist_check: 0,
        })),
    );
    g.add(3, common(CommonGoal::ranged(1.25, 40, 40, 20.0)));
    g.add(3, G::panic(1.2));
    g.add(4, G::breed(1.0));
    g.add(
        5,
        G::tempt(1.25, TemptItems::Tag("minecraft:llama_tempt_items")),
    );
    g.add(
        6,
        G::FollowParent {
            speed: 1.0,
            parent: None,
            recalc: 0,
        },
    );
    g.add(7, G::stroll(0.7));
    g.add(8, G::look_at_player(6.0));
    g.add(9, G::random_look());
    let tg = &mut m.targets;
    tg.add(1, G::hurt_by(false));
    // `LlamaAttackWolfGoal`: untamed wolves within a quarter of the follow range.
    tg.add(
        2,
        fam(FamGoal::Llama(Goal::AttackWolf {
            state: TargetState::new(false),
            found: None,
        })),
    );
    if trader {
        m.goals.add(1, G::panic(2.0));
        let tg = &mut m.targets;
        tg.add(
            1,
            fam(FamGoal::Llama(Goal::DefendTrader {
                state: TargetState::new(false),
                timestamp: 0,
                found: None,
            })),
        );
        let zombies: TargetFilter = zombie_not_piglin;
        let illagers: TargetFilter = illager;
        tg.add(2, common(CommonGoal::nearest(10, true, zombies)));
        tg.add(2, common(CommonGoal::nearest(10, true, illagers)));
    }
    m.nav.can_float = true;
    let follow = m.attrs.follow_range;
    m.nav.set_required_path_length(40.0, follow);
}

/// `finalizeSpawn` of the llamas: strength, variant (shared by the group), babies.
pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    let max = if m.random.next_float() < 0.04 { 5 } else { 3 };
    let strength = 1 + m.random.next_int_bounded(max);
    st_mut(m).strength = strength.clamp(1, 5);
    m.variant = if let GroupData::Llama(v) = ctx.group {
        v
    } else {
        let v = m.random.next_int_bounded(4);
        ctx.group = GroupData::Llama(v);
        v
    };
    let trader = m.kind == Kind::TraderLlama;
    if trader && ctx.reason == SpawnReason::Event {
        m.age = 0;
    }
    super::horse::randomize_attributes(m);
    // `AgeableMobGroupData(false)` for traders, `LlamaGroupData` (babies) otherwise.
    super::ageable_group(m, ctx, !trader, 0.05);
}

/// `Llama.spit` (`performRangedAttack`).
pub fn spit(t: &mut Tick<'_, '_>, target: EntityId, _power: f32) {
    let Some(o) = t.other(target) else {
        return;
    };
    let p = t.pos();
    let width = f64::from(t.core().dims().width);
    let body = f64::from(t.mob_ref().y_body_rot) * std::f64::consts::PI / 180.0;
    let start = Vec3::new(
        p.x - (width + 1.0) * 0.5 * body.sin(),
        t.eye_y() - f64::from(0.1_f32),
        p.z + (width + 1.0) * 0.5 * body.cos(),
    );
    let xd = o.pos.x - p.x;
    let yd = o.pos.y + f64::from(o.height) * 0.333_333_333_333_333_3 - start.y;
    let zd = o.pos.z - p.z;
    let yo = (xd * xd + zd * zd).sqrt() * f64::from(0.2_f32);
    let dir = Vec3::new(xd, yd + yo, zd);
    let mut own = crate::mob::missile::new_projectile_random();
    let (velocity, rot) = crate::mob::projectile::shoot(&mut own, dir, 1.5, 10.0);
    let data = MissileData::new(MissileKind::LlamaSpit);
    crate::mob::missile::spawn_shot(t, start, velocity, rot, data);
    let pitch = {
        let r = &mut t.mob().random;
        1.0 + (r.next_float() - r.next_float()) * 0.2
    };
    crate::mob::kinds::play_sound(t, "minecraft:entity.llama.spit", 1.0, pitch);
    st_mut(t.mob()).did_spit = true;
    stop_hurt_by_after_spit(t);
}

/// `LlamaHurtByTargetGoal.canContinueToUse`: a spit ends the revenge.
fn stop_hurt_by_after_spit(t: &mut Tick<'_, '_>) {
    let m = t.mob();
    let running = m
        .targets
        .slots
        .iter()
        .position(|s| s.running && matches!(s.goal, crate::mob::goals::Goal::HurtBy { .. }));
    if let Some(i) = running {
        st_mut(m).did_spit = false;
        m.targets.slots[i].running = false;
        m.target = None;
    }
}

/// `LlamaSpit.tick`: flies with drag and gravity, hurts the first entity it touches (1) and
/// vanishes in blocks and water.
pub fn spit_tick(t: &mut MissileTick<'_, '_>) {
    let Some(d) = t.ents.missile(t.id).cloned() else {
        return;
    };
    if d.kind != MissileKind::LlamaSpit {
        return;
    }
    let Some(e) = t.ents.store.get(t.id) else {
        return;
    };
    let pos = e.pos();
    let velocity = e.velocity();
    let aabb = e.aabb();
    let mut left_owner = d.left_owner;
    if !left_owner {
        let area = aabb
            .expand_towards(velocity.x, velocity.y, velocity.z)
            .inflate(1.0, 1.0, 1.0);
        left_owner = !d.owner.is_some_and(|o| {
            t.ents
                .store
                .get(o)
                .is_some_and(|oe| oe.aabb().intersects(&area))
        });
    }
    let to = pos + velocity;
    let block_hit = physics::clip(t.level as &dyn Blocks, pos, to);
    let end = block_hit.map_or(to, |(_, p)| p);
    let margin = crate::mob::missile::margin(d.life);
    let hit = first_entity_hit(
        t.ents, t.id, d.owner, left_owner, pos, end, aabb, velocity, margin,
    );
    if let Some(m) = t.ents.missile_mut(t.id) {
        m.left_owner = left_owner;
    }
    // Bukkit's `ProjectileHitEvent`: a cancelled hit is ignored, the spit flies on.
    let hit_event = |what| crate::mob::missile::hit_event(t.level, t.ents, t.id, d.owner, what);
    let hit = hit.filter(|&target| hit_event(crate::mob::missile::HitWhat::Entity(target)));
    let block_hit = block_hit.filter(|&(block, at)| {
        hit.is_some() || hit_event(crate::mob::missile::HitWhat::Block(block, at))
    });
    if let Some(target) = hit {
        if let Some(owner) = d.owner.filter(|o| {
            t.ents.mob(*o).is_some_and(MobData::is_alive) || t.ents.player(*o).is_some()
        }) {
            let kind = t.env.types.named("minecraft:spit");
            let source = crate::mob::combat::entity_source(t.ents, kind, owner, t.id);
            if crate::mob::combat::hurt(t.level, t.ents, t.env, t.cx, target, source, 1.0) {
                crate::mob::attack::post_attack_effects(
                    t.level, t.ents, t.env, t.cx, target, &source,
                );
            }
        }
        crate::mob::missile::land_event(t.env.out, t.level, t.id, d.owner, None, end);
    } else if let Some((block, at)) = block_hit {
        crate::mob::missile::hit_block(t.level, t.cx, t.id, block, at);
        crate::mob::missile::land_event(t.env.out, t.level, t.id, d.owner, Some(block), at);
        if let Some(m) = t.ents.missile_mut(t.id) {
            m.removed = true;
        }
        return;
    }
    // The position update.
    let next = pos + velocity;
    let new_box = Aabb::new(
        aabb.min_x, aabb.min_y, aabb.min_z, aabb.max_x, aabb.max_y, aabb.max_z,
    );
    let touches = touches_block(t.level, &new_box);
    let in_water = crate::mob::is_water(t.level.block(BlockPos::containing(pos.x, pos.y, pos.z)));
    if touches || in_water {
        if let Some(m) = t.ents.missile_mut(t.id) {
            m.removed = true;
        }
        return;
    }
    let v = velocity * f64::from(0.99_f32);
    let v = Vec3::new(v.x, v.y - 0.06, v.z);
    if let Some(mut e) = t.ents.store.get_mut(t.id) {
        e.set_velocity(v);
        e.set_pos(next);
        let hd = (velocity.x * velocity.x + velocity.z * velocity.z).sqrt();
        #[allow(clippy::cast_possible_truncation)]
        e.set_rot(Rotation {
            y_rot: crate::mob::atan2_deg(velocity.x, velocity.z) as f32,
            x_rot: crate::mob::atan2_deg(velocity.y, hd) as f32,
            y_head_rot: 0.0,
        });
    }
}

/// Whether a non-air block is in the box (`findBlocksIn(...).filter(!air)`).
fn touches_block(level: &Level, b: &Aabb) -> bool {
    let (x0, y0, z0) = (
        b.min_x.floor() as i32,
        b.min_y.floor() as i32,
        b.min_z.floor() as i32,
    );
    let (x1, y1, z1) = (
        b.max_x.floor() as i32,
        b.max_y.floor() as i32,
        b.max_z.floor() as i32,
    );
    for x in x0..=x1 {
        for y in y0..=y1 {
            for z in z0..=z1 {
                if !level.block(BlockPos::new(x, y, z)).is_air() {
                    return true;
                }
            }
        }
    }
    false
}

/// `ProjectileUtil.getEntityHitResult` along the move: the nearest living entity whose box
/// (grown by `margin`) the segment crosses, the owner excluded until the projectile left it.
#[allow(clippy::too_many_arguments)]
pub fn first_entity_hit(
    ents: &Entities,
    me: EntityId,
    owner: Option<EntityId>,
    left_owner: bool,
    from: Vec3,
    to: Vec3,
    aabb: Aabb,
    velocity: Vec3,
    margin: f64,
) -> Option<EntityId> {
    let area = aabb
        .expand_towards(velocity.x, velocity.y, velocity.z)
        .inflate(1.0, 1.0, 1.0);
    let mut best: Option<(EntityId, f64)> = None;
    ents.store.for_each_in(&area, false, |other| {
        if other == me || (!left_owner && owner == Some(other)) {
            return;
        }
        let alive = ents.mob(other).is_some_and(MobData::is_alive)
            || ents.player(other).is_some_and(|p| {
                !p.survival.is_dead_or_dying()
                    && p.game_mode != vanillite_config::GameMode::Spectator
            });
        if !alive {
            return;
        }
        let Some(oe) = ents.store.get(other) else {
            return;
        };
        let grown = oe.aabb().inflate(margin, margin, margin);
        if crate::mob::projectile::segment_box(from, to, &grown).is_some() {
            let d = from.distance_to_sqr(oe.pos());
            if best.is_none_or(|(_, b)| d < b) {
                best = Some((other, d));
            }
        }
    });
    best.map(|(id, _)| id)
}

/// `getBreedOffspring` of the llamas: strength from the parents, a parent's variant.
pub fn breed_offspring(
    parent: &MobData,
    partner: &MobData,
    child: &mut MobData,
    r: &mut LegacyRandom,
) {
    super::horse::offspring_attributes(parent, partner, child, r);
    let mut strength = r.next_int_bounded(st(parent).strength.max(st(partner).strength).max(1)) + 1;
    if r.next_float() < 0.03 {
        strength += 1;
    }
    st_mut(child).strength = strength.clamp(1, 5);
    child.variant = if r.next_boolean() {
        parent.variant
    } else {
        partner.variant
    };
    if child.kind == Kind::TraderLlama {
        child.persistent = true;
    }
}

/// `Llama.mobInteract` is the chested horse's.
pub fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
    super::horse::interact(ix)
}

/// Whether the llama is leashed to a wandering trader.
pub fn leashed_to_trader(ents: &Entities, llama: EntityId) -> bool {
    ents.leashes
        .get(&llama)
        .and_then(|&h| ents.store.get(h))
        .is_some_and(|e| e.kind() == ids::WANDERING_TRADER)
}

/// `TraderLlama.maybeDespawn`.
pub fn trader_despawn(t: &mut Tick<'_, '_>) {
    let holder = t.ents.leashes.get(&t.id).copied();
    let to_trader = leashed_to_trader(t.ents, t.id);
    let other_leash = holder.is_some() && !to_trader;
    let one_player =
        t.ents.riding.passengers_of(t.id).len() == 1 && super::first_player_passenger(t).is_some();
    let m = t.mob_ref();
    if m.tame || other_leash || one_player || m.persistent {
        return;
    }
    // A llama leashed to its trader despawns with it.
    let trader_delay = holder
        .filter(|_| to_trader)
        .and_then(|h| t.ents.mob(h))
        .map(|m| crate::mob::fam::village::state(m).trader.despawn_delay);
    let m = t.mob();
    let s = st_mut(m);
    s.despawn_delay = match trader_delay {
        Some(d) => d - 1,
        None => s.despawn_delay - 1,
    };
    if s.despawn_delay <= 0 {
        m.removed = true;
        crate::mob::tame::drop_leash(t.ents, t.env, t.id, false);
    }
}

/// `Llama` spawn rules: the animal rules.
pub fn spawn_rules(level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    super::animal_spawn_rules(level, pos, reason)
}

// --- Caravans -----------------------------------------------------------------------------

fn head(ents: &Entities, id: EntityId) -> Option<EntityId> {
    ents.mob(id)
        .and_then(|m| st(m).caravan_head)
        .filter(|&h| is_llama(ents, h))
}

fn has_tail(ents: &Entities, id: EntityId) -> bool {
    ents.mob(id)
        .and_then(|m| st(m).caravan_tail)
        .is_some_and(|t| is_llama(ents, t))
}

fn leashed(ents: &Entities, id: EntityId) -> bool {
    ents.leashes.contains_key(&id)
}

/// `firstIsLeashed`.
fn first_is_leashed(ents: &Entities, mut id: EntityId, mut counter: i32) -> bool {
    loop {
        if counter > 8 {
            return false;
        }
        let Some(h) = head(ents, id) else {
            return false;
        };
        if leashed(ents, h) {
            return true;
        }
        counter += 1;
        id = h;
    }
}

fn leave_caravan(ents: &mut Entities, id: EntityId) {
    let h = ents.mob(id).and_then(|m| st(m).caravan_head);
    if let Some(h) = h
        && let Some(hm) = ents.mob_mut(h)
    {
        st_mut(hm).caravan_tail = None;
    }
    if let Some(m) = ents.mob_mut(id) {
        st_mut(m).caravan_head = None;
    }
}

pub fn can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::FollowCaravan { .. } => {
            if leashed(t.ents, t.id) || head(t.ents, t.id).is_some() {
                return false;
            }
            let area = t.aabb().inflate(9.0, 4.0, 9.0);
            let mut llamas = std::mem::take(&mut t.cx.ids);
            llamas.clear();
            let me = t.id;
            let ents = &*t.ents;
            ents.store.for_each_in(&area, false, |id| {
                if id != me && is_llama(ents, id) {
                    llamas.push(id);
                }
            });
            let pos = t.pos();
            let dist = |id: EntityId| {
                t.ents
                    .store
                    .get(id)
                    .map_or(f64::MAX, |e| e.pos().distance_to_sqr(pos))
            };
            let mut closest: Option<(EntityId, f64)> = None;
            for &c in &llamas {
                if head(t.ents, c).is_some() && !has_tail(t.ents, c) {
                    let d = dist(c);
                    if closest.is_none_or(|(_, b)| d <= b) {
                        closest = Some((c, d));
                    }
                }
            }
            if closest.is_none() {
                for &c in &llamas {
                    if leashed(t.ents, c) && !has_tail(t.ents, c) {
                        let d = dist(c);
                        if closest.is_none_or(|(_, b)| d <= b) {
                            closest = Some((c, d));
                        }
                    }
                }
            }
            t.cx.ids = llamas;
            let Some((c, d)) = closest else {
                return false;
            };
            if d < 4.0 || (!leashed(t.ents, c) && !first_is_leashed(t.ents, c, 1)) {
                return false;
            }
            let me = t.id;
            st_mut(t.mob()).caravan_head = Some(c);
            if let Some(cm) = t.ents.mob_mut(c) {
                st_mut(cm).caravan_tail = Some(me);
            }
            true
        }
        Goal::AttackWolf { found, .. } => {
            if t.mob().random.next_int_bounded(reduced(16)) != 0 {
                return false;
            }
            let range = t.mob_ref().attrs.follow_range * 0.25;
            let area = t.aabb().inflate(range, range, range);
            let wolves: TargetFilter = wolf_not_tame;
            *found = crate::mob::common::find_nearest(t, wolves, None, range, false, false, area);
            found.is_some()
        }
        Goal::DefendTrader {
            found, timestamp, ..
        } => {
            let Some(&holder) = t.ents.leashes.get(&t.id) else {
                return false;
            };
            if !t
                .ents
                .store
                .get(holder)
                .is_some_and(|e| e.kind() == ids::WANDERING_TRADER)
            {
                return false;
            }
            // The trader's attacker (`getLastHurtByMob`), once per hit, if it can be attacked
            // (`TargetingConditions.DEFAULT`: no range, line of sight).
            let Some((by, stamp)) = t
                .ents
                .mob(holder)
                .map(|m| (m.last_hurt_by_mob, m.last_hurt_by_mob_time))
            else {
                return false;
            };
            *found = by;
            let Some(by) = by else {
                return false;
            };
            if stamp == *timestamp {
                return false;
            }
            if t.ents.player(by).is_some() {
                crate::mob::goals::test_player(t, by, true, -1.0, true)
            } else {
                crate::mob::goals::can_attack(t, by) && crate::mob::living::has_line_of_sight(t, by)
            }
        }
    }
}

pub fn can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::FollowCaravan { speed, dist_check } => {
            let Some(h) = head(t.ents, t.id) else {
                return false;
            };
            if !first_is_leashed(t.ents, t.id, 0) {
                return false;
            }
            let hp = t.ents.store.get(h).map_or(t.pos(), |e| e.pos());
            if hp.distance_to_sqr(t.pos()) > 676.0 {
                if *speed <= 3.0 {
                    *speed *= 1.2;
                    *dist_check = reduced(40);
                    return true;
                }
                if *dist_check == 0 {
                    return false;
                }
            }
            if *dist_check > 0 {
                *dist_check -= 1;
            }
            true
        }
        Goal::AttackWolf { state, .. } | Goal::DefendTrader { state, .. } => {
            crate::mob::goals::target_can_continue(t, state)
        }
    }
}

pub fn start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    if let Goal::DefendTrader { timestamp, .. } = g
        && let Some(holder) = t.ents.leashes.get(&t.id).copied()
        && let Some(m) = t.ents.mob(holder)
        && m.kind == crate::mob::Kind::WanderingTrader
    {
        *timestamp = m.last_hurt_by_mob_time;
    }
    if let Goal::AttackWolf { found, state } | Goal::DefendTrader { found, state, .. } = g {
        crate::mob::goals::set_target(t, *found);
        state.unseen_ticks = 0;
    }
}

pub fn stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::FollowCaravan { speed, .. } => {
            let me = t.id;
            leave_caravan(t.ents, me);
            *speed = 2.1;
        }
        Goal::AttackWolf { state, .. } | Goal::DefendTrader { state, .. } => {
            crate::mob::goals::set_target(t, None);
            state.target_mob = None;
        }
    }
}

pub fn tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    if let Goal::FollowCaravan { speed, .. } = g {
        let Some(h) = head(t.ents, t.id) else {
            return;
        };
        let on_knot = t
            .ents
            .leashes
            .get(&t.id)
            .and_then(|&k| t.ents.store.get(k))
            .is_some_and(|e| e.kind() == ids::LEASH_KNOT);
        if on_knot {
            return;
        }
        let Some(hp) = t.ents.store.get(h).map(|e| e.pos()) else {
            return;
        };
        let p = t.pos();
        let d = hp.distance_to_sqr(p).sqrt();
        let delta = (hp - p).normalize() * (d - 2.0).max(0.0);
        nav::move_to_pos(t, p + delta, *speed);
    }
}
