// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//! Cache layer for Soroban RPC `simulateTransaction` responses — issue #223.
//!
//! `simulateTransaction` executes contract WASM in a sandbox on every call,
//! so identical requests are expensive to recompute. This module provides:
//!
//! - [`SimulationCache`]: an in-memory LRU keyed on the canonical simulation
//!   request (contract ID + function name + args + ledger sequence),
//!   returning the raw JSON-RPC response bytes captured at fill time.
//! - [`SimCacheKey`]: deterministic key derivation shared with the Redis
//!   tier, so both tiers agree on identity across processes.
//! - [`RedisSimCacheStore`]: an optional Redis tier speaking RESP directly
//!   over TCP (same dependency-free pattern as
//!   `rest_api::gateway::distributed_ratelimit`), sharing sim-cache entries
//!   across proxy replicas.
//!
//! Correctness contract (the issue's "never serve stale data across ledger
//! boundaries" constraint): the ledger sequence is part of the cache key and
//! is also tracked as a monotonic high-water mark. Two layers of protection:
//!
//! 1. **Key isolation** — a request pinned to ledger *N* can never hit an
//!    entry filled at ledger *M ≠ N*, even across LRU eviction and Redis
//!    TTLs, because the key hashes the sequence.
//! 2. **Invalidation hook** — [`SimulationCache::on_ledger_increment`]
//!    removes every entry at or below the previous sequence on each ledger
//!    increment, and the high-water mark rejects any older entry that slips
//!    through (e.g. filled by another replica between the hook and the
//!    increment). Unpinned entries (no `ledgerSeq` in the request) are
//!    dropped on every increment and additionally expire after
//!    [`UNKNOWN_SEQUENCE_TTL`].
//!
//! The cache is fail-open by construction: every fallible operation degrades
//! to a miss, never to an error surfaced to the RPC client.

use std::num::NonZeroUsize;
use std::time::Duration;

use lru::LruCache;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::warn;

/// Default LRU capacity in entries.
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;
/// Default Redis TTL for shared sim-cache entries. Short by design: the
/// ledger sequence in the key is the correctness mechanism, the TTL only
/// bounds cross-replica memory and staleness windows.
pub const DEFAULT_REDIS_TTL: Duration = Duration::from_secs(30);
/// Default deadline for each Redis round-trip. Fail-open design: a slow Redis
/// must never dominate the latency of the request it was meant to protect.
pub const DEFAULT_REDIS_TIMEOUT: Duration = Duration::from_millis(25);
/// Envelope TTL for pinned entries (see [`SimulationCacheConfig::entry_ttl`]).
pub const DEFAULT_ENTRY_TTL: Duration = Duration::from_secs(300);
/// TTL for entries captured without a pinned ledger sequence: they follow the
/// network head, which can move at any time, so they must expire quickly even
/// if no ledger-increment hook fires.
pub const UNKNOWN_SEQUENCE_TTL: Duration = Duration::from_secs(5);

/// The RPC method whose responses this cache stores.
///
/// Deliberately only `simulateTransaction`: a `getTransaction` response is a
/// function of ledger state (NOT_FOUND transitions to SUCCESS/FAILED when the
/// transaction is included), and its params carry no `ledgerSeq` to key on —
/// caching it would serve stale statuses across ledger boundaries. The
/// fail-open `soroban-cache-proxy` continues to cache idempotent state reads
/// (`getLedgerEntry` and friends) through its existing `StateCache` path.
pub const CACHED_METHODS: [&str; 1] = ["simulateTransaction"];

// ─────────────────────────────────────────────────────────────────────────────
// Cache keys
// ─────────────────────────────────────────────────────────────────────────────

/// Canonical identity of a cacheable simulation request.
///
/// Built from the JSON-RPC params; hashed to a stable string key shared by
/// the LRU and the Redis tier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SimCacheKey {
    pub method: &'static str,
    /// SHA-256 over the canonical serialization of the simulation parameters
    /// (contract ID, function name, args, auth, resource config, ledger).
    pub params_hash: [u8; 32],
    /// Ledger sequence the request was pinned to. `None` when the params
    /// carried no `ledgerSeq` (request follows the network head).
    pub ledger_sequence: Option<u64>,
}

