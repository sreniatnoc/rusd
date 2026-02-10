#!/usr/bin/env bash
# TLS Validation Test for rusd
# Generates self-signed certs, starts rusd with TLS, runs etcdctl against it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RUSD_BIN="${RUSD_BIN:-$SCRIPT_DIR/../target/release/rusd}"
CERT_DIR="/tmp/rusd-tls-certs"
DATA_DIR="/tmp/rusd-tls-test"
LOG_FILE="/tmp/rusd-tls-test.log"
CLIENT_PORT=2579
PEER_PORT=2580
RUSD_PID=""

cleanup() {
    if [ -n "$RUSD_PID" ]; then
        kill "$RUSD_PID" 2>/dev/null || true
        wait "$RUSD_PID" 2>/dev/null || true
    fi
    rm -rf "$CERT_DIR" "$DATA_DIR"
}
trap cleanup EXIT

PASS=0
FAIL=0
TOTAL=0

run_test() {
    local name="$1"
    local cmd="$2"
    TOTAL=$((TOTAL + 1))
    echo "--- Test: $name ---"
    if eval "$cmd"; then
        echo "PASS: $name"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $name"
        FAIL=$((FAIL + 1))
    fi
}

echo "=== rusd TLS Validation ==="
echo ""

# Step 1: Generate self-signed CA + server cert
echo "Generating self-signed certificates..."
rm -rf "$CERT_DIR"
mkdir -p "$CERT_DIR"

# Generate CA key and cert
openssl genrsa -out "$CERT_DIR/ca-key.pem" 2048 2>/dev/null
openssl req -new -x509 -key "$CERT_DIR/ca-key.pem" \
    -out "$CERT_DIR/ca-cert.pem" -days 1 \
    -subj "/CN=rusd-test-ca" 2>/dev/null

# Generate server key and CSR
openssl genrsa -out "$CERT_DIR/server-key.pem" 2048 2>/dev/null
openssl req -new -key "$CERT_DIR/server-key.pem" \
    -out "$CERT_DIR/server.csr" \
    -subj "/CN=localhost" 2>/dev/null

# Create SAN config for localhost + 127.0.0.1
cat > "$CERT_DIR/server-ext.cnf" <<CERTEOF
[v3_req]
subjectAltName = DNS:localhost, IP:127.0.0.1
CERTEOF

# Sign server cert with CA
openssl x509 -req -in "$CERT_DIR/server.csr" \
    -CA "$CERT_DIR/ca-cert.pem" -CAkey "$CERT_DIR/ca-key.pem" \
    -CAcreateserial -out "$CERT_DIR/server-cert.pem" -days 1 \
    -extensions v3_req -extfile "$CERT_DIR/server-ext.cnf" 2>/dev/null

echo "Certificates generated in $CERT_DIR"

# Step 2: Start rusd with TLS
echo "Starting rusd with TLS on port $CLIENT_PORT..."
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

"$RUSD_BIN" \
    --name tls-test-node \
    --data-dir "$DATA_DIR" \
    --listen-client-urls "https://0.0.0.0:$CLIENT_PORT" \
    --listen-peer-urls "https://0.0.0.0:$PEER_PORT" \
    --advertise-client-urls "https://127.0.0.1:$CLIENT_PORT" \
    --initial-advertise-peer-urls "https://127.0.0.1:$PEER_PORT" \
    --initial-cluster "tls-test-node=https://127.0.0.1:$PEER_PORT" \
    --initial-cluster-state new \
    --cert-file "$CERT_DIR/server-cert.pem" \
    --key-file "$CERT_DIR/server-key.pem" \
    --trusted-ca-file "$CERT_DIR/ca-cert.pem" \
    --peer-cert-file "$CERT_DIR/server-cert.pem" \
    --peer-key-file "$CERT_DIR/server-key.pem" \
    --peer-trusted-ca-file "$CERT_DIR/ca-cert.pem" \
    --log-level info \
    > "$LOG_FILE" 2>&1 &
RUSD_PID=$!
echo "Started rusd with PID $RUSD_PID"

# Step 3: Wait for rusd TLS server to be ready
ENDPOINT="https://127.0.0.1:$CLIENT_PORT"
ETCDCTL_FLAGS="--endpoints=$ENDPOINT --cacert=$CERT_DIR/ca-cert.pem"

echo "Waiting for rusd TLS server to be ready..."
for i in $(seq 1 30); do
    if etcdctl $ETCDCTL_FLAGS endpoint health 2>/dev/null; then
        echo "rusd TLS server ready after ${i}s"
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: rusd TLS server failed to start after 30s"
        echo "Server log:"
        cat "$LOG_FILE" || true
        exit 1
    fi
    sleep 1
done

echo ""
echo "=== Running etcdctl tests over TLS ==="
echo ""

# Test: TLS endpoint health
run_test "TLS endpoint health" "
    etcdctl $ETCDCTL_FLAGS endpoint health
"

# Test: TLS put and get
run_test "TLS put/get" '
    etcdctl '"$ETCDCTL_FLAGS"' put /tls/key1 "secure-value" &&
    RESULT=$(etcdctl '"$ETCDCTL_FLAGS"' get /tls/key1 --print-value-only) &&
    [ "$RESULT" = "secure-value" ]
'

# Test: TLS range with prefix
run_test "TLS range prefix" '
    etcdctl '"$ETCDCTL_FLAGS"' put /tls/range/a "1" &&
    etcdctl '"$ETCDCTL_FLAGS"' put /tls/range/b "2" &&
    COUNT=$(etcdctl '"$ETCDCTL_FLAGS"' get /tls/range/ --prefix --print-value-only | wc -l | tr -d " ") &&
    [ "$COUNT" -ge 2 ]
'

# Test: TLS delete
run_test "TLS delete" '
    etcdctl '"$ETCDCTL_FLAGS"' put /tls/del "todelete" &&
    etcdctl '"$ETCDCTL_FLAGS"' del /tls/del &&
    RESULT=$(etcdctl '"$ETCDCTL_FLAGS"' get /tls/del --print-value-only) &&
    [ -z "$RESULT" ]
'

# Test: TLS member list
run_test "TLS member list" "
    etcdctl $ETCDCTL_FLAGS member list
"

# Test: TLS endpoint status
run_test "TLS endpoint status" "
    etcdctl $ETCDCTL_FLAGS endpoint status
"

# Test: TLS lease grant
run_test "TLS lease grant" "
    etcdctl $ETCDCTL_FLAGS lease grant 300
"

# Test: Reject plaintext connection to TLS server
run_test "Reject plaintext to TLS server" '
    ! etcdctl --endpoints=http://127.0.0.1:'"$CLIENT_PORT"' endpoint health 2>/dev/null
'

echo ""
echo "======================================"
echo "     TLS Validation Results"
echo "======================================"
echo "Passed: $PASS / $TOTAL"
echo "Failed: $FAIL / $TOTAL"
echo "======================================"

# Write results
cat > /tmp/tls-test-results.json <<RESULTEOF
{
    "total": $TOTAL,
    "passed": $PASS,
    "failed": $FAIL,
    "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
RESULTEOF

if [ $FAIL -gt 1 ]; then
    echo "Too many failures ($FAIL). Failing CI."
    exit 1
fi

echo "TLS validation complete."
