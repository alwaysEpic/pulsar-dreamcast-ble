# Maple Bus Debug Log

Running log of tests, assumptions, and results for the Dreamcast controller adapter project.

---

## Current State

**Date:** 2026-02-17

**Status:** Both DK and XIAO working. XIAO root cause was building without `--release` — Embassy GPIO calls not inlined in debug builds, destroying TX timing.

**See also:** `docs/xiao_debug_log.md` for full XIAO investigation details.

---

## Session: 2026-02-17 (XIAO + DK Maple Bus Debugging)

### DK Baseline (CONFIRMED WORKING)

Flashed DK with current code (includes diagnostics + hot-path fix). Controller detected, BLE works.
```
RX: start pattern found (b_trans=5, wait=228)
RX DECODE: bits=139 a_falls=70 b_falls=69 gaps=0 first_edge=4
```
- **139 bits** — first chunk (frame word + ~3 payload words), enough for controller detection
- **Deterministic** — same result every run, consistent b_trans and timing
- **NOTE:** 139 bits is less than full 936-bit Device Info Response, but frame word decodes correctly and controller detection succeeds. Previous 937-bit captures included all 7 chunks; current code may stop after first successful frame decode.

### XIAO Investigation

**Problem:** XIAO build captures 20-34 bits. First bits are **random each run** — not a real controller response. Controller never detected.

**Confirmed fix:** Removed rprintln from TX→RX hot path in `host.rs`. Improved XIAO from 10-13 to 20-34 bits.

**Disproved hypotheses:**
- SoftDevice interrupts (REVERTED — crashed SoftDevice, bits unchanged)
- MIN_START_TRANSITIONS=6 (REVERTED — no effect, kept at 3)
- Internal pull-ups Pull::None (REVERTED — no effect)
- I2C pin IMU interference (board is regular variant, no IMU)

**Root cause found:** XIAO pull-ups wired through a **voltage divider** off 5V to create a 3.3V reference. A voltage divider is high-impedance — can't supply current at 2Mbps edge rates. Signal degrades after ~30 edges as divider sags under load. The XIAO has a `3V3` pin that should be used directly for pull-ups.

### Hardware Setup Clarification
- **DK**: 47Ω inline resistors on data lines + 4.7kΩ pull-ups to 3.3V (voltage divider from 5V)
- **XIAO**: No inline resistors + 4.7kΩ pull-ups to direct 3V3 pin
- **Key difference**: 47Ω inline resistors (present on DK, absent on XIAO)
- Earlier hypothesis that XIAO's voltage divider pull-ups were the problem was **incorrect** — the DK also uses a voltage divider and works fine

### Changes Kept
1. **Removed pre-TX and post-TX rprintln** (`src/maple/host.rs`) — verified improvement
2. **Added edge timing diagnostics** (`src/maple/gpio_bus.rs`) — post-capture only, no hot-path impact
3. **Auto-enter sync mode** (`src/main.rs`) — when no bond found
4. **MIN_START_TRANSITIONS = 3** — unchanged from working DK value (per learnings.md)

### Hardware Setup
- DK: 4.7kΩ pull-ups from data lines to DK 3.3V rail (direct)
- XIAO: 4.7kΩ pull-ups from data lines through voltage divider off 5V (weak!)
- Controller powered from DK 5V output

---

## Session: 2026-02-10 (XIAO Board Support + Power Management)

### What Changed
Added a board abstraction layer so the same firmware supports both the nRF52840-DK (development) and XIAO nRF52840 (production VMU shell) via Cargo feature flags.

### Architecture
- **Feature flags:** `board-dk` (default) and `board-xiao` in `Cargo.toml`
- **Board module:** `src/board/mod.rs` conditionally includes `dk.rs` or `xiao.rs`
- Each board exports: `init_pins()`, `StatusLeds`, `PIN_A_BIT`/`PIN_B_BIT` constants
- `gpio_bus.rs` pin masks now derived from `board::PIN_A_BIT`/`PIN_B_BIT`

### Pin Mappings

**DK** (unchanged):
| Function | Pin | Notes |
|----------|-----|-------|
| SDCKA | P0.05 | Maple bus line A |
| SDCKB | P0.06 | Maple bus line B |
| LED1 (sync) | P0.13 | Active LOW |
| LED2-4 (status) | P0.14-16 | Active LOW |
| Sync button | P0.25 | Active LOW, pull-up |

**XIAO**:
| Function | Pin | Header | Notes |
|----------|-----|--------|-------|
| SDCKA | P0.05 | D5 | Same as DK |
| SDCKB | P0.04 | D4 | Remapped (P0.06 = XIAO blue LED) |
| RGB LED R | P0.26 | internal | Active LOW |
| RGB LED G | P0.30 | internal | Active LOW |
| RGB LED B | P0.06 | internal | Used as sync LED |
| Sync button | P0.29 | D3 | VMU MODE button |
| Wake button | P0.02 | D0 | VMU SLEEP button, GPIO SENSE wake |
| Boost SHDN | P0.28 | D2 | Pololu enable (HIGH=on) |

### XIAO LED Status
- Searching: Red solid
- Controller found: Green solid
- Sync mode: Blue fast blink (via sync button task)
- TX activity: no-op (avoid flicker on single RGB LED)

### XIAO Power Management
- **Auto-sleep:** After BLE disconnect, reconnect timeout (60s) triggers System Off
- **System Off entry:** Disables 5V boost (SHDN LOW), configures P0.02 SENSE LOW, calls `sd_power_system_off()`
- **Wake:** Press VMU SLEEP button → GPIO SENSE triggers full reset → boots fresh, reconnects via bond
- **Expected draw:** ~5µA in System Off

### Files Created
- `src/board/mod.rs` — Board abstraction module root
- `src/board/dk.rs` — DK pin mappings, 4 LEDs, no sleep
- `src/board/xiao.rs` — XIAO pin mappings, RGB LED, boost control, sleep

### Files Modified
- `Cargo.toml` — Added `board-dk`/`board-xiao` features
- `src/lib.rs` — Added `pub mod board`
- `src/main.rs` — Uses `board::init_pins()` + `StatusLeds` methods, XIAO sleep signal
- `src/maple/gpio_bus.rs` — Pin masks use `board::PIN_A_BIT`/`PIN_B_BIT`

### Build Commands
```sh
cargo build                                              # DK (default)
cargo build --features board-xiao --no-default-features  # XIAO
```

### Verification
- `cargo build` — DK clean ✓
- `cargo build --features board-xiao --no-default-features` — XIAO clean ✓
- `cargo clippy -- -W clippy::all -W clippy::pedantic` — 0 warnings (both) ✓
- `cargo fmt --check` — clean ✓

