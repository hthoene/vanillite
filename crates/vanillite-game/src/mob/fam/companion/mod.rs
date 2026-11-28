//! Horses (horse, donkey, mule, skeleton and zombie horse), llamas, trader llamas, wolves,
//! cats, ocelots, parrots, foxes, pandas, polar bears and rabbits: the `AbstractHorse`,
//! `TamableAnimal` and the other `Animal` subclasses of the companion family.
//!
//! The kind state lives in [`State`] (one struct for the family: small integers and flags),
//! the variant of each kind in [`MobData::variant`] and the owner in [`MobData::owner`].

pub mod feline;
pub mod fox;
pub mod horse;
pub mod llama;
pub mod panda;
pub mod parrot;
pub mod polar_bear;
pub mod rabbit;
pub mod wolf;

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_nbt::{Compound, Tag};
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{Dimensions, EntityId};

use super::{FamilyHooks, Hurt, Interact, SpawnContext, SpawnReason};
use crate::item::{self, ItemStack};
use crate::level::Level;
use crate::mob::animal::Interaction;
use crate::mob::common::CommonGoal;
use crate::mob::goals::{self, Goal as BaseGoal};
use crate::mob::kinds::{DefaultSound, Kind};
use crate::mob::missile::{MissileData, MissileTick};
use crate::mob::path::{MalusTable, PathType};
use crate::mob::{Environment, MobData, Tick};
use crate::player::encode::{DataWriter, serializer};
use crate::survival::damage::DamageSource;

/// The family's hooks.
#[derive(Debug)]
pub struct Hooks;

/// The kind state of the companion mobs (in [`super::Ext::Companion`]).
#[derive(Clone, Debug, Default)]
pub struct State {
    // --- `AbstractHorse` / `AbstractChestedHorse` / `Llama` ---
    /// `DATA_ID_FLAGS` (2 tamed, 8 bred, 16 eating, 32 standing, 64 mouth open).
    pub horse_flags: u8,
    pub temper: i32,
    pub eating_counter: i32,
    pub mouth_counter: i32,
    pub stand_counter: i32,
    /// `standAnim` and `standAnimO`: the rearing animation, which lifts and moves the rider.
    pub stand_anim: f32,
    pub stand_anim_o: f32,
    pub sprint_counter: i32,
    /// `AbstractHorse.gallopSoundCounter`.
    pub gallop_counter: i32,
    /// `allowStandSliding`.
    pub stand_sliding: bool,
    /// `AbstractChestedHorse.hasChest`.
    pub chest: bool,
    /// The mount inventory (`inventory`, without the equipment slots).
    pub inventory: Vec<ItemStack>,
    /// `Llama.strength` (1–5).
    pub strength: i32,
    pub did_spit: bool,
    pub caravan_head: Option<EntityId>,
    pub caravan_tail: Option<EntityId>,
    /// `SkeletonHorse.isTrap` and `trapTime`.
    pub trap: bool,
    pub trap_time: i32,
    /// `TraderLlama.despawnDelay`.
    pub despawn_delay: i32,
    // --- Tamable animals ---
    /// `DATA_COLLAR_COLOR` (dye id; red = 14).
    pub collar: u8,
    /// Index into the `*_sound_variant` registry.
    pub sound_variant: u8,
    /// `Wolf.isInterested` (begging).
    pub interested: bool,
    pub wet: bool,
    pub shaking: bool,
    pub shake_anim: f32,
    /// `Cat.isLying`, `isRelaxStateOne`.
    pub lying: bool,
    pub relax_one: bool,
    /// `Ocelot.isTrusting`.
    pub trusting: bool,
    // --- Fox ---
    /// `DATA_FLAGS_ID` of the fox (1 sitting, 4 crouching, 8 interested, 16 pouncing,
    /// 32 sleeping, 64 faceplanted, 128 defending).
    pub fox_flags: u8,
    pub trusted: [Option<u128>; 2],
    pub ticks_since_eaten: i32,
    /// `crouchAmount`: grows by 0.2 a tick while crouching, up to 5 (`isFullyCrouched`).
    pub crouch_amount: f32,
    // --- Panda ---
    pub main_gene: u8,
    pub hidden_gene: u8,
    /// `DATA_ID_FLAGS` (2 sneezing, 4 rolling, 8 sitting, 16 on back).
    pub panda_flags: u8,
    pub unhappy: i32,
    pub sneeze: i32,
    pub eat: i32,
    pub roll_counter: i32,
    /// `rollDelta` (x, z).
    pub roll_delta: (f64, f64),
    // --- Polar bear ---
    pub standing: bool,
    pub warning_sound_ticks: i32,
    // --- Rabbit ---
    pub more_carrot_ticks: i32,
    pub jump_ticks: i32,
    pub jump_duration: i32,
    pub jump_delay_ticks: i32,
    pub was_on_ground: bool,
    // --- Parrot ---
    pub flap: f32,
}

/// The kind state of a companion mob (a default when it has none yet).
pub fn st(m: &MobData) -> &State {
    static EMPTY: std::sync::OnceLock<State> = std::sync::OnceLock::new();
    match &m.ext {
        super::Ext::Companion(s) => s,
        _ => EMPTY.get_or_init(State::default),
    }
}

/// The kind state of a companion mob, mutably.
pub fn st_mut(m: &mut MobData) -> &mut State {
    if !matches!(m.ext, super::Ext::Companion(_)) {
        m.ext = super::Ext::Companion(Box::default());
    }
    match &mut m.ext {
        super::Ext::Companion(s) => s,
        _ => unreachable!("set above"),
    }
}

/// A guard on a shared goal (the subclasses that only add a condition).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cond {
    /// `NonTameRandomTargetGoal`: only while not tamed.
    NotTame,
    /// Only while not trusting (ocelots avoid players).
    NotTrusting,
    /// Only adults.
    NotBaby,
}

/// The family's goals.
#[derive(Clone, Copy, Debug)]
pub enum Goal {
    /// Never runs.
    Idle,
    /// A shared goal behind a condition.
    Guard(Cond, CommonGoal),
    /// `TamableAnimal.TamableAnimalPanicGoal` (and the plain `PanicGoal` with the
    /// environmental causes): `env_only` panics only from fire, lava, freezing and the like.
    TamePanic {
        speed: f64,
        env_only: bool,
        pos: Vec3,
    },
    Horse(horse::Goal),
    Llama(llama::Goal),
    Wolf(wolf::Goal),
    Feline(feline::Goal),
    Parrot(parrot::Goal),
    Fox(fox::Goal),
    Panda(panda::Goal),
    Bear(polar_bear::Goal),
    Rabbit(rabbit::Goal),
    /// `ClimbOnTopOfPowderSnowGoal` (rabbits and foxes).
    ClimbPowderSnow,
}

