use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Represents the persisted state of a Horizon ingestion worker that is tied to
/// a validator identity. When a validator seed is rotated, the ingestion worker
/// must be updated so it continues to track the correct public key and cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HorizonIngestionState {
    /// The validator public key (ed25519, hex-encoded) the worker is tracking.
    pub validator_public_key: String,
    /// The ledger cursor the ingestion worker has advanced to.
    pub ledger_cursor: String,
    /// The paging token / cursor used for the ingestion stream.
    pub paging_token: String,
}

/// A minimal abstraction over the Horizon ingestion database. In production this
/// would be backed by the Horizon Postgres database; here we keep an in-memory
/// map keyed by ingestion worker id so the controller logic can be exercised and
/// wired into the rotation callback.
pub trait HorizonIngestionStore: Send + Sync {
    fn get(&self, worker_id: &str) -> Option<HorizonIngestionState>;
    fn put(&self, worker_id: &str, state: HorizonIngestionState);
    fn list_workers(&self) -> Vec<String>;
}

/// Default in-memory implementation of [`HorizonIngestionStore`].
#[derive(Default)]
pub struct InMemoryHorizonIngestionStore {
    inner: Mutex<HashMap<String, HorizonIngestionState>>,
}

impl InMemoryHorizonIngestionStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl HorizonIngestionStore for InMemoryHorizonIngestionStore {
    fn get(&self, worker_id: &str) -> Option<HorizonIngestionState> {
        self.inner
            .lock()
            .expect("horizon ingestion store poisoned")
            .get(worker_id)
            .cloned()
    }

    fn put(&self, worker_id: &str, state: HorizonIngestionState) {
        self.inner
            .lock()
            .expect("horizon ingestion store poisoned")
            .insert(worker_id.to_string(), state);
    }

    fn list_workers(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("horizon ingestion store poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

pub struct HorizonMigrationController {
    store: Arc<dyn HorizonIngestionStore>,
}

impl HorizonMigrationController {
    /// Create a controller backed by the provided ingestion store.
    pub fn new(store: Arc<dyn HorizonIngestionStore>) -> Self {
        Self { store }
    }

    /// Create a controller backed by a fresh in-memory ingestion store.
    pub fn with_in_memory_store() -> Self {
        Self::new(Arc::new(InMemoryHorizonIngestionStore::new()))
    }

    /// Update the Horizon ingestion worker's stored validator public key (and
    /// related cursors) after a validator seed rotation.
    ///
    /// The ledger cursor and paging token are preserved so that ingestion
    /// continues from the same ledger position; only the identity binding is
    /// changed to reflect the rotated validator key. Returns the number of
    /// ingestion workers that were updated.
    pub fn update_ingestion_validator_key(
        &self,
        old_public_key: &str,
        new_public_key: &str,
    ) -> usize {
        let mut updated = 0usize;
        for worker_id in self.store.list_workers() {
            if let Some(mut state) = self.store.get(&worker_id) {
                if state.validator_public_key == old_public_key {
                    state.validator_public_key = new_public_key.to_string();
                    self.store.put(&worker_id, state);
                    updated += 1;
                }
            }
        }
        updated
    }

    /// Rotation completion callback. Invoked by the security rotation
    /// controller once a validator seed has been successfully rotated. It
    /// propagates the new identity to any attached Horizon ingestion workers.
    pub fn on_rotation_complete(&self, old_public_key: &str, new_public_key: &str) -> usize {
        self.update_ingestion_validator_key(old_public_key, new_public_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(key: &str) -> HorizonIngestionState {
        HorizonIngestionState {
            validator_public_key: key.to_string(),
            ledger_cursor: "12345".to_string(),
            paging_token: "67890".to_string(),
        }
    }

    #[test]
    fn updates_matching_workers_and_preserves_cursors() {
        let store = Arc::new(InMemoryHorizonIngestionStore::new());
        store.put("worker-a", state("OLDKEY"));
        store.put("worker-b", state("OTHERKEY"));

        let controller = HorizonMigrationController::new(store.clone());
        let updated = controller.on_rotation_complete("OLDKEY", "NEWKEY");

        assert_eq!(updated, 1);
        let a = store.get("worker-a").unwrap();
        assert_eq!(a.validator_public_key, "NEWKEY");
        assert_eq!(a.ledger_cursor, "12345");
        assert_eq!(a.paging_token, "67890");

        let b = store.get("worker-b").unwrap();
        assert_eq!(b.validator_public_key, "OTHERKEY");
    }

    #[test]
    fn no_matching_workers_returns_zero() {
        let store = Arc::new(InMemoryHorizonIngestionStore::new());
        store.put("worker-a", state("OTHERKEY"));
        let controller = HorizonMigrationController::new(store);
        assert_eq!(controller.on_rotation_complete("OLDKEY", "NEWKEY"), 0);
    }
}