### What's Next
1. Flash DK — verify existing functionality unchanged
2. When XIAO arrives: flash, verify Maple bus, BLE, sleep/wake, boost control
3. Battery ADC reading (P0.31 via P0.14 enable) — future

---

## Session: 2026-02-09 (Xbox One S BLE Format Alignment)

### Problem
Hardware tester showed "insane" analog values — stick values going in and out "like a flower." Device didn't work with iBlueControlMod (Dreamcast Bluetooth adapter from Handheld Legend). Previous Sonnet session made uncommitted changes (Xbox rename, event-driven reporting) that were reverted.

### Root Cause Analysis
Researched the real Xbox One S BLE HID descriptor (Model 1708, PID 0x02E0) and compared to our implementation. Found **5 critical mismatches**:

| Issue | Our Code (Before) | Real Xbox One S |
|-------|-------------------|-----------------|
| Stick range | **signed** -32768..32767 | **unsigned** 0..65535, center=32768 |
| Field order | buttons, hat, sticks, triggers | **sticks, triggers, hat, buttons** |
| Right stick usages | Rx (0x33) / Ry (0x34) | **Z (0x32) / Rz (0x35)** |
| Trigger usages | Generic Desktop Z/Rz | **Sim Controls Brake (0xC5) / Accel (0xC4)** |
| Trigger packing | 16-bit fields | **10-bit + 6 padding bits** |
| Report size | 15 bytes | **16 bytes** |
| Buttons | 16 | **15 + AC Back separate** |

The "flower" behavior was caused by the signed/unsigned stick mismatch — our center value of 0 was interpreted as far-left (0 in unsigned range) instead of center (32768).

The field order mismatch meant the receiver was parsing our button bytes as stick position data.

### Research: iBlueControlMod
- Product from Handheld Legend, ESP32-based
- Explicitly **NOT** standard BlueRetro — "Upgrading to BlueRetro firmware will damage this product"
- Different GPIO pinout from stock BlueRetro
- Identifies controllers by name ("Xbox Wireless Controller") then HID descriptor fingerprinting
- Supports Xbox One S, Xbox Series X|S, PS3/4/5, Switch Pro, etc.

### Research: Xbox One S BLE Timing
- Internal firmware runs at ~100Hz
- BLE connection interval: 7-9 units (8.75-11.25ms)
- Slave latency: 0 (never skip events)
- Xbox controller has a bug where it doesn't advertise PPCP — hosts fall back to slow 30-50ms intervals
- We set PPCP explicitly via `sd_ble_gap_conn_param_update()`
- Fixed-rate continuous reporting, not event-driven

### Changes Made

#### `src/ble/hid.rs` — Complete HID rewrite
- **Descriptor**: Full 334-byte Xbox One S descriptor with 4 Report IDs:
  - ID 0x01: Main gamepad (16 bytes) — sticks, triggers, hat, buttons, AC Back
  - ID 0x02: Xbox/Guide button (1 byte, AC Home usage)
  - ID 0x03: Rumble output (9 bytes)
  - ID 0x04: Battery (1 byte)
- **Sticks**: Unsigned 0-65535 in Physical collections, X/Y and Z/Rz usages
- **Triggers**: Simulation Controls page, Brake/Accelerator, 10-bit with explicit 6-bit const padding
- **Buttons**: 15 buttons (Usage Min 1, Max 15) + 1-bit padding + AC Back (Consumer 0x0224) in own byte
- **Report struct**: `GamepadReport` fields now `u16` for sticks (not `i16`)
- **`to_bytes()`**: 16-byte output matching Xbox byte layout exactly
  - Right stick hardcoded to `0x00, 0x80` (32768 = center, LE)
  - Triggers masked to `& 0x03FF` before packing
- **Report map Vec**: Capacity 128 → 512 to fit larger descriptor
- **Device info**: Manufacturer "Microsoft", Model "Xbox Wireless Controller"
- Removed D-pad button constants (DPAD_UP/DOWN/LEFT/RIGHT) — D-pad is hat-only now

#### `src/maple/controller_state.rs` — Stick/trigger conversion
- **Sticks**: `u8 * 257` → unsigned `u16` (maps 0→0, 128→32896≈32768, 255→65535)
  - Deadzone outputs 32768 (center) instead of 0
- **Triggers**: `val * 1023 / 255` for exact 8-bit to 10-bit scaling
- Removed unused `to_ble_bytes()`

#### `src/ble/softdevice.rs` — Identity & connection
- Device name: "Xbox Wireless Controller" (was "Dreamcast Wireless Controller")
- Scan data: Updated to match new name (26 bytes, was 31)
- `event_length`: 6 (was 24) — allows shorter connection events for fast intervals
- `attr_tab_size`: 2048 (was 1024) — more room for the larger GATT table

#### `src/main.rs` — Connection parameters
- After connection + security: requests conn interval 7-9 (8.75-11.25ms), slave latency 0, supervision timeout 4000ms via `sd_ble_gap_conn_param_update()`
- Existing 8ms fixed-rate report loop unchanged (≈125Hz, matches Xbox)

### Expected Behavior After Flash
1. Device advertises as "Xbox Wireless Controller" with gamepad appearance
2. iBlueControlMod should recognize it by name and/or descriptor fingerprint
3. Sticks should report center=32768 at idle (not 0)
4. Triggers should report 0 at idle, 1023 fully pressed
5. Connection should negotiate ~10ms interval for ~100Hz report rate
6. All 16 bytes of each report should be parseable by the receiver

### Hardware Test Result (first flash)
- Triggers showed up as "R STICK AXIS 2 / AXIS 3" on tester, not as triggers
- Default position was -1.00000 (should be 0)
- Root cause: Raw Xbox descriptor uses Z/Rz for right stick and Brake/Accelerator (Simulation Controls page) for triggers. Most HID parsers map Z/Rz to triggers instead, so our right stick data appeared as triggers and actual trigger data was unrecognized.

### Fix Applied: xpadneo-patched Usage Convention
Changed the HID descriptor to use the standard convention that the xpadneo Linux driver patches Xbox descriptors to:
- **Right stick**: Z/Rz → **Rx (0x33) / Ry (0x34)** on Generic Desktop
- **Triggers**: Sim Controls Brake/Accelerator → **Z (0x32) / Rz (0x35)** on Generic Desktop

This is the universal gamepad convention (X/Y = left stick, Rx/Ry = right stick, Z/Rz = triggers).
Byte layout unchanged — only the HID usage tags in the descriptor changed.

