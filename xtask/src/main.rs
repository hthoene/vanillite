//! Repository tasks: Vanilla data update, code generation, golden tests and releases.
//!
//! ```text
//! cargo xtask vanilla update [<version>] [--jar <server.jar>]
//! cargo xtask vanilla export <dir>
//! cargo xtask version check
//! cargo xtask version update <version> [--final] [--bulk] [--no-test]
//! cargo xtask multiversion add <version>
//! cargo xtask codegen [--check]
//! cargo xtask golden check
//! cargo xtask golden regen [--all] [--bulk] [<name>...]
//! cargo xtask golden bless <scenario>...
//! cargo xtask parity fuzz [--seed N] [--count N] [--minutes N] [--adopt]
//! cargo xtask parity replay <recording dir | port:name | port:latest>... [--ticks N]
//! cargo xtask parity soak [--jobs N] [--minutes N] [<scenario>...]
//! cargo xtask parity quick
//! cargo xtask parity worlds [--ticks N] [--jobs N] [--minutes N] [--prepare-only] [<world>...]
//! cargo xtask parity coverage [--jobs N] [--heap SIZE] [--fuzz-minutes N] [--report-only] [--history FILE] [<scenario>...]
//! cargo xtask parity coverage --trend [FILE]
//! cargo xtask worldgen sweep [--seed N] [--seeds N] [--world-seed N]... [--size N] [--kinds a,b] [--keep]
//! cargo xtask light sweep [--seed N] [--seeds N] [--world-seed N]... [--size N] [--kinds a,b] [--keep]
//! cargo xtask dev [--release] [-- <server args>]
//! cargo xtask deploy [--host <host>] [--port <port>] [--release]
//! cargo xtask testserver [--host <host>] provision|status|logs|start|stop|restart [<port>]
//! cargo xtask testserver [--host <host>] run [--fetch <path>]... -- <xtask args>
//! cargo xtask testserver [--host <host>] nightly [status | fetch [<run>]]
//! cargo xtask testserver incidents [list [<count>] | show <id> | fetch [<id>...]]
//! cargo xtask docker [--push] [--in-docker] [--tag <tag>]...
//! cargo xtask egg [--check] | egg smoke [--image <ref>]
//! cargo xtask config-docs [--check]
//! cargo xtask loadtest [--server vanillite|paper|both] [--steps <n,...>] [--hold <s>] ...
//! cargo xtask loadtest weak [--minutes <n>] [--binary <file>] [--rule <key>=<value>]...
//! cargo xtask bench all|<scenario>... [--servers <a,b>] [--publish] [--no-deploy]
//! cargo xtask bench list | report | publish <time|latest|results.json> | nightly [--no-push]
//! cargo xtask bench commands <scenario>
//! cargo xtask setup [--global] | regen
//! ```

mod bench;
mod configdoc;
mod coverage;
mod data;
mod devenv;
mod docker;
mod egg;
mod golden;
mod loadtest;
mod merge;
mod parity;
mod parity_worlds;
mod version;
mod weak;
mod worldgen;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vanillite_datagen::{cache, codegen, keep, multiversion, normalize, vanilla};

/// Errors are reported as text; xtask is a developer tool.
pub type Result<T> = std::result::Result<T, String>;

