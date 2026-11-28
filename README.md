<p align="center">
  <img src="docs/assets/logo.svg" width="112" alt="Vanillite logo">
</p>

<h1 align="center">Vanillite</h1>

<p align="center">
  <b>A Minecraft: Java Edition server written in Rust that plays exactly like Vanilla.</b>
</p>

<p align="center">
  <a href="https://github.com/hthoene/vanillite/actions/workflows/ci.yml"><img src="https://github.com/hthoene/vanillite/actions/workflows/ci.yml/badge.svg?branch=dev" alt="CI"></a>
  <a href="https://github.com/hthoene/vanillite/releases"><img src="https://img.shields.io/github/v/release/hthoene/vanillite?include_prereleases&label=release" alt="Release"></a>
  <a href="https://hub.docker.com/r/thoenehannes/vanillite"><img src="https://img.shields.io/docker/v/thoenehannes/vanillite/beta?label=docker" alt="Docker"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache 2.0"></a>
  <img src="https://img.shields.io/badge/Minecraft-26.3-62b47a" alt="Minecraft 26.3">
</p>

Vanillite is a Minecraft Java Edition server that behaves 1:1 like Vanilla in game (the same
mob AI, redstone, world generation and tick order, checked tick by tick against Mojang's
server) while being fast, multithreaded and light on memory. It is a single native binary
without Java, modular down to its storage and world generator, and extensible with native
plugins over a stable C ABI. Vanillite always targets the newest Minecraft release, currently
**26.3**; older clients can join through the built-in version translation, Bedrock players
through the built-in Bedrock frontend.

