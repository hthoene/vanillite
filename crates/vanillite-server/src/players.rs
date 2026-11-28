//! Players in a world: joining, their packets, chunk sending, entity updates and leaving. The
//! network layer hands over connections; the handlers themselves live in
//! `vanillite_game::player`.

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use vanillite_game::entities::PERSISTENT_DATA;

use vanillite_base::BlockPos;
use vanillite_base::hash::FxHashSet;
use vanillite_base::slot_map::SlotKey;
use vanillite_base::{ChunkPos, Vec3};
use vanillite_config::{GameMode, WorldRules};
use vanillite_game::advancements::PlayerAdvancements;
use vanillite_game::entities::Entities;
use vanillite_game::inventory::{Drop, PlayerInventory};
use vanillite_game::item::ItemStack;
use vanillite_game::level::Level;
use vanillite_game::mob::fam::wildlife::ender_pearl;
use vanillite_game::player::chat::ChatInbox;
use vanillite_game::player::encode::EQUIPMENT_SLOTS;
use vanillite_game::player::encode::encode_update;
use vanillite_game::player::join::{self, InfoEntry, LevelInfo};
use vanillite_game::player::play::{Actor, PlayContext, throw_item};
use vanillite_game::player::{
    Body, ChatContext, ChunkSource, ChunkView, ClientInformation, Connection, GameProfile, Handled,
    MoveContext, PacketSink, PlayerData, PlayerState, effective_view_distance,
};
use vanillite_game::stats::{Stat, StatType, StatsCounter, custom};
use vanillite_game::survival::player::Survival;
use vanillite_nbt::{List, Tag};
use vanillite_net::{Injector, Latency, Outbound};
use vanillite_protocol::compress::Codec;
use vanillite_protocol::frame::encode_frame;
use vanillite_protocol::packets::play::clientbound::{
    BlockChangedAck, Disconnect, SetEquipment, SetHeldSlot, SwingAnimation, TakeItemEntity,
};
use vanillite_protocol::packets::play::serverbound::Hand;
use vanillite_protocol::tap::{BatchMark, PacketTap, Stage, Verdict, packet_id};
use vanillite_protocol::text::Component;
use vanillite_protocol::{Direction, SharedPacket, State};
use vanillite_storage::player_data::{self, JsonFile, PlayerDataStore};
use vanillite_world::chunks::Chunks;
use vanillite_world::entity::{EntityFlags, EntityId, EntityRef, Pose, Rotation};
use vanillite_world::tracking::{EntityTracker, TrackerOutput};

use crate::blocks::{LevelEffects, WorldLevel};

/// `ServerPlayer.ENDER_PEARLS_TAG`.
const PEARLS_TAG: &str = "ender_pearls";

use crate::compute::Compute;
use crate::global::Global;
use crate::player_list::Uuid;

/// Packets handled per player and tick; the rest waits for the next tick (SEC-16).
const PACKETS_PER_TICK: usize = 128;
/// Ticks between latency updates of the tab list (Vanilla's `PlayerList.tick`).
const LATENCY_UPDATE_TICKS: u64 = 600;

/// Viewers per fan-out job at least: fewer are sent on the world thread.
const FAN_OUT_VIEWERS_PER_JOB: usize = 64;
/// Pairs (message, viewer) per fan-out job at least: fewer are copied on the world thread.
const FAN_OUT_PAIRS_PER_JOB: usize = 64 * 1024;

/// One job's batches and their sizes for a range of viewer slots.
type BatchPart<'a> = (&'a mut [Vec<u8>], &'a mut [usize]);

/// Encode once, one batch per viewer: the tracker's messages are encoded once into frames in
/// an arena (adjacent messages with the same viewers merged into one run), then jobs over
/// ranges of runs with about the same number of (run, viewer) pairs append each run to the
/// batches of its viewers, one batch per job and viewer, in parallel on the compute pool.
/// Each viewer's batches are queued in job order, so it gets its packets in message order.
/// The appends only touch the ends of the viewers' batches, which stay in cache.
#[derive(Debug, Default)]
pub(crate) struct FanOut {
    /// Frames of this tick, built once for the viewers' compression threshold.
    arena: Vec<u8>,
    codec: Codec,
    /// Runs: arena range and the index of the message whose viewers receive it.
    runs: Vec<(u32, u32, u32)>,
    /// Generation and viewer slot by entity index (generation 0 = no viewer): one load per
    /// viewer of a message.
    slot_of: Vec<(u32, u32)>,
    /// Batches per job, by viewer slot.
    batches: Vec<Vec<Vec<u8>>>,
    /// Size of each batch at the last tick, by job and viewer slot: its capacity this tick.
    last_sizes: Vec<Vec<usize>>,
}

impl FanOut {
    fn run(
        &mut self,
        entities: &Entities,
        updates: &TrackerOutput,
        compute: &Compute,
        tap: &PacketTap,
    ) {
        let messages = updates.messages();
        if messages.is_empty() {
            return;
        }
        let threshold = entities
            .players
            .iter()
            .next()
            .and_then(|(_, player)| player.connection.sink.frame_threshold());

        // Viewer slots: the players of this world with their sinks.
        let sinks: Vec<&Arc<dyn PacketSink>> = entities
            .players
            .iter()
            .map(|(_, player)| &player.connection.sink)
            .collect();
        let viewers_n = sinks.len();
        self.slot_of.fill((0, u32::MAX));
        for (slot, (id, _)) in entities.players.iter().enumerate() {
            let index = id.raw().index() as usize;
            if self.slot_of.len() <= index {
                self.slot_of.resize(index + 1, (0, u32::MAX));
            }
            self.slot_of[index] = (
                id.raw().generation(),
                u32::try_from(slot).unwrap_or(u32::MAX),
            );
        }
        let slot_of = &self.slot_of;
        let slot = |viewer: &EntityId| -> usize {
            match slot_of.get(viewer.raw().index() as usize) {
                Some(&(generation, s)) if generation == viewer.raw().generation() => s as usize,
                _ => usize::MAX,
            }
        };

        // Packet listeners: the broadcast stage runs here, once per encoded packet; the
        // viewers' writers run the stage per receiver on their batches.
        let tapped = tap.active(Direction::Clientbound);
        // Whether a writer has to look into the batches for packet types with a listener per
        // receiver: only if this tick's packets hold one (#169).
        let receivers = tapped && tap.listens(Stage::Receiver);
        let mut mark = tap.batch_mark();

        // Encode once into runs.
        self.arena.clear();
        self.runs.clear();
        let mut pairs = 0usize;
        let mut last: Option<&[EntityId]> = None;
        for (index, message) in messages.iter().enumerate() {
            let Some(packets) = encode_update(message, entities) else {
                continue;
            };
            let start = self.arena.len();
            for packet in &packets {
                let replaced;
                let packet = if tapped {
                    match tap.shared(State::Play, packet) {
                        Verdict::Pass => packet,
                        Verdict::Drop => continue,
                        Verdict::Replace(other) => {
                            replaced = other;
                            &replaced
                        }
                    }
                } else {
                    packet
                };
                if receivers
                    && mark != BatchMark::UNKNOWN
                    && packet_id(packet.body())
                        .is_none_or(|id| tap.wants_at(Stage::Receiver, State::Play, id))
                {
                    mark = BatchMark::UNKNOWN;
                }
                let before = self.arena.len();
                if encode_frame(packet.body(), threshold, &mut self.codec, &mut self.arena).is_err()
                {
                    self.arena.truncate(before);
                }
            }
            let end = self.arena.len();
            let (Ok(start32), Ok(end32), Ok(index32)) = (
                u32::try_from(start),
                u32::try_from(end),
                u32::try_from(index),
            ) else {
                break;
            };
            let viewers = updates.viewers_of(message);
            // `PosRot` + `HeadRot` and the like share one viewer list: one copy per viewer.
            if let (Some(prev), Some(run)) = (last, self.runs.last_mut())
                && std::ptr::eq(prev, viewers)
                && run.1 == start32
            {
                run.1 = end32;
                continue;
            }
            self.runs.push((start32, end32, index32));
            pairs += viewers.len();
            last = Some(viewers);
        }
        if pairs == 0 || viewers_n == 0 {
            return;
        }
        let runs = &self.runs;
        let arena = &self.arena;
        let viewers_of = |run: &(u32, u32, u32)| {
            messages
                .get(run.2 as usize)
                .map_or(&[][..], |message| updates.viewers_of(message))
        };

        // Jobs over ranges of runs with about the same number of pairs.
        let workers = compute.phase_threads();
        let jobs = workers.min(pairs.div_ceil(FAN_OUT_PAIRS_PER_JOB)).max(1);
        let per_job = pairs.div_ceil(jobs);
        let mut ranges = Vec::with_capacity(jobs);
        let (mut first, mut acc) = (0, 0);
        for (i, run) in runs.iter().enumerate() {
            acc += viewers_of(run).len();
            if acc >= per_job {
                ranges.push(first..i + 1);
                first = i + 1;
                acc = 0;
            }
        }
        if first < runs.len() {
            ranges.push(first..runs.len());
        }
        let jobs = ranges.len();
        self.batches
            .resize_with(jobs.max(self.batches.len()), Vec::new);
        self.last_sizes
            .resize_with(jobs.max(self.last_sizes.len()), Vec::new);
        for (batches, sizes) in self.batches.iter_mut().zip(&mut self.last_sizes) {
            batches.resize_with(viewers_n, Vec::new);
            sizes.resize(viewers_n, 0);
        }
        let fill = |range: std::ops::Range<usize>, batches: &mut [Vec<u8>], sizes: &[usize]| {
            for run in &runs[range] {
                let bytes = &arena[run.0 as usize..run.1 as usize];
                for viewer in viewers_of(run) {
                    let s = slot(viewer);
                    if let Some(batch) = batches.get_mut(s) {
                        if batch.capacity() == 0 {
                            let hint = sizes.get(s).copied().unwrap_or(0);
                            batch.reserve(hint + hint / 4);
                        }
                        batch.extend_from_slice(bytes);
                    }
                }
            }
        };
        let per_job = self
            .batches
            .iter_mut()
            .zip(&self.last_sizes)
            .zip(ranges.iter().cloned());
        if jobs == 1 {
            for ((batches, sizes), range) in per_job {
                fill(range, batches, sizes);
            }
        } else {
            compute.scope(|scope| {
                let fill = &fill;
                for ((batches, sizes), range) in per_job {
                    scope.spawn(move || fill(range, batches, sizes));
                }
            });
        }

        // Queue each viewer's batches in job order, in parallel over ranges of viewers.
        let send_jobs = workers
            .min(viewers_n.div_ceil(FAN_OUT_VIEWERS_PER_JOB))
            .max(1);
        let per_range = viewers_n.div_ceil(send_jobs);
        let mut parts: Vec<Vec<BatchPart<'_>>> =
            (0..send_jobs).map(|_| Vec::with_capacity(jobs)).collect();
        for (batches, sizes) in self
            .batches
            .iter_mut()
            .zip(self.last_sizes.iter_mut())
            .take(jobs)
        {
            let chunks = batches[..viewers_n]
                .chunks_mut(per_range)
                .zip(sizes[..viewers_n].chunks_mut(per_range));
            for (part, chunk) in parts.iter_mut().zip(chunks) {
                part.push(chunk);
            }
        }
        let send = |first: usize, mut part: Vec<BatchPart<'_>>| {
            let len = part.first().map_or(0, |(batches, _)| batches.len());
            for i in 0..len {
                let Some(sink) = sinks.get(first + i) else {
                    continue;
                };
                for (batches, sizes) in &mut part {
                    let batch = std::mem::take(&mut batches[i]);
                    sizes[i] = batch.len();
                    if !batch.is_empty() {
                        sink.send_batch(batch, threshold, mark);
                    }
                }
            }
        };
        if send_jobs == 1 {
            for part in parts {
                send(0, part);
            }
            return;
        }
        compute.scope(|scope| {
            let send = &send;
            for (index, part) in parts.into_iter().enumerate() {
                scope.spawn(move || send(index * per_range, part));
            }
        });
    }
}

