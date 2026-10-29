//! Conservative local pruning for Stellar Core bucket storage.
//!
//! Bucket filenames are content-addressed and do not encode their ledger
//! coverage. The pruner therefore deletes bucket files only when a complete
//! Core safety snapshot explicitly marks them obsolete. Ledger XDR files are
//! selected by ledger sequence and retention policy. Any file still required
//! by Core, quorum state, catchup, an attached Horizon node, or an active query
//! must be included in the snapshot's protected paths.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use super::fs::{remove_if_unlocked, scan_bucket_dir, StorageFileKind};

/// Approximate number of ledgers closed in one day.
const LEDGERS_PER_DAY: u32 = 17_280;

/// Default retention window used by the local pruner.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

/// Configuration used by the local bucket pruner.
#[derive(Clone, Debug)]
pub struct PrunerConfig {
    /// Stellar Core bucket directory from `BUCKET_DIR_PATH`.
    pub bucket_dir: PathBuf,
    /// Minimum amount of recent history to retain.
    pub retention_days: u32,
    /// When true, report candidates without deleting them.
    pub dry_run: bool,
}

impl PrunerConfig {
    /// Resolves `BUCKET_DIR_PATH` using the default 30-day retention window.
    pub async fn from_core_config_default(
        core_config_path: impl AsRef<Path>,
        dry_run: bool,
    ) -> Result<Self> {
        Self::from_core_config(core_config_path, DEFAULT_RETENTION_DAYS, dry_run).await
    }

    /// Resolves `BUCKET_DIR_PATH` from a Stellar Core configuration file.
    ///
    /// Relative bucket paths are resolved relative to the Core config file.
    pub async fn from_core_config(
        core_config_path: impl AsRef<Path>,
        retention_days: u32,
        dry_run: bool,
    ) -> Result<Self> {
        if retention_days == 0 {
            bail!("retention_days must be greater than zero");
        }

        let core_config_path = core_config_path.as_ref();
        let contents = tokio::fs::read_to_string(core_config_path)
            .await
            .with_context(|| {
                format!(
                    "failed to read Stellar Core config {}",
                    core_config_path.display()
                )
            })?;
        let core_config: CoreConfig =
            toml::from_str(&contents).context("failed to parse Stellar Core config as TOML")?;
        let bucket_dir = PathBuf::from(core_config.bucket_dir_path);
        let bucket_dir = if bucket_dir.is_absolute() {
            bucket_dir
        } else {
            core_config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(bucket_dir)
        };

        Ok(Self {
            bucket_dir,
            retention_days,
            dry_run,
        })
    }

    fn retention_ledgers(&self) -> u32 {
        self.retention_days.saturating_mul(LEDGERS_PER_DAY)
    }
}

#[derive(Deserialize)]
struct CoreConfig {
    #[serde(rename = "BUCKET_DIR_PATH")]
    bucket_dir_path: String,
}

/// Required, caller-provided view of files and ledgers still in use.
///
/// `inventory_complete` must be true only after obtaining a fresh inventory
/// that includes Core's live bucket list, current quorum state, pending
/// catchup work, every attached Horizon node's retention/catchup requirements,
/// and files with active queries. Missing or incomplete inventory aborts
/// pruning.
#[derive(Clone, Debug)]
pub struct SafetySnapshot {
    /// Latest ledger known to Core.
    pub current_ledger: u32,
    /// Oldest ledger that must remain available to Core/Horizon.
    pub minimum_required_ledger: u32,
    /// Paths which must never be removed (including quorum, catchup, and
    /// active-query files).
    pub protected_files: HashSet<PathBuf>,
    /// Bucket files Core has authoritatively confirmed are no longer needed.
    pub obsolete_bucket_files: HashSet<PathBuf>,
    /// Indicates that all required safety inventories were successfully read.
    pub inventory_complete: bool,
}

/// Summary from one pruning pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Candidate file count.
    pub eligible_files: usize,
    /// Number of files actually removed.
    pub deleted_files: usize,
    /// Files skipped because an active reader held a shared lock.
    pub locked_files: Vec<PathBuf>,
    /// Bytes removed.
    pub bytes_freed: u64,
    /// Whether deletion was disabled.
    pub dry_run: bool,
}

/// Local bucket-storage pruner.
#[derive(Clone, Debug)]
pub struct Pruner {
    config: PrunerConfig,
}

impl Pruner {
    /// Creates a pruner after validating its configuration.
    pub fn new(config: PrunerConfig) -> Result<Self> {
        if config.retention_days == 0 {
            bail!("retention_days must be greater than zero");
        }
        Ok(Self { config })
    }

