//! Riding (`Entity.startRiding`, `stopRiding`, `positionRider`, `getDismountLocationForPassenger`)
//! and the moves of player-steered vehicles (`handleMoveVehicle`, `handlePaddleBoat`).

use vanillite_base::math::floor;
use vanillite_base::random::RandomSource;
use vanillite_base::{Aabb, BlockPos, Vec3};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::packets::play::clientbound::{MoveVehicleBack, SetPassengers};
use vanillite_protocol::types::VarIntList;
use vanillite_world::entity::{EntityFlags, EntityId, Pose};

use crate::advancements::trigger::{Event, TriggerKind};
use crate::entities::Entities;
use crate::level::Level;
use crate::mob::{Kind, MobContext, physics};
use crate::survival::Effect;
use crate::survival::player::Env;

/// `getVehicleAttachmentPoint` height of a passenger (`Avatar.DEFAULT_VEHICLE_ATTACHMENT`,
/// `ridingOffset`).
fn vehicle_attachment(ents: &Entities, passenger: EntityId) -> f64 {
    // The attachments scale with the dimensions (`EntityDimensions.scale`).
    if let Some(p) = ents.player(passenger) {
        let scale = crate::effect::attribute_map::scale_of(&p.survival.effects.modifiers);
        #[allow(clippy::cast_possible_truncation)]
        return if scale == 1.0 {
            0.6
        } else {
            0.6 * f64::from(scale as f32)
        };
    }
    let Some(m) = ents.mob(passenger) else {
        return 0.0;
    };
    let attachment = unscaled_vehicle_attachment(m);
    let scale = crate::effect::attribute_map::scale_of(&m.effects.modifiers);
    if scale == 1.0 {
        attachment
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let s = scale as f32;
        attachment * f64::from(s)
    }
}

fn unscaled_vehicle_attachment(m: &crate::mob::MobData) -> f64 {
    // `EntityType.Builder.ridingOffset(offset)`: the vehicle attachment is `-offset`.
    let offset = match m.kind {
        // `Zombie.BABY_DIMENSIONS` attaches at 0.1875 (husks, drowned, zombie villagers and
        // zombified piglins share it).
        Kind::Zombie
        | Kind::Husk
        | Kind::Drowned
        | Kind::ZombieVillager
        | Kind::ZombifiedPiglin
            if m.baby =>
        {
            return f64::from(0.1875_f32);
        }
        Kind::Zombie
        | Kind::Husk
        | Kind::Drowned
        | Kind::ZombieVillager
        | Kind::ZombifiedPiglin
        | Kind::Skeleton
        | Kind::Stray
        | Kind::Bogged
        | Kind::Parched
        | Kind::Piglin
        | Kind::PiglinBrute => -0.7_f32,
        Kind::Evoker | Kind::Illusioner | Kind::Pillager | Kind::Vindicator => -0.6,
        Kind::WitherSkeleton => -0.875,
        Kind::Giant => -3.75,
        Kind::Phantom => -0.125,
        Kind::Allay | Kind::Vex => 0.04,
        Kind::Ghast | Kind::HappyGhast => 0.5,
        _ => 0.0,
    };
    -f64::from(offset)
}

/// `getPassengerAttachmentPoint` of a living vehicle (`passengerAttachments`), scaled for
/// babies.
pub(crate) fn mob_passenger_height(kind: Kind, height: f32) -> f64 {
    match kind {
        Kind::Pig => f64::from(0.868_75_f32),
        Kind::Zombie => f64::from(2.0125_f32),
        Kind::Spider => f64::from(0.765_f32),
        Kind::Cow => f64::from(1.368_75_f32),
        Kind::Sheep => f64::from(1.2375_f32),
        Kind::Chicken => 0.7,
        Kind::Enderman => f64::from(2.806_25_f32),
        Kind::Horse => f64::from(1.443_75_f32),
        Kind::Donkey => f64::from(1.1125_f32),
        Kind::Mule => f64::from(1.2125_f32),
        Kind::SkeletonHorse | Kind::ZombieHorse => f64::from(1.318_75_f32),
        Kind::Llama | Kind::TraderLlama => 1.37,
        Kind::Wolf => 0.818_75,
        Kind::Cat => f64::from(0.5125_f32),
        Kind::Ocelot => f64::from(0.6375_f32),
        Kind::Parrot => f64::from(0.4625_f32),
        Kind::Fox => 0.6375,
        // `getBodyAnchorAnimationYOffset` of a standing camel: its height less 0.375.
        Kind::Camel | Kind::CamelHusk => f64::from(height) - 0.375,
        Kind::HappyGhast => 4.0,
        Kind::Nautilus | Kind::ZombieNautilus => f64::from(1.1375_f32),
        _ => f64::from(height),
    }
}

/// Sends `set_passengers` of `vehicle` to the players that see it.
fn send_passengers(ents: &Entities, env: &mut Env<'_>, vehicle: EntityId) {
    let Some(v) = ents.store.get(vehicle) else {
        return;
    };
    let passengers = ents
        .riding
        .passengers_of(vehicle)
        .iter()
        .filter_map(|&p| ents.store.get(p).map(|e| e.network_id().0))
        .collect();
    env.out.push(Effect::Tracking {
        entity: vehicle,
        with_self: ents.player(vehicle).is_some(),
        packet: SharedPacket::of(&SetPassengers {
            vehicle: v.network_id().0,
            passengers: VarIntList(passengers),
        }),
    });
}

/// The pairing packet of a vehicle with passengers.
pub fn passengers_packet(ents: &Entities, vehicle: EntityId) -> Option<SetPassengers> {
    let list = ents.riding.passengers_of(vehicle);
    if list.is_empty() {
        return None;
    }
    Some(SetPassengers {
        vehicle: ents.store.get(vehicle)?.network_id().0,
        passengers: VarIntList(
            list.iter()
                .filter_map(|&p| ents.store.get(p).map(|e| e.network_id().0))
                .collect(),
        ),
    })
}

