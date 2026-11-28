//! Worlds as the server sees them, their tick context and cross-world hand-offs.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vanillite_base::ChunkPos;
use vanillite_base::hash::FxHashMap;
use vanillite_config::{TrackingRules, WorldRules};
use vanillite_game::entities::Entities;
use vanillite_game::level::Level;
use vanillite_game::level::tiles::{Phases, TileRun};
use vanillite_world::chunks::{Chunks, FULL_LEVEL, TicketId};
use vanillite_world::entity::EntityId;
use vanillite_world::tracking::{EntityTracker, TrackerInput, TrackerOutput, Viewer};

use crate::blocks::LevelEffects;
use crate::compute::Compute;
use crate::global::Global;
use crate::players::{FanOut, WorldBlocks, WorldPlayers};
use crate::world_setup::ChunkPackets;
use crate::world_updates::WorldUpdates;

/// `PrepareSpawnTask.PREPARE_CHUNK_RADIUS`: chunks around the spawn that are full before a
/// joining player is placed.
const PREPARE_CHUNK_RADIUS: u8 = 3;

mod travel;

/// A world (dimension) of this server: its index in Vanilla's tick order (overworld, nether,
/// end, then others).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorldId(pub u16);

/// `Level.getWorldBorderAdjustedRespawnData`: a world spawn outside the world border moves to
/// the top of the border's centre column (`MOTION_BLOCKING`).
pub(crate) fn effective_spawn(
    level: &vanillite_game::level::Level,
    spawn: (vanillite_base::Vec3, f32),
) -> (vanillite_base::Vec3, f32) {
    let (pos, yaw) = spawn;
    let block = vanillite_base::BlockPos::containing(pos.x, pos.y, pos.z);
    if level.border.is_block_within_bounds(block) {
        return spawn;
    }
    let center =
        vanillite_base::BlockPos::containing(level.border.center_x(), 0.0, level.border.center_z());
    let y = vanillite_game::portal::height_at(
        level,
        vanillite_chunk::HeightmapKind::MotionBlocking,
        center.x,
        center.z,
    );
    (
        vanillite_base::Vec3::new(
            f64::from(center.x) + 0.5,
            f64::from(y),
            f64::from(center.z) + 0.5,
        ),
        yaw,
    )
}

impl WorldId {
    /// The index into the server's world list.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// Work that runs on a world's owner with exclusive access to it.
pub type WorldTask = Box<dyn FnOnce(&mut ServerWorld) + Send + 'static>;

/// One loaded world, owned by exactly one thread at a time: its world thread in parallel mode,
/// the server thread in sequential mode.
///
/// The game state is the [`Level`] (chunks, entities, simulation); the world adds the
/// players' connections, entity tracking and what is sent to clients.
pub struct ServerWorld {
    id: WorldId,
    dimension: String,
    rules: Arc<WorldRules>,
    ticks: u64,
    level: Level,
    /// Where the level's command blocks find the other worlds' levels during the tick.
    lent: Arc<crate::commands::LentLevels>,
    tracker: Box<dyn EntityTracker>,
    /// The tracker's result of the last tick.
    entity_updates: TrackerOutput,
    /// Buffers of the batched sending of `entity_updates`.
    fan_out: FanOut,
    /// Overload protection of the tracker.
    overload: Overload,
    viewers: Vec<Viewer>,
    /// Chunks sent to players this tick, for the tracker.
    chunks_sent: Vec<(EntityId, ChunkPos)>,
    pub(crate) players: WorldPlayers,
    packets: ChunkPackets,
    /// Sounds and crack progress of the player handlers.
    effects: LevelEffects,
    /// The counters of the world access rules at the last report ([`Self::report_rules`]).
    reported: RuleCounters,
    updates: WorldUpdates,
    /// The centre chunk of every player with tickets.
    player_tickets: FxHashMap<EntityId, ChunkPos>,
    /// Joins waiting for the chunks around their spawn (`PrepareSpawnTask`), with the ticket
    /// that loads them.
    spawn_waiting: Vec<(TicketId, ChunkPos, crate::players::Arrival)>,
    /// The players' chunk ticket and simulation ticket at each centre chunk with the number
    /// of players there: equal tickets at one chunk give the same levels as one, so dense
    /// players crossing chunks do not recompute the levels of their whole view.
    center_tickets: FxHashMap<ChunkPos, (u32, TicketId, TicketId)>,
    /// Where the moving players' views head ([`Chunks::set_focus`]); reused per tick.
    focus: Vec<ChunkPos>,
    scratch: Vec<ChunkPos>,
    /// Weather, lightning and the players' survival.
    pub(crate) survival: crate::survival::WorldSurvival,
    /// Mobs: their tick, the players' attacks and spawning.
    pub(crate) mobs: crate::mobs::WorldMobs,
    /// Loading and saving the entities of chunks.
    /// Player ids of the tick (block effects).
    player_scratch: Vec<vanillite_world::entity::EntityId>,
    /// The dimension's saved data (`<dimension>/data/`: world border, chunk tickets, raids,
    /// ...); systems register here when they come.
    pub(crate) data: vanillite_storage::saved_data::SavedDataStore,
    /// The saved chunk tickets of types Vanillite does not know, written back unchanged.
    kept_tickets: Vec<vanillite_nbt::Tag>,
    /// The portal tickets changed since the last save.
    tickets_dirty: bool,
    /// [`ServerWorld::release_memory`] ran and no player has been here since.
    memory_released: bool,
    /// Players arriving from other worlds or respawning, waiting for their destination.
    arrivals: Vec<travel::Waiting>,
    /// Entities besides players arriving through portals, waiting for their destination.
    entity_arrivals: Vec<travel::EntityWaiting>,
    /// Players in End gateways waiting for the exit's chunks.
    gateway_waits: Vec<travel::GatewayWait>,
    /// Portal tickets with the tick they expire.
    portal_tickets: Vec<travel::PortalTicket>,
    /// Ender pearl tickets (load and simulation) by chunk with the tick they expire; placing
    /// one again only extends it.
    pearl_tickets: FxHashMap<ChunkPos, (u64, TicketId, TicketId)>,
    /// What the players' tick asked of portals.
    portal_requests: crate::survival::PortalRequests,
    /// The chunk tickets plugins hold (not saved).
    pub(crate) plugin_tickets: crate::plugins::PluginTickets,
    /// A `/vanillite profile` runs this tick.
    profiling: bool,
    /// What this world measured for the running or last profile, until the server takes it.
    profile: Option<Box<crate::profiler::WorldProfile>>,
}

/// The time since the last lap belongs to `$phase` (only while a profile runs).
macro_rules! lap {
    ($world:ident, $phase:ident) => {
        if $world.profiling
            && let Some(profile) = $world.profile.as_deref_mut()
        {
            profile.lap(crate::profiler::Phase::$phase);
        }
    };
}

impl fmt::Debug for ServerWorld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerWorld")
            .field("id", &self.id)
            .field("dimension", &self.dimension)
            .field("ticks", &self.ticks)
            .field("entities", &self.level.entities.store.len())
            .finish_non_exhaustive()
    }
}

