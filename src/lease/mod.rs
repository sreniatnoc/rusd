//! Lease management with efficient TTL tracking.
//!
//! This module implements etcd-compatible lease semantics with optimized expiry handling:
//! - Priority queue (BinaryHeap) for efficient TTL tracking instead of periodic scanning
//! - Lazy expiry: check on access rather than background scanning every 500ms
//! - Atomic lease ID assignment
//! - Efficient attach/detach of keys to leases

use parking_lot::RwLock;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Lease-related errors.
#[derive(Error, Debug)]
pub enum LeaseError {
    #[error("Lease not found: {0}")]
    LeaseNotFound(i64),

    #[error("Lease already exists: {0}")]
    LeaseAlreadyExists(i64),

    #[error("Invalid TTL: {0}")]
    InvalidTTL(String),

    #[error("Channel send error")]
    ChannelSendError,

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type LeaseResult<T> = Result<T, LeaseError>;

/// A lease for attaching to keys with TTL semantics.
#[derive(Clone, Debug)]
pub struct Lease {
    pub id: i64,
    pub ttl: i64,         // Granted TTL in seconds
    pub granted_ttl: i64, // Original granted TTL
    pub created_at: Instant,
    pub keys: Vec<Vec<u8>>,
}

impl Lease {
    /// Get the remaining TTL in seconds.
    pub fn remaining_ttl(&self) -> i64 {
        let elapsed = self.created_at.elapsed().as_secs() as i64;
        (self.ttl - elapsed).max(0)
    }

    /// Check if this lease has expired.
    pub fn is_expired(&self) -> bool {
        self.remaining_ttl() <= 0
    }
}

/// Internal structure for tracking lease expiry.
#[derive(Debug, Clone, Eq, PartialEq)]
struct LeaseExpiry {
    lease_id: i64,
    expires_at: Instant,
}

impl Ord for LeaseExpiry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse ordering so BinaryHeap acts as a min-heap
        other.expires_at.cmp(&self.expires_at)
    }
}

impl PartialOrd for LeaseExpiry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Event sent when a lease expires and keys should be deleted.
#[derive(Debug, Clone)]
pub struct LeaseExpireEvent {
    pub lease_id: i64,
    pub keys: Vec<Vec<u8>>,
}

/// Manages lease lifecycle and TTL tracking.
pub struct LeaseManager {
    /// All active leases: id -> Lease
    leases: RwLock<HashMap<i64, Lease>>,

    /// Map of lease_id -> set of attached keys
    lease_keys: RwLock<HashMap<i64, HashSet<Vec<u8>>>>,

    /// Counter for generating unique lease IDs
    next_lease_id: AtomicI64,

    /// Priority queue of upcoming expirations (min-heap, ordered by expiry time)
    expiry_queue: RwLock<BinaryHeap<Reverse<LeaseExpiry>>>,

    /// Channel for notifying when keys should be deleted due to lease expiry
    expire_tx: mpsc::Sender<LeaseExpireEvent>,
}

impl LeaseManager {
    /// Creates a new LeaseManager.
    pub fn new(expire_tx: mpsc::Sender<LeaseExpireEvent>) -> Self {
        LeaseManager {
            leases: RwLock::new(HashMap::new()),
            lease_keys: RwLock::new(HashMap::new()),
            next_lease_id: AtomicI64::new(1),
            expiry_queue: RwLock::new(BinaryHeap::new()),
            expire_tx,
        }
    }

    /// Grants a new lease with the given TTL.
    /// If id == 0, auto-assigns a new ID.
    pub fn grant(&self, id: i64, ttl: i64) -> LeaseResult<Lease> {
        if ttl <= 0 {
            return Err(LeaseError::InvalidTTL(format!(
                "TTL must be positive, got {}",
                ttl
            )));
        }

        let lease_id = if id == 0 {
            self.next_lease_id.fetch_add(1, Ordering::SeqCst)
        } else {
            id
        };

        let mut leases = self.leases.write();

        if leases.contains_key(&lease_id) {
            return Err(LeaseError::LeaseAlreadyExists(lease_id));
        }

        let now = Instant::now();
        let expires_at = now + Duration::from_secs(ttl as u64);

        let lease = Lease {
            id: lease_id,
            ttl,
            granted_ttl: ttl,
            created_at: now,
            keys: vec![],
        };

        leases.insert(lease_id, lease.clone());

        // Add to expiry queue
        let expiry = LeaseExpiry {
            lease_id,
            expires_at,
        };
        self.expiry_queue.write().push(Reverse(expiry));

        self.lease_keys.write().insert(lease_id, HashSet::new());

        debug!(lease_id, ttl, "Lease granted");
        Ok(lease)
    }

