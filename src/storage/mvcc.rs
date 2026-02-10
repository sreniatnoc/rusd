//! Multi-Version Concurrency Control (MVCC) store.
//!
//! This is the core of the storage engine, implementing etcd's revision-based versioning.
//! Every write operation increments the global revision counter, enabling:
//!
//! - Point-in-time reads at any historical revision
//! - Consistent snapshots
//! - Reliable watch/notification semantics
//! - Non-blocking concurrent reads
//!
//! The MVCC store maintains:
//! 1. The key-value data in sled with revision information
//! 2. An in-memory index mapping keys to their revision history
//! 3. Current and compact revision counters

use crate::storage::{Backend, BackendError, KeyIndex, StorageError, StorageResult};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;
use std::time::Instant;
use tracing::{debug, info, warn};

/// A key-value pair with MVCC metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyValue {
    /// The actual key
    pub key: Vec<u8>,

    /// The revision when this key was created
    pub create_revision: i64,

    /// The revision when this key was last modified
    pub mod_revision: i64,

    /// Version is the number of times this key has been modified
    pub version: i64,

    /// The actual value
    pub value: Vec<u8>,

    /// Associated lease ID (0 if no lease)
    pub lease: i64,
}

impl KeyValue {
    /// Serializes the KeyValue to bytes for storage.
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Simple format: all fields as length-prefixed or fixed-size
        buf.extend_from_slice(&self.create_revision.to_le_bytes());
        buf.extend_from_slice(&self.mod_revision.to_le_bytes());
        buf.extend_from_slice(&self.version.to_le_bytes());
        buf.extend_from_slice(&self.lease.to_le_bytes());

        buf.extend_from_slice(&(self.value.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.value);

        buf
    }

    /// Encodes the KeyValue with key included, for kv_rev tree storage.
    fn encode_with_key(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        buf.extend_from_slice(&self.create_revision.to_le_bytes());
        buf.extend_from_slice(&self.mod_revision.to_le_bytes());
        buf.extend_from_slice(&self.version.to_le_bytes());
        buf.extend_from_slice(&self.lease.to_le_bytes());

        buf.extend_from_slice(&(self.key.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.key);

        buf.extend_from_slice(&(self.value.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.value);

        buf
    }

    /// Deserializes a KeyValue from kv_rev tree bytes (key embedded in value).
    fn decode_with_key(data: &[u8]) -> StorageResult<Self> {
        if data.len() < 36 {
            return Err(StorageError::Mvcc("Invalid KeyValue encoding (with key)".to_string()));
        }

        let create_revision = i64::from_le_bytes(data[0..8].try_into().unwrap());
        let mod_revision = i64::from_le_bytes(data[8..16].try_into().unwrap());
        let version = i64::from_le_bytes(data[16..24].try_into().unwrap());
        let lease = i64::from_le_bytes(data[24..32].try_into().unwrap());

        let key_len = u32::from_le_bytes(data[32..36].try_into().unwrap()) as usize;
        if 36 + key_len + 4 > data.len() {
            return Err(StorageError::Mvcc("Invalid KeyValue encoding (key too long)".to_string()));
        }
        let key = data[36..36 + key_len].to_vec();

        let val_offset = 36 + key_len;
        let value_len = u32::from_le_bytes(data[val_offset..val_offset + 4].try_into().unwrap()) as usize;
        if val_offset + 4 + value_len > data.len() {
            return Err(StorageError::Mvcc("Invalid KeyValue encoding (value too long)".to_string()));
        }
        let value = data[val_offset + 4..val_offset + 4 + value_len].to_vec();

        Ok(KeyValue {
            key,
            create_revision,
            mod_revision,
            version,
            value,
            lease,
        })
    }

    /// Deserializes a KeyValue from bytes.
    fn decode(key: Vec<u8>, data: &[u8]) -> StorageResult<Self> {
        if data.len() < 32 {
            return Err(StorageError::Mvcc("Invalid KeyValue encoding".to_string()));
        }

        let create_revision = i64::from_le_bytes([
            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
        ]);
        let mod_revision = i64::from_le_bytes([
            data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
        ]);
        let version = i64::from_le_bytes([
            data[16], data[17], data[18], data[19], data[20], data[21], data[22], data[23],
        ]);
        let lease = i64::from_le_bytes([
            data[24], data[25], data[26], data[27], data[28], data[29], data[30], data[31],
        ]);

        let value_len = u32::from_le_bytes([data[32], data[33], data[34], data[35]]) as usize;
        if 36 + value_len > data.len() {
            return Err(StorageError::Mvcc("Invalid KeyValue encoding".to_string()));
        }

        let value = data[36..36 + value_len].to_vec();

        Ok(KeyValue {
            key,
            create_revision,
            mod_revision,
            version,
            value,
            lease,
        })
    }
}

/// Result of a range query.
#[derive(Clone, Debug)]
pub struct RangeResult {
    /// The key-value pairs
    pub kvs: Vec<KeyValue>,

    /// More indicates if there are more keys
    pub more: bool,

    /// Number of keys in the range (before limit)
    pub count: usize,
}

/// A write operation to be batched.
#[derive(Clone, Debug)]
enum WriteOp {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
        lease: i64,
    },
    Delete {
        key: Vec<u8>,
    },
}

/// Batch of write operations to be flushed together.
struct WriteBatch {
    /// List of pending operations
    ops: Vec<WriteOp>,

