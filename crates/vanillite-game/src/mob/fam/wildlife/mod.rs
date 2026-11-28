//! Bees, turtles, mooshrooms, striders, snow and iron golems, bats, squids, glow squids,
//! dolphins and fish (cod, salmon, pufferfish, tropical fish).
//!
//! Each kind group lives in its own module; this module holds the family state, the goal
//! enum and the [`FamilyHooks`] implementation that dispatches to the groups.

mod bat;
pub(crate) mod bee;
mod dolphin;
pub mod ender_pearl;
pub mod eye_of_ender;
pub(crate) mod fish;
pub(crate) mod golem;
pub(crate) mod mooshroom;
pub mod snowball;
mod squid;
mod strider;
mod turtle;

#[cfg(test)]
mod tests;

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{Dimensions, EntityId};

use super::{FamilyHooks, Hurt, Interact, SpawnContext, SpawnReason};
use crate::effect as effects;
use crate::effect::EffectInstance;
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::common::MoveToBlock;
use crate::mob::kinds::{DefaultSound, Kind};
use crate::mob::missile::{MissileData, MissileKind, MissileTick};
use crate::mob::path::{MalusTable, PathType};
use crate::mob::{Environment, MobData, Tick};
use crate::player::encode::DataWriter;
use crate::survival::damage::DamageSource;

/// The family's hooks.
#[derive(Debug)]
pub struct Hooks;

/// The kind state of the family's mobs (in [`super::Ext::Wildlife`]). One flat record: each
/// kind uses its own fields.
#[derive(Clone, Debug)]
pub struct State {
    /// The air supply before `baseTick` (`WaterAnimal.handleAirSupply`).
    pub pre_air: i32,
    /// Was a baby last tick (turtles drop a scute when they grow up).
    pub was_baby: bool,
    // Fish.
    /// `Bucketable.fromBucket`.
    pub from_bucket: bool,
    /// `AbstractSchoolingFish.leader` and `schoolSize`.
    pub leader: Option<EntityId>,
    pub school_size: i32,
    /// Salmon size (0 small, 1 medium, 2 large), the packed tropical fish variant, the
    /// mooshroom type (0 red, 1 brown).
    pub variant: i32,
    /// `MushroomCow.lastLightningBoltUUID` (not saved).
    pub last_bolt: Option<u128>,
    /// `TropicalFish.isSchool`.
    pub is_school: bool,
    /// Pufferfish puff state, inflate counter and deflate timer.
    pub puff_state: i32,
    pub inflate_counter: i32,
    pub deflate_timer: i32,
    // Squids.
    pub tentacle_movement: f32,
    pub tentacle_speed: f32,
    pub rotate_speed: f32,
    pub movement_vector: Vec3,
    pub x_body_rot: f32,
    pub x_body_rot_o: f32,
    /// `yBodyRotO` (the body yaw after `baseTick`), for the ink's direction.
    pub y_body_rot_o: f32,
    /// `GlowSquid` dark ticks.
    pub dark_ticks: i32,
    // Dolphins.
    pub got_fish: bool,
    pub moistness: i32,
    pub treasure_pos: Option<BlockPos>,
    // Bees.
    /// `DATA_FLAGS_ID`: 2 rolling, 4 has stung, 8 has nectar.
    pub bee_flags: u8,
    pub time_since_sting: i32,
    pub ticks_without_nectar: i32,
    pub stay_out_of_hive: i32,
    pub crops_grown: i32,
    pub cooldown_hive: i32,
    pub cooldown_flower: i32,
    pub flower_pos: Option<BlockPos>,
    pub hive_pos: Option<BlockPos>,
    /// `BeeGoToHiveGoal.blacklistedTargets` (at most three, oldest first).
    pub hive_blacklist: Vec<BlockPos>,
    /// The nodes of `BeeGoToHiveGoal.lastPath`.
    pub hive_last_path: Option<Vec<crate::mob::path::PathNode>>,
    pub underwater_ticks: i32,
    pub pollinating: bool,
    // Turtles.
    pub home_pos: BlockPos,
    pub travel_pos: Option<BlockPos>,
    pub has_egg: bool,
    pub laying_egg: bool,
    pub lay_egg_counter: i32,
    pub going_home: bool,
    // Striders.
    pub suffocating: bool,
    // Golems.
    pub pumpkin: bool,
    pub player_created: bool,
    pub attack_animation: i32,
    pub offer_flower: i32,
    // Bats.
    pub resting: bool,
    pub bat_target: Option<BlockPos>,
    // Mooshrooms.
    /// `stew_effects`: the effect and its duration in ticks.
    pub stew_effects: Option<(crate::effect::MobEffect, i32)>,
}

impl State {
    const EMPTY: State = State {
        pre_air: 300,
        was_baby: false,
        from_bucket: false,
        leader: None,
        school_size: 1,
        variant: 0,
        last_bolt: None,
        is_school: true,
        puff_state: 0,
        inflate_counter: 0,
        deflate_timer: 0,
        tentacle_movement: 0.0,
        tentacle_speed: 0.0,
        rotate_speed: 0.0,
        movement_vector: Vec3::ZERO,
        x_body_rot: 0.0,
        x_body_rot_o: 0.0,
        y_body_rot_o: 0.0,
        dark_ticks: 0,
        got_fish: false,
        moistness: 2400,
        treasure_pos: None,
        bee_flags: 0,
        time_since_sting: 0,
        ticks_without_nectar: 0,
        stay_out_of_hive: 0,
        crops_grown: 0,
        cooldown_hive: 0,
        cooldown_flower: 0,
        flower_pos: None,
        hive_pos: None,
        hive_blacklist: Vec::new(),
        hive_last_path: None,
        underwater_ticks: 0,
        pollinating: false,
        home_pos: BlockPos::new(0, 0, 0),
        travel_pos: None,
        has_egg: false,
        laying_egg: false,
        lay_egg_counter: 0,
        going_home: false,
        suffocating: false,
        pumpkin: true,
        player_created: false,
        attack_animation: 0,
        offer_flower: 0,
        resting: false,
        bat_target: None,
        stew_effects: None,
    };
}

impl Default for State {
    fn default() -> Self {
        State::EMPTY
    }
}

static EMPTY: State = State::EMPTY;

/// The family state of a mob (defaults for other mobs).
pub(crate) fn st(m: &MobData) -> &State {
    match &m.ext {
        super::Ext::Wildlife(s) => s,
        _ => &EMPTY,
    }
}

/// The family state of a mob, mutably (created when missing).
pub(crate) fn st_mut(m: &mut MobData) -> &mut State {
    if !matches!(m.ext, super::Ext::Wildlife(_)) {
        m.ext = super::Ext::Wildlife(Box::default());
    }
    match &mut m.ext {
        super::Ext::Wildlife(s) => s,
        _ => unreachable!("set above"),
    }
}

