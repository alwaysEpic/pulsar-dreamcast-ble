# Flash & Debug Quick Reference

## Environment Setup

### Toolchain
```bash
rustup target add thumbv7em-none-eabihf
cargo install cargo-embed
```

### Debug Probe
- **nRF52840 DK**: Built-in J-Link — just connect USB
- **XIAO nRF52840**: Needs external SWD probe (J-Link, DK as programmer, or CMSIS-DAP)

### SoftDevice
The Nordic S140 SoftDevice must be flashed once before the application. Download from [Nordic's website](https://www.nordicsemi.com/Products/Development-software/s140).

```bash
# Erase chip and flash SoftDevice
nrfjprog --eraseall
nrfjprog --program s140_nrf52_7.3.0_softdevice.hex --verify
```

---

## Building & Flashing

### DK (default target)
```bash
cargo embed --release
```

### XIAO
**Must use `--release`** — debug builds break Maple Bus timing (Embassy GPIO calls don't inline).
```bash
cargo embed --release --no-default-features --features board-xiao
```

### Build Only (no flash)
```bash
# DK
cargo build --release

# XIAO
cargo build --release --no-default-features --features board-xiao
```

---

## Debugging

### RTT (Real-Time Transfer)
`cargo embed` opens RTT automatically after flashing.

To attach to an already-running device:
```bash
probe-rs attach --chip nRF52840_xxAA target/thumbv7em-none-eabihf/release/pulsar-dreamcast-ble
```

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

### Re-flash Everything
```bash
nrfjprog --eraseall
nrfjprog --program s140_nrf52_7.3.0_softdevice.hex --verify
cargo embed --release
```

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
