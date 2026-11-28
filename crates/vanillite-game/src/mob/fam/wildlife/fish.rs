//! Fish (`AbstractFish`, `AbstractSchoolingFish`): cod, salmon, pufferfish and tropical fish.
//! They swim with `FishMoveControl`, flop on land, school behind a leader, can be caught in
//! a bucket; pufferfish puff up and poison what touches them.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_nbt::{Compound, Tag};
use vanillite_world::entity::{EntityFlags, EntityId};

use super::{Goal, add_effect, rotlerp, st, st_mut};
use crate::effect::MobEffect;
use crate::item::ItemStack;
use crate::level::Level;
use crate::mob::animal::{Interaction, fill_held};
use crate::mob::common::CommonGoal;
use crate::mob::fam::{Interact, SpawnContext};
use crate::mob::goals::{self, Goal as MobGoal, reduced};
use crate::mob::kinds::Kind;
use crate::mob::nav::Operation;
use crate::mob::{MobData, Tick, combat, is_water, nav, randompos};

/// `TropicalFish.DEFAULT_VARIANT`: kob, white on white.
pub(crate) const DEFAULT_TROPICAL_VARIANT: i32 = 0;

/// The packed ids of the tropical fish patterns (`base | index << 8`), in `Pattern` order.
const PATTERNS: [i32; 12] = [0, 256, 512, 768, 1024, 1280, 1, 257, 513, 769, 1025, 1281];

/// `TropicalFish.packVariant(pattern, baseColor, patternColor)`.
pub(crate) fn pack_variant(pattern: i32, base: i32, pattern_color: i32) -> i32 {
    pattern & 0xFFFF | (base & 0xFF) << 16 | (pattern_color & 0xFF) << 24
}

/// `TropicalFish.COMMON_VARIANTS` as (pattern index, base colour, pattern colour).
const COMMON: [(usize, i32, i32); 22] = [
    (7, 1, 7),   // stripey orange gray
    (6, 7, 7),   // flopper gray gray
    (6, 7, 11),  // flopper gray blue
    (11, 0, 7),  // clayfish white gray
    (1, 11, 7),  // sunstreak blue gray
    (0, 1, 0),   // kob orange white
    (5, 6, 3),   // spotty pink light blue
    (9, 10, 4),  // blockfish purple yellow
    (11, 0, 14), // clayfish white red
    (5, 0, 4),   // spotty white yellow
    (8, 0, 7),   // glitter white gray
    (11, 0, 1),  // clayfish white orange
    (3, 9, 6),   // dasher cyan pink
    (4, 5, 3),   // brinely lime light blue
    (10, 14, 0), // betty red white
    (2, 7, 14),  // snooper gray red
    (9, 14, 0),  // blockfish red white
    (6, 0, 4),   // flopper white yellow
    (0, 14, 0),  // kob red white
    (1, 7, 0),   // sunstreak gray white
    (3, 9, 4),   // dasher cyan yellow
    (6, 4, 4),   // flopper yellow yellow
];

/// The common variant `i` packed.
pub(crate) fn common_variant(i: usize) -> i32 {
    let (p, b, c) = COMMON[i];
    pack_variant(PATTERNS[p], b, c)
}

/// `getMaxSchoolSize`: salmon swim in fives, the others in eights.
fn max_school_size(kind: Kind) -> i32 {
    if kind == Kind::Salmon { 5 } else { 8 }
}

fn is_schooling(kind: Kind) -> bool {
    matches!(kind, Kind::Cod | Kind::Salmon | Kind::TropicalFish)
}

/// `AbstractFish.registerGoals` and the kinds' additions.
pub(crate) fn register_goals(m: &mut MobData) {
    let kind = m.kind;
    let g = &mut m.goals;
    g.add(0, MobGoal::panic(1.25));
    g.add(
        2,
        MobGoal::Common(CommonGoal::avoid(non_spectator_player, 8.0, 1.6, 1.4)),
    );
    g.add(
        4,
        MobGoal::Fam(crate::mob::fam::FamGoal::Wildlife(Goal::Swim {
            speed: 1.0,
            interval: 40,
            fish: true,
            wanted: Vec3::ZERO,
        })),
    );
    if is_schooling(kind) {
        // `FollowFlockLeaderGoal(mob)`: the first start delay is rolled at construction.
        let next_start = reduced(200 + m.random.next_int_bounded(200) % 20);
        m.goals.add(
            5,
            MobGoal::Fam(crate::mob::fam::FamGoal::Wildlife(
                Goal::FollowFlockLeader {
                    recalc: 0,
                    next_start,
                },
            )),
        );
    }
    if kind == Kind::Pufferfish {
        m.goals.add(
            1,
            MobGoal::Fam(crate::mob::fam::FamGoal::Wildlife(Goal::Puff)),
        );
    }
}

