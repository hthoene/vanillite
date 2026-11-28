//! Chunk pipeline benchmark: generates (or loads) every chunk within a radius through the real
//! chunk runtime with the built-in slots (Vanilla generator, BFS light, Anvil storage) on the
//! compute pool, and reports wall time, throughput, CPU time per status and memory.
//!
//! ```text
//! cargo run --profile release-fast -p vanillite-server --example chunk_bench -- \
//!     --radius 16 --threads 4 [--tick-ms 50] [--pump] [--seed N] [--dir /tmp/bench] [--load] \\
//!     [--anti-xray] [--fly N]
//! ```
//!
//! `--tick-ms` is the world tick interval: the runtime applies results and starts jobs once per
//! tick, as on a server. `--load` saves the generated chunks and then measures loading them
//! from disk into a fresh runtime. `--anti-xray` measures encoding the chunk packets of the
//! generated chunks without Anti-Xray and in its `hide` and `obfuscate` modes (Paper's lists).
//! `--fly N` (with `--load`) then moves a view distance 10 player ticket `N` chunks along x
//! through the saved terrain, one chunk at a time, and reports the chunks read per chunk moved
//! and the chunks in memory (`--radius` must cover the flight and the view).
//! Output is one JSON object per phase on stdout.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use vanillite_base::{BlockPos, ChunkPos};
use vanillite_chunk::light::ChunkLight;
use vanillite_chunk::slots::{
    ChunkCodec, ChunkGenerator, ChunkStatus, LightEngine, Neighbours, ProtoChunk, RecordMeta,
    WorldInfo,
};
use vanillite_server::{Compute, ComputePool};
use vanillite_world::chunks::{ChunkSettings, ChunkSlots, Chunks, Executor, FULL_LEVEL};
use vanillite_worldgen::{DimensionSpec, TerrainGenerator, VanillaChunkGenerator};

/// CPU nanoseconds and calls per status (index = `ChunkStatus::index`), plus load and save.
#[derive(Default)]
struct Timings {
    nanos: [AtomicU64; 12],
    calls: [AtomicU64; 12],
}

const LOAD: usize = 10;
const SAVE: usize = 11;

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

unsafe extern "C" {
    fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
}

