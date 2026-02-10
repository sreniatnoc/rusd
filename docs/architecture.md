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
