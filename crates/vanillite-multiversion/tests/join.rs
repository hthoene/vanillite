//! Clients of older versions join a server of the current version through the multi-version
//! layer: configuration, chunks, walking, and seeing a current-version player (and being seen
//! by it). One test per oldest release of each larger protocol change: 1.20.5, 1.21, 1.21.4,
//! 26.1 and the previous release 26.2.
//!
//! The old clients are written against the older wire formats by hand (packet ids and facts
//! from `data/multiversion/<version>/`), not with the translation code they test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use serde_json::Value;
use vanillite_protocol::client::TestClient;
use vanillite_protocol::codec::{Reader, write_str, write_varint};
use vanillite_protocol::packets::handshake::Intent;
use vanillite_protocol::packets::play::serverbound::{
    AcceptTeleportation, ChunkBatchReceived, ClientTickEnd, MoveFlags, MovePlayerPos, PlayerLoaded,
};
use vanillite_protocol::sets::ClientboundPlay;
use vanillite_protocol::types::{GameProfile, Uuid};
use vanillite_server::{ServerBuilder, VanillaPack, VirtualConnection, VirtualOpener};

const TIMEOUT: Duration = Duration::from_secs(30);

/// A virtual connection as a byte stream with uncompressed length-prefixed frames.
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
        loop {
            let mut r = Reader::new(&self.output);
            let Ok(len) = r.varint() else {
                return Ok(buf.len());
            };
            let header = self.output.len() - r.remaining();
            let len = usize::try_from(len).unwrap();
            if r.remaining() < len {
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

fn mem(opener: &VirtualOpener, name: &str) -> TestClient<Mem> {
    let conn = opener.open_remote(
        GameProfile {
            id: Uuid::offline(name),
            name: name.to_owned(),
            properties: Vec::new(),
        },
        ([127, 0, 0, 1], 0).into(),
    );
    TestClient::new(Mem {
        conn,
        input: Vec::new(),
        pos: 0,
        output: Vec::new(),
    })
}

/// The facts of an older version the old client needs, from the kept data.
struct OldVersion {
    /// The release the client claims to be (its core pack version).
    name: &'static str,
    protocol: i32,
    packets: Value,
    registries: Value,
    synchronized: Value,
    entity_data: Value,
    block_states: usize,
}

impl OldVersion {
    fn load(dir: &str, name: &'static str) -> Self {
        let dir = format!(
            "{}/",
            vanillite_multiversion::data_paths::file(&format!("data/multiversion/{dir}"))
        );
        let json = |f: &str| -> Value {
            serde_json::from_str(&std::fs::read_to_string(format!("{dir}{f}")).unwrap()).unwrap()
        };
        let version: String = std::fs::read_to_string(format!("{dir}version.toml")).unwrap();
        let protocol = version
            .lines()
            .find_map(|l| l.strip_prefix("protocol = "))
            .unwrap()
            .parse()
            .unwrap();
        let blocks = json("blocks.json");
        let block_states = blocks
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                let n: u64 = b["properties"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["values"].as_array().unwrap().len() as u64)
                    .product();
                b["first_state"].as_u64().unwrap() + n
            })
            .max()
            .unwrap();
        Self {
            name,
            protocol,
            packets: json("packets.json"),
            registries: json("registries.json"),
            synchronized: json("synchronized.json"),
            entity_data: json("entity_data.json"),
            block_states: usize::try_from(block_states).unwrap(),
        }
    }

    /// Checks `set_entity_data` of an entity of type `kind` (old numbering) against the old
    /// layout: every item has the index and serializer of the old type.
    fn check_entity_data(&self, kind: i32, data: &[u8]) {
        let names = self.registries["minecraft:entity_type"]["entries"]
            .as_array()
            .unwrap();
        let name = names[usize::try_from(kind).unwrap()].as_str().unwrap();
        let layout = self.entity_data["types"][name].as_array().unwrap();
        let serializers = self.entity_data["serializers"].as_array().unwrap();
        let mut r = Reader::new(data);
        r.varint().unwrap();
        loop {
            let index = r.u8().unwrap();
            if index == 0xff {
                break;
            }
            let serializer = usize::try_from(r.varint().unwrap()).unwrap();
            let item = layout
                .iter()
                .find(|i| i[0] == u64::from(index))
                .unwrap_or_else(|| panic!("{}: {name} has no data item {index}", self.name));
            assert_eq!(
                item[1], serializers[serializer],
                "{}: {name} data item {index}",
                self.name
            );
            // Only the values of the serializers players use are skipped here.
            match serializers[serializer].as_str().unwrap() {
                "byte" | "boolean" => drop(r.take(1).unwrap()),
                "int" | "pose" | "humanoid_arm" | "optional_unsigned_int" => {
                    r.varint().unwrap();
                }
                "float" => drop(r.take(4).unwrap()),
                "particles" => assert_eq!(r.varint().unwrap(), 0, "no particles expected"),
                "optional_component" | "optional_block_pos" => {
                    assert_eq!(r.u8().unwrap(), 0, "absent value expected");
                }
                other => panic!("{}: unexpected serializer {other} for {name}", self.name),
            }
        }
        assert_eq!(r.remaining(), 0, "{}: set_entity_data layout", self.name);
    }

    fn id(&self, state: &str, dir: &str, name: &str) -> i32 {
        let list = self.packets[state][dir].as_array().unwrap();
        let name = format!("minecraft:{name}");
        i32::try_from(
            list.iter()
                .position(|p| p == &Value::from(name.as_str()))
                .unwrap_or_else(|| panic!("{} has no {state} {dir} {name}", self.name)),
        )
        .unwrap()
    }

    fn registry_id(&self, registry: &str, name: &str) -> i32 {
        let list = self.registries[registry]["entries"].as_array().unwrap();
        i32::try_from(list.iter().position(|p| p == name).unwrap()).unwrap()
    }
}

