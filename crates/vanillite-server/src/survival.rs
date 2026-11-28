//! Survival in a world: weather and game time, the players' survival tick, their moves, death
//! and respawn, lightning, and the delivery of what the survival logic sends to other players.

use std::sync::Arc;

use vanillite_base::hash::FxHashMap;
use vanillite_base::random::{LegacyRandom, RandomSource};
use vanillite_base::{BlockPos, ChunkPos, Vec3};
use vanillite_chunk::HeightmapKind;
use vanillite_data::block::BlockState;
use vanillite_data::entity::ids;
use vanillite_game::block::weather::Climate;
use vanillite_game::entities::Entities;
use vanillite_game::level::block_entity::BlockEntity;
use vanillite_game::portal::PortalKind;
use vanillite_game::survival::Effect;
use vanillite_game::survival::blocks::BlockAccess;
use vanillite_game::survival::player::{self, Env, TickResult};
use vanillite_game::survival::rules::{Difficulty, GameRules, Rule};
use vanillite_game::survival::weather::{LevelWeather, WeatherPacket};
use vanillite_protocol::SharedPacket;
use vanillite_protocol::compress::Codec;
use vanillite_protocol::frame::encode_frame;
use vanillite_protocol::packets::play::clientbound::{
    ChangeDifficulty, ClockUpdate, GameEvent, RemoveEntities, SetTime,
};
use vanillite_protocol::tap::BatchMark;
use vanillite_protocol::text::Component;
use vanillite_protocol::types::VarIntList;
use vanillite_world::chunks::Chunks;
use vanillite_world::entity::EntityId;

use crate::compute::Compute;
use crate::global::Global;
use crate::level::LevelState;
use vanillite_game::player::PacketSink;

/// Block access of a world's loaded chunks.
pub(crate) struct WorldBlocks<'a>(pub(crate) &'a Chunks);

impl BlockAccess for WorldBlocks<'_> {
    #[inline]
    fn block_state(&self, pos: BlockPos) -> Option<BlockState> {
        self.0.block(pos).map(BlockState)
    }

    fn block_states_in(&self, min: BlockPos, max: BlockPos, f: &mut dyn FnMut(Option<BlockState>)) {
        self.0.blocks_in(min, max, |state| f(state.map(BlockState)));
    }
}

/// What the players' tick asks of the world's portals.
#[derive(Debug, Default)]
pub(crate) struct PortalRequests {
    /// Players that go through a portal now: the portal and the block it entered.
    pub(crate) travels: Vec<(EntityId, PortalKind, BlockPos)>,
    /// Gateways that took a player this tick and start their cooldown.
    pub(crate) gateways: Vec<BlockPos>,
}

/// The survival state of one world.
#[derive(Debug)]
pub(crate) struct WorldSurvival {
    pub(crate) weather: LevelWeather,
    /// `canHaveWeather`: sky light and no ceiling (the overworld).
    can_have_weather: bool,
    /// The overworld advances the game time.
    ticks_time: bool,
    pub(crate) effects: Vec<Effect>,
    /// Game events of the delivered effects, posted by the world (`ServerWorld`) to its
    /// listeners.
    pub(crate) game_events: Vec<PendingGameEvent>,
    weather_out: Vec<WeatherPacket>,
    /// The world's biomes for `isRainingAt`, shared with the level.
    climate: Option<Arc<Climate>>,
    pub(crate) min_y: i32,
    /// `logicalHeight` of the dimension type.
    logical_height: i32,
    /// Tracking range of players in blocks.
    player_range: i32,
    /// Player rows by chunk, rebuilt when effects are delivered.
    grid: FxHashMap<ChunkPos, Vec<u32>>,
    /// The players by row while effects are delivered.
    receivers: Vec<Receiver>,
    scratch: Vec<EntityId>,
    /// The players of a tick (reused).
    ticking: Vec<EntityId>,
    /// The delivered packets per receiver.
    outgoing: Outgoing,
    /// The compute pool for the parallel delivery to crowds (set by the world's tick).
    pub(crate) compute: Option<Compute>,
}

/// Receivers from which effects are delivered in parallel parts (fewer: on the tick thread).
const PARALLEL_RECEIVERS: usize = 256;
/// Pairs (effect, receiver) from which a delivery runs in parallel parts: below, splitting
/// costs more than it saves.
const PARALLEL_PAIRS: usize = 128 * 1024;
/// Receivers per part of a parallel delivery at least.
const RECEIVERS_PER_PART: usize = 128;

/// Whom a packet effect of a parallel delivery reaches (see [`WorldSurvival::deliver`]).
#[derive(Clone, Copy, Debug)]
enum Reach {
    /// The players tracking `entity` at `pos` (in `chunk`), and its own row if given.
    Tracking {
        entity: EntityId,
        pos: Vec3,
        chunk: ChunkPos,
        own_row: Option<u32>,
    },
    /// The players within `range` of `pos` but `except`.
    Nearby {
        pos: Vec3,
        range: f64,
        except: Option<EntityId>,
    },
    /// The player in `row`.
    Row(u32),
}

/// A player as a receiver of delivered effects.
#[derive(Clone, Copy, Debug)]
struct Receiver {
    /// Its view (`entity` is the player).
    view: vanillite_world::tracking::Viewer,
    /// Its position; `None` if it has no entity.
    pos: Option<Vec3>,
}

/// The packets of one delivery, queued per receiver as one batch: each packet is framed
/// (and compressed) once and its frame copied to the batches of its receivers, so a crowd
/// costs one queue entry per receiver instead of one per packet and receiver.
#[derive(Debug, Default)]
struct Outgoing {
    threshold: Option<usize>,
    codec: Codec,
    /// The frame of the packet being delivered (empty: it could not be framed).
    frame: Vec<u8>,
    /// Frames by player row.
    batches: Vec<Vec<u8>>,
    /// The frames of a parallel delivery.
    arena: Vec<u8>,
    /// Rows with frames, in the order they got their first one.
    rows: Vec<u32>,
}

impl Outgoing {
    /// Starts a delivery to the players of `entities`.
    fn begin(&mut self, entities: &Entities) {
        self.threshold = entities
            .players
            .iter()
            .next()
            .and_then(|(_, player)| player.connection.sink.frame_threshold());
        if self.batches.len() < entities.players.len() {
            self.batches.resize_with(entities.players.len(), Vec::new);
        }
    }

