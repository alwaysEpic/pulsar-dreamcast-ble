// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! HID over GATT (HOG) implementation for gamepad.
//!
//! Implements Xbox One S BLE HID format (Model 1708, PID `0x02E0`).
//! Pure report types re-exported from `maple_protocol::xbox_hid`.

#![allow(clippy::redundant_else)] // Macro-generated code
#![allow(clippy::missing_errors_doc)] // Internal API
#![allow(clippy::trivially_copy_pass_by_ref)] // Macro-generated _set methods
#![allow(clippy::unnecessary_semicolon)] // Macro-generated code
#![allow(dead_code)] // Macro-generated event enum fields

#[allow(unused_imports)] // Re-exports for external consumers
pub use maple_protocol::xbox_hid::{
    buttons, hat, GamepadReport, HID_REPORT_DESCRIPTOR_EXT, HID_REPORT_DESCRIPTOR_STD,
};

use heapless::Vec;
use nrf_softdevice::ble::gatt_server::{NotifyValueError, SetValueError};
use nrf_softdevice::ble::Connection;

/// HID Information characteristic value.
/// bcdHID: 1.11, bCountryCode: 0, Flags: `RemoteWake` | `NormallyConnectable`
pub const HID_INFO: [u8; 4] = [0x11, 0x01, 0x00, 0x03];

/// Protocol Mode: Report Protocol (1) vs Boot Protocol (0)
pub const PROTOCOL_MODE_REPORT: u8 = 1;

// GATT Service definitions using nrf-softdevice macros

/// HID Service (UUID 0x1812)
/// Security: `JustWorks` (encrypted, unauthenticated) - required by HOGP spec
#[allow(dead_code)] // Macro-generated fields
#[nrf_softdevice::gatt_service(uuid = "1812")]
pub struct HidService {
    /// HID Information (UUID 0x2A4A) - Read only
    /// Value: [bcdHID_lo, bcdHID_hi, bCountryCode, flags]
    #[characteristic(uuid = "2A4A", read, security = "JustWorks")]
    pub hid_info: [u8; 4],

    /// Report Map (UUID 0x2A4B) - Read only, contains HID descriptor
    #[characteristic(uuid = "2A4B", read, security = "JustWorks")]
    pub report_map: Vec<u8, 512>,

    /// HID Report - Input (UUID 0x2A4D), Report ID 1
    /// Main gamepad state (16 bytes)
    #[characteristic(
        uuid = "2A4D",
        read,
        notify,
        security = "JustWorks",
        descriptor(uuid = "2908", security = "JustWorks", value = "[0x01, 0x01]")
    )]
    pub report: [u8; 16],

    /// HID Control Point (UUID 0x2A4C) - Write without response
    #[characteristic(uuid = "2A4C", write_without_response, security = "JustWorks")]
    pub control_point: u8,

    /// Protocol Mode (UUID 0x2A4E) - Read, Write Without Response
    #[characteristic(uuid = "2A4E", read, write_without_response, security = "JustWorks")]
    pub protocol_mode: u8,
}

/// Device Information Service (UUID 0x180A)
#[allow(dead_code)] // Macro-generated fields
#[nrf_softdevice::gatt_service(uuid = "180A")]
pub struct DeviceInfoService {
    /// Manufacturer Name (UUID 0x2A29)
    #[characteristic(uuid = "2A29", read)]
    pub manufacturer: Vec<u8, 32>,

    /// Model Number (UUID 0x2A24)
    #[characteristic(uuid = "2A24", read)]
    pub model_number: Vec<u8, 32>,

    /// PnP ID (UUID 0x2A50) - Vendor ID, Product ID, Version
    #[characteristic(uuid = "2A50", read)]
    pub pnp_id: [u8; 7],
}

/// Battery Service (UUID 0x180F)
#[allow(dead_code)] // Macro-generated fields
#[nrf_softdevice::gatt_service(uuid = "180F")]
pub struct BatteryService {
    /// Battery Level (UUID 0x2A19) - 0-100%
    #[characteristic(uuid = "2A19", read, notify)]
    pub battery_level: u8,
}

