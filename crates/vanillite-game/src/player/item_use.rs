//! A player's item use with the entities (`ServerPlayerGameMode.useItem`, the entity side of
//! `useItemOn`, `Player.attack`): the cooldown check, the item's `use` and the `use_cooldown`
//! of instantly used items afterwards. The server and the parity scenarios both call these.

use vanillite_base::{BlockPos, Direction, Vec3};
use vanillite_config::GameMode;
use vanillite_data::entity::ids;
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{EntityId, NetworkIds};

use crate::entities::Entities;
use crate::item::{self, ItemStack};
use crate::level::Level;
use crate::mob::{MobContext, animal, attack, tame};
use crate::survival::damage::DamageSource;
use crate::survival::player::Env;

/// The inventory index of a hand.
pub fn hand_index(ents: &Entities, player: EntityId, hand: Hand) -> usize {
    match hand {
        Hand::MainHand => ents.player(player).map_or(0, |p| p.inventory.selected),
        Hand::OffHand => crate::inventory::OFFHAND,
    }
}

/// Takes the stack out of a hand to use it with the entities; [`put_back`] returns it.
pub fn take_held(ents: &mut Entities, player: EntityId, hand: Hand) -> ItemStack {
    let i = hand_index(ents, player, hand);
    ents.player_mut(player)
        .and_then(|p| p.inventory.get_mut(i).map(std::mem::take))
        .unwrap_or_default()
}

/// Puts a stack into a hand.
pub fn put_back(ents: &mut Entities, player: EntityId, hand: Hand, stack: ItemStack) {
    let i = hand_index(ents, player, hand);
    if let Some(p) = ents.player_mut(player) {
        p.inventory.set(i, stack);
    }
}

/// `UseCooldown.apply`: a player's cooldown for the group of `before`.
pub fn apply_use_cooldown(ents: &mut Entities, player: EntityId, before: &ItemStack) {
    if let Some(p) = ents.player_mut(player) {
        let sink = std::sync::Arc::clone(&p.connection.sink);
        p.survival
            .cooldowns
            .apply_use_cooldown(before, sink.as_ref());
    }
}

/// Whether the player's cooldowns hold the stack back (`isOnCooldown(stack)`).
pub fn is_on_cooldown(ents: &Entities, player: EntityId, stack: &ItemStack) -> bool {
    ents.player(player)
        .is_some_and(|p| p.survival.cooldowns.is_on_cooldown(stack))
}

/// What an item's `use` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Used {
    /// Nothing (`PASS`/`FAIL`).
    Pass,
    /// It was used at once (`SUCCESS`): the `use_cooldown` applies.
    Success,
    /// Using it over time started (`CONSUME`).
    Started,
}

