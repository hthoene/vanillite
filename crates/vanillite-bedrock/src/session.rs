//! One Bedrock player: its RakNet peer, the Bedrock login sequence, the Java connection behind
//! it and the translation in both directions.
//!
//! Everything for one player runs on one task, so no state is shared except the read-only data,
//! the section cache, the skins of other players and the form registry.

mod container;
mod dialog;
mod heads;
mod hud;
mod inventory;
mod piston;
mod play;
mod scoreboard;
mod trade;
mod utility;
mod vehicle;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use vanillite_protocol::Packet;
use vanillite_protocol::frame::encode_body;
use vanillite_protocol::packets::common::ClientInfo;
use vanillite_protocol::packets::configuration::serverbound as csb;
use vanillite_protocol::packets::handshake::{Intent, Intention};
use vanillite_protocol::packets::login::{Hello, LoginAcknowledged};
use vanillite_protocol::packets::play::clientbound::{self as pcb, GameMode, relative};
use vanillite_protocol::packets::play::serverbound as psb;
use vanillite_protocol::sets::{ClientboundConfiguration, ClientboundLogin, ClientboundPlay};
use vanillite_protocol::types::{GameProfile, Uuid};

use crate::batch::{self, Codec};
use crate::chunk::{self, BiomeTable, ChunkCache, Layout};
use crate::login::{self, TokenKeys, field};
use crate::num::{angle_deg, f32_of, floor_i32};
use crate::packets::{self as bp, id, input, move_mode, status};
use crate::raknet::RakConn;
use crate::skin::Skin;
use crate::version::Version;
use crate::{BedrockConfig, Connector, Status};

/// Eye height of a standing player; Bedrock positions players at their eyes.
const EYE: f64 = 1.62;

/// State shared by all sessions of one frontend.
pub(crate) struct Shared {
    pub config: BedrockConfig,
    pub connector: Connector,
    pub keys: Arc<TokenKeys>,
    pub cache: ChunkCache,
    /// Skins of connected Bedrock players by Java UUID.
    pub skins: Mutex<FxHashMap<u128, Arc<Skin>>>,
    pub plain: Arc<Skin>,
    pub status: Arc<Status>,
    pub guid: i64,
    pub entity_types: FxHashSet<String>,
    /// Loaded Java skins by texture URL (`None` while loading).
    pub java_skins: Mutex<FxHashMap<String, Option<Arc<Skin>>>>,
    pub forms: Arc<crate::forms::FormRegistry>,
}

/// Messages to a session from other tasks.
#[derive(Debug)]
pub(crate) enum Event {
    /// A form to show (id, JSON).
    Form(u32, String),
    /// A loaded Java skin of a listed player.
    Skin(u128, Arc<Skin>),
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

/// The last Bedrock packet ids of one direction (disconnect logs).
#[derive(Debug, Default)]
struct Recent {
    ids: [u32; 8],
    count: usize,
}

impl Recent {
    fn push(&mut self, id: u32) {
        if let Some(slot) = self.ids.get_mut(self.count % 8) {
            *slot = id;
        }
        self.count = self.count.wrapping_add(1);
    }
}

impl std::fmt::Display for Recent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = self.count.min(8);
        write!(f, "{} packets, last [", self.count)?;
        for i in 0..shown {
            let at = self.count.wrapping_sub(shown).wrapping_add(i) % 8;
            let sep = if i == 0 { "" } else { " " };
            write!(f, "{sep}{}", self.ids.get(at).copied().unwrap_or_default())?;
        }
        f.write_str("]")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    NetworkSettings,
    Login,
    Handshake,
    Packs,
    Joining,
    Playing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JavaState {
    Login,
    Configuration,
    Play,
}

/// What a Java entity is on the Bedrock side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Kind {
    Player,
    /// An item entity (`add_item_actor`).
    Item,
    /// An item entity whose item has not arrived yet (not spawned on Bedrock).
    PendingItem,
    /// A painting whose variant has not arrived yet (not spawned on Bedrock).
    PendingPainting,
    /// A painting (`add_painting`).
    Painting,
    /// An item frame: a frame block on Bedrock at the entity's block.
    Frame,
    /// An entity Bedrock does not show (markers, block and item displays, cushions).
    Hidden,
    #[default]
    Actor,
}

#[derive(Debug, Clone, Default)]
struct Entity {
    pos: [f64; 3],
    yaw: f32,
    pitch: f32,
    head_yaw: f32,
    kind: Kind,
    java_type: u16,
    flags: u8,
    name: Option<String>,
    baby: bool,
    /// A saddle in the `saddle` equipment slot.
    saddled: bool,
    /// Sheep wool (colour and the sheared bit).
    wool: u8,
    /// Rides another entity.
    riding: bool,
    /// A tamed mount whose inventory was opened, and whether it carries a chest.
    tamed: bool,
    chested: bool,
    /// Held by a lead.
    leashed: bool,
    /// The profile id of a player entity.
    uuid: u128,
    /// Block position and Java 3D direction of hanging entities (paintings, item frames).
    block: [i32; 3],
    direction: u8,
}

/// The player's world on the Java side, after the Java `login`.
#[derive(Debug)]
struct World {
    /// Java entity id of the first join: the Bedrock runtime id of the own player.
    entity_id: i32,
    /// Current Java entity id of the own player (changes with a server switch).
    java_id: i32,
    layout: Layout,
    biomes: BiomeTable,
    biome_key: u64,
    game_mode: GameMode,
    hardcore: bool,
    started: bool,
    spawned: bool,
    /// Chunks that arrived before the spawn: the spawn waits for the one at the player, like
    /// the Java client's level loading screen.
    early_chunks: FxHashSet<(i32, i32)>,
    pos: [f64; 3],
    yaw: f32,
    pitch: f32,
    pending_teleport: Option<(i32, [f64; 3])>,
    entities: FxHashMap<i32, Entity>,
    names: FxHashMap<u128, String>,
    listed: FxHashSet<u128>,
    batch_ack_due: bool,
    sneaking: bool,
    sprinting: bool,
    flying: bool,
    abilities: u8,
    walk_speed: f32,
    fly_speed: f32,
    /// Sequence of Java block interactions.
    sequence: i32,
    /// Block being broken.
    breaking: Option<[i32; 3]>,
    dead: bool,
    /// Block cracking of others: Java breaker id -> progress.
    cracking: FxHashMap<i32, u8>,
    /// Last sent `dodaylightcycle`.
    daylight: Option<bool>,
    /// Boss bars: actor, title, health, colour.
    bosses: FxHashMap<u128, (i64, String, f32, u8)>,
    next_boss: u32,
    /// Dimension to switch to at the next position, and whether it is the same dimension
    /// (a server switch).
    pending_dimension: Option<(i32, bool)>,
    /// Passengers by vehicle (Java ids).
    passengers: FxHashMap<i32, Vec<i32>>,
    /// The vehicle the own player rides.
    vehicle: Option<i32>,
    /// The vehicle whose movement the client predicts (it controls it).
    predicted_vehicle: Option<i32>,
    /// Last movement keys sent to Java.
    input: psb::Input,
    /// The open merchant menu.
    merchant: Option<trade::Merchant>,
    /// Item frames by block (Java entity ids): uses and hits of the frame block go to them.
    frames: FxHashMap<[i32; 3], i32>,
    /// Lecterns holding a book: hitting one takes the book, as on Bedrock.
    lecterns: FxHashSet<[i32; 3]>,
}

/// The layout of a Java dimension (`dim_type`: its dimension type name).
fn layout_for(dimension: &str, dim_type: &str) -> Layout {
    let (min_y, height) = match dim_type {
        "minecraft:the_nether" | "minecraft:the_end" => (0, 256),
        _ => (-64, 384),
    };
    Layout::for_dimension(dimension, min_y, height)
}

/// Bedrock runtime and unique id of a Java entity id.
fn rid(java: i32) -> u64 {
    u64::from(u32::from_ne_bytes(java.to_ne_bytes())).saturating_add(1)
}

/// Most block states kept for block entity updates.
const RECENT_BLOCKS: usize = 256;

fn uid(java: i32) -> i64 {
    i64::try_from(rid(java)).unwrap_or(i64::MAX)
}

fn f3(p: [f64; 3]) -> [f32; 3] {
    p.map(f32_of)
}

fn bedrock_game_mode(m: GameMode) -> i32 {
    match m {
        GameMode::Survival => 0,
        GameMode::Creative => 1,
        GameMode::Adventure => 2,
        GameMode::Spectator => 6,
    }
}

