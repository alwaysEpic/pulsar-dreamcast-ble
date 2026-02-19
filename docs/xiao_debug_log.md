# XIAO nRF52840 Debug Log

Running log specific to the XIAO board build. The XIAO has never successfully received Maple Bus responses — this log tracks the investigation.

---

## Session: 2026-02-17 (Initial XIAO Maple Bus Debugging)

### Setup
- XIAO nRF52840 (regular, not Sense) flashed via DK's J-Link (external SWD)
- S140 SoftDevice pre-flashed on XIAO
- Controller powered from DK 5V output
- 4.7kΩ pull-ups from data lines to XIAO 3V3 pin (direct)
- **No inline resistors** on data lines
- XIAO pins: SDCKA = P0.05 (D5), SDCKB = P0.04 (D4)

### Problem
XIAO build detects start patterns but captures far too few bits. Controller never detected. DK build works with the same shared protocol code.

### Test 1: Baseline (before any fixes)
```
TX: DeviceInfoRequest (pre-TX pins: A=false B=false)
TX: Post-TX pins: A=false B=false
RX: start pattern found (b_trans=5, wait=8)
RX DECODE: bits=13 a_falls=7 b_falls=6 gaps=0 first_edge=16
RX: response=false
```
Only 10-13 bits captured per attempt (expected 256+).

### Fix 1: Remove RTT logging from TX→RX hot path
**Root cause:** `rprintln!` calls in `host.rs:request_device_info()` between `write_packet()` and `read_packet_bulk()` added ~20ms of dead time. The controller responds within ~100µs, so the entire response was missed.

**Change:** Removed pre-TX and post-TX rprintln from `host.rs`. Added comment explaining why no logging is allowed in this gap.

### Test 2: After RTT fix
```
RX: start pattern found (b_trans=6, wait=14)
RX DECODE: bits=31 a_falls=16 b_falls=15 gaps=0 first_edge=5
RX: response=false
```
**Improvement:** 10-13 bits → 31 bits. Start detection now better (b_trans=6). But still far short of 256+.

31 bits ≈ 1/4 of first 4-word chunk, then signal disappears.

### Fix 2 (REVERTED): Disable interrupts around sampling
**Hypothesis:** SoftDevice radio ISRs preempting the sampling loop for 50-200µs.

**Change:** Added `cortex_m::interrupt::disable()` before sampling, re-enabled after.

**Result:** SoftDevice assertion failure → panic-reset → infinite reboot loop. SoftDevice cannot tolerate 2ms interrupt blackout.

**Critical:** Also disproved the hypothesis — bits=31 even WITH interrupts disabled. Interrupts are NOT the cause.

### Fix 3 (REVERTED): MIN_START_TRANSITIONS = 6
**Hypothesis:** False start pattern detection with threshold of 3.

**Result:** b_trans was already 6+ naturally. Changing threshold from 3 to 6 did not change bit count (still 31). Reverted to 3 to keep DK build unchanged.

### Raw sample diagnostic added
Added post-capture analysis (no hot-path impact) that dumps a few samples from the already-captured buffer when < 32 bits are decoded. Shows raw pin states at sample[0], sample[500], sample[5000], and sample[end], plus the pin masks being used.

**Awaiting flash to see output.**

### I2C Pin Investigation (P0.04 / P0.05) — RULED OUT

D4 (P0.04) and D5 (P0.05) are the default I2C SDA/SCL pins on the XIAO. Initially suspected as a cause, but:
- **Board is regular variant** (not Sense) — no IMU on these pins
- **Pull::None had no effect** — on-board pull-ups (if any) aren't the issue
- **Root cause was the voltage divider**, not the pins themselves

### Fix 4 (REVERTED): Internal pull-ups disabled (Pull::None)
**Hypothesis:** On-board I2C pull-ups on D4/D5 combined with internal pull-ups creating too-strong pull-up.

**Change:** XIAO build uses `Pull::None` instead of `Pull::Up`.

