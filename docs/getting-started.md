---
title: Getting Started
---

# Getting Started

## Prerequisites

- **Rust 1.70+** with `cargo` ([install](https://rustup.rs/))
- **protoc** (Protocol Buffers compiler)
  - macOS: `brew install protobuf`
  - Ubuntu: `apt-get install protobuf-compiler`
- **etcdctl** (for testing): `brew install etcd` or from [etcd releases](https://github.com/etcd-io/etcd/releases)

## Build

```bash
git clone https://github.com/sreniatnoc/rusd.git
cd rusd
cargo build --release
```

The binary is at `./target/release/rusd`.

## Run a Single Node

```bash
./target/release/rusd \
  --name node1 \
  --listen-client-urls http://0.0.0.0:2379 \
  --listen-peer-urls http://0.0.0.0:2380 \
  --initial-cluster "node1=http://127.0.0.1:2380"
```

## Verify with etcdctl

```bash
export ETCDCTL_API=3
export ETCDCTL_ENDPOINTS=http://localhost:2379

# Basic KV
etcdctl put mykey "Hello, World!"
etcdctl get mykey
etcdctl get /app/ --prefix

# Watch (runs in background)
etcdctl watch /app/ --prefix &
etcdctl put /app/config "updated"

# Transactions
etcdctl txn <<EOF
value("mykey") = "Hello, World!"

put mykey "Updated"

put mykey "Rollback"
EOF

# Leases
LEASE=$(etcdctl lease grant 60 | grep -o 'ID [0-9a-f]*' | awk '{print $2}')
etcdctl put temp-key temp-val --lease=$LEASE
etcdctl lease timetolive $LEASE
etcdctl lease revoke $LEASE

# Cluster info
etcdctl member list
etcdctl endpoint status
etcdctl endpoint health

# Snapshot
etcdctl snapshot save backup.db
```

## Run with Auto-TLS

rusd can generate self-signed TLS certificates at startup, matching etcd's `--auto-tls` behavior:

```bash
./target/release/rusd \
  --name node1 \
  --listen-client-urls https://127.0.0.1:2379 \
  --auto-tls
```

Connect with etcdctl using `--insecure-transport=false --insecure-skip-tls-verify`:

```bash
etcdctl --endpoints=https://127.0.0.1:2379 \
  --insecure-transport=false \
  --insecure-skip-tls-verify \
  endpoint health
```

## Run with TLS (Manual Certificates)

```bash
./target/release/rusd --name node1 \
  --listen-client-urls https://127.0.0.1:2379 \
  --cert-file certs/server.crt \
  --key-file certs/server.key \
  --trusted-ca-file certs/ca.crt \
  --client-cert-auth
```

## Run as Kubernetes Backend (Kind)

rusd runs natively on the macOS host; the Kind cluster connects via `host.docker.internal`.

```bash
# Start rusd on a non-default port to avoid conflicts
./target/release/rusd \
  --listen-client-urls http://0.0.0.0:2479 \
  --advertise-client-urls http://host.docker.internal:2479

# Create Kind cluster with external etcd config
cat <<EOF | kind create cluster --name rusd-test --config -
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
networking:
  apiServerPort: 6443
nodes:
  - role: control-plane
    kubeadmConfigPatches:
      - |
        kind: ClusterConfiguration
        etcd:
          external:
            endpoints:
              - http://host.docker.internal:2479
EOF

# Verify
kubectl cluster-info --context kind-rusd-test
kubectl get nodes
kubectl create namespace rusd-test
kubectl create deployment nginx --image=nginx:alpine -n rusd-test
kubectl scale deployment nginx --replicas=3 -n rusd-test
kubectl get pods -n rusd-test

# Cleanup
kind delete cluster --name rusd-test
```

## Run a Multi-Node Cluster

```bash
# Start a 3-node cluster
./target/release/rusd --name node1 \
  --listen-client-urls http://127.0.0.1:2379 \
  --listen-peer-urls http://127.0.0.1:2380 \
  --initial-cluster "node1=http://127.0.0.1:2380,node2=http://127.0.0.1:2480,node3=http://127.0.0.1:2580" &

./target/release/rusd --name node2 \
  --listen-client-urls http://127.0.0.1:2479 \
  --listen-peer-urls http://127.0.0.1:2480 \
  --initial-cluster "node1=http://127.0.0.1:2380,node2=http://127.0.0.1:2480,node3=http://127.0.0.1:2580" &

./target/release/rusd --name node3 \
  --listen-client-urls http://127.0.0.1:2579 \
  --listen-peer-urls http://127.0.0.1:2580 \
  --initial-cluster "node1=http://127.0.0.1:2380,node2=http://127.0.0.1:2480,node3=http://127.0.0.1:2580" &

# Verify cluster health
etcdctl --endpoints=http://127.0.0.1:2379,http://127.0.0.1:2479,http://127.0.0.1:2579 endpoint health
etcdctl member list
```

**Leader forwarding:** In multi-node clusters, write requests sent to followers are automatically forwarded to the current leader. You can send writes to any node endpoint.

## Run etcd's E2E Tests Against rusd

See [etcd E2E Tests](etcd-e2e-tests) for the full guide. Quick start:

```bash
# Run the compatibility test script
./scripts/etcd-e2e-compat.sh --tier 2
```

## Run Tests

```bash
# All tests (44 unit + 4 main + 15 integration)
cargo test

# Unit tests only
cargo test --lib

# Integration tests (starts gRPC server per test on random ports)
cargo test --test integration_test

# Multi-node Raft tests
cargo test --test multi_node_test

# TLS tests
cargo test tls

# Chaos tests (leader kill + recovery, data integrity under churn)
./scripts/chaos-test.sh

# etcd e2e compatibility tests
./scripts/etcd-e2e-compat.sh

# With verbose output
RUST_LOG=rusd=debug cargo test -- --nocapture

# Benchmarks (Criterion)
cargo bench
```

## Debug Logging

```bash
# Full debug output
./target/release/rusd --log-level debug

# JSON logging (for log aggregation)
./target/release/rusd --log-format json

# Or use RUST_LOG env var
RUST_LOG=rusd=debug ./target/release/rusd
```
