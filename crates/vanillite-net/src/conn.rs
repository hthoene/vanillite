//! One connection: the state machine of handshake, status, login and configuration, then the
//! play read loop that validates and forwards packets to the world.

use crate::Shared;
use crate::auth::{AuthResult, is_valid_player_name, server_hash};
use crate::limits::{ConnectionGuard, RateLimiter};
use crate::outbound::{Out, Outbound};
use crate::player::{Injected, Joined, Latency, LoginAnswer, LoginCheck};
use crate::velocity;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::Instant;
use vanillite_protocol::Inbound;
use vanillite_protocol::frame::FrameDecoder;
use vanillite_protocol::packets::common::{ClientInfo, ResourcePackAction};
use vanillite_protocol::packets::handshake::Intent;
use vanillite_protocol::packets::{configuration as cfg, login, play, status};
use vanillite_protocol::sets::{
    ServerboundConfiguration, ServerboundHandshake, ServerboundLogin, ServerboundPlay,
    ServerboundStatus,
};
use vanillite_protocol::tap::{PacketTap, Peer, Verdict};
use vanillite_protocol::text::{Component, JsonComponent};
use vanillite_protocol::types::{GameProfile, Identifier, RawBytes, Uuid};
use vanillite_protocol::{Bucket, Direction, Error, State};

/// Why a connection ends.
#[derive(Debug)]
pub(crate) enum End {
    /// Malformed or unexpected input (SEC-05/07).
    Protocol(Error),
    /// The peer closed the connection, or the socket failed with this error.
    Closed(Option<std::io::ErrorKind>),
    /// A state deadline or the read timeout passed.
    Timeout,
    /// Disconnect with a message to the client.
    Kick(Box<Component>),
    /// The connection finished normally (status ping answered).
    Done,
}

impl From<Error> for End {
    fn from(e: Error) -> Self {
        Self::Protocol(e)
    }
}

type Result<T> = std::result::Result<T, End>;

enum Source {
    Tcp {
        read: tokio::net::tcp::OwnedReadHalf,
        decoder: Box<FrameDecoder>,
        buf: Box<[u8]>,
    },
    Virtual {
        rx: mpsc::Receiver<Vec<u8>>,
        max: usize,
    },
}

enum Read {
    Bytes,
    Frame(Vec<u8>),
    /// Closed by the peer, or the read error.
    Eof(Option<std::io::ErrorKind>),
}

impl Source {
    async fn read(&mut self) -> Read {
        match self {
            Self::Tcp { read, decoder, buf } => match read.read(buf).await {
                Ok(0) => Read::Eof(None),
                Err(e) => Read::Eof(Some(e.kind())),
                Ok(n) => {
                    decoder.push(buf.get(..n).unwrap_or_default());
                    Read::Bytes
                }
            },
            Self::Virtual { rx, .. } => rx.recv().await.map_or(Read::Eof(None), Read::Frame),
        }
    }

    fn set_state(&mut self, state: State) {
        match self {
            Self::Tcp { decoder, .. } => decoder.set_limits(state.limits()),
            Self::Virtual { max, .. } => *max = state.limits().max_uncompressed,
        }
    }
}

/// `rate-limit` (Vanilla's `RateKickingConnection`): packets of the current second and the
/// smoothed average of the last seconds.
struct PacketRate {
    second: Instant,
    received: u32,
    average: f32,
}

struct KeepAlive {
    enabled: bool,
    next: Instant,
    pending: Option<(i64, Instant)>,
    latency: Latency,
    /// In play until the game wrote its first packet: the `SentLog::written` count on
    /// entering play.
    login_mark: Option<usize>,
}

pub(crate) struct Conn {
    shared: Arc<Shared>,
    addr: SocketAddr,
    source: Source,
    out: Outbound,
    state: State,
    deadline: Option<Instant>,
    last_read: Instant,
    rate: RateLimiter,
    keepalive: KeepAlive,
    vouched: Option<GameProfile>,
    guard: Option<ConnectionGuard>,
    /// An in-process client that takes all chunks at once (Vanilla's `memoryConnection`).
    memory: bool,
    /// Translates the packets of a client of an older version (multi-version layer).
    adapter: Option<vanillite_multiversion::ServerboundHalf>,
    packet_rate: PacketRate,
    /// When the connection entered play, and the player's name.
    joined: Option<(Instant, String)>,
    /// This connection for the packet tap.
    peer: Peer,
    /// Packets plugins feed in as if the client sent them (play only).
    injected: Option<Arc<Injected>>,
    /// Protocol, brand and packet counts for `/vanillite debug`.
    details: Arc<crate::player::ClientDetails>,
}

/// The packet tap's verdict on a received body: one load and branch without listeners.
#[inline]
fn tap_in(tap: &PacketTap, peer: Peer, state: State, body: &[u8]) -> Verdict<Vec<u8>> {
    if tap.active(Direction::Serverbound) {
        tap.serverbound(peer, state, body)
    } else {
        Verdict::Pass
    }
}

