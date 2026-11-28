//! Using an item over time (`LivingEntity.startUsingItem`, `updatingUsingItem`,
//! `releaseUsingItem`, `completeUsingItem`) for players: eating and drinking, blocking with a
//! shield, drawing a bow, loading and firing a crossbow, charging with a spear (its damage is
//! [`crate::spear::tick_players`]). What a stack does comes from its components
//! ([`crate::consumable`], [`crate::blocking`]), and from the item for those with an own use
//! ([`class_use_duration`]).

use vanillite_base::Vec3;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_config::GameMode;
use vanillite_protocol::packets::play::clientbound::{EntityEvent, SoundSource};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::EntityId;

use crate::consumable::{Consumable, ConsumeEffect};
use crate::entities::Entities;
use crate::item::{self, ItemStack};
use crate::survival::play_sound;
use crate::survival::player::Env;

/// The item a player uses (`useItem`, `useItemRemaining`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsingItem {
    pub hand: Hand,
    /// The item used; using stops when the hand holds another one.
    pub item: u16,
    /// Ticks used so far.
    pub ticks: i32,
    /// `getUseDuration`.
    pub duration: i32,
    /// The loading sounds a crossbow played.
    pub crossbow: crate::crossbow::UseFlags,
    /// The used stack broke in the hand (a shield that blocked): Vanilla keeps using the
    /// emptied stack until the hand holds something else.
    pub emptied: bool,
}

/// The equipment slot of a hand.
pub(crate) fn hand_slot(hand: Hand) -> crate::item::EquipmentSlot {
    match hand {
        Hand::MainHand => crate::item::EquipmentSlot::MainHand,
        Hand::OffHand => crate::item::EquipmentSlot::OffHand,
    }
}

fn hand_index(ents: &Entities, player: EntityId, hand: Hand) -> usize {
    match hand {
        Hand::MainHand => ents.player(player).map_or(0, |p| p.inventory.selected),
        Hand::OffHand => crate::inventory::OFFHAND,
    }
}

fn held(ents: &Entities, player: EntityId, hand: Hand) -> ItemStack {
    let i = hand_index(ents, player, hand);
    ents.player(player)
        .map(|p| p.inventory.get(i).clone())
        .unwrap_or_default()
}

/// Draws from the player's own random (`Entity.random`).
fn random<R>(
    ents: &mut Entities,
    player: EntityId,
    f: impl FnOnce(&mut LegacyRandom) -> R,
) -> Option<R> {
    ents.player_mut(player).map(|p| f(&mut p.survival.random))
}

/// `getUseDuration` of the items with their own `use` (`BowItem`, `CrossbowItem`,
/// `TridentItem`, `BrushItem`, `BundleItem`, `SpyglassItem`, `InstrumentItem`); 0 for every
/// other item, whose use is `Item.use` with the stack's components.
fn class_use_duration(stack: &ItemStack) -> i32 {
    let name = item::item_id(stack).name();
    if name == "minecraft:bow" || name == crate::crossbow::CROSSBOW || name == "minecraft:trident" {
        return 72000;
    }
    if name == "minecraft:brush" || crate::bundle::has_contents(stack) {
        return 200;
    }
    if name == "minecraft:spyglass" {
        return 1200;
    }
    if super::items::instrument(stack).is_some() {
        return super::items::instrument_use_ticks(stack);
    }
    0
}

/// `getUseDuration` of a stack, 0 when it is not used over time: the item's own, else
/// `Item.getUseDuration` (the `consumable` ticks, or 72000 with `blocks_attacks` or
/// `kinetic_weapon`).
fn use_duration(stack: &ItemStack) -> i32 {
    let class = class_use_duration(stack);
    if class > 0 {
        return class;
    }
    if let Some(c) = crate::consumable::consumable(stack) {
        return c.consume_ticks();
    }
    if crate::blocking::has_blocks_attacks(stack) || crate::spear::is_kinetic(stack) {
        72000
    } else {
        0
    }
}

/// The stack's `consumable` component as `Item.use` and `Item.finishUsingItem` see it: none
/// for the items with their own use.
fn item_consumable(stack: &ItemStack) -> Option<Consumable> {
    if class_use_duration(stack) > 0 {
        return None;
    }
    crate::consumable::consumable(stack)
}

/// `ItemStack.causeUseVibration`: `use_effects.interact_vibrations` of the stack (on by
/// default, off for spears).
pub(crate) fn use_vibrations(stack: &ItemStack) -> bool {
    crate::consumable::use_vibrations(stack)
}

/// `gameEvent(event)` of a player at its position.
fn player_game_event(ents: &Entities, env: &mut Env<'_>, player: EntityId, event: u16) {
    if let Some(pos) = ents.store.get(player).map(|e| e.pos()) {
        crate::survival::game_event(env.out, event, pos, Some(player));
    }
}

fn mark(ents: &mut Entities, player: EntityId) {
    if let Some(mut e) = ents.store.get_mut(player) {
        e.mark_data_dirty();
    }
}