> **Beta.** Vanillite `0.1.0-beta.3` is a beta release ([changes](CHANGELOG.md)). Back up your worlds and
> report problems in the [issues](https://github.com/hthoene/vanillite/issues).

## Highlights

- **Vanilla in game.** World generation, block behaviour, redstone, fluids, light, mobs and
  their path finding follow Vanilla's code paths, random calls and tick order. Parity tests
  run scenarios in Mojang's server and in Vanillite and compare every tick.
- **Fast and lean.** Dimensions can tick in parallel, world generation, light and saving run
  on a shared worker pool, chunks are encoded once and sent to every viewer. Runs on a single
  core and respects container CPU and memory limits.
- **Profiles instead of forks.** One switch between exact Vanilla (`vanilla`, the default),
  `technical`, `paper`, `spigot` and `performance`; every deviation from Vanilla is a single,
  documented rule.
- **Own world format.** New worlds are stored in a compact, crash-safe format; existing Anvil
  worlds keep working, and both convert into each other at any time.
- **Native plugins.** Plugins are shared libraries written against a stable C ABI, with a safe
  Rust SDK. They can replace whole systems such as the world generator.
- **More clients.** Java clients from 1.20.5 to 26.2 through the multi-version layer, Bedrock
  Edition through the built-in frontend, and a Velocity-compatible proxy.

### Measured

From the nightly benchmark suite on 4 cores of a Ryzen 9 5950X with headless bots, run of
2026-11-27 (Paper and Vanilla rows and the last two lines from the run of 2026-11-25). Every
number, the method and the history are in [`benchmarks/README.md`](benchmarks/README.md).

| Scenario | Vanillite | Paper 26.3 | Vanilla 26.3 |
|---|---:|---:|---:|
| Idle memory after start (RSS) | **282 MiB** | 1385 MiB | 1003 MiB |
| 500 cows in a pen, mean tick time | **6.69 ms** | 10.4 ms | 11.3 ms |
| 2 000 dropped items, mean tick time | **5.96 ms** | 8.27 ms | 12.44 ms |
| Flowing water and lava, mean tick time | **4.06 ms** | 5.05 ms | 5.28 ms |
| 1 024 hoppers, mean tick time | **4.5 ms** | 5.33 ms | 4.78 ms |
| World pre-generation, chunks per second | **248** | 60.96 | – |
| Proxy CPU at 500 players (vanillite-proxy vs Velocity) | **78 %** | 257 % | – |

Pre-generation compares Vanillite's chunk pipeline benchmark with the Chunky plugin in a
running Paper server. Not every scenario is ahead yet: the redstone and TNT scenarios currently take longer per
tick than Paper, and the world save scenario regressed in the latest run. The benchmark page
lists them all.

## Quick start

Vanillite needs no Java. On the first start it downloads Mojang's server jar once (for the
Vanilla data) into `cache/` and creates `eula.txt`: accept the
[Minecraft EULA](https://aka.ms/MinecraftEULA) there or with `VANILLITE_EULA=true`. Started
from a terminal (or with a double click on Windows), the server asks instead: type `yes` and it
saves `eula=true` and starts.

### Release archive

Download the archive for your system from the
[releases](https://github.com/hthoene/vanillite/releases) (Linux x86_64 and aarch64, Windows
x86_64, macOS aarch64 and x86_64), unpack it into its own folder and start it there:

```sh
./vanillite            # Windows: vanillite.exe
```

`vanillite --help` lists the commands (`run`, `health`, `convert`, `export`, `upgrade-world`,
`version`); `--dir <folder>` picks another server folder. Stop the server with `stop` or
Ctrl+C: both save the worlds.

### Docker

The image `thoenehannes/vanillite` (linux/amd64 and linux/arm64) holds a static binary, plus
`vanillite-gnu` that it starts instead when `plugins/` holds plugins. World, configuration,
plugins and cache live in the volume `/data`.

```sh
docker run -d --name vanillite -p 25565:25565 -v vanillite-data:/data \
    -e VANILLITE_EULA=true -e VANILLITE_MOTD="A Vanillite server" \
    --stop-timeout 60 thoenehannes/vanillite:beta
```

```yaml
services:
  vanillite:
    image: thoenehannes/vanillite:beta
    ports:
      - "25565:25565"
    volumes:
      - vanillite-data:/data
    environment:
      VANILLITE_EULA: "true"
      VANILLITE_MAX_PLAYERS: "50"
    stop_grace_period: 60s # SIGTERM saves all worlds before the server exits
    restart: unless-stopped
volumes:
  vanillite-data:
```

| Tag | Follows |
|---|---|
| `beta` | the newest beta, also across Minecraft versions |
| `mc26.3-beta` | the newest beta for Minecraft 26.3 |
| `0.1.0-beta.3-mc26.3` | exactly this release |

The health check (`vanillite health`) sends a status ping. A bind-mounted `/data` must be
writable by UID 10001. Console commands go through `docker attach` (start the container with
`-it` for that).

### Pterodactyl and Pelican

There are two eggs; import one or both in the admin panel (*Nests → Import Egg*, Pelican:
*Eggs → Import*) and create a server with it; its primary allocation is the game port.

| Egg | File | For |
|---|---|---|
| *Vanillite (Simple)* | [`egg-vanillite-simple.json`](deploy/pterodactyl/egg-vanillite-simple.json) | Admins who want a server that just runs: only the basic settings, everything else uses the profile's defaults. |
| *Vanillite* | [`egg-vanillite.json`](deploy/pterodactyl/egg-vanillite.json) | Every `server.properties` and `vanillite.toml` setting. |

Both start the same image the same way and use the same variable names, so a server can be
switched between them (admin panel: the server's *Startup* tab, *Egg*); check the *Startup* page
of the server after a switch.

- **Minecraft version:** the image on the *Startup* page picks it. *Minecraft 26.3 - Beta*
  (`mc26.3-beta`, the default) stays on Minecraft 26.3 and gets every Vanillite update for it as
  long as we publish them; *Latest Minecraft - Beta* (`beta`) moves to each new Minecraft version
  on the next restart (Wings pulls the image on every start). After the first stable release
  *Minecraft 26.3* (`mc26.3`) and *Latest Minecraft* (`latest`) join them. New Minecraft
  versions appear in the dropdown once the admin re-imports the egg (*Nests → Import Egg* over
  the existing one). When a version gets no more updates, its entry leaves the egg and its image
  stays pullable but frozen: switch such a server to a newer version (worlds are upgraded, never
  downgraded).
- **EULA:** switch on *Accept the Minecraft EULA*, or accept the panel's EULA prompt on the
  first start.
- **Settings (simple egg):** server type (profile), name (MOTD), max players, game mode,
  difficulty, hardcore, whitelist, online mode, seed, view distance, world format and Bedrock
  players.
- **Settings (full egg):** the *Startup* page starts with the settings most servers change
  (profile, MOTD, players, game mode, difficulty, whitelist, seed, distances, Bedrock players);
  every other setting follows, grouped by area. Empty technical settings (or `default`) keep the
  profile's value.
- **Bedrock** players join on the server's port (UDP) unless *Bedrock: Port* names another
  allocation.

- **Plugins:** upload Paper/Spigot plugin jars into `plugins/` with the panel's file manager and
  restart (see "Paper and Spigot plugins").

The eggs are generated from the configuration schema, so re-import them after an update.

### Paper and Spigot plugins

1. Put the plugin jars into `plugins/` of the server folder (Docker: `/data/plugins/`).
2. Start (or restart) the server.

That is all. When `plugins/` holds Paper or Spigot plugins, the server installs the Bukkit layer
([vanillite-bukkit](https://github.com/hthoene/vanillite-bukkit), the release made for this
Vanillite version) into `plugins/bukkit/` and, if there is no Java 25 runtime, Eclipse Temurin
into `plugins/bukkit/runtime/`, both checked by SHA-256, and loads the plugins; the log says
every step. Without such jars there is no Java and no cost. Offline servers set
`plugins.bukkit.auto-setup = false`: the log then says which file to download and where to put
it. See [docs/plugins.md](docs/plugins.md#paper-and-spigot-plugins).

## Configuration

| File | What |
|---|---|
| `server.properties` | Vanilla's keys with Vanilla's meaning and defaults. |
| `vanillite.toml` | The `profile` and every Vanillite rule, written on first start with documentation, commented out. Per-world overrides go into `[worlds."minecraft:the_nether"]` sections. |
| `VANILLITE_<KEY>` | Environment variables override any key of both files for one run, e.g. `VANILLITE_MAX_PLAYERS=50`, `VANILLITE_PROFILE=performance`, `VANILLITE_PERFORMANCE__TICK__PARALLEL_DIMENSIONS=false` (`.` becomes `__`, `-` becomes `_`). |

| Profile | |
|---|---|
| `vanilla` | Exactly like Vanilla, tick for tick (the default). Deviates only to close security holes. |
| `technical` | Vanilla plus what technical players need. |
| `paper` | Paper's defaults (exploit fixes, parallel dimensions). |
| `spigot` | Spigot's defaults, including Spigot's world folder layout. |
| `performance` | The strongest optimisations that change Vanilla behaviour only a little. |

Rules without a restart marker are reloaded while the server runs (`vanillite reload config`).
Every key with its values in each profile is listed in the generated
[configuration reference](docs/configuration.md).

## Worlds

- **Own format** for new worlds (`storage.world-format = "vanillite"`): about 1.75 × smaller
  than Anvil and several times faster to load and save, with atomic writes.
- **Anvil** worlds of Vanilla, Paper and Spigot are opened as they are and stay in Anvil unless
  `storage.convert-world` is set. `vanillite convert --to vanillite|anvil` converts a stopped
  world; the new files are read back and compared before the old ones are moved aside.
- **Export:** `vanillite export <folder>` writes a Vanilla (Anvil) copy of an own-format world
  that Vanilla, Paper and the usual tools open.
- **Layouts:** `storage.layout` keeps dimensions in Vanilla's and Paper's layout or Spigot's
  `world_nether`/`world_the_end` folders and moves a world from the other layout at startup.
- **Saving and backups:** autosave spreads its work over the ticks; `backup.interval` makes zip
  backups of the running world in the background.

More in [docs/worlds.md](docs/worlds.md).

## Plugins

Plugins are native libraries (`.so`, `.dll`, `.dylib`) with a `plugin.toml`, placed in
`plugins/`. They talk to the server only through a versioned C ABI
([`vanillite-abi`](crates/vanillite-abi)), so a plugin keeps working across server builds and
compilers. The Rust SDK [`vanillite-plugin`](crates/vanillite-plugin) wraps it safely: events,
Brigadier commands with suggestions, scheduling on the owning thread, persistent data,
configuration, and providers that replace built-in systems such as the chunk generator.

```rust
use vanillite_plugin::prelude::*;

struct Hello;

impl Plugin for Hello {
    fn load(registrar: &mut Registrar<Self>) -> Result<Self, Error> {
        registrar.command(Command::literal("hello").executes(|_, ctx, invocation| {
            invocation.reply(ctx, "Hello!");
            Ok(1)
        }));
        Ok(Hello)
    }
}

vanillite_plugin::export_plugin!(Hello);
```

The complete example is [`examples/hello-plugin`](examples/hello-plugin); the guide is
[docs/plugins.md](docs/plugins.md). Java plugins for Bukkit, Spigot or Paper do not run on
Vanillite.

## Bedrock, older clients and the proxy

- **Bedrock Edition** (`bedrock.enabled = true`): a RakNet listener (UDP, port 19132 by
  default) with Xbox Live login translates Bedrock clients into Java connections inside the
  server. Bedrock players get a name prefix (`.` by default) and a UUID derived from their
  XUID.
- **Older Java clients** (`multiversion.versions`): 1.20.5 to 26.2 by default, translated to
  26.3 and back; clients of the server's own version pay nothing.
- **vanillite-proxy**: a Velocity-compatible proxy (modern, BungeeCord or no forwarding, server
  switches without reconnect, fallback when a server goes down, the BungeeCord plugin channel,
  `/server`, `/glist`, `/send`, `/alert`, `/proxy reload`, a console) in front of Vanillite,
  Paper and Spigot servers, with the Bedrock frontend built in. Build it with
  `cargo build --release -p vanillite-proxy`; it reads `proxy.toml` from its folder. On the
  server, set `proxy.velocity-secret`. It has no plugin API yet.

## Status and known limitations

Vanillite is a beta for Minecraft 26.3. Known limitations:

- Bedrock players cannot yet use containers other than their inventory (chests, crafting
  tables, furnaces), craft or interact with entities, and do not see Java particles.
- Some benchmark scenarios are still slower than Paper (see [Measured](#measured)).
- Worlds of Minecraft versions before 26.1 should be opened once with the Vanilla 26.3 server
  first, which upgrades their data.
- The release archives contain only the server; the proxy is built from source.

## Building from source

The Rust toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml); rustup installs
it on the first build.

```sh
git clone https://github.com/hthoene/vanillite.git && cd Vanillite
cargo build --release -p vanillite          # target/release/vanillite
cargo xtask dev                              # build and run a dev server in ./run
```

Tests, code generation, parity goldens, benchmarks and releases are described in the
[development guide](docs/development.md).

## Documentation

- **User guide:** [docs.vanillite.org](https://docs.vanillite.org): installation, administration,
  commands and features.
- **In this repository:** [docs/](docs/README.md): the configuration reference, world formats,
  plugin development, architecture and development.

## Contributing

Issues and pull requests are welcome; base pull requests on `dev`. Before you push, run the
quick check:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask egg --check
```

Behaviour changes need a parity scenario against Vanilla (see the
[development guide](docs/development.md#parity-tests)). Contributions are licensed under
Apache 2.0 like the project.

## License

Copyright 2026 Hannes Thöne. Licensed under the [Apache License, Version 2.0](LICENSE); see
[NOTICE](NOTICE) for third-party attributions.

Vanillite is not an official Minecraft product and is not approved by or associated with
Mojang or Microsoft. It contains no Minecraft code or assets: it downloads Mojang's server jar
on first start and reads the Vanilla data from it.