/// CPU time of the calling thread in nanoseconds (`CLOCK_THREAD_CPUTIME_ID`), so the numbers
/// hold on a busy machine.
#[allow(unsafe_code)]
fn thread_cpu_ns() -> u64 {
    let mut ts = Timespec { sec: 0, nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec; clock 3 is CLOCK_THREAD_CPUTIME_ID on Linux.
    unsafe { clock_gettime(3, &mut ts) };
    ts.sec as u64 * 1_000_000_000 + ts.nsec as u64
}

impl Timings {
    fn record(&self, slot: usize, since: u64) {
        self.nanos[slot].fetch_add(thread_cpu_ns() - since, Ordering::Relaxed);
        self.calls[slot].fetch_add(1, Ordering::Relaxed);
    }

    fn take(&self) -> Vec<(String, f64, u64)> {
        let name = |i: usize| match i {
            LOAD => "load".to_owned(),
            SAVE => "save".to_owned(),
            i => ChunkStatus::ALL[i].name().to_owned(),
        };
        (0..12)
            .map(|i| {
                (
                    name(i),
                    self.nanos[i].swap(0, Ordering::Relaxed) as f64 / 1e6,
                    self.calls[i].swap(0, Ordering::Relaxed),
                )
            })
            .filter(|(_, _, calls)| *calls > 0)
            .collect()
    }
}

struct TimedGenerator(Arc<dyn ChunkGenerator>, Arc<Timings>);

impl ChunkGenerator for TimedGenerator {
    fn new_chunk(&self, pos: ChunkPos) -> ProtoChunk {
        self.0.new_chunk(pos)
    }

    fn prepare(&self) {
        self.0.prepare();
    }

    fn generate(
        &self,
        status: ChunkStatus,
        targets: &mut [ProtoChunk],
        neighbours: &mut Neighbours<'_>,
    ) {
        let start = thread_cpu_ns();
        self.0.generate(status, targets, neighbours);
        self.1.record(status.index(), start);
    }
}

struct TimedLight(Arc<dyn LightEngine>, Arc<Timings>);

impl LightEngine for TimedLight {
    fn initialize_light(&self, targets: &mut [ProtoChunk]) {
        let start = thread_cpu_ns();
        self.0.initialize_light(targets);
        self.1.record(ChunkStatus::InitializeLight.index(), start);
    }

    fn light_chunks(&self, targets: &mut [ProtoChunk], neighbours: &mut Neighbours<'_>) {
        let start = thread_cpu_ns();
        self.0.light_chunks(targets, neighbours);
        self.1.record(ChunkStatus::Light.index(), start);
    }

    fn relight(&self, changes: &[BlockPos], area: &Neighbours<'_>) -> Vec<(ChunkPos, ChunkLight)> {
        self.0.relight(changes, area)
    }
}

/// Times decoding (load) and encoding (save); the store's IO runs on its own threads.
struct TimedCodec(Arc<dyn ChunkCodec>, Arc<Timings>);

impl ChunkCodec for TimedCodec {
    fn decode(
        &self,
        pos: ChunkPos,
        meta: &RecordMeta,
        bytes: &[u8],
    ) -> std::io::Result<ProtoChunk> {
        let start = thread_cpu_ns();
        let chunk = self.0.decode(pos, meta, bytes);
        self.1.record(LOAD, start);
        chunk
    }

    fn encode(&self, chunk: &ProtoChunk) -> std::io::Result<(RecordMeta, Arc<[u8]>)> {
        let start = thread_cpu_ns();
        let record = self.0.encode(chunk);
        self.1.record(SAVE, start);
        record
    }

    fn chunks_per_job(&self) -> usize {
        self.0.chunks_per_job()
    }
}

struct PoolExecutor(Compute);

impl Executor for PoolExecutor {
    fn spawn(&self, priority: u32, job: Box<dyn FnOnce() + Send>) {
        self.0.spawn_background(priority, job);
    }
}

struct Args {
    radius: i32,
    threads: usize,
    tick: Duration,
    seed: i64,
    dir: PathBuf,
    load: bool,
    max_jobs: usize,
    pump: bool,
    anti_xray: bool,
    fly: i32,
}

fn args() -> Args {
    let mut a = Args {
        radius: 16,
        threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
        tick: Duration::from_millis(50),
        seed: -578_941_029_922_802_438,
        dir: std::env::temp_dir().join("vanillite-chunk-bench"),
        load: false,
        pump: false,
        anti_xray: false,
        fly: 0,
        max_jobs: ChunkSettings::default().max_jobs,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().expect("flag value");
        match flag.as_str() {
            "--radius" => a.radius = value().parse().expect("radius"),
            "--threads" => a.threads = value().parse().expect("threads"),
            "--tick-ms" => a.tick = Duration::from_millis(value().parse().expect("tick")),
            "--seed" => a.seed = value().parse().expect("seed"),
            "--dir" => a.dir = value().into(),
            "--max-jobs" => a.max_jobs = value().parse().expect("max jobs"),
            "--load" => a.load = true,
            "--pump" => a.pump = true,
            "--anti-xray" => a.anti_xray = true,
            "--fly" => a.fly = value().parse().expect("fly"),
            other => panic!("unknown flag {other}"),
        }
    }
    a
}

fn rss_kib(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

fn cpu_ms() -> f64 {
    // Sum of the run time of every thread of the process (`schedstat`, nanoseconds).
    let mut total = 0;
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        for task in tasks.flatten() {
            total += std::fs::read_to_string(task.path().join("schedstat"))
                .ok()
                .and_then(|s| {
                    s.split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<u64>().ok())
                })
                .unwrap_or(0);
        }
    }
    total as f64 / 1e6
}