/// A family goal as a selector goal.
pub fn fam(g: Goal) -> BaseGoal {
    BaseGoal::Fam(super::FamGoal::Companion(g))
}

/// A shared goal as a selector goal.
pub fn common(g: CommonGoal) -> BaseGoal {
    BaseGoal::Common(g)
}

/// A guarded shared goal as a selector goal.
pub fn guard(c: Cond, g: CommonGoal) -> BaseGoal {
    fam(Goal::Guard(c, g))
}

fn cond_holds(t: &Tick<'_, '_>, c: Cond) -> bool {
    let m = t.mob_ref();
    match c {
        Cond::NotTame => !m.tame,
        Cond::NotTrusting => !st(m).trusting,
        Cond::NotBaby => !m.baby,
    }
}

// --- Shared helpers ---------------------------------------------------------------------

/// Whether the kind is a `TamableAnimal`.
pub fn is_tamable(kind: Kind) -> bool {
    matches!(kind, Kind::Wolf | Kind::Cat | Kind::Parrot)
}

/// Whether the kind is an `AbstractHorse`.
pub fn is_horse(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Horse
            | Kind::Donkey
            | Kind::Mule
            | Kind::SkeletonHorse
            | Kind::ZombieHorse
            | Kind::Llama
            | Kind::TraderLlama
    )
}

/// The registry name of a stack's item (`minecraft:bone`).
pub fn item_name(stack: &ItemStack) -> &'static str {
    if stack.is_empty() {
        return "minecraft:air";
    }
    item::item_id(stack).name()
}

/// `stack.is(tag)` for an item tag.
pub fn in_tag(stack: &ItemStack, tag: &str) -> bool {
    !stack.is_empty()
        && crate::player::play::game_data().is_some_and(|d| d.item_in_tag(stack.item, tag))
}

/// The `DyeColor` id of a stack's `dye` component (`DataComponents.DYE`, patched or the
/// item's default).
pub fn dye_color(stack: &ItemStack) -> Option<u8> {
    crate::menu::station::dye_color(stack).and_then(|i| u8::try_from(i).ok())
}

/// `DyeColor.getMixedColor`: the dye the two colours craft into, else one of them.
pub fn mixed_color(a: u8, b: u8, random: &mut LegacyRandom) -> u8 {
    let (lo, hi) = (a.min(b), a.max(b));
    let mixed = match (lo, hi) {
        (0, 15) => Some(7),
        (0, 7) => Some(8),
        (0, 14) => Some(6),
        (0, 11) => Some(3),
        (0, 13) => Some(5),
        (4, 14) => Some(1),
        (6, 10) => Some(2),
        (11, 14) => Some(10),
        (11, 13) => Some(9),
        _ => None,
    };
    mixed.unwrap_or(if random.next_boolean() { a } else { b })
}

/// Plays a sound of a mob outside its tick with explicit volume and pitch.
pub fn sound(ix: &mut Interact<'_, '_>, sound: &str, volume: f32, pitch: f32) {
    let Some(pos) = ix.ents.store.get(ix.target).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = ix.ents.mob_mut(ix.target) else {
        return;
    };
    if m.silent {
        return;
    }
    let source = m.sound_source();
    let seed = crate::survival::sound_seed();
    crate::survival::play_sound(ix.env.out, None, pos, sound, source, volume, pitch, seed);
}

/// `heal(amount)`.
pub fn heal(m: &mut MobData, amount: f32) {
    if m.health > 0.0 {
        m.health = (m.health + amount).min(m.max_health());
    }
}

/// Marks the target's synched data dirty (after an interaction).
pub fn dirty(ix: &mut Interact<'_, '_>) {
    if let Some(mut e) = ix.ents.store.get_mut(ix.target) {
        e.mark_data_dirty();
    }
}

/// Whether the interacting player owns the target.
pub fn owned_by(ix: &Interact<'_, '_>) -> bool {
    let uuid = ix.ents.store.get(ix.player).map(|e| e.uuid());
    ix.ents
        .mob(ix.target)
        .is_some_and(|m| m.owner.is_some() && m.owner == uuid)
}

/// The UUID of the interacting player.
pub fn player_uuid(ix: &Interact<'_, '_>) -> Option<u128> {
    ix.ents.store.get(ix.player).map(|e| e.uuid())
}

/// Whether the interacting player is sneaking (`isSecondaryUseActive`).
pub fn sneaking(ix: &Interact<'_, '_>) -> bool {
    ix.sneaking
}

/// Uses up one item of the held stack (`usePlayerItem` / `consume(1, player)`).
pub fn consume(ix: &mut Interact<'_, '_>) {
    crate::mob::animal::use_item(ix.ents, ix.player, ix.hand);
}

/// `level.broadcastEntityEvent(target, event)`.
pub fn event(ix: &mut Interact<'_, '_>, event: i8) {
    crate::mob::animal::entity_event(ix.ents, ix.env, ix.target, event);
}

/// `TamableAnimal.feed(player, hand, stack, factor, default)`: heals by the food's nutrition.
pub fn feed(ix: &mut Interact<'_, '_>, factor: f32, default: f32, eat_sound: Option<&str>) {
    let nutrition = food_nutrition(ix.stack);
    consume(ix);
    if let Some(m) = ix.ents.mob_mut(ix.target) {
        heal(m, nutrition.map_or(default, |n| factor * n));
    }
    dirty(ix);
    if let Some(s) = eat_sound {
        sound(ix, s, 1.0, 1.0);
    }
}

