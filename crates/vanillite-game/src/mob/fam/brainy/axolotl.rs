//! Axolotls (`Axolotl`, `AxolotlAi`, `PlayDead`, `ValidatePlayDead`,
//! `AxolotlAttackablesSensor`): amphibious hunters of fish and guardians that play dead,
//! dry out on land and go into buckets.

use vanillite_base::BlockPos;
use vanillite_base::random::RandomSource;
use vanillite_nbt::Compound;
use vanillite_world::entity::EntityId;

use super::{SpawnContext, nbt_bool, nbt_int, spec, state, state_mut};
use crate::mob::animal::Interaction;
use crate::mob::brain::behavior::{self as bh, Shape, Spec};
use crate::mob::brain::memory::MemoryStatus as S;
use crate::mob::brain::{
    self, Activity, ActivitySpec, Brain, BrainSpec, MemValue, MemoryStatus, MemoryType as M,
    SensorKind, brain_ref, sensing,
};
use crate::mob::fam::Interact;
use crate::mob::nav::{LookKind, MoveKind, NavKind};
use crate::mob::path::{MalusTable, PathType};
use crate::mob::{MobData, Tick};
use crate::player::encode::DataWriter;
use crate::survival::damage::DamageSource;

/// Water is no obstacle.
pub const MALUS: &MalusTable = &[(PathType::Water, 0.0)];

/// `AxolotlAttackablesSensor.requires`.
pub const SENSOR_MEMORIES: &[M] = &[
    M::NearestAttackable,
    M::NearestVisibleLivingEntities,
    M::HasHuntingCooldown,
];

/// The variant names by id (`Axolotl.Variant`).
pub const VARIANTS: [&str; 5] = ["lucy", "wild", "gold", "cyan", "blue"];

/// The axolotl behaviours.
#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    PlayDead,
    ValidatePlayDead,
}

fn ax(b: Behavior) -> Spec {
    spec(super::Behavior::Axolotl(b))
}

fn in_water(t: &Tick<'_, '_>) -> bool {
    t.mob_ref().fluids.in_water()
}

fn speed(t: &Tick<'_, '_>) -> f32 {
    if in_water(t) { 0.5 } else { 0.15 }
}

fn chase_speed(t: &Tick<'_, '_>) -> f32 {
    if in_water(t) { 0.6 } else { 0.15 }
}

fn is_breeding(t: &mut Tick<'_, '_>) -> bool {
    brain_ref(t).has(M::BreedTarget)
}

fn is_player(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.player(id).is_some()
}

/// `canSetWalkTargetFromLookTarget`: the look target is in water exactly when we are.
fn look_target_same_medium(t: &mut Tick<'_, '_>) -> bool {
    let Some(look) = brain_ref(t).look_target() else {
        return false;
    };
    let Some(pos) = bh::tracker_block(t, &look) else {
        return false;
    };
    crate::mob::is_water(t.level.block(pos)) == in_water(t)
}

