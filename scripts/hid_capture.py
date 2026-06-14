#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["hidapi>=0.14"]
# ///
"""Capture and analyze the Pulsar adapter's BLE HID report stream.

Quantifies controller input quality for issue #5 (fighting-game direction
inputs dropping/misordering over BLE). Run it with the adapter connected to
this machine as an Xbox HID gamepad.

    uv run scripts/hid_capture.py --list                 # find the device
    uv run scripts/hid_capture.py --seconds 10           # capture + analyze
    uv run scripts/hid_capture.py --input dpad           # use the hat, not the stick

Two analyses run on the same captured stream:

  1. Link rate/jitter  — inter-arrival of reports while the input is MOVING
     (idle periods are excluded: the firmware sends on-change, so "no report"
     during stillness is not a drop). Gives effective Hz + jitter, the BLE-leg
     equivalent of what gamepadla measures.

  2. Rotation-completeness — bins the stick angle (or the hat) into 8 octants
     and checks that a rotation visits every direction in order. A jump of >=2
     octants between consecutive samples is a *skipped direction* — the exact
     misdirection symptom from issue #5. This is the quantitative version of
     the in-game KEY DISPLAY test.

IMPORTANT: keep the stick (or d-pad) in CONTINUOUS rotation for the whole
capture window, at the speed you actually play at. Static input produces no
reports and nothing to measure.

Report layout parsed (see maple-protocol/src/xbox_hid.rs): report ID 1, then
LX u16le[0:2], LY u16le[2:4], hat low-nibble[12], buttons[13:16].
"""
from __future__ import annotations

import argparse
import math
import statistics
import sys
import time
from collections import Counter

REPORT_LEN = 16
STICK_CENTER = 0x8000
# Xbox hat: 1=N,2=NE,3=E,4=SE,5=S,6=SW,7=W,8=NW (0/9-15 = neutral) -> octant 0..7
# Stick octant 0..7 from atan2; orientation is irrelevant to adjacency, only
# that consecutive octants differ by +/-1 around the circle.


def find_devices():
    import hid
    return hid.enumerate()


def looks_like_gamepad(d: dict) -> bool:
    name = (d.get("product_string") or "").lower()
    return (
        d.get("usage_page") == 0x01 and d.get("usage") == 0x05  # Generic Desktop / Gamepad
        or d.get("vendor_id") == 0x045E  # Microsoft
        or "xbox" in name
        or "dreamcast" in name
        or "wireless controller" in name
    )


def list_devices() -> int:
    devs = find_devices()
    if not devs:
        print("No HID devices found.")
        return 1
    print(f"{'VID:PID':<12} {'usage':<10} product (★ = likely the adapter)")
    for d in devs:
        star = "★" if looks_like_gamepad(d) else " "
        vidpid = f"{d['vendor_id']:04x}:{d['product_id']:04x}"
        usage = f"{d.get('usage_page', 0):#06x}/{d.get('usage', 0):#04x}"
        print(f"{star} {vidpid:<12} {usage:<10} {d.get('product_string') or '?'}")
    return 0


def open_device(args):
    import hid
    devs = find_devices()
    chosen = None
    for d in devs:
        if args.vid and d["vendor_id"] != args.vid:
            continue
        if args.pid and d["product_id"] != args.pid:
            continue
        if args.name and args.name.lower() not in (d.get("product_string") or "").lower():
            continue
        if args.vid or args.pid or args.name or looks_like_gamepad(d):
            chosen = d
            break
    if chosen is None:
        print("No matching gamepad found. Run with --list to see devices, then "
              "pass --vid/--pid/--name.", file=sys.stderr)
        return None
    print(f"Opening {chosen['vendor_id']:04x}:{chosen['product_id']:04x} "
          f"\"{chosen.get('product_string') or '?'}\"")
    h = hid.device()
    h.open_path(chosen["path"])
    h.set_nonblocking(True)
    return h


def parse_report(data: list[int], report_id: int) -> bytes | None:
    """Return the 16-byte gamepad payload, or None for other/!matching reports."""
    if not data:
        return None
    if report_id == 0:                       # device emits unnumbered reports
        payload = data[:REPORT_LEN]
    elif data[0] == report_id:               # numbered: strip the report-id byte
        payload = data[1:1 + REPORT_LEN]
    else:
        return None                          # battery/guide/rumble report — skip
    return bytes(payload) if len(payload) == REPORT_LEN else None


