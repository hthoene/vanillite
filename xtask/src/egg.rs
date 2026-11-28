//! `cargo xtask egg`: the two Pterodactyl eggs, generated from the typed configuration schema of
//! `vanillite-config`: the advanced egg `deploy/pterodactyl/egg-vanillite.json` with every
//! setting and the simple egg `deploy/pterodactyl/egg-vanillite-simple.json` with only
//! [`SIMPLE`]. Both start the same images the same way and name a setting with the same
//! variable, so a server can switch between them.
//!
//! Every `server.properties` key marked for panels and every rule of `vanillite.toml` (except
//! the `debug` helpers) becomes an egg variable named like its `VANILLITE_<KEY>` override. The
//! curated [`ESSENTIALS`] come first, in their order and with plain names; every other variable
//! follows, grouped by area ([`AREAS`]) with the area as name prefix. The simple egg holds only
//! [`SIMPLE`]; what it leaves out keeps the profile's value. Rule variables keep the
//! profile's value until a value is entered in the panel: text fields stay empty, dropdowns of
//! booleans and enumerations offer `default` (both are ignored by the server).
//!
//! How the panels render a variable (Pterodactyl 1.x `VariableBox`, Pelican `StartupVariable`):
//! the rule `in:true,false` is a switch in Pterodactyl (a dropdown in Pelican), any other `in:`
//! rule a dropdown, everything else a text field; the order is the order of the egg.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vanillite_config::{
    PropertyKind, RuleInfo, RuleSet, ServerProperties, ServerRules, ValueType, WorldRules, env_name,
};

use crate::Result;

/// The egg with every setting, relative to the repository root.
pub const EGG_PATH: &str = "deploy/pterodactyl/egg-vanillite.json";
/// The egg with only the basic settings, relative to the repository root.
pub const SIMPLE_EGG_PATH: &str = "deploy/pterodactyl/egg-vanillite-simple.json";

/// Which of the two eggs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Egg {
    /// Every setting ([`EGG_PATH`]).
    Advanced,
    /// Only [`SIMPLE`] ([`SIMPLE_EGG_PATH`]).
    Simple,
}

impl Egg {
    pub const ALL: [Egg; 2] = [Egg::Advanced, Egg::Simple];

    pub fn path(self) -> &'static str {
        match self {
            Egg::Advanced => EGG_PATH,
            Egg::Simple => SIMPLE_EGG_PATH,
        }
    }
}
/// The image repository on Docker Hub (published by `.github/workflows/docker.yml`).
const REPOSITORY: &str = "thoenehannes/vanillite";
/// The generated Minecraft version, relative to the repository root.
const MC_VERSION_PATH: &str = "data/vanilla/version.toml";
/// The startup command; the image's entrypoint replaces `{{VAR}}` with the variable. Bedrock
/// listens on the allocation's port (UDP) unless `Bedrock: Port` is set.
const STARTUP: &str = "env VANILLITE_SERVER_PORT={{SERVER_PORT}} \
     VANILLITE_BEDROCK__PORT=${VANILLITE_BEDROCK__PORT:-{{SERVER_PORT}}} \
     /vanillite run --dir /home/container";
/// The console line that marks a finished start.
const DONE: &str = ")! For help, type ";
/// Rule groups that never belong on a hosted server.
const HIDDEN_GROUPS: &[&str] = &["debug"];
/// The value of a dropdown that keeps the profile's value (ignored by the server).
const KEEP: &str = "default";

/// A variable shown first, with a plain name and a one-sentence description. `key` is a
/// `server.properties` key, a `vanillite.toml` rule or one of `eula`, `profile`.
struct Essential {
    key: &'static str,
    name: &'static str,
    description: &'static str,
}

