//! The mob families: every kind beyond the classic ones (zombie, skeleton, creeper, spider,
//! enderman and the farm animals in `kinds.rs`) belongs to a family module that implements
//! [`FamilyHooks`]. The hooks are the points where Vanilla's subclasses override `Mob`:
//! goals, `finalizeSpawn`, the tick parts, combat, interaction, synched data and NBT. Every
//! hook has the behaviour of the base classes as its default, so a family only writes what
//! differs.
//!
//! Dispatch is a `match` on [`Family`] with static calls: no dynamic dispatch on the hot path.
//! A family keeps its kind state in [`Ext`] and its goals in [`FamGoal`].

pub mod boss;
pub mod brainy;
pub mod classic;
pub mod companion;
pub mod decor;
pub mod illager;
pub mod mount;
pub mod undead;
pub mod village;
pub mod wildlife;

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::Compound;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{Dimensions, EntityId};

use super::animal::Interaction;
use super::kinds::{DefaultSound, FarmClimate, Kind};
use super::path::MalusTable;
use super::spawn::{BiomeSpawns, Category};
use super::{Environment, MobContext, MobData, Tick};
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::level::difficulty::DifficultyInstance;
use crate::player::encode::DataWriter;
use crate::survival::damage::DamageSource;
use crate::survival::player::Env;
use crate::survival::rules::Difficulty;

/// The family of a kind: which module implements it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    /// The kinds in `kinds.rs`.
    Classic,
    /// Zombie and skeleton variants, small monsters, cubes, phantoms.
    Undead,
    /// Raiders, vexes, ghasts, blazes, shulkers.
    Illager,
    /// Brain AI mobs: piglins, hoglins, goats, axolotls, frogs, armadillos, sniffers.
    Brainy,
    /// Brain AI mounts and helpers: camels, happy ghasts, nautiluses, allays, copper golems,
    /// breezes, creakings.
    Mount,
    /// Horses, llamas, wolves, cats, parrots, foxes, pandas, polar bears, rabbits.
    Companion,
    /// Bees, turtles, mooshrooms, striders, golems, bats, squids, dolphins, fish.
    Wildlife,
    /// Guardians, the wither, the ender dragon and the warden (`crate::mob::boss`).
    Boss,
    /// Armor stands and mannequins: living entities without AI (`crate::decoration::stand`).
    Decor,
    /// Villagers and wandering traders (`AbstractVillager`).
    Village,
}

/// Why a mob spawns (`EntitySpawnReason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnReason {
    Natural,
    ChunkGeneration,
    Spawner,
    Structure,
    Breeding,
    MobSummoned,
    Jockey,
    Event,
    Conversion,
    Reinforcement,
    Triggered,
    Bucket,
    SpawnItemUse,
    Command,
    Dispenser,
    Patrol,
    TrialSpawner,
    Load,
    DimensionTravel,
}

impl SpawnReason {
    /// The plugin code (`vanillite_abi::spawn_reason`; `LOAD` never fires an event).
    pub fn abi(self) -> u32 {
        use vanillite_abi::spawn_reason as r;
        match self {
            SpawnReason::Natural => r::NATURAL,
            SpawnReason::ChunkGeneration => r::CHUNK_GENERATION,
            SpawnReason::Spawner => r::SPAWNER,
            SpawnReason::Structure => r::STRUCTURE,
            SpawnReason::Breeding => r::BREEDING,
            SpawnReason::MobSummoned => r::MOB_SUMMONED,
            SpawnReason::Jockey => r::JOCKEY,
            SpawnReason::Event => r::EVENT,
            SpawnReason::Conversion => r::CONVERSION,
            SpawnReason::Reinforcement => r::REINFORCEMENT,
            SpawnReason::Triggered => r::TRIGGERED,
            SpawnReason::Bucket => r::BUCKET,
            SpawnReason::SpawnItemUse => r::SPAWN_ITEM_USE,
            SpawnReason::Command => r::COMMAND,
            SpawnReason::Dispenser => r::DISPENSER,
            SpawnReason::Patrol => r::PATROL,
            SpawnReason::TrialSpawner => r::TRIAL_SPAWNER,
            SpawnReason::Load => r::DEFAULT,
            SpawnReason::DimensionTravel => r::DIMENSION_TRAVEL,
        }
    }
}

/// How a mob that spawns together with another one relates to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// It rides the new mob (a skeleton on a spider).
    RidesIt,
    /// The new mob rides it (a baby zombie on a chicken).
    CarriesIt,
    /// Spawned next to it.
    Beside,
    /// It rides an earlier extra mob (by its index in [`SpawnContext::extra`]): the second
    /// rider of a camel husk.
    RidesExtra(usize),
    /// The new mob rides this existing entity (the mob of the entry is unused).
    Existing(EntityId),
}

/// A mob spawned together with the one being finalized (jockeys, the horses of a skeleton
/// trap).
#[derive(Debug)]
pub struct ExtraMob {
    pub mob: MobData,
    /// Offset from the new mob's position.
    pub offset: Vec3,
    pub relation: Relation,
}

/// The `SpawnGroupData` a spawn group shares beyond its size: what the first member chose and
/// the later members take over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupData {
    /// No group data yet (the first member, or a single spawn).
    #[default]
    None,
    /// `Horse.HorseGroupData`: the coat.
    Horse(i32),
    /// `Llama.LlamaGroupData`: the variant.
    Llama(i32),
    /// `Wolf.WolfPackData`: the variant.
    Wolf(i32),
    /// `Rabbit.RabbitGroupData`: the variant.
    Rabbit(i32),
    /// `Fox.FoxGroupData`: the variant.
    Fox(i32),
    /// `Axolotl.AxolotlGroupData`: two common variants to pick from.
    Axolotl(i32, i32),
    /// `TropicalFish.TropicalFishGroupData`: the packed variant.
    TropicalFish(i32),
    /// `Spider.SpiderEffectsGroupData`: the effect of the group, if any.
    Spider(Option<crate::effect::MobEffect>),
    /// `Zombie.ZombieGroupData`: the whole group spawns as babies or as adults.
    Zombie(bool),
}

