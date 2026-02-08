# Storage Engine Quick Reference

## Quick Start

```rust
use rusd::storage::{Backend, BackendConfig, MvccStore, Compactor, CompactionMode};
use std::sync::Arc;
use std::time::Duration;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize backend
    let backend = Backend::new(BackendConfig {
        data_dir: PathBuf::from("./data"),
        cache_size_mb: 256,
        flush_interval_ms: 1000,
        compression: true,
    })?;

    // 2. Create MVCC store
    let store = Arc::new(MvccStore::new(backend)?);

    // 3. Start compactor
    let compactor = Compactor::new(
        store.clone(),
        CompactionMode::Periodic(Duration::from_secs(600)),
        Duration::from_secs(3600),
    );
    tokio::spawn(async move { compactor.run().await });

    // 4. Use the store
    store.put(b"key1", b"value1", 0)?;
    let result = store.range(b"key1", b"key2", 0, 10, false)?;
    println!("{:?}", result.kvs);

    Ok(())
}
```

## Core API

### MvccStore

```rust
// Writing
let (rev, kv) = store.put(key: &[u8], value: &[u8], lease: i64)?;
let (rev, deleted) = store.delete_range(start: &[u8], end: &[u8])?;

// Reading
let result = store.range(
    start: &[u8],
    end: &[u8],
    revision: i64,           // 0 = current
    limit: i64,              // 0 = no limit
    count_only: bool,
)?;

// Metadata
let rev = store.current_revision();
let compact = store.compact_revision();

// Operations
store.compact(revision: i64)?;
store.flush()?;
let events = store.watch_events(start_revision: i64)?;
```

## Key Concepts

### Revisions
- Global counter: incrementing with each write
- Format: (main, sub) where main is counter, sub is operation index
- Enables point-in-time reads

### KeyValue
```rust
pub struct KeyValue {
    pub key: Vec<u8>,
    pub create_revision: i64,
    pub mod_revision: i64,
    pub version: i64,
    pub value: Vec<u8>,
    pub lease: i64,
}
```

### RangeResult
```rust
pub struct RangeResult {
    pub kvs: Vec<KeyValue>,
    pub more: bool,
    pub count: usize,
}
```

## Configuration

```rust
pub struct BackendConfig {
    pub data_dir: PathBuf,           // Where to store data
    pub cache_size_mb: u64,          // RAM cache (default: 256)
    pub flush_interval_ms: u64,      // Flush interval (default: 1000)
    pub compression: bool,           // Compress values (default: true)
}
```

## Common Patterns

### Single Key Operations
```rust
// Write
store.put(b"user:123:name", b"Alice", 0)?;

// Read current
let result = store.range(b"user:123:name", b"user:123:name\0", 0, 1, false)?;
if let Some(kv) = result.kvs.first() {
    println!("Value: {}", String::from_utf8_lossy(&kv.value));
}

// Delete
store.delete_range(b"user:123:name", b"user:123:name\0")?;
```

### Range Queries
```rust
// All keys matching prefix
let result = store.range(b"user:", b"user;\0", 0, 1000, false)?;

// All keys in range [start, end)
let result = store.range(b"key1", b"key9", 0, 100, false)?;

// Count only
let result = store.range(b"", b"", 0, 0, true)?;
println!("Total keys: {}", result.count);
```

### Historical Reads
```rust
// Get current revision
let current = store.current_revision();

// Make changes
store.put(b"key", b"value1", 0)?;
store.put(b"key", b"value2", 0)?;

// Read at earlier revision
let result = store.range(b"key", b"key\0", current, 1, false)?;
// Returns state from before the changes
```

### Lease-Based TTL
```rust
// Associate key with lease ID
let (rev, kv) = store.put(b"temp_key", b"data", 123)?;

// Lease system manages expiration
// When lease 123 expires, delete_range removes all keys with that lease
```

### Transactions (CAS)
```rust
// Simple compare-and-swap
store.txn(
    vec![], // comparisons
    vec![], // success operations
    vec![], // failure operations
)?;
```

## Error Handling

```rust
use rusd::storage::{StorageError, StorageResult};

match store.put(key, value, 0) {
    Ok((rev, kv)) => println!("Wrote at revision {}", rev),
    Err(StorageError::NotFound) => println!("Key not found"),
    Err(StorageError::Conflict) => println!("Conflict"),
    Err(StorageError::Mvcc(msg)) => println!("MVCC error: {}", msg),
    Err(e) => println!("Other error: {}", e),
}
```

## Performance Tips

### For High Throughput Writes
```rust
// Use batch operations instead of individual puts
let pairs = vec![
    (b"key1".as_ref(), b"val1".as_ref()),
    (b"key2".as_ref(), b"val2".as_ref()),
    (b"key3".as_ref(), b"val3".as_ref()),
];
backend.batch_put("kv", &pairs)?;
```

### For Large Working Sets
```rust
// Increase cache size
BackendConfig {
    cache_size_mb: 1024,  // 1GB instead of default 256MB
    ..Default::default()
}
```

### For Long-Running Servers
```rust
// More aggressive compaction
let compactor = Compactor::new(
    store,
    CompactionMode::Periodic(Duration::from_secs(60)),  // Every minute
    Duration::from_secs(300),  // Keep only 5 minutes
);
```

## Monitoring

```rust
let rev = store.current_revision();
let compact = store.compact_revision();
let size = backend.size();

println!("Current: {}, Compact: {}, Size: {} bytes", rev, compact, size);
```

## Testing

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_store() -> Arc<MvccStore> {
        let temp = TempDir::new().unwrap();
        let backend = Backend::new(BackendConfig {
            data_dir: temp.path().to_path_buf(),
            cache_size_mb: 64,
            ..Default::default()
        }).unwrap();
        Arc::new(MvccStore::new(backend).unwrap())
    }

    #[test]
    fn test_basic_put_get() {
        let store = setup_store();
        let (_, kv) = store.put(b"key", b"value", 0).unwrap();
        assert_eq!(kv.value, b"value");
    }
}
```

## Files and Location

- Core implementation: `/src/storage/`
  - `mod.rs` - Module exports
  - `backend.rs` - Sled backend
  - `mvcc.rs` - MVCC store
  - `index.rs` - Key index
  - `compaction.rs` - Background compaction

- Documentation: Root directory
  - `STORAGE_DESIGN.md` - Design documentation
  - `INTEGRATION_GUIDE.md` - Integration instructions
  - `MVCC_IMPLEMENTATION.md` - Implementation summary
  - `STORAGE_QUICK_REFERENCE.md` - This file

## Frequently Asked Questions

**Q: Do I need to manually manage compaction?**
A: No, Compactor handles it automatically. Just create and spawn it.

**Q: Can I read historical data?**
A: Yes, pass `revision` parameter to `range()`. 0 means current.

**Q: What happens when a key is updated?**
A: `mod_revision` increases, `version` increases, `create_revision` stays same.

**Q: Is it thread-safe?**
A: Yes, all components use Arc/RwLock/AtomicI64. No manual locking needed.

**Q: How do I integrate with my API service?**
A: See INTEGRATION_GUIDE.md for detailed examples.

**Q: Can I run multiple stores simultaneously?**
A: Yes, just use different data_dir for each BackendConfig.

**Q: What's the minimum cache_size_mb?**
A: 64MB works for small tests, 256MB+ recommended for production.

**Q: How often should I compact?**
A: Every 10 minutes is reasonable, depends on write rate.

## Support

For issues or questions, see:
- `STORAGE_DESIGN.md` for architecture details
- `INTEGRATION_GUIDE.md` for integration help
- Unit tests in respective `.rs` files for examples