    /// Frames the packet that the next [`Outgoing::send`] calls deliver.
    fn packet(&mut self, packet: &SharedPacket) {
        self.frame.clear();
        if encode_frame(
            packet.body(),
            self.threshold,
            &mut self.codec,
            &mut self.frame,
        )
        .is_err()
        {
            self.frame.clear();
        }
    }

    /// Queues the current packet for the player `id`.
    fn send(&mut self, entities: &Entities, id: EntityId, packet: &Arc<SharedPacket>) {
        let Some(entity) = entities.store.get(id) else {
            return;
        };
        if entity.kind() == ids::PLAYER {
            self.send_row(entities, entity.kind_row(), packet);
        }
    }

    /// Queues the current packet for the player in `row`.
    fn send_row(&mut self, entities: &Entities, row: u32, packet: &Arc<SharedPacket>) {
        match self.batches.get_mut(row as usize) {
            Some(batch) if !self.frame.is_empty() => {
                if batch.is_empty() {
                    self.rows.push(row);
                }
                batch.extend_from_slice(&self.frame);
            }
            _ => {
                // Not framed: queue it on its own after what is batched for the player.
                self.flush_row(entities, row);
                if let Some(player) = entities.players.get(row) {
                    player.connection.sink.send_shared(packet);
                }
            }
        }
    }

    fn flush_row(&mut self, entities: &Entities, row: u32) {
        let Some(batch) = self.batches.get_mut(row as usize) else {
            return;
        };
        if batch.is_empty() {
            return;
        }
        let frames = std::mem::take(batch);
        if let Some(player) = entities.players.get(row) {
            player
                .connection
                .sink
                .send_batch(frames, self.threshold, BatchMark::UNKNOWN);
        }
        self.rows.retain(|&r| r != row);
    }

    /// Queues the batches: before anything else is sent to the players and at the end.
    fn flush(&mut self, entities: &Entities) {
        for row in self.rows.drain(..) {
            let Some(batch) = self.batches.get_mut(row as usize) else {
                continue;
            };
            let frames = std::mem::take(batch);
            if let Some(player) = entities.players.get(row) {
                player
                    .connection
                    .sink
                    .send_batch(frames, self.threshold, BatchMark::UNKNOWN);
            }
        }
    }
}

/// `Level.isRainingAt` without the sky light test (the heightmap test covers it outside of
/// caves and under transparent blocks).
pub(crate) fn raining_at(
    chunks: &Chunks,
    climate: Option<&Climate>,
    weather: &LevelWeather,
    pos: BlockPos,
) -> bool {
    if !weather.is_raining() {
        return false;
    }
    let Some(chunk) = chunks.chunk(pos.chunk()) else {
        return false;
    };
    let Some(heights) = chunk.chunk.heightmaps().get(HeightmapKind::MotionBlocking) else {
        return false;
    };
    #[allow(clippy::cast_sign_loss, clippy::as_conversions)]
    let top = heights.get((pos.x & 15) as u32, (pos.z & 15) as u32) as i32 + chunk.chunk.min_y();
    if top > pos.y {
        return false;
    }
    let Some(climate) = climate else {
        return true;
    };
    let biome = climate
        .manager
        .get_biome(pos.x, pos.y, pos.z, &mut |qx: i32, qy: i32, qz: i32| {
            noise_biome(chunks, qx, qy, qz)
        });
    let data = climate.biomes.get(biome);
    data.has_precipitation && data.warm_enough_to_rain(pos.x, pos.y, pos.z, climate.sea_level)
}

/// `getNoiseBiome` of the loaded chunks; the quart height is clamped to the chunk.
fn noise_biome(chunks: &Chunks, qx: i32, qy: i32, qz: i32) -> u16 {
    let Some(chunk) = chunks.chunk(ChunkPos::new(qx >> 2, qz >> 2)) else {
        return 0;
    };
    let sections = chunk.chunk.sections();
    let min_quart = chunk.chunk.min_section() * 4;
    let count = i32::try_from(sections.len()).unwrap_or(0);
    let qy = qy.clamp(min_quart, min_quart + count * 4 - 1);
    let index = usize::try_from((qy - min_quart) >> 2).unwrap_or(0);
    #[allow(clippy::cast_sign_loss, clippy::as_conversions)]
    sections.get(index).map_or(0, |s| {
        s.biome((qx & 3) as u32, (qy & 3) as u32, (qz & 3) as u32)
    })
}

fn chunk_of(pos: Vec3) -> ChunkPos {
    ChunkPos::new(
        vanillite_base::math::floor(pos.x) >> 4,
        vanillite_base::math::floor(pos.z) >> 4,
    )
}

/// `Difficulty` of `server.properties`.
pub(crate) fn difficulty(d: vanillite_config::Difficulty) -> Difficulty {
    match d {
        vanillite_config::Difficulty::Peaceful => Difficulty::Peaceful,
        vanillite_config::Difficulty::Easy => Difficulty::Easy,
        vanillite_config::Difficulty::Normal => Difficulty::Normal,
        vanillite_config::Difficulty::Hard => Difficulty::Hard,
    }
}

/// `ClientboundChangeDifficultyPacket`.
pub(crate) fn change_difficulty(level: &LevelState) -> ChangeDifficulty {
    ChangeDifficulty {
        difficulty: level.difficulty().id(),
        locked: level.difficulty_locked(),
    }
}

/// The current `set_time` with all clocks (`createFullSyncPacket`).
pub(crate) fn full_time_sync(level: &LevelState) -> SetTime {
    let advance = level.rules().bool(Rule::ADVANCE_TIME);
    level.clocks().full_sync(level.game_time(), advance)
}

/// The `set_time` after `clock` changed.
pub(crate) fn clock_update(level: &LevelState, clock: usize) -> SetTime {
    let advance = level.rules().bool(Rule::ADVANCE_TIME);
    level
        .clocks()
        .update_packet(clock, level.game_time(), advance)
}

/// `forceGameTimeSynchronization`: the game time without clocks.
pub(crate) fn game_time_sync(level: &LevelState) -> SetTime {
    SetTime {
        game_time: level.game_time(),
        clocks: Vec::<ClockUpdate>::new(),
    }
}

impl WorldSurvival {
    pub(crate) fn new(
        dimension: &str,
        level: &LevelState,
        (min_y, _height): (i32, u32),
        logical_height: i32,
        player_range: i32,
        climate: Option<Climate>,
    ) -> WorldSurvival {
        let overworld = dimension == "minecraft:overworld";
        WorldSurvival {
            weather: if overworld {
                LevelWeather::prepare(&level.weather())
            } else {
                LevelWeather::default()
            },
            can_have_weather: overworld,
            ticks_time: overworld,
            effects: Vec::new(),
            game_events: Vec::new(),
            weather_out: Vec::new(),
            climate: climate.map(Arc::new),
            min_y,
            logical_height,
            player_range: player_range.max(0),
            grid: FxHashMap::default(),
            receivers: Vec::new(),
            scratch: Vec::new(),
            ticking: Vec::new(),
            outgoing: Outgoing::default(),
            compute: None,
        }
    }