### What To Watch For
- Does iBlueControlMod pair and connect?
- Are stick values sane on hardware tester? (should be ~32768 at center)
- Do buttons register correctly? (A=bit0, B=bit1, X=bit2, Y=bit3)
- Is D-pad working via hat switch?
- Are triggers smooth 0-1023, showing 0 at idle?
- Triggers should now appear as trigger axes, not right stick
- Any disconnections or instability?

---

## Previous State

**Date:** 2026-02-06

**Status:** Full response now captured (937 bits, 7 inter-chunk gaps). Phase alignment remains the key issue - decoded bytes are wrong due to starting with B edge instead of A edge.

---

## Session: 2026-02-06 (Buffer Expansion & Phase Debugging)

### Major Changes Made

1. **Buffer size increased: 4096 → 24576 samples**
   - Now captures full 28-word response + all inter-chunk gaps
   - Uses static buffer (`static mut SAMPLE_BUFFER`) to avoid stack allocation delay
   - Previous stack allocation of 96KB caused us to miss the response entirely

2. **Bits vector increased: 512 → 1024 capacity**
   - Full response is 936 bits, now have headroom

3. **Sampling changed: at-edge instead of after-edge**
   - Was: `samples[i+1]` (one sample after edge)
   - Now: `samples[i]` (at the edge)
   - `find_first_edges` was showing different values than `decode_bulk_samples`

4. **b_trans threshold lowered: 5 → 3**
   - Was getting inconsistent start pattern detection (sometimes 3, sometimes 5+)

### Test Results

**Test 26: Full Response Capture (14:14:30)**
```
RX BULK: wait_cycles=280 b_trans=5
RX BULK: Captured 24576 samples
RX BULK: Initial state A=0 B=0  ← Both LOW = mid-transmission!
RX BULK: First 20 samples:
  AB: 0000010101010101010101010000000000101010
RX BULK: First edge at sample 12
RX BULK: Edge gaps (first 20):
  e0: gap=0 B data=0   ← First edge is B, not A!
  e1: gap=16 A data=0
  e2: gap=14 B data=1
  e3: gap=16 A data=1
  ... (alternating B-A-B-A pattern)
RX BULK: Decoded 937 bits (A falls=469, B falls=469, gaps=7)
RX BULK: First 40 bits: 0111000010000000000000000001010000000100
RX BULK: First 8 bytes: C2 00 00 50 10 00 00 0F
RX BULK: Frame = 0x500000C2
```

**Key Observations:**
- **937 bits captured** - Full response! (expected ~936)
- **7 gaps detected** - Matches expected inter-chunk boundaries
- **469 A falls, 469 B falls** - Symmetric edge detection
- **Initial state A=0 B=0** - We're starting mid-transmission

### Root Cause Analysis: Phase Alignment

**The Problem:**
After `wait_for_start_silent()` returns, we expect:
- State: A=HIGH, B=LOW (ready for Phase 1)
- First data edge: A falls (Phase 1)

**What we're seeing:**
- State: A=LOW, B=LOW (both LOW!)
- First edge detected: B falls (e0)
- This means A has ALREADY fallen before we started sampling

**Why this happens:**
The controller starts sending data immediately after the start pattern. Between `wait_for_start_silent()` returning and `bulk_sample()` starting, the first clock edge (A falling) has already occurred.

**Bit Shift Effect:**
- When first edge is B (wrong): bits are `10001110...` → byte 0 = 0x39
- When first edge is A (correct): bits are `00011100...` → byte 0 = 0x1C
- The bits are shifted by 1 position!

### Sample Pattern Analysis

First 20 samples decoded:
```
Sample  0-1:  A=0, B=0  ← Mid-bit, both transitioning
Sample  2-11: A=0, B=1  ← B is HIGH (data value)
Sample 12:    A=0, B=0  ← B falls! (this is e0)
Sample 13-16: A=0, B=0  ← Both LOW
Sample 17+:   A=1, B=0  ← A rises!
```

This shows we captured DURING the first data bit, not at its start.

### THE FIX THAT WORKED

**Created `wait_and_sample()` function** that combines start pattern detection with bulk sampling:
- Samples IMMEDIATELY when A goes HIGH at end of start pattern
- No function return delay between detection and sampling
- This captures the first clock edge (A falling) correctly

**Result:**
```
RX BULK: Frame = 0x0500201C
- cmd = 0x05 ✓ (Device Info Response)
- sender = 0x20 ✓ (Controller in Port A)
- recipient = 0x00 ✓ (Host)
- length = 0x1C (28) ✓ (28 payload words)
```

**FRAME WORD DECODED CORRECTLY!**

### Key Changes That Fixed It

1. **Combined wait + sample function** - No return delay between detecting start pattern end and beginning sampling
2. **Sample AT edge** - `samples[i]` instead of `samples[i+1]`
3. **SKIP_BITS = 0** - Now properly aligned, no bit skipping needed
4. **b_trans threshold = 3** - More lenient start pattern detection

### Current Code State

**wait_and_sample():** Combines wait-for-start + bulk sampling in one function
**decode_bulk_samples():** Samples AT edge, skips B edges until first A fall seen
**SKIP_BITS:** 0 (no skip needed now that timing is correct)

---

## Previous State (2026-02-05)

**Key Finding:** Using `wait_for_start_silent()` with zero prints + bulk sampling captures clean edge data. Seeing ~14 samples per edge (~218ns), close to expected 250ns/phase. But decoded bits don't match expected frame content.

**Hardware:**
- nRF52840-DK
- Genuine Dreamcast controller
- Wiring: Red→P0.05 (SDCKA), White→P0.06 (SDCKB), Green→GND (sense), Blue→5V, Black→GND

**Latest Output (Test 24 - Bulk Sampling):**
```
RX BULK: wait_cycles=233 b_trans=8  ← Perfect! 8 B transitions = 4 toggle cycles
RX BULK: Captured 4096 samples
RX BULK: First 16 samples (A,B): [all A=1 B=0]  ← Correct idle state!
RX BULK: First 20 edges:
  [ 385] A fell, data=0
  [ 400] B fell, data=1
  [ 416] A fell, data=1
  [ 427] B fell, data=1
  [ 442] A fell, data=0
  ...
RX BULK: Decoded 196 bits (A falls=98, B falls=98)
RX BULK: First 8 bytes: 47 36 17 46 E6 F4 36 C6  ← Not matching expected
```

**Expected Frame:**
```
Frame = 0x05 00 20 1C (command=0x05, recipient=0x00, sender=0x20, length=0x1C)
Bytes transmitted (LSB first): 1C 20 00 05
First byte bits: 0 0 0 1 1 1 0 0 = 0x1C
```

