# VMU Display & Save Goals

## Goals

1. **Real-time VMU animation** — Support something like the RE: Code Veronica
   ECG heartbeat. The game streams LCD frames to the VMU continuously showing
   a scrolling ECG line that changes based on health state (Fine/Caution/Danger).
   This requires frequent, reliable 192-byte LCD writes.

2. **Save transfer to dongle** — Button combo (sync button) to read VMU save
   data over Maple Bus and transfer it to the Pico2Maple dongle (SD card storage).
   Depends on Pico2Maple bridge being operational.

3. **Display ICONDATA_VMS default icon** — The VMU filesystem stores a file
   called `ICONDATA_VMS` with a 32x32 monochrome bitmap (128 bytes at a known
   offset). Read it via BLOCK_READ and display on the LCD as the default icon
   when no game is actively writing.

4. **Battery symbol always present** — Composite the battery overlay onto every
   frame regardless of source (pulsar logo, game icon, ICONDATA, animation).
   Already works for the pulsar logo, just needs to be applied universally.

## Key Numbers

| Item | Size |
|------|------|
| VMU LCD | 48x32 px, 1-bit mono = **192 bytes** |
| ICONDATA_VMS monochrome icon | 32x32 px, 1-bit = **128 bytes** |
| VMU save file (full) | Up to 128KB (200 blocks x 512 bytes) |
| LCD write TX duration | ~1.65ms for 192 bytes (50 words + framing) |
| BLE connection interval | 8.75-11.25ms |
| Maple poll interval | 16ms |
| Inter-chunk gap (peripheral TX) | 110-130µs (peripheral-side only, not host) |

## Current LCD Write Path

```
build_frame(battery_percent)
  → start with PULSAR_LOGO (192 bytes const)
  → composite_battery() overlay (12x7 px, top-right)
  → rotate_180() (VMU mounts upside-down)
  → write_lcd() via bit-bang GPIO (~1.65ms TX, continuous, no gaps)
  → read ACK (0x07) via bulk sample
```

Currently writes only when battery bar count changes, with 180-poll (~3s)
cooldown between attempts. The write is fire-and-forget with no retry on
failure — if SoftDevice interrupts corrupt the TX, the frame is just lost.

## The Core Problem: SoftDevice Interrupts During LCD TX

The LCD write is 50 Maple Bus words = ~1.65ms of continuous bit-bang TX.
SoftDevice BLE connection events occur every 8.75-11.25ms at interrupt
priority 0 (highest), and can preempt the bit-bang at any point. This
corrupts the Maple Bus waveform and the VMU ignores the malformed packet.

For occasional writes (battery icon), this is fine — retry in 3 seconds.
For real-time animation (RE heartbeat at ~10-30fps), this is a blocker:
too many frames will be lost to maintain smooth animation.

---

## Research Findings

### 1. Partial Screen Updates — RULED OUT

**The VMU LCD only accepts full 192-byte writes. No partial updates.**

Confirmed across multiple sources: Flycast emulator, KallistiOS, MaplePad,
and the VMU peripheral spec. The location word (partition/phase/block) is
**ignored** for LCD writes — it only applies to the storage function.

Every emulator reads and discards the location word when `FUNC_LCD` (0x04):
```cpp
// Flycast: maple_devs.cpp
case MFID_2_LCD:
    r32();    // PT, phase, block# -- read and discarded
    rptr(lcd_data, 192);
```

KallistiOS `vmu_draw_lcd()` always sends exactly 48 data words. MaplePad
asserts `NumWords * sizeof(uint) == 192`. There is no sub-region addressing.

**Implication**: Every LCD update must send all 192 bytes. Cannot optimize
by sending only changed rows.

### 2. Radio Notification Scheduling — MOST PROMISING

**Use `sd_radio_notification_cfg_set()` to predict BLE event windows.**

The SoftDevice can fire `SWI1_IRQHandler` ~800µs before every radio event
(active edge) and after every radio event completes (inactive edge).

