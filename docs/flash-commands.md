# Flash & Debug Reference

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

On panic, the firmware writes the panic message to flash (`0xFC000`) and resets. On the next boot (with RTT enabled), the stored panic is printed and cleared. This helps diagnose crashes without needing to reproduce them with a debugger attached.

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

# Erase app only (preserves SoftDevice)
nrfjprog --erasepage 0x27000-0x100000
```