/// `canAddPassenger` of a vehicle.
fn can_add_passenger(ents: &Entities, vehicle: EntityId) -> bool {
    let count = ents.riding.passengers_of(vehicle).len();
    // `Marker.canAddPassenger`.
    if ents
        .store
        .get(vehicle)
        .is_some_and(|e| e.kind() == vanillite_data::entity::ids::MARKER)
    {
        return false;
    }
    if let Some(b) = ents.boat(vehicle) {
        return count < b.kind.max_passengers();
    }
    if let Some(m) = ents.mob(vehicle) {
        return count < crate::mob::fam::max_passengers(m);
    }
    count == 0
}

/// `startRiding(vehicle, force)`: `passenger` gets on `vehicle`. Returns whether it did.
pub fn start_riding(
    ents: &mut Entities,
    env: &mut Env<'_>,
    passenger: EntityId,
    vehicle: EntityId,
    force: bool,
) -> bool {
    if passenger == vehicle || ents.vehicle_of(passenger) == Some(vehicle) {
        return false;
    }
    // No loops: the vehicle must not already ride the passenger.
    let mut up = Some(vehicle);
    while let Some(v) = up {
        if v == passenger {
            return false;
        }
        up = ents.vehicle_of(v);
    }
    // `canRide`: the wither, the warden and the dragon never get on by themselves.
    let cannot_ride = ents
        .mob(passenger)
        .is_some_and(|m| matches!(m.kind, Kind::Wither | Kind::Warden | Kind::EnderDragon));
    if !force && (cannot_ride || !can_add_passenger(ents, vehicle)) {
        return false;
    }
    if !riding_event(
        ents,
        vanillite_abi::event::VEHICLE_ENTER,
        passenger,
        vehicle,
    ) {
        return false;
    }
    if ents.vehicle_of(passenger).is_some() && stop_riding(ents, env, passenger).is_none() {
        return false;
    }
    let is_player = ents.player(passenger).is_some();
    if let Some(mut e) = ents.store.get_mut(passenger) {
        e.set_pose(Pose::Standing);
        e.set_flags(EntityFlags::PASSENGER, true);
    }
    let ghast = ents
        .mob(vehicle)
        .is_some_and(|m| m.kind == Kind::HappyGhast);
    if ghast {
        let first = ents.riding.passengers_of(vehicle).is_empty();
        crate::mob::fam::mount::ghast::add_passenger(ents, env, vehicle, first);
    }
    ents.riding.add(passenger, vehicle, is_player);
    if ghast {
        crate::mob::fam::mount::ghast::passenger_added(ents, vehicle);
    }
    if let Some(at) = ents.store.get(vehicle).map(|e| e.pos()) {
        let event = crate::level::game_event::events().entity_mount;
        crate::survival::game_event(env.out, event, at, Some(passenger));
    }
    // `STARTED_RIDING` of every player riding the vehicle, directly or not
    // (`getIndirectPassengersStream`).
    let mut riders = vec![vehicle];
    let mut i = 0;
    while let Some(&v) = riders.get(i) {
        i += 1;
        riders.extend_from_slice(ents.riding.passengers_of(v));
    }
    for &rider in riders.iter().skip(1) {
        if let Some(p) = ents.player_mut(rider) {
            p.advancements
                .trigger_player(crate::advancements::trigger::TriggerKind::STARTED_RIDING);
        }
    }
    if let Some(p) = ents.player_mut(passenger) {
        p.connection.movement.passenger = true;
    }
    crate::effect::living::send_vehicle_effects(ents, passenger, vehicle, true);
    if let Some(m) = ents.mob_mut(passenger) {
        m.nav.stop();
    }
    // `Mob.startRiding`: a leashed mob drops its lead (where it stood).
    if ents.mob(passenger).is_some() {
        crate::mob::tame::drop_leash(ents, env, passenger, true);
    }
    position_riders_of(ents, vehicle);
    send_passengers(ents, env, vehicle);
    // `Mob.startRiding`: a leashed mob drops its lead.
    if ents.mob(passenger).is_some() && ents.leashes.contains_key(&passenger) {
        crate::mob::tame::drop_leash(ents, env, passenger, true);
    }
    true
}

/// The plugins' `VEHICLE_ENTER` or `VEHICLE_EXIT` event (`vanillite_abi::event`) of
/// `passenger` and `vehicle`: whether it may. Without listeners one branch.
fn riding_event(ents: &Entities, kind: u32, passenger: EntityId, vehicle: EntityId) -> bool {
    let Some(hooks) = ents.hooks_for(kind) else {
        return true;
    };
    let (Some(p), Some(v)) = (ents.store.get(passenger), ents.store.get(vehicle)) else {
        return true;
    };
    let mut ev = crate::hooks::Event {
        entity: p.uuid(),
        entity_type: p.kind().name().to_owned(),
        other: v.uuid(),
        other_type: v.kind().name().to_owned(),
        ..crate::hooks::Event::new(kind)
    };
    hooks.event(crate::hooks::HookWorld::entities(ents), &mut ev);
    !ev.cancelled
}

/// `stopRiding` from block code without the survival environment (a player lying down in a
/// bed): `passenger` gets off where it is and the players near the vehicle see it. The exit
/// event cannot be cancelled here.
pub fn stop_riding_in_level(level: &mut crate::level::Level, passenger: EntityId) {
    if let Some(vehicle) = level.entities.vehicle_of(passenger) {
        riding_event(
            &level.entities,
            vanillite_abi::event::VEHICLE_EXIT,
            passenger,
            vehicle,
        );
    }
    let Some(vehicle) = level.entities.riding.remove(passenger) else {
        return;
    };
    if let Some(mut e) = level.entities.store.get_mut(passenger) {
        e.set_flags(EntityFlags::PASSENGER, false);
    }
    if let Some(p) = level.entities.player_mut(passenger) {
        p.connection.movement.passenger = false;
    }
    crate::effect::living::send_vehicle_effects(&mut level.entities, passenger, vehicle, false);
    let Some(v) = level.entities.store.get(vehicle) else {
        return;
    };
    let at = v.pos();
    let packet = SetPassengers {
        vehicle: v.network_id().0,
        passengers: VarIntList(
            level
                .entities
                .riding
                .passengers_of(vehicle)
                .iter()
                .filter_map(|&p| level.entities.store.get(p).map(|e| e.network_id().0))
                .collect(),
        ),
    };
    let event = crate::level::game_event::events().entity_dismount;
    level.game_event(
        event,
        at,
        crate::level::game_event::GameEventContext::of(Some(passenger)),
    );
    level
        .output()
        .nearby_packets
        .push((at, 64.0, vanillite_protocol::SharedPacket::of(&packet)));
}

