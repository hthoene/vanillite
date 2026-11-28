//! Warden and sculk mechanics on the flat test world (needs the Vanilla data pack, see
//! `level::tests`).

use vanillite_base::random::LegacyRandom;
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::{BlockState, ids};
use vanillite_world::entity::{EntityId, Pose, Rotation};
use vanillite_worldgen::behaviour::StateExt;

use super::ai::{self, Behavior as B};
use super::*;
use crate::level::game_event::events;
use crate::level::tests::{flat_level, tick};
use crate::level::{BlockEntity, update};
use crate::mob::brain::{MemValue, MemoryType as M, brain, brain_ref};
use crate::survival::blocks::{BlockAccess, BlockFacts};
use crate::survival::damage::DamageTypes;
use crate::survival::rules::{Difficulty, GameRules};

struct LevelBlocks<'a>(&'a Level);

impl BlockAccess for LevelBlocks<'_> {
    fn block_state(&self, pos: BlockPos) -> Option<BlockState> {
        Some(self.0.block(pos))
    }
}

fn spawn(level: &mut Level, kind: Kind, x: f64, z: f64) -> EntityId {
    let m = MobData::new(kind, 7);
    let uuid = level.new_uuid();
    let ids = std::sync::Arc::clone(level.network_ids_arc());
    crate::mob::spawn(
        &mut level.entities,
        &ids,
        uuid,
        Vec3::new(x, -60.0, z),
        Rotation::default(),
        m,
    )
}

/// Runs `f` on a mob tick of `id` (entities taken out of the level like the world does).
fn with_tick<R>(level: &mut Level, id: EntityId, f: impl FnOnce(&mut Tick<'_, '_>) -> R) -> R {
    let types = DamageTypes::built_in();
    let facts = BlockFacts::default();
    let rules = GameRules::default();
    let mut random = LegacyRandom::new(1);
    let mut out = Vec::new();
    let raining = |_| false;
    let mut ents = std::mem::take(&mut level.entities);
    let mut cx = MobContext::default();
    let r;
    {
        let level_ref: &Level = level;
        let blocks = LevelBlocks(level_ref);
        let mut no_gateway = |_| false;
        let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
        let mut env = Env {
            hooks: &crate::hooks::NoHooks,
            hook_world: crate::hooks::HookWorld::NONE,
            types: &types,
            facts: &facts,
            blocks: &blocks,
            rules: &rules,
            difficulty: Difficulty::Normal,
            game_time: level_ref.game_time(),
            min_y: level_ref.min_y(),
            logical_height: 384,
            dimension: "minecraft:overworld",
            raining_at: &raining,
            border: None,
            enter_gateway: &mut no_gateway,
            display_name: &names,
            entity_team: &|_| None,
            random: &mut random,
            out: &mut out,
        };
        let mut t = Tick {
            level: level_ref,
            ents: &mut ents,
            env: &mut env,
            cx: &mut cx,
            id,
        };
        r = f(&mut t);
    }
    level.entities = ents;
    for action in cx.actions {
        if let WorldAction::Level(a) = action {
            a.apply(level);
        }
    }
    r
}

#[test]
fn sonic_boom_deals_ten_damage_after_its_charge_and_cools_down() {
    let mut level = flat_level();
    let warden = spawn(&mut level, Kind::Warden, 0.5, 0.5);
    let zombie = spawn(&mut level, Kind::Zombie, 6.5, 0.5);
    with_tick(&mut level, warden, |t| {
        brain(t).set(M::AttackTarget, MemValue::Entity(zombie));
        let mut b = B::SonicBoom;
        assert!(brain_ref(t).check_all(ai::behavior_requirements(&b)));
        assert!(ai::behavior_check(t, &mut b));
        ai::behavior_start(t, &mut b, 0);
        assert!(brain_ref(t).has(M::AttackCoolingDown));
        for time in 1..=33 {
            brain(t).forget_outdated();
            ai::behavior_tick(t, &mut b, time);
        }
        assert_eq!(
            t.ents.mob(zombie).unwrap().health,
            20.0,
            "before the sound delay"
        );
        for time in 34..=60 {
            brain(t).forget_outdated();
            ai::behavior_tick(t, &mut b, time);
        }
        assert_eq!(t.ents.mob(zombie).unwrap().health, 10.0);
        ai::behavior_stop(t, &mut b, 61);
        assert_eq!(brain_ref(t).ttl(M::SonicBoomCooldown), 40);
        assert!(!brain_ref(t).check_all(ai::behavior_requirements(&B::SonicBoom)));
    });
}

