//! The survival state of a player and its logic: `LivingEntity`/`Player`/`ServerPlayer`'s
//! damage, death, regeneration, air, fire, fall damage, exhaustion and experience.

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{
    DamageEvent, EntityEvent, PlayerCombatKill, SetExperience, SetHealth, SoundSource,
};
use vanillite_protocol::text::Component;
use vanillite_world::entity::{EntityFlags, EntityId, EntityMut, Pose};

use super::blocks::{self, BlockAccess, BlockFacts, FluidContact, Landing};
use super::combat::{self, CombatEntry, CombatTracker, FallLocation, SourceNames};
use super::damage::{DamageKind, DamageSource, DamageTypes, tag};
use super::food::{FoodAction, FoodData, exhaustion};
use super::rules::{Difficulty, GameRules, Rule};
use super::{Effect, play_sound};
use crate::item::EquipmentSlot;
use crate::player::PlayerData;
use crate::scoreboard::CriteriaKind;
use crate::stats::custom;

use crate::advancements::tracking::reset_fall_distance;

/// `Math.round(damage * 10.0F)` of the damage statistics, for damage above 0 and below
/// `Float.MAX_VALUE / 10`.
#[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
pub fn damage_stat(damage: f32) -> Option<i32> {
    (damage > 0.0 && damage < 3.402_823_5e37).then(|| (damage * 10.0).round() as i32)
}

/// `Attributes.MAX_HEALTH` of players.
pub const MAX_HEALTH: f32 = 20.0;
/// `Entity.TOTAL_AIR_SUPPLY`.
pub const MAX_AIR: i32 = 300;
/// `Player.getFireImmuneTicks`.
const FIRE_IMMUNE_TICKS: i32 = 20;
/// `Attributes.SAFE_FALL_DISTANCE`.
const SAFE_FALL_DISTANCE: f64 = 3.0;
/// Player eye height when standing (`Player.STANDING_EYE_HEIGHT`) and the other poses.
pub fn eye_height(height: f32) -> f64 {
    // `EntityDimensions.eyeHeight` of the player's poses: 1.62 standing, 1.27 crouching,
    // 0.4 swimming/gliding, 0.2 sleeping/dying.
    let eye: f32 = if height >= 1.8 {
        1.62
    } else if height >= 1.5 {
        1.27
    } else if height >= 0.6 {
        0.4
    } else {
        0.2
    };
    f64::from(eye)
}

/// Entity event ids (`EntityEvent`).
pub mod entity_event {
    pub const DEATH: i8 = 3;
    pub const POOF: i8 = 60;
    pub const DROWN_PARTICLES: i8 = 67;
    pub const HONEY_SLIDE: i8 = 54;
}

/// A player's abilities as its game mode sets them (`GameType.updatePlayerAbilities`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Abilities {
    pub invulnerable: bool,
    pub mayfly: bool,
    pub instabuild: bool,
}

impl Abilities {
    /// The abilities of a game mode.
    pub fn of(mode: GameMode) -> Abilities {
        match mode {
            GameMode::Creative => Abilities {
                invulnerable: true,
                mayfly: true,
                instabuild: true,
            },
            GameMode::Spectator => Abilities {
                invulnerable: true,
                mayfly: true,
                instabuild: false,
            },
            GameMode::Survival | GameMode::Adventure => Abilities {
                invulnerable: false,
                mayfly: false,
                instabuild: false,
            },
        }
    }
}

/// Experience (`experienceLevel`, `experienceProgress`, `totalExperience`, `enchantmentSeed`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Experience {
    pub level: i32,
    pub progress: f32,
    pub total: i32,
    pub seed: i32,
}

impl Experience {
    /// `getXpNeededForNextLevel`.
    pub fn needed_for_next_level(&self) -> i32 {
        if self.level >= 30 {
            112 + (self.level - 30) * 9
        } else if self.level >= 15 {
            37 + (self.level - 15) * 5
        } else {
            7 + self.level * 2
        }
    }

    /// `giveExperienceLevels` (without the level-up sound).
    pub fn give_levels(&mut self, amount: i32) {
        self.level = self.level.saturating_add(amount);
        if self.level < 0 {
            self.level = 0;
            self.progress = 0.0;
            self.total = 0;
        }
    }

    /// `giveExperiencePoints`.
    pub fn give_points(&mut self, points: i32) {
        self.give_points_with(points, &mut |_| {});
    }

    /// `giveExperiencePoints` calling `level_up` with the new level of every level gained.
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    pub fn give_points_with(&mut self, points: i32, level_up: &mut dyn FnMut(i32)) {
        self.progress += points as f32 / self.needed_for_next_level() as f32;
        self.total = self.total.saturating_add(points).max(0);
        while self.progress < 0.0 {
            let remaining = self.progress * self.needed_for_next_level() as f32;
            if self.level > 0 {
                self.give_levels(-1);
                self.progress = 1.0 + remaining / self.needed_for_next_level() as f32;
            } else {
                self.give_levels(-1);
                self.progress = 0.0;
            }
        }
        while self.progress >= 1.0 {
            self.progress = (self.progress - 1.0) * self.needed_for_next_level() as f32;
            self.give_levels(1);
            level_up(self.level);
            self.progress /= self.needed_for_next_level() as f32;
        }
    }
}

/// The values last sent to the client (`lastSentHealth`, …).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sent {
    health: f32,
    food: i32,
    saturation_zero: bool,
    exp: i32,
    /// The active effects were sent (after joining or respawning).
    effects: bool,
}

impl Default for Sent {
    fn default() -> Self {
        Sent {
            health: -1.0e8,
            food: -99_999_999,
            saturation_zero: false,
            exp: -99_999_999,
            effects: false,
        }
    }
}

/// The survival state of a player.
#[derive(Clone, Debug, PartialEq)]
pub struct Survival {
    /// The player's own random (`Entity.random`) as the state of its legacy LCG
    /// (`LegacyRandom::from_state`); thrown items draw from it.
    pub own_random: i64,
    /// `DATA_HEALTH_ID`.
    pub health: f32,
    /// `DATA_PLAYER_ABSORPTION_ID`.
    pub absorption: f32,
    pub food: FoodData,
    /// `DATA_AIR_SUPPLY_ID`.
    pub air: i32,
    /// `remainingFireTicks`.
    pub fire_ticks: i32,
    pub fall_distance: f64,
    pub hurt_time: i32,
    /// `DATA_ARROW_COUNT_ID`, `removeArrowTime`.
    pub arrows: crate::mob::projectile::StuckArrows,
    /// `damageCooldownTime`.
    pub damage_cooldown: i32,
    /// `lastHurt`.
    pub last_hurt: f32,
    pub death_time: i32,
    /// `isRemoved()`: `tickDeath` removed the dead player (once, until it respawns).
    pub removed: bool,
    /// `skipDropExperience` (`wasExperienceConsumed`): a sculk catalyst took the experience
    /// reward (an armor stand the player broke next to one); the player drops no orbs and
    /// feeds no catalyst until it respawns.
    pub experience_consumed: bool,
    pub xp: Experience,
    /// `DATA_SCORE_ID`.
    pub score: i32,
    /// `LastDeathLocation`: dimension and block position.
    pub last_death: Option<(String, BlockPos)>,
    /// `respawn` (`ServerPlayer.RespawnConfig`): where the player respawns.
    pub respawn: Option<RespawnConfig>,
    /// `SLEEPING_POS_ID`: the bed the player sleeps in.
    pub sleeping_pos: Option<BlockPos>,
    /// `sleepCounter` (`SleepTimer`): ticks asleep up to 100, then 100..110 after waking.
    pub sleep_timer: i32,
    /// A plugin's `Player.setSleepingIgnored` (Bukkit's `fauxSleeping`): counts as sleeping
    /// for the night skip while someone really sleeps. Not saved.
    pub sleeping_ignored: bool,
    /// Something woke the player (`stopSleeping` from damage or death); the sleep pass of the
    /// world tick gets it out of bed.
    pub wake: bool,
    pub combat: CombatTracker,
    /// `tickCount`.
    pub tick_count: i32,
    /// The entities on the player's shoulders.
    pub shoulders: super::shoulder::Shoulders,
    /// `lastClimbablePos`'s fall location.
    pub last_climbable: Option<FallLocation>,
    /// The fluids around the player after the last update.
    pub fluids: FluidContact,
    /// Portal cooldown, the portal the player stands in and the end credits.
    pub portal: crate::portal::PortalState,
    /// The player's box at the end of the last tick, for portals touched on the way.
    last_box: Option<vanillite_base::Aabb>,
    /// `attackStrengthTicker`.
    pub attack_ticker: i32,
    /// `takeXpDelay`: ticks until the next experience orb can be taken.
    pub take_xp_delay: i32,
    /// `lastLevelUpTime`: the tick of the last level-up sound.
    pub last_level_up_time: i32,
    /// `activeEffects` with their attribute modifiers.
    pub effects: crate::effect::ActiveEffects,
    /// `wardenSpawnTracker`.
    pub warden_spawn_tracker: crate::block::sculk_shrieker::WardenSpawnTracker,
    /// The positions of the distance triggers.
    pub tracking: crate::advancements::tracking::Tracking,
    /// `moveDist` and `nextStep`: when the next step makes a sound and a vibration.
    pub move_dist: f32,
    pub next_step: f32,
    /// `crystalSoundIntensity` and `lastCrystalSoundPlayTick` (the amethyst chime of steps).
    pub crystal_intensity: f32,
    pub last_crystal_tick: i32,
    /// Game events of the player's moves (`STEP`, `SWIM`, `HIT_GROUND`) for the world to
    /// post: event, position and the affected block.
    pub game_events: Vec<(u16, Vec3, Option<vanillite_data::block::BlockState>)>,
    /// World changes of the player's contact with blocks (trampled farmland, redstone ore
    /// stepped on) for the world to apply.
    pub world_actions: Vec<crate::mob::WorldAction>,
    /// Frost Walker and Soul Speed (`lastPos`, active location-based effects).
    pub location: crate::enchant::location::LocationState,
    /// The equipment's and the location-based enchantments' attribute modifiers as last sent
    /// to the client (`collectEquipmentChanges`).
    pub equipment_modifiers: crate::effect::attribute::Modifiers,
    /// The attribute instances a command or plugin changed (`AttributeMap`): their base
    /// values and permanent modifiers, saved as `attributes`; the values live in the effects'
    /// modifiers.
    pub attributes: crate::mob::entity_nbt::NbtAttributes,
    /// The location-based modifiers changed: the attributes are synced again.
    pub equipment_dirty: bool,
    /// `getTicksFrozen` and `isInPowderSnow`.
    pub ticks_frozen: i32,
    pub in_powder_snow: bool,
    /// `lastHurtMob` with the tick it was hit (`lastHurtMobTimestamp`).
    pub last_hurt_mob: Option<(vanillite_world::entity::EntityId, i32)>,
    /// `swingState`: the ticks of the running swing and its duration.
    pub swing: Option<(i32, i32)>,
    /// `spawnExtraParticlesOnFall`: the next landing bursts block particles (mace smash).
    pub spawn_extra_particles_on_fall: bool,
    /// `lastHurtByMob` with the tick it hit (`lastHurtByMobTimestamp`).
    pub last_hurt_by_mob: Option<(vanillite_world::entity::EntityId, i32)>,
    /// `lastHurtByPlayer` with the ticks it is still remembered (`lastHurtByPlayerMemoryTime`).
    pub last_hurt_by_player: Option<(vanillite_world::entity::EntityId, i32)>,
    /// The display names of `lastHurtByMob` and `lastHurtByPlayer` (`getKillCredit` of the
    /// death message).
    pub last_hurt_by_names: (Option<Component>, Option<Component>),
    /// The item in use (`useItem`): its hand and the ticks it has been used.
    pub using_item: Option<crate::player::using::UsingItem>,
    /// `recentKineticEnemies`: the entities the spear in use hit, with the game time.
    pub stabbed: Vec<(vanillite_world::entity::EntityId, i64)>,
    /// A hit blocked by the item in use whose attacker reacts itself
    /// ([`DamageSource::own_blocked_by_item`]): whether it was fully blocked.
    pub blocked_by_item: Option<bool>,
    /// The player's own random (`Entity.random`).
    pub random: LegacyRandom,
    /// `cooldowns`.
    pub cooldowns: crate::player::cooldowns::ItemCooldowns,
    /// The map ids in the inventory at the last inventory tick.
    pub carried_maps: Vec<i32>,
    /// The item in the main hand at the last tick (`lastItemInMainHand`).
    pub last_main_hand_item: u16,
    /// `Abilities.flying`: the player flies (creative or spectator flight).
    pub flying: bool,
    /// `raidOmenPosition` and what the omen effects see of villages and raids.
    pub raid_omen: crate::raid::PlayerOmen,
    /// The server moves the player itself (`canSimulateMovement` for a player no client
    /// moves: the parity scenarios' mock players), see [`simulate_travel`].
    pub simulate_movement: bool,
    /// `yBodyRot`, only kept while the server moves the player.
    pub y_body_rot: f32,
    /// `horizontalCollision` of the last server-side move.
    pub(crate) horizontal_collision: bool,
    /// `fallFlyTicks`: ticks gliding.
    pub fall_fly_ticks: i32,
    /// `Player.fishing`: the player's bobber.
    pub fishing: Option<EntityId>,
    /// `currentImpulseImpactPos`: the fall damage of the current impulse (a mace smash)
    /// counts from here.
    pub impulse_impact: Option<Vec3>,
    /// `currentImpulseContextResetGraceTime`.
    pub impulse_grace: i32,
    /// `autoSpinAttackTicks`, `autoSpinAttackDmg` and `autoSpinAttackItemStack` of a riptide.
    pub spin_attack: Option<crate::trident::SpinAttack>,
    /// How far `Player.travel` moves a player its client moves in this tick's `aiStep`
    /// ([`do_tick_ai`]): Vanilla pushes and touches the entities from there, then
    /// `tickPlayer` puts the player back where its client has it.
    pub server_step: Vec3,
    /// A plugin decided at the last death whether the inventory stays (else the game rule at
    /// the respawn decides, as in Vanilla).
    pub keep_inventory_override: Option<bool>,
    first_tick: bool,
    sent: Sent,
    recorded: Recorded,
}

/// `ServerPlayer.lastRecorded*`: the values the built-in score criteria got last.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Recorded {
    health: f32,
    food: i32,
    air: i32,
    armor: i32,
    experience: i32,
    level: i32,
}

impl Default for Recorded {
    fn default() -> Self {
        Recorded {
            // Java's `Float.MIN_VALUE`.
            health: f32::from_bits(1),
            food: i32::MIN,
            air: i32::MIN,
            armor: i32::MIN,
            experience: i32::MIN,
            level: i32::MIN,
        }
    }
}

impl Default for Survival {
    fn default() -> Self {
        Survival {
            own_random: LegacyRandom::new(crate::mob::new_unique_seed()).state(),
            health: MAX_HEALTH,
            absorption: 0.0,
            food: FoodData::default(),
            air: MAX_AIR,
            fire_ticks: 0,
            fall_distance: 0.0,
            hurt_time: 0,
            arrows: crate::mob::projectile::StuckArrows::default(),
            damage_cooldown: 0,
            last_hurt: 0.0,
            death_time: 0,
            removed: false,
            experience_consumed: false,
            xp: Experience::default(),
            score: 0,
            last_death: None,
            respawn: None,
            sleeping_pos: None,
            sleep_timer: 0,
            sleeping_ignored: false,
            wake: false,
            combat: CombatTracker::default(),
            tick_count: 0,
            shoulders: super::shoulder::Shoulders::default(),
            last_climbable: None,
            fluids: FluidContact::default(),
            portal: crate::portal::PortalState::default(),
            last_box: None,
            attack_ticker: 0,
            take_xp_delay: 0,
            last_level_up_time: 0,
            effects: crate::effect::ActiveEffects::default(),
            warden_spawn_tracker: crate::block::sculk_shrieker::WardenSpawnTracker::default(),
            tracking: crate::advancements::tracking::Tracking::default(),
            move_dist: 0.0,
            next_step: 1.0,
            crystal_intensity: 0.0,
            last_crystal_tick: 0,
            game_events: Vec::new(),
            world_actions: Vec::new(),
            location: crate::enchant::location::LocationState::default(),
            equipment_modifiers: crate::effect::attribute::Modifiers::default(),
            attributes: crate::mob::entity_nbt::NbtAttributes::default(),
            equipment_dirty: false,
            ticks_frozen: 0,
            in_powder_snow: false,
            last_hurt_mob: None,
            spawn_extra_particles_on_fall: false,
            swing: None,
            last_hurt_by_mob: None,
            last_hurt_by_player: None,
            last_hurt_by_names: (None, None),
            using_item: None,
            stabbed: Vec::new(),
            blocked_by_item: None,
            random: LegacyRandom::new(crate::mob::new_unique_seed()),
            cooldowns: crate::player::cooldowns::ItemCooldowns::default(),
            carried_maps: Vec::new(),
            last_main_hand_item: 0,
            flying: false,
            raid_omen: crate::raid::PlayerOmen::default(),
            simulate_movement: false,
            y_body_rot: 0.0,
            horizontal_collision: false,
            fall_fly_ticks: 0,
            fishing: None,
            impulse_impact: None,
            impulse_grace: 0,
            spin_attack: None,
            server_step: Vec3::ZERO,
            keep_inventory_override: None,
            first_tick: true,
            sent: Sent::default(),
            recorded: Recorded::default(),
        }
    }
}

/// `ServerPlayer.RespawnConfig`: a respawn position (`LevelData.RespawnData`) and whether it
/// is kept without a bed or anchor there.
#[derive(Clone, Debug, PartialEq)]
pub struct RespawnConfig {
    /// `dimension`.
    pub dimension: String,
    /// `pos`.
    pub pos: BlockPos,
    /// `yaw`, wrapped to -180..180.
    pub yaw: f32,
    /// `pitch`, -90..90.
    pub pitch: f32,
    /// `forced`.
    pub forced: bool,
}

impl RespawnConfig {
    /// `RespawnConfig.CODEC`; `None` when a field is missing or out of range.
    pub fn load(data: &Compound) -> Option<RespawnConfig> {
        let (Some(Tag::String(dimension)), Some(Tag::IntArray(pos))) =
            (data.get("dimension"), data.get("pos"))
        else {
            return None;
        };
        let [x, y, z] = pos.as_slice() else {
            return None;
        };
        let yaw = float(data, "yaw").filter(|v| (-180.0..=180.0).contains(v))?;
        let pitch = float(data, "pitch").filter(|v| (-90.0..=90.0).contains(v))?;
        Some(RespawnConfig {
            dimension: dimension.clone(),
            pos: BlockPos::new(*x, *y, *z),
            yaw,
            pitch,
            forced: matches!(data.get("forced"), Some(Tag::Byte(b)) if *b != 0),
        })
    }

