//! A player's connection as the world sees it, and the play packet handlers of milestone 1
//! (Vanilla's `ServerGamePacketListenerImpl`).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use vanillite_base::Vec3;
use vanillite_protocol::chat::{
    ArgumentSignature, ChatSessionData, LastSeenUpdate, MessageSignature,
};
use vanillite_protocol::packets::common::ChatVisibility;
use vanillite_protocol::packets::common::{ClientInfo, MainHand as WireMainHand};
use vanillite_protocol::packets::play::clientbound::{SystemChat, debug_subscription};
use vanillite_protocol::{Inbound, InboundReceiver};

use crate::block::technical;
use vanillite_protocol::packets::play::serverbound::{
    ClientCommandAction, PlayerAction, SeenAdvancementsAction,
};
use vanillite_protocol::sets::ServerboundPlay;
use vanillite_protocol::text::{Color, Component, NamedColor};

use super::chat::{
    ChatInbox, ChatSession, ChatSettings, MessageChain, TickThrottler, is_chat_message_illegal,
};
use super::chunks::PlayerChunks;
use super::movement::{Body, Kick, MoveContext, MoveRequest, Movement};
use super::play::{Digging, PlayContext};
use super::sink::PacketSink;
use super::{ClientInformation, MainHand};
use crate::inventory::ClickContext;
use vanillite_base::BlockPos;
use vanillite_protocol::packets::play::clientbound::SetHeldSlot;
use vanillite_protocol::packets::play::serverbound::{DigAction, Hand};

/// The state of a player that its packets change. The caller copies it from the entity store
/// and player data before handling packets and writes changed values back.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerState {
    /// Position, rotation and movement flags.
    pub body: Body,
    /// The sneak key is held (`setShiftKeyDown`).
    pub shift_key_down: bool,
    /// Sprinting.
    pub sprinting: bool,
    /// New client settings, when the client sent them.
    pub client: Option<ClientInformation>,
    /// The client asked to respawn (`client_command` `PERFORM_RESPAWN`).
    pub respawn: bool,
    /// `Abilities.flying`.
    pub flying: bool,
}

/// An attack or interaction of a player with an entity, by network id.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EntityAction {
    /// `attack`.
    Attack(i32),
    /// `interact`: hand, hit location relative to the entity, sneaking.
    Interact {
        entity: i32,
        hand: Hand,
        location: Vec3,
        sneaking: bool,
    },
    /// `use_item` (after the block handling): items used on air that need the entities
    /// (boats, carrot on a stick, shields, eating, throwing), with the player's known movement
    /// when the packet arrived (later moves of the tick change it).
    UseItem { hand: Hand, known_movement: Vec3 },
    /// `use_item_on` with an item that acts on entities (spawn eggs, leads on fences).
    UseItemOn {
        hand: Hand,
        pos: vanillite_base::BlockPos,
        face: vanillite_base::Direction,
        /// The clicked point (`BlockHitResult.getLocation`).
        location: Vec3,
        /// The player is an operator (`entity_data` of op-only entity types).
        op: bool,
    },
    /// `FenceBlock.useWithoutItem`: the player's leashed mobs get tied to the fence.
    BindToFence(vanillite_base::BlockPos),
    /// `BlockItem.useOn` falling back to `Item.use`: the consumable in this hand is used
    /// (`startUsingItem`).
    StartUsing(Hand),
    /// `player_action` `RELEASE_USE_ITEM`.
    ReleaseUseItem,
    /// `player_command` `OPEN_INVENTORY` while riding: the vehicle's inventory opens
    /// (`HasCustomInventoryScreen`: chest boats).
    OpenVehicleInventory,
    /// `spectator_action` of a spectator: spectate the entity of this network id.
    Spectate(i32),
    /// `pick_item_from_entity`: the entity's pick result goes to the hand.
    PickItem { entity: i32, include_data: bool },
    /// `player_action` `STAB`: a jab with the piercing weapon in the main hand.
    Stab,
    /// `player_command` `START_FALL_FLYING`.
    StartFallFlying,
    /// `punch`: the swing resets the attack strength (`resetAttackStrengthTicker`).
    Punch,
    /// `stopUsingItem` (a new hotbar slot while the main hand is in use: `main_hand_only`; a
    /// hand swap: any hand).
    StopUsing { main_hand_only: bool },
}

/// What a player sends to steer its vehicle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VehicleInput {
    /// The last `player_input`.
    pub input: vanillite_protocol::packets::play::serverbound::Input,
    /// `paddle_boat`: left and right paddle.
    pub paddles: (bool, bool),
    /// The last `move_vehicle` since the world took it: position, yaw, pitch, on ground.
    pub moved: Option<(Vec3, f32, f32, bool)>,
    /// `player_command` `START_RIDING_JUMP` with its strength, taken by the world.
    pub jump: Option<i32>,
}

/// How handling a player's packets ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handled {
    /// All queued packets (up to the budget) were handled.
    Ok,
    /// The connection is gone; the player leaves.
    Disconnected,
    /// The player must be kicked with this reason; the player leaves.
    Kick(Box<Component>),
}

/// The signed part of a `chat_command_signed` packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedCommand {
    /// Epoch milliseconds.
    pub timestamp: i64,
    /// Salt.
    pub salt: i64,
    /// Signatures of the message arguments.
    pub signatures: Vec<ArgumentSignature>,
    /// The signatures the client had seen.
    pub last_seen: Vec<MessageSignature>,
}

/// A chat or command request of a player, handled by the server after the player's packets in
/// packet order (Vanilla runs them on the server thread through `chatMessageChain`).
#[derive(Clone, Debug)]
pub enum ChatRequest {
    /// A chat message; its last-seen update is already applied.
    Chat {
        /// The text (at most 256 characters, no illegal characters).
        message: String,
        /// Epoch milliseconds.
        timestamp: i64,
        /// Salt.
        salt: i64,
        /// The signature, if the client signed it.
        signature: Option<MessageSignature>,
        /// The signatures the client had seen.
        last_seen: Vec<MessageSignature>,
    },
    /// A command without the leading slash.
    Command {
        /// The command line.
        command: String,
        /// Signature data of `chat_command_signed`.
        signed: Option<SignedCommand>,
    },
    /// The player started a new, validated chat session; other players learn it through the
    /// tab list.
    Session(ChatSessionData),
    /// `openItemGui`: the written book in the hand is resolved for the player (selectors,
    /// scores, NBT), then opened.
    OpenBook(Hand),
    /// `LecternBlockEntity.resolveBook`: the written book put on the lectern at the position is
    /// resolved for the player.
    ResolveLecternBook(BlockPos),
    /// An operator screen packet; the server checks the permissions.
    Admin(AdminRequest),
}

/// The packets of the operator screens, handled by the server in packet order with the chat
/// requests (Vanilla's `ServerGamePacketListenerImpl.handleChangeDifficulty` and friends).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminRequest {
    /// `change_difficulty` with the raw difficulty id.
    ChangeDifficulty(i32),
    /// `lock_difficulty`.
    LockDifficulty(bool),
    /// `change_game_mode` (F3+F4) with the raw game mode id.
    ChangeGameMode(i32),
    /// `set_game_rule`: rule keys and serialized values.
    SetGameRules(Vec<(String, String)>),
    /// `client_command` `REQUEST_GAMERULE_VALUES`.
    RequestGameRuleValues,
    /// `block_entity_tag_query` (F3+I on a block).
    BlockEntityTagQuery { transaction_id: i32, pos: BlockPos },
    /// `entity_tag_query` (F3+I on an entity).
    EntityTagQuery { transaction_id: i32, entity_id: i32 },
    /// `teleport_to_entity`: the spectator menu teleport to the entity with this UUID.
    TeleportToEntity(u128),
    /// `debug_subscription_request`: the requested `debug_subscription` ids as a bit mask.
    DebugSubscriptions(u32),
    /// `custom_payload` on a channel outside the `minecraft` namespace, for plugins.
    PluginMessage {
        /// The channel.
        channel: String,
        /// The payload.
        data: Vec<u8>,
    },
}

/// A player's chat state.
#[derive(Debug)]
pub struct PlayerChatState {
    /// The sender side: session and signed message chain.
    pub chain: MessageChain,
    /// The receiver side, shared with everyone who sends chat to this player.
    pub inbox: Arc<Mutex<ChatInbox>>,
    /// `chatSpamThrottler`.
    pub chat_spam: TickThrottler,
    /// `commandSpamThrottler`.
    pub command_spam: TickThrottler,
    /// The latest tab completion request (Vanilla answers the latest one, one per tick).
    pub suggestion: Option<(i32, String)>,
    /// Requests of this tick, in packet order.
    pub requests: Vec<ChatRequest>,
}

