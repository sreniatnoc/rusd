//! In-memory key index for MVCC (equivalent to etcd's treeIndex).
//!
//! The KeyIndex maintains a BTreeMap of all keys to their revision history.
//! This allows efficient point-in-time reads and range queries at specific revisions.
//!
//! Key design:
//! - Uses generations to track the lifecycle of keys (created, updated, deleted)
//! - Each generation contains a list of revisions where modifications occurred
//! - Supports fast lookups of "key at revision N"
//! - Compaction removes old revisions to bound memory usage

use std::collections::BTreeMap;
use tracing::debug;

/// Represents a specific revision (main, sub).
/// In etcd, main is the global revision counter, sub is the operation within that revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Revision {
    /// Main revision number (global counter)
    pub main: i64,
    /// Sub-revision (operation index within a main revision)
    pub sub: i64,
}

impl Revision {
    pub fn new(main: i64, sub: i64) -> Self {
        Self { main, sub }
    }

    /// Compares revisions. Returns Ordering based on main first, then sub.
    pub fn cmp(&self, other: &Revision) -> std::cmp::Ordering {
        match self.main.cmp(&other.main) {
            std::cmp::Ordering::Equal => self.sub.cmp(&other.sub),
            other => other,
        }
    }
}

/// A single generation of a key's lifecycle.
///
/// A generation starts when a key is created and ends when it's deleted.
/// Each generation contains a list of revisions where the key was modified.
#[derive(Clone, Debug)]
struct Generation {
    /// The revision when this generation was created
    created: Revision,

    /// All revisions where this key was modified (including creation)
    revisions: Vec<Revision>,

    /// The revision when this generation was deleted (None if still alive)
    deleted: Option<Revision>,
}

/// Index entry for a single key.
#[derive(Clone, Debug)]
struct KeyIndexEntry {
    /// List of generations (lifecycle stages) for this key
    generations: Vec<Generation>,
}

impl KeyIndexEntry {
    fn new() -> Self {
        Self {
            generations: Vec::new(),
        }
    }

    /// Gets the generation that was alive at the given revision.
    fn get_generation_at(&self, revision: i64) -> Option<&Generation> {
        // Find the generation that was created before or at this revision
        // and deleted after this revision (or not deleted yet)
        self.generations.iter().find(|gen| {
            gen.created.main <= revision && gen.deleted.map_or(true, |del| del.main > revision)
        })
    }
}

/// The in-memory key index for MVCC operations.
///
/// This structure tracks all keys and their revision history, enabling:
/// - Point-in-time reads at any historical revision
/// - Efficient range queries
/// - Compaction to remove old revisions
pub struct KeyIndex {
    /// Map from key to its index entry
    tree: BTreeMap<Vec<u8>, KeyIndexEntry>,
}

impl KeyIndex {
    /// Creates a new empty key index.
    pub fn new() -> Self {
        Self {
            tree: BTreeMap::new(),
        }
    }

    /// Gets the revision of a key at a specific point in time.
    ///
    /// Returns the highest revision <= the given revision where the key existed.
    /// Returns None if the key didn't exist at that revision.
    pub fn get(&self, key: &[u8], revision: i64) -> Option<Revision> {
        let entry = self.tree.get(key)?;

        // Find the generation alive at this revision
        let gen = entry.get_generation_at(revision)?;

        // Find the highest revision in this generation that's <= the requested revision
        gen.revisions.iter().rfind(|r| r.main <= revision).copied()
    }

    /// Records a new modification (write) of a key.
    ///
    /// If this is the first write to the key, creates a new generation.
    /// Otherwise, adds to the current generation.
    pub fn put(&mut self, key: &[u8], revision: Revision) {
        let entry = self
            .tree
            .entry(key.to_vec())
            .or_insert_with(KeyIndexEntry::new);

        // Check if we need to create a new generation
        if entry.generations.is_empty() || entry.generations.last().unwrap().deleted.is_some() {
            // Start a new generation
            entry.generations.push(Generation {
                created: revision,
                revisions: vec![revision],
                deleted: None,
            });
        } else {
            // Add to the current (latest) generation
            entry
                .generations
                .last_mut()
                .unwrap()
                .revisions
                .push(revision);
        }

        debug!(
            "Index: put key {:?} at revision {:?}",
            String::from_utf8_lossy(key),
            revision
        );
    }

    /// Records a deletion of a key.
    ///
    /// Creates a "tombstone" mark indicating the key was deleted at this revision.
    pub fn tombstone(&mut self, key: &[u8], revision: Revision) {
        let entry = self
            .tree
            .entry(key.to_vec())
            .or_insert_with(KeyIndexEntry::new);

        // The current generation should be deleted
        if let Some(gen) = entry.generations.last_mut() {
            if gen.deleted.is_none() {
                gen.deleted = Some(revision);
            }
        } else {
            // Create a generation for the deleted key
            entry.generations.push(Generation {
                created: revision,
                revisions: vec![revision],
                deleted: Some(revision),
            });
        }

        debug!(
            "Index: tombstone key {:?} at revision {:?}",
            String::from_utf8_lossy(key),
            revision
        );
    }