    /// The saved compound.
    pub fn save(&self) -> Compound {
        let mut out = Compound::with_capacity(5);
        out.insert("dimension", Tag::String(self.dimension.clone()));
        out.insert(
            "pos",
            Tag::IntArray(vec![self.pos.x, self.pos.y, self.pos.z]),
        );
        out.insert("yaw", Tag::Float(self.yaw));
        out.insert("pitch", Tag::Float(self.pitch));
        if self.forced {
            out.insert("forced", Tag::Byte(1));
        }
        out
    }
}

/// What the survival logic reads and writes besides the player.
pub struct Env<'a> {
    pub types: &'a DamageTypes,
    pub facts: &'a BlockFacts,
    pub blocks: &'a dyn BlockAccess,
    pub rules: &'a GameRules,
    pub difficulty: Difficulty,
    pub game_time: i64,
    /// The world's lowest block.
    pub min_y: i32,
    /// `logicalHeight` of the world's dimension type.
    pub logical_height: i32,
    /// The world's dimension, e.g. `minecraft:overworld`.
    pub dimension: &'a str,
    /// `Level.isRainingAt`.
    pub raining_at: &'a dyn Fn(BlockPos) -> bool,
    /// The world border, where players take `outsideBorder` damage.
    pub border: Option<&'a crate::level::border::WorldBorder>,
    /// An End gateway at the position takes a player: `false` while it cools down, else it
    /// starts its cooldown (`TheEndGatewayBlockEntity.triggerCooldown`).
    pub enter_gateway: &'a mut dyn FnMut(BlockPos) -> bool,
    pub random: &'a mut LegacyRandom,
    pub out: &'a mut Vec<Effect>,
    /// Plugin hooks (damage).
    pub hooks: &'a dyn crate::hooks::GameHooks,
    /// What plugins may read during these hooks.
    pub hook_world: crate::hooks::HookWorld<'a>,
    /// `Player.getDisplayName` by name and UUID: the name in its team's format.
    pub display_name: &'a dyn Fn(&str, u128) -> Component,
    /// `Entity.getTeam` by score holder (a player's name, another entity's UUID string): its
    /// scoreboard team.
    pub entity_team: &'a dyn Fn(&str) -> Option<crate::scoreboard::TeamFacts>,
}

impl std::fmt::Debug for Env<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("difficulty", &self.difficulty)
            .field("game_time", &self.game_time)
            .field("dimension", &self.dimension)
            .finish_non_exhaustive()
    }
}

/// One accepted client move (`handlePlayerPositionChange`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MoveRecord {
    /// Position before the move.
    pub from: Vec3,
    /// Position after the move.
    pub to: Vec3,
    /// On ground before the move.
    pub was_on_ground: bool,
    /// On ground as the client reports it now.
    pub on_ground: bool,
    /// The target is above the last good position.
    pub moved_upwards: bool,
    /// The move passed the collision checks; a rejected move ("moved wrongly", into a block)
    /// sent the player back to `from` (`to == from`).
    pub accepted: bool,
}

pub(crate) fn block_pos(v: Vec3) -> BlockPos {
    BlockPos::new(
        vanillite_base::math::floor(v.x),
        vanillite_base::math::floor(v.y),
        vanillite_base::math::floor(v.z),
    )
}

fn float(data: &Compound, key: &str) -> Option<f32> {
    match data.get(key) {
        Some(Tag::Float(v)) if v.is_finite() => Some(*v),
        _ => None,
    }
}

fn int(data: &Compound, key: &str) -> Option<i32> {
    data.get(key)
        .and_then(Tag::as_i64)
        .and_then(|v| i32::try_from(v).ok())
}

impl Survival {
    /// `isDeadOrDying`.
    pub fn is_dead_or_dying(&self) -> bool {
        self.health <= 0.0
    }

    /// `isHurt`.
    pub fn is_hurt(&self) -> bool {
        self.health > 0.0 && self.health < self.max_health()
    }

    /// `getMaxHealth`: the base maximum with the effect modifiers (Health Boost).
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    pub fn max_health(&self) -> f32 {
        if self.effects.modifiers.is_empty() {
            return MAX_HEALTH;
        }
        self.effects.attribute(
            crate::effect::attribute::Attribute::MAX_HEALTH,
            f64::from(MAX_HEALTH),
        ) as f32
    }

    /// `getMaxAbsorption`.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    pub fn max_absorption(&self) -> f32 {
        self.effects
            .attribute(crate::effect::attribute::Attribute::MAX_ABSORPTION, 0.0) as f32
    }

    /// Reads the survival fields of a player data file.
    pub fn load(data: &Compound) -> Survival {
        let mut s = Survival {
            food: FoodData::load(data),
            ..Survival::default()
        };
        // The effects (with their modifiers) first: Health Boost raises the maximum.
        s.effects.load(data.get("active_effects"));
        s.effects.resend_all();
        s.load_attributes(data.get("attributes"));
        let max = s.max_health();
        s.health = float(data, "Health").unwrap_or(max).clamp(0.0, max);
        s.absorption = float(data, "AbsorptionAmount")
            .unwrap_or(0.0)
            .clamp(0.0, s.max_absorption());
        s.air = int(data, "Air").unwrap_or(MAX_AIR);
        s.fire_ticks = int(data, "Fire").unwrap_or(0);
        s.fall_distance = match data.get("fall_distance") {
            Some(Tag::Double(v)) if v.is_finite() => *v,
            _ => 0.0,
        };
        s.hurt_time = int(data, "HurtTime").unwrap_or(0);
        s.death_time = int(data, "DeathTime").unwrap_or(0);
        s.xp = Experience {
            level: int(data, "XpLevel").unwrap_or(0),
            progress: float(data, "XpP").unwrap_or(0.0),
            total: int(data, "XpTotal").unwrap_or(0),
            seed: int(data, "XpSeed").unwrap_or(0),
        };
        s.score = int(data, "Score").unwrap_or(0);
        if let Some(Tag::Compound(abilities)) = data.get("abilities") {
            s.flying = matches!(abilities.get("flying"), Some(Tag::Byte(b)) if *b != 0);
        }
        s.warden_spawn_tracker = crate::block::sculk_shrieker::WardenSpawnTracker::load(
            data.get("warden_spawn_tracker"),
        );
        s.portal = crate::portal::PortalState::load(data);
        if let Some(Tag::IntArray(v)) = data.get("raid_omen_position")
            && let [x, y, z] = v.as_slice()
        {
            s.raid_omen.position = Some(BlockPos::new(*x, *y, *z));
        }
        s.tracking = crate::advancements::tracking::Tracking::load(data);
        s.spawn_extra_particles_on_fall =
            matches!(data.get("spawn_extra_particles_on_fall"), Some(Tag::Byte(b)) if *b != 0);
        if let Some(Tag::Compound(loc)) = data.get("LastDeathLocation")
            && let (Some(Tag::String(dim)), Some(Tag::IntArray(pos))) =
                (loc.get("dimension"), loc.get("pos"))
            && let [x, y, z] = pos.as_slice()
        {
            s.last_death = Some((dim.clone(), BlockPos::new(*x, *y, *z)));
        }
        if let Some(Tag::Compound(respawn)) = data.get("respawn") {
            s.respawn = RespawnConfig::load(respawn);
        }
        // `readAdditionalSaveData`: a player saved asleep wakes at once (`stopSleeping` with
        // its bed's chunk not loaded yet), which resets the timer.
        s.sleep_timer = int(data, "SleepTimer").unwrap_or(0);
        if matches!(data.get("sleeping_pos"), Some(Tag::IntArray(p)) if p.len() == 3) {
            s.sleep_timer = 0;
        }
        s.shoulders = super::shoulder::Shoulders::load(data);
        s
    }

    /// Writes the survival fields into a player data file (Vanilla's names and types).
    pub fn save(&self, out: &mut Compound) {
        out.insert("Health", Tag::Float(self.health));
        out.insert("AbsorptionAmount", Tag::Float(self.absorption));
        out.insert(
            "Air",
            Tag::Short(i16::try_from(self.air).unwrap_or(i16::MAX)),
        );
        out.insert(
            "Fire",
            Tag::Short(i16::try_from(self.fire_ticks).unwrap_or(i16::MAX)),
        );
        out.insert("fall_distance", Tag::Double(self.fall_distance));
        out.insert(
            "HurtTime",
            Tag::Short(i16::try_from(self.hurt_time).unwrap_or(0)),
        );
        out.insert(
            "DeathTime",
            Tag::Short(i16::try_from(self.death_time).unwrap_or(0)),
        );
        out.insert("XpLevel", Tag::Int(self.xp.level));
        out.insert("XpP", Tag::Float(self.xp.progress));
        out.insert("XpTotal", Tag::Int(self.xp.total));
        out.insert("XpSeed", Tag::Int(self.xp.seed));
        out.insert("Score", Tag::Int(self.score));
        out.insert(
            "warden_spawn_tracker",
            Tag::Compound(self.warden_spawn_tracker.save()),
        );
        match self.effects.save() {
            Some(list) => out.insert("active_effects", list),
            None => {
                out.remove("active_effects");
            }
        }
        out.insert("attributes", self.attributes.to_list());
        self.portal.save(out);
        self.tracking.save(out);
        self.food.save(out);
        out.insert(
            "spawn_extra_particles_on_fall",
            Tag::Byte(i8::from(self.spawn_extra_particles_on_fall)),
        );
        out.remove("raid_omen_position");
        if let Some(p) = self.raid_omen.position {
            out.insert("raid_omen_position", Tag::IntArray(vec![p.x, p.y, p.z]));
        }
        out.remove("LastDeathLocation");
        if let Some((dim, pos)) = &self.last_death {
            let mut loc = Compound::with_capacity(2);
            loc.insert("dimension", Tag::String(dim.clone()));
            loc.insert("pos", Tag::IntArray(vec![pos.x, pos.y, pos.z]));
            out.insert("LastDeathLocation", Tag::Compound(loc));
        }
        out.remove("respawn");
        if let Some(respawn) = &self.respawn {
            out.insert("respawn", Tag::Compound(respawn.save()));
        }
        out.insert(
            "SleepTimer",
            Tag::Short(i16::try_from(self.sleep_timer).unwrap_or(0)),
        );
        out.remove("sleeping_pos");
        if let Some(p) = self.sleeping_pos {
            out.insert("sleeping_pos", Tag::IntArray(vec![p.x, p.y, p.z]));
        }
        self.shoulders.save(out);
    }

    /// `restoreFrom` for a respawn after death: health, food and air start over; experience,
    /// score and the death location survive only as Vanilla keeps them.
    /// `keep_inventory` is the game rule or a spectator dying.
    pub fn respawned(&self, keep_inventory: bool) -> Survival {
        // `restoreFrom`: the portal state and the credits carry over.
        let mut s = Survival {
            last_death: self.last_death.clone(),
            respawn: self.respawn.clone(),
            warden_spawn_tracker: self.warden_spawn_tracker,
            portal: self.portal.clone(),
            shoulders: self.shoulders.clone(),
            tracking: self.tracking.respawned(),
            ..Survival::default()
        };
        if keep_inventory {
            s.xp = self.xp;
            s.score = self.score;
        }
        s.xp.seed = self.xp.seed;
        // `assignBaseValues`: the base values carry over, the permanent modifiers do not.
        s.attributes = self.attributes.clone();
        for a in &mut s.attributes.0 {
            a.modifiers.clear();
        }
        for (a, v) in self.effects.modifiers.bases() {
            s.effects.modifiers.set_base(a, v);
        }
        s.effects.modifiers.take_dirty();
        s.health = s.max_health();
        s
    }

    /// `AttributeMap.apply` of a player's `attributes` list: base values and permanent
    /// modifiers, kept for the save.
    fn load_attributes(&mut self, tag: Option<&Tag>) {
        use vanillite_data::entity::ids;
        for a in crate::mob::entity_nbt::read_attributes(ids::PLAYER, tag) {
            let base = a.attribute.sanitize(a.base);
            if base.to_bits() != a.attribute.base_of(ids::PLAYER).to_bits() {
                self.effects.modifiers.set_base(a.attribute, base);
            }
            for x in &a.modifiers {
                self.effects
                    .modifiers
                    .set(a.attribute, &x.id, x.amount, x.operation);
            }
            self.attributes.0.push(a);
        }
    }

    /// The player was moved without walking (teleport, portal): the blocks it touches next
    /// tick are only those at its new place.
    pub fn teleported(&mut self) {
        self.last_box = None;
    }

    /// `Player.giveExperiencePoints`: the score grows by the points too. Returns the volume of
    /// the level-up sound (`entity.player.levelup`) to play at the player, if one plays: every
    /// fifth level, at most once per 100 ticks.
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    pub fn give_experience_points(&mut self, points: i32) -> Option<f32> {
        self.score = self.score.wrapping_add(points);
        let tick = self.tick_count;
        let last = &mut self.last_level_up_time;
        let mut volume = None;
        self.xp.give_points_with(points, &mut |level| {
            if level % 5 == 0 && (*last as f32) < tick as f32 - 100.0 {
                let v = if level > 30 { 1.0 } else { level as f32 / 30.0 };
                volume = Some(v * 0.75);
                *last = tick;
            }
        });
        volume
    }

    /// Forces the next [`sync`](Self::sync) to send health, food and experience
    /// (`resetSentInfo`).
    pub fn reset_sent(&mut self) {
        self.sent = Sent::default();
    }

    /// Forces the next [`sync`](Self::sync) to send the experience (`lastSentExp = -1`).
    pub fn resend_experience(&mut self) {
        self.sent.exp = Sent::default().exp;
    }

    /// `doTick`'s `updateScoreForCriteria`: the built-in criteria whose values changed since
    /// the last call, with the new values (`Mth.ceil` of health with absorption, food, air,
    /// armour, total experience and level).
    pub fn changed_criteria(&mut self) -> Vec<(CriteriaKind, i32)> {
        let mut out = Vec::new();
        let r = &mut self.recorded;
        let health = self.health + self.absorption;
        if health != r.health {
            r.health = health;
            // `Mth.ceil`: the float's ceiling as an int, saturating like Java's cast.
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            out.push((CriteriaKind::Health, health.ceil() as i32));
        }
        // TODO(#27): the armour value of the equipment; it is 0 until armour exists.
        let armor = 0;
        for (kind, value, last) in [
            (CriteriaKind::Food, self.food.food_level, &mut r.food),
            (CriteriaKind::Air, self.air, &mut r.air),
            (CriteriaKind::Armor, armor, &mut r.armor),
            (CriteriaKind::Experience, self.xp.total, &mut r.experience),
            (CriteriaKind::Level, self.xp.level, &mut r.level),
        ] {
            if value != *last {
                *last = value;
                out.push((kind, value));
            }
        }
        out
    }

    /// `doTick`'s packets: `set_health` and `set_experience` when they changed.
    pub fn sync(&mut self, sink: &dyn crate::player::PacketSink) {
        let saturation_zero = self.food.saturation == 0.0;
        if self.health != self.sent.health
            || self.sent.food != self.food.food_level
            || saturation_zero != self.sent.saturation_zero
        {
            sink.send(&SetHealth {
                health: self.health,
                food: self.food.food_level,
                saturation: self.food.saturation,
            });
            self.sent.health = self.health;
            self.sent.food = self.food.food_level;
            self.sent.saturation_zero = saturation_zero;
        }
        if self.xp.total != self.sent.exp {
            self.sent.exp = self.xp.total;
            sink.send(&SetExperience {
                progress: self.xp.progress,
                level: self.xp.level,
                total: self.xp.total,
            });
        }
    }
}

/// `checkTotemDeathProtection` of a dying player: a death protection item in a hand is used
/// up and the player lives on with one health point.
fn check_totem(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    kind: DamageKind,
) -> bool {
    if !crate::totem::can_protect(env.types, kind) {
        return false;
    }
    let Some((slot, effects)) =
        crate::totem::protecting_hand(player.inventory.selected_item(), &player.inventory.offhand)
    else {
        return false;
    };
    if !env
        .hooks
        .entity_resurrect(env.hook_world, entity.as_ref().uuid(), "minecraft:player")
    {
        return false;
    }
    let index = match slot {
        crate::item::EquipmentSlot::OffHand => crate::inventory::OFFHAND,
        _ => player.inventory.selected,
    };
    let used = player.inventory.get(index).with_count(1);
    if let Some(s) = player.inventory.get_mut(index) {
        s.shrink(1);
    }
    player.stats.award(
        crate::stats::Stat::of(crate::stats::StatType::Used, used.item),
        1,
    );
    player
        .advancements
        .trigger(crate::advancements::trigger::Event::Item(
            crate::advancements::trigger::TriggerKind::USED_TOTEM,
            used.clone(),
        ));
    // `causeUseVibration(ITEM_INTERACT_FINISH)`.
    if crate::consumable::use_vibrations(&used) {
        let e = entity.as_ref();
        crate::survival::game_event(
            env.out,
            crate::level::game_event::events().item_interact_finish,
            e.pos(),
            Some(e.id()),
        );
    }
    set_health(entity, &mut player.survival, 1.0);
    // `ApplyStatusEffectsConsumeEffect`: one roll of the player's random per entry.
    let rolls: Vec<f32> = effects
        .iter()
        .filter(|e| matches!(e, crate::consumable::ConsumeEffect::Apply(..)))
        .map(|_| player.survival.random.next_float())
        .collect();
    let mut sounds = Vec::new();
    {
        let mut host = crate::effect::living::PlayerHost {
            entity: &mut *entity,
            player: &mut *player,
            env: &mut *env,
            players: None,
            names: &[],
        };
        let mut rolls = rolls.into_iter();
        let mut roll = || rolls.next().unwrap_or(0.0);
        crate::totem::apply_effects(
            &effects,
            vanillite_data::entity::ids::PLAYER,
            &mut host,
            &mut roll,
            &mut |s| sounds.push(s.to_owned()),
        );
    }
    let at = entity.as_ref().pos();
    for s in sounds {
        crate::totem::play_consume_sound(
            env.out,
            at,
            &s,
            vanillite_protocol::packets::play::clientbound::SoundSource::Players,
        );
    }
    let (id, network_id) = (entity.as_ref().id(), entity.as_ref().network_id().0);
    crate::totem::animation(env.out, id, network_id);
    true
}

/// `setHealth`: clamped to `[0, max]`; marks the entity data dirty when it changed.
fn set_health(entity: &mut EntityMut<'_>, s: &mut Survival, health: f32) {
    let health = health.clamp(0.0, s.max_health());
    if health != s.health {
        s.health = health;
        entity.mark_data_dirty();
    }
}

fn set_air(entity: &mut EntityMut<'_>, s: &mut Survival, air: i32) {
    if air != s.air {
        s.air = air;
        entity.mark_data_dirty();
    }
}

/// `heal`.
pub fn heal(entity: &mut EntityMut<'_>, player: &mut PlayerData, amount: f32) {
    let s = &mut player.survival;
    if s.health > 0.0 {
        let health = s.health + amount;
        set_health(entity, s, health);
    }
}

/// `causeFoodExhaustion`.
pub fn cause_food_exhaustion(player: &mut PlayerData, amount: f32) {
    if !Abilities::of(player.game_mode).invulnerable {
        player.survival.food.add_exhaustion(amount);
    }
}

