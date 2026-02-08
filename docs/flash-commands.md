# Flash and Debug Commands Reference

Common commands for flashing and debugging the nRF52840 DK.

## Prerequisites

- `nrfjprog` (Nordic command-line tools)
- `probe-rs` (Rust embedded tools)
- `cargo-embed` (optional, for integrated flash+debug)

---

## Recovery (When Chip is Locked)

```bash
# Full chip recovery (use when chip is unresponsive)
nrfjprog --recover

# Erase all flash
nrfjprog --eraseall
```

---

## Flashing SoftDevice (One-Time)

```bash
# Erase chip first
nrfjprog --eraseall

# Flash S140 SoftDevice
nrfjprog --program vendor/s140_softdevice/s140_nrf52_7.3.0_softdevice.hex --verify
```

---

## Flashing Application

### Using nrfjprog (recommended)

```bash
# Build release
cargo build --release

# Convert ELF to hex
arm-none-eabi-objcopy -O ihex \
    target/thumbv7em-none-eabihf/release/embedded_rust_setup \
    target/app.hex

# Flash (preserves SoftDevice)
nrfjprog --program target/app.hex --verify

# Reset to start
nrfjprog --reset
```

### Using probe-rs

```bash
# Flash and run
probe-rs download --chip nRF52840_xxAA \
    target/thumbv7em-none-eabihf/release/embedded_rust_setup

# Or with cargo-embed (if configured)
cargo embed --release
```

---

## Debug and RTT

### Attach to running target with RTT

```bash
probe-rs attach --chip nRF52840_xxAA \
    target/thumbv7em-none-eabihf/release/embedded_rust_setup
```

### Run with debugger (GDB)

```bash
probe-rs gdb --chip nRF52840_xxAA \
    target/thumbv7em-none-eabihf/release/embedded_rust_setup
```

---

## Status Commands

```bash
# List connected probes
probe-rs list

# Check device info
nrfjprog --deviceversion

# Read device memory
nrfjprog --memrd 0x00027000 --n 16
```

---

## Reset Commands

```bash
# Soft reset
nrfjprog --reset

# Pin reset (hardware)
nrfjprog --pinreset
```

---

## Erase Commands

```bash
# Erase all (including SoftDevice!)
nrfjprog --eraseall

# Erase only app section (preserves SoftDevice)
nrfjprog --erasepage 0x27000-0x100000
```

---

## Troubleshooting

### "Core is in locked up status"
```bash
nrfjprog --recover
```

### "Probe not found"
- Unplug and replug USB
- Check `probe-rs list` output
- Try `nrfjprog --deviceversion` to verify connection

### Flash verify fails
```bash
nrfjprog --eraseall
# Then re-flash SoftDevice and app
```

### BLE not advertising
- Verify SoftDevice is flashed at 0x0
- Check that app starts at 0x27000 (memory.x)
- RTT should show "BLE: Advertising..."
