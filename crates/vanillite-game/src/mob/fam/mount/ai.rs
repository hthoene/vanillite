//! The brains of the family (`CamelAi`, `HappyGhastAi`, `NautilusAi`, `ZombieNautilusAi`,
//! `AllayAi`, `CopperGolemAi`, `BreezeAi`, `CreakingAi`) and the family's own behaviours
//! (`CamelAi.RandomSitting`, `GoAndGiveItemsToTarget`, `StayCloseToTarget`, `ChargeAttack`,
//! the breeze's `Shoot`, `LongJump`, `ShootWhenStuck`, `Slide`) and sensor
//! (`BreezeAttackEntitySensor`).

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::Compound;
use vanillite_world::entity::{EntityId, Pose};

use super::{Behavior, Sensor};
use crate::mob::brain::behavior::{
    self as b, Behavior as B, FamBehavior, PanicMode, Shape, Spec, StrollMode,
};
use crate::mob::brain::sensor::{FamSensor, SensorKind};
use crate::mob::brain::{
    self, Activity, ActivitySpec, Brain, BrainSpec, MemValue, MemoryStatus as S, MemoryType as M,
    Tracker, WalkTarget, brain_ref, sensing,
};
use crate::mob::{Kind, MobData, Tick};
use crate::survival::damage::{DamageSource, tag};

// --- Brain setup --------------------------------------------------------------------------

fn fam(b: Behavior) -> Spec {
    Spec::fam(FamBehavior::Mount(b))
}

fn install(m: &mut MobData, spec: BrainSpec) {
    let brain = Brain::new(spec, &mut m.random);
    m.brain = Some(Box::new(brain));
}

/// A player (`SetEntityLookTargetSometimes.create(EntityType.PLAYER, ...)`).
fn is_player(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.player(id).is_some()
}

/// `AnimalPanic(speed, flyHeight)`: flees through the air.
fn fly_panic(speed: f32, fly_height: i32) -> Spec {
    Spec::timed(
        B::AnimalPanic {
            speed,
            mode: PanicMode::Fly(fly_height),
            causes: tag::PANIC_CAUSES,
            on_start: None,
        },
        100,
        120,
    )
}

/// `BabyFollowAdult.create(range, speed, memory, targetEyeHeight)`.
fn follow_adult(min: i32, max: i32, speed: f32, memory: M, target_eye: bool) -> Spec {
    Spec::of(B::BabyFollowAdult {
        min,
        max,
        speed,
        memory,
        target_eye,
        speed_fn: None,
    })
}

/// Whether a camel may walk (`not(Camel::refuseToMove)`).
fn camel_moves(t: &mut Tick<'_, '_>) -> bool {
    let now = t.game_time();
    !super::camel::refuse_to_move(t.mob_ref(), now)
}

/// `Camel` brain: `CamelAi`.
pub fn camel_brain(m: &mut MobData) {
    let core = ActivitySpec::new(
        Activity::Core,
        0,
        vec![
            Spec::swim(0.8),
            // `CamelPanic`: the standing up and the mob controlled check are in the camel tick.
            Spec::animal_panic(4.0),
            Spec::look_at_target_sink(45, 90),
            Spec::move_to_target_sink(),
            Spec::count_down(M::TemptationCooldownTicks),
            Spec::count_down(M::GazeCooldownTicks),
        ],
    );
    let partner = m.kind;
    let idle = ActivitySpec::with_priorities(
        Activity::Idle,
        vec![
            (
                0,
                Spec::look_at_entity_sometimes(Some(is_player), 6.0, 30, 60),
            ),
            (1, Spec::make_love(partner, 1.0, 2)),
            (
                2,
                Spec::run_one(vec![
                    (Spec::follow_temptation(|_| 2.5, 3.5, false), 1),
                    (
                        Spec::trigger_if(
                            camel_moves,
                            follow_adult(5, 16, 2.5, M::NearestVisibleAdult, false),
                        ),
                        1,
                    ),
                ]),
            ),
            (3, Spec::random_look_around(150, 250, 30.0, 0.0, 0.0)),
            (
                4,
                Spec::run_one_if(
                    &[(M::WalkTarget, S::ValueAbsent)],
                    vec![
                        (Spec::trigger_if(camel_moves, Spec::stroll(2.0)), 1),
                        (
                            Spec::trigger_if(camel_moves, Spec::walk_to_look_target(2.0, 3)),
                            1,
                        ),
                        (
                            fam(Behavior::CamelSitting {
                                min_pose_ticks: 400,
                            }),
                            1,
                        ),
                        (Spec::do_nothing(30, 60), 1),
                    ],
                ),
            ),
        ],
    );
    install(
        m,
        BrainSpec {
            sensors: vec![
                SensorKind::NearestLivingEntities,
                SensorKind::HurtBy,
                SensorKind::Tempting(None),
                SensorKind::NearestAdult,
            ],
            activities: vec![core, idle],
            ..BrainSpec::default()
        },
    );
}

/// The ghastling brain: `HappyGhastAi`.
pub fn happy_ghast_brain(m: &mut MobData) {
    let core = ActivitySpec::new(
        Activity::Core,
        0,
        vec![
            Spec::swim(0.8),
            fly_panic(2.0, 0),
            Spec::look_at_target_sink(45, 90),
            Spec::move_to_target_sink(),
            Spec::count_down(M::TemptationCooldownTicks),
        ],
    );
    let idle = ActivitySpec::with_priorities(
        Activity::Idle,
        vec![
            (1, Spec::follow_temptation(|_| 1.25, 3.0, true)),
            (2, follow_adult(3, 16, 1.1, M::NearestVisiblePlayer, true)),
            (3, follow_adult(3, 16, 1.1, M::NearestVisibleAdult, true)),
            (
                4,
                Spec::run_one(vec![
                    (Spec::stroll_mode(1.0, 10, 7, StrollMode::Fly), 1),
                    (Spec::walk_to_look_target(1.0, 3), 1),
                ]),
            ),
        ],
    );
    let panic = ActivitySpec::with_priorities(Activity::Panic, Vec::new())
        .requires(&[(M::IsPanicking, S::ValuePresent)]);
    install(
        m,
        BrainSpec {
            sensors: vec![
                SensorKind::NearestLivingEntities,
                SensorKind::HurtBy,
                SensorKind::Tempting(None),
                SensorKind::NearestAdultAnyType,
                SensorKind::NearestPlayers,
            ],
            activities: vec![core, idle, panic],
            ..BrainSpec::default()
        },
    );
}