/// `Player.isInvulnerableTo` with `ServerPlayer`'s additions (a client that has not loaded the
/// world takes no damage).
fn is_invulnerable_to(player: &PlayerData, env: &Env<'_>, kind: DamageKind) -> bool {
    if !player.connection.movement.has_client_loaded() {
        return true;
    }
    let t = env.types;
    let rules = env.rules;
    if crate::enchant::location::frost_walker_immune(
        t,
        kind,
        player.inventory.equipment(crate::item::EquipmentSlot::Feet),
    ) {
        return true;
    }
    if t.is(kind, tag::IS_DROWNING) {
        !rules.bool(Rule::DROWNING_DAMAGE)
    } else if t.is(kind, tag::IS_FALL) {
        !rules.bool(Rule::FALL_DAMAGE)
    } else if t.is(kind, tag::IS_FIRE) {
        !rules.bool(Rule::FIRE_DAMAGE)
    } else if t.is(kind, tag::IS_FREEZING) {
        !rules.bool(Rule::FREEZE_DAMAGE)
    } else {
        false
    }
}

/// `hurtServer` of a player (`ServerPlayer` → `Player` → `LivingEntity`). Returns whether the
/// damage was taken.
pub fn hurt(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    source: DamageSource,
    amount: f32,
) -> bool {
    hurt_with(
        entity,
        player,
        env,
        source,
        amount,
        None,
        (None, None),
        SourceNames::default(),
    )
}

/// [`hurt`] with the UUIDs of the source's causing and direct entities for the plugins'
/// damage and death events (resolved by callers that have the entities, only when a plugin
/// listens: [`wants_attackers`]).
pub fn hurt_by(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    source: DamageSource,
    amount: f32,
    attackers: (Option<u128>, Option<u128>),
) -> bool {
    hurt_with(
        entity,
        player,
        env,
        source,
        amount,
        None,
        attackers,
        SourceNames::default(),
    )
}

/// Whether the plugins' damage or death events need the UUIDs of a player's attackers.
pub fn wants_attackers(hooks: &dyn crate::hooks::GameHooks) -> bool {
    hooks.wants(vanillite_abi::event::ENTITY_DAMAGE)
        || hooks.wants(vanillite_abi::event::PLAYER_DEATH)
}

/// [`hurt`] with the armour effectiveness of the source's weapon (Breach, see
/// [`crate::mob::attack::armor_effectiveness`]) and the UUIDs of the causing and direct
/// entities (see [`hurt_by`]).
#[allow(clippy::too_many_arguments)]
pub fn hurt_with(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    source: DamageSource,
    amount: f32,
    armor_effectiveness: Option<&dyn Fn(f32) -> f32>,
    attackers: (Option<u128>, Option<u128>),
    names: SourceNames,
) -> bool {
    let types = env.types;
    let kind = source.kind;
    if is_invulnerable_to(player, env, kind) || immune_to_damage(entity, player, env, &source) {
        return false;
    }
    if Abilities::of(player.game_mode).invulnerable
        && !types.is(kind, tag::BYPASSES_INVULNERABILITY)
    {
        return false;
    }
    if player.survival.is_dead_or_dying() {
        return false;
    }
    super::shoulder::remove_entities(entity, player, env);
    if types.is(kind, tag::IS_FIRE)
        && player
            .survival
            .effects
            .has(crate::effect::MobEffect::FIRE_RESISTANCE)
    {
        return false;
    }
    let mut damage = amount;
    if source.scales_with_difficulty(types, source.cause_is_mob) {
        match env.difficulty {
            Difficulty::Peaceful => damage = 0.0,
            Difficulty::Easy => damage = (damage / 2.0 + 1.0).min(damage),
            Difficulty::Normal => {}
            Difficulty::Hard => damage = damage * 3.0 / 2.0,
        }
    }
    let uuid = entity.as_ref().uuid();
    let (attacker, direct) = attackers;
    match env.hooks.entity_damage(
        env.hook_world,
        uuid,
        "minecraft:player",
        &types.get(kind).id,
        damage,
        attacker,
        direct,
    ) {
        Some(changed) => damage = changed,
        None => return false,
    }
    if damage == 0.0 {
        return false;
    }
    // `LivingEntity.hurtServer`: a hit wakes a sleeper.
    if player.survival.sleeping_pos.is_some() {
        player.survival.wake = true;
    }
    if damage < 0.0 {
        damage = 0.0;
    }
    let (blocked_amount, blocker) = apply_item_blocking(entity, player, env, &source, damage);
    damage -= blocked_amount;
    let blocked = blocked_amount > 0.0;
    // `#damages_helmet` (falling anvils, blocks, stalactites) with a helmet on: `hurtHelmet`
    // (`doHurtEquipment(HEAD)`) and three quarters of the damage.
    if types.is(kind, tag::DAMAGES_HELMET) && !player.inventory.get(39).is_empty() {
        hurt_armor(entity, player, env, &source, damage, 39..40);
        damage *= 0.75;
    }
    if !damage.is_finite() {
        damage = f32::MAX;
    }
    let cause_name = names.cause.as_ref().map(|a| a.name.clone());
    let responsible_name = names.responsible.clone();
    let s = &mut player.survival;
    let took_full = if s.damage_cooldown > 10 && !types.is(kind, tag::BYPASSES_COOLDOWN) {
        if damage <= s.last_hurt {
            return false;
        }
        let delta = damage - s.last_hurt;
        actually_hurt(
            entity,
            player,
            env,
            source,
            delta,
            armor_effectiveness,
            names,
        );
        player.survival.last_hurt = damage;
        false
    } else {
        s.last_hurt = damage;
        s.damage_cooldown = 20;
        actually_hurt(
            entity,
            player,
            env,
            source,
            damage,
            armor_effectiveness,
            names,
        );
        player.survival.hurt_time = 10;
        true
    };
    let id = entity.as_ref().id();
    let pos = entity.as_ref().pos();
    // `setLastHurtByMob`: the living entity that caused it.
    if let Some(cause) = source.cause
        && cause != id
    {
        player.survival.last_hurt_by_mob = Some((cause, player.survival.tick_count));
        player.survival.last_hurt_by_names.0 = cause_name;
    }
    // `resolvePlayerResponsibleForDamage`: remembered for 100 ticks.
    if let Some(by) = source.responsible_player
        && by != id
    {
        player.survival.last_hurt_by_player = Some((by, LAST_HURT_BY_PLAYER_MEMORY));
        player.survival.last_hurt_by_names.1 = responsible_name;
    }
    if took_full
        && blocked
        && let Some(block_sound) = blocker
    {
        // `BlocksAttacks.onBlocked` of the item in use: the block sound, if any, instead of
        // the damage event (an item that broke has no component left: the damage event).
        if let Some(sound) = block_sound {
            let pitch = 0.8 + env.random.next_float() * 0.4;
            let seed = crate::survival::sound_seed();
            crate::survival::play_sound(
                env.out,
                None,
                pos,
                &sound,
                SoundSource::Players,
                1.0,
                pitch,
                seed,
            );
        }
    } else if took_full {
        env.out.push(Effect::Tracking {
            entity: id,
            with_self: true,
            packet: SharedPacket::of(&DamageEvent {
                entity_id: entity.as_ref().network_id().0,
                source_type: i32::from(kind.0),
                source_cause_id: -1,
                source_direct_id: -1,
                source_position: source.position.map(|p| vanillite_protocol::types::Vec3 {
                    x: p.x,
                    y: p.y,
                    z: p.z,
                }),
            }),
        });
    }
    if took_full {
        if !types.is(kind, tag::NO_IMPACT) && (!blocked || damage > 0.0) {
            entity.set_flags(EntityFlags::HURT_MARKED, true);
        }
        let fully_blocked = blocked && damage <= 0.0;
        if !types.is(kind, tag::NO_KNOCKBACK) && !fully_blocked {
            // `dealDefaultKnockback`: away from the source position.
            let (xd, zd) = source.knockback_direction(pos);
            let on_ground = entity.as_ref().flags().contains(EntityFlags::ON_GROUND);
            let resistance = knockback_resistance(player);
            let v = crate::mob::combat::knockback(
                entity.as_ref().velocity(),
                on_ground,
                f64::from(0.4_f32),
                xd,
                zd,
                resistance,
                // `this.random`: the player's own.
                &mut player.survival.random,
            );
            if let Some(v) = v {
                entity.set_velocity(v);
            }
            if !blocked {
                // `ServerPlayer.indicateDamage`: the camera tilts away from the source.
                #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
                let yaw = (zd.atan2(xd) * 180.0 / f64::from(std::f32::consts::PI)
                    - f64::from(entity.as_ref().rot().y_rot)) as f32;
                env.out.push(Effect::To {
                    player: id,
                    packet: SharedPacket::of(
                        &vanillite_protocol::packets::play::clientbound::HurtAnimation {
                            entity_id: entity.as_ref().network_id().0,
                            yaw,
                        },
                    ),
                });
            }
        }
    }
    if player.survival.is_dead_or_dying() {
        if !check_totem(entity, player, env, kind) {
            if took_full {
                player_sound(env, id, pos, "minecraft:entity.player.death");
            }
            die(entity, player, env, source, attackers.0);
        }
    } else if took_full {
        player_sound(env, id, pos, types.get(kind).effects.sound());
    }
    if !player.survival.effects.is_empty() && (!blocked || damage > 0.0) {
        // `onMobHurt` of the effects (Infested), unless the hit was blocked completely.
        let e = entity.as_ref();
        let look = crate::effect::living::view_vector(e.rot().x_rot, e.rot().y_rot);
        let height = e.dims().height;
        let s = &mut player.survival;
        crate::effect::living::on_hurt(
            &s.effects,
            &mut s.random,
            env.random,
            pos,
            height,
            look,
            env.out,
        );
    }
    // `ENTITY_HURT_PLAYER`, `DAMAGE_BLOCKED_BY_SHIELD` and the causing player's
    // `PLAYER_HURT_ENTITY` (resolved with the entities: `view::resolve_combat`).
    if source.cause.is_some()
        || player
            .advancements
            .listens(crate::advancements::trigger::TriggerKind::ENTITY_HURT_PLAYER)
    {
        player
            .advancements
            .combat
            .push(crate::advancements::view::PendingCombat::Hurt {
                source,
                damage,
                blocked,
            });
    }
    if let Some(v) = damage_stat(blocked_amount) {
        player.stats.award(custom::DAMAGE_BLOCKED_BY_SHIELD, v);
    }
    // `success = !blocked || damage > 0`: a fully blocked hit did not hurt.
    !blocked || damage > 0.0
}

/// `Player.playSound`: heard by everyone near except the player itself (its client plays it).
/// A player's voice pitch is constant (`Player.getVoicePitch`): no draws.
fn player_sound(
    env: &mut Env<'_>,
    except: vanillite_world::entity::EntityId,
    pos: Vec3,
    sound: &str,
) {
    let seed = crate::survival::sound_seed();
    play_sound(
        env.out,
        Some(except),
        pos,
        sound,
        SoundSource::Players,
        1.0,
        1.0,
        seed,
    );
}

/// `Player.actuallyHurt` without armour items (the formula runs with zero armour).
fn actually_hurt(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    source: DamageSource,
    damage: f32,
    armor_effectiveness: Option<&dyn Fn(f32) -> f32>,
    names: SourceNames,
) {
    let types = env.types;
    let kind = source.kind;
    if is_invulnerable_to(player, env, kind) || immune_to_damage(entity, player, env, &source) {
        return;
    }
    let mut damage = damage;
    if !types.is(kind, tag::BYPASSES_ARMOR) {
        // `hurtArmor`, then `getDamageAfterArmorAbsorb` with the equipment's armour.
        hurt_armor(entity, player, env, &source, damage, 36..40);
        let (armor, toughness) = armor_values(player);
        damage = combat::damage_after_absorb_with(damage, armor, toughness, armor_effectiveness);
    }
    // `getDamageAfterMagicAbsorb`: Resistance (with `DAMAGE_RESISTED`), then the protection
    // enchantments.
    if !types.is(kind, tag::BYPASSES_EFFECTS) {
        if !types.is(kind, tag::BYPASSES_RESISTANCE) {
            let before = damage;
            damage = player.survival.effects.resistance_damage(damage);
            if let Some(v) = damage_stat(before - damage) {
                player.stats.award(custom::DAMAGE_RESISTED, v);
            }
        }
        if damage <= 0.0 {
            damage = 0.0;
        }
        if !types.is(kind, tag::BYPASSES_ENCHANTMENTS) && damage > 0.0 {
            let protection = protection(entity, player, env, &source);
            if protection > 0.0 {
                damage = combat::damage_after_magic_absorb(damage, protection);
            }
        }
    }
    let s = &mut player.survival;
    let remaining = (damage - s.absorption).max(0.0);
    s.absorption -= damage - remaining;
    let absorbed = damage - remaining;
    if absorbed > 0.0
        && let Some(v) = damage_stat(absorbed)
    {
        player.stats.award(custom::DAMAGE_ABSORBED, v);
    }
    if remaining != 0.0 {
        let exhaustion = types.get(kind).exhaustion;
        cause_food_exhaustion(player, exhaustion);
        let s = &mut player.survival;
        let entry = CombatEntry {
            source,
            damage: remaining,
            fall_location: fall_location(s),
            // `(float)this.mob.fallDistance`.
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            fall_distance: s.fall_distance as f32,
            names,
        };
        let alive = s.health > 0.0;
        let change = s
            .combat
            .record_damage(entry, s.tick_count, alive, source.cause.is_some());
        combat_packets(env, entity.as_ref().id(), change);
        let health = s.health - remaining;
        set_health(entity, s, health);
        if let Some(v) = damage_stat(remaining) {
            player.stats.award(custom::DAMAGE_TAKEN, v);
        }
        crate::survival::game_event(
            env.out,
            crate::level::game_event::events().entity_damage,
            entity.as_ref().pos(),
            Some(entity.as_ref().id()),
        );
    }
}

/// The armour and armour toughness of a player's equipment (`getArmorValue`,
/// `ARMOR_TOUGHNESS`).
pub fn armor_values(player: &PlayerData) -> (f32, f32) {
    let mut armor = Vec::new();
    let mut toughness = Vec::new();
    for slot in crate::player::encode::EQUIPMENT_SLOTS {
        let stack = player.inventory.equipment(slot);
        crate::item::attribute_modifiers(stack, slot, "minecraft:armor", &mut armor);
        crate::item::attribute_modifiers(stack, slot, "minecraft:armor_toughness", &mut toughness);
    }
    #[allow(clippy::cast_possible_truncation)]
    (
        crate::item::attribute_value(0.0, &armor, 0.0, 30.0) as f32,
        crate::item::attribute_value(0.0, &toughness, 0.0, 20.0) as f32,
    )
}

/// `hurtArmor` → `doHurtEquipment`: each armour piece takes `hurtAndBreak(max(1, damage / 4))`
/// (Unbreaking through the `item_damage` effects, the break event of its slot).
fn hurt_armor(
    entity: &EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    source: &DamageSource,
    damage: f32,
    slots: std::ops::Range<usize>,
) {
    if damage <= 0.0 || player.game_mode == GameMode::Creative {
        return;
    }
    #[allow(clippy::cast_possible_truncation)]
    let amount = (damage / 4.0).max(1.0) as i32;
    for index in slots {
        let Some(stack) = player.inventory.get_mut(index) else {
            continue;
        };
        // `doHurtEquipment`: only `equippable` items with `damage_on_hurt` (not elytra) that
        // `canBeHurtBy` the source (`damage_resistant`, e.g. netherite against fire).
        if stack.is_empty()
            || !crate::item::is_damageable(stack)
            || !crate::item::equippable(stack).is_some_and(|e| e.damage_on_hurt)
            || !crate::item::can_be_hurt_by(stack, env.types, source.kind)
        {
            continue;
        }
        let n = crate::durability::change(stack, amount, false, env.random);
        if n != 0 {
            let d = crate::item::damage(stack) + n;
            let broke = crate::advancements::view::apply_damage(
                Some(&mut player.advancements),
                Some(&mut player.stats),
                stack,
                d,
            );
            let slot = match index {
                36 => EquipmentSlot::Feet,
                37 => EquipmentSlot::Legs,
                38 => EquipmentSlot::Chest,
                _ => EquipmentSlot::Head,
            };
            if broke {
                let e = entity.as_ref();
                env.out.push(Effect::Tracking {
                    entity: e.id(),
                    with_self: true,
                    packet: crate::durability::break_packet(e.network_id().0, slot),
                });
            }
        }
    }
}

/// `EnchantmentHelper.getDamageProtection` of a player's equipment.
fn protection(
    entity: &EntityMut<'_>,
    player: &PlayerData,
    env: &mut Env<'_>,
    source: &DamageSource,
) -> f32 {
    with_damage_context(
        entity,
        player,
        env,
        source,
        |enchants, equipment, cx, random| enchants.damage_protection(equipment, cx, random),
    )
    .unwrap_or(0.0)
}

/// `EnchantmentHelper.isImmuneToDamage` of a player's equipment (Frost Walker on magma).
fn immune_to_damage(
    entity: &EntityMut<'_>,
    player: &PlayerData,
    env: &mut Env<'_>,
    source: &DamageSource,
) -> bool {
    with_damage_context(
        entity,
        player,
        env,
        source,
        |enchants, equipment, cx, random| enchants.immune_to_damage(equipment, cx, random),
    )
    .unwrap_or(false)
}

/// Runs `f` with the enchantment context of `source` hurting the player and its non-empty
/// equipment; `None` without equipment.
fn with_damage_context<R>(
    entity: &EntityMut<'_>,
    player: &PlayerData,
    env: &mut Env<'_>,
    source: &DamageSource,
    f: impl FnOnce(
        &crate::enchant::Enchantments,
        &[(crate::item::EquipmentSlot, &crate::item::ItemStack)],
        &crate::enchant::EnchantContext<'_>,
        &mut dyn crate::enchant::FloatRandom,
    ) -> R,
) -> Option<R> {
    let data = crate::player::play::game_data()?;
    let equipment: Vec<(crate::item::EquipmentSlot, &crate::item::ItemStack)> =
        crate::player::encode::EQUIPMENT_SLOTS
            .iter()
            .map(|&s| (s, player.inventory.equipment(s)))
            .filter(|(_, s)| !s.is_empty())
            .collect();
    if equipment.is_empty() {
        return None;
    }
    let flags = entity.as_ref().flags();
    let this = crate::predicate::EntityFacts {
        kind: Some(entity.as_ref().kind()),
        on_fire: player.survival.fire_ticks > 0,
        baby: false,
        sneaking: flags.contains(EntityFlags::CROUCHING),
        sprinting: flags.contains(EntityFlags::SPRINTING),
        swimming: flags.contains(EntityFlags::SWIMMING),
        on_ground: flags.contains(EntityFlags::ON_GROUND),
        smelts_loot: false,
        sheep_color: None,
        sheared: false,
        villager_variant: None,
        ..Default::default()
    };
    let cx = crate::enchant::EnchantContext {
        types: env.types,
        tags: &*data,
        this: Some(this),
        attacker: None,
        direct_attacker: None,
        attacking_player: None,
        source: Some(crate::predicate::SourceFacts {
            kind: source.kind,
            is_direct: source.cause.is_some() && source.cause == source.direct,
            direct: None,
            cause: None,
        }),
        raining: false,
        thundering: false,
        location: None,
    };
    Some(f(&data.enchants, &equipment, &cx, env.random))
}