/// What `finalizeSpawn` gets (`DifficultyInstance`, the spawn reason, the biome) and the mobs
/// it adds.
#[derive(Debug)]
pub struct SpawnContext<'a> {
    pub difficulty: Difficulty,
    /// `DifficultyInstance.getSpecialMultiplier`.
    pub special_multiplier: f32,
    /// `DifficultyInstance.getEffectiveDifficulty`.
    pub effective_difficulty: f32,
    pub climate: FarmClimate,
    /// Index of the mob in its spawn group (0 for the first).
    pub group_index: u32,
    /// The group data the earlier members of the spawn group left; `finalizeSpawn` updates it.
    pub group: GroupData,
    pub reason: SpawnReason,
    /// The biome at the spawn position (`None` without data packs).
    pub biome: Option<&'a BiomeSpawns>,
    pub pos: BlockPos,
    /// The level when it can be read (natural spawning, commands); `None` in tests.
    pub level: Option<&'a Level>,
    /// The environment of the world (sky darkness, weather).
    pub env: Environment,
    /// Mobs to spawn with this one.
    pub extra: Vec<ExtraMob>,
    /// An existing entity the new mob rides (a free chicken for a baby zombie).
    pub ride: Option<EntityId>,
    /// The state of the mob's own random while `finalizeSpawn` runs (`this.random`; the
    /// mob's `random` is the level's meanwhile).
    pub own_state: i64,
}

impl<'a> SpawnContext<'a> {
    /// A context without a level (tests, simple spawns): the difficulty of a fresh world.
    pub fn simple(difficulty: Difficulty, reason: SpawnReason) -> SpawnContext<'static> {
        let instance = DifficultyInstance::new(difficulty, 0, 0, 0.0);
        SpawnContext {
            difficulty,
            special_multiplier: instance.special_multiplier(),
            effective_difficulty: instance.effective,
            climate: FarmClimate::Temperate,
            group_index: 0,
            group: GroupData::None,
            reason,
            biome: None,
            pos: BlockPos::new(0, 64, 0),
            level: None,
            env: Environment::default(),
            extra: Vec::new(),
            ride: None,
            own_state: 0,
        }
    }

    /// A context at `pos` of `level` with its local difficulty
    /// (`level.getCurrentDifficultyAt(pos)`).
    pub fn at(level: &'a Level, pos: BlockPos, reason: SpawnReason) -> SpawnContext<'a> {
        let instance = crate::level::difficulty::at(level, pos);
        SpawnContext {
            difficulty: instance.base,
            special_multiplier: instance.special_multiplier(),
            effective_difficulty: instance.effective,
            pos,
            level: Some(level),
            ..SpawnContext::simple(instance.base, reason)
        }
    }

    /// `biome.is(tag)`; `false` without a biome.
    pub fn biome_is(&self, tag: &str) -> bool {
        self.biome.is_some_and(|b| b.is(tag))
    }
}

/// `SpecialDates.isHalloween`: October 20 to November 3 (UTC).
pub fn is_halloween() -> bool {
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    let days = i64::try_from(now.as_secs() / 86_400).unwrap_or(0);
    let (month, day) = month_day(days);
    (month == 10 && day >= 20) || (month == 11 && day <= 3)
}

