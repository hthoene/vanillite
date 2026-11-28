//! `cargo xtask parity worlds`: complex real-world worlds and schematics (redstone, farms,
//! machines) in Vanilla and Vanillite, compared tick by tick (#244).
//!
//! The corpus is `crates/vanillite-test/parity-worlds.toml` (format there): URL, SHA-256,
//! licence and contents of every world. Nothing of it is in the repository; the files are
//! downloaded into `<cache>/parity-worlds/<name>/` (`~/.cache/vanillite/parity-worlds/`) and
//! verified by their SHA-256.
//!
//! 1. Prepare (once per world, file and Minecraft version): the pinned Vanilla server upgrades
//!    the world (`--forceUpgrade`), or pastes the schematic (converted to a structure template
//!    by `golden/WorldPrep.java`) with `/place template` into a void world at [`ORIGIN`]; the
//!    game is frozen (`/tick freeze`) before anything ticks, the area is saved. That upgraded
//!    copy is the snapshot both servers start from.
//! 2. Each world becomes a soak scenario (`target/parity-worlds/scenarios/<name>.json`): the
//!    snapshot, its region, rules, triggers (actions) and ticks; frames hold the blocks, block
//!    and fluid ticks, the entities created while it runs and the block entities with their
//!    full data. `parity_soak --dir` runs them: Vanilla's per-tick checksums are recorded once
//!    into the bulk store, Vanillite compares every tick and the first diverging tick gets a
//!    full diff (`target/parity-worlds/<name>.md`, `summary.md`).

use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Result;
use crate::golden;
use crate::vanilla::{download, java_tool, remove_dir, run, sha256_file};

/// Where a schematic's minimum corner is pasted.
pub const ORIGIN: [i32; 3] = [0, 0, 0];
/// Chunks around the region that the preparation loads and saves, so neither server generates
/// terrain next to the machine (the scenario worlds read up to 5 chunks around the region).
const PREPARE_MARGIN: i32 = 6;
/// Blocks around a schematic's box that its default region observes.
const REGION_MARGIN: i32 = 2;
/// Default length of a world's run.
const TICKS: u32 = 1200;

/// One world of the manifest.
struct World {
    name: String,
    group: String,
    url: String,
    sha256: String,
    format: String,
    contents: String,
    licence: String,
    source: String,
    world_path: Option<String>,
    seed: i64,
    region: Option<[[i32; 3]; 2]>,
    ticks: Option<u32>,
    rules: toml::Table,
    difficulty: Option<String>,
    actions: Vec<Value>,
    entities: Vec<Value>,
    /// Flip every lever of a schematic at `trigger_tick` when it has no actions.
    trigger: bool,
    trigger_tick: u32,
    /// The world's own entities load with its chunks (`world.entities`).
    snapshot_entities: bool,
    /// Where the scripted player stands (`player`; relative for schematics); `None` = the
    /// default spot, `Some(None)` = no player (`player = "none"`).
    player: Option<Option<[f64; 3]>>,
}

