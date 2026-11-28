//! Owners and leads, shared by every tameable and leashable mob: `TamableAnimal` (taming,
//! sitting, following and protecting the owner: `SitWhenOrderedToGoal`, `FollowOwnerGoal`,
//! `OwnerHurtByTargetGoal`, `OwnerHurtTargetGoal`) and `Leashable` (leads held by players,
//! mobs or fence knots). The tameable kinds plug in through [`tame`] and the goals.

use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_config::GameMode;
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{SetEntityLink, SoundSource};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_world::entity::{EntityFlags, EntityId, NetworkIds, NewEntity};

use super::animal::Interaction;
use super::kinds::Kind;
use super::{MobData, Tick, nav};
use crate::entities::Entities;
use crate::item::ItemStack;
use crate::level::Level;
use crate::survival::player::Env;
use crate::survival::rules::Rule;
use crate::survival::{Effect, play_sound};

/// `TamableAnimal.tame(player)`: the player owns the mob from now on.
pub fn tame(m: &mut MobData, owner: u128) {
    m.tame = true;
    m.owner = Some(owner);
    m.persistent = true;
}

/// The plugins' tame event (`vanillite_abi::event::ENTITY_TAME`) of `animal` by the player
/// `owner` (its UUID): whether it may (`false`: the attempt fails). Without listeners one
/// branch.
pub fn tame_event(ents: &Entities, animal: EntityId, owner: u128) -> bool {
    let Some(hooks) = ents.hooks_for(vanillite_abi::event::ENTITY_TAME) else {
        return true;
    };
    let Some(e) = ents.store.get(animal) else {
        return true;
    };
    let mut ev = crate::hooks::Event {
        entity: e.uuid(),
        entity_type: e.kind().name().to_owned(),
        other: owner,
        player: owner,
        ..crate::hooks::Event::new(vanillite_abi::event::ENTITY_TAME)
    };
    hooks.event(crate::hooks::HookWorld::entities(ents), &mut ev);
    !ev.cancelled
}

/// The `tame_animal` trigger of `TamableAnimal.tame(player)` and
/// `AbstractHorse.tameWithName(player)`.
pub fn award_tame(ents: &mut Entities, animal: EntityId, player: EntityId) {
    use crate::advancements::trigger::{Event, TriggerKind};
    use crate::advancements::view;
    if !view::listens(ents, player, TriggerKind::TAME_ANIMAL) {
        return;
    }
    if let Some(e) = view::snapshot(ents, animal)
        && let Some(p) = ents.player_mut(player)
    {
        p.advancements
            .trigger(Event::Entity(TriggerKind::TAME_ANIMAL, e));
    }
}

/// The owner entity of a tamed mob in this world (`getOwner`).
pub fn owner_of(ents: &Entities, m: &MobData) -> Option<EntityId> {
    ents.player_by_uuid(m.owner?)
}

/// `setInSittingPose`.
pub fn set_in_sitting_pose(t: &mut Tick<'_, '_>, on: bool) {
    if t.mob_ref().in_sitting_pose != on {
        t.mob().in_sitting_pose = on;
        t.core_mut().mark_data_dirty();
    }
}

/// `SitWhenOrderedToGoal.canUse`.
pub fn sit_can_use(t: &mut Tick<'_, '_>) -> bool {
    let m = t.mob_ref();
    // An untamed mob ordered to sit (`Sitting` loaded or merged) sits too.
    if (!m.order_to_sit && !m.tame) || m.fluids.in_water() || !t.on_ground() {
        return false;
    }
    let ordered = m.order_to_sit;
    let Some(owner) = owner_of(t.ents, m) else {
        return true;
    };
    let near = t
        .ents
        .store
        .get(owner)
        .is_some_and(|e| e.pos().distance_to_sqr(t.pos()) < 144.0);
    let hurt = t
        .ents
        .player(owner)
        .and_then(|p| p.survival.last_hurt_by_mob)
        .is_some();
    if near && hurt { false } else { ordered }
}

/// `unableToMoveToOwner`.
fn unable_to_move_to_owner(t: &Tick<'_, '_>, owner: EntityId) -> bool {
    t.mob_ref().order_to_sit
        || t.ents.vehicle_of(t.id).is_some()
        || t.ents.leashes.contains_key(&t.id)
        || t.ents
            .player(owner)
            .is_some_and(|p| p.game_mode == GameMode::Spectator)
}

/// `FollowOwnerGoal.canUse`.
pub fn follow_can_use(t: &mut Tick<'_, '_>, start_distance: f32) -> bool {
    let Some(owner) = owner_of(t.ents, t.mob_ref()) else {
        return false;
    };
    if unable_to_move_to_owner(t, owner) {
        return false;
    }
    let d = f64::from(start_distance);
    t.ents
        .store
        .get(owner)
        .is_some_and(|e| e.pos().distance_to_sqr(t.pos()) >= d * d)
}

/// `FollowOwnerGoal.canContinueToUse`.
pub fn follow_can_continue(t: &mut Tick<'_, '_>, stop_distance: f32) -> bool {
    if t.nav_ref().is_done() {
        return false;
    }
    let Some(owner) = owner_of(t.ents, t.mob_ref()) else {
        return false;
    };
    if unable_to_move_to_owner(t, owner) {
        return false;
    }
    let d = f64::from(stop_distance);
    t.ents
        .store
        .get(owner)
        .is_some_and(|e| e.pos().distance_to_sqr(t.pos()) > d * d)
}

/// `FollowOwnerGoal.tick`: walk to the owner, or teleport next to it from 12 blocks on.
pub fn follow_tick(t: &mut Tick<'_, '_>, speed: f64, recalc: &mut i32) {
    let Some(owner) = owner_of(t.ents, t.mob_ref()) else {
        return;
    };
    let Some(o) = t.other(owner) else {
        return;
    };
    let teleport = t.pos().distance_to_sqr(o.pos) >= 144.0;
    if !teleport {
        t.mob()
            .look
            .set_look_at(Vec3::new(o.pos.x, o.eye_y, o.pos.z), 10.0, 40.0);
    }
    *recalc -= 1;
    if *recalc <= 0 {
        *recalc = super::goals::reduced(10);
        if teleport {
            try_to_teleport_to_owner(t, o.pos);
        } else {
            nav::move_to_entity(t, o.pos, 1, speed);
        }
    }
}