impl SimCacheKey {
    /// Derive a cache key from a JSON-RPC request body.
    ///
    /// Returns `None` for anything that is not a cacheable simulation or
    /// transaction-status request with an ID (notifications bypass the cache).
    pub fn from_request(body: &[u8]) -> Option<Self> {
        let request: serde_json::Value = serde_json::from_slice(body).ok()?;
        let object = request.as_object()?;
        // Notifications have no response ID and must bypass the cache.
        object.get("id")?;
        let method = object.get("method")?.as_str()?;
        if method != "simulateTransaction" {
            return None;
        }
        let params = object
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        Self::from_params("simulateTransaction", &params)
    }

    /// Derive a key from already-extracted params (used by callers that parse
    /// the envelope separately).
    pub fn from_params(method: &'static str, params: &serde_json::Value) -> Option<Self> {
        if !CACHED_METHODS.contains(&method) {
            return None;
        }
        let (params_hash, ledger_sequence) = match params {
            // simulateTransaction: params is a single object.
            serde_json::Value::Object(_) => (hash_params(params), extract_ledger(params)),
            _ => return None,
        };
        Some(Self {
            method,
            params_hash,
            ledger_sequence,
        })
    }

    /// String form used as the LRU key and (prefixed) the Redis key. Both
    /// tiers must derive it the same way.
    pub fn encoded(&self) -> String {
        let ledger = self
            .ledger_sequence
            .map(|s| s.to_string())
            .unwrap_or_else(|| "head".to_string());
        format!(
            "simcache:{}:{}:{ledger}",
            self.method,
            hex(&self.params_hash)
        )
    }
}

/// SHA-256 over a canonical serialization of the params value.
///
/// `serde_json` serializes `Value` objects through `BTreeMap`, so two
/// structurally identical params with different key order hash identically;
/// whitespace never survives parsing.
fn hash_params(params: &serde_json::Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(params).unwrap_or_default());
    hasher.finalize().into()
}

fn extract_ledger(params: &serde_json::Value) -> Option<u64> {
    match params.get("ledgerSeq") {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// Lowercase hex without external crates.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// In-memory LRU
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for [`SimulationCache`].
#[derive(Debug, Clone)]
pub struct SimulationCacheConfig {
    /// Maximum entries in the LRU.
    pub max_entries: usize,
    /// Envelope TTL applied to entries pinned to a ledger sequence. The
    /// sequence in the key plus the invalidation hook are the correctness
    /// mechanisms; this TTL is only a garbage-collection backstop for the
    /// case where the network stalls and no increments fire.
    pub entry_ttl: Duration,
}

impl Default for SimulationCacheConfig {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            entry_ttl: DEFAULT_ENTRY_TTL,
        }
    }
}

/// One cached JSON-RPC response plus its capture metadata.
#[derive(Debug, Clone)]
pub struct CachedSimulation {
    /// Raw JSON-RPC response body as captured from upstream.
    pub body: Vec<u8>,
    /// Ledger sequence the simulation was executed against (from the request
    /// params at fill time).
    pub ledger_sequence: Option<u64>,
    /// When the entry was filled (monotonic clock; process-local only).
    pub captured_at: std::time::Instant,
}

/// Hit/miss/invalidation counters surfaced via `/stats` for the issue's
/// "cache hit/miss ratio metrics under load" review requirement.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub invalidations: u64,
}

struct LruState {
    lru: LruCache<String, CachedSimulation>,
    /// High-water mark: the highest ledger sequence seen by any
    /// `on_ledger_increment` call. Entries pinned below it are stale.
    ledger_high_water: Option<u64>,
    stats: CacheStats,
}

/// In-memory LRU cache for simulation responses.
///
/// Clones cheaply; the inner state is behind a tokio `Mutex` so
/// `get`/`insert` can be awaited from async contexts without blocking the
/// executor.
#[derive(Clone)]
pub struct SimulationCache {
    config: SimulationCacheConfig,
    inner: std::sync::Arc<Mutex<LruState>>,
}

