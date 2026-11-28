//! Player movement from the client, validated like Vanilla's `ServerGamePacketListenerImpl`
//! (`handleMovePlayer`, `handleAcceptTeleportPacket`, `teleport`, `tickPlayer`).
//!
//! A move is collided with the blocks like `player.move` and goes back when it ends too far
//! from the server's result ("moved wrongly") or enters a block
//! (`isEntityCollidingWithAnythingNew`), unless `movement.wrong-movement-check` is off.
//! Not yet ported: the kick for floating too long (`movement.flying-kick`), and collisions
//! with entities (boats, shulkers) in these checks.

use vanillite_base::Vec3;
use vanillite_config::MovementRules;
use vanillite_protocol::packets::play::clientbound::{PlayerPosition, PositionMoveRotation};
use vanillite_protocol::packets::play::serverbound::AcceptTeleportation;
use vanillite_protocol::text::Component;
use vanillite_protocol::types;

use super::sink::PacketSink;

/// The player's `step_height` attribute default.
const PLAYER_STEP_HEIGHT: f32 = 0.6;
/// Ticks after which an unconfirmed teleport is sent again.
const TELEPORT_RESEND_TICKS: i32 = 20;
/// Ticks the server waits for `player_loaded` after joining before treating the client as loaded.
const CLIENT_LOADED_TIMEOUT: i32 = 60;

/// The movement-related state of the player's entity as the handlers see it. The caller copies
/// it from the entity store before handling packets and writes changed values back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Body {
    /// Position.
    pub pos: Vec3,
    /// Velocity (`getDeltaMovement`).
    pub velocity: Vec3,
    /// Yaw in degrees.
    pub y_rot: f32,
    /// Pitch in degrees, -90..=90.
    pub x_rot: f32,
    /// On ground as reported by the client.
    pub on_ground: bool,
    /// Horizontal collision as reported by the client.
    pub horizontal_collision: bool,
    /// Gliding with an elytra.
    pub fall_flying: bool,
}

/// The connection must be closed with this reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kick(pub Box<Component>);

impl Kick {
    /// Vanilla's `multiplayer.disconnect.invalid_player_movement`.
    pub fn invalid_movement() -> Kick {
        Kick(Box::new(Component::translatable(
            "multiplayer.disconnect.invalid_player_movement",
            vec![],
        )))
    }
}

/// One `move_player_*` packet: position and rotation are present depending on its kind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MoveRequest {
    /// New position (`move_player_pos`, `move_player_pos_rot`).
    pub pos: Option<Vec3>,
    /// New yaw and pitch (`move_player_rot`, `move_player_pos_rot`).
    pub rot: Option<(f32, f32)>,
    /// On ground.
    pub on_ground: bool,
    /// Horizontal collision.
    pub horizontal_collision: bool,
}

/// What the movement checks need from the world.
#[derive(Clone, Copy)]
pub struct MoveContext<'a> {
    /// The tick rate manager runs normally (not frozen).
    pub runs_normally: bool,
    /// The world's movement rules.
    pub rules: &'a MovementRules,
    /// No "moved wrongly" check for this player (`isCreative()`, `isSpectator()`).
    pub no_wrong_move_check: bool,
    /// The player has no physics (`noPhysics`, spectators): moves are not collided.
    pub no_physics: bool,
    /// The player's size in its pose (`getDimensions`).
    pub dims: vanillite_world::entity::Dimensions,
    /// The blocks the player collides with (`None`: no block collision).
    pub blocks: Option<&'a dyn crate::mob::physics::Blocks>,
    /// The `player_movement_check` game rule.
    pub movement_check: bool,
    /// The `elytra_movement_check` game rule.
    pub elytra_movement_check: bool,
    /// Collision answers of this batch's moves worked out ahead ([`Movement::collide_ahead`]).
    pub ahead: Option<&'a MovesAhead>,
}

impl std::fmt::Debug for MoveContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoveContext")
            .field("runs_normally", &self.runs_normally)
            .field("no_wrong_move_check", &self.no_wrong_move_check)
            .field("no_physics", &self.no_physics)
            .field("dims", &self.dims)
            .finish_non_exhaustive()
    }
}

/// Per-connection movement state.
#[derive(Clone, Debug)]
pub struct Movement {
    first_good: Vec3,
    last_good: Vec3,
    received_move_packets: i32,
    known_move_packets: i32,
    received_position_this_tick: bool,
    tick_count: i32,
    awaiting_teleport: i32,
    awaiting_position: Option<Vec3>,
    awaiting_teleport_time: i32,
    client_loaded_timeout: i32,
    waiting_for_respawn: bool,
    /// Moves accepted since the caller took them last (for fall damage and exhaustion).
    pub moves: Vec<crate::survival::player::MoveRecord>,
    /// The player rides a vehicle (`isPassenger`): moves only turn it, the vehicle places it.
    pub passenger: bool,
    /// `lastKnownClientMovement`: the last move of this client tick, zero after a client tick
    /// without one (`getKnownMovement`).
    pub known_movement: Vec3,
    received_movement_this_tick: bool,
    /// `ServerChunkCache.move(player)` is due: the server took a move of the client (or a
    /// vehicle move, or a spectator's camera moved it). The player's chunk tickets follow it
    /// then, and every entity is checked against it again (`TrackedEntity.updatePlayer`);
    /// the world clears it after its entity tracking.
    pub chunk_move: bool,
    /// `movementThisTick`: the moves `Entity.move` made since the player's last tick, for
    /// the blocks on their way (`applyEffectsFromBlocks`).
    pub path: Vec<crate::survival::inside::Movement>,
    /// The tick of the last "moved wrongly" warning and the warnings held back since: the
    /// log shows one per second and player, with a count of the rest.
    wrongly_logged: Option<i32>,
    wrongly_held: u32,
}

