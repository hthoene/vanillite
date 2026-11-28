//! Players interacting with mobs (`Player.interactOn`, `Mob.interact`, `mobInteract`) and the
//! animal life cycle around it: feeding, love mode and breeding (`Animal`, `BreedGoal`),
//! babies growing up, shearing, milking, saddles and riding pigs, spawn eggs, name tags and
//! leads.

use vanillite_base::random::RandomSource;
use vanillite_base::{BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{EntityEvent, SoundSource};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{EntityId, NetworkIds, Rotation};

use super::fam::classic;
use super::{Kind, MobContext, MobData, combat, kinds};
use crate::entities::Entities;
use crate::inventory::OFFHAND;
use crate::item::{self, ItemStack};
use crate::level::Level;
use crate::survival::player::Env;
use crate::survival::rules::Rule;
use crate::survival::{Effect, play_sound};

/// `InteractionResult`, as far as the server acts on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interaction {
    Pass,
    /// Handled; the player swings its arm.
    Success,
    /// Handled without a swing.
    Consume,
}

impl Interaction {
    /// `consumesAction`.
    pub fn consumes(self) -> bool {
        self != Interaction::Pass
    }
}

/// The breeding food item tag of an animal kind (`isFood`).
pub fn food_tag(kind: Kind) -> Option<&'static str> {
    Some(match kind {
        Kind::Cow => "minecraft:cow_food",
        Kind::Pig => "minecraft:pig_food",
        Kind::Sheep => "minecraft:sheep_food",
        Kind::Chicken => "minecraft:chicken_food",
        _ => return None,
    })
}

/// `Animal.isFood(item)`.
pub fn is_food(kind: Kind, item: u16) -> bool {
    if kind.family() != super::Family::Classic {
        return super::fam::is_food(kind, item);
    }
    food_tag(kind).is_some_and(|tag| {
        crate::player::play::game_data().is_some_and(|d| d.item_in_tag(item, tag))
    })
}

/// `isPanicking`: a panic goal runs.
/// The target of `PanicGoal.canUse`: a burning mob runs to the nearest water
/// (`lookForWater(level, mob, 5)`, the block's corner), else `DefaultRandomPos.getPos(5, 4)`.
pub fn panic_pos(t: &mut super::Tick<'_, '_>) -> Option<Vec3> {
    let fire_immune = t.core().kind().info().fire_immune;
    if t.mob_ref().fire_ticks > 0 && !fire_immune {
        let me = t.block_pos();
        if t.level.block(me).collision_shape().is_empty() {
            let mut found = None;
            super::brain::behavior::for_manhattan(me, 5, 1, |p| {
                if super::is_water(t.level.block(p)) {
                    found = Some(p);
                    return true;
                }
                false
            });
            if let Some(p) = found {
                return Some(Vec3::new(f64::from(p.x), f64::from(p.y), f64::from(p.z)));
            }
        }
    }
    super::randompos::default_pos(t, 5, 4)
}

pub fn is_panicking(m: &MobData) -> bool {
    m.goals
        .slots
        .iter()
        .any(|s| s.running && matches!(s.goal, super::goals::Goal::Panic { .. }))
}

/// `BreedGoal.getFreePartner`: the closest animal of the same type in love within 8 blocks.
pub fn free_partner(t: &mut super::Tick<'_, '_>) -> Option<EntityId> {
    let kind = t.mob_ref().kind;
    let area = t.aabb().inflate(8.0, 8.0, 8.0);
    let pos = t.pos();
    let mut ids = std::mem::take(&mut t.cx.ids);
    ids.clear();
    t.ents.store.for_each_in(&area, false, |id| ids.push(id));
    let mut best: Option<(EntityId, f64)> = None;
    for &id in &ids {
        if id == t.id {
            continue;
        }
        let Some(m) = t.ents.mob(id) else {
            continue;
        };
        if !m.is_alive() {
            continue;
        }
        if kind.family() == super::Family::Classic {
            if m.kind != kind || m.in_love <= 0 {
                continue;
            }
        } else if !super::fam::can_mate(t.mob_ref(), m) {
            continue;
        }
        // `AbstractHorse.canParent`: neither parent carries a rider or rides.
        if super::fam::companion::is_horse(kind) {
            let riding = &t.ents.riding;
            if [t.id, id]
                .iter()
                .any(|&a| !riding.passengers_of(a).is_empty() || riding.vehicle_of(a).is_some())
            {
                continue;
            }
        }
        let Some(e) = t.ents.store.get(id) else {
            continue;
        };
        let d = e.pos().distance_to_sqr(pos);
        if d > 64.0 {
            continue;
        }
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((id, d));
        }
    }
    t.cx.ids = ids;
    best.map(|(id, _)| id)
}

/// `level.broadcastEntityEvent(entity, event)`.
pub fn entity_event(ents: &Entities, env: &mut Env<'_>, id: EntityId, event: i8) {
    if let Some(e) = ents.store.get(id) {
        env.out.push(Effect::Tracking {
            entity: id,
            with_self: false,
            packet: SharedPacket::of(&EntityEvent {
                entity_id: e.network_id().0,
                event,
            }),
        });
    }
}

/// A sound at an entity's position (`playSound(sound, volume, pitch)` of the entity).
pub fn sound_at(
    ents: &Entities,
    env: &mut Env<'_>,
    id: EntityId,
    sound: &str,
    source: SoundSource,
    pitch: f32,
) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let seed = crate::survival::sound_seed();
    play_sound(env.out, None, pos, sound, source, 1.0, pitch, seed);
}

