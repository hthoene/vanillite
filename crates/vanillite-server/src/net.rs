//! The network as the server sees it: data packs for the configuration phase, the built-in
//! network pipeline, and joined connections entering the game.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vanillite_base::Vec3;
use vanillite_config::Config;
use vanillite_datapack::LoadedPacks;
use vanillite_game::player::join::LevelInfo;
use vanillite_game::player::{ClientInformation, GameProfile, PacketSink, ProfileProperty};
use vanillite_net::{ConfigurationData, Joined, NetConfig, Network, StatusInfo};
use vanillite_protocol::text::Component;
use vanillite_protocol::types::Identifier;
use vanillite_storage::player_data::PlayerDataStore;

use crate::players::{Arrival, NetSink, WorldPlayers};
use crate::server::StartError;

/// Where the Vanilla data pack comes from.
#[derive(Clone, Debug, Default)]
pub enum VanillaPack {
    /// Downloaded once from Mojang into `<server dir>/cache` (default).
    #[default]
    Download,
    /// Extracted from this local copy of the pinned server jar.
    Jar(PathBuf),
    /// No data packs: empty registries. Only for tests whose clients do not need them.
    None,
}

/// Data packs and what the worlds derive from them.
pub(crate) struct Prepared {
    pub(crate) configuration: ConfigurationData,
    pub(crate) worlds: Vec<WorldPlayers>,
    pub(crate) chat_types: crate::global::ChatTypes,
    pub(crate) registries: std::collections::BTreeMap<String, Vec<String>>,
    /// The loaded data packs; `None` with [`VanillaPack::None`].
    pub(crate) packs: Option<LoadedPacks>,
    /// The pack repository and the packs selected at start.
    pub(crate) selected: crate::packs::Selected,
    /// Seed and generators of the world.
    pub(crate) world_gen: crate::world_setup::WorldGen,
    /// Vanilla's English translations (empty without the Vanilla pack).
    pub(crate) language: std::collections::HashMap<String, String>,
    /// `level.dat` has a spawn; else the server searches one (`setInitialSpawn`).
    pub(crate) spawn_known: bool,
}

/// What the players of the world `(dimension, dimension type)` are told of it: `levels` all
/// dimension keys, `seed` its seed, `spawn` its spawn.
pub(crate) fn level_info(
    config: &Config,
    packs: Option<&LoadedPacks>,
    levels: Vec<Identifier>,
    (dimension, kind): (&str, &str),
    seed: i64,
    is_flat: bool,
    spawn: (Vec3, f32),
) -> LevelInfo {
    let properties = &config.properties;
    let rules = config.world_rules(dimension);
    let view_distance = rules
        .distances
        .view_distance
        .or(u8::try_from(properties.view_distance.clamp(2, 32)).unwrap_or(10));
    let simulation_distance = rules
        .distances
        .simulation_distance
        .or(u8::try_from(properties.simulation_distance.clamp(2, 32)).unwrap_or(10));
    LevelInfo {
        levels,
        dimension: Identifier::new(dimension).unwrap_or_else(|_| Identifier::vanilla("overworld")),
        dimension_type: i32::try_from(registry_index(packs, "minecraft:dimension_type", kind))
            .unwrap_or(0),
        hashed_seed: vanillite_worldgen::biome::obfuscate_seed(seed),
        is_flat,
        sea_level: 63,
        spawn,
        max_players: properties.max_players,
        view_distance: i32::from(view_distance),
        simulation_distance: i32::from(simulation_distance),
        hardcore: properties.hardcore,
        online_mode: properties.online_mode,
    }
}

/// Where new players spawn: `spawn` of `level.dat`, else on top of the flat layers, or above
/// the noise terrain until the server searched the new world's spawn
/// (`crate::world::initial_spawn`). The flag says whether `level.dat` had one.
fn world_spawn(level_dir: &Path, is_flat: bool) -> ((Vec3, f32), bool) {
    let spawn = vanillite_storage::level_dat::LevelData::read(level_dir)
        .ok()
        .flatten()
        .and_then(|level| level.spawn);
    let pos = spawn.as_ref().map(|s| s.pos.to_vec());
    let yaw = spawn.as_ref().map_or(0.0, |s| s.yaw);
    let known = spawn.is_some();
    let spawn = match pos.as_deref() {
        Some(&[x, y, z]) => (
            Vec3::new(f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5),
            yaw,
        ),
        // The classic flat layers end 4 blocks above the overworld's bottom (-64).
        _ if is_flat => (Vec3::new(0.5, -60.0, 0.5), 0.0),
        // The top of the overworld; the player falls onto the terrain.
        _ => (Vec3::new(0.5, 320.0, 0.5), 0.0),
    };
    (spawn, known)
}

