//! Status effects (Vanilla `MobEffect`, `MobEffectInstance`, `LivingEntity.activeEffects`) of
//! players and mobs, potions (`Potion`, `PotionContents`) and the attribute modifiers effects
//! add (`AttributeMap`).
//!
//! - [`MobEffect`] and [`info`]: the 40 effects of 26.3 with their category, colour, particle
//!   and attribute modifier templates.
//! - [`EffectInstance`]: duration, amplifier, flags and the chain of hidden effects
//!   (`MobEffectInstance.update`).
//! - [`ActiveEffects`]: the effects of one living entity: adding with Vanilla's merge rules,
//!   the per-tick effect behaviour through an [`EffectHost`], removal, the synched particles
//!   and the packets for the entity's own client.
//! - [`attribute`]: attribute values with modifiers and their client sync.
//! - [`potion`]: the potion registry and the `potion_contents` / `suspicious_stew_effects`
//!   components.
//! - [`consume`]: what consuming an item does to the effects (food, potions, milk, honey).

pub mod active;
pub mod attribute;
pub mod attribute_map;
pub mod cloud;
pub mod consume;
pub mod instance;
pub mod living;
pub mod potion;
pub mod thrown;

pub use active::{ActiveEffects, EffectEvent, EffectHost, HurtKind};
pub use instance::EffectInstance;

/// A `minecraft:mob_effect` registry id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MobEffect(pub u8);

/// `MobEffectCategory`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Beneficial,
    Harmful,
    Neutral,
}

/// `AttributeModifier.Operation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    AddValue = 0,
    AddMultipliedBase = 1,
    AddMultipliedTotal = 2,
}

/// The particle an effect shows (`MobEffect.particleFactory`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Particle {
    /// `entity_effect` tinted with the effect colour (alpha 38 when ambient).
    EntityEffect,
    /// A particle without options (`SimpleParticleType`).
    Simple(&'static str),
}

/// An attribute modifier an effect adds (`MobEffect.AttributeTemplate`): the amount is
/// multiplied by `amplifier + 1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModifierTemplate {
    /// Index into `vanillite_data::entity::ATTRIBUTES` (the `minecraft:attribute` registry id).
    pub attribute: attribute::Attribute,
    pub id: &'static str,
    pub amount: f64,
    pub operation: Operation,
}

/// The static facts of one effect.
#[derive(Debug)]
pub struct EffectInfo {
    pub name: &'static str,
    pub category: Category,
    pub color: i32,
    pub particle: Particle,
    pub modifiers: &'static [ModifierTemplate],
    /// `isInstantaneous`.
    pub instantaneous: bool,
    /// `soundOnAdded`.
    pub sound_on_added: Option<&'static str>,
}

impl MobEffect {
    pub const SPEED: MobEffect = MobEffect(0);
    pub const SLOWNESS: MobEffect = MobEffect(1);
    pub const HASTE: MobEffect = MobEffect(2);
    pub const MINING_FATIGUE: MobEffect = MobEffect(3);
    pub const STRENGTH: MobEffect = MobEffect(4);
    pub const INSTANT_HEALTH: MobEffect = MobEffect(5);
    pub const INSTANT_DAMAGE: MobEffect = MobEffect(6);
    pub const JUMP_BOOST: MobEffect = MobEffect(7);
    pub const NAUSEA: MobEffect = MobEffect(8);
    pub const REGENERATION: MobEffect = MobEffect(9);
    pub const RESISTANCE: MobEffect = MobEffect(10);
    pub const FIRE_RESISTANCE: MobEffect = MobEffect(11);
    pub const WATER_BREATHING: MobEffect = MobEffect(12);
    pub const INVISIBILITY: MobEffect = MobEffect(13);
    pub const BLINDNESS: MobEffect = MobEffect(14);
    pub const NIGHT_VISION: MobEffect = MobEffect(15);
    pub const HUNGER: MobEffect = MobEffect(16);
    pub const WEAKNESS: MobEffect = MobEffect(17);
    pub const POISON: MobEffect = MobEffect(18);
    pub const WITHER: MobEffect = MobEffect(19);
    pub const HEALTH_BOOST: MobEffect = MobEffect(20);
    pub const ABSORPTION: MobEffect = MobEffect(21);
    pub const SATURATION: MobEffect = MobEffect(22);
    pub const GLOWING: MobEffect = MobEffect(23);
    pub const LEVITATION: MobEffect = MobEffect(24);
    pub const LUCK: MobEffect = MobEffect(25);
    pub const UNLUCK: MobEffect = MobEffect(26);
    pub const SLOW_FALLING: MobEffect = MobEffect(27);
    pub const CONDUIT_POWER: MobEffect = MobEffect(28);
    pub const DOLPHINS_GRACE: MobEffect = MobEffect(29);
    pub const BAD_OMEN: MobEffect = MobEffect(30);
    pub const HERO_OF_THE_VILLAGE: MobEffect = MobEffect(31);
    pub const DARKNESS: MobEffect = MobEffect(32);
    pub const TRIAL_OMEN: MobEffect = MobEffect(33);
    pub const RAID_OMEN: MobEffect = MobEffect(34);
    pub const WIND_CHARGED: MobEffect = MobEffect(35);
    pub const WEAVING: MobEffect = MobEffect(36);
    pub const OOZING: MobEffect = MobEffect(37);
    pub const INFESTED: MobEffect = MobEffect(38);
    pub const BREATH_OF_THE_NAUTILUS: MobEffect = MobEffect(39);