/// Ticks the runtime until every chunk within `radius` is full; returns the time to the first
/// full centre chunk and the total time. With `wake`, finished jobs are pumped between ticks
/// as the server does.
fn run(
    chunks: &mut Chunks,
    radius: i32,
    tick: Duration,
    wake: Option<&mpsc::Receiver<()>>,
) -> (Duration, Duration, f64) {
    let start = Instant::now();
    let cpu_start = cpu_ms();
    // One ticket reaches at most 32 chunks; larger areas get a ticket per chunk, as a
    // pregenerator does.
    let mut tickets = Vec::new();
    if radius < i32::from(FULL_LEVEL) {
        tickets.push(chunks.add_ticket(ChunkPos::new(0, 0), FULL_LEVEL - radius as u8));
    } else {
        for z in -radius..=radius {
            for x in -radius..=radius {
                tickets.push(chunks.add_ticket(ChunkPos::new(x, z), FULL_LEVEL));
            }
        }
    }
    let mut first = None;
    let mut first_cpu = 0.0;
    let total = (2 * radius + 1).pow(2) as usize;
    loop {
        let tick_start = Instant::now();
        chunks.tick();
        if first.is_none() && chunks.chunk(ChunkPos::new(0, 0)).is_some() {
            first = Some(start.elapsed());
            first_cpu = cpu_ms() - cpu_start;
        }
        let mut full = 0;
        for z in -radius..=radius {
            for x in -radius..=radius {
                full += usize::from(chunks.chunk(ChunkPos::new(x, z)).is_some());
            }
        }
        if full == total {
            return (first.unwrap_or_default(), start.elapsed(), first_cpu);
        }
        let deadline = tick_start + tick;
        match wake {
            Some(wake) => {
                while let Some(rest) = deadline.checked_duration_since(Instant::now()) {
                    if wake.recv_timeout(rest).is_ok() {
                        chunks.pump();
                    }
                }
            }
            None => {
                if let Some(rest) = deadline.checked_duration_since(Instant::now()) {
                    std::thread::sleep(rest);
                }
            }
        }
    }
}

fn report(
    phase: &str,
    a: &Args,
    chunks: &Chunks,
    timings: &Timings,
    (first, total, first_cpu): (Duration, Duration, f64),
    cpu: f64,
) {
    let full = (2 * a.radius + 1).pow(2) as f64;
    let stages: Vec<String> = timings
        .take()
        .into_iter()
        .map(|(name, ms, calls)| {
            format!(
                "\"{name}\":{{\"cpu_ms\":{ms:.1},\"calls\":{calls},\"us_per_call\":{:.1}}}",
                ms * 1000.0 / calls as f64
            )
        })
        .collect();
    println!(
        "{{\"phase\":\"{phase}\",\"radius\":{},\"threads\":{},\"tick_ms\":{},\"full_chunks\":{full},\
         \"loaded_chunks\":{},\"first_ms\":{:.0},\"first_cpu_ms\":{first_cpu:.0},\"wall_ms\":{:.0},\"full_per_s\":{:.1},\
         \"process_cpu_ms\":{cpu:.0},\"rss_mib\":{},\"peak_rss_mib\":{},\"stages\":{{{}}}}}",
        a.radius,
        a.threads,
        a.tick.as_millis(),
        chunks.loaded(),
        first.as_secs_f64() * 1000.0,
        total.as_secs_f64() * 1000.0,
        full / total.as_secs_f64(),
        rss_kib("VmRSS:") / 1024,
        rss_kib("VmHWM:") / 1024,
        stages.join(",")
    );
}

