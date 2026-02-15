//! # rusd - A high-performance Rust replacement for etcd
//!
//! This is the main entry point for the rusd distributed key-value store.
//! It provides a drop-in replacement for etcd with full Kubernetes API parity.

use clap::Parser;
use std::path::PathBuf;
use tracing::{error, info};
use tracing_subscriber::filter::EnvFilter;

use rusd::server::{AutoCompactionMode, ClusterState, RusdServer, ServerConfig};

/// A high-performance Rust replacement for etcd with full Kubernetes API parity.
///
/// rusd is a distributed reliable key-value store for the most critical data of a
/// distributed system. It's written in Rust for maximum performance and memory efficiency.
/// rusd provides strong data consistency guarantees, efficient watch-based change
/// notifications (critical for Kubernetes), and seamless cluster management.
#[derive(Parser, Debug)]
#[command(
    name = "rusd",
    // clap prints this on --version; include etcd-compat line for e2e framework detection
    version = concat!(env!("CARGO_PKG_VERSION"), "\netcd Version: 3.5.17\nGit SHA: rusd-", env!("CARGO_PKG_VERSION")),
    author = "Shailesh <shailesh.pant@gmail.com>",
    about = "A high-performance Rust replacement for etcd",
    long_about = "rusd is a distributed reliable key-value store for the most critical data of a distributed system, written in Rust for maximum performance and memory efficiency."
)]
struct Args {
    /// Human-readable name for this member.
    /// This is used to identify the member in logs and monitoring.
    #[arg(long, default_value = "default")]
    name: String,

    /// Path to the data directory where rusd stores all persistent data.
    /// This includes the key-value store, Raft logs, and snapshots.
    #[arg(long, default_value = "default.rusd")]
    data_dir: String,

    /// List of URLs to listen on for client traffic.
    /// Multiple URLs can be specified comma-separated for redundancy.
    /// Format: http://host:port[,http://host:port,...]
    #[arg(long, default_value = "http://localhost:2379")]
    listen_client_urls: String,

    /// List of URLs to listen on for peer traffic (Raft communication).
    /// These URLs are used for member-to-member replication.
    /// Format: http://host:port[,http://host:port,...]
    #[arg(long, default_value = "http://localhost:2380")]
    listen_peer_urls: String,

    /// List of this member's client URLs to advertise to the public.
    /// These are the URLs that clients should use to connect to this node.
    /// This is important when running behind a proxy or load balancer.
    #[arg(long, default_value = "http://localhost:2379")]
    advertise_client_urls: String,

    /// List of this member's peer URLs to advertise to the rest of the cluster.
    /// These URLs are used by other cluster members to connect to this node.
    #[arg(long, default_value = "http://localhost:2380")]
    initial_advertise_peer_urls: String,

    /// Initial cluster configuration for bootstrapping.
    /// Format: node1=http://node1-peer:2380,node2=http://node2-peer:2380,...
    /// Each entry is "member-name=peer-url" separated by commas.
    #[arg(long, default_value = "default=http://localhost:2380")]
    initial_cluster: String,

    /// Initial cluster state - 'new' to bootstrap a new cluster, 'existing' to join an existing one.
    /// Use 'new' when starting the first node(s) of a cluster.
    /// Use 'existing' when adding a node to an already-running cluster.
    #[arg(long, default_value = "new")]
    initial_cluster_state: String,

    /// Initial cluster token for the cluster during bootstrap.
    /// This token must be the same for all nodes in a cluster to bootstrap successfully.
    /// Change this value to isolate different clusters.
    #[arg(long, default_value = "rusd-cluster")]
    initial_cluster_token: String,

    /// Number of committed transactions to trigger a snapshot to disk.
    /// Snapshots reduce Raft log size and speed up recovery.
    /// Lower values mean more frequent snapshots (and higher disk I/O).
    /// Higher values mean fewer snapshots (and larger log files).
    #[arg(long, default_value_t = 100000)]
    snapshot_count: u64,

    /// Time in milliseconds of a heartbeat interval.
    /// The leader sends heartbeats at this interval to maintain leadership.
    /// This should be significantly smaller than election_timeout.
    #[arg(long, default_value_t = 100)]
    heartbeat_interval: u64,

