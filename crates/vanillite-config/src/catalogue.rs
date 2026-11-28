//! The rule catalogue: every deviation from Vanilla and every Vanillite setting, declared once.
//!
//! [`ServerRules`] apply to the whole server, [`WorldRules`] can be overridden per world in
//! `[worlds."<dimension>"]`. Both read their keys from the top level of `vanillite.toml`, so a
//! group key must be unique across both sets. Performance knobs live under `performance.*`.

use crate::rule::{
    AntiXrayMode, DisabledOr, PluginRuntime, RedstoneImplementation, StorageLayout, VanillaOr,
    WorldFormat,
};
use crate::rules;

rules! {
    /// Rules and settings of server scope.
    pub struct ServerRules {
        /// The tick loop.
        tick: TickRules = "performance.tick" {
            /// Dimensions tick in parallel, each on its own thread (decision D2). Cross-dimension
            /// effects then apply at the end of the tick instead of immediately: an entity
            /// entering a portal from the overworld arrives one tick later, `execute in` other
            /// dimensions runs at the end of the tick. `false` (the `vanilla` and `technical`
            /// profiles) ticks the dimensions one after another in Vanilla's exact order.
            parallel_dimensions: bool = "parallel-dimensions", Performance, restart = true,
                vanilla = false, paper = true, spigot = true, performance = true;
        }
        /// Thread counts.
        threads: ThreadRules = "performance.threads" {
            /// Worker threads of the compute pool (world generation, lighting, saving, parallel
            /// tick phases). 0 = the cores the server may use (`performance.budget.cpu`, the
            /// cgroup CPU quota and the CPU affinity): the workers run at a lower OS priority
            /// (`compute-nice`), so the tick threads still get a core whenever they need one.
            compute: u16 = "compute", Setting, range = [0, 1024], restart = true, vanilla = 0;
            /// Nice value of the compute workers (Linux; the tick threads stay at 0). Higher
            /// values leave the CPU to the tick threads sooner: 10 gives a tick thread sharing a
            /// core with a worker about 90 % of it. 0 = the same priority as the tick threads.
            compute_nice: u8 = "compute-nice", Setting, range = [0, 19], restart = true,
                vanilla = 10;
            /// I/O threads of the network (connections, encryption, compression of outgoing
            /// packets). 0 = one per two cores, at least 2; 1 with less than two cores, so the
            /// tick keeps the only core.
            network: u16 = "network", Setting, range = [0, 64], restart = true, vanilla = 0;
            /// Threads that share a parallel phase of a world's tick (in crowds: the entity
            /// packets, the delivery of sounds and particles, the advancement checks; with players
            /// spread out: the ticking checks of the chunks for natural spawning and random
            /// ticks; the tiles of `region-threading`), the tick thread included. 0 = one per core the server may
            /// use; 1 = the tick thread alone. The other threads are tick helpers at the tick
            /// threads' OS priority (not the compute workers, which run below it). The result is
            /// the same with any number, only the speed differs.
            tick_phases: u16 = "tick-phases", Setting, range = [0, 1024], restart = true,
                vanilla = 0;
        }
        /// What the hardware allows. With less than two cores, background work (generation,
        /// light, saving) runs only between ticks and below the CPU quota, so the tick stays
        /// smooth; caches shrink under a memory limit. Only when work runs changes, never its
        /// result.
        budget: BudgetRules = "performance.budget" {
            /// CPU cores the server may use. 0 = detect: the cgroup CPU quota (`cpu.max`, Docker
            /// `--cpus`, Pterodactyl's CPU limit) and the CPU affinity.
            cpu: f64 = "cpu", Setting, range = [0.0, 1024.0], restart = true, vanilla = 0.0;
            /// Memory in MiB the server should stay far below; caches are sized from it. 0 =
            /// detect: the cgroup memory limit (`memory.max`), none without a container.
            memory: u32 = "memory", Setting, range = [0, 16_777_216], restart = true, vanilla = 0;
        }
        /// Memory of an empty server.
        memory: MemoryRules = "performance.memory" {
            /// Seconds without players after which the server frees what it can build again:
            /// the tables of the world generators, structure templates and caches. They come
            /// back in the background when a player joins (new chunks then start a moment
            /// later, chunks from disk do not wait). In-game behaviour does not change;
            /// disabled keeps everything in memory.
            release_when_empty: DisabledOr<u32> = "release-when-empty", Setting,
                range = [1, 86_400], vanilla = DisabledOr::Value(60);
        }
        /// Saving.
        autosave: AutosaveRules = "performance.autosave" {
            /// Ticks between autosaves. Vanilla saves every 5 minutes of game time.
            interval: VanillaOr<u32> = "interval", Setting, range = [20, 1_728_000],
                vanilla = VanillaOr::Vanilla;
        }
        /// World storage.
        storage: StorageRules = "storage" {
            /// Folder layout of the dimensions: `"vanilla"` keeps every dimension in
            /// `<level>/dimensions/` like Vanilla and Paper; `"spigot"` puts the nether into
            /// `<level>_nether/` and the end into `<level>_the_end/` like Spigot and CraftBukkit,
            /// so the world can be opened by Spigot without conversion. A world found in the
            /// other layout is moved into this one once at startup.
            layout: StorageLayout = "layout", Setting, restart = true,
                vanilla = StorageLayout::Vanilla, spigot = StorageLayout::Spigot;
            /// Format of the chunk, entity and POI files of new worlds: `"vanillite"`, the own
            /// format (about 1.75 × smaller than Anvil, loads and saves several times faster,
            /// `vanillite export` writes a Vanilla copy), or `"anvil"`, Vanilla's region files.
            /// An existing world keeps its format unless `convert-world` is set.
            world_format: WorldFormat = "world-format", Setting, restart = true,
                vanilla = WorldFormat::Vanillite;
            /// Converts an existing world into `world-format` at startup, before it loads.
            /// The new files are read back and compared before the old ones are removed; if
            /// that fails, the world stays as it is and the server starts with it.
            convert_world: bool = "convert-world", Setting, restart = true, vanilla = false;
            /// After a conversion the original files are moved into
            /// `world-format-backup/<time>/` in the server folder instead of being deleted.
            keep_converted_files: bool = "keep-converted-files", Setting, restart = true,
                vanilla = true;
            /// zstd level of chunk records in the own format, 1 (fastest) to 19 (smallest).
            /// Takes effect for chunks saved from then on.
            compression_level: u8 = "compression-level", Performance, range = [1, 19],
                restart = true, vanilla = 3;
            /// Disk IO threads shared by all dimensions. They only read and write files;
            /// compression runs on the compute pool. More help on network volumes with high
            /// latency.
            io_threads: u16 = "io-threads", Performance, range = [1, 16], restart = true,
                vanilla = 2;
            /// Seconds after which saves of unloaded chunks are made durable (synced when
            /// `sync-chunk-writes` is on). Save rounds and `save-all flush` commit at their end.
            commit_interval: u32 = "commit-interval", Performance, range = [5, 300],
                vanilla = 30;
            /// Reads the neighbours of a loaded chunk in its 4 × 4 tile along when they lie
            /// next to it in the file, and keeps them for a short time: fewer disk reads for
            /// moving players.
            read_ahead: bool = "read-ahead", Performance, restart = true, vanilla = true;
            /// MiB of chunk records read ahead kept per dimension; under a memory limit
            /// (`performance.budget.memory`) at most 1/32 of it.
            read_cache: u32 = "read-cache", Performance, range = [0, 65_536], restart = true,
                vanilla = 64;
            /// Region files kept open per dimension while unused; files in use always stay
            /// open.
            max_open_files: u32 = "max-open-files", Performance, range = [16, 65_536],
                restart = true, vanilla = 256;
            /// MiB of saved chunks waiting for the disk per dimension above which no more
            /// saves are queued: changed chunks stay loaded and save rounds pause until the
            /// disk caught up. Bounds RAM on slow disks.
            max_pending_writes: u32 = "max-pending-writes", Performance, range = [4, 65_536],
                vanilla = 64;
        }
        /// Autosave without tick spikes: a save round captures the changed chunks after the
        /// world tick from the tick's remaining time, spread over as many ticks as needed.
        storage_autosave: StorageAutosaveRules = "storage.autosave" {
            /// Most microseconds of one tick spent capturing chunks for a save round; only the
            /// time left in the tick is used. ~380 chunks fit into the default.
            tick_budget: u32 = "tick-budget", Performance, range = [100, 10_000], vanilla = 500;
            /// Chunks captured per tick even when the tick has no time left, so a round ends
            /// on an overloaded server.
            min_chunks_per_tick: u32 = "min-chunks-per-tick", Performance, range = [1, 1024],
                vanilla = 16;
        }
        /// Automatic backups: zip archives of the world, made in the background from a fresh
        /// save while the server runs. Vanilla has none.
        backup: BackupRules = "backup" {
            /// Minutes between backups; disabled makes none. The first one is made one interval
            /// after the start.
            interval: DisabledOr<u32> = "interval", Setting, range = [1, 525_600],
                vanilla = DisabledOr::Disabled;
            /// Backups kept; older ones are deleted after a new one is complete. 0 keeps all.
            keep: u32 = "keep", Setting, range = [0, 10_000], vanilla = 10;
            /// Folder of the archives, relative to the server folder.
            directory: String = "directory", Setting, vanilla = "backups".to_owned();
            /// Deflate level of the archives, 1 (fast) to 9 (small); 0 stores without
            /// compression. Region files are compressed already, so higher levels gain little.
            compression_level: u8 = "compression-level", Performance, range = [0, 9],
                vanilla = 1;
        }
        /// The watchdog that detects a hanging server. The hard limit is `max-tick-time` in
        /// `server.properties`.
        watchdog: WatchdogRules = "watchdog" {
            /// Seconds without a finished tick after which a first warning is logged.
            early_warning_after: DisabledOr<u32> = "early-warning-after", Setting,
                range = [1, 3600], vanilla = DisabledOr::Value(10);
            /// Seconds between further warnings while the server hangs.
            warning_interval: u32 = "warning-interval", Setting, range = [1, 3600], vanilla = 5;
        }
        /// Limits against abuse.
        security: SecurityRules = "security" {
            /// Logins accepted into the game per tick. Protects the tick from join floods.
            max_joins_per_tick: u32 = "max-joins-per-tick", Security, range = [1, 1000],
                vanilla = 5;
        }
        /// Remote administration: RCON (`enable-rcon`, `rcon.port`, `rcon.password`) and Query
        /// (`enable-query`, `query.port`) of `server.properties`, with the limits of SEC-24.
        admin: AdminRules = "admin" {
            /// Address RCON listens on. Vanilla listens on `server-ip` (all interfaces when
            /// empty); Vanillite listens on the loopback interface unless this is changed.
            /// Empty = `server-ip` like Vanilla.
            rcon_bind: String = "rcon-bind", Security, restart = true,
                vanilla = String::from("127.0.0.1");
            /// Shortest `rcon.password` accepted; with a shorter one RCON stays off (Vanilla
            /// only refuses an empty password).
            rcon_min_password_length: u32 = "rcon-min-password-length", Security,
                range = [1, 1024], restart = true, vanilla = 8;
            /// Failed RCON logins per IP address and minute; after that, connections of the
            /// address are closed at once until the minute is over (Vanilla: no limit).
            rcon_max_failed_logins: u32 = "rcon-max-failed-logins", Security,
                range = [1, 10_000], restart = true, vanilla = 5;
            /// RCON connections open at the same time (Vanilla: no limit).
            rcon_max_connections: u32 = "rcon-max-connections", Security, range = [1, 1024],
                restart = true, vanilla = 16;
            /// Address Query listens on. Vanilla listens on `server-ip`; Vanillite listens on
            /// the loopback interface unless this is changed (set `0.0.0.0` for public server
            /// lists). Empty = `server-ip` like Vanilla.
            query_bind: String = "query-bind", Security, restart = true,
                vanilla = String::from("127.0.0.1");
            /// Query packets answered per IP address and second; more are dropped (Vanilla:
            /// no limit). Keeps the UDP listener from being used for reflection.
            query_max_packets_per_second: u32 = "query-max-packets-per-second", Security,
                range = [1, 10_000], restart = true, vanilla = 20;
        }
        /// Limits of command parsing and tab completion (SEC-04, SEC-11).
        commands: CommandRules = "commands" {
            /// Longest command line that is parsed (Vanilla: the packet's 32767 characters).
            max_command_length: u32 = "max-command-length", Security, range = [16, 32767],
                vanilla = 32767;
            /// Longest tab completion input; longer requests get no suggestions (Vanilla
            /// accepts 32500 characters).
            max_suggestion_input: u32 = "max-suggestion-input", Security, range = [16, 32500],
                vanilla = 2048;
            /// Suggestions per answer.
            max_suggestions: u32 = "max-suggestions", Security, range = [1, 10000],
                vanilla = 1000;
            /// Characters of a message argument (`say`, `msg`, `kick`).
            max_message_length: u32 = "max-message-length", Security, range = [16, 32767],
                vanilla = 256;
            /// Nesting depth of SNBT in arguments.
            max_nbt_depth: u32 = "max-nbt-depth", Security, range = [1, 512], vanilla = 512;
            /// Nodes of an NBT path.
            max_nbt_path_nodes: u32 = "max-nbt-path-nodes", Security, range = [1, 4096],
                vanilla = 256;
            /// Options inside one entity selector `[...]`.
            max_selector_options: u32 = "max-selector-options", Security, range = [1, 1024],
                vanilla = 64;
            /// Entity selectors inside one message argument.
            max_message_selectors: u32 = "max-message-selectors", Security, range = [0, 256],
                vanilla = 16;
            /// Nested redirects followed while parsing one command.
            max_redirect_depth: u32 = "max-redirect-depth", Security, range = [1, 1024],
                vanilla = 64;
            /// Paper's `/version` (also `/ver` and `/about`): the server's version or that of
            /// a plugin, for everyone with `bukkit.command.version` (all players by default);
            /// Vanilla's version dump moves to `/minecraft:version`. Off, `/version` is
            /// Vanilla's command for operators. `/plugins` is always there.
            bukkit_commands: bool = "bukkit-commands", Setting, restart = true,
                vanilla = false, paper = true, spigot = true;
        }
        /// Survival: time and weather.
        survival: SurvivalRules = "performance.survival" {
            /// Ticks between the game time packets every player gets (Vanilla: every second).
            /// Clients run the clocks themselves in between; a longer interval saves traffic
            /// with many players, the game time on the client may then lag behind a little.
            time_sync_interval: u32 = "time-sync-interval", Performance, range = [1, 1200],
                vanilla = 20, performance = 60;
        }
        /// Redstone.
        redstone: RedstoneRules = "performance.redstone" {
            /// The redstone wire implementation. "vanilla" is tick-exact. "alternate-current"
            /// follows Alternate Current (by Space Walker, MIT): a change computes the power of
            /// the whole connected wire network in one pass and then updates every block
            /// around the changed wires once, instead of Vanilla's recursive updates with
            /// thousands of redundant ones. Final wire powers are the same as Vanilla's, but
            /// the order of the neighbour and shape updates differs: it follows the direction
            /// power flows in (front, back, right, left, down, up of each wire, wires nearest
            /// the change first) instead of Vanilla's location-dependent hash order, so a
            /// circuit behaves the same in every rotation and place, but contraptions that
            /// depend on Vanilla's update order (some piston and observer builds) can behave
            /// differently. Wires powered only by blocks update north first. Only the
            /// `performance` profile uses it (Paper's own default is its vanilla wire); set
            /// "vanilla" there if such a contraption breaks.
            implementation: RedstoneImplementation = "implementation", Performance,
                restart = true, vanilla = RedstoneImplementation::Vanilla,
                performance = RedstoneImplementation::AlternateCurrent;
        }
        /// Development and test helpers. Never needed on a production server.
        debug: DebugRules = "debug" {
            /// In-process fake players (`TestBot`, `TestBot2`, ...) that join over virtual
            /// connections, walk in a circle around the spawn and sneak now and then, so a
            /// single tester sees other players. Virtual connections need no authentication,
            /// so keep this at 0 outside test servers.
            test_bots: u8 = "test-bots", Setting, range = [0, 16], restart = true, vanilla = 0;
            /// Records the players' sessions in the overworld for `cargo xtask parity replay`,
            /// which plays them again in Vanilla and Vanillite and compares both tick by tick.
            /// When the first player joins, the world is saved and the overworld's chunks are
            /// copied (the overworld tick waits for the copy); then every tick the players'
            /// serverbound play packets are written, until the last player has left. Chat,
            /// commands, signs, books and plugin messages are left out, and players get
            /// anonymous ids instead of their names, addresses and UUIDs. For test servers.
            session_recording: bool = "session-recording", Setting, restart = true,
                vanilla = false;
            /// Folder of the session recordings, relative to the server folder: one folder per
            /// session (`<start time>/` with `world/`, `meta.json`, `packets.jsonl`).
            session_recording_dir: String = "session-recording-dir", Setting, restart = true,
                vanilla = "recordings".to_owned();
        }
        /// Player info forwarding from a proxy in front of the server (Velocity "modern"
        /// forwarding, also used by `vanillite-proxy`).
        proxy: ProxyRules = "proxy" {
            /// Shared secret of modern forwarding (the proxy's `forwarding.secret`). Empty turns
            /// forwarding off. With a secret, players' identities and addresses come from the
            /// proxy, signed with it; `online-mode` then only applies to direct connections.
            velocity_secret: String = "velocity-secret", Setting, restart = true,
                vanilla = String::new();
            /// With forwarding on, also accept players who connect directly instead of through
            /// the proxy (authenticated as usual). Off: they are refused.
            allow_direct_connections: bool = "allow-direct-connections", Setting, restart = true,
                vanilla = false;
        }
        /// Clients of older Minecraft versions (`vanillite-multiversion`): their packets are
        /// translated to the server's version and back, like with ViaVersion.
        multiversion: MultiversionRules = "multiversion" {
            /// Older versions whose clients may join: `"all"` (every version this build
            /// translates, currently 1.20.5 to 26.2), `"none"`, or a comma-separated list of
            /// releases such as `"1.21.4, 26.2"` (a release accepts every release of its
            /// protocol). Clients of the server's own version are not translated and pay
            /// nothing.
            versions: String = "versions", Setting, restart = true,
                vanilla = String::from("all");
        }
        /// Native plugins in `plugins/`: packages with a `plugin.toml` and a library built
        /// against the plugin API. Plugins are trusted code with the server's rights.
        plugins: PluginRules = "plugins" {
            /// Load the plugins in `plugins/`.
            enabled: bool = "enabled", Setting, restart = true, vanilla = true;
            /// After a crash inside a plugin's code (a fatal signal or a watchdog kill), skip
            /// that plugin at the next starts until its library changes or `plugins/.crashed`
            /// is removed, so a supervisor (Docker, systemd) brings the server back.
            safe_mode_after_crash: bool = "safe-mode-after-crash", Setting, restart = true,
                vanilla = true;
            /// Development: the console command `vanillite plugin reload <id>` loads a new
            /// build of a plugin at runtime and moves its state. Old builds stay in memory.
            /// Not for production servers.
            development: bool = "development", Setting, restart = true, vanilla = false;
            /// Pinned SHA-256 hashes of plugin libraries, `id=hex,id=hex`. A plugin whose
            /// library has another hash is refused.
            pins: String = "pins", Security, restart = true, vanilla = String::new();
            /// Which build loads the plugins. The Linux release archives and the Docker image
            /// run a static build, which cannot load libraries, and ship `vanillite-gnu` (built
            /// from the same commit against glibc) next to it: `"auto"` starts `vanillite-gnu`
            /// in its place when `plugins/` holds plugins (packages or Paper/Spigot jars),
            /// `"gnu"` always, `"static"` never (plugins are then not loaded). Other builds load
            /// plugins themselves and ignore this.
            runtime: PluginRuntime = "runtime", Setting, restart = true,
                vanilla = PluginRuntime::Auto;
        }
        /// Paper and Spigot plugins (`plugins/*.jar` with a `plugin.yml` or
        /// `paper-plugin.yml`). They run on the Bukkit layer (vanillite-bukkit), a native plugin
        /// that hosts a Java virtual machine; without such jars nothing of it exists.
        bukkit: BukkitRules = "plugins.bukkit" {
            /// When `plugins/` holds Paper or Spigot plugins and the layer is not installed,
            /// download the layer release made for this Vanillite version into
            /// `plugins/bukkit/` and, without a Java 25 runtime, Eclipse Temurin into
            /// `plugins/bukkit/runtime/` (both checked by SHA-256). Off (offline servers): the
            /// log says what to download and where to put it.
            auto_setup: bool = "auto-setup", Setting, restart = true, vanilla = true;
        }
        /// Bedrock Edition players (`vanillite-bedrock`): a RakNet listener on UDP that
        /// translates Bedrock clients into virtual connections. The address is `server-ip`.
        bedrock: BedrockRules = "bedrock" {
            /// Accept Bedrock players.
            enabled: bool = "enabled", Setting, restart = true, vanilla = false;
            /// UDP and TCP port (Bedrock 1.26.60 and newer join over NetherNet, which needs
            /// both). Bedrock's default is 19132.
            port: u16 = "port", Setting, range = [1, 65535], restart = true, vanilla = 19132;
            /// Require an Xbox Live login. Off accepts any identity a client claims: only for
            /// tests on closed networks.
            online_mode: bool = "online-mode", Security, restart = true, vanilla = true;
            /// Oldest Bedrock network protocol that may join. 0 accepts every Bedrock version
            /// this build supports (listed at startup); e.g. 2225 admits only Bedrock 1.26.60
            /// and newer.
            min_protocol: u32 = "min-protocol", Setting, range = [0, 100_000], restart = true,
                vanilla = 0;
            /// Prefix of Bedrock players' Java names, so they never collide with Java accounts.
            /// Bedrock players get the UUID `00000000-0000-0000-xxxx-xxxxxxxxxxxx` of their XUID
            /// (like Floodgate); whitelist and operators use that UUID.
            name_prefix: String = "name-prefix", Setting, restart = true,
                vanilla = String::from(".");
            /// Bedrock connections per IP address.
            max_connections_per_ip: u32 = "max-connections-per-ip", Security,
                range = [1, 1000], restart = true, vanilla = 8;
            /// Packets per second a Bedrock client may send before it is kicked.
            max_packets_per_second: u32 = "max-packets-per-second", Security,
                range = [50, 100_000], restart = true, vanilla = 1000;
            /// Largest chunk radius granted to Bedrock clients.
            max_view_distance: u8 = "max-view-distance", Performance, range = [2, 32],
                restart = true, vanilla = 12;
            /// Translated chunk sections kept for all Bedrock players (each section is
            /// translated once for every viewer). About 4 KiB each.
            chunk_cache_sections: u32 = "chunk-cache-sections", Performance,
                range = [0, 1_048_576], restart = true, vanilla = 16_384;
            /// Threads of the Bedrock frontend.
            threads: u16 = "threads", Performance, range = [1, 64], restart = true, vanilla = 2;
        }
        /// Chat. Spam thresholds are `chat-spam-threshold-seconds` and
        /// `command-spam-threshold-seconds` in `server.properties`.
        chat: ChatRules = "chat" {
            /// Signed messages a player may have received without acknowledging them before it
            /// is kicked (`too_many_pending_chats`).
            max_pending_messages: u32 = "max-pending-messages", Security,
                range = [64, 65536], vanilla = 4096;
        }
    }
}

