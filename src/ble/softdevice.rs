// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! `SoftDevice` initialization and BLE advertising.

use crate::log;
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use nrf_softdevice::ble::{peripheral, Connection};
use nrf_softdevice::{raw, Softdevice};

use crate::ble::hid::GamepadServer;
use crate::ble::profile::{Profile, PROFILE_XBOX};
use crate::ble::security::Bonder;

/// Connection state machine states.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ConnectionState {
    /// Power-on: trying to reconnect to bonded device (60s timeout)
    Reconnecting = 0,
    /// No connection, not advertising (after reconnect timeout)
    Idle = 1,
    /// User-initiated sync mode: discoverable to all (60s timeout)
    SyncMode = 2,
    /// Connected to a device
    Connected = 3,
}

#[allow(clippy::match_same_arms)]
impl From<u8> for ConnectionState {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Reconnecting,
            1 => Self::Idle,
            2 => Self::SyncMode,
            3 => Self::Connected,
            _ => Self::Idle,
        }
    }
}

/// Global connection state (atomic for cross-task access).
static CONNECTION_STATE: AtomicU8 = AtomicU8::new(ConnectionState::Reconnecting as u8);

/// Get current connection state.
pub fn get_connection_state() -> ConnectionState {
    CONNECTION_STATE.load(Ordering::Relaxed).into()
}

/// Set connection state.
pub fn set_connection_state(state: ConnectionState) {
    CONNECTION_STATE.store(state as u8, Ordering::Relaxed);
}

/// `SoftDevice` configuration for BLE peripheral mode.
#[allow(clippy::cast_possible_truncation)] // SoftDevice FFI constants are small values
fn softdevice_config(profile: &Profile) -> nrf_softdevice::Config {
    let name = profile.gap_name.as_ptr();
    // Subtract trailing NUL — SoftDevice expects unterminated length.
    let name_len = (profile.gap_name.len() - 1) as u16;

    nrf_softdevice::Config {
        clock: Some(raw::nrf_clock_lf_cfg_t {
            source: raw::NRF_CLOCK_LF_SRC_RC as u8,
            rc_ctiv: 16,
            rc_temp_ctiv: 2,
            accuracy: raw::NRF_CLOCK_LF_ACCURACY_500_PPM as u8,
        }),
        conn_gap: Some(raw::ble_gap_conn_cfg_t {
            conn_count: 1,
            event_length: 6, // Allow short events for fast intervals
        }),
        conn_gatt: Some(raw::ble_gatt_conn_cfg_t { att_mtu: 64 }),
        gatts_attr_tab_size: Some(raw::ble_gatts_cfg_attr_tab_size_t {
            attr_tab_size: 2048,
        }),
        gap_role_count: Some(raw::ble_gap_cfg_role_count_t {
            adv_set_count: 1,
            periph_role_count: 1,
            central_role_count: 0,
            central_sec_count: 0,
            _bitfield_1: raw::ble_gap_cfg_role_count_t::new_bitfield_1(0),
        }),
        gap_device_name: Some(raw::ble_gap_cfg_device_name_t {
            p_value: name.cast_mut(),
            current_len: name_len,
            max_len: name_len,
            write_perm: raw::ble_gap_conn_sec_mode_t {
                _bitfield_1: raw::ble_gap_conn_sec_mode_t::new_bitfield_1(0, 0),
            },
            _bitfield_1: raw::ble_gap_cfg_device_name_t::new_bitfield_1(
                raw::BLE_GATTS_VLOC_STACK as u8,
            ),
        }),
        ..Default::default()
    }
}

/// Initialize the `SoftDevice` and return a mutable reference to it.
///
/// `profile`: active profile selected at boot. Determines GAP name, scan
/// response, manufacturer/model, VID/PID, and HID descriptor.
///
/// # Safety
/// This must be called exactly once at program start, before any BLE operations.
#[must_use]
pub fn init_softdevice(profile: &Profile) -> &'static mut Softdevice {
    let config = softdevice_config(profile);
    Softdevice::enable(&config)
}

/// BLE advertising data for sync mode - General Discoverable.
/// This makes the device visible in Bluetooth menus on Mac/iPhone/etc.
/// Format: [length, type, data...] for each AD structure
#[rustfmt::skip]
static ADV_DATA_SYNC: [u8; 13] = [
    // Flags AD structure
    0x02,              // Length: 2 bytes follow
    0x01,              // AD Type: Flags
    0x06,              // Flags: LE General Discoverable | BR/EDR Not Supported

    // Appearance AD structure (Gamepad = 0x03C4)
    0x03,              // Length: 3 bytes follow
    0x19,              // AD Type: Appearance
    0xC4, 0x03,        // Appearance: Gamepad (0x03C4 little-endian)

    // Complete list of 16-bit service UUIDs
    0x05,              // Length: 5 bytes follow
    0x03,              // AD Type: Complete List of 16-bit Service UUIDs
    0x12, 0x18,        // HID Service (0x1812)
    0x0F, 0x18,        // Battery Service (0x180F)
];

