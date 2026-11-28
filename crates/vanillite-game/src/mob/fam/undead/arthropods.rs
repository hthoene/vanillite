//! The small monsters: cave spiders, silverfish and endermites.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::Compound;
use vanillite_world::entity::{Dimensions, EntityId};

use super::{Goal, State, goal, state, state_mut};
use crate::effect as effects;
use crate::effect::EffectInstance;
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::common::{self, CommonGoal};
use crate::mob::fam::{Hurt, Interact, SpawnContext, SpawnReason};
use crate::mob::goals::{self, MeleeKind};
use crate::mob::nav::{self, NavKind};
use crate::mob::{Environment, Kind, MobData, Tick, WorldAction, kinds, randompos};
use crate::player::encode::DataWriter;
use crate::survival::rules::{Difficulty, Rule};

/// The state of a silverfish or endermite.
#[derive(Clone, Debug, Default)]
pub struct ArthropodState {
    /// `SilverfishWakeUpFriendsGoal.lookForFriends`.
    pub look_for_friends: i32,
    /// `Endermite.life`.
    pub life: i32,
}

fn st_mut(m: &mut MobData) -> &mut ArthropodState {
    let s = state_mut(m);
    if !matches!(s, State::Arthropod(_)) {
        *s = State::Arthropod(ArthropodState::default());
    }
    match s {
        State::Arthropod(a) => a,
        _ => unreachable!("the arthropod state was just set"),
    }
}

fn st(m: &MobData) -> Option<&ArthropodState> {
    match state(m) {
        State::Arthropod(a) => Some(a),
        _ => None,
    }
}

// --- Properties -------------------------------------------------------------------------

pub fn ambient_sound(m: &MobData) -> Option<&'static str> {
    Some(match m.kind {
        Kind::CaveSpider => "minecraft:entity.spider.ambient",
        Kind::Silverfish => "minecraft:entity.silverfish.ambient",
        _ => "minecraft:entity.endermite.ambient",
    })
}

pub fn hurt_sound(m: &MobData) -> &'static str {
    match m.kind {
        Kind::CaveSpider => "minecraft:entity.spider.hurt",
        Kind::Silverfish => "minecraft:entity.silverfish.hurt",
        _ => "minecraft:entity.endermite.hurt",
    }
}

pub fn death_sound(m: &MobData) -> &'static str {
    match m.kind {
        Kind::CaveSpider => "minecraft:entity.spider.death",
        Kind::Silverfish => "minecraft:entity.silverfish.death",
        _ => "minecraft:entity.endermite.death",
    }
}

pub fn experience_reward(m: &mut MobData) -> i32 {
    let xp = if m.kind == Kind::Endermite { 3 } else { 5 };
    super::zombies::equipment_experience(m, xp)
}

pub fn dimensions(_m: &MobData) -> Option<Dimensions> {
    None
}

/// `InfestedBlock.isCompatibleHostBlock` and the infested block of a host.
pub fn infested_of(state: vanillite_data::block::BlockState) -> Option<&'static str> {
    Some(match state.block().name() {
        "minecraft:stone" => "minecraft:infested_stone",
        "minecraft:cobblestone" => "minecraft:infested_cobblestone",
        "minecraft:stone_bricks" => "minecraft:infested_stone_bricks",
        "minecraft:mossy_stone_bricks" => "minecraft:infested_mossy_stone_bricks",
        "minecraft:cracked_stone_bricks" => "minecraft:infested_cracked_stone_bricks",
        "minecraft:chiseled_stone_bricks" => "minecraft:infested_chiseled_stone_bricks",
        "minecraft:deepslate" => "minecraft:infested_deepslate",
        _ => return None,
    })
}

/// `Silverfish.getWalkTargetValue`: host blocks below are best.
pub fn silverfish_walk_value(level: &Level, env: &Environment, pos: BlockPos) -> f64 {
    if infested_of(level.block(BlockPos::new(pos.x, pos.y - 1, pos.z))).is_some() {
        10.0
    } else {
        kinds::base_walk_target_value(true, level, env, pos)
    }
}

