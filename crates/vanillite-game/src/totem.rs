//! Totems of undying (`death_protection`, `LivingEntity.checkTotemDeathProtection`): a living
//! entity about to die from damage that does not bypass invulnerability uses up the first
//! death protection item in its hands, keeps one health point, gets the item's death effects
//! (the totem: no effects, then Regeneration II, Absorption II and Fire Resistance; from the
//! stack's patch or the item's default component) and shows
//! the totem animation (entity event 35).

use vanillite_protocol::codec::Reader;
use vanillite_protocol::packets::play::clientbound::EntityEvent;
use vanillite_world::entity::EntityId;

use crate::effect::EffectHost;
use crate::effect::consume::ConsumeEffect;
use crate::item::{self, EquipmentSlot, ItemStack};
use crate::survival::Effect;
use crate::survival::damage::{DamageKind, DamageTypes, tag};

/// The death effects of a stack's `death_protection` component, `None` without one.
pub fn death_protection(stack: &ItemStack) -> Option<Vec<ConsumeEffect>> {
    if stack.is_empty() {
        return None;
    }
    let id = vanillite_data::registry::DATA_COMPONENT_TYPE.id("minecraft:death_protection")?;
    if let Some(p) = stack.patch() {
        if let Some(v) = p.get(id) {
            return Some(decode_effects(v));
        }
        if p.is_removed(id) {
            return None;
        }
    }
    item::item_id(stack)
        .info()
        .death_protection
        .map(crate::consumable::default_effects)
}

/// `DeathProtection.STREAM_CODEC`: the list of consume effects.
fn decode_effects(bytes: &[u8]) -> Vec<ConsumeEffect> {
    crate::consumable::read_effects(&mut Reader::new(bytes)).unwrap_or_default()
}

/// Whether damage of `kind` may be survived with a totem.
pub fn can_protect(types: &DamageTypes, kind: DamageKind) -> bool {
    !types.is(kind, tag::BYPASSES_INVULNERABILITY)
}

/// The first hand (main, then off hand) that holds a death protection item, with its effects.
pub fn protecting_hand(
    main: &ItemStack,
    off: &ItemStack,
) -> Option<(EquipmentSlot, Vec<ConsumeEffect>)> {
    death_protection(main)
        .map(|e| (EquipmentSlot::MainHand, e))
        .or_else(|| death_protection(off).map(|e| (EquipmentSlot::OffHand, e)))
}

/// `DeathProtection.applyEffects` on an effect host (`random` rolls the probabilities): the
/// effects that touch the status effects, and `play_sound` through `sound`
/// (`PlaySoundConsumeEffect`). `teleport_randomly` is not applied here.
pub fn apply_effects(
    effects: &[ConsumeEffect],
    kind: vanillite_data::entity::EntityTypeId,
    host: &mut dyn EffectHost,
    random: &mut dyn FnMut() -> f32,
    sound: &mut dyn FnMut(&str),
) {
    for effect in effects {
        if let ConsumeEffect::PlaySound(name) = effect {
            sound(name);
        } else {
            crate::effect::consume::apply_status(effect, kind, host, random);
        }
    }
}

/// `PlaySoundConsumeEffect.apply`: `playSound(null, entity.blockPosition(), sound,
/// entity.getSoundSource(), 1, 1)`.
pub fn play_consume_sound(
    out: &mut Vec<Effect>,
    pos: vanillite_base::Vec3,
    sound: &str,
    source: vanillite_protocol::packets::play::clientbound::SoundSource,
) {
    crate::survival::play_sound(
        out,
        None,
        vanillite_base::Vec3::new(
            pos.x.floor() + 0.5,
            pos.y.floor() + 0.5,
            pos.z.floor() + 0.5,
        ),
        sound,
        source,
        1.0,
        1.0,
        crate::survival::sound_seed(),
    );
}

/// The totem animation (entity event 35) for the entity and the players tracking it.
pub fn animation(out: &mut Vec<Effect>, entity: EntityId, network_id: i32) {
    out.push(Effect::Tracking {
        entity,
        with_self: true,
        packet: vanillite_protocol::SharedPacket::of(&EntityEvent {
            entity_id: network_id,
            event: 35,
        }),
    });
}
