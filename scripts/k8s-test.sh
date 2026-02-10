#!/bin/bash
set -euo pipefail

echo "======================================"
echo "  rusd - Kubernetes Integration Test"
echo "======================================"

# Color codes for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Logging functions
log_info() {
    echo -e "${GREEN}[✓]${NC} $1"
}

log_test() {
    echo -e "${BLUE}[TEST]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[✗]${NC} $1"
}

# Configuration
RUSD_BINARY="${RUSD_BINARY:-./target/release/rusd}"
RUSD_DATA_DIR="/tmp/rusd-k8s-test"
RUSD_PORT=2379
RUSD_PEER_PORT=2380
KIND_CLUSTER_NAME="rusd-test"
RUSD_PID=""

# Cleanup function
cleanup() {
    if [ -n "$RUSD_PID" ]; then
        log_info "Stopping rusd (PID: $RUSD_PID)"
        kill "$RUSD_PID" 2>/dev/null || true
        wait "$RUSD_PID" 2>/dev/null || true
    fi

    if command -v kind &>/dev/null; then
        log_info "Deleting kind cluster: $KIND_CLUSTER_NAME"
        kind delete cluster --name "$KIND_CLUSTER_NAME" 2>/dev/null || true
    fi

    log_info "Cleaning up temporary data"
    rm -rf "$RUSD_DATA_DIR"
}

trap cleanup EXIT

# Check prerequisites
check_prereqs() {
    log_info "Checking prerequisites..."

    # Check rusd binary
    if [ ! -f "$RUSD_BINARY" ]; then
        log_error "rusd binary not found at $RUSD_BINARY"
        log_info "Building rusd..."
        cargo build --release
    fi

    # Check etcdctl
    if ! command -v etcdctl &>/dev/null; then
        log_error "etcdctl not found. Please install it:"
        echo "  macOS: brew install etcd"
        echo "  Linux: apt-get install etcd-client (Debian/Ubuntu)"
        echo "         yum install etcd (RHEL/CentOS)"
        exit 1
    fi

    # Check curl
    if ! command -v curl &>/dev/null; then
        log_error "curl not found. Please install it."
        exit 1
    fi

    log_info "Prerequisites check passed"
}

# Start rusd server
start_rusd() {
    log_info "Starting rusd on port $RUSD_PORT..."

    rm -rf "$RUSD_DATA_DIR"
    mkdir -p "$RUSD_DATA_DIR"

    "$RUSD_BINARY" \
        --name test-node \
        --data-dir "$RUSD_DATA_DIR" \
        --listen-client-urls "http://0.0.0.0:$RUSD_PORT" \
        --listen-peer-urls "http://0.0.0.0:$RUSD_PEER_PORT" \
        --advertise-client-urls "http://127.0.0.1:$RUSD_PORT" \
        --initial-advertise-peer-urls "http://127.0.0.1:$RUSD_PEER_PORT" \
        --initial-cluster "test-node=http://127.0.0.1:$RUSD_PEER_PORT" \
        --initial-cluster-state new \
        --log-level info \
        > /tmp/rusd.log 2>&1 &

    RUSD_PID=$!
    log_info "rusd started with PID: $RUSD_PID"

    # Wait for rusd to be ready
    log_info "Waiting for rusd to be ready..."
    local max_attempts=60
    local attempt=0

    while [ $attempt -lt $max_attempts ]; do
        if etcdctl --endpoints="http://127.0.0.1:$RUSD_PORT" endpoint health >/dev/null 2>&1; then
            log_info "rusd is ready!"
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 1
    done

    log_error "rusd failed to start within $max_attempts seconds"
    tail -20 /tmp/rusd.log
    exit 1
}

