//! Mobs and animals (`Mob`, `PathfinderMob`, `Monster`, `Animal`): their kind data, the
//! living entity tick in Vanilla's order (`Entity.baseTick`, `LivingEntity.aiStep`,
//! `Mob.serverAiStep`), movement physics, path finding, goals, combat and natural spawning.
//!
//! A mob tick works on the world's [`Entities`] taken out of the [`Level`] (the level stays
//! readable for blocks and light); what changes blocks (explosions, a sheep eating grass) is
//! queued as a [`WorldAction`] and applied by the world right after that mob's tick.

pub mod activation;
pub mod age_lock;
pub mod animal;
pub mod attack;
pub mod boss;
pub mod brain;
pub mod combat;
pub mod common;
pub mod entity_nbt;
pub mod equipment;
pub mod fam;
pub mod goals;
pub mod golem_build;
pub mod item_components;
pub mod kinds;
pub mod living;
pub mod missile;
pub mod movement;
pub mod nav;
pub mod nbt;
pub mod nearby;
pub mod path;
pub mod physics;
pub mod projectile;
pub mod randompos;
pub mod spawn;
pub mod structures;
pub mod tame;
pub mod teleport;

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{Aabb, BlockPos, ChunkPos, Vec3};
use vanillite_data::block::BlockState;
use vanillite_data::entity::{ATTRIBUTES, EntityTypeId};
use vanillite_world::entity::{
    EntityFlags, EntityId, EntityMut, EntityRef, NetworkIds, NewEntity, Rotation,
};

use crate::entities::Entities;
use crate::level::Level;
use crate::survival::blocks::FluidContact;
use crate::survival::damage::DamageKind;
use crate::survival::player::Env;

pub use fam::Family;
pub use kinds::Kind;
pub use vanillite_world::entity::Dimensions;

/// The attribute values a mob uses (`AttributeMap` with the base values and the spawn
/// modifiers folded in).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attrs {
    pub max_health: f64,
    pub movement_speed: f64,
    pub flying_speed: f64,
    pub follow_range: f64,
    pub attack_damage: f64,
    pub attack_knockback: f64,
    pub armor: f64,
    pub armor_toughness: f64,
    pub knockback_resistance: f64,
    pub step_height: f64,
    pub jump_strength: f64,
    pub gravity: f64,
    pub safe_fall_distance: f64,
    pub fall_damage_multiplier: f64,
    pub water_movement_efficiency: f64,
    pub movement_efficiency: f64,
    pub friction_modifier: f64,
    pub air_drag_modifier: f64,
    pub explosion_knockback_resistance: f64,
}

impl Attrs {
    /// The default attribute values of an entity type.
    pub fn of(kind: EntityTypeId) -> Attrs {
        let info = kind.info();
        let value = |name: &str| -> f64 {
            let Some(id) = ATTRIBUTES.iter().position(|a| a.name == name) else {
                return 0.0;
            };
            info.attributes
                .iter()
                .find(|(a, _)| usize::from(*a) == id)
                .map_or(ATTRIBUTES[id].default, |&(_, v)| v)
        };
        Attrs {
            max_health: value("minecraft:max_health"),
            movement_speed: value("minecraft:movement_speed"),
            flying_speed: value("minecraft:flying_speed"),
            follow_range: value("minecraft:follow_range"),
            attack_damage: value("minecraft:attack_damage"),
            attack_knockback: value("minecraft:attack_knockback"),
            armor: value("minecraft:armor"),
            armor_toughness: value("minecraft:armor_toughness"),
            knockback_resistance: value("minecraft:knockback_resistance"),
            step_height: value("minecraft:step_height"),
            jump_strength: value("minecraft:jump_strength"),
            gravity: value("minecraft:gravity"),
            safe_fall_distance: value("minecraft:safe_fall_distance"),
            fall_damage_multiplier: value("minecraft:fall_damage_multiplier"),
            water_movement_efficiency: value("minecraft:water_movement_efficiency"),
            movement_efficiency: value("minecraft:movement_efficiency"),
            friction_modifier: value("minecraft:friction_modifier"),
            air_drag_modifier: value("minecraft:air_drag_modifier"),
            explosion_knockback_resistance: value("minecraft:explosion_knockback_resistance"),
        }
    }
}

/// The kind data of a mob (`Dense<MobData>`).
///
/// This is the record every mob carries: what every living mob ticks, plus the layers several
/// families share (breeding, taming, steering, anger, equipment). State that only one kind or
/// one family has lives in [`MobData::ext`] (a family's state, [`fam::classic`] for the kinds
/// without a family) or [`MobData::boss`], never here: a new kind adds its fields there, and
/// the test `mob_data_size` keeps this record from growing unnoticed.
#[derive(Clone, Debug)]
pub struct MobData {
    pub kind: Kind,
    /// The entity's own random (`Entity.random`).
    pub random: LegacyRandom,
    pub attrs: Attrs,
    pub health: f32,
    pub tick_count: i32,
    pub hurt_time: i32,
    /// `invulnerableTime` / `damageCooldownTime`.
    pub damage_cooldown: i32,
    pub last_hurt: f32,
    pub death_time: i32,
    pub dead: bool,
    /// Removed during its own tick (`isRemoved`); the world despawns it after the tick.
    pub removed: bool,
    pub no_action_time: i32,
    pub fall_distance: f64,
    pub fire_ticks: i32,
    pub air: i32,
    /// `getTicksFrozen` (`DATA_TICKS_FROZEN`).
    pub ticks_frozen: i32,
    /// `isInPowderSnow` (set by powder snow this tick) and `wasInPowderSnow`.
    pub in_powder_snow: bool,
    pub was_in_powder_snow: bool,
    pub y_body_rot: f32,
    pub speed: f32,
    pub xxa: f32,
    pub yya: f32,
    pub zza: f32,
    pub jumping: bool,
    pub no_jump_delay: i32,
    pub horizontal_collision: bool,
    pub vertical_collision: bool,
    pub fluids: FluidContact,
    pub stuck_speed: Vec3,
    /// The last damage and the game time it was taken (`lastDamageSource`, `lastDamageStamp`).
    pub last_damage: Option<(DamageKind, i64)>,
    /// The causing entity of the last damage (`lastDamageSource.getEntity()`).
    pub last_damage_cause: Option<EntityId>,
    pub last_hurt_by_player: Option<EntityId>,
    pub last_hurt_by_player_time: i32,
    pub last_hurt_by_mob: Option<EntityId>,
    pub last_hurt_by_mob_time: i32,
    pub last_hurt_mob: Option<EntityId>,
    pub persistent: bool,
    pub no_ai: bool,
    pub silent: bool,
    pub ambient_sound_time: i32,
    pub target: Option<EntityId>,
    pub aggressive: bool,
    pub goals: goals::GoalSelector,
    pub targets: goals::GoalSelector,
    pub nav: nav::Navigation,
    pub move_ctl: nav::MoveControl,
    pub look: nav::LookControl,
    pub jump: bool,
    pub body: nav::BodyRotation,
    /// `Sensing`: line of sight results of this tick.
    pub seen: Vec<EntityId>,
    pub unseen: Vec<EntityId>,
    /// `AgeableMob.age`: negative for babies.
    pub age: i32,
    pub in_love: i32,
    pub baby: bool,
    /// `AgeableMob.AGE_LOCKED` (golden dandelion) and `ageLockParticleTimer`.
    pub age_locked: bool,
    pub age_lock_timer: u8,
    /// Ticks the mob draws its bow (`useItemRemaining` counted up), `-1` when not using it.
    pub using_item: i32,
    /// `Entity.lastKnownPosition` (`None` before the first tick).
    pub last_known_pos: Option<Vec3>,
    /// `Entity.lastKnownSpeed`: the movement between the starts of the last two ticks.
    pub known_speed: Vec3,
    /// `recentKineticEnemies`: the entities a kinetic weapon in use hit, with the game time.
    pub stabbed: Vec<(EntityId, i64)>,
    pub sheared: bool,
    pub climbing: bool,
    /// The path a melee goal computed in `canUse` for its `start`.
    pub goal_path: Option<path::Path>,
    /// Index into the `minecraft:*_variant` registry (pigs, cows, chickens); 0 = default.
    pub variant: i32,
    /// Index into the `minecraft:{pig,cow,chicken}_sound_variant` registry (`classic` by
    /// default, see [`kinds::classic_sound_variant`]).
    pub sound_variant: u8,
    pub xp_reward: i32,
    /// The last block position (`lastPos`).
    pub last_pos: BlockPos,
    /// `activeEffects` with their attribute modifiers.
    pub effects: crate::effect::ActiveEffects,
    /// `getAbsorptionAmount`.
    pub absorption: f32,
    /// The attribute values without effect modifiers while modifiers are active; `attrs`
    /// holds the modified values then.
    pub base_attrs: Option<Box<Attrs>>,
    /// Wears a saddle (`EquipmentSlot.SADDLE`).
    pub saddled: bool,
    /// `ItemBasedSteering`: boosting, the ticks boosted and the boost length
    /// (`DATA_BOOST_TIME`).
    pub boosting: bool,
    pub boost_time: i32,
    pub boost_total: i32,
    /// `getCustomName`, encoded as the `custom_name` component (network NBT).
    pub custom_name: Option<Box<[u8]>>,
    /// `TamableAnimal`: the owner's UUID, tame, ordered to sit, sitting pose.
    pub owner: Option<u128>,
    pub tame: bool,
    pub order_to_sit: bool,
    pub in_sitting_pose: bool,
    /// `loveCause`: the player who fed it.
    pub love_cause: Option<u128>,
    /// `forcedAge` and `forcedAgeTimer` (babies fed to grow).
    pub forced_age: i32,
    pub forced_age_timer: i32,
    /// A leash read from NBT, attached once its holder is there.
    pub pending_leash: Option<LeashRef>,
    /// The equipment slots and their drop chances.
    pub equipment: equipment::Equipment,
    /// `canPickUpLoot`.
    pub can_pick_up_loot: bool,
    /// `isLeftHanded`.
    pub left_handed: bool,
    /// The amount of the `random_spawn_bonus` follow range modifier `Mob.finalizeSpawn` added
    /// (folded into `attrs.follow_range`; 0 without one), for the saved `attributes`.
    pub spawn_bonus: f64,
    /// `PathfinderMob.restrictTo`: the home position and radius (`-1` = none).
    pub restrict_center: BlockPos,
    pub restrict_radius: i32,
    /// The brain of brain AI mobs (`Brain`), `None` for goal AI mobs.
    pub brain: Option<Box<brain::Brain>>,
    /// The state of a family kind.
    pub ext: fam::Ext,
    /// `Zombie.canBreakDoors` (and the other door breakers).
    pub can_break_doors: bool,
    /// `NeutralMob`: the game time the anger ends (`-1` = none) and whom it is angry at.
    pub anger_end: i64,
    pub anger_target: Option<u128>,
    /// `discardFriction` (long jumps of brain mobs).
    pub discard_friction: bool,
    /// `moveDist` and `nextStep`: when the next step makes a vibration.
    pub move_dist: f32,
    pub next_step: f32,
    /// `flyDist` and `nextFlap` (chickens and parrots flap when the flight passes it).
    pub fly_dist: f32,
    pub next_flap: f32,
    /// `crystalSoundIntensity` and `lastCrystalSoundPlayTick` (the amethyst chime of steps).
    pub crystal_intensity: f32,
    pub last_crystal_tick: i32,
    /// The state of bosses and the other kinds with their own machinery.
    pub boss: boss::Ext,
    /// The last world tick the mob counts as active for `performance.activation-range` (in
    /// range of a player, immune or woken up; see [`activation`]). Not saved.
    pub active_until: u64,
}