/// A network connection as the game's packet sink.
pub(crate) struct NetSink {
    out: Outbound,
    latency: Latency,
    injector: Injector,
    /// The play `login` packet is queued. The player is in the player list before its world
    /// sends `login` (the world may run on another thread), so broadcasts and plugins can reach
    /// the connection first: those packets are dropped, like Vanilla, which lists a player only
    /// after `login`. The client would fail on them without a level (#199).
    playing: AtomicBool,
}

impl NetSink {
    pub(crate) fn new(out: Outbound, latency: Latency, injector: Injector) -> NetSink {
        NetSink {
            out,
            latency,
            injector,
            playing: AtomicBool::new(false),
        }
    }

    /// Whether a packet may go out now.
    fn passes(&self, body: &[u8]) -> bool {
        passes_gate(self.playing.load(AtomicOrdering::Acquire), body)
    }
}

/// Whether a packet body may go to a client: all once `login` is queued, before it only
/// `disconnect`.
fn passes_gate(playing: bool, body: &[u8]) -> bool {
    playing || packet_id(body) == Some(<Disconnect as vanillite_protocol::Packet>::ID)
}

impl PacketSink for NetSink {
    fn send_body(&self, body: Vec<u8>) -> bool {
        if !self.passes(&body) {
            return !self.out.is_closed();
        }
        self.out.send_body(body)
    }

    fn send_shared(&self, packet: &Arc<SharedPacket>) -> bool {
        if !self.playing.load(AtomicOrdering::Acquire) {
            return !self.out.is_closed();
        }
        self.out.send_shared(packet)
    }

    fn send_batch(&self, frames: Vec<u8>, threshold: Option<usize>, mark: BatchMark) -> bool {
        if !self.playing.load(AtomicOrdering::Acquire) {
            return !self.out.is_closed();
        }
        self.out.send_batch(frames, threshold, mark)
    }

    fn send_untapped(&self, body: Vec<u8>) -> bool {
        if !self.passes(&body) {
            return !self.out.is_closed();
        }
        self.out.send_untapped(body)
    }

    fn send_login(&self, body: Vec<u8>) -> bool {
        // Queued before the flag flips: whoever sees `playing` queues behind `login`.
        let open = self.out.send_body(body);
        self.playing.store(true, AtomicOrdering::Release);
        open
    }

    fn inject(&self, body: Vec<u8>, skip_listeners: bool) -> bool {
        self.injector.inject(body, skip_listeners)
    }

    fn frame_threshold(&self) -> Option<usize> {
        self.out.threshold()
    }

    fn flush(&self) {
        self.out.flush();
    }

    fn close(&self) {
        self.out.close();
    }

    fn is_closed(&self) -> bool {
        self.out.is_closed()
    }

    fn latency_ms(&self) -> u32 {
        self.latency.millis()
    }
}

/// A player on the way into a world.
pub(crate) struct Arrival {
    pub(crate) uuid: u128,
    pub(crate) profile: GameProfile,
    pub(crate) client: ClientInformation,
    pub(crate) game_mode: GameMode,
    pub(crate) sink: Arc<dyn PacketSink>,
    pub(crate) inbound: vanillite_protocol::InboundReceiver,
    pub(crate) memory_connection: bool,
    /// A virtual connection (test bot, Bedrock player): no chat session, chat is unsigned.
    pub(crate) virtual_connection: bool,
    pub(crate) saved: Option<player_data::PlayerData>,
    /// The statistics file.
    pub(crate) stats: Option<String>,
    /// The advancements file.
    pub(crate) advancements: Option<String>,
    pub(crate) address: String,
    pub(crate) inbox: Arc<Mutex<ChatInbox>>,
    /// Transferred from another server: no `server_data` at join.
    pub(crate) transferred: bool,
    /// Protocol, brand and packet counts of a network connection.
    pub(crate) details: Option<Arc<vanillite_net::ClientDetails>>,
}

/// The players' side of a world.
pub(crate) struct WorldPlayers {
    pub(crate) level: LevelInfo,
    store: PlayerDataStore,
    view_distance: i32,
    leaving: Vec<(EntityId, Option<Box<Component>>)>,
    /// Players that left this tick; their tab list entries go after the entity removal.
    left: Vec<u128>,
    /// Players with chat or command requests this tick.
    chatting: Vec<EntityId>,
    /// Dead players that asked to respawn this tick.
    respawns: Vec<EntityId>,
    /// `debug.session-recording` (overworld only).
    pub(crate) recording: crate::session_recording::SessionRecording,
    /// The locator bar.
    waypoints: crate::waypoints::Waypoints,
}

fn chunk_of(pos: Vec3) -> ChunkPos {
    ChunkPos::new(
        vanillite_base::math::floor(pos.x) >> 4,
        vanillite_base::math::floor(pos.z) >> 4,
    )
}

impl WorldPlayers {
    pub(crate) fn new(level: LevelInfo, store: PlayerDataStore) -> WorldPlayers {
        WorldPlayers {
            view_distance: level.view_distance,
            level,
            store,
            leaving: Vec::new(),
            left: Vec::new(),
            chatting: Vec::new(),
            respawns: Vec::new(),
            recording: crate::session_recording::SessionRecording::default(),
            waypoints: crate::waypoints::Waypoints::default(),
        }
    }

    /// The players that asked to respawn since the last call.
    pub(crate) fn take_respawns(&mut self) -> Vec<EntityId> {
        std::mem::take(&mut self.respawns)
    }

    /// The world's view distance in chunks.
    pub(crate) fn view_distance(&self) -> i32 {
        self.view_distance
    }

