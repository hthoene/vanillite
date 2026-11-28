//! What differs between the mob kinds: goals, sounds, experience, spawn setup and the kind
//! specific parts of their tick (creeper fuse, spider climbing, sheep grazing, enderman
//! teleports, animal ageing).

use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, Vec3};
use vanillite_data::block::{BlockState, ids};
use vanillite_data::entity::{EntityTypeId, ids as entity_ids};
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{Dimensions, EntityId};

use super::fam::classic;
use super::goals::{Goal, MeleeKind};
use super::path::{MalusTable, PathType};
use super::physics::{self, Blocks};
use super::{
    Environment, ExplosionRequest, MobContext, MobData, Other, Tick, WorldAction, atan2_deg,
    combat, light_magic_value, nav,
};
use super::{equipment, fam};
use crate::entities::Entities;
use crate::level::Level;
use crate::survival::damage::DamageSource;
use crate::survival::player::Env;
use crate::survival::rules::{Difficulty, Rule};

/// Declares every mob kind with its name, entity type and family.
macro_rules! kinds {
    ($($kind:ident $name:literal $id:ident $family:ident;)*) => {
        /// The mob kinds with behaviour.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Kind {
            $($kind,)*
        }

        impl Kind {
            /// All kinds.
            pub const ALL: &[Kind] = &[$(Kind::$kind,)*];

            /// The entity type.
            pub fn type_id(self) -> EntityTypeId {
                match self {
                    $(Kind::$kind => entity_ids::$id,)*
                }
            }

            /// The id without namespace (`zombie`).
            pub fn name(self) -> &'static str {
                match self {
                    $(Kind::$kind => $name,)*
                }
            }

            /// `entity.minecraft.<name>`.
            pub fn translation_key(self) -> &'static str {
                match self {
                    $(Kind::$kind => concat!("entity.minecraft.", $name),)*
                }
            }

            /// The family that implements the kind.
            pub fn family(self) -> Family {
                match self {
                    $(Kind::$kind => Family::$family,)*
                }
            }

            /// `minecraft:entity.<name>.<suffix>`: the default ambient, hurt and death sounds.
            pub fn default_sound(self, sound: DefaultSound) -> &'static str {
                match (self, sound) {
                    $(
                        (Kind::$kind, DefaultSound::Ambient) => concat!("minecraft:entity.", $name, ".ambient"),
                        (Kind::$kind, DefaultSound::Hurt) => concat!("minecraft:entity.", $name, ".hurt"),
                        (Kind::$kind, DefaultSound::Death) => concat!("minecraft:entity.", $name, ".death"),
                    )*
                }
            }
        }
    };
}

kinds! {
    Zombie "zombie" ZOMBIE Classic;
    Skeleton "skeleton" SKELETON Classic;
    Creeper "creeper" CREEPER Classic;
    Spider "spider" SPIDER Classic;
    Enderman "enderman" ENDERMAN Classic;
    Cow "cow" COW Classic;
    Pig "pig" PIG Classic;
    Sheep "sheep" SHEEP Classic;
    Chicken "chicken" CHICKEN Classic;
    Husk "husk" HUSK Undead;
    Drowned "drowned" DROWNED Undead;
    ZombieVillager "zombie_villager" ZOMBIE_VILLAGER Undead;
    ZombifiedPiglin "zombified_piglin" ZOMBIFIED_PIGLIN Undead;
    Giant "giant" GIANT Undead;
    Stray "stray" STRAY Undead;
    Bogged "bogged" BOGGED Undead;
    Parched "parched" PARCHED Undead;
    WitherSkeleton "wither_skeleton" WITHER_SKELETON Undead;
    CaveSpider "cave_spider" CAVE_SPIDER Undead;
    Silverfish "silverfish" SILVERFISH Undead;
    Endermite "endermite" ENDERMITE Undead;
    Slime "slime" SLIME Undead;
    MagmaCube "magma_cube" MAGMA_CUBE Undead;
    SulfurCube "sulfur_cube" SULFUR_CUBE Undead;
    Phantom "phantom" PHANTOM Undead;
    Witch "witch" WITCH Illager;
    Pillager "pillager" PILLAGER Illager;
    Vindicator "vindicator" VINDICATOR Illager;
    Evoker "evoker" EVOKER Illager;
    Illusioner "illusioner" ILLUSIONER Illager;
    Vex "vex" VEX Illager;
    Ravager "ravager" RAVAGER Illager;
    Ghast "ghast" GHAST Illager;
    Blaze "blaze" BLAZE Illager;
    Shulker "shulker" SHULKER Illager;
    Piglin "piglin" PIGLIN Brainy;
    PiglinBrute "piglin_brute" PIGLIN_BRUTE Brainy;
    Hoglin "hoglin" HOGLIN Brainy;
    Zoglin "zoglin" ZOGLIN Brainy;
    Goat "goat" GOAT Brainy;
    Axolotl "axolotl" AXOLOTL Brainy;
    Frog "frog" FROG Brainy;
    Tadpole "tadpole" TADPOLE Brainy;
    Armadillo "armadillo" ARMADILLO Brainy;
    Sniffer "sniffer" SNIFFER Brainy;
    Camel "camel" CAMEL Mount;
    CamelHusk "camel_husk" CAMEL_HUSK Mount;
    HappyGhast "happy_ghast" HAPPY_GHAST Mount;
    Nautilus "nautilus" NAUTILUS Mount;
    ZombieNautilus "zombie_nautilus" ZOMBIE_NAUTILUS Mount;
    Allay "allay" ALLAY Mount;
    CopperGolem "copper_golem" COPPER_GOLEM Mount;
    Breeze "breeze" BREEZE Mount;
    Creaking "creaking" CREAKING Mount;
    Horse "horse" HORSE Companion;
    Donkey "donkey" DONKEY Companion;
    Mule "mule" MULE Companion;
    SkeletonHorse "skeleton_horse" SKELETON_HORSE Companion;
    ZombieHorse "zombie_horse" ZOMBIE_HORSE Companion;
    Llama "llama" LLAMA Companion;
    TraderLlama "trader_llama" TRADER_LLAMA Companion;
    Wolf "wolf" WOLF Companion;
    Cat "cat" CAT Companion;
    Ocelot "ocelot" OCELOT Companion;
    Parrot "parrot" PARROT Companion;
    Fox "fox" FOX Companion;
    Panda "panda" PANDA Companion;
    PolarBear "polar_bear" POLAR_BEAR Companion;
    Rabbit "rabbit" RABBIT Companion;
    Bee "bee" BEE Wildlife;
    Turtle "turtle" TURTLE Wildlife;
    Mooshroom "mooshroom" MOOSHROOM Wildlife;
    Strider "strider" STRIDER Wildlife;
    SnowGolem "snow_golem" SNOW_GOLEM Wildlife;
    IronGolem "iron_golem" IRON_GOLEM Wildlife;
    Bat "bat" BAT Wildlife;
    Squid "squid" SQUID Wildlife;
    GlowSquid "glow_squid" GLOW_SQUID Wildlife;
    Dolphin "dolphin" DOLPHIN Wildlife;
    Cod "cod" COD Wildlife;
    Salmon "salmon" SALMON Wildlife;
    Pufferfish "pufferfish" PUFFERFISH Wildlife;
    TropicalFish "tropical_fish" TROPICAL_FISH Wildlife;
    Guardian "guardian" GUARDIAN Boss;
    ElderGuardian "elder_guardian" ELDER_GUARDIAN Boss;
    Wither "wither" WITHER Boss;
    EnderDragon "ender_dragon" ENDER_DRAGON Boss;
    Warden "warden" WARDEN Boss;
    ArmorStand "armor_stand" ARMOR_STAND Decor;
    Mannequin "mannequin" MANNEQUIN Decor;
    Villager "villager" VILLAGER Village;
    WanderingTrader "wandering_trader" WANDERING_TRADER Village;
}

/// The default sounds of a kind (`minecraft:entity.<name>.<suffix>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultSound {
    Ambient,
    Hurt,
    Death,
}

pub use super::fam::Family;

/// Farm animal climate of a biome (`spawns_warm/cold_variant_farm_animals`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FarmClimate {
    #[default]
    Temperate,
    Warm,
    Cold,
}

impl Kind {
    /// The kind of an entity type, `None` for types without mob behaviour yet.
    pub fn of(t: EntityTypeId) -> Option<Kind> {
        static BY_TYPE: std::sync::OnceLock<Vec<Option<Kind>>> = std::sync::OnceLock::new();
        BY_TYPE
            .get_or_init(|| {
                let mut v = Vec::new();
                for &k in Kind::ALL {
                    let i = usize::from(k.type_id().0);
                    if v.len() <= i {
                        v.resize(i + 1, None);
                    }
                    v[i] = Some(k);
                }
                v
            })
            .get(usize::from(t.0))
            .copied()
            .flatten()
    }

    fn classic(self) -> bool {
        self.family() == Family::Classic
    }

    /// `AgeableMob`: animals grow up and breed.
    pub fn ageable(self) -> bool {
        if !self.classic() {
            return fam::ageable(self);
        }
        matches!(self, Kind::Cow | Kind::Pig | Kind::Sheep | Kind::Chicken)
    }

