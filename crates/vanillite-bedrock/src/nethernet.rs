//! NetherNet, the WebRTC transport Bedrock clients use since 1.26.60 (RakNet is deprecated and
//! current clients refuse to fall back to it, "InitialConnection-149").
//!
//! Signaling is plain HTTP on the game port (TCP): `GET /v1/join` answers the server status,
//! `POST /v1/join/{network id}` takes the client's SDP offer (all candidates included, no
//! trickle ICE) and returns the answer. ICE (lite, controlled), DTLS and SCTP then run on the
//! frontend's UDP socket next to RakNet: datagrams from unknown addresses are matched to their
//! session by the ICE username of their STUN request. Game batches travel on the
//! `ReliableDataChannel`, each message split into segments behind a one-byte count of the
//! segments still to come; they carry no `0xfe` id and no Bedrock encryption (DTLS encrypts).
//!
//! Both sides prove their identity in the SDP `a=identity` attribute: a token with the public
//! key (`cpk`) and an ES384 signature of the DTLS fingerprints. The client's key must be the key
//! of its login, so a login cannot be replayed over another connection. The server signs with a
//! key kept in a file, because clients remember it (trust on first use).
//!
//! The protocol follows the description of go-nethernet (MIT, df-mc); no code is taken from it.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use p384::ecdsa::signature::{Signer, Verifier};
use rustc_hash::FxHashMap;
use serde_json::Value;
use str0m::channel::ChannelId;
use str0m::config::Fingerprint;
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, IceCreds, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

use crate::raknet::{self, Received};
use crate::session::Shared;
use crate::version::Version;

/// Datagrams of one NetherNet session with their source address.
pub(crate) type Inbox = mpsc::Sender<(SocketAddr, Vec<u8>)>;

const RELIABLE: &str = "ReliableDataChannel";
const UNRELIABLE: &str = "UnreliableDataChannel";
/// Largest request head and body (an SDP offer is a few KiB).
const MAX_HEAD: usize = 8 << 10;
const MAX_BODY: usize = 64 << 10;
/// Signaling connections at the same time.
const MAX_HTTP: usize = 64;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Payload of one segment; a message has at most 255 segments. A segment and its count byte
/// must fit str0m's limit for outgoing messages: its direct API never learns the client's
/// `a=max-message-size` and keeps the 64 KiB default, refusing anything larger (#182).
const SEGMENT: usize = (64 << 10) - 1;
/// `a=max-message-size` the server accepts.
const MAX_MESSAGE: u32 = 256 << 10;
/// Data the SCTP layer may hold unacknowledged.
const SCTP_BUFFER: usize = 1 << 20;
const STUN_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];

/// Sessions waiting for or using their UDP path, by local ICE username.
pub(crate) struct Hub {
    sessions: Mutex<FxHashMap<String, Inbox>>,
    per_ip: Mutex<raknet::PerIp>,
    key: p384::SecretKey,
    /// Tells the UDP loop to forget the addresses of ended sessions.
    closed: mpsc::UnboundedSender<Option<SocketAddr>>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub").finish_non_exhaustive()
    }
}

impl Hub {
    /// A hub signing with the key in `key_file` (created when missing).
    pub(crate) fn new(
        key_file: Option<&Path>,
        closed: mpsc::UnboundedSender<Option<SocketAddr>>,
    ) -> Self {
        Self {
            closed,
            sessions: Mutex::new(FxHashMap::default()),
            per_ip: Mutex::new(raknet::PerIp::default()),
            key: load_key(key_file),
        }
    }

    /// The session a STUN request from an unknown address belongs to.
    pub(crate) fn route(&self, datagram: &[u8]) -> Option<Inbox> {
        let ufrag = stun_ufrag(datagram)?;
        self.sessions.lock().ok()?.get(ufrag).cloned()
    }

    fn leave(&self, ufrag: &str, ip: IpAddr) {
        if let Ok(mut s) = self.sessions.lock() {
            s.remove(ufrag);
        }
        if let Ok(mut p) = self.per_ip.lock() {
            p.release(ip);
        }
        let _ = self.closed.send(None);
    }
}

