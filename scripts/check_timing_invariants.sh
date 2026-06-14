#!/usr/bin/env bash
# Timing-invariant checks on a compiled ELF.
#
# Wire timing on this firmware must not depend on codegen (debug log
# 2026-06-11/12: a layout shift slowed the RX sampling loop +1 cycle/sample;
# a feature-flag change re-optimized the bit-banged TX and the controller
# garbled ~2/3 of commands). Both fixes are structural — pinned asm for RX,
# hardware PWM/EasyDMA for TX — and this script keeps them that way:
#
#  1. The pinned RX sampling loop must be present with its exact 5-instruction
#     encoding at a word-aligned address (a misaligned branch target costs
#     +1 fetch cycle per iteration on Cortex-M4: 24,576 samples -> +384us).
#  2. The hardware-timed TX path (pwm_tx::write_packet_dma) must be present —
#     nobody quietly reverts command TX to a bit-banged (codegen-timed) path.
#
# Usage: check_timing_invariants.sh <elf> <label>

set -euo pipefail

ELF="$1"
LABEL="${2:-$(basename "$ELF")}"

OBJDUMP=$(ls "$(rustc --print sysroot)"/lib/rustlib/*/bin/llvm-objdump 2>/dev/null | head -1)
if [ -z "$OBJDUMP" ]; then
    echo "FAIL [$LABEL]: llvm-objdump not found (rustup component add llvm-tools)"
    exit 1
fi

# Normalize tabs to spaces so patterns are grep-portable (BSD grep, no -P).
# Work from a temp file: piping a large variable into `grep -q`/`-m1` causes
# SIGPIPE on the writer, which `set -o pipefail` turns into a false FAIL.
DISASM=$(mktemp)
trap 'rm -f "$DISASM"' EXIT
"$OBJDUMP" -d "$ELF" | tr '\t' ' ' > "$DISASM"

# --- Check 1: pinned RX sampling loop, word-aligned, exact encoding --------
# Identify the loop by its full 5-instruction body (several unrelated sites
# can match the head instruction alone), then check the verified head's
# alignment.
ADDR=""
for CAND in $(grep 'ldr\.w r0, \[r12\]' "$DISASM" | sed -E 's/^ *([0-9a-f]+):.*/\1/'); do
    BODY=$(grep -A4 "^ *$CAND:" "$DISASM")
    OK=1
    for PAT in 'str r0, \[r2, r1\]' 'adds r1, #0x4' 'cmp\.w r1, #0x18000' 'bne'; do
        if ! echo "$BODY" | grep -c "$PAT" >/dev/null; then
            OK=0
            break
        fi
    done
    if [ "$OK" -eq 1 ]; then
        ADDR="$CAND"
        break
    fi
done
if [ -z "$ADDR" ]; then
    echo "FAIL [$LABEL]: pinned sampling loop (exact 5-instruction encoding) not found"
    exit 1
fi
if [ $((0x$ADDR % 4)) -ne 0 ]; then
    echo "FAIL [$LABEL]: sampling loop at 0x$ADDR is not word-aligned (% 4 = $((0x$ADDR % 4)))"
    exit 1
fi

# --- Check 2: hardware-timed TX present -------------------------------------
if ! grep -c 'write_packet_dma' "$DISASM" >/dev/null; then
    echo "FAIL [$LABEL]: pwm_tx::write_packet_dma absent — command TX must stay hardware-timed"
    exit 1
fi

echo "OK   [$LABEL]: sampling loop word-aligned @0x$ADDR, exact encoding; hardware TX present"