/// `tryToTeleportToOwner`: ten tries at a free spot within 3 blocks of the owner.
/// `tryToTeleportToOwner` to the owner's position (the pets' panic goal uses it too).
pub fn teleport_near_owner(t: &mut Tick<'_, '_>, owner: Vec3) {
    try_to_teleport_to_owner(t, owner);
}

fn try_to_teleport_to_owner(t: &mut Tick<'_, '_>, owner: Vec3) {
    let base = BlockPos::containing(owner.x, owner.y, owner.z);
    for _ in 0..10 {
        let m = t.mob();
        let dx = m.random.next_int_between_inclusive(-3, 3);
        let dy = m.random.next_int_between_inclusive(-1, 1);
        let dz = m.random.next_int_between_inclusive(-3, 3);
        if dx.abs() < 2 && dz.abs() < 2 {
            continue;
        }
        let pos = BlockPos::new(base.x + dx, base.y + dy, base.z + dz);
        if can_teleport_to(t, pos) {
            let p = Vec3::new(
                f64::from(pos.x) + 0.5,
                f64::from(pos.y),
                f64::from(pos.z) + 0.5,
            );
            t.core_mut().set_pos(p);
            t.nav().stop();
            return;
        }
    }
}

/// `canTeleportTo`: a walkable spot, not on leaves, with room for the mob.
fn can_teleport_to(t: &mut Tick<'_, '_>, pos: BlockPos) -> bool {
    let below = t.level.block(BlockPos::new(pos.x, pos.y - 1, pos.z));
    if below.collision_shape().is_empty() || below.block().info().name.ends_with("_leaves") {
        return false;
    }
    let here = t.level.block(pos);
    if !here.collision_shape().is_empty() {
        return false;
    }
    let dims = t.core().dims();
    let half = f64::from(dims.width) / 2.0;
    let (x, y, z) = (
        f64::from(pos.x) + 0.5,
        f64::from(pos.y),
        f64::from(pos.z) + 0.5,
    );
    let b = Aabb::new(
        x - half,
        y,
        z - half,
        x + half,
        y + f64::from(dims.height),
        z + half,
    );
    super::physics::no_collision(&t.level.view(), &b, &mut t.cx.boxes)
}

/// The owner's `lastHurtByMob` (`by_owner = true`) or `lastHurtMob` with its time stamp, while
/// it is at most 100 ticks old.
fn owner_hurt_info(t: &Tick<'_, '_>, by_owner: bool) -> Option<(EntityId, i32)> {
    let owner = owner_of(t.ents, t.mob_ref())?;
    let p = t.ents.player(owner)?;
    let entry = if by_owner {
        p.survival.last_hurt_by_mob
    } else {
        p.survival.last_hurt_mob
    }?;
    (p.survival.tick_count - entry.1 <= 100).then_some(entry)
}

/// `OwnerHurtByTargetGoal.canUse` (`by_owner`) and `OwnerHurtTargetGoal.canUse`.
pub fn owner_hurt_by_can_use(
    t: &mut Tick<'_, '_>,
    timestamp: i32,
    found: &mut Option<EntityId>,
    by_owner: bool,
) -> bool {
    let m = t.mob_ref();
    if !m.tame || m.order_to_sit {
        return false;
    }
    let Some((target, stamp)) = owner_hurt_info(t, by_owner) else {
        return false;
    };
    if stamp == timestamp || target == t.id {
        return false;
    }
    // `canAttack(target, TargetingConditions.DEFAULT)` (never the owner).
    let owner = owner_of(t.ents, t.mob_ref());
    if !crate::mob::goals::can_attack(t, target) || owner == Some(target) {
        return false;
    }
    // `Wolf.wantsToAttack`: no creepers, ghasts, armour stands, tamed horses or tamed pets
    // (other owners' wolves are fair game); other pets want to attack anything.
    if t.mob_ref().kind == Kind::Wolf
        && let Some(o) = t.ents.mob(target)
    {
        let wanted = match o.kind {
            Kind::Creeper | Kind::Ghast | Kind::ArmorStand => false,
            Kind::Wolf => !o.tame || o.owner != t.mob_ref().owner,
            k if crate::mob::fam::companion::is_horse(k)
                || matches!(k, Kind::Camel | Kind::CamelHusk) =>
            {
                !o.tame
            }
            k if crate::mob::fam::companion::is_tamable(k)
                || matches!(k, Kind::Nautilus | Kind::ZombieNautilus) =>
            {
                !o.tame
            }
            _ => true,
        };
        if !wanted {
            return false;
        }
    }
    // `Wolf.wantsToAttack`: a player's wolf attacks no player when `canHarmPlayer` fails (the
    // pvp rule).
    if t.ents.player(target).is_some()
        && owner.is_some_and(|o| t.ents.player(o).is_some())
        && !t.env.rules.bool(crate::survival::rules::Rule::PVP)
    {
        return false;
    }
    *found = Some(target);
    true
}

/// The time stamp an owner goal remembers when it starts.
pub fn owner_hurt_stamp(t: &Tick<'_, '_>, by_owner: bool) -> i32 {
    owner_hurt_info(t, by_owner).map_or(0, |(_, s)| s)
}

// Leads.

/// `canBeLeashed` with its overrides: tamables, squids, dolphins, axolotls and hoglins yes;
/// fish, bats, villagers, pandas, turtles and enemies no; angry wolves, aggravated nautiluses
/// and mob-ridden zombie mounts no; sulfur cubes only with a body item.
fn can_be_leashed(ents: &Entities, id: EntityId, m: &MobData) -> bool {
    use crate::mob::brain::memory::MemoryType;
    let mob_controlled = || {
        ents.riding
            .passengers_of(id)
            .first()
            .is_some_and(|&p| ents.mob(p).is_some())
    };
    let aggravated = || {
        m.brain
            .as_ref()
            .is_some_and(|b| b.has(MemoryType::AngryAt) || b.has(MemoryType::AttackTarget))
    };
    match m.kind {
        Kind::Wolf => !crate::mob::common::is_angry(m, ents.game_time),
        Kind::Squid
        | Kind::GlowSquid
        | Kind::Dolphin
        | Kind::Axolotl
        | Kind::Hoglin
        | Kind::Zoglin => true,
        Kind::Cod
        | Kind::Salmon
        | Kind::Pufferfish
        | Kind::TropicalFish
        | Kind::Tadpole
        | Kind::Bat
        | Kind::Villager
        | Kind::WanderingTrader
        | Kind::Panda
        | Kind::Turtle => false,
        Kind::CamelHusk => !mob_controlled(),
        Kind::ZombieHorse => m.tame || !mob_controlled(),
        Kind::Nautilus => !aggravated(),
        Kind::ZombieNautilus => !aggravated() && !mob_controlled(),
        Kind::SulfurCube => crate::mob::fam::undead::cubes::has_body_item(m),
        _ if crate::mob::fam::companion::is_tamable(m.kind) => true,
        k => !k.is_monster(),
    }
}