/// The nutrition of a food item (`DataComponents.FOOD`), for the foods pets eat.
pub fn food_nutrition(stack: &ItemStack) -> Option<f32> {
    if stack.is_empty() {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    item::item_id(stack).info().food.map(|f| f.nutrition as f32)
}

/// Whether the stack is food (`DataComponents.FOOD`).
pub fn is_food_item(stack: &ItemStack) -> bool {
    food_nutrition(stack).is_some()
}

/// `TamableAnimal.tame(player)`: owner, tame flag and persistence.
pub fn tame_by(m: &mut MobData, owner: u128) {
    crate::mob::tame::tame(m, owner);
}

/// `AgeableMob.finalizeSpawn` with `AgeableMobGroupData(shouldSpawnBaby, chance)`: later
/// members of a spawn group may be babies.
pub fn ageable_group(m: &mut MobData, ctx: &SpawnContext<'_>, spawn_baby: bool, chance: f32) {
    if spawn_baby && ctx.group_index > 0 && m.random.next_float() <= chance {
        m.age = -24000;
        m.baby = true;
    }
}

/// `AgeableMob` and `Animal` NBT: age and love.
pub fn save_ageable(m: &MobData, out: &mut Compound) {
    out.insert("Age", m.age);
    out.insert("ForcedAge", m.forced_age);
    out.insert("AgeLocked", m.age_locked);
    out.insert("InLove", m.in_love);
    if let Some(c) = m.love_cause {
        out.insert("LoveCause", uuid_ints(c));
    }
}

/// Reads the `AgeableMob` and `Animal` NBT.
pub fn load_ageable(m: &mut MobData, tag: &Compound) {
    m.age = int(tag, "Age").unwrap_or(0);
    m.baby = m.age < 0;
    m.forced_age = int(tag, "ForcedAge").unwrap_or(0);
    m.age_locked = crate::mob::entity_nbt::get_bool(tag, "AgeLocked").unwrap_or(false);
    m.in_love = int(tag, "InLove").unwrap_or(0);
    m.love_cause = crate::mob::nbt::uuid_key_of(tag, "LoveCause");
}

/// The `attributes` list of the base values a kind changes (horse stats, tame wolves).
pub fn save_attributes(m: &MobData, out: &mut Compound) {
    let mut list = Vec::new();
    let mut push = |id: &str, base: f64| {
        let mut c = Compound::new();
        c.insert("id", id);
        c.insert("base", base);
        list.push(Tag::Compound(c));
    };
    let base = m.base_attrs.as_deref().unwrap_or(&m.attrs);
    push("minecraft:max_health", base.max_health);
    push("minecraft:movement_speed", base.movement_speed);
    push("minecraft:jump_strength", base.jump_strength);
    out.insert("attributes", vanillite_nbt::List(list));
}

/// A UUID as an int array.
pub fn uuid_ints(u: u128) -> Vec<i32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let v = vec![
        (u >> 96) as i32,
        (u >> 64) as i32,
        (u >> 32) as i32,
        u as i32,
    ];
    v
}

/// An int (or byte/short) value of a compound.
pub fn int(tag: &Compound, key: &str) -> Option<i32> {
    crate::mob::entity_nbt::get_int(tag, key)
}

/// A boolean (byte) value of a compound.
pub fn boolean(tag: &Compound, key: &str) -> bool {
    crate::mob::entity_nbt::get_bool(tag, key).unwrap_or(false)
}

/// A string value of a compound.
pub fn string<'a>(tag: &'a Compound, key: &str) -> Option<&'a str> {
    tag.get(key).and_then(Tag::as_str)
}

/// The `AgeableMob` data (baby, age locked) every companion writes first.
pub fn encode_ageable(w: &mut DataWriter<'_>, m: &MobData) {
    w.boolean(16, m.baby, false);
    w.boolean(17, m.age_locked, false);
}

/// The `TamableAnimal` data (flags, owner).
pub fn encode_tamable(w: &mut DataWriter<'_>, m: &MobData) {
    let flags = u8::from(m.in_sitting_pose) | if m.tame { 4 } else { 0 };
    w.byte(18, flags, 0);
    w.optional_uuid(19, m.owner);
}

/// A registry holder value (variants): always sent.
pub fn encode_holder(w: &mut DataWriter<'_>, id: u8, serializer: i32, v: i32) {
    w.varint(id, serializer, v, -1);
}

/// A `LONG` item.
pub fn encode_long(w: &mut DataWriter<'_>, id: u8, v: i64, default: i64) {
    w.raw(id, serializer::LONG, v == default, |o| {
        vanillite_protocol::codec::write_varlong(o, v);
    });
}

/// A random entry of a biome-dependent variant list (`VariantUtils.selectVariantToSpawn`,
/// `PriorityProvider.pick`): `(index, condition)` pairs in registry order; the entries whose
/// biome condition holds (priority 1) win over the default (priority 0), and one of the
/// winners is drawn (`Util.getRandomSafe`, a draw even for a single one).
pub fn pick_variant(
    ctx: &SpawnContext<'_>,
    random: &mut LegacyRandom,
    entries: &[(i32, Option<&str>)],
    default: i32,
) -> i32 {
    let mut matched: Vec<i32> = entries
        .iter()
        .filter(|(_, c)| c.is_some_and(|c| biome_matches(ctx, c)))
        .map(|(i, _)| *i)
        .collect();
    if matched.is_empty() {
        matched.push(default);
    }
    let n = i32::try_from(matched.len()).unwrap_or(i32::MAX);
    let i = usize::try_from(random.next_int_bounded(n)).unwrap_or(0);
    matched[i]
}

/// A biome condition: a biome id or `#tag`.
pub fn biome_matches(ctx: &SpawnContext<'_>, cond: &str) -> bool {
    let Some(b) = ctx.biome else {
        return false;
    };
    if cond.starts_with('#') {
        b.is(cond)
    } else {
        &*b.name == cond
    }
}

/// `Animal.checkAnimalSpawnRules`: grass (`#animals_spawnable_on`) below and bright enough.
pub fn animal_spawn_rules(level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    let below = level.block(BlockPos::new(pos.x, pos.y - 1, pos.z));
    below.block() == vanillite_data::block::ids::GRASS_BLOCK && bright_enough(level, pos, reason)
}

/// `Animal.isBrightEnoughToSpawn` (spawners and chunk generation ignore light).
pub fn bright_enough(level: &Level, pos: BlockPos, reason: SpawnReason) -> bool {
    if matches!(
        reason,
        SpawnReason::ChunkGeneration | SpawnReason::TrialSpawner
    ) {
        return true;
    }
    let (sky, block) = level.light(pos);
    sky.max(block) > 8
}

/// Whether the block below is in a block tag of `names` (the `*_spawnable_on` tags).
pub fn below_is(level: &Level, pos: BlockPos, names: &[&str]) -> bool {
    let below = level.block(BlockPos::new(pos.x, pos.y - 1, pos.z));
    let name = below.block().name();
    names.contains(&name)
}

impl FamilyHooks for Hooks {
    type Goal = Goal;

    fn is_monster(_kind: Kind) -> bool {
        false
    }

    fn ageable(_kind: Kind) -> bool {
        true
    }

    fn ambient_sound_interval(kind: Kind) -> i32 {
        if kind == Kind::Ocelot {
            return 900;
        }
        if matches!(
            kind,
            Kind::Horse | Kind::Donkey | Kind::Mule | Kind::SkeletonHorse | Kind::ZombieHorse
        ) || matches!(kind, Kind::Llama | Kind::TraderLlama)
        {
            400
        } else {
            120
        }
    }

