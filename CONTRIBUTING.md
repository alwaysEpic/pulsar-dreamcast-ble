# Contributing

Thanks for checking out Pulsar Dreamcast BLE! Whether you're fixing a bug, adding a feature, or porting to new hardware — contributions are welcome.

## Getting Started

### Prerequisites

- Rust stable toolchain with `thumbv7em-none-eabihf` target
- For on-hardware testing, one of the three boards: an nRF52840-DK (built-in J-Link), a
  Seeed XIAO nRF52840 (flashes over USB — no probe needed), or a Pulsar v1. Each has a
  start-to-finish guide under [`docs/build/`](docs/build/).

```bash
rustup target add thumbv7em-none-eabihf
```

### Building

```bash
# DK (default)
cargo build --release

# XIAO
cargo build --release --no-default-features --features board-xiao

# Pulsar v1
cargo build --release --no-default-features --features board-pulsarv1
```

Boards are selected by mutually exclusive feature, never by `cfg` sprinkled through the
logic. `--release` is mandatory on every board — a debug build misses the Maple timing
window outright.

### Running Checks

Two commands:

```bash
./scripts/check.sh [dk|xiao|pulsarv1]   # after every change  — ~2 s warm
./scripts/ci.sh                         # before every commit — ~12 s warm
```

`check.sh` runs the `maple-protocol` tests and clippy for one board. `ci.sh` is the gate:
formatting, the `maple-protocol` tests (including the BlueRetro mapping fixtures), clippy for
all three boards, and every shipped release build (`board-xiao` and `board-pulsarv1` with and
without `rtt`, plus `board-dk`). Each build is then checked for timing invariants — the Maple
sampling loop must stay word-aligned with its pinned encoding, and the hardware-timed TX path
must be present (`scripts/check_timing_invariants.sh`). CI runs the same checks on every PR.

The lint policy lives in `[workspace.lints]` in `Cargo.toml`, so a bare `cargo clippy` gives
exactly what CI gives. Use `#[expect(lint, reason = "…")]` rather than `#[allow]`; every
`unsafe` block needs a `// SAFETY:` comment, and `unwrap`/`expect`/`panic` are forbidden
outside tests — there is no unwind on this target.

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
- **`docs/`** — Build guides (`docs/build/`), user guide, protocol reference, learnings; `docs/MOC.md` is the index.
- **`3d_files/`** — Enclosure models (not covered by GPL, see [3d_files/README.md](3d_files/README.md)).

### Decision records

Comments cite `ADR-NNN` and design-note sections (e.g. "remap design v2 §4.5"). Those are
the maintainer's architecture decision records, kept outside this repository. The comment
states the decision and its reason; the number is a cross-reference, not required reading.
If a comment leans on a record without saying what it decided, that is a bug — open an issue.

## Ways to Contribute

### No Hardware Needed

The `maple-protocol` crate is pure Rust with no embedded dependencies. Contributions to controller state parsing, HID report generation, and packet construction can be built and tested entirely on the host with `cargo test`.

### Hardware Testing

If you have hardware, testing with a real Dreamcast controller is incredibly valuable. Bug reports with details about your setup (board, controller model, host device) help a lot.

For changes that touch the Maple Bus or poll loop, `scripts/bench_check.sh` is the release gate: it flashes a DK with timing instrumentation, waits for your BLE host to connect, and fails unless the input path is healthy on real silicon (median sampling read-min ≤ 3250µs and median controller-read retries ≤ 90 per 60 polls). It needs the physical bench plus a connected host, so it's a manual pre-release check rather than part of `ci.sh`. The script prints the healthy reference thresholds and saves the RTT log from any failing run for diagnosis.

### Adding Board Support

The firmware is designed to make adding new boards straightforward. Each board gets a module in `src/board/` that defines pin mappings, LED behavior, and optional features like battery monitoring or sleep. If you have a different nRF52840 board (Adafruit Feather, nice!nano, etc.), adding support is a great first contribution.

We're also open to supporting other chips in the nRF52 family (nRF52833, nRF5340) — the Embassy and SoftDevice ecosystem covers these, so much of the firmware would carry over. Support for non-Nordic chips (ESP32, RP2040) would be a bigger effort since it means replacing the BLE stack, but the `maple-protocol` crate is fully portable.

The current Maple Bus implementation (`src/maple/gpio_bus.rs`) uses CPU bit-banging with bulk sampling because the nRF52840 doesn't have a hardware peripheral suited to the 2Mbps alternating-clock protocol. Other chips may handle this differently — for example, the RP2040's PIO state machines could implement the protocol timing in hardware rather than software. A port would replace `gpio_bus.rs` while keeping the rest of the stack intact.

If you're thinking about a port, open an issue first so we can discuss the approach.

## Questions?

Open an issue — happy to help.
