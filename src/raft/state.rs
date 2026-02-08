use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::{HashMap, HashSet};
use parking_lot::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftRole {
    Follower,
    Candidate,
    PreCandidate,
    Leader,
}

impl std::fmt::Display for RaftRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RaftRole::Follower => write!(f, "Follower"),
            RaftRole::Candidate => write!(f, "Candidate"),
            RaftRole::PreCandidate => write!(f, "PreCandidate"),
            RaftRole::Leader => write!(f, "Leader"),
        }
    }
}

#[derive(Debug)]
pub struct RaftState {
    // Persistent state (should be written to disk)
    pub current_term: AtomicU64,
    pub voted_for: RwLock<Option<u64>>,

    // Volatile state
    pub role: RwLock<RaftRole>,
    pub leader_id: RwLock<Option<u64>>,
    pub commit_index: AtomicU64,
    pub last_applied: AtomicU64,

    // Leader volatile state
    pub next_index: RwLock<HashMap<u64, u64>>,
    pub match_index: RwLock<HashMap<u64, u64>>,

    // Learner tracking
    pub learners: RwLock<HashSet<u64>>,
}

impl RaftState {
    pub fn new() -> Self {
        Self {
            current_term: AtomicU64::new(0),
            voted_for: RwLock::new(None),
            role: RwLock::new(RaftRole::Follower),
            leader_id: RwLock::new(None),
            commit_index: AtomicU64::new(0),
            last_applied: AtomicU64::new(0),
            next_index: RwLock::new(HashMap::new()),
            match_index: RwLock::new(HashMap::new()),
            learners: RwLock::new(HashSet::new()),
        }
    }

    pub fn become_follower(&self, term: u64) {
        self.current_term.store(term, Ordering::Release);
        *self.role.write() = RaftRole::Follower;
        *self.voted_for.write() = None;
        *self.leader_id.write() = None;
    }

    pub fn become_candidate(&self) {
        let new_term = self.current_term.load(Ordering::Acquire) + 1;
        self.current_term.store(new_term, Ordering::Release);
        *self.role.write() = RaftRole::Candidate;
        *self.voted_for.write() = None; // Will be set after voting for self
        *self.leader_id.write() = None;
    }

    pub fn become_pre_candidate(&self) {
        *self.role.write() = RaftRole::PreCandidate;
        *self.leader_id.write() = None;
    }

    pub fn become_leader(&self, peer_ids: &[u64]) {
        self.current_term.fetch_add(1, Ordering::AcqRel);
        *self.role.write() = RaftRole::Leader;
        *self.voted_for.write() = None;
        *self.leader_id.write() = None; // Leader doesn't set its own leader_id

        // Initialize next_index and match_index for all peers
        let mut next_idx = HashMap::new();
        let mut match_idx = HashMap::new();
        for &peer_id in peer_ids {
            next_idx.insert(peer_id, 0);
            match_idx.insert(peer_id, 0);
        }
        *self.next_index.write() = next_idx;
        *self.match_index.write() = match_idx;
    }

    pub fn is_leader(&self) -> bool {
        *self.role.read() == RaftRole::Leader
    }

    pub fn is_candidate(&self) -> bool {
        let role = *self.role.read();
        role == RaftRole::Candidate || role == RaftRole::PreCandidate
    }

    pub fn is_follower(&self) -> bool {
        *self.role.read() == RaftRole::Follower
    }

    pub fn role(&self) -> RaftRole {
        *self.role.read()
    }

    pub fn term(&self) -> u64 {
        self.current_term.load(Ordering::Acquire)
    }

    pub fn set_term(&self, term: u64) {
        self.current_term.store(term, Ordering::Release);
    }

    pub fn voted_for(&self) -> Option<u64> {
        *self.voted_for.read()
    }

    pub fn vote_for(&self, candidate_id: u64) {
        *self.voted_for.write() = Some(candidate_id);
    }

    pub fn leader_id(&self) -> Option<u64> {
        *self.leader_id.read()
    }

    pub fn set_leader_id(&self, leader_id: u64) {
        *self.leader_id.write() = Some(leader_id);
    }

    pub fn clear_leader_id(&self) {
        *self.leader_id.write() = None;
    }

    pub fn commit_index(&self) -> u64 {
        self.commit_index.load(Ordering::Acquire)
    }

    pub fn set_commit_index(&self, index: u64) {
        self.commit_index.store(index, Ordering::Release);
    }

    pub fn last_applied(&self) -> u64 {
        self.last_applied.load(Ordering::Acquire)
    }

    pub fn set_last_applied(&self, index: u64) {
        self.last_applied.store(index, Ordering::Release);
    }

    pub fn next_index(&self, peer_id: u64) -> Option<u64> {
        self.next_index.read().get(&peer_id).copied()
    }

    pub fn set_next_index(&self, peer_id: u64, index: u64) {
        self.next_index.write().insert(peer_id, index);
    }

    pub fn decrement_next_index(&self, peer_id: u64) -> Option<u64> {
        let mut next_idx = self.next_index.write();
        if let Some(idx) = next_idx.get_mut(&peer_id) {
            if *idx > 0 {
                *idx -= 1;
                return Some(*idx);
            }
        }
        None
    }

    pub fn match_index(&self, peer_id: u64) -> Option<u64> {
        self.match_index.read().get(&peer_id).copied()
    }

    pub fn set_match_index(&self, peer_id: u64, index: u64) {
        self.match_index.write().insert(peer_id, index);
    }

    pub fn add_learner(&self, peer_id: u64) {
        self.learners.write().insert(peer_id);
    }

    pub fn remove_learner(&self, peer_id: u64) {
        self.learners.write().remove(&peer_id);
    }

    pub fn is_learner(&self, peer_id: u64) -> bool {
        self.learners.read().contains(&peer_id)
    }

    pub fn learners(&self) -> HashSet<u64> {
        self.learners.read().clone()
    }

    pub fn get_majority_match_index(&self, all_peer_ids: &[u64]) -> u64 {
        let mut indices: Vec<u64> = all_peer_ids
            .iter()
            .filter_map(|&id| self.match_index(id))
            .collect();
        indices.sort_by(|a, b| b.cmp(a)); // Sort descending

        if indices.is_empty() {
            return 0;
        }

        let majority_idx = (indices.len() + 1) / 2;
        if majority_idx <= indices.len() {
            indices[majority_idx - 1]
        } else {
            0
        }
    }
}

impl Default for RaftState {
    fn default() -> Self {
        Self::new()
    }
}