With connection interval of 8.75-11.25ms and radio event duration of ~1-3ms:
- Safe window after radio goes idle: **~6-10ms**
- LCD write needs: **~1.65ms**
- Margin: **4-8ms** — very comfortable

**Approach:**
1. `sd_radio_notification_cfg_set(INT_ON_BOTH, DISTANCE_800US)`
2. In `SWI1_IRQHandler`: set atomic flag `RADIO_IDLE` or `RADIO_ACTIVE_SOON`
3. Before `write_lcd()`: check `RADIO_IDLE` — only start if radio just finished
4. If radio active or about to be: skip this write, retry next poll
5. Expected first-attempt success rate: **~80-90%**

**Gotchas:**
- SoftDevice can still fire brief housekeeping interrupts outside connection
  events, but these are typically microseconds, not milliseconds
- Connection events drift with clock drift — radio notification tracks this
- Short connection intervals (7.5ms) shrink the window but ours are fine

**This is the recommended primary approach. No BLE disruption, no timeslot
API, just schedule writes during known-safe windows.**

### 3. Hardware Peripheral DMA — PARTIAL FEASIBILITY

Explored SPI, I2S, and PWM as interrupt-proof TX mechanisms:

**SPI/I2S: RULED OUT** — Both only drive a single output pin via DMA.
Maple Bus needs two independent pins (SDCKA, SDCKB). Two unsynchronized
SPI instances would drift within microseconds. I2S has the same single-
output limitation.

**PWM: THEORETICALLY POSSIBLE but complex.** The nRF52840 PWM peripheral
has 4 output channels with independent duty cycles per period, all driven
from EasyDMA. In "individual" decode mode:
- Set PWM period to 500ns (matches half-bit)
- Channel 0 → SDCKA, Channel 1 → SDCKB
- Duty 0 = pin low, duty = TOP = pin high
- Pre-compute sequence of (ch0, ch1) pairs for entire waveform
- DMA plays it out — completely interrupt-proof

**Requirements:** 4800 half-bit steps × 4 channels × 2 bytes = 38.4KB RAM.
The existing 96KB sample buffer could be repurposed. Edge jitter is ≤62.5ns
at 16MHz PWM clock — should be within Maple Bus tolerance.

**Status:** Backup approach. Significant implementation effort but would
completely solve the interrupt problem without any BLE disruption.

### 4. TIMER + PPI + GPIOTE — RULED OUT

Only 8 GPIOTE channels, 6 CC registers per timer. Would need ~4800
transitions for an LCD write. The CPU would need to reprogram registers
continuously — worse than bit-banging. Good for simple periodic waveforms
(PWM-like) but cannot replay arbitrary data-dependent GPIO sequences.

### 5. Split-Phase / Resumable TX — RULED OUT for mid-packet

Maple Bus has ~500ns half-bit timing. A 1-3ms SoftDevice interrupt creates
a gap of 2000-6000 bit periods. No receiver can tolerate that mid-packet.

**However, the protocol has an interesting property:** There is no explicit
mid-packet timeout as long as both lines aren't held HIGH simultaneously.
The idle state during transmission is A=HIGH, B=LOW. The protocol spec
says there's "no maximum time limit" as long as both lines aren't HIGH
for an extended period.

**But this is unreliable because:**
- If the interrupt hits mid-bit (between the two half-bit phases), the pin
  state is indeterminate — the receiver sees an impossibly long bit period
- The VMU sits behind the controller's internal LM Bus — the controller's
  Maple Bus receiver firmware may have stricter undocumented timeouts
- No way to test the VMU's actual tolerance without hardware experiments

**Inter-chunk gaps (110-130µs) are a peripheral-side artifact** from 4-word
FIFOs in controllers/VMUs. The host (us) transmits continuously with no
natural gaps. We cannot exploit the inter-chunk timing because it doesn't
exist in host-to-peripheral direction.

### 6. Double-Buffer with Frame Skip — ALREADY DOING THIS