    fn allowed_in_peaceful(_kind: Kind) -> bool {
        true
    }

    fn remove_when_far_away(kind: Kind) -> bool {
        matches!(kind, Kind::ZombieHorse | Kind::Cat | Kind::Ocelot)
    }

    fn requires_custom_persistence(m: &MobData) -> bool {
        // `Cat`/`Ocelot.removeWhenFarAway`: only strays older than two minutes despawn.
        match m.kind {
            Kind::Cat => m.tame || m.tick_count <= 2400,
            Kind::Ocelot => st(m).trusting || m.tick_count <= 2400,
            _ => false,
        }
    }

    fn fall_damage_immune(kind: Kind) -> bool {
        // `Parrot.checkFallDamage` does nothing.
        kind == Kind::Parrot
    }

    fn malus(kind: Kind) -> &'static MalusTable {
        const ANIMAL: &MalusTable = &[(PathType::FireInNeighbor, 16.0), (PathType::Fire, -1.0)];
        const WOLF: &MalusTable = &[
            (PathType::FireInNeighbor, 16.0),
            (PathType::Fire, -1.0),
            (PathType::PowderSnow, -1.0),
            (PathType::OnTopOfPowderSnow, -1.0),
        ];
        const PARROT: &MalusTable = &[
            (PathType::FireInNeighbor, -1.0),
            (PathType::Fire, -1.0),
            (PathType::Cocoa, -1.0),
        ];
        const FOX: &MalusTable = &[
            (PathType::FireInNeighbor, 16.0),
            (PathType::Fire, -1.0),
            (PathType::DamagingInNeighbor, 0.0),
            (PathType::Damaging, 0.0),
        ];
        match kind {
            Kind::Wolf => WOLF,
            Kind::Parrot => PARROT,
            Kind::Fox => FOX,
            _ => ANIMAL,
        }
    }

    fn sound_source(_kind: Kind) -> SoundSource {
        SoundSource::Neutral
    }

    fn sound_volume(kind: Kind) -> f32 {
        match kind {
            Kind::Wolf => 0.4,
            Kind::Horse | Kind::Donkey | Kind::Mule | Kind::SkeletonHorse | Kind::ZombieHorse => {
                0.8
            }
            Kind::Llama | Kind::TraderLlama => 0.8,
            Kind::Rabbit => 0.6,
            _ => 1.0,
        }
    }

    fn ambient_sound(m: &mut MobData) -> Option<&'static str> {
        match m.kind {
            k if is_horse(k) => horse::ambient_sound(m),
            Kind::Wolf => wolf::ambient_sound(m),
            Kind::Cat => feline::cat_ambient_sound(m),
            Kind::Ocelot => Some("minecraft:entity.ocelot.ambient"),
            Kind::Parrot => Some("minecraft:entity.parrot.ambient"),
            Kind::Fox => fox::ambient_sound(m),
            Kind::Panda => panda::ambient_sound(m),
            Kind::PolarBear => Some(if m.baby {
                "minecraft:entity.polar_bear.ambient_baby"
            } else {
                "minecraft:entity.polar_bear.ambient"
            }),
            Kind::Rabbit => Some("minecraft:entity.rabbit.ambient"),
            k => Some(k.default_sound(DefaultSound::Ambient)),
        }
    }

    fn hurt_sound(m: &MobData) -> &'static str {
        match m.kind {
            k if is_horse(k) => horse::hurt_sound(m),
            Kind::Wolf => wolf::hurt_sound(m),
            Kind::Cat => feline::cat_sound(m, "hurt"),
            Kind::Parrot => "minecraft:entity.parrot.hurt",
            Kind::Panda => "minecraft:entity.panda.hurt",
            k => k.default_sound(DefaultSound::Hurt),
        }
    }

    fn death_sound(m: &MobData) -> &'static str {
        match m.kind {
            k if is_horse(k) => horse::death_sound(m),
            Kind::Wolf => wolf::death_sound(m),
            Kind::Cat => feline::cat_sound(m, "death"),
            k => k.default_sound(DefaultSound::Death),
        }
    }

    fn dimensions(m: &MobData) -> Option<Dimensions> {
        if !m.baby {
            return None;
        }
        let base = Dimensions::of(m.kind.type_id());
        let scale = match m.kind {
            Kind::Wolf => {
                return Some(Dimensions {
                    width: 0.3,
                    height: 0.425,
                });
            }
            Kind::Horse | Kind::SkeletonHorse | Kind::ZombieHorse => 0.7,
            Kind::Donkey | Kind::Mule | Kind::Llama | Kind::TraderLlama => 0.5,
            Kind::Rabbit => 0.6,
            Kind::Fox | Kind::Panda | Kind::PolarBear | Kind::Cat | Kind::Ocelot => 0.5,
            _ => return None,
        };
        Some(Dimensions {
            width: base.width * scale,
            height: base.height * scale,
        })
    }

    fn walk_target_value(kind: Kind, level: &Level, env: &Environment, pos: BlockPos) -> f64 {
        match kind {
            Kind::Fox | Kind::Rabbit | Kind::Parrot | Kind::Ocelot => {
                // `PathfinderMob.getWalkTargetValue` (0) for the kinds that do not prefer
                // grass; foxes and rabbits rank by their own rules.
                match kind {
                    Kind::Rabbit | Kind::Fox | Kind::Ocelot => {
                        crate::mob::kinds::base_walk_target_value(false, level, env, pos)
                    }
                    _ => parrot::walk_target_value(level, pos),
                }
            }
            _ => crate::mob::kinds::base_walk_target_value(false, level, env, pos),
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
            // `SpawnPlacements`: `Monster::checkMonsterSpawnRules`.
            Kind::ZombieHorse => {
                crate::mob::spawn::check_monster_spawn_rules(kind, level, env, pos, random, reason)
            }
            Kind::Wolf => wolf::spawn_rules(level, pos, reason),
            Kind::Rabbit => rabbit::spawn_rules(level, pos, reason),
            Kind::Fox => fox::spawn_rules(level, pos, reason),
            Kind::PolarBear => polar_bear::spawn_rules(level, pos, reason),
            Kind::Ocelot => feline::ocelot_spawn_rules(level, pos, random, reason),
            Kind::Parrot => parrot::spawn_rules(level, pos, reason),
            Kind::Llama => llama::spawn_rules(level, pos, reason),
            Kind::SkeletonHorse => horse::skeleton_spawn_rules(level, pos, reason),
            Kind::Panda
            | Kind::Horse
            | Kind::Donkey
            | Kind::Mule
            | Kind::Cat
            | Kind::TraderLlama => animal_spawn_rules(level, pos, reason),
            _ => animal_spawn_rules(level, pos, reason),
        }
    }

    fn is_food(kind: Kind, item: u16) -> bool {
        let tag = match kind {
            Kind::Horse | Kind::Donkey | Kind::Mule | Kind::SkeletonHorse => "minecraft:horse_food",
            Kind::ZombieHorse => "minecraft:zombie_horse_food",
            Kind::Llama | Kind::TraderLlama => "minecraft:llama_food",
            Kind::Wolf => "minecraft:wolf_food",
            Kind::Cat => "minecraft:cat_food",
            Kind::Ocelot => "minecraft:ocelot_food",
            Kind::Parrot => "minecraft:parrot_food",
            Kind::Fox => "minecraft:fox_food",
            Kind::Panda => "minecraft:panda_food",
            Kind::PolarBear => "minecraft:polar_bear_food",
            Kind::Rabbit => "minecraft:rabbit_food",
            _ => return false,
        };
        crate::player::play::game_data().is_some_and(|d| d.item_in_tag(item, tag))
    }

    fn anger_range(_kind: Kind) -> (i32, i32) {
        (20 * 20, 39 * 20)
    }

    fn init(m: &mut MobData) {
        let s = st_mut(m);
        s.collar = 14;
        s.strength = 1;
        s.despawn_delay = 47_999;
        if m.kind == Kind::Wolf {
            // `WolfSoundVariants.CLASSIC` and `WolfVariants.DEFAULT` (pale).
            st_mut(m).sound_variant = wolf::SOUND_CLASSIC;
            m.variant = wolf::VARIANT_PALE;
        }
    }

    fn register_goals(m: &mut MobData) {
        match m.kind {
            k if is_horse(k) && !matches!(k, Kind::Llama | Kind::TraderLlama) => {
                horse::register_goals(m);
            }
            Kind::Llama | Kind::TraderLlama => llama::register_goals(m),
            Kind::Wolf => wolf::register_goals(m),
            Kind::Cat | Kind::Ocelot => feline::register_goals(m),
            Kind::Parrot => parrot::register_goals(m),
            Kind::Fox => fox::register_goals(m),
            Kind::Panda => panda::register_goals(m),
            Kind::PolarBear => polar_bear::register_goals(m),
            Kind::Rabbit => rabbit::register_goals(m),
            _ => {}
        }
    }

    fn finalize_spawn(m: &mut MobData, ctx: &mut SpawnContext<'_>) {
        match m.kind {
            k if is_horse(k) && !matches!(k, Kind::Llama | Kind::TraderLlama) => {
                horse::finalize_spawn(m, ctx);
            }
            Kind::Llama | Kind::TraderLlama => llama::finalize_spawn(m, ctx),
            Kind::Wolf => wolf::finalize_spawn(m, ctx),
            Kind::Cat | Kind::Ocelot => feline::finalize_spawn(m, ctx),
            Kind::Parrot => parrot::finalize_spawn(m, ctx),
            Kind::Fox => fox::finalize_spawn(m, ctx),
            Kind::Panda => panda::finalize_spawn(m, ctx),
            Kind::PolarBear => polar_bear::finalize_spawn(m, ctx),
            Kind::Rabbit => rabbit::finalize_spawn(m, ctx),
            _ => ageable_group(m, ctx, true, 0.05),
        }
    }

    fn before_tick(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            k if is_horse(k) => horse::before_tick(t),
            Kind::Wolf => wolf::before_tick(t),
            Kind::Cat => feline::cat_tick(t),
            Kind::PolarBear => polar_bear::before_tick(t),
            _ => {}
        }
    }

    fn after_tick(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            k if is_horse(k) => horse::after_tick(t.mob()),
            // `Panda.tick` and `Fox.tick` run their own part after `super.tick()`.
            Kind::Panda => panda::after_tick(t),
            Kind::Fox => fox::after_tick(t),
            _ => {}
        }
    }

    fn is_immobile(m: &MobData) -> bool {
        is_horse(m.kind) && horse::is_immobile(m)
    }
    fn ai_step_start(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            k if is_horse(k) => horse::ai_step_start(t),
            Kind::Parrot => parrot::ai_step_start(t),
            _ => {}
        }
    }
    fn ai_step(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            k if is_horse(k) => horse::ai_step(t),
            Kind::Wolf => wolf::ai_step(t),
            Kind::Parrot => parrot::ai_step(t),
            Kind::Fox => fox::ai_step(t),
            Kind::Panda => panda::ai_step(t),
            Kind::PolarBear => polar_bear::ai_step(t),
            Kind::Rabbit => rabbit::ai_step(t),
            _ => {}
        }
    }

    fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
        match t.mob_ref().kind {
            Kind::Cat | Kind::Ocelot => feline::custom_server_ai_step(t),
            Kind::Rabbit => rabbit::custom_server_ai_step(t),
            Kind::Fox => fox::custom_server_ai_step(t),
            _ => {}
        }
    }

    fn look_control(t: &mut Tick<'_, '_>) -> bool {
        if t.mob_ref().kind == Kind::Fox {
            fox::look_control(t);
            return true;
        }
        false
    }

    fn move_control(t: &mut Tick<'_, '_>) -> bool {
        match t.mob_ref().kind {
            Kind::Rabbit => rabbit::move_control(t),
            _ => false,
        }
    }

    fn on_fall_damage(t: &mut Tick<'_, '_>, distance: f64) {
        if is_horse(t.mob_ref().kind) {
            horse::on_fall_damage(t, distance);
        }
    }

    fn do_hurt_target(t: &mut Tick<'_, '_>, target: EntityId) -> Option<bool> {
        match t.mob_ref().kind {
            Kind::Rabbit => rabbit::do_hurt_target(t, target),
            Kind::Fox => fox::do_hurt_target(t, target),
            Kind::Panda => panda::do_hurt_target(t, target),
            Kind::PolarBear => polar_bear::do_hurt_target(t, target),
            Kind::Parrot => parrot::do_hurt_target(t, target),
            _ => None,
        }
    }

    fn perform_ranged_attack(t: &mut Tick<'_, '_>, target: EntityId, power: f32) {
        if matches!(t.mob_ref().kind, Kind::Llama | Kind::TraderLlama) {
            llama::spit(t, target, power);
        }
    }

    fn hurt_filter(h: &mut Hurt<'_, '_>, amount: f32) -> Option<f32> {
        let kind = h.ents.mob(h.target).map(|m| m.kind)?;
        match kind {
            Kind::Wolf => wolf::hurt_filter(h, amount),
            Kind::Parrot => {
                // `Parrot.hurtServer` stands up (a hurt cat keeps sitting).
                if let Some(m) = h.ents.mob_mut(h.target) {
                    m.order_to_sit = false;
                }
                Some(amount)
            }
            Kind::Panda => panda::hurt_filter(h, amount),
            Kind::Fox => fox::hurt_filter(h, amount),
            _ => Some(amount),
        }
    }

    fn after_hurt(h: &mut Hurt<'_, '_>, amount: f32) {
        let Some(kind) = h.ents.mob(h.target).map(|m| m.kind) else {
            return;
        };
        if is_horse(kind) {
            horse::after_hurt(h, amount);
        } else if kind == Kind::Ocelot {
            feline::ocelot_after_hurt(h);
        }
    }

    fn on_die(h: &mut Hurt<'_, '_>) {
        let Some(kind) = h.ents.mob(h.target).map(|m| m.kind) else {
            return;
        };
        if is_horse(kind) {
            horse::on_die(h);
        } else if kind == Kind::Fox {
            fox::on_die(h);
        }
    }

    fn pick_up_items(t: &mut Tick<'_, '_>) {
        let reach = (1.0, 0.0, 1.0);
        match t.mob_ref().kind {
            Kind::Fox => {
                let rules = crate::mob::equipment::PickupRules {
                    wants: fox::can_hold,
                    preferred_weapon: None,
                };
                crate::mob::equipment::pick_up_items(t, &rules, Some(fox::pick_up), reach);
            }
            Kind::Panda if !t.mob_ref().baby => {
                let rules = panda::pickup_rules();
                crate::mob::equipment::pick_up_items(t, &rules, Some(panda::pick_up), reach);
            }
            _ => {}
        }
    }

    fn interact(ix: &mut Interact<'_, '_>) -> Interaction {
        let Some(kind) = ix.ents.mob(ix.target).map(|m| m.kind) else {
            return Interaction::Pass;
        };
        match kind {
            Kind::Llama | Kind::TraderLlama => llama::interact(ix),
            k if is_horse(k) => horse::interact(ix),
            Kind::Wolf => wolf::interact(ix),
            Kind::Cat => feline::cat_interact(ix),
            Kind::Ocelot => feline::ocelot_interact(ix),
            Kind::Parrot => parrot::interact(ix),
            Kind::Fox => animal_interact(ix),
            Kind::Panda => panda::interact(ix),
            Kind::PolarBear | Kind::Rabbit => animal_interact(ix),
            _ => Interaction::Pass,
        }
    }

    fn can_mate(me: &MobData, partner: &MobData) -> bool {
        match me.kind {
            k if is_horse(k) => horse::can_mate(me, partner),
            Kind::Wolf | Kind::Cat => {
                me.tame
                    && partner.kind == me.kind
                    && partner.tame
                    && !(me.kind == Kind::Wolf && partner.in_sitting_pose)
                    && me.in_love > 0
                    && partner.in_love > 0
            }
            Kind::Parrot => false,
            Kind::Panda => panda::can_mate(me, partner),
            _ => me.kind == partner.kind && me.in_love > 0 && partner.in_love > 0,
        }
    }

    fn offspring_kind(parent: &MobData, partner: &MobData) -> Kind {
        horse::offspring_kind(parent, partner)
    }

    fn rider_controls_mob(m: &MobData) -> bool {
        is_horse(m.kind) && horse::is_saddled(m)
    }

    fn breed_offspring(
        parent: &MobData,
        partner: &MobData,
        child: &mut MobData,
        random: &mut LegacyRandom,
    ) {
        match parent.kind {
            Kind::Llama | Kind::TraderLlama => {
                llama::breed_offspring(parent, partner, child, random)
            }
            k if is_horse(k) => horse::breed_offspring(parent, partner, child, random),
            Kind::Wolf | Kind::Cat => {
                child.variant = if random.next_boolean() {
                    parent.variant
                } else {
                    partner.variant
                };
                if parent.tame {
                    child.owner = parent.owner;
                    child.tame = true;
                    child.persistent = true;
                    let c = mixed_color(st(parent).collar, st(partner).collar, random);
                    st_mut(child).collar = c;
                    if child.kind == Kind::Wolf {
                        wolf::apply_taming_side_effects(child);
                    }
                }
                if child.kind == Kind::Wolf {
                    st_mut(child).sound_variant = wolf::random_sound_variant(random);
                }
            }
            Kind::Fox => fox::breed_offspring(parent, partner, child, random),
            Kind::Panda => panda::breed_offspring(parent, partner, child, random),
            Kind::Rabbit => rabbit::breed_offspring(parent, partner, child, random),
            Kind::Ocelot | Kind::PolarBear | Kind::Parrot => {}
            _ => {}
        }
    }

    fn encode_data(w: &mut DataWriter<'_>, m: &MobData) {
        encode_ageable(w, m);
        match m.kind {
            k if is_horse(k) => horse::encode(w, m),
            Kind::Wolf => wolf::encode(w, m),
            Kind::Cat => feline::cat_encode(w, m),
            Kind::Ocelot => w.boolean(18, st(m).trusting, false),
            Kind::Parrot => {
                encode_tamable(w, m);
                w.int(20, m.variant, 0);
            }
            Kind::Fox => fox::encode(w, m),
            Kind::Panda => panda::encode(w, m),
            Kind::PolarBear => w.boolean(18, st(m).standing, false),
            Kind::Rabbit => w.int(18, m.variant, 0),
            _ => {}
        }
    }

    fn save(m: &MobData, out: &mut Compound) {
        save_ageable(m, out);
        if is_horse(m.kind) || m.kind == Kind::Wolf || m.kind == Kind::Panda {
            save_attributes(m, out);
        }
        match m.kind {
            k if is_horse(k) => horse::save(m, out),
            Kind::Wolf => wolf::save(m, out),
            Kind::Cat => feline::cat_save(m, out),
            Kind::Ocelot => out.insert("Trusting", st(m).trusting),
            Kind::Parrot => out.insert("Variant", m.variant),
            Kind::Fox => fox::save(m, out),
            Kind::Panda => panda::save(m, out),
            Kind::PolarBear => crate::mob::common::save_anger(m, out),
            Kind::Rabbit => {
                out.insert("RabbitType", m.variant);
                out.insert("MoreCarrotTicks", st(m).more_carrot_ticks);
            }
            _ => {}
        }
    }

    fn load(m: &mut MobData, tag: &Compound) {
        load_ageable(m, tag);
        match m.kind {
            k if is_horse(k) => horse::load(m, tag),
            Kind::Wolf => wolf::load(m, tag),
            Kind::Cat => feline::cat_load(m, tag),
            Kind::Ocelot => st_mut(m).trusting = boolean(tag, "Trusting"),
            Kind::Parrot => m.variant = int(tag, "Variant").unwrap_or(0).clamp(0, 4),
            Kind::Fox => fox::load(m, tag),
            Kind::Panda => panda::load(m, tag),
            Kind::PolarBear => crate::mob::common::load_anger(m, tag, 0),
            Kind::Rabbit => {
                m.variant = rabbit::clamp_type(int(tag, "RabbitType").unwrap_or(0));
                st_mut(m).more_carrot_ticks = int(tag, "MoreCarrotTicks").unwrap_or(0);
                if m.variant == rabbit::EVIL {
                    rabbit::apply_evil(m);
                }
            }
            _ => {}
        }
    }

    fn missile_tick(t: &mut MissileTick<'_, '_>) {
        llama::spit_tick(t);
    }

    fn missile_encode(_w: &mut DataWriter<'_>, _d: &MissileData) {}

    // --- Goals ------------------------------------------------------------------------

    fn goal_flags(g: &Goal) -> u8 {
        match g {
            Goal::Idle => 0,
            Goal::Guard(_, c) => c.flags(),
            Goal::TamePanic { .. } => crate::mob::goals::flag::MOVE,
            Goal::Horse(g) => horse::flags(g),
            Goal::Llama(g) => llama::flags(g),
            Goal::Wolf(g) => wolf::flags(g),
            Goal::Feline(g) => feline::flags(g),
            Goal::Parrot(g) => parrot::flags(g),
            Goal::Fox(g) => fox::flags(g),
            Goal::Panda(g) => panda::flags(g),
            Goal::Bear(g) => polar_bear::flags(g),
            Goal::Rabbit(g) => rabbit::flags(g),
            Goal::ClimbPowderSnow => crate::mob::goals::flag::JUMP,
        }
    }

    fn goal_every_tick(g: &Goal) -> bool {
        match g {
            Goal::Idle => false,
            Goal::Guard(_, c) => c.every_tick(),
            Goal::TamePanic { .. } => false,
            Goal::Horse(g) => horse::every_tick(g),
            Goal::Llama(_) => false,
            Goal::Wolf(_) => false,
            Goal::Feline(g) => feline::every_tick(g),
            Goal::Parrot(_) => false,
            Goal::Fox(g) => fox::every_tick(g),
            Goal::Panda(g) => panda::every_tick(g),
            Goal::Bear(g) => polar_bear::every_tick(g),
            Goal::Rabbit(g) => rabbit::every_tick(g),
            Goal::ClimbPowderSnow => true,
        }
    }

    fn goal_can_use(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
        match g {
            Goal::Idle => false,
            Goal::Guard(c, cg) => cond_holds(t, *c) && crate::mob::common::can_use(t, cg),
            Goal::TamePanic { env_only, pos, .. } => {
                if !should_panic(t, *env_only) {
                    return false;
                }
                match crate::mob::animal::panic_pos(t) {
                    Some(p) => {
                        *pos = p;
                        true
                    }
                    None => false,
                }
            }
            Goal::Horse(g) => horse::can_use(t, g),
            Goal::Llama(g) => llama::can_use(t, g),
            Goal::Wolf(g) => wolf::can_use(t, g),
            Goal::Feline(g) => feline::can_use(t, g),
            Goal::Parrot(g) => parrot::can_use(t, g),
            Goal::Fox(g) => fox::can_use(t, g),
            Goal::Panda(g) => panda::can_use(t, g),
            Goal::Bear(g) => polar_bear::can_use(t, g),
            Goal::Rabbit(g) => rabbit::can_use(t, g),
            Goal::ClimbPowderSnow => climb_powder_snow_can_use(t),
        }
    }

    fn goal_can_continue(t: &mut Tick<'_, '_>, g: &mut Goal) -> bool {
        match g {
            Goal::Idle => false,
            Goal::Guard(c, cg) => cond_holds(t, *c) && crate::mob::common::can_continue(t, cg),
            Goal::TamePanic { .. } => !t.nav_ref().is_done(),
            Goal::Horse(g) => horse::can_continue(t, g),
            Goal::Llama(g) => llama::can_continue(t, g),
            Goal::Wolf(g) => wolf::can_continue(t, g),
            Goal::Feline(g) => feline::can_continue(t, g),
            Goal::Parrot(g) => parrot::can_continue(t, g),
            Goal::Fox(g) => fox::can_continue(t, g),
            Goal::Panda(g) => panda::can_continue(t, g),
            Goal::Bear(g) => polar_bear::can_continue(t, g),
            Goal::Rabbit(g) => rabbit::can_continue(t, g),
            Goal::ClimbPowderSnow => climb_powder_snow_can_use(t),
        }
    }

    fn goal_start(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::Idle => {}
            Goal::Guard(_, cg) => crate::mob::common::start(t, cg),
            Goal::TamePanic { speed, pos, .. } => {
                crate::mob::nav::move_to_pos(t, *pos, *speed);
            }
            Goal::Horse(g) => horse::start(t, g),
            Goal::Llama(g) => llama::start(t, g),
            Goal::Wolf(g) => wolf::start(t, g),
            Goal::Feline(g) => feline::start(t, g),
            Goal::Parrot(g) => parrot::start(t, g),
            Goal::Fox(g) => fox::start(t, g),
            Goal::Panda(g) => panda::start(t, g),
            Goal::Bear(g) => polar_bear::start(t, g),
            Goal::Rabbit(g) => rabbit::start(t, g),
            Goal::ClimbPowderSnow => {}
        }
    }

    fn goal_stop(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::Idle => {}
            Goal::Guard(_, cg) => crate::mob::common::stop(t, cg),
            Goal::TamePanic { .. } => {}
            Goal::Horse(g) => horse::stop(t, g),
            Goal::Llama(g) => llama::stop(t, g),
            Goal::Wolf(g) => wolf::stop(t, g),
            Goal::Feline(g) => feline::stop(t, g),
            Goal::Parrot(g) => parrot::stop(t, g),
            Goal::Fox(g) => fox::stop(t, g),
            Goal::Panda(g) => panda::stop(t, g),
            Goal::Bear(g) => polar_bear::stop(t, g),
            Goal::Rabbit(g) => rabbit::stop(t, g),
            Goal::ClimbPowderSnow => {}
        }
    }

    fn goal_tick(t: &mut Tick<'_, '_>, g: &mut Goal) {
        match g {
            Goal::Idle => {}
            Goal::Guard(_, cg) => crate::mob::common::tick(t, cg),
            Goal::TamePanic { .. } => {
                // `TamableAnimalPanicGoal.tick`: far owners pull the pet to them.
                if is_tamable(t.mob_ref().kind) {
                    crate::mob::fam::companion::teleport_to_owner_if_far(t);
                }
            }
            Goal::Horse(g) => horse::tick(t, g),
            Goal::Llama(g) => llama::tick(t, g),
            Goal::Wolf(g) => wolf::tick(t, g),
            Goal::Feline(g) => feline::tick(t, g),
            Goal::Parrot(g) => parrot::tick(t, g),
            Goal::Fox(g) => fox::tick(t, g),
            Goal::Panda(g) => panda::tick(t, g),
            Goal::Bear(g) => polar_bear::tick(t, g),
            Goal::Rabbit(g) => rabbit::tick(t, g),
            Goal::ClimbPowderSnow => t.mob().jump = true,
        }
    }
}

