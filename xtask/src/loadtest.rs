//! `cargo xtask loadtest`: scaling benchmark with `tools/loadbot`.
//!
//! The server under test runs alone on the benchmark server vanillite-bench (#186;
//! `vanillite@30901`, or the Paper reference `paper-loadtest` on 30900), only during the run.
//! The bots run there too, on the CPUs the server's hardware profile leaves free
//! (`vanillite-bench bots`), so the network between here and there plays no part. Per step of
//! the player ramp the run records server MSPT, RSS, CPU and network throughput (from
//! `vanillite-loadtest-monitor` on the test server) and the bots' join latency, round trip and
//! traffic, and how fast chunks reach the bots and the disk is read. Every step can move the
//! bots differently (random walk or straight flight, see `tools/loadbot`). Output: one JSON file per server, a Markdown table and an SVG chart. Results are
//! kept on the test server; the regression gate compares with the median of the last 5 runs
//! on the same hardware and limits.
//!
//! Every server under test runs under a fixed hardware profile (`vanillite-bench limit`: pinned
//! CPUs, CPU quota and memory limit), and every result records the test server's hardware
//! (`hardware`) and the host load during the run (`load`), so numbers stay comparable when the
//! machine changes and runs on an overloaded host are marked unreliable.

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Result;

/// SSH host of the benchmark server; override with `VANILLITE_BENCH_HOST`.
const BENCH_HOST: &str = "vanillite-bench";
const RESULTS: &str = "/opt/vanillite/bench/results";
/// Where `vanillite@30901` saves its players (`--fresh-players`).
const PLAYERS: &str = "/opt/vanillite/30901/world/players";
/// Where `vanillite@30901` loads plugins from.
const PLUGINS: &str = "/opt/vanillite/30901/plugins";
const DEFAULT_STEPS: &[u64] = &[50, 100, 250, 500, 1000, 2000, 5000];
/// The gate fails when a step is this much worse than the median of the history.
const GATE_TOLERANCE: f64 = 1.10;
const GATE_HISTORY: usize = 5;
/// The hardware profile of every benchmark instance unless a scenario asks for another one:
/// 4 pinned CPUs with a quota of 400 % and 8 GiB memory, no swap (`vanillite-bench limit`).
pub(crate) const LIMITS: &str = "4c8g";
/// A run is unreliable when the host's load per CPU was above this during it.
const OVERLOAD_PER_CPU: f64 = 1.0;
/// A run is unreliable when the hypervisor gave more than this share of the CPU time away.
const MAX_STEAL_PERCENT: f64 = 5.0;

/// A server (or proxy) under test on the test server; the references take turns on 30900.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Vanillite,
    Paper,
    Vanilla,
    Pumpkin,
    /// Proxies in front of `vanillite@30901`.
    Velocity,
    VanilliteProxy,
}

