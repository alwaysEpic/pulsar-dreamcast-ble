//! Flash-based bonding storage.
//!
//! Stores BLE bonding data in the last flash page so it persists across power cycles.

use embedded_storage_async::nor_flash::NorFlash;
use nrf_softdevice::ble::{Address, EncryptionInfo, IdentityKey, IdentityResolutionKey, MasterId};
use nrf_softdevice::Flash;

/// Flash address for bonding storage (last page before 1MB boundary)
const BOND_FLASH_ADDR: u32 = 0x000FE000;

/// Flash page size
const PAGE_SIZE: u32 = 4096;

/// Magic number to identify valid bonding data
const BOND_MAGIC: u32 = 0xB00D_DA7A;

/// Stored bonding data structure (must be 4-byte aligned for flash writes)
#[repr(C, align(4))]
#[derive(Clone, Copy)]
pub struct StoredBond {
    magic: u32,
    /// MasterId.ediv
    ediv: u16,
    /// Padding for alignment
    _pad1: u16,
    /// MasterId.rand (8 bytes)
    rand: [u8; 8],
    /// EncryptionInfo.ltk (16 bytes)
    ltk: [u8; 16],
    /// EncryptionInfo.flags
    enc_flags: u8,
    /// Address flags
    addr_flags: u8,
    /// Padding
    _pad2: u16,
    /// Address bytes (6 bytes)
    addr_bytes: [u8; 6],
    /// Padding
    _pad3: u16,
    /// IRK (16 bytes)
    irk: [u8; 16],
    /// sys_attrs length
    sys_attrs_len: u8,
    /// Padding
    _pad4: [u8; 3],
    /// sys_attrs data (64 bytes max)
    sys_attrs: [u8; 64],
}

impl StoredBond {
    /// Check if the stored data is valid
    pub fn is_valid(&self) -> bool {
        self.magic == BOND_MAGIC
    }
}

/// Read bonding data from flash
pub fn load_bond() -> Option<(MasterId, EncryptionInfo, IdentityKey, &'static [u8])> {
    // Safety: reading from flash memory is safe
    let stored = unsafe { &*(BOND_FLASH_ADDR as *const StoredBond) };

    if !stored.is_valid() {
        return None;
    }

    let master_id = MasterId {
        ediv: stored.ediv,
        rand: stored.rand,
    };

    let enc_info = EncryptionInfo {
        ltk: stored.ltk,
        flags: stored.enc_flags,
    };

    // Reconstruct Address directly from stored fields
    let addr = Address {
        flags: stored.addr_flags,
        bytes: stored.addr_bytes,
    };

    // Reconstruct IdentityResolutionKey via transmute (repr(C) with just [u8;16])
    let irk: IdentityResolutionKey =
        unsafe { core::mem::transmute::<[u8; 16], IdentityResolutionKey>(stored.irk) };

    let peer_id = IdentityKey { irk, addr };

    // Return sys_attrs slice
    let sys_attrs_len = stored.sys_attrs_len as usize;
    let sys_attrs = if sys_attrs_len > 0 && sys_attrs_len <= 64 {
        unsafe { core::slice::from_raw_parts(stored.sys_attrs.as_ptr(), sys_attrs_len) }
    } else {
        &[]
    };

    Some((master_id, enc_info, peer_id, sys_attrs))
}

/// Save bonding data to flash
pub async fn save_bond(
    flash: &mut Flash,
    master_id: &MasterId,
    enc_info: &EncryptionInfo,
    peer_id: &IdentityKey,
    sys_attrs: &[u8],
) -> Result<(), ()> {
    // Prepare the data structure
    let mut stored = StoredBond {
        magic: BOND_MAGIC,
        ediv: master_id.ediv,
        _pad1: 0,
        rand: master_id.rand,
        ltk: enc_info.ltk,
        enc_flags: enc_info.flags,
        addr_flags: peer_id.addr.flags,
        _pad2: 0,
        addr_bytes: peer_id.addr.bytes,
        _pad3: 0,
        irk: unsafe { core::mem::transmute::<IdentityResolutionKey, [u8; 16]>(peer_id.irk) },
        sys_attrs_len: sys_attrs.len().min(64) as u8,
        _pad4: [0u8; 3],
        sys_attrs: [0u8; 64],
    };
    let copy_len = sys_attrs.len().min(64);
    stored.sys_attrs[..copy_len].copy_from_slice(&sys_attrs[..copy_len]);

    // Erase the flash page first
    flash
        .erase(BOND_FLASH_ADDR, BOND_FLASH_ADDR + PAGE_SIZE)
        .await
        .map_err(|_| ())?;

    // Write the data
    let data = unsafe {
        core::slice::from_raw_parts(
            &stored as *const StoredBond as *const u8,
            core::mem::size_of::<StoredBond>(),
        )
    };

    flash.write(BOND_FLASH_ADDR, data).await.map_err(|_| ())?;

    Ok(())
}
