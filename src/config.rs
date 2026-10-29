use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub kafka: KafkaConfig,
    pub scp_stream: ScpStreamConfig,
    pub key_rotation: KeyRotationConfig,
    pub wasm_pool: WasmPoolConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            kafka: KafkaConfig::default(),
            scp_stream: ScpStreamConfig::default(),
            key_rotation: KeyRotationConfig::default(),
            wasm_pool: WasmPoolConfig::default(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut config = Config::default();
        if let Ok(v) = std::env::var("KAFKA_BROKERS") {
            config.kafka.brokers = v;
        }
        if let Ok(v) = std::env::var("KAFKA_TOPIC") {
            config.kafka.topic = v;
        }
        if let Ok(v) = std::env::var("KAFKA_GROUP_ID") {
            config.kafka.group_id = v;
        }
        if let Ok(v) = std::env::var("KAFKA_PARTITIONING") {
            config.kafka.partitioning = match v.as_str() {
                "dynamic" => PartitionMode::Dynamic,
                _ => PartitionMode::Single,
            };
        }
        if let Ok(v) = std::env::var("KAFKA_METADATA_REFRESH_INTERVAL") {
            config.kafka.metadata_refresh_interval_secs = v.parse().unwrap_or(15);
        }
        if let Ok(v) = std::env::var("SCP_HASH_SEED") {
            config.scp_stream.hash_seed = v.parse().unwrap_or(0);
        }
        if let Ok(v) = std::env::var("SCP_BUFFER_SIZE") {
            config.scp_stream.buffer_size = v.parse().unwrap_or(100_000);
        }
        if let Ok(v) = std::env::var("SCP_OVERFLOW_STRATEGY") {
            config.scp_stream.overflow_strategy = match v.as_str() {
                "backpressure" => OverflowStrategy::Backpressure,
                _ => OverflowStrategy::DropOldest,
            };
        }
        if let Ok(v) = std::env::var("SCP_DRAIN_BATCH_SIZE") {
            config.scp_stream.drain_batch_size = v.parse().unwrap_or(256);
        }
        if let Ok(v) = std::env::var("KEY_ROTATION_ENABLED") {
            config.key_rotation.enabled = matches!(v.as_str(), "1" | "true" | "TRUE" | "yes");
        }
        if let Ok(v) = std::env::var("KEY_ROTATION_ESO_BACKEND") {
            config.key_rotation.eso_backend = match v.as_str() {
                "vault" => EsoBackend::Vault,
                _ => EsoBackend::AwsSecretsManager,
            };
        }
        if let Ok(v) = std::env::var("KEY_ROTATION_INTERVAL_SECS") {
            config.key_rotation.rotation_interval_secs = v.parse().unwrap_or(30 * 24 * 60 * 60);
        }
        if let Ok(v) = std::env::var("KEY_ROTATION_STAGGER_DELAY_SECS") {
            config.key_rotation.stagger_delay_secs = v.parse().unwrap_or(300);
        }
        if let Ok(v) = std::env::var("KEY_ROTATION_MIN_QUORUM") {
            config.key_rotation.min_quorum = v.parse().unwrap_or(3);
        }
        if let Ok(v) = std::env::var("WASM_POOL_SIZE") {
            config.wasm_pool.pool_size = v.parse().unwrap_or(4);
        }
        if let Ok(v) = std::env::var("WASM_POOL_MAX_IDLE") {
            config.wasm_pool.max_idle = v.parse().unwrap_or(4);
        }
        if let Ok(v) = std::env::var("WASM_POOL_SCRUB_ON_RETURN") {
            config.wasm_pool.scrub_on_return = matches!(v.as_str(), "1" | "true" | "TRUE" | "yes");
        }
        config
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KafkaConfig {
    pub brokers: String,
    pub topic: String,
    pub group_id: String,
    pub num_partitions: usize,
    pub partitioning: PartitionMode,
    pub metadata_refresh_interval_secs: u64,
}

impl Default for KafkaConfig {
    fn default() -> Self {
        Self {
            brokers: "localhost:9092".to_string(),
            topic: "scp-telemetry".to_string(),
            group_id: "scp-analytics".to_string(),
            num_partitions: 1,
            partitioning: PartitionMode::Single,
            metadata_refresh_interval_secs: 15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionMode {
    Single,
    Dynamic,
}

impl Default for PartitionMode {
    fn default() -> Self {
        PartitionMode::Single
    }
}

/// Buffer overflow strategy when the ring-buffer is full.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverflowStrategy {
    /// Silently drop the oldest unread message and insert the new one.
    DropOldest,
    /// Signal backpressure to the caller (write returns `false`).
    Backpressure,
}

impl Default for OverflowStrategy {
    fn default() -> Self {
        OverflowStrategy::DropOldest
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScpStreamConfig {
    pub hash_seed: u64,
    /// Capacity of the lock-free ring-buffer (number of messages).
    pub buffer_size: usize,
    /// What to do when the buffer is full.
    pub overflow_strategy: OverflowStrategy,
    /// Number of messages drained per micro-batch iteration.
    pub drain_batch_size: usize,
}

impl Default for ScpStreamConfig {
    fn default() -> Self {
        Self {
            hash_seed: 0xD1B5A32D,
            buffer_size: 100_000,
            overflow_strategy: OverflowStrategy::DropOldest,
            drain_batch_size: 256,
        }
    }
}

/// External secret store backing the validator key rotation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EsoBackend {
    /// AWS Secrets Manager via the External Secrets Operator.
    AwsSecretsManager,
    /// HashiCorp Vault via the External Secrets Operator.
    Vault,
}

impl Default for EsoBackend {
    fn default() -> Self {
        EsoBackend::AwsSecretsManager
    }
}

/// Configuration for automated validator node identity rotation.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeyRotationConfig {
    /// Master switch; rotation is disabled unless explicitly enabled.
    pub enabled: bool,
    /// Which external secret store to pull ed25519 identities from.
    pub eso_backend: EsoBackend,
    /// Interval between rotations (default 30 days).
    pub rotation_interval_secs: u64,
    /// Delay between rotating successive nodes to preserve quorum.
    pub stagger_delay_secs: u64,
    /// Minimum number of healthy nodes that must remain online during rotation.
    pub min_quorum: usize,
}

impl Default for KeyRotationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            eso_backend: EsoBackend::AwsSecretsManager,
            rotation_interval_secs: 30 * 24 * 60 * 60,
            stagger_delay_secs: 300,
            min_quorum: 3,
        }
    }
}

/// Configuration for the warm WASM execution sandbox pool.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WasmPoolConfig {
    /// Number of pre-initialized sandboxes kept warm.
    pub pool_size: usize,
    /// Maximum number of idle sandboxes retained in the pool.
    pub max_idle: usize,
    /// Whether linear memory is scrubbed before a sandbox returns to the pool.
    pub scrub_on_return: bool,
}

impl Default for WasmPoolConfig {
    fn default() -> Self {
        Self {
            pool_size: 4,
            max_idle: 4,
            scrub_on_return: true,
        }
    }
}
