---
title: etcd E2E Tests
---

# Running etcd's E2E Tests Against rusd

rusd can be tested using etcd's own end-to-end test suite. The test framework spawns the binary as a subprocess and exercises every API through `etcdctl`, providing the most rigorous compatibility validation possible.

## How It Works

etcd's test framework (`tests/framework/e2e/`) does the following:

1. Starts the binary specified by the bin directory
2. Waits for `"ready to serve client requests"` on stdout
3. Parses `--version` output for `"etcd Version: X.Y.Z"`
4. Runs tests using `etcdctl` against the spawned process
5. Passes various CLI flags (TLS, auth, logging) that the binary must accept

rusd outputs the correct ready signal and version format, and accepts all required CLI flags.

## Quick Start

```bash
# Build rusd
cargo build --release

# Run Tier 1 (smoke tests)
./scripts/etcd-e2e-compat.sh --tier 1

# Run Tier 2 (core KV tests)
./scripts/etcd-e2e-compat.sh --tier 2

# Run all tiers
./scripts/etcd-e2e-compat.sh
```

## Test Tiers

The script runs etcd v3.5.17's tests in progressive tiers:

| Tier | Name | Tests | Timeout | Coverage |
|------|------|-------|---------|----------|
| 1 | Smoke | Put/Get NoTLS | 5m | Basic connectivity |
| 2 | Core KV | All Put, Get, Del | 10m | Full KV operations, TLS modes, output formats |
| 3 | Watch + Lease + Auth | Watch, Lease, Auth suites | 15m | Event streaming, TTL management, RBAC |
| 4 | Cluster + Member | Member add/remove, endpoints | 15m | Dynamic membership |
| 5 | Advanced | Compact, Defrag, Snapshot | 15m | Maintenance operations |
| 6 | Full Suite | All e2e tests | 60m | Complete compatibility |

## Current Results (v0.2.0)

| Tier | Passed | Failed | Rate |
|------|--------|--------|------|
| 1 — Smoke | 2/2 | 0 | **100%** |
| 2 — Core KV | 24/26 | 2 | **92.3%** |

### Tier 2 Remaining Failures

**TestCtlV3GetFormat (protobuf binary):** The test compares raw protobuf bytes that encode `cluster_id` and `member_id`. rusd generates different IDs than etcd (different ID generation algorithms), so the binary output never matches. This is not a correctness issue — the data is semantically correct.

**TestCtlV3GetRevokedCRL:** Requires Certificate Revocation List (CRL) enforcement in the TLS handshake. tonic's `ServerTlsConfig` does not expose CRL checking; fixing this requires building a custom `rustls::ServerConfig` with a CRL verifier.

Neither failure affects Kubernetes workloads.

## What the Script Does

1. **Prerequisites check** — Verifies Go 1.22+, git, and cargo are available
2. **Clone etcd** — Shallow clone of etcd v3.5.17 to `/tmp/rusd-etcd-e2e`
3. **Build tools** — Compiles `etcdctl` and `etcdutl` from etcd's multi-module repo
4. **Symlink** — Links the rusd binary as `etcd` in the etcd repo's `bin/` directory
5. **Run tests** — Executes Go tests with `go test ./tests/e2e/...` against the symlinked binary
6. **Report** — Generates a JSON compatibility report with pass/fail counts

## Manual Setup

If you want to run individual tests:

```bash
# Clone etcd
git clone --depth 1 --branch v3.5.17 https://github.com/etcd-io/etcd.git /tmp/etcd-src

# Build etcdctl (separate Go module)
cd /tmp/etcd-src/etcdctl && go build -o ../bin/etcdctl .
cd /tmp/etcd-src/etcdutl && go build -o ../bin/etcdutl .

# Symlink rusd as etcd
ln -sf $(pwd)/target/release/rusd /tmp/etcd-src/bin/etcd

# Verify
/tmp/etcd-src/bin/etcd --version
/tmp/etcd-src/bin/etcdctl version

# Run a specific test
cd /tmp/etcd-src/tests
export PATH="/tmp/etcd-src/bin:$PATH"
go test ./e2e/... -run "TestCtlV3PutNoTLS" -v -timeout 5m -count=1
```

## Compatibility Shims

rusd includes several compatibility features specifically for the etcd test framework:

- **Ready signal:** `println!("ready to serve client requests")` on stdout after gRPC server binds
- **Version output:** `rusd --version` includes `etcd Version: 3.5.17`
- **CLI flags:** All etcd flags are accepted (some functional, some accepted-and-ignored)
- **Error codes:** "not a leader" returns gRPC `Unavailable` (enables etcdctl retry logic)
- **Leader forwarding:** Followers proxy writes to the leader in multi-node clusters

## CI Integration

The etcd e2e compatibility tests run as CI Job 9 on every push and PR. The job:

1. Downloads the rusd binary artifact from Job 1
2. Installs Go 1.22
3. Clones etcd v3.5.17 (shallow)
4. Builds etcdctl and etcdutl
5. Symlinks rusd as etcd
6. Runs Tiers 1-3
7. Uploads test logs and compatibility report as artifacts
