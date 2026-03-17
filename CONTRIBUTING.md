# Contributing

Thanks for checking out Pulsar Dreamcast BLE! Whether you're fixing a bug, adding a feature, or porting to new hardware — contributions are welcome.

## Getting Started

### Prerequisites

- Rust stable toolchain with `thumbv7em-none-eabihf` target
- For on-hardware testing: nRF52840 DK or Seeed XIAO nRF52840 with a debug probe

```bash
rustup target add thumbv7em-none-eabihf
```

### Building

```bash
# DK (default)
cargo build --release

# XIAO
cargo build --release --no-default-features --features board-xiao
```

### Running Checks

Before submitting a PR, run the full check suite:

```bash
./scripts/ci.sh
```

This runs formatting, maple-protocol unit tests, clippy lints, and release builds for both board targets. CI runs the same checks on every PR.

## Submitting Changes

1. Fork the repo and create a branch from `main`
2. Make your changes — keep commits focused and incremental
3. Run `./scripts/ci.sh` and ensure it passes
4. Open a pull request with a clear description of what and why

Don't worry about getting everything perfect — feedback on PRs is part of the process.

## Project Structure

- **`maple-protocol/`** — Pure protocol library (no embedded deps, runs on host). Tests go here.
- **`src/`** — Firmware: BLE stack, Maple Bus GPIO, board support, button handling.
- **`src/board/`** — Board-specific pin mappings, LEDs, battery, and power management.
- **`docs/`** — Protocol reference, user guide, learnings.
- **`3d_files/`** — Enclosure models (not covered by GPL, see [3d_files/README.md](3d_files/README.md)).

## Ways to Contribute

### No Hardware Needed

The `maple-protocol` crate is pure Rust with no embedded dependencies. Contributions to controller state parsing, HID report generation, and packet construction can be built and tested entirely on the host with `cargo test`.

### Hardware Testing

If you have hardware, testing with a real Dreamcast controller is incredibly valuable. Bug reports with details about your setup (board, controller model, host device) help a lot.

### Adding Board Support

The firmware is designed to make adding new boards straightforward. Each board gets a module in `src/board/` that defines pin mappings, LED behavior, and optional features like battery monitoring or sleep. If you have a different nRF52840 board (Adafruit Feather, nice!nano, etc.), adding support is a great first contribution.

We're also open to supporting other chips in the nRF52 family (nRF52833, nRF5340) — the Embassy and SoftDevice ecosystem covers these, so much of the firmware would carry over. Support for non-Nordic chips (ESP32, RP2040) would be a bigger effort since it means replacing the BLE stack, but the `maple-protocol` crate is fully portable.

The current Maple Bus implementation (`src/maple/gpio_bus.rs`) uses CPU bit-banging with bulk sampling because the nRF52840 doesn't have a hardware peripheral suited to the 2Mbps alternating-clock protocol. Other chips may handle this differently — for example, the RP2040's PIO state machines could implement the protocol timing in hardware rather than software. A port would replace `gpio_bus.rs` while keeping the rest of the stack intact.

If you're thinking about a port, open an issue first so we can discuss the approach.

## Questions?

Open an issue — happy to help.