/// `Animal.spawnChildFromBreeding` and `finalizeSpawnChildFromBreeding`.
pub fn breed(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    parent: EntityId,
    partner: EntityId,
) {
    let (Some(a), Some(b)) = (ents.mob(parent), ents.mob(partner)) else {
        return;
    };
    let kind = a.kind;
    let (va, vb) = (a.variant, b.variant);
    let (ca, cb) = (classic::sheep(a).color, classic::sheep(b).color);
    let cause = a.love_cause.or(b.love_cause);
    let Some(pos) = ents.store.get(parent).map(|e| e.pos()) else {
        return;
    };
    let seed = crate::mob::new_entity_seed();
    let child_kind = match (ents.mob(parent), ents.mob(partner)) {
        (Some(a), Some(b)) if kind.family() != super::Family::Classic => {
            super::fam::offspring_kind(a, b)
        }
        _ => kind,
    };
    let mut child = MobData::new(child_kind, seed);
    // The horses' and llamas' `getBreedOffspring` draw only from the parent's own random, in
    // their `breed_offspring`.
    let horse_like =
        super::fam::companion::is_horse(kind) || matches!(kind, Kind::Llama | Kind::TraderLlama);
    let pick = if horse_like {
        true
    } else {
        ents.mob_mut(parent)
            .map(|m| m.random.next_boolean())
            .unwrap_or(true)
    };
    child.variant = if pick { va } else { vb };
    if kind == Kind::Sheep {
        classic::sheep_mut(&mut child).color = offspring_color(ca, cb, pick);
    }
    if kind.family() != super::Family::Classic
        && let (Some(a), Some(b)) = (ents.mob(parent), ents.mob(partner))
    {
        if horse_like {
            let mut random = a.random.clone();
            super::fam::breed_offspring(a, b, &mut child, &mut random);
            if let Some(m) = ents.mob_mut(parent) {
                m.random = random;
            }
        } else {
            let mut random = vanillite_base::random::LegacyRandom::new(env.random.next_long());
            super::fam::breed_offspring(a, b, &mut child, &mut random);
        }
    }
    child.age = -24000;
    child.baby = true;
    let uuid = crate::mob::new_uuid();
    // The breed event (`vanillite_abi::event::ENTITY_BREED`): the experience is drawn from
    // the parent's random first, which nothing else draws from in between.
    let mut experience = None;
    if let Some(hooks) = ents.hooks_for(vanillite_abi::event::ENTITY_BREED) {
        let xp = if env.rules.bool(Rule::MOB_DROPS) {
            ents.mob_mut(parent)
                .map_or(1, |m| m.random.next_int_bounded(7) + 1)
        } else {
            0
        };
        let Some(xp) = breed_event(
            &*hooks,
            ents,
            (uuid, child.kind),
            parent,
            partner,
            cause.unwrap_or(0),
            xp,
        ) else {
            // The attempt fails: the parents lose their love.
            for id in [parent, partner] {
                if let Some(m) = ents.mob_mut(id) {
                    m.in_love = 0;
                    m.love_cause = None;
                }
            }
            return;
        };
        experience = Some(xp);
    }
    // `snapTo(pos, 0, 0)` keeps the constructor's head yaw.
    let rot = Rotation {
        y_head_rot: super::constructor_y_rot(seed),
        ..Rotation::default()
    };
    let child = super::spawn(ents, level.network_ids(), uuid, pos, rot, child);
    let parent_uuid = ents.store.get(parent).map_or(0, |e| e.uuid());
    ents.spawned(child, vanillite_abi::spawn_reason::BREEDING, parent_uuid);
    award_breeding(ents, parent, partner, Some(child));
    for id in [parent, partner] {
        if let Some(m) = ents.mob_mut(id) {
            m.age = 6000;
            m.in_love = 0;
            m.love_cause = None;
        }
    }
    entity_event(ents, env, parent, 18);
    // `finalizeSpawnChildFromBreeding`: `addFreshEntity(new ExperienceOrb(...))`, one orb of
    // the whole value without `award`'s merge roll.
    let xp = match experience {
        Some(xp) => xp,
        None if env.rules.bool(Rule::MOB_DROPS) => ents
            .mob_mut(parent)
            .map_or(1, |m| m.random.next_int_bounded(7) + 1),
        None => 0,
    };
    if xp > 0 {
        crate::experience_orb::spawn(ents, level.network_ids(), pos, Vec3::ZERO, xp);
    }
}

/// The plugins' breed event (`vanillite_abi::event::ENTITY_BREED`) of `parent` and `partner`
/// about to make the child `child` (its UUID and kind, not added yet) for the player
/// `breeder` (its UUID or 0): `None` when a plugin cancelled it, else the experience
/// (`experience` or a handler's).
pub fn breed_event(
    hooks: &dyn crate::hooks::GameHooks,
    ents: &Entities,
    child: (u128, Kind),
    parent: EntityId,
    partner: EntityId,
    breeder: u128,
    experience: i32,
) -> Option<i32> {
    let uuid_of = |id| ents.store.get(id).map_or(0, |e| e.uuid());
    let mut ev = crate::hooks::Event {
        entity: child.0,
        entity_type: child.1.type_id().name().to_owned(),
        other: uuid_of(parent),
        source: uuid_of(partner),
        player: breeder,
        value: i64::from(experience),
        ..crate::hooks::Event::new(vanillite_abi::event::ENTITY_BREED)
    };
    hooks.event(crate::hooks::HookWorld::entities(ents), &mut ev);
    (!ev.cancelled).then(|| crate::hooks::experience_value(ev.value))
}

/// The player of a UUID in this world (`level.getPlayerByUUID`).
pub fn player_by_uuid(ents: &Entities, uuid: Option<u128>) -> Option<EntityId> {
    uuid.and_then(|u| ents.players_by_uuid.get(&u).copied())
}

/// The award of `finalizeSpawnChildFromBreeding` (and the turtle's and fox's breed goals):
/// the love cause of `parent`, else of `partner`, gets `animals_bred` and `bred_animals`.
/// Called before the love is reset.
pub fn award_breeding(
    ents: &mut Entities,
    parent: EntityId,
    partner: EntityId,
    child: Option<EntityId>,
) {
    use crate::advancements::trigger::{Event, TriggerKind};
    use crate::advancements::view;
    let cause = ents
        .mob(parent)
        .and_then(|m| m.love_cause)
        .and_then(|u| player_by_uuid(ents, Some(u)))
        .or_else(|| player_by_uuid(ents, ents.mob(partner).and_then(|m| m.love_cause)));
    let Some(player) = cause else {
        return;
    };
    if let Some(p) = ents.player_mut(player) {
        p.stats.award(crate::stats::custom::ANIMALS_BRED, 1);
    }
    if !view::listens(ents, player, TriggerKind::BRED_ANIMALS) {
        return;
    }
    if let (Some(a), Some(b)) = (view::snapshot(ents, parent), view::snapshot(ents, partner)) {
        let c = child.and_then(|c| view::snapshot(ents, c));
        if let Some(p) = ents.player_mut(player) {
            p.advancements.trigger(Event::BredAnimals(a, b, c));
        }
    }
}

/// `FILLED_BUCKET.trigger(player, bucket)`.
pub fn filled_bucket(ents: &mut Entities, player: EntityId, bucket: &ItemStack) {
    if let Some(p) = ents.player_mut(player) {
        p.advancements.trigger_item(
            crate::advancements::trigger::TriggerKind::FILLED_BUCKET,
            bucket,
        );
    }
}

/// The entity of a UUID in this world, players first (`level.getEntity(uuid)`).
pub fn entity_by_uuid(ents: &Entities, uuid: u128) -> Option<EntityId> {
    ents.players_by_uuid.get(&uuid).copied().or_else(|| {
        ents.mobs
            .iter()
            .map(|(id, _)| id)
            .find(|&id| ents.store.get(id).is_some_and(|e| e.uuid() == uuid))
    })
}

/// `onItemPickup(itemEntity)` of `picker` for an item entity with an owner (`thrower`):
/// `thrown_item_picked_up_by_entity` for a throwing player and, when a player picks it up,
/// `thrown_item_picked_up_by_player` (`ServerPlayer.onItemPickup`). `stack` is the item
/// entity's stack after the pickup (`getItem()`).
pub fn item_picked_up(ents: &mut Entities, picker: EntityId, thrower: u128, stack: &ItemStack) {
    use crate::advancements::trigger::{Event, TriggerKind};
    use crate::advancements::view;
    let by_entity = TriggerKind::THROWN_ITEM_PICKED_UP_BY_ENTITY;
    if let Some(owner) = ents.players_by_uuid.get(&thrower).copied() {
        view::fire(ents, owner, by_entity, |ents| {
            Event::EntityItem(by_entity, view::snapshot(ents, picker), stack.clone())
        });
    }
    let by_player = TriggerKind::THROWN_ITEM_PICKED_UP_BY_PLAYER;
    if ents.player(picker).is_some()
        && view::listens(ents, picker, by_player)
        && let Some(owner) = entity_by_uuid(ents, thrower)
    {
        view::fire(ents, picker, by_player, |ents| {
            Event::EntityItem(by_player, view::snapshot(ents, owner), stack.clone())
        });
    }
}