/// Ticks between two "moved wrongly" warnings of one player.
const WRONGLY_LOG_INTERVAL: i32 = 20;

/// `Mth.wrapDegrees`.
pub fn wrap_degrees(angle: f32) -> f32 {
    let mut a = angle % 360.0;
    if a >= 180.0 {
        a -= 360.0;
    }
    if a < -180.0 {
        a += 360.0;
    }
    a
}

/// `Entity.absSnapRotationTo` (with `setXRot`'s clamp).
pub(crate) fn snap_rotation(body: &mut Body, y_rot: f32, x_rot: f32) {
    if y_rot.is_finite() {
        body.y_rot = y_rot % 360.0;
    }
    if x_rot.is_finite() {
        body.x_rot = (x_rot.clamp(-90.0, 90.0) % 360.0).clamp(-90.0, 90.0);
    }
}

fn contains_invalid_values(pos: Vec3, y_rot: f32, x_rot: f32) -> bool {
    pos.x.is_nan() || pos.y.is_nan() || pos.z.is_nan() || !y_rot.is_finite() || !x_rot.is_finite()
}

fn clamp_horizontal(v: f64) -> f64 {
    v.clamp(-3.0e7, 3.0e7)
}

fn clamp_vertical(v: f64) -> f64 {
    v.clamp(-2.0e7, 2.0e7)
}

impl Movement {
    /// State for a player that just joined at `pos`.
    pub fn new(pos: Vec3) -> Movement {
        Movement {
            first_good: pos,
            last_good: pos,
            received_move_packets: 0,
            known_move_packets: 0,
            received_position_this_tick: false,
            tick_count: 0,
            awaiting_teleport: 0,
            awaiting_position: None,
            awaiting_teleport_time: 0,
            client_loaded_timeout: CLIENT_LOADED_TIMEOUT,
            waiting_for_respawn: false,
            moves: Vec::new(),
            passenger: false,
            known_movement: Vec3::ZERO,
            received_movement_this_tick: false,
            chunk_move: false,
            path: Vec::new(),
            wrongly_logged: None,
            wrongly_held: 0,
        }
    }

    /// Logs "moved wrongly" (Vanilla logs every one): at most once per second and player,
    /// the rest as a count, so one client cannot flood the log.
    fn warn_moved_wrongly(&mut self) {
        if self.wrongly_log_due() {
            log::warn!("Player moved wrongly!");
            self.wrongly_logged = Some(self.tick_count);
        } else {
            self.wrongly_held = self.wrongly_held.saturating_add(1);
        }
    }

    fn wrongly_log_due(&self) -> bool {
        self.wrongly_logged
            .is_none_or(|at| self.tick_count.wrapping_sub(at) >= WRONGLY_LOG_INTERVAL)
    }

    /// A teleport waits for the client's confirmation (moves are ignored meanwhile).
    pub fn awaiting_teleport(&self) -> bool {
        self.awaiting_position.is_some()
    }

    /// The teleport the server waits for the client to accept: its id and target
    /// (`awaitingTeleport`, `awaitingPositionFromClient`).
    pub fn pending_teleport(&self) -> Option<(i32, Vec3)> {
        self.awaiting_position.map(|p| (self.awaiting_teleport, p))
    }

    /// `markClientUnloadedAfterDeath`: the client loads the world again after respawning.
    pub fn mark_client_unloaded(&mut self) {
        self.waiting_for_respawn = true;
    }

    /// `restartClientLoadTimerAfterRespawn`.
    pub fn restart_client_load_timer(&mut self) {
        self.waiting_for_respawn = false;
        self.client_loaded_timeout = CLIENT_LOADED_TIMEOUT;
    }

    /// `hasClientLoaded`: `player_loaded` arrived or the timeout ran out.
    pub fn has_client_loaded(&self) -> bool {
        !self.waiting_for_respawn && self.client_loaded_timeout <= 0
    }

    /// `player_loaded`.
    pub fn mark_client_loaded(&mut self) {
        self.client_loaded_timeout = 0;
    }

    /// `tickClientLoadTimeout`, once per player tick.
    pub fn tick_client_load_timeout(&mut self) {
        if self.client_loaded_timeout > 0 {
            self.client_loaded_timeout = self.client_loaded_timeout.saturating_sub(1);
        }
    }

    /// `resetPosition`: the movement checks start from `pos` (after a server-side teleport).
    pub fn reset_position(&mut self, pos: Vec3) {
        self.first_good = pos;
        self.last_good = pos;
    }