/// The server key, read from `path` or created there.
fn load_key(path: Option<&Path>) -> p384::SecretKey {
    let Some(path) = path else {
        return <p384::SecretKey as p384::elliptic_curve::Generate>::generate();
    };
    if let Ok(text) = std::fs::read_to_string(path)
        && let Some(key) = hex(text.trim()).and_then(|b| p384::SecretKey::from_slice(&b).ok())
    {
        return key;
    }
    let key = <p384::SecretKey as p384::elliptic_curve::Generate>::generate();
    let text: String = key.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()))
    {
        Ok(()) => log::info!("Created the Bedrock server identity {}", path.display()),
        Err(e) => log::warn!(
            "Cannot save the Bedrock server identity to {}: {e}; clients will be asked to trust the server again after a restart",
            path.display()
        ),
    }
    key
}

fn hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok())
        .collect()
}

/// The local ICE username of a STUN binding request (`server:client`).
fn stun_ufrag(d: &[u8]) -> Option<&str> {
    if d.get(4..8)? != STUN_COOKIE || *d.first()? > 1 {
        return None;
    }
    let mut at = 20usize;
    while let Some(head) = d.get(at..at.checked_add(4)?) {
        let kind = u16::from_be_bytes([*head.first()?, *head.get(1)?]);
        let len = usize::from(u16::from_be_bytes([*head.get(2)?, *head.get(3)?]));
        let start = at.checked_add(4)?;
        let value = d.get(start..start.checked_add(len)?)?;
        if kind == 0x0006 {
            let user = std::str::from_utf8(value).ok()?;
            return user.split(':').next();
        }
        at = start.checked_add(len.checked_add(3)? & !3)?;
    }
    None
}

// ---- Signaling ----------------------------------------------------------------------------

/// Serves `/v1/join` on the game port.
pub(crate) async fn serve(
    listener: TcpListener,
    shared: Arc<Shared>,
    hub: Arc<Hub>,
    socket: Arc<UdpSocket>,
) {
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_HTTP));
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log::debug!("Bedrock signaling accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            continue;
        };
        let (shared, hub, socket) = (Arc::clone(&shared), Arc::clone(&hub), Arc::clone(&socket));
        tokio::spawn(async move {
            let _permit = permit;
            let _ =
                tokio::time::timeout(HTTP_TIMEOUT, request(stream, from, &shared, &hub, &socket))
                    .await;
        });
    }
}

struct Response {
    code: u16,
    kind: &'static str,
    body: String,
}

impl Response {
    fn text(code: u16, body: &str) -> Self {
        Self {
            code,
            kind: "text/plain",
            body: body.into(),
        }
    }
}

async fn request(
    mut stream: TcpStream,
    from: SocketAddr,
    shared: &Arc<Shared>,
    hub: &Arc<Hub>,
    socket: &Arc<UdpSocket>,
) {
    let Ok(local) = stream.local_addr() else {
        return;
    };
    let mut buf = Vec::with_capacity(2048);
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD || !read_more(&mut stream, &mut buf).await {
            return;
        }
        // HTTPS is probed first: refuse TLS at once so the client goes on to HTTP.
        if !buf.first().is_some_and(u8::is_ascii_uppercase) {
            return;
        }
    };
    let Ok(head) = std::str::from_utf8(buf.get(..head_end).unwrap_or_default()) else {
        return;
    };
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (
        parts.next().unwrap_or_default().to_owned(),
        parts
            .next()
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
            .to_owned(),
    );
    let mut length = 0usize;
    let mut host = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().unwrap_or(usize::MAX);
        } else if name.eq_ignore_ascii_case("host") {
            host = value.chars().take(256).collect();
        }
    }
    let response = if method == "GET" && path == "/v1/join" {
        Response {
            code: 200,
            kind: "application/json",
            body: status_json(shared),
        }
    } else if method == "POST"
        && let Some(id) = path.strip_prefix("/v1/join/")
    {
        if id.parse::<u64>().is_err() {
            Response::text(400, "Network ID must be uint64")
        } else if length == 0 || length > MAX_BODY {
            Response::text(400, "Missing or too large SDP offer")
        } else {
            let body_start = head_end.saturating_add(4);
            let want = body_start.saturating_add(length);
            while buf.len() < want {
                if !read_more(&mut stream, &mut buf).await {
                    return;
                }
            }
            let offer = buf
                .get(body_start..want)
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or_default();
            match accept(offer, from, local, &host, shared, hub, socket).await {
                Ok(answer) => Response {
                    code: 200,
                    kind: "application/sdp",
                    body: answer,
                },
                Err((code, why)) => {
                    log::info!("Bedrock NetherNet offer from {from} refused: {why}");
                    Response::text(code, &why)
                }
            }
        }
    } else {
        Response::text(404, "Not found")
    };
    let reason = match response.code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Service Unavailable",
    };
    let out = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.code,
        response.kind,
        response.body.len(),
        response.body
    );
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 4096];
    match stream.read(&mut chunk).await {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
            true
        }
    }
}

