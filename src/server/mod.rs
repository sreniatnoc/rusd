//! Main server module that orchestrates all subsystems.
//!
//! This module coordinates the initialization and execution of:
//! - Persistent storage backend (sled-based key-value store)
//! - MVCC store (multi-version concurrency control)
//! - Raft consensus engine (distributed state machine)
//! - Watch hub (real-time change notifications)
//! - Lease manager (TTL-based key expiration)
//! - Authentication/RBAC store
//! - Cluster membership management
//! - gRPC service servers

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::transport::Server;
use tracing::{debug, error, info, warn};

use crate::api::kv_service::KvService;
use crate::api::watch_service::WatchService;
use crate::api::lease_service::LeaseService;
use crate::api::cluster_service::ClusterService;
use crate::api::maintenance_service::MaintenanceService;
use crate::api::auth_service::AuthService;
use crate::auth::AuthStore;
use crate::cluster::ClusterManager;
use crate::etcdserverpb::{
    auth_server::AuthServer, cluster_server::ClusterServer, kv_server::KvServer,
    lease_server::LeaseServer, maintenance_server::MaintenanceServer, watch_server::WatchServer,
};
use crate::lease::LeaseManager;
use crate::raft::config::{RaftConfig, PeerConfig};
use crate::raft::log::RaftLog;
use crate::raft::node::RaftNode;
use crate::raft::transport::GrpcTransport;
use crate::storage::backend::{Backend, BackendConfig};
use crate::storage::mvcc::MvccStore;
use crate::watch::WatchHub;

/// Main rusd server that coordinates all subsystems.
pub struct RusdServer {
    config: ServerConfig,
    backend: Arc<Backend>,
    store: Arc<MvccStore>,
    raft: Arc<RaftNode>,
    watch_hub: Arc<WatchHub>,
    lease_mgr: Arc<LeaseManager>,
    auth_store: Arc<AuthStore>,
    cluster_mgr: Arc<ClusterManager>,
    background_tasks: Vec<JoinHandle<()>>,
}

/// Configuration for the rusd server, matching etcd's CLI flags.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Human-readable name for this member.
    pub name: String,

    /// Path to the data directory.
    pub data_dir: PathBuf,

    /// List of URLs to listen on for client traffic.
    pub listen_client_urls: Vec<String>,

    /// List of URLs to listen on for peer traffic.
    pub listen_peer_urls: Vec<String>,

    /// List of this member's client URLs to advertise to the public.
    pub advertise_client_urls: Vec<String>,

    /// List of this member's peer URLs to advertise to the rest of the cluster.
    pub initial_advertise_peer_urls: Vec<String>,

    /// Initial cluster configuration for bootstrapping.
    /// Format: "node1=http://...,node2=http://..."
    pub initial_cluster: String,

    /// Initial cluster state ('new' or 'existing').
    pub initial_cluster_state: ClusterState,

    /// Initial cluster token for the cluster during bootstrap.
    pub initial_cluster_token: String,

    /// Number of committed transactions to trigger a snapshot to disk.
    pub snapshot_count: u64,

    /// Time in milliseconds of a heartbeat interval.
    pub heartbeat_interval_ms: u64,

    /// Time in milliseconds for an election to timeout.
    pub election_timeout_ms: u64,

    /// Maximum number of snapshot files to retain (0 is unlimited).
    pub max_snapshots: u32,

    /// Maximum number of WAL files to retain (0 is unlimited).
    pub max_wals: u32,

    /// Raise alarms when backend size exceeds the given quota (0 defaults to 8GB).
    pub quota_backend_bytes: u64,

    /// Auto compaction mode (periodic or revision).
    pub auto_compaction_mode: AutoCompactionMode,

    /// Auto compaction retention (retention string or revision number).
    pub auto_compaction_retention: String,

    /// Backend page cache size in megabytes.
    pub cache_size_mb: u64,
}

/// Cluster initialization state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClusterState {
    /// Bootstrap a new cluster.
    New,
    /// Join an existing cluster.
    Existing,
}

impl ClusterState {
    /// Parse from string (etcd-compatible).
    pub fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.to_lowercase().as_str() {
            "new" => Ok(ClusterState::New),
            "existing" => Ok(ClusterState::Existing),
            _ => Err(anyhow::anyhow!(
                "Invalid cluster state: {}. Must be 'new' or 'existing'",
                s
            )),
        }
    }
}