/// The position of a lectern block actor that holds a book.
fn lectern_with_book(actor: &[u8]) -> Option<[i32; 3]> {
    let at = actor.windows(7).position(|w| w == b"hasBook")?;
    if actor.get(at.checked_add(7)?) != Some(&1) || !actor.windows(7).any(|w| w == b"Lectern") {
        return None;
    }
    let mut r = crate::io::Reader::new(actor);
    Some([r.vari32().ok()?, r.vari32().ok()?, r.vari32().ok()?])
}

/// English text of Java disconnect keys a Bedrock client cannot translate.
fn reason_text(
    c: &vanillite_protocol::text::Component,
    language: &crate::text::Language,
) -> String {
    let text = crate::text::render(c, language);
    match text.as_str() {
        "multiplayer.disconnect.not_whitelisted" => {
            "You are not whitelisted on this server!".into()
        }
        "multiplayer.disconnect.server_full" => "The server is full!".into(),
        "multiplayer.disconnect.duplicate_login" => "You logged in from another location".into(),
        "multiplayer.disconnect.kicked" => "Kicked by an operator".into(),
        "multiplayer.disconnect.server_shutdown" => "Server closed".into(),
        "multiplayer.disconnect.banned" => "You are banned from this server".into(),
        _ => text,
    }
}

/// The transport of a session.
#[derive(Debug)]
pub(crate) enum Link {
    Rak(Box<RakConn>),
    Net(Box<crate::nethernet::NetConn>),
}

/// One player.
pub(crate) struct Session {
    shared: Arc<Shared>,
    peer: SocketAddr,
    link: Link,
    codec: Codec,
    /// The client's Bedrock version (known after `request_network_settings`).
    pub(super) version: &'static Version,
    phase: Phase,
    batch: Vec<u8>,
    started: Instant,
    closing: Option<Instant>,
    pub done: bool,
    window: (Instant, u32),
    uuid: u128,
    name: String,
    xuid: String,
    language: String,
    skin: Arc<Skin>,
    java_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// A new Java receiver for the task loop to take.
    pub java_rx: Option<mpsc::Receiver<Vec<u8>>>,
    java: JavaState,
    biome_names: Vec<String>,
    /// `minecraft:banner_pattern` entries and tags of the server (the loom's button order).
    banner_patterns: Vec<String>,
    banner_tags: FxHashMap<String, Vec<i32>>,
    dimension_types: Vec<String>,
    /// Java enchantment ids -> Bedrock ids (`minecraft:enchantment` of the server).
    enchantments: crate::items::Enchantments,
    world: Option<World>,
    chunk_radius: i32,
    scratch: Vec<Vec<u8>>,
    inv: inventory::Inventory,
    /// The block or entity the player used last (a menu opens at it).
    used: Option<(container::Used, Instant)>,
    /// The last Bedrock window id of a menu.
    next_window: u8,
    /// The Bedrock recipes of the Java recipe book.
    recipes: crate::recipes::Recipes,
    scoreboard: scoreboard::Scoreboard,
    teams: scoreboard::Teams,
    /// Recent Java states of blocks whose block actor needs their block entity's NBT.
    recent_blocks: FxHashMap<[i32; 3], u32>,
    /// The sign side the server let the client edit.
    editing_sign: Option<([i32; 3], bool)>,
    /// The Java dialog shown as a form.
    dialog: Option<Box<dialog::Shown>>,
    /// The dialog registry: names and (for dialogs outside the known packs) their NBT.
    dialogs: Vec<(String, Option<vanillite_nbt::Tag>)>,
    /// `server_links`: label and URL.
    server_links: Vec<(String, String)>,
    /// Advancements that show a toast when done.
    toasts: hud::Toasts,
    border: hud::Border,
    pistons: piston::Pistons,
    /// Player heads shown as entities with their skins.
    heads: heads::Heads,
    next_head: u64,
    /// The player hit a lectern to take its book; its menu closes at once.
    taking_book: bool,
    /// Soft enums of server-suggested command arguments, asked for one per tick.
    probes: Vec<crate::commands::Probe>,
    next_probe: usize,
    /// The songs jukeboxes play, by position (to stop them).
    records: FxHashMap<[i32; 3], String>,
    /// Send the whole inventory after this packet (a request was refused).
    inventory_resync: bool,
    inventory_open: bool,
    /// Names of the `minecraft:world_clock` registry (ids of `set_time`).
    world_clocks: Vec<String>,
    equipment: play::Equipment,
    /// The Java maps the client was sent, by map id.
    maps: FxHashMap<i32, utility::MapCache>,
    /// The book the client edits.
    book: utility::BookEdit,
    events_tx: mpsc::Sender<Event>,
    /// The event receiver for the task loop to take.
    pub events_rx: Option<mpsc::Receiver<Event>>,
    /// Packet ids sent and received, and client warnings logged (disconnect diagnostics).
    sent_ids: Recent,
    received_ids: Recent,
    warnings_logged: u8,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub(crate) fn new(shared: Arc<Shared>, peer: SocketAddr, link: Link, now: Instant) -> Self {
        let c = &shared.config;
        let codec = Codec::new(
            c.compression_threshold,
            c.compression_level,
            c.max_batch_size,
            c.max_packets_per_batch,
        );
        let chunk_radius = i32::from(c.max_view_distance);
        let plain = Arc::clone(&shared.plain);
        let (events_tx, events_rx) = mpsc::channel(64);
        Self {
            shared,
            peer,
            link,
            codec,
            version: Version::latest(),
            phase: Phase::NetworkSettings,
            batch: Vec::new(),
            started: now,
            closing: None,
            done: false,
            window: (now, 0),
            uuid: 0,
            name: String::new(),
            xuid: String::new(),
            language: "en_us".into(),
            skin: plain,
            java_tx: None,
            java_rx: None,
            java: JavaState::Login,
            biome_names: Vec::new(),
            banner_patterns: Vec::new(),
            banner_tags: FxHashMap::default(),
            dimension_types: Vec::new(),
            enchantments: crate::items::Enchantments::default(),
            world: None,
            chunk_radius,
            scratch: Vec::new(),
            inv: inventory::Inventory::default(),
            used: None,
            next_window: 0,
            recipes: crate::recipes::Recipes::default(),
            scoreboard: scoreboard::Scoreboard::default(),
            teams: scoreboard::Teams::default(),
            recent_blocks: FxHashMap::default(),
            editing_sign: None,
            dialog: None,
            dialogs: Vec::new(),
            server_links: Vec::new(),
            toasts: hud::Toasts::default(),
            border: hud::Border::default(),
            pistons: piston::Pistons::default(),
            records: FxHashMap::default(),
            probes: Vec::new(),
            taking_book: false,
            heads: heads::Heads::default(),
            next_head: 0,
            next_probe: 0,
            inventory_resync: false,
            inventory_open: false,
            world_clocks: Vec::new(),
            equipment: play::Equipment::default(),
            maps: FxHashMap::default(),
            book: utility::BookEdit::default(),
            events_tx,
            events_rx: Some(events_rx),
            sent_ids: Recent::default(),
            received_ids: Recent::default(),
            warnings_logged: 0,
        }
    }

    // ---- Output ---------------------------------------------------------------------------

    fn send(&mut self, packet: u32, body: &[u8]) {
        self.sent_ids.push(packet);
        batch::push_packet(&mut self.batch, packet, body);
    }

    /// Encodes the pending packets as one batch.
    fn flush_batch(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let payload = self.codec.encode(&self.batch);
        self.batch.clear();
        match &mut self.link {
            Link::Rak(rak) => rak.send_game(&payload),
            Link::Net(net) => net.send_game(&payload),
        }
    }

    /// Sends everything due on the socket.
    pub(crate) fn flush(&mut self, socket: &UdpSocket, now: Instant) {
        self.flush_batch();
        let peer = self.peer;
        // A full socket buffer drops the datagram; RakNet and SCTP resend it.
        match &mut self.link {
            Link::Rak(rak) => rak.flush(now, |d| {
                let _ = socket.try_send_to(d, peer);
            }),
            Link::Net(net) => net.flush(now, |to, d| {
                let _ = socket.try_send_to(d, to);
            }),
        }
    }