fn status_json(shared: &Shared) -> String {
    let (motd, online, max) = shared.status.values();
    serde_json::json!({
        "name": motd,
        "protocol": Version::latest().protocol,
        "version": Version::latest().name(),
        "level": shared.config.world_name,
        "players": online,
        "maxPlayers": max,
        "gameType": 0,
    })
    .to_string()
}

/// What the offer says.
#[derive(Debug, Default)]
struct Offer<'a> {
    ufrag: &'a str,
    pwd: &'a str,
    fingerprint: &'a str,
    setup: &'a str,
    mid: &'a str,
    max_message: u32,
    identity: Option<&'a str>,
}

fn parse_offer(sdp: &str) -> Option<Offer<'_>> {
    let mut o = Offer {
        mid: "0",
        ..Offer::default()
    };
    let mut media = 0u32;
    for line in sdp.lines() {
        let line = line.trim_end();
        if line.starts_with("m=") {
            media = media.saturating_add(1);
            continue;
        }
        let Some(attr) = line.strip_prefix("a=") else {
            continue;
        };
        let (key, value) = attr.split_once(':').unwrap_or((attr, ""));
        match key {
            "ice-ufrag" => o.ufrag = value,
            "ice-pwd" => o.pwd = value,
            "fingerprint" => o.fingerprint = value,
            "setup" => o.setup = value,
            "mid" => o.mid = value,
            "max-message-size" => o.max_message = value.parse().unwrap_or(0),
            "identity" => o.identity = Some(value),
            _ => {}
        }
    }
    let usable = media == 1
        && !o.ufrag.is_empty()
        && !o.pwd.is_empty()
        && o.fingerprint.contains(' ')
        && o.max_message > 1
        && o.mid.len() <= 32
        && o.mid.chars().all(|c| c.is_ascii_alphanumeric());
    usable.then_some(o)
}

/// The candidate address: where the client reached the signaling, with the UDP port.
async fn public_address(host: &str, local: SocketAddr, udp_port: u16) -> Option<SocketAddr> {
    if host.is_empty() {
        return Some(SocketAddr::new(local.ip(), udp_port));
    }
    if let Ok(a) = host.parse::<SocketAddr>() {
        return Some(a);
    }
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, udp_port));
    }
    let with_port = if host.contains(':') {
        host.to_owned()
    } else {
        format!("{host}:{udp_port}")
    };
    let found: Vec<SocketAddr> = tokio::net::lookup_host(with_port).await.ok()?.collect();
    found
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| found.first())
        .copied()
}

/// Sets up a session for an offer and returns the answer.
async fn accept(
    sdp: &str,
    from: SocketAddr,
    tcp_local: SocketAddr,
    host: &str,
    shared: &Arc<Shared>,
    hub: &Arc<Hub>,
    socket: &Arc<UdpSocket>,
) -> Result<String, (u16, String)> {
    let bad = |why: &str| (400u16, why.to_owned());
    let offer = parse_offer(sdp).ok_or_else(|| bad("malformed SDP offer"))?;
    let client_key = match offer.identity {
        Some(identity) => Some(verify_identity(identity, offer.fingerprint).map_err(|e| bad(&e))?),
        None if shared.config.online_mode => return Err(bad("the offer has no identity")),
        None => None,
    };
    let udp_port = socket
        .local_addr()
        .map_err(|e| (503, e.to_string()))?
        .port();
    let candidate_addr = public_address(host, tcp_local, udp_port)
        .await
        .filter(|a| !a.ip().is_unspecified())
        .ok_or_else(|| bad("cannot resolve the Host header"))?;
    let (conn, answer) = connect(
        &offer,
        candidate_addr,
        client_key,
        shared.config.max_batch_size,
        &hub.key,
        Instant::now(),
    )?;
    let ufrag = conn.ufrag.clone();
    let (tx, rx) = mpsc::channel(512);
    {
        let mut sessions = hub.sessions.lock().map_err(|_| (503, "poisoned".into()))?;
        let full = sessions.len() >= usize::try_from(shared.config.max_connections).unwrap_or(0);
        let admitted = !full
            && hub
                .per_ip
                .lock()
                .is_ok_and(|mut p| p.admit(from.ip(), shared.config.max_connections_per_ip));
        if !admitted {
            return Err((503, "too many connections".into()));
        }
        sessions.insert(ufrag.clone(), tx);
    }
    let (shared, hub, socket) = (Arc::clone(shared), Arc::clone(hub), Arc::clone(socket));
    tokio::spawn(async move {
        crate::session::run(
            shared,
            socket,
            from,
            crate::session::Link::Net(Box::new(conn)),
            rx,
        )
        .await;
        hub.leave(&ufrag, from.ip());
    });
    Ok(answer)
}

