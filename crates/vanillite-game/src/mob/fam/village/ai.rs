//! The villager brain (`VillagerGoalPackages`, the villager behaviours and sensors).
//!
//! Every package is built like Vanilla's (`getCorePackage`, `getWorkPackage`, …) from the
//! shared behaviours of [`crate::mob::brain::behavior`] and the villager behaviours here.
//! What a behaviour keeps between ticks that is not `Copy` (retry caches, the farmland list,
//! the shown trades) lives in the villager state ([`super::villager::Villager`]).

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, SectionPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_data::item::ids as items;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::EntityId;
use vanillite_worldgen::behaviour::StateExt;

use super::villager::{self as v, profession, vs, vs_mut};
use super::{state, state_mut};
use crate::effect::MobEffect;
use crate::item::{EquipmentSlot, ItemStack};
use crate::mob::brain::behavior::{self as bh, FamBehavior, Shape, Spec, present};
use crate::mob::brain::memory::MemoryStatus as S;
use crate::mob::brain::{
    Activity, ActivitySpec, BrainSpec, GlobalPos, MemValue, MemoryStatus, MemoryType as M,
    SensorKind, Tracker, WalkTarget, brain, brain_ref, sensing,
};
use crate::mob::spawn::Category;
use crate::mob::{Kind, Tick, WorldAction, randompos};
use crate::poi::{Occupancy, PoiType};

const SPEED: f32 = 0.5;

/// Which `AcquirePoi` of the core package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acquire {
    /// A job site into `POTENTIAL_JOB_SITE` (adults without a job site).
    Job,
    /// A free bed into `HOME` (event 14).
    Home,
    /// A bell into `MEETING_POINT` (adults, event 14).
    Meeting,
}

/// The villager behaviours.
#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    /// Never runs.
    Idle,
    /// `InteractWithDoor`.
    Door {
        last_node: Option<BlockPos>,
        cooldown: i32,
    },
    /// `VillagerPanicTrigger`.
    PanicTrigger,
    /// `WakeUp`.
    WakeUp,
    /// `ReactToBell`.
    ReactToBell,
    /// `SetRaidStatus`.
    SetRaidStatus,
    /// `ValidateNearbyPoi(predicate of the memory, memory)`.
    ValidatePoi { memory: M },
    /// `PoiCompetitorScan`.
    PoiCompetitorScan,
    /// `LookAndFollowTradingPlayerSink(speed)`.
    FollowTradingPlayer,
    /// `AcquirePoi`.
    AcquirePoi { which: Acquire, next_start: i64 },
    /// `GoToPotentialJobSite(speed)`.
    GoToPotentialJobSite,
    /// `YieldJobSite(speed)`.
    YieldJobSite,
    /// `AssignProfessionFromJobSite`.
    AssignProfession,
    /// `ResetProfession`.
    ResetProfession,
    /// `WorkAtPoi` / `WorkAtComposter`.
    WorkAtPoi { composter: bool, last_check: i64 },
    /// `StrollToPoiList(SECONDARY_JOB_SITE, speed, 1, 6, JOB_SITE)`.
    StrollToPoiList { next_ok: i64 },
    /// `HarvestFarmland`.
    HarvestFarmland {
        target: Option<BlockPos>,
        next_ok: i64,
        worked: i32,
    },
    /// `UseBonemeal`.
    UseBonemeal {
        crop: Option<BlockPos>,
        next_cycle: i64,
        last_session: i32,
        worked: i32,
    },
    /// `ShowTradesToPlayer(400, 1600)`.
    ShowTrades {
        cycle: i32,
        index: i32,
        look_time: i32,
    },
    /// `SetWalkTargetFromBlockMemory(memory, speed, closeEnough, tooFar, tooLong)`.
    WalkToMemory {
        memory: M,
        speed: f32,
        close: i32,
        too_far: i32,
        too_long: i64,
    },
    /// `GiveGiftToHero(100)`.
    GiveGift {
        until_next: i32,
        given: bool,
        since: i64,
    },
    /// `UpdateActivityFromSchedule`.
    UpdateSchedule,
    /// `PlayTagWithOtherKids`.
    PlayTag,
    /// `VillageBoundRandomStroll(speed, xz, y)`.
    VillageStroll { speed: f32, xz: i32, y: i32 },
    /// `JumpOnBed(speed)`.
    JumpOnBed {
        bed: Option<BlockPos>,
        time_to_reach: i32,
        jumps: i32,
        cooldown: i32,
    },
    /// `SleepInBed`.
    SleepInBed { next_ok: i64 },
    /// `SetClosestHomeAsWalkTarget(speed)`.
    ClosestHome,
    /// `InsideBrownianWalk(speed)`.
    InsideBrownian,
    /// `GoToClosestVillage(speed, 4)`.
    ClosestVillage,
    /// `SocializeAtBell`.
    SocializeAtBell,
    /// `TradeWithVillager`.
    TradeWithVillager,
    /// `VillagerMakeLove`.
    MakeLove { birth: i64 },
    /// `VillagerCalmDown`.
    CalmDown,
    /// `RingBell`.
    RingBell,
    /// `MoveToSkySeeingSpot(speed)`.
    SkySeeingSpot { speed: f32 },
    /// `CelebrateVillagersSurvivedRaid(600, 600)`.
    Celebrate,
    /// `LocateHidingPlace(radius, speed, closeEnough)`.
    LocateHiding { radius: i32, speed: f32, close: i32 },
    /// `ResetRaidStatus`.
    ResetRaidStatus,
    /// `SetHiddenState(seconds, closeEnough)`.
    HiddenState { stay_ticks: i32, close: i32 },
    /// `InteractWith.of(VILLAGER, 8, canBreed, canBreed, BREED_TARGET, speed, 2)`.
    InteractBreed,
}

/// The villager sensors.
#[derive(Clone, Copy, Debug)]
pub enum Sensor {
    /// Never ticks.
    Idle,
    /// `NearestBedSensor`.
    NearestBed,
    /// `VillagerHostilesSensor`.
    Hostiles,
    /// `VillagerBabiesSensor`.
    Babies,
    /// `SecondaryPoiSensor`.
    SecondaryPois,
    /// `GolemSensor`.
    Golem,
}

fn spec(b: Behavior) -> Spec {
    Spec::fam(FamBehavior::Village(b))
}

// --- Look filters ----------------------------------------------------------------------------

fn kind_of(t: &Tick<'_, '_>, id: EntityId) -> Option<Kind> {
    t.ents.mob(id).map(|m| m.kind)
}

fn is_villager(t: &Tick<'_, '_>, id: EntityId) -> bool {
    kind_of(t, id) == Some(Kind::Villager)
}

fn is_cat(t: &Tick<'_, '_>, id: EntityId) -> bool {
    kind_of(t, id) == Some(Kind::Cat)
}

fn is_player(t: &Tick<'_, '_>, id: EntityId) -> bool {
    t.ents.player(id).is_some()
}

fn category(t: &Tick<'_, '_>, id: EntityId) -> Option<Category> {
    t.ents.store.get(id).and_then(|e| Category::of(e.kind()))
}

fn is_creature(t: &Tick<'_, '_>, id: EntityId) -> bool {
    category(t, id) == Some(Category::Creature)
}
fn is_water_creature(t: &Tick<'_, '_>, id: EntityId) -> bool {
    category(t, id) == Some(Category::WaterCreature)
}
fn is_axolotls(t: &Tick<'_, '_>, id: EntityId) -> bool {
    category(t, id) == Some(Category::Axolotls)
}
fn is_underground_water(t: &Tick<'_, '_>, id: EntityId) -> bool {
    category(t, id) == Some(Category::UndergroundWaterCreature)
}
fn is_water_ambient(t: &Tick<'_, '_>, id: EntityId) -> bool {
    category(t, id) == Some(Category::WaterAmbient)
}
fn is_monster(t: &Tick<'_, '_>, id: EntityId) -> bool {
    category(t, id) == Some(Category::Monster)
}

/// `getFullLookBehavior`.
fn full_look() -> (i32, Spec) {
    (
        5,
        Spec::run_one(vec![
            (Spec::look_at_entity(Some(is_cat), 8.0), 8),
            (Spec::look_at_entity(Some(is_villager), 8.0), 2),
            (Spec::look_at_entity(Some(is_player), 8.0), 2),
            (Spec::look_at_entity(Some(is_creature), 8.0), 1),
            (Spec::look_at_entity(Some(is_water_creature), 8.0), 1),
            (Spec::look_at_entity(Some(is_axolotls), 8.0), 1),
            (Spec::look_at_entity(Some(is_underground_water), 8.0), 1),
            (Spec::look_at_entity(Some(is_water_ambient), 8.0), 1),
            (Spec::look_at_entity(Some(is_monster), 8.0), 1),
            (Spec::do_nothing(30, 60), 2),
        ]),
    )
}

/// `getMinimalLookBehavior`.
fn minimal_look() -> (i32, Spec) {
    (
        5,
        Spec::run_one(vec![
            (Spec::look_at_entity(Some(is_villager), 8.0), 2),
            (Spec::look_at_entity(Some(is_player), 8.0), 2),
            (Spec::do_nothing(30, 60), 8),
        ]),
    )
}

fn interact_with(filter: bh::EntityPred) -> Spec {
    Spec::of(bh::Behavior::InteractWith {
        filter,
        range: 8,
        memory: M::InteractionTarget,
        speed: SPEED,
        stop: 2,
    })
}

fn stroll(speed: f32) -> Spec {
    spec(Behavior::VillageStroll {
        speed,
        xz: 10,
        y: 7,
    })
}

fn walk_to_memory(memory: M, speed: f32, close: i32, too_far: i32, too_long: i64) -> Spec {
    spec(Behavior::WalkToMemory {
        memory,
        speed,
        close,
        too_far,
        too_long,
    })
}

fn give_gift() -> Spec {
    spec(Behavior::GiveGift {
        until_next: 600,
        given: false,
        since: 0,
    })
}

fn show_trades() -> Spec {
    spec(Behavior::ShowTrades {
        cycle: 0,
        index: 0,
        look_time: 0,
    })
}

fn set_look_and_interact() -> Spec {
    Spec::of(bh::Behavior::SetLookAndInteract {
        filter: is_player,
        range: 4,
    })
}

fn jump_on_bed() -> Spec {
    spec(Behavior::JumpOnBed {
        bed: None,
        time_to_reach: 0,
        jumps: 0,
        cooldown: 0,
    })
}

fn gate_exit(exit: &'static [M], inner: Spec) -> Spec {
    Spec::gate(&[], exit, false, true, vec![(inner, 1)])
}

/// `raidExistsAndActive`.
fn raid_active(t: &mut Tick<'_, '_>) -> bool {
    let pos = t.block_pos();
    t.ents
        .raids
        .raid_at(pos)
        .is_some_and(|r| r.is_active() && !r.is_victory() && !r.is_loss())
}

/// `raidExistsAndNotVictory` (true for a victory, like Vanilla's).
fn raid_victory(t: &mut Tick<'_, '_>) -> bool {
    let pos = t.block_pos();
    t.ents.raids.raid_at(pos).is_some_and(|r| r.is_victory())
}

