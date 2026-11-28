//! Milestone 1 smoke test: two in-memory clients join, see each other (spawn, movement,
//! sneaking), get statistics and advancements, one leaves and rejoins at the position it left
//! with its advancements.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use vanillite_data::entity::ids;
use vanillite_protocol::chat::LastSeenUpdate;
use vanillite_protocol::client::TestClient;
use vanillite_protocol::codec::write_varint;
use vanillite_protocol::commands::NodeKind;
use vanillite_protocol::item::ItemStack;
use vanillite_protocol::packets::handshake::Intent;
use vanillite_protocol::packets::play::clientbound::GameMode;
use vanillite_protocol::packets::play::serverbound::{
    AcceptTeleportation, Chat, ChatCommand, ChunkBatchReceived, ClientCommand, ClientCommandAction,
    ClientTickEnd, CommandSuggestion, DigAction, Hand, Input, MoveFlags, MovePlayerPos,
    PlayerActionPacket, PlayerInput, PlayerLoaded, SetCreativeModeSlot, UseItemOn,
};
use vanillite_protocol::sets::ClientboundPlay;
use vanillite_protocol::types::BlockPos as WirePos;
use vanillite_protocol::types::{GameProfile, Uuid};
use vanillite_server::{ServerBuilder, VanillaPack, VirtualConnection, VirtualOpener};

const TIMEOUT: Duration = Duration::from_secs(20);

/// A virtual connection as a byte stream with uncompressed length-prefixed frames, so the
/// protocol's test client can drive it.
struct Mem {
    conn: VirtualConnection,
    input: Vec<u8>,
    pos: usize,
    output: Vec<u8>,
}

