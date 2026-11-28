//! Load bot: many headless offline-mode clients in one process, one tokio task per bot.
//!
//! ```text
//! loadbot --host <host> --port <port> [--join-rate <per s>] [--join-concurrency <n>]
//!         [--source-ips <n>] [--radius <blocks>] [--chat-interval <s>] [--view-distance <chunks>]
//!         [--prefix <name>] [--seed <n>] [--movement <spec>] [--check]
//! ```
//!
//! Movement (`--movement`, see [`Movement::parse`]): `random` walks at walking speed within
//! the radius around the point where the mode started; `straight:<speed>[:<turn s>]` flies
//! straight in a random direction per bot at `<speed>` blocks per second (a number or a
//! preset: `walk`, `sprint`, `elytra`, `boat`), turning to a new random direction every
//! `<turn s>` seconds if given; `spread:<radius>` flies at boat speed to a random point within
//! the radius around the point where the mode started and hovers there; `still` stands where
//! the server put the bot (e.g. after a `/tp`); `survival` plays like a survival player near
//! the point where the mode started: it walks on the ground within the radius, every few
//! seconds places the block in its first hotbar slot next to it and breaks it again (give it
//! blocks, e.g. `/give @a dirt 64`), chases and attacks mobs near it and respawns after
//! death. Flight cruises at y = 300, above all overworld
//! terrain, with `on_ground` false: the server needs `allow-flight=true`. `--check` only
//! validates the arguments.
//!
//! Control on stdin, one command per line: `target <n>` raises the number of bots to `n`
//! (bots join at the join rate), `movement <spec>` switches the movement of all bots,
//! `rate <joins per s> [<concurrent logins>]` changes the join rate (the concurrency can only
//! grow), `say <text>` makes every joined bot send `<text>` once (a chat message, or a command
//! when it starts with `/`), `quit` ends the process. Every second one JSON line with the
//! metrics of that second goes to stdout (see [`Metrics::report`]). Driven by
//! `cargo xtask loadtest`.

mod bot;

use std::collections::HashMap;
use std::io::BufRead;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, watch};

/// Bot settings shared by all bots.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Bot joins started per second.
    pub join_rate: f64,
    /// Bots logging in at the same time (servers limit pending connections per address).
    pub join_concurrency: usize,
    /// Local addresses the bots spread over against a loopback host (0 = the system's
    /// choice): players come from many addresses, the server limits pending logins per address.
    pub source_ips: u32,
    /// Bots walk within this many blocks around their spawn point.
    pub radius: f64,
    /// Seconds between chat messages of one bot; 0 turns chat off.
    pub chat_interval: f64,
    /// View distance the bots send in their client information.
    pub view_distance: i8,
    pub prefix: String,
    pub seed: u64,
    /// Movement of the bots until a `movement` command changes it.
    pub movement: Movement,
}

/// How the bots move once they are in the world.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Movement {
    /// Random walk at walking speed within the radius around the point where it started.
    Random,
    /// Straight flight at `speed` blocks per second in a random direction per bot; a new
    /// random direction every `turn` seconds (0 = never).
    Straight { speed: f64, turn: f64 },
    /// Flight to a random point within `radius` blocks, then hovering there.
    Spread { radius: f64 },
    /// No movement at all.
    Still,
    /// Walking on the ground within the radius, building, breaking and fighting mobs.
    Survival,
}

impl Movement {
    /// Speed presets in blocks per second: walking, sprinting, elytra with rockets and a boat
    /// on blue ice.
    const PRESETS: [(&str, f64); 4] = [
        ("walk", 4.317),
        ("sprint", 5.612),
        ("elytra", 33.5),
        ("boat", 72.7),
    ];
    /// Highest speed: 7.5 blocks per tick plus the climb stays under Vanilla's "moved too
    /// quickly" limit of 10 blocks per movement packet.
    const MAX_SPEED: f64 = 150.0;

    /// `random`, `still`, `survival`, `spread:<radius>` or `straight:<speed or preset>[:<turn s>]`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "movement {spec:?}: expected random, still, survival, spread:<radius> or \
                 straight:<blocks per s|walk|sprint|elytra|boat>[:<turn s>] with a speed up to {}",
                Self::MAX_SPEED
            )
        };
        let parts: Vec<&str> = spec.split(':').collect();
        match parts.as_slice() {
            ["random"] => Ok(Self::Random),
            ["still"] => Ok(Self::Still),
            ["survival"] => Ok(Self::Survival),
            ["spread", radius] => radius
                .parse()
                .ok()
                .filter(|r: &f64| *r > 0.0 && r.is_finite())
                .map(|radius| Self::Spread { radius })
                .ok_or_else(bad),
            ["straight", speed, turn @ ..] if turn.len() <= 1 => {
                let speed = Self::PRESETS
                    .iter()
                    .find(|(name, _)| name == speed)
                    .map(|(_, v)| *v)
                    .or_else(|| speed.parse().ok())
                    .filter(|v: &f64| *v > 0.0 && *v <= Self::MAX_SPEED)
                    .ok_or_else(bad)?;
                let turn = match turn.first() {
                    Some(t) => t.parse().ok().filter(|t: &f64| *t >= 0.0).ok_or_else(bad)?,
                    None => 0.0,
                };
                Ok(Self::Straight { speed, turn })
            }
            _ => Err(bad()),
        }
    }
}

