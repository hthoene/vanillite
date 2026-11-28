//! Gliding with an elytra (`glider` items): starting to fall fly (`tryToStartFallFlying`), the
//! durability and game event while gliding (`updateFallFlying`), the glide physics of players
//! the server moves itself (`travelFallFlying`), the player's pose, and putting worn items on
//! with a right click (`Equippable.swapWithEquipmentSlot`).

use vanillite_base::Vec3;
use vanillite_base::random::RandomSource;
use vanillite_config::GameMode;
use vanillite_protocol::packets::play::clientbound::SoundSource;
use vanillite_world::entity::{EntityFlags, EntityId, EntityMut, EntityRef, Pose};

use crate::entities::Entities;
use crate::item::{self, EquipmentSlot, ItemStack};
use crate::player::PlayerData;
use crate::player::item_use::Used;
use crate::survival::play_sound;
use crate::survival::player::Env;

/// Whether the stack has the `glider` component (elytras).
pub fn is_glider(stack: &ItemStack) -> bool {
    item::has_glider(stack)
}

/// `canGlideUsing(stack, slot)`: a glider worn in its slot that the next damage does not break.
pub fn can_glide_using(stack: &ItemStack, slot: EquipmentSlot) -> bool {
    is_glider(stack)
        && item::equippable_slot(stack) == Some(slot)
        && !(item::is_damageable(stack) && item::damage(stack) >= item::max_damage(stack) - 1)
}

/// The equipment slots of a player that hold a usable glider.
fn glider_slots(player: &PlayerData) -> Vec<EquipmentSlot> {
    [
        EquipmentSlot::MainHand,
        EquipmentSlot::OffHand,
        EquipmentSlot::Feet,
        EquipmentSlot::Legs,
        EquipmentSlot::Chest,
        EquipmentSlot::Head,
    ]
    .into_iter()
    .filter(|&s| can_glide_using(player.inventory.equipment(s), s))
    .collect()
}

/// `canGlide`: off the ground, riding nothing, no levitation, and a usable glider worn.
pub fn can_glide(entity: &EntityRef<'_>, player: &PlayerData) -> bool {
    let flags = entity.flags();
    if flags.contains(EntityFlags::ON_GROUND) || flags.contains(EntityFlags::PASSENGER) {
        return false;
    }
    !player
        .survival
        .effects
        .has(crate::effect::MobEffect::LEVITATION)
        && !glider_slots(player).is_empty()
}

/// `player_command` `START_FALL_FLYING`: `tryToStartFallFlying`, else `stopFallFlying`.
pub fn start_fall_flying(ents: &mut Entities, player: EntityId) {
    let Entities { store, players, .. } = ents;
    let Some(mut entity) = store.get_mut(player) else {
        return;
    };
    let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
        return;
    };
    let f = data.survival.fluids;
    let flying = entity.as_ref().flags().contains(EntityFlags::FALL_FLYING);
    let start = !flying && can_glide(&entity.as_ref(), data) && !f.in_water() && !f.in_lava();
    // `stopFallFlying` sets the flag and clears it: the client hears it is off.
    entity.set_flags(EntityFlags::FALL_FLYING, start);
    entity.mark_data_dirty();
}

/// `updateFallFlying` of a gliding player (in `aiStep`, before its travel): a slow fall caps
/// the fall distance at 1; the gliding stops when the player cannot glide; every 10 ticks the
/// `elytra_glide` game event, every 20 a point of durability of a glider.
pub fn update_fall_flying(entity: &mut EntityMut<'_>, player: &mut PlayerData, env: &mut Env<'_>) {
    // `checkFallDistanceAccumulation`.
    let dy = if player.survival.simulate_movement {
        entity.as_ref().velocity().y
    } else {
        player.connection.movement.known_movement.y
    };
    if dy > -0.5 && player.survival.fall_distance > 1.0 {
        player.survival.fall_distance = 1.0;
    }
    if !can_glide(&entity.as_ref(), player) {
        entity.set_flags(EntityFlags::FALL_FLYING, false);
        entity.mark_data_dirty();
        return;
    }
    let ticks = player.survival.fall_fly_ticks.saturating_add(1);
    if ticks % 10 != 0 {
        return;
    }
    if (ticks / 10) % 2 == 0 {
        // `Util.getRandom(slots, random)` draws from the player's own random.
        let slots = glider_slots(player);
        if !slots.is_empty() {
            let n = i32::try_from(slots.len()).unwrap_or(i32::MAX);
            let i = player.survival.random.next_int_bounded(n);
            if let Some(&slot) = slots.get(usize::try_from(i).unwrap_or(0)) {
                hurt_slot(entity, player, env, slot);
            }
        }
    }
    let e = entity.as_ref();
    crate::survival::game_event(
        env.out,
        crate::level::game_event::events().elytra_glide,
        e.pos(),
        Some(e.id()),
    );
}