The current implementation is effectively double-buffered with frame skip:
build the frame, attempt write, if it fails the VMU holds the last good
frame. This is the standard approach in all embedded display drivers.

**Enhancement:** Track write success explicitly and retry on next poll
cycle instead of waiting for the next content change. Add a max retry
count (3) before backing off.

### 7. Precedent: WS2812/NeoPixel on nRF52 with SoftDevice

This is the closest real-world parallel. WS2812 LEDs need strict timing
(~400ns/800ns pulses) and bit-banging fails with SoftDevice for the exact
same reason. The community converged on:
- **I2S DMA** (most popular): Encode each protocol bit as 4 I2S bits, DMA
  streams the buffer — interrupt-proof. Official Zephyr approach.
- **PWM DMA**: Encode each bit as one PWM period with specific duty cycle.
- **SPI DMA**: Encode each bit as 3-4 SPI bits.

All work because WS2812 is **single-wire**. Our dual-wire Maple Bus makes
SPI and I2S unusable. PWM is the only peripheral that can drive 2 pins
independently from DMA.

### 8. Maple Bus Timing Breakdown

Exact timing of `write_lcd()` at 64MHz with 32 NOPs per half-bit:
```
Start pattern:           ~5.5µs  (11 half-bits)
Frame word (1):         ~32µs   (32 bits × 2 × 500ns)
Function word (1):      ~32µs
Location word (1):      ~32µs
Pixel data (48 words):  ~1,536µs
CRC byte:               ~8µs    (8 bits × 2 × 500ns)
End pattern:            ~3.5µs  (7 half-bits)
─────────────────────────────────
Total:                  ~1,649µs (~1.65ms)
```

The transmission is completely continuous — no natural pause points.
The `write_word()` loop runs back-to-back with no inter-word delay.

---

## Recommended Strategy

**Primary: Radio Notification + Double-Buffer + Frame Skip**

1. Implement `sd_radio_notification_cfg_set(INT_ON_BOTH, DISTANCE_800US)`
2. Track radio idle/active state via atomic flag in `SWI1_IRQHandler`
3. Only start LCD writes when radio is idle (safe ~6-10ms window)
4. If write fails or window unavailable, skip and retry next poll
5. Animation state machine advances regardless of write success
6. Expected result: ~80-90% frame delivery, smooth enough for VMU animation

**Backup: PWM DMA TX**

If radio notification scheduling isn't reliable enough:
1. Pre-compute Maple Bus waveform as PWM duty-cycle sequence (38.4KB)
2. Use PWM "individual" decode mode with 2 channels → SDCKA, SDCKB
3. DMA plays waveform in hardware — completely interrupt-proof
4. Significant implementation effort but eliminates the problem entirely

**Future: Pico 2 W Bridge**

PIO on RP2350 is the correct hardware for this. Dedicated Maple Bus state
machine with DMA feed, completely decoupled from wireless. All VMU goals
become trivial with dedicated cores.

---

## ICONDATA_VMS File Format

```
Offset  Size  Description
0x00    16    Text description (ASCII)
0x10    4     Offset to monochrome icon (LE u32)
0x14    4     Offset to color icon (LE u32, 0 = none)
0x18    var   Monochrome icon: 128 bytes (32x32, 1bpp)
              1 = black (foreground), 0 = transparent
var     var   Color icon: 544 bytes (32 bytes palette + 512 bytes pixels)
```

To display: BLOCK_READ the FAT/directory to find ICONDATA_VMS, then
BLOCK_READ the file data blocks, extract the monochrome bitmap, scale
or center the 32x32 icon within the 48x32 LCD, composite battery overlay.

## RE: Code Veronica ECG Reference

The game displays a real-time scrolling ECG heartbeat on the VMU LCD:
- **Fine**: Steady, regular heartbeat trace
- **Caution**: Faster, erratic heartbeat
- **Danger**: Very fast / irregular, approaching flatline

The game continuously streams LCD frames via BLOCK_WRITE (0x0C) to the
VMU. Update rate is likely ~5-15 fps based on the scrolling animation
speed. Each frame is a full 192-byte LCD write.