/// The few settings most servers change, in the order the panel shows them.
const ESSENTIALS: &[Essential] = &[
    Essential {
        key: "eula",
        name: "Accept the Minecraft EULA",
        description: "Switch on to accept the Minecraft EULA (https://aka.ms/MinecraftEULA); \
                      the server does not start before (the panel's EULA prompt does the same).",
    },
    Essential {
        key: "profile",
        name: "Profile",
        description: "Preset of all technical settings below: paper (the default: behaves \
                      like a default Paper server), vanilla (reproduces Vanilla behaviour as \
                      exactly as possible: choose it for exact Vanilla), technical (Vanilla plus \
                      what technical players need), spigot (its defaults) or performance \
                      (faster, deviates a little from Vanilla).",
    },
    Essential {
        key: "motd",
        name: "Server Description (MOTD)",
        description: "The text shown below the server in the multiplayer list.",
    },
    Essential {
        key: "max-players",
        name: "Max Players",
        description: "How many players can be online at the same time.",
    },
    Essential {
        key: "gamemode",
        name: "Game Mode",
        description: "The game mode of players who join for the first time.",
    },
    Essential {
        key: "difficulty",
        name: "Difficulty",
        description: "The difficulty of the world.",
    },
    Essential {
        key: "hardcore",
        name: "Hardcore",
        description: "Hardcore mode: the difficulty is locked to hard and players have one life.",
    },
    Essential {
        key: "online-mode",
        name: "Online Mode",
        description: "Only players with a Minecraft account checked by Mojang can join; switch \
                      off only behind a proxy that checks the accounts.",
    },
    Essential {
        key: "white-list",
        name: "Whitelist",
        description: "Only players on the whitelist (/whitelist add <name>) can join.",
    },
    Essential {
        key: "level-seed",
        name: "World Seed",
        description: "The seed of a new world; empty picks a random one (no effect on an \
                      existing world).",
    },
    Essential {
        key: "view-distance",
        name: "View Distance",
        description: "How far players can see, in chunks (2 to 32, default 10).",
    },
    Essential {
        key: "simulation-distance",
        name: "Simulation Distance",
        description: "How far around players the world is alive (mobs, crops, redstone), in \
                      chunks (2 to 32, default 10).",
    },
    Essential {
        key: "bedrock.enabled",
        name: "Bedrock Players",
        description: "Let Bedrock Edition players join, on the server's port (UDP) unless \
                      \"Bedrock: Port\" says otherwise.",
    },
];

/// The settings of the simple egg, in the order the panel shows them, with descriptions for
/// admins without technical background. The variables are the same as in the advanced egg.
const SIMPLE: &[Essential] = &[
    Essential {
        key: "eula",
        name: "Accept the Minecraft EULA",
        description: "Switch on to accept the Minecraft EULA (https://aka.ms/MinecraftEULA). \
                      The server only starts after you accept it.",
    },
    Essential {
        key: "profile",
        name: "Server Type (Profile)",
        description: "How the server plays. paper (default): behaves like a default Paper \
                      server. vanilla: as close to the official Minecraft server as possible; \
                      choose it for exact Vanilla behaviour. performance: faster with many \
                      players, a few small differences to Vanilla. spigot: behaves like a \
                      Spigot server. technical: like Vanilla, with extras for technical players.",
    },
    Essential {
        key: "motd",
        name: "Server Name (MOTD)",
        description: "The text players see below your server in their multiplayer list.",
    },
    Essential {
        key: "max-players",
        name: "Max Players",
        description: "How many players can play at the same time.",
    },
    Essential {
        key: "gamemode",
        name: "Game Mode",
        description: "The game mode of new players: survival, creative, adventure or spectator.",
    },
    Essential {
        key: "difficulty",
        name: "Difficulty",
        description: "How hard the game is: peaceful (no monsters), easy, normal or hard.",
    },
    Essential {
        key: "hardcore",
        name: "Hardcore",
        description: "Switch on for hardcore: always hard, and players have only one life.",
    },
    Essential {
        key: "white-list",
        name: "Whitelist",
        description: "Switch on so that only players you add with /whitelist add <name> in the \
                      console can join.",
    },
    Essential {
        key: "online-mode",
        name: "Online Mode",
        description: "Keep on: only players with a real Minecraft account can join. Switch off \
                      only if your server runs behind a proxy such as Velocity.",
    },
    Essential {
        key: "level-seed",
        name: "World Seed",
        description: "The seed of a new world. Leave empty for a random world. Has no effect \
                      once the world exists.",
    },
    Essential {
        key: "view-distance",
        name: "View Distance",
        description: "How far players can see, in chunks (2 to 32). 10 is the Minecraft \
                      default; lower values save memory and CPU.",
    },
    Essential {
        key: "storage.world-format",
        name: "World Format",
        description: "How the world is saved for new worlds. vanillite: smaller and faster \
                      (recommended). anvil: the files of the official Minecraft server. An \
                      existing world keeps its format.",
    },
    Essential {
        key: "bedrock.enabled",
        name: "Bedrock Players",
        description: "Switch on to let players of Minecraft Bedrock Edition (phones, consoles, \
                      Windows) join on the same port.",
    },
];

/// The order of the areas after the essentials; areas not listed follow in catalogue order.
const AREAS: &[&str] = &[
    "Server",
    "Performance",
    "Storage",
    "Backup",
    "Watchdog",
    "Security",
    "Admin",
    "Commands",
    "Chat",
    "Movement",
    "Exploits",
    "Anticheat",
    "Proxy",
    "Multiversion",
    "Bedrock",
    "Plugins",
];

