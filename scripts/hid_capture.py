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


def list_devices(verbose: bool) -> int:
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
        if verbose:
            for key in (
                "manufacturer_string",
                "serial_number",
                "release_number",
                "interface_number",
                "path",
            ):
                value = d.get(key)
                if isinstance(value, bytes):
                    value = value.decode(errors="replace")
                print(f"    {key}: {value!r}")
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
        # untracked CAD (hardware/enclosure/*.blend, *.step), so a plain --porcelain marks
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


def print_history(path: pathlib.Path, limit: int, unit: str | None = None) -> int:
    """Print the last `limit` recorded runs as a table, optionally one unit's only."""
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
    scope = ""
    if unit:
        # Filter before slicing, so `limit` counts this unit's runs, not all runs.
        rows = [r for r in rows if r.get("unit") == unit]
        scope = f" for {unit}"
        if not rows:
            print(f"No runs recorded{scope} — captures made before --unit existed are untagged")
            return 0
    shown = rows[-limit:]
    print(f"── Last {len(shown)} of {len(rows)} run(s){scope} — {path} ──")
    hdr = (f"{'date':<17} {'commit':<12} {'unit':<10} {'board':<9} {'Hz':>5} {'med':>5} "
           f"{'IQR':>5} {'p95':>5} {'p99':>6} {'max':>6} {'rev':>4} {'skip':>5} {'s/dir':>6}")
    print(hdr)
    print("-" * len(hdr))
    for r in shown:
        c = (r.get("commit") or "?") + ("*" if r.get("dirty") else "")
        spo = r.get("samples_per_direction")
        print(f"{(r.get('date') or '?')[:17]:<17} {c:<12} {(r.get('unit') or '-'):<10} "
              f"{(r.get('board') or '-'):<9} "
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
    # Capture-health counters (v297). Cumulative since boot, saturating at
    # 65535 on-device — so these are read as a difference across the capture,
    # not as the last value. Tags 8/9 read gpio_bus::NO_TRIGGER / INCOMPLETE,
    # which exist only on the SPIM boards; a CPU-sampling build publishes 0.
    #
    # Ported to `main`'s telemetry 2026-09-18: the counters behind
    # 8/9 already incremented there, only the payload table was missing. Before
    # that they lived on bench branches only, and this script decoded
    # them anyway because it reads captures from those branches and got them
    # wrong once already.
    7: ("polls", "count, cumulative"),
    8: ("SPIM no-trigger", "count, cumulative"),
    9: ("SPIM incomplete", "count, cumulative"),
    # NOT cumulative-saturating, however it was documented and read on v297:
    # this is `MAPLE_FAIL_TOTAL`, a plain wrapping `fetch_add` on a u16 with no
    # saturation anywhere in its path. Classified with tag 6 below.
    10: ("get_condition fails", "count, WRAPPING"),
    11: ("app version", "build number"),
    12: ("polls, no frame shadow", "count, cumulative"),
    13: ("no-trigger, no shadow", "count, cumulative"),
    # Which arm of an interleaved A/B the binary is. Two arms are the same
    # version by necessity — downgrade prevention refuses a lower one — so
    # without this nothing on the wire says which is running, which is the gap
    # the v299 pair had to close with staged payload shas.
    14: ("A/B arm", "0 = baseline, 1 = candidate"),
}

# Counters that only ever climb on-device and saturate at 65535 (`sat_count`
# in `poll_period.rs`). Read as a difference across the capture. A DECREASE is
# therefore impossible in normal operation: it means the counter was reset —
# the unit rebooted, or DFU'd — and the two ends belong to different
# populations. Subtracting across one silently invents exposure: 20,000 -> 100
# read as 45,636 polls with zero failures on a real v297 capture.
PP_CUMULATIVE = (5, 7, 8, 9, 12, 13)

# Counters backed by a u16 that genuinely wraps. Here a decrease is expected
# and modulo subtraction is the right reading — but only while the counter
# wraps at most once between reports, which is why these are kept apart from
# the saturating ones rather than sharing their arithmetic.
PP_WRAPPING = (6, 10)


