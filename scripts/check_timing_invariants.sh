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
#     This is check 1 in `cpu` mode. In `spim` mode it is INVERTED — see 4.
#  2. The hardware-timed VMU LCD TX must stay on the PWM/EasyDMA path: its
#     waveform builder (pwm_tx::WaveformBuilder) must be present in the binary —
#     it disappears if the LCD write is reverted to bit-bang. (write_lcd_dma
#     itself inlines away under LTO, so we key off the builder it calls. The
#     controller command TX is intentionally bit-bang at this revision.)
#
#  3. In `cpu` mode the loop must not start on a 16-byte flash line (check 1a),
#     and optionally (`cpu:<addr>`) must sit at an exact expected address.
#     The loop's cycle count is the sample rate the decode thresholds are
#     calibrated to, and it depends on where the 14-byte loop falls against the
#     flash's 16-byte fetch lines: every Control-healthy build since v242 has it
#     at 0x1c mod 32, and every build with it 16-byte aligned failed (v210,
#     v253, v257, v261, v278-v280; v282 measured 11.00 cycles per
#     sample at the two 16-byte-aligned phases against 8.00 at the other six).
#     memory.x USED to pin the Maple code in .maple_text so the address was a
#     property of the source rather than of the link; a later revision removed
#     that pad, because production no longer compiles a sampler at all. The DK
#     still does, so the phase check carries that protection now — it encodes
#     the measured cause instead of one proven address, which an exact hold
#     over-constrains (six of the eight phases are fine). No call site uses
#     `cpu:<addr>` today; it stays for pinning a specific build under test.
#
#  4. The carrier and the XIAO read every Maple reply on
#     the SPIM/EasyDMA capture, and the DK alone keeps the CPU sampler. For
#     those two boards the invariant is the opposite one — the pinned loop must
#     be ABSENT, because its presence means a CPU sampling loop came back into
#     a build whose decode thresholds no longer stand behind one. `spim` mode
#     checks that, keyed off the same 5-instruction encoding check 1 keys off,
#     the way check 2 keys off the PWM waveform builder.
#
# Usage: check_timing_invariants.sh <elf> <label> [cpu[:<expected-address>] | spim]
#
#   (default `cpu`; a bare hex address is accepted as `cpu:<address>` so older
#   call sites keep working)

set -euo pipefail

ELF="$1"
LABEL="${2:-$(basename "$ELF")}"
ARG="${3:-cpu}"

# Mode + optional held address. `cpu` is the default and `<addr>` on its own
# still means `cpu:<addr>`.
EXPECT=""
case "$ARG" in
    "" | cpu) MODE=cpu ;;
    cpu:*)
        MODE=cpu
        EXPECT="${ARG#cpu:}"
        if [ -z "$EXPECT" ]; then
            echo "FAIL [$LABEL]: 'cpu:' given with no address"
            exit 1
        fi
        ;;
    spim) MODE=spim ;;
    0x* | [0-9]*)
        MODE=cpu
        EXPECT="$ARG"
        ;;
    *)
        echo "FAIL [$LABEL]: unknown mode '$ARG' (expected cpu, cpu:<address> or spim)"
        exit 1
        ;;
esac

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

# --- Locate the pinned RX sampling loop -------------------------------------
# Identify the loop by its full 5-instruction body (several unrelated sites
# can match the head instruction alone). Both modes need the answer: `cpu`
# requires it, `spim` forbids it.
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

if [ "$MODE" = spim ]; then
    # --- Check 1 (inverted): no CPU sampling loop in a SPIM-capture build ----
    if [ -n "$ADDR" ]; then
        echo "FAIL [$LABEL]: pinned CPU sampling loop present @0x$ADDR — this board's RX"
        echo "      capture is the SPIM/EasyDMA path (maple::spim_capture) and a"
        echo "      CPU sampling loop has come back. The decode thresholds behind this build"
        echo "      are not calibrated to a loop's cycle count any more; a sampler here is a"
        echo "      regression, not a fallback. Check read_packet_bulk still captures on the"
        echo "      board's spim_capture Parts, as the LCD TX must stay on PWM/EasyDMA."
        exit 1
    fi
else
    # --- Check 1: pinned RX sampling loop, word-aligned, exact encoding ------
    if [ -z "$ADDR" ]; then
        echo "FAIL [$LABEL]: pinned sampling loop (exact 5-instruction encoding) not found"
        exit 1
    fi
    if [ $((0x$ADDR % 4)) -ne 0 ]; then
        echo "FAIL [$LABEL]: sampling loop at 0x$ADDR is not word-aligned (% 4 = $((0x$ADDR % 4)))"
        exit 1
    fi

    # --- Check 1a: the loop must not start on a 16-byte flash line ----------
    # The measured cause, not a proven address: the 14-byte loop straddles a
    # 16-byte fetch line, and v282 clocked 11.00 cycles per sample at the two
    # 16-byte-aligned phases against 8.00 at the other six. Every Control
    # failure attributed to placement (v210, v253, v257, v261, v278-v280) had
    # the loop 16-byte aligned; every healthy build did not.
    #
    # This replaces what memory.x's .maple_text pad used to guarantee. The pad
    # was removed because production no longer compiles a
    # sampler at all — but the DK still does, and deleting the section would
    # otherwise have left its loop wherever the link happened to put it. An
    # exact address hold would over-constrain (six of eight phases are fine);
    # this encodes the physics instead.
    if [ $((0x$ADDR % 16)) -eq 0 ]; then
        echo "FAIL [$LABEL]: sampling loop at 0x$ADDR starts on a 16-byte flash line."
        echo "      The 14-byte loop then straddles two fetch lines and costs +1 cycle per"
        echo "      iteration — 11.00 cycles/sample against 8.00 (v282), which is the"
        echo "      placement failure the guard exists for. Six of the eight 4-byte phases are"
        echo "      fine; this build drew one of the two that are not. Shift the code ahead"
        echo "      of it, or pad, and re-check."
        exit 1
    fi

    # --- Check 1b: the loop is where the proven build had it ----------------
    if [ -n "$EXPECT" ] && [ $((0x$ADDR)) -ne $((EXPECT)) ]; then
        printf 'FAIL [%s]: sampling loop at 0x%s, expected %s — the Maple code moved.\n' \
            "$LABEL" "$ADDR" "$EXPECT"
        echo "      Prove the new address with a Control capture, then update the expected"
        echo "      address at the call site. (The .maple_text pad that used to place this"
        echo "      loop was removed — see check 1a.)"
        exit 1
    fi
fi

# --- Check 2: hardware-timed TX present -------------------------------------
if ! grep -c 'WaveformBuilder' "$DISASM" >/dev/null; then
    echo "FAIL [$LABEL]: pwm_tx::WaveformBuilder absent — VMU LCD TX must stay on the PWM/EasyDMA path"
    exit 1
fi

if [ "$MODE" = spim ]; then
    echo "OK   [$LABEL]: no CPU sampling loop (RX on the SPIM/EasyDMA capture); hardware TX present"
else
    HELD=""
    [ -n "$EXPECT" ] && HELD=", held to $EXPECT"
    # The phase is printed because it, not the address, is what decides the
    # sample rate — a reader comparing two builds needs to see it move.
    printf 'OK   [%s]: sampling loop @0x%s (0x%x mod 32, off the 16-byte line)%s, exact encoding; hardware TX present\n' \
        "$LABEL" "$ADDR" "$((0x$ADDR % 32))" "$HELD"
fi
