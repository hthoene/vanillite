//! Brain tests: memories, activities, schedules, gates and behaviour durations, plus a test
//! harness that runs a mob tick on a flat world.

use vanillite_base::random::LegacyRandom;
use vanillite_base::{BlockPos, Vec3};
use vanillite_world::entity::EntityId;

use super::behavior::{Behavior, Spec, manhattan_ordered};
use super::memory::MemoryStatus as S;
use super::*;
use crate::level::Level;
use crate::mob::{Kind, MobContext, MobData};
use crate::survival::blocks::{BlockAccess, BlockFacts};
use crate::survival::damage::DamageTypes;
use crate::survival::rules::{Difficulty, GameRules};

struct LevelBlocks<'a>(&'a crate::level::Level);

impl BlockAccess for LevelBlocks<'_> {
    fn block_state(&self, pos: BlockPos) -> Option<vanillite_data::block::BlockState> {
        Some(self.0.block(pos))
    }
}

/// Runs `f` with a [`Tick`] of mob `id` on `level` at `game_time`.
pub(crate) fn with_tick<R>(
    level: &mut Level,
    id: EntityId,
    game_time: i64,
    f: impl FnOnce(&mut Tick<'_, '_>) -> R,
) -> R {
    let mut ents = std::mem::take(&mut level.entities);
    let types = DamageTypes::built_in();
    let facts = BlockFacts::default();
    let rules = GameRules::default();
    let mut random = LegacyRandom::new(7);
    let mut out = Vec::new();
    let raining = |_| false;
    let hooks = crate::hooks::NoHooks;
    let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
    let r = {
        let blocks = LevelBlocks(level);
        let mut env = crate::survival::player::Env {
            types: &types,
            facts: &facts,
            blocks: &blocks,
            rules: &rules,
            difficulty: Difficulty::Normal,
            game_time,
            min_y: level.min_y(),
            logical_height: 384,
            dimension: "minecraft:overworld",
            raining_at: &raining,
            border: None,
            display_name: &names,
            entity_team: &|_| None,
            enter_gateway: &mut |_| false,
            random: &mut random,
            out: &mut out,
            hooks: &hooks,
            hook_world: crate::hooks::HookWorld::NONE,
        };
        let mut cx = MobContext {
            path_budget: u64::MAX,
            ..MobContext::default()
        };
        let mut t = Tick {
            level,
            ents: &mut ents,
            env: &mut env,
            cx: &mut cx,
            id,
        };
        f(&mut t)
    };
    level.entities = ents;
    r
}

/// A flat test world with a mob of `kind` at the surface.
pub(crate) fn level_with(kind: Kind, seed: i64) -> (Level, EntityId) {
    let mut level = crate::level::tests::flat_level();
    let m = MobData::new(kind, seed);
    let id = crate::mob::spawn::add_mob(
        &mut level,
        m,
        Vec3::new(0.5, -60.0, 0.5),
        0.0,
        crate::mob::fam::SpawnReason::Command,
    );
    (level, id)
}

fn spec(sensors: Vec<SensorKind>, activities: Vec<ActivitySpec>) -> BrainSpec {
    BrainSpec {
        sensors,
        activities,
        ..BrainSpec::default()
    }
}

#[test]
fn memories_expire_after_their_time_to_live() {
    let mut b = Brain::new(
        BrainSpec {
            memories: vec![MemoryType::AdmiringItem],
            ..BrainSpec::default()
        },
        &mut LegacyRandom::new(1),
    );
    b.set_with_expiry(MemoryType::AdmiringItem, MemValue::Bool(true), 2);
    assert!(b.has(MemoryType::AdmiringItem));
    b.forget_outdated();
    b.forget_outdated();
    assert!(b.has(MemoryType::AdmiringItem), "ttl 0 is still present");
    b.forget_outdated();
    assert!(!b.has(MemoryType::AdmiringItem));
}

#[test]
fn unregistered_memories_are_ignored() {
    let mut b = Brain::default();
    b.set(MemoryType::AttackTarget, MemValue::Unit);
    assert!(!b.has(MemoryType::AttackTarget));
    assert!(!b.check(MemoryType::AttackTarget, S::ValueAbsent));
    b.register(MemoryType::AttackTarget);
    assert!(b.check(MemoryType::AttackTarget, S::ValueAbsent));
    assert!(b.check(MemoryType::AttackTarget, S::Registered));
}

#[test]
fn empty_lists_erase_the_memory() {
    let mut b = Brain::default();
    b.register(MemoryType::NearestLivingEntities);
    b.set_entities(MemoryType::NearestLivingEntities, std::iter::empty());
    assert!(!b.has(MemoryType::NearestLivingEntities));
}