impl PlayerChatState {
    /// The state of a joining player with its receiver side.
    pub fn new(settings: &ChatSettings, inbox: Arc<Mutex<ChatInbox>>) -> PlayerChatState {
        PlayerChatState {
            chain: MessageChain::default(),
            inbox,
            chat_spam: TickThrottler::new(20, settings.chat_spam_threshold),
            command_spam: TickThrottler::new(20, settings.command_spam_threshold),
            suggestion: None,
            requests: Vec::new(),
        }
    }

    /// Locks the receiver side.
    pub fn inbox(&self) -> MutexGuard<'_, ChatInbox> {
        self.inbox.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the chat handlers need from the server.
#[derive(Clone, Copy, Debug)]
pub struct ChatContext<'a> {
    /// The player's UUID.
    pub uuid: u128,
    /// The player's name, for logs.
    pub name: &'a str,
    /// Server chat settings.
    pub settings: &'a ChatSettings,
}

/// A player's connection: packet queues and per-connection protocol state.
pub struct Connection {
    /// Clientbound packets.
    pub sink: Arc<dyn PacketSink>,
    inbound: InboundReceiver,
    /// The packets taken from `inbound` for this tick (kept for its capacity).
    taken: Vec<Inbound>,
    /// Movement validation and teleports.
    pub movement: Movement,
    /// Chunk view and sending.
    pub chunks: PlayerChunks,
    /// Chat and commands.
    pub chat: PlayerChatState,
    /// Attacks and interactions with entities since the last tick (`attack`, `interact`), in
    /// packet order; the world resolves them with the entities.
    pub entity_actions: Vec<EntityAction>,
    /// `STOP_SLEEPING` (leave bed) arrived; the world wakes the player after its packets.
    pub leave_bed: bool,
    /// `ServerPlayer.camera`: the entity a spectator watches through (`None`: itself).
    pub camera: Option<vanillite_world::entity::EntityId>,
    /// Vehicle input: the last `player_input`, `paddle_boat` and `move_vehicle`.
    pub vehicle: VehicleInput,
    /// Digging, placing and item handling.
    pub play: Digging,
    /// A virtual connection (test bot, Bedrock player). These have no chat session, so their
    /// chat is accepted unsigned even when the server enforces secure chat.
    pub is_virtual: bool,
    /// The last action of the player (`ServerPlayer.lastActionTime`), for
    /// `player-idle-timeout`.
    pub last_action: std::time::Instant,
    /// The packets handled since the session recording (`debug.session-recording`) last took
    /// them; `None` while the player is not recorded.
    pub recorded: Option<Vec<ServerboundPlay>>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("movement", &self.movement)
            .field("chunks", &self.chunks)
            .field("chat", &self.chat)
            .finish_non_exhaustive()
    }
}

/// `ChunkMap.getPlayerViewDistance`: the client's view distance clamped to 2 and the
/// server's view distance (SEC-19).
pub fn effective_view_distance(client: &ClientInformation, server_view_distance: i32) -> i32 {
    i32::from(client.view_distance).clamp(2, server_view_distance.max(2))
}

impl From<&ClientInfo> for ClientInformation {
    /// Takes over what the server uses; a negative view distance counts as 0 (it is clamped
    /// to at least 2 where it is used).
    fn from(info: &ClientInfo) -> ClientInformation {
        ClientInformation {
            language: info.language.clone(),
            view_distance: u8::try_from(info.view_distance).unwrap_or(0),
            model_customisation: info.model_customisation,
            main_hand: match info.main_hand {
                WireMainHand::Left => MainHand::Left,
                WireMainHand::Right => MainHand::Right,
            },
            chat_visibility: info.chat_visibility,
            allows_listing: info.allows_listing,
        }
    }
}

impl Connection {
    /// A connection for a player that joins at `pos`.
    pub fn new(
        sink: Arc<dyn PacketSink>,
        inbound: InboundReceiver,
        pos: Vec3,
        memory_connection: bool,
        is_virtual: bool,
        chat: &ChatSettings,
        inbox: Arc<Mutex<ChatInbox>>,
    ) -> Connection {
        Connection {
            sink,
            inbound,
            taken: Vec::new(),
            movement: Movement::new(pos),
            chunks: PlayerChunks::new(memory_connection),
            chat: PlayerChatState::new(chat, inbox),
            entity_actions: Vec::new(),
            leave_bed: false,
            camera: None,
            vehicle: VehicleInput::default(),
            play: Digging::new(),
            last_action: std::time::Instant::now(),
            is_virtual,
            recorded: None,
        }
    }

    /// Handles at most `budget` queued packets in arrival order (SEC-16/17: the rest waits for
    /// the next tick; the network layer bounds the queue).
    pub fn handle_packets(
        &mut self,
        state: &mut PlayerState,
        ctx: MoveContext<'_>,
        chat: ChatContext<'_>,
        play: &mut PlayContext<'_>,
        budget: usize,
    ) -> Handled {
        let mut taken = std::mem::take(&mut self.taken);
        taken.clear();
        let disconnected = self.inbound.take(budget, &mut taken).is_err();
        for inbound in taken.drain(..) {
            if let Some(recorded) = &mut self.recorded {
                recorded.push(inbound.packet.clone());
            }
            play.actor.pos = state.body.pos;
            play.actor.y_rot = state.body.y_rot;
            play.actor.x_rot = state.body.x_rot;
            play.actor.on_ground = state.body.on_ground;
            play.actor.sneaking = state.shift_key_down;
            if let Err(Kick(reason)) = self.handle(inbound.packet, state, ctx, chat, play) {
                return Handled::Kick(reason);
            }
        }
        self.taken = taken;
        if disconnected {
            return Handled::Disconnected;
        }
        if self.sink.is_closed() {
            Handled::Disconnected
        } else {
            Handled::Ok
        }
    }

    fn handle(
        &mut self,
        packet: ServerboundPlay,
        state: &mut PlayerState,
        ctx: MoveContext<'_>,
        chat: ChatContext<'_>,
        play: &mut PlayContext<'_>,
    ) -> Result<(), Kick> {
        // `resetLastActionTime`: the packets that count as an action of the player.
        let loaded = self.movement.has_client_loaded();
        let acts = match &packet {
            ServerboundPlay::PlayerInput(_)
            | ServerboundPlay::PlayerAction(_)
            | ServerboundPlay::UseItemOn(_)
            | ServerboundPlay::UseItem(_) => loaded,
            ServerboundPlay::SetCarriedItem(p) => {
                usize::try_from(p.slot).is_ok_and(|s| s < crate::inventory::HOTBAR)
            }
            ServerboundPlay::Chat(_) => self.chat.inbox().visibility != ChatVisibility::Hidden,
            ServerboundPlay::ChatCommand(_)
            | ServerboundPlay::ChatCommandSigned(_)
            | ServerboundPlay::Punch(_)
            | ServerboundPlay::PlayerCommand(_)
            | ServerboundPlay::Attack(_)
            | ServerboundPlay::ClientCommand(_)
            | ServerboundPlay::ContainerClick(_)
            | ServerboundPlay::PlaceRecipe(_)
            | ServerboundPlay::ContainerButtonClick(_)
            | ServerboundPlay::SignUpdate(_) => true,
            ServerboundPlay::SpectatorAction(_) => {
                loaded && play.actor.game_mode == vanillite_config::GameMode::Spectator
            }
            _ => false,
        };
        let pos_before = state.body.pos;
        self.handle_packet(packet, state, ctx, chat, play)?;
        // `handlePlayerKnownMovement`: a move of more than `sqrt(1e-5)` blocks.
        if acts || state.body.pos.distance_to_sqr(pos_before) > 1.0e-5 {
            self.last_action = std::time::Instant::now();
        }
        Ok(())
    }

    fn handle_packet(
        &mut self,
        packet: ServerboundPlay,
        state: &mut PlayerState,
        ctx: MoveContext<'_>,
        chat: ChatContext<'_>,
        play: &mut PlayContext<'_>,
    ) -> Result<(), Kick> {
        // Bukkit's `PlayerMoveEvent`: where the player was before a movement packet.
        let move_before = matches!(
            packet,
            ServerboundPlay::MovePlayerPos(_)
                | ServerboundPlay::MovePlayerPosRot(_)
                | ServerboundPlay::MovePlayerRot(_)
                | ServerboundPlay::MovePlayerStatusOnly(_)
        )
        .then(|| (state.body, self.movement.moves.len()));
        self.handle_packet_inner(packet, state, ctx, chat, play)?;
        if let Some((before, moves)) = move_before {
            self.player_move(before, moves, &mut state.body, play);
        }
        Ok(())
    }

