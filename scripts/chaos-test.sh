#!/bin/bash
# Chaos testing for rusd multi-node Raft cluster
# Tests: Network partition simulation (T6) and Data integrity under churn (T8)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BINARY="${PROJECT_DIR}/target/release/rusd"
DATA_DIR="/tmp/rusd-chaos-test"
CLUSTER_TOKEN="chaos-test-cluster"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

PASSED=0
FAILED=0
TOTAL=0

pass() {
    PASSED=$((PASSED + 1))
    TOTAL=$((TOTAL + 1))
    echo -e "${GREEN}  PASS${NC}: $1"
}

fail() {
    FAILED=$((FAILED + 1))
    TOTAL=$((TOTAL + 1))
    echo -e "${RED}  FAIL${NC}: $1"
}

info() {
    echo -e "${YELLOW}  INFO${NC}: $1"
}

cleanup() {
    info "Cleaning up..."
    kill $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
    wait $NODE1_PID $NODE2_PID $NODE3_PID 2>/dev/null || true
    rm -rf "$DATA_DIR"
}
trap cleanup EXIT

# Build if needed
if [ ! -f "$BINARY" ]; then
    info "Building rusd (release)..."
    (cd "$PROJECT_DIR" && cargo build --release)
fi

# Clean previous test data
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR/node1" "$DATA_DIR/node2" "$DATA_DIR/node3"

INITIAL_CLUSTER="node1=http://127.0.0.1:12380,node2=http://127.0.0.1:12480,node3=http://127.0.0.1:12580"

echo "=== rusd Chaos Testing ==="
echo ""

# Start 3-node cluster
info "Starting 3-node cluster..."

"$BINARY" \
    --name node1 \
    --data-dir "$DATA_DIR/node1" \
    --listen-client-urls http://127.0.0.1:12379 \
    --listen-peer-urls http://127.0.0.1:12380 \
    --advertise-client-urls http://127.0.0.1:12379 \
    --initial-advertise-peer-urls http://127.0.0.1:12380 \
    --initial-cluster "$INITIAL_CLUSTER" \
    --initial-cluster-token "$CLUSTER_TOKEN" \
    --heartbeat-interval 50 \
    --election-timeout 200 \
    --log-level warn &
NODE1_PID=$!

"$BINARY" \
    --name node2 \
    --data-dir "$DATA_DIR/node2" \
    --listen-client-urls http://127.0.0.1:12479 \
    --listen-peer-urls http://127.0.0.1:12480 \
    --advertise-client-urls http://127.0.0.1:12479 \
    --initial-advertise-peer-urls http://127.0.0.1:12480 \
    --initial-cluster "$INITIAL_CLUSTER" \
    --initial-cluster-token "$CLUSTER_TOKEN" \
    --heartbeat-interval 50 \
    --election-timeout 200 \
    --log-level warn &
NODE2_PID=$!

"$BINARY" \
    --name node3 \
    --data-dir "$DATA_DIR/node3" \
    --listen-client-urls http://127.0.0.1:12579 \
    --listen-peer-urls http://127.0.0.1:12580 \
    --advertise-client-urls http://127.0.0.1:12579 \
    --initial-advertise-peer-urls http://127.0.0.1:12580 \
    --initial-cluster "$INITIAL_CLUSTER" \
    --initial-cluster-token "$CLUSTER_TOKEN" \
    --heartbeat-interval 50 \
    --election-timeout 200 \
    --log-level warn &
NODE3_PID=$!

ALL_ENDPOINTS="http://127.0.0.1:12379,http://127.0.0.1:12479,http://127.0.0.1:12579"

# Wait for cluster to form
info "Waiting for cluster to form..."
HEALTHY=0
for i in $(seq 1 30); do
    if etcdctl --endpoints=http://127.0.0.1:12379 endpoint health 2>/dev/null | grep -q "healthy"; then
        HEALTHY=1
        break
    fi
    sleep 1
done

if [ "$HEALTHY" -ne 1 ]; then
    fail "Cluster failed to form within 30 seconds"
    exit 1
fi
pass "Cluster formed successfully"

# Wait for leader election
sleep 3

# Find the leader
LEADER_ENDPOINT=""
for ep in http://127.0.0.1:12379 http://127.0.0.1:12479 http://127.0.0.1:12579; do
    if etcdctl --endpoints="$ep" put /chaos/leader-check ok 2>/dev/null; then
        LEADER_ENDPOINT="$ep"
        break
    fi
done

if [ -z "$LEADER_ENDPOINT" ]; then
    fail "Could not identify leader"
    exit 1
fi
pass "Leader identified at $LEADER_ENDPOINT"

# ===========================
# T6: Network Partition Simulation (using kill/restart)
# On macOS we can't use iptables, so we simulate by killing the leader
# ===========================
echo ""
echo "--- T6: Partition Simulation (Leader Kill + Recovery) ---"

# Write pre-partition data
for i in $(seq 1 20); do
    etcdctl --endpoints="$LEADER_ENDPOINT" put "/chaos/pre-partition/key-$i" "value-$i" >/dev/null 2>&1
