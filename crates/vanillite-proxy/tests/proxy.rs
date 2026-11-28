//! The proxy in front of two in-process Vanillite servers with modern forwarding: a player
//! joins the first server, switches to the second with `/server` without reconnecting, is moved
//! by the console's `send`, and is sent back to the first when the second stops. Both servers are Vanillite, so the proxy
//! relays them over the trusted pass-through link.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use vanillite_protocol::Packet;
use vanillite_protocol::client::{ClientError, TestClient};
use vanillite_protocol::commands::NodeKind;
use vanillite_protocol::packets::handshake::Intent;
use vanillite_protocol::packets::play::clientbound::StartConfiguration;
use vanillite_protocol::packets::play::serverbound::{
    ChatCommand, ChunkBatchReceived, CommandSuggestion, ConfigurationAcknowledged, KeepAlive,
};
use vanillite_protocol::sets::ClientboundPlay;
use vanillite_proxy::{Proxy, ProxyConfig};
use vanillite_server::{ServerBuilder, VanillaPack};

const TIMEOUT: Duration = Duration::from_secs(20);
const SECRET: &str = "proxy-test-secret";

fn server_dir(name: &str, max_players: u32) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vanillite-proxy-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.properties"),
        format!("view-distance=2\nonline-mode=false\nmax-players={max_players}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("vanillite.toml"),
        format!("profile = \"vanilla\"\n[proxy]\nvelocity-secret = \"{SECRET}\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    dir
}

/// Receives play packets until `f` matches, answering keepalives and chunk batches. A
/// `start_configuration` returns `None`.
fn until(
    client: &mut TestClient<TcpStream>,
    what: &str,
    mut f: impl FnMut(&ClientboundPlay) -> bool,
) -> Option<ClientboundPlay> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let raw = client.recv_raw().expect("receive");
        if raw.first() == Some(&u8::try_from(StartConfiguration::ID).unwrap()) && raw.len() == 1 {
            return None;
        }
        let packet = ClientboundPlay::decode(&raw).expect("decode");
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
            return Some(packet);
        }
    }
}

/// Waits for the play login and returns its `max_players`, which tells the servers apart.
fn login_max_players(client: &mut TestClient<TcpStream>) -> i32 {
    match until(client, "login", |p| matches!(p, ClientboundPlay::Login(_))) {
        Some(ClientboundPlay::Login(l)) => l.max_players,
        _ => panic!("expected login"),
    }
}

/// Waits for `start_configuration`, acknowledges and runs the configuration phase.
fn reconfigure(client: &mut TestClient<TcpStream>, what: &str) {
    assert!(until(client, what, |_| false).is_none());
    client.send(&ConfigurationAcknowledged).unwrap();
    client.configure().expect("configuration after the switch");
}