/// `startUsingItem(hand)` from `use_item`, when the held item is used over time. A bow needs
/// arrows unless the player has infinite materials; a loaded crossbow fires instead
/// (`CrossbowItem.use`), an empty one needs something to load.
pub fn start(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &vanillite_world::entity::NetworkIds,
    player: EntityId,
    hand: Hand,
) {
    let stack = held(ents, player, hand);
    if stack.is_empty() {
        return;
    }
    if crate::crossbow::is_crossbow(&stack) {
        if crate::crossbow::is_charged(&stack) {
            crossbow_shoot(ents, env, network_ids, player, hand);
            return;
        }
        if crossbow_projectile(ents, player).is_none() {
            return;
        }
    }
    // `Item.use`: `Consumable.startConsuming` (`canConsume`: food needs `Player.canEat`).
    if let Some(c) = item_consumable(&stack) {
        let can = crate::consumable::food(&stack).is_none_or(|food| {
            ents.player(player).is_some_and(|p| {
                food.can_always_eat
                    || matches!(p.game_mode, GameMode::Creative | GameMode::Spectator)
                    || p.survival.food.needs_food()
            })
        });
        if !can {
            return;
        }
        if c.consume_ticks() <= 0 {
            // Consumed at once.
            consume(ents, env, player, hand, &stack, &c);
            return;
        }
    }
    let duration = use_duration(&stack);
    if duration <= 0 {
        return;
    }
    if crate::trident::is_trident(&stack) && !crate::trident::can_start(ents, env, player, &stack) {
        return;
    }
    if item::item_id(&stack).name() == "minecraft:bow" && find_arrow(ents, player).is_none() {
        let creative = ents
            .player(player)
            .is_some_and(|p| p.game_mode == GameMode::Creative);
        if !creative {
            return;
        }
    }
    let kinetic = crate::spear::kinetic_weapon(&stack);
    // `startUsingItem` does nothing while an item is in use.
    let using = ents
        .player(player)
        .is_some_and(|p| p.survival.using_item.is_some());
    if !using {
        if let Some(p) = ents.player_mut(player) {
            p.survival.using_item = Some(UsingItem {
                hand,
                item: stack.item,
                ticks: 0,
                duration,
                crossbow: crate::crossbow::UseFlags::default(),
                emptied: false,
            });
            p.survival.stabbed.clear();
        }
        mark(ents, player);
        if use_vibrations(&stack) {
            let event = crate::level::game_event::events().item_interact_start;
            player_game_event(ents, env, player, event);
        }
    }
    // `Item.use` of a kinetic weapon: `KineticWeapon.makeSound`.
    if let Some(k) = kinetic {
        crate::spear::make_sound(ents, env, player, k.sound.as_ref());
    }
}

/// `stopUsingItem`: the item in use (`stack`) vibrates when it was in use.
pub fn stop(ents: &mut Entities, env: &mut Env<'_>, player: EntityId, stack: &ItemStack) {
    if stop_quietly(ents, player) && !stack.is_empty() && use_vibrations(stack) {
        let event = crate::level::game_event::events().item_interact_finish;
        player_game_event(ents, env, player, event);
    }
}

/// `stopUsingItem` without its game event; returns whether an item was in use.
fn stop_quietly(ents: &mut Entities, player: EntityId) -> bool {
    let was = ents.player_mut(player).and_then(|p| {
        p.survival.stabbed = Vec::new();
        p.survival.using_item.take()
    });
    if was.is_some() {
        mark(ents, player);
    }
    was.is_some()
}

/// Whether the player blocks with the item it uses (`isBlocking`: `getItemBlockingWith`).
pub fn is_blocking(ents: &Entities, player: EntityId) -> bool {
    let Some(u) = ents.player(player).and_then(|p| p.survival.using_item) else {
        return false;
    };
    let stack = held(ents, player, u.hand);
    !u.emptied
        && stack.item == u.item
        && crate::blocking::block_delay_ticks(&stack).is_some_and(|delay| u.ticks >= delay)
}

/// The living entity flags of a player (`DATA_LIVING_ENTITY_FLAGS`): using an item and the
/// off hand.
pub fn living_flags(u: Option<UsingItem>) -> u8 {
    match u {
        Some(u) => 1 | if u.hand == Hand::OffHand { 2 } else { 0 },
        None => 0,
    }
}

/// `updatingUsingItem` of a player, between its base tick and `aiStep`: stops when the hand
/// changed, runs the per-tick behaviour and finishes consumables.
pub fn tick(ents: &mut Entities, env: &mut Env<'_>, player: EntityId) {
    // The end of `LivingEntity.baseTick` (just before): eyes under water (no bubble column)
    // leave a `#dismounts_underwater` vehicle.
    let underwater = ents
        .player(player)
        .is_some_and(|p| p.survival.fluids.eye_in_water && !p.survival.is_dead_or_dying())
        && ents.vehicle_of(player).is_some()
        && ents.store.get(player).is_some_and(|e| {
            let p = e.pos();
            let eye = vanillite_base::BlockPos::containing(
                p.x,
                p.y + crate::survival::player::eye_height(e.dims().height),
                p.z,
            );
            !env.blocks
                .block_state(eye)
                .is_some_and(|s| env.facts.is_bubble_column(s))
        });
    if underwater {
        crate::riding::dismount_underwater(ents, env, player);
    }
    let Some(u) = ents.player(player).and_then(|p| p.survival.using_item) else {
        return;
    };
    let stack = held(ents, player, u.hand);
    if u.emptied {
        // The used stack is empty: it stays in use until the hand holds another item.
        if !stack.is_empty() {
            stop_quietly(ents, player);
        }
        return;
    }
    if stack.item != u.item || stack.is_empty() {
        stop(ents, env, player, &ItemStack::new(u.item, 1));
        return;
    }
    let ticks = u.ticks.saturating_add(1);
    if let Some(p) = ents.player_mut(player) {
        // `ServerPlayer.updateUsingItem`: `USING_ITEM`.
        p.advancements.trigger_item(
            crate::advancements::trigger::TriggerKind::USING_ITEM,
            &stack,
        );
        if let Some(state) = p.survival.using_item.as_mut() {
            state.ticks = ticks;
        }
    }
    if crate::blocking::block_delay_ticks(&stack) == Some(ticks) {
        // The blocking state becomes visible (`isBlocking`).
        mark(ents, player);
    }
    let remaining = u.duration.saturating_sub(ticks);
    if is_brush(&stack) {
        // `BrushItem.onUseTick` needs the level: [`tick_brushes`]. The brush is no
        // consumable, `completeUsingItem` only stops using it.
        if remaining <= 0 {
            stop(ents, env, player, &stack);
        }
        return;
    }
    if crate::crossbow::is_crossbow(&stack) {
        // `onUseTick` sees the ticks before this one's (`useItemRemaining` counts down after).
        crossbow_use_tick(ents, env, player, u.hand, ticks.saturating_sub(1));
        return;
    }
    if crate::bundle::has_contents(&stack) {
        // `BundleItem.onUseTick` with Vanilla's remaining ticks before the decrement.
        super::items::bundle_use_tick(
            ents,
            env,
            player,
            u.hand,
            remaining.saturating_add(1),
            u.duration,
        );
    }
    // `onUseTick` sees the remaining ticks before this tick's decrement.
    on_use_tick(ents, env, player, &stack, remaining.saturating_add(1));
    if remaining <= 0 && u.duration < 72000 {
        finish(ents, env, player, u);
    }
}

