//! Runs scenarios and compares their traces with the goldens.
//!
//! [`check`] runs a scenario twice, with the world's parallel phases run serially and in
//! parallel (verify mode): both traces must be identical. Then the trace must equal the golden
//! exactly: the blessed golden if the scenario has one, else the Vanilla recording.
//!
//! `VANILLITE_BLESS=<scenario>,...` (or `all`; set by `cargo xtask golden bless`) writes the
//! traces of those scenarios as their blessed goldens instead of comparing them.
//! `VANILLITE_SCENARIO=<scenario>,...` runs only those scenarios.
//! `VANILLITE_SCENARIO_SHARD=<k>/<n>` runs only part k of n (CI jobs side by side): the
//! scenarios in name order, every n-th from the k-th on, so the parts together run all of them.
//!
//! [`check_quick`] is the quick tier (`cargo xtask parity quick`, part of the quick check): the
//! fast scenarios without the slow ones listed in `scenarios/quick-skip.txt`.

use std::path::PathBuf;

use crate::golden::{self, Keys};
use crate::scenario::{OpKind, Scenario, Tier};
use crate::trace::{Frame, Trace};
use crate::world::{Execution, ScenarioWorld, WorldFactory, parse_state, state_name};

/// Differences listed per failing scenario.
const DIFF_LIMIT: usize = 30;

/// Runs `scenario` in a world from `factory` and records its trace.
pub fn record(
    scenario: &Scenario,
    factory: WorldFactory,
    execution: Execution,
) -> Result<Trace, String> {
    let mut frames = Vec::new();
    let mut trace = stream(scenario, factory, execution, &mut |_, frame| {
        frames.push(frame);
        Ok(true)
    })?;
    trace.frames = frames;
    Ok(trace)
}

/// Runs `scenario` in a world from `factory` and hands every frame to `sink` as it is
/// recorded, with the trace so far (its palette; it keeps no frames); the run stops early
/// when `sink` returns `false`. Returns the trace without frames.
pub fn stream(
    scenario: &Scenario,
    factory: WorldFactory,
    execution: Execution,
    sink: &mut dyn FnMut(&Trace, Frame) -> Result<bool, String>,
) -> Result<Trace, String> {
    let mut world = factory(&scenario.world, execution);
    for op in &scenario.setup {
        apply_in(world.as_mut(), scenario, op, false, &mut Vec::new())?;
    }
    world.settle_light();
    world.add_entities(&scenario.entities, scenario.entity_seed)?;
    world.settle_light();
    let mut trace = Trace::new(scenario.min, scenario.max);
    // Palette index per block state, filled as states show up.
    let mut cache = vec![u32::MAX; 1 << 16];
    let frame = observe(world.as_mut(), scenario, &mut trace, &mut cache, 0)?;
    // Frames without block changes share the blocks of the frame before (see `Frame::blocks`).
    let mut last = std::sync::Arc::clone(&frame.blocks);
    if !sink(&trace, frame)? {
        return Ok(trace);
    }
    for tick in 1..=scenario.ticks {
        for (i, spec) in scenario.entities.iter().enumerate() {
            if spec.join == Some(tick) {
                world.join(spec, i)?;
            }
        }
        world.client_begin()?;
        let mut acted = false;
        let mut feedback = Vec::new();
        for op in scenario.actions.iter().filter(|a| a.tick == tick) {
            apply_in(world.as_mut(), scenario, op, true, &mut feedback)?;
            acted = true;
        }
        if acted {
            world.settle_light();
        }
        world.client_end()?;
        world.step();
        let mut frame = observe(world.as_mut(), scenario, &mut trace, &mut cache, tick)?;
        if scenario.world.feedback {
            frame.feedback = feedback;
        }
        if frame.blocks == last {
            frame.blocks = std::sync::Arc::clone(&last);
        } else {
            last = std::sync::Arc::clone(&frame.blocks);
        }
        if !sink(&trace, frame)? {
            break;
        }
    }
    Ok(trace)
}

