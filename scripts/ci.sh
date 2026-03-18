#!/usr/bin/env bash
# Pre-commit quality checks for embedded_dreamcast
# Run this before committing to catch issues early.

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

pass() { echo -e "${GREEN}PASS${NC} $1"; }
fail() { echo -e "${RED}FAIL${NC} $1"; exit 1; }

echo "=== Formatting ==="
cargo fmt --all && pass "cargo fmt"

echo ""
echo "=== maple-protocol tests ==="
(cd maple-protocol && cargo test) && pass "cargo test" || fail "cargo test"

echo ""
echo "=== Clippy (main crate, default features) ==="
cargo clippy -- -W clippy::all -W clippy::pedantic && pass "clippy (dk)" || fail "clippy (dk)"

echo ""
echo "=== Clippy (maple-protocol) ==="
(cd maple-protocol && cargo clippy -- -W clippy::all -W clippy::pedantic) && pass "clippy (maple-protocol)" || fail "clippy (maple-protocol)"

echo ""
echo "=== Build: XIAO release (with RTT) ==="
cargo build --release --no-default-features --features board-xiao,rtt && pass "build (xiao+rtt)" || fail "build (xiao+rtt)"

echo ""
echo "=== Build: XIAO release (production, no RTT) ==="
cargo build --release --no-default-features --features board-xiao && pass "build (xiao)" || fail "build (xiao)"

echo ""
echo "=== Build: DK release ==="
cargo build --release --no-default-features --features board-dk && pass "build (dk)" || fail "build (dk)"

echo ""
echo -e "${GREEN}All checks passed!${NC}"