    /// A monster (`Monster` or `Enemy`), else an animal.
    /// The kind's class extends `Monster` (its `aiStep` runs `updateNoActionTime`).
    pub fn extends_monster(self) -> bool {
        matches!(
            self,
            Kind::Zombie
                | Kind::Husk
                | Kind::Drowned
                | Kind::ZombieVillager
                | Kind::ZombifiedPiglin
                | Kind::Giant
                | Kind::Skeleton
                | Kind::Stray
                | Kind::Bogged
                | Kind::Parched
                | Kind::WitherSkeleton
                | Kind::Creeper
                | Kind::Spider
                | Kind::CaveSpider
                | Kind::Enderman
                | Kind::Endermite
                | Kind::Silverfish
                | Kind::Blaze
                | Kind::Vex
                | Kind::Breeze
                | Kind::Creaking
                | Kind::Zoglin
                | Kind::Piglin
                | Kind::PiglinBrute
                | Kind::Guardian
                | Kind::ElderGuardian
                | Kind::Wither
                | Kind::Warden
        ) || self.is_raider()
    }

    /// `Raider`: illagers, witches and ravagers.
    pub fn is_raider(self) -> bool {
        matches!(
            self,
            Kind::Pillager
                | Kind::Vindicator
                | Kind::Evoker
                | Kind::Illusioner
                | Kind::Witch
                | Kind::Ravager
        )
    }

    pub fn is_monster(self) -> bool {
        if !self.classic() {
            return fam::is_monster(self);
        }
        matches!(
            self,
            Kind::Zombie | Kind::Skeleton | Kind::Creeper | Kind::Spider | Kind::Enderman
        )
    }

    /// `getHeadRotSpeed`: the head turn speed of `LookControl.setLookAt(x, y, z)`.
    pub fn head_rot_speed(self) -> f32 {
        match self {
            Kind::Frog => 35.0,
            Kind::Breeze => 25.0,
            _ => 10.0,
        }
    }

    /// The bosses (`requiresCustomPersistence`, never despawn): the wither and the ender
    /// dragon.
    pub fn is_boss(self) -> bool {
        matches!(self, Kind::Wither | Kind::EnderDragon)
    }

    /// `getAmbientSoundInterval`.
    pub fn ambient_sound_interval(self) -> i32 {
        if !self.classic() {
            return fam::ambient_sound_interval(self);
        }
        if self.is_monster() { 80 } else { 120 }
    }

    /// `instanceof AbstractHorse` (horses, donkeys, mules, llamas, camels).
    pub fn is_abstract_horse(self) -> bool {
        matches!(
            self,
            Kind::Horse
                | Kind::Donkey
                | Kind::Mule
                | Kind::SkeletonHorse
                | Kind::ZombieHorse
                | Kind::Llama
                | Kind::TraderLlama
                | Kind::Camel
                | Kind::CamelHusk
        )
    }

    /// `#burn_in_daylight`.
    pub fn burns_in_daylight(self) -> bool {
        if matches!(self, Kind::ZombieHorse | Kind::ZombieNautilus) {
            return true;
        }
        if !self.classic() {
            return fam::burns_in_daylight(self);
        }
        matches!(self, Kind::Zombie | Kind::Skeleton)
    }

    /// `#can_breathe_under_water` (the undead).
    pub fn breathes_underwater(self) -> bool {
        if !self.classic() {
            return fam::breathes_underwater(self);
        }
        matches!(self, Kind::Zombie | Kind::Skeleton)
    }

    /// `isSensitiveToWater`.
    pub fn sensitive_to_water(self) -> bool {
        if !self.classic() {
            return fam::sensitive_to_water(self);
        }
        self == Kind::Enderman
    }

    /// `#fall_damage_immune` and `Chicken.causeFallDamage`.
    pub fn fall_damage_immune(self) -> bool {
        if !self.classic() {
            return fam::fall_damage_immune(self);
        }
        self == Kind::Chicken
    }

    /// `EntityType.isAllowedInPeaceful`: all but the types built with `notInPeaceful()`
    /// (shulkers and piglins stay).
    pub fn allowed_in_peaceful(self) -> bool {
        super::spawn::allowed_in_peaceful(self.name())
    }

    /// `removeWhenFarAway`: animals stay.
    pub fn remove_when_far_away(self) -> bool {
        if !self.classic() {
            return fam::remove_when_far_away(self);
        }
        self.is_monster()
    }

    /// `setPathfindingMalus` of the kind.
    pub fn malus(self) -> &'static MalusTable {
        const ANIMAL: &MalusTable = &[(PathType::FireInNeighbor, 16.0), (PathType::Fire, -1.0)];
        const CHICKEN: &MalusTable = &[
            (PathType::FireInNeighbor, 16.0),
            (PathType::Fire, -1.0),
            (PathType::Water, 0.0),
        ];
        const ENDERMAN: &MalusTable = &[(PathType::Water, -1.0)];
        match self {
            Kind::Cow | Kind::Pig | Kind::Sheep => ANIMAL,
            Kind::Chicken => CHICKEN,
            Kind::Enderman => ENDERMAN,
            _ if !self.classic() => fam::malus(self),
            _ => &[],
        }
    }

    /// `getSoundSource`.
    pub fn sound_source(self) -> SoundSource {
        if !self.classic() {
            return fam::sound_source(self);
        }
        if self.is_monster() {
            SoundSource::Hostile
        } else {
            SoundSource::Neutral
        }
    }

    /// `getSoundVolume`.
    pub fn sound_volume(self) -> f32 {
        if !self.classic() {
            return fam::sound_volume(self);
        }
        if self == Kind::Cow { 0.4 } else { 1.0 }
    }

    /// `getAmbientSound` of the kind without state, `None` for silent kinds.
    pub fn ambient_sound(self) -> Option<&'static str> {
        match self {
            Kind::Creeper => None,
            _ => Some(self.default_sound(DefaultSound::Ambient)),
        }
    }

    /// `getHurtSound` of the kind without state.
    pub fn hurt_sound(self) -> &'static str {
        self.default_sound(DefaultSound::Hurt)
    }

    /// `getDeathSound` of the kind without state.
    pub fn death_sound(self) -> &'static str {
        self.default_sound(DefaultSound::Death)
    }

    /// `getFallSounds().small()`.
    pub fn fall_sound_small(self) -> &'static str {
        if self.is_monster() {
            "minecraft:entity.hostile.small_fall"
        } else {
            "minecraft:entity.generic.small_fall"
        }
    }

    /// `getFallSounds().big()`.
    pub fn fall_sound_big(self) -> &'static str {
        if self.is_monster() {
            "minecraft:entity.hostile.big_fall"
        } else {
            "minecraft:entity.generic.big_fall"
        }
    }

    /// `getBaseExperienceReward` without equipment: 5 for monsters, 1–3 for animals.
    pub fn base_experience_reward(self, random: &mut LegacyRandom) -> i32 {
        match self {
            _ if self.is_monster() => 5,
            _ => 1 + random.next_int_bounded(3),
        }
    }

    /// `Kind` by its id (`minecraft:zombie`).
    pub fn by_name(name: &str) -> Option<Kind> {
        let name = name.strip_prefix("minecraft:").unwrap_or(name);
        Kind::ALL.iter().copied().find(|k| k.name() == name)
    }
}

/// The state dependent sounds of a mob (`getAmbientSound`, `getHurtSound`, `getDeathSound`).
/// `getMaxHeadXRot`: the pitch limit of `LookControl.setLookAt(x, y, z)`.
pub fn max_head_x_rot(m: &MobData) -> f32 {
    match m.kind {
        Kind::Guardian | Kind::ElderGuardian | Kind::Shulker => 180.0,
        Kind::Axolotl | Kind::Dolphin => 1.0,
        Kind::Slime | Kind::MagmaCube | Kind::SulfurCube => 0.0,
        Kind::Wolf if m.in_sitting_pose => 20.0,
        _ => 40.0,
    }
}

/// `Mob.clampHeadRotationToBody`.
pub fn clamp_head_rotation_to_body(t: &mut Tick<'_, '_>) {
    let limit = max_head_y_rot(t.mob_ref());
    let body = t.mob_ref().y_body_rot;
    let mut rot = t.core().rot();
    let delta = vanillite_base::math::wrap_degrees_f(body - rot.y_head_rot);
    let target = delta.clamp(-limit, limit);
    rot.y_head_rot = rot.y_head_rot + delta - target;
    t.core_mut().set_rot(rot);
}

/// `getMaxHeadYRot`: how far the head turns from the body (`LookControl`,
/// `BodyRotationControl`).
pub fn max_head_y_rot(m: &MobData) -> f32 {
    match m.kind {
        Kind::Axolotl | Kind::Dolphin => 1.0,
        Kind::Armadillo if fam::brainy::armadillo::is_scared(m) => 0.0,
        Kind::Armadillo => 32.0,
        Kind::Camel | Kind::CamelHusk | Kind::Breeze => 30.0,
        Kind::Frog => 5.0,
        Kind::Sniffer => 50.0,
        Kind::Goat => 15.0,
        Kind::Shulker => 180.0,
        Kind::Ravager => 45.0,
        _ => 75.0,
    }
}

/// The entries of the `pig_sound_variant`, `cow_sound_variant` and `chicken_sound_variant`
/// registries in registry (network id) order; empty for other kinds.
pub fn sound_variant_names(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Pig => &["big", "classic", "mini"],
        Kind::Cow => &["classic", "moody"],
        Kind::Chicken => &["classic", "picky"],
        _ => &[],
    }
}

/// The registry index of the `classic` sound variant (the entity data default).
pub fn classic_sound_variant(kind: Kind) -> u8 {
    u8::from(kind == Kind::Pig)
}

