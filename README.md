# Pulsar Dreamcast BLE

[![CI](https://github.com/alwaysEpic/pulsar-dreamcast-ble/actions/workflows/ci.yml/badge.svg)](https://github.com/alwaysEpic/pulsar-dreamcast-ble/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)

A Bluetooth Low Energy adapter that lets you use a Dreamcast controller wirelessly. Built in Rust on the nRF52840 SoC, it speaks the Dreamcast Maple Bus protocol natively and presents itself as an Xbox One S BLE gamepad to any connected host.

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
- 4.7kΩ pull-up resistors from each data line to 3.3V
- Controller powered at 5V (signals are 3.3V TTL)

**XIAO pin mapping:**

| Function | Pin | Notes |
|----------|-----|-------|
| SDCKA (Red) | P0.05 (D5) | Maple Bus clock/data A |
| SDCKB (White) | P0.03 (D1) | Maple Bus clock/data B |
| Sync Button | P1.15 (D10) | Pairing, name toggle, wake from sleep |
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

```bash
rustup target add thumbv7em-none-eabihf
cargo install cargo-embed
```

## Building & Flashing

**XIAO** (must use `--release` — debug builds break Maple Bus timing):
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

Pure protocol logic is extracted into the `maple-protocol` library crate and runs on the host:

```bash
cd maple-protocol && cargo test
```

This tests controller state parsing, HID report generation, and packet construction without needing embedded hardware.

## Debugging (RTT)

The firmware uses RTT (Real-Time Transfer) for debug output. `cargo embed` opens RTT automatically after flashing.

To attach to an already-running device:
```bash
probe-rs attach --chip nRF52840_xxAA target/thumbv7em-none-eabihf/release/pulsar-dreamcast-ble
```

Note: `rprintln!()` takes ~15µs per call. Do not use in timing-critical paths (TX/RX hot path).

## Project Structure

```
.
├── maple-protocol/            # Pure protocol library (no embedded deps, host-testable)
│   └── src/
│       ├── controller_state.rs    # Dreamcast controller state parsing
│       ├── xbox_hid.rs            # Xbox One S BLE gamepad report
│       └── packet.rs              # Maple Bus packet construction
├── src/
│   ├── main.rs                # Entry point, Maple Bus polling loop
│   ├── lib.rs                 # Shared signals, constants, module declarations
│   ├── button.rs              # Sync button task (hold, triple-press)
│   ├── ble/
│   │   ├── task.rs            # BLE advertising/connection state machine
│   │   ├── hid.rs             # GATT service definitions (HID, DeviceInfo, Battery)
│   │   ├── security.rs        # BLE bonding/pairing
│   │   ├── flash_bond.rs      # Flash storage for bonds and name preference
│   │   └── softdevice.rs      # SoftDevice init and advertising
│   ├── maple/
│   │   ├── gpio_bus.rs        # Maple Bus GPIO bit-banging
│   │   ├── host.rs            # Maple Bus host (Device Info, Get Condition)
│   │   ├── controller_state.rs    # Re-exports from maple-protocol
│   │   └── packet.rs              # Re-exports from maple-protocol
│   └── board/
│       ├── dk.rs              # nRF52840 DK pin mappings and LEDs
│       └── xiao.rs            # XIAO pin mappings, battery, sleep
├── 3d_files/                  # VMU enclosure models (see 3d_files/README.md)
├── docs/
│   ├── users_guide.md         # Non-technical user guide
│   ├── maple_bus_protocol.md  # Maple Bus protocol reference
│   ├── learnings.md           # Implementation lessons learned
│   ├── battery_optimization.md    # Power management strategy
│   ├── flash-commands.md      # Flashing & debugging cheat sheet
│   ├── signal_references/     # Oscilloscope captures
│   └── working_logs/          # Development debug logs (DK and XIAO)
└── Embed.toml                 # cargo-embed configuration
```

## Contributing

Contributions are welcome! See [CONTRIBUTING.md](CONTRIBUTING.md) for build instructions and how to submit changes.

## 3D Models

The `3d_files/` directory contains VMU-shaped enclosure models. These are **not** covered by the GPL-3.0 license — see [3d_files/README.md](3d_files/README.md) for attribution details.

## License

This project is licensed under the [GNU General Public License v3.0 or later](LICENSE).

See individual source files for the SPDX license identifier.