/// Environment variables Pterodactyl sets itself; eggs must not declare them.
const RESERVED: &[&str] = &[
    "SERVER_MEMORY",
    "SERVER_IP",
    "SERVER_PORT",
    "ENV",
    "HOME",
    "USER",
    "STARTUP",
    "SERVER_UUID",
    "UUID",
];
const INSTALL_SCRIPT: &str = "#!/bin/ash
# Vanillite needs no installation: the image holds the server, and the first start downloads
# Mojang's server jar for the Vanilla data into cache/.
mkdir -p /mnt/server
echo \"Nothing to install.\"
";

/// `cargo xtask egg [--check]` and `cargo xtask egg smoke [--image <ref>]`.
pub fn run(root: &Path, args: &[&str]) -> Result<()> {
    match args {
        [] => {
            crate::write(&root.join(VERSIONS_PATH), &render_versions(root)?)?;
            println!("wrote {VERSIONS_PATH}");
            for kind in Egg::ALL {
                let egg = render(root, kind)?;
                check_format(&egg)?;
                crate::write(&root.join(kind.path()), &egg)?;
                println!("wrote {}", kind.path());
            }
            // The configuration reference comes from the same schema.
            crate::configdoc::run(root, &[])
        }
        ["--check"] => check(root),
        ["smoke"] => smoke(root, &default_image()),
        ["smoke", "--image", image] => smoke(root, image),
        _ => Err(crate::USAGE.to_owned()),
    }
}

/// Fails if a committed egg differs from the generated one or breaks the egg format, if the
/// eggs differ in what they share, or if the configuration reference is stale.
pub fn check(root: &Path) -> Result<()> {
    crate::configdoc::check(root)?;
    if crate::read(&root.join(VERSIONS_PATH)).unwrap_or_default() != render_versions(root)? {
        return Err(format!(
            "{VERSIONS_PATH} lacks the Minecraft version or release channel of the sources; run \
             `cargo xtask egg`"
        ));
    }
    let mut eggs = Vec::new();
    for kind in Egg::ALL {
        let egg = render(root, kind)?;
        check_format(&egg)?;
        let path = kind.path();
        let committed = crate::read(&root.join(path)).unwrap_or_default();
        if committed != egg {
            return Err(format!(
                "{path} is out of date with the configuration schema; run `cargo xtask egg`"
            ));
        }
        eggs.push(serde_json::from_str::<Value>(&egg).map_err(|e| e.to_string())?);
    }
    check_switchable(&eggs[0], &eggs[1])
}

/// A server switches between the eggs without changes: the same images, startup and stop, and
/// every variable of the simple egg exists in the advanced one, which accepts its default.
fn check_switchable(advanced: &Value, simple: &Value) -> Result<()> {
    for pointer in ["/docker_images", "/startup", "/config", "/features"] {
        if advanced.pointer(pointer) != simple.pointer(pointer) {
            return Err(format!("eggs: {pointer} differs between the two eggs"));
        }
    }
    let vars =
        |egg: &Value| -> Vec<Value> { egg["variables"].as_array().cloned().unwrap_or_default() };
    let advanced = vars(advanced);
    for var in vars(simple) {
        let env = &var["env_variable"];
        let other = advanced
            .iter()
            .find(|v| v["env_variable"] == *env)
            .ok_or_else(|| format!("eggs: {env} is only in the simple egg"))?;
        let default = var["default_value"].as_str().unwrap_or_default();
        validate(other["rules"].as_str().unwrap_or_default(), default).map_err(|e| {
            format!("eggs: the simple egg's default {default:?} of {env}: advanced egg {e}")
        })?;
    }
    Ok(())
}

/// One generated variable before ordering.
struct Entry {
    /// The configuration key ([`Essential::key`]).
    key: String,
    /// The area (name prefix) outside the essentials.
    area: String,
    /// The name inside the area.
    name: String,
    description: String,
    env: String,
    default: String,
    rules: String,
    /// A rule's value in the default profile (empty for other variables).
    preset: String,
}