/// The holder of `mob`'s lead.
pub fn leash_holder(ents: &Entities, mob: EntityId) -> Option<EntityId> {
    ents.leashes.get(&mob).copied()
}

fn link_packet(ents: &Entities, env: &mut Env<'_>, mob: EntityId, holder: Option<EntityId>) {
    let Some(e) = ents.store.get(mob) else {
        return;
    };
    let dest = holder
        .and_then(|h| ents.store.get(h))
        .map_or(0, |h| h.network_id().0);
    env.out.push(Effect::Tracking {
        entity: mob,
        with_self: false,
        packet: SharedPacket::of(&SetEntityLink {
            source_id: e.network_id().0,
            dest_id: dest,
        }),
    });
}

/// The pairing packet of a leashed mob.
pub fn link_pairing_packet(ents: &Entities, mob: EntityId) -> Option<SetEntityLink> {
    let holder = leash_holder(ents, mob)?;
    Some(SetEntityLink {
        source_id: ents.store.get(mob)?.network_id().0,
        dest_id: ents.store.get(holder)?.network_id().0,
    })
}

/// `setLeashedTo(holder, true)`.
pub fn set_leashed_to(ents: &mut Entities, env: &mut Env<'_>, mob: EntityId, holder: EntityId) {
    ents.leashes.insert(mob, holder);
    // A new `LeashData` starts without angular momentum; a changed holder keeps it.
    ents.leash_momentum.entry(mob).or_insert(0.0);
    // Leading is not persistence (`requiresCustomPersistence` while leashed).
    if let Some(m) = ents.mob_mut(mob) {
        m.pending_leash = None;
    }
    if let Some(b) = ents.boat_mut(mob) {
        b.pending_leash = None;
    }
    link_packet(ents, env, mob, Some(holder));
    if ents.vehicle_of(mob).is_some() {
        crate::riding::stop_riding(ents, env, mob);
    }
}

/// `dropAllLeashConnections` of a player: the mobs on its leads go free and drop their leads;
/// `true` when there were any.
pub fn drop_leashes_of(ents: &mut Entities, env: &mut Env<'_>, holder: EntityId) -> bool {
    let mut mine: Vec<EntityId> = ents
        .leashes
        .iter()
        .filter(|(_, h)| **h == holder)
        .map(|(m, _)| *m)
        .collect();
    mine.sort_unstable();
    for &m in &mine {
        drop_leash(ents, env, m, true);
    }
    !mine.is_empty()
}

/// `Entity.shearOffAllLeashConnections(player)` (a player's or, with `None`, a dispenser's
/// shears): the entity's own lead and the leads tied to it drop, `SHEAR` at the entity, the
/// snip sound at its block in the player's (else the entity's) sound source; `true` when there
/// was a lead.
pub fn shear_off_all_leash_connections(
    ents: &mut Entities,
    env: &mut Env<'_>,
    id: EntityId,
    player: Option<EntityId>,
) -> bool {
    let mut tied: Vec<EntityId> = ents
        .leashes
        .iter()
        .filter(|(_, h)| **h == id)
        .map(|(m, _)| *m)
        .collect();
    tied.sort_unstable();
    let own = leash_holder(ents, id).is_some();
    if !own && tied.is_empty() {
        return false;
    }
    if own {
        drop_leash(ents, env, id, true);
    }
    for m in tied {
        drop_leash(ents, env, m, true);
    }
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return true;
    };
    let event = crate::level::game_event::events().shear;
    super::animal::entity_game_event(ents, env, id, event, player);
    let source = if player.is_some() {
        SoundSource::Players
    } else {
        ents.mob(id)
            .map_or(SoundSource::Neutral, |m| m.sound_source())
    };
    let block = BlockPos::containing(pos.x, pos.y, pos.z);
    let at = Vec3::new(
        f64::from(block.x) + 0.5,
        f64::from(block.y) + 0.5,
        f64::from(block.z) + 0.5,
    );
    let seed = crate::survival::sound_seed();
    play_sound(
        env.out,
        None,
        at,
        "minecraft:item.shears.snip",
        source,
        1.0,
        1.0,
        seed,
    );
    true
}

/// `dropLeash` (`drop_item`) or `removeLeash`.
pub fn drop_leash(ents: &mut Entities, env: &mut Env<'_>, mob: EntityId, drop_item: bool) {
    if ents.leashes.remove(&mob).is_none() {
        return;
    }
    ents.leash_momentum.remove(&mob);
    // `Mob.onLeashRemoved`: `clearHome`.
    if let Some(m) = ents.mob_mut(mob) {
        m.restrict_center = BlockPos::ZERO;
        m.restrict_radius = -1;
    }
    link_packet(ents, env, mob, None);
    if drop_item {
        spawn_lead(ents, env, mob);
    }
}

/// `spawnAtLocation(level, Items.LEAD)`.
fn spawn_lead(ents: &Entities, env: &mut Env<'_>, mob: EntityId) {
    if let Some(pos) = ents.store.get(mob).map(|e| e.pos())
        && let Some(lead) = vanillite_data::registry::ITEM.id("minecraft:lead")
    {
        let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
        env.out.push(Effect::DropItem {
            random: item_random,
            uuid,
            pos,
            velocity,
            y_rot,
            pickup_delay: 10,
            stack: ItemStack::new(lead, 1),
        });
    }
}

fn entity_sound(ents: &Entities, env: &mut Env<'_>, id: EntityId, sound: &str) {
    let Some(pos) = ents.store.get(id).map(|e| e.pos()) else {
        return;
    };
    let seed = crate::survival::sound_seed();
    play_sound(
        env.out,
        None,
        pos,
        sound,
        SoundSource::Neutral,
        1.0,
        1.0,
        seed,
    );
}

