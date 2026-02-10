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

## Run Tests

```bash
# All tests (44 unit + 11 integration)
cargo test

# Unit tests only
cargo test --lib

# Integration tests (starts gRPC server per test on random ports)
cargo test --test integration_test

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
