use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use super::{Result as RaftResult, RaftError, LogEntry};
use tokio::sync::RwLock;
use tracing::{debug, warn};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppendEntriesRequest {
    pub term: u64,
    pub leader_id: u64,
    pub prev_log_index: u64,
    pub prev_log_term: u64,
    pub entries: Vec<LogEntry>,
    pub leader_commit: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppendEntriesResponse {
    pub term: u64,
    pub success: bool,
    pub match_index: u64,
    pub conflict_index: u64,
    pub conflict_term: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestVoteRequest {
    pub term: u64,
    pub candidate_id: u64,
    pub last_log_index: u64,
    pub last_log_term: u64,
    pub pre_vote: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestVoteResponse {
    pub term: u64,
    pub vote_granted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallSnapshotRequest {
    pub term: u64,
    pub leader_id: u64,
    pub last_included_index: u64,
    pub last_included_term: u64,
    pub offset: u64,
    pub data: Vec<u8>,
    pub done: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallSnapshotResponse {
    pub term: u64,
}

#[async_trait]
pub trait RaftTransport: Send + Sync {
    async fn send_append_entries(
        &self,
        target: u64,
        req: AppendEntriesRequest,
    ) -> RaftResult<AppendEntriesResponse>;

    async fn send_request_vote(
        &self,
        target: u64,
        req: RequestVoteRequest,
    ) -> RaftResult<RequestVoteResponse>;

    async fn send_install_snapshot(
        &self,
        target: u64,
        req: InstallSnapshotRequest,
    ) -> RaftResult<InstallSnapshotResponse>;
}

/// gRPC-based Raft transport using the raftpb proto definitions.
/// Maintains persistent connections to peer nodes.
pub struct GrpcTransport {
    peer_addresses: Arc<RwLock<HashMap<u64, String>>>,
    client_timeout: std::time::Duration,
    /// Cached gRPC client connections to peers
    clients: Arc<RwLock<HashMap<u64, crate::raftpb::raft_internal_client::RaftInternalClient<tonic::transport::Channel>>>>,
}

impl GrpcTransport {
    pub fn new(peer_addresses: HashMap<u64, String>) -> Self {
        Self {
            peer_addresses: Arc::new(RwLock::new(peer_addresses)),
            client_timeout: std::time::Duration::from_secs(5),
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn add_peer(&self, id: u64, address: String) {
        self.peer_addresses.write().await.insert(id, address);
        // Remove cached client so it reconnects
        self.clients.write().await.remove(&id);
    }

    pub async fn remove_peer(&self, id: u64) {
        self.peer_addresses.write().await.remove(&id);
        self.clients.write().await.remove(&id);
    }

    async fn get_peer_address(&self, peer_id: u64) -> RaftResult<String> {
        self.peer_addresses
            .read()
            .await
            .get(&peer_id)
            .cloned()
            .ok_or_else(|| {
                RaftError::TransportError(format!("Peer {} not found", peer_id))
            })
    }

    /// Get or create a gRPC client for a peer
    async fn get_client(&self, peer_id: u64) -> RaftResult<crate::raftpb::raft_internal_client::RaftInternalClient<tonic::transport::Channel>> {
        // Check cache first
        {
            let clients = self.clients.read().await;
            if let Some(client) = clients.get(&peer_id) {
                return Ok(client.clone());
            }
        }

        // Create new connection
        let address = self.get_peer_address(peer_id).await?;
        let endpoint = tonic::transport::Endpoint::from_shared(address.clone())
            .map_err(|e| RaftError::TransportError(format!("Invalid endpoint {}: {}", address, e)))?
            .timeout(self.client_timeout)
            .connect_timeout(std::time::Duration::from_secs(3));

        let channel = endpoint.connect().await
            .map_err(|e| RaftError::TransportError(format!("Failed to connect to peer {}: {}", peer_id, e)))?;

        let client = crate::raftpb::raft_internal_client::RaftInternalClient::new(channel);

        // Cache the client
        self.clients.write().await.insert(peer_id, client.clone());
        debug!("Connected to peer {} at {}", peer_id, address);

        Ok(client)
    }
}

/// Convert internal LogEntry to proto LogEntry
fn to_proto_entry(entry: &LogEntry) -> crate::raftpb::LogEntry {
    crate::raftpb::LogEntry {
        index: entry.index,
        term: entry.term,
        data: entry.data.clone(),
        entry_type: match entry.entry_type {
            crate::raft::EntryType::Normal => crate::raftpb::EntryType::Normal as i32,
            crate::raft::EntryType::ConfigChange => crate::raftpb::EntryType::ConfigChange as i32,
            crate::raft::EntryType::Snapshot => crate::raftpb::EntryType::Snapshot as i32,
        },
    }
}

/// Convert proto LogEntry to internal LogEntry
fn from_proto_entry(entry: &crate::raftpb::LogEntry) -> LogEntry {
    LogEntry {
        index: entry.index,
        term: entry.term,
        data: entry.data.clone(),
        entry_type: match entry.entry_type {
            x if x == crate::raftpb::EntryType::ConfigChange as i32 => crate::raft::EntryType::ConfigChange,
            x if x == crate::raftpb::EntryType::Snapshot as i32 => crate::raft::EntryType::Snapshot,
            _ => crate::raft::EntryType::Normal,
        },
    }
}

#[async_trait]
impl RaftTransport for GrpcTransport {
    async fn send_append_entries(
        &self,
        target: u64,
        req: AppendEntriesRequest,
    ) -> RaftResult<AppendEntriesResponse> {
        let mut client = self.get_client(target).await?;

        let proto_req = crate::raftpb::AppendEntriesRequest {
            term: req.term,
            leader_id: req.leader_id,
            prev_log_index: req.prev_log_index,
            prev_log_term: req.prev_log_term,
            entries: req.entries.iter().map(to_proto_entry).collect(),
            leader_commit: req.leader_commit,
        };

        let response = client.append_entries(proto_req).await
            .map_err(|e| {
                // Remove cached client on error so it reconnects
                let clients = self.clients.clone();
                tokio::spawn(async move { clients.write().await.remove(&target); });
                RaftError::TransportError(format!("AppendEntries to peer {} failed: {}", target, e))
            })?;

        let resp = response.into_inner();
        Ok(AppendEntriesResponse {
            term: resp.term,
            success: resp.success,
            match_index: resp.match_index,
            conflict_index: resp.conflict_index,
            conflict_term: resp.conflict_term,
        })
    }

    async fn send_request_vote(
        &self,
        target: u64,
        req: RequestVoteRequest,
    ) -> RaftResult<RequestVoteResponse> {
        let mut client = self.get_client(target).await?;

        let proto_req = crate::raftpb::RequestVoteRequest {
            term: req.term,
            candidate_id: req.candidate_id,
            last_log_index: req.last_log_index,
            last_log_term: req.last_log_term,
            pre_vote: req.pre_vote,
        };

        let response = client.request_vote(proto_req).await
            .map_err(|e| {
                let clients = self.clients.clone();
                tokio::spawn(async move { clients.write().await.remove(&target); });
                RaftError::TransportError(format!("RequestVote to peer {} failed: {}", target, e))
            })?;

        let resp = response.into_inner();
        Ok(RequestVoteResponse {
            term: resp.term,
            vote_granted: resp.vote_granted,
        })
    }

    async fn send_install_snapshot(
        &self,
        target: u64,
        req: InstallSnapshotRequest,
    ) -> RaftResult<InstallSnapshotResponse> {
        let mut client = self.get_client(target).await?;

        // Send as a single-chunk stream
        let chunk = crate::raftpb::InstallSnapshotChunk {
            term: req.term,
            leader_id: req.leader_id,
            last_included_index: req.last_included_index,
            last_included_term: req.last_included_term,
            offset: req.offset,
            data: req.data.clone(),
            done: req.done,
        };

        let stream = tokio_stream::once(chunk);
        let response = client.install_snapshot(stream).await
            .map_err(|e| {
                let clients = self.clients.clone();
                tokio::spawn(async move { clients.write().await.remove(&target); });
                RaftError::TransportError(format!("InstallSnapshot to peer {} failed: {}", target, e))
            })?;

        let resp = response.into_inner();
        Ok(InstallSnapshotResponse { term: resp.term })
    }
}

// Mock transport for testing
pub struct MockTransport {
    responses: Arc<RwLock<HashMap<u64, Vec<u8>>>>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self {
            responses: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RaftTransport for MockTransport {
    async fn send_append_entries(
        &self,
        _target: u64,
        req: AppendEntriesRequest,
    ) -> RaftResult<AppendEntriesResponse> {
        Ok(AppendEntriesResponse {
            term: req.term,
            success: true,
            match_index: req.prev_log_index + req.entries.len() as u64,
            conflict_index: 0,
            conflict_term: 0,
        })
    }

    async fn send_request_vote(
        &self,
        _target: u64,
        req: RequestVoteRequest,
    ) -> RaftResult<RequestVoteResponse> {
        Ok(RequestVoteResponse {
            term: req.term,
            vote_granted: true,
        })
    }

    async fn send_install_snapshot(
        &self,
        _target: u64,
        req: InstallSnapshotRequest,
    ) -> RaftResult<InstallSnapshotResponse> {
        Ok(InstallSnapshotResponse { term: req.term })
    }
}