pub fn check_spawn_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    use super::zombies::{is_spawner, mob_rules, monster_rules};
    match kind {
        Kind::CaveSpider => monster_rules(kind, level, env, pos, random, reason),
        _ => {
            // `checkSilverfishSpawnRules` / `checkEndermiteSpawnRules`: any light, no
            // player within 5 blocks.
            if !mob_rules(kind, level, pos, reason) {
                return false;
            }
            if is_spawner(reason) {
                return true;
            }
            let c = Vec3::new(
                f64::from(pos.x) + 0.5,
                f64::from(pos.y) + 0.5,
                f64::from(pos.z) + 0.5,
            );
            let area = vanillite_base::Aabb::new(
                c.x - 5.0,
                c.y - 5.0,
                c.z - 5.0,
                c.x + 5.0,
                c.y + 5.0,
                c.z + 5.0,
            );
            let ents = &level.entities;
            let mut near = false;
            ents.store.for_each_in(&area, true, |id| {
                if let (Some(p), Some(e)) = (ents.player(id), ents.store.get(id))
                    && p.game_mode != vanillite_config::GameMode::Spectator
                    && e.pos().distance_to_sqr(c) < 25.0
                {
                    near = true;
                }
            });
            !near
        }
    }
}

// --- Setup ------------------------------------------------------------------------------

pub fn init(m: &mut MobData) {
    match m.kind {
        Kind::CaveSpider => {
            m.nav.kind = NavKind::WallClimber;
        }
        _ => {
            st_mut(m);
        }
    }
}

/// `!armadillo.isScared()`.
fn armadillo(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.mob(id).is_some_and(|m| {
        m.kind == Kind::Armadillo && !crate::mob::fam::brainy::armadillo::is_scared(m)
    })
}

pub fn register_goals(m: &mut MobData) {
    let g = &mut m.goals;
    let tg = &mut m.targets;
    match m.kind {
        Kind::CaveSpider | Kind::Spider => {
            // `Spider.registerGoals`.
            g.add(1, goals::Goal::Float);
            g.add(
                2,
                goals::Goal::Common(
                    CommonGoal::avoid(armadillo, 6.0, 1.0, 1.2).of_kinds(&common::ARMADILLOS),
                ),
            );
            g.add(
                3,
                goals::Goal::Leap {
                    yd: 0.4,
                    target: None,
                },
            );
            g.add(4, goals::Goal::melee(MeleeKind::Spider, 1.0, true));
            g.add(5, goals::Goal::stroll(0.8));
            g.add(6, goals::Goal::look_at_player(8.0));
            g.add(6, goals::Goal::random_look());
            tg.add(1, goals::Goal::hurt_by(false));
            tg.add(2, goals::Goal::spider_target());
            tg.add(
                3,
                goal(Goal::SpiderTarget(
                    CommonGoal::nearest(10, true, common::iron_golem)
                        .of_kinds(&common::IRON_GOLEMS),
                )),
            );
            m.nav.can_float = true;
        }
        Kind::Silverfish => {
            g.add(1, goals::Goal::Float);
            g.add(3, goal(Goal::SilverfishWakeUpFriends));
            g.add(4, goals::Goal::melee(MeleeKind::Plain, 1.0, false));
            g.add(
                5,
                goal(Goal::SilverfishMergeWithStone {
                    wanted: Vec3::ZERO,
                    direction: 0,
                    do_merge: false,
                }),
            );
            tg.add(1, goals::Goal::hurt_by(true));
            tg.add(2, goals::Goal::nearest_player(true));
            m.nav.can_float = true;
        }
        _ => {
            g.add(1, goals::Goal::Float);
            g.add(2, goals::Goal::melee(MeleeKind::Plain, 1.0, false));
            g.add(3, goals::Goal::stroll(1.0));
            g.add(7, goals::Goal::look_at_player(8.0));
            g.add(8, goals::Goal::random_look());
            tg.add(1, goals::Goal::hurt_by(true));
            tg.add(2, goals::Goal::nearest_player(true));
            m.nav.can_float = true;
        }
    }
}

