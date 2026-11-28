//! `cargo xtask parity coverage` (#257): which of Vanilla's code the parity scenarios exercise.
//!
//! The scenario recorder plays every Vanilla-recorded scenario of
//! `crates/vanillite-test/scenarios/` (fast and bulk tier) on the pinned jar with the JaCoCo
//! agent (a tool only, downloaded from Maven Central into `target/vanilla/jacoco/`);
//! `--fuzz-minutes N` adds a `parity fuzz` run under the same agent. JaCoCo's report (HTML per
//! package, class and method, and XML) is summarised per gameplay system into `summary.md` and
//! `coverage.json` in `target/parity-coverage/` (never committed, #213). `cargo xtask parity
//! fuzz` reads that `coverage.json` and weights its generated scenarios towards what is
//! least exercised. `--history <file>` appends the run's per-system figures as one JSON line
//! (the nightly on vanillite-test2 does, #267); `--trend [<file>]` prints that history.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::golden;
use crate::vanilla::{download, java_tool, remove_dir, run, sha1_file};
use crate::{Result, write};

const JACOCO: &str = "0.8.16";
/// Maven Central paths and SHA-1 of the agent and the command line tool.
const AGENT: (&str, &str) = (
    "org/jacoco/org.jacoco.agent/0.8.16/org.jacoco.agent-0.8.16-runtime.jar",
    "4325ec32eef129534b61041814d25ec88e3023ce",
);
const CLI: (&str, &str) = (
    "org/jacoco/org.jacoco.cli/0.8.16/org.jacoco.cli-0.8.16-nodeps.jar",
    "00718402b9bf534b3c8dceb66408b3c568907e64",
);
/// Scenarios per recorder JVM.
const CHUNK: usize = 8;
/// A recorder that finishes no scenario for this long is stopped.
const STALL: Duration = Duration::from_secs(15 * 60);
/// Areas listed in the summary.
const TOP: usize = 30;

/// The output directory; `coverage.json` there steers `parity fuzz`.
pub fn out_dir(root: &Path) -> PathBuf {
    root.join("target/parity-coverage")
}

/// The history file `--history` and `--trend` use when given no path.
fn default_history(root: &Path) -> PathBuf {
    out_dir(root).join("history.jsonl")
}