/// The family's goals.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// Never runs.
    Idle,
    /// `RandomSwimmingGoal(mob, speed, interval)`; `fish` adds `FishSwimGoal`'s
    /// `canRandomSwim` (schooling followers do not swim on their own).
    Swim {
        speed: f64,
        interval: i32,
        fish: bool,
        wanted: Vec3,
    },
    /// `FollowFlockLeaderGoal`.
    FollowFlockLeader { recalc: i32, next_start: i32 },
    /// `PufferfishPuffGoal`.
    Puff,
    /// `SquidRandomMovementGoal`.
    SquidRandomMovement,
    /// `SquidFleeGoal`.
    SquidFlee { ticks: i32 },
    /// `TryFindWaterGoal`.
    TryFindWater,
    /// `BreathAirGoal`.
    BreathAir,
    /// `DolphinSwimToTreasureGoal`.
    DolphinSwimToTreasure { stuck: bool },
    /// `DolphinSwimWithPlayerGoal`.
    DolphinSwimWithPlayer {
        speed: f64,
        player: Option<EntityId>,
    },
    /// `DolphinJumpGoal(dolphin, interval)`.
    DolphinJump { interval: i32, breached: bool },
    /// `Dolphin.MoveToItemGoal`.
    DolphinMoveToItem { cooldown: i32 },
    /// `Dolphin.PlayWithItemsGoal`.
    DolphinPlayWithItems { cooldown: i32 },
    /// `MoveTowardsTargetGoal(mob, speed, within)`.
    MoveTowardsTarget {
        speed: f64,
        within: f32,
        target: Option<EntityId>,
        wanted: Vec3,
    },
    /// `TurtlePanicGoal`.
    TurtlePanic { speed: f64, pos: Vec3 },
    /// `TurtleBreedGoal`.
    TurtleBreed {
        speed: f64,
        partner: Option<EntityId>,
        love_time: i32,
    },
    /// `TurtleLayEggGoal`.
    TurtleLayEgg(MoveToBlock),
    /// `TurtleGoToWaterGoal`.
    TurtleGoToWater(MoveToBlock),
    /// `TurtleGoHomeGoal`.
    TurtleGoHome {
        speed: f64,
        stuck: bool,
        close_ticks: i32,
    },
    /// `TurtleTravelGoal`.
    TurtleTravel { speed: f64, stuck: bool },
    /// `TurtleRandomStrollGoal`.
    TurtleStroll {
        speed: f64,
        interval: i32,
        wanted: Vec3,
    },
    /// `StriderGoToLavaGoal`.
    StriderGoToLava(MoveToBlock),
    /// `BeePollinateGoal`.
    BeePollinate {
        successful: i32,
        last_sound: i32,
        hover: Vec3,
        has_hover: bool,
        ticks: i32,
    },
    /// `BeeGrowCropGoal`.
    BeeGrowCrop,
    /// `BeeWanderGoal`.
    BeeWander,
    /// `BeeGoToKnownFlowerGoal`.
    BeeGoToKnownFlower { travelling: i32 },
    /// `ValidateFlowerGoal` (its cooldown is rolled at construction).
    BeeValidateFlower { cooldown: i32, last_validate: i64 },
    /// `ValidateHiveGoal` (its cooldown is rolled at construction).
    BeeValidateHive { cooldown: i32, last_validate: i64 },
    /// `BeeEnterHiveGoal`.
    BeeEnterHive,
    /// `BeeLocateHiveGoal`.
    BeeLocateHive,
    /// `BeeGoToHiveGoal`.
    BeeGoToHive { travelling: i32, stuck: i32 },
    /// `GolemRandomStrollInVillageGoal(0.6)` and (`back`) `MoveBackToVillageGoal(0.6, false)`.
    GolemStroll {
        back: bool,
        speed: f64,
        interval: i32,
        wanted: Vec3,
    },
    /// `OfferFlowerGoal`.
    OfferFlower { target: Option<EntityId>, tick: i32 },
    /// `DefendVillageTargetGoal`: `candidate` is `potentialTarget`, kept between checks.
    DefendVillage {
        state: crate::mob::goals::TargetState,
        candidate: Option<EntityId>,
    },
}

/// `Goal.Flag` bits.
use crate::mob::goals::flag;

/// Adds a status effect to a mob or player, applied by `source`.
pub(crate) fn add_effect(
    ents: &mut crate::entities::Entities,
    target: EntityId,
    effect: crate::effect::MobEffect,
    duration: i32,
    amplifier: u8,
    source: Option<EntityId>,
) {
    effects::living::add_detached_from(
        ents,
        target,
        EffectInstance::new(effect, duration, i32::from(amplifier)),
        source,
    );
}

/// Whether the kind is a fish (`AbstractFish`).
pub(crate) fn is_fish(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Cod | Kind::Salmon | Kind::Pufferfish | Kind::TropicalFish
    )
}

/// Whether the kind lives in water (`WaterAnimal` or `AgeableWaterCreature`).
pub(crate) fn is_water_animal(kind: Kind) -> bool {
    is_fish(kind) || matches!(kind, Kind::Squid | Kind::GlowSquid | Kind::Dolphin)
}

/// `isInterruptable` of the wildlife goals: false for `BreathAirGoal`, `DolphinJumpGoal` and
/// `DolphinSwimToTreasureGoal`.
pub(crate) fn goal_interruptable(g: &Goal) -> bool {
    !matches!(
        g,
        Goal::BreathAir | Goal::DolphinJump { .. } | Goal::DolphinSwimToTreasure { .. }
    )
}

/// A block of the given block tag (the tag tables of the data packs).
pub(crate) fn block_tags() -> &'static vanillite_worldgen::behaviour::BlockTags {
    crate::block::block_tags()
}

/// An item of an item tag.
pub(crate) fn item_in_tag(item: u16, tag: &str) -> bool {
    crate::player::play::game_data().is_some_and(|d| d.item_in_tag(item, tag))
}

/// `AgeableMob` / `Animal` save data.
pub(crate) fn save_ageable(m: &MobData, out: &mut Compound) {
    out.insert("Age", m.age);
    out.insert("ForcedAge", m.forced_age);
    out.insert("AgeLocked", m.age_locked);
}

/// `Animal` save data (love).
pub(crate) fn save_love(m: &MobData, out: &mut Compound) {
    out.insert("InLove", m.in_love);
    if let Some(cause) = m.love_cause {
        out.insert("LoveCause", uuid_ints(cause));
    }
}

/// Reads `AgeableMob` and `Animal` save data.
pub(crate) fn load_ageable(m: &mut MobData, tag: &Compound) {
    m.age = int(tag, "Age").unwrap_or(0);
    m.baby = m.age < 0;
    m.forced_age = int(tag, "ForcedAge").unwrap_or(0);
    m.age_locked = crate::mob::entity_nbt::get_bool(tag, "AgeLocked").unwrap_or(false);
    m.in_love = int(tag, "InLove").unwrap_or(0);
    m.love_cause = crate::mob::nbt::uuid_key_of(tag, "LoveCause");
}