/// The brain of a villager (`BRAIN_PROVIDER` with `VillagerGoalPackages`).
pub fn brain_spec(prof: u16, baby: bool) -> BrainSpec {
    let mut activities = Vec::new();
    if baby {
        activities.push(ActivitySpec::with_priorities(
            Activity::Play,
            vec![
                (0, Spec::move_to_target_sink_for(80, 120)),
                full_look(),
                (5, spec(Behavior::PlayTag)),
                (
                    5,
                    Spec::run_one_if(
                        &[(M::VisibleVillagerBabies, S::ValueAbsent)],
                        vec![
                            (interact_with(is_villager), 2),
                            (interact_with(is_cat), 1),
                            (stroll(SPEED), 1),
                            (Spec::walk_to_look_target(SPEED, 2), 1),
                            (jump_on_bed(), 2),
                            (Spec::do_nothing(20, 40), 2),
                        ],
                    ),
                ),
                (99, spec(Behavior::UpdateSchedule)),
            ],
        ));
    } else {
        let farmer = prof == profession::FARMER;
        activities.push(
            ActivitySpec::with_priorities(
                Activity::Work,
                vec![
                    minimal_look(),
                    (
                        5,
                        Spec::run_one(vec![
                            (
                                spec(Behavior::WorkAtPoi {
                                    composter: farmer,
                                    last_check: 0,
                                }),
                                7,
                            ),
                            (
                                Spec::of(bh::Behavior::StrollAroundPoi {
                                    memory: M::JobSite,
                                    speed: 0.4,
                                    max_dist: 4,
                                    next_ok: 0,
                                }),
                                2,
                            ),
                            (
                                Spec::of(bh::Behavior::StrollToPoi {
                                    memory: M::JobSite,
                                    speed: 0.4,
                                    close: 1,
                                    max_dist: 10,
                                    next_ok: 0,
                                }),
                                5,
                            ),
                            (spec(Behavior::StrollToPoiList { next_ok: 0 }), 5),
                            (
                                spec(Behavior::HarvestFarmland {
                                    target: None,
                                    next_ok: 0,
                                    worked: 0,
                                }),
                                if farmer { 2 } else { 5 },
                            ),
                            (
                                spec(Behavior::UseBonemeal {
                                    crop: None,
                                    next_cycle: 0,
                                    last_session: 0,
                                    worked: 0,
                                }),
                                if farmer { 4 } else { 7 },
                            ),
                        ]),
                    ),
                    (10, show_trades()),
                    (10, set_look_and_interact()),
                    (2, walk_to_memory(M::JobSite, SPEED, 9, 100, 1200)),
                    (3, give_gift()),
                    (99, spec(Behavior::UpdateSchedule)),
                ],
            )
            .requires(&[(M::JobSite, S::ValuePresent)]),
        );
    }
    activities.push(ActivitySpec::with_priorities(
        Activity::Core,
        vec![
            (0, Spec::swim(0.8)),
            (
                0,
                spec(Behavior::Door {
                    last_node: None,
                    cooldown: 0,
                }),
            ),
            (0, Spec::look_at_target_sink(45, 90)),
            (0, spec(Behavior::PanicTrigger)),
            (0, spec(Behavior::WakeUp)),
            (0, spec(Behavior::ReactToBell)),
            (0, spec(Behavior::SetRaidStatus)),
            (0, spec(Behavior::ValidatePoi { memory: M::JobSite })),
            (
                0,
                spec(Behavior::ValidatePoi {
                    memory: M::PotentialJobSite,
                }),
            ),
            (1, Spec::move_to_target_sink()),
            (2, spec(Behavior::PoiCompetitorScan)),
            (3, spec(Behavior::FollowTradingPlayer)),
            (5, Spec::go_to_wanted_item(None, SPEED, false, 4)),
            (
                6,
                spec(Behavior::AcquirePoi {
                    which: Acquire::Job,
                    next_start: 0,
                }),
            ),
            (7, spec(Behavior::GoToPotentialJobSite)),
            (8, spec(Behavior::YieldJobSite)),
            (
                10,
                spec(Behavior::AcquirePoi {
                    which: Acquire::Home,
                    next_start: 0,
                }),
            ),
            (
                10,
                spec(Behavior::AcquirePoi {
                    which: Acquire::Meeting,
                    next_start: 0,
                }),
            ),
            (10, spec(Behavior::AssignProfession)),
            (10, spec(Behavior::ResetProfession)),
        ],
    ));
    activities.push(
        ActivitySpec::with_priorities(
            Activity::Meet,
            vec![
                (
                    2,
                    Spec::trigger_one_shuffled(vec![
                        (
                            Spec::of(bh::Behavior::StrollAroundPoi {
                                memory: M::MeetingPoint,
                                speed: 0.4,
                                max_dist: 40,
                                next_ok: 0,
                            }),
                            2,
                        ),
                        (spec(Behavior::SocializeAtBell), 2),
                    ]),
                ),
                (10, show_trades()),
                (10, set_look_and_interact()),
                (2, walk_to_memory(M::MeetingPoint, SPEED, 6, 100, 200)),
                (3, give_gift()),
                (
                    3,
                    spec(Behavior::ValidatePoi {
                        memory: M::MeetingPoint,
                    }),
                ),
                (
                    3,
                    gate_exit(&[M::InteractionTarget], spec(Behavior::TradeWithVillager)),
                ),
                full_look(),
                (99, spec(Behavior::UpdateSchedule)),
            ],
        )
        .requires(&[(M::MeetingPoint, S::ValuePresent)]),
    );
    activities.push(ActivitySpec::with_priorities(
        Activity::Rest,
        vec![
            (2, walk_to_memory(M::Home, SPEED, 1, 150, 1200)),
            (3, spec(Behavior::ValidatePoi { memory: M::Home })),
            (3, spec(Behavior::SleepInBed { next_ok: 0 })),
            (
                5,
                Spec::run_one_if(
                    &[(M::Home, S::ValueAbsent)],
                    vec![
                        (spec(Behavior::ClosestHome), 1),
                        (spec(Behavior::InsideBrownian), 4),
                        (spec(Behavior::ClosestVillage), 2),
                        (Spec::do_nothing(20, 40), 2),
                    ],
                ),
            ),
            minimal_look(),
            (99, spec(Behavior::UpdateSchedule)),
        ],
    ));
    activities.push(ActivitySpec::with_priorities(
        Activity::Idle,
        vec![
            (
                2,
                Spec::run_one(vec![
                    (interact_with(is_villager), 2),
                    (spec(Behavior::InteractBreed), 1),
                    (interact_with(is_cat), 1),
                    (stroll(SPEED), 1),
                    (Spec::walk_to_look_target(SPEED, 2), 1),
                    (jump_on_bed(), 1),
                    (Spec::do_nothing(30, 60), 1),
                ]),
            ),
            (3, give_gift()),
            (3, set_look_and_interact()),
            (3, show_trades()),
            (
                3,
                gate_exit(&[M::InteractionTarget], spec(Behavior::TradeWithVillager)),
            ),
            (
                3,
                gate_exit(&[M::BreedTarget], spec(Behavior::MakeLove { birth: 0 })),
            ),
            full_look(),
            (99, spec(Behavior::UpdateSchedule)),
        ],
    ));
    let runaway = SPEED * 1.5;
    activities.push(ActivitySpec::with_priorities(
        Activity::Panic,
        vec![
            (0, spec(Behavior::CalmDown)),
            (
                1,
                Spec::walk_away_from(M::NearestHostile, runaway, 6, false),
            ),
            (1, Spec::walk_away_from(M::HurtByEntity, runaway, 6, false)),
            (
                3,
                spec(Behavior::VillageStroll {
                    speed: runaway,
                    xz: 2,
                    y: 2,
                }),
            ),
            minimal_look(),
        ],
    ));
    activities.push(ActivitySpec::with_priorities(
        Activity::PreRaid,
        vec![
            (0, spec(Behavior::RingBell)),
            (
                0,
                Spec::trigger_one_shuffled(vec![
                    (walk_to_memory(M::MeetingPoint, SPEED * 1.5, 2, 150, 200), 6),
                    (stroll(SPEED * 1.5), 2),
                ]),
            ),
            minimal_look(),
            (99, spec(Behavior::ResetRaidStatus)),
        ],
    ));
    activities.push(ActivitySpec::with_priorities(
        Activity::Raid,
        vec![
            (
                0,
                Spec::sequence(
                    Spec::of(bh::Behavior::Check(raid_victory)),
                    Spec::trigger_one_shuffled(vec![
                        (spec(Behavior::SkySeeingSpot { speed: SPEED }), 5),
                        (stroll(SPEED * 1.1), 2),
                    ]),
                ),
            ),
            (0, spec(Behavior::Celebrate)),
            (
                2,
                Spec::sequence(
                    Spec::of(bh::Behavior::Check(raid_active)),
                    spec(Behavior::LocateHiding {
                        radius: 24,
                        speed: SPEED * 1.4,
                        close: 1,
                    }),
                ),
            ),
            minimal_look(),
            (99, spec(Behavior::ResetRaidStatus)),
        ],
    ));
    activities.push(ActivitySpec::with_priorities(
        Activity::Hide,
        vec![
            (
                0,
                spec(Behavior::HiddenState {
                    stay_ticks: 15 * 20,
                    close: 2,
                }),
            ),
            (
                1,
                spec(Behavior::LocateHiding {
                    radius: 32,
                    speed: SPEED * 1.25,
                    close: 2,
                }),
            ),
            minimal_look(),
        ],
    ));
    BrainSpec {
        memories: vec![
            M::Home,
            M::JobSite,
            M::PotentialJobSite,
            M::MeetingPoint,
            M::NearestLivingEntities,
            M::NearestVisibleLivingEntities,
            M::VisibleVillagerBabies,
            M::NearestPlayers,
            M::NearestVisiblePlayer,
            M::NearestVisibleAttackablePlayer,
            M::NearestVisibleWantedItem,
            M::ItemPickupCooldownTicks,
            M::WalkTarget,
            M::LookTarget,
            M::InteractionTarget,
            M::BreedTarget,
            M::Path,
            M::DoorsToClose,
            M::NearestBed,
            M::HurtBy,
            M::HurtByEntity,
            M::NearestHostile,
            M::SecondaryJobSite,
            M::HidingPlace,
            M::HeardBellTime,
            M::CantReachWalkTargetSince,
            M::LastSlept,
            M::LastWoken,
            M::LastWorkedAtPoi,
            M::GolemDetectedRecently,
        ],
        sensors: vec![
            SensorKind::NearestLivingEntities,
            SensorKind::NearestPlayers,
            SensorKind::NearestItems,
            SensorKind::Fam(sensor_fam(Sensor::NearestBed)),
            SensorKind::HurtBy,
            SensorKind::Fam(sensor_fam(Sensor::Hostiles)),
            SensorKind::Fam(sensor_fam(Sensor::Babies)),
            SensorKind::Fam(sensor_fam(Sensor::SecondaryPois)),
            SensorKind::Fam(sensor_fam(Sensor::Golem)),
        ],
        activities,
        core: vec![Activity::Core],
        default_activity: Some(Activity::Idle),
        schedule: None,
    }
}

fn sensor_fam(s: Sensor) -> crate::mob::brain::sensor::FamSensor {
    crate::mob::brain::sensor::FamSensor::Village(s)
}

// --- Sensors -------------------------------------------------------------------------------

pub fn sensor_scan_rate(s: Sensor) -> i32 {
    match s {
        Sensor::SecondaryPois => 40,
        Sensor::Golem => 200,
        _ => 20,
    }
}

pub fn sensor_requires(s: &Sensor) -> &'static [M] {
    match s {
        Sensor::Idle => &[],
        Sensor::NearestBed => &[M::NearestBed],
        Sensor::Hostiles => &[M::NearestHostile, M::NearestVisibleLivingEntities],
        Sensor::Babies => &[M::VisibleVillagerBabies],
        Sensor::SecondaryPois => &[M::SecondaryJobSite],
        Sensor::Golem => &[M::NearestLivingEntities, M::GolemDetectedRecently],
    }
}

/// `VillagerHostilesSensor.ACCEPTABLE_DISTANCE_FROM_HOSTILES`.
fn hostile_distance(kind: Kind) -> Option<f32> {
    Some(match kind {
        Kind::Drowned | Kind::Husk | Kind::Vex | Kind::Zombie | Kind::ZombieVillager => 8.0,
        Kind::Evoker | Kind::Illusioner | Kind::Ravager => 12.0,
        Kind::Pillager => 15.0,
        Kind::Vindicator | Kind::Zoglin => 10.0,
        _ => return None,
    })
}

pub fn sensor_tick(t: &mut Tick<'_, '_>, s: Sensor) {
    match s {
        Sensor::Idle => {}
        Sensor::NearestBed => nearest_bed(t),
        Sensor::Hostiles => {
            // `NearestVisibleLivingEntitySensor`: the closest visible matching one.
            let found = sensing::visible_find_closest(t, |t, id| {
                let Some(d) = t.ents.mob(id).and_then(|m| hostile_distance(m.kind)) else {
                    return false;
                };
                bh::dist_sqr(t, id) <= f64::from(d * d)
            });
            brain(t).set_opt(M::NearestHostile, found.map(MemValue::Entity));
        }
        Sensor::Babies => {
            let mut babies = Vec::new();
            sensing::visible_find_all(
                t,
                |t, id| {
                    t.ents
                        .mob(id)
                        .is_some_and(|m| m.kind == Kind::Villager && m.baby)
                },
                &mut babies,
            );
            brain(t).set(M::VisibleVillagerBabies, MemValue::Entities(babies));
        }
        Sensor::SecondaryPois => {
            let center = t.block_pos();
            let prof = vs(t.mob_ref()).profession;
            let dim: std::sync::Arc<str> = t.env.dimension.into();
            let mut sites = Vec::new();
            for x in -4..=4 {
                for y in -2..=2 {
                    for z in -4..=4 {
                        let p = center.offset(x, y, z);
                        if v::secondary_poi(prof, t.level.block(p)) {
                            sites.push(GlobalPos {
                                dimension: dim.clone(),
                                pos: p,
                            });
                        }
                    }
                }
            }
            if sites.is_empty() {
                brain(t).erase(M::SecondaryJobSite);
            } else {
                brain(t).set(M::SecondaryJobSite, MemValue::GlobalPositions(sites));
            }
        }
        Sensor::Golem => {
            let golem = brain_ref(t)
                .entities(M::NearestLivingEntities)
                .iter()
                .any(|&id| kind_of(t, id) == Some(Kind::IronGolem));
            if golem {
                v::golem_detected(t.mob());
            }
        }
    }
}

/// `NearestBedSensor.doTick`.
fn nearest_bed(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().baby {
        return;
    }
    let now = t.game_time();
    let last = now + i64::from(t.env.random.next_int_bounded(20));
    {
        let vv = vs_mut(t.mob());
        vv.bed_tried = 0;
        vv.bed_last_update = last;
    }
    let (targets, _) = home_candidates(t, true, last);
    let reach = targets
        .iter()
        .map(|&(ty, _)| ty.valid_range())
        .fold(1, i32::max);
    let positions: Vec<BlockPos> = targets.iter().map(|&(_, p)| p).collect();
    let path = crate::mob::nav::create_path_to_any(t, &positions, reach);
    if let Some(p) = path.filter(|p| p.reached) {
        if t.ents.poi.get_type(p.target).is_some() {
            brain(t).set(M::NearestBed, MemValue::Pos(p.target));
        }
    } else if vs(t.mob_ref()).bed_tried < 5 {
        vs_mut(t.mob()).bed_cache.retain(|&(_, at)| at >= last);
    }
}