fn registry_index(packs: Option<&LoadedPacks>, registry: &str, entry: &str) -> usize {
    packs
        .and_then(|p| p.registry(registry))
        .and_then(|r| r.id_of(entry))
        .unwrap_or(0)
}

/// The Vanilla data pack's root in the cache `cache` (downloaded once), if one is used.
fn vanilla_root(pack: &VanillaPack, cache: &Path) -> Result<Option<PathBuf>, StartError> {
    Ok(match pack {
        VanillaPack::None => None,
        VanillaPack::Download | VanillaPack::Jar(_) => {
            let jar = match pack {
                VanillaPack::Jar(jar) => Some(jar.as_path()),
                _ => None,
            };
            let root = vanillite_datapack::vanilla::ensure_vanilla_pack(cache, jar)
                .map_err(|e| StartError::Io(format!("Vanilla data pack: {e}")))?;
            Some(root)
        }
    })
}

/// The data packs enabled in the world `level_dir` (with `initial` for a new world), loaded,
/// for the offline commands; `None` without the Vanilla data pack.
pub(crate) fn load_world_packs(
    pack: &VanillaPack,
    dir: &Path,
    level_dir: &Path,
    initial: (&str, &str),
) -> Result<Option<LoadedPacks>, StartError> {
    let cache = dir.join("cache");
    let Some(root) = vanilla_root(pack, &cache)? else {
        return Ok(None);
    };
    let selected = crate::packs::select(Some(root), level_dir, &cache, initial);
    let packs = vanillite_datapack::load_opened(&selected.opened)
        .map_err(|e| StartError::Io(format!("data packs: {e}")))?;
    Ok(Some(packs))
}

