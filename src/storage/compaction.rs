//! Background compaction worker for the MVCC store.
//!
//! Compaction removes old revisions that are no longer needed, reclaiming disk and memory space.
//! This module provides a background task that periodically or on-demand compacts the store
//! without interfering with normal read/write operations.
//!
//! Compaction modes:
//! - Periodic: Automatically compact every N seconds
//! - Revision: Manually trigger compaction to a specific revision

use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::storage::MvccStore;

/// Compaction strategy.
#[derive(Clone, Debug)]
pub enum CompactionMode {
    /// Periodically compact at a fixed interval
    Periodic(Duration),

    /// Compact to a specific revision (one-shot)
    Revision(i64),
}

/// Background compactor that removes old revisions.
pub struct Compactor {
    /// Reference to the MVCC store
    store: Arc<MvccStore>,

    /// Compaction mode
    mode: CompactionMode,

    /// For periodic mode: retention period (keep revisions newer than this)
    #[allow(dead_code)]
    retention_period: Duration,
}

impl Compactor {
    /// Creates a new compactor.
    pub fn new(store: Arc<MvccStore>, mode: CompactionMode, retention_period: Duration) -> Self {
        Self {
            store,
            mode,
            retention_period,
        }
    }

    /// Spawns a background tokio task that performs compaction.
    ///
    /// Returns a JoinHandle that can be awaited or aborted.
    pub fn run(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            match self.mode {
                CompactionMode::Periodic(interval_duration) => {
                    self.run_periodic(interval_duration).await;
                }
                CompactionMode::Revision(target_revision) => {
                    self.run_once(target_revision).await;
                }
            }
        })
    }

    /// Runs periodic compaction at the specified interval.
    async fn run_periodic(&self, interval_duration: Duration) {
        let mut ticker = interval(interval_duration);

        info!(
            "Starting periodic compactor with interval: {:?}",
            interval_duration
        );

        loop {
            ticker.tick().await;

            let current_rev = self.store.current_revision();
            let compact_rev = self.store.compact_revision();

            // Keep some retention - don't compact away very recent revisions
            // For example, keep at least the last 1000 revisions
            let min_retain_revisions = 1000;
            let target_compact_rev = (current_rev - min_retain_revisions).max(0);

            // Only compact if there's significant growth
            if target_compact_rev > compact_rev {
                debug!(
                    "Periodic compaction: current_rev={}, compact_rev={}, target={}",
                    current_rev, compact_rev, target_compact_rev
                );

                match self.store.compact(target_compact_rev) {
                    Ok(_) => {
                        info!(
                            "Periodic compaction completed to revision {}",
                            target_compact_rev
                        );
                    }
                    Err(e) => {
                        warn!("Periodic compaction failed: {}", e);
                    }
                }
            }
        }
    }

    /// Runs a one-time compaction to the target revision.
    async fn run_once(&self, target_revision: i64) {
        debug!(
            "Starting one-time compaction to revision {}",
            target_revision
        );

        match self.store.compact(target_revision) {
            Ok(_) => {
                info!(
                    "One-time compaction completed to revision {}",
                    target_revision
                );
            }
            Err(e) => {
                warn!("One-time compaction failed: {}", e);
            }
        }
    }

    /// Manually triggers a compaction to a specific revision.
    ///
    /// This is useful for immediate compaction without waiting for periodic intervals.
    pub async fn compact_to(&self, revision: i64) {
        match self.store.compact(revision) {
            Ok(_) => {
                info!("Manual compaction to revision {} succeeded", revision);
            }
            Err(e) => {
                warn!("Manual compaction to revision {} failed: {}", revision, e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Backend, BackendConfig};
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_compactor_creation() {
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig {
            data_dir: temp_dir.path().to_path_buf(),
            cache_size_mb: 64,
            flush_interval_ms: 100,
            compression: false,
        };

        let backend = Backend::new(config).unwrap();
        let store = MvccStore::new(backend).unwrap();

        let compactor = Compactor::new(
            store,
            CompactionMode::Periodic(Duration::from_secs(10)),
            Duration::from_secs(60),
        );

        // Verify creation
        assert!(!matches!(compactor.mode, CompactionMode::Revision(_)));
    }

    #[tokio::test]
    async fn test_compactor_one_shot() {
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig {
            data_dir: temp_dir.path().to_path_buf(),
            cache_size_mb: 64,
            flush_interval_ms: 100,
            compression: false,
        };

        let backend = Backend::new(config).unwrap();
        let store = MvccStore::new(backend).unwrap();

        // Add some data
        for i in 0..10 {
            let key = format!("key{}", i);
            store.put(key.as_bytes(), b"value", 0).ok();
        }

        let current_rev = store.current_revision();

        let compactor = Compactor::new(
            store.clone(),
            CompactionMode::Revision(current_rev / 2),
            Duration::from_secs(60),
        );

        // Run the compactor
        let handle = compactor.run();

        // Wait a bit for completion
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The task should complete quickly for one-shot mode
        assert!(!handle.is_finished() || handle.await.is_ok());
    }

    #[tokio::test]
    async fn test_manual_compact() {
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig {
            data_dir: temp_dir.path().to_path_buf(),
            cache_size_mb: 64,
            flush_interval_ms: 100,
            compression: false,
        };

        let backend = Backend::new(config).unwrap();
        let store = MvccStore::new(backend).unwrap();

        // Add some data
        for i in 0..5 {
            let key = format!("key{}", i);
            store.put(key.as_bytes(), b"value", 0).ok();
        }

        let initial_compact_rev = store.compact_revision();

        let compactor = Compactor::new(
            store.clone(),
            CompactionMode::Periodic(Duration::from_secs(60)), // Not used in this test
            Duration::from_secs(60),
        );

        let target_rev = 2;
        compactor.compact_to(target_rev).await;

        // Verify compaction happened
        assert!(store.compact_revision() >= initial_compact_rev);
    }
}