/// `Mob.canShearEquipment` and `attemptToShearEquipment`: shears take the first shearable item
/// off (a wolf's only for its owner, others only without passengers), drop it at the passenger
/// attachment, play its shearing sound and post `SHEAR`.
fn shear_equipment(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
    hand: Hand,
) -> bool {
    let Some(m) = ents.mob(target) else {
        return false;
    };
    let allowed = if m.kind == Kind::Wolf {
        ents.store.get(player).map(|e| e.uuid()) == m.owner
    } else {
        ents.riding.passengers_of(target).is_empty()
    };
    if !allowed {
        return false;
    }
    let creative = ents
        .player(player)
        .is_some_and(|p| p.game_mode == GameMode::Creative);
    let found = (0..8)
        .filter_map(crate::item::EquipmentSlot::from_ordinal)
        .find_map(|slot| {
            let s = m.equipment.get(slot);
            let sound = crate::item::equippable(s)
                .filter(|e| e.can_be_sheared)?
                .shearing_sound;
            // `PREVENT_ARMOR_CHANGE` (Curse of Binding) keeps it, unless creative.
            let bound = crate::player::play::game_data()
                .is_some_and(|d| d.enchantment_level(s, "minecraft:binding_curse") > 0);
            (!s.is_empty() && (!bound || creative)).then(|| (slot, s.clone(), sound))
        });
    let Some((slot, stack, sound)) = found else {
        return false;
    };
    let kind = m.kind;
    let sheared = crate::advancements::trigger::TriggerKind::PLAYER_SHEARED_EQUIPMENT;
    let award = crate::advancements::view::listens(ents, player, sheared).then(|| stack.clone());
    super::animal::hurt_held(ents, env, player, hand, 1);
    let height = ents.store.get(target).map_or(0.0, |e| e.dims().height);
    if let Some(m) = ents.mob_mut(target) {
        m.equipment.set(slot, ItemStack::default());
        if slot == crate::item::EquipmentSlot::Saddle {
            m.saddled = false;
        }
    }
    let event = crate::level::game_event::events().shear;
    super::animal::entity_game_event(ents, env, target, event, Some(player));
    if let Some(pos) = ents.store.get(target).map(|e| e.pos()) {
        // `getAttachments().getAverage(PASSENGER)`: the type's passenger points, unrotated.
        let offset = match kind {
            Kind::Wolf => Vec3::new(0.0, 0.818_75, -0.0625),
            Kind::Llama | Kind::TraderLlama => Vec3::new(0.0, 1.37, -0.3),
            Kind::Camel | Kind::CamelHusk | Kind::Strider => Vec3::new(0.0, f64::from(height), 0.0),
            _ => Vec3::new(0.0, crate::riding::mob_passenger_height(kind, height), 0.0),
        };
        let (uuid, y_rot, velocity, item_random) = crate::mob::new_item_entity();
        env.out.push(crate::survival::Effect::DropItem {
            random: item_random,
            uuid,
            pos: pos + offset,
            velocity,
            y_rot,
            pickup_delay: 10,
            stack,
        });
    }
    if let Some(stack) = award {
        crate::advancements::view::fire(ents, player, sheared, |ents| {
            crate::advancements::trigger::Event::EntityItem(
                sheared,
                crate::advancements::view::snapshot(ents, target),
                stack,
            )
        });
    }
    if let Some(update) = super::equipment::tracking_update(ents, target) {
        env.out.push(update);
    }
    if let Some(mut e) = ents.store.get_mut(target) {
        e.mark_data_dirty();
    }
    // `playSound(shearingSound)`: the mob's sound source, its random draws no pitch.
    if let Some(m) = ents.mob(target)
        && !m.silent
        && let Some(pos) = ents.store.get(target).map(|e| e.pos())
    {
        let seed = crate::survival::sound_seed();
        play_sound(env.out, None, pos, &sound, m.sound_source(), 1.0, 1.0, seed);
    }
    true
}

/// The lead part of `Entity.interact`: tie, untie or pass on leads.
pub fn leash_interact(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    target: EntityId,
    hand: Hand,
    sneaking: bool,
) -> Interaction {
    let leashable = ents
        .mob(target)
        .is_some_and(|m| m.is_alive() && can_be_leashed(ents, target, m))
        || ents.boat(target).is_some();
    let baby = ents.mob(target).is_some_and(|m| m.baby);
    if sneaking && leashable && !baby {
        // The player's leashed mobs around the target that can be tied to it
        // (`leashableInArea` + `canHaveALeashAttachedTo`).
        let mine = leashed_to_attachable_around(ents, target, player, target);
        let mut any = false;
        for m in mine {
            set_leashed_to(ents, env, m, target);
            any = true;
        }
        if any {
            // `gameEvent(ENTITY_ACTION, blockPosition(), Context.of(player))`.
            if let Some(p) = ents.store.get(target).map(|e| e.pos()) {
                let b = BlockPos::containing(p.x, p.y, p.z);
                crate::survival::game_event(
                    env.out,
                    crate::level::game_event::events().entity_action,
                    Vec3::new(
                        f64::from(b.x) + 0.5,
                        f64::from(b.y) + 0.5,
                        f64::from(b.z) + 0.5,
                    ),
                    Some(player),
                );
            }
            entity_sound(ents, env, target, "minecraft:item.lead.tied");
            return Interaction::Success;
        }
    }
    let stack = {
        let i = match hand {
            Hand::MainHand => ents.player(player).map_or(0, |p| p.inventory.selected),
            Hand::OffHand => crate::inventory::OFFHAND,
        };
        ents.player(player)
            .map(|p| p.inventory.get(i).clone())
            .unwrap_or_default()
    };
    let name = crate::item::item_id(&stack).name();
    if name == "minecraft:shears"
        && shear_off_all_leash_connections(ents, env, target, Some(player))
    {
        super::animal::hurt_held(ents, env, player, hand, 1);
        return Interaction::Success;
    }
    if name == "minecraft:shears" && !sneaking && shear_equipment(ents, env, player, target, hand) {
        return Interaction::Success;
    }
    if !leashable {
        return Interaction::Pass;
    }
    if leash_holder(ents, target) == Some(player) {
        let creative = ents
            .player(player)
            .is_some_and(|p| p.game_mode == GameMode::Creative);
        drop_leash(ents, env, target, !creative);
        let event = crate::level::game_event::events().entity_interact;
        super::animal::entity_game_event(ents, env, target, event, Some(player));
        entity_sound(ents, env, target, "minecraft:item.lead.untied");
        return Interaction::Success;
    }
    let held_by_player = leash_holder(ents, target).is_some_and(|h| ents.player(h).is_some());
    if name == "minecraft:lead" && !held_by_player {
        let close = match (ents.store.get(target), ents.store.get(player)) {
            (Some(a), Some(b)) => a.pos().distance_to_sqr(b.pos()) <= 144.0,
            _ => false,
        };
        if close {
            if leash_holder(ents, target).is_some() {
                drop_leash(ents, env, target, true);
            }
            set_leashed_to(ents, env, target, player);
            entity_sound(ents, env, target, "minecraft:item.lead.tied");
            let creative = ents
                .player(player)
                .is_some_and(|p| p.game_mode == GameMode::Creative);
            if !creative {
                let i = match hand {
                    Hand::MainHand => ents.player(player).map_or(0, |p| p.inventory.selected),
                    Hand::OffHand => crate::inventory::OFFHAND,
                };
                if let Some(p) = ents.player_mut(player)
                    && let Some(s) = p.inventory.get_mut(i)
                {
                    s.shrink(1);
                }
            }
            return Interaction::Success;
        }
    }
    Interaction::Pass
}

