// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! The block-byte contract: which order a VMU block's 512 bytes are in, on the
//! wire and on the BLE link.
//!
//! # The two orders
//!
//! A Maple frame is a sequence of 32-bit words sent **least-significant byte
//! first**. So a block that the card holds as the word `W` at some offset
//! arrives as the four bytes `W, W>>8, W>>16, W>>24` — the word's bytes
//! backwards. That is **wire order**: what [`crate::block_decode`] produces and
//! what a capture buffer holds.
//!
//! The console's Maple DMA reverses each word again on the way into memory, so
//! what the BIOS, the filesystem and every save file are laid out in is
//! `u32::from_be_bytes` of four consecutive wire-order bytes. That is **image
//! order**: a dumped 128 KiB VMU image is in it, and the root block's fields
//! only parse in it.
//!
//! The two differ by reversing each 4-byte group, and the transform is its own
//! inverse: `k -> (k & !3) | (3 - (k & 3))`.
//!
//! # The contract
//!
//! **Everything above the wire speaks image order.** The `81 DATA` payload, the
//! `02 WRITE` payload and the `card` fingerprint are all image order; only this
//! firmware's capture buffers and its `BLOCK_WRITE` frames are in wire order,
//! and this module is the one place that converts between them.
//!
//! Settled 2026-09-18, for three reasons:
//!
//! 1. **Image order is the only one that means anything.** Offset `0x46` of the
//!    root block is the FAT's block number in image order and is half of a
//!    different field in wire order. A contract stated in terms of the
//!    filesystem can be checked by reading it; one stated in terms of the
//!    transport can only be checked by agreeing.
//! 2. **The far side is already written in it.** A console-side sender builds
//!    a reply word with `u32::from_be_bytes` of four image bytes, and a
//!    formatter writes the media info the same
//!    way. Carrying wire order over BLE would put a reversal on the sender's
//!    every path *and* leave its own image in image order regardless.
//! 3. **Both ends already read the frame's header words this way.**
//!    `maple::block_read` takes the function and location words as
//!    `u32::from_le_bytes` of wire bytes, which is the same natural-word
//!    convention. Only the payload was left raw.
//!
//! # What the card actually reports
//!
//! Not a doctrinal choice. The diagnostic resolved the order per card
//! by parsing the root block both ways and keeping whichever was sane, and
//! reported the answer as bit 7 of telemetry tag 44. Across **33 bench runs
//! that resolved a layout** (2026-09-14 to 2026-09-17, builds v263–v299, runs
//! #123–#196) the answer was
//! **word-swapped, every time**, with FAT 254, directory 253, directory size 13
//! and 200 user blocks — the geometry of a stock 128 KiB card, and exactly what the
//! fixture in this module's tests holds.
//!
//! # Who calls this
//!
//! `maple::block_read` converts a decoded block **once**, on the way out of the
//! decoder, so nothing downstream of it ever holds wire order. The write path
//! converts in the other direction as it builds each `BLOCK_WRITE` frame. No
//! other caller should need either.

/// Data bytes in a VMU storage block.
pub const BLOCK_BYTES: usize = 512;

/// Bytes carried by one `02 WRITE` / `81 DATA` phase: a quarter of a block.
///
/// A phase is word-aligned and a whole number of words, so [`wire_to_image`]
/// and [`image_to_wire`] may be applied per phase or to a whole block with the
/// same result — which is what lets the write path convert a phase as it
/// arrives rather than buffering a block first.
pub const PHASE_BYTES: usize = BLOCK_BYTES / 4;
const _: () = assert!(PHASE_BYTES.is_multiple_of(4));

/// The root block's FAT block number, u16 little-endian in image order.
pub const ROOT_FAT_AT: usize = 0x46;
/// The root block's directory block number.
pub const ROOT_DIR_AT: usize = 0x4A;
/// How many blocks the directory occupies.
pub const ROOT_DIR_BLOCKS_AT: usize = 0x4C;
/// How many user blocks the card has.
pub const ROOT_USER_BLOCKS_AT: usize = 0x50;

/// Reverse the bytes of each 4-byte group in place.
///
/// A trailing group shorter than a word is left alone: every caller here passes
/// a whole number of words, asserted at the call site, and silently mangling a
/// short tail would be worse than leaving it.
fn reverse_words(bytes: &mut [u8]) {
    for word in bytes.chunks_exact_mut(4) {
        word.swap(0, 3);
        word.swap(1, 2);
    }
}

/// Wire order to image order: a captured block becomes the bytes the
/// filesystem is laid out in.
pub fn wire_to_image(bytes: &mut [u8]) {
    reverse_words(bytes);
}