/// A client of an older protocol driven packet by packet.
struct OldClient {
    client: TestClient<Mem>,
    v: OldVersion,
    /// Entity id → type (old numbering) of the added entities.
    entities: std::collections::HashMap<i32, i32>,
    /// Data packets checked against the old layouts.
    checked_data: usize,
}

impl OldClient {
    fn send(&mut self, state: &str, name: &str, fields: &[u8]) {
        let mut body = Vec::new();
        write_varint(&mut body, self.v.id(state, "serverbound", name));
        body.extend_from_slice(fields);
        let mut frame = Vec::new();
        write_varint(&mut frame, i32::try_from(body.len()).unwrap());
        frame.extend_from_slice(&body);
        self.client.send_raw(&frame).unwrap();
    }

    /// The next packet: its name in the old version and fields.
    fn recv(&mut self, state: &str) -> (String, Vec<u8>) {
        let raw = self.client.recv_raw().expect("receive");
        let mut r = Reader::new(&raw);
        let id = usize::try_from(r.varint().unwrap()).unwrap();
        let name = self.v.packets[state]["clientbound"][id]
            .as_str()
            .unwrap_or_else(|| panic!("{state} packet id {id} unknown to {}", self.v.name))
            .trim_start_matches("minecraft:")
            .to_owned();
        (name, r.rest().to_vec())
    }

    /// Play packets until `f` matches, answering keepalives and chunk batches like a client.
    fn until<T>(&mut self, what: &str, mut f: impl FnMut(&str, &[u8]) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "{} client timed out waiting for {what}",
                self.v.name
            );
            let (name, data) = self.recv("play");
            match name.as_str() {
                "keep_alive" => self.send("play", "keep_alive", &data),
                "chunk_batch_finished" => {
                    self.send("play", "chunk_batch_received", &64.0f32.to_be_bytes());
                }
                "disconnect" => panic!("{} client kicked", self.v.name),
                "level_chunk_with_light" => check_chunk(&data, &self.v),
                "login" => {
                    let id = Reader::new(&data).i32().unwrap();
                    let player = self
                        .v
                        .registry_id("minecraft:entity_type", "minecraft:player");
                    self.entities.insert(id, player);
                }
                "add_entity" => {
                    let mut r = Reader::new(&data);
                    let id = r.varint().unwrap();
                    r.take(16).unwrap();
                    self.entities.insert(id, r.varint().unwrap());
                }
                "set_entity_data" => {
                    let id = Reader::new(&data).varint().unwrap();
                    if let Some(&kind) = self.entities.get(&id) {
                        self.v.check_entity_data(kind, &data);
                        self.checked_data += 1;
                    }
                }
                _ => {}
            }
            if let Some(v) = f(&name, &data) {
                return v;
            }
        }
    }
}

