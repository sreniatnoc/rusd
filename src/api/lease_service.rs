use tonic::{Request, Response, Status, Code};
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tokio::sync::mpsc;

use crate::etcdserverpb::*;
use crate::etcdserverpb::lease_server::Lease as LeaseTrait;
use crate::raft::node::RaftNode;

/// Bridge between the server layer and the lease API.
/// Wraps both the Raft node (for consensus) and the core LeaseManager (for TTL logic).
pub struct ApiLeaseManager {
    pub raft: Arc<RaftNode>,
    pub core: Arc<crate::lease::LeaseManager>,
}

impl ApiLeaseManager {
    pub fn new(raft: Arc<RaftNode>, core: Arc<crate::lease::LeaseManager>) -> Self {
        Self { raft, core }
    }

    pub fn build_response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: 0, // Will be set by server
            member_id: 0,
            revision: 0,
            raft_term: self.raft.current_term(),
        }
    }
}

pub struct LeaseService {
    api_lease_mgr: Arc<ApiLeaseManager>,
}

impl LeaseService {
    pub fn new(api_lease_mgr: Arc<ApiLeaseManager>) -> Self {
        Self { api_lease_mgr }
    }
}

#[tonic::async_trait]
impl LeaseTrait for LeaseService {
    type LeaseKeepAliveStream = ReceiverStream<Result<LeaseKeepAliveResponse, Status>>;

    async fn lease_grant(
        &self,
        request: Request<LeaseGrantRequest>,
    ) -> Result<Response<LeaseGrantResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.ttl <= 0 {
            return Err(Status::new(
                Code::InvalidArgument,
                "ttl must be positive",
            ));
        }

        // Check if leader
        if !self.api_lease_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Generate lease ID
        let lease_id = self.api_lease_mgr.raft.generate_lease_id();

        // Propose to Raft
        let grant_cmd = format!("GRANT_LEASE:{},{}", lease_id, req.ttl);

        self.api_lease_mgr.raft.propose(grant_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("raft proposal failed: {}", e)))?;

        // Grant the lease in the core lease manager
        self.api_lease_mgr.core.grant(lease_id, req.ttl)
            .map_err(|e| Status::new(Code::Internal, format!("lease grant failed: {}", e)))?;

        let response = LeaseGrantResponse {
            header: Some(self.api_lease_mgr.build_response_header()),
            id: lease_id,
            ttl: req.ttl,
            error: String::new(),
        };

        Ok(Response::new(response))
    }

    async fn lease_revoke(
        &self,
        request: Request<LeaseRevokeRequest>,
    ) -> Result<Response<LeaseRevokeResponse>, Status> {
        let req = request.into_inner();

        if req.id == 0 {
            return Err(Status::new(
                Code::InvalidArgument,
                "lease id must not be zero",
            ));
        }

        // Check if leader
        if !self.api_lease_mgr.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Propose to Raft
        let revoke_cmd = format!("REVOKE_LEASE:{}", req.id);

        self.api_lease_mgr.raft.propose(revoke_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("raft proposal failed: {}", e)))?;

        // Revoke the lease in the core lease manager
        self.api_lease_mgr.core.revoke(req.id)
            .map_err(|e| Status::new(Code::Internal, format!("lease revoke failed: {}", e)))?;

        let response = LeaseRevokeResponse {
            header: Some(self.api_lease_mgr.build_response_header()),
        };

        Ok(Response::new(response))
    }

    async fn lease_keep_alive(
        &self,
        request: Request<tonic::Streaming<LeaseKeepAliveRequest>>,
    ) -> Result<Response<Self::LeaseKeepAliveStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(128);
        let api_lease_mgr = self.api_lease_mgr.clone();

        tokio::spawn(async move {
            while let Ok(Some(keep_alive_req)) = stream.message().await {
                if keep_alive_req.id == 0 {
                    let _ = tx.send(Err(Status::new(
                        Code::InvalidArgument,
                        "lease id must not be zero",
                    ))).await;
                    continue;
                }

                // Propose keepalive to Raft
                let keepalive_cmd = format!("KEEPALIVE_LEASE:{}", keep_alive_req.id);

                match api_lease_mgr.raft.propose(keepalive_cmd.into_bytes()).await {
                    Ok(_) => {
                        // Get the renewed TTL from the lease manager
                        let ttl = api_lease_mgr.core.renew(keep_alive_req.id)
                            .unwrap_or(0);

                        let response = LeaseKeepAliveResponse {
                            header: Some(api_lease_mgr.build_response_header()),
                            id: keep_alive_req.id,
                            ttl,
                        };

                        if tx.send(Ok(response)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(Status::new(
                            Code::Internal,
                            format!("keepalive failed: {}", e),
                        ))).await;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn lease_time_to_live(
        &self,
        request: Request<LeaseTimeToLiveRequest>,
    ) -> Result<Response<LeaseTimeToLiveResponse>, Status> {
        let req = request.into_inner();

        if req.id == 0 {
            return Err(Status::new(
                Code::InvalidArgument,
                "lease id must not be zero",
            ));
        }

        // Fetch the actual TTL from the lease manager
        match self.api_lease_mgr.core.time_to_live(req.id) {
            Ok((ttl, granted_ttl, keys)) => {
                let response = LeaseTimeToLiveResponse {
                    header: Some(self.api_lease_mgr.build_response_header()),
                    id: req.id,
                    ttl,
                    granted_ttl,
                    keys,
                };
                Ok(Response::new(response))
            }
            Err(e) => {
                Err(Status::new(
                    Code::NotFound,
                    format!("lease not found: {}", e),
                ))
            }
        }
    }

    async fn lease_leases(
        &self,
        request: Request<LeaseLeasesRequest>,
    ) -> Result<Response<LeaseLeasesResponse>, Status> {
        // Fetch all active leases from the lease manager
        let leases = self.api_lease_mgr.core.list()
            .into_iter()
            .map(|lease| LeaseLeases {
                id: lease.id,
            })
            .collect();

        let response = LeaseLeasesResponse {
            header: Some(self.api_lease_mgr.build_response_header()),
            leases,
        };

        Ok(Response::new(response))
    }
}
