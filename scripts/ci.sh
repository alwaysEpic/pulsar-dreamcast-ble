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

# `--check`, not a bare `cargo fmt`. The old form *rewrote* the tree as a side
# effect of running the gate, so a "passing" run could silently differ from what
# was staged — and an agent iterating against it never saw a formatting failure
# at all, because the failure fixed itself.
echo "=== Formatting ==="
cargo fmt --all --check && pass "cargo fmt --check" \
    || fail "cargo fmt --check (run 'cargo fmt --all' to fix)"

echo ""
echo "=== maple-protocol tests ==="
(cd maple-protocol && cargo test) && pass "cargo test" || fail "cargo test"

# No -W flags here any more: the lint policy lives in [workspace.lints] in
# Cargo.toml, so this sees exactly what a bare `cargo clippy` sees on anyone's
# machine. `--all-targets` so tests and build scripts are linted too — build.rs
# was previously never checked at all.
echo ""
echo "=== Clippy (maple-protocol) ==="
(cd maple-protocol && cargo clippy --all-targets -- -D warnings) \
    && pass "clippy (maple-protocol)" || fail "clippy (maple-protocol)"

# Every board, not just the default. The boards select mutually exclusive
# feature sets (ADR-013), so a lint clean on `dk` says nothing about the code
# behind `board-pulsarv1`.
for b in dk xiao pulsarv1; do
    echo ""
    echo "=== Clippy (board-$b) ==="
    cargo clippy --no-default-features --features "board-$b" -- -D warnings \
        && pass "clippy (board-$b)" || fail "clippy (board-$b)"
done

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