## Implementation Priority

1. **Rotating pulsar test** — animate the existing logo to stress-test
   LCD write reliability and measure actual frame delivery rate
2. **Radio notification timing** — implement `sd_radio_notification` to
   schedule LCD writes during safe windows between BLE events
3. **Double-buffer animation** — decouple animation state from write success
4. **ICONDATA_VMS reader** — implement BLOCK_READ for VMU filesystem
5. **PWM DMA TX** — backup if radio notification isn't sufficient
6. **Pico2Maple bridge** — ultimately the right solution for reliable writes

## BLE Architecture: Bridge ↔ nRF Communication

### Design Principles

- **Gamepad latency is the #1 priority** — never degrade HID input for VMU features
- **Keep `event_length` conservative** — no increase for VMU data, let it fragment
  across connection events if needed
- **Save transfer is manual** — button combo triggered, default off, not real-time
- **Live LCD streaming is a stretch goal** — nice to have, not required

### Custom GATT Service for VMU Data

Separate from the Xbox HID gamepad service. Own UUID, own characteristics.
The host sees two independent services that don't interfere.

**Service UUID**: `DC000001-0000-1000-8000-00805F9B34FB` (custom)

| Characteristic | UUID | Size | Direction | Use |
|---|---|---|---|---|
| LCD Frame | `DC000002-...` | 192 bytes | Bridge → nRF (Write) | Display frames |
| Save Data | `DC000003-...` | up to 244 bytes/chunk | Both directions | Save transfer |

### MTU and Packet Sizing

- nRF52840 S140 supports ATT_MTU up to **247 bytes** (244 payload)
- 192-byte LCD frame fits in **one notification** at MTU 247
- With DLE (automatic on S140), goes as **one radio packet** on the air
- Save blocks (512 bytes) need **3 chunks** with a sequence header
- Full save (128KB) = ~530 notifications ≈ 8 seconds at 15ms interval

**Config changes needed** (in `softdevice_config()`):
- `att_mtu: 247` (currently 64)
- `attr_tab_size: 4096` (currently 2048, needed for 192-byte characteristic)
- `event_length`: **keep at 6** — gamepad priority, VMU data fragments if needed

### Save Transfer Protocol

- **Triggered by**: Sync button combo (e.g., hold 5s)
- **Default**: Off — must be enabled per-session
- **Direction**: VMU → nRF → BLE → Bridge → SD card (or reverse)
- **Not real-time**: Transfer happens during idle gameplay or pause screen
- **Chunked**: 512-byte blocks sent as 3× ~170-byte BLE writes with block number header
- **Checksummed**: Each block verified before ACK, retry on failure

### Flow: Bridge → nRF → VMU (LCD frame from game)

```
Dreamcast → Maple BLOCK_WRITE → Pico2Maple captures LCD frame
  → Bridge writes 192 bytes to BLE LCD characteristic
    → nRF receives, stores in vmu_framebuf, sets dirty flag
      → Generic writer sends to VMU on next radio idle window
```

### Flow: VMU → nRF → Bridge (save backup)

```
User holds sync combo → nRF sends BLOCK_READ to VMU over Maple Bus
  → Reads save data one block at a time (512 bytes, retries on failure)
    → Chunks each block into BLE notifications to bridge
      → Bridge writes to SD card
```

## Future: Pico 2 W Bridge

On the Maple bridge with dedicated cores, all of these problems evaporate:
- PIO handles Maple Bus timing in hardware (interrupt-proof)
- Dedicated core for Maple means no contention with wireless
- SD card provides ample save storage
- Full VMU emulation becomes feasible (storage + LCD + clock)

### Bridge as VMU Emulator (Primary Save Architecture)

The bridge emulates a **second VMU** on the Maple Bus (sub-peripheral slot 2,
address `0x02`). The Dreamcast BIOS treats it as a real VMU — saves show up
in the standard file manager, and users copy saves using the OEM UI.