/// What holds a leash in entity NBT (`leash`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeashRef {
    Entity(u128),
    Knot(BlockPos),
}

/// `LivingEntity`'s constructor yaw: `random.nextFloat() * (float) (Math.PI * 2)`.
const CONSTRUCTOR_YAW_SCALE: f32 = std::f32::consts::PI * 2.0;

/// The yaw (and head yaw) `LivingEntity`'s constructor gives a mob created with `seed`: a
/// mob placed with `snapTo(pos, yRot, xRot)` keeps it as its head yaw.
pub fn constructor_y_rot(seed: i64) -> f32 {
    LegacyRandom::new(seed).next_float() * CONSTRUCTOR_YAW_SCALE
}

impl MobData {
    /// A fresh mob of `kind` (`EntityType.create` without `finalizeSpawn`).
    pub fn new(kind: Kind, seed: i64) -> MobData {
        let attrs = Attrs::of(kind.type_id());
        #[allow(clippy::cast_possible_truncation)]
        let health = attrs.max_health as f32;
        let mut mob = MobData {
            kind,
            random: LegacyRandom::new(seed),
            attrs,
            health,
            tick_count: 0,
            hurt_time: 0,
            damage_cooldown: 0,
            last_hurt: 0.0,
            death_time: 0,
            dead: false,
            removed: false,
            no_action_time: 0,
            fall_distance: 0.0,
            fire_ticks: 0,
            air: 300,
            ticks_frozen: 0,
            in_powder_snow: false,
            was_in_powder_snow: false,
            y_body_rot: 0.0,
            speed: 0.0,
            xxa: 0.0,
            yya: 0.0,
            zza: 0.0,
            jumping: false,
            no_jump_delay: 0,
            horizontal_collision: false,
            vertical_collision: false,
            fluids: FluidContact::default(),
            stuck_speed: Vec3::ZERO,
            last_damage: None,
            last_damage_cause: None,
            last_hurt_by_player: None,
            last_hurt_by_player_time: 0,
            last_hurt_by_mob: None,
            last_hurt_by_mob_time: 0,
            last_hurt_mob: None,
            persistent: false,
            no_ai: false,
            silent: false,
            ambient_sound_time: 0,
            target: None,
            aggressive: false,
            goals: goals::GoalSelector::default(),
            targets: goals::GoalSelector::default(),
            nav: nav::Navigation::new(attrs.follow_range),
            move_ctl: nav::MoveControl::default(),
            look: nav::LookControl::default(),
            jump: false,
            body: nav::BodyRotation::default(),
            seen: Vec::new(),
            unseen: Vec::new(),
            age: 0,
            in_love: 0,
            baby: false,
            age_locked: false,
            age_lock_timer: 0,
            using_item: -1,
            last_known_pos: None,
            known_speed: Vec3::ZERO,
            stabbed: Vec::new(),
            sheared: false,
            climbing: false,
            goal_path: None,
            // Chickens, cows and pigs start as the temperate variant (registry index 1).
            variant: i32::from(matches!(kind, Kind::Chicken | Kind::Cow | Kind::Pig)),
            sound_variant: kinds::classic_sound_variant(kind),
            xp_reward: 0,
            effects: crate::effect::ActiveEffects::default(),
            absorption: 0.0,
            base_attrs: None,
            last_pos: BlockPos::new(i32::MIN, i32::MIN, i32::MIN),
            saddled: false,
            boosting: false,
            boost_time: 0,
            boost_total: 0,
            custom_name: None,
            owner: None,
            tame: false,
            order_to_sit: false,
            in_sitting_pose: false,
            love_cause: None,
            forced_age: 0,
            forced_age_timer: 0,
            pending_leash: None,
            equipment: equipment::Equipment::default(),
            can_pick_up_loot: false,
            left_handed: false,
            spawn_bonus: 0.0,
            restrict_center: BlockPos::new(0, 0, 0),
            restrict_radius: -1,
            brain: None,
            ext: fam::classic::new_ext(kind).unwrap_or_default(),
            can_break_doors: false,
            anger_end: -1,
            anger_target: None,
            discard_friction: false,
            move_dist: 0.0,
            next_step: 1.0,
            fly_dist: 0.0,
            next_flap: 1.0,
            crystal_intensity: 0.0,
            last_crystal_tick: 0,
            boss: boss::new_ext(kind),
            active_until: 0,
        };
        // `Mob.ambientSoundTime` starts at 0; only playing a sound resets it to `-interval`.
        // `LivingEntity`'s constructor turns the entity to a random yaw (`setYRot(random.nextFloat()
        // * 2 PI)`); spawning sets the real rotation afterwards, but the draw stays, and the
        // cubes' move control keeps a direction from it.
        let y_rot = mob.random.next_float() * CONSTRUCTOR_YAW_SCALE;
        // The draws of field initializers (`eggTime`, `idleAnimationTimeout`).
        match kind {
            Kind::Chicken => {
                let egg_time = mob.random.next_int_bounded(6000) + 6000;
                fam::classic::chicken_mut(&mut mob).egg_time = egg_time;
            }
            Kind::Rabbit => {
                let _ = mob.random.next_int_bounded(40);
            }
            // `Guardian`: `clientSideTailAnimation = random.nextFloat()`.
            Kind::Guardian | Kind::ElderGuardian => {
                let _ = mob.random.next_float();
            }
            _ => {}
        }
        kinds::register_goals(&mut mob);
        fam::undead::cubes::construct_move_yaw(&mut mob, y_rot);
        mob
    }

