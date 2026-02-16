#!/bin/bash
# ==============================================================================
# rusd Multi-Node Cluster Test Script
# ==============================================================================
#
# This script starts a 3-node rusd cluster on a single machine using different
# ports, then validates leader election, log replication, failover, and data
# consistency.
#
# Usage:
#   ./scripts/multi-node-test.sh
#
# Environment Variables:
#   RUSD_BINARY  - Path to the rusd binary (default: ./target/release/rusd)
#   ETCDCTL      - Path to etcdctl (default: etcdctl from PATH)
#   VERBOSE      - Set to 1 for verbose output
#
# Exit Codes:
#   0 - All tests passed
#   1 - One or more tests failed
#   2 - Prerequisites not met
#
# Multi-node Raft is fully implemented:
#   - Raft gRPC transport (peer-to-peer communication)
#   - Leader election with majority vote
#   - Log replication with commit-wait
#   - Follower state machine apply via apply channel
#   - Deterministic member IDs from name + cluster token
# ==============================================================================

set -euo pipefail

# Configuration
RUSD_BINARY="${RUSD_BINARY:-./target/release/rusd}"
ETCDCTL="${ETCDCTL:-etcdctl}"
TEST_DIR="/tmp/rusd-test"
VERBOSE="${VERBOSE:-0}"

# Node configurations
declare -a NODE_NAMES=("rusd1" "rusd2" "rusd3")
declare -a CLIENT_PORTS=(2379 2479 2579)
declare -a PEER_PORTS=(2380 2480 2580)
declare -a NODE_PIDS=()

# Test counters
TESTS_TOTAL=0
TESTS_PASSED=0
TESTS_FAILED=0

# Color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

# ==============================================================================
# Logging Functions
# ==============================================================================

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_test() {
    echo -e "${BLUE}[TEST]${NC} $1"
}

