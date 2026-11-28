//! A headless Bedrock client sees what Java shows without a Bedrock counterpart (#266): sounds
//! stopped by `/stopsound`, the world border as a particle wall, and an entity moved by `/tp`
//! (`teleport_entity`). Dialogs need the Vanilla pack; `tools/bedrock-strict` covers them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use std::time::Duration;

use vanillite_bedrock::Version;
use vanillite_bedrock::client::{self, AuthInput, Client, input_flag, packet_id as id};
use vanillite_server::{ServerBuilder, VanillaPack};

const TIMEOUT: Duration = Duration::from_secs(30);

/// Waits for a packet `pid` that `f` accepts, sending movement meanwhile.
async fn until(
    c: &mut Client,
    input: &mut impl FnMut() -> Vec<u8>,
    what: &str,
    pid: u32,
    mut f: impl FnMut(&[u8]) -> bool,
) -> Vec<u8> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut tick = std::time::Instant::now();
    loop {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        let next = c.next(Duration::from_millis(50)).await;
        // Movement at 20 ticks per second, like a client.
        if tick.elapsed() >= Duration::from_millis(50) {
            tick = std::time::Instant::now();
            c.send(id::PLAYER_AUTH_INPUT, &input()).await.unwrap();
        }
        let Ok((p, body)) = next else {
            continue;
        };
        if p == id::DISCONNECT {
            panic!("{what}: disconnected");
        }
        if p == pid && f(&body) {
            return body;
        }
    }
}

/// A zigzag VarInt, as Bedrock writes signed ints.
fn vari32(v: i32) -> Vec<u8> {
    let mut u = ((v << 1) ^ (v >> 31)) as u32;
    let mut out = Vec::new();
    loop {
        let b = (u & 0x7f) as u8;
        u >>= 7;
        if u == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

#[test]
fn java_only_features_reach_bedrock() {
    let version = Version::latest();
    let dir = std::env::temp_dir().join(format!("vanillite-bedrock-hud-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.properties"),
        "view-distance=2\nonline-mode=false\nspawn-protection=0\n",
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
    let bedrock_addr = server.bedrock_addr().expect("Bedrock frontend runs");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut c = Client::connect_as(bedrock_addr, "2535400000000003", "Hud", version)
            .await
            .expect("Bedrock login");
        let start = c.recv(id::START_GAME).await.expect("start_game");
        let (own_id, eye) = client::read_start_game(&start).unwrap();
        c.send(id::REQUEST_CHUNK_RADIUS, &[8, 8]).await.unwrap();
        c.recv(id::PLAY_STATUS).await.expect("spawn");
        c.send(
            id::SET_LOCAL_PLAYER_AS_INITIALIZED,
            &[u8::try_from(own_id).unwrap()],
        )
        .await
        .unwrap();
        let mut tick = 0u64;
        let mut input = || {
            tick += 1;
            client::auth_input(
                &AuthInput {
                    pitch: 0.0,
                    yaw: 0.0,
                    position: eye,
                    head_yaw: 0.0,
                    move_vector: [0.0; 2],
                    flags: 1 << input_flag::VERTICAL_COLLISION,
                    tick,
                    delta: [0.0; 3],
                },
                version,
            )
        };
        let (x, z) = (eye[0].floor() as i32, eye[2].floor() as i32);

        handle.console("stopsound .Hud".into());
        until(&mut c, &mut input, "stop_sound", id::STOP_SOUND, |_| true).await;

        // `teleport_entity` moves an entity on Bedrock too (an armour stand: the test server
        // has no mob data).
        let y = (eye[1] - 1.62).round() as i32;
        handle.console(format!(
            "summon armor_stand {} {y} {z} {{NoGravity:1b}}",
            x + 2
        ));
        until(&mut c, &mut input, "armor stand", id::ADD_ACTOR, |_| true).await;
        handle.console(format!("tp @e[type=armor_stand] {} {y} {z}", x + 5));
        until(
            &mut c,
            &mut input,
            "armor stand moved",
            id::MOVE_ACTOR_ABSOLUTE,
            |_| true,
        )
        .await;
        // A border 2 blocks away: the wall shows as deny particles (level event 2008).
        handle.console(format!("worldborder center {} {z}", x + 3));
        handle.console("worldborder set 4".into());
        let deny = vari32(2008);
        until(&mut c, &mut input, "border wall", id::LEVEL_EVENT, |b| {
            b.starts_with(&deny)
        })
        .await;
        // A large border again: the wall goes away.
        handle.console("worldborder set 1000".into());
        handle.console("stopsound .Hud".into());
        until(
            &mut c,
            &mut input,
            "stop_sound again",
            id::STOP_SOUND,
            |_| true,
        )
        .await;
        let quiet = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < quiet {
            if let Ok((p, body)) = c.next(Duration::from_millis(50)).await {
                assert!(
                    !(p == id::LEVEL_EVENT && body.starts_with(&deny)),
                    "wall after the border grew"
                );
            }
        }
    });

    handle.stop();
    thread.join().unwrap();
}