/// Image order to wire order: link bytes become the bytes a `BLOCK_WRITE`
/// frame's words are sent as.
///
/// The same transform as [`wire_to_image`] — it is an involution — and named
/// separately so a call site says which way it meant.
pub fn image_to_wire(bytes: &mut [u8]) {
    reverse_words(bytes);
}

/// A u16 field of a block in image order, little-endian, or `None` if the
/// offset does not hold one.
#[must_use]
pub fn image_u16(block: &[u8], at: usize) -> Option<u16> {
    let pair = block.get(at..at + 2)?;
    Some(u16::from_le_bytes([*pair.first()?, *pair.get(1)?]))
}

/// FNV-1a 32's offset basis — where a fingerprint starts.
pub const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

/// FNV-1a 32 over `bytes`, continuing from `seed`.
///
/// The `card` fingerprint of protocol v1: FNV-1a 32 over the 512 data bytes of
/// block 255 then block 254, **in image order**. It is taken in two calls
/// because the two blocks arrive one at a time — seed with
/// [`FNV_OFFSET_BASIS`] for 255, then feed 254 with what that returned.
///
/// The dongle computes the same hash over the same two blocks of its pulled
/// image, which is what makes it a comparison rather than a checksum: it
/// answers "is this the card I pulled?" across a reconnect.
#[must_use]
pub fn fnv1a32(seed: u32, bytes: &[u8]) -> u32 {
    let mut h = seed;
    for &b in bytes {
        h ^= u32::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first 88 bytes of a **formatted card's root block (255)**, in image
    /// order — the fixture the contract is pinned to.
    ///
    /// These are exact bytes, written out rather than rebuilt from constants on
    /// purpose: a fixture computed by the same code it checks would pass under
    /// either convention. Their provenance is `DreamPicoPort`'s
    /// `formatted_storage.bin` — sixteen `0x55`, the format flags at `0x10`,
    /// the format date at `0x30`, and the six media-info words from `0x40`.
    ///
    /// Everything from `0x58` to the end of the block is zero.
    const ROOT_IMAGE: [u8; 0x58] = [
        0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, // 0x00
        0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, // 0x08
        0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, // 0x10  formatted
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x18
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x20
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x28
        0x19, 0x99, 0x09, 0x09, 0x00, 0x00, 0x10, 0x00, // 0x30  1999-09-09
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x38
        0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0xFE, 0x00, // 0x40  blocks-1; system, FAT
        0x01, 0x00, 0xFD, 0x00, 0x0D, 0x00, 0x00, 0x00, // 0x48  FAT count, dir; dir size
        0xC8, 0x00, 0x1F, 0x00, 0x00, 0x00, 0x80, 0x00, // 0x50  user blocks, save area
    ];

    /// The same 88 bytes as they arrive on the wire — each word backwards.
    ///
    /// Also written out rather than derived, for the same reason: this is the
    /// half of the fixture that fails if the convention is flipped.
    const ROOT_WIRE: [u8; 0x58] = [
        0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, // 0x00
        0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, // 0x08
        0xFF, 0xFF, 0xFF, 0x01, 0x00, 0x00, 0x00, 0xFF, // 0x10
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x18
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x20
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x28
        0x09, 0x09, 0x99, 0x19, 0x00, 0x10, 0x00, 0x00, // 0x30
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x38
        0x00, 0x00, 0x00, 0xFF, 0x00, 0xFE, 0x00, 0xFF, // 0x40
        0x00, 0xFD, 0x00, 0x01, 0x00, 0x00, 0x00, 0x0D, // 0x48
        0x00, 0x1F, 0x00, 0xC8, 0x00, 0x80, 0x00, 0x00, // 0x50
    ];

    /// A whole block from one of the fixture heads: the rest of a formatted
    /// root block is zero in both orders.
    fn block(head: &[u8; 0x58]) -> [u8; BLOCK_BYTES] {
        let mut b = [0u8; BLOCK_BYTES];
        b[..head.len()].copy_from_slice(head);
        b
    }

    #[test]
    fn known_block_converts_to_the_exact_image_bytes() {
        let mut b = block(&ROOT_WIRE);
        wire_to_image(&mut b);
        assert_eq!(b, block(&ROOT_IMAGE));
    }

    #[test]
    fn known_block_converts_back_to_the_exact_wire_bytes() {
        let mut b = block(&ROOT_IMAGE);
        image_to_wire(&mut b);
        assert_eq!(b, block(&ROOT_WIRE));
    }

    /// The fixture would be worthless if the two orders happened to coincide:
    /// the 0x55 run and the zero tail are the same either way, and only the
    /// date and the media info tell them apart.
    #[test]
    fn the_two_orders_are_actually_different() {
        assert_ne!(ROOT_IMAGE, ROOT_WIRE);
        assert_ne!(ROOT_IMAGE[0x30..0x38], ROOT_WIRE[0x30..0x38]);
        assert_ne!(ROOT_IMAGE[0x40..0x58], ROOT_WIRE[0x40..0x58]);
    }

    /// The geometry the bench read off its test card in all 33 runs that
    /// resolved a layout — and the check the diagnostic used to pick the order.
    /// It only holds in image order, which is what makes image order the one
    /// worth naming in the protocol.
    #[test]
    fn the_root_fields_parse_in_image_order_only() {
        let image = block(&ROOT_IMAGE);
        assert_eq!(image_u16(&image, ROOT_FAT_AT), Some(254));
        assert_eq!(image_u16(&image, ROOT_DIR_AT), Some(253));
        assert_eq!(image_u16(&image, ROOT_DIR_BLOCKS_AT), Some(13));
        assert_eq!(image_u16(&image, ROOT_USER_BLOCKS_AT), Some(200));

        let wire = block(&ROOT_WIRE);
        for at in [
            ROOT_FAT_AT,
            ROOT_DIR_AT,
            ROOT_DIR_BLOCKS_AT,
            ROOT_USER_BLOCKS_AT,
        ] {
            assert_ne!(image_u16(&wire, at), image_u16(&image, at));
        }
    }

    /// The property the `81 DATA` split depends on: converting a block whole
    /// and converting it a phase at a time give the same bytes, so neither end
    /// has to reassemble before it converts.
    #[test]
    fn a_phase_converts_the_same_as_the_whole_block() {
        let mut whole = block(&ROOT_WIRE);
        wire_to_image(&mut whole);

        let mut by_phase = block(&ROOT_WIRE);
        for phase in by_phase.chunks_exact_mut(PHASE_BYTES) {
            wire_to_image(phase);
        }
        assert_eq!(whole, by_phase);
    }

    #[test]
    fn converting_twice_is_the_identity() {
        let mut b = block(&ROOT_WIRE);
        wire_to_image(&mut b);
        image_to_wire(&mut b);
        assert_eq!(b, block(&ROOT_WIRE));
    }

    /// A short tail is left alone rather than half-swapped. No caller passes
    /// one, and this pins the behaviour if one ever does.
    #[test]
    fn a_partial_word_is_left_alone() {
        let mut b = [1u8, 2, 3, 4, 5, 6];
        wire_to_image(&mut b);
        assert_eq!(b, [4, 3, 2, 1, 5, 6]);
    }

    /// The published FNV-1a 32 vectors. The fingerprint is only worth
    /// anything if both ends compute the *same* hash, so the function is
    /// pinned to the reference rather than to itself.
    #[test]
    fn fnv1a32_matches_the_reference_vectors() {
        assert_eq!(fnv1a32(FNV_OFFSET_BASIS, b""), FNV_OFFSET_BASIS);
        assert_eq!(fnv1a32(FNV_OFFSET_BASIS, b"a"), 0xe40c_292c);
        assert_eq!(fnv1a32(FNV_OFFSET_BASIS, b"foobar"), 0xbf9c_f968);
    }

    /// And that seeding is the same as hashing the concatenation, which is
    /// what lets `card` be taken one block at a time.
    #[test]
    fn seeding_continues_the_hash() {
        let split = fnv1a32(fnv1a32(FNV_OFFSET_BASIS, b"foo"), b"bar");
        assert_eq!(split, fnv1a32(FNV_OFFSET_BASIS, b"foobar"));
    }

    /// The fingerprint has to see the order: the same block in the other
    /// byte order must not hash the same, or a wire-order dongle and an
    /// image-order Pulsar would agree about a card they disagree about.
    #[test]
    fn the_two_orders_fingerprint_differently() {
        let image = block(&ROOT_IMAGE);
        let wire = block(&ROOT_WIRE);
        assert_ne!(
            fnv1a32(FNV_OFFSET_BASIS, &image),
            fnv1a32(FNV_OFFSET_BASIS, &wire)
        );
    }

    #[test]
    fn image_u16_refuses_an_offset_past_the_end() {
        let b = [0u8; 4];
        assert_eq!(image_u16(&b, 3), None);
        assert_eq!(image_u16(&b, 2), Some(0));
    }
}