#[test]
fn activities_switch_by_requirements_and_erase_memories() {
    let fight = ActivitySpec::with_memory(
        Activity::Fight,
        10,
        vec![Spec::do_nothing(1, 1)],
        MemoryType::AttackTarget,
    );
    let idle = ActivitySpec::new(Activity::Idle, 10, vec![Spec::do_nothing(1, 1)]);
    let mut spec = spec(vec![], vec![fight, idle]);
    spec.memories.push(MemoryType::AttackTarget);
    let mut b = Brain::new(spec, &mut LegacyRandom::new(1));
    assert!(b.is_active(Activity::Core) && b.is_active(Activity::Idle));
    b.set_active_activity_to_first_valid(&[Activity::Fight, Activity::Idle]);
    assert_eq!(b.active_non_core(), Some(Activity::Idle));
    b.set(MemoryType::AttackTarget, MemValue::Unit);
    b.set_active_activity_to_first_valid(&[Activity::Fight, Activity::Idle]);
    assert_eq!(b.active_non_core(), Some(Activity::Fight));
    b.set_active_activity_if_possible(Activity::Idle);
    assert_eq!(b.active_non_core(), Some(Activity::Idle));
    assert!(
        !b.has(MemoryType::AttackTarget),
        "stopping fight erased its memory"
    );
}

#[test]
fn schedules_pick_the_last_keyframe() {
    let s = Schedule::villager();
    assert_eq!(s.activity_at(5), Activity::Rest);
    assert_eq!(s.activity_at(2500), Activity::Work);
    assert_eq!(s.activity_at(9500), Activity::Meet);
    assert_eq!(s.activity_at(23999), Activity::Rest);
    let mut b = Brain::new(
        BrainSpec {
            activities: vec![
                ActivitySpec::new(Activity::Work, 0, vec![]),
                ActivitySpec::new(Activity::Idle, 0, vec![]),
            ],
            schedule: Some(s),
            ..BrainSpec::default()
        },
        &mut LegacyRandom::new(1),
    );
    b.update_activity_from_schedule(100, 2500);
    assert!(b.is_active(Activity::Work));
}

#[test]
fn sensors_start_at_a_random_delay_below_their_scan_rate() {
    let b = Brain::new(
        spec(
            vec![SensorKind::NearestLivingEntities, SensorKind::HurtBy],
            vec![],
        ),
        &mut LegacyRandom::new(3),
    );
    assert_eq!(b.sensors.len(), 2);
    assert!(b.sensors.iter().all(|s| (0..20).contains(&s.time_to_tick)));
    assert!(b.is_registered(MemoryType::NearestVisibleLivingEntities));
    assert!(b.is_registered(MemoryType::HurtByEntity));
}

#[test]
fn behaviour_memories_are_registered() {
    let b = Brain::new(
        spec(
            vec![],
            vec![ActivitySpec::new(
                Activity::Core,
                0,
                vec![
                    Spec::look_at_target_sink(45, 90),
                    Spec::move_to_target_sink(),
                ],
            )],
        ),
        &mut LegacyRandom::new(1),
    );
    for m in [
        MemoryType::LookTarget,
        MemoryType::WalkTarget,
        MemoryType::Path,
        MemoryType::CantReachWalkTargetSince,
    ] {
        assert!(b.is_registered(m), "{m:?}");
    }
}

#[test]
fn manhattan_order_matches_vanilla() {
    let mut seen = Vec::new();
    manhattan_ordered(BlockPos::new(0, 0, 0), 1, 1, 1, 3, |p| {
        seen.push((p.x, p.y, p.z));
        false
    });
    assert_eq!(seen[0], (0, 0, 0));
    assert_eq!(&seen[1..4], &[(-1, 0, 0), (0, -1, 0), (0, 0, 1)]);
    assert_eq!(seen[4], (0, 0, -1));
    assert_eq!(seen.len(), 27);
}

#[test]
fn gates_run_one_child_and_behaviours_last_their_duration() {
    let (mut level, id) = level_with(Kind::Cow, 5);
    let idle = ActivitySpec::new(
        Activity::Idle,
        10,
        vec![Spec::run_one(vec![
            (Spec::do_nothing(3, 3), 1),
            (Spec::do_nothing(3, 3), 1),
        ])],
    );
    let b = Brain::new(spec(vec![], vec![idle]), &mut LegacyRandom::new(1));
    level.entities.mob_mut(id).expect("mob").brain = Some(Box::new(b));
    with_tick(&mut level, id, 100, |t| {
        tick(t);
        let b = brain_ref(t);
        assert_eq!(b.nodes[0].status, Status::Running);
        let running = b.nodes[1..]
            .iter()
            .filter(|n| n.status == Status::Running)
            .count();
        assert_eq!(running, 1, "RunOne starts exactly one child");
    });
    for time in 101..=103 {
        with_tick(&mut level, id, time, tick);
    }
    with_tick(&mut level, id, 104, |t| {
        tick(t);
        // The child ended after 3 ticks and the gate stopped with it, then restarted.
        let b = brain_ref(t);
        assert!(matches!(b.nodes[0].behavior, Behavior::Gate { .. }));
    });
}