/// `ClimbOnTopOfPowderSnowGoal.canUse`: in powder snow (this or the last tick), a
/// `#powder_snow_walkable_mobs` kind, with powder snow or no collision above.
fn climb_powder_snow_can_use(t: &Tick<'_, '_>) -> bool {
    let m = t.mob_ref();
    if !(m.was_in_powder_snow || m.in_powder_snow)
        || !crate::effect::entity_in_tag(m.kind.type_id(), "minecraft:powder_snow_walkable_mobs")
    {
        return false;
    }
    let p = t.block_pos();
    let above = t.level.block(BlockPos::new(p.x, p.y + 1, p.z));
    above.block() == vanillite_data::block::ids::POWDER_SNOW || above.collision_shape().is_empty()
}

/// `Animal.mobInteract`: feeding for love and growth.
pub fn animal_interact(ix: &mut Interact<'_, '_>) -> Interaction {
    crate::mob::animal::animal_interact(ix.ents, ix.env, ix.player, ix.target, ix.hand, ix.stack)
}

/// `PanicGoal.shouldPanic`: the last damage is a panic cause (`#panic_causes`, or
/// `#panic_environmental_causes` for `env_only`).
fn should_panic(t: &Tick<'_, '_>, env_only: bool) -> bool {
    let Some(kind) = t.mob_ref().last_damage_source(t.game_time()) else {
        return false;
    };
    if env_only {
        let id = &t.env.types.get(kind).id;
        matches!(
            id.as_str(),
            "minecraft:cactus"
                | "minecraft:freeze"
                | "minecraft:hot_floor"
                | "minecraft:sulfur_cube_hot"
                | "minecraft:in_fire"
                | "minecraft:lava"
                | "minecraft:lightning_bolt"
                | "minecraft:on_fire"
        )
    } else {
        t.env
            .types
            .is(kind, crate::survival::damage::tag::PANIC_CAUSES)
    }
}

