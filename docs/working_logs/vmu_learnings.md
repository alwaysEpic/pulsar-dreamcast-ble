# VMU Support — Learnings & Session Notes

**Date:** 2026-03-20
**Status:** WIP — functional prototype, BLE coexistence issues unresolved

---

## What Works

### VMU LCD Write Protocol
- **Command:** `BLOCK_WRITE` (0x0C) to address `0x01` (SUB_PERIPHERAL_1)
- **Payload:** 50 words total:
  - Word 0: Function type `0x00000004` (FUNC_LCD)
  - Word 1: Location word `0x00000000` (partition=0, phase=0, block=0)
  - Words 2–49: 192 bytes of pixel data (48 words)
- **Response:** ACK (0x07), no payload
- **CRC:** XOR of all bytes (frame word + payload), appended as final byte

### Byte Ordering
- **Pixel data must be byte-swapped within each 32-bit word.** The VMU interprets each word as big-endian, so bytes go on the wire reversed: `[chunk[3], chunk[2], chunk[1], chunk[0]]` instead of `[chunk[0], chunk[1], chunk[2], chunk[3]]`.
- The frame word, function word, and location word are NOT swapped — only the pixel data.
- **Without the swap:** image appears garbled (pixels shifted by word boundaries).
- **With the swap:** image displays correctly.

### VMU Orientation
- The VMU mounts **upside-down** in the Dreamcast controller.
- A 180° rotation is required before sending: reverse the byte array + bit-reverse each byte.
- Rotation is applied AFTER compositing the battery overlay (so the overlay ends up in the correct position when viewed through the controller window).

### VMU Enumeration
- The VMU **will not accept BLOCK_WRITE until DEVICE_INFO (0x01) has been sent** to its address (0x01). Without enumeration, writes are silently ignored.
- Enumeration only needs to happen **once** after VMU insertion.
- Fire-and-forget enumeration works: `let _ = host.enumerate_vmu(&mut bus);` — ignoring the result, then immediately attempting the LCD write, succeeds.

### SoftDevice Radio Timeslot API
- **Long Maple Bus TX (~200 bytes, ~1.6ms) gets corrupted by SoftDevice BLE interrupts** during CPU bit-banging. Short TX (~5 bytes for controller polls) usually dodges interrupts.
- The **Radio Timeslot API** (`sd_radio_session_open`, `sd_radio_request`) grants guaranteed interrupt-free CPU time.
- Pre-computing the entire GPIO waveform into a buffer of `(set_mask, clr_mask)` pairs, then blasting it in the timeslot callback at priority 0, produces clean signals.
- **Timeslot duration:** 4ms covers the ~2.4ms waveform with margin.
- Raw bindings available in `nrf-softdevice-s140` crate (SVC calls 72–74).

### Waveform Encoding
- Each data bit requires **3 waveform entries** (set data pin, drop clock, restore data), not 2.
- A 50-word BLOCK_WRITE = 50 × 32 × 3 + start + end + CRC ≈ 4844 entries.
- `MAX_WAVEFORM_LEN` must be ≥ 5000 (initial estimate of 3600 was too small and caused buffer overrun → crash).
- The TX waveform buffer can **share memory with the RX sample buffer** (96KB) since TX and RX never overlap. Saves ~28KB of RAM.

### Compositing
- AND-then-OR masked blit works correctly for overlaying the battery icon.
- Battery icon: 12×7 pixels, positioned at top-right corner (col 36).
- 5 states: 4 bars (75–100%), 3 bars (50–74%), 2 bars (25–49%), 1 bar (10–24%), 0 bars (<10%).
- Compositing is done on the CPU in <1µs — negligible cost.

---

## What Doesn't Work Yet

### BLE Coexistence — RESOLVED
- **The timeslot API was the problem, not the long TX.** Direct bit-bang TX (~1.6ms) works fine alongside BLE — the SoftDevice interrupts either don't fire during that window or the VMU tolerates minor timing glitches.
- The Radio Timeslot API disrupted BLE regardless of priority (HIGH/NORMAL) or session lifecycle (persistent/per-write open-close). Even a single timeslot use left BLE in a fragile state where controller inputs caused crashes.
- **Solution:** Use the same direct bit-bang approach as controller polling. No timeslot needed. The timeslot version is preserved as `write_vmu_lcd_timeslot()` for reference.
- VMU reseat causes a brief BLE connection blip (controller resets Maple Bus), but it recovers automatically.

### VMU Presence Detection
- **Sender bit checking (`pkt.sender & 0x01`)** on GET_CONDITION responses should work per the protocol spec (confirmed by KallistiOS, dreamwave, MaplePad sources), but was unreliable in our testing.
- Possible causes: bulk sampling decoder bit errors, controller variant differences, or timing issues.
- **Needs RTT investigation** to see the actual `pkt.sender` values.
- Periodic enumeration probing disrupts the VMU (causes it to power-cycle and beep).

### Repeated Enumeration
- Sending DEVICE_INFO to address 0x01 when no VMU is present may corrupt the Maple Bus state, causing controller communication failures.
- Sending DEVICE_INFO repeatedly to a connected VMU causes it to power-cycle (beep every few minutes).
- **Enumeration should be done exactly once**, not repeatedly.