**Commands to handle (5 total):**
- `0x01` Device Info — report function codes `0x0E` (storage + LCD + timer)
- `0x0A` Get Memory Info — report standard VMU geometry (200 user blocks)
- `0x0B` Block Read — serve 512-byte blocks from SD-backed RAM buffer
- `0x0C` Block Write — accept blocks in 4 phases (128 bytes each)
- `0x0D` Get Last Error — commit writes, return status

**Storage:** 128KB RAM buffer per bank, backed by `.vmu` image files on SD card.
Each `.vmu` file is a standard raw dump — compatible with Flycast, Redream,
Dreamcast VMU Explorer, and other existing tools.

### Multi-Bank Storage (Expanded Memory)

Original 4x memory cards (Nexus, Performance) used a physical button to cycle
128KB banks. The Dreamcast had no idea — each bank was an independent VMU
filesystem. Same approach here:

**Phase 1: Button bank switching**
- SD card holds multiple `.vmu` image files (e.g., `bank_00.vmu` .. `bank_99.vmu`)
- Button on bridge cycles active bank
- Hot-swap: flush current bank to SD, load next bank into RAM
- Dreamcast re-enumerates on next poll, sees "new" VMU with different saves

**Phase 2: USB mass storage**
- Bridge mounts as USB mass storage when plugged into PC/Mac
- SD card contents visible as regular files
- Users drag `.vmu` files on/off, use existing tools to edit saves
- Download saves from emulators, transfer to real hardware and vice versa

**Phase 3: Web UI (WiFi)**
- CYW43 on Pico 2 W hosts AP mode — bridge creates its own WiFi network
- Phone/laptop connects, opens browser to bridge's web server
- File manager showing all `.vmu` banks and their save contents
- Upload/download `.vmu` images
- Select active bank
- Parse VMS headers to show game names and icons as previews
- Drag and drop saves between banks
- No app install needed — just a browser

### VMU Filesystem Quick Reference

```
Block layout (256 blocks × 512 bytes = 128KB):
  0-199:   User data (save files, linked via FAT)
  241-253: Directory (13 blocks, 200 entries × 32 bytes)
  254:     FAT (1 block, 256 entries × 2 bytes)
  255:     Root block (filesystem metadata)

Directory entry (32 bytes):
  0x00:      File type (0x33=DATA, 0xCC=GAME)
  0x02-0x03: First block number (u16 LE)
  0x04-0x0F: Filename (12 bytes ASCII)
  0x18-0x19: Size in blocks (u16 LE)

FAT entries (u16 LE):
  0xFFFC = free block
  0xFFFA = end of file (EOF)
  other  = next block in chain

VMS file header (in first data block):
  0x00-0x0F: Short description (shown in BIOS)
  0x10-0x1F: Long description
  0x20-0x2F: Product ID (identifies game)
```

## References

- [Flycast emulator maple_devs.cpp](https://github.com/flyinghead/flycast/blob/master/core/hw/maple/maple_devs.cpp)
- [MaplePad - RP2040 controller emulator](https://github.com/mackieks/MaplePad)
- [KallistiOS vmu.h](http://gamedev.allusion.net/docs/kos-current/vmu_8h.html)
- [DreamPicoPort](https://github.com/OrangeFox86/DreamPicoPort)
- [Nordic Radio Notification Guide](https://devzone.nordicsemi.com/guides/short-range-guides/b/software-development-kit/posts/radio-notification)
- [Dreamcast Maple Bus Wiki](https://dreamcast.wiki/Maple_bus)
- [VMU Peripheral Wiki](https://dreamcast.wiki/VMU_peripheral)
- [mc.pp.se Maple Bus Wire Protocol](http://mc.pp.se/dc/maplewire.html)
- [Zephyr WS2812 LED Strip Driver](https://docs.zephyrproject.org/latest/samples/drivers/led_ws2812/README.html)
- [Nordic PWM Spec (nRF52840)](https://docs.nordicsemi.com/bundle/ps_nrf52840/page/pwm.html)