rules! {
    /// Rules of one world. Overridable per world in `[worlds."<dimension>"]`.
    pub struct WorldRules {
        /// Deterministic Vanilla mechanics that are commonly called exploits (decision D1).
        /// `true` keeps Vanilla's behaviour.
        exploits: ExploitRules = "exploits" {
            /// TNT can be duplicated with pistons and slime or honey blocks.
            tnt_duplication: bool = "tnt-duplication", Exploit, vanilla = true, paper = false;
            /// Carpets can be duplicated with pistons.
            carpet_duplication: bool = "carpet-duplication", Exploit, vanilla = true, paper = false;
            /// Rails can be duplicated with pistons.
            rail_duplication: bool = "rail-duplication", Exploit, vanilla = true, paper = false;
            /// Pistons without a head can be created.
            headless_pistons: bool = "headless-pistons", Exploit, vanilla = true, paper = false;
            /// Bedrock and end portal frames can be broken with pistons and explosions.
            break_unbreakable_blocks: bool = "break-unbreakable-blocks", Exploit,
                vanilla = true, paper = false;
            /// Falling blocks can be duplicated through end portals.
            gravity_block_duplication: bool = "gravity-block-duplication", Exploit,
                vanilla = true, paper = false;
            /// String can be duplicated with tripwire mechanics.
            string_duplication: bool = "string-duplication", Exploit, vanilla = true, paper = false;
            /// End crystals can be made invulnerable.
            invulnerable_end_crystals: bool = "invulnerable-end-crystals", Exploit,
                vanilla = true, paper = false;
        }
        /// View and simulation distance of this world.
        distances: DistanceRules = "performance.distances" {
            /// View distance in chunks; "vanilla" uses `view-distance` of server.properties.
            view_distance: VanillaOr<u8> = "view-distance", Performance, range = [2, 32],
                vanilla = VanillaOr::Vanilla;
            /// Simulation distance in chunks; "vanilla" uses `simulation-distance` of
            /// server.properties.
            simulation_distance: VanillaOr<u8> = "simulation-distance", Performance,
                range = [2, 32], vanilla = VanillaOr::Vanilla;
        }
        /// Entity tracking: how far away players see entities. Ranges are in blocks and still
        /// capped by the view distance. "vanilla" uses the entity type's own range scaled by
        /// `entity-broadcast-range-percentage`. Entity types are grouped by their mob category:
        /// `monster`, the animal categories (creatures, ambient and water mobs) and `misc`.
        tracking: TrackingRules = "performance.entity-tracking" {
            /// Tracking range of players.
            player_range: VanillaOr<u16> = "player-range", Performance, range = [8, 512],
                restart = true, vanilla = VanillaOr::Vanilla, paper = VanillaOr::Value(128),
                spigot = VanillaOr::Value(128), performance = VanillaOr::Value(128);
            /// Tracking range of animals, ambient and water mobs.
            animal_range: VanillaOr<u16> = "animal-range", Performance, range = [8, 512],
                restart = true, vanilla = VanillaOr::Vanilla, paper = VanillaOr::Value(96),
                spigot = VanillaOr::Value(96), performance = VanillaOr::Value(80);
            /// Tracking range of monsters.
            monster_range: VanillaOr<u16> = "monster-range", Performance, range = [8, 512],
                restart = true, vanilla = VanillaOr::Vanilla, paper = VanillaOr::Value(96),
                spigot = VanillaOr::Value(96), performance = VanillaOr::Value(96);
            /// Tracking range of everything else (items, projectiles, vehicles, decorations).
            misc_range: VanillaOr<u16> = "misc-range", Performance, range = [8, 512],
                restart = true, vanilla = VanillaOr::Vanilla, paper = VanillaOr::Value(96),
                spigot = VanillaOr::Value(96), performance = VanillaOr::Value(64);
            /// Overload protection: while a world tick takes longer than this many
            /// milliseconds (smoothed), movement updates of tracked entities (position,
            /// rotation, head) are sent less often, up to `overload-max-interval-scale` times
            /// their Vanilla interval, and back at the Vanilla rate once ticks are fast again.
            /// Spawns, removals and entity data are never delayed. "disabled" keeps Vanilla's
            /// intervals: an overloaded server then slows down like Vanilla.
            overload_tick_millis: DisabledOr<u32> = "overload-tick-millis", Performance,
                range = [1, 1000], vanilla = DisabledOr::Disabled,
                performance = DisabledOr::Value(45);
            /// Largest factor on the movement update interval under overload.
            overload_max_interval_scale: u32 = "overload-max-interval-scale", Performance,
                range = [1, 20], vanilla = 4;
        }
        /// Server-side checks of player movement (Vanilla's `ServerGamePacketListenerImpl`).
        movement: MovementRules = "movement" {
            /// Players that move faster than Vanilla allows are teleported back ("moved too
            /// quickly"). Vanilla's `playerMovementCheck` game rule.
            too_fast_check: bool = "too-fast-check", Security, vanilla = true;
            /// The "moved too quickly" check also applies while gliding with an elytra.
            /// Vanilla's `elytraMovementCheck` game rule.
            elytra_too_fast_check: bool = "elytra-too-fast-check", Security, vanilla = true;
            /// Players whose movement does not match the server's collision result ("moved
            /// wrongly") or that move into a block are teleported back, like Vanilla's
            /// `handleMovePlayer`. Off accepts every move that passes the other checks.
            wrong_movement_check: bool = "wrong-movement-check", Security, vanilla = true;
            /// Players floating in the air longer than Vanilla allows are kicked unless
            /// `allow-flight` is on. Not enforced yet: it needs block collision (milestone 3).
            flying_kick: bool = "flying-kick", Security, vanilla = true;
        }
        /// Chunk loading, generation and unloading.
        chunks: ChunkRules = "performance.chunks" {
            /// Ticks a chunk stays loaded after no player or ticket needs it, so walking back
            /// does not load it again.
            unload_delay: u16 = "unload-delay", Performance, range = [0, 6000], vanilla = 100;
            /// Background chunk jobs (loads, generation steps, light) in flight per world. Higher
            /// values load faster with many players at the cost of memory for queued work.
            max_jobs: u16 = "max-jobs", Performance, range = [1, 4096], vanilla = 64;
        }
        /// Tile region mode: one world on several cores. The world is cut into fixed square
        /// tiles; the tiles' natural mob spawning and random ticks are worked out in parallel
        /// on the world as it is at the start of the phase, each tile with its own random
        /// numbers, and then applied tile by tile. Helps when players are spread out (every
        /// player loads own chunks); a crowd in one place lies in one tile and gains nothing.
        /// The result is deterministic: it does not depend on the number of threads.
        /// Deviations (see `docs/architecture/region-threading.md`): random ticks, spawn
        /// attempts and the shuffled chunk order draw from a random stream per tile and tick
        /// instead of the world's one stream (same rates, other positions than a seeded
        /// Vanilla run); tiles tick in tile order instead of the world's chunk order; a tile
        /// does not see the mobs other tiles spawn in the same tick, so a player's mob cap can
        /// be exceeded by one tick's spawns at a tile border; thunder and inhabited time of a
        /// tile's chunks run after its spawn attempts. While all ticking chunks lie in one
        /// tile or are fewer than 512, the world ticks in Vanilla's exact order.
        region_threading: RegionThreadingRules = "performance.region-threading" {
            /// Turns the tile region mode on: natural mob spawning and random ticks of a world
            /// are worked out per tile (a fixed square of chunks) on several cores. Faster when
            /// players are spread out; no gain for a crowd in one place. Deviation: these
            /// phases draw their random numbers per tile, so a seeded world differs from
            /// Vanilla (same rates), and a player's mob cap can be exceeded by one tick's
            /// spawns at a tile border. On in the performance profile; `false` keeps Vanilla's
            /// exact serial order at no cost.
            enabled: bool = "enabled", Performance, vanilla = false, performance = true;
            /// Edge of a tile in chunks. Smaller tiles balance better over the cores, larger
            /// ones cost less bookkeeping.
            tile_size: u8 = "tile-size", Performance, range = [8, 32], vanilla = 16;
        }
        /// World simulation: scheduled ticks, neighbour updates and light after block changes.
        /// Work over a budget waits for the next tick.
        simulation: SimulationRules = "performance.simulation" {
            /// Scheduled block ticks run per tick.
            max_block_ticks: u32 = "max-block-ticks", Performance, range = [1, 1_000_000],
                vanilla = 65536;
            /// Scheduled fluid ticks run per tick.
            max_fluid_ticks: u32 = "max-fluid-ticks", Performance, range = [1, 1_000_000],
                vanilla = 65536;
            /// Chained neighbour and shape updates of one block change before the rest is
            /// skipped; "vanilla" uses `max-chained-neighbor-updates` of server.properties.
            max_chained_neighbor_updates: VanillaOr<u32> = "max-chained-neighbor-updates",
                Performance, range = [1, 100_000_000], vanilla = VanillaOr::Vanilla;
            /// Chunks whose light is updated together after block changes, in one background
            /// job that recomputes only the cells the changes reach; the rest follows in the
            /// next job. Lower values split bursts into smaller jobs.
            max_relights_per_tick: u32 = "max-relights-per-tick", Performance,
                range = [1, 65536], vanilla = 1024;
        }
        /// Entity collisions.
        collisions: CollisionRules = "performance.collisions" {
            /// Entities a mob pushes and is pushed by per tick (`pushEntities`). Saves the push
            /// work in crowded mob pens and farms. Deviation: in a crowd of more entities only
            /// the first ones push, so crowds spread more slowly; cramming damage still counts
            /// every entity. "vanilla" pushes all.
            max_entity_collisions: VanillaOr<u32> = "max-entity-collisions", Performance,
                range = [0, 1024], vanilla = VanillaOr::Vanilla, paper = VanillaOr::Value(8),
                performance = VanillaOr::Value(8);
        }
        /// Mobs and animals: AI work per tick. The Vanilla profile keeps every mob exact.
        mobs: MobRules = "performance.mobs" {
            /// Path finding nodes all mobs of the world may visit per tick; searches over the
            /// budget fail and are retried later. 0 = unlimited (Vanilla).
            path_nodes_per_tick: u32 = "path-nodes-per-tick", Performance,
                range = [0, 10_000_000], vanilla = 0, performance = 50_000;
        }
        /// Activation range: mobs farther from every player than their group's range (in
        /// blocks on the X and Z axes, at any height; spectators do not count) are inactive.
        /// An inactive mob gets a full tick only every 20th tick; in between it only ages
        /// (babies grow up and breeding cooldowns end in time). Saves most of the mob tick on
        /// servers with many mobs away from players. Deviation: inactive mobs move,
        /// path-find and breed at about 1/20 of Vanilla's speed. Mobs stay active while
        /// they are new (10 seconds), leashed, burning, hurt, in water, falling, riding or
        /// ridden, have a target, were attacked recently or are in love, and villagers while
        /// they panic or work (see below). Bosses always tick. 0 = the group always ticks
        /// (Vanilla); at most 128.
        activation: ActivationRules = "performance.activation-range" {
            /// Range of animals (`creature` and `ambient` mobs: cows, sheep, bats, ...).
            animals: u16 = "animals", Performance, range = [0, 128], vanilla = 0,
                paper = 32, spigot = 32, performance = 32;
            /// Range of monsters (`monster` mobs that are no raiders or flyers).
            monsters: u16 = "monsters", Performance, range = [0, 128], vanilla = 0,
                paper = 32, spigot = 32, performance = 32;
            /// Range of raiders (pillagers, vindicators, evokers, illusioners, witches and ravagers).
            raiders: u16 = "raiders", Performance, range = [0, 128], vanilla = 0,
                paper = 64, spigot = 48, performance = 64;
            /// Range of the other mobs (golems, armor stands, ...).
            misc: u16 = "misc", Performance, range = [0, 128], vanilla = 0,
                paper = 16, spigot = 16, performance = 16;
            /// Range of water mobs (fish, squid, dolphins, axolotls, ...).
            water: u16 = "water", Performance, range = [0, 128], vanilla = 0,
                paper = 16, spigot = 16, performance = 16;
            /// Range of villagers and wandering traders.
            villagers: u16 = "villagers", Performance, range = [0, 128], vanilla = 0,
                paper = 32, spigot = 32, performance = 32;
            /// Range of flying monsters (ghasts, phantoms, blazes, vexes).
            flying: u16 = "flying", Performance, range = [0, 128], vanilla = 0,
                paper = 32, spigot = 32, performance = 32;
            /// Ticks an inactive villager must have been inactive before it becomes active
            /// while it works at its job site, so workstations keep restocking trades; 0 =
            /// never.
            villagers_work_immunity_after: u16 = "villagers-work-immunity-after", Performance,
                range = [0, 12000], vanilla = 0, paper = 100, spigot = 100, performance = 100;
            /// Ticks a working villager then stays active.
            villagers_work_immunity_for: u16 = "villagers-work-immunity-for", Performance,
                range = [0, 12000], vanilla = 0, paper = 20, spigot = 20, performance = 20;
            /// Panicking villagers stay active (they flee and alarm the village, which spawns
            /// iron golems).
            villagers_active_for_panic: bool = "villagers-active-for-panic", Performance,
                vanilla = false, paper = true, spigot = true, performance = true;
            /// Ticks between the goal and target selector runs of an inactive mob between its
            /// full ticks: it notices targets, food and partners sooner, but does not move
            /// before its next full tick. Lower values cost more. "disabled": inactive mobs
            /// choose goals only in their full ticks (cheapest).
            inactive_mob_goal_selector_throttle: DisabledOr<u32> =
                "inactive-mob-goal-selector-throttle", Performance, range = [1, 20],
                vanilla = DisabledOr::Disabled, paper = DisabledOr::Value(10);
        }
        /// Wake-up plan of the activation range: mobs of a group that were inactive for
        /// `<group>-every` ticks tick fully for `<group>-for` ticks, so breeders, villages and
        /// far mobs still progress a little. About one mob per group and tick is woken up,
        /// at most `<group>-max-per-tick` in one tick after ticks without any. A max of 0
        /// wakes none.
        wake_up: WakeUpRules = "performance.activation-range.wake-up-inactive" {
            /// Animals woken up per tick at most.
            animals_max_per_tick: u16 = "animals-max-per-tick", Performance, range = [0, 1000],
                vanilla = 0, paper = 4, spigot = 4, performance = 4;
            /// Ticks animals must have been inactive before they may be woken up.
            animals_every: u32 = "animals-every", Performance, range = [1, 1_728_000],
                vanilla = 1200;
            /// Ticks woken up animals stay active.
            animals_for: u16 = "animals-for", Performance, range = [1, 12000], vanilla = 100;
            /// Villagers woken up per tick at most.
            villagers_max_per_tick: u16 = "villagers-max-per-tick", Performance, range = [0, 1000],
                vanilla = 0, paper = 4, spigot = 4, performance = 4;
            /// Ticks villagers must have been inactive before they may be woken up.
            villagers_every: u32 = "villagers-every", Performance, range = [1, 1_728_000],
                vanilla = 600;
            /// Ticks woken up villagers stay active.
            villagers_for: u16 = "villagers-for", Performance, range = [1, 12000], vanilla = 100;
            /// Monsters woken up per tick at most.
            monsters_max_per_tick: u16 = "monsters-max-per-tick", Performance, range = [0, 1000],
                vanilla = 0, paper = 8, spigot = 8, performance = 8;
            /// Ticks monsters must have been inactive before they may be woken up.
            monsters_every: u32 = "monsters-every", Performance, range = [1, 1_728_000],
                vanilla = 400;
            /// Ticks woken up monsters stay active.
            monsters_for: u16 = "monsters-for", Performance, range = [1, 12000], vanilla = 100;
            /// Flying woken up per tick at most.
            flying_max_per_tick: u16 = "flying-max-per-tick", Performance, range = [0, 1000],
                vanilla = 0, paper = 8, spigot = 8, performance = 8;
            /// Ticks flying must have been inactive before they may be woken up.
            flying_every: u32 = "flying-every", Performance, range = [1, 1_728_000],
                vanilla = 200;
            /// Ticks woken up flying stay active.
            flying_for: u16 = "flying-for", Performance, range = [1, 12000], vanilla = 100;
        }
        /// Item entities (dropped items).
        items: ItemRules = "performance.items" {
            /// Ticks an item lies on the ground before it despawns. Lower values keep fewer
            /// item entities around on busy servers.
            despawn_ticks: u16 = "despawn-ticks", Performance, range = [20, 32766],
                vanilla = 6000;
            /// Horizontal radius in blocks within which equal item stacks merge into one entity
            /// (vertically the radius minus 0.5). Fewer item entities to tick, track and send
            /// near farms and mob grinders. Deviation: with more than Vanilla's 0.5, items
            /// lying apart jump together into one stack.
            merge_radius: f64 = "merge-radius", Performance, range = [0.5, 16.0],
                vanilla = 0.5, performance = 2.5;
            /// Radius in blocks within which experience orbs of the same value merge (Vanilla's
            /// scan every 20 ticks). Fewer orb entities after mob grinders. Deviation: with more
            /// than Vanilla's 0.5, orbs lying apart merge and fly to the player as one.
            xp_merge_radius: f64 = "xp-merge-radius", Performance, range = [0.5, 16.0],
                vanilla = 0.5, performance = 3.0;
            /// Ticks an arrow stuck in a block stays before it despawns. Fewer arrow entities on
            /// PvP servers and around skeletons. Deviation: arrows vanish earlier than
            /// Vanilla's 60 seconds.
            arrow_despawn_ticks: u16 = "arrow-despawn-ticks", Performance, range = [20, 1200],
                vanilla = 1200, performance = 300;
        }
        /// Natural mob spawning and despawning (`NaturalSpawner`). Values per category
        /// replace Vanilla's mob caps (per 17 × 17 chunks around players, also per player).
        spawning: SpawningRules = "performance.spawning" {
            /// Ticks between natural spawn rounds of monsters, ambient and water mobs (animals
            /// keep Vanilla's 400). Saves spawn attempts, which cost a lot with many players.
            /// Deviation: caps refill more slowly after mobs die (mob farms produce less).
            spawn_interval: u16 = "spawn-interval", Performance, range = [1, 400],
                vanilla = 1, performance = 2;
            /// Mob cap of monsters (Vanilla 70). Fewer mobs to tick; fewer monsters at night.
            monster_cap: VanillaOr<u16> = "monster-cap", Gameplay, range = [0, 1000],
                vanilla = VanillaOr::Vanilla, performance = VanillaOr::Value(50);
            /// Mob cap of animals (`creature`, Vanilla 10).
            creature_cap: VanillaOr<u16> = "creature-cap", Gameplay, range = [0, 1000],
                vanilla = VanillaOr::Vanilla;
            /// Mob cap of bats (`ambient`, Vanilla 15).
            ambient_cap: VanillaOr<u16> = "ambient-cap", Gameplay, range = [0, 1000],
                vanilla = VanillaOr::Vanilla, performance = VanillaOr::Value(5);
            /// Mob cap of axolotls (Vanilla 5).
            axolotls_cap: VanillaOr<u16> = "axolotls-cap", Gameplay, range = [0, 1000],
                vanilla = VanillaOr::Vanilla;
            /// Mob cap of glow squids (`underground_water_creature`, Vanilla 5).
            underground_water_creature_cap: VanillaOr<u16> = "underground-water-creature-cap",
                Gameplay, range = [0, 1000], vanilla = VanillaOr::Vanilla;
            /// Mob cap of squids and dolphins (`water_creature`, Vanilla 5).
            water_creature_cap: VanillaOr<u16> = "water-creature-cap", Gameplay,
                range = [0, 1000], vanilla = VanillaOr::Vanilla;
            /// Mob cap of fish (`water_ambient`, Vanilla 20).
            water_ambient_cap: VanillaOr<u16> = "water-ambient-cap", Gameplay,
                range = [0, 1000], vanilla = VanillaOr::Vanilla, performance = VanillaOr::Value(10);
            /// Blocks from the nearest player beyond which a mob that may despawn is removed at
            /// once, and beyond which no mob spawns (Vanilla 128, fish 64). Fewer far mobs to
            /// tick. Deviation: mobs disappear closer to players.
            hard_despawn_range: VanillaOr<u16> = "hard-despawn-range", Gameplay,
                range = [8, 512], vanilla = VanillaOr::Vanilla, performance = VanillaOr::Value(96);
            /// Blocks from the nearest player beyond which an idle mob may despawn by chance
            /// (Vanilla 32).
            soft_despawn_range: VanillaOr<u16> = "soft-despawn-range", Gameplay,
                range = [8, 512], vanilla = VanillaOr::Vanilla;
        }
        /// Explosions.
        explosions: ExplosionRules = "performance.explosions" {
            /// Primed TNT entities ticked per world tick; the rest wait (their fuse pauses)
            /// until a later tick. Keeps huge TNT chains from stalling the server. Deviation:
            /// above the limit, TNT explodes later than in Vanilla.
            max_tnt_per_tick: DisabledOr<u32> = "max-tnt-per-tick", Limit, range = [1, 100_000],
                vanilla = DisabledOr::Disabled, paper = DisabledOr::Value(100),
                spigot = DisabledOr::Value(100), performance = DisabledOr::Value(100);
        }
        /// Hoppers.
        hoppers: HopperRules = "performance.hoppers" {
            /// Ticks a hopper waits after it moved an item (Vanilla 8). Higher values move items
            /// more slowly.
            transfer_cooldown: u8 = "transfer-cooldown", Gameplay, range = [1, 64],
                vanilla = 8;
            /// Ticks a hopper waits after it found nothing to move (Vanilla 1: it tries every
            /// tick). Saves the container and item lookups of idle hoppers. Deviation: a hopper
            /// notices new items up to this many ticks later; item sorters and clocks built on
            /// hopper timing run differently.
            check_cooldown: u8 = "check-cooldown", Performance, range = [1, 64],
                vanilla = 1, performance = 4;
        }
        /// Server-side checks of block breaking.
        digging: DiggingRules = "anticheat.digging" {
            /// Validates "stop digging" against the server's own break progress: the time since
            /// the player started digging, the tool, Haste and Mining Fatigue, being in water and
            /// on the ground. Vanilla breaks a block whose progress falls short on the next tick
            /// once the player has been online for a while (it takes the start tick as the time
            /// spent), so a client that starts and at once stops digging breaks any block. On, a
            /// stop with too little progress waits for the progress to complete like a legit
            /// dig with latency and is rejected (the block is sent again, nothing drops) when it
            /// does not complete within the dig time seen so far plus one second. Instant
            /// mining, creative and normal digging with lag are not affected.
            fast_break_check: bool = "fast-break-check", Fix, vanilla = false, paper = true,
                spigot = true, performance = true;
        }
        /// Anti-Xray: ores and other hidden blocks that touch no transparent block are kept from
        /// clients, so x-ray clients and texture packs cannot find them. Chunk packets stay
        /// shared by every player of the world; blocks around a changed block are sent with
        /// their real state.
        anti_xray: AntiXrayRules = "anticheat.anti-xray" {
            /// "disabled" sends the real blocks (Vanilla). "hide" sends hidden blocks as the
            /// dimension's base stone (deepslate below y 0, netherrack, end stone). "obfuscate"
            /// sends hidden and replacement blocks as random hidden blocks (bigger chunk packets).
            engine_mode: AntiXrayMode = "engine-mode", Limit, vanilla = AntiXrayMode::Disabled,
                paper = AntiXrayMode::Hide;
            /// Blocks that are hidden (block ids).
            hidden_blocks: Vec<String> = "hidden-blocks", Limit,
                vanilla = anti_xray_hidden_blocks();
            /// Blocks that "obfuscate" also replaces with random hidden blocks (block ids).
            replacement_blocks: Vec<String> = "replacement-blocks", Limit,
                vanilla = anti_xray_replacement_blocks();
            /// Blocks at and above this height are always sent as they are.
            max_block_height: i32 = "max-block-height", Limit, range = [-2048, 2048],
                vanilla = 64;
            /// Distance in blocks around a changed block (mined, exploded, moved) within which
            /// hidden blocks are sent with their real state.
            update_radius: u8 = "update-radius", Limit, range = [1, 4], vanilla = 2;
        }
    }
}

