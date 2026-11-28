//! The list of online players.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use vanillite_game::player::PacketSink;
use vanillite_game::player::chat::{ChatInbox, ChatMessage};
use vanillite_game::player::join::InfoEntry;
use vanillite_protocol::chat::ChatTypeBound;
use vanillite_protocol::packets::play::clientbound::SystemChat;
use vanillite_protocol::text::Component;
use vanillite_protocol::{Packet, SharedPacket};

use crate::world::WorldId;

/// A player UUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uuid(pub u128);

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            v >> 96,
            (v >> 80) & 0xffff,
            (v >> 64) & 0xffff,
            (v >> 48) & 0xffff,
            v & 0xffff_ffff_ffff
        )
    }
}

/// One online player.
#[derive(Clone)]
pub struct PlayerEntry {
    /// The player's UUID.
    pub uuid: Uuid,
    /// The player's name.
    pub name: String,
    /// The world the player is in.
    pub world: WorldId,
    /// The tab list entry (profile with skin, game mode, latency).
    pub info: InfoEntry,
    /// The player's connection, for messages to all players.
    pub sink: Arc<dyn PacketSink>,
    /// The receiver side of the player's chat.
    pub inbox: Arc<Mutex<ChatInbox>>,
    /// The remote IP address (`getIpAddress`), for IP bans.
    pub ip: String,
    /// The client allows being listed in the server list sample (`allowsListing`).
    pub allows_listing: bool,
    /// Protocol, brand and packet counts of a network connection (`/vanillite debug`).
    pub details: Option<Arc<vanillite_net::ClientDetails>>,
    /// A Bedrock player (a remote virtual connection of the Bedrock frontend).
    pub bedrock: bool,
    /// `requestedDebugSubscriptions`: bit `n` = `debug_subscription` registry id `n`.
    pub debug_subscriptions: u32,
    /// Where the player is, as its world publishes it for the others.
    pub presence: Arc<vanillite_game::presence::Presence>,
}

/// A player's connection with its chat inbox.
pub type Receiver = (Arc<dyn PacketSink>, Arc<Mutex<ChatInbox>>);

fn lock_inbox(inbox: &Mutex<ChatInbox>) -> MutexGuard<'_, ChatInbox> {
    inbox.lock().unwrap_or_else(PoisonError::into_inner)
}

impl fmt::Debug for PlayerEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlayerEntry")
            .field("uuid", &self.uuid)
            .field("name", &self.name)
            .field("world", &self.world)
            .finish_non_exhaustive()
    }
}

/// Online players in join order (Vanilla's order). Cold data behind a mutex: changed on join,
/// quit and world change, read for the server list, the tab list and messages to everyone.
#[derive(Debug)]
pub struct PlayerList {
    max_players: usize,
    players: Mutex<Vec<PlayerEntry>>,
    /// Some player may have requested debug subscriptions (cleared by
    /// [`PlayerList::debug_subscribers`] when none has).
    debug_requested: std::sync::atomic::AtomicBool,
}