/// [`apply`] in the op's dimension (default the overworld); the region's dimension stays
/// selected. The messages of commands go to `feedback`.
fn apply_in(
    world: &mut dyn ScenarioWorld,
    scenario: &Scenario,
    op: &crate::scenario::Op,
    action: bool,
    feedback: &mut Vec<String>,
) -> Result<(), String> {
    let dimension = op
        .dimension
        .as_deref()
        .unwrap_or(crate::scenario::OVERWORLD);
    if dimension == scenario.world.dimension {
        return apply(world, op, action, feedback);
    }
    world.select_dimension(dimension)?;
    let applied = apply(world, op, action, feedback);
    world.select_dimension(&scenario.world.dimension)?;
    applied
}

fn apply(
    world: &mut dyn ScenarioWorld,
    op: &crate::scenario::Op,
    action: bool,
    feedback: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(hurt) = &op.hurt {
        return world.hurt(hurt);
    }
    if let Some(effect) = &op.effect {
        return world.effect(effect);
    }
    if let Some(use_item) = &op.use_item {
        return world.use_item(use_item);
    }
    if let Some(command) = &op.command {
        feedback.extend(world.command(command)?);
        return Ok(());
    }
    if let Some(potion) = &op.throw_potion {
        return world.throw_potion(potion);
    }
    if let Some(click) = &op.player_use {
        return world.player_use(click);
    }
    if let Some(packet) = &op.packet {
        return world.packet(packet);
    }
    let updates = match op.kind {
        OpKind::Set => false,
        OpKind::Place => true,
        OpKind::Fill => action,
        OpKind::Hurt => return Err("hurt without its damage".to_owned()),
        OpKind::Lightning => return world.lightning(op.from),
        OpKind::Effect => return Err("effect without its effect".to_owned()),
        OpKind::UseItem | OpKind::Interact => return Err("use_item without its item".to_owned()),
        OpKind::Command => return Err("command without its line".to_owned()),
        OpKind::ThrowPotion => return Err("throw_potion without its potion".to_owned()),
        OpKind::Use | OpKind::Attack => {
            return Err("use without its click".to_owned());
        }
        OpKind::Packet => return Err("packet without its packet".to_owned()),
        OpKind::Force | OpKind::Unforce => {
            return world.set_forced(&op.chunks, op.kind == OpKind::Force);
        }
        OpKind::Restart => return world.restart(),
        OpKind::Bonemeal => return world.bonemeal(op.from),
    };
    let state = parse_state(&op.block)?;
    let (a, b) = (op.from, op.to);
    // `BlockPos.betweenClosed` order: x fastest, then y, then z.
    for z in a.z.min(b.z)..=a.z.max(b.z) {
        for y in a.y.min(b.y)..=a.y.max(b.y) {
            for x in a.x.min(b.x)..=a.x.max(b.x) {
                let pos = vanillite_base::BlockPos::new(x, y, z);
                if updates {
                    world.place_block(pos, state);
                } else {
                    world.set_block_raw(pos, state);
                }
            }
        }
    }
    Ok(())
}

fn observe(
    world: &mut dyn ScenarioWorld,
    scenario: &Scenario,
    trace: &mut Trace,
    cache: &mut [u32],
    tick: u32,
) -> Result<Frame, String> {
    let light = scenario.world.observe_light;
    let volume: usize = trace.size().iter().product();
    let mut blocks = Vec::with_capacity(volume);
    for i in 0..volume {
        let pos = trace.pos_of(i);
        let index = if world.is_loaded(pos) {
            let state = world.block(pos);
            let slot = &mut cache[usize::from(state.0)];
            if *slot == u32::MAX {
                *slot = trace.intern(&state_name(state));
            }
            *slot
        } else {
            trace.intern(crate::scenario::UNLOADED)
        };
        blocks.push(index);
    }
    let mut frame = Frame {
        tick,
        blocks: std::sync::Arc::new(blocks),
        ..Frame::default()
    };
    world.block_ticks(trace.min, trace.max, &mut frame.block_ticks);
    world.fluid_ticks(trace.min, trace.max, &mut frame.fluid_ticks);
    world.entities(trace.min, trace.max, &mut frame.entities);
    if light {
        world.light(trace.min, trace.max, &mut frame.light);
    }
    if scenario.world.block_entities {
        world.block_entities(trace.min, trace.max, &mut frame.block_entities);
        frame.block_entities.sort_unstable();
    }
    frame.level_rng = world.level_rng();
    if scenario.world.client_view {
        frame.view = world.client_view()?;
    }
    for view in &scenario.world.views {
        world.select_dimension(&view.dimension)?;
        world.entities(view.min, view.max, &mut frame.entities);
    }
    if !scenario.world.views.is_empty() {
        world.select_dimension(&scenario.world.dimension)?;
    }
    Ok(frame)
}

