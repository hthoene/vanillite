//! Settings of the built-in network pipeline. Every knob has a documented default; the server
//! maps them from its configuration files.

use std::net::SocketAddr;
use std::time::Duration;
use vanillite_protocol::Bucket;

/// Token bucket of one rate-limit class: `rate` packet costs per second, bursts up to `burst`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimit {
    pub rate: f32,
    pub burst: f32,
}

/// The server resource pack (`resource-pack*` and `require-resource-pack` of
/// `server.properties`, Vanilla's `ServerResourcePackInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct ResourcePackInfo {
    pub id: vanillite_protocol::types::Uuid,
    pub url: String,
    /// SHA-1 in hex; empty without one.
    pub hash: String,
    /// Players who decline it are kicked.
    pub required: bool,
    pub prompt: Option<vanillite_protocol::text::Component>,
}

/// Network settings.
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Address to listen on. Default `0.0.0.0:25565`.
    pub bind: SocketAddr,
    /// I/O threads of the network runtime. Default 2.
    pub io_threads: usize,
    /// Online mode: authenticate with the session server and encrypt. Default true (SEC-14).
    pub online_mode: bool,
    /// Pass the client address to the session server (`prevent-proxy-connections`). Default false.
    pub prevent_proxy_connections: bool,
    /// Compression threshold in bytes; negative turns compression off. Default 256.
    pub compression_threshold: i32,
    /// libdeflate level for outbound packets (1..=12). Default 4.
    pub compression_level: i32,
    /// Accept logins of clients transferred from another server. Default false.
    pub accepts_transfers: bool,
    /// Maximum open connections in total. Default 1024.
    pub max_connections: usize,
    /// Maximum connections per IP address that are not yet playing. Default 8 (SEC-18).
    pub max_pending_per_ip: u32,
    /// Maximum concurrent session-server requests. Default 16.
    pub max_concurrent_auth: usize,
    /// Time allowed for handshake and status. Default 10 s.
    pub status_timeout: Duration,
    /// Time allowed for the login state (Vanilla: 600 ticks). Default 30 s.
    pub login_timeout: Duration,
    /// Time allowed for the configuration state. Default 60 s.
    pub configuration_timeout: Duration,
    /// Disconnect after this long without any packet (Vanilla's read timeout). Default 30 s.
    pub read_timeout: Duration,
    /// Keepalive interval and timeout (Vanilla: 15 s). Default 15 s.
    pub keepalive_interval: Duration,
    /// Timeout of the session-server request. Default 10 s.
    pub auth_timeout: Duration,
    /// Outbound bytes queued per connection before it is dropped as not reading (SEC-17).
    /// Default 8 MiB.
    pub max_outbound_bytes: usize,
    /// Inbound play packets queued per connection for the world (SEC-17). Default 1024.
    pub max_inbound_packets: usize,
    /// Joined connections waiting for the server (SEC-18). Default 256.
    pub max_pending_joins: usize,
    /// Rate limit per bucket, indexed by [`Bucket::index`] (SEC-16).
    pub rate_limits: [RateLimit; Bucket::COUNT],
    /// Server brand sent in configuration. Default "vanillite".
    pub brand: String,
    /// Secret of Velocity modern forwarding; with it, identities come from the proxy. Default
    /// none (off).
    pub forwarding_secret: Option<crate::velocity::Secret>,
    /// With forwarding on, also accept clients that connect directly (they log in normally).
    /// Default false: direct connections are refused (SEC-14).
    pub allow_direct_connections: bool,
    /// Older client versions accepted through the multi-version layer. Default none.
    pub multiversion: vanillite_multiversion::Accepted,
    /// Answer server list pings (`enable-status`). Default true.
    pub enable_status: bool,
    /// Average packets per second after which a connection is kicked (`rate-limit`); 0 turns
    /// the check off. Default 0.
    pub rate_limit: u32,
    /// Show client addresses in the log (`log-ips`). Default true.
    pub log_ips: bool,
    /// The resource pack sent in configuration. Default none.
    pub resource_pack: Option<ResourcePackInfo>,
    /// Links sent in configuration (`bug-report-link`). Default none.
    pub server_links: Vec<vanillite_protocol::packets::common::ServerLink>,
    /// Codes of conduct by lower-case language (`enable-code-of-conduct`). Default none.
    pub code_of_conduct: std::collections::BTreeMap<String, String>,
    /// The packet tap: where plugins listen on packets. Default: no listeners.
    pub tap: std::sync::Arc<vanillite_protocol::tap::PacketTap>,
    /// Runs on an I/O thread each time it runs out of work (the server hands back what the
    /// thread's allocator heap keeps). Default none.
    pub on_idle: Option<IdleHook>,
}

/// See [`NetConfig::on_idle`].
#[derive(Clone)]
pub struct IdleHook(pub std::sync::Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for IdleHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IdleHook")
    }
}

impl Default for NetConfig {
    fn default() -> Self {
        let r = |rate: f32, burst: f32| RateLimit { rate, burst };
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 25565)),
            io_threads: 2,
            online_mode: true,
            prevent_proxy_connections: false,
            compression_threshold: 256,
            compression_level: 4,
            accepts_transfers: false,
            max_connections: 1024,
            max_pending_per_ip: 8,
            max_concurrent_auth: 16,
            status_timeout: Duration::from_secs(10),
            login_timeout: Duration::from_secs(30),
            configuration_timeout: Duration::from_secs(60),
            read_timeout: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(15),
            auth_timeout: Duration::from_secs(10),
            max_outbound_bytes: 8 << 20,
            max_inbound_packets: 1024,
            max_pending_joins: 256,
            // Default, KeepAlive, Movement, Chat, Command, Interact, Inventory, Payload.
            rate_limits: [
                r(200.0, 500.0),
                r(5.0, 20.0),
                r(100.0, 300.0),
                r(10.0, 40.0),
                r(20.0, 60.0),
                r(100.0, 300.0),
                r(100.0, 300.0),
                r(50.0, 200.0),
            ],
            brand: "vanillite".into(),
            forwarding_secret: None,
            allow_direct_connections: false,
            multiversion: vanillite_multiversion::Accepted::default(),
            enable_status: true,
            rate_limit: 0,
            log_ips: true,
            resource_pack: None,
            server_links: Vec::new(),
            code_of_conduct: std::collections::BTreeMap::new(),
            tap: std::sync::Arc::default(),
            on_idle: None,
        }
    }
}