/// Auto compaction mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoCompactionMode {
    /// Periodic compaction.
    Periodic,
    /// Compaction by revision.
    Revision,
}

impl AutoCompactionMode {
    /// Parse from string (etcd-compatible).
    pub fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.to_lowercase().as_str() {
            "periodic" => Ok(AutoCompactionMode::Periodic),
            "revision" => Ok(AutoCompactionMode::Revision),
            _ => Err(anyhow::anyhow!(
                "Invalid compaction mode: {}. Must be 'periodic' or 'revision'",
                s
            )),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            data_dir: PathBuf::from("default.rusd"),
            listen_client_urls: vec!["http://localhost:2379".to_string()],
            listen_peer_urls: vec!["http://localhost:2380".to_string()],
            advertise_client_urls: vec!["http://localhost:2379".to_string()],
            initial_advertise_peer_urls: vec!["http://localhost:2380".to_string()],
            initial_cluster: "default=http://localhost:2380".to_string(),
            initial_cluster_state: ClusterState::New,
            initial_cluster_token: "rusd-cluster".to_string(),
            snapshot_count: 100000,
            heartbeat_interval_ms: 100,
            election_timeout_ms: 1000,
            max_snapshots: 5,
            max_wals: 5,
            quota_backend_bytes: 8 * 1024 * 1024 * 1024, // 8GB
            auto_compaction_mode: AutoCompactionMode::Periodic,
            auto_compaction_retention: "0".to_string(),
            cache_size_mb: 256,
        }
    }
}

impl RusdServer {
    /// Create a new rusd server instance with the given configuration.
    ///
    /// This initializes all subsystems:
    /// 1. Persistent storage backend
    /// 2. MVCC store for versioned data
    /// 3. Raft consensus engine
    /// 4. Watch hub for change notifications
    /// 5. Lease manager for TTL semantics
    /// 6. Authentication/RBAC store
    /// 7. Cluster manager for membership
    pub async fn new(config: ServerConfig) -> anyhow::Result<Self> {
        info!(
            name = %config.name,
            data_dir = %config.data_dir.display(),
            "Initializing rusd server"
        );

        // Generate a unique member ID and cluster ID
        let member_id = uuid::Uuid::new_v4().as_u64_pair().0;
        let cluster_id = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            config.initial_cluster_token.hash(&mut hasher);
            hasher.finish()
        };

        // 1. Initialize backend storage (sled-based)
        let backend_config = BackendConfig {
            data_dir: config.data_dir.clone(),
            cache_size_mb: config.cache_size_mb,
            flush_interval_ms: 1000,
            compression: true,
        };
        let backend = Backend::new(backend_config)
            .map_err(|e| anyhow::anyhow!("Failed to initialize backend: {}", e))?;
        info!("Backend storage initialized");

        // 2. Initialize MVCC store
        let store = MvccStore::new(backend.clone())
            .map_err(|e| anyhow::anyhow!("Failed to initialize MVCC store: {}", e))?;
        info!("MVCC store initialized");

        // 3. Initialize Raft consensus
        // Open a dedicated sled tree for the Raft log
        let raft_db = sled::open(config.data_dir.join("raft"))
            .map_err(|e| anyhow::anyhow!("Failed to open Raft log database: {}", e))?;
        let raft_tree = raft_db
            .open_tree("raft_log")
            .map_err(|e| anyhow::anyhow!("Failed to open Raft log tree: {}", e))?;
        let raft_log = Arc::new(
            RaftLog::new(raft_tree)
                .map_err(|e| anyhow::anyhow!("Failed to initialize Raft log: {}", e))?,
        );

        // Build peer addresses map from initial cluster configuration
        let mut peer_addresses = std::collections::HashMap::new();
        let peers = parse_peer_configs(&config.initial_cluster, &config.name);
        for peer in &peers {
            peer_addresses.insert(peer.id, format!("http://{}:2379", peer.id)); // TODO: Use actual peer URLs
        }
        let transport = Arc::new(GrpcTransport::new(peer_addresses));
        let (apply_tx, apply_rx) = mpsc::channel(10000);

        // Parse peers from initial_cluster config
        let peers = parse_peer_configs(&config.initial_cluster, &config.name);