    /// Bukkit's `PlayerMoveEvent` after a movement packet that moved or turned the player
    /// (not while a teleport waits for its confirmation, nor for passengers): cancelled, the
    /// player goes back to where it was (the move counts for nothing); a changed destination
    /// teleports it there.
    fn player_move(
        &mut self,
        before: super::movement::Body,
        moves: usize,
        body: &mut super::movement::Body,
        play: &PlayContext<'_>,
    ) {
        use vanillite_abi::event;
        if !play.hooks.wants(event::PLAYER_MOVE)
            || self.movement.passenger
            || self.movement.awaiting_teleport()
            || (body.pos == before.pos && body.y_rot == before.y_rot && body.x_rot == before.x_rot)
        {
            return;
        }
        let from = crate::hooks::Place::looking(before.pos, before.y_rot, before.x_rot);
        let to = crate::hooks::Place::looking(body.pos, body.y_rot, body.x_rot);
        let Some(ev) = super::player_event(play.hooks, event::PLAYER_MOVE, play.actor.uuid, |e| {
            e.from = from;
            e.to = to;
        }) else {
            return;
        };
        let sink = self.sink.as_ref();
        if ev.cancelled {
            self.movement.moves.truncate(moves);
            self.movement
                .teleport(body, before.pos, before.y_rot, before.x_rot, sink);
        } else if ev.to != to
            && ev.to.world.is_none()
            && let Some((pos, y_rot, x_rot)) = super::movement::checked_place(&ev.to)
        {
            self.movement.teleport(body, pos, y_rot, x_rot, sink);
        }
    }