/// Path of the Vanilla golden of a scenario (repository or bulk tier).
pub fn golden_path(scenario: &Scenario) -> PathBuf {
    match scenario.tier {
        Tier::Fast => golden::crate_dir()
            .join("goldens/scenarios")
            .join(format!("{}.json", scenario.name)),
        Tier::Bulk => golden::bulk_dir().join("scenarios").join(format!(
            "{}-{}.json",
            scenario.name,
            &scenario.sha1[..12]
        )),
    }
}

/// Path of the blessed golden of a scenario.
pub fn blessed_path(scenario: &Scenario) -> PathBuf {
    golden::crate_dir()
        .join("goldens/blessed")
        .join(format!("{}.json", scenario.name))
}

fn bless_requested(name: &str) -> bool {
    std::env::var("VANILLITE_BLESS")
        .is_ok_and(|v| v.split(',').any(|n| n.trim() == name || n.trim() == "all"))
}

/// The expected trace of a scenario: blessed, else Vanilla's, after checking its key.
fn expected(scenario: &Scenario) -> Result<Trace, String> {
    let blessed = blessed_path(scenario);
    if blessed.exists() {
        let trace = read_trace(&blessed)?;
        if trace.blessed_for.as_deref() != Some(scenario.sha1.as_str()) {
            return Err(format!(
                "{} was blessed for another version of the scenario; bless it again with \
                 `cargo xtask golden bless {}` or delete it",
                blessed.display(),
                scenario.name
            ));
        }
        return Ok(trace);
    }
    if scenario.world.profile != "vanilla" {
        return Err(format!(
            "profile `{}` cannot be recorded on Vanilla: bless the scenario with \
             `cargo xtask golden bless {}`",
            scenario.world.profile, scenario.name
        ));
    }
    let path = golden_path(scenario);
    match scenario.tier {
        Tier::Fast => Keys::check(
            path.parent().expect("golden directory"),
            &scenario.name,
            &scenario.sha1,
        )?,
        Tier::Bulk if !path.exists() => {
            return Err(format!(
                "bulk golden {} is missing; run `cargo xtask golden regen --bulk` (needs Java 25)",
                path.display()
            ));
        }
        Tier::Bulk => {}
    }
    read_trace(&path)
}

fn read_trace(path: &std::path::Path) -> Result<Trace, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Trace::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Runs a scenario in verify mode and compares it with its golden (or blesses it).
pub fn check(scenario: &Scenario, factory: WorldFactory) -> Result<(), String> {
    let serial = record(scenario, factory, Execution::Serial)?;
    let parallel = record(scenario, factory, Execution::Parallel)?;
    let diff = serial.diff(&parallel, DIFF_LIMIT);
    if !diff.is_empty() {
        return Err(format!(
            "verify mode: the parallel run differs from the serial run:\n  {}",
            diff.join("\n  ")
        ));
    }
    if bless_requested(&scenario.name) {
        let path = blessed_path(scenario);
        let mut trace = serial;
        trace.blessed_for = Some(scenario.sha1.clone());
        std::fs::create_dir_all(path.parent().expect("blessed directory"))
            .map_err(|e| e.to_string())?;
        std::fs::write(&path, trace.to_json()).map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("blessed {}", path.display());
        return Ok(());
    }
    let mut expected = expected(scenario)?;
    let mut serial = serial;
    if scenario.world.client_view && !scenario.world.view_events {
        // Level and entity events only count where the scenario asks for them.
        for trace in [&mut expected, &mut serial] {
            for frame in &mut trace.frames {
                frame
                    .view
                    .retain(|e| !e.contains(" level_event ") && !e.contains(" entity_event "));
            }
        }
    }
    let first = expected.first_divergence(&serial);
    let known = scenario.diverges.as_ref();
    if first == known.map(|d| d.tick) {
        return Ok(());
    }
    let diff = expected.diff(&serial, DIFF_LIMIT);
    match (known, first) {
        (Some(d), None) => Err(format!(
            "matches the golden now: remove scenarios/divergences/{}.toml (tick {}: {})",
            scenario.name, d.tick, d.cause
        )),
        (Some(d), Some(t)) => Err(format!(
            "the first divergence moved from tick {} to tick {t} ({}; {}, owner {}): {} the \
             tick in scenarios/divergences/{}.toml if intended:\n  {}",
            d.tick,
            scenario.description,
            d.cause,
            d.owner,
            if t > d.tick {
                "progress, raise"
            } else {
                "regression, or lower"
            },
            scenario.name,
            diff.join("\n  ")
        )),
        _ => Err(format!(
            "differs from the golden ({}):\n  {}",
            scenario.description,
            diff.join("\n  ")
        )),
    }
}