/// `cargo xtask parity coverage [--jobs N] [--heap SIZE] [--fuzz-minutes N] [--report-only]
/// [--history FILE] [<scenario>...]` or `cargo xtask parity coverage --trend [FILE]`.
pub fn run_coverage(root: &Path, args: &[&str]) -> Result<()> {
    if let Some(i) = args.iter().position(|a| *a == "--trend") {
        let file = args
            .get(i + 1)
            .map_or_else(|| default_history(root), PathBuf::from);
        print!("{}", trend(&file)?);
        return Ok(());
    }
    let mut jobs = std::thread::available_parallelism().map_or(1, |n| (n.get() / 4).max(1));
    let mut heap = "4g";
    let mut fuzz_minutes = None;
    let mut report_only = false;
    let mut history = None;
    let mut names = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match *a {
            "--jobs" => jobs = value()?.parse().map_err(|e| format!("--jobs: {e}"))?,
            "--fuzz-minutes" => {
                fuzz_minutes = Some(*value()?);
            }
            "--heap" => heap = *value()?,
            "--history" => history = Some(PathBuf::from(value()?)),
            "--report-only" => report_only = true,
            other if other.starts_with("--") => return Err(format!("unknown argument {other}")),
            name => names.push(name),
        }
    }
    let key = golden::Key::current(root)?;
    let out = out_dir(root);
    let tools = root.join("target/vanilla/jacoco");
    let agent = fetch(&tools, AGENT)?;
    let cli = fetch(&tools, CLI)?;
    let exec_dir = out.join("exec");
    let work_dir = root.join("target/vanilla").join(&key.minecraft);
    // The last run's figures, for `--report-only`.
    let mut stats = crate::read_json(&out.join("coverage.json"))
        .map(|j| j["run"].clone())
        .unwrap_or_else(|_| json!({}));
    if !report_only {
        for d in ["exec", "traces", "logs", "html"] {
            remove_dir(&out.join(d))?;
            std::fs::create_dir_all(out.join(d)).map_err(|e| e.to_string())?;
        }
        let (classes, cp, broken) = golden::harness(root, &key)?;
        if broken
            .files
            .iter()
            .any(|f| f.ends_with("ScenarioRecorder.java"))
        {
            return Err(format!(
                "the scenario recorder does not compile against {}:\n{}",
                key.minecraft, broken.errors
            ));
        }
        let parity_agent = golden::agent_jar(&classes, &work_dir)?;
        let sep = if cfg!(windows) { ";" } else { ":" };
        let classpath = format!("{}{sep}{cp}", classes.display());
        let mut scenarios = golden::scenario_files(root, &key)?;
        if !names.is_empty() {
            scenarios.retain(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| names.contains(&s))
            });
        }
        println!(
            "coverage: recording {} scenarios on {jobs} JVMs with JaCoCo {JACOCO}",
            scenarios.len()
        );
        let started = Instant::now();
        let java = |exec: &Path| {
            let mut cmd = Command::new(java_tool("java"));
            // JaCoCo first: it must see the jar's class bytes, before the parity agent changes
            // some of them, or its class ids do not match the jar's.
            cmd.current_dir(&work_dir)
                .arg(jacoco_option(&agent, exec))
                .arg(format!("-javaagent:{}", parity_agent.display()))
                // A small scenario world needs little; the default (a quarter of the RAM per
                // JVM) invites the OOM killer on a shared machine.
                .arg(format!("-Xmx{heap}"))
                .arg("-cp")
                .arg(&classpath)
                .arg("dev.vanillite.extractor.golden.ScenarioRecorder");
            cmd
        };
        let failed = record_all(&java, &scenarios, &out, jobs)?;
        println!(
            "coverage: recorded in {:.0} s, {} failed",
            started.elapsed().as_secs_f64(),
            failed.len()
        );
        stats = json!({ "scenarios": scenarios.len(), "failed": failed });
        if let Some(minutes) = fuzz_minutes {
            println!("coverage: parity fuzz for {minutes} minutes");
            let opt = jacoco_option(&agent, &exec_dir.join("fuzz.exec")) + ",append=true";
            if let Err(e) = crate::parity::driver(
                root,
                &["--minutes", minutes],
                Some(("JAVA_TOOL_OPTIONS", opt)),
            ) {
                println!("coverage: the fuzz run failed ({e}); its coverage is kept");
            }
            stats["fuzz_minutes"] = json!(minutes.parse::<u64>().unwrap_or(0));
        }
        remove_dir(&out.join("traces"))?;
    }

    let mut execs: Vec<PathBuf> = std::fs::read_dir(&exec_dir)
        .map_err(|e| format!("{}: {e}", exec_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "exec"))
        .collect();
    execs.sort();
    if execs.is_empty() {
        return Err(format!("no coverage data in {}", exec_dir.display()));
    }
    let jar = work_dir
        .join("versions")
        .join(&key.minecraft)
        .join(format!("server-{}.jar", key.minecraft));
    let xml = out.join("jacoco.xml");
    run(Command::new(java_tool("java"))
        .arg("-jar")
        .arg(&cli)
        .arg("report")
        .args(&execs)
        .arg("--classfiles")
        .arg(&jar)
        .arg("--name")
        .arg(format!("Vanilla {} (parity scenarios)", key.minecraft))
        .arg("--xml")
        .arg(&xml)
        .arg("--html")
        .arg(out.join("html")))?;
    let text = std::fs::read_to_string(&xml).map_err(|e| format!("{}: {e}", xml.display()))?;
    let classes: Vec<Class> = parse(&text)
        .into_iter()
        .filter(|c| c.name.starts_with("net/minecraft/"))
        .collect();
    let (classes, excluded) = exclude(classes);
    stats["excluded_instructions"] = json!(excluded.im + excluded.ic);
    let (json, md) = summarise(&key.minecraft, &classes, stats);
    write(&out.join("coverage.json"), &(json.to_string() + "\n"))?;
    write(&out.join("summary.md"), &md)?;
    if let Some(file) = history {
        append_history(root, &file, &json)?;
        println!("coverage: appended to {}", file.display());
    }
    let g = &json["gameplay"]["instructions"];
    println!(
        "coverage: {}% of Vanilla's gameplay code exercised (instructions); {} (per method: {})",
        g["percent"],
        out.join("summary.md").display(),
        out.join("html/index.html").display()
    );
    Ok(())
}