    /// The world's biomes, shared with the level.
    pub(crate) fn climate(&self) -> Option<&Arc<Climate>> {
        self.climate.as_ref()
    }

    /// The weather packets for a player entering this world.
    pub(crate) fn join_weather(&self) -> Vec<GameEvent> {
        self.weather.join_packets()
    }

    /// `advanceWeatherCycle` and `tickTime`, at the start of the world tick when it runs.
    pub(crate) fn tick_level(&mut self, world: &mut vanillite_game::level::Level, global: &Global) {
        let (entities, _, random) = world.entity_parts();
        let level = &global.level;
        let advance = level.rules().bool(Rule::ADVANCE_WEATHER);
        self.weather_out.clear();
        if self.can_have_weather {
            if advance && global.plugins.wants(vanillite_abi::event::WEATHER_CHANGE) {
                weather_change_event(global, random);
                let mut data = level.weather();
                self.weather
                    .advance(Some(&mut data), false, random, &mut self.weather_out);
            } else {
                let mut data = level.weather();
                self.weather
                    .advance(Some(&mut data), advance, random, &mut self.weather_out);
            }
        } else {
            self.weather
                .advance(None, advance, random, &mut self.weather_out);
        }
        for packet in self.weather_out.drain(..) {
            match packet {
                WeatherPacket::World(event) => {
                    let shared = SharedPacket::of(&event);
                    for (_, p) in entities.players.iter() {
                        p.connection.sink.send_shared(&shared);
                    }
                }
                WeatherPacket::All(event) => global.players.broadcast(&event, None),
            }
        }
        if self.ticks_time {
            level
                .game_time
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The survival `Env` over this world's state; `random` is the level random
    /// ([`vanillite_game::level::Level::lend_random`] while the level is borrowed).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn env<'a>(
        &'a mut self,
        random: &'a mut LegacyRandom,
        level: &'a LevelState,
        rules: &'a GameRules,
        blocks: &'a dyn BlockAccess,
        raining_at: &'a dyn Fn(BlockPos) -> bool,
        dimension: &'a str,
        enter_gateway: &'a mut dyn FnMut(BlockPos) -> bool,
        hooks: &'a dyn vanillite_game::hooks::GameHooks,
        display_name: &'a dyn Fn(&str, u128) -> Component,
        entity_team: &'a dyn Fn(&str) -> Option<vanillite_game::scoreboard::TeamFacts>,
    ) -> Env<'a> {
        Env {
            types: &level.damage_types,
            facts: &level.block_facts,
            blocks,
            rules,
            difficulty: level.difficulty(),
            game_time: level.game_time(),
            min_y: self.min_y,
            logical_height: self.logical_height,
            dimension,
            raining_at,
            border: None,
            enter_gateway,
            random,
            out: &mut self.effects,
            hooks,
            hook_world: vanillite_game::hooks::HookWorld::NONE,
            display_name,
            entity_team,
        }
    }

    /// The accepted moves of every player: jump exhaustion, fall damage and movement
    /// exhaustion (`handleMovePlayer`).
    pub(crate) fn moves(
        &mut self,
        world: &mut vanillite_game::level::Level,
        global: &Global,
        dimension: &str,
    ) {
        let level = &global.level;
        let rules = level.rules().clone();
        let mut random = world.lend_random();
        let (entities, chunks) = (&mut world.entities, &world.chunks);
        let blocks = WorldBlocks(chunks);
        let (weather, climate) = (self.weather, self.climate.clone());
        let raining = |pos| raining_at(chunks, climate.as_deref(), &weather, pos);
        let mut no_gateway = |_| false;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
        let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
        let mut env = self.env(
            &mut random,
            level,
            &rules,
            &blocks,
            &raining,
            dimension,
            &mut no_gateway,
            &hooks,
            &names,
            &teams,
        );
        let Entities { store, players, .. } = entities;
        for (id, data) in players.iter_mut() {
            if data.connection.movement.moves.is_empty() {
                continue;
            }
            let Some(mut entity) = store.get_mut(id) else {
                data.connection.movement.moves.clear();
                continue;
            };
            let moves = std::mem::take(&mut data.connection.movement.moves);
            for mv in &moves {
                player::on_move(&mut entity, data, &mut env, *mv);
            }
            let mut moves = moves;
            moves.clear();
            data.connection.movement.moves = moves;
        }
        world.restore_random(random);
        self.deliver(world, global);
    }

    /// The entity tick of players (`ServerPlayer.tick`).
    pub(crate) fn tick_entities(&mut self, entities: &mut Entities) {
        for (_, data) in entities.players.iter_mut() {
            player::tick_entity(data);
        }
    }

    /// `ServerPlayer.doTick` of every player, in the connection tick. Players whose death
    /// animation ended or whose end credits started disappear for the others; players going
    /// through portals and the gateways they entered go to `portals`.
    pub(crate) fn do_tick(
        &mut self,
        level: &mut vanillite_game::level::Level,
        cx: &mut vanillite_game::mob::MobContext,
        global: &Global,
        dimension: &str,
        portals: &mut PortalRequests,
    ) {
        self.tick_players(level, cx, global, dimension, portals, None);
    }

    /// The rest of the tick of the player `id`, which a portal just brought to this world
    /// (`player::tick_after_portal`).
    pub(crate) fn finish_portal_tick(
        &mut self,
        level: &mut vanillite_game::level::Level,
        cx: &mut vanillite_game::mob::MobContext,
        global: &Global,
        dimension: &str,
        id: EntityId,
    ) {
        self.tick_players(
            level,
            cx,
            global,
            dimension,
            &mut PortalRequests::default(),
            Some(id),
        );
    }

