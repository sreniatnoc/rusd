use crate::raft::config::RaftConfig;
use crate::raft::log::RaftLog;
use crate::raft::state::{RaftRole, RaftState};
use crate::raft::transport::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    RaftTransport, RequestVoteRequest, RequestVoteResponse,
};
use crate::raft::{EntryType, LogEntry, RaftError, RaftMessage, Result as RaftResult};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex; // std::sync::Mutex guards are Send (unlike parking_lot)
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

/// Notifies waiters when the Raft commit index advances past their requested index.
/// Used by the KV service to wait for proposals to be committed before applying.
pub struct CommitNotifier {
    waiters: Mutex<BTreeMap<u64, Vec<oneshot::Sender<()>>>>,
}

impl Default for CommitNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitNotifier {
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(BTreeMap::new()),
        }
    }

    /// Register a waiter for the given log index. Returns a receiver that
    /// completes when commit_index >= index.
    pub fn register(&self, index: u64) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap()
            .entry(index)
            .or_default()
            .push(tx);
        rx
    }

    /// Notify all waiters with index <= committed_index.
    pub fn notify_up_to(&self, committed_index: u64) {
        let mut waiters = self.waiters.lock().unwrap();
        let keys: Vec<u64> = waiters.range(..=committed_index).map(|(&k, _)| k).collect();
        for key in keys {
            if let Some(senders) = waiters.remove(&key) {
                for sender in senders {
                    let _ = sender.send(());
                }
            }
        }
    }
}

/// Callback to create a snapshot of the store.
pub type SnapshotCreateFn = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;
/// Callback to restore the store from snapshot data.
pub type SnapshotRestoreFn = Arc<dyn Fn(&[u8]) -> Result<(), String> + Send + Sync>;

pub struct RaftNode {
    config: Arc<RaftConfig>,
    state: Arc<RaftState>,
    log: Arc<RaftLog>,
    transport: Arc<dyn RaftTransport>,
    apply_tx: tokio::sync::mpsc::Sender<LogEntry>,
    commit_notifier: Arc<CommitNotifier>,

    // Timers - use std::sync::Mutex (Send-safe guards, unlike parking_lot)
    election_timer: Mutex<Option<Instant>>,
    heartbeat_timer: Mutex<Option<Instant>>,

    // Message queue for incoming RPC responses
    #[allow(dead_code)]
    message_rx: Arc<Mutex<tokio::sync::mpsc::Receiver<(u64, RaftMessage)>>>,
    #[allow(dead_code)]
    message_tx: Arc<tokio::sync::mpsc::Sender<(u64, RaftMessage)>>,

    // Snapshot callbacks for creating and restoring store snapshots
    create_snapshot_fn: Option<SnapshotCreateFn>,
    restore_snapshot_fn: Option<SnapshotRestoreFn>,
}

impl RaftNode {
    pub fn new(
        config: RaftConfig,
        log: Arc<RaftLog>,
        transport: Arc<dyn RaftTransport>,
        apply_tx: tokio::sync::mpsc::Sender<LogEntry>,
    ) -> RaftResult<Self> {
        Self::with_snapshot_callbacks(config, log, transport, apply_tx, None, None)
    }

    pub fn with_snapshot_callbacks(
        config: RaftConfig,
        log: Arc<RaftLog>,
        transport: Arc<dyn RaftTransport>,
        apply_tx: tokio::sync::mpsc::Sender<LogEntry>,
        create_snapshot_fn: Option<SnapshotCreateFn>,
        restore_snapshot_fn: Option<SnapshotRestoreFn>,
    ) -> RaftResult<Self> {
        config.validate()?;

        let (message_tx, message_rx) = tokio::sync::mpsc::channel(1000);

        let config = Arc::new(config);
        let state = Arc::new(RaftState::new());
        let commit_notifier = Arc::new(CommitNotifier::new());

        // Single-node cluster: auto-elect as leader immediately
        if config.peers.is_empty() {
            state.set_term(1); // Must be non-zero like etcd (election increments term)
            state.become_leader(config.id, &[], log.last_index());
        }

        Ok(Self {
            election_timer: Mutex::new(Some(
                Instant::now() + Self::random_election_timeout_static(&config),
            )),
            heartbeat_timer: Mutex::new(None),
            config,
            state,
            log,
            transport,
            apply_tx,
            commit_notifier,
            message_tx: Arc::new(message_tx),
            message_rx: Arc::new(Mutex::new(message_rx)),
            create_snapshot_fn,
            restore_snapshot_fn,
        })
    }

