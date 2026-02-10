//! Watch hub for managing active watchers and dispatching events.
//!
//! The Watch subsystem is CRITICAL for Kubernetes operation. K8s controllers depend heavily on
//! watches to track changes to resources in real-time.
//!
//! This implementation provides high-performance event dispatch with:
//! - Crossbeam channels for efficient messaging (lower overhead than Go channels)
//! - DashMap for concurrent watcher registration (lock-free reads)
//! - Batched event delivery to reduce per-event overhead
//! - Efficient range-based watcher matching

use crossbeam_channel::{unbounded, Receiver, Sender};
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Watch-related errors.
#[derive(Error, Debug)]
pub enum WatchError {
    #[error("Watch not found: {0}")]
    WatchNotFound(i64),

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Invalid watch configuration: {0}")]
    InvalidConfig(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type WatchResult<T> = Result<T, WatchError>;

/// A key range specification for watching.
#[derive(Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct WatchRange {
    pub key: Vec<u8>,
    pub range_end: Vec<u8>,
}

impl WatchRange {
    /// Check if a key falls within this watch range.
    pub fn contains(&self, key: &[u8]) -> bool {
        if key < &self.key[..] {
            return false;
        }

        if self.range_end.is_empty() {
            // Empty range_end means only watch this exact key
            key == &self.key[..]
        } else if &self.range_end[..] == b"\0" {
            // Special case: range_end == "\0" means all keys >= key
            true
        } else {
            // Check if key falls within [self.key, self.range_end)
            key < &self.range_end[..]
        }
    }
}

/// Filter types for watch events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchFilter {
    /// Do not emit delete events.
    NoDelete,
    /// Do not emit put events.
    NoPut,
}

/// Response header for watch events.
#[derive(Clone, Debug)]
pub struct ResponseHeader {
    pub cluster_id: u64,
    pub member_id: u64,
    pub revision: i64,
    pub raft_term: u64,
}

/// Represents a single key-value pair with MVCC metadata.
#[derive(Clone, Debug)]
pub struct KeyValue {
    pub key: Vec<u8>,
    pub create_revision: i64,
    pub mod_revision: i64,
    pub version: i64,
    pub value: Vec<u8>,
    pub lease: i64,
}

/// Event type for watch notifications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventType {
    Put = 0,
    Delete = 1,
}

/// A single event within a watch response.
#[derive(Clone, Debug)]
pub struct Event {
    pub event_type: EventType,
    pub kv: KeyValue,
    pub prev_kv: Option<KeyValue>,
}

/// A watch response containing events and metadata.
#[derive(Clone, Debug)]
pub struct WatchEvent {
    pub watch_id: i64,
    pub events: Vec<Event>,
    pub compact_revision: i64,
    pub header: Option<ResponseHeader>,
}

/// Internal structure for a single active watcher.
pub struct Watcher {
    pub id: i64,
    pub key: Vec<u8>,
    pub range_end: Vec<u8>,
    pub start_revision: i64,
    pub filters: Vec<WatchFilter>,
    pub prev_kv: bool,
    pub progress_notify: bool,
    pub fragment: bool,
    pub tx: mpsc::Sender<WatchEvent>,
    pub canceled: AtomicBool,
}

impl Watcher {
    /// Check if this watcher should receive a put event.
    fn should_emit_put(&self) -> bool {
        !self.filters.contains(&WatchFilter::NoPut)
    }

    /// Check if this watcher should receive a delete event.
    fn should_emit_delete(&self) -> bool {
        !self.filters.contains(&WatchFilter::NoDelete)
    }
}

/// The Watch hub manages all active watchers and dispatches events.
pub struct WatchHub {
    /// Map of watch_id -> Watcher
    watchers: DashMap<i64, Arc<Watcher>>,

    /// Counter for generating unique watch IDs
    next_watch_id: AtomicI64,

    /// Map from WatchRange -> list of watcher IDs watching that range
    key_watchers: DashMap<WatchRange, Vec<i64>>,