/// The egg as pretty JSON with a trailing newline.
pub fn render(root: &Path, kind: Egg) -> Result<String> {
    let mut entries = vec![
        Entry {
            key: "eula".to_owned(),
            area: "Server".to_owned(),
            name: "Accept the Minecraft EULA".to_owned(),
            description: String::new(),
            env: "VANILLITE_EULA".to_owned(),
            default: "false".to_owned(),
            rules: "required|string|in:true,false".to_owned(),
            preset: String::new(),
        },
        Entry {
            key: "profile".to_owned(),
            area: "Server".to_owned(),
            name: "Profile".to_owned(),
            description: String::new(),
            env: "VANILLITE_PROFILE".to_owned(),
            default: vanillite_config::Profile::default().name().to_owned(),
            rules: "required|string|in:vanilla,technical,paper,spigot,performance".to_owned(),
            preset: String::new(),
        },
        Entry {
            key: "log".to_owned(),
            area: "Server".to_owned(),
            name: "Log Level".to_owned(),
            description: "Console log level: error, warn, info, debug or trace. Default: info."
                .to_owned(),
            env: "VANILLITE_LOG".to_owned(),
            default: "info".to_owned(),
            rules: "required|string|in:error,warn,info,debug,trace".to_owned(),
            preset: String::new(),
        },
    ];

    let mut defaults = std::collections::BTreeMap::new();
    ServerProperties::default().write_to_map(&mut defaults);
    for info in ServerProperties::INFO.iter().filter(|i| i.panel) {
        let default = defaults.get(info.key).cloned().unwrap_or_default();
        let rules = match (info.kind, info.range) {
            (PropertyKind::Bool, _) => "required|string|in:true,false".to_owned(),
            (PropertyKind::Integer, Some((min, max))) => {
                format!("required|integer|between:{min},{max}")
            }
            (PropertyKind::Integer, None) => "required|integer".to_owned(),
            (PropertyKind::Text, _) => "nullable|string".to_owned(),
            (PropertyKind::Enum(names), _) => format!("required|string|in:{}", names.join(",")),
        };
        let mut description = sentence(info.doc);
        if let Some((min, max)) = info.range {
            description.push_str(&format!(" Range: {min} to {max}."));
        }
        if default.is_empty() {
            description.push_str(" Default: empty.");
        } else {
            description.push_str(&format!(" Default: {default}."));
        }
        if info.kind == PropertyKind::Text {
            description.push_str(" Empty: the value in server.properties.");
        }
        description.push_str(&format!(" server.properties: {}.", info.key));
        entries.push(Entry {
            key: info.key.to_owned(),
            area: "Server".to_owned(),
            name: title(info.key),
            description,
            env: env_name(info.key),
            default,
            rules,
            preset: String::new(),
        });
    }

    rule_entries::<ServerRules>(&mut entries, "");
    rule_entries::<WorldRules>(&mut entries, " Applies to every world.");

    let essentials = match kind {
        Egg::Advanced => ESSENTIALS,
        Egg::Simple => SIMPLE,
    };
    let mut variables = Vec::with_capacity(entries.len());
    for essential in essentials {
        let i = entries
            .iter()
            .position(|e| e.key == essential.key)
            .ok_or_else(|| format!("egg: essential {:?} is not a setting", essential.key))?;
        let mut entry = entries.remove(i);
        if let Some(choices) = entry.rules.strip_prefix("nullable|string|in:default,") {
            // A switch or dropdown of its own, starting at the default profile's value.
            entry.default = entry.preset.clone();
            entry.rules = format!("required|string|in:{choices}");
        }
        variables.push(variable(
            essential.name,
            essential.description,
            &entry.env,
            &entry.default,
            &entry.rules,
        ));
    }
    // Stable: the catalogue order stays inside an area.
    let rank = |area: &str| AREAS.iter().position(|a| *a == area).unwrap_or(AREAS.len());
    entries.sort_by_key(|e| rank(&e.area));
    if kind == Egg::Simple {
        entries.clear();
    }
    for entry in &entries {
        variables.push(variable(
            &format!("{}: {}", entry.area, entry.name),
            &entry.description,
            &entry.env,
            &entry.default,
            &entry.rules,
        ));
    }

    let (name, description) = match kind {
        Egg::Advanced => (
            "Vanillite",
            "Vanillite: a Minecraft Java Edition server written in Rust that behaves like \
             Vanilla in game; fast, multithreaded and low on RAM. The startup page shows the \
             essential settings first, then every server.properties and vanillite.toml \
             setting, grouped by area. For only the basic settings use the egg \
             \"Vanillite (Simple)\". The image (Startup page) picks the Minecraft version; \"Latest Minecraft\" follows new \
             versions on the next restart, other new versions appear after re-importing this \
             egg. https://github.com/hthoene/vanillite",
        ),
        Egg::Simple => (
            "Vanillite (Simple)",
            "Vanillite: a Minecraft Java Edition server that plays exactly like the official \
             one, but is faster and needs less memory. This egg shows only the basic \
             settings; everything else uses the defaults of the chosen server type. For every \
             setting use the egg \"Vanillite\" (servers can switch between the two). The image (Startup page) picks the Minecraft version; \"Latest Minecraft\" follows new \
             versions on the next restart, other new versions appear after re-importing this \
             egg. \
             https://github.com/hthoene/vanillite",
        ),
    };
    let config = |value: Value| Value::String(serde_json::to_string(&value).unwrap_or_default());
    let egg = json!({
        "_comment": "DO NOT EDIT: generated from the vanillite-config schema by `cargo xtask egg`",
        "meta": { "version": "PTDL_v2", "update_url": null },
        "exported_at": "2026-11-24T00:00:00+00:00",
        "name": name,
        "author": "hthoene@users.noreply.github.com",
        "description": description,
        "features": ["eula"],
        "docker_images": docker_images(root)?,
        "file_denylist": [],
        "startup": STARTUP,
        "config": {
            "files": config(json!({})),
            "startup": config(json!({ "done": DONE })),
            "logs": config(json!({})),
            "stop": "stop",
        },
        "scripts": {
            "installation": {
                "script": INSTALL_SCRIPT,
                "container": "ghcr.io/pterodactyl/installers:alpine",
                "entrypoint": "ash",
            }
        },
        "variables": variables,
    });
    let mut out = serde_json::to_string_pretty(&egg).map_err(|e| e.to_string())?;
    out.push('\n');
    Ok(out)
}

