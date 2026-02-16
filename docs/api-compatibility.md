---
title: API Compatibility
---

# API Compatibility

rusd implements 24/24 etcd v3 gRPC API endpoints with full Kubernetes compatibility.

## API Coverage

### KV Service (6/6)

| RPC | Status | Notes |
|-----|--------|-------|
| Range | Implemented | Supports `keys_only`, `count_only`, `min_mod_revision`, `max_mod_revision`, prefix via range_end |
| Put | Implemented | Supports `ignore_value`, `ignore_lease`, `prev_kv` |
| DeleteRange | Implemented | Supports prefix deletion, `prev_kv` |
| Txn | Implemented | Compare-and-swap with value, version, create, mod comparisons |
| Compact | Implemented | Physical compaction of revision history |
| ~~RequestProgress~~ | N/A | etcd internal, not exposed via gRPC |

### Watch Service (2/2)

| RPC | Status | Notes |
|-----|--------|-------|
| Watch (Create) | Implemented | Key, prefix, and range watches with start_revision |
| Watch (Cancel) | Implemented | Cancel by watch_id |

### Lease Service (5/5)

| RPC | Status | Notes |
|-----|--------|-------|
| LeaseGrant | Implemented | TTL-based leases |
| LeaseRevoke | Implemented | Revokes lease and deletes attached keys |
| LeaseKeepAlive | Implemented | Bidirectional streaming TTL refresh |
| LeaseTimeToLive | Implemented | Returns remaining TTL and attached keys |
| LeaseLeases | Implemented | Lists all active leases |

### Auth Service (7/7)

| RPC | Status | Notes |
|-----|--------|-------|
| AuthEnable | Implemented | Enables auth with root user check |
| AuthDisable | Implemented | Disables auth |
| Authenticate | Implemented | JWT token generation |
| UserAdd / UserGet / UserDelete / UserList | Implemented | Full CRUD |
| UserChangePassword | Implemented | bcrypt hashed |
| UserGrantRole / UserRevokeRole | Implemented | Role binding |
| RoleAdd / RoleGet / RoleDelete / RoleList | Implemented | Full CRUD |
| RoleGrantPermission / RoleRevokePermission | Implemented | Key range permissions |

### Cluster Service (4/4)

| RPC | Status | Notes |
|-----|--------|-------|
| MemberAdd | Implemented | With learner support |
| MemberRemove | Implemented | Via Raft ConfigChange |
| MemberUpdate | Implemented | Peer URL update |
| MemberList | Implemented | Returns all members with status |
| MemberPromote | Implemented | Learner to voter promotion |

### Maintenance Service (6/6)

| RPC | Status | Notes |
|-----|--------|-------|
| Alarm | Implemented | Get/activate/deactivate alarms |
| Status | Implemented | Returns leader, db size, raft info |
| Defragment | Implemented | sled flush + compaction |
| Hash | Implemented | CRC32 of database state |
| HashKV | Implemented | CRC32 at specific revision |
| Snapshot | Implemented | Server-streaming snapshot in 64KB chunks |

## Key Compatibility Details

### Range End Semantics

- Empty `range_end`: single-key lookup
- `range_end = \x00` (null byte): unbounded range from key onwards (etcd "all keys" convention)
- Other values: exclusive upper bound

### Leader Forwarding

In multi-node clusters, followers transparently forward write operations (Put, DeleteRange, Txn, Compact) to the current leader via gRPC. Reads are served locally from any node.

### Error Codes

- `Unavailable`: Returned when no leader is elected or leader forwarding fails (triggers etcdctl retry)
- `PermissionDenied`: Auth failures
- `InvalidArgument`: Malformed requests

## etcd E2E Test Results

See [etcd E2E Tests](etcd-e2e-tests) for detailed results from running etcd v3.5.17's own test suite against rusd.

Current: **24/26 Tier 2 tests pass (92.3%)**.