    /// `getMaxHealth`.
    pub fn max_health(&self) -> f32 {
        #[allow(clippy::cast_possible_truncation)]
        let m = self.attrs.max_health as f32;
        m
    }

    /// `isDeadOrDying`.
    pub fn is_dead_or_dying(&self) -> bool {
        self.health <= 0.0
    }

    /// `hasRestriction`.
    pub fn has_restriction(&self) -> bool {
        self.restrict_radius != -1
    }

    /// `isWithinRestriction(pos)`.
    pub fn is_within_restriction(&self, pos: BlockPos) -> bool {
        if self.restrict_radius == -1 {
            return true;
        }
        let r = f64::from(self.restrict_radius);
        self.restrict_center.dist_sqr(pos) < r * r
    }

    /// `getSoundSource`: the kind's, but `HOSTILE` for the killer bunny (`Rabbit`).
    pub fn sound_source(&self) -> vanillite_protocol::packets::play::clientbound::SoundSource {
        if self.kind == Kind::Rabbit && self.variant == fam::companion::rabbit::EVIL {
            return vanillite_protocol::packets::play::clientbound::SoundSource::Hostile;
        }
        self.kind.sound_source()
    }

    /// `isAlive`.
    pub fn is_alive(&self) -> bool {
        !self.removed && self.health > 0.0
    }

    /// `isOnFire` on the server: burning ticks left.
    pub fn is_on_fire(&self) -> bool {
        self.fire_ticks > 0
    }

    /// `getLastDamageSource`: forgotten 40 ticks after the hit.
    pub fn last_damage_source(&self, game_time: i64) -> Option<DamageKind> {
        self.last_damage
            .filter(|&(_, at)| game_time - at <= 40)
            .map(|(k, _)| k)
    }
}

/// The parts of the time of day and the weather mobs react to, for one world tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Environment {
    /// `Level.getSkyDarken()`.
    pub sky_darken: i32,
    /// `EnvironmentAttributes.MONSTERS_BURN`.
    pub monsters_burn: bool,
    /// `isThundering`.
    pub thundering: bool,
    /// `isRaining`.
    pub raining: bool,
    /// The dimension has sky light.
    pub has_sky_light: bool,
    /// `monster_spawn_block_light_limit` of the dimension type.
    pub monster_block_light_limit: i32,
    /// `monster_spawn_light_level` of the dimension type as an inclusive range.
    pub monster_light_test: (i32, i32),
    /// `DimensionType.ambientLight`.
    pub ambient_light: f32,
    /// `DimensionType.hasCeiling`.
    pub has_ceiling: bool,
    /// `EnvironmentAttributes.SURFACE_SLIME_SPAWN_CHANCE` (the moon's brightness / 2 in the
    /// overworld).
    pub surface_slime_chance: f32,
    /// The world seed (slime chunks).
    pub world_seed: i64,
    /// The days of the overworld clock (`Timelines.OVERWORLD_DAY` period count).
    pub day_count: i64,
    /// `getSeaLevel`.
    pub sea_level: i32,
    /// The time of the overworld day (ticks into the `minecraft:overworld` clock's day): the
    /// universal timelines (the villager schedules) follow it in every dimension.
    pub day_time: i64,
    /// `EnvironmentAttributes.BEES_STAY_IN_HIVE`: the night of the `minecraft:day` timeline or
    /// rain (any rain level).
    pub bees_stay_in_hive: bool,
    /// `EnvironmentAttributes.CREAKING_ACTIVE` (the overworld's `day` timeline at night).
    pub creaking_active: bool,
    /// `EnvironmentAttributes.TURTLE_EGG_HATCH_CHANCE` (the `day` timeline in the overworld).
    pub turtle_egg_hatch_chance: f32,
    /// `EnvironmentAttributes.CAT_WAKING_UP_GIFT_CHANCE` (the `day` timeline in the overworld).
    pub cat_waking_up_gift_chance: f32,
    /// `EnvironmentAttributes.SUN_ANGLE` in degrees (the `day` timeline in the overworld, 0
    /// elsewhere).
    pub sun_angle: f32,
    /// `EnvironmentAttributes.EYEBLOSSOM_OPEN`: `None` where no timeline sets it (eyeblossoms
    /// keep their state).
    pub eyeblossom_open: Option<bool>,
    /// `EnvironmentAttributes.NETHER_PORTAL_SPAWNS_PIGLINS` (the overworld's dimension type).
    pub nether_portal_spawns_piglins: bool,
}

impl Default for Environment {
    fn default() -> Self {
        Environment {
            sky_darken: 0,
            monsters_burn: true,
            thundering: false,
            raining: false,
            has_sky_light: true,
            monster_block_light_limit: 0,
            monster_light_test: (0, 7),
            ambient_light: 0.0,
            has_ceiling: false,
            surface_slime_chance: 0.0,
            day_count: 0,
            world_seed: 0,
            sea_level: 63,
            day_time: 6000,
            bees_stay_in_hive: false,
            creaking_active: false,
            turtle_egg_hatch_chance: 0.002,
            cat_waking_up_gift_chance: 0.0,
            sun_angle: 0.0,
            eyeblossom_open: None,
            nether_portal_spawns_piglins: false,
        }
    }
}

/// `Level.getSeaLevel`: the sea level of the level's generator (63 without one).
pub fn sea_level(level: &crate::level::Level) -> i32 {
    level.climate().map_or(63, |c| c.sea_level)
}

impl Environment {
    /// The overworld at `day` ticks of the day (the `minecraft:day` timeline) with the rain and
    /// thunder levels of the weather: sky darkness, whether monsters burn.
    pub fn overworld(day: i64, rain: f32, thunder: f32) -> Environment {
        let mut sky = 15.0 * day_sky_factor(day);
        sky += (4.0 - sky) * 0.3125 * rain;
        sky += (4.0 - sky) * 0.527_343_75 * thunder;
        #[allow(clippy::cast_possible_truncation)]
        let sky_darken = (15.0 - sky) as i32;
        let night = (12542..23460).contains(&day.rem_euclid(24000));
        Environment {
            sky_darken,
            monsters_burn: !night,
            bees_stay_in_hive: night || rain > 0.0,
            creaking_active: (12600..23401).contains(&day.rem_euclid(24000)),
            thundering: thunder > 0.9,
            raining: rain > 0.2,
            has_sky_light: true,
            monster_block_light_limit: 0,
            monster_light_test: (0, 7),
            ambient_light: 0.0,
            turtle_egg_hatch_chance: turtle_egg_hatch_chance(day),
            // Constant ease: 0.7 from 23667 to 362, 0 in between.
            cat_waking_up_gift_chance: if (362..23667).contains(&day.rem_euclid(24000)) {
                0.0
            } else {
                0.7
            },
            sun_angle: day_sun_angle(day),
            eyeblossom_open: Some((12600..23401).contains(&day.rem_euclid(24000))),
            nether_portal_spawns_piglins: true,
            ..Environment::default()
        }
    }

    /// `Level.isBrightOutside`.
    pub fn is_bright_outside(&self) -> bool {
        self.has_sky_light && self.sky_darken < 4
    }

    /// `Level.isDarkOutside`: false without a sky like `isBrightOutside`.
    pub fn is_dark_outside(&self) -> bool {
        self.has_sky_light && self.sky_darken >= 4
    }
}

/// The `day` timeline's `gameplay/turtle_egg_hatch_chance` track at `ticks` of the day: 1 from
/// 21062 to 21905 (constant ease), else the attribute's default 0.002.
pub fn turtle_egg_hatch_chance(ticks: i64) -> f32 {
    if (21062..21905).contains(&ticks.rem_euclid(24000)) {
        1.0
    } else {
        0.002
    }
}

