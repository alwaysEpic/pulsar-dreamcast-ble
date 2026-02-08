//! Simple BLE security handler for HID gamepad.
//!
//! Implements "Just Works" pairing without passkey.

use core::cell::{Cell, RefCell};
use heapless::Vec;
use nrf_softdevice::ble::gatt_server::{get_sys_attrs, set_sys_attrs};
use nrf_softdevice::ble::security::{IoCapabilities, SecurityHandler};
use nrf_softdevice::ble::{Connection, EncryptionInfo, IdentityKey, MasterId};
use rtt_target::rprintln;

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
}

impl Bonder {
    pub const fn new() -> Self {
        Self {
            peer: Cell::new(None),
            sys_attrs: RefCell::new(Vec::new()),
        }
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

    fn display_passkey(&self, passkey: &[u8; 6]) {
        rprintln!("BLE: Passkey: {:?}", passkey);
    }

    fn on_bonded(
        &self,
        _conn: &Connection,
        master_id: MasterId,
        key: EncryptionInfo,
        peer_id: IdentityKey,
    ) {
        rprintln!("BLE: Bonded with peer (EDIV=0x{:04X})", master_id.ediv);
        self.sys_attrs.borrow_mut().clear();
        self.peer.set(Some(Peer {
            master_id,
            key,
            peer_id,
        }));
    }

    fn get_key(&self, _conn: &Connection, master_id: MasterId) -> Option<EncryptionInfo> {
        let result = self.peer
            .get()
            .and_then(|peer| (master_id == peer.master_id).then_some(peer.key));
        if result.is_some() {
            rprintln!("BLE: Found stored key for EDIV=0x{:04X}", master_id.ediv);
        } else {
            rprintln!("BLE: No key found for EDIV=0x{:04X}", master_id.ediv);
        }
        result
    }

    fn save_sys_attrs(&self, conn: &Connection) {
        if let Some(peer) = self.peer.get() {
            if peer.peer_id.is_match(conn.peer_address()) {
                let mut sys_attrs = self.sys_attrs.borrow_mut();
                let capacity = sys_attrs.capacity();
                sys_attrs.resize(capacity, 0).ok();
                if let Ok(len) = get_sys_attrs(conn, &mut sys_attrs) {
                    sys_attrs.truncate(len);
                    rprintln!("BLE: Saved {} bytes of sys_attrs", len);
                }
            } else {
                rprintln!("BLE: Peer address mismatch, not saving sys_attrs");
            }
        } else {
            rprintln!("BLE: No bonded peer, not saving sys_attrs");
        }
    }

    fn load_sys_attrs(&self, conn: &Connection) {
        let addr = conn.peer_address();
        let attrs = self.sys_attrs.borrow();
        let is_bonded_peer = self
            .peer
            .get()
            .map(|peer| peer.peer_id.is_match(addr))
            .unwrap_or(false);

        let attrs = if is_bonded_peer {
            if !attrs.is_empty() {
                rprintln!("BLE: Loading {} bytes of sys_attrs for bonded peer", attrs.len());
                Some(attrs.as_slice())
            } else {
                rprintln!("BLE: Bonded peer but no stored sys_attrs");
                None
            }
        } else {
            rprintln!("BLE: New peer, no sys_attrs to load");
            None
        };

        if let Err(e) = set_sys_attrs(conn, attrs) {
            rprintln!("BLE: Failed to set sys_attrs: {:?}", e);
        }
    }
}
