//! Login authentication: the server key pair, Minecraft's server hash and the
//! [`Authenticator`] slot with the session-server implementation (SEC-14).

use rsa::PrivateKey;
use rsa::pkcs8::EncodePublicKey;
use sha1::{Digest, Sha1};
use vanillite_protocol::types::{GameProfile, Property, Uuid};

/// The RSA key pair of a server run (1024 bits like Vanilla).
pub struct KeyPair {
    private: PrivateKey,
    public_der: Vec<u8>,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyPair(..)")
    }
}

impl KeyPair {
    /// Generates a fresh key pair.
    pub fn generate() -> Result<Self, String> {
        let private = PrivateKey::new(&mut getrandom::SysRng, 1024).map_err(|e| e.to_string())?;
        let public_der = private
            .to_public_key()
            .to_public_key_der()
            .map_err(|e| e.to_string())?
            .into_vec();
        Ok(Self {
            private,
            public_der,
        })
    }

    /// The public key as X.509 `SubjectPublicKeyInfo` DER (what the client expects).
    pub fn public_der(&self) -> &[u8] {
        &self.public_der
    }

    /// Decrypts a PKCS#1 v1.5 block from the client.
    ///
    /// Blinded and with implicit rejection (no Marvin/Bleichenbacher timing oracle): bad
    /// padding yields a deterministic synthetic message instead of an error, so callers
    /// must check the result (shared secret length, echoed challenge) like any other input.
    pub fn decrypt(&self, data: &[u8]) -> Option<Vec<u8>> {
        self.private.pkcs1v15_decrypt(data).ok()
    }
}

/// Minecraft's server hash: SHA-1 over server id, shared secret and public key, printed as
/// Java's signed `BigInteger` hex.
pub fn server_hash(server_id: &str, secret: &[u8], public_der: &[u8]) -> String {
    let mut sha = Sha1::new();
    sha.update(server_id.as_bytes());
    sha.update(secret);
    sha.update(public_der);
    let mut digest: [u8; 20] = sha.finalize().into();
    let negative = digest.first().is_some_and(|b| b & 0x80 != 0);
    if negative {
        // Two's complement negation of the big-endian number.
        let mut carry = true;
        for b in digest.iter_mut().rev() {
            *b = !*b;
            if carry {
                let (v, c) = b.overflowing_add(1);
                *b = v;
                carry = c;
            }
        }
    }
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let trimmed = hex.trim_start_matches('0');
    let body = if trimmed.is_empty() { "0" } else { trimmed };
    if negative {
        format!("-{body}")
    } else {
        body.to_owned()
    }
}

/// Outcome of an authentication attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    /// The profile the session server confirmed.
    Verified(GameProfile),
    /// The session server does not know this join (`multiplayer.disconnect.unverified_username`).
    Unverified,
    /// The session server could not be reached (`multiplayer.disconnect.authservers_down`).
    Unavailable,
}

/// Verifies that a player joined through their client (the `Authenticator` slot). Called on a
/// blocking thread.
pub trait Authenticator: Send + Sync + 'static {
    /// Checks `name` against `server_hash`; `ip` is set when proxy connections are prevented.
    fn authenticate(
        &self,
        name: &str,
        server_hash: &str,
        ip: Option<std::net::IpAddr>,
    ) -> AuthResult;

    /// A login is about to be authenticated (the client got the encryption request and asks
    /// the session server itself now): a chance to open the connection to the session server
    /// in the meantime. Called on a blocking thread; must not affect the result of
    /// [`Authenticator::authenticate`].
    fn prepare(&self) {}
}

/// How long an opened session server connection counts as warm: within this time after the
/// last request [`SessionServer::prepare`] sends nothing (the pooled connection is still
/// open, `ureq`'s idle limit is 15 s).
const WARM_FOR: std::time::Duration = std::time::Duration::from_secs(10);

/// Mojang's session server.
#[derive(Debug)]
pub struct SessionServer {
    base: String,
    agent: ureq::Agent,
    /// When the last request to the session server ended.
    last_used: std::sync::Mutex<Option<std::time::Instant>>,
}

impl SessionServer {
    /// The official session server with a request timeout.
    pub fn new(timeout: std::time::Duration) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            base: "https://sessionserver.mojang.com/session/minecraft/hasJoined".into(),
            agent,
            last_used: std::sync::Mutex::new(None),
        }
    }

    /// Notes a finished request; returns whether the connection was warm before.
    fn touch(&self) -> bool {
        let mut last = self
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        let warm = last.is_some_and(|t| now.duration_since(t) < WARM_FOR);
        *last = Some(now);
        warm
    }
}

impl Authenticator for SessionServer {
    fn authenticate(
        &self,
        name: &str,
        server_hash: &str,
        ip: Option<std::net::IpAddr>,
    ) -> AuthResult {
        let mut req = self
            .agent
            .get(&self.base)
            .query("username", name)
            .query("serverId", server_hash);
        if let Some(ip) = ip {
            req = req.query("ip", ip.to_string());
        }
        let result = req.call();
        self.touch();
        let mut resp = match result {
            Ok(r) => r,
            Err(_) => return AuthResult::Unavailable,
        };
        match resp.status().as_u16() {
            200 => {}
            204 | 404 => return AuthResult::Unverified,
            _ => return AuthResult::Unavailable,
        }
        let Ok(body) = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_string()
        else {
            return AuthResult::Unavailable;
        };
        parse_profile(&body).map_or(AuthResult::Unavailable, AuthResult::Verified)
    }

    /// Opens (and pools) the TLS connection while the client talks to Mojang, so
    /// `hasJoined` does not pay for the handshake: a `HEAD` without parameters, at most once
    /// per [`WARM_FOR`]. Its answer is ignored.
    fn prepare(&self) {
        if self.touch() {
            return;
        }
        let _ = self.agent.head(&self.base).call();
        self.touch();
    }
}

/// Parses the session server's profile JSON with the protocol's limits.
fn parse_profile(body: &str) -> Option<GameProfile> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let id = Uuid::parse(v.get("id")?.as_str()?)?;
    let name = v.get("name")?.as_str()?;
    if name.is_empty() || name.chars().count() > 16 {
        return None;
    }
    let properties = v
        .get("properties")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .take(16)
                .filter_map(|p| {
                    Some(Property {
                        name: p.get("name")?.as_str()?.chars().take(64).collect(),
                        value: p.get("value")?.as_str()?.to_owned(),
                        signature: p
                            .get("signature")
                            .and_then(|s| s.as_str())
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(GameProfile {
        id,
        name: name.to_owned(),
        properties,
    })
}

/// Vanilla's `StringUtil.isValidPlayerName`: at most 16 characters, none of them control,
/// space or non-ASCII.
pub fn is_valid_player_name(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 16 && name.chars().all(|c| ('!'..='~').contains(&c))
}