/// `SUMMONED_ENTITY.trigger(player, entity)` for each of `players`.
pub fn award_summoned(ents: &mut Entities, players: &[EntityId], entity: EntityId) {
    use crate::advancements::trigger::{Event, TriggerKind};
    use crate::advancements::view;
    let kind = TriggerKind::SUMMONED_ENTITY;
    let listening: Vec<EntityId> = players
        .iter()
        .copied()
        .filter(|&p| view::listens(ents, p, kind))
        .collect();
    if listening.is_empty() {
        return;
    }
    let Some(e) = view::snapshot(ents, entity) else {
        return;
    };
    for p in listening {
        if let Some(p) = ents.player_mut(p) {
            p.advancements.trigger(Event::Entity(kind, e.clone()));
        }
    }
}

/// The players whose box intersects `aabb` (`getEntitiesOfClass(ServerPlayer.class, aabb)`).
pub fn players_in(ents: &Entities, aabb: &vanillite_base::Aabb) -> Vec<EntityId> {
    ents.players
        .iter()
        .map(|(id, _)| id)
        .filter(|&id| {
            ents.store
                .get(id)
                .is_some_and(|e| e.aabb().intersects(aabb))
        })
        .collect()
}

/// `Sheep.getOffspringColor`: the dye two parent colours craft into, else one of them.
fn offspring_color(a: u8, b: u8, pick_first: bool) -> u8 {
    // `DyeColor` ids: 0 white, 1 orange, 2 magenta, 3 light blue, 4 yellow, 5 lime, 6 pink,
    // 7 gray, 8 light gray, 9 cyan, 10 purple, 11 blue, 12 brown, 13 green, 14 red, 15 black.
    let (lo, hi) = (a.min(b), a.max(b));
    let mixed = match (lo, hi) {
        (0, 15) => Some(7),
        (0, 7) => Some(8),
        (0, 14) => Some(6),
        (0, 11) => Some(3),
        (0, 13) => Some(5),
        (4, 14) => Some(1),
        (10, 6) | (6, 10) => Some(2),
        (11, 14) => Some(10),
        (11, 13) => Some(9),
        _ => None,
    };
    mixed.unwrap_or(if pick_first { a } else { b })
}

/// The inventory index of a hand.
pub fn hand_index(ents: &Entities, player: EntityId, hand: Hand) -> usize {
    match hand {
        Hand::MainHand => ents.player(player).map_or(0, |p| p.inventory.selected),
        Hand::OffHand => OFFHAND,
    }
}

/// The stack in a player's hand.
pub fn held(ents: &Entities, player: EntityId, hand: Hand) -> ItemStack {
    let i = hand_index(ents, player, hand);
    ents.player(player)
        .map(|p| p.inventory.get(i).clone())
        .unwrap_or_default()
}

pub fn infinite(ents: &Entities, player: EntityId) -> bool {
    ents.player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative)
}

/// `usePlayerItem`: one item of the hand is used up unless the player has infinite materials.
pub fn use_item(ents: &mut Entities, player: EntityId, hand: Hand) {
    if infinite(ents, player) {
        return;
    }
    let i = hand_index(ents, player, hand);
    if let Some(p) = ents.player_mut(player)
        && let Some(s) = p.inventory.get_mut(i)
    {
        s.shrink(1);
    }
}

/// `ItemUtils.createFilledResult`: the hand's stack turns into `filled` (one item of it).
pub fn fill_held(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    hand: Hand,
    filled: ItemStack,
) {
    let creative = infinite(ents, player);
    let i = hand_index(ents, player, hand);
    let pos = ents.store.get(player).map_or(Vec3::ZERO, |e| e.pos());
    let Some(p) = ents.player_mut(player) else {
        return;
    };
    let mut filled = filled;
    if creative {
        if p.inventory.find_matching(&filled).is_none() {
            p.inventory.add(None, &mut filled, true);
        }
        return;
    }
    let Some(s) = p.inventory.get_mut(i) else {
        return;
    };
    s.shrink(1);
    if s.is_empty() {
        *s = filled;
    } else if !p.inventory.add(None, &mut filled, false) {
        // `drop(filled, false)`: it falls in front of the player.
        let (uuid, y_rot, _, item_random) = crate::mob::new_item_entity();
        env.out.push(Effect::DropItem {
            random: item_random,
            uuid,
            pos: pos + Vec3::new(0.0, 1.32, 0.0),
            velocity: Vec3::new(0.0, 0.1, 0.0),
            y_rot,
            pickup_delay: 40,
            stack: filled,
        });
    }
}

/// `hurtAndBreak(amount, player, hand)` of the stack in a hand: the `item_damage` effects
/// (Unbreaking) from the level random, and the break event of the hand when it breaks.
pub fn hurt_held(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    hand: Hand,
    amount: i32,
) {
    let n = crate::durability::change(
        &held(ents, player, hand),
        amount,
        infinite(ents, player),
        env.random,
    );
    if n == 0 {
        return;
    }
    let i = hand_index(ents, player, hand);
    let Some(p) = ents.player_mut(player) else {
        return;
    };
    let crate::player::PlayerData {
        inventory,
        advancements,
        stats,
        ..
    } = p;
    let Some(s) = inventory.get_mut(i) else {
        return;
    };
    let d = item::damage(s) + n;
    if crate::advancements::view::apply_damage(Some(advancements), Some(stats), s, d) {
        let slot = match hand {
            Hand::MainHand => item::EquipmentSlot::MainHand,
            Hand::OffHand => item::EquipmentSlot::OffHand,
        };
        crate::durability::broken_in_env(ents, env, player, slot);
    }
}

/// `Player.interactOn(entity, hand, location)` for a mob: the mob's own interaction, then the
/// held item's (`interactLivingEntity`).
#[allow(clippy::too_many_arguments)]
pub fn interact_on(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    sneaking: bool,
) -> Interaction {
    if ents
        .player(player)
        .is_none_or(|p| p.game_mode == GameMode::Spectator)
    {
        return Interaction::Pass;
    }
    if ents.boat(target).is_some() {
        // `AbstractBoat.interact`: leads first, then getting in.
        let r = super::tame::leash_interact(ents, env, player, target, hand, sneaking);
        if r.consumes() {
            return r;
        }
        let out_of_control = ents
            .boat(target)
            .is_some_and(|b| b.out_of_control_ticks >= 60.0);
        if !sneaking
            && !out_of_control
            && crate::riding::start_riding(ents, env, player, target, false)
        {
            return Interaction::Success;
        }
        // `AbstractChestBoat.interact`: sneaking, or a full boat, opens the chest.
        if let Some(b) = ents.boat(target).filter(|b| b.kind.chest) {
            let room = ents.riding.passengers_of(target).len() < b.kind.max_passengers();
            if room && !sneaking {
                return Interaction::Pass;
            }
            crate::minecart::open_container(ents, env, cx, player, target);
            // `PiglinAi.angerNearbyPiglins(player, true)`.
            super::fam::brainy::piglin::anger_nearby_piglins(level, ents, env, cx, player, true);
            return Interaction::Success;
        }
        return Interaction::Pass;
    }
    if let Some(kind) = ents.minecart(target).map(|m| m.kind) {
        let r = crate::minecart::interact(ents, env, cx, player, target, hand, sneaking);
        // `MinecartChest.interact`: `PiglinAi.angerNearbyPiglins(player, true)`.
        if kind == crate::minecart::MinecartKind::Chest && r.consumes() {
            super::fam::brainy::piglin::anger_nearby_piglins(level, ents, env, cx, player, true);
        }
        return r;
    }
    if !ents.mob(target).is_some_and(MobData::is_alive) {
        return Interaction::Pass;
    }
    let stack = held(ents, player, hand);
    let before = stack.count;
    let r = mob_interact(level, ents, env, cx, player, target, hand, sneaking, &stack);
    if r.consumes() {
        // Creative players keep their items.
        if infinite(ents, player) {
            let i = hand_index(ents, player, hand);
            if let Some(p) = ents.player_mut(player)
                && let Some(s) = p.inventory.get_mut(i)
                && s.item == stack.item
                && s.count < before
            {
                s.count = before;
            }
        }
        return r;
    }
    if !stack.is_empty() {
        return item_interact(ents, env, player, target, hand, &stack);
    }
    Interaction::Pass
}