/// Default of `anticheat.anti-xray.hidden-blocks`: ores, raw ore blocks and blocks that mark
/// dungeons, mineshafts and strongholds, in all three dimensions.
fn anti_xray_hidden_blocks() -> Vec<String> {
    [
        "coal_ore",
        "deepslate_coal_ore",
        "copper_ore",
        "deepslate_copper_ore",
        "raw_copper_block",
        "iron_ore",
        "deepslate_iron_ore",
        "raw_iron_block",
        "gold_ore",
        "deepslate_gold_ore",
        "redstone_ore",
        "deepslate_redstone_ore",
        "lapis_ore",
        "deepslate_lapis_ore",
        "diamond_ore",
        "deepslate_diamond_ore",
        "emerald_ore",
        "deepslate_emerald_ore",
        "nether_gold_ore",
        "nether_quartz_ore",
        "ancient_debris",
        "mossy_cobblestone",
        "obsidian",
        "clay",
        "chest",
        "ender_chest",
    ]
    .map(|id| format!("minecraft:{id}"))
    .to_vec()
}

/// Default of `anticheat.anti-xray.replacement-blocks`: the common blocks around hidden ones.
fn anti_xray_replacement_blocks() -> Vec<String> {
    [
        "stone",
        "deepslate",
        "oak_planks",
        "netherrack",
        "end_stone",
    ]
    .map(|id| format!("minecraft:{id}"))
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Profile, RuleSet};

    /// Only `performance` gives up the tick-exact redstone wire: `paper` and `spigot` follow
    /// their servers' defaults, `technical` needs Vanilla's update order.
    #[test]
    fn redstone_wire_by_profile() {
        for profile in Profile::ALL {
            let expected = if profile == Profile::Performance {
                RedstoneImplementation::AlternateCurrent
            } else {
                RedstoneImplementation::Vanilla
            };
            let rules = ServerRules::for_profile(profile);
            assert_eq!(rules.redstone.implementation, expected, "{profile}");
        }
    }
}