    fn random_election_timeout(&self) -> Duration {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        );
        let hash = hasher.finish();

        let range = self.config.election_timeout_max.as_millis()
            - self.config.election_timeout_min.as_millis();
        let offset_ms = (hash as u128 % range) as u64;
        self.config.election_timeout_min + Duration::from_millis(offset_ms)
    }

    fn random_election_timeout_static(config: &RaftConfig) -> Duration {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        );
        let hash = hasher.finish();

        let range =
            config.election_timeout_max.as_millis() - config.election_timeout_min.as_millis();
        let offset_ms = (hash as u128 % range) as u64;
        config.election_timeout_min + Duration::from_millis(offset_ms)
    }

    pub async fn tick(&self) -> RaftResult<()> {
        let now = Instant::now();

        // Check election timeout (extract value before any .await)
        let election_expired = {
            self.election_timer
                .lock()
                .unwrap()
                .map_or(false, |t| now > t)
        };
        if election_expired {
            self.start_election().await?;
            {
                *self.election_timer.lock().unwrap() =
                    Some(now + Self::random_election_timeout_static(&self.config));
            }
        }

        // Check heartbeat timeout (only for leader)
        if self.state.is_leader() {
            let heartbeat_state = {
                let timer = *self.heartbeat_timer.lock().unwrap();
                match timer {
                    Some(t) if now > t => 1, // expired
                    Some(_) => 0,            // not expired
                    None => 2,               // not set
                }
            };
            match heartbeat_state {
                1 => {
                    self.send_heartbeats().await?;
                    {
                        *self.heartbeat_timer.lock().unwrap() =
                            Some(now + self.config.heartbeat_interval);
                    }
                }
                2 => {
                    *self.heartbeat_timer.lock().unwrap() =
                        Some(now + self.config.heartbeat_interval);
                }
                _ => {} // not expired yet
            }
        }

        // Apply committed entries
        self.apply_committed_entries().await?;

        // Check if snapshot is needed
        let log_size = self.log.last_index().saturating_sub(self.log.first_index());
        if log_size > self.config.snapshot_threshold {
            self.trigger_snapshot().await?;
        }

        Ok(())
    }

    async fn start_election(&self) -> RaftResult<()> {
        let current_role = self.state.role();

        match current_role {
            RaftRole::Follower | RaftRole::Candidate => {
                if self.config.pre_vote {
                    self.start_pre_vote().await
                } else {
                    self.start_real_election().await
                }
            }
            RaftRole::PreCandidate => self.start_real_election().await,
            RaftRole::Leader => Ok(()),
        }
    }

    async fn start_pre_vote(&self) -> RaftResult<()> {
        let term = self.state.term();
        let last_index = self.log.last_index();
        let last_term = self.log.last_term();

        self.state.become_pre_candidate();

        let peers: Vec<u64> = self
            .config
            .peers
            .iter()
            .map(|p| p.id)
            .filter(|id| *id != self.config.id)
            .collect();

        if peers.is_empty() {
            return self.start_real_election().await;
        }

        let request = RequestVoteRequest {
            term: term + 1, // Pre-vote uses next term
            candidate_id: self.config.id,
            last_log_index: last_index,
            last_log_term: last_term,
            pre_vote: true,
        };

        let total_nodes = peers.len() + 1;
        let majority = total_nodes / 2 + 1;
        let (vote_tx, mut vote_rx) = tokio::sync::mpsc::channel::<bool>(peers.len());

        for peer_id in peers {
            let request_clone = request.clone();
            let transport_clone = self.transport.clone();
            let vote_tx_clone = vote_tx.clone();

            tokio::spawn(async move {
                match transport_clone
                    .send_request_vote(peer_id, request_clone)
                    .await
                {
                    Ok(response) => {
                        let _ = vote_tx_clone.send(response.vote_granted).await;
                    }
                    Err(_) => {
                        let _ = vote_tx_clone.send(false).await;
                    }
                }
            });
        }
        drop(vote_tx);

        let mut vote_count: usize = 1; // Self vote
        let election_timeout = self.random_election_timeout();
        let deadline = tokio::time::Instant::now() + election_timeout;

        loop {
            tokio::select! {
                result = vote_rx.recv() => {
                    match result {
                        Some(granted) => {
                            if granted { vote_count += 1; }
                            if vote_count >= majority {
                                debug!("Pre-vote passed for term {} ({}/{})", term + 1, vote_count, total_nodes);
                                return self.start_real_election().await;
                            }
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }

        debug!(
            "Pre-vote failed for term {} ({}/{})",
            term + 1,
            vote_count,
            total_nodes
        );
        Ok(())
    }

    async fn start_real_election(&self) -> RaftResult<()> {
        let new_term = self.state.term() + 1;
        self.state.set_term(new_term);
        self.state.become_candidate();
        self.state.vote_for(self.config.id);

        let last_index = self.log.last_index();
        let last_term = self.log.last_term();

        let peers: Vec<u64> = self
            .config
            .peers
            .iter()
            .map(|p| p.id)
            .filter(|id| *id != self.config.id)
            .collect();

        if peers.is_empty() {
            // Single-node cluster: auto-elect
            self.become_leader().await?;
            return Ok(());
        }

        let request = RequestVoteRequest {
            term: new_term,
            candidate_id: self.config.id,
            last_log_index: last_index,
            last_log_term: last_term,
            pre_vote: false,
        };

        // Collect vote responses with a timeout
        let (vote_tx, mut vote_rx) = tokio::sync::mpsc::channel::<bool>(peers.len());
        let total_nodes = peers.len() + 1; // peers + self
        let majority = total_nodes / 2 + 1;

        for peer_id in peers.iter().copied() {
            let request_clone = request.clone();
            let transport_clone = self.transport.clone();
            let vote_tx_clone = vote_tx.clone();
            let state_clone = self.state.clone();

            tokio::spawn(async move {
                match transport_clone
                    .send_request_vote(peer_id, request_clone)
                    .await
                {
                    Ok(response) => {
                        if response.term > new_term {
                            // Higher term discovered, step down
                            state_clone.become_follower(response.term);
                        }
                        let _ = vote_tx_clone.send(response.vote_granted).await;
                    }
                    Err(_) => {
                        let _ = vote_tx_clone.send(false).await;
                    }
                }
            });
        }
        drop(vote_tx); // Drop sender so rx completes when all spawned tasks finish

        // Count votes (self-vote = 1)
        let mut vote_count: usize = 1;
        let election_timeout = self.random_election_timeout();

        let deadline = tokio::time::Instant::now() + election_timeout;
        loop {
            tokio::select! {
                result = vote_rx.recv() => {
                    match result {
                        Some(granted) => {
                            if granted {
                                vote_count += 1;
                            }
                            if vote_count >= majority {
                                info!("Won election for term {} ({}/{} votes)", new_term, vote_count, total_nodes);
                                self.become_leader().await?;
                                return Ok(());
                            }
                        }
                        None => break, // All responses received
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    debug!("Election timeout for term {}, got {}/{} votes", new_term, vote_count, total_nodes);
                    break;
                }
            }
        }

        // Didn't get majority, remain candidate
        if !self.state.is_leader() {
            debug!(
                "Election for term {} inconclusive ({}/{} votes)",
                new_term, vote_count, total_nodes
            );
        }

        Ok(())
    }

    async fn become_leader(&self) -> RaftResult<()> {
        self.state.become_leader(
            self.config.id,
            &self.config.peers.iter().map(|p| p.id).collect::<Vec<_>>(),
            self.log.last_index(),
        );

        // Reset heartbeat timer (guard drops before .await)
        {
            *self.heartbeat_timer.lock().unwrap() =
                Some(Instant::now() + self.config.heartbeat_interval);
        }

        // Send initial heartbeats
        self.send_heartbeats().await?;

        Ok(())
    }

    async fn send_heartbeats(&self) -> RaftResult<()> {
        if !self.state.is_leader() {
            return Ok(());
        }

        let peers: Vec<u64> = self
            .config
            .peers
            .iter()
            .map(|p| p.id)
            .filter(|id| *id != self.config.id)
            .collect();

        let leader_commit = self.state.commit_index();
        // Capture all peer IDs and leader's log index for majority calculation
        let all_peer_ids: Vec<u64> = peers.clone();
        let leader_last_index = self.log.last_index();

        for peer_id in peers {
            let next_idx = self.state.next_index(peer_id).unwrap_or(1);

            // If the follower is behind our first log index, send a snapshot instead
            if next_idx < self.log.first_index() {
                if let Some(snapshot_data) = self.log.get_snapshot_data() {
                    let snapshot_index = self.log.snapshot_index();
                    let snapshot_term = self.log.snapshot_term();
                    let transport_clone = self.transport.clone();
                    let state_clone = self.state.clone();
                    let term = self.state.term();
                    let leader_id = self.config.id;

                    let req = crate::raft::transport::InstallSnapshotRequest {
                        term,
                        leader_id,
                        last_included_index: snapshot_index,
                        last_included_term: snapshot_term,
                        offset: 0,
                        data: snapshot_data,
                        done: true,
                    };

                    tokio::spawn(async move {
                        match transport_clone.send_install_snapshot(peer_id, req).await {
                            Ok(_resp) => {
                                state_clone.set_match_index(peer_id, snapshot_index);
                                state_clone.set_next_index(peer_id, snapshot_index + 1);
                                info!(
                                    peer_id = peer_id,
                                    snapshot_index = snapshot_index,
                                    "Sent snapshot to follower"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    peer_id = peer_id,
                                    error = %e,
                                    "Failed to send snapshot to follower"
                                );
                            }
                        }
                    });
                    continue;
                }
            }

            // Determine previous log index and term
            let prev_log_index = if next_idx > 0 { next_idx - 1 } else { 0 };
            let prev_log_term = self.log.term_at(prev_log_index).ok().flatten().unwrap_or(0);

            // Get entries to send
            let entries: Vec<LogEntry> = self
                .log
                .get_range(
                    next_idx,
                    next_idx + self.config.max_log_entries_per_request as u64,
                )
                .ok()
                .unwrap_or_default();

            let request = AppendEntriesRequest {
                term: self.state.term(),
                leader_id: self.config.id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
            };

            let transport_clone = self.transport.clone();
            let state_clone = self.state.clone();
            let all_peer_ids_clone = all_peer_ids.clone();
            let commit_notifier_clone = self.commit_notifier.clone();

            tokio::spawn(async move {
                match transport_clone.send_append_entries(peer_id, request).await {
                    Ok(response) => {
                        if response.success {
                            state_clone.set_match_index(peer_id, response.match_index);
                            state_clone.set_next_index(peer_id, response.match_index + 1);
                        } else if response.conflict_index > 0 {
                            // Handle log conflict with accelerated conflict resolution
                            state_clone.set_next_index(peer_id, response.conflict_index);
                        } else {
                            // Fallback: decrement next_index
                            let _ = state_clone.decrement_next_index(peer_id);
                        }

                        // Advance commit_index using ALL peers + leader's own index
                        let majority_match_idx = state_clone
                            .get_majority_match_index(&all_peer_ids_clone, leader_last_index);
                        if majority_match_idx > state_clone.commit_index() {
                            state_clone.set_commit_index(majority_match_idx);
                            commit_notifier_clone.notify_up_to(majority_match_idx);
                        }
                    }
                    Err(_e) => {
                        // Log error, but don't crash
                    }
                }
            });
        }

        Ok(())
    }

    pub async fn handle_append_entries(&self, req: AppendEntriesRequest) -> AppendEntriesResponse {
        let current_term = self.state.term();

        // Reply false if term < currentTerm
        if req.term < current_term {
            return AppendEntriesResponse {
                term: current_term,
                success: false,
                match_index: 0,
                conflict_index: 0,
                conflict_term: 0,
            };
        }

        // If term > currentTerm, become follower
        if req.term > current_term {
            self.state.become_follower(req.term);
            *self.election_timer.lock().unwrap() =
                Some(Instant::now() + Self::random_election_timeout_static(&self.config));
        }

        // Set leader_id
        self.state.set_leader_id(req.leader_id);
        *self.election_timer.lock().unwrap() =
            Some(Instant::now() + Self::random_election_timeout_static(&self.config));

        // Check if we have the prev_log_entry
        if !self.log.has_entry(req.prev_log_index, req.prev_log_term) {
            let conflict_term = self
                .log
                .term_at(req.prev_log_index)
                .ok()
                .flatten()
                .unwrap_or(0);

            // Find first index with conflict_term
            let mut conflict_index = req.prev_log_index;
            while conflict_index > self.log.first_index()
                && self
                    .log
                    .term_at(conflict_index - 1)
                    .ok()
                    .flatten()
                    .unwrap_or(0)
                    == conflict_term
            {
                conflict_index -= 1;
            }

            return AppendEntriesResponse {
                term: current_term,
                success: false,
                match_index: 0,
                conflict_index,
                conflict_term,
            };
        }

        // Truncate log if needed
        if !req.entries.is_empty() {
            let first_new_index = req.entries[0].index;
            if first_new_index <= self.log.last_index() {
                let _ = self.log.truncate_after(first_new_index - 1);
            }

            // Append new entries
            if let Err(_e) = self.log.append(&req.entries) {
                return AppendEntriesResponse {
                    term: current_term,
                    success: false,
                    match_index: 0,
                    conflict_index: 0,
                    conflict_term: 0,
                };
            }
        }

        // Update commit index
        if req.leader_commit > self.state.commit_index() {
            let new_commit_index = std::cmp::min(req.leader_commit, self.log.last_index());
            self.state.set_commit_index(new_commit_index);
        }

        let match_index = req.prev_log_index + req.entries.len() as u64;

        AppendEntriesResponse {
            term: current_term,
            success: true,
            match_index,
            conflict_index: 0,
            conflict_term: 0,
        }
    }

    pub async fn handle_request_vote(&self, req: RequestVoteRequest) -> RequestVoteResponse {
        let current_term = self.state.term();

        // Reply false if term < currentTerm
        if req.term < current_term {
            return RequestVoteResponse {
                term: current_term,
                vote_granted: false,
            };
        }

        // If term > currentTerm, become follower
        if req.term > current_term {
            self.state.become_follower(req.term);
        }

        // Check if we've already voted
        let vote_for = self.state.voted_for();
        let can_vote = vote_for.is_none() || vote_for == Some(req.candidate_id);

        // Check if candidate's log is at least as up-to-date
        let last_index = self.log.last_index();
        let last_term = self.log.last_term();

        let log_ok = (req.last_log_term > last_term)
            || (req.last_log_term == last_term && req.last_log_index >= last_index);

        let vote_granted = can_vote && log_ok;

        if vote_granted && !req.pre_vote {
            self.state.vote_for(req.candidate_id);
            *self.election_timer.lock().unwrap() =
                Some(Instant::now() + Self::random_election_timeout_static(&self.config));
        }

        RequestVoteResponse {
            term: current_term,
            vote_granted,
        }
    }

    pub async fn handle_install_snapshot(
        &self,
        req: InstallSnapshotRequest,
    ) -> InstallSnapshotResponse {
        let current_term = self.state.term();

        if req.term < current_term {
            return InstallSnapshotResponse { term: current_term };
        }

        if req.term > current_term {
            self.state.become_follower(req.term);
        }

        self.state.set_leader_id(req.leader_id);
        *self.election_timer.lock().unwrap() =
            Some(Instant::now() + Self::random_election_timeout_static(&self.config));

        if req.done {
            // Restore the store from snapshot data
            if let Some(ref restore_fn) = self.restore_snapshot_fn {
                match restore_fn(&req.data) {
                    Ok(()) => {
                        info!(
                            "Restored snapshot at index={} term={}",
                            req.last_included_index, req.last_included_term
                        );
                    }
                    Err(e) => {
                        warn!("Failed to restore snapshot: {}", e);
                    }
                }
            }

            // Save snapshot metadata and truncate log
            let _ = self.log.save_snapshot_with_data(
                req.last_included_index,
                req.last_included_term,
                req.data,
            );
            let _ = self.log.truncate_before(req.last_included_index + 1);

            // Update state
            self.state.set_commit_index(req.last_included_index);
            self.state.set_last_applied(req.last_included_index);
        }

        InstallSnapshotResponse { term: current_term }
    }

    pub async fn propose(&self, data: Vec<u8>) -> RaftResult<u64> {
        if !self.state.is_leader() {
            return Err(RaftError::NotLeader);
        }

        let index = self.log.last_index() + 1;
        let entry = LogEntry {
            index,
            term: self.state.term(),
            data,
            entry_type: EntryType::Normal,
        };

        self.log.append(&[entry])?;

        // Single-node: immediately commit since there are no peers to replicate to
        if self.config.peers.is_empty() {
            self.state.set_commit_index(index);
        }

        self.send_heartbeats().await?;

        Ok(index)
    }

    async fn apply_committed_entries(&self) -> RaftResult<()> {
        let commit_index = self.state.commit_index();
        let last_applied = self.state.last_applied();

        // Always notify commit waiters (handles races between register and commit)
        self.commit_notifier.notify_up_to(commit_index);

        for index in (last_applied + 1)..=commit_index {
            if let Some(entry) = self.log.get(index)? {
                let _ = self.apply_tx.send(entry).await;
            }
        }

        self.state.set_last_applied(commit_index);
        Ok(())
    }

    async fn trigger_snapshot(&self) -> RaftResult<()> {
        let snapshot_index = self
            .log
            .last_index()
            .saturating_sub(self.config.snapshot_threshold / 2);
        if snapshot_index > self.log.snapshot_index() {
            let snapshot_term = self.log.term_at(snapshot_index).ok().flatten().unwrap_or(0);

            // Create actual snapshot data from the store if callback is available
            if let Some(ref create_fn) = self.create_snapshot_fn {
                let data = create_fn();
                self.log
                    .save_snapshot_with_data(snapshot_index, snapshot_term, data)?;
                info!(
                    "Created snapshot at index={} term={}",
                    snapshot_index, snapshot_term
                );
            } else {
                self.log.save_snapshot(snapshot_index, snapshot_term)?;
            }

            self.log.truncate_before(snapshot_index + 1)?;
        }

        Ok(())
    }

    pub async fn step(&self, _from: u64, msg: RaftMessage) -> RaftResult<()> {
        match msg {
            RaftMessage::AppendEntries(req) => {
                let response = self.handle_append_entries(req).await;
                // In real implementation, send response back to leader
                let _ = response;
            }
            RaftMessage::RequestVote(req) => {
                let response = self.handle_request_vote(req).await;
                // In real implementation, send response back to candidate
                let _ = response;
            }
            RaftMessage::InstallSnapshot(req) => {
                let response = self.handle_install_snapshot(req).await;
                // In real implementation, send response back to leader
                let _ = response;
            }
            _ => {}
        }

        Ok(())
    }

    pub fn run(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            info!("Raft event loop started (node {})", self.config.id);
            let mut interval = tokio::time::interval(Duration::from_millis(10));
            loop {
                interval.tick().await;
                if let Err(e) = self.tick().await {
                    warn!("Raft tick error: {}", e);
                }
            }
        })
    }

    pub fn get_state(&self) -> Arc<RaftState> {
        self.state.clone()
    }

    pub fn get_log(&self) -> Arc<RaftLog> {
        self.log.clone()
    }

    pub fn get_config(&self) -> Arc<RaftConfig> {
        self.config.clone()
    }

    pub fn commit_notifier(&self) -> &CommitNotifier {
        &self.commit_notifier
    }

    pub fn is_leader(&self) -> bool {
        self.state.is_leader()
    }

    /// Check if the cluster has an elected leader (may or may not be this node).
    pub fn has_leader(&self) -> bool {
        self.state.leader_id().is_some()
    }

    /// Get the node ID (used as member_id).
    pub fn member_id(&self) -> u64 {
        self.config.id
    }

    /// Get the cluster ID. Computed from sorted member IDs using SHA-1,
    /// matching etcd v3.5.x's algorithm.
    pub fn cluster_id(&self) -> u64 {
        self.config.cluster_id
    }

    /// Get the current Raft term.
    pub fn current_term(&self) -> u64 {
        self.state.term()
    }

    /// Propose a new log entry. Only succeeds if this node is the leader.
    pub async fn propose_new(&self, data: Vec<u8>) -> RaftResult<()> {
        if !self.is_leader() {
            return Err(RaftError::NotLeader);
        }

        let entry = LogEntry {
            index: self.log.last_index() + 1,
            term: self.state.term(),
            data,
            entry_type: crate::raft::log::EntryType::Normal,
        };

        self.log
            .append(&[entry])
            .map_err(|e| RaftError::LogError(format!("{}", e)))?;
        Ok(())
    }

    /// Propose a configuration change through the Raft log.
    pub async fn propose_conf_change(&self, data: Vec<u8>) -> RaftResult<u64> {
        if !self.is_leader() {
            return Err(RaftError::NotLeader);
        }

        let index = self.log.last_index() + 1;
        let entry = LogEntry {
            index,
            term: self.state.term(),
            data,
            entry_type: EntryType::ConfigChange,
        };

        self.log.append(&[entry])?;

        // Single-node: immediately commit
        if self.config.peers.is_empty() {
            self.state.set_commit_index(index);
        }

        self.send_heartbeats().await?;

        Ok(index)
    }

    /// Generate a lease ID.
    pub fn generate_lease_id(&self) -> i64 {
        // TODO: Implement proper lease ID generation
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64
    }

    /// Generate a member ID.
    pub fn generate_member_id(&self) -> u64 {
        // TODO: Implement proper member ID generation
        self.config.id.wrapping_add(1)
    }
}