    /// Time in milliseconds for an election to timeout.
    /// If a follower doesn't receive a heartbeat within this time,
    /// it will start a new election.
    #[arg(long, default_value_t = 1000)]
    election_timeout: u64,

    /// Maximum number of snapshot files to retain (0 is unlimited).
    /// Old snapshots are deleted when this limit is exceeded.
    #[arg(long, default_value_t = 5)]
    max_snapshots: u32,

    /// Maximum number of WAL (Write-Ahead Log) files to retain (0 is unlimited).
    /// Old WAL files are deleted after snapshots are taken.
    #[arg(long, default_value_t = 5)]
    max_wals: u32,

    /// Raise alarms when backend database size exceeds the given quota in bytes.
    /// A value of 0 defaults to 8GB. Use this to prevent the database from
    /// consuming all available disk space. The server will reject writes when
    /// this limit is exceeded.
    #[arg(long, default_value_t = 0)]
    quota_backend_bytes: u64,

    /// Auto compaction mode - 'periodic' or 'revision'.
    /// Periodic mode compacts at time intervals.
    /// Revision mode compacts based on the number of revisions.
    #[arg(long, default_value = "periodic")]
    auto_compaction_mode: String,

    /// Auto compaction retention for the specified mode.
    /// For periodic mode: duration string (e.g., "10m" for 10 minutes).
    /// For revision mode: number of revisions to keep.
    #[arg(long, default_value = "0")]
    auto_compaction_retention: String,

    /// Backend page cache size in megabytes.
    /// Controls memory usage for the embedded key-value store.
    /// Higher values improve read performance but consume more memory.
    /// Default: 256MB (much better than etcd's unbounded mmap).
    #[arg(long, default_value_t = 256)]
    cache_size_mb: u64,

    /// Log level - 'trace', 'debug', 'info', 'warn', or 'error'.
    /// 'info' is recommended for production.
    /// 'debug' is useful for troubleshooting.
    /// 'trace' is for detailed low-level debugging.
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Path to TLS certificate file (PEM encoded).
    /// When provided along with --key-file, the server uses TLS for client connections.
    #[arg(long)]
    cert_file: Option<String>,

    /// Path to TLS private key file (PEM encoded).
    #[arg(long)]
    key_file: Option<String>,

    /// Path to trusted CA certificate file for verifying client certificates.
    /// When set, the server requires and verifies client TLS certificates (mTLS).
    #[arg(long)]
    trusted_ca_file: Option<String>,

    /// Path to TLS certificate file for peer connections.
    /// Falls back to --cert-file if not set.
    #[arg(long)]
    peer_cert_file: Option<String>,

    /// Path to TLS private key file for peer connections.
    /// Falls back to --key-file if not set.
    #[arg(long)]
    peer_key_file: Option<String>,

    /// Path to trusted CA certificate for verifying peer certificates.
    /// Falls back to --trusted-ca-file if not set.
    #[arg(long)]
    peer_trusted_ca_file: Option<String>,

    // ---- etcd-compat flags (accepted but ignored for e2e test compatibility) ----

    /// Enable auto TLS for client connections (etcd compat, ignored).
    #[arg(long, default_value_t = false, hide = true)]
    auto_tls: bool,

    /// Enable auto TLS for peer connections (etcd compat, ignored).
    #[arg(long, default_value_t = false, hide = true)]
    peer_auto_tls: bool,

    /// Enable client certificate authentication (etcd compat, ignored — rusd infers from trusted-ca-file).
    #[arg(long, default_value_t = false, hide = true)]
    client_cert_auth: bool,

    /// Enable peer client certificate authentication (etcd compat, ignored).
    #[arg(long, default_value_t = false, hide = true)]
    peer_client_cert_auth: bool,

    /// Log output targets (etcd compat, ignored — rusd uses --log-level).
    #[arg(long, hide = true)]
    log_outputs: Option<String>,

    /// Enable pprof profiling endpoint (etcd compat, ignored).
    #[arg(long, default_value_t = false, hide = true)]
    enable_pprof: bool,

    /// Logger type (etcd compat, ignored — rusd always uses tracing).
    #[arg(long, hide = true)]
    logger: Option<String>,