    fn handle_packet_inner(
        &mut self,
        packet: ServerboundPlay,
        state: &mut PlayerState,
        ctx: MoveContext<'_>,
        chat: ChatContext<'_>,
        play: &mut PlayContext<'_>,
    ) -> Result<(), Kick> {
        let sink = self.sink.as_ref();
        let ctx = MoveContext {
            blocks: play
                .level
                .level()
                .map(|l| -> &dyn crate::mob::physics::Blocks { l }),
            ..ctx
        };
        let mv =
            |pos: Option<(f64, f64, f64)>,
             rot,
             flags: vanillite_protocol::packets::play::serverbound::MoveFlags| {
                MoveRequest {
                    pos: pos.map(|(x, y, z)| Vec3::new(x, y, z)),
                    rot,
                    on_ground: flags.on_ground,
                    horizontal_collision: flags.horizontal_collision,
                }
            };
        match packet {
            ServerboundPlay::MovePlayerPos(p) => self.movement.handle_move(
                mv(Some((p.x, p.y, p.z)), None, p.flags),
                &mut state.body,
                ctx,
                sink,
            )?,
            ServerboundPlay::MovePlayerPosRot(p) => self.movement.handle_move(
                mv(Some((p.x, p.y, p.z)), Some((p.y_rot, p.x_rot)), p.flags),
                &mut state.body,
                ctx,
                sink,
            )?,
            ServerboundPlay::MovePlayerRot(p) => self.movement.handle_move(
                mv(None, Some((p.y_rot, p.x_rot)), p.flags),
                &mut state.body,
                ctx,
                sink,
            )?,
            ServerboundPlay::MovePlayerStatusOnly(p) => {
                self.movement
                    .handle_move(mv(None, None, p.flags), &mut state.body, ctx, sink)?;
            }
            ServerboundPlay::AcceptTeleportation(p) => {
                self.movement
                    .accept_teleport(&p, &mut state.body, ctx, sink)?;
            }
            ServerboundPlay::ClientTickEnd(_) => self.movement.client_tick_end(),
            ServerboundPlay::ClientCommand(p) => match p.action {
                ClientCommandAction::PerformRespawn => state.respawn = true,
                // `sendStats`.
                ClientCommandAction::RequestStats => {
                    if let Some(stats) = play.stats.as_deref_mut() {
                        sink.send(&stats.award_packet());
                    }
                }
                ClientCommandAction::RequestGameRuleValues => {
                    self.admin(AdminRequest::RequestGameRuleValues);
                }
            },
            ServerboundPlay::SeenAdvancements(p) => {
                // `handleSeenAdvancements`: an opened tab of a known advancement.
                if p.0.action == SeenAdvancementsAction::OpenedTab
                    && let Some(tab) = &p.0.tab
                    && let Some(advancements) = play.advancements.as_deref_mut()
                    && advancements.advancements().index(tab.as_str()).is_some()
                    && let Some(packet) = advancements.select_tab(Some(tab.as_str()))
                {
                    sink.send(&packet);
                }
            }
            ServerboundPlay::PlayerLoaded(_) => self.movement.mark_client_loaded(),
            ServerboundPlay::ChunkBatchReceived(p) => {
                self.chunks.batch_received(p.desired_chunks_per_tick);
            }
            ServerboundPlay::ClientInformation(p) => {
                let info = ClientInformation::from(&p.0);
                self.chat.inbox().visibility = info.chat_visibility;
                state.client = Some(info);
            }
            ServerboundPlay::Chat(p) => {
                let last_seen = self.apply_last_seen(&p.last_seen, chat)?;
                if self.accept_chat(&p.message, false)? {
                    self.chat.requests.push(ChatRequest::Chat {
                        message: p.message,
                        timestamp: p.timestamp,
                        salt: p.salt,
                        signature: p.signature,
                        last_seen,
                    });
                }
            }
            ServerboundPlay::ChatCommand(p) => {
                if self.accept_chat(&p.command, true)? {
                    self.chat.requests.push(ChatRequest::Command {
                        command: p.command,
                        signed: None,
                    });
                }
            }
            ServerboundPlay::ChatCommandSigned(p) => {
                let last_seen = self.apply_last_seen(&p.last_seen, chat)?;
                if self.accept_chat(&p.command, true)? {
                    self.chat.requests.push(ChatRequest::Command {
                        command: p.command,
                        signed: Some(SignedCommand {
                            timestamp: p.timestamp,
                            salt: p.salt,
                            signatures: p.argument_signatures,
                            last_seen,
                        }),
                    });
                }
            }
            ServerboundPlay::ChatAck(p) => {
                let applied = self.chat.inbox().last_seen.apply_offset(p.offset);
                if let Err(e) = applied {
                    log::error!(
                        "Failed to validate message acknowledgement offset from {}: {e}",
                        chat.name
                    );
                    return Err(chat_validation_failed());
                }
            }
            ServerboundPlay::ChatSessionUpdate(p) => self.update_chat_session(p.0, chat)?,
            ServerboundPlay::CommandSuggestion(p) => {
                self.chat.suggestion = Some((p.id, p.command));
            }
            ServerboundPlay::PlayerInput(p) => {
                if self.movement.has_client_loaded() {
                    if p.0.shift != state.shift_key_down {
                        super::player_event(
                            play.hooks,
                            vanillite_abi::event::PLAYER_TOGGLE_SNEAK,
                            play.actor.uuid,
                            |e| e.flags = u32::from(p.0.shift),
                        );
                    }
                    state.shift_key_down = p.0.shift;
                    self.vehicle.input = p.0;
                }
            }
            ServerboundPlay::SpectatorAction(p) => {
                // `handleSpectatorAction`: only loaded spectators, bounded like attacks.
                let spectator = play.actor.game_mode == vanillite_config::GameMode::Spectator;
                if let Some(id) = p.spectate_entity_id
                    && spectator
                    && self.movement.has_client_loaded()
                    && self.entity_actions.len() < 16
                {
                    self.entity_actions.push(EntityAction::Spectate(id));
                }
            }
            ServerboundPlay::Attack(p) => {
                // `handleAttack`: bounded like the other per-tick packet budgets.
                if self.movement.has_client_loaded() && self.entity_actions.len() < 16 {
                    self.entity_actions.push(EntityAction::Attack(p.entity_id));
                }
            }
            ServerboundPlay::Interact(p) => {
                // `handleInteract`: sets the sneak state, then interacts.
                if self.movement.has_client_loaded() {
                    state.shift_key_down = p.using_secondary_action;
                    let l = p.location.0;
                    if self.entity_actions.len() < 16
                        && l.x.is_finite()
                        && l.y.is_finite()
                        && l.z.is_finite()
                    {
                        self.entity_actions.push(EntityAction::Interact {
                            entity: p.entity_id,
                            hand: p.hand,
                            location: Vec3::new(l.x, l.y, l.z),
                            sneaking: p.using_secondary_action,
                        });
                    }
                }
            }
            ServerboundPlay::MoveVehicle(p) => {
                if self.movement.has_client_loaded() {
                    self.vehicle.moved =
                        Some((Vec3::new(p.x, p.y, p.z), p.y_rot, p.x_rot, p.on_ground));
                }
            }
            ServerboundPlay::PaddleBoat(p) => self.vehicle.paddles = (p.left, p.right),
            ServerboundPlay::PlayerCommand(p) => {
                if self.movement.has_client_loaded() {
                    match p.action {
                        PlayerAction::StartSprinting | PlayerAction::StopSprinting => {
                            let sprinting = p.action == PlayerAction::StartSprinting;
                            if sprinting != state.sprinting {
                                super::player_event(
                                    play.hooks,
                                    vanillite_abi::event::PLAYER_TOGGLE_SPRINT,
                                    play.actor.uuid,
                                    |e| e.flags = u32::from(sprinting),
                                );
                            }
                            state.sprinting = sprinting;
                        }
                        // Riding jumps' end comes later.
                        PlayerAction::StartRidingJump => {
                            self.vehicle.jump = Some(p.data);
                        }
                        PlayerAction::OpenInventory => {
                            if self.entity_actions.len() < 16 {
                                self.entity_actions.push(EntityAction::OpenVehicleInventory);
                            }
                        }
                        PlayerAction::StartFallFlying => {
                            if self.entity_actions.len() < 16 {
                                self.entity_actions.push(EntityAction::StartFallFlying);
                            }
                        }
                        // `if (player.isSleeping()) player.stopSleepInBed(false, true)`.
                        PlayerAction::StopSleeping => self.leave_bed = true,
                        PlayerAction::StopRidingJump => {}
                    }
                }
            }
            ServerboundPlay::PlayerAction(p) => {
                if self.movement.has_client_loaded() {
                    self.handle_player_action(p, play);
                }
            }
            ServerboundPlay::UseItemOn(p) => {
                if self.movement.has_client_loaded() {
                    let stack = match p.hand {
                        Hand::MainHand => play.inventory.selected_item(),
                        Hand::OffHand => play
                            .inventory
                            .equipment(crate::item::EquipmentSlot::OffHand),
                    };
                    let name = crate::item::item_id(stack).name();
                    let entity_item = crate::player::item_use::uses_entities_on_block(stack);
                    self.play.use_item_on(play, &p, self.sink.as_ref());
                    // `LecternBlockEntity.setBook` resolves a written book for the player.
                    let at = BlockPos::new(p.pos.x, p.pos.y, p.pos.z);
                    if name == "minecraft:written_book"
                        && let Some(level) = play.level.level_mut()
                        && let Some(crate::level::BlockEntity::Lectern(l)) = level.block_entity(at)
                        && crate::book::needs_resolving(&l.book)
                    {
                        self.chat.requests.push(ChatRequest::ResolveLecternBook(at));
                    }
                    if let Some(fence) = self.play.bind_fence.take()
                        && self.entity_actions.len() < 16
                    {
                        self.entity_actions.push(EntityAction::BindToFence(fence));
                    }
                    if let Some(hand) = self.play.start_using.take()
                        && self.entity_actions.len() < 16
                    {
                        self.entity_actions.push(EntityAction::StartUsing(hand));
                    }
                    if entity_item && self.play.entity_item_use && self.entity_actions.len() < 16 {
                        let pos = vanillite_base::BlockPos::new(p.pos.x, p.pos.y, p.pos.z);
                        self.entity_actions.push(EntityAction::UseItemOn {
                            hand: p.hand,
                            pos,
                            face: crate::player::play::direction_wrapped(p.face),
                            location: Vec3::new(
                                f64::from(pos.x) + f64::from(p.cursor_x),
                                f64::from(pos.y) + f64::from(p.cursor_y),
                                f64::from(pos.z) + f64::from(p.cursor_z),
                            ),
                            op: play.actor.permission_level > 0,
                        });
                    }
                }
            }
            ServerboundPlay::UseItem(p) => {
                if self.movement.has_client_loaded() {
                    // `handleUseItem`: a held item turns the player to the packet's rotation
                    // first.
                    let stack = match p.hand {
                        Hand::MainHand => play.inventory.selected_item(),
                        Hand::OffHand => &play.inventory.offhand,
                    };
                    if !stack.is_empty() {
                        let y_rot = crate::player::movement::wrap_degrees(p.y_rot);
                        let x_rot = crate::player::movement::wrap_degrees(p.x_rot);
                        if x_rot != state.body.x_rot || y_rot != state.body.y_rot {
                            crate::player::movement::snap_rotation(&mut state.body, y_rot, x_rot);
                        }
                    }
                    let handled = self
                        .play
                        .use_item(play, p.hand, p.sequence, p.y_rot, p.x_rot);
                    if !handled && self.entity_actions.len() < 16 {
                        self.entity_actions.push(EntityAction::UseItem {
                            hand: p.hand,
                            known_movement: self.movement.known_movement,
                        });
                    }
                }
            }
            ServerboundPlay::Punch(_) => {
                // Bukkit's `PlayerInteractEvent` `LEFT_CLICK_AIR`: cancelled, nobody sees the
                // swing.
                let swing = !play.hooks.wants(vanillite_abi::event::PLAYER_INTERACT)
                    || play.hooks.player_interact(
                        play.level.hook_world(),
                        play.actor.uuid,
                        vanillite_abi::interact::LEFT_CLICK_AIR,
                        vanillite_abi::hand::MAIN,
                        crate::item::item_id(play.inventory.selected_item()).name(),
                        None,
                    );
                if swing {
                    // `handlePunch`: the main hand's `attack_animation`.
                    play.swings.push((Hand::MainHand, true));
                }
                if self.entity_actions.len() < 16 {
                    self.entity_actions.push(EntityAction::Punch);
                }
            }
            ServerboundPlay::SetCarriedItem(p) => match usize::try_from(p.slot) {
                Ok(slot) if slot < crate::inventory::HOTBAR => {
                    // Bukkit's `PlayerItemHeldEvent`: cancelled, the client goes back.
                    let old = play.inventory.selected;
                    let cancelled = super::player_event(
                        play.hooks,
                        vanillite_abi::event::PLAYER_ITEM_HELD,
                        play.actor.uuid,
                        |e| {
                            e.value = i64::try_from(slot).unwrap_or(0);
                            e.extra = i64::try_from(old).unwrap_or(0);
                        },
                    )
                    .is_some_and(|e| e.cancelled);
                    if cancelled {
                        sink.send(&SetHeldSlot {
                            slot: i32::try_from(old).unwrap_or(0),
                        });
                    } else {
                        // `handleSetCarriedItem`: another slot stops a main hand use.
                        if slot != old && self.entity_actions.len() < 16 {
                            self.entity_actions.push(EntityAction::StopUsing {
                                main_hand_only: true,
                            });
                        }
                        play.inventory.selected = slot;
                    }
                }
                _ => log::warn!("{} tried to set an invalid carried item", chat.name),
            },
            ServerboundPlay::SetCreativeModeSlot(p) => {
                self.play.set_creative_slot(play, p.slot, p.item);
                play.inventory.broadcast_changes(self.sink.as_ref());
            }
            ServerboundPlay::ContainerClick(p) => self.container_click(p, play),
            ServerboundPlay::PlaceRecipe(p) => self.place_recipe(&p, play),
            ServerboundPlay::RecipeBookSeenRecipe(p) => {
                if let Some(book) = play.recipe_book.as_deref_mut() {
                    book.remove_highlight(&play.data.recipes.book, p.recipe);
                }
            }
            ServerboundPlay::RecipeBookChangeSettings(p) => {
                if let Some(book) = play.recipe_book.as_deref_mut() {
                    book.set_settings(p.book_type, p.open, p.filtering);
                }
            }
            ServerboundPlay::ContainerClose(_) => {
                // `doCloseContainer`: whatever menu is open (Vanilla does not check the id).
                let infinite = play.actor.game_mode == vanillite_config::GameMode::Creative;
                if play.inventory.container.is_some() {
                    let who = crate::menu::container::Opener {
                        spectator: play.actor.game_mode == vanillite_config::GameMode::Spectator,
                        infinite,
                        disconnected: false,
                    };
                    if let Some(level) = play.level.level_mut() {
                        crate::menu::container::close(play.inventory, level, who, play.drops, None);
                    }
                } else {
                    play.inventory.close(infinite, play.drops);
                }
            }
            ServerboundPlay::PickItemFromBlock(p) => {
                self.pick_item_from_block(p.pos, p.include_data, play);
            }
            ServerboundPlay::PickItemFromEntity(p) => {
                if self.entity_actions.len() < 16 {
                    self.entity_actions.push(EntityAction::PickItem {
                        entity: p.id,
                        include_data: p.include_data,
                    });
                }
            }
            ServerboundPlay::EditBook(p) => {
                // `handleEditBook`: a hotbar slot or the off hand.
                let slot = usize::try_from(p.slot)
                    .ok()
                    .filter(|&s| s < crate::inventory::HOTBAR || s == crate::inventory::OFFHAND);
                if let Some(slot) = slot
                    && let Some(stack) = play.inventory.get_mut(slot)
                {
                    match &p.title {
                        Some(title) => {
                            if let Some(book) = crate::book::sign(stack, title, chat.name, &p.pages)
                            {
                                *stack = book;
                            }
                        }
                        None => crate::book::update_pages(stack, &p.pages),
                    }
                }
            }
            ServerboundPlay::SelectBundleItem(p) => {
                self.select_bundle_item(p.slot_id, p.selected_item_index, play);
            }
            ServerboundPlay::ContainerButtonClick(p) => {
                self.container_button(p.container_id, p.button_id, play)
            }
            ServerboundPlay::RenameItem(p) => self.rename_item(&p.name, play),
            ServerboundPlay::SelectTrade(p) => self.select_trade(p.item, play),
            ServerboundPlay::SetBeacon(p) => self.set_beacon(p.primary, p.secondary, play),
            ServerboundPlay::ContainerSlotStateChanged(p) => {
                self.container_slot_state(p.slot_id, p.container_id, p.new_state, play);
            }
            ServerboundPlay::SignUpdate(p) => {
                // `handleSignUpdate`: the editing player sets one side of a loaded sign.
                let pos = BlockPos::new(p.pos.x, p.pos.y, p.pos.z);
                let uuid = play.actor.uuid;
                if let Some(level) = play.level.level_mut()
                    && level.is_loaded(pos)
                {
                    crate::block::sign::update_text(level, pos, uuid, p.front, &p.lines);
                }
            }
            ServerboundPlay::SetCommandBlock(p) => {
                self.technical(play, |l, e| technical::command_block::handle(l, p, e));
            }
            ServerboundPlay::SetCommandMinecart(p) => {
                self.technical(play, |l, e| {
                    technical::command_block::handle_minecart(l, p, e)
                });
            }
            ServerboundPlay::SetStructureBlock(p) => {
                self.technical(play, |l, e| technical::structure_block::handle(l, p, e));
            }
            ServerboundPlay::SetJigsawBlock(p) => {
                self.technical(play, |l, e| technical::jigsaw::handle(l, p, e));
            }
            ServerboundPlay::JigsawGenerate(p) => {
                self.technical(play, |l, e| technical::jigsaw::handle_generate(l, p, e));
            }
            ServerboundPlay::SetTestBlock(p) => {
                self.technical(play, |l, e| technical::test_block::handle(l, p, e));
            }
            ServerboundPlay::TestInstanceBlockAction(p) => {
                self.technical(play, |l, e| technical::test_instance::handle(l, p, e));
            }
            // `handlePlayerAbilities`.
            ServerboundPlay::PlayerAbilities(p) => {
                let flying = p.is_flying()
                    && crate::survival::player::Abilities::of(play.actor.game_mode).mayfly;
                // Bukkit's `PlayerToggleFlightEvent`: cancelled, the client is corrected.
                let cancelled = flying != state.flying
                    && super::player_event(
                        play.hooks,
                        vanillite_abi::event::PLAYER_TOGGLE_FLIGHT,
                        play.actor.uuid,
                        |e| e.flags = u32::from(flying),
                    )
                    .is_some_and(|e| e.cancelled);
                if cancelled {
                    super::join::send_abilities(sink, play.actor.game_mode, state.flying);
                } else {
                    state.flying = flying;
                }
            }
            ServerboundPlay::ChangeDifficulty(p) => {
                self.admin(AdminRequest::ChangeDifficulty(p.difficulty));
            }
            ServerboundPlay::LockDifficulty(p) => {
                self.admin(AdminRequest::LockDifficulty(p.locked));
            }
            ServerboundPlay::ChangeGameMode(p) => self.admin(AdminRequest::ChangeGameMode(p.mode)),
            ServerboundPlay::SetGameRule(p) => {
                let entries = p
                    .entries
                    .into_iter()
                    .map(|e| (e.rule.to_string(), e.value))
                    .collect();
                self.admin(AdminRequest::SetGameRules(entries));
            }
            ServerboundPlay::BlockEntityTagQuery(p) => {
                self.admin(AdminRequest::BlockEntityTagQuery {
                    transaction_id: p.transaction_id,
                    pos: BlockPos::new(p.pos.x, p.pos.y, p.pos.z),
                });
            }
            ServerboundPlay::EntityTagQuery(p) => self.admin(AdminRequest::EntityTagQuery {
                transaction_id: p.transaction_id,
                entity_id: p.entity_id,
            }),
            ServerboundPlay::TeleportToEntity(p) => {
                self.admin(AdminRequest::TeleportToEntity(p.uuid.0));
            }
            // `handleDebugSubscriptionRequest`: unknown ids are a decode error in Vanilla.
            ServerboundPlay::DebugSubscriptionRequest(p) => {
                let mut mask = 0u32;
                for id in p.subscriptions.0 {
                    if !(0..debug_subscription::COUNT).contains(&id) {
                        return Err(Kick(Box::new(Component::translatable(
                            "multiplayer.disconnect.invalid_packet",
                            vec![],
                        ))));
                    }
                    mask |= 1u32.checked_shl(id.unsigned_abs()).unwrap_or(0);
                }
                self.admin(AdminRequest::DebugSubscriptions(mask));
            }
            // The server never asks the client to reconfigure yet.
            ServerboundPlay::ConfigurationAcknowledged(_) => {
                return Err(Kick(Box::new(Component::translatable(
                    "multiplayer.disconnect.invalid_packet",
                    vec![],
                ))));
            }
            // Payloads of other namespaces may be plugin messages; Vanilla ignores them.
            // Bukkit's `PlayerResourcePackStatusEvent` (the network task answered the status).
            ServerboundPlay::ResourcePack(p) => {
                super::player_event(
                    play.hooks,
                    vanillite_abi::event::PLAYER_RESOURCE_PACK,
                    play.actor.uuid,
                    |e| {
                        e.key = p.id.to_string();
                        e.cause = resource_pack_status(p.action);
                    },
                );
            }
            ServerboundPlay::CustomPayload(p) if p.channel.namespace() != "minecraft" => {
                self.admin(AdminRequest::PluginMessage {
                    channel: p.channel.as_str().to_owned(),
                    data: p.data.0,
                });
            }
            // `handleCookieResponse`: the server never asks for cookies.
            ServerboundPlay::CookieResponse(_) => {
                return Err(Kick(Box::new(Component::translatable(
                    "multiplayer.disconnect.unexpected_query_response",
                    vec![],
                ))));
            }
            // Keep-alives and ping requests are answered by the network task; custom payloads
            // and pongs are no-ops in Vanilla (`handleCustomClickAction` only logs).
            ServerboundPlay::CustomClickAction(_)
            | ServerboundPlay::CustomPayload(_)
            | ServerboundPlay::KeepAlive(_)
            | ServerboundPlay::PingRequest(_)
            | ServerboundPlay::Pong(_)
            | ServerboundPlay::Unhandled { .. } => {}
        }
        Ok(())
    }

