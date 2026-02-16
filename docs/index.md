---
title: Home
---

# rusd

A high-performance Rust replacement for etcd with full Kubernetes API parity.

rusd is a distributed key-value store that serves as a drop-in replacement for etcd v3. It maintains full gRPC API compatibility while delivering significantly better write performance and lower memory usage.

## Key Results

| Metric | rusd | etcd v3.6.7 |
|--------|------|-------------|
| Sequential PUT | 107 ops/s | 68 ops/s |
| Deploy + Scale (K8s) | 1.5s | 15.9s |
| Rolling Update (K8s) | 2.0s | 12.1s |
| Memory (1000 keys) | 21 MB | 42 MB |
| Binary size | 5.2 MB | ~23 MB |

## Architecture

![Architecture Overview]({{ site.baseurl }}/images/architecture-overview.svg)

rusd is built on four pillars:

1. **MVCC Store** with dual-write to `kv` (latest values) and `kv_rev` (full revision history)
2. **Raft Consensus** with PreVote, real gRPC peer transport, snapshot transfer, leader forwarding, and dynamic membership
3. **Watch Hub** using DashMap + crossbeam channels for real-time event streaming
4. **sled Backend** providing lock-free B+ tree storage

## Pages

- [Architecture](architecture) - Detailed system architecture with diagrams
- [Getting Started](getting-started) - Build, run, and test rusd
- [Benchmarks](benchmarks) - Methodology for head-to-head benchmarks vs etcd v3.6.7
- [Benchmark Results](benchmark-results) - Detailed performance comparison data
- [API Compatibility](api-compatibility) - Full 24/24 API mapping and etcd e2e test results
- [etcd E2E Tests](etcd-e2e-tests) - Running etcd's own test suite against rusd
- [CLI Reference](cli-reference) - All command-line flags

## Status (v0.2.0)

- **24/24 etcd API compatibility** — KV, Watch, Lease, Auth, Cluster, Maintenance (incl. Snapshot)
- **26/26 etcd e2e tests pass** (Tier 2 Core KV, 100%)
- 34/34 Kubernetes compliance tests pass (Kind v1.35)
- Multi-node Raft with leader election, log replication, leader forwarding, and snapshot transfer
- TLS/mTLS and Auto-TLS certificate generation
- Dynamic cluster membership (add/remove/promote members)
- Chaos testing: leader kill + recovery, data integrity under node churn
- Defragmentation and hash verification

### Test Matrix

| Category | Count |
|----------|-------|
| Unit tests | 44 |
| Main tests | 4 |
| Integration tests | 15 |
| Multi-node Raft tests | 7 |
| TLS tests | 8 |
| Chaos tests | 11 |
| K8s compliance tests | 34 |
| Criterion benchmarks | 32 |
| **Total** | **155** |

All tests pass in CI across 9 jobs.