impl SimulationCache {
    /// Build a cache with the given configuration.
    pub fn new(config: SimulationCacheConfig) -> Self {
        let capacity = NonZeroUsize::new(config.max_entries.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            config,
            inner: std::sync::Arc::new(Mutex::new(LruState {
                lru: LruCache::new(capacity),
                ledger_high_water: None,
                stats: CacheStats::default(),
            })),
        }
    }

    /// Look up a simulation response.
    ///
    /// Key-isolation first: a key whose sequence is below the ledger
    /// high-water mark is answered with a miss (and dropped) even if the LRU
    /// still holds it, so no entry can be served across a boundary it
    /// predates. Then the TTL envelope, then the LRU hit.
    pub async fn get(&self, key: &SimCacheKey) -> Option<CachedSimulation> {
        let mut state = self.inner.lock().await;
        let encoded = key.encoded();
        if let (Some(high_water), Some(seq)) = (state.ledger_high_water, key.ledger_sequence) {
            if seq < high_water {
                state.lru.pop(&encoded);
                state.stats.misses += 1;
                return None;
            }
        }
        let ttl = if key.ledger_sequence.is_some() {
            self.config.entry_ttl
        } else {
            UNKNOWN_SEQUENCE_TTL
        };
        if let Some(entry) = state.lru.get(&encoded).cloned() {
            if entry.captured_at.elapsed() > ttl {
                state.lru.pop(&encoded);
                state.stats.misses += 1;
                return None;
            }
            state.stats.hits += 1;
            return Some(entry);
        }
        state.stats.misses += 1;
        None
    }

    /// Store a response captured from upstream.
    pub async fn insert(&self, key: SimCacheKey, body: Vec<u8>) {
        let mut state = self.inner.lock().await;
        state.lru.put(
            key.encoded(),
            CachedSimulation {
                body,
                ledger_sequence: key.ledger_sequence,
                captured_at: std::time::Instant::now(),
            },
        );
        state.stats.stores += 1;
    }

    /// Invalidation hook: call whenever the global ledger sequence
    /// increments. Drops every cached entry whose sequence is below
    /// `new_ledger` (i.e. everything that could predate state the new ledger
    /// changed) and raises the high-water mark.
    ///
    /// `None`-sequence entries (unpinned requests) are dropped too: they were
    /// captured against an unknown head and the head just moved.
    pub async fn on_ledger_increment(&self, new_ledger: u64) {
        let mut state = self.inner.lock().await;
        let before = state.lru.len();
        let stale: Vec<String> = state
            .lru
            .iter()
            .filter(|(_, entry)| match entry.ledger_sequence {
                Some(seq) => seq < new_ledger,
                // Unpinned entries were captured against the previous head.
                None => true,
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &stale {
            state.lru.pop(key);
        }
        let removed = before - state.lru.len();
        state.stats.invalidations += removed as u64;
        state.ledger_high_water = Some(state.ledger_high_water.unwrap_or(0).max(new_ledger));
    }

    /// Current stats snapshot.
    pub async fn stats(&self) -> CacheStats {
        self.inner.lock().await.stats.clone()
    }

    /// Number of live entries.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.lru.len()
    }

    /// True when the cache holds no entries.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Redis tier (RESP over TCP, no extra dependency)
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the optional Redis tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisSimCacheConfig {
    /// `host:port` of the Redis endpoint.
    pub address: String,
    /// TTL for shared entries. Must be short relative to ledger progression:
    /// the ledger sequence in the key is the correctness mechanism, the TTL
    /// only bounds cross-replica memory.
    pub ttl: Duration,
    /// Deadline for connect and for each command.
    pub timeout: Duration,
    /// Maximum pooled connections.
    pub pool_size: usize,
    /// Key prefix (namespacing for shared clusters).
    pub key_prefix: String,
}

impl Default for RedisSimCacheConfig {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:6379".to_string(),
            ttl: DEFAULT_REDIS_TTL,
            timeout: DEFAULT_REDIS_TIMEOUT,
            pool_size: 8,
            key_prefix: "stellar".to_string(),
        }
    }
}

