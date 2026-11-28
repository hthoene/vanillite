//! Soak runs (#166): long parity scenarios (one to several game days) against Vanilla.
//!
//! Soak scenarios are the files in `scenarios/soak/` (the scenario format of
//! [`crate::scenario`]); they run tens of thousands of ticks with the game rules of a real server
//! on (random ticks, mob spawning, day cycle, weather). No trace is held in memory: both sides
//! stream their frames, and frames compare by [`Trace::checksum`].
//!
//! 1. Vanilla's run is recorded once per scenario and golden key (Minecraft version, jar,
//!    harness version, scenario hash) into the bulk store `<bulk>/soak/`: `<name>-<sha>.sums`
//!    holds the checksum of every frame (little-endian `u64`, frame 0 first), `<name>-<sha>/`
//!    the full frame every [`SNAPSHOT`] ticks (one-frame traces, for looking at the state).
//!    The recorder streams the frames (`ScenarioRecorder --stream`).
//! 2. Vanillite runs the scenario (the server's parallel execution) and compares the checksum
//!    of every frame. At the first frame that differs it stops.
//! 3. Vanilla records again exactly up to that tick; its checksums on the way must equal the
//!    stored ones (else Vanilla's run is not reproducible, which the report says). The two
//!    frames of that tick are diffed in full ([`Trace::diff`]) and written to
//!    `<out>/<name>/{vanilla,vanillite}-<tick>.json`; `<out>/<name>.md` is the report.
//!
//! A known divergence (`scenarios/divergences/<name>.toml`, like for every scenario) marks a
//! first divergence at exactly its tick as known.

use std::fmt::Write as _;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::golden;
use crate::runner;
use crate::scenario::Scenario;
use crate::trace::{Frame, Trace};
use crate::world::Execution;

/// Every this many ticks the full Vanilla frame is kept next to its checksums.
pub const SNAPSHOT: u32 = 6000;

/// Differences written per divergence.
const DIFF_LIMIT: usize = 200;

/// Ticks of a game day.
const DAY: u32 = 24_000;

/// The Java recorder on the pinned jar (from `cargo xtask parity soak`).
#[derive(Clone, Debug)]
pub struct Java {
    pub java: PathBuf,
    pub agent: PathBuf,
    pub classpath: String,
    pub work_dir: PathBuf,
}

/// How a soak scenario ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every frame equals Vanilla's.
    Same,
    /// The first frame that differs.
    Diverges { tick: u32, known: bool },
    /// The time limit ended the run after this many ticks without a difference.
    Stopped(u32),
    /// The run failed (recorder, Vanillite panic, ...).
    Error(String),
}

/// The result of one soak scenario.
#[derive(Clone, Debug)]
pub struct Report {
    pub name: String,
    pub ticks: u32,
    pub verdict: Verdict,
    /// Time of the Vanilla recording, if it was recorded in this run.
    pub vanilla: Option<Duration>,
    /// Time of the Vanillite run and the ticks it ran.
    pub vanillite: Duration,
    pub ran: u32,
    /// What changed in the region during Vanillite's run (up to the first divergence).
    pub activity: Activity,
}

/// What changes in the region from frame to frame: a run without any of it is idle, and its
/// "same as Vanilla" says little.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    /// Frames compared with the one before.
    pub ticks: u32,
    /// Blocks whose state changed.
    pub blocks: u64,
    /// Block entities whose data changed, came or went.
    pub block_entities: u64,
    /// Entities that came (per type, the rise of its count).
    pub spawns: u64,
    /// Entities that went.
    pub deaths: u64,
    /// Item entities that came.
    pub items: u64,
}

impl Activity {
    /// Whether nothing changed but entities moving.
    pub fn idle(&self) -> bool {
        self.blocks == 0 && self.block_entities == 0 && self.spawns == 0 && self.deaths == 0
    }