**Got:**
```
First 8 bits: 0 1 1 1 0 1 0 0 = 0x74  ← Some bits match, some don't
```

**Bit Comparison (first byte):**
| Bit | Expected | Got | Match |
|-----|----------|-----|-------|
| 0 | 0 | 0 | ✓ |
| 1 | 0 | 1 | ✗ |
| 2 | 0 | 1 | ✗ |
| 3 | 1 | 1 | ✓ |
| 4 | 1 | 0 | ✗ |
| 5 | 1 | 1 | ✓ |
| 6 | 0 | 0 | ✓ |
| 7 | 0 | 0 | ✓ |

5 of 8 bits match. Pattern suggests possible timing skew or signal integrity issue.

---

## Next Session: Test Routes

### Route 1: Scope First (Recommended)
**Effort:** Low | **Risk:** Low | **Info Gain:** High

Use the digital scope to verify our assumptions before changing more code.

**Steps:**
1. Connect Ch1 to SDCKA (P0.05/Red), Ch2 to SDCKB (P0.06/White)
2. Trigger on falling edge of Ch1
3. Send Device Info Request, capture response
4. Measure:
   - Idle state (should be both HIGH ~3.3V)
   - Start pattern duration (~4µs)
   - Bit timing (~500ns per phase)
   - Response delay after our TX ends

**What we'll learn:**
- Are signals clean or noisy?
- Is timing as expected (~250ns/phase from controller)?
- Are we seeing proper voltage levels?

---

### Route 2: Bulk Sampling (Most Likely Fix)
**Effort:** Medium | **Risk:** Low | **Info Gain:** Medium

Implement the "sample everything, decode later" approach that worked for raphnet.

**Steps:**
1. Create 256-entry u32 array for samples
2. After start pattern, tight loop: `samples[i] = p0_in.read().bits()`
3. After sampling, scan for falling edges and extract bits
4. Compare decoded frame to expected

**Code sketch:**
```rust
let mut samples: [u32; 256] = [0; 256];
for i in 0..256 {
    samples[i] = p0_in.read().bits();
}
// Post-process: find edges, extract bits
```

**Why it might work:**
- Eliminates timing-critical decisions during receive
- raphnet had same issues, this fixed them
- At 64MHz, 256 samples = ~4µs coverage (enough for frame word)

---

### Route 3: Tune Current Approach
**Effort:** Low | **Risk:** Medium | **Info Gain:** Low

Small tweaks to existing edge detection code.

**Options to try:**
1. **Add 1-2 NOPs after edge detect** - let signal settle
2. **Invert Phase 2 samples** - maybe we have logic backwards
3. **Start with phase=false** after specific delay
4. **Increase timeout** significantly

**Quick tests:**
```rust
// Option 1: Small settling delay
if (val & PIN_A_MASK) == 0 {
    let _ = p0_in.read().bits(); // One extra read as delay
    bit = (p0_in.read().bits() & PIN_B_MASK) != 0;
}

// Option 2: Invert Phase 2
if !*phase {
    bit = !bit; // Invert the sampled value
}
```

---

### Route 4: PIO/Hardware Assist (Advanced)
**Effort:** High | **Risk:** Medium | **Info Gain:** High

nRF52840 doesn't have PIO like RP2040, but has other options:

**Options:**
1. **GPIOTE + PPI** - Hardware event detection on pin edges
2. **TIMER capture** - Timestamp edges for offline analysis
3. **SPI peripheral** - Abuse SPI to sample at fixed rate

**Why consider:**
- Software bit-banging at 2Mbps is at the edge of what's reliable
- Hardware assist could make timing deterministic

---

### Route 5: Different Controller
**Effort:** Low | **Risk:** Low | **Info Gain:** Medium

Try a different Dreamcast controller to rule out hardware issues.

**What to try:**
- Third-party controller (may have different timing)
- VMU directly (simpler device)
- Loopback test (connect our TX to our RX)

---

## Recommended Order

1. **Route 1 (Scope)** - 15 min to verify assumptions
2. **Route 2 (Bulk Sampling)** - If scope looks good, implement this
3. **Route 3 (Tune)** - Quick tweaks if bulk sampling is overkill
4. **Route 5 (Different Controller)** - If nothing else works

---

## Current Hypothesis

The 500-cycle delay after detecting A going low causes us to miss the first several data bits. The controller starts transmitting data immediately after its start pattern, but we're still waiting.

**Evidence:**
- Frame = 0xFFFFFBF3 (mostly 1s with a few 0s)
- "A went high after 0 cycles" means start pattern already ended during our delay
- At 2Mbps, 500 cycles (~8µs) = ~8 bits missed

---

## Tests Performed

### Test 1: Basic Hardware Verification
**Date:** 2026-02-05
**What:** Flashed blinky_test to verify nRF52840-DK works
**Result:** ✅ LEDs cycle 1-2-3-4 correctly
**Conclusion:** Hardware is functional

### Test 2: Memory Layout Fix
**Date:** 2026-02-05
**What:** Changed memory.x from SoftDevice offset (0x27000) to standalone (0x00000000)
**Result:** ✅ Main app now runs (was not running before)
**Conclusion:** SoftDevice not flashed, must use standalone memory layout

### Test 3: Initial Bus State Reading
**Date:** 2026-02-05
**What:** Read SDCKA/SDCKB as inputs before any communication
**Result:** A=0, B=1 at startup; A=1, B=1 after bus setup
**Conclusion:** Initial A=0 is unexpected (should be HIGH). May be floating/settling issue. After toggling pins, idle state is correct (both HIGH).

### Test 4: Controller Response Detection
**Date:** 2026-02-05
**What:** Send Device Info Request (0x01), monitor for response
**Result:** ✅ "A went low after 64 cycles" - controller IS responding
**Conclusion:** TX works, controller receives our request and responds

### Test 5: Fast Register Access in read_bit_timeout
**Date:** 2026-02-05
**What:** Changed read_bit_timeout to use direct P0->IN register instead of HAL
**Result:** ❌ Hard fault, chip locked up
**Conclusion:** Something wrong with the implementation - reverted

### Test 6: HAL-based reading with 500-cycle delay
**Date:** 2026-02-05
**What:** Using HAL methods for bit reading with 500-cycle delay in wait_for_start
**Result:** Frame = 0xFFFFFBF3 (garbage), timeout reading payload
**Conclusion:** Timing is off - we're missing data bits due to delay