pub(super) fn brain_spec() -> BrainSpec {
    let core = ActivitySpec::new(
        Activity::Core,
        0,
        vec![
            Spec::look_at_target_sink(45, 90),
            Spec::move_to_target_sink(),
            ax(Behavior::ValidatePlayDead),
            Spec::count_down(M::TemptationCooldownTicks),
        ],
    );
    let idle = ActivitySpec::with_priorities(
        Activity::Idle,
        vec![
            (
                0,
                Spec::look_at_entity_sometimes(Some(is_player), 6.0, 30, 60),
            ),
            (1, Spec::make_love(crate::mob::Kind::Axolotl, 0.2, 2)),
            (
                2,
                Spec::run_one(vec![
                    (Spec::follow_temptation(speed, 2.5, false), 1),
                    (
                        {
                            let mut s = Spec::baby_follow_adult(5, 16, 0.6);
                            s.set_speed_fn(chase_speed);
                            s
                        },
                        1,
                    ),
                ]),
            ),
            (3, Spec::start_attacking(None, find_attack_target)),
            (3, Spec::try_find_water(6, 0.15)),
            (
                4,
                Spec::gate(
                    &[(M::WalkTarget, S::ValueAbsent)],
                    &[],
                    false,
                    false,
                    vec![
                        (Spec::stroll_mode(0.5, 10, 7, bh::StrollMode::Swim), 2),
                        (
                            Spec::stroll_mode(
                                0.15,
                                10,
                                7,
                                bh::StrollMode::Land { from_water: false },
                            ),
                            2,
                        ),
                        (
                            Spec::of(bh::Behavior::WalkToLookTarget {
                                cond: Some(look_target_same_medium),
                                speed: 0.5,
                                close: 3,
                                speed_fn: Some(speed),
                            }),
                            3,
                        ),
                        (
                            Spec::of(bh::Behavior::Check(|t| t.mob_ref().fluids.in_water())),
                            5,
                        ),
                        (Spec::of(bh::Behavior::Check(|t| t.on_ground())), 5),
                    ],
                ),
            ),
        ],
    );
    let fight = ActivitySpec::with_memory(
        Activity::Fight,
        0,
        vec![
            Spec::of(bh::Behavior::StopAttackingIfTargetInvalid {
                stop_when: None,
                on_erased: Some(on_stop_attacking),
                tired: true,
            }),
            {
                let mut s = Spec::walk_to_attack_target(0.6);
                s.set_speed_fn(chase_speed);
                s
            },
            Spec::melee(20),
            Spec::erase_if(is_breeding, M::AttackTarget),
        ],
        M::AttackTarget,
    );
    let play_dead = ActivitySpec::with_priorities(
        Activity::PlayDead,
        vec![
            (0, ax(Behavior::PlayDead)),
            (1, Spec::erase_if(is_breeding, M::PlayDeadTicks)),
        ],
    )
    .requires(&[(M::PlayDeadTicks, S::ValuePresent)])
    .erases(&[M::PlayDeadTicks]);
    BrainSpec {
        sensors: vec![
            SensorKind::NearestLivingEntities,
            SensorKind::NearestAdult,
            SensorKind::HurtBy,
            super::sensor(super::Sensor::AxolotlAttackables),
            SensorKind::Tempting(None),
        ],
        activities: vec![core, idle, fight, play_dead],
        ..BrainSpec::default()
    }
}

pub fn init(m: &mut MobData) {
    m.brain = Some(Box::new(Brain::new(brain_spec(), &mut m.random)));
    m.nav.kind = NavKind::Amphibious;
    m.move_ctl.kind = MoveKind::SmoothSwimming {
        max_turn_x: 85,
        max_turn_y: 10,
        in_water_speed: 0.1,
        outside_water_speed: 0.5,
        apply_gravity: false,
    };
    m.look.kind = LookKind::SmoothSwimming { max_y_rot: 20 };
    m.air = 6000;
}

/// `AxolotlAi.findNearestValidAttackTarget`.
fn find_attack_target(t: &mut Tick<'_, '_>) -> Option<EntityId> {
    if brain_ref(t).has(M::BreedTarget) {
        return None;
    }
    brain_ref(t).entity(M::NearestAttackable)
}

/// `Axolotl.onStopAttacking`: a dead target whose last damage came from a player within 20
/// blocks of the axolotl gives that player the supporting effects.
fn on_stop_attacking(t: &mut Tick<'_, '_>, target: EntityId) {
    let game_time = t.env.game_time;
    let Some(player) = t
        .ents
        .mob(target)
        .filter(|m| m.is_dead_or_dying())
        .and_then(|m| m.last_damage_source(game_time).and(m.last_damage_cause))
        .filter(|&p| t.ents.player(p).is_some())
    else {
        return;
    };
    // `getEntitiesOfClass(Player.class, getBoundingBox().inflate(20.0))`.
    let area = t.aabb().inflate(20.0, 20.0, 20.0);
    if t.ents
        .store
        .get(player)
        .is_some_and(|p| p.aabb().intersects(&area))
    {
        apply_supporting_effects(t, player);
    }
}

/// `Axolotl.applySupportingEffects`: Regeneration (up to two minutes, from the axolotl), and
/// Mining Fatigue ends.
fn apply_supporting_effects(t: &mut Tick<'_, '_>, player: EntityId) {
    use crate::effect::{EffectInstance, MobEffect};
    let Some(p) = t.ents.player(player) else {
        return;
    };
    let regen = p.survival.effects.get(MobEffect::REGENERATION);
    if regen.is_none_or(|e| e.ends_within(2399)) {
        let previous = regen.map_or(0, |e| e.duration);
        let duration = (100 + previous).min(2400);
        let me = t.id;
        crate::effect::living::add_to_entity_from(
            t.level,
            t.ents,
            t.env,
            t.cx,
            player,
            EffectInstance::new(MobEffect::REGENERATION, duration, 0),
            Some(me),
        );
    }
    if let Some(fx) = t.ents.effects_mut(player) {
        fx.remove(MobEffect::MINING_FATIGUE);
    }
}

