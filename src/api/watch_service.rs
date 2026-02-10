use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Request, Response, Status};
use tracing::{debug, warn};

use crate::etcdserverpb::watch_server::Watch;
use crate::etcdserverpb::*;
use crate::raft::node::RaftNode;
use crate::storage::mvcc::MvccStore;
use crate::watch::{WatchFilter, WatchHub};

pub struct WatchService {
    store: Arc<MvccStore>,
    raft: Arc<RaftNode>,
    watch_hub: Arc<WatchHub>,
}

impl WatchService {
    pub fn new(store: Arc<MvccStore>, raft: Arc<RaftNode>, watch_hub: Arc<WatchHub>) -> Self {
        Self {
            store,
            raft,
            watch_hub,
        }
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
        let watch_hub = self.watch_hub.clone();

        tokio::spawn(async move {
            let mut active_watches: std::collections::HashMap<i64, tokio::task::JoinHandle<()>> =
                std::collections::HashMap::new();

            loop {
                match stream.message().await {
                    Ok(Some(watch_req)) => {
                        if let Some(request_union) = watch_req.request_union {
                            match request_union {
                                watch_request::RequestUnion::CreateRequest(create_req) => {
                                    // Validate key
                                    if create_req.key.is_empty() {
                                        let _ = tx
                                            .send(Err(Status::new(
                                                Code::InvalidArgument,
                                                "key must not be empty",
                                            )))
                                            .await;
                                        continue;
                                    }

                                    // Parse filters from proto i32 to WatchFilter
                                    let filters: Vec<WatchFilter> = create_req
                                        .filters
                                        .iter()
                                        .filter_map(|f| match *f {
                                            0 => Some(WatchFilter::NoPut),
                                            1 => Some(WatchFilter::NoDelete),
                                            _ => None,
                                        })
                                        .collect();

                                    let start_revision = if create_req.start_revision > 0 {
                                        create_req.start_revision
                                    } else {
                                        store.current_revision() + 1
                                    };

                                    // Create mpsc channel for this watch's events from WatchHub
                                    let (watch_tx, mut watch_rx) = mpsc::channel(256);

                                    // Register watch with WatchHub
                                    let watch_id = match watch_hub.create_watch(
                                        create_req.key.clone(),
                                        create_req.range_end.clone(),
                                        start_revision,
                                        filters,
                                        create_req.prev_kv,
                                        create_req.progress_notify,
                                        create_req.fragment,
                                        watch_tx,
                                    ) {
                                        Ok(id) => id,
                                        Err(e) => {
                                            warn!(error = %e, "Failed to create watch");
                                            let _ = tx
                                                .send(Err(Status::new(
                                                    Code::Internal,
                                                    format!("failed to create watch: {}", e),
                                                )))
                                                .await;
                                            continue;
                                        }
                                    };

                                    debug!(watch_id, "Watch registered with WatchHub");

                                    // Send watch created acknowledgment
                                    let response = WatchResponse {
                                        header: Some(ResponseHeader {
                                            cluster_id: raft.cluster_id(),
                                            member_id: raft.member_id(),
                                            revision: store.current_revision(),
                                            raft_term: raft.current_term(),
                                        }),
                                        watch_id,
                                        created: true,
                                        canceled: false,
                                        cancel_reason: String::new(),
                                        events: vec![],
                                        compact_revision: 0,
                                        fragment: false,
                                    };

                                    if tx.send(Ok(response)).await.is_err() {
                                        break;
                                    }

                                    // Spawn task to forward WatchHub events to gRPC stream
                                    let tx_clone = tx.clone();
                                    let raft_clone = raft.clone();
                                    let store_clone = store.clone();
                                    let forward_handle = tokio::spawn(async move {
                                        while let Some(watch_event) = watch_rx.recv().await {
                                            // Convert internal watch::Event -> proto Event
                                            let events: Vec<Event> = watch_event
                                                .events
                                                .into_iter()
                                                .map(|e| {
                                                    let event_type = match e.event_type {
                                                        crate::watch::EventType::Put => 0,
                                                        crate::watch::EventType::Delete => 1,
                                                    };
                                                    Event {
                                                        r#type: event_type,
                                                        kv: Some(KeyValue {
                                                            key: e.kv.key,
                                                            create_revision: e.kv.create_revision,
                                                            mod_revision: e.kv.mod_revision,
                                                            version: e.kv.version,
                                                            value: e.kv.value,
                                                            lease: e.kv.lease,
                                                        }),
                                                        prev_kv: e.prev_kv.map(|pk| KeyValue {
                                                            key: pk.key,
                                                            create_revision: pk.create_revision,
                                                            mod_revision: pk.mod_revision,
                                                            version: pk.version,
                                                            value: pk.value,
                                                            lease: pk.lease,
                                                        }),
                                                    }
                                                })
                                                .collect();

                                            let is_progress = events.is_empty();

                                            let response = WatchResponse {
                                                header: Some(ResponseHeader {
                                                    cluster_id: raft_clone.cluster_id(),
                                                    member_id: raft_clone.member_id(),
                                                    revision: store_clone.current_revision(),
                                                    raft_term: raft_clone.current_term(),
                                                }),
                                                watch_id: watch_event.watch_id,
                                                created: false,
                                                canceled: false,
                                                cancel_reason: String::new(),
                                                events,
                                                compact_revision: watch_event.compact_revision,
                                                fragment: false,
                                            };

                                            if tx_clone.send(Ok(response)).await.is_err() {
                                                break;
                                            }
                                        }
                                    });

                                    active_watches.insert(watch_id, forward_handle);
                                }
                                watch_request::RequestUnion::CancelRequest(cancel_req) => {
                                    let watch_id = cancel_req.watch_id;

                                    // Cancel in WatchHub
                                    let _ = watch_hub.cancel_watch(watch_id);

                                    // Abort the forwarding task
                                    if let Some(handle) = active_watches.remove(&watch_id) {
                                        handle.abort();
                                    }

                                    debug!(watch_id, "Watch canceled");

                                    let response = WatchResponse {
                                        header: Some(ResponseHeader {
                                            cluster_id: raft.cluster_id(),
                                            member_id: raft.member_id(),
                                            revision: store.current_revision(),
                                            raft_term: raft.current_term(),
                                        }),
                                        watch_id,
                                        created: false,
                                        canceled: true,
                                        cancel_reason: String::new(),
                                        events: vec![],
                                        compact_revision: 0,
                                        fragment: false,
                                    };

                                    let _ = tx.send(Ok(response)).await;
                                }
                                watch_request::RequestUnion::ProgressRequest(_) => {
                                    let response = WatchResponse {
                                        header: Some(ResponseHeader {
                                            cluster_id: raft.cluster_id(),
                                            member_id: raft.member_id(),
                                            revision: store.current_revision(),
                                            raft_term: raft.current_term(),
                                        }),
                                        watch_id: 0,
                                        created: false,
                                        canceled: false,
                                        cancel_reason: String::new(),
                                        events: vec![],
                                        compact_revision: 0,
                                        fragment: false,
                                    };

                                    let _ = tx.send(Ok(response)).await;
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        // Client closed the stream - cancel all active watches
                        for (watch_id, handle) in active_watches.drain() {
                            let _ = watch_hub.cancel_watch(watch_id);
                            handle.abort();
                        }
                        break;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::new(
                                Code::Internal,
                                format!("stream error: {}", e),
                            )))
                            .await;
                        // Cancel all watches on error
                        for (watch_id, handle) in active_watches.drain() {
                            let _ = watch_hub.cancel_watch(watch_id);
                            handle.abort();
                        }
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