fn is_brush(stack: &ItemStack) -> bool {
    item::item_id(stack).name() == "minecraft:brush"
}

/// `BrushItem.onUseTick` of every player brushing (after [`tick`], with the entities back
/// in the level): the view ray must hit a block within the block interaction range, else the
/// player stops (`releaseUsingItem`); every tenth tick (`timeElapsed % 10 == 5`) the stroke
/// plays the brush sound and brushes a brushable block (`BrushableBlockEntity.brush`).
pub fn tick_brushes(level: &mut crate::level::Level) {
    let users: Vec<(EntityId, UsingItem)> = level
        .entities
        .players
        .iter()
        .filter_map(|(id, p)| p.survival.using_item.map(|u| (id, u)))
        .collect();
    for (player, u) in users {
        let stack = held(&level.entities, player, u.hand);
        if stack.item != u.item || !is_brush(&stack) {
            continue;
        }
        let Some(hit) = brush_hit(level, player) else {
            if stop_quietly(&mut level.entities, player)
                && use_vibrations(&stack)
                && let Some(at) = level.entities.store.get(player).map(|e| e.pos())
            {
                let event = crate::level::game_event::events().item_interact_finish;
                level.game_event(
                    event,
                    at,
                    crate::level::game_event::GameEventContext::of(Some(player)),
                );
            }
            continue;
        };
        // `timeElapsed = getUseDuration - ticksRemaining + 1` with the remaining ticks before
        // this tick's decrement: the ticks used so far.
        if u.ticks % 10 != 5 {
            continue;
        }
        let (pos, face) = hit;
        let state = level.block(pos);
        let sound = crate::block::brushable::brush_sound(state);
        let sound = crate::block::sound_id(sound.strip_prefix("minecraft:").unwrap_or(sound));
        level.play_block_sound(
            Some(player),
            pos,
            sound,
            crate::level::SOUND_BLOCKS,
            1.0,
            1.0,
        );
        if !matches!(
            level.block_entity(pos),
            Some(crate::level::BlockEntity::Brushable(_))
        ) {
            continue;
        }
        let i = hand_index(&level.entities, player, u.hand);
        let mut brush = level
            .entities
            .player_mut(player)
            .and_then(|p| p.inventory.get_mut(i).map(std::mem::take))
            .unwrap_or_default();
        let time = level.game_time();
        let slot = match u.hand {
            Hand::MainHand => crate::item::EquipmentSlot::MainHand,
            Hand::OffHand => crate::item::EquipmentSlot::OffHand,
        };
        crate::block::brushable::brush(level, time, player, pos, face, &mut brush, slot);
        if let Some(p) = level.entities.player_mut(player)
            && let Some(slot) = p.inventory.get_mut(i)
        {
            *slot = brush;
        }
    }
}

/// `ProjectileUtil.getHitResultOnViewVector(player, CAN_BE_PICKED, blockInteractionRange)`:
/// the block and face the player looks at, `None` when nothing or an entity is hit first.
fn brush_hit(
    level: &crate::level::Level,
    player: EntityId,
) -> Option<(vanillite_base::BlockPos, vanillite_base::Direction)> {
    // `blockInteractionRange()`: the attribute (creative players have its +0.5 modifier).
    let range = level.entities.player(player).map_or(4.5, |p| {
        crate::player::play::interaction_ranges(&p.inventory, p.game_mode).0
    });
    let e = level.entities.store.get(player)?;
    let info = e.kind().info();
    let eye_height =
        f64::from(info.eye_height) * f64::from(e.dims().height / info.height.max(0.01));
    let rot = e.rot();
    let p = e.pos();
    let eye = Vec3::new(p.x, p.y + eye_height, p.z);
    let view = crate::mob::view_vector(rot.x_rot, rot.y_rot);
    let to = Vec3::new(
        eye.x + view.x * range,
        eye.y + view.y * range,
        eye.z + view.z * range,
    );
    let (pos, at) = crate::mob::physics::clip(level, eye, to)?;
    // An entity on the way is hit first (`getEntityHitResult` with `isPickable`).
    let area =
        vanillite_base::Aabb::new(eye.x, eye.y, eye.z, at.x, at.y, at.z).inflate(1.0, 1.0, 1.0);
    let mut entity_first = false;
    let store = &level.entities.store;
    store.for_each_in(&area, false, |other| {
        if other == player || entity_first {
            return;
        }
        let Some(o) = store.get(other) else {
            return;
        };
        if !pickable(level, other, o.kind()) {
            return;
        }
        let delta = Vec3::new(at.x - eye.x, at.y - eye.y, at.z - eye.z);
        if let Some(t) = ray_box(eye, delta, &o.aabb())
            && t < 1.0
        {
            entity_first = true;
        }
    });
    if entity_first {
        return None;
    }
    Some((pos, hit_face(pos, at, view)))
}