    /// The number of effects.
    pub const COUNT: usize = EFFECTS.len();

    /// The effect of a registry name (`minecraft:speed` or `speed`).
    pub fn by_name(name: &str) -> Option<MobEffect> {
        let name = name.strip_prefix("minecraft:").unwrap_or(name);
        EFFECTS
            .iter()
            .position(|e| e.name == name)
            .and_then(|i| u8::try_from(i).ok())
            .map(MobEffect)
    }

    /// The effect of a network / registry id.
    pub fn from_id(id: i32) -> Option<MobEffect> {
        let id = u8::try_from(id).ok()?;
        (usize::from(id) < EFFECTS.len()).then_some(MobEffect(id))
    }

    /// The static facts of the effect.
    pub fn info(self) -> &'static EffectInfo {
        EFFECTS.get(usize::from(self.0)).unwrap_or(&EFFECTS[0])
    }

    /// The registry name with namespace.
    pub fn key(self) -> String {
        format!("minecraft:{}", self.info().name)
    }

    /// `isBeneficial`.
    pub fn is_beneficial(self) -> bool {
        self.info().category == Category::Beneficial
    }

    /// `isInstantaneous`.
    pub fn is_instantaneous(self) -> bool {
        self.info().instantaneous
    }

    /// `shouldApplyEffectTickThisTick(tickCount, amplifier)`.
    pub fn should_apply_tick(self, tick_count: i32, amplifier: i32) -> bool {
        let every = |base: i32| {
            // Java's `>>` masks the shift distance to five bits.
            let interval = base.wrapping_shr(u32::try_from(amplifier).unwrap_or(0));
            interval <= 0 || tick_count % interval == 0
        };
        match self {
            MobEffect::REGENERATION => every(50),
            MobEffect::POISON => every(25),
            MobEffect::WITHER => every(40),
            MobEffect::HUNGER | MobEffect::ABSORPTION | MobEffect::BAD_OMEN => true,
            MobEffect::RAID_OMEN => tick_count == 1,
            _ if self.is_instantaneous() => tick_count >= 1,
            _ => false,
        }
    }
}

use Operation::{AddMultipliedTotal as Total, AddValue as Add};
use attribute::Attribute as A;

const fn m(attribute: A, id: &'static str, amount: f64, operation: Operation) -> ModifierTemplate {
    ModifierTemplate {
        attribute,
        id,
        amount,
        operation,
    }
}

const fn e(name: &'static str, category: Category, color: i32) -> EffectInfo {
    EffectInfo {
        name,
        category,
        color,
        particle: Particle::EntityEffect,
        modifiers: &[],
        instantaneous: false,
        sound_on_added: None,
    }
}

use Category::{Beneficial as B, Harmful as H, Neutral as N};