impl ServerWorld {
    /// Posts the game events of the players' moves (steps, swimming, landing) and those the
    /// survival logic produced.
    fn post_player_game_events(&mut self) {
        let mut pending = Vec::new();
        let mut actions = Vec::new();
        for (id, p) in self.level.entities.players.iter_mut() {
            pending.extend(
                p.survival
                    .game_events
                    .drain(..)
                    .map(|(event, pos, affected)| (id, event, pos, affected)),
            );
            actions.append(&mut p.survival.world_actions);
        }
        // The players' contact with blocks: trampled farmland, redstone ore stepped on.
        for action in actions {
            match action {
                // A sculk catalyst at a player's death.
                vanillite_game::mob::WorldAction::Level(action) => action.apply(&mut self.level),
                other => {
                    other.apply_to_level(&mut self.level);
                }
            }
        }
        for (id, event, pos, affected) in pending {
            let context = vanillite_game::level::game_event::GameEventContext {
                source: Some(id),
                affected,
                owner: None,
            };
            self.level.game_event(event, pos, context);
        }
        for (event, pos, context) in std::mem::take(&mut self.survival.game_events) {
            self.level.game_event(event, pos, context);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: WorldId,
        dimension: String,
        rules: Arc<WorldRules>,
        tracker: Box<dyn EntityTracker>,
        players: WorldPlayers,
        mut level: Level,
        survival: crate::survival::WorldSurvival,
        mut mobs: crate::mobs::WorldMobs,
        data: vanillite_storage::saved_data::SavedDataStore,
        dragon_fight: bool,
    ) -> ServerWorld {
        // `dimensionType().hasEnderDragonFight()`.
        if dragon_fight {
            mobs.init_dragon_fight(&data, &mut level);
        }
        mobs.init_raids(&data, &mut level);
        // `MinecraftServer.customSpawners`: the wandering trader spawner of the overworld.
        if dimension == "minecraft:overworld" {
            mobs.init_trader_spawner(&data);
        }
        if let Some(climate) = survival.climate() {
            level.set_climate(Arc::clone(climate));
        }
        let mut packets = ChunkPackets::default();
        packets.set_anti_xray(&rules.anti_xray, &dimension);
        ServerWorld {
            id,
            dimension,
            rules,
            ticks: 0,
            level,
            lent: Arc::default(),
            tracker,
            entity_updates: TrackerOutput::default(),
            fan_out: FanOut::default(),
            overload: Overload::default(),
            viewers: Vec::new(),
            chunks_sent: Vec::new(),
            players,
            packets,
            updates: WorldUpdates::default(),
            player_tickets: FxHashMap::default(),
            spawn_waiting: Vec::new(),
            focus: Vec::new(),
            center_tickets: FxHashMap::default(),
            effects: LevelEffects::default(),
            reported: RuleCounters::default(),
            scratch: Vec::new(),
            survival,
            mobs,
            player_scratch: Vec::new(),
            data,
            kept_tickets: Vec::new(),
            tickets_dirty: false,
            memory_released: false,
            arrivals: Vec::new(),
            entity_arrivals: Vec::new(),
            gateway_waits: Vec::new(),
            portal_tickets: Vec::new(),
            pearl_tickets: FxHashMap::default(),
            portal_requests: crate::survival::PortalRequests::default(),
            plugin_tickets: crate::plugins::PluginTickets::default(),
            profiling: false,
            profile: None,
        }
    }

    /// The chunks of this world.
    pub fn chunks(&self) -> &Chunks {
        &self.level.chunks
    }

    /// The chunks of this world, mutably (tickets, blocks).
    pub fn chunks_mut(&mut self) -> &mut Chunks {
        &mut self.level.chunks
    }

    /// The game state of this world.
    pub fn level(&self) -> &Level {
        &self.level
    }

    /// Sets where this world lends the other worlds' levels to its command blocks.
    pub(crate) fn set_lent_levels(&mut self, lent: Arc<crate::commands::LentLevels>) {
        self.lent = lent;
    }

    /// The game state of this world, mutably (block changes go through
    /// [`Level::set_block`]).
    pub fn level_mut(&mut self) -> &mut Level {
        &mut self.level
    }

    /// The level with the survival and mob systems, for plugin calls that run game code
    /// between ticks (an explosion's damage).
    pub(crate) fn level_systems(
        &mut self,
    ) -> (
        &mut Level,
        &mut crate::survival::WorldSurvival,
        &mut crate::mobs::WorldMobs,
    ) {
        (&mut self.level, &mut self.survival, &mut self.mobs)
    }

    /// Keeps every player's view loaded and the chunks in its simulation distance ticking:
    /// one load ticket and one simulation ticket at each centre chunk, shared by the players
    /// there, moved with a player that was alone, removed when the last one is gone.
    /// `ChunkMap.skipPlayer`: without `spectators_generate_chunks`, spectators hold no tickets
    /// (they see what others keep loaded).
    fn update_player_tickets(&mut self, spectators_generate_chunks: bool) {
        let chunks = &mut self.level.chunks;
        let level = chunks.player_ticket_level();
        let simulation = chunks.player_simulation_level();
        let centers = &mut self.center_tickets;
        for (id, player) in self.level.entities.players.iter() {
            if !spectators_generate_chunks
                && player.game_mode == vanillite_config::GameMode::Spectator
            {
                if let Some(old) = self.player_tickets.remove(&id) {
                    release_center(chunks, centers, old);
                }
                continue;
            }
            let center = player.chunk_view.center;
            let old = self.player_tickets.get(&id).copied();
            if old == Some(center) {
                continue;
            }
            self.player_tickets.insert(id, center);
            let alone = old.and_then(|old| {
                centers
                    .get(&old)
                    .is_some_and(|entry| entry.0 == 1)
                    .then_some(old)
            });
            if let Some(entry) = centers.get_mut(&center) {
                entry.0 += 1;
            } else if let Some(old) = alone {
                // The only player at its old centre: move those tickets, touching only the
                // chunks whose level changes.
                if let Some(entry) = centers.remove(&old) {
                    chunks.move_ticket(entry.1, center);
                    chunks.move_simulation_ticket(entry.2, center);
                    centers.insert(center, entry);
                }
                continue;
            } else {
                let ticket = chunks.add_ticket(center, level);
                let sim = chunks.add_simulation_ticket(center, simulation);
                centers.insert(center, (1, ticket, sim));
            }
            // The new tickets first, then the old ones: chunks in both views stay loaded.
            if let Some(old) = old {
                release_center(chunks, centers, old);
            }
        }
        // Moving views order the chunk work ahead of them first.
        let focus = &mut self.focus;
        focus.clear();
        focus.extend(
            self.level
                .entities
                .players
                .iter()
                .filter_map(|(_, player)| player.connection.chunks.focus())
                .take(vanillite_world::chunks::MAX_FOCUS),
        );
        chunks.set_focus(focus);
        let entities = &self.level.entities;
        self.player_tickets.retain(|&id, &mut center| {
            let alive = entities.player(id).is_some();
            if !alive {
                release_center(chunks, centers, center);
            }
            alive
        });
    }

    /// The entities of this world.
    pub fn entities(&self) -> &Entities {
        &self.level.entities
    }

    /// The entities of this world, mutably.
    pub fn entities_mut(&mut self) -> &mut Entities {
        &mut self.level.entities
    }

    /// What the entity tracker produced at the end of the last tick: encode each message
    /// once and send it to its viewers ([`TrackerOutput::fan_out`]).
    pub fn entity_updates(&self) -> &TrackerOutput {
        &self.entity_updates
    }

    /// Takes a player that joined the server into this world once the chunks within
    /// [`PREPARE_CHUNK_RADIUS`] of its spawn position are full (`PrepareSpawnTask`: a
    /// `player_spawn` ticket loads them and the player is placed when they are ready, so it
    /// never stands above chunks that are not there).
    pub(crate) fn join_player(&mut self, global: &Global, arrival: crate::players::Arrival) {
        let pos = arrival
            .saved
            .as_ref()
            .map_or(self.players.level.spawn.0, |saved| {
                vanillite_base::Vec3::new(saved.pos[0], saved.pos[1], saved.pos[2])
            });
        let chunk = ChunkPos::new(
            vanillite_base::math::floor(pos.x) >> 4,
            vanillite_base::math::floor(pos.z) >> 4,
        );
        let ticket = self
            .level
            .chunks
            .add_ticket(chunk, FULL_LEVEL - PREPARE_CHUNK_RADIUS);
        self.spawn_waiting.push((ticket, chunk, arrival));
        self.place_ready_spawns(global);
    }

    /// Places the waiting joins whose spawn chunks are full.
    fn place_ready_spawns(&mut self, global: &Global) {
        if self.spawn_waiting.is_empty() {
            return;
        }
        let radius = i32::from(PREPARE_CHUNK_RADIUS);
        let chunks = &self.level.chunks;
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.spawn_waiting)
            .into_iter()
            .partition(|(_, center, _)| {
                (-radius..=radius).all(|dz| {
                    (-radius..=radius)
                        .all(|dx| chunks.is_full(ChunkPos::new(center.x + dx, center.z + dz)))
                })
            });
        self.spawn_waiting = waiting;
        for (ticket, _, arrival) in ready {
            self.place_player(global, arrival);
            // The player's own tickets hold the chunks now.
            self.level.chunks.remove_ticket(ticket);
        }
    }