/// `Entity.isPickable` of the entities a view ray can hit: living entities that are not
/// spectators, boats, minecarts and other solid entities, not items, orbs or projectiles.
fn pickable(
    level: &crate::level::Level,
    id: EntityId,
    kind: vanillite_data::entity::EntityTypeId,
) -> bool {
    use vanillite_data::entity::ids;
    if let Some(p) = level.entities.player(id) {
        return p.game_mode != GameMode::Spectator && !p.survival.is_dead_or_dying();
    }
    if let Some(m) = level.entities.mob(id) {
        return m.is_alive();
    }
    crate::vehicle::BoatKind::of(kind).is_some()
        || crate::minecart::MinecartKind::of(kind).is_some()
        || kind == ids::END_CRYSTAL
        || kind == ids::ARMOR_STAND
}

/// The entry fraction of the segment `from + t * delta` (`t` in 0..=1) into the box.
fn ray_box(from: Vec3, delta: Vec3, b: &vanillite_base::Aabb) -> Option<f64> {
    let mut t0 = 0.0_f64;
    let mut t1 = 1.0_f64;
    for (o, d, lo, hi) in [
        (from.x, delta.x, b.min_x, b.max_x),
        (from.y, delta.y, b.min_y, b.max_y),
        (from.z, delta.z, b.min_z, b.max_z),
    ] {
        if d.abs() < 1e-12 {
            if o < lo || o > hi {
                return None;
            }
            continue;
        }
        let (a, c) = ((lo - o) / d, (hi - o) / d);
        let (a, c) = if a < c { (a, c) } else { (c, a) };
        t0 = t0.max(a);
        t1 = t1.min(c);
        if t0 > t1 {
            return None;
        }
    }
    Some(t0)
}

/// The face of the block the ray entered at `at`: the side whose plane the hit point lies on,
/// facing against the ray.
fn hit_face(pos: vanillite_base::BlockPos, at: Vec3, view: Vec3) -> vanillite_base::Direction {
    use vanillite_base::Direction;
    let local = Vec3::new(
        at.x - f64::from(pos.x),
        at.y - f64::from(pos.y),
        at.z - f64::from(pos.z),
    );
    let candidates = [
        (local.x, view.x, Direction::West, Direction::East),
        (local.y, view.y, Direction::Down, Direction::Up),
        (local.z, view.z, Direction::North, Direction::South),
    ];
    let mut best = (f64::MAX, Direction::Up);
    for (c, d, low, high) in candidates {
        if d > 0.0 && c.abs() < best.0 {
            best = (c.abs(), low);
        } else if d < 0.0 && (1.0 - c).abs() < best.0 {
            best = ((1.0 - c).abs(), high);
        }
    }
    best.1
}

/// `ItemStack.onUseTick`: the sounds and particles of a consumable while it is consumed.
fn on_use_tick(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    stack: &ItemStack,
    remaining: i32,
) {
    if let Some(c) = crate::consumable::consumable(stack)
        && c.should_emit(remaining)
    {
        emit(ents, env, player, &c, 5);
    }
}

/// `Consumable.emitParticlesAndSounds(random, user, stack, particleCount)` with the player's
/// random: the volume and pitch draws, the draws of `spawnItemParticles` (the particles are
/// the clients'), then the consume sound for everyone else.
fn emit(ents: &mut Entities, env: &mut Env<'_>, player: EntityId, c: &Consumable, particles: i32) {
    let Some(pos) = ents.store.get(player).map(|e| e.pos()) else {
        return;
    };
    let drawn = random(ents, player, |r| {
        let eat_volume = if r.next_boolean() { 0.5 } else { 1.0 };
        let eat_pitch = 1.0 + 0.2 * (r.next_float() - r.next_float());
        let drink_pitch = r.next_float() * (1.0 - 0.9) + 0.9;
        if c.has_consume_particles {
            for _ in 0..particles.saturating_mul(4) {
                r.next_float();
            }
        }
        if c.drink {
            (0.5, drink_pitch)
        } else {
            (eat_volume, eat_pitch)
        }
    });
    let Some((volume, pitch)) = drawn else {
        return;
    };
    play_sound(
        env.out,
        Some(player),
        pos,
        &c.sound,
        SoundSource::Players,
        volume,
        pitch,
        crate::survival::sound_seed(),
    );
}