/// A UUID as an int array.
pub(crate) fn uuid_ints(uuid: u128) -> Vec<i32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let v = vec![
        (uuid >> 96) as i32,
        (uuid >> 64) as i32,
        (uuid >> 32) as i32,
        uuid as i32,
    ];
    v
}

/// An int (or byte/short) value.
pub(crate) fn int(tag: &Compound, key: &str) -> Option<i32> {
    crate::mob::entity_nbt::get_int(tag, key)
}

/// A boolean value.
pub(crate) fn boolean(tag: &Compound, key: &str, default: bool) -> bool {
    crate::mob::entity_nbt::get_bool(tag, key).unwrap_or(default)
}

/// A block position (`BlockPos.CODEC`: an int array of 3).
pub(crate) fn block_pos(tag: &Compound, key: &str) -> Option<BlockPos> {
    match tag.get(key)? {
        Tag::IntArray(v) if v.len() == 3 => Some(BlockPos::new(v[0], v[1], v[2])),
        _ => None,
    }
}

/// Writes a block position.
pub(crate) fn put_block_pos(out: &mut Compound, key: &str, p: BlockPos) {
    out.insert(key, vec![p.x, p.y, p.z]);
}

/// `BlockPos.closerToCenterThan(pos, dist)`.
pub(crate) fn closer_to_center(p: BlockPos, pos: Vec3, dist: f64) -> bool {
    let c = Vec3::new(
        f64::from(p.x) + 0.5,
        f64::from(p.y) + 0.5,
        f64::from(p.z) + 0.5,
    );
    c.distance_to_sqr(pos) < dist * dist
}

/// `Animal.isBrightEnoughToSpawn`: raw brightness above 8.
pub(crate) fn bright_enough(level: &Level, pos: BlockPos) -> bool {
    let (sky, block) = level.light(pos);
    sky.max(block) > 8
}

/// `Mob.checkMobSpawnRules`: the block below lets mobs spawn (or it is a spawner spawn).
pub(crate) fn mob_spawn_rules(level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    if reason == SpawnReason::Spawner {
        return true;
    }
    let below = level.block(BlockPos::new(pos.x, pos.y - 1, pos.z));
    below.misc().sturdy_full & 2 != 0 && below.light_emission() < 14
}

impl FamilyHooks for Hooks {
    /// `TropicalFish.isMaxGroupSizeReached`: a fish of a pattern that does not school ends
    /// its spawn group.
    fn is_max_group_size_reached(m: &MobData, _group_size: i32) -> bool {
        m.kind == Kind::TropicalFish && !st(m).is_school
    }

    type Goal = Goal;

    fn ageable(kind: Kind) -> bool {
        matches!(
            kind,
            Kind::Bee
                | Kind::Turtle
                | Kind::Mooshroom
                | Kind::Strider
                | Kind::Squid
                | Kind::GlowSquid
                | Kind::Dolphin
        )
    }

    fn is_monster(_kind: Kind) -> bool {
        false
    }

    fn ambient_sound_interval(kind: Kind) -> i32 {
        match kind {
            Kind::Turtle => 200,
            // An `AmbientCreature`, no `Animal`.
            Kind::Bat => 80,
            _ => 120,
        }
    }

    fn breathes_underwater(kind: Kind) -> bool {
        // `#can_breathe_under_water`.
        matches!(
            kind,
            Kind::Turtle
                | Kind::GlowSquid
                | Kind::Squid
                | Kind::Cod
                | Kind::Pufferfish
                | Kind::Salmon
                | Kind::TropicalFish
        )
    }

    fn sensitive_to_water(kind: Kind) -> bool {
        matches!(kind, Kind::SnowGolem | Kind::Strider)
    }

    fn fall_damage_immune(kind: Kind) -> bool {
        // `#fall_damage_immune`.
        matches!(
            kind,
            Kind::IronGolem | Kind::SnowGolem | Kind::Bat | Kind::Bee
        )
    }

    fn allowed_in_peaceful(_kind: Kind) -> bool {
        true
    }

    fn remove_when_far_away(kind: Kind) -> bool {
        // `Animal` and `AbstractGolem` stay; bats, squids, dolphins and (unnamed, wild) fish
        // despawn.
        matches!(
            kind,
            Kind::Bat | Kind::Squid | Kind::GlowSquid | Kind::Dolphin
        ) || is_fish(kind)
    }

    fn requires_custom_persistence(m: &MobData) -> bool {
        is_fish(m.kind) && (st(m).from_bucket || m.custom_name.is_some())
    }

