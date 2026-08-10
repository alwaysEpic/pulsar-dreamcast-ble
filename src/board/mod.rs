// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Board-specific pin mappings and peripherals.
//!
//! # The board contract
//!
//! `main.rs` is board-agnostic: it contains **no `#[cfg(feature = "board-*")]`
//! blocks**. Every board module (`dk`, `xiao`, `pulsarv1`, …) exports the *same*
//! surface, and capabilities a board lacks are implemented as no-ops / `None` /
//! `SUPPORTS_SLEEP = false`. Selecting a board is done purely by the `board-*`
//! feature, which aliases one module to `board::*`.
//!
//! A conforming board module MUST export:
//!
//! ## Maple bus
//! - `pub const PIN_A_BIT: u32` / `pub const PIN_B_BIT: u32`
//!   — SDCKA/SDCKB bit positions in the P0 GPIO register (both pins MUST be P0).
//!
//! ## Capabilities
//! - `pub const SUPPORTS_SLEEP: bool` — whether `enter_sleep()` actually powers
//!   down (true) or is a WFI-halt fallback (false, dev boards).
//! - `pub const HAS_USB_PASSTHROUGH: bool` — whether external 5 V reaches the
//!   controller rail directly (a Schottky USB passthrough), so `main.rs` can
//!   leave `rail_off()` while plugged in. False where the rail is always
//!   locally generated; `main.rs` then never consults `is_externally_powered()`
//!   for rail decisions, and `rail_on`/`rail_off` stay pure no-ops.
//!
//! ## Lifecycle
//! - `pub fn configure_embassy(config: &mut embassy_nrf::config::Config)` —
//!   board-specific Embassy init tweaks (e.g. `dcdc.reg1`), applied before
//!   `embassy_nrf::init`. No-op where not applicable.
//! - `pub unsafe fn early_init()` — pre-Embassy silicon housekeeping (pin
//!   disconnect, QSPI deep-power-down). No-op where not applicable.
//! - `pub fn init(p: embassy_nrf::Peripherals) -> BoardPins` — consume the HAL
//!   peripherals, grab whatever pins/peripherals the board needs, return the
//!   handles `main.rs` uses. `main.rs` never names an individual pin.
//! - `pub unsafe fn enter_sleep() -> !` — deep sleep / power-off (or WFI halt).
//!
//! ## Handles returned in [`BoardPins`]
//! - `sdcka`, `sdckb`: `Flex<'static>` Maple lines.
//! - `sync_button`: `Input<'static>`, `sync_led`: `Output<'static>`.
//! - `status`: `StatusIndicator` — logical LED/lighting state.
//! - `power`: `Power` — 5 V rail, external-power detect, and battery gauge.
//! - `rumble`: `Rumble` — `set(intensity: u8)` drives a vibration motor (no-op where absent).
//!
//! ## `StatusIndicator` (logical, not physical — RGB / discrete / WS2812)
//! - `async fn startup(&mut self)` · `fn searching(&mut self)`
//! - `fn connected(&mut self)` · `fn off(&mut self)`
//! - `fn tx_activity_on(&mut self)` · `fn tx_activity_off(&mut self)`
//!
//! ## `Power` (rail + gauge; no-op / `None` where a board has neither)
//! - `fn rail_on(&mut self)` · `fn rail_off(&mut self)`
//! - `fn prepare_for_sleep(&mut self)` — power the 5 V rail down before
//!   `enter_sleep()` so it can't drain the battery in System Off. `main.rs`
//!   calls it from its `sleep_now` helper ahead of every `enter_sleep()`.
//!   No-op where a board has no rail, or (XIAO) handles rail-off inside
//!   `enter_sleep` itself.
//! - `fn is_externally_powered(&self) -> bool`
//! - `async fn battery(&mut self) -> Option<BatteryStatus>` — `None` = no gauge.
//!
//! Everything is compile-time monomorphized through the one selected module, so
//! the uniform API is **zero-cost** — no trait objects, no dynamic dispatch.

#[cfg(feature = "board-dk")]
mod dk;
#[cfg(feature = "board-pulsarv1")]
mod ip5306;
#[cfg(feature = "board-pulsarv1")]
mod pulsarv1;
#[cfg(feature = "board-pulsarv1")]
mod ws2812;
#[cfg(feature = "board-xiao")]
mod xiao;

// Shared silicon + onboard-RGB status for boards built on the XIAO nRF52840
// module (xiao, pulsarv1). Private — used by those modules via `super::`.
#[cfg(any(feature = "board-xiao", feature = "board-pulsarv1"))]
mod xiao_common;

#[cfg(feature = "board-dk")]
pub use dk::*;
#[cfg(feature = "board-pulsarv1")]
pub use pulsarv1::*;
#[cfg(feature = "board-xiao")]
pub use xiao::*;

/// Uniform battery snapshot returned by a board's [`Power`] subsystem.
///
/// Boards without a fuel gauge return `None` from `battery()` rather than a
/// synthetic value, so `main.rs` can treat "no battery" and "battery present"
/// uniformly (the reporting/low-cutoff logic simply skips when `None`).
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // fields consumed per-board; not all boards read all fields
pub struct BatteryStatus {
    /// Battery terminal voltage in millivolts.
    pub millivolts: u32,
    /// State-of-charge estimate, 0–100 %.
    pub percent: u8,
    /// True while charging (report as full / freeze discharge tracking).
    pub charging: bool,
}