/// `AxolotlAttackablesSensor`.
pub fn attackables_sensor(t: &mut Tick<'_, '_>) {
    let hunting_cooldown = brain_ref(t).has(M::HasHuntingCooldown);
    let mut candidates = Vec::new();
    sensing::visible_find_all(
        t,
        |t, id| {
            bh::dist_sqr(t, id) <= 64.0
                && t.ents.mob(id).is_some_and(|m| m.fluids.in_water())
                && (brain::sensor::in_entity_tag(t, id, "minecraft:axolotl_always_hostiles")
                    || (!hunting_cooldown
                        && brain::sensor::in_entity_tag(t, id, "minecraft:axolotl_hunt_targets")))
        },
        &mut candidates,
    );
    let found = candidates
        .into_iter()
        .find(|&id| sensing::is_attackable(t, id));
    brain::brain(t).set_opt(M::NearestAttackable, found.map(MemValue::Entity));
}

/// `finalizeSpawn`: a common variant (bucket spawns keep theirs); the third of a group is a
/// baby.
pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    if ctx.reason == super::SpawnReason::Bucket {
        return;
    }
    // `AxolotlGroupData`: two common variants (`getCommonSpawnVariant`), one per member.
    let (a, b) = if let crate::mob::fam::GroupData::Axolotl(a, b) = ctx.group {
        (a, b)
    } else {
        let a = m.random.next_int_bounded(4);
        let b = m.random.next_int_bounded(4);
        ctx.group = crate::mob::fam::GroupData::Axolotl(a, b);
        (a, b)
    };
    state_mut(m).variant = if m.random.next_int_bounded(2) == 0 {
        a
    } else {
        b
    };
    if ctx.group_index >= 2 {
        m.age = -24000;
        m.baby = true;
    }
}

/// `getBreedOffspring`: rarely blue, else a parent's variant; persistent.
pub fn breed_offspring(
    parent: &MobData,
    partner: &MobData,
    child: &mut MobData,
    random: &mut vanillite_base::random::LegacyRandom,
) {
    let variant = if random.next_int_bounded(1200) == 0 {
        4
    } else if random.next_boolean() {
        state(parent).variant
    } else {
        state(partner).variant
    };
    state_mut(child).variant = variant;
    child.persistent = true;
}

/// `before_tick`: remembers the air before `baseTick` (`handleAirSupply`).
pub fn before_tick(t: &mut Tick<'_, '_>) {
    state_mut(t.mob()).pre_air = t.mob_ref().air;
}

/// `Axolotl.handleAirSupply` after `baseTick` and the playing dead flag
/// (`customServerAiStep`).
pub fn update_activity(t: &mut Tick<'_, '_>) {
    let old = brain_ref(t).active_non_core();
    if old != Some(Activity::PlayDead) {
        brain::brain(t).set_active_activity_to_first_valid(&[
            Activity::PlayDead,
            Activity::Fight,
            Activity::Idle,
        ]);
        if old == Some(Activity::Fight) && brain_ref(t).active_non_core() != Some(Activity::Fight) {
            brain::brain(t).set_with_expiry(M::HasHuntingCooldown, MemValue::Bool(true), 2400);
        }
    }
    if !t.mob_ref().no_ai {
        let playing = brain_ref(t).int(M::PlayDeadTicks).is_some_and(|v| v > 0);
        if state(t.mob_ref()).playing_dead != playing {
            state_mut(t.mob()).playing_dead = playing;
            t.core_mut().mark_data_dirty();
        }
        dry_out(t);
    }
}

/// `handleAirSupply`: out of water and rain the air runs out, then it dries out.
fn dry_out(t: &mut Tick<'_, '_>) {
    let pre = state(t.mob_ref()).pre_air;
    let p = t.pos();
    let rain = (t.env.raining_at)(BlockPos::containing(p.x, p.y, p.z));
    if t.mob_ref().is_alive() && !(t.mob_ref().fluids.in_water() || rain) {
        t.mob().air = pre - 1;
        if t.mob_ref().air <= -20 {
            t.mob().air = 0;
            let kind = t.env.types.named("minecraft:dry_out");
            let id = t.id;
            crate::mob::combat::hurt(
                t.level,
                t.ents,
                t.env,
                t.cx,
                id,
                DamageSource::of(kind),
                2.0,
            );
        }
    } else {
        t.mob().air = 6000;
    }
}

