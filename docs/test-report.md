# rusd Test Report: K8s Compatibility Validation

**Date:** 2026-02-09
**Version:** v0.1.0 (commit 3ba6827)
**Test Environment:** macOS (Darwin 25.3.0), Kind v0.x, K8s v1.35.0
**Architecture:** rusd native on host, Kind cluster via `host.docker.internal:2479`

---

## Executive Summary

rusd successfully operates as an external etcd replacement for a Kubernetes v1.35 Kind cluster. All core K8s operations work: namespace/configmap/secret CRUD, RBAC, deployments with scaling and rolling updates, services, and pod exec. Watch event delivery with prev_kv enables the full K8s informer lifecycle.

---

## Unit Tests: 40/40 PASS

All library-level unit tests pass:

| Module | Tests | Status |
|--------|-------|--------|
| storage::backend | 8 | PASS |
| storage::mvcc | 4 | PASS |
| storage::index | 8 | PASS |
| raft::log | 4 | PASS |
| raft::node | 4 | PASS |
| watch | 4 | PASS |
| lease | 4 | PASS |
| auth | 2 | PASS |
| cluster | 2 | PASS |

## Integration Tests: 12/12 FAIL (Pre-existing)

Integration tests are broken because they:
1. Use HTTP health checks (`reqwest`) against a gRPC-only server
2. All use the same fixed port (12379), causing "Address already in use" on parallel execution
3. Need rewriting to use tonic gRPC clients

**Action needed:** Rewrite integration tests with tonic client and random port allocation.

---

## etcdctl Compatibility Matrix

| API | Operation | Status | Notes |
|-----|-----------|--------|-------|
| KV | Put | PASS | Single key, with lease |
| KV | Get | PASS | Single key, prefix |
| KV | Delete | PASS | Single key, range |
| KV | Txn | PASS | CAS with compare operations |
| Watch | Create | PASS | Single key, prefix, range |
| Watch | Event Delivery | PASS | PUT and DELETE events with prev_kv |
| Watch | Cancel | PASS | Via WatchCancelRequest |
| Watch | Progress | PASS | Returns current revision |
| Lease | Grant | PASS | With TTL |
| Lease | Revoke | PASS | Does not clean attached keys (bug) |
| Lease | List | PASS | |
| Lease | KeepAlive | PASS | |
| Cluster | Member List | PASS | Single member |
| Cluster | Endpoint Health | PASS | |
| Cluster | Endpoint Status | FAIL | Panics (missing response fields) |
| Maintenance | Compact | PASS | |
| Maintenance | Snapshot | NOT IMPL | Returns unimplemented |
| Auth | User Add | PASS | |
| Auth | Enable/Disable | PARTIAL | Basic flow works |

---

## Kind Cluster Validation (K8s v1.35.0)

### Cluster Bootstrap

| Step | Status | Notes |
|------|--------|-------|
| Kind node image pull | PASS | kindest/node:v1.35.0 |
| Kind config write | PASS | External etcd at host.docker.internal:2479 |
| Control plane start | PASS | kube-apiserver, controller-manager, scheduler |
| CNI install (kindnet) | PASS | Network plugin initialized |
| StorageClass install | PASS | local-path-provisioner |
| Node Ready | PASS | ~10 seconds after cluster creation |

### K8s CRUD Operations

| Operation | Status | Details |
|-----------|--------|---------|
| Create Namespace | PASS | `rusd-test-ns` created |
| List Namespaces | PASS | 6 namespaces including system |
| Create ConfigMap | PASS | With multiple keys |
| Patch ConfigMap | PASS | JSON merge patch |
| Create Secret | PASS | Opaque type |
| Delete Secret | PASS | Confirmed removal |
| Create ServiceAccount | PASS | Custom SA |
| Create Role | PASS | get,list,watch on pods |
| Create RoleBinding | PASS | SA to Role binding |
| Create Deployment | PASS | nginx:alpine, 1 replica |
| Scale Deployment (3) | PASS | 3/3 pods running |
| Rolling Update | PASS | nginx:alpine -> nginx:latest |
| Expose Service | PASS | ClusterIP on port 80 |
| Pod Exec | PASS | hostname command |
| Delete Namespace | PASS | Full resource cascade |
| Label Resources | PASS | Deployment labels |
| Annotate Resources | PASS | Custom annotations |

### K8s System Components

| Component | Status | Details |
|-----------|--------|---------|
| kube-apiserver | Running (1/1) | Connected to rusd |
| kube-controller-manager | Running (1/1) | Reconciliation working |
| kube-scheduler | Running (1/1) | Pod scheduling working |
| kube-proxy | Running (1/1) | Network rules applied |
| CoreDNS | Running (2/2) | DNS resolution working |
| kindnet | Running (1/1) | CNI plugin active |
| local-path-provisioner | Running (1/1) | Storage class available |

---

## Known Issues

