use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tonic::{Code, Request, Response, Status};

use crate::etcdserverpb::kv_client::KvClient;
use crate::etcdserverpb::kv_server::Kv;
use crate::etcdserverpb::response_op;
use crate::etcdserverpb::*;
use crate::raft::node::RaftNode;
use crate::storage::mvcc::MvccStore;
use crate::watch::WatchHub;

pub struct KvService {
    store: Arc<MvccStore>,
    raft: Arc<RaftNode>,
    watch_hub: Arc<WatchHub>,
    /// Map of member_id → client_url for leader forwarding in multi-node clusters
    peer_client_urls: HashMap<u64, String>,
    /// Cached gRPC clients for leader forwarding
    leader_clients: Arc<RwLock<HashMap<u64, KvClient<tonic::transport::Channel>>>>,
}

impl KvService {
    pub fn new(store: Arc<MvccStore>, raft: Arc<RaftNode>, watch_hub: Arc<WatchHub>) -> Self {
        Self {
            store,
            raft,
            watch_hub,
            peer_client_urls: HashMap::new(),
            leader_clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a KvService with peer client URLs for leader forwarding.
    pub fn with_peer_urls(
        store: Arc<MvccStore>,
        raft: Arc<RaftNode>,
        watch_hub: Arc<WatchHub>,
        peer_client_urls: HashMap<u64, String>,
    ) -> Self {
        Self {
            store,
            raft,
            watch_hub,
            peer_client_urls,
            leader_clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a gRPC client for forwarding to the leader.
    async fn get_leader_client(&self) -> Result<KvClient<tonic::transport::Channel>, Status> {
        let leader_id = self.raft.get_state().leader_id().ok_or_else(|| {
            Status::new(Code::Unavailable, "no leader elected")
        })?;

        // Check cache
        {
            let clients = self.leader_clients.read().await;
            if let Some(client) = clients.get(&leader_id) {
                return Ok(client.clone());
            }
        }

        // Get leader's client URL
        let leader_url = self.peer_client_urls.get(&leader_id).ok_or_else(|| {
            Status::new(
                Code::Unavailable,
                format!("leader {} client URL unknown", leader_id),
            )
        })?;

        // Create new connection
        let endpoint = tonic::transport::Endpoint::from_shared(leader_url.clone())
            .map_err(|e| Status::new(Code::Internal, format!("invalid leader URL: {}", e)))?
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3));

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| Status::new(Code::Unavailable, format!("cannot connect to leader: {}", e)))?;

        let client = KvClient::new(channel);

        // Cache it
        self.leader_clients
            .write()
            .await
            .insert(leader_id, client.clone());

        Ok(client)
    }

    fn build_response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: self.raft.cluster_id(),
            member_id: self.raft.member_id(),
            revision: self.store.current_revision(),
            raft_term: self.raft.current_term(),
        }
    }

    fn convert_kv(kv: crate::storage::mvcc::KeyValue) -> KeyValue {
        KeyValue {
            key: kv.key,
            create_revision: kv.create_revision,
            mod_revision: kv.mod_revision,
            version: kv.version,
            value: kv.value,
            lease: kv.lease,
        }
    }

    fn to_watch_kv(kv: &crate::storage::mvcc::KeyValue) -> crate::watch::KeyValue {
        crate::watch::KeyValue {
            key: kv.key.clone(),
            create_revision: kv.create_revision,
            mod_revision: kv.mod_revision,
            version: kv.version,
            value: kv.value.clone(),
            lease: kv.lease,
        }
    }

    /// Evaluates a single Compare operation for txn.
    fn evaluate_compare(&self, cmp: &Compare, current_revision: i64) -> bool {
        use crate::etcdserverpb::compare::{CompareResult, TargetUnion};

        // Look up the key
        let mut range_end = cmp.key.clone();
        range_end.push(0);
        let kv = match self
            .store
            .range(&cmp.key, &range_end, current_revision, 1, false)
        {
            Ok(result) => result.kvs.into_iter().next(),
            Err(_) => None,
        };

        let result = CompareResult::try_from(cmp.result).unwrap_or(CompareResult::Equal);

        match &cmp.target_union {
            Some(TargetUnion::Version(target_version)) => {
                let actual = kv.as_ref().map(|kv| kv.version).unwrap_or(0);
                Self::compare_i64(actual, *target_version, result)
            }
            Some(TargetUnion::CreateRevision(target_rev)) => {
                let actual = kv.as_ref().map(|kv| kv.create_revision).unwrap_or(0);
                Self::compare_i64(actual, *target_rev, result)
            }
            Some(TargetUnion::ModRevision(target_rev)) => {
                let actual = kv.as_ref().map(|kv| kv.mod_revision).unwrap_or(0);
                Self::compare_i64(actual, *target_rev, result)
            }
            Some(TargetUnion::Value(target_value)) => {
                let actual = kv.as_ref().map(|kv| kv.value.as_slice()).unwrap_or(&[]);
                Self::compare_bytes(actual, target_value, result)
            }
            Some(TargetUnion::Lease(target_lease)) => {
                let actual = kv.as_ref().map(|kv| kv.lease).unwrap_or(0);
                Self::compare_i64(actual, *target_lease, result)
            }
            None => {
                // No target union - comparison is vacuously true
                true
            }
        }
    }

    fn compare_i64(
        actual: i64,
        target: i64,
        result: crate::etcdserverpb::compare::CompareResult,
    ) -> bool {
        use crate::etcdserverpb::compare::CompareResult;
        match result {
            CompareResult::Equal => actual == target,
            CompareResult::Greater => actual > target,
            CompareResult::Less => actual < target,
            CompareResult::NotEqual => actual != target,
        }
    }

    fn compare_bytes(
        actual: &[u8],
        target: &[u8],
        result: crate::etcdserverpb::compare::CompareResult,
    ) -> bool {
        use crate::etcdserverpb::compare::CompareResult;
        match result {
            CompareResult::Equal => actual == target,
            CompareResult::Greater => actual > target,
            CompareResult::Less => actual < target,
            CompareResult::NotEqual => actual != target,
        }
    }
}