/// `applyItemBlocking`: the item a player blocks with (`blocks_attacks`, raised for its block
/// delay) takes what its damage reductions resolve for the angle of the hit, loses durability
/// and is put on cool-down by a weapon that disables blocking. Returns the blocked damage
/// and, while the item in use still has the component after the hit, its block sound.
fn apply_item_blocking(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    source: &DamageSource,
    damage: f32,
) -> (f32, Option<Option<crate::consumable::Sound>>) {
    use vanillite_protocol::packets::play::serverbound::Hand;
    if damage <= 0.0 {
        return (0.0, None);
    }
    // `getItemBlockingWith`.
    let Some(using) = player.survival.using_item else {
        return (0.0, None);
    };
    let index = match using.hand {
        Hand::MainHand => player.inventory.selected,
        Hand::OffHand => crate::inventory::OFFHAND,
    };
    let stack = player.inventory.get(index).clone();
    if stack.item != using.item || using.emptied {
        return (0.0, None);
    }
    let Some(blocks) = crate::blocking::blocks_attacks(&stack) else {
        return (0.0, None);
    };
    if using.ticks < blocks.block_delay_ticks()
        || blocks.bypassed_by(env.types, source.kind)
        || source.piercing_arrow
    {
        return (0.0, None);
    }
    let pos = entity.as_ref().pos();
    let angle = match source.position {
        Some(at) => {
            let head = f64::from(entity.as_ref().rot().y_head_rot) * std::f64::consts::PI / 180.0;
            let view = Vec3::new(-head.sin(), 0.0, head.cos());
            let to = Vec3::new(at.x - pos.x, 0.0, at.z - pos.z).normalize();
            to.dot(view).clamp(-1.0, 1.0).acos()
        }
        None => f64::from(std::f32::consts::PI),
    };
    let blocked = blocks.resolve_blocked_damage(env.types, source.kind, damage, angle);
    // `hurtBlockingItem`: the use statistic, then the durability (`hurtAndBreak`).
    player.stats.award(
        crate::stats::Stat::of(crate::stats::StatType::Used, stack.item),
        1,
    );
    let points = blocks.item_damage(blocked);
    if points > 0 {
        let creative = player.game_mode == GameMode::Creative;
        let points = crate::durability::change(&stack, points, creative, env.random);
        if points != 0
            && let Some(s) = player.inventory.get_mut(index)
        {
            let d = crate::item::damage(s) + points;
            let broke = crate::advancements::view::apply_damage(
                Some(&mut player.advancements),
                Some(&mut player.stats),
                s,
                d,
            );
            if broke {
                // `onEquippedItemBroken`: the shield's break event (sound and particles).
                let slot = if index == crate::inventory::OFFHAND {
                    EquipmentSlot::OffHand
                } else {
                    EquipmentSlot::MainHand
                };
                let e = entity.as_ref();
                env.out.push(Effect::Tracking {
                    entity: e.id(),
                    with_self: true,
                    packet: crate::durability::break_packet(e.network_id().0, slot),
                });
                if let Some(u) = player.survival.using_item.as_mut() {
                    // Vanilla keeps using the emptied stack (`onEquippedItemBroken` does not
                    // stop it).
                    u.emptied = true;
                }
            }
        }
    }
    let emptied = player.survival.using_item.is_some_and(|u| u.emptied);
    // `blockUsingItem`: only a living entity that hit directly (no projectile).
    let melee = source.direct.is_some() && source.direct == source.cause;
    if blocked > 0.0 && !env.types.is(source.kind, tag::IS_PROJECTILE) && melee {
        if source.own_blocked_by_item {
            player.survival.blocked_by_item = Some(blocked >= damage);
        } else if blocked < damage
            && let Some(at) = source.position
        {
            // `blockedByItem`: a hit that was not fully blocked pushes the defender back.
            let on_ground = entity.as_ref().flags().contains(EntityFlags::ON_GROUND);
            let resistance = knockback_resistance(player);
            let v = crate::mob::combat::knockback(
                entity.as_ref().velocity(),
                on_ground,
                0.5,
                pos.x - at.x,
                pos.z - at.z,
                resistance,
                &mut player.survival.random,
            );
            if let Some(v) = v {
                entity.set_velocity(v);
            }
        }
        // `Player.blockUsingItem`: weapons that disable blocking put the item on cool-down
        // (`BlocksAttacks.disable`).
        let ticks = blocks.disable_ticks(source.disable_shield_seconds);
        // An item that just broke has no component left (`getItemBlockingWith`).
        if ticks > 0 && !emptied {
            let sink = std::sync::Arc::clone(&player.connection.sink);
            player.survival.cooldowns.add(&stack, ticks, sink.as_ref());
            // `stopUsingItem`: the item (still in hand) vibrates.
            player.survival.using_item = None;
            player.survival.stabbed = Vec::new();
            if crate::consumable::use_vibrations(&stack) {
                crate::survival::game_event(
                    env.out,
                    crate::level::game_event::events().item_interact_finish,
                    pos,
                    Some(entity.as_ref().id()),
                );
            }
            entity.mark_data_dirty();
            if let Some(sound) = &blocks.disabled_sound {
                let seed = crate::survival::sound_seed();
                crate::survival::play_sound(
                    env.out,
                    None,
                    pos,
                    sound,
                    SoundSource::Players,
                    0.8,
                    0.8 + env.random.next_float() * 0.4,
                    seed,
                );
            }
        }
    }
    (blocked, (!emptied).then_some(blocks.block_sound))
}

/// `FallLocation.getCurrentFallLocation`.
fn fall_location(s: &Survival) -> Option<FallLocation> {
    match s.last_climbable {
        Some(location) => Some(location),
        None if s.fluids.in_water() => Some(FallLocation::Water),
        None => None,
    }
}

/// How long a player remembers the player that hurt it last (`lastHurtByPlayerMemoryTime`).
const LAST_HURT_BY_PLAYER_MEMORY: i32 = 100;

/// `LivingEntity.getKillCredit`: the player that hurt it last, else the last attacker, while
/// remembered (`baseTick` forgets them).
pub fn kill_credit(s: &Survival) -> Option<vanillite_world::entity::EntityId> {
    s.last_hurt_by_player
        .map(|(by, _)| by)
        .or(s.last_hurt_by_mob.map(|(by, _)| by))
}

/// `ServerPlayer.die`; `killer` is the UUID of the source's causing entity (the kill credit)
/// for the plugins' death event, when the caller resolved it.
/// `ServerPlayer.onLeaveCombat` and `onEnterCombat`: `player_combat_end` and
/// `player_combat_enter` to the player itself.
fn combat_packets(env: &mut Env<'_>, player: EntityId, change: combat::CombatChange) {
    use vanillite_protocol::packets::play::clientbound::{PlayerCombatEnd, PlayerCombatEnter};
    if let Some(duration) = change.left {
        env.out.push(Effect::To {
            player,
            packet: SharedPacket::of(&PlayerCombatEnd { duration }),
        });
    }
    if change.entered {
        env.out.push(Effect::To {
            player,
            packet: SharedPacket::of(&PlayerCombatEnter),
        });
    }
}

pub fn die(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    source: DamageSource,
    killer: Option<u128>,
) {
    let network_id = entity.as_ref().network_id().0;
    let uuid = entity.as_ref().uuid();
    let id = entity.as_ref().id();
    crate::survival::game_event(
        env.out,
        crate::level::game_event::events().entity_die,
        entity.as_ref().pos(),
        Some(id),
    );
    // `CatalystListener.handleGameEvent(ENTITY_DIE)`: the nearest sculk catalyst takes the
    // experience reward as charge (`skipDropExperience`: no orbs) and blooms.
    let consumed_before = player.survival.experience_consumed;
    let consumed = consumed_before
        || crate::block::sculk_catalyst::catalyst_block_near(env.blocks, entity.as_ref().pos());
    if consumed && !consumed_before {
        // `Player.getBaseExperienceReward`.
        let reward =
            if env.rules.bool(Rule::KEEP_INVENTORY) || player.game_mode == GameMode::Spectator {
                0
            } else {
                player.survival.xp.level.saturating_mul(7).min(100)
            };
        player
            .survival
            .world_actions
            .push(crate::mob::WorldAction::Level(
                crate::mob::boss::warden::LevelAction::SculkDeath {
                    pos: entity.as_ref().pos(),
                    experience: reward,
                    player: None,
                },
            ));
    }
    env.out.push(Effect::PearlsVanish(uuid));
    let sink = std::sync::Arc::clone(&player.connection.sink);
    let mut death = crate::hooks::Death {
        message: env.rules.bool(Rule::SHOW_DEATH_MESSAGES).then(|| {
            let name = (env.display_name)(&player.profile.name, uuid);
            // `getKillCredit`: the player that hurt it last, else the last attacker.
            let s = &player.survival;
            let credit = match (s.last_hurt_by_player, s.last_hurt_by_mob) {
                (Some(_), _) => s.last_hurt_by_names.1.clone(),
                (None, Some(_)) => s.last_hurt_by_names.0.clone(),
                (None, None) => None,
            };
            s.combat.death_message(env.types, name, credit)
        }),
        keep_inventory: env.rules.bool(Rule::KEEP_INVENTORY),
    };
    env.hooks
        .player_death(uuid, &env.types.get(source.kind).id, killer, &mut death);
    sink.send(&PlayerCombatKill {
        player_id: network_id,
        message: death.message.clone().unwrap_or_else(Component::empty),
    });
    let broadcast = death.message;
    if death.keep_inventory != env.rules.bool(Rule::KEEP_INVENTORY) {
        player.survival.keep_inventory_override = Some(death.keep_inventory);
    }
    super::shoulder::remove_entities(entity, player, env);
    // `dropAllDeathLoot` → `dropEquipment` and `dropExperience`.
    if !death.keep_inventory && player.game_mode != GameMode::Spectator {
        let pos = entity.as_ref().pos();
        // `getEyeY()` with the pose's float eye height.
        let eye = match entity.as_ref().pose() {
            Pose::Crouching => 1.27f32,
            Pose::Swimming | Pose::FallFlying | Pose::SpinAttack => 0.4,
            Pose::Sleeping | Pose::Dying => 0.2,
            _ => 1.62,
        };
        let eye_y = pos.y + f64::from(eye);
        let mut drops = take_drops(&mut player.inventory);
        // `getBaseExperienceReward`.
        let mut amount = player.survival.xp.level.saturating_mul(7).min(100);
        if env.hooks.wants(vanillite_abi::event::ENTITY_DROPS) {
            let mut ev = crate::hooks::Event {
                entity: uuid,
                entity_type: vanillite_data::entity::ids::PLAYER.name().to_owned(),
                key: env.types.get(source.kind).id.clone(),
                item: crate::hooks::items_snbt(&drops),
                value: i64::from(amount),
                to: crate::hooks::Place::at(pos),
                ..crate::hooks::Event::new(vanillite_abi::event::ENTITY_DROPS)
            };
            env.hooks.event(env.hook_world, &mut ev);
            if let Some(new) = crate::hooks::drops_out(&ev) {
                drops = new;
            }
            amount = crate::hooks::experience_value(ev.value);
        }
        throw_drops(
            drops,
            &mut player.survival.random,
            Vec3::new(pos.x, eye_y - f64::from(0.3_f32), pos.z),
            env,
        );
        if amount > 0 && !consumed {
            env.out.push(Effect::Experience { pos, amount });
        }
    }
    // `getKillCredit`: `ENTITY_KILLED_BY`, `awardKillScore` (`entity_killed_player`).
    let credit = kill_credit(&player.survival).filter(|&k| k != id);
    if let Some(killer) = credit {
        player
            .advancements
            .combat
            .push(crate::advancements::view::PendingCombat::Died { source, killer });
    }
    env.out.push(Effect::Died {
        victim: id,
        killer: credit,
        killer_kind: None,
        message: broadcast,
    });
    env.out.push(Effect::Tracking {
        entity: id,
        with_self: true,
        packet: SharedPacket::of(&EntityEvent {
            entity_id: network_id,
            event: entity_event::DEATH,
        }),
    });
    // `ServerPlayer.die` does not call `LivingEntity.die`: the pose (and the box) stay.
    let s = &mut player.survival;
    // `die`: a sleeper leaves its bed.
    if s.sleeping_pos.is_some() {
        s.wake = true;
    }
    s.fire_ticks = s.fire_ticks.min(0);
    s.ticks_frozen = 0;
    entity.set_flags(EntityFlags::ON_FIRE, false);
    let alive = s.health > 0.0;
    let left = s.combat.recheck_status(s.tick_count, alive);
    combat_packets(
        env,
        id,
        combat::CombatChange {
            left,
            entered: false,
        },
    );
    let s = &mut player.survival;
    s.last_death = Some((env.dimension.to_owned(), block_pos(entity.as_ref().pos())));
    player.stats.award(custom::DEATHS, 1);
    player.stats.reset(custom::TIME_SINCE_DEATH);
    player.stats.reset(custom::TIME_SINCE_REST);
    player.connection.movement.mark_client_unloaded();
}

/// `destroyVanishingCursedItems` + `Inventory.dropAll`: the stacks that drop, taken out of the
/// inventory, in drop order.
fn take_drops(inventory: &mut crate::inventory::PlayerInventory) -> Vec<crate::item::ItemStack> {
    let vanishing = |stack: &crate::item::ItemStack| {
        crate::player::play::game_data()
            .is_some_and(|d| d.enchantment_level(stack, "minecraft:vanishing_curse") > 0)
    };
    let inv = inventory;
    // The main inventory, then the equipment in `EquipmentSlot` order.
    let slots = inv
        .items
        .iter_mut()
        .chain(std::iter::once(&mut inv.offhand))
        .chain(inv.armor.iter_mut());
    slots
        .map(|slot| std::mem::replace(slot, crate::item::ItemStack::EMPTY))
        .filter(|stack| !stack.is_empty() && !vanishing(stack))
        .collect()
}

/// `Inventory.dropAll`'s throws: every stack in a random direction from `from`
/// (`createItemStackToDrop(stack, true, false)`).
fn throw_drops(
    drops: Vec<crate::item::ItemStack>,
    random: &mut LegacyRandom,
    from: Vec3,
    env: &mut Env<'_>,
) {
    for stack in drops {
        // `new ItemEntity(level, x, y, z, stack)`; the throw replaces its motion and draws
        // from the player's own random.
        let (uuid, y_rot, _, item_random) = crate::mob::new_item_entity();
        let pow = random.next_float() * 0.5;
        let dir = random.next_float() * std::f32::consts::TAU;
        let velocity = Vec3::new(
            f64::from(-vanillite_base::math::sin(f64::from(dir)) * pow),
            f64::from(0.2f32),
            f64::from(vanillite_base::math::cos(f64::from(dir)) * pow),
        );
        env.out.push(Effect::DropItem {
            random: item_random,
            uuid,
            pos: from,
            velocity,
            y_rot,
            pickup_delay: 40,
            stack,
        });
    }
}

/// `/kill`: `hurtServer(genericKill, Float.MAX_VALUE)`.
pub fn kill(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) -> bool {
    let source = DamageSource::of(env.types.generic_kill);
    hurt(entity, player, env, source, f32::MAX)
}

/// What the world must do with a player after its tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickResult {
    /// Nothing.
    Alive,
    /// The death animation is over: remove the entity (`remove(KILLED)`); the player waits
    /// for its respawn.
    Remove,
    /// The player goes through a portal now (`handlePortal`): the portal and the block it
    /// entered.
    Portal(crate::portal::PortalKind, BlockPos),
    /// The player entered the End's exit portal for the first time: the credits roll
    /// (`showEndCredits`), the entity leaves the world and the player respawns when they end.
    Credits,
}

/// `LivingEntity.swing`'s `swingState.startIfAble` with `getModifiedSwingDuration`: refused
/// while a swing is in its first half; returns whether the swing starts (and is sent).
pub fn start_swing(s: &mut Survival, duration: i32) -> bool {
    if let Some((ticks, current)) = s.swing
        && ticks > 0
        && ticks <= current / 2
    {
        return false;
    }
    let duration = if s.effects.has_dig_speed() {
        duration - (1 + s.effects.dig_speed_amplification())
    } else if let Some(fatigue) = s
        .effects
        .amplifier(crate::effect::MobEffect::MINING_FATIGUE)
    {
        duration + (1 + fatigue) * 2
    } else {
        duration
    };
    s.swing = Some((0, duration));
    true
}

/// `ServerPlayer.tick`'s survival part (once per world tick, with the entities).
pub fn tick_entity(player: &mut PlayerData) {
    let s = &mut player.survival;
    if s.damage_cooldown > 0 {
        s.damage_cooldown -= 1;
    }
    s.attack_ticker = s.attack_ticker.saturating_add(1);
    // `SwingState.tick`.
    if let Some((ticks, duration)) = s.swing {
        s.swing = (ticks <= duration).then_some((ticks.saturating_add(1), duration));
    }
    if s.take_xp_delay > 0 {
        s.take_xp_delay -= 1;
    }
    // `resetAttackStrengthTicker` when another item comes into the main hand.
    let held = player.inventory.selected_item().item;
    let s = &mut player.survival;
    if held != s.last_main_hand_item {
        s.attack_ticker = 0;
        s.last_main_hand_item = held;
    }
}

/// `ServerPlayer.doTick`: `Player.tick` (base tick: fluids, fire, void, suffocation, air,
/// death, effects; `aiStep`: regeneration and block effects), the food tick and the health
/// and experience packets. Without `updatingUsingItem`, which needs the other entities: the
/// worlds run [`do_tick_base`], [`crate::player::using::tick`], [`do_tick_ai`] and
/// [`do_tick_end`] themselves.
pub fn do_tick(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
) -> TickResult {
    match do_tick_base(entity, player, env) {
        std::ops::ControlFlow::Break(result) => result,
        std::ops::ControlFlow::Continue(result) => {
            let result = do_tick_ai(entity, player, env, result);
            do_tick_end(entity, player, env, result)
        }
    }
}