/// One line of the history: date, commit, run, gameplay and per-system percentages.
fn append_history(root: &Path, file: &Path, json: &Value) -> Result<()> {
    use std::io::Write as _;
    let pct = |v: &Value| json!([v["instructions"]["percent"], v["branches"]["percent"]]);
    let systems: serde_json::Map<String, Value> = json["systems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some((s["name"].as_str()?.to_owned(), pct(s))))
        .collect();
    let date =
        crate::loadtest::command_output(Command::new("date").args(["-u", "+%Y-%m-%d %H:%M"]))?;
    let commit = crate::loadtest::git(root, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_owned());
    let run = &json["run"];
    let line = json!({
        "date": date.trim(),
        "commit": commit.trim(),
        "minecraft": json["minecraft"],
        "scenarios": run["scenarios"],
        "failed": run["failed"].as_array().map_or(0, Vec::len),
        "gameplay": pct(&json["gameplay"]),
        "total": pct(&json["total"]),
        "systems": systems,
    });
    if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("{}: {e}", file.display()))
}

/// The history as Markdown: gameplay coverage per run (the last 30), then every system's
/// instructions and branches in the first, the previous and the latest run.
fn trend(file: &Path) -> Result<String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let runs: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let (Some(first), Some(last)) = (runs.first(), runs.last()) else {
        return Err(format!("{}: no runs", file.display()));
    };
    let prev = &runs[runs.len().saturating_sub(2)];
    let p = |v: &Value, i: usize| {
        v[i].as_f64()
            .map_or_else(|| "-".to_owned(), |x| format!("{x:.1}"))
    };
    let mut md = format!(
        "# Vanilla code exercised by the parity scenarios ({} runs, {})\n\n\
         | Date (UTC) | Commit | Scenarios | Failed | Gameplay instr. % | Gameplay branches % | All instr. % |\n\
         |---|---|---|---|---|---|---|\n",
        runs.len(),
        file.display()
    );
    for r in runs.iter().rev().take(30).rev() {
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} |",
            r["date"].as_str().unwrap_or("-"),
            r["commit"].as_str().unwrap_or("-"),
            r["scenarios"],
            r["failed"],
            p(&r["gameplay"], 0),
            p(&r["gameplay"], 1),
            p(&r["total"], 0),
        );
    }
    let _ = writeln!(
        md,
        "\n## Per system (instructions % / branches %)\n\n\
         | System | first ({}) | previous ({}) | latest ({}) | change since previous |\n\
         |---|---|---|---|---|",
        first["date"].as_str().unwrap_or("-"),
        prev["date"].as_str().unwrap_or("-"),
        last["date"].as_str().unwrap_or("-"),
    );
    let both = |r: &Value, s: &str| {
        let v = &r["systems"][s];
        format!("{} / {}", p(v, 0), p(v, 1))
    };
    for (name, _) in SYSTEMS {
        let delta = |i: usize| match (
            last["systems"][name][i].as_f64(),
            prev["systems"][name][i].as_f64(),
        ) {
            (Some(a), Some(b)) => format!("{:+.1}", a - b),
            _ => "-".to_owned(),
        };
        let _ = writeln!(
            md,
            "| {name} | {} | {} | {} | {} / {} |",
            both(first, name),
            both(prev, name),
            both(last, name),
            delta(0),
            delta(1),
        );
    }
    Ok(md)
}

fn jacoco_option(agent: &Path, exec: &Path) -> String {
    format!(
        "-javaagent:{}=destfile={},includes=net.minecraft.*,output=file",
        agent.display(),
        exec.display()
    )
}