/// The nautilus brains: `NautilusAi` and `ZombieNautilusAi`.
pub fn nautilus_brain(m: &mut MobData) {
    let zombie = m.kind == Kind::ZombieNautilus;
    let mut core = vec![
        Spec::look_at_target_sink(45, 90),
        Spec::move_to_target_sink(),
        Spec::count_down(M::TemptationCooldownTicks),
        Spec::count_down(M::ChargeCooldownTicks),
        Spec::count_down(M::AttackTargetCooldown),
    ];
    if !zombie {
        core.insert(0, Spec::animal_panic(1.6));
    }
    let wander = || {
        Spec::gate(
            &[(M::WalkTarget, S::ValueAbsent)],
            &[],
            false,
            false,
            vec![
                (Spec::stroll_mode(1.0, 10, 7, StrollMode::Swim), 2),
                (Spec::walk_to_look_target(1.0, 3), 3),
            ],
        )
    };
    let idle = if zombie {
        vec![
            (1, Spec::follow_temptation(|_| 0.9, 3.5, false)),
            (2, Spec::start_attacking(None, nautilus_target)),
            (3, wander()),
        ]
    } else {
        vec![
            (1, Spec::make_love(Kind::Nautilus, 0.4, 2)),
            (2, Spec::follow_temptation(|_| 1.3, 3.5, false)),
            (3, Spec::start_attacking(None, nautilus_target)),
            (4, wander()),
        ]
    };
    let fight = ActivitySpec::with_priorities(
        Activity::Fight,
        vec![(
            0,
            fam(Behavior::Charge {
                speed: if zombie { 0.5 } else { 0.6 },
                zombie,
                start: Vec3::ZERO,
                velocity: Vec3::ZERO,
            }),
        )],
    )
    .requires(&[
        (M::AttackTarget, S::ValuePresent),
        (M::TemptingPlayer, S::ValueAbsent),
        (M::BreedTarget, S::ValueAbsent),
        (M::ChargeCooldownTicks, S::ValueAbsent),
    ]);
    install(
        m,
        BrainSpec {
            memories: vec![M::AngryAt, M::AttackTargetCooldown],
            sensors: vec![
                SensorKind::NearestLivingEntities,
                SensorKind::NearestAdult,
                SensorKind::NearestPlayers,
                SensorKind::HurtBy,
                SensorKind::Tempting(Some("minecraft:nautilus_food")),
            ],
            activities: vec![
                ActivitySpec::new(Activity::Core, 0, core),
                ActivitySpec::with_priorities(Activity::Idle, idle),
                fight,
            ],
            ..BrainSpec::default()
        },
    );
}

/// `NautilusAi.initMemories`: the first non-player attack waits 2400 to 3600 ticks.
pub fn nautilus_init_memories(m: &mut MobData) {
    let ticks = m.random.next_int_between_inclusive(2400, 3600);
    if let Some(b) = m.brain.as_deref_mut() {
        b.set(M::AttackTargetCooldown, MemValue::Int(ticks));
    }
}

/// `NautilusAi.setAngerTarget`: angry at the attacker for 400 ticks.
pub fn nautilus_set_anger(m: &mut MobData, uuid: u128, _now: i64) {
    if let Some(b) = m.brain.as_deref_mut() {
        b.erase(M::CantReachWalkTargetSince);
        b.set_with_expiry(M::AngryAt, MemValue::Uuid(uuid), 400);
    }
}

/// `NautilusAi.findNearestValidAttackTarget`.
fn nautilus_target(t: &mut Tick<'_, '_>) -> Option<EntityId> {
    let m = t.mob_ref();
    let breeding = brain_ref(t).has(M::BreedTarget);
    if breeding || !m.fluids.in_water() || m.baby || m.tame {
        return None;
    }
    if let Some(uuid) = brain_ref(t).uuid(M::AngryAt)
        && let Some(id) = b::living_from_uuid(t, uuid)
        && in_water(t, id)
        && sensing::is_attackable_ignoring_los(t, id)
    {
        return Some(id);
    }
    if brain_ref(t).has(M::AttackTargetCooldown) {
        return None;
    }
    let ticks = t.env.random.next_int_between_inclusive(2400, 3600);
    brain::brain(t).set(M::AttackTargetCooldown, MemValue::Int(ticks));
    if t.env.random.next_float() < 0.5 {
        return None;
    }
    sensing::visible_find_closest(t, |t, id| {
        in_water(t, id)
            && crate::mob::brain::sensor::in_entity_tag(t, id, "minecraft:nautilus_hostiles")
    })
}

fn in_water(t: &Tick<'_, '_>, id: EntityId) -> bool {
    if let Some(m) = t.ents.mob(id) {
        return m.fluids.in_water();
    }
    t.ents.store.get(id).is_some_and(|e| {
        let p = e.pos();
        crate::mob::is_water(t.level.block(BlockPos::containing(p.x, p.y, p.z)))
    })
}

/// The allay brain: `AllayAi`.
pub fn allay_brain(m: &mut MobData) {
    let core = ActivitySpec::new(
        Activity::Core,
        0,
        vec![
            Spec::swim(0.8),
            Spec::animal_panic(2.5),
            Spec::look_at_target_sink(45, 90),
            Spec::move_to_target_sink(),
            Spec::count_down(M::LikedNoteblockCooldownTicks),
            Spec::count_down(M::ItemPickupCooldownTicks),
        ],
    );
    let idle = ActivitySpec::new(
        Activity::Idle,
        0,
        vec![
            Spec::go_to_wanted_item(None, 1.75, true, 32),
            fam(Behavior::GiveItems),
            fam(Behavior::StayClose),
            Spec::look_at_entity_sometimes(None, 6.0, 30, 60),
            Spec::run_one(vec![
                (Spec::stroll_mode(1.0, 10, 7, StrollMode::Fly), 2),
                (Spec::walk_to_look_target(1.0, 3), 2),
                (Spec::do_nothing(30, 60), 1),
            ]),
        ],
    );
    install(
        m,
        BrainSpec {
            memories: vec![
                M::LikedPlayer,
                M::LikedNoteblockPosition,
                M::LikedNoteblockCooldownTicks,
            ],
            sensors: vec![
                SensorKind::NearestLivingEntities,
                SensorKind::NearestPlayers,
                SensorKind::HurtBy,
                SensorKind::NearestItems,
            ],
            activities: vec![core, idle],
            ..BrainSpec::default()
        },
    );
}

/// The allay's liked player (`LIKED_PLAYER`).
pub fn liked_player(m: &MobData) -> Option<u128> {
    m.brain.as_deref().and_then(|b| b.uuid(M::LikedPlayer))
}

/// Sets or forgets the allay's liked player.
pub fn set_liked_player(m: &mut MobData, uuid: Option<u128>) {
    if let Some(b) = m.brain.as_deref_mut() {
        b.set_opt(M::LikedPlayer, uuid.map(MemValue::Uuid));
    }
}

/// The allay's pickup cooldown (`ITEM_PICKUP_COOLDOWN_TICKS` present).
pub fn on_pickup_cooldown(m: &MobData) -> bool {
    m.brain
        .as_deref()
        .is_some_and(|b| b.has(M::ItemPickupCooldownTicks))
}

/// `AllayAi.hearNoteblock` (from the allay's vibration listener).
pub fn hear_noteblock(m: &mut MobData, pos: BlockPos, dimension: &str) {
    let Some(b) = m.brain.as_deref_mut() else {
        return;
    };
    let current = b.pos(M::LikedNoteblockPosition);
    if current.is_none() {
        b.set(
            M::LikedNoteblockPosition,
            MemValue::GlobalPos(crate::mob::brain::GlobalPos {
                dimension: dimension.into(),
                pos,
            }),
        );
        b.set(M::LikedNoteblockCooldownTicks, MemValue::Int(600));
    } else if current == Some(pos) {
        b.set(M::LikedNoteblockCooldownTicks, MemValue::Int(600));
    }
}

/// The copper golem brain: `CopperGolemAi`.
pub fn copper_golem_brain(m: &mut MobData) {
    let core = ActivitySpec::new(
        Activity::Core,
        0,
        vec![
            Spec::animal_panic(1.5),
            Spec::look_at_target_sink(45, 90),
            Spec::move_to_target_sink(),
            Spec::of(crate::mob::brain::behavior::Behavior::InteractWithDoor {
                last_node: None,
                cooldown: 0,
            }),
            Spec::count_down(M::GazeCooldownTicks),
            Spec::count_down(M::TransportItemsCooldownTicks),
        ],
    );
    let idle = ActivitySpec::with_priorities(
        Activity::Idle,
        vec![
            (
                0,
                fam(Behavior::GolemTransport(
                    super::transport::Transport::default(),
                )),
            ),
            (
                1,
                Spec::look_at_entity_sometimes(Some(is_player), 6.0, 40, 80),
            ),
            (
                2,
                Spec::run_one_if(
                    &[
                        (M::WalkTarget, S::ValueAbsent),
                        (M::TransportItemsCooldownTicks, S::ValuePresent),
                    ],
                    vec![
                        (
                            Spec::stroll_mode(1.0, 2, 2, StrollMode::Land { from_water: true }),
                            1,
                        ),
                        (Spec::do_nothing(30, 60), 1),
                    ],
                ),
            ),
        ],
    );
    install(
        m,
        BrainSpec {
            sensors: vec![SensorKind::NearestLivingEntities, SensorKind::HurtBy],
            activities: vec![core, idle],
            ..BrainSpec::default()
        },
    );
}

