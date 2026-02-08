# Storage Engine Integration Guide

## Overview

This guide explains how to integrate the MVCC storage engine with other rusd components (API server, Raft consensus, lease manager, watch system).

## Module Exports

The storage module exports these public types and functions:

```rust
pub use storage::{
    // Core components
    Backend, BackendConfig, BackendError, BackendResult,
    MvccStore, KeyValue, RangeResult, Event,
    KeyIndex, Revision,
    Compactor, CompactionMode,

    // Error types
    StorageError, StorageResult,
};
```

## Initialization Pattern

### 1. Create Backend

```rust
let config = BackendConfig {
    data_dir: PathBuf::from("./data"),
    cache_size_mb: 256,
    flush_interval_ms: 1000,
    compression: true,
};

let backend = Backend::new(config)?;
```

### 2. Initialize MVCC Store

```rust
let store = Arc::new(MvccStore::new(backend)?);
```

### 3. Start Compactor

```rust
use std::time::Duration;

let compactor = Compactor::new(
    store.clone(),
    CompactionMode::Periodic(Duration::from_secs(600)), // 10 minutes
    Duration::from_secs(3600), // Keep 1 hour of history
);

let compactor_handle = compactor.run();
```

### 4. Store is Ready

Pass `Arc<MvccStore>` to other components:

```rust
// API service
let api_service = ApiService::new(store.clone());

// Lease manager
let lease_manager = LeaseManager::new(store.clone());

// Watch system
let watch_service = WatchService::new(store.clone());

// Raft consensus
let raft_node = RaftNode::new(store.clone());
```

## API Service Integration

### Range Queries

```rust
impl KvService {
    pub async fn range(&self, request: RangeRequest) -> Result<RangeResponse> {
        let result = self.store.range(
            &request.key,
            &request.range_end,
            request.revision, // 0 for current
            request.limit,
            request.count_only,
        )?;

        Ok(RangeResponse {
            kvs: result.kvs,
            more: result.more,
            count: result.count as i64,
        })
    }
}
```

### Put/Delete Operations

```rust
pub async fn put(&self, request: PutRequest) -> Result<PutResponse> {
    let (revision, kv) = self.store.put(
        &request.key,
        &request.value,
        request.lease,
    )?;

    self.notify_watchers(&kv, None)?;

    Ok(PutResponse {
        header: ResponseHeader { revision },
        prev_kv: None,
    })
}

pub async fn delete_range(&self, request: DeleteRangeRequest) -> Result<DeleteRangeResponse> {
    let (revision, deleted_kvs) = self.store.delete_range(
        &request.key,
        &request.range_end,
    )?;

    for kv in &deleted_kvs {
        self.notify_watchers(kv, None)?;
    }

    Ok(DeleteRangeResponse {
        header: ResponseHeader { revision },
        deleted: deleted_kvs.len() as i64,
        prev_kvs: deleted_kvs,
    })
}
```

## Lease System Integration

### Lease-Tied Keys

```rust
impl LeaseManager {
    pub async fn grant(&mut self, lease_id: i64, ttl: i64) -> Result<()> {
        // Track lease in lease tree
        self.store.put_lease(lease_id, ttl)?;
        Ok(())
    }

    pub async fn revoke(&mut self, lease_id: i64) -> Result<()> {
        // Delete all keys associated with this lease
        let keys = self.store.get_keys_by_lease(lease_id)?;
        for key in keys {
            self.store.delete_range(&key, &[])?;
        }
        Ok(())
    }

    pub async fn on_key_put(&mut self, key: &KeyValue, lease: i64) {
        // Called when a key is put with a lease
        if lease > 0 {
            self.store.associate_key_with_lease(key, lease)?;
        }
    }
}
```

## Watch System Integration

### Getting Events

```rust
impl WatchService {
    pub async fn watch(
        &self,
        key: &[u8],
        range_end: &[u8],
        start_revision: i64,
    ) -> Result<Vec<Event>> {
        // Get all events since start_revision
        let events = self.store.watch_events(start_revision)?;

        // Filter to requested key range
        Ok(events.into_iter()
            .filter(|e| self.in_range(&e.kv.key, key, range_end))
            .collect())
    }

    fn in_range(&self, key: &[u8], start: &[u8], end: &[u8]) -> bool {
        key >= start && (end.is_empty() || key < end)
    }
}
```

### Event Notification

```rust
impl ApiService {
    fn notify_watchers(&self, kv: &KeyValue, prev_kv: Option<&KeyValue>) -> Result<()> {
        let event = Event {
            event_type: "Put".to_string(),
            kv: kv.clone(),
            prev_kv: prev_kv.cloned(),
        };

        // Broadcast to all watchers
        self.watch_broadcast.send(event)?;
        Ok(())
    }
}
```

## Raft Consensus Integration

### Applying Log Entries