    /// [`Self::do_tick`], or with `arrived` only the rest of that player's tick.
    fn tick_players(
        &mut self,
        world_mut: &mut vanillite_game::level::Level,
        cx: &mut vanillite_game::mob::MobContext,
        global: &Global,
        dimension: &str,
        portals: &mut PortalRequests,
        arrived: Option<EntityId>,
    ) {
        let level = &global.level;
        let rules = level.rules().clone();
        // The players' `pushEntities` and `touch` see the level without its entities, as the
        // mob ticks do.
        let mut ents = std::mem::take(&mut world_mut.entities);
        let mut random = world_mut.lend_random();
        let entities = &mut ents;
        let world: &vanillite_game::level::Level = world_mut;
        let (chunks, block_entities) = (&world.chunks, &world.block_entities);
        let blocks = WorldBlocks(chunks);
        let (weather, climate) = (self.weather, self.climate.clone());
        let raining = |pos| raining_at(chunks, climate.as_deref(), &weather, pos);
        let mut removed = std::mem::take(&mut self.scratch);
        let mut ids = std::mem::take(&mut self.ticking);
        {
            let hooks = crate::plugins::Hooks::of(global, dimension);
            let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
            let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
            // `EndGatewayBlock.entityInside`: a gateway takes a player unless it cools down,
            // then starts its cooldown (applied after the tick, seen at once by the others).
            let gateways = &mut portals.gateways;
            let mut enter_gateway = |pos: BlockPos| {
                let ready = matches!(
                    block_entities.get(pos),
                    Some(BlockEntity::EndGateway(gateway)) if !gateway.is_cooling_down()
                ) && !gateways.contains(&pos);
                if ready {
                    gateways.push(pos);
                }
                ready
            };
            let mut env = self.env(
                &mut random,
                level,
                &rules,
                &blocks,
                &raining,
                dimension,
                &mut enter_gateway,
                &hooks,
                &names,
                &teams,
            );
            env.border = Some(&world.border);
            ids.clear();
            ids.extend(
                entities
                    .players
                    .iter()
                    .map(|(id, _)| id)
                    .filter(|&id| arrived.is_none_or(|a| a == id)),
            );
            for &id in &ids {
                // The base tick with the effects.
                let base = {
                    let Entities { store, players, .. } = &mut *entities;
                    let Some(mut entity) = store.get_mut(id) else {
                        continue;
                    };
                    let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
                        continue;
                    };
                    if arrived.is_some() {
                        std::ops::ControlFlow::Continue(player::base_tick_after_portal(
                            &mut entity,
                            data,
                            &mut env,
                        ))
                    } else {
                        player::do_tick_base(&mut entity, data, &mut env)
                    }
                };
                let result = match base {
                    std::ops::ControlFlow::Break(result) => result,
                    std::ops::ControlFlow::Continue(result) => {
                        // `updatingUsingItem`, then `aiStep` and the rest of the tick.
                        vanillite_game::player::using::tick(entities, &mut env, id);
                        let result = {
                            let Entities { store, players, .. } = &mut *entities;
                            let Some(mut entity) = store.get_mut(id) else {
                                continue;
                            };
                            let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
                                continue;
                            };
                            player::do_tick_ai(&mut entity, data, &mut env, result)
                        };
                        // `pushEntities` and `touch` at the end of the player's `aiStep`; the
                        // world picks up items, arrows and orbs in the connection tick.
                        vanillite_game::mob::living::player_ai_step_end(
                            world,
                            entities,
                            &mut env,
                            cx,
                            id,
                            |_, _| {},
                        );
                        let Entities { store, players, .. } = &mut *entities;
                        let Some(mut entity) = store.get_mut(id) else {
                            continue;
                        };
                        let Some(data) = players.get_mut(entity.as_ref().kind_row()) else {
                            continue;
                        };
                        player::do_tick_end(&mut entity, data, &mut env, result)
                    }
                };
                match result {
                    TickResult::Remove | TickResult::Credits => removed.push(id),
                    TickResult::Portal(kind, entry) => portals.travels.push((id, kind, entry)),
                    TickResult::Alive => {}
                }
            }
        }
        self.ticking = ids;
        for id in removed.drain(..) {
            // `remove(KILLED)`: the entity leaves the others' view; the player itself stays
            // until it respawns.
            if let Some(entity) = entities.store.get(id) {
                self.effects.push(Effect::Tracking {
                    entity: id,
                    with_self: false,
                    packet: SharedPacket::of(&RemoveEntities {
                        ids: VarIntList(vec![entity.network_id().0]),
                    }),
                });
            }
        }
        self.scratch = removed;
        world_mut.entities = ents;
        world_mut.restore_random(random);
        crate::scoreboard::update_criteria(global, &mut world_mut.entities);
        self.deliver(world_mut, global);
    }

    /// Kills a player (`/kill`) and delivers the effects at once; `random` is the level
    /// random.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn kill(
        entities: &mut Entities,
        chunks: Option<&Chunks>,
        random: &mut LegacyRandom,
        global: &Global,
        dimension: &str,
        id: EntityId,
        events: &mut Vec<PendingGameEvent>,
    ) -> bool {
        Self::on_player(
            entities,
            chunks,
            random,
            global,
            dimension,
            id,
            events,
            player::kill,
        )
    }

    /// Hurts a player (`/damage`, `hurtServer`) and delivers the effects at once.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn hurt(
        entities: &mut Entities,
        chunks: Option<&Chunks>,
        random: &mut LegacyRandom,
        global: &Global,
        dimension: &str,
        id: EntityId,
        source: vanillite_game::survival::damage::DamageSource,
        amount: f32,
        events: &mut Vec<PendingGameEvent>,
    ) -> bool {
        // The causing and direct entities for the plugins' damage and death events.
        let attackers = if global.plugins.wants(vanillite_abi::event::ENTITY_DAMAGE)
            || global.plugins.wants(vanillite_abi::event::PLAYER_DEATH)
        {
            let uuid_of =
                |id: Option<EntityId>| id.and_then(|i| entities.store.get(i)).map(|e| e.uuid());
            (uuid_of(source.cause), uuid_of(source.direct))
        } else {
            (None, None)
        };
        Self::on_player(
            entities,
            chunks,
            random,
            global,
            dimension,
            id,
            events,
            |entity, data, env| player::hurt_by(entity, data, env, source, amount, attackers),
        )
    }

    /// Runs `f` on the entities of a level with a survival environment and a mob context
    /// outside the tick (commands acting on any entity); delivers the effects, applies the
    /// world changes that need only the level and removes the discarded entities.
    pub(crate) fn with_entities<R>(
        level: &mut vanillite_game::level::Level,
        global: &Global,
        dimension: &str,
        f: impl FnOnce(
            &vanillite_game::level::Level,
            &mut Entities,
            &mut Env<'_>,
            &mut vanillite_game::mob::MobContext,
        ) -> R,
    ) -> R {
        let state = &global.level;
        let rules = state.rules().clone();
        // The level random, as in the world's tick (`level.random` of Vanilla).
        let mut random = level.lend_random();
        let mut out = Vec::new();
        let mut cx = vanillite_game::mob::MobContext::default();
        let mut ents = std::mem::take(&mut level.entities);
        let r = {
            let blocks = WorldBlocks(&level.chunks);
            let raining = |_| false;
            let mut no_gateway = |_| false;
            let hooks = crate::plugins::Hooks::of(global, dimension);
            let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
            let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
            let mut env = Env {
                hooks: &hooks,
                hook_world: vanillite_game::hooks::HookWorld::NONE,
                display_name: &names,
                entity_team: &teams,
                types: &state.damage_types,
                facts: &state.block_facts,
                blocks: &blocks,
                rules: &rules,
                // The level's own (the server's, kept in sync every tick).
                difficulty: level.settings.difficulty,
                game_time: state.game_time(),
                min_y: level.min_y(),
                logical_height: global.dimensions.logical_height_of(dimension),
                dimension,
                raining_at: &raining,
                border: None,
                enter_gateway: &mut no_gateway,
                random: &mut random,
                out: &mut out,
            };
            f(level, &mut ents, &mut env, &mut cx)
        };
        level.entities = ents;
        level.restore_random(random);
        for action in std::mem::take(&mut cx.actions) {
            use vanillite_game::mob::WorldAction;
            match action {
                // The entities an explosion hurts take the damage in the world's next tick.
                WorldAction::Explode(e) => e.run(level, &rules),
                WorldAction::Level(action) => action.apply(level),
                other => {
                    other.apply_to_level(level);
                }
            }
        }
        for id in std::mem::take(&mut cx.discarded) {
            level.entities.despawn(id);
        }
        // The End fight is the world's: it takes these before its next tick.
        level.output().dragon_events.append(&mut cx.dragon_events);
        let (entities, network_ids, random) = level.entity_parts();
        deliver_simple(&out, entities, network_ids, random, global);
        for effect in &out {
            if let Effect::GameEvent {
                event,
                pos,
                context,
            } = effect
            {
                level.game_event(*event, *pos, *context);
            }
        }
        r
    }

    /// Runs `f` on a player with a survival environment outside the tick (commands) and
    /// delivers its effects at once; its game events go to `events` for the caller to post.
    #[allow(clippy::too_many_arguments)]
    fn on_player(
        entities: &mut Entities,
        chunks: Option<&Chunks>,
        random: &mut LegacyRandom,
        global: &Global,
        dimension: &str,
        id: EntityId,
        events: &mut Vec<PendingGameEvent>,
        f: impl FnOnce(
            &mut vanillite_world::entity::EntityMut<'_>,
            &mut vanillite_game::player::PlayerData,
            &mut Env<'_>,
        ) -> bool,
    ) -> bool {
        struct NoBlocks;
        impl BlockAccess for NoBlocks {
            fn block_state(&self, _: BlockPos) -> Option<BlockState> {
                None
            }
        }
        let level = &global.level;
        let rules = level.rules().clone();
        let mut out = Vec::new();
        let no_blocks = NoBlocks;
        let world_blocks = chunks.map(WorldBlocks);
        let blocks: &dyn BlockAccess = match &world_blocks {
            Some(b) => b,
            None => &no_blocks,
        };
        let raining = |_| false;
        let mut no_gateway = |_| false;
        let hooks = crate::plugins::Hooks::of(global, dimension);
        let names = |name: &str, uuid| crate::scoreboard::display_name(global, name, uuid);
        let teams = |holder: &str| crate::scoreboard::entity_team(global, holder);
        let mut env = Env {
            hooks: &hooks,
            hook_world: vanillite_game::hooks::HookWorld::NONE,
            display_name: &names,
            entity_team: &teams,
            types: &level.damage_types,
            facts: &level.block_facts,
            blocks,
            rules: &rules,
            difficulty: level.difficulty(),
            game_time: level.game_time(),
            min_y: 0,
            logical_height: global.dimensions.logical_height_of(dimension),
            dimension,
            raining_at: &raining,
            border: None,
            enter_gateway: &mut no_gateway,
            random,
            out: &mut out,
        };
        let Entities { store, players, .. } = entities;
        let Some(mut entity) = store.get_mut(id) else {
            return false;
        };
        let row = entity.as_ref().kind_row();
        let Some(data) = players.get_mut(row) else {
            return false;
        };
        let done = f(&mut entity, data, &mut env);
        deliver_simple(&out, entities, &global.network_ids, random, global);
        collect_game_events(&out, events);
        done
    }

    /// The players by row and by chunk for the delivery of effects.
    fn build_grid(&mut self, entities: &Entities) {
        for list in self.grid.values_mut() {
            list.clear();
        }
        self.receivers.clear();
        for ((id, player), row) in entities.players.iter().zip(0u32..) {
            let pos = entities.store.get(id).map(|e| e.pos());
            if let Some(pos) = pos {
                self.grid.entry(chunk_of(pos)).or_default().push(row);
            }
            self.receivers.push(Receiver {
                view: vanillite_world::tracking::Viewer {
                    entity: id,
                    view_center: player.chunk_view.center,
                    view_distance: player.chunk_view.view_distance,
                    sent_radius: 0,
                },
                pos,
            });
        }
        self.grid.retain(|_, list| !list.is_empty());
    }

    /// Sends what the survival logic produced to its receivers. Each receiver lookup visits
    /// the players near the source only: all players when there are few, else the chunks in
    /// range.
    pub(crate) fn deliver(&mut self, world: &mut vanillite_game::level::Level, global: &Global) {
        if self.effects.is_empty() {
            return;
        }
        let (entities, _, random) = world.entity_parts();
        let effects = std::mem::take(&mut self.effects);
        collect_game_events(&effects, &mut self.game_events);
        self.build_grid(entities);
        let mut out = std::mem::take(&mut self.outgoing);
        out.begin(entities);
        match self.compute.clone().filter(|c| {
            c.phase_threads() > 1
                && self.receivers.len() >= PARALLEL_RECEIVERS
                && effects.len().saturating_mul(self.receivers.len()) >= PARALLEL_PAIRS
        }) {
            Some(compute) => self.deliver_parts(&effects, entities, global, &mut out, &compute),
            None => self.deliver_serial(&effects, entities, global, &mut out),
        }
        out.flush(entities);
        self.outgoing = out;
        spawn_drops(&effects, entities, &global.network_ids, random, global);
    }

    /// [`WorldSurvival::deliver`] to a crowd: the packets are framed once on the tick thread,
    /// then parts of the receivers (contiguous rows) pick the packets they get in effect
    /// order, each into its own batches, and queue them, in parallel. Every receiver gets the
    /// same packets in the same order as from [`WorldSurvival::deliver_serial`]. Effects that
    /// reach beyond the packets (messages, deaths) and packets that cannot be framed run on
    /// the tick thread between the parts.
    fn deliver_parts(
        &self,
        effects: &[Effect],
        entities: &mut Entities,
        global: &Global,
        out: &mut Outgoing,
        compute: &Compute,
    ) {
        let mut reaches: Vec<(Reach, usize, usize)> = Vec::new();
        out.arena.clear();
        for effect in effects {
            let (reach, packet) = match effect {
                Effect::Tracking {
                    entity,
                    with_self,
                    packet,
                } => {
                    let Some(source) = entities.store.get(*entity) else {
                        continue;
                    };
                    let pos = source.pos();
                    let own_row =
                        (*with_self && source.kind() == ids::PLAYER).then(|| source.kind_row());
                    let reach = Reach::Tracking {
                        entity: *entity,
                        pos,
                        chunk: chunk_of(pos),
                        own_row,
                    };
                    (reach, packet)
                }
                Effect::Nearby {
                    pos,
                    range,
                    except,
                    packet,
                } => {
                    let reach = Reach::Nearby {
                        pos: *pos,
                        range: *range,
                        except: *except,
                    };
                    (reach, packet)
                }
                Effect::To { player, packet } if entities.player(*player).is_some() => {
                    match entities.store.get(*player) {
                        Some(e) if e.kind() == ids::PLAYER => (Reach::Row(e.kind_row()), packet),
                        _ => continue,
                    }
                }
                Effect::To { player, packet } => {
                    // Not in this world: sent past the batches, which only this world's
                    // players get.
                    send_departed(entities, global, *player, packet);
                    continue;
                }
                Effect::Broadcast(_) | Effect::Died { .. } => {
                    self.run_parts(&reaches, entities, out, compute);
                    reaches.clear();
                    out.arena.clear();
                    self.deliver_serial(std::slice::from_ref(effect), entities, global, out);
                    out.flush(entities);
                    continue;
                }
                _ => continue,
            };
            out.packet(packet);
            if out.frame.is_empty() {
                // Not framed: on its own after what is batched, like the serial delivery.
                self.run_parts(&reaches, entities, out, compute);
                reaches.clear();
                out.arena.clear();
                self.deliver_serial(std::slice::from_ref(effect), entities, global, out);
                out.flush(entities);
                continue;
            }
            let start = out.arena.len();
            out.arena.extend_from_slice(&out.frame);
            reaches.push((reach, start, out.arena.len()));
        }
        self.run_parts(&reaches, entities, out, compute);
        out.arena.clear();
    }

    /// The parallel parts of [`WorldSurvival::deliver_parts`] for `reaches` (framed in
    /// `out.arena`): batches and queues the packets of every receiver.
    fn run_parts(
        &self,
        reaches: &[(Reach, usize, usize)],
        entities: &Entities,
        out: &mut Outgoing,
        compute: &Compute,
    ) {
        if reaches.is_empty() {
            return;
        }
        let sinks: Vec<&Arc<dyn PacketSink>> = entities
            .players
            .iter()
            .map(|(_, player)| &player.connection.sink)
            .collect();
        let n = sinks.len().min(out.batches.len());
        let Outgoing {
            threshold,
            arena,
            batches,
            ..
        } = out;
        let (threshold, arena) = (*threshold, &*arena);
        let (grid, receivers) = (&self.grid, &self.receivers);
        let player_range = f64::from(self.player_range);
        compute.parts(&mut batches[..n], RECEIVERS_PER_PART, |offset, part| {
            let rows = offset..offset + part.len();
            let mut near = Vec::new();
            let mut add = |row: u32, frame: &[u8]| {
                if let Some(batch) = (row as usize)
                    .checked_sub(offset)
                    .and_then(|i| part.get_mut(i))
                {
                    batch.extend_from_slice(frame);
                }
            };
            for &(reach, start, end) in reaches {
                let frame = &arena[start..end];
                match reach {
                    Reach::Tracking {
                        entity,
                        pos,
                        chunk,
                        own_row,
                    } => {
                        near_rows(grid, receivers.len(), pos, player_range, &mut near);
                        for &row in &near {
                            if !rows.contains(&(row as usize)) {
                                continue;
                            }
                            let Some(&Receiver {
                                view,
                                pos: Some(at),
                            }) = receivers.get(row as usize)
                            else {
                                continue;
                            };
                            if view.entity == entity {
                                continue;
                            }
                            let d = at - pos;
                            if view.sees_chunk(chunk)
                                && d.x * d.x + d.z * d.z <= player_range * player_range
                            {
                                add(row, frame);
                            }
                        }
                        if let Some(row) = own_row {
                            add(row, frame);
                        }
                    }
                    Reach::Nearby { pos, range, except } => {
                        near_rows(grid, receivers.len(), pos, range, &mut near);
                        for &row in &near {
                            if !rows.contains(&(row as usize)) {
                                continue;
                            }
                            let Some(&Receiver {
                                view,
                                pos: Some(at),
                            }) = receivers.get(row as usize)
                            else {
                                continue;
                            };
                            if Some(view.entity) == except {
                                continue;
                            }
                            let d = at - pos;
                            if d.x * d.x + d.y * d.y + d.z * d.z < range * range {
                                add(row, frame);
                            }
                        }
                    }
                    Reach::Row(row) => add(row, frame),
                }
            }
            for (i, batch) in part.iter_mut().enumerate() {
                if batch.is_empty() {
                    continue;
                }
                let frames = std::mem::take(batch);
                if let Some(sink) = sinks.get(offset + i) {
                    sink.send_batch(frames, threshold, BatchMark::UNKNOWN);
                }
            }
        });
    }

    /// [`WorldSurvival::deliver`] on the tick thread, effect by effect.
    fn deliver_serial(
        &self,
        effects: &[Effect],
        entities: &mut Entities,
        global: &Global,
        out: &mut Outgoing,
    ) {
        let mut receivers = Vec::new();
        for effect in effects {
            match effect {
                Effect::Tracking {
                    entity,
                    with_self,
                    packet,
                } => {
                    let Some(source) = entities.store.get(*entity) else {
                        continue;
                    };
                    let pos = source.pos();
                    let chunk = chunk_of(pos);
                    let range = f64::from(self.player_range);
                    near_rows(&self.grid, self.receivers.len(), pos, range, &mut receivers);
                    out.packet(packet);
                    for &row in &receivers {
                        let Some(&Receiver {
                            view,
                            pos: Some(at),
                        }) = self.receivers.get(row as usize)
                        else {
                            continue;
                        };
                        if view.entity == *entity {
                            continue;
                        }
                        let d = at - pos;
                        if view.sees_chunk(chunk) && d.x * d.x + d.z * d.z <= range * range {
                            out.send_row(entities, row, packet);
                        }
                    }
                    if *with_self {
                        out.send(entities, *entity, packet);
                    }
                }
                Effect::Nearby {
                    pos,
                    range,
                    except,
                    packet,
                } => {
                    near_rows(
                        &self.grid,
                        self.receivers.len(),
                        *pos,
                        *range,
                        &mut receivers,
                    );
                    out.packet(packet);
                    for &row in &receivers {
                        let Some(&Receiver {
                            view,
                            pos: Some(at),
                        }) = self.receivers.get(row as usize)
                        else {
                            continue;
                        };
                        if Some(view.entity) == *except {
                            continue;
                        }
                        let d = at - *pos;
                        if d.x * d.x + d.y * d.y + d.z * d.z < range * range {
                            out.send_row(entities, row, packet);
                        }
                    }
                }
                Effect::Broadcast(message) => {
                    out.flush(entities);
                    global
                        .players
                        .broadcast_system(message.clone(), false, None);
                }
                Effect::Died {
                    victim,
                    killer,
                    killer_kind,
                    message,
                } => {
                    out.flush(entities);
                    // `ServerPlayer.die`: `tellNeutralMobsThatIDied` with `forgive_dead_players`.
                    if entities.player(*victim).is_some()
                        && global.level.rules().bool(Rule::FORGIVE_DEAD_PLAYERS)
                    {
                        vanillite_game::mob::common::tell_neutral_mobs_player_died(
                            entities, *victim,
                        );
                    }
                    crate::scoreboard::died(
                        global,
                        entities,
                        *victim,
                        *killer,
                        *killer_kind,
                        message.as_ref(),
                    );
                }
                Effect::To { player, packet } => {
                    if entities.player(*player).is_none() {
                        send_departed(entities, global, *player, packet);
                        continue;
                    }
                    out.packet(packet);
                    out.send(entities, *player, packet);
                }
                Effect::DropItem { .. }
                | Effect::Experience { .. }
                | Effect::SpawnMob { .. }
                | Effect::GameEvent { .. }
                | Effect::PearlsVanish(_)
                | Effect::RespawnShoulder { .. } => {}
            }
        }
    }
}

