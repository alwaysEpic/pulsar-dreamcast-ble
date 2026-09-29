// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! HID over GATT (HOG) implementation for gamepad.
//!
//! Implements Xbox One S BLE HID format (Model 1708, PID `0x02E0`).
//! Pure report types re-exported from `maple_protocol::xbox_hid`.

#![expect(
    clippy::redundant_else,
    reason = "expanded from the nrf-softdevice #[gatt_server]/#[gatt_service] macros; \
            the shape is not ours to change"
)]
#![expect(
    clippy::missing_errors_doc,
    reason = "the error type is this module's own single-variant enum, and every caller \
            is in-crate; a # Errors section would restate the signature"
)]
#![expect(
    clippy::unnecessary_semicolon,
    reason = "expanded from the nrf-softdevice #[gatt_server]/#[gatt_service] macros; \
            the shape is not ours to change"
)]

pub use maple_protocol::xbox_hid::{
    buttons, hat, GamepadReport, HID_REPORT_DESCRIPTOR_GENERIC, HID_REPORT_DESCRIPTOR_XBOX,
};

use heapless::Vec;
use maple_protocol::notify_budget::Outcome;
use nrf_softdevice::ble::gatt_server::{NotifyValueError, SetValueError};
use nrf_softdevice::ble::{Connection, SecurityMode};
use nrf_softdevice::RawError;

/// HID Information characteristic value.
/// bcdHID: 1.11, bCountryCode: 0, Flags: `RemoteWake` | `NormallyConnectable`
pub const HID_INFO: [u8; 4] = [0x11, 0x01, 0x00, 0x03];

/// Protocol Mode: Report Protocol (1) vs Boot Protocol (0)
pub const PROTOCOL_MODE_REPORT: u8 = 1;

// GATT Service definitions using nrf-softdevice macros

/// HID Service (UUID 0x1812)
/// Security: `JustWorks` (encrypted, unauthenticated) - required by HOGP spec
///
/// **Characteristic order is load-bearing**: with the services in
/// `GamepadServer`'s order it puts Report Map at handle `0x001C`, the input
/// report at `0x001E` with its CCCD at `0x001F`, and rumble at `0x0022` —
/// a real Xbox One S's handles. The 8BitDo USB Wireless Adapter 2 skips
/// service discovery and uses those handles directly. Add characteristics
/// only after `rumble`.
#[nrf_softdevice::gatt_service(uuid = "1812")]
pub struct HidService {
    /// HID Information (UUID 0x2A4A) - Read only
    /// Value: [bcdHID_lo, bcdHID_hi, bCountryCode, flags]
    #[characteristic(uuid = "2A4A", read, security = "JustWorks")]
    pub hid_info: [u8; 4],

    /// HID Control Point (UUID 0x2A4C) - Write without response
    #[characteristic(uuid = "2A4C", write_without_response, security = "JustWorks")]
    pub control_point: u8,

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

    /// HID Report - Output (UUID 0x2A4D), Report ID 3 — rumble (8 bytes).
    /// The real Xbox One S/Series BLE controller exposes this, and Windows'
    /// "Bluetooth LE XINPUT compatible input device" driver requires an Output
    /// report to start (no Output → Code 10 / STATUS_INVALID_PARAMETER). Writes
    /// are accepted and ignored — the gatt_server run loop swallows all writes
    /// (no Dreamcast rumble actuator wired yet).
    #[characteristic(
        uuid = "2A4D",
        write,
        write_without_response,
        security = "JustWorks",
        descriptor(uuid = "2908", security = "JustWorks", value = "[0x03, 0x02]")
    )]
    pub rumble: [u8; 8],

    /// Protocol Mode (UUID 0x2A4E) - Read, Write Without Response
    #[characteristic(uuid = "2A4E", read, write_without_response, security = "JustWorks")]
    pub protocol_mode: u8,
}

/// Device Information Service (UUID 0x180A)
#[nrf_softdevice::gatt_service(uuid = "180A")]
pub struct DeviceInfoService {
    /// Manufacturer Name (UUID 0x2A29)
    #[characteristic(uuid = "2A29", read)]
    pub manufacturer: Vec<u8, 32>,

    /// PnP ID (UUID 0x2A50) - Vendor ID, Product ID, Version
    #[characteristic(uuid = "2A50", read)]
    pub pnp_id: [u8; 7],

    /// Firmware Revision (UUID 0x2A26). A real pad has one here; it also makes
    /// this service four characteristics long, as the handle layout needs.
    #[characteristic(uuid = "2A26", read)]
    pub firmware_revision: Vec<u8, 16>,