/// Month and day of a day number since 1970-01-01 (`civil_from_days`).
fn month_day(days: i64) -> (u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// A player using an item on a mob (`mobInteract`).
pub struct Interact<'a, 'e> {
    pub level: &'a Level,
    pub ents: &'a mut Entities,
    pub env: &'a mut Env<'e>,
    pub cx: &'a mut MobContext,
    pub player: EntityId,
    pub target: EntityId,
    pub hand: Hand,
    pub sneaking: bool,
    /// The held stack (a copy; change the player's inventory through the helpers in
    /// `animal.rs`).
    pub stack: &'a ItemStack,
}

impl std::fmt::Debug for Interact<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interact")
            .field("player", &self.player)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// A hit on a mob (`hurtServer`).
pub struct Hurt<'a, 'e> {
    pub level: &'a Level,
    pub ents: &'a mut Entities,
    pub env: &'a mut Env<'e>,
    pub cx: &'a mut MobContext,
    pub target: EntityId,
    pub source: DamageSource,
}

impl std::fmt::Debug for Hurt<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hurt")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// What a family overrides. Every method has the base class behaviour as its default.
pub trait FamilyHooks {
    /// The family's goals (`Copy`, run by the goal selector like the common goals).
    type Goal: Copy + std::fmt::Debug;

    // --- Kind properties --------------------------------------------------------------

    /// `Monster` or `Enemy`: hostile sounds, despawning, peaceful removal.
    fn is_monster(kind: Kind) -> bool {
        Category::of(kind.type_id()) == Some(Category::Monster)
    }
    /// `AgeableMob`: grows up, has babies.
    fn ageable(_kind: Kind) -> bool {
        false
    }
    fn ambient_sound_interval(kind: Kind) -> i32 {
        if Self::is_monster(kind) { 80 } else { 120 }
    }
    fn burns_in_daylight(_kind: Kind) -> bool {
        false
    }
    fn breathes_underwater(_kind: Kind) -> bool {
        false
    }
    fn sensitive_to_water(_kind: Kind) -> bool {
        false
    }
    fn fall_damage_immune(_kind: Kind) -> bool {
        false
    }
    fn allowed_in_peaceful(kind: Kind) -> bool {
        !Self::is_monster(kind)
    }
    /// `removeWhenFarAway` without state.
    fn remove_when_far_away(kind: Kind) -> bool {
        Self::is_monster(kind)
    }
    /// `removeWhenFarAway(distSqr)` with the mob's state (tamed cats, fish from buckets,
    /// patrols); defaults to the kind's.
    fn remove_when_far_away_mob(m: &MobData, _dist_sqr: f64) -> bool {
        Self::remove_when_far_away(m.kind)
    }
    /// `requiresCustomPersistence` (tamed, from a bucket, ...).
    fn requires_custom_persistence(_m: &MobData) -> bool {
        false
    }
    fn malus(_kind: Kind) -> &'static MalusTable {
        &[]
    }
    fn sound_source(kind: Kind) -> SoundSource {
        if Self::is_monster(kind) {
            SoundSource::Hostile
        } else {
            SoundSource::Neutral
        }
    }
    fn sound_volume(_kind: Kind) -> f32 {
        1.0
    }
    /// `getSoundVolume` with the mob's state (slimes scale it with their size).
    fn mob_sound_volume(m: &MobData) -> f32 {
        Self::sound_volume(m.kind)
    }
    /// What `jumpFromGround` adds to the jump power (magma cubes jump higher when large).
    fn jump_power_bonus(_m: &MobData) -> f32 {
        0.0
    }
    /// `jumpInLiquid` when the kind does it differently; `true` when handled.
    fn jump_in_liquid(_t: &mut Tick<'_, '_>) -> bool {
        false
    }
    fn ambient_sound(m: &mut MobData) -> Option<&'static str> {
        Some(m.kind.default_sound(DefaultSound::Ambient))
    }
    fn hurt_sound(m: &MobData) -> &'static str {
        m.kind.default_sound(DefaultSound::Hurt)
    }
    fn death_sound(m: &MobData) -> &'static str {
        m.kind.default_sound(DefaultSound::Death)
    }
    /// `getBaseExperienceReward`.
    fn experience_reward(m: &mut MobData) -> i32 {
        if Self::is_monster(m.kind) {
            5
        } else if Self::ageable(m.kind) {
            1 + m.random.next_int_bounded(3)
        } else {
            0
        }
    }
    /// The dimensions of the mob when they depend on its state (slime size, baby scale);
    /// `None` for the type's (half size for babies).
    fn dimensions(_m: &MobData) -> Option<Dimensions> {
        None
    }
    /// The factor the kind's default dimensions and eye height are scaled by (sizes and
    /// puff states; babies at half size otherwise).
    fn size_scale(_m: &MobData) -> Option<f32> {
        None
    }
    /// `getWalkTargetValue(pos)`.
    fn walk_target_value(kind: Kind, level: &Level, env: &Environment, pos: BlockPos) -> f64 {
        super::kinds::base_walk_target_value(Self::is_monster(kind), level, env, pos)
    }
    /// `SpawnPlacements.checkSpawnRules` of the kind (the placement type and heightmap are
    /// checked before).
    fn check_spawn_rules(
        kind: Kind,
        level: &Level,
        env: &Environment,
        pos: BlockPos,
        random: &mut LegacyRandom,
        reason: SpawnReason,
    ) -> bool {
        super::spawn::default_spawn_rules(kind, level, env, pos, random, reason)
    }
    /// `startPersistentAngerTimer`'s range in ticks (`NeutralMob`).
    fn anger_range(_kind: Kind) -> (i32, i32) {
        (20 * 20, 39 * 20)
    }
    /// `isMaxGroupSizeReached(groupSize)` of natural spawning (tropical fish schools).
    fn is_max_group_size_reached(_m: &MobData, _group_size: i32) -> bool {
        false
    }
    /// How many passengers the mob carries (`canAddPassenger`).
    fn max_passengers(_m: &MobData) -> usize {
        1
    }
    /// Whether `player`, the first passenger, steers the mob (`getControllingPassenger`).
    fn rider_controls(_ents: &Entities, _player: EntityId, _vehicle: EntityId) -> bool {
        false
    }
    /// `PlayerRideableJumping.handleStartJump`: the rider's jump key (strength 0 to 100).
    fn rider_jump(_ents: &mut Entities, _env: &mut Env<'_>, _vehicle: EntityId, _strength: i32) {}
    /// The breeding food (`isFood`).
    fn is_food(_kind: Kind, _item: u16) -> bool {
        false
    }

    // --- Life cycle -------------------------------------------------------------------

    /// Sets the kind state in [`MobData::ext`] and the navigation kind before the goals.
    fn init(_m: &mut MobData) {}
    /// `registerGoals`.
    fn register_goals(_m: &mut MobData) {}
    /// What the constructor reads of its level: the mob joins a level at this game time and
    /// time of the overworld day.
    fn created(_m: &mut MobData, _game_time: i64, _day_time: i64) {}
    /// `finalizeSpawn` (after `Mob.finalizeSpawn`'s follow range bonus and handedness).
    fn finalize_spawn(_m: &mut MobData, _ctx: &mut SpawnContext<'_>) {}
    /// The draws of the kind's `finalizeSpawn` from the mob's own random (`this.random`;
    /// [`FamilyHooks::finalize_spawn`] gets the level's random as `m.random`).
    fn finalize_spawn_own(_m: &mut MobData) {}
    /// Work before `LivingEntity.tick` (the kind's `tick` override before `super.tick()`).
    fn before_tick(_t: &mut Tick<'_, '_>) {}
    /// Work after `LivingEntity.tick`.
    fn after_tick(_t: &mut Tick<'_, '_>) {}
    /// The kind's `aiStep` override before it calls `super.aiStep()`.
    fn ai_step_start(_t: &mut Tick<'_, '_>) {}
    /// The kind's additions at the end of `aiStep`.
    fn ai_step(_t: &mut Tick<'_, '_>) {}
    /// `customServerAiStep` (runs after the goals and navigation).
    fn custom_server_ai_step(_t: &mut Tick<'_, '_>) {}
    /// `travel(input)` when the kind moves differently; `true` when handled.
    fn travel(_t: &mut Tick<'_, '_>, _input: Vec3) -> bool {
        false
    }
    /// `isPushedByFluid` (water animals, axolotls, turtles, frogs and swimming drowned are
    /// not).
    fn pushed_by_fluid(_m: &MobData) -> bool {
        true
    }
    /// `getWaterSlowDown` (polar bears 0.98, skeleton horses 0.96).
    fn water_slow_down(_kind: Kind) -> f32 {
        0.8
    }
    /// The kind's own `MoveControl.tick` (with [`super::nav::MoveKind::Custom`]); `true` when
    /// handled.
    fn move_control(_t: &mut Tick<'_, '_>) -> bool {
        false
    }
    /// The kind's own `LookControl.tick` (with [`super::nav::LookKind::Custom`]).
    fn look_control(_t: &mut Tick<'_, '_>) -> bool {
        false
    }
    /// The end of `baseTick` (`WaterAnimal.handleAirSupply`).
    fn after_base_tick(_t: &mut Tick<'_, '_>) {}
    /// `thunderHit`: `true` when the kind handles the lightning strike itself.
    fn thunder_hit(_h: &mut Hurt<'_, '_>) -> bool {
        false
    }
    /// `isImmobile` besides being dead (a roaring or stunned ravager): no AI and no input.
    fn is_immobile(_m: &MobData) -> bool {
        false
    }
    /// `causeFallDamage` additions (the fall distance).
    fn on_fall_damage(_t: &mut Tick<'_, '_>, _distance: f64) {}

    // --- Combat -----------------------------------------------------------------------

    /// `doHurtTarget`; `None` for `Mob.doHurtTarget`.
    fn do_hurt_target(_t: &mut Tick<'_, '_>, _target: EntityId) -> Option<bool> {
        None
    }
    /// After a successful `doHurtTarget` (effects on the target, fire).
    fn after_attack(_t: &mut Tick<'_, '_>, _target: EntityId) {}
    /// `performRangedAttack(target, power)` of `RangedAttackGoal` users.
    fn perform_ranged_attack(_t: &mut Tick<'_, '_>, _target: EntityId, _power: f32) {}
    /// `hurtServer` before the damage: `None` cancels the hit, else the amount to apply.
    fn hurt_filter(_h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
        Some(amount)
    }
    /// `getDamageAfterMagicAbsorb` additions (witches resist magic).
    fn absorb(_m: &MobData, _source: &DamageSource, _env: &Env<'_>, damage: f32) -> f32 {
        damage
    }
    /// `actuallyHurt` after the damage, before the knockback (the armadillo rolls up).
    fn actually_hurt(_h: &mut Hurt<'_, '_>) {}
    /// After `hurtServer` hurt the mob (calls for help, teleports).
    fn after_hurt(_h: &mut Hurt<'_, '_>, _amount: f32) {}
    /// `die` before the loot (splitting, conversions, extra drops).
    fn on_die(_h: &mut Hurt<'_, '_>) {}

    // --- Players ----------------------------------------------------------------------

    /// `mobInteract` (after name tags, spawn eggs and leads).
    fn interact(_ix: &mut Interact<'_, '_>) -> Interaction {
        Interaction::Pass
    }
    /// `Animal.canMate(partner)`: the default is the same kind, both in love.
    fn can_mate(me: &MobData, partner: &MobData) -> bool {
        me.kind == partner.kind && me.in_love > 0 && partner.in_love > 0
    }
    /// The kind of the bred baby (`getBreedOffspring`'s type; a mule of a horse and a donkey).
    fn offspring_kind(parent: &MobData, _partner: &MobData) -> Kind {
        parent.kind
    }
    /// Whether a player that is the first passenger steers the mob (`getControllingPassenger`
    /// is the player: a saddled horse); its client then moves it.
    fn rider_controls_mob(_m: &MobData) -> bool {
        false
    }
    /// `getBreedOffspring` setup of a bred baby (variants, colours, genes).
    fn breed_offspring(
        _parent: &MobData,
        _partner: &MobData,
        _child: &mut MobData,
        _random: &mut LegacyRandom,
    ) {
    }

    // --- Client data and storage ------------------------------------------------------

    /// The kind's synched data (ids 16 and up).
    fn encode_data(_w: &mut DataWriter<'_>, _m: &MobData) {}
    /// `Mob.aiStep`'s looting (`canPickUpLoot`, `wantsToPickUp`, `pickUpItem`).
    fn pick_up_items(t: &mut Tick<'_, '_>) {
        let rules = super::equipment::PickupRules::default();
        super::equipment::pick_up_items(t, &rules, None, (1.0, 0.0, 1.0));
    }
    /// `wantsToPickUp(stack)` (the item sensor of brain mobs).
    fn wants_to_pick_up(_t: &Tick<'_, '_>, _stack: &ItemStack) -> bool {
        true
    }
    /// The living entity flags (using an item, hand).
    fn living_flags(m: &MobData) -> u8 {
        u8::from(m.using_item >= 0)
    }
    /// `addAdditionalSaveData` of the kind.
    fn save(_m: &MobData, _out: &mut Compound) {}
    /// `readAdditionalSaveData` of the kind.
    fn load(_m: &mut MobData, _tag: &Compound) {}

    // --- Projectiles (the missile kinds of the family, `MissileKind::family`) ----------

    /// One tick of a projectile of the family (`Projectile.tick`).
    fn missile_tick(_t: &mut super::missile::MissileTick<'_, '_>) {}
    /// The projectile's own NBT.
    fn missile_save(_d: &super::missile::MissileData, _out: &mut Compound) {}
    /// Reads the projectile's own NBT.
    fn missile_load(_d: &mut super::missile::MissileData, _tag: &Compound) {}
    /// The projectile's synched data (ids 8 and up).
    fn missile_encode(_w: &mut DataWriter<'_>, _d: &super::missile::MissileData) {}

    // --- Goals ------------------------------------------------------------------------

    fn goal_flags(_g: &Self::Goal) -> u8 {
        0
    }
    fn goal_every_tick(_g: &Self::Goal) -> bool {
        false
    }
    fn goal_can_use(_t: &mut Tick<'_, '_>, _g: &mut Self::Goal) -> bool {
        false
    }
    fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut Self::Goal) -> bool {
        Self::goal_can_use(t, g)
    }
    fn goal_start(_t: &mut Tick<'_, '_>, _g: &mut Self::Goal) {}
    fn goal_stop(_t: &mut Tick<'_, '_>, _g: &mut Self::Goal) {}
    fn goal_tick(_t: &mut Tick<'_, '_>, _g: &mut Self::Goal) {}
}

/// The kind state of a family mob.
#[derive(Clone, Debug, Default)]
pub enum Ext {
    #[default]
    None,
    /// A classic kind with state of its own (creeper, sheep, chicken, enderman).
    Classic(classic::State),
    Undead(undead::State),
    Illager(illager::State),
    Brainy(brainy::State),
    Mount(mount::State),
    /// Boxed: the records are large and most mobs are of other families.
    Companion(Box<companion::State>),
    Wildlife(Box<wildlife::State>),
    /// Armor stands and mannequins.
    Decor(Box<crate::decoration::stand::State>),
    Village(Box<village::State>),
}

/// A goal of a family.
#[derive(Clone, Copy, Debug)]
pub enum FamGoal {
    Undead(undead::Goal),
    Illager(illager::Goal),
    Brainy(brainy::Goal),
    Mount(mount::Goal),
    Companion(companion::Goal),
    Wildlife(wildlife::Goal),
    Village(village::Goal),
}

/// Calls a hook of the kind's family; `$classic` for the classic kinds.
macro_rules! by_family {
    ($kind:expr, $classic:expr, $f:ident($($arg:expr),*)) => {
        match $kind.family() {
            Family::Classic => $classic,
            Family::Undead => <undead::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Illager => <illager::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Brainy => <brainy::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Mount => <mount::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Companion => <companion::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Wildlife => <wildlife::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Boss => <boss::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Decor => <decor::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Village => <village::Hooks as FamilyHooks>::$f($($arg),*),
        }
    };
}

/// Calls a goal hook of the family the goal belongs to.
macro_rules! by_goal {
    ($g:expr, $f:ident($($pre:expr,)* @g $(, $post:expr)*)) => {
        match $g {
            FamGoal::Undead(g) => <undead::Hooks as FamilyHooks>::$f($($pre,)* g $(, $post)*),
            FamGoal::Illager(g) => <illager::Hooks as FamilyHooks>::$f($($pre,)* g $(, $post)*),
            FamGoal::Brainy(g) => <brainy::Hooks as FamilyHooks>::$f($($pre,)* g $(, $post)*),
            FamGoal::Mount(g) => <mount::Hooks as FamilyHooks>::$f($($pre,)* g $(, $post)*),
            FamGoal::Companion(g) => {
                <companion::Hooks as FamilyHooks>::$f($($pre,)* g $(, $post)*)
            }
            FamGoal::Wildlife(g) => <wildlife::Hooks as FamilyHooks>::$f($($pre,)* g $(, $post)*),
            FamGoal::Village(g) => <village::Hooks as FamilyHooks>::$f($($pre,)* g $(, $post)*),
        }
    };
}

// --- Dispatch (the classic kinds never reach these with `Family::Classic` except where a
// --- classic default is given) ----------------------------------------------------------

pub fn is_monster(kind: Kind) -> bool {
    by_family!(kind, false, is_monster(kind))
}
pub fn ageable(kind: Kind) -> bool {
    by_family!(kind, false, ageable(kind))
}
pub fn ambient_sound_interval(kind: Kind) -> i32 {
    by_family!(kind, 80, ambient_sound_interval(kind))
}
pub fn burns_in_daylight(kind: Kind) -> bool {
    by_family!(kind, false, burns_in_daylight(kind))
}
pub fn breathes_underwater(kind: Kind) -> bool {
    by_family!(kind, false, breathes_underwater(kind))
}
pub fn sensitive_to_water(kind: Kind) -> bool {
    by_family!(kind, false, sensitive_to_water(kind))
}
pub fn fall_damage_immune(kind: Kind) -> bool {
    by_family!(kind, false, fall_damage_immune(kind))
}
pub fn allowed_in_peaceful(kind: Kind) -> bool {
    by_family!(kind, true, allowed_in_peaceful(kind))
}
pub fn remove_when_far_away(kind: Kind) -> bool {
    by_family!(kind, false, remove_when_far_away(kind))
}
pub fn remove_when_far_away_mob(m: &MobData, dist_sqr: f64) -> bool {
    by_family!(
        m.kind,
        if m.kind == Kind::Chicken {
            classic::chicken(m).jockey
        } else {
            m.kind.remove_when_far_away()
        },
        remove_when_far_away_mob(m, dist_sqr)
    )
}
pub fn requires_custom_persistence(m: &MobData) -> bool {
    // `EnderMan.requiresCustomPersistence`: while it carries a block.
    if m.kind == Kind::Enderman {
        return classic::enderman(m).carried.is_some();
    }
    by_family!(m.kind, false, requires_custom_persistence(m))
}
pub fn malus(kind: Kind) -> &'static MalusTable {
    by_family!(kind, &[], malus(kind))
}
pub fn sound_source(kind: Kind) -> SoundSource {
    by_family!(kind, SoundSource::Neutral, sound_source(kind))
}
pub fn sound_volume(kind: Kind) -> f32 {
    by_family!(kind, 1.0, sound_volume(kind))
}
pub fn mob_sound_volume(m: &MobData) -> f32 {
    by_family!(m.kind, m.kind.sound_volume(), mob_sound_volume(m))
}
pub fn jump_power_bonus(m: &MobData) -> f32 {
    by_family!(m.kind, 0.0, jump_power_bonus(m))
}
pub fn jump_in_liquid(t: &mut Tick<'_, '_>) -> bool {
    by_family!(t.mob_ref().kind, false, jump_in_liquid(t))
}
pub fn ambient_sound(m: &mut MobData) -> Option<&'static str> {
    by_family!(m.kind, m.kind.ambient_sound(), ambient_sound(m))
}
pub fn hurt_sound(m: &MobData) -> &'static str {
    by_family!(m.kind, m.kind.hurt_sound(), hurt_sound(m))
}
pub fn death_sound(m: &MobData) -> &'static str {
    by_family!(m.kind, m.kind.death_sound(), death_sound(m))
}
pub fn experience_reward(m: &mut MobData) -> i32 {
    let kind = m.kind;
    by_family!(kind, 0, experience_reward(m))
}
pub fn dimensions(m: &MobData) -> Option<Dimensions> {
    by_family!(m.kind, None, dimensions(m))
}
pub fn size_scale(m: &MobData) -> Option<f32> {
    by_family!(m.kind, None, size_scale(m))
}
pub fn walk_target_value(kind: Kind, level: &Level, env: &Environment, pos: BlockPos) -> f64 {
    by_family!(
        kind,
        super::kinds::base_walk_target_value(kind.is_monster(), level, env, pos),
        walk_target_value(kind, level, env, pos)
    )
}
pub fn check_spawn_rules(
    kind: Kind,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
    random: &mut LegacyRandom,
    reason: SpawnReason,
) -> bool {
    by_family!(
        kind,
        super::spawn::default_spawn_rules(kind, level, env, pos, random, reason),
        check_spawn_rules(kind, level, env, pos, random, reason)
    )
}
pub fn anger_range(kind: Kind) -> (i32, i32) {
    by_family!(kind, (20 * 20, 39 * 20), anger_range(kind))
}
pub fn is_max_group_size_reached(m: &MobData, group_size: i32) -> bool {
    by_family!(m.kind, false, is_max_group_size_reached(m, group_size))
}
pub fn max_passengers(m: &MobData) -> usize {
    by_family!(m.kind, 1, max_passengers(m))
}
pub fn rider_controls(ents: &Entities, player: EntityId, vehicle: EntityId) -> bool {
    let Some(kind) = ents.mob(vehicle).map(|m| m.kind) else {
        return false;
    };
    by_family!(kind, false, rider_controls(ents, player, vehicle))
}
pub fn rider_jump(ents: &mut Entities, env: &mut Env<'_>, vehicle: EntityId, strength: i32) {
    let Some(kind) = ents.mob(vehicle).map(|m| m.kind) else {
        return;
    };
    by_family!(kind, (), rider_jump(ents, env, vehicle, strength));
}
pub fn is_food(kind: Kind, item: u16) -> bool {
    by_family!(kind, false, is_food(kind, item))
}
pub fn init(m: &mut MobData) {
    let kind = m.kind;
    by_family!(kind, (), init(m));
}
pub fn register_goals(m: &mut MobData) {
    let kind = m.kind;
    by_family!(kind, (), register_goals(m));
}
pub fn created(m: &mut MobData, game_time: i64, day_time: i64) {
    let kind = m.kind;
    by_family!(kind, (), created(m, game_time, day_time));
}
/// Whether the kind's `finalizeSpawn` makes its own draws before `super.finalizeSpawn`, so
/// `Mob.finalizeSpawn` (follow range, left-handedness) comes last: the horses
/// (`AbstractHorse.finalizeSpawn`: `randomizeAttributes`, then `AgeableMob`, then `Mob`).
pub fn mob_spawn_part_last(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Horse
            | Kind::Donkey
            | Kind::Mule
            | Kind::Llama
            | Kind::TraderLlama
            | Kind::SkeletonHorse
            | Kind::ZombieHorse
            | Kind::Camel
            | Kind::CamelHusk
            | Kind::Wolf
            | Kind::Vex
            | Kind::Axolotl
            | Kind::Fox
            | Kind::Panda
    )
}