/// `cargo xtask parity worlds [--ticks N] [--jobs N] [--minutes N] [--prepare-only]
/// [<name>...]`.
pub fn worlds(root: &Path, args: &[&str]) -> Result<()> {
    let mut ticks = None;
    let mut pass: Vec<String> = Vec::new();
    let mut only = Vec::new();
    let mut prepare_only = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().copied().ok_or(format!("{name} needs a value"));
        match *arg {
            "--ticks" => {
                ticks = Some(
                    value("--ticks")?
                        .parse::<u32>()
                        .map_err(|e| format!("--ticks: {e}"))?,
                );
            }
            "--jobs" | "--minutes" => {
                let v = value(arg)?;
                pass.push((*arg).to_owned());
                pass.push(v.to_owned());
            }
            "--prepare-only" => prepare_only = true,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            name => only.push(name.to_owned()),
        }
    }
    let manifest = root.join("crates/vanillite-test/parity-worlds.toml");
    let all = read_manifest(&manifest)?;
    if let Some(n) = only.iter().find(|n| !all.iter().any(|w| &w.name == *n)) {
        return Err(format!("no world {n} in {}", manifest.display()));
    }
    let todo: Vec<&World> = all
        .iter()
        .filter(|w| only.is_empty() || only.contains(&w.name))
        .collect();

    let (classpath, agent, work_dir) = crate::parity::recorder(root)?;
    let key = golden::Key::current(root)?;
    let cache = vanillite_datagen::cache::cache_root(root).join("parity-worlds");
    let out = root.join("target/parity-worlds");
    remove_dir(&out)?;
    let scenarios = out.join("scenarios");
    std::fs::create_dir_all(&scenarios).map_err(|e| format!("{}: {e}", scenarios.display()))?;

    let mut corpus = String::from(
        "\n## Corpus\n\n| world | group | contents | licence | prepared |\n|---|---|---|---|---|\n",
    );
    // A few Vanilla servers at a time.
    let parallel = std::thread::available_parallelism()
        .map_or(1, |n| (n.get() / 3).max(1))
        .min(4);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let rows: std::sync::Mutex<Vec<(usize, String, bool)>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..parallel.min(todo.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(w) = todo.get(i) else { return };
                    let started = Instant::now();
                    let result = prepare(
                        w,
                        &cache.join(&w.name),
                        &key.minecraft,
                        &work_dir,
                        &classpath,
                    )
                    .and_then(|p| {
                        let text = serde_json::to_string_pretty(&scenario(w, &p, ticks))
                            .map_err(|e| e.to_string())?;
                        let file = scenarios.join(format!("{}.json", w.name));
                        std::fs::write(&file, text).map_err(|e| format!("{}: {e}", file.display()))
                    });
                    let (status, ok) = match result {
                        Ok(()) => (format!("{:.0} s", started.elapsed().as_secs_f64()), true),
                        Err(e) => {
                            println!("parity worlds: {}: {e}", w.name);
                            (
                                format!("ERROR: {}", e.lines().next().unwrap_or_default()),
                                false,
                            )
                        }
                    };
                    let row = format!(
                        "| {} | {} | {} | {} | {status} |\n",
                        w.name,
                        w.group,
                        w.contents,
                        w.licence.replace('|', "/")
                    );
                    rows.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((i, row, ok));
                }
            });
        }
    });
    let mut rows = rows
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    rows.sort_by_key(|r| r.0);
    let prepared = rows.iter().filter(|r| r.2).count();
    for (_, row, _) in &rows {
        corpus.push_str(row);
    }
    println!(
        "parity worlds: {prepared} of {} worlds prepared",
        todo.len()
    );
    std::fs::write(out.join("corpus.md"), &corpus).map_err(|e| e.to_string())?;
    if prepare_only || prepared == 0 {
        return if prepared == todo.len() {
            Ok(())
        } else {
            Err("not every world could be prepared".to_owned())
        };
    }

    let result = run(
        Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
            .current_dir(root)
            .args([
                "run",
                "-q",
                "--profile",
                "release-fast",
                "-p",
                "vanillite-test",
                "--bin",
                "parity_soak",
                "--",
            ])
            .arg("--java")
            .arg(java_tool("java"))
            .arg("--agent")
            .arg(&agent)
            .arg("--classpath")
            .arg(&classpath)
            .arg("--work-dir")
            .arg(&work_dir)
            .arg("--out")
            .arg(&out)
            .arg("--dir")
            .arg(&scenarios)
            .args(["--title", "Parity worlds (#244)"])
            .args(&pass),
    );
    let summary = out.join("summary.md");
    let mut text = std::fs::read_to_string(&summary).unwrap_or_default();
    text.push_str(&corpus);
    std::fs::write(&summary, text).map_err(|e| format!("{}: {e}", summary.display()))?;
    println!("parity worlds: {}", summary.display());
    result?;
    if prepared == todo.len() {
        Ok(())
    } else {
        Err("not every world could be prepared".to_owned())
    }
}