/// `ServerPlayerGameMode.useItem` of the items that need entities: boats, the carrot on a
/// stick, thrown items, rockets, fishing rods, equipment swaps, and items used over time.
#[allow(clippy::too_many_arguments)]
pub fn use_item(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    network_ids: &NetworkIds,
    player: EntityId,
    hand: Hand,
) {
    let Some(game_mode) = ents.player(player).map(|p| p.game_mode) else {
        return;
    };
    if game_mode == GameMode::Spectator {
        return;
    }
    let i = hand_index(ents, player, hand);
    if ents.player(player).is_none_or(|p| {
        p.inventory.get(i).is_empty() || is_on_cooldown(ents, player, p.inventory.get(i))
    }) {
        return;
    }
    let before = ents
        .player(player)
        .map(|p| p.inventory.get(i).clone())
        .unwrap_or_default();
    let using_before = ents
        .player(player)
        .is_some_and(|p| p.survival.using_item.is_some());
    if crate::player::items::use_item(level, ents, env, network_ids, player, hand) {
        // `ItemStack.use` of an instantly used item (books, knowledge book, empty map):
        // `applyAfterUseComponentSideEffects` puts its `use_cooldown` group on cooldown.
        let started = !using_before
            && ents
                .player(player)
                .is_some_and(|p| p.survival.using_item.is_some());
        if !started {
            apply_use_cooldown(ents, player, &before);
        }
        return;
    }
    let held = ents
        .player(player)
        .map(|p| p.inventory.get(i).clone())
        .unwrap_or_default();
    if let Some(projectile) = launched_projectile(&held)
        && let Some(uuid) = ents.store.get(player).map(|e| e.uuid())
        && !env.hooks.projectile_launch(
            crate::hooks::HookWorld::split(level, ents),
            uuid,
            projectile,
        )
    {
        return;
    }
    let mut stack = take_held(ents, player, hand);
    let before = stack.clone();
    let item = stack.item;
    let creative = game_mode == GameMode::Creative;
    let name = item::item_id(&stack).name();
    let used = if name.ends_with("_boat") || name.ends_with("_raft") {
        if crate::vehicle::use_boat_item(
            level,
            ents,
            env,
            network_ids,
            player,
            &mut stack,
            creative,
        ) {
            award_used(ents, player, item);
        }
        Used::Success
    } else if name == "minecraft:carrot_on_a_stick" || name == "minecraft:warped_fungus_on_a_stick"
    {
        // `FoodOnAStickItem.use` counts the use only when nothing was boosted.
        if !crate::riding::boost(ents, env, player, &mut stack, creative, hand) {
            award_used(ents, player, item);
        }
        Used::Success
    } else if crate::effect::thrown::kind_of(&stack).is_some() {
        crate::effect::thrown::throw(ents, env, network_ids, player, &mut stack, creative);
        award_used(ents, player, item);
        Used::Success
    } else if name == crate::mob::fam::wildlife::ender_pearl::ITEM {
        if crate::mob::fam::wildlife::ender_pearl::throw(
            ents,
            env,
            network_ids,
            player,
            &mut stack,
            hand,
        ) {
            award_used(ents, player, item);
            Used::Success
        } else {
            Used::Pass
        }
    } else if let Some(kind) = crate::mob::fam::wildlife::snowball::thrown_kind(&stack) {
        crate::mob::fam::wildlife::snowball::player_throw(
            ents,
            env,
            network_ids,
            player,
            &mut stack,
            kind,
        );
        Used::Success
    } else if name == "minecraft:wind_charge" {
        crate::mob::fam::mount::wind::player_throw(ents, env, network_ids, player, &mut stack);
        Used::Success
    } else if name == "minecraft:ender_eye" {
        crate::mob::fam::wildlife::eye_of_ender::use_item(
            level,
            ents,
            env,
            network_ids,
            player,
            &mut stack,
        )
    } else if name == "minecraft:firework_rocket" {
        crate::firework::use_while_gliding(ents, env, network_ids, player, &mut stack, hand)
    } else if name == "minecraft:fishing_rod" {
        // The rod stays in the hand while it is used (`shouldStopFishing` looks at the hands).
        put_back(ents, player, hand, stack.clone());
        let used =
            crate::fishing::use_rod(level, ents, env, cx, network_ids, player, &mut stack, hand);
        let _ = take_held(ents, player, hand);
        used
    } else if !crate::consumable::is_consumable(&stack)
        && let Some(slot) = crate::elytra::swappable_slot(&stack)
    {
        crate::elytra::swap_with_equipment(ents, env, player, &mut stack, slot)
    } else {
        // A brush only starts from `useOn` (`BrushItem` keeps `Item.use`: `PASS`).
        let brush = item::item_id(&stack) == vanillite_data::item::ids::BRUSH;
        put_back(ents, player, hand, stack);
        if !brush {
            crate::player::using::start(ents, env, network_ids, player, hand);
        }
        return;
    };
    put_back(ents, player, hand, stack);
    if used == Used::Success {
        apply_use_cooldown(ents, player, &before);
    }
}

/// The projectile a player's use of `stack` launches, for [`crate::hooks::GameHooks
/// ::projectile_launch`].
fn launched_projectile(stack: &ItemStack) -> Option<&'static str> {
    Some(match item::item_id(stack).name() {
        "minecraft:snowball" => "minecraft:snowball",
        "minecraft:egg" | "minecraft:blue_egg" | "minecraft:brown_egg" => "minecraft:egg",
        "minecraft:experience_bottle" => "minecraft:experience_bottle",
        "minecraft:ender_pearl" => "minecraft:ender_pearl",
        "minecraft:wind_charge" => "minecraft:wind_charge",
        "minecraft:ender_eye" => "minecraft:eye_of_ender",
        "minecraft:splash_potion" => "minecraft:splash_potion",
        "minecraft:lingering_potion" => "minecraft:lingering_potion",
        _ => return None,
    })
}

