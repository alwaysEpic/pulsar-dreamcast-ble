// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Bit-packed captures to and from the byte-wide capture format.
//!
//! A SPIM receiving LSB-first stores sample `i` of its line as bit `i % 8` of
//! byte `i / 8`. The block decoder reads byte-wide samples, one `P0.IN` low
//! byte per sample, with SDCKA and SDCKB at their pin bits. This module
//! converts between the two so a hardware capture is judged by the same
//! decoder as the CPU capture, and no second decoder has to be trusted.
//!
//! `unpack_words_into` is the same conversion into whole words, for the bulk
//! read path whose start finder takes `u32` samples.
//!
//! `unpack_into` runs once per read on the device, over 53,248 samples. It
//! spreads four bits at a time into four byte lanes with one multiply, so
//! the cost is a few cycles per sample rather than a branch per bit; a table
//! would be faster still but adds 4 KB of read-only data to the image, and
//! a probe does not need it.

/// Spread the low four bits of `n` into bit 0 of each of four byte lanes:
/// bit `k` of `n` lands at bit `8k`. The four shifted copies the multiply
/// sums occupy disjoint bit ranges (0–3, 7–10, 14–17, 21–24), so the sum is
/// an OR and the mask keeps one bit per lane.
const fn spread4(n: u32) -> u32 {
    ((n & 0xF) * 0x0020_4081) & 0x0101_0101
}

/// The four bytes of `c` as a little-endian word, so sample `i` of the group
/// sits at bit `i` — the same order within the group that a byte-at-a-time
/// read gives within a byte. `copy_from_slice` rather than `try_into().unwrap()`
/// because the crate forbids `unwrap`, and rather than an OR of four shifted
/// bytes because only this form compiled to the single unaligned `ldr` ARMv7E-M
/// can do: the OR chain stayed four `ldrb`s and spilled the loop's registers.
///
/// # Panics
///
/// If `c` is not four bytes. Every caller is a `chunks_exact(4)` item, so the
/// length is a property of the iterator and the check compiles away.
#[inline]
const fn word_le(c: &[u8]) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(c);
    u32::from_le_bytes(w)
}

/// Unpack two LSB-first bit streams into byte-wide samples.
///
/// Sample `i` is `pin_a` if bit `i` of `a` is set, plus `pin_b` if bit `i` of
/// `b` is set, and nothing else. Stops at the shortest of the three buffers
/// and returns the number of samples written.
///
/// `pin_a` and `pin_b` are single-bit masks (`1 << bit`, bit ≤ 7), as the
/// decoder's const parameters are; the lane multiply below relies on that.
#[must_use]
pub fn unpack_into(a: &[u8], b: &[u8], out: &mut [u8], pin_a: u8, pin_b: u8) -> usize {
    let (pa, pb) = (u32::from(pin_a), u32::from(pin_b));
    let mut written = 0;
    for ((&ab, &bb), o) in a.iter().zip(b).zip(out.chunks_exact_mut(8)) {
        let lo = (spread4(u32::from(ab)) * pa) | (spread4(u32::from(bb)) * pb);
        let hi = (spread4(u32::from(ab >> 4)) * pa) | (spread4(u32::from(bb >> 4)) * pb);
        o[..4].copy_from_slice(&lo.to_le_bytes());
        o[4..].copy_from_slice(&hi.to_le_bytes());
        written += 8;
    }
    // Fewer than eight samples of `out` left, with stream bytes to serve them.
    // An index loop, not an iterator chain: under the firmware's fat LTO the
    // `Take<Enumerate<IterMut>>` adapter came out as an out-of-line `core`
    // instance placed ahead of the Maple bus code, which alone moves it
    // (found on the first v267 build).
    let end = out.len().min(a.len() * 8).min(b.len() * 8);
    let mut i = written;
    while i < end {
        let (byte, bit) = (i / 8, i % 8);
        let a_set = (a[byte] >> bit) & 1 != 0;
        let b_set = (b[byte] >> bit) & 1 != 0;
        out[i] = if a_set { pin_a } else { 0 } | if b_set { pin_b } else { 0 };
        i += 1;
    }
    end
}