/// The rows of the receivers near `pos`: all of them when there are few, else those of the
/// chunks in range.
fn near_rows(
    grid: &FxHashMap<ChunkPos, Vec<u32>>,
    receivers: usize,
    pos: Vec3,
    range: f64,
    out: &mut Vec<u32>,
) {
    out.clear();
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let r = (range / 16.0).ceil() as i32 + 1;
    let side = usize::try_from(2 * r + 1).unwrap_or(usize::MAX);
    if receivers <= side.saturating_mul(side) {
        out.extend((0u32..).take(receivers));
        return;
    }
    let c = chunk_of(pos);
    for x in c.x - r..=c.x + r {
        for z in c.z - r..=c.z + r {
            if let Some(list) = grid.get(&ChunkPos::new(x, z)) {
                out.extend_from_slice(list);
            }
        }
    }
}

/// Effects of a command, delivered by looking at all players of the world.
/// `random` is the level random.
fn deliver_simple(
    effects: &[Effect],
    entities: &mut Entities,
    network_ids: &vanillite_world::entity::NetworkIds,
    random: &mut LegacyRandom,
    global: &Global,
) {
    for effect in effects {
        match effect {
            Effect::Tracking {
                entity,
                with_self,
                packet,
            } => {
                let Some(source) = entities.store.get(*entity) else {
                    continue;
                };
                let chunk = chunk_of(source.pos());
                for (id, p) in entities.players.iter() {
                    let sees = vanillite_world::tracking::Viewer {
                        entity: id,
                        view_center: p.chunk_view.center,
                        view_distance: p.chunk_view.view_distance,
                        sent_radius: 0,
                    }
                    .sees_chunk(chunk);
                    if (id == *entity && *with_self) || (id != *entity && sees) {
                        p.connection.sink.send_shared(packet);
                    }
                }
            }
            Effect::Nearby {
                pos,
                range,
                except,
                packet,
            } => {
                for (id, p) in entities.players.iter() {
                    if Some(id) == *except {
                        continue;
                    }
                    if let Some(e) = entities.store.get(id) {
                        let d = e.pos() - *pos;
                        if d.x * d.x + d.y * d.y + d.z * d.z < range * range {
                            p.connection.sink.send_shared(packet);
                        }
                    }
                }
            }
            Effect::Broadcast(message) => {
                global
                    .players
                    .broadcast_system(message.clone(), false, None);
            }
            Effect::Died {
                victim,
                killer,
                killer_kind,
                message,
            } => {
                // `ServerPlayer.die`: `tellNeutralMobsThatIDied` with `forgive_dead_players`.
                if entities.player(*victim).is_some()
                    && global.level.rules().bool(Rule::FORGIVE_DEAD_PLAYERS)
                {
                    vanillite_game::mob::common::tell_neutral_mobs_player_died(entities, *victim);
                }
                crate::scoreboard::died(
                    global,
                    entities,
                    *victim,
                    *killer,
                    *killer_kind,
                    message.as_ref(),
                );
            }
            Effect::To { player, packet } => {
                if let Some(p) = entities.player(*player) {
                    p.connection.sink.send_shared(packet);
                } else {
                    send_departed(entities, global, *player, packet);
                }
            }
            Effect::DropItem { .. }
            | Effect::Experience { .. }
            | Effect::SpawnMob { .. }
            | Effect::GameEvent { .. }
            | Effect::PearlsVanish(_)
            | Effect::RespawnShoulder { .. } => {}
        }
    }
    spawn_drops(effects, entities, network_ids, random, global);
}