/// [`do_tick`] up to the end of the base tick (`LivingEntity.baseTick` with `tickEffects`):
/// `Break` when the tick ends early. `updatingUsingItem` follows
/// ([`crate::player::using::tick`]), then [`do_tick_ai`].
pub fn do_tick_base(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
) -> std::ops::ControlFlow<TickResult, TickResult> {
    use std::ops::ControlFlow::{Break, Continue};
    let abilities = Abilities::of(player.game_mode);
    if player.survival.portal.won_game {
        // Removed from the world while the credits roll.
        return Break(TickResult::Alive);
    }
    {
        let s = &mut player.survival;
        s.tick_count = s.tick_count.wrapping_add(1);
        // `ServerPlayer.tick`: `wardenSpawnTracker.tick()`.
        s.warden_spawn_tracker.tick();
    }
    // `Entity.baseTick`: `handlePortal` first (`canUsePortal(false)`: alive and riding
    // nothing; a vehicle takes its riders along).
    let can_use_portal = !player.survival.is_dead_or_dying()
        && !entity.as_ref().flags().contains(EntityFlags::PASSENGER);
    let portal_delay = |kind| match kind {
        crate::portal::PortalKind::Nether => env
            .rules
            .get(if abilities.invulnerable {
                Rule::PLAYERS_NETHER_PORTAL_CREATIVE_DELAY
            } else {
                Rule::PLAYERS_NETHER_PORTAL_DEFAULT_DELAY
            })
            .max(0),
        _ => 0,
    };
    if let Some((kind, entry)) = player.survival.portal.handle(can_use_portal, portal_delay) {
        return Break(TickResult::Portal(kind, entry));
    }
    Continue(base_tick_after_portal(entity, player, env))
}

/// The base tick of a player whose portal took it to another world (`TickResult::Portal`),
/// run where it arrived: Vanilla teleports inside `handlePortal` and the tick goes on in the
/// new world ([`crate::player::using::tick`], [`do_tick_ai`] and [`do_tick_end`] follow).
pub fn base_tick_after_portal(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
) -> TickResult {
    let spectator = player.game_mode == GameMode::Spectator;
    let abilities = Abilities::of(player.game_mode);
    let mut result = TickResult::Alive;
    let id = entity.as_ref().id();
    player.survival.in_powder_snow = false;
    update_fluids(entity, player, env);
    update_swimming(entity, player);
    {
        let s = &mut player.survival;
        if s.fire_ticks > 0 {
            let in_lava = !s.first_tick && s.fluids.in_lava();
            if s.fire_ticks % 20 == 0 && !in_lava {
                let source = DamageSource::of(env.types.on_fire);
                hurt(entity, player, env, source, 1.0);
            }
            player.survival.fire_ticks -= 1;
        }
    }
    {
        let s = &mut player.survival;
        if !s.first_tick && s.fluids.in_lava() {
            s.fall_distance *= 0.5;
        }
    }
    if entity.as_ref().pos().y < f64::from(env.min_y.saturating_sub(64)) {
        // `LivingEntity.onBelowWorld`.
        let source = DamageSource::of(env.types.out_of_world);
        hurt(entity, player, env, source, 4.0);
    }
    let on_fire = player.survival.fire_ticks > 0;
    entity.set_flags(EntityFlags::ON_FIRE, on_fire);
    player.survival.first_tick = false;
    // `LivingEntity.baseTick`.
    let alive = !player.survival.is_dead_or_dying();
    if alive {
        let pos = entity.as_ref().pos();
        let dims = entity.as_ref().dims();
        let eye = Vec3::new(pos.x, pos.y + eye_height(dims.height), pos.z);
        if !spectator && blocks::is_in_wall(env.blocks, env.facts, eye, dims.width) {
            let source = DamageSource::of(env.types.in_wall);
            hurt(entity, player, env, source, 1.0);
        } else if let Some(amount) = env
            .border
            .and_then(|b| b.damage(pos.x, pos.z, f64::from(dims.width) / 2.0))
        {
            let source = DamageSource::of(env.types.outside_border);
            hurt(entity, player, env, source, amount);
        }
        let eye_in_water = player.survival.fluids.eye_in_water
            && !env
                .blocks
                .block_state(block_pos(eye))
                .is_some_and(|s| env.facts.is_bubble_column(s));
        if eye_in_water {
            if !abilities.invulnerable && !player.survival.effects.has_water_breathing() {
                let air = decrease_air(player, player.survival.air);
                set_air(entity, &mut player.survival, air);
                if player.survival.air <= -20 {
                    set_air(entity, &mut player.survival, 0);
                    env.out.push(Effect::Tracking {
                        entity: id,
                        with_self: true,
                        packet: SharedPacket::of(&EntityEvent {
                            entity_id: entity.as_ref().network_id().0,
                            event: entity_event::DROWN_PARTICLES,
                        }),
                    });
                    let source = DamageSource::of(env.types.drown);
                    hurt(entity, player, env, source, 2.0);
                }
            } else if player.survival.air < MAX_AIR && player.survival.effects.refills_air() {
                let air = (player.survival.air + 4).min(MAX_AIR);
                set_air(entity, &mut player.survival, air);
            }
        } else if player.survival.air < MAX_AIR {
            let air = (player.survival.air + 4).min(MAX_AIR);
            set_air(entity, &mut player.survival, air);
        }
        // `EnchantmentHelper.tickEffects` and `onChangedBlock` (Frost Walker, Soul Speed).
        if crate::enchant::location::base_tick(entity, player, env) {
            player.survival.equipment_dirty = true;
        }
    }
    {
        let s = &mut player.survival;
        if s.hurt_time > 0 {
            s.hurt_time -= 1;
        }
        if s.arrows.tick() {
            entity.mark_data_dirty();
        }
    }
    if player.survival.is_dead_or_dying() {
        // `tickDeath`.
        let s = &mut player.survival;
        s.death_time += 1;
        if s.death_time >= 20 && !s.removed {
            s.removed = true;
            // `remove(KILLED)` → `triggerOnDeathMobEffects`.
            if !s.effects.is_empty() {
                let cramming = env
                    .rules
                    .get(crate::survival::rules::Rule::MAX_ENTITY_CRAMMING);
                s.world_actions
                    .push(crate::mob::WorldAction::PlayerDeathEffects(id, cramming));
            }
            env.out.push(Effect::Tracking {
                entity: id,
                with_self: true,
                packet: SharedPacket::of(&EntityEvent {
                    entity_id: entity.as_ref().network_id().0,
                    event: entity_event::POOF,
                }),
            });
            result = TickResult::Remove;
        }
    }
    {
        // `baseTick`: `lastHurtByPlayer` and `lastHurtByMob` are forgotten after 100 ticks.
        let s = &mut player.survival;
        s.last_hurt_by_player = match s.last_hurt_by_player {
            Some((by, time)) if time > 0 => Some((by, time - 1)),
            _ => None,
        };
        if s.last_hurt_by_mob
            .is_some_and(|(_, at)| s.tick_count.wrapping_sub(at) > 100)
        {
            s.last_hurt_by_mob = None;
        }
        // `LivingEntity.tick`: every 20 ticks.
        if s.tick_count.wrapping_rem(20) == 0 {
            let alive = s.health > 0.0;
            let left = s.combat.recheck_status(s.tick_count, alive);
            combat_packets(
                env,
                id,
                combat::CombatChange {
                    left,
                    entered: false,
                },
            );
        }
    }
    // `tickEffects`, with the effect packets, flags and attributes.
    crate::effect::living::tick_player(entity, player, env);
    result
}

/// [`do_tick`] from `Player.aiStep` to its end (without its `pushEntities` and `touch`, which
/// need the other entities), after [`do_tick_base`] and the item in use.
pub fn do_tick_ai(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    result: TickResult,
) -> TickResult {
    let spectator = player.game_mode == GameMode::Spectator;
    let mut result = result;
    {
        // `aiStep`: Slow Falling and Levitation reset the fall distance.
        let s = &mut player.survival;
        if s.effects.has(crate::effect::MobEffect::SLOW_FALLING)
            || s.effects.has(crate::effect::MobEffect::LEVITATION)
        {
            reset_fall_distance(player);
        }
    }
    // `Player.aiStep`: peaceful regeneration (`ServerPlayer.tickRegeneration`).
    if env.difficulty == Difficulty::Peaceful && env.rules.bool(Rule::NATURAL_HEALTH_REGENERATION) {
        if player.survival.tick_count % 20 == 0 {
            if player.survival.is_hurt() {
                heal(entity, player, 1.0);
            }
            let food = &mut player.survival.food;
            if food.saturation < 20.0 {
                food.saturation += 1.0;
            }
        }
        let s = &mut player.survival;
        if s.food.needs_food() && s.tick_count % 10 == 0 {
            s.food.food_level += 1;
        }
    }
    if entity.as_ref().flags().contains(EntityFlags::FALL_FLYING) {
        crate::elytra::update_fall_flying(entity, player, env);
    }
    let before_travel = entity.as_ref().aabb();
    player.survival.server_step = Vec3::ZERO;
    if result == TickResult::Alive {
        if player.survival.simulate_movement {
            if entity.as_ref().flags().contains(EntityFlags::PASSENGER) {
                // `rideTick`: no motion of its own; the vehicle positions it
                // (`positionRider`) and `LivingEntity.rideTick` resets the fall distance.
                entity.set_velocity(Vec3::ZERO);
                player.survival.fall_distance = 0.0;
            } else {
                simulate_travel(entity, player, env);
            }
        } else {
            player.survival.server_step = server_step(entity, env);
            travel_motion(entity, player, env);
        }
    }
    // `autoSpinAttackTicks` count down after the travel (the hits are the world's:
    // [`crate::trident::spin_attack_hits`]).
    if let Some(spin) = player.survival.spin_attack.as_mut() {
        spin.ticks -= 1;
        spin.swept = crate::trident::minmax(before_travel, entity.as_ref().aabb());
        spin.horizontal_collision = player.survival.horizontal_collision;
    }
    if result == TickResult::Alive && !player.survival.is_dead_or_dying() {
        if !spectator {
            apply_block_effects(entity, player, env);
        }
        tick_freezing(entity, player, env);
        if enter_portals(entity, player, env) {
            result = TickResult::Credits;
        }
    }
    result
}

/// [`do_tick`] after `Player.aiStep`: the body turn, the food tick and the health packets.
pub fn do_tick_end(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    result: TickResult,
) -> TickResult {
    if player.survival.simulate_movement {
        tick_head_turn(entity, &mut player.survival);
    }
    crate::elytra::count_fall_fly_ticks(entity, player);
    if player.survival.impulse_grace > 0 {
        player.survival.impulse_grace -= 1;
    }
    crate::elytra::update_player_pose(
        entity,
        player.survival.spin_attack.is_some(),
        crate::effect::attribute_map::scale_of(&player.survival.effects.modifiers),
    );
    player.survival.last_box = Some(entity.as_ref().aabb());
    // `Player.aiStep`: `handleShoulderEntities`.
    let sleeping = player.survival.sleeping_pos.is_some();
    super::shoulder::handle(entity, player, env, sleeping);
    // `FoodData.tick`.
    let s = &mut player.survival;
    let action = s.food.tick(
        env.difficulty,
        env.rules.bool(Rule::NATURAL_HEALTH_REGENERATION),
        s.is_hurt(),
        s.health,
    );
    match action {
        FoodAction::None => {}
        FoodAction::Heal(amount) => heal(entity, player, amount),
        FoodAction::Starve => {
            let source = DamageSource::of(env.types.starve);
            hurt(entity, player, env, source, 1.0);
        }
    }
    // `ServerPlayer.doTick`'s time statistics.
    let stats = &mut player.stats;
    stats.award(custom::PLAY_TIME, 1);
    stats.award(custom::TOTAL_WORLD_TIME, 1);
    if player.survival.health > 0.0 {
        stats.award(custom::TIME_SINCE_DEATH, 1);
    }
    // `isDiscrete`: the sneak key is held.
    if entity.as_ref().flags().contains(EntityFlags::CROUCHING) {
        stats.award(custom::CROUCH_TIME, 1);
    }
    if player.survival.sleeping_pos.is_none() {
        stats.award(custom::TIME_SINCE_REST, 1);
    }
    let sink = std::sync::Arc::clone(&player.connection.sink);
    // `Player.tick`: the turtle helmet outside water, then the item cool-downs.
    if !player.survival.fluids.eye_in_water {
        let head = player.inventory.equipment(crate::item::EquipmentSlot::Head);
        if crate::item::item_id(head) == vanillite_data::item::ids::TURTLE_HELMET {
            let mut host = crate::effect::living::PlayerHost {
                entity,
                player,
                env,
                players: None,
                names: &[],
            };
            crate::effect::active::add(
                &mut host,
                crate::effect::EffectInstance::with_flags(
                    crate::effect::MobEffect::WATER_BREATHING,
                    200,
                    0,
                    false,
                    false,
                    true,
                ),
            );
            crate::effect::living::sync_player(entity, player, env);
        }
    }
    player.survival.cooldowns.tick(sink.as_ref());
    player.survival.sync(sink.as_ref());
    result
}

/// `updateFluidInteraction`: the fluids around the player; touching water resets the fall
/// distance, and entering it splashes.
fn update_fluids(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let e = entity.as_ref();
    let pos = e.pos();
    let eye = Vec3::new(pos.x, pos.y + eye_height(e.dims().height), pos.z);
    let contact = blocks::fluid_contact(env.blocks, env.facts, e.aabb(), eye);
    let was_in_water = player.survival.fluids.in_water();
    player.survival.fluids = contact;
    if contact.in_water() {
        reset_fall_distance(player);
        if !was_in_water && !player.survival.first_tick && player.game_mode != GameMode::Spectator {
            water_splash(entity, env);
        }
        // `isPushedByFluid` (not flying): `applyCurrentTo(WATER, 0.014)`.
        if !player.survival.flying && player.game_mode != GameMode::Spectator {
            push_by_fluid(entity, env, true, 0.014);
        }
    }
    // `applyCurrentTo(LAVA, ...)`: three times as hard where lava is fast (the Nether).
    if contact.in_lava() && !player.survival.flying && player.game_mode != GameMode::Spectator {
        let scale = if env.dimension == "minecraft:the_nether" {
            0.007
        } else {
            0.002_333_333_333_333_333_5
        };
        push_by_fluid(entity, env, false, scale);
    }
}

/// `EntityFluidInteraction.update` and `applyCurrentTo(fluid, entity, scale)` for a player:
/// the flows of the water (or lava) the box touches (scaled by the height of that fluid so far
/// below 0.4), averaged (other entities take the direction instead).
fn push_by_fluid(entity: &mut EntityMut<'_>, env: &Env<'_>, water: bool, scale: f64) {
    let e = entity.as_ref();
    let aabb = e.aabb();
    let b = aabb.inflate(-0.001, -0.001, -0.001);
    let ceil = vanillite_base::math::ceil;
    let floor = vanillite_base::math::floor;
    let flows = MoveBlocks(env.blocks, env.border);
    let (mut sum, mut count, mut height) = (Vec3::ZERO, 0_u32, 0.0_f64);
    for x in floor(b.min_x)..ceil(b.max_x) {
        for y in floor(b.min_y)..ceil(b.max_y) {
            for z in floor(b.min_z)..ceil(b.max_z) {
                let pos = BlockPos::new(x, y, z);
                let Some(state) = env.blocks.block_state(pos) else {
                    return;
                };
                let fluid = state.fluid_state();
                let same = if water {
                    env.facts.is_water(fluid)
                } else {
                    env.facts.is_lava(fluid)
                };
                if !same {
                    continue;
                }
                let top = f64::from(y)
                    + f64::from(blocks::fluid_height(env.blocks, env.facts, pos, fluid));
                if top < b.min_y {
                    continue;
                }
                height = height.max(top - aabb.min_y);
                let mut flow = crate::mob::living::fluid_flow(&flows, env.facts, pos, fluid, water);
                if height < 0.4 {
                    flow = flow * height;
                }
                sum = sum + flow;
                count += 1;
            }
        }
    }
    if count == 0 || sum.length_sqr() < f64::from(1.0e-5_f32) {
        return;
    }
    let old = e.velocity();
    let mut impulse = sum * (1.0 / f64::from(count)) * scale;
    if old.x.abs() < 0.003 && old.z.abs() < 0.003 && impulse.length() < 0.004_500_000_000_000_000_5
    {
        impulse = impulse.normalize() * 0.004_500_000_000_000_000_5;
    }
    entity.set_velocity(old + impulse);
}

/// `Player.doWaterSplashEffect`: the splash sound and the `splash` game event (the bubble and
/// splash particles are the clients'; the player's own random is not modelled, so the pitch
/// and the particle draws do not touch the level random).
fn water_splash(entity: &EntityMut<'_>, env: &mut Env<'_>) {
    let e = entity.as_ref();
    let v = e.velocity();
    let point_two = f64::from(0.2f32);
    #[allow(clippy::cast_possible_truncation)]
    let speed =
        ((v.x * v.x * point_two + v.y * v.y + v.z * v.z * point_two).sqrt() as f32 * 0.2).min(1.0);
    let sound = if speed < 0.25 {
        "minecraft:entity.player.splash"
    } else {
        "minecraft:entity.player.splash.high_speed"
    };
    let seed = crate::survival::sound_seed();
    let mut r = LegacyRandom::new(seed);
    let pitch = 1.0 + (r.next_float() - r.next_float()) * 0.4;
    let id = e.id();
    crate::survival::play_sound(
        env.out,
        Some(id),
        e.pos(),
        sound,
        SoundSource::Players,
        speed,
        pitch,
        r.next_long(),
    );
    crate::survival::game_event(
        env.out,
        crate::level::game_event::events().splash,
        e.pos(),
        Some(id),
    );
}

/// `updateSwimming`: sprinting under water swims.
fn update_swimming(entity: &mut EntityMut<'_>, player: &PlayerData) {
    let flags = entity.as_ref().flags();
    let fluids = player.survival.fluids;
    let sprinting = flags.contains(EntityFlags::SPRINTING);
    let swimming = if flags.contains(EntityFlags::SWIMMING) {
        sprinting && fluids.in_water()
    } else {
        sprinting && fluids.eye_in_water && fluids.in_water()
    };
    let swimming = swimming && player.game_mode != GameMode::Spectator;
    if swimming != flags.contains(EntityFlags::SWIMMING) {
        entity.set_flags(EntityFlags::SWIMMING, swimming);
    }
}

/// The portal blocks the player touched on its way this tick (`entityInside` of nether and
/// End portals and gateways). Returns whether the end credits start.
fn enter_portals(entity: &EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) -> bool {
    use crate::portal::PortalKind;
    let now = entity.as_ref().aabb();
    let swept = match player.survival.last_box {
        Some(last) => vanillite_base::Aabb {
            min_x: now.min_x.min(last.min_x),
            min_y: now.min_y.min(last.min_y),
            min_z: now.min_z.min(last.min_z),
            max_x: now.max_x.max(last.max_x),
            max_y: now.max_y.max(last.max_y),
            max_z: now.max_z.max(last.max_z),
        },
        None => now,
    };
    // `canUsePortal(false)` of the portal blocks' `entityInside`: riders stay out.
    if entity.as_ref().flags().contains(EntityFlags::PASSENGER) {
        return false;
    }
    let portals = blocks::inside_portals(env.blocks, swept);
    let portal = &mut player.survival.portal;
    if let Some(pos) = portals.nether {
        portal.set_inside(PortalKind::Nether, pos);
    }
    if let Some(pos) = portals.end {
        if env.dimension == "minecraft:the_end" && !portal.seen_credits {
            // `showEndCredits`.
            portal.seen_credits = true;
            if !portal.won_game {
                portal.won_game = true;
                player.connection.sink.send(
                    &vanillite_protocol::packets::play::clientbound::GameEvent {
                        event: vanillite_protocol::packets::play::clientbound::GameEvent::WIN_GAME,
                        param: 0.0,
                    },
                );
                return true;
            }
        } else {
            portal.set_inside(PortalKind::End, pos);
        }
    }
    if let Some(pos) = portals.gateway
        && (env.enter_gateway)(pos)
    {
        portal.set_inside(PortalKind::Gateway, pos);
    }
    false
}

