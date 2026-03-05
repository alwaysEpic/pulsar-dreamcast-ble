// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Bluetooth Low Energy module for Dreamcast controller adapter.
//!
//! Uses nRF `SoftDevice` S140 for BLE peripheral functionality.
//! Implements HID over GATT (HOG) for standard gamepad support.

pub mod flash_bond;
pub mod hid;
pub mod security;
pub mod softdevice;
pub mod task;

pub use hid::GamepadServer;
pub use security::Bonder;
pub use softdevice::{
    advertise, get_connection_state, init_softdevice, set_connection_state, set_name_mode,
    AdvertiseMode, ConnectionState,
};
