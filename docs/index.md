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
2. **Raft Consensus** with PreVote, real gRPC peer transport, and deterministic member IDs
3. **Watch Hub** using DashMap + crossbeam channels for real-time event streaming
4. **sled Backend** providing lock-free B+ tree storage

## Pages

- [Architecture](architecture) - Detailed system architecture with diagrams
- [Benchmarks](benchmarks) - Head-to-head benchmark results vs etcd v3.6.7
- [Getting Started](getting-started) - Build, run, and test rusd

## Status

- 34/34 Kubernetes compliance tests pass (Kind v1.35)
- 44 unit tests + 11 integration tests
- Full KV, Watch, Lease, Auth, Cluster, Maintenance APIs
- Single-node production ready; multi-node Raft in progress