log_pass() {
    echo -e "${GREEN}[PASS]${NC} $1"
    TESTS_PASSED=$((TESTS_PASSED + 1))
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $1"
    TESTS_FAILED=$((TESTS_FAILED + 1))
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_section() {
    echo ""
    echo -e "${CYAN}=====================================================${NC}"
    echo -e "${CYAN}  $1${NC}"
    echo -e "${CYAN}=====================================================${NC}"
    echo ""
}

log_verbose() {
    if [ "$VERBOSE" = "1" ]; then
        echo -e "${YELLOW}[DEBUG]${NC} $1"
    fi
}

# ==============================================================================
# Cleanup
# ==============================================================================

cleanup() {
    log_info "Cleaning up..."

    # Kill all rusd processes we started
    for pid in "${NODE_PIDS[@]}"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            log_verbose "Killing rusd process $pid"
            kill "$pid" 2>/dev/null || true
        fi
    done

    # Wait for processes to exit
    for pid in "${NODE_PIDS[@]}"; do
        if [ -n "$pid" ]; then
            wait "$pid" 2>/dev/null || true
        fi
    done

    # Remove temp data (keep logs for debugging)
    for name in "${NODE_NAMES[@]}"; do
        rm -rf "${TEST_DIR}/${name}/data"
    done

    log_info "Cleanup complete"
}

trap cleanup EXIT

# ==============================================================================
# Prerequisites
# ==============================================================================

check_prerequisites() {
    log_section "Checking Prerequisites"

    # Check rusd binary
    if [ ! -f "$RUSD_BINARY" ]; then
        log_warn "rusd binary not found at $RUSD_BINARY"
        log_info "Building rusd..."
        if ! cargo build --release 2>&1; then
            echo "Failed to build rusd"
            exit 2
        fi
    fi

    if [ ! -x "$RUSD_BINARY" ]; then
        chmod +x "$RUSD_BINARY"
    fi
    log_info "rusd binary: $RUSD_BINARY"

    # Check etcdctl
    if ! command -v "$ETCDCTL" &>/dev/null; then
        echo "etcdctl not found. Please install etcd client tools."
        echo "  macOS:  brew install etcd"
        echo "  Ubuntu: See https://github.com/etcd-io/etcd/releases"
        exit 2
    fi
    log_info "etcdctl: $($ETCDCTL version 2>&1 | head -1)"

    # Check that ports are available
    for port in "${CLIENT_PORTS[@]}" "${PEER_PORTS[@]}"; do
        if ss -tlnp 2>/dev/null | grep -q ":${port} " || \
           lsof -i ":${port}" &>/dev/null 2>&1; then
            echo "Port $port is already in use. Cannot start test cluster."
            exit 2
        fi
    done
    log_info "All required ports are available"

    # Create test directory
    rm -rf "$TEST_DIR"
    mkdir -p "$TEST_DIR"
    log_info "Test directory: $TEST_DIR"
}

# ==============================================================================
# Cluster Management
# ==============================================================================

# Build the --initial-cluster string for all nodes
build_initial_cluster() {
    local cluster=""
    for i in "${!NODE_NAMES[@]}"; do
        if [ -n "$cluster" ]; then
            cluster="${cluster},"
        fi
        cluster="${cluster}${NODE_NAMES[$i]}=http://127.0.0.1:${PEER_PORTS[$i]}"
    done
    echo "$cluster"
}

# Start a single rusd node
start_node() {
    local index=$1
    local name="${NODE_NAMES[$index]}"
    local client_port="${CLIENT_PORTS[$index]}"
    local peer_port="${PEER_PORTS[$index]}"
    local data_dir="${TEST_DIR}/${name}/data"
    local log_file="${TEST_DIR}/${name}.log"
    local initial_cluster
    initial_cluster=$(build_initial_cluster)

    mkdir -p "$data_dir"

    log_verbose "Starting $name on client=$client_port peer=$peer_port"

    "$RUSD_BINARY" \
        --name "$name" \
        --data-dir "$data_dir" \
        --listen-client-urls "http://0.0.0.0:${client_port}" \
        --listen-peer-urls "http://0.0.0.0:${peer_port}" \
        --advertise-client-urls "http://127.0.0.1:${client_port}" \
        --initial-advertise-peer-urls "http://127.0.0.1:${peer_port}" \
        --initial-cluster "$initial_cluster" \
        --initial-cluster-state new \
        --initial-cluster-token "rusd-multi-test" \
        --heartbeat-interval 100 \
        --election-timeout 1000 \
        --log-level info \
        > "$log_file" 2>&1 &

    local pid=$!
    NODE_PIDS[$index]=$pid
    log_info "Started $name (PID: $pid, client: $client_port, peer: $peer_port)"
}

# Start all nodes in the cluster
start_cluster() {
    log_section "Starting 3-Node Cluster"

    for i in "${!NODE_NAMES[@]}"; do
        start_node "$i"
    done

    log_info "All nodes started. Waiting for cluster to form..."
}

# Wait for a specific node to be healthy
wait_for_node() {
    local index=$1
    local port="${CLIENT_PORTS[$index]}"
    local name="${NODE_NAMES[$index]}"
    local max_wait=${2:-30}

    for i in $(seq 1 "$max_wait"); do
        if $ETCDCTL --endpoints="http://127.0.0.1:${port}" endpoint health 2>/dev/null; then
            log_verbose "$name is healthy"
            return 0
        fi
        sleep 1
    done

    log_warn "$name did not become healthy within ${max_wait}s"
    return 1
}

# Wait for all nodes to be healthy
wait_for_cluster() {
    local max_wait=${1:-30}
    local healthy_count=0

    log_info "Waiting up to ${max_wait}s for cluster health..."

    for i in "${!NODE_NAMES[@]}"; do
        if wait_for_node "$i" "$max_wait"; then
            healthy_count=$((healthy_count + 1))
        fi
    done

    if [ "$healthy_count" -eq "${#NODE_NAMES[@]}" ]; then
        log_info "All ${#NODE_NAMES[@]} nodes are healthy"
        return 0
    else
        log_warn "Only $healthy_count/${#NODE_NAMES[@]} nodes are healthy"
        return 1
    fi
}

# Wait for a leader to be elected (multi-node clusters need election timeout)
wait_for_leader() {
    local max_wait=${1:-15}
    local endpoints
    endpoints=$(all_endpoints)

    log_info "Waiting up to ${max_wait}s for leader election..."

    for i in $(seq 1 "$max_wait"); do
        local status_output
        status_output=$($ETCDCTL --endpoints="$endpoints" endpoint status --write-out=json 2>/dev/null || true)

        if [ -n "$status_output" ]; then
            local leader_count
            leader_count=$(echo "$status_output" | python3 -c "
import json, sys
data = json.load(sys.stdin)
leaders = [e for e in data if e.get('Status', {}).get('leader', 0) == e.get('Status', {}).get('header', {}).get('member_id', -1)]
print(len(leaders))
" 2>/dev/null || echo "0")

            if [ "$leader_count" = "1" ]; then
                log_info "Leader elected after ${i}s"
                return 0
            fi
        fi
        sleep 1
    done

    log_warn "No leader elected within ${max_wait}s"
    return 1
}

# Build the endpoints string for etcdctl
all_endpoints() {
    local endpoints=""
    for port in "${CLIENT_PORTS[@]}"; do
        if [ -n "$endpoints" ]; then
            endpoints="${endpoints},"
        fi
        endpoints="${endpoints}http://127.0.0.1:${port}"
    done
    echo "$endpoints"
}

# Get the leader endpoint
# Returns the client URL of the leader, or empty string if no leader found
get_leader_endpoint() {
    local endpoints
    endpoints=$(all_endpoints)

    # etcdctl endpoint status returns: endpoint, id, version, db_size, is_leader, ...
    # We look for the line where is_leader is true
    local status_output
    status_output=$($ETCDCTL --endpoints="$endpoints" endpoint status --write-out=table 2>/dev/null || true)

    if [ -z "$status_output" ]; then
        echo ""
        return 1
    fi

    # Parse for leader - look for 'true' in the is_leader column
    local leader_endpoint
    leader_endpoint=$(echo "$status_output" | grep -i "true" | awk -F'|' '{print $2}' | tr -d ' ' | head -1)

    if [ -n "$leader_endpoint" ]; then
        echo "$leader_endpoint"
        return 0
    fi

    echo ""
    return 1
}

# Get the PID of the node running on a given client port
get_pid_for_port() {
    local port=$1
    for i in "${!CLIENT_PORTS[@]}"; do
        if [ "${CLIENT_PORTS[$i]}" = "$port" ]; then
            echo "${NODE_PIDS[$i]}"
            return 0
        fi
    done
    echo ""
    return 1
}

# Get the index of a node by its client port
get_index_for_port() {
    local port=$1
    for i in "${!CLIENT_PORTS[@]}"; do
        if [ "${CLIENT_PORTS[$i]}" = "$port" ]; then
            echo "$i"
            return 0
        fi
    done
    echo ""
    return 1
}

# Extract port from endpoint URL
extract_port() {
    echo "$1" | sed 's|.*:\([0-9]*\)$|\1|'
}

# ==============================================================================
# Test Functions
# ==============================================================================

run_test() {
    local test_name="$1"
    local test_func="$2"
    TESTS_TOTAL=$((TESTS_TOTAL + 1))
    log_test "$test_name"

    if $test_func; then
        log_pass "$test_name"
    else
        log_fail "$test_name"
    fi
}

# ---------- T1: Leader Election ----------

test_leader_election() {
    local endpoints
    endpoints=$(all_endpoints)

    # Check endpoint status
    local status_output
    status_output=$($ETCDCTL --endpoints="$endpoints" endpoint status --write-out=json 2>/dev/null || true)

    if [ -z "$status_output" ]; then
        echo "  Could not get endpoint status"
        return 1
    fi

    log_verbose "Endpoint status: $status_output"

    # Count leaders (expect exactly 1)
    local leader_count
    leader_count=$(echo "$status_output" | python3 -c "
import json, sys
data = json.load(sys.stdin)
leaders = [e for e in data if e.get('Status', {}).get('leader', 0) == e.get('Status', {}).get('header', {}).get('member_id', -1)]
print(len(leaders))
" 2>/dev/null || echo "0")

    if [ "$leader_count" != "1" ]; then
        echo "  Expected exactly 1 leader, found $leader_count"
        return 1
    fi

    # Verify all nodes report the same leader
    local leader_ids
    leader_ids=$(echo "$status_output" | python3 -c "
import json, sys
data = json.load(sys.stdin)
leaders = set(e.get('Status', {}).get('leader', 0) for e in data)
print(' '.join(str(l) for l in leaders))
" 2>/dev/null || echo "")

    local unique_leaders
    unique_leaders=$(echo "$leader_ids" | tr ' ' '\n' | sort -u | wc -l | tr -d ' ')

    if [ "$unique_leaders" != "1" ]; then
        echo "  Nodes disagree on leader: $leader_ids"
        return 1
    fi

    log_verbose "All nodes agree on leader"
    return 0
}

# ---------- T2: Log Replication ----------

test_log_replication() {
    local leader_endpoint
    leader_endpoint=$(get_leader_endpoint)

    if [ -z "$leader_endpoint" ]; then
        echo "  No leader found"
        return 1
    fi

    log_verbose "Writing to leader: $leader_endpoint"

    # Write 100 keys to leader
    for i in $(seq 1 100); do
        if ! $ETCDCTL --endpoints="$leader_endpoint" put "/repl/key$(printf '%03d' $i)" "value${i}" >/dev/null 2>&1; then
            echo "  Failed to write key $i to leader"
            return 1
        fi
    done
    log_verbose "Wrote 100 keys to leader"

    # Wait for replication
    sleep 3

    # Quick sanity check: verify a known key exists on leader before counting
    local sanity_val
    sanity_val=$($ETCDCTL --endpoints="$leader_endpoint" get /repl/key050 --print-value-only 2>/dev/null || echo "")
    if [ "$sanity_val" != "value50" ]; then
        echo "  Sanity check failed: /repl/key050 on leader returned '$sanity_val' (expected 'value50')"
        return 1
    fi
    log_verbose "Sanity check passed: /repl/key050 exists on leader"

    # Read from each node and verify count
    local all_ok=true
    for port in "${CLIENT_PORTS[@]}"; do
        local endpoint="http://127.0.0.1:${port}"
        local count
        count=$($ETCDCTL --endpoints="$endpoint" get /repl/ --prefix --keys-only 2>/dev/null | grep -c '/repl/' || true)

        if [ "$count" -ne 100 ]; then
            echo "  Node on port $port has $count/100 keys (endpoint: $endpoint)"
            all_ok=false
        else
            log_verbose "Node on port $port has all 100 keys"
        fi
    done

    if [ "$all_ok" = false ]; then
        return 1
    fi

    # Spot-check 10 random keys for value correctness on non-leader nodes
    for port in "${CLIENT_PORTS[@]}"; do
        local endpoint="http://127.0.0.1:${port}"
        if [ "$endpoint" = "$leader_endpoint" ]; then
            continue
        fi

        for check_i in 1 10 25 42 50 67 75 88 95 100; do
            local key="/repl/key$(printf '%03d' $check_i)"
            local expected="value${check_i}"
            local actual
            actual=$($ETCDCTL --endpoints="$endpoint" get "$key" --print-value-only 2>/dev/null || echo "")

            if [ "$actual" != "$expected" ]; then
                echo "  Value mismatch on port $port for $key: expected '$expected', got '$actual'"
                return 1
            fi
        done
    done

    log_verbose "All spot-checks passed"
    return 0
}

# ---------- T3: Write to Leader, Read Immediately from Follower ----------

test_read_consistency() {
    local leader_endpoint
    leader_endpoint=$(get_leader_endpoint)

    if [ -z "$leader_endpoint" ]; then
        echo "  No leader found"
        return 1
    fi

    # Find a follower endpoint
    local follower_endpoint=""
    for port in "${CLIENT_PORTS[@]}"; do
        local ep="http://127.0.0.1:${port}"
        if [ "$ep" != "$leader_endpoint" ]; then
            follower_endpoint="$ep"
            break
        fi
    done

    if [ -z "$follower_endpoint" ]; then
        echo "  No follower found"
        return 1
    fi

    log_verbose "Leader: $leader_endpoint, Follower: $follower_endpoint"

    # Write to leader and immediately read from follower (repeat 20 times)
    local mismatches=0
    for i in $(seq 1 20); do
        local key="/consistency/key${i}"
        local val="val-$(date +%s%N)-${i}"

        $ETCDCTL --endpoints="$leader_endpoint" put "$key" "$val" >/dev/null 2>&1
        sleep 0.2  # Small delay for replication

        local read_val
        read_val=$($ETCDCTL --endpoints="$follower_endpoint" get "$key" --print-value-only 2>/dev/null || echo "")

        if [ "$read_val" != "$val" ]; then
            mismatches=$((mismatches + 1))
            log_verbose "Consistency miss on iteration $i: wrote '$val', read '$read_val'"
        fi
    done

    if [ "$mismatches" -gt 2 ]; then
        echo "  Too many consistency mismatches: $mismatches/20"
        return 1
    fi

    if [ "$mismatches" -gt 0 ]; then
        log_warn "  $mismatches/20 reads returned stale data (eventual consistency delay)"
    fi

    return 0
}

# ---------- T4: Leader Failover ----------

test_leader_failover() {
    local leader_endpoint
    leader_endpoint=$(get_leader_endpoint)

    if [ -z "$leader_endpoint" ]; then
        echo "  No leader found before failover"
        return 1
    fi

    local leader_port
    leader_port=$(extract_port "$leader_endpoint")
    local leader_pid
    leader_pid=$(get_pid_for_port "$leader_port")
    local leader_index
    leader_index=$(get_index_for_port "$leader_port")

    if [ -z "$leader_pid" ]; then
        echo "  Could not find PID for leader on port $leader_port"
        return 1
    fi

    log_verbose "Killing leader (PID: $leader_pid) on port $leader_port"
    kill -9 "$leader_pid" 2>/dev/null || true
    wait "$leader_pid" 2>/dev/null || true

    # Wait for new leader election (up to 10 seconds = 2x election timeout)
    log_verbose "Waiting for new leader election..."
    sleep 5

    # Build endpoints excluding the killed node
    local surviving_endpoints=""
    for i in "${!CLIENT_PORTS[@]}"; do
        if [ "$i" -ne "$leader_index" ]; then
            if [ -n "$surviving_endpoints" ]; then
                surviving_endpoints="${surviving_endpoints},"
            fi
            surviving_endpoints="${surviving_endpoints}http://127.0.0.1:${CLIENT_PORTS[$i]}"
        fi
    done

    # Check that a new leader was elected
    local new_leader_endpoint
    new_leader_endpoint=""

    for attempt in $(seq 1 10); do
        new_leader_endpoint=$($ETCDCTL --endpoints="$surviving_endpoints" endpoint status --write-out=table 2>/dev/null | grep -i "true" | awk -F'|' '{print $2}' | tr -d ' ' | head -1 || echo "")

        if [ -n "$new_leader_endpoint" ]; then
            break
        fi
        sleep 1
    done

    if [ -z "$new_leader_endpoint" ]; then
        echo "  No new leader elected after killing leader"
        return 1
    fi

    local new_leader_port
    new_leader_port=$(extract_port "$new_leader_endpoint")

    if [ "$new_leader_port" = "$leader_port" ]; then
        echo "  New leader is on the same port as killed leader -- something is wrong"
        return 1
    fi

    log_verbose "New leader elected on port $new_leader_port"

    # Verify we can still write and read
    if ! $ETCDCTL --endpoints="$new_leader_endpoint" put /failover/test "survived" >/dev/null 2>&1; then
        echo "  Cannot write to new leader after failover"
        return 1
    fi

    local read_val
    read_val=$($ETCDCTL --endpoints="$new_leader_endpoint" get /failover/test --print-value-only 2>/dev/null || echo "")
    if [ "$read_val" != "survived" ]; then
        echo "  Read after failover returned wrong value: $read_val"
        return 1
    fi

    log_verbose "Write and read after failover succeeded"
    return 0
}

# ---------- T5: Data Integrity After Failover ----------

test_data_integrity_after_failover() {
    # The keys written in T2 (/repl/key001 through /repl/key100) should still
    # be present on surviving nodes after the leader kill in T4.

    local surviving_endpoints=""
    for i in "${!CLIENT_PORTS[@]}"; do
        local pid="${NODE_PIDS[$i]}"
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            if [ -n "$surviving_endpoints" ]; then
                surviving_endpoints="${surviving_endpoints},"
            fi
            surviving_endpoints="${surviving_endpoints}http://127.0.0.1:${CLIENT_PORTS[$i]}"
        fi
    done

    if [ -z "$surviving_endpoints" ]; then
        echo "  No surviving endpoints"
        return 1
    fi

    # Use the first surviving endpoint
    local endpoint
    endpoint=$(echo "$surviving_endpoints" | cut -d',' -f1)

    local count
    count=$($ETCDCTL --endpoints="$endpoint" get /repl/ --prefix --keys-only 2>/dev/null | grep -c '/repl/' || true)

    if [ "$count" -lt 100 ]; then
        echo "  Data loss: expected 100 keys under /repl/, found $count"
        return 1
    fi

    log_verbose "All 100 /repl/ keys intact after failover"

    # Verify the failover key is also present (allow replication delay on CI)
    local failover_val=""
    for attempt in 1 2 3 4 5; do
        failover_val=$($ETCDCTL --endpoints="$surviving_endpoints" get /failover/test --print-value-only 2>/dev/null || echo "")
        if [ "$failover_val" = "survived" ]; then
            break
        fi
        log_verbose "Failover key not yet replicated (attempt $attempt/5), waiting 1s..."
        sleep 1
    done
    if [ "$failover_val" != "survived" ]; then
        echo "  Failover key missing or wrong: $failover_val"
        return 1
    fi

    return 0
}

# ---------- T6: Node Rejoin ----------

test_node_rejoin() {
    # Find the killed node and restart it
    local killed_index=""
    for i in "${!NODE_PIDS[@]}"; do
        local pid="${NODE_PIDS[$i]}"
        if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
            killed_index=$i
            break
        fi
    done

    if [ -z "$killed_index" ]; then
        echo "  No killed node found to rejoin"
        return 1
    fi

    log_verbose "Restarting ${NODE_NAMES[$killed_index]}..."
    start_node "$killed_index"
    sleep 5  # Give it time to rejoin and catch up

    # Wait for the restarted node to become healthy
    if ! wait_for_node "$killed_index" 30; then
        echo "  Restarted node did not become healthy"
        return 1
    fi

    # Check that it has the data
    local port="${CLIENT_PORTS[$killed_index]}"
    local endpoint="http://127.0.0.1:${port}"

    # Wait a bit more for log replay/snapshot restore
    sleep 5

    local count
    count=$($ETCDCTL --endpoints="$endpoint" get /repl/ --prefix --keys-only 2>/dev/null | grep -c '/repl/' || true)

    if [ "$count" -lt 100 ]; then
        echo "  Restarted node only has $count/100 replicated keys"
        return 1
    fi

    log_verbose "Restarted node has all replicated data"
    return 0
}

# ---------- T7: Concurrent Writes Under Cluster Stress ----------

test_concurrent_writes() {
    local endpoints
    endpoints=$(all_endpoints)

    local leader_endpoint
    leader_endpoint=$(get_leader_endpoint)

    if [ -z "$leader_endpoint" ]; then
        echo "  No leader found"
        return 1
    fi

    # Write 100 keys rapidly (5 parallel streams of 20 keys each)
    log_verbose "Writing 100 keys in 5 parallel streams..."
    local pids=()
    for stream in $(seq 1 5); do
        (
            for i in $(seq 1 20); do
                $ETCDCTL --endpoints="$leader_endpoint" put "/stress/s${stream}/key${i}" "v${stream}-${i}" >/dev/null 2>&1
            done
        ) &
        pids+=($!)
    done

    # Wait for all streams to complete
    for pid in "${pids[@]}"; do
        wait "$pid" 2>/dev/null || true
    done

    sleep 5  # Wait for replication

    # Count keys on leader first to establish baseline
    local leader_count
    leader_count=$($ETCDCTL --endpoints="$leader_endpoint" get /stress/ --prefix --keys-only 2>/dev/null | grep -c '/stress/' || true)

    if [ "$leader_count" -lt 80 ]; then
        echo "  Leader only accepted $leader_count/100 writes (too many failures)"
        return 1
    fi

    log_verbose "Leader has $leader_count/100 stress test keys"

    # Allow 5% replication lag (recently-restarted nodes may trail slightly)
    local min_replicated=$(( leader_count * 95 / 100 ))

    # Verify all healthy nodes replicated the leader's data
    for port in "${CLIENT_PORTS[@]}"; do
        local ep="http://127.0.0.1:${port}"
        local pid_for_port
        pid_for_port=$(get_pid_for_port "$port")
        if [ -n "$pid_for_port" ] && ! kill -0 "$pid_for_port" 2>/dev/null; then
            continue  # Skip dead nodes
        fi

        local count
        count=$($ETCDCTL --endpoints="$ep" get /stress/ --prefix --keys-only 2>/dev/null | grep -c '/stress/' || true)

        if [ "$count" -lt "$min_replicated" ]; then
            echo "  Node on port $port has $count/$leader_count stress test keys (below 95% threshold: $min_replicated)"
            return 1
        fi
    done

    log_verbose "Stress test: $leader_count keys on leader, all nodes >= $min_replicated (95%)"
    return 0
}

# ==============================================================================
# Main Execution
# ==============================================================================

main() {
    echo ""
    echo "============================================================"
    echo "  rusd Multi-Node Cluster Test Suite"
    echo "============================================================"
    echo ""
    echo "  Nodes:    ${#NODE_NAMES[@]}"
    echo "  Clients:  ${CLIENT_PORTS[*]}"
    echo "  Peers:    ${PEER_PORTS[*]}"
    echo "  Test dir: $TEST_DIR"
    echo ""

    check_prerequisites
    start_cluster

    if ! wait_for_cluster 30; then
        echo ""
        log_warn "Cluster did not fully form. Some tests may fail."
        echo "Node logs are in $TEST_DIR/*.log"
        echo ""
    fi

    if ! wait_for_leader 15; then
        echo ""
        log_warn "No leader elected. Multi-node tests will likely fail."
        echo "Node logs are in $TEST_DIR/*.log"
        echo ""
    fi

    log_section "Running Tests"

    # T1: Leader Election
    run_test "T1: Leader election (exactly one leader)" test_leader_election

    # T2: Log Replication
    run_test "T2: Log replication (100 keys, leader -> followers)" test_log_replication

    # T3: Read Consistency
    run_test "T3: Read consistency (write leader, read follower)" test_read_consistency

    # T4: Leader Failover
    run_test "T4: Leader failover (kill leader, new election)" test_leader_failover

    # T5: Data Integrity After Failover
    run_test "T5: Data integrity after failover (no data loss)" test_data_integrity_after_failover

    # T6: Node Rejoin
    run_test "T6: Node rejoin (restart killed node, verify catch-up)" test_node_rejoin

    # T7: Concurrent Writes
    run_test "T7: Concurrent writes under cluster stress (100 keys, 5 streams)" test_concurrent_writes

    # ==== Results Summary ====

    log_section "Test Results"

    echo "  Total:  $TESTS_TOTAL"
    echo "  Passed: $TESTS_PASSED"
    echo "  Failed: $TESTS_FAILED"
    echo ""

    # Save results as JSON
    cat > "${TEST_DIR}/results.json" <<EOF
{
  "total": $TESTS_TOTAL,
  "passed": $TESTS_PASSED,
  "failed": $TESTS_FAILED,
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "nodes": ${#NODE_NAMES[@]},
  "client_ports": [${CLIENT_PORTS[*]// /,}],
  "peer_ports": [${PEER_PORTS[*]// /,}]
}
EOF

    echo "  Results saved to: ${TEST_DIR}/results.json"
    echo "  Node logs:        ${TEST_DIR}/*.log"
    echo ""

    if [ "$TESTS_FAILED" -gt 0 ]; then
        echo -e "${RED}Some tests failed. Check node logs for details.${NC}"
        echo ""
        echo "To view logs:"
        for name in "${NODE_NAMES[@]}"; do
            echo "  cat ${TEST_DIR}/${name}.log"
        done
        echo ""
        exit 1
    fi

    echo -e "${GREEN}All tests passed.${NC}"
    exit 0
}

main "$@"
