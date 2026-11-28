//! `PolarBear`: neutral, defends its cubs, stands up and warns before it strikes.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_world::entity::EntityId;

use super::{Cond, G, Goal as FamGoal, MeleeKind, SpawnContext, common, fam, guard, st, st_mut};
use crate::level::Level;
use crate::mob::common::{CommonGoal, TargetFilter};
use crate::mob::fam::SpawnReason;
use crate::mob::goals::{TargetState, flag, reduced};
use crate::mob::kinds::Kind;
use crate::mob::{MobData, Tick};

/// The polar bear goals.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// `PanicGoal` (any cause for cubs, environmental for adults).
    Panic { pos: Vec3 },
    /// `PolarBearAttackPlayersGoal`: adults near cubs attack players within half the range.
    AttackPlayers {
        state: TargetState,
        found: Option<EntityId>,
    },
}

pub fn flags(g: &Goal) -> u8 {
    match g {
        Goal::Panic { .. } => flag::MOVE,
        Goal::AttackPlayers { .. } => flag::TARGET,
    }
}

pub fn every_tick(_g: &Goal) -> bool {
    false
}

fn fox(t: &Tick<'_, '_>, id: EntityId) -> bool {
    crate::mob::common::mob_of(t, id, Kind::Fox)
}

/// `registerGoals` of the polar bear.
pub fn register_goals(m: &mut MobData) {
    let g = &mut m.goals;
    g.add(0, G::Float);
    g.add(1, G::melee(MeleeKind::Plain, 1.25, true));
    g.add(1, fam(FamGoal::Bear(Goal::Panic { pos: Vec3::ZERO })));
    g.add(
        4,
        G::FollowParent {
            speed: 1.25,
            parent: None,
            recalc: 0,
        },
    );
    g.add(5, crate::mob::movement::MoveGoal::random_stroll(1.0, 120));
    g.add(6, G::look_at_player(6.0));
    g.add(7, G::random_look());
    let tg = &mut m.targets;
    tg.add(1, G::hurt_by(true));
    tg.add(
        2,
        fam(FamGoal::Bear(Goal::AttackPlayers {
            state: TargetState::new(true),
            found: None,
        })),
    );
    let angry: TargetFilter = crate::mob::common::angry_at;
    tg.add(3, common(CommonGoal::nearest_player(true, angry)));
    let foxes: TargetFilter = fox;
    tg.add(
        4,
        guard(Cond::NotBaby, CommonGoal::nearest(10, true, foxes)),
    );
    tg.add(
        5,
        common(CommonGoal::ResetUniversalAnger {
            alert_same_type: false,
            last_hurt: 0,
        }),
    );
    m.nav.can_float = true;
}

/// `finalizeSpawn`: every bear after the first of a group is a cub.
pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    super::ageable_group(m, ctx, true, 1.0);
}

fn set_standing(t: &mut Tick<'_, '_>, on: bool) {
    if st(t.mob_ref()).standing != on {
        st_mut(t.mob()).standing = on;
        t.core_mut().mark_data_dirty();
    }
}

/// `PolarBear.tick` and `PolarBearMeleeAttackGoal.checkAndPerformAttack`'s standing: an
/// adult rears up and growls the last ten ticks before its strike.
pub fn before_tick(t: &mut Tick<'_, '_>) {
    if st(t.mob_ref()).warning_sound_ticks > 0 {
        st_mut(t.mob()).warning_sound_ticks -= 1;
    }
    let melee = t.mob_ref().goals.slots.iter().find_map(|s| match s.goal {
        G::Melee { attack_in, .. } if s.running => Some(attack_in),
        _ => None,
    });
    let Some(attack_in) = melee else {
        set_standing(t, false);
        return;
    };
    let Some(o) = t.mob_ref().target.and_then(|id| t.other(id)) else {
        set_standing(t, false);
        return;
    };
    let w = o.aabb.max_x - o.aabb.min_x + 3.0;
    if o.pos.distance_to_sqr(t.pos()) < w * w {
        if attack_in <= 0 {
            set_standing(t, false);
        }
        if attack_in <= 10 {
            set_standing(t, true);
            if st(t.mob_ref()).warning_sound_ticks <= 0 {
                let id = t.id;
                crate::mob::combat::mob_sound(
                    t.ents,
                    t.env,
                    id,
                    "minecraft:entity.polar_bear.warning",
                );
                st_mut(t.mob()).warning_sound_ticks = 40;
            }
        }
    } else {
        set_standing(t, false);
    }
}