    fn malus(kind: Kind) -> &'static MalusTable {
        const WATER: &MalusTable = &[(PathType::Water, 0.0)];
        const ANIMAL: &MalusTable = &[(PathType::FireInNeighbor, 16.0), (PathType::Fire, -1.0)];
        const TURTLE: &MalusTable = &[
            (PathType::FireInNeighbor, 16.0),
            (PathType::Fire, -1.0),
            (PathType::Water, 0.0),
            (PathType::DoorIronClosed, -1.0),
            (PathType::DoorWoodClosed, -1.0),
            (PathType::DoorOpen, -1.0),
        ];
        const STRIDER: &MalusTable = &[
            (PathType::Water, -1.0),
            (PathType::Lava, 0.0),
            (PathType::FireInNeighbor, 0.0),
            (PathType::Fire, 0.0),
        ];
        const BEE: &MalusTable = &[
            (PathType::FireInNeighbor, 16.0),
            (PathType::Fire, -1.0),
            (PathType::Water, -1.0),
            (PathType::WaterBorder, 16.0),
            (PathType::Cocoa, -1.0),
            (PathType::Fence, -1.0),
        ];
        match kind {
            Kind::Turtle => TURTLE,
            Kind::Strider => STRIDER,
            Kind::Bee => BEE,
            Kind::Mooshroom => ANIMAL,
            _ if is_water_animal(kind) => WATER,
            _ => &[],
        }
    }

    fn sound_source(_kind: Kind) -> SoundSource {
        SoundSource::Neutral
    }

    fn sound_volume(kind: Kind) -> f32 {
        match kind {
            Kind::Squid | Kind::GlowSquid | Kind::Bee | Kind::Mooshroom => 0.4,
            Kind::Bat => 0.1,
            _ => 1.0,
        }
    }

    fn ambient_sound(m: &mut MobData) -> Option<&'static str> {
        let s = st(m);
        match m.kind {
            Kind::Pufferfish | Kind::IronGolem | Kind::Bee => None,
            Kind::Mooshroom => Some("minecraft:entity.cow.ambient"),
            Kind::Dolphin => Some(if m.fluids.in_water() {
                "minecraft:entity.dolphin.ambient_water"
            } else {
                "minecraft:entity.dolphin.ambient"
            }),
            Kind::Turtle => (!m.fluids.in_water() && !m.baby && m.vertical_collision)
                .then_some("minecraft:entity.turtle.ambient_land"),
            Kind::Strider => (!crate::mob::animal::is_panicking(m) && !strider::is_tempted(m))
                .then_some("minecraft:entity.strider.ambient"),
            // `Bat.getAmbientSound`: a resting bat squeaks one time in four.
            Kind::Bat => {
                let resting = s.resting;
                (!resting || m.random.next_int_bounded(4) == 0)
                    .then_some("minecraft:entity.bat.ambient")
            }
            _ => Some(m.kind.default_sound(DefaultSound::Ambient)),
        }
    }

    fn hurt_sound(m: &MobData) -> &'static str {
        match m.kind {
            Kind::Pufferfish => "minecraft:entity.puffer_fish.hurt",
            Kind::Mooshroom => "minecraft:entity.cow.hurt",
            Kind::Turtle if m.baby => "minecraft:entity.turtle.hurt_baby",
            _ => m.kind.default_sound(DefaultSound::Hurt),
        }
    }

    fn death_sound(m: &MobData) -> &'static str {
        match m.kind {
            Kind::Pufferfish => "minecraft:entity.puffer_fish.death",
            Kind::Mooshroom => "minecraft:entity.cow.death",
            Kind::Turtle if m.baby => "minecraft:entity.turtle.death_baby",
            _ => m.kind.default_sound(DefaultSound::Death),
        }
    }

    fn experience_reward(m: &mut MobData) -> i32 {
        match m.kind {
            Kind::IronGolem | Kind::SnowGolem | Kind::Bat => 0,
            _ => 1 + m.random.next_int_bounded(3),
        }
    }

    fn size_scale(m: &MobData) -> Option<f32> {
        match m.kind {
            Kind::Salmon => {
                Some([0.5, 1.0, 1.5][usize::try_from(st(m).variant.clamp(0, 2)).unwrap_or(1)])
            }
            Kind::Pufferfish => Some(match st(m).puff_state {
                0 => 0.5,
                1 => 0.7,
                _ => 1.0,
            }),
            _ => None,
        }
    }

    fn dimensions(m: &MobData) -> Option<Dimensions> {
        let d = Dimensions::of(m.kind.type_id());
        let scaled = |f: f32| Dimensions {
            width: d.width * f,
            height: d.height * f,
        };
        let fixed = |width: f32, height: f32| Dimensions { width, height };
        Some(match m.kind {
            Kind::Salmon => {
                scaled([0.5, 1.0, 1.5][usize::try_from(st(m).variant.clamp(0, 2)).unwrap_or(1)])
            }
            Kind::Pufferfish => scaled(match st(m).puff_state {
                0 => 0.5,
                1 => 0.7,
                _ => 1.0,
            }),
            Kind::Squid | Kind::GlowSquid if m.baby => fixed(0.5, 0.5),
            Kind::Dolphin if m.baby => scaled(0.65),
            Kind::Turtle if m.baby => scaled(0.3),
            Kind::Mooshroom if m.baby => fixed(0.45, 0.7),
            Kind::Strider if m.baby => fixed(0.45, 0.85),
            _ if m.baby => scaled(0.5),
            _ => d,
        })
    }

    fn walk_target_value(kind: Kind, level: &Level, env: &Environment, pos: BlockPos) -> f64 {
        let light = || f64::from(crate::mob::light_magic_value(level, env, pos) - 0.5);
        let below = level.block(BlockPos::new(pos.x, pos.y - 1, pos.z));
        match kind {
            Kind::Bee => {
                if level.block(pos).is_air() {
                    10.0
                } else {
                    0.0
                }
            }
            Kind::Mooshroom => {
                if below.block() == vanillite_data::block::ids::MYCELIUM {
                    10.0
                } else {
                    -light()
                }
            }
            // `Turtle.getWalkTargetValue` needs the going-home state; see `turtle`.
            Kind::Turtle => turtle::walk_target_value(level, env, pos, false),
            // `Strider.getWalkTargetValue` (in lava: negative infinity off lava, see `randompos::walk_value`).
            Kind::Strider => {
                if crate::mob::is_lava(level.block(pos)) {
                    10.0
                } else {
                    0.0
                }
            }
            // `AgeableWaterCreature`s are no `Animal`s: `PathfinderMob.getWalkTargetValue`.
            Kind::Squid | Kind::GlowSquid | Kind::Dolphin => 0.0,
            _ if Self::ageable(kind) => {
                if below.block() == vanillite_data::block::ids::GRASS_BLOCK {
                    10.0
                } else {
                    -light()
                }
            }
            // `PathfinderMob.getWalkTargetValue`.
            _ => 0.0,
        }
    }

    fn check_spawn_rules(
        kind: Kind,
        level: &Level,
        env: &Environment,
        pos: BlockPos,
        random: &mut LegacyRandom,
        reason: SpawnReason,
    ) -> bool {
        match kind {
            Kind::Cod | Kind::Salmon | Kind::Pufferfish | Kind::Squid | Kind::Dolphin => {
                fish::surface_water_rules(level, pos)
            }
            Kind::TropicalFish => fish::tropical_rules(level, pos),
            Kind::GlowSquid => squid::glow_squid_rules(level, pos),
            Kind::Bat => bat::spawn_rules(level, env, pos, random, reason),
            Kind::Turtle => turtle::spawn_rules(level, pos),
            Kind::Mooshroom => {
                let below = level.block(BlockPos::new(pos.x, pos.y - 1, pos.z));
                block_tags().mooshrooms_spawnable_on.contains(below) && bright_enough(level, pos)
            }
            Kind::Strider => strider::spawn_rules(level, pos),
            Kind::Bee => {
                crate::mob::spawn::default_spawn_rules(kind, level, env, pos, random, reason)
            }
            _ => mob_spawn_rules(level, pos, reason),
        }
    }

    fn is_food(kind: Kind, item: u16) -> bool {
        let tag = match kind {
            Kind::Bee => "minecraft:bee_food",
            Kind::Turtle => "minecraft:turtle_food",
            Kind::Mooshroom => "minecraft:cow_food",
            Kind::Strider => "minecraft:strider_food",
            _ => return false,
        };
        item_in_tag(item, tag)
    }

    fn init(m: &mut MobData) {
        let mut s = State::default();
        if m.kind == Kind::Squid || m.kind == Kind::GlowSquid {
            // `Squid()`: the tentacle speed from the entity random.
            s.tentacle_speed = 1.0 / (m.random.next_float() + 1.0) * 0.2;
        }
        if m.kind == Kind::Bat {
            s.resting = true;
        }
        if m.kind == Kind::Salmon {
            s.variant = 1;
        }
        if m.kind == Kind::TropicalFish {
            s.variant = fish::DEFAULT_TROPICAL_VARIANT;
        }
        if m.kind == Kind::Dolphin {
            m.can_pick_up_loot = true;
        }
        m.ext = super::Ext::Wildlife(Box::new(s));
        use crate::mob::nav::{LookKind, MoveKind, NavKind};
        match m.kind {
            k if is_fish(k) => {
                m.nav.kind = NavKind::Water { breaching: false };
                m.move_ctl.kind = MoveKind::Custom;
            }
            Kind::Squid | Kind::GlowSquid => m.nav.kind = NavKind::None,
            Kind::Dolphin => {
                m.nav.kind = NavKind::Water { breaching: true };
                m.move_ctl.kind = MoveKind::SmoothSwimming {
                    max_turn_x: 85,
                    max_turn_y: 10,
                    in_water_speed: 0.02,
                    outside_water_speed: 0.1,
                    apply_gravity: true,
                };
                m.look.kind = LookKind::SmoothSwimming { max_y_rot: 10 };
            }
            Kind::Turtle => {
                m.nav.kind = NavKind::Amphibious;
                m.move_ctl.kind = MoveKind::Custom;
            }
            Kind::Bee => {
                m.nav.kind = NavKind::Flying;
                m.nav.variant = crate::mob::nav::NavVariant::Bee;
                let follow = m.attrs.follow_range;
                m.nav.set_required_path_length(48.0, follow);
                m.nav.can_open_doors = false;
                m.nav.can_float = false;
                m.move_ctl.kind = MoveKind::Flying {
                    max_turn: 20,
                    hovers: true,
                };
                m.look.kind = LookKind::Custom;
            }
            Kind::Bat => m.nav.kind = NavKind::None,
            _ => {}
        }
    }

    fn register_goals(m: &mut MobData) {
        match m.kind {
            k if is_fish(k) => fish::register_goals(m),
            Kind::Squid | Kind::GlowSquid => squid::register_goals(m),
            Kind::Dolphin => dolphin::register_goals(m),
            Kind::SnowGolem | Kind::IronGolem => golem::register_goals(m),
            Kind::Turtle => turtle::register_goals(m),
            Kind::Mooshroom => mooshroom::register_goals(m),
            Kind::Strider => strider::register_goals(m),
            Kind::Bee => bee::register_goals(m),
            _ => {}
        }
    }

    fn finalize_spawn_own(m: &mut MobData) {
        fish::finalize_spawn_own(m);
    }

    fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
        match m.kind {
            k if is_fish(k) => fish::finalize_spawn(m, ctx),
            Kind::Squid | Kind::GlowSquid => ageable_group_baby(m, ctx, 0.05),
            Kind::Dolphin => {
                m.air = 4800;
                ageable_group_baby(m, ctx, 0.1);
            }
            Kind::Turtle => {
                st_mut(m).home_pos = ctx.pos;
                ageable_group_baby(m, ctx, 0.05);
            }
            Kind::Strider => strider::finalize_spawn(m, ctx),
            Kind::Bee | Kind::Mooshroom => ageable_group_baby(m, ctx, 0.05),
            _ => {}
        }
        st_mut(m).was_baby = m.baby;
    }

    fn before_tick(t: &mut Tick<'_, '_>) {
        let air = t.mob_ref().air;
        st_mut(t.mob()).pre_air = air;
        match t.mob_ref().kind {
            Kind::Pufferfish => fish::puffer_tick(t),
            Kind::Strider => strider::before_tick(t),
            _ => {}
        }
    }

    fn after_base_tick(t: &mut Tick<'_, '_>) {
        let kind = t.mob_ref().kind;
        if is_fish(kind) || matches!(kind, Kind::Squid | Kind::GlowSquid) {
            handle_air_supply(t);
        } else if kind == Kind::Dolphin {
            // `increaseAirSupply` gives the full 4800 out of water.
            if !t.mob_ref().fluids.eye_in_water {
                t.mob().air = 4800;
            }
        } else if kind == Kind::IronGolem {
            // `decreaseAirSupply` keeps the air.
            let pre = st(t.mob_ref()).pre_air;
            let m = t.mob();
            m.air = m.air.max(pre);
        }
    }

    fn ai_step_start(t: &mut Tick<'_, '_>) {
        if is_fish(t.mob_ref().kind) {
            fish::flop(t);
        }
    }

    fn after_tick(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            Kind::Bat => bat::after_tick(t),
            Kind::Dolphin => dolphin::after_tick(t),
            Kind::Strider => strider::after_tick(t),
            k if is_fish(k) => fish::after_tick(t),
            _ => {}
        }
    }

    fn ai_step(t: &mut Tick<'_, '_>) {
        let grown = {
            let m = t.mob_ref();
            st(m).was_baby && !m.baby
        };
        let baby = t.mob_ref().baby;
        st_mut(t.mob()).was_baby = baby;
        match t.mob_ref().kind {
            Kind::Squid | Kind::GlowSquid => squid::ai_step(t),
            Kind::Pufferfish => fish::puffer_ai_step(t),
            Kind::SnowGolem => golem::snow_ai_step(t),
            Kind::IronGolem => golem::iron_ai_step(t),
            Kind::Turtle => turtle::ai_step(t, grown),
            Kind::Bee => bee::ai_step(t),
            _ => {}
        }
    }

    fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            Kind::Bat => bat::custom_server_ai_step(t),
            Kind::Bee => bee::custom_server_ai_step(t),
            _ => {}
        }
    }

    fn travel(t: &mut Tick<'_, '_>, input: Vec3) -> bool {
        let kind = t.mob_ref().kind;
        let in_water = t.mob_ref().fluids.in_water();
        match kind {
            Kind::Squid | Kind::GlowSquid => {
                // `Squid.travel`: it only moves by its own velocity.
                let v = t.velocity();
                crate::mob::living::move_self(t, v);
                true
            }
            k if in_water && is_fish(k) => {
                water_travel(t, input, 0.01, true);
                true
            }
            Kind::Dolphin if in_water => {
                let speed = t.mob_ref().speed;
                water_travel(t, input, speed, true);
                true
            }
            Kind::Turtle if in_water => {
                let s = st(t.mob_ref());
                let home = s.home_pos;
                let near_home = s.going_home && closer_to_center(home, t.pos(), 20.0);
                water_travel(t, input, 0.1, !near_home);
                true
            }
            _ => false,
        }
    }

    fn move_control(t: &mut Tick<'_, '_>) -> bool {
        match t.mob_ref().kind {
            k if is_fish(k) => fish::move_control(t),
            Kind::Turtle => turtle::move_control(t),
            _ => return false,
        }
        true
    }

    fn look_control(t: &mut Tick<'_, '_>) -> bool {
        if t.mob_ref().kind != Kind::Bee {
            return false;
        }
        // `BeeLookControl`: angry bees do not turn their heads; a pollinating bee keeps its
        // pitch (`resetXRotOnTick`).
        if !bee::is_angry(t) {
            let reset = !bee::is_pollinating(t.mob_ref());
            crate::mob::nav::default_look_tick(t, reset);
        }
        true
    }

    fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
        match t.mob_ref().kind {
            Kind::IronGolem => Some(golem::iron_hurt_target(t, target)),
            Kind::Bee => Some(bee::sting(t, target)),
            Kind::Dolphin => {
                let r = crate::mob::combat::base_mob_attack(t, target);
                if r {
                    crate::mob::kinds::play_sound(t, "minecraft:entity.dolphin.attack", 1.0, 1.0);
                }
                Some(r)
            }
            _ => None,
        }
    }

    fn perform_ranged_attack(t: &mut Tick<'_, '_>, target: EntityId, _power: f32) {
        if t.mob_ref().kind == Kind::SnowGolem {
            golem::snow_ranged_attack(t, target);
        }
    }

    fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
        let kind = h.ents.mob(h.target).map(|m| m.kind)?;
        match kind {
            Kind::Bat => {
                if let Some(m) = h.ents.mob_mut(h.target)
                    && st(m).resting
                {
                    st_mut(m).resting = false;
                    if let Some(mut e) = h.ents.store.get_mut(h.target) {
                        e.mark_data_dirty();
                    }
                }
            }
            Kind::Bee => {
                if let Some(m) = h.ents.mob_mut(h.target) {
                    st_mut(m).pollinating = false;
                }
            }
            _ => {}
        }
        Some(amount)
    }

    fn after_hurt(h: &mut Hurt<'_, '_>, amount: f32) {
        let Some(kind) = h.ents.mob(h.target).map(|m| m.kind) else {
            return;
        };
        match kind {
            Kind::Squid | Kind::GlowSquid => squid::after_hurt(h),
            Kind::IronGolem => golem::iron_after_hurt(h, amount),
            _ => {}
        }
    }

    fn thunder_hit(h: &mut Hurt<'_, '_>) -> bool {
        let Some(kind) = h.ents.mob(h.target).map(|m| m.kind) else {
            return false;
        };
        match kind {
            Kind::Mooshroom => {
                mooshroom::thunder_hit(h);
                true
            }
            Kind::Turtle => {
                // `Turtle.thunderHit`: the strike kills it.
                let source = DamageSource::of(h.env.types.lightning_bolt);
                crate::mob::combat::hurt(h.level, h.ents, h.env, h.cx, h.target, source, f32::MAX);
                true
            }
            _ => false,
        }
    }

    fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
        let Some(kind) = ix.ents.mob(ix.target).map(|m| m.kind) else {
            return Interaction::Pass;
        };
        match kind {
            k if is_fish(k) => fish::interact(ix),
            Kind::Dolphin => dolphin::interact(ix),
            Kind::SnowGolem => golem::snow_interact(ix),
            Kind::IronGolem => golem::iron_interact(ix),
            Kind::Mooshroom => mooshroom::interact(ix),
            Kind::Strider => strider::interact(ix),
            Kind::Bee => bee::interact(ix),
            Kind::Turtle => crate::mob::animal::animal_interact(
                ix.ents, ix.env, ix.player, ix.target, ix.hand, ix.stack,
            ),
            // `AgeableMob.mobInteract`: the golden dandelion.
            Kind::Squid | Kind::GlowSquid => {
                crate::mob::age_lock::interact_pub(ix).unwrap_or(Interaction::Pass)
            }
            _ => Interaction::Pass,
        }
    }

    fn breed_offspring(
        parent: &MobData,
        partner: &MobData,
        child: &mut MobData,
        random: &mut LegacyRandom,
    ) {
        if parent.kind == Kind::Mooshroom {
            mooshroom::offspring(parent, partner, child, random);
        }
    }

    fn encode_data(w: &mut DataWriter<'_>, m: &MobData) {
        use crate::player::encode::serializer;
        let s = st(m);
        if Self::ageable(m.kind) {
            w.boolean(16, m.baby, false);
            w.boolean(17, m.age_locked, false);
        }
        match m.kind {
            k if is_fish(k) => {
                w.boolean(16, s.from_bucket, false);
                match k {
                    Kind::Salmon => w.int(17, s.variant, 1),
                    Kind::Pufferfish => w.int(17, s.puff_state, 0),
                    Kind::TropicalFish => w.int(17, s.variant, fish::DEFAULT_TROPICAL_VARIANT),
                    _ => {}
                }
            }
            Kind::GlowSquid => w.int(18, s.dark_ticks, 0),
            Kind::Dolphin => {
                w.boolean(18, s.got_fish, false);
                w.int(19, s.moistness, 2400);
            }
            Kind::Bee => {
                w.byte(18, s.bee_flags, 0);
                w.raw(18 + 1, serializer::LONG, m.anger_end == -1, |o| {
                    write_varlong(o, m.anger_end);
                });
            }
            Kind::Turtle => {
                w.boolean(18, s.has_egg, false);
                w.boolean(19, s.laying_egg, false);
            }
            Kind::Mooshroom => w.int(18, s.variant, 0),
            Kind::Strider => {
                w.int(18, if m.boosting { m.boost_total } else { 0 }, 0);
                w.boolean(19, s.suffocating, false);
            }
            Kind::SnowGolem => w.byte(16, if s.pumpkin { 16 } else { 0 }, 16),
            Kind::IronGolem => w.byte(16, u8::from(s.player_created), 0),
            Kind::Bat => w.byte(16, u8::from(s.resting), 0),
            _ => {}
        }
    }

    fn save(m: &MobData, out: &mut Compound) {
        if Self::ageable(m.kind) {
            save_ageable(m, out);
            if !matches!(m.kind, Kind::Squid | Kind::GlowSquid | Kind::Dolphin) {
                save_love(m, out);
            }
        }
        let s = st(m);
        match m.kind {
            k if is_fish(k) => fish::save(m, out),
            Kind::GlowSquid => out.insert("DarkTicksRemaining", s.dark_ticks),
            Kind::Dolphin => {
                out.insert("GotFish", s.got_fish);
                out.insert("Moistness", s.moistness);
            }
            Kind::Bee => bee::save(m, out),
            Kind::Turtle => {
                put_block_pos(out, "home_pos", s.home_pos);
                out.insert("has_egg", s.has_egg);
            }
            Kind::Mooshroom => mooshroom::save(m, out),
            Kind::SnowGolem => out.insert("Pumpkin", s.pumpkin),
            Kind::IronGolem => {
                out.insert("PlayerCreated", s.player_created);
                crate::mob::common::save_anger(m, out);
            }
            Kind::Bat => out.insert("BatFlags", i8::from(s.resting)),
            _ => {}
        }
    }

    fn load(m: &mut MobData, tag: &Compound) {
        if Self::ageable(m.kind) {
            load_ageable(m, tag);
        }
        match m.kind {
            k if is_fish(k) => fish::load(m, tag),
            Kind::GlowSquid => st_mut(m).dark_ticks = int(tag, "DarkTicksRemaining").unwrap_or(0),
            Kind::Dolphin => {
                let s = st_mut(m);
                s.got_fish = boolean(tag, "GotFish", false);
                s.moistness = int(tag, "Moistness").unwrap_or(2400);
            }
            Kind::Bee => bee::load(m, tag),
            Kind::Turtle => {
                let home = block_pos(tag, "home_pos");
                let s = st_mut(m);
                if let Some(h) = home {
                    s.home_pos = h;
                }
                s.has_egg = boolean(tag, "has_egg", false);
            }
            Kind::Mooshroom => mooshroom::load(m, tag),
            Kind::SnowGolem => st_mut(m).pumpkin = boolean(tag, "Pumpkin", true),
            Kind::IronGolem => {
                st_mut(m).player_created = boolean(tag, "PlayerCreated", false);
                crate::mob::common::load_anger(m, tag, 0);
            }
            Kind::Bat => st_mut(m).resting = int(tag, "BatFlags").unwrap_or(0) & 1 != 0,
            _ => {}
        }
        let baby = m.baby;
        st_mut(m).was_baby = baby;
    }

    fn pick_up_items(t: &mut Tick<'_, '_>) {
        if t.mob_ref().kind == Kind::Dolphin {
            dolphin::pick_up_items(t);
        }
    }

    fn missile_tick(t: &mut MissileTick<'_, '_>) {
        snowball::tick(t);
    }

    fn missile_save(d: &MissileData, out: &mut Compound) {
        match d.kind {
            MissileKind::FireworkRocket => crate::firework::save(d, out),
            MissileKind::FishingBobber => crate::fishing::save(d, out),
            _ => {}
        }
    }

    fn missile_load(d: &mut MissileData, tag: &Compound) {
        match d.kind {
            MissileKind::FireworkRocket => crate::firework::load(d, tag),
            MissileKind::FishingBobber => crate::fishing::load(d, tag),
            _ => {}
        }
    }

    fn missile_encode(w: &mut DataWriter<'_>, d: &MissileData) {
        match d.kind {
            MissileKind::FireworkRocket => crate::firework::encode(w, d),
            MissileKind::FishingBobber => crate::fishing::encode(w, d),
            // `ThrowableItemProjectile.DATA_ITEM_STACK`, `EyeOfEnder.DATA_ITEM_STACK`.
            _ => w.stack(8, &d.item),
        }
    }

    fn goal_flags(g: &Goal) -> u8 {
        use flag::{JUMP, LOOK, MOVE};
        match g {
            Goal::Swim { .. }
            | Goal::DolphinMoveToItem { .. }
            | Goal::DolphinPlayWithItems { .. }
            | Goal::MoveTowardsTarget { .. }
            | Goal::TurtlePanic { .. }
            | Goal::TurtleStroll { .. }
            | Goal::BeePollinate { .. }
            | Goal::BeeWander
            | Goal::BeeGoToKnownFlower { .. }
            | Goal::BeeGoToHive { .. }
            | Goal::GolemStroll { .. } => MOVE,
            Goal::OfferFlower { .. } => MOVE | LOOK,
            Goal::DefendVillage { .. } => flag::TARGET,
            Goal::DolphinSwimToTreasure { .. }
            | Goal::DolphinSwimWithPlayer { .. }
            | Goal::BreathAir
            | Goal::TurtleBreed { .. } => MOVE | LOOK,
            Goal::DolphinJump { .. }
            | Goal::TurtleLayEgg(_)
            | Goal::TurtleGoToWater(_)
            | Goal::StriderGoToLava(_) => MOVE | JUMP,
            _ => 0,
        }
    }

    fn goal_every_tick(g: &Goal) -> bool {
        matches!(
            g,
            Goal::SquidFlee { .. }
                | Goal::TurtleLayEgg(_)
                | Goal::TurtleGoToWater(_)
                | Goal::StriderGoToLava(_)
                | Goal::BeePollinate { .. }
        )
    }

    fn goal_can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
        match g {
            Goal::Idle => false,
            Goal::Swim { .. } | Goal::FollowFlockLeader { .. } | Goal::Puff => fish::can_use(t, g),
            Goal::SquidRandomMovement | Goal::SquidFlee { .. } => squid::can_use(t, g),
            Goal::TryFindWater
            | Goal::BreathAir
            | Goal::DolphinSwimToTreasure { .. }
            | Goal::DolphinSwimWithPlayer { .. }
            | Goal::DolphinJump { .. }
            | Goal::DolphinMoveToItem { .. }
            | Goal::DolphinPlayWithItems { .. } => dolphin::can_use(t, g),
            Goal::MoveTowardsTarget { .. }
            | Goal::GolemStroll { .. }
            | Goal::OfferFlower { .. }
            | Goal::DefendVillage { .. } => golem::can_use(t, g),
            Goal::TurtlePanic { .. }
            | Goal::TurtleBreed { .. }
            | Goal::TurtleLayEgg(_)
            | Goal::TurtleGoToWater(_)
            | Goal::TurtleGoHome { .. }
            | Goal::TurtleTravel { .. }
            | Goal::TurtleStroll { .. } => turtle::can_use(t, g),
            Goal::StriderGoToLava(_) => strider::can_use(t, g),
            Goal::BeePollinate { .. }
            | Goal::BeeGrowCrop
            | Goal::BeeWander
            | Goal::BeeGoToKnownFlower { .. }
            | Goal::BeeValidateFlower { .. }
            | Goal::BeeValidateHive { .. }
            | Goal::BeeEnterHive
            | Goal::BeeLocateHive
            | Goal::BeeGoToHive { .. } => bee::can_use(t, g),
        }
    }

    fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
        match g {
            Goal::Idle => false,
            Goal::Swim { .. } | Goal::FollowFlockLeader { .. } | Goal::Puff => {
                fish::can_continue(t, g)
            }
            Goal::SquidRandomMovement | Goal::SquidFlee { .. } => squid::can_use(t, g),
            Goal::TryFindWater
            | Goal::BreathAir
            | Goal::DolphinSwimToTreasure { .. }
            | Goal::DolphinSwimWithPlayer { .. }
            | Goal::DolphinJump { .. }
            | Goal::DolphinMoveToItem { .. }
            | Goal::DolphinPlayWithItems { .. } => dolphin::can_continue(t, g),
            Goal::MoveTowardsTarget { .. }
            | Goal::GolemStroll { .. }
            | Goal::OfferFlower { .. }
            | Goal::DefendVillage { .. } => golem::can_continue(t, g),
            Goal::TurtlePanic { .. }
            | Goal::TurtleBreed { .. }
            | Goal::TurtleLayEgg(_)
            | Goal::TurtleGoToWater(_)
            | Goal::TurtleGoHome { .. }
            | Goal::TurtleTravel { .. }
            | Goal::TurtleStroll { .. } => turtle::can_continue(t, g),
            Goal::StriderGoToLava(_) => strider::can_continue(t, g),
            Goal::BeePollinate { .. }
            | Goal::BeeGrowCrop
            | Goal::BeeWander
            | Goal::BeeGoToKnownFlower { .. }
            | Goal::BeeValidateFlower { .. }
            | Goal::BeeValidateHive { .. }
            | Goal::BeeEnterHive
            | Goal::BeeLocateHive
            | Goal::BeeGoToHive { .. } => bee::can_continue(t, g),
        }
    }

    fn goal_start(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::Swim { .. } | Goal::FollowFlockLeader { .. } | Goal::Puff => fish::start(t, g),
            Goal::SquidFlee { ticks } => *ticks = 0,
            Goal::TryFindWater
            | Goal::BreathAir
            | Goal::DolphinSwimToTreasure { .. }
            | Goal::DolphinSwimWithPlayer { .. }
            | Goal::DolphinJump { .. }
            | Goal::DolphinMoveToItem { .. }
            | Goal::DolphinPlayWithItems { .. } => dolphin::start(t, g),
            Goal::MoveTowardsTarget { .. }
            | Goal::GolemStroll { .. }
            | Goal::OfferFlower { .. }
            | Goal::DefendVillage { .. } => golem::start(t, g),
            Goal::TurtlePanic { .. }
            | Goal::TurtleBreed { .. }
            | Goal::TurtleLayEgg(_)
            | Goal::TurtleGoToWater(_)
            | Goal::TurtleGoHome { .. }
            | Goal::TurtleTravel { .. }
            | Goal::TurtleStroll { .. } => turtle::start(t, g),
            Goal::StriderGoToLava(s) => s.start(t),
            Goal::BeePollinate { .. }
            | Goal::BeeGrowCrop
            | Goal::BeeWander
            | Goal::BeeGoToKnownFlower { .. }
            | Goal::BeeValidateFlower { .. }
            | Goal::BeeValidateHive { .. }
            | Goal::BeeEnterHive
            | Goal::BeeLocateHive
            | Goal::BeeGoToHive { .. } => bee::start(t, g),
            _ => {}
        }
    }

    fn goal_stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::FollowFlockLeader { .. } | Goal::Puff => fish::stop(t, g),
            Goal::DolphinSwimToTreasure { .. }
            | Goal::DolphinSwimWithPlayer { .. }
            | Goal::DolphinJump { .. }
            | Goal::DolphinMoveToItem { .. }
            | Goal::DolphinPlayWithItems { .. } => dolphin::stop(t, g),
            Goal::MoveTowardsTarget { target, .. } => *target = None,
            Goal::GolemStroll { .. } | Goal::OfferFlower { .. } | Goal::DefendVillage { .. } => {
                golem::stop(t, g);
            }
            Goal::TurtleGoHome { .. } | Goal::TurtleTravel { .. } | Goal::TurtleBreed { .. } => {
                turtle::stop(t, g);
            }
            Goal::BeePollinate { .. }
            | Goal::BeeGoToKnownFlower { .. }
            | Goal::BeeGoToHive { .. } => bee::stop(t, g),
            _ => {}
        }
    }

    fn goal_tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::FollowFlockLeader { .. } => fish::tick(t, g),
            Goal::SquidRandomMovement | Goal::SquidFlee { .. } => squid::tick(t, g),
            Goal::TryFindWater
            | Goal::BreathAir
            | Goal::DolphinSwimToTreasure { .. }
            | Goal::DolphinSwimWithPlayer { .. }
            | Goal::DolphinJump { .. }
            | Goal::DolphinMoveToItem { .. }
            | Goal::DolphinPlayWithItems { .. } => dolphin::tick(t, g),
            Goal::TurtleBreed { .. }
            | Goal::TurtleLayEgg(_)
            | Goal::TurtleGoToWater(_)
            | Goal::TurtleGoHome { .. }
            | Goal::TurtleTravel { .. } => turtle::tick(t, g),
            Goal::StriderGoToLava(s) => {
                let target = s.block_pos;
                s.tick_with(t, target, 1.0, 20);
            }
            Goal::BeePollinate { .. }
            | Goal::BeeGrowCrop
            | Goal::BeeGoToKnownFlower { .. }
            | Goal::BeeGoToHive { .. } => bee::tick(t, g),
            Goal::OfferFlower { .. } => golem::tick(t, g),
            _ => {}
        }
    }
}

