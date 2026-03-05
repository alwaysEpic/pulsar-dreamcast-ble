// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Dreamcast controller protocol library.
//!
//! Pure logic for Maple Bus packet construction, controller state parsing,
//! and Xbox One S BLE HID report generation. No embedded or hardware
//! dependencies — just `heapless` for `no_std` collections.
//!
//! # Not covered
//!
//! Hardware-specific code lives in the main crate:
//! - `MapleBus` (GPIO register access for Maple Bus signaling)
//! - `MapleHost` (uses `MapleBus` for TX/RX transactions)
//! - `BatteryReader` (SAADC peripheral)
//! - BLE GATT services (nrf-softdevice macros)
//! - Power management (`enter_system_off`, `disable_boost`)

#![no_std]

pub mod controller_state;
pub mod packet;
pub mod xbox_hid;
