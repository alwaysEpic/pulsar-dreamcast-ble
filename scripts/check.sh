#!/usr/bin/env bash
# Fast feedback loop — run this after every change.
#
# `scripts/ci.sh` is the gate: fmt, three boards of clippy, five release builds
# and the timing invariants on each — ~12 s warm, minutes from cold or in CI.
# This is the thing you run while actually working: ~2 s warm.
#
# It runs clippy rather than `cargo check` on purpose: clippy is a strict
# superset — it type-checks everything check does, then applies the lint policy
# from Cargo.toml — so one command gives both, against the same rules as CI.
#
# The cost is small enough that there is no reason to reach for check first.
# Measured on this workspace, incremental after a one-file edit:
#
#     cargo check    ~0.27 s
#     cargo clippy   ~0.50 s
#
# and the two do *not* invalidate each other — a check straight after a clippy
# is still ~0.3 s, so alternating them costs nothing either. Clippy is simply
# the strictly more useful of the two for a quarter-second.
#
# Usage:
#   scripts/check.sh              # host-testable crate + default board
#   scripts/check.sh pulsarv1     # a specific board
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

BOARD="${1:-dk}"
case "$BOARD" in
    dk|xiao|pulsarv1) ;;
    *) echo "usage: $0 [dk|xiao|pulsarv1]" >&2; exit 2 ;;
esac

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
pass() { echo -e "${GREEN}PASS${NC} $1"; }
fail() { echo -e "${RED}FAIL${NC} $1"; exit 1; }

# maple-protocol first: it is host-native and holds every test, so it fails
# fastest and its errors block the firmware crate anyway.
echo "=== maple-protocol: tests ==="
(cd maple-protocol && cargo test -q) && pass "tests" || fail "tests"

echo ""
echo "=== maple-protocol: clippy ==="
(cd maple-protocol && cargo clippy -q --all-targets -- -D warnings) \
    && pass "clippy (maple-protocol)" || fail "clippy (maple-protocol)"

echo ""
echo "=== firmware: clippy (board-$BOARD) ==="
cargo clippy -q --no-default-features --features "board-$BOARD" -- -D warnings \
    && pass "clippy (board-$BOARD)" || fail "clippy (board-$BOARD)"

echo ""
echo -e "${GREEN}Clean.${NC} Run ./scripts/ci.sh before committing —"
echo "it builds all five release variants and checks the timing invariants,"
echo "which this deliberately does not."