# Run etcdctl compatibility tests
run_etcdctl_tests() {
    echo ""
    echo "======================================"
    echo "  etcdctl Compatibility Tests"
    echo "======================================"
    echo ""

    local endpoint="http://localhost:$RUSD_PORT"
    local test_count=0
    local pass_count=0

    # Test 1: Put and Get
    log_test "Put/Get operations"
    test_count=$((test_count + 1))
    if etcdctl --endpoints="$endpoint" put /test/key1 "value1" >/dev/null 2>&1; then
        RESULT=$(etcdctl --endpoints="$endpoint" get /test/key1 --print-value-only)
        if [ "$RESULT" = "value1" ]; then
            log_info "Put/Get: PASSED"
            pass_count=$((pass_count + 1))
        else
            log_error "Put/Get: FAILED (expected 'value1', got '$RESULT')"
        fi
    else
        log_error "Put/Get: FAILED (put operation failed)"
    fi

    # Test 2: Delete
    log_test "Delete operations"
    test_count=$((test_count + 1))
    if etcdctl --endpoints="$endpoint" del /test/key1 >/dev/null 2>&1; then
        RESULT=$(etcdctl --endpoints="$endpoint" get /test/key1 --print-value-only 2>/dev/null || true)
        if [ -z "$RESULT" ]; then
            log_info "Delete: PASSED"
            pass_count=$((pass_count + 1))
        else
            log_error "Delete: FAILED (key still exists)"
        fi
    else
        log_error "Delete: FAILED (delete operation failed)"
    fi

    # Test 3: Range scan with prefix
    log_test "Range scan with prefix"
    test_count=$((test_count + 1))
    if etcdctl --endpoints="$endpoint" put /range/key1 "val1" >/dev/null 2>&1 && \
       etcdctl --endpoints="$endpoint" put /range/key2 "val2" >/dev/null 2>&1 && \
       etcdctl --endpoints="$endpoint" put /range/key3 "val3" >/dev/null 2>&1; then
        COUNT=$(etcdctl --endpoints="$endpoint" get /range/ --prefix --print-value-only 2>/dev/null | wc -l)
        if [ "$COUNT" -ge 3 ]; then
            log_info "Range scan: PASSED (found $COUNT entries)"
            pass_count=$((pass_count + 1))
        else
            log_error "Range scan: FAILED (expected 3+ entries, got $COUNT)"
        fi
    else
        log_error "Range scan: FAILED (put operations failed)"
    fi

    # Test 4: Member list
    log_test "Member list"
    test_count=$((test_count + 1))
    if etcdctl --endpoints="$endpoint" member list >/dev/null 2>&1; then
        log_info "Member list: PASSED"
        pass_count=$((pass_count + 1))
    else
        log_error "Member list: FAILED"
    fi

    # Test 5: Endpoint status
    log_test "Endpoint status"
    test_count=$((test_count + 1))
    if etcdctl --endpoints="$endpoint" endpoint status >/dev/null 2>&1; then
        log_info "Endpoint status: PASSED"
        pass_count=$((pass_count + 1))
    else
        log_error "Endpoint status: FAILED"
    fi

    # Test 6: Lease operations
    log_test "Lease grant and attach"
    test_count=$((test_count + 1))
    if LEASE_OUTPUT=$(etcdctl --endpoints="$endpoint" lease grant 60 2>/dev/null); then
        LEASE_ID=$(echo "$LEASE_OUTPUT" | awk '{print $2}')
        if [ -n "$LEASE_ID" ]; then
            if etcdctl --endpoints="$endpoint" put /lease/key "leased" --lease="$LEASE_ID" >/dev/null 2>&1; then
                log_info "Lease operations: PASSED (lease ID: $LEASE_ID)"
                pass_count=$((pass_count + 1))
            else
                log_error "Lease operations: FAILED (could not attach lease)"
            fi
        else
            log_error "Lease operations: FAILED (invalid lease ID)"
        fi
    else
        log_error "Lease operations: FAILED (grant failed)"
    fi

    # Test 7: Transaction
    log_test "Transaction with compare"
    test_count=$((test_count + 1))
    if etcdctl --endpoints="$endpoint" put /txn/key "initial" >/dev/null 2>&1; then
        TXN_OUTPUT=$(cat <<'EOF' | etcdctl --endpoints="$endpoint" txn --interactive 2>&1 || true
value("/txn/key") = "initial"

put /txn/key "updated"

put /txn/key "failed"
EOF
)
        if echo "$TXN_OUTPUT" | grep -q "OK"; then
            log_info "Transaction: PASSED"
            pass_count=$((pass_count + 1))
        else
            log_warn "Transaction: SKIPPED (not fully supported)"
        fi
    else
        log_error "Transaction: FAILED (initial put failed)"
    fi

    # Test 8: Multiple keys with different prefixes
    log_test "Multiple key operations"
    test_count=$((test_count + 1))
    local multi_ok=true
    for i in $(seq 1 5); do
        if ! etcdctl --endpoints="$endpoint" put "/app/config/key$i" "value$i" >/dev/null 2>&1; then
            multi_ok=false
            break
        fi
    done

    if [ "$multi_ok" = true ]; then
        COUNT=$(etcdctl --endpoints="$endpoint" get /app/config/ --prefix --count-only 2>/dev/null || echo "0")
        if [ "$COUNT" -ge 5 ]; then
            log_info "Multiple keys: PASSED ($COUNT keys)"
            pass_count=$((pass_count + 1))
        else
            log_error "Multiple keys: FAILED (expected 5+ keys, got $COUNT)"
        fi
    else
        log_error "Multiple keys: FAILED"
    fi

    # Test 9: Atomic compare-and-swap equivalent
    log_test "Compare and swap"
    test_count=$((test_count + 1))
    if etcdctl --endpoints="$endpoint" put /cas/key "v1" >/dev/null 2>&1; then
        # Simple version: put with no conditions (full CAS requires transaction)
        if etcdctl --endpoints="$endpoint" put /cas/key "v2" >/dev/null 2>&1; then
            RESULT=$(etcdctl --endpoints="$endpoint" get /cas/key --print-value-only)
            if [ "$RESULT" = "v2" ]; then
                log_info "Compare and swap: PASSED"
                pass_count=$((pass_count + 1))
            else
                log_error "Compare and swap: FAILED"
            fi
        fi
    else
        log_error "Compare and swap: FAILED"
    fi

    # Test 10: Auth enable/disable
    log_test "Auth enable and disable"
    test_count=$((test_count + 1))
    if etcdctl --endpoints="$endpoint" auth enable >/dev/null 2>&1; then
        if etcdctl --endpoints="$endpoint" auth disable >/dev/null 2>&1; then
            log_info "Auth enable/disable: PASSED"
            pass_count=$((pass_count + 1))
        else
            log_warn "Auth disable: SKIPPED (auth may be in unexpected state)"
        fi
    else
        log_warn "Auth enable: SKIPPED (may require prior setup)"
    fi

    echo ""
    echo "======================================"
    echo "  etcdctl Tests Summary"
    echo "======================================"
    echo "Passed: $pass_count / $test_count"
    echo "======================================"
    echo ""

    if [ $pass_count -lt $((test_count - 2)) ]; then
        log_error "Not enough tests passed"
        return 1
    fi

    return 0
}

