//! Configuration: rule catalogue, profiles, `server.properties` and `vanillite.toml` loading,
//! layering and reload.
//!
//! - `server.properties` holds Vanilla's keys only ([`ServerProperties`]).
//! - `vanillite.toml` holds everything Vanillite-specific: the `profile`, rule overrides,
//!   per-world sections `[worlds."minecraft:the_nether"]` and the extra worlds
//!   `[extra-worlds.<name>]` ([`ExtraWorld`]).
//! - Layers, lowest first: profile defaults → global keys → the world's section → environment
//!   variables `VANILLITE_<KEY>` (dots become `__`, dashes `_`, e.g.
//!   `VANILLITE_PERFORMANCE__TICK__PARALLEL_DIMENSIONS=false`; `VANILLITE_PROFILE` selects the profile).
//!   An empty variable is ignored (for boolean and enumeration rules also `default`), so panels
//!   can leave a key at its profile or file value.
//! - The result is plain structs ([`ServerRules`], [`WorldRules`]); hot paths read fields.
//!   A reload builds a new [`Config`]; the server swaps it at a tick boundary.

mod catalogue;
pub mod properties;
mod rule;

pub use catalogue::*;
pub use properties::{
    Difficulty, GameMode, PropertyInfo, PropertyKind, RegionFileCompression, ServerProperties,
};
pub use rule::{
    AntiXrayMode, DisabledOr, Kind, PluginRuntime, Profile, RedstoneImplementation, RuleInfo,
    RuleSet, RuleValue, StorageLayout, ValueShape, ValueType, VanillaOr, WorldFormat,
};
pub use toml;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

/// File name of the Vanilla properties.
pub const PROPERTIES_FILE: &str = "server.properties";
/// File name of the Vanillite configuration.
pub const VANILLITE_FILE: &str = "vanillite.toml";

/// The loaded configuration of a server.
#[derive(Clone, Debug)]
pub struct Config {
    /// Vanilla's `server.properties`.
    pub properties: ServerProperties,
    /// The selected profile.
    pub profile: Profile,
    /// Rules of server scope.
    pub server: ServerRules,
    /// World rules of worlds without their own section.
    world_defaults: WorldRules,
    /// World rules of worlds with a `[worlds."<dimension>"]` section.
    worlds: BTreeMap<String, WorldRules>,
    /// The extra worlds the server loads at startup (`[extra-worlds.<name>]`), by name.
    pub extra_worlds: Vec<ExtraWorld>,
}

/// An extra world of `[extra-worlds.<name>]`: a world with its own folder `<name>/`, its own
/// `level.dat`, seed and generator, besides the default world (`level-name`), like a world of
/// Multiverse on Paper. Its dimension key is `minecraft:<name in lower case>`; its rules come
/// from `[worlds."minecraft:<name>"]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraWorld {
    /// The folder and name, e.g. `lobby`.
    pub name: String,
    /// `environment`: `normal`, `nether` or `the_end` (its dimension type).
    pub environment: String,
    /// `generator`: `normal` (the environment's Vanilla terrain), `flat`, `void`,
    /// `amplified`, `large_biomes`, or the key of a data pack dimension whose generator it
    /// takes. Only used when the world is created.
    pub generator: String,
    /// `seed`: a number or a text like `level-seed`; empty for a random one. Only used when
    /// the world is created.
    pub seed: String,
    /// `generate-structures`. Only used when the world is created.
    pub generate_structures: bool,
}

impl ExtraWorld {
    /// The environments an extra world can have.
    pub const ENVIRONMENTS: [&str; 3] = ["normal", "nether", "the_end"];

    /// The generators an extra world can name besides a data pack dimension's key.
    pub const GENERATORS: [&str; 5] = ["normal", "flat", "void", "amplified", "large_biomes"];

    /// A world with the defaults: `normal`, Vanilla terrain, random seed, structures.
    pub fn new(name: &str) -> ExtraWorld {
        ExtraWorld {
            name: name.to_owned(),
            environment: "normal".to_owned(),
            generator: "normal".to_owned(),
            seed: String::new(),
            generate_structures: true,
        }
    }

