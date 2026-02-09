#!/bin/bash
# Head-to-Head Benchmark: etcd vs rusd
# Runs identical K8s workloads against both backends and captures metrics
#
# Usage: ./benchmarks/k8s-benchmark.sh

set -euo pipefail

RUSD_BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/rusd"
RESULTS_DIR="/tmp/rusd-benchmark-results"
RUSD_PORT=2479
ETCD_PORT=2579

mkdir -p "$RESULTS_DIR"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log() { echo -e "${GREEN}[BENCH]${NC} $1"; }
log_blue() { echo -e "${BLUE}[BENCH]${NC} $1"; }

# ============================================================
# Phase 1: etcdctl Microbenchmarks
# ============================================================

run_etcdctl_bench() {
    local NAME=$1
    local PORT=$2
    local ENDPOINT="http://localhost:$PORT"
    local OUT="$RESULTS_DIR/${NAME}-etcdctl.txt"

    log "Running etcdctl benchmarks against $NAME ($ENDPOINT)"

    echo "=== $NAME etcdctl Benchmark ===" > "$OUT"
    echo "Timestamp: $(date -Iseconds)" >> "$OUT"
    echo "" >> "$OUT"

    # Sequential puts
    echo "--- Sequential Put (1000 keys) ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    for i in $(seq 1 1000); do
        ETCDCTL_API=3 etcdctl --endpoints="$ENDPOINT" put "/bench/key$i" "value-$i-$(head -c 100 /dev/urandom | base64 | head -c 100)" 2>/dev/null >/dev/null
    done
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    OPS_SEC=$(( 1000 * 1000 / (DUR_MS > 0 ? DUR_MS : 1) ))
    echo "Duration: ${DUR_MS}ms" >> "$OUT"
    echo "Throughput: ${OPS_SEC} ops/sec" >> "$OUT"
    echo "" >> "$OUT"
    log "  $NAME seq-put: ${DUR_MS}ms (${OPS_SEC} ops/s)"

    # Sequential gets
    echo "--- Sequential Get (1000 keys) ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    for i in $(seq 1 1000); do
        ETCDCTL_API=3 etcdctl --endpoints="$ENDPOINT" get "/bench/key$i" 2>/dev/null >/dev/null
    done
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    OPS_SEC=$(( 1000 * 1000 / (DUR_MS > 0 ? DUR_MS : 1) ))
    echo "Duration: ${DUR_MS}ms" >> "$OUT"
    echo "Throughput: ${OPS_SEC} ops/s" >> "$OUT"
    echo "" >> "$OUT"
    log "  $NAME seq-get: ${DUR_MS}ms (${OPS_SEC} ops/s)"

    # Range query (prefix scan)
    echo "--- Range Query (prefix /bench/) ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    for i in $(seq 1 100); do
        ETCDCTL_API=3 etcdctl --endpoints="$ENDPOINT" get /bench/ --prefix --limit=100 2>/dev/null >/dev/null
    done
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    OPS_SEC=$(( 100 * 1000 / (DUR_MS > 0 ? DUR_MS : 1) ))
    echo "Duration: ${DUR_MS}ms (100 iterations)" >> "$OUT"
    echo "Throughput: ${OPS_SEC} ops/s" >> "$OUT"
    echo "" >> "$OUT"
    log "  $NAME range-100x: ${DUR_MS}ms (${OPS_SEC} ops/s)"

    # Delete all
    echo "--- Sequential Delete (1000 keys) ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    for i in $(seq 1 1000); do
        ETCDCTL_API=3 etcdctl --endpoints="$ENDPOINT" del "/bench/key$i" 2>/dev/null >/dev/null
    done
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    OPS_SEC=$(( 1000 * 1000 / (DUR_MS > 0 ? DUR_MS : 1) ))
    echo "Duration: ${DUR_MS}ms" >> "$OUT"
    echo "Throughput: ${OPS_SEC} ops/s" >> "$OUT"
    echo "" >> "$OUT"
    log "  $NAME seq-del: ${DUR_MS}ms (${OPS_SEC} ops/s)"

    # Memory usage
    if [ "$NAME" = "rusd" ]; then
        PID=$(pgrep -f "target/release/rusd" | head -1)
    else
        PID=$(pgrep -f "etcd --name" | head -1)
    fi
    if [ -n "$PID" ]; then
        RSS=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
        RSS_MB=$((RSS / 1024))
        echo "Memory (RSS): ${RSS_MB}MB" >> "$OUT"
        log "  $NAME memory: ${RSS_MB}MB RSS"
    fi
}

# ============================================================
# Phase 2: K8s Workload Benchmark
# ============================================================