    /// Scans and prunes according to the supplied, current safety snapshot.
    pub async fn prune_once(&self, snapshot: &SafetySnapshot) -> Result<PruneReport> {
        if !snapshot.inventory_complete {
            bail!("refusing to prune without a complete Core/Horizon safety inventory");
        }
        if snapshot.minimum_required_ledger > snapshot.current_ledger {
            bail!("minimum_required_ledger cannot exceed current_ledger");
        }

        let root = tokio::fs::canonicalize(&self.config.bucket_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to resolve bucket directory {}",
                    self.config.bucket_dir.display()
                )
            })?;
        let files = scan_bucket_dir(&root).await?;
        let protected_files = canonicalize_paths(&root, &snapshot.protected_files).await?;
        let obsolete_buckets = canonicalize_paths(&root, &snapshot.obsolete_bucket_files).await?;
        let retention_floor = snapshot
            .current_ledger
            .saturating_sub(self.config.retention_ledgers())
            .min(snapshot.minimum_required_ledger);

        let mut candidates = Vec::new();
        for file in files {
            if protected_files.contains(&file.path) {
                continue;
            }

            let obsolete = match file.kind {
                StorageFileKind::LedgerXdr { ledger } => ledger < retention_floor,
                StorageFileKind::Bucket => obsolete_buckets.contains(&file.path),
            };
            if obsolete {
                candidates.push(file);
            }
        }

        let mut report = PruneReport {
            eligible_files: candidates.len(),
            dry_run: self.config.dry_run,
            ..PruneReport::default()
        };

        for candidate in candidates {
            if self.config.dry_run {
                continue;
            }
            match remove_if_unlocked(&candidate.path)
                .await
                .with_context(|| format!("failed to remove {}", candidate.path.display()))?
            {
                true => {
                    report.deleted_files += 1;
                    report.bytes_freed = report.bytes_freed.saturating_add(candidate.size_bytes);
                    info!(path = %candidate.path.display(), bytes = candidate.size_bytes, "pruned obsolete Stellar storage file");
                }
                false => {
                    warn!(path = %candidate.path.display(), "preserving actively locked Stellar storage file");
                    report.locked_files.push(candidate.path);
                }
            }
        }

        Ok(report)
    }

    /// Runs pruning periodically until the task is cancelled or an error occurs.
    ///
    /// The snapshot provider must query fresh Core and attached-Horizon state on
    /// every iteration. Aborting the returned task stops the daemon.
    pub async fn run_periodically<F, Fut>(
        &self,
        period: Duration,
        mut snapshot_provider: F,
    ) -> Result<()>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<SafetySnapshot>>,
    {
        if period.is_zero() {
            bail!("pruning period must be greater than zero");
        }

        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            let snapshot = snapshot_provider().await?;
            self.prune_once(&snapshot).await?;
        }
    }
}

