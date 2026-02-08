//! HID over GATT (HOG) implementation for gamepad.
//!
//! Implements standard BLE HID service with Xbox-compatible gamepad layout.

use heapless::Vec;
use nrf_softdevice::ble::gatt_server::{NotifyValueError, SetValueError};
use nrf_softdevice::ble::Connection;

/// HID Report Descriptor for gamepad.
///
/// Layout (16 buttons + 6 axes = 8 bytes):
/// - 16 buttons (2 bytes)
/// - 6 axes: Left X, Left Y, Right X, Right Y, Left Trigger, Right Trigger
#[rustfmt::skip]
pub const HID_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01,        // Usage Page (Generic Desktop)
    0x09, 0x05,        // Usage (Gamepad)
    0xA1, 0x01,        // Collection (Application)

    // Report ID 1
    0x85, 0x01,        //   Report ID (1)

    // 16 Buttons (2 bytes)
    0x05, 0x09,        //   Usage Page (Button)
    0x19, 0x01,        //   Usage Minimum (Button 1)
    0x29, 0x10,        //   Usage Maximum (Button 16)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x01,        //   Logical Maximum (1)
    0x75, 0x01,        //   Report Size (1)
    0x95, 0x10,        //   Report Count (16)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)

    // Axes in Xbox-compatible order (6 bytes, signed -127 to 127)
    // Standard Gamepad: Axis 0=LX, 1=LY, 2=RX, 3=RY, then triggers
    0x05, 0x01,        //   Usage Page (Generic Desktop)
    0x09, 0x30,        //   Usage (X) - Left stick X
    0x09, 0x31,        //   Usage (Y) - Left stick Y
    0x09, 0x33,        //   Usage (Rx) - Right stick X
    0x09, 0x34,        //   Usage (Ry) - Right stick Y
    0x09, 0x32,        //   Usage (Z) - Left trigger
    0x09, 0x35,        //   Usage (Rz) - Right trigger
    0x15, 0x81,        //   Logical Minimum (-127)
    0x25, 0x7F,        //   Logical Maximum (127)
    0x75, 0x08,        //   Report Size (8)
    0x95, 0x06,        //   Report Count (6)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)

    0xC0,              // End Collection
];

/// HID Information characteristic value.
/// bcdHID: 1.11, bCountryCode: 0, Flags: RemoteWake | NormallyConnectable
pub const HID_INFO: [u8; 4] = [0x11, 0x01, 0x00, 0x03];

/// Protocol Mode: Report Protocol (1) vs Boot Protocol (0)
pub const PROTOCOL_MODE_REPORT: u8 = 1;

/// HID Gamepad report (8 bytes of data).
///
/// NOTE: Report ID is NOT included in the data when using BLE HID with
/// Report Reference descriptor - the descriptor identifies the report.
///
/// Layout MUST match HID_REPORT_DESCRIPTOR exactly:
///   - 16 buttons (2 bytes)
///   - 4 stick axes (4 bytes)
///   - 2 trigger axes (2 bytes)
#[derive(Clone, Copy, Default)]
#[repr(C, packed)]
pub struct GamepadReport {
    /// Button states - 16 buttons as a bitmask (2 bytes)
    /// Bits 0-3: A,B,X,Y | Bits 4-7: LB,RB,Back,Start | Bits 8-11: L3,R3,Guide,unused | Bits 12-15: Up,Down,Left,Right
    pub buttons: u16,
    /// Left stick X (-127=left, 0=center, 127=right)
    pub left_x: i8,
    /// Left stick Y (-127=up, 0=center, 127=down)
    pub left_y: i8,
    /// Right stick X (-127=left, 0=center, 127=right)
    pub right_x: i8,
    /// Right stick Y (-127=up, 0=center, 127=down)
    pub right_y: i8,
    /// Left trigger (-127=released, 127=fully pressed)
    pub left_trigger: i8,
    /// Right trigger (-127=released, 127=fully pressed)
    pub right_trigger: i8,
}

impl GamepadReport {
    /// Create a new report with neutral/centered values.
    pub fn new() -> Self {
        Self {
            buttons: 0,
            left_x: 0,           // Center
            left_y: 0,           // Center
            right_x: 0,          // Center
            right_y: 0,          // Center
            left_trigger: -127,  // Released
            right_trigger: -127, // Released
        }
    }

    /// Convert to byte array for BLE transmission.
    /// Layout: buttons(2), axes(6) = 8 bytes
    /// Axes: X, Y, Rx, Ry, Z, Rz
    /// NOTE: Report ID is NOT included - Report Reference descriptor identifies the report.
    pub fn to_bytes(&self) -> [u8; 8] {
        [
            (self.buttons & 0xFF) as u8,        // Buttons low byte
            ((self.buttons >> 8) & 0xFF) as u8, // Buttons high byte
            self.left_y as u8,                  // X axis (swapped - Dreamcast Y → HID X)
            self.left_x as u8,                  // Y axis (swapped - Dreamcast X → HID Y)
            self.right_x as u8,                 // Rx axis (right stick X)
            self.right_y as u8,                 // Ry axis (right stick Y)
            self.left_trigger as u8,            // Z axis (left trigger)
            self.right_trigger as u8,           // Rz axis (right trigger)
        ]
    }
}