impl Target {
    /// The name in results and for `vanillite-bench` on the test server.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Vanillite => "vanillite",
            Self::Paper => "paper",
            Self::Vanilla => "vanilla",
            Self::Pumpkin => "pumpkin",
            Self::Velocity => "velocity",
            Self::VanilliteProxy => "vanillite-proxy",
        }
    }

    pub(crate) fn port(self) -> u16 {
        match self {
            Self::Vanillite => 30901,
            _ => 30900,
        }
    }

    pub(crate) fn unit(self) -> String {
        match self {
            Self::Vanillite => format!("vanillite@{}.service", self.port()),
            t => format!("{}-loadtest.service", t.name()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Options {
    pub(crate) targets: Vec<Target>,
    pub(crate) steps: Vec<u64>,
    /// Bot movement per step (`tools/loadbot` movement spec).
    pub(crate) movements: Vec<String>,
    /// Join rate and concurrent logins per step (`None`: the run's join rate).
    pub(crate) rates: Vec<Option<(f64, usize)>>,
    pub(crate) hold: Duration,
    /// Wait between reaching a step and its measurement window.
    pub(crate) settle: Duration,
    pub(crate) join_rate: f64,
    pub(crate) chat_interval: f64,
    pub(crate) view_distance: u8,
    pub(crate) profile: &'static str,
    /// Hardware profile of the server under test (`vanillite-bench limit`).
    pub(crate) limits: String,
    pub(crate) deploy: bool,
    pub(crate) gate: bool,
    /// Rules of `vanillite.toml` set for this run (`key=value`), as environment variables of
    /// the Vanillite instance.
    pub(crate) rules: Vec<(String, String)>,
    /// Deploy this server binary instead of building `vanillite`.
    pub(crate) binary: Option<PathBuf>,
    /// Plugins installed on `vanillite@30901` for the run.
    pub(crate) plugins: Vec<PluginSpec>,
    /// Delete the saved players of `vanillite@30901` first, so the bots start at the spawn.
    pub(crate) fresh_players: bool,
}

/// A plugin folder for the run: `plugin.toml`, the library it names (from the folder, or built
/// from the folder's crate) and `config.toml` with the `--plugin-set` lines appended.
#[derive(Debug)]
pub(crate) struct PluginSpec {
    dir: PathBuf,
    set: Vec<String>,
}

/// A plugin folder ready to copy: its name on the server and its files.
struct StagedPlugin {
    name: String,
    /// The folder with exactly the files the server gets.
    dir: PathBuf,
    /// What the run's results say about it.
    info: Value,
}

fn parse(args: &[&str]) -> Result<Options> {
    let mut o = Options {
        targets: vec![Target::Vanillite, Target::Paper],
        steps: DEFAULT_STEPS.to_vec(),
        movements: Vec::new(),
        rates: Vec::new(),
        hold: Duration::from_secs(60),
        settle: Duration::ZERO,
        join_rate: 20.0,
        chat_interval: 0.0,
        view_distance: 10,
        profile: "release-fast",
        limits: LIMITS.to_owned(),
        deploy: true,
        gate: true,
        binary: None,
        plugins: Vec::new(),
        fresh_players: false,
        rules: Vec::new(),
    };
    let mut movement = "random";
    let mut step_movements = vec![None; o.steps.len()];
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().copied().ok_or(format!("{arg} needs a value"));
        let num = |v: &str| v.parse::<f64>().map_err(|e| format!("{arg}: {e}"));
        match *arg {
            "--server" => {
                o.targets = match value()? {
                    "vanillite" => vec![Target::Vanillite],
                    "paper" => vec![Target::Paper],
                    "both" => vec![Target::Vanillite, Target::Paper],
                    v => return Err(format!("--server: unknown server {v:?}")),
                }
            }
            "--steps" => {
                (o.steps, step_movements) = value()?
                    .split(',')
                    .map(|s| {
                        let (n, m) = s
                            .trim()
                            .split_once('@')
                            .map_or((s.trim(), None), |(n, m)| (n, Some(m.to_owned())));
                        let n: u64 = n.parse().map_err(|e| format!("--steps: {e}"))?;
                        Ok((n, m))
                    })
                    .collect::<Result<Vec<(u64, Option<String>)>>>()?
                    .into_iter()
                    .unzip();
            }
            "--movement" => movement = value()?,
            "--hold" => o.hold = Duration::from_secs_f64(num(value()?)?),
            "--settle" => o.settle = Duration::from_secs_f64(num(value()?)?),
            "--join-rate" => o.join_rate = num(value()?)?,
            "--chat" => o.chat_interval = num(value()?)?,
            "--view-distance" => {
                o.view_distance = value()?.parse().map_err(|e| format!("{arg}: {e}"))?;
            }
            "--release" => o.profile = "release",
            "--limits" => o.limits = value()?.to_owned(),
            "--fresh-players" => o.fresh_players = true,
            "--binary" => o.binary = Some(PathBuf::from(value()?)),
            "--plugin" => o.plugins.push(PluginSpec {
                dir: PathBuf::from(value()?),
                set: Vec::new(),
            }),
            "--plugin-set" => {
                let line = value()?.to_owned();
                o.plugins
                    .last_mut()
                    .ok_or("--plugin-set belongs to a --plugin before it")?
                    .set
                    .push(line);
            }
            "--no-deploy" => o.deploy = false,
            "--no-gate" => o.gate = false,
            "--rule" => {
                let (key, rule) = value()?.split_once('=').ok_or(
                    "--rule needs <key>=<value>, e.g. performance.tick.parallel-dimensions=true",
                )?;
                let plain = |s: &str| {
                    !s.is_empty()
                        && s.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                };
                if !plain(key) || !plain(rule) {
                    return Err(format!("--rule: unsupported characters in {key}={rule}"));
                }
                o.rules.push((key.to_owned(), rule.to_owned()));
            }
            _ => return Err(crate::USAGE.to_owned()),
        }
    }
    if o.steps.is_empty() || o.steps.windows(2).any(|w| w[0] > w[1]) {
        return Err("--steps must not decrease".into());
    }
    o.movements = step_movements
        .into_iter()
        .map(|m| m.unwrap_or_else(|| movement.to_owned()))
        .collect();
    o.rates = vec![None; o.steps.len()];
    Ok(o)
}

/// `cargo xtask loadtest [options]`.
pub fn run(root: &Path, args: &[&str]) -> Result<()> {
    if let ["report", files @ ..] = args {
        return report(files);
    }
    let o = parse(args)?;
    let host = bench_host();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut build = vec!["build", "--locked", "--profile", o.profile, "-p", "loadbot"];
    if o.deploy && o.binary.is_none() && o.targets.contains(&Target::Vanillite) {
        build.extend(["-p", "vanillite"]);
    }
    let packages = o
        .plugins
        .iter()
        .map(|p| package_name(&p.dir))
        .collect::<Result<Vec<_>>>()?;
    build.extend(packages.iter().flatten().flat_map(|p| ["-p", p.as_str()]));
    crate::devenv::run(Command::new(&cargo).current_dir(root).args(&build))?;
    let bin_dir = crate::target_dir(root).join(o.profile);
    let mut specs = o.movements.clone();
    specs.dedup();
    for spec in &specs {
        crate::devenv::run(Command::new(bin_dir.join("loadbot")).args([
            "--movement",
            spec,
            "--check",
        ]))?;
    }
    let commit = git(root, &["rev-parse", "--short=12", "HEAD"])?;
    let dirty = !git(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    let stamp = command_output(Command::new("date").args(["-u", "+%Y%m%dT%H%M%SZ"]))?;
    let out_dir = root.join("target/loadtest").join(&stamp);
    let plugins = o
        .plugins
        .iter()
        .map(|p| stage_plugin(p, &bin_dir, &out_dir.join("plugins")))
        .collect::<Result<Vec<_>>>()?;

    let _lock = Lock::take(&host, &format!("xtask loadtest from {}", root.display()))?;
    install(&host, "loadbot", &bin_dir.join("loadbot"))?;
    let hw = hardware(&host)?;
    println!(
        "   hardware: {}; limits {}",
        hw["baseline"].as_str().unwrap_or("?"),
        o.limits
    );
    let mut runs = Vec::new();
    let mut failures = Vec::new();
    for &target in &o.targets {
        println!(
            "== load test: {} on {host}:{}",
            target.name(),
            target.port()
        );
        let with_plugins = target == Target::Vanillite && !plugins.is_empty();
        let before = load_sample(&host);
        let limits = bench_json(&host, &["limit", target.name(), &o.limits]);
        let result = limits
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|_| {
                if target == Target::Vanillite && o.fresh_players {
                    ssh(
                        &host,
                        &format!("systemctl stop {} && rm -rf {PLAYERS}", target.unit()),
                    )
                } else {
                    Ok(())
                }
            })
            .and_then(|()| {
                if with_plugins {
                    install_plugins(&host, &plugins)
                } else {
                    Ok(())
                }
            })
            .and_then(|()| deploy_or_restart(target, &o, &host, &bin_dir))
            .and_then(|()| Session::open(&host, target, &o, &[]))
            .and_then(|mut session| {
                let steps = ramp(&o, &mut session);
                session.close();
                steps
            });
        // The instance is offline mode: never leave it running.
        let stop = match target {
            Target::Vanillite => format!("systemctl disable --now {}", target.unit()),
            _ => format!("systemctl stop {}", target.unit()),
        };
        if let Err(e) = ssh(&host, &stop) {
            eprintln!("warning: cannot stop {}: {e}", target.unit());
        }
        if with_plugins {
            let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
            if let Err(e) = ssh(
                &host,
                &format!("cd {PLUGINS} && rm -rf -- {}", names.join(" ")),
            ) {
                eprintln!("warning: cannot remove the plugins from {PLUGINS}: {e}");
            }
        }
        if target == Target::Vanillite
            && !o.rules.is_empty()
            && let Err(e) = set_rules(&host, &[])
        {
            eprintln!("warning: cannot remove {RULES_DROPIN}: {e}");
        }
        let steps = match result {
            Ok(steps) => steps,
            Err(e) => {
                failures.push(format!("{}: {e}", target.name()));
                continue;
            }
        };
        let load = load_window(&before, &load_sample(&host), &hw);
        let mut run = json!({
            "server": target.name(),
            "commit": commit,
            "dirty": dirty,
            "date": stamp,
            "profile": if target == Target::Vanillite { o.profile } else { "paper" },
            "settings": {
                "hold_s": o.hold.as_secs_f64(),
                "join_rate": o.join_rate,
                "chat_interval_s": o.chat_interval,
                "view_distance": o.view_distance,
            },
            "hardware": hw,
            "limits": limits.unwrap_or_default(),
            "load": load,
            "max_players_under_50ms": max_players_under(&steps, 50.0),
            "steps": steps,
        });
        if load["unreliable"] == true {
            println!(
                "   host overloaded during the run (load {} on {} CPUs): unreliable",
                load["host_load"], hw["host_cpus"]
            );
        }
        // Runs that only walk randomly keep the settings of the runs before movement modes.
        if o.movements.iter().any(|m| m != "random") {
            run["settings"]["movement"] = json!(o.movements);
        }
        if with_plugins {
            run["settings"]["plugins"] = plugins.iter().map(|p| p.info.clone()).collect();
        }
        if let (Target::Vanillite, Some(binary)) = (target, &o.binary) {
            run["settings"]["binary"] = json!(binary.display().to_string());
        }
        // Runs with rules set are experiments: they neither gate nor become a baseline.
        let with_rules = target == Target::Vanillite && !o.rules.is_empty();
        if with_rules {
            run["settings"]["rules"] = json!(
                o.rules
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
            );
        }
        let file = out_dir.join(format!("{}.json", target.name()));
        let text = serde_json::to_string_pretty(&run).map_err(|e| e.to_string())?;
        crate::write(&file, &text)?;
        println!("wrote {}", file.display());
        if o.gate && !with_rules {
            match gate(&host, &run) {
                Ok(problems) => failures.extend(problems),
                Err(e) => eprintln!("warning: regression gate skipped: {e}"),
            }
        }
        // Runs with rules, plugins or another binary stay out of the history the gate
        // compares with.
        if !dirty && !with_rules && !with_plugins && o.binary.is_none() {
            store(
                &host,
                &run,
                &format!("{}/{}/{stamp}-{commit}.json", RESULTS, target.name()),
            )?;
        }
        runs.push(run);
    }
    if !runs.is_empty() {
        let md = out_dir.join("report.md");
        crate::write(&md, &markdown(&runs))?;
        crate::write(&out_dir.join("chart.svg"), &svg(&runs))?;
        println!("wrote {} and chart.svg", md.display());
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

/// `cargo xtask loadtest report <run.json>...`: the Markdown table and chart of earlier runs,
/// written next to the first file.
fn report(files: &[&str]) -> Result<()> {
    let first = files.first().ok_or(crate::USAGE)?;
    let runs = files
        .iter()
        .map(|f| crate::read_json(Path::new(f)))
        .collect::<Result<Vec<_>>>()?;
    let dir = Path::new(first).parent().unwrap_or(Path::new("."));
    crate::write(&dir.join("report.md"), &markdown(&runs))?;
    crate::write(&dir.join("chart.svg"), &svg(&runs))?;
    println!("wrote {0}/report.md and {0}/chart.svg", dir.display());
    Ok(())
}

/// One line of the server monitor, with its arrival time in seconds since the run start.
#[derive(Debug, Clone)]
pub(crate) enum Sample {
    /// RSS KiB, CPU ticks, net rx bytes, net tx bytes, then from `/proc/<pid>/io` read
    /// syscalls, bytes read from storage and bytes written to storage (0 from older monitors).
    Stats(f64, [u64; 7]),
    /// A tick in nanoseconds (Vanillite).
    Tick(f64, u64),
    /// `/tick query` P50, P95, P99 and mean in milliseconds (Paper, Vanilla and Pumpkin,
    /// 100-tick windows).
    Query(f64, [f64; 4]),
}

#[derive(Debug, Default)]
pub(crate) struct Collected {
    pub(crate) clock_ticks: u64,
    pub(crate) server: Vec<Sample>,
    /// Bot reports with their arrival time.
    pub(crate) bots: Vec<(f64, Value)>,
}

impl Collected {
    /// The newest RSS sample in MiB.
    pub(crate) fn rss_mib(&self) -> Option<f64> {
        self.server.iter().rev().find_map(|s| match s {
            Sample::Stats(_, v) if v[0] > 0 => Some(v[0] as f64 / 1024.0),
            _ => None,
        })
    }

    /// Bots online in the newest report and bots that joined or failed so far.
    pub(crate) fn bots_online(&self) -> (u64, u64) {
        let last = self.bots.last().map(|(_, v)| v.clone()).unwrap_or_default();
        let field = |k: &str| last.get(k).and_then(Value::as_u64).unwrap_or(0);
        (field("online"), field("joined") + field("failed"))
    }
}

/// The package of a plugin folder that is a crate (`Cargo.toml` in it), to build its library.
fn package_name(dir: &Path) -> Result<Option<String>> {
    let manifest = dir.join("Cargo.toml");
    if !manifest.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let value: toml::Table = text
        .parse()
        .map_err(|e| format!("{}: {e}", manifest.display()))?;
    value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str)
        .map(|name| Some(name.to_owned()))
        .ok_or_else(|| format!("{}: no package name", manifest.display()))
}

/// Copies a plugin's files into `<out>/<folder name>`: `plugin.toml`, the library it names
/// (from the folder, else from `bin_dir` where cargo built it) and `config.toml` (if any)
/// with the `--plugin-set` lines set.
fn stage_plugin(spec: &PluginSpec, bin_dir: &Path, out: &Path) -> Result<StagedPlugin> {
    let dir = &spec.dir;
    let name = dir
        .canonicalize()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !n.is_empty() && !n.starts_with('.'))
        .ok_or_else(|| format!("--plugin {}: not a plugin folder", dir.display()))?;
    let manifest_path = dir.join("plugin.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let library = manifest
        .parse::<toml::Table>()
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?
        .get("library")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("{}: no library", manifest_path.display()))?
        .to_owned();
    let file = format!("lib{library}.so");
    let source = [dir.join(&file), bin_dir.join(&file)]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| format!("--plugin {}: no {file} (build the plugin)", dir.display()))?;
    let staged = out.join(&name);
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged).map_err(|e| format!("{}: {e}", staged.display()))?;
    let copy = |from: &Path, to: &str| {
        std::fs::copy(from, staged.join(to))
            .map(|_| ())
            .map_err(|e| format!("{}: {e}", from.display()))
    };
    copy(&manifest_path, "plugin.toml")?;
    copy(&source, &file)?;
    let config = std::fs::read_to_string(dir.join("config.toml")).unwrap_or_default();
    let config = spec
        .set
        .iter()
        .fold(config, |config, line| set_key(&config, line));
    if !config.is_empty() {
        crate::write(&staged.join("config.toml"), &config)?;
    }
    println!("plugin {name}: {}", source.display());
    Ok(StagedPlugin {
        info: json!({ "name": name, "set": spec.set }),
        name,
        dir: staged,
    })
}

