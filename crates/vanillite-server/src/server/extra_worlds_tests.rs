//! Extra worlds: loaded from `[extra-worlds]`, created and unloaded through the plugin
//! services, players moved between worlds, folders locked while loaded, the same index and
//! seed after loading again, in both tick modes.

use vanillite_abi::{environment, status, world_flag};
use vanillite_plugin_host::{Location, ServerAccess, WorldCreator};

use super::*;
use crate::dimensions::Environment;

fn start(name: &str, toml: &str) -> (PathBuf, Server) {
    let dir = std::env::temp_dir().join(format!(
        "vanillite-extra-worlds-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    fs::write(
        dir.join("server.properties"),
        "view-distance=2\nsimulation-distance=2\n",
    )
    .unwrap();
    fs::write(
        dir.join(VANILLITE_FILE),
        format!("profile = \"vanilla\"\n{toml}"),
    )
    .unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    builder.harness = true;
    (dir, builder.start().expect("server starts"))
}

impl Server {
    fn step_once(&mut self) {
        self.wait_until_next_tick(0, true);
        self.tick_server();
        self.wait_until_next_tick(0, true);
    }

    /// Ticks until the player `uuid` is in `world`: a move between worlds places the player
    /// once the destination chunks are generated, on the worker threads (in wall-clock time,
    /// not in a fixed number of these unthrottled ticks).
    fn step_until_held(&mut self, world: WorldId, uuid: u128) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.holds_player(world, uuid) && Instant::now() < deadline {
            self.step_once();
        }
    }

    fn services(&self) -> crate::plugins::Services {
        crate::plugins::Services::new(&self.global, &self.compute, &self.config, &self.dir)
    }

    fn world_of(&self, uuid: u128) -> Option<WorldId> {
        self.global
            .players
            .snapshot()
            .iter()
            .find(|p| p.uuid.0 == uuid)
            .map(|p| p.world)
    }

    fn holds_player(&self, world: WorldId, uuid: u128) -> bool {
        match &self.worlds {
            Worlds::Sequential(worlds) => worlds[world.index()]
                .as_ref()
                .is_some_and(|w| w.entities().player_by_uuid(uuid).is_some()),
            Worlds::Parallel(_) => false,
        }
    }
}

fn to(world: WorldId) -> Location {
    Location {
        world: u32::from(world.0),
        pos: [0.5, 5.0, 0.5],
        yaw: 0.0,
        pitch: 0.0,
    }
}

#[test]
fn extra_worlds_load_move_players_and_unload() {
    let (dir, mut server) = start(
        "sequential",
        "[extra-worlds.lobby]\ngenerator = \"flat\"\nseed = \"42\"\n",
    );
    let dims = server.global.dimensions.clone();
    let lobby = dims
        .world("minecraft:lobby")
        .expect("the configured world is loaded");
    assert_eq!(lobby, WorldId(3));
    assert_eq!(dims.seed(lobby), Some(42));
    assert_eq!(dims.extra_environment(lobby), Some(Environment::Normal));
    assert!(dir.join("lobby/level.dat").exists());
    assert!(
        dir.join("lobby/data/minecraft/world_gen_settings.dat")
            .exists()
    );
    assert!(
        lock_world(&dir.join("lobby")).is_err(),
        "a loaded world is locked"
    );

    // A plugin creates a world: its index now, loaded in the next global phase.
    let services = server.services();
    let arena = services
        .create_world(&WorldCreator {
            name: "Arena",
            environment: environment::NETHER,
            generator: "void",
            seed: Some(7),
            structures: true,
        })
        .unwrap();
    let arena = WorldId(u16::try_from(arena).unwrap());
    assert!(!dims.is_loaded(arena));
    server.step_once();
    assert_eq!(dims.world("minecraft:arena"), Some(arena));
    assert_eq!(dims.extra_environment(arena), Some(Environment::Nether));
    let info = services.world_info(u32::from(arena.0)).unwrap();
    assert_eq!(info.environment, environment::NETHER);
    assert_eq!(info.flags, world_flag::LOADED | world_flag::EXTRA);
    assert_eq!((info.seed, info.folder.as_str()), (7, "Arena"));
    assert_eq!(services.world_count(), 5);

    // A player goes into the lobby and back; a world with players stays loaded.
    let uuid = 0x1234;
    let _inbound = server.join_unread("Traveller", uuid, None);
    for _ in 0..3 {
        server.step_once();
    }
    assert_eq!(server.world_of(uuid), Some(WorldId(0)));
    services.move_player(uuid, to(lobby)).unwrap();
    server.step_until_held(lobby, uuid);
    assert_eq!(server.world_of(uuid), Some(lobby));
    assert!(server.holds_player(lobby, uuid));
    assert!(!server.holds_player(WorldId(0), uuid));
    assert_eq!(
        services.unload_world(u32::from(lobby.0), true),
        Err(status::INVALID_ARGUMENT)
    );
    assert!(server.unload_world(lobby, true).is_err());
    services.move_player(uuid, to(WorldId(0))).unwrap();
    server.step_until_held(WorldId(0), uuid);
    assert!(server.holds_player(WorldId(0), uuid));

    // Unloading releases the folder; loading again keeps the index and the seed.
    assert_eq!(services.unload_world(1, true), Err(status::WRONG_OWNER));
    services.unload_world(u32::from(lobby.0), true).unwrap();
    server.step_once();
    assert!(!dims.is_loaded(lobby));
    assert_eq!(dims.world("minecraft:lobby"), None);
    drop(lock_world(&dir.join("lobby")).expect("an unloaded world's folder is free"));
    assert_eq!(services.load_world("missing"), Err(status::NOT_FOUND));
    assert_eq!(services.load_world("lobby"), Ok(u32::from(lobby.0)));
    server.step_once();
    assert_eq!(dims.world("minecraft:lobby"), Some(lobby));
    assert_eq!(dims.seed(lobby), Some(42));

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn extra_worlds_tick_on_their_own_threads() {
    let (dir, mut server) = start(
        "parallel",
        "[performance.tick]\nparallel-dimensions = true\n\n[extra-worlds.lobby]\ngenerator = \"void\"\n",
    );
    let dims = server.global.dimensions.clone();
    let lobby = dims
        .world("minecraft:lobby")
        .expect("the configured world is loaded");
    for _ in 0..3 {
        server.step_once();
    }
    server.unload_world(lobby, true).unwrap();
    assert!(!dims.is_loaded(lobby));
    server.step_once();
    let spec =
        crate::worlds::WorldSpec::from_config(&vanillite_config::ExtraWorld::new("lobby")).unwrap();
    assert_eq!(server.open_world(&spec, false).unwrap(), lobby);
    server.step_once();
    assert!(dims.is_loaded(lobby));
    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn folders_of_the_default_world_are_no_extra_worlds() {
    let (dir, mut server) = start("names", "");
    for name in ["world", "WORLD"] {
        let spec = crate::worlds::WorldSpec::from_config(&vanillite_config::ExtraWorld::new(name))
            .unwrap();
        assert!(server.open_world(&spec, true).is_err(), "{name}");
    }
    assert!(crate::worlds::dimension_key("../up").is_err());
    assert!(crate::worlds::dimension_key("").is_err());
    assert_eq!(
        crate::worlds::dimension_key("Lobby_1").unwrap(),
        "minecraft:lobby_1"
    );
    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}