/// Selects and loads the data packs and prepares each world's players' side.
pub(crate) fn prepare(
    config: &Config,
    dir: &Path,
    level_dir: &Path,
    pack: &VanillaPack,
    player_store: &PlayerDataStore,
) -> Result<Prepared, StartError> {
    let cache = dir.join("cache");
    let vanilla = vanilla_root(pack, &cache)?;
    let language = vanilla
        .as_deref()
        .map(|root| load_language(root, pack))
        .unwrap_or_default();
    let loads_registries = vanilla.is_some();
    let properties = &config.properties;
    let selected = crate::packs::select(
        vanilla,
        level_dir,
        &cache,
        (
            &properties.initial_enabled_packs,
            &properties.initial_disabled_packs,
        ),
    );
    let packs = if loads_registries {
        let packs = vanillite_datapack::load_opened(&selected.opened)
            .map_err(|e| StartError::Io(format!("data packs: {e}")))?;
        for warning in &packs.warnings {
            log::warn!("{warning}");
        }
        Some(packs)
    } else {
        None
    };
    let features: Vec<&str> = selected.packs.features.iter().map(String::as_str).collect();
    let configuration = match &packs {
        Some(packs) => ConfigurationData::from_packs(packs, &features),
        None => ConfigurationData::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vanillite_protocol::packets::configuration::clientbound::UpdateTags {
                registries: Vec::new(),
            },
        ),
    };
    crate::world_setup::install_facts(packs.as_ref())?;
    let world_gen = crate::world_setup::WorldGen::load_or_create(
        level_dir,
        &config.properties,
        packs.as_ref(),
    )?;
    let is_flat = world_gen.is_flat();
    let (spawn, spawn_known) = world_spawn(level_dir, is_flat);

    let dimensions = world_gen.dimensions(packs.as_ref());
    let levels: Vec<Identifier> = dimensions
        .names()
        .map(|d| Identifier::new(d).unwrap_or_else(|_| Identifier::vanilla("overworld")))
        .chain(
            config
                .extra_worlds
                .iter()
                .filter_map(|w| crate::worlds::dimension_key(&w.name).ok())
                .filter_map(|k| Identifier::new(&k).ok()),
        )
        .collect();
    let worlds = dimensions
        .iter()
        .map(|(_, dimension, kind)| {
            let level = level_info(
                config,
                packs.as_ref(),
                levels.clone(),
                (dimension, kind),
                world_gen.seed,
                is_flat,
                spawn,
            );
            let mut players = WorldPlayers::new(level, player_store.clone());
            if dimension == "minecraft:overworld" && config.server.debug.session_recording {
                players.recording = crate::session_recording::SessionRecording::new(Some(
                    dir.join(&config.server.debug.session_recording_dir),
                ));
            }
            players
        })
        .collect();
    let chat_type = |name: &str| {
        i32::try_from(registry_index(packs.as_ref(), "minecraft:chat_type", name)).unwrap_or(0)
    };
    let chat_types = crate::global::ChatTypes {
        chat: chat_type("minecraft:chat"),
        say_command: chat_type("minecraft:say_command"),
        msg_incoming: chat_type("minecraft:msg_command_incoming"),
        msg_outgoing: chat_type("minecraft:msg_command_outgoing"),
        emote_command: chat_type("minecraft:emote_command"),
        team_msg_incoming: chat_type("minecraft:team_msg_command_incoming"),
        team_msg_outgoing: chat_type("minecraft:team_msg_command_outgoing"),
    };
    let registries = [
        "minecraft:timeline",
        "minecraft:world_clock",
        "minecraft:enchantment",
        "minecraft:dialog",
        "minecraft:worldgen/biome",
        // Checked by command arguments (`ResourceArgument`, `/locate structure`).
        "minecraft:damage_type",
        "minecraft:worldgen/structure",
    ]
    .into_iter()
    .map(|name| {
        let entries = packs
            .as_ref()
            .and_then(|p| p.registry(name))
            .map(|r| r.entries().iter().map(|e| e.id.to_string()).collect())
            .unwrap_or_default();
        (name.to_owned(), entries)
    })
    .collect();
    Ok(Prepared {
        configuration,
        worlds,
        chat_types,
        registries,
        packs,
        selected,
        world_gen,
        language,
        spawn_known,
    })
}

/// Vanilla's `en_us.json` (`Language.loadDefault`), for texts of the console and RCON. Without
/// it, translation keys are shown.
fn load_language(root: &Path, pack: &VanillaPack) -> std::collections::HashMap<String, String> {
    let jar = match pack {
        VanillaPack::Jar(jar) => Some(jar.as_path()),
        VanillaPack::Download | VanillaPack::None => None,
    };
    let text = vanillite_datapack::vanilla::ensure_language(root, jar)
        .map_err(|e| e.to_string())
        .and_then(|path| std::fs::read_to_string(&path).map_err(|e| e.to_string()))
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()));
    match text {
        Ok(language) => language,
        Err(e) => {
            log::warn!("English translations unavailable, showing translation keys: {e}");
            std::collections::HashMap::new()
        }
    }
}

/// The running network.
pub(crate) struct NetState {
    pub(crate) network: Network,
    arrivals: vanillite_net::Arrivals,
    status: StatusInfo,
    online: usize,
    /// Player names for Query.
    pub(crate) query: Arc<vanillite_net::query::QueryPlayers>,
    /// State of the random picks of the player sample.
    random: u64,
    /// The Bedrock frontend (`bedrock.enabled`).
    pub(crate) bedrock: Option<(vanillite_bedrock::Frontend, Arc<vanillite_bedrock::Status>)>,
}