/// The server side of an offer: its connection and the answer.
fn connect(
    offer: &Offer<'_>,
    candidate_addr: SocketAddr,
    client_key: Option<p384::PublicKey>,
    max_message: usize,
    key: &p384::SecretKey,
    now: Instant,
) -> Result<(NetConn, String), (u16, String)> {
    let bad = |why: &str| (400u16, why.to_owned());
    let candidate =
        Candidate::host(candidate_addr, "udp").map_err(|e| bad(&format!("candidate: {e}")))?;
    let rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_sctp_max_message_size(MAX_MESSAGE)
        .set_sctp_max_buffered_amount(SCTP_BUFFER)
        .build(now);
    let fingerprint: Fingerprint = offer
        .fingerprint
        .parse()
        .map_err(|e: String| bad(&format!("fingerprint: {e}")))?;
    let segment = usize::try_from(offer.max_message)
        .unwrap_or(usize::MAX)
        .saturating_sub(1)
        .clamp(1, SEGMENT);
    let mut conn = NetConn {
        rtc,
        destination: candidate_addr,
        reliable: None,
        incoming: Vec::new(),
        remaining: None,
        queue: std::collections::VecDeque::new(),
        queued: 0,
        segment,
        max_message,
        out: Vec::new(),
        game: Vec::new(),
        closed: None,
        next_timeout: now,
        last_receive: now,
        client_key,
        ufrag: String::new(),
    };
    // The client offers `actpass` (or `active`): the server is the DTLS server then.
    let dtls_active = offer.setup == "passive";
    // str0m wants its output drained after every change.
    conn.rtc.direct_api().set_ice_controlling(false);
    conn.drain();
    conn.rtc.direct_api().set_remote_ice_credentials(IceCreds {
        ufrag: offer.ufrag.to_owned(),
        pass: offer.pwd.to_owned(),
    });
    conn.drain();
    conn.rtc.direct_api().set_remote_fingerprint(fingerprint);
    conn.drain();
    conn.rtc
        .direct_api()
        .start_dtls(dtls_active)
        .map_err(|e| (503, format!("DTLS: {e}")))?;
    conn.drain();
    conn.rtc.direct_api().start_sctp(dtls_active);
    conn.drain();
    if conn.rtc.add_local_candidate(candidate).is_none() {
        return Err(bad("unusable candidate address"));
    }
    conn.drain();
    let local_creds = conn.rtc.direct_api().local_ice_credentials();
    let local_fingerprint = conn.rtc.direct_api().local_dtls_fingerprint().to_string();
    let answer = answer_sdp(
        offer,
        &local_creds,
        &local_fingerprint,
        candidate_addr,
        dtls_active,
        key,
    );
    conn.ufrag = local_creds.ufrag;
    Ok((conn, answer))
}

fn answer_sdp(
    offer: &Offer<'_>,
    creds: &IceCreds,
    fingerprint: &str,
    addr: SocketAddr,
    dtls_active: bool,
    key: &p384::SecretKey,
) -> String {
    let (algorithm, digest) = fingerprint.split_once(' ').unwrap_or(("sha-256", ""));
    let identity = server_identity(key, algorithm, digest);
    let family = if addr.is_ipv4() { "IP4" } else { "IP6" };
    let ip = addr.ip();
    let port = addr.port();
    let session = u64::from_ne_bytes(vanillite_net::rand_bytes()) >> 2;
    let setup = if dtls_active { "active" } else { "passive" };
    let mid = offer.mid;
    format!(
        "v=0\r\n\
         o=- {session} 2 IN IP4 127.0.0.1\r\n\
         s=-\r\n\
         t=0 0\r\n\
         a=group:BUNDLE {mid}\r\n\
         a=extmap-allow-mixed\r\n\
         a=msid-semantic: WMS\r\n\
         a=identity:{identity}\r\n\
         a=ice-lite\r\n\
         m=application {port} UDP/DTLS/SCTP webrtc-datachannel\r\n\
         c=IN {family} {ip}\r\n\
         a=candidate:0 1 udp 2130706431 {ip} {port} typ host generation 0\r\n\
         a=ice-ufrag:{}\r\n\
         a=ice-pwd:{}\r\n\
         a=ice-options:trickle\r\n\
         a=fingerprint:{fingerprint}\r\n\
         a=setup:{setup}\r\n\
         a=mid:{mid}\r\n\
         a=sctp-port:5000\r\n\
         a=max-message-size:{MAX_MESSAGE}\r\n",
        creds.ufrag, creds.pass
    )
}