pub fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
    let kind = m.kind;
    by_family!(kind, (), finalize_spawn(m, ctx));
}
pub fn finalize_spawn_own(m: &mut MobData) {
    let kind = m.kind;
    by_family!(kind, (), finalize_spawn_own(m));
}
pub fn before_tick(t: &mut Tick<'_, '_>) {
    by_family!(t.mob_ref().kind, (), before_tick(t));
}
pub fn after_tick(t: &mut Tick<'_, '_>) {
    by_family!(t.mob_ref().kind, (), after_tick(t));
}
pub fn ai_step_start(t: &mut Tick<'_, '_>) {
    by_family!(t.mob_ref().kind, (), ai_step_start(t));
}
pub fn ai_step(t: &mut Tick<'_, '_>) {
    by_family!(t.mob_ref().kind, (), ai_step(t));
}
pub fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
    by_family!(t.mob_ref().kind, (), custom_server_ai_step(t));
}
pub fn travel(t: &mut Tick<'_, '_>, input: Vec3) -> bool {
    by_family!(t.mob_ref().kind, false, travel(t, input))
}
/// `isPushedByFluid`: water animals, axolotls, frogs, turtles and nautiluses swim against
/// the current; the families can refine it.
pub fn pushed_by_fluid(m: &MobData) -> bool {
    if matches!(
        m.kind,
        Kind::Cod
            | Kind::Salmon
            | Kind::Pufferfish
            | Kind::TropicalFish
            | Kind::Squid
            | Kind::GlowSquid
            | Kind::Dolphin
            | Kind::Axolotl
            | Kind::Fox
            | Kind::Frog
            | Kind::Tadpole
            | Kind::Turtle
            | Kind::Nautilus
            | Kind::ZombieNautilus
    ) {
        return false;
    }
    by_family!(m.kind, true, pushed_by_fluid(m))
}
/// `getWaterSlowDown`: 0.8, skeleton horses 0.96, polar bears 0.98.
pub fn water_slow_down(kind: Kind) -> f32 {
    match kind {
        Kind::SkeletonHorse => 0.96,
        Kind::PolarBear => 0.98,
        _ => by_family!(kind, 0.8, water_slow_down(kind)),
    }
}
pub fn move_control(t: &mut Tick<'_, '_>) -> bool {
    by_family!(t.mob_ref().kind, false, move_control(t))
}
pub fn look_control(t: &mut Tick<'_, '_>) -> bool {
    by_family!(t.mob_ref().kind, false, look_control(t))
}
pub fn after_base_tick(t: &mut Tick<'_, '_>) {
    by_family!(t.mob_ref().kind, (), after_base_tick(t));
}
pub fn thunder_hit(h: &mut Hurt<'_, '_>, kind: Kind) -> bool {
    by_family!(kind, false, thunder_hit(h))
}
pub fn is_immobile(m: &MobData) -> bool {
    by_family!(m.kind, false, is_immobile(m))
}
pub fn on_fall_damage(t: &mut Tick<'_, '_>, distance: f64) {
    by_family!(t.mob_ref().kind, (), on_fall_damage(t, distance));
}
pub fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
    by_family!(t.mob_ref().kind, None, do_hurt_target(t, target))
}
pub fn after_attack(t: &mut Tick<'_, '_>, target: EntityId) {
    by_family!(t.mob_ref().kind, (), after_attack(t, target));
}
pub fn perform_ranged_attack(t: &mut Tick<'_, '_>, target: EntityId, power: f32) {
    by_family!(
        t.mob_ref().kind,
        (),
        perform_ranged_attack(t, target, power)
    );
}
pub fn hurt_filter(h: &mut Hurt<'_, '_>, kind: Kind, amount: f32) -> Option<f32> {
    by_family!(kind, Some(amount), hurt_filter(h, amount))
}
pub fn absorb(m: &MobData, source: &DamageSource, env: &Env<'_>, damage: f32) -> f32 {
    by_family!(m.kind, damage, absorb(m, source, env, damage))
}
pub fn actually_hurt(h: &mut Hurt<'_, '_>, kind: Kind) {
    by_family!(kind, (), actually_hurt(h));
}
pub fn after_hurt(h: &mut Hurt<'_, '_>, kind: Kind, amount: f32) {
    by_family!(kind, (), after_hurt(h, amount));
}
pub fn on_die(h: &mut Hurt<'_, '_>, kind: Kind) {
    by_family!(kind, (), on_die(h));
}
pub fn interact(ix: &mut Interact<'_, '_>, kind: Kind) -> Interaction {
    by_family!(kind, Interaction::Pass, interact(ix))
}
pub fn can_mate(me: &MobData, partner: &MobData) -> bool {
    by_family!(me.kind, me.kind == partner.kind, can_mate(me, partner))
}
pub fn offspring_kind(parent: &MobData, partner: &MobData) -> Kind {
    by_family!(parent.kind, parent.kind, offspring_kind(parent, partner))
}
pub fn rider_controls_mob(m: &MobData) -> bool {
    by_family!(m.kind, false, rider_controls_mob(m))
}
pub fn breed_offspring(
    parent: &MobData,
    partner: &MobData,
    child: &mut MobData,
    random: &mut LegacyRandom,
) {
    by_family!(
        parent.kind,
        (),
        breed_offspring(parent, partner, child, random)
    );
}
pub fn encode_data(w: &mut DataWriter<'_>, m: &MobData) {
    by_family!(m.kind, (), encode_data(w, m));
}
pub fn pick_up_items(t: &mut Tick<'_, '_>) {
    by_family!(t.mob_ref().kind, (), pick_up_items(t));
}
pub fn wants_to_pick_up(t: &Tick<'_, '_>, stack: &ItemStack) -> bool {
    by_family!(t.mob_ref().kind, true, wants_to_pick_up(t, stack))
}
pub fn living_flags(m: &MobData) -> u8 {
    by_family!(m.kind, u8::from(m.using_item >= 0), living_flags(m))
}
pub fn save(m: &MobData, out: &mut Compound) {
    by_family!(m.kind, (), save(m, out));
}
pub fn load(m: &mut MobData, tag: &Compound) {
    let kind = m.kind;
    by_family!(kind, (), load(m, tag));
}