/// `hurtServer`: a third of hits in water start playing dead.
pub fn hurt_filter(h: &mut crate::mob::fam::Hurt<'_, '_>, amount: f32) -> Option<f32> {
    let source = h.source;
    let Some(m) = h.ents.mob_mut(h.target) else {
        return Some(amount);
    };
    let health = m.health;
    let max = m.max_health();
    // The checks in Vanilla's order (each random roll only when reached).
    if m.no_ai || m.random.next_int_bounded(3) != 0 {
        return Some(amount);
    }
    #[allow(clippy::cast_precision_loss)]
    let roll = m.random.next_int_bounded(3) as f32;
    if (roll < amount || health / max < 0.5)
        && amount < health
        && m.fluids.in_water()
        && (source.cause.is_some() || source.direct.is_some())
        && !state(m).playing_dead
        && let Some(b) = m.brain.as_deref_mut()
    {
        b.set(M::PlayDeadTicks, MemValue::Int(200));
    }
    Some(amount)
}

pub fn after_hurt(_t: &mut Tick<'_, '_>, _cause: Option<EntityId>, _amount: f32) {}

/// `playAttackSound`.
pub fn after_attack(t: &mut Tick<'_, '_>, _target: EntityId) {
    super::sound_at(
        t,
        "minecraft:entity.axolotl.attack",
        crate::mob::kinds::Kind::Axolotl.sound_source(),
        1.0,
        1.0,
    );
}

/// The ambient sound in or out of water (none while playing dead).
pub fn ambient_sound(m: &MobData) -> &'static str {
    if m.fluids.in_water() {
        "minecraft:entity.axolotl.idle_water"
    } else {
        "minecraft:entity.axolotl.idle_air"
    }
}

/// `Bucketable.bucketMobPickup`, then feeding (axolotls and tadpoles).
pub fn bucket_or_feed(ix: &mut Interact<'_, '_>) -> Interaction {
    let Some(kind) = ix.ents.mob(ix.target).map(|m| m.kind) else {
        return Interaction::Pass;
    };
    if kind == crate::mob::Kind::Tadpole && super::item_in_tag(ix.stack.item, "minecraft:frog_food")
    {
        let fed = super::frog::feed_tadpole(ix);
        if fed != Interaction::Pass {
            return fed;
        }
    }
    // `Tadpole.mobInteract`: the golden dandelion (`setAge(0)`).
    if kind == crate::mob::Kind::Tadpole
        && ix
            .ents
            .mob(ix.target)
            .is_some_and(|m| crate::mob::age_lock::can_use(m, ix.stack, true))
    {
        crate::mob::age_lock::toggle(ix.ents, ix.env, ix.player, ix.target, ix.hand, |m| {
            super::state_mut(m).tadpole_age = 0;
        });
        return Interaction::Success;
    }
    let name = crate::item::item_id(ix.stack).name();
    let alive = ix.ents.mob(ix.target).is_some_and(MobData::is_alive);
    if name == "minecraft:water_bucket" && alive {
        let (bucket, sound) = if kind == crate::mob::Kind::Axolotl {
            (
                "minecraft:axolotl_bucket",
                "minecraft:item.bucket.fill_axolotl",
            )
        } else {
            (
                "minecraft:tadpole_bucket",
                "minecraft:item.bucket.fill_tadpole",
            )
        };
        if let Some(pos) = ix.ents.store.get(ix.target).map(|e| e.pos()) {
            let seed = crate::survival::sound_seed();
            crate::survival::play_sound(
                ix.env.out,
                None,
                pos,
                sound,
                vanillite_protocol::packets::play::clientbound::SoundSource::Neutral,
                1.0,
                1.0,
                seed,
            );
        }
        let flags = ix
            .ents
            .store
            .get(ix.target)
            .map(|e| e.flags())
            .unwrap_or_default();
        let Some(stack) = ix
            .ents
            .mob(ix.target)
            .map(|m| bucket_stack(m, flags, bucket))
        else {
            return Interaction::Pass;
        };
        crate::mob::animal::filled_bucket(ix.ents, ix.player, &stack);
        crate::mob::animal::fill_held(ix.ents, ix.env, ix.player, ix.hand, stack);
        if let Some(m) = ix.ents.mob_mut(ix.target) {
            m.removed = true;
        }
        ix.cx.discarded.push(ix.target);
        return Interaction::Success;
    }
    if kind == crate::mob::Kind::Axolotl {
        return crate::mob::animal::animal_interact_pub(ix);
    }
    Interaction::Pass
}

