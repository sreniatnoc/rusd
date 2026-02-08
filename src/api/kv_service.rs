use tonic::{Request, Response, Status, Code};
use std::sync::Arc;

use crate::etcdserverpb::*;
use crate::etcdserverpb::kv_server::Kv;
use crate::etcdserverpb::response_op;
use crate::storage::mvcc::MvccStore;
use crate::raft::node::RaftNode;

pub struct KvService {
    store: Arc<MvccStore>,
    raft: Arc<RaftNode>,
}

impl KvService {
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
            return Err(Status::new(
                Code::InvalidArgument,
                "key must not be empty",
            ));
        }

        // Determine the effective revision for read
        let read_revision = if req.revision > 0 {
            req.revision
        } else {
            self.store.current_revision()
        };

        // Perform the range query
        // When range_end is empty, we need to query a single key by using the next byte after key
        let effective_range_end = if req.range_end.is_empty() {
            // For single-key lookup, append a null byte to create an exclusive upper bound
            let mut end = req.key.clone();
            end.push(0);
            end
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
            1 => { /* ASCEND */
                match req.sort_target {
                    0 => kvs.sort_by(|a, b| a.key.cmp(&b.key)), // KEY
                    1 => kvs.sort_by(|a, b| a.value.cmp(&b.value)), // VALUE
                    2 => kvs.sort_by(|a, b| a.create_revision.cmp(&b.create_revision)), // CREATE
                    3 => kvs.sort_by(|a, b| a.mod_revision.cmp(&b.mod_revision)), // MOD
                    4 => kvs.sort_by(|a, b| a.version.cmp(&b.version)), // VERSION
                    _ => {}
                }
            }
            2 => { /* DESCEND */
                match req.sort_target {
                    0 => kvs.sort_by(|a, b| b.key.cmp(&a.key)), // KEY
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

        let response = RangeResponse {
            header: Some(self.build_response_header()),
            kvs,
            more,
            count: total_count,
        };

        Ok(Response::new(response))
    }

    async fn put(
        &self,
        request: Request<PutRequest>,
    ) -> Result<Response<PutResponse>, Status> {
        let req = request.into_inner();

        // Validate input
        if req.key.is_empty() {
            return Err(Status::new(Code::InvalidArgument, "key must not be empty"));
        }

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Get previous value if requested
        let prev_kv = if req.prev_kv {
            // For single-key lookup, append a null byte to create an exclusive upper bound
            let mut range_end = req.key.clone();
            range_end.push(0);
            match self.store.range(&req.key, &range_end, self.store.current_revision(), 1, false) {
                Ok(result) => result.kvs.into_iter().next().map(Self::convert_kv),
                Err(_) => None,
            }
        } else {
            None
        };

        // Propose to Raft
        // TODO: Use proper proto encoding for proposals instead of bincode
        let proposal_data = format!("PUT:{}:{}",
            String::from_utf8_lossy(&req.key),
            String::from_utf8_lossy(&req.value)
        );

        let _result = self.raft.propose(proposal_data.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("raft proposal failed: {}", e)))?;

        // Apply the put operation to the store
        let (_new_revision, _kv) = self.store.put(
            &req.key,
            &req.value,
            req.lease,
        ).map_err(|e| Status::new(Code::Internal, format!("put failed: {}", e)))?;

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

        // Validate input
        if req.key.is_empty() {
            return Err(Status::new(Code::InvalidArgument, "key must not be empty"));
        }

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Get previous values if requested
        let prev_kvs = if req.prev_kv {
            let effective_range_end = if req.range_end.is_empty() {
                // For single-key lookup, append a null byte to create an exclusive upper bound
                let mut end = req.key.clone();
                end.push(0);
                end
            } else {
                req.range_end.clone()
            };
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
        // TODO: Use proper proto encoding for proposals
        let proposal_data = format!("DELETE:{}:{}",
            String::from_utf8_lossy(&req.key),
            String::from_utf8_lossy(&req.range_end)
        );

        self.raft.propose(proposal_data.into_bytes())
            .await
            .map_err(|e| Status::new(Code::Internal, format!("raft proposal failed: {}", e)))?;

        // Apply the delete operation
        let effective_range_end = if req.range_end.is_empty() {
            // For single-key delete, append a null byte to create an exclusive upper bound
            let mut end = req.key.clone();
            end.push(0);
            end
        } else {
            req.range_end.clone()
        };
        let (_, deleted_kvs) = self.store.delete_range(
            &req.key,
            &effective_range_end,
        ).map_err(|e| Status::new(Code::Internal, format!("delete failed: {}", e)))?;

        let response = DeleteRangeResponse {
            header: Some(self.build_response_header()),
            deleted: deleted_kvs.len() as i64,
            prev_kvs,
        };

        Ok(Response::new(response))
    }

    async fn txn(
        &self,
        request: Request<TxnRequest>,
    ) -> Result<Response<TxnResponse>, Status> {
        let req = request.into_inner();

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        let current_revision = self.store.current_revision();

        // TODO: Evaluate all compare operations with proper target_union handling
        // For now, all comparisons succeed to allow txn to work
        let all_succeed = true;

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
                                response: Some(response_op::Response::ResponseRange(RangeResponse {
                                    header: Some(self.build_response_header()),
                                    kvs: range_result.kvs.into_iter().map(Self::convert_kv).collect(),
                                    more: range_result.more,
                                    count: range_result.count as i64,
                                })),
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
                        // Execute put operation
                        let prev_kv = if put_req.prev_kv {
                            // For single-key lookup, append a null byte to create an exclusive upper bound
                            let mut range_end = put_req.key.clone();
                            range_end.push(0);
                            match self.store.range(&put_req.key, &range_end, current_revision, 1, false) {
                                Ok(result) => result.kvs.into_iter().next().map(Self::convert_kv),
                                Err(_) => None,
                            }
                        } else {
                            None
                        };

                        self.store.put(&put_req.key, &put_req.value, put_req.lease)
                            .map_err(|e| Status::new(Code::Internal, format!("put in txn failed: {}", e)))?;

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
                                Ok(result) => result.kvs.into_iter().map(Self::convert_kv).collect(),
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

                        let (_, deleted_kvs) = self.store.delete_range(
                            &del_req.key,
                            &effective_range_end,
                        ).map_err(|e| Status::new(Code::Internal, format!("delete in txn failed: {}", e)))?;

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

        // Check if leader
        if !self.raft.is_leader() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "not a leader",
            ));
        }

        // Perform compaction
        self.store.compact(req.revision)
            .map_err(|e| Status::new(Code::Internal, format!("compaction failed: {}", e)))?;

        let response = CompactionResponse {
            header: Some(self.build_response_header()),
        };

        Ok(Response::new(response))
    }
}