/// `stopRiding`: `passenger` gets off and is placed at the dismount location. Returns the
/// old vehicle; `None` also when a plugin cancelled it (`VEHICLE_EXIT`: it stays, its
/// client is corrected).
/// `LivingEntity.baseTick` with the eyes under water (no bubble column): a passenger of a
/// vehicle in `#dismounts_underwater` gets off.
pub fn dismount_underwater(ents: &mut Entities, env: &mut Env<'_>, passenger: EntityId) {
    if let Some(v) = ents.vehicle_of(passenger)
        && ents.store.get(v).is_some_and(|e| {
            crate::effect::entity_in_tag(e.kind(), "minecraft:dismounts_underwater")
        })
    {
        stop_riding(ents, env, passenger);
    }
}

pub fn stop_riding(
    ents: &mut Entities,
    env: &mut Env<'_>,
    passenger: EntityId,
) -> Option<EntityId> {
    let vehicle = ents.vehicle_of(passenger)?;
    if !riding_event(ents, vanillite_abi::event::VEHICLE_EXIT, passenger, vehicle) {
        send_passengers(ents, env, vehicle);
        return None;
    }
    dismount(ents, env, passenger)
}

/// [`stop_riding`] where Vanilla does not let the passenger stay (its vehicle breaks): the
/// exit event cannot be cancelled.
fn dismount(ents: &mut Entities, env: &mut Env<'_>, passenger: EntityId) -> Option<EntityId> {
    let vehicle = ents.riding.remove(passenger)?;
    if ents
        .mob(vehicle)
        .is_some_and(|m| m.kind == Kind::HappyGhast)
    {
        crate::mob::fam::mount::ghast::passenger_removed(ents, env, vehicle);
    }
    if let Some(at) = ents.store.get(vehicle).map(|e| e.pos()) {
        let event = crate::level::game_event::events().entity_dismount;
        crate::survival::game_event(env.out, event, at, Some(passenger));
    }
    if let Some(mut e) = ents.store.get_mut(passenger) {
        e.set_flags(EntityFlags::PASSENGER, false);
    }
    if let Some(p) = ents.player_mut(passenger) {
        p.connection.movement.passenger = false;
    }
    crate::effect::living::send_vehicle_effects(ents, passenger, vehicle, false);
    send_passengers(ents, env, vehicle);
    crate::decoration::cushion::passenger_left(ents, env, vehicle);
    let alive = ents.store.contains(passenger);
    if alive && let Some(target) = dismount_location(ents, env, vehicle, passenger) {
        dismount_to(ents, env.hooks, passenger, target);
    }
    Some(vehicle)
}

/// `ejectPassengers` (a vehicle breaks or a rail ejects): the exit events cannot be
/// cancelled.
pub fn eject_passengers(ents: &mut Entities, env: &mut Env<'_>, vehicle: EntityId) {
    let list: Vec<EntityId> = ents.riding.passengers_of(vehicle).to_vec();
    for p in list.into_iter().rev() {
        riding_event(ents, vanillite_abi::event::VEHICLE_EXIT, p, vehicle);
        dismount(ents, env, p);
    }
}

/// `dismountTo`: players are teleported (a plugin may cancel or move it: Bukkit's
/// `PlayerTeleportEvent` with `DISMOUNT`), others placed.
fn dismount_to(
    ents: &mut Entities,
    hooks: &dyn crate::hooks::GameHooks,
    passenger: EntityId,
    target: Vec3,
) {
    let Some(e) = ents.store.get(passenger) else {
        return;
    };
    let mut rot = e.rot();
    let mut target = target;
    if ents.player(passenger).is_some() && hooks.wants(vanillite_abi::event::PLAYER_TELEPORT) {
        let to = crate::hooks::Place::looking(target, rot.y_rot, rot.x_rot);
        let Some(place) = crate::player::movement::teleport_event(
            Some(hooks),
            ents,
            passenger,
            vanillite_abi::teleport_cause::DISMOUNT,
            to,
        ) else {
            return;
        };
        if place.world.is_none() {
            target = place.vec();
            (rot.y_rot, rot.x_rot) = (place.yaw, place.pitch);
        }
    }
    let Some(e) = ents.store.get(passenger) else {
        return;
    };
    let velocity = e.velocity();
    let on_ground = e.flags().contains(EntityFlags::ON_GROUND);
    let mut body = crate::player::Body {
        pos: e.pos(),
        velocity,
        y_rot: rot.y_rot,
        x_rot: rot.x_rot,
        on_ground,
        horizontal_collision: false,
        fall_flying: false,
    };
    if let Some(p) = ents.player_mut(passenger) {
        let sink = std::sync::Arc::clone(&p.connection.sink);
        p.connection
            .movement
            .teleport(&mut body, target, rot.y_rot, rot.x_rot, sink.as_ref());
    }
    if let Some(mut e) = ents.store.get_mut(passenger) {
        e.set_pos(target);
        let old = e.as_ref().rot();
        if old.y_rot != rot.y_rot || old.x_rot != rot.x_rot {
            e.set_rot(rot);
        }
    }
}

/// The blocks of the survival environment as collision blocks (unloaded = air).
struct EnvBlocks<'a>(
    &'a dyn crate::survival::blocks::BlockAccess,
    Option<&'a crate::level::border::WorldBorder>,
);

impl physics::Blocks for EnvBlocks<'_> {
    fn state(&self, pos: BlockPos) -> vanillite_data::block::BlockState {
        self.0
            .block_state(pos)
            .unwrap_or(vanillite_data::block::BlockState::AIR)
    }

    fn border(&self) -> Option<&crate::level::border::WorldBorder> {
        self.1
    }
}