/// The `day` timeline's `visual/sun_angle` track at `ticks` of the day: 0 at noon (6000),
/// growing to 360 over the day along the track's cubic Bézier ease.
pub fn day_sun_angle(ticks: i64) -> f32 {
    let t = ticks.rem_euclid(24000);
    // The segments wrap around the single key time 6000.
    let (from, to) = if t < 6000 {
        (-18000, 6000)
    } else {
        (6000, 30000)
    };
    if t <= from {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let alpha = (t - from) as f32 / (to - from) as f32;
    let eased = cubic_bezier(0.362, 0.241, 0.638, 0.759, alpha);
    0.0 + eased * 360.0
}

/// `EasingType.CubicBezier.apply(x)`: Newton–Raphson, then bisection.
pub fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    let curve = |v1: f32, v2: f32| (3.0 * v1 - 3.0 * v2 + 1.0, -6.0 * v1 + 3.0 * v2, 3.0 * v1);
    let sample = |(a, b, c): (f32, f32, f32), t: f32| ((a * t + b) * t + c) * t;
    let gradient = |(a, b, c): (f32, f32, f32), t: f32| (3.0 * a * t + 2.0 * b) * t + c;
    let (xc, yc) = (curve(x1, x2), curve(y1, y2));
    let mut t = x;
    let mut solved = false;
    for _ in 0..4 {
        let error = sample(xc, t) - x;
        if error.abs() < 1.0e-5 {
            solved = true;
            break;
        }
        let g = gradient(xc, t);
        if g < 1.0e-5 {
            break;
        }
        t -= (error / g).clamp(-0.25, 0.25);
    }
    if !solved {
        // `solveTBisect`.
        let (mut t0, mut t1) = (0.0f32, 1.0f32);
        while t0 < t1 {
            let error = sample(xc, t) - x;
            if error.abs() < 1.0e-5 {
                break;
            }
            if error < 0.0 {
                t0 = t;
            } else {
                t1 = t;
            }
            t = (t1 + t0) / 2.0;
        }
    }
    sample(yc, t)
}

/// The overworld's sky light factor at `ticks` of the day (the `day` timeline's
/// `gameplay/sky_light_level` track).
pub fn day_sky_factor(ticks: i64) -> f32 {
    const KEYS: [(i64, f32); 4] = [
        (133, 1.0),
        (11867, 1.0),
        (13670, 0.266_666_68),
        (22330, 0.266_666_68),
    ];
    let t = ticks.rem_euclid(24000);
    for w in KEYS.windows(2) {
        let (a, b) = (w[0], w[1]);
        if t >= a.0 && t < b.0 {
            #[allow(clippy::cast_precision_loss)]
            let f = (t - a.0) as f32 / (b.0 - a.0) as f32;
            return a.1 + (b.1 - a.1) * f;
        }
    }
    // Wraps from the last key to the first of the next day.
    let (last, first) = (KEYS[3], KEYS[0]);
    let span = first.0 + 24000 - last.0;
    let since = if t >= last.0 {
        t - last.0
    } else {
        t + 24000 - last.0
    };
    #[allow(clippy::cast_precision_loss)]
    let f = since as f32 / span as f32;
    last.1 + (first.1 - last.1) * f
}

/// Performance knobs of the entity tick (rules of `performance.*`). The default is Vanilla.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MobSettings {
    /// Path finding nodes all mobs of a world may visit per tick; `0` = unlimited (Vanilla).
    pub path_nodes_per_tick: u32,
    /// The activation range (`performance.activation-range.*`); off by default (Vanilla).
    pub activation: activation::ActivationSettings,
    /// Entities a mob pushes per tick; `None` = all (Vanilla).
    pub max_entity_collisions: Option<u32>,
    /// Ticks an arrow stays stuck in a block (Vanilla 1200).
    pub arrow_despawn_ticks: i32,
    /// Distance to the nearest player beyond which a mob despawns at once; `None` = its
    /// category's (Vanilla).
    pub hard_despawn_range: Option<f64>,
    /// Distance to the nearest player beyond which an idle mob may despawn (Vanilla 32).
    pub soft_despawn_range: f64,
    /// Primed TNT ticked per world tick; `None` = all (Vanilla).
    pub max_tnt_per_tick: Option<u32>,
}

impl Default for MobSettings {
    fn default() -> Self {
        MobSettings {
            path_nodes_per_tick: 0,
            activation: activation::ActivationSettings::default(),
            max_entity_collisions: None,
            arrow_despawn_ticks: 1200,
            hard_despawn_range: None,
            soft_despawn_range: 32.0,
            max_tnt_per_tick: None,
        }
    }
}

/// Explosion parameters (`Level.explode`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExplosionRequest {
    pub source: Option<EntityId>,
    /// The entity responsible (`getIndirectSourceEntity`); `None`: the source itself.
    pub indirect: Option<EntityId>,
    pub pos: Vec3,
    pub radius: f32,
    /// `ExplosionInteraction.MOB`: blocks only break with `mobGriefing`.
    pub mob: bool,
    /// The source's `getBlockExplosionResistance` (the wither's skulls, primed TNT minecarts).
    pub resistance: crate::level::explosion::BlockResistance,
    /// `fire`: blown up positions may catch fire (ghast fireballs with `mobGriefing`).
    pub fire: bool,
}

/// A wind charge burst (`AbstractWindCharge.explode`): a knockback-only explosion that
/// triggers blocks instead of breaking them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindBurst {
    /// The exploding charge.
    pub source: Option<EntityId>,
    pub pos: Vec3,
    pub radius: f32,
    /// The knockback multiplier of the damage calculator.
    pub knockback: f64,
    /// The burst sound.
    pub sound: &'static str,
}

impl ExplosionRequest {
    /// `Level.explode` with the block interaction of the rules (`ExplosionInteraction.MOB` or
    /// `TNT`).
    pub fn run(&self, level: &mut crate::level::Level, rules: &crate::survival::rules::GameRules) {
        use crate::level::explosion::BlockInteraction;
        use crate::survival::rules::Rule;
        let decay = if self.mob {
            Rule::MOB_EXPLOSION_DROP_DECAY
        } else {
            Rule::TNT_EXPLOSION_DROP_DECAY
        };
        let interaction = if self.mob && !rules.bool(Rule::MOB_GRIEFING) {
            BlockInteraction::Keep
        } else if rules.bool(decay) {
            BlockInteraction::DestroyWithDecay
        } else {
            BlockInteraction::Destroy
        };
        // `getIndirectSourceEntity`: a living source caused it itself.
        level.explode_with(
            self.source,
            self.indirect.or(self.source),
            self.pos,
            self.radius,
            interaction,
            crate::level::explosion::ExplosionStyle {
                resistance: self.resistance,
                fire: self.fire,
                ..Default::default()
            },
        );
    }
}