def analyze_pollperiod(samples):
    """Decode poll-loop period telemetry from the right-stick bytes (4-7).

    Only meaningful against a `poll-period-debug` firmware build. The firmware
    publishes window means of the poll period and its attribution (get_condition
    span, sleep span, retry count) as rotating tagged payloads, so ONE capture
    answers what previously took a day of exact-binary A/B: did this binary
    layout roll a healthy poll loop?
    """
    by_tag = {}
    series = {}  # tag -> [(t, value), ...] in report order
    non_magic = 0
    for t, p in samples:
        tag = p[7] ^ PP_MAGIC_BASE
        if tag not in PP_TAGS:
            non_magic += 1
            continue
        v = int.from_bytes(p[4:6], "little")
        # Values repeat until the next window flush — dedup per (window, tag)
        # so slow-rotation captures don't overweight long-lived windows.
        #
        # NOTE the window byte is 8 bits, so it wraps after 256 windows (~123 s
        # at 32 polls/window) and later windows overwrite earlier ones. That
        # costs the mean/min/max columns on a long capture. The cumulative
        # figures below come from `series`, which is every report in order and
        # is unaffected.
        by_tag.setdefault(tag, {})[p[6]] = v
        series.setdefault(tag, []).append((t, v))
    stats = {}
    for tag, wins in by_tag.items():
        vals = list(wins.values())
        ser = series[tag]
        (t0, v0), (t1, v1) = ser[0], ser[-1]
        wrapping = tag in PP_WRAPPING
        cumulative = tag in PP_CUMULATIVE

        # Every step down across the whole capture, not just first vs last: a
        # reset in the middle can leave the ends looking monotonic.
        drops = sum(1 for (_, a), (_, b) in zip(ser, ser[1:]) if b < a)
        # Saturation is a property of the saturating counters only; on a
        # wrapping one 65535 is just a value it passes through.
        pegged = cumulative and any(v == 0xFFFF for _, v in ser)

        reset = cumulative and drops > 0
        if cumulative:
            # Suppressed outright on a reset. There is no arithmetic that
            # recovers the exposure — the pre-reset history has no known
            # duration — so the honest answer is that this population is
            # unavailable, not a number computed from two unrelated ends.
            delta = None if reset else v1 - v0
        elif wrapping:
            # Summed over adjacent reports, not taken across the endpoints: a
            # counter that turns over a whole revolution between the first and
            # last report differences to nothing, though the reports in
            # between show every increment. 100 -> 65000 -> 100 is 65,536, not
            # 0. This assumes at most ONE wrap between two adjacent reports,
            # which is what `wrapped` below flags — past that the sum is a
            # floor, so nothing derived from it may be printed as exact.
            delta = sum((b - a) & 0xFFFF for (_, a), (_, b) in zip(ser, ser[1:]))
        else:
            # A per-window figure (a mean, a max, a version). Differencing the
            # ends of an oscillating series answers nothing.
            delta = None

        rate = delta / (t1 - t0) if delta is not None and t1 > t0 else None
        stats[tag] = {
            # mean/min/max come from the window-deduped values and carry the
            # rollover caveat above; `last` must not, so it is the final
            # CHRONOLOGICAL sample. The deduped dict keeps a window ID at its
            # first insertion point, so once the 8-bit ID rolls over its last
            # entry is whatever window 255 held — an old value that the record
            # would then pair with the run's final timestamp, leaving a JSONL
            # row that cannot reproduce its own delta.
            "n": len(vals), "mean": statistics.fmean(vals),
            "min": min(vals), "max": max(vals), "last": v1,
            "rate": rate,
            # Cumulative tags are read as a difference across the capture, not
            # as a last value: the counters are boot-level, so a unit that has
            # been up for an hour carries an hour of history into the run.
            "first": v0, "delta": delta,
            "first_t": t0, "last_t": t1,
            "kind": "cumulative" if cumulative else "wrapping" if wrapping else "window",
            # `reset` means this tag's figures are unavailable, full stop.
            # `pegged` means the on-device u16 hit its ceiling, so `delta` and
            # everything derived from it is a floor, not a measurement.
            # `wrapped` means a wrapping counter turned over at least once, so
            # `delta` is a floor for the same reason.
            "reset": reset, "pegged": pegged,
            "wrapped": wrapping and drops > 0, "drops": drops,
        }
    return {"stats": stats, "non_magic": non_magic, "total": len(samples)}


