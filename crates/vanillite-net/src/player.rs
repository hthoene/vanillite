//! What the server receives from a connection: the login to decide on, then the player that
//! finished configuration and enters play.

use crate::outbound::Outbound;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use tokio::sync::{Notify, oneshot};

use vanillite_protocol::packets::common::ClientInfo;
use vanillite_protocol::text::Component;
use vanillite_protocol::types::GameProfile;

/// A login waiting for the server's decision before `LoginFinished` (Vanilla's
/// `verifyLoginAndFinishConnectionSetup` → `PlayerList.canPlayerLogin` on the server thread):
/// bans, whitelist, player limit, plugins. The connection waits until [`LoginCheck::admit`] or
/// [`LoginCheck::refuse`], at most until its login timeout; dropping the check unanswered
/// refuses the login.
#[derive(Debug)]
pub struct LoginCheck {
    /// Authenticated (or vouched-for) profile.
    pub profile: GameProfile,
    /// Remote address.
    pub address: SocketAddr,
    /// Whether this is a virtual connection (in-memory, Bedrock, proxy edge).
    pub is_virtual: bool,
    /// An in-process client (Vanilla's `memoryConnection`).
    pub memory: bool,
    /// Alive as long as the connection is: what the server prepares for the login (the spawn
    /// chunks) can end with it.
    pub connection: std::sync::Weak<ClientDetails>,
    pub(crate) answer: oneshot::Sender<LoginAnswer>,
}

/// The server's answer to a [`LoginCheck`].
#[derive(Debug)]
pub(crate) enum LoginAnswer {
    /// Logged in; `overridden` when the server let the player in against its own rules.
    Admit { overridden: bool },
    /// Refused with this reason, sent as `LoginDisconnect`.
    Refuse(Box<Component>),
}

impl LoginCheck {
    /// Lets the player log in. `overridden`: the server's rules refused it and something else
    /// (a plugin) let it in anyway; it comes back as [`Joined::login_overridden`].
    pub fn admit(self, overridden: bool) {
        let _ = self.answer.send(LoginAnswer::Admit { overridden });
    }

    /// Refuses the login with `reason` (the client's login disconnect screen).
    pub fn refuse(self, reason: Component) {
        let _ = self.answer.send(LoginAnswer::Refuse(Box::new(reason)));
    }
}

/// A player connection handed to the server. Dropping `inbound`'s sender (the connection
/// ending) shows up as a disconnected channel.
#[derive(Debug)]
pub struct Joined {
    /// Authenticated (or vouched-for) profile.
    pub profile: GameProfile,
    /// Settings the client sent during configuration.
    pub client_info: ClientInfo,
    /// Remote address.
    pub address: SocketAddr,
    /// Whether this is a virtual connection (in-memory, Bedrock, proxy edge).
    pub is_virtual: bool,
    /// An in-process client that takes all pending chunks at once (Vanilla's
    /// `memoryConnection`). Virtual connections of remote clients (Bedrock) are not.
    pub memory: bool,
    /// Validated play packets, bounded (SEC-17); drained by the world between ticks.
    pub inbound: vanillite_protocol::InboundReceiver,
    /// Packets to the client.
    pub outbound: Outbound,
    /// Smoothed keepalive round trip.
    pub latency: Latency,
    /// Protocol version, brand and packet counts of the client.
    pub details: Arc<ClientDetails>,
    /// Feeds packets into the connection as if the client sent them.
    pub injector: Injector,
    /// The server's login check admitted the player against its own rules
    /// ([`LoginCheck::admit`]).
    pub login_overridden: bool,
    /// The handshake's intent was a transfer from another server (`cookie.transferred()`).
    pub transferred: bool,
}

/// Feeds play packets into a connection as if its client sent them (a plugin's
/// `packet_receive`, ProtocolLib's `receiveClientPacket`). The connection's read loop takes
/// them between the client's packets: they pass the packet listeners (unless skipped), the
/// validating decoder and the rate limits like the client's own, and reach the world through
/// the one inbound queue, so the world still sees the connection end when the queue closes.
/// Cheap to clone; holds the connection only weakly.
#[derive(Debug, Clone, Default)]
pub struct Injector(Weak<Injected>);

/// The packets fed into one connection, waiting for its read loop.
#[derive(Debug)]
pub(crate) struct Injected {
    /// Bodies (`id` + fields, the server's protocol version) and whether they skip the
    /// packet listeners.
    queue: Mutex<VecDeque<(Vec<u8>, bool)>>,
    /// Wakes the read loop.
    pub(crate) ready: Notify,
    /// At most this many wait (SEC-17).
    max: usize,
}

impl Injected {
    pub(crate) fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            ready: Notify::new(),
            max: max.max(1),
        })
    }

    pub(crate) fn injector(self: &Arc<Self>) -> Injector {
        Injector(Arc::downgrade(self))
    }

    /// The next packet fed in, if any.
    pub(crate) fn take(&self) -> Option<(Vec<u8>, bool)> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
}

impl Injector {
    /// Queues a play packet body as if the client sent it; `skip_listeners` lets it pass the
    /// packet listeners. Returns false when the connection is gone or too many packets wait.
    pub fn inject(&self, body: Vec<u8>, skip_listeners: bool) -> bool {
        let Some(injected) = self.0.upgrade() else {
            return false;
        };
        {
            let mut queue = injected
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if queue.len() >= injected.max {
                return false;
            }
            queue.push_back((body, skip_listeners));
        }
        injected.ready.notify_one();
        true
    }
}

/// What `/vanillite debug` shows of a connection (#250): the client's protocol version and
/// brand, set once, and the packets it sent and got, counted by the reader and the writer.
#[derive(Debug)]
pub struct ClientDetails {
    protocol: AtomicI32,
    brand: OnceLock<String>,
    received: AtomicU64,
    sent: Arc<crate::sent_log::SentLog>,
}

impl ClientDetails {
    pub(crate) fn new(sent: Arc<crate::sent_log::SentLog>) -> Self {
        ClientDetails {
            protocol: AtomicI32::new(vanillite_protocol::PROTOCOL_VERSION),
            brand: OnceLock::new(),
            received: AtomicU64::new(0),
            sent,
        }
    }

    pub(crate) fn set_protocol(&self, protocol: i32) {
        self.protocol.store(protocol, Ordering::Relaxed);
    }

    pub(crate) fn set_brand(&self, brand: &str) {
        let _ = self.brand.set(brand.to_owned());
    }

    #[inline]
    pub(crate) fn count_received(&self) {
        self.received.fetch_add(1, Ordering::Relaxed);
    }

    /// The protocol version of the client (older ones through the multi-version layer).
    pub fn protocol(&self) -> i32 {
        self.protocol.load(Ordering::Relaxed)
    }

    /// The brand the client sent in configuration (`minecraft:brand`).
    pub fn brand(&self) -> Option<&str> {
        self.brand.get().map(String::as_str)
    }

    /// Packets received from the client so far.
    pub fn packets_received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Packets written to the client so far (a batch counts up to 32 of its frames).
    pub fn packets_sent(&self) -> u64 {
        u64::try_from(self.sent.written()).unwrap_or(u64::MAX)
    }
}

/// Keepalive latency in milliseconds, updated by the network thread.
#[derive(Debug, Clone, Default)]
pub struct Latency(pub(crate) Arc<AtomicU32>);

impl Latency {
    /// Current value in milliseconds.
    pub fn millis(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}
