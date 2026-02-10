//! Cluster membership and coordination management.
//!
//! This module manages cluster topology, member discovery, and peer communication.
//! It tracks member information including peer URLs for Raft communication and
//! client URLs for API servers.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tracing::debug;
use uuid::Uuid;

/// Cluster-related errors.
#[derive(Error, Debug)]
pub enum ClusterError {
    #[error("Member not found: {0}")]
    MemberNotFound(u64),

    #[error("Member already exists: {0}")]
    MemberAlreadyExists(u64),

    #[error("Invalid member configuration: {0}")]
    InvalidConfig(String),

    #[error("Cannot remove leader")]
    CannotRemoveLeader,

    #[error("Raft error: {0}")]
    RaftError(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type ClusterResult<T> = Result<T, ClusterError>;

/// Information about a cluster member.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct Member {
    /// Unique identifier for this member
    pub id: u64,

    /// Human-readable name of the member
    pub name: String,

    /// Peer URLs used for Raft communication
    pub peer_urls: Vec<String>,

    /// Client URLs for API access
    pub client_urls: Vec<String>,

    /// Whether this is a learner member (non-voting)
    pub is_learner: bool,
}

impl Member {
    /// Creates a new member with the given parameters.
    pub fn new(id: u64, name: String, peer_urls: Vec<String>, client_urls: Vec<String>) -> Self {
        Member {
            id,
            name,
            peer_urls,
            client_urls,
            is_learner: false,
        }
    }

    /// Validates the member configuration.
    pub fn validate(&self) -> ClusterResult<()> {
        if self.id == 0 {
            return Err(ClusterError::InvalidConfig(
                "Member ID cannot be 0".to_string(),
            ));
        }

        if self.name.is_empty() {
            return Err(ClusterError::InvalidConfig(
                "Member name cannot be empty".to_string(),
            ));
        }

        if self.peer_urls.is_empty() {
            return Err(ClusterError::InvalidConfig(
                "At least one peer URL is required".to_string(),
            ));
        }

        for url in &self.peer_urls {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(ClusterError::InvalidConfig(format!(
                    "Invalid peer URL: {}",
                    url
                )));
            }
        }

        for url in &self.client_urls {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(ClusterError::InvalidConfig(format!(
                    "Invalid client URL: {}",
                    url
                )));
            }
        }

        Ok(())
    }
}

/// Manages cluster membership and topology.
pub struct ClusterManager {
    /// Unique cluster ID (128-bit)
    cluster_id: u64,

    /// ID of this member
    member_id: u64,

    /// Map of member_id -> Member
    members: RwLock<std::collections::HashMap<u64, Member>>,

    /// ID of the current leader (0 if unknown)
    leader_id: std::sync::atomic::AtomicU64,
}

impl ClusterManager {
    /// Creates a new ClusterManager.
    pub fn new(cluster_id: u64, member_id: u64) -> ClusterResult<Arc<Self>> {
        if cluster_id == 0 || member_id == 0 {
            return Err(ClusterError::InvalidConfig(
                "Cluster ID and member ID must be non-zero".to_string(),
            ));
        }

        Ok(Arc::new(ClusterManager {
            cluster_id,
            member_id,
            members: RwLock::new(std::collections::HashMap::new()),
            leader_id: std::sync::atomic::AtomicU64::new(0),
        }))
    }

    /// Gets the cluster ID.
    pub fn cluster_id(&self) -> u64 {
        self.cluster_id
    }

    /// Gets the local member ID.
    pub fn member_id(&self) -> u64 {
        self.member_id
    }