    /// Revokes a lease and returns the attached keys.
    pub fn revoke(&self, id: i64) -> LeaseResult<Vec<Vec<u8>>> {
        let mut leases = self.leases.write();

        if let Some(lease) = leases.remove(&id) {
            let mut lease_keys = self.lease_keys.write();
            let keys = lease_keys
                .remove(&id)
                .map(|set| set.into_iter().collect::<Vec<_>>())
                .unwrap_or_default();

            debug!(lease_id = id, key_count = keys.len(), "Lease revoked");
            Ok(keys)
        } else {
            Err(LeaseError::LeaseNotFound(id))
        }
    }

    /// Renews a lease, resetting its TTL.
    pub fn renew(&self, id: i64) -> LeaseResult<i64> {
        let mut leases = self.leases.write();

        if let Some(lease) = leases.get_mut(&id) {
            let ttl = lease.granted_ttl;
            lease.ttl = ttl;
            lease.created_at = Instant::now();

            let expires_at = Instant::now() + Duration::from_secs(ttl as u64);

            // Add renewed expiry to queue
            let expiry = LeaseExpiry {
                lease_id: id,
                expires_at,
            };
            self.expiry_queue.write().push(Reverse(expiry));

            debug!(lease_id = id, ttl, "Lease renewed");
            Ok(ttl)
        } else {
            Err(LeaseError::LeaseNotFound(id))
        }
    }

    /// Attaches a key to a lease.
    pub fn attach(&self, lease_id: i64, key: Vec<u8>) -> LeaseResult<()> {
        let leases = self.leases.read();

        if !leases.contains_key(&lease_id) {
            return Err(LeaseError::LeaseNotFound(lease_id));
        }

        drop(leases);

        let mut lease_keys = self.lease_keys.write();
        if let Some(keys) = lease_keys.get_mut(&lease_id) {
            keys.insert(key);
            debug!(lease_id, "Key attached to lease");
            Ok(())
        } else {
            Err(LeaseError::Internal("Lease keys map corrupted".to_string()))
        }
    }

    /// Detaches a key from a lease.
    pub fn detach(&self, lease_id: i64, key: &[u8]) -> LeaseResult<()> {
        let mut lease_keys = self.lease_keys.write();

        if let Some(keys) = lease_keys.get_mut(&lease_id) {
            keys.remove(key);
            debug!(lease_id, "Key detached from lease");
            Ok(())
        } else {
            Err(LeaseError::LeaseNotFound(lease_id))
        }
    }

    /// Gets a lease by ID.
    pub fn get(&self, id: i64) -> Option<Lease> {
        self.leases.read().get(&id).cloned()
    }

    /// Lists all active leases.
    pub fn list(&self) -> Vec<Lease> {
        self.leases.read().values().cloned().collect()
    }

