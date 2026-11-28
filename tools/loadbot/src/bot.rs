//! One bot: login, configuration and the play loop (random walk, straight flight, spread,
//! standing still or survival play, chat, latency probes).
//!
//! Incoming packets are framed and counted; only the few a client must answer are decoded.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, watch};
use vanillite_data::entity::ENTITY_TYPES;
use vanillite_protocol::codec::{Encode, Reader, write_str, write_varint};
use vanillite_protocol::frame::{FrameDecoder, FrameEncoder};
use vanillite_protocol::ids;
use vanillite_protocol::packets::common::ClientInfo;
use vanillite_protocol::packets::configuration::serverbound as csb;
use vanillite_protocol::packets::handshake::{Intent, Intention};
use vanillite_protocol::packets::login::{
    CookieResponse, CustomQueryAnswer, Hello, LoginAcknowledged,
};
use vanillite_protocol::packets::play::clientbound::{
    AddEntity, EntityPositionSync, MoveEntityPos, MoveEntityPosRot, PlayerPosition, PositionPath,
    RemoveEntities, TeleportEntity, VecDelta,
};
use vanillite_protocol::packets::play::serverbound::{
    AcceptTeleportation, Attack, ChunkBatchReceived, ClientCommand, ClientCommandAction,
    ClientTickEnd, DigAction, Hand, MoveFlags, MovePlayerPosRot, PlayerActionPacket, PlayerLoaded,
    UseItemOn,
};
use vanillite_protocol::sets::{ClientboundConfiguration, ClientboundLogin};
use vanillite_protocol::types::{BlockPos, Uuid};
use vanillite_protocol::{Decode, Packet, State};

use crate::{Config, Metrics, Movement};

/// Walking speed in blocks per tick (Vanilla's 4.317 blocks per second).
const WALK_SPEED: f64 = 4.317 / 20.0;
/// Cruise altitude of straight flight: above the highest overworld terrain, so no move
/// collides with blocks.
const FLIGHT_Y: f64 = 300.0;
/// Climb or descent towards the cruise altitude in blocks per tick.
const CLIMB_SPEED: f64 = 5.0;
/// Seconds between latency probes (`ping_request`).
const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// Chunks around the bot that count as "near" for the holes metric: a square of
/// (2 * NEAR + 1)² chunks.
const NEAR: i32 = 2;
/// Speed of the flight to the spread point in blocks per second (boat on blue ice).
const SPREAD_SPEED: f64 = 72.7;
/// Bytes read per `read` call.
const READ_CHUNK: usize = 16 * 1024;
/// Id of the `MOTION_BLOCKING` heightmap in `level_chunk_with_light`.
const MOTION_BLOCKING: i32 = 4;
/// Lowest block y of the overworld, the base of the heightmap values (load tests run there).
const MIN_Y: f64 = -64.0;
/// Half the width of a player's bounding box.
const HALF_WIDTH: f64 = 0.3;
/// Survival: ticks between two building rounds (place a block, break it again).
const BUILD_EVERY: u64 = 200;
/// Survival: ticks a bot digs before it finishes breaking (dirt by hand takes 15).
const DIG_TICKS: u64 = 24;
/// Survival: mobs within this many blocks are chased, within `ATTACK_RANGE` hit.
const CHASE_RANGE: f64 = 16.0;
const ATTACK_RANGE: f64 = 2.5;
/// Survival: ticks between two hits (a sword's attack cooldown is 12.5).
const ATTACK_EVERY: u64 = 13;
/// Survival: higher ground than this is a wall, a drop deeper than `MAX_DROP` a cliff; steps
/// up are climbed by `CLIMB_STEP` per tick.
const MAX_CLIMB: f64 = 1.25;
const MAX_DROP: f64 = 3.0;
const CLIMB_STEP: f64 = 0.42;

/// The ground of one chunk: the `MOTION_BLOCKING` heights by `x + 16 * z`, as block y above
/// [`MIN_Y`].
type Ground = Box<[u16; 256]>;

type BotResult<T> = Result<T, String>;