        let raft_config = RaftConfig {
            id: member_id,
            election_timeout_min: Duration::from_millis(config.election_timeout_ms),
            election_timeout_max: Duration::from_millis(config.election_timeout_ms * 2),
            heartbeat_interval: Duration::from_millis(config.heartbeat_interval_ms),
            max_log_entries_per_request: 100,
            snapshot_threshold: config.snapshot_count,
            peers,
            data_dir: config.data_dir.clone(),
            pre_vote: true,
        };
        let raft = Arc::new(
            RaftNode::new(raft_config, raft_log, transport, apply_tx)
                .map_err(|e| anyhow::anyhow!("Failed to initialize Raft node: {}", e))?,
        );
        info!(member_id = member_id, "Raft node initialized");

        // 4. Initialize Cluster manager
        let cluster_mgr = ClusterManager::new(cluster_id, member_id)
            .map_err(|e| anyhow::anyhow!("Failed to initialize cluster manager: {}", e))?;
        register_initial_members(&cluster_mgr, &config.initial_cluster, &config.name)?;
        info!(
            initial_cluster = %config.initial_cluster,
            "Cluster configuration parsed"
        );

        // 5. Initialize Watch hub
        let watch_hub = WatchHub::new(cluster_id, member_id);
        info!("Watch hub initialized");

        // 6. Initialize Lease manager (with expiry channel)
        let (expire_tx, expire_rx) = mpsc::channel(1000);
        let lease_mgr = Arc::new(LeaseManager::new(expire_tx));
        info!("Lease manager initialized");

        // 7. Initialize Auth store
        let auth_store = Arc::new(AuthStore::new(None));
        info!("Auth store initialized");

        let mut server = Self {
            config,
            backend,
            store,
            raft,
            watch_hub,
            lease_mgr,
            auth_store,
            cluster_mgr,
            background_tasks: Vec::new(),
        };

        // Start background task to process Raft apply channel
        let store_clone = server.store.clone();
        let watch_hub_clone = server.watch_hub.clone();
        let apply_handle = tokio::spawn(async move {
            process_apply_channel(apply_rx, store_clone, watch_hub_clone).await;
        });
        server.background_tasks.push(apply_handle);

        // Start lease expiry background task
        let lease_mgr_clone = server.lease_mgr.clone();
        let store_clone = server.store.clone();
        let watch_hub_clone = server.watch_hub.clone();
        let expiry_handle = tokio::spawn(async move {
            process_lease_expiries(expire_rx, store_clone, watch_hub_clone).await;
        });
        server.background_tasks.push(expiry_handle);