/// `entity.gameEvent(event, source)`: at the entity's position.
pub fn entity_game_event(
    ents: &Entities,
    env: &mut Env<'_>,
    entity: EntityId,
    event: u16,
    source: Option<EntityId>,
) {
    if let Some(pos) = ents.store.get(entity).map(|e| e.pos()) {
        crate::survival::game_event(env.out, event, pos, source);
    }
}

/// `Mob.interact`: name tags and spawn eggs, leads, then `mobInteract`; the important
/// interactions and `mobInteract` post `ENTITY_INTERACT` when they consume the action.
#[allow(clippy::too_many_arguments)]
fn mob_interact(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    _cx: &mut MobContext,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    sneaking: bool,
    stack: &ItemStack,
) -> Interaction {
    let name = item::item_id(stack).name();
    // `CamelHusk.interact` and `AbstractNautilus.interact`: any interaction keeps it.
    if let Some(m) = ents.mob_mut(target)
        && matches!(
            m.kind,
            super::Kind::CamelHusk | super::Kind::Nautilus | super::Kind::ZombieNautilus
        )
    {
        m.persistent = true;
    }
    // `checkAndHandleImportantInteractions`.
    let interacted = |ents: &Entities, env: &mut Env<'_>| {
        let event = crate::level::game_event::events().entity_interact;
        entity_game_event(ents, env, target, event, Some(player));
    };
    if name == "minecraft:name_tag" {
        let r = name_tag(ents, player, target, hand, stack);
        if r.consumes() {
            interacted(ents, env);
            return r;
        }
    }
    if let Some(egg_kind) = spawn_egg_kind(stack) {
        // `SpawnEggItem.spawnOffspringFromSpawnEgg`; without an offspring the interaction
        // goes on to the leads and `mobInteract`.
        let own = ents.mob(target).map(|m| m.kind);
        if own == Some(egg_kind) && spawn_offspring(level, ents, player, target, hand) {
            interacted(ents, env);
            return Interaction::Success;
        }
    }
    // `Entity.interact`: leads (with their own game events).
    let r = super::tame::leash_interact(ents, env, player, target, hand, sneaking);
    if r != Interaction::Pass {
        return r;
    }
    // `mobInteract`.
    let r = own_interact(level, ents, env, _cx, player, target, hand, sneaking, stack);
    if r.consumes() {
        interacted(ents, env);
    }
    r
}

/// `mobInteract` of the mob kinds.
#[allow(clippy::too_many_arguments)]
fn own_interact(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    _cx: &mut MobContext,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    sneaking: bool,
    stack: &ItemStack,
) -> Interaction {
    let name = item::item_id(stack).name();
    let kind = ents.mob(target).map_or(Kind::Zombie, |m| m.kind);
    if kind.family() != super::Family::Classic {
        let mut ix = super::fam::Interact {
            level,
            ents,
            env,
            cx: _cx,
            player,
            target,
            hand,
            sneaking,
            stack,
        };
        return super::fam::interact(&mut ix, kind);
    }
    match kind {
        Kind::Creeper if super::fam::companion::in_tag(stack, "minecraft:creeper_igniters") => {
            // `Creeper.mobInteract`: the igniter's sound (not for the player), then `ignite`.
            let sound = if name == "minecraft:fire_charge" {
                "minecraft:item.firecharge.use"
            } else {
                "minecraft:item.flintandsteel.use"
            };
            let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
                return Interaction::Pass;
            };
            let pitch = ents
                .mob_mut(target)
                .map_or(1.0, |m| m.random.next_float() * 0.4 + 0.8);
            let seed = crate::survival::sound_seed();
            play_sound(
                env.out,
                Some(player),
                pos,
                sound,
                SoundSource::Hostile,
                1.0,
                pitch,
                seed,
            );
            if let Some(c) = ents
                .mob_mut(target)
                .and_then(super::fam::classic::try_creeper_mut)
            {
                c.ignited = true;
            }
            if item::is_damageable(stack) {
                hurt_held(ents, env, player, hand, 1);
            } else {
                let i = hand_index(ents, player, hand);
                if let Some(s) = ents.player_mut(player).and_then(|p| p.inventory.get_mut(i)) {
                    s.shrink(1);
                }
            }
            return Interaction::Success;
        }
        Kind::Cow if name == "minecraft:bucket" && !ents.mob(target).is_some_and(|m| m.baby) => {
            // `AbstractCow.mobInteract`: the milk sound is the player's own (`player.playSound`:
            // its client plays it, the others hear it).
            if let Some(pos) = ents.store.get(player).map(|e| e.pos()) {
                let seed = crate::survival::sound_seed();
                play_sound(
                    env.out,
                    Some(player),
                    pos,
                    "minecraft:entity.cow.milk",
                    SoundSource::Players,
                    1.0,
                    1.0,
                    seed,
                );
            }
            if let Some(milk) = vanillite_data::registry::ITEM.id("minecraft:milk_bucket") {
                fill_held(ents, env, player, hand, ItemStack::new(milk, 1));
            }
            return Interaction::Success;
        }
        Kind::Sheep if name == "minecraft:shears" => {
            let ready = ents
                .mob(target)
                .is_some_and(|m| m.is_alive() && !m.sheared && !m.baby);
            if ready {
                shear_sheep(level, ents, env, _cx, target, SoundSource::Players);
                let event = crate::level::game_event::events().shear;
                entity_game_event(ents, env, target, event, Some(player));
                hurt_held(ents, env, player, hand, 1);
                return Interaction::Success;
            }
            return Interaction::Consume;
        }
        Kind::Pig => {
            let food = is_food(kind, stack.item);
            let ridden = !ents.riding.passengers_of(target).is_empty();
            let saddled = ents.mob(target).is_some_and(|m| m.saddled);
            if !food && saddled && !ridden && !sneaking {
                crate::riding::start_riding(ents, env, player, target, false);
                return Interaction::Success;
            }
            let r = animal_interact(ents, env, player, target, hand, stack);
            if r.consumes() {
                return r;
            }
            if name == "minecraft:saddle" {
                return equip_saddle(ents, env, player, target, hand);
            }
            return Interaction::Pass;
        }
        _ => {}
    }
    if name == "minecraft:shears" && !sneaking && ents.mob(target).is_some_and(|m| m.saddled) {
        // `attemptToShearEquipment`: the saddle comes off.
        unequip_saddle(ents, env, target);
        let event = crate::level::game_event::events().shear;
        entity_game_event(ents, env, target, event, Some(player));
        hurt_held(ents, env, player, hand, 1);
        return Interaction::Success;
    }
    if kind.ageable() {
        return animal_interact(ents, env, player, target, hand, stack);
    }
    Interaction::Pass
}

/// `Animal.mobInteract`: food makes adults fall in love and babies grow faster.
/// `ItemUtils.createFilledResult` for the family kinds: the held item becomes `item`.
pub fn fill_held_pub(ix: &mut super::fam::Interact<'_, '_>, item: &str) {
    if let Some(id) = vanillite_data::registry::ITEM.id(item) {
        fill_held(ix.ents, ix.env, ix.player, ix.hand, ItemStack::new(id, 1));
    }
}