fn read_manifest(path: &Path) -> Result<Vec<World>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(entries) = table.get("world").and_then(toml::Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut worlds = Vec::new();
    for e in entries {
        let s = |k: &str| e.get(k).and_then(toml::Value::as_str).map(str::to_owned);
        let name = s("name").ok_or("a world without name")?;
        let need = |k: &str| s(k).ok_or(format!("world {name}: {k} is missing"));
        let url = need("url")?;
        let format = match s("format") {
            Some(f) => f,
            None => url
                .rsplit('.')
                .next()
                .map(|x| x.to_ascii_lowercase())
                .filter(|x| ["litematic", "schem", "schematic", "nbt", "zip"].contains(&x.as_str()))
                .map(|x| if x == "zip" { "world".to_owned() } else { x })
                .ok_or(format!("world {name}: set format"))?,
        };
        let json = |k: &str| -> Result<Vec<Value>> {
            e.get(k)
                .and_then(toml::Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
                        .collect()
                })
                .unwrap_or(Ok(Vec::new()))
        };
        let region = match e.get("region") {
            Some(r) => {
                let v: Vec<Vec<i32>> = r
                    .clone()
                    .try_into()
                    .map_err(|e| format!("world {name}: region: {e}"))?;
                match v.as_slice() {
                    [a, b] if a.len() == 3 && b.len() == 3 => {
                        Some([[a[0], a[1], a[2]], [b[0], b[1], b[2]]])
                    }
                    _ => return Err(format!("world {name}: region is [[x, y, z], [x, y, z]]")),
                }
            }
            None => None,
        };
        worlds.push(World {
            group: need("group")?,
            sha256: need("sha256")?.to_ascii_lowercase(),
            contents: need("contents")?,
            licence: need("licence")?,
            source: s("source").unwrap_or_else(|| url.clone()),
            world_path: s("world_path"),
            seed: e.get("seed").and_then(toml::Value::as_integer).unwrap_or(0),
            region,
            ticks: e
                .get("ticks")
                .and_then(toml::Value::as_integer)
                .and_then(|t| u32::try_from(t).ok()),
            rules: e
                .get("rules")
                .and_then(toml::Value::as_table)
                .cloned()
                .unwrap_or_default(),
            difficulty: s("difficulty"),
            actions: json("actions")?,
            entities: json("entities")?,
            trigger: s("trigger").is_none_or(|t| t != "none"),
            trigger_tick: e
                .get("trigger_tick")
                .and_then(toml::Value::as_integer)
                .and_then(|t| u32::try_from(t).ok())
                .unwrap_or(20),
            snapshot_entities: e
                .get("snapshot_entities")
                .and_then(toml::Value::as_bool)
                .unwrap_or(true),
            player: match e.get("player") {
                None => None,
                Some(toml::Value::String(s)) if s == "none" => Some(None),
                Some(v) => {
                    let p: Vec<f64> = v
                        .clone()
                        .try_into()
                        .map_err(|e| format!("world {name}: player: {e}"))?;
                    match p[..] {
                        [x, y, z] => Some(Some([x, y, z])),
                        _ => return Err(format!("world {name}: player is [x, y, z] or \"none\"")),
                    }
                }
            },
            format,
            url,
            name,
        });
    }
    let mut names: Vec<&str> = worlds.iter().map(|w| w.name.as_str()).collect();
    names.sort_unstable();
    if let Some(d) = names.windows(2).find(|p| p[0] == p[1]) {
        return Err(format!("world {} is twice in the manifest", d[0]));
    }
    Ok(worlds)
}

/// An upgraded world, ready for both servers.
struct Prepared {
    /// The overworld folder with `region/` and `poi/`.
    snapshot: PathBuf,
    /// The pasted box of a schematic (inclusive).
    pasted: Option<[[i32; 3]; 2]>,
    /// The levers of a schematic (absolute) with their state flipped.
    levers: Vec<(BlockPos3, String)>,
    /// The buttons of a schematic (absolute).
    buttons: Vec<BlockPos3>,
    /// A world's player (or else spawn) position: the default region is around it.
    focus: Option<BlockPos3>,
}