/// Button indices in the 16-bit button bitmask.
/// Empirically adjusted for macOS/browser interpretation:
/// - Original bit 9 showed as button 8, so Start needs +1 offset (bit 10)
/// - Original bits 12-15 showed as buttons 13-16, so D-pad needs -1 offset (bits 11-14)
pub mod buttons {
    pub const A: u16 = 1 << 0;
    pub const B: u16 = 1 << 1;
    pub const X: u16 = 1 << 2;
    pub const Y: u16 = 1 << 3;
    pub const LB: u16 = 1 << 4; // Left bumper (unused on Dreamcast)
    pub const RB: u16 = 1 << 5; // Right bumper (unused on Dreamcast)
    pub const LT_BTN: u16 = 1 << 6; // Left trigger button (unused)
    pub const RT_BTN: u16 = 1 << 7; // Right trigger button (unused)
    pub const BACK: u16 = 1 << 9; // Back/Select (unused on Dreamcast)
    pub const START: u16 = 1 << 8; // Start - at bit 10 to show as button 9
    pub const L3: u16 = 1 << 10; // Placeholder
    pub const R3: u16 = 1 << 15; // Placeholder (moved to end)
    pub const DPAD_UP: u16 = 1 << 11; // At bit 11 to show as button 12
    pub const DPAD_DOWN: u16 = 1 << 12; // At bit 12 to show as button 13
    pub const DPAD_LEFT: u16 = 1 << 13; // At bit 13 to show as button 14
    pub const DPAD_RIGHT: u16 = 1 << 14; // At bit 14 to show as button 15
}

// GATT Service definitions using nrf-softdevice macros

/// HID Service (UUID 0x1812)
/// Security: JustWorks (encrypted, unauthenticated) - required by HOGP spec
#[nrf_softdevice::gatt_service(uuid = "1812")]
pub struct HidService {
    /// HID Information (UUID 0x2A4A) - Read only
    /// Value: [bcdHID_lo, bcdHID_hi, bCountryCode, flags]
    #[characteristic(uuid = "2A4A", read, security = "JustWorks")]
    pub hid_info: [u8; 4],

    /// Report Map (UUID 0x2A4B) - Read only, contains HID descriptor
    #[characteristic(uuid = "2A4B", read, security = "JustWorks")]
    pub report_map: Vec<u8, 128>,

    /// HID Report (UUID 0x2A4D) - Read, Notify (Input Report)
    /// Report Reference descriptor (0x2908): [Report ID=1, Report Type=Input(0x01)]
    #[characteristic(
        uuid = "2A4D",
        read,
        notify,
        security = "JustWorks",
        descriptor(uuid = "2908", security = "JustWorks", value = "[0x01, 0x01]")
    )]
    pub report: [u8; 8],

    /// HID Control Point (UUID 0x2A4C) - Write without response
    /// Used by host to signal suspend (0x00) or exit suspend (0x01)
    #[characteristic(uuid = "2A4C", write_without_response, security = "JustWorks")]
    pub control_point: u8,

    /// Protocol Mode (UUID 0x2A4E) - Read, Write Without Response
    /// 0 = Boot Protocol, 1 = Report Protocol (default)
    #[characteristic(uuid = "2A4E", read, write_without_response, security = "JustWorks")]
    pub protocol_mode: u8,
}

/// Device Information Service (UUID 0x180A)
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
#[nrf_softdevice::gatt_service(uuid = "180F")]
pub struct BatteryService {
    /// Battery Level (UUID 0x2A19) - 0-100%
    #[characteristic(uuid = "2A19", read, notify)]
    pub battery_level: u8,
}

/// Combined GATT server with all services.
#[nrf_softdevice::gatt_server]
pub struct GamepadServer {
    pub hid: HidService,
    pub device_info: DeviceInfoService,
    pub battery: BatteryService,
}

impl GamepadServer {
    /// Initialize the server with default values.
    pub fn init(&self) -> Result<(), SetValueError> {
        // Set HID Information
        self.hid.hid_info_set(&HID_INFO)?;

        // Set Report Map (HID descriptor)
        let mut report_map: Vec<u8, 128> = Vec::new();
        report_map.extend_from_slice(HID_REPORT_DESCRIPTOR).ok();
        self.hid.report_map_set(&report_map)?;

        // Set Protocol Mode to Report Protocol
        self.hid.protocol_mode_set(&PROTOCOL_MODE_REPORT)?;

        // Set initial report (neutral state)
        let initial_report = GamepadReport::new();
        self.hid.report_set(&initial_report.to_bytes())?;

        // Set Device Information
        let mut manufacturer: Vec<u8, 32> = Vec::new();
        manufacturer.extend_from_slice(b"Dreamcast").ok();
        self.device_info.manufacturer_set(&manufacturer)?;

        let mut model: Vec<u8, 32> = Vec::new();
        model.extend_from_slice(b"Controller").ok();
        self.device_info.model_number_set(&model)?;

        // PnP ID: Vendor ID Source (0x02 = USB), Vendor ID, Product ID, Version
        // Using generic values - could be customized
        let pnp_id: [u8; 7] = [
            0x02, // Vendor ID Source (USB)
            0x5E, 0x04, // Vendor ID (Microsoft, for Xbox compat) - little endian
            0x8E, 0x02, // Product ID (Xbox controller) - little endian
            0x00, 0x01, // Version 1.0
        ];
        self.device_info.pnp_id_set(&pnp_id)?;

        // Set battery level to 100% (we don't actually measure this)
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