impl PlayerList {
    /// An empty list for at most `max_players` players.
    pub fn new(max_players: usize) -> PlayerList {
        PlayerList {
            max_players,
            players: Mutex::new(Vec::new()),
            debug_requested: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<PlayerEntry>> {
        self.players.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The configured maximum.
    pub fn max_players(&self) -> usize {
        self.max_players
    }

    /// Number of online players.
    pub fn count(&self) -> usize {
        self.lock().len()
    }

    /// Whether a player with this UUID is online.
    pub fn contains(&self, uuid: Uuid) -> bool {
        self.lock().iter().any(|p| p.uuid == uuid)
    }

    /// Adds a player. Returns `false` if a player with that UUID is already online.
    pub fn add(&self, entry: PlayerEntry) -> bool {
        let mut players = self.lock();
        if players.iter().any(|p| p.uuid == entry.uuid) {
            return false;
        }
        players.push(entry);
        true
    }

    /// Removes a player.
    pub fn remove(&self, uuid: Uuid) -> Option<PlayerEntry> {
        let mut players = self.lock();
        let index = players.iter().position(|p| p.uuid == uuid)?;
        Some(players.remove(index))
    }

    /// The world of an online player with its published presence.
    pub fn presence(
        &self,
        uuid: Uuid,
    ) -> Option<(WorldId, Arc<vanillite_game::presence::Presence>)> {
        self.lock()
            .iter()
            .find(|p| p.uuid == uuid)
            .map(|p| (p.world, Arc::clone(&p.presence)))
    }

    /// The world an online player is in.
    pub fn world_of(&self, uuid: Uuid) -> Option<WorldId> {
        self.lock().iter().find(|p| p.uuid == uuid).map(|p| p.world)
    }

    /// `requestDebugSubscriptions`: the subscriptions a player asked for (bit mask).
    pub fn set_debug_subscriptions(&self, uuid: Uuid, mask: u32) {
        if let Some(player) = self.lock().iter_mut().find(|p| p.uuid == uuid) {
            player.debug_subscriptions = mask;
        }
        if mask != 0 {
            self.debug_requested
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The UUIDs and connections of the players that requested debug subscription `id`
    /// (without the permission check). Cheap while nobody requested any.
    pub fn debug_subscribers(&self, id: i32) -> Vec<(Uuid, Arc<dyn PacketSink>)> {
        use std::sync::atomic::Ordering;
        if !self.debug_requested.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let players = self.lock();
        if players.iter().all(|p| p.debug_subscriptions == 0) {
            self.debug_requested.store(false, Ordering::Relaxed);
            return Vec::new();
        }
        let bit = u32::try_from(id)
            .ok()
            .and_then(|i| 1u32.checked_shl(i))
            .unwrap_or(0);
        players
            .iter()
            .filter(|p| p.debug_subscriptions & bit != 0)
            .map(|p| (p.uuid, Arc::clone(&p.sink)))
            .collect()
    }

    /// Records that a player moved to another world.
    pub fn set_world(&self, uuid: Uuid, world: WorldId) {
        if let Some(player) = self.lock().iter_mut().find(|p| p.uuid == uuid) {
            player.world = world;
        }
    }

    /// Updates a player's tab list entry.
    pub fn update_info(&self, uuid: Uuid, update: impl FnOnce(&mut InfoEntry)) {
        if let Some(player) = self.lock().iter_mut().find(|p| p.uuid == uuid) {
            update(&mut player.info);
        }
    }

    /// A copy of the list.
    pub fn snapshot(&self) -> Vec<PlayerEntry> {
        self.lock().clone()
    }

    /// The tab list entries of all players, with current latencies.
    pub fn info_entries(&self) -> Vec<InfoEntry> {
        self.lock()
            .iter()
            .map(|p| InfoEntry {
                latency: p.sink.latency_ms(),
                ..p.info.clone()
            })
            .collect()
    }

    /// The connection and chat inbox of an online player.
    pub fn connection(&self, uuid: Uuid) -> Option<Receiver> {
        self.lock()
            .iter()
            .find(|p| p.uuid == uuid)
            .map(|p| (Arc::clone(&p.sink), Arc::clone(&p.inbox)))
    }

    /// `broadcastSystemMessage`: a system message to every player that accepts it, encoded
    /// once.
    pub fn broadcast_system(&self, content: Component, overlay: bool, except: Option<Uuid>) {
        let shared = SharedPacket::of(&SystemChat { content, overlay });
        for (sink, inbox) in self.receivers(except) {
            if lock_inbox(&inbox).accepts_system(overlay) {
                sink.send_shared(&shared);
            }
        }
    }

    /// A system message to the players whose name passes `to` (`broadcastSystemToTeam`,
    /// `broadcastSystemToAllExceptTeam`), encoded once.
    pub fn broadcast_system_to(&self, content: Component, to: impl Fn(&str) -> bool) {
        let shared = SharedPacket::of(&SystemChat {
            content,
            overlay: false,
        });
        let receivers: Vec<Receiver> = self
            .lock()
            .iter()
            .filter(|p| to(&p.name))
            .map(|p| (Arc::clone(&p.sink), Arc::clone(&p.inbox)))
            .collect();
        for (sink, inbox) in receivers {
            if lock_inbox(&inbox).accepts_system(false) {
                sink.send_shared(&shared);
            }
        }
    }

    /// `broadcastChatMessage`: a chat message to every player that accepts chat. Players
    /// with too many unacknowledged messages are kicked.
    pub fn broadcast_chat(
        &self,
        message: &ChatMessage,
        chat_type: &ChatTypeBound,
        max_pending: usize,
    ) {
        for (sink, inbox) in self.receivers(None) {
            let sent = lock_inbox(&inbox).send(message, chat_type, sink.as_ref(), max_pending);
            if !sent {
                sink.kick(Component::translatable(
                    "multiplayer.disconnect.too_many_pending_chats",
                    vec![],
                ));
            }
        }
    }

    /// Connections and inboxes of all players, copied so no lock is held while sending.
    fn receivers(&self, except: Option<Uuid>) -> Vec<Receiver> {
        self.lock()
            .iter()
            .filter(|p| Some(p.uuid) != except)
            .map(|p| (Arc::clone(&p.sink), Arc::clone(&p.inbox)))
            .collect()
    }

    /// Sends encoded packets, in order, to every player that passes `to`.
    pub fn broadcast_all_to(&self, packets: &[Arc<SharedPacket>], to: impl Fn(Uuid) -> bool) {
        if packets.is_empty() {
            return;
        }
        let sinks: Vec<Arc<dyn PacketSink>> = self
            .lock()
            .iter()
            .filter(|p| to(p.uuid))
            .map(|p| Arc::clone(&p.sink))
            .collect();
        for sink in sinks {
            for packet in packets {
                sink.send_shared(packet);
            }
        }
    }

    /// Sends a packet to every player except `except`. The packet is encoded once.
    pub fn broadcast<P: Packet>(&self, packet: &P, except: Option<Uuid>) {
        self.broadcast_shared(&SharedPacket::of(packet), except);
    }

    /// Sends an encoded packet to every player except `except`.
    pub fn broadcast_shared(&self, shared: &Arc<SharedPacket>, except: Option<Uuid>) {
        let sinks: Vec<Arc<dyn PacketSink>> = self
            .lock()
            .iter()
            .filter(|p| Some(p.uuid) != except)
            .map(|p| Arc::clone(&p.sink))
            .collect();
        for sink in sinks {
            sink.send_shared(shared);
        }
    }
}

/// The players one world finds in the other worlds (`getPlayerInAnyDimension`): their
/// presence as published before the world's current tick.
pub(crate) struct WorldElsewhere {
    global: std::sync::Weak<crate::global::Global>,
    world: WorldId,
    tick: std::sync::atomic::AtomicU64,
}

impl WorldElsewhere {
    /// For the world `world`.
    pub(crate) fn new(global: &Arc<crate::global::Global>, world: WorldId) -> WorldElsewhere {
        WorldElsewhere {
            global: Arc::downgrade(global),
            world,
            tick: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl vanillite_game::presence::PlayersElsewhere for WorldElsewhere {
    fn find(&self, uuid: u128) -> Option<vanillite_game::presence::RemotePlayer> {
        let global = self.global.upgrade()?;
        let (world, presence) = global.players.presence(Uuid(uuid))?;
        if world == self.world {
            return None;
        }
        presence.read(self.tick.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn set_tick(&self, tick: u64) {
        self.tick.store(tick, std::sync::atomic::Ordering::Relaxed);
    }
}
