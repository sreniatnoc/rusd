//! # rusd: A high-performance Rust replacement for etcd
//!
//! rusd is a distributed reliable key-value store written in Rust, designed as a drop-in
//! replacement for etcd with full Kubernetes API parity. It provides:
//!
//! - **MVCC semantics**: Multi-version concurrency control with point-in-time reads
//! - **Raft consensus**: Distributed consensus for high availability
//! - **Watch subsystem**: Real-time change notifications (critical for K8s)
//! - **Lease management**: TTL-based automatic key deletion
//! - **Authentication & RBAC**: Fine-grained access control
//! - **High performance**: Optimized for Kubernetes workloads
//!
//! # Usage
//!
//! ```bash
//! rusd --name rusd-node1 \
//!      --listen-client-urls http://localhost:2379 \
//!      --listen-peer-urls http://localhost:2380 \
//!      --initial-cluster rusd-node1=http://localhost:2380
//! ```

// Re-export generated protobuf types
pub mod etcdserverpb {
    tonic::include_proto!("etcdserverpb");
}

pub mod raftpb {
    tonic::include_proto!("raftpb");
}

// Core modules
pub mod api;
pub mod auth;
pub mod cluster;
pub mod lease;
pub mod raft;
pub mod server;
pub mod storage;
pub mod watch;

// Re-export main types at crate root for convenience
pub use auth::AuthStore;
pub use cluster::ClusterManager;
pub use lease::LeaseManager;
pub use server::{RusdServer, ServerConfig};
pub use storage::{Backend, Event, KeyValue, MvccStore};
pub use watch::WatchHub;