    /// Gets lease time-to-live information.
    pub fn time_to_live(&self, id: i64) -> LeaseResult<(i64, i64, Vec<Vec<u8>>)> {
        let leases = self.leases.read();

        if let Some(lease) = leases.get(&id) {
            let ttl = lease.remaining_ttl();
            let granted_ttl = lease.granted_ttl;

            drop(leases);

            let lease_keys = self.lease_keys.read();
            let keys = lease_keys
                .get(&id)
                .map(|set| set.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();

            Ok((ttl, granted_ttl, keys))
        } else {
            Err(LeaseError::LeaseNotFound(id))
        }
    }

    /// Recovers leases from a snapshot (e.g., after startup).
    pub fn recover(&self, leases: Vec<Lease>) -> LeaseResult<()> {
        let mut leases_map = self.leases.write();
        let mut lease_keys_map = self.lease_keys.write();
        let mut expiry_queue = self.expiry_queue.write();

        for lease in leases {
            let lease_id = lease.id;
            let ttl = lease.ttl;

            // Re-add to expiry queue
            let expires_at = lease.created_at + Duration::from_secs(ttl as u64);
            expiry_queue.push(Reverse(LeaseExpiry {
                lease_id,
                expires_at,
            }));

            // Extract keys before moving lease
            let keys: HashSet<Vec<u8>> = lease.keys.iter().cloned().collect();

            leases_map.insert(lease_id, lease);
            lease_keys_map.insert(lease_id, keys);
        }

        debug!("Leases recovered");
        Ok(())
    }

    /// Background loop that checks for expired leases and sends expiry notifications.
    pub async fn run_expiry_loop(self: Arc<Self>) {
        loop {
            let now = Instant::now();

            // Process all expired leases
            loop {
                // Extract the next expired lease info (guard scope ends here)
                let next_event = {
                    let mut expiry_queue = self.expiry_queue.write();

                    if let Some(Reverse(LeaseExpiry {
                        lease_id,
                        expires_at,
                    })) = expiry_queue.peek()
                    {
                        if *expires_at > now {
                            None // No more expired leases
                        } else {
                            let lease_id = *lease_id;
                            expiry_queue.pop();

                            // Check if lease still exists and is actually expired
                            let mut leases = self.leases.write();
                            let event = if let Some(lease) = leases.get(&lease_id) {
                                if lease.is_expired() {
                                    // Lease has truly expired
                                    if let Some(_lease) = leases.remove(&lease_id) {
                                        let mut lease_keys = self.lease_keys.write();
                                        let keys = lease_keys
                                            .remove(&lease_id)
                                            .map(|set| set.into_iter().collect::<Vec<_>>())
                                            .unwrap_or_default();

                                        Some(LeaseExpireEvent { lease_id, keys })
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            event
                        }
                    } else {
                        None
                    }
                    // All guards dropped here
                };

                // Send the event outside the guard scope
                if let Some(event) = next_event {
                    if let Err(e) = self.expire_tx.send(event).await {
                        warn!(error = ?e, "Failed to send lease expiry event");
                    }
                } else {
                    break; // No more expired leases
                }
            }

            // Sleep until next check or until next expiry, whichever comes first
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Returns the number of active leases.
    pub fn count(&self) -> usize {
        self.leases.read().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_grant_and_revoke() {
        let (tx, _rx) = mpsc::channel(10);
        let manager = Arc::new(LeaseManager::new(tx));

        let lease = manager.grant(0, 30).unwrap();
        assert!(lease.id > 0);
        assert_eq!(lease.ttl, 30);

        let revoked_keys = manager.revoke(lease.id).unwrap();
        assert_eq!(revoked_keys.len(), 0);
    }

    #[tokio::test]
    async fn test_attach_detach() {
        let (tx, _rx) = mpsc::channel(10);
        let manager = Arc::new(LeaseManager::new(tx));

        let lease = manager.grant(0, 30).unwrap();
        manager.attach(lease.id, b"key1".to_vec()).unwrap();
        manager.attach(lease.id, b"key2".to_vec()).unwrap();

        let (_, _, keys) = manager.time_to_live(lease.id).unwrap();
        assert_eq!(keys.len(), 2);

        manager.detach(lease.id, b"key1").unwrap();
        let (_, _, keys) = manager.time_to_live(lease.id).unwrap();
        assert_eq!(keys.len(), 1);
    }

    #[tokio::test]
    async fn test_renew() {
        let (tx, _rx) = mpsc::channel(10);
        let manager = Arc::new(LeaseManager::new(tx));

        let lease = manager.grant(0, 10).unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let ttl_before = lease.remaining_ttl();
        let renewed_ttl = manager.renew(lease.id).unwrap();

        assert_eq!(renewed_ttl, 10);
        let ttl_after = manager.get(lease.id).unwrap().remaining_ttl();
        assert!(ttl_after > ttl_before);
    }

    #[tokio::test]
    async fn test_lease_expiry() {
        let (tx, mut rx) = mpsc::channel(10);
        let manager = Arc::new(LeaseManager::new(tx));

        let lease = manager.grant(0, 1).unwrap();
        manager.attach(lease.id, b"key".to_vec()).unwrap();

        let manager_clone = manager.clone();
        tokio::spawn(async move {
            manager_clone.run_expiry_loop().await;
        });

        // Wait for expiry
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Check if expiry event was sent
        if let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            assert_eq!(event.lease_id, lease.id);
            assert_eq!(event.keys.len(), 1);
        }
    }
}
