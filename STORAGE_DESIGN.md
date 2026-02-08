# MVCC Storage Engine Design

## Overview

The rusd storage engine implements Multi-Version Concurrency Control (MVCC) semantics, providing etcd-compatible key-value storage with strong consistency guarantees and excellent performance characteristics.

### Key Design Principles

1. **Memory Efficiency**: Configurable page cache (default 256MB) instead of etcd's unbounded mmap
2. **Write Performance**: Atomic batch operations for multi-key updates without single-writer bottlenecks
3. **Read Performance**: Lock-free B+ tree (sled) for concurrent reads without blocking writes
4. **Historical Reads**: Point-in-time reads at any past revision

## Architecture

### Components

#### 1. Backend (`backend.rs`)
The low-level persistent storage using sled. Manages multiple logical trees:
- **kv**: Main key-value data storage
- **index**: Key-to-revision mappings
- **meta**: Cluster metadata
- **lease**: TTL lease data
- **auth**: Authentication and authorization

**Key Features**:
- Atomic batch writes
- Configurable cache size
- Snapshot/restore for cluster replication
- Non-blocking concurrent reads

#### 2. MVCC Store (`mvcc.rs`)
The core versioning engine providing:
- Monotonically increasing global revision counter
- KeyValue structs with metadata (create_revision, mod_revision, version)
- Range queries at specific revisions
- Transaction support
- Compaction for old revisions

**KeyValue Structure**:
```rust
pub struct KeyValue {
    pub key: Vec<u8>,              // The key
    pub create_revision: i64,       // When created
    pub mod_revision: i64,          // Last modification
    pub version: i64,               // Number of updates to this key
    pub value: Vec<u8>,             // The actual value
    pub lease: i64,                 // Associated lease (0 = no lease)
}
```

#### 3. Key Index (`index.rs`)
In-memory BTreeMap maintaining revision history for each key. Enables:
- Fast "key at revision N" lookups
- Generation tracking (lifecycle of keys)
- Compaction of old revisions

**Revision Encoding**:
- Each revision has a `main` (global counter) and `sub` (operation index)
- This matches etcd's revision semantics

#### 4. Compactor (`compaction.rs`)
Background worker that:
- Removes old revisions beyond the compact point
- Reclaims disk and memory space
- Supports periodic or one-shot modes
- Non-blocking operation

## Data Model

### Revisions

Every write operation increments the global revision counter:

```
Revision = (main, sub)
  main: Global monotonically increasing counter
  sub:  Operation index within that main revision
```

Example sequence:
```
Operation     Revision   Key     Value
Put key1      (1, 0)     key1    value1
Put key2      (2, 0)     key2    value2
Delete key1   (3, 0)     key1    (deleted)
Put key1      (4, 0)     key1    value1_new
```

### Point-in-Time Reads

Each revision creates a consistent snapshot:

```
Range at revision 2: returns {key2: value2} (key1 deleted)
Range at revision 4: returns {key1: value1_new, key2: value2}
```

### Key Generations

A key can have multiple generations (creation -> updates -> deletion -> recreation):

```
Generation 1: Revisions 1, 3 (created at 1, updated at 3)
  Created: (1, 0)
  Deleted: (3, 0)

Generation 2: Revision 4 (recreated)
  Created: (4, 0)
  Deleted: None (still alive)
```

## Performance Characteristics

### Memory Usage