/// `PolarBear.tick`: the anger; cubs only call for help, they never fight.
pub fn ai_step(t: &mut Tick<'_, '_>) {
    crate::mob::common::update_persistent_anger(t, true, (400, 780));
    if t.mob_ref().baby && t.mob_ref().target.is_some() {
        // `PolarBearHurtByTargetGoal`: a cub alerts the adults and stops.
        if let Some(by) = t.mob_ref().target {
            alert_adults(t, by);
        }
        t.mob().target = None;
    }
}

/// `alertOthers` restricted to adult bears.
fn alert_adults(t: &mut Tick<'_, '_>, by: EntityId) {
    let range = t.mob_ref().attrs.follow_range;
    let p = t.pos();
    let area = vanillite_base::Aabb::new(p.x, p.y, p.z, p.x + 1.0, p.y + 1.0, p.z + 1.0)
        .inflate(range, 10.0, range);
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&area, false, |id| ids.push(id));
    for &id in &ids {
        if id == t.id {
            continue;
        }
        let alerted = t.ents.mob(id).is_some_and(|m| {
            m.kind == Kind::PolarBear && !m.baby && m.target.is_none() && m.is_alive()
        });
        if alerted
            && crate::mob::goals::target_event(
                t.ents,
                id,
                None,
                Some(by),
                vanillite_abi::target_reason::RETALIATE,
            )
            && let Some(m) = t.ents.mob_mut(id)
        {
            m.target = Some(by);
        }
    }
    t.cx.ids = ids;
}

/// The bear's strike ends its standing.
pub fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
    let r = crate::mob::combat::base_mob_attack(t, target);
    set_standing(t, false);
    Some(r)
}

/// `checkPolarBearSpawnRules`: the animal rules, or ice in the icy biomes.
pub fn spawn_rules(level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    super::animal_spawn_rules(level, pos, reason)
        || (super::bright_enough(level, pos, reason)
            && super::below_is(level, pos, &["minecraft:ice"]))
}

pub fn can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::Panic { pos } => {
            let baby = t.mob_ref().baby;
            let Some(k) = t.mob_ref().last_damage_source(t.game_time()) else {
                return false;
            };
            let panics = if baby {
                t.env
                    .types
                    .is(k, crate::survival::damage::tag::PANIC_CAUSES)
            } else {
                t.env
                    .types
                    .is(k, crate::survival::damage::tag::PANIC_ENVIRONMENTAL_CAUSES)
            };
            if !panics {
                return false;
            }
            match crate::mob::animal::panic_pos(t) {
                Some(p) => {
                    *pos = p;
                    true
                }
                None => false,
            }
        }
        Goal::AttackPlayers { state, found } => {
            if t.mob_ref().baby {
                return false;
            }
            if t.mob().random.next_int_bounded(reduced(20)) != 0 {
                return false;
            }
            let range = t.mob_ref().attrs.follow_range * 0.5;
            let area = t.aabb().inflate(range, range, range);
            let players: TargetFilter = crate::mob::common::any_player;
            *found = crate::mob::common::find_nearest(
                t,
                players,
                None,
                range,
                state.must_see,
                true,
                area,
            );
            if found.is_none() {
                return false;
            }
            let area = t.aabb().inflate(8.0, 4.0, 8.0);
            let ents = &*t.ents;
            let mut cub = false;
            ents.store.for_each_in(&area, false, |id| {
                if ents
                    .mob(id)
                    .is_some_and(|m| m.kind == Kind::PolarBear && m.baby)
                {
                    cub = true;
                }
            });
            cub
        }
    }
}

pub fn can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::Panic { .. } => !t.nav_ref().is_done(),
        Goal::AttackPlayers { state, .. } => crate::mob::goals::target_can_continue(t, state),
    }
}

pub fn start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::Panic { pos } => {
            crate::mob::nav::move_to_pos(t, *pos, 2.0);
        }
        Goal::AttackPlayers { state, found } => {
            crate::mob::goals::set_target(t, *found);
            state.unseen_ticks = 0;
        }
    }
}

pub fn stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    if let Goal::AttackPlayers { state, .. } = g {
        crate::mob::goals::set_target(t, None);
        state.target_mob = None;
    }
}

pub fn tick(_t: &mut Tick<'_, '_>, _g: &mut Goal) {}