/// Calls a projectile hook of the family that owns the missile kind.
macro_rules! by_missile {
    ($kind:expr, $f:ident($($arg:expr),*)) => {
        match $kind.family() {
            Family::Classic => {}
            Family::Undead => <undead::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Illager => <illager::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Brainy => <brainy::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Mount => <mount::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Companion => <companion::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Wildlife => <wildlife::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Boss => <boss::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Decor => <decor::Hooks as FamilyHooks>::$f($($arg),*),
            Family::Village => <village::Hooks as FamilyHooks>::$f($($arg),*),
        }
    };
}

pub fn missile_tick(
    t: &mut super::missile::MissileTick<'_, '_>,
    kind: super::missile::MissileKind,
) {
    by_missile!(kind, missile_tick(t));
}
pub fn missile_save(d: &super::missile::MissileData, out: &mut Compound) {
    by_missile!(d.kind, missile_save(d, out));
}
pub fn missile_load(d: &mut super::missile::MissileData, tag: &Compound) {
    let kind = d.kind;
    by_missile!(kind, missile_load(d, tag));
}
pub fn missile_encode(w: &mut DataWriter<'_>, d: &super::missile::MissileData) {
    by_missile!(d.kind, missile_encode(w, d));
}

pub fn goal_flags(g: &FamGoal) -> u8 {
    by_goal!(g, goal_flags(@g))
}
pub fn goal_every_tick(g: &FamGoal) -> bool {
    by_goal!(g, goal_every_tick(@g))
}
pub fn goal_can_use(t: &mut Tick<'_, '_>, g: &mut FamGoal) -> bool {
    by_goal!(g, goal_can_use(t, @g))
}
pub fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut FamGoal) -> bool {
    by_goal!(g, goal_can_continue(t, @g))
}
pub fn goal_start(t: &mut Tick<'_, '_>, g: &mut FamGoal) {
    by_goal!(g, goal_start(t, @g));
}
pub fn goal_stop(t: &mut Tick<'_, '_>, g: &mut FamGoal) {
    by_goal!(g, goal_stop(t, @g));
}
pub fn goal_tick(t: &mut Tick<'_, '_>, g: &mut FamGoal) {
    by_goal!(g, goal_tick(t, @g));
}