/// `EntitySelector.NO_SPECTATORS` for players.
fn non_spectator_player(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents
        .player(id)
        .is_some_and(|p| p.game_mode != GameMode::Spectator)
}

/// `Salmon.finalizeSpawn`: the size from the salmon's own random (weighted: small 30, medium
/// 50, large 15).
pub(super) fn finalize_spawn_own(m: &mut MobData) {
    if m.kind == Kind::Salmon {
        let roll = m.random.next_int_bounded(95);
        st_mut(m).variant = if roll < 30 {
            0
        } else if roll < 80 {
            1
        } else {
            2
        };
    }
}

/// `finalizeSpawn` of tropical fish (variant).
pub(super) fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    if m.kind != Kind::TropicalFish {
        return;
    }
    let variant = if let crate::mob::fam::GroupData::TropicalFish(v) = ctx.group {
        v
    } else if m.random.next_float() < 0.9 {
        let i = usize::try_from(m.random.next_int_bounded(22)).unwrap_or(0);
        let v = common_variant(i);
        ctx.group = crate::mob::fam::GroupData::TropicalFish(v);
        v
    } else {
        st_mut(m).is_school = false;
        let p = usize::try_from(m.random.next_int_bounded(12)).unwrap_or(0);
        let base = m.random.next_int_bounded(16);
        let color = m.random.next_int_bounded(16);
        pack_variant(PATTERNS[p], base, color)
    };
    st_mut(m).variant = variant;
}

/// `WaterAnimal.checkSurfaceWaterAnimalSpawnRules`: within 13 blocks below the sea level, in
/// water with water above.
pub(super) fn surface_water_rules(level: &Level, pos: BlockPos) -> bool {
    let sea_level = crate::mob::sea_level(level);
    pos.y >= sea_level - 13
        && pos.y <= sea_level
        && is_water(level.block(BlockPos::new(pos.x, pos.y - 1, pos.z)))
        && level.block(BlockPos::new(pos.x, pos.y + 1, pos.z)).block()
            == vanillite_data::block::ids::WATER
}

/// `TropicalFish.checkTropicalFishSpawnRules`: water below and above, and either a biome of
/// `#allows_tropical_fish_spawns_at_any_height` (lush caves) or the surface rules.
pub(super) fn tropical_rules(level: &Level, pos: BlockPos) -> bool {
    is_water(level.block(BlockPos::new(pos.x, pos.y - 1, pos.z)))
        && level.block(BlockPos::new(pos.x, pos.y + 1, pos.z)).block()
            == vanillite_data::block::ids::WATER
        && (crate::mob::fam::undead::zombies::biome_is(
            level,
            pos,
            "minecraft:allows_tropical_fish_spawns_at_any_height",
        ) || surface_water_rules(level, pos))
}

/// `AbstractFish.aiStep` before `super.aiStep()`: a fish on land flops.
pub(super) fn flop(t: &mut Tick<'_, '_>) {
    let m = t.mob_ref();
    if m.fluids.in_water() || !t.on_ground() || !m.vertical_collision {
        return;
    }
    let r = &mut t.mob().random;
    let x = f64::from((r.next_float() * 2.0 - 1.0) * 0.05);
    let z = f64::from((r.next_float() * 2.0 - 1.0) * 0.05);
    let v = t.velocity();
    t.set_velocity(Vec3::new(v.x + x, v.y + f64::from(0.4_f32), v.z + z));
    {
        let mut core = t.core_mut();
        core.set_flags(EntityFlags::ON_GROUND, false);
        core.set_flags(EntityFlags::HAS_IMPULSE, true);
    }
    let sound = match t.mob_ref().kind {
        Kind::Cod => "minecraft:entity.cod.flop",
        Kind::Salmon => "minecraft:entity.salmon.flop",
        Kind::Pufferfish => "minecraft:entity.puffer_fish.flop",
        _ => "minecraft:entity.tropical_fish.flop",
    };
    let id = t.id;
    combat::mob_sound(t.ents, t.env, id, sound);
}