/// `ItemStack.hurtAndBreak(1, player, slot)` of a worn glider: Unbreaking may save the point
/// (2·level / (5·level + 5) for armour enchantables); creative players' items keep it.
pub(crate) fn hurt_slot(
    entity: &EntityMut<'_>,
    player: &mut PlayerData,
    env: &mut Env<'_>,
    slot: EquipmentSlot,
) {
    if player.game_mode == GameMode::Creative {
        return;
    }
    let index = match slot {
        EquipmentSlot::MainHand => player.inventory.selected,
        EquipmentSlot::OffHand => crate::inventory::OFFHAND,
        EquipmentSlot::Feet => 36,
        EquipmentSlot::Legs => 37,
        EquipmentSlot::Chest => 38,
        EquipmentSlot::Head => 39,
        EquipmentSlot::Body | EquipmentSlot::Saddle => return,
    };
    let stack = player.inventory.get(index).clone();
    let change = crate::durability::change(&stack, 1, false, env.random);
    if change == 0 {
        return;
    }
    let Some(s) = player.inventory.get_mut(index) else {
        return;
    };
    let d = item::damage(s).saturating_add(change);
    if crate::advancements::view::apply_damage(
        Some(&mut player.advancements),
        Some(&mut player.stats),
        s,
        d,
    ) {
        let e = entity.as_ref();
        env.out.push(crate::survival::Effect::Tracking {
            entity: e.id(),
            with_self: true,
            packet: crate::durability::break_packet(e.network_id().0, slot),
        });
    }
}

/// `LivingEntity.tick`'s `fallFlyTicks` after `aiStep`: counts while gliding.
pub fn count_fall_fly_ticks(entity: &EntityMut<'_>, player: &mut PlayerData) {
    let s = &mut player.survival;
    s.fall_fly_ticks = if entity.as_ref().flags().contains(EntityFlags::FALL_FLYING) {
        s.fall_fly_ticks.saturating_add(1)
    } else {
        0
    };
}

/// `updateFallFlyingMovement`: lift, the dive and pull-up conversions and drag.
pub fn fall_flying_movement(movement: Vec3, x_rot: f32, y_rot: f32, gravity: f64) -> Vec3 {
    let look = crate::firework::view_vector(x_rot, y_rot);
    let lean = x_rot * crate::mob::DEG_TO_RAD;
    let look_h = (look.x * look.x + look.z * look.z).sqrt();
    let move_h = (movement.x * movement.x + movement.z * movement.z).sqrt();
    let lift = f64::from(lean).cos().powi(2);
    let mut m = movement + Vec3::new(0.0, gravity * (-1.0 + lift * 0.75), 0.0);
    if m.y < 0.0 && look_h > 0.0 {
        let convert = m.y * -0.1 * lift;
        m = m + Vec3::new(
            look.x * convert / look_h,
            convert,
            look.z * convert / look_h,
        );
    }
    if lean < 0.0 && look_h > 0.0 {
        let convert = move_h * f64::from(-vanillite_base::math::sin(f64::from(lean))) * 0.04;
        m = m + Vec3::new(
            -look.x * convert / look_h,
            convert * 3.2,
            -look.z * convert / look_h,
        );
    }
    if look_h > 0.0 {
        m = m + Vec3::new(
            (look.x / look_h * move_h - m.x) * 0.1,
            0.0,
            (look.z / look_h * move_h - m.z) * 0.1,
        );
    }
    m.multiply(
        f64::from(0.99_f32),
        f64::from(0.98_f32),
        f64::from(0.99_f32),
    )
}