### Test 7: Remove 500-cycle delay
**Date:** 2026-02-05
**What:** Removed delay, immediately wait for A to go HIGH after detecting A LOW
**Result:**
- A went low after 47 cycles (was 64)
- A high after 1 cycles (was 0)
- Frame = 0xFDFDDFFF (different garbage)
- Timeout at bit 2, payload word 18
**Conclusion:** Data changed but still garbage. "A high after 1 cycles" is suspicious - start pattern should keep A low for ~256 cycles (4µs). We may be catching noise or wrong edge.

### Test 8: Measure start pattern (count B transitions)
**Date:** 2026-02-05
**What:** Added B transition counter during start pattern wait
**Result:** A low for 2 cycles, 1 B transition (expected ~256 cycles, 8 transitions)
**Conclusion:** NOT seeing a proper start pattern. Way too short.

### Test 9: Fast register reads in read_bit_timeout + 50µs pre-delay
**Date:** 2026-02-05
**What:**
- Changed read_bit_timeout to use direct P0->IN register reads (PAC VolatileCell)
- Added 3200 cycle (~50µs) delay before looking for response
**Result:**
- **NO CRASH!** Fast register reads work!
- Initial A=1 B=0 (B is LOW - unusual)
- A low for 0 cycles, 0 B transitions
- Frame = 0xDBD83F00, len=0
- Got to CRC check (further than before!)
- CRC mismatch
**Conclusion:** Fast reads work. 50µs delay might be too long - catching middle/end of response. Try shorter delay.

### Test 10: Fast reads with 10µs pre-delay
**Date:** 2026-02-05
**What:** Reduced pre-delay from 50µs to 10µs (640 cycles)
**Result:**
- A went low after 2 cycles
- A low for 1 cycles, 1 B transitions (still too short)
- Frame = 0x00008060
- cmd=0x00, len=96, from=0x80, to=0x00
- Timeout reading payload word 21
**Conclusion:** Frame data changing, getting some valid-looking bytes (0x00, 0x80). Still not seeing proper start pattern though.

### Test 11: Improved start pattern detection with true idle wait
**Date:** 2026-02-05
**What:**
- Wait for true idle (A=1, B=1) before looking for start
- Use direct register reads throughout
- Add warning if <4 B transitions seen
**Result:**
- Initial A=1, B=1 (true idle confirmed)
- A went low after 50 cycles
- Pattern: 3 cycles, 2 B transitions (WARNING)
- Frame = 0x00000000 (all zeros!)
- CRC mismatch
**Conclusion:** Polling loop iterations ≈ 50-100 CPU cycles each. 3 iterations is ~150-300 cycles, close to expected 256 for start pattern. We're detecting it, but reading all zeros suggests timing issue in data read.

### Test 12: With pre-read state debug + 16 cycle delay
**Date:** 2026-02-05
**What:** Added debug showing A/B state right before reading, with 16 cycle delay
**Result:**
- A went low after 52 cycles
- **Ready A=0 B=1** ← A already LOW when we start reading!
- Frame = 0x00000011 ← Got non-zero bits!
- len=17 (vs expected ~28)
- CRC mismatch
**Conclusion:** Progress! The 16 cycle delay caused us to miss the first clock edge - A was already LOW when we checked. Frame has some correct bits but still wrong. Need to remove delay entirely.

### Test 13: Stale probe-rs process discovery
**Date:** 2026-02-05
**What:** Discovered J-Link issues caused by stale probe-rs process (PID 50500 from 10:42AM)
**Result:** ✅ Killing stale process fixed J-Link connection
**Conclusion:** Always check for orphaned probe-rs processes: `ps aux | grep probe-rs`

### Test 14: Remove rprintln between wait_for_start and reading
**Date:** 2026-02-05
**What:** Removed `rprintln!("RX: Got start pattern!")` which cost ~1000+ cycles
**Result:** Frame = 0x03000004, cmd=0x03, to=0x00 (correct!)
**Conclusion:** Debug prints in critical path cause missed bits. to=0x00 is now correct!

### Test 15: Add wait for B=LOW after A=HIGH (100 iteration loop)
**Date:** 2026-02-05
**What:** After detecting A=HIGH, loop waiting for B=LOW
**Result:** Frame = 0x00000081, got to payload word 22 before timeout
**Conclusion:** Extra loop changed timing, different data pattern

### Test 16: Try phase=false
**Date:** 2026-02-05
**What:** Changed initial phase from true to false
**Result:** Frame = 0x01000002, cmd=0x01 (our REQUEST command echoed back?)
**Conclusion:** phase=false gives worse results, stick with phase=true

### Test 17: Remove B=LOW wait, use phase=true
**Date:** 2026-02-05
**What:** Simplified sync - just break when A goes HIGH
**Result:** Frame = 0x03000004 (same as Test 14)
**Conclusion:** Simple sync works best, extra waits don't help

### Test 18: Phase-agnostic edge detection
**Date:** 2026-02-05
**What:** Implemented new read_bit_any_edge() that watches for ANY falling edge
**Result:** Frame = 0xFF30228B, from=0x22 (close to expected 0x20!), got to word 20
**Conclusion:** Different approach gives different (but still wrong) data. Interesting that from field is close.

### Test 19: Phase-based with bit capture
**Date:** 2026-02-05
**What:** Captured individual bits for all 4 bytes of frame word
**Result:**
```
Byte0 bits: 1 0 0 0 0 1 0 0 = 0x84 (expected 0x1C)
Byte1 bits: 0 0 0 0 0 0 0 0 = 0x00 (expected 0x20)
Byte2 bits: 0 0 0 0 0 0 0 0 = 0x00 (expected 0x00) ✓
Byte3 bits: 0 0 0 0 0 0 1 1 = 0x03 (expected 0x05)
```
**Conclusion:** Byte2 is correct (all zeros). Byte0 has leading 1 (possible start pattern tail). Bytes 1-2 are mostly zeros which is suspicious.

### Test 20: Re-read after edge detection
**Date:** 2026-02-05
**What:** Added second register read after detecting clock edge before sampling data
**Result:** Frame = 0xFFFEEFFF (mostly 1s!)
**Conclusion:** The signal is VERY short-lived. Re-reading samples pull-up values. Original single-read approach is correct.

### Test 22: Initial state capture after start pattern
**Date:** 2026-02-05
**What:** Added debug to capture pin state right after wait_for_start returns
**Key Finding:**
- With B=LOW wait loop (100 iterations): Initial state A=0, B=0 → **BAD** (missed first clock edge)
- Without B=LOW wait loop: Initial state A=1, B=0 → **GOOD** (correct state!)
**Conclusion:** The B=LOW wait loop was causing us to miss the first data bit. Removing it puts us in the correct initial state.