/// `getSoundSet` of pigs, cows and chickens: the ambient, hurt, death and eat sound events of
/// the mob's sound variant (`entity.<kind>[_<variant>]`; babies of pigs and chickens use
/// `entity.baby_<kind>`).
fn farm_sound_set(m: &MobData) -> [&'static str; 4] {
    macro_rules! sets {
        ($key:expr; $($p:literal)*) => {
            match $key {
                $($p => [
                    concat!("minecraft:entity.", $p, ".ambient"),
                    concat!("minecraft:entity.", $p, ".hurt"),
                    concat!("minecraft:entity.", $p, ".death"),
                    concat!("minecraft:entity.", $p, ".eat"),
                ],)*
                _ => [
                    "minecraft:entity.pig.ambient",
                    "minecraft:entity.pig.hurt",
                    "minecraft:entity.pig.death",
                    "minecraft:entity.pig.eat",
                ],
            }
        };
    }
    let v = sound_variant_names(m.kind)
        .get(usize::from(m.sound_variant))
        .copied()
        .unwrap_or("classic");
    let prefix = match (m.kind, m.baby, v) {
        (Kind::Pig, true, _) => "baby_pig",
        (Kind::Chicken, true, _) => "baby_chicken",
        (Kind::Pig, _, "big") => "pig_big",
        (Kind::Pig, _, "mini") => "pig_mini",
        (Kind::Cow, _, "moody") => "cow_moody",
        (Kind::Cow, ..) => "cow",
        (Kind::Chicken, _, "picky") => "chicken_picky",
        (Kind::Chicken, ..) => "chicken",
        _ => "pig",
    };
    sets!(prefix; "pig" "pig_big" "pig_mini" "baby_pig" "cow" "cow_moody" "chicken"
        "chicken_picky" "baby_chicken")
}

/// The sound event of a pig, cow or chicken of its sound variant.
pub fn farm_sound(m: &MobData, sound: DefaultSound) -> &'static str {
    let set = farm_sound_set(m);
    match sound {
        DefaultSound::Ambient => set[0],
        DefaultSound::Hurt => set[1],
        DefaultSound::Death => set[2],
    }
}

/// `playEatingSound` of a pig: the eat sound of its sound variant.
pub fn pig_eat_sound(m: &MobData) -> &'static str {
    farm_sound_set(m)[3]
}

pub fn ambient_sound(m: &mut MobData) -> Option<&'static str> {
    if !m.kind.classic() {
        return fam::ambient_sound(m);
    }
    if matches!(m.kind, Kind::Pig | Kind::Cow | Kind::Chicken) {
        return Some(farm_sound(m, DefaultSound::Ambient));
    }
    if m.kind == Kind::Enderman && classic::enderman(m).creepy {
        return Some("minecraft:entity.enderman.scream");
    }
    m.kind.ambient_sound()
}

/// `getHurtSound` of a mob.
pub fn hurt_sound(m: &MobData) -> &'static str {
    if matches!(m.kind, Kind::Pig | Kind::Cow | Kind::Chicken) {
        farm_sound(m, DefaultSound::Hurt)
    } else if m.kind.classic() {
        m.kind.hurt_sound()
    } else {
        fam::hurt_sound(m)
    }
}

/// `getDeathSound` of a mob.
pub fn death_sound(m: &MobData) -> &'static str {
    if matches!(m.kind, Kind::Pig | Kind::Cow | Kind::Chicken) {
        farm_sound(m, DefaultSound::Death)
    } else if m.kind.classic() {
        m.kind.death_sound()
    } else {
        fam::death_sound(m)
    }
}

/// `getBaseExperienceReward` of a mob.
pub fn experience_reward(m: &mut MobData) -> i32 {
    // `Zombie.getBaseExperienceReward`: babies give 2.5 times, plus the equipment bonus.
    if m.kind == Kind::Zombie {
        return fam::undead::zombies::experience_reward(m);
    }
    if m.kind.classic() {
        if m.kind.is_monster() {
            // `Monster.xpReward` 5 with `Mob.getBaseExperienceReward`'s equipment bonus.
            return fam::undead::zombies::equipment_experience(m, 5);
        }
        // `Chicken.getBaseExperienceReward`: 10 for a chicken jockey.
        if m.kind == Kind::Chicken && fam::classic::chicken(m).jockey {
            return 10;
        }
        let kind = m.kind;
        kind.base_experience_reward(&mut m.random)
    } else {
        fam::experience_reward(m)
    }
}

/// The dimensions of a mob: its family's when they depend on state, else the type's (babies
/// at half size).
pub fn mob_dimensions(m: &MobData) -> Dimensions {
    let d = unscaled_dimensions(m);
    // `getDimensions(pose).scale(getScale())` with the `minecraft:scale` attribute.
    let scale = crate::effect::attribute_map::scale_of(&m.effects.modifiers);
    if scale == 1.0 {
        return d;
    }
    #[allow(clippy::cast_possible_truncation)]
    let s = scale as f32;
    Dimensions {
        width: d.width * s,
        height: d.height * s,
    }
}

/// [`mob_dimensions`] without the `minecraft:scale` attribute.
fn unscaled_dimensions(m: &MobData) -> Dimensions {
    if m.kind == Kind::Zombie {
        // `Zombie.BABY_DIMENSIONS` of the classic zombie.
        if let Some(d) = fam::undead::zombies::dimensions(m) {
            return d;
        }
    }
    if m.baby
        && let Some((width, height, _)) = baby_dimensions(m.kind)
    {
        return Dimensions { width, height };
    }
    fam::dimensions(m).unwrap_or_else(|| dimensions(m.kind.type_id(), m.baby))
}

/// The `BABY_DIMENSIONS` of the kinds that define their own (width, height, eye height): a
/// fixed size, or the adult's scaled (`scale` scales the eye height too) with an eye height of
/// its own where Vanilla sets one.
pub fn baby_dimensions(kind: Kind) -> Option<(f32, f32, f32)> {
    let fixed = |w: f32, h: f32, eye: f32| Some((w, h, eye));
    let scaled = |scale: f32, eye: Option<f32>| {
        let info = kind.type_id().info();
        let d = Dimensions::of(kind.type_id());
        Some((
            d.width * scale,
            d.height * scale,
            eye.unwrap_or(info.eye_height * scale),
        ))
    };
    match kind {
        Kind::Axolotl => fixed(0.375, 0.21, 0.093_75),
        Kind::Chicken => fixed(0.3, 0.4, 0.281_25),
        Kind::Cow | Kind::Mooshroom => fixed(0.45, 0.7, 0.69),
        Kind::Cat | Kind::Ocelot => fixed(0.3, 0.35, 0.343_75),
        Kind::Goat => fixed(0.45, 0.65, 0.593_75),
        Kind::Pig => fixed(0.45, 0.45, 0.406_25),
        Kind::PolarBear => fixed(0.7, 0.7, 0.343_75),
        Kind::Rabbit => fixed(0.24, 0.4, 0.39),
        Kind::Sheep => fixed(0.45, 0.65, 0.656_25),
        Kind::Squid | Kind::GlowSquid => fixed(0.5, 0.5, 0.37),
        Kind::Wolf => fixed(0.3, 0.425, 0.343_75),
        Kind::Strider => fixed(0.45, 0.85, 0.4375),
        Kind::Hoglin | Kind::Zoglin => fixed(0.75, 0.85, 0.625),
        Kind::Piglin => fixed(0.49, 0.98, 0.78),
        Kind::Villager => fixed(0.49, 0.98, 0.63),
        Kind::Dolphin => scaled(0.65, Some(0.093_75)),
        Kind::Horse | Kind::SkeletonHorse | Kind::ZombieHorse => scaled(0.7, None),
        Kind::Llama | Kind::TraderLlama => scaled(0.5, None),
        Kind::Fox => scaled(0.6, Some(0.343_75)),
        Kind::HappyGhast => scaled(0.2375, Some(0.468_75)),
        Kind::Nautilus => scaled(0.5, None),
        Kind::Panda => scaled(0.5, Some(0.281_25)),
        Kind::Turtle => scaled(0.3, None),
        Kind::Armadillo => scaled(0.6, Some(0.218_75)),
        _ => None,
    }
}

/// The dimensions of a mob type (babies at half size).
pub fn dimensions(kind: EntityTypeId, baby: bool) -> Dimensions {
    let d = Dimensions::of(kind);
    if baby {
        Dimensions {
            width: d.width * 0.5,
            height: d.height * 0.5,
        }
    } else {
        d
    }
}

