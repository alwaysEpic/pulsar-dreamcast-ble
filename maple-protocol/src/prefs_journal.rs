// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! The prefs page at `0xF2000` as an append-only slot journal (remap design
//! v2 §3, gate G12).
//!
//! Pure codec and scan logic over a page snapshot — internal flash is
//! memory-mapped, so the firmware hands this module the page as a byte
//! slice and only the write/erase side touches a driver. Old/new redundancy
//! lives *inside* the page: each save appends a 32-byte record to the first
//! free slot, the highest-indexed valid record is effective, and the page
//! is erased only at compaction — so a torn or failed write leaves the
//! previous slot effective, which is what lets the protocol report `Flash`
//! without having destroyed anything.
//!
//! Slot layout (`repr` by hand — these offsets are flash ABI):
//! `magic: u32` at 0, `integrity: u32` at 4 (FNV-1a over bytes 8..32 —
//! named for what it is, not "crc"), `schema: u8` at 8, `profile_id: u8`
//! at 9, pad at 10..12, `remap: RemapTable` at 12..32. A slot is written
//! body first, header last (per `StoredBond` V2), so a torn slot can never
//! be valid.

use crate::remap::RemapTable;

/// One erase page.
pub const PAGE_SIZE: usize = 4096;
/// One record.
pub const SLOT_SIZE: usize = 32;
/// Records per page.
pub const SLOT_COUNT: usize = PAGE_SIZE / SLOT_SIZE;
/// New-format record magic. The legacy profile magic is `0xB10F_C0DE`.
pub const MAGIC: u32 = 0xB10F_C0DF;
/// Pre-journal `StoredProfile` magic: `magic` at offset 0, `profile_id` at
/// offset 4 — *not* the new struct's offset 9 (§3, the offset the review
/// caught).
pub const LEGACY_MAGIC: u32 = 0xB10F_C0DE;
/// Record schema this firmware writes and reads.
pub const SCHEMA: u8 = 1;

/// Byte offset of the record body (everything the integrity covers).
const BODY_OFFSET: usize = 8;
/// Byte offset of the remap table within a slot.
const REMAP_OFFSET: usize = 12;

/// Where the effective record came from — the `Info` payload's `source`
/// field (design v2 §4.2).
pub mod source {
    /// A valid new-format journal record.
    pub const RECORD: u8 = 0;
    /// The legacy profile page (profile only; map is `DEFAULT`).
    pub const LEGACY: u8 = 1;
    /// A blank page — nothing was ever written, or a compaction erase was
    /// interrupted before the rewrite.
    pub const EMPTY: u8 = 2;
    /// Bytes are present but nothing decodes — corruption, or an unknown
    /// future schema.
    pub const INVALID: u8 = 3;
}

/// The durable preferences one slot carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredPrefs {
    pub profile_id: u8,
    pub remap: RemapTable,
}

/// 32-bit FNV-1a — the record's integrity word.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Serialize one slot. The caller (the flash driver) writes bytes
/// `8..32` first and `0..8` last so the record only becomes valid once it
/// is whole.
#[must_use]
pub fn encode_slot(prefs: &StoredPrefs) -> [u8; SLOT_SIZE] {
    let mut slot = [0u8; SLOT_SIZE];
    slot[BODY_OFFSET] = SCHEMA;
    slot[BODY_OFFSET + 1] = prefs.profile_id;
    // Pad bytes 10..12 stay 0.
    slot[REMAP_OFFSET..].copy_from_slice(&prefs.remap.to_bytes());
    let integrity = fnv1a(&slot[BODY_OFFSET..]);
    slot[..4].copy_from_slice(&MAGIC.to_le_bytes());
    slot[4..8].copy_from_slice(&integrity.to_le_bytes());
    slot
}

/// Decode and fully validate one slot: magic, integrity over the body,
/// schema, and §4.4's map validation. `None` for anything else — a torn
/// slot can never decode.
#[must_use]
pub fn decode_slot(slot: &[u8]) -> Option<StoredPrefs> {
    if slot.len() != SLOT_SIZE {
        return None;
    }
    if slot[..4] != MAGIC.to_le_bytes() {
        return None;
    }
    let integrity = u32::from_le_bytes([slot[4], slot[5], slot[6], slot[7]]);
    if integrity != fnv1a(&slot[BODY_OFFSET..]) {
        return None;
    }
    if slot[BODY_OFFSET] != SCHEMA {
        return None;
    }
    let remap = RemapTable::from_bytes(&slot[REMAP_OFFSET..])?;
    Some(StoredPrefs {
        profile_id: slot[BODY_OFFSET + 1],
        remap,
    })
}

