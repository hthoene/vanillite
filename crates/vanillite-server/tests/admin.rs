//! RCON and Query on the wire against a running server (`enable-rcon`, `enable-query`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

use vanillite_server::{ServerBuilder, VanillaPack};

fn free_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn send(stream: &mut TcpStream, id: i32, kind: i32, body: &str) {
    let mut out = Vec::new();
    out.extend_from_slice(&(i32::try_from(body.len()).unwrap() + 10).to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(&[0, 0]);
    stream.write_all(&out).unwrap();
}

fn receive(stream: &mut TcpStream) -> (i32, i32, String) {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; usize::try_from(i32::from_le_bytes(len)).unwrap()];
    stream.read_exact(&mut body).unwrap();
    let id = i32::from_le_bytes(body[0..4].try_into().unwrap());
    let kind = i32::from_le_bytes(body[4..8].try_into().unwrap());
    let text = String::from_utf8(body[8..body.len() - 2].to_vec()).unwrap();
    (id, kind, text)
}

#[test]
fn rcon_and_query_answer_like_vanilla() {
    let dir = std::env::temp_dir().join(format!("vanillite-admin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (rcon_port, query_port) = (free_tcp_port(), free_udp_port());
    std::fs::write(
        dir.join("server.properties"),
        format!(
            "view-distance=2\nmotd=Admin Test\nenable-rcon=true\nrcon.port={rcon_port}\n\
             rcon.password=correct-horse\nenable-query=true\nquery.port={query_port}\n\
             server-ip=127.0.0.1\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("vanillite.toml"), "profile = \"vanilla\"\n").unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    // RCON: commands before the login and wrong passwords get request id -1.
    let mut rcon = TcpStream::connect(("127.0.0.1", rcon_port)).unwrap();
    rcon.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    send(&mut rcon, 1, 2, "list");
    assert_eq!(receive(&mut rcon), (-1, 2, String::new()));
    send(&mut rcon, 2, 3, "wrong-password");
    assert_eq!(receive(&mut rcon), (-1, 2, String::new()));
    send(&mut rcon, 3, 3, "correct-horse");
    assert_eq!(receive(&mut rcon), (3, 2, String::new()));
    // Without the Vanilla pack there are no English texts: the key is the answer.
    send(&mut rcon, 4, 2, "list");
    assert_eq!(
        receive(&mut rcon),
        (4, 0, "commands.list.players".to_owned())
    );
    send(&mut rcon, 5, 9, "");
    assert_eq!(receive(&mut rcon), (5, 0, "Unknown request 9".to_owned()));
    // A packet longer than Vanilla's buffer closes the connection.
    rcon.write_all(&5000i32.to_le_bytes()).unwrap();
    let mut rest = Vec::new();
    assert_eq!(
        rcon.read_to_end(&mut rest).map(|_| rest.len()).ok(),
        Some(0)
    );

    // Query: challenge, then basic and full stat.
    let query = UdpSocket::bind("127.0.0.1:0").unwrap();
    query
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    query.connect(("127.0.0.1", query_port)).unwrap();
    let mut buf = [0u8; 2048];
    query.send(&[0xFE, 0xFD, 9, 0, 0, 0, 1]).unwrap();
    let n = query.recv(&mut buf).unwrap();
    assert_eq!(&buf[..5], &[9, 0, 0, 0, 1]);
    let token: i32 = std::str::from_utf8(&buf[5..n - 1])
        .unwrap()
        .parse()
        .unwrap();
    let mut basic = vec![0xFE, 0xFD, 0, 0, 0, 0, 1];
    basic.extend_from_slice(&token.to_be_bytes());
    query.send(&basic).unwrap();
    let n = query.recv(&mut buf).unwrap();
    let mut expected = vec![0, 0, 0, 0, 1];
    expected.extend_from_slice(b"Admin Test\0SMP\0world\x000\x0020\0");
    expected.extend_from_slice(&25565u16.to_le_bytes());
    expected.extend_from_slice(b"127.0.0.1\0");
    assert_eq!(&buf[..n], &expected[..]);
    let mut full = basic;
    full.extend_from_slice(&[0, 0, 0, 0]);
    query.send(&full).unwrap();
    let n = query.recv(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(text.contains("\0hostname\0Admin Test\0gametype\0SMP\0game_id\0MINECRAFT\0"));
    assert!(text.contains("\0map\0world\0numplayers\x000\0maxplayers\x0020\0"));
    assert!(text.ends_with("\0\u{1}player_\0\0\0"));

    handle.stop();
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn configuration_sends_links_code_of_conduct_and_required_pack() {
    use vanillite_protocol::client::TestClient;
    use vanillite_protocol::packets::common::ResourcePackAction;
    use vanillite_protocol::packets::configuration::serverbound as csb;
    use vanillite_protocol::packets::handshake::Intent;
    use vanillite_protocol::sets::ClientboundConfiguration;

    let dir = std::env::temp_dir().join(format!("vanillite-admin-cfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("codeofconduct")).unwrap();
    std::fs::write(dir.join("codeofconduct/en_us.txt"), "Be nice.").unwrap();
    std::fs::write(
        dir.join("server.properties"),
        "view-distance=2\nonline-mode=false\nenable-status=false\nenable-code-of-conduct=true\n\
         bug-report-link=https\\://example.com/bugs\nrequire-resource-pack=true\n\
         resource-pack=https\\://example.com/pack.zip\n",
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("vanillite.toml"), "profile = \"vanilla\"\n").unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let addr = server.local_addr().expect("listening");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());
    let connect = || {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        TestClient::new(stream)
    };

    // `enable-status=false`: the connection is closed without an answer.
    let mut pinger = connect();
    pinger
        .handshake("localhost", addr.port(), Intent::Status)
        .unwrap();
    assert!(pinger.status().is_err());

    // A client that accepts everything gets the links, the code of conduct and the pack.
    let mut alice = connect();
    alice
        .handshake("localhost", addr.port(), Intent::Login)
        .unwrap();
    alice.login("Alice", None).unwrap();
    alice.configure().unwrap();
    assert!(alice.received.contains(&"server_links"));
    assert!(alice.received.contains(&"resource_pack_push"));

    // Declining the required pack kicks.
    let mut bob = connect();
    bob.handshake("localhost", addr.port(), Intent::Login)
        .unwrap();
    bob.login("Bob", None).unwrap();
    bob.send(&csb::ClientInformation(Default::default()))
        .unwrap();
    let reason = loop {
        match ClientboundConfiguration::decode(&bob.recv_raw().unwrap()).unwrap() {
            ClientboundConfiguration::SelectKnownPacks(p) => {
                bob.send(&csb::SelectKnownPacks { packs: p.packs }).unwrap()
            }
            ClientboundConfiguration::CodeOfConduct(c) => {
                assert_eq!(c.text, "Be nice.");
                bob.send(&csb::AcceptCodeOfConduct).unwrap();
            }
            ClientboundConfiguration::ResourcePackPush(p) => {
                assert!(p.required);
                bob.send(&csb::ResourcePack {
                    id: p.id,
                    action: ResourcePackAction::Declined,
                })
                .unwrap();
            }
            ClientboundConfiguration::KeepAlive(k) => {
                bob.send(&csb::KeepAlive { id: k.id }).unwrap()
            }
            ClientboundConfiguration::Disconnect(d) => break d.reason,
            ClientboundConfiguration::FinishConfiguration(_) => panic!("joined"),
            _ => {}
        }
    };
    assert_eq!(
        reason.plain(),
        "multiplayer.requiredTexturePrompt.disconnect"
    );

    handle.stop();
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A player the server refuses (whitelist on, not on it) gets a login disconnect with
/// Vanilla's reason before `LoginFinished`, not a kick after configuration (#212).
#[test]
fn refusal_is_a_login_disconnect() {
    use vanillite_protocol::client::{ClientError, TestClient};
    use vanillite_protocol::packets::handshake::Intent;

    let dir = std::env::temp_dir().join(format!("vanillite-admin-wl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.properties"),
        "view-distance=2\nonline-mode=false\nwhite-list=true\n",
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    std::fs::write(dir.join("vanillite.toml"), "profile = \"vanilla\"\n").unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let addr = server.local_addr().expect("listening");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    let stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut client = TestClient::new(stream);
    client
        .handshake("localhost", addr.port(), Intent::Login)
        .unwrap();
    match client.login("Mallory", None) {
        Err(ClientError::Disconnected(reason)) => {
            assert_eq!(reason, "multiplayer.disconnect.not_whitelisted");
        }
        other => panic!("expected a login disconnect, got {other:?}"),
    }

    handle.stop();
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
