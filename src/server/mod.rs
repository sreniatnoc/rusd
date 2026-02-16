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
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing::{debug, error, info, warn};

/// Generate a self-signed TLS certificate and key pair using rcgen.
/// Returns (cert_pem, key_pem) as byte vectors.
fn generate_self_signed_cert(name: &str) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let san_names = vec![
        name.to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "0.0.0.0".to_string(),
    ];

    let certified_key = rcgen::generate_simple_self_signed(san_names)
        .map_err(|e| anyhow::anyhow!("Failed to generate self-signed cert: {}", e))?;

    let cert_pem = certified_key.cert.pem();
    let key_pem = certified_key.signing_key.serialize_pem();

    Ok((cert_pem.into_bytes(), key_pem.into_bytes()))
}

/// Wrapper around a TLS stream that implements tonic's `Connected` trait.
/// Used for custom TLS configurations (e.g., CRL enforcement) that bypass
/// tonic's built-in `ServerTlsConfig`.
struct CrlTlsStream {
    inner: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
}

impl AsyncRead for CrlTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for CrlTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl tonic::transport::server::Connected for CrlTlsStream {
    type ConnectInfo = tonic::transport::server::TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        // Delegate to the underlying TcpStream's Connected impl
        self.inner.get_ref().0.connect_info()
    }
}