/// `FishMoveControl.tick`.
pub(super) fn move_control(t: &mut Tick<'_, '_>) {
    if t.mob_ref().fluids.eye_in_water {
        let v = t.velocity();
        t.set_velocity(Vec3::new(v.x, v.y + 0.005, v.z));
    }
    let ctl = t.mob_ref().move_ctl;
    if ctl.operation == Operation::MoveTo && !t.nav_ref().is_done() {
        #[allow(clippy::cast_possible_truncation)]
        let target = (ctl.speed_modifier * t.mob_ref().attrs.movement_speed) as f32;
        let speed = t.mob_ref().speed;
        let speed = speed + 0.125 * (target - speed);
        let m = t.mob();
        m.speed = speed;
        m.zza = speed;
        let p = t.pos();
        let (xd, yd, zd) = (ctl.wanted.x - p.x, ctl.wanted.y - p.y, ctl.wanted.z - p.z);
        if yd != 0.0 {
            let dd = (xd * xd + yd * yd + zd * zd).sqrt();
            let v = t.velocity();
            t.set_velocity(Vec3::new(
                v.x,
                v.y + f64::from(speed) * (yd / dd) * 0.1,
                v.z,
            ));
        }
        if xd != 0.0 || zd != 0.0 {
            #[allow(clippy::cast_possible_truncation)]
            let y_rot_d = crate::mob::atan2_deg(zd, xd) as f32 - 90.0;
            let rot = t.core().rot();
            let y = rotlerp(rot.y_rot, y_rot_d, 90.0);
            t.core_mut()
                .set_rot(vanillite_world::entity::Rotation { y_rot: y, ..rot });
            t.mob().y_body_rot = y;
        }
    } else {
        let m = t.mob();
        m.speed = 0.0;
        m.zza = 0.0;
    }
}

/// `AbstractSchoolingFish.tick`: a leader whose school swam away forgets it.
pub(super) fn after_tick(t: &mut Tick<'_, '_>) {
    let kind = t.mob_ref().kind;
    if !is_schooling(kind) || st(t.mob_ref()).school_size <= 1 {
        return;
    }
    if t.env.random.next_int_bounded(200) != 1 {
        return;
    }
    let area = t.aabb().inflate(8.0, 8.0, 8.0);
    let mut n = 0;
    let ents = &*t.ents;
    ents.store.for_each_in(&area, false, |id| {
        if ents.mob(id).is_some_and(|m| m.kind == kind) {
            n += 1;
        }
    });
    if n <= 1 {
        st_mut(t.mob()).school_size = 1;
    }
}

/// `isFollower`: the leader is alive.
fn is_follower(t: &Tick<'_, '_>, m: &MobData) -> bool {
    st(m)
        .leader
        .is_some_and(|l| t.ents.mob(l).is_some_and(MobData::is_alive))
}

/// `canBeFollowed`: has followers and room for more.
fn can_be_followed(m: &MobData) -> bool {
    let s = st(m);
    s.school_size > 1 && s.school_size < max_school_size(m.kind)
}

/// `startFollowing(leader)`.
fn start_following(t: &mut Tick<'_, '_>, follower: EntityId, leader: EntityId) {
    if let Some(m) = t.ents.mob_mut(follower) {
        st_mut(m).leader = Some(leader);
    }
    if let Some(m) = t.ents.mob_mut(leader) {
        st_mut(m).school_size += 1;
    }
}

/// `stopFollowing`.
fn stop_following(t: &mut Tick<'_, '_>) {
    let leader = st_mut(t.mob()).leader.take();
    if let Some(l) = leader
        && let Some(m) = t.ents.mob_mut(l)
    {
        st_mut(m).school_size -= 1;
    }
}

/// The pufferfish's `TARGETING_CONDITIONS`: no spectators or creative players and none of
/// `#not_scary_for_pufferfish`.
fn scary(t: &Tick<'_, '_>, id: EntityId) -> bool {
    if id == t.id {
        return false;
    }
    if let Some(p) = t.ents.player(id) {
        return !matches!(p.game_mode, GameMode::Creative | GameMode::Spectator)
            && !p.survival.is_dead_or_dying();
    }
    let Some(m) = t.ents.mob(id) else {
        return false;
    };
    m.is_alive()
        && !matches!(
            m.kind,
            Kind::Turtle
                | Kind::Guardian
                | Kind::ElderGuardian
                | Kind::Cod
                | Kind::Pufferfish
                | Kind::Salmon
                | Kind::TropicalFish
                | Kind::Dolphin
                | Kind::Squid
                | Kind::GlowSquid
                | Kind::Tadpole
                | Kind::Nautilus
                | Kind::ZombieNautilus
                | Kind::SulfurCube
        )
}