    /// Disconnects with a message (shown by the client), then closes after a short delay.
    pub(crate) fn kick(&mut self, message: &str) {
        if self.closing.is_some() {
            return;
        }
        if !self.name.is_empty() {
            log::info!("Bedrock player {} disconnected: {message}", self.name);
        } else {
            log::info!(
                "Bedrock connection {} closed during {:?}: {message}",
                self.peer,
                self.phase
            );
        }
        self.send(id::DISCONNECT, &bp::disconnect(message));
        self.flush_batch();
        if let Link::Rak(rak) = &mut self.link {
            rak.disconnect();
        }
        self.java_tx = None;
        self.closing = Some(Instant::now());
    }

    /// Closes without telling the client (it left, timed out or broke the protocol).
    pub(crate) fn close_silently(&mut self, why: &str) {
        if self.phase == Phase::Playing {
            log::info!("Bedrock player {} lost connection: {why}", self.name);
        } else {
            log::info!(
                "Bedrock connection {} lost during {:?}: {why}",
                self.peer,
                self.phase
            );
        }
        let transport = match &mut self.link {
            Link::Rak(rak) => format!("RakNet: {} bytes waiting", rak.backlog()),
            Link::Net(net) => net.describe(Instant::now()),
        };
        log::info!(
            "Bedrock connection {} after {:.1}s: sent {}, received {}; {transport}",
            self.peer,
            self.started.elapsed().as_secs_f32(),
            self.sent_ids,
            self.received_ids
        );
        self.java_tx = None;
        self.done = true;
    }

    fn java_send<P: Packet>(&mut self, packet: &P) {
        self.java_send_raw(encode_body(packet));
    }

    /// Sends a Java packet body (id and payload).
    fn java_send_raw(&mut self, body: Vec<u8>) {
        let Some(tx) = &self.java_tx else {
            return;
        };
        if tx.try_send(body).is_err() {
            // The server does not keep up with this player's input (SEC-17).
            self.kick("Connection to the server lost");
        }
    }

    // ---- Timers -----------------------------------------------------------------------------

    pub(crate) fn tick(&mut self, now: Instant) {
        if let Some(t) = self.closing {
            if now.saturating_duration_since(t) > Duration::from_millis(500) {
                self.done = true;
            }
            return;
        }
        let c = &self.shared.config;
        let last_receive = match &self.link {
            Link::Rak(rak) => rak.last_receive,
            Link::Net(net) => net.last_receive,
        };
        if now.saturating_duration_since(last_receive) > c.idle_timeout {
            self.close_silently("timed out");
            return;
        }
        if let Link::Net(net) = &self.link
            && let Some(why) = net.closed()
        {
            let why = why.to_owned();
            self.close_silently(&why);
            return;
        }
        if self.phase != Phase::Playing
            && now.saturating_duration_since(self.started) > c.login_timeout
        {
            self.kick("Login timed out");
            return;
        }
        let (overloaded, backlog) = match &self.link {
            Link::Rak(rak) => (rak.overloaded(), rak.backlog()),
            Link::Net(net) => (net.overloaded(c.raknet.max_send_queue), net.backlog()),
        };
        if overloaded {
            self.kick("Connection too slow");
            return;
        }
        self.flush_book(now);
        // Chunk flow control: acknowledge a batch once RakNet has sent most of it.
        let backlog_ok = backlog < self.shared.config.chunk_backlog;
        if let Some(w) = &mut self.world
            && w.batch_ack_due
            && backlog_ok
        {
            w.batch_ack_due = false;
            self.java_send(&psb::ChunkBatchReceived {
                desired_chunks_per_tick: 16.0,
            });
        }
    }

    // ---- Bedrock input ------------------------------------------------------------------

    pub(crate) fn datagram(&mut self, from: SocketAddr, data: &[u8], now: Instant) {
        if self.closing.is_some() || self.done {
            return;
        }
        let received = match &mut self.link {
            Link::Rak(rak) => rak
                .receive(data, now)
                .map_err(|e| format!("RakNet error: {e}")),
            Link::Net(net) => net.receive(from, data, now),
        };
        let received = match received {
            Ok(r) => r,
            Err(e) => {
                self.close_silently(&e);
                return;
            }
        };
        if received.disconnected {
            self.close_silently("the client disconnected");
            return;
        }
        for game in received.game {
            let mut packets = std::mem::take(&mut self.scratch);
            packets.clear();
            if let Err(e) = self.codec.decode(game, &mut packets) {
                self.kick(&format!("Bad packet: {e}"));
                return;
            }
            for packet in packets.drain(..) {
                if !self.count_packet(now) {
                    self.kick("Too many packets");
                    return;
                }
                let Ok((pid, body)) = batch::split_packet(&packet) else {
                    self.kick("Bad packet header");
                    return;
                };
                self.received_ids.push(pid);
                if let Err(e) = self.bedrock_packet(pid, body) {
                    self.kick(&format!("Bad packet {pid}: {e}"));
                    return;
                }
                if self.closing.is_some() {
                    return;
                }
            }
            self.scratch = packets;
        }
    }