/// Skips one network NBT tag (heightmaps before 1.21.5).
fn skip_nbt(r: &mut Reader<'_>) {
    let mut budget = vanillite_nbt::Budget::new(
        vanillite_nbt::read::NETWORK_QUOTA,
        vanillite_nbt::read::MAX_DEPTH,
    );
    let n = vanillite_nbt::read::skip_network(r.rest(), &mut budget).unwrap();
    r.take(n).unwrap();
}

/// Every block state of every section is one the old client knows; direct palettes use its
/// global bit count; the layout is the old version's (heightmaps, section counts, data
/// lengths).
fn check_chunk(data: &[u8], v: &OldVersion) {
    let old_states = v.block_states;
    let mut r = Reader::new(data);
    r.take(8).unwrap();
    if v.protocol >= 770 {
        let heightmaps = r.varint().unwrap();
        for _ in 0..heightmaps {
            r.varint().unwrap();
            let n = usize::try_from(r.varint().unwrap()).unwrap();
            r.take(n * 8).unwrap();
        }
    } else {
        skip_nbt(&mut r);
    }
    let len = usize::try_from(r.varint().unwrap()).unwrap();
    let mut s = Reader::new(r.take(len).unwrap());
    let global = usize::BITS - (old_states - 1).leading_zeros();
    while s.remaining() > 0 {
        s.take(if v.protocol >= 775 { 4 } else { 2 }).unwrap();
        for (entries, max_indirect) in [(4096usize, 8u8), (64, 3)] {
            let bits = s.u8().unwrap();
            let longs = if bits == 0 {
                let state = usize::try_from(s.varint().unwrap()).unwrap();
                if entries == 4096 {
                    assert!(state < old_states, "single block state {state} unknown");
                }
                0
            } else {
                if bits <= max_indirect {
                    let n = s.varint().unwrap();
                    for _ in 0..n {
                        let state = usize::try_from(s.varint().unwrap()).unwrap();
                        if entries == 4096 {
                            assert!(state < old_states, "palette block state {state} unknown");
                        }
                    }
                } else if entries == 4096 {
                    assert_eq!(u32::from(bits), global, "direct palette bits");
                }
                entries.div_ceil(64 / usize::from(bits))
            };
            if v.protocol < 770 {
                assert_eq!(
                    usize::try_from(s.varint().unwrap()).unwrap(),
                    longs,
                    "data length before 1.21.5"
                );
            }
            s.take(longs * 8).unwrap();
        }
    }
}
/// A current-version player.
struct Player {
    client: TestClient<Mem>,
    /// While set, it sends this position every tick like a real client: the server pairs
    /// entities in chunks sent since with it only at a move it takes (`ChunkMap.move`).
    idle: Option<[f64; 3]>,
    last_idle: Instant,
}

impl Player {
    fn until<T>(&mut self, what: &str, mut f: impl FnMut(&ClientboundPlay) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let raw = self.client.recv_raw().expect("receive");
            let packet = ClientboundPlay::decode(&raw).expect("decode");
            if let Some([x, y, z]) = self.idle
                && self.last_idle.elapsed() >= Duration::from_millis(50)
            {
                self.last_idle = Instant::now();
                let flags = MoveFlags {
                    on_ground: true,
                    horizontal_collision: false,
                };
                self.client.send(&MovePlayerPos { x, y, z, flags }).unwrap();
                self.client.send(&ClientTickEnd).unwrap();
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
                _ => {}
            }
            if let Some(v) = f(&packet) {
                return v;
            }
        }
    }
}

fn f64s(data: &[u8], at: usize) -> [f64; 3] {
    let mut r = Reader::new(&data[at..]);
    [r.f64().unwrap(), r.f64().unwrap(), r.f64().unwrap()]
}

#[test]
fn client_26_2_joins_walks_and_sees_players() {
    join_walk_see("26.2", "26.2");
}

#[test]
fn client_26_1_joins_walks_and_sees_players() {
    join_walk_see("26.1.2", "26.1");
}

#[test]
fn client_1_21_4_joins_walks_and_sees_players() {
    join_walk_see("1.21.4", "1.21.4");
}

#[test]
fn client_1_21_joins_walks_and_sees_players() {
    join_walk_see("1.21.1", "1.21");
}

#[test]
fn client_1_20_5_joins_walks_and_sees_players() {
    join_walk_see("1.20.6", "1.20.5");
}