    /// The movement part of `tickPlayer`, once per tick before the player's packets of the
    /// next tick are handled.
    pub fn tick(&mut self, body: &Body) {
        self.reset_position(body.pos);
        // The player's tick walked them (`applyEffectsFromBlocks`).
        self.path.clear();
        self.tick_count = self.tick_count.wrapping_add(1);
        self.known_move_packets = self.received_move_packets;
        if self.wrongly_held > 0 && self.wrongly_log_due() {
            let held = std::mem::take(&mut self.wrongly_held);
            log::warn!("Player moved wrongly {held} more times in the last second");
            self.wrongly_logged = Some(self.tick_count);
        }
        // TODO(milestone 3): the "floating too long" kick (`movement.flying-kick`) needs
        // `noBlocksAround`, i.e. block collision.
    }

    /// `client_tick_end`.
    pub fn client_tick_end(&mut self) {
        if !self.received_movement_this_tick {
            self.known_movement = Vec3::ZERO;
        }
        self.received_movement_this_tick = false;
        self.received_position_this_tick = false;
    }

    /// `ServerGamePacketListenerImpl.teleport` to an absolute position and rotation.
    pub fn teleport(
        &mut self,
        body: &mut Body,
        pos: Vec3,
        y_rot: f32,
        x_rot: f32,
        sink: &dyn PacketSink,
    ) {
        self.teleport_relative(body, pos, y_rot, x_rot, 0, sink);
    }

    /// `ServerGamePacketListenerImpl.teleport` with [`relative`] bits: each flagged
    /// coordinate or angle of `pos`, `y_rot` and `x_rot` is an offset from the current value.
    /// The client gets the values as they are; the server applies them
    /// (`PositionMoveRotation.calculateAbsolute`). The velocity is cleared except for the
    /// relative delta components.
    ///
    /// [`relative`]: vanillite_protocol::packets::play::clientbound::relative
    pub fn teleport_relative(
        &mut self,
        body: &mut Body,
        pos: Vec3,
        y_rot: f32,
        x_rot: f32,
        relatives: i32,
        sink: &dyn PacketSink,
    ) {
        use vanillite_protocol::packets::play::clientbound::relative;
        let rel = |flag: i32, current: f64, value: f64| {
            if relatives & flag != 0 {
                current + value
            } else {
                value
            }
        };
        let rel_f = |flag: i32, current: f32, value: f32| {
            if relatives & flag != 0 {
                current + value
            } else {
                value
            }
        };
        // `teleportSetPosition`: `clearMovementThisTick`.
        self.path.clear();
        self.awaiting_teleport_time = self.tick_count;
        self.awaiting_teleport = self.awaiting_teleport.wrapping_add(1);
        if self.awaiting_teleport == i32::MAX {
            self.awaiting_teleport = 0;
        }
        let current = body.pos;
        body.pos = Vec3::new(
            rel(relative::X, current.x, pos.x),
            rel(relative::Y, current.y, pos.y),
            rel(relative::Z, current.z, pos.z),
        );
        body.velocity = Vec3::new(
            rel(relative::DELTA_X, body.velocity.x, 0.0),
            rel(relative::DELTA_Y, body.velocity.y, 0.0),
            rel(relative::DELTA_Z, body.velocity.z, 0.0),
        );
        let (abs_y, abs_x) = (
            rel_f(relative::Y_ROT, body.y_rot, y_rot),
            rel_f(relative::X_ROT, body.x_rot, x_rot),
        );
        snap_rotation(body, abs_y, abs_x);
        self.awaiting_position = Some(body.pos);
        sink.send(&PlayerPosition {
            teleport_id: self.awaiting_teleport,
            change: PositionMoveRotation {
                position: types::Vec3 {
                    x: pos.x,
                    y: pos.y,
                    z: pos.z,
                },
                delta: types::Vec3::default(),
                y_rot,
                x_rot,
            },
            relatives,
        });
    }

    /// `updateAwaitingTeleport`: `true` while a teleport is unconfirmed (it is sent again
    /// after 20 ticks).
    fn update_awaiting_teleport(&mut self, body: &mut Body, sink: &dyn PacketSink) -> bool {
        match self.awaiting_position {
            Some(pos) => {
                if self.tick_count.wrapping_sub(self.awaiting_teleport_time) > TELEPORT_RESEND_TICKS
                {
                    self.awaiting_teleport_time = self.tick_count;
                    let (y_rot, x_rot) = (body.y_rot, body.x_rot);
                    self.teleport(body, pos, y_rot, x_rot, sink);
                }
                true
            }
            None => {
                self.awaiting_teleport_time = self.tick_count;
                false
            }
        }
    }