/// Unpack two LSB-first bit streams into word-wide samples.
///
/// Sample `i` is `1 << pin_a` if bit `i` of `a` is set, plus `1 << pin_b` if
/// bit `i` of `b` is set, and nothing else — the shape [`wire::find_data_start`]
/// reads, so a SPIM capture is aligned by the same finder as a `P0.IN` capture
/// without a second one to trust. Bit order within a stream byte is exactly
/// [`unpack_into`]'s. Stops at the shortest of the three buffers and returns
/// the number of samples written.
///
/// Unlike [`unpack_into`], whose pins are single-bit *masks*, `pin_a` and
/// `pin_b` are bit **indices** 0..32 — they come straight from
/// `board::PIN_A_BIT` / `PIN_B_BIT`, which are `u32` positions. An index past
/// the word selects nothing rather than faulting the loop.
///
/// [`wire::find_data_start`]: crate::wire::find_data_start
#[must_use]
pub fn unpack_words_into(a: &[u8], b: &[u8], out: &mut [u32], pin_a: u32, pin_b: u32) -> usize {
    // Shift once here, not per sample. `unwrap_or(0)` rather than a panic: an
    // out-of-range index can only come from a board constant, and this loop
    // runs 24,576 times per controller poll — it must not carry a fault path.
    let (ma, mb) = (
        1u32.checked_shl(pin_a).unwrap_or(0),
        1u32.checked_shl(pin_b).unwrap_or(0),
    );
    let end = out.len().min(a.len() * 8).min(b.len() * 8);
    // The fast path takes four bytes of each stream — one full word of 32
    // samples — per iteration, with every sample's shift a compile-time
    // constant. Rotating the stream word *left* by the pin index puts its bit
    // `i` at bit `pin + i`, so sample `i` is that word rotated right by `i` and
    // masked, and on Cortex-M4 a constant `ror` folds into the AND's second
    // operand: a sample costs two ANDs, an ORR and a store, and nothing else.
    // A full 32-bit rotate, not a shift, because it must not drop the bits that
    // a pin index near 31 pushes off the top — and because 32 samples per word
    // is exactly one rotation, so `i` spans 0..32 with no bit unaccounted for.
    //
    // Thirty-two samples an iteration, not eight, so the two loads, the counter
    // and the branch are paid once per 32 rather than once per byte pair: that
    // is the difference between ~4.9 and ~4.3 cycles a sample on the compiled
    // loop (133 instructions for 32 samples, measured on the v285 ELF). The `chunks_exact` zip is [`unpack_into`]'s shape, and it is what
    // removes the per-sample bounds check — v267's warning is about adapters
    // that stay out of line, and the disassembly of this one has no call in the
    // loop body at all.
    let n32 = end / 32;
    let (fa, fb, fo) = (&a[..n32 * 4], &b[..n32 * 4], &mut out[..n32 * 32]);
    for ((ca, cb), o) in fa
        .chunks_exact(4)
        .zip(fb.chunks_exact(4))
        .zip(fo.chunks_exact_mut(32))
    {
        let wa = word_le(ca).rotate_left(pin_a);
        let wb = word_le(cb).rotate_left(pin_b);
        o[0] = (ma & wa) | (mb & wb);
        o[1] = (ma & wa.rotate_right(1)) | (mb & wb.rotate_right(1));
        o[2] = (ma & wa.rotate_right(2)) | (mb & wb.rotate_right(2));
        o[3] = (ma & wa.rotate_right(3)) | (mb & wb.rotate_right(3));
        o[4] = (ma & wa.rotate_right(4)) | (mb & wb.rotate_right(4));
        o[5] = (ma & wa.rotate_right(5)) | (mb & wb.rotate_right(5));
        o[6] = (ma & wa.rotate_right(6)) | (mb & wb.rotate_right(6));
        o[7] = (ma & wa.rotate_right(7)) | (mb & wb.rotate_right(7));
        o[8] = (ma & wa.rotate_right(8)) | (mb & wb.rotate_right(8));
        o[9] = (ma & wa.rotate_right(9)) | (mb & wb.rotate_right(9));
        o[10] = (ma & wa.rotate_right(10)) | (mb & wb.rotate_right(10));
        o[11] = (ma & wa.rotate_right(11)) | (mb & wb.rotate_right(11));
        o[12] = (ma & wa.rotate_right(12)) | (mb & wb.rotate_right(12));
        o[13] = (ma & wa.rotate_right(13)) | (mb & wb.rotate_right(13));
        o[14] = (ma & wa.rotate_right(14)) | (mb & wb.rotate_right(14));
        o[15] = (ma & wa.rotate_right(15)) | (mb & wb.rotate_right(15));
        o[16] = (ma & wa.rotate_right(16)) | (mb & wb.rotate_right(16));
        o[17] = (ma & wa.rotate_right(17)) | (mb & wb.rotate_right(17));
        o[18] = (ma & wa.rotate_right(18)) | (mb & wb.rotate_right(18));
        o[19] = (ma & wa.rotate_right(19)) | (mb & wb.rotate_right(19));
        o[20] = (ma & wa.rotate_right(20)) | (mb & wb.rotate_right(20));
        o[21] = (ma & wa.rotate_right(21)) | (mb & wb.rotate_right(21));
        o[22] = (ma & wa.rotate_right(22)) | (mb & wb.rotate_right(22));
        o[23] = (ma & wa.rotate_right(23)) | (mb & wb.rotate_right(23));
        o[24] = (ma & wa.rotate_right(24)) | (mb & wb.rotate_right(24));
        o[25] = (ma & wa.rotate_right(25)) | (mb & wb.rotate_right(25));
        o[26] = (ma & wa.rotate_right(26)) | (mb & wb.rotate_right(26));
        o[27] = (ma & wa.rotate_right(27)) | (mb & wb.rotate_right(27));
        o[28] = (ma & wa.rotate_right(28)) | (mb & wb.rotate_right(28));
        o[29] = (ma & wa.rotate_right(29)) | (mb & wb.rotate_right(29));
        o[30] = (ma & wa.rotate_right(30)) | (mb & wb.rotate_right(30));
        o[31] = (ma & wa.rotate_right(31)) | (mb & wb.rotate_right(31));
    }
    // The tail: `out` ending mid-byte, or a stream shorter than the samples
    // asked for. An index loop, not an iterator chain, and `>> 3` / `& 7`
    // rather than `/ 8` / `% 8`: see the tail loop in `unpack_into` for what an
    // adapter chain costs the firmware's layout (found on the first v267
    // build). It runs at most 31 times per call, so its shape is free — and on
    // the controller poll, whose 24,576 samples are 768 whole words, not at
    // all.
    let mut i = n32 * 32;
    while i < end {
        let (byte, bit) = (i >> 3, i & 7);
        // 0 or 1, negated to 0 or all-ones, so each pin costs a mask and no
        // branch per sample.
        let a_bit = u32::from(a[byte] >> bit) & 1;
        let b_bit = u32::from(b[byte] >> bit) & 1;
        out[i] = (ma & a_bit.wrapping_neg()) | (mb & b_bit.wrapping_neg());
        i += 1;
    }
    end
}