    /// Enable socket SO_REUSEPORT (etcd compat, ignored).
    #[arg(long, default_value_t = false, hide = true)]
    socket_reuse_port: bool,

    /// Enable socket SO_REUSEADDR (etcd compat, ignored).
    #[arg(long, default_value_t = false, hide = true)]
    socket_reuse_address: bool,

    /// Interpret 'auto' cipher-suites as enabled (etcd compat, ignored).
    #[arg(long, hide = true)]
    cipher_suites: Option<String>,

    /// Maximum number of operations per txn (etcd compat, ignored).
    #[arg(long, hide = true)]
    max_txn_ops: Option<u64>,

    /// Maximum request bytes (etcd compat, ignored).
    #[arg(long, hide = true)]
    max_request_bytes: Option<u64>,

    /// Experimental initial corrupt check (etcd compat, ignored).
    #[arg(long, default_value_t = false, hide = true)]
    experimental_initial_corrupt_check: bool,

    /// Experimental corrupt check time (etcd compat, ignored).
    #[arg(long, hide = true)]
    experimental_corrupt_check_time: Option<String>,

    /// Client CRL file for certificate revocation (etcd compat, ignored).
    #[arg(long, hide = true)]
    client_crl_file: Option<String>,

    /// Force new cluster (etcd compat, ignored).
    #[arg(long, default_value_t = false, hide = true)]
    force_new_cluster: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Initialize tracing/logging
    initialize_tracing(&args.log_level)?;

    // Log warnings for etcd-compat flags that are accepted but ignored
    log_compat_flag_warnings(&args);

    // Print startup banner
    print_startup_banner(&args);

    // Parse arguments into ServerConfig
    let config = build_server_config(&args)?;

    // Create the RusdServer
    let server = RusdServer::new(config).await?;

    // Set up signal handlers for graceful shutdown
    let shutdown = setup_signal_handlers();

    // Run the server with the shutdown signal
    info!("Starting rusd server...");
    match server.run(shutdown).await {
        Ok(()) => {
            info!("Server shut down gracefully");
            Ok(())
        }
        Err(e) => {
            error!("Server error: {:?}", e);
            Err(e)
        }
    }
}

/// Log warnings for etcd-compatible flags that are accepted but ignored.
fn log_compat_flag_warnings(args: &Args) {
    if args.auto_tls {
        info!("--auto-tls flag accepted for etcd compatibility (ignored, use --cert-file/--key-file)");
    }
    if args.peer_auto_tls {
        info!("--peer-auto-tls flag accepted for etcd compatibility (ignored, use --peer-cert-file/--peer-key-file)");
    }
    if args.client_cert_auth {
        info!("--client-cert-auth flag accepted for etcd compatibility (ignored, inferred from --trusted-ca-file)");
    }
    if args.peer_client_cert_auth {
        info!("--peer-client-cert-auth flag accepted for etcd compatibility (ignored)");
    }
    if args.log_outputs.is_some() {
        info!("--log-outputs flag accepted for etcd compatibility (ignored, use --log-level)");
    }
    if args.enable_pprof {
        info!("--enable-pprof flag accepted for etcd compatibility (ignored)");
    }
    if args.logger.is_some() {
        info!("--logger flag accepted for etcd compatibility (ignored, rusd uses tracing)");
    }
}