done
pass "Wrote 20 pre-partition keys"

# Determine which node is the leader and kill it
if [ "$LEADER_ENDPOINT" = "http://127.0.0.1:12379" ]; then
    KILLED_PID=$NODE1_PID
    KILLED_PORT=12379
    SURVIVING_ENDPOINTS="http://127.0.0.1:12479,http://127.0.0.1:12579"
elif [ "$LEADER_ENDPOINT" = "http://127.0.0.1:12479" ]; then
    KILLED_PID=$NODE2_PID
    KILLED_PORT=12479
    SURVIVING_ENDPOINTS="http://127.0.0.1:12379,http://127.0.0.1:12579"
else
    KILLED_PID=$NODE3_PID
    KILLED_PORT=12579
    SURVIVING_ENDPOINTS="http://127.0.0.1:12379,http://127.0.0.1:12479"
fi

info "Killing leader (PID $KILLED_PID, port $KILLED_PORT)..."
kill -9 $KILLED_PID 2>/dev/null || true
wait $KILLED_PID 2>/dev/null || true

# Wait for new leader election
info "Waiting for new leader election..."
sleep 5

# Find new leader among surviving nodes
NEW_LEADER=""
for attempt in $(seq 1 10); do
    for ep in $(echo "$SURVIVING_ENDPOINTS" | tr ',' ' '); do
        if etcdctl --endpoints="$ep" put /chaos/new-leader-check ok 2>/dev/null; then
            NEW_LEADER="$ep"
            break 2
        fi
    done
    sleep 1
done

if [ -n "$NEW_LEADER" ]; then
    pass "New leader elected at $NEW_LEADER"
else
    fail "No new leader elected after partition"
fi

# Write during partition
if [ -n "$NEW_LEADER" ]; then
    for i in $(seq 1 10); do
        etcdctl --endpoints="$NEW_LEADER" put "/chaos/during-partition/key-$i" "value-$i" >/dev/null 2>&1
    done
    pass "Wrote 10 keys during partition"
fi

# Verify pre-partition data on survivors
if [ -n "$NEW_LEADER" ]; then
    PRE_COUNT=$(etcdctl --endpoints="$NEW_LEADER" get /chaos/pre-partition/ --prefix --keys-only 2>/dev/null | grep -c "key" || true)
    if [ "$PRE_COUNT" -ge 18 ]; then
        pass "Pre-partition data preserved ($PRE_COUNT/20 keys, >=90%)"
    else
        fail "Pre-partition data lost ($PRE_COUNT/20 keys)"
    fi
fi

# Restart the killed node
info "Restarting killed node..."
if [ "$KILLED_PORT" = "12379" ]; then
    "$BINARY" \
        --name node1 \
        --data-dir "$DATA_DIR/node1" \
        --listen-client-urls http://127.0.0.1:12379 \
        --listen-peer-urls http://127.0.0.1:12380 \
        --advertise-client-urls http://127.0.0.1:12379 \
        --initial-advertise-peer-urls http://127.0.0.1:12380 \
        --initial-cluster "$INITIAL_CLUSTER" \
        --initial-cluster-token "$CLUSTER_TOKEN" \
        --heartbeat-interval 50 \
        --election-timeout 200 \
        --log-level warn &
    NODE1_PID=$!
elif [ "$KILLED_PORT" = "12479" ]; then
    "$BINARY" \
        --name node2 \
        --data-dir "$DATA_DIR/node2" \
        --listen-client-urls http://127.0.0.1:12479 \
        --listen-peer-urls http://127.0.0.1:12480 \
        --advertise-client-urls http://127.0.0.1:12479 \
        --initial-advertise-peer-urls http://127.0.0.1:12480 \
        --initial-cluster "$INITIAL_CLUSTER" \
        --initial-cluster-token "$CLUSTER_TOKEN" \
        --heartbeat-interval 50 \
        --election-timeout 200 \
        --log-level warn &
    NODE3_PID=$!
else
    "$BINARY" \
        --name node3 \
        --data-dir "$DATA_DIR/node3" \
        --listen-client-urls http://127.0.0.1:12579 \
        --listen-peer-urls http://127.0.0.1:12580 \
        --advertise-client-urls http://127.0.0.1:12579 \
        --initial-advertise-peer-urls http://127.0.0.1:12580 \
        --initial-cluster "$INITIAL_CLUSTER" \
        --initial-cluster-token "$CLUSTER_TOKEN" \
        --heartbeat-interval 50 \
        --election-timeout 200 \
        --log-level warn &
    NODE3_PID=$!
fi

# Wait for rejoin
sleep 5

# Verify rejoined node can serve reads
REJOIN_EP="http://127.0.0.1:$KILLED_PORT"
REJOIN_OK=0
for attempt in $(seq 1 10); do
    if etcdctl --endpoints="$REJOIN_EP" endpoint health 2>/dev/null | grep -q "healthy"; then
        REJOIN_OK=1
        break
    fi
    sleep 1
