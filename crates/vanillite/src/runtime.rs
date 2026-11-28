//! Plugin runtime dispatch: the static (musl) Linux builds of the release archives and the
//! Docker image cannot load libraries, so they cannot load plugins. They ship `vanillite-gnu`,
//! built from the same commit against glibc, next to them and start it in their place when
//! `plugins/` holds plugins (`plugins.runtime`). Without plugins the static build runs.

use std::path::Path;

/// File name of the glibc build next to the static binary.
const GNU_BINARY: &str = "vanillite-gnu";

/// Set for `vanillite-gnu` when the static build started it: the dispatch never repeats.
const DISPATCHED: &str = "VANILLITE_RUNTIME_DISPATCHED";

/// What to log about the runtime once the logger runs.
#[derive(Debug)]
pub enum Outcome {
    /// Nothing to say.
    None,
    /// An info line.
    #[cfg_attr(all(target_os = "linux", target_env = "musl"), allow(dead_code))]
    Info(String),
    /// A warning: the server starts without plugins.
    #[cfg_attr(not(all(target_os = "linux", target_env = "musl")), allow(dead_code))]
    Warn(String),
}

impl Outcome {
    pub fn log(self) {
        match self {
            Outcome::None => {}
            Outcome::Info(message) => log::info!("{message}"),
            Outcome::Warn(message) => log::warn!("{message}"),
        }
    }
}

/// Replaces this static build with `vanillite-gnu` when the server folder `dir` has plugins and
/// `plugins.runtime` allows it. Where that is not possible the server starts without plugins.
/// Runs first in `run`, while the process has one thread.
#[cfg(all(target_os = "linux", target_env = "musl"))]
pub fn dispatch(dir: &Path, version: &str) -> Outcome {
    use std::os::unix::process::CommandExt as _;
    use vanillite_config::PluginRuntime;
    use vanillite_plugin_host::loader;

    if std::env::var_os(DISPATCHED).is_some() {
        return without_plugins(format!(
            "{GNU_BINARY} started the static build again; plugins are not loaded"
        ));
    }
    // An invalid configuration is reported by the server start.
    let Ok(config) = vanillite_config::Config::peek(dir) else {
        return Outcome::None;
    };
    let rules = &config.server.plugins;
    if !rules.enabled {
        return Outcome::None;
    }
    let plugins = dir.join("plugins");
    let wanted = match rules.runtime {
        PluginRuntime::Static => false,
        PluginRuntime::Gnu => true,
        PluginRuntime::Auto => {
            loader::has_packages(&plugins) || !loader::bukkit_jars(&plugins).is_empty()
        }
    };
    if !wanted {
        if rules.runtime == PluginRuntime::Static && loader::has_packages(&plugins) {
            return without_plugins(
                "plugins.runtime = \"static\": the static build cannot load plugins".to_owned(),
            );
        }
        return Outcome::None;
    }
    let Ok(exe) = std::env::current_exe() else {
        return without_plugins("cannot find the server binary".to_owned());
    };
    let gnu = exe.with_file_name(GNU_BINARY);
    if !gnu.is_file() {
        return without_plugins(format!(
            "plugins/ holds plugins, but this static build cannot load them and {} is missing",
            gnu.display()
        ));
    }
    // Both builds must come from the same commit.
    let gnu_version = std::process::Command::new(&gnu).arg("version").output();
    match gnu_version {
        // The first line of `vanillite version` names the build.
        Ok(out)
            if out.status.success()
                && String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .map(str::trim)
                    == Some(version) => {}
        Ok(out) => {
            return without_plugins(format!(
                "{} is another build ({:?}, this one is {version:?})",
                gnu.display(),
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
            ));
        }
        Err(e) => return without_plugins(format!("cannot start {}: {e}", gnu.display())),
    }
    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap_or_else(|| gnu.clone().into_os_string());
    let err = std::process::Command::new(&gnu)
        .arg0(arg0)
        .args(args)
        .env(DISPATCHED, "1")
        .exec();
    without_plugins(format!("starting {} failed: {err}", gnu.display()))
}

/// Turns plugin loading off for this process and says why.
#[cfg(all(target_os = "linux", target_env = "musl"))]
fn without_plugins(why: String) -> Outcome {
    // SAFETY: called from `dispatch`, before the process starts a second thread.
    unsafe { std::env::set_var("VANILLITE_PLUGINS__ENABLED", "false") };
    Outcome::Warn(format!("Starting without plugins: {why}"))
}

/// Builds that load plugins themselves: says when the static build started this one.
#[cfg(not(all(target_os = "linux", target_env = "musl")))]
pub fn dispatch(_dir: &Path, _version: &str) -> Outcome {
    if std::env::var_os(DISPATCHED).is_some() {
        Outcome::Info(format!(
            "Runtime: {GNU_BINARY}, started by the static build to load the plugins"
        ))
    } else {
        Outcome::None
    }
}