    /// Returns all keys (and their revisions) in a range at a specific point in time.
    ///
    /// Returns key-revision pairs for all keys where start <= key < end,
    /// where the key was alive at the given revision.
    pub fn range(&self, start: &[u8], end: &[u8], revision: i64) -> Vec<(Vec<u8>, Revision)> {
        let mut results = Vec::new();

        // Use range to iterate over keys in the specified range
        let range_iter = if end.is_empty() {
            // If end is empty, it means unbounded (to the end of the range)
            self.tree.range(start.to_vec()..)
        } else {
            self.tree.range(start.to_vec()..end.to_vec())
        };

        for (key, entry) in range_iter {
            if let Some(rev) = entry.get_generation_at(revision) {
                if let Some(key_rev) = rev.revisions.iter().rfind(|r| r.main <= revision) {
                    results.push((key.clone(), *key_rev));
                }
            }
        }

        results
    }

    /// Compacts the index by removing all revisions before the given revision.
    ///
    /// After compaction, point-in-time reads at revisions < compact_revision will fail.
    /// This helps reclaim memory used by old revisions.
    pub fn compact(&mut self, compact_revision: i64) {
        for entry in self.tree.values_mut() {
            for gen in &mut entry.generations {
                // Remove revisions before the compact point
                gen.revisions.retain(|rev| rev.main >= compact_revision);

                // If all revisions in a generation are removed and it's not deleted,
                // mark a synthetic creation at compact point
                if gen.revisions.is_empty() && gen.deleted.is_none() {
                    gen.created.main = compact_revision;
                }
            }
        }

        debug!("Index: compacted at revision {}", compact_revision);
    }

    /// Counts the total number of revisions for a key.
    ///
    /// Useful for version tracking.
    pub fn count_revisions(&self, key: &[u8]) -> usize {
        self.tree
            .get(key)
            .map(|entry| {
                entry
                    .generations
                    .iter()
                    .map(|gen| gen.revisions.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Returns the total number of keys in the index.
    pub fn len(&self) -> usize {
        self.tree.len()
    }

    /// Checks if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// Returns an iterator over all keys.
    pub fn keys(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.tree.keys()
    }

    /// Removes all data from the index.
    pub fn clear(&mut self) {
        self.tree.clear();
    }
}

impl Default for KeyIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_put_and_get() {
        let mut index = KeyIndex::new();

        // Put key at revision (1, 0)
        index.put(b"key1", Revision::new(1, 0));

        // Get key at revision 1
        assert_eq!(index.get(b"key1", 1), Some(Revision::new(1, 0)));

        // Get key at revision 0 (before it existed)
        assert_eq!(index.get(b"key1", 0), None);

        // Get key at revision 2 (after it existed)
        assert_eq!(index.get(b"key1", 2), Some(Revision::new(1, 0)));
    }

    #[test]
    fn test_multiple_updates() {
        let mut index = KeyIndex::new();

        // Initial write
        index.put(b"key1", Revision::new(1, 0));

        // Update at revision 3
        index.put(b"key1", Revision::new(3, 0));

        // At revision 2, should see revision 1
        assert_eq!(index.get(b"key1", 2), Some(Revision::new(1, 0)));

        // At revision 3, should see revision 3
        assert_eq!(index.get(b"key1", 3), Some(Revision::new(3, 0)));

        // At revision 5, should still see revision 3
        assert_eq!(index.get(b"key1", 5), Some(Revision::new(3, 0)));
    }

    #[test]
    fn test_tombstone() {
        let mut index = KeyIndex::new();

        index.put(b"key1", Revision::new(1, 0));
        index.tombstone(b"key1", Revision::new(3, 0));

        // At revision 2, key should exist
        assert_eq!(index.get(b"key1", 2), Some(Revision::new(1, 0)));

        // At revision 3 or later, key should not exist
        assert_eq!(index.get(b"key1", 3), None);
    }

    #[test]
    fn test_range() {
        let mut index = KeyIndex::new();

        index.put(b"a", Revision::new(1, 0));
        index.put(b"b", Revision::new(1, 0));
        index.put(b"c", Revision::new(1, 0));
        index.put(b"d", Revision::new(1, 0));

        let results = index.range(b"b", b"d", 1);
        assert_eq!(results.len(), 2); // Should contain "b" and "c"

        assert_eq!(results[0].0, b"b");
        assert_eq!(results[1].0, b"c");
    }

    #[test]
    fn test_compaction() {
        let mut index = KeyIndex::new();

        index.put(b"key1", Revision::new(1, 0));
        index.put(b"key1", Revision::new(3, 0));
        index.put(b"key1", Revision::new(5, 0));

        assert_eq!(index.count_revisions(b"key1"), 3);

        // Compact at revision 4
        index.compact(4);

        // Count should decrease
        assert_eq!(index.count_revisions(b"key1"), 1); // Only revision 5 remains
    }

    #[test]
    fn test_generation_lifecycle() {
        let mut index = KeyIndex::new();

        // First generation
        index.put(b"key1", Revision::new(1, 0));

        // Delete
        index.tombstone(b"key1", Revision::new(2, 0));

        // Recreate (new generation)
        index.put(b"key1", Revision::new(3, 0));

        // At revision 1, should exist
        assert_eq!(index.get(b"key1", 1), Some(Revision::new(1, 0)));

        // At revision 2, should be deleted
        assert_eq!(index.get(b"key1", 2), None);

        // At revision 3, should exist again
        assert_eq!(index.get(b"key1", 3), Some(Revision::new(3, 0)));
    }
}