/// Errors surfaced by the Redis tier. The caller treats all of them as
/// "treat as a miss" (fail-open).
#[derive(Debug)]
pub enum RedisSimCacheError {
    Unavailable(String),
    Protocol(String),
}

impl std::fmt::Display for RedisSimCacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "redis unavailable: {m}"),
            Self::Protocol(m) => write!(f, "redis protocol error: {m}"),
        }
    }
}

impl std::error::Error for RedisSimCacheError {}

/// Minimal RESP reply model for this client's needs.
#[derive(Debug, PartialEq, Eq)]
enum SimRedisReply {
    Status(String),
    Bulk(Option<Vec<u8>>),
    Error(String),
}

/// A pooled Redis connection.
struct SimRedisConnection {
    reader: tokio::io::BufReader<tokio::net::TcpStream>,
}

impl SimRedisConnection {
    async fn connect(address: &str, timeout: Duration) -> Result<Self, RedisSimCacheError> {
        let stream = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address))
            .await
            .map_err(|_| {
                RedisSimCacheError::Unavailable(format!("connect to {address} timed out"))
            })?
            .map_err(|e| RedisSimCacheError::Unavailable(format!("connect to {address}: {e}")))?;
        // Cache round-trips are tiny and latency-critical; Nagle would batch
        // them into the next ACK and blow the sub-10ms budget the issue sets.
        let _ = stream.set_nodelay(true);
        Ok(Self {
            reader: tokio::io::BufReader::new(stream),
        })
    }

    async fn send_raw(
        &mut self,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<SimRedisReply, RedisSimCacheError> {
        use tokio::io::AsyncWriteExt;
        tokio::time::timeout(timeout, async {
            self.reader
                .get_mut()
                .write_all(payload)
                .await
                .map_err(|e| RedisSimCacheError::Unavailable(e.to_string()))?;
            read_sim_reply(&mut self.reader).await
        })
        .await
        .map_err(|_| RedisSimCacheError::Unavailable("command timed out".into()))?
    }
}

/// Redis-backed shared store for simulation responses, speaking RESP directly
/// over pooled TCP connections (no Redis client crate — same rationale as
/// `distributed_ratelimit`).
pub struct RedisSimCacheStore {
    config: RedisSimCacheConfig,
    pool: Mutex<Vec<SimRedisConnection>>,
}

impl RedisSimCacheStore {
    /// Create a store. No connection is opened until the first command.
    pub fn new(config: RedisSimCacheConfig) -> Self {
        Self {
            config,
            pool: Mutex::new(Vec::new()),
        }
    }

    /// Fetch one entry. `None` is a miss or any error (fail-open); the two
    /// are deliberately indistinguishable to callers.
    pub async fn get(&self, key: &SimCacheKey) -> Option<Vec<u8>> {
        let full = self.full_key(key);
        let payload = encode_sim_command(&["GET", &full]);
        match self.run_raw(&payload).await {
            Ok(SimRedisReply::Bulk(Some(bytes))) => Some(bytes),
            Ok(_) => None,
            Err(error) => {
                warn!(%error, "simulation cache redis get failed; failing open");
                None
            }
        }
    }

    /// Store one entry with the configured TTL.
    pub async fn set(&self, key: &SimCacheKey, body: &[u8]) {
        let full = self.full_key(key);
        let ttl_secs = self.config.ttl.as_secs().max(1).to_string();
        // RESP2 bulk strings are length-prefixed and binary-safe, so the
        // JSON-RPC response bytes go over the wire verbatim — no base64.
        let mut payload = format!("*4\r\n$3\r\nSET\r\n${}\r\n{full}\r\n", full.len()).into_bytes();
        payload.extend_from_slice(format!("${}\r\n", body.len()).as_bytes());
        payload.extend_from_slice(body);
        payload.extend_from_slice(b"\r\n");
        payload.extend_from_slice(format!("${}\r\n{ttl_secs}\r\n", ttl_secs.len()).as_bytes());
        if let Err(error) = self.run_raw(&payload).await {
            warn!(%error, "simulation cache redis set failed; failing open");
        }
    }