/// `registerGoals` of each kind.
pub fn register_goals(m: &mut MobData) {
    if !m.kind.classic() {
        fam::init(m);
        fam::register_goals(m);
        return;
    }
    let g = &mut m.goals;
    let tg = &mut m.targets;
    match m.kind {
        // `Zombie.registerGoals`/`addBehaviourGoals` and `Spider.registerGoals`, shared with
        // the other zombies and the cave spider.
        Kind::Zombie => fam::undead::zombies::register_goals(m),
        Kind::Spider => {
            m.nav.kind = super::nav::NavKind::WallClimber;
            fam::undead::arthropods::register_goals(m);
        }
        Kind::Skeleton => {
            g.add(2, Goal::RestrictSun);
            g.add(
                3,
                Goal::FleeSun {
                    speed: 1.0,
                    wanted: Vec3::ZERO,
                },
            );
            g.add(
                3,
                Goal::Common(super::common::CommonGoal::avoid(
                    |t, id| super::common::mob_of(t, id, Kind::Wolf),
                    6.0,
                    1.0,
                    1.2,
                )),
            );
            g.add(5, Goal::stroll(1.0));
            g.add(6, Goal::look_at_player(8.0));
            g.add(6, Goal::random_look());
            tg.add(1, Goal::hurt_by(false));
            tg.add(2, Goal::nearest_player(true));
            tg.add(
                3,
                Goal::Common(super::common::CommonGoal::nearest(
                    10,
                    true,
                    super::common::iron_golem,
                )),
            );
            tg.add(
                3,
                Goal::Common(super::common::CommonGoal::nearest(
                    10,
                    true,
                    super::common::baby_turtle_on_land,
                )),
            );
            // The constructor's `reassessWeaponGoal` (no bow yet): the melee goal.
            g.add(4, fam::undead::skeletons::melee_goal());
        }
        Kind::Creeper => {
            g.add(1, Goal::Float);
            g.add(2, Goal::Swell);
            for kind in [Kind::Ocelot, Kind::Cat] {
                let filter: super::common::TargetFilter = match kind {
                    Kind::Ocelot => |t, id| super::common::mob_of(t, id, Kind::Ocelot),
                    _ => |t, id| super::common::mob_of(t, id, Kind::Cat),
                };
                g.add(
                    3,
                    Goal::Common(super::common::CommonGoal::avoid(filter, 6.0, 1.0, 1.2)),
                );
            }
            g.add(4, Goal::melee(MeleeKind::Plain, 1.0, false));
            g.add(5, Goal::stroll(0.8));
            g.add(6, Goal::look_at_player(8.0));
            g.add(6, Goal::random_look());
            tg.add(1, Goal::nearest_player(true));
            tg.add(2, Goal::hurt_by(false));
            m.nav.can_float = true;
        }
        Kind::Enderman => {
            g.add(0, Goal::Float);
            g.add(1, Goal::EndermanFreeze { target: None });
            g.add(2, Goal::melee(MeleeKind::Plain, 1.0, false));
            g.add(7, Goal::stroll_with(1.0, 0.0));
            g.add(8, Goal::look_at_player(8.0));
            g.add(8, Goal::random_look());
            g.add(10, Goal::EndermanLeaveBlock);
            g.add(11, Goal::EndermanTakeBlock);
            tg.add(
                1,
                Goal::EndermanStare {
                    state: super::goals::TargetState {
                        must_see: false,
                        unseen_ticks: 0,
                        unseen_memory: 60,
                        target_mob: None,
                    },
                    pending: None,
                    aggro_time: 0,
                    teleport_time: 0,
                },
            );
            tg.add(2, Goal::hurt_by(false));
            tg.add(
                3,
                Goal::Common(super::common::CommonGoal::nearest(10, true, |t, id| {
                    super::common::mob_of(t, id, Kind::Endermite)
                })),
            );
            tg.add(
                4,
                Goal::Common(super::common::CommonGoal::ResetUniversalAnger {
                    alert_same_type: false,
                    last_hurt: 0,
                }),
            );
            m.nav.can_float = true;
        }
        Kind::Cow | Kind::Pig | Kind::Sheep | Kind::Chicken => {
            use super::goals::TemptItems;
            let follow = |speed| Goal::FollowParent {
                speed,
                parent: None,
                recalc: 0,
            };
            g.add(0, Goal::Float);
            match m.kind {
                Kind::Cow => {
                    g.add(1, Goal::panic(2.0));
                    g.add(2, Goal::breed(1.0));
                    g.add(3, Goal::tempt(1.25, TemptItems::Food));
                    g.add(4, follow(1.25));
                    g.add(5, Goal::stroll(1.0));
                    g.add(6, Goal::look_at_player(6.0));
                    g.add(7, Goal::random_look());
                }
                Kind::Pig => {
                    g.add(1, Goal::panic(1.25));
                    g.add(3, Goal::breed(1.0));
                    g.add(4, Goal::tempt(1.2, TemptItems::CarrotOnAStick));
                    g.add(4, Goal::tempt(1.2, TemptItems::Food));
                    g.add(5, follow(1.1));
                    g.add(6, Goal::stroll(1.0));
                    g.add(7, Goal::look_at_player(6.0));
                    g.add(8, Goal::random_look());
                }
                Kind::Sheep => {
                    g.add(1, Goal::panic(1.25));
                    g.add(2, Goal::breed(1.0));
                    g.add(3, Goal::tempt(1.1, TemptItems::Food));
                    g.add(4, follow(1.1));
                    g.add(5, Goal::EatBlock { tick: 0 });
                    g.add(6, Goal::stroll(1.0));
                    g.add(7, Goal::look_at_player(6.0));
                    g.add(8, Goal::random_look());
                }
                _ => {
                    g.add(1, Goal::panic(1.4));
                    g.add(2, Goal::breed(1.0));
                    g.add(3, Goal::tempt(1.0, TemptItems::Food));
                    g.add(4, follow(1.1));
                    g.add(5, Goal::stroll(1.0));
                    g.add(6, Goal::look_at_player(6.0));
                    g.add(7, Goal::random_look());
                }
            }
            m.nav.can_float = true;
        }
        _ => {}
    }
    // Goals are kept sorted by insertion like Vanilla's linked set; nothing to do here.
}

/// `finalizeSpawn`: the random spawn bonus, babies, colours and variants. Vanilla draws them
/// from the level's random (`ServerLevelAccessor.getRandom()`), passed as `random`: while it
/// runs, the mob's `random` is that random.
pub fn finalize_spawn(
    m: &mut MobData,
    ctx: &mut fam::SpawnContext<'_>,
    random: &mut vanillite_base::random::LegacyRandom,
) {
    if !m.kind.classic() {
        fam::finalize_spawn_own(m);
    }
    std::mem::swap(&mut m.random, random);
    ctx.own_state = random.state();
    finalize_spawn_inner(m, ctx);
    if ctx.own_state != random.state() {
        *random = vanillite_base::random::LegacyRandom::from_state(ctx.own_state);
    }
    std::mem::swap(&mut m.random, random);
}

fn finalize_spawn_inner(m: &mut MobData, ctx: &mut fam::SpawnContext<'_>) {
    if m.kind == Kind::CaveSpider
        || m.kind == Kind::Axolotl && ctx.reason == fam::SpawnReason::Bucket
    {
        // `CaveSpider.finalizeSpawn` calls no `super`: not even `Mob.finalizeSpawn`; nor does
        // `Axolotl.finalizeSpawn` for a mob from a bucket.
        return;
    }
    // `Mob.finalizeSpawn`: follow range bonus and left-handedness; the kinds that call
    // `super.finalizeSpawn` after their own draws run it last.
    let mob_part = |m: &mut MobData| {
        let bonus = m.random.triangle(0.0, 0.114_850_000_000_000_01);
        m.attrs.follow_range *= 1.0 + bonus;
        m.spawn_bonus = bonus;
        m.left_handed = m.random.next_float() < 0.05;
    };
    if !m.kind.classic() {
        let last = fam::mob_spawn_part_last(m.kind);
        if !last {
            mob_part(m);
        }
        fam::finalize_spawn(m, ctx);
        if last {
            mob_part(m);
        }
        return;
    }
    let (difficulty, climate, group_index) = (ctx.difficulty, ctx.climate, ctx.group_index);
    if matches!(m.kind, Kind::Cow | Kind::Pig | Kind::Sheep | Kind::Chicken) {
        // `Cow`, `Pig` and `Chicken.finalizeSpawn`: the variant of the biome
        // (`selectVariantToSpawn` picks one of the matching entries: one draw) and a random
        // sound variant; `Sheep.finalizeSpawn`: the colour. Then `AgeableMob.finalizeSpawn`
        // (5 % babies after the first of a group) and `Mob.finalizeSpawn` last.
        if m.kind == Kind::Sheep {
            let color = sheep_color(climate, &mut m.random);
            classic::sheep_mut(m).color = color;
        } else {
            let _ = m.random.next_int_bounded(1);
            m.variant = match climate {
                FarmClimate::Cold => 0,
                FarmClimate::Temperate => 1,
                FarmClimate::Warm => 2,
            };
            // `*SoundVariants.pickRandomSoundVariant`: any entry of the registry.
            let sounds = sound_variant_names(m.kind).len();
            let i = m
                .random
                .next_int_bounded(i32::try_from(sounds).unwrap_or(1));
            m.sound_variant = u8::try_from(i).unwrap_or(0);
        }
        if group_index > 0 && m.random.next_float() <= 0.05 {
            m.age = -24000;
            m.baby = true;
        }
        mob_part(m);
        return;
    }
    mob_part(m);
    match m.kind {
        Kind::Zombie => {
            // `Zombie.finalizeSpawn`, shared with the zombie variants.
            fam::undead::zombies::finalize_spawn(m, ctx);
        }
        Kind::Skeleton => {
            // `populateDefaultEquipmentSlots`: armour and the bow.
            equipment::populate_default_armor(m, difficulty, ctx.special_multiplier);
            m.equipment.set(
                crate::item::EquipmentSlot::MainHand,
                equipment::stack("minecraft:bow"),
            );
            equipment::populate_default_enchantments(m, ctx.special_multiplier);
            fam::undead::skeletons::reassess_weapon_goal(m, difficulty);
            m.can_pick_up_loot = m.random.next_float() < 0.55 * ctx.special_multiplier;
            halloween_head(m);
        }
        Kind::Spider => spider_finalize(m, ctx),
        _ => {}
    }
}

