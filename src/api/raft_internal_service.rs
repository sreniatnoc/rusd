use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::debug;

use crate::raft::node::RaftNode;
use crate::raft::transport as raft_transport;
use crate::raftpb::raft_internal_server::RaftInternal;
use crate::raftpb::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotChunk,
    InstallSnapshotResponse, RequestVoteRequest, RequestVoteResponse,
};

/// gRPC service that handles incoming Raft peer RPCs.
/// This is the server-side counterpart to GrpcTransport (the client).
pub struct RaftInternalService {
    raft: Arc<RaftNode>,
}

impl RaftInternalService {
    pub fn new(raft: Arc<RaftNode>) -> Self {
        Self { raft }
    }
}

#[tonic::async_trait]
impl RaftInternal for RaftInternalService {
    async fn append_entries(
        &self,
        request: Request<AppendEntriesRequest>,
    ) -> Result<Response<AppendEntriesResponse>, Status> {
        let req = request.into_inner();

        // Convert proto entries to internal format
        let entries: Vec<crate::raft::LogEntry> = req
            .entries
            .iter()
            .map(|e| crate::raft::LogEntry {
                index: e.index,
                term: e.term,
                data: e.data.clone(),
                entry_type: match e.entry_type {
                    x if x == crate::raftpb::EntryType::ConfigChange as i32 => {
                        crate::raft::EntryType::ConfigChange
                    }
                    x if x == crate::raftpb::EntryType::Snapshot as i32 => {
                        crate::raft::EntryType::Snapshot
                    }
                    _ => crate::raft::EntryType::Normal,
                },
            })
            .collect();

        let internal_req = raft_transport::AppendEntriesRequest {
            term: req.term,
            leader_id: req.leader_id,
            prev_log_index: req.prev_log_index,
            prev_log_term: req.prev_log_term,
            entries,
            leader_commit: req.leader_commit,
        };

        let response = self.raft.handle_append_entries(internal_req).await;

        Ok(Response::new(AppendEntriesResponse {
            term: response.term,
            success: response.success,
            match_index: response.match_index,
            conflict_index: response.conflict_index,
            conflict_term: response.conflict_term,
        }))
    }

    async fn request_vote(
        &self,
        request: Request<RequestVoteRequest>,
    ) -> Result<Response<RequestVoteResponse>, Status> {
        let req = request.into_inner();

        let internal_req = raft_transport::RequestVoteRequest {
            term: req.term,
            candidate_id: req.candidate_id,
            last_log_index: req.last_log_index,
            last_log_term: req.last_log_term,
            pre_vote: req.pre_vote,
        };

        let response = self.raft.handle_request_vote(internal_req).await;

        Ok(Response::new(RequestVoteResponse {
            term: response.term,
            vote_granted: response.vote_granted,
        }))
    }

    async fn install_snapshot(
        &self,
        request: Request<tonic::Streaming<InstallSnapshotChunk>>,
    ) -> Result<Response<InstallSnapshotResponse>, Status> {
        let mut stream = request.into_inner();

        // Collect all chunks
        let mut last_chunk = None;
        let mut all_data = Vec::new();

        while let Some(chunk) = stream.message().await.map_err(|e| {
            Status::internal(format!("Failed to receive snapshot chunk: {}", e))
        })? {
            all_data.extend_from_slice(&chunk.data);
            last_chunk = Some(chunk);
        }

        let chunk = last_chunk
            .ok_or_else(|| Status::invalid_argument("Empty snapshot stream"))?;

        let internal_req = raft_transport::InstallSnapshotRequest {
            term: chunk.term,
            leader_id: chunk.leader_id,
            last_included_index: chunk.last_included_index,
            last_included_term: chunk.last_included_term,
            offset: 0,
            data: all_data,
            done: true,
        };

        let response = self.raft.handle_install_snapshot(internal_req).await;

        Ok(Response::new(InstallSnapshotResponse {
            term: response.term,
        }))
    }
}