/// Runs one bot until it is disconnected; records the outcome in `metrics`.
pub async fn run(
    index: u64,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    movement: watch::Receiver<Movement>,
    mut say: watch::Receiver<String>,
    permit: OwnedSemaphorePermit,
) {
    // Only what is said after the bot started.
    say.mark_unchanged();
    let mut bot = Bot {
        index,
        movement,
        say,
        name: format!("{}{index:05}", config.prefix),
        rng: Rng::new(config.seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
        config,
        metrics,
        conn: None,
    };
    let mut joined = false;
    let mut permit = Some(permit);
    let result = bot.play(&mut joined, &mut permit).await;
    let reason = match result {
        Ok(()) => "closed".to_owned(),
        Err(e) => e,
    };
    if joined {
        bot.metrics.online.fetch_sub(1, Ordering::Relaxed);
        bot.metrics.disconnected.fetch_add(1, Ordering::Relaxed);
    } else {
        bot.metrics.failed.fetch_add(1, Ordering::Relaxed);
    }
    bot.metrics.error(reason);
}

struct Bot {
    index: u64,
    movement: watch::Receiver<Movement>,
    /// Text to send once (`say` on stdin): a chat message or, with a leading `/`, a command.
    say: watch::Receiver<String>,
    name: String,
    rng: Rng,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    conn: Option<Conn>,
}

/// What the play loop does with an incoming packet.
enum Action {
    KeepAlive(i64),
    Ping(i32),
    Teleport(PlayerPosition),
    BatchFinished,
    Chunk(i32, i32, Option<Ground>),
    Forget(i32, i32),
    Pong(i64),
    Disconnect(String),
    StartConfiguration,
    /// An entity appeared (id, position, whether it is a mob).
    Entity(i32, [f64; 3], bool),
    EntityMoved(i32, [f64; 3]),
    EntityAt(i32, [f64; 3]),
    EntitiesRemoved(Vec<i32>),
    Health(f32),
    Respawned,
}

/// A survival bot's building round: place a block next to it, then dig it away.
#[derive(Debug, Clone, Copy)]
struct Build {
    /// The block the bot placed and digs (its column's ground rose by one).
    pos: BlockPos,
    /// Tick of the placement; digging starts 2 ticks later and ends `DIG_TICKS` after that.
    at: u64,
}

/// An entity a survival bot sees.
struct Seen {
    pos: [f64; 3],
    mob: bool,
}

impl Bot {
    /// Connects to the server. Against a loopback IPv4 host each bot uses one of
    /// `source_ips` local addresses (`127.0.0.2` ...), as players come from many addresses:
    /// the server's per-address limit of pending logins then does not cap the join rate.
    async fn connect(&self) -> BotResult<TcpStream> {
        let config = &self.config;
        let host = config.host.parse::<std::net::Ipv4Addr>().ok();
        let stream = match host.filter(|h| h.is_loopback() && config.source_ips > 0) {
            Some(host) => {
                let n = u64::from(config.source_ips);
                let k = u32::try_from(self.index % n).unwrap_or(0) + 2;
                let source = std::net::Ipv4Addr::from(0x7F00_0000 | k);
                let socket = tokio::net::TcpSocket::new_v4().map_err(|e| format!("socket: {e}"))?;
                socket
                    .bind((source, 0).into())
                    .map_err(|e| format!("bind {source}: {e}"))?;
                socket.connect((host, config.port).into()).await
            }
            None => TcpStream::connect((config.host.as_str(), config.port)).await,
        };
        stream.map_err(|e| format!("connect: {e}"))
    }

    async fn play(
        &mut self,
        joined: &mut bool,
        permit: &mut Option<OwnedSemaphorePermit>,
    ) -> BotResult<()> {
        let started = Instant::now();
        let stream = self.connect().await?;
        stream
            .set_nodelay(true)
            .map_err(|e| format!("nodelay: {e}"))?;
        let conn = self
            .conn
            .insert(Conn::new(stream, Arc::clone(&self.metrics)));
        conn.login(&self.name, &self.config).await?;

        // Play: answer what must be answered, walk once the first chunk is there.
        let mut pos: Option<(f64, f64, f64)> = None;
        let mut home = (0.0, 0.0);
        // The next server teleport sets home (after a respawn).
        let mut home_reset = false;
        let mut yaw: f64 = self.rng.next_f64() * 360.0;
        let mut movement = *self.movement.borrow_and_update();
        let mut next_turn = turn_deadline(movement);
        // Spread: the point this bot flies to (picked when the mode starts).
        let mut goal: Option<(f64, f64)> = None;
        let mut loaded: HashSet<(i32, i32)> = HashSet::new();
        // The ground of the chunks the random walk can reach: bots walk on the terrain
        // instead of into it, which the server would answer with a setback.
        let mut ground: HashMap<(i32, i32), Ground> = HashMap::new();
        // Full view: every chunk within the view distance (a circle) has arrived.
        let mut full_view = false;
        let view = i32::from(self.config.view_distance.max(1));
        // Chunks that entered the view after the full view and did not arrive yet, with the
        // time they entered; the chunk the bot stood in when they were last updated.
        let mut wanted: HashMap<(i32, i32), Instant> = HashMap::new();
        let mut wanted_center: Option<(i32, i32)> = None;
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(self.rng.next_u64() % 50),
            Duration::from_millis(50),
        );
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut probe = tokio::time::interval(PROBE_INTERVAL);
        let chat_every = (self.config.chat_interval > 0.0)
            .then(|| Duration::from_secs_f64(self.config.chat_interval));
        let mut next_chat = tokio::time::Instant::now()
            + chat_every.map_or(Duration::ZERO, |d| d.mul_f64(self.rng.next_f64()));
        let mut actions = Vec::new();
        // Survival: entities in view, the running building round, tick counters.
        let mut seen: HashMap<i32, Seen> = HashMap::new();
        let mut build: Option<Build> = None;
        let mut ticks: u64 = 0;
        let mut next_build: u64 = BUILD_EVERY / 2 + self.rng.next_u64() % BUILD_EVERY;
        let mut next_attack: u64 = 0;
        let mut sequence: i32 = 0;
        let mut dead = false;
        loop {
            tokio::select! {
                ready = conn.stream.readable() => {
                    ready.map_err(|e| format!("read: {e}"))?;
                    conn.fill()?;
                    conn.drain_play(&mut actions, movement == Movement::Survival)?;
                    for action in actions.drain(..) {
                        match action {
                            Action::KeepAlive(id) => {
                                let mut b = body(ids::play::serverbound::KEEP_ALIVE);
                                id.encode(&mut b);
                                conn.queue_body(&b)?;
                            }
                            Action::Ping(id) => {
                                let mut b = body(ids::play::serverbound::PONG);
                                id.encode(&mut b);
                                conn.queue_body(&b)?;
                            }
                            Action::Teleport(t) => {
                                let p = t.change.position;
                                conn.queue(&AcceptTeleportation {
                                    id: t.teleport_id,
                                    x: p.x,
                                    y: p.y,
                                    z: p.z,
                                    y_rot: t.change.y_rot,
                                    x_rot: t.change.x_rot,
                                })?;
                                if pos.is_none() || home_reset {
                                    home = (p.x, p.z);
                                    home_reset = false;
                                } else if *joined {
                                    self.metrics.teleports.fetch_add(1, Ordering::Relaxed);
                                }
                                pos = Some((p.x, p.y, p.z));
                            }
                            Action::BatchFinished => conn.queue(&ChunkBatchReceived {
                                desired_chunks_per_tick: 64.0,
                            })?,
                            Action::Forget(cx, cz) => {
                                loaded.remove(&(cx, cz));
                                ground.remove(&(cx, cz));
                                self.metrics.chunks_forgotten.fetch_add(1, Ordering::Relaxed);
                            }
                            Action::Chunk(cx, cz, heights) => {
                                // Chunks within the walk radius around home (the walk turns
                                // back at the radius, so it reaches a bit beyond).
                                let reach = self.config.radius + 32.0;
                                let (dx, dz) = (
                                    (f64::from(cx) * 16.0 + 8.0 - home.0).abs(),
                                    (f64::from(cz) * 16.0 + 8.0 - home.1).abs(),
                                );
                                if let Some(heights) = heights
                                    && dx < reach
                                    && dz < reach
                                {
                                    ground.insert((cx, cz), heights);
                                }
                                if let Some(since) = wanted.remove(&(cx, cz))
                                    && let Ok(mut v) = self.metrics.chunk_ms.lock()
                                {
                                    v.push(since.elapsed().as_secs_f64() * 1e3);
                                }
                                loaded.insert((cx, cz));
                                self.metrics.chunks.fetch_add(1, Ordering::Relaxed);
                            }
                            Action::Pong(sent) => {
                                let now = micros_since(started);
                                let ms = now.saturating_sub(u64::try_from(sent).unwrap_or(0)) as f64 / 1e3;
                                if let Ok(mut v) = self.metrics.rtt_ms.lock() {
                                    v.push(ms);
                                }
                            }
                            Action::Disconnect(reason) => return Err(format!("kicked: {reason}")),
                            Action::StartConfiguration => {
                                return Err("server restarted configuration".into());
                            }
                            Action::Entity(id, pos, mob) => {
                                seen.insert(id, Seen { pos, mob });
                            }
                            Action::EntityMoved(id, d) => {
                                if let Some(e) = seen.get_mut(&id) {
                                    (0..3).for_each(|i| e.pos[i] += d[i]);
                                }
                            }
                            Action::EntityAt(id, pos) => {
                                if let Some(e) = seen.get_mut(&id) {
                                    e.pos = pos;
                                }
                            }
                            Action::EntitiesRemoved(ids) => {
                                for id in ids {
                                    seen.remove(&id);
                                }
                            }
                            Action::Health(health) => {
                                if health <= 0.0 && !dead {
                                    dead = true;
                                    build = None;
                                    self.metrics.deaths.fetch_add(1, Ordering::Relaxed);
                                    conn.queue(&ClientCommand {
                                        action: ClientCommandAction::PerformRespawn,
                                    })?;
                                } else if health > 0.0 {
                                    dead = false;
                                }
                            }
                            Action::Respawned => {
                                seen.clear();
                                // The walk continues around the respawn point.
                                home_reset = true;
                            }
                        }
                    }
                    // In the world once the position and a first chunk are there, in either
                    // order (Paper may send chunks before the position).
                    if !*joined && pos.is_some() && !loaded.is_empty() {
                        *joined = true;
                        permit.take();
                        let ms = started.elapsed().as_secs_f64() * 1e3;
                        if let Ok(mut v) = self.metrics.join_ms.lock() {
                            v.push(ms);
                        }
                        self.metrics.joined.fetch_add(1, Ordering::Relaxed);
                        self.metrics.online.fetch_add(1, Ordering::Relaxed);
                        conn.queue(&PlayerLoaded)?;
                    }
                    if *joined
                        && !full_view
                        && let Some((x, _, z)) = pos
                    {
                        let (cx, cz) = ((x / 16.0).floor() as i32, (z / 16.0).floor() as i32);
                        full_view = (-view..=view).all(|dz| {
                            (-view..=view).all(|dx| {
                                dx * dx + dz * dz > view * view
                                    || loaded.contains(&(cx + dx, cz + dz))
                            })
                        });
                        if full_view && let Ok(mut v) = self.metrics.full_view_ms.lock() {
                            v.push(started.elapsed().as_secs_f64() * 1e3);
                        }
                    }
                    conn.flush().await?;
                }
                _ = tick.tick(), if *joined => {
                    ticks += 1;
                    let Some((x, y, z)) = pos.as_mut() else { continue };
                    let center = ((*x / 16.0).floor() as i32, (*z / 16.0).floor() as i32);
                    if full_view && wanted_center != Some(center) {
                        let inside = |c: (i32, i32), p: (i32, i32)| {
                            let (dx, dz) = (p.0 - c.0, p.1 - c.1);
                            dx * dx + dz * dz <= view * view
                        };
                        // Chunks that left the view unsent count as waited until now.
                        let now = Instant::now();
                        wanted.retain(|&p, since| {
                            let keep = inside(center, p);
                            if !keep && let Ok(mut v) = self.metrics.chunk_ms.lock() {
                                v.push(now.duration_since(*since).as_secs_f64() * 1e3);
                            }
                            keep
                        });
                        for dz in -view..=view {
                            for dx in -view..=view {
                                let p = (center.0 + dx, center.1 + dz);
                                if inside(center, p)
                                    && !loaded.contains(&p)
                                    && wanted_center.is_none_or(|old| !inside(old, p))
                                {
                                    wanted.entry(p).or_insert(now);
                                }
                            }
                        }
                        wanted_center = Some(center);
                    }
                    if self.movement.has_changed().unwrap_or(false) {
                        movement = *self.movement.borrow_and_update();
                        next_turn = turn_deadline(movement);
                        home = (*x, *z);
                        yaw = self.rng.next_f64() * 360.0;
                        goal = None;
                    }
                    let speed = match movement {
                        Movement::Still => 0.0,
                        Movement::Spread { radius } => {
                            let (gx, gz) = *goal.get_or_insert_with(|| {
                                // Uniform in the disk around the point where the mode started.
                                let r = radius * self.rng.next_f64().sqrt();
                                let a = self.rng.next_f64() * std::f64::consts::TAU;
                                (*x + r * a.cos(), *z + r * a.sin())
                            });
                            let (dx, dz) = (gx - *x, gz - *z);
                            let left = (dx * dx + dz * dz).sqrt();
                            yaw = (-dx).atan2(dz).to_degrees();
                            climb(y, (SPREAD_SPEED / 20.0).min(left))
                        }
                        Movement::Survival => {
                            // Dead bots wait for the respawn; building and fighting bots
                            // stand.
                            let mut speed = WALK_SPEED;
                            if dead {
                                speed = 0.0;
                            } else if let Some(b) = build {
                                speed = 0.0;
                                let dig = BlockPos { x: b.pos.x, y: b.pos.y, z: b.pos.z };
                                if ticks == b.at + 2 {
                                    conn.queue(&PlayerActionPacket {
                                        action: DigAction::StartDestroyBlock,
                                        pos: dig,
                                        direction: 1,
                                        sequence: next(&mut sequence),
                                    })?;
                                } else if ticks >= b.at + 2 + DIG_TICKS {
                                    conn.queue(&PlayerActionPacket {
                                        action: DigAction::StopDestroyBlock,
                                        pos: dig,
                                        direction: 1,
                                        sequence: next(&mut sequence),
                                    })?;
                                    lower(&mut ground, b.pos);
                                    self.metrics.broken.fetch_add(1, Ordering::Relaxed);
                                    build = None;
                                }
                            } else if let Some((id, d, ex, ez)) = nearest_mob(&seen, (*x, *y, *z))
                                && d < CHASE_RANGE
                                && (ex - home.0).hypot(ez - home.1) < self.config.radius + CHASE_RANGE
                            {
                                yaw = (-(ex - *x)).atan2(ez - *z).to_degrees();
                                if d < ATTACK_RANGE {
                                    speed = 0.0;
                                    if ticks >= next_attack {
                                        next_attack = ticks + ATTACK_EVERY;
                                        conn.queue(&Attack { entity_id: id })?;
                                        self.metrics.attacks.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            } else if ticks >= next_build {
                                next_build = ticks + BUILD_EVERY;
                                // The column two blocks ahead: out of the bot's box, in reach.
                                let rad = yaw.to_radians();
                                let bx = (*x - rad.sin() * 2.0).floor() as i32;
                                let bz = (*z + rad.cos() * 2.0).floor() as i32;
                                if let Some(top) = raise(&mut ground, bx, bz) {
                                    speed = 0.0;
                                    conn.queue(&UseItemOn {
                                        hand: Hand::MainHand,
                                        pos: BlockPos { x: bx, y: top - 1, z: bz },
                                        face: 1,
                                        cursor_x: 0.5,
                                        cursor_y: 1.0,
                                        cursor_z: 0.5,
                                        inside: false,
                                        world_border_hit: false,
                                        sequence: next(&mut sequence),
                                    })?;
                                    self.metrics.placed.fetch_add(1, Ordering::Relaxed);
                                    build = Some(Build {
                                        pos: BlockPos { x: bx, y: top, z: bz },
                                        at: ticks,
                                    });
                                }
                            } else {
                                // Drift the heading, turn back home outside the radius.
                                let (dx, dz) = (*x - home.0, *z - home.1);
                                if dx * dx + dz * dz > self.config.radius * self.config.radius {
                                    yaw = (-dx).atan2(dz).to_degrees()
                                        + 180.0
                                        + self.rng.range(-30.0, 30.0);
                                } else {
                                    yaw += self.rng.range(-15.0, 15.0);
                                }
                            }
                            speed
                        }
                        Movement::Random => {
                            // Drift the heading, turn back home outside the radius.
                            let (dx, dz) = (*x - home.0, *z - home.1);
                            if dx * dx + dz * dz > self.config.radius * self.config.radius {
                                yaw = (-dx).atan2(dz).to_degrees()
                                    + 180.0
                                    + self.rng.range(-30.0, 30.0);
                            } else {
                                yaw += self.rng.range(-15.0, 15.0);
                            }
                            WALK_SPEED
                        }
                        Movement::Straight { speed, turn } => {
                            if let Some(at) = next_turn
                                && tokio::time::Instant::now() >= at
                            {
                                next_turn = Some(at + Duration::from_secs_f64(turn));
                                yaw = self.rng.next_f64() * 360.0;
                            }
                            climb(y, speed / 20.0)
                        }
                    };
                    if self.say.has_changed().unwrap_or(false) {
                        let text = self.say.borrow_and_update().clone();
                        match text.strip_prefix('/') {
                            Some(command) => {
                                let mut b = body(ids::play::serverbound::CHAT_COMMAND);
                                write_str(&mut b, command);
                                conn.queue_body(&b)?;
                            }
                            None => conn.queue_body(&chat_body(&text))?,
                        }
                    }
                    if movement == Movement::Still {
                        conn.queue(&ClientTickEnd)?;
                        conn.flush().await?;
                        continue;
                    }
                    let rad = yaw.to_radians();
                    let (old_x, old_z) = (*x, *z);
                    *x -= rad.sin() * speed;
                    *z += rad.cos() * speed;
                    if movement == Movement::Survival
                        && let Some(new) = ground_at(&ground, *x, *z)
                        && (new > *y + MAX_CLIMB || new < *y - MAX_DROP || new > *y + 1e-6)
                    {
                        // Walls and cliffs turn the bot around; a step up is climbed first,
                        // straight up, then walked onto.
                        *x = old_x;
                        *z = old_z;
                        if new > *y + MAX_CLIMB || new < *y - MAX_DROP {
                            yaw += 180.0 + self.rng.range(-45.0, 45.0);
                        } else {
                            *y = new.min(*y + CLIMB_STEP);
                        }
                    } else if matches!(movement, Movement::Random | Movement::Survival)
                        && let Some(new) = ground_at(&ground, *x, *z)
                    {
                        // Up at once; down only once the box left the higher ground, as
                        // the server moves along y first, at the old position.
                        *y = match ground_at(&ground, old_x, old_z) {
                            Some(old) if new < *y => new.max(old.min(*y)),
                            _ => new,
                        };
                    }
                    conn.queue(&MovePlayerPosRot {
                        x: *x,
                        y: *y,
                        z: *z,
                        y_rot: (yaw.rem_euclid(360.0)) as f32,
                        x_rot: 0.0,
                        flags: MoveFlags {
                            on_ground: matches!(movement, Movement::Random | Movement::Survival),
                            horizontal_collision: false,
                        },
                    })?;
                    conn.queue(&ClientTickEnd)?;
                    if let Some(every) = chat_every
                        && tokio::time::Instant::now() >= next_chat
                    {
                        next_chat += every;
                        let n = self.rng.next_u64() % 1000;
                        conn.queue_body(&chat_body(&format!("hello from {} #{n}", self.name)))?;
                    }
                    conn.flush().await?;
                }
                _ = probe.tick(), if *joined => {
                    if let Some((x, _, z)) = pos {
                        let (cx, cz) = ((x / 16.0).floor() as i32, (z / 16.0).floor() as i32);
                        let mut missing = 0;
                        for dz in -NEAR..=NEAR {
                            for dx in -NEAR..=NEAR {
                                missing += u64::from(!loaded.contains(&(cx + dx, cz + dz)));
                            }
                        }
                        self.metrics.near_missing.fetch_add(missing, Ordering::Relaxed);
                        self.metrics.near_checked.fetch_add(
                            ((2 * NEAR + 1) * (2 * NEAR + 1)) as u64,
                            Ordering::Relaxed,
                        );
                    }
                    let mut b = body(ids::play::serverbound::PING_REQUEST);
                    i64::try_from(micros_since(started)).unwrap_or(i64::MAX).encode(&mut b);
                    conn.queue_body(&b)?;
                    conn.flush().await?;
                }
            }
        }
    }
}

/// The next block action sequence number.
fn next(sequence: &mut i32) -> i32 {
    *sequence = sequence.wrapping_add(1);
    *sequence
}

/// The nearest mob: id, distance and horizontal position.
fn nearest_mob(
    seen: &HashMap<i32, Seen>,
    (x, y, z): (f64, f64, f64),
) -> Option<(i32, f64, f64, f64)> {
    seen.iter()
        .filter(|(_, e)| e.mob)
        .map(|(&id, e)| {
            let d =
                ((e.pos[0] - x).powi(2) + (e.pos[1] - y).powi(2) + (e.pos[2] - z).powi(2)).sqrt();
            (id, d, e.pos[0], e.pos[2])
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// Raises the known ground of block column `x`, `z` by one (a block placed on top); returns
/// the y of the new block, `None` if the chunk is not known.
fn raise(ground: &mut HashMap<(i32, i32), Ground>, x: i32, z: i32) -> Option<i32> {
    let heights = ground.get_mut(&(x >> 4, z >> 4))?;
    let h = &mut heights[((x & 15) + 16 * (z & 15)) as usize];
    let top = MIN_Y as i32 + i32::from(*h);
    *h += 1;
    Some(top)
}

/// Lowers the known ground of the column of `pos` back to below `pos` (the block was broken).
fn lower(ground: &mut HashMap<(i32, i32), Ground>, pos: BlockPos) {
    if let Some(heights) = ground.get_mut(&(pos.x >> 4, pos.z >> 4)) {
        let h = u16::try_from(pos.y - MIN_Y as i32).unwrap_or(0);
        let cell = &mut heights[((pos.x & 15) + 16 * (pos.z & 15)) as usize];
        *cell = (*cell).min(h);
    }
}

/// Sum of the steps of a `move_entity_pos(_rot)` delta in blocks.
fn delta_blocks(delta: &VecDelta) -> [f64; 3] {
    let steps: &[[i16; 3]] = match delta {
        VecDelta::Linear(d) => std::slice::from_ref(d),
        VecDelta::Stepped(s) => {
            return s.iter().fold([0.0; 3], |mut a, (_, d)| {
                (0..3).for_each(|i| a[i] += f64::from(d[i]) / 4096.0);
                a
            });
        }
    };
    let d = steps[0];
    [
        f64::from(d[0]) / 4096.0,
        f64::from(d[1]) / 4096.0,
        f64::from(d[2]) / 4096.0,
    ]
}

/// The highest ground under a player's box at `x`, `z` (`None`: a chunk is not known).
fn ground_at(ground: &HashMap<(i32, i32), Ground>, x: f64, z: f64) -> Option<f64> {
    let mut top = 0u16;
    for (dx, dz) in [(-1.0, -1.0), (-1.0, 1.0), (1.0, -1.0), (1.0, 1.0)] {
        let bx = (x + dx * HALF_WIDTH).floor() as i32;
        let bz = (z + dz * HALF_WIDTH).floor() as i32;
        let heights = ground.get(&(bx >> 4, bz >> 4))?;
        top = top.max(heights[((bx & 15) + 16 * (bz & 15)) as usize]);
    }
    Some(MIN_Y + f64::from(top))
}

/// Reads the `MOTION_BLOCKING` heightmap from the rest of `level_chunk_with_light` after the
/// position (`None` if it is missing or malformed).
fn read_ground(r: &mut Reader<'_>) -> Option<Ground> {
    let mut found = None;
    for _ in 0..r.varint().ok()? {
        let kind = r.varint().ok()?;
        let longs = usize::try_from(r.varint().ok()?).ok()?;
        if kind != MOTION_BLOCKING {
            for _ in 0..longs {
                r.i64().ok()?;
            }
            continue;
        }
        // Values do not span longs (`SimpleBitStorage`).
        let per_long = 256usize.div_ceil(longs.max(1));
        let bits = 64 / per_long;
        let mask = (1u64 << bits) - 1;
        let mut heights = Box::new([0u16; 256]);
        for l in 0..longs {
            let v = r.i64().ok()? as u64;
            for k in 0..per_long {
                if let Some(h) = heights.get_mut(l * per_long + k) {
                    *h = ((v >> (k * bits)) & mask) as u16;
                }
            }
        }
        found = Some(heights);
    }
    found
}

/// Moves `y` towards the cruise altitude; returns the horizontal speed: none while climbing,
/// so bots rise straight up out of the terrain instead of flying through hills (which servers
/// that check movement reject).
fn climb(y: &mut f64, speed: f64) -> f64 {
    let step = (FLIGHT_Y - *y).clamp(-CLIMB_SPEED, CLIMB_SPEED);
    *y += step;
    if step.abs() < CLIMB_SPEED { speed } else { 0.0 }
}

/// When straight flight turns next (`None`: never).
fn turn_deadline(movement: Movement) -> Option<tokio::time::Instant> {
    match movement {
        Movement::Straight { turn, .. } if turn > 0.0 => {
            Some(tokio::time::Instant::now() + Duration::from_secs_f64(turn))
        }
        _ => None,
    }
}

/// A packet body starting with its id.
fn body(id: i32) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    write_varint(&mut b, id);
    b
}

/// An unsigned `chat` message: message, timestamp, salt, no signature, no acknowledgements.
fn chat_body(message: &str) -> Vec<u8> {
    let mut b = body(ids::play::serverbound::CHAT);
    write_str(&mut b, message);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0));
    millis.encode(&mut b);
    0i64.encode(&mut b);
    false.encode(&mut b);
    write_varint(&mut b, 0);
    b.extend_from_slice(&[0, 0, 0]);
    b.push(0);
    b
}

fn micros_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// The connection with framing, compression and byte counters.
struct Conn {
    stream: TcpStream,
    decoder: FrameDecoder,
    encoder: FrameEncoder,
    out: Vec<u8>,
    metrics: Arc<Metrics>,
}

impl Conn {
    fn new(stream: TcpStream, metrics: Arc<Metrics>) -> Self {
        let mut decoder = FrameDecoder::new();
        decoder.set_limits(State::Play.limits());
        Self {
            stream,
            decoder,
            encoder: FrameEncoder::new(),
            out: Vec::new(),
            metrics,
        }
    }

    fn queue<P: Packet>(&mut self, packet: &P) -> BotResult<()> {
        self.encoder
            .packet(packet, &mut self.out)
            .map_err(|e| format!("encode {}: {e}", P::NAME))?;
        self.metrics.packets_out.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn queue_body(&mut self, body: &[u8]) -> BotResult<()> {
        self.encoder
            .body(body, &mut self.out)
            .map_err(|e| format!("encode: {e}"))?;
        self.metrics.packets_out.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn flush(&mut self) -> BotResult<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        self.stream
            .write_all(&self.out)
            .await
            .map_err(|e| format!("write: {e}"))?;
        self.metrics
            .bytes_out
            .fetch_add(self.out.len() as u64, Ordering::Relaxed);
        self.out.clear();
        Ok(())
    }

    /// Reads what the socket has without waiting.
    fn fill(&mut self) -> BotResult<()> {
        loop {
            let spare = self.decoder.spare(READ_CHUNK);
            match self.stream.try_read(spare) {
                Ok(0) => {
                    self.decoder.commit(READ_CHUNK, 0);
                    return Err("connection closed".into());
                }
                Ok(n) => {
                    self.decoder.commit(READ_CHUNK, n);
                    self.metrics.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
                    if n < READ_CHUNK {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    self.decoder.commit(READ_CHUNK, 0);
                    return Ok(());
                }
                Err(e) => {
                    self.decoder.commit(READ_CHUNK, 0);
                    return Err(format!("read: {e}"));
                }
            }
        }
    }

    /// The next frame, waiting for the socket if needed (login and configuration only).
    async fn next_frame(&mut self) -> BotResult<Vec<u8>> {
        loop {
            if let Some(frame) = self
                .decoder
                .next_frame()
                .map_err(|e| format!("frame: {e}"))?
            {
                self.metrics.packets_in.fetch_add(1, Ordering::Relaxed);
                return Ok(frame.to_vec());
            }
            self.stream
                .readable()
                .await
                .map_err(|e| format!("read: {e}"))?;
            self.fill()?;
        }
    }

    /// Handshake, login (offline mode) and configuration; ends in the play state.
    async fn login(&mut self, name: &str, config: &Config) -> BotResult<()> {
        self.queue(&Intention {
            protocol_version: vanillite_protocol::PROTOCOL_VERSION,
            host: config.host.clone(),
            port: config.port,
            intent: Intent::Login,
        })?;
        self.queue(&Hello {
            name: name.into(),
            profile_id: Uuid::offline(name),
        })?;
        self.flush().await?;
        loop {
            let frame = self.next_frame().await?;
            match ClientboundLogin::decode(&frame).map_err(|e| format!("login: {e}"))? {
                ClientboundLogin::LoginCompression(c) => {
                    self.encoder.set_compression(c.threshold);
                    self.decoder.set_compression(c.threshold);
                }
                ClientboundLogin::LoginFinished(_) => {
                    self.queue(&LoginAcknowledged)?;
                    break;
                }
                ClientboundLogin::LoginDisconnect(d) => {
                    return Err(format!("login refused: {}", d.reason.0.plain()));
                }
                ClientboundLogin::EncryptionRequest(_) => {
                    return Err("server is in online mode".into());
                }
                ClientboundLogin::CustomQuery(q) => self.queue(&CustomQueryAnswer {
                    transaction_id: q.transaction_id,
                    payload: None,
                })?,
                ClientboundLogin::CookieRequest(c) => self.queue(&CookieResponse {
                    key: c.key,
                    payload: None,
                })?,
            }
            self.flush().await?;
        }

        self.queue(&csb::ClientInformation(ClientInfo {
            view_distance: config.view_distance,
            ..ClientInfo::default()
        }))?;
        self.flush().await?;
        use ids::configuration::clientbound as id;
        loop {
            let frame = self.next_frame().await?;
            let packet_id = Reader::new(&frame)
                .varint()
                .map_err(|e| format!("id: {e}"))?;
            // Registries, tags and the rest are only counted.
            if !matches!(
                packet_id,
                id::SELECT_KNOWN_PACKS
                    | id::KEEP_ALIVE
                    | id::PING
                    | id::CODE_OF_CONDUCT
                    | id::DISCONNECT
                    | id::FINISH_CONFIGURATION
                    | id::COOKIE_REQUEST
            ) {
                continue;
            }
            match ClientboundConfiguration::decode(&frame)
                .map_err(|e| format!("configuration: {e}"))?
            {
                ClientboundConfiguration::SelectKnownPacks(p) => {
                    self.queue(&csb::SelectKnownPacks { packs: p.packs })?;
                }
                ClientboundConfiguration::KeepAlive(k) => {
                    self.queue(&csb::KeepAlive { id: k.id })?
                }
                ClientboundConfiguration::Ping(p) => self.queue(&csb::Pong { id: p.id })?,
                ClientboundConfiguration::CodeOfConduct(_) => {
                    self.queue(&csb::AcceptCodeOfConduct)?
                }
                ClientboundConfiguration::CookieRequest(c) => self.queue(&csb::CookieResponse {
                    key: c.key,
                    payload: None,
                })?,
                ClientboundConfiguration::Disconnect(d) => {
                    return Err(format!("configuration refused: {}", d.reason.plain()));
                }
                ClientboundConfiguration::FinishConfiguration(_) => {
                    self.queue(&csb::FinishConfiguration)?;
                    self.flush().await?;
                    return Ok(());
                }
                _ => {}
            }
            self.flush().await?;
        }
    }

    /// Decodes the buffered play packets into actions; the rest is only counted. Entity
    /// packets only with `entities` (survival bots).
    fn drain_play(&mut self, actions: &mut Vec<Action>, entities: bool) -> BotResult<()> {
        // Counted once per call: the shared counter costs more than decoding at these rates.
        let mut packets = 0u64;
        let drained = self.drain_frames(actions, entities, &mut packets);
        self.metrics
            .packets_in
            .fetch_add(packets, Ordering::Relaxed);
        drained.map_err(|e| format!("play: {e}"))
    }

    fn drain_frames(
        &mut self,
        actions: &mut Vec<Action>,
        entities: bool,
        packets: &mut u64,
    ) -> Result<(), vanillite_protocol::Error> {
        use ids::play::clientbound as id;
        while let Some(frame) = self.decoder.next_frame()? {
            *packets += 1;
            let mut r = Reader::new(frame);
            let action = match r.varint()? {
                id::KEEP_ALIVE => Action::KeepAlive(r.i64()?),
                id::PING => Action::Ping(r.i32()?),
                id::PONG_RESPONSE => Action::Pong(r.i64()?),
                id::PLAYER_POSITION => Action::Teleport(PlayerPosition::decode(&mut r)?),
                id::CHUNK_BATCH_FINISHED => Action::BatchFinished,
                id::LEVEL_CHUNK_WITH_LIGHT => {
                    let (x, z) = (r.i32()?, r.i32()?);
                    Action::Chunk(x, z, read_ground(&mut r))
                }
                id::FORGET_LEVEL_CHUNK => {
                    // `ChunkPos` as one long: x in the low, z in the high 32 bits.
                    let packed = r.i64()?;
                    Action::Forget(packed as i32, (packed >> 32) as i32)
                }
                id::START_CONFIGURATION => Action::StartConfiguration,
                id::SET_HEALTH => Action::Health(r.f32()?),
                id::RESPAWN => Action::Respawned,
                id::ADD_ENTITY if entities => {
                    let e = AddEntity::decode(&mut r)?;
                    // Mobs: types with attributes (living), except players.
                    let mob = usize::try_from(e.entity_type)
                        .ok()
                        .and_then(|t| ENTITY_TYPES.get(t))
                        .is_some_and(|t| !t.attributes.is_empty() && t.name != "minecraft:player");
                    let p = e.position;
                    Action::Entity(e.entity_id, [p.x, p.y, p.z], mob)
                }
                id::MOVE_ENTITY_POS if entities => {
                    let m = MoveEntityPos::decode(&mut r)?;
                    Action::EntityMoved(m.entity_id, delta_blocks(&m.delta))
                }
                id::MOVE_ENTITY_POS_ROT if entities => {
                    let m = MoveEntityPosRot::decode(&mut r)?;
                    Action::EntityMoved(m.entity_id, delta_blocks(&m.delta))
                }
                id::ENTITY_POSITION_SYNC if entities => {
                    let s = EntityPositionSync::decode(&mut r)?;
                    let p = match s.path {
                        PositionPath::Linear(p) => p,
                        PositionPath::Stepped(steps) => match steps.last() {
                            Some((p, _)) => *p,
                            None => continue,
                        },
                    };
                    Action::EntityAt(s.entity_id, [p.x, p.y, p.z])
                }
                id::TELEPORT_ENTITY if entities => {
                    let t = TeleportEntity::decode(&mut r)?;
                    // Relative teleports are rare; the next sync corrects them.
                    if t.relatives & 0b111 != 0 {
                        continue;
                    }
                    let p = t.change.position;
                    Action::EntityAt(t.entity_id, [p.x, p.y, p.z])
                }
                id::REMOVE_ENTITIES if entities => {
                    Action::EntitiesRemoved(RemoveEntities::decode(&mut r)?.ids.0)
                }
                id::DISCONNECT => Action::Disconnect(
                    vanillite_protocol::packets::play::clientbound::Disconnect::decode(&mut r)
                        .map_or_else(|_| "?".to_owned(), |d| d.reason.plain()),
                ),
                _ => continue,
            };
            actions.push(action);
        }
        Ok(())
    }
}

/// SplitMix64: deterministic per bot, so runs are comparable.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, min: f64, max: f64) -> f64 {
        min + self.next_f64() * (max - min)
    }
}
