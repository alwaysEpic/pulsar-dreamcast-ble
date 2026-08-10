#!/usr/bin/env bash
# Bench release-gate: the only red test that measures REAL Maple Bus timing on
# real silicon with the real SoftDevice running. Everything in ci.sh is a
# static proxy; this is the ground truth.
#
# Flashes the DK with poll-timing instrumentation, waits for the paired host
# to connect (so the SoftDevice radio is active — the ~70-retry baseline IS
# measured under live BLE interference, not in artificial silence), collects
# POLLTIME/POLLPHASE windows, and fails red unless the input path is healthy.
#
# Healthy reference (2026-06-12, hardware-timed TX + pinned RX sampling):
#   read min  3117-3128 us   (sampling-loop floor; misaligned builds >= 3279)
#   tries     63-79 / 60 polls (bad builds 100-177)
#   period    ~16500 us      (~60 Hz; starved builds ~30000)
#
# Usage:
#   scripts/bench_check.sh          # dk + poll-timing (no VMU hardware needed)
#   scripts/bench_check.sh --vmu    # also exercise the VMU write path (dock the VMU)
#
# Requires: a DK on USB (J-Link), and the controller's BLE host (your Mac)
# paired and ready to connect. The script tells you when to connect.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; NC='\033[0m'

# --- Gate criteria ----------------------------------------------------------
# Two HARD gates, chosen for what they isolate:
#
#   read-min (median) — PURELY firmware: the sampling-loop floor, untouched by
#     anything host- or bus-side. The gold-standard regression signal. Use the
#     MEDIAN per-window value, not the global minimum: sick builds occasionally
#     log a very short read-min on a truncated/failed read (a 2655us outlier in
#     an otherwise-3400us sick window), so the floor is not a discriminator.
#     Healthy ~3128, sick ~3452 (validated vs rtt_logs/abtest_{E,D}).
#
#   tries (median) — firmware TX/RX robustness. Elevated tries means controller
#     frames are failing; healthy 63-79, regressed builds 140-177.
#
# period is REPORTED but NOT gated: it is ~entirely derived from tries (each
# retry adds a full read+decode to get_condition), so failing on it would
# double-count the tries failure. Kept visible as a sanity readout.
READ_MIN_MEDIAN_MAX=3250  # HARD gate: median per-window read-min must be under this
TRIES_MEDIAN_MAX=90       # HARD gate: median tries/60-polls must be under this
COLLECT_SECS=45           # measurement window after the poll loop starts
CONNECT_TIMEOUT=120       # seconds to wait for the host to connect

FEATURES="board-dk,rtt,poll-timing"
LABEL="dk+poll-timing"
if [ "${1:-}" = "--vmu" ]; then
    # Label-only marker: VMU is always compiled in (there is no `vmu` feature),
    # so this flag does not change the build. It just records that you've docked
    # the VMU to exercise the LCD write path on real hardware during this run.
    LABEL="dk+poll-timing+vmu"
fi

ELF="target/thumbv7em-none-eabihf/release/pulsar-dreamcast-ble"
LOG="$(mktemp)"
PROBE_PID=""
cleanup() {
    rc=$?
    [ -n "$PROBE_PID" ] && kill "$PROBE_PID" 2>/dev/null || true
    # Preserve the RTT log on failure so a red run is diagnosable without a
    # re-run (the detail showing WHERE retries come from is only in the log).
    if [ "$rc" -ne 0 ] && [ -s "$LOG" ]; then
        saved="rtt_logs/bench_check_fail_$$.txt"
        cp "$LOG" "$saved" 2>/dev/null && echo "  RTT log saved for diagnosis: $saved"
    fi
    rm -f "$LOG"
}
trap cleanup EXIT

echo "=== Bench gate: $LABEL ==="
echo "Building..."
cargo build --release --no-default-features --features "$FEATURES" >/dev/null 2>&1 \
    || { echo -e "${RED}FAIL${NC}: build failed"; exit 1; }

echo "Flashing + streaming RTT..."
probe-rs run --chip nRF52840_xxAA "$ELF" >"$LOG" 2>&1 &
PROBE_PID=$!