/// Decodes a received body and returns it from `recv_packet`, unless a packet listener
/// cancelled it; a listener's replacement is decoded and validated like the client's packet.
macro_rules! deliver {
    ($conn:ident, $decode:ident, $body:expr) => {{
        let body: &[u8] = $body;
        match tap_in(&$conn.shared.config.tap, $conn.peer, $conn.state, body) {
            Verdict::Pass => return Ok($decode(body)?),
            Verdict::Replace(replaced) => return Ok($decode(&replaced)?),
            Verdict::Drop => {}
        }
    }};
}

/// Resolves when a plugin fed a packet into the connection; never without an injector.
async fn injected_ready(injected: Option<&Injected>) {
    match injected {
        Some(injected) => injected.ready.notified().await,
        None => std::future::pending().await,
    }
}

/// A connection the client drops this soon after joining logs the last packets it was sent:
/// the client most likely failed on one of them.
const EARLY_DROP: Duration = Duration::from_secs(10);
/// Packets named when a client closes the connection later (one log line at info).
const LAST_PACKETS_ON_CLOSE: usize = 24;

impl Conn {
    pub(crate) fn tcp(
        shared: Arc<Shared>,
        addr: SocketAddr,
        read: tokio::net::tcp::OwnedReadHalf,
        out: Outbound,
        guard: ConnectionGuard,
    ) -> Self {
        let source = Source::Tcp {
            read,
            decoder: Box::new(FrameDecoder::new()),
            buf: vec![0; 8192].into_boxed_slice(),
        };
        Self::new(shared, addr, source, out, None, Some(guard))
    }

    pub(crate) fn virtual_(
        shared: Arc<Shared>,
        addr: SocketAddr,
        rx: mpsc::Receiver<Vec<u8>>,
        out: Outbound,
        profile: GameProfile,
        memory: bool,
    ) -> Self {
        let source = Source::Virtual {
            rx,
            max: State::Handshake.limits().max_uncompressed,
        };
        let mut conn = Self::new(shared, addr, source, out, Some(profile), None);
        conn.memory = memory;
        conn
    }

    fn new(
        shared: Arc<Shared>,
        addr: SocketAddr,
        source: Source,
        out: Outbound,
        vouched: Option<GameProfile>,
        guard: Option<ConnectionGuard>,
    ) -> Self {
        let now = Instant::now();
        let rate = RateLimiter::new(shared.config.rate_limits);
        let connection = out.connection();
        let details = Arc::new(crate::player::ClientDetails::new(out.sent_log()));
        Self {
            deadline: now.checked_add(shared.config.status_timeout),
            shared,
            addr,
            source,
            out,
            state: State::Handshake,
            last_read: now,
            rate,
            keepalive: KeepAlive {
                enabled: false,
                next: now,
                pending: None,
                latency: Latency::default(),
                login_mark: None,
            },
            vouched,
            guard,
            memory: false,
            adapter: None,
            packet_rate: PacketRate {
                second: now,
                received: 0,
                average: 0.0,
            },
            joined: None,
            peer: Peer {
                connection,
                player: 0,
            },
            injected: None,
            details,
        }
    }

    /// Runs the connection to its end; one log line per abnormal end (SEC-08).
    pub(crate) async fn run(mut self) {
        let end = match self.drive().await {
            Ok(()) => End::Done,
            Err(e) => e,
        };
        self.report_drop(&end);
        match end {
            End::Done | End::Closed(_) => {}
            End::Kick(reason) => {
                let reason = *reason;
                log::debug!(
                    "{}: disconnected in {:?}: {}",
                    self.addr,
                    self.state,
                    reason.plain()
                );
                self.send_disconnect(&reason);
            }
            End::Timeout => {
                log::debug!("{}: timed out in {:?}", self.addr, self.state);
                self.send_disconnect(&Component::translatable("disconnect.timeout", vec![]));
            }
            End::Protocol(e) => {
                log::debug!("{}: protocol error in {:?}: {e}", self.addr, self.state);
                self.send_disconnect(&Component::translatable(
                    "disconnect.genericReason",
                    vec![Component::text("Invalid packet")],
                ));
            }
        }
        self.out.close();
    }