/// `Spider.finalizeSpawn` (after `Mob.finalizeSpawn`): 1 % carry a skeleton, and on hard the
/// first spider of a group may choose a lasting effect for the whole group.
fn spider_finalize(m: &mut MobData, ctx: &mut fam::SpawnContext<'_>) {
    use crate::effect::{EffectInstance, MobEffect};
    if m.random.next_int_bounded(100) == 0 {
        let seed = crate::mob::new_entity_seed();
        let mut rider = MobData::new(Kind::Skeleton, seed);
        let mut inner = fam::SpawnContext {
            group: fam::GroupData::None,
            group_index: 0,
            extra: Vec::new(),
            ride: None,
            ..*ctx
        };
        finalize_spawn(&mut rider, &mut inner, &mut m.random);
        ctx.extra.push(fam::ExtraMob {
            mob: rider,
            offset: Vec3::ZERO,
            relation: fam::Relation::RidesIt,
        });
    }
    let effect = if let fam::GroupData::Spider(effect) = ctx.group {
        effect
    } else {
        let mut effect = None;
        if ctx.difficulty == Difficulty::Hard
            && m.random.next_float() < 0.1 * ctx.special_multiplier
        {
            // `SpiderEffectsGroupData.setRandomEffect`.
            effect = Some(match m.random.next_int_bounded(5) {
                0 | 1 => MobEffect::SPEED,
                2 => MobEffect::STRENGTH,
                3 => MobEffect::REGENERATION,
                _ => MobEffect::INVISIBILITY,
            });
        }
        ctx.group = fam::GroupData::Spider(effect);
        effect
    };
    if let Some(effect) = effect {
        m.effects.force_add(EffectInstance::new(effect, -1, 0));
    }
}

/// `Zombie.finalizeSpawn`'s chicken jockey of a baby: 5 % ride a free chicken nearby, else
/// 5 % a new chicken.
pub fn zombie_chicken_jockey(m: &mut MobData, ctx: &mut fam::SpawnContext<'_>) {
    if m.random.next_float() < 0.05 {
        let p = ctx.pos;
        let center = Vec3::new(f64::from(p.x) + 0.5, f64::from(p.y), f64::from(p.z) + 0.5);
        let area = vanillite_base::Aabb::new(
            center.x - 0.3 - 5.0,
            center.y - 3.0,
            center.z - 0.3 - 5.0,
            center.x + 0.3 + 5.0,
            center.y + 1.95 + 3.0,
            center.z + 0.3 + 5.0,
        );
        if let Some(level) = ctx.level {
            let ents = &level.entities;
            let mut found = None;
            ents.store.for_each_in(&area, false, |id| {
                if found.is_none()
                    && ents.mob(id).is_some_and(|c| c.kind == Kind::Chicken)
                    && ents.riding.passengers_of(id).is_empty()
                {
                    found = Some(id);
                }
            });
            if let Some(chicken) = found {
                ctx.ride = Some(chicken);
            }
        }
    } else if m.random.next_float() < 0.05 {
        let seed = crate::mob::new_entity_seed();
        let mut chicken = MobData::new(Kind::Chicken, seed);
        let mut inner = fam::SpawnContext {
            reason: fam::SpawnReason::Jockey,
            extra: Vec::new(),
            ride: None,
            ..*ctx
        };
        finalize_spawn(&mut chicken, &mut inner, &mut m.random);
        classic::chicken_mut(&mut chicken).jockey = true;
        ctx.extra.push(fam::ExtraMob {
            mob: chicken,
            offset: Vec3::ZERO,
            relation: fam::Relation::CarriesIt,
        });
    }
}

/// The pumpkin heads of Halloween (`SpecialDates.isHalloween`): 25 % without a helmet, 10 %
/// of them a jack o'lantern; they never drop.
pub fn halloween_head(m: &mut MobData) {
    use crate::item::EquipmentSlot;
    if m.equipment.get(EquipmentSlot::Head).is_empty()
        && fam::is_halloween()
        && m.random.next_float() < 0.25
    {
        let head = if m.random.next_float() < 0.1 {
            "minecraft:jack_o_lantern"
        } else {
            "minecraft:carved_pumpkin"
        };
        m.equipment.set(EquipmentSlot::Head, equipment::stack(head));
        m.equipment.set_drop_chance(EquipmentSlot::Head, 0.0);
    }
}

/// `SheepColorSpawnRules.getSheepColor`.
fn sheep_color(climate: FarmClimate, random: &mut LegacyRandom) -> u8 {
    const WHITE: u8 = 0;
    const LIGHT_GRAY: u8 = 8;
    const GRAY: u8 = 7;
    const BLACK: u8 = 15;
    const BROWN: u8 = 12;
    const PINK: u8 = 6;
    let (singles, common): ([(u8, i32); 4], u8) = match climate {
        FarmClimate::Temperate => ([(BLACK, 5), (GRAY, 5), (LIGHT_GRAY, 5), (BROWN, 3)], WHITE),
        FarmClimate::Warm => ([(GRAY, 5), (LIGHT_GRAY, 5), (WHITE, 5), (BLACK, 3)], BROWN),
        FarmClimate::Cold => ([(LIGHT_GRAY, 5), (GRAY, 5), (WHITE, 5), (BROWN, 3)], BLACK),
    };
    let mut roll = random.next_int_bounded(100);
    for (color, weight) in singles {
        if roll < weight {
            return color;
        }
        roll -= weight;
    }
    if random.next_int_bounded(500) < 499 {
        common
    } else {
        PINK
    }
}

/// `getWalkTargetValue(pos)` of a kind.
pub fn walk_target_value(kind: Kind, level: &Level, env: &Environment, pos: BlockPos) -> f64 {
    fam::walk_target_value(kind, level, env, pos)
}

/// `Monster.getWalkTargetValue` (`monster`) or `Animal.getWalkTargetValue`.
pub fn base_walk_target_value(
    monster: bool,
    level: &Level,
    env: &Environment,
    pos: BlockPos,
) -> f64 {
    let cost = f64::from(light_magic_value(level, env, pos) - 0.5);
    if monster {
        -cost
    } else if level.block(BlockPos::new(pos.x, pos.y - 1, pos.z)).block() == ids::GRASS_BLOCK {
        10.0
    } else {
        cost
    }
}

/// Plays a sound of the ticking mob (`playSound(sound, volume, pitch)`).
pub fn play_sound(t: &mut Tick<'_, '_>, sound: &str, volume: f32, pitch: f32) {
    if t.mob_ref().silent {
        return;
    }
    let pos = t.pos();
    let source = t.mob_ref().sound_source();
    let seed = crate::survival::sound_seed();
    crate::survival::play_sound(t.env.out, None, pos, sound, source, volume, pitch, seed);
}

/// `playAmbientSound`.
pub fn play_ambient_sound(t: &mut Tick<'_, '_>) {
    if t.mob_ref().kind == Kind::Warden {
        super::boss::warden::play_ambient_sound(t);
        return;
    }
    if t.mob_ref().kind == Kind::Parrot {
        let sound = super::fam::companion::parrot::ambient_sound(t);
        let id = t.id;
        combat::mob_sound(t.ents, t.env, id, sound);
        return;
    }
    if let Some(sound) = ambient_sound(t.mob()) {
        let id = t.id;
        combat::mob_sound(t.ents, t.env, id, sound);
    }
}

/// Kind specific work before `LivingEntity.tick` (`Creeper.tick`'s fuse, using a bow).
pub fn before_tick(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().kind.classic() {
        fam::before_tick(t);
        return;
    }
    match t.mob_ref().kind {
        Kind::Creeper if t.mob_ref().is_alive() => {
            let c = classic::creeper_mut(t.mob());
            c.old_swell = c.swell;
            if c.ignited {
                c.swell_dir = 1;
            }
            let (dir, swell) = (c.swell_dir, c.swell);
            if dir > 0 && swell == 0 {
                play_sound(t, "minecraft:entity.creeper.primed", 1.0, 0.5);
                // `gameEvent(GameEvent.PRIME_FUSE)`.
                let (event, pos) = (crate::level::game_event::events().prime_fuse, t.pos());
                super::living::post_game_event(t, event, pos, None);
            }
            let c = classic::creeper_mut(t.mob());
            c.swell = (c.swell + dir).max(0);
            if c.swell >= c.max_swell {
                c.swell = c.max_swell;
                explode_creeper(t);
            }
        }
        Kind::Skeleton => {
            // `Skeleton.tick`: the powder snow conversion into a stray first.
            fam::undead::skeletons::freezing_tick(t);
            if t.mob_ref().removed {
                return;
            }
            fam::undead::skeletons::reassess_if_due(t);
            let m = t.mob();
            if m.using_item >= 0 {
                m.using_item += 1;
            }
        }
        _ => {}
    }
}

/// `Creeper.explodeCreeper`.
fn explode_creeper(t: &mut Tick<'_, '_>) {
    let m = t.mob();
    let c = classic::creeper(m);
    let multiplier = if c.powered { 2.0 } else { 1.0 };
    #[allow(clippy::cast_precision_loss)]
    let radius = c.explosion_radius as f32 * multiplier;
    m.dead = true;
    m.removed = true;
    let pos = t.pos();
    let id = t.id;
    t.cx.actions.push(WorldAction::Explode(ExplosionRequest {
        source: Some(id),
        indirect: None,
        pos,
        radius,
        mob: true,
        resistance: crate::level::explosion::BlockResistance::Default,
        fire: false,
    }));
    // `spawnLingeringCloud`: a cloud with a copy of every active effect.
    let effects: Vec<_> = t.mob_ref().effects.iter().cloned().collect();
    if !effects.is_empty() {
        let (radius, duration) = (2.5_f32, 300);
        #[allow(clippy::cast_precision_loss)]
        let data = crate::effect::cloud::CloudData {
            radius,
            radius_on_use: -0.5,
            wait_time: 10,
            duration,
            duration_scale: 0.25,
            radius_per_tick: -radius / duration as f32,
            contents: crate::effect::potion::PotionContents {
                custom_effects: effects,
                ..Default::default()
            },
            ..Default::default()
        };
        t.cx.actions
            .push(WorldAction::SpawnCloud(pos, Box::new(data)));
    }
    // `triggerOnDeathMobEffects(KILLED)`.
    crate::effect::living::on_mob_killed(t);
}

