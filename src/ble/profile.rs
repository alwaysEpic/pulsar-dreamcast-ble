// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! BLE HID controller profiles.
//!
//! Each `Profile` bundles everything that differs between operating modes:
//! GAP name, scan response, manufacturer/model strings, VID/PID, HID descriptor,
//! and VMU display label. The active profile is selected at boot from flash and
//! persists across power cycles.
//!
//! The two profiles are host-facing identities. `Xbox` is the faithful original
//! Xbox One S identity/layout used by XInput, Steam, SDL, emulators, Linux,
//! BlueRetro, and retro adapters — and the one macOS's browser Gamepad API
//! surfaces (it only recognizes Xbox/DS4/MFi). `Generic` keeps the Dreamcast name
//! + a contiguous layout under a neutral pid.codes identity so Windows treats it as
//! a plain HID gamepad (DirectInput) rather than loading the Xbox/XInput driver.

use crate::ble::hid::{HID_REPORT_DESCRIPTOR_GENERIC, HID_REPORT_DESCRIPTOR_XBOX};
use crate::vmu::{GLYPH_DREAMCAST, GLYPH_XBOX};
use maple_protocol::xbox_hid::GamepadReport;

/// Identifier for the active profile, persisted to flash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ProfileId {
    /// Real Xbox One S 1708 **BLE** identity (Microsoft VID 0x045E / PID 0x0B20,
    /// post-FW-5.11) serving the raw Xbox report-1 layout. This is what a genuine
    /// Xbox controller advertises *over Bluetooth LE*, so it matches the BLE decode
    /// path Windows/XInput, Steam, SDL (Flycast/RetroArch/MAME), Linux xpadneo, and
    /// BlueRetro use. (The USB/dongle PID 0x02E0 was dropped — a BLE controller
    /// never presents it, and 0x02E0-over-BLE is what hosts incl. Steam choke on.)
    Xbox = 0,
    /// Neutral pid.codes identity (VID 0x1209 / PID 0xDC01) serving our clean
    /// single-report standard-gamepad descriptor (contiguous Buttons 1-15, two
    /// separate trigger axes). Advertised as "Dreamcast Wireless Controller". The
    /// non-Xbox VID keeps Windows on the generic HID (DirectInput) path instead of
    /// the Xbox/XInput driver — for Windows, the browser Gamepad API, Steam-generic,
    /// Linux, and Android. NOT visible to the macOS browser Gamepad API (use Xbox).
    Generic = 1,
}

impl ProfileId {
    /// Resolve to the static `Profile` describing this mode.
    #[must_use]
    pub fn profile(self) -> &'static Profile {
        match self {
            Self::Xbox => &PROFILE_XBOX,
            Self::Generic => &PROFILE_GENERIC,
        }
    }

    /// Return the next profile in cycle order (currently a toggle).
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Xbox => Self::Generic,
            Self::Generic => Self::Xbox,
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

const NAME_XBOX: &[u8] = b"Xbox Wireless Controller\0";
const NAME_DREAMCAST: &[u8] = b"Dreamcast Wireless Controller\0";

// === Pre-built scan responses (Complete Local Name AD structure) ===
//
// Each entry encodes [length, AD type 0x09, name bytes...] where length covers
// the AD type byte plus the name characters (NUL not included).

#[rustfmt::skip]
const SCAN_RESPONSE_XBOX: [u8; 26] = [
    0x19, 0x09,
    b'X', b'b', b'o', b'x', b' ',
    b'W', b'i', b'r', b'e', b'l', b'e', b's', b's', b' ',
    b'C', b'o', b'n', b't', b'r', b'o', b'l', b'l', b'e', b'r',
];

#[rustfmt::skip]
const SCAN_RESPONSE_DREAMCAST: [u8; 31] = [
    0x1E, 0x09,
    b'D', b'r', b'e', b'a', b'm', b'c', b'a', b's', b't', b' ',
    b'W', b'i', b'r', b'e', b'l', b'e', b's', b's', b' ',
    b'C', b'o', b'n', b't', b'r', b'o', b'l', b'l', b'e', b'r',
];

// Compile-time guard: scan response length byte = total bytes after it.
const _: () = assert!(SCAN_RESPONSE_XBOX[0] as usize == SCAN_RESPONSE_XBOX.len() - 1);
const _: () = assert!(SCAN_RESPONSE_DREAMCAST[0] as usize == SCAN_RESPONSE_DREAMCAST.len() - 1);

// Compile-time guard: HID descriptors must fit in the report_map Vec<u8, 512>.
const _: () = assert!(HID_REPORT_DESCRIPTOR_XBOX.len() <= 512);
const _: () = assert!(HID_REPORT_DESCRIPTOR_GENERIC.len() <= 512);

// === Profile definitions ===

/// Real Xbox One S 1708 BLE identity (Microsoft 0x045E / PID 0x0B20) serving the
/// raw Xbox report-1 layout. The BLE-native Xbox identity; the default profile.
pub static PROFILE_XBOX: Profile = Profile {
    id: ProfileId::Xbox,
    gap_name: NAME_XBOX,
    scan_response: &SCAN_RESPONSE_XBOX,
    manufacturer: b"Microsoft",
    model: b"Xbox Wireless Controller",
    vid: 0x045E,
    // Real Xbox One S 1708 BLE PID (post-FW-5.11): the identity a genuine Xbox
    // presents over BLE, so host BLE decode paths (SDL/Windows) match our report.
    pid: 0x0B20,
    version: 0x0100,
    hid_descriptor: HID_REPORT_DESCRIPTOR_XBOX,
    serialize_report: GamepadReport::to_bytes_ms,
    vmu_glyph: &GLYPH_XBOX,
    vmu_label: b"XBOX",
};

/// Generic profile: a clean single-report standard-gamepad descriptor served
/// under a neutral pid.codes identity (VID 0x1209 / PID 0xDC01), visible name
/// "Dreamcast Wireless Controller".
///
/// This is the **Windows / browser-Gamepad-API / Steam-generic / Linux / Android**
/// profile: a non-Xbox VID makes Windows treat it as a plain HID gamepad
/// (DirectInput) instead of loading the Xbox/XInput driver. NOTE: macOS's
/// GameController framework only surfaces *recognized* controllers (Xbox/DS4/MFi),
/// so this identity is NOT visible to the macOS browser Gamepad API — use the Xbox
/// profile there. (Reverts ac46f97's 0x045E/0x0B20, which fixed macOS browser but
/// broke Windows by routing it into the Xbox/XInput driver path.)
pub static PROFILE_GENERIC: Profile = Profile {
    id: ProfileId::Generic,
    gap_name: NAME_DREAMCAST,
    scan_response: &SCAN_RESPONSE_DREAMCAST,
    manufacturer: b"Pulsar",
    model: b"Dreamcast Wireless Controller",
    // pid.codes open-source VID + project PID — deliberately NOT a Microsoft/Xbox
    // VID, so Windows uses the generic HID path. Trade-off: invisible to the macOS
    // browser Gamepad API (macOS only surfaces recognized VID/PIDs — use Xbox there).
    vid: 0x1209,
    pid: 0xDC01,
    version: 0x0100,
    hid_descriptor: HID_REPORT_DESCRIPTOR_GENERIC,
    serialize_report: GamepadReport::to_bytes,
    vmu_glyph: &GLYPH_DREAMCAST,
    vmu_label: b"DC",
};
