---
title: Architecture
---

# Architecture

rusd is a layered system with clear separation between the gRPC API surface, consensus layer, storage engine, and supporting subsystems.

## System Overview

![Architecture Overview]({{ site.baseurl }}/images/architecture-overview.svg)

## MVCC Dual-Write

Every write operation goes through the MVCC store, which maintains two parallel storage trees:

![MVCC Dual-Write Flow]({{ site.baseurl }}/images/mvcc-dual-write.svg)

### kv tree (latest value, key-indexed)
- Key: `{key}` (raw key bytes)
- Value: serialized `KeyValue` (create_rev, mod_rev, version, lease, value)
- Overwrites previous value on each PUT
- Used for: Range queries at current revision, single-key lookups

### kv_rev tree (historical, revision-indexed)
- Key: `{revision.to_be_bytes()}{key}` (8-byte big-endian revision prefix + key bytes)
- Value: serialized `KeyValue` with key included
- Append-only, immutable entries
- Used for: Historical reads, watch catchup from specific revision, point-in-time snapshots

### Compaction

The compaction engine deletes entries from `kv_rev` below a specified revision. Because keys are big-endian sorted, the scan stops at the first entry >= compact_revision, making compaction efficient.

## Raft Consensus

![Raft Consensus Protocol]({{ site.baseurl }}/images/raft-consensus.svg)

### Election Flow (with Pre-Vote)
1. Election timer expires (randomized 150-300ms)
2. **Pre-Vote**: Send `RequestVote(pre_vote=true)` without incrementing term
3. If Pre-Vote gets majority, run **Real Election**: increment term, send `RequestVote(pre_vote=false)`
4. If Real Election gets majority, become Leader
5. Send heartbeats (empty AppendEntries) to all followers

Pre-Vote prevents unnecessary term inflation when a node is temporarily partitioned.

### Log Replication
- Leader appends proposed entries to its local log
- Sends `AppendEntries` RPCs to all followers via `GrpcTransport`
- Followers respond with `(success, match_index)`
- Leader advances `commit_index` when a majority of `match_index` values exceed it
- Committed entries are applied to the state machine

### Peer Transport
Raft peer-to-peer communication uses `raftpb.proto` with three RPCs:
- `AppendEntries` - Log replication and heartbeats
- `RequestVote` - Leader election (both pre-vote and real)
- `InstallSnapshot` - Streaming snapshot transfer

The `GrpcTransport` maintains persistent connections to peers with automatic reconnection on failure.

## Watch Hub

The watch subsystem uses `DashMap` (concurrent hashmap) for subscriber management and `crossbeam-channel` for event dispatch:

1. Watcher registers with a key range and optional start revision
2. On each write, `MvccStore` calls `watch_hub.notify(events, revision, compact_rev)`
3. WatchHub matches events against registered key ranges
4. Matching events are sent to watchers via dedicated crossbeam channels
5. For catchup (start_revision < current), events are read from `kv_rev` tree

## Storage Backend

rusd uses [sled](https://github.com/spacejam/sled), a modern embedded database with:
- Lock-free B+ tree data structure
- Atomic batch operations
- Configurable page cache (256MB default)
- Zero-copy reads via memory mapping

Six sled trees:
- `kv` - Latest key-value pairs
- `kv_rev` - Revision-indexed historical entries
- `index` - Key index metadata
- `meta` - Server metadata (revisions, compact state)
- `lease` - Lease data
- `auth` - Authentication data (users, roles)

## Snapshot Transfer

When a follower falls too far behind the leader's log (i.e., the leader has already compacted the entries the follower needs), the leader sends its full state via the `InstallSnapshot` RPC instead of `AppendEntries`.

### Snapshot Create
1. Leader's `trigger_snapshot()` invokes a callback that serializes the MVCC store
2. Snapshot data format: `[current_revision (8 bytes)][compact_revision (8 bytes)][sled export data]`
3. Snapshot is cached in `RaftLog` alongside the snapshot index and term

### Snapshot Install
1. Leader detects follower needs snapshot when `next_index < log.first_index()`
2. Sends `InstallSnapshot` RPC with the cached snapshot data
3. Follower receives snapshot and invokes the restore callback:
   - Clears all six sled trees
   - Restores sled data from the snapshot payload
   - Rebuilds the in-memory `KeyIndex` from the restored `kv` tree
   - Updates `current_revision` and `compact_revision` atomics
4. Follower resets its commit and applied indices to the snapshot index

### Snapshot Streaming (Maintenance API)
The `Snapshot` RPC streams the MVCC snapshot to clients in 64KB chunks via gRPC server-streaming, compatible with `etcdctl snapshot save`.

## Dynamic Cluster Membership

Cluster membership changes (add, remove, update, promote) go through Raft consensus as `ConfigChange` log entries:

1. Client sends request to leader via Cluster gRPC API
2. Leader validates request and updates `ClusterManager`
3. Leader proposes a `ConfigChange` entry to the Raft log
4. Entry is replicated to followers and committed at majority
5. On apply, all nodes process the membership change

This ensures consistent cluster membership across all nodes. Learner nodes can be added first and promoted to voting members once they catch up.

## Leader Forwarding

In multi-node clusters, followers transparently proxy write requests to the current leader:

1. Client sends a write (Put, DeleteRange, Txn, Compact) to any node
2. If the node is not the leader, it looks up the leader's client URL from its peer mapping
3. A cached gRPC client forwards the request to the leader
4. The leader executes the write through Raft consensus
5. The leader's response is returned to the client through the follower

Leader client URLs are derived at startup from the `--initial-cluster` configuration. The mapping uses the convention `client_port = peer_port - 1` (matching etcd's default behavior).

Reads are served locally from any node without forwarding.

## Auto-TLS

When `--auto-tls` is set and no explicit certificate files are provided, rusd generates self-signed TLS certificates at startup using the `rcgen` crate:

1. Generates an X.509 certificate with Subject Alternative Names (SANs):
   - The node's `--name` value
   - `localhost`
   - `127.0.0.1`
   - `0.0.0.0`
2. Creates a matching private key
3. Configures the gRPC server with the generated cert/key pair

For peer auto-TLS (`--peer-auto-tls`), rusd uses plaintext transport between peers. This is because tonic/rustls does not support `InsecureSkipVerify` (Go's crypto/tls feature), so self-signed peer certificates cannot be verified. This matches the security model: auto-TLS is intended for development and testing, not production.

## TLS/mTLS

rusd supports TLS encryption for both client-to-server and peer-to-peer connections:

- **Client TLS**: `--cert-file`, `--key-file`, `--trusted-ca-file` flags
- **Peer TLS**: `--peer-cert-file`, `--peer-key-file`, `--peer-trusted-ca-file` flags
- **Auto-TLS**: `--auto-tls` generates self-signed certificates at startup
- **mTLS**: When `--client-cert-auth` or `--peer-client-cert-auth` is set, clients must present valid certificates
- Certificate loading uses `rustls` with no OpenSSL dependency