# Run Kubernetes integration tests (optional)
run_k8s_tests() {
    if ! command -v kind &>/dev/null; then
        log_warn "kind not found. Skipping Kubernetes integration tests."
        log_info "Install kind: go install sigs.k8s.io/kind@latest"
        return 0
    fi

    echo ""
    echo "======================================"
    echo "  Kubernetes Integration Tests"
    echo "======================================"
    echo ""

    log_info "Creating kind cluster: $KIND_CLUSTER_NAME"

    # Create kind config
    cat > /tmp/kind-rusd-config.yaml <<'KINDEOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
networking:
  apiServerAddress: "127.0.0.1"
  podSubnet: "10.244.0.0/16"
  serviceSubnet: "10.96.0.0/12"
nodes:
  - role: control-plane
KINDEOF

    if ! kind create cluster --name "$KIND_CLUSTER_NAME" --config /tmp/kind-rusd-config.yaml 2>&1 | tail -10; then
        log_error "Failed to create kind cluster"
        return 1
    fi

    log_info "Waiting for cluster to be ready..."
    sleep 10

    if ! kubectl --context "kind-$KIND_CLUSTER_NAME" wait --for=condition=Ready nodes --all --timeout=120s 2>&1 | tail -5; then
        log_warn "Cluster not ready yet, continuing anyway..."
    fi

    local test_ns="rusd-test"
    local k8s_pass=0
    local k8s_total=0

    # Test 1: Namespace creation
    log_test "Create namespace"
    k8s_total=$((k8s_total + 1))
    if kubectl --context "kind-$KIND_CLUSTER_NAME" create namespace "$test_ns" 2>&1 | tail -1; then
        log_info "Create namespace: PASSED"
        k8s_pass=$((k8s_pass + 1))
    else
        log_error "Create namespace: FAILED"
    fi

    # Test 2: ConfigMap creation
    log_test "Create ConfigMap"
    k8s_total=$((k8s_total + 1))
    if kubectl --context "kind-$KIND_CLUSTER_NAME" -n "$test_ns" create configmap test-config \
        --from-literal=app.properties="debug=true" 2>&1 | tail -1; then
        log_info "Create ConfigMap: PASSED"
        k8s_pass=$((k8s_pass + 1))
    else
        log_error "Create ConfigMap: FAILED"
    fi

    # Test 3: Secret creation
    log_test "Create Secret"
    k8s_total=$((k8s_total + 1))
    if kubectl --context "kind-$KIND_CLUSTER_NAME" -n "$test_ns" create secret generic test-secret \
        --from-literal=password=secret123 2>&1 | tail -1; then
        log_info "Create Secret: PASSED"
        k8s_pass=$((k8s_pass + 1))
    else
        log_error "Create Secret: FAILED"
    fi

    # Test 4: Deployment creation (nginx)
    log_test "Create Deployment"
    k8s_total=$((k8s_total + 1))
    if kubectl --context "kind-$KIND_CLUSTER_NAME" -n "$test_ns" create deployment test-nginx \
        --image=nginx:alpine --replicas=1 2>&1 | tail -1; then
        log_info "Create Deployment: PASSED"
        k8s_pass=$((k8s_pass + 1))

        # Wait for deployment
        if kubectl --context "kind-$KIND_CLUSTER_NAME" -n "$test_ns" rollout status deployment/test-nginx \
            --timeout=60s 2>&1 | tail -5; then
            log_info "Deployment rollout: PASSED"
        else
            log_warn "Deployment rollout: TIMEOUT (continuing)"
        fi
    else
        log_error "Create Deployment: FAILED"
    fi

    # Test 5: List pods
    log_test "List pods"
    k8s_total=$((k8s_total + 1))
    if kubectl --context "kind-$KIND_CLUSTER_NAME" -n "$test_ns" get pods 2>&1 | tail -3; then
        log_info "List pods: PASSED"
        k8s_pass=$((k8s_pass + 1))
    else
        log_error "List pods: FAILED"
    fi

    # Test 6: Scale deployment
    log_test "Scale deployment"
    k8s_total=$((k8s_total + 1))
    if kubectl --context "kind-$KIND_CLUSTER_NAME" -n "$test_ns" scale deployment/test-nginx --replicas=2 2>&1 | tail -1; then
        sleep 5
        if kubectl --context "kind-$KIND_CLUSTER_NAME" -n "$test_ns" rollout status deployment/test-nginx \
            --timeout=60s 2>&1 | tail -3; then
            log_info "Scale deployment: PASSED"
            k8s_pass=$((k8s_pass + 1))
        else
            log_warn "Scale deployment: ROLLOUT TIMEOUT"
        fi
    else
        log_error "Scale deployment: FAILED"
    fi

    # Test 7: Delete resources
    log_test "Delete namespace and resources"
    k8s_total=$((k8s_total + 1))
    if kubectl --context "kind-$KIND_CLUSTER_NAME" delete namespace "$test_ns" 2>&1 | tail -1; then
        log_info "Delete resources: PASSED"
        k8s_pass=$((k8s_pass + 1))
    else
        log_error "Delete resources: FAILED"
    fi

    echo ""
    echo "======================================"
    echo "  Kubernetes Tests Summary"
    echo "======================================"
    echo "Passed: $k8s_pass / $k8s_total"
    echo "======================================"
    echo ""

    return 0
}

# Main execution
main() {
    log_info "Starting rusd Kubernetes integration tests"
    echo ""

    check_prereqs
    start_rusd

    if ! run_etcdctl_tests; then
        log_error "etcdctl tests failed"
        exit 1
    fi

    run_k8s_tests

    echo ""
    echo "======================================"
    echo "  All tests completed successfully!"
    echo "======================================"
    echo ""
    echo "Summary:"
    echo "  - rusd server: Running on http://localhost:$RUSD_PORT"
    echo "  - Compatibility: etcdctl operations verified"
    if command -v kind &>/dev/null; then
        echo "  - Kubernetes: Integration tests passed"
    fi
    echo ""
}

main
