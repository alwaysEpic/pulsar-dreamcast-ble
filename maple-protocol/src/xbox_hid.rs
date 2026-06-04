// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Xbox One S BLE HID gamepad report types.
//!
//! Contains the `GamepadReport` struct and byte serialization.
//! GATT service definitions live in the main crate (require nrf-softdevice macros).

/// 10-bit trigger mask.
const TRIGGER_10BIT_MASK: u16 = 0x03FF;

/// Right stick center as little-endian bytes (32768 = 0x8000).
const RIGHT_STICK_CENTER_LE: [u8; 2] = [0x00, 0x80];

/// Xbox BLE stick center value (unsigned 16-bit).
const STICK_CENTER: u16 = 32768;

/// Xbox One S BLE gamepad report (Report ID 0x01, 16 bytes data).
///
/// NOTE: Report ID is NOT included in the byte array -- the Report Reference
/// descriptor on the characteristic identifies this as Report ID 1.
///
/// Byte layout matches real Xbox One S (Model 1708) exactly:
///   Bytes 0-1:   Left Stick X   (uint16 LE, 0-65535, center=32768)
///   Bytes 2-3:   Left Stick Y   (uint16 LE, 0-65535, center=32768)
///   Bytes 4-5:   Right Stick X  (uint16 LE, 0-65535, center=32768)
///   Bytes 6-7:   Right Stick Y  (uint16 LE, 0-65535, center=32768)
///   Bytes 8-9:   Left Trigger   (10-bit LE in low bits, 6 padding in high bits)
///   Bytes 10-11: Right Trigger  (10-bit LE in low bits, 6 padding in high bits)
///   Byte 12:     Hat Switch     (4-bit in low nibble, 4 padding in high nibble)
///   Bytes 13-14: Buttons 1-15   (15 bits, 1-bit padding)
///   Byte 15:     AC Back        (1 bit, 7-bit padding)
#[derive(Clone, Copy)]
pub struct GamepadReport {
    /// Left stick X (0=left, 32768=center, 65535=right)
    pub left_x: u16,
    /// Left stick Y (0=top, 32768=center, 65535=bottom)
    pub left_y: u16,
    /// Left trigger (0=released, 1023=fully pressed)
    pub left_trigger: u16,
    /// Right trigger (0=released, 1023=fully pressed)
    pub right_trigger: u16,
    /// Hat switch / D-pad (0=neutral, 1-8=directions)
    pub hat: u8,
    /// Button bitmask (bits 0-14 = buttons 1-15)
    pub buttons: u16,
}

impl Default for GamepadReport {
    fn default() -> Self {
        Self {
            left_x: STICK_CENTER,
            left_y: STICK_CENTER,
            left_trigger: 0,
            right_trigger: 0,
            hat: hat::NEUTRAL,
            buttons: 0,
        }
    }
}

/// Hat switch values (Xbox One convention: 1-8, 0=neutral/null).
pub mod hat {
    pub const NEUTRAL: u8 = 0;
    pub const NORTH: u8 = 1;
    pub const NORTH_EAST: u8 = 2;
    pub const EAST: u8 = 3;
    pub const SOUTH_EAST: u8 = 4;
    pub const SOUTH: u8 = 5;
    pub const SOUTH_WEST: u8 = 6;
    pub const WEST: u8 = 7;
    pub const NORTH_WEST: u8 = 8;
}

