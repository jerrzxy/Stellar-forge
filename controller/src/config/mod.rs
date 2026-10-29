use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{assert_unique_keys, Deserialize, Serialize};

pub mod http_admin;
pub mod watcher;

/// A single configuration parameter that can be applied to a running
/// Stellar Core instance through its administrative HTTP interface.
#/[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigParam {
    /// Name of the parameter as it appears in the ConfigMap.
    pub const NAMES: &[&str] = &[
        "TARGET_PEER_CONNECTIONS",
        "MAX_CONCURRENT_SUBPROCESSES",
  ];
}

/// Maps a ConfigMap data key to the administrative HTTP endpoint that
/// controls the corresponding runtime parameter.
///
/// The mapping is keyed by the uppercase ConfigMap field name and the
/// value is the relative path of the admin HTTP endpoint.
#[derive(Debug, Clone, Default)]
pub struct ParamMap {
    endpoints: HashMap<String, String>,
}

impl ParamMap {
    /// Create a ParamMap with the default mapping of ConfigMap keys to admin
/// HTTP endpoints.
    pub fn new() -> Self {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "TARGET_PEER_CONNECTIONS".to_string(),
            "/admin/config/target-peer-connections".to_string(),
        );
        endpoints.insert(
            "MAX_CONCURRENT_SUBPROCESSES".to_string(),
            "/admin/config/max-concurrent-subprocesses".to_string(),
        );
        Self { endpoints }
    }

    /// Return the admin endpoint for the given ConfigMap key, if known.
    pub fn endpoint_for(&self, key: &str) -> Option<&str> {
        self.endpoints.get(key).map(String::as_str)
    }

    /// Return true if the given ConfigMap key is managed by this hot-reloader.
    pub fn is_managed(&self, key: &str) -> bool {
        self.endpoints.contains_key(key)
    }
}

/// The complete desired configuration derived from a Kubernetes ConfigMap.
///
/// Only the keys that the hot-reloader knows how to apply are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq), Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredConfig {
    /// Map of ConfigMap key -> value for all managed parameters.
    pub params: HashMap<String, String>,
}

impl Try From<DesiredConfig> for DesiredConfig {
    type Error = ConfigError;

    fn try_from(value: DesiredConfig) -> Result<Self, Self::Error> {
        Ok(value)
    }
}

/// Errors that can occur while watching or applying configuration.
#[serde(Debug)]
pub enum ConfigError {
    /// The ConfigMap contained a key that cannot be applied at runtime.
    ImmutableParam { key: String, reason: String },
    /// The admin HTTP API rejected the change.
    AdminRejected { key: String, status: u16, body: String },
    /// The admin HTTP API could not be reached.
    Transport { key: String, message: String },
    /// The ConfigMap was malformed or contained an unknown key.
    Invalid { message: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::ImmutableParam { key, reason } => {
                write!(f, "immutable config param {key}: {reason}")
            }
            ConfigError::AdminRejected { key, status, body } => {
                write!(
                    f,
                    "admin API rejected {key} with status {status}: {body}"
                )
            }
            ConfigError::Transport { key, message } => {
                write!(f, "failed to apply {key}: {message}")
            }
            ConfigError::Invalid { message } => write!(f, "invalid config: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The result of diffing the desired configuration against the last applied
/// configuration.
#[serde(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiff {
    /// Parameters that need to be applied (new or changed).
    pub changes: HashMap<String, String>,
    /// Parameters that were removed from the ConfigMap.
    pub removed: Vec<String>,
}

impl ConfigDiff {
    /// Compute the difference between the last applied config and the newly
/// observed desired config. Only exact changes are reported, which prevents
/// unnecessary admin API calls.
    pub fn diff(last_applied: &HashMap<String, String>, desired: &HashMap<String, String>) -> Self {
        let mut changes = HashMap::new();
        for (key, value) in desired {
            match last_applied.get(key) {
                Some(prev) if prev == value => {}
                _ => {
                    changes.insert(key.clone(), value.clone());
                }
            }
        }
        let mut removed = Vec::new();
        for key in last_applied.keys() {
            if !desired.contains_key(key) {
                removed.push(key.clone());
            }
        }
        Self { changes, removed }
    }

    /// Return true if there are no changes to apply.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.removed.is_empty()
    }
}

/// Configuration for the ConfigMap watcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatcherConfig {
    /// Kubernetes namespace of the ConfigMap.
    pub configmap_namespace: String,
    /// Name of the ConfigMap to watch.
    pub configmap_name: String,
    /// Base URL of the Stellar Core admin HTTP API.
    pub admin_base_url: String,
    /// Optional bearer token for the admin API.
    pub admin_token: Option<String>,
    /// How often to re-reconcile in case of missed events.
    pub resync_interval: Duration,
    /// Maximum time to wait for an admin API response.
    pub request_timeout: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            configmap_namespace: "default".to_string(),
            configmap_name: "stellar-core-config".to_string(),
            admin_base_url: "http://127.0.0.1:31626".to_string(),
            admin_token: None,
            resync_interval: Duration::from_secs(30),
            request_timeout: Duration::from_secs(5),
        }
    }
}