/// `HoneyBlock.entityInside` of a player sliding down the side of a honey block:
/// `HONEY_BLOCK_SLIDE` every 20 game ticks, the slide motion, the reset fall distance and the
/// slide sound and particles.
fn honey_slide(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    block: BlockPos,
    state: vanillite_data::block::BlockState,
) {
    let old_dy = |dy: f64| dy / f64::from(0.98_f32) + 0.08;
    let new_dy = |dy: f64| (dy - 0.08) * f64::from(0.98_f32);
    let e = entity.as_ref();
    let (p, v, id) = (e.pos(), e.velocity(), e.id());
    // `isSlidingDown`.
    if e.flags().contains(EntityFlags::ON_GROUND)
        || p.y > f64::from(block.y) + 0.9375 - 1.0e-7
        || old_dy(v.y) >= -0.08
    {
        return;
    }
    let dx = (f64::from(block.x) + 0.5 - p.x).abs();
    let dz = (f64::from(block.z) + 0.5 - p.z).abs();
    let overlap = 0.4375 + f64::from(e.dims().width / 2.0);
    if !(dx + 1.0e-7 > overlap || dz + 1.0e-7 > overlap) {
        return;
    }
    // `maybeDoSlideAchievement`.
    if env.game_time % 20 == 0 {
        player
            .advancements
            .trigger(crate::advancements::trigger::Event::SlideDownBlock(state));
    }
    // `doSlideMovement`.
    let old = old_dy(v.y);
    let velocity = if old < -0.13 {
        let factor = -0.05 / old;
        Vec3::new(v.x * factor, new_dy(-0.05), v.z * factor)
    } else {
        Vec3::new(v.x, new_dy(-0.05), v.z)
    };
    entity.set_velocity(velocity);
    crate::advancements::tracking::reset_fall_distance(player);
    // `maybeDoSlideEffects` (the level random).
    if env.random.next_int_bounded(5) == 0 {
        player_sound(env, id, p, "minecraft:block.honey_block.slide");
    }
    if env.random.next_int_bounded(5) == 0 {
        env.out.push(Effect::Tracking {
            entity: id,
            with_self: true,
            packet: SharedPacket::of(&EntityEvent {
                entity_id: entity.as_ref().network_id().0,
                event: 53,
            }),
        });
    }
}

/// `Entity.applyEffectsFromBlocks` at the player's position: the floor it stands on (magma),
/// fire and lava inside its box, water and rain putting the fire out.
fn apply_block_effects(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let e = entity.as_ref();
    let pos = e.pos();
    let id = e.id();
    let flags = e.flags();
    let aabb = e.aabb();
    let dims = e.dims();
    if flags.contains(EntityFlags::ON_GROUND) {
        let on = BlockPos::new(
            vanillite_base::math::floor(pos.x),
            vanillite_base::math::floor(pos.y - 0.2),
            vanillite_base::math::floor(pos.z),
        );
        let on_state = env.blocks.block_state(on);
        // `MagmaBlock.stepOn`: careful (sneaking) players are safe.
        if on_state.is_some_and(|s| env.facts.is_hot_floor(s))
            && !flags.contains(EntityFlags::CROUCHING)
        {
            let source = DamageSource::of(env.types.hot_floor);
            hurt(entity, player, env, source, 1.0);
        } else if on_state.is_some_and(|s| crate::block::contact::has_step_on(s.block()))
            && !flags.contains(EntityFlags::CROUCHING)
        {
            // `RedStoneOreBlock.stepOn`, sculk sensors and shriekers.
            player
                .survival
                .world_actions
                .push(crate::mob::WorldAction::StepOn(on, id));
        }
    }
    let was_on_fire = player.survival.fire_ticks > 0;
    let was_freezing = player.survival.ticks_frozen > 0;
    let previous = player.survival.fire_ticks;
    // The moves of the player's packets and of the server's own simulation, then the way to
    // where the player is now (`applyEffectsFromBlocks`).
    let mut path = std::mem::take(&mut player.connection.movement.path);
    crate::survival::inside::finish(&mut path, pos, pos);
    let mut honey = Vec::new();
    let mut bubbles = Vec::new();
    // `Player.onInsideBlock`: `enter_block` for every block the box is inside on its way.
    let enter_block = crate::advancements::trigger::TriggerKind::ENTER_BLOCK;
    let listens = player.advancements.listens(enter_block);
    let mut entered = Vec::new();
    let inside = blocks::inside_blocks(
        env.blocks,
        env.facts,
        dims,
        player.survival.fall_distance > 2.5,
        &path,
        |cell| {
            if listens && cell.in_block {
                entered.push(cell.state);
            }
            if cell.state.block() == vanillite_data::block::ids::HONEY_BLOCK {
                honey.push((cell.pos, cell.state));
            } else if cell.state.block() == vanillite_data::block::ids::BUBBLE_COLUMN
                && cell.precise
            {
                bubbles.push(cell.pos);
            }
        },
    );
    // `BubbleColumnBlock.entityInside` of a player that does not fly: the particles at the
    // top of the column (the drag is the client's), the fall distance reset inside it.
    if !player.survival.flying {
        for column in bubbles {
            let above = env
                .blocks
                .block_state(BlockPos::new(column.x, column.y + 1, column.z))
                .unwrap_or(vanillite_data::block::BlockState::AIR);
            if above.collision_shape().is_empty() && above.fluid_state().is_empty() {
                player
                    .survival
                    .world_actions
                    .push(crate::mob::WorldAction::BubbleColumnParticles(column));
            } else {
                crate::advancements::tracking::reset_fall_distance(player);
            }
        }
    }
    for state in entered {
        player
            .advancements
            .trigger(crate::advancements::trigger::Event::EnterBlock(state));
    }
    path.clear();
    player.connection.movement.path = path;
    for (block, state) in honey {
        honey_slide(entity, player, env, block, state);
    }
    // `makeStuckInBlock` of cobwebs, sweet berry bushes and powder snow the feet are in:
    // the fall so far does not count (`Player`: unless flying); the slowdown is the client's.
    let feet_in_snow = inside.powder_snow.is_some()
        && env
            .blocks
            .block_state(BlockPos::containing(pos.x, pos.y, pos.z))
            .is_some_and(|s| s.block() == vanillite_data::block::ids::POWDER_SNOW);
    if (inside.cobweb || inside.berry_bush.is_some() || feet_in_snow) && !player.survival.flying {
        crate::advancements::tracking::reset_fall_distance(player);
    }
    // The `entityInside` effects that act at once.
    if inside.cactus {
        let source = DamageSource::of(env.types.cactus);
        hurt(entity, player, env, source, 1.0);
    }
    if inside.berry_bush == Some(true) {
        // `SweetBerryBushBlock.entityInside`: the client reports the player's movement
        // (`getKnownMovement`).
        let moved = player.connection.movement.known_movement;
        if moved.x * moved.x + moved.z * moved.z > 0.0
            && (moved.x.abs() >= f64::from(0.003_f32) || moved.z.abs() >= f64::from(0.003_f32))
        {
            let source = DamageSource::of(env.types.sweet_berry_bush);
            hurt(entity, player, env, source, 1.0);
        }
    }
    if let Some(damage) = inside.campfire {
        let source = DamageSource::of(env.types.campfire);
        hurt(entity, player, env, source, damage);
    }
    if inside.wither_rose
        && env.difficulty != Difficulty::Peaceful
        && !Abilities::of(player.game_mode).invulnerable
        && !is_invulnerable_to(player, env, env.types.wither)
    {
        // `WitherRoseBlock.entityInside`.
        let mut host = crate::effect::living::PlayerHost {
            entity,
            player,
            env,
            players: None,
            names: &[],
        };
        crate::effect::active::add(
            &mut host,
            crate::effect::EffectInstance::new(crate::effect::MobEffect::WITHER, 40, 0),
        );
        crate::effect::living::sync_player(entity, player, env);
    }
    // The collected effects, step by step (`StepBasedCollector.applyAndClear`).
    for step in inside.steps() {
        if step.freeze {
            // `InsideBlockEffectType.FREEZE`.
            player.survival.in_powder_snow = true;
            if player_can_freeze(player) {
                let s = &mut player.survival;
                s.ticks_frozen = (s.ticks_frozen + 1).min(crate::mob::living::TICKS_TO_FREEZE);
            }
        }
        if step.clear_freeze {
            // `InsideBlockEffectType.CLEAR_FREEZE`.
            player.survival.ticks_frozen = 0;
        }
        if let Some(damage) = step.fire {
            // `BaseFireBlock.fireIgnite`, then the fire damage.
            let s = &mut player.survival;
            if s.fire_ticks < 0 {
                s.fire_ticks += 1;
            } else {
                s.fire_ticks += env.random.next_int_between(1, 3);
            }
            if s.fire_ticks >= 0 {
                ignite_for_ticks(player, 160);
            }
            let source = DamageSource::of(env.types.in_fire);
            hurt(entity, player, env, source, damage);
        }
        if step.lava_blocks > 0 && !player.survival.is_dead_or_dying() {
            // `lavaIgnite`, then `lavaHurt`.
            ignite_for_ticks(player, 300);
            let source = DamageSource::of(env.types.lava);
            if hurt(entity, player, env, source, 4.0) {
                let pitch = 2.0 + env.random.next_float() * 0.4;
                let seed = crate::survival::sound_seed();
                play_sound(
                    env.out,
                    None,
                    pos,
                    "minecraft:entity.generic.burn",
                    SoundSource::Players,
                    0.4,
                    pitch,
                    seed,
                );
            }
        }
        if let Some(snow) = step.melt_snow
            && player.survival.fire_ticks > 0
        {
            // A burning player melts the powder snow (`runBefore(EXTINGUISH)`).
            player
                .survival
                .world_actions
                .push(crate::mob::WorldAction::DestroyBlock(snow, false));
        }
        if step.extinguish {
            let s = &mut player.survival;
            s.fire_ticks = s.fire_ticks.min(0);
        }
    }
    let raining = (env.raining_at)(block_pos(pos))
        || (env.raining_at)(BlockPos::new(
            vanillite_base::math::floor(pos.x),
            vanillite_base::math::floor(aabb.max_y),
            vanillite_base::math::floor(pos.z),
        ));
    if raining {
        let s = &mut player.survival;
        s.fire_ticks = s.fire_ticks.min(0);
    }
    if was_on_fire && player.survival.fire_ticks <= 0
        || was_freezing && player.survival.ticks_frozen <= 0
    {
        // `playEntityOnFireExtinguishedSound`.
        let pitch = 1.6 + (env.random.next_float() - env.random.next_float()) * 0.4;
        let seed = crate::survival::sound_seed();
        play_sound(
            env.out,
            Some(id),
            pos,
            "minecraft:entity.generic.extinguish_fire",
            SoundSource::Players,
            0.7,
            pitch,
            seed,
        );
    }
    let ignited = player.survival.fire_ticks > previous;
    let s = &mut player.survival;
    if s.fire_ticks <= 0 && !ignited {
        s.fire_ticks = -FIRE_IMMUNE_TICKS;
    }
}

/// `LivingEntity.canFreeze` of a player: not a spectator, no leather armor.
fn player_can_freeze(player: &PlayerData) -> bool {
    use crate::item::EquipmentSlot;
    use vanillite_data::item::ids;
    if player.game_mode == GameMode::Spectator {
        return false;
    }
    let leather = [
        ids::LEATHER_BOOTS,
        ids::LEATHER_LEGGINGS,
        ids::LEATHER_CHESTPLATE,
        ids::LEATHER_HELMET,
        ids::LEATHER_HORSE_ARMOR,
    ];
    ![
        EquipmentSlot::Feet,
        EquipmentSlot::Legs,
        EquipmentSlot::Chest,
        EquipmentSlot::Head,
        EquipmentSlot::Body,
    ]
    .into_iter()
    .any(|slot| leather.contains(&crate::item::item_id(player.inventory.equipment(slot))))
}

/// `LivingEntity.aiStep`'s freezing: thawing outside powder snow and the freeze damage every
/// two seconds once fully frozen (the speed penalty is the client's).
fn tick_freezing(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let freezes = player_can_freeze(player);
    let s = &mut player.survival;
    if !s.in_powder_snow || !freezes {
        s.ticks_frozen = (s.ticks_frozen - 2).max(0);
    }
    if s.tick_count % 40 == 0 && s.ticks_frozen >= crate::mob::living::TICKS_TO_FREEZE && freezes {
        let source = DamageSource::of(env.types.freeze);
        hurt(entity, player, env, source, 1.0);
    }
}

/// `igniteForTicks`.
fn ignite_for_ticks(player: &mut PlayerData, ticks: i32) {
    let burning =
        crate::player::play::equipment_attribute(&player.inventory, "minecraft:burning_time", 1.0);
    let ticks = crate::mob::attack::burning_ticks(ticks, burning);
    let s = &mut player.survival;
    if s.fire_ticks < ticks {
        s.fire_ticks = ticks;
    }
    // `clearFreeze`.
    s.ticks_frozen = 0;
}

/// `Entity.thunderHit`: one more fire tick, eight seconds of fire when that reached zero, and
/// the lightning damage.
pub fn thunder_hit(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let s = &mut player.survival;
    s.fire_ticks = s.fire_ticks.saturating_add(1);
    if s.fire_ticks == 0 {
        ignite_for_ticks(player, 160);
    }
    let source = DamageSource::of(env.types.lightning_bolt);
    hurt(entity, player, env, source, 5.0);
}

/// The survival effects of one accepted client move, in Vanilla's order: the jump's exhaustion,
/// `doCheckFallDamage`, the fall reset after moving up and `checkMovementStatistics`.
pub fn on_move(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    mv: MoveRecord,
) {
    if player.survival.is_dead_or_dying() {
        return;
    }
    let flags = entity.as_ref().flags();
    let sprinting = flags.contains(EntityFlags::SPRINTING);
    let crouching = flags.contains(EntityFlags::CROUCHING);
    if mv.was_on_ground && !mv.on_ground && mv.moved_upwards {
        // `ServerPlayer.jumpFromGround`.
        let cost = if sprinting {
            exhaustion::SPRINT_JUMP
        } else {
            exhaustion::JUMP
        };
        player.stats.award(custom::JUMP, 1);
        cause_food_exhaustion(player, cost);
    }
    let dy = mv.to.y - mv.from.y;
    check_fall_damage(entity, player, env, dy, mv.on_ground, crouching);
    if !mv.accepted {
        // The rejected move only checks the fall damage before the player goes back.
        return;
    }
    if mv.moved_upwards {
        reset_fall_distance(player);
    }
    if player.game_mode != GameMode::Spectator && !(mv.on_ground && crouching) {
        let flying = player.survival.flying;
        movement_emission(entity, player, env, mv, crouching, !flying);
    }
    update_climbable(entity, player, env);
    movement_statistics(player, entity, env, mv);
}

/// `Entity.applyMovementEmissionAndPlaySound` for a player's move: the step and swim sounds
/// for the others (`sounds`: not while flying) and the `STEP` and `SWIM` game events.
fn movement_emission(
    entity: &EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    mv: MoveRecord,
    crouching: bool,
    sounds: bool,
) {
    let delta = mv.to - mv.from;
    #[allow(clippy::cast_possible_truncation)]
    let (moved, horizontal) = (
        (delta.length() * f64::from(0.6_f32)) as f32,
        ((delta.x * delta.x + delta.z * delta.z).sqrt() * f64::from(0.6_f32)) as f32,
    );
    let pos = mv.to;
    let at = |dy: f64| {
        BlockPos::new(
            vanillite_base::math::floor(pos.x),
            vanillite_base::math::floor(pos.y - dy),
            vanillite_base::math::floor(pos.z),
        )
    };
    // `getOnPos()` and `getOnPosLegacy()` from the feet (the supporting block search of
    // `mainSupportingBlockPos` is approximated by the block below the feet).
    let supporting_pos = at(1.0e-5);
    let effect_pos = at(0.2);
    let state = |p: BlockPos| env.blocks.block_state(p);
    let Some(supporting) = state(supporting_pos) else {
        return;
    };
    let climbing = env.facts.is_climbable(supporting.block());
    let swimming = entity.as_ref().flags().contains(EntityFlags::SWIMMING);
    let s = &mut player.survival;
    s.move_dist += if climbing { moved } else { horizontal };
    if s.move_dist <= s.next_step || supporting.is_air() {
        return;
    }
    let step = |p: BlockPos, vibrate: bool, out: &mut Vec<_>| -> bool {
        let Some(st) = state(p) else {
            return false;
        };
        if st.is_air() {
            return false;
        }
        let climbable = env.facts.is_climbable(st.block());
        if (mv.on_ground || climbable || crouching && delta.y == 0.0) && !swimming {
            if vibrate {
                out.push((crate::level::game_event::events().step, pos, Some(st)));
            }
            true
        } else {
            false
        }
    };
    let only_effect = supporting_pos == effect_pos;
    let mut produced = step(effect_pos, only_effect, &mut s.game_events);
    let effect_stepped = produced;
    if !only_effect {
        produced |= step(supporting_pos, true, &mut s.game_events);
    }
    let me = entity.as_ref();
    let (id, velocity) = (me.id(), me.velocity());
    if produced {
        s.next_step = next_step(s.move_dist);
    } else if s.fluids.in_water() {
        s.next_step = next_step(s.move_dist);
        if sounds {
            player_swim_sound(player, env, id, pos, velocity);
        }
        player
            .survival
            .game_events
            .push((crate::level::game_event::events().swim, pos, None));
    }
    if effect_stepped
        && sounds
        && let Some(on_state) = state(effect_pos)
    {
        player_walking_step_sound(player, env, id, pos, velocity, effect_pos, on_state);
    }
}

/// `Player.playSound`: heard by everybody but the player.
fn step_sound_event(
    env: &mut Env<'_>,
    id: EntityId,
    pos: Vec3,
    sound: vanillite_protocol::packets::play::clientbound::SoundEvent,
    volume: f32,
    pitch: f32,
) {
    let seed = super::sound_seed();
    super::play_sound_event(
        env.out,
        Some(id),
        pos,
        sound,
        SoundSource::Players,
        volume,
        pitch,
        seed,
    );
}

/// `Entity.playStepSound` / `playMuffledStepSound` of a block's sound type.
fn block_step_sound(
    env: &mut Env<'_>,
    id: EntityId,
    pos: Vec3,
    state: vanillite_data::block::BlockState,
    muffled: bool,
) {
    let st = state.block().sound();
    let sound =
        vanillite_protocol::packets::play::clientbound::SoundEvent(i32::from(st.step_sound));
    if muffled {
        step_sound_event(env, id, pos, sound, st.volume * 0.05, st.pitch * 0.8);
    } else {
        step_sound_event(env, id, pos, sound, st.volume * 0.15, st.pitch);
    }
}