### Test 23: Small delay after A goes HIGH
**Date:** 2026-02-05
**What:** Tried adding small delays (compiler_fence, register reads) after detecting A HIGH
**Results:** Mixed - some configurations gave slightly better data (0x14 vs 0x04 for first byte)
**Current state:** Frame = 0x03000004 with cmd=0x03 (expected 0x05)

### Test 24: Bulk Sampling Implementation
**Date:** 2026-02-05
**What:** Implemented "sample everything, decode later" approach inspired by raphnet
- Created `wait_for_start_silent()` with zero prints in critical path
- Bulk sample 4096 GPIO reads into array
- Post-process to find edges and extract bits

**Results:**
```
b_trans=8  ← PERFECT - exactly 4 toggle cycles (8 transitions) in start pattern!
Captured 4096 samples
First 16 samples all A=1, B=0  ← Correct idle state after start pattern
First edge at sample 385 (~6µs after capture start - controller response delay)
Edge spacing: ~14 samples = ~218ns per edge (close to expected 250ns)
Decoded 196 bits (A falls=98, B falls=98)  ← Symmetric as expected
```

**Edge Analysis:**
```
[ 385] A fell, data=0  (first bit)
[ 400] B fell, data=1  (second bit)
[ 416] A fell, data=1
[ 427] B fell, data=1
...
```

**Decoded vs Expected:**
- First byte decoded: 0x74 (01110100)
- First byte expected: 0x1C (00011100)
- 5 of 8 bits match

**Bit Mismatch Analysis:**
- Phase 2 (B falling, sample A) seems to give too many 1s
- Expected 0 at bit 1, got 1
- Expected 0 at bit 2, got 1
- Suggests A line may be slow to transition LOW

**Conclusion:**
- Start pattern detection now PERFECT
- Bulk sampling captures valid alternating Phase1/Phase2 edges
- Bit values don't match expected - possible signal integrity or timing skew
- **Next step: Verify with scope** to see actual waveforms

### Test 25: Pre-edge Sampling (sample 2 before edge)
**Date:** 2026-02-05
**What:** Modified decode_bulk_samples to sample 2 positions BEFORE detected edge
**Results:** Same bit pattern as Test 24
**Conclusion:** Issue is not in sampling timing offset - need scope to see actual signals

---

### Test 21: (Pending) Skip bits experiment results
**Results from earlier:**
- Skip 0 bits: 0x03000084 (Byte0=0x84, Byte3=0x03)
- Skip 1 bit:  0x07000008 (Byte0=0x08, Byte3=0x07) - leading 1 gone!
- Skip 2 bits: 0x0F800010 (Byte0=0x10, Byte3=0x0F) - shifted too far

**Observation:** Skipping 1 bit removes the errant leading 1, suggesting we're catching the last bit of the start pattern.

---

## Key Observations from Recent Tests

### Timing Sensitivity
- Different delays/sync approaches give very different frame values
- Test 14 and 17 both gave 0x03000004 (most consistent)
- cmd=0x03 vs expected cmd=0x05 (differ in bits 0 and 1)
- to=0x00 is now correct in several tests

### Pattern Analysis
| Test | Frame | cmd | Expected |
|------|-------|-----|----------|
| 14 | 0x03000004 | 0x03 | 0x05 |
| 15 | 0x00000081 | 0x00 | 0x05 |
| 16 | 0x01000002 | 0x01 | 0x05 |
| 17 | 0x03000004 | 0x03 | 0x05 |
| 18 | 0xFF30228B | 0xFF | 0x05 |

### Best Results So Far
- Tests 14 and 17 gave to=0x00 (correct recipient)
- Phase=true with simple sync is most consistent
- Still off by 1-2 bits on command byte

---

## Research from Other Implementations

### From raphnet (Dreamcast USB Adapter)
Source: https://www.raphnet.net/programmation/dreamcast_usb/index_en.php

**Critical Timing Insights:**
- Data is only stable for **~240ns** (not 250ns as documented!)
- At 12 MHz (83ns/cycle), sampling was unreliable - needed **16 MHz** (62.5ns/cycle)
- Controller response times vary: **56µs (official Sega)** to **159µs (third-party)**

**Implementation Approach:**
- Used "memory hungry but fast" method: sample port **640 times** consecutively, then decode
- Assembly: `in r16, PINC; st z+, r16` repeated 640 times
- Process samples offline rather than decode in real-time

**Voltage Issues:**
- 3.3V with pull-ups had issues due to cable capacitance
- Ultimately used direct 5V MCU (out of spec but works)

### From ismell/maplebus (FPGA Implementation)
Source: https://github.com/ismell/maplebus

**Key Findings:**
- Must respond within **1 millisecond** of command
- USB latency (125µs minimum) made software-only solutions problematic
- Eventually moved to FPGA for hardware-level timing control

### From Gmanmodz (PIC32 Implementation)
Source: https://github.com/Gmanmodz/Dreamcast-Controller-Emulator

- Uses PIC32 at higher clock speeds
- C++ implementation with dedicated timing loops

---

## Proposed Test Plans

### Test Plan A: Bulk Sampling Approach
**Hypothesis:** Our polling loop might be too slow or inconsistent. Try the "sample everything, decode later" approach.

1. After start pattern, immediately sample the GPIO register 128+ times in tight loop
2. Store raw samples in array
3. Post-process to find falling edges and extract bits
4. Compare decoded data to expected values

**Pros:** Eliminates timing-dependent logic during critical window
**Cons:** Higher memory usage, more complex post-processing

### Test Plan B: Scope Verification
**Hypothesis:** We may have signal integrity issues or incorrect timing assumptions.

1. Connect scope to SDCKA and SDCKB
2. Send Device Info Request, capture response waveforms
3. Verify:
   - Idle state is both HIGH
   - Start pattern has 4 B toggles
   - Data bit timing (~250ns/phase for controller)
   - Actual response delay from end of TX to start of RX
4. Compare scope timing to our code assumptions

### Test Plan C: Inverted/Swapped Sampling
**Hypothesis:** We may be sampling the correct edge but with inverted logic.

1. Try inverting the sampled bit value in Phase 2 only
2. Try inverting in Phase 1 only
3. Try inverting in both phases
4. Compare results to expected

### Test Plan D: Relaxed Timing (Slower Polling)
**Hypothesis:** Our polling may be catching glitches or partial transitions.

1. Add small delay (1-2 register reads) between detecting edge and sampling
2. Extend timeout to ensure we don't miss slower controllers
3. Compare results

### Test Plan E: Alternative Edge Detection
**Hypothesis:** The "detect any falling edge, sample other pin" approach from protocol doc.

