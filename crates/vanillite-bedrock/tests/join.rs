//! A headless Bedrock client joins an in-process server through the Bedrock frontend, gets the
//! world, walks, and sees a Java player who sees it walking; chat goes both ways. Once with the
//! oldest and once with the newest supported Bedrock version.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use vanillite_bedrock::Version;
use vanillite_bedrock::client::{self, AuthInput, Client, input_flag, packet_id as id};
use vanillite_protocol::client::TestClient;
use vanillite_protocol::packets::handshake::Intent;
use vanillite_protocol::packets::play::serverbound::{ChunkBatchReceived, KeepAlive};
use vanillite_protocol::sets::ClientboundPlay;
use vanillite_server::{ServerBuilder, VanillaPack};

const TIMEOUT: Duration = Duration::from_secs(30);

fn java_until(
    client: &mut TestClient<TcpStream>,
    what: &str,
    mut f: impl FnMut(&ClientboundPlay) -> bool,
) -> ClientboundPlay {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let raw = client.recv_raw().expect("receive");
        let Ok(packet) = ClientboundPlay::decode(&raw) else {
            continue;
        };
        match &packet {
            ClientboundPlay::KeepAlive(k) => client.send(&KeepAlive { id: k.id }).unwrap(),
            ClientboundPlay::ChunkBatchFinished(_) => client
                .send(&ChunkBatchReceived {
                    desired_chunks_per_tick: 64.0,
                })
                .unwrap(),
            ClientboundPlay::Disconnect(d) => panic!("kicked: {}", d.reason.plain()),
            _ => {}
        }
        if f(&packet) {
            return packet;
        }
    }
}

#[test]
fn oldest_bedrock_version_joins_walks_and_meets_java_players() {
    joins_walks_and_meets_java_players(Version::oldest());
}

#[test]
fn newest_bedrock_version_joins_walks_and_meets_java_players() {
    joins_walks_and_meets_java_players(Version::latest());
}