/// Runs `f` with the player as an effect host.
fn with_host(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    f: impl FnOnce(&mut crate::effect::living::PlayerHost<'_, '_, '_>),
) {
    let Entities { store, players, .. } = &mut *ents;
    if let Some(mut entity) = store.get_mut(player)
        && let Some(data) = players.get_mut(entity.as_ref().kind_row())
    {
        let mut host = crate::effect::living::PlayerHost {
            entity: &mut entity,
            player: data,
            env: &mut *env,
            players: None,
            names: &[],
        };
        f(&mut host);
    }
}

/// `Consumable.onConsume` of the `stack` a player holds in `hand`, then
/// `applyAfterUseComponentSideEffects` (`use_remainder`, `use_cooldown`): sounds, the use
/// statistic, the listeners (`food` feeds, potion contents, stew effects, ominous bottle),
/// the `on_consume_effects` in order, the game event; then the stack shrinks. Returns whether
/// it was consumed (a plugin may cancel it).
fn consume(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    hand: Hand,
    stack: &ItemStack,
    c: &Consumable,
) -> bool {
    // Bukkit's `PlayerItemConsumeEvent`: cancelled, nothing is consumed and the client sees
    // its inventory again.
    if let Some(uuid) = ents.store.get(player).map(|e| e.uuid())
        && super::player_event(
            env.hooks,
            vanillite_abi::event::PLAYER_ITEM_CONSUME,
            uuid,
            |e| {
                e.item = crate::hooks::item_snbt(stack);
                e.cause = match hand {
                    Hand::MainHand => vanillite_abi::hand::MAIN,
                    Hand::OffHand => vanillite_abi::hand::OFF,
                };
            },
        )
        .is_some_and(|e| e.cancelled)
    {
        if let Some(p) = ents.player_mut(player) {
            let sink = std::sync::Arc::clone(&p.connection.sink);
            p.inventory.broadcast_full_state(sink.as_ref());
        }
        return false;
    }
    let kind = vanillite_data::entity::ids::PLAYER;
    let pos = ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos());
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    emit(ents, env, player, c, 16);
    if let Some(p) = ents.player_mut(player) {
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, stack.item),
            1,
        );
        p.advancements.trigger_item(
            crate::advancements::trigger::TriggerKind::CONSUME_ITEM,
            stack,
        );
    }
    // `FoodProperties.onConsume`: the consume sound for everyone, the food, the burp.
    if let Some(food) = crate::consumable::food(stack)
        && let Some((pitch, burp)) = random(ents, player, |r| {
            let pitch = 1.0 + 0.4 * (r.next_float() - r.next_float());
            (pitch, r.next_float() * (1.0 - 0.9) + 0.9)
        })
    {
        play_sound(
            env.out,
            None,
            pos,
            &c.sound,
            SoundSource::Neutral,
            1.0,
            pitch,
            crate::survival::sound_seed(),
        );
        if let Some(p) = ents.player_mut(player) {
            p.survival.food.eat(&food);
        }
        play_sound(
            env.out,
            None,
            pos,
            "minecraft:entity.player.burp",
            SoundSource::Players,
            0.5,
            burp,
            crate::survival::sound_seed(),
        );
    }
    with_host(ents, env, player, |host| {
        crate::effect::consume::listeners(stack, kind, host, Some(player));
    });
    for effect in &c.effects {
        match effect {
            ConsumeEffect::Teleport(diameter, particles) => {
                super::chorus::teleport_randomly(
                    ents,
                    env,
                    player,
                    f64::from(*diameter),
                    *particles,
                );
            }
            ConsumeEffect::PlaySound(sound) => {
                // At the centre of the player's block (`playSound(null, blockPosition(), ..)`).
                let at = ents.store.get(player).map_or(pos, |e| e.pos());
                play_sound(
                    env.out,
                    None,
                    Vec3::new(at.x.floor() + 0.5, at.y.floor() + 0.5, at.z.floor() + 0.5),
                    sound,
                    SoundSource::Players,
                    1.0,
                    1.0,
                    crate::survival::sound_seed(),
                );
            }
            status => {
                // `apply_effects` rolls its probability with the player's random.
                let roll = if matches!(status, ConsumeEffect::Apply(..)) {
                    random(ents, player, |r| r.next_float()).unwrap_or(0.0)
                } else {
                    0.0
                };
                with_host(ents, env, player, |host| {
                    crate::effect::consume::apply_status(status, kind, host, &mut || roll);
                });
            }
        }
    }
    let events = crate::level::game_event::events();
    let event = if c.drink { events.drink } else { events.eat };
    player_game_event(ents, env, player, event);
    // `stack.consume(1, user)`, then `UseRemainder.convertIntoRemainder`: the remainder takes
    // the place of a used up stack, else it goes to the inventory.
    if !creative {
        let remainder = crate::consumable::use_remainder(stack);
        let i = hand_index(ents, player, hand);
        let mut extra = None;
        if let Some(p) = ents.player_mut(player) {
            if let Some(s) = p.inventory.get_mut(i) {
                s.shrink(1);
                if let Some(rest) = remainder {
                    if s.is_empty() {
                        *s = rest;
                    } else {
                        extra = Some(rest);
                    }
                }
            }
            // `handleExtraItemsCreatedOnUse`: `inventory.add`, else `drop(extra, false)`.
            if let Some(rest) = &mut extra
                && p.inventory.add(None, rest, false)
            {
                extra = None;
            }
        }
        if let Some(rest) = extra.filter(|r| !r.is_empty()) {
            super::items::drop_from_player(ents, env, player, rest, false);
        }
    }
    // The `use_cooldown` of the used stack.
    if let Some(p) = ents.player_mut(player) {
        let sink = std::sync::Arc::clone(&p.connection.sink);
        p.survival
            .cooldowns
            .apply_use_cooldown(stack, sink.as_ref());
    }
    true
}

/// `completeUsingItem`: a consumable is consumed (`Item.finishUsingItem`), the client is told
/// (`entity_event` 9).
fn finish(ents: &mut Entities, env: &mut Env<'_>, player: EntityId, u: UsingItem) {
    // `stopUsingItem` comes after `finishUsingItem`: its game event is posted at the end.
    let was_using = stop_quietly(ents, player);
    let stack = held(ents, player, u.hand);
    let Some(c) = item_consumable(&stack) else {
        // The stack stays; the spyglass stops zooming.
        if item::item_id(&stack).name() == "minecraft:spyglass" {
            let pos = ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos());
            spyglass_stop(env, player, pos);
        }
        finished_event(ents, player);
        // `stopUsingItem`: the kept stack vibrates.
        if was_using && use_vibrations(&stack) {
            let event = crate::level::game_event::events().item_interact_finish;
            player_game_event(ents, env, player, event);
        }
        return;
    };
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    let consumed = consume(ents, env, player, u.hand, &stack, &c);
    finished_event(ents, player);
    if !consumed {
        return;
    }
    // `stopUsingItem`: the used stack vibrates unless it was used up.
    let left = creative || stack.count > 1;
    if was_using && left && use_vibrations(&stack) {
        let event = crate::level::game_event::events().item_interact_finish;
        player_game_event(ents, env, player, event);
    }
}