#[test]
fn join_switch_and_fallback() {
    let mut servers = Vec::new();
    let mut addrs = Vec::new();
    for (name, max) in [("a", 11), ("b", 22)] {
        let mut builder = ServerBuilder::new(server_dir(name, max));
        builder.vanilla_pack(VanillaPack::None).network(false);
        let server = builder.start().expect("server starts");
        addrs.push(server.local_addr().expect("network"));
        let handle = server.handle();
        servers.push((handle, std::thread::spawn(move || server.run())));
    }

    let mut config = ProxyConfig::parse(&format!(
        "bind = \"127.0.0.1:0\"\nonline-mode = false\ntry = [\"a\", \"b\"]\n\
         [servers]\na = \"{}\"\nb = \"{}\"\n[limits]\nlogin-interval-ms = 0\n",
        addrs[0], addrs[1]
    ))
    .unwrap();
    config.secret = vanillite_net::velocity::Secret::new(SECRET.as_bytes());
    let proxy = Proxy::start(config).unwrap();

    // A direct connection to a backend is refused: forwarding is required.
    let stream = TcpStream::connect(addrs[1]).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut direct = TestClient::new(stream);
    direct.handshake("localhost", 25565, Intent::Login).unwrap();
    assert!(direct.login("Mallory", None).is_err());

    // Join through the proxy: the first server of the try order.
    let stream = TcpStream::connect(proxy.local_addr()).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut client = TestClient::new(stream);
    client
        .handshake("localhost", proxy.local_addr().port(), Intent::Login)
        .unwrap();
    let profile = client.login("Alice", None).unwrap();
    assert_eq!(profile.name, "Alice");
    client.configure().unwrap();
    assert_eq!(login_max_players(&mut client), 11);

    // The proxy's commands are in the command tree.
    let tree = until(&mut client, "commands", |p| {
        matches!(p, ClientboundPlay::Commands(_))
    });
    let Some(ClientboundPlay::Commands(tree)) = tree else {
        panic!("expected commands");
    };
    let root = &tree.0.nodes[usize::try_from(tree.0.root).unwrap()];
    assert!(root.children.iter().any(|&c| matches!(
        &tree.0.nodes[usize::try_from(c).unwrap()].kind,
        NodeKind::Literal(l) if l == "server"
    )));

    // `/server` completes server names.
    client
        .send(&CommandSuggestion {
            id: 7,
            command: "/server ".to_owned(),
        })
        .unwrap();
    let suggestions = until(&mut client, "suggestions", |p| {
        matches!(p, ClientboundPlay::CommandSuggestions(_))
    });
    let Some(ClientboundPlay::CommandSuggestions(s)) = suggestions else {
        panic!("expected suggestions");
    };
    let names: Vec<&str> = s.suggestions.iter().map(|s| s.text.as_str()).collect();
    assert_eq!((s.id, s.start, s.length, names), (7, 8, 0, vec!["a", "b"]));

    // Switch to the second server without reconnecting.
    client
        .send(&ChatCommand {
            command: "server b".to_owned(),
        })
        .unwrap();
    reconfigure(&mut client, "the switch to b");
    assert_eq!(login_max_players(&mut client), 22);
    assert!(proxy.console("glist").contains("[b] (1): Alice"));

    // The console moves players: one by name, then everybody.
    assert_eq!(proxy.console("send alice a"), "Sending 1 players to a.");
    reconfigure(&mut client, "the send to a");
    assert_eq!(login_max_players(&mut client), 11);
    assert_eq!(proxy.console("send all b"), "Sending 1 players to b.");
    reconfigure(&mut client, "the send to b");
    assert_eq!(login_max_players(&mut client), 22);

    // The second server stops: the proxy falls back to the first.
    let (handle, thread) = servers.pop().unwrap();
    handle.stop();
    thread.join().unwrap();
    reconfigure(&mut client, "the fallback to a");
    assert_eq!(login_max_players(&mut client), 11);

    proxy.shutdown();
    for (handle, thread) in servers {
        handle.stop();
        thread.join().unwrap();
    }
}

/// A player no server admits (whitelists on, #206) is disconnected with the last server's
/// reason after each server was tried once, instead of being passed between them.
#[test]
fn refused_everywhere_ends_with_the_reason() {
    let mut servers = Vec::new();
    let mut addrs = Vec::new();
    for name in ["wl-a", "wl-b"] {
        let dir = server_dir(name, 20);
        let properties = dir.join("server.properties");
        let mut text = std::fs::read_to_string(&properties).unwrap();
        text.push_str("white-list=true\n");
        std::fs::write(&properties, text).unwrap();
        let mut builder = ServerBuilder::new(dir);
        builder.vanilla_pack(VanillaPack::None).network(false);
        let server = builder.start().expect("server starts");
        addrs.push(server.local_addr().expect("network"));
        let handle = server.handle();
        servers.push((handle, std::thread::spawn(move || server.run())));
    }
    let mut config = ProxyConfig::parse(&format!(
        "bind = \"127.0.0.1:0\"\nonline-mode = false\ntry = [\"a\", \"b\"]\n\
         [servers]\na = \"{}\"\nb = \"{}\"\n[limits]\nlogin-interval-ms = 0\n",
        addrs[0], addrs[1]
    ))
    .unwrap();
    config.secret = vanillite_net::velocity::Secret::new(SECRET.as_bytes());
    let proxy = Proxy::start(config).unwrap();

    let stream = TcpStream::connect(proxy.local_addr()).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut client = TestClient::new(stream);
    client
        .handshake("localhost", proxy.local_addr().port(), Intent::Login)
        .unwrap();
    client.login("Mallory", None).unwrap();
    // Each Vanillite refuses in its login state: the proxy tries the other server once,
    // then disconnects the client (in configuration) with the last server's reason.
    match client.configure() {
        Err(ClientError::Disconnected(reason)) => {
            assert_eq!(reason, "multiplayer.disconnect.not_whitelisted");
        }
        other => panic!("expected the servers' refusal, got {other:?}"),
    }
    // Nothing after the kick: the connection is closed.
    assert!(client.recv_raw().is_err());

    drop(proxy);
    for (handle, thread) in servers {
        handle.stop();
        thread.join().unwrap();
    }
}