/// A client of release `name` (data in `data/multiversion/<dir>/`) joins, walks and sees a
/// current-version player.
#[allow(clippy::too_many_lines)]
fn join_walk_see(dir: &str, release: &'static str) {
    let v = OldVersion::load(dir, release);
    let proto = v.protocol;
    let dir = std::env::temp_dir().join(format!(
        "vanillite-multiversion-{}-{release}",
        std::process::id()
    ));
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

    // --- The old client logs in and configures.
    let mut old = OldClient {
        client: mem(&opener, "Oldie"),
        v,
        entities: std::collections::HashMap::new(),
        checked_data: 0,
    };
    let mut hs = Vec::new();
    write_varint(&mut hs, proto);
    write_str(&mut hs, "localhost");
    hs.extend_from_slice(&25565u16.to_be_bytes());
    write_varint(&mut hs, 2);
    old.send("handshake", "intention", &hs);
    let mut hello = Vec::new();
    write_str(&mut hello, "Oldie");
    hello.extend_from_slice(&Uuid::offline("Oldie").0.to_be_bytes());
    old.send("login", "hello", &hello);
    loop {
        let (packet, data) = old.recv("login");
        if packet == "login_finished" || packet == "game_profile" {
            // Profile, then the strict error handling flag (1.20.5–1.21.1) or the session
            // id (26.2), nothing else.
            let mut r = Reader::new(&data);
            r.take(16).unwrap();
            r.string(16).unwrap();
            for _ in 0..r.varint().unwrap() {
                r.string(32767).unwrap();
                r.string(32767).unwrap();
                if r.u8().unwrap() != 0 {
                    r.string(32767).unwrap();
                }
            }
            let tail = match proto {
                ..768 => 1,
                776.. => 16,
                _ => 0,
            };
            assert_eq!(r.remaining(), tail, "{release} login_finished layout");
            break;
        }
        assert_ne!(packet, "login_disconnect", "old client refused");
    }
    old.send("login", "login_acknowledged", &[]);

    let mut registries = Vec::new();
    loop {
        let (name, data) = old.recv("configuration");
        let mut r = Reader::new(&data);
        match name.as_str() {
            "select_known_packs" => {
                let n = r.varint().unwrap();
                let mut core = None;
                for _ in 0..n {
                    let (ns, id, version) = (
                        r.string(256).unwrap(),
                        r.string(256).unwrap(),
                        r.string(256).unwrap(),
                    );
                    if ns == "minecraft" && id == "core" {
                        core = Some(version.to_owned());
                    }
                }
                // A test server without data packs offers no pack.
                let _ = core;
                let mut answer = Vec::new();
                write_varint(&mut answer, 1);
                for s in ["minecraft", "core", release] {
                    write_str(&mut answer, s);
                }
                old.send("configuration", "select_known_packs", &answer);
            }
            "registry_data" => registries.push(r.string(256).unwrap().to_owned()),
            "keep_alive" => old.send("configuration", "keep_alive", &data),
            "finish_configuration" => {
                old.send("configuration", "finish_configuration", &[]);
                break;
            }
            "disconnect" => panic!("old client kicked in configuration"),
            _ => {}
        }
    }
    // (The test server has no data packs, so this list may be empty; the golden tests cover
    // the registry filter.)
    let synchronized = old.v.synchronized.as_object().unwrap();
    for r in &registries {
        assert!(
            synchronized.contains_key(r),
            "registry {r} is not synchronised by {release}"
        );
    }

    // --- Play: join, confirm the teleport the old way (only the id), receive chunks.
    old.until("login", |n, _| (n == "login").then_some(()));
    let (teleport, pos) = old.until("player_position", |n, d| {
        (n == "player_position").then(|| {
            let mut r = Reader::new(d);
            if proto >= 768 {
                let id = r.varint().unwrap();
                (id, f64s(d, d.len() - r.remaining()))
            } else {
                // Position, rotation, flags, then the teleport id.
                r.take(24 + 8 + 1).unwrap();
                let id = r.varint().unwrap();
                assert_eq!(r.remaining(), 0, "player_position layout before 1.21.2");
                (id, f64s(d, 0))
            }
        })
    });
    let mut confirm = Vec::new();
    write_varint(&mut confirm, teleport);
    old.send("play", "accept_teleportation", &confirm);
    old.until("chunk", |n, _| {
        (n == "level_chunk_with_light").then_some(())
    });
    if proto >= 769 {
        old.send("play", "player_loaded", &[]);
    }

    // --- A current-version player joins; both see each other.
    let mut new = Player {
        client: mem(&opener, "Newbie"),
        idle: None,
        last_idle: Instant::now(),
    };
    new.client
        .handshake("localhost", 25565, Intent::Login)
        .unwrap();
    new.client.login("Newbie", None).unwrap();
    new.client.configure().unwrap();
    let new_pos = new.until("player_position", |p| match p {
        ClientboundPlay::PlayerPosition(t) => Some(t.clone()),
        _ => None,
    });
    new.client
        .send(&AcceptTeleportation {
            id: new_pos.teleport_id,
            x: new_pos.change.position.x,
            y: new_pos.change.position.y,
            z: new_pos.change.position.z,
            y_rot: new_pos.change.y_rot,
            x_rot: new_pos.change.x_rot,
        })
        .unwrap();
    new.client.send(&PlayerLoaded).unwrap();
    let p = new_pos.change.position;
    new.idle = Some([p.x, p.y, p.z]);

    let player_type = old
        .v
        .registry_id("minecraft:entity_type", "minecraft:player");
    let newbie = Uuid::offline("Newbie").0;
    let newbie_id = old.until("add_entity of the new player", |n, d| {
        if n != "add_entity" {
            return None;
        }
        let mut r = Reader::new(d);
        let id = r.varint().unwrap();
        let uuid = r.u128().unwrap();
        let kind = r.varint().unwrap();
        (uuid == newbie).then(|| {
            assert_eq!(kind, player_type, "entity type in {release} numbering");
            id
        })
    });
    let oldie = Uuid::offline("Oldie").0;
    let oldie_id = new.until("add_entity of the old player", |p| match p {
        ClientboundPlay::AddEntity(a) if a.uuid.0 == oldie => Some(a.entity_id),
        _ => None,
    });
    new.idle = None;

    // --- The new player walks; the old client sees it in its movement format.
    let p = new_pos.change.position;
    new.client
        .send(&MovePlayerPos {
            x: p.x + 1.5,
            y: p.y,
            z: p.z - 0.5,
            flags: MoveFlags {
                on_ground: true,
                horizontal_collision: false,
            },
        })
        .unwrap();
    new.client.send(&ClientTickEnd).unwrap();
    old.until("the new player moving", |n, d| {
        let mut r = Reader::new(d);
        match n {
            // Id, three shorts, (yaw, pitch,) on ground.
            "move_entity_pos" | "move_entity_pos_rot" => {
                if r.varint().unwrap() != newbie_id {
                    return None;
                }
                let dx = r.i16().unwrap();
                r.take(4).unwrap();
                if n == "move_entity_pos_rot" {
                    r.take(2).unwrap();
                }
                r.u8().unwrap();
                assert_eq!(r.remaining(), 0, "{release} {n} layout");
                assert_eq!(dx, 6144, "x delta in 1/4096 blocks");
                Some(())
            }
            // Id, position, movement, yaw, pitch, on ground.
            "entity_position_sync" => {
                if r.varint().unwrap() != newbie_id {
                    return None;
                }
                r.take(24 + 24 + 8 + 1).unwrap();
                assert_eq!(r.remaining(), 0, "{release} entity_position_sync layout");
                Some(())
            }
            // Before 1.21.2: id, position, yaw and pitch bytes, on ground.
            "teleport_entity" if proto < 768 => {
                if r.varint().unwrap() != newbie_id {
                    return None;
                }
                r.take(24 + 2 + 1).unwrap();
                assert_eq!(r.remaining(), 0, "{release} teleport_entity layout");
                Some(())
            }
            _ => None,
        }
    });

    // --- The old client walks; the new player sees it.
    let mut walk = Vec::new();
    for v in [pos[0] - 1.0, pos[1], pos[2] + 0.5] {
        walk.extend_from_slice(&v.to_be_bytes());
    }
    walk.push(1);
    old.send("play", "move_player_pos", &walk);
    if proto >= 768 {
        old.send("play", "client_tick_end", &[]);
    }
    new.until("the old player moving", |p| match p {
        ClientboundPlay::MoveEntityPos(m) if m.entity_id == oldie_id => Some(()),
        ClientboundPlay::MoveEntityPosRot(m) if m.entity_id == oldie_id => Some(()),
        ClientboundPlay::EntityPositionSync(m) if m.entity_id == oldie_id => Some(()),
        _ => None,
    });

    eprintln!(
        "{release}: {} entity data packets checked against the old layouts",
        old.checked_data
    );
    handle.stop();
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