run_k8s_bench() {
    local NAME=$1
    local PORT=$2
    local CTX=$3
    local OUT="$RESULTS_DIR/${NAME}-k8s.txt"

    log "Running K8s benchmarks against $NAME (context: $CTX)"

    echo "=== $NAME K8s Workload Benchmark ===" > "$OUT"
    echo "Timestamp: $(date -Iseconds)" >> "$OUT"
    echo "" >> "$OUT"

    # K8s cluster creation time is measured separately

    # Namespace creation
    echo "--- Create 10 Namespaces ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    for i in $(seq 1 10); do
        kubectl --context "$CTX" create namespace "bench-ns-$i" 2>/dev/null >/dev/null
    done
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    echo "Duration: ${DUR_MS}ms" >> "$OUT"
    log "  $NAME 10 namespaces: ${DUR_MS}ms"

    # ConfigMap creation
    echo "" >> "$OUT"
    echo "--- Create 50 ConfigMaps ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    for i in $(seq 1 50); do
        kubectl --context "$CTX" create configmap "bench-cm-$i" --from-literal=data="value-$i" -n bench-ns-1 2>/dev/null >/dev/null
    done
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    echo "Duration: ${DUR_MS}ms" >> "$OUT"
    log "  $NAME 50 configmaps: ${DUR_MS}ms"

    # Deployment + scaling
    echo "" >> "$OUT"
    echo "--- Deployment Create + Scale to 5 ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    kubectl --context "$CTX" create deployment bench-app --image=nginx:alpine -n bench-ns-1 2>/dev/null >/dev/null
    kubectl --context "$CTX" scale deployment bench-app --replicas=5 -n bench-ns-1 2>/dev/null >/dev/null
    kubectl --context "$CTX" rollout status deployment/bench-app -n bench-ns-1 --timeout=120s 2>/dev/null >/dev/null
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    echo "Duration: ${DUR_MS}ms" >> "$OUT"
    log "  $NAME deploy+scale(5): ${DUR_MS}ms"

    # Rolling update
    echo "" >> "$OUT"
    echo "--- Rolling Update (5 replicas) ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    kubectl --context "$CTX" set image deployment/bench-app nginx=nginx:latest -n bench-ns-1 2>/dev/null >/dev/null
    kubectl --context "$CTX" rollout status deployment/bench-app -n bench-ns-1 --timeout=120s 2>/dev/null >/dev/null
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    echo "Duration: ${DUR_MS}ms" >> "$OUT"
    log "  $NAME rolling-update(5): ${DUR_MS}ms"

    # Cleanup
    echo "" >> "$OUT"
    echo "--- Cleanup 10 Namespaces ---" >> "$OUT"
    START=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    for i in $(seq 1 10); do
        kubectl --context "$CTX" delete namespace "bench-ns-$i" --wait=false 2>/dev/null >/dev/null &
    done
    wait
    END=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    DUR_MS=$(( (END - START) / 1000000 ))
    echo "Duration: ${DUR_MS}ms (async)" >> "$OUT"
    log "  $NAME cleanup: ${DUR_MS}ms"
}

# ============================================================
# Main
# ============================================================

echo ""
echo "╔════════════════════════════════════════════════╗"
echo "║     Head-to-Head Benchmark: etcd vs rusd      ║"
echo "╚════════════════════════════════════════════════╝"
echo ""

# --- Benchmark rusd (already running) ---
log_blue "Phase 1: rusd etcdctl microbenchmarks"
if ! pgrep -f "target/release/rusd" >/dev/null 2>&1; then
    log "Starting rusd..."
    rm -rf /tmp/rusd-bench-data
    "$RUSD_BIN" --name bench-node --data-dir /tmp/rusd-bench-data \
        --listen-client-urls "http://0.0.0.0:$RUSD_PORT" \
        --advertise-client-urls "http://localhost:$RUSD_PORT" \
        --listen-peer-urls http://127.0.0.1:2480 \
        --initial-cluster "bench-node=http://127.0.0.1:2480" &
    sleep 2
fi
run_etcdctl_bench "rusd" $RUSD_PORT

# --- Benchmark etcd ---
log_blue "Phase 2: etcd etcdctl microbenchmarks"
rm -rf /tmp/etcd-bench-data
etcd --name bench-etcd --data-dir /tmp/etcd-bench-data \
    --listen-client-urls "http://0.0.0.0:$ETCD_PORT" \
    --advertise-client-urls "http://localhost:$ETCD_PORT" \
    --listen-peer-urls "http://127.0.0.1:2580" \
    --initial-advertise-peer-urls "http://127.0.0.1:2580" \
    --initial-cluster "bench-etcd=http://127.0.0.1:2580" \
    --initial-cluster-state new \
    --log-level warn &