/// Combined GATT server with all services.
#[allow(dead_code)] // Macro-generated fields
#[nrf_softdevice::gatt_server]
pub struct GamepadServer {
    pub hid: HidService,
    pub device_info: DeviceInfoService,
    pub battery: BatteryService,
}

impl GamepadServer {
    /// Initialize the server from the active `Profile`.
    pub fn init(&self, profile: &crate::ble::profile::Profile) -> Result<(), SetValueError> {
        self.hid.hid_info_set(&HID_INFO)?;

        let mut report_map: Vec<u8, 512> = Vec::new();
        let _ = report_map.extend_from_slice(profile.hid_descriptor).ok();
        self.hid.report_map_set(&report_map)?;

        self.hid.protocol_mode_set(&PROTOCOL_MODE_REPORT)?;

        // Initial report: sticks centered (32768), everything else zero
        let initial_report = GamepadReport::new();
        self.hid.report_set(&initial_report.to_bytes())?;

        // Device Information from active profile
        let mut manufacturer: Vec<u8, 32> = Vec::new();
        let _ = manufacturer.extend_from_slice(profile.manufacturer).ok();
        self.device_info.manufacturer_set(&manufacturer)?;

        let mut model: Vec<u8, 32> = Vec::new();
        let _ = model.extend_from_slice(profile.model).ok();
        self.device_info.model_number_set(&model)?;

        let vid = profile.vid.to_le_bytes();
        let pid = profile.pid.to_le_bytes();
        let ver = profile.version.to_le_bytes();
        let pnp_id: [u8; 7] = [
            0x02, // Vendor ID Source (USB-IF)
            vid[0], vid[1], pid[0], pid[1], ver[0], ver[1],
        ];
        self.device_info.pnp_id_set(&pnp_id)?;

        self.battery.battery_level_set(&100)?;

        Ok(())
    }

    /// Send a gamepad report notification using the active profile's serializer.
    ///
    /// Wire-level dedup: if the serialized 16-byte payload is byte-identical
    /// to the previous one we sent, skip the notify. This catches anything
    /// the input-side `state_changed` filter missed and keeps a sleeping
    /// host from being woken by a stream of no-op reports.
    pub fn send_report(
        &self,
        conn: &Connection,
        report: &GamepadReport,
    ) -> Result<(), NotifyValueError> {
        let profile = crate::ble::softdevice::get_profile();
        let bytes = (profile.serialize_report)(*report);

        let mut skip = false;
        LAST_REPORT.lock(|cell| {
            let cached = cell.get();
            if cached.is_some_and(|c| c == bytes) {
                skip = true;
            } else {
                cell.set(Some(bytes));
            }
        });
        if skip {
            return Ok(());
        }

        // Debug-only: stamp a 7-bit sequence counter into byte 15 bits 1-7 (HID
        // padding; bit 0 = Consumer Record, left untouched) so a host capture can
        // detect reports dropped between here and the host. Injected *after* dedup,
        // so the dedup/send-on-change behavior under test is unchanged.
        #[cfg(feature = "seq-counter")]
        let bytes = {
            let mut b = bytes;
            let n = SEQ_COUNTER.lock(|c| {
                let v = c.get();
                c.set(v.wrapping_add(1));
                v
            });
            b[15] = (b[15] & 0x01) | ((n & 0x7F) << 1);
            b
        };

        self.hid.report_notify(conn, &bytes)
    }
}

/// Cache of the most recently sent 16-byte HID report payload, used by
/// `send_report` for wire-level dedup. `None` until the first send.
static LAST_REPORT: embassy_sync::blocking_mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    core::cell::Cell<Option<[u8; 16]>>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::Cell::new(None));

/// Reset the wire-level dedup cache. Call when the BLE connection drops so
/// the first report on the next connection is always sent (the new host has
/// no prior state to dedup against).
pub fn reset_report_cache() {
    LAST_REPORT.lock(|cell| cell.set(None));
}

/// Debug sequence counter stamped into report byte 15 (bits 1-7) when the
/// `seq-counter` feature is on, so a host capture can detect dropped reports.
#[cfg(feature = "seq-counter")]
static SEQ_COUNTER: embassy_sync::blocking_mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    core::cell::Cell<u8>,
> = embassy_sync::blocking_mutex::Mutex::new(core::cell::Cell::new(0));