```rust
impl RaftNode {
    pub async fn apply_entry(&mut self, entry: LogEntry) -> Result<()> {
        match entry.command {
            Command::Put { key, value, lease } => {
                let (revision, _) = self.store.put(&key, &value, lease)?;
                self.last_applied_revision = revision;
            }
            Command::Delete { key, range_end } => {
                let (revision, _) = self.store.delete_range(&key, &range_end)?;
                self.last_applied_revision = revision;
            }
        }
        Ok(())
    }
}
```

### Snapshots

```rust
impl RaftNode {
    pub async fn create_snapshot(&self) -> Result<Vec<u8>> {
        // Get snapshot from storage
        let snapshot = self.store.snapshot()?;
        Ok(snapshot)
    }

    pub async fn restore_snapshot(&mut self, data: &[u8]) -> Result<()> {
        // Restore from snapshot
        self.store.restore(data)?;
        Ok(())
    }
}
```

## Transaction Support

### Compare-and-Swap

```rust
pub async fn txn(&self, request: TxnRequest) -> Result<TxnResponse> {
    // Build compare conditions
    let compares = request.compare.iter()
        .map(|c| parse_compare(c))
        .collect();

    // Parse success operations
    let success_ops = parse_request_ops(&request.success);

    // Parse failure operations
    let failure_ops = parse_request_ops(&request.failure);

    // Execute transaction
    self.store.txn(compares, success_ops, failure_ops)?;

    Ok(TxnResponse {
        header: ResponseHeader {
            revision: self.store.current_revision(),
        },
        succeeded: true, // Simplified
        responses: vec![], // Would contain actual responses
    })
}
```

## Error Handling

Map storage errors to gRPC status codes:

```rust
impl From<StorageError> for tonic::Status {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::NotFound => {
                Status::not_found("Key not found")
            }
            StorageError::Conflict => {
                Status::failed_precondition("Transaction conflict")
            }
            StorageError::InvalidArgument(msg) => {
                Status::invalid_argument(msg)
            }
            StorageError::Mvcc(msg) => {
                Status::internal(format!("MVCC error: {}", msg))
            }
            StorageError::Backend(e) => {
                Status::internal(format!("Backend error: {}", e))
            }
            _ => Status::internal("Storage error"),
        }
    }
}
```

## Monitoring and Metrics

### Key Metrics

```rust
impl MvccStore {
    pub fn metrics(&self) -> StorageMetrics {
        StorageMetrics {
            current_revision: self.current_revision(),
            compact_revision: self.compact_revision(),
            key_count: self.key_index.read().len(),
            db_size_bytes: self.backend.size(),
        }
    }
}
```

### Logging

The storage engine uses the `tracing` crate:

```rust
// In your application setup
use tracing_subscriber::{fmt, EnvFilter};

tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::from_default_env())
    .init();

// Set RUST_LOG=debug for storage debug logs
// RUST_LOG=rusd::storage=debug for just storage
```

Debug logs show:
- Put/delete operations
- Compaction progress
- Backend operations
- Index updates

## Testing Integration

### Mock Store for Testing

```rust
#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    fn setup_test_store() -> Arc<MvccStore> {
        let temp = TempDir::new().unwrap();
        let config = BackendConfig {
            data_dir: temp.path().to_path_buf(),
            cache_size_mb: 64,
            flush_interval_ms: 100,
            compression: false,
        };

        let backend = Backend::new(config).unwrap();
        Arc::new(MvccStore::new(backend).unwrap())
    }

    #[tokio::test]
    async fn test_api_service_with_store() {
        let store = setup_test_store();
        let service = ApiService::new(store);

        let resp = service.range(RangeRequest {
            key: b"key1".to_vec(),
            ..Default::default()
        }).await.unwrap();

        assert_eq!(resp.count, 0); // Empty initially
    }
}
```

## Performance Tuning

### Cache Configuration

```rust
// For small deployments (< 100K keys)
let config = BackendConfig {
    cache_size_mb: 128,
    ..Default::default()
};

// For medium deployments (100K - 1M keys)
let config = BackendConfig {
    cache_size_mb: 512,
    ..Default::default()
};

// For large deployments (> 1M keys)
let config = BackendConfig {
    cache_size_mb: 1024,
    ..Default::default()
};
```

### Compaction Tuning

```rust
// Conservative (keep more history)
let compactor = Compactor::new(
    store,
    CompactionMode::Periodic(Duration::from_secs(3600)), // Every hour
    Duration::from_secs(86400), // Keep 24 hours
);

// Aggressive (minimal history)
let compactor = Compactor::new(
    store,
    CompactionMode::Periodic(Duration::from_secs(60)), // Every minute
    Duration::from_secs(300), // Keep 5 minutes
);
```

## Troubleshooting

### High Memory Usage

- Reduce `cache_size_mb`
- Increase compaction frequency
- Check for key bloat (very large values)

### Slow Reads

- Increase `cache_size_mb` (working set in cache)
- Check for full table scans (use proper range limits)

### Slow Writes

- Check compaction isn't blocking (run periodically)
- Reduce flush interval if disk I/O is limiting

### Storage Growth

- Reduce compaction retention period
- Check for keys that should be expired by leases
