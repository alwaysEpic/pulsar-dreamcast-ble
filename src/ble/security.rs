//! Simple BLE security handler for HID gamepad.
//!
//! Implements "Just Works" pairing without passkey.

use core::cell::{Cell, RefCell};
use heapless::Vec;
use nrf_softdevice::ble::gatt_server::{get_sys_attrs, set_sys_attrs};
use nrf_softdevice::ble::security::{IoCapabilities, SecurityHandler};
use nrf_softdevice::ble::{Connection, EncryptionInfo, IdentityKey, MasterId};

/// Stored bond information for a peer.
#[derive(Debug, Clone, Copy)]
struct Peer {
    master_id: MasterId,
    key: EncryptionInfo,
    peer_id: IdentityKey,
}

/// Simple bonder that stores one peer bond in RAM.
/// In a real product, you'd store this in flash.
pub struct Bonder {
    peer: Cell<Option<Peer>>,
    sys_attrs: RefCell<Vec<u8, 64>>,
    sys_attrs_len: Cell<usize>, // Track actual saved length
}

impl Bonder {
    pub const fn new() -> Self {
        Self {
            peer: Cell::new(None),
            sys_attrs: RefCell::new(Vec::new()),
            sys_attrs_len: Cell::new(0),
        }
    }

    /// Initialize bonder with data loaded from flash
    pub fn load_from_flash(
        &self,
        master_id: MasterId,
        key: EncryptionInfo,
        peer_id: IdentityKey,
        sys_attrs_data: &[u8],
    ) {
        self.peer.set(Some(Peer {
            master_id,
            key,
            peer_id,
        }));
        let mut attrs = self.sys_attrs.borrow_mut();
        attrs.clear();
        attrs.extend_from_slice(sys_attrs_data).ok();
        self.sys_attrs_len.set(sys_attrs_data.len());
    }

    /// Get current bonding data for saving to flash
    pub fn get_bond_data(&self) -> Option<(MasterId, EncryptionInfo, IdentityKey)> {
        self.peer.get().map(|p| (p.master_id, p.key, p.peer_id))
    }

    /// Get current sys_attrs for saving
    pub fn get_sys_attrs(&self) -> heapless::Vec<u8, 64> {
        let attrs = self.sys_attrs.borrow();
        let len = self.sys_attrs_len.get();
        let mut result: heapless::Vec<u8, 64> = heapless::Vec::new();
        if len > 0 && len <= attrs.len() {
            result.extend_from_slice(&attrs[..len]).ok();
        }
        result
    }

    /// Check if we have bonding data that should be saved
    pub fn has_bond(&self) -> bool {
        self.peer.get().is_some()
    }
}

impl Default for Bonder {
    fn default() -> Self {
        Self::new()
    }
}

impl SecurityHandler for Bonder {
    fn io_capabilities(&self) -> IoCapabilities {
        // No input/output - use "Just Works" pairing
        IoCapabilities::None
    }

    fn can_bond(&self, _conn: &Connection) -> bool {
        true
    }

    fn display_passkey(&self, _passkey: &[u8; 6]) {
        // Just Works pairing - no passkey display needed
    }

    fn on_bonded(
        &self,
        _conn: &Connection,
        master_id: MasterId,
        key: EncryptionInfo,
        peer_id: IdentityKey,
    ) {
        self.sys_attrs.borrow_mut().clear();
        self.sys_attrs_len.set(0);
        self.peer.set(Some(Peer {
            master_id,
            key,
            peer_id,
        }));
    }

    fn get_key(&self, _conn: &Connection, master_id: MasterId) -> Option<EncryptionInfo> {
        self.peer
            .get()
            .and_then(|peer| (master_id == peer.master_id).then_some(peer.key))
    }

    fn save_sys_attrs(&self, conn: &Connection) {
        if let Some(peer) = self.peer.get() {
            if peer.peer_id.is_match(conn.peer_address()) {
                let mut sys_attrs = self.sys_attrs.borrow_mut();
                let capacity = sys_attrs.capacity();
                sys_attrs.clear();
                sys_attrs.resize(capacity, 0).ok();
                if let Ok(len) = get_sys_attrs(conn, &mut sys_attrs) {
                    self.sys_attrs_len.set(len);
                } else {
                    self.sys_attrs_len.set(0);
                }
            }
        }
    }

    fn load_sys_attrs(&self, conn: &Connection) {
        let addr = conn.peer_address();
        let attrs = self.sys_attrs.borrow();
        let saved_len = self.sys_attrs_len.get();
        let is_bonded_peer = self
            .peer
            .get()
            .map(|peer| peer.peer_id.is_match(addr))
            .unwrap_or(false);

        let attrs_slice = if is_bonded_peer && saved_len > 0 {
            Some(&attrs.as_slice()[..saved_len])
        } else {
            None
        };

        let _ = set_sys_attrs(conn, attrs_slice);
    }
}
