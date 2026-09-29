// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! `SoftDevice` initialization and BLE advertising.

use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use nrf_softdevice::ble::{peripheral, Address, AddressType, Connection, TxPower};
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

#[expect(
    clippy::match_same_arms,
    reason = "the arms are distinct SoftDevice events that happen to need the same handling; merging them would hide which events are covered"
)]
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
#[expect(
    clippy::cast_possible_truncation,
    reason = "SoftDevice FFI constants are small enum discriminants and length values that fit the narrower types"
)]
fn softdevice_config(gap_name: &'static [u8]) -> nrf_softdevice::Config {
    let name = gap_name.as_ptr();
    // Subtract trailing NUL — SoftDevice expects unterminated length.
    let name_len = (gap_name.len() - 1) as u16;

    nrf_softdevice::Config {
        // LFCLK = the module's 32.768 kHz crystal. Verified against the Seeed
        // XIAO nRF52840 schematic v1.1 (sheet 3: X1 32.768KHz on P0.00/XL1 +
        // P0.01/XL2, C7 10 pF) — the crystal IS populated. The Adafruit
        // bootloader runs LFRC, but that is a software default, not a hardware
        // constraint; either source works here. History (2026-08-04/05): this
        // config was flip-flopped RC→XTAL→RC→XTAL chasing a rate regression
        // that turned out to be binary-layout timing variance (the compiled-
        // timing lottery) — the clock source was never
        // the cause. XTAL is kept for its real 20 ppm accuracy and because the
        // capture-validated v209 binary runs it.
        clock: Some(raw::nrf_clock_lf_cfg_t {
            source: raw::NRF_CLOCK_LF_SRC_XTAL as u8,
            rc_ctiv: 0, // must be 0 for non-RC sources
            rc_temp_ctiv: 0,
            accuracy: raw::NRF_CLOCK_LF_ACCURACY_20_PPM as u8,
        }),
        conn_gap: Some(raw::ble_gap_conn_cfg_t {
            conn_count: 1,
            event_length: 6, // Allow short events for fast intervals
        }),
        // 247, up from the 64 set in the first HID commit (`01b9883`) with no
        // recorded reason. A VMU frame is 192 bytes and an ATT write command
        // spends 3 on the opcode and handle, so 195 is the floor for pushing a
        // whole frame in one write; 247 is the largest MTU that
        // still fits a 251-byte link-layer payload, so asking for more would
        // only fragment.
        //
        // The MTU is a ceiling, not a promise: the host proposes its own in the
        // Exchange MTU procedure and the smaller of the two wins. A sender that
        // ends up under 195 falls back to four 49-byte row writes, which is why
        // that shape exists and must keep working — 0.5.0 and earlier
        // requested an MTU of 64, and a host may still offer less than 195.
        //
        // Without Data Length Extension a 192-byte write still fragments into
        // 27-byte LL packets — ~8 packets, ~2 connection events. That is fine at
        // LCD rates and deliberately not "fixed" with DLE: longer LL packets
        // mean longer connection events, and the Maple poll lives in the quiet
        // gap between them (see the poll pacer in main.rs). Do not raise this
        // without a capture.
        conn_gatt: Some(raw::ble_gatt_conn_cfg_t { att_mtu: 247 }),
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
        // GAP + GATT must end at handle 0x0008, as on a real Xbox One S, so our
        // services land on its handles (see `HidService`). That
        // means GAP without the Central Address Resolution characteristic (a
        // peripheral has no use for it) and GATT without Service Changed. The
        // cost of the second: a host that bonded before a GATT layout change
        // keeps a stale table and must re-pair — as a real pad's hosts do.
        gap_car_incl: Some(raw::ble_gap_cfg_car_incl_cfg_t {
            include_cfg: raw::BLE_GAP_CHAR_INCL_CONFIG_EXCLUDE_WITHOUT_SPACE as u8,
        }),
        gatts_service_changed: Some(raw::ble_gatts_cfg_service_changed_t {
            _bitfield_1: raw::ble_gatts_cfg_service_changed_t::new_bitfield_1(0),
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
    let config = softdevice_config(profile.gap_name);
    let sd = Softdevice::enable(&config);
    if let Some(prefix) = profile.public_prefix {
        // Keep the chip's own low three bytes so units still differ from each
        // other; `Address` bytes are little-endian, so the prefix is the top three.
        let own = nrf_softdevice::ble::get_address(sd).bytes();
        let bytes = [own[0], own[1], own[2], prefix[2], prefix[1], prefix[0]];
        nrf_softdevice::ble::set_address(sd, &Address::new(AddressType::Public, bytes));
    }
    sd
}

/// Initialize the SoftDevice for the isolated configuration personality.
///
/// This must be selected before enable because the GAP name is part of the
/// SoftDevice configuration and cannot be swapped with the runtime GATT
/// server. The alternate address is installed separately immediately after
/// enable and before any advertising.
#[must_use]
pub fn init_config_softdevice() -> &'static mut Softdevice {
    const CONFIG_GAP_NAME: &[u8] = b"Pulsar Configure\0";
    let config = softdevice_config(CONFIG_GAP_NAME);
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
static ACTIVE_PROFILE: AtomicPtr<Profile> = AtomicPtr::new((&raw const PROFILE_XBOX).cast_mut());

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

/// Transmit power for advertising — and so for the connection, which S140
/// documents as inheriting the power of the advertiser that led to it.
///
/// Nothing set this before 0.6.0: advertising took the 0 dBm default and
/// the connection inherited it, on a part that offers +8. The cost is TX current during a
/// 16-byte notify every ~11 ms — a fraction of a milliamp averaged, against a
/// 5 V boost feeding a Dreamcast controller.
pub const TX_POWER: TxPower = TxPower::Plus8dBm;

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
                tx_power: TX_POWER,
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
                tx_power: TX_POWER,
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
                tx_power: TX_POWER,
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