    /// Logs the last packets sent when the client dropped the connection soon after joining
    /// (warn) or the connection failed (debug). A Java client sends no reason when it gives up
    /// on a packet it cannot handle, so these are the best hint (#135).
    fn report_drop(&self, end: &End) {
        let why = match end {
            End::Done | End::Kick(_) => return,
            // The server closed the connection first (a kick from the world).
            End::Closed(None) if self.out.is_closed() => return,
            End::Closed(None) => "closed by the client".to_owned(),
            End::Closed(Some(kind)) => format!("connection error: {kind}"),
            End::Timeout => "timed out".to_owned(),
            End::Protocol(e) => format!("invalid packet: {e}"),
        };
        match &self.joined {
            Some((at, name)) if at.elapsed() < EARLY_DROP => log::warn!(
                "Suspicious disconnect: {name} ({}) dropped {:.1}s after joining: {why}\n  last packets sent: {}",
                self.addr,
                at.elapsed().as_secs_f32(),
                self.out.sent_log().dump()
            ),
            // A client that leaves closes the connection the same way as one that failed on
            // a packet ("Network Protocol Error"), so every close in play names the packets
            // the client got last; a failing one is among them (#243).
            Some((at, name)) if matches!(end, End::Closed(_)) => log::info!(
                "{name} ({}) dropped {:.0}s after joining: {why}; last packets sent: {}",
                self.addr,
                at.elapsed().as_secs_f32(),
                self.out.sent_log().dump_last(LAST_PACKETS_ON_CLOSE)
            ),
            _ if log::log_enabled!(log::Level::Debug) && !matches!(end, End::Closed(None)) => {
                log::debug!(
                    "{} ({:?}): {why}; last packets sent: {}",
                    self.addr,
                    self.state,
                    self.out.sent_log().dump()
                );
            }
            _ => {}
        }
    }

    fn send_disconnect(&self, reason: &Component) {
        match self.state {
            State::Handshake | State::Status => {}
            State::Login => {
                self.out.send(&login::LoginDisconnect {
                    reason: JsonComponent(reason.clone()),
                });
            }
            State::Configuration => {
                self.out.send(&cfg::clientbound::Disconnect {
                    reason: reason.clone(),
                });
            }
            State::Play => {
                self.out.send(&play::clientbound::Disconnect {
                    reason: reason.clone(),
                });
            }
        }
    }

    fn enter(&mut self, state: State, timeout: Option<Duration>) {
        self.state = state;
        self.out.control(Out::State(state));
        self.source.set_state(state);
        self.out.sent_log().set_state(state);
        self.deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        self.keepalive.enabled = matches!(state, State::Configuration | State::Play);
        // Like Vanilla's new packet listener per state: a keepalive of the previous state is
        // not waited for any more, and the first one goes out one interval after entering
        // (`ServerCommonPacketListenerImpl.keepAliveTime`). The client answers a keepalive on
        // its network thread; one sent at once can arrive before the client switched its
        // outbound protocol, and the client then disconnects with "Pipeline has no outbound
        // protocol configured" (#135).
        self.keepalive.pending = None;
        // Vanilla sends `login` right when the play listener is created; here the game sends
        // it once it placed the player, which can take longer than an interval under load. A
        // keepalive before `login` breaks the client, so the play keepalive starts after the
        // game wrote its first packet.
        self.keepalive.login_mark = (state == State::Play).then(|| self.out.sent_log().written());
        let now = Instant::now();
        self.keepalive.next = now
            .checked_add(self.shared.config.keepalive_interval)
            .unwrap_or(now);
    }

    /// Receives and decodes the next packet and counts it for `rate-limit`.
    async fn recv<T>(&mut self, decode: fn(&[u8]) -> vanillite_protocol::Result<T>) -> Result<T> {
        let packet = self.recv_packet(decode).await?;
        self.details.count_received();
        self.count_packet()?;
        Ok(packet)
    }

    /// `Connection.tickSecond` of `RateKickingConnection`: once per second the average is
    /// `lerp(0.75, received, average)`; above `rate-limit` the connection is kicked.
    fn count_packet(&mut self) -> Result<()> {
        let limit = self.shared.config.rate_limit;
        if limit == 0 {
            return Ok(());
        }
        let r = &mut self.packet_rate;
        let elapsed = Instant::now().saturating_duration_since(r.second).as_secs();
        if elapsed > 0 {
            #[allow(clippy::as_conversions)] // a packet count, exact up to 2^24
            let received = r.received as f32;
            r.average += 0.75 * (received - r.average);
            // Seconds without packets.
            for _ in 1..elapsed.min(16) {
                r.average *= 0.25;
            }
            r.received = 0;
            r.second = r
                .second
                .checked_add(Duration::from_secs(elapsed))
                .unwrap_or(r.second);
            #[allow(clippy::as_conversions)] // at most `i32::MAX`
            if r.average > limit as f32 {
                log::warn!(
                    "Player exceeded rate-limit (sent {} packets per second)",
                    r.average
                );
                return Err(kick(Component::translatable(
                    "disconnect.exceeded_packet_rate",
                    vec![],
                )));
            }
        }
        r.received = r.received.saturating_add(1);
        Ok(())
    }

    /// A packet listener cancelled a received packet: it still counts against the rate
    /// limits (SEC-16), so cancelling cannot be used to flood the server.
    fn cancelled(&mut self) -> Result<()> {
        self.count_packet()?;
        self.rate((Bucket::Default, 1))
    }