// ---- Identity -----------------------------------------------------------------------------

/// The canonical JSON the fingerprint signature covers.
fn fingerprint_payload(algorithm: &str, digest: &str) -> String {
    serde_json::json!({ "fingerprint": [{ "algorithm": algorithm, "digest": digest }] }).to_string()
}

fn jwk(key: &p384::PublicKey) -> Value {
    let sec1 = key.to_sec1_bytes();
    let (x, y) = sec1
        .get(1..)
        .unwrap_or_default()
        .split_at(sec1.len().saturating_sub(1) / 2);
    serde_json::json!({
        "kty": "EC",
        "crv": "P-384",
        "x": URL_SAFE_NO_PAD.encode(x),
        "y": URL_SAFE_NO_PAD.encode(y),
    })
}

fn sign(key: &p384::SecretKey, input: &str) -> String {
    let sig: p384::ecdsa::Signature = p384::ecdsa::SigningKey::from(key).sign(input.as_bytes());
    URL_SAFE_NO_PAD.encode(sig.to_bytes())
}

/// The `a=identity` value of the answer (base64 JSON).
fn server_identity(key: &p384::SecretKey, algorithm: &str, digest: &str) -> String {
    let public = key.public_key();
    let now = crate::login::now_secs();
    let header = serde_json::json!({ "alg": "ES384", "x5u": crate::login::encode_key(&public) });
    let claims = serde_json::json!({
        "cpk": jwk(&public),
        "exp": now.saturating_add(60),
        "iat": now,
    });
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let token = format!("{input}.{}", sign(key, &input));
    let protected = URL_SAFE_NO_PAD.encode(r#"{"alg":"ES384"}"#);
    let payload = URL_SAFE_NO_PAD.encode(fingerprint_payload(algorithm, digest));
    let fingerprints = format!(
        "{protected}..{}",
        sign(key, &format!("{protected}.{payload}"))
    );
    let assertion = serde_json::json!({ "fingerprints": fingerprints, "token": token }).to_string();
    let identity = serde_json::json!({
        "assertion": assertion,
        "idp": { "domain": "self", "protocol": "default" },
    });
    STANDARD.encode(identity.to_string())
}

/// Checks the client's `a=identity` against its DTLS fingerprint; returns its key.
fn verify_identity(attr: &str, fingerprint: &str) -> Result<p384::PublicKey, String> {
    let json = STANDARD
        .decode(attr.trim())
        .map_err(|_| "identity is not base64")?;
    let identity: Value = serde_json::from_slice(&json).map_err(|_| "identity is not JSON")?;
    let assertion: Value = serde_json::from_str(
        identity
            .get("assertion")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )
    .map_err(|_| "identity assertion is not JSON")?;
    let token = assertion
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let claims = token
        .split('.')
        .nth(1)
        .and_then(|c| URL_SAFE_NO_PAD.decode(c).ok())
        .and_then(|c| serde_json::from_slice::<Value>(&c).ok())
        .ok_or("identity token is malformed")?;
    let key = match claims.get("cpk") {
        Some(Value::String(der)) => crate::login::parse_key(der).map_err(|e| e.0)?,
        Some(Value::Object(k)) => {
            let coord = |n: &str| {
                k.get(n)
                    .and_then(Value::as_str)
                    .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
                    .unwrap_or_default()
            };
            let mut sec1 = vec![4u8];
            sec1.extend(coord("x"));
            sec1.extend(coord("y"));
            p384::PublicKey::from_sec1_bytes(&sec1).map_err(|_| "identity key is invalid")?
        }
        _ => return Err("identity token has no key".into()),
    };
    let detached = assertion
        .get("fingerprints")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut parts = detached.split('.');
    let (Some(protected), Some(""), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("fingerprint assertion is malformed".into());
    };
    let header: Value = URL_SAFE_NO_PAD
        .decode(protected)
        .ok()
        .and_then(|h| serde_json::from_slice(&h).ok())
        .unwrap_or_default();
    if header.get("alg").and_then(Value::as_str) != Some("ES384") {
        return Err("fingerprint assertion is not ES384".into());
    }
    let (algorithm, digest) = fingerprint.split_once(' ').unwrap_or_default();
    let input = format!(
        "{protected}.{}",
        URL_SAFE_NO_PAD.encode(fingerprint_payload(algorithm, digest))
    );
    let sig = URL_SAFE_NO_PAD
        .decode(sig)
        .ok()
        .and_then(|s| p384::ecdsa::Signature::from_slice(&s).ok())
        .ok_or("fingerprint signature is malformed")?;
    p384::ecdsa::VerifyingKey::from(&key)
        .verify(input.as_bytes(), &sig)
        .map_err(|_| "fingerprint signature does not match")?;
    Ok(key)
}

// ---- Connection ---------------------------------------------------------------------------

/// The transport of one NetherNet session (driven by its session task).
pub(crate) struct NetConn {
    rtc: Rtc,
    /// The candidate address (datagrams arrive there as far as ICE is concerned).
    destination: SocketAddr,
    reliable: Option<ChannelId>,
    /// The message being reassembled and the segments it still expects.
    incoming: Vec<u8>,
    remaining: Option<u8>,
    /// Segments not yet taken by SCTP.
    queue: std::collections::VecDeque<Vec<u8>>,
    queued: usize,
    segment: usize,
    max_message: usize,
    out: Vec<(SocketAddr, Vec<u8>)>,
    game: Vec<Vec<u8>>,
    closed: Option<String>,
    next_timeout: Instant,
    /// Last time anything arrived.
    pub last_receive: Instant,
    /// The key the client proved in its offer.
    pub client_key: Option<p384::PublicKey>,
    /// The local ICE username (the session's key in the hub).
    ufrag: String,
}

impl std::fmt::Debug for NetConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetConn")
            .field("destination", &self.destination)
            .finish_non_exhaustive()
    }
}

