// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Bluetooth LE adapter for Dreamcast controllers.
//!
//! Speaks the Dreamcast Maple Bus protocol over GPIO and presents controller
//! input as an Xbox One S BLE HID gamepad. Built on Embassy async with the
//! Nordic `SoftDevice` S140 BLE stack.

#![no_std]

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

/// RTT print macro — compiles to nothing when the `rtt` feature is disabled.
///
/// Formats into a stack buffer FIRST, then hands the finished string to RTT:
/// `rprintln!` runs the entire `format_args` rendering inside rtt-target's
/// critical section, and a ~120-char POLLTIME line with ten integer
/// conversions is a 50-200µs interrupt blackout. At 2 log lines/sec that
/// produced stochastic SoftDevice assertion panics (~1/min) in every
/// instrumented build of 2026-06-10/11 — see poll_timing's module docs for
/// the full post-mortem. With pre-formatting, the critical section shrinks
/// to a memcpy of the rendered bytes. Lines over 256 chars are truncated.
#[cfg(feature = "rtt")]
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut _s: heapless::String<256> = heapless::String::new();
        let _ = core::write!(_s, $($arg)*);
        rtt_target::rprintln!("{}", _s.as_str());
    }};
}

/// RTT print macro — compiles to nothing when the `rtt` feature is disabled.
#[cfg(not(feature = "rtt"))]
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{}};
}

/// RTT init — compiles to nothing when the `rtt` feature is disabled.
#[cfg(feature = "rtt")]
#[macro_export]
macro_rules! log_init {
    () => {
        rtt_target::rtt_init_print!()
    };
}

/// RTT init — compiles to nothing when the `rtt` feature is disabled.
#[cfg(not(feature = "rtt"))]
#[macro_export]
macro_rules! log_init {
    () => {{}};
}

pub mod ble;
pub mod board;
pub mod button;
pub mod maple;
pub mod panic_handler;
#[cfg(feature = "poll-timing")]
pub mod poll_timing;
pub mod vmu;

/// BLE HID notification interval (~125Hz, matches Xbox One S).
pub const NOTIFY_INTERVAL_MS: u64 = 8;

/// Delay before sending the first HID notify, giving the host time to
/// finish service discovery and write the CCCD that subscribes to
/// notifications. Reports sent before subscription return an error from
/// `report_notify` and count toward `MAX_NOTIFY_FAILURES` — too short and
/// we'll disconnect a slow-subscribing host.
///
/// Original value 5000 ms (commit 1d66d2c) bundled pairing time too, but
/// pairing is now handled separately in `handle_connection` (~600 ms of
/// explicit waits) before the notify task starts, so this only needs to
/// cover service discovery + CCCD write, which macOS typically completes
/// in well under 1 s. 1500 ms keeps a comfortable margin.
pub const SERVICE_DISCOVERY_DELAY_MS: u64 = 1500;

/// Max consecutive BLE notify failures before disconnecting.
pub const MAX_NOTIFY_FAILURES: u8 = 10;

/// Timeout before entering sleep when disconnected (ms).
pub const SLEEP_TIMEOUT_MS: u64 = 60_000;

/// Shared controller state between maple and BLE tasks.
pub static CONTROLLER_STATE: Signal<CriticalSectionRawMutex, maple::ControllerState> =
    Signal::new();

/// Signal to trigger sync/pairing mode (clears bonds).
pub static SYNC_MODE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Signal to switch active BLE profile and reset. Carries the new `ProfileId`.
pub static PROFILE_CHANGE: Signal<CriticalSectionRawMutex, ble::ProfileId> = Signal::new();

/// Set by the button task on a 10-second hold to request a graceful System Off.
/// The main task picks this up, writes a "BYE" splash to the VMU, briefly
/// holds, then enters System Off. Avoids sleeping mid-write so the goodbye
/// frame actually lands on the LCD.
pub static GOODBYE_PENDING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Set by the BLE task on the rising edge of the Guide chord (L+R+Start held).
/// The main poll loop picks this up and briefly flashes a "home" glyph on the
/// VMU LCD, then resumes normal content. Purely cosmetic and strictly
/// best-effort: a single non-blocking atomic store on the BLE side, and the
/// main loop is free to drop it (goodbye in flight, frame CRC-collision) — it
/// must never perturb Maple bus timing or the controller poll.
pub static GUIDE_GLYPH_PENDING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Signaled by the button task on any short sync-button press. The BLE task
/// uses this as the explicit "wake from silent reconnect-wait" trigger,
/// matching how Xbox / PlayStation controllers use their dedicated wake
/// buttons. Without this signal, the BLE task stays silent after the initial
/// reconnect window so a sleeping host isn't woken by ongoing advertising.
pub static WAKE_REQUEST: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Battery level percentage (0-100) for BLE reporting.
/// Signals 0xFF when charging (tells BLE task to report "charging" state).
#[cfg(feature = "board-xiao")]
pub static BATTERY_LEVEL: Signal<CriticalSectionRawMutex, u8> = Signal::new();