/// `LivingEntity.readAdditionalSaveData`: a saved `Brain` makes a new brain
/// (`makeBrain(packed)`): sensors with new random delays from the mob's random, every
/// behaviour stopped, only the saved memories.
pub fn reload_brain(m: &mut MobData, tag: &Compound) {
    if m.brain.is_none() || tag.get("Brain").is_none() {
        return;
    }
    match m.kind {
        Kind::Camel | Kind::CamelHusk => mount::ai::camel_brain(m),
        Kind::HappyGhast => mount::ai::happy_ghast_brain(m),
        Kind::Nautilus | Kind::ZombieNautilus => mount::ai::nautilus_brain(m),
        Kind::Allay => mount::ai::allay_brain(m),
        Kind::CopperGolem => mount::ai::copper_golem_brain(m),
        Kind::Breeze => mount::ai::breeze_brain(m),
        Kind::Creaking => mount::ai::creaking_brain(m),
        Kind::Warden => crate::mob::boss::warden::ai::make_brain(m),
        Kind::Villager => {
            m.brain = None;
            village::villager::rebuild_brain(m, None);
        }
        _ => brainy::make_brain(m),
    }
    if let Some(b) = m.brain.as_deref_mut() {
        super::brain::load(b, tag);
    }
}

/// `Bucketable.saveDefaultDataToBucketTag`: the bucket `item` with the mob's custom name, and
/// the default `bucket_entity_data` the kind adds to before [`set_bucket_data`].
pub(crate) fn bucket_default(
    m: &MobData,
    flags: vanillite_world::entity::EntityFlags,
    item: &str,
) -> (ItemStack, Compound) {
    use vanillite_world::entity::EntityFlags;
    let mut stack = super::equipment::stack(item);
    if let Some(name) = m.custom_name.as_deref() {
        stack.set_component(
            vanillite_protocol::components::ids::CUSTOM_NAME,
            name.into(),
        );
    }
    let mut data = Compound::new();
    if m.no_ai {
        data.insert("NoAI", true);
    }
    if m.silent {
        data.insert("Silent", true);
    }
    if flags.contains(EntityFlags::NO_GRAVITY) {
        data.insert("NoGravity", true);
    }
    if flags.contains(EntityFlags::GLOWING_TAG) {
        data.insert("Glowing", true);
    }
    if flags.contains(EntityFlags::INVULNERABLE) {
        data.insert("Invulnerable", true);
    }
    if m.persistent {
        data.insert("PersistenceRequired", true);
    }
    data.insert("Health", m.health);
    (stack, data)
}