    /// `handleAcceptTeleportPacket`.
    pub fn accept_teleport(
        &mut self,
        packet: &AcceptTeleportation,
        body: &mut Body,
        ctx: MoveContext<'_>,
        sink: &dyn PacketSink,
    ) -> Result<(), Kick> {
        if packet.id != self.awaiting_teleport {
            return Ok(());
        }
        let reached = Vec3::new(packet.x, packet.y, packet.z);
        let Some(target) = self.awaiting_position else {
            return Err(Kick::invalid_movement());
        };
        if contains_invalid_values(reached, packet.y_rot, packet.x_rot) {
            return Err(Kick::invalid_movement());
        }
        body.pos = Vec3::new(
            clamp_horizontal(target.x),
            target.y,
            clamp_horizontal(target.z),
        );
        self.last_good = target;
        self.awaiting_position = None;
        self.position_change(
            reached,
            packet.y_rot,
            packet.x_rot,
            false,
            false,
            body,
            ctx,
            sink,
        );
        Ok(())
    }

    /// `handleMovePlayer`. Errors close the connection.
    pub fn handle_move(
        &mut self,
        request: MoveRequest,
        body: &mut Body,
        ctx: MoveContext<'_>,
        sink: &dyn PacketSink,
    ) -> Result<(), Kick> {
        let pos = request.pos.unwrap_or(Vec3::ZERO);
        let (req_y, req_x) = request.rot.unwrap_or((0.0, 0.0));
        if contains_invalid_values(pos, req_y, req_x) {
            return Err(Kick::invalid_movement());
        }
        if request.pos.is_some() {
            if self.received_position_this_tick {
                return Err(Kick::invalid_movement());
            }
            self.received_position_this_tick = true;
        }
        if self.tick_count == 0 {
            self.reset_position(body.pos);
        }
        if !self.has_client_loaded() {
            return Ok(());
        }
        let (y_rot, x_rot) = request.rot.unwrap_or((body.y_rot, body.x_rot));
        if self.update_awaiting_teleport(body, sink) {
            snap_rotation(body, wrap_degrees(y_rot), wrap_degrees(x_rot));
        } else if self.passenger {
            snap_rotation(body, wrap_degrees(y_rot), wrap_degrees(x_rot));
            self.chunk_move = true;
        } else {
            self.position_change(
                request.pos.unwrap_or(body.pos),
                y_rot,
                x_rot,
                request.on_ground,
                request.horizontal_collision,
                body,
                ctx,
                sink,
            );
        }
        Ok(())
    }

    /// `shouldCheckPlayerMovement`: the game rules, and the server's own switches on top.
    fn should_check_movement(ctx: MoveContext<'_>, fall_flying: bool) -> bool {
        ctx.movement_check
            && ctx.rules.too_fast_check
            && (!fall_flying || (ctx.elytra_movement_check && ctx.rules.elytra_too_fast_check))
    }

    /// `handlePlayerPositionChange` without vehicles and sleeping.
    #[allow(clippy::too_many_arguments)]
    fn position_change(
        &mut self,
        requested: Vec3,
        requested_y_rot: f32,
        requested_x_rot: f32,
        on_ground: bool,
        horizontal_collision: bool,
        body: &mut Body,
        ctx: MoveContext<'_>,
        sink: &dyn PacketSink,
    ) {
        let target = Vec3::new(
            clamp_horizontal(requested.x),
            clamp_vertical(requested.y),
            clamp_horizontal(requested.z),
        );
        let y_rot = wrap_degrees(requested_y_rot);
        let x_rot = wrap_degrees(requested_x_rot);
        let moved = target.distance_to_sqr(self.first_good);
        let expected = body.velocity.length_sqr();
        if ctx.runs_normally {
            self.received_move_packets = self.received_move_packets.wrapping_add(1);
            let mut delta_packets = self
                .received_move_packets
                .wrapping_sub(self.known_move_packets);
            if delta_packets > 5 {
                log::debug!(
                    "Player is sending move packets too frequently ({delta_packets} packets since last tick)"
                );
                delta_packets = 1;
            }
            if Self::should_check_movement(ctx, body.fall_flying) {
                let meters_per_tick: f32 = if body.fall_flying { 300.0 } else { 100.0 };
                let limit = f64::from(
                    meters_per_tick * f32::from(i16::try_from(delta_packets).unwrap_or(1)),
                );
                if moved - expected > limit {
                    let d = Vec3::new(
                        target.x - self.first_good.x,
                        target.y - self.first_good.y,
                        target.z - self.first_good.z,
                    );
                    log::warn!("Player moved too quickly! {},{},{}", d.x, d.y, d.z);
                    let (pos, y, x) = (body.pos, body.y_rot, body.x_rot);
                    self.teleport(body, pos, y, x, sink);
                    return;
                }
            }
        }
        let from = body.pos;
        let was_on_ground = body.on_ground;
        let delta = Vec3::new(
            target.x - self.last_good.x,
            target.y - self.last_good.y,
            target.z - self.last_good.z,
        );
        let moved_upwards = delta.y > 0.0;
        let (allowed, moved_to) = self.collision_allows(body, target, delta, ctx);
        if let Some(to) = moved_to {
            // `Entity.move(MoverType.PLAYER, delta)` records its way.
            crate::survival::inside::record(
                &mut self.path,
                crate::survival::inside::Movement {
                    from,
                    to,
                    original: Some(delta),
                },
            );
        }
        if !allowed {
            self.warn_moved_wrongly();
            let (y, x) = (y_rot, x_rot);
            self.teleport(body, from, y, x, sink);
            self.moves.push(crate::survival::player::MoveRecord {
                from,
                to: from,
                was_on_ground,
                on_ground,
                moved_upwards,
                accepted: false,
            });
            return;
        }
        body.pos = Vec3::new(
            clamp_horizontal(target.x),
            target.y,
            clamp_horizontal(target.z),
        );
        snap_rotation(body, y_rot, x_rot);
        body.on_ground = on_ground;
        body.horizontal_collision = horizontal_collision;
        self.last_good = body.pos;
        // `handlePlayerKnownMovement`.
        self.known_movement = Vec3::new(
            body.pos.x - from.x,
            body.pos.y - from.y,
            body.pos.z - from.z,
        );
        self.received_movement_this_tick = true;
        self.chunk_move = true;
        self.moves.push(crate::survival::player::MoveRecord {
            from,
            to: body.pos,
            was_on_ground,
            on_ground,
            moved_upwards,
            accepted: true,
        });
    }

