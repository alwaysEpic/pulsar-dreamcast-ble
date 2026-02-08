//! SoftDevice initialization and BLE advertising.

use nrf_softdevice::ble::{gatt_server, peripheral, Connection};
use nrf_softdevice::{raw, Softdevice};
use rtt_target::rprintln;

use crate::ble::hid::GamepadServer;
use crate::ble::security::Bonder;

/// SoftDevice configuration for BLE peripheral mode.
fn softdevice_config() -> nrf_softdevice::Config {
    nrf_softdevice::Config {
        clock: Some(raw::nrf_clock_lf_cfg_t {
            source: raw::NRF_CLOCK_LF_SRC_RC as u8,
            rc_ctiv: 16,
            rc_temp_ctiv: 2,
            accuracy: raw::NRF_CLOCK_LF_ACCURACY_500_PPM as u8,
        }),
        conn_gap: Some(raw::ble_gap_conn_cfg_t {
            conn_count: 1,
            event_length: 24, // 24 * 1.25ms = 30ms
        }),
        conn_gatt: Some(raw::ble_gatt_conn_cfg_t { att_mtu: 64 }),
        gatts_attr_tab_size: Some(raw::ble_gatts_cfg_attr_tab_size_t {
            attr_tab_size: 1024,
        }),
        gap_role_count: Some(raw::ble_gap_cfg_role_count_t {
            adv_set_count: 1,
            periph_role_count: 1,
            central_role_count: 0,
            central_sec_count: 0,
            _bitfield_1: raw::ble_gap_cfg_role_count_t::new_bitfield_1(0),
        }),
        gap_device_name: Some(raw::ble_gap_cfg_device_name_t {
            p_value: b"DC Controller\0" as *const u8 as _,
            current_len: 13,
            max_len: 13,
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

/// Initialize the SoftDevice and return a mutable reference to it.
///
/// # Safety
/// This must be called exactly once at program start, before any BLE operations.
pub fn init_softdevice() -> &'static mut Softdevice {
    let config = softdevice_config();
    Softdevice::enable(&config)
}

/// BLE advertising data (raw bytes).
/// Format: [length, type, data...] for each AD structure
/// Length = size of (type + data), NOT including length byte itself
#[rustfmt::skip]
static ADV_DATA: [u8; 13] = [
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

/// Scan response with device name.
#[rustfmt::skip]
static SCAN_DATA: [u8; 17] = [
    // Complete Local Name
    0x10,              // Length: 16 bytes follow (1 type + 15 name chars)
    0x09,              // AD Type: Complete Local Name
    b'D', b'C', b' ', b'C', b'o', b'n', b't', b'r', b'o', b'l', b'l', b'e', b'r', b' ', b' ',
];

/// Start BLE advertising as HID Gamepad with security/bonding support.
pub async fn advertise(
    sd: &'static Softdevice,
    _server: &GamepadServer,
    bonder: &'static Bonder,
) -> Result<Connection, peripheral::AdvertiseError> {
    let config = peripheral::Config::default();
    let adv = peripheral::ConnectableAdvertisement::ScannableUndirected {
        adv_data: &ADV_DATA,
        scan_data: &SCAN_DATA,
    };

    rprintln!("BLE: Advertising (pairable)...");
    peripheral::advertise_pairable(sd, adv, &config, bonder).await
}

/// Run the GATT server for a connection.
#[allow(dead_code)]
pub async fn run_gatt_server(
    _sd: &'static Softdevice,
    conn: Connection,
    server: &GamepadServer,
) {
    rprintln!("BLE: Client connected");

    let err = gatt_server::run(&conn, server, |event| {
        // Handle GATT events (read/write requests)
        match event {
            crate::ble::hid::GamepadServerEvent::Hid(_) => {}
            crate::ble::hid::GamepadServerEvent::DeviceInfo(_) => {}
            crate::ble::hid::GamepadServerEvent::Battery(_) => {}
        }
    })
    .await;

    rprintln!("BLE: GATT server ended: {:?}", err);
}