/// A JaCoCo jar from Maven Central, checked against its pinned SHA-1.
fn fetch(dir: &Path, (path, sha1): (&str, &str)) -> Result<PathBuf> {
    let file = dir.join(path.rsplit('/').next().unwrap_or(path));
    if file.exists() && sha1_file(&file)? == sha1 {
        return Ok(file);
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    download(&format!("https://repo1.maven.org/maven2/{path}"), &file)?;
    let actual = sha1_file(&file)?;
    if actual != sha1 {
        return Err(format!(
            "{}: SHA-1 {actual}, expected {sha1}",
            file.display()
        ));
    }
    Ok(file)
}

/// Records the scenarios in chunks on `jobs` recorder JVMs, each with its own coverage file.
/// A scenario that fails ends its JVM: it is counted as failed and the rest of its chunk goes
/// back into the queue. Returns the failed scenarios.
fn record_all(
    java: &(dyn Fn(&Path) -> Command + Sync),
    scenarios: &[PathBuf],
    out: &Path,
    jobs: usize,
) -> Result<Vec<String>> {
    let queue: Mutex<VecDeque<usize>> = Mutex::new((0..scenarios.len()).collect());
    let failed = Mutex::new(Vec::new());
    // Scenarios that failed once: a recorder JVM can also die from outside (the OOM killer of
    // a shared machine), so each gets a second try in a JVM of its own.
    let retried = Mutex::new(std::collections::HashSet::new());
    let runs = std::sync::atomic::AtomicUsize::new(0);
    let trace = |i: usize| out.join("traces").join(format!("{i}.json"));
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| {
                loop {
                    let chunk: Vec<usize> = {
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        let retry = q
                            .front()
                            .is_some_and(|i| retried.lock().is_ok_and(|r| r.contains(i)));
                        let n = q.len().min(if retry { 1 } else { CHUNK });
                        q.drain(..n).collect()
                    };
                    if chunk.is_empty() {
                        return;
                    }
                    let n = runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut cmd = java(&out.join("exec").join(format!("run-{n:04}.exec")));
                    for &i in &chunk {
                        cmd.arg(&scenarios[i]).arg(trace(i));
                    }
                    let log =
                        std::fs::File::create(out.join("logs").join(format!("run-{n:04}.log")));
                    let (stdout, stderr) = match log.and_then(|l| Ok((l.try_clone()?, l))) {
                        Ok((a, b)) => (Stdio::from(a), Stdio::from(b)),
                        Err(_) => (Stdio::null(), Stdio::null()),
                    };
                    let done =
                        |chunk: &[usize]| chunk.iter().filter(|&&i| trace(i).exists()).count();
                    match cmd.stdout(stdout).stderr(stderr).spawn() {
                        Ok(mut child) => watch(&mut child, || done(&chunk)),
                        Err(e) => println!("coverage: java: {e}"),
                    }
                    let ok = done(&chunk);
                    if ok < chunk.len() {
                        let first = retried
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(chunk[ok]);
                        let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                        q.extend(&chunk[ok + 1..]);
                        let name = scenarios[chunk[ok]].display().to_string();
                        if first {
                            q.push_front(chunk[ok]);
                        } else {
                            println!("coverage: {name} failed (log: logs/run-{n:04}.log)");
                            failed.lock().unwrap_or_else(|e| e.into_inner()).push(name);
                        }
                    }
                }
            });
        }
    });
    Ok(failed.into_inner().unwrap_or_else(|e| e.into_inner()))
}