    /// The collision part of `handlePlayerPositionChange`: `player.move` by `delta` from the
    /// current position, then the move to `target` passes unless it ends more than 0.25
    /// blocks horizontally from the collided position ("moved wrongly", not for creative
    /// players) from a free box, or enters a block. A rejected move leaves the player where
    /// the collision put it, with its `onGround`. With the answer comes where `Entity.move`
    /// took the player, if it moved it (the move as asked for where nothing checks it).
    fn collision_allows(
        &self,
        body: &mut Body,
        target: Vec3,
        delta: Vec3,
        ctx: MoveContext<'_>,
    ) -> (bool, Option<Vec3>) {
        if ctx.no_physics {
            return (true, None);
        }
        let Some(blocks) = ctx.blocks else {
            return (true, Some(target));
        };
        if !ctx.rules.wrong_movement_check {
            return (true, Some(target));
        }
        let input = CollideInput {
            pos: body.pos,
            on_ground: body.on_ground,
            target,
            delta,
            dims: ctx.dims,
            no_wrong_move_check: ctx.no_wrong_move_check,
        };
        let answer = ctx
            .ahead
            .and_then(|ahead| ahead.answer(&input, blocks))
            .unwrap_or_else(|| input.collide(blocks));
        if let Some(on_ground) = answer.on_ground {
            body.on_ground = on_ground;
        }
        (answer.allowed, answer.moved_to)
    }

    /// Works out the collisions of the moves in `packets` ahead, read-only and on any
    /// thread, as [`Movement::handle_move`] would ask for them if every move passes. The
    /// answers go to `out`; [`MoveContext::ahead`] hands them to the handling, which takes an
    /// answer only for exactly its inputs while the blocks keep the same
    /// [`physics::Stamp`] and collides anew otherwise, so the outcome is the same either way.
    ///
    /// [`physics::Stamp`]: crate::mob::physics::Stamp
    pub fn collide_ahead<'p>(
        &self,
        packets: impl IntoIterator<Item = &'p vanillite_protocol::sets::ServerboundPlay>,
        body: &Body,
        ctx: MoveContext<'_>,
        out: &mut MovesAhead,
    ) {
        use vanillite_protocol::sets::ServerboundPlay;
        out.stamp = None;
        out.len = 0;
        let Some(blocks) = ctx.blocks else {
            return;
        };
        if ctx.no_physics
            || !ctx.rules.wrong_movement_check
            || !self.has_client_loaded()
            || self.awaiting_teleport()
            || self.passenger
        {
            return;
        }
        let Some(stamp) = blocks.stamp() else {
            return;
        };
        out.stamp = Some(stamp);
        let (mut pos, mut on_ground) = (body.pos, body.on_ground);
        let mut last_good = if self.tick_count == 0 {
            body.pos
        } else {
            self.last_good
        };
        for packet in packets {
            let (requested, flags) = match packet {
                ServerboundPlay::MovePlayerPos(p) => (Some(Vec3::new(p.x, p.y, p.z)), p.flags),
                ServerboundPlay::MovePlayerPosRot(p) => (Some(Vec3::new(p.x, p.y, p.z)), p.flags),
                ServerboundPlay::MovePlayerRot(p) => (None, p.flags),
                ServerboundPlay::MovePlayerStatusOnly(p) => (None, p.flags),
                ServerboundPlay::AcceptTeleportation(_) => break,
                _ => continue,
            };
            let Some(slot) = out.moves.get_mut(usize::from(out.len)) else {
                break;
            };
            let requested = requested.unwrap_or(pos);
            let target = Vec3::new(
                clamp_horizontal(requested.x),
                clamp_vertical(requested.y),
                clamp_horizontal(requested.z),
            );
            let input = CollideInput {
                pos,
                on_ground,
                target,
                delta: Vec3::new(
                    target.x - last_good.x,
                    target.y - last_good.y,
                    target.z - last_good.z,
                ),
                dims: ctx.dims,
                no_wrong_move_check: ctx.no_wrong_move_check,
            };
            let answer = input.collide(blocks);
            *slot = (input, answer);
            out.len = out.len.saturating_add(1);
            if !answer.allowed {
                // The player goes back; the moves after wait for the teleport.
                break;
            }
            pos = Vec3::new(
                clamp_horizontal(target.x),
                target.y,
                clamp_horizontal(target.z),
            );
            on_ground = flags.on_ground;
            last_good = pos;
        }
    }
}

