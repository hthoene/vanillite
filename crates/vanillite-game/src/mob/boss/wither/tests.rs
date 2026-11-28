//! Wither mechanics on a flat world: summoning, the charge-up, skull hits and the powered
//! phase.

use vanillite_base::random::LegacyRandom;
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::{BlockState, ids as block_ids};
use vanillite_data::entity::ids as entity_ids;
use vanillite_world::entity::{EntityId, NewEntity, Rotation};

use super::*;
use crate::level::tests::flat_level;
use crate::mob::projectile::ArrowData;
use crate::survival::blocks::{BlockAccess, BlockFacts};
use crate::survival::damage::DamageTypes;
use crate::survival::rules::GameRules;

struct NoBlocks;

impl BlockAccess for NoBlocks {
    fn block_state(&self, _: BlockPos) -> Option<BlockState> {
        None
    }
}

fn place_t(level: &mut Level, base: BlockPos, along_x: bool, block: BlockState) -> [BlockPos; 3] {
    let side = |i: i32| {
        if along_x {
            BlockPos::new(base.x + i, base.y, base.z)
        } else {
            BlockPos::new(base.x, base.y, base.z + i)
        }
    };
    level.set_block(base.relative(Direction::Down), block, update::ALL);
    for i in -1..=1 {
        level.set_block(side(i), block, update::ALL);
    }
    [side(-1), side(0), side(1)].map(|p| p.relative(Direction::Up))
}

fn skull() -> BlockState {
    block_ids::WITHER_SKELETON_SKULL.default_state()
}

fn soul(sand: bool) -> BlockState {
    (if sand {
        block_ids::SOUL_SAND
    } else {
        block_ids::SOUL_SOIL
    })
    .default_state()
}

fn summon(along_x: bool, sand: bool) -> (Level, Option<EntityId>, BlockPos) {
    let mut level = flat_level();
    let base = BlockPos::new(4, -59, 4);
    let skulls = place_t(&mut level, base, along_x, soul(sand));
    for p in skulls {
        level.set_block(p, skull(), update::ALL);
    }
    let id = check_spawn(&mut level, skulls[2], Difficulty::Normal, 1);
    (level, id, base)
}

#[test]
fn pattern_along_x_with_soul_sand_summons_a_charging_wither() {
    let (level, id, base) = summon(true, true);
    let id = id.expect("the T of soul sand with three skulls summons a wither");
    let m = level.entities.mob(id).expect("the wither is a mob");
    assert_eq!(m.kind, Kind::Wither);
    assert_eq!(
        state(m).map(|s| s.invulnerable_ticks),
        Some(INVULNERABLE_TICKS)
    );
    assert!((m.health - m.max_health() / 3.0).abs() < 1e-4);
    let e = level.entities.store.get(id).expect("in the store");
    let p = e.pos();
    assert_eq!(
        BlockPos::containing(p.x, p.y, p.z),
        base.relative(Direction::Down)
    );
    assert!((p.y - (f64::from(base.y - 1) + 0.55)).abs() < 1e-9);
    // The pattern is gone.
    for dx in -1..=1 {
        assert!(
            level
                .block(BlockPos::new(base.x + dx, base.y, base.z))
                .is_air()
        );
        assert!(
            level
                .block(BlockPos::new(base.x + dx, base.y + 1, base.z))
                .is_air()
        );
    }
    assert!(level.block(base.relative(Direction::Down)).is_air());
}

#[test]
fn pattern_along_z_with_soul_soil_summons() {
    let (level, id, base) = summon(false, false);
    let id = id.expect("soul soil works like soul sand, in both orientations");
    let e = level.entities.store.get(id).expect("in the store");
    assert_eq!(e.rot().y_rot, 0.0);
    for dz in -1..=1 {
        assert!(
            level
                .block(BlockPos::new(base.x, base.y + 1, base.z + dz))
                .is_air()
        );
    }
}