/// `Entity.waterSwimSound` of a player (`entity.player.swim`).
fn player_swim_sound(player: &mut PlayerData, env: &mut Env<'_>, id: EntityId, pos: Vec3, v: Vec3) {
    let point_two = f64::from(0.2_f32);
    #[allow(clippy::cast_possible_truncation)]
    let volume =
        ((v.x * v.x * point_two + v.y * v.y + v.z * v.z * point_two).sqrt() as f32 * 0.35).min(1.0);
    let r = &mut player.survival.random;
    let pitch = 1.0 + (r.next_float() - r.next_float()) * 0.4;
    step_sound_event(
        env,
        id,
        pos,
        super::sound_event("minecraft:entity.player.swim"),
        volume,
        pitch,
    );
}

/// `Entity.walkingStepSound` of a player: `Player.playStepSound` (in water the swim sound and
/// a muffled step; else the block above for carpets and snow, combined with the muffled block
/// below for `#combination_step_sound_blocks`), then the amethyst chime.
#[allow(clippy::too_many_arguments)]
fn player_walking_step_sound(
    player: &mut PlayerData,
    env: &mut Env<'_>,
    id: EntityId,
    pos: Vec3,
    velocity: Vec3,
    on_pos: BlockPos,
    on_state: vanillite_data::block::BlockState,
) {
    use crate::mob::spawn::in_block_tag;
    if player.survival.fluids.in_water() {
        player_swim_sound(player, env, id, pos, velocity);
        block_step_sound(env, id, pos, on_state, true);
    } else {
        let above_pos = BlockPos::new(on_pos.x, on_pos.y + 1, on_pos.z);
        let above = env.blocks.block_state(above_pos);
        let combination =
            above.is_some_and(|a| in_block_tag(a, "minecraft:combination_step_sound_blocks"));
        let inside = above.is_some_and(|a| in_block_tag(a, "minecraft:inside_step_sound_blocks"));
        match above {
            Some(a) if combination => {
                block_step_sound(env, id, pos, a, false);
                block_step_sound(env, id, pos, on_state, true);
            }
            Some(a) if inside => block_step_sound(env, id, pos, a, false),
            _ => block_step_sound(env, id, pos, on_state, false),
        }
    }
    let s = &mut player.survival;
    if s.tick_count >= s.last_crystal_tick.saturating_add(20)
        && in_block_tag(on_state, "minecraft:crystal_sound_blocks")
    {
        #[allow(clippy::cast_possible_truncation)]
        let decay = 0.997_f64.powi(s.tick_count - s.last_crystal_tick) as f32;
        s.crystal_intensity = (s.crystal_intensity * decay + 0.07).min(1.0);
        let pitch = 0.5 + s.crystal_intensity * s.random.next_float() * 1.2;
        let volume = 0.1 + s.crystal_intensity * 1.2;
        s.last_crystal_tick = s.tick_count;
        step_sound_event(
            env,
            id,
            pos,
            super::sound_event("minecraft:block.amethyst_block.chime"),
            volume,
            pitch,
        );
    }
}

/// `Entity.nextStep`: `(int) moveDist + 1`.
fn next_step(move_dist: f32) -> f32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let next = (move_dist as i32).saturating_add(1) as f32;
    next
}

/// `onClimbable`: remembers the climbable block the player is in for the fall message.
fn update_climbable(entity: &EntityMut<'_>, player: &mut PlayerData, env: &Env<'_>) {
    if player.game_mode == GameMode::Spectator {
        return;
    }
    let pos = block_pos(entity.as_ref().pos());
    if let Some(state) = env.blocks.block_state(pos) {
        let block = state.block();
        if env.facts.is_climbable(block) {
            player.survival.last_climbable = Some(FallLocation::of_block(
                block.name(),
                env.facts.is_trapdoor(block),
            ));
        }
    }
}

/// `LivingEntity.checkFallDamage` → `Entity.checkFallDamage` → `Block.fallOn`.
fn check_fall_damage(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    dy: f64,
    on_ground: bool,
    crouching: bool,
) {
    if !player.survival.fluids.in_water() {
        update_fluids(entity, player, env);
    }
    let pos = entity.as_ref().pos();
    let on_pos = BlockPos::new(
        vanillite_base::math::floor(pos.x),
        vanillite_base::math::floor(pos.y - 0.2),
        vanillite_base::math::floor(pos.z),
    );
    let Some(on_state) = env.blocks.block_state(on_pos) else {
        // `touchingUnloadedChunk`.
        return;
    };
    if !player.survival.fluids.in_water() && dy < 0.0 {
        player.survival.fall_distance -= dy;
    }
    // `ServerPlayer.checkFallDamage`: the landing after a mace smash on the ground bursts
    // block particles.
    if player.survival.spawn_extra_particles_on_fall
        && on_ground
        && player.survival.fall_distance > 0.0
    {
        let center = Vec3::new(
            f64::from(on_pos.x) + 0.5,
            f64::from(on_pos.y) + 1.0,
            f64::from(on_pos.z) + 0.5,
        );
        #[allow(clippy::cast_possible_truncation)]
        let count = (50.0 * player.survival.fall_distance).clamp(0.0, 200.0) as i32;
        let mut particle = crate::level::particles::simple("minecraft:block");
        vanillite_protocol::codec::write_varint(&mut particle.options, i32::from(on_state.0));
        let packet = crate::level::particles::packet(
            particle,
            center,
            count,
            Vec3::new(0.3, 0.3, 0.3),
            f64::from(0.15_f32),
        );
        env.out.push(Effect::Nearby {
            pos: center,
            range: 32.0,
            except: None,
            packet: SharedPacket::of(&packet),
        });
        player.survival.spawn_extra_particles_on_fall = false;
    }
    if on_ground {
        let fall = player.survival.fall_distance;
        if fall > 0.0 {
            // `onChangedBlock` of a landing.
            if crate::enchant::location::on_changed_block(entity, player, env) {
                player.survival.equipment_dirty = true;
            }
            player.survival.game_events.push((
                crate::level::game_event::events().hit_ground,
                pos,
                Some(on_state),
            ));
            // `TurtleEggBlock.fallOn`: one egg in three breaks (spawn protection is not
            // checked).
            if on_state.block() == vanillite_data::block::ids::TURTLE_EGG {
                player
                    .survival
                    .world_actions
                    .push(crate::mob::WorldAction::TrampleTurtleEgg {
                        pos: on_pos,
                        can_destroy: true,
                        randomness: 3,
                    });
            }
            match env.facts.landing(on_state) {
                Landing::Fall(multiplier) => {
                    cause_fall_damage(entity, player, env, fall, multiplier, env.types.fall);
                }
                Landing::Slime => {
                    // `SlimeBlock.fallOn`: a sneaking player neither bounces nor takes damage.
                    if !crouching {
                        cause_fall_damage(entity, player, env, fall, 0.0, env.types.fall);
                    }
                }
                Landing::Honey => {
                    let id = entity.as_ref().id();
                    player_sound(env, id, pos, "minecraft:block.honey_block.slide");
                    env.out.push(Effect::Tracking {
                        entity: id,
                        with_self: true,
                        packet: SharedPacket::of(&EntityEvent {
                            entity_id: entity.as_ref().network_id().0,
                            event: entity_event::HONEY_SLIDE,
                        }),
                    });
                    if cause_fall_damage(entity, player, env, fall, 0.2, env.types.fall) {
                        // The honey block's fall sound (`soundType.getFallSound()`).
                        let sound = super::sound_event("minecraft:block.honey_block.fall");
                        step_sound_event(env, id, pos, sound, 0.5, 0.75);
                    }
                }
                Landing::Stalagmite => {
                    let kind = env.types.stalagmite;
                    cause_fall_damage(entity, player, env, fall + 2.5, 2.0, kind);
                }
                Landing::Bed => {
                    let fall = fall * f64::from(1.0_f32 - 0.5);
                    cause_fall_damage(entity, player, env, fall, 1.0, env.types.fall);
                }
                Landing::Farmland => {
                    // `FarmlandBlock.fallOn` (spawn protection is not checked).
                    if f64::from(env.random.next_float()) < fall - 0.5 {
                        let dims = entity.as_ref().dims();
                        if crate::block::contact::can_trample(dims.width, dims.height) {
                            let id = entity.as_ref().id();
                            player
                                .survival
                                .world_actions
                                .push(crate::mob::WorldAction::TrampleFarmland(on_pos, id));
                        }
                    }
                    cause_fall_damage(entity, player, env, fall, 1.0, env.types.fall);
                }
                Landing::PowderSnow => {
                    if fall >= 4.0 {
                        let id = entity.as_ref().id();
                        let sound = if fall < 7.0 {
                            "minecraft:entity.player.small_fall"
                        } else {
                            "minecraft:entity.player.big_fall"
                        };
                        player_sound(env, id, pos, sound);
                    }
                }
            }
        }
        reset_fall_distance(player);
        player.survival.last_climbable = None;
    }
}

/// `ShelfMushroomBlock.bounceOn`: the bounce sound at the block below that affects the
/// movement (`getBlockPosBelowThatAffectsMyMovement`) of an entity at `pos`.
pub fn shelf_mushroom_bounce(out: &mut Vec<Effect>, pos: Vec3) {
    let below = BlockPos::new(
        vanillite_base::math::floor(pos.x),
        vanillite_base::math::floor(pos.y - 0.500_001),
        vanillite_base::math::floor(pos.z),
    );
    play_sound(
        out,
        None,
        Vec3::new(
            f64::from(below.x) + 0.5,
            f64::from(below.y) + 0.5,
            f64::from(below.z) + 0.5,
        ),
        "minecraft:block.shelf_mushroom.bounce",
        vanillite_protocol::packets::play::clientbound::SoundSource::Blocks,
        1.0,
        1.0,
        super::sound_seed(),
    );
}

/// `calculateFallDamage`.
pub fn fall_damage(fall_distance: f64, multiplier: f32) -> i32 {
    fall_damage_with(fall_distance, multiplier, SAFE_FALL_DISTANCE)
}

/// `calculateFallDamage` with the `safe_fall_distance` attribute value (Jump Boost raises it).
pub fn fall_damage_with(fall_distance: f64, multiplier: f32, safe_fall_distance: f64) -> i32 {
    let power = fall_distance + 1.0e-6 - safe_fall_distance;
    // `Mth.floor(double)`.
    vanillite_base::math::floor(power * f64::from(multiplier))
}

/// `Entity.propagateFallToPassengers` for a player riding a falling mob.
pub(crate) fn passenger_fall(
    ents: &mut crate::entities::Entities,
    env: &mut Env<'_>,
    player: EntityId,
    fall_distance: f64,
    multiplier: f32,
    kind: DamageKind,
) {
    let crate::entities::Entities { store, players, .. } = ents;
    if let Some(mut entity) = store.get_mut(player) {
        let row = entity.as_ref().kind_row();
        if let Some(data) = players.get_mut(row) {
            cause_fall_damage(&mut entity, data, env, fall_distance, multiplier, kind);
        }
    }
}

/// `Player.causeFallDamage` → `LivingEntity.causeFallDamage`.
fn cause_fall_damage(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    fall_distance: f64,
    multiplier: f32,
    kind: DamageKind,
) -> bool {
    if Abilities::of(player.game_mode).mayfly {
        return false;
    }
    // `LivingEntity.causeFallDamage`: a fall after an impulse counts from its impact point.
    let fall_distance = match player.survival.impulse_impact {
        Some(impact) => {
            let effective = fall_distance.min(impact.y - entity.as_ref().pos().y);
            if effective <= 0.0 || player.survival.impulse_grace == 0 {
                player.survival.impulse_impact = None;
                player.survival.impulse_grace = 0;
            }
            effective
        }
        None => fall_distance,
    };
    if fall_distance >= 2.0 {
        #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
        let cm = (fall_distance * 100.0).round() as i32;
        player.stats.award(custom::FALL_ONE_CM, cm);
    }
    let safe = player.survival.effects.attribute(
        crate::effect::attribute::Attribute::SAFE_FALL_DISTANCE,
        SAFE_FALL_DISTANCE,
    );
    let damage = fall_damage_with(fall_distance, multiplier, safe);
    if damage > 0 {
        let id = entity.as_ref().id();
        let pos = entity.as_ref().pos();
        let sound = if damage > 4 {
            "minecraft:entity.player.big_fall"
        } else {
            "minecraft:entity.player.small_fall"
        };
        player_sound(env, id, pos, sound);
        // `playBlockFallSound`: the block 0.2 below the feet.
        if let Some(st) = env.blocks.block_state(BlockPos::new(
            vanillite_base::math::floor(pos.x),
            vanillite_base::math::floor(pos.y - f64::from(0.2_f32)),
            vanillite_base::math::floor(pos.z),
        )) && !st.is_air()
        {
            let t = st.block().sound();
            let sound =
                vanillite_protocol::packets::play::clientbound::SoundEvent(i32::from(t.fall_sound));
            step_sound_event(env, id, pos, sound, t.volume * 0.5, t.pitch * 0.75);
        }
        player.survival.impulse_impact = None;
        player.survival.impulse_grace = 0;
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let amount = damage as f32;
        hurt(entity, player, env, DamageSource::of(kind), amount);
    }
    damage > 0
}

/// `ServerPlayer.checkMovementStatistics`: the distance statistics and the exhaustion of
/// swimming, walking in water and sprinting (players never ride yet).
fn movement_statistics(
    player: &mut PlayerData,
    entity: &EntityMut<'_>,
    env: &Env<'_>,
    mv: MoveRecord,
) {
    let (dx, dy, dz) = (
        mv.to.x - mv.from.x,
        mv.to.y - mv.from.y,
        mv.to.z - mv.from.z,
    );
    // `didNotMove`.
    if dx * dx + dy * dy + dz * dz < 1.0e-5 {
        return;
    }
    // `Math.round((float)Math.sqrt(...) * 100.0F)`.
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let cm = |d: f64| ((d.sqrt() as f32) * 100.0).round() as i32;
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    let cost = |per_meter: f32, cm: i32| per_meter * cm as f32 * 0.01;
    let flags = entity.as_ref().flags();
    let fluids = player.survival.fluids;
    // `onClimbable`.
    let climbable = player.game_mode != GameMode::Spectator
        && env
            .blocks
            .block_state(block_pos(entity.as_ref().pos()))
            .is_some_and(|s| env.facts.is_climbable(s.block()));
    let in_fluid = |player: &mut PlayerData, stat, d: i32| {
        if d > 0 {
            player.stats.award(stat, d);
            cause_food_exhaustion(player, cost(exhaustion::SWIM, d));
        }
    };
    if flags.contains(EntityFlags::SWIMMING) {
        in_fluid(player, custom::SWIM_ONE_CM, cm(dx * dx + dy * dy + dz * dz));
    } else if fluids.eye_in_water {
        let d = cm(dx * dx + dy * dy + dz * dz);
        in_fluid(player, custom::WALK_UNDER_WATER_ONE_CM, d);
    } else if fluids.in_water() {
        in_fluid(player, custom::WALK_ON_WATER_ONE_CM, cm(dx * dx + dz * dz));
    } else if climbable {
        if dy > 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let up = (dy * 100.0).round() as i32;
            player.stats.award(custom::CLIMB_ONE_CM, up);
        }
    } else if mv.on_ground {
        let d = cm(dx * dx + dz * dz);
        if d > 0 {
            if flags.contains(EntityFlags::SPRINTING) {
                player.stats.award(custom::SPRINT_ONE_CM, d);
                cause_food_exhaustion(player, cost(exhaustion::SPRINT, d));
            } else if entity.as_ref().pose() == Pose::Crouching {
                player.stats.award(custom::CROUCH_ONE_CM, d);
            } else {
                player.stats.award(custom::WALK_ONE_CM, d);
            }
        }
    } else if flags.contains(EntityFlags::FALL_FLYING) {
        player
            .stats
            .award(custom::AVIATE_ONE_CM, cm(dx * dx + dy * dy + dz * dz));
    } else {
        let d = cm(dx * dx + dz * dz);
        if d > 25 {
            player.stats.award(custom::FLY_ONE_CM, d);
        }
    }
}

/// `LivingEntity.aiStep` → `Player.travel` without input, for a player the server moves itself
/// ([`Survival::simulate_movement`]): the small motion cut-off, then air or fluid travel with
/// gravity, friction and block collisions (`Entity.move`).
fn simulate_travel(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let v = entity.as_ref().velocity();
    let (x, z) = if v.x * v.x + v.z * v.z < 9.0e-6 {
        (0.0, 0.0)
    } else {
        (v.x, v.z)
    };
    let y = if v.y.abs() < 0.003 { 0.0 } else { v.y };
    entity.set_velocity(Vec3::new(x, y, z));
    // A dying player keeps moving (`travel` runs for immobile entities too).
    let fluids = player.survival.fluids;
    if fluids.in_water() || fluids.in_lava() {
        travel_in_fluid(entity, player, env);
    } else if entity.as_ref().flags().contains(EntityFlags::FALL_FLYING) {
        travel_fall_flying(entity, player, env);
    } else {
        travel_in_air(entity, player, env);
    }
}

/// The `Entity.move(MoverType.SELF, deltaMovement)` of `Player.travel` for a player its client
/// moves: how far the motion takes it past the blocks in the way (after the small motion
/// cut-off of `aiStep`).
fn server_step(entity: &EntityMut<'_>, env: &Env<'_>) -> Vec3 {
    let v = entity.as_ref().velocity();
    let (x, z) = if v.x * v.x + v.z * v.z < 9.0e-6 {
        (0.0, 0.0)
    } else {
        (v.x, v.z)
    };
    let y = if v.y.abs() < 0.003 { 0.0 } else { v.y };
    let delta = Vec3::new(x, y, z);
    let blocks = MoveBlocks(env.blocks, env.border);
    let mut scratch = Vec::new();
    let movement = crate::mob::physics::collide(
        &blocks,
        entity.as_ref().aabb(),
        delta,
        0.6,
        entity.as_ref().flags().contains(EntityFlags::ON_GROUND),
        &mut scratch,
    );
    let length = movement.length_sqr();
    if length > 1.0e-7 || delta.length_sqr() - length < 1.0e-7 {
        movement
    } else {
        Vec3::ZERO
    }
}