type BlockPos3 = [i32; 3];

/// Bumped when the preparation changes, so cached ones are made again.
const PREPARATION: u32 = 2;

/// Runs `golden/WorldPrep.java` and returns its output.
fn world_prep(work_dir: &Path, classpath: &str, args: &[&Path]) -> Result<String> {
    let output = Command::new(java_tool("java"))
        .current_dir(work_dir)
        // A bounded heap: the JVM's default (a quarter of the memory the host reports) is more
        // than a container may have, and the system kills it as soon as others need memory.
        .arg("-Xmx3G")
        .arg("-cp")
        .arg(classpath)
        .arg("dev.vanillite.extractor.golden.WorldPrep")
        .args(args)
        .output()
        .map_err(|e| format!("java: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "WorldPrep failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `<word> x y z [rest]` lines of WorldPrep's output.
fn positions<'a>(text: &'a str, word: &str) -> Vec<(BlockPos3, &'a str)> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.splitn(5, ' ');
            if parts.next()? != word {
                return None;
            }
            let mut c = || parts.next()?.parse::<f64>().ok().map(|v| v.floor() as i32);
            let pos = [c()?, c()?, c()?];
            Some((pos, parts.next().unwrap_or("")))
        })
        .collect()
}

/// Downloads, verifies and prepares a world in `dir` (cached).
fn prepare(
    w: &World,
    dir: &Path,
    minecraft: &str,
    work_dir: &Path,
    classpath: &str,
) -> Result<Prepared> {
    let downloads = dir.join("download");
    std::fs::create_dir_all(&downloads).map_err(|e| format!("{}: {e}", downloads.display()))?;
    let ext = if w.format == "world" {
        "zip"
    } else {
        w.format.as_str()
    };
    let file = downloads.join(format!("{}.{ext}", w.name));
    if !file.exists() || sha256_file(&file)? != w.sha256 {
        println!("parity worlds: downloading {}", w.url);
        download(&w.url, &file)?;
        let actual = sha256_file(&file)?;
        if actual != w.sha256 {
            let _ = std::fs::remove_file(&file);
            return Err(format!(
                "SHA-256 mismatch of {}: expected {}, got {actual}",
                w.url, w.sha256
            ));
        }
    }
    let prepared = dir.join(format!(
        "prepared-{minecraft}-{}-v{PREPARATION}",
        &w.sha256[..12]
    ));
    let marker = prepared.join("prepared.json");
    if let Ok(text) = std::fs::read_to_string(&marker) {
        let mut v: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", marker.display()))?;
        let pasted: Option<[BlockPos3; 2]> = serde_json::from_value(v["pasted"].clone()).ok();
        // Preparations from before the buttons were listed: only the schematic is read again.
        if let Some([origin, _]) = pasted
            && v.get("buttons").is_none()
        {
            let scratch = prepared.join("buttons.nbt");
            let text = world_prep(work_dir, classpath, &[&file, &scratch])?;
            let _ = std::fs::remove_file(&scratch);
            v["buttons"] = json!(buttons(&text, origin));
            std::fs::write(&marker, v.to_string())
                .map_err(|e| format!("{}: {e}", marker.display()))?;
        }
        return Ok(Prepared {
            snapshot: PathBuf::from(v["snapshot"].as_str().unwrap_or_default()),
            pasted,
            levers: serde_json::from_value(v["levers"].clone()).unwrap_or_default(),
            buttons: serde_json::from_value(v["buttons"].clone()).unwrap_or_default(),
            focus: serde_json::from_value(v["focus"].clone()).ok(),
        });
    }
    // Earlier preparations (other versions or files) go.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("prepared-") {
                remove_dir(&e.path())?;
            }
        }
    }
    println!(
        "parity worlds: preparing {} with Vanilla {minecraft}",
        w.name
    );
    let run_dir = prepared.join("run");
    let world = run_dir.join("world");
    std::fs::create_dir_all(&run_dir).map_err(|e| format!("{}: {e}", run_dir.display()))?;
    let mut pasted = None;
    let mut levers = Vec::new();
    let mut buttons_at = Vec::new();
    let mut focus = None;
    let mut commands = vec!["tick freeze".to_owned()];
    if w.format == "world" {
        unpack_world(&file, w.world_path.as_deref(), &world)?;
        let text = world_prep(
            work_dir,
            classpath,
            &[Path::new("level"), &world.join("level.dat")],
        )?;
        focus = positions(&text, "focus").first().map(|p| p.0);
    } else {
        let structure = world
            .join("generated/minecraft/structure")
            .join(format!("parity_{}.nbt", w.name));
        let text = world_prep(work_dir, classpath, &[&file, &structure])?;
        let size: Vec<i32> = text
            .lines()
            .find_map(|l| l.strip_prefix("size "))
            .ok_or("WorldPrep printed no size")?
            .split(' ')
            .filter_map(|v| v.parse().ok())
            .collect();
        let [sx, sy, sz] = size[..] else {
            return Err("WorldPrep printed a bad size".to_owned());
        };
        if sx < 1 || sy < 1 || sz < 1 || sy > 384 {
            return Err(format!("the schematic's size {sx}x{sy}x{sz} does not fit"));
        }
        let origin = [ORIGIN[0], if sy > 320 { -64 } else { ORIGIN[1] }, ORIGIN[2]];
        let max = [origin[0] + sx - 1, origin[1] + sy - 1, origin[2] + sz - 1];
        pasted = Some([origin, max]);
        levers = positions(&text, "lever")
            .into_iter()
            .map(|(p, state)| {
                (
                    [p[0] + origin[0], p[1] + origin[1], p[2] + origin[2]],
                    state.to_owned(),
                )
            })
            .collect();
        buttons_at = buttons(&text, origin);
        commands.extend(forceload(
            [origin[0] >> 4, origin[2] >> 4],
            [max[0] >> 4, max[2] >> 4],
        ));
        commands.push(format!(
            "place template minecraft:parity_{} {} {} {}",
            w.name, origin[0], origin[1], origin[2]
        ));
    }
    if let Some([min, max]) = w.region {
        commands.extend(forceload(
            [min[0] >> 4, min[2] >> 4],
            [max[0] >> 4, max[2] >> 4],
        ));
    }
    std::fs::write(run_dir.join("eula.txt"), "eula=true\n").map_err(|e| e.to_string())?;
    std::fs::write(run_dir.join("server.properties"), properties(free_port()?))
        .map_err(|e| e.to_string())?;
    vanilla_session(
        work_dir,
        &run_dir,
        w.format == "world",
        &commands,
        &prepared.join("vanilla.log"),
    )?;

    let dimension = world.join("dimensions/minecraft/overworld");
    let snapshot = if dimension.join("region").is_dir() {
        dimension
    } else {
        world.clone()
    };
    if !snapshot.join("region").is_dir() {
        return Err(format!(
            "{}: no region folder after the upgrade",
            snapshot.display()
        ));
    }
    std::fs::create_dir_all(snapshot.join("poi")).map_err(|e| e.to_string())?;
    // What the scenario runs do not read.
    for leftover in ["libraries", "versions", "logs"] {
        remove_dir(&run_dir.join(leftover))?;
    }
    let snapshot = std::fs::canonicalize(&snapshot).map_err(|e| e.to_string())?;
    let v = json!({
        "snapshot": snapshot.to_string_lossy(),
        "pasted": pasted,
        "levers": levers,
        "buttons": buttons_at,
        "focus": focus,
    });
    std::fs::write(&marker, v.to_string()).map_err(|e| format!("{}: {e}", marker.display()))?;
    Ok(Prepared {
        snapshot,
        pasted,
        levers,
        buttons: buttons_at,
        focus,
    })
}

