use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use super::{Result as RaftResult, RaftError, LogEntry};
use tokio::sync::RwLock;

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

pub struct GrpcTransport {
    peer_addresses: Arc<RwLock<HashMap<u64, String>>>,
    client_timeout: std::time::Duration,
}

impl GrpcTransport {
    pub fn new(peer_addresses: HashMap<u64, String>) -> Self {
        Self {
            peer_addresses: Arc::new(RwLock::new(peer_addresses)),
            client_timeout: std::time::Duration::from_secs(10),
        }
    }

    pub async fn add_peer(&self, id: u64, address: String) {
        self.peer_addresses.write().await.insert(id, address);
    }

    pub async fn remove_peer(&self, id: u64) {
        self.peer_addresses.write().await.remove(&id);
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
}

#[async_trait]
impl RaftTransport for GrpcTransport {
    async fn send_append_entries(
        &self,
        target: u64,
        req: AppendEntriesRequest,
    ) -> RaftResult<AppendEntriesResponse> {
        let _address = self.get_peer_address(target).await?;

        // In a real implementation, this would establish a gRPC connection and send the request.
        // For now, we return a mock response to allow compilation.
        // In production, use tonic to generate gRPC client stubs from .proto files.

        Ok(AppendEntriesResponse {
            term: req.term,
            success: false,
            match_index: 0,
            conflict_index: 0,
            conflict_term: 0,
        })
    }

    async fn send_request_vote(
        &self,
        target: u64,
        req: RequestVoteRequest,
    ) -> RaftResult<RequestVoteResponse> {
        let _address = self.get_peer_address(target).await?;

        // In a real implementation, this would establish a gRPC connection and send the request.
        // For now, we return a mock response to allow compilation.

        Ok(RequestVoteResponse {
            term: req.term,
            vote_granted: false,
        })
    }

    async fn send_install_snapshot(
        &self,
        target: u64,
        req: InstallSnapshotRequest,
    ) -> RaftResult<InstallSnapshotResponse> {
        let _address = self.get_peer_address(target).await?;

        // In a real implementation, this would establish a gRPC connection and send the request.
        // For now, we return a mock response to allow compilation.

        Ok(InstallSnapshotResponse { term: req.term })
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
