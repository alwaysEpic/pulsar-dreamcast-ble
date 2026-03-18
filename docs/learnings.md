# Maple Bus Implementation Learnings

Key lessons from implementing Maple Bus communication on the nRF52840 at 2Mbps. These apply broadly to any high-speed GPIO protocol on a microcontroller.

---

## 1. Bulk Sampling Beats Real-Time Edge Detection

At 2Mbps, each bit lasts 500ns. Trying to detect edges and make decisions within that window is fragile — any function call, branch mispredict, or interrupt can miss data.

**Solution:** Capture raw GPIO samples into a large static buffer as fast as possible, then decode offline. This separates "is the signal there?" from "can we decode it?" and makes debugging much easier.

At 64MHz, the nRF52840 gets ~15 samples per bit period — plenty of resolution for post-processing.

## 2. Every Microsecond Counts in the Hot Path

A single `rprintln!()` call costs ~15-20µs via RTT — that's 30-40 bits lost. The controller responds within ~100µs of a request, so any debug logging between TX and RX will miss the entire response.

**Rule:** Zero logging in the TX→RX→decode path. Log before TX and after decode, never in between.

## 3. Function Call Overhead Kills Timing

Returning from a `wait_for_start()` function and then calling `bulk_sample()` introduced enough delay to miss the first clock edges. Even a handful of nanoseconds matters when the controller starts transmitting immediately after the start pattern.

**Solution:** Combine detection and sampling in a single function. The moment the start pattern is detected, begin sampling inline — no function returns, no setup overhead.

## 4. Static Buffers, Not Stack Allocation

Allocating a 96KB buffer on the stack (`[u32; 24576]`) takes measurable time. By the time the stack frame is set up, the controller's response is already in progress.

**Solution:** Use `static mut` buffers — pre-allocated at link time, zero runtime cost.

## 5. Phase Alignment Is Critical

Maple Bus alternates which line is clock and which is data:
- Phase 1: A falls → sample B
- Phase 2: B falls → sample A

If the first detected edge is B falling instead of A falling, every subsequent bit is shifted by one position. The result looks almost right but every byte is wrong.

**Solution:** After detecting the start pattern, skip any B edges until the first A fall. This guarantees correct phase alignment.

## 6. Release Builds Are Mandatory

Embassy's `Flex::set_high()` / `set_low()` are `#[inline]` wrappers around single OUTSET/OUTCLR register writes. In a release build, these inline to ~1 instruction each. In a debug build, `#[inline]` is ignored — each pin toggle becomes 4+ nested function calls, destroying TX timing. The controller won't recognize the malformed request.

```bash
# Correct — TX timing works:
cargo embed --release --no-default-features --features board-xiao

# Broken — controller won't respond:
cargo embed --no-default-features --features board-xiao
```

This was the root cause of the XIAO board not working — the DK happened to always be built with `--release`.

## 7. Check Wiring with Initial State

Before any communication, read both pins as inputs:
```
Expected idle: A=1, B=0
A=0, B=1 → Wires are swapped
A=0, B=0 → Controller not powered or not connected
A=1, B=1 → Pull-up problem or pin short
```

## 8. Pull-Ups Are Non-Negotiable

Both SDCKA and SDCKB need external pull-ups to 3.3V. 10kΩ works reliably and saves a bit of current over 4.7kΩ. Without external pull-ups, floating lines cause false edge detection and unreliable communication. Internal pull-ups (~13kΩ) are too weak and vary with temperature.

## 9. Ground the Sense Pin

Dreamcast controllers won't respond at all unless the GND/Sense pin (green wire) is connected to ground. This catches people every time.

## 10. Check for Pin Shorts on Perfboard

Flux residue or solder bridges (especially under castellated-pad boards like the XIAO) can short pins to power rails. Symptoms:
- `Initial state A=1 B=1` (B should be 0)
- Board resets when grounding a data wire (shorted directly to 3.3V, not through pull-up)

**Diagnostic:** Short each data wire to GND one at a time. Board stays alive + pin reads LOW = correct (current through pull-up). Board resets = pin shorted to power rail.

## 11. Power Routing Matters

The XIAO's 3.3V regulator can't supply enough current for the boost converter + controller (~200mA+). Feeding the Pololu VIN from the 3.3V rail causes brownouts. The battery must feed the boost converter directly.

## 12. Input Pipeline: Signal, Not Channel