    /// `PrepareSpawnTask.Ready.spawn`: places a player whose spawn chunks are ready.
    fn place_player(&mut self, global: &Global, arrival: crate::players::Arrival) {
        let weather = self.survival.join_weather();
        let (uuid, name) = (arrival.uuid, arrival.profile.name.clone());
        let vehicle = arrival
            .saved
            .as_ref()
            .and_then(|saved| saved.other.get("RootVehicle"))
            .and_then(vanillite_nbt::Tag::as_compound)
            .cloned();
        let saved_pearls = arrival
            .saved
            .as_ref()
            .and_then(|saved| saved.other.get("ender_pearls"))
            .cloned();
        let state = crate::border::level_packets(&self.level.border, global);
        // `PrepareSpawnTask`: a new player lands around the world spawn
        // (`PlayerSpawnFinder.findSpawn`).
        let fresh = arrival.saved.is_none().then(|| {
            let (spawn, _) = effective_spawn(&self.level, self.players.level.spawn);
            let radius = global
                .level
                .rules()
                .get(vanillite_game::survival::rules::Rule::RESPAWN_RADIUS);
            vanillite_game::survival::respawn::find_spawn(
                &mut self.level,
                vanillite_base::BlockPos::containing(spawn.x, spawn.y, spawn.z),
                radius,
                crate::players::game_mode_by_id(global.level.default_game_type())
                    == Some(vanillite_config::GameMode::Adventure),
            )
        });
        self.players.join(
            &mut self.level.entities,
            global,
            self.id,
            arrival,
            fresh,
            &weather,
            &state,
        );
        // `PrepareSpawnTask`: the player rides back in.
        if let Some(vehicle) = vehicle
            && let Some(id) = self.level.entities.player_by_uuid(uuid)
        {
            self.level.spawn_root_vehicle(id, &vehicle);
        }
        let pearls = self.players.take_saved_pearls(uuid, saved_pearls.as_ref());
        self.load_pearls(global, pearls);
        let plugins = global.plugins.clone();
        if plugins.wants(vanillite_abi::event::PLAYER_JOIN) {
            let message = vanillite_game::player::join::joined_message(
                crate::scoreboard::display_name(global, &name, uuid),
            );
            let message = crate::plugins::player_message(global, message, |json| {
                plugins.player_join(
                    &mut crate::plugins::PluginWorld(self, global),
                    uuid,
                    &name,
                    json,
                )
            });
            if let Some(message) = message {
                global.players.broadcast_system(message, false, None);
            }
        }
    }

    /// The UUIDs and positions of this world's players.
    fn player_positions(&self) -> Vec<(u128, [f64; 3])> {
        let entities = &self.level.entities;
        entities
            .players
            .iter()
            .filter_map(|(id, _)| {
                let e = entities.store.get(id)?;
                let pos = e.pos();
                Some((e.uuid(), [pos.x, pos.y, pos.z]))
            })
            .collect()
    }

    /// This world's id.
    pub fn id(&self) -> WorldId {
        self.id
    }

    /// Lets finished chunk jobs wake this world between ticks: `wake` must queue a
    /// [`ServerWorld::pump_chunks`] task for the world's owner.
    pub(crate) fn set_chunk_waker(&mut self, wake: vanillite_world::chunks::Waker) {
        self.level.chunks.set_waker(wake);
    }

    /// Applies finished chunk jobs and starts the next generation steps between ticks
    /// ([`vanillite_world::chunks::Chunks::pump`]).
    pub(crate) fn pump_chunks(&mut self) {
        self.level.chunks.pump();
    }

