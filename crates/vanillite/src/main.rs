//! The Vanillite server binary: a thin CLI over `vanillite-server`.

mod cpu;
mod health;
mod logger;
mod runtime;
mod terminal;

use std::path::PathBuf;
use std::process::ExitCode;

use vanillite_server::{ServerBuilder, StartError};

// musl's allocator is slow with many threads; the static (Docker) build uses mimalloc, and so
// does its glibc build for plugins (vanillite-gnu, feature `mimalloc`).
#[cfg(any(target_env = "musl", feature = "mimalloc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// mimalloc's defaults for this server, set before mimalloc reads its options (the program's
/// first constructors run before mimalloc's own); `MIMALLOC_<OPTION>` variables still win.
/// Committing arena memory only on use and returning freed pages at once keeps transparent
/// huge pages from filling and stops freed memory from staying resident (#313: 20-60 MiB
/// less RSS at idle and under load, at the tick time of the default, #164).
#[cfg(all(target_os = "linux", any(target_env = "musl", feature = "mimalloc")))]
#[used]
#[unsafe(link_section = ".init_array.00100")]
static MIMALLOC_DEFAULTS: extern "C" fn() = {
    extern "C" fn set() {
        unsafe extern "C" {
            fn mi_option_set_default(option: core::ffi::c_int, value: core::ffi::c_long);
        }
        // mimalloc's `mi_option_t`: arena_eager_commit and purge_delay.
        const ARENA_EAGER_COMMIT: core::ffi::c_int = 4;
        const PURGE_DELAY: core::ffi::c_int = 15;
        // SAFETY: only sets the default values of two options; allocates nothing.
        unsafe {
            mi_option_set_default(ARENA_EAGER_COMMIT, 0);
            mi_option_set_default(PURGE_DELAY, 0);
        }
    }
    set
};

/// Returns freed memory of the allocator to the system. mimalloc reaches only the calling
/// thread's heap: the server runs it on every thread ([`ServerBuilder::thread_memory_release`]).
fn release_memory() {
    #[cfg(any(target_env = "musl", feature = "mimalloc"))]
    {
        // Part of the mimalloc library the global allocator links.
        unsafe extern "C" {
            fn mi_collect(force: bool);
        }
        // SAFETY: `mi_collect` may be called from any thread at any time.
        unsafe { mi_collect(true) };
    }
    #[cfg(all(target_os = "linux", target_env = "gnu", not(feature = "mimalloc")))]
    // SAFETY: `malloc_trim` may be called from any thread at any time.
    unsafe {
        libc::malloc_trim(0);
    }
}

const USAGE: &str = "\
Usage: vanillite [command] [options]

Commands:
  run              Start the server (default)
  health           Send a status ping to the local server; exit code 0 if it answers
  convert          Convert the world into another format while the server is stopped
  export <target>  Write a copy of the world in another format into <target>
                   (default: a Vanilla (Anvil) copy of an own-format world)
  upgrade-world    Rewrite every chunk of an own-format world written by an older
                   Minecraft version in this version (otherwise chunks are upgraded
                   when they are saved)
  version          Print the version

Options of run, health, convert, export and upgrade-world:
  --dir <path>   Server folder with config files and worlds (default: current folder)

Options of convert:
  --to <format>  vanillite or anvil (default: storage.world-format of vanillite.toml).
                 The new files are verified before the old ones are moved into
                 world-format-backup/ (storage.keep-converted-files) or deleted.
                 An interrupted conversion is resumed by running it again

Options of export:
  --to <format>  anvil (default) or vanillite

Options of health:
  --port <port>  Port to ping (default: VANILLITE_SERVER_PORT, else server-port of
                 server.properties, else 25565)