/// The images the panel offers, the default first: one entry per Minecraft version of
/// [`VERSIONS_PATH`] (the version of the sources first, then newest first), then the moving tags
/// that follow new Minecraft versions. Tags as `scripts/release/meta.sh` pushes them.
fn docker_images(root: &Path) -> Result<Value> {
    let mut versions = minecraft_versions(root)?;
    let current = current_version(root)?.id;
    if let Some(i) = versions.iter().position(|v| v.id == current) {
        let v = versions.remove(i);
        versions.insert(0, v);
    }
    let mut images = serde_json::Map::new();
    for v in &versions {
        let (name, tag) = if v.channel == STABLE {
            (format!("Minecraft {}", v.id), format!("mc{}", v.id))
        } else {
            (
                format!("Minecraft {} - {}", v.id, title(&v.channel)),
                format!("mc{}-{}", v.id, v.channel),
            )
        };
        images.insert(name, format!("{REPOSITORY}:{tag}").into());
    }
    if versions.iter().any(|v| v.channel == STABLE) {
        images.insert(
            "Latest Minecraft".to_owned(),
            format!("{REPOSITORY}:latest").into(),
        );
    }
    // Stable releases move the beta tags too, so `beta` always holds the newest build.
    images.insert(
        "Latest Minecraft - Beta".to_owned(),
        format!("{REPOSITORY}:beta").into(),
    );
    Ok(Value::Object(images))
}

/// The Minecraft versions with images, generated into [`VERSIONS_PATH`]. One line per version:
/// `<minecraft version> <channel>`, the channel of its newest images (`beta`, or `stable` once a
/// stable release for it exists).
const VERSIONS_PATH: &str = "deploy/minecraft-versions.txt";
/// The channel of a release without a pre-release part.
const STABLE: &str = "stable";
const VERSIONS_HEADER: &str = "\
# Minecraft versions Vanillite publishes Docker images for: `<minecraft version> <channel>`,
# newest first, channel `beta` or `stable` (a stable release exists). The source of the image
# dropdown of the Pterodactyl eggs and of the moving Docker tags (scripts/release/meta.sh moves
# `beta`/`latest` only for the newest version). `cargo xtask egg` adds the version of the sources;
# remove a line once that version gets no more images (its tags stay pullable).
";

/// One line of [`VERSIONS_PATH`].
struct McVersion {
    id: String,
    channel: String,
}

/// The Minecraft version of the sources and the release channel of the workspace version.
fn current_version(root: &Path) -> Result<McVersion> {
    let source = crate::read(&root.join(MC_VERSION_PATH))?;
    let id = source
        .lines()
        .find_map(|l| l.strip_prefix("id = \""))
        .and_then(|l| l.strip_suffix('"'))
        .ok_or_else(|| format!("{MC_VERSION_PATH}: no id"))?;
    let channel = match env!("CARGO_PKG_VERSION").split_once('-') {
        Some((_, pre)) => pre.split('.').next().unwrap_or(pre),
        None => STABLE,
    };
    if channel != STABLE && channel != "beta" {
        return Err(format!(
            "release channel {channel}: the eggs and {VERSIONS_PATH} know only beta and stable"
        ));
    }
    Ok(McVersion {
        id: id.to_owned(),
        channel: channel.to_owned(),
    })
}

/// The committed versions plus the version of the sources (a stable entry stays stable), newest
/// first.
fn minecraft_versions(root: &Path) -> Result<Vec<McVersion>> {
    let current = current_version(root)?;
    let text = crate::read(&root.join(VERSIONS_PATH)).unwrap_or_default();
    let mut versions = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(id), Some(channel), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!(
                "{VERSIONS_PATH}: `{line}` is not `<version> <channel>`"
            ));
        };
        if channel != STABLE && channel != "beta" {
            return Err(format!(
                "{VERSIONS_PATH}: {id}: channel must be beta or stable"
            ));
        }
        if !id
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(format!("{VERSIONS_PATH}: {id} is not a release version"));
        }
        if versions.iter().any(|v: &McVersion| v.id == id) {
            return Err(format!("{VERSIONS_PATH}: {id} is listed twice"));
        }
        versions.push(McVersion {
            id: id.to_owned(),
            channel: channel.to_owned(),
        });
    }
    match versions.iter_mut().find(|v| v.id == current.id) {
        Some(v) if current.channel == STABLE => v.channel = current.channel,
        Some(_) => {}
        None => versions.push(current),
    }
    versions.sort_by_cached_key(|v| {
        let parts: Vec<u64> = v.id.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        std::cmp::Reverse(parts)
    });
    Ok(versions)
}

