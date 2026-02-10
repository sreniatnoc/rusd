---
title: Benchmarks
---

# Benchmarks

Head-to-head benchmark results: rusd v0.1.0 vs etcd v3.6.7 on Apple Silicon (2026-02-09).

## etcdctl Microbenchmarks

1000 keys, sequential access via `etcdctl`:

| Operation | rusd | etcd | Advantage |
|-----------|------|------|-----------|
| Sequential PUT (1000 keys) | 9.3s (107 ops/s) | 14.5s (68 ops/s) | **1.56x faster** |
| Sequential GET (1000 keys) | 7.1s (141 ops/s) | 7.2s (139 ops/s) | ~same |
| Range Query (100 iterations) | 740ms (135 ops/s) | 747ms (133 ops/s) | ~same |
| Sequential DELETE (1000 keys) | 7.1s (140 ops/s) | 12.3s (81 ops/s) | **1.73x faster** |
| Memory RSS (after benchmark) | **21 MB** | 42 MB | **2x less** |

### Analysis

**Writes (PUT/DELETE)**: rusd is 1.5-1.7x faster due to sled's write-optimized B+ tree vs etcd's bbolt. sled batches writes and uses lock-free concurrency.

**Reads (GET/Range)**: Essentially identical. Both are limited by gRPC overhead on sequential single-key access. The bottleneck is the network round-trip, not the storage engine.

**Memory**: rusd uses half the memory, benefiting from Rust's zero-cost abstractions and sled's efficient memory management. No garbage collector overhead.

## Kubernetes Workload Benchmarks

Real Kubernetes operations on a Kind cluster:

| Operation | rusd | etcd | Advantage |
|-----------|------|------|-----------|
| Create 10 Namespaces | 314ms | 384ms | **1.2x faster** |
| Create 50 ConfigMaps | 1,538ms | 1,820ms | **1.2x faster** |
| Deploy + Scale to 5 replicas | **1,542ms** | 15,863ms | **10.3x faster** |
| Rolling Update (5 replicas) | **2,006ms** | 12,067ms | **6.0x faster** |

### Why is rusd so much faster for deployments?

The 10x speedup for deploy+scale comes from three factors:

1. **Faster write throughput**: kube-apiserver commits scheduling decisions faster
2. **Faster watch delivery**: rusd's WatchHub uses dedicated per-watcher channels (crossbeam) vs etcd's shared broadcast
3. **Lower memory overhead**: More CPU cycles available for serving requests

Rolling updates (6x faster) involve rapid create/delete cycles for pods, where rusd's write advantage compounds with each operation.

## Resource Usage

| Metric | rusd | etcd |
|--------|------|------|
| Binary size | 5.2 MB | ~23 MB |
| Startup time | ~30ms | ~500ms |
| Idle RSS | 14 MB | ~35 MB |
| Post-benchmark RSS | 21 MB | 42 MB |
| Language | Rust | Go |
| Storage engine | sled (B+ tree) | bbolt (B+ tree) |

## Caveats

1. **Single-node benchmarks**: Both etcd and rusd ran in single-node mode. Multi-node Raft consensus adds latency.
2. **Sequential access**: etcdctl benchmarks use sequential patterns. Concurrent access may show different characteristics.
3. **K8s timing includes all components**: Kubernetes benchmarks include kube-apiserver, kubelet, and controller-manager processing, not just storage latency.
4. **Apple Silicon only**: Results are from macOS on Apple Silicon. Linux x86_64 may show different relative performance.