/// `BlockGetter.getBlockFloorHeight(pos)`.
fn block_floor_height(level: &dyn physics::Blocks, pos: BlockPos) -> f64 {
    let top = |p: BlockPos| {
        level
            .state(p)
            .collision_shape()
            .iter()
            .map(|b| b[4])
            .reduce(f64::max)
    };
    match top(pos) {
        Some(h) => h,
        None => match top(BlockPos::new(pos.x, pos.y - 1, pos.z)) {
            Some(h) if h >= 1.0 => h - 1.0,
            _ => f64::NEG_INFINITY,
        },
    }
}

fn floor_valid(h: f64) -> bool {
    h.is_finite() && h < 1.0
}

/// `DismountHelper.canDismountTo`: the passenger's box at `target` is free of blocks.
fn can_dismount_to(
    level: &dyn physics::Blocks,
    width: f64,
    height: f64,
    target: Vec3,
    scratch: &mut Vec<Aabb>,
) -> bool {
    let half = width / 2.0;
    let b = Aabb::new(
        target.x - half,
        target.y,
        target.z - half,
        target.x + half,
        target.y + height,
        target.z + half,
    );
    physics::no_collision(level, &b, scratch)
}

/// `getDismountLocationForPassenger`.
fn dismount_location(
    ents: &Entities,
    env: &Env<'_>,
    vehicle: EntityId,
    passenger: EntityId,
) -> Option<Vec3> {
    let v = ents.store.get(vehicle)?;
    let p = ents.store.get(passenger)?;
    let level = &EnvBlocks(env.blocks, env.border);
    let vpos = v.pos();
    let fallback = Vec3::new(vpos.x, v.aabb().max_y, vpos.z);
    let (pw, ph) = (f64::from(p.dims().width), f64::from(p.dims().height));
    let mut scratch = Vec::new();
    // `HappyGhast.getDismountLocationForPassenger`: always the top centre.
    if ents
        .mob(vehicle)
        .is_some_and(|m| m.kind == Kind::HappyGhast)
    {
        return Some(fallback);
    }
    if let Some(m) = ents.minecart(vehicle) {
        let flipped = m.flipped;
        if let Some(t) = minecart_dismount(ents, level, vehicle, passenger, flipped) {
            return Some(t);
        }
        return Some(fallback);
    }
    if ents.boat(vehicle).is_some() {
        // `getCollisionHorizontalEscapeVector(width * sqrt2, passengerWidth, passengerYRot)`.
        let width = f64::from(v.dims().width) * std::f64::consts::SQRT_2;
        let d = (width + pw + 1.0e-5) / 2.0;
        let a = p.rot().y_rot * std::f32::consts::PI / 180.0;
        let (xd, zd) = (f64::from(-a.sin()), f64::from(a.cos()));
        let d2 = xd.abs().max(zd.abs());
        let (tx, tz) = (vpos.x + xd * d / d2, vpos.z + zd * d / d2);
        let target = BlockPos::containing(tx, v.aabb().max_y, tz);
        let below = BlockPos::new(target.x, target.y - 1, target.z);
        if !crate::mob::is_water(physics::Blocks::state(level, below)) {
            for pos in [target, below] {
                let h = block_floor_height(level, pos);
                if floor_valid(h) {
                    let t = Vec3::new(tx, f64::from(pos.y) + h, tz);
                    if can_dismount_to(level, pw, ph, t, &mut scratch) {
                        return Some(t);
                    }
                }
            }
        }
        return Some(fallback);
    }
    if ents.mob(vehicle).is_some_and(|m| {
        matches!(
            m.kind,
            Kind::Horse
                | Kind::Donkey
                | Kind::Mule
                | Kind::ZombieHorse
                | Kind::SkeletonHorse
                | Kind::Llama
                | Kind::TraderLlama
                | Kind::Camel
                | Kind::CamelHusk
        )
    }) {
        return Some(horse_dismount(ents, level, vehicle, passenger).unwrap_or(vpos));
    }
    // `LivingEntity.getDismountLocationForPassenger`: the blocks around the vehicle in
    // `DismountHelper.offsetsForDirection(getMotionDirection())` order.
    let yaw = f64::from(v.rot().y_rot);
    let dir = floor(yaw / 90.0 + 0.5) & 3;
    // `Direction.from2DDataValue`: south, west, north, east.
    let (dx, dz) = [(0, 1), (-1, 0), (0, -1), (1, 0)][usize::try_from(dir).unwrap_or(0)];
    // Clockwise of the direction.
    let (cx, cz) = (-dz, dx);
    let offsets = [
        (cx, cz),
        (-cx, -cz),
        (-dx + cx, -dz + cz),
        (-dx - cx, -dz - cz),
        (dx + cx, dz + cz),
        (dx - cx, dz - cz),
        (-dx, -dz),
        (dx, dz),
    ];
    let base = BlockPos::containing(vpos.x, vpos.y, vpos.z);
    for (ox, oz) in offsets {
        let pos = BlockPos::new(base.x + ox, base.y, base.z + oz);
        let h = block_floor_height(level, pos);
        if floor_valid(h) {
            let t = Vec3::new(
                f64::from(pos.x) + 0.5,
                f64::from(pos.y) + h,
                f64::from(pos.z) + 0.5,
            );
            if can_dismount_to(level, pw, ph, t, &mut scratch) {
                return Some(t);
            }
        }
    }
    Some(fallback)
}