/// The text of [`VERSIONS_PATH`]: the versions newest first.
fn render_versions(root: &Path) -> Result<String> {
    let versions = minecraft_versions(root)?;
    let mut out = VERSIONS_HEADER.to_owned();
    for v in versions {
        out.push_str(&format!("{} {}\n", v.id, v.channel));
    }
    Ok(out)
}

fn rule_entries<R: RuleSet>(entries: &mut Vec<Entry>, scope: &str) {
    let profile = vanillite_config::Profile::default();
    let preset = R::for_profile(profile);
    for info in R::RULES {
        if HIDDEN_GROUPS.contains(&info.group) {
            continue;
        }
        let shown = preset
            .get(info.key)
            .map(|v| match v {
                toml::Value::String(s) => s,
                other => other.to_string(),
            })
            .unwrap_or_default();
        let fixed = info.value.ty == ValueType::Bool || !info.value.choices.is_empty();
        let mut description = sentence(info.doc);
        description.push_str(scope);
        if let Some((min, max)) = info.range {
            description.push_str(&format!(" Range: {min} to {max}."));
        }
        if let Some(keyword) = info.value.keyword {
            description.push_str(&format!(" Also accepts {keyword}."));
        }
        let keep = if fixed { KEEP } else { "Empty" };
        description.push_str(&format!(
            " {keep}: the profile's value ({profile} profile: {shown}). vanillite.toml: {}.",
            info.key
        ));
        let (area, sub) = info.group.split_once('.').unwrap_or((info.group, ""));
        let name = if sub.is_empty() {
            title(info.name)
        } else {
            format!("{} › {}", title(sub), title(info.name))
        };
        entries.push(Entry {
            key: info.key.to_owned(),
            area: title(area),
            name,
            description,
            env: env_name(info.key),
            default: if fixed {
                KEEP.to_owned()
            } else {
                String::new()
            },
            rules: rule_rules(info),
            preset: shown,
        });
    }
}

/// The panel's validation rules of a rule variable.
fn rule_rules(info: &RuleInfo) -> String {
    if !info.value.choices.is_empty() {
        return format!("nullable|string|in:{KEEP},{}", info.value.choices.join(","));
    }
    let number = match info.value.ty {
        ValueType::Bool => return format!("nullable|string|in:{KEEP},true,false"),
        ValueType::Text => return "nullable|string".to_owned(),
        ValueType::Integer => ("integer", "[0-9]*"),
        ValueType::Number => ("numeric", "[0-9]*(\\.[0-9]+)?"),
    };
    if let Some(keyword) = info.value.keyword {
        // The keyword or a number (the range is checked by the server on start). No `|` in the
        // pattern: the panel splits the rules on it.
        return format!(
            "nullable|string|regex:/^(?!{keyword}[0-9])({keyword})?{}$/",
            number.1
        );
    }
    match info.range {
        Some((min, max)) => format!("nullable|{}|between:{min},{max}", number.0),
        None => format!("nullable|{}", number.0),
    }
}

fn variable(name: &str, description: &str, env: &str, default: &str, rules: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "env_variable": env,
        "default_value": default,
        "user_viewable": true,
        "user_editable": true,
        "rules": rules,
        "field_type": "text",
    })
}

