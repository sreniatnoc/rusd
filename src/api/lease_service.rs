use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Request, Response, Status};
use tracing::{debug, error, info};

use crate::etcdserverpb::lease_server::Lease as LeaseTrait;
use crate::etcdserverpb::*;
use crate::raft::node::RaftNode;
use crate::storage::mvcc::MvccStore;
use crate::watch::WatchHub;

/// Bridge between the server layer and the lease API.
/// Wraps the Raft node, core LeaseManager, MvccStore, and WatchHub.
pub struct ApiLeaseManager {
    pub raft: Arc<RaftNode>,
    pub core: Arc<crate::lease::LeaseManager>,
    pub store: Arc<MvccStore>,
    pub watch_hub: Arc<WatchHub>,
}

impl ApiLeaseManager {
    pub fn new(
        raft: Arc<RaftNode>,
        core: Arc<crate::lease::LeaseManager>,
        store: Arc<MvccStore>,
        watch_hub: Arc<WatchHub>,
    ) -> Self {
        Self {
            raft,
            core,
            store,
            watch_hub,
        }
    }

    pub fn build_response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: 0, // Will be set by server
            member_id: 0,
            revision: self.store.current_revision(),
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
            return Err(Status::new(Code::InvalidArgument, "ttl must be positive"));
        }

        // Check if leader
        if !self.api_lease_mgr.raft.is_leader() {
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Generate lease ID
        let lease_id = self.api_lease_mgr.raft.generate_lease_id();

        // Propose to Raft
        let grant_cmd = format!("GRANT_LEASE:{},{}", lease_id, req.ttl);

        self.api_lease_mgr
            .raft
            .propose(grant_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("raft proposal failed: {}", e)))?;

        // Grant the lease in the core lease manager
        self.api_lease_mgr
            .core
            .grant(lease_id, req.ttl)
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
            return Err(Status::new(Code::FailedPrecondition, "not a leader"));
        }

        // Propose to Raft
        let revoke_cmd = format!("REVOKE_LEASE:{}", req.id);

        self.api_lease_mgr
            .raft
            .propose(revoke_cmd.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("raft proposal failed: {}", e)))?;

        // Revoke the lease in the core lease manager (returns attached keys)
        let keys = self
            .api_lease_mgr
            .core
            .revoke(req.id)
            .map_err(|e| Status::new(Code::Internal, format!("lease revoke failed: {}", e)))?;

        // Delete all attached keys from storage
        for key in &keys {
            let mut end_key = key.clone();
            if let Some(last) = end_key.last_mut() {
                *last = last.wrapping_add(1);
            }

            match self.api_lease_mgr.store.delete_range(key, &end_key) {
                Ok((rev, deleted_kvs)) => {
                    for deleted_kv in &deleted_kvs {
                        let delete_event = crate::watch::Event {
                            event_type: crate::watch::EventType::Delete,
                            kv: crate::watch::KeyValue {
                                key: deleted_kv.key.clone(),
                                create_revision: deleted_kv.create_revision,
                                mod_revision: rev,
                                version: deleted_kv.version,
                                value: Vec::new(),
                                lease: 0,
                            },
                            prev_kv: None,
                        };
                        let _ = self
                            .api_lease_mgr
                            .watch_hub
                            .notify(vec![delete_event], rev, 0);
                    }
                    debug!(key = ?String::from_utf8_lossy(key), "Deleted key on lease revoke");
                }
                Err(e) => {
                    error!(key = ?key, error = %e, "Failed to delete key on lease revoke");
                }
            }
        }

        info!(
            lease_id = req.id,
            deleted_keys = keys.len(),
            "Lease revoked with key cleanup"
        );

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
                    let _ = tx
                        .send(Err(Status::new(
                            Code::InvalidArgument,
                            "lease id must not be zero",
                        )))
                        .await;
                    continue;
                }

                // Propose keepalive to Raft
                let keepalive_cmd = format!("KEEPALIVE_LEASE:{}", keep_alive_req.id);

                match api_lease_mgr.raft.propose(keepalive_cmd.into_bytes()).await {
                    Ok(_) => {
                        // Get the renewed TTL from the lease manager
                        let ttl = api_lease_mgr.core.renew(keep_alive_req.id).unwrap_or(0);

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
                        let _ = tx
                            .send(Err(Status::new(
                                Code::Internal,
                                format!("keepalive failed: {}", e),
                            )))
                            .await;
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
            Err(e) => Err(Status::new(
                Code::NotFound,
                format!("lease not found: {}", e),
            )),
        }
    }

    async fn lease_leases(
        &self,
        _request: Request<LeaseLeasesRequest>,
    ) -> Result<Response<LeaseLeasesResponse>, Status> {
        // Fetch all active leases from the lease manager
        let leases = self
            .api_lease_mgr
            .core
            .list()
            .into_iter()
            .map(|lease| LeaseLeases { id: lease.id })
            .collect();

        let response = LeaseLeasesResponse {
            header: Some(self.api_lease_mgr.build_response_header()),
            leases,
        };

        Ok(Response::new(response))
    }
}