/// `ServerPlayer.completeUsingItem`: `entity_event` 9 to the player.
fn finished_event(ents: &Entities, player: EntityId) {
    if let Some(e) = ents.store.get(player)
        && let Some(p) = ents.player(player)
    {
        p.connection.sink.send(&EntityEvent {
            entity_id: e.network_id().0,
            event: 9,
        });
    }
}

/// `SpyglassItem.stopUsing`: the player's sound for everyone else.
fn spyglass_stop(env: &mut Env<'_>, player: EntityId, pos: Vec3) {
    play_sound(
        env.out,
        Some(player),
        pos,
        "minecraft:item.spyglass.stop_using",
        SoundSource::Players,
        1.0,
        1.0,
        crate::survival::sound_seed(),
    );
}

/// The first arrow (`#arrows`: arrows, tipped and spectral arrows) in the inventory
/// (`getProjectile`): off hand, main hand, then the slots.
fn find_arrow(ents: &Entities, player: EntityId) -> Option<usize> {
    let p = ents.player(player)?;
    let is_arrow = |s: &ItemStack| {
        !s.is_empty()
            && matches!(
                item::item_id(s).name(),
                "minecraft:arrow" | "minecraft:tipped_arrow" | "minecraft:spectral_arrow"
            )
    };
    if is_arrow(p.inventory.get(crate::inventory::OFFHAND)) {
        return Some(crate::inventory::OFFHAND);
    }
    if is_arrow(p.inventory.selected_item()) {
        return Some(p.inventory.selected);
    }
    (0..36).find(|&i| is_arrow(p.inventory.get(i)))
}

/// Where a player's crossbow load comes from (`Player.getProjectile`).
enum Ammo {
    Slot(usize),
    /// `hasInfiniteMaterials` without ammo: a new arrow.
    Free(ItemStack),
}

/// `Player.getProjectile(crossbow)`: an arrow or rocket in a hand (off hand first), else the
/// first arrow of the inventory, else an arrow for creative players.
fn crossbow_projectile(ents: &Entities, player: EntityId) -> Option<Ammo> {
    let p = ents.player(player)?;
    let off = crate::inventory::OFFHAND;
    if crate::crossbow::is_held_projectile(p.inventory.get(off)) {
        return Some(Ammo::Slot(off));
    }
    if crate::crossbow::is_held_projectile(p.inventory.selected_item()) {
        return Some(Ammo::Slot(p.inventory.selected));
    }
    if let Some(i) = (0..36).find(|&i| crate::crossbow::is_arrow(p.inventory.get(i))) {
        return Some(Ammo::Slot(i));
    }
    let arrow = vanillite_data::registry::ITEM.id("minecraft:arrow")?;
    (p.game_mode == GameMode::Creative).then(|| Ammo::Free(ItemStack::new(arrow, 1)))
}

/// `CrossbowItem.onUseTick` of a player after `used` ticks: the loading sounds, and the load
/// once fully drawn (`tryLoadProjectiles`).
fn crossbow_use_tick(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    hand: Hand,
    used: i32,
) {
    let weapon = held(ents, player, hand);
    let duration = crate::crossbow::charge_duration(&weapon, env.random);
    let Some(mut flags) = ents
        .player(player)
        .and_then(|p| p.survival.using_item)
        .map(|u| u.crossbow)
    else {
        return;
    };
    let stage = crate::crossbow::use_tick(&mut flags, used, duration);
    if let Some(u) = ents
        .player_mut(player)
        .and_then(|p| p.survival.using_item.as_mut())
    {
        u.crossbow = flags;
    }
    let pos = ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos());
    let sounds = crate::crossbow::sounds(&weapon);
    for (on, sound) in [
        (stage.start_sound, sounds.start.as_deref()),
        (stage.mid_sound, sounds.mid.as_deref()),
    ] {
        if on && let Some(sound) = sound {
            let seed = crate::survival::sound_seed();
            play_sound(
                env.out,
                None,
                pos,
                sound,
                SoundSource::Players,
                0.5,
                1.0,
                seed,
            );
        }
    }
    if stage.full
        && !crate::crossbow::is_charged(&weapon)
        && crossbow_load(ents, env, player, hand)
        && let Some(sound) = sounds.end.as_deref()
    {
        let pitch = 1.0 / (env.random.next_float() * 0.5 + 1.0) + 0.2;
        let seed = crate::survival::sound_seed();
        play_sound(
            env.out,
            None,
            pos,
            sound,
            SoundSource::Players,
            1.0,
            pitch,
            seed,
        );
    }
}

/// `CrossbowItem.tryLoadProjectiles`: draws the projectiles into `charged_projectiles`.
fn crossbow_load(ents: &mut Entities, env: &mut Env<'_>, player: EntityId, hand: Hand) -> bool {
    let i = hand_index(ents, player, hand);
    let mut weapon = held(ents, player, hand);
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    let (mut projectile, slot) = match crossbow_projectile(ents, player) {
        Some(Ammo::Slot(j)) => (
            ents.player(player)
                .map(|p| p.inventory.get(j).clone())
                .unwrap_or_default(),
            Some(j),
        ),
        Some(Ammo::Free(s)) => (s, None),
        None => return false,
    };
    let drawn = crate::crossbow::draw(&weapon, &mut projectile, creative, env.random);
    let Some(p) = ents.player_mut(player) else {
        return false;
    };
    if let Some(j) = slot {
        p.inventory.set(j, projectile);
    }
    if drawn.is_empty() {
        return false;
    }
    crate::crossbow::set_charged(&mut weapon, &drawn);
    p.inventory.set(i, weapon);
    true
}