impl GamepadReport {
    /// Create a new report with neutral/centered values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Convert to 16-byte array using the contiguous (xpadneo-patched) button layout.
    ///
    /// Buttons 1-15 are packed as a single contiguous bitfield in bytes 13-14.
    /// Compatible with Steam Input, Linux desktop, Android generic HID parsers.
    ///
    /// Trigger packing: 10 data bits in low bits of `u16`, 6 zero padding in high bits.
    /// Byte 8 = `trigger[7:0]`, Byte 9 = `000000 | trigger[9:8]`
    #[must_use]
    pub fn to_bytes(self) -> [u8; 16] {
        let lx = self.left_x.to_le_bytes();
        let ly = self.left_y.to_le_bytes();
        // Triggers: mask to 10 bits, stored as LE u16 (padding is in high 6 bits)
        let lt = (self.left_trigger & TRIGGER_10BIT_MASK).to_le_bytes();
        let rt = (self.right_trigger & TRIGGER_10BIT_MASK).to_le_bytes();

        #[allow(clippy::cast_possible_truncation)]
        [
            // Left Stick X (bytes 0-1, uint16 LE)
            lx[0],
            lx[1],
            // Left Stick Y (bytes 2-3, uint16 LE)
            ly[0],
            ly[1],
            // Right Stick X (bytes 4-5) - Dreamcast has no right stick, center=32768
            RIGHT_STICK_CENTER_LE[0],
            RIGHT_STICK_CENTER_LE[1],
            // Right Stick Y (bytes 6-7)
            RIGHT_STICK_CENTER_LE[0],
            RIGHT_STICK_CENTER_LE[1],
            // Left Trigger (bytes 8-9, 10-bit + 6 padding)
            lt[0],
            lt[1],
            // Right Trigger (bytes 10-11, 10-bit + 6 padding)
            rt[0],
            rt[1],
            // Hat Switch (byte 12, low nibble = value, high nibble = padding)
            self.hat & 0x0F,
            // Buttons 1-8 (byte 13)
            (self.buttons & 0xFF) as u8,
            // Buttons 9-15 + 1-bit padding (byte 14)
            ((self.buttons >> 8) & 0x7F) as u8,
            // AC Back (byte 15, bit 0) - unused, always 0
            0x00,
        ]
    }

    /// Convert to 16-byte array using the original Microsoft Xbox One S BT Classic
    /// button layout (PID 0x02E0). Buttons 1-10 are placed at specific bit positions
    /// with reserved gaps in between, matching the descriptor `BlueRetro` and other
    /// retro adapters key off via VID/PID.
    ///
    /// Byte 13 (face buttons + bumpers):
    ///   bit 0=A, 1=B, 2=reserved, 3=X, 4=Y, 5=reserved, 6=LB, 7=RB
    /// Byte 14 (system buttons + stick clicks):
    ///   bits 0-1=reserved, 2=View, 3=Menu, 4=reserved (Xbox is on Report 2),
    ///   5=L3, 6=R3, 7=reserved
    ///
    /// Sticks, triggers, hat, and byte 15 are identical to `to_bytes`.
    #[must_use]
    pub fn to_bytes_ms(self) -> [u8; 16] {
        let lx = self.left_x.to_le_bytes();
        let ly = self.left_y.to_le_bytes();
        let lt = (self.left_trigger & TRIGGER_10BIT_MASK).to_le_bytes();
        let rt = (self.right_trigger & TRIGGER_10BIT_MASK).to_le_bytes();

        let b = self.buttons;
        let mut byte13 = 0u8;
        if b & buttons::A != 0 {
            byte13 |= 1 << 0;
        }
        if b & buttons::B != 0 {
            byte13 |= 1 << 1;
        }
        if b & buttons::X != 0 {
            byte13 |= 1 << 3;
        }
        if b & buttons::Y != 0 {
            byte13 |= 1 << 4;
        }
        if b & buttons::LB != 0 {
            byte13 |= 1 << 6;
        }
        if b & buttons::RB != 0 {
            byte13 |= 1 << 7;
        }

        let mut byte14 = 0u8;
        if b & buttons::BACK != 0 {
            byte14 |= 1 << 2;
        }
        if b & buttons::START != 0 {
            byte14 |= 1 << 3;
        }
        if b & buttons::L3 != 0 {
            byte14 |= 1 << 5;
        }
        if b & buttons::R3 != 0 {
            byte14 |= 1 << 6;
        }

        [
            lx[0],
            lx[1],
            ly[0],
            ly[1],
            RIGHT_STICK_CENTER_LE[0],
            RIGHT_STICK_CENTER_LE[1],
            RIGHT_STICK_CENTER_LE[0],
            RIGHT_STICK_CENTER_LE[1],
            lt[0],
            lt[1],
            rt[0],
            rt[1],
            self.hat & 0x0F,
            byte13,
            byte14,
            0x00,
        ]
    }
}