/// The items whose use on a block acts on the entities (the client sends them on).
pub fn uses_entities_on_block(stack: &ItemStack) -> bool {
    let name = item::item_id(stack).name();
    name.ends_with("_spawn_egg")
        || name == "minecraft:lead"
        || name.ends_with("minecart")
        || name == "minecraft:firework_rocket"
}

/// `useItemOn` with spawn eggs, leads, minecarts and rockets (`location` is the clicked point).
#[allow(clippy::too_many_arguments)]
pub fn use_item_on(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &NetworkIds,
    player: EntityId,
    hand: Hand,
    pos: BlockPos,
    face: Direction,
    location: Vec3,
    op: bool,
) {
    let mut stack = take_held(ents, player, hand);
    if is_on_cooldown(ents, player, &stack) {
        put_back(ents, player, hand, stack);
        return;
    }
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    let before = stack.clone();
    let used = if let Some(kind) = animal::spawn_egg_kind(&stack) {
        animal::use_spawn_egg_on(
            level,
            ents,
            env,
            network_ids,
            kind,
            pos,
            face,
            &mut stack,
            creative,
            player,
        )
    } else if item::item_id(&stack).name() == "minecraft:lead" {
        tame::use_lead_on_fence(level, ents, env, player, pos)
    } else if item::item_id(&stack).name() == "minecraft:firework_rocket" {
        crate::firework::use_on(ents, env, network_ids, player, &mut stack, face, location)
    } else {
        let placed =
            crate::minecart::use_item_on(level, ents, network_ids, pos, &mut stack, creative, op);
        if placed {
            // `gameEvent(ENTITY_PLACE, pos, Context.of(player, blockBelow))`.
            crate::survival::game_event(
                env.out,
                crate::level::game_event::events().entity_place,
                Vec3::new(
                    f64::from(pos.x) + 0.5,
                    f64::from(pos.y) + 0.5,
                    f64::from(pos.z) + 0.5,
                ),
                Some(player),
            );
        }
        placed
    };
    put_back(ents, player, hand, stack);
    if used && !before.is_empty() {
        // `ItemStack.useOn` of a successful item interaction, then `ITEM_USED_ON_BLOCK` and
        // `ANY_BLOCK_USE`.
        award_used(ents, player, before.item);
        if let Some(p) = ents.player_mut(player) {
            use crate::advancements::trigger::Event;
            p.advancements
                .trigger(Event::ItemUsedOnBlock(pos, before.clone()));
            p.advancements.trigger(Event::AnyBlockUse(pos, before));
        }
    }
}

/// `player.awardStat(Stats.ITEM_USED.get(item))`.
pub fn award_used(ents: &mut Entities, player: EntityId, item: u16) {
    if let Some(p) = ents.player_mut(player) {
        p.stats.award(
            crate::stats::Stat::of(crate::stats::StatType::Used, item),
            1,
        );
    }
}

/// `Player.attack` of mobs and players, boats and knots.
pub fn attack_entity(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    player: EntityId,
    target: EntityId,
) {
    let kind = ents.store.get(target).map(|e| e.kind());
    if ents.decor(target).is_some() {
        // `skipAttackInteraction` of block-attached entities and interactions.
        if crate::decoration::attack(level, ents, env, cx, player, target) {
            return;
        }
    }
    if ents.missile(target).is_some() {
        let mut t = crate::mob::missile::MissileTick {
            level,
            ents: &mut *ents,
            env: &mut *env,
            cx: &mut *cx,
            id: target,
        };
        crate::mob::fam::illager::missiles::attacked_by_player(&mut t, player);
        return;
    }
    if kind == Some(ids::LEASH_KNOT) {
        // `LeashFenceKnotEntity.hurtServer`: it breaks.
        tame::break_knot(ents, env, target, &mut cx.discarded);
        return;
    }
    // Vehicles, end crystals, projectiles and living entities (`canHarmPlayer` is the hurt's).
    attack::player_attack(level, ents, env, cx, player, target);
}

/// `VehicleEntity.hurtServer` of a boat or minecart.
pub fn hurt_vehicle(
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: DamageSource,
    amount: f32,
) {
    if ents.minecart(target).is_some() {
        crate::minecart::hurt(ents, env, cx, target, source, amount);
    } else {
        crate::vehicle::hurt_boat(ents, env, cx, target, source, amount);
    }
}