    /// The counts per 100 ticks.
    pub fn per_100(&self) -> String {
        let rate = |n: u64| n as f64 * 100.0 / f64::from(self.ticks.max(1));
        format!(
            "{:.1} blocks, {:.1} block entities, +{:.1}/-{:.1} entities, {:.1} items",
            rate(self.blocks),
            rate(self.block_entities),
            rate(self.spawns),
            rate(self.deaths),
            rate(self.items)
        )
    }
}

type LastFrame = (
    std::sync::Arc<Vec<u32>>,
    std::collections::BTreeMap<String, u64>,
    Vec<String>,
);

/// Counts the [`Activity`] of a stream of frames.
#[derive(Default)]
struct ActivityMeter {
    activity: Activity,
    /// The last frame's blocks, entities per type and block entities.
    last: Option<LastFrame>,
}

impl ActivityMeter {
    fn frame(&mut self, frame: &Frame) {
        let mut kinds = std::collections::BTreeMap::new();
        for e in &frame.entities {
            *kinds.entry(e.kind.clone()).or_insert(0u64) += 1;
        }
        if let Some((blocks, last_kinds, block_entities)) = &self.last {
            let a = &mut self.activity;
            a.ticks += 1;
            a.blocks += blocks
                .iter()
                .zip(frame.blocks.iter())
                .filter(|(x, y)| x != y)
                .count() as u64;
            // Both lists are sorted: the entries only on one side.
            let (mut i, mut j) = (0, 0);
            let (old, new) = (block_entities, &frame.block_entities);
            while i < old.len() || j < new.len() {
                match (old.get(i), new.get(j)) {
                    (Some(x), Some(y)) if x == y => (i, j) = (i + 1, j + 1),
                    (Some(x), Some(y)) if x < y => {
                        (i, a.block_entities) = (i + 1, a.block_entities + 1)
                    }
                    (Some(_), None) => (i, a.block_entities) = (i + 1, a.block_entities + 1),
                    _ => (j, a.block_entities) = (j + 1, a.block_entities + 1),
                }
            }
            for (kind, n) in &kinds {
                let before = last_kinds.get(kind).copied().unwrap_or(0);
                let came = n.saturating_sub(before);
                a.spawns += came;
                if kind == "minecraft:item" {
                    a.items += came;
                }
            }
            for (kind, before) in last_kinds {
                a.deaths += before.saturating_sub(kinds.get(kind).copied().unwrap_or(0));
            }
        }
        self.last = Some((frame.blocks.clone(), kinds, frame.block_entities.clone()));
    }
}

impl Report {
    /// One row of the summary table.
    pub fn row(&self) -> String {
        let result = match &self.verdict {
            Verdict::Same if self.activity.idle() => "inactive (same as Vanilla)".to_owned(),
            Verdict::Same => "same as Vanilla".to_owned(),
            Verdict::Diverges { tick, known } => format!(
                "{}diverges at tick {tick} (day {}, +{})",
                if *known { "known: " } else { "" },
                tick / DAY + 1,
                tick % DAY
            ),
            Verdict::Stopped(t) => format!("time limit after {t} ticks, same so far"),
            Verdict::Error(e) => format!("ERROR: {}", e.lines().next().unwrap_or_default()),
        };
        let tps = self.ran as f64 / self.vanillite.as_secs_f64().max(1e-3);
        format!(
            "| {} | {} | {result} | {} | {} | {:.0} s ({tps:.0} ticks/s) |",
            self.name,
            self.ticks,
            self.activity.per_100(),
            self.vanilla.map_or_else(
                || "cached".to_owned(),
                |d| format!("{:.1} min", d.as_secs_f64() / 60.0)
            ),
            self.vanillite.as_secs_f64()
        )
    }

    /// Whether the nightly should flag it: errors and divergences that are not known.
    pub fn failed(&self) -> bool {
        matches!(
            self.verdict,
            Verdict::Error(_) | Verdict::Diverges { known: false, .. }
        )
    }
}