    /// The dimension key, e.g. `minecraft:overworld`.
    pub fn dimension(&self) -> &str {
        &self.dimension
    }

    /// The rules of this world. Hot paths read plain fields.
    pub fn rules(&self) -> &WorldRules {
        &self.rules
    }

    /// Ticks this world has run.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Swaps the rules; called between ticks after a config reload.
    pub(crate) fn set_rules(&mut self, rules: Arc<WorldRules>, max_chained_property: i32) {
        let mut settings = self.level.settings.clone();
        crate::world_setup::apply_simulation_rules(&mut settings, &rules, max_chained_property);
        self.level.set_settings(settings);
        self.level.chunks.set_max_relights(
            usize::try_from(rules.simulation.max_relights_per_tick).unwrap_or(usize::MAX),
        );
        self.mobs.set_rules(&rules);
        self.packets
            .set_anti_xray(&rules.anti_xray, &self.dimension);
        self.rules = rules;
    }

    /// One world tick in Vanilla's order (architecture 3.3, `ServerLevel.tick`): the players'
    /// packets (Vanilla handles them between ticks), game time, scheduled block and fluid
    /// ticks, the chunk source (player tickets, loading, generation, post-processing of new
    /// chunks, random ticks, unloading), block events, the entities in tick order, then what
    /// clients see: light and block changes, events, chunk sending, entity tracking and the
    /// connection tick.
    pub(crate) fn tick(&mut self, ctx: &mut TickContext<'_>) {
        debug_assert_eq!(ctx.world, self.id);
        if self.survival.compute.is_none() {
            self.survival.compute = Some(ctx.compute.clone());
        }
        self.place_ready_spawns(ctx.global);
        if self.memory_released && self.level.entities.players.iter().next().is_some() {
            // Players are back: the generator is built ahead, off the tick.
            self.memory_released = false;
            self.level.chunks.prepare_generator();
        }
        let plugins = ctx.global.plugins.clone();
        let positions = plugins
            .wants(vanillite_abi::event::PLAYERS_MOVED)
            .then(|| self.player_positions());
        self.profiling = ctx.global.diagnostics.profiler.is_active();
        self.mobs.profiling = self.profiling;
        if self.profiling {
            let loads = self.level.chunks.sync_loads();
            self.profile
                .get_or_insert_with(|| {
                    Box::new(crate::profiler::WorldProfile::new((
                        loads.count,
                        loads.waited,
                    )))
                })
                .begin_tick();
        }
        let started = Instant::now();
        if self.players.recording.wants_start(&self.level.entities) {
            self.start_session_recording(ctx.global);
        }
        self.players.handle_packets(
            &mut self.level,
            &self.rules,
            ctx.runs_normally,
            ctx.global,
            self.id,
            &mut WorldBlocks {
                effects: &mut self.effects,
                tracker: &*self.tracker,
            },
        );
        self.effects.flush(&self.level.entities);
        // `handleTestInstanceBlockAction` of the players' packets.
        crate::commands::gametest::handle_actions(&mut crate::commands::Env {
            level: &mut self.level,
            global: ctx.global,
            world: self.id,
            others: Vec::new(),
        });
        if let Some(before) = positions {
            let moves: Vec<(u128, [f64; 3], [f64; 3])> = self
                .player_positions()
                .into_iter()
                .filter_map(|(uuid, to)| {
                    let (_, from) = before.iter().find(|(u, _)| *u == uuid)?;
                    (*from != to).then_some((uuid, *from, to))
                })
                .collect();
            lap!(self, NetworkIn);
            plugins.players_moved(&mut crate::plugins::PluginWorld(self, ctx.global), &moves);
            lap!(self, Plugins);
        }
        lap!(self, NetworkIn);
        plugins.run_world_tasks(&mut crate::plugins::PluginWorld(self, ctx.global), ctx.tick);
        lap!(self, Plugins);
        self.survival
            .moves(&mut self.level, ctx.global, &self.dimension);
        self.post_player_game_events();
        self.mobs.player_actions(
            &mut self.level,
            &mut self.survival,
            ctx.global,
            &self.dimension,
        );
        for id in self.players.take_respawns() {
            self.respawn(ctx, id);
        }
        self.tick_travel(ctx.global);
        lap!(self, Players);
        if self.id.0 == 0 && ctx.runs_normally {
            // `ServerFunctionManager.tick`: the data pack functions run as the server in the
            // overworld before the clocks and the worlds tick.
            crate::commands::tick_functions(
                &mut self.level,
                ctx.global,
                self.id,
                ctx.other_levels().collect(),
            );
            lap!(self, Functions);
        }
        self.players_inside_blocks();
        lap!(self, Players);
        // The rules block behaviour reads (sculk shriekers).
        self.level.settings.peaceful =
            ctx.global.level.difficulty() == vanillite_game::survival::rules::Difficulty::Peaceful;
        self.level.settings.spawn_wardens = ctx
            .global
            .level
            .rules()
            .bool(vanillite_game::survival::rules::Rule::SPAWN_WARDENS);
        self.level.set_handling_tick(true);
        if ctx.runs_normally {
            // `advanceWeatherCycle`, then `tickTime`: the overworld advances the shared game
            // time, every world runs on it. The world border moves first.
            self.level.border.tick();
            self.survival.tick_level(&mut self.level, ctx.global);
            self.level.set_weather(self.survival.weather);
            self.level
                .settings
                .apply_game_rules(&ctx.global.level.rules());
            self.skip_night(ctx.global);
            self.mobs.update_environment(
                &mut self.level,
                &self.survival,
                ctx.global,
                &self.dimension,
            );
            self.level.settings.difficulty = ctx.global.level.difficulty();
            self.level.settings.overworld_clock_time =
                crate::mobs::overworld_clock_time(ctx.global);
            self.level.settings.moon_brightness =
                crate::mobs::moon_brightness(ctx.global, &self.dimension);
            let level = &mut self.level;
            level.set_game_time(ctx.global.level.game_time());
            if self.id.0 == 0 {
                crate::commands::tick_scheduled(
                    level,
                    ctx.global,
                    self.id,
                    ctx.other_levels().collect(),
                );
            }
            lap!(self, WorldState);
            // Command blocks run here: `execute in` continues in the other world at once.
            self.lent
                .with(ctx.other_levels(), || self.level.run_scheduled_ticks());
            lap!(self, BlockTicks);
            // `raids.tick`.
            self.mobs.tick_raids(
                &mut self.level,
                &mut self.survival,
                ctx.global,
                &self.dimension,
            );
            lap!(self, Raids);
        }
        let spectators_generate_chunks = ctx
            .global
            .level
            .rules()
            .bool(vanillite_game::survival::rules::Rule::SPECTATORS_GENERATE_CHUNKS);
        self.update_player_tickets(spectators_generate_chunks);
        let level = &mut self.level;
        // `/save-off` (`ServerLevel.noSave`): chunks without tickets stay loaded.
        level.chunks.set_no_save(
            !ctx.global
                .level
                .auto_save
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        if ctx.runs_normally {
            // `ServerChunkCache.tick`: tickets that time out go first.
            level.chunks.purge_stale_tickets();
        }
        level.tick_chunk_runtime();
        level.post_process();
        lap!(self, Chunks);
        let level = &mut self.level;
        if ctx.runs_normally {
            let (spawn, _) = effective_spawn(level, self.players.level.spawn);
            let spawn_point = (self.dimension == "minecraft:overworld")
                .then(|| vanillite_base::BlockPos::containing(spawn.x, spawn.y, spawn.z));
            // `performance.region-threading`: spawning and random ticks run per tile on the
            // compute pool; off, both keep Vanilla's serial order.
            let compute = ctx.compute;
            let fork = move |n: usize, job: &(dyn Fn(usize) + Sync)| compute.for_each(n, job);
            let region = &self.rules.region_threading;
            let tiles = region.enabled.then(|| TileRun {
                size: i32::from(region.tile_size),
                min_chunks: vanillite_game::level::tiles::MIN_CHUNKS,
                fork: &fork,
            });
            // `performance.threads.tick-phases`: the read-only parts of both in parallel.
            let phases = Phases {
                threads: compute.phase_threads(),
                fork: &fork,
            };
            self.mobs.spawn(
                level,
                ctx.global,
                &self.dimension,
                spawn_point,
                tiles.as_ref(),
                &phases,
            );
            lap!(self, Spawning);
            match &tiles {
                Some(run) => level.tick_chunks_tiled(&mut self.scratch, run, &phases),
                None => level.tick_chunks(&mut self.scratch, &phases),
            }
            lap!(self, RandomTicks);
        }
        self.updates.chunk_source_tick(level);
        lap!(self, Chunks);
        if ctx.runs_normally {
            level.run_block_events();
            lap!(self, BlockEvents);
        }
        level.set_handling_tick(false);
        if ctx.runs_normally {
            self.ticks += 1;
            {
                // Command block minecarts.
                let global = ctx.global;
                let lent = Arc::clone(&self.lent);
                lent.with(ctx.other_levels(), || self.tick_entities(global));
            }
            self.entity_travel(ctx);
            lap!(self, Entities);
            self.level.tick_block_entities();
            lap!(self, BlockEntities);
            // `GameTestTicker.tick` for this world's tests.
            crate::commands::gametest::tick(&mut crate::commands::Env {
                level: &mut self.level,
                global: ctx.global,
                world: self.id,
                others: Vec::new(),
            });
            self.mobs.apply_magic_hurts(
                &mut self.level,
                &mut self.survival,
                ctx.global,
                &self.dimension,
            );
        }
        self.players.tick_entities(&mut self.level.entities);
        if ctx.runs_normally {
            self.survival.tick_entities(&mut self.level.entities);
        }
        // `entityManager.tick()`: entities of chunks that became visible this tick load now.
        self.level.tick_entity_manager();
        lap!(self, Entities);
        self.level.chunks.end_tick();
        let (light, ready) = self.updates.light_and_ready();
        self.level.chunks.take_light_changes(light);
        // Masked blocks revealed by the tick's changes go out with this tick's broadcast.
        self.packets
            .invalidate(&mut self.level.chunks, light, ready);
        for pos in &self.level.output().block_entity_updates {
            self.packets.invalidate_chunk(pos.chunk());
        }
        self.updates.broadcast(&mut self.level, ctx.global, self.id);
        lap!(self, Chunks);
        self.track_entities();
        WorldPlayers::send_entity_updates(
            &self.level.entities,
            &self.entity_updates,
            ctx.compute,
            &mut self.fan_out,
            ctx.global.plugins.packet_tap(),
        );
        lap!(self, Tracking);
        // `sendNextChunks` runs after `ChunkMap.tick`: entities in the chunks sent now are
        // still pending for this tick's tracking and become visible from the next one.
        WorldPlayers::send_chunks(
            &mut self.level.entities,
            &self
                .packets
                .source(&self.level.chunks, &self.level.block_entities),
        );
        lap!(self, ChunkSending);
        let mut portals = std::mem::take(&mut self.portal_requests);
        // The omen effects see the village and raids at the players (they tick without the
        // level); raid omens that ran out start their raids right after the players' ticks.
        self.level.flush_poi_changes();
        vanillite_game::raid::update_omen_views(&mut self.level.entities);
        {
            // `Player.tick`'s sleep part; the sleeping player list follows joins, leaves and
            // game mode changes.
            let hooks = crate::plugins::Hooks::of(ctx.global, &self.dimension);
            vanillite_game::survival::sleep::update_sleeping_player_list(&mut self.level);
            vanillite_game::survival::sleep::tick_players(&mut self.level, &hooks);
        }
        self.survival.do_tick(
            &mut self.level,
            self.mobs.cx_mut(),
            ctx.global,
            &self.dimension,
            &mut portals,
        );
        self.mobs.after_player_ticks(
            &mut self.level,
            &mut self.survival,
            ctx.global,
            &self.dimension,
        );
        self.mobs.tick_player_items(
            &mut self.level,
            &mut self.survival,
            ctx.global,
            &self.dimension,
        );
        vanillite_game::raid::start_pending_raids(
            &mut self.level.entities,
            ctx.global
                .level
                .rules()
                .bool(vanillite_game::survival::rules::Rule::RAIDS),
            self.dimension != "minecraft:the_nether",
            ctx.global.level.difficulty(),
        );
        for pos in portals.gateways.drain(..) {
            let state = self.level.block(pos);
            vanillite_game::block::portal::trigger_gateway_cooldown(&mut self.level, pos, state);
        }
        for (id, kind, entry) in portals.travels.drain(..) {
            self.enter_portal(ctx, id, kind, entry);
        }
        self.portal_requests = portals;
        self.post_player_game_events();
        lap!(self, PlayerTicks);
        self.players.tick_connections(
            &mut self.level,
            ctx.tick,
            ctx.global,
            self.id,
            &mut WorldBlocks {
                effects: &mut self.effects,
                tracker: &*self.tracker,
            },
            ctx.compute,
        );
        self.effects.flush(&self.level.entities);
        crate::scoreboard::flush(ctx.global);
        // One write per connection and tick.
        for (_, player) in self.level.entities.players.iter() {
            player.connection.sink.flush();
        }
        lap!(self, NetworkOut);
        self.overload
            .update(started.elapsed(), &self.rules.tracking, &self.dimension);
        self.report_rules();
        // Save rounds take only the time left in the tick (#47).
        let left = ctx.deadline.saturating_sub(ctx.global.clock.now());
        self.level
            .chunks
            .autosave_step(std::time::Duration::from_nanos(left));
        lap!(self, Saving);
    }

    /// Takes what this world measured for the last profile (`/vanillite profile`), with its
    /// dimension name.
    pub(crate) fn take_profile(&mut self) -> Option<(String, crate::profiler::WorldProfile)> {
        let mut profile = *self.profile.take()?;
        if let Some(entities) = self.mobs.take_entity_profile() {
            profile.entities = entities;
        }
        let loads = self.level.chunks.sync_loads();
        let (count, waited) = profile.sync_loads_at_start;
        profile.sync_loads = (
            loads.count.saturating_sub(count),
            loads.waited.saturating_sub(waited),
        );
        Some((self.dimension.clone(), profile))
    }

    /// `MinecraftServer.setInitialSpawn`'s search on this (the overworld's) level: the
    /// generator's origin chunk, then the first chunk with a respawn column in a spiral of
    /// 11 × 11 chunks around it. `flat_height` is the flat layers' height.
    pub(crate) fn search_initial_spawn(
        &mut self,
        flat_height: Option<i32>,
    ) -> vanillite_base::BlockPos {
        let level = &mut self.level;
        let origin = level
            .generation
            .as_ref()
            .map_or(vanillite_base::ChunkPos::new(0, 0), |g| {
                g.terrain().origin()
            });
        let min_y = level.min_y();
        let mut height =
            flat_height.map_or(64, |layers| min_y + layers.min(level.max_y() - min_y + 1));
        let (x, z) = (origin.min_block_x() + 8, origin.min_block_z() + 8);
        if height < min_y {
            level.load_chunk(origin);
            height = vanillite_game::portal::height_at(
                level,
                vanillite_chunk::HeightmapKind::WorldSurface,
                x,
                z,
            );
        }
        let mut spawn = vanillite_base::BlockPos::new(x, height, z);
        let (mut dx, mut dz, mut step_x, mut step_z) = (0, 0, 0, -1);
        for _ in 0..121 {
            if (-5..=5).contains(&dx) && (-5..=5).contains(&dz) {
                let chunk = vanillite_base::ChunkPos::new(origin.x + dx, origin.z + dz);
                if let Some(pos) =
                    vanillite_game::survival::respawn::spawn_pos_in_chunk(level, chunk, height)
                {
                    spawn = pos;
                    break;
                }
            }
            if dx == dz || (dx < 0 && dx == -dz) || (dx > 0 && dx == 1 - dz) {
                let old = step_x;
                step_x = -step_z;
                step_z = old;
            }
            dx += step_x;
            dz += step_z;
        }
        spawn
    }

    /// The night skip of `ServerLevel.tick`: enough players slept long enough. They wake,
    /// the dimension's clock moves to its `wake_up_from_sleep` marker (with `advance_time`)
    /// and rain stops (with `advance_weather`).
    fn skip_night(&mut self, global: &Global) {
        use vanillite_game::survival::rules::Rule;
        let hooks = crate::plugins::Hooks::of(global, &self.dimension);
        if !vanillite_game::survival::sleep::skip_night(&mut self.level, &hooks) {
            return;
        }
        let (advance_time, advance_weather) = {
            let rules = global.level.rules();
            (
                rules.bool(Rule::ADVANCE_TIME),
                rules.bool(Rule::ADVANCE_WEATHER),
            )
        };
        let clock = global.dimensions.default_clock(self.id);
        if advance_time && let Some(clock) = clock {
            let moved = if global.plugins.wants(vanillite_abi::event::TIME_SKIP) {
                self.time_skip(global, clock)
            } else {
                global
                    .level
                    .clocks()
                    .move_to_marker(clock, "minecraft:wake_up_from_sleep")
            };
            if moved == vanillite_game::survival::clock::MoveResult::Moved {
                crate::survival::broadcast_all(
                    global,
                    &crate::survival::clock_update(&global.level, clock),
                );
            }
        }
        if advance_weather && self.survival.weather.is_raining() {
            // `resetWeatherCycle`.
            let mut weather = global.level.weather();
            weather.rain_time = 0;
            weather.raining = false;
            weather.thunder_time = 0;
            weather.thundering = false;
        }
    }

    /// `moveToTimeMarker(wake_up_from_sleep)` of the night skip as plugins allow it
    /// (`TIME_SKIP`, Bukkit's `TimeSkipEvent`): cancelled, the time stays; handlers may change
    /// the ticks skipped.
    fn time_skip(
        &mut self,
        global: &Global,
        clock: usize,
    ) -> vanillite_game::survival::clock::MoveResult {
        use vanillite_game::survival::clock::MoveResult;
        /// The most ticks a plugin may skip at once (a thousand days).
        const MAX_SKIP: i64 = 24_000_000;
        let skip = {
            let clocks = global.level.clocks();
            let Some(c) = clocks.clocks.get(clock) else {
                return MoveResult::NoTimeMarkerFound;
            };
            let Some(marker) = c.marker("minecraft:wake_up_from_sleep") else {
                return MoveResult::NoTimeMarkerFound;
            };
            if marker.occurs_at(c.total_ticks) {
                return MoveResult::NotMoved;
            }
            marker.resolve_time_to_move_to(c.total_ticks) - c.total_ticks
        };
        let mut ev = vanillite_game::hooks::Event {
            value: skip,
            ..vanillite_game::hooks::Event::new(vanillite_abi::event::TIME_SKIP)
        };
        crate::plugins::Hooks::of(global, &self.dimension).event_in(
            Some(&mut crate::plugins::PluginWorld(self, global)),
            &mut ev,
        );
        if ev.cancelled {
            return MoveResult::NotMoved;
        }
        let mut clocks = global.level.clocks();
        let Some(total) = clocks.clocks.get(clock).map(|c| c.total_ticks) else {
            return MoveResult::NoTimeMarkerFound;
        };
        let skip = ev.value.clamp(0, MAX_SKIP);
        clocks.set_total_ticks(clock, total.saturating_add(skip));
        MoveResult::Moved
    }

    /// `applyEffectsFromBlocks` after the players' moves: pressure plates, tripwire and other
    /// blocks they stand in react (`Entity.checkInsideBlocks`).
    fn players_inside_blocks(&mut self) {
        self.player_scratch.clear();
        self.player_scratch
            .extend(self.level.entities.players.iter().map(|(id, _)| id));
        for &id in &self.player_scratch {
            vanillite_game::block::plate::check_inside_blocks(&mut self.level, id);
        }
    }

    /// `entityTickList.forEach(tickNonPassenger)`: every entity in tick order; players move
    /// through their packets.
    fn tick_entities(&mut self, global: &Global) {
        self.mobs
            .tick_dragon_fight(&mut self.level, &mut self.survival, global, &self.dimension);
        let items = crate::mobs::ItemTick {
            settings: crate::blocks::ItemSettings {
                lifetime: i16::try_from(self.rules.items.despawn_ticks).unwrap_or(i16::MAX),
                merge_radius: self.rules.items.merge_radius,
            },
        };
        self.mobs.tick_entities(
            &mut self.level,
            &mut self.survival,
            global,
            &self.dimension,
            items,
        );
    }

    /// Once a minute: how often the two world access rules applied
    /// (`docs/architecture/world-access.md`). Synchronous chunk loads block the tick and are
    /// logged; deferred entity world changes only at debug level.
    fn report_rules(&mut self) {
        const EVERY: u64 = 20 * 60;
        if self.ticks == self.reported.at || !self.ticks.is_multiple_of(EVERY) {
            return;
        }
        let now = RuleCounters {
            at: self.ticks,
            sync_loads: self.level.chunks.sync_loads(),
            deferred_actions: self.mobs.deferred_actions(),
        };
        let loads = now.sync_loads.count - self.reported.sync_loads.count;
        if loads > 0 {
            let waited = now.sync_loads.waited - self.reported.sync_loads.waited;
            log::info!(
                "{}: {loads} synchronous chunk loads blocked the tick for {} ms in the last minute",
                self.dimension,
                waited.as_millis()
            );
        }
        log::debug!(
            "{}: {} entity world changes applied after their tick in the last minute",
            self.dimension,
            now.deferred_actions - self.reported.deferred_actions
        );
        self.reported = now;
    }

    /// Runs the entity tracker over this tick's state and resets the per-tick sync state.
    fn track_entities(&mut self) {
        self.viewers.clear();
        self.entity_updates.clear();
        self.chunks_sent.clear();
        let mut sent = Vec::new();
        for (entity, player) in self.level.entities.players.iter_mut() {
            let view_center = player.chunk_view.center;
            self.viewers.push(Viewer {
                entity,
                view_center,
                view_distance: player.chunk_view.view_distance,
                sent_radius: player.connection.chunks.sent_radius(view_center),
            });
            player.connection.chunks.take_sent(&mut sent);
            self.chunks_sent
                .extend(sent.drain(..).map(|pos| (entity, pos)));
        }
        let entities = &self.level.entities;
        let input = TrackerInput {
            entities: &entities.store,
            viewers: &self.viewers,
            chunk_pending: &|player, chunk| WorldPlayers::chunk_pending(entities, player, chunk),
            chunks_sent: &self.chunks_sent,
            update_scale: self.overload.scale,
        };
        self.tracker.update(&input, &mut self.entity_updates);
        self.level.entities.store.clear_removed();
        self.level.entities.store.clear_sync_flags();
    }

    /// Frees what this world can build again (`performance.memory.release-when-empty`).
    pub(crate) fn release_memory(&mut self) {
        self.level.chunks.release_memory();
        self.packets.release_memory();
        let entities = &mut self.level.entities;
        entities.mobs.shrink_to_fit();
        entities.items.shrink_to_fit();
        entities.players.shrink_to_fit();
        self.memory_released = true;
    }

    /// Runs `then` once the running save round of this world is committed.
    pub(crate) fn when_saved(&mut self, then: vanillite_world::chunks::SavedCallback) {
        self.level.chunks.when_saved(then);
    }

    /// Whether the world has saved tickets to load before the first tick.
    pub(crate) fn has_saved_tickets(&self) -> bool {
        !self.level.forced.is_empty() || !self.portal_tickets.is_empty()
    }

    /// One step of `prepareLevels` for the saved tickets: runs the chunk system without a game
    /// tick and returns how many ticketed chunks are not full yet.
    pub(crate) fn prepare_step(&mut self) -> usize {
        self.level.chunks.tick();
        self.level.chunks.pending_full()
    }

    /// Starts a session recording (`debug.session-recording`): saves the world and waits until
    /// it is written, then the recording copies it.
    fn start_session_recording(&mut self, global: &Global) {
        self.save(true, false);
        let day_time = global
            .level
            .clocks()
            .clocks
            .iter()
            .find(|c| c.id == "minecraft:overworld")
            .map_or(0, |c| c.total_ticks);
        let weather = &self.survival.weather;
        let start = crate::session_recording::SessionStart {
            seed: self.level.chunks.info().seed,
            day_time,
            game_time: self.level.game_time(),
            weather: if weather.thunder_level > 0.0 {
                "thunder"
            } else if weather.rain_level > 0.0 {
                "rain"
            } else {
                "clear"
            },
            difficulty: match global.level.difficulty() {
                vanillite_game::survival::rules::Difficulty::Peaceful => "peaceful",
                vanillite_game::survival::rules::Difficulty::Easy => "easy",
                vanillite_game::survival::rules::Difficulty::Normal => "normal",
                vanillite_game::survival::rules::Difficulty::Hard => "hard",
            },
        };
        let dir = self.level.chunks.info().dir.clone();
        self.players.recording.start(&dir, &start);
    }

    /// Saves the world like Vanilla (`saveEverything`): the players always, the chunks and the
    /// dimension's saved data unless `skip_chunks` (`/save-off`). `flush` waits until the
    /// chunks are written and committed (shutdown, `/save-all flush`); otherwise a save round
    /// starts that captures them from the remaining time of the next ticks.
    pub(crate) fn save(&mut self, flush: bool, skip_chunks: bool) {
        log::debug!("Saving {} (flush: {flush})", self.dimension);
        self.players.save_all(&self.level);
        if !skip_chunks {
            if let Some(hooks) = self.level.hooks_for(vanillite_abi::event::WORLD_SAVE) {
                hooks.event(
                    vanillite_game::hooks::HookWorld::NONE,
                    &mut vanillite_game::hooks::Event::new(vanillite_abi::event::WORLD_SAVE),
                );
            }
            if let Some(border) = self.level.border.take_dirty() {
                self.data.write(crate::border::SAVED_DATA, border.to_nbt());
            }
            if self.level.structure_refs().take_dirty() {
                self.data.write(
                    vanillite_game::level::structure_refs::SAVED_DATA,
                    self.level.structure_refs().to_nbt(),
                );
            }
            if self.level.forced.take_dirty() | std::mem::take(&mut self.tickets_dirty) {
                crate::chunk_tickets::save(&self.data, &self.level, &self.saved_tickets());
            }
            self.data.save();
            self.level.save(flush);
        }
    }
}

/// Overload protection of the entity tracker (`performance.entity-tracking.overload-*`):
/// scales the movement update intervals with the smoothed world tick time.
#[derive(Debug)]
struct Overload {
    /// Smoothed world tick time in milliseconds.
    average_millis: f64,
    /// The factor on the update intervals (1 = Vanilla).
    scale: i32,
    /// Ticks since the scale last changed.
    since_change: u32,
}

impl Default for Overload {
    fn default() -> Overload {
        Overload {
            average_millis: 0.0,
            scale: 1,
            since_change: 0,
        }
    }
}

impl Overload {
    /// Ticks between two steps of the scale.
    const STEP_TICKS: u32 = 20;