/// A game event for the world to post: the event, its position and context.
pub(crate) type PendingGameEvent = (
    u16,
    Vec3,
    vanillite_game::level::game_event::GameEventContext,
);

/// The [`Effect::GameEvent`]s of `effects`, for the world to post.
fn collect_game_events(effects: &[Effect], out: &mut Vec<PendingGameEvent>) {
    for effect in effects {
        if let Effect::GameEvent {
            event,
            pos,
            context,
        } = effect
        {
            out.push((*event, *pos, *context));
        }
    }
}

/// `ExperienceOrb.award` of an [`Effect::Experience`]: the merge group comes from the level
/// `random`, the new orbs' yaw and motion from the orbs' own random.
fn award_experience(
    effect: &Effect,
    entities: &mut Entities,
    network_ids: &vanillite_world::entity::NetworkIds,
    random: &mut LegacyRandom,
) {
    if let Effect::Experience { pos, amount } = *effect {
        vanillite_game::experience_orb::award_in(
            entities,
            network_ids,
            random,
            pos,
            Vec3::default(),
            amount,
        );
    }
}

/// [`Effect::To`] for a player that left the world: it reaches the player in its new world
/// (the boss bars' removal once they notice it is gone).
fn send_departed(
    entities: &Entities,
    global: &Global,
    player: EntityId,
    packet: &Arc<SharedPacket>,
) {
    if let Some(uuid) = entities.departed_uuid(player)
        && let Some((sink, _)) = global.players.connection(crate::player_list::Uuid(uuid))
    {
        sink.send_shared(packet);
    }
}