/// `saveToBucketTag` of axolotls (with `axolotl/variant` and the hunting cooldown) and
/// tadpoles: the default bucket data, their age and age lock.
fn bucket_stack(
    m: &MobData,
    flags: vanillite_world::entity::EntityFlags,
    item: &str,
) -> crate::item::ItemStack {
    let (mut stack, mut data) = crate::mob::fam::bucket_default(m, flags, item);
    let s = state(m);
    if m.kind == crate::mob::Kind::Axolotl {
        if let Some(id) =
            vanillite_data::registry::DATA_COMPONENT_TYPE.id("minecraft:axolotl/variant")
        {
            let mut v = Vec::new();
            vanillite_protocol::codec::write_varint(&mut v, s.variant);
            stack.set_component(id, v.into());
        }
        data.insert("Age", m.age);
        data.insert("AgeLocked", m.age_locked);
        if let Some(b) = m.brain.as_deref()
            && b.has(M::HasHuntingCooldown)
        {
            data.insert("HuntingCooldown", b.ttl(M::HasHuntingCooldown));
        }
    } else {
        data.insert("Age", s.tadpole_age);
        data.insert("AgeLocked", m.age_locked);
    }
    crate::mob::fam::set_bucket_data(&mut stack, data);
    stack
}

/// Synched data: baby (16), age locked (17), variant (18), playing dead (19), from bucket (20).
pub fn encode(w: &mut DataWriter<'_>, m: &MobData) {
    let s = state(m);
    crate::mob::fam::companion::encode_ageable(w, m);
    w.int(18, s.variant, 0);
    w.boolean(19, s.playing_dead, false);
    w.boolean(20, s.from_bucket, false);
}

pub fn save(m: &MobData, out: &mut Compound) {
    let s = state(m);
    out.insert("Variant", s.variant);
    out.insert("FromBucket", s.from_bucket);
}

pub fn load(m: &mut MobData, tag: &Compound) {
    let s = state_mut(m);
    s.variant = nbt_int(tag, "Variant", 0).clamp(0, 4);
    s.from_bucket = nbt_bool(tag, "FromBucket", false);
}

// --- Behaviours -----------------------------------------------------------------------------

pub fn duration(b: &Behavior) -> (i32, i32) {
    match b {
        Behavior::PlayDead => (200, 200),
        Behavior::ValidatePlayDead => (60, 60),
    }
}

pub fn shape(b: &Behavior) -> Shape {
    match b {
        Behavior::PlayDead => Shape::Timed,
        Behavior::ValidatePlayDead => Shape::OneShot,
    }
}

pub fn requirements(b: &Behavior) -> &'static [(M, MemoryStatus)] {
    match b {
        Behavior::PlayDead => &[
            (M::PlayDeadTicks, S::ValuePresent),
            (M::HurtByEntity, S::ValuePresent),
        ],
        Behavior::ValidatePlayDead => &[
            (M::PlayDeadTicks, S::ValuePresent),
            (M::HurtByEntity, S::Registered),
        ],
    }
}

pub fn no_timeout(_b: &Behavior) -> bool {
    false
}

pub fn check(b: &mut Behavior, t: &mut Tick<'_, '_>) -> bool {
    match b {
        Behavior::PlayDead => in_water(t),
        Behavior::ValidatePlayDead => true,
    }
}

pub fn can_still_use(b: &mut Behavior, t: &mut Tick<'_, '_>, _now: i64) -> bool {
    match b {
        Behavior::PlayDead => in_water(t) && brain_ref(t).has(M::PlayDeadTicks),
        Behavior::ValidatePlayDead => false,
    }
}

pub fn start(b: &mut Behavior, t: &mut Tick<'_, '_>, _now: i64) {
    if let Behavior::PlayDead = b {
        let br = brain::brain(t);
        br.erase(M::WalkTarget);
        br.erase(M::LookTarget);
        crate::effect::living::add_to_mob(
            t.mob(),
            crate::effect::EffectInstance::new(crate::effect::MobEffect::REGENERATION, 200, 0),
        );
    }
}

pub fn tick(_b: &mut Behavior, _t: &mut Tick<'_, '_>, _now: i64) {}

pub fn stop(_b: &mut Behavior, _t: &mut Tick<'_, '_>, _now: i64) {}

pub fn trigger(b: &mut Behavior, t: &mut Tick<'_, '_>, _now: i64) -> bool {
    match b {
        Behavior::ValidatePlayDead => {
            let Some(ticks) = brain_ref(t).int(M::PlayDeadTicks) else {
                return false;
            };
            let br = brain::brain(t);
            if ticks <= 0 {
                br.erase(M::PlayDeadTicks);
                br.erase(M::HurtByEntity);
                br.use_default_activity();
            } else {
                br.set(M::PlayDeadTicks, MemValue::Int(ticks - 1));
            }
            true
        }
        Behavior::PlayDead => false,
    }
}