def pp_run_reset(st):
    """Tags that prove the unit's counters were reset during the capture."""
    return sorted(t for t in PP_CUMULATIVE if t in st and st[t]["reset"])


def pp_unavailable(st, tag):
    """Why tag `tag`'s difference cannot be quoted, or None if it can.

    Saturation does not make a figure unavailable — it makes it a floor, which
    is still worth printing as long as it is never printed as an exact count.

    A reset does, and it does so for the WHOLE RUN rather than for the counter
    that happened to show it. Every counter here is boot-level and they all
    reset together; which ones reveal it is an accident of how densely each tag
    was sampled. A reboot between two reports of a sparsely sampled tag leaves
    that tag monotonic, and a tag sitting at zero never decreases at all — so
    suppressing only the tag that dropped announces the reset and then prints a
    passing gate underneath it, which is the exact shape of the bug this
    function exists to prevent.
    """
    s = st.get(tag)
    if s is None:
        return "not published by this build"
    if s["delta"] is None and not s["reset"]:
        return "a per-window figure, not a cumulative one"
    if s["reset"]:
        return (f"counter reset mid-capture ({s['drops']} decrease(s)) —"
                " the two ends are different populations")
    others = [t for t in pp_run_reset(st) if t != tag]
    if others and s["kind"] in ("cumulative", "wrapping"):
        return ("a counter reset was detected on tag "
                + ", ".join(str(t) for t in others)
                + " — the unit rebooted mid-capture, so this counter spans the"
                  " same break even though it did not decrease")
    return None