/// The collision answers of one player's moves of a tick, worked out ahead
/// ([`Movement::collide_ahead`]). Fixed size: further moves collide when they are handled.
#[derive(Clone, Copy, Debug, Default)]
pub struct MovesAhead {
    stamp: Option<crate::mob::physics::Stamp>,
    len: u8,
    moves: [(CollideInput, Collided); 2],
}

impl MovesAhead {
    /// The answer for exactly `input`, if the blocks still have the stamp it was worked out
    /// with.
    fn answer(
        &self,
        input: &CollideInput,
        blocks: &dyn crate::mob::physics::Blocks,
    ) -> Option<Collided> {
        let moves = self.moves.get(..usize::from(self.len))?;
        let (_, answer) = moves.iter().find(|(i, _)| i.same(input))?;
        (self.stamp.is_some() && self.stamp == blocks.stamp()).then_some(*answer)
    }
}

/// Everything the collision of a move reads besides the blocks.
#[derive(Clone, Copy, Debug)]
struct CollideInput {
    pos: Vec3,
    on_ground: bool,
    target: Vec3,
    delta: Vec3,
    dims: vanillite_world::entity::Dimensions,
    no_wrong_move_check: bool,
}

impl Default for CollideInput {
    fn default() -> Self {
        CollideInput {
            pos: Vec3::ZERO,
            on_ground: false,
            target: Vec3::ZERO,
            delta: Vec3::ZERO,
            dims: vanillite_world::entity::Dimensions {
                width: 0.0,
                height: 0.0,
            },
            no_wrong_move_check: false,
        }
    }
}

/// The answer of [`CollideInput::collide`].
#[derive(Clone, Copy, Debug, Default)]
struct Collided {
    allowed: bool,
    /// Where `Entity.move` took the player, if it moved it.
    moved_to: Option<Vec3>,
    /// The player's `onGround` after a rejected vertical move.
    on_ground: Option<bool>,
}

impl CollideInput {
    /// Bit for bit the same inputs.
    fn same(&self, other: &CollideInput) -> bool {
        let bits = |v: Vec3| [v.x.to_bits(), v.y.to_bits(), v.z.to_bits()];
        bits(self.pos) == bits(other.pos)
            && bits(self.target) == bits(other.target)
            && bits(self.delta) == bits(other.delta)
            && self.on_ground == other.on_ground
            && self.dims.width.to_bits() == other.dims.width.to_bits()
            && self.dims.height.to_bits() == other.dims.height.to_bits()
            && self.no_wrong_move_check == other.no_wrong_move_check
    }

    /// `player.move` by `delta` from `pos`, then the "moved wrongly" and
    /// `isEntityCollidingWithAnythingNew` checks of the move to `target`.
    fn collide(&self, blocks: &dyn crate::mob::physics::Blocks) -> Collided {
        use crate::mob::physics;
        let (pos, delta, target) = (self.pos, self.delta, self.target);
        let old_box = self.dims.bounding_box(pos);
        let mut scratch = Vec::new();
        // `Entity.move(MoverType.PLAYER, delta)`.
        let movement = physics::collide(
            blocks,
            old_box,
            delta,
            PLAYER_STEP_HEIGHT,
            self.on_ground,
            &mut scratch,
        );
        let length = movement.length_sqr();
        let moved = length > 1.0e-7 || delta.length_sqr() - length < 1.0e-7;
        let moved_to = if moved {
            Vec3::new(pos.x + movement.x, pos.y + movement.y, pos.z + movement.z)
        } else {
            pos
        };
        // The vertical difference never counts (`yDist > -0.5 || yDist < 0.5`).
        let (dx, dz) = (target.x - moved_to.x, target.z - moved_to.z);
        let fail = dx * dx + dz * dz > 0.0625 && !self.no_wrong_move_check;
        let new_box = self.dims.bounding_box(target);
        let allowed = (!fail || !physics::no_collision(blocks, &old_box, &mut scratch))
            && !physics::collides_with_anything_new(blocks, &old_box, &new_box);
        Collided {
            allowed,
            moved_to: moved.then_some(moved_to),
            // `setOnGroundWithMovement(verticalCollisionBelow, ...)` of the move.
            on_ground: (!allowed && delta.y.abs() > 0.0)
                .then_some(delta.y != movement.y && delta.y < 0.0),
        }
    }
}

/// A destination a plugin set: position and rotation when they are finite, the position
/// clamped like a client's move (`clampHorizontal`, `clampVertical`).
pub fn checked_place(place: &crate::hooks::Place) -> Option<(Vec3, f32, f32)> {
    let [x, y, z] = place.pos;
    if !(x.is_finite()
        && y.is_finite()
        && z.is_finite()
        && place.yaw.is_finite()
        && place.pitch.is_finite())
    {
        return None;
    }
    Some((
        Vec3::new(clamp_horizontal(x), clamp_vertical(y), clamp_horizontal(z)),
        wrap_degrees(place.yaw),
        place.pitch.clamp(-90.0, 90.0),
    ))
}