/// Counters shared by all bots; the reporter turns them into one JSON line per second.
#[derive(Debug, Default)]
pub struct Metrics {
    pub online: AtomicU64,
    pub joined: AtomicU64,
    pub failed: AtomicU64,
    pub disconnected: AtomicU64,
    /// Chunks received (`level_chunk_with_light`) and released (`forget_level_chunk`).
    pub chunks: AtomicU64,
    pub chunks_forgotten: AtomicU64,
    /// Server teleports after the join (rubber-banding of rejected moves).
    pub teleports: AtomicU64,
    /// Survival bots: attacks on mobs, blocks placed and broken, deaths.
    pub attacks: AtomicU64,
    pub placed: AtomicU64,
    pub broken: AtomicU64,
    pub deaths: AtomicU64,
    /// Chunks around the bots checked every probe interval, and how many of them were
    /// missing (holes near the players).
    pub near_checked: AtomicU64,
    pub near_missing: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub packets_in: AtomicU64,
    pub packets_out: AtomicU64,
    /// Join latencies (connect to first chunk) in milliseconds since the last report.
    pub join_ms: Mutex<Vec<f64>>,
    /// Times from connect until every chunk within the view distance arrived, in
    /// milliseconds since the last report.
    pub full_view_ms: Mutex<Vec<f64>>,
    /// Times from a chunk entering a bot's view (its position moved into the view distance)
    /// until it arrived, in milliseconds since the last report; after the bot had its full
    /// view once. Chunks that left the view before arriving count at the time they left.
    pub chunk_ms: Mutex<Vec<f64>>,
    /// Round trips of `ping_request` in milliseconds since the last report.
    pub rtt_ms: Mutex<Vec<f64>>,
    /// Failure and disconnect reasons since the last report.
    pub errors: Mutex<HashMap<String, u64>>,
}

impl Metrics {
    /// Records a failed join or a disconnect.
    pub fn error(&self, reason: String) {
        let mut reason = reason;
        reason.truncate(120);
        if let Ok(mut errors) = self.errors.lock() {
            *errors.entry(reason).or_insert(0) += 1;
        }
    }

    /// One report: cumulative counters, the samples since the last report and the CPU time
    /// of this process (clock ticks), so a saturated bot host can be detected.
    fn report(&self, start: Instant, target: u64) -> serde_json::Value {
        let take = |v: &Mutex<Vec<f64>>| {
            v.lock()
                .map(|mut v| std::mem::take(&mut *v))
                .unwrap_or_default()
        };
        let errors = self
            .errors
            .lock()
            .map(|mut e| std::mem::take(&mut *e))
            .unwrap_or_default();
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        serde_json::json!({
            "t": start.elapsed().as_secs_f64(),
            "target": target,
            "online": load(&self.online),
            "joined": load(&self.joined),
            "failed": load(&self.failed),
            "disconnected": load(&self.disconnected),
            "chunks": load(&self.chunks),
            "chunks_forgotten": load(&self.chunks_forgotten),
            "teleports": load(&self.teleports),
            "attacks": load(&self.attacks),
            "placed": load(&self.placed),
            "broken": load(&self.broken),
            "deaths": load(&self.deaths),
            "near_checked": load(&self.near_checked),
            "near_missing": load(&self.near_missing),
            "bytes_in": load(&self.bytes_in),
            "bytes_out": load(&self.bytes_out),
            "packets_in": load(&self.packets_in),
            "packets_out": load(&self.packets_out),
            "cpu_ticks": process_cpu_ticks(),
            "join_ms": take(&self.join_ms),
            "full_view_ms": take(&self.full_view_ms),
            "chunk_ms": take(&self.chunk_ms),
            "rtt_ms": take(&self.rtt_ms),
            "errors": errors,
        })
    }
}

/// User plus system time of this process in clock ticks, from `/proc/self/stat`.
fn process_cpu_ticks() -> u64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    // Fields after the command name (which may contain spaces): state is field 3, utime 14.
    let rest = stat.rsplit_once(") ").map_or("", |(_, r)| r);
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let field = |i: usize| {
        fields
            .get(i)
            .and_then(|f| f.parse::<u64>().ok())
            .unwrap_or(0)
    };
    field(11) + field(12)
}