/// `config` with its first uncommented `key = ...` line replaced by `line` (`key = value`), or
/// with `line` put first (before any table) when the key is not there.
fn set_key(config: &str, line: &str) -> String {
    let key = |l: &str| l.split_once('=').map(|(k, _)| k.trim().to_owned());
    let wanted = key(line);
    let mut replaced = false;
    let mut out: Vec<&str> = Vec::new();
    for l in config.lines() {
        if !replaced && wanted.is_some() && key(l) == wanted && !l.trim_start().starts_with('#') {
            out.push(line);
            replaced = true;
        } else {
            out.push(l);
        }
    }
    if !replaced {
        out.insert(0, line);
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Puts the plugin folders into the plugins directory of `vanillite@30901` (replacing folders
/// of the same name); the run removes them again when it stops the instance.
fn install_plugins(host: &str, plugins: &[StagedPlugin]) -> Result<()> {
    for plugin in plugins {
        let parent = plugin.dir.parent().ok_or("plugin folder without parent")?;
        let tar = Command::new("tar")
            .arg("-C")
            .arg(parent)
            .args(["-cf", "-", &plugin.name])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("tar: {e}"))?;
        let stdout = tar.stdout.ok_or("no tar output")?;
        crate::devenv::run(
            Command::new("ssh")
                .args([
                    host,
                    &format!(
                        "install -d -o vanillite -g vanillite {PLUGINS} && cd {PLUGINS} \
                         && rm -rf -- '{0}' && tar -xf - --no-same-owner \
                         && chown -R vanillite:vanillite '{0}'",
                        plugin.name
                    ),
                ])
                .stdin(Stdio::from(stdout)),
        )?;
    }
    Ok(())
}

/// The systemd drop-in with the rules of this run (`--rule`), gone again after the run.
const RULES_DROPIN: &str = "/run/systemd/system/vanillite@30901.service.d/loadtest-rules.conf";

/// Writes the run's rules as `VANILLITE_<KEY>` variables of the Vanillite instance (dots
/// become `__`, dashes `_`), or removes the drop-in of an earlier run.
fn set_rules(host: &str, rules: &[(String, String)]) -> Result<()> {
    let mut script = format!("rm -f {RULES_DROPIN}");
    if !rules.is_empty() {
        let dir = RULES_DROPIN.rsplit_once('/').map_or("", |d| d.0);
        script = format!("install -d {dir} && printf '[Service]\\n");
        for (key, value) in rules {
            let name = key.to_uppercase().replace('.', "__").replace('-', "_");
            script.push_str(&format!("Environment=VANILLITE_{name}={value}\\n"));
        }
        script.push_str(&format!("' > {RULES_DROPIN}"));
    }
    ssh(host, &format!("{script} && systemctl daemon-reload"))
}

fn deploy_or_restart(target: Target, o: &Options, host: &str, bin_dir: &Path) -> Result<()> {
    if target == Target::Vanillite {
        set_rules(host, &o.rules)?;
    }
    match target {
        Target::Vanillite if o.deploy => {
            let binary = o
                .binary
                .clone()
                .unwrap_or_else(|| bin_dir.join("vanillite"));
            let file =
                std::fs::File::open(&binary).map_err(|e| format!("{}: {e}", binary.display()))?;
            crate::devenv::run(
                Command::new("ssh")
                    .args([host, "vanillite-deploy", &target.port().to_string()])
                    .stdin(Stdio::from(file)),
            )?;
        }
        _ => ssh(host, &format!("systemctl restart {}", target.unit()))?,
    }
    wait_for_port(host, target.port(), Duration::from_secs(180))
}

/// The test server's benchmark lock: `xtask loadtest` and `xtask bench` restart the benchmark
/// instances (30900, 30901), so only one run may use them at a time. Held while this lives.
pub(crate) struct Lock(Child);

impl Lock {
    /// Waits for the lock; `owner` is shown to runs that wait for it.
    pub(crate) fn take(host: &str, owner: &str) -> Result<Self> {
        Self::take_with(host, "vanillite-bench lock", owner)
    }

    /// Waits for a lock of the same protocol taken by the remote command `lock` (for example
    /// `vanillite-lock` on vanillite-test2).
    pub(crate) fn take_with(host: &str, lock: &str, owner: &str) -> Result<Self> {
        let mut child = Command::new("ssh")
            .args([host, &format!("{lock} '{}'", owner.replace('\'', ""))])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ssh: {e}"))?;
        let out = child.stdout.take().ok_or("no ssh output")?;
        for line in BufReader::new(out).lines().map_while(std::io::Result::ok) {
            if let Some(holder) = line.strip_prefix("busy") {
                println!("waiting for the lock on {host} (held by{holder})");
            } else if line == "locked" {
                return Ok(Self(child));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        Err(format!("cannot take the lock on {host}"))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The server monitor and the load bots of one run against a started server.
pub(crate) struct Session {
    pub(crate) target: Target,
    start: Instant,
    pub(crate) data: Arc<Mutex<Collected>>,
    monitor: Child,
    bot: Child,
    bot_in: ChildStdin,
}

impl Session {
    /// Starts the monitor of `target` and the load bot on the benchmark server (no bots
    /// online yet); `extra` are more loadbot arguments.
    pub(crate) fn open(host: &str, target: Target, o: &Options, extra: &[&str]) -> Result<Self> {
        let start = Instant::now();
        let data = Arc::new(Mutex::new(Collected {
            clock_ticks: 100,
            ..Collected::default()
        }));
        let mut monitor = Command::new("ssh")
            .args([host, "vanillite-loadtest-monitor", &target.unit()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ssh: {e}"))?;
        let monitor_out = monitor.stdout.take().ok_or("no monitor output")?;
        let d = Arc::clone(&data);
        std::thread::spawn(move || read_monitor(monitor_out, start, &d));

        // `vanillite-bench bots` runs the installed loadbot against 127.0.0.1 on the CPUs the
        // server does not use; stdin and stdout go through SSH.
        let mut bot = Command::new("ssh")
            .args([host, "vanillite-bench", "bots"])
            .args([
                "--port",
                &target.port().to_string(),
                "--join-rate",
                &o.join_rate.to_string(),
                "--chat-interval",
                &o.chat_interval.to_string(),
                "--view-distance",
                &o.view_distance.to_string(),
            ])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("loadbot: {e}"))?;
        let bot_out = bot.stdout.take().ok_or("no loadbot output")?;
        let d = Arc::clone(&data);
        std::thread::spawn(move || {
            for line in BufReader::new(bot_out)
                .lines()
                .map_while(std::io::Result::ok)
            {
                if let (Ok(v), Ok(mut d)) = (serde_json::from_str::<Value>(&line), d.lock()) {
                    d.bots.push((start.elapsed().as_secs_f64(), v));
                }
            }
        });
        let bot_in = bot.stdin.take().ok_or("no loadbot input")?;
        Ok(Self {
            target,
            start,
            data,
            monitor,
            bot,
            bot_in,
        })
    }

    /// Seconds since the session started (the time base of all samples).
    pub(crate) fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Sends a control command to the load bot (`target <n>`, `movement <spec>`, ...).
    pub(crate) fn bot(&mut self, command: &str) -> Result<()> {
        writeln!(self.bot_in, "{command}").map_err(|e| format!("loadbot: {e}"))
    }

    /// Waits until `n` bots are online; gives up after 30 s without join progress. Returns
    /// whether all are online.
    pub(crate) fn wait_online(&self, n: u64) -> Result<bool> {
        let mut last_progress = (0u64, Instant::now());
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let (online, settled) = self.data.lock().map_err(|_| "poisoned")?.bots_online();
            if online >= n {
                return Ok(true);
            }
            if settled != last_progress.0 {
                last_progress = (settled, Instant::now());
            } else if last_progress.1.elapsed() > Duration::from_secs(30) {
                println!("   no join progress for 30 s at {online}/{n} players");
                return Ok(false);
            }
        }
    }

    /// The metrics of a window (see [`aggregate`]).
    pub(crate) fn window(&self, wanted: u64, ramp_start: f64, from: f64, to: f64) -> Result<Value> {
        let d = self.data.lock().map_err(|_| "poisoned")?;
        Ok(aggregate(&d, self.target, wanted, ramp_start, from, to))
    }

    /// Ends the bots and the monitor.
    pub(crate) fn close(mut self) {
        let _ = writeln!(self.bot_in, "quit");
        drop(self.bot_in);
        finish(&mut self.bot, Duration::from_secs(5));
        let _ = self.monitor.kill();
        let _ = self.monitor.wait();
    }
}

/// Runs the steps; stops early when a step is not reached or the server is gone.
pub(crate) fn ramp(o: &Options, session: &mut Session) -> Result<Vec<Value>> {
    // Let the monitor and the server settle.
    std::thread::sleep(Duration::from_secs(5));
    let idle_rss = session.data.lock().map_err(|_| "poisoned")?.rss_mib();
    let mut steps = Vec::new();
    let mut movement = "";
    for ((&n, spec), rate) in o.steps.iter().zip(&o.movements).zip(&o.rates) {
        if spec != movement {
            movement = spec;
            session.bot(&format!("movement {spec}"))?;
        }
        if let Some((per_s, logins)) = rate {
            session.bot(&format!("rate {per_s} {logins}"))?;
        }
        session.bot(&format!("target {n}"))?;
        let ramp_start = session.now();
        session.wait_online(n)?;
        let ramp_end = session.now();
        std::thread::sleep(o.settle);
        let hold_start = session.now();
        std::thread::sleep(o.hold);
        let hold_end = session.now();
        let mut step = session.window(n, ramp_start, hold_start, hold_end)?;
        step["movement"] = json!(spec);
        step["ramp_s"] = round(Some(ramp_end - ramp_start));
        let during_ramp = session.window(n, ramp_start, ramp_start, ramp_end)?;
        step["ramp_mspt_max"] = during_ramp["mspt"]["max"].clone();
        let players = step["players"].as_f64().unwrap_or(0.0);
        if let (Some(idle), Some(rss)) = (idle_rss, step["server"]["rss_mib"].as_f64())
            && players > 0.0
        {
            step["server"]["idle_rss_mib"] = round(Some(idle));
            step["server"]["rss_net_per_player_kib"] =
                round(Some((rss - idle).max(0.0) * 1024.0 / players));
        }
        let players = step["players"].as_u64().unwrap_or(0);
        println!(
            "   {players:>5}/{n:<5} players: MSPT p50 {} p95 {} p99 {} ms, RSS {} MiB, CPU {} %, \
             out {} MB/s, join p50 {} ms, {}: {} chunks/s, disk read {} MB/s, {} teleports",
            show(&step["mspt"]["p50"]),
            show(&step["mspt"]["p95"]),
            show(&step["mspt"]["p99"]),
            show(&step["server"]["rss_mib"]),
            show(&step["server"]["cpu_percent"]),
            show(&step["server"]["net_tx_mbps"]),
            show(&step["bots"]["join_ms_p50"]),
            spec,
            show(&step["bots"]["chunks_per_s"]),
            show(&step["server"]["disk_read_mbps"]),
            step["bots"]["teleports"],
        );
        let alive = step["server"]["rss_mib"].as_f64().unwrap_or(0.0) > 0.0;
        let reached = players * 100 >= n * 95;
        steps.push(step);
        if !alive {
            println!("   server is not running any more; ramp stopped");
            break;
        }
        if !reached {
            println!("   step not reached; ramp stopped");
            break;
        }
    }
    Ok(steps)
}

fn read_monitor(out: impl std::io::Read, start: Instant, data: &Mutex<Collected>) {
    for line in BufReader::new(out).lines().map_while(std::io::Result::ok) {
        let t = start.elapsed().as_secs_f64();
        let mut words = line.split_whitespace();
        let kind = words.next();
        let Ok(mut d) = data.lock() else { return };
        match kind {
            Some("H") => d.clock_ticks = words.next().and_then(|w| w.parse().ok()).unwrap_or(100),
            Some("S") => {
                let v: Vec<u64> = words.filter_map(|w| w.parse().ok()).collect();
                match v[..] {
                    [rss, cpu, rx, tx] => {
                        d.server.push(Sample::Stats(t, [rss, cpu, rx, tx, 0, 0, 0]));
                    }
                    [rss, cpu, rx, tx, syscr, read, write] => d
                        .server
                        .push(Sample::Stats(t, [rss, cpu, rx, tx, syscr, read, write])),
                    _ => {}
                }
            }
            Some("T") => {
                if let Some(ns) = words.next().and_then(|w| w.parse().ok()) {
                    d.server.push(Sample::Tick(t, ns));
                }
            }
            Some("Q") => {
                // "... Percentiles: P50: 1.2ms P95: 3.4ms P99: 5.6ms. Sample: 100"
                let p = |key: &str| {
                    line.split(key)
                        .nth(1)
                        .and_then(|r| r.split("ms").next())
                        .and_then(|v| v.trim().parse::<f64>().ok())
                };
                if let (Some(a), Some(b), Some(c)) = (p("P50:"), p("P95:"), p("P99:")) {
                    let mean = p("Average time per tick:").unwrap_or(f64::NAN);
                    d.server.push(Sample::Query(t, [a, b, c, mean]));
                }
            }
            _ => {}
        }
    }
}

/// The metrics of one step: server values over the hold window, join latencies over the
/// whole step.
fn aggregate(
    d: &Collected,
    target: Target,
    wanted: u64,
    ramp_start: f64,
    from: f64,
    to: f64,
) -> Value {
    let inside = |t: f64| t >= from && t <= to;
    let stats: Vec<(f64, [u64; 7])> = d
        .server
        .iter()
        .filter_map(|s| match s {
            Sample::Stats(t, v) if inside(*t) && v[0] > 0 => Some((*t, *v)),
            _ => None,
        })
        .collect();
    let rate = |i: usize, scale: f64| -> Option<f64> {
        let (first, last) = (stats.first()?, stats.last()?);
        let dt = last.0 - first.0;
        (dt > 0.0).then(|| last.1[i].saturating_sub(first.1[i]) as f64 / dt * scale)
    };
    let rss_kib = stats.iter().map(|s| s.1[0]).max().unwrap_or(0);

    let mspt = match target {
        Target::Vanillite => {
            let mut ticks: Vec<f64> = d
                .server
                .iter()
                .filter_map(|s| match s {
                    Sample::Tick(t, ns) if inside(*t) => Some(*ns as f64 / 1e6),
                    _ => None,
                })
                .collect();
            ticks.sort_by(f64::total_cmp);
            json!({
                "source": "tick log",
                "ticks": ticks.len(),
                "mean": round(mean(&ticks)),
                "p50": round(pct(&ticks, 50.0)),
                "p95": round(pct(&ticks, 95.0)),
                "p99": round(pct(&ticks, 99.0)),
                "max": round(ticks.last().copied()),
            })
        }
        Target::Paper | Target::Vanilla | Target::Pumpkin => {
            let windows: Vec<[f64; 4]> = d
                .server
                .iter()
                .filter_map(|s| match s {
                    Sample::Query(t, v) if inside(*t) => Some(*v),
                    _ => None,
                })
                .collect();
            let col = |i: usize| -> Vec<f64> {
                windows
                    .iter()
                    .map(|w| w[i])
                    .filter(|v| v.is_finite())
                    .collect()
            };
            json!({
                "source": "/tick query, mean of 100-tick windows",
                "windows": windows.len(),
                "mean": round(mean(&col(3))),
                "p50": round(mean(&col(0))),
                "p95": round(mean(&col(1))),
                "p99": round(mean(&col(2))),
                "max": round(col(2).into_iter().reduce(f64::max)),
            })
        }
        // Proxies do not tick.
        _ => json!({ "source": "none" }),
    };

    let reports: Vec<&(f64, Value)> = d.bots.iter().filter(|(t, _)| inside(*t)).collect();
    let samples = |key: &str, after: f64| -> Vec<f64> {
        let mut v: Vec<f64> = d
            .bots
            .iter()
            .filter(|(t, _)| *t > after && *t <= to)
            .flat_map(|(_, r)| r[key].as_array().cloned().unwrap_or_default())
            .filter_map(|x| x.as_f64())
            .collect();
        v.sort_by(f64::total_cmp);
        v
    };
    let join = samples("join_ms", ramp_start);
    let full_view = samples("full_view_ms", ramp_start);
    let chunk_wait = samples("chunk_ms", from);
    let rtt = samples("rtt_ms", from);
    let last = reports.last().map(|r| r.1.clone()).unwrap_or_default();
    // Peak and mean of the bots online in the window (a crashing server drops them all).
    let online: Vec<f64> = reports
        .iter()
        .filter_map(|r| r.1["online"].as_f64())
        .collect();
    let players = online.iter().copied().fold(0.0, f64::max) as u64;
    let mean_online = mean(&online).unwrap_or(0.0);
    let bot_rate = |key: &str, per_bot: bool| -> Option<f64> {
        let (first, last) = (reports.first()?, reports.last()?);
        let dt = last.0 - first.0;
        let delta = last.1[key].as_u64()?.saturating_sub(first.1[key].as_u64()?) as f64;
        if per_bot && mean_online < 1.0 {
            return None;
        }
        let divisor = if per_bot { mean_online } else { 1.0 };
        (dt > 0.0).then(|| delta / dt / divisor)
    };
    let mut errors = serde_json::Map::new();
    for (t, r) in &d.bots {
        if *t > ramp_start
            && *t <= to
            && let Some(e) = r["errors"].as_object()
        {
            for (k, v) in e {
                let n = errors.get(k).and_then(Value::as_u64).unwrap_or(0);
                errors.insert(k.clone(), json!(n + v.as_u64().unwrap_or(0)));
            }
        }
    }
    let field = |k: &str| last[k].as_u64().unwrap_or(0);
    let delta = |k: &str| {
        let first = reports.first().map_or(0, |r| r.1[k].as_u64().unwrap_or(0));
        field(k).saturating_sub(first)
    };
    let per_player = |v: Option<f64>| v.filter(|_| players > 0).map(|v| v / players as f64);
    json!({
        "target": wanted,
        "players": players,
        "mspt": mspt,
        "server": {
            "rss_mib": round(Some(rss_kib as f64 / 1024.0)),
            "rss_per_player_kib": round((players > 0).then(|| rss_kib as f64 / players as f64)),
            "cpu_percent": round(rate(1, 100.0 / d.clock_ticks as f64)),
            "net_rx_mbps": round(rate(2, 1e-6)),
            "net_tx_mbps": round(rate(3, 1e-6)),
            "read_syscalls_per_s": round(rate(4, 1.0)),
            "read_syscalls_per_s_per_player": round(per_player(rate(4, 1.0))),
            "disk_read_mbps": round(rate(5, 1e-6)),
            "disk_read_kbps_per_player": round(per_player(rate(5, 1e-3))),
            "disk_write_mbps": round(rate(6, 1e-6)),
        },
        "bots": {
            "join_ms_p50": round(pct(&join, 50.0)),
            "join_ms_p95": round(pct(&join, 95.0)),
            "full_view_ms_p50": round(pct(&full_view, 50.0)),
            "full_view_ms_p95": round(pct(&full_view, 95.0)),
            "chunk_ms_p50": round(pct(&chunk_wait, 50.0)),
            "chunk_ms_p95": round(pct(&chunk_wait, 95.0)),
            "rtt_ms_p50": round(pct(&rtt, 50.0)),
            "rtt_ms_p95": round(pct(&rtt, 95.0)),
            "rtt_ms_p99": round(pct(&rtt, 99.0)),
            "in_kbps_per_bot": round(bot_rate("bytes_in", true).map(|v| v / 1e3)),
            "out_kbps_per_bot": round(bot_rate("bytes_out", true).map(|v| v / 1e3)),
            "packets_in_per_bot": round(bot_rate("packets_in", true)),
            "cpu_percent": round(bot_rate("cpu_ticks", false)),
            "chunks_per_s": round(bot_rate("chunks", false)),
            "chunks_per_s_per_bot": round(bot_rate("chunks", true)),
            "chunks_forgotten_per_s": round(bot_rate("chunks_forgotten", false)),
            "teleports": delta("teleports"),
            "near_missing_percent": round({
                let checked = delta("near_checked");
                (checked > 0).then(|| delta("near_missing") as f64 * 100.0 / checked as f64)
            }),
            "failed": field("failed"),
            "disconnected": field("disconnected"),
        },
        "errors": errors,
    })
}

pub(crate) fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// Nearest-rank percentile of sorted values.
pub(crate) fn pct(sorted: &[f64], p: f64) -> Option<f64> {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.clamp(1, sorted.len().max(1)) - 1).copied()
}

pub(crate) fn round(v: Option<f64>) -> Value {
    v.map_or(Value::Null, |v| json!((v * 100.0).round() / 100.0))
}

pub(crate) fn show(v: &Value) -> String {
    v.as_f64()
        .map_or_else(|| "-".to_owned(), |v| format!("{v}"))
}

pub(crate) fn max_players_under(steps: &[Value], ms: f64) -> u64 {
    steps
        .iter()
        .filter(|s| s["mspt"]["p95"].as_f64().is_some_and(|p| p < ms))
        .filter_map(|s| s["players"].as_u64())
        .max()
        .unwrap_or(0)
}

/// Compares p95 MSPT and RSS per player of every step with the median of the last runs of
/// the same server and settings; returns the regressions.
fn gate(host: &str, run: &Value) -> Result<Vec<String>> {
    let server = run["server"].as_str().unwrap_or_default();
    let listing = command_output(Command::new("ssh").args([
        host,
        &format!(
            "for f in $(ls -1 {RESULTS}/{server}/*.json 2> /dev/null | tail -n 20); do \
             tr -d '\\n' < \"$f\"; echo; done"
        ),
    ]))?;
    let history: Vec<Value> = listing
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|h| h["settings"] == run["settings"])
        // Only runs on the same hardware with the same limits, measured on a quiet host.
        .filter(|h| {
            h["hardware"]["baseline"] == run["hardware"]["baseline"]
                && h["limits"]["profile"] == run["limits"]["profile"]
                && h["load"]["unreliable"] != true
        })
        .collect();
    let history = &history[history.len().saturating_sub(GATE_HISTORY)..];
    if run["load"]["unreliable"] == true {
        println!("   regression gate: skipped, the host was overloaded during the run");
        return Ok(Vec::new());
    }
    if history.is_empty() {
        println!("   regression gate: no earlier {server} runs with these settings");
        return Ok(Vec::new());
    }
    let mut problems = Vec::new();
    let empty = Vec::new();
    for step in run["steps"].as_array().unwrap_or(&empty) {
        for (what, path) in [
            ("MSPT p95", ["mspt", "p95"]),
            ("RSS per player", ["server", "rss_per_player_kib"]),
        ] {
            let Some(now) = step[path[0]][path[1]].as_f64() else {
                continue;
            };
            let mut past: Vec<f64> = history
                .iter()
                .flat_map(|h| h["steps"].as_array().cloned().unwrap_or_default())
                .filter(|s| s["target"] == step["target"] && movement(s) == movement(step))
                .filter_map(|s| s[path[0]][path[1]].as_f64())
                .collect();
            past.sort_by(f64::total_cmp);
            let Some(median) = pct(&past, 50.0) else {
                continue;
            };
            if now > median * GATE_TOLERANCE && now - median > 0.05 {
                problems.push(format!(
                    "{server} at {} players: {what} {now} is more than 10 % above the median \
                     {median} of the last {} runs",
                    step["target"],
                    past.len()
                ));
            }
        }
    }
    println!(
        "   regression gate against {} earlier runs: {}",
        history.len(),
        if problems.is_empty() { "ok" } else { "FAILED" }
    );
    Ok(problems)
}

/// The movement of a step; steps from before movement modes walked randomly.
fn movement(step: &Value) -> &str {
    step["movement"].as_str().unwrap_or("random")
}

pub(crate) fn store(host: &str, run: &Value, path: &str) -> Result<()> {
    let dir = Path::new(path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut child = Command::new("ssh")
        .args([host, &format!("mkdir -p {} && cat > {path}", dir.display())])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    let text = serde_json::to_string(run).map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or("no ssh input")?
        .write_all(text.as_bytes())
        .map_err(|e| format!("ssh: {e}"))?;
    let status = child.wait().map_err(|e| format!("ssh: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("storing {path} failed: {status}"))
}

fn markdown(runs: &[Value]) -> String {
    let mut s = String::new();
    let first = &runs[0];
    s.push_str(&format!(
        "# Load test {}\n\nCommit `{}`{}; hold {} s per step, {} joins/s, view distance {}, chat {}.\n\
         Server: test server ({}), alone, limits {}; bots: development container.\n",
        first["date"].as_str().unwrap_or("?"),
        first["commit"].as_str().unwrap_or("?"),
        if first["dirty"].as_bool() == Some(true) { " (with local changes)" } else { "" },
        first["settings"]["hold_s"],
        first["settings"]["join_rate"],
        first["settings"]["view_distance"],
        match first["settings"]["chat_interval_s"].as_f64() {
            Some(c) if c > 0.0 => format!("every {c} s per bot"),
            _ => "off".to_owned(),
        },
        first["hardware"]["baseline"].as_str().unwrap_or("hardware not recorded"),
        first["limits"]["profile"].as_str().unwrap_or("not recorded"),
    ));
    for run in runs {
        s.push_str(&format!(
            "\n## {} ({}, run {})\n\nMSPT source: {}. Most players with p95 MSPT under 50 ms: {}.\n\n\
             | Players | MSPT p50 | p95 | p99 | max | RSS MiB | RSS/player KiB | CPU % | Net out MB/s | Net in MB/s | Join p50/p95 ms | RTT p50/p95 ms | In KB/s per bot | Failed / dropped |\n\
             |---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
            run["server"].as_str().unwrap_or("?"),
            run["profile"].as_str().unwrap_or("?"),
            run["date"].as_str().unwrap_or("?"),
            run["steps"][0]["mspt"]["source"].as_str().unwrap_or("-"),
            run["max_players_under_50ms"],
        ));
        if run["load"]["unreliable"] == true {
            s.push_str(&format!(
                "**Unreliable:** the host was overloaded during this run (load {} per CPU).\n\n",
                run["load"]["host_load_per_cpu"]
            ));
        }
        for st in run["steps"].as_array().into_iter().flatten() {
            s.push_str(&format!(
                "| {} / {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} / {} | {} / {} | {} | {} / {} |\n",
                st["players"], st["target"],
                show(&st["mspt"]["p50"]), show(&st["mspt"]["p95"]), show(&st["mspt"]["p99"]),
                show(&st["mspt"]["max"]),
                show(&st["server"]["rss_mib"]), show(&st["server"]["rss_per_player_kib"]),
                show(&st["server"]["cpu_percent"]), show(&st["server"]["net_tx_mbps"]),
                show(&st["server"]["net_rx_mbps"]),
                show(&st["bots"]["join_ms_p50"]), show(&st["bots"]["join_ms_p95"]),
                show(&st["bots"]["rtt_ms_p50"]), show(&st["bots"]["rtt_ms_p95"]),
                show(&st["bots"]["in_kbps_per_bot"]),
                st["bots"]["failed"], st["bots"]["disconnected"],
            ));
        }
        s.push_str(
            "\nChunk loading (chunks: received by the bots; disk: the server process's storage \
             I/O; read calls include socket reads):\n\n\
             | Players | Movement | Chunks/s | Chunks/s per bot | Forgotten/s | Teleports | \
             Disk read MB/s | Disk read KB/s per player | Read calls/s per player | \
             Disk write MB/s | RSS/player KiB |\n\
             |---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for st in run["steps"].as_array().into_iter().flatten() {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                st["players"],
                movement(st),
                show(&st["bots"]["chunks_per_s"]),
                show(&st["bots"]["chunks_per_s_per_bot"]),
                show(&st["bots"]["chunks_forgotten_per_s"]),
                show(&st["bots"]["teleports"]),
                show(&st["server"]["disk_read_mbps"]),
                show(&st["server"]["disk_read_kbps_per_player"]),
                show(&st["server"]["read_syscalls_per_s_per_player"]),
                show(&st["server"]["disk_write_mbps"]),
                show(&st["server"]["rss_per_player_kib"]),
            ));
        }
        let errors: Vec<String> = run["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|st| st["errors"].as_object().cloned().unwrap_or_default())
            .map(|(k, v)| format!("{v} × {k}"))
            .collect();
        if !errors.is_empty() {
            s.push_str(&format!("\nBot errors: {}\n", errors.join("; ")));
        }
    }
    s
}

/// Small multiples over the player count (log scale): MSPT p95, RSS and outgoing traffic,
/// one line per server.
fn svg(runs: &[Value]) -> String {
    const SERIES: [(&str, &str); 2] = [("vanillite", "#2a78d6"), ("paper", "#eb6834")];
    const PANELS: [(&str, [&str; 2], &str); 3] = [
        ("MSPT p95 (ms)", ["mspt", "p95"], "ms"),
        ("RSS (MiB)", ["server", "rss_mib"], "MiB"),
        ("Network out (MB/s)", ["server", "net_tx_mbps"], "MB/s"),
    ];
    let (pw, ph, left, top, gap) = (300.0, 220.0, 56.0, 84.0, 40.0);
    let width = left + 3.0 * pw + 2.0 * gap + 24.0;
    let height = top + ph + 72.0;
    let points = |run: &Value, path: [&str; 2]| -> Vec<(f64, f64)> {
        run["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| Some((s["players"].as_f64()?, s[path[0]][path[1]].as_f64()?)))
            .filter(|(p, _)| *p > 0.0)
            .collect()
    };
    let all: Vec<f64> = runs
        .iter()
        .flat_map(|r| points(r, ["mspt", "p95"]))
        .map(|p| p.0)
        .collect();
    let xmin = all
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min)
        .clamp(1.0, 50.0);
    let xmax = all.iter().copied().fold(0.0, f64::max).max(xmin * 10.0);
    let (lx0, lx1) = (xmin.log10(), xmax.log10());

    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" \
         viewBox=\"0 0 {width} {height}\" font-family=\"system-ui, sans-serif\" font-size=\"12\">\n\
         <rect width=\"100%\" height=\"100%\" fill=\"#fcfcfb\"/>\n\
         <text x=\"{left}\" y=\"22\" font-size=\"15\" font-weight=\"600\" fill=\"#0b0b0b\">\
         Vanillite load test: server cost by player count</text>\n"
    );
    // Legend.
    let mut lx = left;
    for (name, color) in SERIES {
        if runs.iter().any(|r| r["server"] == name) {
            s.push_str(&format!(
                "<rect x=\"{lx}\" y=\"34\" width=\"14\" height=\"3\" rx=\"1.5\" fill=\"{color}\"/>\
                 <text x=\"{}\" y=\"40\" fill=\"#52514e\">{name}</text>\n",
                lx + 20.0
            ));
            lx += 100.0;
        }
    }
    for (i, (title, path, unit)) in PANELS.iter().enumerate() {
        let x0 = left + i as f64 * (pw + gap);
        let ymax = runs
            .iter()
            .flat_map(|r| points(r, *path))
            .map(|p| p.1)
            .fold(0.0, f64::max);
        let ymax = nice_ceiling(ymax);
        let px = |p: f64| x0 + (p.log10() - lx0) / (lx1 - lx0) * pw;
        let py = |v: f64| top + ph - v / ymax * ph;
        s.push_str(&format!(
            "<text x=\"{x0}\" y=\"{}\" font-weight=\"600\" fill=\"#0b0b0b\">{title}</text>\n",
            top - 10.0
        ));
        for k in 0..=4 {
            let v = ymax * f64::from(k) / 4.0;
            let y = py(v);
            s.push_str(&format!(
                "<line x1=\"{x0}\" x2=\"{}\" y1=\"{y}\" y2=\"{y}\" stroke=\"#e4e3df\" stroke-width=\"1\"/>\
                 <text x=\"{}\" y=\"{}\" text-anchor=\"end\" fill=\"#52514e\">{}</text>\n",
                x0 + pw,
                x0 - 6.0,
                y + 4.0,
                trim(v)
            ));
        }
        for tick in [
            10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0,
        ] {
            if tick < xmin * 0.99 || tick > xmax * 1.01 {
                continue;
            }
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" fill=\"#52514e\">{}</text>\n",
                px(tick),
                top + ph + 18.0,
                trim(tick)
            ));
        }
        s.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" fill=\"#52514e\">players (log scale)</text>\n",
            x0 + pw / 2.0,
            top + ph + 38.0
        ));
        if *unit == "ms" {
            let y = py(50.0);
            if y >= top {
                s.push_str(&format!(
                    "<line x1=\"{x0}\" x2=\"{}\" y1=\"{y}\" y2=\"{y}\" stroke=\"#52514e\" \
                     stroke-dasharray=\"4 4\"/><text x=\"{}\" y=\"{}\" text-anchor=\"end\" \
                     fill=\"#52514e\">50 ms budget</text>\n",
                    x0 + pw,
                    x0 + pw,
                    y - 4.0
                ));
            }
        }
        for (name, color) in SERIES {
            let Some(run) = runs.iter().find(|r| r["server"] == name) else {
                continue;
            };
            let pts = points(run, *path);
            let line: Vec<String> = pts
                .iter()
                .map(|(p, v)| format!("{:.1},{:.1}", px(*p), py(*v)))
                .collect();
            s.push_str(&format!(
                "<polyline points=\"{}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"2\" \
                 stroke-linejoin=\"round\"/>\n",
                line.join(" ")
            ));
            for (p, v) in &pts {
                s.push_str(&format!(
                    "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"4\" fill=\"{color}\" stroke=\"#fcfcfb\" \
                     stroke-width=\"2\"><title>{name}: {p} players, {} {unit}</title></circle>\n",
                    px(*p),
                    py(*v),
                    trim(*v)
                ));
            }
        }
    }
    s.push_str("</svg>\n");
    s
}

/// A round axis maximum at or above `v`.
fn nice_ceiling(v: f64) -> f64 {
    if v <= 0.0 {
        return 1.0;
    }
    let magnitude = 10f64.powf(v.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0]
        .iter()
        .map(|m| m * magnitude)
        .find(|c| *c >= v)
        .unwrap_or(10.0 * magnitude)
}

fn trim(v: f64) -> String {
    if v >= 100.0 || v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Waits on the benchmark server until `port` accepts connections there.
fn wait_for_port(host: &str, port: u16, timeout: Duration) -> Result<()> {
    let secs = timeout.as_secs();
    ssh(
        host,
        &format!(
            "timeout {secs} bash -c 'until (exec 3<>/dev/tcp/127.0.0.1/{port}) 2> /dev/null; \
             do sleep 2; done'"
        ),
    )
    .map_err(|_| format!("{host}:{port} did not open within {secs} s"))
}

/// SSH host of the benchmark server: `VANILLITE_BENCH_HOST`, else vanillite-bench.
pub(crate) fn bench_host() -> String {
    std::env::var("VANILLITE_BENCH_HOST").unwrap_or_else(|_| BENCH_HOST.to_owned())
}

/// Installs a binary from here as /opt/vanillite/bench/bin/<name> on the benchmark server.
pub(crate) fn install(host: &str, name: &str, file: &Path) -> Result<()> {
    let f = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    crate::devenv::run(
        Command::new("ssh")
            .args([host, "vanillite-bench", "install", name])
            .stdin(Stdio::from(f)),
    )
}

fn finish(child: &mut Child, grace: Duration) {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `vanillite-bench <args>` on the test server; its output.
pub(crate) fn bench_cmd(host: &str, args: &[&str]) -> Result<String> {
    let quoted: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    command_output(
        Command::new("ssh").args([host, &format!("vanillite-bench {}", quoted.join(" "))]),
    )
}

/// `vanillite-bench <args>` on the test server; the last line of its output as JSON.
pub(crate) fn bench_json(host: &str, args: &[&str]) -> Result<Value> {
    let out = bench_cmd(host, args)?;
    let line = out.lines().last().unwrap_or_default();
    serde_json::from_str(line)
        .map_err(|e| format!("vanillite-bench {}: {e}: {out}", args.join(" ")))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The benchmark server's hardware (`vanillite-bench hw`), the CPUs the bots run on, and the
/// `baseline`: what must stay the same for results to be compared.
pub(crate) fn hardware(host: &str) -> Result<Value> {
    let mut hw = bench_json(host, &["hw"])?;
    let gib = |v: &Value| (v.as_f64().unwrap_or(0.0) / 1024.0).round();
    hw["baseline"] = json!(format!(
        "{}, {} CPUs and {} GiB RAM (host: {} CPUs, {} GiB)",
        hw["cpu_model"].as_str().unwrap_or("?"),
        hw["cpus"],
        gib(&hw["mem_mib"]),
        hw["host_cpus"],
        gib(&hw["host_mem_mib"]),
    ));
    let bot_cpus = hw["bot_cpu_list"].as_str().unwrap_or_default().to_owned();
    hw["bots"] = json!({
        "cpu_model": hw["cpu_model"],
        "cpus": bot_cpus.split(',').filter(|c| !c.is_empty()).count(),
        "cpu_list": bot_cpus,
        "same_machine": true,
    });
    Ok(hw)
}

/// Host load averages and the test server's CPU pressure counter now (`Null` when unknown).
pub(crate) fn load_sample(host: &str) -> Value {
    bench_json(host, &["load"]).unwrap_or(Value::Null)
}

/// The load between two samples: the host's load (mean of the 1-minute averages at both ends
/// and the 5-minute one at the end), the share of time the test server's tasks waited for a
/// CPU, the share of CPU time the hypervisor gave to other machines (steal), and whether the
/// host was overloaded (load per host CPU above 1, or more than 5 % steal).
pub(crate) fn load_window(before: &Value, after: &Value, hw: &Value) -> Value {
    let (Some(t0), Some(t1)) = (before["t"].as_f64(), after["t"].as_f64()) else {
        return Value::Null;
    };
    let loads: Vec<f64> = [&before["load1"], &after["load1"], &after["load5"]]
        .iter()
        .filter_map(|v| v.as_f64())
        .collect();
    let host_load = mean(&loads);
    let pressure = match (
        before["cpu_pressure_some_us"].as_f64(),
        after["cpu_pressure_some_us"].as_f64(),
    ) {
        (Some(a), Some(b)) if t1 > t0 => Some((b - a) / ((t1 - t0) * 1e6) * 100.0),
        _ => None,
    };
    let per_cpu = host_load
        .zip(hw["host_cpus"].as_f64().filter(|n| *n > 0.0))
        .map(|(l, n)| l / n);
    let steal = match (
        before["steal_s"].as_f64(),
        after["steal_s"].as_f64(),
        hw["cpus"].as_f64().filter(|n| *n > 0.0),
    ) {
        (Some(a), Some(b), Some(n)) if t1 > t0 => Some((b - a) / ((t1 - t0) * n) * 100.0),
        _ => None,
    };
    json!({
        "seconds": round(Some(t1 - t0)),
        "host_load": round(host_load),
        "host_load_per_cpu": round(per_cpu),
        "cpu_pressure_percent": round(pressure),
        "steal_percent": round(steal),
        "unreliable": per_cpu.is_some_and(|l| l > OVERLOAD_PER_CPU)
            || steal.is_some_and(|s| s > MAX_STEAL_PERCENT),
    })
}

pub(crate) fn ssh(host: &str, command: &str) -> Result<()> {
    crate::devenv::run(Command::new("ssh").args([host, command]))
}

pub(crate) fn git(root: &Path, args: &[&str]) -> Result<String> {
    command_output(Command::new("git").current_dir(root).args(args))
}

pub(crate) fn command_output(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(format!(
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}
