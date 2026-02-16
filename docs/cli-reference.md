---
title: CLI Reference
---

# CLI Reference

rusd accepts all standard etcd v3 CLI flags. Some flags are fully functional, others are accepted for compatibility with tools that pass etcd flags.

## Core Flags

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--name` | string | `"default"` | Human-readable name for this member |
| `--data-dir` | string | `"default.rusd"` | Path to the data directory |
| `--listen-client-urls` | string | `http://localhost:2379` | URLs to listen for client traffic |
| `--listen-peer-urls` | string | `http://localhost:2380` | URLs to listen for peer traffic |
| `--advertise-client-urls` | string | (same as listen) | Client URLs to advertise to the cluster |
| `--initial-advertise-peer-urls` | string | (same as listen) | Peer URLs to advertise to the cluster |

## Cluster Flags

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--initial-cluster` | string | | Comma-separated `name=url` pairs for initial cluster members |
| `--initial-cluster-state` | string | `"new"` | `"new"` or `"existing"` |
| `--initial-cluster-token` | string | `"etcd-cluster"` | Initial cluster token for bootstrapping |

## TLS Flags

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--cert-file` | string | | Path to client server TLS cert |
| `--key-file` | string | | Path to client server TLS key |
| `--trusted-ca-file` | string | | Path to client server TLS trusted CA cert |
| `--client-cert-auth` | bool | false | Enable client cert authentication |
| `--auto-tls` | bool | false | Generate self-signed TLS certificates |
| `--peer-cert-file` | string | | Path to peer server TLS cert |
| `--peer-key-file` | string | | Path to peer server TLS key |
| `--peer-trusted-ca-file` | string | | Path to peer server TLS trusted CA cert |
| `--peer-client-cert-auth` | bool | false | Enable peer client cert authentication |
| `--peer-auto-tls` | bool | false | Generate self-signed peer TLS certificates |
| `--client-crl-file` | string | | Path to client certificate revocation list |

## Logging Flags

| Flag | Type | Default | Description |
|------|------|---------|-------------|
| `--log-level` | string | `"info"` | Log level: debug, info, warn, error |
| `--log-format` | string | `"text"` | Log format: text, json |
| `--log-outputs` | string | | Accepted for compatibility, ignored |
| `--logger` | string | | Accepted for compatibility, ignored |

## Compatibility Flags (Accepted, Ignored)

These flags are accepted so that tools like etcd's e2e test framework can pass them without errors:

| Flag | Type | Notes |
|------|------|-------|
| `--enable-pprof` | bool | Go profiling, not applicable |
| `--socket-reuse-port` | bool | OS-level, not applicable |
| `--socket-reuse-address` | bool | OS-level, not applicable |
| `--cipher-suites` | string | rustls uses its own cipher selection |
| `--max-txn-ops` | u64 | Not enforced |
| `--max-request-bytes` | u64 | Not enforced |
| `--experimental-initial-corrupt-check` | bool | Not applicable |
| `--experimental-corrupt-check-time` | string | Not applicable |
| `--force-new-cluster` | bool | Not applicable |

## Version Output

`rusd --version` outputs both the rusd version and an etcd-compatible version string for tool compatibility:

```
rusd 0.2.0
etcd Version: 3.5.17
Git SHA: rusd
Go Version: not-applicable
Go OS/Arch: not-applicable
```
