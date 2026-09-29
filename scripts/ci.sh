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
# was staged — and anyone iterating against it never saw a formatting failure
# at all, because the failure fixed itself.
echo "=== Formatting ==="
cargo fmt --all --check && pass "cargo fmt --check" \
    || fail "cargo fmt --check (run 'cargo fmt --all' to fix)"

echo ""
echo "=== maple-protocol tests ==="
(cd maple-protocol && cargo test) && pass "cargo test" || fail "cargo test"
# The block decoder's statistics are a feature; the reference test asserts
# them only when it is on, so both configurations run.
(cd maple-protocol && cargo test --features decode-stats) \
    && pass "cargo test (decode-stats)" || fail "cargo test (decode-stats)"

# battery-policy: the low-battery rule, host-native for the same reason — the
# firmware crate cannot run a test, and this logic powers units off.
echo ""
echo "=== battery-policy tests ==="
(cd battery-policy && cargo test) && pass "cargo test (battery-policy)" \
    || fail "cargo test (battery-policy)"
(cd battery-policy && cargo clippy --all-targets -- -D warnings) \
    && pass "clippy (battery-policy)" || fail "clippy (battery-policy)"

# No -W flags here any more: the lint policy lives in [workspace.lints] in
# Cargo.toml, so this sees exactly what a bare `cargo clippy` sees on anyone's
# machine. `--all-targets` so tests and build scripts are linted too — build.rs
# was previously never checked at all.
echo ""
echo "=== Clippy (maple-protocol) ==="
(cd maple-protocol && cargo clippy --all-targets -- -D warnings) \
    && pass "clippy (maple-protocol)" || fail "clippy (maple-protocol)"
(cd maple-protocol && cargo clippy --all-targets --features decode-stats -- -D warnings) \
    && pass "clippy (maple-protocol, decode-stats)" || fail "clippy (maple-protocol, decode-stats)"

# Every board, not just the default. The boards select mutually exclusive
# feature sets (ADR-013), so a lint clean on `dk` says nothing about the code
# behind `board-pulsarv1`.
for b in dk xiao pulsarv1; do
    echo ""
    echo "=== Clippy (board-$b) ==="
    cargo clippy --no-default-features --features "board-$b" -- -D warnings \
        && pass "clippy (board-$b)" || fail "clippy (board-$b)"
done

# No board feature implies `poll-period-debug` and no release build below
# enables it, so until this line existed `src/poll_period.rs` was never
# compiled by the gate at all — a whole module could rot silently, and did:
# two lints had accumulated in untouched code by 2026-09. The telemetry the
# the gate reads comes out of that module; it is held to the same bar as
# the rest of the firmware.
echo ""
echo "=== Clippy (board-pulsarv1 + poll-period-debug) ==="
cargo clippy --no-default-features --features board-pulsarv1,poll-period-debug -- -D warnings \
    && pass "clippy (poll-period-debug)" || fail "clippy (poll-period-debug)"

ELF="target/thumbv7em-none-eabihf/release/pulsar-dreamcast-ble"

# Build each release variant and verify its timing invariants (the RX capture
# in the form that board uses, hardware-timed VMU LCD TX). VMU is always
# compiled in now, so every variant exercises it — there is no separate +vmu
# matrix to maintain.
#
# The third argument is the RX mode: `spim` for the boards whose RX capture is
# the SPIM/EasyDMA path — where a CPU sampling loop must be ABSENT — and `cpu`
# (optionally `cpu:<address>`) for the DK, which keeps the sampler. The capture backend,
# route (a): the carrier and the XIAO are on the capture, the DK is not.
build_and_check() {
    local label="$1" features="$2" mode="${3:-cpu}"
    echo ""
    echo "=== Build: $label ==="
    cargo build --release --no-default-features --features "$features" \
        && pass "build ($label)" || fail "build ($label)"
    ./scripts/check_timing_invariants.sh "$ELF" "$label" "$mode" \
        && pass "timing invariants ($label)" || fail "timing invariants ($label)"
}

# There is no production RX loop address to hold any more.
# The hold was PRODUCTION_RX_LOOP=0x2719c, the address of the CPU sampling loop
# in production_reference_v281.elf, kept because that loop's cycle count was the
# sample rate the decode thresholds were calibrated to and a build that moved it
# was a placement roll.
#
# Route (a) removed the loop. The carrier and the XIAO read every Maple reply on
# the SPIM/EasyDMA capture, clocked by hardware, so no production build has a
# CPU sampling loop to place — and `spim` mode below checks the opposite
# invariant, that none has come back. The DK keeps its sampler and is still
# checked in `cpu` mode.
#
# The DK did NOT inherit that argument. It still compiles the sampler, and
# removing the pad removed its placement control too — so `cpu` mode now
# rejects a 16-byte-aligned loop outright (check 1a), which is the measured
# cause rather than one proven address. That is what carries the placement guard's
# protection on the one board that still needs it.
#
# NOT YET RETIRED BY PROOF. The pad and the hold came out together, so this
# build's placement differs by construction — but the pin is retired when a
# Control run on a pulsarv1 unit passes on it, not by this file. See ADR-018.
build_and_check "xiao+rtt" "board-xiao,rtt" spim
build_and_check "xiao" "board-xiao" spim
build_and_check "pulsarv1+rtt" "board-pulsarv1,rtt" spim
build_and_check "pulsarv1" "board-pulsarv1" spim
build_and_check "dk" "board-dk" cpu

echo ""
echo -e "${GREEN}All checks passed!${NC}"