/// The item entities of [`Effect::DropItem`]s (`addFreshEntity`) and the experience orbs of
/// [`Effect::Experience`]s. `random` is the level random.
fn spawn_drops(
    effects: &[Effect],
    entities: &mut Entities,
    network_ids: &vanillite_world::entity::NetworkIds,
    random: &mut LegacyRandom,
    global: &Global,
) {
    for effect in effects {
        if let Effect::PearlsVanish(uuid) = *effect
            && global
                .level
                .rules()
                .bool(vanillite_game::survival::rules::Rule::ENDER_PEARLS_VANISH_ON_DEATH)
        {
            for index in global.dimensions.worlds().map(|w| w.0) {
                global.run_in_world(crate::world::WorldId(index), move |world| {
                    world.discard_pearls_of(uuid);
                });
            }
        }
        award_experience(effect, entities, network_ids, random);
        if let Effect::SpawnMob {
            kind,
            pos,
            y_rot,
            velocity,
        } = effect
            && let Some(kind) = vanillite_game::mob::Kind::of(*kind)
        {
            let (uuid, seed) = vanillite_game::experience_orb::with_entity_random(|r| {
                (vanillite_game::mob::random_uuid(r), r.next_long())
            });
            let rot = vanillite_world::entity::Rotation {
                y_rot: *y_rot,
                x_rot: 0.0,
                y_head_rot: *y_rot,
            };
            let data = vanillite_game::mob::MobData::new(kind, seed);
            let id = vanillite_game::mob::spawn(entities, network_ids, uuid, *pos, rot, data);
            if let Some(mut e) = entities.store.get_mut(id) {
                e.set_velocity(*velocity);
            }
            entities.spawned(id, vanillite_abi::spawn_reason::TRIGGERED, 0);
        }
        if let Effect::RespawnShoulder { tag, pos, owner } = effect {
            // `EntityType.create(tag, LOAD)`, `setOwner`, `setPos`, `addWithUUID`.
            let seed = vanillite_game::experience_orb::with_entity_random(|r| r.next_long());
            if let Some(mut loaded) = vanillite_game::mob::nbt::load_mob(tag, seed) {
                loaded.data.owner = Some(*owner);
                let id = vanillite_game::mob::spawn(
                    entities,
                    network_ids,
                    loaded.uuid,
                    *pos,
                    loaded.rot,
                    loaded.data,
                );
                if let Some(mut e) = entities.store.get_mut(id) {
                    e.set_velocity(loaded.velocity);
                }
            }
        }
        if let Effect::DropItem {
            uuid,
            random: item_random,
            pos,
            velocity,
            y_rot,
            pickup_delay,
            stack,
        } = effect
        {
            let id = entities.spawn_item(
                network_ids.next(),
                *uuid,
                *pos,
                *velocity,
                *y_rot,
                vanillite_game::item_entity::ItemData::new(
                    stack.clone(),
                    *pickup_delay,
                    None,
                    *item_random,
                ),
            );
            entities.spawned(id, vanillite_abi::spawn_reason::DEFAULT, 0);
        }
    }
}