fn main() {
    let a = args();
    let cache = std::env::var_os("VANILLITE_CACHE").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/vanilla-cache"),
        PathBuf::from,
    );
    let pack = vanillite_datapack::vanilla::ensure_vanilla_pack(&cache, None).expect("pack");
    let packs = vanillite_datapack::load(&[pack.as_path()]).expect("packs");
    let tag = |name: &str| packs.tag_ids("minecraft:block", name).expect("tag");
    let biome_registry = packs.registry("minecraft:worldgen/biome").expect("biomes");
    vanillite_chunk::facts::install_vanilla(
        &tag("minecraft:blocks_motion_in_heightmap"),
        &tag("minecraft:blocks_motion_in_heightmap_no_leaves"),
        biome_registry.entries().len() as u32,
    );
    let biomes: Arc<[String]> = biome_registry
        .entries()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    let default_biome = biomes
        .iter()
        .position(|b| b == "minecraft:plains")
        .unwrap_or(0) as u16;
    let _ = std::fs::remove_dir_all(&a.dir);
    let info = WorldInfo {
        dimension: "minecraft:overworld".to_owned(),
        dir: a.dir.clone(),
        min_y: -64,
        height: 384,
        seed: a.seed,
        has_sky_light: true,
        biomes,
        default_biome,
        air: vanillite_data::block::BlockState::AIR.0,
    };
    let biome_source =
        serde_json::json!({"type": "minecraft:multi_noise", "preset": "minecraft:overworld"});
    let spec = DimensionSpec {
        noise_settings: "minecraft:overworld",
        biome_source: &biome_source,
        min_y: -64,
        height: 384,
        has_sky_light: true,
    };
    let setup = Instant::now();
    let terrain = TerrainGenerator::new(&packs, spec, a.seed).expect("generator");
    eprintln!("generator setup {:?}", setup.elapsed());
    let timings = Arc::new(Timings::default());
    let pool = ComputePool::new(a.threads).expect("pool");
    let executor: Arc<dyn Executor> = Arc::new(PoolExecutor(Compute::new(Arc::new(pool))));
    let generator: Arc<dyn ChunkGenerator> = Arc::new(TimedGenerator(
        Arc::new(VanillaChunkGenerator::new(terrain, info.clone())),
        Arc::clone(&timings),
    ));
    let prepare = Instant::now();
    generator.prepare();
    eprintln!("generator prepare {:?}", prepare.elapsed());
    let light: Arc<dyn LightEngine> = Arc::new(TimedLight(
        Arc::new(vanillite_light::BfsLightEngine::new(&info)),
        Arc::clone(&timings),
    ));
    let io = vanillite_storage::IoPool::new(2).expect("IO threads");
    let codec: Arc<dyn ChunkCodec> = Arc::new(TimedCodec(
        Arc::new(vanillite_storage::AnvilCodec::new(
            &info,
            vanillite_storage::Compression::Deflate,
        )),
        Arc::clone(&timings),
    ));
    let store = || {
        Arc::new(vanillite_storage::RegionStore::new(
            info.dir.clone(),
            vanillite_storage::WorldFormat::Anvil,
            vanillite_storage::StoreSettings::default(),
            io.clone(),
        ))
    };
    let settings = ChunkSettings {
        view_distance: a.radius.clamp(2, 32),
        max_jobs: a.max_jobs,
        ..ChunkSettings::default()
    };
    let slots = ChunkSlots {
        store: store(),
        codec: Arc::clone(&codec),
        generator: Arc::clone(&generator),
        light: Arc::clone(&light),
        executor: Arc::clone(&executor),
    };
    let mut chunks = Chunks::new(info.clone(), settings, slots);
    let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(1);
    let waker = move || {
        let _ = wake_tx.try_send(());
    };
    if a.pump {
        chunks.set_waker(Arc::new(waker.clone()));
    }
    let wake = a.pump.then_some(&wake_rx);
    let cpu = cpu_ms();
    let times = run(&mut chunks, a.radius, a.tick, wake);
    report("generate", &a, &chunks, &timings, times, cpu_ms() - cpu);
    if a.anti_xray {
        anti_xray(&chunks, a.radius - 1);
    }
    if a.load {
        let save = Instant::now();
        chunks.save_all(true);
        // Saves run as jobs; tick until they are applied.
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(20));
            chunks.tick();
        }
        eprintln!("save {:?}", save.elapsed());
        drop(chunks);
        timings.take();
        let (codec_for_fly, generator_for_fly, light_for_fly, executor_for_fly) = (
            Arc::clone(&codec),
            Arc::clone(&generator),
            Arc::clone(&light),
            Arc::clone(&executor),
        );
        let slots = ChunkSlots {
            store: store(),
            codec,
            generator,
            light,
            executor,
        };
        let mut chunks = Chunks::new(info.clone(), settings, slots);
        if a.pump {
            chunks.set_waker(Arc::new(waker));
        }
        let cpu = cpu_ms();
        let times = run(&mut chunks, a.radius, a.tick, wake);
        report("load", &a, &chunks, &timings, times, cpu_ms() - cpu);
        if a.fly > 0 {
            drop(chunks);
            let settings = ChunkSettings {
                view_distance: 10,
                ..settings
            };
            let slots = ChunkSlots {
                store: store(),
                codec: Arc::clone(&codec_for_fly),
                generator: Arc::clone(&generator_for_fly),
                light: Arc::clone(&light_for_fly),
                executor: Arc::clone(&executor_for_fly),
            };
            let mut chunks = Chunks::new(info.clone(), settings, slots);
            fly(&mut chunks, &timings, a.fly, a.tick);
        }
    }
}