/// `stack.consume(1, player)` for the family kinds (creative players keep it).
pub fn use_held_pub(ix: &mut super::fam::Interact<'_, '_>) {
    use_item(ix.ents, ix.player, ix.hand);
}

/// `hurtAndBreak(amount, player, hand)` of the held item for the family kinds.
pub fn hurt_held_pub(ix: &mut super::fam::Interact<'_, '_>, amount: i32) {
    hurt_held(ix.ents, ix.env, ix.player, ix.hand, amount);
}

/// Whether the interacting player keeps its items (creative).
pub fn infinite_pub(ents: &Entities, player: EntityId) -> bool {
    infinite(ents, player)
}

/// `Animal.usePlayerItem`; `Axolotl.usePlayerItem` turns a bucket of tropical fish into a
/// water bucket.
fn use_player_item(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    stack: &ItemStack,
) {
    let axolotl = ents.mob(target).is_some_and(|m| m.kind == Kind::Axolotl);
    if axolotl
        && Some(stack.item) == vanillite_data::registry::ITEM.id("minecraft:tropical_fish_bucket")
    {
        if let Some(id) = vanillite_data::registry::ITEM.id("minecraft:water_bucket") {
            fill_held(ents, env, player, hand, ItemStack::new(id, 1));
        }
    } else {
        use_item(ents, player, hand);
    }
}

/// `Animal.mobInteract` for the family kinds: feeding, love mode and growing babies.
pub fn animal_interact_pub(ix: &mut super::fam::Interact<'_, '_>) -> Interaction {
    animal_interact(ix.ents, ix.env, ix.player, ix.target, ix.hand, ix.stack)
}

pub fn animal_interact(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    stack: &ItemStack,
) -> Interaction {
    let Some(m) = ents.mob(target) else {
        return Interaction::Pass;
    };
    if !is_food(m.kind, stack.item) {
        // `AgeableMob.mobInteract`: the golden dandelion.
        return super::age_lock::interact(ents, env, player, target, hand, stack)
            .unwrap_or(Interaction::Pass);
    }
    let age = m.age;
    let uuid = ents.store.get(player).map(|e| e.uuid());
    // `canFallInLove`: the happy ghast never falls in love from food, a `Turtle` not while
    // it carries an egg.
    let can_fall_in_love = m.in_love <= 0
        && m.kind != Kind::HappyGhast
        && !(m.kind == Kind::Turtle && super::fam::wildlife::st(m).has_egg);
    if age == 0 && can_fall_in_love {
        use_player_item(ents, env, player, target, hand, stack);
        if let Some(m) = ents.mob_mut(target) {
            m.in_love = 600;
            m.love_cause = uuid;
        }
        entity_event(ents, env, target, 18);
        eating_sound(ents, env, target);
        return Interaction::Success;
    }
    // `canAgeUp`: a locked baby passes on to the golden dandelion (not food).
    if m.baby && !m.age_locked {
        use_player_item(ents, env, player, target, hand, stack);
        if let Some(m) = ents.mob_mut(target) {
            age_up(m, speed_up_seconds_when_feeding(-age), true);
        }
        eating_sound(ents, env, target);
        return Interaction::Success;
    }
    // `super.mobInteract`: nothing else uses the food on the server.
    Interaction::Pass
}

/// `AgeableMob.getSpeedUpSecondsWhenFeeding`.
pub fn speed_up_seconds_when_feeding(ticks_until_adult: i32) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let seconds = ((ticks_until_adult / 20) as f32 * 0.1) as i32;
    seconds
}

/// `AgeableMob.ageUp(seconds, forced)`: a forced age up adds to `forcedAge` (and starts the
/// particle timer); reaching age 0 sets the age to `forcedAge`, the breeding cooldown.
pub fn age_up(m: &mut MobData, seconds: i32, forced: bool) {
    let new_age = (m.age + seconds * 20).min(0);
    let delta = new_age - m.age;
    m.age = new_age;
    if forced {
        m.forced_age += delta;
        if m.forced_age_timer == 0 {
            m.forced_age_timer = 40;
        }
    }
    if m.age == 0 {
        m.age = m.forced_age;
    }
}

/// `playEatingSound`: sheep, pigs and frogs have one.
pub fn eating_sound(ents: &mut Entities, env: &mut Env<'_>, target: EntityId) {
    let sound = match ents.mob(target) {
        // `Frog.playEatingSound`: follows the frog, no voice pitch.
        Some(m) if m.kind == Kind::Frog => {
            crate::survival::play_entity_sound(
                env.out,
                ents,
                None,
                target,
                "minecraft:entity.frog.eat",
                SoundSource::Neutral,
                2.0,
                1.0,
            );
            return;
        }
        Some(m) if m.kind == Kind::Sheep => "minecraft:entity.sheep.eat",
        Some(m) if m.kind == Kind::Pig => super::kinds::pig_eat_sound(m),
        _ => return,
    };
    combat::mob_sound(ents, env, target, sound);
}

/// `Sheep.shear`: the wool of the loot table `shearing/sheep` (1 to 3 of its colour) pops
/// out, one item each, pushed by the sheep's random.
pub(crate) fn shear_sheep(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    cx: &mut MobContext,
    target: EntityId,
    source: SoundSource,
) {
    crate::survival::play_entity_sound(
        env.out,
        ents,
        None,
        target,
        "minecraft:entity.sheep.shear",
        source,
        1.0,
        1.0,
    );
    let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
        return;
    };
    // `dropFromShearingLootTable` while the sheep still has its wool.
    let drops = {
        let mut t = super::Tick {
            level,
            ents: &mut *ents,
            env: &mut *env,
            cx,
            id: target,
        };
        super::fam::brainy::gift_loot(&mut t, "minecraft:shearing/sheep")
    };
    for drop in drops {
        for _ in 0..drop.count {
            let Some(m) = ents.mob_mut(target) else {
                return;
            };
            // `spawnAtLocation(stack, 1.0F)`: an item one block up with the default motion.
            let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
            let r = &mut m.random;
            let extra = Vec3::new(
                f64::from((r.next_float() - r.next_float()) * 0.1),
                f64::from(r.next_float() * 0.05),
                f64::from((r.next_float() - r.next_float()) * 0.1),
            );
            env.out.push(Effect::DropItem {
                random: item_random,
                uuid,
                pos: pos + Vec3::new(0.0, 1.0, 0.0),
                velocity: velocity + extra,
                y_rot,
                pickup_delay: 10,
                stack: drop.with_count(1),
            });
        }
    }
    if let Some(m) = ents.mob_mut(target) {
        m.sheared = true;
    }
    if let Some(mut e) = ents.store.get_mut(target) {
        e.mark_data_dirty();
    }
}

/// Saddles a pig (the saddle's `equippable` component with `equip_on_interact`).
fn equip_saddle(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
    hand: Hand,
) -> Interaction {
    let ok = ents
        .mob(target)
        .is_some_and(|m| m.is_alive() && !m.saddled && !m.baby);
    if !ok {
        return Interaction::Pass;
    }
    let pos = ents.store.get(target).map(|e| e.pos());
    if let Some(m) = ents.mob_mut(target) {
        m.saddled = true;
        m.persistent = true;
        // `onEquipItem`: the equip sound, seeded by the pig's random, then `EQUIP`.
        if !m.silent
            && let Some(pos) = pos
        {
            let seed = m.random.next_long();
            let source = m.sound_source();
            play_sound(
                env.out,
                None,
                pos,
                "minecraft:entity.pig.saddle",
                source,
                1.0,
                1.0,
                seed,
            );
        }
    }
    let event = crate::level::game_event::events().equip;
    entity_game_event(ents, env, target, event, Some(target));
    use_item(ents, player, hand);
    send_saddle(ents, env, target);
    Interaction::Success
}