Environment:
  VANILLITE_LOG=<level>   error, warn, info (default), debug or trace
  VANILLITE_COLOR=<mode>  auto (default), always or never: console colours
                          (NO_COLOR also turns them off)
  VANILLITE_<KEY>=<value> Overrides a key of vanillite.toml or server.properties
                          (e.g. VANILLITE_SERVER_PORT, VANILLITE_MOTD)";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next();
    match command.as_deref() {
        None | Some("run") => run(args),
        Some("health") => health::run(args, USAGE),
        Some("convert") => convert(args),
        Some("export") => export(args),
        Some("upgrade-world") => upgrade_world(args),
        Some("version" | "--version" | "-V") => {
            println!("{}\n{}", version(), about());
            ExitCode::SUCCESS
        }
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("Unknown command {other:?}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut dir = PathBuf::from(".");
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--dir", Some(path)) => dir = PathBuf::from(path),
            _ => {
                eprintln!("Invalid option {arg:?}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    // The CPU build first, then the plugin runtime: with plugins, the static build gives way to
    // `vanillite-gnu`. In this order the process that logs is the one that decided.
    let baseline = cpu::dispatch();
    let runtime = runtime::dispatch(&dir, &version());
    logger::init();
    runtime.log();
    match baseline {
        Some(why) => log::info!("CPU build: {} ({why})", cpu::level()),
        None => log::info!("CPU build: {}", cpu::level()),
    }

    // The tick loop runs on a thread named like Vanilla's, so log lines show "Server thread".
    let server_thread = std::thread::Builder::new()
        .name("Server thread".to_owned())
        .spawn(move || {
            let server = loop {
                let mut builder = ServerBuilder::new(dir.clone());
                builder.version(version());
                builder.memory_release(release_memory);
                #[cfg(any(target_env = "musl", feature = "mimalloc"))]
                builder.thread_memory_release(release_memory);
                match builder.start() {
                    Ok(server) => break server,
                    Err(e @ StartError::Eula(_)) => {
                        log::error!("{e}");
                        if !terminal::accept_eula(&dir) {
                            return false;
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to start the server: {e}");
                        return false;
                    }
                }
            };
            let handle = server.handle();
            let signal_handle = handle.clone();
            if let Err(e) = ctrlc::set_handler(move || signal_handle.stop()) {
                log::warn!("Cannot handle Ctrl-C and SIGTERM: {e}");
            }
            terminal::spawn_console(handle);
            server.run();
            true
        });
    let started = match server_thread.map(|thread| thread.join()) {
        Ok(Ok(started)) => started,
        Ok(Err(_)) => {
            log::error!("The server thread crashed");
            false
        }
        Err(e) => {
            log::error!("Cannot start the server thread: {e}");
            false
        }
    };
    terminal::finish();
    if started {
        ExitCode::SUCCESS
    } else {
        terminal::hold_window();
        ExitCode::FAILURE
    }
}

/// `Vanillite <version> (Minecraft <id>, <commit>)`.
fn version() -> String {
    format!(
        "Vanillite {} (Minecraft {}, {})",
        env!("CARGO_PKG_VERSION"),
        vanillite_data::version::VERSION_ID,
        env!("VANILLITE_COMMIT")
    )
}

/// The author, website and licence line of `vanillite --version`.
fn about() -> String {
    format!(
        "by {}, {}, licensed under {}",
        env!("CARGO_PKG_AUTHORS"),
        env!("CARGO_PKG_HOMEPAGE"),
        env!("CARGO_PKG_LICENSE")
    )
}

/// `vanillite convert [--to <format>] [--dir <path>]`.
fn convert(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut dir = PathBuf::from(".");
    let mut to = None;
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next().as_deref()) {
            ("--dir", Some(path)) => dir = PathBuf::from(path),
            ("--to", Some("vanillite")) => to = Some(vanillite_storage::WorldFormat::Vanillite),
            ("--to", Some("anvil")) => to = Some(vanillite_storage::WorldFormat::Anvil),
            _ => {
                eprintln!("Invalid option {arg:?}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    logger::init();
    match vanillite_server::convert_world(&dir, to) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("Conversion failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `vanillite upgrade-world [--dir <path>]`.
fn upgrade_world(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut dir = PathBuf::from(".");
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--dir", Some(path)) => dir = PathBuf::from(path),
            _ => {
                eprintln!("Invalid option {arg:?}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    logger::init();
    match vanillite_server::upgrade_world(&dir) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("Upgrade failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `vanillite export <target> [--to <format>] [--dir <path>]`.
fn export(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut dir = PathBuf::from(".");
    let mut target = None;
    let mut to = vanillite_storage::WorldFormat::Anvil;
    while let Some(arg) = args.next() {
        match (arg.as_str(), target.is_none()) {
            ("--dir", _) => {
                let Some(path) = args.next() else {
                    eprintln!("--dir needs a folder\n\n{USAGE}");
                    return ExitCode::FAILURE;
                };
                dir = PathBuf::from(path);
            }
            ("--to", _) => match args.next().as_deref() {
                Some("anvil") => to = vanillite_storage::WorldFormat::Anvil,
                Some("vanillite") => to = vanillite_storage::WorldFormat::Vanillite,
                _ => {
                    eprintln!("--to needs anvil or vanillite\n\n{USAGE}");
                    return ExitCode::FAILURE;
                }
            },
            (_, true) if !arg.starts_with("--") => target = Some(PathBuf::from(arg)),
            _ => {
                eprintln!("Invalid option {arg:?}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(target) = target else {
        eprintln!("export needs a target folder\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    logger::init();
    match vanillite_server::export_world(&dir, &target, to) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("Export failed: {e}");
            ExitCode::FAILURE
        }
    }
}
