use super::{RaftError, Result as RaftResult};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub index: u64,
    pub term: u64,
    pub data: Vec<u8>,
    pub entry_type: EntryType,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum EntryType {
    Normal,
    ConfigChange,
    Snapshot,
}

pub struct RaftLog {
    db: sled::Tree,
    first_index: AtomicU64,
    last_index: AtomicU64,
    last_term: AtomicU64,
    snapshot_index: AtomicU64,
    snapshot_term: AtomicU64,
    /// Cached snapshot data for sending to followers
    snapshot_data: parking_lot::Mutex<Option<Vec<u8>>>,
}

impl RaftLog {
    pub fn new(db: sled::Tree) -> RaftResult<Self> {
        let log = Self {
            db,
            first_index: AtomicU64::new(1),
            last_index: AtomicU64::new(0),
            last_term: AtomicU64::new(0),
            snapshot_index: AtomicU64::new(0),
            snapshot_term: AtomicU64::new(0),
            snapshot_data: parking_lot::Mutex::new(None),
        };

        // Recover state from disk
        log.recover()?;
        Ok(log)
    }

    fn recover(&self) -> RaftResult<()> {
        // Try to load metadata
        if let Ok(Some(snapshot_idx)) = self.db.get("snapshot_index") {
            let idx = u64::from_le_bytes(
                snapshot_idx
                    .to_vec()
                    .try_into()
                    .map_err(|_| RaftError::LogError("Invalid snapshot_index size".to_string()))?,
            );
            self.snapshot_index.store(idx, Ordering::Release);
            self.first_index.store(idx + 1, Ordering::Release);
        }

        if let Ok(Some(snapshot_term)) = self.db.get("snapshot_term") {
            let term = u64::from_le_bytes(
                snapshot_term
                    .to_vec()
                    .try_into()
                    .map_err(|_| RaftError::LogError("Invalid snapshot_term size".to_string()))?,
            );
            self.snapshot_term.store(term, Ordering::Release);
        }

        // Find last index and term by scanning
        if let Ok(Some(last_idx)) = self.db.get("last_index") {
            let idx = u64::from_le_bytes(
                last_idx
                    .to_vec()
                    .try_into()
                    .map_err(|_| RaftError::LogError("Invalid last_index size".to_string()))?,
            );
            self.last_index.store(idx, Ordering::Release);

            // Get the term of the last entry
            if let Ok(Some(entry_bytes)) = self.db.get(format!("entry:{}", idx)) {
                if let Ok(entry) = bincode::deserialize::<LogEntry>(&entry_bytes) {
                    self.last_term.store(entry.term, Ordering::Release);
                }
            }
        }

        Ok(())
    }

    pub fn append(&self, entries: &[LogEntry]) -> RaftResult<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut batch = sled::Batch::default();

        for entry in entries {
            let key = format!("entry:{}", entry.index);
            let value = bincode::serialize(entry)
                .map_err(|e| RaftError::LogError(format!("Serialize error: {}", e)))?;
            batch.insert(key.as_bytes(), value);
        }

        // Update last_index and last_term
        let last_entry = &entries[entries.len() - 1];
        batch.insert(b"last_index", last_entry.index.to_le_bytes().to_vec());
        batch.insert(b"last_term", last_entry.term.to_le_bytes().to_vec());

        self.db
            .apply_batch(batch)
            .map_err(|e| RaftError::LogError(format!("Apply batch error: {}", e)))?;

        self.last_index.store(last_entry.index, Ordering::Release);
        self.last_term.store(last_entry.term, Ordering::Release);

