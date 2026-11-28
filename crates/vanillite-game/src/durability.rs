//! `ItemStack.hurtAndBreak`: durability loss through the enchantments' `item_damage` effects
//! (`EnchantmentHelper.processDurabilityChange`, Unbreaking's `remove_binomial` from the level
//! random), the damage, and the break feedback (`LivingEntity.onEquippedItemBroken`: an entity
//! event per slot that makes clients play the stack's `break_sound` with item particles).

use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::EntityEvent;

use crate::enchant::FloatRandom;
use crate::item::{self, EquipmentSlot, ItemStack};

/// `ItemStack.processDurabilityChange(amount, level, player)`: nothing for items that take no
/// damage or for players with infinite materials; a positive amount through the
/// enchantments' `item_damage` effects.
pub fn change(stack: &ItemStack, amount: i32, infinite: bool, random: &mut dyn FloatRandom) -> i32 {
    if stack.is_empty() || !item::is_damageable(stack) || infinite {
        return 0;
    }
    if amount <= 0 {
        return amount;
    }
    match crate::player::play::game_data() {
        Some(d) => d.enchants.durability_change(stack, random, amount),
        None => amount,
    }
}

/// `ItemStack.applyDamage` without the owner's part: sets the damage and shrinks a stack that
/// reaches its maximum. Returns whether it broke.
pub fn apply(stack: &mut ItemStack, new_damage: i32) -> bool {
    item::set_damage(stack, new_damage);
    if new_damage >= item::max_damage(stack) {
        stack.shrink(1);
        true
    } else {
        false
    }
}

/// `hurtAndBreak(amount, level, null, ...)` without an owner: [`change`] and [`apply`].
/// Returns whether it broke.
pub fn hurt_and_break(stack: &mut ItemStack, amount: i32, random: &mut dyn FloatRandom) -> bool {
    let change = change(stack, amount, false, random);
    change != 0 && apply(stack, item::damage(stack).saturating_add(change))
}

/// `LivingEntity.entityEventForEquipmentBreak`.
pub fn break_event(slot: EquipmentSlot) -> i8 {
    match slot {
        EquipmentSlot::MainHand => 47,
        EquipmentSlot::OffHand => 48,
        EquipmentSlot::Head => 49,
        EquipmentSlot::Chest => 50,
        EquipmentSlot::Legs => 51,
        EquipmentSlot::Feet => 52,
        EquipmentSlot::Body => 65,
        EquipmentSlot::Saddle => 68,
    }
}

/// The entity event packet of [`break_event`] for the entity with `network_id`.
pub fn break_packet(network_id: i32, slot: EquipmentSlot) -> std::sync::Arc<SharedPacket> {
    SharedPacket::of(&EntityEvent {
        entity_id: network_id,
        event: break_event(slot),
    })
}

/// `onEquippedItemBroken` on a level: the break event to the entity's trackers and itself.
pub fn broken_on_level(
    level: &mut crate::level::Level,
    owner: vanillite_world::entity::EntityId,
    slot: EquipmentSlot,
) {
    level.broadcast_entity_event(owner, break_event(slot));
}

/// `onEquippedItemBroken` from a survival environment: the break event to the entity's
/// trackers and itself.
pub fn broken_in_env(
    ents: &crate::entities::Entities,
    env: &mut crate::survival::player::Env<'_>,
    owner: vanillite_world::entity::EntityId,
    slot: EquipmentSlot,
) {
    if let Some(e) = ents.store.get(owner) {
        env.out.push(crate::survival::Effect::Tracking {
            entity: owner,
            with_self: true,
            packet: break_packet(e.network_id().0, slot),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vanillite_base::random::LegacyRandom;

    /// `RemoveBinomial.process` against Vanilla 26.3 values: Unbreaking III on a tool
    /// (chance 3/4) removes the points whose draw is below it; above 128 points one Gaussian
    /// draw decides.
    #[test]
    fn remove_binomial_like_vanilla() {
        use crate::enchant::remove_binomial;
        let mut r = LegacyRandom::new(42);
        let draws: Vec<f32> = (0..4)
            .map(|_| vanillite_base::random::RandomSource::next_float(&mut r))
            .collect();
        let kept = draws.iter().filter(|&&f| f >= 0.75).count();
        let mut r = LegacyRandom::new(42);
        #[allow(clippy::cast_precision_loss)]
        let expected = kept as f32;
        assert_eq!(remove_binomial(0.75, &mut r, 4.0), expected);
        // 200 points at 1/2: floor(100) + gaussian * sqrt(50), rounded.
        let mut g = LegacyRandom::new(7);
        let gauss = vanillite_base::random::RandomSource::next_gaussian(&mut g);
        #[allow(clippy::cast_possible_truncation)]
        let drop = (100.0 + gauss * 50f64.sqrt() + 0.5).floor() as i32;
        let mut r = LegacyRandom::new(7);
        #[allow(clippy::cast_precision_loss)]
        let want = 200.0 - drop.clamp(0, 200) as f32;
        assert_eq!(remove_binomial(0.5, &mut r, 200.0), want);
        assert_eq!(break_event(EquipmentSlot::Body), 65);
    }
}