/// Build a TLS acceptor with CRL (Certificate Revocation List) enforcement.
/// Uses rustls's `WebPkiClientVerifier` with CRL support, bypassing tonic's
/// `ServerTlsConfig` which doesn't expose CRL configuration.
fn build_crl_tls_acceptor(
    cert_file: &str,
    key_file: &str,
    ca_file: &str,
    crl_file: &str,
) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::io::BufReader;

    // Read server certificate chain
    let cert_pem = std::fs::read(cert_file)
        .map_err(|e| anyhow::anyhow!("Failed to read cert file {}: {}", cert_file, e))?;
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_slice()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Failed to parse cert PEM: {}", e))?;

    // Read private key
    let key_pem = std::fs::read(key_file)
        .map_err(|e| anyhow::anyhow!("Failed to read key file {}: {}", key_file, e))?;
    let private_key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_slice()))
            .map_err(|e| anyhow::anyhow!("Failed to parse key PEM: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("No private key found in {}", key_file))?;

    // Read CA certificate(s) for client verification
    let ca_pem = std::fs::read(ca_file)
        .map_err(|e| anyhow::anyhow!("Failed to read CA file {}: {}", ca_file, e))?;
    let ca_certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(ca_pem.as_slice()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Failed to parse CA PEM: {}", e))?;

    let mut root_store = rustls::RootCertStore::empty();
    for ca_cert in ca_certs {
        root_store
            .add(ca_cert)
            .map_err(|e| anyhow::anyhow!("Failed to add CA cert to root store: {}", e))?;
    }

    // Read CRL(s) — supports both PEM and DER formats
    let crl_bytes = std::fs::read(crl_file)
        .map_err(|e| anyhow::anyhow!("Failed to read CRL file {}: {}", crl_file, e))?;
    let mut crls: Vec<rustls::pki_types::CertificateRevocationListDer<'static>> =
        rustls_pemfile::crls(&mut BufReader::new(crl_bytes.as_slice()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_default();
    if crls.is_empty() {
        // Not PEM — treat the whole file as a single DER-encoded CRL
        crls.push(rustls::pki_types::CertificateRevocationListDer::from(crl_bytes));
    }

    info!(
        crl_file = crl_file,
        crl_count = crls.len(),
        "Loading CRL for client certificate revocation checking"
    );

    // Build client verifier with CRL enforcement
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .with_crls(crls)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build CRL verifier: {}", e))?;

    // Build server config with CRL-aware client verification
    let server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, private_key)
        .map_err(|e| anyhow::anyhow!("Failed to build TLS config with CRL: {}", e))?;

    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(server_config)))
}

use crate::api::auth_service::AuthService;
use crate::api::cluster_service::ClusterService;
use crate::api::kv_service::KvService;
use crate::api::lease_service::LeaseService;
use crate::api::maintenance_service::MaintenanceService;
use crate::api::raft_internal_service::RaftInternalService;
use crate::api::watch_service::WatchService;
use crate::auth::AuthStore;
use crate::cluster::ClusterManager;
use crate::etcdserverpb::{
    auth_server::AuthServer, cluster_server::ClusterServer, kv_server::KvServer,
    lease_server::LeaseServer, maintenance_server::MaintenanceServer, watch_server::WatchServer,
};
use crate::lease::LeaseManager;
use crate::raft::config::{PeerConfig, RaftConfig};
use crate::raft::log::RaftLog;
use crate::raft::node::RaftNode;
use crate::raft::transport::GrpcTransport;
use crate::raftpb::raft_internal_server::RaftInternalServer;
use crate::storage::backend::{Backend, BackendConfig};
use crate::storage::mvcc::MvccStore;
use crate::watch::WatchHub;

/// Main rusd server that coordinates all subsystems.
pub struct RusdServer {
    config: ServerConfig,
    #[allow(dead_code)]
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

    /// TLS certificate file path for client connections (PEM encoded).
    pub tls_cert_file: Option<String>,

    /// TLS private key file path for client connections (PEM encoded).
    pub tls_key_file: Option<String>,

    /// Trusted CA file for client certificate verification (mTLS).
    pub tls_trusted_ca_file: Option<String>,

    /// TLS certificate file path for peer connections.
    pub peer_tls_cert_file: Option<String>,

    /// TLS private key file path for peer connections.
    pub peer_tls_key_file: Option<String>,

    /// Trusted CA file for peer certificate verification.
    pub peer_tls_trusted_ca_file: Option<String>,

    /// Enable auto-TLS for client connections (generate self-signed cert).
    pub auto_tls: bool,

    /// Enable auto-TLS for peer connections (generate self-signed cert).
    pub peer_auto_tls: bool,

    /// Path to client CRL file for certificate revocation checking.
    pub client_crl_file: Option<String>,
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
            tls_cert_file: None,
            tls_key_file: None,
            tls_trusted_ca_file: None,
            peer_tls_cert_file: None,
            peer_tls_key_file: None,
            peer_tls_trusted_ca_file: None,
            auto_tls: false,
            peer_auto_tls: false,
            client_crl_file: None,
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

        // Generate deterministic member and cluster IDs matching etcd v3.5.x's algorithm.
        // member_id = SHA-1(sorted_peer_urls + cluster_token) → first 8 bytes big-endian u64
        // cluster_id = SHA-1(sorted_member_ids) → first 8 bytes big-endian u64
        let local_peer_urls =
            extract_member_peer_urls(&config.initial_cluster, &config.name);
        let member_id = compute_member_id(&local_peer_urls, &config.initial_cluster_token);

        // Compute all member IDs for the initial cluster to derive cluster_id
        let all_member_ids: Vec<u64> = config
            .initial_cluster
            .split(',')
            .filter_map(|entry| {
                let parts: Vec<&str> = entry.trim().split('=').collect();
                if parts.len() == 2 {
                    let peer_urls = vec![parts[1].trim().to_string()];
                    Some(compute_member_id(&peer_urls, &config.initial_cluster_token))
                } else {
                    None
                }
            })
            .collect();
        let cluster_id = compute_cluster_id(&all_member_ids);

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
        let peers = parse_peer_configs_with_token(
            &config.initial_cluster,
            &config.name,
            &config.initial_cluster_token,
        );
        for peer in &peers {
            peer_addresses.insert(peer.id, peer.address.clone());
        }
        // For peer auto-TLS, rewrite https:// peer addresses to http:// for outgoing connections.
        // tonic/rustls doesn't support InsecureSkipVerify (which Go's crypto/tls uses for auto-TLS),
        // so we use plaintext for peer transport and TLS only on the listening side.
        // The server still listens with auto-TLS for inbound connections from etcdctl.
        if config.peer_auto_tls {
            for addr in peer_addresses.values_mut() {
                if addr.starts_with("https://") {
                    *addr = addr.replace("https://", "http://");
                    info!("Peer auto-TLS: rewrote peer address to plaintext: {}", addr);
                }
            }
        }

        // Create transport with optional TLS
        let transport = if config.peer_tls_cert_file.is_some() || config.tls_cert_file.is_some() {
            let cert_file = config
                .peer_tls_cert_file
                .as_deref()
                .or(config.tls_cert_file.as_deref());
            let ca_file = config
                .peer_tls_trusted_ca_file
                .as_deref()
                .or(config.tls_trusted_ca_file.as_deref());
            let mut tls = tonic::transport::ClientTlsConfig::new();
            if let Some(ca_path) = ca_file {
                let ca_pem = std::fs::read(ca_path)
                    .map_err(|e| anyhow::anyhow!("Failed to read peer CA {}: {}", ca_path, e))?;
                tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(&ca_pem));
            }
            if let Some(cert_path) = cert_file {
                let key_file = config
                    .peer_tls_key_file
                    .as_deref()
                    .or(config.tls_key_file.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("Peer cert requires peer key"))?;
                let cert_pem = std::fs::read(cert_path).map_err(|e| {
                    anyhow::anyhow!("Failed to read peer cert {}: {}", cert_path, e)
                })?;
                let key_pem = std::fs::read(key_file)
                    .map_err(|e| anyhow::anyhow!("Failed to read peer key {}: {}", key_file, e))?;
                tls = tls.identity(tonic::transport::Identity::from_pem(&cert_pem, &key_pem));
            }
            Arc::new(GrpcTransport::with_tls(peer_addresses, tls))
        } else {
            Arc::new(GrpcTransport::new(peer_addresses))
        };
        let (apply_tx, apply_rx) = mpsc::channel(10000);

        // Parse peers from initial_cluster config
        let peers = parse_peer_configs_with_token(
            &config.initial_cluster,
            &config.name,
            &config.initial_cluster_token,
        );

        let raft_config = RaftConfig {
            id: member_id,
            cluster_id,
            election_timeout_min: Duration::from_millis(config.election_timeout_ms),
            election_timeout_max: Duration::from_millis(config.election_timeout_ms * 2),
            heartbeat_interval: Duration::from_millis(config.heartbeat_interval_ms),
            max_log_entries_per_request: 100,
            snapshot_threshold: config.snapshot_count,
            peers,
            data_dir: config.data_dir.clone(),
            pre_vote: true,
        };
        // Create snapshot callbacks for the Raft node
        let store_for_create = store.clone();
        let create_snapshot_fn: crate::raft::node::SnapshotCreateFn =
            Arc::new(move || store_for_create.create_snapshot().unwrap_or_default());
        let store_for_restore = store.clone();
        let restore_snapshot_fn: crate::raft::node::SnapshotRestoreFn =
            Arc::new(move |data: &[u8]| {
                store_for_restore
                    .restore_snapshot(data)
                    .map_err(|e| e.to_string())
            });

        let raft = Arc::new(
            RaftNode::with_snapshot_callbacks(
                raft_config,
                raft_log,
                transport,
                apply_tx,
                Some(create_snapshot_fn),
                Some(restore_snapshot_fn),
            )
            .map_err(|e| anyhow::anyhow!("Failed to initialize Raft node: {}", e))?,
        );
        info!(member_id = member_id, "Raft node initialized");

        // 4. Initialize Cluster manager
        let cluster_mgr = ClusterManager::new(cluster_id, member_id)
            .map_err(|e| anyhow::anyhow!("Failed to initialize cluster manager: {}", e))?;
        register_initial_members(&cluster_mgr, &config.initial_cluster, &config.initial_cluster_token)?;
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
        let raft_clone = server.raft.clone();
        let apply_handle = tokio::spawn(async move {
            process_apply_channel(apply_rx, store_clone, watch_hub_clone, raft_clone).await;
        });
        server.background_tasks.push(apply_handle);

        // Start lease expiry background task
        let _lease_mgr_clone = server.lease_mgr.clone();
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
    pub async fn run(
        mut self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> anyhow::Result<()> {
        // Start Raft node's event loop (now Send-safe with std::sync::Mutex)
        let raft_handle = self.raft.clone().run();
        self.background_tasks.push(raft_handle);

        // Start peer RPC server on peer URLs (for Raft internal communication)
        let peer_urls = parse_socket_addrs(&self.config.listen_peer_urls)?;
        if !peer_urls.is_empty() {
            let peer_addr = peer_urls[0];
            let raft_internal_service = RaftInternalService::new(self.raft.clone());

            // Configure peer TLS: for peer-auto-tls, skip TLS on the server side too
            // (tonic/rustls doesn't support InsecureSkipVerify, so we use plaintext peer transport).
            // For explicit certs, use the provided cert/key/ca files.
            let peer_tls = if self.config.peer_auto_tls {
                info!("Peer auto-TLS: using plaintext for peer transport (rustls doesn't support InsecureSkipVerify)");
                None
            } else {
                load_tls_config(
                    self.config
                        .peer_tls_cert_file
                        .as_deref()
                        .or(self.config.tls_cert_file.as_deref()),
                    self.config
                        .peer_tls_key_file
                        .as_deref()
                        .or(self.config.tls_key_file.as_deref()),
                    self.config
                        .peer_tls_trusted_ca_file
                        .as_deref()
                        .or(self.config.tls_trusted_ca_file.as_deref()),
                )?
            };

            let mut peer_builder = Server::builder();
            if let Some(tls) = peer_tls {
                peer_builder = peer_builder.tls_config(tls)?;
                info!("Peer RPC server TLS enabled");
            }

            let peer_server = peer_builder
                .add_service(RaftInternalServer::new(raft_internal_service))
                .serve(peer_addr);

            let peer_handle = tokio::spawn(async move {
                info!("Peer RPC server listening on {}", peer_addr);
                if let Err(e) = peer_server.await {
                    error!("Peer RPC server error: {}", e);
                }
            });
            self.background_tasks.push(peer_handle);
        }

        // Parse client URLs and start gRPC server
        let client_urls = parse_socket_addrs(&self.config.listen_client_urls)?;
        if client_urls.is_empty() {
            return Err(anyhow::anyhow!("No valid client URLs to listen on"));
        }

        // We use the first URL as the primary listener
        let addr = client_urls[0];

        info!(addr = %addr, "Starting gRPC server");

        // Build peer client URL map for leader forwarding in multi-node clusters.
        // Maps member_id → client_url for every node (including self).
        let peer_client_urls = build_peer_client_urls(
            &self.config.initial_cluster,
            &self.config.initial_cluster_token,
            &self.config.listen_client_urls,
            &self.config.name,
        );

        // Create service instances matching their actual constructors
        let kv_service = KvService::with_peer_urls(
            self.store.clone(),
            self.raft.clone(),
            self.watch_hub.clone(),
            peer_client_urls,
        );
        let watch_service = WatchService::new(
            self.store.clone(),
            self.raft.clone(),
            self.watch_hub.clone(),
        );

        // LeaseService needs the ApiLeaseManager bridge (raft + core lease manager + store + watch_hub)
        let api_lease_mgr = Arc::new(crate::api::lease_service::ApiLeaseManager::new(
            self.raft.clone(),
            self.lease_mgr.clone(),
            self.store.clone(),
            self.watch_hub.clone(),
        ));
        let lease_service = LeaseService::new(api_lease_mgr);

        // ClusterService uses the ClusterManager for proper membership tracking
        let cluster_service = ClusterService::new(self.raft.clone(), self.cluster_mgr.clone());

        let maintenance_service =
            MaintenanceService::new(self.store.clone(), self.backend.clone(), self.raft.clone());

        // AuthService needs the AuthManager wrapper
        let auth_mgr_for_service = Arc::new(crate::api::auth_service::AuthManager::new(
            self.raft.clone(),
        ));
        let auth_service = AuthService::new(auth_mgr_for_service);

        // Check if CRL enforcement is needed (requires cert + key + CA + CRL files)
        let use_crl = self.config.client_crl_file.is_some()
            && self.config.tls_cert_file.is_some()
            && self.config.tls_key_file.is_some()
            && self.config.tls_trusted_ca_file.is_some();

        // Configure client TLS: auto-TLS generates self-signed certs, otherwise use provided files.
        // CRL path uses a custom rustls config (tonic's ServerTlsConfig doesn't support CRL).
        let client_tls = if use_crl {
            // CRL path: skip tonic's TLS, we'll use custom TLS acceptor below
            None
        } else if self.config.auto_tls && self.config.tls_cert_file.is_none() {
            let (cert_pem, key_pem) = generate_self_signed_cert(&self.config.name)
                .map_err(|e| anyhow::anyhow!("Failed to generate auto-TLS cert: {}", e))?;
            info!("Auto-TLS: generated self-signed certificate for client connections");
            let identity = Identity::from_pem(&cert_pem, &key_pem);
            Some(ServerTlsConfig::new().identity(identity))
        } else {
            load_tls_config(
                self.config.tls_cert_file.as_deref(),
                self.config.tls_key_file.as_deref(),
                self.config.tls_trusted_ca_file.as_deref(),
            )?
        };

        // Build gRPC server with all services and HTTP/2 settings for K8s compat
        let mut builder = Server::builder();
        if let Some(tls) = client_tls {
            builder = builder.tls_config(tls)?;
            info!("Client gRPC server TLS enabled");
        }
        let router = builder
            .http2_keepalive_interval(Some(Duration::from_secs(10)))
            .http2_keepalive_timeout(Some(Duration::from_secs(20)))
            .initial_connection_window_size(Some(1024 * 1024)) // 1MB
            .initial_stream_window_size(Some(1024 * 1024)) // 1MB
            .add_service(KvServer::new(kv_service))
            .add_service(WatchServer::new(watch_service))
            .add_service(LeaseServer::new(lease_service))
            .add_service(ClusterServer::new(cluster_service))
            .add_service(MaintenanceServer::new(maintenance_service))
            .add_service(AuthServer::new(auth_service));

        info!("rusd server listening on {}", addr);
        info!("Data directory: {}", self.config.data_dir.display());
        info!("Member name: {}", self.config.name);
        info!(
            "Advertise client URLs: {}",
            self.config.advertise_client_urls.join(", ")
        );

        // Print readiness line (etcd e2e framework blocks on this).
        // For multi-node clusters, wait for leader election first.
        let is_multi_node = self.config.initial_cluster.matches('=').count() > 1;
        if is_multi_node {
            let raft_for_ready = self.raft.clone();
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                let mut interval = tokio::time::interval(Duration::from_millis(100));
                loop {
                    interval.tick().await;
                    if raft_for_ready.has_leader() {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        warn!("Timed out waiting for leader election, printing ready anyway");
                        break;
                    }
                }
                println!("ready to serve client requests");
            });
        } else {
            println!("ready to serve client requests");
        }

        // Run server — CRL path uses custom TLS acceptor with serve_with_incoming_shutdown
        if use_crl {
            let tls_acceptor = build_crl_tls_acceptor(
                self.config.tls_cert_file.as_deref().unwrap(),
                self.config.tls_key_file.as_deref().unwrap(),
                self.config.tls_trusted_ca_file.as_deref().unwrap(),
                self.config.client_crl_file.as_deref().unwrap(),
            )?;
            info!("Client gRPC server TLS enabled with CRL enforcement");

            // Create a channel-based incoming stream:
            // - A background task accepts TCP connections and performs TLS handshake
            // - Successfully handshaked connections are sent to tonic via the channel
            // - Revoked client certs fail the TLS handshake (never reach tonic)
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<CrlTlsStream, std::io::Error>>(128);
            let tcp_listener = tokio::net::TcpListener::bind(addr).await?;

            tokio::spawn(async move {
                loop {
                    match tcp_listener.accept().await {
                        Ok((tcp, peer_addr)) => {
                            let acceptor = tls_acceptor.clone();
                            let tx = tx.clone();
                            tokio::spawn(async move {
                                match acceptor.accept(tcp).await {
                                    Ok(tls) => {
                                        let _ = tx.send(Ok(CrlTlsStream { inner: tls })).await;
                                    }
                                    Err(e) => {
                                        // TLS handshake failed — expected for revoked certs
                                        debug!(
                                            peer = %peer_addr,
                                            error = %e,
                                            "TLS handshake rejected (CRL check or invalid cert)"
                                        );
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            error!("TCP accept error: {}", e);
                        }
                    }
                }
            });

            let incoming = tokio_stream::wrappers::ReceiverStream::new(rx);
            router.serve_with_incoming_shutdown(incoming, shutdown).await?;
        } else {
            router.serve_with_shutdown(addr, shutdown).await?;
        }

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

/// Compute a deterministic member ID from peer URLs and cluster token.
/// Matches etcd v3.5.x's algorithm exactly:
///   SHA-1(sorted_peer_urls_concatenated + cluster_token)
///   → first 8 bytes as big-endian u64
fn compute_member_id(peer_urls: &[String], cluster_token: &str) -> u64 {
    use sha1::{Digest, Sha1};
    let mut sorted_urls = peer_urls.to_vec();
    sorted_urls.sort();
    let mut data = Vec::new();
    for url in &sorted_urls {
        data.extend_from_slice(url.as_bytes());
    }
    data.extend_from_slice(cluster_token.as_bytes());
    let hash = Sha1::digest(&data);
    u64::from_be_bytes(hash[..8].try_into().unwrap())
}

/// Compute a deterministic cluster ID from all member IDs.
/// Matches etcd v3.5.x's algorithm exactly:
///   SHA-1(sorted_member_ids as 8-byte big-endian each)
///   → first 8 bytes as big-endian u64
fn compute_cluster_id(member_ids: &[u64]) -> u64 {
    use sha1::{Digest, Sha1};
    let mut sorted = member_ids.to_vec();
    sorted.sort();
    let mut data = Vec::with_capacity(sorted.len() * 8);
    for id in &sorted {
        data.extend_from_slice(&id.to_be_bytes());
    }
    let hash = Sha1::digest(&data);
    u64::from_be_bytes(hash[..8].try_into().unwrap())
}

/// Extract peer URLs for a specific member from the initial cluster string.
/// Returns the peer URLs for the named member.
fn extract_member_peer_urls(initial_cluster: &str, member_name: &str) -> Vec<String> {
    for member_str in initial_cluster.split(',') {
        let member_str = member_str.trim();
        let parts: Vec<&str> = member_str.split('=').collect();
        if parts.len() == 2 && parts[0].trim() == member_name {
            return vec![parts[1].trim().to_string()];
        }
    }
    Vec::new()
}

/// Parse initial cluster string into PeerConfig entries (excluding self).
/// Uses deterministic hash-based IDs matching each node's member_id.
#[allow(dead_code)]
fn parse_peer_configs(initial_cluster: &str, local_name: &str) -> Vec<PeerConfig> {
    parse_peer_configs_with_token(initial_cluster, local_name, "rusd-cluster")
}

/// Parse initial cluster string with a specific cluster token for ID computation.
fn parse_peer_configs_with_token(
    initial_cluster: &str,
    local_name: &str,
    cluster_token: &str,
) -> Vec<PeerConfig> {
    let mut peers = Vec::new();
    for member_str in initial_cluster.split(',') {
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

        let peer_urls = vec![peer_url.to_string()];
        peers.push(PeerConfig {
            id: compute_member_id(&peer_urls, cluster_token),
            address: peer_url.to_string(),
        });
    }
    peers
}

/// Register initial cluster members in the ClusterManager with deterministic IDs.
/// Uses the same SHA-1 algorithm as etcd v3.5.x to compute member IDs.
fn register_initial_members(
    cluster_mgr: &ClusterManager,
    initial_cluster: &str,
    cluster_token: &str,
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
        let member_id = compute_member_id(&peer_urls, cluster_token);

        match cluster_mgr.add_member_with_id(
            member_id,
            name.to_string(),
            peer_urls,
            client_urls,
            false,
        ) {
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

/// Load TLS configuration from cert/key files.
/// Returns None if cert_file is not provided (TLS disabled).
fn load_tls_config(
    cert_file: Option<&str>,
    key_file: Option<&str>,
    ca_file: Option<&str>,
) -> anyhow::Result<Option<ServerTlsConfig>> {
    let (cert_path, key_path) = match (cert_file, key_file) {
        (Some(c), Some(k)) => (c, k),
        (Some(_), None) => return Err(anyhow::anyhow!("--cert-file requires --key-file")),
        (None, Some(_)) => return Err(anyhow::anyhow!("--key-file requires --cert-file")),
        (None, None) => return Ok(None),
    };

    let cert_pem = std::fs::read(cert_path)
        .map_err(|e| anyhow::anyhow!("Failed to read cert file {}: {}", cert_path, e))?;
    let key_pem = std::fs::read(key_path)
        .map_err(|e| anyhow::anyhow!("Failed to read key file {}: {}", key_path, e))?;

    let identity = Identity::from_pem(&cert_pem, &key_pem);
    let mut tls_config = ServerTlsConfig::new().identity(identity);

    if let Some(ca_path) = ca_file {
        let ca_pem = std::fs::read(ca_path)
            .map_err(|e| anyhow::anyhow!("Failed to read CA file {}: {}", ca_path, e))?;
        let ca_cert = tonic::transport::Certificate::from_pem(&ca_pem);
        tls_config = tls_config.client_ca_root(ca_cert);
        info!(
            "mTLS enabled: client certificates will be verified against {}",
            ca_path
        );
    }

    info!("TLS configured with cert={}, key={}", cert_path, key_path);
    Ok(Some(tls_config))
}

/// Background task that processes log entries from Raft's apply channel.
/// On followers, this applies replicated mutations to the local store.
/// On the leader, mutations are skipped (leader applies in the KV service after commit-wait).
async fn process_apply_channel(
    mut apply_rx: mpsc::Receiver<crate::raft::LogEntry>,
    store: Arc<MvccStore>,
    watch_hub: Arc<WatchHub>,
    raft: Arc<RaftNode>,
) {
    while let Some(entry) = apply_rx.recv().await {
        match entry.entry_type {
            crate::raft::EntryType::Normal => {
                // Leader applies in KV service write path; skip here to avoid double-apply
                if raft.is_leader() {
                    continue;
                }

                let data = String::from_utf8_lossy(&entry.data);

                if let Some(rest) = data.strip_prefix("PUT:") {
                    // Format: PUT:key:value
                    if let Some((key, value)) = rest.split_once(':') {
                        match store.put(key.as_bytes(), value.as_bytes(), 0) {
                            Ok((rev, kv, old_kv)) => {
                                let watch_prev = old_kv.as_ref().map(|k| crate::watch::KeyValue {
                                    key: k.key.clone(),
                                    create_revision: k.create_revision,
                                    mod_revision: k.mod_revision,
                                    version: k.version,
                                    value: k.value.clone(),
                                    lease: k.lease,
                                });
                                let put_event = crate::watch::Event {
                                    event_type: crate::watch::EventType::Put,
                                    kv: crate::watch::KeyValue {
                                        key: kv.key.clone(),
                                        create_revision: kv.create_revision,
                                        mod_revision: kv.mod_revision,
                                        version: kv.version,
                                        value: kv.value.clone(),
                                        lease: kv.lease,
                                    },
                                    prev_kv: watch_prev,
                                };
                                let _ = watch_hub.notify(vec![put_event], rev, 0);
                                debug!(key = key, revision = rev, "Follower applied PUT");
                            }
                            Err(e) => {
                                error!(key = key, error = %e, "Follower failed to apply PUT");
                            }
                        }
                    }
                } else if let Some(rest) = data.strip_prefix("DELETE:") {
                    // Format: DELETE:key:range_end
                    if let Some((key, range_end)) = rest.split_once(':') {
                        let effective_range_end = if range_end.is_empty() {
                            let mut end = key.as_bytes().to_vec();
                            end.push(0);
                            end
                        } else {
                            range_end.as_bytes().to_vec()
                        };
                        match store.delete_range(key.as_bytes(), &effective_range_end) {
                            Ok((rev, deleted_kvs)) => {
                                if !deleted_kvs.is_empty() {
                                    let delete_events: Vec<crate::watch::Event> = deleted_kvs
                                        .iter()
                                        .map(|kv| crate::watch::Event {
                                            event_type: crate::watch::EventType::Delete,
                                            kv: crate::watch::KeyValue {
                                                key: kv.key.clone(),
                                                create_revision: kv.create_revision,
                                                mod_revision: rev,
                                                version: kv.version,
                                                value: Vec::new(),
                                                lease: 0,
                                            },
                                            prev_kv: Some(crate::watch::KeyValue {
                                                key: kv.key.clone(),
                                                create_revision: kv.create_revision,
                                                mod_revision: kv.mod_revision,
                                                version: kv.version,
                                                value: kv.value.clone(),
                                                lease: kv.lease,
                                            }),
                                        })
                                        .collect();
                                    let _ = watch_hub.notify(delete_events, rev, 0);
                                }
                                debug!(key = key, revision = rev, "Follower applied DELETE");
                            }
                            Err(e) => {
                                error!(key = key, error = %e, "Follower failed to apply DELETE");
                            }
                        }
                    }
                }
            }
            crate::raft::EntryType::ConfigChange => {
                let data = String::from_utf8_lossy(&entry.data);
                info!(
                    "Processing config change at index {}: {}",
                    entry.index, data
                );
                // Config changes are applied eagerly by the ClusterService
                // on the leader. Followers just log them for now.
            }
            crate::raft::EntryType::Snapshot => {
                info!("Processing snapshot entry at index {}", entry.index);
                // Snapshot entries are handled by handle_install_snapshot
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
        info!(
            lease_id = event.lease_id,
            key_count = event.keys.len(),
            "Processing lease expiry"
        );
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
#[allow(dead_code)]
async fn raft_event_loop(raft: Arc<RaftNode>) {
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    loop {
        interval.tick().await;
        if let Err(e) = raft.tick().await {
            error!(error = %e, "Raft tick failed");
        }
    }
}

/// Build a mapping of member_id → client_url for all nodes in the cluster.
/// Used for leader forwarding in multi-node deployments.
///
/// For each node in initial_cluster, compute the member_id from name + cluster_token,
/// then derive the client URL from the peer URL (peer_port - 1).
/// For the local node, use its actual listen_client_urls.
fn build_peer_client_urls(
    initial_cluster: &str,
    cluster_token: &str,
    local_client_urls: &[String],
    local_name: &str,
) -> std::collections::HashMap<u64, String> {
    let mut map = std::collections::HashMap::new();

    for member_str in initial_cluster.split(',') {
        let member_str = member_str.trim();
        let parts: Vec<&str> = member_str.split('=').collect();
        if parts.len() != 2 {
            continue;
        }
        let name = parts[0].trim();
        let peer_url = parts[1].trim();
        let peer_urls = vec![peer_url.to_string()];
        let member_id = compute_member_id(&peer_urls, cluster_token);

        if name == local_name {
            // Use local client URL
            if let Some(url) = local_client_urls.first() {
                map.insert(member_id, url.clone());
            }
        } else {
            // Derive client URL from peer URL: peer_port - 1
            // e.g., https://localhost:20001 → http://localhost:20000
            let client_url = derive_client_url_from_peer(peer_url);
            map.insert(member_id, client_url);
        }
    }
    map
}

/// Derive a client URL from a peer URL by subtracting 1 from the port.
/// etcd convention: client_port = peer_port - 1.
fn derive_client_url_from_peer(peer_url: &str) -> String {
    // Parse the URL to extract host and port
    let url_without_scheme = if peer_url.starts_with("https://") {
        &peer_url[8..]
    } else if peer_url.starts_with("http://") {
        &peer_url[7..]
    } else {
        peer_url
    };

    if let Some(colon_pos) = url_without_scheme.rfind(':') {
        let host = &url_without_scheme[..colon_pos];
        if let Ok(port) = url_without_scheme[colon_pos + 1..].parse::<u16>() {
            let client_port = port.saturating_sub(1);
            return format!("http://{}:{}", host, client_port);
        }
    }

    // Fallback: replace :2380 with :2379
    peer_url
        .replace(":2380", ":2379")
        .replace("https://", "http://")
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
        assert_eq!(ClusterState::from_str("NEW").unwrap(), ClusterState::New);
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
        let addrs = parse_socket_addrs(&["http://0.0.0.0:2379".to_string()]).unwrap();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 2379);
    }
}