/// `AgeableMob.finalizeSpawn` with `AgeableMobGroupData(babyChance)`: later members of a group
/// may be babies.
pub(crate) fn ageable_group_baby(m: &mut MobData, ctx: &SpawnContext<'_>, chance: f32) {
    if ctx.group_index > 0 && m.random.next_float() <= chance {
        m.age = -24000;
        m.baby = true;
    }
}

/// `WaterAnimal.handleAirSupply`: out of water the air runs out, in water it refills.
fn handle_air_supply(t: &mut Tick<'_, '_>) {
    let pre = st(t.mob_ref()).pre_air;
    if t.mob_ref().is_alive() && !t.mob_ref().fluids.in_water() {
        t.mob().air = pre - 1;
        if t.mob_ref().air <= -20 {
            t.mob().air = 0;
            let source = DamageSource::of(t.env.types.drown);
            crate::mob::living::hurt_self(t, source, 2.0);
        }
    } else {
        t.mob().air = 300;
    }
}

/// `travelInWater` of fish, dolphins and turtles: `moveRelative(speed)`, move, slow down by
/// 0.9 and sink a little without a target (`sink`).
pub(crate) fn water_travel(t: &mut Tick<'_, '_>, input: Vec3, speed: f32, sink: bool) {
    crate::mob::living::move_relative(t, speed, input);
    let v = t.velocity();
    crate::mob::living::move_self(t, v);
    if !t.exists() {
        return;
    }
    let mut v = t.velocity() * 0.9;
    if t.mob_ref().target.is_none() && sink {
        v = v + Vec3::new(0.0, -0.005, 0.0);
    }
    t.set_velocity(v);
}

/// Writes a var long (`LONG` serializer).
fn write_varlong(out: &mut Vec<u8>, value: i64) {
    #[allow(clippy::cast_sign_loss)]
    let mut v = value as u64;
    loop {
        if v & !0x7F == 0 {
            #[allow(clippy::cast_possible_truncation)]
            out.push(v as u8);
            return;
        }
        #[allow(clippy::cast_possible_truncation)]
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
}

/// `MoveControl.rotlerp` re-export for the kinds' own controls.
pub(crate) use crate::mob::nav::rotlerp;