/// The `bucket_entity_data` component of a filled bucket.
pub(crate) fn set_bucket_data(stack: &mut ItemStack, data: Compound) {
    if let (Some(id), Some(bytes)) = (
        vanillite_data::registry::DATA_COMPONENT_TYPE.id("minecraft:bucket_entity_data"),
        crate::mob::nbt::tag_component(&vanillite_nbt::Tag::Compound(data)),
    ) {
        stack.set_component(id, bytes);
    }
}

/// `loadDefaultDataFromBucketTag`'s entity flags (`NoGravity`, `Glowing`, `Invulnerable`) of
/// the bucket's `bucket_entity_data` on the spawned mob.
pub fn load_bucket_flags(ents: &mut crate::entities::Entities, id: EntityId, stack: &ItemStack) {
    use vanillite_world::entity::EntityFlags;
    let Some(vanillite_nbt::Tag::Compound(tag)) = vanillite_data::registry::DATA_COMPONENT_TYPE
        .id("minecraft:bucket_entity_data")
        .and_then(|c| stack.component(c))
        .and_then(crate::mob::nbt::component_tag)
    else {
        return;
    };
    let glowing_effect = ents
        .mob(id)
        .is_some_and(|m| m.effects.has(crate::effect::MobEffect::GLOWING));
    let Some(mut e) = ents.store.get_mut(id) else {
        return;
    };
    let before = e.as_ref().flags();
    for (key, flag) in [
        ("NoGravity", EntityFlags::NO_GRAVITY),
        ("Glowing", EntityFlags::GLOWING_TAG),
        ("Invulnerable", EntityFlags::INVULNERABLE),
    ] {
        if let Some(v) = crate::mob::entity_nbt::get_bool(&tag, key) {
            e.set_flags(flag, v);
        }
    }
    let tagged = e.as_ref().flags().contains(EntityFlags::GLOWING_TAG);
    e.set_flags(EntityFlags::GLOWING, tagged || glowing_effect);
    if e.as_ref().flags() != before {
        e.mark_data_dirty();
    }
}