/// Takes the saddle off and drops it (`attemptToShearEquipment`).
fn unequip_saddle(ents: &mut Entities, env: &mut Env<'_>, target: EntityId) {
    if let Some(m) = ents.mob_mut(target) {
        m.saddled = false;
    }
    sound_at(
        ents,
        env,
        target,
        "minecraft:item.saddle.unequip",
        SoundSource::Players,
        1.0,
    );
    send_saddle(ents, env, target);
    let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
        return;
    };
    if let Some(saddle) = vanillite_data::registry::ITEM.id("minecraft:saddle") {
        let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
        env.out.push(Effect::DropItem {
            random: item_random,
            uuid,
            pos: pos + Vec3::new(0.0, 1.0, 0.0),
            velocity,
            y_rot,
            pickup_delay: 10,
            stack: ItemStack::new(saddle, 1),
        });
    }
}

/// `set_equipment` with the saddle slot of a mob.
fn send_saddle(ents: &Entities, env: &mut Env<'_>, target: EntityId) {
    let Some(e) = ents.store.get(target) else {
        return;
    };
    let stack = if ents.mob(target).is_some_and(|m| m.saddled) {
        vanillite_data::registry::ITEM
            .id("minecraft:saddle")
            .map_or_else(ItemStack::default, |i| ItemStack::new(i, 1))
    } else {
        ItemStack::default()
    };
    env.out.push(Effect::Tracking {
        entity: target,
        with_self: false,
        packet: SharedPacket::of(
            &vanillite_protocol::packets::play::clientbound::SetEquipment {
                entity_id: e.network_id().0,
                slots: vec![(item::EquipmentSlot::Saddle.ordinal(), stack)],
            },
        ),
    });
}

/// `NameTagItem.interactLivingEntity`: a named tag names the mob and keeps it from
/// despawning.
fn name_tag(
    ents: &mut Entities,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    stack: &ItemStack,
) -> Interaction {
    let Some(name) = stack.component(vanillite_protocol::components::ids::CUSTOM_NAME) else {
        return Interaction::Pass;
    };
    let name: Box<[u8]> = name.into();
    if let Some(m) = ents.mob_mut(target) {
        m.custom_name = Some(name);
        m.persistent = true;
    }
    if let Some(mut e) = ents.store.get_mut(target) {
        e.mark_data_dirty();
    }
    use_item(ents, player, hand);
    Interaction::Success
}

/// The mob kind a spawn egg spawns (`entity_data` default `id`), by its item name.
pub fn spawn_egg_kind(stack: &ItemStack) -> Option<Kind> {
    let name = item::item_id(stack).name();
    let base = name.strip_suffix("_spawn_egg")?;
    // `SpawnEggItem.getType`: the type of the stack's `entity_data` (a patched one may name
    // another mob), else the egg's own.
    use vanillite_protocol::components::ids;
    if let Some(wire) = stack.component(ids::ENTITY_DATA)
        && let Ok(vanillite_nbt::Tag::Compound(data)) = vanillite_protocol::components_nbt::to_nbt(
            ids::ENTITY_DATA,
            wire,
            &vanillite_protocol::item::InstalledRegistries,
        )
        && let Some(id) = data.get("id").and_then(vanillite_nbt::Tag::as_str)
    {
        let qualified = if id.contains(':') {
            id.to_owned()
        } else {
            format!("minecraft:{id}")
        };
        return vanillite_data::entity::EntityTypeId::by_name(&qualified).and_then(Kind::of);
    }
    let ty = vanillite_data::entity::EntityTypeId::by_name(base)?;
    Kind::of(ty)
}

/// `SpawnEggItem.spawnOffspringFromSpawnEgg` on a mob of the egg's type: an `AgeableMob`'s
/// `getBreedOffspring` with itself as partner (none for the wandering trader), else a new
/// mob made a baby (only the zombies, piglins and zoglins can be one), then
/// `onOffspringSpawnedFromEgg`. Whether a baby came.
fn spawn_offspring(
    level: &Level,
    ents: &mut Entities,
    player: EntityId,
    target: EntityId,
    hand: Hand,
) -> bool {
    let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
        return false;
    };
    let Some(m) = ents.mob(target) else {
        return false;
    };
    let kind = m.kind;
    let seed = crate::mob::new_entity_seed();
    let mut child;
    if kind.ageable() {
        if kind == Kind::WanderingTrader {
            return false;
        }
        if kind == Kind::Villager {
            child = super::fam::village::villager::egg_offspring(
                level,
                ents.mob_mut(target).expect("checked above"),
                BlockPos::containing(pos.x, pos.y, pos.z),
                seed,
            );
        } else {
            let child_kind = if kind.family() == super::Family::Classic {
                kind
            } else {
                super::fam::offspring_kind(m, m)
            };
            child = MobData::new(child_kind, seed);
            child.variant = m.variant;
            if let Some(c) = classic::try_sheep_mut(&mut child) {
                c.color = classic::sheep(m).color;
            }
            // `getBreedOffspring(level, this)` draws from the parent's own random.
            let parent = m.clone();
            let mut random = parent.random.clone();
            if kind.family() == super::Family::Classic {
                random.next_boolean();
            } else {
                super::fam::breed_offspring(&parent, &parent, &mut child, &mut random);
            }
            if let Some(m) = ents.mob_mut(target) {
                m.random = random;
            }
        }
        child.age = -24000;
    } else if super::fam::undead::zombies::is_zombie(kind)
        || matches!(kind, Kind::Piglin | Kind::Zoglin)
    {
        child = MobData::new(kind, seed);
    } else {
        return false;
    }
    // `setBaby(true)` with the zombies' and piglins' baby speed.
    if child.kind == Kind::Piglin {
        child.baby = true;
        super::fam::brainy::piglin::apply_baby_speed(&mut child);
    } else {
        super::fam::undead::set_baby(&mut child, true);
    }
    if super::fam::undead::zombies::is_zombie(kind) {
        // `Zombie.onOffspringSpawnedFromEgg`: loot pickup from the parent's random.
        let special =
            crate::level::difficulty::at(level, BlockPos::containing(pos.x, pos.y, pos.z))
                .special_multiplier();
        if let Some(m) = ents.mob_mut(target) {
            child.can_pick_up_loot = m.random.next_float() < 0.55 * special;
        }
    }
    // `Fox.onOffspringSpawnedFromEgg`: the fox trusts the player.
    if child.kind == Kind::Fox
        && let Some(uuid) = ents.store.get(player).map(|e| e.uuid())
    {
        super::fam::companion::fox::add_trusted(&mut child, uuid);
    }
    let stack = held(ents, player, hand);
    if let Some(name) = stack.component(vanillite_protocol::components::ids::CUSTOM_NAME) {
        child.custom_name = Some(name.into());
    }
    let uuid = crate::mob::new_uuid();
    // `snapTo(pos, 0, 0)` keeps the constructor's head yaw.
    let rot = Rotation {
        y_head_rot: super::constructor_y_rot(seed),
        ..Rotation::default()
    };
    let id = super::spawn(ents, level.network_ids(), uuid, pos, rot, child);
    // `offspring.applyComponentsFromItemStack(spawnEggStack)`.
    super::item_components::apply_components(ents, id, &stack);
    let parent = ents.store.get(target).map_or(0, |e| e.uuid());
    if !ents.spawned(id, vanillite_abi::spawn_reason::SPAWN_ITEM_USE, parent) {
        return true;
    }
    use_item(ents, player, hand);
    true
}