**Result:** No improvement. Still 20-34 bits. Reverted.

### Test 3: After Pull::None + raw sample diagnostic
```
RX: start pattern found (b_trans=3, wait=7)
RX DECODE: bits=30 a_falls=15 b_falls=15 gaps=0 first_edge=5
RX RAW: [0]=0x20 [500]=0x30 [5000]=0x30 [end]=0x30 masks=A:0x20 B:0x10
RX: first bits: [1, 0, 1, 0, 1, 0, 1, 0, 0, 1, 1, 0, 0, 1, 1, 0]
```
**Key observations:**
- `[500]=0x30` — both pins HIGH by sample 500 (~40µs). Signal dies very early.
- First bits are **random every run** — not reading real controller response
- b_trans=3-4, lower than DK's typical 3-7

### Ruled Out
- **I2C pin IMU interference** — Board confirmed as regular variant (Amazon ASIN B09T9VVQG7), no IMU on D4/D5
- **I2C on-board pull-ups** — Pull::None had no effect; even if present, not the cause
- **SoftDevice interrupts** — Disproved (bits unchanged with interrupts disabled)
- **MIN_START_TRANSITIONS threshold** — No effect (b_trans already passing)

### Leading Hypothesis: Missing 47Ω Inline Resistors

**Corrected understanding of both setups (2026-02-17):**