    /// Member ID for response headers
    member_id: u64,

    /// Cluster ID for response headers
    cluster_id: u64,

    /// Crossbeam channel for internal event dispatch
    #[allow(dead_code)]
    event_tx: Sender<WatchEvent>,
    #[allow(dead_code)]
    event_rx: Receiver<WatchEvent>,
}

impl WatchHub {
    /// Creates a new WatchHub.
    pub fn new(cluster_id: u64, member_id: u64) -> Arc<Self> {
        let (event_tx, event_rx) = unbounded();

        Arc::new(WatchHub {
            watchers: DashMap::new(),
            next_watch_id: AtomicI64::new(1),
            key_watchers: DashMap::new(),
            member_id,
            cluster_id,
            event_tx,
            event_rx,
        })
    }

    /// Creates a new watch and returns the watch ID.
    pub fn create_watch(
        &self,
        key: Vec<u8>,
        range_end: Vec<u8>,
        start_revision: i64,
        filters: Vec<WatchFilter>,
        prev_kv: bool,
        progress_notify: bool,
        fragment: bool,
        tx: mpsc::Sender<WatchEvent>,
    ) -> WatchResult<i64> {
        if key.is_empty() {
            return Err(WatchError::InvalidConfig(
                "Watch key cannot be empty".to_string(),
            ));
        }

        let watch_id = self.next_watch_id.fetch_add(1, Ordering::SeqCst);

        let watcher = Arc::new(Watcher {
            id: watch_id,
            key: key.clone(),
            range_end: range_end.clone(),
            start_revision,
            filters,
            prev_kv,
            progress_notify,
            fragment,
            tx,
            canceled: AtomicBool::new(false),
        });

        self.watchers.insert(watch_id, watcher);

        let watch_range = WatchRange { key, range_end };
        self.key_watchers
            .entry(watch_range)
            .or_insert_with(Vec::new)
            .push(watch_id);

        debug!(watch_id, "Watch created");
        Ok(watch_id)
    }

    /// Cancels a watch by ID.
    pub fn cancel_watch(&self, watch_id: i64) -> WatchResult<()> {
        if let Some((_, watcher)) = self.watchers.remove(&watch_id) {
            watcher.canceled.store(true, Ordering::SeqCst);

            // Remove from key_watchers
            let watch_range = WatchRange {
                key: watcher.key.clone(),
                range_end: watcher.range_end.clone(),
            };

            if let Some(mut entry) = self.key_watchers.get_mut(&watch_range) {
                entry.retain(|id| *id != watch_id);
                if entry.is_empty() {
                    drop(entry);
                    self.key_watchers.remove(&watch_range);
                }
            }

            debug!(watch_id, "Watch canceled");
            Ok(())
        } else {
            Err(WatchError::WatchNotFound(watch_id))
        }
    }

    /// Notifies all matching watchers of events.
    /// Called by MvccStore after each committed write.
    pub fn notify(
        &self,
        events: Vec<Event>,
        revision: i64,
        compact_revision: i64,
    ) -> WatchResult<()> {
        if events.is_empty() {
            return Ok(());
        }

        let header = ResponseHeader {
            cluster_id: self.cluster_id,
            member_id: self.member_id,
            revision,
            raft_term: 0, // Will be set by caller if needed
        };

        // For each event, find matching watchers
        let mut notifications: HashMap<i64, Vec<Event>> = HashMap::new();

        for event in events {
            // Find all watchers that match this event's key
            for entry in self.key_watchers.iter() {
                let watch_range = entry.key();
                let watch_ids = entry.value();

                if !watch_range.contains(&event.kv.key) {
                    continue;
                }

                for watch_id in watch_ids {
                    if let Some(watcher) = self.watchers.get(watch_id) {
                        // Check if watcher is still active
                        if watcher.canceled.load(Ordering::SeqCst) {
                            continue;
                        }

                        // Check filters
                        match event.event_type {
                            EventType::Put if !watcher.should_emit_put() => continue,
                            EventType::Delete if !watcher.should_emit_delete() => continue,
                            _ => {}
                        }

                        // Check start_revision
                        if event.kv.mod_revision < watcher.start_revision {
                            continue;
                        }

                        notifications
                            .entry(*watch_id)
                            .or_insert_with(Vec::new)
                            .push(event.clone());
                    }
                }
            }
        }

        // Send batched notifications
        for (watch_id, batch_events) in notifications {
            if let Some(watcher) = self.watchers.get(&watch_id) {
                let watch_event = WatchEvent {
                    watch_id,
                    events: batch_events,
                    compact_revision,
                    header: Some(header.clone()),
                };

                // Non-blocking send; if channel is full, log warning
                if let Err(e) = watcher.tx.try_send(watch_event) {
                    warn!(watch_id, error = ?e, "Failed to send watch event");
                }
            }
        }

        Ok(())
    }