/// Bukkit's `PlayerTeleportEvent` (`vanillite_abi::event::PLAYER_TELEPORT`) of the player
/// `id` going to `to`, `cause` from `vanillite_abi::teleport_cause`: `None` when a plugin
/// cancelled it, else the destination: a handler's checked and clamped ([`checked_place`];
/// `world` set for another world, which only callers that can move players between worlds
/// honour). Without hooks or a listener `to` comes back at once.
pub fn teleport_event(
    hooks: Option<&dyn crate::hooks::GameHooks>,
    ents: &crate::entities::Entities,
    id: vanillite_world::entity::EntityId,
    cause: u32,
    to: crate::hooks::Place,
) -> Option<crate::hooks::Place> {
    let kind = vanillite_abi::event::PLAYER_TELEPORT;
    let Some(hooks) = hooks.filter(|h| h.wants(kind)) else {
        return Some(to);
    };
    let Some(entity) = ents.store.get(id) else {
        return Some(to);
    };
    let rot = entity.rot();
    let mut ev = crate::hooks::Event {
        entity: entity.uuid(),
        entity_type: "minecraft:player".to_owned(),
        cause,
        from: crate::hooks::Place::looking(entity.pos(), rot.y_rot, rot.x_rot),
        to,
        ..crate::hooks::Event::new(kind)
    };
    hooks.event(crate::hooks::HookWorld::entities(ents), &mut ev);
    if ev.cancelled {
        return None;
    }
    if ev.to == to {
        return Some(to);
    }
    Some(match checked_place(&ev.to) {
        Some((pos, yaw, pitch)) => crate::hooks::Place {
            world: ev.to.world,
            pos: [pos.x, pos.y, pos.z],
            yaw,
            pitch,
        },
        None => to,
    })
}