    fn from_toml(name: &str, section: &toml::Table) -> Result<ExtraWorld, String> {
        let mut world = ExtraWorld::new(name);
        let text = |key: &str, value: &toml::Value| match value {
            toml::Value::String(s) => Ok(s.clone()),
            toml::Value::Integer(i) if key == "seed" => Ok(i.to_string()),
            other => Err(format!(
                "extra-worlds.{name}.{key}: expected a string, found {other}"
            )),
        };
        for (key, value) in section {
            match key.as_str() {
                "environment" => {
                    let env = text(key, value)?.to_ascii_lowercase();
                    if !Self::ENVIRONMENTS.contains(&env.as_str()) {
                        return Err(format!(
                            "extra-worlds.{name}.environment: {env:?} is none of normal, nether, the_end"
                        ));
                    }
                    world.environment = env;
                }
                "generator" => {
                    let generator = text(key, value)?;
                    if !Self::GENERATORS.contains(&generator.as_str()) && !generator.contains(':') {
                        return Err(format!(
                            "extra-worlds.{name}.generator: {generator:?} is none of {} and no \
                             dimension key",
                            Self::GENERATORS.join(", ")
                        ));
                    }
                    world.generator = generator;
                }
                "seed" => world.seed = text(key, value)?,
                "generate-structures" => {
                    world.generate_structures = value.as_bool().ok_or_else(|| {
                        format!("extra-worlds.{name}.generate-structures: expected a boolean")
                    })?;
                }
                other => {
                    log::warn!(
                        "{VANILLITE_FILE}: unknown key {other:?} in extra-worlds.{name} is ignored"
                    );
                }
            }
        }
        Ok(world)
    }
}

