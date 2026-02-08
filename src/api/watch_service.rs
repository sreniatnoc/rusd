use tonic::{Request, Response, Status, Code};
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use tokio::sync::mpsc;

use crate::etcdserverpb::*;
use crate::etcdserverpb::watch_server::Watch;
use crate::storage::mvcc::MvccStore;
use crate::raft::node::RaftNode;

pub struct WatchService {
    store: Arc<MvccStore>,
    raft: Arc<RaftNode>,
}

impl WatchService {
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

    async fn handle_watch_request(
        &self,
        watch_id: i64,
        req: WatchCreateRequest,
        tx: mpsc::Sender<Result<WatchResponse, Status>>,
    ) -> Result<(), Status> {
        // Send watch created response
        let response = WatchResponse {
            header: Some(self.build_response_header()),
            watch_id,
            created: true,
            canceled: false,
            cancel_reason: String::new(),
            events: vec![],
            compact_revision: 0,
            fragment: false,
            progress_notify: false,
        };

        tx.send(Ok(response))
            .await
            .map_err(|e| Status::new(Code::Internal, format!("failed to send response: {}", e)))?;

        // Determine the revision to start watching from
        let start_revision = if req.start_revision > 0 {
            req.start_revision
        } else {
            self.store.current_revision() + 1
        };

        // TODO: Implement proper watch mechanism
        // For now, just return empty watch response
        tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        Ok(())
    }
}

#[tonic::async_trait]
impl Watch for WatchService {
    type WatchStream = ReceiverStream<Result<WatchResponse, Status>>;

    async fn watch(
        &self,
        request: Request<tonic::Streaming<WatchRequest>>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(128);

        let store = self.store.clone();
        let raft = self.raft.clone();

        tokio::spawn(async move {
            let mut next_watch_id: i64 = 1;
            let mut active_watches = std::collections::HashMap::new();

            loop {
                match stream.message().await {
                    Ok(Some(watch_req)) => {
                        if let Some(request_union) = watch_req.request_union {
                            match request_union {
                                watch_request::RequestUnion::CreateRequest(create_req) => {
                                    let watch_id = next_watch_id;
                                    next_watch_id += 1;

                                    // Validate key
                                    if create_req.key.is_empty() {
                                        let _ = tx.send(Err(Status::new(
                                            Code::InvalidArgument,
                                            "key must not be empty",
                                        ))).await;
                                        continue;
                                    }

                                    // Send watch created response
                                    let response_header = ResponseHeader {
                                        cluster_id: raft.cluster_id(),
                                        member_id: raft.member_id(),
                                        revision: store.current_revision(),
                                        raft_term: raft.current_term(),
                                    };

                                    let response = WatchResponse {
                                        header: Some(response_header),
                                        watch_id,
                                        created: true,
                                        canceled: false,
                                        cancel_reason: String::new(),
                                        events: vec![],
                                        compact_revision: 0,
                                        fragment: false,
                                        progress_notify: false,
                                    };

                                    if tx.send(Ok(response)).await.is_err() {
                                        break;
                                    }

                                    // Store watch info
                                    active_watches.insert(watch_id, create_req);

                                    // In a real implementation, we would spawn a task to handle this watch
                                    // For now, we just acknowledge it
                                }
                                watch_request::RequestUnion::CancelRequest(cancel_req) => {
                                    active_watches.remove(&cancel_req.watch_id);

                                    let response_header = ResponseHeader {
                                        cluster_id: raft.cluster_id(),
                                        member_id: raft.member_id(),
                                        revision: store.current_revision(),
                                        raft_term: raft.current_term(),
                                    };

                                    let response = WatchResponse {
                                        header: Some(response_header),
                                        watch_id: cancel_req.watch_id,
                                        created: false,
                                        canceled: true,
                                        cancel_reason: String::new(),
                                        events: vec![],
                                        compact_revision: 0,
                                        fragment: false,
                                        progress_notify: false,
                                    };

                                    let _ = tx.send(Ok(response)).await;
                                }
                                watch_request::RequestUnion::ProgressRequest(_) => {
                                    let response_header = ResponseHeader {
                                        cluster_id: raft.cluster_id(),
                                        member_id: raft.member_id(),
                                        revision: store.current_revision(),
                                        raft_term: raft.current_term(),
                                    };

                                    let response = WatchResponse {
                                        header: Some(response_header),
                                        watch_id: 0,
                                        created: false,
                                        canceled: false,
                                        cancel_reason: String::new(),
                                        events: vec![],
                                        compact_revision: 0,
                                        fragment: false,
                                        progress_notify: false,
                                    };

                                    let _ = tx.send(Ok(response)).await;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        // Client closed the stream
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(Status::new(
                            Code::Internal,
                            format!("stream error: {}", e),
                        ))).await;
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