        info!("rusd server initialization complete");
        Ok(server)
    }

    /// Run the server, starting the gRPC server and all background tasks.
    ///
    /// This method will block until the shutdown signal is received or an error occurs.
    pub async fn run(mut self, shutdown: impl std::future::Future<Output = ()>) -> anyhow::Result<()> {
        // Start Raft node's event loop (now Send-safe with std::sync::Mutex)
        let raft_handle = self.raft.clone().run();
        self.background_tasks.push(raft_handle);

        // Parse client URLs and start gRPC server
        let client_urls = parse_socket_addrs(&self.config.listen_client_urls)?;
        if client_urls.is_empty() {
            return Err(anyhow::anyhow!("No valid client URLs to listen on"));
        }

        // We use the first URL as the primary listener
        let addr = client_urls[0];

        info!(addr = %addr, "Starting gRPC server");

        // Create service instances matching their actual constructors
        let kv_service = KvService::new(self.store.clone(), self.raft.clone(), self.watch_hub.clone());
        let watch_service = WatchService::new(self.store.clone(), self.raft.clone(), self.watch_hub.clone());

        // LeaseService needs the ApiLeaseManager bridge (raft + core lease manager + store + watch_hub)
        let api_lease_mgr = Arc::new(crate::api::lease_service::ApiLeaseManager::new(
            self.raft.clone(),
            self.lease_mgr.clone(),
            self.store.clone(),
            self.watch_hub.clone(),
        ));
        let lease_service = LeaseService::new(api_lease_mgr);

        // ClusterService needs raft and a shared member list
        let members = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let cluster_service = ClusterService::new(self.raft.clone(), members);

        let maintenance_service =
            MaintenanceService::new(self.store.clone(), self.raft.clone());

        // AuthService needs the AuthManager wrapper
        let auth_mgr_for_service = Arc::new(crate::api::auth_service::AuthManager::new(
            self.raft.clone(),
        ));
        let auth_service = AuthService::new(auth_mgr_for_service);

        // Build gRPC server with all services and HTTP/2 settings for K8s compat
        let server = Server::builder()
            .http2_keepalive_interval(Some(Duration::from_secs(10)))
            .http2_keepalive_timeout(Some(Duration::from_secs(20)))
            .initial_connection_window_size(Some(1024 * 1024))  // 1MB
            .initial_stream_window_size(Some(1024 * 1024))      // 1MB
            .add_service(KvServer::new(kv_service))
            .add_service(WatchServer::new(watch_service))
            .add_service(LeaseServer::new(lease_service))
            .add_service(ClusterServer::new(cluster_service))
            .add_service(MaintenanceServer::new(maintenance_service))
            .add_service(AuthServer::new(auth_service))
            .serve_with_shutdown(addr, shutdown);

        info!("rusd server listening on {}", addr);
        info!("Data directory: {}", self.config.data_dir.display());
        info!("Member name: {}", self.config.name);
        info!(
            "Advertise client URLs: {}",
            self.config.advertise_client_urls.join(", ")
        );

        // Run server until shutdown signal or error
        server.await?;

        info!("rusd server shutting down");
        Ok(())
    }

    /// Get reference to the MVCC store.
    pub fn store(&self) -> Arc<MvccStore> {
        self.store.clone()
    }

    /// Get reference to the Raft node.
    pub fn raft(&self) -> Arc<RaftNode> {
        self.raft.clone()
    }

    /// Get reference to the Watch hub.
    pub fn watch_hub(&self) -> Arc<WatchHub> {
        self.watch_hub.clone()
    }

    /// Get reference to the Lease manager.
    pub fn lease_mgr(&self) -> Arc<LeaseManager> {
        self.lease_mgr.clone()
    }

    /// Get reference to the Auth store.
    pub fn auth_store(&self) -> Arc<AuthStore> {
        self.auth_store.clone()
    }

    /// Get reference to the Cluster manager.
    pub fn cluster_mgr(&self) -> Arc<ClusterManager> {
        self.cluster_mgr.clone()
    }

    /// Get the server configuration.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }
}

/// Parse initial cluster string into PeerConfig entries (excluding self).
fn parse_peer_configs(initial_cluster: &str, local_name: &str) -> Vec<PeerConfig> {
    let mut peers = Vec::new();
    for (idx, member_str) in initial_cluster.split(',').enumerate() {
        let member_str = member_str.trim();
        let parts: Vec<&str> = member_str.split('=').collect();
        if parts.len() != 2 {
            warn!(member_str = member_str, "Skipping malformed cluster entry");
            continue;
        }

        let name = parts[0].trim();
        let peer_url = parts[1].trim();

        // Skip self
        if name == local_name {
            continue;
        }

        peers.push(PeerConfig {
            id: (idx as u64) + 1,
            address: peer_url.to_string(),
        });
    }
    peers
}

/// Register initial cluster members in the ClusterManager.
fn register_initial_members(
    cluster_mgr: &ClusterManager,
    initial_cluster: &str,
    _local_name: &str,
) -> anyhow::Result<()> {
    for member_str in initial_cluster.split(',') {
        let member_str = member_str.trim();
        let parts: Vec<&str> = member_str.split('=').collect();
        if parts.len() != 2 {
            warn!(member_str = member_str, "Skipping malformed cluster entry");
            continue;
        }

        let name = parts[0].trim();
        let peer_url = parts[1].trim();

        let peer_urls = vec![peer_url.to_string()];
        let client_urls = vec![peer_url.replace(":2380", ":2379")];

        match cluster_mgr.add_member(name.to_string(), peer_urls, client_urls, false) {
            Ok(member) => {
                info!(name = name, id = member.id, "Registered cluster member");
            }
            Err(e) => {
                warn!(name = name, error = %e, "Failed to register cluster member");
            }
        }
    }
    Ok(())
}

/// Parse listen URLs into socket addresses.
fn parse_socket_addrs(urls: &[String]) -> anyhow::Result<Vec<SocketAddr>> {
    let mut addrs = Vec::new();
    for url in urls {
        // Simple parser for http://host:port format
        let url_str = if url.starts_with("http://") {
            &url[7..]
        } else if url.starts_with("https://") {
            &url[8..]
        } else {
            url.as_str()
        };

        match url_str.to_socket_addrs() {
            Ok(mut iter) => {
                if let Some(addr) = iter.next() {
                    addrs.push(addr);
                }
            }
            Err(e) => {
                warn!(url = url, error = %e, "Failed to parse listen URL");
            }
        }
    }

    Ok(addrs)
}

