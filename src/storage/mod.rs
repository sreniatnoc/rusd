//! Storage engine module providing MVCC (Multi-Version Concurrency Control) semantics.
//!
//! The storage module implements etcd's core versioning and consistency model using sled as
//! the underlying key-value store. Key design improvements over etcd's bbolt:
//!
//! 1. **Memory efficiency**: Configurable page cache instead of unbounded mmap
//! 2. **Write performance**: Atomic batch operations for multi-key updates
//! 3. **Read performance**: Lock-free B+ tree for concurrent reads without blocking
//!
//! The MVCC model ensures:
//! - Every write creates a monotonically increasing revision number
//! - Point-in-time reads at any historical revision
//! - Efficient range queries with prefix scanning
//! - Background compaction to reclaim storage space

pub mod backend;
pub mod mvcc;
pub mod index;
pub mod compaction;

pub use backend::{Backend, BackendConfig, BackendError, BackendResult};
pub use mvcc::{MvccStore, KeyValue, RangeResult, Event};
pub use index::{KeyIndex, Revision};
pub use compaction::{Compactor, CompactionMode};

use thiserror::Error;

/// Storage engine errors.
#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Backend error: {0}")]
    Backend(#[from] BackendError),

    #[error("MVCC error: {0}")]
    Mvcc(String),

    #[error("Index error: {0}")]
    Index(String),

    #[error("Compaction error: {0}")]
    Compaction(String),

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("Key not found")]
    NotFound,

    #[error("Conflict")]
    Conflict,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type StorageResult<T> = Result<T, StorageError>;