async fn canonicalize_paths(root: &Path, paths: &HashSet<PathBuf>) -> Result<HashSet<PathBuf>> {
    let mut canonical_paths = HashSet::with_capacity(paths.len());
    for path in paths {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        };
        let canonical = tokio::fs::canonicalize(&path)
            .await
            .with_context(|| format!("safety inventory path does not exist: {}", path.display()))?;
        if !canonical.starts_with(root) {
            bail!(
                "safety inventory path escaped bucket directory: {}",
                canonical.display()
            );
        }
        canonical_paths.insert(canonical);
    }
    Ok(canonical_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintenance::fs::FileReadGuard;
    use std::fs;
    use tempfile::tempdir;

    fn complete_snapshot(
        current_ledger: u32,
        minimum_required_ledger: u32,
        protected_files: HashSet<PathBuf>,
        obsolete_bucket_files: HashSet<PathBuf>,
    ) -> SafetySnapshot {
        SafetySnapshot {
            current_ledger,
            minimum_required_ledger,
            protected_files,
            obsolete_bucket_files,
            inventory_complete: true,
        }
    }

    #[tokio::test]
    async fn resolves_bucket_path_from_core_config() {
        let temp = tempdir().unwrap();
        let config_path = temp.path().join("stellar-core.cfg");
        fs::write(&config_path, "BUCKET_DIR_PATH = \"data/buckets\"\n").unwrap();

        let config = PrunerConfig::from_core_config(&config_path, 30, true)
            .await
            .unwrap();

        assert_eq!(config.bucket_dir, temp.path().join("data/buckets"));
        assert_eq!(config.retention_days, 30);
        assert!(config.dry_run);
    }

    #[tokio::test]
    async fn defaults_to_thirty_day_retention() {
        let temp = tempdir().unwrap();
        let config_path = temp.path().join("stellar-core.cfg");
        fs::write(&config_path, "BUCKET_DIR_PATH = \"buckets\"\n").unwrap();

        let config = PrunerConfig::from_core_config_default(&config_path, true)
            .await
            .unwrap();

        assert_eq!(config.retention_days, DEFAULT_RETENTION_DAYS);
    }

    #[tokio::test]
    async fn dry_run_reports_old_ledger_files_without_deleting() {
        let temp = tempdir().unwrap();
        let buckets = temp.path().join("buckets");
        fs::create_dir_all(&buckets).unwrap();
        let old = buckets.join("ledger-1.xdr");
        fs::write(&old, b"old ledger").unwrap();
        let recent = buckets.join("ledger-600000.xdr");
        fs::write(&recent, b"recent ledger").unwrap();
        let pruner = Pruner::new(PrunerConfig {
            bucket_dir: buckets,
            retention_days: 1,
            dry_run: true,
        })
        .unwrap();

        let report = pruner
            .prune_once(&complete_snapshot(
                600_000,
                200_000,
                HashSet::new(),
                HashSet::new(),
            ))
            .await
            .unwrap();

        assert_eq!(report.eligible_files, 1);
        assert_eq!(report.deleted_files, 0);
        assert!(old.exists());
        assert!(recent.exists());
    }

    #[tokio::test]
    async fn only_core_confirmed_obsolete_buckets_can_be_deleted() {
        let temp = tempdir().unwrap();
        let buckets = temp.path().join("buckets");
        fs::create_dir_all(&buckets).unwrap();
        let obsolete = buckets.join("bucket-obsolete.xdr");
        let unclassified = buckets.join("bucket-current.xdr");
        fs::write(&obsolete, b"obsolete").unwrap();
        fs::write(&unclassified, b"current").unwrap();
        let pruner = Pruner::new(PrunerConfig {
            bucket_dir: buckets,
            retention_days: 30,
            dry_run: false,
        })
        .unwrap();

        let report = pruner
            .prune_once(&complete_snapshot(
                1_000_000,
                1,
                HashSet::new(),
                HashSet::from([obsolete.clone()]),
            ))
            .await
            .unwrap();

        assert_eq!(report.deleted_files, 1);
        assert!(!obsolete.exists());
        assert!(unclassified.exists());
    }

    #[tokio::test]
    async fn protected_files_are_never_deleted() {
        let temp = tempdir().unwrap();
        let buckets = temp.path().join("buckets");
        fs::create_dir_all(&buckets).unwrap();
        let protected = buckets.join("ledger-1.xdr");
        fs::write(&protected, b"must retain").unwrap();
        let pruner = Pruner::new(PrunerConfig {
            bucket_dir: buckets,
            retention_days: 1,
            dry_run: false,
        })
        .unwrap();

        let report = pruner
            .prune_once(&complete_snapshot(
                600_000,
                200_000,
                HashSet::from([protected.clone()]),
                HashSet::new(),
            ))
            .await
            .unwrap();

        assert_eq!(report.eligible_files, 0);
        assert!(protected.exists());
    }

    #[tokio::test]
    async fn retains_exactly_the_configured_thirty_day_ledger_window() {
        let temp = tempdir().unwrap();
        let buckets = temp.path().join("buckets");
        fs::create_dir_all(&buckets).unwrap();
        let older = buckets.join("ledger-481599.xdr");
        let boundary = buckets.join("ledger-481600.xdr");
        fs::write(&older, b"older").unwrap();
        fs::write(&boundary, b"boundary").unwrap();
        let pruner = Pruner::new(PrunerConfig {
            bucket_dir: buckets,
            retention_days: 30,
            dry_run: true,
        })
        .unwrap();

        let report = pruner
            .prune_once(&complete_snapshot(
                1_000_000,
                481_600,
                HashSet::new(),
                HashSet::new(),
            ))
            .await
            .unwrap();

        assert_eq!(report.eligible_files, 1);
        assert!(older.exists());
        assert!(boundary.exists());
    }

    #[tokio::test]
    async fn active_query_lock_is_preserved_and_reported() {
        let temp = tempdir().unwrap();
        let buckets = temp.path().join("buckets");
        fs::create_dir_all(&buckets).unwrap();
        let active = buckets.join("bucket-active.xdr");
        fs::write(&active, b"active query").unwrap();
        let _guard = FileReadGuard::open(&active).unwrap();
        let pruner = Pruner::new(PrunerConfig {
            bucket_dir: buckets,
            retention_days: 30,
            dry_run: false,
        })
        .unwrap();

        let report = pruner
            .prune_once(&complete_snapshot(
                1_000_000,
                1,
                HashSet::new(),
                HashSet::from([active.clone()]),
            ))
            .await
            .unwrap();

        assert_eq!(
            report.locked_files,
            vec![fs::canonicalize(&active).unwrap()]
        );
        assert!(active.exists());
    }

    #[tokio::test]
    async fn refuses_incomplete_safety_inventory() {
        let temp = tempdir().unwrap();
        let pruner = Pruner::new(PrunerConfig {
            bucket_dir: temp.path().to_path_buf(),
            retention_days: 30,
            dry_run: false,
        })
        .unwrap();

        let error = pruner
            .prune_once(&SafetySnapshot {
                current_ledger: 100,
                minimum_required_ledger: 10,
                protected_files: HashSet::new(),
                obsolete_bucket_files: HashSet::new(),
                inventory_complete: false,
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("complete"));
    }
}