/// The breeze brain: `BreezeAi` (default activity `FIGHT`).
pub fn breeze_brain(m: &mut MobData) {
    let core = ActivitySpec::new(
        Activity::Core,
        0,
        vec![Spec::swim(0.8), Spec::look_at_target_sink(45, 90)],
    );
    let idle = ActivitySpec::with_priorities(
        Activity::Idle,
        vec![
            (
                0,
                Spec::start_attacking(None, |t| brain_ref(t).entity(M::NearestAttackable)),
            ),
            (1, Spec::start_attacking(None, hurt_by_living)),
            // `SlideToTargetSink(20, 40)`: the sliding pose is set in the breeze tick.
            (2, Spec::move_to_target_sink_for(20, 40)),
            (
                3,
                Spec::run_one(vec![(Spec::do_nothing(20, 100), 1), (Spec::stroll(0.6), 2)]),
            ),
        ],
    );
    let fight = ActivitySpec::with_priorities(
        Activity::Fight,
        vec![
            (
                0,
                Spec::of(B::StopAttackingIfTargetInvalid {
                    stop_when: Some(|t, id| !super::breeze::was_attackable_last_100(t, id)),
                    on_erased: None,
                    tired: true,
                }),
            ),
            (1, fam(Behavior::BreezeShoot)),
            (2, fam(Behavior::BreezeLongJump)),
            (3, fam(Behavior::BreezeShootWhenStuck)),
            (4, fam(Behavior::BreezeSlide)),
        ],
    )
    .requires(&[
        (M::AttackTarget, S::ValuePresent),
        (M::WalkTarget, S::ValueAbsent),
    ]);
    install(
        m,
        BrainSpec {
            sensors: vec![
                SensorKind::NearestLivingEntities,
                SensorKind::HurtBy,
                SensorKind::NearestPlayers,
                SensorKind::Fam(FamSensor::Mount(Sensor::BreezeAttackEntity)),
            ],
            activities: vec![core, idle, fight],
            default_activity: Some(Activity::Fight),
            ..BrainSpec::default()
        },
    );
}

/// `HURT_BY`'s living cause.
fn hurt_by_living(t: &mut Tick<'_, '_>) -> Option<EntityId> {
    match brain_ref(t).get(M::HurtBy)? {
        MemValue::Damage(_, Some(e)) => {
            let e = *e;
            (t.ents.mob(e).is_some() || t.ents.player(e).is_some()).then_some(e)
        }
        _ => None,
    }
}

/// The creaking brain: `CreakingAi`.
pub fn creaking_brain(m: &mut MobData) {
    let core = ActivitySpec::new(
        Activity::Core,
        0,
        vec![
            Spec::swim(0.8),
            Spec::look_at_target_sink(45, 90),
            Spec::move_to_target_sink(),
        ],
    );
    let idle = ActivitySpec::new(
        Activity::Idle,
        10,
        vec![
            Spec::start_attacking(
                Some(|t| super::creaking::creaking(t.mob_ref()).active),
                |t| brain_ref(t).entity(M::NearestVisibleAttackablePlayer),
            ),
            Spec::look_at_entity_sometimes(None, 8.0, 30, 60),
            Spec::run_one(vec![
                (Spec::stroll(0.3), 2),
                (Spec::walk_to_look_target(0.3, 3), 2),
                (Spec::do_nothing(30, 60), 1),
            ]),
        ],
    );
    let fight = ActivitySpec::new(
        Activity::Fight,
        10,
        vec![
            Spec::walk_to_attack_target(1.0),
            Spec::of(B::MeleeAttack {
                cond: Some(|t| super::creaking::can_move(t.mob_ref())),
                cooldown: 40,
            }),
            Spec::of(B::StopAttackingIfTargetInvalid {
                stop_when: Some(|t, id| {
                    !brain_ref(t)
                        .entities(M::NearestVisibleAttackablePlayers)
                        .contains(&id)
                }),
                on_erased: None,
                tired: true,
            }),
        ],
    )
    .requires(&[(M::AttackTarget, S::ValuePresent)]);
    install(
        m,
        BrainSpec {
            sensors: vec![
                SensorKind::NearestLivingEntities,
                SensorKind::NearestPlayers,
            ],
            activities: vec![core, idle, fight],
            ..BrainSpec::default()
        },
    );
}

/// The creaking's view of the nearest players (`NEAREST_PLAYERS`).
pub fn nearest_players(t: &Tick<'_, '_>) -> Vec<EntityId> {
    brain_ref(t).entities(M::NearestPlayers).to_vec()
}

/// Sets or erases `ATTACK_TARGET`.
pub fn set_attack_target(m: &mut MobData, target: Option<EntityId>) {
    if let Some(b) = m.brain.as_deref_mut() {
        b.set_opt(M::AttackTarget, target.map(MemValue::Entity));
    }
}

// --- Ticking ------------------------------------------------------------------------------

/// `brain.tick` and the kind's `updateActivity` (the brains of ghastlings only).
pub fn tick(t: &mut Tick<'_, '_>) {
    if t.mob_ref().brain.is_none() {
        return;
    }
    brain::tick(t);
    if !t.exists() || t.mob_ref().removed {
        return;
    }
    let kind = t.mob_ref().kind;
    let b = brain::brain(t);
    match kind {
        Kind::Camel | Kind::CamelHusk | Kind::Allay | Kind::CopperGolem => {
            b.set_active_activity_to_first_valid(&[Activity::Idle]);
        }
        Kind::HappyGhast => {
            b.set_active_activity_to_first_valid(&[Activity::Panic, Activity::Idle])
        }
        Kind::Nautilus | Kind::ZombieNautilus | Kind::Breeze => {
            b.set_active_activity_to_first_valid(&[Activity::Fight, Activity::Idle]);
        }
        Kind::Creaking => {
            if super::creaking::can_move(t.mob_ref()) {
                brain::brain(t)
                    .set_active_activity_to_first_valid(&[Activity::Fight, Activity::Idle]);
            } else {
                brain::brain(t).use_default_activity();
            }
        }
        _ => {}
    }
    // The mob's target follows `ATTACK_TARGET` (`getTargetFromBrain`).
    let target = brain_ref(t).entity(M::AttackTarget);
    if crate::mob::brain::target_changed(t, target) {
        t.mob().target = target;
    }
    match kind {
        Kind::Camel | Kind::CamelHusk => {
            // `CamelPanic.start`: a panicking camel stands up at once.
            let panicking = brain_ref(t).has(M::IsPanicking);
            if panicking && super::camel::is_sitting(t.mob_ref()) {
                let id = t.id;
                super::camel::stand_up_instantly(t.ents, t.env, id);
            }
        }
        Kind::Breeze => breeze_slide_pose(t),
        _ => {}
    }
}

/// `SlideToTargetSink.start`/`stop`: slides while walking to a walk target.
fn breeze_slide_pose(t: &mut Tick<'_, '_>) {
    let pose = t.core().pose();
    let walking = brain_ref(t).has(M::WalkTarget) && !t.nav_ref().is_done();
    if walking && pose == Pose::Standing && brain_ref(t).is_active(Activity::Idle) {
        crate::mob::kinds::play_sound(t, "minecraft:entity.breeze.slide", 1.0, 1.0);
        set_pose(t, Pose::Sliding);
    } else if !walking && pose == Pose::Sliding {
        set_pose(t, Pose::Standing);
        if brain_ref(t).has(M::AttackTarget) {
            brain::brain(t).set_with_expiry(M::BreezeShoot, MemValue::Unit, 60);
        }
    }
}

