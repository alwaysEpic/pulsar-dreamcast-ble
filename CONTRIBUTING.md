# Contributing

Thanks for your interest in contributing to Pulsar Dreamcast BLE!

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
./check.sh
```

This runs:
- `cargo fmt` — formatting
- `cargo test` — maple-protocol unit tests
- `cargo clippy` — lints for both crates
- Release builds for both board targets

All checks must pass. CI runs the same checks on every PR.

## Submitting Changes

1. Fork the repo and create a branch from `main`
2. Make your changes — keep commits focused and incremental
3. Run `./check.sh` and ensure it passes
4. Open a pull request with a clear description of what and why

## Project Structure

- **`maple-protocol/`** — Pure protocol library (no embedded deps, runs on host). Tests go here.
- **`src/`** — Firmware: BLE stack, Maple Bus GPIO, board support, button handling.
- **`docs/`** — Protocol reference, user guide, learnings.
- **`3d_files/`** — Enclosure models (not covered by GPL, see [3d_files/README.md](3d_files/README.md)).

## Hardware Testing

If you have hardware, testing with a real Dreamcast controller is the most valuable contribution. See [docs/test_plan.md](docs/test_plan.md) for the test matrix.

If you don't have hardware, contributions to `maple-protocol` (parsing, HID reports, packet construction) can be tested entirely on the host.

## Questions?

Open an issue — happy to help.
