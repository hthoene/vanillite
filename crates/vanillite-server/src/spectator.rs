//! The spectator camera (`ServerPlayer.camera`, `setCamera` and its part of
//! `ServerPlayer.tick`) and the spectator packets (`spectator_action`, `teleport_to_entity`).

use vanillite_base::Vec3;
use vanillite_game::entities::Entities;
use vanillite_protocol::packets::play::clientbound::SetCamera;
use vanillite_world::entity::{EntityFlags, EntityId};

/// `isAlive` of a camera entity.
fn alive(entities: &Entities, id: EntityId) -> bool {
    if !entities.store.contains(id) {
        return false;
    }
    if let Some(m) = entities.mob(id) {
        return m.is_alive();
    }
    entities
        .player(id)
        .is_none_or(|p| !p.survival.is_dead_or_dying())
}

/// `isPickable`: what a spectator may spectate.
pub(crate) fn pickable(entities: &Entities, id: EntityId) -> bool {
    if let Some(p) = entities.player(id) {
        return p.game_mode != vanillite_config::GameMode::Spectator;
    }
    entities.mob(id).is_some() || entities.boat(id).is_some() || entities.minecart(id).is_some()
}

/// `ServerPlayer.setCamera(camera)`: `None` (or the player itself) watches through the
/// player. A new camera moves the player to it (`teleportTo` with its own rotation), the
/// client is told (`set_camera`) and the position resets.
pub(crate) fn set_camera(entities: &mut Entities, player: EntityId, camera: Option<EntityId>) {
    let camera = camera.filter(|&c| c != player);
    let Some(data) = entities.player(player) else {
        return;
    };
    if data.connection.camera == camera {
        return;
    }
    let target = camera.unwrap_or(player);
    let (Some(view), Some(me)) = (entities.store.get(target), entities.store.get(player)) else {
        return;
    };
    let (to, camera_id) = (view.pos(), view.network_id().0);
    let rot = me.rot();
    if let Some(p) = entities.player_mut(player) {
        p.connection.camera = camera;
    }
    teleport(entities, player, to, rot.y_rot, rot.x_rot);
    if let Some(p) = entities.player_mut(player) {
        p.connection.sink.send(&SetCamera { camera_id });
        p.connection.movement.reset_position(to);
    }
}

/// `ServerPlayer.teleportTo` in the same level without relative parts.
fn teleport(entities: &mut Entities, player: EntityId, to: Vec3, y_rot: f32, x_rot: f32) {
    let Some(me) = entities.store.get(player) else {
        return;
    };
    let rot = me.rot();
    let mut body = vanillite_game::player::Body {
        pos: me.pos(),
        velocity: me.velocity(),
        y_rot: rot.y_rot,
        x_rot: rot.x_rot,
        on_ground: me.flags().contains(EntityFlags::ON_GROUND),
        horizontal_collision: false,
        fall_flying: false,
    };
    if let Some(p) = entities.player_mut(player) {
        let sink = std::sync::Arc::clone(&p.connection.sink);
        p.connection
            .movement
            .teleport(&mut body, to, y_rot, x_rot, sink.as_ref());
    }
    if let Some(mut e) = entities.store.get_mut(player) {
        e.set_pos(to);
        let mut r = e.as_ref().rot();
        r.y_rot = y_rot;
        r.x_rot = x_rot;
        e.set_rot(r);
    }
}

/// The camera part of `ServerPlayer.tick`: a spectator follows its living camera and leaves
/// it when sneaking; a gone camera falls back to the player.
pub(crate) fn tick(entities: &mut Entities) {
    let watching: Vec<(EntityId, EntityId, bool)> = entities
        .players
        .iter()
        .filter_map(|(id, p)| {
            let spectator = p.game_mode == vanillite_config::GameMode::Spectator;
            p.connection.camera.map(|c| (id, c, spectator))
        })
        .collect();
    for (player, camera, spectator) in watching {
        // `setGameMode` out of spectator sets the camera back (`setCamera(this)`).
        if !spectator || !alive(entities, camera) {
            set_camera(entities, player, None);
            continue;
        }
        // `absSnapTo(camera position and rotation)`.
        let Some((pos, rot)) = entities.store.get(camera).map(|e| (e.pos(), e.rot())) else {
            continue;
        };
        let sneaking = if let Some(mut e) = entities.store.get_mut(player) {
            e.set_pos(pos);
            let mut own = e.as_ref().rot();
            own.y_rot = rot.y_rot;
            own.x_rot = rot.x_rot;
            e.set_rot(own);
            e.as_ref().flags().contains(EntityFlags::CROUCHING)
        } else {
            false
        };
        if let Some(p) = entities.player_mut(player) {
            p.connection.movement.reset_position(pos);
            p.connection.movement.chunk_move = true;
        }
        // `wantsToStopRiding`.
        if sneaking {
            set_camera(entities, player, None);
        }
    }
}

/// `handleSpectatorAction`: a spectator spectates the entity of `network_id` when it is
/// inside the world border, within interaction range and pickable.
pub(crate) fn spectate(
    entities: &mut Entities,
    border: &vanillite_game::level::border::WorldBorder,
    hooks: &dyn vanillite_game::hooks::GameHooks,
    player: EntityId,
    target: EntityId,
) {
    let Some(pos) = entities.store.get(target).map(|e| e.pos()) else {
        return;
    };
    let block = vanillite_base::BlockPos::containing(pos.x, pos.y, pos.z);
    if border.is_block_within_bounds(block) && pickable(entities, target) {
        // Bukkit's `PlayerTeleportEvent` (`SPECTATE`) of the move to the camera: cancelled,
        // the player keeps its view (the camera decides where it goes).
        let watching = entities
            .player(player)
            .is_some_and(|p| p.connection.camera == Some(target));
        if !watching && let Some(rot) = entities.store.get(player).map(|e| e.rot()) {
            let to = vanillite_game::hooks::Place::looking(pos, rot.y_rot, rot.x_rot);
            if vanillite_game::player::movement::teleport_event(
                Some(hooks),
                entities,
                player,
                vanillite_abi::teleport_cause::SPECTATE,
                to,
            )
            .is_none()
            {
                return;
            }
        }
        set_camera(entities, player, Some(target));
    }
}