/// `CrossbowItem.performShooting` of a player (`use` with a loaded crossbow): critical arrows
/// along the view, durability with Unbreaking.
fn crossbow_shoot(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &vanillite_world::entity::NetworkIds,
    player: EntityId,
    hand: Hand,
) {
    let i = hand_index(ents, player, hand);
    let mut weapon = held(ents, player, hand);
    let projectiles = crate::crossbow::charged(&weapon);
    crate::crossbow::set_charged(&mut weapon, &[]);
    if let Some(p) = ents.player_mut(player) {
        p.inventory.set(i, weapon.clone());
    }
    if projectiles.is_empty() {
        return;
    }
    let Some(e) = ents.store.get(player) else {
        return;
    };
    let pos = e.pos();
    let rot = e.rot();
    let shooter = crate::crossbow::Shooter {
        id: player,
        pos,
        eye_y: pos.y + f64::from(1.62_f32 * (e.dims().height / 1.8)),
        x_rot: rot.x_rot,
        y_rot: rot.y_rot,
        player: true,
        source: SoundSource::Players,
    };
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    let power = crate::crossbow::shooting_power(&projectiles);
    // The player's own random (`getRandom`) for the shot sounds.
    let Some(mut own) = ents.player(player).map(|p| p.survival.random.clone()) else {
        return;
    };
    let random = &mut *env.random;
    let mut broke = false;
    crate::crossbow::shoot(
        ents,
        network_ids,
        env.out,
        &mut own,
        shooter,
        &weapon,
        &projectiles,
        power,
        1.0,
        None,
        |ents, amount| {
            let b = !creative && hurt_held(ents, random, player, i, amount);
            broke |= b;
            b
        },
    );
    if broke && let Some(selected) = ents.player(player).map(|p| p.inventory.selected) {
        // `onEquippedItemBroken` of the crossbow's hand.
        let slot = crate::mob::attack::inventory_slot(selected, i);
        crate::durability::broken_in_env(ents, env, player, slot);
    }
    if let Some(p) = ents.player_mut(player) {
        p.survival.random = own;
    }
    // `performShooting`: `SHOT_CROSSBOW` and the use statistic with the (maybe broken) weapon.
    if let Some(p) = ents.player_mut(player) {
        let weapon = p.inventory.get(i).clone();
        p.advancements.trigger_item(
            crate::advancements::trigger::TriggerKind::SHOT_CROSSBOW,
            &weapon,
        );
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, weapon.item),
            1,
        );
    }
}

/// `hurtAndBreak(amount, player, slot)` of the stack in inventory slot `i`: the
/// `item_damage` effects (Unbreaking) from the level random; whether it broke (the caller
/// sends the break event).
fn hurt_held(
    ents: &mut Entities,
    random: &mut vanillite_base::random::LegacyRandom,
    player: EntityId,
    i: usize,
    amount: i32,
) -> bool {
    let Some(stack) = ents.player(player).map(|p| p.inventory.get(i).clone()) else {
        return false;
    };
    let points = crate::durability::change(&stack, amount, false, random);
    if points == 0 {
        return false;
    }
    let Some(p) = ents.player_mut(player) else {
        return false;
    };
    let Some(s) = p.inventory.get_mut(i) else {
        return false;
    };
    let d = item::damage(s).saturating_add(points);
    crate::advancements::view::apply_damage(Some(&mut p.advancements), Some(&mut p.stats), s, d)
}

/// `BowItem.getPowerForTime`.
pub fn bow_power(ticks: i32) -> f32 {
    let f = f32::from(i16::try_from(ticks.clamp(0, 1000)).unwrap_or(0)) / 20.0;
    ((f * f + f * 2.0) / 3.0).min(1.0)
}

/// `releaseUsingItem` (`player_action` `RELEASE_USE_ITEM`): a drawn bow shoots.
pub fn release(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &vanillite_world::entity::NetworkIds,
    player: EntityId,
) {
    let Some(u) = ents.player(player).and_then(|p| p.survival.using_item) else {
        return;
    };
    if u.emptied {
        // `releaseUsingItem` of an empty stack only stops.
        stop_quietly(ents, player);
        return;
    }
    let stack = held(ents, player, u.hand);
    if stack.item == u.item && item::item_id(&stack).name() == "minecraft:spyglass" {
        let pos = ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos());
        // `releaseUsing` (the sound), then `stopUsingItem`.
        spyglass_stop(env, player, pos);
        stop(ents, env, player, &stack);
        return;
    }
    if stack.item == u.item && crate::crossbow::is_crossbow(&stack) {
        // `useOnRelease`: one more `updatingUsingItem` (a crossbow loads when released just
        // in time); `releaseUsing` itself changes nothing.
        crossbow_use_tick(ents, env, player, u.hand, u.ticks);
        let stack = held(ents, player, u.hand);
        stop(ents, env, player, &stack);
        return;
    }
    let was_using = stop_quietly(ents, player);
    let mut stack = stack;
    if stack.item == u.item && crate::trident::is_trident(&stack) {
        let i = hand_index(ents, player, u.hand);
        crate::trident::release(ents, env, network_ids, player, i, u.ticks);
        stack = held(ents, player, u.hand);
    } else {
        shoot_bow(ents, env, network_ids, player, u, &stack);
    }
    // `stopUsingItem` after `releaseUsing`.
    if was_using && !stack.is_empty() && use_vibrations(&stack) {
        let event = crate::level::game_event::events().item_interact_finish;
        player_game_event(ents, env, player, event);
    }
}