impl NetConn {
    /// Bytes waiting for SCTP.
    pub fn backlog(&self) -> usize {
        self.queued
    }

    /// Whether the peer takes so little data that it must be dropped.
    pub fn overloaded(&self, limit: usize) -> bool {
        self.queued > limit
    }

    /// Handles one datagram.
    pub fn receive(
        &mut self,
        from: SocketAddr,
        data: &[u8],
        now: Instant,
    ) -> Result<Received, String> {
        if let Ok(contents) = data.try_into() {
            let input = Input::Receive(
                now,
                Receive {
                    proto: Protocol::Udp,
                    source: from,
                    destination: self.destination,
                    contents,
                },
            );
            if self.rtc.accepts(&input) {
                self.rtc.handle_input(input).map_err(|e| e.to_string())?;
                self.last_receive = now;
                self.drain();
            }
        }
        // A client that rejects a packet sends packet_violation_warning or disconnect and closes
        // the channel at once: hand out what arrived before the close (the next tick reports it).
        let game = std::mem::take(&mut self.game);
        if game.is_empty()
            && let Some(why) = &self.closed
        {
            return Err(why.clone());
        }
        Ok(Received {
            game,
            ..Received::default()
        })
    }

    /// Queues a game batch (codec output, without `0xfe`).
    pub fn send_game(&mut self, payload: &[u8]) {
        let count = payload.len().div_ceil(self.segment).max(1);
        let Ok(mut remaining) = u8::try_from(count.saturating_sub(1)) else {
            self.closed = Some(format!("batch of {} bytes is too large", payload.len()));
            return;
        };
        for chunk in payload.chunks(self.segment) {
            let mut seg = Vec::with_capacity(chunk.len().saturating_add(1));
            seg.push(remaining);
            seg.extend_from_slice(chunk);
            self.queued = self.queued.saturating_add(seg.len());
            self.queue.push_back(seg);
            remaining = remaining.saturating_sub(1);
        }
    }

    /// Writes what SCTP takes, runs timers and sends the datagrams due.
    pub fn flush(&mut self, now: Instant, mut send: impl FnMut(SocketAddr, &[u8])) {
        if let Some(id) = self.reliable {
            while let Some(seg) = self.queue.front() {
                let len = seg.len();
                let accepted = match self.rtc.channel(id).map(|mut c| c.write(true, seg)) {
                    Some(Ok(accepted)) => accepted,
                    Some(Err(e)) => {
                        // A refused write would stall the queue for good: end the session.
                        self.closed = Some(format!("SCTP refused a {len} byte message: {e}"));
                        false
                    }
                    None => false,
                };
                self.drain();
                if !accepted {
                    break;
                }
                self.queued = self.queued.saturating_sub(len);
                self.queue.pop_front();
            }
        }
        if now >= self.next_timeout {
            if let Err(e) = self.rtc.handle_input(Input::Timeout(now)) {
                self.closed = Some(e.to_string());
            }
            self.drain();
        }
        for (to, d) in self.out.drain(..) {
            send(to, &d);
        }
    }