1. Implement truly phase-agnostic reader
2. Track previous state of BOTH pins
3. On ANY falling edge, sample the OTHER pin
4. Don't track phase explicitly

---

## Approaches NOT Yet Tried

1. ~~Remove the 500-cycle delay~~ ✓ Done
2. ~~Count B transitions~~ ✓ Done (saw 1-3 transitions)
3. ~~Add debug for first few bits~~ ✓ Done - captured bit arrays
4. ~~Try phase=false~~ ✓ Tried, worse results
5. ~~Fast register access~~ ✓ Working
6. **Bulk sampling approach** - sample 100+ times, decode later
7. **Scope verification** - verify actual waveforms
8. **Inverted sampling** - try inverting bit values
9. **Phase-agnostic detection** - any falling edge triggers sample

---

## Key Observations

### Controller Responds
- A goes low after 64 cycles (~1µs) = controller detected our request
- This confirms: TX encoding works, wiring is correct enough for TX

### Timing Mismatch
- Our delay: 500 cycles = ~8µs
- Start pattern duration: ~4µs (4 toggles at 2Mbps)
- First data bits sent: immediately after start pattern
- We're ~4µs late to start reading

### Data Pattern
- 0xFFFFFBF3 = mostly 1s (pullups)
- Some 0 bits present (F3 = 11110011, FB = 11111011)
- Suggests we ARE catching some edges, just out of sync

### Initial A=0 Mystery
- At very first read (before any setup): A=0, B=1
- After toggling pins and setting up bus: A=1, B=1 (correct)
- Possible causes: floating input, controller not powered yet, settling time

---

## Tooling / Debug Environment Notes

### J-Link Connection Issues

**Symptom:** "Probe not found" or "Out of sync, resynchronizing" errors even though J-Link is connected.

**Common causes:**
1. **Stale probe-rs process** - Previous flash/debug session didn't terminate cleanly
2. **JLinkGUIServerExe** - SEGGER's GUI server may be holding the connection
3. **nrfutil-device** - VS Code nRF Connect extension holds J-Link connection

**IMPORTANT: Always check before erasing or flashing:**
```bash
# Check for blocking processes (run this FIRST!)
ps aux | grep -iE 'jlink|probe-rs|openocd|nrf' | grep -v grep

# Common offenders:
# - nrfutil-device (from VS Code nRF Connect extension)
# - probe-rs (from previous session)
# - JLinkGUIServerExe

# Kill blocking process (replace PID)
kill <pid>

# Then proceed with erase/flash
probe-rs erase --chip nRF52840_xxAA --allow-erase-all
```

**Prevention:** Always ensure probe-rs commands complete or are properly terminated. If a flash hangs, Ctrl+C may not fully release the J-Link - check for orphaned processes.

### DK Chip Lock / Hard Fault Recovery

If the DK locks up (hard fault, infinite loop), the debugger may fail to connect. Fix:
```bash
probe-rs erase --chip nRF52840_xxAA --allow-erase-all
```

For XIAO recovery, see `docs/xiao_debug_log.md`.

---

## Hardware Notes

### nRF52840-DK Pin Mapping
- P0.05 → SDCKA (directly on P2 header)
- P0.06 → SDCKB (directly on P2 header)
- P0.11 → Button 1 (active low)
- P0.13-16 → LEDs 1-4 (active low)

### Dreamcast Controller Connector
- Pin 1 (Red): SDCKA
- Pin 2 (Blue): +5V
- Pin 3 (Green): GND/Sense - MUST be grounded for controller to respond
- Pin 4 (Black): GND
- Pin 5 (White): SDCKB

---

## Code State

### Files Modified
- `src/maple/gpio_bus.rs` - Main bit-banging implementation
- `src/maple/host.rs` - Host controller logic
- `src/main.rs` - Test harness
- `memory.x` - Changed to standalone mode

### Current wait_for_start Logic
```rust
1. Wait for A=HIGH (bus idle)
2. Wait for A=LOW (start pattern begins) - takes ~64 cycles
3. delay(500) ← PROBLEM: too long
4. Wait for A=HIGH (start pattern ends) - already HIGH (0 cycles)
5. delay_half_bit() x2 ← more delays
6. Return, start reading with phase=true
```

### Proposed Fix
```rust
1. Wait for A=HIGH (bus idle)
2. Wait for A=LOW (start pattern begins)
3. Wait for A=HIGH (start pattern ends) - NO artificial delay
4. Immediately return, ready to read
```

---

## Next Steps

1. Remove the 500-cycle delay
2. Add debug showing first few bit reads (pin states, timing)
3. Verify we catch the first clock edge (A going LOW for first data bit)

---

## Session: 2026-02-09 (Triple-Press Name Toggle)

### Feature
Added ability to toggle device name between "Xbox Wireless Controller" and "Dreamcast Wireless Controller" via triple-press of the sync button (Button 4).

### Why
The iBlueControlMod adapter identifies controllers by BLE name ("Xbox Wireless Controller"), so that must remain the default. However, for general BLE use (Mac, iPhone, PC), "Dreamcast Wireless Controller" is more descriptive. Users can triple-press to switch.

### Implementation
- **Flash storage** (`src/ble/flash_bond.rs`): New flash page at `0x000FD000` stores name preference with magic `0x4E414D45` ("NAME"). Default = Xbox (0x00).
- **SoftDevice name** (`src/ble/softdevice.rs`): `init_softdevice(is_dreamcast: bool)` picks the name at boot. Two static scan data arrays for advertising.
- **Triple-press** (`src/main.rs`): 3 presses within 2 seconds (without triggering the 3s hold-for-sync) toggles the preference. Signals `ble_task` to write flash, then `SCB::sys_reset()`.
- LED gives 5 rapid blinks to confirm the toggle before reset.

### Expected Behavior
1. Boot → loads preference → advertises with chosen name
2. Triple-press sync button → 5 rapid blinks → device resets → advertises with other name
3. Preference persists across power cycles
4. 3-second hold for sync mode still works unchanged

---

## 2026-02-09: Clean up all clippy pedantic warnings and dead code

### What Changed
Comprehensive cleanup to achieve zero warnings under `cargo clippy -- -W clippy::all -W clippy::pedantic`.

### Deleted
- **`src/board/mod.rs`** — entire module removed (unused `LedState`, `BoardConfig`, `Nrf52840Dk`, pin constants). Removed `mod board` from `main.rs` and `lib.rs`.
- **`src/maple/packet.rs`** — deleted unused `encode()`, `decode()`, `crc()`, `crc8_word()`, `Default` impl.
- **`src/maple/host.rs`** — deleted unused `commands::NO_RESPONSE`, `functions::{MEMORY_CARD, LCD, TIMER, VIBRATION}`, `MapleHost::with_timeout()`, `MapleResult::CrcError`.
- **`src/maple/gpio_bus.rs`** — deleted unused `MapleBusGpioOut` type alias.