/// The beds `findAllWithType(HOME, cacheTest, pos, 48, ANY)` of the nearest bed sensor
/// (`sensor`) or `SetClosestHomeAsWalkTarget`, with their batch cache.
fn home_candidates(
    t: &mut Tick<'_, '_>,
    sensor: bool,
    last: i64,
) -> (Vec<(PoiType, BlockPos)>, i32) {
    let center = t.block_pos();
    let records = t
        .ents
        .poi
        .in_range(|ty| ty == PoiType::Home, center, 48, Occupancy::Any);
    let vv = vs_mut(t.mob());
    let mut tried = if sensor { vv.bed_tried } else { 0 };
    let mut out = Vec::new();
    {
        let cache = if sensor {
            &mut vv.bed_cache
        } else {
            &mut vv.home_cache
        };
        for r in records {
            if cache.iter().any(|&(p, _)| p == r.pos) {
                continue;
            }
            tried += 1;
            if tried >= 5 {
                continue;
            }
            cache.push((r.pos, last + 40));
            out.push((r.ty, r.pos));
        }
    }
    if sensor {
        vv.bed_tried = tried;
    }
    (out, tried)
}

// --- Shape, duration, requirements ------------------------------------------------------------

pub fn behavior_duration(b: &Behavior) -> (i32, i32) {
    match b {
        Behavior::FollowTradingPlayer => (i32::MAX, i32::MAX),
        Behavior::GoToPotentialJobSite => (1200, 1200),
        Behavior::ShowTrades { .. } => (400, 1600),
        Behavior::GiveGift { .. } => (100, 100),
        Behavior::MakeLove { .. } => (350, 350),
        Behavior::Celebrate => (600, 600),
        _ => (60, 60),
    }
}

pub fn behavior_shape(b: &Behavior) -> Shape {
    match b {
        Behavior::PanicTrigger
        | Behavior::FollowTradingPlayer
        | Behavior::GoToPotentialJobSite
        | Behavior::WorkAtPoi { .. }
        | Behavior::HarvestFarmland { .. }
        | Behavior::UseBonemeal { .. }
        | Behavior::ShowTrades { .. }
        | Behavior::GiveGift { .. }
        | Behavior::JumpOnBed { .. }
        | Behavior::SleepInBed { .. }
        | Behavior::TradeWithVillager
        | Behavior::MakeLove { .. }
        | Behavior::Celebrate => Shape::Timed,
        _ => Shape::OneShot,
    }
}

pub fn behavior_no_timeout(b: &Behavior) -> bool {
    matches!(
        b,
        Behavior::FollowTradingPlayer | Behavior::JumpOnBed { .. } | Behavior::SleepInBed { .. }
    )
}

const REG: MemoryStatus = S::Registered;
const ABSENT: MemoryStatus = S::ValueAbsent;
const PRESENT: MemoryStatus = S::ValuePresent;

pub fn behavior_requirements(b: &Behavior) -> &'static [(M, MemoryStatus)] {
    match b {
        Behavior::Idle
        | Behavior::PanicTrigger
        | Behavior::WakeUp
        | Behavior::SetRaidStatus
        | Behavior::UpdateSchedule
        | Behavior::Celebrate
        | Behavior::ResetRaidStatus => &[],
        Behavior::Door { .. } => &[
            (M::Path, PRESENT),
            (M::DoorsToClose, REG),
            (M::NearestLivingEntities, REG),
        ],
        Behavior::ReactToBell => &[(M::HeardBellTime, PRESENT)],
        Behavior::ValidatePoi { memory } => present(*memory),
        Behavior::PoiCompetitorScan => {
            &[(M::JobSite, PRESENT), (M::NearestLivingEntities, PRESENT)]
        }
        Behavior::FollowTradingPlayer => &[(M::WalkTarget, REG), (M::LookTarget, REG)],
        Behavior::AcquirePoi { which, .. } => match which {
            Acquire::Job => &[(M::JobSite, ABSENT), (M::PotentialJobSite, ABSENT)],
            Acquire::Home => &[(M::Home, ABSENT)],
            Acquire::Meeting => &[(M::MeetingPoint, ABSENT)],
        },
        Behavior::GoToPotentialJobSite => &[(M::PotentialJobSite, PRESENT)],
        Behavior::YieldJobSite => &[
            (M::PotentialJobSite, PRESENT),
            (M::JobSite, ABSENT),
            (M::NearestLivingEntities, PRESENT),
            (M::WalkTarget, REG),
            (M::LookTarget, REG),
        ],
        Behavior::AssignProfession => &[(M::PotentialJobSite, PRESENT), (M::JobSite, REG)],
        Behavior::ResetProfession => &[(M::JobSite, ABSENT)],
        Behavior::WorkAtPoi { .. } => &[(M::JobSite, PRESENT), (M::LookTarget, REG)],
        Behavior::StrollToPoiList { .. } => &[
            (M::WalkTarget, REG),
            (M::SecondaryJobSite, PRESENT),
            (M::JobSite, PRESENT),
        ],
        Behavior::HarvestFarmland { .. } => &[
            (M::LookTarget, ABSENT),
            (M::WalkTarget, ABSENT),
            (M::SecondaryJobSite, PRESENT),
        ],
        Behavior::UseBonemeal { .. } => &[(M::LookTarget, ABSENT), (M::WalkTarget, ABSENT)],
        Behavior::ShowTrades { .. } => &[(M::InteractionTarget, PRESENT)],
        Behavior::WalkToMemory { memory, .. } => match memory {
            M::Home => &[
                (M::CantReachWalkTargetSince, REG),
                (M::WalkTarget, ABSENT),
                (M::Home, PRESENT),
            ],
            M::MeetingPoint => &[
                (M::CantReachWalkTargetSince, REG),
                (M::WalkTarget, ABSENT),
                (M::MeetingPoint, PRESENT),
            ],
            _ => &[
                (M::CantReachWalkTargetSince, REG),
                (M::WalkTarget, ABSENT),
                (M::JobSite, PRESENT),
            ],
        },
        Behavior::GiveGift { .. } => &[
            (M::WalkTarget, REG),
            (M::LookTarget, REG),
            (M::InteractionTarget, REG),
            (M::NearestVisiblePlayer, PRESENT),
        ],
        Behavior::PlayTag => &[
            (M::VisibleVillagerBabies, PRESENT),
            (M::WalkTarget, ABSENT),
            (M::LookTarget, REG),
            (M::InteractionTarget, REG),
        ],
        Behavior::VillageStroll { .. }
        | Behavior::InsideBrownian
        | Behavior::ClosestVillage
        | Behavior::SkySeeingSpot { .. } => &[(M::WalkTarget, ABSENT)],
        Behavior::JumpOnBed { .. } => &[(M::NearestBed, PRESENT), (M::WalkTarget, ABSENT)],
        Behavior::SleepInBed { .. } => &[
            (M::Home, PRESENT),
            (M::LastWoken, REG),
            (M::LastSlept, REG),
            (M::WalkTarget, REG),
            (M::CantReachWalkTargetSince, REG),
        ],
        Behavior::ClosestHome => &[(M::WalkTarget, ABSENT), (M::Home, ABSENT)],
        Behavior::SocializeAtBell => &[
            (M::WalkTarget, REG),
            (M::LookTarget, REG),
            (M::MeetingPoint, PRESENT),
            (M::NearestVisibleLivingEntities, PRESENT),
            (M::InteractionTarget, ABSENT),
        ],
        Behavior::TradeWithVillager => &[
            (M::InteractionTarget, PRESENT),
            (M::NearestVisibleLivingEntities, PRESENT),
        ],
        Behavior::MakeLove { .. } => &[
            (M::BreedTarget, PRESENT),
            (M::NearestVisibleLivingEntities, PRESENT),
        ],
        Behavior::CalmDown => &[
            (M::HurtBy, REG),
            (M::HurtByEntity, REG),
            (M::NearestHostile, REG),
        ],
        Behavior::RingBell => &[(M::MeetingPoint, PRESENT)],
        Behavior::LocateHiding { .. } => &[
            (M::WalkTarget, ABSENT),
            (M::Home, REG),
            (M::HidingPlace, REG),
            (M::Path, REG),
            (M::LookTarget, REG),
            (M::BreedTarget, REG),
            (M::InteractionTarget, REG),
        ],
        Behavior::HiddenState { .. } => &[(M::HidingPlace, PRESENT), (M::HeardBellTime, PRESENT)],
        Behavior::InteractBreed => &[
            (M::BreedTarget, REG),
            (M::LookTarget, REG),
            (M::WalkTarget, ABSENT),
            (M::NearestVisibleLivingEntities, PRESENT),
        ],
    }
}

// --- Helpers --------------------------------------------------------------------------------

fn global_pos(t: &Tick<'_, '_>, memory: M) -> Option<GlobalPos> {
    match brain_ref(t).get(memory) {
        Some(MemValue::GlobalPos(g)) => Some(g.clone()),
        _ => None,
    }
}

fn here(t: &Tick<'_, '_>, pos: BlockPos) -> GlobalPos {
    GlobalPos {
        dimension: t.env.dimension.into(),
        pos,
    }
}

fn same_dimension(t: &Tick<'_, '_>, g: &GlobalPos) -> bool {
    *g.dimension == *t.env.dimension
}

/// `BlockPos.closerToCenterThan(position, distance)`.
fn closer_to_center(pos: BlockPos, v: Vec3, d: f64) -> bool {
    let c = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    c.distance_to_sqr(v) < d * d
}

/// `Vec3i.closerThan(other, distance)`.
fn closer_than_block(a: BlockPos, b: BlockPos, d: f64) -> bool {
    a.dist_sqr(b) < d * d
}

fn manhattan(a: BlockPos, b: BlockPos) -> i32 {
    (a.x - b.x).abs() + (a.y - b.y).abs() + (a.z - b.z).abs()
}

fn prof(t: &Tick<'_, '_>) -> u16 {
    vs(t.mob_ref()).profession
}

/// The POI predicate of a validated memory.
fn memory_poi(t: &Tick<'_, '_>, memory: M, ty: PoiType) -> bool {
    match memory {
        M::JobSite => v::held_job_site(prof(t), ty),
        M::PotentialJobSite => v::acquirable_job_site(prof(t), ty),
        M::Home => ty == PoiType::Home,
        M::MeetingPoint => ty == PoiType::Meeting,
        _ => false,
    }
}

/// `#mob_interactable_doors`: wooden and copper doors.
fn is_mob_door(state: BlockState) -> bool {
    let n = state.block().name();
    n.ends_with("_door") && n != "minecraft:iron_door"
}

fn is_open(state: BlockState) -> bool {
    state.flag("open")
}

fn set_door(t: &mut Tick<'_, '_>, pos: BlockPos, open: bool) {
    let id = t.id;
    t.cx.actions
        .push(WorldAction::SetDoorOpen(pos, open, Some(id)));
}

fn remember_door(t: &mut Tick<'_, '_>, pos: BlockPos) {
    let g = here(t, pos);
    let mut doors = match brain_ref(t).get(M::DoorsToClose) {
        Some(MemValue::GlobalPositions(v)) => v.clone(),
        _ => Vec::new(),
    };
    if !doors.contains(&g) {
        doors.push(g);
    }
    brain(t).set(M::DoorsToClose, MemValue::GlobalPositions(doors));
}

/// The path's previous and next node (`getPreviousNode`, `getNextNode`), if it is running.
fn path_nodes(t: &Tick<'_, '_>, id: EntityId) -> Option<(Option<BlockPos>, BlockPos)> {
    let path = t.ents.mob(id)?.nav.path.as_ref()?;
    if path.is_done() {
        return None;
    }
    let next = path.nodes.get(path.next)?.pos();
    let prev = path
        .next
        .checked_sub(1)
        .and_then(|i| path.nodes.get(i))
        .map(|n| n.pos());
    Some((prev, next))
}

/// `InteractWithDoor.closeDoorsThatIHaveOpenedOrPassedThrough`.
pub fn close_doors(t: &mut Tick<'_, '_>, from: Option<BlockPos>, to: Option<BlockPos>) {
    let doors = match brain_ref(t).get(M::DoorsToClose) {
        Some(MemValue::GlobalPositions(v)) => v.clone(),
        _ => return,
    };
    let mut keep = Vec::new();
    for g in doors {
        let pos = g.pos;
        if from == Some(pos) || to == Some(pos) {
            keep.push(g);
            continue;
        }
        if !same_dimension(t, &g) || !closer_to_center(pos, t.pos(), 3.0) {
            continue;
        }
        let state = t.level.block(pos);
        if !is_mob_door(state) || !is_open(state) {
            continue;
        }
        if others_coming_through(t, pos) {
            continue;
        }
        set_door(t, pos, false);
    }
    brain(t).set(M::DoorsToClose, MemValue::GlobalPositions(keep));
}