pub const USAGE: &str = "usage:
  cargo xtask vanilla update [<version>] [--jar <server.jar>]
      Download (or take) the server jar, verify its SHA-1, run Mojang's data generator and
      tools/extractor, normalise into the data cache, pin the version in
      data/vanilla/version.toml and print a change report. <version> defaults to the latest
      release. Uses a JDK of the version the jar asks for (VANILLITE_JAVA_HOME, JAVA_HOME,
      java/javac on PATH, else Temurin downloaded into the cache).
  cargo xtask vanilla export <dir>
      Copy the data of all pinned versions (extracted first if needed) to <dir> in the layout
      VANILLITE_DATA_DIR expects (vanilla/, multiversion/<id>/): offline builds, build
      machines without network or Java.
  cargo xtask version check
      Compare the pinned version (data/vanilla/version.toml) with the latest release in Mojang's
      version manifest; fails if a newer release exists (in GitHub Actions also sets the step
      output `newer`).
  cargo xtask version update <version> [--final] [--bulk] [--no-test]
      Prepare the switch to another version (release, pre-release or snapshot): extract the
      pinned and the new jar, store the new data in the data cache and pin it, record all
      repository goldens (--bulk also the bulk goldens and runs the ignored tests) and run the
      tests, all in the worktree target/version/<version>/tree, and write the change report
      target/version/<version>/change-report.md (protocol, packet fields, registries, blocks,
      items, entities, data pack, goldens, failing tests, affected code areas). --final does it
      in this checkout instead (only for releases): the real switch. --no-test skips the tests.
  cargo xtask multiversion add <version>...
      Extract older versions (releases or snapshots, 1.20.5 and later; obfuscated jars are
      remapped with Mojang's mappings) and keep what the multi-version layer needs to translate
      between each and its successor (packets, packet shapes, registries, blocks, entity types,
      synchronised registry entries) in the data cache, and pin each in
      data/multiversion/<version>/version.toml. `version update --final` pins the outgoing
      version the same way.
  cargo xtask codegen [--check]
      Write the tables the build generates (vanillite-data from the current version, the
      adapter tables of vanillite-multiversion from the kept versions) to target/generated/
      for reading; --check only generates them. The build itself generates them into OUT_DIR.
  cargo xtask golden check
      Fail if a repository golden (worldgen, data, fast scenarios) is missing or stale: its key
      (Minecraft version, jar SHA-1, tools/extractor/harness-version, source or scenario hash)
      differs from the current one. No Java needed.
  cargo xtask golden regen [--all] [--bulk] [<name>...]
      Record the missing and stale goldens with the Java harness on the pinned jar (downloaded
      and unpacked if needed; needs a JDK 25). --all records every repository golden, --bulk
      also the bulk scenario goldens into $VANILLITE_GOLDENS (default target/goldens); names
      (part like ChunkGoldens or chunk, or scenario) record just those.
  cargo xtask parity fuzz [--seed N] [--count N] [--minutes N] [--jobs N] [--adopt] [--no-coverage]
      Random parity scenarios (#132): generate scenarios with a packet-level player from
      consecutive seeds (default: 20 from seed 1), record them in Vanilla and run them in
      Vanillite, shrink every divergence to a small scenario. Writes target/parity-fuzz/found/
      and summary.md. --adopt copies the found scenarios into crates/vanillite-test/scenarios/
      and records their goldens (then add scenarios/divergences/<name>.toml with the cause).
      Also: --replay <scenario.json> runs given scenarios without generating. With a
      target/parity-coverage/coverage.json (parity coverage) the generator picks behaviours,
      mobs, items and blocks whose Vanilla code is least exercised more often (--no-coverage:
      not).
  cargo xtask parity replay <recording dir | port:name | port:latest>... [--ticks N] [--jobs N]
      Replay recorded server sessions (debug.session-recording, #152) in Vanilla and
      Vanillite on its world snapshot and compare them tick by tick; a divergence is shrunk
      like a random one. `30001:latest` (or `30001:<name>`) fetches a recording of the test
      server into target/recordings/ first. --ticks replays only the first N ticks. Writes
      target/parity-fuzz/ (sessions/<name>.json, found/, summary.md with one line per
      session and the distinct divergences).
  cargo xtask parity soak [--jobs N] [--minutes N] [<scenario>...]
      Soak parity runs (#166): the long scenarios of crates/vanillite-test/scenarios/soak/
      (game days with random ticks, mob spawning, day cycle and weather) against Vanilla.
      Vanilla's per-tick checksums are recorded once per golden key into $VANILLITE_GOLDENS/
      <key>/soak/; Vanillite stops at the first differing tick, Vanilla is re-run to that
      tick and the full diff goes to target/parity-soak/<scenario>.md (summary.md lists all).
      Runs nightly on vanillite-test2 (scripts/testserver/nightly.sh).
  cargo xtask parity quick
      The quick tier of the parity scenarios (#257, part of scripts/quick-check.sh): the fast
      scenarios without the slow ones of crates/vanillite-test/scenarios/quick-skip.txt against
      their goldens, serial and parallel, on all cores (about a minute). The full fast tier
      runs on every land.
  cargo xtask parity worlds [--ticks N] [--jobs N] [--minutes N] [--prepare-only] [<world>...]
      Complex real-world worlds and schematics (redstone, farms, machines; #244) against
      Vanilla: the corpus crates/vanillite-test/parity-worlds.toml is downloaded into
      ~/.cache/vanillite/parity-worlds/ (SHA-256 checked), upgraded or pasted by the pinned
      Vanilla server, then run like soak scenarios (blocks, ticks, entities and block entities
      per tick). Writes target/parity-worlds/summary.md and <world>.md. Runs nightly.
  cargo xtask parity coverage [--jobs N] [--heap SIZE] [--fuzz-minutes N] [--report-only]
                              [--history FILE] [<scenario>...]
  cargo xtask parity coverage --trend [FILE]
      Which Vanilla code the parity scenarios exercise (#257): the recorder plays every
      Vanilla-recorded scenario (fast and bulk) on the pinned jar under the JaCoCo agent
      (downloaded into target/vanilla/jacoco/), on --jobs JVMs; --fuzz-minutes adds a parity
      fuzz run under the agent. Writes target/parity-coverage/ (never committed, #213):
      summary.md (gameplay coverage, per system, the 30 biggest uncovered classes and
      packages), coverage.json (also per class, read by parity fuzz) and html/ (per package,
      class and method). --report-only re-reads the coverage data of the last run; scenario
      names record just those. --heap sets each recorder JVM's -Xmx (default 4g). --history
      appends the run's gameplay and per-system figures (instructions/branches %) as one JSON
      line to FILE (the nightly: /var/lib/vanillite-nightly/coverage-history.jsonl); --trend
      prints that history as Markdown (default FILE target/parity-coverage/history.jsonl).
  cargo xtask worldgen sweep [--seed N] [--seeds N] [--world-seed N]... [--size N] [--kinds a,b] [--keep]
      World generation against Vanilla (#223): for --seeds world seeds (default 3, derived from
      --seed, default the day number; or the given --world-seed values) and every kind
      (overworld, nether, end, amplified, large_biomes, flat; --kinds picks some), Vanilla's
      code generates the size×size chunks around the origin (default 16) and a far area of half
      the size in-process (tools/extractor/.../golden/WorldSweep.java), then the ignored test
      vanillite-worldgen/tests/vanilla_sweep.rs generates them with Vanillite and compares
      blocks, biomes, heightmaps, ticks, structure starts and references and block entities.
      Writes target/worldgen-sweep/summary.md (--keep keeps the Vanilla areas in vanilla/).
      Runs nightly on vanillite-test2.
  cargo xtask light sweep [--seed N] [--seeds N] [--world-seed N]... [--size N] [--kinds a,b] [--keep]
      Light against Vanilla (#237), with the seeds, kinds and areas of the worldgen sweep
      (origin area default 6×6 chunks): Vanilla generates and lights each area in-process and
      replays a script of block changes seeded from it (tools/extractor/.../golden/
      LightSweep.java), then the ignored test vanillite-light/tests/vanilla_light_sweep.rs
      lights the same blocks with Vanillite, replays the changes and compares the light a
      client receives after generation and after every batch. Writes
      target/light-sweep/summary.md. Runs nightly on vanillite-test2.
  cargo xtask golden bless <scenario>...|all
      Run the scenarios in Vanillite and store their traces as the expected goldens in
      crates/vanillite-test/goldens/blessed/ (intended differences, non-vanilla profiles).
  cargo xtask dev [--release] [-- <server args>]
      Build and run the server in ./run (debug build; --release uses the release-fast profile).
  cargo xtask deploy [--host <host>] [--port <port>] [--release]
      Build the server (release-fast; --release for the full release profile) and install it on
      the test server as vanillite@<port>.service. <port> is 30003-30999, default 30003
      (30001, 30002 and the proxy follow dev automatically).
      The SSH host is --host, else VANILLITE_TEST_HOST, else vanillite-test (vanillite-test2:
      the internal server for experiments, offline mode).
  cargo xtask testserver [--host <host>] provision|status|logs|start|stop|restart [<port>]
      provision runs scripts/testserver/provision.sh on the test server (idempotent; on
      vanillite-test2 scripts/testserver/provision-test2.sh; on vanillite-bench, the benchmark
      server, provision.sh with the role bench: benchmark instances and hardening only); status
      lists the services, ports,
      the last auto-deploys and incidents (vanillite-test2: lock, disk, last nightly run); the
      others act on vanillite@<port> (default 30001). Host as for deploy.
  cargo xtask testserver [--host <host>] run [--fetch <path>]... -- <xtask args>
      Heavy tests and experiments on vanillite-test2 (default host): wait for its lock, copy
      this checkout (without target/ and .git/) to /opt/vanillite-work/src there, run
      `cargo xtask <xtask args>` in it, then copy the --fetch paths (relative to the checkout,
      e.g. target/parity-fuzz) back. Example: testserver run --fetch target/parity-fuzz --
      parity fuzz --minutes 30.
  cargo xtask testserver [--host <host>] nightly [status | fetch [<run>]]
      The nightly heavy tests on vanillite-test2 (scripts/testserver/nightly.sh, 00:30
      Europe/Berlin): summaries of the newest runs, or download one run (default the newest)
      into target/nightly/.
  cargo xtask testserver incidents [list [<count>] | show <id> | fetch [<id>...]]
      Crashes and suspicious disconnects that vanillite-incidents saved on the test server:
      list the newest (default 20), show one (summary and backtrace), or download bundles into
      target/incidents/<id>/ (without ids: all not downloaded yet).
  cargo xtask docker [--push] [--in-docker] [--tag <tag>]...
      Build the server image (Dockerfile) for linux/amd64 and linux/arm64, tagged dev and the
      short commit hash, as $VANILLITE_IMAGE (default docker.hthoene.de/vanillite/vanillite).
      The static binaries are cross-compiled here with cargo-zigbuild (--in-docker builds them
      in the image instead). Builds on $DOCKER_HOST, the local engine, or else the test server
      over SSH. --push logs in as $VANILLITE_REGISTRY_USER (default deploy) with the password
      from $PRIVATE_DOCKER_REPO_KEY and pushes; without it the image is loaded into the engine.
  cargo xtask egg [--check]
      Generate the Pterodactyl eggs deploy/pterodactyl/egg-vanillite.json (every setting) and
      egg-vanillite-simple.json (basic settings only) from the configuration schema
      (server.properties keys and vanillite.toml rules) and check them against the egg format,
      and the configuration reference (config-docs). --check fails if a committed file is not
      current or the eggs differ in what they share.
  cargo xtask config-docs [--check]
      Generate the configuration reference docs/configuration.md from the configuration schema
      (server.properties keys and vanillite.toml rules with their value in every profile).
      --check fails if the committed file is not current.
  cargo xtask egg smoke [--image <ref>]
      Start the image (default $VANILLITE_IMAGE:dev, see docker) like Pterodactyl's Wings does,
      with the egg's startup command and default variables (EULA accepted), on the Docker host
      of `docker`, and check that the server starts and answers a status ping on port 30950.
  cargo xtask loadtest [--server vanillite|paper|both] [--steps 50,100@<movement>,...]
                       [--movement <movement>] [--hold <s>] [--settle <s>]
                       [--join-rate <per s>] [--chat <s>]
                       [--view-distance <n>] [--release] [--no-deploy] [--no-gate]
                       [--limits 4c8g|1c1g] [--rule <key>=<value>]...
                       [--fresh-players] [--binary <file>]
                       [--plugin <dir> [--plugin-set <toml line>]...]...
      Scaling benchmark: tools/loadbot bots against vanillite@30901 (deployed from this
      checkout) and the Paper reference paper-loadtest on 30900, both offline mode on the
      benchmark server vanillite-bench (VANILLITE_BENCH_HOST overrides it) and only running
      during the run; the bots run there too, on the CPUs the server's profile leaves free. Steps default to 50,100,250,500,1000,2000,5000,
      each held 60 s after --settle seconds (default 0); chat is off by default (--chat sets
      seconds between messages per bot). The server runs under the fixed hardware profile
      --limits (default 4c8g: 4 pinned CPUs, 8 GiB; 1c1g: 1 CPU, 1 GiB); every result records
      the test server's hardware and the host load, and runs on an overloaded host (load per
      CPU above 1) are marked unreliable and neither gate nor serve as a baseline.
      Movement (default for all steps: --movement, per step: <players>@<movement>; counts
      must not decrease): random (walk around the spawn) or straight:<speed>[:<turn s>],
      straight flight at y 300 in a random direction per bot at <speed> blocks/s or a preset
      (walk 4.317, sprint 5.612, elytra 33.5, boat 72.7), optionally turning every <turn s>;
      spread:<radius> (fly to a random point within the radius and stay) or still.
      E.g. --steps 50,50@straight:boat:30. Steps also report chunks/s received by the bots,
      holes (missing chunks within 2 chunks of a bot), teleports (rejected moves), the
      server's disk reads and read calls per player, and RSS per player above the idle RSS.
      Writes target/loadtest/<time>/{vanillite,paper}.json, report.md and chart.svg, keeps
      results of clean checkouts under /opt/vanillite/bench/results on the test server and
      fails when p95 MSPT or RSS per player of a step is more than 10 % above the median of
      the last 5 runs on the same hardware and limits (--no-gate skips the check). --rule sets a rule of vanillite.toml for
      the Vanillite instance of this run (e.g. performance.region-threading.enabled=true);
      such runs are experiments: no regression check, not kept as a baseline.
      --binary deploys that server binary instead of building one (before/after comparisons).
      --plugin installs a plugin folder on vanillite@30901 for the run and removes it
      afterwards: plugin.toml, the library it names (from the folder, or built from the
      folder's crate, e.g. examples/packet-bench) and config.toml with each --plugin-set line set (e.g. --plugin-set 'mode = \"receiver\"').
      Runs with --plugin or --binary are not kept in the results history. --fresh-players
      deletes the players vanillite@30901 saved, so the bots start at the spawn and not where
      an earlier run (e.g. a bench spread scenario) left them.
  cargo xtask loadtest report <run.json>...
      Writes report.md and chart.svg for earlier runs next to the first file.
  cargo xtask loadtest weak [--minutes <n>] [--binary <file>] [--release] [--cpu <n>]
                            [--memory-mib <n>] [--port <n>] [--rule <key>=<value>]...
                            [--prebuilt <dir>]
      Weak hardware under a realistic mixed load, on this machine (#270): the server from
      this checkout (or --binary) with a fresh world on one CPU (--cpu, default the first one
      allowed) and 1 GiB (systemd-run scope as root or via the vanillite-limited helper of
      vanillite-test2; else only pinned), offline mode on --port (default 30950), difficulty
      normal. tools/loadbot on the other CPUs: 3 explorers fly at sprint speed through new
      terrain (view distance 10), 3 survival players walk, build, break and fight mobs near
      the spawn (view distance 8), one player each stands at an animal farm (500 cows) and a
      redstone contraption (93 observer clocks). Measures --minutes (default 20): MSPT
      p50/p95/p99/max, TPS, RSS (per minute too), CPU, chunks/s and chunk wait of the bots.
      Writes target/loadtest-weak/<time>/result.json and server.log (with the phase profile
      of two minutes in the middle under server/profiles/). --prebuilt <dir> builds nothing:
      it runs <dir>/vanillite (or --binary) and <dir>/loadbot, results under
      <dir>/loadtest-weak/ (a machine without the checkout). On vanillite-test2:
      cargo xtask testserver run --fetch target/loadtest-weak -- loadtest weak
  cargo xtask bench all|<scenario>... [--servers <server,...>] [--publish] [--no-deploy]
                                     [--limits 4c8g|1c1g]
      Benchmark suite (`bench list` shows the scenarios): every scenario against Vanillite
      (vanillite@30901, deployed from this checkout; also with the paper rule profile), Paper,
      Vanilla and Pumpkin (taking turns on 30900), or the proxies Velocity and vanillite-proxy,
      with the same settings and a new world each (`vanillite-bench` on the benchmark server
      vanillite-bench, VANILLITE_BENCH_HOST overrides it). Player load from tools/loadbot
      there; command-built scenarios (redstone, mobs, farm, monsters, villagers, mobs-far,
      items, hoppers, TNT, fluids, light) use console commands and one observer bot. A server
      that lacks a
      command is reported as n/a. Writes target/bench/<time>/results.json and keeps the raw
      results under /opt/vanillite/bench/suite on the test server; --publish also updates
      benchmarks/results/ (latest.json, history.json, runs/<time>.json) and regenerates
      benchmarks/README.md with regressions and improvements against the previous run.
      --servers limits the run to these servers; --no-deploy uses the binaries installed there.
      --limits runs every scenario under that hardware profile instead (e.g. 1c1g for the weak
      hardware numbers of startup, redstone, mobs, flight); such runs cannot be published.
      Every server runs under the same fixed hardware profile (4c8g; the weak scenario 1c1g),
      and the run records the test server's hardware and the host load per server.
  cargo xtask bench publish <time|latest|results.json>
      Publishes a finished run like --publish: a local results.json or the suite directory
      /opt/vanillite/bench/suite/<time>/ on the test server (latest: the newest).
  cargo xtask bench report
      Regenerates benchmarks/README.md from benchmarks/results/.
  cargo xtask bench commands <scenario>
      Prints the console commands of a command-built scenario (the observer's teleport first),
      to build it on a local server for profiling.
  cargo xtask bench nightly [--no-push]
      When origin/dev has code newer than the published results: runs `bench all --publish` in
      the worktree target/bench/tree and commits the results on the branch
      bench/results-<date> (pushed unless --no-push; then hand it to the normal landing).
      Then runs `loadtest --steps 1000,2000,5000` on the same commit (#88 is judged from these
      quiet night runs; results under /opt/vanillite/bench/results on the test server).
  cargo xtask setup [--global]
      Install the git merge driver `vanillite` for the files .gitattributes marks (Cargo.lock,
      golden KEYS.toml, the egg, Cargo.toml, module lists, the rule catalogue): parallel
      additions there merge without conflicts. --global: for all clones of this user.
  cargo xtask regen
      After a merge: rewrite the generated files (the egg, the configuration reference) from
      their sources; cargo updates Cargo.lock on the way.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["vanilla", "update", rest @ ..] => vanilla_update(rest),
        ["version", "check"] => version::check(&repo_root()),
        ["version", "update", rest @ ..] => version::update(&repo_root(), rest),
        ["vanilla", "export", rest @ ..] => data::export(&repo_root(), rest),
        ["multiversion", "add", rest @ ..] => data::multiversion_add(&repo_root(), rest),
        ["codegen"] => data::codegen(&repo_root(), false),
        ["codegen", "--check"] => data::codegen(&repo_root(), true),
        ["golden", "check"] => golden::check(&repo_root()),
        ["golden", "regen", rest @ ..] => golden::regen(&repo_root(), rest),
        ["golden", "bless", names @ ..] => golden::bless(&repo_root(), names),
        ["parity", "fuzz", rest @ ..] => parity::fuzz(&repo_root(), rest),
        ["parity", "replay", rest @ ..] => parity::replay(&repo_root(), rest),
        ["parity", "soak", rest @ ..] => parity::soak(&repo_root(), rest),
        ["parity", "quick"] => parity::quick(&repo_root()),
        ["parity", "worlds", rest @ ..] => parity_worlds::worlds(&repo_root(), rest),
        ["parity", "coverage", rest @ ..] => coverage::run_coverage(&repo_root(), rest),
        ["worldgen", "sweep", rest @ ..] => worldgen::sweep(&repo_root(), rest),
        ["light", "sweep", rest @ ..] => worldgen::light_sweep(&repo_root(), rest),
        ["dev", rest @ ..] => devenv::dev(&repo_root(), rest),
        ["deploy", rest @ ..] => devenv::deploy(&repo_root(), rest),
        ["testserver", rest @ ..] => devenv::testserver(&repo_root(), rest),
        ["docker", rest @ ..] => docker::run_docker(&repo_root(), rest),
        ["egg", rest @ ..] => egg::run(&repo_root(), rest),
        ["config-docs", rest @ ..] => configdoc::run(&repo_root(), rest),
        ["loadtest", "weak", rest @ ..] => weak::run(&repo_root(), rest),
        ["loadtest", rest @ ..] => loadtest::run(&repo_root(), rest),
        ["bench", rest @ ..] => bench::run(&repo_root(), rest),
        ["setup", rest @ ..] => merge::setup(rest),
        ["regen"] => merge::regen(&repo_root()),
        ["merge-driver", base, ours, theirs, marker, path] => {
            merge::driver(base, ours, theirs, marker, path)
        }
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn vanilla_update(args: &[&str]) -> Result<()> {
    let mut version = None;
    let mut jar = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match *arg {
            "--jar" => jar = Some(PathBuf::from(it.next().ok_or("--jar needs a path")?)),
            v if !v.starts_with('-') && version.is_none() => version = Some(v.to_owned()),
            _ => return Err(USAGE.to_owned()),
        }
    }
    data::update(&repo_root(), version.as_deref(), jar.as_deref())
}

/// The workspace root (the parent of the xtask crate).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Cargo's target directory of a checkout: `CARGO_TARGET_DIR` when set, else `<root>/target`.
pub fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), |d| root.join(d))
}

/// Reads a file to a string with the path in the error.
pub fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Parses a JSON file.
pub fn read_json(path: &Path) -> Result<serde_json::Value> {
    serde_json::from_str(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes a file, creating parent directories.
pub fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, contents).map_err(|e| format!("{}: {e}", path.display()))
}