def _pp_route_a_gate(st):
    """Print the capture-health readings from tags 7-13.

    Silent on a build that does not carry them, so this stays a no-op for the
    ordinary layout-lottery use the channel was built for.

    Every figure is a difference across the capture. The counters are
    boot-level, so a unit that has been powered for a while carries history
    into the run; the last value answers a different question than the gate
    asks.

    A reading is never printed once it is known to be unsound. On v297 the
    saturation warning was printed and then a derived "0 of 12,000" was printed
    underneath it, which reads as a pass; a reset was silently absorbed by the
    modulo subtraction and reported 45,636 polls of exposure with zero
    failures. Both are suppressed here, and suppression names the tag.
    """
    if 14 in st:
        arm = st[14]["last"]
        name = {0: "A (baseline)", 1: "B (candidate)"}.get(arm, f"unknown ({arm})")
        lo, hi = st[14]["min"], st[14]["max"]
        if lo != hi:
            print(f"    ⚠ A/B arm changed mid-capture ({lo} → {hi}) — the reports in this run")
            print("      come from two different binaries. Judge neither; re-capture.")
        else:
            print(f"    A/B arm: {name} — read this before any comparison; the two arms"
                  " carry the same version number by necessity")
    if 11 in st:
        v = st[11]["last"]
        print(f"    app version (bootloader settings): {v if v else 'not recorded'}"
              " — can lag a flash by one; a stale reading is unwritten settings,"
              " not the wrong build")
    if 7 not in st:
        return

    # A reset anywhere invalidates the run's arithmetic, so say so once, at the
    # top, before any figure is printed.
    reset = sorted(t for t in PP_CUMULATIVE if t in st and st[t]["reset"])
    if reset:
        print("    ⚠ COUNTER RESET during the capture (tag(s) "
              + ", ".join(f"{t} {PP_TAGS[t][0]}" for t in reset) + ") — the unit")
        print("      rebooted or was reflashed mid-run. EVERY cumulative figure in this")
        print("      run is reported unavailable below, not just the tag that decreased:")
        print("      they all reset together, and which ones show it is an accident of")
        print("      sampling. The pre-reset history has no known duration, so no exposure")
        print("      figure can be recovered from this capture.")
    if any(st[t]["pegged"] for t in PP_CUMULATIVE if t in st):
        print("    ⚠ a cumulative counter reached 65535 (the channel's ceiling) — every")
        print("      figure derived from it is a FLOOR, not a count, and no exact rate or")
        print("      exposure verdict can be read off this run. Power-cycle and re-run.")

    why = pp_unavailable(st, 7)
    if why:
        print(f"    polls in this capture: unavailable — {why}")
        print("    No gate verdict from this run.")
        return
    polls = st[7]["delta"]
    if st[7]["pegged"]:
        print(f"    polls in this capture: >= {polls} — the counter saturated, so this is a"
              " floor and cannot be judged against the gate's >= 10,000")
    else:
        print(f"    polls in this capture: {polls}"
              f" ({'meets' if polls >= 10000 else 'SHORT OF'} the gate's >= 10,000)")
    if not polls:
        return

    def pct(tag, label, denom, denom_exact):
        why = pp_unavailable(st, tag)
        if why:
            print(f"      {label:<20} unavailable — {why}")
            return
        n = st[tag]["delta"]
        # A wrapped counter is a floor for the same reason a saturated one is:
        # the sum holds only while no two adjacent reports straddle more than
        # one revolution. Checked HERE, with the figure — a floor warning
        # printed after the exact percentage it qualifies is read as a pass.
        if st[tag]["pegged"] or st[tag]["wrapped"] or not denom_exact:
            why = ("it turned over during this capture"
                   if st[tag]["wrapped"] else "the counter saturated")
            print(f"      {label:<20} >= {n} of {denom} — a floor ({why});"
                  " no rate from this run")
        else:
            print(f"      {label:<20} {n} of {denom} — {100.0 * n / denom:.2f} %")

    exact = not st[7]["pegged"] and not st[7]["wrapped"]
    for tag, label in ((8, "pooled no-trigger"), (9, "pooled incomplete"),
                       (10, "get_condition fails")):
        if tag in st:
            pct(tag, label, polls, exact)

    if 12 in st and 13 in st:
        why = pp_unavailable(st, 12) or pp_unavailable(st, 13)
        print("    THE GATE READING — outside the LCD frame shadow (poll >= 15 ms after the"
              " last frame, or before any frame):")
        if why:
            print(f"      unavailable — {why}")
        elif st[12]["delta"]:
            clear, nt_clear = st[12]["delta"], st[13]["delta"]
            if any(st[t]["pegged"] or st[t]["wrapped"] for t in (12, 13)):
                print(f"      no-trigger >= {nt_clear} of >= {clear} polls — floors, no rate")
            else:
                print(f"      no-trigger {nt_clear} of {clear} polls —"
                      f" {100.0 * nt_clear / clear:.3f} %")
                print(f"      ({'meets' if clear >= 10000 else 'SHORT OF'} the gate's >= 10,000;"
                      " run it on each of the owner's controllers)")
        else:
            print("      no polls outside the shadow — nothing to read")
        # Tag 8 counts no-trigger EVENTS, retries included; tag 13 counts POLLS
        # with at least one. They are not additive and their difference is not
        # a frame-shadow figure.
        print("      The pooled line above counts events (retries included) and this one"
              " counts polls, so the two do not subtract. The pooled figure also carries"
              " the v290/v292 frame effect, tracked separately and NOT attributed to"
              " route (a): on v296, 58-60 of 64 pooled no-triggers sat in the 0-2 ms"
              " post-frame bucket. Gate on this line, not on the pooled one.")
    elif 8 in st:
        print("      pooled only — this build carries no frame-shadow split (tags 12/13)."
              " Pooled, the no-trigger rate mostly measures how often a poll follows an LCD"
              " frame (~1 % on every route-(a) build measured) and does not gate route (a).")


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


GAUGE_PCT_UNDECODED = 0xFF
GAUGE_FLAG_CHARGING = 0x01
GAUGE_FLAG_FULL = 0x02
GAUGE_SEQ_SHIFT, GAUGE_SEQ_MASK = 2, 0x03
GAUGE_FLAG_CHARGING_UNREAD = 0x10
GAUGE_FLAG_FULL_UNREAD = 0x20
GAUGE_FLAG_GAUGE_UNREAD = 0x40