- Backend cache: Configurable (default 256MB vs etcd's unbounded)
- Key index: One entry per unique key in current snapshot
- Write batch: Temporary, cleared after flush

### Write Path (put operation)

1. Allocate next revision O(1)
2. Encode KeyValue O(k) where k = value size
3. Store in backend O(log n) where n = total keys
4. Update index O(log m) where m = revisions for this key

**Total**: O(log n + k) amortized

### Read Path (range query)

1. Query index at revision O(log m)
2. Lookup values in backend O(q log n) where q = results
3. Total: O(q log n) for q results

This is *non-blocking*: readers don't block writers (sled uses lock-free B+ trees).

### Compaction

Removes all revisions < compact_revision:
- O(n log m) where n = keys, m = avg revisions per key
- Background task, non-blocking
- Typical: seconds to minutes depending on dataset size

## Usage Example

### Basic Put/Get

```rust
use rusd::storage::{Backend, BackendConfig, MvccStore};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize backend
    let config = BackendConfig {
        data_dir: std::path::PathBuf::from("./data"),
        cache_size_mb: 256,
        flush_interval_ms: 1000,
        compression: true,
    };

    let backend = Backend::new(config)?;
    let store = MvccStore::new(backend)?;

    // Write a key
    let (rev1, kv1) = store.put(b"mykey", b"myvalue", 0)?;
    println!("Created key at revision {}", rev1);

    // Query at current revision
    let result = store.range(b"mykey", b"mykeyZ", 0, 10, false)?;
    assert_eq!(result.kvs[0].value, b"myvalue");

    // Update the key
    let (rev2, kv2) = store.put(b"mykey", b"newvalue", 0)?;
    assert_eq!(kv2.version, 2);
    assert_eq!(kv1.create_revision, kv2.create_revision);

    Ok(())
}
```

### Range Queries

```rust
// Query a range
let result = store.range(
    b"key1",           // start
    b"key9",           // end (exclusive)
    0,                 // revision (0 = current)
    100,               // limit
    false              // count_only
)?;

println!("Found {} keys", result.kvs.len());
for kv in result.kvs {
    println!("  {}: {} (v={})",
        String::from_utf8_lossy(&kv.key),
        String::from_utf8_lossy(&kv.value),
        kv.version
    );
}
```

### Point-in-Time Reads

```rust
// Get current revision
let current_rev = store.current_revision();

// Modify data
store.put(b"key1", b"value1", 0)?;
store.put(b"key2", b"value2", 0)?;
store.put(b"key1", b"value1_updated", 0)?;

// Read as of the snapshot before the update
let past_result = store.range(b"key1", b"key2", current_rev, 10, false)?;
// This returns the state from earlier revision
```

### Compaction

```rust
use rusd::storage::{Compactor, CompactionMode};
use std::time::Duration;

// Create periodic compactor (every 10 seconds)
let compactor = Compactor::new(
    store.clone(),
    CompactionMode::Periodic(Duration::from_secs(10)),
    Duration::from_secs(60), // Keep last 60 seconds of data
);

// Run in background
let handle = compactor.run();

// Or manually trigger compaction to specific revision
let target_rev = store.current_revision() - 1000;
compactor.compact_to(target_rev).await?;
```

## Design Decisions vs etcd

### 1. sled vs bbolt

| Aspect | etcd (bbolt) | rusd (sled) |
|--------|--------------|-----------|
| Memory | Unbounded mmap | Configurable cache (256MB default) |
| Reads | Blocked during writes | Non-blocking lock-free |
| Writes | Single writer, atomic | Atomic batches |
| Concurrency | Mutex per bucket | Lock-free B+ tree |

### 2. Key Index Optimization

Unlike etcd's treeIndex, we use:
- Generation-based lifecycle tracking
- Efficient sub-revision arrays
- Fast point-in-time queries without full B+ tree traversal

### 3. Batch Operations

For multi-key writes:
- etcd: Multiple sequential transactions (may block readers)
- rusd: Single atomic batch (non-blocking)

## Consistency Guarantees

1. **Serializability**: Each revision is a consistent snapshot
2. **Durability**: All writes persisted to disk (sled's fsync)
3. **Atomicity**: Batch operations are all-or-nothing
4. **Isolation**: Readers never see partial updates
5. **Consistency**: Global revision counter ensures ordering

## Future Optimizations

1. **Write coalescing**: Batch multiple operations into single revision
2. **Compression**: Value compression for large blobs
3. **Tiered storage**: Cold revisions to slower storage
4. **Incremental snapshots**: Delta-based cluster replication
5. **Bloom filters**: Avoid negative lookups in deleted keyspaces

## Testing

Comprehensive unit tests included for:
- Backend: put/get/delete, batch operations, snapshots
- Index: generation tracking, point-in-time reads, compaction
- MVCC: write/delete semantics, range queries, revisions
- Compaction: periodic and one-shot modes

Run tests with:
```bash
cargo test --lib storage --release
```

## Benchmarking

Create benchmarks with:
```bash
cargo bench --bench rusd_bench -- storage
```

Expected performance:
- Writes: 100k-500k ops/sec (SSD)
- Reads: 1M+ ops/sec (lock-free)
- Range: 100k-300k ranges/sec
