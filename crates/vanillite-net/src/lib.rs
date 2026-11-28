//! Built-in network pipeline: listener, I/O threads, connection state machine (handshake,
//! status, login, configuration), authentication, rate limits, bounded queues and virtual
//! connections.
//!
//! The pipeline runs on its own small tokio runtime; the game never touches it. A connection
//! reaches the server only after configuration, as a [`Joined`] with a bounded inbound queue of
//! validated play packets and an [`Outbound`] handle.

mod auth;
mod config;
mod configuration;
mod conn;
mod limits;
mod outbound;
mod player;
pub mod query;
pub mod rcon;
mod sent_log;
pub mod services;
mod status;
pub mod velocity;

pub use auth::{
    AuthResult, Authenticator, KeyPair, SessionServer, is_valid_player_name, server_hash,
};
pub use config::{IdleHook, NetConfig, RateLimit, ResourcePackInfo};
pub use configuration::{ConfigurationData, json_to_nbt, network_tags};
pub use conn::rand_bytes;
pub use limits::{ConnectionGuard, ConnectionLimiter};
pub use outbound::Outbound;
pub use player::{ClientDetails, Injector, Joined, Latency, LoginCheck};
pub use status::{StatusCache, StatusInfo};
pub use vanillite_protocol::{Inbound, SharedPacket};

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc};
use vanillite_protocol::types::{GameProfile, Uuid};

/// State shared by all connections.
pub(crate) struct Shared {
    pub(crate) config: NetConfig,
    pub(crate) status: StatusCache,
    pub(crate) keys: Option<KeyPair>,
    pub(crate) authenticator: Arc<dyn Authenticator>,
    /// Replaced when `/reload` changes the tags.
    configuration: std::sync::RwLock<Arc<ConfigurationData>>,
    pub(crate) limiter: Arc<ConnectionLimiter>,
    pub(crate) auth_permits: Semaphore,
    pub(crate) logins: std::sync::mpsc::SyncSender<LoginCheck>,
    pub(crate) joins: std::sync::mpsc::SyncSender<Joined>,
    pub(crate) session_id: Uuid,
}

impl Shared {
    /// The current configuration data.
    pub(crate) fn configuration(&self) -> Arc<ConfigurationData> {
        Arc::clone(
            &self
                .configuration
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// A virtual connection (architecture 14.1): packet bodies (`id` + fields, current protocol,
/// uncompressed, unencrypted) in both directions, for an identity the creator vouches for. The
/// peer talks like a Java client from the handshake on; login skips encryption and the session
/// server and uses the vouched profile.
#[derive(Debug)]
pub struct VirtualConnection {
    /// Packets to the server (bounded).
    pub to_server: mpsc::Sender<Vec<u8>>,
    /// Packets from the server.
    pub from_server: mpsc::Receiver<Vec<u8>>,
}

/// Opens virtual connections (Bedrock layer, proxies, tests). Cheap to clone.
#[derive(Clone)]
pub struct VirtualOpener {
    runtime: tokio::runtime::Handle,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for VirtualOpener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualOpener").finish_non_exhaustive()
    }
}

impl VirtualOpener {
    /// Opens a virtual connection for a vouched-for in-process client (all chunks at once).
    pub fn open(&self, profile: GameProfile, address: SocketAddr) -> VirtualConnection {
        self.open_with(profile, address, true)
    }

    /// Opens a virtual connection for a vouched-for remote client (a Bedrock player): chunks
    /// are paced by `chunk_batch_received` like for a TCP client.
    pub fn open_remote(&self, profile: GameProfile, address: SocketAddr) -> VirtualConnection {
        self.open_with(profile, address, false)
    }

    fn open_with(
        &self,
        profile: GameProfile,
        address: SocketAddr,
        memory: bool,
    ) -> VirtualConnection {
        let (to_server, server_rx) = mpsc::channel(self.shared.config.max_inbound_packets.max(1));
        let (client_tx, from_server) = mpsc::channel(self.shared.config.max_inbound_packets.max(1));
        let tap = Arc::clone(&self.shared.config.tap);
        let connection = tap.next_connection();
        let (out, out_rx, queued) =
            Outbound::new(self.shared.config.max_outbound_bytes, connection);
        let sent = out.sent_log();
        let conn = conn::Conn::virtual_(
            Arc::clone(&self.shared),
            address,
            server_rx,
            out,
            profile,
            memory,
        );
        self.runtime.spawn(outbound::virtual_writer(
            client_tx, out_rx, queued, sent, tap, connection,
        ));
        self.runtime.spawn(conn.run());
        VirtualConnection {
            to_server,
            from_server,
        }
    }
}

/// What connections hand to the server: logins waiting for its decision, then joined players.
/// Both are drained on the server thread every tick.
#[derive(Debug)]
pub struct Arrivals {
    /// Logins to admit or refuse before `LoginFinished`.
    pub logins: std::sync::mpsc::Receiver<LoginCheck>,
    /// Players that finished configuration.
    pub joins: std::sync::mpsc::Receiver<Joined>,
}

/// The running network pipeline.
pub struct Network {
    runtime: tokio::runtime::Runtime,
    shared: Arc<Shared>,
    local_addr: SocketAddr,
}

impl std::fmt::Debug for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Network")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl Network {
    /// Binds the listener and starts the I/O threads. Logins to decide on and joined players
    /// arrive on the returned [`Arrivals`]. `authenticator` defaults to Mojang's session
    /// server in online mode.
    pub fn start(
        config: NetConfig,
        status: StatusInfo,
        configuration: ConfigurationData,
        authenticator: Option<Arc<dyn Authenticator>>,
    ) -> std::io::Result<(Self, Arrivals)> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder
            .worker_threads(config.io_threads.max(1))
            .max_blocking_threads(config.max_concurrent_auth.max(1))
            .thread_name("vanillite-net")
            .enable_io()
            .enable_time();
        if let Some(hook) = config.on_idle.clone() {
            builder.on_thread_park(move || (hook.0)());
        }
        let runtime = builder.build()?;
        let keys = if config.online_mode {
            Some(KeyPair::generate().map_err(std::io::Error::other)?)
        } else {
            log::warn!("The server runs in offline mode: player identities are not verified");
            None
        };
        let authenticator =
            authenticator.unwrap_or_else(|| Arc::new(SessionServer::new(config.auth_timeout)));
        let (joins, joined) = std::sync::mpsc::sync_channel(config.max_pending_joins);
        let (logins, login_checks) = std::sync::mpsc::sync_channel(config.max_pending_joins);
        let shared = Arc::new(Shared {
            status: StatusCache::new(&status),
            keys,
            authenticator,
            configuration: std::sync::RwLock::new(Arc::new(configuration)),
            limiter: ConnectionLimiter::new(config.max_connections, config.max_pending_per_ip),
            auth_permits: Semaphore::new(config.max_concurrent_auth.max(1)),
            logins,
            joins,
            session_id: Uuid(u128::from_be_bytes(conn::rand_bytes())),
            config,
        });
        let listener = runtime.block_on(tokio::net::TcpListener::bind(shared.config.bind))?;
        let local_addr = listener.local_addr()?;
        runtime.spawn(accept_loop(listener, Arc::clone(&shared)));
        log::info!("Listening on {local_addr}");
        Ok((
            Self {
                runtime,
                shared,
                local_addr,
            },
            Arrivals {
                logins: login_checks,
                joins: joined,
            },
        ))
    }