/// Joins the lines of a doc comment into one paragraph.
fn sentence(doc: &str) -> String {
    doc.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `max-players` → `Max Players`, keeping common abbreviations upper case.
fn title(key: &str) -> String {
    key.split(['-', '.', '_'])
        .map(|word| match word {
            "motd" | "rcon" | "jmx" | "tnt" | "nbt" | "id" | "ips" | "url" | "io" | "xp" | "ip"
            | "rc" => match word {
                "ips" => "IPs".to_owned(),
                other => other.to_ascii_uppercase(),
            },
            "sha1" => "SHA-1".to_owned(),
            "xray" => "X-Ray".to_owned(),
            _ => {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Checks the egg against the PTDL_v2 format the panel imports, and every default value against
/// its own validation rules (the panel checks them when a server is created).
pub fn check_format(egg: &str) -> Result<()> {
    let egg: Value = serde_json::from_str(egg).map_err(|e| format!("egg: {e}"))?;
    let str_at = |pointer: &str| -> Result<&str> {
        egg.pointer(pointer)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("egg: {pointer} must be a string"))
    };
    if str_at("/meta/version")? != "PTDL_v2" {
        return Err("egg: meta.version must be PTDL_v2".to_owned());
    }
    for pointer in [
        "/name",
        "/author",
        "/description",
        "/startup",
        "/config/stop",
    ] {
        if str_at(pointer)?.is_empty() {
            return Err(format!("egg: {pointer} is empty"));
        }
    }
    if !str_at("/author")?.contains('@') {
        return Err("egg: author must be an e-mail address".to_owned());
    }
    for pointer in [
        "/scripts/installation/script",
        "/scripts/installation/container",
        "/scripts/installation/entrypoint",
    ] {
        str_at(pointer)?;
    }
    for pointer in ["/config/files", "/config/startup", "/config/logs"] {
        let parsed: Value = serde_json::from_str(str_at(pointer)?)
            .map_err(|e| format!("egg: {pointer} is not JSON: {e}"))?;
        if !parsed.is_object() {
            return Err(format!("egg: {pointer} must hold a JSON object"));
        }
    }
    let images = egg
        .pointer("/docker_images")
        .and_then(Value::as_object)
        .filter(|m| !m.is_empty() && m.values().all(Value::is_string))
        .ok_or("egg: docker_images must map names to images")?;
    // The panel stores the images as `name|image` lines and rejects names with other characters
    // than letters, digits, `_`, `#`, `.`, `/`, `-` and spaces (e.g. a comma).
    for (name, image) in images {
        let image = image.as_str().unwrap_or_default();
        let name_ok = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " _#./-".contains(c));
        let image_ok = image
            .strip_prefix('~')
            .unwrap_or(image)
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " _./-:@".contains(c));
        if !name_ok || !image_ok || image.is_empty() {
            return Err(format!(
                "egg: docker image \"{name}|{image}\" is not accepted by the panel"
            ));
        }
    }
    for pointer in ["/features", "/file_denylist"] {
        egg.pointer(pointer)
            .and_then(Value::as_array)
            .filter(|a| a.iter().all(Value::is_string))
            .ok_or_else(|| format!("egg: {pointer} must be a list of strings"))?;
    }

    let variables = egg
        .pointer("/variables")
        .and_then(Value::as_array)
        .ok_or("egg: variables must be a list")?;
    let mut envs = BTreeSet::new();
    for var in variables {
        let field = |name: &str| -> Result<&str> {
            var.get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("egg variable {var}: {name} must be a string"))
        };
        let env = field("env_variable")?;
        let name = field("name")?;
        if name.is_empty() || name.chars().count() > 191 {
            return Err(format!("{env}: name must have 1 to 191 characters"));
        }
        field("description")?;
        if env.is_empty()
            || env.len() > 191
            || !env.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(format!("{env}: invalid environment variable name"));
        }
        if RESERVED.contains(&env) {
            return Err(format!("{env}: reserved by Pterodactyl"));
        }
        if !envs.insert(env) {
            return Err(format!("{env}: declared twice"));
        }
        for flag in ["user_viewable", "user_editable"] {
            if !var.get(flag).is_some_and(Value::is_boolean) {
                return Err(format!("{env}: {flag} must be true or false"));
            }
        }
        let default = field("default_value")?;
        validate(field("rules")?, default)
            .map_err(|e| format!("{env}: default {default:?}: {e}"))?;
    }
    Ok(())
}

/// Checks a value against Laravel validation rules, as far as the egg uses them.
fn validate(rules: &str, value: &str) -> Result<()> {
    let rules: Vec<&str> = rules.split('|').collect();
    if value.is_empty() {
        return if rules.contains(&"nullable") {
            Ok(())
        } else {
            Err("is required".to_owned())
        };
    }
    let numeric = rules.contains(&"integer") || rules.contains(&"numeric");
    for rule in rules {
        let (name, param) = rule.split_once(':').unwrap_or((rule, ""));
        let ok = match name {
            "required" | "nullable" | "string" => true,
            "integer" => value.parse::<i64>().is_ok(),
            "numeric" => value.parse::<f64>().is_ok(),
            "in" => param.split(',').any(|v| v == value),
            "between" => {
                let (min, max) = param
                    .split_once(',')
                    .and_then(|(a, b)| Some((a.parse::<f64>().ok()?, b.parse::<f64>().ok()?)))
                    .ok_or_else(|| format!("invalid rule {rule}"))?;
                let size = if numeric {
                    value.parse::<f64>().unwrap_or(f64::NAN)
                } else {
                    value.chars().count() as f64
                };
                (min..=max).contains(&size)
            }
            "regex" => regex_matches(param, value)?,
            _ => return Err(format!("unknown rule {rule}")),
        };
        if !ok {
            return Err(format!("fails {rule}"));
        }
    }
    Ok(())
}