fn set_pose(t: &mut Tick<'_, '_>, pose: Pose) {
    let mut e = t.core_mut();
    e.set_pose(pose);
    e.mark_data_dirty();
}

/// Writes the brain memories (`Brain` in the entity NBT).
pub fn save_brain(m: &MobData, out: &mut Compound) {
    if let Some(b) = m.brain.as_deref() {
        brain::save(b, out);
    }
}

/// Reads the brain memories.
pub fn load_brain(m: &mut MobData, tag: &Compound) {
    if let Some(b) = m.brain.as_deref_mut() {
        brain::load(b, tag);
    }
}

// --- The family's behaviours ----------------------------------------------------------------

pub fn behavior_duration(b: &Behavior) -> (i32, i32) {
    match b {
        Behavior::GiveItems => (20, 20),
        Behavior::BreezeShoot => (20, 20),
        Behavior::BreezeLongJump => (200, 200),
        _ => (60, 60),
    }
}

pub fn behavior_shape(b: &Behavior) -> Shape {
    match b {
        Behavior::StayClose | Behavior::Idle => Shape::OneShot,
        _ => Shape::Timed,
    }
}

pub fn behavior_requirements(b: &Behavior) -> &'static [(M, S)] {
    match b {
        Behavior::GiveItems => &[
            (M::LookTarget, S::Registered),
            (M::WalkTarget, S::Registered),
            (M::ItemPickupCooldownTicks, S::Registered),
        ],
        Behavior::StayClose => &[
            (M::LookTarget, S::Registered),
            (M::WalkTarget, S::Registered),
        ],
        Behavior::Charge { .. } => &[
            (M::ChargeCooldownTicks, S::ValueAbsent),
            (M::AttackTarget, S::ValuePresent),
        ],
        Behavior::BreezeShoot => &[
            (M::AttackTarget, S::ValuePresent),
            (M::BreezeShootCooldown, S::ValueAbsent),
            (M::BreezeShootCharging, S::ValueAbsent),
            (M::BreezeShootRecovering, S::ValueAbsent),
            (M::BreezeShoot, S::ValuePresent),
            (M::WalkTarget, S::ValueAbsent),
            (M::BreezeJumpTarget, S::ValueAbsent),
        ],
        Behavior::BreezeLongJump => &[
            (M::AttackTarget, S::ValuePresent),
            (M::BreezeJumpCooldown, S::ValueAbsent),
            (M::BreezeJumpInhaling, S::Registered),
            (M::BreezeJumpTarget, S::Registered),
            (M::BreezeShoot, S::ValueAbsent),
            (M::WalkTarget, S::ValueAbsent),
            (M::BreezeLeavingWater, S::Registered),
        ],
        Behavior::BreezeShootWhenStuck => &[
            (M::AttackTarget, S::ValuePresent),
            (M::BreezeJumpInhaling, S::ValueAbsent),
            (M::BreezeJumpTarget, S::ValueAbsent),
            (M::WalkTarget, S::ValueAbsent),
            (M::BreezeShoot, S::ValueAbsent),
        ],
        Behavior::BreezeSlide => &[
            (M::AttackTarget, S::ValuePresent),
            (M::WalkTarget, S::ValueAbsent),
            (M::BreezeJumpCooldown, S::ValueAbsent),
            (M::BreezeShoot, S::ValueAbsent),
        ],
        Behavior::GolemTransport(_) => &[
            (M::VisitedBlockPositions, S::Registered),
            (M::UnreachableTransportBlockPositions, S::Registered),
            (M::TransportItemsCooldownTicks, S::ValueAbsent),
            (M::IsPanicking, S::ValueAbsent),
        ],
        Behavior::CamelSitting { .. } | Behavior::Idle => &[],
    }
}

pub fn behavior_no_timeout(b: &Behavior) -> bool {
    matches!(b, Behavior::GolemTransport(_))
}

/// `checkExtraStartConditions`.
pub fn behavior_check(t: &mut Tick<'_, '_>, b: &mut Behavior) -> bool {
    match b {
        Behavior::CamelSitting { min_pose_ticks } => {
            let now = t.game_time();
            let m = t.mob_ref();
            !m.fluids.in_water()
                && super::camel::pose_time(m, now) >= i64::from(*min_pose_ticks)
                && !t.ents.leashes.contains_key(&t.id)
                && t.on_ground()
                && t.ents.riding.passengers_of(t.id).is_empty()
                && super::camel::can_change_pose(t)
        }
        Behavior::GiveItems => can_throw_item(t),
        Behavior::GolemTransport(_) => super::transport::check(t),
        Behavior::Charge { .. } => brain_ref(t).has(M::AttackTarget),
        Behavior::BreezeShoot => {
            if t.core().pose() != Pose::Standing {
                return false;
            }
            let Some(target) = brain_ref(t).entity(M::AttackTarget) else {
                return false;
            };
            let within =
                b::entity_pos(t, target).is_some_and(|p| t.pos().distance_to_sqr(p) < 256.0);
            if !within {
                brain::brain(t).erase(M::BreezeShoot);
            }
            within
        }
        Behavior::BreezeLongJump => long_jump_can_run(t),
        Behavior::BreezeShootWhenStuck => {
            t.ents.vehicle_of(t.id).is_some()
                || t.mob_ref().fluids.in_water()
                || t.mob_ref()
                    .effects
                    .has(crate::effect::MobEffect::LEVITATION)
        }
        Behavior::BreezeSlide => {
            t.on_ground() && !t.mob_ref().fluids.in_water() && t.core().pose() == Pose::Standing
        }
        _ => true,
    }
}

/// `canStillUse`.
pub fn behavior_can_still_use(t: &mut Tick<'_, '_>, b: &mut Behavior, _now: i64) -> bool {
    match b {
        Behavior::GiveItems => can_throw_item(t),
        Behavior::GolemTransport(_) => super::transport::can_still_use(t),
        Behavior::Charge { start, .. } => charge_can_still_use(t, *start),
        Behavior::BreezeShoot => {
            brain_ref(t).has(M::AttackTarget) && brain_ref(t).has(M::BreezeShoot)
        }
        Behavior::BreezeLongJump => {
            t.core().pose() != Pose::Standing && !brain_ref(t).has(M::BreezeJumpCooldown)
        }
        _ => false,
    }
}

