//! Clients of older versions through the proxy (the previous release 26.2 and the oldest
//! supported, 1.20.5): the server list shows the proxy as compatible, the client joins a
//! current-version backend, and the proxy's own command `/glist` works in its protocol. The
//! backend only ever sees the current version.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::Value;
use vanillite_protocol::codec::{Reader, write_str, write_varint};
use vanillite_proxy::{Proxy, ProxyConfig};
use vanillite_server::{ServerBuilder, VanillaPack};

const TIMEOUT: Duration = Duration::from_secs(20);
const SECRET: &str = "multiversion-proxy-secret";

/// Uncompressed frames over TCP with the older version's packet ids.
struct Old {
    stream: TcpStream,
    buf: Vec<u8>,
    packets: Value,
}

impl Old {
    fn id(&self, state: &str, dir: &str, name: &str) -> i32 {
        let name = format!("minecraft:{name}");
        let list = self.packets[state][dir].as_array().unwrap();
        i32::try_from(list.iter().position(|p| p.as_str() == Some(&name)).unwrap()).unwrap()
    }

    fn send(&mut self, state: &str, name: &str, fields: &[u8]) {
        let mut body = Vec::new();
        write_varint(&mut body, self.id(state, "serverbound", name));
        body.extend_from_slice(fields);
        let mut frame = Vec::new();
        write_varint(&mut frame, i32::try_from(body.len()).unwrap());
        frame.extend_from_slice(&body);
        self.stream.write_all(&frame).unwrap();
    }

    /// The next packet's name in the older version and its fields.
    fn recv(&mut self, state: &str) -> (String, Vec<u8>) {
        loop {
            let mut r = Reader::new(&self.buf);
            if let Ok(len) = r.varint() {
                let len = usize::try_from(len).unwrap();
                let header = self.buf.len() - r.remaining();
                if r.remaining() >= len {
                    let body = self.buf[header..header + len].to_vec();
                    self.buf.drain(..header + len);
                    let mut r = Reader::new(&body);
                    let id = usize::try_from(r.varint().unwrap()).unwrap();
                    let name = self.packets[state]["clientbound"][id]
                        .as_str()
                        .unwrap()
                        .trim_start_matches("minecraft:")
                        .to_owned();
                    return (name, r.rest().to_vec());
                }
            }
            let mut chunk = [0u8; 8192];
            let n = self.stream.read(&mut chunk).expect("read");
            assert!(n > 0, "connection closed");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn handshake(stream: &mut TcpStream, protocol: i32, port: u16, intent: i32) {
    let mut hs = vec![0];
    write_varint(&mut hs, protocol);
    write_str(&mut hs, "localhost");
    hs.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut hs, intent);
    let mut frame = Vec::new();
    write_varint(&mut frame, i32::try_from(hs.len()).unwrap());
    frame.extend_from_slice(&hs);
    stream.write_all(&frame).unwrap();
}

#[test]
fn previous_version_client_through_the_proxy() {
    through_the_proxy("26.2", "26.2", 776);
}

#[test]
fn oldest_version_client_through_the_proxy() {
    through_the_proxy("1.20.6", "1.20.5", 766);
}

/// A client of `release` (protocol `protocol`, data in `data/multiversion/<data>/`).
#[allow(clippy::too_many_lines)]
fn through_the_proxy(data: &str, release: &str, protocol: i32) {
    let dir = std::env::temp_dir().join(format!(
        "vanillite-proxy-mv-{}-{release}",
        std::process::id()
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
        format!("profile = \"vanilla\"\n[proxy]\nvelocity-secret = \"{SECRET}\"\n[multiversion]\nversions = \"none\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("eula.txt"), "eula=true\n").unwrap();
    let mut builder = ServerBuilder::new(&dir);
    builder.vanilla_pack(VanillaPack::None).network(false);
    let server = builder.start().expect("server starts");
    let backend = server.local_addr().expect("network");
    let handle = server.handle();
    let thread = std::thread::spawn(move || server.run());

    let mut config = ProxyConfig::parse(&format!(
        "bind = \"127.0.0.1:0\"\nonline-mode = false\ncompression-threshold = -1\n\
         try = [\"a\"]\nolder-versions = \"{release}\"\n[servers]\na = \"{backend}\"\n\
         [limits]\nlogin-interval-ms = 0\n"
    ))
    .unwrap();
    config.secret = vanillite_net::velocity::Secret::new(SECRET.as_bytes());
    let proxy = Proxy::start(config).unwrap();
    let port = proxy.local_addr().port();
    let packets: Value = serde_json::from_str(
        &std::fs::read_to_string(vanillite_multiversion::data_paths::file(&format!(
            "data/multiversion/{data}/packets.json"
        )))
        .unwrap(),
    )
    .unwrap();
    let connect = || {
        let stream = TcpStream::connect(proxy.local_addr()).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream
    };

    // The server list shows the proxy as compatible with the client.
    let mut stream = connect();
    handshake(&mut stream, protocol, port, 1);
    let mut old = Old {
        stream,
        buf: Vec::new(),
        packets: packets.clone(),
    };
    old.send("status", "status_request", &[]);
    let (name, data) = old.recv("status");
    assert_eq!(name, "status_response");
    let json = Reader::new(&data).string(32767).unwrap().to_owned();
    assert!(json.contains(&format!("\"protocol\":{protocol}")), "{json}");

    // Login and configuration in the older protocol.
    let mut stream = connect();
    handshake(&mut stream, protocol, port, 2);
    let mut old = Old {
        stream,
        buf: Vec::new(),
        packets,
    };
    let mut hello = Vec::new();
    write_str(&mut hello, "Oldie");
    hello.extend_from_slice(
        &vanillite_protocol::types::Uuid::offline("Oldie")
            .0
            .to_be_bytes(),
    );
    old.send("login", "hello", &hello);
    loop {
        let (name, _) = old.recv("login");
        assert_ne!(name, "login_disconnect", "refused by the proxy");
        if name == "login_finished" || name == "game_profile" {
            break;
        }
    }
    old.send("login", "login_acknowledged", &[]);
    loop {
        let (name, data) = old.recv("configuration");
        match name.as_str() {
            "select_known_packs" => {
                old.send("configuration", "select_known_packs", &data);
            }
            "keep_alive" => old.send("configuration", "keep_alive", &data),
            "finish_configuration" => {
                old.send("configuration", "finish_configuration", &[]);
                break;
            }
            "disconnect" => panic!("kicked in configuration"),
            _ => {}
        }
    }

    // Play: the backend's login arrives; the proxy's /glist answers in the older packets.
    let deadline = Instant::now() + TIMEOUT;
    let mut joined = false;
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let (name, data) = old.recv("play");
        match name.as_str() {
            "login" if !joined => {
                joined = true;
                let mut command = Vec::new();
                write_str(&mut command, "glist");
                old.send("play", "chat_command", &command);
            }
            "keep_alive" => old.send("play", "keep_alive", &data),
            "chunk_batch_finished" => {
                old.send("play", "chunk_batch_received", &64.0f32.to_be_bytes());
            }
            "system_chat" if joined => {
                let text = String::from_utf8_lossy(&data).into_owned();
                if text.contains("Oldie") {
                    break;
                }
            }
            "disconnect" => panic!("kicked in play"),
            _ => {}
        }
    }

    proxy.shutdown();
    handle.stop();
    thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