/// A change of the world a mob tick causes; applied right after that entity's tick (see
/// `docs/architecture/world-access.md`). Reads of the same tick that must see it go through
/// [`Tick::block`].
#[derive(Clone, Debug, PartialEq)]
pub enum WorldAction {
    Explode(ExplosionRequest),
    /// `addFreshEntity` of an area effect cloud at the position (after an explosion queued
    /// before it: `Creeper.spawnLingeringCloud`).
    SpawnCloud(Vec3, Box<crate::effect::cloud::CloudData>),
    /// `setBlock(pos, state, flags)`.
    SetBlock(BlockPos, BlockState, u32),
    /// `destroyBlock(pos, drop)`.
    DestroyBlock(BlockPos, bool),
    /// `levelEvent(event, pos, data)`.
    LevelEvent(i32, BlockPos, i32),
    /// `globalLevelEvent(event, pos, data)` (the `global_sound_events` rule decides who hears
    /// it).
    GlobalLevelEvent(i32, BlockPos, i32),
    /// `DoorBlock.setOpen(mob, level, state, pos, open)`.
    SetDoorOpen(BlockPos, bool, Option<EntityId>),
    /// A wind charge burst.
    WindBurst(WindBurst),
    /// `explode(source, damageSource(source, indirect), pos, radius, interaction)` with
    /// `ExplosionInteraction.BLOCK` (`blocks`) or `NONE`.
    ExplodeWith {
        source: Option<EntityId>,
        indirect: Option<EntityId>,
        pos: Vec3,
        radius: f32,
        blocks: bool,
    },
    /// Game events, listener moves and sculk catalyst charges.
    Level(boss::warden::LevelAction),
    /// `entityInside` of the block at the position for the entity (`applyEffectsFromBlocks`).
    EntityInside(BlockPos, EntityId),
    /// `stepOn` of the block at the position for the entity standing on it.
    StepOn(BlockPos, EntityId),
    /// `Entity.sendBubbleColumnParticles` at the top of the bubble column at the position.
    BubbleColumnParticles(BlockPos),
    /// `triggerOnDeathMobEffects` of a dead player (with the `max_entity_cramming` rule).
    PlayerDeathEffects(EntityId, i32),
    /// `onProjectileHit` of the block at the position, hit by the projectile at the point
    /// with the speed.
    ProjectileHitBlock(BlockPos, Vec3, EntityId, f64),
    /// A hopper minecart takes in an item (`MinecartHopper.tryConsumeItems`).
    HopperMinecart(EntityId),
    /// A chest minecart or chest boat that still had its loot table was destroyed.
    DestroyLootVehicle(Box<crate::minecart::LootVehicleDrop>),
    /// A spawner minecart ticks its spawner (`MinecartSpawner.tick`).
    SpawnerMinecart(EntityId),
    /// `updateNeighbourForOutputSignal(pos, AIR)`: comparators re-read an item frame.
    OutputSignal(BlockPos),
    /// A command block minecart runs its command (`BaseCommandBlock.performCommand`).
    MinecartCommand(EntityId),
    /// A burning mob in a water or powder snow cauldron takes a level of it.
    BurningInCauldron(BlockPos),
    /// `BellBlock.attemptToRing(entity, level, pos, null)`.
    RingBell(BlockPos, Option<EntityId>),
    /// `BoneMealItem.growCrop` on the block (a villager's bone meal).
    BoneMeal(BlockPos),
    /// `ServerLevel.sendParticles`.
    Particles(Box<crate::level::particles::LevelParticles>),
    /// `BeehiveBlockEntity.addOccupant`: the bee, saved as `data` when it entered (its tick
    /// discards it), moves into the hive at `hive`; `flower` is its saved flower.
    EnterHive {
        hive: BlockPos,
        bee: EntityId,
        data: Box<vanillite_nbt::Compound>,
        flower: Option<BlockPos>,
    },
    /// `CreakingHeartBlockEntity.creakingHurt` of the heart at the position.
    CreakingHurt(BlockPos),
    /// `container.startOpen(user)` (`true`) or `stopOpen(user)` of a block container by a mob.
    ContainerOpen(crate::block::container::ContainerRef, bool, EntityId),
    /// `setItem` of the given slots of a block container by a mob, then `setChanged`.
    ContainerSlots(
        crate::block::container::ContainerRef,
        Vec<(usize, crate::item::ItemStack)>,
    ),
    /// `TurtleEggBlock.destroyEgg` of an entity stepping (randomness 100) or falling (3) on
    /// turtle eggs; `can_destroy` is `canDestroyEgg`.
    TrampleTurtleEgg {
        pos: BlockPos,
        can_destroy: bool,
        randomness: i32,
    },
    /// `FarmlandBlock.turnToBaseBlock(entity, …)`: the entity trampled the farmland.
    TrampleFarmland(BlockPos, EntityId),
    /// `destroyBlock(pos, drop, breaker)` (ravagers in crops).
    DestroyBlockBy(BlockPos, bool, EntityId),
    /// `PiglinAi.angerNearbyPiglins(player, true)` of a chest vehicle the player opened or
    /// broke: queued like the block containers' ([`crate::level::LevelOutput::piglin_anger`]).
    PiglinAnger(EntityId),
    /// Frost Walker's `replace_disk` around a player's position.
    FrostWalker {
        pos: Vec3,
        radius: i32,
        source: EntityId,
    },
}

impl WorldAction {
    /// Applies the actions that need nothing but the level; returns `false` for the others.
    pub fn apply_to_level(&self, level: &mut crate::level::Level) -> bool {
        match *self {
            WorldAction::SetBlock(pos, block, flags) => {
                level.set_block(pos, block, flags);
            }
            WorldAction::DestroyBlock(pos, drop) => {
                level.destroy_block(pos, drop, None);
            }
            WorldAction::LevelEvent(event, pos, data) => {
                level.level_event(None, event, pos, data);
            }
            WorldAction::GlobalLevelEvent(event, pos, data) => {
                level.global_level_event(event, pos, data);
            }
            WorldAction::EntityInside(pos, id) => {
                if level.entities.store.contains(id) {
                    level.entity_inside(pos, id);
                }
            }
            WorldAction::StepOn(pos, id) => {
                if level.entities.store.contains(id) {
                    crate::block::contact::step_on(level, pos, id);
                }
            }
            WorldAction::SpawnCloud(pos, ref data) => {
                let ids = std::sync::Arc::clone(level.network_ids_arc());
                crate::effect::cloud::spawn_cloud(&mut level.entities, &ids, pos, (**data).clone());
            }
            WorldAction::PlayerDeathEffects(player, cramming) => {
                crate::effect::living::player_death_effects(level, player, cramming);
            }
            WorldAction::BubbleColumnParticles(pos) => {
                crate::level::particles::bubble_column_particles(level, pos);
            }
            WorldAction::TrampleFarmland(pos, id) => {
                crate::block::contact::trample_farmland(level, pos, Some(id));
            }
            WorldAction::DestroyBlockBy(pos, drop, id) => {
                level.destroy_block(pos, drop, Some(id));
            }
            WorldAction::PiglinAnger(player) => {
                level.output().piglin_anger.push((player, true));
            }
            WorldAction::FrostWalker {
                pos,
                radius,
                source,
            } => {
                crate::enchant::location::frost_walker(level, pos, radius, source);
            }
            WorldAction::ProjectileHitBlock(pos, at, id, speed) => {
                crate::block::projectile_hit::projectile_hit_block(level, pos, at, id, speed);
            }
            WorldAction::HopperMinecart(id) => crate::minecart::hopper_suck(level, id),
            WorldAction::SpawnerMinecart(id) => crate::block::spawner::tick_minecart(level, id),
            WorldAction::DestroyLootVehicle(ref drop) => {
                crate::minecart::destroy_with_loot(level, (**drop).clone());
            }
            WorldAction::OutputSignal(pos) => {
                level.update_neighbour_for_output_signal(pos, vanillite_data::block::ids::AIR);
            }
            WorldAction::MinecartCommand(id) => {
                crate::block::technical::command_block::perform_command(
                    level,
                    crate::block::technical::command_block::Owner::Minecart(id),
                );
            }
            WorldAction::BurningInCauldron(pos) => {
                crate::block::cauldron::burning_inside(level, pos)
            }
            WorldAction::RingBell(pos, entity) => {
                crate::block::bell::attempt_to_ring(level, entity, pos, None);
            }
            WorldAction::EnterHive {
                hive,
                bee,
                ref data,
                flower,
            } => crate::block::beehive::add_occupant(level, hive, bee, data, flower),
            WorldAction::CreakingHurt(pos) => {
                crate::block::creaking_heart::creaking_hurt(level, pos)
            }
            WorldAction::ContainerOpen(c, open, by) => {
                let halves = match c {
                    crate::block::container::ContainerRef::Block(p) => [Some(p), None],
                    crate::block::container::ContainerRef::Double(a, b) => [Some(a), Some(b)],
                    crate::block::container::ContainerRef::Entity(_)
                    | crate::block::container::ContainerRef::Composter(..) => [None, None],
                };
                for p in halves.into_iter().flatten() {
                    if open {
                        crate::block::container::start_open(level, p, false, Some(by));
                    } else {
                        crate::block::container::stop_open(level, p, false, Some(by));
                    }
                }
            }
            WorldAction::ContainerSlots(c, ref slots) => {
                for (slot, stack) in slots {
                    c.set(level, *slot, stack.clone());
                }
                c.set_changed(level);
            }
            WorldAction::BoneMeal(pos) => {
                let mut meal =
                    crate::item::ItemStack::new(vanillite_data::item::ids::BONE_MEAL.0, 1);
                crate::block::bonemeal::grow_crop(level, pos, &mut meal);
            }
            WorldAction::TrampleTurtleEgg {
                pos,
                can_destroy,
                randomness,
            } => crate::block::eggs::trample_turtle_egg(level, pos, can_destroy, randomness),
            WorldAction::SetDoorOpen(pos, open, source) => {
                crate::block::redstone::set_door_open(level, pos, open, source);
            }
            WorldAction::Particles(ref p) => level.send_particles_packet((**p).clone()),
            WorldAction::WindBurst(b) => {
                // `ExplosionInteraction.TRIGGER` without damage (`damagesEntities` off).
                level.explode_with(
                    b.source,
                    None,
                    b.pos,
                    b.radius,
                    crate::level::explosion::BlockInteraction::TriggerBlock,
                    crate::level::explosion::ExplosionStyle {
                        damages_entities: false,
                        knockback: b.knockback,
                        wind_sound: Some(b.sound),
                        resistance: crate::level::explosion::BlockResistance::WindCharge,
                        fire: false,
                        bad_respawn_point: false,
                    },
                );
            }
            WorldAction::Explode(_) | WorldAction::ExplodeWith { .. } | WorldAction::Level(_) => {
                return false;
            }
        }
        true
    }
}