    /// `placeNewPlayer`: spawns the player and sends the join sequence. The player is already
    /// in the global player list. `fresh_spawn` is where a new player lands.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn join(
        &mut self,
        entities: &mut Entities,
        global: &Global,
        world: crate::world::WorldId,
        arrival: Arrival,
        fresh_spawn: Option<Vec3>,
        weather: &[vanillite_protocol::packets::play::clientbound::GameEvent],
        state: &join::LevelPackets,
    ) {
        let mut survival = arrival
            .saved
            .as_ref()
            .map(|saved| Survival::load(&saved.other))
            .unwrap_or_default();
        // `loadGameTypes` → `updatePlayerAbilities`: creative keeps the saved flying state.
        match arrival.game_mode {
            GameMode::Spectator => survival.flying = true,
            GameMode::Survival | GameMode::Adventure => survival.flying = false,
            GameMode::Creative => {}
        }
        let (pos, rot, on_ground) = match &arrival.saved {
            Some(saved) => (
                Vec3::new(saved.pos[0], saved.pos[1], saved.pos[2]),
                (saved.rotation[0], saved.rotation[1]),
                saved.on_ground,
            ),
            None => (
                fresh_spawn.unwrap_or(self.level.spawn.0),
                (self.level.spawn.1, 0.0),
                true,
            ),
        };
        let center = chunk_of(pos);
        let view_distance = effective_view_distance(&arrival.client, self.view_distance);
        let sink = arrival.sink;
        let name = arrival.profile.name.clone();
        let inventory = arrival
            .saved
            .as_ref()
            .map(|saved| PlayerInventory::load(&saved.other))
            .unwrap_or_default();
        let post_effects = arrival
            .saved
            .as_ref()
            .map(|saved| load_post_effects(&saved.other))
            .unwrap_or_default();
        let mut stats = match arrival.stats.as_deref().map(StatsCounter::from_json) {
            Some(Ok(stats)) => stats,
            Some(Err(e)) => {
                log::error!("Couldn't parse statistics of {name}: {e}");
                StatsCounter::default()
            }
            None => StatsCounter::default(),
        };
        stats.mark_all_dirty();
        let advancements = PlayerAdvancements::load(
            global.systems.advancements(),
            arrival.advancements.as_deref(),
        );
        let recipe_book = match (
            arrival
                .saved
                .as_ref()
                .and_then(|saved| saved.other.get("recipeBook"))
                .and_then(vanillite_nbt::Tag::as_compound),
            vanillite_game::player::play::game_data(),
        ) {
            (Some(c), Some(d)) => {
                vanillite_game::recipe::book::PlayerRecipeBook::load(c, &d.recipes.book)
            }
            _ => vanillite_game::recipe::book::PlayerRecipeBook::default(),
        };
        let saved = arrival.saved.as_ref().map(|saved| &saved.other);
        let previous_game_mode = saved
            .and_then(|d| d.get("previousPlayerGameType"))
            .and_then(Tag::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .and_then(game_mode_by_id);
        let tags = saved.map(load_tags).unwrap_or_default();
        let persistent = saved.and_then(|d| match d.get(PERSISTENT_DATA) {
            Some(Tag::Compound(c)) => Some(c.clone()),
            _ => None,
        });
        let motion = saved.map_or(Vec3::ZERO, load_motion);
        let icon = saved
            .and_then(|d| d.get("locator_bar_icon"))
            .and_then(Tag::as_compound)
            .and_then(crate::command_state::WaypointIcon::from_nbt);
        if let Some(icon) = icon {
            global.systems.waypoints().insert(arrival.uuid, icon);
        }
        let data = PlayerData {
            stats,
            advancements,
            recipe_book,
            inventory,
            last_equipment: std::array::from_fn(|_| ItemStack::EMPTY),
            profile: arrival.profile,
            game_mode: arrival.game_mode,
            previous_game_mode,
            client: arrival.client,
            chunk_view: ChunkView {
                center,
                view_distance,
            },
            connection: Connection::new(
                Arc::clone(&sink),
                arrival.inbound,
                pos,
                arrival.memory_connection,
                arrival.virtual_connection,
                &global.chat,
                Arc::clone(&arrival.inbox),
            ),
            survival,
            post_effects,
        };
        let id = entities.spawn_player(
            &global.network_ids,
            arrival.uuid,
            pos,
            Rotation {
                y_rot: rot.0,
                x_rot: rot.1,
                y_head_rot: rot.0,
            },
            data,
        );
        entities.set_tags(id, tags);
        entities.set_persistent_data(id, persistent);
        let Some(mut entity) = entities.store.get_mut(id) else {
            return;
        };
        entity.set_flags(EntityFlags::ON_GROUND, on_ground);
        entity.set_velocity(motion);
        let network_id = entity.as_ref().network_id().0;
        let mut body = body_of(entities, id);
        let Some(player) = entities.player_mut(id) else {
            return;
        };
        log::info!(
            "{name}[{}] logged in with entity id {network_id} at ({}, {}, {})",
            arrival.address,
            pos.x,
            pos.y,
            pos.z
        );
        let rules = global.level.rules().clone();
        join::send_login(
            sink.as_ref(),
            &self.level,
            network_id,
            player,
            global.chat.enforce_secure,
            &rules,
        );
        sink.send(&crate::survival::change_difficulty(&global.level));
        join::send_abilities(sink.as_ref(), player.game_mode, player.survival.flying);
        let Some(global_arc) = global.arc() else {
            return;
        };
        let source = crate::commands::CommandSource::new(
            global_arc,
            crate::commands::SourceKind::Player {
                uuid: arrival.uuid,
                name: name.clone(),
                sink: Arc::clone(&sink),
            },
            crate::commands::permission_level(global, arrival.uuid),
            world,
            pos,
            rot,
            None,
        );
        crate::commands::send_permissions(global, sink.as_ref(), network_id, &source);
        crate::scoreboard::send_all(global, sink.as_ref(), arrival.uuid);
        // `CustomBossEvents.onPlayerConnect`.
        let mut bars = Vec::new();
        global
            .systems
            .bossbars()
            .player_connected(arrival.uuid, &mut bars);
        for (_, packet) in bars {
            sink.send_shared(&packet);
        }
        // With join listeners the message goes out after them (they may change it).
        if !global.plugins.wants(vanillite_abi::event::PLAYER_JOIN) {
            global.players.broadcast_system(
                join::joined_message(crate::scoreboard::display_name(global, &name, arrival.uuid)),
                false,
                None,
            );
        }
        let connection = &mut player.connection;
        connection
            .movement
            .teleport(&mut body, pos, rot.0, rot.1, sink.as_ref());
        if !arrival.transferred {
            let data = global
                .server_data
                .read()
                .map_or_else(|e| Arc::clone(&e.into_inner()), |d| Arc::clone(&d));
            sink.send_shared(&data);
        }
        // The tab list: everyone to the new player (itself included), the new player to everyone else.
        let entries = global.players.info_entries();
        sink.send(&join::player_info_add(&entries));
        if let Some(own) = entries.iter().find(|e| e.uuid == arrival.uuid) {
            global
                .players
                .broadcast(&join::player_info_add([own]), Some(Uuid(arrival.uuid)));
        }
        join::send_level_info(
            sink.as_ref(),
            &self.level,
            state,
            &crate::survival::full_time_sync(&global.level),
            weather,
        );
        connection
            .chunks
            .update_view(center, view_distance, sink.as_ref());
        // `placeNewPlayer`: the selected slot, then `initInventoryMenu`.
        sink.send(&SetHeldSlot {
            slot: i32::try_from(player.inventory.selected).unwrap_or(0),
        });
        let recipes = Arc::clone(
            &global
                .recipes_packet
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        sink.send_shared(&recipes);
        if let Some(d) = vanillite_game::player::play::game_data() {
            let (settings, book) = player.recipe_book.initial(&d.recipes.book);
            sink.send(&settings);
            sink.send(&book);
        }
        player.inventory.send_all(sink.as_ref());
        for (last, slot) in player.last_equipment.iter_mut().zip(EQUIPMENT_SLOTS) {
            *last = player.inventory.equipment(slot).clone();
        }
        // The first `collectEquipmentChanges`: the equipment's attribute modifiers.
        player.survival.equipment_modifiers = Default::default();
        if let Some(update) =
            vanillite_game::effect::living::player_equipment_update(network_id, player, true)
        {
            sink.send(&update);
        }
        // `sendPostEffects` of the first tick (an empty list changes nothing on the client).
        if !player.post_effects.is_empty() {
            crate::commands::send_post_effects(player);
        }
    }

    /// Handles the queued packets of all players (Vanilla handles them between ticks), applies
    /// their effects to the entities and moves chunk views.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn handle_packets(
        &mut self,
        level: &mut Level,
        rules: &WorldRules,
        runs_normally: bool,
        global: &Global,
        world: crate::world::WorldId,
        blocks: &mut WorldBlocks<'_>,
    ) {
        let (movement_check, elytra_movement_check) = {
            use vanillite_game::survival::rules::Rule;
            let game_rules = global.level.rules();
            (
                game_rules.bool(Rule::PLAYER_MOVEMENT_CHECK),
                game_rules.bool(Rule::ELYTRA_MOVEMENT_CHECK),
            )
        };
        let ctx = MoveContext {
            runs_normally,
            rules: &rules.movement,
            no_wrong_move_check: false,
            no_physics: false,
            dims: vanillite_world::entity::Dimensions {
                width: 0.6,
                height: 1.8,
            },
            blocks: None,
            movement_check,
            elytra_movement_check,
        };
        let protection = self.spawn_protection(global, level);
        let dimension_name = self.level.dimension.clone();
        let dimension_name = dimension_name.as_str();
        // The player kind data leaves the level while the packets are handled, so the block
        // handlers can change the level (`setBlock` with its updates) meanwhile. Nothing in
        // between adds or removes players.
        self.recording.begin_tick(&mut level.entities);
        let mut players = std::mem::take(&mut level.entities.players);
        // The holders of leads (`FenceBlock.useWithoutItem` ties their mobs to fences).
        let lead_holders: FxHashSet<EntityId> = level.entities.leashes.values().copied().collect();
        let spectators: FxHashSet<EntityId> = players
            .iter()
            .filter(|(_, p)| p.game_mode == GameMode::Spectator)
            .map(|(id, _)| id)
            .collect();
        let mut swings: Vec<(EntityId, (Hand, bool))> = Vec::new();
        for (id, player) in players.iter_mut() {
            player.connection.play.fast_break_check = rules.digging.fast_break_check;
            let Some(entity) = level.entities.store.get(id) else {
                continue;
            };
            let chat = ChatContext {
                uuid: entity.uuid(),
                name: &player.profile.name,
                settings: &global.chat,
            };
            let flags = entity.flags();
            let rot = entity.rot();
            let before = PlayerState {
                body: Body {
                    pos: entity.pos(),
                    velocity: entity.velocity(),
                    y_rot: rot.y_rot,
                    x_rot: rot.x_rot,
                    on_ground: flags.contains(EntityFlags::ON_GROUND),
                    horizontal_collision: false,
                    fall_flying: flags.contains(EntityFlags::FALL_FLYING),
                },
                shift_key_down: flags.contains(EntityFlags::CROUCHING),
                sprinting: flags.contains(EntityFlags::SPRINTING),
                client: None,
                respawn: false,
                flying: player.survival.flying,
            };
            let mut state = before.clone();
            let mut actor = actor_of(
                &entity,
                player.game_mode,
                &player.survival.effects,
                &player.inventory,
                global,
                &level.chunks,
            );
            actor.respawn_here = player
                .survival
                .respawn
                .as_ref()
                .filter(|r| r.dimension == dimension_name)
                .map(|r| r.pos);
            actor.leads_mobs = lead_holders.contains(&id);
            let op = actor.permission_level > 0;
            let protected = |pos: BlockPos| protection.protects(pos, op);
            let (aabb, dims) = (entity.aabb(), entity.dims());
            let mut drops: Vec<Drop> = Vec::new();
            let mut hands: Vec<(Hand, bool)> = Vec::new();
            let handled = {
                let mut level = WorldLevel {
                    level: &mut *level,
                    effects: &mut *blocks.effects,
                    spectators: &spectators,
                    actor: Some((id, aabb)),
                    dimension: dimension_name,
                };
                let hooks = crate::plugins::Hooks::of(global, self.level.dimension.as_str());
                let mut play = PlayContext {
                    level: &mut level,
                    inventory: &mut player.inventory,
                    xp: Some(&mut player.survival.xp),
                    random: Some(&mut player.survival.random),
                    food: Some(&mut player.survival.food),
                    recipe_book: Some(&mut player.recipe_book),
                    actor,
                    data: &global.game(),
                    spawn_protected: &protected,
                    drops: &mut drops,
                    swings: &mut hands,
                    hooks: &hooks,
                    stats: Some(&mut player.stats),
                    advancements: Some(&mut player.advancements),
                };
                let handled = player.connection.handle_packets(
                    &mut state,
                    MoveContext {
                        no_wrong_move_check: matches!(
                            player.game_mode,
                            GameMode::Spectator | GameMode::Creative
                        ),
                        no_physics: player.game_mode == GameMode::Spectator,
                        dims,
                        ..ctx
                    },
                    chat,
                    &mut play,
                    PACKETS_PER_TICK,
                );
                actor = play.actor;
                for mut drop in drops {
                    // `LivingEntity.drop`: a stack to drop swings the main hand.
                    if !drop.stack.is_empty() {
                        hands.push((
                            vanillite_protocol::packets::play::serverbound::Hand::MainHand,
                            false,
                        ));
                    }
                    // Bukkit's `PlayerDropItemEvent`: cancelled, the stack goes back into the
                    // inventory (what no longer fits still falls).
                    if drop.thrown
                        && !drop.stack.is_empty()
                        && vanillite_game::player::player_event(
                            &hooks,
                            vanillite_abi::event::PLAYER_DROP_ITEM,
                            actor.uuid,
                            |e| e.item = vanillite_game::hooks::item_snbt(&drop.stack),
                        )
                        .is_some_and(|e| e.cancelled)
                    {
                        let selected = player.inventory.selected;
                        player.inventory.add(Some(selected), &mut drop.stack, false);
                        player.inventory.add(None, &mut drop.stack, false);
                        if drop.stack.is_empty() {
                            continue;
                        }
                    }
                    // `ServerPlayer.drop` of a stack thrown from the hand.
                    if drop.thrown && !drop.stack.is_empty() {
                        let item = drop.stack.item;
                        player
                            .stats
                            .award(Stat::of(StatType::Dropped, item), drop.stack.count);
                        player.stats.award(custom::DROP, 1);
                    }
                    throw_item(&mut level, &actor, drop, &mut player.survival.own_random);
                }
                handled
            };
            self.recording.packets(id, player, &level.entities);
            swings.extend(hands.into_iter().map(|h| (id, h)));
            // `player-idle-timeout` (`ServerGamePacketListenerImpl.tick`; not while the end
            // credits show).
            let idle_minutes = u64::try_from(global.access.idle_timeout()).unwrap_or(0);
            let handled = match handled {
                Handled::Ok
                    if idle_minutes > 0
                        && !player.survival.portal.won_game
                        && player.connection.last_action.elapsed()
                            > std::time::Duration::from_secs(idle_minutes.saturating_mul(60)) =>
                {
                    let reason = vanillite_protocol::text::Component::translatable(
                        "multiplayer.disconnect.idling",
                        vec![],
                    );
                    match crate::plugins::player_kick(
                        &global.plugins,
                        u32::from(world.0),
                        actor.uuid,
                        reason,
                    ) {
                        Some(reason) => Handled::Kick(Box::new(reason)),
                        None => Handled::Ok,
                    }
                }
                other => other,
            };
            match handled {
                Handled::Ok => {}
                Handled::Disconnected => self.leaving.push((id, None)),
                Handled::Kick(reason) => self.leaving.push((id, Some(reason))),
            }
            let Some(mut entity) = level.entities.store.get_mut(id) else {
                continue;
            };
            let body = state.body;
            if body.pos != before.body.pos {
                entity.set_pos(body.pos);
            }
            if body.velocity != before.body.velocity {
                entity.set_velocity(body.velocity);
            }
            if body.y_rot != before.body.y_rot || body.x_rot != before.body.x_rot {
                entity.set_rot(Rotation {
                    y_rot: body.y_rot,
                    x_rot: body.x_rot,
                    y_head_rot: body.y_rot,
                });
            }
            entity.set_flags(EntityFlags::ON_GROUND, body.on_ground);
            entity.set_flags(EntityFlags::SPRINTING, state.sprinting);
            player.survival.flying = state.flying;
            if state.shift_key_down != before.shift_key_down || state.flying != before.flying {
                // `setShiftKeyDown`, and the pose `Player.updatePlayerPose` picks (without the
                // collision test, which needs blocks): flying players do not crouch.
                entity.set_flags(EntityFlags::CROUCHING, state.shift_key_down);
                let pose = if state.shift_key_down && !state.flying {
                    Pose::Crouching
                } else {
                    Pose::Standing
                };
                entity.set_pose(pose);
                entity.set_dims(vanillite_game::player::scaled_dimensions(
                    pose,
                    vanillite_game::effect::attribute_map::scale_of(
                        &player.survival.effects.modifiers,
                    ),
                ));
            }
            if let Some(client) = state.client {
                if client.model_customisation != player.client.model_customisation
                    || client.main_hand != player.client.main_hand
                {
                    entity.mark_data_dirty();
                }
                let parts = client.model_customisation;
                global
                    .players
                    .update_info(Uuid(entity.as_ref().uuid()), |e| {
                        e.model_customisation = parts;
                    });
                player.client = client;
            }
            if state.respawn {
                self.respawns.push(id);
            }
            if !player.connection.chat.requests.is_empty() {
                self.chatting.push(id);
            }
            let view = ChunkView {
                center: chunk_of(body.pos),
                view_distance: effective_view_distance(&player.client, self.view_distance),
            };
            if view != player.chunk_view {
                player.chunk_view = view;
                player.connection.chunks.update_view(
                    view.center,
                    view.view_distance,
                    player.connection.sink.as_ref(),
                );
            }
        }
        level.entities.players = players;
        // The bed and anchor clicks, then `STOP_SLEEPING`.
        {
            let hooks = crate::plugins::Hooks::of(global, dimension_name);
            vanillite_game::survival::sleep::run_block_uses(level, &hooks);
            let leaving: Vec<EntityId> = level
                .entities
                .players
                .iter_mut()
                .filter_map(|(id, p)| std::mem::take(&mut p.connection.leave_bed).then_some(id))
                .collect();
            for id in leaving {
                vanillite_game::survival::sleep::stop_sleep_in_bed(level, id, false, true, &hooks);
            }
        }
        let entities = &mut level.entities;
        // Hand swings go to the players that see the swinging one (`sendToTrackingPlayers`).
        for (id, (hand, attack)) in swings {
            let Some(network_id) = entities.store.get(id).map(|e| e.network_id().0) else {
                continue;
            };
            // `LivingEntity.swing(hand, animation, false)`: only when `startIfAble` lets it.
            let Some((kind, duration)) = entities.player_mut(id).and_then(|p| {
                let animation = if attack {
                    vanillite_game::item::attack_animation(p.inventory.selected_item())
                } else {
                    (1, 6)
                };
                vanillite_game::survival::player::start_swing(&mut p.survival, animation.1)
                    .then_some(animation)
            }) else {
                continue;
            };
            let packet = SharedPacket::of(&SwingAnimation {
                entity_id: network_id,
                hand,
                kind,
                duration,
            });
            for viewer in blocks.tracker.seen_by(id) {
                if let Some(p) = entities.player(*viewer) {
                    p.connection.sink.send_shared(&packet);
                }
            }
        }
        for id in std::mem::take(&mut self.chatting) {
            crate::chat::handle_requests(level, global, id, world, &mut self.leaving);
        }
        for (id, reason) in std::mem::take(&mut self.leaving) {
            self.leave(level, global, id, reason);
        }
        self.recording.end_tick(&level.entities);
    }

    /// Spawn protection of this world for this tick (`isUnderSpawnProtection`).
    fn spawn_protection(&self, global: &Global, level: &Level) -> SpawnProtection {
        let spawn = crate::world::effective_spawn(level, self.level.spawn).0;
        SpawnProtection {
            active: self.level.dimension.as_str() == "minecraft:overworld"
                && global.spawn_protection > 0
                && !global.access.ops().is_empty(),
            x: vanillite_base::math::floor(spawn.x),
            z: vanillite_base::math::floor(spawn.z),
            radius: global.spawn_protection,
        }
    }

    /// The player part of the entity tick (`ServerPlayer.tick`).
    pub(crate) fn tick_entities(&mut self, entities: &mut Entities) {
        for (_, player) in entities.players.iter_mut() {
            player.connection.movement.tick_client_load_timeout();
        }
        crate::spectator::tick(entities);
    }

    /// `ChunkMap.tick`: one chunk batch per player.
    pub(crate) fn send_chunks(entities: &mut Entities, source: &dyn ChunkSource) {
        for (_, player) in entities.players.iter_mut() {
            let connection = &mut player.connection;
            connection
                .chunks
                .send_next_chunks(source, connection.sink.as_ref());
        }
    }

    /// Whether `chunk` is in view of `player` but not sent yet.
    pub(crate) fn chunk_pending(entities: &Entities, player: EntityId, chunk: ChunkPos) -> bool {
        entities
            .player(player)
            .is_some_and(|p| p.connection.chunks.is_pending(chunk))
    }

    /// Sends the tracker's updates: each encoded once, then copied into one batch per viewer
    /// and tick, which is queued as a single entry ([`FanOut`]).
    pub(crate) fn send_entity_updates(
        entities: &Entities,
        updates: &TrackerOutput,
        compute: &Compute,
        fan_out: &mut FanOut,
        tap: &PacketTap,
    ) {
        fan_out.run(entities, updates, compute, tap);
    }

    /// The connection tick (`ServerGamePacketListenerImpl.tick`) after the world tick.
    pub(crate) fn tick_connections(
        &mut self,
        level: &mut Level,
        tick: u64,
        global: &Global,
        world: crate::world::WorldId,
        blocks: &mut WorldBlocks<'_>,
        compute: &Compute,
    ) {
        crate::commands::answer_suggestions(&mut level.entities, global, world);
        for uuid in std::mem::take(&mut self.left) {
            crate::scoreboard::left(global, uuid);
            global.systems.bossbars().player_disconnected(uuid);
            if global.players.remove(Uuid(uuid)).is_some() {
                global
                    .players
                    .broadcast(&join::player_info_remove(uuid), None);
            }
        }
        self.tick_players(level, global, blocks);
        crate::progress::tick(level, global, world, self.level.dimension.as_str(), compute);
        let entities = &mut level.entities;
        let Entities { store, players, .. } = entities;
        for (id, player) in players.iter_mut() {
            if let Some(entity) = store.get(id) {
                let body = Body {
                    pos: entity.pos(),
                    velocity: entity.velocity(),
                    y_rot: entity.rot().y_rot,
                    x_rot: entity.rot().x_rot,
                    on_ground: entity.flags().contains(EntityFlags::ON_GROUND),
                    horizontal_collision: false,
                    fall_flying: entity.flags().contains(EntityFlags::FALL_FLYING),
                };
                player.connection.movement.tick(&body);
            }
            player.connection.chat.chat_spam.tick();
            player.connection.chat.command_spam.tick();
        }
        self.waypoints
            .tick(entities, global, compute, global.plugins.packet_tap());
        // `PlayerList.tick`: latencies for the tab list. The first world sends them for all.
        if tick.is_multiple_of(LATENCY_UPDATE_TICKS)
            && self.level.dimension.as_str() == "minecraft:overworld"
        {
            let entries = global.players.info_entries();
            if !entries.is_empty() {
                let update = vanillite_protocol::packets::play::clientbound::PlayerInfoUpdate {
                    actions:
                        vanillite_protocol::packets::play::clientbound::info_action::UPDATE_LATENCY,
                    ..join::player_info_add(&entries)
                };
                global.players.broadcast(&update, None);
            }
        }
    }

    /// `ServerGamePacketListenerImpl.tick` and `ServerPlayer.doTick` for blocks and items: the
    /// block change acknowledgement, digging progress, item pickup, the inventory menu's
    /// `broadcastChanges` and equipment changes for other players.
    fn tick_players(&mut self, level: &mut Level, global: &Global, blocks: &mut WorldBlocks<'_>) {
        let protection = self.spawn_protection(global, level);
        let dimension_name = self.level.dimension.clone();
        let dimension_name = dimension_name.as_str();
        // As in `handle_packets`: the players leave the level while it may change.
        let mut players = std::mem::take(&mut level.entities.players);
        let spectators: FxHashSet<EntityId> = players
            .iter()
            .filter(|(_, p)| p.game_mode == GameMode::Spectator)
            .map(|(id, _)| id)
            .collect();
        let mut taken: Vec<(EntityId, EntityId, i32)> = Vec::new();
        let mut equipment: Vec<(EntityId, SetEquipment)> = Vec::new();
        let mut attributes = Vec::new();
        let mut taken_orbs: Vec<(EntityId, EntityId)> = Vec::new();
        // Picked up stacks with a thrower: the picker, the thrower's UUID and `getItem()`.
        let mut thrown: Vec<(EntityId, u128, ItemStack)> = Vec::new();
        let mut taken_projectiles: Vec<(EntityId, EntityId)> = Vec::new();
        for (id, player) in players.iter_mut() {
            let sink = &player.connection.sink;
            if let Some(sequence) = player.connection.play.take_ack() {
                sink.send(&BlockChangedAck { sequence });
            }
            let Some(entity) = level.entities.store.get(id) else {
                continue;
            };
            let actor = actor_of(
                &entity,
                player.game_mode,
                &player.survival.effects,
                &player.inventory,
                global,
                &level.chunks,
            );
            let aabb = entity.aabb();
            let network_id = entity.network_id().0;
            let uuid = entity.uuid();
            let op = actor.permission_level > 0;
            let protected = |pos: BlockPos| protection.protects(pos, op);
            {
                let mut level = WorldLevel {
                    level: &mut *level,
                    effects: &mut *blocks.effects,
                    spectators: &spectators,
                    actor: Some((id, aabb)),
                    dimension: dimension_name,
                };
                let mut drops = Vec::new();
                let mut hands = Vec::new();
                let hooks = crate::plugins::Hooks::of(global, self.level.dimension.as_str());
                let mut play = PlayContext {
                    level: &mut level,
                    inventory: &mut player.inventory,
                    xp: Some(&mut player.survival.xp),
                    random: Some(&mut player.survival.random),
                    food: Some(&mut player.survival.food),
                    recipe_book: Some(&mut player.recipe_book),
                    actor,
                    data: &global.game(),
                    spawn_protected: &protected,
                    drops: &mut drops,
                    swings: &mut hands,
                    hooks: &hooks,
                    stats: Some(&mut player.stats),
                    advancements: Some(&mut player.advancements),
                };
                player.connection.play.tick(&mut play, sink.as_ref());
            }
            // `Player.aiStep` → `touch`: item entities in the box grown by (1, 0.5, 1), where
            // the player's server-side travel took it (as its push and the cubes' touch,
            // `player_ai_step_end`).
            // A passenger touches the box spanning it and its vehicle, grown by (1, 0, 1); a
            // dead player touches nothing.
            let step = player.survival.server_step;
            let own = aabb.offset(step.x, step.y, step.z);
            let reach = match level
                .entities
                .vehicle_of(id)
                .and_then(|v| level.entities.store.get(v))
            {
                Some(v) => {
                    let b = v.aabb();
                    vanillite_base::Aabb::new(
                        own.min_x.min(b.min_x),
                        own.min_y.min(b.min_y),
                        own.min_z.min(b.min_z),
                        own.max_x.max(b.max_x),
                        own.max_y.max(b.max_y),
                        own.max_z.max(b.max_z),
                    )
                    .inflate(1.0, 0.0, 1.0)
                }
                None => own.inflate(1.0, 0.5, 1.0),
            };
            if player.game_mode != GameMode::Spectator && player.survival.health > 0.0 {
                let store = &level.entities.store;
                let mut near = Vec::new();
                store.for_each_non_player_in(&reach, |e| {
                    if crate::blocks::is_item(store, e) {
                        near.push(e);
                    }
                });
                let infinite = player.game_mode == GameMode::Creative;
                // `AbstractArrow.playerTouch`: arrows and tridents in the ground.
                let mut projectiles = Vec::new();
                store.for_each_non_player_in(&reach, |e| projectiles.push(e));
                for p in projectiles {
                    if vanillite_game::mob::projectile::try_pickup(
                        &mut level.entities,
                        p,
                        player,
                        id,
                    ) {
                        taken_projectiles.push((p, id));
                    }
                }
                let Entities { store, items, .. } = &mut level.entities;
                for item_id in near {
                    let Some(row) = store.get(item_id).map(|e| e.kind_row()) else {
                        continue;
                    };
                    let Some(item) = items.get_mut(row) else {
                        continue;
                    };
                    let target_ok = item.target.is_none_or(|t| t == uuid);
                    if item.pickup_delay != 0 || !target_ok || item.stack.is_empty() {
                        continue;
                    }
                    // Bukkit's `EntityPickupItemEvent` with what fits: cancelled, the item
                    // stays.
                    if global
                        .plugins
                        .wants(vanillite_abi::event::ENTITY_PICKUP_ITEM)
                    {
                        let hooks = crate::plugins::Hooks::of(global, dimension_name);
                        let room = if infinite {
                            item.stack.count
                        } else {
                            player.inventory.room_for(&item.stack)
                        };
                        if room > 0
                            && let Some(item_uuid) = store.get(item_id).map(|e| e.uuid())
                            && vanillite_game::player::player_event(
                                &hooks,
                                vanillite_abi::event::ENTITY_PICKUP_ITEM,
                                uuid,
                                |e| {
                                    e.other = item_uuid;
                                    e.other_type = "minecraft:item".to_owned();
                                    e.item = vanillite_game::hooks::item_snbt(&item.stack);
                                    e.value = i64::from(room);
                                },
                            )
                            .is_some_and(|e| e.cancelled)
                        {
                            continue;
                        }
                    }
                    let count = item.stack.count;
                    let kind = item.stack.item;
                    let before = item.thrower.map(|t| (t, item.stack.clone()));
                    if player.inventory.add(None, &mut item.stack, infinite) {
                        taken.push((item_id, id, count));
                        if let Some((thrower, mut stack)) = before {
                            // `getItem()` after `take`: the rest, or the whole stack again.
                            if !item.stack.is_empty() {
                                stack = item.stack.clone();
                            }
                            thrown.push((id, thrower, stack));
                        }
                        player
                            .stats
                            .award(Stat::of(StatType::PickedUp, kind), count);
                    }
                }
            }
            // The rest of `touch`: one random experience orb of those the box touches.
            if player.game_mode != GameMode::Spectator && player.survival.health > 0.0 {
                touch_orb(
                    level,
                    id,
                    &mut player.survival,
                    &mut player.inventory,
                    &reach,
                    global,
                    &mut taken_orbs,
                );
            }
            // `Inventory.tick` of maps and lodestone compasses, and the map updates.
            {
                let owner = vanillite_game::carried::Owner {
                    uuid,
                    pos: actor.pos,
                    y_rot: actor.y_rot,
                    dimension: dimension_name,
                    has_ceiling: global
                        .dimensions
                        .world(dimension_name)
                        .and_then(|w| global.dimensions.facts(w))
                        .is_some_and(|t| t.has_ceiling),
                };
                vanillite_game::carried::tick(level, owner, player);
            }
            let sink = &player.connection.sink;
            if player.inventory.container.is_some() {
                // `containerMenu.broadcastChanges`, then `stillValid`.
                use vanillite_game::menu::container as menus;
                menus::with_view(
                    &mut player.inventory,
                    level,
                    actor.pos,
                    Some(&mut player.survival.xp),
                    Some(&mut player.survival.random),
                    |view| {
                        view.flush_result_updates(sink.as_ref());
                        vanillite_game::menu::broadcast_changes(view, sink.as_ref());
                    },
                );
                let in_range = |pos| actor.within_block_range(pos, 4.0);
                let in_entity_range = |b: &vanillite_base::Aabb| actor.within_entity_range(b, 4.0);
                if !menus::still_valid(&player.inventory, level, &in_range, &in_entity_range) {
                    let who = menus::Opener {
                        spectator: player.game_mode == GameMode::Spectator,
                        infinite: player.game_mode == GameMode::Creative,
                        disconnected: false,
                    };
                    let mut drops = Vec::new();
                    menus::close(
                        &mut player.inventory,
                        level,
                        who,
                        &mut drops,
                        Some(sink.as_ref()),
                    );
                    let mut world = WorldLevel {
                        level: &mut *level,
                        effects: &mut *blocks.effects,
                        spectators: &spectators,
                        actor: Some((id, aabb)),
                        dimension: dimension_name,
                    };
                    for drop in drops {
                        throw_item(&mut world, &actor, drop, &mut player.survival.own_random);
                    }
                }
            } else {
                player.inventory.broadcast_changes(sink.as_ref());
            }
            // `detectEquipmentUpdates`.
            let mut changed = Vec::new();
            for (last, slot) in player.last_equipment.iter_mut().zip(EQUIPMENT_SLOTS) {
                let now = player.inventory.equipment(slot);
                if !vanillite_game::item::matches(last, now) {
                    *last = now.clone();
                    changed.push((slot.ordinal(), now.clone()));
                }
            }
            // `collectEquipmentChanges`: the attribute modifiers of the new equipment (and of
            // Frost Walker / Soul Speed) to the client and its trackers.
            if (!changed.is_empty() || player.survival.equipment_dirty)
                && let Some(update) = vanillite_game::effect::living::player_equipment_update(
                    network_id, player, false,
                )
            {
                attributes.push((id, update));
            }
            if !changed.is_empty() {
                equipment.push((
                    id,
                    SetEquipment {
                        entity_id: network_id,
                        slots: changed,
                    },
                ));
            }
        }
        level.entities.players = players;
        let entities = &mut level.entities;
        for (picker, thrower, stack) in thrown {
            vanillite_game::mob::animal::item_picked_up(entities, picker, thrower, &stack);
        }
        for (id, packet) in attributes {
            let packet = SharedPacket::of(&packet);
            if let Some(p) = entities.player(id) {
                p.connection.sink.send_shared(&packet);
            }
            for viewer in blocks.tracker.seen_by(id) {
                if let Some(p) = entities.player(*viewer) {
                    p.connection.sink.send_shared(&packet);
                }
            }
        }
        for (id, packet) in equipment {
            let packet = SharedPacket::of(&packet);
            for viewer in blocks.tracker.seen_by(id) {
                if let Some(p) = entities.player(*viewer) {
                    p.connection.sink.send_shared(&packet);
                }
            }
        }
        for (orb, taker) in taken_orbs {
            // `Player.take(orb, 1)`; the orb goes when its last merged orb was taken.
            let Some(orb_net) = entities.store.get(orb).map(|e| e.network_id().0) else {
                continue;
            };
            let Some(player_net) = entities.store.get(taker).map(|e| e.network_id().0) else {
                continue;
            };
            let packet = SharedPacket::of(&TakeItemEntity {
                item_id: orb_net,
                player_id: player_net,
                amount: 1,
            });
            for viewer in blocks.tracker.seen_by(orb) {
                if let Some(p) = entities.player(*viewer) {
                    p.connection.sink.send_shared(&packet);
                }
            }
            if entities.experience_orb(orb).is_some_and(|d| d.count <= 0) {
                entities.despawn(orb);
            }
        }
        for (projectile, taker) in taken_projectiles {
            // `Player.take(arrow, 1)`, then the arrow is gone.
            let (Some(net), Some(player_net)) = (
                entities.store.get(projectile).map(|e| e.network_id().0),
                entities.store.get(taker).map(|e| e.network_id().0),
            ) else {
                continue;
            };
            let packet = SharedPacket::of(&TakeItemEntity {
                item_id: net,
                player_id: player_net,
                amount: 1,
            });
            for viewer in blocks.tracker.seen_by(projectile) {
                if let Some(p) = entities.player(*viewer) {
                    p.connection.sink.send_shared(&packet);
                }
            }
            entities.despawn(projectile);
        }
        for (item_id, player_id, count) in taken {
            let (Some(item_net), Some(player_net)) = (
                entities.store.get(item_id).map(|e| e.network_id().0),
                entities.store.get(player_id).map(|e| e.network_id().0),
            ) else {
                continue;
            };
            let packet = SharedPacket::of(&TakeItemEntity {
                item_id: item_net,
                player_id: player_net,
                amount: count,
            });
            for viewer in blocks.tracker.seen_by(item_id) {
                if let Some(p) = entities.player(*viewer) {
                    p.connection.sink.send_shared(&packet);
                }
            }
            let empty = entities.item(item_id).is_none_or(|i| i.stack.is_empty());
            if empty {
                entities.despawn(item_id);
            } else if let Some(mut e) = entities.store.get_mut(item_id) {
                e.mark_data_dirty();
            }
        }
    }

    /// The player leaves: kicked with `reason`, or the connection is gone.
    fn leave(
        &mut self,
        level: &mut Level,
        global: &Global,
        id: EntityId,
        reason: Option<Box<Component>>,
    ) {
        // `ServerPlayer.disconnect`: out of bed before the player is saved.
        let hooks = crate::plugins::Hooks::of(global, self.level.dimension.as_str());
        vanillite_game::survival::sleep::stop_sleep_in_bed(level, id, true, false, &hooks);
        let entities = &mut level.entities;
        let Some(entity) = entities.store.get(id) else {
            return;
        };
        let uuid = entity.uuid();
        let Some(player) = entities.player_mut(id) else {
            return;
        };
        player.stats.award(custom::LEAVE_GAME, 1);
        let name = player.profile.name.clone();
        let mut changes = player.stats.take_score_changes();
        player.stats.fire_increases(&mut changes, &hooks, uuid);
        crate::progress::apply_scores(global, vec![(name.clone(), uuid, changes)]);
        match reason {
            Some(reason) => {
                log::info!("{name} lost connection: {}", reason.plain());
                player.connection.sink.kick(*reason);
            }
            None => log::info!("{name} lost connection: Disconnected"),
        }
        self.save_player(level, Some(global), id);
        level.remove_root_vehicle(id);
        self.store_pearls(level, global, uuid);
        if let Some(p) = level.entities.player_mut(id) {
            let mut carried = std::mem::take(&mut p.survival.carried_maps);
            if let Some(maps) = level.maps() {
                for map in carried.drain(..) {
                    maps.with(map, |d| d.carrier_gone(uuid));
                }
            }
        }
        drop_menus_on_leave(level, id);
        let entities = &mut level.entities;
        entities.despawn(id);
        // Vanilla's order: the message, the entity removal (sent by the tracker this tick),
        // then the tab list entry.
        let message = join::left_message(crate::scoreboard::display_name(global, &name, uuid));
        let world = global
            .dimensions
            .world(self.level.dimension.as_str())
            .map_or(0, |w| u32::from(w.0));
        let message = crate::plugins::player_message(global, message, |json| {
            global.plugins.player_leave(world, uuid, &name, json)
        });
        if let Some(message) = message {
            global
                .players
                .broadcast_system(message, false, Some(Uuid(uuid)));
        }
        self.left.push(uuid);
    }

    /// `saveEnderPearls` and the removal of a leaving player's ender pearls: those of this
    /// world go into its data file now, the other worlds add theirs when they get the request.
    fn store_pearls(&self, level: &mut Level, global: &Global, uuid: u128) {
        let dimension = self.level.dimension.as_str().to_owned();
        let pearls = ender_pearl::take_owned(&mut level.entities, uuid, &dimension);
        self.store.update(uuid, move |data| {
            if pearls.is_empty() {
                data.other.remove(PEARLS_TAG);
            } else {
                let list = pearls
                    .into_iter()
                    .map(vanillite_nbt::Tag::Compound)
                    .collect();
                data.other.insert(
                    PEARLS_TAG,
                    vanillite_nbt::Tag::List(vanillite_nbt::List(list)),
                );
            }
        });
        for (world, other, _) in global.dimensions.iter() {
            if other == dimension.as_str() {
                continue;
            }
            global.run_in_world(world, move |world| {
                world.add_pearls_of(uuid);
            });
        }
    }

    /// The ender pearls of a player that left another world: removed here, added to its data
    /// file.
    pub(crate) fn add_pearls(&self, level: &mut Level, uuid: u128) {
        let dimension = self.level.dimension.as_str();
        let pearls = ender_pearl::take_owned(&mut level.entities, uuid, dimension);
        if pearls.is_empty() {
            return;
        }
        self.store.update(uuid, move |data| {
            let mut list = match data.other.remove(PEARLS_TAG) {
                Some(vanillite_nbt::Tag::List(list)) => list.0,
                _ => Vec::new(),
            };
            list.extend(pearls.into_iter().map(vanillite_nbt::Tag::Compound));
            data.other.insert(
                PEARLS_TAG,
                vanillite_nbt::Tag::List(vanillite_nbt::List(list)),
            );
        });
    }

    /// Takes the ender pearls out of a joining player's data (`loadAndSpawnEnderPearls`): they
    /// return to their worlds and live there until the player leaves again.
    pub(crate) fn take_saved_pearls(
        &self,
        uuid: u128,
        saved: Option<&vanillite_nbt::Tag>,
    ) -> Vec<vanillite_nbt::Compound> {
        let Some(vanillite_nbt::Tag::List(list)) = saved else {
            return Vec::new();
        };
        self.store.update(uuid, |data| {
            data.other.remove(PEARLS_TAG);
        });
        list.0
            .iter()
            .filter_map(|t| t.as_compound().cloned())
            .collect()
    }

    /// Queues the write of a player's data file (`PlayerList.save`): what the server keeps of
    /// the player is written from its live state, the fields it does not handle stay as they
    /// are in the file. Without `global` (world saves) the waypoint icon stays as saved.
    fn save_player(&self, level: &Level, global: Option<&Global>, id: EntityId) {
        let entities = &level.entities;
        let (Some(entity), Some(player)) = (entities.store.get(id), entities.player(id)) else {
            return;
        };
        let pos = entity.pos();
        let pos = [pos.x, pos.y, pos.z];
        let rotation = [entity.rot().y_rot, entity.rot().x_rot];
        let flags = entity.flags();
        let on_ground = flags.contains(EntityFlags::ON_GROUND);
        let dimension = self.level.dimension.as_str().to_owned();
        let game_mode = game_mode_id(player.game_mode);
        let uuid = entity.uuid();
        let mut live = vanillite_nbt::Compound::new();
        live.insert(
            "UUID",
            Tag::IntArray(vanillite_nbt::snbt::uuid_to_ints(uuid).to_vec()),
        );
        let motion = entity.velocity();
        live.insert(
            "Motion",
            Tag::List(List(vec![
                Tag::Double(motion.x),
                Tag::Double(motion.y),
                Tag::Double(motion.z),
            ])),
        );
        let tags = entities.tags_of(id);
        if !tags.is_empty() {
            live.insert(
                "Tags",
                Tag::List(List(tags.iter().map(|t| Tag::String(t.clone())).collect())),
            );
        }
        if let Some(data) = entities.persistent_data(id) {
            live.insert(PERSISTENT_DATA, data.clone());
        }
        player.inventory.save(&mut live);
        player.survival.save(&mut live);
        live.insert(
            "post_effects",
            Tag::List(List(
                player
                    .post_effects
                    .iter()
                    .map(|e| Tag::String(e.clone()))
                    .collect(),
            )),
        );
        live.insert("recipeBook", player.recipe_book.save());
        if let Some(previous) = player.previous_game_mode {
            live.insert("previousPlayerGameType", Tag::Int(game_mode_id(previous)));
        }
        // Players do not glide, freeze or keep explosion impulses here yet: those keys carry
        // the state of a player that does none of it.
        live.insert(
            "FallFlying",
            Tag::Byte(i8::from(flags.contains(EntityFlags::FALL_FLYING))),
        );
        live.insert("current_impulse_context_reset_grace_time", Tag::Int(0));
        live.insert("spawn_extra_particles_on_fall", Tag::Byte(0));
        if let Some((attacker, tick)) = player.survival.last_hurt_by_mob
            && let Some(attacker) = entities.store.get(attacker)
        {
            live.insert(
                "last_hurt_by_mob",
                Tag::IntArray(vanillite_nbt::snbt::uuid_to_ints(attacker.uuid()).to_vec()),
            );
            live.insert(
                "ticks_since_last_hurt_by_mob",
                Tag::Int(player.survival.tick_count.wrapping_sub(tick)),
            );
        }
        if let Some(vehicle) = level.save_root_vehicle(id) {
            live.insert("RootVehicle", Tag::Compound(vehicle));
        }
        let icon = global.map(|global| {
            global
                .systems
                .waypoints()
                .get(&uuid)
                .and_then(crate::command_state::WaypointIcon::to_nbt)
        });
        if let Some(Some(icon)) = &icon {
            live.insert("locator_bar_icon", Tag::Compound(icon.clone()));
        }
        let abilities = Abilities {
            mode: player.game_mode,
            flying: player.survival.flying,
        };
        // `PlayerList.save`: the statistics and advancements files with the data.
        self.store
            .save_json(JsonFile::Stats, uuid, player.stats.to_json());
        self.store
            .save_json(JsonFile::Advancements, uuid, player.advancements.to_json());
        let icon_is_live = icon.is_some();
        self.store.update(uuid, move |data| {
            data.pos = pos;
            data.rotation = rotation;
            data.on_ground = on_ground;
            data.dimension = dimension;
            data.game_mode = game_mode;
            for key in LIVE_KEYS {
                data.other.remove(key);
            }
            if icon_is_live {
                data.other.remove("locator_bar_icon");
            }
            let saved = match data.other.remove("abilities") {
                Some(Tag::Compound(c)) => c,
                _ => vanillite_nbt::Compound::new(),
            };
            data.other
                .insert("abilities", Tag::Compound(abilities.save(saved)));
            // The pearls live in their worlds while the player is online.
            data.other.remove(PEARLS_TAG);
            for (key, tag) in live.iter() {
                data.other.insert(key, tag.clone());
            }
        });
    }

    /// Saves all players of this world.
    pub(crate) fn save_all(&self, level: &Level) {
        for (id, _) in level.entities.players.iter() {
            self.save_player(level, None, id);
        }
    }
}