/// The golden files of a scenario: checksums and snapshot directory.
fn golden_paths(scenario: &Scenario) -> (PathBuf, PathBuf) {
    let dir = golden::bulk_dir().join("soak");
    let stem = format!("{}-{}", scenario.name, &scenario.sha1[..12]);
    (dir.join(format!("{stem}.sums")), dir.join(stem))
}

/// Runs one soak scenario (file `file`); results go to `out`. Stops at `deadline`.
pub fn run(
    scenario: &Scenario,
    file: &Path,
    java: &Java,
    out: &Path,
    deadline: Option<Instant>,
) -> Report {
    let mut report = Report {
        name: scenario.name.clone(),
        ticks: scenario.ticks,
        verdict: Verdict::Same,
        vanilla: None,
        vanillite: Duration::ZERO,
        ran: 0,
        activity: Activity::default(),
    };
    let dir = out.join(&scenario.name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        report.verdict = Verdict::Error(format!("{}: {e}", dir.display()));
        return report;
    }
    let mut text = format!(
        "# {}\n\n{}\n\n{} ticks ({:.1} game days).\n\n",
        scenario.name,
        scenario.description,
        scenario.ticks,
        f64::from(scenario.ticks) / f64::from(DAY)
    );
    match soak(scenario, file, java, &dir, deadline, &mut report, &mut text) {
        Ok(()) => {}
        Err(e) => {
            let _ = writeln!(text, "ERROR: {e}");
            report.verdict = Verdict::Error(e);
        }
    }
    let _ = writeln!(text, "\n{}", report.row());
    if let Err(e) = std::fs::write(out.join(format!("{}.md", scenario.name)), text) {
        eprintln!("soak: {}: {e}", scenario.name);
    }
    report
}