/// The mob state of a world besides the entities: search buffers, queued world changes and
/// this tick's environment.
#[derive(Debug, Default)]
pub struct MobContext {
    pub finder: path::PathFinder,
    pub boxes: Vec<Aabb>,
    /// `movementThisTick` of the mob that ticks: the moves of `Entity.move`, for the blocks
    /// on their way (`applyEffectsFromBlocks`).
    pub movements: Vec<crate::survival::inside::Movement>,
    pub actions: Vec<WorldAction>,
    pub env: Environment,
    pub settings: MobSettings,
    /// Path nodes left this tick (`u64::MAX` = unlimited).
    pub path_budget: u64,
    pub ids: Vec<EntityId>,
    /// The world has at least one player.
    pub has_players: bool,
    /// Entities to remove after the tick (`discard` of other entities).
    pub discarded: Vec<EntityId>,
    /// The boss bars of entities (the wither's), by entity; see
    /// [`boss::wither::sweep_boss_bars`].
    pub boss_bars: vanillite_base::hash::FxHashMap<EntityId, crate::boss_bar::BossBar>,
    /// The End fight as the dragon and the crystals see it; `None` outside the End.
    pub dragon_fight: Option<boss::dragon::FightView>,
    /// What the dragon and the crystals report to the fight, handled after each tick.
    pub dragon_events: Vec<boss::dragon::DragonEvent>,
    /// Menus players opened at entities (chest boats, container minecarts), opened once the
    /// entities are back in the level.
    pub menus: Vec<crate::menu::container::MenuRequest>,
    /// End gateways that took an entity this tick: they start their cooldown after the
    /// entities' tick ([`crate::block::portal::trigger_gateway_cooldown`]).
    pub gateways: Vec<BlockPos>,
    /// What the entities' tick asks of the world: chunk tickets, gateway exits to load, ender
    /// pearl owners to bring over.
    pub travel: Vec<crate::portal::TravelRequest>,
}

impl MobContext {
    /// `ServerLevel.sendParticles(particle, x, y, z, count, xDist, yDist, zDist, speed)`.
    pub fn send_particles(
        &mut self,
        particle: crate::level::particles::Particle,
        pos: Vec3,
        count: i32,
        dist: Vec3,
        speed: f64,
    ) {
        let packet = crate::level::particles::packet(particle, pos, count, dist, speed);
        self.actions.push(WorldAction::Particles(Box::new(packet)));
    }

    /// Resets the per-tick state.
    pub fn begin_tick(&mut self, env: Environment, settings: MobSettings, has_players: bool) {
        self.env = env;
        self.settings = settings;
        self.has_players = has_players;
        self.path_budget = if settings.path_nodes_per_tick == 0 {
            u64::MAX
        } else {
            u64::from(settings.path_nodes_per_tick)
        };
    }
}

/// `Mth.DEG_TO_RAD` (`(float) (Math.PI / 180.0)`): Vanilla turns float degrees into radians
/// with a float multiplication before `Mth.sin`/`Mth.cos`.
pub const DEG_TO_RAD: f32 = 0.017_453_292;

/// `Entity.calculateViewVector(xRot, yRot)` with Vanilla's table sine.
pub fn view_vector(x_rot: f32, y_rot: f32) -> Vec3 {
    use vanillite_base::math::{cos, sin};
    let f = x_rot * DEG_TO_RAD;
    let f1 = -y_rot * DEG_TO_RAD;
    let (f2, f3) = (cos(f64::from(f1)), sin(f64::from(f1)));
    let (f4, f5) = (cos(f64::from(f)), sin(f64::from(f)));
    Vec3::new(f64::from(f3 * f4), f64::from(-f5), f64::from(f2 * f4))
}

/// `-((float) Mth.atan2(x, z)) * Mth.RAD_TO_DEG`: the yaw of a direction as the flying mobs
/// compute it, a float product.
pub fn direction_yaw(x: f64, z: f64) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    let a = vanillite_base::math::atan2(x, z) as f32;
    -a * (180.0 / std::f32::consts::PI)
}

/// `Mth.atan2(y, x) * Mth.RAD_TO_DEG` (the float `57.295776F` widened to double), as every
/// Vanilla entity rotation computes it.
pub fn atan2_deg(y: f64, x: f64) -> f64 {
    vanillite_base::math::atan2(y, x) * f64::from(57.295_776_f32)
}

fn fluid_ids(names: [&str; 2]) -> [u16; 2] {
    names.map(|n| vanillite_data::registry::FLUID.id(n).unwrap_or(u16::MAX))
}

/// Whether a block holds water (`FluidTags.WATER`).
pub fn is_water(state: BlockState) -> bool {
    static IDS: std::sync::OnceLock<[u16; 2]> = std::sync::OnceLock::new();
    let f = state.fluid_state();
    !f.is_empty()
        && IDS
            .get_or_init(|| fluid_ids(["minecraft:water", "minecraft:flowing_water"]))
            .contains(&f.info().fluid)
}

/// Whether a block holds lava (`FluidTags.LAVA`).
pub fn is_lava(state: BlockState) -> bool {
    static IDS: std::sync::OnceLock<[u16; 2]> = std::sync::OnceLock::new();
    let f = state.fluid_state();
    !f.is_empty()
        && IDS
            .get_or_init(|| fluid_ids(["minecraft:lava", "minecraft:flowing_lava"]))
            .contains(&f.info().fluid)
}

/// `getEyeHeight` of a mob: the type's eye height, scaled for babies.
pub fn eye_height(mob: &MobData) -> f64 {
    let eye = unscaled_eye_height(mob);
    // The `minecraft:scale` attribute scales the eye height with the dimensions.
    let scale = crate::effect::attribute_map::scale_of(&mob.effects.modifiers);
    if scale == 1.0 {
        return eye;
    }
    #[allow(clippy::cast_possible_truncation)]
    f64::from(eye as f32 * scale as f32)
}

fn unscaled_eye_height(mob: &MobData) -> f64 {
    if let Some(eye) = fam::undead::zombies::baby_eye_height(mob) {
        return f64::from(eye);
    }
    if mob.baby
        && let Some((_, _, eye)) = kinds::baby_dimensions(mob.kind)
    {
        return f64::from(eye);
    }
    let info = mob.kind.type_id().info();
    let scale = fam::size_scale(mob).unwrap_or(if mob.baby { 0.5 } else { 1.0 });
    f64::from(info.eye_height * scale)
}

/// Adds a mob to the entities (`addFreshEntity` without `finalizeSpawn`).
pub fn spawn(
    entities: &mut Entities,
    network_ids: &NetworkIds,
    uuid: u128,
    pos: Vec3,
    rot: Rotation,
    mut mob: MobData,
) -> EntityId {
    fam::created(&mut mob, entities.game_time, entities.day_time);
    // The dragon's eight parts take the ids after its own.
    let network_id = if mob.kind == Kind::EnderDragon {
        network_ids.next_block(9)
    } else {
        network_ids.next()
    };
    let mut new = NewEntity::new(mob.kind.type_id(), network_id, uuid, pos);
    new.rot = rot;
    let dims = kinds::mob_dimensions(&mob);
    let decor = mob.kind.family() == Family::Decor;
    let id = entities.spawn_mob(new, mob);
    if let Some(mut e) = entities.store.get_mut(id)
        && e.as_ref().dims() != dims
    {
        e.set_dims(dims);
    }
    if decor {
        crate::decoration::stand::after_spawn(entities, id);
    }
    id
}