/// What a page scan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scan {
    /// The effective record — the highest-indexed valid slot, or the legacy
    /// profile with `RemapTable::DEFAULT` — or `None` when the page holds
    /// nothing loadable (the caller applies its defaults).
    pub prefs: Option<StoredPrefs>,
    /// Which [`source`] produced `prefs`.
    pub source: u8,
    /// First all-`0xFF` slot, where the next append goes. `None` means the
    /// journal is full: the next save compacts (erase, rewrite into slot 0).
    pub first_free: Option<usize>,
}

/// Scan the page: highest-indexed valid record wins; with no valid record
/// the page start is reinterpreted as the legacy `StoredProfile` (§3).
///
/// The legacy record occupies only the first 8 bytes of slot 0, so the
/// first new-format commit simply appends to slot 1 — migration needs no
/// erase.
#[must_use]
pub fn scan(page: &[u8; PAGE_SIZE]) -> Scan {
    let mut newest: Option<StoredPrefs> = None;
    let mut first_free: Option<usize> = None;
    for index in 0..SLOT_COUNT {
        let slot = &page[index * SLOT_SIZE..(index + 1) * SLOT_SIZE];
        if let Some(prefs) = decode_slot(slot) {
            newest = Some(prefs);
        } else if first_free.is_none() && slot.iter().all(|&b| b == 0xFF) {
            first_free = Some(index);
        }
    }
    if let Some(prefs) = newest {
        return Scan {
            prefs: Some(prefs),
            source: source::RECORD,
            first_free,
        };
    }
    if page[..4] == LEGACY_MAGIC.to_le_bytes() {
        return Scan {
            prefs: Some(StoredPrefs {
                // Legacy layout: profile_id at offset 4, never through the
                // new struct where it sits at offset 9.
                profile_id: page[4],
                remap: RemapTable::DEFAULT,
            }),
            source: source::LEGACY,
            first_free,
        };
    }
    Scan {
        prefs: None,
        source: if page.iter().all(|&b| b == 0xFF) {
            source::EMPTY
        } else {
            source::INVALID
        },
        first_free,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remap::dest;

    fn a_b_swapped() -> StoredPrefs {
        let mut remap = RemapTable::DEFAULT;
        remap.buttons[crate::remap::source::A] = dest::B;
        remap.buttons[crate::remap::source::B] = dest::A;
        StoredPrefs {
            profile_id: 1,
            remap,
        }
    }

    fn blank_page() -> [u8; PAGE_SIZE] {
        [0xFF; PAGE_SIZE]
    }

    fn write_slot(page: &mut [u8; PAGE_SIZE], index: usize, slot: &[u8; SLOT_SIZE]) {
        page[index * SLOT_SIZE..(index + 1) * SLOT_SIZE].copy_from_slice(slot);
    }

    #[test]
    fn roundtrip() {
        let prefs = a_b_swapped();
        assert_eq!(decode_slot(&encode_slot(&prefs)), Some(prefs));
    }

    #[test]
    fn corruption_rejected() {
        let good = encode_slot(&a_b_swapped());

        let mut bad = good;
        bad[20] ^= 0x01; // one body bit
        assert!(
            decode_slot(&bad).is_none(),
            "integrity must catch body flip"
        );

        let mut bad = good;
        bad[0] ^= 0x01; // magic
        assert!(decode_slot(&bad).is_none());

        let mut bad = good;
        bad[8] = 2; // future schema
                    // Re-seal so only the schema is unknown.
        let integrity = fnv1a(&bad[8..]);
        bad[4..8].copy_from_slice(&integrity.to_le_bytes());
        assert!(decode_slot(&bad).is_none(), "unknown schema rejected");

        assert!(decode_slot(&good[..31]).is_none(), "wrong length rejected");
    }

    #[test]
    fn invalid_map_in_sealed_record_rejected() {
        // A record whose integrity is fine but whose map fails §4.4 must
        // not load — validation applies at boot, not just at the GATT seam.
        let mut slot = encode_slot(&a_b_swapped());
        slot[REMAP_OFFSET + 18] = 0; // trigger_threshold = 0
        let integrity = fnv1a(&slot[8..]);
        slot[4..8].copy_from_slice(&integrity.to_le_bytes());
        assert!(decode_slot(&slot).is_none());
    }

    #[test]
    fn highest_valid_slot_wins() {
        let mut page = blank_page();
        let older = StoredPrefs {
            profile_id: 0,
            remap: RemapTable::DEFAULT,
        };
        let newer = a_b_swapped();
        write_slot(&mut page, 0, &encode_slot(&older));
        write_slot(&mut page, 5, &encode_slot(&newer));
        let scan = scan(&page);
        assert_eq!(scan.prefs, Some(newer));
        assert_eq!(scan.source, source::RECORD);
        assert_eq!(scan.first_free, Some(1), "appends fill the first gap");
    }

    #[test]
    fn torn_slot_leaves_previous_effective() {
        let mut page = blank_page();
        let good = a_b_swapped();
        write_slot(&mut page, 0, &encode_slot(&good));
        // Slot 1: body written, header interrupted (still 0xFF) — exactly
        // what body-first/header-last leaves behind on power loss.
        let torn = encode_slot(&StoredPrefs {
            profile_id: 1,
            remap: RemapTable::DEFAULT,
        });
        page[SLOT_SIZE + 8..2 * SLOT_SIZE].copy_from_slice(&torn[8..]);
        let scan = scan(&page);
        assert_eq!(scan.prefs, Some(good), "torn slot is skipped");
        assert_eq!(scan.source, source::RECORD);
        assert_eq!(scan.first_free, Some(2), "the torn slot is not free");
    }

    #[test]
    fn legacy_page_reads_profile_at_offset_4() {
        let mut page = blank_page();
        // Legacy StoredProfile: magic at 0, profile_id at 4. Poison offset
        // 9 (the new struct's profile_id position) to prove the legacy path
        // never reads through the new layout.
        page[..4].copy_from_slice(&LEGACY_MAGIC.to_le_bytes());
        page[4] = 7;
        page[9] = 42;
        let scan = scan(&page);
        assert_eq!(
            scan.prefs,
            Some(StoredPrefs {
                profile_id: 7,
                remap: RemapTable::DEFAULT,
            })
        );
        assert_eq!(scan.source, source::LEGACY);
        assert_eq!(
            scan.first_free,
            Some(1),
            "legacy header occupies slot 0; migration appends, no erase"
        );
    }

    #[test]
    fn empty_page_loads_nothing_and_appends_at_zero() {
        // Also the interrupted-compaction-erase outcome: the record is lost
        // to DEFAULT (the caller's fallback), and the next append works.
        let page = blank_page();
        let scan = scan(&page);
        assert_eq!(scan.prefs, None);
        assert_eq!(scan.source, source::EMPTY);
        assert_eq!(scan.first_free, Some(0));
    }

    #[test]
    fn garbage_page_is_invalid_and_forces_compaction() {
        let page = [0xA5u8; PAGE_SIZE];
        let scan = scan(&page);
        assert_eq!(scan.prefs, None);
        assert_eq!(scan.source, source::INVALID);
        assert_eq!(scan.first_free, None, "no free slot: next save compacts");
    }

    #[test]
    fn full_journal_has_no_free_slot() {
        let mut page = blank_page();
        for i in 0..SLOT_COUNT {
            let prefs = StoredPrefs {
                profile_id: u8::try_from(i % 2).unwrap(),
                remap: RemapTable::DEFAULT,
            };
            write_slot(&mut page, i, &encode_slot(&prefs));
        }
        let scan = scan(&page);
        assert_eq!(scan.source, source::RECORD);
        assert_eq!(scan.first_free, None, "the 128th save triggers compaction");
    }

    #[test]
    fn compaction_rewrite_into_slot_zero_is_effective() {
        // Post-compaction page: erased, record rewritten at slot 0.
        let mut page = blank_page();
        let prefs = a_b_swapped();
        write_slot(&mut page, 0, &encode_slot(&prefs));
        let scan = scan(&page);
        assert_eq!(scan.prefs, Some(prefs));
        assert_eq!(scan.first_free, Some(1));
    }
}