The controller state flows from Maple Bus poll → `Signal` → BLE notify task. Embassy's `Signal` is last-writer-wins (only holds one value), which matches the industry standard for gamepad HID — every major controller (Xbox, PlayStation, Switch Pro, BlueRetro) sends state snapshots, not event queues.

A `Channel` (FIFO queue) was considered but rejected: analog stick jitter fills the queue every poll, and the BLE task would process stale queued states, adding 32-64ms of latency for stick movements. Channels solve a problem that doesn't exist for gamepads.

**Optimizations applied (in order of impact):**

1. **Only signal on change** — reduces the window where a button press can be overwritten by identical idle-state polls. Analog jitter no longer constantly overwrites the signal.
2. **Wake BLE task immediately on state change** — use `select` between timer and signal instead of fixed 8ms sleep, so the BLE task reads new state within ~1ms instead of up to 8ms.

**Future option if needed:** Button edge accumulation — track a bitmask of buttons pressed since the last BLE read, OR it into the signaled state. This catches press+release within one poll cycle, but adds complexity and only helps when the BLE task is delayed past 16ms (rare with optimizations #1 and #2). The Maple Bus poll at 60Hz (16ms) already means sub-16ms taps are missed at the hardware level, matching original Dreamcast behavior.

## 13. Voltage-Based Battery Percentage Is Good Enough

A 10-point LiPo discharge curve lookup table with linear interpolation is the industry standard for embedded gamepads. Xbox controllers only report 4 levels. Meshtastic uses an 11-point table nearly identical to ours.

**Key findings from discharge testing (500mAh LiPo, FNB58 meter):**

- Linear voltage mapping is misleading — battery sat at "57%" for over an hour during the 3.7-3.9V plateau
- The protection circuit cuts off at ~3300mV under load, not 3.0V — the battery never reaches the theoretical minimum
- Voltage recovery after sleep or load removal causes readings to temporarily increase, confusing users

**Solutions applied:**
- Lookup table calibrated to measured cutoff (0% = 3300mV)
- 8x hardware oversampling (SAADC) for noise reduction
- Monotonic decrease constraint — percentage never goes up unless charging is detected

**What we didn't do (and why):**
- Fuel gauge IC (MAX17048) — adds hardware complexity, only +/-5% vs our +/-10-15%, not worth it without a custom PCB
- Rolling average — the monotonic decrease constraint handles the main noise source (voltage bounce) without the complexity
- Reading during radio idle — marginal benefit, the 8x oversampling handles transients

## 14. Flash-Based Panic Logging

`panic-reset` silently resets with no diagnostic information. Writing the panic message to a dedicated flash page before reset costs almost nothing (a few microseconds of NVMC writes) and provides critical debugging data on the next boot.

**Implementation notes:**
- Must use raw NVMC register writes — the SoftDevice async flash API is not available in a panic handler
- Use a fixed flash page well away from SoftDevice and application data (0xFC000)
- Always erase the page after reading to prevent stale data and limit flash wear
- nRF52840 flash is rated for ~10,000 erase cycles per page — not a concern

---

## Quick Reference

| Problem | Symptom | Solution |
|---------|---------|----------|
| Debug prints in hot path | Garbage data, missed bits | Remove all logging between TX and RX |
| Stack allocation delay | First edge missed | Use static buffer |
| Function return delay | First edge is B not A | Combine wait + sample in one function |
| Wrong phase start | Bytes shifted by 1 bit | Skip B edges until first A fall |
| Debug build | Controller never responds | Always use `--release` |
| Wires swapped | Initial state A=0, B=1 | Check Red→SDCKA, White→SDCKB |
| No pull-ups | False edges, unreliable reads | 10kΩ external pull-ups to 3.3V |
| Sense pin floating | Zero response from controller | Connect GND/Sense to ground |
| Pin short on perfboard | A=1, B=1 or board resets | Test each data wire to GND individually |

---

## Expected Working Output

```
Initial bus state (as inputs): A=1 B=0
TX: DeviceInfoRequest
RX: Frame=0x0500201C cmd=0x05 len=28
RX: OK!
Controller detected!
  Functions: 0x00000001
```

- **A=1, B=0** = correct idle state
- **cmd=0x05** = Device Info Response
- **len=28** = full 28-word payload (standard controller)
- **Functions: 0x00000001** = controller function code