/// The inverse of [`unpack_into`], for tests.
///
/// Packs byte-wide samples into two LSB-first bit streams, `a` from `pin_a`
/// and `b` from `pin_b`; the other six bits of each sample are dropped. Every
/// stream byte that receives a bit is cleared first. Stops at the shortest of
/// the three buffers and returns the number of samples packed.
#[must_use]
pub fn pack_from(samples: &[u8], pin_a: u8, pin_b: u8, a: &mut [u8], b: &mut [u8]) -> usize {
    let n = samples.len().min(a.len() * 8).min(b.len() * 8);
    let bytes = n.div_ceil(8);
    a[..bytes].fill(0);
    b[..bytes].fill(0);
    for (i, &s) in samples[..n].iter().enumerate() {
        let (byte, bit) = (i / 8, i % 8);
        if s & pin_a != 0 {
            a[byte] |= 1 << bit;
        }
        if s & pin_b != 0 {
            b[byte] |= 1 << bit;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;
    use std::vec;
    use std::vec::Vec;

    use super::*;
    use crate::block_decode::{BlockDecoder, Outcome};
    use crate::test_wave::{block_frame, Wave, A, B};
    use crate::wire::{find_data_start, find_data_start_in};

    /// Bit indices for the word-wide tests: xiao's real pair. `PIN_A` is not
    /// the builder's `A` bit (2), so a sample that kept its byte-wide position
    /// fails rather than passing by coincidence.
    const PIN_A: u32 = 5;
    const PIN_B: u32 = 3;

    fn pack(s: &[u8], pin_a: u8, pin_b: u8) -> (Vec<u8>, Vec<u8>) {
        let bytes = s.len().div_ceil(8);
        let (mut a, mut b) = (vec![0xAAu8; bytes], vec![0x55u8; bytes]);
        assert_eq!(pack_from(s, pin_a, pin_b, &mut a, &mut b), s.len());
        (a, b)
    }

    fn unpack(a: &[u8], b: &[u8], n: usize, pin_a: u8, pin_b: u8) -> Vec<u8> {
        let mut out = vec![0xFFu8; n];
        assert_eq!(unpack_into(a, b, &mut out, pin_a, pin_b), n);
        out
    }

    fn unpack_words(a: &[u8], b: &[u8], n: usize, pin_a: u32, pin_b: u32) -> Vec<u32> {
        let mut out = vec![0xFFFF_FFFFu32; n];
        assert_eq!(unpack_words_into(a, b, &mut out, pin_a, pin_b), n);
        out
    }

    /// Decode as the diagnostic does: outcome, bytes, the checksum residue,
    /// the bit count and each byte's end position.
    fn decode(s: &[u8]) -> (Outcome, Vec<u8>, u8, u32, Vec<u16>) {
        let mut d = BlockDecoder::<A, B>::new();
        let mut out = vec![0u8; 528];
        let mut pos = vec![0u16; 528];
        let o = d.begin(s).unwrap_or_else(|| loop {
            if let Some(o) = d.step(s, &mut out, &mut pos, 1024) {
                break o;
            }
        });
        out.truncate(d.nbytes());
        pos.truncate(d.nbytes());
        (o, out, d.xor(), d.stats().bits, pos)
    }

    #[test]
    fn sample_i_is_bit_i_of_byte_i_over_8_lsb_first() {
        let out = unpack(&[0b0000_0001, 0b1000_0000], &[0b0001_0000, 0], 16, A, B);
        let mut want = [0u8; 16];
        want[0] = A;
        want[4] = B;
        want[15] = A;
        assert_eq!(out, want);
    }

    #[test]
    fn word_sample_i_is_bit_i_of_byte_i_over_8_lsb_first() {
        // The same streams as the byte-wide case above, one bit order to read.
        let out = unpack_words(
            &[0b0000_0001, 0b1000_0000],
            &[0b0001_0000, 0],
            16,
            PIN_A,
            PIN_B,
        );
        let mut want = [0u32; 16];
        want[0] = 1 << PIN_A;
        want[4] = 1 << PIN_B;
        want[15] = 1 << PIN_A;
        assert_eq!(out, want);
    }

    #[test]
    fn word_samples_carry_only_the_two_pins_moved_to_their_bit_indices() {
        let f = block_frame(7, 21);
        let mut w = Wave::new(5, 2, 3);
        w.frame(&f, (100, 300));
        let (a, b) = pack(&w.s, A, B);
        let words = unpack_words(&a, &b, w.s.len(), PIN_A, PIN_B);
        for (i, (&s, &word)) in w.s.iter().zip(&words).enumerate() {
            // The builder's noise on the six non-Maple pins must not survive.
            let want =
                if s & A != 0 { 1 << PIN_A } else { 0 } | if s & B != 0 { 1 << PIN_B } else { 0 };
            assert_eq!(word, want, "sample {i}");
        }
    }

    #[test]
    fn the_word_samples_find_the_same_data_start_as_the_byte_samples() {
        // Alignment is what the word form exists for: the same capture, the
        // same finder, one reading `u32` samples and one `u8`.
        for seed in 1..40u32 {
            for &(lo, hi) in &[(1, 1), (1, 2), (1, 3), (2, 4), (3, 6)] {
                let f = block_frame(seed.to_le_bytes()[0], seed * 31 + 3);
                let mut w = Wave::new(seed, lo, hi);
                w.frame(&f, (60, 900));
                let (a, b) = pack(&w.s, A, B);
                let bytes = unpack(&a, &b, w.s.len(), A, B);
                let words = unpack_words(&a, &b, w.s.len(), PIN_A, PIN_B);
                let tag = format!("seed {seed} rate {lo}-{hi}");
                let byte_start = find_data_start_in(&bytes, u32::from(A), u32::from(B));
                assert!(byte_start.is_some(), "{tag}");
                assert_eq!(
                    find_data_start(&words, 1 << PIN_A, 1 << PIN_B),
                    byte_start,
                    "{tag}"
                );
            }
        }
    }

    #[test]
    fn unpack_words_stops_at_the_shortest_buffer_and_leaves_the_rest_untouched() {
        let a = [0xFFu8; 3];
        let b = [0x00u8; 2];
        // `b` limits to 16 samples; `out` has room for 21.
        let mut out = [0x7777_7777u32; 21];
        assert_eq!(unpack_words_into(&a, &b, &mut out, PIN_A, PIN_B), 16);
        assert!(out[..16].iter().all(|&x| x == 1 << PIN_A));
        assert!(out[16..].iter().all(|&x| x == 0x7777_7777));
        // `out` limits, mid-byte, at the two extreme bit indices.
        let mut out = [0x7777_7777u32; 13];
        assert_eq!(unpack_words_into(&a, &a, &mut out, 31, 0), 13);
        assert!(out.iter().all(|&x| x == (1 << 31) | 1));
        // Empty streams write nothing.
        let mut out = [0x7777_7777u32; 8];
        assert_eq!(unpack_words_into(&[], &a, &mut out, PIN_A, PIN_B), 0);
        assert!(out.iter().all(|&x| x == 0x7777_7777));
    }

    /// The unrolled word-at-a-time fast path and the sample-at-a-time tail must
    /// agree with the definition at every length — including the exact
    /// multiples of 32, where the tail runs not at all, and each of the 31 tail
    /// lengths either side of one. Nine stream bytes is 72 samples: two whole
    /// words and a tail, so a second iteration of the fast path is covered too.
    #[test]
    fn the_word_fast_path_and_its_tail_agree_at_every_length() {
        let a: Vec<u8> = (0..6u8)
            .map(|i| i.wrapping_mul(37).wrapping_add(11))
            .collect();
        let b: Vec<u8> = (0..6u8)
            .map(|i| i.wrapping_mul(91).wrapping_add(5))
            .collect();
        // The real pair, then both extreme indices in both orders, then a pair
        // straddling a byte boundary.
        for &(pa, pb) in &[(PIN_A, PIN_B), (0, 31), (31, 0), (7, 8)] {
            for n in 0..=a.len() * 8 {
                // Three untouched words past `n` catch a fast path that ran
                // one byte pair too far.
                let mut out = vec![0x7777_7777u32; n + 3];
                assert_eq!(unpack_words_into(&a, &b, &mut out[..n], pa, pb), n);
                for i in 0..n {
                    let want = if a[i / 8] >> (i % 8) & 1 != 0 {
                        1 << pa
                    } else {
                        0
                    } | if b[i / 8] >> (i % 8) & 1 != 0 {
                        1 << pb
                    } else {
                        0
                    };
                    assert_eq!(out[i], want, "pins {pa}/{pb} len {n} sample {i}");
                }
                assert!(
                    out[n..].iter().all(|&x| x == 0x7777_7777),
                    "pins {pa}/{pb} len {n} wrote past the end"
                );
            }
        }
    }

    #[test]
    fn every_pin_pair_round_trips_through_both_directions() {
        let f = block_frame(9, 11);
        let mut w = Wave::new(2, 1, 3);
        w.frame(&f, (100, 300));
        // The builder puts the Maple lines at A and B; move them for the
        // other pairs so the pins under test carry the waveform.
        for (pa, pb) in [(A, B), (1, 1 << 7), (1 << 7, 1), (1 << 3, 1 << 4)] {
            let moved: Vec<u8> =
                w.s.iter()
                    .map(|&x| {
                        let noise = x & !(A | B) & !(pa | pb);
                        noise | if x & A != 0 { pa } else { 0 } | if x & B != 0 { pb } else { 0 }
                    })
                    .collect();
            let streams = pack(&moved, pa, pb);
            let back = unpack(&streams.0, &streams.1, moved.len(), pa, pb);
            let masked: Vec<u8> = moved.iter().map(|&x| x & (pa | pb)).collect();
            assert_eq!(back, masked, "pins {pa:#x}/{pb:#x}");
            assert_eq!(pack(&back, pa, pb), streams, "pins {pa:#x}/{pb:#x}");
        }
    }

    #[test]
    fn the_reference_captures_decode_the_same_packed_and_unpacked() {
        for seed in 1..40u32 {
            for &(lo, hi) in &[(1, 1), (1, 2), (1, 3), (2, 4), (3, 6)] {
                let f = block_frame(seed.to_le_bytes()[0], seed * 31 + 3);
                let mut w = Wave::new(seed, lo, hi);
                w.frame(&f, (60, 900));
                let (a, b) = pack(&w.s, A, B);
                let back = unpack(&a, &b, w.s.len(), A, B);
                let tag = format!("seed {seed} rate {lo}-{hi}");
                let direct = decode(&w.s);
                assert_eq!(direct.0, Outcome::Complete, "{tag}");
                assert_eq!(direct.1, f, "{tag}");
                assert_eq!(decode(&back), direct, "{tag}");
            }
        }
    }

    #[test]
    fn a_truncated_and_a_startless_capture_agree_too() {
        let f = block_frame(1, 5);
        let mut w = Wave::new(3, 2, 3);
        w.frame(&f, (500, 800));
        let half = &w.s[..w.s.len() / 2];
        let (a, b) = pack(half, A, B);
        let back = unpack(&a, &b, half.len(), A, B);
        assert_eq!(decode(half).0, Outcome::Exhausted);
        assert_eq!(decode(&back), decode(half));

        let idle = vec![A | B | 0x40; 1000];
        let (a, b) = pack(&idle, A, B);
        let back = unpack(&a, &b, idle.len(), A, B);
        assert_eq!(decode(&back).0, Outcome::NoStart);
    }

    #[test]
    fn unpack_stops_at_the_shortest_buffer_and_leaves_the_rest_untouched() {
        let a = [0xFFu8; 3];
        let b = [0x00u8; 2];
        // `b` limits to 16 samples; `out` has room for 21.
        let mut out = [0x77u8; 21];
        assert_eq!(unpack_into(&a, &b, &mut out, A, B), 16);
        assert!(out[..16].iter().all(|&x| x == A));
        assert!(out[16..].iter().all(|&x| x == 0x77));
        // `out` limits, mid-byte: 13 samples, the last five by the tail loop.
        let mut out = [0x77u8; 13];
        assert_eq!(unpack_into(&a, &a, &mut out, A, B), 13);
        assert!(out.iter().all(|&x| x == A | B));
        // Empty streams write nothing.
        let mut out = [0x77u8; 8];
        assert_eq!(unpack_into(&[], &a, &mut out, A, B), 0);
        assert!(out.iter().all(|&x| x == 0x77));
    }

    #[test]
    fn pack_stops_at_the_shortest_buffer_and_clears_only_what_it_fills() {
        let s = [A; 13];
        let (mut a, mut b) = ([0xFFu8; 4], [0xFFu8; 4]);
        assert_eq!(pack_from(&s, A, B, &mut a, &mut b), 13);
        assert_eq!(a, [0xFF, 0x1F, 0xFF, 0xFF]);
        assert_eq!(b, [0x00, 0x00, 0xFF, 0xFF]);
        // A one-byte stream takes eight samples.
        let (mut a, mut b) = ([0u8; 1], [0u8; 4]);
        assert_eq!(pack_from(&s, A, B, &mut a, &mut b), 8);
        assert_eq!(a, [0xFF]);
    }
}
