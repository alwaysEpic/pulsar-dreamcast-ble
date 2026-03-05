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

pub mod ble;
pub mod board;
pub mod button;
pub mod maple;

/// BLE HID notification interval (~125Hz, matches Xbox One S).
pub const NOTIFY_INTERVAL_MS: u64 = 8;

/// Delay for BLE client to discover services and subscribe (ms).
pub const SERVICE_DISCOVERY_DELAY_MS: u64 = 5000;

/// Max consecutive BLE notify failures before disconnecting.
pub const MAX_NOTIFY_FAILURES: u8 = 10;

/// Timeout before entering sleep when disconnected (ms).
pub const SLEEP_TIMEOUT_MS: u64 = 60_000;

/// Shared controller state between maple and BLE tasks.
pub static CONTROLLER_STATE: Signal<CriticalSectionRawMutex, maple::ControllerState> =
    Signal::new();

/// Signal to trigger sync/pairing mode (clears bonds).
pub static SYNC_MODE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Signal to toggle device name and reset. Carries new `is_dreamcast` value.
pub static NAME_TOGGLE: Signal<CriticalSectionRawMutex, bool> = Signal::new();

/// Battery level percentage (0-100) for BLE reporting.
/// Signals 0xFF when charging (tells BLE task to report "charging" state).
#[cfg(feature = "board-xiao")]
pub static BATTERY_LEVEL: Signal<CriticalSectionRawMutex, u8> = Signal::new();