/// `start`.
pub fn behavior_start(t: &mut Tick<'_, '_>, b: &mut Behavior, now: i64) {
    match b {
        Behavior::CamelSitting { .. } => {
            if super::camel::is_sitting(t.mob_ref()) {
                super::camel::stand_up(t);
            } else if !brain_ref(t).has(M::IsPanicking) {
                super::camel::sit_down(t);
            }
        }
        Behavior::GiveItems => {
            if let Some(tracker) = deposit_target(t) {
                b::set_walk_and_look(t, tracker, 2.25, 3);
            }
        }
        Behavior::Charge {
            speed,
            zombie,
            start,
            velocity,
        } => {
            *start = t.pos();
            let Some(target) = brain_ref(t).entity(M::AttackTarget) else {
                return;
            };
            let Some(tp) = b::entity_pos(t, target) else {
                return;
            };
            *velocity = (tp - t.pos()).normalize() * f64::from(*speed);
            if charge_can_still_use(t, *start) {
                let s = if *zombie {
                    "minecraft:entity.zombie_nautilus.dash"
                } else {
                    "minecraft:entity.nautilus.dash"
                };
                super::make_sound(t, s);
            }
        }
        Behavior::BreezeShoot => {
            if brain_ref(t).has(M::AttackTarget) {
                set_pose(t, Pose::Shooting);
            }
            brain::brain(t).set_with_expiry(M::BreezeShootCharging, MemValue::Unit, 15);
            crate::mob::kinds::play_sound(t, "minecraft:entity.breeze.inhale", 1.0, 1.0);
        }
        Behavior::BreezeLongJump => {
            if !brain_ref(t).has(M::BreezeJumpInhaling) {
                brain::brain(t).set_with_expiry(M::BreezeJumpInhaling, MemValue::Unit, 10);
            }
            set_pose(t, Pose::Inhaling);
            crate::survival::play_entity_sound(
                t.env.out,
                t.ents,
                None,
                t.id,
                "minecraft:entity.breeze.charge",
                vanillite_protocol::packets::play::clientbound::SoundSource::Hostile,
                1.0,
                1.0,
            );
            if let Some(target) = brain_ref(t).pos(M::BreezeJumpTarget) {
                look_at_eyes(
                    t,
                    crate::mob::randompos::bottom_center(target) + Vec3::new(0.0, 0.5, 0.0),
                );
            }
        }
        Behavior::BreezeShootWhenStuck => {
            brain::brain(t).set_with_expiry(M::BreezeShoot, MemValue::Unit, 60);
        }
        Behavior::BreezeSlide => breeze_slide_start(t),
        Behavior::GolemTransport(_) => super::transport::start(t),
        _ => {}
    }
    let _ = now;
}

/// `tick`.
pub fn behavior_tick(t: &mut Tick<'_, '_>, b: &mut Behavior, now: i64) {
    match b {
        Behavior::GiveItems => {
            let Some(tracker) = deposit_target(t) else {
                return;
            };
            let Some(pos) = b::tracker_pos(t, &tracker) else {
                return;
            };
            let p = t.pos();
            let eye = Vec3::new(p.x, t.eye_y(), p.z);
            if pos.distance_to_sqr(eye) < 9.0 {
                throw_to(t, pos);
                brain::brain(t).set(M::ItemPickupCooldownTicks, MemValue::Int(60));
            }
        }
        Behavior::Charge { velocity, .. } => charge_tick(t, *velocity, b, now),
        Behavior::BreezeShoot => breeze_shoot_tick(t),
        Behavior::BreezeLongJump => long_jump_tick(t),
        Behavior::GolemTransport(s) => super::transport::tick(t, s),
        _ => {}
    }
}

/// `stop`.
pub fn behavior_stop(t: &mut Tick<'_, '_>, b: &mut Behavior, _now: i64) {
    match b {
        Behavior::Charge { .. } => charge_stop(t),
        Behavior::GolemTransport(s) => super::transport::stop(t, s),
        Behavior::BreezeShoot => {
            if t.core().pose() == Pose::Shooting {
                set_pose(t, Pose::Standing);
            }
            let br = brain::brain(t);
            br.set_with_expiry(M::BreezeShootCooldown, MemValue::Unit, 10);
            br.erase(M::BreezeShoot);
        }
        Behavior::BreezeLongJump => {
            let pose = t.core().pose();
            if pose == Pose::LongJumping || pose == Pose::Inhaling {
                set_pose(t, Pose::Standing);
            }
            let br = brain::brain(t);
            br.erase(M::BreezeJumpTarget);
            br.erase(M::BreezeJumpInhaling);
            br.erase(M::BreezeLeavingWater);
        }
        _ => {}
    }
}

/// One shots: `StayCloseToTarget`.
pub fn behavior_trigger(t: &mut Tick<'_, '_>, b: &mut Behavior, _now: i64) -> bool {
    match b {
        Behavior::StayClose => {
            let wants_item = brain_ref(t).has(M::NearestVisibleWantedItem);
            let Some(tracker) = deposit_target(t) else {
                return false;
            };
            if wants_item {
                return false;
            }
            let Some(pos) = b::tracker_pos(t, &tracker) else {
                return false;
            };
            if pos.distance_to_sqr(t.pos()) < 16.0 * 16.0 {
                return false;
            }
            let br = brain::brain(t);
            br.set(M::LookTarget, MemValue::Look(tracker));
            br.set(
                M::WalkTarget,
                MemValue::Walk(WalkTarget {
                    target: tracker,
                    speed: 2.25,
                    close_enough: 4,
                }),
            );
            true
        }
        _ => false,
    }
}

// --- Allay ----------------------------------------------------------------------------------

/// `GoAndGiveItemsToTarget.canThrowItemToTarget`: items in the inventory and somewhere to
/// bring them.
fn can_throw_item(t: &mut Tick<'_, '_>) -> bool {
    !super::allay::allay(t.mob_ref()).inventory.is_empty() && deposit_target(t).is_some()
}

/// `AllayAi.getItemDepositPosition`: the liked note block while it is fresh, else the liked
/// player within 64 blocks in survival or creative.
fn deposit_target(t: &mut Tick<'_, '_>) -> Option<Tracker> {
    if let Some(pos) = brain_ref(t).pos(M::LikedNoteblockPosition) {
        let fresh = brain_ref(t).has(M::LikedNoteblockCooldownTicks);
        let close = BlockPos::new(pos.x, pos.y, pos.z).dist_sqr(t.block_pos()) < 1024.0 * 1024.0;
        let note = t.level.block(pos).block().name() == "minecraft:note_block";
        if fresh && close && note {
            return Some(Tracker::block(BlockPos::new(pos.x, pos.y + 1, pos.z)));
        }
        brain::brain(t).erase(M::LikedNoteblockPosition);
    }
    liked_player_near(t).map(|id| Tracker::entity(id, true))
}

/// `AllayAi.getLikedPlayer`: the liked player within 64 blocks in survival or creative.
fn liked_player_near(t: &Tick<'_, '_>) -> Option<EntityId> {
    let uuid = brain_ref(t).uuid(M::LikedPlayer)?;
    let id = t.ents.player_by_uuid(uuid)?;
    let p = t.ents.player(id)?;
    let mode_ok = matches!(
        p.game_mode,
        vanillite_config::GameMode::Survival | vanillite_config::GameMode::Creative
    );
    let close = t
        .ents
        .store
        .get(id)
        .is_some_and(|e| e.pos().distance_to_sqr(t.pos()) < 64.0 * 64.0);
    (mode_ok && close).then_some(id)
}

/// `AllayAi.throwItem`: one item towards the target, the throw sound now and then.
fn throw_to(t: &mut Tick<'_, '_>, target: Vec3) {
    let stack = {
        let a = super::allay::allay_mut(t.mob());
        if a.inventory.is_empty() {
            return;
        }
        let one = a.inventory.with_count(1);
        a.inventory.shrink(1);
        one
    };
    let id = t.id;
    // `ALLAY_DROP_ITEM_ON_BLOCK` for the liked player, at the block below the target.
    let drop_kind = crate::advancements::trigger::TriggerKind::ALLAY_DROP_ITEM_ON_BLOCK;
    if let Some(player) = liked_player_near(t)
        && crate::advancements::view::listens(t.ents, player, drop_kind)
        && let Some(p) = t.ents.player_mut(player)
    {
        let below = crate::player::play::block_pos_of(target).offset(0, -1, 0);
        p.advancements
            .trigger(crate::advancements::trigger::Event::AllayDropItemOnBlock(
                below,
                stack.clone(),
            ));
    }
    super::allay::throw_item(
        t.ents,
        t.env,
        id,
        stack,
        target + Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(f64::from(0.2_f32), f64::from(0.3_f32), f64::from(0.2_f32)),
        0.2,
    );
    if t.env.game_time % 7 == 0 && t.env.random.next_double() < 0.9 {
        const PITCHES: [f32; 16] = [
            0.5625, 0.625, 0.75, 0.9375, 1.0, 1.0, 1.125, 1.25, 1.5, 1.875, 2.0, 2.25, 2.5, 3.0,
            3.75, 4.0,
        ];
        let i = usize::try_from(t.env.random.next_int_bounded(16)).unwrap_or(0);
        let pitch = PITCHES.get(i).copied().unwrap_or(1.0);
        crate::survival::play_entity_sound(
            t.env.out,
            t.ents,
            None,
            id,
            "minecraft:entity.allay.item_thrown",
            vanillite_protocol::packets::play::clientbound::SoundSource::Neutral,
            1.0,
            pitch,
        );
    }
}