/// Waits for a recorder; one that makes no progress for [`STALL`] is stopped with SIGTERM (its
/// shutdown hooks still write the coverage), then killed.
fn watch(child: &mut std::process::Child, progress: impl Fn() -> usize) {
    let mut last = (progress(), Instant::now());
    loop {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        let p = progress();
        if p != last.0 {
            last = (p, Instant::now());
        } else if last.1.elapsed() > STALL {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            let end = Instant::now() + Duration::from_secs(60);
            while Instant::now() < end && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Missed and covered instructions, branches and methods.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    im: u64,
    ic: u64,
    bm: u64,
    bc: u64,
    mm: u64,
    mc: u64,
}

impl Counts {
    fn add(&mut self, o: &Counts) {
        self.im += o.im;
        self.ic += o.ic;
        self.bm += o.bm;
        self.bc += o.bc;
        self.mm += o.mm;
        self.mc += o.mc;
    }

    fn json(&self) -> Value {
        let part = |m: u64, c: u64| json!({"missed": m, "covered": c, "percent": percent(c, m)});
        json!({
            "instructions": part(self.im, self.ic),
            "branches": part(self.bm, self.bc),
            "methods": part(self.mm, self.mc),
        })
    }
}

fn percent(covered: u64, missed: u64) -> f64 {
    if covered + missed == 0 {
        return 0.0;
    }
    (covered as f64 * 1000.0 / (covered + missed) as f64).round() / 10.0
}

/// A method: name and its missed and covered instructions.
#[derive(Debug)]
struct Method {
    name: String,
    desc: String,
    missed: u64,
    covered: u64,
    branches_missed: u64,
    branches_covered: u64,
}

#[derive(Debug)]
struct Class {
    /// `net/minecraft/world/entity/monster/Zombie`.
    name: String,
    counts: Counts,
    methods: Vec<Method>,
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn attr<'a>(attrs: &'a str, key: &str) -> &'a str {
    let pat = format!(" {key}=\"");
    attrs
        .find(&pat)
        .map(|i| &attrs[i + pat.len()..])
        .and_then(|r| r.split('"').next())
        .unwrap_or("")
}

/// The classes of a JaCoCo XML report with their counters and methods.
fn parse(xml: &str) -> Vec<Class> {
    let mut classes = Vec::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut class: Option<Class> = None;
    let mut method: Option<Method> = None;
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            match name.trim() {
                "method" => {
                    if let (Some(c), Some(m)) = (class.as_mut(), method.take()) {
                        c.methods.push(m);
                    }
                }
                "class" => classes.extend(class.take()),
                _ => {}
            }
            stack.pop();
            continue;
        }
        let closed = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, attrs) = tag
            .find(char::is_whitespace)
            .map_or((tag, ""), |i| (&tag[..i], &tag[i..]));
        match name {
            "class" => {
                class = Some(Class {
                    name: unescape(attr(attrs, "name")),
                    counts: Counts::default(),
                    methods: Vec::new(),
                });
            }
            "method" => {
                method = Some(Method {
                    name: unescape(attr(attrs, "name")),
                    desc: attr(attrs, "desc").to_owned(),
                    missed: 0,
                    covered: 0,
                    branches_missed: 0,
                    branches_covered: 0,
                });
            }
            "counter" => {
                let missed = attr(attrs, "missed").parse().unwrap_or(0);
                let covered = attr(attrs, "covered").parse().unwrap_or(0);
                let kind = attr(attrs, "type");
                match stack.last() {
                    Some(&"method") => {
                        if let Some(m) = method.as_mut() {
                            match kind {
                                "INSTRUCTION" => (m.missed, m.covered) = (missed, covered),
                                "BRANCH" => {
                                    (m.branches_missed, m.branches_covered) = (missed, covered);
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(&"class") => {
                        if let Some(c) = class.as_mut() {
                            let k = &mut c.counts;
                            match kind {
                                "INSTRUCTION" => (k.im, k.ic) = (missed, covered),
                                "BRANCH" => (k.bm, k.bc) = (missed, covered),
                                "METHOD" => (k.mm, k.mc) = (missed, covered),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if !closed {
            stack.push(name);
        }
    }
    classes
}

/// Leaves out code a server never runs in game: the creative tabs (client), the registry
/// bootstraps of data generation (`bootstrap(BootstrapContext)` and its lambdas), `toString` and
/// crash report details. Returns the rest and what was left out.
fn exclude(classes: Vec<Class>) -> (Vec<Class>, Counts) {
    let mut excluded = Counts::default();
    let mut kept = Vec::with_capacity(classes.len());
    for mut c in classes {
        if c.name
            .starts_with("net/minecraft/world/item/CreativeModeTab")
        {
            excluded.add(&c.counts);
            continue;
        }
        let datagen = c
            .methods
            .iter()
            .any(|m| m.name == "bootstrap" && m.desc.contains("BootstrapContext"));
        let (out, keep): (Vec<Method>, Vec<Method>) = c.methods.into_iter().partition(|m| {
            m.desc.contains("BootstrapContext")
                || (datagen && m.name.starts_with("lambda$bootstrap$"))
                || m.name == "toString"
                || m.name == "fillCrashReportCategory"
        });
        for m in &out {
            let d = Counts {
                im: m.missed,
                ic: m.covered,
                bm: m.branches_missed,
                bc: m.branches_covered,
                mm: u64::from(m.covered == 0),
                mc: u64::from(m.covered > 0),
            };
            excluded.add(&d);
            let k = &mut c.counts;
            k.im = k.im.saturating_sub(d.im);
            k.ic = k.ic.saturating_sub(d.ic);
            k.bm = k.bm.saturating_sub(d.bm);
            k.bc = k.bc.saturating_sub(d.bc);
            k.mm = k.mm.saturating_sub(d.mm);
            k.mc = k.mc.saturating_sub(d.mc);
        }
        c.methods = keep;
        kept.push(c);
    }
    (kept, excluded)
}

/// The systems in summary order: name and whether it counts as gameplay code. World generation
/// has its own sweeps against Vanilla (`worldgen sweep`, `light sweep`); storage, network and
/// infrastructure are no in-game behaviour. `crates/vanillite-test/src/fuzz.rs` weights its
/// behaviours by these names.
const SYSTEMS: &[(&str, bool)] = &[
    ("entity AI", true),
    ("mobs", true),
    ("player", true),
    ("entities", true),
    ("combat", true),
    ("projectiles", true),
    ("vehicles", true),
    ("effects, enchantments & potions", true),
    ("items", true),
    ("inventory & crafting", true),
    ("villager trading", true),
    ("blocks", true),
    ("block entities", true),
    ("redstone", true),
    ("fluids", true),
    ("world & level", true),
    ("chunks, lighting & level loading", true),
    ("commands", true),
    ("advancements & statistics", true),
    ("loot", true),
    ("scoreboard, teams & boss bars", true),
    ("data packs & registries", true),
    ("world generation", false),
    ("storage & data fixing", false),
    ("network", false),
    ("infrastructure", false),
];

/// Block classes that belong to redstone.
const REDSTONE: &[&str] = &[
    "Redstone",
    "Piston",
    "Diode",
    "Repeater",
    "Comparator",
    "Observer",
    "Lever",
    "Button",
    "PressurePlate",
    "Dispenser",
    "Dropper",
    "DaylightDetector",
    "TargetBlock",
    "TripWire",
    "Hopper",
    "Crafter",
    "Rail",
    "NoteBlock",
    "Sculk",
    "CopperBulb",
];

/// The system of a class (`net/minecraft/…`).
fn system(class: &str) -> &'static str {
    let p = class.strip_prefix("net/minecraft/").unwrap_or(class);
    let simple = p.rsplit('/').next().unwrap_or(p);
    let any = |prefixes: &[&str]| prefixes.iter().any(|x| p.starts_with(x));
    if any(&[
        "world/entity/ai/",
        "world/entity/schedule/",
        "world/level/pathfinder/",
    ]) {
        "entity AI"
    } else if any(&["world/entity/vehicle/"]) {
        "vehicles"
    } else if any(&["world/entity/projectile/"]) {
        "projectiles"
    } else if any(&[
        "world/entity/monster/",
        "world/entity/animal/",
        "world/entity/npc/",
        "world/entity/boss/",
        "world/entity/ambient/",
        "world/entity/raid/",
    ]) {
        "mobs"
    } else if any(&[
        "world/entity/player/",
        "server/level/ServerPlayer",
        "server/network/ServerGamePacketListenerImpl",
        "server/players/",
        "world/food/",
    ]) {
        "player"
    } else if any(&["world/damagesource/"]) {
        "combat"
    } else if any(&[
        "world/effect/",
        "world/item/enchantment/",
        "world/item/alchemy/",
    ]) {
        "effects, enchantments & potions"
    } else if any(&["world/entity/"]) {
        "entities"
    } else if any(&[
        "world/inventory/",
        "world/item/crafting/",
        "world/Container",
        "world/SimpleContainer",
    ]) {
        "inventory & crafting"
    } else if any(&["world/item/trading/"]) {
        "villager trading"
    } else if any(&["world/item/"]) {
        "items"
    } else if any(&["world/level/redstone/", "world/level/block/piston/"])
        || (any(&["world/level/block/"]) && REDSTONE.iter().any(|k| simple.contains(k)))
    {
        "redstone"
    } else if any(&["world/level/material/"])
        || (any(&["world/level/block/"])
            && (simple.starts_with("LiquidBlock") || simple.starts_with("BubbleColumn")))
    {
        "fluids"
    } else if any(&["world/level/block/entity/"]) {
        "block entities"
    } else if any(&["world/level/block/"]) {
        "blocks"
    } else if any(&[
        "world/level/levelgen/",
        "world/level/biome/",
        "data/worldgen/",
    ]) {
        "world generation"
    } else if any(&["world/level/storage/loot/"]) {
        "loot"
    } else if any(&[
        "world/level/storage/",
        "world/level/chunk/storage/",
        "nbt/",
        "util/datafix/",
    ]) {
        "storage & data fixing"
    } else if any(&[
        "world/level/chunk/",
        "world/level/lighting/",
        "server/level/",
    ]) {
        "chunks, lighting & level loading"
    } else if any(&["commands/", "server/commands/"]) {
        "commands"
    } else if any(&["advancements/", "stats/"]) {
        "advancements & statistics"
    } else if any(&[
        "world/scores/",
        "server/bossevents/",
        "server/ServerScoreboard",
        "world/BossEvent",
    ]) {
        "scoreboard, teams & boss bars"
    } else if any(&["world/"]) {
        "world & level"
    } else if any(&[
        "tags/",
        "resources/",
        "core/",
        "server/packs/",
        "server/ReloadableServerResources",
        "server/ServerFunction",
        "server/ServerAdvancementManager",
    ]) {
        "data packs & registries"
    } else if any(&["network/", "server/network/"]) {
        "network"
    } else {
        "infrastructure"
    }
}

fn is_gameplay(system: &str) -> bool {
    SYSTEMS.iter().any(|(s, g)| *s == system && *g)
}

/// An outer class with its inner classes: counts and methods (name, missed, covered).
type Area = (Counts, Vec<(String, u64, u64)>);

/// `coverage.json` and `summary.md`.
fn summarise(minecraft: &str, classes: &[Class], stats: Value) -> (Value, String) {
    let mut systems: BTreeMap<&str, (Counts, usize, usize)> = BTreeMap::new();
    let mut total = Counts::default();
    let mut gameplay = Counts::default();
    // Outer class → counts, methods (inner classes merged in).
    let mut outer: BTreeMap<&str, Area> = BTreeMap::new();
    let mut packages: BTreeMap<&str, Counts> = BTreeMap::new();
    let mut class_json = serde_json::Map::new();
    for c in classes {
        let sys = system(&c.name);
        let e = systems.entry(sys).or_default();
        e.0.add(&c.counts);
        e.1 += 1;
        e.2 += usize::from(c.counts.ic > 0);
        total.add(&c.counts);
        let uncovered: Vec<&str> = c
            .methods
            .iter()
            .filter(|m| m.covered == 0 && m.missed > 0)
            .map(|m| m.name.as_str())
            .collect();
        class_json.insert(
            c.name.clone(),
            json!({
                "system": sys, "im": c.counts.im, "ic": c.counts.ic, "bm": c.counts.bm,
                "bc": c.counts.bc, "mm": c.counts.mm, "mc": c.counts.mc, "uncovered": uncovered,
            }),
        );
        if !is_gameplay(sys) {
            continue;
        }
        gameplay.add(&c.counts);
        let base = c.name.split('$').next().unwrap_or(&c.name);
        let o = outer.entry(base).or_default();
        o.0.add(&c.counts);
        let inner = c.name.split_once('$').map(|(_, i)| i);
        for m in &c.methods {
            let name = inner.map_or_else(|| m.name.clone(), |i| format!("{i}.{}", m.name));
            o.1.push((name, m.missed, m.covered));
        }
        let package = c.name.rsplit_once('/').map_or("", |(p, _)| p);
        packages.entry(package).or_default().add(&c.counts);
    }

    let mut gaps: Vec<(&str, &Area)> = outer.iter().map(|(k, v)| (*k, v)).collect();
    gaps.sort_by_key(|(_, (c, _))| std::cmp::Reverse(c.im));
    gaps.truncate(TOP);
    let gap_methods = |methods: &[(String, u64, u64)]| {
        let mut m: Vec<&(String, u64, u64)> =
            methods.iter().filter(|m| m.2 == 0 && m.1 > 0).collect();
        m.sort_by_key(|m| std::cmp::Reverse(m.1));
        m.into_iter()
            .take(8)
            .map(|m| m.0.clone())
            .collect::<Vec<_>>()
    };
    let mut package_gaps: Vec<(&str, &Counts)> = packages.iter().map(|(k, v)| (*k, v)).collect();
    package_gaps.sort_by_key(|(_, c)| std::cmp::Reverse(c.im));
    package_gaps.truncate(TOP);

    let system_json: Vec<Value> = SYSTEMS
        .iter()
        .map(|(name, g)| {
            let (c, n, touched) = systems.get(name).copied().unwrap_or_default();
            let mut v = c.json();
            v["name"] = json!(name);
            v["gameplay"] = json!(g);
            v["classes"] = json!(n);
            v["classes_touched"] = json!(touched);
            v
        })
        .collect();
    let gaps_json: Vec<Value> = gaps
        .iter()
        .map(|(name, (c, methods))| {
            json!({
                "class": name, "system": system(name), "missed": c.im,
                "percent": percent(c.ic, c.im), "uncovered_methods": gap_methods(methods),
            })
        })
        .collect();
    let mut json = json!({
        "minecraft": minecraft,
        "jacoco": JACOCO,
        "run": stats,
        "gameplay": gameplay.json(),
        "total": total.json(),
        "systems": system_json,
        "gaps": gaps_json,
        "classes": class_json,
    });
    json["gameplay"]["note"] = json!(
        "net.minecraft without world generation (own sweeps), storage, network and infrastructure"
    );

    let mut md = String::new();
    let _ = writeln!(
        md,
        "# Vanilla {minecraft} code exercised by the parity scenarios\n\n\
         JaCoCo {JACOCO} on the scenario recorder (`cargo xtask parity coverage`, #257); run: \
         `{}`.\n\n\
         **Gameplay code exercised: {}% of instructions, {}% of branches, {}% of methods** \
         (net.minecraft without world generation, storage, network and infrastructure; all of \
         net.minecraft: {}% of instructions). Not counted: code a server never runs in game \
         (creative tabs, data generation bootstraps, `toString`, crash reports). Registry \
         and data loading at startup count as covered by every scenario.\n",
        json["run"],
        percent(gameplay.ic, gameplay.im),
        percent(gameplay.bc, gameplay.bm),
        percent(gameplay.mc, gameplay.mm),
        percent(total.ic, total.im),
    );
    md.push_str(
        "## Per system\n\n| System | Gameplay | Instructions | Branches | Methods | Classes touched | Missed instructions |\n|---|---|---|---|---|---|---|\n",
    );
    for (name, g) in SYSTEMS {
        let (c, n, touched) = systems.get(name).copied().unwrap_or_default();
        let _ = writeln!(
            md,
            "| {name} | {} | {}% | {}% | {}% | {touched}/{n} | {} |",
            if *g { "yes" } else { "no" },
            percent(c.ic, c.im),
            percent(c.bc, c.bm),
            percent(c.mc, c.mm),
            c.im
        );
    }
    let _ = writeln!(
        md,
        "\n## The {TOP} biggest uncovered gameplay areas (classes by missed instructions)\n\n\
         | # | Class | System | Covered | Missed instructions | Biggest untouched methods |\n\
         |---|---|---|---|---|---|"
    );
    for (i, (name, (c, methods))) in gaps.iter().enumerate() {
        let _ = writeln!(
            md,
            "| {} | `{}` | {} | {}% | {} | {} |",
            i + 1,
            name.trim_start_matches("net/minecraft/").replace('/', "."),
            system(name),
            percent(c.ic, c.im),
            c.im,
            gap_methods(methods).join(", ")
        );
    }
    let _ = writeln!(
        md,
        "\n## The {TOP} gameplay packages with the most missed instructions\n\n\
         | Package | Covered | Missed instructions |\n|---|---|---|"
    );
    for (name, c) in &package_gaps {
        let _ = writeln!(
            md,
            "| `{}` | {}% | {} |",
            name.replace('/', "."),
            percent(c.ic, c.im),
            c.im
        );
    }
    md.push_str(
        "\nPer package, class and method: `html/index.html` next to this file. `cargo xtask \
         parity fuzz` weights its scenarios by `coverage.json`.\n",
    );
    (json, md)
}