/// `AbstractHorse.getDismountLocationForPassenger`: beside the rider's main hand, then its
/// other hand (`getDismountLocationInDirection`); `None` falls back to the horse's position.
fn horse_dismount(
    ents: &Entities,
    level: &dyn physics::Blocks,
    vehicle: EntityId,
    passenger: EntityId,
) -> Option<Vec3> {
    let v = ents.store.get(vehicle)?;
    let p = ents.store.get(passenger)?;
    let right = match ents.player(passenger) {
        Some(pd) => pd.client.main_hand == crate::player::MainHand::Right,
        None => !ents.mob(passenger).is_some_and(|m| m.left_handed),
    };
    let (vw, pw) = (f64::from(v.dims().width), f64::from(p.dims().width));
    let (vpos, aabb, y_rot) = (v.pos(), v.aabb(), v.rot().y_rot);
    // `getDismountPoses` with their box sizes.
    let poses: Vec<(f64, f64)> = if ents.player(passenger).is_some() {
        vec![(0.6, 1.8), (0.6, 1.5), (0.6, 0.6)]
    } else {
        vec![(pw, f64::from(p.dims().height))]
    };
    let mut scratch = Vec::new();
    for main in [true, false] {
        let side = if right == main { 90.0 } else { -90.0 };
        // `getCollisionHorizontalEscapeVector(width, passengerWidth, yRot +- 90)`.
        let distance = (vw + pw + f64::from(1.0e-5_f32)) / 2.0;
        let a = f64::from((y_rot + side) * (std::f32::consts::PI / 180.0));
        let (xd, zd) = (-vanillite_base::math::sin(a), vanillite_base::math::cos(a));
        let scale = xd.abs().max(zd.abs());
        let (tx, tz) = (
            vpos.x + f64::from(xd) * distance / f64::from(scale),
            vpos.z + f64::from(zd) * distance / f64::from(scale),
        );
        let limit = aabb.max_y + 0.75;
        for &(width, height) in &poses {
            let mut pos = BlockPos::containing(tx, aabb.min_y, tz);
            loop {
                let h = block_floor_height(level, pos);
                if f64::from(pos.y) + h > limit {
                    break;
                }
                if floor_valid(h) {
                    let t = Vec3::new(tx, f64::from(pos.y) + h, tz);
                    if can_dismount_to(level, width, height, t, &mut scratch) {
                        return Some(t);
                    }
                }
                pos = BlockPos::new(pos.x, pos.y + 1, pos.z);
                // `do { ... } while (!(y < limit))` as Vanilla has it.
                if f64::from(pos.y) < limit {
                    break;
                }
            }
        }
    }
    None
}