// --- Nautilus charge (`ChargeAttack`) ---------------------------------------------------------

fn charge_can_still_use(t: &mut Tick<'_, '_>, start: Vec3) -> bool {
    let Some(target) = brain_ref(t).entity(M::AttackTarget) else {
        return false;
    };
    if t.mob_ref().tame {
        return false;
    }
    let p = t.pos();
    if p.distance_to_sqr(start) >= 12.0 * 12.0 {
        return false;
    }
    let Some(tp) = b::entity_pos(t, target) else {
        return false;
    };
    if tp.distance_to_sqr(p) >= 11.0 * 11.0 {
        return false;
    }
    crate::mob::living::has_line_of_sight(t, target) && !brain_ref(t).has(M::ChargeCooldownTicks)
}

fn charge_tick(t: &mut Tick<'_, '_>, velocity: Vec3, b: &mut Behavior, now: i64) {
    let Some(target) = brain_ref(t).entity(M::AttackTarget) else {
        return;
    };
    if let Some(o) = t.other(target) {
        crate::mob::kinds::look_at_entity(t, o, 360.0, 360.0);
    }
    t.set_velocity(velocity);
    // The first living entity in its box that it may attack.
    let aabb = t.aabb();
    let mut hit = None;
    let me = t.id;
    let mut found = std::mem::take(&mut t.cx.ids);
    found.clear();
    t.ents.store.for_each_in(&aabb, false, |id| found.push(id));
    for &id in &found {
        if id != me
            && (t.ents.mob(id).is_some() || t.ents.player(id).is_some())
            && crate::mob::goals::can_attack(t, id)
        {
            hit = Some(id);
            break;
        }
    }
    t.cx.ids = found;
    let Some(victim) = hit else {
        return;
    };
    if t.ents.riding.passengers_of(me).contains(&victim) {
        return;
    }
    let damage = t.mob_ref().attrs.attack_damage;
    let source = crate::mob::combat::entity_source(t.ents, t.env.types.mob_attack, me, me);
    #[allow(clippy::cast_possible_truncation)]
    if crate::mob::combat::hurt(t.level, t.ents, t.env, t.cx, victim, source, damage as f32) {
        crate::mob::attack::post_attack_effects(t.level, t.ents, t.env, t.cx, victim, &source);
    }
    // `dealKnockBack`: the speed effects change the push.
    let effects = &t.mob_ref().effects;
    let level = |e| effects.amplifier(e).map_or(0, |a| a + 1);
    #[allow(clippy::cast_precision_loss)]
    let boost = 0.25
        * (level(crate::effect::MobEffect::SPEED) - level(crate::effect::MobEffect::SLOWNESS))
            as f32;
    let speed = if let Behavior::Charge { speed, .. } = b {
        *speed
    } else {
        0.6
    };
    #[allow(clippy::cast_possible_truncation)]
    let factor = (speed * t.mob_ref().attrs.movement_speed as f32).clamp(0.2, 2.0) + boost;
    let power = f64::from(factor * 2.0);
    let v = t.velocity();
    crate::mob::combat::knock_living(t.ents, t.env, victim, power * 0.5, -v.x, -v.z, None);
    let _ = now;
    charge_stop(t);
}

fn charge_stop(t: &mut Tick<'_, '_>) {
    let br = brain::brain(t);
    br.set(M::ChargeCooldownTicks, MemValue::Int(80));
    br.erase(M::AttackTarget);
}

// --- Breeze ---------------------------------------------------------------------------------

/// `LivingEntity.lookAt(EYES, pos)`: head and body turn at once.
fn look_at_eyes(t: &mut Tick<'_, '_>, at: Vec3) {
    let p = t.pos();
    let eye = Vec3::new(p.x, t.eye_y(), p.z);
    let d = at - eye;
    let h = (d.x * d.x + d.z * d.z).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let x_rot = vanillite_base::math::wrap_degrees_f(-(crate::mob::atan2_deg(d.y, h) as f32));
    #[allow(clippy::cast_possible_truncation)]
    let y_rot = vanillite_base::math::wrap_degrees_f(crate::mob::atan2_deg(d.z, d.x) as f32 - 90.0);
    let mut e = t.core_mut();
    let mut rot = e.as_ref().rot();
    rot.x_rot = x_rot;
    rot.y_rot = y_rot;
    rot.y_head_rot = y_rot;
    e.set_rot(rot);
    t.mob().y_body_rot = y_rot;
}

/// `Shoot.tick`: after the charge, one wind charge, then the recovery.
fn breeze_shoot_tick(t: &mut Tick<'_, '_>) {
    let Some(target) = brain_ref(t).entity(M::AttackTarget) else {
        return;
    };
    let Some(tp) = b::entity_pos(t, target) else {
        return;
    };
    look_at_eyes(t, tp);
    if brain_ref(t).has(M::BreezeShootCharging) || brain_ref(t).has(M::BreezeShootRecovering) {
        return;
    }
    brain::brain(t).set_with_expiry(M::BreezeShootRecovering, MemValue::Unit, 4);
    let riding = t.ents.vehicle_of(target).is_some();
    let target_h = t
        .ents
        .store
        .get(target)
        .map_or(1.8, |e| f64::from(e.dims().height));
    let firing = super::breeze::firing_y(t);
    let p = t.pos();
    let dir = Vec3::new(
        tp.x - p.x,
        tp.y + target_h * if riding { 0.8 } else { 0.3 } - firing,
        tp.z - p.z,
    );
    #[allow(clippy::cast_precision_loss)]
    let uncertainty = (5 - t.env.difficulty as i32 * 4) as f32;
    let mut own = crate::mob::missile::new_projectile_random();
    let velocity = super::wind::movement_to_shoot(&mut own, dir, 0.7, uncertainty);
    let rot = super::wind::rotation_of(velocity);
    let data = super::wind::new_charge(crate::mob::missile::MissileKind::BreezeWindCharge);
    crate::mob::missile::spawn_shot(t, Vec3::new(p.x, firing, p.z), velocity, rot, data);
    crate::mob::kinds::play_sound(t, "minecraft:entity.breeze.shoot", 1.5, 1.0);
}

/// `Slide.start`: away from a close target, behind it or into the middle ring.
fn breeze_slide_start(t: &mut Tick<'_, '_>) {
    let Some(enemy) = brain_ref(t).entity(M::AttackTarget) else {
        return;
    };
    let Some(ep) = b::entity_pos(t, enemy) else {
        return;
    };
    let mut position = None;
    if super::breeze::within_inner_circle(t, ep)
        && let Some(away) = crate::mob::randompos::default_pos_away(t, 5, 5, ep)
        && has_line_of_sight_to(t, away)
        && ep.distance_to_sqr(away) > ep.distance_to_sqr(t.pos())
    {
        position = Some(away);
    }
    let position = match position {
        Some(p) => p,
        None => {
            if t.mob().random.next_boolean() {
                point_behind(t, enemy, ep)
            } else {
                let p = t.pos();
                let dir = ep - p;
                let f = t.mob().random.next_double();
                let dist = dir.length() - (8.0 + f * (4.0 - 8.0));
                p + dir.normalize() * dist
            }
        }
    };
    brain::brain(t).set(
        M::WalkTarget,
        MemValue::Walk(WalkTarget {
            target: Tracker::at(Vec3::new(
                position.x.floor() + 0.5,
                position.y.floor(),
                position.z.floor() + 0.5,
            )),
            speed: 0.6,
            close_enough: 1,
        }),
    );
}