fn joins_walks_and_meets_java_players(version: &'static Version) {
    let dir = std::env::temp_dir().join(format!(
        "vanillite-bedrock-join-{}-{}",
        std::process::id(),
        version.protocol
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.properties"),
        "view-distance=2\nonline-mode=false\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("vanillite.toml"),
        "profile = \"vanilla\"\n[bedrock]\nenabled = true\nonline-mode = false\n",
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    let mut builder = ServerBuilder::new(dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let java_addr = server.local_addr().unwrap();
    let bedrock_addr = server.bedrock_addr().expect("Bedrock frontend runs");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    // A Java player is online first.
    let stream = TcpStream::connect(java_addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut java = TestClient::new(stream);
    java.handshake("localhost", java_addr.port(), Intent::Login)
        .unwrap();
    java.login("Alice", None).unwrap();
    java.configure().unwrap();
    java_until(&mut java, "login", |p| {
        matches!(p, ClientboundPlay::Login(_))
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut bedrock =
            Client::connect_as(bedrock_addr, "2535400000000001", "Bedrock Tester", version)
                .await
                .expect("Bedrock login");
        let start = bedrock.recv(id::START_GAME).await.expect("start_game");
        let (own_id, spawn) = client::read_start_game(&start).unwrap();
        assert!(own_id > 0);
        bedrock
            .send(id::REQUEST_CHUNK_RADIUS, &[8, 8])
            .await
            .unwrap();

        // The world arrives as Bedrock chunks, then the client may spawn.
        let mut chunks = 0;
        let mut alice = None;
        loop {
            let (pid, body) = bedrock
                .next(TIMEOUT)
                .await
                .expect("packets after start_game");
            match pid {
                id::LEVEL_CHUNK => chunks += 1,
                id::ADD_PLAYER => alice = client::read_add_player(&body),
                id::PLAY_STATUS => break,
                _ => {}
            }
        }
        assert!(chunks > 0, "chunks before spawning");
        bedrock
            .send(
                id::SET_LOCAL_PLAYER_AS_INITIALIZED,
                &[u8::try_from(own_id).unwrap()],
            )
            .await
            .unwrap();

        // The Java player is shown as a Bedrock player. Like a real client, it sends its input
        // every tick meanwhile: the server pairs entities in chunks sent since with it only at
        // a move it takes (`ChunkMap.move`), not when the chunk is sent.
        let mut tick = 0u64;
        let deadline = Instant::now() + TIMEOUT;
        while alice.is_none() {
            assert!(Instant::now() < deadline, "Alice: timed out");
            let input = AuthInput {
                pitch: 0.0,
                yaw: -90.0,
                position: spawn,
                head_yaw: -90.0,
                move_vector: [0.0; 2],
                flags: 1 << input_flag::VERTICAL_COLLISION,
                tick,
                delta: [0.0, -0.078, 0.0],
            };
            tick += 1;
            bedrock
                .send(id::PLAYER_AUTH_INPUT, &client::auth_input(&input, version))
                .await
                .unwrap();
            while let Ok((pid, body)) = bedrock.next(Duration::from_millis(50)).await {
                if pid == id::ADD_PLAYER {
                    alice = client::read_add_player(&body);
                }
            }
        }
        assert_eq!(alice.unwrap().0, "Alice");

        // Walk 3 blocks east, one block per tick at most.
        let mut pos = spawn;
        for tick in tick..tick + 30 {
            pos[0] += 0.1;
            let input = AuthInput {
                pitch: 0.0,
                yaw: -90.0,
                position: pos,
                head_yaw: -90.0,
                move_vector: [0.0; 2],
                flags: 1 << input_flag::VERTICAL_COLLISION,
                tick,
                delta: [0.1, -0.078, 0.0],
            };
            bedrock
                .send(id::PLAYER_AUTH_INPUT, &client::auth_input(&input, version))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            while let Ok(p) = bedrock.next(Duration::from_millis(1)).await {
                let _ = p;
            }
        }
        bedrock
            .send(id::TEXT, &client::chat("", "hello from bedrock"))
            .await
            .unwrap();
        bedrock
    });

    // The Java player sees the Bedrock player walking east and its chat.
    let mut bedrock_entity = None;
    let mut start_x = None;
    let mut moved = false;
    let mut chatted = false;
    let deadline = Instant::now() + TIMEOUT;
    while !(moved && chatted) {
        assert!(
            Instant::now() < deadline,
            "moved {moved}, chatted {chatted}"
        );
        match java_until(&mut java, "Bedrock player updates", |_| true) {
            ClientboundPlay::PlayerInfoUpdate(u) => {
                for e in &u.entries {
                    if let Some(p) = &e.profile {
                        assert_ne!(p.name, "Bedrock Tester");
                    }
                }
            }
            ClientboundPlay::AddEntity(e)
                if e.uuid.0 == vanillite_protocol::types::Uuid::offline(".Bedrock_Tester").0 =>
            {
                bedrock_entity = Some(e.entity_id);
                start_x = Some(e.position.x);
            }
            ClientboundPlay::EntityPositionSync(s) if Some(s.entity_id) == bedrock_entity => {
                if let vanillite_protocol::packets::play::clientbound::PositionPath::Linear(p) =
                    s.path
                {
                    moved |= p.x > start_x.unwrap_or(f64::MAX) + 0.5;
                }
            }
            ClientboundPlay::MoveEntityPos(m) if Some(m.entity_id) == bedrock_entity => {
                moved = true
            }
            ClientboundPlay::MoveEntityPosRot(m) if Some(m.entity_id) == bedrock_entity => {
                moved = true
            }
            ClientboundPlay::PlayerChat(c) => chatted |= c.content.contains("hello from bedrock"),
            ClientboundPlay::SystemChat(c) => {
                chatted |= c.content.plain().contains("hello from bedrock")
            }
            _ => {}
        }
    }
    assert!(bedrock_entity.is_some());

    handle.stop();
    thread.join().unwrap();
}