/// `CaveSpider.finalizeSpawn` does nothing (not even `Mob.finalizeSpawn`; `kinds.rs` skips
/// it); silverfish and endermites only have `Mob.finalizeSpawn`.
pub fn finalize_spawn(_m: &mut MobData, _ctx: &mut SpawnContext<'_>) {}

// --- Tick -------------------------------------------------------------------------------

pub fn before_tick(t: &mut Tick<'_, '_>) {
    // `Silverfish.tick` / `Endermite.tick`: the body follows the head's yaw.
    if matches!(t.mob_ref().kind, Kind::Silverfish | Kind::Endermite) {
        let y_rot = t.core().rot().y_rot;
        t.mob().y_body_rot = y_rot;
    }
}

pub fn after_tick(t: &mut Tick<'_, '_>) {
    if t.mob_ref().kind == Kind::CaveSpider {
        // `Spider.tick`: climbing against walls.
        let climbing = t.mob_ref().horizontal_collision;
        if climbing != t.mob_ref().climbing {
            t.mob().climbing = climbing;
            t.core_mut().mark_data_dirty();
        }
    }
}

pub fn ai_step(t: &mut Tick<'_, '_>) {
    if t.mob_ref().kind == Kind::Endermite {
        let persistent = t.mob_ref().persistent;
        let s = st_mut(t.mob());
        if !persistent {
            s.life += 1;
        }
        if s.life >= 2400 {
            t.mob().removed = true;
        }
    }
}

pub fn custom_server_ai_step(_t: &mut Tick<'_, '_>) {}

pub fn move_control(_t: &mut Tick<'_, '_>) -> bool {
    false
}

pub fn travel(_t: &mut Tick<'_, '_>, _input: Vec3) -> bool {
    false
}

// --- Combat ---------------------------------------------------------------------------------

/// `CaveSpider.doHurtTarget`: poison for 7 (Normal) or 15 (Hard) seconds.
pub fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
    if t.mob_ref().kind != Kind::CaveSpider {
        return None;
    }
    let hit = crate::mob::combat::base_mob_attack(t, target);
    if hit {
        let seconds = match t.env.difficulty {
            Difficulty::Normal => 7,
            Difficulty::Hard => 15,
            _ => 0,
        };
        if seconds > 0 {
            let me = t.id;
            super::add_effect(
                t.ents,
                target,
                EffectInstance::new(effects::MobEffect::POISON, seconds * 20, 0),
                Some(me),
            );
        }
    }
    Some(hit)
}

/// `Silverfish.hurtServer`: hits by an entity (or `#always_triggers_silverfish`) wake the
/// friends.
pub fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
    let source = h.source;
    let Some(m) = h.ents.mob_mut(h.target) else {
        return Some(amount);
    };
    if m.kind == Kind::Silverfish
        // `#always_triggers_silverfish` is magic.
        && (source.cause.is_some() || source.kind == h.env.types.magic)
    {
        let s = st_mut(m);
        if s.look_for_friends == 0 {
            s.look_for_friends = goals::reduced(20);
        }
    }
    Some(amount)
}

pub fn after_hurt(_h: &mut Hurt<'_, '_>, _amount: f32) {}

pub fn on_die(_h: &mut Hurt<'_, '_>) {}

pub fn interact(_ix: &mut Interact<'_, '_>) -> Interaction {
    Interaction::Pass
}

pub fn pick_up_items(t: &mut Tick<'_, '_>) {
    let rules = crate::mob::equipment::PickupRules::default();
    crate::mob::equipment::pick_up_items(t, &rules, None, (1.0, 0.0, 1.0));
}

// --- Client data and storage ------------------------------------------------------------

pub fn encode_data(w: &mut DataWriter<'_>, m: &MobData) {
    if m.kind == Kind::CaveSpider {
        w.byte(16, u8::from(m.climbing), 0);
    }
}

pub fn save(m: &MobData, out: &mut Compound) {
    if m.kind == Kind::Endermite {
        out.insert("Lifetime", st(m).map_or(0, |s| s.life));
    }
}

pub fn load(m: &mut MobData, tag: &Compound) {
    if m.kind == Kind::Endermite {
        st_mut(m).life = crate::mob::entity_nbt::get_int(tag, "Lifetime").unwrap_or(0);
    }
}

// --- Goals --------------------------------------------------------------------------------

pub fn goal_flags(g: &Goal) -> u8 {
    use crate::mob::goals::flag::{MOVE, TARGET};
    match g {
        Goal::SpiderTarget(_) => TARGET,
        Goal::SilverfishMergeWithStone { .. } => MOVE,
        _ => 0,
    }
}

pub fn goal_every_tick(_g: &Goal) -> bool {
    false
}

/// `Direction.from3DDataValue` order: down, up, north, south, west, east.
fn direction_offset(d: u8) -> (i32, i32, i32) {
    match d {
        0 => (0, -1, 0),
        1 => (0, 1, 0),
        2 => (0, 0, -1),
        3 => (0, 0, 1),
        4 => (-1, 0, 0),
        _ => (1, 0, 0),
    }
}

fn merge_pos(t: &Tick<'_, '_>, direction: u8) -> BlockPos {
    let p = t.pos();
    let base = BlockPos::containing(p.x, p.y + 0.5, p.z);
    let (dx, dy, dz) = direction_offset(direction);
    BlockPos::new(base.x + dx, base.y + dy, base.z + dz)
}

