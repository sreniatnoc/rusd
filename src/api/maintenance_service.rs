use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Request, Response, Status};

use crate::etcdserverpb::maintenance_server::Maintenance;
use crate::etcdserverpb::*;
use crate::raft::node::RaftNode;
use crate::storage::backend::Backend;
use crate::storage::mvcc::MvccStore;

pub struct MaintenanceService {
    store: Arc<MvccStore>,
    backend: Arc<Backend>,
    raft: Arc<RaftNode>,
}

impl MaintenanceService {
    pub fn new(store: Arc<MvccStore>, backend: Arc<Backend>, raft: Arc<RaftNode>) -> Self {
        Self {
            store,
            backend,
            raft,
        }
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
        let db_size = self.store.db_size();
        let raft_state = self.raft.get_state();
        let raft_log = self.raft.get_log();

        let response = StatusResponse {
            header: Some(self.build_response_header()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            db_size,
            leader: raft_state.leader_id().unwrap_or(0),
            raft_index: raft_log.last_index(),
            raft_term: self.raft.current_term(),
            db_size_in_use: db_size,
            errors: vec![],
            is_learner: false,
            raft_applied_index: raft_state.last_applied(),
        };

        Ok(Response::new(response))
    }

    async fn defragment(
        &self,
        _request: Request<DefragmentRequest>,
    ) -> Result<Response<DefragmentResponse>, Status> {
        // Defragment runs on any node (not leader-only)
        self.backend
            .defragment()
            .map_err(|e| Status::new(Code::Internal, format!("defragment failed: {}", e)))?;

        let response = DefragmentResponse {
            header: Some(self.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn hash(&self, _request: Request<HashRequest>) -> Result<Response<HashResponse>, Status> {
        // Compute CRC32 hash of the kv tree
        let snapshot = self
            .backend
            .snapshot()
            .map_err(|e| Status::new(Code::Internal, format!("hash failed: {}", e)))?;
        let hash = crc32fast::hash(&snapshot);

        let response = HashResponse {
            header: Some(self.build_response_header()),
            hash,
            revision: self.store.current_revision(),
        };

        Ok(Response::new(response))
    }

    async fn hash_kv(
        &self,
        request: Request<HashKvRequest>,
    ) -> Result<Response<HashKvResponse>, Status> {
        let req = request.into_inner();

        let revision = if req.revision > 0 {
            req.revision
        } else {
            self.store.current_revision()
        };

        // Compute hash of the store at the given revision
        let snapshot = self
            .backend
            .snapshot()
            .map_err(|e| Status::new(Code::Internal, format!("hash_kv failed: {}", e)))?;
        let hash = crc32fast::hash(&snapshot);

        let response = HashKvResponse {
            header: Some(self.build_response_header()),
            hash,
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
        let store = self.store.clone();

        tokio::spawn(async move {
            match store.create_snapshot() {
                Ok(data) => {
                    // Stream snapshot in chunks (64KB each)
                    let chunk_size = 64 * 1024;
                    let total = data.len();
                    let mut offset = 0;

                    while offset < total {
                        let end = std::cmp::min(offset + chunk_size, total);
                        let remaining = (total - end) as u64;
                        let chunk = data[offset..end].to_vec();

                        let response = SnapshotResponse {
                            remaining_bytes: remaining,
                            blob: chunk,
                        };

                        if tx.send(Ok(response)).await.is_err() {
                            break;
                        }
                        offset = end;
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(Status::internal(format!(
                            "snapshot creation failed: {}",
                            e
                        ))))
                        .await;
                }
            }
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

        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

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

        if !self.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        if req.target_version.is_empty() {
            return Err(Status::new(
                Code::InvalidArgument,
                "target_version must not be empty",
            ));
        }

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