impl Config {
    /// Loads both files from `dir`, writing `server.properties` back with all keys and creating
    /// a commented `vanillite.toml` if it is missing.
    pub fn load(dir: &Path) -> Result<Config, ConfigError> {
        let properties = load_properties(&dir.join(PROPERTIES_FILE))?;
        let toml_path = dir.join(VANILLITE_FILE);
        let text = match fs::read_to_string(&toml_path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let text = default_vanillite_toml();
                fs::write(&toml_path, &text).map_err(|e| ConfigError::io(&toml_path, e))?;
                text
            }
            Err(e) => return Err(ConfigError::io(&toml_path, e)),
        };
        let table: toml::Table = text.parse().map_err(|e: toml::de::Error| ConfigError {
            path: toml_path.clone(),
            message: e.to_string(),
        })?;
        Self::from_parts(properties, table, &EnvOverrides::from_process()).map_err(|message| {
            ConfigError {
                path: toml_path,
                message,
            }
        })
    }

    /// Builds the configuration from parsed parts. Unknown keys are logged as warnings;
    /// invalid values are errors.
    pub fn from_parts(
        properties: ServerProperties,
        mut table: toml::Table,
        env: &EnvOverrides,
    ) -> Result<Config, String> {
        let profile_name = match env.profile.as_deref() {
            Some(name) => Some(name.to_owned()),
            None => match table.remove("profile") {
                Some(toml::Value::String(name)) => Some(name),
                Some(other) => return Err(format!("profile: expected a string, found {other}")),
                None => None,
            },
        };
        let profile = match profile_name {
            Some(name) => Profile::parse(&name).ok_or_else(|| {
                format!(
                    "unknown profile {name:?} (vanilla, technical, paper, spigot, performance or crowd)"
                )
            })?,
            None => Profile::Vanilla,
        };
        let extra_worlds = match table.remove("extra-worlds") {
            Some(toml::Value::Table(worlds)) => worlds
                .iter()
                .map(|(name, section)| match section {
                    toml::Value::Table(section) => ExtraWorld::from_toml(name, section),
                    _ => Err(format!("extra-worlds.{name:?}: expected a table")),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(other) => return Err(format!("extra-worlds: expected a table, found {other}")),
            None => Vec::new(),
        };
        let world_sections = match table.remove("worlds") {
            Some(toml::Value::Table(worlds)) => worlds,
            Some(other) => return Err(format!("worlds: expected a table, found {other}")),
            None => toml::Table::new(),
        };

        let mut server = ServerRules::for_profile(profile);
        let mut world_defaults = WorldRules::for_profile(profile);
        for (key, value) in flatten(&table) {
            let known = server.set(&key, value).map_err(|e| format!("{key}: {e}"))?
                | world_defaults
                    .set(&key, value)
                    .map_err(|e| format!("{key}: {e}"))?;
            if !known {
                log::warn!("{VANILLITE_FILE}: unknown key {key:?} is ignored");
            }
        }

        let mut worlds = BTreeMap::new();
        for (dimension, section) in world_sections {
            let toml::Value::Table(section) = section else {
                return Err(format!("worlds.{dimension:?}: expected a table"));
            };
            let mut rules = world_defaults.clone();
            for (key, value) in flatten(&section) {
                let known = rules
                    .set(&key, value)
                    .map_err(|e| format!("worlds.{dimension:?}.{key}: {e}"))?;
                if !known {
                    let hint = if ServerRules::RULES.iter().any(|r| r.key == key) {
                        " (a server-wide key cannot be set per world)"
                    } else {
                        ""
                    };
                    log::warn!(
                        "{VANILLITE_FILE}: unknown key {key:?} in worlds.{dimension:?} is ignored{hint}"
                    );
                }
            }
            worlds.insert(dimension, rules);
        }

        env.apply(&mut server)?;
        env.apply(&mut world_defaults)?;
        for rules in worlds.values_mut() {
            env.apply(rules)?;
        }

        Ok(Config {
            properties,
            profile,
            server,
            world_defaults,
            worlds,
            extra_worlds,
        })
    }

    /// The rules of a world, by its dimension key (e.g. `minecraft:the_nether`).
    pub fn world_rules(&self, dimension: &str) -> WorldRules {
        self.worlds
            .get(dimension)
            .unwrap_or(&self.world_defaults)
            .clone()
    }
}

/// Overrides from environment variables `VANILLITE_<KEY>`.
#[derive(Clone, Debug, Default)]
pub struct EnvOverrides {
    profile: Option<String>,
    values: BTreeMap<String, String>,
}

impl EnvOverrides {
    /// Reads the overrides of the current process.
    pub fn from_process() -> EnvOverrides {
        Self::from_vars(std::env::vars())
    }

    /// Reads the overrides from `(name, value)` pairs. Empty values are ignored, and so is
    /// `default` for rules with a fixed set of values (booleans and enumerations): panels offer
    /// it to keep the profile's value.
    pub fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> EnvOverrides {
        let vars: BTreeMap<String, String> =
            vars.into_iter().filter(|(_, v)| !v.is_empty()).collect();
        let mut overrides = EnvOverrides {
            profile: vars.get("VANILLITE_PROFILE").cloned(),
            values: BTreeMap::new(),
        };
        for rule in ServerRules::RULES.iter().chain(WorldRules::RULES) {
            let fixed = rule.value.ty == ValueType::Bool || !rule.value.choices.is_empty();
            if let Some(value) = vars.get(&env_name(rule.key))
                && !(fixed && value == "default")
            {
                overrides.values.insert(rule.key.to_owned(), value.clone());
            }
        }
        overrides
    }

    fn apply<R: RuleSet>(&self, rules: &mut R) -> Result<(), String> {
        for (key, raw) in &self.values {
            // A bare word that is not valid TOML is taken as a string.
            let value = raw
                .parse::<toml::Value>()
                .unwrap_or_else(|_| toml::Value::String(raw.clone()));
            rules
                .set(key, &value)
                .map_err(|e| format!("{}: {e}", env_name(key)))?;
        }
        Ok(())
    }
}

/// The environment variable that overrides a dotted key.
pub fn env_name(key: &str) -> String {
    format!(
        "VANILLITE_{}",
        key.to_ascii_uppercase()
            .replace('.', "__")
            .replace('-', "_")
    )
}

/// Flattens nested tables into dotted keys. Arrays and other values are leaves.
fn flatten(table: &toml::Table) -> Vec<(String, &toml::Value)> {
    fn walk<'a>(prefix: &str, table: &'a toml::Table, out: &mut Vec<(String, &'a toml::Value)>) {
        for (key, value) in table {
            let full = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(inner) => walk(&full, inner, out),
                leaf => out.push((full, leaf)),
            }
        }
    }
    let mut out = Vec::new();
    walk("", table, &mut out);
    out
}

