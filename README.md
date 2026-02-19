# Dreamcast Wireless Controller Adapter

A Bluetooth Low Energy adapter that lets you use a Dreamcast controller wirelessly. Built on the nRF52840 SoC, it speaks the Dreamcast Maple Bus protocol natively and presents itself as an Xbox One S BLE gamepad to any connected host.

## Features

- Full Dreamcast controller support: A/B/X/Y, Start, D-pad, analog stick, analog triggers
- Emulates Xbox One S BLE gamepad (compatible with iBlueControlMod and other BLE HID hosts)
- 60Hz controller polling, ~125Hz BLE report rate
- Flash-based bonding (pairing persists across power cycles)
- Sync button for pairing and device name toggle
- Battery monitoring with BLE Battery Service (XIAO only)
- Inactivity sleep with button wake (XIAO only)

## Hardware

### Supported Boards

| Board | Status | Notes |
|-------|--------|-------|
| Seeed XIAO nRF52840 | Primary target | Battery, sleep, boost converter support |
| nRF52840 DK | Development | Full debug LED support |

### Wiring

Both boards require:
- 4.7k pull-up resistors from each data line to 3.3V
- Controller powered at 5V (signals are 3.3V TTL)

**XIAO pin mapping:**

| Function | Pin | Notes |
|----------|-----|-------|
| SDCKA (Red) | P0.05 (D5) | Maple Bus clock/data A |
| SDCKB (White) | P0.03 (D1) | Maple Bus clock/data B |
| Sync Button | P1.12 (D7) | Also used as wake-from-sleep |
| Boost SHDN | P0.28 (D2) | 5V boost converter enable |
| Battery ADC | P0.31 (AIN7) | Via P0.14 enable gate |
| RGB LED | P0.26/P0.30/P0.06 | R/G/B, active low |

**DK pin mapping:**

| Function | Pin | Notes |
|----------|-----|-------|
| SDCKA (Red) | P0.05 | Maple Bus clock/data A |
| SDCKB (White) | P0.06 | Maple Bus clock/data B |
| Sync Button | P0.25 | Button 4, active low |
| Sync LED | P0.13 | LED1 |
| Status LEDs | P0.14-P0.16 | LED2-LED4 |

## Prerequisites

- Rust toolchain with `thumbv7em-none-eabihf` target
- `probe-rs` or `cargo-embed` for flashing
- nRF52840 SoftDevice S140 pre-flashed on the target

```
rustup target add thumbv7em-none-eabihf
cargo install cargo-embed
```

## Building & Flashing

**XIAO** (must use `--release` -- debug builds break Maple Bus timing):
```bash
cargo embed --release --no-default-features --features board-xiao
```

**DK:**
```bash
cargo embed --release
```

The default feature is `board-dk`, so `cargo embed --release` targets the DK.

### SoftDevice

The S140 SoftDevice must be flashed before the application. If the chip is erased:
```bash
probe-rs erase --chip nRF52840_xxAA --allow-erase-all
# Then flash S140 hex (see Nordic SDK)
```

## Testing

Pure protocol logic is extracted into the `dc-protocol` library crate and runs on the host:

```bash
cd dc-protocol && cargo test
```

This tests controller state parsing, HID report generation, and packet construction without needing embedded hardware.

## Debugging (RTT)

The firmware uses RTT (Real-Time Transfer) for debug output. `cargo embed` opens RTT automatically after flashing.

To attach to an already-running device:
```bash
probe-rs attach --chip nRF52840_xxAA target/thumbv7em-none-eabihf/release/embedded_rust_setup
```

Note: `rprintln!()` takes ~15us per call. Do not use in timing-critical paths (TX/RX hot path).

## Project Structure

```
.
├── dc-protocol/           # Pure protocol library (no embedded deps, host-testable)
│   └── src/
│       ├── controller_state.rs  # Dreamcast controller state parsing
│       ├── hid.rs               # Xbox One S BLE gamepad report
│       └── packet.rs            # Maple Bus packet construction
├── src/
│   ├── main.rs            # Entry point, polling loop, BLE task
│   ├── ble/
│   │   ├── hid.rs         # GATT service definitions (HID, DeviceInfo, Battery)
│   │   ├── security.rs    # BLE bonding/pairing
│   │   ├── flash_bond.rs  # Flash storage for bonds and name preference
│   │   └── softdevice.rs  # SoftDevice init and advertising
│   ├── maple/
│   │   ├── gpio_bus.rs    # Maple Bus GPIO bit-banging
│   │   ├── host.rs        # Maple Bus host (Device Info, Get Condition)
│   │   ├── controller_state.rs  # Re-exports from dc-protocol
│   │   └── packet.rs            # Re-exports from dc-protocol
│   └── board/
│       ├── dk.rs          # nRF52840 DK pin mappings and LEDs
│       └── xiao.rs        # XIAO pin mappings, battery, sleep
├── docs/
│   └── users_guide.md    # Non-technical user guide
└── Embed.toml             # cargo-embed configuration
```