/// `ItemStack.interactLivingEntity` after the mob passed: the stack's `equippable` component
/// with `equip_on_interact` (`Equippable.equipOnTarget`), then the item's own
/// `interactLivingEntity` (dyes on sheep; name tags are in [`mob_interact`]). A consumed
/// interaction posts `ENTITY_INTERACT` (`Player.interactOn`).
fn item_interact(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    stack: &ItemStack,
) -> Interaction {
    let mut r = equip_on_target(ents, env, player, target, hand, stack);
    if r == Interaction::Pass {
        r = dye_sheep(ents, env, player, target, hand, stack);
    }
    if r.consumes() {
        let event = crate::level::game_event::events().entity_interact;
        entity_game_event(ents, env, target, event, Some(player));
    }
    r
}

/// `itemStack.interactLivingEntity(player, mob, hand)` for the family kinds that call it in
/// their `mobInteract` (saddles and harnesses through `Equippable.equipOnTarget`).
pub fn item_interact_pub(ix: &mut super::fam::Interact<'_, '_>) -> Interaction {
    item_interact(ix.ents, ix.env, ix.player, ix.target, ix.hand, ix.stack)
}

/// `LivingEntity.canUseSlot` of the kinds that restrict their saddle or body slot.
pub(crate) fn can_use_slot(m: &MobData, slot: item::EquipmentSlot) -> bool {
    use item::EquipmentSlot::{Body, Saddle};
    let adult = m.is_alive() && !m.baby;
    match (m.kind, slot) {
        (Kind::Pig | Kind::Strider, Saddle) | (Kind::HappyGhast | Kind::SulfurCube, Body) => adult,
        // `Camel.isTamed` is always true.
        (Kind::Camel | Kind::CamelHusk, Saddle) => adult,
        (Kind::Donkey | Kind::Mule, Saddle)
        | (Kind::Nautilus | Kind::ZombieNautilus, Saddle | Body) => adult && m.tame,
        _ => true,
    }
}

/// `Equippable.equipOnTarget` for the items whose component has `equip_on_interact` (saddles,
/// harnesses, nautilus armor): an alive mob of the component's allowed entities with that slot
/// usable and empty takes one item, which it drops for sure when it dies.
fn equip_on_target(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    stack: &ItemStack,
) -> Interaction {
    let Some(equippable) = item::equippable(stack).filter(|e| e.equip_on_interact) else {
        return Interaction::Pass;
    };
    let slot = equippable.slot;
    let Some(m) = ents.mob(target) else {
        return Interaction::Pass;
    };
    let kind = m.kind;
    // A pig keeps its saddle as a flag and takes it in its own `mobInteract`.
    let fits = kind != Kind::Pig
        && can_use_slot(m, slot)
        && m.equipment.get(slot).is_empty()
        && m.is_alive()
        && equippable.can_be_equipped_by(kind.type_id().0);
    if !fits {
        return Interaction::Pass;
    }
    let one = stack.with_count(1);
    // `itemStack.split(1)`: taken from the hand in creative too.
    let i = hand_index(ents, player, hand);
    if let Some(p) = ents.player_mut(player)
        && let Some(s) = p.inventory.get_mut(i)
    {
        s.shrink(1);
    }
    let Some(pos) = ents.store.get(target).map(|e| e.pos()) else {
        return Interaction::Pass;
    };
    let Some(m) = ents.mob_mut(target) else {
        return Interaction::Pass;
    };
    m.equipment.set(slot, one);
    m.equipment.set_guaranteed_drop(slot);
    // `LivingEntity.onEquipItem`: the equip sound (seeded by the mob's random) and `EQUIP`.
    if !m.silent {
        let saddle = slot == item::EquipmentSlot::Saddle;
        let sound = match kind {
            Kind::Strider if saddle => "minecraft:entity.strider.saddle",
            Kind::Camel if saddle => "minecraft:entity.camel.saddle",
            Kind::CamelHusk if saddle => "minecraft:entity.camel_husk.saddle",
            Kind::Nautilus | Kind::ZombieNautilus if saddle => {
                if m.fluids.eye_in_water {
                    "minecraft:item.nautilus_saddle_underwater_equip"
                } else {
                    "minecraft:item.nautilus_saddle_equip"
                }
            }
            _ => &equippable.equip_sound,
        };
        let seed = m.random.next_long();
        let source = kind.sound_source();
        play_sound(env.out, None, pos, sound, source, 1.0, 1.0, seed);
    }
    let event = crate::level::game_event::events().equip;
    entity_game_event(ents, env, target, event, Some(target));
    if let Some(update) = super::equipment::tracking_update(ents, target) {
        env.out.push(update);
    }
    Interaction::Success
}

/// `DyeItem.interactLivingEntity`: a dye colours an alive, unshorn sheep of another colour.
fn dye_sheep(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    stack: &ItemStack,
) -> Interaction {
    let Some(color) = crate::block::sign::dye_color(stack)
        .and_then(|c| crate::block::sign::DYE_COLORS.iter().position(|d| *d == c))
        .and_then(|c| u8::try_from(c).ok())
    else {
        return Interaction::Pass;
    };
    let Some(m) = ents.mob(target) else {
        return Interaction::Pass;
    };
    let old = super::fam::classic::sheep(m).color;
    if m.kind != Kind::Sheep || !m.is_alive() || m.sheared || old == color {
        return Interaction::Pass;
    }
    let uuid = |ents: &Entities, id| ents.store.get(id).map_or(0, |e| e.uuid());
    let name = crate::block::sign::DYE_COLORS[usize::from(color)];
    if !env.hooks.sheep_dye_wool(
        crate::hooks::HookWorld::entities(ents),
        uuid(ents, target),
        name,
        uuid(ents, player),
    ) {
        return Interaction::Pass;
    }
    // `level.playSound(player, sheep, ...)`: the player's own client plays it itself.
    crate::survival::play_entity_sound(
        env.out,
        ents,
        Some(player),
        target,
        "minecraft:item.dye.use",
        SoundSource::Players,
        1.0,
        1.0,
    );
    if let Some(m) = ents.mob_mut(target) {
        super::fam::classic::sheep_mut(m).color = color;
    }
    if let Some(mut e) = ents.store.get_mut(target) {
        e.mark_data_dirty();
    }
    use_item(ents, player, hand);
    Interaction::Success
}

