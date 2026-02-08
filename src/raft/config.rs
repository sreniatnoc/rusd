use std::time::Duration;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct RaftConfig {
    pub id: u64,
    pub election_timeout_min: Duration,
    pub election_timeout_max: Duration,
    pub heartbeat_interval: Duration,
    pub max_log_entries_per_request: usize,
    pub snapshot_threshold: u64,
    pub peers: Vec<PeerConfig>,
    pub data_dir: PathBuf,
    pub pre_vote: bool,
}

#[derive(Clone, Debug)]
pub struct PeerConfig {
    pub id: u64,
    pub address: String,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            id: 1,
            election_timeout_min: Duration::from_millis(150),
            election_timeout_max: Duration::from_millis(300),
            heartbeat_interval: Duration::from_millis(50),
            max_log_entries_per_request: 100,
            snapshot_threshold: 10000,
            peers: Vec::new(),
            data_dir: PathBuf::from("./data"),
            pre_vote: true,
        }
    }
}

impl RaftConfig {
    pub fn new(id: u64, data_dir: PathBuf) -> Self {
        Self {
            id,
            data_dir,
            ..Default::default()
        }
    }

    pub fn with_peers(mut self, peers: Vec<PeerConfig>) -> Self {
        self.peers = peers;
        self
    }

    pub fn with_timeouts(
        mut self,
        election_min: Duration,
        election_max: Duration,
        heartbeat: Duration,
    ) -> Self {
        self.election_timeout_min = election_min;
        self.election_timeout_max = election_max;
        self.heartbeat_interval = heartbeat;
        self
    }

    pub fn validate(&self) -> super::Result<()> {
        if self.election_timeout_min >= self.election_timeout_max {
            return Err(super::RaftError::ConfigError(
                "election_timeout_min must be less than election_timeout_max".to_string(),
            ));
        }

        if self.heartbeat_interval >= self.election_timeout_min {
            return Err(super::RaftError::ConfigError(
                "heartbeat_interval must be less than election_timeout_min".to_string(),
            ));
        }

        if self.max_log_entries_per_request == 0 {
            return Err(super::RaftError::ConfigError(
                "max_log_entries_per_request must be greater than 0".to_string(),
            ));
        }

        Ok(())
    }
}