/// Matches the only regex shape the egg generates: `/^(?!K[0-9])(K)?<digits>$/`.
fn regex_matches(pattern: &str, value: &str) -> Result<bool> {
    let body = pattern
        .strip_prefix("/^(?!")
        .and_then(|p| p.strip_suffix("$/"))
        .ok_or_else(|| format!("unexpected regex {pattern}"))?;
    let (keyword, rest) = body
        .split_once("[0-9])(")
        .ok_or_else(|| format!("unexpected regex {pattern}"))?;
    let number = rest
        .strip_prefix(keyword)
        .and_then(|r| r.strip_prefix(")?"))
        .ok_or_else(|| format!("unexpected regex {pattern}"))?;
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let number_ok = |s: &str| match number {
        "[0-9]*" => digits(s),
        "[0-9]*(\\.[0-9]+)?" => match s.split_once('.') {
            Some((a, b)) => digits(a) && !b.is_empty() && digits(b),
            None => digits(s),
        },
        _ => false,
    };
    Ok(value == keyword || number_ok(value))
}

fn default_image() -> String {
    let image = std::env::var("VANILLITE_IMAGE")
        .unwrap_or_else(|_| crate::docker::DEFAULT_IMAGE.to_owned());
    format!("{image}:dev")
}

/// Starts `image` like Pterodactyl's Wings does (startup command and default variables from the
/// egg, the server folder at /home/container, a non-root user, a read-only root file system, a
/// terminal) and checks that the server starts and answers a status ping on the published port.
fn smoke(root: &Path, image: &str) -> Result<()> {
    const NAME: &str = "vanillite-egg-smoke";
    const PORT: &str = "30950";
    let egg: Value =
        serde_json::from_str(&render(root, Egg::Advanced)?).map_err(|e| e.to_string())?;
    let host = crate::docker::docker_host();
    let docker = || crate::docker::docker(host.as_deref());
    let _ = docker()
        .args(["rm", "-f", NAME])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let mut cmd = docker();
    cmd.args([
        "run",
        "-d",
        "-i",
        "-t",
        "--name",
        NAME,
        "--user",
        "988:988",
        "--read-only",
    ])
    .args(["--tmpfs", "/tmp:rw,exec,mode=1777", "--memory", "4g"])
    .args(["--mount", "type=tmpfs,dst=/home/container,tmpfs-mode=1777"])
    .args(["-p", &format!("{PORT}:{PORT}")])
    .args([
        "-e",
        &format!("STARTUP={}", egg["startup"].as_str().unwrap_or_default()),
    ])
    .args([
        "-e",
        &format!("SERVER_PORT={PORT}"),
        "-e",
        "SERVER_IP=0.0.0.0",
    ])
    .args(["-e", "SERVER_MEMORY=4096", "-e", "TZ=UTC"]);
    for var in egg["variables"].as_array().into_iter().flatten() {
        let env = var["env_variable"].as_str().unwrap_or_default();
        // Accepting the EULA is the one choice the admin has to make.
        let value = if env == "VANILLITE_EULA" {
            "true"
        } else {
            var["default_value"].as_str().unwrap_or_default()
        };
        cmd.args(["-e", &format!("{env}={value}")]);
    }
    cmd.arg(image).stdout(Stdio::null());
    crate::devenv::run(&mut cmd)?;

    let result = wait_and_ping(&docker, image, NAME, PORT);
    if result.is_err() {
        let _ = docker().args(["logs", "--tail", "50", NAME]).status();
    }
    let _ = docker()
        .args(["rm", "-f", NAME])
        .stdout(Stdio::null())
        .status();
    result
}

fn wait_and_ping(docker: &dyn Fn() -> Command, image: &str, name: &str, port: &str) -> Result<()> {
    let start = Instant::now();
    loop {
        let logs = docker()
            .args(["logs", name])
            .output()
            .map_err(|e| format!("docker logs: {e}"))?;
        let text = String::from_utf8_lossy(&logs.stdout);
        if text.contains(DONE) {
            println!("started in {:.0} s", start.elapsed().as_secs_f64());
            break;
        }
        let running = docker()
            .args(["inspect", "-f", "{{.State.Running}}", name])
            .output()
            .map_err(|e| format!("docker inspect: {e}"))?;
        if String::from_utf8_lossy(&running.stdout).trim() != "true" {
            return Err("the container stopped before the server was started".to_owned());
        }
        if start.elapsed() > Duration::from_secs(600) {
            return Err("the server did not start within 10 minutes".to_owned());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    // The ping comes from a second container on the host network, through the published port.
    crate::devenv::run(docker().args([
        "run",
        "--rm",
        "--network",
        "host",
        image,
        "health",
        "--port",
        port,
    ]))?;
    println!("status ping answered on port {port}");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn committed_egg_is_current() {
        if let Err(e) = super::check(&crate::repo_root()) {
            panic!("{e}");
        }
    }
}