/// BLE advertising data for reconnect mode - NOT discoverable.
/// Only bonded devices can connect via directed advertising.
#[rustfmt::skip]
static ADV_DATA_RECONNECT: [u8; 13] = [
    // Flags AD structure - NOT discoverable
    0x02,              // Length: 2 bytes follow
    0x01,              // AD Type: Flags
    0x04,              // Flags: BR/EDR Not Supported (no discoverable flag)

    // Appearance AD structure (Gamepad = 0x03C4)
    0x03,              // Length: 3 bytes follow
    0x19,              // AD Type: Appearance
    0xC4, 0x03,        // Appearance: Gamepad (0x03C4 little-endian)

    // Complete list of 16-bit service UUIDs
    0x05,              // Length: 5 bytes follow
    0x03,              // AD Type: Complete List of 16-bit Service UUIDs
    0x12, 0x18,        // HID Service (0x1812)
    0x0F, 0x18,        // Battery Service (0x180F)
];

/// Active profile pointer, set at init and read during advertising.
/// Defaults to `PROFILE_XBOX` so calls before `set_profile` still resolve.
static ACTIVE_PROFILE: AtomicPtr<Profile> =
    AtomicPtr::new(&PROFILE_XBOX as *const Profile as *mut Profile);

/// Set the active profile (called once at init before advertising starts).
pub fn set_profile(profile: &'static Profile) {
    ACTIVE_PROFILE.store(
        core::ptr::from_ref::<Profile>(profile).cast_mut(),
        Ordering::Relaxed,
    );
}

/// Get the active profile.
#[must_use]
pub fn get_profile() -> &'static Profile {
    let ptr = ACTIVE_PROFILE.load(Ordering::Relaxed);
    // SAFETY: ACTIVE_PROFILE is initialized to a valid 'static reference and
    // only ever reassigned via set_profile() which takes a 'static reference.
    unsafe { &*ptr }
}

/// Advertising mode determines visibility and connection behavior.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AdvertiseMode {
    /// Sync mode: visible to all devices, fast advertising
    SyncMode,
    /// Fast reconnect: 20ms interval, not discoverable (first 5s after disconnect)
    ReconnectFast,
    /// Reconnect mode: only bonded device can connect (not visible to others)
    Reconnect,
}

/// Tracks last advertise mode to log only on change.
static LAST_ADV_MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0xFF);

/// Start BLE advertising based on mode.
///
/// - `SyncMode`: General Discoverable, visible in Bluetooth menus, accepts any pairing
/// - `Reconnect`: Not discoverable (won't appear in Bluetooth scans), but bonded device can reconnect
///
/// # Errors
/// Returns `peripheral::AdvertiseError` if advertising fails.
pub async fn advertise(
    sd: &'static Softdevice,
    _server: &GamepadServer,
    bonder: &'static Bonder,
    mode: AdvertiseMode,
) -> Result<Connection, peripheral::AdvertiseError> {
    let (adv_data, config, _log_msg) = match mode {
        AdvertiseMode::SyncMode => {
            // Sync mode: Fast advertising, discoverable, no timeout
            let config = peripheral::Config {
                interval: 32, // 32 * 0.625ms = 20ms (fast)
                timeout: None,
                ..Default::default()
            };
            (
                &ADV_DATA_SYNC,
                config,
                "BLE: Advertising (SYNC MODE - discoverable)",
            )
        }
        AdvertiseMode::ReconnectFast => {
            // Fast reconnect: 20ms interval, NOT discoverable (first 5s after disconnect)
            // 10s timeout so the task loop can check elapsed time for sleep
            let config = peripheral::Config {
                interval: 32,        // 32 * 0.625ms = 20ms (fast for quick reconnection)
                timeout: Some(1000), // 1000 * 10ms = 10s
                ..Default::default()
            };
            (
                &ADV_DATA_RECONNECT,
                config,
                "BLE: Advertising (fast reconnect)",
            )
        }
        AdvertiseMode::Reconnect => {
            // Reconnect mode: Slower advertising, NOT discoverable
            // Device won't appear in Bluetooth scans, but bonded devices can still connect
            // 10s timeout so the task loop can check elapsed time for sleep
            let config = peripheral::Config {
                interval: 800,       // 800 * 0.625ms = 500ms (saves ~22uA vs 100ms)
                timeout: Some(1000), // 1000 * 10ms = 10s
                ..Default::default()
            };
            (
                &ADV_DATA_RECONNECT,
                config,
                "BLE: Advertising (reconnect - not discoverable)",
            )
        }
    };

    let scan_data: &[u8] = get_profile().scan_response;

    let adv = peripheral::ConnectableAdvertisement::ScannableUndirected {
        adv_data,
        scan_data,
    };

    // Log only on mode change to reduce spam
    let mode_id = match mode {
        AdvertiseMode::SyncMode => 0,
        AdvertiseMode::ReconnectFast => 1,
        AdvertiseMode::Reconnect => 2,
    };
    if LAST_ADV_MODE.swap(mode_id, core::sync::atomic::Ordering::Relaxed) != mode_id {
        log!("{}", _log_msg);
    }

    peripheral::advertise_pairable(sd, adv, &config, bonder).await
}
