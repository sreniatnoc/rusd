#!/bin/bash
# ==============================================================================
# rusd etcd E2E Compatibility Test Script
# ==============================================================================
#
# This script runs etcd's own end-to-end test suite against rusd by:
# 1. Cloning the etcd repository (shallow)
# 2. Building etcdctl and etcdutl from their respective Go modules
# 3. Symlinking rusd as the "etcd" binary in the etcd repo's bin/ directory
# 4. Running etcd's Go-based e2e tests in progressive tiers
#
# etcd v3.5.x is a multi-module Go repo. The test framework lives at
# tests/framework/e2e/ and uses a -bin-dir flag (default: ../../bin relative
# to the test package directory, which resolves to <etcd-repo>/bin/).
# It detects readiness by scanning stdout/stderr for the substring
# "ready to serve client requests" and parses version via
# "etcd Version: X.Y.Z" from --version output.
#
# Usage:
#   ./scripts/etcd-e2e-compat.sh [--tier N] [--etcd-branch TAG] [--keep]
#
# Options:
#   --tier N          Run only tier N (1-6, default: run 1-3)
#   --etcd-branch TAG etcd git tag/branch to test against (default: v3.5.17)
#   --keep            Keep the work directory after completion
#   --work-dir DIR    Override work directory (default: /tmp/rusd-etcd-e2e)
#   --verbose         Show full test output (not just summary)
#
# Environment Variables:
#   RUSD_BINARY      - Path to the rusd binary (default: ./target/release/rusd)
#
# Tiers:
#   1 - Smoke: KV Put/Get NoTLS (fastest, highest signal)
#   2 - Core KV: All KV operations (Put, Get, Del, Txn)
#   3 - Watch + Lease + Auth
#   4 - Cluster + Member operations
#   5 - Selected advanced tests (compact, defrag, snapshot)
#   6 - Full e2e suite (long-running)
#
# Exit Codes:
#   0 - All tests in requested tier(s) passed
#   1 - One or more test failures
#   2 - Prerequisites not met
#   3 - Build/setup failure
# ==============================================================================

set -euo pipefail

# ==============================================================================
# Configuration
# ==============================================================================

RUSD_BINARY="${RUSD_BINARY:-./target/release/rusd}"
ETCD_BRANCH="v3.5.17"
WORK_DIR="/tmp/rusd-etcd-e2e"
MAX_TIER=3
SINGLE_TIER=""
KEEP_DIR=false
VERBOSE=false

# Color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# ==============================================================================
# Parse Arguments
# ==============================================================================

while [[ $# -gt 0 ]]; do
    case $1 in
        --tier)
            SINGLE_TIER="$2"
            MAX_TIER="$2"
            shift 2
            ;;
        --etcd-branch)
            ETCD_BRANCH="$2"
            shift 2
            ;;
        --work-dir)
            WORK_DIR="$2"
            shift 2
            ;;
        --keep)
            KEEP_DIR=true
            shift
            ;;
        --verbose)
            VERBOSE=true
            shift
            ;;
        -h|--help)
            head -50 "$0" | tail -46
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 2
            ;;
    esac
done

# ==============================================================================
# Helper Functions
# ==============================================================================

log_info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
log_ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }
log_tier()  { echo -e "${CYAN}[TIER $1]${NC} $2"; }

# ==============================================================================
# Prerequisites Check
# ==============================================================================

check_prerequisites() {
    log_info "Checking prerequisites..."

    # Check rusd binary
    if [[ ! -x "$RUSD_BINARY" ]]; then
        log_error "rusd binary not found or not executable: $RUSD_BINARY"
        log_info "Build it with: cargo build --release"
        exit 2
    fi

    # Verify rusd outputs etcd-compatible version
    if ! "$RUSD_BINARY" --version 2>&1 | grep -q "etcd Version:"; then
        log_error "rusd binary does not output etcd-compatible version string"
        log_info "Expected 'etcd Version: X.Y.Z' in --version output"
        exit 2
    fi

    # Check Go
    if ! command -v go &>/dev/null; then
        log_error "Go is not installed. etcd e2e tests require Go 1.22+"
        log_info "Install from: https://go.dev/dl/"
        exit 2
    fi

    # Parse Go version (macOS-compatible, no grep -P)
    GO_VERSION=$(go version | sed -E 's/.*go([0-9]+\.[0-9]+).*/\1/')
    GO_MAJOR=$(echo "$GO_VERSION" | cut -d. -f1)
    GO_MINOR=$(echo "$GO_VERSION" | cut -d. -f2)
    if [[ "$GO_MAJOR" -lt 1 ]] || { [[ "$GO_MAJOR" -eq 1 ]] && [[ "$GO_MINOR" -lt 22 ]]; }; then
        log_error "Go version $GO_VERSION is too old. Need 1.22+ (etcd v3.5.17 requirement)"
        exit 2
    fi
    log_ok "Go $GO_VERSION"

    # Check git
    if ! command -v git &>/dev/null; then
        log_error "git is not installed"
        exit 2
    fi

    log_ok "All prerequisites met"
}