/// `MobEffects` in registration order (the registry ids).
static EFFECTS: [EffectInfo; 40] = [
    EffectInfo {
        modifiers: &[m(A::MOVEMENT_SPEED, "minecraft:effect.speed", 0.2, Total)],
        ..e("speed", B, 3_402_751)
    },
    EffectInfo {
        modifiers: &[m(
            A::MOVEMENT_SPEED,
            "minecraft:effect.slowness",
            -0.15,
            Total,
        )],
        ..e("slowness", H, 9_154_528)
    },
    EffectInfo {
        modifiers: &[m(A::ATTACK_SPEED, "minecraft:effect.haste", 0.1, Total)],
        ..e("haste", B, 14_270_531)
    },
    EffectInfo {
        modifiers: &[m(
            A::ATTACK_SPEED,
            "minecraft:effect.mining_fatigue",
            -0.1,
            Total,
        )],
        ..e("mining_fatigue", H, 4_866_583)
    },
    EffectInfo {
        modifiers: &[m(A::ATTACK_DAMAGE, "minecraft:effect.strength", 3.0, Add)],
        ..e("strength", B, 16_762_624)
    },
    EffectInfo {
        instantaneous: true,
        ..e("instant_health", B, 16_262_179)
    },
    EffectInfo {
        instantaneous: true,
        ..e("instant_damage", H, 11_101_546)
    },
    EffectInfo {
        modifiers: &[m(
            A::SAFE_FALL_DISTANCE,
            "minecraft:effect.jump_boost",
            1.0,
            Add,
        )],
        ..e("jump_boost", B, 16_646_020)
    },
    e("nausea", H, 5_578_058),
    e("regeneration", B, 13_458_603),
    e("resistance", B, 9_520_880),
    e("fire_resistance", B, 16_750_848),
    e("water_breathing", B, 10_017_472),
    EffectInfo {
        modifiers: &[m(
            A::WAYPOINT_TRANSMIT_RANGE,
            "minecraft:effect.waypoint_transmit_range_hide",
            -1.0,
            Total,
        )],
        ..e("invisibility", B, 16_185_078)
    },
    e("blindness", H, 2_039_587),
    e("night_vision", B, 12_779_366),
    e("hunger", H, 5_797_459),
    EffectInfo {
        modifiers: &[m(A::ATTACK_DAMAGE, "minecraft:effect.weakness", -4.0, Add)],
        ..e("weakness", H, 4_738_376)
    },
    e("poison", H, 8_889_187),
    e("wither", H, 7_561_558),
    EffectInfo {
        modifiers: &[m(A::MAX_HEALTH, "minecraft:effect.health_boost", 4.0, Add)],
        ..e("health_boost", B, 16_284_963)
    },
    EffectInfo {
        modifiers: &[m(
            A::MAX_ABSORPTION,
            "minecraft:effect.absorption",
            4.0,
            Add,
        )],
        ..e("absorption", B, 2_445_989)
    },
    EffectInfo {
        instantaneous: true,
        ..e("saturation", B, 16_262_179)
    },
    e("glowing", N, 9_740_385),
    e("levitation", H, 13_565_951),
    EffectInfo {
        modifiers: &[m(A::LUCK, "minecraft:effect.luck", 1.0, Add)],
        ..e("luck", B, 5_882_118)
    },
    EffectInfo {
        modifiers: &[m(A::LUCK, "minecraft:effect.unluck", -1.0, Add)],
        ..e("unluck", H, 12_624_973)
    },
    e("slow_falling", B, 15_978_425),
    e("conduit_power", B, 1_950_417),
    e("dolphins_grace", B, 8_954_814),
    EffectInfo {
        sound_on_added: Some("minecraft:event.mob_effect.bad_omen"),
        ..e("bad_omen", N, 745_784)
    },
    e("hero_of_the_village", B, 4_521_796),
    e("darkness", H, 2_696_993),
    EffectInfo {
        particle: Particle::Simple("minecraft:trial_omen"),
        sound_on_added: Some("minecraft:event.mob_effect.trial_omen"),
        ..e("trial_omen", N, 1_484_454)
    },
    EffectInfo {
        particle: Particle::Simple("minecraft:raid_omen"),
        sound_on_added: Some("minecraft:event.mob_effect.raid_omen"),
        ..e("raid_omen", N, 14_565_464)
    },
    EffectInfo {
        particle: Particle::Simple("minecraft:small_gust"),
        ..e("wind_charged", H, 12_438_015)
    },
    EffectInfo {
        particle: Particle::Simple("minecraft:item_cobweb"),
        ..e("weaving", H, 7_891_290)
    },
    EffectInfo {
        particle: Particle::Simple("minecraft:item_slime"),
        ..e("oozing", H, 10_092_451)
    },
    EffectInfo {
        particle: Particle::Simple("minecraft:infested"),
        ..e("infested", H, 9_214_860)
    },
    e("breath_of_the_nautilus", B, 65_518),
];