/// `AbstractMinecart.getDismountLocationForPassenger`: the blocks beside the minecart's
/// direction of travel, at its height, one up and one down, for each pose the passenger can
/// take.
fn minecart_dismount(
    ents: &Entities,
    level: &dyn physics::Blocks,
    vehicle: EntityId,
    passenger: EntityId,
    flipped: bool,
) -> Option<Vec3> {
    let v = ents.store.get(vehicle)?;
    let p = ents.store.get(passenger)?;
    // `getMotionDirection`: the clockwise of the facing (of its opposite when flipped).
    let yaw = f64::from(v.rot().y_rot);
    let mut dir = (floor(yaw / 90.0 + 0.5) & 3) as usize;
    if flipped {
        dir = (dir + 2) & 3;
    }
    // `Direction.from2DDataValue`: south, west, north, east; clockwise is the next one.
    const STEPS: [(i32, i32); 4] = [(0, 1), (-1, 0), (0, -1), (1, 0)];
    let (dx, dz) = STEPS[(dir + 1) & 3];
    // `DismountHelper.offsetsForDirection(forward)`.
    let (rx, rz) = (-dz, dx);
    let (lx, lz) = (-rx, -rz);
    let (bx, bz) = (-dx, -dz);
    let offsets = [
        (rx, rz),
        (lx, lz),
        (bx + rx, bz + rz),
        (bx + lx, bz + lz),
        (dx + rx, dz + rz),
        (dx + lx, dz + lz),
        (bx, bz),
        (dx, dz),
    ];
    let base = BlockPos::containing(v.pos().x, v.pos().y, v.pos().z);
    // `getDismountPoses` with their heights (`POSE_DISMOUNT_HEIGHTS`).
    const ALL: &[i32] = &[0, 1, -1];
    let poses: Vec<(f64, f64, &[i32])> = if ents.player(passenger).is_some() {
        vec![(0.6, 1.8, ALL), (0.6, 1.5, ALL), (0.6, 0.6, &[0, 1])]
    } else {
        let d = p.dims();
        vec![(f64::from(d.width), f64::from(d.height), ALL)]
    };
    let mut scratch = Vec::new();
    for (width, height, heights) in poses {
        let reach = width.min(1.0);
        for &oy in heights {
            for &(ox, oz) in &offsets {
                let pos = BlockPos::new(base.x + ox, base.y + oy, base.z + oz);
                let h = block_floor_height(level, pos);
                if !floor_valid(h) {
                    continue;
                }
                let t = Vec3::new(
                    f64::from(pos.x) + 0.5,
                    f64::from(pos.y) + h,
                    f64::from(pos.z) + 0.5,
                );
                if can_dismount_to(level, reach, height, t, &mut scratch) {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// `instanceof Animal`: the entity types whose class extends `Animal` (not villagers, golems,
/// fish, bats, allays or armor stands).
fn is_animal(kind: vanillite_data::entity::EntityTypeId) -> bool {
    matches!(
        kind.name().strip_prefix("minecraft:").unwrap_or(""),
        "armadillo"
            | "axolotl"
            | "bee"
            | "camel"
            | "camel_husk"
            | "cat"
            | "chicken"
            | "cow"
            | "donkey"
            | "fox"
            | "frog"
            | "goat"
            | "happy_ghast"
            | "hoglin"
            | "horse"
            | "llama"
            | "mooshroom"
            | "mule"
            | "nautilus"
            | "ocelot"
            | "panda"
            | "parrot"
            | "pig"
            | "polar_bear"
            | "rabbit"
            | "sheep"
            | "skeleton_horse"
            | "sniffer"
            | "strider"
            | "trader_llama"
            | "turtle"
            | "wolf"
            | "zombie_horse"
            | "zombie_nautilus"
    )
}

/// `positionRider` of every passenger of `vehicle`.
pub fn position_riders_of(ents: &mut Entities, vehicle: EntityId) {
    let list: Vec<EntityId> = ents.riding.passengers_of(vehicle).to_vec();
    let Some(v) = ents.store.get(vehicle) else {
        return;
    };
    let vpos = v.pos();
    let y_rot = v.rot().y_rot;
    let height = v.dims().height;
    let count = list.len();
    // `AbstractHorse.positionRider`: living passengers turn their body with the horse.
    let horse_body_rot = ents
        .mob(vehicle)
        .and_then(|m| m.kind.is_abstract_horse().then_some(m.y_body_rot));
    for (i, &p) in list.iter().enumerate() {
        let attach = if let Some(b) = ents.boat(vehicle) {
            let animal = ents.store.get(p).is_some_and(|e| is_animal(e.kind()));
            crate::vehicle::passenger_offset(b, y_rot, count, i, animal)
        } else if ents.minecart(vehicle).is_some() {
            Vec3::new(0.0, crate::minecart::passenger_height(ents, p), 0.0)
        } else if let Some(m) = ents.mob(vehicle) {
            let scale = if m.baby { 0.5 } else { 1.0 };
            // The `minecraft:scale` attribute: the dimensions carry it already, the fixed
            // attachments are scaled with it.
            let attr = crate::effect::attribute_map::scale_of(&m.effects.modifiers);
            let h = mob_passenger_height(m.kind, height);
            #[allow(clippy::cast_possible_truncation)]
            let base = if attr == 1.0 || h != mob_passenger_height(m.kind, height + 1.0) {
                Vec3::new(0.0, h * scale, 0.0)
            } else {
                Vec3::new(0.0, h * f64::from(scale as f32 * attr as f32), 0.0)
            };
            if crate::mob::fam::companion::is_horse(m.kind) {
                #[allow(clippy::cast_possible_truncation)]
                let s = scale as f32;
                base + crate::mob::fam::companion::horse::rearing_offset(m, y_rot, s)
            } else {
                base
            }
        } else {
            Vec3::new(0.0, f64::from(height), 0.0)
        };
        let target = vpos + attach - Vec3::new(0.0, vehicle_attachment(ents, p), 0.0);
        if let Some(mut e) = ents.store.get_mut(p) {
            e.set_pos(target);
        }
        if let Some(rot) = horse_body_rot
            && let Some(m) = ents.mob_mut(p)
        {
            m.y_body_rot = rot;
        }
    }
}

/// Places the passengers of every vehicle after the vehicles moved (`rideTick`): from the
/// lowest vehicle up (`tickPassenger` places a passenger before its own passengers), so a
/// stack of riders sits on where its vehicles are now.
pub fn position_all_riders(ents: &mut Entities) {
    if ents.riding.is_empty() {
        return;
    }
    let vehicles = ents.riding.vehicles();
    let mut stack: Vec<EntityId> = vehicles
        .iter()
        .rev()
        .copied()
        .filter(|&v| ents.riding.vehicle_of(v).is_none())
        .collect();
    while let Some(v) = stack.pop() {
        position_riders_of(ents, v);
        stack.extend(ents.riding.passengers_of(v).iter().rev().copied());
    }
}

/// `Player.rideTick`'s `wantsToStopRiding`: a player who holds sneak (its last `player_input`)
/// gets off its vehicle after the vehicles ticked, before the passengers are placed.
pub fn stop_riding_sneaking_players(ents: &mut Entities, env: &mut Env<'_>) {
    if ents.riding.is_empty() {
        return;
    }
    let sneaking: Vec<EntityId> = ents
        .players
        .iter()
        .filter(|(id, p)| p.connection.vehicle.input.shift && ents.riding.vehicle_of(*id).is_some())
        .map(|(id, _)| id)
        .collect();
    for player in sneaking {
        stop_riding(ents, env, player);
    }
}

/// `rideTick` of every passenger: [`position_all_riders`] with the riding statistics of the
/// players (`ServerPlayer.checkRidingStatistics`).
pub fn ride_tick(ents: &mut Entities) {
    if ents.riding.is_empty() {
        return;
    }
    let riders: Vec<(EntityId, EntityId, Vec3)> = ents
        .players
        .iter()
        .filter_map(|(id, _)| {
            let vehicle = ents.riding.vehicle_of(id)?;
            Some((id, vehicle, ents.store.get(id)?.pos()))
        })
        .collect();
    position_all_riders(ents);
    for (id, vehicle, before) in riders {
        let Some(after) = ents.store.get(id).map(|e| e.pos()) else {
            continue;
        };
        let (dx, dy, dz) = (after.x - before.x, after.y - before.y, after.z - before.z);
        // `didNotMove`.
        if dx == 0.0 && dy == 0.0 && dz == 0.0 {
            continue;
        }
        let stat = riding_stat(ents, vehicle);
        if let Some(stat) = stat
            && let Some(p) = ents.player_mut(id)
        {
            // `Math.round((float)Math.sqrt(...) * 100.0F)`.
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let cm = (((dx * dx + dy * dy + dz * dz).sqrt() as f32) * 100.0).round() as i32;
            p.stats.award(stat, cm);
        }
    }
}

/// The distance statistic of riding `vehicle`.
fn riding_stat(ents: &Entities, vehicle: EntityId) -> Option<crate::stats::Stat> {
    use crate::stats::custom;
    if ents.minecart(vehicle).is_some() {
        return Some(custom::MINECART_ONE_CM);
    }
    if ents.boat(vehicle).is_some() {
        return Some(custom::BOAT_ONE_CM);
    }
    Some(match ents.mob(vehicle)?.kind {
        Kind::Pig => custom::PIG_ONE_CM,
        Kind::Horse
        | Kind::Donkey
        | Kind::Mule
        | Kind::SkeletonHorse
        | Kind::ZombieHorse
        | Kind::Llama
        | Kind::TraderLlama
        | Kind::Camel
        | Kind::CamelHusk => custom::HORSE_ONE_CM,
        Kind::Strider => custom::STRIDER_ONE_CM,
        Kind::HappyGhast => custom::HAPPY_GHAST_ONE_CM,
        Kind::Nautilus | Kind::ZombieNautilus => custom::NAUTILUS_ONE_CM,
        _ => return None,
    })
}

/// Whether `player` controls `vehicle` (the first passenger of a boat or a saddled pig).
pub fn controls(ents: &Entities, player: EntityId, vehicle: EntityId) -> bool {
    if ents.riding.passengers_of(vehicle).first() != Some(&player) {
        return false;
    }
    if ents.boat(vehicle).is_some() {
        return true;
    }
    if crate::mob::fam::rider_controls(ents, player, vehicle) {
        return true;
    }
    ents.mob(vehicle).is_some_and(|m| {
        (m.saddled && m.kind == Kind::Pig && holds_carrot_on_a_stick(ents, player))
            || (m.kind == Kind::Strider
                && !m
                    .equipment
                    .get(crate::item::EquipmentSlot::Saddle)
                    .is_empty()
                && holds_item(
                    ents,
                    player,
                    vanillite_data::item::ids::WARPED_FUNGUS_ON_A_STICK.0,
                ))
            || (m.kind.family() != crate::mob::Family::Classic
                && crate::mob::fam::rider_controls_mob(m))
    })
}

/// Whether a player holds `item` in either hand.
fn holds_item(ents: &Entities, player: EntityId, item: u16) -> bool {
    let Some(p) = ents.player(player) else {
        return false;
    };
    [
        crate::item::EquipmentSlot::MainHand,
        crate::item::EquipmentSlot::OffHand,
    ]
    .iter()
    .any(|&s| p.inventory.equipment(s).item == item)
}

/// `Pig.getControllingPassenger`: the player holds a carrot on a stick in either hand.
fn holds_carrot_on_a_stick(ents: &Entities, player: EntityId) -> bool {
    let Some(p) = ents.player(player) else {
        return false;
    };
    let id = vanillite_data::item::ids::CARROT_ON_A_STICK.0;
    [
        crate::item::EquipmentSlot::MainHand,
        crate::item::EquipmentSlot::OffHand,
    ]
    .iter()
    .any(|&s| p.inventory.equipment(s).item == id)
}

/// `handleMoveVehicle`: the vehicle the player steers moves to where its client put it, unless
/// that is too far or into blocks; then the client gets the server's position back.
#[allow(clippy::too_many_arguments)]
pub fn move_vehicle(
    level: &Level,
    ents: &mut Entities,
    cx: &mut MobContext,
    player: EntityId,
    target: Vec3,
    y_rot: f32,
    x_rot: f32,
    on_ground: bool,
) {
    let Some(vehicle) = ents.vehicle_of(player) else {
        return;
    };
    if ents.vehicle_of(vehicle).is_some() || !controls(ents, player, vehicle) {
        return;
    }
    let Some(v) = ents.store.get(vehicle) else {
        return;
    };
    let old = v.pos();
    let old_box = v.aabb();
    let rot = v.rot();
    let velocity = v.velocity();
    let send_back = |ents: &mut Entities| {
        if let Some(p) = ents.player(player) {
            p.connection.sink.send(&MoveVehicleBack {
                position: vanillite_protocol::types::Vec3 {
                    x: old.x,
                    y: old.y,
                    z: old.z,
                },
                y_rot: rot.y_rot,
                x_rot: rot.x_rot,
            });
        }
    };
    let delta = target - old;
    if delta.length_sqr() - velocity.length_sqr() > 100.0 {
        send_back(ents);
        return;
    }
    let step = if ents.mob(vehicle).is_some() {
        1.0
    } else {
        0.0
    };
    let was_on_ground = v.flags().contains(EntityFlags::ON_GROUND);
    let on_lava = ents.mob(vehicle).is_some_and(|m| m.nav.stands_on_lava);
    let moved = physics::collide_for(
        &level.view(),
        old_box,
        delta,
        step,
        was_on_ground,
        &mut cx.boxes,
        on_lava,
    );
    let mut err = target - (old + moved);
    if err.y > -0.5 && err.y < 0.5 {
        err.y = 0.0;
    }
    let wrong = err.length_sqr() > 0.0625;
    if wrong && physics::no_collision(&level.view(), &old_box, &mut cx.boxes) {
        send_back(ents);
        return;
    }
    if let Some(mut e) = ents.store.get_mut(vehicle) {
        e.set_pos(target);
        e.set_rot(vanillite_world::entity::Rotation {
            y_rot,
            x_rot,
            y_head_rot: y_rot,
        });
        e.set_flags(EntityFlags::ON_GROUND, on_ground);
    }
    if let Some(m) = ents.mob_mut(vehicle) {
        m.y_body_rot = y_rot;
    }
    position_riders_of(ents, vehicle);
}

/// `handlePaddleBoat`.
pub fn paddle_boat(ents: &mut Entities, player: EntityId, left: bool, right: bool) {
    let Some(vehicle) = ents.vehicle_of(player) else {
        return;
    };
    let changed = ents.boat_mut(vehicle).is_some_and(|b| {
        let changed = b.paddles != (left, right);
        b.paddles = (left, right);
        changed
    });
    if changed && let Some(mut e) = ents.store.get_mut(vehicle) {
        e.mark_data_dirty();
    }
}

/// `FoodOnAStickItem.use`: the pig (carrot on a stick) or strider (warped fungus on a stick)
/// the player steers gets a boost; the stick loses 7 (carrot) or 1 (fungus) durability and
/// turns into a fishing rod when it breaks.
pub fn boost(
    ents: &mut Entities,
    env: &mut Env<'_>,
    player: EntityId,
    stack: &mut crate::item::ItemStack,
    creative: bool,
    hand: vanillite_protocol::packets::play::serverbound::Hand,
) -> bool {
    let Some(vehicle) = ents.vehicle_of(player) else {
        return false;
    };
    if !controls(ents, player, vehicle) {
        return false;
    }
    let Some(m) = ents.mob_mut(vehicle) else {
        return false;
    };
    // `vehicle.is(canInteractWith)` and `consumeItemDamage`.
    let fungus = stack.item == vanillite_data::item::ids::WARPED_FUNGUS_ON_A_STICK.0;
    let wanted = if fungus { Kind::Strider } else { Kind::Pig };
    if m.kind != wanted || m.boosting {
        return false;
    }
    let cost = if fungus { 1 } else { 7 };
    m.boosting = true;
    m.boost_time = 0;
    m.boost_total = m.random.next_int_bounded(841) + 140;
    if let Some(mut e) = ents.store.get_mut(vehicle) {
        e.mark_data_dirty();
    }
    if creative {
        return true;
    }
    let n = crate::durability::change(stack, cost, false, env.random);
    if n == 0 {
        return true;
    }
    let d = crate::item::damage(stack) + n;
    if n > 0
        && let Some(p) = ents.player_mut(player)
    {
        // `applyDamage`: `ITEM_DURABILITY_CHANGED`, and `ITEM_BROKEN` when it breaks.
        p.advancements
            .trigger_with(TriggerKind::ITEM_DURABILITY_CHANGED, || {
                Event::ItemDurabilityChanged(stack.clone(), d)
            });
        if d >= crate::item::max_damage(stack) {
            p.stats.award(
                crate::stats::Stat::of(crate::stats::StatType::Broken, stack.item),
                1,
            );
        }
    }
    if d >= crate::item::max_damage(stack) {
        // `onEquippedItemBroken`.
        let slot = match hand {
            vanillite_protocol::packets::play::serverbound::Hand::MainHand => {
                crate::item::EquipmentSlot::MainHand
            }
            vanillite_protocol::packets::play::serverbound::Hand::OffHand => {
                crate::item::EquipmentSlot::OffHand
            }
        };
        crate::durability::broken_in_env(ents, env, player, slot);
        // `hurtAndConvertOnBreak(cost, FISHING_ROD)`: the components stay, the damage resets.
        let mut rod = stack.clone();
        rod.item = vanillite_data::item::ids::FISHING_ROD.0;
        rod.count = 1;
        crate::item::set_damage(&mut rod, 0);
        *stack = rod;
    } else {
        crate::item::set_damage(stack, d);
    }
    true
}

/// `player_command` `START_RIDING_JUMP`: the jump of a mount the player rides
/// (`PlayerRideableJumping.handleStartJump`).
pub fn riding_jump(ents: &mut Entities, env: &mut Env<'_>, player: EntityId, strength: i32) {
    let Some(vehicle) = ents.vehicle_of(player) else {
        return;
    };
    if strength > 0 {
        crate::mob::fam::rider_jump(ents, env, vehicle, strength);
    }
}

/// `getMoveSimulationType() == SERVER_AND_CLIENT` (items, falling blocks, TNT, experience
/// orbs, projectiles) or `!type.hasUpdateInterval()` (area effect clouds, cushions, end
/// crystals, item frames, leash knots): kinds whose teleports go to every tracking player.
pub fn teleport_reaches_trackers(kind: vanillite_data::entity::EntityTypeId) -> bool {
    use vanillite_data::entity::ids as e;
    kind.info().update_interval == i32::MAX
        || [
            e::ITEM,
            e::FALLING_BLOCK,
            e::TNT,
            e::EXPERIENCE_ORB,
            e::ARROW,
            e::SPECTRAL_ARROW,
            e::TRIDENT,
            e::SNOWBALL,
            e::EGG,
            e::ENDER_PEARL,
            e::EXPERIENCE_BOTTLE,
            e::SPLASH_POTION,
            e::LINGERING_POTION,
            e::FIREBALL,
            e::SMALL_FIREBALL,
            e::DRAGON_FIREBALL,
            e::WITHER_SKULL,
            e::WIND_CHARGE,
            e::BREEZE_WIND_CHARGE,
            e::SHULKER_BULLET,
            e::LLAMA_SPIT,
            e::FIREWORK_ROCKET,
            e::FISHING_BOBBER,
        ]
        .contains(&kind)
}

/// `Entity.sendTeleportTransitionToPlayers` after a same-level teleport of a non-player entity
/// (already at its new place): `teleport_entity` to every player riding it (the controlling
/// one gets the transition as given, the others the new absolute place), and to the players
/// near its old position when [`teleport_reaches_trackers`]. `on_ground` is the state before
/// the teleport.
pub fn send_teleport_transition(
    level: &mut Level,
    id: EntityId,
    transition: vanillite_protocol::packets::play::clientbound::PositionMoveRotation,
    relatives: i32,
    old_pos: Vec3,
    on_ground: bool,
) {
    use vanillite_protocol::packets::play::clientbound::{PositionMoveRotation, TeleportEntity};
    let ents = &level.entities;
    let Some(e) = ents.store.get(id) else {
        return;
    };
    let kind = e.kind();
    let rot = e.rot();
    let entity_id = e.network_id().0;
    let absolute = TeleportEntity {
        entity_id,
        change: PositionMoveRotation {
            position: wire_vec(e.pos()),
            delta: wire_vec(e.velocity()),
            y_rot: rot.y_rot,
            x_rot: rot.x_rot,
        },
        relatives: 0,
        on_ground,
    };
    // `getIndirectPassengersStream`: depth first.
    let mut stack: Vec<EntityId> = ents
        .riding
        .passengers_of(id)
        .iter()
        .rev()
        .copied()
        .collect();
    while let Some(p) = stack.pop() {
        stack.extend(ents.riding.passengers_of(p).iter().rev().copied());
        let Some(player) = ents.player(p) else {
            continue;
        };
        if controls(ents, p, id) {
            player.connection.sink.send(&TeleportEntity {
                entity_id,
                change: transition,
                relatives,
                on_ground,
            });
        } else {
            player.connection.sink.send(&absolute);
        }
    }
    if teleport_reaches_trackers(kind) {
        // Riding players are near: they get it a second time (Vanilla skips them), which
        // leaves them at the same place.
        let range = f64::from(kind.info().tracking_range.saturating_mul(16));
        level
            .output()
            .nearby_packets
            .push((old_pos, range, SharedPacket::of(&absolute)));
    }
}

/// A position for the wire.
pub fn wire_vec(v: Vec3) -> vanillite_protocol::types::Vec3 {
    vanillite_protocol::types::Vec3 {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}