/// Whether a block holds leash knots (`#minecraft:leash_knots`: the fences).
pub fn is_knot_block(state: vanillite_data::block::BlockState) -> bool {
    state.block().info().name.ends_with("_fence")
}

/// The knot on a fence, created when missing (`LeashFenceKnotEntity.getOrCreateKnot`).
pub fn knot_at(
    ents: &mut Entities,
    network_ids: &NetworkIds,
    pos: BlockPos,
    uuid: u128,
) -> EntityId {
    let center = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.375,
        f64::from(pos.z) + 0.5,
    );
    let area = Aabb::of_block(pos).inflate(0.1, 0.1, 0.1);
    let mut found = None;
    ents.store.for_each_in(&area, false, |id| {
        if ents
            .store
            .get(id)
            .is_some_and(|e| e.kind() == vanillite_data::entity::ids::LEASH_KNOT)
        {
            found = Some(id);
        }
    });
    if let Some(id) = found {
        return id;
    }
    let new = NewEntity::new(
        vanillite_data::entity::ids::LEASH_KNOT,
        network_ids.next(),
        uuid,
        center,
    );
    let network_id = new.network_id.0;
    let knots = &mut ents.knots;
    let id = ents.store.spawn(new, |id| knots.push(id, pos));
    ents.by_network_id.insert(network_id, id);
    id
}

/// `LeadItem.useOn` on a fence and `FenceBlock.useWithoutItem` (`LeadItem.bindPlayerMobs`): the
/// player's leashed mobs around the fence get tied to its knot.
pub fn use_lead_on_fence(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    pos: BlockPos,
) -> bool {
    if !is_knot_block(level.block(pos)) {
        return false;
    }
    bind_player_mobs(ents, env, level.network_ids(), player, pos)
}

/// `LeadItem.bindPlayerMobs`: whether any mob got tied (`SUCCESS_SERVER`, else `PASS`).
pub fn bind_player_mobs(
    ents: &mut Entities,
    env: &mut Env<'_>,
    network_ids: &NetworkIds,
    player: EntityId,
    pos: BlockPos,
) -> bool {
    let center = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.5,
        f64::from(pos.z) + 0.5,
    );
    // `Leashable.leashableInArea`: a 32 block cube around the block centre.
    let area = Aabb::new(
        center.x - 16.0,
        center.y - 16.0,
        center.z - 16.0,
        center.x + 16.0,
        center.y + 16.0,
        center.z + 16.0,
    );
    let mut mine = Vec::new();
    ents.store.for_each_in(&area, false, |id| {
        if ents.leashes.get(&id) == Some(&player) {
            mine.push(id);
        }
    });
    if mine.is_empty() {
        return false;
    }
    // `canHaveALeashAttachedTo(knot)`: the knot's box centre within `leashSnapDistance`.
    let knot_center = Vec3::new(
        f64::from(pos.x) + 0.5,
        f64::from(pos.y) + 0.625,
        f64::from(pos.z) + 0.5,
    );
    mine.retain(|&m| {
        let Some(e) = ents.store.get(m) else {
            return false;
        };
        let snap = if e.kind() == vanillite_data::entity::ids::GHAST
            || e.kind() == vanillite_data::entity::ids::HAPPY_GHAST
        {
            16.0
        } else {
            12.0
        };
        let b = e.aabb();
        let c = Vec3::new(
            (b.min_x + b.max_x) / 2.0,
            (b.min_y + b.max_y) / 2.0,
            (b.min_z + b.max_z) / 2.0,
        );
        c.distance_to_sqr(knot_center).sqrt() <= snap
            && ents.mob(m).is_none_or(|mob| can_be_leashed(ents, m, mob))
    });
    // Without a mob to tie, Vanilla discards the knot it just created.
    if mine.is_empty() {
        return false;
    }
    let uuid = crate::mob::new_entity_uuid();
    let knots = ents.knots.len();
    let knot = knot_at(ents, network_ids, pos, uuid);
    if ents.knots.len() > knots && !ents.spawned(knot, vanillite_abi::spawn_reason::DEFAULT, 0) {
        return false;
    }
    for m in mine {
        set_leashed_to(ents, env, m, knot);
    }
    entity_sound(ents, env, knot, "minecraft:item.lead.tied");
    crate::survival::game_event(
        env.out,
        crate::level::game_event::events().block_attach,
        center,
        Some(player),
    );
    true
}

/// `Leashable.leashableLeashedTo(holder)` (a 32 block cube around the holder's box centre)
/// filtered by `canHaveALeashAttachedTo(other)`: within the snap distance of `other` and
/// leashable.
fn leashed_to_attachable(ents: &Entities, holder: EntityId, other: EntityId) -> Vec<EntityId> {
    leashed_to_attachable_around(ents, holder, holder, other)
}