def decode_gauge(raw, pct, flags):
    """One gauge sample as the firmware meant it. `None` is *unknown*, never false or zero.

    The firmware reports each of its three I²C reads separately, because a read
    that failed is not a `false` and a byte nobody can decode is not 0 %. This
    has to keep that distinction or the capture manufactures the very reading
    the firmware refused to: an all-reads-failed sample once printed here as
    "255 % discharging".

    `gauge` is one of "ok", "undecodable" (the byte read; nobody knows what it
    means — `raw` is kept, it is what a characterization run is looking for) or
    "unread" (the I²C read failed; `raw` is meaningless and reported as None).
    """
    if flags & GAUGE_FLAG_GAUGE_UNREAD:
        gauge, raw, percent = "unread", None, None
    elif pct == GAUGE_PCT_UNDECODED:
        gauge, percent = "undecodable", None
    else:
        gauge, percent = "ok", pct
    return {
        "raw": raw,
        "percent": percent,
        "gauge": gauge,
        "charging": None if flags & GAUGE_FLAG_CHARGING_UNREAD else bool(flags & GAUGE_FLAG_CHARGING),
        "full": None if flags & GAUGE_FLAG_FULL_UNREAD else bool(flags & GAUGE_FLAG_FULL),
        "seq": (flags >> GAUGE_SEQ_SHIFT) & GAUGE_SEQ_MASK,
    }


def gauge_state(entry):
    """The charge state in words, saying "unknown" where the firmware did."""
    chg, full = entry["charging"], entry["full"]
    if chg:
        return "charging"
    if full:
        return "full"
    if chg is None and full is None:
        return "charge state unknown"
    if chg is None:
        return "charging unknown, not full"
    if full is None:
        return "not charging, full unknown"
    return "discharging"


