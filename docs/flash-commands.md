# Flash & Debug Reference

Three boards, three routes. Pick by what you have:

| Board | Feature | How it's flashed |
|---|---|---|
| **XIAO** | `board-xiao` | [UF2 over USB](#uf2-flashing-recommended) — no probe needed. Add `,rtt` for debug logging |
| **DK** | `board-dk` | [SWD](#swd-flashing-development) via the onboard J-Link. Includes `rtt` by default |
| **Pulsar v1** | `board-pulsarv1` | [Signed OTA](#pulsar-v1-signed-ota), or SWD for bring-up |

Release builds are mandatory on all three — debug builds miss the Maple Bus timing window.

## UF2 Flashing (Recommended)

The easiest way to flash the XIAO — no debug probe needed. The XIAO ships with a UF2 bootloader that includes the Nordic SoftDevice S140 v7.3.0.

### Flash Pre-Built Firmware

1. Download the `.uf2` file from [Releases](https://github.com/alwaysEpic/pulsar-dreamcast-ble/releases)
2. Double-tap the reset button on the XIAO — it mounts as `XIAO-BOOT`
3. Copy the file to the drive:
   ```bash
   cp pulsar-dreamcast-ble.uf2 /Volumes/XIAO-BOOT/
   ```
4. The board auto-resets and runs the firmware

### Build and Flash from Source

```bash
# Build (production — no RTT logging)
cargo build --release --no-default-features --features board-xiao

# Build (development — with RTT debug logging)
cargo build --release --no-default-features --features board-xiao,rtt

# Convert ELF → HEX → UF2
rust-objcopy -O ihex \
  target/thumbv7em-none-eabihf/release/pulsar-dreamcast-ble \
  target/pulsar-dreamcast-ble.hex

python3 uf2conv.py \
  -c -f 0xADA52840 \
  -o target/pulsar-dreamcast-ble.uf2 \
  target/pulsar-dreamcast-ble.hex

# Double-tap reset, then copy
cp target/pulsar-dreamcast-ble.uf2 /Volumes/XIAO-BOOT/
```

**uf2conv.py** is from [Microsoft's UF2 repo](https://github.com/microsoft/uf2/tree/master/utils). Download `uf2conv.py` and `uf2families.json` into the same directory.

### Restore Stock Bootloader

If the bootloader has been overwritten (e.g., by SWD flashing), restore it with a J-Link:

```bash
curl -LO https://github.com/adafruit/Adafruit_nRF52_Bootloader/releases/download/0.10.0/xiao_nrf52840_ble_bootloader-0.10.0_s140_7.3.0.hex

nrfjprog --program xiao_nrf52840_ble_bootloader-0.10.0_s140_7.3.0.hex --chiperase --verify --reset
```

This restores both the UF2 bootloader and SoftDevice S140 v7.3.0 in a single flash.

---

## SWD Flashing (Development)

For development with RTT debug logging. Requires a J-Link probe or nRF52840 DK.

### Environment Setup

```bash
rustup target add thumbv7em-none-eabihf
cargo install cargo-embed
```

### SoftDevice

The Nordic S140 SoftDevice must be flashed once before the application. Download v7.3.0 from [Nordic's website](https://www.nordicsemi.com/Products/Development-software/S140/Download).

```bash
nrfjprog --eraseall
nrfjprog --program s140_nrf52_7.3.0_softdevice.hex --verify
```

### Building & Flashing

**XIAO** — must use `--release` (debug builds break Maple Bus timing):
```bash
# Development (with RTT logging)
cargo embed --release --no-default-features --features board-xiao,rtt

# Production (no RTT — smaller binary, slightly lower power)
cargo embed --release --no-default-features --features board-xiao
```

**DK** (default target, always includes RTT):
```bash
cargo embed --release
```

### Build Only (no flash)
```bash
# DK (includes RTT by default)
cargo build --release

# XIAO development (with RTT)
cargo build --release --no-default-features --features board-xiao,rtt

# XIAO production (no RTT)
cargo build --release --no-default-features --features board-xiao
```

---

## Pulsar v1 (Signed OTA)

Pulsar v1 carries a Nordic Secure DFU bootloader, so it updates wirelessly and only accepts
firmware signed with the project's release key. Building the application is the same as any
other board:

```bash
# Production
cargo build --release --no-default-features --features board-pulsarv1

# With RTT (bring-up only — see the note below)
cargo build --release --no-default-features --features board-pulsarv1,rtt
```

**Putting a unit into update mode:** hold sync past 3.5s with the controller's **Start** held.
Three fast flashes confirm, and it reboots advertising as `PulsarDFU`. It refuses on a low
battery unless charging (`CHRG` on the VMU). The gesture does not clear your pairing.

> **No button or pin-reset entry on this board, and no UF2 volume.** `sdk_config.h` sets both
> `NRF_BL_DFU_ENTER_METHOD_BUTTON` and `NRF_BL_DFU_ENTER_METHOD_PINRESET` to `0`; entry is
> GPREGRET `0xB1` only, written by the firmware gesture. Double-tapping reset does nothing and
> no `XIAO-BOOT` drive appears — that is the design, not a fault. Practically: the gesture
> needs a controller attached (it checks Start), and an absent or invalid application
> auto-enters DFU at power-on, so a failed update recovers itself rather than bricking.

> **The web updater cannot talk to an Adafruit-bootloader board.** Chrome's Web Bluetooth
> blocklist permanently excludes the legacy DFU service UUID (`00001530-…`) by name, so the
> UF2 boards are unreachable from a browser no matter what. Nordic Secure DFU (`0xFE59`) is
> not blocklisted, which is why retail units carry that bootloader instead. XIAO and DK builds
> use UF2 or SWD.

**Browser requirements for the web route.** Web Bluetooth is not universal, and the common
failure is a page that loads fine and never offers a device:

| Browser | Status |
|---|---|
| Chrome / Edge / Opera, Windows or macOS | Works as shipped |
| **Brave** | **Disabled by default** — enable `Web Bluetooth API` in `brave://flags`, then relaunch. Brave gates it deliberately as a privacy decision |
| Chrome on Linux | Requires `chrome://flags/#enable-experimental-web-platform-features`, plus BlueZ 5.41+ |
| Firefox, Safari (incl. iOS/iPadOS) | Not implemented, no workaround in-browser |

Web Bluetooth also requires a secure context and a user gesture, so the page must be served
over HTTPS and the chooser has to be opened by a click.

**Signed packages are produced by the maintainer** and served from the update page. Because
the bootloader verifies a signature, a locally built binary will not install over the air —
use SWD for local work on a Pulsar v1, or a XIAO build for iteration.

> **RTT does not work on this board.** There is no debug probe on a retail unit, so `rtt`
> compiles but you cannot read it. The HID side channels exist for exactly this reason —
> `gauge-debug`, `connparam-debug`, `poll-period-debug`, and `maple-fail-debug` smuggle
> telemetry out through unused report bytes, readable with `scripts/hid_capture.py`. They are
> mutually exclusive; see `Cargo.toml` for the invocation of each.

**Owner builds.** Nothing here prevents you running your own firmware on hardware you own:
the signature check lives in the bootloader, and the bootloader can be replaced over SWD with
one carrying your own key. That route is yours to take — the release key only governs which
firmware the *official* update channel can install.

## Debugging

### RTT (Real-Time Transfer)

RTT logging is gated behind the `rtt` feature flag. The DK board always includes it. For XIAO, add `rtt` to the features list:

```bash
cargo embed --release --no-default-features --features board-xiao,rtt
```

`cargo embed` opens RTT automatically after flashing. To attach to an already-running device:
```bash
probe-rs attach --chip nRF52840_xxAA target/thumbv7em-none-eabihf/release/pulsar-dreamcast-ble
```

### Panic Logging

On panic, the firmware writes the panic message to flash (`0xF1000`) and resets. On the next boot (with RTT enabled), the stored panic is printed and cleared. This helps diagnose crashes without needing to reproduce them with a debugger attached.

### GDB
```bash
probe-rs gdb --chip nRF52840_xxAA target/thumbv7em-none-eabihf/release/pulsar-dreamcast-ble
```

---

## Recovery

### Chip Locked / Unresponsive
```bash
nrfjprog --recover
```

### Re-flash Everything (SWD)
```bash
nrfjprog --eraseall
nrfjprog --program s140_nrf52_7.3.0_softdevice.hex --verify
cargo embed --release
```

### Re-flash Everything (UF2)
Restore the bootloader first (see above), then double-tap reset and copy the `.uf2` file.

### Probe Not Found
- Unplug and replug USB
- Check `probe-rs list` for connected probes
- Kill stale processes: `ps aux | grep -iE 'jlink|probe-rs|nrf' | grep -v grep`

---

## Useful Commands

```bash
# Check connected probes
probe-rs list

# Device info
nrfjprog --deviceversion

# Read flash memory
nrfjprog --memrd 0x00027000 --n 16

# Soft reset
nrfjprog --reset

# Erase app only (preserves SoftDevice, bootloader, and stored data pages).
# Do NOT erase up to 0x100000: 0xF4000+ is the bootloader, 0xFE000 the MBR
# params page, 0xFF000 its settings page — wiping those needs a full reflash.
nrfjprog --erasepage 0x27000-0xF1000
```
