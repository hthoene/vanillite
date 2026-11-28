//! Simulation tests of the dragon and the fight on a flat world.

use vanillite_base::random::LegacyRandom;
use vanillite_base::{BlockPos, Vec3};
use vanillite_world::entity::{EntityId, Rotation};

use super::crystal::{Crystal, spawn_crystal};
use super::fight::{DragonFight, RespawnStage};
use super::*;
use crate::level::Level;
use crate::level::tests::flat_level;
use crate::mob::{Kind, MobContext, MobData};
use crate::survival::Effect;
use crate::survival::blocks::{BlockAccess, BlockFacts};
use crate::survival::damage::{DamageType, DamageTypes, DeathMessageType, Effects, Scaling, tag};
use crate::survival::rules::{Difficulty, GameRules};

struct NoBlocks;

impl BlockAccess for NoBlocks {
    fn block_state(&self, _: BlockPos) -> Option<vanillite_data::block::BlockState> {
        None
    }
}

/// A few damage types with their tags.
fn types() -> DamageTypes {
    let t = |id: &str, tags: u32| DamageType {
        id: id.to_owned(),
        msg_id: id.to_owned(),
        scaling: Scaling::Never,
        exhaustion: 0.0,
        effects: Effects::Hurt,
        death_message_type: DeathMessageType::Default,
        tags,
    };
    DamageTypes::new(vec![
        t("minecraft:generic", tag::BYPASSES_ARMOR),
        t("minecraft:explosion", tag::IS_EXPLOSION),
        t("minecraft:mob_attack", 0),
        t("minecraft:fall", tag::IS_FALL),
    ])
}

struct World {
    level: Level,
    types: DamageTypes,
    facts: BlockFacts,
    rules: GameRules,
    random: LegacyRandom,
    out: Vec<Effect>,
    cx: MobContext,
}

impl World {
    fn new() -> World {
        World {
            level: flat_level(),
            types: types(),
            facts: BlockFacts::default(),
            rules: GameRules::default(),
            random: LegacyRandom::new(5),
            out: Vec::new(),
            cx: MobContext::default(),
        }
    }