ETCD_PID=$!
sleep 3
run_etcdctl_bench "etcd" $ETCD_PORT
kill $ETCD_PID 2>/dev/null
wait $ETCD_PID 2>/dev/null || true

# --- K8s benchmark with rusd (Kind cluster already running) ---
log_blue "Phase 3: K8s workload benchmark (rusd)"
if kind get clusters 2>/dev/null | grep -q rusd-test; then
    run_k8s_bench "rusd" $RUSD_PORT "kind-rusd-test"
else
    log "No rusd Kind cluster found, skipping K8s benchmark for rusd"
fi

# --- K8s benchmark with etcd ---
log_blue "Phase 4: K8s workload benchmark (etcd)"
# Start etcd for Kind
rm -rf /tmp/etcd-kind-data
etcd --name kind-node --data-dir /tmp/etcd-kind-data \
    --listen-client-urls "http://0.0.0.0:$ETCD_PORT" \
    --advertise-client-urls "http://host.docker.internal:$ETCD_PORT" \
    --listen-peer-urls "http://127.0.0.1:2580" \
    --initial-advertise-peer-urls "http://127.0.0.1:2580" \
    --initial-cluster "kind-node=http://127.0.0.1:2580" \
    --initial-cluster-state new \
    --log-level warn &
ETCD_PID=$!
sleep 3

# Create etcd Kind config
cat > /tmp/kind-etcd.yaml << 'KINDEOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
networking:
  apiServerPort: 6444
nodes:
  - role: control-plane
    kubeadmConfigPatches:
      - |
        kind: ClusterConfiguration
        etcd:
          external:
            endpoints:
              - http://host.docker.internal:2579
KINDEOF

log "Creating Kind cluster with etcd..."
START_CLUSTER=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
if kind create cluster --name etcd-test --config /tmp/kind-etcd.yaml 2>/dev/null; then
    END_CLUSTER=$(gdate +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
    CLUSTER_MS=$(( (END_CLUSTER - START_CLUSTER) / 1000000 ))
    echo "etcd Kind cluster creation: ${CLUSTER_MS}ms" >> "$RESULTS_DIR/etcd-k8s.txt"
    log "  etcd cluster creation: ${CLUSTER_MS}ms"

    # Wait for Ready
    for i in $(seq 1 30); do
        STATUS=$(kubectl --context kind-etcd-test get nodes -o jsonpath='{.items[0].status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
        if [ "$STATUS" = "True" ]; then break; fi
        sleep 5
    done

    run_k8s_bench "etcd" $ETCD_PORT "kind-etcd-test"

    kind delete cluster --name etcd-test 2>/dev/null
fi

kill $ETCD_PID 2>/dev/null
wait $ETCD_PID 2>/dev/null || true

# ============================================================
# Generate comparison report
# ============================================================

log_blue "Generating comparison report..."

REPORT="$RESULTS_DIR/comparison.txt"
echo "╔════════════════════════════════════════════════════════════════╗" > "$REPORT"
echo "║         Head-to-Head Benchmark: etcd vs rusd                 ║" >> "$REPORT"
echo "║         $(date -Iseconds)                       ║" >> "$REPORT"
echo "╚════════════════════════════════════════════════════════════════╝" >> "$REPORT"
echo "" >> "$REPORT"

echo "=== etcdctl Microbenchmarks ===" >> "$REPORT"
echo "" >> "$REPORT"
echo "--- rusd ---" >> "$REPORT"
cat "$RESULTS_DIR/rusd-etcdctl.txt" >> "$REPORT" 2>/dev/null
echo "" >> "$REPORT"
echo "--- etcd ---" >> "$REPORT"
cat "$RESULTS_DIR/etcd-etcdctl.txt" >> "$REPORT" 2>/dev/null
echo "" >> "$REPORT"

echo "=== K8s Workload Benchmarks ===" >> "$REPORT"
echo "" >> "$REPORT"
if [ -f "$RESULTS_DIR/rusd-k8s.txt" ]; then
    echo "--- rusd ---" >> "$REPORT"
    cat "$RESULTS_DIR/rusd-k8s.txt" >> "$REPORT"
    echo "" >> "$REPORT"
fi
if [ -f "$RESULTS_DIR/etcd-k8s.txt" ]; then
    echo "--- etcd ---" >> "$REPORT"
    cat "$RESULTS_DIR/etcd-k8s.txt" >> "$REPORT"
    echo "" >> "$REPORT"
fi

log "Results saved to: $RESULTS_DIR/"
log "Comparison report: $REPORT"
echo ""
cat "$REPORT"