    /// Model Number (UUID 0x2A24). Where a real pad has its serial number.
    #[characteristic(uuid = "2A24", read)]
    pub model_number: Vec<u8, 32>,
}

/// Battery Service (UUID 0x180F)
#[nrf_softdevice::gatt_service(uuid = "180F")]
pub struct BatteryService {
    /// Battery Level (UUID 0x2A19) - 0-100%
    #[characteristic(uuid = "2A19", read, notify)]
    pub battery_level: u8,
}

/// Pulsar host-integration service  — the console-side dongle's
/// channel to the docked VMU's screen.
///
/// A 128-bit primary service, so it is invisible to a host that does not look
/// for it by UUID and cannot collide with an assigned number. The UUIDs are
/// a published contract (`docs/host_integration.md`) — every sender uses
/// exactly these values, so they are never to be regenerated.
///
/// Present in **both** BLE personalities, Xbox and Generic, because there is
/// one GATT table and the profile only swaps names, IDs and the report
/// descriptor. So a sender can key on this service under either identity —
/// the service, not the identity, is what identifies a Pulsar.
#[nrf_softdevice::gatt_service(uuid = "7EDF0001-3536-4A03-82D0-8AB9122016C6")]
pub struct HostService {
    /// LCD frame (UUID `…0002`) — Write Without Response.
    ///
    /// `Vec<u8, LCD_BYTES>`, not `[u8; LCD_BYTES]`, and the difference is
    /// load-bearing rather than stylistic. `GattValue for [u8; N]` reports
    /// `MIN_SIZE = 0` and **zero-pads** anything shorter to N, so a 49-byte
    /// chunk write would arrive as a 192-byte array indistinguishable from a
    /// whole frame followed by 143 blank bytes — the two wire shapes are told
    /// apart by length, and the array type destroys exactly that. The `Vec`
    /// impl preserves it. Writes longer than 192 never reach us: the attribute
    /// is registered with a 192-byte maximum and the SoftDevice rejects the
    /// rest at the ATT layer.
    ///
    /// `JustWorks` like the HID characteristics: a sender is already bonded and
    /// encrypted to push HID, so this costs it nothing and keeps the screen off
    /// the air for unpaired strangers.
    #[characteristic(
        uuid = "7EDF0002-3536-4A03-82D0-8AB9122016C6",
        write_without_response,
        security = "JustWorks"
    )]
    pub lcd_frame: Vec<u8, { crate::vmu::LCD_BYTES }>,

    /// VMU storage, up (UUID `…0003`) — Notify. `81 DATA`, `83 STATUS`.
    ///
    /// **Attribute-table cost, and the failure mode to check for first.** These
    /// two characteristics add roughly 320 bytes to `gatts_attr_tab_size`
    /// (2048): a 132-byte value each, their declarations, and a CCCD. An earlier revision
    /// estimated ~1000 in use before its 192-byte frame characteristic, so this
    /// should leave ~470 spare — an estimate, not a measurement, and the
    /// failure mode if it is wrong is `GamepadServer::new` erroring into main's
    /// silent `wfi` loop: a device that boots dark and never advertises. The
    /// first bench boot is that check.
    ///
    /// The 132 is the protocol's, not this revision's: a READ is two bytes and
    /// the write path that needs the rest is a later step. Registering it small
    /// now and growing it later would move the table under a cached bond, which
    /// is the more expensive mistake.
    ///
    /// 132 bytes: a four-byte header and one 128-byte write phase, which is
    /// protocol v1's unit in both directions. **Not** the MTU and not 512 — a
    /// GATTS event is bounded by the *registered maximum*, and sizing this to
    /// the MTU is the shape that panicked an earlier revision.
    #[characteristic(
        uuid = "7EDF0003-3536-4A03-82D0-8AB9122016C6",
        notify,
        security = "JustWorks"
    )]
    pub vmu_up: Vec<u8, { maple_protocol::host_vmu_io::MSG_MAX }>,

    /// VMU storage, down (UUID `…0004`) — Write Without Response. `01 READ`,
    /// `02 WRITE`, `03 STATUS?`.
    ///
    /// `Vec`, not an array, for `lcd_frame`'s reason: `GattValue for [u8; N]`
    /// zero-pads a short write to N, and every op here is told apart by its
    /// length as well as its first byte.
    #[characteristic(
        uuid = "7EDF0004-3536-4A03-82D0-8AB9122016C6",
        write_without_response,
        security = "JustWorks"
    )]
    pub vmu_down: Vec<u8, { maple_protocol::host_vmu_io::MSG_MAX }>,
}

