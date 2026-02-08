#!/bin/bash
set -euo pipefail

echo "======================================"
echo "  rusd - Rust etcd Replacement"
echo "  Setup & Build Script"
echo "======================================"

# Get the directory where this script is located
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Color codes for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Logging functions
log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check prerequisites
check_prereqs() {
    log_info "Checking prerequisites..."
    local all_ok=true

    # Check for Rust
    if ! command -v rustc &>/dev/null; then
        log_warn "Rust not found. Installing via rustup..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env"
    fi

    RUST_VERSION=$(rustc --version)
    log_info "Rust: $RUST_VERSION"

    # Check Rust version is 1.70+
    RUST_MINOR=$(echo "$RUST_VERSION" | sed -E 's/rustc 1\.([0-9]+).*/\1/')
    if [ "$RUST_MINOR" -lt 70 ]; then
        log_warn "Rust 1.70+ recommended. Updating..."
        rustup update
    fi

    # Check for protoc
    if ! command -v protoc &>/dev/null; then
        log_warn "protoc not found. Installing..."
        if [[ "$(uname)" == "Darwin" ]]; then
            if command -v brew &>/dev/null; then
                brew install protobuf
            else
                log_error "Please install Homebrew first: https://brew.sh"
                all_ok=false
            fi
        elif [[ -f /etc/debian_version ]]; then
            sudo apt-get update && sudo apt-get install -y protobuf-compiler libprotobuf-dev pkg-config libssl-dev
        elif [[ -f /etc/redhat-release ]]; then
            sudo yum install -y protobuf-compiler protobuf-devel openssl-devel
        else
            log_error "Please install protobuf compiler manually"
            all_ok=false
        fi
    fi

    if command -v protoc &>/dev/null; then
        PROTOC_VERSION=$(protoc --version)
        log_info "$PROTOC_VERSION"
    fi

    # Check for git
    if ! command -v git &>/dev/null; then
        log_error "git is required but not installed"
        all_ok=false
    fi

    # Check for curl (for docker tests)
    if ! command -v curl &>/dev/null; then
        log_warn "curl not found (needed for Docker tests)"
    fi

    # Check for docker (optional)
    if command -v docker &>/dev/null; then
        log_info "Docker: $(docker --version)"
    else
        log_warn "Docker not found (Docker tests will be skipped)"
    fi

    if [ "$all_ok" = false ]; then
        log_error "Missing required dependencies. Please install and try again."
        exit 1
    fi

    log_info "Prerequisites check passed!"
}

# Build the project
build() {
    log_info "Building rusd..."
    cd "$PROJECT_ROOT"

    # Clean previous builds
    log_info "Cleaning previous builds..."
    cargo clean

    # Build with optimizations
    log_info "Running cargo build --release..."
    cargo build --release 2>&1 | tail -20

    if [ -f "$PROJECT_ROOT/target/release/rusd" ]; then
        log_info "Build successful!"
        ls -lh "$PROJECT_ROOT/target/release/rusd"
    else
        log_error "Build failed: binary not found"
        exit 1
    fi
}

# Run tests
test() {
    log_info "Running tests..."
    cd "$PROJECT_ROOT"

    log_info "Running unit tests..."
    cargo test --lib --all 2>&1 | tail -30

    log_info "Running integration tests..."
    cargo test --test '*' --all 2>&1 | tail -30

    log_info "All tests passed!"
}

# Run linting and formatting checks
lint() {
    log_info "Running linting checks..."
    cd "$PROJECT_ROOT"

    log_info "Checking formatting..."
    cargo fmt -- --check 2>&1 || {
        log_warn "Code formatting issues found. Run: cargo fmt"
        return 1
    }

    log_info "Running clippy..."
    cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -20 || {
        log_warn "Clippy warnings found"
        return 1
    }

    log_info "Lint checks passed!"
}

# Build Docker image
build_docker() {
    log_info "Building Docker image..."
    cd "$PROJECT_ROOT"

    if ! command -v docker &>/dev/null; then
        log_error "Docker not installed. Skipping Docker build."
        return 1
    fi

    docker build -t rusd:latest -t rusd:1.0.0 . 2>&1 | tail -20
    log_info "Docker image built successfully!"
    docker images | grep rusd
}

# Show help
show_help() {
    cat <<EOF
Usage: $0 [COMMAND]

Commands:
    check       Check prerequisites only
    build       Build rusd (default)
    test        Run all tests
    lint        Run formatting and clippy checks
    docker      Build Docker image
    all         Run check, build, lint, and test
    full        Run everything including Docker build
    help        Show this help message

Examples:
    # Build only
    $0 build

    # Build and test
    $0 build test

    # Full setup (prerequisites, build, lint, test)
    $0 all

    # Everything including Docker
    $0 full

EOF
}

# Main function
main() {
    local commands=()

    # If no args, default to build
    if [ $# -eq 0 ]; then
        commands=("check" "build")
    else
        commands=("$@")
    fi

    for cmd in "${commands[@]}"; do
        case "$cmd" in
            check)
                check_prereqs
                ;;
            build)
                check_prereqs
                build
                ;;
            test)
                test
                ;;
            lint)
                lint
                ;;
            docker)
                check_prereqs
                build_docker
                ;;
            all)
                check_prereqs
                build
                lint
                test
                ;;
            full)
                check_prereqs
                build
                build_docker
                lint
                test
                ;;
            help|-h|--help)
                show_help
                ;;
            *)
                log_error "Unknown command: $cmd"
                show_help
                exit 1
                ;;
        esac
    done

    echo ""
    echo "======================================"
    echo "  Setup Complete!"
    echo "======================================"
    echo ""
    echo "Next steps:"
    echo ""
    echo "  1. Start rusd:"
    echo "     ./target/release/rusd"
    echo ""
    echo "  2. Or run a 3-node cluster:"
    echo "     docker-compose up -d"
    echo ""
    echo "  3. Test with etcdctl:"
    echo "     etcdctl --endpoints=http://localhost:2379 put foo bar"
    echo "     etcdctl --endpoints=http://localhost:2379 get foo"
    echo ""
    echo "  4. Run Kubernetes integration tests:"
    echo "     ./scripts/k8s-test.sh"
    echo ""
    echo "======================================"
}

main "$@"