fn parse_args() -> Result<(Config, bool), String> {
    let mut config = Config {
        host: "127.0.0.1".into(),
        port: 25565,
        join_rate: 20.0,
        join_concurrency: 32,
        source_ips: 64,
        radius: 48.0,
        chat_interval: 0.0,
        view_distance: 10,
        prefix: "bot".into(),
        seed: 1,
        movement: Movement::Random,
    };
    let mut check = false;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        let bad = |e: &dyn std::fmt::Display| format!("{arg}: {e}");
        match arg.as_str() {
            "--host" => config.host = value()?.clone(),
            "--port" => config.port = value()?.parse().map_err(|e| bad(&e))?,
            "--join-rate" => config.join_rate = value()?.parse().map_err(|e| bad(&e))?,
            "--join-concurrency" => {
                config.join_concurrency = value()?.parse().map_err(|e| bad(&e))?;
            }
            "--source-ips" => config.source_ips = value()?.parse().map_err(|e| bad(&e))?,
            "--radius" => config.radius = value()?.parse().map_err(|e| bad(&e))?,
            "--chat-interval" => config.chat_interval = value()?.parse().map_err(|e| bad(&e))?,
            "--view-distance" => config.view_distance = value()?.parse().map_err(|e| bad(&e))?,
            "--prefix" => config.prefix = value()?.clone(),
            "--seed" => config.seed = value()?.parse().map_err(|e| bad(&e))?,
            "--movement" => config.movement = Movement::parse(value()?)?,
            "--check" => check = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let rate_ok = config.join_rate > 0.0;
    if config.prefix.len() > 10 || !rate_ok || config.join_concurrency == 0 {
        return Err("--prefix must be at most 10 characters, --join-rate and \
                    --join-concurrency positive"
            .into());
    }
    Ok((config, check))
}

fn main() {
    let config = match parse_args() {
        Ok((_, true)) => return,
        Ok((c, false)) => Arc::new(c),
        Err(e) => {
            eprintln!("loadbot: {e}");
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("loadbot: cannot start the runtime: {e}");
            std::process::exit(1);
        }
    };
    let metrics = Arc::new(Metrics::default());
    let (target_tx, target_rx) = watch::channel(0u64);
    let (movement_tx, movement_rx) = watch::channel(config.movement);
    let (rate_tx, rate_rx) = watch::channel((config.join_rate, config.join_concurrency));
    let (say_tx, say_rx) = watch::channel(String::new());
    let start = Instant::now();

    runtime.spawn(spawner(
        Arc::clone(&config),
        Arc::clone(&metrics),
        target_rx.clone(),
        movement_rx,
        rate_rx,
        say_rx,
    ));
    let reporter_metrics = Arc::clone(&metrics);
    runtime.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let target = *target_rx.borrow();
            println!("{}", reporter_metrics.report(start, target));
        }
    });

    // Control commands on stdin; the runtime keeps running on its own threads.
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Some(text) = line.strip_prefix("say ") {
            let _ = say_tx.send(text.to_owned());
            continue;
        }
        match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["target", n] => match n.parse::<u64>() {
                Ok(n) => {
                    let _ = target_tx.send(n);
                }
                Err(e) => eprintln!("loadbot: target: {e}"),
            },
            ["movement", spec] => match Movement::parse(spec) {
                Ok(m) => {
                    let _ = movement_tx.send(m);
                }
                Err(e) => eprintln!("loadbot: {e}"),
            },
            ["rate", rate, concurrency @ ..] if concurrency.len() <= 1 => {
                let rate = rate.parse::<f64>().ok().filter(|r| *r > 0.0);
                let concurrency = match concurrency.first() {
                    Some(c) => c.parse::<usize>().ok().filter(|c| *c > 0),
                    None => Some(rate_tx.borrow().1),
                };
                match (rate, concurrency) {
                    (Some(r), Some(c)) => {
                        let _ = rate_tx.send((r, c));
                    }
                    _ => eprintln!("loadbot: rate: expected <joins per s> [<logins>]"),
                }
            }
            ["quit"] => break,
            [] => {}
            _ => eprintln!("loadbot: unknown command {line:?}"),
        }
    }
    println!("{}", metrics.report(start, *target_tx.borrow()));
    runtime.shutdown_timeout(Duration::from_secs(1));
}

/// Starts bots until the target count is reached, at the join rate and with at most
/// `join_concurrency` bots logging in at once.
async fn spawner(
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    mut target: watch::Receiver<u64>,
    movement: watch::Receiver<Movement>,
    mut rate: watch::Receiver<(f64, usize)>,
    say: watch::Receiver<String>,
) {
    let mut concurrency = config.join_concurrency;
    let pending = Arc::new(Semaphore::new(concurrency));
    let new_interval = |per_s: f64| {
        let mut i = tokio::time::interval(Duration::from_secs_f64(1.0 / per_s));
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        i
    };
    let mut interval = new_interval(config.join_rate);
    let mut started = 0u64;
    loop {
        while started >= *target.borrow() {
            if target.changed().await.is_err() {
                return;
            }
        }
        if rate.has_changed().unwrap_or(false) {
            let (per_s, logins) = *rate.borrow_and_update();
            interval = new_interval(per_s);
            if logins > concurrency {
                pending.add_permits(logins - concurrency);
                concurrency = logins;
            }
        }
        interval.tick().await;
        let Ok(permit) = Arc::clone(&pending).acquire_owned().await else {
            return;
        };
        let index = started;
        started += 1;
        tokio::spawn(bot::run(
            index,
            Arc::clone(&config),
            Arc::clone(&metrics),
            movement.clone(),
            say.clone(),
            permit,
        ));
    }
}
