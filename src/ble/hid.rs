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
pub use maple_protocol::xbox_hid::{buttons, hat, GamepadReport};

use heapless::Vec;
use nrf_softdevice::ble::gatt_server::{NotifyValueError, SetValueError};
use nrf_softdevice::ble::Connection;

/// HID Report Descriptor for Xbox One S BLE controller format.
///
/// Uses xpadneo-patched usage convention for broad HID parser compatibility:
///   - Left stick:  X (0x30) / Y (0x31)    — Generic Desktop
///   - Right stick:  Rx (0x33) / Ry (0x34)  — Generic Desktop
///   - Triggers:    Z (0x32) / Rz (0x35)    — Generic Desktop
///
/// Report ID 0x01 - Main input (16 bytes):
///   Bytes 0-1:   Left Stick X   (uint16, 0-65535, center=32768)
///   Bytes 2-3:   Left Stick Y   (uint16, 0-65535, center=32768)
///   Bytes 4-5:   Right Stick X  (uint16, 0-65535, center=32768)
///   Bytes 6-7:   Right Stick Y  (uint16, 0-65535, center=32768)
///   Bytes 8-9:   Left Trigger   (10-bit 0-1023 + 6-bit padding)
///   Bytes 10-11: Right Trigger  (10-bit 0-1023 + 6-bit padding)
///   Byte 12:     Hat Switch     (4-bit 1-8, 0=null + 4-bit padding)
///   Bytes 13-14: Buttons 1-15   (15 bits + 1-bit padding)
///   Byte 15:     AC Back        (1 bit + 7-bit padding)
///
/// Report ID 0x02 - Xbox/Guide button (1 byte):
///   Byte 0: AC Home (1 bit + 7-bit padding)
///
/// Report ID 0x03 - Force feedback output (9 bytes, host→device)
#[rustfmt::skip]
pub const HID_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01,        // Usage Page (Generic Desktop)
    0x09, 0x05,        // Usage (Gamepad)
    0xA1, 0x01,        // Collection (Application)

    // === Report ID 0x01: Main Gamepad Input ===
    0x85, 0x01,        //   Report ID (1)

    // Left Stick (Physical collection, unsigned 16-bit)
    0x09, 0x01,        //   Usage (Pointer)
    0xA1, 0x00,        //   Collection (Physical)
    0x09, 0x30,        //     Usage (X)
    0x09, 0x31,        //     Usage (Y)
    0x15, 0x00,        //     Logical Minimum (0)
    0x27, 0xFF, 0xFF, 0x00, 0x00, //  Logical Maximum (65535)
    0x95, 0x02,        //     Report Count (2)
    0x75, 0x10,        //     Report Size (16)
    0x81, 0x02,        //     Input (Data, Variable, Absolute)
    0xC0,              //   End Collection

    // Right Stick (Physical collection, unsigned 16-bit)
    // Uses Rx/Ry (standard convention, matches xpadneo-patched Xbox descriptor)
    0x09, 0x01,        //   Usage (Pointer)
    0xA1, 0x00,        //   Collection (Physical)
    0x09, 0x33,        //     Usage (Rx)
    0x09, 0x34,        //     Usage (Ry)
    0x15, 0x00,        //     Logical Minimum (0)
    0x27, 0xFF, 0xFF, 0x00, 0x00, //  Logical Maximum (65535)
    0x95, 0x02,        //     Report Count (2)
    0x75, 0x10,        //     Report Size (16)
    0x81, 0x02,        //     Input (Data, Variable, Absolute)
    0xC0,              //   End Collection

    // Left Trigger (Generic Desktop Z, 10-bit + 6 padding)
    // Uses Z/Rz (standard convention, matches xpadneo-patched Xbox descriptor)
    0x05, 0x01,        //   Usage Page (Generic Desktop)
    0x09, 0x32,        //   Usage (Z)
    0x15, 0x00,        //   Logical Minimum (0)
    0x26, 0xFF, 0x03,  //   Logical Maximum (1023)
    0x95, 0x01,        //   Report Count (1)
    0x75, 0x0A,        //   Report Size (10)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x00,        //   Logical Maximum (0)
    0x75, 0x06,        //   Report Size (6)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x03,        //   Input (Constant) - padding

    // Right Trigger (Generic Desktop Rz, 10-bit + 6 padding)
    0x09, 0x35,        //   Usage (Rz)
    0x15, 0x00,        //   Logical Minimum (0)
    0x26, 0xFF, 0x03,  //   Logical Maximum (1023)
    0x95, 0x01,        //   Report Count (1)
    0x75, 0x0A,        //   Report Size (10)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x00,        //   Logical Maximum (0)
    0x75, 0x06,        //   Report Size (6)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x03,        //   Input (Constant) - padding

    // Hat Switch / D-pad (4-bit value + 4-bit padding)
    0x05, 0x01,        //   Usage Page (Generic Desktop)
    0x09, 0x39,        //   Usage (Hat Switch)
    0x15, 0x01,        //   Logical Minimum (1)
    0x25, 0x08,        //   Logical Maximum (8)
    0x35, 0x00,        //   Physical Minimum (0)
    0x46, 0x3B, 0x01,  //   Physical Maximum (315)
    0x66, 0x14, 0x00,  //   Unit (Degrees)
    0x75, 0x04,        //   Report Size (4)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x42,        //   Input (Data, Variable, Absolute, Null State)
    0x75, 0x04,        //   Report Size (4)
    0x95, 0x01,        //   Report Count (1)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x00,        //   Logical Maximum (0)
    0x35, 0x00,        //   Physical Minimum (0)
    0x45, 0x00,        //   Physical Maximum (0)
    0x65, 0x00,        //   Unit (None)
    0x81, 0x03,        //   Input (Constant) - padding

    // Buttons 1-15
    0x05, 0x09,        //   Usage Page (Button)
    0x19, 0x01,        //   Usage Minimum (Button 1)
    0x29, 0x0F,        //   Usage Maximum (Button 15)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x01,        //   Logical Maximum (1)
    0x75, 0x01,        //   Report Size (1)
    0x95, 0x0F,        //   Report Count (15)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)
    // 1-bit padding
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x00,        //   Logical Maximum (0)
    0x75, 0x01,        //   Report Size (1)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x03,        //   Input (Constant) - padding

    // AC Back (Consumer Control, 1-bit + 7-bit padding)
    0x05, 0x0C,        //   Usage Page (Consumer)
    0x0A, 0x24, 0x02,  //   Usage (AC Back)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x01,        //   Logical Maximum (1)
    0x95, 0x01,        //   Report Count (1)
    0x75, 0x01,        //   Report Size (1)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x00,        //   Logical Maximum (0)
    0x75, 0x07,        //   Report Size (7)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x03,        //   Input (Constant) - padding

    // === Report ID 0x02: Xbox/Guide Button ===
    0x05, 0x0C,        //   Usage Page (Consumer)
    0x09, 0x01,        //   Usage (Consumer Control)
    0x85, 0x02,        //   Report ID (2)
    0xA1, 0x01,        //   Collection (Application)
    0x05, 0x0C,        //     Usage Page (Consumer)
    0x0A, 0x23, 0x02,  //     Usage (AC Home)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x95, 0x01,        //     Report Count (1)
    0x75, 0x01,        //     Report Size (1)
    0x81, 0x02,        //     Input (Data, Variable, Absolute)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x00,        //     Logical Maximum (0)
    0x75, 0x07,        //     Report Size (7)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x03,        //     Input (Constant) - padding
    0xC0,              //   End Collection

    // === Report ID 0x03: Rumble Output ===
    0x05, 0x0F,        //   Usage Page (Physical Interface Device)
    0x09, 0x21,        //   Usage (Set Effect Report)
    0x85, 0x03,        //   Report ID (3)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x97,        //     Usage (DC Enable Actuators)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x04,        //     Report Size (4)
    0x95, 0x01,        //     Report Count (1)
    0x91, 0x02,        //     Output (Data, Variable, Absolute)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x00,        //     Logical Maximum (0)
    0x75, 0x04,        //     Report Size (4)
    0x95, 0x01,        //     Report Count (1)
    0x91, 0x03,        //     Output (Constant) - padding
    0x09, 0x70,        //     Usage (Magnitude)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x64,        //     Logical Maximum (100)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x04,        //     Report Count (4)
    0x91, 0x02,        //     Output (Data, Variable, Absolute)
    0x09, 0x50,        //     Usage (Duration)
    0x66, 0x01, 0x10,  //     Unit (SI Linear: Time)
    0x55, 0x0E,        //     Unit Exponent (-2)
    0x15, 0x00,        //     Logical Minimum (0)
    0x26, 0xFF, 0x00,  //     Logical Maximum (255)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x01,        //     Report Count (1)
    0x91, 0x02,        //     Output (Data, Variable, Absolute)
    0x09, 0xA7,        //     Usage (Start Delay)
    0x15, 0x00,        //     Logical Minimum (0)
    0x26, 0xFF, 0x00,  //     Logical Maximum (255)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x01,        //     Report Count (1)
    0x91, 0x02,        //     Output (Data, Variable, Absolute)
    0x65, 0x00,        //     Unit (None)
    0x55, 0x00,        //     Unit Exponent (0)
    0x09, 0x7C,        //     Usage (Loop Count)
    0x15, 0x00,        //     Logical Minimum (0)
    0x26, 0xFF, 0x00,  //     Logical Maximum (255)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x01,        //     Report Count (1)
    0x91, 0x02,        //     Output (Data, Variable, Absolute)
    0xC0,              //   End Collection

    // === Report ID 0x04: Battery ===
    0x05, 0x06,        //   Usage Page (Generic Device Controls)
    0x09, 0x20,        //   Usage (Battery Strength)
    0x85, 0x04,        //   Report ID (4)
    0x15, 0x00,        //   Logical Minimum (0)
    0x26, 0xFF, 0x00,  //   Logical Maximum (255)
    0x75, 0x08,        //   Report Size (8)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)

    0xC0,              // End Collection
];

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
    /// Initialize the server with default values.
    pub fn init(&self) -> Result<(), SetValueError> {
        self.hid.hid_info_set(&HID_INFO)?;

        let mut report_map: Vec<u8, 512> = Vec::new();
        let _ = report_map.extend_from_slice(HID_REPORT_DESCRIPTOR).ok();
        self.hid.report_map_set(&report_map)?;

        self.hid.protocol_mode_set(&PROTOCOL_MODE_REPORT)?;

        // Initial report: sticks centered (32768), everything else zero
        let initial_report = GamepadReport::new();
        self.hid.report_set(&initial_report.to_bytes())?;

        // Device Information - match Xbox One S
        let mut manufacturer: Vec<u8, 32> = Vec::new();
        let _ = manufacturer.extend_from_slice(b"Microsoft").ok();
        self.device_info.manufacturer_set(&manufacturer)?;

        let mut model: Vec<u8, 32> = Vec::new();
        let _ = model.extend_from_slice(b"Xbox Wireless Controller").ok();
        self.device_info.model_number_set(&model)?;

        // PnP ID: Xbox One S Controller over BLE
        let pnp_id: [u8; 7] = [
            0x02, // Vendor ID Source (USB-IF)
            0x5E, 0x04, // Vendor ID: 0x045E (Microsoft)
            0xE0, 0x02, // Product ID: 0x02E0 (Xbox One S BLE)
            0x00, 0x01, // Version 1.0
        ];
        self.device_info.pnp_id_set(&pnp_id)?;

        self.battery.battery_level_set(&100)?;

        Ok(())
    }

    /// Send a gamepad report notification.
    pub fn send_report(
        &self,
        conn: &Connection,
        report: &GamepadReport,
    ) -> Result<(), NotifyValueError> {
        self.hid.report_notify(conn, &report.to_bytes())
    }
}