/// `Leashable.leashableInArea(around, l -> l.getLeashHolder() == holder)` (a 32 block cube
/// around `around`'s box centre) filtered by `canHaveALeashAttachedTo(other)`.
fn leashed_to_attachable_around(
    ents: &Entities,
    around: EntityId,
    holder: EntityId,
    other: EntityId,
) -> Vec<EntityId> {
    let center = |id: EntityId| {
        ents.store.get(id).map(|e| {
            let b = e.aabb();
            Vec3::new(
                (b.min_x + b.max_x) / 2.0,
                (b.min_y + b.max_y) / 2.0,
                (b.min_z + b.max_z) / 2.0,
            )
        })
    };
    let (Some(hc), Some(oc)) = (center(around), center(other)) else {
        return Vec::new();
    };
    let area = Aabb::new(
        hc.x - 16.0,
        hc.y - 16.0,
        hc.z - 16.0,
        hc.x + 16.0,
        hc.y + 16.0,
        hc.z + 16.0,
    );
    let mut out = Vec::new();
    ents.store.for_each_in(&area, false, |id| {
        if ents.leashes.get(&id) == Some(&holder) {
            out.push(id);
        }
    });
    out.retain(|&m| {
        if m == other {
            return false;
        }
        let snap = match ents.store.get(m).map(|e| e.kind()) {
            Some(k)
                if k == vanillite_data::entity::ids::GHAST
                    || k == vanillite_data::entity::ids::HAPPY_GHAST =>
            {
                16.0
            }
            _ => 12.0,
        };
        center(m).is_some_and(|c| c.distance_to_sqr(oc).sqrt() <= snap)
            && ents.mob(m).is_none_or(|mob| can_be_leashed(ents, m, mob))
    });
    out
}

/// `LeashFenceKnotEntity.interact`: shears cut the knot's leads; the player's own leashed mobs
/// get tied to the knot, else (not sneaking) the knot's mobs go onto the player's lead
/// (`BLOCK_ATTACH`, the tie sound). `Pass` when nothing changed; an emptied knot is discarded
/// (`notifyLeasheeRemoved`).
pub fn knot_interact(
    ents: &mut Entities,
    env: &mut Env<'_>,
    discarded: &mut Vec<EntityId>,
    player: EntityId,
    knot: EntityId,
    hand: Hand,
    sneaking: bool,
) -> Interaction {
    let stack = super::animal::held(ents, player, hand);
    if vanillite_data::registry::ITEM.name(stack.item) == Some("minecraft:shears")
        && shear_off_all_leash_connections(ents, env, knot, Some(player))
    {
        super::animal::hurt_held(ents, env, player, hand, 1);
        discard_if_empty(ents, knot, discarded);
        return Interaction::Success;
    }
    let mine = leashed_to_attachable(ents, player, knot);
    let attached = !mine.is_empty();
    for m in mine {
        set_leashed_to(ents, env, m, knot);
    }
    let mut moved = false;
    if !attached && !sneaking {
        for m in leashed_to_attachable(ents, knot, player) {
            set_leashed_to(ents, env, m, player);
            moved = true;
        }
    }
    if !attached && !moved {
        return Interaction::Pass;
    }
    if let Some(pos) = ents.store.get(knot).map(|e| e.pos()) {
        let event = crate::level::game_event::events().block_attach;
        crate::survival::game_event(env.out, event, pos, Some(player));
    }
    entity_sound(ents, env, knot, "minecraft:item.lead.tied");
    discard_if_empty(ents, knot, discarded);
    Interaction::Success
}

/// `LeashFenceKnotEntity.notifyLeasheeRemoved`: a knot nothing is tied to goes.
fn discard_if_empty(ents: &Entities, knot: EntityId, discarded: &mut Vec<EntityId>) {
    if !ents.leashes.values().any(|&h| h == knot) {
        discarded.push(knot);
    }
}

/// A knot is hit or its fence is gone: it goes (`kill`, `dropItem`: the untie sound). Its
/// mobs drop their leads in their next `tickLeash` (only with `entityDrops`), see
/// [`tick_leashes`].
pub fn break_knot(
    ents: &mut Entities,
    env: &mut Env<'_>,
    knot: EntityId,
    discarded: &mut Vec<EntityId>,
) {
    entity_sound(ents, env, knot, "minecraft:item.lead.untied");
    discarded.push(knot);
}

/// `Leashable.tickLeash` of every leashed mob, and knots on fences that are gone. Runs once per
/// world tick after the entities.
pub fn tick_leashes(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    discarded: &mut Vec<EntityId>,
) {
    if !ents.knots.is_empty() {
        let gone: Vec<EntityId> = ents
            .knots
            .iter()
            .filter(|(_, pos)| level.is_loaded(**pos) && !is_knot_block(level.block(**pos)))
            .map(|(id, _)| id)
            .collect();
        for k in gone {
            break_knot(ents, env, k, discarded);
        }
    }
    // Holders that left the world.
    let orphans: Vec<EntityId> = ents
        .leashes
        .iter()
        .filter(|&(_, h)| !ents.store.contains(*h))
        .map(|(&m, _)| m)
        .collect();
    for mob in orphans {
        if ents.store.contains(mob) {
            let drops = env.rules.bool(Rule::ENTITY_DROPS);
            drop_leash(ents, env, mob, drops);
        } else {
            ents.leashes.remove(&mob);
        }
    }
    // `LeashFenceKnotEntity.notifyLeasheeRemoved`: knots whose last lead was dropped go.
    if !ents.knots.is_empty() {
        let empty: Vec<EntityId> = ents
            .knots
            .iter()
            .map(|(id, _)| id)
            .filter(|k| !ents.leashes.values().any(|h| h == k) && !discarded.contains(k))
            .collect();
        discarded.extend(empty);
    }
}

/// What `Leashable.tickLeash` did, for the mob-specific parts.
enum LeashStep {
    /// Not leashed (any more).
    Free,
    /// The lead snapped (`leashTooFarBehaviour`).
    Snapped,
    /// `checkElasticInteractions` pulled (`onElasticLeashPull`).
    Pulled,
    /// `closeRangeLeashBehaviour` towards the holder.
    Close(EntityId),
}

/// `getLeashHolder` of a restored `LeashData`: any entity of the world by UUID
/// (`level.getEntity(uuid)`) or the knot at a fence. `None` while not found.
fn restore_holder(level: &Level, ents: &mut Entities, leash: super::LeashRef) -> Option<EntityId> {
    match leash {
        super::LeashRef::Entity(uuid) => ents
            .players_by_uuid
            .get(&uuid)
            .copied()
            .or_else(|| ents.store.by_uuid(uuid)),
        super::LeashRef::Knot(pos) => {
            if level.is_loaded(pos) && is_knot_block(level.block(pos)) {
                let uuid = crate::mob::new_entity_uuid();
                Some(knot_at(ents, level.network_ids(), pos, uuid))
            } else {
                None
            }
        }
    }
}