/// `BreezeUtil.randomPointBehindTarget`.
fn point_behind(t: &mut Tick<'_, '_>, enemy: EntityId, ep: Vec3) -> Vec3 {
    let head = t.ents.store.get(enemy).map_or(0.0, |e| e.rot().y_head_rot);
    let r = &mut t.mob().random;
    #[allow(clippy::cast_possible_truncation)]
    let angle = head + 180.0 + r.next_gaussian() as f32 * 90.0 / 2.0;
    let dist = 4.0 + r.next_float() * (8.0 - 4.0);
    let dir = b::direction_from_rotation(0.0, angle) * f64::from(dist);
    ep + dir
}

/// `BreezeUtil.hasLineOfSight`: within 50 blocks (or the follow range) and no block in
/// between.
fn has_line_of_sight_to(t: &Tick<'_, '_>, target: Vec3) -> bool {
    let from = t.pos();
    let max = t.mob_ref().attrs.follow_range.max(50.0);
    if target.distance_to_sqr(from) > max * max {
        return false;
    }
    crate::mob::physics::clip(t.level, from, target).is_none()
}

/// `LongJump.canRun`.
fn long_jump_can_run(t: &mut Tick<'_, '_>) -> bool {
    let fluids = t.mob_ref().fluids;
    if !t.on_ground() && !fluids.in_water() {
        return false;
    }
    if fluids.water_height > super::super::super::eye_height(t.mob_ref()) || fluids.in_lava() {
        return false;
    }
    if brain_ref(t).has(M::BreezeJumpTarget) {
        return true;
    }
    let Some(target) = brain_ref(t).entity(M::AttackTarget) else {
        return false;
    };
    let Some(tp) = b::entity_pos(t, target) else {
        return false;
    };
    let range = t.mob_ref().attrs.follow_range;
    let dist_sqr = tp.distance_to_sqr(t.pos());
    if dist_sqr >= range * range {
        brain::brain(t).erase(M::AttackTarget);
        return false;
    }
    if dist_sqr.sqrt() - 4.0 <= 0.0 {
        return false;
    }
    let here = t.block_pos();
    if t.level.block(here).block().name() == "minecraft:honey_block" {
        return false;
    }
    for i in 1..=4 {
        let s = t.level.block(BlockPos::new(here.x, here.y + i, here.z));
        if !s.is_air() && !crate::mob::is_water(s) {
            return false;
        }
    }
    let behind = point_behind(t, target, tp);
    let Some(pos) = snap_to_surface(t, behind) else {
        return false;
    };
    let below = t.level.block(BlockPos::new(pos.x, pos.y - 1, pos.z));
    if !crate::mob::spawn::valid_empty_spawn_block(below, t.core().kind()) && !below.is_air() {
        // `isBlockDangerous` of the block below.
        let name = below.block().name();
        if name.contains("fire") || name == "minecraft:magma_block" || name == "minecraft:cactus" {
            return false;
        }
    }
    let center = crate::mob::randompos::bottom_center(pos) + Vec3::new(0.0, 0.5, 0.0);
    let above = Vec3::new(center.x, center.y + 4.0, center.z);
    if !has_line_of_sight_to(t, center) && !has_line_of_sight_to(t, above) {
        return false;
    }
    brain::brain(t).set(M::BreezeJumpTarget, MemValue::Pos(pos));
    true
}

/// `LongJump.snapToSurface`: the surface within 10 blocks below, else above.
fn snap_to_surface(t: &Tick<'_, '_>, target: Vec3) -> Option<BlockPos> {
    let down = Vec3::new(target.x, target.y - 10.0, target.z);
    if let Some((_, at)) = crate::mob::physics::clip(t.level, target, down) {
        let p = BlockPos::containing(at.x, at.y, at.z);
        return Some(BlockPos::new(p.x, p.y + 1, p.z));
    }
    let up = Vec3::new(target.x, target.y + 10.0, target.z);
    crate::mob::physics::clip(t.level, target, up).map(|(_, at)| {
        let p = BlockPos::containing(at.x, at.y, at.z);
        BlockPos::new(p.x, p.y + 1, p.z)
    })
}

/// `LongJump.tick`: after inhaling it jumps; landing ends the jump with a cooldown and the
/// order to shoot.
fn long_jump_tick(t: &mut Tick<'_, '_>) {
    let in_water = t.mob_ref().fluids.in_water();
    if !in_water && brain_ref(t).has(M::BreezeLeavingWater) {
        brain::brain(t).erase(M::BreezeLeavingWater);
    }
    let pose = t.core().pose();
    let finished_inhaling = !brain_ref(t).has(M::BreezeJumpInhaling) && pose == Pose::Inhaling;
    if finished_inhaling {
        let target = brain_ref(t).pos(M::BreezeJumpTarget);
        let velocity =
            target.and_then(|p| optimal_jump(t, crate::mob::randompos::bottom_center(p)));
        let Some(v) = velocity else {
            set_pose(t, Pose::Standing);
            return;
        };
        if in_water {
            brain::brain(t).set(M::BreezeLeavingWater, MemValue::Unit);
        }
        crate::mob::kinds::play_sound(t, "minecraft:entity.breeze.jump", 1.0, 1.0);
        set_pose(t, Pose::LongJumping);
        let body = t.mob_ref().y_body_rot;
        let mut rot = t.core().rot();
        rot.y_rot = body;
        t.core_mut().set_rot(rot);
        super::breeze::set_discard_friction(t.mob(), true);
        t.set_velocity(v);
        return;
    }
    let landed_water = in_water && !brain_ref(t).has(M::BreezeLeavingWater);
    if pose == Pose::LongJumping && (t.on_ground() || landed_water) {
        crate::mob::kinds::play_sound(t, "minecraft:entity.breeze.land", 1.0, 1.0);
        set_pose(t, Pose::Standing);
        super::breeze::set_discard_friction(t.mob(), false);
        let hurt = brain_ref(t).has(M::HurtBy);
        let br = brain::brain(t);
        br.set_with_expiry(
            M::BreezeJumpCooldown,
            MemValue::Unit,
            if hurt { 2 } else { 10 },
        );
        br.set_with_expiry(M::BreezeShoot, MemValue::Unit, 100);
    }
}

/// `LongJump.calculateOptimalJumpVector`: the first of the allowed angles (shuffled) that
/// reaches the target within the maximum speed.
fn optimal_jump(t: &mut Tick<'_, '_>, target: Vec3) -> Option<Vec3> {
    let mut angles = [40, 55, 60, 75, 80];
    let r = &mut t.mob().random;
    for i in (2..=angles.len()).rev() {
        let j = usize::try_from(r.next_int_bounded(i32::try_from(i).unwrap_or(1))).unwrap_or(0);
        angles.swap(i - 1, j);
    }
    #[allow(clippy::cast_possible_truncation)]
    let max_velocity = 0.058_333_334_f32 * t.mob_ref().attrs.follow_range as f32;
    let gravity = t.mob_ref().attrs.gravity;
    let mob = t.pos();
    for angle in angles {
        if let Some(v) = jump_vector_for_angle(mob, target, max_velocity, angle, gravity) {
            if let Some(a) = t
                .mob_ref()
                .effects
                .amplifier(crate::effect::MobEffect::JUMP_BOOST)
            {
                // `getJumpBoostPower`: 0.1 per level.
                let boost = v.normalize().y * 0.1 * (f64::from(a) + 1.0);
                return Some(v + Vec3::new(0.0, boost, 0.0));
            }
            return Some(v);
        }
    }
    None
}

