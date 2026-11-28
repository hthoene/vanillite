//! `cargo xtask loadtest weak`: realistic mixed load on weak hardware (#270), on this machine.
//!
//! The server (built from this checkout) runs in its own directory with a fresh world under a
//! limit of one CPU and 1 GiB: `systemd-run --scope` with `CPUQuota` and `MemoryMax` (as root,
//! or through the `vanillite-limited` sudo helper of vanillite-test2), pinned to one CPU with
//! `taskset`; without systemd only the pinning applies and the result says so. Bots of
//! `tools/loadbot` on the other CPUs play like a small survival server: explorers fly through
//! new terrain, survival players walk, build, break and fight mobs near the spawn (difficulty
//! normal, natural spawning), and two players stand at an animal farm (500 cows) and a
//! redstone contraption (93 observer clocks). Records MSPT, TPS, RSS (also per minute, to see
//! the allocator return memory or not), CPU, chunk delivery to the bots and the bots' actions.
//! Output: `target/loadtest-weak/<time>/result.json` and the server log next to it. Run it on
//! vanillite-test2 with `cargo xtask testserver run --fetch target/loadtest-weak -- loadtest
//! weak`.

use std::io::{BufRead, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Result;
use crate::loadtest::{command_output, git, mean, pct, round, show};

/// World seed of all runs (the benchmark seed).
const SEED: &str = "-578941029922802438";
/// The animal farm and the redstone contraption (corner x, y, z): platforms high above the
/// terrain, so they do not depend on the world.
const FARM: (i32, i32, i32) = (320, 240, 320);
const REDSTONE: (i32, i32, i32) = (-384, 240, 320);
/// Length of the phase profile (`/vanillite profile`) taken in the middle of the run.
const PROFILE_SECONDS: u64 = 120;
/// Chunks a watcher receives after its teleport before the builds start (most of its view).
const WATCHER_CHUNKS: u64 = 300;
/// Seconds between `/give` rounds that keep the survival bots supplied with blocks.
const GIVE_EVERY: u64 = 120;

/// The server's threads by name: the tick, the compute workers (generation, light, loading),
/// network I/O and the rest.
const THREAD_GROUPS: [&str; 4] = ["tick", "compute", "network", "other"];

fn thread_group(name: &str) -> usize {
    match name {
        "Server thread" => 0,
        n if n.starts_with("Compute") => 1,
        n if n.starts_with("vanillite-net") => 2,
        _ => 3,
    }
}

/// One group of bots: one `tools/loadbot` process.
struct Group {
    name: &'static str,
    prefix: &'static str,
    bots: u64,
    movement: &'static str,
    view_distance: u8,
}

const GROUPS: &[Group] = &[
    Group {
        name: "explorers",
        prefix: "ex",
        bots: 3,
        movement: "straight:sprint:90",
        view_distance: 10,
    },
    Group {
        name: "survival",
        prefix: "sv",
        bots: 3,
        movement: "survival",
        view_distance: 8,
    },
    Group {
        name: "farm",
        prefix: "fa",
        bots: 1,
        movement: "still",
        view_distance: 10,
    },
    Group {
        name: "redstone",
        prefix: "rs",
        bots: 1,
        movement: "still",
        view_distance: 10,
    },
];

struct Options {
    minutes: f64,
    binary: Option<PathBuf>,
    /// A directory with `vanillite` and `loadbot`: nothing is built, results go below it.
    prebuilt: Option<PathBuf>,
    profile: &'static str,
    cpu: Option<u32>,
    memory_mib: u64,
    port: u16,
    rules: Vec<(String, String)>,
}

fn parse(args: &[&str]) -> Result<Options> {
    let mut o = Options {
        minutes: 20.0,
        binary: None,
        prebuilt: None,
        profile: "release-fast",
        cpu: None,
        memory_mib: 1024,
        port: 30950,
        rules: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().copied().ok_or(format!("{arg} needs a value"));
        let bad = |e: &dyn std::fmt::Display| format!("{arg}: {e}");
        match *arg {
            "--minutes" => o.minutes = value()?.parse().map_err(|e| bad(&e))?,
            "--binary" => o.binary = Some(PathBuf::from(value()?)),
            "--prebuilt" => o.prebuilt = Some(PathBuf::from(value()?)),
            "--release" => o.profile = "release",
            "--cpu" => o.cpu = Some(value()?.parse().map_err(|e| bad(&e))?),
            "--memory-mib" => o.memory_mib = value()?.parse().map_err(|e| bad(&e))?,
            "--port" => o.port = value()?.parse().map_err(|e| bad(&e))?,
            "--rule" => {
                let (key, rule) = value()?
                    .split_once('=')
                    .ok_or("--rule needs <key>=<value>")?;
                o.rules.push((key.to_owned(), rule.to_owned()));
            }
            _ => return Err(crate::USAGE.to_owned()),
        }
    }
    if o.minutes <= 0.0 {
        return Err("--minutes must be positive".into());
    }
    Ok(o)
}

/// What the readers of the server and the bots collected, by seconds since the start.
#[derive(Default)]
struct Collected {
    /// Duration of every tick in milliseconds.
    ticks: Vec<(f64, f64)>,
    /// RSS in KiB and CPU time in clock ticks of the server, every second.
    stats: Vec<(f64, u64, u64)>,
    /// CPU time in clock ticks per thread group ([`THREAD_GROUPS`]), every second.
    threads: Vec<(f64, [u64; 4])>,
    /// One report per second and bot group.
    bots: Vec<(f64, usize, Value)>,
    /// The server printed its "Done" line.
    ready: Option<f64>,
}

/// `cargo xtask loadtest weak [options]`.
pub fn run(root: &Path, args: &[&str]) -> Result<()> {
    let o = parse(args)?;
    // `--prebuilt`: binaries copied to a machine without the checkout, results next to them.
    let (bin_dir, base) = match &o.prebuilt {
        Some(dir) => (dir.clone(), dir.join("loadtest-weak")),
        None => {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
            let mut build = vec!["build", "--locked", "--profile", o.profile, "-p", "loadbot"];
            if o.binary.is_none() {
                build.extend(["-p", "vanillite"]);
            }
            crate::devenv::run(Command::new(&cargo).current_dir(root).args(&build))?;
            (
                crate::target_dir(root).join(o.profile),
                crate::target_dir(root).join("loadtest-weak"),
            )
        }
    };
    let server_bin = o
        .binary
        .clone()
        .unwrap_or_else(|| bin_dir.join("vanillite"));
    let server_bin =
        std::fs::canonicalize(&server_bin).map_err(|e| format!("{}: {e}", server_bin.display()))?;
    let loadbot = bin_dir.join("loadbot");

    let commit = git(root, &["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    let stamp = command_output(Command::new("date").args(["-u", "+%Y%m%dT%H%M%SZ"]))?;
    let out = base.join(&stamp);
    let dir = out.join("server");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // The Vanilla data pack is downloaded once and shared by all runs.
    let cache = base.join("cache");
    std::fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    std::os::unix::fs::symlink(&cache, dir.join("cache")).map_err(|e| format!("cache: {e}"))?;
    crate::write(&dir.join("eula.txt"), "eula=true\n")?;
    crate::write(&dir.join("server.properties"), &properties(o.port))?;

    let cpus = allowed_cpus()?;
    let server_cpu = o.cpu.unwrap_or(cpus[0]);
    let bot_cpus: Vec<u32> = cpus.iter().copied().filter(|&c| c != server_cpu).collect();
    let bot_cpus = if bot_cpus.is_empty() {
        cpus.clone()
    } else {
        bot_cpus
    };
    let bot_cpus = bot_cpus
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let (mut cmd, limiter) = limited(server_cpu, o.memory_mib);
    cmd.arg("env").arg("VANILLITE_TICK_LOG=tick.log");
    for (key, value) in &o.rules {
        let name = key.to_uppercase().replace('.', "__").replace('-', "_");
        cmd.arg(format!("VANILLITE_{name}={value}"));
    }
    cmd.arg(&server_bin)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    println!(
        "== loadtest weak: {} on CPU {server_cpu} ({limiter}), bots on CPUs {bot_cpus}, {} min",
        server_bin.display(),
        o.minutes
    );
    let start = Instant::now();
    let data = Arc::new(Mutex::new(Collected::default()));
    let mut server = cmd.spawn().map_err(|e| format!("server: {e}"))?;
    let stdout = server.stdout.take().ok_or("no server output")?;
    let stderr = server.stderr.take().ok_or("no server output")?;
    let log = std::fs::File::create(out.join("server.log")).map_err(|e| e.to_string())?;
    let log = Arc::new(Mutex::new(log));
    for stream in [
        Box::new(stdout) as Box<dyn std::io::Read + Send>,
        Box::new(stderr),
    ] {
        let data = Arc::clone(&data);
        let log = Arc::clone(&log);
        std::thread::spawn(move || read_server(stream, &log, start, &data));
    }
    let console = server.stdin.take().ok_or("no server input")?;
    let mut run = Run {
        server,
        console,
        pid: None,
        bots: Vec::new(),
        data,
        start,
    };
    let result = run.measure(&o, &dir, &loadbot, &bot_cpus);
    let cgroup = run.pid.map(cgroup_memory).unwrap_or(Value::Null);
    run.stop();
    let mut result = result?;
    result["commit"] = json!(commit);
    result["date"] = json!(stamp);
    result["settings"] = json!({
        "minutes": o.minutes,
        "server_cpu": server_cpu,
        "bot_cpus": bot_cpus,
        "limiter": limiter,
        "memory_limit_mib": o.memory_mib,
        "profile": o.profile,
        "binary": o.binary.as_ref().map(|b| b.display().to_string()),
        "rules": o.rules.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
        "groups": GROUPS.iter().map(|g| json!({
            "name": g.name, "bots": g.bots, "movement": g.movement,
            "view_distance": g.view_distance,
        })).collect::<Vec<_>>(),
    });
    result["cgroup"] = cgroup;
    let file = out.join("result.json");
    let text = serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?;
    crate::write(&file, &text)?;
    println!("wrote {}", file.display());
    if result["status"] == "failed" {
        return Err(result["note"].as_str().unwrap_or("failed").to_owned());
    }
    Ok(())
}

fn properties(port: u16) -> String {
    [
        format!("server-port={port}"),
        "online-mode=false".into(),
        "white-list=false".into(),
        "enforce-secure-profile=false".into(),
        format!("level-seed={SEED}"),
        "level-type=minecraft:normal".into(),
        "generate-structures=true".into(),
        "spawn-protection=0".into(),
        // Explorers fly above the terrain; watchers hover over their build.
        "allow-flight=true".into(),
        "difficulty=normal".into(),
        "view-distance=10".into(),
        "simulation-distance=10".into(),
        "pause-when-empty-seconds=0".into(),
        "max-players=100".into(),
    ]
    .join("\n")
        + "\n"
}

/// The CPUs this process may run on (`Cpus_allowed_list`, e.g. `0-3,8`).
fn allowed_cpus() -> Result<Vec<u32>> {
    let status = crate::read(Path::new("/proc/self/status"))?;
    let list = status
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
        .ok_or("no Cpus_allowed_list")?;
    let mut cpus = Vec::new();
    for part in list.trim().split(',') {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let (a, b): (u32, u32) = (
            a.parse().map_err(|e| format!("cpus {list}: {e}"))?,
            b.parse().map_err(|e| format!("cpus {list}: {e}"))?,
        );
        cpus.extend(a..=b);
    }
    if cpus.is_empty() {
        return Err("no CPUs".into());
    }
    Ok(cpus)
}

/// The command prefix that runs the server on `cpu` with `memory_mib`, and what limits it.
fn limited(cpu: u32, memory_mib: u64) -> (Command, &'static str) {
    let works = |cmd: &mut Command| {
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let scope = [
        "--scope".to_owned(),
        "--quiet".to_owned(),
        "-p".to_owned(),
        "CPUQuota=100%".to_owned(),
        "-p".to_owned(),
        format!("MemoryMax={memory_mib}M"),
        "-p".to_owned(),
        "MemorySwapMax=0".to_owned(),
    ];
    let pin = ["taskset".to_owned(), "-c".to_owned(), cpu.to_string()];
    if works(Command::new("systemd-run").args(&scope).arg("true")) {
        let mut cmd = Command::new("systemd-run");
        cmd.args(&scope).args(pin);
        return (cmd, "systemd scope: CPUQuota 100 %, MemoryMax, pinned");
    }
    // vanillite-test2: the helper starts the scope as the calling user.
    if works(Command::new("sudo").args(["-n", "vanillite-limited", &cpu.to_string(), "64", "true"]))
    {
        let mut cmd = Command::new("sudo");
        cmd.args([
            "-n",
            "vanillite-limited",
            &cpu.to_string(),
            &memory_mib.to_string(),
        ]);
        return (cmd, "vanillite-limited: CPUQuota 100 %, MemoryMax, pinned");
    }
    let mut cmd = Command::new(&pin[0]);
    cmd.args(&pin[1..]);
    (cmd, "taskset only: pinned to one CPU, no memory limit")
}

/// Memory accounting of the server's cgroup, if it has its own (`memory.peak`, OOM kills).
fn cgroup_memory(pid: u32) -> Value {
    let Ok(cgroup) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
        return Value::Null;
    };
    let Some(path) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) else {
        return Value::Null;
    };
    let dir = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let mib = |name: &str| {
        read(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .map(|v| v / 1048576.0)
    };
    let oom = read("memory.events").and_then(|e| {
        e.lines()
            .find_map(|l| l.strip_prefix("oom_kill "))
            .and_then(|v| v.trim().parse::<u64>().ok())
    });
    json!({
        "path": path,
        "memory_peak_mib": round(mib("memory.peak")),
        "memory_max": read("memory.max").map(|v| v.trim().to_owned()),
        "oom_kills": oom,
    })
}

/// Logs the server's output and notes when it is ready.
fn read_server(
    out: impl std::io::Read,
    log: &Mutex<std::fs::File>,
    start: Instant,
    data: &Mutex<Collected>,
) {
    for line in BufReader::new(out).lines().map_while(std::io::Result::ok) {
        if let Ok(mut log) = log.lock() {
            let _ = writeln!(log, "{line}");
        }
        if line.contains("Done (")
            && let Ok(mut d) = data.lock()
            && d.ready.is_none()
        {
            d.ready = Some(start.elapsed().as_secs_f64());
        }
    }
}

struct Bots {
    child: Child,
    input: ChildStdin,
}

struct Run {
    server: Child,
    console: ChildStdin,
    /// The server process itself (the child may be a wrapper).
    pid: Option<u32>,
    bots: Vec<Bots>,
    data: Arc<Mutex<Collected>>,
    start: Instant,
}

impl Run {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn console(&mut self, lines: &[String]) -> Result<()> {
        for line in lines {
            writeln!(self.console, "{line}").map_err(|e| format!("console: {e}"))?;
        }
        self.console.flush().map_err(|e| format!("console: {e}"))
    }

    /// A counter of the newest report of a bot group.
    fn counter(&self, group: usize, key: &str) -> u64 {
        self.data.lock().map_or(0, |d| {
            d.bots
                .iter()
                .rev()
                .find(|(_, g, _)| *g == group)
                .and_then(|(_, _, v)| v[key].as_u64())
                .unwrap_or(0)
        })
    }

    fn online(&self, group: usize) -> u64 {
        self.counter(group, "online")
    }

    fn measure(
        &mut self,
        o: &Options,
        dir: &Path,
        loadbot: &Path,
        bot_cpus: &str,
    ) -> Result<Value> {
        // Ready: the "Done" line, at most 5 minutes (a first run downloads the data pack).
        while self.data.lock().map_err(|_| "poisoned")?.ready.is_none() {
            if self.now() > 300.0 {
                return Err("the server did not start within 300 s".into());
            }
            if let Ok(Some(status)) = self.server.try_wait() {
                return Err(format!("the server exited: {status}"));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let ready_s = self.data.lock().map_err(|_| "poisoned")?.ready;
        self.pid = server_pid(self.server.id());
        let pid = self.pid.ok_or("cannot find the server process")?;
        {
            let data = Arc::clone(&self.data);
            let tick_log = dir.join("tick.log");
            let start = self.start;
            std::thread::spawn(move || sample(pid, &tick_log, start, &data));
        }
        std::thread::sleep(Duration::from_secs(10));
        let idle = self.window(self.now() - 8.0, self.now());

        for (i, g) in GROUPS.iter().enumerate() {
            let mut child = Command::new("taskset")
                .args(["-c", bot_cpus])
                .arg(loadbot)
                .args(["--host", "127.0.0.1", "--port", &o.port.to_string()])
                .args(["--join-rate", "2", "--radius", "48", "--prefix", g.prefix])
                .args(["--view-distance", &g.view_distance.to_string()])
                .args(["--movement", g.movement, "--seed", &(i + 1).to_string()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("loadbot: {e}"))?;
            let out = child.stdout.take().ok_or("no loadbot output")?;
            let mut input = child.stdin.take().ok_or("no loadbot input")?;
            writeln!(input, "target {}", g.bots).map_err(|e| format!("loadbot: {e}"))?;
            let data = Arc::clone(&self.data);
            let start = self.start;
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(std::io::Result::ok) {
                    if let (Ok(v), Ok(mut d)) = (serde_json::from_str::<Value>(&line), data.lock())
                    {
                        d.bots.push((start.elapsed().as_secs_f64(), i, v));
                    }
                }
            });
            self.bots.push(Bots { child, input });
        }
        let joined = self.now();
        while (0..GROUPS.len()).any(|i| self.online(i) < GROUPS[i].bots) {
            if self.now() - joined > 120.0 {
                return Err("not all bots joined within 120 s".into());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let (fx, fy, fz) = FARM;
        let (rx, ry, rz) = REDSTONE;
        let watchers = [GROUPS.len() - 2, GROUPS.len() - 1];
        let before = watchers.map(|g| self.counter(g, "chunks"));
        let teleported = self.now();
        self.console(&[
            "give @a minecraft:dirt 64".to_owned(),
            format!("tp fa00000 {} {} {}", fx + 32, fy + 12, fz + 32),
            format!("tp rs00000 {} {} {}", rx + 32, ry + 12, rz + 32),
        ])?;
        // The terrain under the builds is generated for the watchers first (their view, about
        // 300 chunks each): building next to chunks that are not there yet would load them
        // synchronously, as in Vanilla.
        while watchers
            .iter()
            .zip(before)
            .any(|(&g, b)| self.counter(g, "chunks").saturating_sub(b) < WATCHER_CHUNKS)
        {
            if self.now() - teleported > 300.0 {
                return Err("the watchers' chunks did not arrive within 300 s".into());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let setup_s = self.now() - teleported;
        let mut build = crate::bench::mobs(fx, fy, fz);
        build.extend(crate::bench::redstone(rx, ry, rz));
        self.console(&build)?;
        std::thread::sleep(Duration::from_secs(10));

        let from = self.now();
        let to = from + o.minutes * 60.0;
        let mut next_give = from + GIVE_EVERY as f64;
        // The phase profile of two minutes in the middle of the run (written under the server
        // directory).
        let mut profile_at =
            Some(from + (o.minutes * 60.0 - PROFILE_SECONDS as f64).max(0.0) / 2.0);
        let mut minutes = Vec::new();
        let mut minute_start = from;
        while self.now() < to {
            std::thread::sleep(Duration::from_secs(1));
            if let Ok(Some(status)) = self.server.try_wait() {
                return Ok(json!({
                    "status": "failed",
                    "note": format!("the server exited during the run: {status}"),
                    "minutes": minutes,
                }));
            }
            if profile_at.is_some_and(|t| self.now() >= t) {
                profile_at = None;
                self.console(&[format!("vanillite profile {PROFILE_SECONDS}")])?;
            }
            if self.now() >= next_give {
                next_give += GIVE_EVERY as f64;
                self.console(&["give @a minecraft:dirt 64".to_owned()])?;
            }
            if self.now() - minute_start >= 60.0 {
                // The memory breakdown (chunks, entities, other) goes to the server log.
                self.console(&["vanillite memory".to_owned()])?;
                let end = self.now();
                let w = self.window(minute_start, end);
                println!(
                    "   minute {:>2}: MSPT p50 {} p95 {} max {} ms, TPS {}, RSS {} MiB, CPU {} %, \
                     tick CPU {} ms/tick, chunk wait p95 {} ms, {} bots online",
                    minutes.len() + 1,
                    show(&w["mspt"]["p50"]),
                    show(&w["mspt"]["p95"]),
                    show(&w["mspt"]["max"]),
                    show(&w["tps"]),
                    show(&w["rss_mib"]["max"]),
                    show(&w["cpu_percent"]),
                    show(&w["threads"]["tick"]["cpu_ms_per_tick"]),
                    show(&w["bots"]["chunk_ms_p95"]),
                    w["bots"]["online"],
                );
                minutes.push(w);
                minute_start = end;
            }
        }
        let mut result = self.window(from, to);
        result["start_s"] = round(ready_s);
        result["watcher_view_s"] = round(Some(setup_s));
        result["idle"] = idle;
        result["minutes"] = json!(minutes);
        let profiles: Vec<String> = std::fs::read_dir(dir.join("profiles"))
            .map(|d| {
                d.filter_map(|e| Some(e.ok()?.path().display().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        result["profiles"] = json!(profiles);
        println!(
            "== {} min: MSPT mean {} p50 {} p95 {} p99 {} max {} ms, TPS {}, ticks over 50 ms {}, \
             RSS max {} MiB, CPU {} % (tick {} ms/tick, compute {} %), chunks {}/s, chunk wait \
             p50 {} p95 {} ms",
            o.minutes,
            show(&result["mspt"]["mean"]),
            show(&result["mspt"]["p50"]),
            show(&result["mspt"]["p95"]),
            show(&result["mspt"]["p99"]),
            show(&result["mspt"]["max"]),
            show(&result["tps"]),
            result["mspt"]["over_50ms"],
            show(&result["rss_mib"]["max"]),
            show(&result["cpu_percent"]),
            show(&result["threads"]["tick"]["cpu_ms_per_tick"]),
            show(&result["threads"]["compute"]["cpu_percent"]),
            show(&result["bots"]["chunks_per_s"]),
            show(&result["bots"]["chunk_ms_p50"]),
            show(&result["bots"]["chunk_ms_p95"]),
        );
        let online: u64 = (0..GROUPS.len()).map(|i| self.online(i)).sum();
        let wanted: u64 = GROUPS.iter().map(|g| g.bots).sum();
        if online < wanted {
            result["status"] = json!("failed");
            result["note"] = json!(format!("only {online} of {wanted} bots online at the end"));
        }
        Ok(result)
    }

    /// Server and bot metrics of the window `from`..`to` (seconds since the start).
    fn window(&self, from: f64, to: f64) -> Value {
        let Ok(d) = self.data.lock() else {
            return Value::Null;
        };
        let inside = |t: f64| t >= from && t <= to;
        let mut ticks: Vec<f64> = d
            .ticks
            .iter()
            .filter(|(t, _)| inside(*t))
            .map(|(_, ms)| *ms)
            .collect();
        ticks.sort_by(f64::total_cmp);
        let stats: Vec<&(f64, u64, u64)> = d.stats.iter().filter(|s| inside(s.0)).collect();
        // Per thread group: CPU share, and CPU milliseconds per tick (robust against other
        // load on the same CPU, unlike the tick's wall time).
        let per_thread: Vec<&(f64, [u64; 4])> = d.threads.iter().filter(|s| inside(s.0)).collect();
        let mut threads = serde_json::Map::new();
        if let (Some(a), Some(b)) = (per_thread.first(), per_thread.last())
            && b.0 > a.0
        {
            let hz = clock_ticks();
            for (i, name) in THREAD_GROUPS.iter().enumerate() {
                let used = b.1[i].saturating_sub(a.1[i]) as f64 / hz;
                threads.insert(
                    (*name).to_owned(),
                    json!({
                        "cpu_percent": round(Some(used / (b.0 - a.0) * 100.0)),
                        "cpu_ms_per_tick": round((!ticks.is_empty()).then(|| used * 1e3 / ticks.len() as f64)),
                    }),
                );
            }
        }
        let rss: Vec<f64> = stats.iter().map(|s| s.1 as f64 / 1024.0).collect();
        let cpu = match (stats.first(), stats.last()) {
            (Some(a), Some(b)) if b.0 > a.0 => {
                Some(b.2.saturating_sub(a.2) as f64 / (b.0 - a.0) * 100.0 / clock_ticks())
            }
            _ => None,
        };
        let samples = |key: &str| {
            let mut v: Vec<f64> = d
                .bots
                .iter()
                .filter(|(t, _, _)| inside(*t))
                .flat_map(|(_, _, r)| r[key].as_array().cloned().unwrap_or_default())
                .filter_map(|x| x.as_f64())
                .collect();
            v.sort_by(f64::total_cmp);
            v
        };
        // Per group the first and last report of the window: counter deltas.
        let delta = |key: &str| -> u64 {
            (0..GROUPS.len())
                .map(|g| {
                    let mut it = d.bots.iter().filter(|(t, i, _)| *i == g && inside(*t));
                    let first = it.next().map_or(0, |r| r.2[key].as_u64().unwrap_or(0));
                    let last = it
                        .next_back()
                        .map_or(first, |r| r.2[key].as_u64().unwrap_or(0));
                    last.saturating_sub(first)
                })
                .sum()
        };
        let online: u64 = (0..GROUPS.len())
            .map(|g| {
                d.bots
                    .iter()
                    .rfind(|(t, i, _)| *i == g && inside(*t))
                    .and_then(|r| r.2["online"].as_u64())
                    .unwrap_or(0)
            })
            .sum();
        let mut errors = serde_json::Map::new();
        for (_, _, r) in d.bots.iter().filter(|(t, _, _)| inside(*t)) {
            if let Some(e) = r["errors"].as_object() {
                for (k, v) in e {
                    let n = errors.get(k).and_then(Value::as_u64).unwrap_or(0);
                    errors.insert(k.clone(), json!(n + v.as_u64().unwrap_or(0)));
                }
            }
        }
        let chunk_ms = samples("chunk_ms");
        let full_view = samples("full_view_ms");
        let rtt = samples("rtt_ms");
        let seconds = (to - from).max(1e-9);
        json!({
            "seconds": round(Some(to - from)),
            "mspt": {
                "ticks": ticks.len(),
                "mean": round(mean(&ticks)),
                "p50": round(pct(&ticks, 50.0)),
                "p95": round(pct(&ticks, 95.0)),
                "p99": round(pct(&ticks, 99.0)),
                "max": round(ticks.last().copied()),
                "over_50ms": ticks.iter().filter(|&&t| t > 50.0).count(),
            },
            "tps": round(Some((ticks.len() as f64 / seconds).min(20.0))),
            "rss_mib": {
                "min": round(rss.iter().copied().reduce(f64::min)),
                "mean": round(mean(&rss)),
                "max": round(rss.iter().copied().reduce(f64::max)),
                "last": round(rss.last().copied()),
            },
            "cpu_percent": round(cpu),
            "threads": threads,
            "bots": {
                "online": online,
                "chunks_per_s": round(Some(delta("chunks") as f64 / seconds)),
                "chunk_ms_p50": round(pct(&chunk_ms, 50.0)),
                "chunk_ms_p95": round(pct(&chunk_ms, 95.0)),
                "chunk_ms_max": round(chunk_ms.last().copied()),
                "full_view_ms_p50": round(pct(&full_view, 50.0)),
                "rtt_ms_p50": round(pct(&rtt, 50.0)),
                "rtt_ms_p95": round(pct(&rtt, 95.0)),
                "teleports": delta("teleports"),
                "attacks": delta("attacks"),
                "placed": delta("placed"),
                "broken": delta("broken"),
                "deaths": delta("deaths"),
                "disconnected": delta("disconnected"),
                "errors": errors,
            },
        })
    }

    fn stop(mut self) {
        for mut b in std::mem::take(&mut self.bots) {
            let _ = writeln!(b.input, "quit");
            drop(b.input);
            let _ = b.child.wait();
        }
        let _ = writeln!(self.console, "stop");
        let _ = self.console.flush();
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.server.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        eprintln!("warning: the server did not stop within 90 s; killing it");
        if let Some(pid) = self.pid {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn clock_ticks() -> f64 {
    command_output(Command::new("getconf").arg("CLK_TCK"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100.0)
}

/// The server process: `child` itself or its newest descendant named `vanillite` (the
/// child may be `sudo`).
fn server_pid(child: u32) -> Option<u32> {
    let comm = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok();
    let mut todo = vec![child];
    while let Some(pid) = todo.pop() {
        if comm(pid).is_some_and(|c| c.trim() == "vanillite") {
            return Some(pid);
        }
        let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"));
        todo.extend(
            children
                .unwrap_or_default()
                .split_whitespace()
                .filter_map(|p| p.parse::<u32>().ok()),
        );
    }
    None
}

/// User plus system time in clock ticks from a `stat` file of `/proc`.
fn stat_cpu(stat: &str) -> u64 {
    let Some((_, rest)) = stat.rsplit_once(") ") else {
        return 0;
    };
    let f: Vec<u64> = rest
        .split_whitespace()
        .take(13)
        .map(|v| v.parse().unwrap_or(0))
        .collect();
    f.get(11).copied().unwrap_or(0) + f.get(12).copied().unwrap_or(0)
}

/// Every second: RSS and CPU time of the server, and the new lines of its tick log.
fn sample(pid: u32, tick_log: &Path, start: Instant, data: &Mutex<Collected>) {
    let mut offset = 0u64;
    let mut rest = String::new();
    loop {
        let t = start.elapsed().as_secs_f64();
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return;
        };
        let rss = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .unwrap_or(0);
        let cpu = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| stat_cpu(&s))
            .unwrap_or(0);
        let mut new = String::new();
        if let Ok(mut f) = std::fs::File::open(tick_log)
            && f.seek(SeekFrom::Start(offset)).is_ok()
            && let Ok(n) = f.read_to_string(&mut new)
        {
            offset += n as u64;
        }
        rest.push_str(&new);
        let complete = rest.rfind('\n').map_or(0, |i| i + 1);
        let lines: Vec<f64> = rest[..complete]
            .lines()
            .filter_map(|l| l.trim().parse::<f64>().ok())
            .map(|ns| ns / 1e6)
            .collect();
        rest.drain(..complete);
        let mut groups = [0u64; 4];
        if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            for task in tasks.flatten() {
                let path = task.path();
                let (Ok(comm), Ok(stat)) = (
                    std::fs::read_to_string(path.join("comm")),
                    std::fs::read_to_string(path.join("stat")),
                ) else {
                    continue;
                };
                groups[thread_group(comm.trim())] += stat_cpu(&stat);
            }
        }
        if let Ok(mut d) = data.lock() {
            d.stats.push((t, rss, cpu));
            d.threads.push((t, groups));
            d.ticks.extend(lines.into_iter().map(|ms| (t, ms)));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
