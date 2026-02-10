# rusd Multi-Node Execution and Validation Plan

## Table of Contents
1. [Feasibility Analysis: Current Raft Implementation](#1-feasibility-analysis)
2. [Multi-Node Raft Validation Plan](#2-multi-node-raft-validation-plan)
3. [GitHub Actions CI/CD Workflow](#3-github-actions-cicd-workflow)
4. [Multi-Node Test Script](#4-multi-node-test-script)
5. [Implementation Roadmap](#5-implementation-roadmap)

---

## 1. Feasibility Analysis

### What the Source Code Reveals

After a thorough reading of every file in `src/raft/`, `src/server/mod.rs`, and `src/main.rs`, here is an honest assessment of the current Raft implementation status.

### 1.1 Leader Election: PARTIALLY IMPLEMENTED (single-node only)

**What works:**
- `RaftState` has all role transitions: `become_follower()`, `become_candidate()`, `become_pre_candidate()`, `become_leader()` (file: `src/raft/state.rs`)
- `RaftNode::start_election()` dispatches to either `start_pre_vote()` or `start_real_election()` depending on config (file: `src/raft/node.rs`, lines 139-153)
- Single-node auto-election: when `config.peers.is_empty()`, `RaftNode::new()` immediately calls `state.become_leader(&[])` (file: `src/raft/node.rs`, lines 47-49)
- `RequestVoteRequest` / `RequestVoteResponse` structs are fully defined (file: `src/raft/transport.rs`)
- `handle_request_vote()` correctly implements the Raft vote-granting logic: term comparison, voted-for check, log up-to-date check (file: `src/raft/node.rs`, lines 444-485)
- Pre-vote protocol is supported via `pre_vote: bool` on `RequestVoteRequest`

**What does NOT work for multi-node:**
- `start_real_election()` fires off vote requests via `tokio::spawn` but **does not collect responses**. The spawned tasks discard the `RequestVoteResponse` (file: `src/raft/node.rs`, lines 224-233). The comment on line 235 says: "In real implementation, we'd wait for responses and check for majority."
- The majority check on line 237 (`if vote_count * 2 > (self.config.peers.len() + 1)`) uses `vote_count = 1` (self-vote only) because responses are never aggregated. This means a node will only self-elect if it is the sole member.
- `start_pre_vote()` has the same problem: votes fire-and-forget, majority check uses only self-vote (line 191).

### 1.2 Log Replication: STRUCTURALLY PRESENT, NOT WIRED

**What works:**
- `RaftLog` is a fully functional persistent log backed by sled. It supports `append()`, `get()`, `get_range()`, `truncate_after()`, `truncate_before()`, recovery from disk, and snapshots (file: `src/raft/log.rs`).
- `AppendEntriesRequest` / `AppendEntriesResponse` structs are properly defined with all Raft fields including `conflict_index` / `conflict_term` for accelerated backtracking (file: `src/raft/transport.rs`).
- `handle_append_entries()` on the follower side is correctly implemented per the Raft paper: term checks, log consistency checks, truncation of conflicting entries, append, and commit index advancement (file: `src/raft/node.rs`, lines 349-442).
- `send_heartbeats()` correctly constructs `AppendEntriesRequest` with entries from `next_index` and processes responses with conflict resolution (file: `src/raft/node.rs`, lines 264-347).

**What does NOT work for multi-node:**
- `GrpcTransport::send_append_entries()` returns a **hardcoded mock response** (`success: false`) instead of actually making a gRPC call (file: `src/raft/transport.rs`, lines 114-131). The comment says: "In a real implementation, this would establish a gRPC connection and send the request."
- Same for `send_request_vote()` (returns `vote_granted: false`) and `send_install_snapshot()` (file: `src/raft/transport.rs`, lines 134-162).
- There is no Raft RPC **server** endpoint -- the gRPC server in `src/server/mod.rs` only exposes etcd client APIs (KV, Watch, Lease, Cluster, Maintenance, Auth). There is no service for peer-to-peer Raft RPCs (AppendEntries, RequestVote, InstallSnapshot).

### 1.3 Raft Event Loop: DISABLED

- `raft_event_loop()` is defined in `src/server/mod.rs` (lines 593-601) and calls `raft.tick()` every 10ms.
- However, it is **commented out** in `RusdServer::run()` (lines 331-337) with the comment: "TODO: Fix Send trait issue with parking_lot::Mutex for raft event loop."
- The root cause: `RaftNode` uses `parking_lot::Mutex` for `election_timer` and `heartbeat_timer`, whose guards are `!Send`, preventing the struct from being used across `.await` points in a `tokio::spawn` context.
- `RaftNode::run()` also returns a dummy `JoinHandle` with the same TODO (file: `src/raft/node.rs`, lines 594-602).

### 1.4 Message Routing: STUB

- `RaftNode::step()` receives `RaftMessage` variants and dispatches to handlers, but responses are discarded (line 576: `let _ = response;`). There is no return channel to send responses back to the originating peer (file: `src/raft/node.rs`, lines 571-592).

### 1.5 Server Multi-Node Configuration: PARTIALLY WORKING

- `main.rs` accepts all etcd-compatible CLI flags including `--initial-cluster`, `--initial-cluster-state`, `--initial-cluster-token`, `--listen-peer-urls` (file: `src/main.rs`).
- `parse_peer_configs()` correctly parses `"node1=http://...,node2=http://..."` format and excludes self (file: `src/server/mod.rs`, lines 440-464).
- Peer addresses are fed to `GrpcTransport` but the transport is a stub.
- Each node generates a **random UUID** for its `member_id` on every startup (file: `src/server/mod.rs`, line 206). This means nodes cannot form a stable cluster because their IDs change between restarts.

### 1.6 Summary Table

| Component | Status | Blocking Issues |
|---|---|---|
| Raft state machine | DONE | None |
| Raft log (sled-backed) | DONE | None |
| Election (single-node) | DONE | None |
| Election (multi-node) | STUB | Vote responses not collected |
| Log replication logic | DONE (handler side) | Transport is a mock |
| gRPC transport (peer RPC) | STUB | Returns hardcoded responses |
| Raft RPC server endpoint | MISSING | No peer-facing gRPC service |
| Raft event loop | DISABLED | parking_lot Send issue |
| Snapshot transfer | STUB | Transport mock |
| Stable node identity | MISSING | Random UUID on each start |
| etcd client API (KV/Watch/...) | DONE | None (34/34 K8s tests pass single-node) |

### 1.7 Verdict

**Multi-node Raft does NOT work today.** Starting 3 rusd processes with `--initial-cluster` will produce 3 independent single-node leaders, each serving client requests from its own isolated store. No data will replicate between them.

The good news is that the hardest parts (Raft state machine, log, follower-side handlers) are correctly implemented. The gaps are in the networking/transport layer and event loop plumbing.

---

## 2. Multi-Node Raft Validation Plan

This section describes the test strategy to validate multi-node Raft once the implementation gaps are filled. Each test can run on a **single GitHub Actions runner** using different ports.

### 2.1 Port Allocation Scheme

| Node | Name | Client Port | Peer Port | Data Dir |
|------|------|-------------|-----------|----------|
| 1 | rusd1 | 2379 | 2380 | /tmp/rusd-test/node1 |
| 2 | rusd2 | 2479 | 2480 | /tmp/rusd-test/node2 |
| 3 | rusd3 | 2579 | 2580 | /tmp/rusd-test/node3 |
| 4 | rusd4 | 2679 | 2680 | /tmp/rusd-test/node4 |
| 5 | rusd5 | 2779 | 2780 | /tmp/rusd-test/node5 |

### 2.2 Test Categories

#### T1: Leader Election (3-node cluster)

**Precondition:** Build rusd binary.

1. Start 3 nodes with `--initial-cluster "rusd1=http://127.0.0.1:2380,rusd2=http://127.0.0.1:2480,rusd3=http://127.0.0.1:2580"`.
2. Wait up to 30 seconds for cluster health.
3. Query `etcdctl endpoint status` on all 3 endpoints.
4. **Assert:** Exactly one node reports itself as leader.
5. **Assert:** All 3 nodes report the same leader ID.
6. **Assert:** All 3 nodes report the same Raft term.

#### T2: Log Replication (Write-to-leader, read-from-follower)

**Precondition:** T1 passes (cluster is healthy with a leader).

1. Identify the leader endpoint from T1.
2. Write 100 keys to the leader: `etcdctl --endpoints=<leader> put /repl/key{i} value{i}`.
3. Wait 2 seconds for replication.
4. Read all 100 keys from each follower: `etcdctl --endpoints=<follower> get /repl/ --prefix --count-only`.
5. **Assert:** Each follower returns exactly 100 keys.
6. **Assert:** Values match on all nodes (spot-check 10 random keys).

#### T3: Linearizable Read Consistency

**Precondition:** T2 passes.

1. Write a key to the leader.
2. Immediately read it from a follower with `--consistency=l` (linearizable).
3. **Assert:** The read returns the latest value without stale data.
4. Repeat 50 times to verify consistency is not probabilistic.

#### T4: Leader Failover

**Precondition:** T1 passes.

1. Identify the current leader PID.
2. `kill -9 <leader_pid>` (simulate crash).
3. Wait up to 10 seconds (2x election timeout).
4. Query `endpoint status` on the 2 surviving nodes.
5. **Assert:** A new leader is elected (different from the killed node).
6. **Assert:** Both surviving nodes agree on the new leader.
7. Write a key to the new leader.
8. Read it from the remaining follower.
9. **Assert:** Data is accessible and consistent.

#### T5: Rejoin After Failure

**Precondition:** T4 passes (a node was killed).

1. Restart the killed node with the same configuration.
2. Wait up to 30 seconds for it to rejoin.
3. Query `endpoint status` on all 3 nodes.
4. **Assert:** The restarted node is a follower.
5. **Assert:** All keys written during the downtime are replicated to the restarted node.
6. Read 10 keys from the restarted node.
7. **Assert:** Values match the leader.

#### T6: Network Partition Simulation

**Precondition:** 3-node cluster is healthy.

Network partitions on a single machine can be simulated using `iptables` (Linux) or `pfctl` (macOS). On GitHub Actions (Ubuntu), `iptables` is available.

1. Block traffic to/from the leader's peer port using iptables:
   ```bash
   sudo iptables -A INPUT -p tcp --dport <leader_peer_port> -j DROP
   sudo iptables -A OUTPUT -p tcp --dport <leader_peer_port> -j DROP
   ```
2. Wait for election timeout (5 seconds).
3. **Assert:** The 2 connected nodes elect a new leader.
4. **Assert:** The partitioned node steps down (loses leadership).
5. Write a key to the new leader.
6. Remove the iptables rules.
7. Wait for the partitioned node to rejoin.
8. **Assert:** The previously partitioned node catches up with the new writes.

#### T7: 5-Node Cluster

Repeat T1-T4 with 5 nodes (ports 2379-2779). Validates that quorum logic works correctly with `quorum = 3`.

1. Start 5 nodes.
2. **Assert:** One leader elected.
3. Kill 2 nodes (minority).
4. **Assert:** Cluster remains operational (3/5 quorum maintained).
5. Kill a third node.
6. **Assert:** Cluster becomes read-only or unavailable (only 2/5, no quorum).

#### T8: Data Integrity Under Churn

1. Start 3-node cluster.
2. Run a continuous write workload (10 keys/second).
3. Cycle through killing and restarting each node with 10-second intervals.
4. After all nodes have been killed and restarted, stop the write workload.
5. **Assert:** All keys are present on all 3 nodes.
6. **Assert:** No data loss or corruption.

### 2.3 Tooling

- **etcdctl v3**: Primary test driver. Install via GitHub release or package manager.
- **grpcurl**: For raw gRPC inspection of Raft RPC endpoints (when implemented).
- **iptables**: For network partition simulation (Linux only, GitHub Actions Ubuntu runners).
- **timeout / wait-for-it**: For health check polling.
- **jq**: For parsing JSON output from etcdctl.

---

## 3. GitHub Actions CI/CD Workflow

The workflow file is located at `.github/workflows/ci.yml`.

It includes:
- Dependency caching (Cargo registry and build artifacts)
- Proto compilation dependencies
- Build, unit test, integration test, benchmark stages
- Single-node validation with etcdctl
- Multi-node cluster validation (conditional, runs when multi-node Raft is ready)
- Artifact uploads for test reports and benchmark results

See the actual file at `.github/workflows/ci.yml` for the complete workflow definition.

---

## 4. Multi-Node Test Script

The test script is located at `scripts/multi-node-test.sh`.

It performs:
1. Builds rusd if the binary is not present.
2. Starts 3 rusd nodes on ports 2379, 2479, and 2579 with proper `--initial-cluster` configuration.
3. Waits for the cluster to become healthy (up to 30 seconds).
4. Detects the leader by querying all endpoints.
5. Runs data consistency tests: write to leader, verify on followers.
6. Tests failover: kills the leader, waits for new election, verifies continued operation.
7. Tests data integrity: verifies no data loss after failover.
8. Reports a summary of passed/failed tests.
9. Cleans up all processes and temp directories.

The script is designed to exit with code 0 on success and non-zero on any failure, making it suitable for CI.

See the actual file at `scripts/multi-node-test.sh`.

---

## 5. Implementation Roadmap

### Phase 1: Fix the Raft Event Loop (Estimated: 1-2 days)

**Problem:** `parking_lot::Mutex` guards are `!Send`, preventing `RaftNode` from being used in `tokio::spawn`.

**Fix:**
- Replace `parking_lot::Mutex<Option<Instant>>` for `election_timer` and `heartbeat_timer` with `tokio::sync::Mutex<Option<Instant>>`.
- Replace `parking_lot::Mutex<tokio::sync::mpsc::Receiver<...>>` for `message_rx` with `tokio::sync::Mutex`.
- Alternatively, restructure the tick loop to avoid holding guards across `.await` points.

**Validation:** Uncomment the `raft_event_loop` spawn in `RusdServer::run()`. Verify single-node still works with the event loop active.

### Phase 2: Implement Raft gRPC Transport (Estimated: 3-5 days)

**Problem:** `GrpcTransport` returns hardcoded mock responses.

**Tasks:**
1. Define a Raft internal service in protobuf (or use the existing tonic codegen):
   ```protobuf
   service RaftInternal {
     rpc AppendEntries (AppendEntriesReq) returns (AppendEntriesResp);
     rpc RequestVote (RequestVoteReq) returns (RequestVoteResp);
     rpc InstallSnapshot (InstallSnapshotReq) returns (InstallSnapshotResp);
   }
   ```
2. Implement the gRPC client in `GrpcTransport` to actually connect to peers and send RPCs.
3. Implement the gRPC server handler that receives peer RPCs and dispatches to `RaftNode::handle_*` methods.
4. Add the Raft internal service to the tonic `Server::builder()` in `RusdServer::run()`, binding to `listen_peer_urls`.

**Validation:** Start 2 nodes and verify they can exchange heartbeats (visible in logs).

### Phase 3: Wire Up Vote Collection (Estimated: 1-2 days)

**Problem:** `start_real_election()` fires vote requests but discards responses.

**Fix:**
- Use `tokio::select!` or `futures::future::join_all` to collect vote responses.
- Count granted votes and call `become_leader()` when a majority is reached.
- Handle response terms (step down if a higher term is seen).

**Validation:** Start 3 nodes. Verify that exactly one becomes leader within the election timeout.

### Phase 4: Stable Node Identity (Estimated: 0.5 day)

**Problem:** `member_id` is a random UUID generated on each startup.

**Fix:**
- Persist the member ID in the data directory on first boot.
- On subsequent boots, read the persisted ID.
- Derive the ID deterministically from `--name` and `--initial-cluster-token` for the initial bootstrap case.

**Validation:** Restart a node. Verify it has the same member ID as before.

### Phase 5: Multi-Node Validation (Estimated: 2-3 days)

Run the full test suite from Section 2:
- T1 through T8.
- Run `scripts/multi-node-test.sh` in CI.
- Fix any bugs discovered.

### Total Estimated Effort: 8-13 days

### Priority Order

1. Phase 1 (event loop fix) -- unblocks everything
2. Phase 2 (transport) -- enables actual communication
3. Phase 3 (vote collection) -- enables leader election
4. Phase 4 (stable identity) -- enables cluster stability
5. Phase 5 (validation) -- proves it all works