#[test]
fn the_warden_listener_follows_the_warden_and_hears_block_events() {
    let mut level = flat_level();
    let warden = spawn(&mut level, Kind::Warden, 0.5, 0.5);
    with_tick(&mut level, warden, after_tick);
    assert_eq!(level.game_event_listeners().len(), 1);
    let pos = BlockPos::new(4, -60, 0);
    level.game_event_at(None, events().block_place, pos);
    let s = level.entities.mob(warden).unwrap().boss.warden().unwrap();
    let (vibration, _) = s.vibration.candidate.expect("the warden heard it");
    assert_eq!(vibration.event, events().block_place);
    // A digging warden hears nothing.
    level
        .entities
        .store
        .get_mut(warden)
        .unwrap()
        .set_pose(Pose::Digging);
    level
        .entities
        .mob_mut(warden)
        .unwrap()
        .boss
        .warden_mut()
        .unwrap()
        .vibration = VibrationData::default();
    level.game_event_at(None, events().block_place, pos);
    let s = level.entities.mob(warden).unwrap().boss.warden().unwrap();
    assert!(s.vibration.candidate.is_none());
    // Once the warden is gone its listener is dropped by the next event.
    level.entities.despawn(warden);
    level.game_event_at(None, events().block_place, pos);
    assert!(level.game_event_listeners().is_empty());
}

#[test]
fn a_summoned_warden_emerges_on_a_block_top() {
    let mut level = flat_level();
    assert!(try_spawn_triggered(
        &mut level,
        BlockPos::new(0, -61, 0),
        20,
        5,
        6
    ));
    let (id, _) = level.entities.mobs.iter().next().expect("a warden");
    let e = level.entities.store.get(id).unwrap();
    assert_eq!(e.pose(), Pose::Emerging);
    assert_eq!(e.dims().height, 1.0);
    assert_eq!(e.pos().y, -60.0);
    let m = level.entities.mob(id).unwrap();
    assert_eq!(m.kind, Kind::Warden);
    let mem = m.brain.as_deref().unwrap();
    assert!(mem.has(M::IsEmerging) && mem.has(M::DigCooldown));
}

#[test]
fn a_catalyst_takes_the_experience_of_a_death_and_spreads_sculk() {
    let mut level = flat_level();
    let catalyst = BlockPos::new(0, -60, 0);
    level.set_block(catalyst, ids::SCULK_CATALYST.default_state(), update::ALL);
    assert!(matches!(
        level.block_entity(catalyst),
        Some(BlockEntity::SculkCatalyst(_))
    ));
    assert!(crate::block::sculk_catalyst::catalyst_near(
        &level,
        Vec3::new(3.5, -60.0, 0.5)
    ));
    assert!(!crate::block::sculk_catalyst::catalyst_near(
        &level,
        Vec3::new(30.5, -60.0, 0.5)
    ));
    assert!(crate::block::sculk_catalyst::on_mob_death(
        &mut level,
        Vec3::new(3.5, -60.0, 0.5),
        30
    ));
    assert_eq!(level.block(catalyst).value("bloom"), Some("true"));
    let Some(BlockEntity::SculkCatalyst(c)) = level.block_entity(catalyst) else {
        panic!("catalyst");
    };
    assert_eq!(c.cursors.iter().map(|c| c.charge).sum::<i32>(), 30);
    for _ in 0..200 {
        tick(&mut level);
    }
    // The bloom ended and sculk replaced the grass around the death.
    assert_eq!(level.block(catalyst).value("bloom"), Some("false"));
    let mut sculk = 0;
    for x in -8..=12 {
        for z in -8..=8 {
            if level.block(BlockPos::new(x, -61, z)).is(ids::SCULK) {
                sculk += 1;
            }
        }
    }
    assert!(sculk > 0, "no sculk spread");
}