impl Read for Mem {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.input.len() {
            let deadline = Instant::now() + TIMEOUT;
            let body = loop {
                match self.conn.from_server.try_recv() {
                    Ok(body) => break body,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return Ok(0),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        if Instant::now() > deadline {
                            return Err(io::ErrorKind::TimedOut.into());
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            };
            self.input.clear();
            self.pos = 0;
            write_varint(&mut self.input, i32::try_from(body.len()).unwrap());
            self.input.extend_from_slice(&body);
        }
        let n = buf.len().min(self.input.len() - self.pos);
        buf[..n].copy_from_slice(&self.input[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for Mem {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(buf);
        // Split complete frames: a VarInt length, then the body.
        loop {
            let mut len = 0usize;
            let mut shift = 0;
            let mut header = 0;
            let mut complete = false;
            for &b in self.output.iter().take(5) {
                len |= usize::from(b & 0x7F) << shift;
                shift += 7;
                header += 1;
                if b & 0x80 == 0 {
                    complete = true;
                    break;
                }
            }
            if !complete || self.output.len() < header + len {
                return Ok(buf.len());
            }
            let body = self.output[header..header + len].to_vec();
            self.output.drain(..header + len);
            self.conn
                .to_server
                .blocking_send(body)
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Player {
    client: TestClient<Mem>,
    uuid: u128,
    /// Root literals of the command tree the player got on joining.
    commands: Vec<String>,
    /// `test:walk` came in an advancements update (it may arrive while waiting for other
    /// packets: the `location` trigger runs every 20 ticks).
    walk_added: bool,
    /// An advancement was announced in chat.
    announced: bool,
    /// While set, it sends this position every tick like a real client: the server pairs
    /// entities in chunks sent since with it only at a move it takes (`ChunkMap.move`).
    idle: Option<(f64, f64, f64)>,
    last_idle: Instant,
}

impl Player {
    fn recv(&mut self) -> ClientboundPlay {
        let raw = self.client.recv_raw().expect("receive");
        ClientboundPlay::decode(&raw).expect("decode")
    }

    /// Receives until `f` matches, answering what a client must answer on the way.
    fn until<T>(&mut self, what: &str, mut f: impl FnMut(&ClientboundPlay) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let packet = self.recv();
            if let Some((x, y, z)) = self.idle
                && self.last_idle.elapsed() >= Duration::from_millis(50)
            {
                self.last_idle = Instant::now();
                let flags = MoveFlags {
                    on_ground: true,
                    horizontal_collision: false,
                };
                self.send(&MovePlayerPos { x, y, z, flags });
                self.send(&ClientTickEnd);
            }
            match &packet {
                ClientboundPlay::ChunkBatchFinished(_) => self
                    .client
                    .send(&ChunkBatchReceived {
                        desired_chunks_per_tick: 64.0,
                    })
                    .unwrap(),
                ClientboundPlay::KeepAlive(k) => self
                    .client
                    .send(&vanillite_protocol::packets::play::serverbound::KeepAlive { id: k.id })
                    .unwrap(),
                ClientboundPlay::Disconnect(d) => panic!("kicked: {}", d.reason.plain()),
                ClientboundPlay::UpdateAdvancements(u) => {
                    self.walk_added |= u.added.iter().any(|e| e.id.as_str() == "test:walk");
                }
                _ => {}
            }
            self.announced |= system_key(&packet, "chat.type.advancement.task").is_some();
            if let Some(v) = f(&packet) {
                return v;
            }
        }
    }

    fn send<P: vanillite_protocol::Packet>(&mut self, packet: &P) {
        self.client.send(packet).expect("send");
    }
}

fn connect(opener: &VirtualOpener, name: &str) -> Player {
    let uuid = Uuid::offline(name);
    let conn = opener.open(
        GameProfile {
            id: uuid,
            name: name.to_owned(),
            properties: Vec::new(),
        },
        ([127, 0, 0, 1], 0).into(),
    );
    let mut client = TestClient::new(Mem {
        conn,
        input: Vec::new(),
        pos: 0,
        output: Vec::new(),
    });
    client.handshake("localhost", 25565, Intent::Login).unwrap();
    client.login(name, None).unwrap();
    Player {
        client,
        uuid: uuid.0,
        commands: Vec::new(),
        walk_added: false,
        announced: false,
        idle: None,
        last_idle: Instant::now(),
    }
}

/// Configuration and the join sequence; returns the teleport position.
fn join(player: &mut Player) -> (f64, f64, f64) {
    player.client.configure().unwrap();
    // Like Vanilla, no keepalive right after a protocol switch: the client answers it before
    // its outbound protocol is switched and disconnects (#135).
    assert!(
        !player.client.received.contains(&"keep_alive"),
        "keepalive during configuration"
    );
    // `login` comes first: the client has no level before it and fails on other play packets,
    // also on broadcasts that reach a player already listed (#199).
    let first = player.recv();
    assert!(
        matches!(first, ClientboundPlay::Login(_)),
        "first play packet is not login: {first:?}"
    );
    let mut roots = Vec::new();
    let teleport = player.until("player_position", |p| match p {
        ClientboundPlay::Commands(tree) => {
            let nodes = &tree.0.nodes;
            let root = usize::try_from(tree.0.root).ok().and_then(|i| nodes.get(i));
            roots = root
                .map(|r| {
                    r.children
                        .iter()
                        .filter_map(|&c| match nodes.get(usize::try_from(c).ok()?)?.kind {
                            NodeKind::Literal(ref name) => Some(name.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            None
        }
        ClientboundPlay::PlayerPosition(t) => Some(t.clone()),
        _ => None,
    });
    player.commands = roots;
    let pos = teleport.change.position;
    player.send(&AcceptTeleportation {
        id: teleport.teleport_id,
        x: pos.x,
        y: pos.y,
        z: pos.z,
        y_rot: teleport.change.y_rot,
        x_rot: teleport.change.x_rot,
    });
    player.until("chunks", |p| {
        matches!(p, ClientboundPlay::LevelChunkWithLight(_)).then_some(())
    });
    player.send(&PlayerLoaded);
    (pos.x, pos.y, pos.z)
}

/// `Some(())` for a system message with the translation key `key`.
fn system_key(packet: &ClientboundPlay, key: &str) -> Option<()> {
    let ClientboundPlay::SystemChat(chat) = packet else {
        return None;
    };
    let mut stack = vec![&chat.content];
    while let Some(c) = stack.pop() {
        if let vanillite_protocol::text::Content::Translatable { key: k, .. } = &c.content
            && k == key
        {
            return Some(());
        }
        stack.extend(c.extra.iter());
    }
    None
}

/// Waits until `viewer` sees an entity with `uuid` spawn and returns its network id.
fn sees_spawn(viewer: &mut Player, uuid: u128) -> i32 {
    viewer.until("add_entity", |p| match p {
        ClientboundPlay::AddEntity(a) if a.uuid.0 == uuid => Some(a.entity_id),
        _ => None,
    })
}

#[test]
fn two_players_join_see_each_other_and_rejoin() {
    let dir = std::env::temp_dir().join(format!("vanillite-join-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("server.properties"), "view-distance=2\n").unwrap();
    // Alice is an operator.
    std::fs::write(
        dir.join("ops.json"),
        format!(
            r#"[{{"uuid":"{}","name":"Alice","level":4,"bypassesPlayerLimit":false}}]"#,
            Uuid::offline("Alice")
        ),
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("vanillite.toml"), "profile = \"vanilla\"\n").unwrap();
    // A data pack with two advancement tabs: one only commands grant, one for being in the
    // overworld (the `location` trigger).
    let pack = dir.join("world/datapacks/test");
    let advancements = pack.join("data/test/advancement");
    std::fs::create_dir_all(&advancements).unwrap();
    std::fs::write(
        pack.join("pack.mcmeta"),
        r#"{"pack": {"description": "Test", "min_format": 121, "max_format": 121}}"#,
    )
    .unwrap();
    let display = r#""display":{"icon":{"id":"minecraft:stone"},"title":"T","description":"D","background":"minecraft:b"}"#;
    std::fs::write(
        advancements.join("a.json"),
        format!(r#"{{"criteria":{{"x":{{"trigger":"minecraft:impossible"}}}},{display}}}"#),
    )
    .unwrap();
    std::fs::write(
        advancements.join("walk.json"),
        format!(
            r#"{{"criteria":{{"here":{{"trigger":"minecraft:location","conditions":{{"player":
            {{"type":"minecraft:entity_properties","entity":"this","predicate":
            {{"minecraft:location":{{"dimension":"minecraft:overworld"}}}}}}}}}}}},{display}}}"#
        ),
    )
    .unwrap();
    let start = |dir: &std::path::Path| {
        let mut builder = ServerBuilder::new(dir);
        builder.vanilla_pack(VanillaPack::None).network(false);
        builder.start().expect("server starts")
    };

    let server = start(&dir);
    let opener = server.virtual_opener().expect("network");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    let mut a = connect(&opener, "Alice");
    let (x, y, z) = join(&mut a);
    // The F3 debug ping is echoed with the same payload.
    let time = 0x0123_4567_89ab_cdef;
    a.send(&vanillite_protocol::packets::play::serverbound::PingRequest { time });
    a.until("pong_response", |p| match p {
        ClientboundPlay::PongResponse(r) => {
            assert_eq!(r.time, time);
            Some(())
        }
        _ => None,
    });
    let mut b = connect(&opener, "Bob");
    b.idle = Some(join(&mut b));

    // Both see each other with a skin entry in the tab list.
    let alice_id = sees_spawn(&mut b, a.uuid);
    b.idle = None;
    a.until("Bob in the tab list", |p| match p {
        ClientboundPlay::PlayerInfoUpdate(u) => u
            .entries
            .iter()
            .any(|e| e.id.0 == b.uuid && e.profile.is_some())
            .then_some(()),
        _ => None,
    });
    sees_spawn(&mut a, b.uuid);

    // Alice walks; Bob sees the movement.
    let target = (x + 1.5, y, z - 0.5);
    a.send(&MovePlayerPos {
        x: target.0,
        y: target.1,
        z: target.2,
        flags: MoveFlags {
            on_ground: true,
            horizontal_collision: false,
        },
    });
    a.send(&ClientTickEnd);
    b.until("Alice moving", |p| match p {
        ClientboundPlay::MoveEntityPos(m) if m.entity_id == alice_id => Some(()),
        ClientboundPlay::MoveEntityPosRot(m) if m.entity_id == alice_id => Some(()),
        ClientboundPlay::EntityPositionSync(m) if m.entity_id == alice_id => Some(()),
        _ => None,
    });

    // Alice sneaks; Bob gets her entity data (crouching flag 0x02, pose 5).
    a.send(&PlayerInput(Input {
        shift: true,
        ..Input::default()
    }));
    b.until("Alice sneaking", |p| match p {
        ClientboundPlay::SetEntityData(d) if d.entity_id == alice_id => {
            (d.items.0.windows(3).any(|w| w == [0, 0, 0x02])).then_some(())
        }
        _ => None,
    });

    // Alice chats; both get her (unsigned) message as player chat.
    a.send(&Chat {
        message: "hello world".to_owned(),
        timestamp: 0,
        salt: 0,
        signature: None,
        last_seen: LastSeenUpdate::default(),
    });
    let alice = a.uuid;
    for viewer in [&mut b, &mut a] {
        viewer.until("Alice's chat message", |p| match p {
            ClientboundPlay::PlayerChat(c) if c.sender.0 == alice => {
                (c.content == "hello world" && c.signature.is_none()).then_some(())
            }
            _ => None,
        });
    }

    // The command tree follows the permission level.
    assert!(a.commands.iter().any(|c| c == "gamemode"));
    assert!(a.commands.iter().any(|c| c == "list"));
    assert!(!b.commands.iter().any(|c| c == "gamemode"));
    assert!(b.commands.iter().any(|c| c == "msg"));

    // Alice (operator) changes her game mode: the game event to her, the tab list to Bob.
    a.send(&ChatCommand {
        command: "gamemode creative".to_owned(),
    });
    a.until("creative game event", |p| match p {
        ClientboundPlay::GameEvent(e) if e.event == 3 && e.param == 1.0 => Some(()),
        _ => None,
    });
    a.until("game mode feedback", |p| {
        system_key(p, "commands.gamemode.success.self")
    });
    b.until("Alice creative in the tab list", |p| match p {
        ClientboundPlay::PlayerInfoUpdate(u) => u
            .entries
            .iter()
            .any(|e| e.id.0 == alice && e.game_mode == GameMode::Creative)
            .then_some(()),
        _ => None,
    });

    // Alice takes stone from the creative inventory and places it; Bob sees the block.
    a.send(&SetCreativeModeSlot {
        slot: 36,
        item: ItemStack::new(1, 64),
    });
    let below = WirePos {
        x: (target.0.floor() as i32) + 2,
        y: (target.1.floor() as i32) - 1,
        z: target.2.floor() as i32,
    };
    a.send(&UseItemOn {
        hand: Hand::MainHand,
        pos: below,
        face: 1,
        cursor_x: 0.5,
        cursor_y: 1.0,
        cursor_z: 0.5,
        inside: false,
        world_border_hit: false,
        sequence: 1,
    });
    a.until("place acknowledged", |p| match p {
        ClientboundPlay::BlockChangedAck(ack) if ack.sequence == 1 => Some(()),
        _ => None,
    });
    let placed = b.until("Alice's stone", |p| match p {
        ClientboundPlay::BlockUpdate(u) if u.state == 1 => Some(u.pos),
        _ => None,
    });
    assert!(
        placed == below
            || placed
                == WirePos {
                    y: below.y + 1,
                    ..below
                }
    );
    // She breaks it again (instantly in creative); Bob sees the particles and the air.
    a.send(&PlayerActionPacket {
        action: DigAction::StartDestroyBlock,
        pos: placed,
        direction: 1,
        sequence: 2,
    });
    a.until("break acknowledged", |p| match p {
        ClientboundPlay::BlockChangedAck(ack) if ack.sequence == 2 => Some(()),
        _ => None,
    });
    let local = i64::from(((placed.x & 15) << 8) | ((placed.z & 15) << 4) | (placed.y & 15));
    let (mut particles, mut air) = (false, false);
    b.until("break particles and the broken stone", |p| {
        match p {
            ClientboundPlay::LevelEvent(e) if e.event == 2001 && e.pos == placed => {
                particles = e.data == 1;
            }
            ClientboundPlay::BlockUpdate(u) if u.pos == placed => air = u.state == 0,
            ClientboundPlay::SectionBlocksUpdate(u) => {
                air |= u.changes.contains(&local);
            }
            _ => {}
        }
        (particles && air).then_some(())
    });
    // She drops one stone (Q); Bob sees the item entity.
    a.send(&PlayerActionPacket {
        action: DigAction::DropItem,
        pos: WirePos { x: 0, y: 0, z: 0 },
        direction: 0,
        sequence: 0,
    });
    b.until("dropped item", |p| match p {
        ClientboundPlay::AddEntity(e) if e.entity_type == i32::from(ids::ITEM.0) => Some(()),
        _ => None,
    });
    // Her statistics count the drop (`minecraft:custom` 8, `minecraft:drop` 24).
    a.send(&ClientCommand {
        action: ClientCommandAction::RequestStats,
    });
    a.until("statistics", |p| match p {
        ClientboundPlay::AwardStats(s) => s
            .stats
            .iter()
            .any(|v| v.stat_type == 8 && v.value_id == 24 && v.count == 1)
            .then_some(()),
        _ => None,
    });
    // Being in the overworld grants `test:walk` to both, announced in chat.
    if !a.walk_added {
        a.until("walk advancement", |p| match p {
            ClientboundPlay::UpdateAdvancements(u) => u
                .added
                .iter()
                .any(|e| e.id.as_str() == "test:walk")
                .then_some(()),
            _ => None,
        });
    }
    if !b.announced {
        b.until("announcement", |p| {
            system_key(p, "chat.type.advancement.task")
        });
    }
    // Alice grants herself `test:a` once.
    a.send(&ChatCommand {
        command: "advancement grant @s only test:a".to_owned(),
    });
    a.until("grant feedback", |p| {
        system_key(p, "commands.advancement.grant.one.to.one.success")
    });
    a.send(&ChatCommand {
        command: "advancement grant Alice only test:a x".to_owned(),
    });
    a.until("grant failure", |p| {
        system_key(p, "commands.advancement.grant.criterion.to.one.failure")
    });

    // Bob may not: the command is unknown to him (permission before parse, SEC-11).
    b.send(&ChatCommand {
        command: "gamemode creative".to_owned(),
    });
    b.until("unknown command error", |p| {
        system_key(p, "command.unknown.command")
    });
    // Bob lists the players.
    b.send(&ChatCommand {
        command: "list".to_owned(),
    });
    b.until("player list", |p| system_key(p, "commands.list.players"));
    // Alice says something; unsigned commands come as system messages with the say type.
    a.send(&ChatCommand {
        command: "say hi there".to_owned(),
    });
    b.until("say message", |p| match p {
        ClientboundPlay::DisguisedChat(c) => (c.message.plain() == "hi there").then_some(()),
        _ => None,
    });
    // Bob completes `/msg `: both names.
    b.send(&CommandSuggestion {
        id: 7,
        command: "/msg ".to_owned(),
    });
    b.until("suggestions", |p| match p {
        ClientboundPlay::CommandSuggestions(s) if s.id == 7 => {
            let names: Vec<&str> = s.suggestions.iter().map(|s| s.text.as_str()).collect();
            assert!(
                names.contains(&"Alice") && names.contains(&"Bob"),
                "{names:?}"
            );
            assert_eq!(s.start, 5);
            Some(())
        }
        _ => None,
    });

    // Alice leaves; Bob sees her go.
    let alice_uuid = a.uuid;
    drop(a);
    b.until("Alice removed", |p| match p {
        ClientboundPlay::RemoveEntities(r) if r.ids.0.contains(&alice_id) => Some(()),
        _ => None,
    });
    b.until("Alice off the tab list", |p| match p {
        ClientboundPlay::PlayerInfoRemove(r) => {
            r.ids.iter().any(|u| u.0 == alice_uuid).then_some(())
        }
        _ => None,
    });

    // She rejoins where she left.
    let mut alice_again = connect(&opener, "Alice");
    let rejoined = join(&mut alice_again);
    assert_eq!(rejoined, target);
    // Her advancements were saved and come back with the first, resetting update.
    alice_again.until("saved advancements", |p| match p {
        ClientboundPlay::UpdateAdvancements(u) if u.reset => {
            assert!(u.added.iter().any(|e| e.id.as_str() == "test:a"));
            Some(())
        }
        _ => None,
    });
    sees_spawn(&mut b, alice_again.uuid);

    handle.stop();
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The first of `keys` a system message has.
fn system_keys(packet: &ClientboundPlay, keys: &[&'static str]) -> Option<&'static str> {
    keys.iter()
        .copied()
        .find(|k| system_key(packet, k).is_some())
}

#[test]
fn data_packs_are_listed_enabled_disabled_and_created() {
    let dir = std::env::temp_dir().join(format!("vanillite-packs-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let pack = dir.join("world/datapacks/test");
    std::fs::create_dir_all(pack.join("data/t/function")).unwrap();
    std::fs::write(
        pack.join("pack.mcmeta"),
        r#"{"pack": {"description": "Test", "min_format": 121, "max_format": 121}}"#,
    )
    .unwrap();
    std::fs::write(pack.join("data/t/function/hello.mcfunction"), "say hi\n").unwrap();
    std::fs::write(dir.join("server.properties"), "view-distance=2\n").unwrap();
    std::fs::write(
        dir.join("ops.json"),
        format!(
            r#"[{{"uuid":"{}","name":"Alice","level":4,"bypassesPlayerLimit":false}}]"#,
            Uuid::offline("Alice")
        ),
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("vanillite.toml"), "profile = \"vanilla\"\n").unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let opener = server.virtual_opener().expect("network");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());
    let mut a = connect(&opener, "Alice");
    join(&mut a);
    let command = |a: &mut Player, line: &str, keys: &[&'static str]| {
        a.send(&ChatCommand {
            command: line.to_owned(),
        });
        a.until(line, |p| system_keys(p, keys))
    };

    // The new world pack was enabled at start.
    let enabled = ["commands.datapack.list.enabled.success"];
    assert_eq!(
        command(&mut a, "datapack list enabled", &enabled),
        enabled[0]
    );
    let unknown = ["commands.datapack.unknown"];
    assert_eq!(
        command(&mut a, "datapack disable nope", &unknown),
        unknown[0]
    );
    let disable = ["commands.datapack.modify.disable"];
    assert_eq!(
        command(&mut a, "datapack disable \"file/test\"", &disable),
        disable[0]
    );
    // The reload loads on a worker, but the selection changes at once.
    let available = [
        "commands.datapack.list.available.success",
        "commands.datapack.list.available.none",
    ];
    assert_eq!(
        command(&mut a, "datapack list available", &available),
        available[0]
    );
    // The functions follow once the reload is applied.
    let function = [
        "commands.function.scheduled.single",
        "arguments.function.unknown",
    ];
    let until_function = |a: &mut Player, expected: &str| {
        for _ in 0..100 {
            if command(a, "function t:hello", &function) == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("function t:hello never gave {expected}");
    };
    until_function(&mut a, function[1]);
    let enable = ["commands.datapack.modify.enable"];
    assert_eq!(
        command(&mut a, "datapack enable \"file/test\" first", &enable),
        enable[0]
    );
    let reload = ["commands.reload.success"];
    assert_eq!(command(&mut a, "reload", &reload), reload[0]);
    until_function(&mut a, function[0]);

    let created = ["commands.datapack.create.success"];
    assert_eq!(
        command(&mut a, "datapack create made \"Made\"", &created),
        created[0]
    );
    let meta = std::fs::read_to_string(dir.join("world/datapacks/made/pack.mcmeta")).unwrap();
    assert!(meta.contains("\"description\": \"Made\""), "{meta}");
    let exists = ["commands.datapack.create.already_exists"];
    assert_eq!(
        command(&mut a, "datapack create made x", &exists),
        exists[0]
    );
    let invalid = ["commands.datapack.create.invalid_name"];
    assert_eq!(
        command(&mut a, "datapack create Made x", &invalid),
        invalid[0]
    );

    handle.stop();
    thread.join().unwrap();
    // `level.dat` keeps the selection.
    let level = vanillite_storage::level_dat::LevelData::read(&dir.join("world"))
        .unwrap()
        .unwrap();
    let (enabled, _) = level.data_packs().unwrap();
    assert!(enabled.iter().any(|p| p == "file/test"), "{enabled:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Waits for the respawn packet into `dimension` and returns how much data the client keeps.
fn respawns_in(player: &mut Player, dimension: &str) -> u8 {
    player.until("respawn", |p| match p {
        ClientboundPlay::Respawn(r) if r.spawn.dimension.to_string() == dimension => {
            Some(r.data_to_keep)
        }
        _ => None,
    })
}

/// Waits for the next teleport, accepts it and returns its position.
fn teleported(player: &mut Player) -> (f64, f64, f64) {
    let t = player.until("player_position", |p| match p {
        ClientboundPlay::PlayerPosition(t) => Some(t.clone()),
        _ => None,
    });
    let pos = t.change.position;
    player.send(&AcceptTeleportation {
        id: t.teleport_id,
        x: pos.x,
        y: pos.y,
        z: pos.z,
        y_rot: t.change.y_rot,
        x_rot: t.change.x_rot,
    });
    (pos.x, pos.y, pos.z)
}

/// Puts an End portal block at `pos` of `world`.
fn end_portal_at(handle: &vanillite_server::ServerHandle, world: u16, pos: (f64, f64, f64)) {
    let pos = vanillite_base::BlockPos::containing(pos.0, pos.1, pos.2);
    handle.run_in_world(vanillite_server::WorldId(world), move |w| {
        let portal = vanillite_data::block::ids::END_PORTAL.default_state();
        w.level_mut()
            .set_block(pos, portal, vanillite_game::level::update::ALL);
    });
}

#[test]
fn players_travel_to_the_end_and_back_through_the_credits() {
    let dir = std::env::temp_dir().join(format!("vanillite-end-trip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("server.properties"), "view-distance=2\n").unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("vanillite.toml"), "profile = \"vanilla\"\n").unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let opener = server.virtual_opener().expect("network");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    let mut p = connect(&opener, "Carol");
    let start = join(&mut p);

    // An End portal at the player's feet takes it onto the End's obsidian platform.
    end_portal_at(&handle, 0, start);
    assert_eq!(respawns_in(&mut p, "minecraft:the_end"), 3);
    assert_eq!(teleported(&mut p), (100.5, 49.0, 0.5));

    // The exit portal rolls the credits the first time; afterwards the player respawns in
    // the overworld with everything it had.
    end_portal_at(&handle, 2, (100.5, 49.0, 0.5));
    p.until("credits", |packet| match packet {
        ClientboundPlay::GameEvent(e) if e.event == 4 => Some(()),
        _ => None,
    });
    p.send(
        &vanillite_protocol::packets::play::serverbound::ClientCommand {
            action:
                vanillite_protocol::packets::play::serverbound::ClientCommandAction::PerformRespawn,
        },
    );
    assert_eq!(respawns_in(&mut p, "minecraft:overworld"), 1);
    teleported(&mut p);

    handle.stop();
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A player data file Vanilla 26.3 wrote (`bot00000` riding a pig, with tags, an earlier game
/// mode, ender items, equipment, attributes, effects, a respawn point and a waypoint colour)
/// is loaded and saved again without losing or adding a key; what the player did not change
/// keeps its value, and the player rides the pig again.
#[test]
fn vanilla_player_data_round_trips() {
    use vanillite_nbt::Tag;
    let dir = std::env::temp_dir().join(format!("vanillite-player-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("server.properties"), "view-distance=2\n").unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("vanillite.toml"), "profile = \"vanilla\"\n").unwrap();
    let original = dir.join("original.dat");
    std::fs::write(&original, include_bytes!("data/vanilla_player.dat")).unwrap();
    let uuid = Uuid::offline("bot00000");
    let players = dir.join("world/players/data");
    std::fs::create_dir_all(&players).unwrap();
    let file = players.join(format!("{uuid}.dat"));
    std::fs::copy(&original, &file).unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let opener = server.virtual_opener().expect("network");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    let mut p = connect(&opener, "bot00000");
    // Riding, its moves only turn it, but each is a tracker check that pairs the pig.
    p.idle = Some(join(&mut p));
    p.until("the pig", |packet| match packet {
        ClientboundPlay::AddEntity(a) if a.entity_type == i32::from(ids::PIG.0) => Some(()),
        _ => None,
    });
    drop(p);
    handle.stop();
    thread.join().unwrap();

    let read = |path: &std::path::Path| {
        vanillite_storage::files::read_gzip_nbt(path)
            .unwrap()
            .unwrap()
    };
    let (before, after) = (read(&original), read(&file));
    let keys = |c: &vanillite_nbt::Compound| {
        let mut k: Vec<String> = c.iter().map(|(k, _)| k.to_owned()).collect();
        k.sort();
        k
    };
    assert_eq!(keys(&after), keys(&before));
    for key in [
        "DataVersion",
        "Dimension",
        "EnderItems",
        "Inventory",
        "SelectedItemSlot",
        "Score",
        "Tags",
        "UUID",
        "XpLevel",
        "XpP",
        "XpTotal",
        "abilities",
        "attributes",
        "equipment",
        "locator_bar_icon",
        "playerGameType",
        "post_effects",
        "previousPlayerGameType",
        "respawn",
        "seenCredits",
    ] {
        // Without data packs there is no enchantment registry: item components that refer to
        // one cannot be read, so the stacks are compared without their components.
        let strip = |tag: Option<&Tag>| match tag {
            Some(Tag::List(vanillite_nbt::List(items))) if key == "Inventory" => {
                Some(Tag::List(vanillite_nbt::List(
                    items
                        .iter()
                        .map(|item| match item {
                            Tag::Compound(c) => {
                                let mut c = c.clone();
                                c.remove("components");
                                sorted(&Tag::Compound(c))
                            }
                            other => sorted(other),
                        })
                        .collect(),
                )))
            }
            other => other.map(sorted),
        };
        assert_eq!(strip(after.get(key)), strip(before.get(key)), "{key}");
    }
    let vehicle = |c: &vanillite_nbt::Compound| match c.get("RootVehicle") {
        Some(Tag::Compound(v)) => (
            v.get("Attach").cloned(),
            match v.get("Entity") {
                Some(Tag::Compound(e)) => (
                    e.get("id").cloned(),
                    e.get("UUID").cloned(),
                    e.get("Tags").cloned(),
                ),
                _ => (None, None, None),
            },
        ),
        _ => panic!("no RootVehicle"),
    };
    assert_eq!(vehicle(&after), vehicle(&before));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tag with the keys of its compounds sorted: the order of compound keys carries no meaning.
fn sorted(tag: &vanillite_nbt::Tag) -> vanillite_nbt::Tag {
    use vanillite_nbt::{Compound, List, Tag};
    match tag {
        Tag::Compound(c) => {
            let mut entries: Vec<(&str, &Tag)> = c.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            let mut out = Compound::new();
            for (k, v) in entries {
                out.insert(k, sorted(v));
            }
            Tag::Compound(out)
        }
        Tag::List(List(items)) => Tag::List(List(items.iter().map(sorted).collect())),
        other => other.clone(),
    }
}

/// Arrows, spectral arrows and tridents resting in a block send no synched data and no
/// movement but Vanilla's forced position every 60 ticks: the client restarts the shake on
/// every `IN_GROUND` it receives, so data sent each tick made resting arrows jitter (#198).
#[test]
fn resting_arrows_send_no_updates() {
    let dir = std::env::temp_dir().join(format!("vanillite-arrows-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("server.properties"), "view-distance=2\n").unwrap();
    std::fs::write(
        dir.join("ops.json"),
        format!(
            r#"[{{"uuid":"{}","name":"Erin","level":4,"bypassesPlayerLimit":false}}]"#,
            Uuid::offline("Erin")
        ),
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("vanillite.toml"), "profile = \"vanilla\"\n").unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let opener = server.virtual_opener().expect("network");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    let mut p = connect(&opener, "Erin");
    join(&mut p);
    let kinds = [ids::ARROW, ids::SPECTRAL_ARROW, ids::TRIDENT];
    for (i, kind) in ["arrow", "spectral_arrow", "trident"].iter().enumerate() {
        let z = 2 * i32::try_from(i).unwrap() - 2;
        for command in [
            format!("setblock ~3 ~ ~{z} minecraft:stone"),
            format!("summon minecraft:{kind} ~3 ~3 ~{z} {{Motion:[0.0,-0.5,0.0]}}"),
        ] {
            p.send(&ChatCommand { command });
        }
    }
    let mut network_ids = [None; 3];
    p.until("arrows", |packet| {
        if let ClientboundPlay::AddEntity(a) = packet
            && let Some(i) = kinds.iter().position(|k| a.entity_type == i32::from(k.0))
        {
            network_ids[i] = Some(a.entity_id);
        }
        network_ids.iter().all(Option::is_some).then_some(())
    });
    let network_ids = network_ids.map(Option::unwrap);
    // The time sync every 20 ticks is the clock: two for landing, six to watch them rest.
    let mut syncs = 0;
    let mut data = [0; 3];
    let mut moves = [0; 3];
    p.until("120 resting ticks", |packet| {
        let (counter, id) = match packet {
            ClientboundPlay::SetTime(_) => {
                syncs += 1;
                return (syncs == 8).then_some(());
            }
            ClientboundPlay::SetEntityData(d) => (&mut data, d.entity_id),
            ClientboundPlay::MoveEntityPos(m) => (&mut moves, m.entity_id),
            ClientboundPlay::MoveEntityPosRot(m) => (&mut moves, m.entity_id),
            ClientboundPlay::MoveEntityRot(m) => (&mut moves, m.entity_id),
            ClientboundPlay::EntityPositionSync(m) => (&mut moves, m.entity_id),
            ClientboundPlay::SetEntityMotion(m) => (&mut moves, m.entity_id),
            _ => return None,
        };
        if syncs >= 2
            && let Some(i) = network_ids.iter().position(|&n| n == id)
        {
            counter[i] += 1;
        }
        None
    });
    assert_eq!(data, [0; 3], "synched data of resting arrows");
    assert!(
        moves.iter().all(|&m| m <= 2),
        "movement of resting arrows: {moves:?}"
    );

    handle.stop();
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