# ==============================================================================
# Setup: Clone etcd, Build Tools, Create Bin Directory
# ==============================================================================

setup_environment() {
    log_info "Setting up test environment in $WORK_DIR"
    mkdir -p "$WORK_DIR"/results

    RUSD_BINARY_ABS="$(cd "$(dirname "$RUSD_BINARY")" && pwd)/$(basename "$RUSD_BINARY")"
    ETCD_DIR="$WORK_DIR/etcd"
    RESULTS_DIR="$WORK_DIR/results"

    # Clone etcd if not already present
    if [[ ! -d "$ETCD_DIR/.git" ]]; then
        log_info "Cloning etcd $ETCD_BRANCH (shallow)..."
        git clone --depth 1 --branch "$ETCD_BRANCH" \
            https://github.com/etcd-io/etcd.git "$ETCD_DIR" 2>&1 | tail -3
        log_ok "Cloned etcd $ETCD_BRANCH"
    else
        log_ok "etcd source already present at $ETCD_DIR"
    fi

    # The e2e framework defaults to <etcd-repo>/bin/ for binary lookup.
    # It resolves ../../bin relative to the test package (tests/e2e/).
    BIN_DIR="$ETCD_DIR/bin"
    mkdir -p "$BIN_DIR"

    # Symlink rusd as etcd
    ln -sf "$RUSD_BINARY_ABS" "$BIN_DIR/etcd"
    log_ok "Symlinked rusd as etcd: $BIN_DIR/etcd -> $RUSD_BINARY_ABS"

    # etcd v3.5.x is a multi-module repo. etcdctl and etcdutl are
    # separate Go modules — must build from their directories.
    if [[ ! -x "$BIN_DIR/etcdctl" ]]; then
        log_info "Building etcdctl from etcd source (separate Go module)..."
        (cd "$ETCD_DIR/etcdctl" && go build -o "$BIN_DIR/etcdctl" .) 2>&1
        log_ok "Built etcdctl"
    else
        log_ok "etcdctl already built"
    fi

    if [[ ! -x "$BIN_DIR/etcdutl" ]]; then
        log_info "Building etcdutl from etcd source (separate Go module)..."
        (cd "$ETCD_DIR/etcdutl" && go build -o "$BIN_DIR/etcdutl" .) 2>&1
        log_ok "Built etcdutl"
    else
        log_ok "etcdutl already built"
    fi

    # Verify symlink works
    log_info "Verifying etcd symlink..."
    if "$BIN_DIR/etcd" --version 2>&1 | grep -q "etcd Version:"; then
        log_ok "etcd symlink returns compatible version"
    else
        log_error "etcd symlink does not return expected version output"
        "$BIN_DIR/etcd" --version 2>&1
        exit 3
    fi

    export PATH="$BIN_DIR:$PATH"
    log_ok "Environment ready (BIN_DIR=$BIN_DIR)"
}

# ==============================================================================
# Test Runner
# ==============================================================================

TIER_PASS=0
TIER_FAIL=0
TIER_SKIP=0
TOTAL_PASS=0
TOTAL_FAIL=0
TOTAL_SKIP=0

run_tier() {
    local tier=$1
    local name=$2
    local test_filter=$3
    local timeout=$4

    log_tier "$tier" "Running: $name"
    log_info "  Filter:  $test_filter"
    log_info "  Timeout: $timeout"

    local log_file="$RESULTS_DIR/tier-${tier}.log"
    local rc=0

    # etcd v3.5.x tests module is at tests/go.mod (go.etcd.io/etcd/tests/v3).
    # e2e tests are at tests/e2e/. Run from the tests/ directory.
    local cmd="cd $ETCD_DIR/tests && go test ./e2e/..."
    if [[ "$test_filter" != "ALL" ]]; then
        cmd+=" -run '$test_filter'"
    fi
    cmd+=" -v -timeout $timeout -count=1"

    if $VERBOSE; then
        eval "$cmd" 2>&1 | tee "$log_file" || rc=$?
    else
        eval "$cmd" > "$log_file" 2>&1 || rc=$?
    fi

    # Parse results
    local passed failed skipped
    passed=$(grep -c "^--- PASS:" "$log_file" 2>/dev/null || true)
    failed=$(grep -c "^--- FAIL:" "$log_file" 2>/dev/null || true)
    skipped=$(grep -c "^--- SKIP:" "$log_file" 2>/dev/null || true)
    passed=${passed:-0}
    failed=${failed:-0}
    skipped=${skipped:-0}

    TIER_PASS=$passed
    TIER_FAIL=$failed
    TIER_SKIP=$skipped
    TOTAL_PASS=$((TOTAL_PASS + passed))
    TOTAL_FAIL=$((TOTAL_FAIL + failed))
    TOTAL_SKIP=$((TOTAL_SKIP + skipped))

    if [[ $rc -eq 0 ]]; then
        log_ok "Tier $tier PASSED: $passed passed, $skipped skipped"
    else
        log_error "Tier $tier HAD FAILURES: $passed passed, $failed failed, $skipped skipped"
        if ! $VERBOSE; then
            log_info "Failed tests:"
            grep "^--- FAIL:" "$log_file" | head -20 || true
            log_info "Full log: $log_file"
        fi
    fi

    echo ""
    return $rc
}