    /// Receives and decodes the next packet, handling timeouts and keepalives while waiting.
    async fn recv_packet<T>(
        &mut self,
        decode: fn(&[u8]) -> vanillite_protocol::Result<T>,
    ) -> Result<T> {
        loop {
            // Packets a plugin fed in: validated and counted like the client's own.
            if let Some((body, skip_listeners)) = self.injected.as_ref().and_then(|i| i.take()) {
                if skip_listeners {
                    return Ok(decode(&body)?);
                }
                deliver!(self, decode, &body);
                self.cancelled()?;
                continue;
            }
            // Packets the multi-version layer added to the last one come first.
            if let Some(t) = self.adapter.as_mut().and_then(|a| a.pending()) {
                deliver!(self, decode, t);
                self.cancelled()?;
                continue;
            }
            if let Source::Tcp { decoder, .. } = &mut self.source
                && let Some(frame) = decoder.next_frame()?
            {
                match &mut self.adapter {
                    None => deliver!(self, decode, frame),
                    Some(a) => match a.translate(frame)? {
                        Some(t) => deliver!(self, decode, t),
                        None => continue,
                    },
                }
                self.cancelled()?;
                continue;
            }
            let read_deadline = self.last_read.checked_add(self.shared.config.read_timeout);
            let mut wake = [self.deadline, read_deadline].into_iter().flatten().min();
            if self.keepalive.enabled {
                let ka = match self.keepalive.pending {
                    Some((_, sent)) => sent.checked_add(self.shared.config.keepalive_interval),
                    None => Some(self.keepalive.next),
                };
                wake = [wake, ka].into_iter().flatten().min();
            }
            let wake = wake.unwrap_or_else(Instant::now);
            tokio::select! {
                r = self.source.read() => {
                    self.last_read = Instant::now();
                    match r {
                        Read::Bytes => {}
                        Read::Eof(e) => return Err(End::Closed(e)),
                        Read::Frame(f) => {
                            let max = match &self.source { Source::Virtual { max, .. } => *max, Source::Tcp { .. } => 0 };
                            if f.len() > max {
                                return Err(Error::TooLong { what: "frame", len: f.len(), max }.into());
                            }
                            match &mut self.adapter {
                                None => deliver!(self, decode, &f),
                                Some(a) => match a.translate(&f)? {
                                    Some(t) => deliver!(self, decode, t),
                                    None => continue,
                                },
                            }
                            self.cancelled()?;
                        }
                    }
                }
                () = tokio::time::sleep_until(wake) => self.tick()?,
                () = injected_ready(self.injected.as_deref()) => {}
            }
        }
    }

    fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        if self.deadline.is_some_and(|d| now >= d)
            || self
                .last_read
                .checked_add(self.shared.config.read_timeout)
                .is_some_and(|d| now >= d)
        {
            return Err(End::Timeout);
        }
        if !self.keepalive.enabled {
            return Ok(());
        }
        if let Some(mark) = self.keepalive.login_mark {
            if self.out.sent_log().written() == mark {
                if now >= self.keepalive.next {
                    self.keepalive.next = now
                        .checked_add(self.shared.config.keepalive_interval)
                        .unwrap_or(now);
                }
                return Ok(());
            }
            self.keepalive.login_mark = None;
        }
        match self.keepalive.pending {
            Some((_, sent))
                if now.saturating_duration_since(sent) >= self.shared.config.keepalive_interval =>
            {
                Err(End::Timeout)
            }
            Some(_) => Ok(()),
            None if now >= self.keepalive.next => {
                let id = i64::try_from(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                )
                .unwrap_or_default();
                self.keepalive.pending = Some((id, now));
                self.keepalive.next = now
                    .checked_add(self.shared.config.keepalive_interval)
                    .unwrap_or(now);
                if self.state == State::Play {
                    self.out.send(&play::clientbound::KeepAlive { id });
                } else {
                    self.out.send(&cfg::clientbound::KeepAlive { id });
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn keepalive_answer(&mut self, id: i64) -> Result<()> {
        match self.keepalive.pending {
            Some((expected, sent)) if expected == id => {
                let rtt = u32::try_from(Instant::now().saturating_duration_since(sent).as_millis())
                    .unwrap_or(u32::MAX);
                let old = self.keepalive.latency.0.load(Ordering::Relaxed);
                let smoothed = (u64::from(old)
                    .saturating_mul(3)
                    .saturating_add(u64::from(rtt)))
                    / 4;
                self.keepalive.latency.0.store(
                    u32::try_from(smoothed).unwrap_or(u32::MAX),
                    Ordering::Relaxed,
                );
                self.keepalive.pending = None;
                Ok(())
            }
            _ => Err(End::Timeout),
        }
    }

    fn rate(&mut self, cost: (vanillite_protocol::Bucket, u16)) -> Result<()> {
        if self.rate.take(cost.0, cost.1) {
            Ok(())
        } else {
            Err(kick(Component::translatable(
                "disconnect.exceeded_packet_rate",
                vec![],
            )))
        }
    }

    async fn drive(&mut self) -> Result<()> {
        let ServerboundHandshake::Intention(hs) = self.recv(ServerboundHandshake::decode).await?;
        match hs.intent {
            // `repliesToStatus`: without it the connection is closed at once.
            Intent::Status if !self.shared.config.enable_status => Err(End::Done),
            Intent::Status => self.status(hs.protocol_version).await,
            Intent::Login | Intent::Transfer => {
                self.enter(State::Login, Some(self.shared.config.login_timeout));
                self.details.set_protocol(hs.protocol_version);
                if hs.protocol_version != vanillite_protocol::PROTOCOL_VERSION
                    && let Some(adapter) = self
                        .shared
                        .config
                        .multiversion
                        .adapter(hs.protocol_version, State::Login)
                {
                    let (serverbound, clientbound) = adapter.split();
                    self.adapter = Some(serverbound);
                    self.out.control(Out::Adapter(Box::new(clientbound)));
                } else if hs.protocol_version != vanillite_protocol::PROTOCOL_VERSION {
                    let key = if hs.protocol_version < 754 {
                        "multiplayer.disconnect.outdated_client"
                    } else {
                        "multiplayer.disconnect.incompatible"
                    };
                    return Err(kick(Component::translatable(
                        key,
                        vec![Component::text(vanillite_protocol::VERSION_ID)],
                    )));
                }
                if hs.intent == Intent::Transfer && !self.shared.config.accepts_transfers {
                    return Err(kick(Component::translatable(
                        "multiplayer.disconnect.transfers_disabled",
                        vec![],
                    )));
                }
                let (profile, overridden) = self.login().await?;
                self.peer.player = profile.id.0;
                self.out.control(Out::Player(profile.id.0));
                let client_info = self.configure().await?;
                let transferred = hs.intent == Intent::Transfer;
                self.play(profile, client_info, overridden, transferred)
                    .await
            }
        }
    }

    async fn status(&mut self, protocol: i32) -> Result<()> {
        self.enter(State::Status, Some(self.shared.config.status_timeout));
        let mut answered = false;
        loop {
            match self.recv(ServerboundStatus::decode).await? {
                ServerboundStatus::StatusRequest(_) if !answered => {
                    answered = true;
                    let json = self.shared.status.get();
                    // Clients of an accepted older version see the server as compatible.
                    let json = if protocol != vanillite_protocol::PROTOCOL_VERSION
                        && self.shared.config.multiversion.version(protocol).is_some()
                    {
                        vanillite_multiversion::status_json(&json, protocol)
                    } else {
                        json.to_string()
                    };
                    self.out.send(&status::StatusResponse { json });
                }
                ServerboundStatus::PingRequest(p) => {
                    self.out.send(&status::PongResponse { time: p.time });
                    return Err(End::Done);
                }
                ServerboundStatus::StatusRequest(_) => {
                    return Err(Error::Invalid("second status request").into());
                }
            }
        }
    }

    /// The login state; the profile and whether the server admitted it against its rules.
    async fn login(&mut self) -> Result<(GameProfile, bool)> {
        let hello = match self.recv(ServerboundLogin::decode).await? {
            ServerboundLogin::Hello(hello) => hello,
            other => return Err(unexpected_login(&other, "expected login hello")),
        };
        // Cheap checks before any RSA or network work (SEC-18).
        if !is_valid_player_name(&hello.name) {
            return Err(kick(Component::translatable(
                "multiplayer.disconnect.invalid_player_data",
                vec![],
            )));
        }
        let shared = Arc::clone(&self.shared);
        let mut threshold = self.shared.config.compression_threshold;
        let profile = if let Some(profile) = self.vouched.clone() {
            profile
        } else if let Some((profile, link)) = self.forwarded(&hello.name).await? {
            // A trusted Vanillite proxy relays frames unchanged if they are compressed for its
            // client: use its threshold.
            if let Some(t) = link {
                threshold = t;
            }
            profile
        } else if let Some(keys) = shared.keys.as_ref() {
            let challenge: [u8; 4] = rand_bytes();
            self.out.send(&login::EncryptionRequest {
                server_id: String::new(),
                public_key: RawBytes(keys.public_der().to_vec()),
                challenge: RawBytes(challenge.to_vec()),
                should_authenticate: true,
            });
            // The client asks the session server now; open our connection to it meanwhile.
            let auth = Arc::clone(&self.shared.authenticator);
            drop(tokio::task::spawn_blocking(move || auth.prepare()));
            let key = match self.recv(ServerboundLogin::decode).await? {
                ServerboundLogin::Key(key) => key,
                other => return Err(unexpected_login(&other, "expected key")),
            };
            let secret: [u8; 16] = keys
                .decrypt(&key.shared_secret.0)
                .and_then(|s| <[u8; 16]>::try_from(s.as_slice()).ok())
                .ok_or(Error::Invalid("shared secret"))?;
            if keys.decrypt(&key.challenge.0).as_deref() != Some(&challenge[..]) {
                return Err(Error::Invalid("challenge").into());
            }
            if let Source::Tcp { decoder, .. } = &mut self.source {
                decoder.enable_encryption(&secret);
            }
            self.out.control(Out::Encryption(secret));
            let hash = server_hash("", &secret, keys.public_der());
            self.authenticate(hello.name, hash).await?
        } else {
            log::debug!("{}: offline-mode login of {}", self.addr, hello.name);
            GameProfile {
                id: Uuid::offline(&hello.name),
                name: hello.name,
                properties: Vec::new(),
            }
        };

        let overridden = self.verify_login(&profile).await?;
        if threshold >= 0 && self.vouched.is_none() {
            self.out.send(&login::LoginCompression { threshold });
            self.out.control(Out::Compression(threshold));
            if let Source::Tcp { decoder, .. } = &mut self.source {
                decoder.set_compression(threshold);
            }
        }
        self.out.send(&login::LoginFinished {
            profile: profile.clone(),
            session_id: self.shared.session_id,
        });
        // Answers to requests we never send are protocol errors, like in Vanilla.
        match self.recv(ServerboundLogin::decode).await? {
            ServerboundLogin::LoginAcknowledged(_) => {}
            other => return Err(unexpected_login(&other, "unexpected login packet")),
        }
        if self.shared.config.log_ips {
            log::info!(
                "{} ({}) logged in from {}",
                profile.name,
                profile.id,
                self.addr
            );
        } else {
            log::info!(
                "{} ({}) logged in from <ip address withheld>",
                profile.name,
                profile.id
            );
        }
        Ok((profile, overridden))
    }

    /// `verifyLoginAndFinishConnectionSetup`: the server thread admits or refuses the player
    /// (`canPlayerLogin`, plugins) before `LoginFinished`, so a refusal is a login disconnect.
    async fn verify_login(&mut self, profile: &GameProfile) -> Result<bool> {
        let (answer, verdict) = tokio::sync::oneshot::channel();
        let check = LoginCheck {
            profile: profile.clone(),
            address: self.addr,
            is_virtual: self.vouched.is_some(),
            memory: self.memory,
            connection: Arc::downgrade(&self.details),
            answer,
        };
        if self.shared.logins.try_send(check).is_err() {
            return Err(kick(Component::translatable(
                "multiplayer.disconnect.server_full",
                vec![],
            )));
        }
        self.shared.wake_server();
        let verdict = match self.deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, verdict)
                .await
                .map_err(|_| End::Timeout)?,
            None => verdict.await,
        };
        match verdict {
            Ok(LoginAnswer::Admit { overridden }) => Ok(overridden),
            Ok(LoginAnswer::Refuse(reason)) => Err(End::Kick(reason)),
            // The server dropped the check (it is stopping).
            Err(_) => Err(kick(Component::translatable(
                "multiplayer.disconnect.generic",
                vec![],
            ))),
        }
    }

    /// Velocity modern forwarding: asks the proxy for the player's identity, and offers the
    /// trusted link in the same round trip. `None` when forwarding is off, or for a direct
    /// connection where those are allowed; otherwise the profile and, if the proxy accepted
    /// the link, its compression threshold.
    async fn forwarded(&mut self, name: &str) -> Result<Option<(GameProfile, Option<i32>)>> {
        let Some(secret) = self.shared.config.forwarding_secret.clone() else {
            return Ok(None);
        };
        let transaction_id = i32::from_be_bytes(rand_bytes()) & (i32::MAX >> 1);
        let link_id = transaction_id.wrapping_add(1);
        let link_request = velocity::link_request(rand_bytes());
        self.out.send(&login::CustomQuery {
            transaction_id,
            channel: Identifier::new(velocity::CHANNEL)?,
            payload: RawBytes(vec![velocity::MAX_VERSION]),
        });
        self.out.send(&login::CustomQuery {
            transaction_id: link_id,
            channel: Identifier::new(velocity::LINK_CHANNEL)?,
            payload: RawBytes(link_request.clone()),
        });
        let mut answers = [None, None];
        for (slot, id) in answers.iter_mut().zip([transaction_id, link_id]) {
            let answer = match self.recv(ServerboundLogin::decode).await? {
                ServerboundLogin::CustomQueryAnswer(answer) => answer,
                other => return Err(unexpected_login(&other, "expected forwarding answer")),
            };
            if answer.transaction_id != id {
                return Err(Error::Invalid("forwarding transaction").into());
            }
            *slot = answer.payload;
        }
        let [forwarding, link] = answers;
        let Some(payload) = forwarding else {
            // A Vanilla client does not understand the request.
            return if self.shared.config.allow_direct_connections {
                Ok(None)
            } else {
                Err(kick(Component::text(
                    "This server only accepts connections through its proxy.",
                )))
            };
        };
        match velocity::decode(&secret, &payload.0) {
            Ok(f) if f.profile.name == name && is_valid_player_name(&f.profile.name) => {
                self.addr = SocketAddr::new(f.address, self.addr.port());
                let link = link.and_then(|a| velocity::link_verify(&secret, &link_request, &a.0));
                Ok(Some((f.profile, link)))
            }
            Ok(_) | Err(_) => {
                log::warn!("{}: invalid forwarding data", self.addr);
                Err(kick(Component::text("Unable to verify player details.")))
            }
        }
    }

    async fn authenticate(&mut self, name: String, hash: String) -> Result<GameProfile> {
        let _permit = self
            .shared
            .auth_permits
            .acquire()
            .await
            .map_err(|_| End::Closed(None))?;
        let auth = Arc::clone(&self.shared.authenticator);
        let ip = self
            .shared
            .config
            .prevent_proxy_connections
            .then(|| self.addr.ip());
        let task = tokio::task::spawn_blocking(move || {
            let r = auth.authenticate(&name, &hash, ip);
            (name, r)
        });
        let (name, result) = task.await.map_err(|_| End::Closed(None))?;
        match result {
            AuthResult::Verified(p) if p.name.eq_ignore_ascii_case(&name) => Ok(p),
            AuthResult::Verified(_) | AuthResult::Unverified => Err(kick(Component::translatable(
                "multiplayer.disconnect.unverified_username",
                vec![],
            ))),
            AuthResult::Unavailable => Err(kick(Component::translatable(
                "multiplayer.disconnect.authservers_down",
                vec![],
            ))),
        }
    }

    async fn configure(&mut self) -> Result<ClientInfo> {
        self.enter(
            State::Configuration,
            Some(self.shared.config.configuration_timeout),
        );
        let data = self.shared.configuration();
        let mut brand = Vec::new();
        vanillite_protocol::codec::write_str(&mut brand, &self.shared.config.brand);
        self.out.send(&cfg::clientbound::CustomPayload {
            channel: Identifier::vanilla("brand"),
            data: RawBytes(brand),
        });
        let config = &self.shared.config;
        if !config.server_links.is_empty() {
            self.out.send(&cfg::clientbound::ServerLinks {
                links: config.server_links.clone(),
            });
        }
        self.out.send_shared(&data.features);
        self.out.send_shared(&data.select_known_packs);
        let mut info = ClientInfo::default();
        // `ServerConfigurationPacketListenerImpl`'s task queue after the registries.
        let mut task = Task::KnownPacks;
        loop {
            let packet = self.recv(ServerboundConfiguration::decode).await?;
            self.rate(packet.cost())?;
            match packet {
                ServerboundConfiguration::ClientInformation(c) => info = c.0,
                ServerboundConfiguration::SelectKnownPacks(p) if task == Task::KnownPacks => {
                    let registries = if p.packs == data.offered_packs {
                        &data.registries_known
                    } else {
                        &data.registries_full
                    };
                    for r in registries {
                        self.out.send_shared(r);
                    }
                    self.out.send_shared(&data.tags);
                    task = self.next_task(Task::KnownPacks, &info);
                }
                ServerboundConfiguration::AcceptCodeOfConduct(_) if task == Task::CodeOfConduct => {
                    task = self.next_task(Task::CodeOfConduct, &info);
                }
                ServerboundConfiguration::ResourcePack(p) => {
                    self.resource_pack_answer(p.action)?;
                    if task == Task::ResourcePack && is_terminal(p.action) {
                        task = self.next_task(Task::ResourcePack, &info);
                    }
                }
                ServerboundConfiguration::FinishConfiguration(_) if task == Task::Join => {
                    return Ok(info);
                }
                ServerboundConfiguration::KeepAlive(k) => self.keepalive_answer(k.id)?,
                ServerboundConfiguration::CustomPayload(p) => {
                    if p.channel.as_str() == "minecraft:brand"
                        && let Ok(brand) =
                            vanillite_protocol::codec::Reader::new(&p.data.0).string(128)
                    {
                        self.details.set_brand(brand);
                    }
                }
                // `handleCookieResponse`: the server never asks for cookies.
                ServerboundConfiguration::CookieResponse(_) => return Err(unexpected_query()),
                ServerboundConfiguration::Pong(_)
                | ServerboundConfiguration::CustomClickAction(_) => {}
                ServerboundConfiguration::SelectKnownPacks(_)
                | ServerboundConfiguration::AcceptCodeOfConduct(_)
                | ServerboundConfiguration::FinishConfiguration(_) => {
                    return Err(Error::Invalid("unexpected configuration packet").into());
                }
            }
        }
    }

    /// Starts the configuration task after `done` and returns it: the code of conduct, the
    /// resource pack, then joining (`finish_configuration`).
    fn next_task(&mut self, done: Task, info: &ClientInfo) -> Task {
        let config = &self.shared.config;
        // Clients of the multi-version layer older than 1.21.9 cannot show (or accept) it.
        let shows_conduct = self
            .adapter
            .as_ref()
            .is_none_or(|a| a.version().has_code_of_conduct());
        if done < Task::CodeOfConduct && !config.code_of_conduct.is_empty() && shows_conduct {
            let text = config
                .code_of_conduct
                .get(&info.language.to_lowercase())
                .or_else(|| config.code_of_conduct.get("en_us"))
                .or_else(|| config.code_of_conduct.values().next())
                .cloned()
                .unwrap_or_default();
            self.out.send(&cfg::clientbound::CodeOfConduct { text });
            return Task::CodeOfConduct;
        }
        if done < Task::ResourcePack
            && let Some(pack) = &config.resource_pack
        {
            self.out.send(&cfg::clientbound::ResourcePackPush {
                id: pack.id,
                url: pack.url.clone(),
                hash: pack.hash.clone(),
                required: pack.required,
                prompt: pack.prompt.clone(),
            });
            return Task::ResourcePack;
        }
        self.out.send(&cfg::clientbound::FinishConfiguration);
        // The client is in play after `finish_configuration`: a configuration keepalive sent
        // now would reach it as an unknown play packet.
        self.keepalive.enabled = false;
        Task::Join
    }

    /// `handleResourcePackResponse`: declining a required pack kicks.
    fn resource_pack_answer(&self, action: ResourcePackAction) -> Result<()> {
        if action == ResourcePackAction::Declined
            && self
                .shared
                .config
                .resource_pack
                .as_ref()
                .is_some_and(|p| p.required)
        {
            return Err(kick(Component::translatable(
                "multiplayer.requiredTexturePrompt.disconnect",
                vec![],
            )));
        }
        Ok(())
    }

    async fn play(
        &mut self,
        profile: GameProfile,
        client_info: ClientInfo,
        login_overridden: bool,
        transferred: bool,
    ) -> Result<()> {
        self.enter(State::Play, None);
        self.joined = Some((Instant::now(), profile.name.clone()));
        let (tx, rx) = vanillite_protocol::inbound_queue(self.shared.config.max_inbound_packets);
        let injected = Injected::new(self.shared.config.max_inbound_packets);
        let joined = Joined {
            profile,
            client_info,
            address: self.addr,
            is_virtual: self.vouched.is_some(),
            memory: self.memory,
            inbound: rx,
            outbound: self.out.clone(),
            latency: self.keepalive.latency.clone(),
            details: Arc::clone(&self.details),
            injector: injected.injector(),
            login_overridden,
            transferred,
        };
        self.injected = Some(injected);
        if self.shared.joins.try_send(joined).is_err() {
            return Err(kick(Component::translatable(
                "multiplayer.disconnect.server_full",
                vec![],
            )));
        }
        self.shared.wake_server();
        if let Some(g) = self.guard.as_mut() {
            g.joined();
        }
        let mut seq: u64 = 0;
        loop {
            let packet = self.recv(ServerboundPlay::decode).await?;
            if self.out.is_closed() {
                return Err(End::Closed(None));
            }
            self.rate(packet.cost())?;
            match packet {
                ServerboundPlay::KeepAlive(k) => self.keepalive_answer(k.id)?,
                // `handlePingRequest` answers on the network thread, like Vanilla.
                ServerboundPlay::PingRequest(p) => {
                    self.out
                        .send(&play::clientbound::PongResponse { time: p.time });
                }
                ServerboundPlay::Unhandled { .. } => {}
                packet => {
                    // Resource pack statuses are answered here; the world still gets them
                    // for the plugins' resource pack events.
                    if let ServerboundPlay::ResourcePack(p) = &packet {
                        self.resource_pack_answer(p.action)?;
                    }
                    seq = seq.wrapping_add(1);
                    if !tx.try_send(Inbound { seq, packet }) {
                        // The world is not draining this connection fast enough (SEC-17).
                        return Err(kick(Component::translatable(
                            "disconnect.exceeded_packet_rate",
                            vec![],
                        )));
                    }
                }
            }
        }
    }
}

/// The configuration task waited for, in Vanilla's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Task {
    /// `SynchronizeRegistriesTask`: the answer to `select_known_packs`.
    KnownPacks,
    /// `ServerCodeOfConductConfigurationTask`.
    CodeOfConduct,
    /// `ServerResourcePackConfigurationTask`.
    ResourcePack,
    /// `JoinWorldTask`: the client's `finish_configuration`.
    Join,
}