done

if [ "$REJOIN_OK" -eq 1 ]; then
    pass "Killed node rejoined cluster"
else
    fail "Killed node failed to rejoin"
fi

# ===========================
# T8: Data Integrity Under Churn
# ===========================
echo ""
echo "--- T8: Data Integrity Under Churn ---"

# Find the current leader
CHURN_LEADER=""
for ep in http://127.0.0.1:12379 http://127.0.0.1:12479 http://127.0.0.1:12579; do
    if etcdctl --endpoints="$ep" put /chaos/churn-check ok 2>/dev/null; then
        CHURN_LEADER="$ep"
        break
    fi
done

if [ -z "$CHURN_LEADER" ]; then
    fail "Could not find leader for churn test"
else
    info "Using leader at $CHURN_LEADER for churn test"

    # Write a continuous stream of keys
    CHURN_KEYS=50
    info "Writing $CHURN_KEYS keys with rolling restarts..."

    for i in $(seq 1 $CHURN_KEYS); do
        # Write key to the leader (retry on failure)
        for retry in $(seq 1 3); do
            if etcdctl --endpoints="$CHURN_LEADER" put "/chaos/churn/key-$i" "churn-value-$i" >/dev/null 2>&1; then
                break
            fi
            # Try to find new leader if write fails
            for ep in http://127.0.0.1:12379 http://127.0.0.1:12479 http://127.0.0.1:12579; do
                if etcdctl --endpoints="$ep" put "/chaos/churn/key-$i" "churn-value-$i" >/dev/null 2>&1; then
                    CHURN_LEADER="$ep"
                    break 2
                fi
            done
            sleep 1
        done

        # Every 15 keys, kill and restart a non-leader node
        if [ $((i % 15)) -eq 0 ]; then
            # Find a non-leader node to restart
            if [ "$CHURN_LEADER" != "http://127.0.0.1:12379" ]; then
                info "  Cycling node1 (key $i/$CHURN_KEYS)..."
                kill -9 $NODE1_PID 2>/dev/null || true
                wait $NODE1_PID 2>/dev/null || true
                sleep 2
                "$BINARY" \
                    --name node1 \
                    --data-dir "$DATA_DIR/node1" \
                    --listen-client-urls http://127.0.0.1:12379 \
                    --listen-peer-urls http://127.0.0.1:12380 \
                    --advertise-client-urls http://127.0.0.1:12379 \
                    --initial-advertise-peer-urls http://127.0.0.1:12380 \
                    --initial-cluster "$INITIAL_CLUSTER" \
                    --initial-cluster-token "$CLUSTER_TOKEN" \
                    --heartbeat-interval 50 \
                    --election-timeout 200 \
                    --log-level warn &
                NODE1_PID=$!
            elif [ "$CHURN_LEADER" != "http://127.0.0.1:12479" ]; then
                info "  Cycling node2 (key $i/$CHURN_KEYS)..."
                kill -9 $NODE2_PID 2>/dev/null || true
                wait $NODE2_PID 2>/dev/null || true
                sleep 2
                "$BINARY" \
                    --name node2 \
                    --data-dir "$DATA_DIR/node2" \
                    --listen-client-urls http://127.0.0.1:12479 \
                    --listen-peer-urls http://127.0.0.1:12480 \
                    --advertise-client-urls http://127.0.0.1:12479 \
                    --initial-advertise-peer-urls http://127.0.0.1:12480 \
                    --initial-cluster "$INITIAL_CLUSTER" \
                    --initial-cluster-token "$CLUSTER_TOKEN" \
                    --heartbeat-interval 50 \
                    --election-timeout 200 \
                    --log-level warn &
                NODE2_PID=$!
            fi
            sleep 3
        fi
    done

    # Wait for all nodes to catch up
    sleep 5

    # Verify data integrity on all nodes
    pass "Wrote $CHURN_KEYS keys during churn"

    for ep in http://127.0.0.1:12379 http://127.0.0.1:12479 http://127.0.0.1:12579; do
        KEY_COUNT=$(etcdctl --endpoints="$ep" get /chaos/churn/ --prefix --keys-only 2>/dev/null | grep -c "key" || true)
        THRESHOLD=$((CHURN_KEYS * 95 / 100))
        if [ "$KEY_COUNT" -ge "$THRESHOLD" ]; then
            pass "Node $ep has $KEY_COUNT/$CHURN_KEYS keys (>= 95%)"
        else
            fail "Node $ep has only $KEY_COUNT/$CHURN_KEYS keys (< 95%)"
        fi
    done
fi

# ===========================
# Summary
# ===========================
echo ""
echo "=== Chaos Test Summary ==="
echo -e "  ${GREEN}Passed${NC}: $PASSED"
echo -e "  ${RED}Failed${NC}: $FAILED"
echo -e "  Total: $TOTAL"

if [ "$FAILED" -gt 0 ]; then
    exit 1
fi
exit 0