### Fixed across all files
- Hex literal separators: `0x000FE000` → `0x000F_E000`, `0x00000001` → `0x0000_0001`, etc.
- Cast warnings: `as u8/u16/u32` → `u8::from()`, `u16::from()`, `u32::from_le_bytes()`, `.to_le_bytes()`
- `&x as *const T` → `&raw const x` / `.cast_mut()` for pointer casts
- `#[inline(always)]` → `#[inline]` on all gpio_bus functions
- `map().unwrap_or()` → `map_or()`, `.is_some_and()` in security.rs
- `to_*` methods on Copy types: `&self` → `self`
- Doc backtick warnings: bare identifiers in doc comments wrapped in backticks
- `let...else` and `if let` patterns replacing match-on-single-pattern
- Removed redundant `continue` and `else` blocks
- Added `#[must_use]` on all pure public functions
- Module-level `#[allow]` for unavoidable macro-generated warnings (nrf-softdevice gatt macros)

### Verification
- `cargo clippy -- -W clippy::all -W clippy::pedantic` — 0 warnings
- `cargo build` — success
- `cargo fmt` — clean

---

## 2026-02-09: Connection Hardening

Added reliability features to make the adapter suitable for real gaming sessions.

### Changes Implemented (5 of 6)

**1. Stale Controller Detection**
- Track consecutive `get_condition` failures in main loop
- After 30 failures (~500ms), send neutral/centered report to zero out all inputs
- Logs "Controller lost" and "Controller reconnected" transitions

**2. BLE Notification Error Handling**
- Track consecutive `send_report` failures (was silently discarded with `let _`)
- After 10 consecutive failures, break out of notify loop to trigger disconnect handling
- Prevents zombie connection state where we keep sending to a dead link

**3. Disconnect Reason Logging**
- `Connection` has no `disconnect_reason()` method — used `gatt_server::run` result instead
- Now logs which future completed (GATT error vs notify failure) with the error value
- Example: "BLE: Disconnected (GATT: Err(Disconnected))"

**4. Fast Reconnect Advertising**
- Added `ReconnectFast` mode: 20ms interval (same as SyncMode) but NOT discoverable
- Used for first 5 seconds after disconnect for snappy reconnection
- After 5s, falls back to `Reconnect` mode (100ms interval) to save power

**5. Connection Parameter Update Logging**
- `sd_ble_gap_conn_param_update` return code now checked and logged on failure
- Previously fire-and-forget with `let _`

### Controller Detection Retry Loop
- Previous code: single `request_device_info` attempt, then block forever in `wfi()` loop
- New code: retries with exponential backoff (100ms → 200ms → ... → 1s cap)
- LED4 on while searching, LED3 on when found
- Uses async `Timer::after` so executor keeps running (BLE task, sync button stay responsive)
- Removed the old blocking `wfi()` reset-button loop entirely

### WDT: Deferred (Not Implemented)

Attempted hardware WDT via raw register writes (base `0x4001_0000`). Removed after
repeated reset loops during debugging. Key learnings:

- **nRF52840 WDT cannot be stopped once started.** It survives system resets (only
  cleared by full power-on reset or pin reset).
- **CONFIG register is write-once.** Once TASKS_START fires, CONFIG/CRV/RREN are locked.
  If a previous firmware started the WDT with bad config (e.g., `HALT=Run`), reflashing
  new firmware with `HALT=Pause` has no effect — the old config persists.
- **`HALT=Run` + debugger = reset loop.** cargo-embed halts the CPU during flash/attach.
  With HALT=Run, the WDT keeps counting during halt and fires before firmware reaches
  the feed instruction. This creates an unrecoverable reset loop that requires a full
  power cycle (USB unplug/replug) to escape.
- **`HALT=Pause` doesn't help** if a previous boot already locked CONFIG with HALT=Run.
- **Conclusion:** WDT is a production-only feature. It must be the LAST thing added,
  and the firmware should feed the WDT as the absolute first instruction (before
  `rtt_init_print`, before Embassy init) to handle the case where a WDT from a previous
  boot is still running. Consider gating WDT behind a compile-time feature flag
  (`#[cfg(feature = "wdt")]`) so development builds never enable it.

### Files Modified
- `src/main.rs` — stale detection, notify errors, disconnect logging, conn param logging,
  ReconnectFast usage, controller detection retry loop
- `src/ble/softdevice.rs` — `ReconnectFast` advertising mode variant + match arm

### Verification
- `cargo build` — success
- `cargo clippy -- -W clippy::all -W clippy::pedantic` — 0 warnings
- Hardware tested: controller detection retry loop works
- Still needed: unplug controller mid-session, BLE disconnect/reconnect, sustained gameplay

---

## Session: 2026-02-19 (dc-protocol extraction, tests, docs)

### What Changed
Extracted pure protocol logic into a separate `dc-protocol/` library crate so it can be tested on the host without embedded toolchains.

### dc-protocol Crate
- New workspace member: `dc-protocol/` (no_std, depends only on `heapless`)
- Moved from main crate:
  - `controller_state.rs` — `ControllerState`, `ButtonState`, `from_payload`, `to_gamepad_report`
  - `hid.rs` — `GamepadReport`, `to_bytes`, `hat` module, `buttons` module
  - `packet.rs` — `MaplePacket`, `frame_word`
- `state_changed()` moved from free function in `main.rs` to method on `ControllerState`
- Main crate files now re-export from `dc_protocol::*`

### Tests (24 total, all passing)
- **controller_state** (17): button parsing, roundtrip, from_payload (valid/short/wrong func),
  stick deadzone, gamepad report (buttons/dpad/triggers/sticks), state_changed (buttons/triggers/sticks)
- **hid** (5): to_bytes default/buttons/triggers/hat/max values
- **packet** (2): frame_word basic/with payload

### Fixed
- `controller_state_parse` test had wrong assertions (raw byte values vs inverted values).
  The test was ported from the original crate where it had never actually run due to the
  `cortex-m` dependency preventing `cargo test` on host.

### Docs
- `README.md` — full rewrite: features, hardware, build/flash, testing, project structure
- `docs/users_guide.md` — non-technical guide: pairing, button mapping, sync button, battery, sleep, troubleshooting

### Verification
- `cd dc-protocol && cargo test` — 24 tests pass
- `cargo build --release --no-default-features --features board-xiao` — success
- `cargo build --release --no-default-features --features board-dk` — success