def octant_from_stick(payload: bytes, deadzone: int) -> int | None:
    lx = int.from_bytes(payload[0:2], "little") - STICK_CENTER
    ly = int.from_bytes(payload[2:4], "little") - STICK_CENTER
    if math.hypot(lx, ly) < deadzone:
        return None
    return int(round(math.atan2(ly, lx) / (math.pi / 4))) % 8


def octant_from_hat(payload: bytes) -> int | None:
    hat = payload[12] & 0x0F
    return (hat - 1) if 1 <= hat <= 8 else None


def capture(h, seconds: float, debug: bool):
    """Return list of (t_perf_seconds, data_tuple) for every non-empty read."""
    raw = []
    t_end = time.perf_counter() + seconds
    print(f"Capturing for {seconds:.0f}s — ROTATE the input continuously now...")
    while time.perf_counter() < t_end:
        data = h.read(64)
        t = time.perf_counter()
        if not data:
            time.sleep(0.0005)               # 0.5ms; well below the BLE conn interval
            continue
        raw.append((t, tuple(data)))
        if debug and len(raw) <= 20:
            print(f"  [{t:8.4f}] len={len(data):2d} id={data[0]:3d}  "
                  + " ".join(f"{b:02x}" for b in data[:18]))
    return raw


def parse_all(raw, report_id):
    out = []
    for t, data in raw:
        payload = parse_report(list(data), report_id)
        if payload is not None:
            out.append((t, payload))
    return out


def diagnose_empty():
    print("\n0 reads — nothing arrived from the device. On macOS this is almost always:")
    print("  • Input Monitoring permission: System Settings → Privacy & Security →")
    print("    Input Monitoring → enable your terminal (Terminal/iTerm), fully quit it,")
    print("    reopen, and retry. hid_read returns nothing without it.")
    print("  • The controller must be actively sending — keep an input moving the whole time.")
    print("  • Run --list: if 045e:02e0 appears on multiple rows, the input lives on a")
    print("    different collection — try --debug, or select with --vid/--pid.")


def analyze_rate(samples, motion_gap_cap_ms: float):
    """Inter-arrival stats over active periods (idle gaps excluded)."""
    if len(samples) < 3:
        return None
    intervals = [(b[0] - a[0]) * 1000.0 for a, b in zip(samples, samples[1:])]
    active = [d for d in intervals if d <= motion_gap_cap_ms]
    big_gaps = [d for d in intervals if d > motion_gap_cap_ms]
    if not active:
        return {"n": 0, "big_gaps": len(big_gaps)}
    med = statistics.median(active)
    q = statistics.quantiles(active, n=4) if len(active) >= 4 else [med, med, med]
    return {
        "n": len(active),
        "min": min(active), "median": med, "mean": statistics.fmean(active),
        "p95": _pct(active, 95), "p99": _pct(active, 99), "max": max(active),
        "stdev": statistics.pstdev(active),
        "iqr": q[2] - q[0],
        "hz": 1000.0 / med if med else 0.0,
        "big_gaps": len(big_gaps),
        "max_gap": max(big_gaps) if big_gaps else 0.0,
    }


def _pct(xs, p):
    s = sorted(xs)
    k = max(0, min(len(s) - 1, int(round((p / 100.0) * (len(s) - 1)))))
    return s[k]


