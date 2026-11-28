# Development

## Setup

- Rust is pinned in `rust-toolchain.toml`; rustup installs it on the first `cargo` call.
- `cargo install --locked cargo-nextest cargo-deny` (or the prebuilt binaries).
- Recommended: [sccache](https://github.com/mozilla/sccache) as `RUSTC_WRAPPER` shares compiled
  dependencies between checkouts and target directories (a fresh clone builds in about half
  the time).
- The first build extracts the Vanilla data from Mojang's server jars (network, `curl` and a JDK; see
  [Vanilla data](#vanilla-data)): about two minutes, once per machine. A JDK 25 is otherwise
  needed only for golden regeneration.
- `cargo xtask setup` once per clone (`--global`: for every clone of this user): installs the
  merge driver for generated and collective files (see [Merging](#merging)).

## Merging

Many branches land in parallel, so files that every branch extends are built to merge without
conflicts:

- **Merge driver** `vanillite` (`xtask/src/merge.rs`, installed by `cargo xtask setup`, a copy
  of the xtask binary under `.git/vanillite/` or with `--global` under
  `~/.local/share/vanillite/`). `.gitattributes` lists the files it merges: `Cargo.lock` (per
  package; dependency lists as sets), the golden `KEYS.toml` files (per entry), the Pterodactyl
  egg (per field and per variable), `Cargo.toml` files and the rule catalogue (two insertions at
  the same place keep both; TOML must still parse, Rust must pass rustfmt) and `lib.rs`,
  `mod.rs`, `main.rs` (the same for `mod`/`use` lines). Any other conflict stays a normal
  conflict. Run `cargo xtask setup` again after the driver changes.
- **After a merge** run `cargo xtask regen`: it rewrites the generated files from their sources
  (the egg and the configuration reference; cargo updates `Cargo.lock` while building xtask). A conflict in a
  generated file is resolved by taking either side and running it.
- **One file per entry** where a list grows with every change: parity scenarios
  (`scenarios/*.json`) and their known divergences (`scenarios/divergences/<scenario>.toml`).
  Keep new registries that way (directory scanning), or sorted with one entry per line and a
  trailing separator.

## Everyday commands

| Command | What |
|---|---|
| `cargo fmt --all` | Format. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lints; CI fails on any warning. |
| `cargo nextest run` | Tests (`.config/nextest.toml`; no fail-fast). |
| `cargo nextest run --run-ignored all` | Including the bulk tests that CI runs nightly. |
| `cargo test --doc` | Doc tests (nextest does not run them). |
| `cargo deny check` | Licences, advisories, duplicate and wildcard dependencies (`deny.toml`). |
| `cargo xtask codegen [--check]` | Write the tables the build generates to `target/generated/` for reading (`--check`: only generate them). |
| `cargo xtask egg --check` | The two Pterodactyl eggs and `docs/configuration.md` match the configuration schema (`cargo xtask egg` regenerates them). |
| `cargo xtask dev [--release] [-- <args>]` | Build and run the server in `./run`. |
| `cargo xtask regen` | After a merge: rewrite generated files (egg, configuration reference). |

## Vanilla data

The data in Mojang's server jar (block states, registries, packets, items, entities, biome
parameters, ...) belongs to Mojang and is not in the repository. The repository only pins the
versions: `data/vanilla/version.toml` (the current version) and
`data/multiversion/<id>/version.toml` (the older versions the multi-version layer translates),
each with the jar's URL and SHA-1.

The build scripts of `vanillite-data` and `vanillite-multiversion` (`crates/vanillite-datagen`)
make the data on the first build: they download each pinned jar from Mojang, verify its SHA-1,
run Mojang's data generator and `tools/extractor`, write the normalised data to the cache and
generate the Rust tables into `OUT_DIR`. A cache entry is
`<cache>/vanilla-data/<id>-<full|mv>-<key>/`, where the key hashes the jar's SHA-1, the extractor
sources and the pipeline code, so it is made once per machine and pipeline version and every
branch finds its own. Warm builds do not touch the network or Java; a build script reruns only
when a pin, the extractor sources or the cache entry changes.

| Variable | Effect |
|---|---|
| `VANILLITE_CACHE_DIR` | Cache root (default `$XDG_CACHE_HOME/vanillite`, `~/.cache/vanillite`, `%LOCALAPPDATA%\vanillite`). |
| `VANILLITE_DATA_DIR` | Use this pre-extracted directory instead (`vanilla/`, `multiversion/<id>/`, made by `cargo xtask vanilla export <dir>`): offline machines or machines without Java. Its jar SHA-1s must match the pins. |
| `VANILLITE_JAVA_HOME`, `JAVA_HOME` | The JDK for the extraction (also `JAVA_HOME_25_X64` of GitHub runners and `java` on `PATH`). Without a JDK of the version the jar needs, Eclipse Temurin 25 is downloaded from Adoptium (SHA-256 checked) into `<cache>/jdk/`. |

The first build prints a warning with the extracted versions and the time (about 20 s for the
current version, two minutes for the eleven older ones, in parallel with compiling). CI, the
release and nightly workflows keep `vanilla-data/` in the GitHub cache; the Docker build keeps
`~/.cache/vanillite` in a cache mount. Tests that read the data files use
`vanillite_data::VANILLA_DATA_DIR` and `vanillite_multiversion::data_paths`; code that embeds a
report uses `vanillite_data::raw`.

| Command | What |
|---|---|
| `cargo xtask vanilla update [<id>] [--jar <server.jar>]` | Extract a version into the cache, pin it in `data/vanilla/version.toml` and print a change report. |
| `cargo xtask multiversion add <id>...` | Extract older versions for the multi-version layer and pin them in `data/multiversion/<id>/version.toml`. |
| `cargo xtask vanilla export <dir>` | Copy the data of all pinned versions to `<dir>` (for `VANILLITE_DATA_DIR`). |

## New Minecraft versions

Vanillite targets only the newest Minecraft release, pinned in `data/vanilla/version.toml`. The
nightly workflow runs `cargo xtask version check` and opens an issue when Mojang releases a newer
version.

| Command | What |
|---|---|
| `cargo xtask version check` | Fails if a newer release than the pinned one exists. |
| `cargo xtask version update <id>` | Preview (any release, pre-release or snapshot): data (into the cache), pins, goldens and a test run in the worktree `target/version/<id>/tree/`, and the change report `target/version/<id>/change-report.md` (protocol, packet fields, registries, blocks, items, entities, data pack, goldens, failing tests, affected code areas). The pinned version stays. |
| `cargo xtask version update <id> --final` | The switch itself (releases only): the same in this checkout; commit the pins (`data/**/version.toml`) and goldens. |

Golden harness sources that do not compile against the new jar are left out (with what needs
them) and named in the report; porting them is part of the switch.

## Build profiles

| Profile | Use |
|---|---|
| `dev` | Daily work. Line tables only; dependencies optimised. |
| `release` | Shipped binary: fat LTO, one codegen unit, stripped. Slow to build. |
| `release-fast` | Close to release speed, quick to rebuild; test server deploys. |
| `profiling` | Release code with symbols for perf, flamegraphs and heaptrack. |

Keep `cargo build` and `cargo test` on one set of compiled crates: a feature enabled only in
`[dev-dependencies]` (directly or by a dev-only crate's own dependencies) changes the features
of a shared dependency in test builds, and cargo then compiles that dependency and every
workspace crate above it a second time. Enable such a feature in the normal dependency, and
keep dev-only crates free of dependencies (the micro benchmarks use `tools/microbench`, not
criterion, for that reason). Build scripts share no compiled crates with the rest, so their
dependencies (`vanillite-datagen`) stay few: downloads go through the system `curl`.

## Test server

`ssh vanillite-test` (host overridable with `VANILLITE_TEST_HOST`). Players may only use ports
30000-30999: Paper (reference) runs on 30000, Vanillite on 30001 and up. The machine is
provisioned by `scripts/testserver/provision.sh`; every Vanillite server is a systemd unit
`vanillite@<port>` in `/opt/vanillite/<port>`, with Paper's whitelist.

**30001, 30002 and the proxy on 30010 always run the newest green dev.** `vanillite-autodeploy`
(a systemd timer on the test server, every 30 minutes) builds the newest dev commit whose CI
workflow passed (`release-fast`, in `/opt/vanillite-build`), announces the
restart in chat 10 seconds ahead, restarts the instances and rolls back to the previous binary
if one does not start. The MOTD shows the deployed commit; the deploy log is
`/var/log/vanillite-autodeploy.log` (`vanillite-autodeploy status`). Don't deploy branches to
these ports by hand: landing on dev is enough. For a test before landing, use a port in
30003-30099 and stop it afterwards.

| Command | What |
|---|---|
| `cargo xtask testserver provision` | Install or update users, units and the deploy helper (idempotent). |
| `cargo xtask deploy [--port 30003] [--release]` | Build (`release-fast`, or `release`) and (re)start `vanillite@<port>` (not 30001/30002). |
| `cargo xtask testserver status` | Services, listening ports and the last auto-deploys. |
| `cargo xtask testserver logs [<port>]` | Follow the log. |
| `cargo xtask testserver start\|stop\|restart [<port>]` | Control a server; stop also disables it. |
| `cargo xtask testserver incidents [list [<count>]]` | The newest crashes and suspicious disconnects. |
| `cargo xtask testserver incidents show <id>` | Summary and backtrace of an incident. |
| `cargo xtask testserver incidents fetch [<id>...]` | Download bundles to `target/incidents/<id>/` (no ids: all not downloaded yet). |

### Incidents

No log digging: `vanillite-incidents` (a systemd service on the test server) follows the journal
of every `vanillite@<port>` and of `vanillite-proxy` and saves an incident bundle in
`/var/lib/vanillite-incidents/<id>/` for

- every **crash**: a panic, a crashed world thread, an exit with an error, a kill or an OOM kill
  (systemd restarts the instance as before);
- every **suspicious disconnect**: a log line starting with `Suspicious disconnect` (the
  server's disconnect diagnosis), a player who loses the connection within 10 s of logging in,
  or a disconnect with an error as reason. Not on the load test instance 30901; at most one per
  instance and player in 10 minutes.

A bundle has `summary.txt` (time, instance, signature, deployed commit and binary, the players
online with their world and login position, the trigger lines, the Vanillite frames of the
backtrace), `backtrace.txt`, `journal.log` (the instance's journal from 5 minutes before a crash,
2 minutes before a disconnect, to 30 seconds after) and `meta.json`. The signature is the panic
location (the first Vanillite frame when the panic is inside std), the exit status or the
disconnect reason; `list` marks the first incident of a signature `new`, so known crashes stand
out from new ones. Bundles are kept 60 days, at most 300. `cargo xtask testserver status` shows
the last five. On the test server itself: `vanillite-incidents list|show <id>`, and
`vanillite-incidents scan <since> [<until>]` collects a past time range after the fact.

## Benchmarks

`cargo xtask bench all|<scenario>...` runs the benchmark suite on the test server against
Vanillite (vanillite@30901), Paper, Vanilla and Pumpkin (`cargo xtask bench list` shows the
scenarios; the test server is locked with `vanillite-bench lock` while it runs, about 3.5 h for
`all`). Raw results stay on the test server under `/opt/vanillite/bench/suite/<time>/`
(`<scenario>-<server>.json` plus `run.json`, the whole run).

**The repository is the source of truth for the published numbers** (the documentation site
reads them from there). Nobody copies numbers by hand:

| File | What |
|---|---|
| `benchmarks/README.md` | Generated summary: regressions and improvements of Vanillite against the previous run at the top, then per scenario tables and mermaid charts, with date, dev commit, versions and hardware. Never edit it. |
| `benchmarks/results/latest.json` | The newest result of every scenario (each with its own date and commit). |
| `benchmarks/results/history.json` | One line per run: the numbers the regression check compares. |
| `benchmarks/results/runs/<time>.json` | Every published run as measured, one line per scenario. |

Values that a server cannot measure are marked n/a (with the reason), broken runs error and
servers that did not reach the load failed; nothing is filled in. A change counts as a
regression or improvement when it exceeds 10 % and a small absolute step per unit (0.5 ms,
0.3 s, 16 MiB, 2 %), or three robust deviations (MAD) of the last 10 runs where the number
scatters more. Changes of the reference servers are listed separately as an indicator of the
machine's own noise.

- `cargo xtask bench <scenarios> --publish` (clean checkout only) or, for a finished run,
  `cargo xtask bench publish <time|latest|path/to/results.json>` writes these files.
  `cargo xtask bench report` regenerates the README from them.
- **Nightly, unattended:** PlainBoard schedule 29 starts the Tooling Developer agent at 03:00
  CET. It runs `cargo xtask bench nightly`: when origin/dev has code newer than the published
  results, it runs `bench all --publish` in the worktree `target/bench/tree`, commits
  `benchmarks/` on the branch `bench/results-<date>` and pushes it. The agent hands that branch
  to `merge_when_green`, which lands it on dev like any other branch. When a run breaks
  half-way, `cargo xtask bench publish <time>` publishes what it measured.

## Docker image

`Dockerfile` builds a static (musl, mimalloc) binary on an empty base for linux/amd64 and
linux/arm64. `cargo xtask docker [--push]` cross-compiles both binaries here (fast, sccache) and
builds the image on `$DOCKER_HOST`, the local engine, or else the test server over SSH (dev
containers cannot run an engine); `--push` pushes `docker.hthoene.de/vanillite/vanillite` with
the tags `dev` and the short commit hash (needs `PRIVATE_DOCKER_REPO_KEY`). Plain
`docker build .` builds the binary inside the image instead.

Plugins: the static binaries cannot load libraries, so the image and the Linux release archives
also ship `vanillite-gnu` (zigbuild `<arch>-unknown-linux-gnu.2.28`, feature `mimalloc`: the same allocator), and the
image the glibc files it and a Java runtime load (from `debian:bookworm-slim`, copied without
running anything). The static binary starts it in its place only when `plugins/` holds plugins
(`crates/vanillite/src/runtime.rs`, `plugins.runtime`).

Linux x86_64 (image and release archive) ships two builds: `vanillite` for every x86-64 CPU and
`vanillite-x86-64-v3` (AVX2/BMI2/FMA, bit-identical, fewer instructions). The baseline binary
starts the v3 build in its place when the CPU supports it (`crates/vanillite/src/cpu.rs`) and the
log says which one runs (`CPU build: ...`); `VANILLITE_CPU=baseline` keeps the baseline. The image
sets `MIMALLOC_ARENA_EAGER_COMMIT=0` and `MIMALLOC_PURGE_DELAY=0` for lower RSS (#164).

Host tools for the cross build:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
cargo install --locked cargo-zigbuild
mkdir -p ~/.local/zig && curl -fsSL https://ziglang.org/download/0.14.1/zig-x86_64-linux-0.14.1.tar.xz \
    | tar -xJ -C ~/.local/zig --strip-components=1   # then put ~/.local/zig on PATH
scripts/docker/install-docker.sh --client   # Docker CLI and buildx
```

### Pterodactyl eggs

`deploy/pterodactyl/egg-vanillite.json` (*Vanillite*, every setting) is generated from the typed
schema in `vanillite-config`:
every `server.properties` key (`ServerProperties::INFO`, except those declared `panel = false`)
and every `vanillite.toml` rule (except the `debug` group) becomes a variable named like its
`VANILLITE_<KEY>` override, with the documentation, default, range or allowed values. The
curated `ESSENTIALS` in `xtask/src/egg.rs` come first, in their order and with plain names and
one-sentence descriptions; all other variables follow grouped by area (`AREAS`, the first part
of the rule's table) with the area as name prefix, so new keys appear without changes to the
generator. Panels render `in:true,false` as a switch (Pterodactyl) and other `in:` rules as
dropdowns: booleans and enumerations (`ValueShape::choices`) of rules get a dropdown with
`default`, which the server ignores like an empty value (`EnvOverrides`), so the profile decides.
The image dropdown comes from `deploy/minecraft-versions.txt` (one line per Minecraft version
with images, `<version> beta|stable`, newest first): one entry per version (`mc<id>-beta` or
`mc<id>`), the version of the sources first (the default), then `latest` (once a stable exists)
and `beta`. `cargo xtask egg` (also run by `version update` and `regen`) adds the version of the
sources and marks it stable for a stable workspace version, `egg --check` fails while the file
lacks it; delete a line by hand when a version gets no more images. `deploy/pterodactyl/egg-vanillite-simple.json` (*Vanillite
(Simple)*) holds only the curated `SIMPLE` settings with plain-language descriptions; everything
else keeps the profile's value. Both eggs share images, startup, stop and variable names, so a
server can switch between them; `egg --check` fails if a simple variable is missing in the full
egg or its default is not valid there. After a change of the schema run `cargo xtask egg`; CI
and the `egg` test of xtask fail while a committed egg is stale. `cargo xtask egg smoke [--image
<ref>]` starts an image like Wings does (STARTUP, `/home/container`, UID 988, read-only root)
and pings it. How to import and use the eggs is in the README.

### Configuration reference

`docs/configuration.md` is generated from the same schema by `cargo xtask config-docs`
(`xtask/src/configdoc.rs`): every `server.properties` key with its default, and every
`vanillite.toml` rule with its value in each profile, its kind, whether it needs a restart and
its environment variable. `cargo xtask egg` writes it too and `cargo xtask egg --check` fails
while it is stale, so a new or changed rule only needs its doc comment in the catalogue.

## Fuzzing

`fuzz/` holds the cargo-fuzz targets of the input decoders (crate `vanillite-fuzz`, its own
workspace, built with nightly): binary NBT, SNBT, inbound framing and decompression, the framing
round trip, and one target per connection state for the serverbound packet decoders. Committed
seeds are in `fuzz/seeds/<target>`; the growing corpus (`fuzz/corpus`) and crashes
(`fuzz/artifacts`) are not committed.

| Command | What |
|---|---|
| `scripts/fuzz.sh [seconds] [jobs]` | Build all targets once and run them in parallel (default 300 s each). |
| `cargo +nightly fuzz run -O <target> fuzz/corpus/<target> fuzz/seeds/<target>` | Run one target. |
| `cargo +nightly fuzz run -O <target> fuzz/artifacts/<target>/<file>` | Reproduce a crash. |

A crash becomes a regular regression test next to the exploit tests in
`crates/<crate>/tests/exploits.rs`, which name the SEC rule of the security research they cover.

## Parity tests

Vanillite is compared with the pinned Vanilla jar (architecture section 13). The Java harness in
`tools/extractor/src/dev/vanillite/extractor/golden/` runs Vanilla in-process and records
goldens; the Rust tests compare against them exactly.

- **Data goldens** (`crates/vanillite-test/tests/data_goldens.rs`): registries, block state ids,
  items, packet ids and the loaded Vanilla data pack (dynamic registries, tags).
- **Scenario traces** (`crates/vanillite-test/scenarios/*.json`, format in
  `crates/vanillite-test/src/scenario.rs`): a scenario runs tick by tick on Vanilla and in
  Vanillite; blocks of a region, pending block and fluid ticks and entities must match in every
  frame. Every scenario also runs serially and in parallel (verify mode). Tier `fast` goldens
  are in the repository and run on every push; tier `bulk` goldens live in
  `$VANILLITE_GOLDENS` (default `target/goldens`) and run nightly.
- **World generation goldens** (`crates/vanillite-worldgen/tests/`).

Every golden is keyed by Minecraft version, jar SHA-1, harness version
(`tools/extractor/harness-version`, bump it when shared recording code changes) and the hash of
its scenario or recorder source. A missing or stale golden fails; it is never skipped.

| Command | What |
|---|---|
| `cargo xtask golden check` | Repository goldens are current (CI, no Java). |
| `cargo xtask golden regen [--all] [--bulk] [<name>...]` | Record missing or stale goldens (JDK 25). |
| `cargo xtask golden bless <scenario>...` | Accept Vanillite's trace for intended differences. |

`VANILLITE_SCENARIO=<name>,... cargo test -p vanillite-test --test parity` runs only those scenarios.
`VANILLITE_SCENARIO_SHARD=<k>/<n>` runs only every n-th scenario in name order from the k-th on.

### Mob parity scenarios

Mob AI, path finding and entity behaviour must match Vanilla tick for tick. Every mob gets at
least one scenario `crates/vanillite-test/scenarios/mob_<what>.json` (see `mob_zombie_chase.json`):
a small arena (`setup`), the time of day (`time`), the entities (`entities`: players and mobs
with a `name` and a `seed`, `"finalize": false` to skip `finalizeSpawn`) and actions such as
`hurt`. The recorder runs it in Vanilla with fixed entity ids, UUIDs and seeds (its Java agent
makes every Vanilla random reproducible), and the trace holds per tick and entity: position,
motion, rotation, health, target, running goals and target goals (Vanilla class names), the
navigation path and the state of the entity's random (`rng`) and of the level random.

Adding a mob:

1. Map its goals to Vanilla's class names in `goal_name` (`crates/vanillite-test/src/game_world.rs`).
2. Write the scenario and record it: `cargo xtask golden regen mob_<what>` (JDK 25).
3. Run `VANILLITE_SCENARIO=mob_<what> cargo test -p vanillite-test --test parity`: it prints the
   first differences with expected and actual values. The first tick where `rng` differs is
   the first random call Vanillite makes differently (a missing goal, a draw from the wrong
   random, a different call order). The number of draws between two `rng` states shows how
   many calls each side made.
4. Fix it, or record what is left in `scenarios/divergences/<scenario>.toml` (tick, cause, owner): the
   check then stays green while the first difference is at exactly that tick, fails when it
   moves earlier (a regression) and asks to raise the tick when it moves later. Editing that
   file needs no new recording.

Adding an action or a field to `Op` (`crates/vanillite-test/src/scenario.rs`): give the new
field a type that implements `Default` (usually `Option<_>`) and derive `Default` for new action
structs that need it. Every construction site names only `tick`, `kind` and the fields of its own
kind and ends with `..Op::default()`, so a field added on another branch never breaks it; keep
new sites that way.

Scenario players are mock players: they join on a connection nobody reads and load no chunks
(the region's chunks are the world). No client moves them, so the server does, in Vanilla as in
Vanillite (`Survival::simulate_movement`): knockback, pushes and explosions move them with
gravity, friction and collisions. Their motion is not compared, their position and body yaw
are.

The recorder makes Vanilla's other hidden state fixed as well: every scenario starts at game
time 0, and every shuffled behaviour list (`ShufflingList`, the brains' `RunOne` gates) seeds its
own random with the same constant (`ParityHooks.SHUFFLE_SEED`), as Vanillite's scenario world
does.

## Releases

Vanillite has its own [SemVer](https://semver.org) version (`version` of `[workspace.package]`
in `Cargo.toml`) with the Minecraft version it targets attached as build metadata; the Minecraft
version is the `id` pinned in `data/vanilla/version.toml` (`cargo xtask version update`), never
written by hand. `vanillite --version` and the first log line show both and the commit, e.g.
`Vanillite 0.1.0-beta.1 (Minecraft 26.3, 3bd22ae)`.

| What | Scheme | Example |
|---|---|---|
| Git tag, GitHub release | `v<version>+mc<minecraft>` | `v0.1.0-beta.1+mc26.3` |
| Release archive | `vanillite-<version>-mc<minecraft>-<os>-<arch>.<tar.gz\|zip>` | `vanillite-0.1.0-beta.1-mc26.3-windows-x86_64.zip` |
| Docker tag, immutable | `<version>-mc<minecraft>` | `0.1.0-beta.1-mc26.3` |
| Docker tag, newest of a channel for a Minecraft version | `mc<minecraft>-<channel>`, stable: `mc<minecraft>` | `mc26.3-beta`, `mc26.3` |
| Docker tag, newest of a channel | `<channel>`, stable: `latest` | `beta`, `latest` |

The channel is the first part of the pre-release (`beta` of `0.1.0-beta.1`, `rc` of
`0.2.0-rc.1`). Pre-releases are GitHub pre-releases and never move `latest` or `mc<minecraft>`.
`scripts/release/meta.sh [<tag>]` derives all names from the sources and fails for a tag that
does not match them.

To release: set the workspace version (and the `version` of every internal crate in
`[workspace.dependencies]`), land it on dev, promote dev to main and push the tag
`v<version>+mc<minecraft>` on that commit. The tag starts two workflows:

- `.github/workflows/release.yml` builds the release profile for Linux (static musl, x86_64 and
  aarch64, cargo-zigbuild, plus `vanillite-gnu` for plugins), Windows (x86_64, static C runtime) and macOS (aarch64 and x86_64),
  packs each binary with LICENSE, NOTICE and a README.txt (`scripts/release/package.sh`,
  template `scripts/release/README.txt`) and creates the GitHub release with SHA256SUMS and
  generated notes. A manual run (`gh workflow run release.yml --ref <branch>`) is a dry run: the
  archives become workflow artifacts and nothing is published.
- `.github/workflows/docker.yml` pushes the linux/amd64 and linux/arm64 image to Docker Hub with
  the tags above. The channel tags `beta` and `latest` move only for the newest Minecraft version of
  `deploy/minecraft-versions.txt`, so an update for an older version does not take them back; a
  stable release also moves `mc<minecraft>-beta` and `beta`. The Pterodactyl eggs offer every
  version of that file (see "Pterodactyl eggs").

The binaries hold everything but Mojang's data: the first start downloads the pinned server jar
into `cache/` (internet access needed once, no Java).

### Signing the Windows build

The release workflow publishes `vanillite.exe` unsigned. The signing certificate (Certum, cloud
certificate in SimplySign) never leaves the owner's account, so the owner signs each release on
their Windows PC after the workflow has published it, with `scripts/release/sign-windows.ps1`.
For the tag it downloads the Windows zip archives and SHA256SUMS (and checks them), signs
`vanillite.exe` with signtool (SHA-256 file digest, RFC 3161 timestamp from
`http://time.certum.pl`), verifies it with `signtool verify /pa`, packs the archive again with
the same layout, updates SHA256SUMS and replaces both on the release (`gh release upload
--clobber`). Archives already signed are skipped, so it can run again; `-DryRun` does everything
but the upload. Linux and macOS archives and the Docker images are not touched.

Once per PC: install SimplySign Desktop, the GitHub CLI (`winget install GitHub.cli`, then `gh
auth login`) and the Windows SDK signing tools (`winget install Microsoft.WindowsSDK.10.0.26100`,
or "Windows SDK Signing Tools for Desktop Apps" in the Visual Studio Installer). Per release:

1. Wait until the release workflow of the tag has finished.
2. Start SimplySign Desktop and log in (token code from the SimplySign mobile app).
3. In PowerShell in a checkout of the repository (current `main`):

   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\release\sign-windows.ps1 -Tag 'v0.1.0-beta.1+mc26.3' -CertificateSubject '<name in the certificate>' -DryRun
   powershell -ExecutionPolicy Bypass -File scripts\release\sign-windows.ps1 -Tag 'v0.1.0-beta.1+mc26.3' -CertificateSubject '<name in the certificate>'
   ```

   `-Thumbprint <sha1>` selects the certificate instead of `-CertificateSubject` when several
   match. Every failed prerequisite (gh missing or logged out, signtool not found, SimplySign
   Desktop not running, certificate not in the store) stops the script with the fix.
4. Check the release: the Windows zip and SHA256SUMS are new, and `vanillite.exe` shows a valid
   signature under Properties > Digital Signatures.

## CI

`.github/workflows/ci.yml` runs on every push and pull request: format, clippy, cargo-deny, the
doc tests with the codegen, egg and golden checks, and the tests in four jobs, all in parallel
with Rust and toolchain caches written by `dev`. The tests are split by crate: the world
generation goldens (`-p vanillite-worldgen`), the game and world crates (`-p vanillite-game -p
vanillite-world`, the menu fuzz and chunk level tests), the test harness with the parity
scenarios (`-p vanillite-test`, built with the world crate at opt-level 1, in three parts by
`VANILLITE_SCENARIO_SHARD=<k>/3`) and all
other crates (`--workspace` with those four excluded). A crate added to the workspace lands in
the last job by itself.
`.github/workflows/nightly.yml` records missing bulk goldens and runs the ignored bulk tests (with
Java 25 and a goldens cache), a
release build and the fuzz targets (`scripts/fuzz.sh`, corpus cached between nights) once a
night.