/// Kind specific work after `LivingEntity.tick` (`Spider.tick`'s climbing flag).
pub fn after_tick(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().kind.classic() {
        fam::after_tick(t);
        return;
    }
    if t.mob_ref().kind == Kind::Spider {
        let climbing = t.mob_ref().horizontal_collision;
        if climbing != t.mob_ref().climbing {
            t.mob().climbing = climbing;
            t.core_mut().mark_data_dirty();
        }
    }
    if t.mob_ref().kind == Kind::Zombie {
        // `Zombie.tick`: the drowning conversion into a drowned.
        fam::undead::zombies::drowning_tick(t);
    }
}

/// `Chicken.aiStep`: the egg timer of an adult that is no jockey; the egg comes from the gift
/// loot table `gameplay/chicken_lay` (by the chicken's variant).
fn lay_egg(t: &mut Tick<'_, '_>) {
    let m = t.mob_ref();
    if !m.is_alive() || m.baby || fam::classic::chicken(m).jockey {
        return;
    }
    let c = fam::classic::chicken_mut(t.mob());
    c.egg_time -= 1;
    if c.egg_time > 0 {
        return;
    }
    let items = fam::brainy::gift_loot(t, "minecraft:gameplay/chicken_lay");
    if !items.is_empty() {
        let uuid = t.core().uuid();
        for stack in items {
            let item = crate::item::item_id(&stack).name();
            if t.env.hooks.entity_drop_item(
                crate::hooks::HookWorld::split(t.level, t.ents),
                uuid,
                "minecraft:chicken",
                item,
            ) {
                super::equipment::spawn_at_location(t, stack, 0.0);
            }
        }
        let r = &mut t.mob().random;
        let pitch = (r.next_float() - r.next_float()) * 0.2 + 1.0;
        play_sound(t, "minecraft:entity.chicken.egg", 1.0, pitch);
        let event = crate::level::game_event::events().entity_place;
        let pos = t.pos();
        crate::survival::game_event(t.env.out, event, pos, Some(t.id));
    }
    let egg_time = t.mob().random.next_int_bounded(6000) + 6000;
    fam::classic::chicken_mut(t.mob()).egg_time = egg_time;
}

/// Kind specific parts of `aiStep` (chicken flapping, animal ageing).
pub fn ai_step(t: &mut Tick<'_, '_>) {
    let kind = t.mob_ref().kind;
    if kind == Kind::Chicken {
        let delta = if t.on_ground() { -1.0 } else { 4.0 } * 0.3;
        let c = fam::classic::chicken_mut(t.mob());
        c.flap_speed = (c.flap_speed + delta).clamp(0.0, 1.0);
        let v = t.velocity();
        if !t.on_ground() && v.y < 0.0 {
            t.set_velocity(v.multiply(1.0, 0.6, 1.0));
        }
        lay_egg(t);
    }
    if kind.ageable() && t.mob_ref().forced_age_timer > 0 {
        // `AgeableMob.aiStep`: a happy villager particle at a random spot every 4 ticks.
        if t.mob_ref().forced_age_timer % 4 == 0 {
            let (pos, dims) = (t.pos(), t.core().dims());
            let (width, height) = (f64::from(dims.width), f64::from(dims.height));
            let r = &mut t.mob().random;
            let at = Vec3::new(
                pos.x + width * (2.0 * r.next_double() - 1.0),
                pos.y + height * r.next_double() + 0.5,
                pos.z + width * (2.0 * r.next_double() - 1.0),
            );
            let particle = crate::level::particles::simple("minecraft:happy_villager");
            t.cx.send_particles(particle, at, 1, Vec3::ZERO, 0.0);
        }
        t.mob().forced_age_timer -= 1;
    }
    if kind.ageable() && t.mob_ref().is_alive() {
        // `AgeableMob.aiStep` (`canAgeUp`: not age locked) and `Animal.aiStep`.
        let m = t.mob();
        // `ZombieHorse` and `SkeletonHorse.canAgeUp` are false.
        let grows = !matches!(m.kind, Kind::ZombieHorse | Kind::SkeletonHorse);
        if m.age < 0 && !m.age_locked && grows {
            m.age += 1;
        } else if m.age > 0 {
            m.age -= 1;
        }
        super::age_lock::tick(t);
        let m = t.mob();
        if m.age != 0 {
            m.in_love = 0;
        }
        if m.in_love > 0 {
            m.in_love -= 1;
            if m.in_love % 10 == 0 {
                // `Animal.aiStep`: the heart particle is the client's, but its speed and
                // position are drawn on the server too.
                let r = &mut m.random;
                let _ = (r.next_gaussian(), r.next_gaussian(), r.next_gaussian());
                let _ = (r.next_double(), r.next_double(), r.next_double());
            }
        }
        // `ItemBasedSteering.tickBoost`.
        if m.boosting {
            m.boost_time += 1;
            if m.boost_time > m.boost_total {
                m.boosting = false;
            }
        }
        let baby = m.age < 0;
        if baby != m.baby {
            m.baby = baby;
            let dims = mob_dimensions(t.mob_ref());
            t.core_mut().set_dims(dims);
            t.core_mut().mark_data_dirty();
        }
    }
    if !kind.classic() {
        fam::ai_step(t);
    }
}

/// `customServerAiStep` (enderman teleports in daylight).
pub fn custom_server_ai_step(t: &mut Tick<'_, '_>) {
    if !t.mob_ref().kind.classic() {
        fam::custom_server_ai_step(t);
        return;
    }
    if t.mob_ref().kind != Kind::Enderman {
        return;
    }
    if t.cx.env.is_bright_outside()
        && t.mob_ref().tick_count >= classic::enderman(t.mob_ref()).target_change_time + 600
    {
        let br = t.light_magic_value();
        let sky = super::can_see_sky(t.level, t.block_pos());
        let roll = t.mob().random.next_float() * 30.0;
        if br > 0.5 && sky && roll < (br - 0.4) * 2.0 {
            t.mob().target = None;
            super::goals::enderman_set_target(t, None);
            let id = t.id;
            teleport_randomly(t.level, t.ents, t.env, t.cx, id);
        }
    }
}

/// `Creeper.causeFallDamage`: a fall shortens the fuse.
pub fn on_fall_damage(t: &mut Tick<'_, '_>, distance: f64) {
    if !t.mob_ref().kind.classic() {
        fam::on_fall_damage(t, distance);
        return;
    }
    if t.mob_ref().kind == Kind::Creeper {
        let c = classic::creeper_mut(t.mob());
        #[allow(clippy::cast_possible_truncation)]
        let add = (distance * 1.5) as i32;
        c.swell += add;
        if c.swell > c.max_swell - 5 {
            c.swell = c.max_swell - 5;
        }
    }
}

/// `Zombie.doHurtTarget`: a burning zombie sets its target on fire.
pub fn after_attack(t: &mut Tick<'_, '_>, target: EntityId) {
    if !t.mob_ref().kind.classic() {
        fam::after_attack(t, target);
        return;
    }
    if t.mob_ref().kind != Kind::Zombie {
        return;
    }
    // `Zombie.doHurtTarget`: a burning zombie with an empty main hand sets its target on fire
    // (chance and seconds from the local effective difficulty).
    let empty = t
        .mob_ref()
        .equipment
        .get(crate::item::EquipmentSlot::MainHand)
        .is_empty();
    if !empty || !t.mob_ref().is_on_fire() {
        return;
    }
    let diff = crate::level::difficulty::at(t.level, t.block_pos()).effective;
    if t.mob().random.next_float() < diff * 0.3 {
        #[allow(clippy::cast_possible_truncation)]
        super::attack::ignite(t.ents, target, 2 * diff as i32 * 20);
    }
}

/// `#edible_for_sheep`.
pub fn edible_for_sheep(state: BlockState) -> bool {
    matches!(
        state.block().name(),
        "minecraft:short_grass"
            | "minecraft:short_dry_grass"
            | "minecraft:tall_dry_grass"
            | "minecraft:fern"
    )
}

/// `EatBlockGoal.tick` at the bite: grass becomes dirt, tall grass disappears.
pub fn eat_block(t: &mut Tick<'_, '_>) {
    let pos = t.block_pos();
    let griefing = t.env.rules.bool(Rule::MOB_GRIEFING);
    if edible_for_sheep(t.level.block(pos)) {
        if griefing {
            t.cx.actions.push(WorldAction::DestroyBlock(pos, false));
        }
    } else {
        let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
        if t.level.block(below).block() != ids::GRASS_BLOCK {
            return;
        }
        if griefing {
            let grass = i32::from(ids::GRASS_BLOCK.default_state().0);
            t.cx.actions
                .push(WorldAction::LevelEvent(2001, below, grass));
            t.cx.actions
                .push(WorldAction::SetBlock(below, ids::DIRT.default_state(), 2));
        }
    }
    // `Sheep.ate`: `Mob.ate` (the eat game event), the wool grows back, babies grow up.
    let at = t.pos();
    super::living::post_game_event(t, crate::level::game_event::events().eat, at, None);
    let m = t.mob();
    m.sheared = false;
    if m.baby && !m.age_locked {
        m.age = (m.age + 60 * 20).min(0);
    }
    t.core_mut().mark_data_dirty();
}