def analyze_gauge(samples):
    """Decode IP5306 gauge samples smuggled in the right-stick bytes (4-7).

    Only meaningful against a `gauge-debug` firmware build. pulsarv1 has no SWD
    probe and the XIAO has no onboard debugger, so RTT can't see this board —
    and the gauge has to be characterized *on battery, untethered*, which is
    exactly what a wired channel would disturb. The Dreamcast has no right
    stick, so bytes 4-7 are otherwise a constant 0x8000/0x8000.

    Layout (LE u32): [raw 0x78, decoded %, flags, MAGIC] — the flag bits are in
    `decode_gauge`, and `src/lib.rs::publish_gauge_sample` is the other half of
    the contract.

    Returns one entry per *change* from the previous report, timestamped at
    first sighting. Not per distinct value: this used to keep a global `seen`
    set, so 25 % -> 0 % -> 25 % reported two entries and the return — the
    rebound a discharge run exists to catch — vanished. The firmware's sequence
    counter is part of what changes, so a repeated identical *measurement* is
    an entry while the same measurement carried by many HID reports is not. An
    older build without the counter still works; it just collapses repeats.
    """
    previous, timeline = None, []
    non_magic = 0
    for t, p in samples:
        if p[7] != GAUGE_MAGIC:
            non_magic += 1
            continue
        key = (p[4], p[5], p[6])
        if key != previous:
            previous = key
            timeline.append({"t": t, **decode_gauge(*key)})
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
    ap.add_argument("--unit", metavar="SERIAL",
                    help="serial of the physical unit under test, e.g. UNIT-01. Without it "
                         "a run cannot be attributed to a board, which is what QC needs. "
                         "With --history, shows only that unit's runs")
    ap.add_argument("--note", help="free-text label for this run, e.g. 'post ip5306 RMW fix'")
    ap.add_argument("--verbose", action="store_true", help="with --list, print host identity fields")
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
        return list_devices(args.verbose)

    if args.history is not None:
        return print_history(args.record_file, args.history, args.unit)

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

    pp = None
    if args.pollperiod:
        pp = analyze_pollperiod(samples)
        print("\n── Poll-loop period (bytes 4-7 — requires a poll-period-debug build) ──")
        if not pp["stats"]:
            print(f"  no 0xB0-0xBD tag in byte 7 across {pp['total']} report(s) —")
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
                # A difference across the capture, not the last value: the
                # counter is cumulative since boot, so a unit that has been up
                # a while carries history into the run.
                why = pp_unavailable(st, 5)
                if why:
                    print(f"    cadence overruns in this capture: unavailable — {why}")
                else:
                    floor = " (a floor — the counter saturated)" if st[5]["pegged"] else ""
                    print(f"    cadence overruns in this capture: {st[5]['delta']}{floor}"
                          f" (since-boot total {st[5]['last']})"
                          " — 0 on a pre-anchor build; on an anchored build, >0/s = this "
                          "layout's body exceeds the period budget, a bad roll caught "
                          "on-device")
            if 6 in st:
                # Same rule as everywhere else: a reboot zeroes the radio
                # counter with the rest of them, so its span is broken too.
                why = pp_unavailable(st, 6)
                if why:
                    print(f"    radio notifications: unavailable — {why}")
                elif st[6]["rate"] is not None:
                    floor = (" — a floor; the counter turned over"
                             f" {st[6]['drops']}× and the sum assumes at most one wrap"
                             " between adjacent reports" if st[6]["wrapped"] else "")
                    print(f"    radio notifications: ≈{st[6]['rate']:.0f}/s{floor} "
                          "(healthy ≈133/s = 2 edges × 66.6 conn events/s; low or bursty = "
                          "the quiet-window gate is starving at its input, upstream of "
                          "classification)")
            _pp_route_a_gate(st)
            print("    NOTE: the telemetry payload now advances once per fresh controller "
                  "sample and is replayed byte-for-byte in between, so wire dedup works on "
                  "this build and report arrivals track the poll loop again. Before that fix "
                  "(every build up to and including v297) the tag rotated per SEND: dedup "
                  "never fired, the ~125 Hz notify loop attempted more sends than there were "
                  "connection events, the surplus was rejected queue-full, and each reject "
                  "consumed a seq — so --seq read 29.7 % 'loss' at a perfectly healthy 66 Hz "
                  "(run #44) and Hz/IQR could not fail at all (v297 run #200). On a capture "
                  "from one of those builds, judge neither.")

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
            print(f"  {len(g['timeline'])} sample(s), one per change "
                  "(the firmware re-reads the gauge every 10-60 s):")
            print(f"    {'t (s)':>8}  {'seq':>3}  {'0x78':>5}  {'bits 7:4':>9}  {'decoded':>12}  state")
            for e in g["timeline"]:
                if e["gauge"] == "unread":
                    raw, nib, decoded = "  --", "----", "read failed"
                else:
                    raw, nib = f"0x{e['raw']:02X}", format(e["raw"] >> 4, "04b")
                    decoded = "undecodable" if e["gauge"] == "undecodable" else f"{e['percent']} %"
                print(f"    {e['t']:8.1f}  {e['seq']:>3}  {raw:>5}  {nib:>9}  {decoded:>12}  {gauge_state(e)}")
            print("  → Log these against a known cell voltage to rebuild the map. Bits 7:4 are")
            print("    believed to be the 4 gauge LEDs, active-LOW (0000 = all lit = 100 %).")

    if not args.no_record:
        git = _git_info()
        record = {
            "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
            "commit": git["commit"],
            "branch": git["branch"],
            "dirty": git["dirty"],
            "unit": args.unit,
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
        # The telemetry counters the capture-health readings rest on. Runs
        # #200/#201 recorded none of this, so the stitched result they produced
        # could not be reconstructed from the records afterwards — only from a
        # console scrollback that happened to be kept. First and last values
        # with their timestamps make the difference re-derivable; `reset` and
        # `pegged` travel with them so a later reader cannot re-make the
        # mistake of differencing across a reset or quoting a saturated floor
        # as a count. Snapshot-to-snapshot spans are date-to-date across runs,
        # never `date` plus `seconds`.
        if pp and pp["stats"]:
            record["poll_period"] = {
                str(tag): {
                    "name": PP_TAGS[tag][0],
                    "kind": t["kind"],
                    "first": t["first"], "last": t["last"],
                    "first_t": _r(t["first_t"], 3), "last_t": _r(t["last_t"], 3),
                    "delta": t["delta"],
                    "reset": t["reset"], "pegged": t["pegged"],
                    "wrapped": t["wrapped"], "drops": t["drops"],
                    "n": t["n"],
                }
                for tag, t in sorted(pp["stats"].items())
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
