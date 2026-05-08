// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! BLE HID controller profiles.
//!
//! Each `Profile` bundles everything that differs between operating modes:
//! GAP name, scan response, manufacturer/model strings, VID/PID, HID descriptor,
//! and VMU display label. The active profile is selected at boot from flash and
//! persists across power cycles.

use crate::ble::hid::{HID_REPORT_DESCRIPTOR_EXT, HID_REPORT_DESCRIPTOR_STD};
use crate::vmu::{GLYPH_EXT, GLYPH_STD};
use maple_protocol::xbox_hid::GamepadReport;

/// Identifier for the active profile, persisted to flash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ProfileId {
    /// Original Microsoft Xbox One S BT Classic layout (PID 0x02E0).
    /// What most parsers expect when they see a Microsoft VID — works with
    /// BlueRetro, kernel HID quirks, browser Gamepad API, retro adapters.
    Std = 0,
    /// Newer xpadneo-patched / post-FW BLE layout (PID 0x0B20).
    /// Contiguous Buttons 1-15. Compatible with Steam Input, Linux xpadneo,
    /// Android generic HID — narrower audience but the "modern" descriptor.
    Ext = 1,
}

impl ProfileId {
    /// Resolve to the static `Profile` describing this mode.
    #[must_use]
    pub fn profile(self) -> &'static Profile {
        match self {
            Self::Std => &PROFILE_STD,
            Self::Ext => &PROFILE_EXT,
        }
    }

    /// Return the next profile in cycle order (currently a toggle).
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Std => Self::Ext,
            Self::Ext => Self::Std,
        }
    }
}

/// Per-profile configuration. All fields are static — profile data lives in flash.
pub struct Profile {
    pub id: ProfileId,
    /// GAP device name, NUL-terminated for the SoftDevice.
    pub gap_name: &'static [u8],
    /// Pre-built scan response payload (length-prefixed AD structure).
    pub scan_response: &'static [u8],
    /// Manufacturer string for Device Information Service (0x2A29).
    pub manufacturer: &'static [u8],
    /// Model string for Device Information Service (0x2A24).
    pub model: &'static [u8],
    /// Vendor ID (PnP ID 0x2A50).
    pub vid: u16,
    /// Product ID (PnP ID 0x2A50).
    pub pid: u16,
    /// Product version (PnP ID 0x2A50).
    pub version: u16,
    /// HID Report Map (0x2A4B).
    pub hid_descriptor: &'static [u8],
    /// Serializer that turns a logical `GamepadReport` into the on-the-wire
    /// 16-byte Report ID 0x01 payload for this profile's descriptor.
    pub serialize_report: fn(GamepadReport) -> [u8; 16],
    /// 32×24 monochrome splash glyph (96 bytes, 4 bytes per row, MSB-left).
    pub vmu_glyph: &'static [u8; crate::vmu::GLYPH_BYTES],
    /// Short label shown on the VMU under the glyph.
    pub vmu_label: &'static [u8],
}

// === Profile names (NUL-terminated for SoftDevice GAP) ===

const NAME_STD: &[u8] = b"Xbox Wireless Controller\0";
const NAME_EXT: &[u8] = b"Dreamcast Wireless Controller\0";

// === Pre-built scan responses (Complete Local Name AD structure) ===
//
// Each entry encodes [length, AD type 0x09, name bytes...] where length covers
// the AD type byte plus the name characters (NUL not included).

#[rustfmt::skip]
const SCAN_RESPONSE_STD: [u8; 26] = [
    0x19, 0x09,
    b'X', b'b', b'o', b'x', b' ',
    b'W', b'i', b'r', b'e', b'l', b'e', b's', b's', b' ',
    b'C', b'o', b'n', b't', b'r', b'o', b'l', b'l', b'e', b'r',
];

#[rustfmt::skip]
const SCAN_RESPONSE_EXT: [u8; 31] = [
    0x1E, 0x09,
    b'D', b'r', b'e', b'a', b'm', b'c', b'a', b's', b't', b' ',
    b'W', b'i', b'r', b'e', b'l', b'e', b's', b's', b' ',
    b'C', b'o', b'n', b't', b'r', b'o', b'l', b'l', b'e', b'r',
];

// Compile-time guard: scan response length byte = total bytes after it.
const _: () = assert!(SCAN_RESPONSE_STD[0] as usize == SCAN_RESPONSE_STD.len() - 1);
const _: () = assert!(SCAN_RESPONSE_EXT[0] as usize == SCAN_RESPONSE_EXT.len() - 1);

// Compile-time guard: HID descriptors must fit in the report_map Vec<u8, 512>.
const _: () = assert!(HID_REPORT_DESCRIPTOR_STD.len() <= 512);
const _: () = assert!(HID_REPORT_DESCRIPTOR_EXT.len() <= 512);

// === Profile definitions ===

/// Original Microsoft Xbox One S BT Classic layout (PID 0x02E0).
/// Compatible with BlueRetro, kernel HID quirks, and most retro adapters.
pub static PROFILE_STD: Profile = Profile {
    id: ProfileId::Std,
    gap_name: NAME_STD,
    scan_response: &SCAN_RESPONSE_STD,
    manufacturer: b"Microsoft",
    model: b"Xbox Wireless Controller",
    vid: 0x045E,
    pid: 0x02E0,
    version: 0x0100,
    hid_descriptor: HID_REPORT_DESCRIPTOR_STD,
    serialize_report: GamepadReport::to_bytes_ms,
    vmu_glyph: &GLYPH_STD,
    vmu_label: b"STD",
};

/// Newer xpadneo-patched / post-FW Xbox One S BLE layout (PID 0x0B20).
/// Compatible with Steam Input, Linux xpadneo, and Android generic HID.
pub static PROFILE_EXT: Profile = Profile {
    id: ProfileId::Ext,
    gap_name: NAME_EXT,
    scan_response: &SCAN_RESPONSE_EXT,
    manufacturer: b"Microsoft",
    model: b"Xbox Wireless Controller",
    vid: 0x045E,
    pid: 0x0B20,
    version: 0x0100,
    hid_descriptor: HID_REPORT_DESCRIPTOR_EXT,
    serialize_report: GamepadReport::to_bytes,
    vmu_glyph: &GLYPH_EXT,
    vmu_label: b"EXT",
};