/// Background task that processes log entries from Raft's apply channel.
async fn process_apply_channel(
    mut apply_rx: mpsc::Receiver<crate::raft::LogEntry>,
    store: Arc<MvccStore>,
    watch_hub: Arc<WatchHub>,
) {
    while let Some(entry) = apply_rx.recv().await {
        match entry.entry_type {
            crate::raft::EntryType::Normal => {
                // Deserialize and apply the data mutation
                match serde_json::from_slice::<serde_json::Value>(&entry.data) {
                    Ok(_value) => {
                        // Application would deserialize into specific operation types
                        // and call store.put/delete_range, then dispatch to watch_hub
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to deserialize log entry");
                    }
                }
            }
            crate::raft::EntryType::ConfigChange => {
                // Handle configuration changes (membership changes)
                info!("Processing config change entry at index {}", entry.index);
            }
            crate::raft::EntryType::Snapshot => {
                // Handle snapshot entries
                info!("Processing snapshot entry at index {}", entry.index);
            }
        }
    }
}

/// Background task that processes lease expiry events.
async fn process_lease_expiries(
    mut expire_rx: mpsc::Receiver<crate::lease::LeaseExpireEvent>,
    store: Arc<MvccStore>,
    watch_hub: Arc<WatchHub>,
) {
    while let Some(event) = expire_rx.recv().await {
        info!(lease_id = event.lease_id, key_count = event.keys.len(), "Processing lease expiry");
        for key in &event.keys {
            // Compute a single-key end range: key + 1 byte to delete exactly one key.
            // Previously this used &[] which caused unbounded range delete.
            let mut end_key = key.clone();
            // Increment the last byte to form an exclusive upper bound for exactly this key
            let key_found = if let Some(last) = end_key.last_mut() {
                *last = last.wrapping_add(1);
                true
            } else {
                false
            };

            if !key_found {
                warn!(key = ?key, "Skipping empty key in lease expiry");
                continue;
            }

            match store.delete_range(key, &end_key) {
                Ok((rev, deleted_kvs)) => {
                    for deleted_kv in &deleted_kvs {
                        // Notify watchers of deleted key
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
                        let _ = watch_hub.notify(vec![delete_event], rev, 0);
                    }
                    debug!(key = ?String::from_utf8_lossy(key), revision = rev, "Deleted expired key");
                }
                Err(e) => {
                    error!(key = ?key, error = %e, "Failed to delete expired key");
                }
            }
        }
    }
}

/// Background task that runs the Raft node's event loop.
async fn raft_event_loop(raft: Arc<RaftNode>) {
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    loop {
        interval.tick().await;
        if let Err(e) = raft.tick().await {
            error!(error = %e, "Raft tick failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cluster_state_parsing() {
        assert_eq!(ClusterState::from_str("new").unwrap(), ClusterState::New);
        assert_eq!(
            ClusterState::from_str("existing").unwrap(),
            ClusterState::Existing
        );
        assert_eq!(
            ClusterState::from_str("NEW").unwrap(),
            ClusterState::New
        );
        assert!(ClusterState::from_str("invalid").is_err());
    }

    #[test]
    fn test_auto_compaction_mode_parsing() {
        assert_eq!(
            AutoCompactionMode::from_str("periodic").unwrap(),
            AutoCompactionMode::Periodic
        );
        assert_eq!(
            AutoCompactionMode::from_str("revision").unwrap(),
            AutoCompactionMode::Revision
        );
        assert_eq!(
            AutoCompactionMode::from_str("PERIODIC").unwrap(),
            AutoCompactionMode::Periodic
        );
        assert!(AutoCompactionMode::from_str("invalid").is_err());
    }

    #[test]
    fn test_parse_peer_configs() {
        let peers = parse_peer_configs(
            "node1=http://node1:2380,node2=http://node2:2380,node3=http://node3:2380",
            "node1",
        );
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].address, "http://node2:2380");
        assert_eq!(peers[1].address, "http://node3:2380");
    }

    #[test]
    fn test_parse_socket_addrs() {
        let addrs =
            parse_socket_addrs(&["http://0.0.0.0:2379".to_string()]).unwrap();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 2379);
    }
}