thread_local! {
    /// The seeds of new entities: Vanilla gives every entity a fresh random
    /// (`RandomSource.create()`), so creating one draws nothing from the level's or another
    /// entity's random. The parity scenarios replace it with a stream (see
    /// [`set_entity_seed_stream`]).
    static ENTITY_SEEDS: std::cell::RefCell<(Option<LegacyRandom>, LegacyRandom)> =
        std::cell::RefCell::new((None, LegacyRandom::new(crate::survival::sound_seed())));
}

/// The seed of a newly created entity's own random (the `Entity` constructor's
/// `RandomSource.create()`).
pub fn new_entity_seed() -> i64 {
    ENTITY_SEEDS.with_borrow_mut(|(stream, unique)| match stream {
        Some(s) => s.next_long(),
        None => unique.next_long(),
    })
}

/// A seed for what Vanilla draws from a new entity's random before the entity gets its seed
/// (`defineSynchedData` in the `Entity` constructor): unique, never from the seed stream.
pub fn new_unique_seed() -> i64 {
    ENTITY_SEEDS.with_borrow_mut(|(_, unique)| unique.next_long())
}

/// From now on (on this thread) new entities take their seeds from a legacy random seeded
/// with `seed` in creation order (`None`: unique seeds again). For tick exact comparisons with
/// Vanilla, whose recorder does the same.
pub fn set_entity_seed_stream(seed: Option<i64>) {
    ENTITY_SEEDS.with_borrow_mut(|(stream, _)| *stream = seed.map(LegacyRandom::new));
}

thread_local! {
    /// The seed of every shuffled behaviour list's own random in parity scenarios (see
    /// [`set_parity_shuffle_seed`]).
    static SHUFFLE_SEED: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// The seed of a new shuffled behaviour list's own random (`ShufflingList.random`, a
/// `RandomSource.create()` of its own): unique, or the parity seed (on this thread).
pub fn new_shuffle_seed() -> i64 {
    SHUFFLE_SEED
        .get()
        .unwrap_or_else(|| ENTITY_SEEDS.with_borrow_mut(|(_, unique)| unique.next_long()))
}

/// From now on (on this thread) every new shuffled behaviour list seeds its random with
/// `seed` (`None`: unique seeds). For tick exact comparisons with Vanilla, whose recorder
/// does the same.
pub fn set_parity_shuffle_seed(seed: Option<i64>) {
    SHUFFLE_SEED.set(seed);
}

/// The UUID of a new entity (`Mth.createInsecureUUID` of its own fresh random): draws from no
/// game random.
pub fn new_uuid() -> u128 {
    crate::block::tnt::insecure_uuid()
}

/// The UUID, yaw, motion and random state ([`crate::item_entity::ItemData::random`]) of a new
/// item entity (`new ItemEntity(level, x, y, z, stack)`): the constructor draws `bobOffs`, the
/// yaw and the motion from the item's own random, which takes its seed from the entity seed
/// stream. Throws replace the motion afterwards.
pub fn new_item_entity() -> (u128, f32, Vec3, i64) {
    let mut own = LegacyRandom::new(new_entity_seed());
    // `bobOffs`, then the yaw.
    let _bob = own.next_float();
    let y_rot = own.next_float() * 360.0;
    let x = own.next_double() * 0.2 - 0.1;
    let z = own.next_double() * 0.2 - 0.1;
    (new_uuid(), y_rot, Vec3::new(x, 0.2, z), own.state())
}

/// The UUID, yaw and random state of a new item entity made with its motion
/// (`new ItemEntity(level, x, y, z, stack, dx, dy, dz)`): the constructor draws only `bobOffs`
/// and the yaw from the item's own random.
pub fn new_item_entity_with_motion() -> (u128, f32, i64) {
    let mut own = LegacyRandom::new(new_entity_seed());
    let _bob = own.next_float();
    let y_rot = own.next_float() * 360.0;
    (new_uuid(), y_rot, own.state())
}

/// The UUID of a new entity that keeps no random of its own here (items): it still takes its
/// seed from the entity seed stream, so the entities created after it get Vanilla's seeds.
pub fn new_entity_uuid() -> u128 {
    new_entity_seed();
    new_uuid()
}

/// A random UUID (`Mth.createInsecureUUID`).
pub fn random_uuid(random: &mut impl RandomSource) -> u128 {
    let most = random.next_long() as u64 & !0xF000 | 0x4000;
    let least = random.next_long() as u64 & 0x3FFF_FFFF_FFFF_FFFF | 0x8000_0000_0000_0000;
    (u128::from(most) << 64) | u128::from(least)
}

/// What one mob tick works on.
pub struct Tick<'a, 'e> {
    pub level: &'a Level,
    pub ents: &'a mut Entities,
    pub env: &'a mut Env<'e>,
    pub cx: &'a mut MobContext,
    pub id: EntityId,
}

impl std::fmt::Debug for Tick<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tick")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Tick<'_, '_> {
    /// The ticking mob's data. Panics are impossible while the mob is alive in the store;
    /// callers check [`Self::exists`] once at the start.
    pub fn mob(&mut self) -> &mut MobData {
        let row = self
            .ents
            .store
            .get(self.id)
            .map_or(u32::MAX, |e| e.kind_row());
        self.ents
            .mobs
            .get_mut(row)
            .expect("the ticking mob is in the mob store")
    }

    /// The ticking mob's data, read only.
    pub fn mob_ref(&self) -> &MobData {
        let row = self
            .ents
            .store
            .get(self.id)
            .map_or(u32::MAX, |e| e.kind_row());
        self.ents
            .mobs
            .get(row)
            .expect("the ticking mob is in the mob store")
    }

    /// The mob whose navigation and move control the ticking mob uses (`Mob.getNavigation`,
    /// `getMoveControl`): the mob vehicle it steers, down the chain, else itself.
    pub fn nav_owner(&self) -> EntityId {
        nav_owner(self.ents, self.id)
    }

    /// `getNavigation()`: the navigation of [`Self::nav_owner`].
    pub fn nav(&mut self) -> &mut nav::Navigation {
        let owner = self.nav_owner();
        if owner == self.id {
            return &mut self.mob().nav;
        }
        &mut self
            .ents
            .mob_mut(owner)
            .expect("the controlled vehicle is a mob")
            .nav
    }

    /// `getNavigation()`, read only.
    pub fn nav_ref(&self) -> &nav::Navigation {
        let owner = self.nav_owner();
        if owner == self.id {
            return &self.mob_ref().nav;
        }
        &self
            .ents
            .mob(owner)
            .expect("the controlled vehicle is a mob")
            .nav
    }

    /// `getMoveControl()`: the move control of [`Self::nav_owner`].
    pub fn move_ctl(&mut self) -> &mut nav::MoveControl {
        let owner = self.nav_owner();
        if owner == self.id {
            return &mut self.mob().move_ctl;
        }
        &mut self
            .ents
            .mob_mut(owner)
            .expect("the controlled vehicle is a mob")
            .move_ctl
    }

    /// Runs `f` as the mob of [`Self::nav_owner`] (the navigation calls of a mob that steers
    /// its vehicle path and move the vehicle).
    pub fn as_nav_owner<R>(&mut self, f: impl FnOnce(&mut Tick<'_, '_>) -> R) -> R {
        let owner = self.nav_owner();
        if owner == self.id {
            return f(self);
        }
        let mut v = Tick {
            level: self.level,
            ents: &mut *self.ents,
            env: &mut *self.env,
            cx: &mut *self.cx,
            id: owner,
        };
        f(&mut v)
    }

    /// Whether the ticking mob still exists.
    pub fn exists(&self) -> bool {
        self.ents.mob(self.id).is_some()
    }

    /// The ticking mob's core columns.
    pub fn core(&self) -> EntityRef<'_> {
        self.ents
            .store
            .get(self.id)
            .expect("the ticking mob is in the store")
    }

    /// The ticking mob's core columns, mutably.
    pub fn core_mut(&mut self) -> EntityMut<'_> {
        self.ents
            .store
            .get_mut(self.id)
            .expect("the ticking mob is in the store")
    }

    pub fn pos(&self) -> Vec3 {
        self.core().pos()
    }

    pub fn velocity(&self) -> Vec3 {
        self.core().velocity()
    }

    pub fn set_velocity(&mut self, v: Vec3) {
        self.core_mut().set_velocity(v);
    }

    pub fn on_ground(&self) -> bool {
        self.core().flags().contains(EntityFlags::ON_GROUND)
    }

    pub fn aabb(&self) -> Aabb {
        self.core().aabb()
    }

    pub fn block_pos(&self) -> BlockPos {
        let p = self.pos();
        BlockPos::containing(p.x, p.y, p.z)
    }

    /// `getBlockState(pos)` as the ticking entity sees it. Its block changes are queued
    /// ([`WorldAction`]) and applied after its tick, while in Vanilla they are immediate: this
    /// read sees its own queued `setBlock` and `destroyBlock` like Vanilla does. Code that reads
    /// a position it may have changed in the same tick reads through this, not `t.level`.
    pub fn block(&self, pos: BlockPos) -> BlockState {
        for action in self.cx.actions.iter().rev() {
            match *action {
                WorldAction::SetBlock(at, state, _) if at == pos => return state,
                WorldAction::DestroyBlock(at, _) if at == pos => {
                    return self.level.fluid(pos).block_state();
                }
                _ => {}
            }
        }
        self.level.block(pos)
    }

    /// `getEyeY`.
    pub fn eye_y(&self) -> f64 {
        self.pos().y + eye_height(self.mob_ref())
    }

    pub fn game_time(&self) -> i64 {
        self.env.game_time
    }

    /// Position and eye height of another living entity (player or mob).
    pub fn other(&self, id: EntityId) -> Option<Other> {
        let e = self.ents.store.get(id)?;
        let eye = if let Some(m) = self.ents.mob(id) {
            if !m.is_alive() {
                return None;
            }
            eye_height(m)
        } else {
            let p = self.ents.player(id)?;
            if p.survival.is_dead_or_dying() {
                return None;
            }
            f64::from(e.kind().info().eye_height)
                * f64::from(e.dims().height / e.kind().info().height.max(0.01))
        };
        Some(Other {
            pos: e.pos(),
            eye_y: e.pos().y + eye,
            aabb: e.aabb(),
            height: e.dims().height,
            is_player: self.ents.player(id).is_some(),
        })
    }

    /// The light level dependent value at the mob's eyes (`getLightLevelDependentMagicValue`).
    pub fn light_magic_value(&self) -> f32 {
        let p = self.pos();
        let pos = BlockPos::containing(p.x, self.eye_y(), p.z);
        if !self.level.is_loaded(pos) {
            return 0.0;
        }
        light_magic_value(self.level, &self.cx.env, pos)
    }
}