/// `BehaviorUtils.getRandomSwimmablePos(mob, h, v)`.
pub(super) fn random_swimmable_pos(t: &mut Tick<'_, '_>, h: i32, v: i32) -> Option<Vec3> {
    randompos::swimmable_pos(t, h, v)
}

pub(super) fn can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::Swim {
            interval,
            fish,
            wanted,
            ..
        } => {
            if *fish && is_schooling(t.mob_ref().kind) && is_follower(t, t.mob_ref()) {
                return false;
            }
            if t.mob_ref().no_action_time >= 100 {
                return false;
            }
            if t.mob().random.next_int_bounded(reduced(*interval).max(1)) != 0 {
                return false;
            }
            match random_swimmable_pos(t, 10, 7) {
                Some(p) => {
                    *wanted = p;
                    true
                }
                None => false,
            }
        }
        Goal::FollowFlockLeader { next_start, .. } => {
            let m = t.mob_ref();
            if st(m).school_size > 1 {
                return false;
            }
            if is_follower(t, m) {
                return true;
            }
            if *next_start > 0 {
                *next_start -= 1;
                return false;
            }
            *next_start = reduced(200 + t.mob().random.next_int_bounded(200) % 20);
            let kind = t.mob_ref().kind;
            let area = t.aabb().inflate(8.0, 8.0, 8.0);
            let mut list = std::mem::take(&mut t.cx.ids);
            list.clear();
            {
                let ents = &*t.ents;
                ents.store.for_each_in(&area, false, |id| {
                    if let Some(m) = ents.mob(id)
                        && m.kind == kind
                        && (can_be_followed(m) || !is_follower(t, m))
                    {
                        list.push(id);
                    }
                });
            }
            let leader = list
                .iter()
                .copied()
                .find(|&id| t.ents.mob(id).is_some_and(can_be_followed))
                .unwrap_or(t.id);
            let room = t
                .ents
                .mob(leader)
                .map_or(0, |m| max_school_size(m.kind) - st(m).school_size);
            let mut added = 0;
            for &id in &list {
                if added >= room {
                    break;
                }
                if t.ents.mob(id).is_some_and(|m| is_follower(t, m)) {
                    continue;
                }
                added += 1;
                if id != leader {
                    start_following(t, id, leader);
                }
            }
            t.cx.ids = list;
            is_follower(t, t.mob_ref())
        }
        Goal::Puff => {
            let area = t.aabb().inflate(2.0, 2.0, 2.0);
            let mut found = false;
            let mut list = std::mem::take(&mut t.cx.ids);
            list.clear();
            t.ents.store.for_each_in(&area, false, |id| list.push(id));
            for &id in &list {
                if scary(t, id) {
                    found = true;
                    break;
                }
            }
            t.cx.ids = list;
            found
        }
        _ => false,
    }
}

pub(super) fn can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
    match g {
        Goal::Swim { .. } => !t.nav_ref().is_done() && t.ents.riding.passengers_of(t.id).is_empty(),
        Goal::FollowFlockLeader { .. } => {
            let m = t.mob_ref();
            if !is_follower(t, m) {
                return false;
            }
            let leader = st(m).leader;
            leader
                .and_then(|l| t.other(l))
                .is_some_and(|o| goals::distance_sqr(t, o.pos) <= 121.0)
        }
        _ => can_use(t, g),
    }
}

pub(super) fn start(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::Swim { speed, wanted, .. } => {
            nav::move_to_pos(t, *wanted, *speed);
        }
        Goal::FollowFlockLeader { recalc, .. } => *recalc = 0,
        Goal::Puff => {
            let s = st_mut(t.mob());
            s.inflate_counter = 1;
            s.deflate_timer = 0;
        }
        _ => {}
    }
}

pub(super) fn stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
    match g {
        Goal::FollowFlockLeader { .. } => stop_following(t),
        Goal::Puff => st_mut(t.mob()).inflate_counter = 0,
        _ => {}
    }
}