        Ok(())
    }

    pub fn get(&self, index: u64) -> RaftResult<Option<LogEntry>> {
        if index < self.first_index.load(Ordering::Acquire) {
            return Ok(None);
        }

        let key = format!("entry:{}", index);
        match self.db.get(&key) {
            Ok(Some(bytes)) => {
                let entry = bincode::deserialize::<LogEntry>(&bytes)
                    .map_err(|e| RaftError::LogError(format!("Deserialize error: {}", e)))?;
                Ok(Some(entry))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(RaftError::LogError(format!("Database error: {}", e))),
        }
    }

    pub fn get_range(&self, start: u64, end: u64) -> RaftResult<Vec<LogEntry>> {
        let first = self.first_index.load(Ordering::Acquire);
        let last = self.last_index.load(Ordering::Acquire);

        if start > last || end < first {
            return Ok(Vec::new());
        }

        let actual_start = std::cmp::max(start, first);
        let actual_end = std::cmp::min(end, last + 1);

        let mut entries = Vec::new();
        for index in actual_start..actual_end {
            if let Some(entry) = self.get(index)? {
                entries.push(entry);
            }
        }

        Ok(entries)
    }

    pub fn truncate_after(&self, index: u64) -> RaftResult<()> {
        let last = self.last_index.load(Ordering::Acquire);

        if index >= last {
            return Ok(());
        }

        // Delete all entries after the given index
        for i in (index + 1)..=last {
            let key = format!("entry:{}", i);
            self.db
                .remove(&key)
                .map_err(|e| RaftError::LogError(format!("Delete error: {}", e)))?;
        }

        // Update metadata
        let mut batch = sled::Batch::default();
        batch.insert(b"last_index", index.to_le_bytes().to_vec());

        // Get the term of the new last entry
        if let Some(entry) = self.get(index)? {
            batch.insert(b"last_term", entry.term.to_le_bytes().to_vec());
            self.last_term.store(entry.term, Ordering::Release);
        }

        self.db
            .apply_batch(batch)
            .map_err(|e| RaftError::LogError(format!("Apply batch error: {}", e)))?;

        self.last_index.store(index, Ordering::Release);

        Ok(())
    }

    pub fn truncate_before(&self, index: u64) -> RaftResult<()> {
        let first = self.first_index.load(Ordering::Acquire);

        if index <= first {
            return Ok(());
        }

        // Delete all entries before the given index
        for i in first..index {
            let key = format!("entry:{}", i);
            self.db
                .remove(&key)
                .map_err(|e| RaftError::LogError(format!("Delete error: {}", e)))?;
        }

        self.first_index.store(index, Ordering::Release);

        Ok(())
    }

    pub fn last_index(&self) -> u64 {
        self.last_index.load(Ordering::Acquire)
    }

    pub fn last_term(&self) -> u64 {
        self.last_term.load(Ordering::Acquire)
    }

    pub fn first_index(&self) -> u64 {
        self.first_index.load(Ordering::Acquire)
    }

    pub fn term_at(&self, index: u64) -> RaftResult<Option<u64>> {
        if index == 0 {
            return Ok(Some(0));
        }

        if index < self.first_index.load(Ordering::Acquire) {
            return Ok(Some(self.snapshot_term.load(Ordering::Acquire)));
        }

        match self.get(index)? {
            Some(entry) => Ok(Some(entry.term)),
            None => Ok(None),
        }
    }

    pub fn has_entry(&self, index: u64, term: u64) -> bool {
        match self.term_at(index) {
            Ok(Some(t)) => t == term,
            _ => false,
        }
    }

    pub fn save_snapshot(&self, index: u64, term: u64) -> RaftResult<()> {
        let mut batch = sled::Batch::default();
        batch.insert(b"snapshot_index", index.to_le_bytes().to_vec());
        batch.insert(b"snapshot_term", term.to_le_bytes().to_vec());

        self.db
            .apply_batch(batch)
            .map_err(|e| RaftError::LogError(format!("Apply batch error: {}", e)))?;

        self.snapshot_index.store(index, Ordering::Release);
        self.snapshot_term.store(term, Ordering::Release);
        self.first_index.store(index + 1, Ordering::Release);

        Ok(())
    }

    pub fn snapshot_index(&self) -> u64 {
        self.snapshot_index.load(Ordering::Acquire)
    }

    pub fn snapshot_term(&self) -> u64 {
        self.snapshot_term.load(Ordering::Acquire)
    }

    /// Saves a snapshot with both metadata and data.
    pub fn save_snapshot_with_data(&self, index: u64, term: u64, data: Vec<u8>) -> RaftResult<()> {
        self.save_snapshot(index, term)?;
        *self.snapshot_data.lock() = Some(data);
        Ok(())
    }

    /// Returns the latest snapshot data if available.
    pub fn get_snapshot_data(&self) -> Option<Vec<u8>> {
        self.snapshot_data.lock().clone()
    }
}