### Controller Recovery After VMU Write
- VMU bus traffic (enumerate + LCD write) causes the controller to stop responding to GET_CONDITION for an extended period.
- **The OEM controller needs ~50ms to recover from a VMU reseat**, but our recovery takes ~10s due to the exponential backoff in the re-detection loop (100→200→400→800→1000ms).
- **The backoff is necessary** — fixed 100ms retry (no backoff) prevents the controller from recovering entirely. The controller needs breathing room between DEVICE_INFO requests.
- Sending DEVICE_INFO to the controller (0x20) immediately after VMU write made things worse (sluggish, bad reconnect).
- **Theory:** VMU bus traffic confuses the controller's Maple Bus state. It stops responding to GET_CONDITION until it receives a well-timed DEVICE_INFO. The backoff accidentally provides this by spacing out requests.
- **Needs RTT investigation** to see fail_count, pkt.sender, and bus state during VMU reseat.

### Battery Updates — WORKING
- Battery percent is wired to VMU LCD writes via `vmu_battery_percent`.
- Bar count change (75% threshold crossed) triggers a re-write automatically.
- Confirmed working: display updated from 4 bars to 3 bars during testing.

---

## Key Findings

### Power & Timing Budget
- VMU LCD write fits within the 16ms poll budget (~3ms for TX + ACK read out of 13ms headroom).
- Battery updates happen every ~15 minutes (when bar count changes) — negligible bus/power cost.
- The hot path (every 16ms) adds only a u8 comparison when VMU is present — nanoseconds.

### RAM Usage
- Pulsar logo (48×32 1bpp): 192 bytes const
- Battery icon masks/outlines: ~50 bytes const
- Composited frame: 192 bytes on stack (only allocated when writing)
- TX waveform: shares the 96KB RX sample buffer (no additional allocation)
- Total new RAM: ~250 bytes const + 192 bytes stack (temporary)

### Protocol Details Learned
- Real Dreamcast sends DEVICE_INFO to one port per VBLANK (round-robin, 4-frame cycle).
- VMU DEVICE_INFO response: 28 words, functions = 0x0E (storage | LCD | timer).
- VMU GET_CONDITION response: 8 bytes, minimal (some games poll it).
- VMU ACK response: 0 payload words (just frame word + CRC = 5 bytes).
- OEM controller uses hardware pin (ID2) for VMU detection — should be reliable.

### Bus Hold / Pin-Hold Trick (from DreamPicoPort)
- **OEM hardware holds bus pins low between data bursts** for large packets (e.g. controller device info). This is within spec — the Dreamcast tolerates extended packets.
- **WinCE games fire commands within microseconds of receiving a reply** — leaves very little prep time for the device side.
- **DreamPicoPort solution:** Hold SDCKB low after the end-of-packet pattern, only release it when ready to receive the next command. The Dreamcast waits for the bus to go idle before sending.
- **Potential application to VMU writes (host side):** After sending a VMU LCD write, the controller forwards it down the LM Bus to the VMU. The controller may hold the Maple Bus pins during this forwarding. If we send GET_CONDITION before the controller releases the bus, it could fail. Adding a "wait for bus idle" (both pins HIGH) after the VMU write ACK read may prevent the controller-lost blip we see after VMU writes.
- **Not yet tested** — noted for the RTT diagnostic session.
- Source: DreamPicoPort (Tails), https://github.com/OrangeFox86/DreamPicoPort

---

## Files Created/Modified

### New Files
- `src/vmu.rs` — Pulsar logo bitmap, battery icon assets, compositing functions, 180° rotation
- `src/maple/timeslot_tx.rs` — SoftDevice Radio Timeslot-based TX with pre-computed waveforms

### Modified Files
- `src/lib.rs` — Added `pub mod vmu`
- `src/maple.rs` — Added `pub mod timeslot_tx`
- `src/maple/gpio_bus.rs` — Added `write_lcd()` for direct LCD streaming, made `SAMPLE_BUFFER` pub(crate), made timing constants pub
- `src/maple/host.rs` — Added `PollResult`, `enumerate_vmu()`, `write_vmu_lcd()`, `SUB_PERIPHERAL_1` addressing, `sender_addr` in `DeviceInfo`
- `src/main.rs` — VMU init/write logic in poll loop (multiple iterations)

---

## Next Steps

1. **RTT diagnostic session (DK board)** — Use the DK's built-in J-Link to log `pkt.sender` values, fail_count during VMU reseat, and bus state during recovery. This will answer whether sender bit detection works and why recovery takes ~10s.
2. **Controller recovery hardening** — Once we understand the bus state via RTT, optimize the re-detection loop for VMU-caused glitches without breaking real disconnect recovery.
3. **Hot-plug support** — Detect VMU insertion/removal during gameplay and re-write the screen.
4. **Dongle architecture** — Long-term: Dreamcast-side dongle intercepts VMU LCD writes over Maple Bus, forwards via BLE to Pulsar adapter.

---

## Reference Projects
- [dreamwave](https://github.com/cluoma/dreamwave) — Pico 2 W Maple Bus host, uses PIO for TX/RX
- [MapleSyrup](https://github.com/Soopahfly/MapleSyrup) — Pico 2 W peripheral emulator (controller + VMU)
- [pico2maple-fw](https://github.com/cluoma/pico2maple-fw) — Closed-source Pico 2 adapter
- [KallistiOS](https://github.com/KallistiOS/KallistiOS) — Official Dreamcast SDK, maple_irq.c for detection flow
- [MaplePad](https://github.com/mackieks/MaplePad) — RP2040 controller emulator
