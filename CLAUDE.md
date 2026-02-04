# Embedded Dreamcast

Bluetooth LE adapter for Dreamcast controllers -- Rust embedded on nRF52.

This project uses the shared RALPH workspace at [ralph/](ralph/).
See [ralph/CLAUDE.md](ralph/CLAUDE.md) for the full agent and template reference.

---

## Project Context

**Stack:**
- Rust (embedded-hal, nRF HAL)
- Target: nRF52840 (ARM Cortex-M4)
- Dreamcast controller protocol
- Bluetooth LE (SoftDevice S140)

**Architecture notes:**
- State machine pattern for controller lifecycle
- Bus abstraction for SPI/UART to Dreamcast controller
- BLE GATT service for host communication

**Conventions:**
- Use `cargo embed` for flashing / debugging
- Tests: `cargo test --target x86_64-unknown-linux-gnu` (host tests only, no on-target tests yet)
- Clippy: `cargo clippy -- -W clippy::all -W clippy::pedantic`
