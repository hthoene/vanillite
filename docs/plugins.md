# Plugin development

Vanillite plugins are native shared libraries. They talk to the server only through the C ABI
of [`vanillite-abi`](../crates/vanillite-abi) (plain C types, versioned tables), so a plugin
depends neither on the server's compiler nor on its dependencies. The Rust SDK
[`vanillite-plugin`](../crates/vanillite-plugin) is a safe API over that ABI; any language that
can export a C function can implement the ABI directly.

Paper and Spigot plugins run too: see [Paper and Spigot plugins](#paper-and-spigot-plugins).

The complete example is [`examples/hello-plugin`](../examples/hello-plugin): a command with
suggestions, a join message from its configuration, persistent player data, cancelled events,
scheduled and background tasks, bulk block access and a chunk generator.

## A first plugin

`Cargo.toml`:

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
# The SDK of the server version you build for (a release tag, or a path to a checkout).
vanillite-plugin = { git = "https://github.com/hthoene/vanillite", tag = "v0.1.0-beta.3+mc26.3" }
```

`src/lib.rs`:

```rust
use vanillite_plugin::prelude::*;

struct Greeter;

impl Plugin for Greeter {
    fn load(registrar: &mut Registrar<Self>) -> Result<Self, Error> {
        registrar.on_join(priority::NORMAL, |_, ctx, event| {
            let _ = ctx
                .server()
                .send_message(event.player(), format!("Welcome, {}!", event.name()));
        });
        registrar.command(Command::literal("greet").executes(|_, ctx, invocation| {
            invocation.reply(ctx, "Hello!");
            Ok(1)
        }));
        Ok(Greeter)
    }
}

vanillite_plugin::export_plugin!(Greeter);
```

`plugin.toml`:

```toml
id = "greeter"          # [a-z0-9_-], 1 to 64 characters; also the data folder's name
name = "Greeter"
version = "0.1.0"
api = "1.1"             # plugin ABI version needed, major.minor
library = "my_plugin"   # base name: libmy_plugin.so, my_plugin.dll, libmy_plugin.dylib
```

Build with `cargo build --release` and put `plugin.toml` and the library into
`plugins/greeter/` of the server folder. The server loads every plugin in `plugins/` at
startup (`plugins.enabled`).

## Packages and `plugin.toml`

A package is a folder or a zip file with the extension `.vlp`, holding `plugin.toml` and the
library in `native/<target-triple>/` (one per platform, e.g.
`native/x86_64-unknown-linux-gnu/libmy_plugin.so`). A folder may also hold the library next to
`plugin.toml`, which suits development builds. Libraries for another platform are refused.

| Key | Required | Meaning |
|---|---|---|
| `id` | yes | Unique id; the plugin's data folder is `plugins/<id>/`. |
| `name`, `version` | yes | Display name and the plugin's own version. |
| `api` | yes | ABI version needed, `major.minor`. A plugin built for `1.1` runs on a `1.4` server; one that needs `1.4` is refused by a `1.1` server. |
| `library` | yes | Base name of the library, without prefix and extension. |
| `depends` | no | Plugin ids that must be loaded first; missing ones stop this plugin. |
| `soft-depends` | no | Plugin ids loaded first when present. |
| `threading` | no | `serialized` (default): never called on two threads at once. `concurrent`: the plugin is thread-safe and may be called from several world threads at once. |
| `uses` | no | What the plugin uses (`files`, `network`, `admin`): shown to the admin, not enforced. |
| `provides` | no | Slots the plugin may fill, e.g. `chunk-generator`. |
| `description`, `authors`, `website` | no | Shown by `/version <plugin>` (`authors` is a list of names). |

## Lifecycle and threads

- `Plugin::load` registers event handlers, commands, permissions and providers; registries are
  writable only there. `Plugin::enable` runs when the worlds exist, `Plugin::disable` at
  shutdown. Plugins are never unloaded.
- There is no main thread. Event handlers and commands run on the thread that owns the player
  or world (one per dimension when dimensions tick in parallel).
- A handler gets a `Ctx` that is valid only during the call. World access (blocks, positions,
  entity data) goes through it and works only on the owner of that world. `Server` (from
  `ctx.server()` or `registrar.server()`) may be kept and used from any thread.
- Work on another world or player is scheduled on its owner with `Server::schedule`;
  `run_later` delays it. Slow work such as file or network I/O belongs in `Server::run_async`
  or `run_async_timer`, off the tick.
- Players and entities are identified by `Uuid`, worlds by `World`, blocks by their state text
  (`minecraft:oak_log[axis=y]`), never by protocol ids, so plugins keep working across
  Minecraft updates. Bulk block access (`blocks`, `set_blocks`) and providers use numeric block
  state ids resolved with `Server::block_state_id` at runtime.
- Panics are contained: a plugin that panics in a handler is disabled and the server keeps
  running. Build with `panic = "unwind"` (the default); the server refuses other builds.

## What the SDK offers

| Area | API |
|---|---|
| Events | `on_login`, `on_join`, `on_leave`, `on_chat`, `on_command`, `on_players_moved` (one batch per world and tick), `on_interact`, `on_block_break`, `on_block_place`, `on_entity_damage`, `on_death`, `on_entity_death`, `on_respawn`, `on_change_world`, `on_game_mode_change`, `on_bed_enter`, `on_bed_leave`, `on_spawn_change`; each with a priority (`priority::*`), many cancellable. |
| Commands | `Registrar::command` with Brigadier trees (`Command::literal`, `Command::argument`, `ArgType`), suggestions and permissions; `Server::run_command` runs any command. |
| Players | messages, action bar, titles, tab list, kick, permission checks, broadcast. |
| Worlds | worlds and their names, single and bulk block reads and writes, player positions; extra worlds with their own folder (`create_world` with a `WorldCreator`, `load_world`, `unload_world`, `world_details`, `on_world_load`, `on_world_unload`) and players moved between worlds from any thread (`move_player`). |
| Data | persistent entity and player data (`entity_data`, `set_entity_data`; saved with the entity), the plugin's `config.toml` (`Registrar::config` writes the defaults, `Server::config_string` and siblings read it), its data folder (`Server::data_dir`), and the server's rules (`Server::rule`). |
| Providers | `Registrar::chunk_generator` replaces the world generator of chosen dimensions (declare `provides = ["chunk-generator"]`); `Registrar::permission_provider` answers permission checks. |
| Logging | `vanillite_plugin::info!`, `debug!` and siblings, prefixed with the plugin's id. |

The API reference is the crate documentation: `cargo doc -p vanillite-plugin --open`.

## Development and operation

- **Reload while developing:** with `plugins.development = true`, the console command
  `vanillite plugin reload <id>` loads a new build at runtime. The old build stays in memory;
  `Plugin::save_state` and `restore_state` move the state across. Not for production servers.
- **Crash safety:** a crash inside a plugin's code (a fatal signal or a watchdog kill) is
  written to `plugins/.crashed`, and the plugin is skipped at the next starts until its library
  changes or that file is removed (`plugins.safe-mode-after-crash`).
- **Pinning:** `plugins.pins = "id=sha256,..."` refuses libraries with another hash.
- Plugins are trusted code with the server's rights; install only plugins you trust.

## Paper and Spigot plugins

Bukkit, Spigot and Paper plugins (`plugins/*.jar` with a `plugin.yml` or `paper-plugin.yml`) run
on the Bukkit layer [vanillite-bukkit](https://github.com/hthoene/vanillite-bukkit): an ordinary
native plugin (`plugins/bukkit/`) that hosts a Java virtual machine. Put the jars into
`plugins/` and start the server:

1. At startup the server finds the jars. If the layer is not installed it takes the layer
   release pinned for this Vanillite version (`crates/vanillite-server/bukkit-layer.txt`):
   `plugins/vanillite-bukkit-<version>-<target>.tar.gz` if it lies there, else a download from
   the layer's GitHub releases into `plugins/.cache/`. The archive must match the pinned
   SHA-256; it is unpacked into `plugins/bukkit/` (the layer's `config.toml` and downloaded
   libraries stay). Layer releases are built for Linux (x86_64, aarch64), Windows (x86_64) and
   macOS (aarch64, x86_64).
2. If the layer would find no Java 25 runtime (`jvm.java-home` in `plugins/bukkit/config.toml`,
   `plugins/bukkit/runtime/`, `JAVA_HOME`, `java` on the `PATH`), Eclipse Temurin 25 (JRE) for
   the machine is downloaded from Adoptium into `plugins/bukkit/runtime/` (`.zip` on Windows,
   `.tar.gz` elsewhere), checked against the SHA-256 Adoptium publishes.
3. The layer loads with the other plugins and loads the jars.

`plugins.bukkit.auto-setup = false` turns the downloads off (offline or air-gapped servers): the
log then names the archive with its SHA-256 and the folder for the runtime. Without Bukkit jars
none of this runs.

The static Linux builds (release archives, Docker image) cannot load libraries. They ship
`vanillite-gnu`, the same commit linked against glibc 2.28, and start it in their place when
`plugins/` holds packages or Bukkit jars (`plugins.runtime`: `auto`, `gnu` or `static`). Both
must come from the same build (checked with `vanillite version`); if `vanillite-gnu` is missing
or another build, the server starts without plugins and says why.

## ABI versioning

`ABI_MAJOR` changes only on a breaking change; `ABI_MINOR` grows when functions, events or
record fields are appended. Every table and record starts with its size, so each side reads only
what the other knows. The test `abi_snapshot` in `vanillite-abi` compares every declaration with
the recorded `abi.snapshot` and fails on a breaking change without a major version bump.