/// `areOtherMobsComingThroughDoor`.
fn others_coming_through(t: &Tick<'_, '_>, door: BlockPos) -> bool {
    let me = t.mob_ref().kind;
    brain_ref(t)
        .entities(M::NearestLivingEntities)
        .iter()
        .filter(|&&id| kind_of(t, id) == Some(me))
        .filter(|&&id| bh::entity_pos(t, id).is_some_and(|p| closer_to_center(door, p, 2.0)))
        .any(|&id| {
            let has_path = t
                .ents
                .mob(id)
                .and_then(|m| m.brain.as_deref())
                .is_some_and(|b| b.has(M::Path));
            has_path
                && path_nodes(t, id).is_some_and(|(prev, next)| {
                    prev.is_some() && (prev == Some(door) || next == door)
                })
        })
}

fn villager_brain_has(t: &Tick<'_, '_>, id: EntityId, m: M) -> bool {
    t.ents
        .mob(id)
        .and_then(|o| o.brain.as_deref())
        .is_some_and(|b| b.has(m))
}

fn other_global(t: &Tick<'_, '_>, id: EntityId, m: M) -> Option<GlobalPos> {
    match t.ents.mob(id)?.brain.as_deref()?.get(m) {
        Some(MemValue::GlobalPos(g)) => Some(g.clone()),
        _ => None,
    }
}

/// `level.isVillage(pos)`.
fn is_village(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    t.ents.poi.is_village(pos)
}

fn sections_to_village(t: &Tick<'_, '_>, pos: BlockPos) -> i32 {
    t.ents.poi.sections_to_village(pos.section())
}

/// `BehaviorUtils.findSectionClosestToVillage(level, center, radius)`.
fn section_closest_to_village(t: &Tick<'_, '_>, center: SectionPos, radius: i32) -> SectionPos {
    let mut best = center;
    let mut best_d = t.ents.poi.sections_to_village(center);
    // `SectionPos.cube(center, radius)`: x fastest, then y, then z.
    for z in -radius..=radius {
        for y in -radius..=radius {
            for x in -radius..=radius {
                let s = SectionPos::new(center.x + x, center.y + y, center.z + z);
                let d = t.ents.poi.sections_to_village(s);
                if d < best_d {
                    best = s;
                    best_d = d;
                }
            }
        }
    }
    best
}

/// `canSeeSky(pos)`.
fn can_see_sky(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    crate::mob::can_see_sky(t.level, pos)
}

/// `MoveToSkySeeingSpot.hasNoBlocksAbove`.
fn no_blocks_above(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    can_see_sky(t, pos)
        && f64::from(
            t.level
                .heightmap_y(vanillite_chunk::HeightmapKind::MotionBlocking, pos),
        ) <= t.pos().y
}

/// `BehaviorUtils.targetIsValid(brain, memory, VILLAGER)`.
fn target_is_villager(t: &Tick<'_, '_>, memory: M) -> Option<EntityId> {
    let id = brain_ref(t).entity(memory)?;
    (is_villager(t, id) && bh::is_alive(t, id)).then_some(id)
}

/// `CropBlock.getMaxAge` of a crop block.
fn crop_max_age(state: BlockState) -> Option<i32> {
    match state.block().name() {
        "minecraft:wheat" | "minecraft:carrots" | "minecraft:potatoes" => Some(7),
        "minecraft:beetroots" => Some(3),
        "minecraft:torchflower_crop" => Some(2),
        _ => None,
    }
}

fn crop_at_max(state: BlockState) -> bool {
    crop_max_age(state).is_some_and(|m| state.int("age").unwrap_or(0) >= m)
}

fn crop_growing(state: BlockState) -> bool {
    crop_max_age(state).is_some_and(|m| state.int("age").unwrap_or(0) < m)
}

fn is_farmland(state: BlockState) -> bool {
    state.block().name() == "minecraft:farmland"
}

/// The crop block a plantable seed places.
fn seed_block(item: u16) -> Option<&'static str> {
    Some(match item {
        i if i == items::WHEAT_SEEDS.0 => "minecraft:wheat",
        i if i == items::POTATO.0 => "minecraft:potatoes",
        i if i == items::CARROT.0 => "minecraft:carrots",
        i if i == items::BEETROOT_SEEDS.0 => "minecraft:beetroots",
        i if i == items::TORCHFLOWER_SEEDS.0 => "minecraft:torchflower_crop",
        i if i == items::PITCHER_POD.0 => "minecraft:pitcher_crop",
        _ => return None,
    })
}

fn play_block_sound(t: &mut Tick<'_, '_>, pos: BlockPos, sound: &str, corner: bool) {
    let at = if corner {
        Vec3::new(f64::from(pos.x), f64::from(pos.y), f64::from(pos.z))
    } else {
        randompos::bottom_center(pos) + Vec3::new(0.0, 0.5, 0.0)
    };
    let seed = crate::survival::sound_seed();
    crate::survival::play_sound(
        t.env.out,
        None,
        at,
        sound,
        SoundSource::Blocks,
        1.0,
        1.0,
        seed,
    );
}

// --- Checks of timed behaviours ---------------------------------------------------------------

pub fn behavior_check(t: &mut Tick<'_, '_>, b: &mut Behavior) -> bool {
    match b {
        Behavior::FollowTradingPlayer => follow_ok(t),
        Behavior::GoToPotentialJobSite => brain_ref(t)
            .active_non_core()
            .is_none_or(|a| matches!(a, Activity::Idle | Activity::Work | Activity::Play)),
        Behavior::WorkAtPoi { last_check, .. } => {
            let now = t.game_time();
            if now - *last_check < 300 {
                return false;
            }
            if t.env.random.next_int_bounded(2) != 0 {
                return false;
            }
            *last_check = now;
            near_job_site(t)
        }
        Behavior::HarvestFarmland { target, .. } => {
            if !t.env.rules.bool(crate::survival::rules::Rule::MOB_GRIEFING) {
                return false;
            }
            if prof(t) != profession::FARMER {
                return false;
            }
            let p = t.pos();
            let mut valid = Vec::new();
            for x in -1..=1 {
                for y in -1..=1 {
                    for z in -1..=1 {
                        let pos = BlockPos::containing(
                            p.x + f64::from(x),
                            p.y + f64::from(y),
                            p.z + f64::from(z),
                        );
                        if valid_farm_pos(t, pos) {
                            valid.push(pos);
                        }
                    }
                }
            }
            vs_mut(t.mob()).farmland = valid;
            *target = pick_farmland(t);
            target.is_some()
        }
        Behavior::UseBonemeal {
            crop, last_session, ..
        } => {
            let tick_count = t.mob_ref().tick_count;
            if tick_count % 10 != 0 || !(*last_session == 0 || *last_session + 160 <= tick_count) {
                return false;
            }
            if v::count_item(&state(t.mob_ref()).merchant.inventory, items::BONE_MEAL.0) <= 0 {
                return false;
            }
            *crop = pick_crop(t);
            crop.is_some()
        }
        Behavior::ShowTrades { .. } => show_trades_ok(t),
        Behavior::GiveGift { until_next, .. } => {
            if nearest_hero(t).is_none() {
                return false;
            }
            if *until_next > 0 {
                *until_next -= 1;
                return false;
            }
            true
        }
        Behavior::JumpOnBed { .. } => {
            t.mob_ref().baby && (on_or_over_bed(t) || brain_ref(t).pos(M::NearestBed).is_some())
        }
        Behavior::SleepInBed { .. } => sleep_ok(t),
        Behavior::TradeWithVillager => target_is_villager(t, M::InteractionTarget).is_some(),
        Behavior::MakeLove { .. } => breeding_possible(t),
        Behavior::Celebrate => {
            let pos = t.block_pos();
            let victory = t.ents.raids.raid_at(pos).is_some_and(|r| r.is_victory());
            victory && no_blocks_above(t, pos)
        }
        _ => true,
    }
}

fn follow_ok(t: &Tick<'_, '_>) -> bool {
    let m = t.mob_ref();
    let Some(player) = state(m).merchant.trading_player else {
        return false;
    };
    m.is_alive()
        && !m.fluids.in_water()
        && !(m.last_hurt_by_mob_time > 0 && m.tick_count - m.last_hurt_by_mob_time < 40)
        && bh::dist_sqr(t, player) <= 16.0
}

fn near_job_site(t: &Tick<'_, '_>) -> bool {
    global_pos(t, M::JobSite)
        .is_some_and(|g| same_dimension(t, &g) && closer_to_center(g.pos, t.pos(), 1.73))
}

fn valid_farm_pos(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    let state = t.level.block(pos);
    let below = t.level.block(pos.offset(0, -1, 0));
    crop_at_max(state) || (state.is_air() && is_farmland(below))
}

fn pick_farmland(t: &mut Tick<'_, '_>) -> Option<BlockPos> {
    let n = vs(t.mob_ref()).farmland.len();
    if n == 0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let i = t.env.random.next_int_bounded(n as i32);
    vs(t.mob_ref())
        .farmland
        .get(usize::try_from(i).unwrap_or(0))
        .copied()
}

/// `UseBonemeal.pickNextTarget`.
fn pick_crop(t: &mut Tick<'_, '_>) -> Option<BlockPos> {
    let center = t.block_pos();
    let mut result = None;
    let mut count = 0;
    for x in -1..=1 {
        for y in -1..=1 {
            for z in -1..=1 {
                let p = center.offset(x, y, z);
                if crop_growing(t.level.block(p)) {
                    count += 1;
                    if t.env.random.next_int_bounded(count) == 0 {
                        result = Some(p);
                    }
                }
            }
        }
    }
    result
}

fn show_trades_ok(t: &Tick<'_, '_>) -> bool {
    let Some(target) = brain_ref(t).entity(M::InteractionTarget) else {
        return false;
    };
    is_player(t, target)
        && t.mob_ref().is_alive()
        && bh::is_alive(t, target)
        && !t.mob_ref().baby
        && bh::dist_sqr(t, target) <= 17.0
}

/// `GiveGiftToHero.getNearestTargetableHero`.
fn nearest_hero(t: &Tick<'_, '_>) -> Option<EntityId> {
    let p = brain_ref(t).entity(M::NearestVisiblePlayer)?;
    t.ents
        .player(p)
        .is_some_and(|d| d.survival.effects.has(MobEffect::HERO_OF_THE_VILLAGE))
        .then_some(p)
}

fn is_jumpable(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    v::is_villager_bed(t.level.block(pos))
}

fn on_or_over_bed(t: &Tick<'_, '_>) -> bool {
    let p = t.block_pos();
    is_jumpable(t, p) || is_jumpable(t, p.offset(0, -1, 0))
}

fn sleep_ok(t: &Tick<'_, '_>) -> bool {
    if t.ents.riding.vehicle_of(t.id).is_some() {
        return false;
    }
    let Some(home) = global_pos(t, M::Home) else {
        return false;
    };
    if !same_dimension(t, &home) {
        return false;
    }
    if let Some(woken) = brain_ref(t).long(M::LastWoken) {
        let since = t.game_time() - woken;
        if since > 0 && since < 100 {
            return false;
        }
    }
    let state = t.level.block(home.pos);
    closer_to_center(home.pos, t.pos(), 2.0) && v::is_villager_bed(state) && !state.flag("occupied")
}

fn breeding_possible(t: &Tick<'_, '_>) -> bool {
    let Some(target) = target_is_villager(t, M::BreedTarget) else {
        return false;
    };
    v::can_breed(t.mob_ref()) && t.ents.mob(target).is_some_and(v::can_breed)
}

pub fn behavior_can_still_use(t: &mut Tick<'_, '_>, b: &mut Behavior, now: i64) -> bool {
    match b {
        Behavior::PanicTrigger => {
            brain_ref(t).has(M::HurtBy) || brain_ref(t).has(M::NearestHostile)
        }
        Behavior::FollowTradingPlayer => follow_ok(t),
        Behavior::GoToPotentialJobSite => brain_ref(t).has(M::PotentialJobSite),
        Behavior::WorkAtPoi { .. } => near_job_site(t),
        Behavior::HarvestFarmland { worked, .. } => *worked < 200,
        Behavior::UseBonemeal { worked, crop, .. } => *worked < 80 && crop.is_some(),
        Behavior::ShowTrades { look_time, .. } => {
            show_trades_ok(t) && *look_time > 0 && brain_ref(t).has(M::InteractionTarget)
        }
        Behavior::GiveGift { given, .. } => nearest_hero(t).is_some() && !*given,
        Behavior::JumpOnBed {
            bed,
            time_to_reach,
            jumps,
            ..
        } => {
            let over = on_or_over_bed(t);
            t.mob_ref().baby
                && bed.is_some_and(|b| is_jumpable(t, b))
                && !(!over && *time_to_reach <= 0)
                && !(over && *jumps <= 0)
        }
        Behavior::SleepInBed { .. } => {
            let Some(home) = global_pos(t, M::Home) else {
                return false;
            };
            brain_ref(t).is_active(Activity::Rest)
                && t.pos().y > f64::from(home.pos.y) + 0.4
                && closer_to_center(home.pos, t.pos(), 1.14)
        }
        Behavior::TradeWithVillager => target_is_villager(t, M::InteractionTarget).is_some(),
        Behavior::MakeLove { birth } => now <= *birth && breeding_possible(t),
        Behavior::Celebrate => {
            let pos = t.block_pos();
            t.ents.raids.raid_at(pos).is_some_and(|r| !r.is_stopped())
        }
        _ => false,
    }
}

// --- Start, tick, stop ------------------------------------------------------------------------