    async fn run_raw(&self, payload: &[u8]) -> Result<SimRedisReply, RedisSimCacheError> {
        let mut conn = match self.pool.lock().await.pop() {
            Some(conn) => conn,
            None => SimRedisConnection::connect(&self.config.address, self.config.timeout).await?,
        };
        let reply = conn.send_raw(payload, self.config.timeout).await;
        if reply.is_ok() {
            let mut pool = self.pool.lock().await;
            if pool.len() < self.config.pool_size {
                pool.push(conn);
            }
        }
        // A connection that errored is dropped, so a half-open socket cannot
        // poison later requests.
        reply
    }

    fn full_key(&self, key: &SimCacheKey) -> String {
        format!("{}:{}", self.config.key_prefix, key.encoded())
    }
}

/// Encode a command as a RESP array of bulk strings (same wire format as
/// `distributed_ratelimit::encode_command`, kept local so this module has no
/// cross-gate dependency).
fn encode_sim_command(args: &[&str]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + args.iter().map(|a| a.len() + 16).sum::<usize>());
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Read one RESP reply (status, bulk, or error).
async fn read_sim_reply(
    reader: &mut tokio::io::BufReader<tokio::net::TcpStream>,
) -> Result<SimRedisReply, RedisSimCacheError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .await
        .map_err(|e| RedisSimCacheError::Unavailable(e.to_string()))?;
    if read == 0 {
        return Err(RedisSimCacheError::Unavailable("connection closed".into()));
    }
    let line = line.trim_end_matches(['\r', '\n']);
    let (tag, rest) = line
        .split_at_checked(1)
        .ok_or_else(|| RedisSimCacheError::Protocol("empty reply".into()))?;
    match tag {
        "+" => Ok(SimRedisReply::Status(rest.to_string())),
        "-" => Ok(SimRedisReply::Error(rest.to_string())),
        "$" => {
            let len: i64 = rest
                .parse()
                .map_err(|_| RedisSimCacheError::Protocol(format!("bad bulk length: {rest}")))?;
            if len < 0 {
                return Ok(SimRedisReply::Bulk(None));
            }
            let mut buf = vec![0u8; len as usize + 2]; // payload + CRLF
            reader
                .read_exact(&mut buf)
                .await
                .map_err(|e| RedisSimCacheError::Unavailable(e.to_string()))?;
            buf.truncate(len as usize);
            Ok(SimRedisReply::Bulk(Some(buf)))
        }
        other => Err(RedisSimCacheError::Protocol(format!(
            "unsupported reply type '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sim_request(ledger: Option<u64>, args: &[u64]) -> Vec<u8> {
        let args: Vec<serde_json::Value> = args.iter().map(|a| json!(format!("{a:064}"))).collect();
        let mut params = json!({
            "transaction": format!("X{:0120}", args.len()),
            "contractId": "CABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789",
            "functionName": "balance",
            "args": args,
        });
        if let Some(seq) = ledger {
            params["ledgerSeq"] = json!(seq);
        }
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateTransaction",
            "params": params,
        })
        .to_string()
        .into_bytes()
    }

    fn key_for(body: &[u8]) -> SimCacheKey {
        SimCacheKey::from_request(body).expect("valid sim request")
    }

    #[tokio::test]
    async fn identical_requests_share_one_key() {
        let a = key_for(&sim_request(Some(100), &[1, 2, 3]));
        let b = key_for(&sim_request(Some(100), &[1, 2, 3]));
        assert_eq!(a, b);
        assert_eq!(a.encoded(), b.encoded());
    }

    #[test]
    fn key_order_and_whitespace_insensitive() {
        // Same logical params, rebuilt with a different top-level key order.
        // Params are hashed canonically and method/ledger are extracted
        // explicitly, so the key must not change.
        let body1 = sim_request(Some(100), &[1, 2, 3]);
        let mut value: serde_json::Value = serde_json::from_slice(&body1).unwrap();
        let obj = value.as_object_mut().unwrap();
        let params = obj.remove("params").unwrap();
        let mut reordered = serde_json::Map::new();
        reordered.insert("params".to_string(), params);
        reordered.insert("method".to_string(), json!("simulateTransaction"));
        reordered.insert("id".to_string(), json!(1));
        reordered.insert("jsonrpc".to_string(), json!("2.0"));
        let body2 = serde_json::to_vec(&reordered).unwrap();
        let k1 = key_for(&body1);
        let k2 = key_for(&body2);
        assert_eq!(k1.params_hash, k2.params_hash);
        assert_eq!(k1.encoded(), k2.encoded());
    }

    #[test]
    fn ledger_sequence_changes_the_key() {
        let a = key_for(&sim_request(Some(100), &[1]));
        let b = key_for(&sim_request(Some(101), &[1]));
        assert_ne!(a.encoded(), b.encoded());
    }

    #[test]
    fn unpinned_request_has_head_key() {
        let key = key_for(&sim_request(None, &[1]));
        assert!(key.ledger_sequence.is_none());
        assert!(key.encoded().ends_with(":head"));
    }

    #[test]
    fn notifications_and_other_methods_bypass() {
        let notification = br#"{"jsonrpc":"2.0","method":"simulateTransaction","params":{}}"#;
        assert!(SimCacheKey::from_request(notification).is_none());
        let send = br#"{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":{}}"#;
        assert!(SimCacheKey::from_request(send).is_none());
        // getTransaction responses depend on ledger state without carrying a
        // ledgerSeq — excluded by design (see CACHED_METHODS).
        let tx = br#"{"jsonrpc":"2.0","id":7,"method":"getTransaction","params":"abc123"}"#;
        assert!(SimCacheKey::from_request(tx).is_none());
        let malformed = b"not json at all";
        assert!(SimCacheKey::from_request(malformed).is_none());
    }

    #[tokio::test]
    async fn lru_roundtrip_and_hit_stats() {
        let cache = SimulationCache::new(SimulationCacheConfig {
            max_entries: 8,
            ..SimulationCacheConfig::default()
        });
        let key = key_for(&sim_request(Some(100), &[1]));
        assert!(cache.get(&key).await.is_none());
        cache
            .insert(key.clone(), br#"{"result":{}}"#.to_vec())
            .await;
        let hit = cache.get(&key).await.expect("hit after insert");
        assert_eq!(hit.body, br#"{"result":{}}"#);
        let stats = cache.stats().await;
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.stores, 1);
    }

    #[tokio::test]
    async fn ledger_increment_invalidates_older_and_unpinned_entries() {
        let cache = SimulationCache::new(SimulationCacheConfig::default());
        let old = key_for(&sim_request(Some(100), &[1]));
        let unpinned = key_for(&sim_request(None, &[2]));
        cache.insert(old.clone(), b"old".to_vec()).await;
        cache.insert(unpinned.clone(), b"unpinned".to_vec()).await;

        cache.on_ledger_increment(101).await;

        assert!(
            cache.get(&old).await.is_none(),
            "old-sequence entry must be dropped"
        );
        assert!(
            cache.get(&unpinned).await.is_none(),
            "unpinned entry must be dropped"
        );

        // A request pinned AT the new ledger is still serviceable if filled
        // after the increment.
        let fresh = key_for(&sim_request(Some(101), &[3]));
        cache.insert(fresh.clone(), b"fresh".to_vec()).await;
        assert!(cache.get(&fresh).await.is_some());
    }

    #[tokio::test]
    async fn high_water_mark_blocks_cross_boundary_hits() {
        let cache = SimulationCache::new(SimulationCacheConfig::default());
        let key = key_for(&sim_request(Some(100), &[1]));
        cache.insert(key.clone(), b"stale".to_vec()).await;
        cache.on_ledger_increment(101).await;
        // Re-inserting the same key directly (simulating a fill that raced
        // the invalidation through a second replica) must still miss:
        // seq < high-water.
        cache.insert(key.clone(), b"reinserted".to_vec()).await;
        assert!(cache.get(&key).await.is_none());
    }

    #[tokio::test]
    async fn lru_eviction_respects_capacity() {
        let cache = SimulationCache::new(SimulationCacheConfig {
            max_entries: 2,
            ..SimulationCacheConfig::default()
        });
        for seq in 0..4u64 {
            let key = key_for(&sim_request(Some(1000 + seq), &[seq]));
            cache.insert(key, b"x".to_vec()).await;
        }
        assert_eq!(cache.len().await, 2);
    }

    #[tokio::test]
    async fn ledger_bump_between_identical_requests_misses_then_hits() {
        // The end-to-end behavior the issue cares about: the same request at
        // ledger N hits; after the ledger increments, the identical request
        // misses once and repopulates against the new ledger.
        let cache = SimulationCache::new(SimulationCacheConfig::default());
        let key = key_for(&sim_request(Some(7), &[9]));
        cache.insert(key.clone(), b"ledger7".to_vec()).await;
        assert!(cache.get(&key).await.is_some());

        cache.on_ledger_increment(8).await;
        assert!(
            cache.get(&key).await.is_none(),
            "stale across ledger boundary"
        );

        let fresh_key = key_for(&sim_request(Some(8), &[9]));
        cache.insert(fresh_key.clone(), b"ledger8".to_vec()).await;
        assert!(cache.get(&fresh_key).await.is_some());
        let stats = cache.stats().await;
        assert!(stats.invalidations >= 1);
    }

    #[test]
    fn encoded_keys_are_redis_safe() {
        let key = key_for(&sim_request(Some(100), &[1]));
        let encoded = key.encoded();
        assert!(encoded.starts_with("simcache:simulateTransaction:"));
        assert!(!encoded.contains(' '));
        assert!(encoded.len() < 256, "redis key length budget");
    }

    #[test]
    fn resp_encoding_matches_expected_wire_format() {
        assert_eq!(
            encode_sim_command(&["GET", "k"]),
            b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n".to_vec()
        );
        assert_eq!(
            encode_sim_command(&["GET", "a\r\nb"]),
            b"*2\r\n$3\r\nGET\r\n$4\r\na\r\nb\r\n".to_vec()
        );
    }

    /// Minimal RESP server that accepts ONE connection and answers each
    /// sequential command with the next scripted reply. One connection is
    /// enough because the store pools and reuses it across commands.
    async fn stub_redis(replies: Vec<&'static str>) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            for reply in replies {
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0, "client disconnected before all replies were served");
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (address, handle)
    }

    #[tokio::test]
    async fn redis_get_set_roundtrip_via_resp() {
        let (addr, handle) = stub_redis(vec![
            "+OK\r\n",                    // SET
            "$13\r\n{\"result\":{}}\r\n", // GET (13-byte bulk)
        ])
        .await;
        let store = RedisSimCacheStore::new(RedisSimCacheConfig {
            address: addr,
            ..RedisSimCacheConfig::default()
        });
        let key = key_for(&sim_request(Some(5), &[1]));
        store.set(&key, b"{\"result\":{}}").await;
        let hit = store.get(&key).await.expect("redis hit");
        assert_eq!(hit, b"{\"result\":{}}");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn redis_miss_returns_none_without_error() {
        let (addr, handle) = stub_redis(vec!["$-1\r\n"]).await;
        let store = RedisSimCacheStore::new(RedisSimCacheConfig {
            address: addr,
            ..RedisSimCacheConfig::default()
        });
        let key = key_for(&sim_request(Some(5), &[2]));
        assert!(store.get(&key).await.is_none());
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn redis_unreachable_fails_open_as_none() {
        // Port 1 on localhost is unroutable; connect must time out fast.
        let store = RedisSimCacheStore::new(RedisSimCacheConfig {
            address: "127.0.0.1:1".to_string(),
            timeout: Duration::from_millis(100),
            ..RedisSimCacheConfig::default()
        });
        let key = key_for(&sim_request(Some(5), &[3]));
        assert!(store.get(&key).await.is_none());
        store.set(&key, b"ignored").await; // must not panic either
    }
}