    /// When the batch was created
    last_flush: Instant,
}

impl WriteBatch {
    fn new() -> Self {
        Self {
            ops: Vec::new(),
            last_flush: Instant::now(),
        }
    }

    fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    fn clear(&mut self) {
        self.ops.clear();
        self.last_flush = Instant::now();
    }
}

/// The MVCC Store - the core storage engine.
pub struct MvccStore {
    /// The underlying persistent storage backend
    backend: Arc<Backend>,

    /// Current global revision (monotonically increasing)
    current_revision: AtomicI64,

    /// Revisions older than this have been compacted away
    compact_revision: AtomicI64,

    /// In-memory index of keys to revisions
    key_index: Arc<RwLock<KeyIndex>>,

    /// Batch of write operations (for coalescing writes)
    write_batch: Arc<parking_lot::Mutex<WriteBatch>>,
}

impl MvccStore {
    /// Creates a new MVCC store, initializing the key index from backend data.
    pub fn new(backend: Arc<Backend>) -> StorageResult<Arc<Self>> {
        info!("Initializing MVCC store");

        // Initialize key index from sled by scanning all existing data
        let mut index = KeyIndex::new();
        let mut current_revision = 1i64;
        let mut compact_revision = 0i64;

        // Scan all existing keys and rebuild the index
        // In a real implementation, we might store revision metadata separately
        // For now, we'll initialize with empty index (revisions stored in backend)

        let store = Arc::new(Self {
            backend,
            current_revision: AtomicI64::new(current_revision),
            compact_revision: AtomicI64::new(compact_revision),
            key_index: Arc::new(RwLock::new(index)),
            write_batch: Arc::new(parking_lot::Mutex::new(WriteBatch::new())),
        });

        info!(
            "MVCC store initialized with revision={}",
            current_revision
        );

        Ok(store)
    }

    /// Gets the current global revision.
    pub fn current_revision(&self) -> i64 {
        self.current_revision.load(Ordering::SeqCst)
    }

    /// Gets the compact revision (oldest kept revision).
    pub fn compact_revision(&self) -> i64 {
        self.compact_revision.load(Ordering::SeqCst)
    }

