//! A headless Bedrock client plays on an in-process server (flat test world): takes blocks from
//! the creative palette, breaks and places blocks, moves items in the survival inventory and
//! drops one, which comes back as an item entity; places a chest, opens it and stores items.

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
use vanillite_bedrock::client::{
    self, AuthInput, Client, RequestAction as A, block_action, container, input_flag,
    packet_id as id,
};
use vanillite_server::{ServerBuilder, VanillaPack};

const TIMEOUT: Duration = Duration::from_secs(30);

/// Waits for a packet `pid` that `f` accepts.
async fn until(c: &mut Client, what: &str, pid: u32, mut f: impl FnMut(&[u8]) -> bool) -> Vec<u8> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        let (p, body) = c
            .next(TIMEOUT)
            .await
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        if p == id::DISCONNECT {
            panic!("{what}: disconnected");
        }
        if p == pid && f(&body) {
            return body;
        }
    }
}

#[test]
fn oldest_bedrock_version_builds_and_uses_the_inventory() {
    builds_and_uses_the_inventory(Version::oldest());
}

#[test]
fn newest_bedrock_version_builds_and_uses_the_inventory() {
    builds_and_uses_the_inventory(Version::latest());
}

fn builds_and_uses_the_inventory(version: &'static Version) {
    let dir = std::env::temp_dir().join(format!(
        "vanillite-bedrock-play-{}-{}",
        std::process::id(),
        version.protocol
    ));
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
        let mut c = Client::connect_as(bedrock_addr, "2535400000000002", "Builder", version)
            .await
            .expect("Bedrock login");
        let start = c.recv(id::START_GAME).await.expect("start_game");
        let (own_id, eye) = client::read_start_game(&start).unwrap();
        c.send(id::REQUEST_CHUNK_RADIUS, &[8, 8]).await.unwrap();
        until(&mut c, "spawn", id::PLAY_STATUS, |_| true).await;
        c.send(
            id::SET_LOCAL_PLAYER_AS_INITIALIZED,
            &[u8::try_from(own_id).unwrap()],
        )
        .await
        .unwrap();
        let feet = [
            eye[0].floor() as i32,
            (eye[1] - 1.62).round() as i32,
            eye[2].floor() as i32,
        ];
        let input = |tick: u64| AuthInput {
            pitch: 60.0,
            yaw: 0.0,
            position: eye,
            head_yaw: 0.0,
            move_vector: [0.0; 2],
            flags: 1 << input_flag::VERTICAL_COLLISION,
            tick,
            delta: [0.0; 3],
        };
        c.send(
            id::PLAYER_AUTH_INPUT,
            &client::auth_input(&input(1), version),
        )
        .await
        .unwrap();

        // Creative: a stack of stone from the palette into the first hotbar slot.
        handle.console("gamemode creative .Builder".into());
        until(&mut c, "creative", id::SET_PLAYER_GAME_TYPE, |b| b == [2]).await;
        let stone =
            client::creative_net_id(version, "minecraft:stone").expect("stone in the palette");
        let take = client::stack_request(
            1,
            &[
                A::CraftCreative(stone),
                A::Take(16, (container::CREATED_OUTPUT, 50), (container::CURSOR, 0)),
            ],
        );
        c.send(id::ITEM_STACK_REQUEST, &take).await.unwrap();
        let r = until(&mut c, "creative take", id::ITEM_STACK_RESPONSE, |_| true).await;
        assert_eq!(client::read_stack_response(&r), Some((1, true)));
        let place = client::stack_request(
            2,
            &[A::Place(16, (container::CURSOR, 0), (container::HOTBAR, 0))],
        );
        c.send(id::ITEM_STACK_REQUEST, &place).await.unwrap();
        let r = until(&mut c, "creative place", id::ITEM_STACK_RESPONSE, |_| true).await;
        assert_eq!(client::read_stack_response(&r), Some((2, true)));

        // Break the grass two blocks east (creative breaks at once).
        let target = [feet[0] + 2, feet[1] - 1, feet[2]];
        let breaking = client::auth_input_with_blocks(
            &input(2),
            version,
            &[(block_action::START_BREAK, target, 1)],
        );
        c.send(id::PLAYER_AUTH_INPUT, &breaking).await.unwrap();
        let air = client::air_runtime_id(version);
        until(&mut c, "block broken", id::UPDATE_BLOCK, |b| {
            client::read_update_block(b) == Some((target, air, 0))
        })
        .await;

        // Place the stone into the hole.
        let below = [target[0], target[1] - 1, target[2]];
        c.send(
            id::INVENTORY_TRANSACTION,
            &client::use_item_on(version, below, 1, 0, [0.5, 1.0, 0.5]),
        )
        .await
        .unwrap();
        until(&mut c, "block placed", id::UPDATE_BLOCK, |b| {
            matches!(client::read_update_block(b), Some((p, rid, 0)) if p == target && rid != air)
        })
        .await;

        // Survival: move the stack into the main inventory with Java clicks, drop one.
        handle.console("gamemode survival .Builder".into());
        until(&mut c, "survival", id::SET_PLAYER_GAME_TYPE, |b| b == [0]).await;
        let mv = client::stack_request(
            3,
            &[
                A::Take(16, (container::HOTBAR, 0), (container::CURSOR, 0)),
                A::Place(16, (container::CURSOR, 0), (container::INVENTORY, 9)),
            ],
        );
        c.send(id::ITEM_STACK_REQUEST, &mv).await.unwrap();
        let r = until(&mut c, "inventory move", id::ITEM_STACK_RESPONSE, |_| true).await;
        assert_eq!(client::read_stack_response(&r), Some((3, true)));
        let drop = client::stack_request(4, &[A::Drop(1, (container::INVENTORY, 9))]);
        c.send(id::ITEM_STACK_REQUEST, &drop).await.unwrap();
        let r = until(&mut c, "drop", id::ITEM_STACK_RESPONSE, |_| true).await;
        assert_eq!(client::read_stack_response(&r), Some((4, true)));
        // The dropped stone is an item entity, which proves the server moved the stack.
        until(&mut c, "item entity", id::ADD_ITEM_ACTOR, |_| true).await;

        // A chest: place it on the stone, open it and move the stones into it.
        handle.console("give .Builder minecraft:chest 1".into());
        // Window 0 slot 0, no container name, no storage item, then a non-air item.
        until(&mut c, "chest given", id::INVENTORY_SLOT, |b| {
            b.get(..2) == Some(&[0, 0]) && b.get(4..6).is_some_and(|i| i != [0, 0])
        })
        .await;
        c.send(
            id::INVENTORY_TRANSACTION,
            &client::use_item_on(version, target, 1, 0, [0.5, 1.0, 0.5]),
        )
        .await
        .unwrap();
        let chest = [target[0], target[1] + 1, target[2]];
        until(&mut c, "chest placed", id::UPDATE_BLOCK, |b| {
            matches!(client::read_update_block(b), Some((p, rid, 0)) if p == chest && rid != air)
        })
        .await;
        c.send(
            id::INVENTORY_TRANSACTION,
            &client::use_item_on(version, chest, 1, 0, [0.5, 1.0, 0.5]),
        )
        .await
        .unwrap();
        let open = until(&mut c, "chest open", id::CONTAINER_OPEN, |b| {
            b.get(1) == Some(&0)
        })
        .await;
        let window = open[0];
        until(&mut c, "chest content", id::INVENTORY_CONTENT, |b| {
            b.first() == Some(&window)
        })
        .await;
        let store = client::stack_request(
            5,
            &[
                A::Take(15, (container::INVENTORY, 9), (container::CURSOR, 0)),
                A::Place(15, (container::CURSOR, 0), (container::LEVEL_ENTITY, 0)),
            ],
        );
        c.send(id::ITEM_STACK_REQUEST, &store).await.unwrap();
        let r = until(&mut c, "store", id::ITEM_STACK_RESPONSE, |_| true).await;
        assert_eq!(client::read_stack_response(&r), Some((5, true)));
        c.send(id::CONTAINER_CLOSE, &[window, 0, 0]).await.unwrap();
        until(&mut c, "chest closed", id::CONTAINER_CLOSE, |b| {
            b.first() == Some(&window)
        })
        .await;
        // Opened again, the chest holds the stones: the server stored them.
        c.send(
            id::INVENTORY_TRANSACTION,
            &client::use_item_on(version, chest, 1, 0, [0.5, 1.0, 0.5]),
        )
        .await
        .unwrap();
        let open = until(&mut c, "chest open again", id::CONTAINER_OPEN, |_| true).await;
        let window = open[0];
        let content = until(&mut c, "chest content again", id::INVENTORY_CONTENT, |b| {
            b.first() == Some(&window)
        })
        .await;
        // Window, 27 slots, then the first item: a non-air id and a count of 15.
        assert_eq!(content[1], 27);
        assert_ne!(
            &content[2..4],
            &[0, 0],
            "the first chest slot holds the stones"
        );
        assert_eq!(u16::from_le_bytes([content[4], content[5]]), 15);
    });

    handle.stop();
    thread.join().unwrap();
}
