//! A headless Bedrock client joins through the proxy's Bedrock frontend: the proxy logs it in
//! to an in-process Vanillite backend with modern forwarding, and the client gets the world.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::time::Duration;

use vanillite_bedrock::client::{self, Client, packet_id as id};
use vanillite_proxy::{Proxy, ProxyConfig};
use vanillite_server::{ServerBuilder, VanillaPack};

const TIMEOUT: Duration = Duration::from_secs(30);
const SECRET: &str = "proxy-bedrock-secret";

#[test]
fn bedrock_player_joins_through_the_proxy() {
    let dir = std::env::temp_dir().join(format!("vanillite-proxy-bedrock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.properties"),
        "view-distance=2\nonline-mode=false\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("vanillite.toml"),
        format!("profile = \"vanilla\"\n[proxy]\nvelocity-secret = \"{SECRET}\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    let mut builder = ServerBuilder::new(dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let backend = server.local_addr().expect("network");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    let mut config = ProxyConfig::parse(&format!(
        "bind = \"127.0.0.1:0\"\nonline-mode = false\ntry = [\"a\"]\n\
         [servers]\na = \"{backend}\"\n[limits]\nlogin-interval-ms = 0\n\
         [bedrock]\nenabled = true\nbind = \"127.0.0.1:0\"\nonline-mode = false\n"
    ))
    .unwrap();
    config.secret = vanillite_net::velocity::Secret::new(SECRET.as_bytes());
    let proxy = Proxy::start(config).unwrap();
    let bedrock_addr = proxy.bedrock_addr().expect("Bedrock frontend runs");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut bedrock = Client::connect(bedrock_addr, "2535400000000002", "Proxy Tester")
            .await
            .expect("Bedrock login");
        let start = bedrock.recv(id::START_GAME).await.expect("start_game");
        let (own_id, _spawn) = client::read_start_game(&start).unwrap();
        assert!(own_id > 0);
        bedrock
            .send(id::REQUEST_CHUNK_RADIUS, &[8, 8])
            .await
            .unwrap();
        let mut chunks = 0;
        loop {
            let (pid, _) = bedrock
                .next(TIMEOUT)
                .await
                .expect("packets after start_game");
            match pid {
                id::LEVEL_CHUNK => chunks += 1,
                id::PLAY_STATUS => break,
                _ => {}
            }
        }
        assert!(chunks > 0, "chunks before spawning");
    });

    proxy.shutdown();
    handle.stop();
    thread.join().unwrap();
}