/// `Leashable.restoreLeashFromSave`: the saved holder, or after 100 ticks without one the
/// lead drops. Returns the pending leash left.
fn restore_leash(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    id: EntityId,
    leash: super::LeashRef,
    tick_count: i32,
) -> Option<super::LeashRef> {
    if let Some(h) = restore_holder(level, ents, leash) {
        set_leashed_to(ents, env, id, h);
        return None;
    }
    if tick_count > 100 {
        spawn_lead(ents, env, id);
        return None;
    }
    Some(leash)
}

/// `leashSnapDistance` / `leashElasticDistance` of the leashed entity.
fn leash_distances(ents: &Entities, id: EntityId) -> (f64, f64) {
    if ents.mob(id).is_some_and(|m| m.kind == Kind::HappyGhast) {
        (16.0, 10.0)
    } else {
        (12.0, 6.0)
    }
}

/// `supportQuadLeash`: horses (not llamas), camels, sniffers and boats.
fn supports_quad_leash(ents: &Entities, id: EntityId) -> bool {
    if ents.boat(id).is_some() {
        return true;
    }
    ents.mob(id).is_some_and(|m| {
        matches!(
            m.kind,
            Kind::Horse
                | Kind::Donkey
                | Kind::Mule
                | Kind::ZombieHorse
                | Kind::SkeletonHorse
                | Kind::Camel
                | Kind::CamelHusk
                | Kind::Sniffer
        )
    })
}

/// `getHolderMovement`: no-AI mobs don't move their leads, the others their known movement.
fn holder_movement(ents: &Entities, id: EntityId) -> Vec3 {
    if ents.mob(id).is_some_and(|m| m.no_ai) {
        return Vec3::ZERO;
    }
    crate::spear::known_movement(ents, id)
}

/// `Entity.canInteractWithLevel`: alive, not removed, not a spectator.
fn can_interact_with_level(ents: &Entities, id: EntityId) -> bool {
    if !ents.store.contains(id) {
        return false;
    }
    if let Some(p) = ents.player(id) {
        return !p.survival.is_dead_or_dying() && p.game_mode != GameMode::Spectator;
    }
    ents.mob(id).is_none_or(MobData::is_alive)
}

/// `Vec3.multiply(dims).yRot(-yRot)` of an attachment point.
fn attachment(point: Vec3, width: f64, height: f64, y_rot: f32) -> Vec3 {
    let rad = -(y_rot * (std::f32::consts::PI / 180.0));
    let v = Vec3::new(point.x * width, point.y * height, point.z * width);
    let (c, s) = (
        f64::from(vanillite_base::math::cos(f64::from(rad))),
        f64::from(vanillite_base::math::sin(f64::from(rad))),
    );
    Vec3::new(v.x * c + v.z * s, v.y, v.z * c - v.x * s)
}

/// `Leashable.checkElasticInteractions`: the dampened spring of each attachment point.
/// Returns whether any spring pulled.
fn check_elastic(ents: &mut Entities, id: EntityId, holder: EntityId, slack: f64) -> bool {
    const QUAD: [Vec3; 4] = [
        Vec3::new(-0.5, 0.5, 0.5),
        Vec3::new(-0.5, 0.5, -0.5),
        Vec3::new(0.5, 0.5, -0.5),
        Vec3::new(0.5, 0.5, 0.5),
    ];
    let quad = ents.mob(holder).is_some_and(|m| m.kind == Kind::HappyGhast)
        && supports_quad_leash(ents, id);
    let (Some(me), Some(he)) = (ents.store.get(id), ents.store.get(holder)) else {
        return false;
    };
    let (mpos, mw, mh, my) = (
        me.pos(),
        f64::from(me.dims().width),
        f64::from(me.dims().height),
        me.rot().y_rot,
    );
    let (hpos, hw, hh, hy) = (
        he.pos(),
        f64::from(he.dims().width),
        f64::from(he.dims().height),
        he.rot().y_rot,
    );
    let (mine, theirs): (&[Vec3], &[Vec3]) = if quad {
        (&QUAD, &QUAD)
    } else {
        (&[Vec3::new(0.0, 0.5, 0.5)], &[Vec3::new(0.0, 0.5, 0.0)])
    };
    let motion = holder_movement(ents, id);
    let mut any = false;
    let (mut force, mut torque) = (Vec3::ZERO, 0.0);
    for (a, b) in mine.iter().zip(theirs) {
        let arm = attachment(*a, mw, mh, my);
        let at = mpos + arm;
        let pivot = hpos + attachment(*b, hw, hh, hy);
        // `computeDampenedSpringInteraction`.
        let distance = at.distance_to_sqr(pivot).sqrt();
        if distance < slack {
            continue;
        }
        let mut d = (pivot - at).normalize() * (distance - slack);
        let t = arm.z * d.x - arm.x * d.z;
        if motion.dot(d) >= 0.0 {
            d = d * f64::from(0.3_f32);
        }
        any = true;
        force = force + d;
        torque += t;
    }
    if !any {
        return false;
    }
    let scale = if quad { 0.25 } else { 1.0 };
    let (force, torque) = (force * scale, torque * scale);
    if let Some(m) = ents.leash_momentum.get_mut(&id) {
        *m += 10.0 * torque;
    }
    let rel = holder_movement(ents, holder) - crate::spear::known_movement(ents, id);
    let v = ents.store.get(id).map_or(Vec3::ZERO, |e| e.velocity());
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_velocity(v + (force.multiply(0.8, 0.2, 0.8) + rel * 0.11));
    }
    true
}