/// Button bit positions in the 15-bit button field.
/// Xbox One S layout (matches HID Button Usage 1-15):
///   Bit 0  = Button 1  = A
///   Bit 1  = Button 2  = B
///   Bit 2  = Button 3  = X
///   Bit 3  = Button 4  = Y
///   Bit 4  = Button 5  = LB (Left Bumper)
///   Bit 5  = Button 6  = RB (Right Bumper)
///   Bit 6  = Button 7  = Back/View
///   Bit 7  = Button 8  = Menu/Start
///   Bit 8  = Button 9  = Left Stick Click (L3)
///   Bit 9  = Button 10 = Right Stick Click (R3)
///   Bits 10-14 = reserved
pub mod buttons {
    pub const A: u16 = 1 << 0;
    pub const B: u16 = 1 << 1;
    pub const X: u16 = 1 << 2;
    pub const Y: u16 = 1 << 3;
    pub const LB: u16 = 1 << 4;
    pub const RB: u16 = 1 << 5;
    pub const BACK: u16 = 1 << 6;
    pub const START: u16 = 1 << 7;
    pub const L3: u16 = 1 << 8;
    pub const R3: u16 = 1 << 9;
}

/// HID Report Descriptor — EXT profile (xpadneo-patched contiguous layout).
///
/// Buttons 1-15 are packed contiguously in bytes 13-14. Compatible with Steam Input,
/// Linux desktop (xpadneo), and Android generic HID. Used by `PROFILE_EXT` (PID 0x0B20).
///
/// Stick/trigger usages follow xpadneo convention:
///   - Left stick:  X (0x30) / Y (0x31)    — Generic Desktop
///   - Right stick: Rx (0x33) / Ry (0x34)  — Generic Desktop
///   - Triggers:    Z (0x32) / Rz (0x35)   — Generic Desktop
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
/// Report ID 0x02 - Xbox/Guide button (1 byte, same Application collection):
///   Byte 0: AC Home (1 bit + 7-bit padding)
///
/// Report ID 0x03 - Force feedback output (9 bytes, host→device)
#[rustfmt::skip]
pub const HID_REPORT_DESCRIPTOR_EXT: &[u8] = &[
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
    0x85, 0x02,        //   Report ID (2)
    0x05, 0x0C,        //   Usage Page (Consumer)
    0x0A, 0x23, 0x02,  //   Usage (AC Home)
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

/// HID Report Descriptor — STD profile (original Microsoft Xbox One S BT Classic).
///
/// Matches the byte layout used by real Xbox One S controllers identifying as
/// PID 0x02E0 (Bluetooth Classic, pre-FW 5.11). Compatible with `BlueRetro` and
/// other retro adapters that key off VID/PID and parse with the legacy layout.
///
/// Differs from `HID_REPORT_DESCRIPTOR_EXT` only in the button section of
/// Report ID 0x01. Buttons 1-10 are placed at specific bit positions with
/// reserved gaps in between, matching `GamepadReport::to_bytes_ms` output:
///
/// Byte 13 (face buttons + bumpers):
///   bit 0=A, 1=B, 2=reserved, 3=X, 4=Y, 5=reserved, 6=LB, 7=RB
/// Byte 14 (system buttons + stick clicks):
///   bits 0-1=reserved, 2=View, 3=Menu, 4=reserved, 5=L3, 6=R3, 7=reserved
/// Byte 15: full padding (no AC Back declaration in legacy layout).
#[rustfmt::skip]
pub const HID_REPORT_DESCRIPTOR_STD: &[u8] = &[
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
    0x05, 0x01,        //   Usage Page (Generic Desktop)
    0x09, 0x32,        //   Usage (Z)
    0x15, 0x00,        //   Logical Minimum (0)
    0x26, 0xFF, 0x03,  //   Logical Maximum (1023)
    0x95, 0x01,        //   Report Count (1)
    0x75, 0x0A,        //   Report Size (10)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)
    0x15, 0x00,
    0x25, 0x00,
    0x75, 0x06,        //   Report Size (6)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x03,        //   Input (Constant) - padding

    // Right Trigger (Generic Desktop Rz, 10-bit + 6 padding)
    0x09, 0x35,        //   Usage (Rz)
    0x15, 0x00,
    0x26, 0xFF, 0x03,  //   Logical Maximum (1023)
    0x95, 0x01,
    0x75, 0x0A,
    0x81, 0x02,
    0x15, 0x00,
    0x25, 0x00,
    0x75, 0x06,
    0x95, 0x01,
    0x81, 0x03,        //   Input (Constant) - padding

    // Hat Switch / D-pad (4-bit value + 4-bit padding)
    0x05, 0x01,        //   Usage Page (Generic Desktop)
    0x09, 0x39,        //   Usage (Hat Switch)
    0x15, 0x01,        //   Logical Minimum (1)
    0x25, 0x08,        //   Logical Maximum (8)
    0x35, 0x00,
    0x46, 0x3B, 0x01,  //   Physical Maximum (315)
    0x66, 0x14, 0x00,  //   Unit (Degrees)
    0x75, 0x04,
    0x95, 0x01,
    0x81, 0x42,        //   Input (Data, Variable, Absolute, Null State)
    0x75, 0x04,
    0x95, 0x01,
    0x15, 0x00,
    0x25, 0x00,
    0x35, 0x00,
    0x45, 0x00,
    0x65, 0x00,
    0x81, 0x03,        //   Input (Constant) - padding

    // Buttons - original Xbox One S BT Classic gappy layout
    0x05, 0x09,        //   Usage Page (Button)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x01,        //   Logical Maximum (1)
    0x75, 0x01,        //   Report Size (1)

    // Buttons 1-2 (A, B) at byte 13 bits 0-1
    0x19, 0x01,        //   Usage Minimum (Button 1)
    0x29, 0x02,        //   Usage Maximum (Button 2)
    0x95, 0x02,        //   Report Count (2)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)
    // 1-bit gap (byte 13 bit 2)
    0x95, 0x01,
    0x81, 0x03,        //   Input (Constant) - padding

    // Buttons 3-4 (X, Y) at byte 13 bits 3-4
    0x19, 0x03,        //   Usage Minimum (Button 3)
    0x29, 0x04,        //   Usage Maximum (Button 4)
    0x95, 0x02,
    0x81, 0x02,
    // 1-bit gap (byte 13 bit 5)
    0x95, 0x01,
    0x81, 0x03,

    // Buttons 5-6 (LB, RB) at byte 13 bits 6-7
    0x19, 0x05,        //   Usage Minimum (Button 5)
    0x29, 0x06,        //   Usage Maximum (Button 6)
    0x95, 0x02,
    0x81, 0x02,

    // 2-bit gap (byte 14 bits 0-1)
    0x95, 0x02,
    0x81, 0x03,

    // Buttons 7-8 (View, Menu) at byte 14 bits 2-3
    0x19, 0x07,        //   Usage Minimum (Button 7)
    0x29, 0x08,        //   Usage Maximum (Button 8)
    0x95, 0x02,
    0x81, 0x02,

    // 1-bit gap (byte 14 bit 4 — reserved, Xbox button is on Report 2)
    0x95, 0x01,
    0x81, 0x03,

    // Buttons 9-10 (L3, R3) at byte 14 bits 5-6
    0x19, 0x09,        //   Usage Minimum (Button 9)
    0x29, 0x0A,        //   Usage Maximum (Button 10)
    0x95, 0x02,
    0x81, 0x02,

    // 1-bit padding (byte 14 bit 7)
    0x95, 0x01,
    0x81, 0x03,

    // Byte 15: full padding (no AC Back in legacy layout)
    0x95, 0x08,
    0x81, 0x03,

    // === Report ID 0x02: Xbox/Guide Button ===
    0x85, 0x02,        //   Report ID (2)
    0x05, 0x0C,        //   Usage Page (Consumer)
    0x0A, 0x23, 0x02,  //   Usage (AC Home)
    0x15, 0x00,
    0x25, 0x01,
    0x95, 0x01,
    0x75, 0x01,
    0x81, 0x02,
    0x15, 0x00,
    0x25, 0x00,
    0x75, 0x07,
    0x95, 0x01,
    0x81, 0x03,        //   Input (Constant) - padding

    // === Report ID 0x03: Rumble Output ===
    0x05, 0x0F,        //   Usage Page (Physical Interface Device)
    0x09, 0x21,        //   Usage (Set Effect Report)
    0x85, 0x03,
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x97,
    0x15, 0x00,
    0x25, 0x01,
    0x75, 0x04,
    0x95, 0x01,
    0x91, 0x02,
    0x15, 0x00,
    0x25, 0x00,
    0x75, 0x04,
    0x95, 0x01,
    0x91, 0x03,
    0x09, 0x70,
    0x15, 0x00,
    0x25, 0x64,
    0x75, 0x08,
    0x95, 0x04,
    0x91, 0x02,
    0x09, 0x50,
    0x66, 0x01, 0x10,
    0x55, 0x0E,
    0x15, 0x00,
    0x26, 0xFF, 0x00,
    0x75, 0x08,
    0x95, 0x01,
    0x91, 0x02,
    0x09, 0xA7,
    0x15, 0x00,
    0x26, 0xFF, 0x00,
    0x75, 0x08,
    0x95, 0x01,
    0x91, 0x02,
    0x65, 0x00,
    0x55, 0x00,
    0x09, 0x7C,
    0x15, 0x00,
    0x26, 0xFF, 0x00,
    0x75, 0x08,
    0x95, 0x01,
    0x91, 0x02,
    0xC0,              //   End Collection

    // === Report ID 0x04: Battery ===
    0x05, 0x06,
    0x09, 0x20,
    0x85, 0x04,
    0x15, 0x00,
    0x26, 0xFF, 0x00,
    0x75, 0x08,
    0x95, 0x01,
    0x81, 0x02,

    0xC0,              // End Collection
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_bytes_default() {
        let report = GamepadReport::default();
        let bytes = report.to_bytes();
        // Sticks centered at 32768 = 0x8000 LE = [0x00, 0x80]
        assert_eq!(bytes[0], 0x00); // left_x low
        assert_eq!(bytes[1], 0x80); // left_x high
        assert_eq!(bytes[2], 0x00); // left_y low
        assert_eq!(bytes[3], 0x80); // left_y high
        assert_eq!(bytes[4], 0x00); // right_x low (center)
        assert_eq!(bytes[5], 0x80); // right_x high
        assert_eq!(bytes[6], 0x00); // right_y low
        assert_eq!(bytes[7], 0x80); // right_y high
        assert_eq!(bytes[8], 0x00); // left trigger low
        assert_eq!(bytes[9], 0x00); // left trigger high
        assert_eq!(bytes[10], 0x00); // right trigger low
        assert_eq!(bytes[11], 0x00); // right trigger high
        assert_eq!(bytes[12], 0x00); // hat (neutral)
        assert_eq!(bytes[13], 0x00); // buttons low
        assert_eq!(bytes[14], 0x00); // buttons high
        assert_eq!(bytes[15], 0x00); // AC back
    }

    #[test]
    fn to_bytes_buttons() {
        let report = GamepadReport {
            buttons: buttons::A | buttons::Y | buttons::START, // 0x01 | 0x08 | 0x80 = 0x89
            ..Default::default()
        };
        let bytes = report.to_bytes();
        assert_eq!(bytes[13], 0x89); // buttons low byte
        assert_eq!(bytes[14], 0x00); // buttons high byte
    }

    #[test]
    fn to_bytes_triggers() {
        let report = GamepadReport {
            left_trigger: 1023, // max 10-bit = 0x03FF
            right_trigger: 512, // 0x0200
            ..Default::default()
        };
        let bytes = report.to_bytes();
        assert_eq!(bytes[8], 0xFF); // left trigger low
        assert_eq!(bytes[9], 0x03); // left trigger high (10-bit)
        assert_eq!(bytes[10], 0x00); // right trigger low
        assert_eq!(bytes[11], 0x02); // right trigger high
    }

    #[test]
    fn to_bytes_hat_values() {
        for hat_val in 0..=8 {
            let report = GamepadReport {
                hat: hat_val,
                ..Default::default()
            };
            let bytes = report.to_bytes();
            assert_eq!(bytes[12], hat_val & 0x0F);
        }
    }

    #[test]
    fn to_bytes_ms_default_matches_to_bytes() {
        let report = GamepadReport::default();
        let ms = report.to_bytes_ms();
        let std = report.to_bytes();
        // Sticks/triggers/hat/byte15 are identical between layouts.
        for i in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 15] {
            assert_eq!(ms[i], std[i], "byte {i} differs");
        }
        assert_eq!(ms[13], 0);
        assert_eq!(ms[14], 0);
    }

    #[test]
    fn to_bytes_ms_face_buttons() {
        // A B X Y mapped to byte 13 bits 0,1,3,4 (gaps at 2 and 5).
        let cases = [
            (buttons::A, 13, 1 << 0),
            (buttons::B, 13, 1 << 1),
            (buttons::X, 13, 1 << 3),
            (buttons::Y, 13, 1 << 4),
            (buttons::LB, 13, 1 << 6),
            (buttons::RB, 13, 1 << 7),
            (buttons::BACK, 14, 1 << 2),
            (buttons::START, 14, 1 << 3),
            (buttons::L3, 14, 1 << 5),
            (buttons::R3, 14, 1 << 6),
        ];
        for (mask, byte_idx, expected) in cases {
            let report = GamepadReport {
                buttons: mask,
                ..Default::default()
            };
            let bytes = report.to_bytes_ms();
            assert_eq!(
                bytes[byte_idx], expected,
                "mask {mask:#x} should set byte {byte_idx} to {expected:#x}"
            );
            // The other button byte must be untouched.
            let other = if byte_idx == 13 { 14 } else { 13 };
            assert_eq!(bytes[other], 0);
        }
    }

    #[test]
    fn to_bytes_ms_all_buttons() {
        let report = GamepadReport {
            buttons: buttons::A
                | buttons::B
                | buttons::X
                | buttons::Y
                | buttons::LB
                | buttons::RB
                | buttons::BACK
                | buttons::START
                | buttons::L3
                | buttons::R3,
            ..Default::default()
        };
        let bytes = report.to_bytes_ms();
        // byte 13: A(0) B(1) _(2) X(3) Y(4) _(5) LB(6) RB(7) = 0xDB
        assert_eq!(bytes[13], 0b1101_1011);
        // byte 14: _(0) _(1) BACK(2) START(3) _(4) L3(5) R3(6) _(7) = 0x6C
        assert_eq!(bytes[14], 0b0110_1100);
    }

    #[test]
    fn to_bytes_max_values() {
        let report = GamepadReport {
            left_x: 65535,
            left_y: 65535,
            left_trigger: 1023,
            right_trigger: 1023,
            hat: 8,
            buttons: 0x7FFF, // all 15 buttons
        };
        let bytes = report.to_bytes();
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1], 0xFF);
        assert_eq!(bytes[2], 0xFF);
        assert_eq!(bytes[3], 0xFF);
        assert_eq!(bytes[8], 0xFF);
        assert_eq!(bytes[9], 0x03);
        assert_eq!(bytes[10], 0xFF);
        assert_eq!(bytes[11], 0x03);
        assert_eq!(bytes[12], 8);
        assert_eq!(bytes[13], 0xFF);
        assert_eq!(bytes[14], 0x7F);
    }
}
