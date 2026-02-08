//! GATT service for Dreamcast controller state.
//!
//! Exposes controller state as a BLE characteristic with read and notify support.

use nrf_softdevice::ble::gatt_server;

// UUIDs are defined inline in the gatt_service/gatt_server macros

/// GATT server for the Dreamcast controller.
#[nrf_softdevice::gatt_server]
pub struct ControllerServer {
    /// Controller service with state characteristic.
    pub controller: ControllerService,
}

/// Controller service containing the state characteristic.
#[nrf_softdevice::gatt_service(uuid = "12340000-1234-5678-1234-56789abcdef0")]
pub struct ControllerService {
    /// Controller state characteristic (12 bytes).
    ///
    /// Format:
    /// - bytes 0-1: buttons (u16 little-endian, bit=1 means pressed)
    /// - byte 2: left trigger (0-255)
    /// - byte 3: right trigger (0-255)
    /// - byte 4: stick X (0-255, 128 = center)
    /// - byte 5: stick Y (0-255, 128 = center)
    /// - bytes 6-11: reserved
    #[characteristic(uuid = "12340001-1234-5678-1234-56789abcdef0", read, notify)]
    pub state: [u8; 12],
}

impl ControllerServer {
    /// Update the controller state characteristic and notify connected clients.
    pub fn update_state(
        &self,
        conn: &nrf_softdevice::ble::Connection,
        state: &[u8; 12],
    ) -> Result<(), gatt_server::NotifyValueError> {
        // Set the value first (ignore error, notify is what matters)
        let _ = self.controller.state_set(state);
        self.controller.state_notify(conn, state)
    }

    /// Set the controller state without notifying (for initial value).
    pub fn set_state(&self, state: &[u8; 12]) -> Result<(), gatt_server::SetValueError> {
        self.controller.state_set(state)
    }
}