    /// The bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The status cache, for the server to update player counts and the description.
    pub fn status(&self) -> &StatusCache {
        &self.shared.status
    }

    /// Replaces the tags joining clients get (after `/reload`).
    pub fn set_tags(
        &self,
        tags: &vanillite_protocol::packets::configuration::clientbound::UpdateTags,
    ) {
        let mut current = self
            .shared
            .configuration
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = Arc::new(current.with_tags(tags));
    }

    /// Starts the RCON listener (`enable-rcon`); returns the bound address.
    pub fn start_rcon(
        &self,
        config: rcon::RconConfig,
        runner: rcon::RconRunner,
    ) -> std::io::Result<SocketAddr> {
        self.runtime.block_on(rcon::bind(config, runner))
    }

    /// Starts the Query listener (`enable-query`); returns the bound address.
    pub fn start_query(
        &self,
        config: query::QueryConfig,
        players: Arc<query::QueryPlayers>,
    ) -> std::io::Result<SocketAddr> {
        self.runtime.block_on(query::bind(config, players))
    }

    /// Opens a virtual connection for a vouched-for profile.
    pub fn open_virtual(&self, profile: GameProfile, address: SocketAddr) -> VirtualConnection {
        self.virtual_opener().open(profile, address)
    }

    /// A handle that opens virtual connections from any thread while the network runs.
    pub fn virtual_opener(&self) -> VirtualOpener {
        VirtualOpener {
            runtime: self.runtime.handle().clone(),
            shared: Arc::clone(&self.shared),
        }
    }

    /// Wakes the I/O threads, so each runs [`NetConfig::on_idle`] when it parks again.
    pub fn idle_waker(&self) -> Box<dyn Fn() + Send + Sync> {
        let runtime = self.runtime.handle().clone();
        let threads = self.shared.config.io_threads.max(1);
        Box::new(move || {
            for _ in 0..threads {
                runtime.spawn(async {});
            }
        })
    }

    /// Stops accepting and closes all connections.
    pub fn shutdown(self) {
        self.runtime
            .shutdown_timeout(std::time::Duration::from_secs(1));
    }
}

async fn accept_loop(listener: tokio::net::TcpListener, shared: Arc<Shared>) {
    loop {
        let (socket, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let Some(guard) = shared.limiter.admit(addr.ip()) else {
            // Over the total or per-IP limit (SEC-18): close without reading anything.
            continue;
        };
        let _ = socket.set_nodelay(true);
        let (read, write) = socket.into_split();
        let tap = Arc::clone(&shared.config.tap);
        let connection = tap.next_connection();
        let (out, out_rx, queued) = Outbound::new(shared.config.max_outbound_bytes, connection);
        tokio::spawn(outbound::tcp_writer(
            write,
            out_rx,
            queued,
            shared.config.compression_level,
            out.sent_log(),
            tap,
            connection,
        ));
        tokio::spawn(conn::Conn::tcp(Arc::clone(&shared), addr, read, out, guard).run());
    }
}
