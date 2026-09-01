// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Prefs journal driver: the page at `0xF2000` as an append-only slot
//! journal (remap design v2 §3, ADR-016).
//!
//! All layout, validation and scan logic lives host-tested in
//! `maple_protocol::prefs_journal`; this module only reads the
//! memory-mapped page and drives the SoftDevice flash for writes. Saves
//! append — the page is erased only when all 128 slots are used, so an
//! interrupted or failed write leaves the previous record effective.

#![expect(
    clippy::missing_errors_doc,
    reason = "internal API; the error type is the unit flash failure the config \
              protocol reports as its Flash status"
)]

use embedded_storage_async::nor_flash::NorFlash;
use maple_protocol::prefs_journal::{self, StoredPrefs, PAGE_SIZE, SLOT_SIZE};
use maple_protocol::remap::RemapTable;
use nrf_softdevice::Flash;

use crate::ble::profile::ProfileId;

/// Flash address of the prefs page (one page below bond data; see
/// `flash_bond.rs` for the app-data window layout).
const PREFS_FLASH_ADDR: u32 = 0x000F_2000;

/// Bytes of slot header (magic + integrity) written after the body.
const SLOT_HEADER_LEN: usize = 8;

/// The memory-mapped prefs page.
const fn page() -> &'static [u8; PAGE_SIZE] {
    // SAFETY: PREFS_FLASH_ADDR is a fixed, page-aligned internal-flash
    // address inside the nRF52840 memory map's code region, which is
    // memory-mapped for reads; the page is PAGE_SIZE bytes by the flash
    // geometry, lives for the life of the program, and u8 has no alignment
    // requirement. Concurrent flash *writes* go through the SoftDevice and
    // complete before any caller re-reads (save re-reads only after its
    // write future resolves).
    unsafe { &*(PREFS_FLASH_ADDR as *const [u8; PAGE_SIZE]) }
}

/// Everything the boot decides from the prefs page.
#[derive(Clone, Copy)]
pub struct LoadedPrefs {
    pub profile_id: ProfileId,
    pub remap: RemapTable,
    /// Where the record came from (`prefs_journal::source`), reported over
    /// the config service's `Info`.
    pub source: u8,
}

/// Scan the journal.
///
/// Defaults (Xbox, `RemapTable::DEFAULT`) apply when the page holds nothing
/// loadable — including after an interrupted compaction erase, which is the
/// one window (§3) that can lose the record.
#[must_use]
pub fn load_prefs() -> LoadedPrefs {
    let scan = prefs_journal::scan(page());
    scan.prefs.map_or(
        LoadedPrefs {
            profile_id: ProfileId::Xbox,
            remap: RemapTable::DEFAULT,
            source: scan.source,
        },
        |prefs| LoadedPrefs {
            profile_id: match prefs.profile_id {
                1 => ProfileId::Generic,
                _ => ProfileId::Xbox,
            },
            remap: prefs.remap,
            source: scan.source,
        },
    )
}

/// Append one record; compact (erase, rewrite into slot 0) only when the
/// journal is full.
///
/// Body first, header last, then a read-back compare — `Ok` here is what
/// lets the protocol's `Complete` mean the save is real.
pub async fn save_prefs(
    flash: &mut Flash,
    profile_id: ProfileId,
    remap: &RemapTable,
) -> Result<(), ()> {
    let record = prefs_journal::encode_slot(&StoredPrefs {
        profile_id: profile_id as u8,
        remap: *remap,
    });

    let slot = if let Some(slot) = prefs_journal::scan(page()).first_free {
        slot
    } else {
        // Compaction: the only erase in the journal's life cycle. An
        // interruption between here and the header write below loses the
        // record to defaults — reachable once per 128 commits.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "PAGE_SIZE is the compile-time constant 4096"
        )]
        let page_len = PAGE_SIZE as u32;
        flash
            .erase(PREFS_FLASH_ADDR, PREFS_FLASH_ADDR + page_len)
            .await
            .map_err(|_| ())?;
        0
    };

    #[expect(
        clippy::cast_possible_truncation,
        reason = "slot < 128 and SLOT_SIZE = 32, so the byte offset is at most 4064, \
                  far inside u32"
    )]
    let base = PREFS_FLASH_ADDR + (slot * SLOT_SIZE) as u32;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "SLOT_HEADER_LEN is the compile-time constant 8"
    )]
    let header_len = SLOT_HEADER_LEN as u32;
    // Body first, header (integrity + magic) last: a save interrupted at
    // any point leaves a slot that can never decode, and the previous slot
    // stays effective.
    flash
        .write(base + header_len, &record[SLOT_HEADER_LEN..])
        .await
        .map_err(|_| ())?;
    flash
        .write(base, &record[..SLOT_HEADER_LEN])
        .await
        .map_err(|_| ())?;

    // Read back and compare through the same memory-mapped view a boot
    // scan will use.
    if page()[slot * SLOT_SIZE..(slot + 1) * SLOT_SIZE] == record {
        Ok(())
    } else {
        Err(())
    }
}

/// A profile save is a journal append through the same path (§3, retiring
/// the old erase-per-save): it carries the current effective map forward.
pub async fn save_profile(flash: &mut Flash, profile_id: ProfileId) -> Result<(), ()> {
    let current = load_prefs();
    save_prefs(flash, profile_id, &current.remap).await
}