/// `ServerPlayer.teleport` within its world (`teleportSameDimension`, `connection.teleport`):
/// the player goes to `pos` with the rotation and the [`relative`] bits of the transition, and
/// its client follows. Returns the new position.
///
/// [`relative`]: vanillite_protocol::packets::play::clientbound::relative
pub fn teleport_within_world(
    ents: &mut crate::entities::Entities,
    id: vanillite_world::entity::EntityId,
    pos: Vec3,
    y_rot: f32,
    x_rot: f32,
    relatives: i32,
) -> Option<Vec3> {
    use vanillite_world::entity::EntityFlags;
    let entity = ents.store.get(id)?;
    let rot = entity.rot();
    let flags = entity.flags();
    let mut body = Body {
        pos: entity.pos(),
        velocity: entity.velocity(),
        y_rot: rot.y_rot,
        x_rot: rot.x_rot,
        on_ground: flags.contains(EntityFlags::ON_GROUND),
        horizontal_collision: false,
        fall_flying: flags.contains(EntityFlags::FALL_FLYING),
    };
    let player = ents.player_mut(id)?;
    let sink = std::sync::Arc::clone(&player.connection.sink);
    player.connection.movement.teleport_relative(
        &mut body,
        pos,
        y_rot,
        x_rot,
        relatives,
        sink.as_ref(),
    );
    player.connection.movement.reset_position(body.pos);
    player.survival.teleported();
    if let Some(mut entity) = ents.store.get_mut(id) {
        entity.set_pos(body.pos);
        entity.set_velocity(body.velocity);
        entity.set_old_pos();
    }
    Some(body.pos)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod tests {
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    use vanillite_base::BlockPos;
    use vanillite_data::block::BlockState;
    use vanillite_protocol::SharedPacket;
    use vanillite_protocol::packets::play::serverbound::{
        MoveFlags, MovePlayerPos, MovePlayerStatusOnly,
    };
    use vanillite_protocol::sets::ServerboundPlay;

    use super::*;
    use crate::mob::physics::{Blocks, Stamp};

    /// A stone floor below y 64 and, when `wall` is set, a wall at x 2; reads are counted.
    #[derive(Default)]
    struct World {
        wall: Cell<bool>,
        epoch: Cell<u64>,
        reads: Cell<usize>,
    }

    impl Blocks for World {
        fn state(&self, pos: BlockPos) -> BlockState {
            self.reads.set(self.reads.get() + 1);
            if pos.y < 64 || (self.wall.get() && pos.x == 2) {
                vanillite_data::block::ids::STONE.default_state()
            } else {
                BlockState::AIR
            }
        }

        fn stamp(&self) -> Option<Stamp> {
            Some(Stamp::new(self.epoch.get(), None))
        }
    }

    #[derive(Default)]
    struct Sink(Mutex<Vec<Vec<u8>>>);

    impl PacketSink for Sink {
        fn send_body(&self, body: Vec<u8>) -> bool {
            self.0.lock().unwrap().push(body);
            true
        }
        fn send_shared(&self, packet: &Arc<SharedPacket>) -> bool {
            self.0.lock().unwrap().push(packet.body().to_vec());
            true
        }
        fn close(&self) {}
        fn is_closed(&self) -> bool {
            false
        }
        fn latency_ms(&self) -> u32 {
            0
        }
    }

    fn rules() -> MovementRules {
        <vanillite_config::WorldRules as vanillite_config::RuleSet>::for_profile(
            vanillite_config::Profile::Vanilla,
        )
        .movement
    }

    fn ctx<'a>(rules: &'a MovementRules, blocks: &'a World) -> MoveContext<'a> {
        MoveContext {
            runs_normally: true,
            rules,
            no_wrong_move_check: false,
            no_physics: false,
            dims: vanillite_world::entity::Dimensions {
                width: 0.6,
                height: 1.8,
            },
            blocks: Some(blocks),
            movement_check: true,
            elytra_movement_check: true,
            ahead: None,
        }
    }

    fn walk(x: f64) -> ServerboundPlay {
        ServerboundPlay::MovePlayerPos(MovePlayerPos {
            x,
            y: 64.0,
            z: 0.5,
            flags: MoveFlags {
                on_ground: true,
                horizontal_collision: false,
            },
        })
    }

    /// Two moves in one tick, a still one, then one into the wall (when it stands).
    fn packets() -> Vec<ServerboundPlay> {
        vec![
            walk(0.8),
            ServerboundPlay::MovePlayerStatusOnly(MovePlayerStatusOnly {
                flags: MoveFlags {
                    on_ground: true,
                    horizontal_collision: false,
                },
            }),
            walk(1.2),
            walk(1.9),
        ]
    }

    fn start() -> (Movement, Body) {
        let pos = Vec3::new(0.5, 64.0, 0.5);
        let mut movement = Movement::new(pos);
        movement.mark_client_loaded();
        movement.tick_count = 1;
        let body = Body {
            pos,
            velocity: Vec3::ZERO,
            y_rot: 0.0,
            x_rot: 0.0,
            on_ground: true,
            horizontal_collision: false,
            fall_flying: false,
        };
        (movement, body)
    }

    /// Handles `packets`' moves (with `ahead`, if given): every body after a move, the moves
    /// taken and the packets sent.
    fn handle(
        world: &World,
        packets: &[ServerboundPlay],
        ahead: Option<&MovesAhead>,
    ) -> (Vec<Body>, usize, Vec<Vec<u8>>) {
        let rules = rules();
        let (mut movement, mut body) = start();
        let sink = Sink::default();
        let mut bodies = Vec::new();
        for packet in packets {
            let ctx = MoveContext {
                ahead,
                ..ctx(&rules, world)
            };
            let request = match packet {
                ServerboundPlay::MovePlayerPos(p) => MoveRequest {
                    pos: Some(Vec3::new(p.x, p.y, p.z)),
                    rot: None,
                    on_ground: p.flags.on_ground,
                    horizontal_collision: p.flags.horizontal_collision,
                },
                ServerboundPlay::MovePlayerStatusOnly(p) => MoveRequest {
                    pos: None,
                    rot: None,
                    on_ground: p.flags.on_ground,
                    horizontal_collision: p.flags.horizontal_collision,
                },
                _ => unreachable!(),
            };
            // One position per client tick.
            movement.client_tick_end();
            movement
                .handle_move(request, &mut body, ctx, &sink)
                .unwrap();
            bodies.push(body);
        }
        let sent = sink.0.into_inner().unwrap();
        (bodies, movement.moves.len(), sent)
    }

    fn ahead(world: &World, packets: &[ServerboundPlay]) -> MovesAhead {
        let rules = rules();
        let (movement, body) = start();
        let mut out = MovesAhead::default();
        movement.collide_ahead(packets, &body, ctx(&rules, world), &mut out);
        out
    }

    /// The moves handled with their collisions worked out ahead give the same bodies, moves
    /// and packets as handled one by one, and read no block while the blocks are unchanged.
    #[test]
    fn moves_worked_out_ahead_handle_like_serial_ones() {
        for wall in [false, true] {
            let world = World::default();
            world.wall.set(wall);
            let serial = handle(&world, &packets(), None);
            let serial_reads = world.reads.replace(0);
            let ahead = ahead(&world, &packets());
            assert_eq!(ahead.len, 2);
            world.reads.set(0);
            let with_ahead = handle(&world, &packets(), Some(&ahead));
            assert_eq!(serial, with_ahead, "wall {wall}");
            // The first two moves took their answers: fewer blocks were read.
            assert!(world.reads.get() < serial_reads, "wall {wall}");
        }
        // The wall turns the last move back: a teleport goes out.
        let world = World::default();
        world.wall.set(true);
        assert!(!handle(&world, &packets(), None).2.is_empty());
    }

    /// A block change between working out ahead and handling: the moves collide anew with
    /// the changed blocks, as when handled one by one.
    #[test]
    fn a_block_change_after_working_out_ahead_collides_anew() {
        let world = World::default();
        let packets = [walk(1.2), walk(1.9)];
        let ahead = ahead(&world, &packets);
        world.wall.set(true);
        world.epoch.set(1);
        let serial = handle(&world, &packets, None);
        assert_eq!(serial, handle(&world, &packets, Some(&ahead)));
        // Without the new stamp the old answers would let the player into the wall.
        world.epoch.set(0);
        assert_ne!(serial, handle(&world, &packets, Some(&ahead)));
    }
}
