# Kind Cluster Validation for rusd

**Last Updated**: 2026-02-10
**K8s Version**: v1.35.0 (kind v0.31.0)
**Status**: WORKING - Full K8s CRUD operations validated

---

## Kind Configuration

File: `/tmp/kind-rusd.yaml`

```yaml
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
```

This config tells Kind to use an external etcd at `http://host.docker.internal:2479` instead of spinning up its own etcd pod. The `host.docker.internal` DNS name resolves to the host machine from inside Docker containers.

---

## Launch Commands

### 1. Start rusd on the host

```bash
cd /Users/shaileshpant/src/gh-orgs/sreniatnoc/rusd
cargo run --release -- --listen-client-urls http://0.0.0.0:2479 --advertise-client-urls http://0.0.0.0:2479
```

Or use the pre-built binary:

```bash
./rusd --listen-client-urls http://0.0.0.0:2479 --advertise-client-urls http://0.0.0.0:2479
```

### 2. Create the Kind cluster

```bash
kind create cluster --name rusd-test --config /tmp/kind-rusd.yaml
```

### 3. Verify the cluster

```bash
kubectl cluster-info --context kind-rusd-test
kubectl get nodes
kubectl get pods -A
```

### 4. Teardown

```bash
kind delete cluster --name rusd-test
```

---

## etcdctl Compatibility Matrix

All commands tested with:
```bash
export ETCDCTL_API=3
export ETCDCTL_ENDPOINTS=http://localhost:2479
```

### KV Operations - ALL WORK

| Command | Status | Notes |
|---------|--------|-------|
| `etcdctl put foo bar` | OK | Returns "OK" |
| `etcdctl get foo` | OK | Returns key + value |
| `etcdctl get --prefix foo` | OK | Returns all keys with prefix |
| `etcdctl del foo` | OK | Returns delete count |

### Watch Operations - ALL WORK

| Command | Status | Notes |
|---------|--------|-------|
| `etcdctl watch foo` | OK | Receives PUT events |
| `etcdctl watch --prefix /` | OK | Receives all events |
| PUT events | OK | Delivered with key, value, mod_revision |
| DELETE events | OK | Delivered with key, mod_revision |
| prev_kv in events | OK | Previous key-value included in watch events |

### Lease Operations - ALL WORK

| Command | Status | Notes |
|---------|--------|-------|
| `etcdctl lease grant 300` | OK | Returns lease ID and TTL |
| `etcdctl lease list` | OK | Lists active lease IDs |
| `etcdctl lease timetolive <id>` | OK | Returns remaining TTL |
| `etcdctl lease revoke <id>` | OK | Revokes the lease |

### Cluster Operations

| Command | Status | Notes |
|---------|--------|-------|
| `etcdctl member list` | OK | Returns single-node member |
| `etcdctl endpoint health` | OK | Reports healthy |
| `etcdctl endpoint status` | FAIL | Panics etcdctl (divide by zero) |

### Auth Operations

| Command | Status | Notes |
|---------|--------|-------|
| `etcdctl user add` | OK | Creates user |
| `etcdctl role add` | OK | Creates role |
| `etcdctl user list` | EMPTY | Returns empty (known gap) |
| `etcdctl role list` | EMPTY | Returns empty (known gap) |

### Txn Operations

| Command | Status | Notes |
|---------|--------|-------|
| CAS (Compare-and-Swap) | OK | Compare operations work correctly |

---

## Known Limitations

### "Too large resource version" errors (NON-FATAL)
- K8s occasionally logs: `"Too large resource version"`
- These are non-fatal; Kubernetes retries automatically and operations succeed
- Root cause: revision numbering diverges from what K8s informers expect in some edge cases

### Revision-based range reads (APPROXIMATE)
- MVCC only stores the latest version of each key (not full history)
- Range reads with a specific revision return current data, not historical snapshots
- This is sufficient for K8s operation but not fully etcd-compatible

### Integration tests broken (12/12 FAIL)
- Tests use HTTP/JSON endpoints (`/v3/kv/put`, `/health`)
- rusd is a pure gRPC server with no HTTP gateway
- Tests need rewrite to use tonic gRPC client
- Port conflict: all tests hardcode port 12379

---

## K8s Operations Validated

All of the following operations were tested and confirmed working on the Kind cluster with rusd as the backing store:

| Operation | Command | Status |
|-----------|---------|--------|
| Create namespace | `kubectl create namespace test-rusd` | OK |
| Create configmap | `kubectl create configmap test-cm --from-literal=key=value -n test-rusd` | OK |
| Create secret | `kubectl create secret generic test-secret --from-literal=password=secret -n test-rusd` | OK |
| Create serviceaccount | `kubectl create serviceaccount test-sa -n test-rusd` | OK |
| Create role | `kubectl create role test-role --verb=get --resource=pods -n test-rusd` | OK |
| Create rolebinding | `kubectl create rolebinding test-rb --role=test-role --serviceaccount=test-rusd:test-sa -n test-rusd` | OK |
| Create deployment | `kubectl create deployment nginx --image=nginx:alpine -n test-rusd` | OK |
| Scale deployment | `kubectl scale deployment nginx --replicas=3 -n test-rusd` | OK |
| Rolling update | `kubectl set image deployment/nginx nginx=nginx:latest -n test-rusd` | OK |
| Create service | `kubectl expose deployment nginx --port=80 -n test-rusd` | OK |
| Pod exec | `kubectl exec -it <pod> -n test-rusd -- /bin/sh` | OK |
| Delete namespace | `kubectl delete namespace test-rusd` | OK |

All K8s CRUD operations work end-to-end with rusd as the etcd backend.
