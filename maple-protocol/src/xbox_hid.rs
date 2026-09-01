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
    pub const fn to_bytes(self) -> [u8; 16] {
        let lx = self.left_x.to_le_bytes();
        let ly = self.left_y.to_le_bytes();
        // Triggers: mask to 10 bits, stored as LE u16 (padding is in high 6 bits)
        let lt = (self.left_trigger & TRIGGER_10BIT_MASK).to_le_bytes();
        let rt = (self.right_trigger & TRIGGER_10BIT_MASK).to_le_bytes();

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

    /// Convert to a 16-byte report using the gappy bit layout a real Microsoft
    /// Xbox One S controller transmits: buttons sit at fixed bit positions with
    /// reserved gaps between them (A=bit0, B=1, X=3, Y=4, ...). This wire layout is
    /// deliberately NOT contiguous. `HID_REPORT_DESCRIPTOR_XBOX` advertises a real
    /// Xbox One S BLE descriptor, which `BlueRetro` fingerprints by descriptor
    /// *shape* (not VID/PID) and then decodes with its own hard-coded Xbox bitmap —
    /// so these gappy bits land on the right Dreamcast functions. Do NOT "align"
    /// these positions to the descriptor's contiguous `Button 1-15` declaration:
    /// that is exactly what broke issue #2.
    ///
    /// Byte 13 (face buttons + bumpers):
    ///   bit 0=A, 1=B, 2=reserved, 3=X, 4=Y, 5=reserved, 6=LB, 7=RB
    /// Byte 14 (system buttons + stick clicks):
    ///   bits 0-1=reserved, 2=View, 3=Menu, 4=Guide/Xbox, 5=L3, 6=R3, 7=reserved
    ///
    /// Sticks, triggers, hat, and byte 15 are identical to `to_bytes`.
    #[must_use]
    pub const fn to_bytes_ms(self) -> [u8; 16] {
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
        if b & buttons::GUIDE != 0 {
            byte14 |= 1 << 4;
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
///
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
///   Bit 10 = Guide / Xbox button (logical flag — `to_bytes_ms` maps it to the
///            real Xbox BLE Guide bit, byte 14 bit 4; not a numbered button)
///   Bits 11-14 = reserved
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
    /// Guide / Xbox button. `to_bytes_ms` emits this at byte 14 bit 4 — the
    /// position SDL/Steam/Windows read as Guide for the 0x0B20 BLE Xbox.
    pub const GUIDE: u16 = 1 << 10;
}

/// HID Report Descriptor — Generic profile (xpadneo-style contiguous layout).
///
/// Buttons 1-15 are packed contiguously in bytes 13-14. Compatible with Steam Input,
/// Linux desktop (xpadneo), and Android generic HID. Used by `PROFILE_GENERIC`
/// (Xbox One S 1708 BLE identity 0x045E/0x0B20). Hosts disagree on 0x0B20's button
/// layout: macOS maps this contiguous form, SDL/Flycast/Windows expect gappy (issue #7).
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
/// This is the *only* report. A HID-over-GATT host can only see reports backed
/// by a GATT Report characteristic (0x2A4D) + Report Reference descriptor
/// (0x2908), and `HidService` exposes exactly one (Report ID 1 / Input). Earlier
/// revisions also declared Report ID 0x02 (Guide/AC Home), 0x03 (rumble Output),
/// and 0x04 (battery) in this Report Map — but none had a backing characteristic,
/// so strict generic-HID hosts (Apple's `GameController` stack / the browser
/// Gamepad API) rejected the whole map while lenient parsers (hidapi, Steam)
/// tolerated it. Battery is reported via the dedicated Battery Service (0x180F),
/// so it is not duplicated here.
#[rustfmt::skip]
pub const HID_REPORT_DESCRIPTOR_GENERIC: &[u8] = &[
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

    // Report IDs 0x02 (Guide/AC Home), 0x03 (rumble Output), and 0x04 (battery)
    // were removed: none had a backing GATT Report characteristic, so strict
    // HID-over-GATT hosts rejected the whole Report Map. Battery is its own
    // service (0x180F); the Guide/rumble functions were never wired up. See the
    // descriptor doc comment above.
    0xC0,              // End Collection
];

/// HID Report Descriptor — Xbox profile (real Xbox One S BLE controller).
///
/// Report ID 0x01 is byte-identical to the descriptor a genuine Xbox One S/Series
/// BLE controller advertises: Z/Rz right stick, Simulation-Controls Brake/Accel
/// triggers, and a contiguous `Button 1-15` block + Consumer Record. `BlueRetro`
/// and other retro adapters fingerprint this descriptor and then apply their own
/// hardcoded Xbox bit map — so the gappy wire bytes from `GamepadReport::to_bytes_ms`
/// (A=0, B=1, X=3, Y=4, LB=6, RB=7, View=10, Menu=11, L3=13, R3=14) decode correctly
/// even though the descriptor declares the buttons contiguously.
///
/// This is the *opposite* trade-off from `HID_REPORT_DESCRIPTOR_GENERIC`: the Generic
/// descriptor uses the xpadneo convention (Rx/Ry + Generic-Desktop Z/Rz, contiguous
/// wire via `to_bytes`) for generic hosts (Android/browsers); the Xbox descriptor
/// mimics the real Xbox so adapters that key off it (`BlueRetro`) map every button correctly.
///
/// Diverging from this layout breaks `BlueRetro` recognition: it then generic-HID
/// parses the buttons by Usage number and X/Y/Start shift (see issue #2).
#[rustfmt::skip]
pub const HID_REPORT_DESCRIPTOR_XBOX: &[u8] = &[
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
    // Z/Rz — matches the real Xbox One S BLE descriptor that BlueRetro fingerprints.
    0x09, 0x01,        //   Usage (Pointer)
    0xA1, 0x00,        //   Collection (Physical)
    0x09, 0x32,        //     Usage (Z)
    0x09, 0x35,        //     Usage (Rz)
    0x15, 0x00,        //     Logical Minimum (0)
    0x27, 0xFF, 0xFF, 0x00, 0x00, //  Logical Maximum (65535)
    0x95, 0x02,        //     Report Count (2)
    0x75, 0x10,        //     Report Size (16)
    0x81, 0x02,        //     Input (Data, Variable, Absolute)
    0xC0,              //   End Collection

    // Left Trigger (Simulation Controls Brake, 10-bit + 6 padding)
    0x05, 0x02,        //   Usage Page (Simulation Controls)
    0x09, 0xC5,        //   Usage (Brake)
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

    // Right Trigger (Simulation Controls Accelerator, 10-bit + 6 padding)
    0x05, 0x02,        //   Usage Page (Simulation Controls)
    0x09, 0xC4,        //   Usage (Accelerator)
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

    // Buttons 1-15 — contiguous declaration matching the real Xbox One S BLE
    // descriptor. The wire bytes stay gappy (GamepadReport::to_bytes_ms); once
    // BlueRetro fingerprints this as an Xbox controller it applies its own
    // hardcoded bit map, so X/Y/Start land correctly despite the contiguous
    // declaration (proven by the harness control test).
    0x05, 0x09,        //   Usage Page (Button)
    0x19, 0x01,        //   Usage Minimum (Button 1)
    0x29, 0x0F,        //   Usage Maximum (Button 15)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x01,        //   Logical Maximum (1)
    0x75, 0x01,        //   Report Size (1)
    0x95, 0x0F,        //   Report Count (15)
    0x81, 0x02,        //   Input (Data, Variable, Absolute)
    // 1-bit padding (byte 14 bit 7)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x00,        //   Logical Maximum (0)
    0x75, 0x01,        //   Report Size (1)
    0x95, 0x01,        //   Report Count (1)
    0x81, 0x03,        //   Input (Constant) - padding

    // Byte 15 bit 0: Consumer Record (Share) — 1 bit + 7-bit padding.
    // Matches the real Xbox descriptor; we never set it (Dreamcast has none).
    0x05, 0x0C,        //   Usage Page (Consumer)
    0x0A, 0xB2, 0x00,  //   Usage (Record)
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

    // === Report ID 0x03: Rumble Output (BACKED by HidService.rumble) ===
    // Matches the real Xbox One S/Series BLE descriptor (verified against DJm00n's
    // 1914 0x0B20 dump). Windows' "Bluetooth LE XINPUT compatible input device"
    // driver requires an Output (rumble) report to start -- without it the device
    // fails Code 10 / STATUS_INVALID_PARAMETER. This is backed by a real GATT
    // Output characteristic (0x2A4D, Report Ref [0x03,0x02]) -- NOT the phantom
    // report that caused the earlier Steam crash. Writes are accepted and ignored
    // (no Dreamcast rumble actuator wired yet).
    // Reports 0x02 (Guide) and 0x04 (battery) stay removed -- the real BLE Xbox has
    // neither (Guide is in Report 1's buttons; battery is the 0x180F service).
    0x05, 0x0F,        //   Usage Page (Physical Interface Device)
    0x09, 0x21,        //   Usage (Set Effect Report)
    0x85, 0x03,        //   Report ID (3)
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
    0xC0,              //   End Collection (rumble)

    0xC0,              // End Collection (application)
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
    fn to_bytes_ms_guide() {
        // Guide/Xbox button -> byte 14 bit 4 (the 0x0B20 BLE Guide position).
        let report = GamepadReport {
            buttons: buttons::GUIDE,
            ..Default::default()
        };
        let bytes = report.to_bytes_ms();
        assert_eq!(bytes[14], 1 << 4);
        assert_eq!(bytes[13], 0);
    }

    #[test]
    fn to_bytes_guide_is_button_11() {
        // Generic/contiguous layout: the Guide chord (same profile-agnostic
        // detection as Xbox) sets buttons::GUIDE (bit 10), which `to_bytes` packs
        // into byte 14 bit 2 = Button 11 — a host-visible numbered button the user
        // can bind to "Guide" in Steam. (Xbox's `to_bytes_ms` instead lands it on
        // the native 0x0B20 Guide bit; see `to_bytes_ms_guide`.)
        let report = GamepadReport {
            buttons: buttons::GUIDE,
            ..Default::default()
        };
        let bytes = report.to_bytes();
        assert_eq!(bytes[14], 1 << 2); // Button 11
        assert_eq!(bytes[13], 0);
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