/// `MobBucketItem.spawn` after the mob is created: `Bucketable.loadFromBucketTag` of the
/// bucket's `bucket_entity_data` and `setFromBucket(true)` (its components are applied by
/// [`crate::mob::item_components::apply_spawn_item`]); the entity flags follow with
/// [`load_bucket_flags`].
pub fn load_from_bucket(m: &mut MobData, stack: &crate::item::ItemStack) {
    let tag = vanillite_data::registry::DATA_COMPONENT_TYPE
        .id("minecraft:bucket_entity_data")
        .and_then(|c| stack.component(c))
        .and_then(crate::mob::nbt::component_tag);
    let empty = Compound::new();
    let tag = match &tag {
        Some(vanillite_nbt::Tag::Compound(c)) => c,
        _ => &empty,
    };
    let flag = |key: &str| match tag.get(key) {
        Some(vanillite_nbt::Tag::Byte(v)) => Some(*v != 0),
        _ => None,
    };
    if let Some(v) = flag("NoAI") {
        m.no_ai = v;
    }
    if let Some(v) = flag("Silent") {
        m.silent = v;
    }
    if flag("PersistenceRequired") == Some(true) {
        m.persistent = true;
    }
    if let Some(vanillite_nbt::Tag::Float(h)) = tag.get("Health") {
        m.health = *h;
    }
    match m.kind {
        Kind::Cod | Kind::Salmon | Kind::Pufferfish | Kind::TropicalFish => {
            wildlife::fish::from_bucket(m);
        }
        Kind::Axolotl => {
            if let Some(vanillite_nbt::Tag::Int(age)) = tag.get("Age") {
                m.age = *age;
                m.baby = m.age < 0;
            }
            m.age_locked = flag("AgeLocked").unwrap_or(false);
            if let Some(b) = m.brain.as_deref_mut() {
                let cooldown = super::brain::MemoryType::HasHuntingCooldown;
                match tag
                    .get("HuntingCooldown")
                    .and_then(vanillite_nbt::Tag::as_i64)
                {
                    Some(t) => b.set_with_expiry(cooldown, super::brain::MemValue::Bool(true), t),
                    None => b.erase(cooldown),
                }
            }
            brainy::state_mut(m).from_bucket = true;
        }
        Kind::Tadpole => {
            let s = brainy::state_mut(m);
            if let Some(vanillite_nbt::Tag::Int(age)) = tag.get("Age") {
                s.tadpole_age = *age;
            }
            s.from_bucket = true;
            m.age_locked = flag("AgeLocked").unwrap_or(false);
        }
        Kind::SulfurCube => {
            let age = match tag.get("age") {
                Some(vanillite_nbt::Tag::Int(age)) => *age,
                _ => 0,
            };
            undead::cubes::load_from_bucket(m, age, flag("age_locked").unwrap_or(false));
        }
        _ => {}
    }
}