/// The keys of a player data file that only exist for some live states: dropped before the
/// live state is written, so a stale value never stays.
const LIVE_KEYS: [&str; 16] = [
    "Tags",
    PERSISTENT_DATA,
    "previousPlayerGameType",
    "LastDeathLocation",
    "active_effects",
    "respawn",
    "entered_nether_pos",
    "RootVehicle",
    "sleeping_pos",
    "TicksFrozen",
    "current_explosion_impact_pos",
    "last_explosion_impact_pos",
    "last_hurt_by_mob",
    "ticks_since_last_hurt_by_mob",
    "last_hurt_by_player",
    "last_hurt_by_player_memory_time",
];

/// `Abilities` as the game mode sets them (`GameType.updatePlayerAbilities`) with the flying
/// state.
#[derive(Clone, Copy, Debug)]
struct Abilities {
    mode: GameMode,
    flying: bool,
}

impl Abilities {
    /// `Abilities.Packed.CODEC` over the saved compound: the speeds the server does not
    /// change stay as saved.
    fn save(self, out: vanillite_nbt::Compound) -> vanillite_nbt::Compound {
        vanillite_game::mob::entity_nbt::abilities_nbt(self.mode, self.flying, out)
    }
}

/// `Entity.load`'s `Tags`.
fn load_tags(data: &vanillite_nbt::Compound) -> Vec<String> {
    match data.get("Tags") {
        Some(Tag::List(list)) => list
            .0
            .iter()
            .filter_map(|t| t.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// `Entity.load`'s `Motion`: components beyond 10 become 0.
fn load_motion(data: &vanillite_nbt::Compound) -> Vec3 {
    let clamp = |v: f64| {
        if v.is_finite() && v.abs() <= 10.0 {
            v
        } else {
            0.0
        }
    };
    match data.get("Motion") {
        Some(Tag::List(list)) => match list.0.as_slice() {
            [Tag::Double(x), Tag::Double(y), Tag::Double(z)] => {
                Vec3::new(clamp(*x), clamp(*y), clamp(*z))
            }
            _ => Vec3::ZERO,
        },
        _ => Vec3::ZERO,
    }
}

/// `ServerPlayer.readAdditionalSaveData`'s `post_effects`: the valid identifiers.
fn load_post_effects(data: &vanillite_nbt::Compound) -> Vec<String> {
    let Some(vanillite_nbt::Tag::List(list)) = data.get("post_effects") else {
        return Vec::new();
    };
    list.0
        .iter()
        .filter_map(|t| t.as_str())
        .filter_map(|id| vanillite_protocol::types::Identifier::new(id).ok())
        .map(|id| id.to_string())
        .collect()
}

/// What the block and item handling of a tick needs from the world.
pub(crate) struct WorldBlocks<'a> {
    pub(crate) effects: &'a mut LevelEffects,
    pub(crate) tracker: &'a dyn EntityTracker,
}

/// `isUnderSpawnProtection` of one world for one tick.
#[derive(Clone, Copy, Debug)]
struct SpawnProtection {
    /// Overworld, a radius above 0 and at least one operator.
    active: bool,
    x: i32,
    z: i32,
    radius: i32,
}

impl SpawnProtection {
    /// Whether a non-operator may not change the block at `pos`.
    fn protects(self, pos: BlockPos, op: bool) -> bool {
        if !self.active || op {
            return false;
        }
        let dx = pos.x.saturating_sub(self.x).saturating_abs();
        let dz = pos.z.saturating_sub(self.z).saturating_abs();
        dx.max(dz) <= self.radius
    }
}

/// The acting player for the block and item handlers.
fn actor_of(
    entity: &EntityRef<'_>,
    game_mode: GameMode,
    effects: &vanillite_game::effect::ActiveEffects,
    inventory: &vanillite_game::inventory::PlayerInventory,
    global: &Global,
    chunks: &Chunks,
) -> Actor {
    let permission_level = crate::commands::permission_level(global, entity.uuid());
    player_actor(
        entity,
        game_mode,
        effects,
        inventory,
        permission_level,
        chunks,
    )
}

/// The player as the packet handlers see it: eye height of its pose, eyes in water, mining
/// speed effects. The scenario harness builds its packet-level players' actor with it too.
pub fn player_actor(
    entity: &EntityRef<'_>,
    game_mode: GameMode,
    effects: &vanillite_game::effect::ActiveEffects,
    inventory: &vanillite_game::inventory::PlayerInventory,
    permission_level: u8,
    chunks: &Chunks,
) -> Actor {
    let pose = entity.pose();
    // `Player.getDefaultDimensions(pose).eyeHeight()`.
    // Eye heights are floats in Vanilla (`EntityDimensions.eyeHeight`).
    let eye_height = f64::from(match pose {
        Pose::Crouching => 1.27f32,
        Pose::Swimming | Pose::FallFlying | Pose::SpinAttack => 0.4,
        Pose::Sleeping | Pose::Dying => 0.2,
        _ => 1.62,
    });
    let pos = entity.pos();
    // `Entity.updateFluidOnEyes`: the water at `getEyeY() - 0.11111111F` reaches above it
    // (`FluidState.getHeight`: a full block below water).
    let eye_y = pos.y + eye_height - f64::from(0.111_111_11_f32);
    let eye = BlockPos::new(
        vanillite_base::math::floor(pos.x),
        vanillite_base::math::floor(eye_y),
        vanillite_base::math::floor(pos.z),
    );
    let fluid_at = |p: BlockPos| {
        chunks
            .block(p)
            .map_or(vanillite_data::fluid::FluidState::EMPTY, |s| {
                vanillite_data::block::BlockState(s).fluid_state()
            })
    };
    let water =
        |f: vanillite_data::fluid::FluidState| !f.is_empty() && matches!(f.info().fluid, 1 | 2);
    let fluid = fluid_at(eye);
    let eyes_in_water = water(fluid) && {
        let height = if water(fluid_at(BlockPos::new(eye.x, eye.y + 1, eye.z))) {
            1.0
        } else {
            fluid.info().own_height
        };
        f64::from(eye.y) + f64::from(height) > eye_y
    };
    let flags = entity.flags();
    Actor {
        entity: Some(entity.id()),
        network_id: entity.network_id().0,
        uuid: entity.uuid(),
        pos,
        eye_height,
        y_rot: entity.rot().y_rot,
        x_rot: entity.rot().x_rot,
        on_ground: flags.contains(EntityFlags::ON_GROUND),
        sneaking: flags.contains(EntityFlags::CROUCHING),
        game_mode,
        permission_level,
        eyes_in_water,
        dig_speed: effects
            .has_dig_speed()
            .then(|| effects.dig_speed_amplification()),
        mining_fatigue: effects.amplifier(vanillite_game::effect::MobEffect::MINING_FATIGUE),
        respawn_here: None,
        leads_mobs: false,
        ranges: vanillite_game::player::play::interaction_ranges(inventory, game_mode),
    }
}

fn body_of(entities: &Entities, id: EntityId) -> Body {
    match entities.store.get(id) {
        Some(e) => Body {
            pos: e.pos(),
            velocity: e.velocity(),
            y_rot: e.rot().y_rot,
            x_rot: e.rot().x_rot,
            on_ground: e.flags().contains(EntityFlags::ON_GROUND),
            horizontal_collision: false,
            fall_flying: false,
        },
        None => Body {
            pos: Vec3::ZERO,
            velocity: Vec3::ZERO,
            y_rot: 0.0,
            x_rot: 0.0,
            on_ground: false,
            horizontal_collision: false,
            fall_flying: false,
        },
    }
}

/// `GameType.byId` without a fallback.
pub(crate) fn game_mode_by_id(id: i32) -> Option<GameMode> {
    match id {
        0 => Some(GameMode::Survival),
        1 => Some(GameMode::Creative),
        2 => Some(GameMode::Adventure),
        3 => Some(GameMode::Spectator),
        _ => None,
    }
}

/// `GameType.getId`.
pub(crate) fn game_mode_id(mode: GameMode) -> i32 {
    match mode {
        GameMode::Survival => 0,
        GameMode::Creative => 1,
        GameMode::Adventure => 2,
        GameMode::Spectator => 3,
    }
}

/// The tab list entry of a joining player.
pub(crate) fn info_entry(arrival: &Arrival) -> InfoEntry {
    InfoEntry {
        uuid: arrival.uuid,
        profile: arrival.profile.clone(),
        game_mode: arrival.game_mode,
        latency: arrival.sink.latency_ms(),
        model_customisation: arrival.client.model_customisation,
        chat_session: None,
    }
}

/// `Player.remove` for a player that leaves or a dead player that respawns: open menus close,
/// the carried stack and the crafting grid drop (`player.drop(stack, false)`).
pub(crate) fn drop_menus_on_leave(level: &mut Level, id: EntityId) {
    let Some((pos, rot, height)) = level
        .entities
        .store
        .get(id)
        .map(|e| (e.pos(), e.rot(), e.dims().height))
    else {
        return;
    };
    let Some(player) = level.entities.player_mut(id) else {
        return;
    };
    let spectator = player.game_mode == GameMode::Spectator;
    let mut inv = std::mem::take(&mut player.inventory);
    let mut drops = Vec::new();
    vanillite_game::menu::container::close_on_leave(&mut inv, level, spectator, &mut drops);
    let Some(player) = level.entities.player_mut(id) else {
        return;
    };
    player.inventory = inv;
    let eye = vanillite_game::survival::player::eye_height(height);
    // The entity's random, which the death drops (`Inventory.dropAll`) drew from before.
    let mut state = player.survival.random.state();
    let throws: Vec<_> = drops
        .into_iter()
        .map(|drop| {
            let (from, velocity) = vanillite_game::player::play::throw_motion(
                pos, eye, rot.y_rot, rot.x_rot, &mut state,
            );
            (from, velocity, drop.stack)
        })
        .collect();
    player.survival.random = vanillite_base::random::LegacyRandom::from_state(state);
    for (from, velocity, stack) in throws {
        vanillite_game::block::drops::spawn_item(level, from, stack, velocity, 40, None);
    }
}

/// `ExperienceOrb.playerTouch` of the orb `Util.getRandom` picks among those `reach`
/// touches: with `takeXpDelay` over, one orb of its stack repairs with Mending and gives the
/// rest as experience points.
fn touch_orb(
    level: &mut Level,
    id: EntityId,
    survival: &mut vanillite_game::survival::player::Survival,
    inventory: &mut vanillite_game::inventory::PlayerInventory,
    reach: &vanillite_base::Aabb,
    global: &Global,
    taken: &mut Vec<(EntityId, EntityId)>,
) {
    use vanillite_base::random::RandomSource;
    use vanillite_game::experience_orb as orbs;
    let mut near = Vec::new();
    orbs::touching(&level.entities, reach, &mut near);
    if near.is_empty() {
        return;
    }
    let n = i32::try_from(near.len()).unwrap_or(i32::MAX);
    // `Util.getRandom(orbs, player.getRandom())`.
    let pick = survival.random.next_int_bounded(n);
    let Some(&orb) = usize::try_from(pick).ok().and_then(|i| near.get(i)) else {
        return;
    };
    if survival.take_xp_delay != 0 {
        return;
    }
    survival.take_xp_delay = orbs::TAKE_XP_DELAY;
    let random = &mut survival.random;
    let points = orbs::take_one(
        &mut level.entities,
        orb,
        inventory,
        &global.game().enchanting,
        &mut |n| random.next_int_bounded(n),
    );
    let Some(points) = points else {
        return;
    };
    taken.push((orb, id));
    if points > 0 {
        // Bukkit's `PlayerExpChangeEvent`: handlers may change the points.
        let mut points = points;
        if let Some(hooks) = level.hooks_for(vanillite_abi::event::PLAYER_EXP_CHANGE)
            && let (Some(player), Some(orb)) =
                (level.entities.store.get(id), level.entities.store.get(orb))
        {
            let mut ev = vanillite_game::hooks::Event {
                entity: player.uuid(),
                entity_type: "minecraft:player".to_owned(),
                other: orb.uuid(),
                other_type: "minecraft:experience_orb".to_owned(),
                value: i64::from(points),
                ..vanillite_game::hooks::Event::new(vanillite_abi::event::PLAYER_EXP_CHANGE)
            };
            hooks.event(vanillite_game::hooks::HookWorld::NONE, &mut ev);
            points = i32::try_from(ev.value.clamp(0, i64::from(i32::MAX))).unwrap_or(0);
        }
        if points <= 0 {
            return;
        }
        let volume = survival.give_experience_points(points);
        let Some(mut entity) = level.entities.store.get_mut(id) else {
            return;
        };
        // The score is synched data.
        entity.mark_data_dirty();
        let pos = entity.as_ref().pos();
        if let Some(volume) = volume {
            level.play_sound(
                None,
                pos.x,
                pos.y,
                pos.z,
                vanillite_game::block::sound_id("entity.player.levelup"),
                vanillite_game::level::SOUND_PLAYERS,
                volume,
                1.0,
            );
        }
    }
}

#[cfg(test)]
mod gate_tests {
    use vanillite_protocol::frame::encode_body;
    use vanillite_protocol::packets::play::clientbound::Disconnect;
    use vanillite_protocol::text::Component;

    /// #199: before `login` a client has no level; a broadcast `set_time` that reached it there
    /// ended in "Network Protocol Error". Only a kick passes the gate before `login`.
    #[test]
    fn only_disconnect_reaches_a_client_before_login() {
        let kick = encode_body(&Disconnect {
            reason: Component::text("bye"),
        });
        let chat = encode_body(
            &vanillite_protocol::packets::play::clientbound::SystemChat {
                content: Component::text("hi"),
                overlay: false,
            },
        );
        assert!(super::passes_gate(false, &kick));
        assert!(!super::passes_gate(false, &chat));
        assert!(super::passes_gate(true, &chat));
        assert!(super::passes_gate(true, &kick));
    }
}