/// `ServerboundResourcePackPacket.Action.isTerminal`.
fn is_terminal(action: ResourcePackAction) -> bool {
    !matches!(
        action,
        ResourcePackAction::Accepted | ResourcePackAction::Downloaded
    )
}

fn kick(reason: Component) -> End {
    End::Kick(Box::new(reason))
}

/// `DISCONNECT_UNEXPECTED_QUERY`: the answer to a cookie request the server never sent.
pub fn unexpected_query() -> End {
    kick(Component::translatable(
        "multiplayer.disconnect.unexpected_query_response",
        vec![],
    ))
}

/// The end for login packet `packet` arriving where another was expected: a cookie answer
/// kicks like Vanilla's `handleCookieResponse`, anything else is a protocol error.
fn unexpected_login(packet: &ServerboundLogin, what: &'static str) -> End {
    if matches!(packet, ServerboundLogin::CookieResponse(_)) {
        unexpected_query()
    } else {
        Error::Invalid(what).into()
    }
}

/// Random bytes from the OS.
pub fn rand_bytes<const N: usize>() -> [u8; N] {
    use rsa::rand_core::Rng;
    let mut b = [0u8; N];
    rsa::rand_core::UnwrapErr(getrandom::SysRng).fill_bytes(&mut b);
    b
}