pub fn behavior_start(t: &mut Tick<'_, '_>, b: &mut Behavior, now: i64) {
    match b {
        Behavior::PanicTrigger => {
            if brain_ref(t).has(M::HurtBy) || brain_ref(t).has(M::NearestHostile) {
                let br = brain(t);
                if !br.is_active(Activity::Panic) {
                    for m in [
                        M::Path,
                        M::WalkTarget,
                        M::LookTarget,
                        M::BreedTarget,
                        M::InteractionTarget,
                    ] {
                        br.erase(m);
                    }
                }
                br.set_active_activity_if_possible(Activity::Panic);
            }
        }
        Behavior::FollowTradingPlayer => follow_player(t),
        Behavior::WorkAtPoi { composter, .. } => {
            let job = global_pos(t, M::JobSite);
            let br = brain(t);
            br.set(M::LastWorkedAtPoi, MemValue::Long(now));
            if let Some(g) = &job {
                br.set(M::LookTarget, MemValue::Look(Tracker::block(g.pos)));
            }
            v::play_work_sound(t);
            if *composter && let Some(g) = job {
                use_composter(t, g.pos);
            }
            if super::merchant::should_restock(t) {
                super::merchant::restock(t);
            }
        }
        Behavior::HarvestFarmland {
            target, next_ok, ..
        } => {
            if now > *next_ok
                && let Some(p) = *target
            {
                let br = brain(t);
                br.set(M::LookTarget, MemValue::Look(Tracker::block(p)));
                br.set(M::WalkTarget, MemValue::Walk(WalkTarget::block(p, 0.5, 1)));
            }
        }
        Behavior::UseBonemeal {
            crop,
            next_cycle,
            worked,
            ..
        } => {
            target_crop(t, *crop);
            let meal = ItemStack::new(items::BONE_MEAL.0, 1);
            t.mob().equipment.set(EquipmentSlot::MainHand, meal);
            t.core_mut().mark_data_dirty();
            *next_cycle = now;
            *worked = 0;
        }
        Behavior::ShowTrades {
            cycle,
            index,
            look_time,
        } => {
            look_at_interaction_target(t);
            *cycle = 0;
            *index = 0;
            *look_time = 40;
        }
        Behavior::GiveGift { given, since, .. } => {
            *given = false;
            *since = now;
            if let Some(hero) = nearest_hero(t) {
                let br = brain(t);
                br.set(M::InteractionTarget, MemValue::Entity(hero));
                br.set(M::LookTarget, MemValue::Look(Tracker::entity(hero, true)));
            }
        }
        Behavior::JumpOnBed {
            bed,
            time_to_reach,
            jumps,
            cooldown,
        } => {
            if let Some(p) = brain_ref(t).pos(M::NearestBed) {
                *bed = Some(p);
                *time_to_reach = 100;
                *jumps = 3 + t.env.random.next_int_bounded(4);
                *cooldown = 0;
                brain(t).set(
                    M::WalkTarget,
                    MemValue::Walk(WalkTarget::block(p, SPEED, 0)),
                );
            }
        }
        Behavior::SleepInBed { next_ok } => {
            if now > *next_ok {
                if brain_ref(t).has(M::DoorsToClose) {
                    close_doors(t, None, None);
                }
                if let Some(home) = global_pos(t, M::Home)
                    && v::start_sleeping(t, home.pos)
                {
                    brain(t).set(M::LastSlept, MemValue::Long(now));
                }
                let br = brain(t);
                br.erase(M::WalkTarget);
                br.erase(M::CantReachWalkTargetSince);
            }
        }
        Behavior::TradeWithVillager => {
            let Some(target) = brain_ref(t).entity(M::InteractionTarget) else {
                return;
            };
            bh::lock_gaze_and_walk_to_each_other(t, target, 0.5, 2);
            let theirs = t
                .ents
                .mob(target)
                .map_or(profession::NONE, |m| vs(m).profession);
            let mine = v::requested_items(prof(t));
            let trades = v::requested_items(theirs)
                .iter()
                .copied()
                .filter(|i| !mine.contains(i))
                .collect();
            vs_mut(t.mob()).willing_trades = trades;
        }
        Behavior::MakeLove { birth } => {
            let Some(target) = brain_ref(t).entity(M::BreedTarget) else {
                return;
            };
            bh::lock_gaze_and_walk_to_each_other(t, target, 0.5, 2);
            v::entity_event(t, target, 18);
            let me = t.id;
            v::entity_event(t, me, 18);
            let d = 275 + t.mob().random.next_int_bounded(50);
            *birth = now + i64::from(d);
        }
        _ => {}
    }
}

fn follow_player(t: &mut Tick<'_, '_>) {
    let Some(p) = state(t.mob_ref()).merchant.trading_player else {
        return;
    };
    let br = brain(t);
    br.set(
        M::WalkTarget,
        MemValue::Walk(WalkTarget::entity(p, SPEED, 2)),
    );
    br.set(M::LookTarget, MemValue::Look(Tracker::entity(p, true)));
}

fn target_crop(t: &mut Tick<'_, '_>, crop: Option<BlockPos>) {
    if let Some(p) = crop {
        let br = brain(t);
        br.set(M::LookTarget, MemValue::Look(Tracker::block(p)));
        br.set(M::WalkTarget, MemValue::Walk(WalkTarget::block(p, 0.5, 1)));
    }
}

fn look_at_interaction_target(t: &mut Tick<'_, '_>) -> Option<EntityId> {
    let target = brain_ref(t).entity(M::InteractionTarget)?;
    brain(t).set(M::LookTarget, MemValue::Look(Tracker::entity(target, true)));
    Some(target)
}

fn set_held(t: &mut Tick<'_, '_>, stack: ItemStack, drop_chance: f32) {
    let m = t.mob();
    m.equipment.set(EquipmentSlot::MainHand, stack);
    m.equipment.drop_chances[usize::from(EquipmentSlot::MainHand.ordinal())] = drop_chance;
    t.core_mut().mark_data_dirty();
}

/// `WorkAtComposter.useWorkstation`: bread from wheat, then seeds into the composter.
fn use_composter(t: &mut Tick<'_, '_>, pos: BlockPos) {
    let mut state = t.level.block(pos);
    if state.block().name() != "minecraft:composter" {
        return;
    }
    // `makeBread`.
    {
        let inv = &mut state_mut(t.mob()).merchant.inventory;
        if v::count_item(inv, items::BREAD.0) <= 36 {
            let wheat = v::count_item(inv, items::WHEAT.0);
            let bread = (wheat / 3).min(3);
            if bread != 0 {
                v::remove_item_type(inv, items::WHEAT.0, bread * 3);
                let rest = v::add_item(inv, &ItemStack::new(items::BREAD.0, bread));
                if !rest.is_empty() {
                    let at = t.pos() + Vec3::new(0.0, 0.5, 0.0);
                    crate::mob::fam::undead::drop_item(t.env, at, rest);
                }
            }
        }
    }
    let original = state;
    if state.int("level").unwrap_or(0) == 8 {
        // `ComposterBlock.extractProduce`.
        let r = &mut *t.env.random;
        let (ox, oz) = (
            f64::from(r.next_float() * 0.7 - 0.35),
            f64::from(r.next_float() * 0.7 - 0.35),
        );
        let at = Vec3::new(
            f64::from(pos.x) + 0.5 + ox,
            f64::from(pos.y) + 1.01,
            f64::from(pos.z) + 0.5 + oz,
        );
        crate::mob::fam::undead::drop_item(t.env, at, ItemStack::new(items::BONE_MEAL.0, 1));
        state = state.set_int("level", 0);
        t.cx.actions
            .push(WorldAction::SetBlock(pos, state, crate::level::update::ALL));
        play_block_sound(t, pos, "minecraft:block.composter.empty", false);
    }
    const COMPOSTABLE: [u16; 2] = [items::WHEAT_SEEDS.0, items::BEETROOT_SEEDS.0];
    let mut total = 20;
    let mut seen = [0i32; 2];
    let mut temp = state;
    let mut done = false;
    for i in (0..8).rev() {
        if total <= 0 || done {
            break;
        }
        let stack = state_mut(t.mob()).merchant.inventory[i].clone();
        let Some(idx) = COMPOSTABLE
            .iter()
            .position(|&c| c == stack.item && !stack.is_empty())
        else {
            continue;
        };
        let size = stack.count;
        let count = seen[idx] + size;
        seen[idx] = count;
        let use_items = (count - 10).min(total).min(size);
        if use_items <= 0 {
            continue;
        }
        total -= use_items;
        for _ in 0..use_items {
            // `insertItem`: `compostable/low`: a layer always on an empty composter, else 30 %.
            let level = temp.int("level").unwrap_or(0);
            if level < 7 {
                let add = if level == 0 {
                    1
                } else {
                    i32::from(t.env.random.next_int_bounded(100) < 30)
                };
                if add > 0 {
                    temp = temp.set_int("level", (level + add).clamp(0, 7));
                    t.cx.actions
                        .push(WorldAction::SetBlock(pos, temp, crate::level::update::ALL));
                }
                state_mut(t.mob()).merchant.inventory[i].shrink(1);
            }
            if temp.int("level").unwrap_or(0) == 7 {
                done = true;
                break;
            }
        }
    }
    t.cx.actions.push(WorldAction::LevelEvent(
        1500,
        pos,
        i32::from(temp != original),
    ));
}

pub fn behavior_tick(t: &mut Tick<'_, '_>, b: &mut Behavior, now: i64) {
    match b {
        Behavior::PanicTrigger => {
            if now % 100 == 0 {
                v::spawn_golem_if_needed(t, now, 3);
            }
        }
        Behavior::FollowTradingPlayer => follow_player(t),
        Behavior::GoToPotentialJobSite => {
            if let Some(g) = global_pos(t, M::PotentialJobSite) {
                bh::set_walk_and_look(t, Tracker::block(g.pos), SPEED, 1);
            }
        }
        Behavior::HarvestFarmland {
            target,
            next_ok,
            worked,
        } => harvest_tick(t, target, next_ok, worked, now),
        Behavior::UseBonemeal {
            crop,
            next_cycle,
            worked,
            ..
        } => {
            let Some(p) = *crop else {
                return;
            };
            if now >= *next_cycle && closer_to_center(p, t.pos(), 1.0) {
                let has =
                    v::count_item(&state(t.mob_ref()).merchant.inventory, items::BONE_MEAL.0) > 0;
                let valid = crop_growing(t.level.block(p));
                if has && valid {
                    v::remove_item_type(
                        &mut state_mut(t.mob()).merchant.inventory,
                        items::BONE_MEAL.0,
                        1,
                    );
                    t.cx.actions.push(WorldAction::BoneMeal(p));
                    t.cx.actions.push(WorldAction::LevelEvent(1505, p, 15));
                    *crop = pick_crop(t);
                    target_crop(t, *crop);
                    *next_cycle = now + 40;
                }
                *worked += 1;
            }
        }
        Behavior::ShowTrades {
            cycle,
            index,
            look_time,
        } => {
            let Some(target) = look_at_interaction_target(t) else {
                return;
            };
            find_items_to_display(t, target, look_time);
            let n = vs(t.mob_ref()).display_items.len();
            if n == 0 {
                set_held(t, ItemStack::EMPTY, 0.085);
                *look_time = (*look_time).min(40);
            } else if n >= 2 {
                *cycle += 1;
                if *cycle >= 40 {
                    *index += 1;
                    *cycle = 0;
                    if usize::try_from(*index).unwrap_or(0) > n - 1 {
                        *index = 0;
                    }
                    let item =
                        vs(t.mob_ref()).display_items[usize::try_from(*index).unwrap_or(0)].clone();
                    set_held(t, item, 0.0);
                }
            }
            *look_time -= 1;
        }
        Behavior::GiveGift { given, since, .. } => {
            let Some(hero) = nearest_hero(t) else {
                return;
            };
            brain(t).set(M::LookTarget, MemValue::Look(Tracker::entity(hero, true)));
            let near = t.ents.store.get(hero).is_some_and(|e| {
                let p = e.pos();
                closer_than_block(t.block_pos(), BlockPos::containing(p.x, p.y, p.z), 5.0)
            });
            if near {
                if now - *since > 20 {
                    throw_gift(t, hero);
                    *given = true;
                }
            } else {
                bh::set_walk_and_look(t, Tracker::entity(hero, false), 0.5, 5);
            }
        }
        Behavior::JumpOnBed {
            time_to_reach,
            jumps,
            cooldown,
            ..
        } => {
            if !on_or_over_bed(t) {
                *time_to_reach -= 1;
            } else if *cooldown > 0 {
                *cooldown -= 1;
            } else if is_jumpable(t, t.block_pos()) {
                t.mob().jump = true;
                *jumps -= 1;
                *cooldown = 5;
            }
        }
        Behavior::TradeWithVillager => trade_tick(t, now),
        Behavior::MakeLove { birth } => {
            let Some(target) = brain_ref(t).entity(M::BreedTarget) else {
                return;
            };
            if bh::dist_sqr(t, target) > 5.0 {
                return;
            }
            bh::lock_gaze_and_walk_to_each_other(t, target, 0.5, 2);
            if now >= *birth {
                v::eat_and_digest(t.mob());
                if let Some(m) = t.ents.mob_mut(target) {
                    v::eat_and_digest(m);
                }
                give_birth(t, target);
            } else if t.mob().random.next_int_bounded(35) == 0 {
                v::entity_event(t, target, 12);
                let me = t.id;
                v::entity_event(t, me, 12);
            }
        }
        Behavior::Celebrate => {
            if t.mob().random.next_int_bounded(100) == 0 {
                v::make_sound(t, "minecraft:entity.villager.celebrate");
            }
            if t.mob().random.next_int_bounded(200) == 0 && no_blocks_above(t, t.block_pos()) {
                // A rocket of a random dye colour and flight duration from the eyes.
                let r = &mut t.mob().random;
                let color = r.next_int_bounded(16);
                let flight = r.next_int_bounded(3);
                let stack = crate::recipe::celebration_rocket(color, flight);
                let p = t.pos();
                let at = vanillite_base::Vec3::new(p.x, t.eye_y(), p.z);
                let me = t.id;
                crate::firework::spawn_rocket(
                    t.ents,
                    t.level.network_ids(),
                    Some(me),
                    at,
                    &stack,
                    None,
                    None,
                );
            }
        }
        _ => {}
    }
}