/// Combined GATT server with all services.
#[nrf_softdevice::gatt_server]
pub struct GamepadServer {
    // Service order is load-bearing: it copies a real Xbox One S's handle
    // layout, which the 8BitDo USB Wireless Adapter 2 relies on (see
    // `HidService`). Our host service goes last, where the pad's vendor
    // service sits.
    pub device_info: DeviceInfoService,
    pub battery: BatteryService,
    pub hid: HidService,
    pub host: HostService,
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
        self.hid
            .report_set(&(profile.serialize_report)(initial_report))?;

        // Device Information from active profile
        let mut manufacturer: Vec<u8, 32> = Vec::new();
        let _ = manufacturer.extend_from_slice(profile.manufacturer).ok();
        self.device_info.manufacturer_set(&manufacturer)?;

        let mut model: Vec<u8, 32> = Vec::new();
        let _ = model.extend_from_slice(profile.model).ok();
        self.device_info.model_number_set(&model)?;

        let mut firmware: Vec<u8, 16> = Vec::new();
        let _ = firmware
            .extend_from_slice(env!("CARGO_PKG_VERSION").as_bytes())
            .ok();
        self.device_info.firmware_revision_set(&firmware)?;

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
        let bytes = Self::serialize_report_for_active_profile(report);

        // Debug-only: overwrite the right-stick bytes (4-7) with the latest raw
        // IP5306 gauge sample. The Dreamcast has no right stick, so those four
        // bytes are a constant 0x8000/0x8000 and carry no real input.
        //
        // Injected *before* dedup, unlike `seq-counter` below: a new gauge
        // sample changes the payload and so forces a notify, which is what makes
        // the channel work on an idle controller. (seq-counter deliberately goes
        // after, because dedup behavior is the thing it measures.)
        #[cfg(feature = "gauge-debug")]
        let bytes = {
            let mut b = bytes;
            let packed = crate::GAUGE_SAMPLE.load(core::sync::atomic::Ordering::Relaxed);
            b[4..8].copy_from_slice(&packed.to_le_bytes());
            b
        };

        // Debug-only: same four right-stick bytes, carrying the connection
        // parameter negotiation instead.
        //
        // `conn.conn_params()` is the **live negotiated** value, not what we
        // asked for: nrf-softdevice writes `state.conn_params` from
        // `BLE_GAP_EVT_CONN_PARAM_UPDATE` (its `gap.rs`), so this is the
        // interval the link is actually running at, read from the SoftDevice's
        // own bookkeeping. Pairing it with the update call's return code
        // separates "the host declined our request" from "the request was never
        // issued" — indistinguishable from outside, and the reason three rounds
        // of parameter tuning produced no information.
        //
        // Both fields are in 1.25 ms units (12 = 15 ms, 9 = 11.25 ms), and are
        // saturated to u8: the BLE maximum is 3200 units, but anything past 255
        // (319 ms) means something has gone far more wrong than a rejected
        // request.
        //
        // ⚠ Unlike `gauge-debug`, this payload is **constant** — rc and the
        // negotiated interval do not change once the link is up. So it does NOT
        // defeat the wire-level dedup below and does NOT self-publish on an idle
        // controller: with a still stick every report is byte-identical, every
        // notify is skipped, and a capture reads **zero** reports. Keep the
        // input moving for the whole capture, exactly like a normal run.
        // (Observed 2026-07-27: a capture taken on a deliberately idle stick
        // returned 0 reads and looked like a dead link.)
        // Debug-only: right-stick bytes carry the Maple poll-failure counters.
        // Injected *before* dedup like `gauge-debug`, but the bytes only change
        // when a poll actually fails (~a few times a second under the fault
        // being chased), so dedup dynamics stay representative. Layout:
        // [4-5] = total failed polls (LE u16, wrapping), [6] = longest
        // consecutive-failure streak since boot, [7] = 0x5A marker.
        #[cfg(feature = "maple-fail-debug")]
        let bytes = {
            let mut b = bytes;
            let total = crate::MAPLE_FAIL_TOTAL.load(core::sync::atomic::Ordering::Relaxed);
            let streak = crate::MAPLE_FAIL_MAX_CONSEC.load(core::sync::atomic::Ordering::Relaxed);
            b[4..6].copy_from_slice(&total.to_le_bytes());
            b[6] = streak;
            b[7] = crate::MAPLE_FAIL_MAGIC;
            b
        };