/// The effect table, for the tests and the command layer.
pub fn all() -> impl Iterator<Item = MobEffect> {
    (0..EFFECTS.len()).filter_map(|i| u8::try_from(i).ok().map(MobEffect))
}

/// Whether an entity type is in an entity type tag of the loaded data packs (`false` before
/// the game data is installed).
pub fn entity_in_tag(kind: vanillite_data::entity::EntityTypeId, tag: &str) -> bool {
    crate::player::play::game_data().is_some_and(|d| {
        d.registry_tags
            .get("minecraft:entity_type")
            .and_then(|tags| tags.get(tag))
            .is_some_and(|ids| ids.contains(&u32::from(kind.0)))
    })
}

/// `LivingEntity.canBeAffected` with the overrides of spiders, nautiluses, the Wither, wither
/// skeletons and parched skeletons.
pub fn can_be_affected(kind: vanillite_data::entity::EntityTypeId, effect: MobEffect) -> bool {
    let name = kind.name();
    let refused = match effect {
        MobEffect::POISON => matches!(
            name,
            "minecraft:spider"
                | "minecraft:cave_spider"
                | "minecraft:nautilus"
                | "minecraft:zombie_nautilus"
        ),
        MobEffect::WITHER => matches!(name, "minecraft:wither" | "minecraft:wither_skeleton"),
        MobEffect::WEAKNESS => name == "minecraft:parched",
        _ => false,
    };
    if refused {
        return false;
    }
    if entity_in_tag(kind, "minecraft:immune_to_infested") {
        effect != MobEffect::INFESTED
    } else if entity_in_tag(kind, "minecraft:immune_to_oozing") {
        effect != MobEffect::OOZING
    } else if entity_in_tag(kind, "minecraft:ignores_poison_and_regen") {
        effect != MobEffect::REGENERATION && effect != MobEffect::POISON
    } else {
        true
    }
}

/// `WitherBoss.addEffect` and `EnderDragon.addEffect`: these kinds refuse every effect (after
/// `canBeAffected`, which still reports the effects they would take).
pub fn refuses_all_effects(kind: vanillite_data::entity::EntityTypeId) -> bool {
    kind == vanillite_data::entity::ids::WITHER || kind == vanillite_data::entity::ids::ENDER_DRAGON
}

/// `isInvertedHealAndHarm`.
pub fn inverted_heal_and_harm(kind: vanillite_data::entity::EntityTypeId) -> bool {
    entity_in_tag(kind, "minecraft:inverted_healing_and_harm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_the_registry() {
        let reg = &vanillite_data::registry::MOB_EFFECT;
        assert_eq!(reg.len(), MobEffect::COUNT);
        for effect in all() {
            assert_eq!(
                reg.name(u16::from(effect.0)),
                Some(effect.key().as_str()),
                "{effect:?}"
            );
        }
    }

    #[test]
    fn tick_intervals_follow_the_amplifier() {
        assert!(MobEffect::REGENERATION.should_apply_tick(50, 0));
        assert!(!MobEffect::REGENERATION.should_apply_tick(49, 0));
        assert!(MobEffect::REGENERATION.should_apply_tick(25, 1));
        assert!(MobEffect::POISON.should_apply_tick(12, 1));
        assert!(MobEffect::POISON.should_apply_tick(7, 5));
        assert!(MobEffect::WITHER.should_apply_tick(40, 0));
        assert!(MobEffect::INSTANT_HEALTH.should_apply_tick(1, 0));
        assert!(!MobEffect::SPEED.should_apply_tick(1, 0));
    }
}