| | DK (WORKS) | XIAO (DOESN'T WORK) |
|--|------------|---------------------|
| Pull-up source | 3.3V via voltage divider from 5V (weak) | Direct 3V3 pin (strong) |
| Pull-up value | 4.7kΩ | 4.7kΩ |
| Inline resistors | **47Ω on each data line** | **None** |
| Data pins | P0.05 / P0.06 | P0.05 / P0.04 |

~~Previously suspected the voltage divider pull-ups on the XIAO were the problem — this was wrong.~~ The DK also uses a voltage divider for 3.3V and works fine. The XIAO pull-ups are actually direct to 3V3 (stronger source).

The key hardware difference is the **47Ω inline resistors** present on the DK but absent on the XIAO. These resistors:
- Dampen signal ringing/reflections after each edge transition
- Form a low-pass filter with line capacitance, cleaning up edges
- Limit current during transitions

Without them, at 2Mbps the data lines can ring after each edge. The sampling loop would see ringing as additional false edges, explaining the **random first bits** (reading ringing artifacts, not real data) and the signal appearing to "die" early (ringing subsides, no more detectable edges).

**Fix:** Add 47Ω inline resistors between controller data lines and XIAO D4/D5, matching the DK setup.

### DK Baseline Comparison (2026-02-17)
DK flashed with same code, **confirmed working** (controller detected, BLE functional):
```
RX: start pattern found (b_trans=5, wait=228)
RX DECODE: bits=139 a_falls=70 b_falls=69 gaps=0 first_edge=4
```
- 139 bits — first chunk, enough for frame decode and controller detection
- Deterministic — same result every run (vs XIAO's random bits)
- DK has 47Ω inline resistors + voltage divider 3.3V pull-ups

### XIAO Pin Constraints
All P0 pins on the XIAO header (required for our P0 register bulk sampling):
| Header | nRF Pin | Current Use |
|--------|---------|-------------|
| D0 | P0.02 | Wake button |
| D1 | P0.03 | Free |
| D2 | P0.28 | Boost SHDN |
| D3 | P0.29 | Sync button |
| D4 | P0.04 | **SDCKB** (I2C SDA) |
| D5 | P0.05 | **SDCKA** (I2C SCL) |

D6-D10 are all on P1 — cannot be used for Maple Bus bulk sampling.
If D4/D5 prove problematic even with proper pull-ups, options are limited (D1 is the only free P0 pin).

### Theories (Ranked by Likelihood)

**1. Missing 47Ω inline resistors (HIGH confidence)**
The only confirmed hardware difference between working DK and non-working XIAO. Without damping, signal ringing at 2Mbps causes false edge detection → random bits → decode failure.
**Test:** Add 47Ω inline resistors to XIAO data lines, matching DK.

**2. Pin differences P0.04 vs P0.06 (MEDIUM confidence)**
DK uses P0.06 for SDCKB, XIAO uses P0.04. P0.04 is the I2C SDA pin — may have different drive characteristics, internal routing, or parasitic capacitance vs P0.06 (generic GPIO). Could combine with missing inline resistors to make ringing worse.
**Test:** If inline resistors don't fully fix it, try P0.03 (D1) instead of P0.04 if wiring allows.

**3. Pull-up strength difference (LOW confidence)**
DK pull-ups: voltage divider 3.3V (higher impedance, weaker drive).
XIAO pull-ups: direct 3V3 (lower impedance, stronger drive).
Stronger pull-ups = faster rise times = more ringing energy. Counterintuitive but could contribute.
**Test:** Try weaker pull-ups on XIAO (10kΩ) or add series resistance to pull-up path.

**4. PCB routing / trace length (LOW confidence)**
XIAO is a tiny board with dense routing. Data line traces may have different impedance than DK's header pins. Unlikely to be the sole cause but could compound other issues.
**Test:** Can't easily change, but inline resistors would mitigate.

**5. Ground/power path differences (LOW confidence)**
XIAO powered from DK 5V + its own regulator. Different ground path length between controller and XIAO vs controller and DK. Could cause ground bounce at 2Mbps.
**Test:** Ensure short, direct ground connection between controller and XIAO.

### Complete Hardware Setup Reference

**DK Setup (WORKING):**
```
Dreamcast Controller          DK Board
─────────────────           ──────────
Pin 1 (Red/SDCKA)  ──47Ω──── P0.05
                        │
                      4.7kΩ
                        │
                      3.3V (voltage divider from 5V)

Pin 5 (White/SDCKB) ──47Ω──── P0.06
                        │
                      4.7kΩ
                        │
                      3.3V (voltage divider from 5V)

Pin 2 (Blue/+5V)   ─────────── DK 5V out
Pin 3 (Green/GND)  ─────────── DK GND
Pin 4 (Black/GND)  ─────────── DK GND
```

**XIAO Setup (NOT WORKING):**
```
Dreamcast Controller          XIAO Board
─────────────────           ──────────────
Pin 1 (Red/SDCKA)  ─────────── D5 (P0.05)
                        │
                      4.7kΩ
                        │
                      XIAO 3V3 pin

Pin 5 (White/SDCKB) ─────────── D4 (P0.04)
                        │
                      4.7kΩ
                        │
                      XIAO 3V3 pin

Pin 2 (Blue/+5V)   ─────────── DK 5V out (external)
Pin 3 (Green/GND)  ─────────── DK GND (external)
Pin 4 (Black/GND)  ─────────── DK GND (external)
```

**Key differences:** No 47Ω inline resistors. Different SDCKB pin (P0.04 vs P0.06). Pull-ups to direct 3V3 vs voltage divider.

### Test 4: MIN_RESPONSE_WAIT=100 (skip early noise)
Added minimum 100-cycle wait before accepting A LOW. Result:
```
RX: start pattern found (b_trans=4, wait=108)
RX DECODE: bits=14 a_falls=5 b_falls=9 gaps=0 first_edge=1
  e0-e11: gaps of 1-5 samples (erratic, expected ~14)
RX DIAG: edges=13 last_active_sample=39 (3 us)
```
- wait jumped from 0-24 to **106-108** — skipped the earliest noise
- But edges are 1-5 samples apart (expected ~14) and not alternating A/B — still noise
- wait=108 is right at our minimum, not at the expected ~228

### Test 5: MIN_RESPONSE_WAIT=200 (look for real response)
Pushed minimum to 200 to skip all noise and find real controller response at ~228 cycles.
```
RX: timeout waiting for A LOW (idle_in=true, idle_cycles=5, wait_cycles=64001)
```
**Every attempt timed out.** No A LOW detected after cycle 200.

**Conclusion: The controller is NOT responding to the XIAO's TX.** All previous "edges" were noise/ringing, not controller data. The issue is on the transmit side — without 47Ω inline resistors, the TX signal may be ringing or overshooting, and the controller doesn't recognize the request.

### Updated Theories (2026-02-17)

**1. ~~Missing 47Ω inline resistors affect TX~~ — RULED OUT**
Added 47Ω inline resistors matching DK setup. No change: still wait=106-108, bits=14-19, noise edges. The resistors are not the differentiator.

**2. Software MIN_RESPONSE_WAIT (PARTIAL — helps with RX noise but doesn't fix TX)**
MIN_RESPONSE_WAIT=100 successfully skips early ringing on receive, but can't fix the root cause: the controller doesn't see a valid request.

### Test 6: 47Ω Inline Resistors Added
Added 47Ω inline resistors on both data lines (Red→47Ω→D5, White→47Ω→D4), matching the DK wiring.
```
RX: start (b_trans=4 wait=108)
RX FAIL: bits=14 edges=14 active=3us first_edge=1
```
**No change.** Identical to pre-resistor results. Controller still not responding.

### XIAO Schematic Investigation (2026-02-17)

Reviewed the XIAO nRF52840 KiCad schematic. Key findings:
- **No series resistors** on GPIO header pins D0-D5. Direct chip-to-pad connections.
- **No on-board I2C pull-ups** on D4/D5.
- **P0.04 is electrically identical to P0.06** at the silicon level. Analog-capable pins (AIN0-7) only differ when SAADC is enabled (it's not in our code). Same pad capacitance (~4pF), same drive strength options, same slew rate.
- Pin choice is NOT the cause.

### Theory 3: OutputDrive::Standard Too Weak (NEW — testing)

DK and XIAO both used `OutputDrive::Standard` (S0S1, 1-4mA). But the XIAO's power rail or board routing may produce marginally weaker edges at Standard drive, especially at 2Mbps. The DK has better decoupling/power rail quality.

**Change:** Switched both data pins to `OutputDrive::HighDrive` (H0H1, 6+mA) in `gpio_bus.rs` — affects both `new()` and `set_output_mode()`. This is a global change (both boards), which is fine since HighDrive is strictly better for clean edges.

### Ranked Theories (updated)
1. **OutputDrive too weak (TESTING)** — Standard drive may be marginal on XIAO
2. **Pin differences P0.04 vs P0.06 — RULED OUT** (schematic confirms identical)
3. **Missing inline resistors — RULED OUT** (added, no change)
4. **Power supply / ground path** — XIAO 3.3V rail quality may differ from DK
5. **Board routing** — XIAO traces are short (~mm) but dense; unlikely sole cause

### XIAO Chip Lock / Recovery

When flashing the XIAO via DK's external SWD, a crash can leave the core in "locked up status" where `probe-rs erase` fails repeatedly.

**Recovery (in order):**

1. **`nrfjprog --recover`** — works when probe-rs cannot connect. Does a full chip erase including UICR.
2. Double-tap XIAO reset button for UF2 bootloader (needs USB connected to host), then retry erase.
3. Physical power cycle (disconnect battery), then erase immediately before firmware boots.

**After recovery, must reflash SoftDevice before app:**
```bash
nrfjprog --recover
nrfjprog --program vendor/s140_softdevice/s140_nrf52_7.3.0_softdevice.hex --verify
cargo embed --features board-xiao --no-default-features
```

### Test 7: HighDrive Output (H0H1, 6+mA)
Switched both data pins to `OutputDrive::HighDrive` for XIAO only.
```
RX: start (b_trans=4 wait=108-139)
RX FAIL: bits=2-7 edges=... active=3us
```
**No improvement.** Wait values shifted slightly (108→139) but still noise, not real response. HighDrive kept as XIAO-only default since it doesn't hurt.

### Test 8: Direct Ground Wire
Added short direct ground wire between controller ground and XIAO ground pad.
```
RX: start (b_trans=4 wait=106-108)
RX FAIL: bits=14 edges=14 active=3us
```
**No change.** Ground path is not the issue.

### Test 9: External Pull-ups Removed
Removed external 4.7kΩ pull-ups, relying only on internal ~13kΩ pull-ups.
```
Same results — no improvement.
```
**Pull-up strength is not the cause.**

### CRITICAL: Code Regression Identified and Fixed
During debugging, experimental changes were made to `wait_and_sample()`:
- Removed start pattern detection, replaced with immediate capture
- Added "slow scan" (NOP-padded sampling at 1/200th speed)
- Doubled buffer to 49,152 samples

**These changes broke BOTH boards.** DK went from 139 bits to 0 bits. The slow scan added ~2ms delay, causing bulk capture to start AFTER the controller response finished.

**Fix:** Reverted ALL experimental changes to original `wait_and_sample()` with start pattern detection.

### Test 10: DK Baseline After Code Restore
DK flashed with restored original code:
```
RX: start (b_trans=5 wait=294)
RX DECODE: bits=139 gaps=0 active=... range=...
RX: start (b_trans=6 wait=303)
RX DECODE: bits=139 gaps=0 active=... range=...
```
**DK confirmed working.** wait=294-303, consistent 139 bits every attempt.

### Test 11: XIAO With Restored Code
Same code flashed to XIAO:
```
RX: start (b_trans=4 wait=4)
RX FAIL: bits=23 edges=23 first_active=0 (0us) last_active=76 (6us)
RX: start (b_trans=3 wait=9)
RX DECODE: bits=33 gaps=0 active=120/24576 range=0..133
RX: start (b_trans=4 wait=20)
RX FAIL: bits=30 edges=30 first_active=0 (0us) last_active=99 (7us)
```
**XIAO still fails.** wait=4-20 (vs DK's 294-303). Activity only in first 76-133 samples (~6-10µs).

### KEY FINDING: wait Value Difference
| | DK | XIAO |
|--|-----|------|
| wait | 294-303 | 4-20 |
| bits | 139 (consistent) | 23-33 (random) |
| activity range | hundreds of samples | 76-133 samples |

The XIAO detects A LOW almost immediately after idle (wait=4-20), meaning it triggers on **TX ringing** — electrical echo from our own transmit. The DK waits ~294 cycles for the **real controller response**.

### Scope Confirmation: Controller DOES Respond
Scope probed at XIAO D5 pin (SDCKA) while XIAO was running:
- **Full Device Info Response visible** — 7 burst groups with inter-chunk gaps
- Signal structure matches DK captures exactly
- The controller IS responding to the XIAO's TX

**The response reaches the XIAO pin, but the GPIO IN register captures TX ringing first, filling the buffer with noise before the real response arrives.**

### PIN_CNF Verification
Read PIN_CNF register for both pins during input mode:
```
PIN_CNF = 0x0000000C  (Input, Connected, PullUp, Standard drive)
```
Both pins correctly configured. No anomaly.

### P0 Register Full Scan
Captured `changed` mask (XOR of first and last sample across entire P0):
```
changed = 0x00000030
```
Only bits 4 and 5 toggle — confirming correct pin mapping (P0.04 and P0.05).

### Ruled Out (Complete List)
1. **I2C pin interference** — Regular variant, no IMU
2. **On-board pull-ups** — None per schematic
3. **SoftDevice interrupts** — Disproved
4. **Start transition threshold** — No effect
5. **47Ω inline resistors** — Added, no change
6. **Pin differences (P0.04 vs P0.06)** — Schematic confirms identical
7. **Pull-up strength** — Removing external pull-ups had no effect
8. **Ground path** — Direct ground wire had no effect
9. **OutputDrive strength** — HighDrive had no effect on response detection

### Remaining Mystery
The scope shows the controller responds with a full packet at the XIAO pin. The GPIO register reads the pin correctly (PIN_CNF verified). But `wait_and_sample()` triggers on TX ringing (wait=4-20) instead of waiting for the real response (wait=~294).

**Why does the DK NOT see TX ringing?** Both boards use the same `write_packet()` → `set_input_mode()` → `wait_and_sample()` sequence. The DK cleanly transitions and waits ~294 cycles. The XIAO sees a false A LOW within 4-20 cycles.

**Possible causes still under investigation:**
1. **XIAO power rail settling** — After switching from output to input mode, the XIAO's 3.3V rail may glitch, causing a momentary LOW on pin A
2. **Pin capacitance / board routing** — XIAO's shorter, denser traces may hold output-mode charge longer, creating a ringing pulse
3. **Pull-up stabilization time** — `PULLUP_STABILIZE_NOPS=200` may not be enough on XIAO
4. **Need longer post-TX settling** — Add explicit delay after `set_input_mode()` before looking for A LOW

### Files Modified This Session
- `src/maple/host.rs` — Removed pre-TX and post-TX rprintln, removed unused import
- `src/maple/gpio_bus.rs` — HighDrive board-specific via `output_drive()` helper, condensed diagnostic output (RX FAIL / RX DECODE lines), all experimental changes reverted to original
- `src/main.rs` — Auto-enter sync mode when no bond
- `docs/dk_debug_log.md` — Updated with session entry
- `docs/xiao_debug_log.md` — This file (created and updated)

---

## Session: 2026-02-17 (Continued — GPIO Hardware Verification & Release Build Fix)

### ROOT CAUSE FOUND: Dev Build (No `--release`)

The XIAO was being flashed with `cargo embed --no-default-features --features board-xiao` (dev/debug build), while the DK was built with `cargo embed --release`. **This was the entire problem.**

### Why Dev Build Breaks Maple Bus

Embassy's `Flex::set_high()` / `set_low()` are thin wrappers around `OUTSET`/`OUTCLR` register writes, marked `#[inline]`. In a **release build**, they compile to single register writes. In a **dev build**:

- `#[inline]` is ignored — each pin toggle becomes 4+ nested function calls:
  `Flex::set_high() → SealedPin::set_high() → block() → outset() → write()`
- `write_bit()` has phase branching, making timing asymmetric
- `delay_half_bit()` NOP loops run ~300× slower due to unoptimized loop overhead
- Net effect: TX timing is completely wrong, controller doesn't recognize the request

### GPIO Hardware Verification (Pre-Discovery)

Before finding the build issue, verified GPIO hardware works perfectly:

1. **PIN_CNF registers confirmed correct:** `0x00000003` (DIR=1 output, INPUT=1 disconnect, PULL=0, DRIVE=0 S0S1)
2. **Direct register toggle test:** Wrote OUTSET/OUTCLR in a tight loop → clean 0V–3.3V square waves at ~250kHz (dev-mode speed), verified on scope with and without controller connected
3. **With controller at ~250kHz:** Full 0V–3.4V swing, some RC rounding but reaching rail-to-rail
4. **Conclusion:** GPIO hardware is fine; Embassy abstraction overhead in dev mode was the issue

### Test 12: Release Build on XIAO

```bash
cargo embed --release --no-default-features --features board-xiao
```

Results:
```
RX: start (b_trans=7 wait=178)
RX DECODE: bits=141 gaps=0 active=20049/24576 range=1..24575
RX: start (b_trans=5 wait=180)
RX DECODE: bits=141 gaps=0 active=20041/24576 range=0..24575
```

**Controller responding consistently!** b_trans=5-7, wait=175-182, bits=141 every attempt.

### New Issue: Buffer Too Small for Release Speed

In release mode, `read_p0_in()` runs much faster (~2-3 instructions/sample vs ~50+ in dev). The 24K sample buffer fills before the controller finishes responding:

- **141 bits captured** — only ~15% of the expected 936 bits (28-word Device Info Response)
- **gaps=0** — not seeing inter-chunk gaps because buffer exhausts during first chunk
- **active=20040/24576** (~82%) — most of buffer shows activity

Need to either add inter-sample delays or switch to on-the-fly decoding for release builds.

### Key Takeaway

**Always use `--release` for the XIAO build.** The Maple Bus protocol is timing-sensitive and relies on Embassy GPIO calls being inlined to single register writes. Dev builds destroy this timing.

---

## Session: 2026-02-18 (Perfboard Build — Pin Short & Power Routing)

### Setup
- XIAO nRF52840 soldered to perfboard
- Pololu U1V11F5 boost converter (battery → 5V for controller)
- 4.7kΩ pull-ups from data lines to 3.3V
- External SWD flashing via DK J-Link

### Problem: Controller Not Responding on Perfboard

Same XIAO and controller that worked on breadboard the night before. Symptoms:
- TX visible on scope at controller connector
- Pull-ups reading 3.2V on data lines
- Continuity tests passing
- Two different controllers tried — neither responds
- `BUS: Initial state A=1 B=1` — **B should be 0** (controller pulls SDCKB LOW at idle)

### Power Routing Discovery

XIAO 3.3V regulator cannot supply enough current for the Pololu boost converter + controller (~200mA+). Battery must feed Pololu VIN directly, not through the XIAO.

**Working power setup:**
```
Battery(+) ──── Pololu VIN (direct)
Battery(+) ──── XIAO BAT+ (through battery pads)
Battery(-) ──── XIAO BAT- (internally = GND)
Pololu GND ──── XIAO GND (common ground)
Controller GND ── XIAO GND (common ground)
Pololu VOUT ──── Controller 5V (blue wire)
XIAO D2 (P0.28) ── Pololu SHDN (enable)
```

### ROOT CAUSE: D4 (P0.04) Shorted to 3.3V

Added `diagnose_bus()` diagnostic — after each failed TX/RX attempt, samples 1000 reads and reports pin activity:
```
DIAG: A_low=0/1000 B_low=0/1000 trans=0 final A=1 B=1
```
Zero transitions, zero activity. Controller completely invisible.

**Key test:** Manually shorting red wire (SDCKA/D5) to GND → `A_low=1000/1000`, board stayed alive. Shorting white wire (SDCKB/D4) to GND → **board reset/disconnected.**

A 4.7kΩ pull-up to 3.3V should only draw ~0.7mA when grounded. Board resetting means D4 has a **direct short to a power rail** (bypassing the resistor), creating a hard short when grounded.

Further investigation: D2, D3, D4, and D6 all show continuity to 3.3V. These are on the same side of the XIAO. No visible solder bridge — may be underneath the castellated pads or within the perfboard.

### Fix: Moved SDCKB to D1 (P0.03)

Changed SDCKB from D4 (P0.04) to D1 (P0.03):
- `xiao.rs`: `PIN_B_BIT = 3` (was 4)
- `main.rs`: Pin assignment `p.P0_03` (was `p.P0_04`)
- Sync button pin reassigned to `p.P0_04` (non-functional, button physically removed)

Result:
```
BUS: Initial state A=1 B=1
MAPLE: Timeout
DIAG: A_low=0/1000 B_low=0/1000 trans=0 final A=1 B=1
MAPLE: Controller detected
```
**Controller detected on first retry!**

### Current Pin Mapping (Perfboard)
| Header | nRF Pin | Use |
|--------|---------|-----|
| D0 | P0.02 | Free (was wake button, no longer needed) |
| D1 | P0.03 | **SDCKB** (moved from D4) |
| D2 | P0.28 | Boost SHDN |
| D3 | P0.29 | **DEAD — shorted to 3.3V** |
| D4 | P0.04 | **DEAD — shorted to 3.3V** |
| D5 | P0.05 | SDCKA |
| D6 | P0.06 | Blue LED (shorted to 3.3V — LED may not work) |
| D7 | P1.12 | Sync button (also wake from System Off) |

### Robustness Improvements

1. **Controller re-detection**: After 30 consecutive poll failures, re-runs `request_device_info()` with backoff instead of polling forever with `get_condition()`.
2. **Sleep on controller loss**: If re-detection fails for 60s, enters System Off.
3. **Sleep on BLE reconnect timeout**: If bonded device doesn't reconnect within 60s, enters System Off.
4. **Sync button = wake button**: D7/P1.12 configured with GPIO SENSE LOW for System Off wake. No dedicated wake button needed — press sync to wake (full reboot).
5. **Post-disconnect state**: Returns to Reconnecting (if bonded) or Idle (if no bond).
6. **BLE without bond**: Auto-enters sync mode (discoverable).

### Lessons Learned
1. **Perfboard soldering can short adjacent pins** — especially with castellated pad boards like the XIAO. Shorts may be invisible (under the board).
2. **`diagnose_bus()` is invaluable** — zero transitions immediately pointed to a hardware issue, not software.
3. **Manual GND short test** differentiates between a pull-up path (safe to short, draws <1mA) and a direct power short (resets board).
4. **Battery must feed Pololu directly** — XIAO 3.3V regulator browns out under boost converter load.

---

## Session: 2026-02-19 (Inactivity Sleep + Battery Level Reporting)

### Changes

#### 1. Inactivity Sleep Timeout (10 minutes)

**Problem:** When the BLE host (Steam Deck) goes to sleep but keeps the connection alive, the adapter polls the controller and sends HID reports indefinitely, draining the battery overnight.

**Fix:** Track `last_activity` timestamp in the main polling loop. Reset it whenever `state_changed()` returns true or the controller is (re)detected. After 10 minutes with no input change, signal `SLEEP_REQUEST` to enter System Off.

- New constant: `INACTIVITY_TIMEOUT_MS = 600_000` (10 min), XIAO-only
- `last_activity` reset on: state change, initial detection, re-detection
- Check runs every poll cycle (16ms) in the main loop, before the poll sleep

#### 2. Battery Level via SAADC

**Problem:** Battery service hardcoded to 100% — never reflects actual charge state.

**Fix:** Read battery voltage via SAADC on P0.31 (AIN7) every 60 seconds.

**Hardware:** XIAO has a 1:2 voltage divider on P0.31, gated by P0.14 (HIGH = enable). This divides the LiPo voltage (3.0-4.2V) to the ADC-safe range (1.5-2.1V).

**Implementation:**
- `BatteryReader` struct in `src/board/xiao.rs` — holds SAADC peripheral + P0.14 enable pin
- 12-bit SAADC with internal 0.6V reference, 1/6 gain → 0-3.6V ADC range
- Battery voltage = ADC reading × 2 (divider)
- Linear mapping: 3.0V = 0%, 4.2V = 100%
- Enable pin driven HIGH for reading, LOW between reads to save power
- `BATTERY_LEVEL` signal carries percentage from main task to BLE task
- BLE task updates Battery Service characteristic + sends notification
- DK board: no battery circuit, stays at init value (100%)

**Files modified:**
- `src/main.rs` — Inactivity timeout, SAADC interrupt binding, battery reader init, periodic read in main loop, battery update in BLE connection handler
- `src/board/xiao.rs` — `BatteryReader` struct with `new()` and `read_percent()`

### Verification Plan
- Build: `cargo embed --release --no-default-features --features board-xiao`
- Check RTT log for `BAT: raw=... v=...mV ...%` lines
- Verify inactivity sleep triggers after 10 min idle (RTT: "Inactivity timeout")
- Verify wake via sync button after sleep
- Verify BLE battery level updates (non-100% value in host)