#[tonic::async_trait]
impl Kv for KvService {
    async fn range(
        &self,
        request: Request<RangeRequest>,
    ) -> Result<Response<RangeResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.key.is_empty() && req.range_end.is_empty() {
            return Err(Status::new(Code::InvalidArgument, "key must not be empty"));
        }

        // Determine the effective revision for read
        let read_revision = if req.revision > 0 {
            req.revision
        } else {
            self.store.current_revision()
        };

        // Perform the range query
        // etcd range_end conventions:
        //   empty     → single-key lookup (append \0 for exclusive upper bound)
        //   [0x00]    → special sentinel meaning "no upper bound" (all keys from key onwards)
        //   other     → literal exclusive upper bound
        let effective_range_end = if req.range_end.is_empty() {
            // For single-key lookup, append a null byte to create an exclusive upper bound
            let mut end = req.key.clone();
            end.push(0);
            end
        } else if req.range_end == vec![0] {
            // etcd convention: range_end="\x00" means unbounded (no upper limit).
            // Pass empty to index layer which treats empty end as unbounded.
            vec![]
        } else {
            req.range_end.clone()
        };

        let range_result = self.store.range(
            &req.key,
            &effective_range_end,
            read_revision,
            req.limit,
            req.count_only,
        );

        let range_data = match range_result {
            Ok(r) => r,
            Err(e) => {
                return Err(Status::new(
                    Code::Internal,
                    format!("range query failed: {}", e),
                ))
            }
        };

        let total_count = range_data.count as i64;
        let more = range_data.more;
        let mut kvs: Vec<KeyValue> = range_data.kvs.into_iter().map(Self::convert_kv).collect();

        // Apply sorting
        match req.sort_order {
            0 => { /* NONE - no sorting */ }
            1 => {
                /* ASCEND */
                match req.sort_target {
                    0 => kvs.sort_by(|a, b| a.key.cmp(&b.key)),     // KEY
                    1 => kvs.sort_by(|a, b| a.value.cmp(&b.value)), // VALUE
                    2 => kvs.sort_by(|a, b| a.create_revision.cmp(&b.create_revision)), // CREATE
                    3 => kvs.sort_by(|a, b| a.mod_revision.cmp(&b.mod_revision)), // MOD
                    4 => kvs.sort_by(|a, b| a.version.cmp(&b.version)), // VERSION
                    _ => {}
                }
            }
            2 => {
                /* DESCEND */
                match req.sort_target {
                    0 => kvs.sort_by(|a, b| b.key.cmp(&a.key)),     // KEY
                    1 => kvs.sort_by(|a, b| b.value.cmp(&a.value)), // VALUE
                    2 => kvs.sort_by(|a, b| b.create_revision.cmp(&a.create_revision)), // CREATE
                    3 => kvs.sort_by(|a, b| b.mod_revision.cmp(&a.mod_revision)), // MOD
                    4 => kvs.sort_by(|a, b| b.version.cmp(&a.version)), // VERSION
                    _ => {}
                }
            }
            _ => {}
        }