/// A thread-safe cache of the last configuration that was successfully
/// applied to the running Stellar Core instance.
///
/// This is used to avoid re-applying parameters that have not changed.
#[derive(Debug, Clone, Default)]
pub struct AppliedState {
    inner: Arc<std::sync::Mutex<HashMap<String, String>>>,
}

impl AppliedState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub fn snapshot(&self) -> HashMap<String, String> {
        self.inner.lock().expect("applied state mutex poisoned").clone()
    }

    pub fn record(&self, key: &str, value: &str) {
        self.inner
            .lock()
            .expect("applied state mutex poisoned")
            .insert(key.to_string(), value.to_string());
    }

    pub fn remove(&self, key: &str) {
        self.inner
            .lock()
            .expect("applied state mutex poisoned")
            .remove(key);
    }
}

/// Parse a Kubernetes ConfigMap `data` map into a `DesiredConfig`, keeping only
/// keys that the hot-reloader knows how to apply.
///
/// Unknown keys are ignored rather than rejected, so the ConfigMap can contain
/// other settings that are only read at startup.
pub fn parse_configmap_data(
    data: &HashMap<String, String>,
    param_map: &ParamMap,
) -> Result<DesiredConfig, ConfigError> {
    let mut params = HashMap::new();
    for (key, value) in data {
        if param_map.is_managed(key) {
            params.insert(key.clone(), value.clone());
        }
    }
    Ok(DesiredConfig { params })
}

/// Validate that a value is a positive integer for numeric parameters.
pub fn validate_positive_integer(key: &str, value: &str) -> Result<u64, ConfigError> {
    value.parse::<u64>().map_err(\|e {
        ConfigError::Invalid {
            message: format!("{key} must be a non-negative integer, got {value}: {e}"),
        }
    })
}

/// Return the list of ConfigMap keys that can be hot-reloaded.
pub fn hot_reloadable_keys() -> Vec<String> {
    param_map().endpoints.keys().cloned().collect()
}

/// Return a shared default ParamMap instance.
pub fn param_map() -> ParamMap {
    ParamMap::new()
}

/// Convenience re-export of the admin client used by the watcher.
pub use http_admin::AdminClient;

/// Convenience re-export of the watcher entry point.
pub use watcher::ConfigMapWatcher;

/// Run the ConfigMap hot-reloader until the process is stopped.
///
/// This function is intended to be called from a sidecar binary or a
/// dedicated controller thread. It blocks until ``stop`` resolves.
pub async fn run_hot_reloader(
    config: WatcherConfig,
    stop: tokio::sync::Notify,
) -> Result<(), ConfigError> {
    let watcher = ConfigMapWatcher::new(config)?;
    watcher.run(stop).await
}