pub(super) fn tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
    if let Goal::FollowFlockLeader { recalc, .. } = g {
        *recalc -= 1;
        if *recalc <= 0 {
            *recalc = reduced(10);
            // `pathToLeader`.
            if let Some(o) = st(t.mob_ref()).leader.and_then(|l| t.other(l)) {
                nav::move_to_entity(t, o.pos, 1, 1.0);
            }
        }
    }
}

/// Sets the puff state (the size follows it).
fn set_puff_state(t: &mut Tick<'_, '_>, state: i32) {
    st_mut(t.mob()).puff_state = state;
    let dims = crate::mob::kinds::mob_dimensions(t.mob_ref());
    let mut core = t.core_mut();
    core.set_dims(dims);
    core.mark_data_dirty();
}

/// The puff state step of `Pufferfish.tick`: the counters advance; returns the new state and
/// its sound when the state changes.
pub(crate) fn puff_transition(s: &mut super::State) -> Option<(i32, &'static str)> {
    const UP: &str = "minecraft:entity.puffer_fish.blow_up";
    const OUT: &str = "minecraft:entity.puffer_fish.blow_out";
    let mut change = None;
    if s.inflate_counter > 0 {
        if s.puff_state == 0 {
            change = Some((1, UP));
        } else if s.inflate_counter > 40 && s.puff_state == 1 {
            change = Some((2, UP));
        }
        s.inflate_counter += 1;
    } else if s.puff_state != 0 {
        if s.deflate_timer > 60 && s.puff_state == 2 {
            change = Some((1, OUT));
        } else if s.deflate_timer > 100 && s.puff_state == 1 {
            change = Some((0, OUT));
        }
        s.deflate_timer += 1;
    }
    change
}

/// `Pufferfish.tick` before `super.tick()`: inflate while something scary is near, deflate
/// slowly afterwards.
pub(super) fn puffer_tick(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().is_alive() || t.mob_ref().no_ai {
        return;
    }
    if let Some((state, sound)) = puff_transition(st_mut(t.mob())) {
        let id = t.id;
        combat::mob_sound(t.ents, t.env, id, sound);
        set_puff_state(t, state);
    }
}

/// `Pufferfish.aiStep` and `playerTouch`: a puffed fish stings the mobs and players touching it.
pub(super) fn puffer_ai_step(t: &mut Tick<'_, '_>) {
    let puff = st(t.mob_ref()).puff_state;
    if !t.mob_ref().is_alive() || puff <= 0 {
        return;
    }
    let mob_area = t.aabb().inflate(0.3, 0.3, 0.3);
    // `Player.touch` checks the entities in the player's box inflated by (1, 0.5, 1).
    let player_area = t.aabb().inflate(1.0, 0.5, 1.0);
    let mut list = std::mem::take(&mut t.cx.ids);
    list.clear();
    t.ents
        .store
        .for_each_in(&player_area, false, |id| list.push(id));
    #[allow(clippy::cast_precision_loss)]
    let damage = (1 + puff) as f32;
    for &id in &list {
        if !t.exists() {
            break;
        }
        let is_player = t.ents.player(id).is_some();
        let touches = if is_player {
            true
        } else {
            t.ents
                .store
                .get(id)
                .is_some_and(|e| e.aabb().intersects(&mob_area))
        };
        if !touches || !scary(t, id) {
            continue;
        }
        let source = combat::entity_source(t.ents, t.env.types.mob_attack, t.id, t.id);
        if combat::hurt(t.level, t.ents, t.env, t.cx, id, source, damage) {
            let me = t.id;
            add_effect(t.ents, id, MobEffect::POISON, 60 * puff, 0, Some(me));
            if !is_player {
                crate::mob::kinds::play_sound(t, "minecraft:entity.puffer_fish.sting", 1.0, 1.0);
            }
        }
    }
    t.cx.ids = list;
}

/// The bucket item of a fish kind.
fn bucket_item(kind: Kind) -> &'static str {
    match kind {
        Kind::Cod => "minecraft:cod_bucket",
        Kind::Salmon => "minecraft:salmon_bucket",
        Kind::Pufferfish => "minecraft:pufferfish_bucket",
        _ => "minecraft:tropical_fish_bucket",
    }
}