/// `TamableAnimal.tryToTeleportToOwner` when the owner is 12 or more blocks away and the
/// pet may move to it.
pub fn teleport_to_owner_if_far(t: &mut Tick<'_, '_>) {
    let m = t.mob_ref();
    let Some(owner) = crate::mob::tame::owner_of(t.ents, m) else {
        return;
    };
    if m.order_to_sit || t.ents.vehicle_of(t.id).is_some() || t.ents.leashes.contains_key(&t.id) {
        return;
    }
    let Some(op) = t.ents.store.get(owner).map(|e| e.pos()) else {
        return;
    };
    if op.distance_to_sqr(t.pos()) >= 144.0 {
        crate::mob::tame::teleport_near_owner(t, op);
    }
}

/// The nearest live, non-spectator player within `range` of the mob that `test` accepts
/// (`getNearestPlayer(forNonCombat().range(range))`).
pub fn nearest_player(
    t: &mut Tick<'_, '_>,
    range: f64,
    test: impl Fn(&crate::player::PlayerData) -> bool,
) -> Option<EntityId> {
    let p = t.pos();
    let area = vanillite_base::Aabb::new(
        p.x - range,
        p.y - range,
        p.z - range,
        p.x + range,
        p.y + range,
        p.z + range,
    );
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&area, true, |id| ids.push(id));
    let mut best: Option<(EntityId, f64)> = None;
    for &id in &ids {
        let Some(pd) = t.ents.player(id) else {
            continue;
        };
        if pd.survival.is_dead_or_dying()
            || pd.game_mode == vanillite_config::GameMode::Spectator
            || !test(pd)
        {
            continue;
        }
        let Some(e) = t.ents.store.get(id) else {
            continue;
        };
        let d = e.pos().distance_to_sqr(p);
        if d <= range * range && best.is_none_or(|(_, b)| d < b) {
            best = Some((id, d));
        }
    }
    t.cx.ids = ids;
    best.map(|(id, _)| id)
}