/// The `button x y z` lines of WorldPrep's output, absolute for a paste at `origin`.
fn buttons(text: &str, origin: BlockPos3) -> Vec<BlockPos3> {
    positions(text, "button")
        .into_iter()
        .map(|(p, _)| [p[0] + origin[0], p[1] + origin[1], p[2] + origin[2]])
        .collect()
}

/// `/forceload add` commands for the chunks `min..=max` and [`PREPARE_MARGIN`] around them,
/// in squares of at most 256 chunks.
fn forceload(min: [i32; 2], max: [i32; 2]) -> Vec<String> {
    let (x0, z0) = (min[0] - PREPARE_MARGIN, min[1] - PREPARE_MARGIN);
    let (x1, z1) = (max[0] + PREPARE_MARGIN, max[1] + PREPARE_MARGIN);
    let mut out = Vec::new();
    for x in (x0..=x1).step_by(16) {
        for z in (z0..=z1).step_by(16) {
            out.push(format!(
                "forceload add {} {} {} {}",
                x * 16,
                z * 16,
                (x + 15).min(x1) * 16 + 15,
                (z + 15).min(z1) * 16 + 15
            ));
        }
    }
    out
}

/// A void flat world without structures; the server only listens on localhost.
fn properties(port: u16) -> String {
    format!(
        "level-name=world\nlevel-seed=0\nonline-mode=false\nserver-ip=127.0.0.1\n\
         server-port={port}\nenable-query=false\nenable-rcon=false\nenable-status=false\n\
         spawn-protection=0\nmax-tick-time=-1\nview-distance=2\nsimulation-distance=2\n\
         generate-structures=false\nsync-chunk-writes=true\nlevel-type=minecraft\\:flat\n\
         generator-settings={{\"layers\"\\:[{{\"block\"\\:\"minecraft\\:air\",\"height\"\\:1}}],\
         \"biome\"\\:\"minecraft\\:plains\"}}\n"
    )
}