pub fn goal_can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::SpiderTarget(c) => t.light_magic_value() < 0.5 && common::can_use(t, c),
        Goal::SilverfishWakeUpFriends => st(t.mob_ref()).is_some_and(|s| s.look_for_friends > 0),
        Goal::SilverfishMergeWithStone {
            wanted,
            direction,
            do_merge,
        } => {
            if t.mob_ref().target.is_some() || !t.nav_ref().is_done() {
                return false;
            }
            let griefing = t.env.rules.bool(Rule::MOB_GRIEFING);
            if griefing && t.mob().random.next_int_bounded(goals::reduced(10)) == 0 {
                let d = u8::try_from(t.mob().random.next_int_bounded(6)).unwrap_or(0);
                *direction = d;
                let pos = merge_pos(t, d);
                if infested_of(t.level.block(pos)).is_some() {
                    *do_merge = true;
                    return true;
                }
            }
            *do_merge = false;
            // `RandomStrollGoal.canUse` with the interval 10.
            if t.mob_ref().no_action_time >= 100 {
                return false;
            }
            if t.mob().random.next_int_bounded(goals::reduced(10)) != 0 {
                return false;
            }
            match randompos::default_pos(t, 10, 7) {
                Some(p) => {
                    *wanted = p;
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

pub fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::SpiderTarget(c) => common::can_continue(t, c),
        Goal::SilverfishMergeWithStone { do_merge, .. } => !*do_merge && !t.nav_ref().is_done(),
        _ => goal_can_use(t, g),
    }
}

pub fn goal_start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::SpiderTarget(c) => common::start(t, c),
        Goal::SilverfishMergeWithStone {
            wanted,
            direction,
            do_merge,
        } => {
            if !*do_merge {
                nav::move_to_pos(t, *wanted, 1.0);
                return;
            }
            let pos = merge_pos(t, *direction);
            let Some(infested) = infested_of(t.level.block(pos)) else {
                return;
            };
            if let Some(block) = vanillite_data::block::BlockId::by_name(infested) {
                t.cx.actions
                    .push(WorldAction::SetBlock(pos, block.default_state(), 3));
            }
            let id = t.id;
            crate::mob::living::entity_event(t, id, crate::mob::living::event::SPAWN_POOF);
            t.mob().removed = true;
        }
        _ => {}
    }
}

pub fn goal_stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    if let Goal::SpiderTarget(c) = g {
        common::stop(t, c);
    }
}

/// `SilverfishWakeUpFriendsGoal.tick`: infested blocks around break (or turn back with
/// mob griefing off), each with a half chance to stop.
pub fn goal_tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    if !matches!(g, Goal::SilverfishWakeUpFriends) {
        return;
    }
    let s = st_mut(t.mob());
    s.look_for_friends -= 1;
    if s.look_for_friends > 0 {
        return;
    }
    let base = t.block_pos();
    let griefing = t.env.rules.bool(Rule::MOB_GRIEFING);
    let mut y = 0;
    while (-5..=5).contains(&y) {
        let mut x = 0;
        while (-10..=10).contains(&x) {
            let mut z = 0;
            while (-10..=10).contains(&z) {
                let pos = BlockPos::new(base.x + x, base.y + y, base.z + z);
                let state = t.level.block(pos);
                if let Some(host) = state.block().name().strip_prefix("minecraft:infested_") {
                    if griefing {
                        t.cx.actions.push(WorldAction::DestroyBlock(pos, true));
                    } else if let Some(b) =
                        vanillite_data::block::BlockId::by_name(&format!("minecraft:{host}"))
                    {
                        t.cx.actions
                            .push(WorldAction::SetBlock(pos, b.default_state(), 3));
                    }
                    if t.mob().random.next_boolean() {
                        return;
                    }
                }
                z = i32::from(z <= 0) - z;
            }
            x = i32::from(x <= 0) - x;
        }
        y = i32::from(y <= 0) - y;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_construct_with_goals() {
        for kind in [Kind::CaveSpider, Kind::Silverfish, Kind::Endermite] {
            let m = MobData::new(kind, 3);
            assert!(!m.goals.slots.is_empty(), "{kind:?}");
            assert!(!m.targets.slots.is_empty(), "{kind:?}");
        }
        assert_eq!(
            MobData::new(Kind::CaveSpider, 1).nav.kind,
            NavKind::WallClimber
        );
    }

    #[test]
    fn endermite_lifetime_round_trip() {
        let mut m = MobData::new(Kind::Endermite, 3);
        st_mut(&mut m).life = 1234;
        let mut out = Compound::new();
        save(&m, &mut out);
        let mut n = MobData::new(Kind::Endermite, 4);
        load(&mut n, &out);
        assert_eq!(st(&n).map(|s| s.life), Some(1234));
    }

    #[test]
    fn host_blocks() {
        let stone =
            vanillite_data::block::BlockId::by_name("minecraft:stone").map(|b| b.default_state());
        assert_eq!(
            stone.and_then(infested_of),
            Some("minecraft:infested_stone")
        );
    }
}
