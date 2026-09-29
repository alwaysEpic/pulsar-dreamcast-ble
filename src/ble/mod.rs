// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Bluetooth Low Energy module for Dreamcast controller adapter.
//!
//! Uses nRF `SoftDevice` S140 for BLE peripheral functionality.
//! Implements HID over GATT (HOG) for standard gamepad support.

pub mod config;
pub mod flash_bond;
pub mod hid;
pub mod host_lcd;
// the host side of VMU storage — READ in, DATA out.
//
// Not behind `spim-capture`, although only a board with the capture can serve
// a read: the GATT table is one table for every board and every personality
// (see `hid::HostService`), and the `gatt_service` macro does not carry a
// `cfg` on a characteristic through anyway. A board that cannot read answers
// `NO_VMU`, which is a better thing for a dongle to hear than silence.
pub mod host_vmu;
pub mod prefs;
pub mod profile;
pub mod security;
pub mod softdevice;
pub mod task;

pub use config::ConfigServer;
pub use hid::GamepadServer;
pub use profile::{Profile, ProfileId, PROFILE_GENERIC, PROFILE_XBOX};
pub use security::Bonder;
pub use softdevice::{
    advertise, get_connection_state, init_config_softdevice, init_softdevice, set_connection_state,
    set_profile, AdvertiseMode, ConnectionState,
};
