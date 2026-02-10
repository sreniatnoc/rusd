use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Request, Response, Status};

use crate::etcdserverpb::maintenance_server::Maintenance;
use crate::etcdserverpb::*;
use crate::raft::node::RaftNode;
use crate::storage::mvcc::MvccStore;

pub struct MaintenanceService {
    store: Arc<MvccStore>,
    raft: Arc<RaftNode>,
}

impl MaintenanceService {
    pub fn new(store: Arc<MvccStore>, raft: Arc<RaftNode>) -> Self {
        Self { store, raft }
    }

    fn build_response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: self.raft.cluster_id(),
            member_id: self.raft.member_id(),
            revision: self.store.current_revision(),
            raft_term: self.raft.current_term(),
        }
    }
}

#[tonic::async_trait]
impl Maintenance for MaintenanceService {
    type SnapshotStream = ReceiverStream<Result<SnapshotResponse, Status>>;

    async fn alarm(
        &self,
        _request: Request<AlarmRequest>,
    ) -> Result<Response<AlarmResponse>, Status> {
        // TODO: Implement alarm management
        let response = AlarmResponse {
            header: Some(self.build_response_header()),
            alarms: vec![],
        };
        Ok(Response::new(response))
    }

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let response = StatusResponse {
            header: Some(self.build_response_header()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            db_size: 0, // TODO: Get db_size from store
            leader: self.raft.member_id(),
            raft_index: 0, // TODO: Get raft_index from raft node
            raft_term: self.raft.current_term(),
            db_size_in_use: 0, // TODO: Get db_size_in_use from store
            errors: vec![],
            is_learner: false,
            raft_applied_index: 0,
        };

        Ok(Response::new(response))
    }

    async fn defragment(
        &self,
        _request: Request<DefragmentRequest>,
    ) -> Result<Response<DefragmentResponse>, Status> {
        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // TODO: Implement defragmentation
        let response = DefragmentResponse {
            header: Some(self.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn hash(&self, _request: Request<HashRequest>) -> Result<Response<HashResponse>, Status> {
        // TODO: Implement hash computation
        let response = HashResponse {
            header: Some(self.build_response_header()),
            hash: 0,
            revision: self.store.current_revision(),
        };

        Ok(Response::new(response))
    }

    async fn hash_kv(
        &self,
        request: Request<HashKvRequest>,
    ) -> Result<Response<HashKvResponse>, Status> {
        let req = request.into_inner();

        // If revision is 0, use current revision
        let revision = if req.revision > 0 {
            req.revision
        } else {
            self.store.current_revision()
        };

        // TODO: Implement hash computation at revision
        let response = HashKvResponse {
            header: Some(self.build_response_header()),
            hash: 0,
            revision,
            compact_revision: self.store.compact_revision(),
        };

        Ok(Response::new(response))
    }

    async fn snapshot(
        &self,
        _request: Request<SnapshotRequest>,
    ) -> Result<Response<Self::SnapshotStream>, Status> {
        let (tx, rx) = mpsc::channel(128);
        let _store = self.store.clone();

        tokio::spawn(async move {
            // TODO: Implement snapshot creation
            // For now, just return empty snapshot
            let response = SnapshotResponse {
                remaining_bytes: 0,
                blob: vec![],
            };
            let _ = tx.send(Ok(response)).await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn move_leader(
        &self,
        request: Request<MoveLeaderRequest>,
    ) -> Result<Response<MoveLeaderResponse>, Status> {
        let req = request.into_inner();

        if req.target_id == 0 {
            return Err(Status::new(
                Code::InvalidArgument,
                "target_id must not be zero",
            ));
        }

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // TODO: Implement leadership transfer
        // For now, just return success

        let response = MoveLeaderResponse {
            header: Some(self.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn downgrade(
        &self,
        request: Request<DowngradeRequest>,
    ) -> Result<Response<DowngradeResponse>, Status> {
        let req = request.into_inner();

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Validate target version
        if req.target_version.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "target_version must not be empty",
            ));
        }

        // Propose downgrade to Raft
        let downgrade_cmd = format!("DOWNGRADE:{}", req.target_version);

        self.raft
            .propose(downgrade_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("downgrade failed: {}", e)))?;

        let response = DowngradeResponse {
            header: Some(self.build_response_header()),
        };

        Ok(Response::new(response))
    }
}