/// `SpawnEggItem.useOn`: spawns the egg's mob on the clicked block face.
#[allow(clippy::too_many_arguments)]
pub fn use_spawn_egg_on(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &NetworkIds,
    kind: Kind,
    pos: vanillite_base::BlockPos,
    face: vanillite_base::Direction,
    stack: &mut ItemStack,
    infinite: bool,
    player: EntityId,
) -> bool {
    // `EntityType.canSpawn`: no monsters in Peaceful (FAIL, the egg is kept).
    if env.difficulty == crate::survival::rules::Difficulty::Peaceful && !kind.allowed_in_peaceful()
    {
        return false;
    }
    let state = level.block(pos);
    // An empty collision shape: spawn inside, else on the face.
    let spawn_pos = if state.collision_shape().is_empty() {
        pos
    } else {
        pos.relative(face)
    };
    let below_face = spawn_pos != pos && face == vanillite_base::Direction::Up;
    let seed = crate::mob::new_entity_seed();
    let mut m = MobData::new(kind, seed);
    // `EntityType.spawn(..., shouldOffsetY, shouldOffsetYMore)`: above the block's shape,
    // facing a random yaw, then `finalizeSpawn`.
    let y_off = spawn_y_offset(level, spawn_pos, below_face, kind);
    let p = Vec3::new(
        f64::from(spawn_pos.x) + 0.5,
        f64::from(spawn_pos.y) + y_off,
        f64::from(spawn_pos.z) + 0.5,
    );
    let yaw = vanillite_base::math::wrap_degrees_f(env.random.next_float() * 360.0);
    let mut ctx =
        super::fam::SpawnContext::at(level, spawn_pos, super::fam::SpawnReason::SpawnItemUse);
    kinds::finalize_spawn(&mut m, &mut ctx, env.random);
    if let Some(name) = stack.component(vanillite_protocol::components::ids::CUSTOM_NAME) {
        // `applyImplicitComponent(CUSTOM_NAME)` only names it (no `setPersistenceRequired`).
        m.custom_name = Some(name.into());
    }
    let rot = Rotation {
        y_rot: yaw,
        x_rot: 0.0,
        y_head_rot: yaw,
    };
    let uuid = crate::mob::new_uuid();
    m.y_body_rot = yaw;
    let id = super::spawn(ents, network_ids, uuid, p, rot, m);
    super::item_components::apply_spawn_item(ents, id, stack);
    if !ents.spawned(id, vanillite_abi::spawn_reason::SPAWN_ITEM_USE, 0) {
        return true;
    }
    play_ambient_sound(ents, env, id);
    if !infinite {
        stack.shrink(1);
    }
    // `spawnMob`: `gameEvent(user, ENTITY_PLACE, spawnPos)`.
    crate::survival::game_event(
        env.out,
        crate::level::game_event::events().entity_place,
        Vec3::new(
            f64::from(spawn_pos.x) + 0.5,
            f64::from(spawn_pos.y) + 0.5,
            f64::from(spawn_pos.z) + 0.5,
        ),
        Some(player),
    );
    true
}

/// `EntityType.spawn(level, stack, null, pos, reason, tryMoveDown, movedUp)` of a mob made
/// from an item without a block click (spawn eggs from dispensers and on liquids, mob
/// buckets): the mob above the block's shape (with `try_move_down`), facing a random yaw,
/// `finalizeSpawn`, the item's components and `entity_data`, for buckets
/// `loadFromBucketTag` and `setFromBucket`; then `addFreshEntity` and its ambient sound.
pub fn spawn_from_item(
    level: &mut Level,
    kind: Kind,
    stack: &ItemStack,
    pos: vanillite_base::BlockPos,
    reason: super::fam::SpawnReason,
    try_move_down: bool,
    moved_up: bool,
) -> Option<EntityId> {
    let seed = crate::mob::new_entity_seed();
    let mut m = MobData::new(kind, seed);
    let y_off = if try_move_down {
        spawn_y_offset(level, pos, moved_up, kind)
    } else {
        0.0
    };
    let p = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + y_off,
        f64::from(pos.z) + 0.5,
    );
    let yaw = vanillite_base::math::wrap_degrees_f(level.random().next_float() * 360.0);
    let mut random = level.random().clone();
    let mut ctx = super::fam::SpawnContext::at(level, pos, reason);
    kinds::finalize_spawn(&mut m, &mut ctx, &mut random);
    *level.random() = random;
    // `createDefaultStackConfig`: the custom name (the other components after the spawn).
    if let Some(name) = stack.component(vanillite_protocol::components::ids::CUSTOM_NAME) {
        // `applyImplicitComponent(CUSTOM_NAME)` only names it (no `setPersistenceRequired`).
        m.custom_name = Some(name.into());
    }
    if reason == super::fam::SpawnReason::Bucket {
        super::fam::load_from_bucket(&mut m, stack);
    }
    m.y_body_rot = yaw;
    let rot = Rotation {
        y_rot: yaw,
        x_rot: 0.0,
        y_head_rot: yaw,
    };
    let uuid = crate::mob::new_uuid();
    let (ents, network_ids, _) = level.entity_parts();
    let id = super::spawn(ents, network_ids, uuid, p, rot, m);
    super::item_components::apply_spawn_item(ents, id, stack);
    if reason == super::fam::SpawnReason::Bucket {
        super::fam::load_bucket_flags(ents, id, stack);
    }
    if !ents.spawned(id, reason.abi(), 0) {
        return None;
    }
    // `mob.playAmbientSound()`: `makeSound` draws the volume and pitch before `playSound`
    // checks `isSilent`.
    let m = ents.mob_mut(id)?;
    let Some(sound) = kinds::ambient_sound(m) else {
        return Some(id);
    };
    let volume = super::fam::mob_sound_volume(m);
    let a = m.random.next_float();
    let b = m.random.next_float();
    let pitch = (a - b) * 0.2 + if m.baby { 1.5 } else { 1.0 };
    if !m.silent {
        #[allow(clippy::as_conversions)]
        let source = m.sound_source() as u8;
        let sound = vanillite_data::registry::SOUND_EVENT.id(sound).unwrap_or(0);
        level.play_sound(None, p.x, p.y, p.z, sound, source, volume, pitch);
    }
    Some(id)
}

/// `Mob.playAmbientSound` of a new mob (`EntityType.spawn`).
fn play_ambient_sound(ents: &mut Entities, env: &mut Env<'_>, id: EntityId) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let Some(m) = ents.mob_mut(id) else {
        return;
    };
    // `makeSound`: the volume and pitch are drawn before `playSound` checks `isSilent`.
    let Some(sound) = kinds::ambient_sound(m) else {
        return;
    };
    let volume = super::fam::mob_sound_volume(m);
    let a = m.random.next_float();
    let b = m.random.next_float();
    let pitch = (a - b) * 0.2 + if m.baby { 1.5 } else { 1.0 };
    if m.silent {
        return;
    }
    let source = m.sound_source();
    crate::survival::play_sound(
        env.out,
        None,
        pos,
        sound,
        source,
        volume,
        pitch,
        crate::survival::sound_seed(),
    );
}

/// `EntityType.getYOffset`: how far up from the block the new mob stands.
pub(crate) fn spawn_y_offset(
    level: &Level,
    spawn_pos: vanillite_base::BlockPos,
    moved_up: bool,
    kind: Kind,
) -> f64 {
    use vanillite_base::Aabb;
    let mut area = Aabb::of_block(spawn_pos);
    if moved_up {
        area = area.expand_towards(0.0, -1.0, 0.0);
    }
    let mut boxes = Vec::new();
    super::physics::block_boxes(&level.view(), &area, &mut boxes);
    let info = kind.type_id().info();
    let half = f64::from(info.width) / 2.0;
    let (x, y, z) = (
        f64::from(spawn_pos.x) + 0.5,
        f64::from(spawn_pos.y) + 1.0,
        f64::from(spawn_pos.z) + 0.5,
    );
    let entity = Aabb::new(
        x - half,
        y,
        z - half,
        x + half,
        y + f64::from(info.height),
        z + half,
    );
    1.0 + super::physics::collide_axis(&boxes, &entity, 1, if moved_up { -2.0 } else { -1.0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sheep_colours_mix_like_dyes() {
        // White and black make gray, red and yellow orange; others keep a parent's colour.
        assert_eq!(offspring_color(0, 15, true), 7);
        assert_eq!(offspring_color(14, 4, false), 1);
        assert_eq!(offspring_color(1, 3, true), 1);
        assert_eq!(offspring_color(1, 3, false), 3);
    }
}