fn free_port() -> Result<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free port: {e}"))
}

/// Unpacks the world folder (the one with `level.dat`, or `world_path`) of a zip into `to`.
fn unpack_world(zip: &Path, world_path: Option<&str>, to: &Path) -> Result<()> {
    let file = std::fs::File::open(zip).map_err(|e| format!("{}: {e}", zip.display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("{}: {e}", zip.display()))?;
    let prefix = match world_path {
        Some(p) => format!("{}/", p.trim_end_matches('/')),
        None => {
            let mut best: Option<String> = None;
            for i in 0..archive.len() {
                let name = archive
                    .by_index(i)
                    .map_err(|e| e.to_string())?
                    .name()
                    .map_err(|e| e.to_string())?
                    .into_owned();
                if let Some(dir) = name.strip_suffix("level.dat")
                    && (dir.is_empty() || dir.ends_with('/'))
                    && best.as_ref().is_none_or(|b| dir.len() < b.len())
                {
                    best = Some(dir.to_owned());
                }
            }
            best.ok_or(format!("{}: no level.dat", zip.display()))?
        }
    };
    let prefix = if prefix == "/" { String::new() } else { prefix };
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let Some(path) = entry.enclosed_name() else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(&prefix) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = to.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out =
            std::fs::File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    if !to.join("level.dat").is_file() {
        return Err(format!("{}: no level.dat under {prefix}", zip.display()));
    }
    Ok(())
}

/// Runs the Vanilla dedicated server in `run_dir` (libraries from `work_dir`), sends
/// `commands` once it is up (`place template` again while its area is not loaded yet), saves
/// and stops it.
fn vanilla_session(
    work_dir: &Path,
    run_dir: &Path,
    upgrade: bool,
    commands: &[String],
    log: &Path,
) -> Result<()> {
    let mut cmd = Command::new(java_tool("java"));
    cmd.current_dir(run_dir)
        .arg(format!("-DbundlerRepoDir={}", work_dir.display()))
        .arg("-Xmx3G")
        .arg("-jar")
        .arg(work_dir.join("server.jar"))
        .arg("--nogui");
    if upgrade {
        cmd.arg("--forceUpgrade");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("java: {e}"))?;
    let (tx, rx) = mpsc::channel::<String>();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stream)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let mut stdin = child.stdin.take().ok_or("no server stdin")?;
    let mut log_file = std::fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    let mut wait_for = |what: &[&str], limit: Duration| -> Result<String> {
        let until = (Instant::now() + limit).min(deadline);
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    let _ = writeln!(log_file, "{line}");
                    if let Some(w) = what.iter().find(|w| line.contains(**w)) {
                        return Ok((*w).to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(format!("the Vanilla server did not say {what:?} in time"));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "the Vanilla server ended before {what:?}; see {}",
                        log.display()
                    ));
                }
            }
        }
    };
    let result = (|| -> Result<()> {
        wait_for(&["Done ("], Duration::from_secs(25 * 60))?;
        let mut send = |line: &str| -> Result<()> {
            writeln!(stdin, "{line}").map_err(|e| format!("server stdin: {e}"))
        };
        for c in commands {
            if c.starts_with("place ") {
                let mut tries = 0;
                loop {
                    send(c)?;
                    let answer = wait_for(
                        &[
                            "Loaded template",
                            "not loaded",
                            "Unknown",
                            "Failed",
                            "no template",
                        ],
                        // The largest schematics (p16_cpu) take minutes to place.
                        Duration::from_secs(600),
                    )?;
                    if answer == "Loaded template" {
                        break;
                    }
                    tries += 1;
                    if answer != "not loaded" || tries > 60 {
                        return Err(format!("`{c}`: {answer}; see {}", log.display()));
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            } else {
                send(c)?;
                if c.starts_with("forceload") {
                    wait_for(&["force loaded", "No chunks"], Duration::from_secs(60))?;
                }
            }
        }
        send("save-all flush")?;
        wait_for(&["Saved the game"], Duration::from_secs(600))?;
        send("stop")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| format!("java: {e}"))?;
    // The rest of the log.
    while let Ok(line) = rx.try_recv() {
        let _ = writeln!(log_file, "{line}");
    }
    result?;
    if !status.success() {
        return Err(format!(
            "the Vanilla server failed ({status}); see {}",
            log.display()
        ));
    }
    Ok(())
}

/// The soak scenario of a prepared world. Schematic positions in actions and entities are
/// relative to the pasted corner.
fn scenario(w: &World, p: &Prepared, ticks: Option<u32>) -> Value {
    let region = match (w.region, p.pasted) {
        (Some(r), _) => r,
        (None, Some([min, max])) => [
            [
                min[0] - REGION_MARGIN,
                (min[1] - REGION_MARGIN).max(-64),
                min[2] - REGION_MARGIN,
            ],
            [
                max[0] + REGION_MARGIN,
                (max[1] + REGION_MARGIN).min(319),
                max[2] + REGION_MARGIN,
            ],
        ],
        (None, None) => {
            let f = p.focus.unwrap_or([0, 64, 0]);
            [
                [f[0] - 24, (f[1] - 8).max(-64), f[2] - 24],
                [f[0] + 24, (f[1] + 16).min(319), f[2] + 24],
            ]
        }
    };
    let origin = p.pasted.map_or([0, 0, 0], |b| b[0]);
    let shift = |v: &Value| -> Value {
        let mut v = v.clone();
        if p.pasted.is_some() {
            for k in ["pos", "from", "to"] {
                if let Some(a) = v.get_mut(k).and_then(Value::as_array_mut) {
                    for (c, o) in a.iter_mut().zip(origin) {
                        if let Some(i) = c.as_i64() {
                            *c = json!(i + i64::from(o));
                        } else if let Some(f) = c.as_f64() {
                            *c = json!(f + f64::from(o));
                        }
                    }
                }
            }
        }
        v
    };
    // A real server's random ticks. Natural spawning stays off unless a world turns it on: the
    // scenario world without dimensions has no custom spawners (phantoms, cats, patrols).
    let mut rules: serde_json::Map<String, Value> = [("random_tick_speed", "3")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), json!(v)))
        .collect();
    rules.extend(w.rules.iter().map(|(k, v)| {
        let text = match v {
            toml::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        (k.clone(), Value::String(text))
    }));
    let mut s = json!({
        "description": format!("{} ({}, parity world corpus #244: {})", w.contents, w.group, w.source),
        "tier": "bulk",
        "world": {
            "seed": w.seed,
            "snapshot": p.snapshot.to_string_lossy(),
            "entities": w.snapshot_entities,
        },
        "rules": rules,
        "region": region,
        "ticks": ticks.or(w.ticks).unwrap_or(TICKS),
        "block_entities": true,
    });
    // The scripted player (survival; the server moves it like Vanilla's mock player): by default
    // at a world's saved player position, or on a barrier block next to a schematic's minimum
    // corner, out of the observed region but in its forced chunks.
    let mut platform = None;
    let player = match w.player {
        Some(Some(at)) => Some(shift(&json!({"pos": at}))["pos"].clone()),
        Some(None) => None,
        None => Some(match (p.pasted, p.focus) {
            (Some([min, _]), _) => {
                let at = [
                    min[0] - REGION_MARGIN - 2,
                    min[1],
                    min[2] - REGION_MARGIN - 2,
                ];
                platform = Some(at);
                json!([
                    f64::from(at[0]) + 0.5,
                    f64::from(at[1] + 1),
                    f64::from(at[2]) + 0.5
                ])
            }
            (None, f) => {
                let f = f.unwrap_or([0, 64, 0]);
                json!([
                    f64::from(f[0]) + 0.5,
                    f64::from(f[1]),
                    f64::from(f[2]) + 0.5
                ])
            }
        }),
    };
    if let Some(at) = platform {
        s["setup"] = json!([{"op": "set", "pos": at, "block": "minecraft:barrier"}]);
    }
    let mut entities: Vec<Value> = w.entities.iter().map(shift).collect();
    let has_player = entities.iter().any(|e| e["type"] == "minecraft:player");
    let player_name = entities
        .iter()
        .find(|e| e["type"] == "minecraft:player")
        .and_then(|e| e["name"].as_str())
        .map_or_else(|| "player".to_owned(), str::to_owned);
    let scripted = player.is_some() && !has_player;
    if let (Some(pos), false) = (player, has_player) {
        entities.insert(
            0,
            json!({"name": "player", "type": "minecraft:player", "pos": pos}),
        );
    }
    let has_player = entities.iter().any(|e| e["type"] == "minecraft:player");
    if !w.actions.is_empty() {
        s["actions"] = Value::Array(w.actions.iter().map(shift).collect());
    } else if w.trigger {
        // The default trigger: every lever flips (`place`, with updates), the player presses
        // every button.
        let mut actions: Vec<Value> = p
            .levers
            .iter()
            .map(|(pos, state)| {
                json!({"tick": w.trigger_tick, "op": "place", "pos": pos, "block": state})
            })
            .collect();
        if has_player {
            actions.extend(p.buttons.iter().map(|pos| {
                json!({"tick": w.trigger_tick, "op": "use", "entity": player_name, "pos": pos})
            }));
        }
        if !actions.is_empty() {
            s["actions"] = Value::Array(actions);
        }
    }
    if scripted {
        // The scripted player survives what the machine does to it (a dead player would leave
        // the recorder's mock connection): resistance V and fire resistance from the start.
        let duration = s["ticks"].as_u64().unwrap_or(u64::from(TICKS)) + 100;
        let mut actions: Vec<Value> = ["minecraft:resistance", "minecraft:fire_resistance"]
            .into_iter()
            .map(|effect| {
                json!({"tick": 1, "op": "effect", "entity": "player", "effect": effect,
                       "duration": duration, "amplifier": 4})
            })
            .collect();
        if let Some(Value::Array(rest)) = s.get_mut("actions").map(Value::take) {
            actions.extend(rest);
        }
        s["actions"] = Value::Array(actions);
    }
    if !entities.is_empty() {
        s["entities"] = Value::Array(entities);
    }
    if let Some(d) = &w.difficulty {
        s["difficulty"] = json!(d);
    }
    s
}