/// `HarvestFarmland.tick`.
fn harvest_tick(
    t: &mut Tick<'_, '_>,
    target: &mut Option<BlockPos>,
    next_ok: &mut i64,
    worked: &mut i32,
    now: i64,
) {
    if target.is_some_and(|p| !closer_to_center(p, t.pos(), 1.0)) {
        return;
    }
    if let Some(p) = *target
        && now > *next_ok
    {
        let block = t.level.block(p);
        let below = t.level.block(p.offset(0, -1, 0));
        if crop_at_max(block) {
            t.cx.actions.push(WorldAction::DestroyBlock(p, true));
        }
        if block.is_air() && is_farmland(below) && v::has_farm_seeds(t.mob_ref()) {
            for i in 0..8 {
                let stack = state(t.mob_ref()).merchant.inventory[i].clone();
                if stack.is_empty() {
                    continue;
                }
                let plantable = crate::player::play::game_data().is_some_and(|d| {
                    d.item_in_tag(stack.item, "minecraft:villager_plantable_seeds")
                });
                let Some(block) = seed_block(stack.item).filter(|_| plantable) else {
                    continue;
                };
                let Some(place) =
                    vanillite_data::block::BlockId::by_name(block).map(|b| b.default_state())
                else {
                    continue;
                };
                t.cx.actions
                    .push(WorldAction::SetBlock(p, place, crate::level::update::ALL));
                play_block_sound(t, p, "minecraft:item.crop.plant", true);
                let inv = &mut state_mut(t.mob()).merchant.inventory;
                inv[i].shrink(1);
                if inv[i].is_empty() {
                    inv[i] = ItemStack::EMPTY;
                }
                break;
            }
        }
        if crop_growing(block) {
            vs_mut(t.mob()).farmland.retain(|&f| f != p);
            *target = pick_farmland(t);
            if let Some(n) = *target {
                *next_ok = now + 20;
                let br = brain(t);
                br.set(M::WalkTarget, MemValue::Walk(WalkTarget::block(n, 0.5, 1)));
                br.set(M::LookTarget, MemValue::Look(Tracker::block(n)));
            }
        }
    }
    *worked += 1;
}

/// `ShowTradesToPlayer.findItemsToDisplay`.
fn find_items_to_display(t: &mut Tick<'_, '_>, player: EntityId, look_time: &mut i32) {
    let held = t
        .ents
        .player(player)
        .map(|p| p.inventory.selected_item().clone())
        .unwrap_or_default();
    let changed = match &vs(t.mob_ref()).shown_player_stack {
        None => true,
        Some(s) => s.item != held.item || s.is_empty() != held.is_empty(),
    };
    if !changed {
        return;
    }
    let vv = vs_mut(t.mob());
    vv.shown_player_stack = Some(held.clone());
    vv.display_items.clear();
    if held.is_empty() {
        return;
    }
    let offers = state(t.mob_ref())
        .merchant
        .offers
        .clone()
        .unwrap_or_default();
    let shown: Vec<ItemStack> = offers
        .0
        .iter()
        .filter(|o| {
            !o.is_out_of_stock()
                && (o.cost_a().item == held.item
                    || (!o.cost_b().is_empty() && o.cost_b().item == held.item))
        })
        .map(|o| o.result.clone())
        .collect();
    if !shown.is_empty() {
        *look_time = 900;
        let first = shown[0].clone();
        vs_mut(t.mob()).display_items = shown;
        set_held(t, first, 0.0);
    }
}

/// `GiveGiftToHero.throwGift`.
fn throw_gift(t: &mut Tick<'_, '_>, hero: EntityId) {
    let table = if t.mob_ref().baby {
        "minecraft:gameplay/hero_of_the_village/baby_gift".to_owned()
    } else {
        match prof(t) {
            profession::NONE | profession::NITWIT => {
                "minecraft:gameplay/hero_of_the_village/unemployed_gift".to_owned()
            }
            p => format!(
                "minecraft:gameplay/hero_of_the_village/{}_gift",
                v::profession_name(p)
            ),
        }
    };
    let items = crate::mob::fam::brainy::gift_loot(t, &table);
    let Some(target) = bh::entity_pos(t, hero) else {
        return;
    };
    for stack in items {
        v::throw_item(t, stack, target);
    }
}

/// `TradeWithVillager.tick`.
fn trade_tick(t: &mut Tick<'_, '_>, now: i64) {
    let Some(target) = brain_ref(t).entity(M::InteractionTarget) else {
        return;
    };
    if bh::dist_sqr(t, target) > 5.0 {
        return;
    }
    bh::lock_gaze_and_walk_to_each_other(t, target, 0.5, 2);
    v::gossip(t, target, now);
    if !t.exists() {
        return;
    }
    let farmer = prof(t) == profession::FARMER;
    let target_wants_food = t.ents.mob(target).is_some_and(v::wants_more_food);
    let Some(to) = bh::entity_pos(t, target) else {
        return;
    };
    if v::has_excess_food(t.mob_ref()) && (farmer || target_wants_food) {
        throw_half_stack(t, |s| v::stack_villager_food(s).is_some(), to);
    }
    if farmer && v::count_item(&state(t.mob_ref()).merchant.inventory, items::WHEAT.0) > 32 {
        throw_half_stack(t, |s| s.item == items::WHEAT.0, to);
    }
    let trades = vs(t.mob_ref()).willing_trades.clone();
    if !trades.is_empty()
        && state(t.mob_ref())
            .merchant
            .inventory
            .iter()
            .any(|s| !s.is_empty() && trades.contains(&s.item))
    {
        throw_half_stack(t, |s| trades.contains(&s.item), to);
    }
}

/// `TradeWithVillager.throwHalfStack`.
fn throw_half_stack(t: &mut Tick<'_, '_>, pred: impl Fn(&ItemStack) -> bool, to: Vec3) {
    let mut thrown = ItemStack::EMPTY;
    {
        let inv = &mut state_mut(t.mob()).merchant.inventory;
        for s in inv.iter_mut() {
            if s.is_empty() || !pred(s) {
                continue;
            }
            let max = crate::item::max_stack_size(s);
            let count = if s.count > max / 2 {
                s.count / 2
            } else if s.count > 24 {
                s.count - 24
            } else {
                continue;
            };
            thrown = s.split(count);
            break;
        }
    }
    v::throw_item(t, thrown, to);
}

/// `VillagerMakeLove.tryToGiveBirth`.
fn give_birth(t: &mut Tick<'_, '_>, partner: EntityId) {
    let center = t.block_pos();
    // `takeVacantBed`: the first free bed the villager can reach.
    let candidates = t
        .ents
        .poi
        .in_range(|ty| ty == PoiType::Home, center, 48, Occupancy::HasSpace);
    let mut bed = None;
    for r in candidates {
        let path = crate::mob::nav::create_path_to(t, r.pos, r.ty.valid_range());
        if path.is_some_and(|p| p.reached) {
            bed = Some(r.pos);
            break;
        }
    }
    let Some(bed) = bed else {
        v::entity_event(t, partner, 13);
        let me = t.id;
        v::entity_event(t, me, 13);
        return;
    };
    t.ents.poi.acquire(bed);
    match v::breed(t, partner) {
        Some(child) => {
            let g = here(t, bed);
            if let Some(b) = t.ents.mob_mut(child).and_then(|m| m.brain.as_deref_mut()) {
                b.set(M::Home, MemValue::GlobalPos(g));
            }
        }
        None => {
            t.ents.poi.release(bed);
        }
    }
}

pub fn behavior_stop(t: &mut Tick<'_, '_>, b: &mut Behavior, now: i64) {
    match b {
        Behavior::FollowTradingPlayer => {
            let br = brain(t);
            br.erase(M::WalkTarget);
            br.erase(M::LookTarget);
        }
        Behavior::GoToPotentialJobSite => {
            if let Some(g) = global_pos(t, M::PotentialJobSite)
                && same_dimension(t, &g)
                && t.ents.poi.exists(g.pos, |_| true)
            {
                t.ents.poi.release(g.pos);
            }
            brain(t).erase(M::PotentialJobSite);
        }
        Behavior::HarvestFarmland {
            next_ok, worked, ..
        } => {
            let br = brain(t);
            br.erase(M::LookTarget);
            br.erase(M::WalkTarget);
            *worked = 0;
            *next_ok = now + 40;
        }
        Behavior::UseBonemeal { last_session, .. } => {
            t.mob()
                .equipment
                .set(EquipmentSlot::MainHand, ItemStack::EMPTY);
            t.core_mut().mark_data_dirty();
            *last_session = t.mob_ref().tick_count;
        }
        Behavior::ShowTrades { .. } => {
            brain(t).erase(M::InteractionTarget);
            set_held(t, ItemStack::EMPTY, 0.085);
            vs_mut(t.mob()).shown_player_stack = None;
        }
        Behavior::GiveGift { until_next, .. } => {
            *until_next = 600 + t.env.random.next_int_bounded(6001);
            let br = brain(t);
            br.erase(M::InteractionTarget);
            br.erase(M::WalkTarget);
            br.erase(M::LookTarget);
        }
        Behavior::JumpOnBed {
            bed,
            time_to_reach,
            jumps,
            cooldown,
        } => {
            *bed = None;
            *time_to_reach = 0;
            *jumps = 0;
            *cooldown = 0;
        }
        Behavior::SleepInBed { next_ok } => {
            if v::is_sleeping(t.mob_ref()) {
                v::stop_sleeping(t);
                *next_ok = now + 40;
            }
        }
        Behavior::TradeWithVillager => brain(t).erase(M::InteractionTarget),
        Behavior::MakeLove { .. } => brain(t).erase(M::BreedTarget),
        Behavior::Celebrate => v::update_activity_from_schedule(t),
        _ => {}
    }
}

// --- One shots ---------------------------------------------------------------------------------