impl NetState {
    /// Starts the network pipeline with the settings of `server.properties`.
    /// Without `listen` it binds to a free port of the loopback interface only.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        config: &Config,
        dir: &Path,
        configuration: ConfigurationData,
        listen: bool,
        enforces_secure_chat: bool,
        language: Arc<std::collections::HashMap<String, String>>,
        tap: Arc<vanillite_protocol::tap::PacketTap>,
        io_threads: usize,
        on_idle: Option<vanillite_net::IdleHook>,
    ) -> Result<NetState, StartError> {
        let p = &config.properties;
        let ip: std::net::IpAddr = if p.server_ip.is_empty() {
            std::net::Ipv4Addr::UNSPECIFIED.into()
        } else {
            p.server_ip
                .parse()
                .map_err(|_| StartError::Io(format!("invalid server-ip {:?}", p.server_ip)))?
        };
        let bind = if listen {
            (ip, p.server_port).into()
        } else {
            std::net::SocketAddr::from(([127, 0, 0, 1], 0))
        };
        let net = NetConfig {
            bind,
            io_threads,
            online_mode: p.online_mode,
            prevent_proxy_connections: p.prevent_proxy_connections,
            compression_threshold: p.network_compression_threshold,
            accepts_transfers: p.accepts_transfers,
            // Room for every allowed player plus logins in progress.
            max_connections: usize::try_from(p.max_players)
                .unwrap_or(0)
                .saturating_add(256)
                .max(NetConfig::default().max_connections),
            forwarding_secret: vanillite_net::velocity::Secret::new(
                config.server.proxy.velocity_secret.as_bytes(),
            ),
            allow_direct_connections: config.server.proxy.allow_direct_connections,
            multiversion: vanillite_multiversion::Accepted::parse(
                &config.server.multiversion.versions,
            )
            .map_err(|e| StartError::Io(format!("vanillite.toml multiversion.versions: {e}")))?,
            enable_status: p.enable_status,
            rate_limit: u32::try_from(p.rate_limit).unwrap_or(0),
            log_ips: p.log_ips,
            resource_pack: crate::admin::resource_pack(p),
            server_links: crate::admin::server_links(p),
            code_of_conduct: if p.enable_code_of_conduct {
                crate::admin::codes_of_conduct(dir).map_err(StartError::Io)?
            } else {
                std::collections::BTreeMap::new()
            },
            tap,
            on_idle,
            ..NetConfig::default()
        };
        let status = StatusInfo {
            description: Component::text(p.motd.clone()),
            max_players: p.max_players,
            favicon: crate::admin::server_icon(dir, &p.level_name),
            enforces_secure_chat,
            ..StatusInfo::default()
        };
        let (network, arrivals) = Network::start(net, status.clone(), configuration, None)
            .map_err(|e| StartError::Io(format!("cannot listen on {ip}:{}: {e}", p.server_port)))?;
        let bedrock = if config.server.bedrock.enabled {
            Some(start_bedrock(config, &network, ip, listen, language)?)
        } else {
            None
        };
        Ok(NetState {
            network,
            arrivals,
            status,
            online: 0,
            query: Arc::default(),
            random: u64::from_le_bytes(vanillite_net::rand_bytes()) | 1,
            bedrock,
        })
    }

    /// Stops the Bedrock frontend and the network.
    pub(crate) fn shutdown(self) {
        if let Some((frontend, _)) = self.bedrock {
            frontend.shutdown();
        }
        self.network.shutdown();
    }

    /// The next login waiting for the server's decision.
    pub(crate) fn next_login(&self) -> Option<vanillite_net::LoginCheck> {
        self.arrivals.logins.try_recv().ok()
    }

    /// The next joined connection.
    pub(crate) fn next_join(&self) -> Option<Joined> {
        self.arrivals.joins.try_recv().ok()
    }

    /// Rebuilds the player sample of the server list (`buildPlayerStatus`; Vanilla does it
    /// every 5 seconds): up to 12 players from a random offset, shuffled, anonymous for players
    /// who do not allow listing, none with `hide-online-players`.
    pub(crate) fn update_sample(&mut self, players: &[(String, u128, bool)], hide: bool) {
        let sample = if hide {
            Vec::new()
        } else {
            let state = &mut self.random;
            let mut random = |bound: usize| {
                // xorshift64.
                *state ^= *state << 13;
                *state ^= *state >> 7;
                *state ^= *state << 17;
                usize::try_from(*state % u64::try_from(bound.max(1)).unwrap_or(1)).unwrap_or(0)
            };
            crate::admin::player_sample(players, &mut random)
        };
        if sample != self.status.sample {
            self.status.sample = sample;
            self.network.status().update(&self.status);
        }
    }

    /// Replaces the message of the day of the server list (a plugin's).
    pub(crate) fn set_motd(&mut self, motd: Component) {
        self.status.description = motd;
        self.network.status().update(&self.status);
    }

    /// Updates the player count of the server list.
    pub(crate) fn set_online(&mut self, online: usize) {
        if online != self.online {
            self.online = online;
            self.status.online_players = i32::try_from(online).unwrap_or(i32::MAX);
            self.network.status().update(&self.status);
            if let Some((_, status)) = &self.bedrock {
                status.set_online(u32::try_from(online).unwrap_or(u32::MAX));
            }
        }
    }
}