    /// The transport state for disconnect logs.
    pub fn describe(&mut self, now: Instant) -> String {
        let buffered = self
            .reliable
            .and_then(|id| self.rtc.channel(id).map(|mut c| c.buffered_amount()));
        format!(
            "NetherNet: last datagram {:.1}s ago, {} segments ({} bytes) waiting, SCTP buffered {}, channel {}",
            now.saturating_duration_since(self.last_receive)
                .as_secs_f32(),
            self.queue.len(),
            self.queued,
            buffered.map_or_else(|| "-".into(), |b| b.to_string()),
            if self.reliable.is_some() {
                "open"
            } else {
                "not open"
            },
        )
    }

    /// Why the connection ended, if it did.
    pub fn closed(&self) -> Option<&str> {
        self.closed.as_deref()
    }

    fn drain(&mut self) {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    self.next_timeout = t;
                    return;
                }
                Ok(Output::Transmit(t)) => self.out.push((t.destination, t.contents.into())),
                Ok(Output::Event(e)) => self.event(e),
                Err(e) => {
                    self.closed = Some(e.to_string());
                    return;
                }
            }
        }
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::ChannelOpen(id, label) if label == RELIABLE => self.reliable = Some(id),
            Event::ChannelOpen(_, label) if label != UNRELIABLE => {
                log::debug!("NetherNet channel {label:?} ignored");
            }
            Event::ChannelData(d) if Some(d.id) == self.reliable => {
                if let Err(e) = self.segment_in(&d.data) {
                    self.closed = Some(e);
                }
            }
            Event::ChannelData(d) => {
                // Unreliable messages are never segmented.
                if let Some((0, rest)) = d.data.split_first()
                    && !rest.is_empty()
                {
                    self.game.push(rest.to_vec());
                }
            }
            Event::ChannelClose(id) if Some(id) == self.reliable => {
                self.closed = Some("the client closed the connection".into());
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.closed = Some("the connection was lost".into());
            }
            _ => {}
        }
    }

    fn segment_in(&mut self, data: &[u8]) -> Result<(), String> {
        let Some((&left, rest)) = data.split_first().filter(|(_, r)| !r.is_empty()) else {
            return Err("empty message".into());
        };
        match self.remaining {
            Some(r) if r.checked_sub(1) != Some(left) => {
                return Err(format!("segment {left} after {r}"));
            }
            Some(_) => {}
            None => self.incoming.clear(),
        }
        if self.incoming.len().saturating_add(rest.len()) > self.max_message {
            return Err("message too large".into());
        }
        self.incoming.extend_from_slice(rest);
        if left == 0 {
            self.remaining = None;
            self.game.push(std::mem::take(&mut self.incoming));
        } else {
            self.remaining = Some(left);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trip() {
        let key = <p384::SecretKey as p384::elliptic_curve::Generate>::generate();
        let fp = "sha-256 AA:BB:CC";
        let attr = server_identity(&key, "sha-256", "AA:BB:CC");
        let got = verify_identity(&attr, fp).ok();
        assert_eq!(got, Some(key.public_key()));
        assert!(verify_identity(&attr, "sha-256 AA:BB:CD").is_err());
    }

    /// Drains a client's output: datagrams for the server, received messages.
    fn drain_client(client: &mut Rtc, out: &mut Vec<Vec<u8>>, got: &mut Vec<Vec<u8>>) -> Instant {
        loop {
            match client.poll_output().expect("client output") {
                Output::Timeout(t) => return t,
                Output::Transmit(t) => out.push(t.contents.into()),
                Output::Event(Event::ChannelData(d)) => got.push(d.data),
                Output::Event(_) => {}
            }
        }
    }

    /// A batch over 64 KiB crosses ICE, DTLS and SCTP in segments str0m accepts (#182: larger
    /// segments were refused and the session stalled before `start_game`).
    #[test]
    fn large_batch_reaches_a_webrtc_client() {
        let start = Instant::now();
        let client_addr: SocketAddr = "127.0.0.1:40000".parse().expect("addr");
        let server_addr: SocketAddr = "127.0.0.1:40001".parse().expect("addr");
        let mut client = Rtc::builder()
            .set_sctp_max_message_size(MAX_MESSAGE)
            .build(start);
        client.add_local_candidate(Candidate::host(client_addr, "udp").expect("candidate"));
        let mut api = client.sdp_api();
        let _ = api.add_channel(RELIABLE.into());
        let (offer, pending) = api.apply().expect("offer");
        let sdp = offer.to_sdp_string();
        let parsed = parse_offer(&sdp).expect("offer parses");
        let key = <p384::SecretKey as p384::elliptic_curve::Generate>::generate();
        let (mut conn, answer) =
            connect(&parsed, server_addr, None, 1 << 20, &key, start).expect("connect");
        let answer = str0m::change::SdpAnswer::from_sdp_string(&answer).expect("answer parses");
        client
            .sdp_api()
            .accept_answer(pending, answer)
            .expect("answer accepted");

        let payload: Vec<u8> = (0..200_000u32)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        let (mut now, mut sent, mut got) = (start, false, Vec::new());
        let mut segments: Vec<Vec<u8>> = Vec::new();
        let mut to_server = Vec::new();
        for _ in 0..4000 {
            let mut wake = drain_client(&mut client, &mut to_server, &mut got);
            for d in to_server.drain(..) {
                conn.receive(client_addr, &d, now).expect("server receive");
            }
            if conn.reliable.is_some() && !sent {
                conn.send_game(&payload);
                sent = true;
            }
            let mut to_client: Vec<Vec<u8>> = Vec::new();
            conn.flush(now, |_, d| to_client.push(d.to_vec()));
            assert_eq!(conn.closed(), None);
            for d in to_client {
                let contents = d.as_slice().try_into().expect("datagram");
                client
                    .handle_input(Input::Receive(
                        now,
                        Receive {
                            proto: Protocol::Udp,
                            source: server_addr,
                            destination: client_addr,
                            contents,
                        },
                    ))
                    .expect("client receive");
                wake = drain_client(&mut client, &mut to_server, &mut got);
            }
            segments.append(&mut got);
            if segments.first().is_some_and(|s| s.first() == Some(&0))
                || segments.last().is_some_and(|s| s.first() == Some(&0))
            {
                break;
            }
            now += Duration::from_millis(5);
            if now >= wake {
                client
                    .handle_input(Input::Timeout(now))
                    .expect("client timeout");
            }
        }
        assert!(sent, "the reliable channel never opened");
        assert!(segments.iter().all(|s| s.len() <= 64 << 10));
        let counts: Vec<u8> = segments.iter().filter_map(|s| s.first().copied()).collect();
        assert_eq!(counts, vec![3, 2, 1, 0]);
        let joined: Vec<u8> = segments
            .iter()
            .flat_map(|s| s.get(1..).unwrap_or_default().iter().copied())
            .collect();
        assert_eq!(joined, payload);
    }

    /// The packets a client sends right before it closes the channel (its packet_violation_warning
    /// when it rejects a packet) are still handed out; the close follows (#280: the complaint was
    /// dropped, so the log never named the rejected packet).
    #[test]
    fn packets_that_arrive_with_the_close_are_handed_out() {
        let start = Instant::now();
        let client_addr: SocketAddr = "127.0.0.1:40002".parse().expect("addr");
        let server_addr: SocketAddr = "127.0.0.1:40003".parse().expect("addr");
        let mut client = Rtc::builder().build(start);
        client.add_local_candidate(Candidate::host(client_addr, "udp").expect("candidate"));
        let mut api = client.sdp_api();
        let _ = api.add_channel(RELIABLE.into());
        let (offer, _pending) = api.apply().expect("offer");
        let sdp = offer.to_sdp_string();
        let parsed = parse_offer(&sdp).expect("offer parses");
        let key = <p384::SecretKey as p384::elliptic_curve::Generate>::generate();
        let (mut conn, _) =
            connect(&parsed, server_addr, None, 1 << 20, &key, start).expect("connect");
        conn.game.push(vec![156, 1, 2]);
        conn.closed = Some("the client closed the connection".into());
        let got = conn
            .receive(client_addr, &[], start)
            .expect("the last packets come first");
        assert_eq!(got.game, vec![vec![156, 1, 2]]);
        assert!(conn.receive(client_addr, &[], start).is_err());
        assert!(conn.closed().is_some());
    }

    #[test]
    fn stun_username() {
        let mut d = vec![0, 1, 0, 12, 0x21, 0x12, 0xa4, 0x42];
        d.extend([0u8; 12]);
        d.extend([0, 6, 0, 7]);
        d.extend(b"abc:xyz\0");
        assert_eq!(stun_ufrag(&d), Some("abc"));
    }
}