/// `Leashable.tickLeash` without the mob-specific behaviour (end of `Entity.baseTick`) of a
/// mob or boat. `in_liquid` is `isInLiquid` for the angular friction.
fn leash_step(
    level: &Level,
    ents: &mut Entities,
    env: &mut Env<'_>,
    id: EntityId,
    in_liquid: bool,
) -> LeashStep {
    let Some(holder) = leash_holder(ents, id) else {
        return LeashStep::Free;
    };
    if !can_interact_with_level(ents, id) || !can_interact_with_level(ents, holder) {
        let drops = env.rules.bool(Rule::ENTITY_DROPS);
        drop_leash(ents, env, id, drops);
        return LeashStep::Free;
    }
    let (Some(me), Some(he)) = (ents.store.get(id), ents.store.get(holder)) else {
        return LeashStep::Free;
    };
    let distance = me
        .aabb()
        .center()
        .distance_to_sqr(he.aabb().center())
        .sqrt();
    let (hpos, hw, mw) = (
        he.pos(),
        f64::from(he.dims().width),
        f64::from(me.dims().width),
    );
    let momentum = ents.leash_momentum.get(&id).copied().unwrap_or(0.0);
    let (snap, elastic) = leash_distances(ents, id);
    // `whenLeashedTo`: `PathfinderMob` homes at the holder; a happy ghast holding a
    // four-point leash shows it (`notifyLeashHolder`).
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    if let Some(m) = ents.mob_mut(id) {
        m.restrict_center = BlockPos::containing(hpos.x, hpos.y, hpos.z);
        m.restrict_radius = elastic as i32 - 1;
    }
    if supports_quad_leash(ents, id)
        && let Some(h) = ents.mob_mut(holder)
        && h.kind == Kind::HappyGhast
    {
        super::fam::mount::ghast::notify_leash_holder(h);
    }
    let step = if distance > snap {
        let seed = crate::survival::sound_seed();
        play_sound(
            env.out,
            None,
            hpos,
            "minecraft:item.lead.break",
            SoundSource::Neutral,
            1.0,
            1.0,
            seed,
        );
        drop_leash(ents, env, id, true);
        LeashStep::Snapped
    } else if distance > elastic - hw - mw && check_elastic(ents, id, holder, elastic) {
        LeashStep::Pulled
    } else {
        LeashStep::Close(holder)
    };
    // `setYRot(getYRot() - angularMomentum)`, then the friction of the momentum (of the
    // `LeashData` read at the start, so also after the lead snapped).
    let momentum = ents.leash_momentum.get(&id).copied().unwrap_or(momentum);
    let Some(me) = ents.store.get(id) else {
        return step;
    };
    let on_ground = me.flags().contains(EntityFlags::ON_GROUND);
    let p = me.pos();
    #[allow(clippy::cast_possible_truncation)]
    let y_rot = (f64::from(me.rot().y_rot) - momentum) as f32;
    let friction = if on_ground {
        let below = BlockPos::containing(p.x, p.y - f64::from(0.500_001_f32), p.z);
        level.block(below).block().info().friction * 0.91
    } else if in_liquid {
        0.8
    } else {
        0.91
    };
    let mut r = me.rot();
    r.y_rot = y_rot;
    if let Some(mut e) = ents.store.get_mut(id) {
        e.set_rot(r);
    }
    if let Some(m) = ents.leash_momentum.get_mut(&id) {
        *m = momentum * f64::from(friction);
    }
    step
}

/// `Leashable.tickLeash` of the ticking mob (end of `Entity.baseTick`) with `Mob` and
/// `PathfinderMob`'s parts.
pub fn tick_leash(t: &mut Tick<'_, '_>) {
    let id = t.id;
    if let Some(l) = t.mob_ref().pending_leash {
        let tick_count = t.mob_ref().tick_count;
        let left = restore_leash(t.level, t.ents, t.env, id, l, tick_count);
        t.mob().pending_leash = left;
    }
    let in_liquid = {
        let f = &t.mob_ref().fluids;
        f.in_water() || f.in_lava()
    };
    match leash_step(t.level, t.ents, t.env, id, in_liquid) {
        LeashStep::Free | LeashStep::Snapped => {}
        LeashStep::Pulled => on_elastic_pull(t),
        LeashStep::Close(holder) => {
            // `PathfinderMob.closeRangeLeashBehaviour`: walk to 2 blocks from the holder.
            let kind = t.mob_ref().kind;
            let stays = !matches!(kind, Kind::Allay | Kind::HappyGhast);
            if stays && !super::animal::is_panicking(t.mob_ref()) {
                let Some(hpos) = t.ents.store.get(holder).map(|e| e.pos()) else {
                    return;
                };
                let mpos = t.pos();
                #[allow(clippy::cast_possible_truncation)]
                let (xd, yd, zd) = (
                    (mpos.x - hpos.x) as f32,
                    (mpos.y - hpos.y) as f32,
                    (mpos.z - hpos.z) as f32,
                );
                let distance = (xd * xd + yd * yd + zd * zd).sqrt();
                let delta = (hpos - mpos).normalize() * f64::from((distance - 2.0).max(0.0));
                let speed = if matches!(kind, Kind::Llama | Kind::TraderLlama) {
                    2.0
                } else {
                    1.0
                };
                nav::move_to_pos(t, mpos + delta, speed);
            }
        }
    }
}

/// `onElasticLeashPull`: the fall distance is capped; horses stop eating, camels stand up,
/// happy ghasts wait.
fn on_elastic_pull(t: &mut Tick<'_, '_>) {
    let vy = t.velocity().y;
    {
        let m = t.mob();
        if vy > -0.5 && m.fall_distance > 1.0 {
            m.fall_distance = 1.0;
        }
    }
    match t.mob_ref().kind {
        Kind::Horse | Kind::Donkey | Kind::Mule | Kind::ZombieHorse | Kind::SkeletonHorse => {
            super::fam::companion::horse::stop_eating(t);
        }
        Kind::Camel | Kind::CamelHusk => {
            use super::fam::mount::camel;
            let now = t.game_time();
            if camel::is_sitting(t.mob_ref())
                && !camel::in_pose_transition(t.mob_ref(), now)
                && camel::can_change_pose(t)
            {
                camel::stand_up(t);
            }
        }
        Kind::HappyGhast => t.mob().move_ctl.operation = nav::Operation::Wait,
        _ => {}
    }
}

/// `Leashable.tickLeash` of a boat (in `Entity.baseTick` of `AbstractBoat.tick`).
pub fn tick_boat_leash(level: &Level, ents: &mut Entities, env: &mut Env<'_>, id: EntityId) {
    if let Some((l, tick_count)) = ents
        .boat(id)
        .and_then(|b| b.pending_leash.map(|l| (l, b.tick_count)))
    {
        let left = restore_leash(level, ents, env, id, l, tick_count);
        if let Some(b) = ents.boat_mut(id) {
            b.pending_leash = left;
        }
    }
    let in_liquid = ents.boat(id).is_some_and(|b| {
        !matches!(
            b.status,
            crate::vehicle::Status::OnLand | crate::vehicle::Status::InAir
        )
    });
    let pulled = matches!(
        leash_step(level, ents, env, id, in_liquid),
        LeashStep::Pulled
    );
    // `onElasticLeashPull`: `checkFallDistanceAccumulation`.
    let vy = ents.store.get(id).map_or(0.0, |e| e.velocity().y);
    if pulled
        && vy > -0.5
        && let Some(b) = ents.boat_mut(id)
        && b.fall_distance > 1.0
    {
        b.fall_distance = 1.0;
    }
}