/// Sends `packet` to every player of the server (`broadcastAll`).
pub(crate) fn broadcast_all<P: vanillite_protocol::Packet>(global: &Global, packet: &P) {
    global.players.broadcast(packet, None);
}

/// The `weather` code of a world's weather for plugins (thunder needs rain).
fn weather_code(raining: bool, thundering: bool) -> u32 {
    use vanillite_abi::weather;
    match (raining, thundering) {
        (false, _) => weather::CLEAR,
        (true, false) => weather::RAIN,
        (true, true) => weather::THUNDER,
    }
}

/// The weather timers of `advanceWeatherCycle` with `WEATHER_CHANGE` (Bukkit's
/// `WeatherChangeEvent` and `ThunderChangeEvent`): when the rain or thunder flips, plugins
/// may cancel it and the old weather stays. The timers are not locked while handlers run.
fn weather_change_event(global: &Global, random: &mut LegacyRandom) {
    let (old, new) = {
        let mut data = global.level.weather();
        let old = (data.raining, data.thundering);
        data.advance(random);
        (old, (data.raining, data.thundering))
    };
    let (old_code, new_code) = (weather_code(old.0, old.1), weather_code(new.0, new.1));
    if old_code == new_code {
        return;
    }
    let mut ev = vanillite_game::hooks::Event {
        cause: new_code,
        extra: i64::from(old_code),
        ..vanillite_game::hooks::Event::new(vanillite_abi::event::WEATHER_CHANGE)
    };
    crate::plugins::Hooks::of(global, "minecraft:overworld").event(&mut ev);
    if ev.cancelled {
        let mut data = global.level.weather();
        data.raining = old.0;
        data.thundering = old.1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An [`Effect::Experience`] spawns its orbs (#134: it borrowed the orb random twice).
    #[test]
    fn experience_effects_spawn_orbs() {
        let ids = vanillite_world::entity::NetworkIds::new();
        let mut entities = Entities::new();
        let mut random = LegacyRandom::new(7);
        let effect = Effect::Experience {
            pos: Vec3::new(0.5, 64.0, 0.5),
            amount: 10,
        };
        award_experience(&effect, &mut entities, &ids, &mut random);
        award_experience(&effect, &mut entities, &ids, &mut random);
        let total: i32 = entities
            .experience_orbs
            .iter()
            .map(|(_, d)| d.value * d.count)
            .sum();
        assert_eq!(total, 20);
    }
}