/// Starts the Bedrock frontend: each Bedrock player becomes a remote virtual connection.
fn start_bedrock(
    config: &Config,
    network: &Network,
    ip: std::net::IpAddr,
    listen: bool,
    language: Arc<std::collections::HashMap<String, String>>,
) -> Result<(vanillite_bedrock::Frontend, Arc<vanillite_bedrock::Status>), StartError> {
    let b = &config.server.bedrock;
    let p = &config.properties;
    let bind = if listen {
        (ip, b.port).into()
    } else {
        std::net::SocketAddr::from(([127, 0, 0, 1], 0))
    };
    let opener = network.virtual_opener();
    let connector: vanillite_bedrock::Connector =
        Arc::new(move |profile, address| opener.open_remote(profile, address));
    let status = Arc::new(vanillite_bedrock::Status::new(
        &p.motd,
        u32::try_from(p.max_players).unwrap_or(0),
    ));
    let bedrock = vanillite_bedrock::BedrockConfig {
        bind,
        online_mode: b.online_mode,
        min_protocol: i32::try_from(b.min_protocol).unwrap_or(i32::MAX),
        name_prefix: b.name_prefix.clone(),
        world_name: p.level_name.clone(),
        max_connections: u32::try_from(p.max_players).unwrap_or(0).saturating_add(64),
        max_connections_per_ip: b.max_connections_per_ip,
        max_packets_per_second: b.max_packets_per_second,
        max_view_distance: b.max_view_distance,
        chunk_cache_sections: usize::try_from(b.chunk_cache_sections).unwrap_or(0),
        identity_file: Some("bedrock-identity.key".into()),
        language,
        ..vanillite_bedrock::BedrockConfig::default()
    };
    let frontend = vanillite_bedrock::Frontend::start(
        bedrock,
        connector,
        Arc::clone(&status),
        usize::from(b.threads),
    )
    .map_err(|e| StartError::Io(format!("cannot listen on UDP {bind} for Bedrock: {e}")))?;
    Ok((frontend, status))
}

/// Turns a joined connection into an arrival for a world.
pub(crate) fn arrival(
    joined: Joined,
    saved: Option<vanillite_storage::player_data::PlayerData>,
    default_mode: vanillite_config::GameMode,
    force_game_mode: bool,
) -> (Arrival, Arc<dyn PacketSink>) {
    let sink: Arc<dyn PacketSink> = Arc::new(NetSink::new(
        joined.outbound,
        joined.latency,
        joined.injector,
    ));
    let saved_mode = saved.as_ref().and_then(|s| match s.game_mode {
        0 => Some(vanillite_config::GameMode::Survival),
        1 => Some(vanillite_config::GameMode::Creative),
        2 => Some(vanillite_config::GameMode::Adventure),
        3 => Some(vanillite_config::GameMode::Spectator),
        _ => None,
    });
    let game_mode = if force_game_mode {
        default_mode
    } else {
        saved_mode.unwrap_or(default_mode)
    };
    let profile = GameProfile {
        name: joined.profile.name.clone(),
        properties: joined
            .profile
            .properties
            .iter()
            .map(|p| ProfileProperty {
                name: p.name.clone(),
                value: p.value.clone(),
                signature: p.signature.clone(),
            })
            .collect(),
    };
    let client = ClientInformation::from(&joined.client_info);
    let inbox = Arc::new(std::sync::Mutex::new(
        vanillite_game::player::chat::ChatInbox::new(client.chat_visibility),
    ));
    let arrival = Arrival {
        uuid: joined.profile.id.0,
        profile,
        client,
        game_mode,
        sink: Arc::clone(&sink),
        inbound: joined.inbound,
        memory_connection: joined.memory,
        virtual_connection: joined.is_virtual,
        saved,
        stats: None,
        advancements: None,
        address: joined.address.to_string(),
        inbox,
        transferred: joined.transferred,
        details: Some(joined.details),
    };
    (arrival, sink)
}