def analyze_rotation(samples, octant_fn):
    """Detect skipped / reversed directions in the octant sequence."""
    seq = []  # (t, octant), consecutive duplicates compressed
    for t, payload in samples:
        oct_ = octant_fn(payload)
        if oct_ is None:
            continue
        if not seq or seq[-1][1] != oct_:
            seq.append((t, oct_))
    if len(seq) < 2:
        return {"transitions": 0, "engaged_samples": len(seq)}

    fwd = bwd = skips = reversals = 0
    skip_detail = []
    prev_dir = 0
    for (t0, o0), (t1, o1) in zip(seq, seq[1:]):
        delta = (o1 - o0) % 8
        step = 1 if delta <= 4 else delta - 8   # nearest-direction signed step (-3..+4)
        if abs(step) >= 2:
            skips += 1
            if len(skip_detail) < 12:
                skip_detail.append((o0, o1, abs(step) - 1))  # directions skipped
        else:
            if step > 0:
                fwd += 1
            elif step < 0:
                bwd += 1
            d = 1 if step > 0 else -1
            if prev_dir and d != prev_dir:
                reversals += 1
            prev_dir = d
    dur = seq[-1][0] - seq[0][0]
    # angular distance covered (each clean step = 45 deg); rough rotation rate
    rotations = (fwd + bwd + sum(s[2] + 1 for s in skip_detail)) / 8.0
    return {
        "transitions": len(seq) - 1,
        "engaged_samples": len(seq),
        "forward": fwd, "backward": bwd,
        "skips": skips, "skip_detail": skip_detail,
        "reversals": reversals,
        "approx_rotations": rotations,
        "rotations_per_sec": rotations / dur if dur else 0.0,
    }