/// `Mob.getControllingPassenger` for mob riders: the first passenger of a mob with AI when it
/// is a mob that can control vehicles (not in `#non_controlling_rider`). Player controllers
/// (saddled horses, pigs, striders) are not mobs and never steer through the AI.
pub fn controlling_mob_passenger(ents: &Entities, vehicle: EntityId) -> Option<EntityId> {
    let &first = ents.riding.passengers_of(vehicle).first()?;
    if ents.mob(vehicle)?.no_ai {
        return None;
    }
    let rider = ents.mob(first)?;
    (!matches!(rider.kind, Kind::Slime | Kind::MagmaCube | Kind::SulfurCube)).then_some(first)
}

/// `hasControllingPassenger`: a player steers `vehicle` or a mob rider does.
pub fn has_controlling_passenger(ents: &Entities, vehicle: EntityId) -> bool {
    ents.riding
        .passengers_of(vehicle)
        .first()
        .is_some_and(|&p| crate::riding::controls(ents, p, vehicle))
        || controlling_mob_passenger(ents, vehicle).is_some()
}

/// `getControlledVehicle() instanceof Mob`: the mob vehicle `id` steers.
pub fn controlled_mob_vehicle(ents: &Entities, id: EntityId) -> Option<EntityId> {
    let vehicle = ents.vehicle_of(id)?;
    (controlling_mob_passenger(ents, vehicle) == Some(id)).then_some(vehicle)
}

/// `getRootVehicle`: the lowest vehicle under `id`, or `id` itself.
pub fn root_vehicle(ents: &Entities, id: EntityId) -> EntityId {
    let mut root = id;
    while let Some(v) = ents.vehicle_of(root) {
        root = v;
    }
    root
}

/// The end of the [`controlled_mob_vehicle`] chain from `id` (`getNavigation` recurses).
pub fn nav_owner(ents: &Entities, id: EntityId) -> EntityId {
    let mut owner = id;
    while let Some(v) = controlled_mob_vehicle(ents, owner) {
        if v == id {
            break;
        }
        owner = v;
    }
    owner
}

/// Another living entity as a mob sees it.
#[derive(Clone, Copy, Debug)]
pub struct Other {
    pub pos: Vec3,
    pub eye_y: f64,
    pub aabb: Aabb,
    /// `getBbHeight()`.
    pub height: f32,
    pub is_player: bool,
}

/// `getMaxLocalRawBrightness(pos)` with the sky darkened by the time of day and the weather.
pub fn max_local_raw_brightness(level: &Level, env: &Environment, pos: BlockPos) -> i32 {
    let (sky, block) = level.light(pos);
    (sky - env.sky_darken).max(block)
}

/// `LevelReader.getLightLevelDependentMagicValue(pos)`.
pub fn light_magic_value(level: &Level, env: &Environment, pos: BlockPos) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let v = max_local_raw_brightness(level, env, pos) as f32 / 15.0;
    let curved = v / (4.0 - 3.0 * v);
    env.ambient_light + (1.0 - env.ambient_light) * curved
}

/// `canSeeSky(pos)`: no light blocking block above (the sky light at full strength).
pub fn can_see_sky(level: &Level, pos: BlockPos) -> bool {
    if pos.y >= level.max_y() {
        return true;
    }
    let Some(c) = level.chunks.chunk(ChunkPos::new(pos.x >> 4, pos.z >> 4)) else {
        return false;
    };
    let Some(heights) = c
        .chunk
        .heightmaps()
        .get(vanillite_chunk::HeightmapKind::MotionBlocking)
    else {
        return false;
    };
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    let top = heights.get((pos.x & 15) as u32, (pos.z & 15) as u32) as i32 + c.chunk.min_y();
    top <= pos.y
}

/// One mob tick (`ServerLevel.tickNonPassenger`: `tickCount++`, then `Mob.tick`), with the
/// despawn check before it. `level.entities` is empty during the call; the entities are in
/// `ents`.
pub fn tick(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) {
    let mut t = Tick {
        level,
        ents,
        env,
        cx,
        id,
    };
    if !t.exists() {
        return;
    }
    living::check_despawn(&mut t);
    if t.mob_ref().removed {
        return;
    }
    t.mob().tick_count += 1;
    // `Entity.rideTick` of a passenger: no motion of its own before the tick, and
    // `LivingEntity.rideTick` resets the fall distance after it.
    let passenger = t.ents.vehicle_of(id).is_some();
    if passenger {
        t.set_velocity(Vec3::ZERO);
    }
    living::tick(&mut t);
    if passenger && t.exists() && t.ents.vehicle_of(id).is_some() {
        t.mob().fall_distance = 0.0;
    }
}

/// The tick of an inactive mob that thinks (`performance.activation-range`, the goal
/// selector throttle): [`activation::age`], then the goal and target selectors choose and tick
/// their goals. Navigation and movement wait for the mob's next full tick. Mobs with a brain
/// only age.
pub fn think(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) {
    let mut t = Tick {
        level,
        ents,
        env,
        cx,
        id,
    };
    if !t.exists() || t.mob_ref().removed {
        return;
    }
    activation::age(t.mob());
    let m = t.mob();
    if m.no_ai || m.brain.is_some() || !m.is_alive() {
        return;
    }
    m.seen.clear();
    m.unseen.clear();
    goals::tick_selector(&mut t, goals::Which::Targets);
    if !t.exists() || t.mob_ref().removed {
        return;
    }
    goals::tick_selector(&mut t, goals::Which::Goals);
}

#[cfg(test)]
mod size_tests {
    /// The record every mob carries must not grow unnoticed: state of one kind or family goes
    /// into `MobData::ext` (see [`super::MobData`]). Raise the limit only for state every mob
    /// needs.
    #[test]
    fn mob_data_size() {
        let size = std::mem::size_of::<super::MobData>();
        assert!(
            size <= LIMIT,
            "MobData grew to {size} bytes (limit {LIMIT})"
        );
    }

    const LIMIT: usize = 1824;
}