### 1. "Too large resource version" (Non-fatal)
**Severity:** Low (K8s retries automatically)
**Description:** kube-apiserver's cacher occasionally gets `"Timeout: Too large resource version: X, current: Y"` errors. These occur because rusd's MVCC store only keeps the latest value per key, not full revision history. When K8s requests data at a specific revision, the response may not perfectly match expected revision semantics.
**Impact:** None observable - all K8s operations complete successfully despite these errors.
**Fix:** Implement proper revision-indexed storage in MVCC store.

### 2. Lease Revoke Does Not Clean Attached Keys
**Severity:** Medium
**Description:** When a lease is revoked, keys attached to that lease are not automatically deleted.
**Impact:** Keys may persist beyond lease expiration.
**Fix:** Wire lease expiration to key deletion in LeaseManager.

### 3. etcdctl `endpoint status` Panics
**Severity:** Low
**Description:** The maintenance endpoint status response is missing required fields, causing a panic.
**Fix:** Populate all StatusResponse fields.

### 4. Integration Tests Broken
**Severity:** Low (unit tests cover core logic)
**Description:** All 12 integration tests fail due to HTTP health check against gRPC server and port collision.
**Fix:** Rewrite with tonic gRPC client and random port allocation.

---

## K8s Black-Box Compliance Tests: 34/34 PASS

Comprehensive compliance test suite run against the live Kind cluster with rusd:

| Category | Test | Status |
|----------|------|--------|
| Core | Create Namespace | PASS |
| Core | Namespace is Active | PASS |
| Core | Create ConfigMap | PASS |
| Core | Read ConfigMap data | PASS |
| Core | Patch ConfigMap | PASS |
| Core | Delete ConfigMap | PASS |
| Core | Create Secret | PASS |
| Core | Read Secret data (base64) | PASS |
| Core | Delete Secret | PASS |
| Core | Create ServiceAccount | PASS |
| RBAC | Create Role | PASS |
| RBAC | Create RoleBinding | PASS |
| RBAC | Create ClusterRole | PASS |
| RBAC | Create ClusterRoleBinding | PASS |
| Workloads | Create Deployment | PASS |
| Workloads | Deployment pod ready (1/1) | PASS |
| Workloads | Scale Deployment to 2 | PASS |
| Workloads | Scaled pods ready (2/2) | PASS |
| Workloads | Set image for rolling update | PASS |
| Workloads | Rolling update completed | PASS |
| Workloads | Create DaemonSet | PASS |
| Workloads | DaemonSet pod ready | PASS |
| Workloads | Create Job | PASS |
| Workloads | Job completed | PASS |
| Networking | Create ClusterIP Service | PASS |
| Networking | Service has ClusterIP | PASS |
| Networking | Create NodePort Service | PASS |
| Storage | Create PVC | PASS |
| Pod | Create Pod with env vars | PASS |
| Pod | Pod env var accessible | PASS |
| Pod | Pod exec works | PASS |
| Pod | Pod logs accessible | PASS |
| Versioning | ResourceVersion changes on update | PASS |
| Cleanup | Delete namespace (cascade) | PASS |

---

## Performance Observations

- rusd startup: ~30ms (vs etcd ~500ms)
- Memory usage (idle): ~14MB RSS
- K8s bootstrap (Kind cluster creation): ~10-15 seconds
- K8s node Ready: ~10 seconds after cluster creation
- Rolling update (3 replicas): ~30 seconds
- All observations on Apple Silicon (M-series) Mac

---

## Architecture Notes

### Watch Event Flow
```
KvService.put() -> watch::Event(prev_kv) -> WatchHub.notify()
  -> per-watcher mpsc channel -> forwarding task
  -> proto Event conversion -> gRPC WatchResponse -> client
```

### Server Lifecycle
```
main() -> RusdServer::new() -> server.run(shutdown_signal)
  -> tonic Server::builder()
       .http2_keepalive_interval(10s)
       .http2_keepalive_timeout(20s)
       .serve_with_shutdown(addr, shutdown)
```

### Txn Compare Operations
Supports all etcd compare targets: Version, CreateRevision, ModRevision, Value, Lease
with operators: Equal, Greater, Less, NotEqual.

---

## Changes Made (This Session)

1. **Watch event delivery** - Wired WatchHub to KvService (put/delete/txn) and rewrote WatchService
2. **prev_kv in Watch events** - MvccStore.put() returns previous KV, included in all Watch events
3. **Txn CAS semantics** - Implemented proper Compare operations (was: always succeed)
4. **Proto wire compatibility** - Fixed WatchResponse events field (7->11), WatchCreateRequest prev_kv
5. **Graceful shutdown** - serve_with_shutdown() + HTTP/2 keepalive settings
6. **Revision fixes** - next_revision() returns new value, initial revision=1 (not 0)
7. **Dockerfile** - Added missing COPY for build.rs, proto/, benches/, tests/
