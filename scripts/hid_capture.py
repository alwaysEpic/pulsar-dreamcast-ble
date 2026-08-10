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

RUNNING LOG: every capture is appended to ~/.pulsar/hid_capture_runs.jsonl with
the date and the git commit it was captured against, so runs stay comparable
across builds. It lives outside the repo on purpose — it records this bench's
hardware, not the source tree, and must survive branch switches without ever
appearing in a diff. `--history` prints the last N runs; `--no-record` skips
the append; `--board` and `--note` label a run for later reading.
"""
from __future__ import annotations

import argparse
import datetime
import json
import math
import pathlib
import statistics
import subprocess
import sys
import time
from collections import Counter

# Running capture log. Deliberately OUTSIDE the repo: it is a record of this
# bench's hardware runs, not of the source tree, so it must survive branch
# switches and clean checkouts and must never show up in a diff. Each line
# carries the commit it was captured against, which is what makes runs
# comparable across builds.
DEFAULT_RECORD = pathlib.Path.home() / ".pulsar" / "hid_capture_runs.jsonl"

REPORT_LEN = 16
STICK_CENTER = 0x8000

# Samples-per-direction thresholds for interpreting skipped directions.
# samples/direction = hz / (rot_per_sec * 8). Below FORCED the capture cannot
# resolve a rotation at all and skips are geometric, not a link defect; between
# FORCED and NOISY the count is unstable (healthy builds have produced 0-6).
# Only above NOISY is a skip evidence of the issue #5 symptom.
RESOLUTION_FORCED = 2.0
RESOLUTION_NOISY = 3.0
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


def _git_info():
    """Commit / branch / dirty for the tree this script lives in. Never raises."""
    here = pathlib.Path(__file__).resolve().parent

    def run(*a):
        try:
            r = subprocess.run(["git", "-C", str(here), *a],
                               capture_output=True, text=True, timeout=5)
            return r.stdout.strip() if r.returncode == 0 else None
        except (OSError, subprocess.SubprocessError):
            return None

    return {
        "commit": run("rev-parse", "--short", "HEAD"),
        "branch": run("rev-parse", "--abbrev-ref", "HEAD"),
        # A dirty tree means the commit does NOT describe the running firmware.
        # `--untracked-files=no` is load-bearing: this repo permanently carries
        # untracked CAD (3d_files/*.blend, *.step), so a plain --porcelain marks
        # EVERY run dirty and the flag becomes noise you learn to ignore. Only
        # modifications to tracked sources can change the firmware.
        "dirty": bool(run("status", "--porcelain", "--untracked-files=no")),
    }


def append_record(path: pathlib.Path, record: dict) -> int | None:
    """Append one run as JSONL. Returns the new total, or None on failure."""
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(record, separators=(",", ":")) + "\n")
        with path.open("r", encoding="utf-8") as fh:
            return sum(1 for _ in fh)
    except OSError as e:
        print(f"  (could not write {path}: {e})", file=sys.stderr)
        return None


def print_history(path: pathlib.Path, limit: int) -> int:
    """Print the last `limit` recorded runs as a table."""
    if not path.exists():
        print(f"No capture log yet at {path}")
        print("Run a capture without --no-record to start one.")
        return 0
    rows = []
    with path.open(encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if line:
                try:
                    rows.append(json.loads(line))
                except json.JSONDecodeError:
                    continue  # tolerate a torn line rather than lose the log
    if not rows:
        print(f"{path} is empty")
        return 0
    shown = rows[-limit:]
    print(f"── Last {len(shown)} of {len(rows)} run(s) — {path} ──")
    hdr = (f"{'date':<17} {'commit':<12} {'board':<9} {'Hz':>5} {'med':>5} "
           f"{'IQR':>5} {'p95':>5} {'p99':>6} {'max':>6} {'rev':>4} {'skip':>5} {'s/dir':>6}")
    print(hdr)
    print("-" * len(hdr))
    for r in shown:
        c = (r.get("commit") or "?") + ("*" if r.get("dirty") else "")
        spo = r.get("samples_per_direction")
        print(f"{(r.get('date') or '?')[:17]:<17} {c:<12} {(r.get('board') or '-'):<9} "
              f"{_f(r.get('hz'), 1, 5)} {_f(r.get('median_ms'), 1, 5)} "
              f"{_f(r.get('iqr_ms'), 1, 5)} {_f(r.get('p95_ms'), 1, 5)} "
              f"{_f(r.get('p99_ms'), 1, 6)} {_f(r.get('max_ms'), 1, 6)} "
              f"{_i(r.get('reversals'), 4)} {_i(r.get('skips'), 5)} {_f(spo, 2, 6)}")
    print("\n  * = captured against a dirty tree; the commit does not describe that firmware.")
    print("  Healthy signature: ~66.6 Hz, median 15.0 ms, IQR ~0.9 ms, reversals 0.")
    print("  IQR is the tell, not Hz. Skips only mean anything at s/dir ≥ 3.")
    return 0


def _f(v, nd, w):
    return f"{v:>{w}.{nd}f}" if isinstance(v, (int, float)) else f"{'-':>{w}}"


def _i(v, w):
    return f"{v:>{w}d}" if isinstance(v, int) else f"{'-':>{w}}"


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


GAUGE_MAGIC = 0xA5
CONNPARAM_MAGIC = 0xC5

# poll-period-debug channel: byte 7 = 0xB0 | tag, bytes 4-5 = LE u16 value,
# byte 6 = window counter (low 8 bits). Values are means over a 32-poll window
# computed on-device from the DWT cycle counter (see src/poll_period.rs).
PP_MAGIC_BASE = 0xB0
PP_WINDOW = 32
PP_TAGS = {
    0: ("period mean", "µs"),
    1: ("period max", "µs"),
    2: ("get_condition mean", "µs"),
    3: ("sleep mean", "µs"),
    4: ("retries/window", "count"),
    5: ("cadence overruns", "count, cumulative"),
    6: ("radio notifications", "count, wrapping"),
}


def analyze_pollperiod(samples):
    """Decode poll-loop period telemetry from the right-stick bytes (4-7).

    Only meaningful against a `poll-period-debug` firmware build. The firmware
    publishes window means of the poll period and its attribution (get_condition
    span, sleep span, retry count) as rotating tagged payloads, so ONE capture
    answers what previously took a day of exact-binary A/B: did this binary
    layout roll a healthy poll loop?
    """
    by_tag = {}
    span = {}  # tag -> [first_t, first_v, last_t, last_v] for rate derivation
    non_magic = 0
    for t, p in samples:
        tag = p[7] ^ PP_MAGIC_BASE
        if tag not in PP_TAGS:
            non_magic += 1
            continue
        v = int.from_bytes(p[4:6], "little")
        # Values repeat until the next window flush — dedup per (window, tag)
        # so slow-rotation captures don't overweight long-lived windows.
        by_tag.setdefault(tag, {})[p[6]] = v
        if tag not in span:
            span[tag] = [t, v, t, v]
        else:
            span[tag][2], span[tag][3] = t, v
    stats = {}
    for tag, wins in by_tag.items():
        vals = list(wins.values())
        t0, v0, t1, v1 = span[tag]
        rate = ((v1 - v0) & 0xFFFF) / (t1 - t0) if t1 > t0 else None
        stats[tag] = {
            "n": len(vals), "mean": statistics.fmean(vals),
            "min": min(vals), "max": max(vals), "last": vals[-1],
            "rate": rate,
        }
    return {"stats": stats, "non_magic": non_magic, "total": len(samples)}

# sd_ble_gap_conn_param_update return codes worth naming (nrf_error.h).
NRF_RC = {
    0x00: "NRF_SUCCESS — request queued (NOT the same as accepted)",
    0x08: "NRF_ERROR_INVALID_STATE — not connected / wrong state",
    0x07: "NRF_ERROR_INVALID_PARAM — parameters rejected locally by the SoftDevice",
    0x11: "NRF_ERROR_BUSY — another procedure in flight; THE REQUEST NEVER WENT OUT",
    0x0C: "NRF_ERROR_DATA_SIZE",
    0x10: "NRF_ERROR_TIMEOUT",
    0x13: "BLE_ERROR_INVALID_CONN_HANDLE",
    0xFF: "(no attempt recorded)",
}


def analyze_gauge(samples):
    """Decode IP5306 gauge samples smuggled in the right-stick bytes (4-7).

    Only meaningful against a `gauge-debug` firmware build. pulsarv1 has no SWD
    probe and the XIAO has no onboard debugger, so RTT can't see this board —
    and the gauge has to be characterized *on battery, untethered*, which is
    exactly what a wired channel would disturb. The Dreamcast has no right
    stick, so bytes 4-7 are otherwise a constant 0x8000/0x8000.

    Layout (LE u32): [raw 0x78, decoded %, flags, MAGIC]; flags bit0=charging,
    bit1=charge-complete. Returns one entry per *distinct* sample, timestamped
    at first sighting, since the firmware only re-reads the gauge every 60 s.
    """
    seen, timeline = set(), []
    non_magic = 0
    for t, p in samples:
        if p[7] != GAUGE_MAGIC:
            non_magic += 1
            continue
        raw, pct, flags = p[4], p[5], p[6]
        key = (raw, pct, flags)
        if key not in seen:
            seen.add(key)
            timeline.append((t, raw, pct, bool(flags & 0x01), bool(flags & 0x02)))
    return {"timeline": timeline, "non_magic": non_magic, "total": len(samples)}


def analyze_connparam(samples):
    """Decode BLE connection-parameter state from the right-stick bytes (4-7).

    Only meaningful against a `connparam-debug` firmware build.

    Layout: [rc, min_interval, max_interval, MAGIC]. Intervals are in 1.25 ms
    units (12 = 15 ms, 9 = 11.25 ms) and are the **live negotiated** values the
    SoftDevice holds, not what the firmware requested. `rc` is the raw return of
    `sd_ble_gap_conn_param_update`; 0xFF means no attempt has been recorded yet.

    This exists to separate two states that are identical from the host side:
    the central declining our request, versus the request never being issued
    (NRF_ERROR_BUSY = 17, if another procedure was in flight).
    """
    seen, timeline = set(), []
    non_magic = 0
    for t, p in samples:
        if p[7] != CONNPARAM_MAGIC:
            non_magic += 1
            continue
        key = (p[4], p[5], p[6])
        if key not in seen:
            seen.add(key)
            timeline.append((t, p[4], p[5], p[6]))
    return {"timeline": timeline, "non_magic": non_magic, "total": len(samples)}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--list", action="store_true", help="list HID devices and exit")
    ap.add_argument("--history", nargs="?", type=int, const=20, metavar="N",
                    help="print the last N recorded runs (default 20) and exit")
    ap.add_argument("--no-record", action="store_true",
                    help="do not append this run to the capture log")
    ap.add_argument("--record-file", type=pathlib.Path, default=DEFAULT_RECORD,
                    help=f"capture log path (default {DEFAULT_RECORD})")
    ap.add_argument("--board", help="board this run was captured against, e.g. pulsarv1")
    ap.add_argument("--note", help="free-text label for this run, e.g. 'post ip5306 RMW fix'")
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
    ap.add_argument("--dump-raw", type=pathlib.Path, metavar="PATH",
                    help="write every parsed report as JSONL (t, seq, lx, ly) for offline "
                         "gap classification — seq is byte 15 bits 1-7 (seq-counter builds)")
    ap.add_argument("--connparam", action="store_true",
                    help="decode BLE connection parameters (connparam-debug build)")
    ap.add_argument("--gauge", action="store_true",
                    help="decode IP5306 gauge samples from bytes 4-7 (needs a gauge-debug build; "
                         "the right stick is corrupted in that build by design)")
    ap.add_argument("--pollperiod", action="store_true",
                    help="decode poll-loop period telemetry from bytes 4-7 "
                         "(requires a poll-period-debug firmware build)")
    ap.add_argument("--seq", action="store_true",
                    help="decode the debug seq-counter in byte 15 (requires a seq-counter firmware build)")
    ap.add_argument("--vid", type=lambda x: int(x, 0), help="vendor id, e.g. 0x045e")
    ap.add_argument("--pid", type=lambda x: int(x, 0), help="product id")
    ap.add_argument("--name", help="substring match on product name")
    args = ap.parse_args()

    if args.list:
        return list_devices()

    if args.history is not None:
        return print_history(args.record_file, args.history)

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

    if args.dump_raw:
        try:
            with args.dump_raw.open("w", encoding="utf-8") as fh:
                for t, p in samples:
                    fh.write(json.dumps({
                        "t": round(t, 6),
                        "seq": (p[15] >> 1) & 0x7F,
                        "lx": int.from_bytes(p[0:2], "little"),
                        "ly": int.from_bytes(p[2:4], "little"),
                        # right-stick bytes: debug side-channels (gauge-debug /
                        # connparam-debug / maple-fail-debug) or 0x8000/0x8000
                        "b4": p[4], "b5": p[5], "b6": p[6], "b7": p[7],
                    }, separators=(",", ":")) + "\n")
            print(f"  raw samples → {args.dump_raw}")
        except OSError as e:
            print(f"  (could not write {args.dump_raw}: {e})", file=sys.stderr)

    rate = analyze_rate(samples, args.gap_cap_ms)
    print("── Link rate / jitter (active periods only) ──")
    if rate and rate["n"]:
        print(f"  samples: {rate['n']}   effective rate: {rate['hz']:.1f} Hz "
              f"(median interval {rate['median']:.1f} ms)")
        print(f"  interval ms — min {rate['min']:.1f} / med {rate['median']:.1f} / "
              f"mean {rate['mean']:.1f} / p95 {rate['p95']:.1f} / p99 {rate['p99']:.1f} / max {rate['max']:.1f}")
        print(f"  jitter — stdev {rate['stdev']:.1f} ms, IQR {rate['iqr']:.1f} ms")
        # The layout-lottery acceptance metric (2026-08-05): with a clean
        # unimodal interval distribution this is ~0; a population of doubled
        # conn intervals at fraction f pushes the mean up by ~f while the
        # median stays put, so skew ≈ doubled-interval fraction. Baseline
        # band 1.3-4.0%; the bad rolls measured 6-30%.
        skew = (rate["mean"] - rate["median"]) / rate["median"] if rate["median"] else 0.0
        print(f"  skew (mean−median)/median: {100 * skew:.1f}% "
              "≈ doubled-interval fraction (baseline band 1.3-4.0%)")
        if rate["big_gaps"]:
            print(f"  ⚠ {rate['big_gaps']} gap(s) > {args.gap_cap_ms:.0f} ms (max {rate['max_gap']:.0f} ms) "
                  "— idle, or coalesced/dropped during motion")
    else:
        print("  not enough active intervals (keep the input moving)")

    octant_fn = octant_from_hat if args.input == "dpad" else (
        lambda p: octant_from_stick(p, args.deadzone))
    rot = analyze_rotation(samples, octant_fn)

    # Samples per direction gates how skips should be read, and is recorded so a
    # past run's skip count stays interpretable without re-deriving the rate.
    rps = rot.get("rotations_per_sec")
    hz = rate.get("hz") if rate else None  # analyze_rate returns None on tiny captures
    spo = (hz / (rps * 8.0)) if (rps and hz) else None

    print(f"\n── Rotation completeness ({args.input}) ──")
    if rot["transitions"] == 0:
        print("  no direction transitions seen — rotate the input through all 8 directions")
    else:
        print(f"  direction transitions: {rot['transitions']}   "
              f"(forward {rot['forward']}, backward {rot['backward']}, reversals {rot['reversals']})")
        print(f"  ≈{rot['approx_rotations']:.1f} rotations at ≈{rot['rotations_per_sec']:.1f} rot/s")

        # Skips are a derivative of sample rate, not automatically a defect.
        # Resolving a rotation needs samples-per-octant = hz / (rot_per_sec * 8);
        # below ~2 skips are geometrically forced no matter how healthy the link,
        # and healthy builds have produced 0-6 in the 2.5-3.3 rot/s band. Warning
        # on any non-zero count made this cry wolf in three separate sessions.
        if rot["skips"]:
            if spo is None:
                verdict = None
            elif spo < RESOLUTION_FORCED:
                verdict = (f"expected — only {spo:.1f} samples/direction; skips are forced "
                           f"below {RESOLUTION_FORCED:.0f}. Rotate slower to test this.")
            elif spo < RESOLUTION_NOISY:
                verdict = (f"inconclusive — {spo:.1f} samples/direction is the noisy band "
                           f"(healthy builds give 0-6 here). Rotate at ≲2 rot/s to test this.")
            else:
                verdict = None

            if verdict:
                print(f"  · {rot['skips']} skipped-direction event(s) — {verdict}")
            else:
                print(f"  ⚠ {rot['skips']} SKIPPED-DIRECTION event(s) — the issue #5 symptom"
                      + (f" ({spo:.1f} samples/direction, enough to resolve):" if spo else ":"))
                for o0, o1, n in rot["skip_detail"]:
                    print(f"      octant {o0} → {o1}  ({n} direction(s) skipped)")
        else:
            print("  ✓ no skipped directions — every step was to an adjacent direction")

        # `reversals` is the unconditional tell: it needs no rate correction,
        # because no sample rate can invent an out-of-order direction.
        if rot["reversals"]:
            print(f"  ⚠ {rot['reversals']} REVERSAL(s) — directions arrived out of order")

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

    if args.pollperiod:
        pp = analyze_pollperiod(samples)
        print("\n── Poll-loop period (bytes 4-7 — requires a poll-period-debug build) ──")
        if not pp["stats"]:
            print(f"  no 0xB0-0xB5 tag in byte 7 across {pp['total']} report(s) —")
            print("  this firmware was NOT built with the poll-period-debug feature. Rebuild")
            print("  with --features board-pulsarv1,rtt,seq-counter,poll-period-debug.")
        else:
            if pp["non_magic"]:
                print(f"  ({pp['non_magic']}/{pp['total']} report(s) without a telemetry tag)")
            print(f"    {'channel':>20}  {'windows':>7}  {'mean':>8}  {'min':>7}  {'max':>7}")
            for tag in sorted(pp["stats"]):
                s = pp["stats"][tag]
                name, unit = PP_TAGS[tag]
                print(f"    {name:>20}  {s['n']:7d}  {s['mean']:8.0f}  {s['min']:7d}  {s['max']:7d}  ({unit})")
            st = pp["stats"]
            if 0 in st and 2 in st and 3 in st:
                other = st[0]["mean"] - st[2]["mean"] - st[3]["mean"]
                print(f"    residual (period − get_cond − sleep): ≈{other:.0f} µs "
                      "= VMU write + battery/IP5306 + loop overhead")
            if 4 in st:
                per_poll = st[4]["mean"] / PP_WINDOW
                print(f"    retries: ≈{per_poll:.2f}/poll — the Maple/BLE collision rate. "
                      "A stretched period WITH raised retries = the coupled-oscillator "
                      "signature; stretched WITHOUT retries = look elsewhere.")
            if 5 in st:
                print(f"    cadence overruns since boot: {st[5]['last']}"
                      " (0 on a pre-anchor build; on an anchored build, >0/s = this "
                      "layout's body exceeds the period budget — a bad roll, caught on-device)")
            if 6 in st and st[6]["rate"] is not None:
                print(f"    radio notifications: ≈{st[6]['rate']:.0f}/s "
                      "(healthy ≈133/s = 2 edges × 66.6 conn events/s; low or bursty = "
                      "the quiet-window gate is starving at its input, upstream of "
                      "classification)")
            print("    NOTE: on a poll-period-debug build the --seq loss figure is NOT link "
                  "loss: the rotating tag defeats wire dedup, the ~125Hz notify loop then "
                  "attempts more sends than there are connection events, and the surplus "
                  "is rejected queue-full — each reject consumes a seq (benign, "
                  "run #44: 29.7% 'loss' at a perfectly healthy 66Hz). Judge transit "
                  "loss only on builds without poll-period-debug.")

    if args.connparam:
        c = analyze_connparam(samples)
        print("\n── BLE connection parameters (bytes 4-7 — requires a connparam-debug build) ──")
        if not c["timeline"]:
            print(f"  no magic byte 0x{CONNPARAM_MAGIC:02X} in byte 7 across {c['total']} report(s) —")
            print("  this firmware was NOT built with the connparam-debug feature. Rebuild with")
            print("  --features board-pulsarv1,connparam-debug to use this check.")
        else:
            if c["non_magic"]:
                print(f"  ⚠ {c['non_magic']}/{c['total']} report(s) lacked the magic byte")
            print(f"    {'t (s)':>8}  {'interval':>18}  update-request result")
            for t, rc, mn, mx in c["timeline"]:
                span = (f"{mn * 1.25:.2f} ms" if mn == mx
                        else f"{mn * 1.25:.2f}-{mx * 1.25:.2f} ms")
                print(f"    {t:8.1f}  {span:>18}  rc={rc} "
                      f"({NRF_RC.get(rc, 'unknown')})")
            _, rc0, mn0, _ = c["timeline"][0]
            print()
            if rc0 == 0x11:
                print("  → THE REQUEST NEVER WENT OUT. BUSY means another procedure (bonding,")
                print("    started 500ms earlier by request_security) was still in flight, so the")
                print("    host never saw it. Every interval conclusion so far is unfounded —")
                print("    retry the call until it is accepted, then re-measure.")
            elif rc0 == 0x00:
                print(f"  → The request WAS queued and sent. The host negotiated {mn0 * 1.25:.2f} ms,")
                print("    so this is the central's decision, not a firmware bug. If that is above")
                print("    what was asked for, the host simply declines to go faster.")
            elif rc0 == 0xFF:
                print("  → No attempt recorded — the update call site never ran on this connection.")
            else:
                print(f"  → The SoftDevice refused the call locally (rc={rc0}); it never reached")
                print("    the host. Fix the call before drawing any conclusion about the central.")

    if args.gauge:
        g = analyze_gauge(samples)
        print("\n── IP5306 gauge (bytes 4-7 — requires a gauge-debug firmware build) ──")
        if not g["timeline"]:
            print(f"  no magic byte 0x{GAUGE_MAGIC:02X} in byte 7 across {g['total']} report(s) —")
            print("  this firmware was NOT built with the gauge-debug feature. Rebuild with")
            print("  --features board-pulsarv1,gauge-debug to use this check.")
        else:
            if g["non_magic"]:
                print(f"  ⚠ {g['non_magic']}/{g['total']} report(s) lacked the magic byte")
            print(f"  {len(g['timeline'])} distinct sample(s) "
                  "(the firmware re-reads the gauge every 60 s):")
            print(f"    {'t (s)':>8}  {'0x78':>5}  {'bits 7:4':>9}  {'decoded':>8}  state")
            for t, raw, pct, chg, full in g["timeline"]:
                nib = format(raw >> 4, "04b")
                state = "charging" if chg else ("full" if full else "discharging")
                print(f"    {t:8.1f}  0x{raw:02X}   {nib:>9}  {pct:6d} %  {state}")
            print("  → Log these against a known cell voltage to rebuild the map. Bits 7:4 are")
            print("    believed to be the 4 gauge LEDs, active-LOW (0000 = all lit = 100 %).")

    if not args.no_record:
        git = _git_info()
        record = {
            "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
            "commit": git["commit"],
            "branch": git["branch"],
            "dirty": git["dirty"],
            "board": args.board,
            "note": args.note,
            "seconds": args.seconds,
            "input": args.input,
            "samples": rate.get("n") if rate else 0,
            "hz": _r(rate.get("hz") if rate else None),
            "median_ms": _r(rate.get("median") if rate else None),
            "mean_ms": _r(rate.get("mean") if rate else None),
            "min_ms": _r(rate.get("min") if rate else None),
            "p95_ms": _r(rate.get("p95") if rate else None),
            "p99_ms": _r(rate.get("p99") if rate else None),
            "max_ms": _r(rate.get("max") if rate else None),
            "stdev_ms": _r(rate.get("stdev") if rate else None),
            "iqr_ms": _r(rate.get("iqr") if rate else None),
            "big_gaps": rate.get("big_gaps") if rate else None,
            "transitions": rot.get("transitions"),
            "reversals": rot.get("reversals"),
            "skips": rot.get("skips"),
            "rotations_per_sec": _r(rot.get("rotations_per_sec")),
            "samples_per_direction": _r(spo, 2),
        }
        total = append_record(args.record_file, record)
        if total is not None:
            dirty = " (dirty tree)" if git["dirty"] else ""
            print(f"\n  recorded run #{total} @ {git['commit'] or 'no-git'}{dirty} "
                  f"→ {args.record_file}")
            print("  compare with --history")
    return 0


def _r(v, nd=1):
    """Round for storage; keeps the log readable and diff-stable."""
    return round(v, nd) if isinstance(v, (int, float)) else None


if __name__ == "__main__":
    sys.exit(main())