def analyze_seq(samples):
    """Decode the debug seq-counter (byte 15 bits 1-7) and count dropped reports.

    Only meaningful against a `seq-counter` firmware build, where the firmware
    stamps an incrementing 7-bit counter per *sent* notification. Gaps in the
    received counter = reports dropped between the firmware and this host.
    """
    seqs = [(p[15] >> 1) & 0x7F for _, p in samples]
    if len(seqs) < 2:
        return None
    received = len(seqs)
    drops = reorders = dupes = 0
    advances = 0                              # consecutive pairs that stepped +1
    for prev, cur in zip(seqs, seqs[1:]):
        gap = (cur - prev) & 0x7F
        if gap == 0:
            dupes += 1
        elif gap == 1:
            advances += 1
        elif gap <= 64:
            drops += gap - 1
        else:
            reorders += 1                    # backward step / wrap ambiguity
    # A real seq-counter build steps +1 almost every report; a build WITHOUT
    # the seq-counter feature leaves byte 15 constant (every gap 0), which the
    # drop logic would otherwise read as a perfect "no loss" — a false pass.
    # Treat the counter as live only if it actually advances most of the time.
    active = advances >= 0.5 * (received - 1)
    sent = received + drops
    return {
        "received": received, "implied_sent": sent, "drops": drops,
        "loss_pct": 100.0 * drops / sent if sent else 0.0,
        "reorders": reorders, "dupes": dupes, "active": active,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--list", action="store_true", help="list HID devices and exit")
    ap.add_argument("--seconds", type=float, default=10.0, help="capture duration (default 10)")
    ap.add_argument("--input", choices=["stick", "dpad"], default="stick",
                    help="rotate the analog stick (default) or the d-pad/hat")
    ap.add_argument("--deadzone", type=int, default=8000,
                    help="stick deadzone in raw counts (default 8000)")
    ap.add_argument("--gap-cap-ms", type=float, default=60.0,
                    help="intervals above this are treated as idle gaps, not link timing (default 60)")
    ap.add_argument("--report-id", type=int, default=1,
                    help="HID report id of the gamepad report; 0 if unnumbered (default 1)")
    ap.add_argument("--debug", action="store_true",
                    help="print the first raw reads (len/id/hex) to diagnose the report format")
    ap.add_argument("--seq", action="store_true",
                    help="decode the debug seq-counter in byte 15 (requires a seq-counter firmware build)")
    ap.add_argument("--vid", type=lambda x: int(x, 0), help="vendor id, e.g. 0x045e")
    ap.add_argument("--pid", type=lambda x: int(x, 0), help="product id")
    ap.add_argument("--name", help="substring match on product name")
    args = ap.parse_args()

    if args.list:
        return list_devices()

    h = open_device(args)
    if h is None:
        return 1
    try:
        raw = capture(h, args.seconds, args.debug)
    finally:
        h.close()

    print(f"\nReceived {len(raw)} raw HID reads in {args.seconds:.0f}s.")
    if not raw:
        diagnose_empty()
        return 1

    len_hist = Counter(len(d) for _, d in raw)
    id_hist = Counter(d[0] for _, d in raw)
    print(f"  lengths: {dict(len_hist)}   first byte (report id?): {dict(id_hist)}")

    samples = parse_all(raw, args.report_id)
    if not samples:
        # Auto-detect: a 17-byte read is [report_id, ...16]; a 16-byte read is unnumbered.
        common_len = len_hist.most_common(1)[0][0]
        guess = id_hist.most_common(1)[0][0] if common_len == 1 + REPORT_LEN else 0
        samples = parse_all(raw, guess)
        if samples:
            print(f"  ⚠ no reports matched --report-id {args.report_id}; auto-detected "
                  f"report id {guess} (pass --report-id {guess} to silence this)")
    print(f"  parsed {len(samples)} gamepad report(s).\n")
    if len(samples) < 3:
        print("Reads arrived but few/none parsed as a 16-byte gamepad report. Re-run with "
              "--debug to see the raw bytes, then set --report-id (0 = unnumbered) accordingly.")
        return 1

    rate = analyze_rate(samples, args.gap_cap_ms)
    print("── Link rate / jitter (active periods only) ──")
    if rate and rate["n"]:
        print(f"  samples: {rate['n']}   effective rate: {rate['hz']:.1f} Hz "
              f"(median interval {rate['median']:.1f} ms)")
        print(f"  interval ms — min {rate['min']:.1f} / med {rate['median']:.1f} / "
              f"mean {rate['mean']:.1f} / p95 {rate['p95']:.1f} / p99 {rate['p99']:.1f} / max {rate['max']:.1f}")
        print(f"  jitter — stdev {rate['stdev']:.1f} ms, IQR {rate['iqr']:.1f} ms")
        if rate["big_gaps"]:
            print(f"  ⚠ {rate['big_gaps']} gap(s) > {args.gap_cap_ms:.0f} ms (max {rate['max_gap']:.0f} ms) "
                  "— idle, or coalesced/dropped during motion")
    else:
        print("  not enough active intervals (keep the input moving)")

    octant_fn = octant_from_hat if args.input == "dpad" else (
        lambda p: octant_from_stick(p, args.deadzone))
    rot = analyze_rotation(samples, octant_fn)
    print(f"\n── Rotation completeness ({args.input}) ──")
    if rot["transitions"] == 0:
        print("  no direction transitions seen — rotate the input through all 8 directions")
    else:
        print(f"  direction transitions: {rot['transitions']}   "
              f"(forward {rot['forward']}, backward {rot['backward']}, reversals {rot['reversals']})")
        print(f"  ≈{rot['approx_rotations']:.1f} rotations at ≈{rot['rotations_per_sec']:.1f} rot/s")
        if rot["skips"]:
            print(f"  ⚠ {rot['skips']} SKIPPED-DIRECTION event(s) — the issue #5 symptom:")
            for o0, o1, n in rot["skip_detail"]:
                print(f"      octant {o0} → {o1}  ({n} direction(s) skipped)")
        else:
            print("  ✓ no skipped directions — every step was to an adjacent direction")

    if args.seq:
        seq = analyze_seq(samples)
        print("\n── Sequence counter (byte 15 — requires a seq-counter firmware build) ──")
        if not seq:
            print("  too few reports")
        elif not seq["active"]:
            print("  byte 15 never advances — this firmware was NOT built with the")
            print("  seq-counter feature, so transit-loss can't be measured. Rebuild with")
            print("  --features seq-counter to use this check. (Skipping — not 'no loss'.)")
        elif seq["received"] and not seq["drops"] and not seq["reorders"]:
            print(f"  received {seq['received']} reports, counter contiguous — NO transit loss.")
            print("  → the low rate is the firmware UNDER-SENDING (send-on-change / conn interval),")
            print("    not the link dropping. The measured Hz is real firmware output.")
        else:
            print(f"  received {seq['received']}, firmware sent ≈{seq['implied_sent']} "
                  f"→ {seq['drops']} dropped in transit ({seq['loss_pct']:.1f}% loss)")
            if seq["reorders"] or seq["dupes"]:
                print(f"  (large-gaps/reorders {seq['reorders']}, duplicates {seq['dupes']})")
            print("  → reports ARE dropped between firmware and host (BLE or macOS HID). Re-run on a")
            print("    Linux host (hidraw) to separate a real BLE drop from macOS delivery coalescing.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