/// Whether a player holds a matching stack in either hand.
pub fn holds(pd: &crate::player::PlayerData, test: impl Fn(&ItemStack) -> bool) -> bool {
    [
        crate::item::EquipmentSlot::MainHand,
        crate::item::EquipmentSlot::OffHand,
    ]
    .iter()
    .any(|&s| {
        let st = pd.inventory.equipment(s);
        !st.is_empty() && test(st)
    })
}

/// The first player passenger of a mob.
pub fn first_player_passenger(t: &Tick<'_, '_>) -> Option<EntityId> {
    t.ents
        .riding
        .passengers_of(t.id)
        .first()
        .copied()
        .filter(|&p| t.ents.player(p).is_some())
}

/// Whether the ticking mob carries passengers (`isVehicle`).
pub fn is_vehicle(t: &Tick<'_, '_>) -> bool {
    !t.ents.riding.passengers_of(t.id).is_empty()
}

/// `isInWaterOrRain`.
pub fn in_water_or_rain(t: &Tick<'_, '_>) -> bool {
    let p = t.pos();
    t.mob_ref().fluids.in_water()
        || (t.env.raining_at)(BlockPos::containing(p.x, p.y, p.z))
        || (t.env.raining_at)(BlockPos::containing(p.x, t.aabb().max_y, p.z))
}

/// `random.nextGaussian` stand-in for particles is not needed server-side; the helper picks a
/// random point near the mob for drops.
pub fn drop_at(t: &mut Tick<'_, '_>, stack: ItemStack) {
    crate::mob::equipment::spawn_at_location(t, stack, 0.0);
}

/// A damage source of `kind` caused by the ticking mob.
pub fn mob_source(t: &Tick<'_, '_>, kind: crate::survival::damage::DamageKind) -> DamageSource {
    crate::mob::combat::entity_source(t.ents, kind, t.id, t.id)
}

/// Shorthand for the goal selector constructors.
pub use goals::{Goal as G, MeleeKind, TemptItems};

#[cfg(test)]
mod tests;