/// `handleFallFlyingCollisions`: a glider that loses speed against a wall takes
/// `10 * lost - 3` damage (`fly_into_wall`); returns it.
pub fn fly_into_wall_damage(last_speed: f64, new_speed: f64) -> Option<f32> {
    #[allow(clippy::cast_possible_truncation)]
    let damage = ((last_speed - new_speed) * 10.0 - 3.0) as f32;
    (damage > 0.0).then_some(damage)
}

/// `Player.updatePlayerPose` without the fit test: sleeping and dying stay, then swimming, fall
/// flying, the riptide spin, crouching, standing.
pub fn update_player_pose(entity: &mut EntityMut<'_>, spin_attack: bool, scale: f64) {
    let current = entity.as_ref().pose();
    if matches!(current, Pose::Sleeping | Pose::Dying) {
        return;
    }
    let flags = entity.as_ref().flags();
    let pose = if flags.contains(EntityFlags::SWIMMING) {
        Pose::Swimming
    } else if flags.contains(EntityFlags::FALL_FLYING) {
        Pose::FallFlying
    } else if spin_attack {
        Pose::SpinAttack
    } else if flags.contains(EntityFlags::CROUCHING) {
        Pose::Crouching
    } else {
        Pose::Standing
    };
    if pose != current {
        entity.set_pose(pose);
        entity.set_dims(crate::player::scaled_dimensions(pose, scale));
    }
}

/// The equipment slot a right click puts the stack in (`equippable` with `swappable`).
pub fn swappable_slot(stack: &ItemStack) -> Option<EquipmentSlot> {
    let e = item::equippable(stack)?;
    (e.slot.is_armor() && e.swappable).then_some(e.slot)
}

/// `Equippable.swapWithEquipmentSlot`: the held item and the worn one change places (a stack
/// puts one on and the worn item into the inventory).
pub fn swap_with_equipment(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    stack: &mut ItemStack,
    slot: EquipmentSlot,
) -> Used {
    let Some(p) = ents.player(player) else {
        return Used::Pass;
    };
    let creative = p.game_mode == GameMode::Creative;
    let worn = p.inventory.equipment(slot).clone();
    let binding = crate::player::play::game_data()
        .is_some_and(|d| d.enchantment_level(&worn, "minecraft:binding_curse") > 0);
    if (binding && !creative) || worn.same_item_same_components(stack) {
        return Used::Pass;
    }
    let index = match slot {
        EquipmentSlot::Feet => 36,
        EquipmentSlot::Legs => 37,
        EquipmentSlot::Chest => 38,
        EquipmentSlot::Head => 39,
        _ => return Used::Pass,
    };
    let sound = crate::block::dispenser::equip_sound(stack);
    let Some(p) = ents.player_mut(player) else {
        return Used::Pass;
    };
    p.stats.award(
        crate::stats::Stat::of(crate::stats::StatType::Used, stack.item),
        1,
    );
    if stack.count <= 1 {
        // The hand keeps the stack only in creative mode when nothing was worn.
        let to_hand = match (worn.is_empty(), creative) {
            (true, true) => stack.clone(),
            (true, false) => ItemStack::default(),
            (false, _) => worn.clone(),
        };
        let to_slot = stack.clone();
        p.inventory.set(index, to_slot);
        *stack = to_hand;
    } else {
        let mut back = worn.clone();
        let one = stack.with_count(1);
        if !creative {
            stack.shrink(1);
        }
        p.inventory.set(index, one);
        if !back.is_empty() && !p.inventory.add(None, &mut back, false) {
            crate::mob::fam::companion::horse::drop_item(
                env,
                ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos()),
                back,
            );
        }
    }
    // `onEquipItem`: the equip sound.
    if let Some(pos) = ents.store.get(player).map(|e| e.pos()) {
        play_sound(
            env.out,
            None,
            pos,
            &format!("minecraft:{sound}"),
            SoundSource::Players,
            1.0,
            1.0,
            crate::survival::sound_seed(),
        );
    }
    Used::Success
}