    /// The edit packets of the technical blocks: run on the player's level with the player as
    /// the editor.
    fn technical(
        &self,
        play: &mut PlayContext<'_>,
        f: impl FnOnce(&mut crate::level::Level, &technical::Editor<'_>),
    ) {
        let actor = &play.actor;
        let editor = technical::Editor {
            player: actor.entity,
            uuid: actor.uuid,
            allowed: technical::can_use_game_master_blocks(
                actor.game_mode == vanillite_config::GameMode::Creative,
                actor.permission_level,
            ),
            permission_level: actor.permission_level,
            sink: self.sink.as_ref(),
        };
        if let Some(level) = play.level.level_mut() {
            f(level, &editor);
        }
    }

    /// Queues an operator screen request. The queue is bounded like the other per-tick
    /// request lists; requests beyond the bound are dropped.
    fn admin(&mut self, request: AdminRequest) {
        if self.chat.requests.len() < 64 {
            self.chat.requests.push(ChatRequest::Admin(request));
        }
    }

    /// `handlePlayerAction`.
    fn handle_player_action(
        &mut self,
        p: vanillite_protocol::packets::play::serverbound::PlayerActionPacket,
        play: &mut PlayContext<'_>,
    ) {
        let spectator = play.actor.game_mode == vanillite_config::GameMode::Spectator;
        match p.action {
            DigAction::SwapItemWithOffhand => {
                // Bukkit's `PlayerSwapHandItemsEvent`: cancelled, the hands stay.
                if !spectator
                    && !super::player_event(
                        play.hooks,
                        vanillite_abi::event::PLAYER_SWAP_HANDS,
                        play.actor.uuid,
                        |e| {
                            e.item = crate::hooks::item_snbt(play.inventory.selected_item());
                            e.other_item = crate::hooks::item_snbt(&play.inventory.offhand);
                        },
                    )
                    .is_some_and(|e| e.cancelled)
                {
                    play.inventory.swap_hands();
                    if self.entity_actions.len() < 16 {
                        self.entity_actions.push(EntityAction::StopUsing {
                            main_hand_only: false,
                        });
                    }
                }
            }
            DigAction::DropItem | DigAction::DropAllItems => {
                // `ServerPlayer.drop(all)`: the swing comes with a dropped stack
                // (`LivingEntity.drop`), then `resetAttackStrengthTicker`.
                let spectator = play.actor.game_mode == vanillite_config::GameMode::Spectator;
                self.play
                    .drop_selected(play, p.action == DigAction::DropAllItems);
                if !spectator && self.entity_actions.len() < 16 {
                    self.entity_actions.push(EntityAction::Punch);
                }
            }
            DigAction::ReleaseUseItem => {
                if self.entity_actions.len() < 16 {
                    self.entity_actions.push(EntityAction::ReleaseUseItem);
                }
            }
            DigAction::Stab => {
                if self.entity_actions.len() < 16 {
                    self.entity_actions.push(EntityAction::Stab);
                }
            }
            DigAction::StartDestroyBlock
            | DigAction::ChangeDestroyDirection
            | DigAction::AbortDestroyBlock
            | DigAction::StopDestroyBlock => {
                let pos = BlockPos::new(p.pos.x, p.pos.y, p.pos.z);
                self.play.handle_action(
                    play,
                    p.action,
                    pos,
                    p.direction,
                    p.sequence,
                    self.sink.as_ref(),
                );
            }
        }
    }

    /// `handleContainerClick` for the open container menu.
    fn container_menu_click(
        &mut self,
        p: vanillite_protocol::packets::play::serverbound::ContainerClick,
        play: &mut PlayContext<'_>,
    ) {
        let sink = self.sink.as_ref();
        let actor = play.actor;
        let PlayContext {
            level,
            inventory,
            drops,
            xp,
            random,
            hooks,
            ..
        } = play;
        let hooks = *hooks;
        let Some(level) = level.level_mut() else {
            return;
        };
        if actor.game_mode == vanillite_config::GameMode::Spectator {
            crate::menu::container::with_view(inventory, level, actor.pos, None, None, |view| {
                crate::menu::send_all(view, sink);
            });
            return;
        }
        let in_range = |pos| actor.within_block_range(pos, 4.0);
        if !crate::menu::container::still_valid(inventory, level, &in_range, &|b| {
            actor.within_entity_range(b, 4.0)
        }) {
            log::debug!("A player interacted with an invalid menu");
            return;
        }
        let ctx = ClickContext {
            infinite: actor.game_mode == vanillite_config::GameMode::Creative,
        };
        crate::menu::container::with_view(
            inventory,
            level,
            actor.pos,
            xp.as_deref_mut(),
            random.as_deref_mut(),
            |view| {
                if !crate::menu::is_valid_slot_index(view, p.slot) {
                    log::debug!("A player clicked invalid slot index {}", p.slot);
                    return;
                }
                // Plugins may cancel the click: nothing changes, the client is corrected.
                if !view.click_events(hooks, actor.uuid, p.slot, p.button, p.input) {
                    crate::menu::send_all(view, sink);
                    return;
                }
                let full_resync = p.state_id != view.menu.state.state_id();
                view.menu.state.suppress_remote = true;
                crate::menu::click(view, p.slot, p.button, p.input, ctx, drops);
                view.craft_prepare_event(hooks, actor.uuid);
                view.flush_result_updates(sink);
                crate::menu::receive_predictions(&mut view.menu.state, &p.changed, &p.carried);
                view.menu.state.suppress_remote = false;
                if full_resync {
                    crate::menu::broadcast_full_state(view, sink);
                } else {
                    crate::menu::broadcast_changes(view, sink);
                }
            },
        );
    }

    /// `handlePlaceRecipe`: a known recipe from the recipe book into the open crafting grid or
    /// furnace; a ghost recipe when items are missing.
    fn place_recipe(
        &mut self,
        p: &vanillite_protocol::packets::play::serverbound::PlaceRecipe,
        play: &mut PlayContext<'_>,
    ) {
        use crate::menu::container::MenuKind;
        use crate::recipe::book::RecipeRef;
        use crate::recipe::place::{self, Grid};
        let sink = self.sink.as_ref();
        let actor = play.actor;
        if actor.game_mode == vanillite_config::GameMode::Spectator {
            return;
        }
        let recipes = &play.data.recipes;
        let Some(entry) = recipes.book.entry(p.recipe) else {
            return;
        };
        if !play
            .recipe_book
            .as_deref()
            .is_some_and(|b| b.contains(&entry.id))
        {
            return;
        }
        let Some(placement) = recipes.placement(entry.recipe) else {
            log::debug!("A player tried to place impossible recipe {}", entry.id);
            return;
        };
        let creative = actor.game_mode == vanillite_config::GameMode::Creative;
        let open_id = play
            .inventory
            .container
            .as_ref()
            .map_or(0, |m| m.state.container_id);
        if open_id != p.container_id {
            return;
        }
        let grid_of = |w: usize, h: usize, first: usize| {
            let input: Vec<usize> = (first..first.saturating_add(w.saturating_mul(h))).collect();
            let mut content = vec![0];
            content.extend_from_slice(&input);
            Grid {
                width: w,
                height: h,
                clear: input.clone(),
                account: input.clone(),
                input,
                clear_content: content,
                account_full: false,
                suppress: false,
                content_direct: true,
            }
        };
        let ghost = if play.inventory.container.is_none() {
            let RecipeRef::Crafting(index) = entry.recipe else {
                return;
            };
            let input = crate::recipe::CraftingInput::of(2, 2, &play.inventory.craft);
            let matches = recipes.crafting_matches(index, &input);
            let grid = grid_of(2, 2, 1);
            let ghost = place::place(
                &mut *play.inventory,
                &grid,
                &placement,
                matches,
                p.use_max_items,
                creative,
                play.drops,
            );
            play.inventory
                .craft_prepare_event(play.hooks, play.actor.uuid);
            play.inventory.flush_result_updates(sink);
            ghost
        } else {
            let hooks = play.hooks;
            let PlayContext {
                level,
                inventory,
                drops,
                xp,
                random,
                ..
            } = play;
            let Some(level) = level.level_mut() else {
                return;
            };
            let in_range = |pos| actor.within_block_range(pos, 4.0);
            if !crate::menu::container::still_valid(inventory, level, &in_range, &|b| {
                actor.within_entity_range(b, 4.0)
            }) {
                log::debug!("A player interacted with an invalid menu");
                return;
            }
            crate::menu::container::with_view(
                inventory,
                level,
                actor.pos,
                xp.as_deref_mut(),
                random.as_deref_mut(),
                |view| {
                    let (grid, matches) = match (view.menu.kind, entry.recipe) {
                        (MenuKind::Crafting, RecipeRef::Crafting(index)) => {
                            let cells = view.menu.local.get(1..10).unwrap_or_default();
                            let input = crate::recipe::CraftingInput::of(3, 3, cells);
                            let mut grid = grid_of(3, 3, 1);
                            grid.suppress = true;
                            (grid, recipes.crafting_matches(index, &input))
                        }
                        (MenuKind::Furnace(_), RecipeRef::Cooking(index)) => {
                            let matches = recipes.cooking.get(index).is_some_and(|r| {
                                r.ingredient.test(crate::menu::MenuSlots::item(view, 0))
                            });
                            let grid = Grid {
                                width: 1,
                                height: 1,
                                input: vec![0],
                                clear: vec![0, 2],
                                clear_content: vec![0, 2],
                                account: vec![0, 1, 2],
                                account_full: true,
                                suppress: false,
                                content_direct: false,
                            };
                            (grid, matches)
                        }
                        _ => return false,
                    };
                    let ghost = place::place(
                        view,
                        &grid,
                        &placement,
                        matches,
                        p.use_max_items,
                        creative,
                        drops,
                    );
                    view.craft_prepare_event(hooks, actor.uuid);
                    view.flush_result_updates(sink);
                    ghost
                },
            )
            .unwrap_or(false)
        };
        if ghost && let Some(display) = recipes.book.display(p.recipe) {
            sink.send(
                &vanillite_protocol::packets::play::clientbound::PlaceGhostRecipe {
                    container_id: p.container_id,
                    display,
                },
            );
        }
    }

    /// `handleContainerButtonClick`.
    fn container_button(&mut self, container_id: i32, button: i32, play: &mut PlayContext<'_>) {
        let sink = self.sink.as_ref();
        let actor = play.actor;
        if actor.game_mode == vanillite_config::GameMode::Spectator
            || play
                .inventory
                .container
                .as_ref()
                .is_none_or(|m| m.state.container_id != container_id)
        {
            return;
        }
        let PlayContext {
            level,
            inventory,
            xp,
            random,
            drops,
            ..
        } = play;
        let Some(level) = level.level_mut() else {
            return;
        };
        let in_range = |pos| actor.within_block_range(pos, 4.0);
        if !crate::menu::container::still_valid(inventory, level, &in_range, &|b| {
            actor.within_entity_range(b, 4.0)
        }) {
            log::debug!("A player interacted with an invalid menu");
            return;
        }
        let may_build = actor.game_mode != vanillite_config::GameMode::Adventure;
        crate::menu::container::with_view(
            inventory,
            level,
            actor.pos,
            xp.as_deref_mut(),
            random.as_deref_mut(),
            |view| {
                view.menu.may_build = may_build;
                if view.button(button, drops) {
                    crate::menu::broadcast_changes(view, sink);
                }
            },
        );
    }

    /// `handleContainerSlotStateChanged`: a crafter slot is enabled or disabled.
    fn container_slot_state(
        &mut self,
        slot: i32,
        container_id: i32,
        enabled: bool,
        play: &mut PlayContext<'_>,
    ) {
        let actor = play.actor;
        if actor.game_mode == vanillite_config::GameMode::Spectator
            || play
                .inventory
                .container
                .as_ref()
                .is_none_or(|m| m.state.container_id != container_id)
        {
            return;
        }
        let PlayContext {
            level, inventory, ..
        } = play;
        let Some(level) = level.level_mut() else {
            return;
        };
        crate::menu::container::with_view(inventory, level, actor.pos, None, None, |view| {
            view.crafter_slot_state(slot, enabled);
        });
    }

    /// `handleSetBeaconPacket`: the powers of the open beacon menu.
    fn set_beacon(
        &mut self,
        primary: Option<i32>,
        secondary: Option<i32>,
        play: &mut PlayContext<'_>,
    ) {
        let actor = play.actor;
        let PlayContext {
            level, inventory, ..
        } = play;
        let Some(level) = level.level_mut() else {
            return;
        };
        if inventory
            .container
            .as_ref()
            .is_none_or(|m| m.kind != crate::menu::container::MenuKind::Beacon)
        {
            return;
        }
        let in_range = |pos| actor.within_block_range(pos, 4.0);
        if !crate::menu::container::still_valid(inventory, level, &in_range, &|b| {
            actor.within_entity_range(b, 4.0)
        }) {
            log::debug!("A player interacted with an invalid menu");
            return;
        }
        crate::menu::container::with_view(inventory, level, actor.pos, None, None, |view| {
            view.update_beacon(primary, secondary);
        });
    }

    /// `handleSelectTrade`: the offer picked in the open merchant menu.
    fn select_trade(&mut self, item: i32, play: &mut PlayContext<'_>) {
        let sink = self.sink.as_ref();
        let actor = play.actor;
        let PlayContext {
            level, inventory, ..
        } = play;
        let Some(level) = level.level_mut() else {
            return;
        };
        if inventory
            .container
            .as_ref()
            .is_none_or(|m| m.kind != crate::menu::container::MenuKind::Merchant)
        {
            return;
        }
        let in_range = |pos| actor.within_block_range(pos, 4.0);
        if !crate::menu::container::still_valid(inventory, level, &in_range, &|b| {
            actor.within_entity_range(b, 4.0)
        }) {
            log::debug!("A player interacted with an invalid menu");
            return;
        }
        crate::menu::container::with_view(inventory, level, actor.pos, None, None, |view| {
            view.merchant_select(item);
            view.flush_result_updates(sink);
            crate::menu::broadcast_changes(view, sink);
        });
    }

    /// `handleRenameItem`: the anvil's name field.
    fn rename_item(&mut self, name: &str, play: &mut PlayContext<'_>) {
        let actor = play.actor;
        let PlayContext {
            level,
            inventory,
            xp,
            random,
            ..
        } = play;
        let Some(level) = level.level_mut() else {
            return;
        };
        if inventory
            .container
            .as_ref()
            .is_none_or(|m| m.kind != crate::menu::container::MenuKind::Anvil)
        {
            return;
        }
        let in_range = |pos| actor.within_block_range(pos, 4.0);
        if !crate::menu::container::still_valid(inventory, level, &in_range, &|b| {
            actor.within_entity_range(b, 4.0)
        }) {
            log::debug!("A player interacted with an invalid menu");
            return;
        }
        crate::menu::container::with_view(
            inventory,
            level,
            actor.pos,
            xp.as_deref_mut(),
            random.as_deref_mut(),
            |view| {
                view.menu.state.creative = actor.game_mode == vanillite_config::GameMode::Creative;
                view.rename(name);
            },
        );
    }

    /// `handleContainerClick`: clicks go to the open menu with that id.
    fn container_click(
        &mut self,
        p: vanillite_protocol::packets::play::serverbound::ContainerClick,
        play: &mut PlayContext<'_>,
    ) {
        if let Some(open) = &play.inventory.container {
            if p.container_id == open.state.container_id {
                self.container_menu_click(p, play);
                let sounds = play
                    .inventory
                    .container
                    .as_mut()
                    .map(|c| std::mem::take(&mut c.state.sounds))
                    .unwrap_or_default();
                play_bundle_sounds(play, sounds);
            }
            return;
        }
        if p.container_id != 0 {
            return;
        }
        let sink = self.sink.as_ref();
        if play.actor.game_mode == vanillite_config::GameMode::Spectator {
            play.inventory.send_all(sink);
            return;
        }
        // `isValidSlotIndex`: -1, -999 or a slot of the menu.
        let valid = p.slot == -1
            || p.slot == -999
            || usize::try_from(p.slot).is_ok_and(|s| s < crate::inventory::MENU_SLOTS);
        if !valid {
            log::debug!("A player clicked invalid slot index {}", p.slot);
            return;
        }
        // Plugins may cancel the click: nothing changes, the client is corrected.
        if !play
            .inventory
            .click_events(play.hooks, play.actor.uuid, p.slot, p.button, p.input)
        {
            play.inventory.send_all(sink);
            return;
        }
        let full_resync = p.state_id != play.inventory.state_id();
        let ctx = ClickContext {
            infinite: play.actor.game_mode == vanillite_config::GameMode::Creative,
        };
        play.inventory.menu.suppress_remote = true;
        play.inventory
            .click(p.slot, p.button, p.input, ctx, play.drops);
        super::play::flush_equips(play);
        let sounds = std::mem::take(&mut play.inventory.menu.sounds);
        play_bundle_sounds(play, sounds);
        play.inventory
            .craft_prepare_event(play.hooks, play.actor.uuid);
        play.inventory.flush_result_updates(sink);
        play.inventory.receive_predictions(&p.changed, &p.carried);
        play.inventory.menu.suppress_remote = false;
        if full_resync {
            play.inventory.broadcast_full_state(sink);
        } else {
            play.inventory.broadcast_changes(sink);
        }
    }

    /// `AbstractContainerMenu.setSelectedBundleItemIndex` of the open menu.
    fn select_bundle_item(&mut self, slot: i32, index: i32, play: &mut PlayContext<'_>) {
        let Ok(slot) = usize::try_from(slot) else {
            return;
        };
        let stack = if play.inventory.container.is_some() {
            let pos = play.actor.pos;
            let Some(level) = play.level.level_mut() else {
                return;
            };
            crate::menu::container::with_view(play.inventory, level, pos, None, None, |view| {
                use crate::menu::MenuSlots;
                (slot < view.slot_count()).then(|| view.item(slot).clone())
            })
            .flatten()
        } else {
            use crate::menu::MenuSlots;
            (slot < crate::inventory::MENU_SLOTS).then(|| play.inventory.item(slot).clone())
        };
        let Some(stack) = stack else {
            return;
        };
        let selection = play.inventory.bundle_selection;
        if let Some(mut contents) = crate::bundle::Contents::of(&stack, selection) {
            contents.toggle_selected(index);
            play.inventory.bundle_selection =
                (contents.selected >= 0).then(|| (crate::bundle::key(&stack), contents.selected));
        }
    }

    /// `handlePickItemFromBlock` (middle click).
    fn pick_item_from_block(
        &mut self,
        pos: vanillite_protocol::types::BlockPos,
        include_data: bool,
        play: &mut PlayContext<'_>,
    ) {
        let pos = BlockPos::new(pos.x, pos.y, pos.z);
        if !play.actor.within_block_range(pos, 1.0) || !play.level.is_loaded(pos) {
            return;
        }
        let state = play.level.block(pos);
        let item = clone_item(state.block());
        if item == u16::MAX || item == 0 {
            return;
        }
        let infinite = play.actor.game_mode == vanillite_config::GameMode::Creative;
        // `includeData` only for players with infinite materials.
        let stack = match play.level.level() {
            Some(level) => {
                crate::level::block_entity::pick_stack(level, pos, item, infinite && include_data)
            }
            None => crate::item::ItemStack::new(item, 1),
        };
        play.inventory.pick_item(stack, infinite);
        let slot = i32::try_from(play.inventory.selected).unwrap_or(0);
        self.sink.send(&SetHeldSlot { slot });
        play.inventory.broadcast_changes(self.sink.as_ref());
    }

    /// `unpackAndApplyLastSeen`: a broken acknowledgement kicks the player.
    fn apply_last_seen(
        &self,
        update: &LastSeenUpdate,
        chat: ChatContext<'_>,
    ) -> Result<Vec<MessageSignature>, Kick> {
        let applied = self.chat.inbox().last_seen.apply_update(update);
        applied.map_err(|e| {
            log::error!(
                "Failed to validate message acknowledgements from {}: {e}",
                chat.name
            );
            chat_validation_failed()
        })
    }

    /// `tryHandleChat`: illegal characters kick; chat (not commands) is refused while the
    /// player hides chat. Returns whether the request goes on.
    fn accept_chat(&self, message: &str, is_command: bool) -> Result<bool, Kick> {
        if is_chat_message_illegal(message) {
            return Err(Kick(Box::new(Component::translatable(
                "multiplayer.disconnect.illegal_characters",
                vec![],
            ))));
        }
        if !is_command && self.chat.inbox().visibility == ChatVisibility::Hidden {
            self.sink.send(&SystemChat {
                content: Component::translatable("chat.disabled.options", vec![])
                    .color(Color::Named(NamedColor::Red)),
                overlay: false,
            });
            return Ok(false);
        }
        Ok(true)
    }

    /// `handleChatSessionUpdate`.
    fn update_chat_session(
        &mut self,
        data: ChatSessionData,
        chat: ChatContext<'_>,
    ) -> Result<(), Kick> {
        let old = self.chat.chain.session().map(|s| &s.data);
        let same_key = old.is_some_and(|o| {
            o.expires_at == data.expires_at
                && o.public_key == data.public_key
                && o.key_signature == data.key_signature
        });
        if same_key {
            return Ok(());
        }
        if old.is_some_and(|o| data.expires_at < o.expires_at) {
            return Err(Kick(Box::new(Component::translatable(
                "multiplayer.disconnect.expired_public_key",
                vec![],
            ))));
        }
        if chat.settings.keys.is_empty() {
            log::warn!(
                "Ignoring chat session from {} due to missing Services public key",
                chat.name
            );
            return Ok(());
        }
        match ChatSession::validate(data, chat.uuid, &chat.settings.keys) {
            Ok(session) => {
                let data = session.data.clone();
                self.chat.chain.reset(session);
                self.chat.requests.push(ChatRequest::Session(data));
                Ok(())
            }
            Err(kick) => {
                log::error!("Failed to validate profile key of {}", chat.name);
                Err(kick)
            }
        }
    }
}

/// `multiplayer.disconnect.chat_validation_failed`.
/// A resource pack status as plugins number it (`vanillite_abi::resource_pack_status`).
fn resource_pack_status(action: vanillite_protocol::packets::common::ResourcePackAction) -> u32 {
    use vanillite_abi::resource_pack_status as status;
    use vanillite_protocol::packets::common::ResourcePackAction as A;
    match action {
        A::SuccessfullyLoaded => status::SUCCESSFULLY_LOADED,
        A::Declined => status::DECLINED,
        A::FailedDownload => status::FAILED_DOWNLOAD,
        A::Accepted => status::ACCEPTED,
        A::Downloaded => status::DOWNLOADED,
        A::InvalidUrl => status::INVALID_URL,
        A::FailedReload => status::FAILED_RELOAD,
        A::Discarded => status::DISCARDED,
    }
}

fn chat_validation_failed() -> Kick {
    Kick(Box::new(Component::translatable(
        "multiplayer.disconnect.chat_validation_failed",
        vec![],
    )))
}

/// The bundle sounds of a click (`Entity.playSound` of the player: everyone around but the
/// player, who predicted them).
fn play_bundle_sounds(play: &mut PlayContext<'_>, sounds: Vec<crate::bundle::Sound>) {
    for sound in sounds {
        let (name, volume, random_pitch) = sound.params();
        let pitch = if random_pitch {
            0.8 + play.level.random_float() * 0.4
        } else {
            1.0
        };
        let id = u16::try_from(crate::survival::sound_event(name).0).unwrap_or(0);
        play.level.play_sound(
            Some(play.actor.network_id),
            play.actor.pos,
            id,
            crate::player::world::SoundSource::Players,
            volume,
            pitch,
        );
    }
}

/// `Block.getCloneItemStack` of blocks without an item of their own: stems give their seeds,
/// bamboo shoots bamboo, plant bodies their head's item, cave vines glow berries, tall
/// seagrass seagrass and potted plants the plant.
fn clone_item(block: vanillite_data::block::BlockId) -> u16 {
    let own = block.info().item;
    if own != u16::MAX && own != 0 {
        return own;
    }
    let name = block.name();
    let mapped = match name {
        "minecraft:attached_pumpkin_stem" | "minecraft:pumpkin_stem" => "minecraft:pumpkin_seeds",
        "minecraft:attached_melon_stem" | "minecraft:melon_stem" => "minecraft:melon_seeds",
        "minecraft:bamboo_sapling" => "minecraft:bamboo",
        "minecraft:kelp_plant" => "minecraft:kelp",
        "minecraft:weeping_vines_plant" => "minecraft:weeping_vines",
        "minecraft:twisting_vines_plant" => "minecraft:twisting_vines",
        "minecraft:cave_vines" | "minecraft:cave_vines_plant" => "minecraft:glow_berries",
        "minecraft:tall_seagrass" => "minecraft:seagrass",
        "minecraft:potted_azalea_bush" => "minecraft:azalea",
        "minecraft:potted_flowering_azalea_bush" => "minecraft:flowering_azalea",
        _ => match name.strip_prefix("minecraft:potted_") {
            Some(plant) => {
                return vanillite_data::block::BlockId::by_name(&format!("minecraft:{plant}"))
                    .map_or(u16::MAX, |b| b.info().item);
            }
            None => return own,
        },
    };
    vanillite_data::item::ItemId::by_name(mapped).map_or(u16::MAX, |i| i.0)
}