# --- Wait for the host to connect (poll loop only runs once connected) -----
echo -e "${YELLOW}>>> Connect the controller's BLE host now (pair/connect your Mac).${NC}"
echo "    Waiting up to ${CONNECT_TIMEOUT}s for the poll loop to start..."
waited=0
until grep -q "POLLPHASE" "$LOG" 2>/dev/null; do
    sleep 1
    waited=$((waited + 1))
    if ! kill -0 "$PROBE_PID" 2>/dev/null; then
        echo -e "${RED}FAIL${NC}: probe-rs exited early. Last lines:"; tail -5 "$LOG"; exit 1
    fi
    if [ "$waited" -ge "$CONNECT_TIMEOUT" ]; then
        echo -e "${RED}FAIL${NC}: no POLLPHASE within ${CONNECT_TIMEOUT}s — host never connected?"
        exit 1
    fi
done
echo -e "${GREEN}Connected.${NC} Measuring for ${COLLECT_SECS}s — leave the stick alone."
sleep "$COLLECT_SECS"

# --- Parse and verdict ------------------------------------------------------
# Snapshot only the lines emitted during the measurement window: drop any
# POLLPHASE/POLLTIME printed before the connect marker was first seen.
RESULT=$(awk '
    /POLLPHASE/ {
        for (i = 1; i <= NF; i++) {
            if ($i ~ /^min=/ && phase == "read") { split($i, a, "="); rmin[++rn] = a[2]; phase = "" }
            if ($i == "read")  phase = "read"
            if ($i ~ /^sum=/) { split($i, a, "="); tr[++tn] = a[2] }
        }
    }
    /POLLTIME/ {
        for (i = 1; i <= NF; i++) {
            if ($i ~ /^avg=/ && pphase == "period") { split($i, a, "="); per[++pn] = a[2]; pphase = "" }
            if ($i == "period") pphase = "period"
        }
    }
    function median(arr, n,   i, c, tmp) {
        for (i = 1; i <= n; i++) c[i] = arr[i]
        for (i = 1; i <= n; i++) for (j = i+1; j <= n; j++) if (c[j] < c[i]) { tmp=c[i]; c[i]=c[j]; c[j]=tmp }
        return (n % 2) ? c[(n+1)/2] : int((c[n/2] + c[n/2+1]) / 2)
    }
    END {
        if (rn == 0) { print "NODATA"; exit }
        printf "%d %d %d %d\n", median(rmin, rn), median(tr, tn), median(per, pn), rn
    }
' "$LOG")

if [ "$RESULT" = "NODATA" ]; then
    echo -e "${RED}FAIL${NC}: no parseable POLLPHASE windows collected."; exit 1
fi
read -r READ_MED TRIES_MED PERIOD_MED WINDOWS <<< "$RESULT"

echo ""
echo "=== Results ($WINDOWS windows) ==="
printf "  read-min median: %5d us   (<= %d, HARD)\n" "$READ_MED" "$READ_MIN_MEDIAN_MAX"
printf "  tries  median  : %5d      (<= %d, HARD)\n" "$TRIES_MED" "$TRIES_MEDIAN_MAX"
printf "  period median  : %5d us   (info — derived from tries)\n" "$PERIOD_MED"
echo ""

ok=1
[ "$READ_MED" -le "$READ_MIN_MEDIAN_MAX" ] || { echo -e "${RED}FAIL${NC}: sampling read-min ${READ_MED}us — loop slow (firmware: misaligned/codegen)"; ok=0; }
[ "$TRIES_MED"  -le "$TRIES_MEDIAN_MAX" ]   || { echo -e "${RED}FAIL${NC}: tries median ${TRIES_MED} — controller frames failing (TX timing, or bus signal integrity if read-min is clean)"; ok=0; }

if [ "$ok" -eq 1 ]; then
    echo -e "${GREEN}PASS${NC}: input path healthy on real hardware."
else
    echo -e "${YELLOW}Note:${NC} read-min clean + tries high => sampling code is fine; suspect the bus/controller (signal integrity), not firmware."
    echo -e "${RED}Bench gate failed — compare against a known-good bench run.${NC}"
    exit 1
fi