pub fn behavior_trigger(t: &mut Tick<'_, '_>, b: &mut Behavior, now: i64) -> bool {
    match b {
        Behavior::Door {
            last_node,
            cooldown,
        } => door(t, last_node, cooldown),
        Behavior::WakeUp => {
            if !brain_ref(t).is_active(Activity::Rest) && v::is_sleeping(t.mob_ref()) {
                v::stop_sleeping(t);
                true
            } else {
                false
            }
        }
        Behavior::ReactToBell => {
            let pos = t.block_pos();
            if t.ents.raids.raid_at(pos).is_none() {
                brain(t).set_active_activity_if_possible(Activity::Hide);
            }
            true
        }
        Behavior::SetRaidStatus => {
            if t.env.random.next_int_bounded(20) != 0 {
                return false;
            }
            let pos = t.block_pos();
            let status = t
                .ents
                .raids
                .raid_at(pos)
                .map(|r| r.has_first_wave_spawned() && !r.is_between_waves());
            if let Some(fighting) = status {
                let a = if fighting {
                    Activity::Raid
                } else {
                    Activity::PreRaid
                };
                let br = brain(t);
                br.set_default_activity(a);
                br.set_active_activity_if_possible(a);
            }
            true
        }
        Behavior::ValidatePoi { memory } => validate_poi(t, *memory),
        Behavior::PoiCompetitorScan => {
            let Some(job) = global_pos(t, M::JobSite) else {
                return false;
            };
            let Some(ty) = t.ents.poi.get_type(job.pos) else {
                return true;
            };
            let nearby: Vec<EntityId> = brain_ref(t).entities(M::NearestLivingEntities).to_vec();
            let mut winner = t.id;
            for id in nearby {
                if id == t.id {
                    continue;
                }
                // A living villager with the same job site whose profession holds it.
                let competes = t.ents.mob(id).is_some_and(|m| {
                    m.kind == Kind::Villager
                        && m.is_alive()
                        && matches!(
                            m.brain.as_deref().and_then(|b| b.get(M::JobSite)),
                            Some(MemValue::GlobalPos(g))
                                if g.pos == job.pos && g.dimension == job.dimension
                        )
                        && v::held_job_site(vs(m).profession, ty)
                });
                if !competes {
                    continue;
                }
                let xp = |e: EntityId| {
                    t.ents
                        .mob(e)
                        .map_or(0, |m| super::state(m).merchant.villager_xp)
                };
                let (win, lose) = if xp(winner) > xp(id) {
                    (winner, id)
                } else {
                    (id, winner)
                };
                if let Some(b) = t.ents.mob_mut(lose).and_then(|m| m.brain.as_deref_mut()) {
                    b.erase(M::JobSite);
                }
                winner = win;
            }
            true
        }
        Behavior::AcquirePoi { which, next_start } => acquire_poi(t, *which, next_start, now),
        Behavior::YieldJobSite => yield_job_site(t),
        Behavior::AssignProfession => {
            let Some(g) = global_pos(t, M::PotentialJobSite) else {
                return false;
            };
            if !closer_to_center(g.pos, t.pos(), 2.0) {
                return false;
            }
            let br = brain(t);
            br.erase(M::PotentialJobSite);
            br.set(M::JobSite, MemValue::GlobalPos(g.clone()));
            let me = t.id;
            v::entity_event(t, me, 14);
            if prof(t) != profession::NONE {
                return true;
            }
            if same_dimension(t, &g)
                && let Some(p) = t
                    .ents
                    .poi
                    .get_type(g.pos)
                    .and_then(v::profession_of_job_site)
            {
                v::set_profession(t.mob(), p);
                t.core_mut().mark_data_dirty();
                v::refresh_brain(t);
            }
            true
        }
        Behavior::ResetProfession => {
            let vv = vs(t.mob_ref());
            let firable = vv.profession != profession::NONE && vv.profession != profession::NITWIT;
            if firable && super::state(t.mob_ref()).merchant.villager_xp == 0 && vv.level <= 1 {
                v::set_profession(t.mob(), profession::NONE);
                t.core_mut().mark_data_dirty();
                v::refresh_brain(t);
                true
            } else {
                false
            }
        }
        Behavior::StrollToPoiList { next_ok } => {
            let list = match brain_ref(t).get(M::SecondaryJobSite) {
                Some(MemValue::GlobalPositions(l)) => l.clone(),
                _ => return false,
            };
            let Some(stay) = global_pos(t, M::JobSite) else {
                return false;
            };
            if list.is_empty() {
                return false;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let i = t.env.random.next_int_bounded(list.len() as i32);
            let target = &list[usize::try_from(i).unwrap_or(0)];
            if !same_dimension(t, target) || !closer_to_center(stay.pos, t.pos(), 6.0) {
                return false;
            }
            if now > *next_ok {
                brain(t).set(
                    M::WalkTarget,
                    MemValue::Walk(WalkTarget::block(target.pos, SPEED, 1)),
                );
                *next_ok = now + 100;
            }
            true
        }
        Behavior::WalkToMemory {
            memory,
            speed,
            close,
            too_far,
            too_long,
        } => walk_to_memory_trigger(t, *memory, *speed, *close, *too_far, *too_long, now),
        Behavior::UpdateSchedule => {
            v::update_activity_from_schedule(t);
            true
        }
        Behavior::PlayTag => play_tag(t),
        Behavior::VillageStroll { speed, xz, y } => {
            let pos = t.block_pos();
            let land = if is_village(t, pos) {
                randompos::land_pos(t, *xz, *y)
            } else {
                let section = pos.section();
                let best = section_closest_to_village(t, section, 2);
                if best == section {
                    randompos::land_pos(t, *xz, *y)
                } else {
                    let c = best.origin().offset(8, 8, 8);
                    randompos::default_pos_towards(
                        t,
                        *xz,
                        *y,
                        randompos::bottom_center(c),
                        randompos::half_pi(),
                    )
                }
            };
            brain(t).set_opt(
                M::WalkTarget,
                land.map(|p| MemValue::Walk(WalkTarget::at(p, *speed, 0))),
            );
            true
        }
        Behavior::ClosestHome => closest_home(t),
        Behavior::InsideBrownian => {
            let pos = t.block_pos();
            if can_see_sky(t, pos) {
                return false;
            }
            let mut list = Vec::with_capacity(27);
            // `BlockPos.betweenClosedStream`: x fastest, then y, then z.
            for z in -1..=1 {
                for y in -1..=1 {
                    for x in -1..=1 {
                        list.push(pos.offset(x, y, z));
                    }
                }
            }
            // `Collections.shuffle(list)`: a random of its own, not the mob's.
            let mut own = vanillite_base::random::LegacyRandom::new(crate::mob::new_shuffle_seed());
            crate::poi::shuffle(&mut list, &mut own);
            let dims = t.core().dims();
            let mut scratch = Vec::new();
            let found = list.into_iter().find(|&p| {
                !can_see_sky(t, p)
                    && t.level.is_loaded(p)
                    && stand_on(t, p)
                    && crate::mob::physics::no_collision(
                        &t.level.view(),
                        &dims.bounding_box(t.pos()),
                        &mut scratch,
                    )
            });
            if let Some(p) = found {
                brain(t).set(
                    M::WalkTarget,
                    MemValue::Walk(WalkTarget::block(p, SPEED, 0)),
                );
            }
            true
        }
        Behavior::ClosestVillage => {
            let pos = t.block_pos();
            if is_village(t, pos) {
                return false;
            }
            let current = sections_to_village(t, pos);
            let mut target = None;
            for _ in 0..5 {
                let land = randompos::land_pos_weighted(t, 15, 7, |t, p| {
                    -f64::from(sections_to_village(t, p))
                });
                if let Some(l) = land {
                    let d = sections_to_village(t, BlockPos::containing(l.x, l.y, l.z));
                    if d < current {
                        target = Some(l);
                        break;
                    }
                    if d == current {
                        target = Some(l);
                    }
                }
            }
            if let Some(l) = target {
                brain(t).set(M::WalkTarget, MemValue::Walk(WalkTarget::at(l, SPEED, 4)));
            }
            true
        }
        Behavior::SocializeAtBell => {
            let Some(meeting) = global_pos(t, M::MeetingPoint) else {
                return false;
            };
            if t.env.random.next_int_bounded(100) != 0
                || !same_dimension(t, &meeting)
                || !closer_to_center(meeting.pos, t.pos(), 4.0)
                || sensing::visible_find_closest(t, is_villager).is_none()
            {
                return false;
            }
            if let Some(other) = sensing::visible_find_closest(t, |t, id| {
                is_villager(t, id) && bh::dist_sqr(t, id) <= 32.0
            }) {
                let br = brain(t);
                br.set(M::InteractionTarget, MemValue::Entity(other));
                br.set(M::LookTarget, MemValue::Look(Tracker::entity(other, true)));
                br.set(
                    M::WalkTarget,
                    MemValue::Walk(WalkTarget::entity(other, 0.3, 1)),
                );
            }
            true
        }
        Behavior::CalmDown => {
            let hurt_by_near = brain_ref(t)
                .entity(M::HurtByEntity)
                .is_some_and(|e| bh::dist_sqr(t, e) <= 36.0);
            let scared =
                brain_ref(t).has(M::HurtBy) || brain_ref(t).has(M::NearestHostile) || hurt_by_near;
            if !scared {
                let br = brain(t);
                br.erase(M::HurtBy);
                br.erase(M::HurtByEntity);
                v::update_activity_from_schedule(t);
            }
            true
        }
        Behavior::RingBell => {
            if t.env.random.next_float() <= 0.95 {
                return false;
            }
            if let Some(g) = global_pos(t, M::MeetingPoint)
                && closer_than_block(g.pos, t.block_pos(), 3.0)
                && t.level.block(g.pos).block().name() == "minecraft:bell"
            {
                let id = t.id;
                t.cx.actions.push(WorldAction::RingBell(g.pos, Some(id)));
            }
            true
        }
        Behavior::SkySeeingSpot { speed } => {
            let pos = t.block_pos();
            if can_see_sky(t, pos) {
                return false;
            }
            for _ in 0..10 {
                let r = &mut t.mob().random;
                let p = pos.offset(
                    r.next_int_bounded(20) - 10,
                    r.next_int_bounded(6) - 3,
                    r.next_int_bounded(20) - 10,
                );
                if no_blocks_above(t, p) {
                    brain(t).set(
                        M::WalkTarget,
                        MemValue::Walk(WalkTarget::at(randompos::bottom_center(p), *speed, 0)),
                    );
                    break;
                }
            }
            true
        }
        Behavior::LocateHiding {
            radius,
            speed,
            close,
        } => {
            let pos = t.block_pos();
            let me = t.pos();
            let close_d = f64::from(*close);
            let near = t
                .ents
                .poi
                .find(
                    |ty| ty == PoiType::Home,
                    |_| true,
                    pos,
                    *close + 1,
                    Occupancy::Any,
                )
                .filter(|p| closer_to_center(*p, me, close_d));
            let found = near
                .or_else(|| {
                    let mut r = std::mem::replace(
                        &mut t.mob().random,
                        vanillite_base::random::LegacyRandom::new(0),
                    );
                    let p = t.ents.poi.get_random(
                        |ty| ty == PoiType::Home,
                        |_| true,
                        Occupancy::Any,
                        pos,
                        *radius,
                        &mut r,
                    );
                    t.mob().random = r;
                    p
                })
                .or_else(|| global_pos(t, M::Home).map(|g| g.pos));
            if let Some(p) = found {
                let g = here(t, p);
                let br = brain(t);
                br.erase(M::Path);
                br.erase(M::LookTarget);
                br.erase(M::BreedTarget);
                br.erase(M::InteractionTarget);
                br.set(M::HidingPlace, MemValue::GlobalPos(g));
                if !closer_to_center(p, me, close_d) {
                    br.set(
                        M::WalkTarget,
                        MemValue::Walk(WalkTarget::block(p, *speed, *close)),
                    );
                }
            }
            true
        }
        Behavior::ResetRaidStatus => {
            if t.env.random.next_int_bounded(20) != 0 {
                return false;
            }
            let pos = t.block_pos();
            let over = t
                .ents
                .raids
                .raid_at(pos)
                .is_none_or(|r| r.is_stopped() || r.is_loss());
            if over {
                brain(t).set_default_activity(Activity::Idle);
                v::update_activity_from_schedule(t);
            }
            true
        }
        Behavior::HiddenState { stay_ticks, close } => {
            let Some(heard) = brain_ref(t).long(M::HeardBellTime) else {
                return false;
            };
            let Some(hide) = global_pos(t, M::HidingPlace) else {
                return false;
            };
            let timed_out = heard + 300 <= now;
            let hidden = vs(t.mob_ref()).ticks_hidden;
            if hidden <= *stay_ticks && !timed_out {
                if closer_than_block(hide.pos, t.block_pos(), f64::from(*close)) {
                    vs_mut(t.mob()).ticks_hidden += 1;
                }
            } else {
                let br = brain(t);
                br.erase(M::HeardBellTime);
                br.erase(M::HidingPlace);
                v::update_activity_from_schedule(t);
                vs_mut(t.mob()).ticks_hidden = 0;
            }
            true
        }
        Behavior::InteractBreed => {
            if !v::can_breed(t.mob_ref()) {
                return false;
            }
            let valid = |t: &Tick<'_, '_>, id: EntityId| {
                is_villager(t, id) && t.ents.mob(id).is_some_and(v::can_breed)
            };
            if sensing::visible_find_closest(t, valid).is_none() {
                return false;
            }
            if let Some(id) = sensing::visible_find_closest(t, |t, id| {
                bh::dist_sqr(t, id) <= 64.0 && valid(t, id)
            }) {
                let br = brain(t);
                br.set(M::BreedTarget, MemValue::Entity(id));
                br.set(M::LookTarget, MemValue::Look(Tracker::entity(id, true)));
                br.set(
                    M::WalkTarget,
                    MemValue::Walk(WalkTarget::entity(id, SPEED, 2)),
                );
            }
            true
        }
        _ => false,
    }
}

/// `loadedAndEntityCanStandOn(pos, entity)`.
fn stand_on(t: &Tick<'_, '_>, pos: BlockPos) -> bool {
    let state = t.level.block(pos);
    vanillite_worldgen::behaviour::is_face_sturdy(
        state,
        vanillite_base::Direction::Up,
        vanillite_worldgen::behaviour::SupportType::Full,
    )
}

/// `InteractWithDoor`.
pub(crate) fn door(
    t: &mut Tick<'_, '_>,
    last_node: &mut Option<BlockPos>,
    cooldown: &mut i32,
) -> bool {
    let me = t.id;
    let Some((prev, next)) = path_nodes(t, me) else {
        return false;
    };
    let started = t.nav_ref().path.as_ref().is_some_and(|p| p.next > 0);
    if !started {
        return false;
    }
    if *last_node == Some(next) {
        *cooldown = 20;
    } else {
        *cooldown -= 1;
        if *cooldown > 0 {
            return false;
        }
    }
    *last_node = Some(next);
    if let Some(from) = prev {
        let state = t.level.block(from);
        if is_mob_door(state) {
            if !is_open(state) {
                set_door(t, from, true);
            }
            remember_door(t, from);
        }
    }
    let state = t.level.block(next);
    if is_mob_door(state) && !is_open(state) {
        set_door(t, next, true);
        remember_door(t, next);
    }
    if brain_ref(t).has(M::DoorsToClose) {
        close_doors(t, prev, Some(next));
    }
    true
}

/// `ValidateNearbyPoi`.
fn validate_poi(t: &mut Tick<'_, '_>, memory: M) -> bool {
    let Some(g) = global_pos(t, memory) else {
        return false;
    };
    if !same_dimension(t, &g) || !closer_to_center(g.pos, t.pos(), 16.0) {
        return false;
    }
    let pos = g.pos;
    let exists = t
        .ents
        .poi
        .get_type(pos)
        .is_some_and(|ty| memory_poi(t, memory, ty));
    if !exists {
        brain(t).erase(memory);
        return true;
    }
    let state = t.level.block(pos);
    let occupied =
        v::is_villager_bed(state) && state.flag("occupied") && !v::is_sleeping(t.mob_ref());
    if occupied {
        brain(t).erase(memory);
        // `bedIsOccupiedByVillager`: a sleeping villager in the bed's block.
        let area = vanillite_base::Aabb::new(
            f64::from(pos.x),
            f64::from(pos.y),
            f64::from(pos.z),
            f64::from(pos.x + 1),
            f64::from(pos.y + 1),
            f64::from(pos.z + 1),
        );
        let mut by_villager = false;
        let ents = &*t.ents;
        ents.store.for_each_in(&area, false, |id| {
            if ents
                .mob(id)
                .is_some_and(|m| m.kind == Kind::Villager && v::is_sleeping(m))
            {
                by_villager = true;
            }
        });
        if !by_villager {
            t.ents.poi.release(pos);
        }
    }
    true
}

/// `AcquirePoi`'s one shot.
fn acquire_poi(t: &mut Tick<'_, '_>, which: Acquire, next_start: &mut i64, now: i64) -> bool {
    let only_adult = matches!(which, Acquire::Job | Acquire::Meeting);
    if only_adult && t.mob_ref().baby {
        return false;
    }
    let game_time = t.game_time();
    if *next_start == 0 {
        *next_start = game_time + i64::from(t.env.random.next_int_bounded(20));
        return false;
    }
    if game_time < *next_start {
        return false;
    }
    *next_start = now + 20 + i64::from(t.env.random.next_int_bounded(20));
    let slot = which as usize;
    vs_mut(t.mob()).acquire_cache[slot].retain(|(_, r)| now - r.previous < 400);
    let p = prof(t);
    let pred = move |ty: PoiType| match which {
        Acquire::Job => v::acquirable_job_site(p, ty),
        Acquire::Home => ty == PoiType::Home,
        Acquire::Meeting => ty == PoiType::Meeting,
    };
    let center = t.block_pos();
    let records = t.ents.poi.in_range(pred, center, 48, Occupancy::HasSpace);
    let mut ordered: Vec<(PoiType, BlockPos)> = records.iter().map(|r| (r.ty, r.pos)).collect();
    ordered.sort_by(|a, b| a.1.dist_sqr(center).total_cmp(&b.1.dist_sqr(center)));
    // The cache test runs lazily over the sorted stream until five pass.
    let mut picked: Vec<(PoiType, BlockPos)> = Vec::new();
    for (ty, pos) in ordered {
        if picked.len() >= 5 {
            break;
        }
        let pass = {
            let mut r =
                std::mem::replace(t.env.random, vanillite_base::random::LegacyRandom::new(0));
            let cache = &mut vs_mut(t.mob()).acquire_cache[slot];
            let ok = match cache.iter_mut().find(|(p, _)| *p == pos) {
                None => true,
                Some((_, marker)) => {
                    if now >= marker.next {
                        marker.mark(&mut r, now);
                        true
                    } else {
                        false
                    }
                }
            };
            *t.env.random = r;
            ok
        };
        if pass {
            picked.push((ty, pos));
        }
    }
    if which == Acquire::Home {
        picked.retain(|&(_, pos)| {
            let s = t.level.block(pos);
            v::is_villager_bed(s) && !s.flag("occupied")
        });
    }
    let reach = picked
        .iter()
        .map(|&(ty, _)| ty.valid_range())
        .fold(1, i32::max);
    let targets: Vec<BlockPos> = picked.iter().map(|&(_, p)| p).collect();
    let path = if targets.is_empty() {
        None
    } else {
        crate::mob::nav::create_path_to_any(t, &targets, reach)
    };
    match path.filter(|p| p.reached) {
        Some(path) => {
            let target = path.target;
            if t.ents.poi.get_type(target).is_some() {
                t.ents.poi.take(pred, |_, p| p == target, target, 1);
                let g = here(t, target);
                let memory = match which {
                    Acquire::Job => M::PotentialJobSite,
                    Acquire::Home => M::Home,
                    Acquire::Meeting => M::MeetingPoint,
                };
                brain(t).set(memory, MemValue::GlobalPos(g));
                if which != Acquire::Job {
                    let me = t.id;
                    v::entity_event(t, me, 14);
                }
                vs_mut(t.mob()).acquire_cache[slot].clear();
            }
        }
        None => {
            for (_, pos) in picked {
                if vs(t.mob_ref()).acquire_cache[slot]
                    .iter()
                    .any(|(p, _)| *p == pos)
                {
                    continue;
                }
                let r = super::villager::Retry::new(t.env.random, now);
                vs_mut(t.mob()).acquire_cache[slot].push((pos, r));
            }
        }
    }
    true
}

/// `YieldJobSite`.
fn yield_job_site(t: &mut Tick<'_, '_>) -> bool {
    if t.mob_ref().baby || prof(t) != profession::NONE {
        return false;
    }
    let Some(g) = global_pos(t, M::PotentialJobSite) else {
        return false;
    };
    let pos = g.pos;
    let Some(ty) = t.ents.poi.get_type(pos) else {
        return true;
    };
    let nearby: Vec<EntityId> = brain_ref(t).entities(M::NearestLivingEntities).to_vec();
    for id in nearby {
        if id == t.id || !is_villager(t, id) || !bh::is_alive(t, id) {
            continue;
        }
        if villager_brain_has(t, id, M::PotentialJobSite) {
            continue;
        }
        let their_prof = t
            .ents
            .mob(id)
            .map_or(profession::NONE, |m| vs(m).profession);
        if !v::held_job_site(their_prof, ty) {
            continue;
        }
        let wants = match other_global(t, id, M::JobSite) {
            Some(j) => j.pos == pos,
            None => can_reach_as(t, id, pos, ty.valid_range()),
        };
        if !wants {
            continue;
        }
        let br = brain(t);
        br.erase(M::WalkTarget);
        br.erase(M::LookTarget);
        br.erase(M::PotentialJobSite);
        if !villager_brain_has(t, id, M::JobSite) {
            let g = here(t, pos);
            if let Some(b) = t.ents.mob_mut(id).and_then(|m| m.brain.as_deref_mut()) {
                let target = Tracker::block(pos);
                b.set(M::LookTarget, MemValue::Look(target));
                b.set(
                    M::WalkTarget,
                    MemValue::Walk(WalkTarget {
                        target,
                        speed: SPEED,
                        close_enough: 1,
                    }),
                );
                b.set(M::PotentialJobSite, MemValue::GlobalPos(g));
            }
        }
        break;
    }
    true
}

/// `canReachPos` of another villager: its own path search.
fn can_reach_as(t: &mut Tick<'_, '_>, other: EntityId, pos: BlockPos, reach: i32) -> bool {
    let mut o = Tick {
        level: t.level,
        ents: &mut *t.ents,
        env: &mut *t.env,
        cx: &mut *t.cx,
        id: other,
    };
    crate::mob::nav::create_path_to(&mut o, pos, reach).is_some_and(|p| p.reached)
}

/// `SetWalkTargetFromBlockMemory`.
fn walk_to_memory_trigger(
    t: &mut Tick<'_, '_>,
    memory: M,
    speed: f32,
    close: i32,
    too_far: i32,
    too_long: i64,
    now: i64,
) -> bool {
    let Some(g) = global_pos(t, memory) else {
        return false;
    };
    let cant = brain_ref(t).long(M::CantReachWalkTargetSince);
    let give_up = |t: &mut Tick<'_, '_>| {
        v::release_poi(t, memory);
        let br = brain(t);
        br.erase(memory);
        br.set(M::CantReachWalkTargetSince, MemValue::Long(now));
    };
    if !same_dimension(t, &g) || cant.is_some_and(|c| t.game_time() - c > too_long) {
        give_up(t);
        return true;
    }
    let me = t.block_pos();
    if manhattan(g.pos, me) > too_far {
        let mut tries = 0;
        loop {
            let towards = randompos::default_pos_towards(
                t,
                15,
                7,
                randompos::bottom_center(g.pos),
                randompos::half_pi(),
            );
            if let Some(p) = towards
                && manhattan(BlockPos::containing(p.x, p.y, p.z), me) <= too_far
            {
                brain(t).set(
                    M::WalkTarget,
                    MemValue::Walk(WalkTarget::at(p, speed, close)),
                );
                break;
            }
            tries += 1;
            if tries == 1000 {
                give_up(t);
                return true;
            }
        }
    } else if manhattan(g.pos, me) > close {
        brain(t).set(
            M::WalkTarget,
            MemValue::Walk(WalkTarget::block(g.pos, speed, close)),
        );
    }
    true
}

/// `PlayTagWithOtherKids`.
fn play_tag(t: &mut Tick<'_, '_>) -> bool {
    if t.env.random.next_int_bounded(10) != 0 {
        return false;
    }
    let friends: Vec<EntityId> = brain_ref(t).entities(M::VisibleVillagerBabies).to_vec();
    let me = t.id;
    let chased_by = |t: &Tick<'_, '_>, id: EntityId| {
        t.ents
            .mob(id)
            .and_then(|m| m.brain.as_deref())
            .and_then(|b| b.entity(M::InteractionTarget))
    };
    if friends.iter().any(|&f| chased_by(t, f) == Some(me)) {
        for _ in 0..10 {
            if let Some(p) = randompos::land_pos(t, 20, 8)
                && is_village(t, BlockPos::containing(p.x, p.y, p.z))
            {
                brain(t).set(M::WalkTarget, MemValue::Walk(WalkTarget::at(p, 0.6, 0)));
                break;
            }
        }
        return true;
    }
    // `findSomeoneBeingChased`: the kid with the fewest chasers (1 to 5).
    let mut counts: Vec<(EntityId, i32)> = Vec::new();
    for &f in &friends {
        if let Some(target) = chased_by(t, f) {
            match counts.iter_mut().find(|(e, _)| *e == target) {
                Some((_, c)) => *c += 1,
                None => counts.push((target, 1)),
            }
        }
    }
    counts.sort_by_key(|&(_, c)| c);
    let target = counts
        .into_iter()
        .find(|&(_, c)| c > 0 && c <= 5)
        .map(|(e, _)| e)
        .or_else(|| friends.first().copied());
    if let Some(kid) = target {
        let br = brain(t);
        br.set(M::InteractionTarget, MemValue::Entity(kid));
        br.set(M::LookTarget, MemValue::Look(Tracker::entity(kid, true)));
        br.set(
            M::WalkTarget,
            MemValue::Walk(WalkTarget::entity(kid, 0.6, 1)),
        );
    }
    true
}

/// `SetClosestHomeAsWalkTarget`.
fn closest_home(t: &mut Tick<'_, '_>) -> bool {
    let now = t.game_time();
    if now - vs(t.mob_ref()).home_last_update < 20 {
        return false;
    }
    let center = t.block_pos();
    let closest = t.ents.poi.find_closest(
        |ty| ty == PoiType::Home,
        |_| true,
        center,
        48,
        Occupancy::Any,
    );
    if closest.is_none_or(|c| c.dist_sqr(center) <= 4.0) {
        return false;
    }
    let last = now + i64::from(t.env.random.next_int_bounded(20));
    vs_mut(t.mob()).home_last_update = last;
    let (candidates, tried) = home_candidates(t, false, last);
    let reach = candidates
        .iter()
        .map(|&(ty, _)| ty.valid_range())
        .fold(1, i32::max);
    let positions: Vec<BlockPos> = candidates.iter().map(|&(_, p)| p).collect();
    let path = crate::mob::nav::create_path_to_any(t, &positions, reach);
    match path.filter(|p| p.reached) {
        Some(p) => {
            if t.ents.poi.get_type(p.target).is_some() {
                brain(t).set(
                    M::WalkTarget,
                    MemValue::Walk(WalkTarget::block(p.target, SPEED, 1)),
                );
            }
        }
        None => {
            if tried < 5 {
                vs_mut(t.mob()).home_cache.retain(|&(_, at)| at >= last);
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_build() {
        for p in [profession::NONE, profession::FARMER, profession::ARMORER] {
            for baby in [false, true] {
                let mut r = vanillite_base::random::LegacyRandom::new(1);
                let b = crate::mob::brain::Brain::new(brain_spec(p, baby), &mut r);
                assert!(b.is_registered(M::JobSite));
                assert!(b.is_registered(M::NearestHostile));
            }
        }
    }

    #[test]
    fn profession_job_sites() {
        assert_eq!(
            v::profession_of_job_site(PoiType::Farmer),
            Some(profession::FARMER)
        );
        assert_eq!(
            v::profession_of_job_site(PoiType::Weaponsmith),
            Some(profession::WEAPONSMITH)
        );
        assert!(v::acquirable_job_site(profession::NONE, PoiType::Librarian));
        assert!(!v::acquirable_job_site(
            profession::NITWIT,
            PoiType::Librarian
        ));
        assert!(!v::held_job_site(profession::NONE, PoiType::Librarian));
    }
}