/// `Mob.lookAt(entity, yMax, xMax)`: turns the body.
pub fn look_at_entity(t: &mut Tick<'_, '_>, o: Other, y_max: f32, x_max: f32) {
    let p = t.pos();
    let xd = o.pos.x - p.x;
    let zd = o.pos.z - p.z;
    let yd = o.eye_y - t.eye_y();
    let sd = (xd * xd + zd * zd).sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let y_rot_d = atan2_deg(zd, xd) as f32 - 90.0;
    #[allow(clippy::cast_possible_truncation)]
    let x_rot_d = -(atan2_deg(yd, sd) as f32);
    let mut rot = t.core().rot();
    let lerp =
        |a: f32, b: f32, max: f32| a + vanillite_base::math::wrap_degrees_f(b - a).clamp(-max, max);
    rot.x_rot = lerp(rot.x_rot, x_rot_d, x_max);
    rot.y_rot = lerp(rot.y_rot, y_rot_d, y_max);
    t.core_mut().set_rot(rot);
}

/// `Creeper.setSwellDir`.
pub fn set_swell_dir(t: &mut Tick<'_, '_>, dir: i32) {
    if classic::creeper(t.mob_ref()).swell_dir != dir {
        classic::creeper_mut(t.mob()).swell_dir = dir;
        t.core_mut().mark_data_dirty();
    }
}

/// `EnderMan.setBeingStaredAt` / the creepy flag (open mouth).
pub fn set_creepy(t: &mut Tick<'_, '_>, on: bool) {
    if classic::enderman(t.mob_ref()).creepy != on {
        classic::enderman_mut(t.mob()).creepy = on;
        t.core_mut().mark_data_dirty();
    }
}

/// `EnderMan.isBeingStaredBy`: the player looks at the enderman's eyes and wears no
/// `#gaze_disguise_equipment` (a carved pumpkin) on its head.
pub fn is_stared_at_by(t: &Tick<'_, '_>, player: EntityId) -> bool {
    let disguised = t.ents.player(player).is_some_and(|p| {
        let head = p.inventory.equipment(crate::item::EquipmentSlot::Head);
        !head.is_empty()
            && crate::mob::fam::wildlife::item_in_tag(
                head.item,
                "minecraft:gaze_disguise_equipment",
            )
    });
    if disguised {
        return false;
    }
    let Some(e) = t.ents.store.get(player) else {
        return false;
    };
    let rot = e.rot();
    let eye = e.pos().y + f64::from(e.kind().info().eye_height);
    // `getViewVector`.
    let view = super::view_vector(rot.x_rot, rot.y_rot).normalize();
    let p = t.pos();
    let to = Vec3::new(p.x - e.pos().x, t.eye_y() - eye, p.z - e.pos().z);
    let dist = to.length_sqr().sqrt();
    if dist == 0.0 {
        return false;
    }
    let dot = view.dot(to.normalize());
    if dot <= 1.0 - 0.025 / dist {
        return false;
    }
    physics::clip(
        t.level as &dyn Blocks,
        Vec3::new(e.pos().x, eye, e.pos().z),
        Vec3::new(p.x, t.eye_y(), p.z),
    )
    .is_none()
}

/// `EnderMan.teleportTowards(entity)`: a spot 16 blocks from it towards `to` (its eye at
/// `eye_y`).
pub fn teleport_towards(t: &mut Tick<'_, '_>, to: Vec3, eye_y: f64) -> bool {
    let p = t.pos();
    let half = p.y + f64::from(t.core().dims().height) * 0.5;
    let dir = Vec3::new(p.x - to.x, half - eye_y, p.z - to.z).normalize();
    let r = &mut t.mob().random;
    let x = p.x + (r.next_double() - 0.5) * 8.0 - dir.x * 16.0;
    let y = p.y + f64::from(r.next_int_bounded(16) - 8) - dir.y * 16.0;
    let z = p.z + (r.next_double() - 0.5) * 8.0 - dir.z * 16.0;
    let id = t.id;
    teleport_to(t.level, t.ents, t.env, t.cx, id, Vec3::new(x, y, z))
}

/// `EnderMan.teleport()`: a random spot within 32 blocks.
pub fn teleport_randomly(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) -> bool {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return false;
    };
    let Some(m) = ents.mob_mut(id) else {
        return false;
    };
    let x = pos.x + (m.random.next_double() - 0.5) * 64.0;
    let y = pos.y + f64::from(m.random.next_int_bounded(64) - 32);
    let z = pos.z + (m.random.next_double() - 0.5) * 64.0;
    teleport_to(level, ents, env, cx, id, Vec3::new(x, y, z))
}

/// `EnderMan.teleport(x, y, z)`: `randomTeleport(x, y, z, true,
/// #enderman_does_not_teleport_to)`, then the teleport game event, level event 2018 (the
/// trail) and the teleport sound at the old and the new position.
fn teleport_to(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    to: Vec3,
) -> bool {
    if ents.vehicle_of(id).is_some() {
        return false;
    }
    let Some(e) = ents.store.get(id) else {
        return false;
    };
    let old = e.pos();
    let dims = e.dims();
    // `LivingEntity.randomTeleport`: walks down from the target to a block of
    // `#entities_can_teleport_to`; the height keeps its fraction.
    let clamped = level.border.clamp_vec(to.x, to.y, to.z);
    let mut pos = BlockPos::containing(clamped.x, clamped.y, clamped.z);
    let mut y = to.y;
    if !level.is_loaded(pos) {
        return false;
    }
    let mut floor = None;
    while pos.y > level.min_y() {
        pos = BlockPos::new(pos.x, pos.y - 1, pos.z);
        let state = level.block(pos);
        if can_teleport_onto(state) {
            floor = Some(state);
            break;
        }
        y -= 1.0;
    }
    let Some(floor) = floor else {
        return false;
    };
    // `checkPositionAndTeleport`.
    if enderman_avoids(floor) {
        return false;
    }
    let dest = Vec3::new(clamped.x, y, clamped.z);
    let aabb = dims.bounding_box(dest);
    let mut scratch = Vec::new();
    if !physics::no_collision(&level.view(), &aabb, &mut scratch) {
        return false;
    }
    // `containsAnyLiquid` and `findBlocksIn(aabb).filterState(isInvalidPosition)`: the blocks
    // from the minimum to the maximum corner, both included.
    let lo = BlockPos::containing(aabb.min_x, aabb.min_y, aabb.min_z);
    let hi = BlockPos::containing(aabb.max_x, aabb.max_y, aabb.max_z);
    for bx in lo.x..=hi.x {
        for by in lo.y..=hi.y {
            for bz in lo.z..=hi.z {
                if !level
                    .block(BlockPos::new(bx, by, bz))
                    .fluid_state()
                    .is_empty()
                {
                    return false;
                }
            }
        }
    }
    for bx in lo.x..=hi.x {
        for by in lo.y..=hi.y {
            for bz in lo.z..=hi.z {
                if enderman_avoids(level.block(BlockPos::new(bx, by, bz))) {
                    return false;
                }
            }
        }
    }
    // `EnderMan.canRandomlyTeleportTo`: no water below.
    let below = BlockPos::containing(dest.x, dest.y, dest.z);
    if super::is_water(level.block(BlockPos::new(below.x, below.y - 1, below.z))) {
        return false;
    }
    if let Some(mut e) = ents.store.get_mut(id) {
        // `teleportTo` → `snapTo`: the old position moves along, the motion stays.
        e.set_pos(dest);
        e.set_old_pos();
    }
    // `broadcastEntityEvent(this, 46)`: the teleport particles.
    if let Some(e) = ents.store.get(id) {
        let network_id = e.network_id().0;
        env.out.push(crate::survival::Effect::Tracking {
            entity: id,
            with_self: false,
            packet: vanillite_protocol::SharedPacket::of(
                &vanillite_protocol::packets::play::clientbound::EntityEvent {
                    entity_id: network_id,
                    event: 46,
                },
            ),
        });
    }
    let silent = match ents.mob_mut(id) {
        Some(m) => {
            m.nav.stop();
            (m.silent, m.kind.sound_source())
        }
        None => (true, SoundSource::Hostile),
    };
    crate::survival::game_event(
        env.out,
        crate::level::game_event::events().teleport,
        old,
        Some(id),
    );
    if !silent.0 {
        let from = BlockPos::containing(old.x, old.y, old.z);
        let now = BlockPos::containing(dest.x, dest.y, dest.z);
        // `BlockUtil.clampedPackDifferenceInPosition(from, now, 127, 127, 127)`.
        let d = |a: i32, b: i32| ((b - a).clamp(-127, 127) + 127) & 0xFF;
        let packed = d(from.x, now.x) << 16 | d(from.y, now.y) << 8 | d(from.z, now.z);
        cx.actions
            .push(super::WorldAction::LevelEvent(2018, from, packed));
        // `playSound(xo, yo, zo)` after `snapTo` and `this.playSound`: both at the new spot.
        for at in [dest, dest] {
            let seed = crate::survival::sound_seed();
            crate::survival::play_sound(
                env.out,
                None,
                at,
                "minecraft:entity.enderman.teleport",
                silent.1,
                1.0,
                1.0,
                seed,
            );
        }
    }
    true
}

/// `#entities_can_teleport_to` (`#blocks_motion`).
fn can_teleport_onto(state: BlockState) -> bool {
    match crate::player::play::game_data() {
        Some(d)
            if d.block_tags
                .contains_key("minecraft:entities_can_teleport_to") =>
        {
            d.block_in_tag(state.block().0, "minecraft:entities_can_teleport_to")
        }
        _ => blocks_motion(state),
    }
}