fn soak(
    scenario: &Scenario,
    file: &Path,
    java: &Java,
    dir: &Path,
    deadline: Option<Instant>,
    report: &mut Report,
    text: &mut String,
) -> Result<(), String> {
    let (sums_path, snapshots) = golden_paths(scenario);
    let sums = match read_sums(&sums_path, scenario.ticks)? {
        Some(s) => s,
        None => {
            let started = Instant::now();
            let sums = record_golden(scenario, file, java, dir, &sums_path, &snapshots, deadline)?;
            report.vanilla = Some(started.elapsed());
            let Some(sums) = sums else {
                report.verdict = Verdict::Stopped(0);
                let _ = writeln!(text, "The time limit ended the Vanilla recording.");
                return Ok(());
            };
            sums
        }
    };

    // Vanillite, frame by frame against the checksums.
    let started = Instant::now();
    let mut names = Vec::new();
    let mut diverged: Option<Trace> = None;
    let mut stopped = false;
    let mut ran = 0;
    let mut meter = ActivityMeter::default();
    let factory = crate::server_world::factory;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runner::stream(
            scenario,
            factory,
            Execution::Parallel,
            &mut |trace, frame| {
                ran = frame.tick;
                meter.frame(&frame);
                if sums.get(frame.tick as usize) != Some(&trace.checksum(&frame, &mut names)) {
                    diverged = Some(one_frame(trace, frame));
                    return Ok(false);
                }
                if frame.tick % 1000 == 0 && deadline.is_some_and(|d| Instant::now() >= d) {
                    stopped = true;
                    return Ok(false);
                }
                Ok(true)
            },
        )
    }));
    report.vanillite = started.elapsed();
    report.ran = ran;
    report.activity = meter.activity;
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(format!("Vanillite at tick {ran}: {e}")),
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            return Err(format!("Vanillite panicked at tick {ran}: {msg}"));
        }
    }
    let Some(actual) = diverged else {
        report.verdict = if stopped {
            Verdict::Stopped(ran)
        } else {
            Verdict::Same
        };
        return Ok(());
    };

    // Vanilla again, exactly up to the tick, for the full frame.
    let tick = ran;
    let mut expected = None;
    let mut vnames = Vec::new();
    let mut unreproducible = None;
    vanilla(
        java,
        file,
        Some(tick),
        &dir.join("vanilla-rerun.log"),
        &mut |trace, frame| {
            let t = frame.tick;
            if unreproducible.is_none()
                && sums.get(t as usize) != Some(&trace.checksum(&frame, &mut vnames))
            {
                unreproducible = Some(t);
            }
            if t == tick {
                expected = Some(one_frame(trace, frame));
            }
            Ok(true)
        },
    )?;
    let expected = expected.ok_or("the Vanilla re-run ended before the tick")?;
    let known = scenario.diverges.as_ref().is_some_and(|d| d.tick == tick);
    report.verdict = Verdict::Diverges { tick, known };
    let _ = writeln!(
        text,
        "First divergence at tick {tick} (game day {}, tick {} of the day).",
        tick / DAY + 1,
        tick % DAY
    );
    if let Some(d) = &scenario.diverges {
        let _ = writeln!(
            text,
            "\nKnown divergence at tick {} ({}, owner {}){}.",
            d.tick,
            d.cause,
            d.owner,
            if known { "" } else { ": this is another tick" }
        );
    }
    if let Some(t) = unreproducible {
        let _ = writeln!(
            text,
            "\nWARNING: Vanilla's re-run differs from its stored run at tick {t}: Vanilla's \
             run is not reproducible, the difference below may not be Vanillite's."
        );
    }
    let diff = expected.diff(&actual, DIFF_LIMIT);
    if diff.is_empty() {
        let _ = writeln!(
            text,
            "\nVanillite's frame equals the one of Vanilla's re-run at this tick."
        );
    } else {
        let _ = writeln!(text, "\n```");
        for line in diff {
            let _ = writeln!(text, "{line}");
        }
        let _ = writeln!(text, "```");
    }
    for (side, trace) in [("vanilla", &expected), ("vanillite", &actual)] {
        let path = dir.join(format!("{side}-{tick}.json"));
        std::fs::write(&path, trace.to_json()).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let _ = writeln!(
        text,
        "\nBoth full frames: `{}/{{vanilla,vanillite}}-{tick}.json`.",
        scenario.name
    );
    Ok(())
}

/// The trace header of `trace` with `frame` as its only frame.
fn one_frame(trace: &Trace, frame: Frame) -> Trace {
    let mut t = trace.clone();
    t.frames = vec![frame];
    t
}

/// The stored checksums of a scenario, if they are complete.
fn read_sums(path: &Path, ticks: u32) -> Result<Option<Vec<u64>>, String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if bytes.len() != (ticks as usize + 1) * 8 {
        return Ok(None);
    }
    Ok(Some(
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect(),
    ))
}