#[test]
fn incomplete_pattern_or_peaceful_summons_nothing() {
    let mut level = flat_level();
    let base = BlockPos::new(4, -59, 4);
    let skulls = place_t(&mut level, base, true, soul(true));
    level.set_block(skulls[0], skull(), update::ALL);
    level.set_block(skulls[1], skull(), update::ALL);
    assert!(check_spawn(&mut level, skulls[1], Difficulty::Hard, 1).is_none());
    level.set_block(skulls[2], skull(), update::ALL);
    assert!(check_spawn(&mut level, skulls[2], Difficulty::Peaceful, 1).is_none());
    assert!(check_spawn(&mut level, skulls[2], Difficulty::Easy, 1).is_some());
}

/// Runs `f` with a tick of `id`.
fn with_tick<R>(
    level: &mut Level,
    id: EntityId,
    difficulty: Difficulty,
    cx: &mut MobContext,
    f: impl FnOnce(&mut Tick<'_, '_>) -> R,
) -> R {
    let types = DamageTypes::built_in();
    let facts = BlockFacts::default();
    let rules = GameRules::default();
    let mut random = LegacyRandom::new(3);
    let mut out = Vec::new();
    let raining = |_| false;
    let mut no_gateway = |_| false;
    let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
    let mut env = Env {
        hooks: &crate::hooks::NoHooks,
        hook_world: crate::hooks::HookWorld::NONE,
        types: &types,
        facts: &facts,
        blocks: &NoBlocks,
        rules: &rules,
        difficulty,
        game_time: 0,
        min_y: -64,
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
    let mut ents = std::mem::take(&mut level.entities);
    let r = {
        let mut t = Tick {
            level,
            ents: &mut ents,
            env: &mut env,
            cx,
            id,
        };
        f(&mut t)
    };
    level.entities = ents;
    r
}

#[test]
fn charge_up_explodes_after_220_ticks_and_heals() {
    let (mut level, id, _) = summon(true, true);
    let id = id.expect("summoned");
    let mut cx = MobContext::default();
    let start = level.entities.mob(id).map_or(0.0, |m| m.health);
    for tick in 1..=INVULNERABLE_TICKS {
        let explode = with_tick(&mut level, id, Difficulty::Normal, &mut cx, |t| {
            t.mob().tick_count = tick;
            custom_server_ai_step(t);
            std::mem::take(&mut t.cx.actions)
                .into_iter()
                .any(|a| matches!(a, WorldAction::Explode(e) if e.radius == 7.0 && e.mob))
        });
        assert_eq!(explode, tick == INVULNERABLE_TICKS, "tick {tick}");
    }
    let m = level.entities.mob(id).expect("alive");
    assert_eq!(state(m).map(|s| s.invulnerable_ticks), Some(0));
    // `heal(10)` every 10 ticks: 22 times, capped at 300.
    assert!((m.health - (start + 220.0).min(300.0)).abs() < 1e-3);
    assert!((cx.boss_bars.get(&id).map_or(0.0, BossBar::progress) - 1.0).abs() < 1e-6);
}

#[test]
fn skull_hit_withers_by_difficulty() {
    assert_eq!(wither_seconds(Difficulty::Easy), 0);
    assert_eq!(wither_seconds(Difficulty::Normal), 10);
    assert_eq!(wither_seconds(Difficulty::Hard), 40);
    for (difficulty, ticks) in [(Difficulty::Normal, 200), (Difficulty::Hard, 800)] {
        let (mut level, wither, _) = summon(true, true);
        let wither = wither.expect("summoned");
        let network_ids = std::sync::Arc::clone(level.network_ids_arc());
        let target = crate::mob::spawn(
            &mut level.entities,
            &network_ids,
            7,
            Vec3::new(10.5, -60.0, 10.5),
            Rotation::default(),
            MobData::new(Kind::Cow, 5),
        );
        let mut ents = std::mem::take(&mut level.entities);
        let skull = spawn_skull(
            &level,
            &mut ents,
            wither,
            Vec3::new(10.5, -59.0, 8.0),
            Vec3::new(0.0, 0.0, 1.0),
            Rotation::default(),
            false,
            9,
        );
        if let Some(SpecialData::WitherSkull(s)) = ents.special_mut(skull) {
            s.left_owner = true;
            // Fast enough to reach the cow this tick.
            s.acceleration_power = 3.0;
        }
        level.entities = ents;
        let mut cx = MobContext::default();
        let types = DamageTypes::built_in();
        let facts = BlockFacts::default();
        let rules = GameRules::default();
        let mut random = LegacyRandom::new(3);
        let mut out = Vec::new();
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
        let mut env = Env {
            hooks: &crate::hooks::NoHooks,
            hook_world: crate::hooks::HookWorld::NONE,
            types: &types,
            facts: &facts,
            blocks: &NoBlocks,
            rules: &rules,
            difficulty,
            game_time: 0,
            min_y: -64,
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
        let mut ents = std::mem::take(&mut level.entities);
        {
            let mut t = SpecialTick {
                level: &level,
                ents: &mut ents,
                env: &mut env,
                cx: &mut cx,
                id: skull,
            };
            tick_skull(&mut t);
        }
        let cow = ents.mob(target).expect("the cow lives");
        assert!(cow.health < cow.max_health(), "the skull hurt the cow");
        let effect = cow.effects.get(MobEffect::WITHER).cloned();
        assert_eq!(
            effect.map(|e| (e.duration, e.amplifier())),
            Some((ticks, 1))
        );
        assert!(cx.discarded.contains(&skull));
        assert!(cx.actions.iter().any(|a| matches!(
            a,
            WorldAction::Explode(e) if e.radius == 1.0 && e.indirect == Some(wither)
        )));
        level.entities = ents;
    }
}

#[test]
fn powered_wither_ignores_arrows() {
    let (mut level, wither, _) = summon(true, true);
    let wither = wither.expect("summoned");
    let arrow = level.entities.spawn_arrow(
        NewEntity::new(
            entity_ids::ARROW,
            level.network_ids().next(),
            11,
            Vec3::new(4.5, -58.0, 4.5),
        ),
        ArrowData {
            owner: None,
            owner_uuid: None,
            base_damage: 2.0,
            in_ground: false,
            life: 0,
            shake: 0,
            crit: false,
            left_owner: true,
            last_state: None,
            tick_count: 0,
            pickup: 0,
            random_state: 0,
            knockback: 0.0,
            fire: 0,
            touching_water: false,
            item: crate::item::ItemStack::default(),
            in_ground_time: 0,
            spectral_duration: 200,
            has_been_shot: false,
            pierce_level: 0,
            pierced: Vec::new(),
            pierced_killed: Vec::new(),
            crossbow_sound: false,
            weapon: None,
            last_deflected: None,
        },
    );
    if let Some(m) = level.entities.mob_mut(wither) {
        if let Some(s) = state_mut(m) {
            s.invulnerable_ticks = 0;
        }
        m.health = m.max_health();
    }
    let types = DamageTypes::built_in();
    let source = DamageSource {
        direct: Some(arrow),
        ..DamageSource::of(types.generic)
    };
    let mut cx = MobContext::default();
    let hits = |level: &mut Level, cx: &mut MobContext| {
        let facts = BlockFacts::default();
        let rules = GameRules::default();
        let mut random = LegacyRandom::new(3);
        let mut out = Vec::new();
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
        let mut env = Env {
            hooks: &crate::hooks::NoHooks,
            hook_world: crate::hooks::HookWorld::NONE,
            types: &types,
            facts: &facts,
            blocks: &NoBlocks,
            rules: &rules,
            difficulty: Difficulty::Normal,
            game_time: 0,
            min_y: -64,
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
        let mut ents = std::mem::take(&mut level.entities);
        let r = filter_hurt(level, &mut ents, &mut env, cx, wither, &source, 5.0);
        level.entities = ents;
        r
    };
    assert_eq!(
        hits(&mut level, &mut cx),
        Some(5.0),
        "arrows hurt at full health"
    );
    if let Some(m) = level.entities.mob_mut(wither) {
        m.health = m.max_health() / 2.0;
    }
    assert_eq!(
        hits(&mut level, &mut cx),
        None,
        "the powered wither deflects arrows"
    );
}