        // Debug-only: same four right-stick bytes, carrying poll-loop period
        // telemetry (rotating tagged payloads — see `crate::poll_period`).
        // Injected before dedup like the channels above. The payload advances
        // once per fresh controller sample and is replayed byte-for-byte in
        // between, so the dedup below still collapses repeats of one sample —
        // it used to rotate per send, which defeated dedup outright and made
        // the host's arrival cadence a property of the connection interval
        // rather than the poll loop (v297 run #200). Dedup dynamics here are
        // close to, but still not, a bare build's: a *changed* stick and a
        // fresh sample are not the same event. Acceptance captures use builds
        // without this feature.
        #[cfg(feature = "poll-period-debug")]
        let bytes = {
            let mut b = bytes;
            crate::poll_period::inject(&mut b);
            b
        };

        #[cfg(feature = "connparam-debug")]
        let bytes = {
            let mut b = bytes;
            let p = conn.conn_params();
            let rc = crate::CONNPARAM_RC.load(core::sync::atomic::Ordering::Relaxed);
            b[4] = u8::try_from(rc).unwrap_or(0xFE); // 0xFE = rc did not fit
            b[5] = u8::try_from(p.min_conn_interval).unwrap_or(0xFF);
            b[6] = u8::try_from(p.max_conn_interval).unwrap_or(0xFF);
            b[7] = crate::CONNPARAM_MAGIC;
            b
        };

        let skip = LAST_REPORT.lock(|cell| cell.get().is_some_and(|c| c == bytes));
        if skip {
            return Ok(());
        }

        // Debug-only: stamp a 7-bit sequence counter into byte 15 bits 1-7 (HID
        // padding; bit 0 = Consumer Record, left untouched) so a host capture can
        // detect reports dropped between here and the host. Injected *after* dedup,
        // so the dedup/send-on-change behavior under test is unchanged.
        #[cfg(feature = "seq-counter")]
        let wire_bytes = {
            let mut b = bytes;
            let n = SEQ_COUNTER.lock(|c| {
                let v = c.get();
                c.set(v.wrapping_add(1));
                v
            });
            b[15] = (b[15] & 0x01) | ((n & 0x7F) << 1);
            b
        };
        #[cfg(not(feature = "seq-counter"))]
        let wire_bytes = bytes;

        // Cache only after the SoftDevice accepts the notification. Updating
        // the cache before the send poisoned it on TX-queue-full errors: the
        // next tick with an unchanged stick deduped against a report the host
        // never received, silently swallowing that state change (host-side
        // symptom: a doubled connection interval). The cache stores the
        // pre-seq payload so dedup compares input state, not counter noise.
        let result = self.hid.report_notify(conn, &wire_bytes);
        if result.is_ok() {
            LAST_REPORT.lock(|cell| cell.set(Some(bytes)));
        }
        result
    }

    /// Reduce a [`send_report`](Self::send_report) result to the
    /// [`Outcome`] the notify budget judges.
    ///
    /// The meaning of each class — which are "not yet" and which are "never" —
    /// lives with the rule in `maple_protocol::notify_budget`, where it is
    /// tested; this is only the mapping from the SoftDevice's codes.
    #[must_use]
    pub const fn notify_outcome(result: Result<(), NotifyValueError>) -> Outcome {
        match result {
            Ok(()) => Outcome::Sent,
            Err(NotifyValueError::Raw(RawError::Resources)) => Outcome::QueueFull,
            Err(NotifyValueError::Raw(RawError::InvalidState)) => Outcome::NotSubscribed,
            Err(NotifyValueError::Raw(RawError::BleGattsSysAttrMissing)) => Outcome::AttrsMissing,
            Err(_) => Outcome::Refused,
        }
    }

    /// Whether the link is encrypted — the gate on sending a report at all.
    ///
    /// `security_mode` is `Open` at connect and raised from
    /// `BLE_GAP_EVT_CONN_SEC_UPDATE` in the same event drain as everything
    /// else, so it is current when the notify loop reads it. Encryption is
    /// security mode 1 at level 2 or above; the `Signed` variants are mode 2,
    /// data signing without encryption, so they are listed out rather than
    /// "anything but `Open`". The pinned library maps a mode it does not
    /// recognise to `Open`, so doubt reads as unencrypted — the safe direction.
    #[must_use]
    pub fn link_encrypted(conn: &Connection) -> bool {
        matches!(
            conn.security_mode(),
            SecurityMode::JustWorks | SecurityMode::Mitm | SecurityMode::LescMitm
        )
    }

    /// Serialize a report using the active BLE profile.
    #[must_use]
    pub fn serialize_report_for_active_profile(report: &GamepadReport) -> [u8; 16] {
        let profile = crate::ble::softdevice::get_profile();
        (profile.serialize_report)(*report)
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