    /// Gets the current leader ID.
    pub fn leader_id(&self) -> u64 {
        self.leader_id.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Sets the current leader ID.
    pub fn set_leader(&self, id: u64) {
        self.leader_id
            .store(id, std::sync::atomic::Ordering::SeqCst);
        if id != 0 {
            debug!(leader_id = id, "Leader changed");
        }
    }

    /// Adds a new member to the cluster.
    /// If is_learner is true, the member will not participate in voting.
    pub fn add_member(
        &self,
        name: String,
        peer_urls: Vec<String>,
        client_urls: Vec<String>,
        is_learner: bool,
    ) -> ClusterResult<Member> {
        // Generate a new member ID
        let member_id = self.generate_member_id();

        let mut member = Member::new(member_id, name, peer_urls, client_urls);
        member.is_learner = is_learner;

        member.validate()?;

        let mut members = self.members.write();

        if members.contains_key(&member_id) {
            return Err(ClusterError::MemberAlreadyExists(member_id));
        }

        members.insert(member_id, member.clone());

        debug!(member_id, is_learner, "Member added to cluster");

        Ok(member)
    }

    /// Removes a member from the cluster.
    pub fn remove_member(&self, id: u64) -> ClusterResult<()> {
        if id == self.leader_id() {
            return Err(ClusterError::CannotRemoveLeader);
        }

        let mut members = self.members.write();

        if members.remove(&id).is_some() {
            debug!(member_id = id, "Member removed from cluster");
            Ok(())
        } else {
            Err(ClusterError::MemberNotFound(id))
        }
    }

    /// Updates a member's peer URLs.
    pub fn update_member(&self, id: u64, peer_urls: Vec<String>) -> ClusterResult<Member> {
        let mut members = self.members.write();

        if let Some(member) = members.get_mut(&id) {
            member.peer_urls = peer_urls;

            // Validate the updated member
            member.validate()?;

            debug!(member_id = id, "Member peer URLs updated");
            Ok(member.clone())
        } else {
            Err(ClusterError::MemberNotFound(id))
        }
    }

    /// Promotes a learner member to a voting member.
    pub fn promote_member(&self, id: u64) -> ClusterResult<Member> {
        let mut members = self.members.write();

        if let Some(member) = members.get_mut(&id) {
            if !member.is_learner {
                return Err(ClusterError::InvalidConfig(format!(
                    "Member {} is not a learner",
                    id
                )));
            }

            member.is_learner = false;

            debug!(member_id = id, "Member promoted to voter");
            Ok(member.clone())
        } else {
            Err(ClusterError::MemberNotFound(id))
        }
    }

    /// Demotes a voting member to a learner.
    pub fn demote_member(&self, id: u64) -> ClusterResult<Member> {
        if id == self.leader_id() {
            return Err(ClusterError::CannotRemoveLeader);
        }

        let mut members = self.members.write();

        if let Some(member) = members.get_mut(&id) {
            if member.is_learner {
                return Err(ClusterError::InvalidConfig(format!(
                    "Member {} is already a learner",
                    id
                )));
            }

            member.is_learner = true;

            debug!(member_id = id, "Member demoted to learner");
            Ok(member.clone())
        } else {
            Err(ClusterError::MemberNotFound(id))
        }
    }

    /// Lists all members in the cluster.
    pub fn list_members(&self) -> Vec<Member> {
        self.members.read().values().cloned().collect()
    }

    /// Gets information about a specific member.
    pub fn get_member(&self, id: u64) -> Option<Member> {
        self.members.read().get(&id).cloned()
    }

    /// Gets all voting members.
    pub fn get_voters(&self) -> Vec<Member> {
        self.members
            .read()
            .values()
            .filter(|m| !m.is_learner)
            .cloned()
            .collect()
    }

    /// Gets all learner members.
    pub fn get_learners(&self) -> Vec<Member> {
        self.members
            .read()
            .values()
            .filter(|m| m.is_learner)
            .cloned()
            .collect()
    }

    /// Returns the number of members in the cluster.
    pub fn member_count(&self) -> usize {
        self.members.read().len()
    }

    /// Returns the number of voting members.
    pub fn voter_count(&self) -> usize {
        self.members
            .read()
            .values()
            .filter(|m| !m.is_learner)
            .count()
    }

    /// Calculates the quorum size for this cluster.
    pub fn quorum_size(&self) -> usize {
        (self.voter_count() / 2) + 1
    }

    /// Generates a random member ID.
    fn generate_member_id(&self) -> u64 {
        // Use random UUID and convert to u64 for member ID
        let uuid = Uuid::new_v4();
        let bytes = uuid.as_bytes();
        u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    }

    /// Recovers cluster membership from a snapshot.
    pub fn recover(&self, members: Vec<Member>) -> ClusterResult<()> {
        let mut members_map = self.members.write();

        for member in members {
            member.validate()?;
            members_map.insert(member.id, member);
        }

        debug!("Cluster membership recovered");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_manager() {
        let manager = ClusterManager::new(1, 1).unwrap();
        assert_eq!(manager.cluster_id(), 1);
        assert_eq!(manager.member_id(), 1);
    }

    #[test]
    fn test_add_member() {
        let manager = ClusterManager::new(1, 1).unwrap();

        let member = manager
            .add_member(
                "member2".to_string(),
                vec!["http://localhost:2380".to_string()],
                vec!["http://localhost:2379".to_string()],
                false,
            )
            .unwrap();

        assert_eq!(member.name, "member2");
        assert!(!member.is_learner);
        assert_eq!(manager.member_count(), 1);
    }

    #[test]
    fn test_add_learner() {
        let manager = ClusterManager::new(1, 1).unwrap();

        let member = manager
            .add_member(
                "learner".to_string(),
                vec!["http://localhost:2380".to_string()],
                vec!["http://localhost:2379".to_string()],
                true,
            )
            .unwrap();

        assert!(member.is_learner);
        assert_eq!(manager.get_learners().len(), 1);
    }

    #[test]
    fn test_promote_learner() {
        let manager = ClusterManager::new(1, 1).unwrap();

        let member = manager
            .add_member(
                "learner".to_string(),
                vec!["http://localhost:2380".to_string()],
                vec!["http://localhost:2379".to_string()],
                true,
            )
            .unwrap();

        let promoted = manager.promote_member(member.id).unwrap();
        assert!(!promoted.is_learner);
    }

    #[test]
    fn test_quorum() {
        let manager = ClusterManager::new(1, 1).unwrap();

        // Initial: 1 member
        assert_eq!(manager.voter_count(), 0);

        // Add 2 more voting members
        let _m2 = manager
            .add_member(
                "m2".to_string(),
                vec!["http://localhost:2380".to_string()],
                vec![],
                false,
            )
            .unwrap();

        let _m3 = manager
            .add_member(
                "m3".to_string(),
                vec!["http://localhost:2381".to_string()],
                vec![],
                false,
            )
            .unwrap();

        assert_eq!(manager.voter_count(), 2);
        assert_eq!(manager.quorum_size(), 2);
    }

    #[test]
    fn test_leader_protection() {
        let manager = ClusterManager::new(1, 1).unwrap();

        manager.set_leader(1);
        assert_eq!(manager.leader_id(), 1);

        // Try to remove leader (should fail if we add them first)
        let m2 = manager
            .add_member(
                "m2".to_string(),
                vec!["http://localhost:2380".to_string()],
                vec![],
                false,
            )
            .unwrap();

        // Cannot remove leader even after adding more members
        manager.set_leader(m2.id);
        let result = manager.remove_member(m2.id);
        assert!(result.is_err());
    }

    #[test]
    fn test_member_validation() {
        let result = ClusterManager::new(0, 1);
        assert!(result.is_err());

        let result = ClusterManager::new(1, 0);
        assert!(result.is_err());

        let manager = ClusterManager::new(1, 1).unwrap();

        let result = manager.add_member(
            "".to_string(),
            vec!["http://localhost:2380".to_string()],
            vec![],
            false,
        );
        assert!(result.is_err());

        let result = manager.add_member("test".to_string(), vec![], vec![], false);
        assert!(result.is_err());

        let result = manager.add_member(
            "test".to_string(),
            vec!["invalid-url".to_string()],
            vec![],
            false,
        );
        assert!(result.is_err());
    }
}