# ==============================================================================
# Tier Definitions
# ==============================================================================

run_tiers() {
    local start_tier=${SINGLE_TIER:-1}
    local end_tier=${MAX_TIER:-3}
    local any_failure=false

    # Tier 1: Smoke — KV Put/Get NoTLS (fastest, most basic)
    if [[ $start_tier -le 1 ]] && [[ $end_tier -ge 1 ]]; then
        run_tier 1 "Smoke — KV Put/Get NoTLS" \
            "TestCtlV3PutNoTLS|TestCtlV3GetNoTLS" \
            "5m" || any_failure=true
    fi

    # Tier 2: Core KV — All Put, Get, Del tests
    if [[ $start_tier -le 2 ]] && [[ $end_tier -ge 2 ]]; then
        run_tier 2 "Core KV Operations" \
            "TestCtlV3Put|TestCtlV3Get|TestCtlV3Del" \
            "10m" || any_failure=true
    fi

    # Tier 3: Watch + Lease + Auth
    if [[ $start_tier -le 3 ]] && [[ $end_tier -ge 3 ]]; then
        run_tier 3 "Watch + Lease + Auth" \
            "TestCtlV3Watch|TestCtlV3Lease|TestCtlV3Auth" \
            "15m" || any_failure=true
    fi

    # Tier 4: Cluster + Member operations
    if [[ $start_tier -le 4 ]] && [[ $end_tier -ge 4 ]]; then
        run_tier 4 "Cluster + Member Operations" \
            "TestCtlV3Member|TestCtlV3Endpoint|TestCtlV3Elect" \
            "15m" || any_failure=true
    fi

    # Tier 5: Advanced — compact, defrag, snapshot
    if [[ $start_tier -le 5 ]] && [[ $end_tier -ge 5 ]]; then
        run_tier 5 "Advanced — Compact, Defrag, Snapshot" \
            "TestCtlV3Compact|TestCtlV3Defrag|TestCtlV3Snapshot" \
            "15m" || any_failure=true
    fi

    # Tier 6: Full e2e suite
    if [[ $start_tier -le 6 ]] && [[ $end_tier -ge 6 ]]; then
        run_tier 6 "Full e2e Suite" \
            "ALL" \
            "60m" || any_failure=true
    fi

    if $any_failure; then
        return 1
    fi
    return 0
}

# ==============================================================================
# Report Generation
# ==============================================================================

generate_report() {
    local exit_code=$1
    local report_file="$RESULTS_DIR/report.json"

    cat > "$report_file" <<EOF
{
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "rusd_version": "$("$BIN_DIR/etcd" --version 2>&1 | head -1)",
  "etcd_branch": "$ETCD_BRANCH",
  "max_tier": $MAX_TIER,
  "total_passed": $TOTAL_PASS,
  "total_failed": $TOTAL_FAIL,
  "total_skipped": $TOTAL_SKIP,
  "exit_code": $exit_code
}
EOF

    echo ""
    echo "╔════════════════════════════════════════════════════════════╗"
    echo "║         etcd E2E Compatibility Report                    ║"
    echo "╚════════════════════════════════════════════════════════════╝"
    echo ""
    echo "  rusd version:    $("$RUSD_BINARY" --version 2>&1 | head -1)"
    echo "  etcd test suite: $ETCD_BRANCH"
    echo "  Tiers run:       ${SINGLE_TIER:-1}-${MAX_TIER}"
    echo ""
    echo "  ┌─────────────────────────────────────┐"
    printf "  │ Passed:  ${GREEN}%-28s${NC}│\n" "$TOTAL_PASS"
    printf "  │ Failed:  ${RED}%-28s${NC}│\n" "$TOTAL_FAIL"
    printf "  │ Skipped: ${YELLOW}%-28s${NC}│\n" "$TOTAL_SKIP"
    echo "  └─────────────────────────────────────┘"
    echo ""
    echo "  Results: $RESULTS_DIR/"
    echo "  Report:  $report_file"
    echo ""

    if [[ $TOTAL_FAIL -gt 0 ]]; then
        log_warn "Some tests failed. Review tier logs in $RESULTS_DIR/ for details."
    else
        log_ok "All tests passed!"
    fi
}

# ==============================================================================
# Cleanup
# ==============================================================================

cleanup() {
    if ! $KEEP_DIR; then
        log_info "Cleaning up (use --keep to preserve work directory)"
        rm -f "$WORK_DIR/etcd/bin/etcd"
    fi
}

# ==============================================================================
# Main
# ==============================================================================

main() {
    echo ""
    echo "╔════════════════════════════════════════════════════════════╗"
    echo "║   rusd etcd E2E Compatibility Test Runner                ║"
    echo "╚════════════════════════════════════════════════════════════╝"
    echo ""

    check_prerequisites
    setup_environment

    local rc=0
    run_tiers || rc=$?

    generate_report $rc

    trap cleanup EXIT

    exit $rc
}

main "$@"