        // Apply filters
        if req.min_create_revision > 0 {
            kvs.retain(|kv| kv.create_revision >= req.min_create_revision);
        }
        if req.max_create_revision > 0 {
            kvs.retain(|kv| kv.create_revision <= req.max_create_revision);
        }
        if req.min_mod_revision > 0 {
            kvs.retain(|kv| kv.mod_revision >= req.min_mod_revision);
        }
        if req.max_mod_revision > 0 {
            kvs.retain(|kv| kv.mod_revision <= req.max_mod_revision);
        }

        // Handle keys_only: strip values from response KVs
        if req.keys_only {
            for kv in &mut kvs {
                kv.value = vec![];
            }
        }

        let response = RangeResponse {
            header: Some(self.build_response_header()),
            kvs,
            more,
            count: total_count,
        };

        Ok(Response::new(response))
    }

    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.key.is_empty() {
            return Err(Status::new(Code::InvalidArgument, "key must not be empty"));
        }

        // If not leader, forward to the leader
        if !self.raft.is_leader() {
            if !self.peer_client_urls.is_empty() {
                let mut client = self.get_leader_client().await?;
                return client.put(Request::new(req)).await;
            }
            return Err(Status::new(Code::Unavailable, "not a leader"));
        }

        // Handle ignore_value: preserve existing value when flag is set
        let effective_value = if req.ignore_value {
            let mut end = req.key.clone();
            end.push(0);
            match self.store.range(
                &req.key,
                &end,
                self.store.current_revision(),
                1,
                false,
            ) {
                Ok(result) => result
                    .kvs
                    .into_iter()
                    .next()
                    .map(|kv| kv.value)
                    .unwrap_or_default(),
                Err(_) => vec![],
            }
        } else {
            req.value.clone()
        };

        // Handle ignore_lease: preserve existing lease when flag is set
        let effective_lease = if req.ignore_lease {
            let mut end = req.key.clone();
            end.push(0);
            match self.store.range(
                &req.key,
                &end,
                self.store.current_revision(),
                1,
                false,
            ) {
                Ok(result) => result
                    .kvs
                    .into_iter()
                    .next()
                    .map(|kv| kv.lease)
                    .unwrap_or(0),
                Err(_) => 0,
            }
        } else {
            req.lease
        };

        // Propose to Raft
        let proposal_data = format!(
            "PUT:{}:{}",
            String::from_utf8_lossy(&req.key),
            String::from_utf8_lossy(&effective_value)
        );

        let log_index = self
            .raft
            .propose(proposal_data.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("raft proposal failed: {}", e)))?;

        // Wait for the entry to be committed (replicated to majority)
        if self.raft.get_state().commit_index() < log_index {
            let rx = self.raft.commit_notifier().register(log_index);
            tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .map_err(|_| {
                    Status::new(Code::DeadlineExceeded, "timed out waiting for raft commit")
                })?
                .map_err(|_| Status::new(Code::Internal, "commit notification channel closed"))?;
        }

        // Apply the put operation to the store
        let (new_revision, kv, old_kv) = self
            .store
            .put(&req.key, &effective_value, effective_lease)
            .map_err(|e| Status::new(Code::Internal, format!("put failed: {}", e)))?;

        // Notify watchers of the put event (include prev_kv for K8s compatibility)
        let watch_prev = old_kv.as_ref().map(|k| Self::to_watch_kv(k));
        let put_event = crate::watch::Event {
            event_type: crate::watch::EventType::Put,
            kv: Self::to_watch_kv(&kv),
            prev_kv: watch_prev,
        };
        let _ = self.watch_hub.notify(vec![put_event], new_revision, 0);

        // Use prev_kv from the store if client requested it
        let prev_kv = if req.prev_kv {
            old_kv.map(Self::convert_kv)
        } else {
            None
        };

        let response = PutResponse {
            header: Some(self.build_response_header()),
            prev_kv,
        };

        Ok(Response::new(response))
    }

    async fn delete_range(
        &self,
        request: Request<DeleteRangeRequest>,
    ) -> Result<Response<DeleteRangeResponse>, Status> {
        let req = request.into_inner();

        // Validate input — allow empty key if range_end is set (prefix delete)
        if req.key.is_empty() && req.range_end.is_empty() {
            return Err(Status::new(Code::InvalidArgument, "key must not be empty"));
        }

        // If not leader, forward to the leader
        if !self.raft.is_leader() {
            if !self.peer_client_urls.is_empty() {
                let mut client = self.get_leader_client().await?;
                return client.delete_range(Request::new(req)).await;
            }
            return Err(Status::new(Code::Unavailable, "not a leader"));
        }

        // Compute effective range_end (same etcd conventions as range)
        let effective_range_end = if req.range_end.is_empty() {
            let mut end = req.key.clone();
            end.push(0);
            end
        } else if req.range_end == vec![0] {
            // etcd convention: range_end="\x00" means unbounded
            vec![]
        } else {
            req.range_end.clone()
        };

        // Get previous values if requested
        let prev_kvs = if req.prev_kv {
            match self.store.range(
                &req.key,
                &effective_range_end,
                self.store.current_revision(),
                0,
                false,
            ) {
                Ok(result) => result.kvs.into_iter().map(Self::convert_kv).collect(),
                Err(e) => {
                    return Err(Status::new(
                        Code::Internal,
                        format!("failed to get previous values: {}", e),
                    ))
                }
            }
        } else {
            vec![]
        };

        // Propose to Raft
        let proposal_data = format!(
            "DELETE:{}:{}",
            String::from_utf8_lossy(&req.key),
            String::from_utf8_lossy(&req.range_end)
        );

        let log_index = self
            .raft
            .propose(proposal_data.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("raft proposal failed: {}", e)))?;

        // Wait for the entry to be committed (replicated to majority)
        if self.raft.get_state().commit_index() < log_index {
            let rx = self.raft.commit_notifier().register(log_index);
            tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .map_err(|_| {
                    Status::new(Code::DeadlineExceeded, "timed out waiting for raft commit")
                })?
                .map_err(|_| Status::new(Code::Internal, "commit notification channel closed"))?;
        }
        let (del_revision, deleted_kvs) =
            self.store
                .delete_range(&req.key, &effective_range_end)
                .map_err(|e| Status::new(Code::Internal, format!("delete failed: {}", e)))?;

        // Notify watchers of delete events (include prev_kv for K8s)
        if !deleted_kvs.is_empty() {
            let delete_events: Vec<crate::watch::Event> = deleted_kvs
                .iter()
                .map(|kv| crate::watch::Event {
                    event_type: crate::watch::EventType::Delete,
                    kv: Self::to_watch_kv(kv),
                    prev_kv: Some(Self::to_watch_kv(kv)),
                })
                .collect();
            let _ = self.watch_hub.notify(delete_events, del_revision, 0);
        }

        let response = DeleteRangeResponse {
            header: Some(self.build_response_header()),
            deleted: deleted_kvs.len() as i64,
            prev_kvs,
        };

        Ok(Response::new(response))
    }

    async fn txn(&self, request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        let req = request.into_inner();

        // If not leader, forward to the leader
        if !self.raft.is_leader() {
            if !self.peer_client_urls.is_empty() {
                let mut client = self.get_leader_client().await?;
                return client.txn(Request::new(req)).await;
            }
            return Err(Status::new(Code::Unavailable, "not a leader"));
        }

        let current_revision = self.store.current_revision();

        // Evaluate all compare operations
        let all_succeed = req
            .compare
            .iter()
            .all(|cmp| self.evaluate_compare(cmp, current_revision));

        // Select success or failure operations
        let ops = if all_succeed {
            &req.success
        } else {
            &req.failure
        };

        let mut responses = Vec::new();

        // Execute operations
        for op in ops {
            if let Some(request_op) = &op.request {
                let response = match request_op {
                    request_op::Request::RequestRange(range_req) => {
                        // Execute range operation
                        let effective_range_end = if range_req.range_end.is_empty() {
                            // For single-key lookup, append a null byte to create an exclusive upper bound
                            let mut end = range_req.key.clone();
                            end.push(0);
                            end
                        } else {
                            range_req.range_end.clone()
                        };
                        match self.store.range(
                            &range_req.key,
                            &effective_range_end,
                            current_revision,
                            0,
                            false,
                        ) {
                            Ok(range_result) => ResponseOp {
                                response: Some(response_op::Response::ResponseRange(
                                    RangeResponse {
                                        header: Some(self.build_response_header()),
                                        kvs: range_result
                                            .kvs
                                            .into_iter()
                                            .map(Self::convert_kv)
                                            .collect(),
                                        more: range_result.more,
                                        count: range_result.count as i64,
                                    },
                                )),
                            },
                            Err(e) => {
                                return Err(Status::new(
                                    Code::Internal,
                                    format!("range in txn failed: {}", e),
                                ))
                            }
                        }
                    }
                    request_op::Request::RequestPut(put_req) => {
                        // Execute put operation (returns prev_kv from store)
                        let (txn_put_rev, txn_put_kv, txn_old_kv) = self
                            .store
                            .put(&put_req.key, &put_req.value, put_req.lease)
                            .map_err(|e| {
                                Status::new(Code::Internal, format!("put in txn failed: {}", e))
                            })?;

                        // Notify watchers (include prev_kv for K8s)
                        let watch_prev = txn_old_kv.as_ref().map(|k| Self::to_watch_kv(k));
                        let put_event = crate::watch::Event {
                            event_type: crate::watch::EventType::Put,
                            kv: Self::to_watch_kv(&txn_put_kv),
                            prev_kv: watch_prev,
                        };
                        let _ = self.watch_hub.notify(vec![put_event], txn_put_rev, 0);

                        let prev_kv = if put_req.prev_kv {
                            txn_old_kv.map(Self::convert_kv)
                        } else {
                            None
                        };

                        ResponseOp {
                            response: Some(response_op::Response::ResponsePut(PutResponse {
                                header: Some(self.build_response_header()),
                                prev_kv,
                            })),
                        }
                    }
                    request_op::Request::RequestDeleteRange(del_req) => {
                        // Execute delete operation
                        let effective_range_end = if del_req.range_end.is_empty() {
                            // For single-key delete, append a null byte to create an exclusive upper bound
                            let mut end = del_req.key.clone();
                            end.push(0);
                            end
                        } else {
                            del_req.range_end.clone()
                        };
                        let prev_kvs = if del_req.prev_kv {
                            match self.store.range(
                                &del_req.key,
                                &effective_range_end,
                                current_revision,
                                0,
                                false,
                            ) {
                                Ok(result) => {
                                    result.kvs.into_iter().map(Self::convert_kv).collect()
                                }
                                Err(e) => {
                                    return Err(Status::new(
                                        Code::Internal,
                                        format!("delete in txn failed: {}", e),
                                    ))
                                }
                            }
                        } else {
                            vec![]
                        };

                        let (txn_del_rev, deleted_kvs) = self
                            .store
                            .delete_range(&del_req.key, &effective_range_end)
                            .map_err(|e| {
                                Status::new(Code::Internal, format!("delete in txn failed: {}", e))
                            })?;

                        // Notify watchers (include prev_kv for K8s)
                        if !deleted_kvs.is_empty() {
                            let delete_events: Vec<crate::watch::Event> = deleted_kvs
                                .iter()
                                .map(|kv| crate::watch::Event {
                                    event_type: crate::watch::EventType::Delete,
                                    kv: Self::to_watch_kv(kv),
                                    prev_kv: Some(Self::to_watch_kv(kv)),
                                })
                                .collect();
                            let _ = self.watch_hub.notify(delete_events, txn_del_rev, 0);
                        }

                        ResponseOp {
                            response: Some(response_op::Response::ResponseDeleteRange(
                                DeleteRangeResponse {
                                    header: Some(self.build_response_header()),
                                    deleted: deleted_kvs.len() as i64,
                                    prev_kvs,
                                },
                            )),
                        }
                    }
                    request_op::Request::RequestTxn(_txn_req) => {
                        return Err(Status::new(
                            Code::InvalidArgument,
                            "nested transactions are not supported",
                        ))
                    }
                };

                responses.push(response);
            }
        }

        let response = TxnResponse {
            header: Some(self.build_response_header()),
            succeeded: all_succeed,
            responses,
        };

        Ok(Response::new(response))
    }

    async fn compact(
        &self,
        request: Request<CompactionRequest>,
    ) -> Result<Response<CompactionResponse>, Status> {
        let req = request.into_inner();

        if req.revision <= 0 {
            return Err(Status::new(
                Code::InvalidArgument,
                "revision must be positive",
            ));
        }

        // If not leader, forward to the leader
        if !self.raft.is_leader() {
            if !self.peer_client_urls.is_empty() {
                let mut client = self.get_leader_client().await?;
                return client.compact(Request::new(req)).await;
            }
            return Err(Status::new(Code::Unavailable, "not a leader"));
        }

        // Perform compaction
        self.store
            .compact(req.revision)
            .map_err(|e| Status::new(Code::Internal, format!("compaction failed: {}", e)))?;

        let response = CompactionResponse {
            header: Some(self.build_response_header()),
        };

        Ok(Response::new(response))
    }
}
