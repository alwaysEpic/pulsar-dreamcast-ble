// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Simple BLE security handler for HID gamepad.
//!
//! Implements "Just Works" pairing without passkey.
//!
//! The bond is **replaced on success, never erased on entry** to a pairing
//! window. Everything a window needs lives in RAM on `Bonder`; the
//! flash record is touched only once a replacement has actually bonded, so an
//! aborted window leaves the previously paired host working.

use core::cell::{Cell, RefCell};
use heapless::Vec;
use nrf_softdevice::ble::gatt_server::{get_sys_attrs, set_sys_attrs, SetSysAttrsError};
use nrf_softdevice::ble::security::{IoCapabilities, SecurityHandler};
use nrf_softdevice::ble::{Connection, EncryptionInfo, IdentityKey, MasterId};
use nrf_softdevice::raw;
use nrf_softdevice::RawError;

/// What a peer has done with security on the current connection.
///
/// The two procedures are distinguishable at the callback level and nowhere
/// else: a peer reusing a stored LTK raises `SEC_INFO_REQUEST` (`get_key`),
/// a peer pairing fresh raises `SEC_PARAMS_REQUEST` (`security_params`). That
/// names the *procedure*, not the peer's intent — a host that still knows us
/// may choose to pair fresh, and it is admitted when it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// Nothing security-related has happened on this connection yet.
    Nothing,
    /// The peer asked us to reuse the stored key.
    OldKeyReuse,
    /// The peer started a fresh pairing.
    FreshPairing,
}

/// Stored bond information for a peer.
#[expect(
    clippy::struct_field_names,
    reason = "the shared prefix mirrors the SoftDevice's own naming for these security parameters"
)]
#[derive(Debug, Clone, Copy)]
struct Peer {
    master_id: MasterId,
    key: EncryptionInfo,
    peer_id: IdentityKey,
}

/// Simple bonder that stores one peer bond in RAM.
/// Bond data is persisted to flash via `flash_bond` module on disconnect.
pub struct Bonder {
    peer: Cell<Option<Peer>>,
    sys_attrs: RefCell<Vec<u8, 64>>,
    sys_attrs_len: Cell<usize>, // Track actual saved length

    /// Open while a replacement window is running. RAM only — the stored bond
    /// is retained for the whole window and replaced only by `on_bonded`,
    /// which is also what closes the window.
    window: Cell<bool>,
    /// What the current connection's peer has done with security.
    observed: Cell<Observed>,
    /// Set when a peer that is not the bonded host starts a fresh pairing
    /// while a bond exists and no window is open. The SoftDevice binding
    /// always answers a pairing request with success, so the pairing itself
    /// cannot be declined: `security_params` starts the disconnect,
    /// `on_bonded` refuses to store the result, and the host service reads
    /// this to ignore the link for the moments it stays up. A Just Works
    /// pairing gives the stranger an encrypted link, and since `HostService`
    /// writes the docked VMU, encryption alone cannot be what admits a peer.
    /// Cleared by the session teardown, before the next advertisement — only
    /// one link exists at a time.
    refused: Cell<bool>,
    /// Holds back attribute snapshots for every session inside a window that
    /// has not bonded. The library calls `save_sys_attrs` on *every* disconnect,
    /// theirs included, and the retained host's CCCDs must not be overwritten
    /// with a refused session's state. Released by `on_bonded` — a peer that
    /// bonded owns the record now — or by `end_window`.
    hold_attrs: Cell<bool>,
    /// The connection handle whose attributes this bonder will save, or `None`
    /// when no session owns them.
    ///
    /// `hold_attrs` alone cannot close the window's last race: `end_window`
    /// releases it, but a refused peer's disconnect is only *started* by
    /// `conn.disconnect()` and its `DISCONNECTED` event can arrive after the
    /// release. The library calls `save_sys_attrs` on that event, and a refused
    /// old host passes the identity guard below — it *is* the retained peer —
    /// so its near-empty attribute read would overwrite a good snapshot. Tying
    /// the permission to a handle ends the race instead of widening a timeout:
    /// a session claims it only once it is real, and the teardown drops it.
    attrs_conn: Cell<Option<u16>>,
    /// Bumped whenever the in-RAM bond record changes — a new bond, or a new
    /// attribute snapshot. `persisted` is the generation flash holds, so
    /// `generation != persisted` is exactly "flash is behind".
    generation: Cell<u32>,
    persisted: Cell<u32>,
}