    /// Sends a progress notification to a specific watcher.
    pub fn progress_notify(&self, watch_id: i64, revision: i64) -> WatchResult<()> {
        if let Some(watcher) = self.watchers.get(&watch_id) {
            if !watcher.progress_notify {
                return Ok(());
            }

            let header = ResponseHeader {
                cluster_id: self.cluster_id,
                member_id: self.member_id,
                revision,
                raft_term: 0,
            };

            let watch_event = WatchEvent {
                watch_id,
                events: vec![], // Empty events list indicates progress notification
                compact_revision: 0,
                header: Some(header),
            };

            watcher.tx.try_send(watch_event).ok();
            Ok(())
        } else {
            Err(WatchError::WatchNotFound(watch_id))
        }
    }

    /// Returns the number of active watchers.
    pub fn get_watcher_count(&self) -> usize {
        self.watchers.len()
    }

    /// Returns the number of watch ranges being monitored.
    pub fn get_watch_range_count(&self) -> usize {
        self.key_watchers.len()
    }

    /// Gets information about a specific watcher.
    pub fn get_watcher_info(&self, watch_id: i64) -> WatchResult<(Vec<u8>, Vec<u8>, i64)> {
        self.watchers
            .get(&watch_id)
            .map(|w| (w.key.clone(), w.range_end.clone(), w.start_revision))
            .ok_or(WatchError::WatchNotFound(watch_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watch_range_contains() {
        let range = WatchRange {
            key: b"foo".to_vec(),
            range_end: b"fop".to_vec(),
        };

        assert!(range.contains(b"foo"));
        assert!(range.contains(b"foobar"));
        assert!(!range.contains(b"fop"));
        assert!(!range.contains(b"aaa"));
        assert!(!range.contains(b"fon")); // "fon" < "foo", outside range
    }

    #[test]
    fn test_watch_range_prefix() {
        let range = WatchRange {
            key: b"foo/".to_vec(),
            range_end: Vec::new(),
        };

        assert!(range.contains(b"foo/"));
        assert!(!range.contains(b"foo0"));
        assert!(!range.contains(b"foo"));
    }

    #[test]
    fn test_watch_range_all_keys() {
        let range = WatchRange {
            key: b"".to_vec(),
            range_end: b"\0".to_vec(),
        };

        assert!(range.contains(b"aaa"));
        assert!(range.contains(b"zzz"));
        assert!(range.contains(b""));
    }

    #[tokio::test]
    async fn test_create_and_cancel_watch() {
        let hub = WatchHub::new(1, 1);
        let (_tx, _rx) = mpsc::channel(10);

        let watch_id = hub
            .create_watch(
                b"key".to_vec(),
                Vec::new(),
                0,
                vec![],
                false,
                false,
                false,
                _tx,
            )
            .unwrap();

        assert_eq!(watch_id, 1);
        assert_eq!(hub.get_watcher_count(), 1);

        hub.cancel_watch(watch_id).unwrap();
        assert_eq!(hub.get_watcher_count(), 0);
    }
}