    fn count_packet(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.window.0) >= Duration::from_secs(1) {
            self.window = (now, 0);
        }
        self.window.1 = self.window.1.saturating_add(1);
        self.window.1 <= self.shared.config.max_packets_per_second
    }

    fn bedrock_packet(&mut self, pid: u32, body: &[u8]) -> crate::io::Result<()> {
        match (self.phase, pid) {
            (_, id::DISCONNECT | id::PACKET_VIOLATION_WARNING) => self.client_complaint(pid, body),
            (Phase::NetworkSettings, id::REQUEST_NETWORK_SETTINGS) => {
                let protocol = crate::io::Reader::new(body).be_i32()?;
                let accepted = self.shared.config.accepts(protocol);
                let Some(version) = Version::of(protocol).filter(|_| accepted) else {
                    log::info!(
                        "Bedrock client {} speaks unsupported protocol {protocol}",
                        self.peer
                    );
                    let (oldest, latest) = self.shared.config.accepted_range();
                    let s = if protocol < oldest.protocol {
                        status::FAILED_CLIENT
                    } else {
                        status::FAILED_SERVER
                    };
                    self.send(id::PLAY_STATUS, &bp::play_status(s));
                    self.kick(&format!(
                        "This server runs Bedrock {} to {}",
                        oldest.name(),
                        latest.last_release()
                    ));
                    return Ok(());
                };
                self.version = version;
                log::debug!("Bedrock client {} speaks protocol {protocol}", self.peer);
                let threshold = self.shared.config.compression_threshold;
                self.send(id::NETWORK_SETTINGS, &bp::network_settings(threshold));
                self.flush_batch();
                self.codec.enable_compression();
                self.phase = Phase::Login;
            }
            (Phase::Login, id::LOGIN) => {
                let mut r = crate::io::Reader::new(body);
                let _protocol = r.be_i32()?;
                let request = r.bytes("login request", self.shared.config.max_batch_size)?;
                self.login(request);
            }
            (Phase::Handshake, id::CLIENT_TO_SERVER_HANDSHAKE) => {
                self.client_to_server_handshake();
            }
            (Phase::Packs, id::RESOURCE_PACK_CLIENT_RESPONSE) => {
                match bp::read_pack_response(body)? {
                    0 => self.kick("Resource packs refused"),
                    1 | 2 => self.send(id::RESOURCE_PACK_STACK, &bp::resource_pack_stack()),
                    3 => self.connect_java(),
                    _ => {}
                }
            }
            (Phase::Joining | Phase::Playing, id::REQUEST_CHUNK_RADIUS) => {
                let requested = bp::read_chunk_radius(body)?;
                let radius = requested.clamp(2, i32::from(self.shared.config.max_view_distance));
                self.chunk_radius = radius;
                self.send(id::CHUNK_RADIUS_UPDATED, &bp::chunk_radius_updated(radius));
                if self.java == JavaState::Play {
                    let info = self.client_info();
                    self.java_send(&psb::ClientInformation(info));
                }
                self.publish_chunks();
            }
            (Phase::Playing, id::SET_LOCAL_PLAYER_AS_INITIALIZED) => {
                self.java_send(&psb::PlayerLoaded);
            }
            (Phase::Playing, id::PLAYER_AUTH_INPUT) => {
                let input = bp::read_auth_input(body, self.version)?;
                let extra = bp::read_auth_input_extra(body, self.version)?;
                self.auth_input(&input, extra.vehicle_rotation);
                if let Some(t) = extra.item_interaction {
                    self.transaction(t);
                }
                if !extra.block_actions.is_empty() {
                    self.block_actions(&extra.block_actions);
                }
                if let Some(req) = extra.stack_request {
                    let resp = self.stack_request(&req);
                    self.send(
                        id::ITEM_STACK_RESPONSE,
                        &bp::item_stack_response(&[resp], self.version),
                    );
                    if std::mem::take(&mut self.inventory_resync) {
                        self.send_inventory();
                    }
                }
            }
            (Phase::Playing, id::ITEM_STACK_REQUEST) => self.stack_requests(body)?,
            (Phase::Playing, id::INVENTORY_TRANSACTION) => {
                let t = bp::read_inventory_transaction(body, self.version)?;
                self.transaction(t);
            }
            (Phase::Playing, id::MOB_EQUIPMENT) => {
                let (hotbar, window) = bp::read_mob_equipment(body)?;
                if window == 0 {
                    self.select_hotbar(hotbar);
                }
            }
            (Phase::Playing, id::INTERACT) => self.interact(body)?,
            (Phase::Playing, id::CONTAINER_CLOSE) => self.container_close(body)?,
            (Phase::Playing, id::PLAYER_ACTION) => self.player_action(body)?,
            (Phase::Playing, id::RESPAWN) => self.bedrock_respawn(body)?,
            (Phase::Playing, id::BLOCK_PICK_REQUEST) => self.pick_block(body)?,
            (Phase::Playing, id::MAP_INFO_REQUEST) => self.map_info_request(body)?,
            (Phase::Playing, id::BOOK_EDIT) => self.book_edit(body, Instant::now())?,
            (Phase::Playing, id::BLOCK_ACTOR_DATA) => self.sign_edited(body)?,
            (Phase::Playing, id::LECTERN_UPDATE) => self.lectern_update(body)?,
            (Phase::Playing, id::PLAYER_TOGGLE_CRAFTER_SLOT_REQUEST) => {
                self.toggle_crafter_slot(body)?;
            }
            (Phase::Playing, id::MODAL_FORM_RESPONSE) => {
                let (form, data) = bp::read_modal_form_response(body)?;
                if form == trade::TRADE_FORM {
                    self.trade_answer(data.as_deref());
                } else if form == dialog::DIALOG_FORM {
                    self.dialog_answer(data.as_deref());
                } else if form == container::MENU_FORM {
                    self.menu_form_answer(data.as_deref());
                } else {
                    self.shared.forms.answer(self.uuid, form, data.as_deref());
                }
            }
            (Phase::Playing, id::TEXT) => {
                let message = bp::read_text_message(body)?;
                let message: String = message
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect();
                if !message.is_empty() {
                    self.java_send(&psb::Chat {
                        message,
                        timestamp: i64::try_from(login::now_secs())
                            .unwrap_or(0)
                            .saturating_mul(1000),
                        salt: 0,
                        signature: None,
                        last_seen: Default::default(),
                    });
                }
            }
            (Phase::Playing, id::COMMAND_REQUEST) => {
                let line = bp::read_command(body)?;
                let command: String = line
                    .strip_prefix('/')
                    .unwrap_or(&line)
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect();
                if !command.is_empty() {
                    self.java_send(&psb::ChatCommand { command });
                }
            }
            (Phase::NetworkSettings | Phase::Login | Phase::Handshake, _) => {
                self.kick(&format!("Unexpected packet {pid} during login"));
            }
            // Everything else (emotes, ...) is not translated.
            _ => {}
        }
        Ok(())
    }

    /// Logs why the client leaves or which packet it could not read (a few per session).
    fn client_complaint(&mut self, pid: u32, body: &[u8]) {
        if self.warnings_logged >= 8 {
            return;
        }
        self.warnings_logged = self.warnings_logged.saturating_add(1);
        let who = if self.name.is_empty() {
            self.peer.to_string()
        } else {
            self.name.clone()
        };
        let mut r = crate::io::Reader::new(body);
        if pid == id::DISCONNECT {
            let reason = r.vari32().unwrap_or(-1);
            let hidden = r.bool().unwrap_or(true);
            let message = if hidden {
                ""
            } else {
                r.string("message", 1024).unwrap_or_default()
            };
            log::info!(
                "Bedrock client {who} disconnects during {:?} (reason {reason}): {message:?}; last sent {}",
                self.phase,
                self.sent_ids
            );
        } else {
            let kind = r.vari32().unwrap_or(-1);
            let severity = r.vari32().unwrap_or(-1);
            let packet = r.vari32().unwrap_or(-1);
            let context = r.string("context", 1024).unwrap_or_default();
            log::warn!(
                "Bedrock client {who} rejected packet {packet} (violation {kind}, severity {severity}): {context:?}; last sent {}",
                self.sent_ids
            );
        }
    }

    fn login(&mut self, request: &[u8]) {
        let online = self.shared.config.online_mode;
        let verified = match login::verify(request, &self.shared.keys, online, login::now_secs()) {
            Ok(v) => v,
            Err(e) => {
                log::info!("Bedrock login from {} refused: {e}", self.peer);
                self.kick(&format!("Login failed: {e}"));
                return;
            }
        };
        let (uuid, name) = login::java_profile(&verified.identity, &self.shared.config.name_prefix);
        self.uuid = uuid;
        self.name = name;
        self.xuid = verified.identity.xuid.clone();
        if let Some(lang) = field(&verified.client_data, "LanguageCode").as_str() {
            self.language = lang.to_ascii_lowercase().chars().take(16).collect();
        }
        if let Some(skin) = Skin::from_client_data(&verified.client_data) {
            self.skin = Arc::new(skin);
        }
        if let Link::Net(net) = &self.link {
            // NetherNet: DTLS encrypts, and the login must be the one of the connection's key.
            if net
                .client_key
                .is_some_and(|k| k != verified.identity.client_key)
            {
                log::info!(
                    "Bedrock login from {} refused: the login key is not the connection's key",
                    self.peer
                );
                self.kick("Login failed: identity mismatch");
                return;
            }
            self.phase = Phase::Handshake;
            self.log_login(&verified.identity.gamertag);
            self.client_to_server_handshake();
            return;
        }
        let handshake = match login::handshake(&verified.identity.client_key) {
            Ok(h) => h,
            Err(e) => {
                self.kick(&e.0);
                return;
            }
        };
        self.send(
            id::SERVER_TO_CLIENT_HANDSHAKE,
            &bp::server_to_client_handshake(&handshake.jwt),
        );
        self.flush_batch();
        self.codec.enable_encryption(handshake.key);
        self.phase = Phase::Handshake;
        self.log_login(&verified.identity.gamertag);
    }

    fn log_login(&self, gamertag: &str) {
        log::info!(
            "Bedrock player {gamertag} (XUID {}) logs in as {} ({}){}",
            if self.xuid.is_empty() {
                "none"
            } else {
                &self.xuid
            },
            self.name,
            Uuid(self.uuid),
            if matches!(self.link, Link::Net(_)) {
                " over NetherNet"
            } else {
                ""
            }
        );
    }

    fn client_to_server_handshake(&mut self) {
        self.send(id::PLAY_STATUS, &bp::play_status(status::LOGIN_SUCCESS));
        self.send(id::RESOURCE_PACKS_INFO, &bp::resource_packs_info());
        self.phase = Phase::Packs;
    }

    fn client_info(&self) -> ClientInfo {
        ClientInfo {
            language: self.language.clone(),
            view_distance: i8::try_from(self.chunk_radius).unwrap_or(i8::MAX),
            ..ClientInfo::default()
        }
    }

    fn connect_java(&mut self) {
        if self.java_tx.is_some() {
            return;
        }
        let profile = GameProfile {
            id: Uuid(self.uuid),
            name: self.name.clone(),
            properties: Vec::new(),
        };
        let conn = (self.shared.connector)(profile, self.peer);
        self.java_tx = Some(conn.to_server);
        self.java_rx = Some(conn.from_server);
        self.java = JavaState::Login;
        self.phase = Phase::Joining;
        if let Ok(mut skins) = self.shared.skins.lock() {
            skins.insert(self.uuid, Arc::clone(&self.skin));
        }
        if let Ok(mut forms) = self.shared.forms.sessions.lock() {
            forms.insert(self.uuid, self.events_tx.clone());
        }
        self.java_send(&Intention {
            protocol_version: vanillite_protocol::PROTOCOL_VERSION,
            host: "bedrock".into(),
            port: self.shared.config.bind.port(),
            intent: Intent::Login,
        });
        self.java_send(&Hello {
            name: self.name.clone(),
            profile_id: Uuid(self.uuid),
        });
    }

    fn auth_input(&mut self, i: &bp::AuthInput, vehicle_rotation: Option<[f32; 2]>) {
        self.draw_border();
        self.piston_tick();
        self.ask_suggestions();
        let Some(w) = &mut self.world else {
            return;
        };
        let eye = i.position.map(f64::from);
        let feet = [eye[0], eye[1] - EYE, eye[2]];
        if let Some((tid, target)) = w.pending_teleport {
            let near = feet.iter().zip(target).all(|(a, b)| (a - b).abs() < 1.0);
            if !(i.has(input::HANDLED_TELEPORT) || near) {
                self.java_send(&psb::ClientTickEnd);
                return;
            }
            w.pending_teleport = None;
            let (yaw, pitch) = (w.yaw, w.pitch);
            self.java_send(&psb::AcceptTeleportation {
                id: tid,
                x: target[0],
                y: target[1],
                z: target[2],
                y_rot: yaw,
                x_rot: pitch,
            });
            let Some(w) = &mut self.world else {
                return;
            };
            w.pos = target;
        }
        let Some(from) = self.world.as_ref().map(|w| w.pos) else {
            return;
        };
        let mut feet = feet;
        if !self.riding()
            && let Some(stop) = self.border_stop(from, feet)
        {
            // The border stops the player: correct the client, send the server the stop.
            feet = stop;
            self.send_own_position(stop);
        }
        let Some(w) = &mut self.world else {
            return;
        };
        w.pos = feet;
        w.yaw = i.yaw;
        w.pitch = i.pitch;
        let on_ground = i.has(input::VERTICAL_COLLISION) && i.delta[1] <= 0.0;
        let mut commands: Vec<psb::PlayerAction> = Vec::new();
        if i.has(input::START_SNEAKING) {
            w.sneaking = true;
        }
        if i.has(input::STOP_SNEAKING) {
            w.sneaking = false;
        }
        if i.has(input::START_SPRINTING) && !w.sprinting {
            w.sprinting = true;
            commands.push(psb::PlayerAction::StartSprinting);
        }
        if i.has(input::STOP_SPRINTING) && w.sprinting {
            w.sprinting = false;
            commands.push(psb::PlayerAction::StopSprinting);
        }
        if i.has(input::START_FLYING) {
            w.flying = true;
        }
        if i.has(input::STOP_FLYING) {
            w.flying = false;
        }
        let jumping = i.has(input::JUMP_DOWN) || i.has(input::START_JUMPING);
        let entity_id = w.java_id;
        let flags = psb::MoveFlags {
            on_ground,
            horizontal_collision: i.has(input::HORIZONTAL_COLLISION),
        };
        let keys = vehicle::keys(i, jumping, w.sneaking, w.sprinting);
        if keys != w.input {
            w.input = keys;
            self.java_send(&psb::PlayerInput(keys));
        }
        for action in commands {
            self.java_send(&psb::PlayerCommand {
                entity_id,
                action,
                data: 0,
            });
        }
        if self.riding() {
            // A passenger only turns (like the Java client); the vehicle moves on its own.
            self.steer_vehicle(i, vehicle_rotation);
            self.java_send(&psb::MovePlayerRot {
                y_rot: i.yaw,
                x_rot: i.pitch,
                flags,
            });
        } else {
            self.java_send(&psb::MovePlayerPosRot {
                x: feet[0],
                y: feet[1],
                z: feet[2],
                y_rot: i.yaw,
                x_rot: i.pitch,
                flags,
            });
        }
        self.java_send(&psb::ClientTickEnd);
    }

    // ---- Java input ---------------------------------------------------------------------

    pub(crate) fn event(&mut self, e: Event) {
        if self.phase != Phase::Playing {
            return;
        }
        match e {
            Event::Form(form, json) => {
                self.send(id::MODAL_FORM_REQUEST, &bp::modal_form_request(form, &json));
            }
            Event::Skin(uuid, skin) => self.apply_skin(uuid, &skin),
        }
    }

    pub(crate) fn java_closed(&mut self) {
        self.java_rx = None;
        if self.java_tx.is_some() {
            self.kick("Disconnected from the server");
        }
    }

    pub(crate) fn java_packet(&mut self, body: &[u8]) {
        if self.closing.is_some() {
            return;
        }
        match self.java {
            JavaState::Login => self.java_login(body),
            JavaState::Configuration => self.java_configuration(body),
            JavaState::Play => self.java_play(body),
        }
    }

    fn java_login(&mut self, body: &[u8]) {
        match ClientboundLogin::decode(body) {
            Ok(ClientboundLogin::LoginFinished(_)) => {
                self.java_send(&LoginAcknowledged);
                self.java = JavaState::Configuration;
                let info = self.client_info();
                self.java_send(&csb::ClientInformation(info));
            }
            Ok(ClientboundLogin::LoginDisconnect(d)) => {
                self.kick(&reason_text(&d.reason.0, &self.shared.config.language))
            }
            Ok(ClientboundLogin::CustomQuery(q)) => {
                self.java_send(&vanillite_protocol::packets::login::CustomQueryAnswer {
                    transaction_id: q.transaction_id,
                    payload: None,
                });
            }
            Ok(ClientboundLogin::CookieRequest(c)) => {
                self.java_send(&vanillite_protocol::packets::login::CookieResponse {
                    key: c.key,
                    payload: None,
                });
            }
            Ok(_) => {}
            Err(e) => self.kick(&format!("Bad server packet: {e}")),
        }
    }

    fn java_configuration(&mut self, body: &[u8]) {
        match ClientboundConfiguration::decode(body) {
            Ok(ClientboundConfiguration::SelectKnownPacks(p)) => {
                self.java_send(&csb::SelectKnownPacks { packs: p.packs });
            }
            Ok(ClientboundConfiguration::KeepAlive(k)) => {
                self.java_send(&csb::KeepAlive { id: k.id })
            }
            Ok(ClientboundConfiguration::Ping(p)) => self.java_send(&csb::Pong { id: p.id }),
            Ok(ClientboundConfiguration::ResourcePackPush(p)) => {
                // Bedrock cannot use Java packs: report it loaded, so a required pack does not
                // kick the player.
                use vanillite_protocol::packets::common::ResourcePackAction as A;
                for action in [A::Accepted, A::SuccessfullyLoaded] {
                    self.java_send(&csb::ResourcePack { id: p.id, action });
                }
            }
            Ok(ClientboundConfiguration::CodeOfConduct(_)) => {
                self.java_send(&csb::AcceptCodeOfConduct)
            }
            Ok(ClientboundConfiguration::CookieRequest(c)) => {
                self.java_send(&csb::CookieResponse {
                    key: c.key,
                    payload: None,
                });
            }
            Ok(ClientboundConfiguration::RegistryData(r)) => {
                let names = r.entries.iter().map(|e| e.id.as_str().to_owned()).collect();
                match r.registry.as_str() {
                    "minecraft:worldgen/biome" => self.biome_names = names,
                    "minecraft:dimension_type" => self.dimension_types = names,
                    "minecraft:enchantment" => {
                        self.enchantments = crate::items::Enchantments::from_names(&names);
                    }
                    "minecraft:world_clock" => self.world_clocks = names,
                    "minecraft:banner_pattern" => self.banner_patterns = names,
                    "minecraft:dialog" => {
                        self.dialogs = r
                            .entries
                            .iter()
                            .take(1024)
                            .map(|e| {
                                let tag = e.data.as_ref().and_then(|d| {
                                    let mut input = d.0.as_slice();
                                    let mut budget = vanillite_nbt::Budget::network();
                                    vanillite_nbt::read::read_network(&mut input, &mut budget)
                                        .ok()
                                        .flatten()
                                });
                                (e.id.as_str().to_owned(), tag)
                            })
                            .collect();
                    }
                    _ => {}
                }
            }
            Ok(ClientboundConfiguration::UpdateTags(t)) => {
                // The loom's pattern lists (`LoomMenu.getSelectablePatterns`).
                if let Some(reg) = t
                    .registries
                    .iter()
                    .find(|r| r.registry.as_str() == "minecraft:banner_pattern")
                {
                    self.banner_tags = reg
                        .tags
                        .iter()
                        .take(256)
                        .map(|tag| (tag.name.as_str().to_owned(), tag.entries.0.clone()))
                        .collect();
                }
            }
            Ok(ClientboundConfiguration::ServerLinks(l)) => self.java_server_links(&l.links),
            Ok(ClientboundConfiguration::FinishConfiguration(_)) => {
                self.java_send(&csb::FinishConfiguration);
                self.java = JavaState::Play;
            }
            Ok(ClientboundConfiguration::Disconnect(d)) => {
                self.kick(&reason_text(&d.reason, &self.shared.config.language))
            }
            Ok(_) => {}
            Err(e) => self.kick(&format!("Bad server packet: {e}")),
        }
    }

    fn java_play(&mut self, body: &[u8]) {
        let packet = match ClientboundPlay::decode(body) {
            Ok(p) => p,
            Err(e) => {
                if !self.java_sound_direct(body) {
                    log::debug!("Undecodable Java packet for {}: {e}", self.name);
                }
                return;
            }
        };
        match packet {
            ClientboundPlay::Login(l) => self.java_join(&l),
            ClientboundPlay::PlayerPosition(p) => self.teleport(&p),
            ClientboundPlay::KeepAlive(k) => self.java_send(&psb::KeepAlive { id: k.id }),
            ClientboundPlay::LevelChunkWithLight(c) => self.chunk(c.x, c.z, &c.data.0),
            ClientboundPlay::ChunkBatchFinished(_) => {
                if let Some(w) = &mut self.world {
                    w.batch_ack_due = true;
                }
                self.spawn_if_ready();
            }
            ClientboundPlay::SetChunkCacheCenter(_) => self.publish_chunks(),
            ClientboundPlay::Disconnect(d) => {
                self.kick(&reason_text(&d.reason, &self.shared.config.language))
            }
            ClientboundPlay::SystemChat(c) => {
                let text = self.text(&c.content);
                if text.is_empty() {
                    // Bedrock rejects an empty text; Java clears the action bar this way.
                } else if c.overlay {
                    self.send(id::TEXT, &bp::text_tip(&text));
                } else {
                    self.send(id::TEXT, &bp::text_raw(&text));
                }
            }
            ClientboundPlay::PlayerChat(c) => {
                let content = c
                    .unsigned_content
                    .as_ref()
                    .map_or_else(|| c.content.clone(), |u| self.text(u));
                let line = format!("<{}§r> {content}", self.text(&c.chat_type.name));
                self.send(id::TEXT, &bp::text_raw(&line));
            }
            ClientboundPlay::DisguisedChat(c) => {
                let line = format!(
                    "<{}§r> {}",
                    self.text(&c.chat_type.name),
                    self.text(&c.message)
                );
                self.send(id::TEXT, &bp::text_raw(&line));
            }
            ClientboundPlay::Commands(t) => {
                let (body, probes) = crate::commands::available_commands(&t.0);
                self.send(id::AVAILABLE_COMMANDS, &body);
                self.probes = probes;
                self.next_probe = 0;
            }
            ClientboundPlay::CommandSuggestions(c) => self.java_suggestions(&c),
            ClientboundPlay::UpdateAdvancements(a) => self.java_advancements(a),
            ClientboundPlay::HurtAnimation(h) => self.java_hurt(h.entity_id),
            ClientboundPlay::GameEvent(e) => self.java_game_event(&e),
            ClientboundPlay::PlayerAbilities(a) => {
                if let Some(w) = &mut self.world {
                    w.abilities = a.flags;
                    w.fly_speed = a.flying_speed;
                    w.walk_speed = a.walking_speed;
                }
                self.send_abilities();
            }
            ClientboundPlay::PlayerInfoUpdate(u) => self.player_info(&u),
            ClientboundPlay::PlayerInfoRemove(r) => {
                let ids: Vec<u128> = r.ids.iter().map(|u| u.0).collect();
                if let Some(w) = &mut self.world {
                    for id in &ids {
                        w.names.remove(id);
                        w.listed.remove(id);
                    }
                }
                self.send(id::PLAYER_LIST, &bp::player_list_remove(&ids));
            }
            ClientboundPlay::AddEntity(e) => self.add_entity(&e),
            ClientboundPlay::RemoveEntities(r) => self.remove_entities(&r.ids.0),
            ClientboundPlay::MoveEntityPos(m) => {
                self.move_entity(m.entity_id, Some(&m.delta), None, m.on_ground);
            }
            ClientboundPlay::MoveEntityPosRot(m) => {
                self.move_entity(
                    m.entity_id,
                    Some(&m.delta),
                    Some((angle_deg(m.y_rot), angle_deg(m.x_rot))),
                    m.on_ground,
                );
            }
            ClientboundPlay::MoveEntityRot(m) => {
                self.move_entity(
                    m.entity_id,
                    None,
                    Some((angle_deg(m.y_rot), angle_deg(m.x_rot))),
                    m.on_ground,
                );
            }
            ClientboundPlay::EntityPositionSync(s) => {
                let target = match &s.path {
                    pcb::PositionPath::Linear(p) => *p,
                    pcb::PositionPath::Stepped(steps) => {
                        steps.last().map(|(p, _)| *p).unwrap_or_default()
                    }
                };
                if let Some(e) = self
                    .world
                    .as_mut()
                    .and_then(|w| w.entities.get_mut(&s.entity_id))
                {
                    e.pos = [target.x, target.y, target.z];
                    e.yaw = s.y_rot;
                    e.pitch = s.x_rot;
                }
                self.send_entity_move(s.entity_id, s.on_ground, true);
            }
            ClientboundPlay::RotateHead(h) => {
                if let Some(e) = self
                    .world
                    .as_mut()
                    .and_then(|w| w.entities.get_mut(&h.entity_id))
                {
                    e.head_yaw = angle_deg(h.head_y_rot);
                }
                self.send_entity_move(h.entity_id, false, false);
            }
            ClientboundPlay::BlockUpdate(b) => {
                self.block_update(
                    [b.pos.x, b.pos.y, b.pos.z],
                    u32::try_from(b.state).unwrap_or(0),
                );
            }
            ClientboundPlay::SectionBlocksUpdate(s) => self.java_section_update(&s),
            ClientboundPlay::BlockDestruction(d) => self.java_block_destruction(&d),
            ClientboundPlay::LevelEvent(e) => self.java_level_event(&e),
            ClientboundPlay::ContainerSetContent(c) => self.java_container_content(c),
            ClientboundPlay::ContainerSetSlot(c) => self.java_container_slot(c),
            ClientboundPlay::SetCursorItem(c) => self.java_cursor(c.item),
            ClientboundPlay::SetPlayerInventory(i) => self.java_player_inventory(i.slot, i.item),
            ClientboundPlay::SetHeldSlot(h) => self.java_held_slot(h.slot),
            ClientboundPlay::SetEquipment(e) => self.java_equipment(&e),
            ClientboundPlay::SetEntityData(d) => self.java_entity_data(&d),
            ClientboundPlay::SetEntityMotion(m) => self.java_motion(&m),
            ClientboundPlay::TakeItemEntity(t) => self.java_take_item(&t),
            ClientboundPlay::SwingAnimation(a) => self.java_swing(&a),
            ClientboundPlay::DamageEvent(d) => self.java_hurt(d.entity_id),
            ClientboundPlay::EntityEvent(e) => self.java_entity_event(&e),
            ClientboundPlay::SetHealth(h) => self.java_health(&h),
            ClientboundPlay::SetExperience(x) => self.java_experience(&x),
            ClientboundPlay::PlayerCombatKill(k) => self.java_combat_kill(&k),
            ClientboundPlay::SetTime(t) => self.java_time(&t),
            ClientboundPlay::ChangeDifficulty(d) => {
                let d = u32::try_from(d.difficulty.clamp(0, 3)).unwrap_or(0);
                self.send(id::SET_DIFFICULTY, &bp::set_difficulty(d));
            }
            ClientboundPlay::Respawn(r) => self.java_respawn(&r),
            ClientboundPlay::SetDefaultSpawnPosition(s) => self.java_spawn_position(&s),
            ClientboundPlay::Sound(s) => self.java_sound(&s),
            ClientboundPlay::UpdateMobEffect(m) => self.java_mob_effect(
                m.entity_id,
                m.effect,
                Some((m.amplifier, m.duration, m.flags)),
            ),
            ClientboundPlay::RemoveMobEffect(m) => {
                self.java_mob_effect(m.entity_id, m.effect, None)
            }
            ClientboundPlay::Cooldown(c) => self.java_cooldown(&c),
            ClientboundPlay::MapItemData(m) => self.java_map_data(m),
            // Bedrock opens written books itself.
            ClientboundPlay::OpenBook(_) => {}
            ClientboundPlay::Unhandled { id } => self.java_unhandled(id, body),
            // Packets the set decodes but this match does not take (`start_configuration`,
            // `teleport_entity`, `sound_entity`, ...) go to the raw handlers by id.
            _ => {
                if let Ok(id) = vanillite_protocol::codec::Reader::new(body).varint() {
                    self.java_unhandled(id, body);
                }
            }
        }
    }

    /// The Bedrock text of a Java component (translated with the server's language table).
    pub(super) fn text(&self, c: &vanillite_protocol::text::Component) -> String {
        crate::text::render(c, &self.shared.config.language)
    }

    fn java_unhandled(&mut self, packet: i32, body: &[u8]) {
        use vanillite_protocol::ids::play::clientbound as ids;
        if packet == ids::START_CONFIGURATION {
            self.java_send(&psb::ConfigurationAcknowledged);
            self.java = JavaState::Configuration;
            return;
        }
        self.java_other(packet, body);
    }

    fn block_update(&mut self, pos: [i32; 3], java_state: u32) {
        if self.piston_block_update(pos, java_state) {
            return;
        }
        if self
            .heads
            .get(&pos)
            .is_some_and(|h| h.state() != java_state)
        {
            self.head_gone(pos);
        }
        let with_book = u16::try_from(java_state).is_ok_and(|s| {
            let s = vanillite_data::block::BlockState(s);
            s.is_valid()
                && s.block().name() == "minecraft:lectern"
                && s.value("has_book") == Some("true")
        });
        if let Some(w) = &mut self.world {
            if with_book {
                w.lecterns.insert(pos);
            } else {
                w.lecterns.remove(&pos);
            }
        }
        let blocks = self.version.palette.blocks();
        let e = blocks.entry(java_state);
        let water = if e & crate::data::WATERLOGGED != 0 {
            blocks.water
        } else {
            blocks.air
        };
        self.send(
            id::UPDATE_BLOCK,
            &bp::update_block(pos, u32::from(e & !crate::data::WATERLOGGED), 0),
        );
        self.send(id::UPDATE_BLOCK, &bp::update_block(pos, water, 1));
        if crate::blockentity::needs_nbt(java_state) {
            if self.recent_blocks.len() >= RECENT_BLOCKS {
                self.recent_blocks.clear();
            }
            self.recent_blocks.insert(pos, java_state);
        }
        if crate::blockentity::from_state(java_state)
            && !self.piston_animating(pos)
            && let Some(nbt) =
                crate::blockentity::translate(pos, java_state, None, self.version.palette)
        {
            self.send(id::BLOCK_ACTOR_DATA, &bp::block_actor_data(pos, &nbt));
        }
    }

    fn java_join(&mut self, l: &pcb::Login) {
        let dim_type = usize::try_from(l.spawn.dimension_type)
            .ok()
            .and_then(|i| self.dimension_types.get(i))
            .cloned()
            .unwrap_or_else(|| l.spawn.dimension.as_str().to_owned());
        let layout = layout_for(l.spawn.dimension.as_str(), &dim_type);
        let biomes = BiomeTable::new(&self.biome_names);
        let biome_key = {
            use std::hash::{Hash, Hasher};
            let mut h = rustc_hash::FxHasher::default();
            biomes.hash(&mut h);
            h.finish()
        };
        if let Some(w) = &mut self.world
            && w.started
        {
            // A second login: a server switch through a proxy.
            w.java_id = l.entity_id;
            w.biomes = biomes;
            w.biome_key = biome_key;
            w.hardcore = l.hardcore;
            let listed: Vec<u128> = w
                .listed
                .iter()
                .copied()
                .filter(|&u| u != self.uuid)
                .collect();
            w.listed.retain(|&u| u == self.uuid);
            w.names.clear();
            self.send(id::PLAYER_LIST, &bp::player_list_remove(&listed));
            self.reset_world(layout, l.spawn.game_mode, true);
            return;
        }
        self.world = Some(World {
            entity_id: l.entity_id,
            java_id: l.entity_id,
            layout,
            biomes,
            biome_key,
            game_mode: l.spawn.game_mode,
            hardcore: l.hardcore,
            started: false,
            spawned: false,
            early_chunks: FxHashSet::default(),
            pos: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
            pending_teleport: None,
            entities: FxHashMap::default(),
            names: FxHashMap::default(),
            listed: FxHashSet::default(),
            batch_ack_due: false,
            sneaking: false,
            sprinting: false,
            flying: false,
            abilities: 0,
            walk_speed: 0.1,
            fly_speed: 0.05,
            sequence: 0,
            breaking: None,
            dead: false,
            cracking: FxHashMap::default(),
            daylight: None,
            bosses: FxHashMap::default(),
            next_boss: 0,
            pending_dimension: None,
            passengers: FxHashMap::default(),
            vehicle: None,
            predicted_vehicle: None,
            input: psb::Input::default(),
            merchant: None,
            frames: FxHashMap::default(),
            lecterns: FxHashSet::default(),
        });
    }

    fn teleport(&mut self, p: &pcb::PlayerPosition) {
        let Some(w) = &mut self.world else {
            return;
        };
        let c = &p.change;
        let rel = |bit: i32| p.relatives & bit != 0;
        let target = [
            if rel(relative::X) {
                w.pos[0] + c.position.x
            } else {
                c.position.x
            },
            if rel(relative::Y) {
                w.pos[1] + c.position.y
            } else {
                c.position.y
            },
            if rel(relative::Z) {
                w.pos[2] + c.position.z
            } else {
                c.position.z
            },
        ];
        w.yaw = if rel(relative::Y_ROT) {
            w.yaw + c.y_rot
        } else {
            c.y_rot
        };
        w.pitch = if rel(relative::X_ROT) {
            w.pitch + c.x_rot
        } else {
            c.x_rot
        };
        if !w.started {
            w.pos = target;
            w.started = true;
            self.start_game();
            let (yaw, pitch) = self.world.as_ref().map_or((0.0, 0.0), |w| (w.yaw, w.pitch));
            self.java_send(&psb::AcceptTeleportation {
                id: p.teleport_id,
                x: target[0],
                y: target[1],
                z: target[2],
                y_rot: yaw,
                x_rot: pitch,
            });
            return;
        }
        w.pending_teleport = Some((p.teleport_id, target));
        let switching = w.pending_dimension.is_some();
        if switching {
            w.pos = target;
            self.finish_dimension_change(target);
            self.publish_chunks();
        }
        self.send_own_position(target);
    }

    fn start_game(&mut self) {
        let Some(w) = &self.world else {
            return;
        };
        let eye = [w.pos[0], w.pos[1] + EYE, w.pos[2]];
        let spawn = [
            floor_i32(w.pos[0]),
            floor_i32(w.pos[1]),
            floor_i32(w.pos[2]),
        ];
        let body = bp::start_game(
            &bp::StartGame {
                entity_id: uid(w.entity_id),
                game_mode: bedrock_game_mode(w.game_mode),
                position: f3(eye),
                yaw: w.yaw,
                pitch: w.pitch,
                dimension: w.layout.dimension,
                difficulty: 2,
                spawn,
                hardcore: w.hardcore,
                world_name: &self.shared.config.world_name,
                permission: 1,
            },
            self.version,
        );
        let runtime = rid(w.entity_id);
        let own = uid(w.entity_id);
        self.send(id::START_GAME, &body);
        self.send(id::ITEM_REGISTRY, self.version.palette.item_registry);
        self.send(id::BIOME_DEFINITION_LIST, crate::data::BIOME_DEFINITIONS);
        self.send(
            id::AVAILABLE_ACTOR_IDENTIFIERS,
            crate::data::ENTITY_IDENTIFIERS,
        );
        self.send(id::CREATIVE_CONTENT, self.version.palette.creative);
        self.send(id::SET_COMMANDS_ENABLED, &bp::set_commands_enabled(true));
        self.send(
            id::UPDATE_ATTRIBUTES,
            &bp::update_movement_speed(runtime, 0.1, 0),
        );
        let radius = self.chunk_radius;
        self.send(id::CHUNK_RADIUS_UPDATED, &bp::chunk_radius_updated(radius));
        self.send_abilities();
        self.publish_chunks();
        // The player's own entry, so its skin shows in third person and the player list.
        let skin = Arc::clone(&self.skin);
        let entry = bp::ListEntry {
            uuid: self.uuid,
            entity_id: own,
            name: &self.name,
            xuid: &self.xuid,
            skin: &skin,
        };
        let body = bp::player_list_add(&[entry], self.version);
        self.send(id::PLAYER_LIST, &body);
        if let Some(w) = &mut self.world {
            w.listed.insert(self.uuid);
        }
        self.phase = Phase::Playing;
    }

    fn spawn_if_ready(&mut self) {
        let Some(w) = &mut self.world else {
            return;
        };
        let own = (floor_i32(w.pos[0]) >> 4, floor_i32(w.pos[2]) >> 4);
        if w.started && !w.spawned && w.early_chunks.contains(&own) {
            w.spawned = true;
            w.early_chunks = FxHashSet::default();
            self.send(id::PLAY_STATUS, &bp::play_status(status::PLAYER_SPAWN));
        }
    }

    fn publish_chunks(&mut self) {
        let Some(w) = &self.world else {
            return;
        };
        if !w.started {
            return;
        }
        let pos = [
            floor_i32(w.pos[0]),
            floor_i32(w.pos[1]),
            floor_i32(w.pos[2]),
        ];
        let radius = u32::try_from(self.chunk_radius.saturating_mul(16)).unwrap_or(0);
        self.send(
            id::NETWORK_CHUNK_PUBLISHER_UPDATE,
            &bp::chunk_publisher_update(pos, radius),
        );
    }

    fn send_abilities(&mut self) {
        let Some(w) = &self.world else {
            return;
        };
        use bp::ability::*;
        let mut values = BUILD | MINE | DOORS | CONTAINERS | ATTACK_PLAYERS | ATTACK_MOBS;
        let flags = w.abilities;
        if flags & 1 != 0 {
            values |= INVULNERABLE;
        }
        if flags & 2 != 0 || w.flying {
            values |= FLYING;
        }
        if flags & 4 != 0 {
            values |= MAY_FLY;
        }
        if flags & 8 != 0 {
            values |= INSTABUILD;
        }
        if w.game_mode == GameMode::Spectator {
            values = (values | NO_CLIP | FLYING | MAY_FLY)
                & !(BUILD | MINE | ATTACK_MOBS | ATTACK_PLAYERS);
        }
        let body = bp::update_abilities(uid(w.entity_id), false, values, w.fly_speed, w.walk_speed);
        self.send(id::UPDATE_ABILITIES, &body);
    }

    fn chunk(&mut self, x: i32, z: i32, data: &[u8]) {
        let Some(w) = &mut self.world else {
            return;
        };
        if !w.spawned {
            w.early_chunks.insert((x, z));
        }
        match chunk::translate(
            self.version,
            x,
            z,
            data,
            w.layout,
            &w.biomes,
            w.biome_key,
            &self.shared.cache,
        ) {
            Ok((body, actors, heads, near_pistons)) => {
                self.send(id::LEVEL_CHUNK, &body);
                self.pistons.remember_chunk(&near_pistons);
                for h in &heads {
                    self.head_found(h.pos, h.state, &h.textures);
                }
                for actor in &actors {
                    if let Some(pos) = lectern_with_book(actor)
                        && let Some(w) = &mut self.world
                    {
                        w.lecterns.insert(pos);
                    }
                    self.send(id::BLOCK_ACTOR_DATA, actor);
                }
            }
            Err(e) => log::debug!("Cannot translate chunk {x} {z} for {}: {e}", self.name),
        }
    }

    fn player_info(&mut self, u: &pcb::PlayerInfoUpdate) {
        use pcb::info_action::ADD_PLAYER;
        if u.actions & ADD_PLAYER == 0 {
            return;
        }
        let skins = self
            .shared
            .skins
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        let mut owned: Vec<(u128, String, Arc<Skin>)> = Vec::new();
        for e in &u.entries {
            let Some(profile) = &e.profile else {
                continue;
            };
            let uuid = e.id.0;
            if let Some(w) = &mut self.world {
                w.names.insert(uuid, profile.name.clone());
                if uuid == self.uuid || !w.listed.insert(uuid) {
                    continue;
                }
            }
            let skin = skins
                .get(&uuid)
                .cloned()
                .unwrap_or_else(|| Arc::clone(&self.shared.plain));
            owned.push((uuid, profile.name.clone(), skin));
        }
        if owned.is_empty() {
            return;
        }
        for e in &u.entries {
            if let Some(profile) = &e.profile
                && !skins.contains_key(&e.id.0)
                && e.id.0 != self.uuid
            {
                self.fetch_java_skin(e.id.0, &profile.properties);
            }
        }
        let entries: Vec<bp::ListEntry<'_>> = owned
            .iter()
            .map(|(uuid, name, skin)| bp::ListEntry {
                uuid: *uuid,
                entity_id: 0,
                name,
                xuid: "",
                skin,
            })
            .collect();
        let body = bp::player_list_add(&entries, self.version);
        self.send(id::PLAYER_LIST, &body);
    }

    fn move_entity(
        &mut self,
        eid: i32,
        delta: Option<&pcb::VecDelta>,
        rot: Option<(f32, f32)>,
        on_ground: bool,
    ) {
        let Some(e) = self
            .world
            .as_mut()
            .and_then(|w| w.entities.get_mut(&eid))
            .filter(|e| e.kind != Kind::PendingItem)
        else {
            return;
        };
        if let Some(d) = delta {
            let steps: Vec<[i16; 3]> = match d {
                pcb::VecDelta::Linear(d) => vec![*d],
                pcb::VecDelta::Stepped(s) => s.iter().map(|(_, d)| *d).collect(),
            };
            for d in steps {
                for (p, v) in e.pos.iter_mut().zip(d) {
                    *p += f64::from(v) / 4096.0;
                }
            }
        }
        if let Some((yaw, pitch)) = rot {
            e.yaw = yaw;
            e.pitch = pitch;
        }
        self.send_entity_move(eid, on_ground, false);
    }

    fn send_entity_move(&mut self, eid: i32, on_ground: bool, teleport: bool) {
        if self.client_moves(eid) {
            return;
        }
        let Some(e) = self
            .world
            .as_ref()
            .and_then(|w| w.entities.get(&eid))
            .cloned()
        else {
            return;
        };
        if e.kind == Kind::Player {
            let eye = [e.pos[0], e.pos[1] + EYE, e.pos[2]];
            let mode = if teleport {
                move_mode::TELEPORT
            } else {
                move_mode::NORMAL
            };
            let body = bp::move_player(
                rid(eid),
                f3(eye),
                e.pitch,
                e.yaw,
                e.head_yaw,
                mode,
                on_ground,
                0,
            );
            self.send(id::MOVE_PLAYER, &body);
        } else {
            let body = bp::move_actor_absolute(
                rid(eid),
                on_ground,
                teleport,
                f3(e.pos),
                [e.pitch, e.yaw, e.head_yaw],
            );
            self.send(id::MOVE_ACTOR_ABSOLUTE, &body);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.uuid != 0
            && let Ok(mut skins) = self.shared.skins.lock()
        {
            skins.remove(&self.uuid);
        }
        if self.uuid != 0 {
            self.shared.forms.leave(self.uuid);
        }
    }
}