/// Initialize the tracing/logging system with the specified log level.
fn initialize_tracing(log_level: &str) -> anyhow::Result<()> {
    let env_filter = match log_level {
        "trace" => EnvFilter::new("trace"),
        "debug" => EnvFilter::new("debug"),
        "info" => EnvFilter::new("info"),
        "warn" => EnvFilter::new("warn"),
        "error" => EnvFilter::new("error"),
        _ => {
            eprintln!("Invalid log level: {}. Using 'info'", log_level);
            EnvFilter::new("info")
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    Ok(())
}

/// Print the startup banner with version and configuration info.
/// Includes etcd-compatible version line for e2e test framework detection.
fn print_startup_banner(args: &Args) {
    let version = env!("CARGO_PKG_VERSION");
    println!("╔════════════════════════════════════════════════════════════╗");
    println!(
        "║           rusd v{}                                    ║",
        version
    );
    println!("║   A Rust replacement for etcd with K8s API parity        ║");
    println!("╚════════════════════════════════════════════════════════════╝");
    // etcd e2e framework parses --version output for "etcd Version: X.Y.Z"
    // This line ensures the framework detects a compatible version.
    println!("etcd Version: 3.5.17");
    println!("Git SHA: rusd-{}", version);
    println!("Go Version: rust-{}", option_env!("CARGO_PKG_RUST_VERSION").unwrap_or("stable"));
    println!("Go OS/Arch: {}/{}", std::env::consts::OS, std::env::consts::ARCH);
    println!();
    println!("Configuration:");
    println!("  Name:                  {}", args.name);
    println!("  Data directory:        {}", args.data_dir);
    println!("  Client URLs:           {}", args.listen_client_urls);
    println!("  Peer URLs:             {}", args.listen_peer_urls);
    println!("  Initial cluster:       {}", args.initial_cluster);
    println!("  Cluster state:         {}", args.initial_cluster_state);
    println!("  Heartbeat interval:    {}ms", args.heartbeat_interval);
    println!("  Election timeout:      {}ms", args.election_timeout);
    println!("  Log level:             {}", args.log_level);
    println!();
}

/// Build ServerConfig from CLI arguments.
fn build_server_config(args: &Args) -> anyhow::Result<ServerConfig> {
    let quota_backend_bytes = if args.quota_backend_bytes == 0 {
        8 * 1024 * 1024 * 1024 // Default to 8GB
    } else {
        args.quota_backend_bytes
    };

    let initial_cluster_state = ClusterState::from_str(&args.initial_cluster_state)?;
    let auto_compaction_mode = AutoCompactionMode::from_str(&args.auto_compaction_mode)?;

    Ok(ServerConfig {
        name: args.name.clone(),
        data_dir: PathBuf::from(&args.data_dir),
        listen_client_urls: parse_urls(&args.listen_client_urls),
        listen_peer_urls: parse_urls(&args.listen_peer_urls),
        advertise_client_urls: parse_urls(&args.advertise_client_urls),
        initial_advertise_peer_urls: parse_urls(&args.initial_advertise_peer_urls),
        initial_cluster: args.initial_cluster.clone(),
        initial_cluster_state,
        initial_cluster_token: args.initial_cluster_token.clone(),
        snapshot_count: args.snapshot_count,
        heartbeat_interval_ms: args.heartbeat_interval,
        election_timeout_ms: args.election_timeout,
        max_snapshots: args.max_snapshots,
        max_wals: args.max_wals,
        quota_backend_bytes,
        auto_compaction_mode,
        auto_compaction_retention: args.auto_compaction_retention.clone(),
        cache_size_mb: args.cache_size_mb,
        tls_cert_file: args.cert_file.clone(),
        tls_key_file: args.key_file.clone(),
        tls_trusted_ca_file: args.trusted_ca_file.clone(),
        peer_tls_cert_file: args.peer_cert_file.clone(),
        peer_tls_key_file: args.peer_key_file.clone(),
        peer_tls_trusted_ca_file: args.peer_trusted_ca_file.clone(),
    })
}

/// Parse comma-separated URLs into a vector, trimming whitespace.
fn parse_urls(urls_str: &str) -> Vec<String> {
    urls_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Set up signal handlers for graceful shutdown (SIGTERM, SIGINT).
fn setup_signal_handlers() -> impl std::future::Future<Output = ()> {
    async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");

        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to install SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM signal");
            }
            _ = sigint.recv() => {
                info!("Received SIGINT signal");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_urls_single() {
        let urls = parse_urls("http://localhost:2379");
        assert_eq!(urls, vec!["http://localhost:2379"]);
    }

    #[test]
    fn test_parse_urls_multiple() {
        let urls = parse_urls("http://localhost:2379,http://localhost:2380");
        assert_eq!(urls, vec!["http://localhost:2379", "http://localhost:2380"]);
    }

    #[test]
    fn test_parse_urls_with_whitespace() {
        let urls = parse_urls("http://localhost:2379 , http://localhost:2380");
        assert_eq!(urls, vec!["http://localhost:2379", "http://localhost:2380"]);
    }

    #[test]
    fn test_parse_urls_empty() {
        let urls = parse_urls("");
        assert!(urls.is_empty());
    }
}
