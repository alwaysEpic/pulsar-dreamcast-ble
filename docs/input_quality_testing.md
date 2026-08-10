# Controller Input Quality & Latency Testing

Reference notes on measuring packet loss / input timing for the adapter, and the
current understanding of the fighting-game "misdirection" report (public issue #5).

> Status: **resolved and shipped.** This page is kept as the full investigation record,
> read top to bottom in chronological order — the early sections describe hypotheses that
> later sections disprove, so do not quote them in isolation. The root cause was the main
> loop completing only ~12–17×/sec, not the BLE link; the fix (decode early-termination
> plus `POLL_INTERVAL_MS` 16→8) landed in v0.2.6 and re-passed acceptance in v0.2.8 with
> the hardware-timed DMA TX. See the acceptance tables at the end for the final numbers.

## The problem (issue #5)

Reporter (Xcynz): rotating the joystick at speed in a Dreamcast fighting game
(Capcom vs SNK 2, training mode, KEY DISPLAY on) drops/misorders direction inputs
over BLE compared to a wired controller, so special-move motions fail. Slow
rotations look fine; only fast ones (~3 rotations/sec) misbehave.

## What we changed, and why it wasn't enough

- v0.2.1 fixed the **sticks** (signed 4-byte `Logical Maximum`).
- The poll-rate experiment matched the Maple poll to the BLE notify cadence:
  `POLL_INTERVAL_MS 16 → 8` (60 → 125 Hz). This removed the "every other BLE notify
  carries a stale Maple sample" aliasing. HIDAPI traces: min inter-arrival
  **30 ms → 13–15 ms**.
- **But the reporter tested it (blinded Build A) and saw "no improvement";** a second
  variant (Build B) felt worse ("serious delay"). So the internal poll fix removed one
  aliasing source but did **not** move the real ceiling.

## The real ceiling (analysis)

The internal Maple poll is no longer the bottleneck. Two downstream limits dominate:

1. **BLE connection interval (host-imposed).** The firmware *already requests*
   8.75–11.25 ms (`src/ble/task.rs:308` `min_conn_interval: 7, max_conn_interval: 9`,
   `slave_latency: 0`). macOS hands back **~13–15 ms (~66 Hz)** anyway — iOS/macOS
   floor BLE-HID at ~15 ms regardless of what the peripheral asks. **We cannot lower
   this from firmware alone; it is negotiated by the central.** A dedicated receiver
   (BlueRetro / iBlueControlMod feeding a real Dreamcast) may negotiate differently —
   measure it there, don't assume the Mac number.
2. **Dreamcast Maple poll (~60 Hz, once per frame)** on the BlueRetro side.

**Nyquist check:** 3 rotations/sec × 8 directions ≈ 24 transitions/sec; a clean
press+release per transition needs ~48 Hz of headroom. A ~66 Hz BLE link with jitter
sits right at that edge — which is exactly why fast rotations alias and slow ones
don't. Xcynz's framing is correct.

**Lever that could actually help:** the *host's* accepted connection interval, not the
internal poll. Worth measuring whether a real BlueRetro receiver grants something
tighter than macOS does. Stick filtering (One Euro, on `feat/stick-filter-experiment`)
trades latency for smoothness and is generally disliked by fighting-game players.

## What test tooling we have today

- `tests/blueretro/` — QEMU harness, but **button mapping only**, no timing/loss.
- `docs/test_plan.md` §7 — *theoretical* latency budget + a "film at 240 fps" idea.
- `gpadtester.com` referenced for manual HID validation.
- The poll-fix HIDAPI traces were **ad-hoc and never committed.** We have the
  technique, not a saved, reproducible test. (This doc + a planned capture script fix that.)

## Off-the-shelf tools worth using

| Tool | Measures | Notes |
|------|----------|-------|
| [gamepadla-plus](https://github.com/WyvernIXTL/gamepadla-plus) | polling rate, jitter, latency | CLI/GUI, **macOS via `uv`**; connect adapter to Mac as an Xbox HID gamepad |
| [cakama3a/Polling](https://github.com/cakama3a/Polling) | polling rate + latency | XInput analog |
| [finger563/esp-latency-test](https://github.com/finger563/esp-latency-test) | actuation→report (BLE) **and** actuation→screen (photodiode) latency | gold standard; needs ESP32 + 3 mm photodiode + resistor |
| [gpadtester.net/latency-test](https://gpadtester.net/latency-test/) | browser latency | zero-setup sanity check |

Background: [Punch Through — BLE connection interval](https://punchthrough.com/ble-connection-interval-throughput/),
[Nordic — BLE HID ~34 pps on default params](https://devzone.nordicsemi.com/f/nordic-q-a/71673/nrf52-ble-hid-packet-rate-problem),
[PulseGeek — BT vs USB latency](https://pulsegeek.com/articles/controller-codecs-and-bluetooth-vs-usb-latency/).

## Proposed test plan

1. **Baseline (no hardware):** run `gamepadla-plus` on the Mac with the adapter
   connected → record polling rate / jitter.
2. **`scripts/hid_capture.py`** (hidapi, `uv`-managed) — **built.** Connect the adapter
   to the Mac as an Xbox HID gamepad, then:
   ```
   uv run scripts/hid_capture.py --list          # confirm the device is seen
   uv run scripts/hid_capture.py --seconds 10     # rotate the stick continuously
   uv run scripts/hid_capture.py --input dpad     # or rotate the d-pad
   ```
   It reports inter-arrival min/median/p99/max, jitter (stdev + IQR), effective Hz, and a
   **rotation-completeness** check — bins the stick angle (or hat) into 8 octants and flags
   any jump of ≥2 octants as a *skipped direction* (the issue #5 symptom), the quantitative
   version of the in-game KEY DISPLAY test. Keep the input moving for the whole window
   (send-on-change means stillness produces no reports).
3. **Sequence counter (`seq-counter` feature) — built.** A debug build stamps a 7-bit
   counter into report byte 15 (HID padding bits 1-7, no phantom buttons) so loss is
   *measured*, not inferred:
   ```
   cargo build --release --no-default-features --features board-dk,rtt,seq-counter  # flash this
   uv run scripts/hid_capture.py --seconds 10 --seq                                 # rotate, read drops
   ```
   Contiguous counter ⇒ firmware under-sending (the measured rate is real); gaps ⇒ reports
   dropped in transit (BLE or macOS) ⇒ re-check on a Linux `hidraw` host to separate the two.
4. **Connection interval:** measure what the real BlueRetro/Dreamcast receiver actually
   negotiates vs the Mac's ~15 ms; that's the lever, not the internal poll.
5. **(Optional) end-to-end:** `esp-latency-test` rig for true press-to-screen latency.

## First measurement (2026-06-08, Mac host, ~1.3 rot/s left-stick rotation)

`hid_capture.py` over 10 s: **136 reports (~14/s overall)**, active-burst median interval
**45 ms (~22 Hz)** with **71 gaps of 60–180 ms** — 3–4× slower than the ~66 Hz the ~15 ms
conn interval should allow. Result: **61 of ~124 direction transitions skipped a direction
(~50%)**.

This reproduces issue #5 **on the BLE leg alone** (Mac host, no BlueRetro/Dreamcast): the
report stream collapses to ~14–22 Hz with 60–180 ms holes during continuous motion, and the
holes straddle octant boundaries → directions vanish. At ~3 rot/s (real fighting-game speed)
it would be far worse.

## Root cause — poll rate, not the link (2026-06-08)

The `seq-counter` build settled it: the counter in byte 15 arrived **perfectly contiguous over
116 reports — zero transit loss.** So the ~12–17 reports/sec is *real firmware output*, not the
BLE link or macOS dropping anything. And `STICK_CHANGE_THRESHOLD = 2/255` is far too sensitive
to be filtering, so the send rate equals the **actual Maple poll rate**: the main loop is only
completing ~12–17 times/sec (a ~30–80 ms iteration where ~8 ms was assumed).

This is the true root cause of issue #5, and it explains why the 60→125 Hz "fix" did nothing —
`POLL_INTERVAL_MS` is only the *delay between* polls, negligible against a 30–80 ms loop. Two
suspects, both in the main loop, both on the Maple bus:
1. **`get_condition` baseline** — even the fastest polls are ~29 ms apart vs the few ms a clean
   Maple read should take (suspect: the 24 KB bulk-capture + software decode, and/or the 3×
   retry firing under BLE radio interference).
2. **VMU LCD writes** — `host.write_vmu_lcd` bit-bangs the framebuffer over the same Maple bus
   when an animation frame is dirty, stalling the poll (fits the ~195 ms spikes).

**Next — attribute the split** with the `poll-timing` feature (DWT cycle counter, logged via RTT
outside the TX/RX window, per `learnings.md` §2):
```
cargo build --release --no-default-features --features board-dk,rtt,poll-timing  # flash DK
# RTT prints every ~60 polls:
#   POLLTIME us | period avg=.. min=.. max=.. | get_cond avg=.. max=.. | vmu avg=.. max=.. | n=..
```
`period − 8000 µs − get_cond − vmu` ≈ the rest of the loop. Whichever dominates is the fix target
(trim the capture/decode, or move the VMU write off the poll path).

## poll-timing measurement (2026-06-10, DK, vmu off, decode early-termination in)

Nine consistent 60-poll windows: `period avg≈34-37ms min≈31.5ms max 64-105ms`
(**~28 Hz**), `get_cond avg≈18-21ms max 48-56ms`, `vmu 0`.

- `period − get_cond ≈ 16.0ms` in every window — that residual is exactly
  `POLL_INTERVAL_MS = 16` (the formula above assumed 8; the 16→8 build was never
  merged). The loop is just the 16 ms timer plus get_cond.
- VMU gating works: the ~195 ms spikes are gone; remaining maxes look like
  get_cond retry multiples (48-56 ≈ 2-3× the ~15.5ms floor, so retries happen).
- get_cond keeps a **~15.5 ms floor** even with the truncated decode — ~15 ms
  unattributed vs the on-paper cost (capture ≈ 2 ms at the assumed ~12.5 MHz
  sample rate, truncated decode ≈ few ms). Either the capture or decode loops
  are slower than assumed, or retries are routine.

**Next — split get_cond itself.** The `poll-timing` build now also prints, per
bus transaction:
```
POLLPHASE us | tx avg=.. max=.. | read avg=.. min=.. max=.. | dec avg=.. min=.. max=.. | tries sum=.. max=.. n=..
```
`tx` = command write, `read` = wait+bulk capture (`wait_and_sample`), `dec` =
edge scan + decode, `tries` = transactions per `get_condition` (sum vs n is the
retry rate). Fix whichever dominates — capture truncation, decode trim, or the
retry cause — then re-measure with `hid_capture.py --seq`, and finally drop
`POLL_INTERVAL_MS` 16→8 (~8 ms + small get_cond ⇒ 80-100 Hz reachable).

## POLLPHASE attribution (2026-06-10) — decode cut never fired; fixed

Eight consistent windows: `tx ≈0.4ms`, `read ≈3.6ms`, `dec ≈10.4-11.3ms with
min pinned at ~10.10ms`, `tries sum ≈100 per 60 polls` (~1.7 transactions/poll,
max 3). The arithmetic closes: (0.4+3.6+10.4) × 1.7 ≈ 24.5ms = get_cond avg,
plus the 16ms timer = the ~40ms period. Three findings:

1. **The decode early-termination never fired** (`dec` min constant ⇒ full
   24,576-sample scan every transaction). It defined end-of-response as a run of
   A HIGH/B LOW — the host's *driven* idle — but during a read both lines float
   HIGH on the pull-ups, so the check never matched. Fixed: quiet is now
   detected as an **edge-free run** (state-independent), same
   CRC-fail-then-retry safety argument. Expected `dec` 10.1 → ~2ms.
2. **Sample rate is ~7 M samples/s**, not the assumed 12.5 MHz (`read` 3.5ms /
   24,576 samples). Inter-chunk gap ≈ 770-910 samples; threshold 3000 clears it 3×.
3. **Retries are routine** (~1.7×/poll): interrupts are *not* disabled during
   capture, so SoftDevice radio events punch gaps into the sample stream → CRC
   fail → retry (also the 94ms `dec` outlier). Masking interrupts for 3.5ms is
   not an option under a SoftDevice; the mitigation is a smaller capture window
   (future work).

Projected after this fix: get_cond ≈ 10ms → ~38Hz; then `POLL_INTERVAL_MS`
16→8 → ~55Hz; capture truncation + retry reduction → 70Hz+.

## Verified (2026-06-10): ~40Hz after the decode fix; POLL_INTERVAL_MS 16→8

Re-measurement matched the projection: `period ≈25.1ms` (**~40Hz**, was ~25-28Hz
at a ~40ms period), `get_cond ≈9.0-9.7ms`, `dec ≈3.7ms` (cut fires), `tries
≈1.2/poll` (down from 1.7 — shorter transactions overlap fewer radio events).
The residual was again exactly the 16ms timer, so `POLL_INTERVAL_MS` is now
**8**: ~17ms loop ≈ **60Hz**, matching the Dreamcast-side Maple rate.

Remaining levers, not yet taken: capture truncation (`read` is a fixed
24,576-sample loop, 3.3ms → ~1ms possible, but it touches the timing-critical
sampling loop), `END_IDLE_THRESHOLD` trim (~0.4ms), retry reduction (~17% of
polls take 2-3 transactions).

**Acceptance test for issue #5:** `hid_capture.py --seq` rotation-completeness —
direction skips should collapse vs the ~50% baseline; then merge and have the
reporter A/B on a real Dreamcast.

## ACCEPTANCE PASSED (2026-06-10, POLL_INTERVAL_MS=8 build, Mac host)

Two 10s captures, fast continuous rotation:

| run | reports | effective rate | transitions | speed | skipped directions |
|-----|---------|---------------|-------------|-------|--------------------|
| baseline (06-08) | 136 | ~14/s | ~124 | ~1.3 rot/s | **61 (~50%)** |
| 1 | 541 | 66.1 Hz | 259 | ~3.6 rot/s | **4 (1.5%)** |
| 2 | 526 | 65.8 Hz | 230 | ~3.3 rot/s | **0** |

Effective rate is now pinned at the Mac's ~15ms connection-interval cap (66 Hz)
instead of the firmware's old ~14/s — the internal poll is no longer the
limiter. Run 1's four skips are all single-octant and consistent with p95 ~30ms
delivery gaps (one coalesced conn event ≈ 0.9 octants at 3.6 rot/s): they live
at the Mac link layer, not in firmware. Subjective report matches: "the stick
looks way better."

Remaining: reporter A/B on a real Dreamcast through BlueRetro (that receiver
negotiates its own conn interval — measure, don't assume the Mac's 15ms).

## Status note (2026-06-11)

The asserts that drove the 06-10/11 VMU investigation were caused by the
diagnostic instrumentation itself (critical sections in poll-timing records
and in RTT log rendering) — see the 2026-06-11 bisect.
VMU writes were innocent; the animation is restored at 6fps on a PWM/EasyDMA
TX in v0.2.5.

⚠ Bench caveat: since ~11:00 on 06-11 the DK bench shows elevated
controller-read retries (`tries` ~110 vs ~70), dropping the effective poll
rate to ~45Hz; rotation captures from this period (26-35 skips) are NOT
comparable to the acceptance numbers above. **Root-caused as TWO stacked
faults, both compiled-code timing (06-11/06-12):**
1. The bulk sampling loop's branch target lost word alignment in builds
   from `2f1a502` onward — +1 cycle per sample, 12.6% slower sampling,
   retries 70→110. **Fixed**: the loop is now `.p2align 2` inline asm with
   pinned registers (fixed encoding in every build).
2. The bit-banged TX: compiling in the `vmu` feature alone re-optimized
   `write_packet` (225→161 instructions), shifting the command waveform's
   timing enough that the controller garbled ~2/3 of commands — retries
   ~170, late responses, dropouts, ~35Hz delivery. (An interim "bench
   electrical" theory was wrong; the A/B that produced it used vmu-feature
   builds on both legs.) **Fixed**: ALL outbound Maple frames now play via
   the hardware-timed PWM/EasyDMA engine (`pwm_tx::write_packet_dma`) —
   timing immune to codegen, layout, and interrupts.

## ACCEPTANCE RE-PASSED (2026-06-12, DMA-TX build, VMU pulsar animating at 6fps)

| run | reports | effective rate | transitions | speed | skipped directions |
|-----|---------|---------------|-------------|-------|--------------------|
| 1 | 532 | 65.4 Hz | 252 | ~3.5 rot/s | **3 (single-octant), 0 reversals** |

POLLPHASE: `tx avg≈250µs` (hardware wire, was ~390 bit-banged), `read
min=3124-3128`, `tries=66-78` with `vmu n=4-6` writes/window. Matches the
2026-06-10 acceptance — which ran without the VMU. First build where good
input quality and the VMU display coexist.