impl Bonder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            peer: Cell::new(None),
            sys_attrs: RefCell::new(Vec::new()),
            sys_attrs_len: Cell::new(0),
            window: Cell::new(false),
            observed: Cell::new(Observed::Nothing),
            refused: Cell::new(false),
            hold_attrs: Cell::new(false),
            attrs_conn: Cell::new(None),
            generation: Cell::new(0),
            persisted: Cell::new(0),
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
        // What we just loaded *is* what flash holds.
        self.generation.set(1);
        self.persisted.set(1);
    }

    /// Get current bonding data for saving to flash
    pub fn get_bond_data(&self) -> Option<(MasterId, EncryptionInfo, IdentityKey)> {
        self.peer.get().map(|p| (p.master_id, p.key, p.peer_id))
    }

    /// Get current `sys_attrs` for saving
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
    pub const fn has_bond(&self) -> bool {
        self.peer.get().is_some()
    }

    /// Clear all bonding data. Not used on entry to pairing mode — see
    /// `begin_window`, which retains the bond for the window's duration.
    pub fn clear(&self) {
        self.peer.set(None);
        self.sys_attrs.borrow_mut().clear();
        self.sys_attrs_len.set(0);
        self.generation.set(0);
        self.persisted.set(0);
    }

    // --- replacement window  ---------------------------------

    /// Open a replacement window. The stored bond stays exactly where it is;
    /// only a peer that actually bonds will replace it.
    pub fn begin_window(&self) {
        self.window.set(true);
        self.observed.set(Observed::Nothing);
    }

    /// Close the window, whatever the outcome.
    pub fn end_window(&self) {
        self.window.set(false);
        self.observed.set(Observed::Nothing);
        self.hold_attrs.set(false);
    }

    /// Claim attribute saving for this connection. Called once a session is
    /// real — for a window session that is after the bond lands, not at
    /// admission.
    pub fn own_attrs(&self, conn: &Connection) {
        self.attrs_conn.set(conn.handle());
    }

    /// Drop the claim. Runs on every session exit, so a handle the SoftDevice
    /// later reuses cannot inherit the previous session's permission.
    pub fn release_attrs(&self) {
        self.attrs_conn.set(None);
    }

    /// Is a replacement window still waiting for a peer to bond? `on_bonded`
    /// closes it, so this is also how a caller learns the replacement landed.
    pub const fn in_window(&self) -> bool {
        self.window.get()
    }

    /// Arm the observation for the *next* connection of a window, and hold
    /// attribute snapshots.
    ///
    /// **Call this before advertising, not after a connection arrives.** The
    /// pinned event loop (`events.rs:79-100`) drains every queued BLE event in
    /// one poll without yielding, and the handler is installed by the
    /// advertising callback partway through that drain — so a peer's
    /// `SEC_PARAMS_REQUEST` can already have been dispatched by the time
    /// `advertise` hands a `Connection` back. Arming afterwards would erase an
    /// observation that had already been delivered.
    ///
    /// The attribute hold stays on until this peer bonds (`on_bonded`) or the
    /// window ends — being admitted as a candidate is not enough, since a
    /// candidate that fails to pair is still a refused session.
    pub fn arm_provisional(&self) {
        self.observed.set(Observed::Nothing);
        self.hold_attrs.set(true);
    }

    /// What the current peer has done with security.
    pub const fn observed(&self) -> Observed {
        self.observed.get()
    }

    /// Was the current link refused as a stranger's pairing? See `refused`.
    pub const fn refused(&self) -> bool {
        self.refused.get()
    }

    /// Forget the refusal. Only the session teardown calls this, once the
    /// refused link is gone and before anything advertises again.
    pub fn clear_refusal(&self) {
        self.refused.set(false);
    }

    /// Would a fresh pairing from this peer replace a bond outside a window?
    ///
    /// The bonded host re-pairing is still admitted when its address resolves
    /// to the stored identity: some hosts pair fresh rather than reuse a key.
    /// Anyone else must go through the pairing window (the 2 s sync hold).
    fn is_stranger(&self, conn: &Connection) -> bool {
        !self.window.get()
            && self
                .peer
                .get()
                .is_some_and(|peer| !peer.peer_id.is_match(conn.peer_address()))
    }

    // --- persistence bookkeeping ----------------------------------------

    /// The generation a save would be committing.
    pub const fn generation(&self) -> u32 {
        self.generation.get()
    }

    /// Does flash hold something older than RAM?
    pub const fn save_owed(&self) -> bool {
        self.generation.get() != self.persisted.get()
    }

    /// Record that `generation` reached flash and read back intact. A save
    /// whose generation has already been superseded must not be recorded —
    /// that is what keeps a stale write from masking a newer bond.
    pub fn mark_persisted(&self, generation: u32) {
        if generation == self.generation.get() {
            self.persisted.set(generation);
        }
    }

    fn bump_generation(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
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

    /// Reply parameters for an **incoming** pairing request.
    ///
    /// This is the only callback that means "the peer is starting a fresh
    /// pairing", which is why the replacement window watches it. `can_bond`
    /// cannot serve: the pinned `Connection::request_security` calls
    /// `can_bond` locally when *we* ask for security, so it fires for our own
    /// request too and cannot tell the two apart.
    ///
    /// Observing is all this override does. The parameters themselves come
    /// from the library's own default body via [`DefaultParams`], so bumping
    /// the pinned revision cannot leave a stale copy behind here.
    fn security_params(&self, conn: &Connection) -> raw::ble_gap_sec_params_t {
        if self.window.get() {
            self.observed.set(Observed::FreshPairing);
        } else if self.is_stranger(conn) {
            // Not declinable here (see `refused`), so end the link instead.
            // `on_bonded` is the backstop if the pairing completes first.
            self.refused.set(true);
            let _ = conn.disconnect();
        }

        DefaultParams(self).security_params(conn)
    }

    fn on_bonded(
        &self,
        conn: &Connection,
        master_id: MasterId,
        key: EncryptionInfo,
        peer_id: IdentityKey,
    ) {
        // A stranger's pairing outside a window never replaces the bond, even
        // if it completed before the disconnect took effect.
        if self.refused.get() || self.is_stranger(conn) {
            self.refused.set(true);
            let _ = conn.disconnect();
            return;
        }

        self.sys_attrs.borrow_mut().clear();
        self.sys_attrs_len.set(0);
        self.peer.set(Some(Peer {
            master_id,
            key,
            peer_id,
        }));
        self.bump_generation();

        // A replacement has been accepted, so the window's job is done. Closing
        // it here rather than back in the task is what stops `get_key` from
        // refusing the *new* host if it reconnects before the task notices.
        self.window.set(false);
        self.hold_attrs.set(false);

        // Bonding grants the attribute permission *here*, not when the session
        // task next runs. Bond, subscribe and disconnect can all be dispatched
        // in one drain, and the library's disconnect-time `save_sys_attrs` would
        // then be refused for want of a claim — persisting the new keys with an
        // empty snapshot, so the host's subscriptions do not survive its own
        // reconnect. Claiming it from the event that proves the bond removes the
        // dependency on when anything else resumes.
        self.attrs_conn.set(conn.handle());
    }

    fn get_key(&self, conn: &Connection, master_id: MasterId) -> Option<EncryptionInfo> {
        let peer = self.peer.get()?;

        // Match the peer's identity as well as the diversifier. `MasterId` is a
        // value the *central* chose, so it is not ours to treat as an identity:
        // under legacy pairing a collision only costs a failed encryption, but
        // LE Secure Connections sends ediv = 0 / rand = 0 for every peer, at
        // which point the identity is the only thing left that distinguishes
        // the bonded host from anyone else asking for a key.
        if !peer.peer_id.is_match(conn.peer_address()) {
            return None;
        }

        // The stored key is retained through a replacement window but must not
        // be *reused* inside one: a host that still holds it would otherwise
        // re-encrypt and take the very session the window exists to offer
        // someone else. Returning `None` fails its encryption; the provisional
        // stage reads this and disconnects it explicitly rather than leaving it
        // stranded mid-procedure.
        if self.window.get() {
            self.observed.set(Observed::OldKeyReuse);
            return None;
        }

        (master_id == peer.master_id).then_some(peer.key)
    }

    fn save_sys_attrs(&self, conn: &Connection) {
        // Provisional and rejected sessions never touch the retained host's
        // attributes. The library calls this on their disconnect too.
        if self.hold_attrs.get() {
            return;
        }

        // Only the session that claimed the attributes may write them. This is
        // what stops a refused peer's late `DISCONNECTED` — dispatched after
        // `end_window` released `hold_attrs` — from saving over the retained
        // host's snapshot.
        // Compared through `Some`, deliberately: `conn.handle()` is `None` once
        // the link is gone, and an unclaimed `attrs_conn` is `None` too, so a
        // bare `!=` would let an unowned dead connection through.
        let Some(owner) = self.attrs_conn.get() else {
            return;
        };
        if conn.handle() != Some(owner) {
            return;
        }

        let Some(peer) = self.peer.get() else { return };
        if !peer.peer_id.is_match(conn.peer_address()) {
            return;
        }

        // Read into scratch, and only replace the stored snapshot once the read
        // has actually succeeded. This is called more than once per session: the
        // pinned library calls it from `on_disconnected` (while the handle is
        // still live, so it succeeds), and `ble_task` calls it again after the
        // link is down, where `get_sys_attrs` fails. The previous shape cleared
        // and zero-filled the stored buffer *before* finding that out, so the
        // failing call destroyed the snapshot the successful one had just saved
        // and the peer's CCCDs were lost on every reconnect.
        let mut scratch = [0u8; 64];
        let Ok(len) = get_sys_attrs(conn, &mut scratch) else {
            return;
        };
        let Some(read) = scratch.get(..len) else {
            return;
        };

        let mut sys_attrs = self.sys_attrs.borrow_mut();
        if sys_attrs.as_slice() == read {
            // Nothing changed, so nothing is owed to flash. Without this an
            // idle reconnect would rewrite the page on every disconnect.
            return;
        }

        sys_attrs.clear();
        if sys_attrs.extend_from_slice(read).is_ok() {
            self.sys_attrs_len.set(len);
        } else {
            self.sys_attrs_len.set(0);
        }
        drop(sys_attrs);
        self.bump_generation();
    }

    fn load_sys_attrs(&self, conn: &Connection) {
        let addr = conn.peer_address();
        let attrs = self.sys_attrs.borrow();
        let saved_len = self.sys_attrs_len.get();
        let is_bonded_peer = self
            .peer
            .get()
            .is_some_and(|peer| peer.peer_id.is_match(addr));

        let attrs_slice = if is_bonded_peer && saved_len > 0 {
            Some(&attrs.as_slice()[..saved_len])
        } else {
            None
        };
        let had_snapshot = attrs_slice.is_some();

        let restored = set_sys_attrs(conn, attrs_slice);
        drop(attrs);

        // The result used to be discarded, and a table the SoftDevice refused to
        // initialise answers every notify with `SysAttrMissing` — which the
        // notify budget treats as "not yet", so the session would sit there
        // forever.
        //
        // Only `NRF_ERROR_INVALID_DATA` is a verdict on the snapshot — S140
        // documents it as "the data should be exactly the same as retrieved with
        // `sd_ble_gatts_sys_attr_get`". Everything else it can return describes
        // the link (`BLE_ERROR_INVALID_CONN_HANDLE`, `INVALID_STATE` — a link
        // that dropped before the library saw its disconnect answers with these,
        // not `Disconnected`) or the call, and deleting a good snapshot for them
        // would reconnect its host with no CCCDs and so no reports.
        let restored = match restored {
            Err(SetSysAttrsError::Raw(RawError::InvalidData)) if had_snapshot => {
                // The stored snapshot was refused — most plausibly one taken
                // under a different attribute table, which a firmware update can
                // hand us. It would be refused again on every reconnect, so drop
                // it and start this link clean: the host re-subscribes (BlueZ
                // rewrites its CCCDs on every reconnect) and its fresh state is
                // saved in place of the old.
                log!("BLE: Stored attributes refused as invalid, starting clean");
                self.sys_attrs.borrow_mut().clear();
                self.sys_attrs_len.set(0);
                self.bump_generation();
                set_sys_attrs(conn, None)
            }
            other => other,
        };

        // No table on this link — the snapshot failed for a reason that is not
        // its own, or even an empty table was refused. Nothing here can work, so
        // end it rather than hold the connection with every notify refused; the
        // snapshot is kept for the next one. On a link that is already gone
        // this is a no-op.
        if let Err(SetSysAttrsError::Raw(_err)) = restored {
            log!(
                "BLE: Attributes not initialised ({:?}), disconnecting",
                _err
            );
            let _ = conn.disconnect();
        }
    }
}

/// Borrows a [`Bonder`] solely to reach `SecurityHandler`'s **default**
/// `security_params` body.
///
/// `Bonder` overrides `security_params` so a replacement window can see an
/// incoming pairing request, and a default method cannot be called from its own
/// override. Copying the default's body would work until the pinned revision
/// moved under it. This is a second implementer that deliberately does *not*
/// override `security_params`: it forwards the four inputs that body reads and
/// inherits everything else, so the library remains the single definition of
/// what the parameters are.
struct DefaultParams<'a>(&'a Bonder);

impl SecurityHandler for DefaultParams<'_> {
    fn io_capabilities(&self) -> IoCapabilities {
        self.0.io_capabilities()
    }

    fn can_recv_out_of_band(&self, conn: &Connection) -> bool {
        self.0.can_recv_out_of_band(conn)
    }

    fn can_bond(&self, conn: &Connection) -> bool {
        self.0.can_bond(conn)
    }

    fn request_mitm_protection(&self, conn: &Connection) -> bool {
        self.0.request_mitm_protection(conn)
    }
}