/// `#enderman_does_not_teleport_to`: `#dangerous_for_teleportation` and bedrock.
fn enderman_avoids(state: BlockState) -> bool {
    super::spawn::in_block_tag(state, "minecraft:enderman_does_not_teleport_to")
}

/// `BlockState.blocksMotion`: solid and not cobweb or bamboo sapling.
fn blocks_motion(state: BlockState) -> bool {
    state.block() != ids::COBWEB && state.block() != ids::BAMBOO_SAPLING && nav::is_solid(state)
}

/// `EnderMan.hurtServer` for projectiles: up to 64 tries to teleport away.
pub fn enderman_dodge(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
) {
    for _ in 0..64 {
        if teleport_randomly(level, ents, env, cx, id) {
            return;
        }
    }
}

/// `EnderMan.hurtServer`: other damage than from a living entity makes it teleport (9 in
/// 10).
pub fn after_hurt(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    id: EntityId,
    source: DamageSource,
) {
    let living_cause = source
        .cause
        .is_some_and(|c| ents.mob(c).is_some() || ents.player(c).is_some());
    let Some(m) = ents.mob_mut(id) else {
        return;
    };
    if m.kind != Kind::Enderman {
        return;
    }
    // `teleport()` does nothing for a dead enderman.
    if !living_cause && m.random.next_int_bounded(10) != 0 && m.is_alive() {
        teleport_randomly(level, ents, env, cx, id);
    }
}

/// Whether the ticking mob has its living flag "using item" (skeletons drawing their bow).
pub fn living_flags(m: &MobData) -> u8 {
    fam::living_flags(m)
}

/// The wool of each sheep colour (`DyeColor` order).
pub const SHEEP_WOOL: [&str; 16] = [
    "minecraft:white_wool",
    "minecraft:orange_wool",
    "minecraft:magenta_wool",
    "minecraft:light_blue_wool",
    "minecraft:yellow_wool",
    "minecraft:lime_wool",
    "minecraft:pink_wool",
    "minecraft:gray_wool",
    "minecraft:light_gray_wool",
    "minecraft:cyan_wool",
    "minecraft:purple_wool",
    "minecraft:blue_wool",
    "minecraft:brown_wool",
    "minecraft:green_wool",
    "minecraft:red_wool",
    "minecraft:black_wool",
];

/// One loot entry: an item with a uniform count, smelted when the mob burns.
struct Drop {
    item: &'static str,
    min: i32,
    max: i32,
    smelted: Option<&'static str>,
    player_kill: bool,
}

const fn drop(item: &'static str, min: i32, max: i32) -> Drop {
    Drop {
        item,
        min,
        max,
        smelted: None,
        player_kill: false,
    }
}

const fn meat(item: &'static str, min: i32, max: i32, smelted: &'static str) -> Drop {
    Drop {
        item,
        min,
        max,
        smelted: Some(smelted),
        player_kill: false,
    }
}

/// The items of `entities/<kind>` of the 26.3 loot tables without looting: one pool per
/// entry. `killed_by_player`, `on_fire` and the sheep's wool decide the conditional parts.
pub fn death_loot(m: &mut MobData, killed_by_player: bool) -> Vec<crate::item::ItemStack> {
    if m.kind.family() == fam::Family::Boss {
        return super::boss::death_loot(m, killed_by_player);
    }
    let pools: &[Drop] = match m.kind {
        Kind::Zombie => &[drop("minecraft:rotten_flesh", 0, 2)],
        Kind::Skeleton => &[drop("minecraft:arrow", 0, 2), drop("minecraft:bone", 0, 2)],
        Kind::Creeper => &[drop("minecraft:gunpowder", 0, 2)],
        Kind::Spider => &[
            drop("minecraft:string", 0, 2),
            Drop {
                item: "minecraft:spider_eye",
                min: -1,
                max: 1,
                smelted: None,
                player_kill: true,
            },
        ],
        Kind::Enderman => &[drop("minecraft:ender_pearl", 0, 1)],
        Kind::Cow => &[
            drop("minecraft:leather", 0, 2),
            meat("minecraft:beef", 1, 3, "minecraft:cooked_beef"),
        ],
        Kind::Pig => &[meat(
            "minecraft:porkchop",
            1,
            3,
            "minecraft:cooked_porkchop",
        )],
        Kind::Chicken => &[
            drop("minecraft:feather", 0, 2),
            meat("minecraft:chicken", 1, 1, "minecraft:cooked_chicken"),
        ],
        Kind::Sheep => &[meat("minecraft:mutton", 1, 2, "minecraft:cooked_mutton")],
        _ => &[],
    };
    let on_fire = m.is_on_fire();
    let mut out = Vec::new();
    let push = |name: &str, count: i32, out: &mut Vec<crate::item::ItemStack>| {
        if count > 0
            && let Some(id) = vanillite_data::registry::ITEM.id(name)
        {
            out.push(crate::item::ItemStack::new(id, count));
        }
    };
    for d in pools {
        if d.player_kill && !killed_by_player {
            continue;
        }
        let count = m.random.next_int_between_inclusive(d.min, d.max);
        let name = match d.smelted {
            Some(cooked) if on_fire => cooked,
            _ => d.item,
        };
        push(name, count, &mut out);
    }
    match m.kind {
        Kind::Zombie => {
            // The rare drop: `killed_by_player` and a 2.5 % chance.
            if killed_by_player && m.random.next_float() < 0.025 {
                let rare = [
                    "minecraft:iron_ingot",
                    "minecraft:carrot",
                    "minecraft:potato",
                ];
                let i = usize::try_from(m.random.next_int_bounded(3)).unwrap_or(0);
                push(rare[i], 1, &mut out);
            }
        }
        Kind::Sheep if !m.sheared => {
            push(
                SHEEP_WOOL[usize::from(classic::sheep(m).color & 15)],
                1,
                &mut out,
            );
        }
        _ => {}
    }
    out
}

/// `EndermanLeaveBlockGoal.tick`: puts the carried block down on a full block nearby.
pub fn enderman_leave_block(t: &mut Tick<'_, '_>) {
    let p = t.pos();
    let m = t.mob();
    let x = vanillite_base::math::floor(p.x - 1.0 + m.random.next_double() * 2.0);
    let y = vanillite_base::math::floor(p.y + m.random.next_double() * 2.0);
    let z = vanillite_base::math::floor(p.z - 1.0 + m.random.next_double() * 2.0);
    let pos = BlockPos::new(x, y, z);
    let Some(carried) = classic::enderman(t.mob_ref()).carried else {
        return;
    };
    let here = t.level.block(pos);
    let below_pos = BlockPos::new(x, y - 1, z);
    let below = t.level.block(below_pos);
    let full_below = below.collision_shape().iter().any(|b| {
        b[0] <= 0.0 && b[1] <= 0.0 && b[2] <= 0.0 && b[3] >= 1.0 && b[4] >= 1.0 && b[5] >= 1.0
    });
    if !here.is_air()
        || below.is_air()
        || below.block() == vanillite_data::block::ids::BEDROCK
        || !full_below
        || !t.level.can_survive(carried, pos)
    {
        return;
    }
    let cell = vanillite_base::Aabb::of_block(pos);
    let mut occupied = false;
    let me = t.id;
    t.ents.store.for_each_in(&cell, false, |id| {
        if id != me {
            occupied = true;
        }
    });
    if occupied {
        return;
    }
    t.cx.actions.push(WorldAction::SetBlock(pos, carried, 3));
    classic::enderman_mut(t.mob()).carried = None;
    t.core_mut().mark_data_dirty();
}

/// `EndermanTakeBlockGoal.tick`: picks up a holdable block it can reach.
pub fn enderman_take_block(t: &mut Tick<'_, '_>) {
    let p = t.pos();
    let m = t.mob();
    let x = vanillite_base::math::floor(p.x - 2.0 + m.random.next_double() * 4.0);
    let y = vanillite_base::math::floor(p.y + m.random.next_double() * 3.0);
    let z = vanillite_base::math::floor(p.z - 2.0 + m.random.next_double() * 4.0);
    let pos = BlockPos::new(x, y, z);
    let state = t.level.block(pos);
    let holdable = crate::player::play::game_data()
        .is_some_and(|d| d.enderman_holdable.contains(&state.block().0));
    if !holdable {
        return;
    }
    let from = vanillite_base::Vec3::new(
        f64::from(vanillite_base::math::floor(p.x)) + 0.5,
        f64::from(y) + 0.5,
        f64::from(vanillite_base::math::floor(p.z)) + 0.5,
    );
    let to = vanillite_base::Vec3::new(f64::from(x) + 0.5, f64::from(y) + 0.5, f64::from(z) + 0.5);
    let reached = match super::physics::clip(t.level, from, to) {
        Some((hit, _)) => hit == pos,
        None => true,
    };
    if !reached {
        return;
    }
    // `removeBlock(pos, false)`.
    t.cx.actions
        .push(WorldAction::SetBlock(pos, BlockState::AIR, 3));
    classic::enderman_mut(t.mob()).carried = Some(state.block().default_state());
    t.core_mut().mark_data_dirty();
}

/// The path costs of a mob: its kind's, or those a creaking gets from its heart
/// (`Creaking.setTransient`).
pub fn mob_malus(m: &MobData) -> &'static MalusTable {
    if fam::mount::creaking::heart_bound(m) {
        fam::mount::creaking::HEART_BOUND_MALUS
    } else {
        m.kind.malus()
    }
}