    fn update(&mut self, elapsed: Duration, rules: &TrackingRules, world: &str) {
        let millis = elapsed.as_secs_f64() * 1000.0;
        self.average_millis = self.average_millis * 0.9 + millis * 0.1;
        self.since_change = self.since_change.saturating_add(1);
        let Some(target) = rules.overload_tick_millis.get() else {
            self.scale = 1;
            return;
        };
        let max = i32::try_from(rules.overload_max_interval_scale)
            .unwrap_or(1)
            .max(1);
        let target = f64::from(target);
        let scale = if self.average_millis > target && self.scale < max {
            self.scale + 1
        } else if self.average_millis < target * 0.6 && self.scale > 1 {
            self.scale - 1
        } else {
            self.scale.min(max)
        };
        if scale != self.scale && self.since_change >= Self::STEP_TICKS {
            if scale > self.scale {
                log::warn!(
                    "{world} is overloaded ({:.1} ms per tick): entity movement updates are sent \
                     every {scale}x their interval",
                    self.average_millis
                );
            } else {
                log::info!("{world}: entity movement updates every {scale}x their interval");
            }
            self.scale = scale;
            self.since_change = 0;
        }
    }
}

/// What a world tick gets besides its world.
pub struct TickContext<'a> {
    /// The world being ticked.
    pub world: WorldId,
    /// The server tick count.
    pub tick: u64,
    /// Whether game elements run this tick (`false` while frozen).
    pub runs_normally: bool,
    /// When the tick should end (clock nanoseconds); autosave uses the time left.
    pub deadline: u64,
    /// The compute pool for parallel phases.
    pub compute: &'a Compute,
    /// Server-wide state.
    pub global: &'a Global,
    /// The other worlds, when they are accessible right now (sequential mode).
    others: Option<OtherWorlds<'a>>,
    /// Cross-world work that runs after the world phase.
    outbox: &'a mut Vec<(WorldId, WorldTask)>,
}