    /// Runs `f` with an environment and the entities taken out of the level.
    fn with<R>(
        &mut self,
        f: impl FnOnce(&Level, &mut Entities, &mut Env<'_>, &mut MobContext) -> R,
    ) -> R {
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
        let mut env = Env {
            hooks: &crate::hooks::NoHooks,
            hook_world: crate::hooks::HookWorld::NONE,
            types: &self.types,
            facts: &self.facts,
            blocks: &NoBlocks,
            rules: &self.rules,
            difficulty: Difficulty::Normal,
            game_time: 0,
            min_y: -64,
            logical_height: 384,
            dimension: "minecraft:the_end",
            raining_at: &raining,
            border: None,
            enter_gateway: &mut no_gateway,
            display_name: &names,
            entity_team: &|_| None,
            random: &mut self.random,
            out: &mut self.out,
        };
        let mut ents = std::mem::take(&mut self.level.entities);
        let r = f(&self.level, &mut ents, &mut env, &mut self.cx);
        self.level.entities = ents;
        r
    }

    fn spawn_dragon(&mut self, pos: Vec3, phase: Phase) -> EntityId {
        let mut m = MobData::new(Kind::EnderDragon, 3);
        if let Some(s) = m.boss.dragon_mut() {
            phase::set_phase_in(s, phase, &mut Vec::new());
        }
        let ids = std::sync::Arc::clone(self.level.network_ids_arc());
        crate::mob::spawn(
            &mut self.level.entities,
            &ids,
            1,
            pos,
            Rotation::default(),
            m,
        )
    }

    fn tick_mob(&mut self, id: EntityId) {
        self.with(|level, ents, env, cx| crate::mob::tick(level, ents, env, cx, id));
        for d in std::mem::take(&mut self.cx.discarded) {
            self.level.entities.despawn(d);
        }
        self.cx.actions.clear();
        self.cx.dragon_events.clear();
    }
}

#[test]
fn part_damage_multipliers() {
    assert_eq!(part_damage(HEAD, 8.0), 8.0);
    assert_eq!(part_damage(NECK, 8.0), 8.0);
    assert_eq!(part_damage(BODY, 8.0), 3.0);
    assert_eq!(part_damage(WING1, 0.5), 0.625);
}

#[test]
fn experience_first_kill_and_later() {
    let total = |first| (0..=200).map(|t| death_xp(first, t)).sum::<i32>();
    assert_eq!(total(true), 12_000);
    assert_eq!(total(false), 500);
    assert_eq!(death_xp(true, 150), 0);
    assert_eq!(death_xp(true, 155), 960);
}

#[test]
fn dragon_parts_reserve_network_ids() {
    let mut w = World::new();
    let id = w.spawn_dragon(Vec3::new(0.5, 100.0, 0.5), Phase::Hover);
    let nid = w.level.entities.store.get(id).unwrap().network_id().0;
    for i in 1..=8 {
        assert_eq!(w.level.entities.by_network_id(nid + i), Some(id));
        assert_eq!(
            part_of(&w.level.entities, id, nid + i),
            Some((i - 1) as usize)
        );
    }
    assert_eq!(part_of(&w.level.entities, id, nid), None);
    let next = w.level.network_ids().next().0;
    assert_eq!(next, nid + 9);
    w.level.entities.despawn(id);
    assert_eq!(w.level.entities.by_network_id(nid + 3), None);
}

#[test]
fn crystals_heal_within_range() {
    for (crystal_x, heals) in [(20.0, true), (60.0, false)] {
        let mut w = World::new();
        let dragon = w.spawn_dragon(Vec3::new(0.5, 100.0, 0.5), Phase::Hover);
        w.level.entities.mob_mut(dragon).unwrap().health = 100.0;
        w.with(|level, ents, env, _| {
            spawn_crystal(
                level,
                ents,
                env,
                Vec3::new(crystal_x, 100.0, 0.5),
                0.0,
                Crystal::default(),
            );
        });
        for _ in 0..200 {
            w.tick_mob(dragon);
        }
        let m = w.level.entities.mob(dragon).unwrap();
        assert_eq!(
            m.health > 100.0,
            heals,
            "crystal at x {crystal_x}: {}",
            m.health
        );
        assert_eq!(m.boss.dragon().unwrap().nearest_crystal.is_some(), heals);
    }
}

#[test]
fn hover_phase_holds_position() {
    let mut w = World::new();
    let dragon = w.spawn_dragon(Vec3::new(0.5, 100.0, 0.5), Phase::Hover);
    for _ in 0..40 {
        w.tick_mob(dragon);
    }
    let p = w.level.entities.store.get(dragon).unwrap().pos();
    assert!(p.distance_to_sqr(Vec3::new(0.5, 100.0, 0.5)) < 4.0, "{p:?}");
    // The parts follow the body.
    let s = w.level.entities.mob(dragon).unwrap().boss.dragon().unwrap();
    assert!(s.parts[HEAD].distance_to_sqr(p) < 100.0);
}

#[test]
fn killing_blow_starts_the_dying_phase() {
    let mut w = World::new();
    let dragon = w.spawn_dragon(Vec3::new(0.5, 100.0, 0.5), Phase::HoldingPattern);
    w.level.entities.mob_mut(dragon).unwrap().health = 1.0;
    w.with(|level, ents, env, cx| {
        let mut source = DamageSource::of(env.types.explosion);
        source.position = Some(Vec3::new(0.5, 100.0, 0.5));
        combat::hurt(level, ents, env, cx, dragon, source, 50.0);
    });
    let m = w.level.entities.mob(dragon).unwrap();
    assert_eq!(m.health, 1.0);
    assert!(!m.dead);
    assert_eq!(m.boss.dragon().unwrap().phase, Phase::Dying);
}

#[test]
fn only_players_and_explosions_hurt_the_dragon() {
    let mut w = World::new();
    let dragon = w.spawn_dragon(Vec3::new(0.5, 100.0, 0.5), Phase::HoldingPattern);
    w.with(|level, ents, env, cx| {
        // The hit counts (`hurt` is true) but does no damage.
        let source = DamageSource::of(env.types.generic);
        assert!(combat::hurt(level, ents, env, cx, dragon, source, 10.0));
    });
    assert_eq!(w.level.entities.mob(dragon).unwrap().health, 200.0);
}

#[test]
fn respawn_stages_take_604_ticks() {
    let mut w = World::new();
    let mut r = LegacyRandom::new(1);
    let mut fight = DragonFight::new(None, 0, &mut r);
    fight.dragon_killed = true;
    fight.respawn_stage = Some(RespawnStage::Start);
    let mut uuids = Vec::new();
    w.with(|level, ents, env, _| {
        for (dx, dz) in [(3, 0), (-3, 0), (0, 3), (0, -3)] {
            let id = spawn_crystal(
                level,
                ents,
                env,
                Vec3::new(f64::from(dx) + 0.5, -60.0, f64::from(dz) + 0.5),
                0.0,
                Crystal::default(),
            );
            uuids.push(ents.store.get(id).unwrap().uuid());
        }
    });
    fight.respawn_crystals = uuids;
    let mut ticks = 0;
    while fight.respawn_stage.is_some() {
        ticks += 1;
        assert!(ticks < 1000, "the respawn never ends");
        let World {
            level,
            types,
            facts,
            rules,
            random,
            out,
            cx,
        } = &mut w;
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
        let mut env = Env {
            hooks: &crate::hooks::NoHooks,
            hook_world: crate::hooks::HookWorld::NONE,
            types,
            facts,
            blocks: &NoBlocks,
            rules,
            difficulty: Difficulty::Normal,
            game_time: 0,
            min_y: -64,
            logical_height: 384,
            dimension: "minecraft:the_end",
            raining_at: &raining,
            border: None,
            enter_gateway: &mut no_gateway,
            display_name: &names,
            entity_team: &|_| None,
            random,
            out,
        };
        fight.tick_respawn(level, &mut env, cx);
        level.output().explosion_hurts.clear();
    }
    // START 1, PREPARING 101, SUMMONING_PILLARS 401, SUMMONING_DRAGON 101.
    assert_eq!(ticks, 604);
    assert!(!fight.dragon_killed);
    let dragons = w
        .level
        .entities
        .mobs
        .iter()
        .filter(|(_, m)| m.kind == Kind::EnderDragon)
        .count();
    assert_eq!(dragons, 1);
    let crystals = w
        .level
        .entities
        .specials
        .iter()
        .filter(|(_, d)| matches!(d, crate::mob::boss::special::SpecialData::EndCrystal(_)))
        .count();
    // The four respawn crystals exploded; the ten pillars got new ones.
    assert_eq!(crystals, 10);
}

impl World {
    /// One `EnderDragonFight.tick` with the level as it is.
    fn tick_fight(&mut self, fight: &mut DragonFight) {
        let World {
            level,
            types,
            facts,
            rules,
            random,
            out,
            cx,
        } = self;
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
        let mut env = Env {
            hooks: &crate::hooks::NoHooks,
            hook_world: crate::hooks::HookWorld::NONE,
            types,
            facts,
            blocks: &NoBlocks,
            rules,
            difficulty: Difficulty::Normal,
            game_time: 0,
            min_y: -64,
            logical_height: 384,
            dimension: "minecraft:the_end",
            raining_at: &raining,
            border: None,
            enter_gateway: &mut no_gateway,
            display_name: &names,
            entity_team: &|_| None,
            random,
            out,
        };
        fight.tick(level, &mut env, cx);
    }
}

#[test]
fn a_player_in_a_new_end_starts_the_fight_and_leaves_its_bar() {
    let mut w = World::new();
    let mut fight = DragonFight::new(None, 0, &mut LegacyRandom::new(1));
    let player = crate::block::spawner_tests::add_player(
        &mut w.level,
        Vec3::new(8.5, -60.0, 8.5),
        vanillite_config::GameMode::Survival,
    );
    let dragons = |level: &Level| {
        level
            .entities
            .mobs
            .iter()
            .filter(|(_, m)| m.kind == Kind::EnderDragon)
            .count()
    };
    // The fight's ticket loads and simulates the arena (no entities stored in it), then the
    // fight scans it: no exit portal and no dragon, so it builds the portal and a dragon.
    for _ in 0..40 {
        w.tick_fight(&mut fight);
        crate::level::tests::tick(&mut w.level);
        if dragons(&w.level) > 0 {
            break;
        }
    }
    assert!(!fight.needs_state_scanning);
    assert_eq!(dragons(&w.level), 1);
    assert_eq!(fight.bar().players(), [player]);
    // The player leaves for another world: the next player scan sends it the bar's removal.
    w.level.entities.despawn_player(player);
    w.out.clear();
    for _ in 0..20 {
        w.tick_fight(&mut fight);
    }
    assert!(fight.bar().players().is_empty());
    assert!(
        w.out
            .iter()
            .any(|e| matches!(e, Effect::To { player: p, .. } if *p == player))
    );
    assert_eq!(w.level.entities.departed_uuid(player), Some(0xABCD));
}

#[test]
fn kill_removes_the_fight_dragon_at_once_and_ends_the_fight() {
    let mut w = World::new();
    let mut fight = DragonFight::new(None, 0, &mut LegacyRandom::new(1));
    crate::block::spawner_tests::add_player(
        &mut w.level,
        Vec3::new(8.5, -60.0, 8.5),
        vanillite_config::GameMode::Survival,
    );
    let dragon = |level: &Level| {
        level
            .entities
            .mobs
            .iter()
            .find(|(_, m)| m.kind == Kind::EnderDragon)
            .map(|(id, _)| id)
    };
    for _ in 0..40 {
        w.tick_fight(&mut fight);
        crate::level::tests::tick(&mut w.level);
        if dragon(&w.level).is_some() {
            break;
        }
    }
    let id = dragon(&w.level).expect("the fight's dragon");
    w.level
        .entities
        .mob_mut(id)
        .unwrap()
        .boss
        .dragon_mut()
        .unwrap()
        .in_fight = true;
    w.cx.dragon_events.clear();
    assert!(w.with(|_, ents, _, cx| kill(ents, cx, id)));
    // `remove(KILLED)`: gone at once, without the dying phase.
    for d in std::mem::take(&mut w.cx.discarded) {
        w.level.entities.despawn(d);
    }
    assert!(dragon(&w.level).is_none());
    let events = std::mem::take(&mut w.cx.dragon_events);
    assert!(matches!(
        events[..],
        [DragonEvent::Update { progress, .. }, DragonEvent::Killed { .. }] if progress == 1.0
    ));
    let World {
        level,
        types,
        facts,
        rules,
        random,
        out,
        cx,
    } = &mut w;
    let raining = |_| false;
    let mut no_gateway = |_| false;
    let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
    let mut env = Env {
        hooks: &crate::hooks::NoHooks,
        hook_world: crate::hooks::HookWorld::NONE,
        types,
        facts,
        blocks: &NoBlocks,
        rules,
        difficulty: Difficulty::Normal,
        game_time: 0,
        min_y: -64,
        logical_height: 384,
        dimension: "minecraft:the_end",
        raining_at: &raining,
        border: None,
        enter_gateway: &mut no_gateway,
        display_name: &names,
        entity_team: &|_| None,
        random,
        out,
    };
    for event in events {
        fight.handle(level, &mut env, cx, event);
    }
    assert!(fight.dragon_killed && fight.previously_killed);
    assert!(!fight.bar().is_visible());
}