/// Runs one peer until it disconnects. `datagrams` carries its UDP payloads.
pub(crate) async fn run(
    shared: Arc<Shared>,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    link: Link,
    mut datagrams: mpsc::Receiver<(SocketAddr, Vec<u8>)>,
) {
    match &link {
        Link::Rak(rak) => log::info!("Bedrock connection from {peer} (MTU {})", rak.mtu()),
        Link::Net(_) => log::info!("Bedrock connection from {peer} (NetherNet)"),
    }
    let mut s = Session::new(shared, peer, link, Instant::now());
    let mut java: Option<mpsc::Receiver<Vec<u8>>> = None;
    let mut events = s.events_rx.take();
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            d = datagrams.recv() => {
                let Some((from, d)) = d else { break };
                let now = Instant::now();
                s.datagram(from, &d, now);
                for _ in 0..64 {
                    match datagrams.try_recv() {
                        Ok((from, d)) => s.datagram(from, &d, now),
                        Err(_) => break,
                    }
                }
            }
            j = async {
                match &mut java {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match j {
                    Some(body) => {
                        s.java_packet(&body);
                        if let Some(rx) = &mut java {
                            for _ in 0..256 {
                                match rx.try_recv() {
                                    Ok(body) => s.java_packet(&body),
                                    Err(_) => break,
                                }
                            }
                        }
                    }
                    None => {
                        java = None;
                        s.java_closed();
                    }
                }
            }
            e = async {
                match &mut events {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => match e {
                Some(e) => s.event(e),
                None => events = None,
            },
            _ = tick.tick() => s.tick(Instant::now()),
        }
        if let Some(rx) = s.java_rx.take() {
            java = Some(rx);
        }
        s.flush(&socket, Instant::now());
        if s.done {
            break;
        }
    }
}