/// `LongJumpUtil.calculateJumpVectorForAngle` without the collision samples.
pub fn jump_vector_for_angle(
    mob: Vec3,
    target: Vec3,
    max_velocity: f32,
    angle: i32,
    gravity: f64,
) -> Option<Vec3> {
    let plane = Vec3::new(target.x - mob.x, 0.0, target.z - mob.z).normalize() * 0.5;
    let target = target - plane;
    let d = target - mob;
    #[allow(clippy::cast_precision_loss)]
    let ang = angle as f32 * std::f32::consts::PI / 180.0;
    let xz = d.z.atan2(d.x);
    let r2 = d.x * d.x + d.z * d.z;
    let r = r2.sqrt();
    let y = d.y;
    let sin2 = f64::from(2.0 * ang).sin();
    let cos_sqr = f64::from(ang).cos().powi(2);
    let v0sqr = r2 * gravity / (r * sin2 - 2.0 * y * cos_sqr);
    if v0sqr < 0.0 {
        return None;
    }
    let v0 = v0sqr.sqrt();
    if v0 > f64::from(max_velocity) {
        return None;
    }
    let v0r = v0 * f64::from(ang).cos();
    let v0y = v0 * f64::from(ang).sin();
    Some(Vec3::new(v0r * xz.cos(), v0y, v0r * xz.sin()) * f64::from(0.95_f32))
}

// --- The family's sensor --------------------------------------------------------------------

pub fn sensor_scan_rate(_s: Sensor) -> i32 {
    20
}

pub fn sensor_requires(s: &Sensor) -> &'static [M] {
    match s {
        Sensor::BreezeAttackEntity => &[
            M::NearestLivingEntities,
            M::NearestVisibleLivingEntities,
            M::NearestAttackable,
        ],
        Sensor::Idle => &[],
    }
}

/// `BreezeAttackEntitySensor`: the nearest living entities, then the first attackable
/// player or iron golem (no creative or spectator players).
pub fn sensor_tick(t: &mut Tick<'_, '_>, s: Sensor) {
    if !matches!(s, Sensor::BreezeAttackEntity) {
        return;
    }
    let r = t.mob_ref().attrs.follow_range;
    let area = t.aabb().inflate(r, r, r);
    let mut list = Vec::new();
    crate::mob::brain::sensor::living_in(t, &area, &mut list);
    let mut found = None;
    for &id in &list {
        let creative = t.ents.player(id).is_some_and(|p| {
            matches!(
                p.game_mode,
                vanillite_config::GameMode::Creative | vanillite_config::GameMode::Spectator
            )
        });
        if creative || !super::breeze::can_attack_kind(t, id) {
            continue;
        }
        if sensing::is_attackable(t, id) {
            found = Some(id);
            break;
        }
    }
    let b = brain::brain(t);
    let visible = crate::mob::brain::Visible {
        sight: vec![0; list.len()],
        entities: list.clone(),
    };
    if list.is_empty() {
        b.erase(M::NearestLivingEntities);
    } else {
        b.set(M::NearestLivingEntities, MemValue::Entities(list));
    }
    b.set(M::NearestVisibleLivingEntities, MemValue::Visible(visible));
    b.set_opt(M::NearestAttackable, found.map(MemValue::Entity));
}

/// The breeze wind charge's damage source kind.
pub fn wind_charge_source(t: &Tick<'_, '_>) -> DamageSource {
    DamageSource::of(t.env.types.named("minecraft:wind_charge"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_gets_a_brain() {
        for kind in [
            Kind::Camel,
            Kind::CamelHusk,
            Kind::Nautilus,
            Kind::ZombieNautilus,
            Kind::Allay,
            Kind::CopperGolem,
            Kind::Breeze,
            Kind::Creaking,
        ] {
            let m = MobData::new(kind, 5);
            assert!(m.brain.is_some(), "{kind:?}");
        }
        // Adult happy ghasts use goals.
        assert!(MobData::new(Kind::HappyGhast, 5).brain.is_none());
    }

    #[test]
    fn breeze_defaults_to_fight() {
        let m = MobData::new(Kind::Breeze, 1);
        let b = m.brain.as_deref().expect("brain");
        assert!(b.is_active(Activity::Fight));
    }

    #[test]
    fn jump_vector_reaches_level_targets() {
        let v = jump_vector_for_angle(Vec3::ZERO, Vec3::new(8.0, 0.0, 0.0), 1.4, 40, 0.08)
            .expect("reachable");
        assert!(v.x > 0.0 && v.y > 0.0 && v.z.abs() < 1e-9);
        assert!(
            jump_vector_for_angle(Vec3::ZERO, Vec3::new(80.0, 0.0, 0.0), 1.4, 40, 0.08).is_none()
        );
    }

    #[test]
    fn nautilus_memories() {
        let mut m = MobData::new(Kind::Nautilus, 9);
        nautilus_init_memories(&mut m);
        let b = m.brain.as_deref().expect("brain");
        let ticks = b.int(M::AttackTargetCooldown).expect("cooldown");
        assert!((2400..=3600).contains(&ticks));
        nautilus_set_anger(&mut m, 77, 0);
        assert_eq!(
            m.brain.as_deref().and_then(|b| b.uuid(M::AngryAt)),
            Some(77)
        );
    }

    struct LevelBlocks<'a>(&'a crate::level::Level);

    impl crate::survival::blocks::BlockAccess for LevelBlocks<'_> {
        fn block_state(&self, pos: BlockPos) -> Option<vanillite_data::block::BlockState> {
            Some(self.0.block(pos))
        }
    }

    /// Runs one mob tick of `id` on `level` at `game_time` (like the brain tests' harness).
    fn tick_mob(level: &mut crate::level::Level, id: EntityId, game_time: i64) {
        let mut ents = std::mem::take(&mut level.entities);
        let types = crate::survival::damage::DamageTypes::built_in();
        let facts = crate::survival::blocks::BlockFacts::default();
        let rules = crate::survival::rules::GameRules::default();
        let mut random = vanillite_base::random::LegacyRandom::new(7);
        let mut out = Vec::new();
        let raining = |_| false;
        let hooks = crate::hooks::NoHooks;
        let names = |name: &str, _| vanillite_protocol::text::Component::text(name);
        {
            let blocks = LevelBlocks(level);
            let mut env = crate::survival::player::Env {
                types: &types,
                facts: &facts,
                blocks: &blocks,
                rules: &rules,
                difficulty: crate::survival::rules::Difficulty::Normal,
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
            let mut cx = crate::mob::MobContext {
                path_budget: u64::MAX,
                ..crate::mob::MobContext::default()
            };
            crate::mob::tick(level, &mut ents, &mut env, &mut cx, id);
        }
        level.entities = ents;
    }

    #[test]
    fn every_kind_ticks_on_a_flat_world() {
        for kind in [
            Kind::Camel,
            Kind::CamelHusk,
            Kind::HappyGhast,
            Kind::Nautilus,
            Kind::ZombieNautilus,
            Kind::Allay,
            Kind::CopperGolem,
            Kind::Breeze,
            Kind::Creaking,
        ] {
            let mut level = crate::level::tests::flat_level();
            let mut m = MobData::new(kind, 11);
            m.persistent = true;
            let id = crate::mob::spawn::add_mob(
                &mut level,
                m,
                Vec3::new(0.5, -60.0, 0.5),
                0.0,
                crate::mob::fam::SpawnReason::Command,
            );
            for time in 0..300 {
                tick_mob(&mut level, id, time);
            }
            assert!(level.entities.mob(id).is_some(), "{kind:?} is still there");
        }
    }
}