struct OtherWorlds<'a> {
    before: &'a mut [Option<ServerWorld>],
    after: &'a mut [Option<ServerWorld>],
}

impl fmt::Debug for TickContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TickContext")
            .field("world", &self.world)
            .field("tick", &self.tick)
            .field("immediate", &self.others.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> TickContext<'a> {
    pub(crate) fn new(
        world: WorldId,
        tick: u64,
        runs_normally: bool,
        deadline: u64,
        compute: &'a Compute,
        global: &'a Global,
        outbox: &'a mut Vec<(WorldId, WorldTask)>,
    ) -> Self {
        TickContext {
            world,
            tick,
            runs_normally,
            deadline,
            compute,
            global,
            others: None,
            outbox,
        }
    }

    /// Gives the context direct access to the other worlds (sequential mode). `before` holds
    /// the worlds with lower ids, `after` those with higher ids (`None` where unloaded).
    pub(crate) fn with_others(
        mut self,
        before: &'a mut [Option<ServerWorld>],
        after: &'a mut [Option<ServerWorld>],
    ) -> Self {
        self.others = Some(OtherWorlds { before, after });
        self
    }

    /// The levels of the other worlds by world index, with `None` at this world's index, when
    /// they are at hand (sequential mode); nothing otherwise.
    pub(crate) fn other_levels(&mut self) -> impl Iterator<Item = Option<&mut Level>> {
        self.others.iter_mut().flat_map(|others| {
            let before = others
                .before
                .iter_mut()
                .map(|w| w.as_mut().map(|w| &mut w.level));
            let after = others
                .after
                .iter_mut()
                .map(|w| w.as_mut().map(|w| &mut w.level));
            before.chain(std::iter::once(None)).chain(after)
        })
    }

    /// Runs `op` on another world after the world phase of this tick, in both modes: for what
    /// Vanilla does after every level ticked (the players' connection tick).
    pub fn after_world_phase(
        &mut self,
        target: WorldId,
        op: impl FnOnce(&mut ServerWorld) + Send + 'static,
    ) {
        self.outbox.push((target, Box::new(op)));
    }

    /// Runs `op` on another world: immediately when that world is accessible (sequential
    /// mode, exact Vanilla order), otherwise after the world phase of this tick, before the
    /// target's next tick (parallel mode). This is the one hand-off primitive: entities and
    /// players changing worlds are moved by such an operation into the target's inbox.
    pub fn in_world(
        &mut self,
        target: WorldId,
        op: impl FnOnce(&mut ServerWorld) + Send + 'static,
    ) {
        let current = self.world.index();
        let index = target.index();
        let world = self.others.as_mut().and_then(|others| {
            if index < current {
                others.before.get_mut(index)
            } else {
                index
                    .checked_sub(current + 1)
                    .and_then(|i| others.after.get_mut(i))
            }
            .and_then(Option::as_mut)
        });
        match world {
            Some(world) => op(world),
            None => self.outbox.push((target, Box::new(op))),
        }
    }
}

/// Drops one player from the tickets at `pos`; the last one removes them.
fn release_center(
    chunks: &mut Chunks,
    centers: &mut FxHashMap<ChunkPos, (u32, TicketId, TicketId)>,
    pos: ChunkPos,
) {
    if let Some(entry) = centers.get_mut(&pos) {
        entry.0 = entry.0.saturating_sub(1);
        if entry.0 == 0 {
            let (_, ticket, sim) = *entry;
            centers.remove(&pos);
            chunks.remove_ticket(ticket);
            chunks.remove_simulation_ticket(sim);
        }
    }
}

/// The counters of the world access rules (`docs/architecture/world-access.md`).
#[derive(Clone, Copy, Debug, Default)]
struct RuleCounters {
    /// The world tick of the counters.
    at: u64,
    sync_loads: vanillite_world::chunks::SyncLoads,
    deferred_actions: u64,
}