/// Records Vanilla's checksums and snapshots of a scenario; `None` if the deadline ended it.
fn record_golden(
    scenario: &Scenario,
    file: &Path,
    java: &Java,
    dir: &Path,
    sums_path: &Path,
    snapshots: &Path,
    deadline: Option<Instant>,
) -> Result<Option<Vec<u64>>, String> {
    std::fs::create_dir_all(snapshots).map_err(|e| format!("{}: {e}", snapshots.display()))?;
    // A recorder the system kills (out of memory while other runs take the host's memory)
    // records once more before the scenario counts as failed.
    match record_sums(scenario, file, java, dir, snapshots, deadline) {
        Err(e) if e.contains("SIGKILL") => {
            eprintln!(
                "{}: the Vanilla recorder was killed, recording again",
                scenario.name
            );
            record_sums(scenario, file, java, dir, snapshots, deadline)
        }
        r => r,
    }
    .and_then(|sums| {
        let Some(sums) = sums else {
            return Ok(None);
        };
        let bytes: Vec<u8> = sums.iter().flat_map(|s| s.to_le_bytes()).collect();
        let tmp = sums_path.with_extension("sums.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, sums_path).map_err(|e| format!("{}: {e}", sums_path.display()))?;
        Ok(Some(sums))
    })
}

/// One recording of [`record_golden`]: the checksums, `None` if the deadline ended it.
fn record_sums(
    scenario: &Scenario,
    file: &Path,
    java: &Java,
    dir: &Path,
    snapshots: &Path,
    deadline: Option<Instant>,
) -> Result<Option<Vec<u64>>, String> {
    let mut sums = Vec::with_capacity(scenario.ticks as usize + 1);
    let mut names = Vec::new();
    let mut stopped = false;
    vanilla(
        java,
        file,
        None,
        &dir.join("vanilla.log"),
        &mut |trace, frame| {
            if frame.tick as usize != sums.len() {
                return Err(format!("frame {} after {} frames", frame.tick, sums.len()));
            }
            sums.push(trace.checksum(&frame, &mut names));
            if frame.tick % SNAPSHOT == 0 {
                let path = snapshots.join(format!("{}.json", frame.tick));
                std::fs::write(&path, one_frame(trace, frame).to_json())
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
            if sums.len() % 1000 == 0 && deadline.is_some_and(|d| Instant::now() >= d) {
                stopped = true;
                return Ok(false);
            }
            Ok(true)
        },
    )?;
    if stopped {
        return Ok(None);
    }
    if sums.len() != scenario.ticks as usize + 1 {
        return Err(format!(
            "Vanilla recorded {} frames, expected {}",
            sums.len(),
            scenario.ticks + 1
        ));
    }
    Ok(Some(sums))
}

/// Runs the recorder on a scenario file (up to `ticks`, default all) and hands every frame
/// to `sink`; `false` from the sink ends the recording. The recorder's log goes to `log`.
fn vanilla(
    java: &Java,
    file: &Path,
    ticks: Option<u32>,
    log: &Path,
    sink: &mut dyn FnMut(&Trace, Frame) -> Result<bool, String>,
) -> Result<(), String> {
    let log_file = std::fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
    let file = std::path::absolute(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut cmd = Command::new(&java.java);
    cmd.current_dir(&java.work_dir)
        .arg(format!("-javaagent:{}", java.agent.display()))
        // A bounded heap: without it the JVM grows towards a quarter of the host's memory, and
        // several recorders side by side (`parity worlds --jobs`) get OOM-killed.
        .arg("-Xmx4G")
        .arg("-cp")
        .arg(&java.classpath)
        .arg("dev.vanillite.extractor.golden.ScenarioRecorder")
        .arg("--stream")
        .arg(&file);
    if let Some(t) = ticks {
        cmd.arg(t.to_string());
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log_file)
        .spawn()
        .map_err(|e| format!("{}: {e}", java.java.display()))?;
    let stdout = child.stdout.take().ok_or("no recorder output")?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, stdout);
    let result: Result<bool, String> = (|| {
        let mut line = String::new();
        let mut trace: Option<Trace> = None;
        let mut last: std::sync::Arc<Vec<u32>> = std::sync::Arc::default();
        loop {
            line.clear();
            if reader
                .read_line(&mut line)
                .map_err(|e| format!("recorder: {e}"))?
                == 0
            {
                return Ok(true);
            }
            let l = line.trim_end();
            let Some(t) = trace.as_mut() else {
                if l.starts_with("{\"format\"") {
                    trace = Some(Trace::stream_header(l)?);
                }
                continue;
            };
            let frame = t
                .stream_frame(l, &last)
                .map_err(|e| format!("recorder frame: {e}"))?;
            last.clone_from(&frame.blocks);
            if !sink(t, frame)? {
                return Ok(false);
            }
        }
    })();
    let complete = matches!(result, Ok(true));
    if !complete {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| format!("recorder: {e}"))?;
    result?;
    if complete && !status.success() {
        return Err(format!(
            "the Vanilla recorder failed ({status}); see {}",
            log.display()
        ));
    }
    Ok(())
}