/// Moves a player ticket `distance` chunks along x through stored terrain, one chunk at a time,
/// waiting each time until its view is full, and reports reads and chunks in memory.
fn fly(chunks: &mut Chunks, timings: &Timings, distance: i32, tick: Duration) {
    let view = 11;
    let level = chunks.player_ticket_level();
    let start = -distance / 2;
    let ticket = chunks.add_ticket(ChunkPos::new(start, 0), level);
    let mut visibility = Vec::new();
    let mut settle = |chunks: &mut Chunks, x: i32| loop {
        chunks.tick();
        visibility.clear();
        chunks.take_visibility_changes(&mut visibility);
        let full = (-view..=view)
            .flat_map(|dz| (-view..=view).map(move |dx| (dx, dz)))
            .all(|(dx, dz)| chunks.chunk(ChunkPos::new(x + dx, dz)).is_some());
        if full && chunks.is_idle() {
            return;
        }
        std::thread::sleep(tick.min(Duration::from_millis(5)));
    };
    settle(chunks, start);
    let in_memory_start = chunks.loaded();
    timings.take();
    let begin = Instant::now();
    let mut in_memory = 0;
    for x in start + 1..=start + distance {
        chunks.move_ticket(ticket, ChunkPos::new(x, 0));
        settle(chunks, x);
        in_memory += chunks.loaded();
    }
    let loads = timings.calls[LOAD].swap(0, Ordering::Relaxed);
    println!(
        "{{\"phase\":\"fly\",\"chunks_moved\":{distance},\"in_memory_at_rest\":{in_memory_start},         \"in_memory_avg\":{:.0},\"reads\":{loads},\"reads_per_chunk_moved\":{:.1},\"wall_ms\":{:.0}}}",
        in_memory as f64 / f64::from(distance),
        loads as f64 / f64::from(distance),
        begin.elapsed().as_secs_f64() * 1000.0
    );
}

/// Encodes the chunk packet of every full chunk within `radius` without Anti-Xray and in its
/// modes, single-threaded, and reports time and size per chunk.
fn anti_xray(chunks: &Chunks, radius: i32) {
    use vanillite_config::{AntiXrayMode, Profile, RuleSet, WorldRules};
    let positions: Vec<ChunkPos> = (-radius..=radius)
        .flat_map(|x| (-radius..=radius).map(move |z| ChunkPos::new(x, z)))
        .filter(|p| chunks.chunk(*p).is_some())
        .collect();
    let mut rules = WorldRules::for_profile(Profile::Paper).anti_xray;
    for mode in [
        AntiXrayMode::Disabled,
        AntiXrayMode::Hide,
        AntiXrayMode::Obfuscate,
    ] {
        rules.engine_mode = mode;
        let engine = vanillite_server::anti_xray::AntiXray::new(&rules, "minecraft:overworld");
        let mut bytes = 0usize;
        let mut body = Vec::new();
        let rounds = 3;
        let start = Instant::now();
        for _ in 0..rounds {
            for &pos in &positions {
                let Some(chunk) = chunks.chunk(pos) else {
                    continue;
                };
                body.clear();
                let masked = engine
                    .as_ref()
                    .and_then(|e| e.mask(chunks, pos, &chunk.chunk));
                vanillite_chunk::packet::encode_chunk_with_sections(
                    &mut body,
                    pos.x,
                    pos.z,
                    &chunk.chunk,
                    masked.as_deref().unwrap_or(chunk.chunk.sections()),
                    &[],
                );
                bytes += body.len();
            }
        }
        let n = (positions.len() * rounds).max(1);
        println!(
            "{}",
            serde_json::json!({
                "phase": "anti-xray",
                "mode": format!("{mode:?}").to_lowercase(),
                "chunks": positions.len(),
                "us_per_chunk": start.elapsed().as_secs_f64() * 1e6 / n as f64,
                "bytes_per_chunk": bytes / n,
            })
        );
    }
}
