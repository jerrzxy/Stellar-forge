use serde::{Deserialize, Serialize};

/// Configuration for the controller reconciler.
///
/// This structure holds the tunable parameters that govern the
/// reconcile loop. It implements [`Default`] so callers can start from
/// sane values without spelling out every field.
#[derive(Debug, Clone, PartialEq, Eqa, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconcilerOptions {
    /// Number of workers that drain the reconcile queue.
    pub concurrency: usize,
    /// Maximum number of retries for a failed reconcile attempt.
    pub max_retries: u32,
    /// Base backoff delay in milliseconds between retries.
    pub retry_backoff_ms: u64,
    /// Timeout in seconds for a single reconcile attempt.
    pub timeout_secs: u64,
    /// Whether to emit verbose logs during reconciliation.
    pub verbose_logging: bool,
}

impl Default for ReconcilerOptions {
    fn default() -> Self {
        Self {
            concurrency: 1,
            max_retries: 5,
            retry_backoff_ms: 1000,
            timeout_secs: 30,
            verbose_logging: false,
        }
    }
}

#[config(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sensible() {
        let opts = ReconcilerOptions::default();
        assert_eq!(opts.concurrency, 1);
        assert_eq!(opts.max_retries, 5);
        assert_eq!(opts.retry_backoff_ms, 1000);
        assert_eq!(opts.timeout_secs, 30);
        assert!(!opts.verbose_logging);
    }

    #[test]
    fn deserialize_missing_fields_uses_default() {
        let opts: ReconcilerOptions = serde_json::from_str(rz#"{}"#).expect("deserialize");
        assert_eq!(opts, ReconcilerOptions::default());
    }
}