/// Loads `server.properties` and writes it back with every known key (Vanilla does the same).
fn load_properties(path: &Path) -> Result<ServerProperties, ConfigError> {
    let existing = match fs::read(path) {
        // Java reads `.properties` as ISO-8859-1 unless escaped; accept UTF-8 as well.
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(e) => Some(e.into_bytes().into_iter().map(char::from).collect()),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(ConfigError::io(path, e)),
    };
    let mut map = existing
        .as_deref()
        .map(properties::parse)
        .unwrap_or_default();
    let typed = ServerProperties::from_map(&map);
    typed.write_to_map(&mut map);
    let rendered = properties::render(&map);
    if existing.as_deref() != Some(rendered.as_str()) {
        fs::write(path, rendered).map_err(|e| ConfigError::io(path, e))?;
    }
    // `VANILLITE_<KEY>` (e.g. `VANILLITE_SERVER_PORT`) overrides a key for this run only; the
    // file keeps its own value. An empty variable is ignored.
    let mut overridden = false;
    for key in ServerProperties::KEYS {
        if let Ok(value) = std::env::var(env_name(key))
            && !value.is_empty()
        {
            map.insert((*key).to_owned(), value);
            overridden = true;
        }
    }
    Ok(if overridden {
        ServerProperties::from_map(&map)
    } else {
        typed
    })
}

/// The `vanillite.toml` written on first start: the profile and every rule, commented out with
/// its documentation and its `vanilla` profile value.
pub fn default_vanillite_toml() -> String {
    let mut out = String::from(
        "# Vanillite configuration. Vanilla's options are in server.properties.\n\
         #\n\
         # A profile presets every rule: vanilla, technical, paper, spigot,\n\
         # performance (Vanillite's optimizations that deviate a little from Vanilla) or\n\
         # crowd (performance plus crowd mode, for thousands of players in one place).\n\
         # Uncomment a key to override the profile. Per-world overrides go into\n\
         # [worlds.\"minecraft:the_nether\"] sections with the same keys.\n\
         # Extra worlds besides level-name (own folder, seed and generator) go into\n\
         # [extra-worlds.<name>] sections: environment = \"normal\" | \"nether\" |\n\
         # \"the_end\", generator = \"normal\" | \"flat\" | \"void\" | \"amplified\" |\n\
         # \"large_biomes\" | a data pack dimension key, seed = \"\", generate-structures.\n\
         # Environment variables VANILLITE_<KEY> override the file, e.g.\n\
         # VANILLITE_PERFORMANCE__TICK__PARALLEL_DIMENSIONS=false.\n\n\
         profile = \"vanilla\"\n",
    );
    render_rules(&mut out, &ServerRules::for_profile(Profile::Vanilla));
    render_rules(&mut out, &WorldRules::for_profile(Profile::Vanilla));
    out
}

fn render_rules<R: RuleSet>(out: &mut String, rules: &R) {
    let mut group = "";
    for info in R::RULES {
        if info.group != group {
            group = info.group;
            let _ = write!(out, "\n[{group}]\n");
        }
        for line in info.doc.lines() {
            let _ = writeln!(out, "#{line}");
        }
        if let Some((min, max)) = info.range {
            let _ = writeln!(out, "# Range: {min}..={max}.");
        }
        if info.restart {
            let _ = writeln!(out, "# Needs a restart.");
        }
        if let Some(value) = rules.get(info.key) {
            let _ = writeln!(out, "#{} = {value}", info.name);
        }
    }
}

/// A configuration file could not be read or is invalid.
#[derive(Debug)]
pub struct ConfigError {
    /// The file.
    pub path: PathBuf,
    /// What is wrong.
    pub message: String,
}

impl ConfigError {
    fn io(path: &Path, error: io::Error) -> ConfigError {
        ConfigError {
            path: path.to_owned(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for ConfigError {}