/// A data component id.
fn component(name: &str) -> Option<u16> {
    vanillite_data::registry::DATA_COMPONENT_TYPE.id(name)
}

/// A var int component value.
fn varint_value(v: i32) -> Box<[u8]> {
    let mut out = Vec::new();
    vanillite_protocol::codec::write_varint(&mut out, v);
    out.into()
}

/// `saveToBucketTag`: the custom name, `bucket_entity_data` and the kind's components.
pub(super) fn bucket_stack(m: &MobData, flags: vanillite_world::entity::EntityFlags) -> ItemStack {
    let (mut stack, data) = crate::mob::fam::bucket_default(m, flags, bucket_item(m.kind));
    crate::mob::fam::set_bucket_data(&mut stack, data);
    let s = st(m);
    match m.kind {
        Kind::Salmon => {
            if let Some(id) = component("minecraft:salmon/size") {
                stack.set_component(id, varint_value(s.variant));
            }
        }
        Kind::TropicalFish => {
            let v = s.variant;
            for (name, value) in [
                ("minecraft:tropical_fish/pattern", v & 0xFFFF),
                ("minecraft:tropical_fish/base_color", v >> 16 & 0xFF),
                ("minecraft:tropical_fish/pattern_color", v >> 24 & 0xFF),
            ] {
                if let Some(id) = component(name) {
                    stack.set_component(id, varint_value(value));
                }
            }
        }
        _ => {}
    }
    stack
}

/// `AbstractFish.mobInteract`: `Bucketable.bucketMobPickup` with a water bucket.
pub(super) fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
    if crate::item::item_id(ix.stack).name() != "minecraft:water_bucket" {
        return Interaction::Pass;
    }
    let Some(m) = ix.ents.mob(ix.target).filter(|m| m.is_alive()) else {
        return Interaction::Pass;
    };
    let flags = ix
        .ents
        .store
        .get(ix.target)
        .map(|e| e.flags())
        .unwrap_or_default();
    let bucket = bucket_stack(m, flags);
    // `pickupEntity.playSound(getPickupSound(), 1.0F, 1.0F)`: no voice pitch, no draws.
    let source = m.sound_source();
    if let Some(pos) = ix.ents.store.get(ix.target).map(|e| e.pos()) {
        crate::survival::play_sound(
            ix.env.out,
            None,
            pos,
            "minecraft:item.bucket.fill_fish",
            source,
            1.0,
            1.0,
            crate::survival::sound_seed(),
        );
    }
    crate::mob::animal::filled_bucket(ix.ents, ix.player, &bucket);
    fill_held(ix.ents, ix.env, ix.player, ix.hand, bucket);
    crate::mob::tame::drop_leash(ix.ents, ix.env, ix.target, true);
    if let Some(m) = ix.ents.mob_mut(ix.target) {
        m.removed = true;
    }
    ix.cx.discarded.push(ix.target);
    Interaction::Success
}

/// `addAdditionalSaveData` of the fish kinds.
pub(super) fn save(m: &MobData, out: &mut Compound) {
    let s = st(m);
    out.insert("FromBucket", s.from_bucket);
    match m.kind {
        Kind::Salmon => out.insert(
            "type",
            match s.variant {
                0 => "small",
                2 => "large",
                _ => "medium",
            },
        ),
        Kind::Pufferfish => out.insert("PuffState", s.puff_state),
        Kind::TropicalFish => out.insert("Variant", s.variant),
        _ => {}
    }
}

/// `readAdditionalSaveData` of the fish kinds.
pub(super) fn load(m: &mut MobData, tag: &Compound) {
    let kind = m.kind;
    let s = st_mut(m);
    s.from_bucket = super::boolean(tag, "FromBucket", false);
    match kind {
        Kind::Salmon => {
            s.variant = match tag.get("type").and_then(Tag::as_str) {
                Some("small") => 0,
                Some("large") => 2,
                _ => 1,
            };
        }
        Kind::Pufferfish => s.puff_state = super::int(tag, "PuffState").unwrap_or(0).min(2),
        Kind::TropicalFish => {
            s.variant = super::int(tag, "Variant").unwrap_or(DEFAULT_TROPICAL_VARIANT);
        }
        _ => {}
    }
}

/// `setFromBucket(true)` of the released fish.
pub(crate) fn from_bucket(m: &mut MobData) {
    st_mut(m).from_bucket = true;
}