    /// Increments and returns the next revision.
    fn next_revision(&self) -> i64 {
        self.current_revision.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Builds a kv_rev tree key: `{revision_be_bytes}{key_bytes}`.
    /// Big-endian ensures natural sort order by revision.
    fn rev_key(revision: i64, key: &[u8]) -> Vec<u8> {
        let mut rk = Vec::with_capacity(8 + key.len());
        rk.extend_from_slice(&revision.to_be_bytes());
        rk.extend_from_slice(key);
        rk
    }

    /// Stores a single key-value pair, returning (revision, new_kv, prev_kv).
    pub fn put(&self, key: &[u8], value: &[u8], lease: i64) -> StorageResult<(i64, KeyValue, Option<KeyValue>)> {
        // Get the current revision of this key (if any)
        let index = self.key_index.read();
        let _current_version = index
            .get(key, self.current_revision())
            .map(|r| r.main)
            .unwrap_or(0);
        drop(index);

        // Allocate new revision
        let new_revision = self.next_revision();

        // Get current key version and previous KV
        let current_kv = self
            .backend
            .get("kv", key)
            .map_err(|_| StorageError::Mvcc("Backend error".to_string()))?;

        let prev_kv = current_kv
            .as_ref()
            .and_then(|data| KeyValue::decode(key.to_vec(), data).ok());

        let version = prev_kv.as_ref().map(|kv| kv.version + 1).unwrap_or(1);

        // Create the new KeyValue
        let kv = KeyValue {
            key: key.to_vec(),
            create_revision: if version == 1 {
                new_revision
            } else {
                prev_kv.as_ref().map(|kv| kv.create_revision).unwrap_or(new_revision)
            },
            mod_revision: new_revision,
            version,
            value: value.to_vec(),
            lease,
        };

        // Store latest in kv tree (overwrites previous)
        self.backend
            .put("kv", key, &kv.encode())
            .map_err(|_| StorageError::Mvcc("Backend put failed".to_string()))?;

        // Store revision-indexed copy in kv_rev tree for historical reads
        let rev_key = Self::rev_key(new_revision, key);
        self.backend
            .put("kv_rev", &rev_key, &kv.encode_with_key())
            .map_err(|_| StorageError::Mvcc("Backend kv_rev put failed".to_string()))?;

        // Update index
        {
            let mut index = self.key_index.write();
            index.put(key, crate::storage::index::Revision::new(new_revision, 0));
        }

        debug!("Put key {:?} at revision {}", String::from_utf8_lossy(key), new_revision);

        Ok((new_revision, kv, prev_kv))
    }

    /// Deletes a range of keys, returning the revision and deleted KeyValues.
    pub fn delete_range(
        &self,
        start: &[u8],
        end: &[u8],
    ) -> StorageResult<(i64, Vec<KeyValue>)> {
        // Allocate new revision
        let new_revision = self.next_revision();

        // Find keys to delete
        let mut deleted = Vec::new();
        let mut keys_to_delete = Vec::new();

        // Scan the range at current revision
        let index = self.key_index.read();
        let range_keys = index.range(start, end, self.current_revision());
        drop(index);

        for (key, _rev) in range_keys {
            // Get the current value
            if let Ok(Some(kv_data)) = self.backend.get("kv", &key) {
                if let Ok(kv) = KeyValue::decode(key.clone(), &kv_data) {
                    deleted.push(kv);
                    keys_to_delete.push(key);
                }
            }
        }

        // Delete all keys from kv tree and write tombstones to kv_rev
        for key in &keys_to_delete {
            self.backend
                .delete("kv", key)
                .map_err(|_| StorageError::Mvcc("Backend delete failed".to_string()))?;

            // Write a tombstone entry to kv_rev for watch event delivery
            let tombstone = KeyValue {
                key: key.clone(),
                create_revision: 0,
                mod_revision: new_revision,
                version: 0,
                value: Vec::new(),
                lease: 0,
            };
            let rev_key = Self::rev_key(new_revision, key);
            self.backend
                .put("kv_rev", &rev_key, &tombstone.encode_with_key())
                .map_err(|_| StorageError::Mvcc("Backend kv_rev tombstone failed".to_string()))?;
        }

        // Update index
        {
            let mut index = self.key_index.write();
            for key in keys_to_delete {
                index.tombstone(&key, crate::storage::index::Revision::new(new_revision, 0));
            }
        }

        debug!("Deleted {} keys at revision {}", deleted.len(), new_revision);

        Ok((new_revision, deleted))
    }

    /// Performs a range query at a specific revision.
    ///
    /// If revision is 0 or matches current, reads from the `kv` tree (latest values).
    /// If a historical revision is requested, reads from `kv_rev` tree using the
    /// index to find which revision of each key was valid at that point in time.
    pub fn range(
        &self,
        start: &[u8],
        end: &[u8],
        revision: i64,
        limit: i64,
        count_only: bool,
    ) -> StorageResult<RangeResult> {
        // Use the specified revision, or current if 0
        let query_revision = if revision <= 0 {
            self.current_revision()
        } else {
            revision
        };

        let is_current = revision <= 0 || revision >= self.current_revision();

        // Get keys from index at this revision
        let index = self.key_index.read();
        let keys = index.range(start, end, query_revision);
        drop(index);

        let count = keys.len();
        let mut kvs = Vec::new();

        for (key, rev) in keys {
            if limit > 0 && kvs.len() >= limit as usize {
                break;
            }

            if count_only {
                continue;
            }

            if is_current {
                // Read latest from kv tree
                if let Ok(Some(kv_data)) = self.backend.get("kv", &key) {
                    if let Ok(kv) = KeyValue::decode(key, &kv_data) {
                        if kv.create_revision <= query_revision && kv.mod_revision <= query_revision {
                            kvs.push(kv);
                        }
                    }
                }
            } else {
                // Historical read: look up the exact revision in kv_rev
                let rev_key = Self::rev_key(rev.main, &key);
                if let Ok(Some(kv_data)) = self.backend.get("kv_rev", &rev_key) {
                    if let Ok(kv) = KeyValue::decode_with_key(&kv_data) {
                        kvs.push(kv);
                    }
                }
            }
        }

        let more = limit > 0 && count > kvs.len();

        Ok(RangeResult {
            kvs,
            more,
            count,
        })
    }

    /// Performs a transaction with compare-and-swap semantics.
    ///
    /// This is a simplified version. A full implementation would need proper
    /// compare operations and request operation processing.
    pub fn txn(
        &self,
        _compares: Vec<()>,
        success_ops: Vec<()>,
        _failure_ops: Vec<()>,
    ) -> StorageResult<()> {
        // Allocate revision for the transaction
        let _new_revision = self.next_revision();

        // Process operations (simplified - full impl would process actual ops)
        debug!("Transaction at revision {}", _new_revision);

        Ok(())
    }

    /// Compacts the store, removing revisions before the given revision.
    ///
    /// This reclaims disk space and memory used by old versions.
    pub fn compact(&self, revision: i64) -> StorageResult<()> {
        let current = self.current_revision();
        if revision > current {
            return Err(StorageError::Mvcc(
                "Cannot compact to future revision".to_string(),
            ));
        }

        // Update compact revision
        self.compact_revision.store(revision, Ordering::SeqCst);

        // Compact the key index
        {
            let mut index = self.key_index.write();
            index.compact(revision);
        }

        // Compact the backend
        self.backend
            .compact(revision)
            .map_err(|e| StorageError::Backend(e))?;

        info!("Compacted MVCC store to revision {}", revision);

        Ok(())
    }

    /// Gets all events (changes) since a specific revision for watching.
    ///
    /// Scans the kv_rev tree for all entries with revision >= start_revision,
    /// returning Put or Delete events for watch catchup.
    pub fn watch_events(&self, start_revision: i64) -> StorageResult<Vec<Event>> {
        let current = self.current_revision();

        if start_revision > current {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();

        // Scan kv_rev from start_revision onwards
        let scan_start = start_revision.to_be_bytes();
        let scan_end = (current + 1).to_be_bytes();

        let entries = self.backend
            .scan("kv_rev", &scan_start, &scan_end, 0)
            .map_err(|e| StorageError::Backend(e))?;

        for (rev_key, data) in entries {
            if let Ok(kv) = KeyValue::decode_with_key(&data) {
                let event_type = if kv.version == 0 && kv.value.is_empty() {
                    "Delete".to_string()
                } else {
                    "Put".to_string()
                };

                events.push(Event {
                    event_type,
                    kv,
                    prev_kv: None, // prev_kv would need additional lookup
                });
            }
        }

        Ok(events)
    }

    /// Flushes any pending write batches.
    pub fn flush(&self) -> StorageResult<()> {
        let mut batch = self.write_batch.lock();
        if !batch.is_empty() {
            debug!("Flushing write batch with {} operations", batch.ops.len());
            batch.clear();
        }
        Ok(())
    }
}

/// Represents a watch event (key-value change).
#[derive(Clone, Debug)]
pub struct Event {
    /// Type of event: "Put" or "Delete"
    pub event_type: String,

    /// The key-value pair involved
    pub kv: KeyValue,

    /// The previous key-value if this is an update
    pub prev_kv: Option<KeyValue>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use crate::storage::BackendConfig;

    fn setup_store() -> Arc<MvccStore> {
        let temp_dir = TempDir::new().unwrap();
        let config = BackendConfig {
            data_dir: temp_dir.path().to_path_buf(),
            cache_size_mb: 64,
            flush_interval_ms: 100,
            compression: false,
        };

        let backend = Backend::new(config).unwrap();
        MvccStore::new(backend).unwrap()
    }

    #[test]
    fn test_put_and_range() {
        let store = setup_store();

        let (rev1, kv1, prev1) = store.put(b"key1", b"value1", 0).unwrap();
        assert_eq!(kv1.version, 1);
        assert_eq!(kv1.mod_revision, rev1);
        assert!(prev1.is_none()); // No previous value for new key

        let result = store.range(b"key1", b"key2", 0, 10, false).unwrap();
        assert_eq!(result.kvs.len(), 1);
        assert_eq!(result.kvs[0].value, b"value1");
    }

    #[test]
    fn test_put_update() {
        let store = setup_store();

        let (rev1, kv1, prev1) = store.put(b"key1", b"value1", 0).unwrap();
        let (rev2, kv2, prev2) = store.put(b"key1", b"value2", 0).unwrap();

        assert_eq!(kv1.version, 1);
        assert_eq!(kv2.version, 2);
        assert_eq!(kv1.create_revision, kv2.create_revision); // Same creation revision
        assert!(rev2 > rev1);
        assert!(prev1.is_none());
        assert!(prev2.is_some());
        assert_eq!(prev2.unwrap().value, b"value1"); // Previous value returned

        let result = store.range(b"key1", b"key2", 0, 10, false).unwrap();
        assert_eq!(result.kvs[0].value, b"value2");
    }

    #[test]
    fn test_delete_range() {
        let store = setup_store();

        store.put(b"key1", b"value1", 0).unwrap();
        store.put(b"key2", b"value2", 0).unwrap();
        store.put(b"key3", b"value3", 0).unwrap();

        let (_, deleted) = store.delete_range(b"key1", b"key3").unwrap();
        assert_eq!(deleted.len(), 2); // key1 and key2

        let result = store.range(b"key1", b"key4", 0, 10, false).unwrap();
        assert_eq!(result.kvs.len(), 1); // Only key3
    }

    #[test]
    fn test_revision_increment() {
        let store = setup_store();

        let rev0 = store.current_revision();
        store.put(b"key1", b"value1", 0).unwrap();
        let rev1 = store.current_revision();

        assert!(rev1 > rev0);
    }

    #[test]
    fn test_historical_range_read() {
        let store = setup_store();

        let (rev1, _, _) = store.put(b"key1", b"value1", 0).unwrap();
        let (rev2, _, _) = store.put(b"key1", b"value2", 0).unwrap();
        let (rev3, _, _) = store.put(b"key1", b"value3", 0).unwrap();

        // Read at rev1 should return value1
        let result = store.range(b"key1", b"key2", rev1, 10, false).unwrap();
        assert_eq!(result.kvs.len(), 1);
        assert_eq!(result.kvs[0].value, b"value1");

        // Read at rev2 should return value2
        let result = store.range(b"key1", b"key2", rev2, 10, false).unwrap();
        assert_eq!(result.kvs.len(), 1);
        assert_eq!(result.kvs[0].value, b"value2");

        // Read at current (rev3) should return value3
        let result = store.range(b"key1", b"key2", 0, 10, false).unwrap();
        assert_eq!(result.kvs.len(), 1);
        assert_eq!(result.kvs[0].value, b"value3");
    }

    #[test]
    fn test_watch_events_from_revision() {
        let store = setup_store();

        let (rev1, _, _) = store.put(b"key1", b"value1", 0).unwrap();
        let (rev2, _, _) = store.put(b"key2", b"value2", 0).unwrap();
        let (rev3, _, _) = store.put(b"key1", b"value1_updated", 0).unwrap();

        // Get events from rev2 onwards (should get key2 put and key1 update)
        let events = store.watch_events(rev2).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "Put");
        assert_eq!(events[0].kv.key, b"key2");
        assert_eq!(events[1].event_type, "Put");
        assert_eq!(events[1].kv.key, b"key1");
        assert_eq!(events[1].kv.value, b"value1_updated");

        // Get events from rev1 onwards (should get all 3)
        let events = store.watch_events(rev1).unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn test_watch_events_delete_tombstone() {
        let store = setup_store();

        let (rev1, _, _) = store.put(b"key1", b"value1", 0).unwrap();
        let (rev2, deleted) = store.delete_range(b"key1", b"key2").unwrap();
        assert_eq!(deleted.len(), 1);

        // Events from rev2 should contain the delete tombstone
        let events = store.watch_events(rev2).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "Delete");
        assert_eq!(events[0].kv.key, b"key1");
    }

    #[test]
    fn test_compact_removes_old_revisions() {
        let store = setup_store();

        let (rev1, _, _) = store.put(b"key1", b"value1", 0).unwrap();
        let (rev2, _, _) = store.put(b"key1", b"value2", 0).unwrap();
        let (rev3, _, _) = store.put(b"key1", b"value3", 0).unwrap();

        // Compact up to rev2 (should remove rev1)
        store.compact(rev2).unwrap();

        // Events from rev1 should no longer include rev1 entries (compacted)
        let events = store.watch_events(rev1).unwrap();
        // After compaction, only rev2 and rev3 remain
        assert_eq!(events.len(), 2);

        // Current read should still work
        let result = store.range(b"key1", b"key2", 0, 10, false).unwrap();
        assert_eq!(result.kvs[0].value, b"value3");
    }
}