/// `BowItem.releaseUsing`.
fn shoot_bow(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &vanillite_world::entity::NetworkIds,
    player: EntityId,
    u: UsingItem,
    stack: &ItemStack,
) {
    let stack = stack.clone();
    if stack.item != u.item || item::item_id(&stack).name() != "minecraft:bow" {
        return;
    }
    let power = bow_power(u.ticks);
    if power < 0.1 {
        return;
    }
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    let infinity = crate::player::play::game_data()
        .is_some_and(|d| d.enchantment_level(&stack, "minecraft:infinity") > 0);
    let arrow_slot = find_arrow(ents, player);
    if arrow_slot.is_none() && !creative {
        return;
    }
    // Creative players without arrows shoot a plain one.
    let ammo = arrow_slot
        .and_then(|i| ents.player(player).map(|p| p.inventory.get(i).clone()))
        .unwrap_or_else(|| {
            ItemStack::new(
                vanillite_data::registry::ITEM
                    .id("minecraft:arrow")
                    .unwrap_or(0),
                1,
            )
        });
    // Infinity (`ammo_use`) only saves plain arrows.
    let infinity = infinity && item::item_id(&ammo).name() == "minecraft:arrow";
    let Some(e) = ents.store.get(player) else {
        return;
    };
    let rot = e.rot();
    let p = e.pos();
    // `new Arrow(level, shooter, ..)`: `getEyeY() - 0.1F`.
    let eye = Vec3::new(
        p.x,
        p.y + crate::survival::player::eye_height(e.dims().height) - f64::from(0.1_f32),
        p.z,
    );
    // `shootFromRotation`: float angles through the sine table.
    let rad = std::f32::consts::PI / 180.0;
    let sin = |a: f32| vanillite_base::math::sin(f64::from(a));
    let cos = |a: f32| vanillite_base::math::cos(f64::from(a));
    let dir = Vec3::new(
        f64::from(-sin(rot.y_rot * rad) * cos(rot.x_rot * rad)),
        f64::from(-sin(rot.x_rot * rad)),
        f64::from(cos(rot.y_rot * rad) * cos(rot.x_rot * rad)),
    );
    // Arrows that cost nothing can only be picked up in creative mode.
    let free = creative || infinity;
    let pickup = if free { 2 } else { 1 };
    crate::mob::projectile::shoot_arrow(
        ents,
        network_ids,
        eye,
        dir,
        power * 3.0,
        1.0,
        Some(player),
        Some(&stack),
        &ammo,
        power >= 1.0,
        pickup,
    );
    // `ProjectileWeaponItem.shoot`: the bow's `hurtAndBreak(1, player, hand)` comes before
    // the sound.
    if !creative {
        let i = hand_index(ents, player, u.hand);
        if hurt_held(ents, env.random, player, i, 1) {
            crate::durability::broken_in_env(ents, env, player, hand_slot(u.hand));
        }
    }
    let pos = ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos());
    let pitch = 1.0 / (env.random.next_float() * 0.4 + 1.2) + power * 0.5;
    let seed = crate::survival::sound_seed();
    play_sound(
        env.out,
        None,
        pos,
        "minecraft:entity.arrow.shoot",
        SoundSource::Players,
        1.0,
        pitch,
        seed,
    );
    if let Some(p) = ents.player_mut(player) {
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, stack.item),
            1,
        );
    }
    if !free
        && let Some(i) = arrow_slot
        && let Some(p) = ents.player_mut(player)
        && let Some(s) = p.inventory.get_mut(i)
    {
        s.shrink(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bow_power_curve() {
        assert!(bow_power(0) == 0.0);
        assert!((bow_power(10) - (0.25 + 1.0) / 3.0).abs() < 1e-6);
        assert!((bow_power(20) - 1.0).abs() < 1e-6);
        assert!((bow_power(100) - 1.0).abs() < 1e-6);
    }

    /// Fish buckets carry `food` without `consumable`: they are not eaten (#119).
    #[test]
    fn fish_buckets_are_not_eaten() {
        use vanillite_data::item::ids;
        for bucket in [
            ids::COD_BUCKET,
            ids::SALMON_BUCKET,
            ids::PUFFERFISH_BUCKET,
            ids::TROPICAL_FISH_BUCKET,
        ] {
            assert!(bucket.info().food.is_some(), "{}", bucket.name());
            assert_eq!(
                use_duration(&ItemStack::new(bucket.0, 1)),
                0,
                "{}",
                bucket.name()
            );
        }
        assert_eq!(use_duration(&ItemStack::new(ids::COD.0, 1)), 32);
        assert_eq!(use_duration(&ItemStack::new(ids::DRIED_KELP.0, 1)), 16);
        assert_eq!(use_duration(&ItemStack::new(ids::HONEY_BOTTLE.0, 1)), 40);
        assert_eq!(use_duration(&ItemStack::new(ids::SHIELD.0, 1)), 72000);
    }

    #[test]
    fn living_flags_of_hands() {
        let u = UsingItem {
            hand: Hand::OffHand,
            item: 1,
            ticks: 0,
            duration: 32,
            crossbow: crate::crossbow::UseFlags::default(),
            emptied: false,
        };
        assert_eq!(living_flags(Some(u)), 3);
        assert_eq!(living_flags(None), 0);
    }
}
