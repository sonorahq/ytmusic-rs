//! Proof-of-origin tokens, the parameter YouTube asks for before it serves a stream.
//!
//! A token is bound to one identifier and refused against any other. For `WEB_REMIX` that
//! identifier is the session: the account's data sync id when signed in, and the visitor id
//! otherwise, so one token covers every track of a run. Minting one takes a browser, because
//! Google's BotGuard virtual machine attests the runtime it is in and a hand-written
//! environment does not pass. That is why the real minter lives outside this crate behind
//! [`Minter`], and why a host without one still gets [`cold_start`], which YouTube honours
//! for the first megabyte or two of a stream and no further.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use tokio::sync::RwLock;

/// How long a minted token is reused before another is asked for. Google's integrity token
/// lasts twelve hours; this stays well inside that so a token is never sent after its
/// minter has gone stale.
const KEEP: Duration = Duration::from_secs(6 * 60 * 60);
/// How long a mint may take before the request goes out without a token. Playback waits on
/// this, so a minter that is warming up or wedged must not hold up a track.
const PATIENCE: Duration = Duration::from_secs(5);
/// The first byte of a cold start token, a protobuf field tag the server expects.
const TAG: u8 = 34;
/// The bytes at the head of a cold start payload that double as the xor key.
const KEY: usize = 2;

/// Mints proof-of-origin tokens for a host that has a browser to run BotGuard in.
///
/// `mint` answers `None` when no token can be had, including while the minter is still
/// warming up; the caller then falls back to a cold start token rather than failing the
/// request. Calls are made on the playback path, so an implementation returns what it has
/// instead of blocking on a fresh attestation.
#[async_trait::async_trait]
pub trait Minter: Send + Sync + 'static {
    async fn mint(&self, binding: &str) -> Option<String>;
}

/// The tokens one client sends, and the minter they come from.
#[derive(Default)]
pub struct PoTokens {
    minter: Option<Arc<dyn Minter>>,
    kept: RwLock<HashMap<String, Kept>>,
}

struct Kept {
    token: String,
    minted: Instant,
}

impl PoTokens {
    pub fn new(minter: Option<Arc<dyn Minter>>) -> Self {
        Self {
            minter,
            kept: RwLock::new(HashMap::new()),
        }
    }

    /// The token to send for `binding`. Answers a minted token when the host has a minter
    /// and it has one ready, and a cold start token otherwise, so a request is never sent
    /// without the parameter at all.
    pub async fn token(&self, binding: &str) -> String {
        if let Some(token) = self.minted(binding).await {
            return token;
        }
        cold_start(binding)
    }

    /// Whether a host installed a minter at all. A run without one only ever sends cold
    /// start tokens, which is worth saying once in a log rather than on every track.
    pub fn has_minter(&self) -> bool {
        self.minter.is_some()
    }

    async fn minted(&self, binding: &str) -> Option<String> {
        let minter = self.minter.as_ref()?;
        if let Some(kept) = self.kept.read().await.get(binding)
            && kept.minted.elapsed() < KEEP
        {
            return Some(kept.token.clone());
        }
        let minted = tokio::time::timeout(PATIENCE, minter.mint(binding)).await;
        let token = match minted {
            Ok(token) => token?,
            Err(_) => {
                log::warn!("potoken: the minter did not answer for {binding} in {PATIENCE:?}");
                return None;
            }
        };
        self.kept.write().await.insert(
            binding.to_string(),
            Kept {
                token: token.clone(),
                minted: Instant::now(),
            },
        );
        Some(token)
    }
}

/// The placeholder the web player sends while BotGuard is still starting up.
///
/// It carries the same binding as a real token but no attestation, so YouTube serves a
/// megabyte or two against it and then refuses the rest. The payload is a two byte random
/// key, a client state byte, the unix timestamp, and the binding, all xored against the key.
pub fn cold_start(binding: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as u32)
        .unwrap_or(0);
    let keys: [u8; KEY] = rand::random();
    let mut payload = Vec::with_capacity(8 + binding.len());
    payload.extend_from_slice(&keys);
    payload.extend_from_slice(&[0, 1]);
    payload.extend_from_slice(&now.to_be_bytes());
    payload.extend_from_slice(binding.as_bytes());
    for at in KEY..payload.len() {
        payload[at] ^= payload[at % KEY];
    }
    let mut packet = Vec::with_capacity(2 + payload.len());
    packet.push(TAG);
    packet.push(payload.len() as u8);
    packet.extend_from_slice(&payload);
    URL_SAFE.encode(packet)
}
