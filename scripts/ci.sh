#!/usr/bin/env bash
# Pre-commit quality checks for pulsar-dreamcast-ble
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

ELF="target/thumbv7em-none-eabihf/release/pulsar-dreamcast-ble"

# Build each release variant and verify its timing invariants (pinned RX
# sampling loop, hardware-timed VMU LCD TX). VMU is always compiled in now, so
# every variant exercises it — there is no separate +vmu matrix to maintain.
build_and_check() {
    local label="$1" features="$2"
    echo ""
    echo "=== Build: $label ==="
    cargo build --release --no-default-features --features "$features" \
        && pass "build ($label)" || fail "build ($label)"
    ./scripts/check_timing_invariants.sh "$ELF" "$label" \
        && pass "timing invariants ($label)" || fail "timing invariants ($label)"
}

build_and_check "xiao+rtt" "board-xiao,rtt"
build_and_check "xiao" "board-xiao"
build_and_check "pulsarv1+rtt" "board-pulsarv1,rtt"
build_and_check "pulsarv1" "board-pulsarv1"
build_and_check "dk" "board-dk"

echo ""
echo -e "${GREEN}All checks passed!${NC}"