/// Parses a `VANILLITE_SCENARIO_SHARD` value `<k>/<n>` into (k, n).
fn shard(v: &str) -> Option<(usize, usize)> {
    let (k, n) = v.trim().split_once('/')?;
    let (k, n) = (k.trim().parse().ok()?, n.trim().parse().ok()?);
    (1 <= k && k <= n).then_some((k, n))
}

/// Checks every scenario of a tier; returns one message per failing scenario.
pub fn check_tier(tier: Tier, factory: WorldFactory) -> Vec<String> {
    check_where(tier, factory, &[])
}

/// Checks the quick tier: the fast scenarios except those in `scenarios/quick-skip.txt` (one
/// name per line, `#` comments). The full fast tier still runs them all.
pub fn check_quick(factory: WorldFactory) -> Vec<String> {
    let path = Scenario::dir().join("quick-skip.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return vec![format!("{}: {e}", path.display())],
    };
    let skip: Vec<&str> = text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .collect();
    let stale: Vec<&str> = skip
        .iter()
        .copied()
        .filter(|n| !Scenario::dir().join(format!("{n}.json")).is_file())
        .collect();
    if !stale.is_empty() {
        return vec![format!(
            "{}: no such scenarios (remove them): {}",
            path.display(),
            stale.join(", ")
        )];
    }
    check_where(Tier::Fast, factory, &skip)
}

fn check_where(tier: Tier, factory: WorldFactory, skip: &[&str]) -> Vec<String> {
    let scenarios = match Scenario::all() {
        Ok(s) => s,
        Err(e) => return vec![e],
    };
    let only = std::env::var("VANILLITE_SCENARIO").ok();
    let (part, parts) = match std::env::var("VANILLITE_SCENARIO_SHARD") {
        Ok(v) => match shard(&v) {
            Some(s) => s,
            None => {
                return vec![format!(
                    "VANILLITE_SCENARIO_SHARD={v}: expected <k>/<n>, 1 <= k <= n"
                )];
            }
        },
        Err(_) => (1, 1),
    };
    let todo: Vec<&Scenario> = scenarios
        .iter()
        .filter(|s| s.tier == tier && !skip.contains(&s.name.as_str()))
        .filter(|s| {
            only.as_deref()
                .is_none_or(|o| o.split(',').any(|n| n.trim() == s.name))
        })
        .enumerate()
        .filter(|(i, _)| i % parts == part - 1)
        .map(|(_, s)| s)
        .collect();
    // Scenarios with a data pack change the installed game data (`install_game_data`, e.g.
    // enchantment registry ids) that every server of the process reads: they run alone,
    // after the others.
    let (alone, todo): (Vec<&Scenario>, Vec<&Scenario>) =
        todo.into_iter().partition(|s| s.world.datapack.is_some());
    // Scenarios are independent: run them on all cores, report in name order.
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut results: Vec<(usize, String)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads.min(todo.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut failed = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(s) = todo.get(i) else {
                            return failed;
                        };
                        if let Err(e) = check(s, factory) {
                            failed.push((i, format!("{}: {e}", s.name)));
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    for (i, s) in alone.iter().enumerate() {
        if let Err(e) = check(s, factory) {
            results.push((todo.len().saturating_add(i), format!("{}: {e}", s.name)));
        }
    }
    results.sort_by_key(|r| r.0);
    results.into_iter().map(|r| r.1).collect()
}