/// `LivingEntity.aiStep` → `Player.travel` without input for a player its client moves
/// (`canSimulateMovement` is true for players on the server too): only the motion, as the next
/// movement packet replaces the position. Knockback and other impulses decay with friction and
/// gravity like in Vanilla, so the motion sent with a later hurt (`syncVelocity`, e.g. fall
/// damage) and the next knockback start from it instead of from a stale impulse.
fn travel_motion(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let v = entity.as_ref().velocity();
    let (x, z) = if v.x * v.x + v.z * v.z < 9.0e-6 {
        (0.0, 0.0)
    } else {
        (v.x, v.z)
    };
    let y = if v.y.abs() < 0.003 { 0.0 } else { v.y };
    let v = Vec3::new(x, y, z);
    if player.survival.is_dead_or_dying() {
        entity.set_velocity(v);
        return;
    }
    let flags = entity.as_ref().flags();
    let flying = player.survival.flying;
    if flags.contains(EntityFlags::FALL_FLYING) && !flying {
        // The glide follows the look the server sees only through the client's moves.
        entity.set_velocity(player.connection.movement.known_movement);
        return;
    }
    let gravity = player_gravity(entity, player);
    let fluids = player.survival.fluids;
    let on_ground = flags.contains(EntityFlags::ON_GROUND);
    // The move into the ground collides and stops the fall.
    let fall = if on_ground && v.y < 0.0 { 0.0 } else { v.y };
    let m = if fluids.in_water() {
        let slow_down = if player
            .survival
            .effects
            .has(crate::effect::MobEffect::DOLPHINS_GRACE)
        {
            0.96_f32
        } else {
            0.8
        };
        Vec3::new(
            v.x * f64::from(slow_down),
            fall * f64::from(0.8_f32) - gravity / 16.0,
            v.z * f64::from(slow_down),
        )
    } else if fluids.in_lava() {
        Vec3::new(v.x * 0.5, fall * 0.5 - gravity / 4.0, v.z * 0.5)
    } else {
        let pos = entity.as_ref().pos();
        let below = env.blocks.block_state(pos_below_affecting_movement(pos));
        let block_friction = match below {
            Some(state) if on_ground => state.block().info().friction,
            _ => 1.0,
        };
        let friction = f64::from(block_friction * 0.91);
        let y = if let Some(a) = player
            .survival
            .effects
            .amplifier(crate::effect::MobEffect::LEVITATION)
        {
            fall + (0.05 * f64::from(a + 1) - fall) * 0.2
        } else if below.is_some() {
            fall - gravity
        } else if pos.y > f64::from(env.min_y) {
            -0.1
        } else {
            0.0
        };
        Vec3::new(v.x * friction, y * f64::from(0.98_f32), v.z * friction)
    };
    // `Player.travel` while flying keeps 60 % of the vertical motion instead.
    let y = if flying { v.y * 0.6 } else { m.y };
    entity.set_velocity(Vec3::new(m.x, y, m.z));
}

/// `move(SELF, (0, dy, 0))` of a player the server moves itself (the riptide lift).
pub fn lift_player(
    ents: &mut crate::entities::Entities,
    env: &mut Env<'_>,
    player: EntityId,
    dy: f64,
) {
    let crate::entities::Entities { store, players, .. } = ents;
    let Some(mut entity) = store.get_mut(player) else {
        return;
    };
    let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
        return;
    };
    let v = entity.as_ref().velocity();
    simulate_move(&mut entity, data, env, Vec3::new(0.0, dy, 0.0));
    entity.set_velocity(v);
}

/// `travelFallFlying`: the glide (`updateFallFlyingMovement`), the move, and the damage of
/// flying into a wall; on a climbable block the player falls normally and stops gliding.
fn travel_fall_flying(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    if player.survival.last_climbable.is_some() {
        travel_in_air(entity, player, env);
        entity.set_flags(EntityFlags::FALL_FLYING, false);
        entity.mark_data_dirty();
        return;
    }
    let last = entity.as_ref().velocity();
    let last_speed = (last.x * last.x + last.z * last.z).sqrt();
    let rot = entity.as_ref().rot();
    let gravity = player_gravity(entity, player);
    let m = crate::elytra::fall_flying_movement(last, rot.x_rot, rot.y_rot, gravity);
    entity.set_velocity(m);
    simulate_move(entity, player, env, m);
    if player.survival.horizontal_collision {
        let now = entity.as_ref().velocity();
        let new_speed = (now.x * now.x + now.z * now.z).sqrt();
        if let Some(damage) = crate::elytra::fly_into_wall_damage(last_speed, new_speed) {
            let sound = if damage > 4.0 {
                "minecraft:entity.player.big_fall"
            } else {
                "minecraft:entity.player.small_fall"
            };
            let seed = crate::survival::sound_seed();
            crate::survival::play_sound(
                env.out,
                None,
                entity.as_ref().pos(),
                sound,
                SoundSource::Players,
                1.0,
                1.0,
                seed,
            );
            let source = DamageSource::of(env.types.named("minecraft:fly_into_wall"));
            hurt(entity, player, env, source, damage);
        }
    }
}

/// The `minecraft:gravity` attribute of players.
const GRAVITY: f64 = 0.08;

/// `getEffectiveGravity` of a player.
fn player_gravity(entity: &EntityMut<'_>, player: &PlayerData) -> f64 {
    let gravity = if entity.as_ref().flags().contains(EntityFlags::NO_GRAVITY) {
        0.0
    } else {
        GRAVITY
    };
    let falling = entity.as_ref().velocity().y <= 0.0;
    player.survival.effects.effective_gravity(gravity, falling)
}

/// `getBlockPosBelowThatAffectsMyMovement`.
fn pos_below_affecting_movement(pos: Vec3) -> BlockPos {
    block_pos(Vec3::new(pos.x, pos.y - f64::from(0.500_001_f32), pos.z))
}

fn travel_in_air(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let pos = entity.as_ref().pos();
    let on_ground = entity.as_ref().flags().contains(EntityFlags::ON_GROUND);
    let below = pos_below_affecting_movement(pos);
    let below_state = env.blocks.block_state(below);
    let block_friction = match below_state {
        Some(state) if on_ground => state.block().info().friction,
        _ => 1.0,
    };
    let v = entity.as_ref().velocity();
    simulate_move(entity, player, env, v);
    let movement = entity.as_ref().velocity();
    let mut y = movement.y;
    if let Some(a) = player
        .survival
        .effects
        .amplifier(crate::effect::MobEffect::LEVITATION)
    {
        y += (0.05 * f64::from(a + 1) - movement.y) * 0.2;
    } else if below_state.is_some() {
        y -= player_gravity(entity, player);
    } else if entity.as_ref().pos().y > f64::from(env.min_y) {
        y = -0.1;
    } else {
        y = 0.0;
    }
    let friction = block_friction * 0.91;
    entity.set_velocity(Vec3::new(
        movement.x * f64::from(friction),
        y * f64::from(0.98_f32),
        movement.z * f64::from(friction),
    ));
}

fn travel_in_fluid(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    let falling = entity.as_ref().velocity().y <= 0.0;
    let old_y = entity.as_ref().pos().y;
    let gravity = player_gravity(entity, player);
    let adjusted = |movement: Vec3| {
        if gravity == 0.0 {
            return movement;
        }
        let y = if falling
            && (movement.y - 0.005).abs() >= 0.003
            && (movement.y - gravity / 16.0).abs() < 0.003
        {
            -0.003
        } else {
            movement.y - gravity / 16.0
        };
        Vec3::new(movement.x, y, movement.z)
    };
    let v = entity.as_ref().velocity();
    simulate_move(entity, player, env, v);
    let movement = entity.as_ref().velocity();
    if player.survival.fluids.in_water() {
        // `travelInWater`: the player's water slow down; no input, no Depth Strider here.
        let mut slow_down = 0.8_f32;
        if player
            .survival
            .effects
            .has(crate::effect::MobEffect::DOLPHINS_GRACE)
        {
            slow_down = 0.96;
        }
        let m = movement.multiply(
            f64::from(slow_down),
            f64::from(0.8_f32),
            f64::from(slow_down),
        );
        entity.set_velocity(adjusted(m));
    } else {
        let m = if player.survival.fluids.lava_height <= 0.4 {
            adjusted(movement.multiply(0.5, f64::from(0.8_f32), 0.5))
        } else {
            movement * 0.5
        };
        let m = if gravity == 0.0 {
            m
        } else {
            Vec3::new(m.x, m.y - gravity / 4.0, m.z)
        };
        entity.set_velocity(m);
    }
    // `jumpOutOfFluid`.
    let v = entity.as_ref().velocity();
    if player.survival.horizontal_collision {
        let pos = entity.as_ref().pos();
        let probe =
            entity
                .as_ref()
                .aabb()
                .offset(v.x, v.y + f64::from(0.6_f32) - pos.y + old_y, v.z);
        let blocks = MoveBlocks(env.blocks, env.border);
        let mut scratch = Vec::new();
        if crate::mob::physics::no_collision(&blocks, &probe, &mut scratch)
            && !crate::mob::physics::contains_any_liquid(&blocks, &probe)
        {
            entity.set_velocity(Vec3::new(v.x, f64::from(0.3_f32), v.z));
        }
    }
}

/// The world's blocks for [`crate::mob::physics`] (air where nothing is loaded).
struct MoveBlocks<'a>(
    &'a dyn BlockAccess,
    Option<&'a crate::level::border::WorldBorder>,
);

impl crate::mob::physics::Blocks for MoveBlocks<'_> {
    fn state(&self, pos: BlockPos) -> vanillite_data::block::BlockState {
        self.0
            .block_state(pos)
            .unwrap_or(vanillite_data::block::BlockState::AIR)
    }

    fn states_in(
        &self,
        min: BlockPos,
        max: BlockPos,
        out: &mut [vanillite_data::block::BlockState],
    ) {
        let mut slots = out.iter_mut();
        self.0.block_states_in(min, max, &mut |state| {
            if let Some(slot) = slots.next() {
                *slot = state.unwrap_or(vanillite_data::block::BlockState::AIR);
            }
        });
    }

    fn border(&self) -> Option<&crate::level::border::WorldBorder> {
        self.1
    }

    fn moving_piston_boxes(&self, pos: BlockPos, out: &mut Vec<[f64; 6]>) {
        self.0.moving_piston_boxes(pos, out);
    }
}

/// `Entity.move(MoverType.SELF, delta)` of a player the server moves: collisions with step up,
/// the ground flag, fall damage, the colliding axes stopped, the step game events and the
/// block speed factor.
fn simulate_move(
    entity: &mut EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    delta: Vec3,
) {
    let from = entity.as_ref().pos();
    let was_on_ground = entity.as_ref().flags().contains(EntityFlags::ON_GROUND);
    let blocks = MoveBlocks(env.blocks, env.border);
    let mut scratch = Vec::new();
    let movement = crate::mob::physics::collide(
        &blocks,
        entity.as_ref().aabb(),
        delta,
        0.6,
        was_on_ground,
        &mut scratch,
    );
    let length = movement.length_sqr();
    if length > 1.0e-7 || delta.length_sqr() - length < 1.0e-7 {
        crate::survival::inside::record(
            &mut player.connection.movement.path,
            crate::survival::inside::Movement {
                from,
                to: from + movement,
                original: Some(delta),
            },
        );
        entity.set_pos(from + movement);
    }
    let x_collision = !java_equal(delta.x, movement.x);
    let z_collision = !java_equal(delta.z, movement.z);
    let horizontal = x_collision || z_collision;
    player.survival.horizontal_collision = horizontal;
    let moved_vertically = delta.y.abs() > 0.0;
    let mut vertical = false;
    let mut on_ground = was_on_ground;
    if moved_vertically {
        vertical = delta.y != movement.y;
        on_ground = vertical && delta.y < 0.0;
        entity.set_flags(EntityFlags::ON_GROUND, on_ground);
    }
    // `isLocalInstanceAuthoritative` is false for players on the server: their fall damage
    // comes with their movement packets only.
    let crouching = entity.as_ref().flags().contains(EntityFlags::CROUCHING);
    if moved_vertically && vertical || horizontal {
        let mut v = entity.as_ref().velocity();
        // `restituteMovementAfterCollisions` (Java's signed zeros): players bounce on the
        // blocks below them (beds, slime) unless they sneak.
        let current = v;
        if x_collision {
            v.x = -v.x * 0.0;
        }
        if z_collision {
            v.z = -v.z * 0.0;
        }
        if vertical {
            let gravity = player_gravity(entity, player);
            let below = entity.as_ref().pos();
            let effect_state = env.blocks.block_state(BlockPos::new(
                vanillite_base::math::floor(below.x),
                vanillite_base::math::floor(below.y - 0.2),
                vanillite_base::math::floor(below.z),
            ));
            let restitution = match effect_state {
                Some(state)
                    if delta.y < 0.0
                        && -current.y > gravity
                        && !crouching
                        && !crate::block::block_tags().suppresses_bounce.contains(state) =>
                {
                    bounce_restitution(state)
                }
                _ => 0.0,
            };
            v.y = if restitution > 0.0 {
                let portion = movement.y / current.y;
                let drag = 1.0 + portion * (f64::from(0.98_f32) - 1.0);
                (portion * gravity - current.y) * drag * restitution
            } else {
                -current.y * 0.0
            };
            if restitution > 0.0 {
                if effect_state
                    .is_some_and(|s| s.block() == vanillite_data::block::ids::SHELF_MUSHROOM)
                {
                    shelf_mushroom_bounce(env.out, below);
                }
                player.survival.game_events.push((
                    crate::level::game_event::events().bounce,
                    below,
                    None,
                ));
            }
        }
        entity.set_velocity(v);
    }
    let to = entity.as_ref().pos();
    if player.game_mode != GameMode::Spectator && !(on_ground && crouching) {
        let mv = MoveRecord {
            from,
            to,
            was_on_ground,
            on_ground,
            moved_upwards: false,
            accepted: true,
        };
        let flying = player.survival.flying;
        movement_emission(entity, player, env, mv, crouching, !flying);
    }
    // `getBlockSpeedFactor`.
    let here = env
        .blocks
        .block_state(block_pos(to))
        .map_or(1.0, |s| s.block().info().speed_factor);
    let factor = if here == 1.0 {
        env.blocks
            .block_state(pos_below_affecting_movement(to))
            .map_or(1.0, |s| s.block().info().speed_factor)
    } else {
        here
    };
    let v = entity.as_ref().velocity();
    entity.set_velocity(v.multiply(f64::from(factor), 1.0, f64::from(factor)));
}

/// `Block.getBounceRestitution` of the block a living entity lands on.
fn bounce_restitution(state: vanillite_data::block::BlockState) -> f64 {
    use vanillite_data::block::ids;
    let block = state.block();
    if block == ids::SLIME_BLOCK {
        1.0
    } else if block == ids::SHELF_MUSHROOM
        || (block.info().kind == vanillite_data::block::BlockKind::BedBlock)
    {
        f64::from(0.75_f32)
    } else {
        0.0
    }
}

/// `Math.abs(a - b) < 1.0E-5F` as `Mth.equal`.
fn java_equal(a: f64, b: f64) -> bool {
    (b - a).abs() < f64::from(1.0e-5_f32)
}

/// `LivingEntity.tick` after `aiStep`: the body turns towards the walk direction
/// (`tickHeadTurn`), for a player the server moves.
fn tick_head_turn(entity: &EntityMut<'_>, s: &mut Survival) {
    let e = entity.as_ref();
    let d = e.pos() - e.old_pos();
    let yaw = e.rot().y_rot;
    #[allow(clippy::cast_possible_truncation)]
    let side = (d.x * d.x + d.z * d.z) as f32;
    let mut target = s.y_body_rot;
    if side > 0.002_500_000_2 {
        #[allow(clippy::cast_possible_truncation)]
        let walk =
            vanillite_base::math::atan2(d.z, d.x) as f32 * (180.0 / std::f32::consts::PI) - 90.0;
        let diff = (vanillite_base::math::wrap_degrees_f(yaw) - walk).abs();
        target = if 95.0 < diff && diff < 265.0 {
            walk - 180.0
        } else {
            walk
        };
    }
    s.y_body_rot += vanillite_base::math::wrap_degrees_f(target - s.y_body_rot) * 0.3;
    let head = vanillite_base::math::wrap_degrees_f(yaw - s.y_body_rot);
    if head.abs() > 50.0 {
        s.y_body_rot += head - head.signum() * 50.0;
    }
}

/// `getAttributeValue(KNOCKBACK_RESISTANCE)` of a player: its equipment's modifiers.
fn knockback_resistance(player: &PlayerData) -> f64 {
    crate::player::play::equipment_attribute(
        &player.inventory,
        "minecraft:knockback_resistance",
        0.0,
    )
}

/// `LivingEntity.decreaseAirSupply`: with `OXYGEN_BONUS` (Respiration) a tick may keep the air,
/// drawing from the player's random.
fn decrease_air(player: &mut PlayerData, air: i32) -> i32 {
    let bonus =
        crate::player::play::equipment_attribute(&player.inventory, "minecraft:oxygen_bonus", 0.0);
    if bonus > 0.0 && player.survival.random.next_double() >= 1.0 / (bonus + 1.0) {
        air
    } else {
        air - 1
    }
}

#[cfg(test)]
mod tests {

    /// `SwingState.startIfAble`: a new swing starts unless the current one is in its first
    /// half; Haste shortens the duration (#272, I3-16).
    #[test]
    fn swing_starts_only_after_half_the_current_one() {
        let mut s = Survival::default();
        assert!(start_swing(&mut s, 6));
        assert_eq!(s.swing, Some((0, 6)));
        // In the same tick (ticks 0) it starts again.
        assert!(start_swing(&mut s, 6));
        s.swing = Some((3, 6));
        assert!(!start_swing(&mut s, 6));
        s.swing = Some((4, 6));
        assert!(start_swing(&mut s, 6));
    }

    use super::*;

    /// `calculateFallDamage` of Vanilla 26.3: `floor(distance + 1e-6 - 3)`.
    #[test]
    fn fall_damage_values() {
        assert_eq!(fall_damage(3.0, 1.0), 0);
        assert_eq!(fall_damage(3.5, 1.0), 0);
        assert_eq!(fall_damage(4.0, 1.0), 1);
        // The captured Vanilla fall: 12 blocks cost 9 health.
        assert_eq!(fall_damage(12.0, 1.0), 9);
        assert_eq!(fall_damage(23.0, 1.0), 20);
        // Hay bale.
        assert_eq!(fall_damage(23.0, 0.2), 4);
        // Slime without sneaking.
        assert_eq!(fall_damage(50.0, 0.0), 0);
    }

    #[test]
    fn experience_levels() {
        let mut xp = Experience::default();
        xp.give_points(7);
        assert_eq!((xp.level, xp.total), (1, 7));
        assert_eq!(xp.progress, 0.0);
        xp.give_points(9 + 11 + 13);
        assert_eq!(xp.level, 4);
        assert_eq!(xp.needed_for_next_level(), 15);
        let mut high = Experience {
            level: 30,
            ..Experience::default()
        };
        assert_eq!(high.needed_for_next_level(), 112);
        high.give_levels(-31);
        assert_eq!((high.level, high.total), (0, 0));
    }

    #[test]
    fn survival_nbt_round_trip() {
        let mut s = Survival {
            health: 7.5,
            ..Survival::default()
        };
        s.food.food_level = 13;
        s.air = 120;
        s.xp.level = 3;
        s.last_death = Some(("minecraft:overworld".to_owned(), BlockPos::new(1, -60, 2)));
        let mut nbt = Compound::new();
        s.save(&mut nbt);
        let back = Survival::load(&nbt);
        assert_eq!(back.health, 7.5);
        assert_eq!(back.food.food_level, 13);
        assert_eq!(back.air, 120);
        assert_eq!(back.xp.level, 3);
        assert_eq!(back.last_death, s.last_death);
    }
}
