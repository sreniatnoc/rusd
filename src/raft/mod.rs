pub mod config;
pub mod state;
pub mod log;
pub mod transport;
pub mod node;

pub use config::{RaftConfig, PeerConfig};
pub use state::{RaftState, RaftRole};
pub use log::{RaftLog, LogEntry, EntryType};
pub use transport::{
    RaftTransport, AppendEntriesRequest, AppendEntriesResponse, RequestVoteRequest,
    RequestVoteResponse, InstallSnapshotRequest, InstallSnapshotResponse, GrpcTransport,
};
pub use node::RaftNode;

#[derive(Debug, Clone)]
pub enum RaftMessage {
    AppendEntries(AppendEntriesRequest),
    AppendEntriesResponse(AppendEntriesResponse),
    RequestVote(RequestVoteRequest),
    RequestVoteResponse(RequestVoteResponse),
    InstallSnapshot(InstallSnapshotRequest),
    InstallSnapshotResponse(InstallSnapshotResponse),
}

#[derive(Debug)]
pub enum RaftError {
    LogError(String),
    TransportError(String),
    StateError(String),
    ConfigError(String),
    NotLeader,
    LogExists,
    InvalidTerm,
}

impl std::fmt::Display for RaftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RaftError::LogError(msg) => write!(f, "Log error: {}", msg),
            RaftError::TransportError(msg) => write!(f, "Transport error: {}", msg),
            RaftError::StateError(msg) => write!(f, "State error: {}", msg),
            RaftError::ConfigError(msg) => write!(f, "Config error: {}", msg),
            RaftError::NotLeader => write!(f, "Not a leader"),
            RaftError::LogExists => write!(f, "Log entry already exists"),
            RaftError::InvalidTerm => write!(f, "Invalid term"),
        }
    }
}

impl std::error::Error for RaftError {}

pub type Result<T> = std::result::Result<T, RaftError>;
