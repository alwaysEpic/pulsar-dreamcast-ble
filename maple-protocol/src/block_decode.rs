// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Incremental decoder
//! for a byte-wide long capture of a VMU `BLOCK_READ` reply.
//!
//! v252 decoded a whole block in one blocking pass and took up to 114.7 ms,
//! about seven poll periods. This is the same bit rule, rebuilt so it can run
//! in the poll loop's slack instead:
//!
//! - **Sliced.** [`BlockDecoder::step`] examines at most `quota` samples and
//!   returns; all state lives in the decoder, so the caller yields between
//!   slices and resumes where it stopped.
//! - **Edge-driven.** Nothing happens between edges, so the bit rule runs
//!   only at them. The scan for the next edge compares four samples at once
//!   against the current bus state replicated into every byte lane, with the
//!   other six pins of `P0.IN`'s low byte masked off, and lands on the first
//!   differing lane by a trailing-zero count; a word with no edge in it costs
//!   one compare. Run lengths — the pause and end-of-reply thresholds, the
//!   idle-state reset of the clock-phase flag, the runs histogram — come from
//!   sample indices, not per-sample counters. The v253 form ran the rule on
//!   every sample and cost 24 ms per block on device; the reply's ~12,600 edges
//!   are the floor this form works to.
//! - **Direct bytes.** Bits shift into a byte in a register; there is no bit
//!   vector, and the frame checksum is carried along as bytes complete.
//! - **It stops at the frame's own length.** The header's length byte says
//!   where the checksum is, so nothing past it is scanned.
//! - **Register-resident.** [`BlockDecoder::step`] copies the decoder's state
//!   into locals, runs the loop on those, and writes the state back once when
//!   the slice ends. v264 ran the same rule through `&mut self` and the
//!   inlined loop kept a stack shadow of every field *and* wrote each one
//!   through to memory at every edge — over 20 loads and stores per edge, and
//!   the device measured ≈ 97 cycles per edge, 18.9 ms per block, against
//!   an 8 ms budget (measured 2026-09-14). The end-of-reply
//!   test is a precomputed bound on the index (`ended_at`), one compare per
//!   word, instead of a subtraction against the previous edge.
//! - **Statistics are a feature.** The per-edge instrumentation — run
//!   lengths, double transitions, pause positions, the last edge, the skipped
//!   count — costs cycles at every edge and only the read diagnostic reads it,
//!   so it is behind `decode-stats`, off for production. With the feature on
//!   every statistic is v252's, and the reference test holds it there.
//!
//! The bit rule is v252's, unchanged, and the tests hold it to that: an SDCKA
//! fall samples SDCKB; an SDCKB fall samples SDCKA once an SDCKA fall has been
//! seen since the last idle gap. Instrumentation rides along for the block-253
//! question — pause positions, byte end positions, run lengths, and **double
//! transitions**: samples where both lines changed at once. Maple never moves
//! both lines in one step, so a double means an intermediate bus state fell
//! between two samples and the bit decoded there is a guess.

use crate::wire;

/// An edge-free run longer than this, after the first data bit, is an
/// inter-chunk pause rather than a bit phase (same threshold as the stock
/// decoder).
pub const GAP_THRESHOLD: u32 = 50;

/// An edge-free run longer than this, after the first data bit, ends the
/// reply (same threshold as the stock decoder).
pub const END_IDLE_THRESHOLD: u32 = 3000;

/// Pause end positions kept per decode. A block reply has 32.
pub const MAX_PAUSES: usize = 48;

/// How a decode ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// No data start pattern in the capture.
    NoStart,
    /// The frame's own length was reached; the checksum can be judged.
    Complete,
    /// The header announces more bytes than the output buffer holds.
    TooLong,
    /// The bus went quiet before the frame's length was reached.
    Ended,
    /// The capture ran out before the frame's length was reached.
    Exhausted,
}

/// What one decode saw, beyond the bytes. Everything but `data_start`,
/// `bits` and `scanned` is instrumentation and needs the `decode-stats`
/// feature.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Sample index decoding began at (the start pattern's closing SDCKA rise).
    pub data_start: u32,
    pub bits: u32,
    /// Samples examined in total, fast path included.
    pub scanned: u32,
    /// Sample index of the last edge examined.
    #[cfg(feature = "decode-stats")]
    pub last_edge: u32,
    /// Pauses (edge-free runs over [`GAP_THRESHOLD`]) after the first bit.
    #[cfg(feature = "decode-stats")]
    pub gaps: u32,
    #[cfg(feature = "decode-stats")]
    pub gap_total: u32,
    #[cfg(feature = "decode-stats")]
    pub max_gap: u32,
    /// Samples at which both lines changed at once.
    #[cfg(feature = "decode-stats")]
    pub doubles: u32,
    /// Byte index being assembled at the first double transition.
    #[cfg(feature = "decode-stats")]
    pub first_double_byte: Option<u16>,
    /// In-frame runs between edges of 1, 2, 3 and 4+ samples — samples per
    /// phase, directly. Pauses are not counted.
    #[cfg(feature = "decode-stats")]
    pub runs: [u32; 4],
    /// Samples passed over four at a time by the pause fast path.
    #[cfg(feature = "decode-stats")]
    pub skipped: u32,
}

/// Incremental decoder state: one instance per capture buffer.
///
/// [`begin`] resets it for the next capture. `A` and `B` pick SDCKA and SDCKB
/// out of a captured byte; they are const so the bit rule tests them as
/// immediates.
///
/// [`begin`]: BlockDecoder::begin
pub struct BlockDecoder<const A: u8, const B: u8> {
    cursor: usize,
    /// Previous sample, masked to the two Maple pins.
    last: u8,
    /// Index of the previous edge. Before the first edge it is the data start
    /// itself, one past the sample `last` was taken at: v252 counted the idle
    /// run's first sample — the previous edge — once it had been examined,
    /// and before the first edge nothing was, so holding `prev` one sample
    /// late makes the idle test `at - prev > GAP_THRESHOLD` in both cases.
    prev: usize,
    seen_a_fall: bool,
    /// The byte being assembled, behind a sentinel bit: 1 when empty, bit 8
    /// set once eight bits are in. The bit count is read off it.
    cur: u16,
    nbytes: usize,
    /// Frame length in bytes once the header is in; 0 before.
    expected: usize,
    /// XOR of every completed byte. Zero over a whole frame means the
    /// checksum byte matched.
    xor: u8,
    #[cfg(feature = "decode-stats")]
    pauses: [u16; MAX_PAUSES],
    #[cfg(feature = "decode-stats")]
    npauses: usize,
    stats: Stats,
    done: Option<Outcome>,
}

impl<const A: u8, const B: u8> Default for BlockDecoder<A, B> {
    fn default() -> Self {
        Self::new()
    }
}

/// The byte-wide [`wire::find_data_start_in`], instantiated in this crate.
///
/// `BlockDecoder` is const-generic, so [`begin`](BlockDecoder::begin) is
/// monomorphised by the firmware crate. Called through the generic directly,
/// the `scan_pattern::<u8>` behind it is instantiated there too, and lands
/// among the firmware's functions at a different size instead of among this
/// crate's — a text shift ahead of the Maple code, which alone has rolled the
/// poll loop before (found building v265). A
/// non-generic call keeps the instance here, where the non-generic decoder
/// had it.
fn find_data_start_u8(samples: &[u8], a_mask: u32, b_mask: u32) -> Option<usize> {
    // Opaque, or fat LTO specialises `scan_pattern` to the one call site's
    // constant pins and it comes out 20 bytes shorter than the non-generic
    // decoder's — the same shift by another route.
    wire::find_data_start_in(
        samples,
        core::hint::black_box(a_mask),
        core::hint::black_box(b_mask),
    )
}

impl<const A: u8, const B: u8> BlockDecoder<A, B> {
    const MASK: u8 = A | B;

    #[must_use]
    pub const fn new() -> Self {
        Self {
            cursor: 0,
            last: 0,
            prev: 0,
            seen_a_fall: false,
            cur: 1,
            nbytes: 0,
            expected: 0,
            xor: 0,
            #[cfg(feature = "decode-stats")]
            pauses: [0; MAX_PAUSES],
            #[cfg(feature = "decode-stats")]
            npauses: 0,
            stats: Stats {
                data_start: 0,
                bits: 0,
                scanned: 0,
                #[cfg(feature = "decode-stats")]
                last_edge: 0,
                #[cfg(feature = "decode-stats")]
                gaps: 0,
                #[cfg(feature = "decode-stats")]
                gap_total: 0,
                #[cfg(feature = "decode-stats")]
                max_gap: 0,
                #[cfg(feature = "decode-stats")]
                doubles: 0,
                #[cfg(feature = "decode-stats")]
                first_double_byte: None,
                #[cfg(feature = "decode-stats")]
                runs: [0; 4],
                #[cfg(feature = "decode-stats")]
                skipped: 0,
            },
            done: None,
        }
    }

    /// Reset for a new capture and find its data start. Returns
    /// `Some(Outcome::NoStart)` if there is none; otherwise decoding proceeds
    /// through [`step`](Self::step).
    pub fn begin(&mut self, samples: &[u8]) -> Option<Outcome> {
        *self = Self::new();
        let Some(start) = find_data_start_u8(samples, u32::from(A), u32::from(B)) else {
            self.done = Some(Outcome::NoStart);
            return self.done;
        };
        let start = start.max(1);
        self.cursor = start;
        self.prev = start;
        self.last = samples.get(start - 1).map_or(0, |&s| s & Self::MASK);
        self.stats.data_start = u32::try_from(start).unwrap_or(u32::MAX);
        None
    }

    /// Examine up to `quota` more samples (overrunning by at most three, to the
    /// end of a word). Returns the outcome once the decode has one; `None`
    /// means call again.
    ///
    /// `out` receives frame bytes in wire order and `pos` the sample index at
    /// which each byte's last bit was taken; `pos` must be at least as long as
    /// `out`.
    ///
    /// The per-edge state lives in locals for the duration of the slice — see
    /// the module doc for why — so the loop body is written out here rather
    /// than split into a per-edge function that would take a dozen `&mut`s.
    /// The per-byte state (`nbytes`, `expected`, `xor`, the output tables) is
    /// touched once per eight bits and stays in memory, where it does not
    /// crowd the registers the scan needs.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "sample indices are bounded by the capture, which is under 65,536 samples; \
                  stored as u16 to keep the per-byte position table small; `want` is `last` \
                  replicated into every byte lane, so its low byte is `last`"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "the slice loop is one hot path kept in registers; splitting it into calls \
                  is what cost v264 its cycles"
    )]
    pub fn step(
        &mut self,
        samples: &[u8],
        out: &mut [u8],
        pos: &mut [u16],
        quota: usize,
    ) -> Option<Outcome> {
        if self.done.is_some() {
            return self.done;
        }
        let lanes = u32::from(Self::MASK) * 0x0101_0101;
        let limit = samples.len().min(self.cursor.saturating_add(quota));
        let start = self.cursor;
        let mut idx = start;

        // The per-edge state, in registers for the slice. `want` is `last`
        // replicated into every byte lane, and stands in for it: lane 0 is
        // `last`, and `A`/`B` test it as immediates.
        let a_lanes = u32::from(A) * 0x0101_0101;
        let mut want = u32::from(self.last) * 0x0101_0101;
        let mut prev = self.prev;
        let mut seen_a_fall = self.seen_a_fall;
        let mut cur = self.cur;
        // The reply has ended once the scan reaches this index with no edge
        // since `prev`: `prev + END_IDLE_THRESHOLD + 2`, i.e. an edge-free run
        // longer than the threshold. Not before the first bit: `usize::MAX`
        // until then, which doubles as the "no bit yet" flag.
        let mut ended_at = if self.stats.bits > 0 {
            prev + END_IDLE_THRESHOLD as usize + 2
        } else {
            usize::MAX
        };
        #[cfg(feature = "decode-stats")]
        let mut skipped = self.stats.skipped;
        let mut done = None;

        'slice: loop {
            // The next edge, or the end of the slice or of the reply: four
            // samples per compare wherever four remain, one sample at the
            // tail. The compiler emits one `ldr` for the four bytes
            // (Cortex-M4 loads unaligned words), so an edge search costs a
            // load, an xor, an and and a branch per four samples, plus the
            // bound.
            let stop = limit.min(ended_at);
            let at = loop {
                if idx >= stop {
                    // No edge before the bound. If the bound was the end
                    // threshold, the reply has ended and there may never be
                    // a later edge to say so: a reply that stops early and
                    // idles to the end of the capture stalled v265's 22nd
                    // read for good (bench, 2026-09-15).
                    if ended_at <= idx {
                        idx = ended_at;
                        done = Some(Outcome::Ended);
                    }
                    break 'slice;
                }
                if let Some(&[s0, s1, s2, s3]) = samples.get(idx..idx + 4) {
                    let diff = (u32::from_le_bytes([s0, s1, s2, s3]) ^ want) & lanes;
                    if diff == 0 {
                        #[cfg(feature = "decode-stats")]
                        {
                            skipped += 4;
                        }
                        idx += 4;
                        continue;
                    }
                    break idx + (diff.trailing_zeros() / 8) as usize;
                }
                if samples[idx] & Self::MASK == want as u8 {
                    idx += 1;
                    continue;
                }
                break idx;
            };
            // Samples between the previous edge and this one, all equal to
            // `last`: v252's `quiet`. The reply has ended if that run is
            // longer than the end threshold, before this edge is looked at.
            if at >= ended_at {
                idx = ended_at;
                done = Some(Outcome::Ended);
                break;
            }
            idx = at + 1;
            let sample = samples[at] & Self::MASK;

            // The bit rule at an edge: v252's per-sample rule, with its
            // counters replaced by the run length.
            #[cfg(feature = "decode-stats")]
            {
                if ended_at != usize::MAX {
                    // Samples between the previous edge and this one, all
                    // equal to `last`: v252's `quiet`. `prev` is a real edge
                    // once a bit is in.
                    let quiet = at - prev - 1;
                    if quiet > GAP_THRESHOLD as usize {
                        let quiet32 = u32::try_from(quiet).unwrap_or(u32::MAX);
                        self.stats.gaps += 1;
                        self.stats.gap_total += quiet32;
                        self.stats.max_gap = self.stats.max_gap.max(quiet32);
                        if let Some(slot) = self.pauses.get_mut(self.npauses) {
                            *slot = at as u16;
                            self.npauses += 1;
                        }
                    } else {
                        self.stats.runs[quiet.min(3)] += 1;
                    }
                }
                let changed = sample ^ (want as u8);
                if changed & A != 0 && changed & B != 0 {
                    self.stats.doubles += 1;
                    if self.stats.first_double_byte.is_none() {
                        self.stats.first_double_byte = Some(self.nbytes as u16);
                    }
                }
                self.stats.last_edge = at as u32;
            }

            // Leaving the inter-chunk idle state (A high, B low) after a long
            // stay forgets the clock phase. This is an edge, so if the bus was
            // idle it is leaving; the run length includes the previous edge
            // (see `prev`).
            let a_high = sample & A != 0;
            let b_high = sample & B != 0;
            if want == a_lanes && at - prev > GAP_THRESHOLD as usize {
                seen_a_fall = false;
            }

            let bit = if want & u32::from(A) != 0 && !a_high {
                seen_a_fall = true;
                Some(b_high)
            } else if want & u32::from(B) != 0 && !b_high && seen_a_fall {
                Some(a_high)
            } else {
                None
            };
            want = u32::from(sample) * 0x0101_0101;
            prev = at;

            if let Some(bit) = bit {
                cur = (cur << 1) | u16::from(bit);
                ended_at = at + END_IDLE_THRESHOLD as usize + 2;
                if cur & 0x100 != 0 {
                    let n = self.nbytes;
                    let (Some(byte), Some(end)) = (out.get_mut(n), pos.get_mut(n)) else {
                        done = Some(Outcome::TooLong);
                        break;
                    };
                    let byte_in = cur as u8;
                    cur = 1;
                    *byte = byte_in;
                    *end = at as u16;
                    self.xor ^= byte_in;
                    self.nbytes = n + 1;
                    if n + 1 == 4 {
                        self.expected = 4 + usize::from(out[0]) * 4 + 1;
                        if self.expected > out.len() {
                            done = Some(Outcome::TooLong);
                            break;
                        }
                    }
                    if n + 1 == self.expected {
                        done = Some(Outcome::Complete);
                        break;
                    }
                }
            } else if ended_at != usize::MAX {
                ended_at = at + END_IDLE_THRESHOLD as usize + 2;
            }
        }

        self.last = want as u8;
        self.prev = prev;
        self.seen_a_fall = seen_a_fall;
        self.cur = cur;
        // Whole bytes, plus the bits behind the sentinel.
        self.stats.bits = u32::try_from(self.nbytes).unwrap_or(0) * 8
            + u32::from(cur).checked_ilog2().unwrap_or(0);
        #[cfg(feature = "decode-stats")]
        {
            self.stats.skipped = skipped;
        }
        self.stats.scanned += u32::try_from(idx - start).unwrap_or(u32::MAX);
        self.cursor = idx;
        if done.is_none() && idx >= samples.len() {
            done = Some(Outcome::Exhausted);
        }
        self.done = done;
        done
    }

    /// Bytes completed so far.
    #[must_use]
    pub const fn nbytes(&self) -> usize {
        self.nbytes
    }

    /// Frame length from the header, once known.
    #[must_use]
    pub const fn expected(&self) -> Option<usize> {
        if self.expected == 0 {
            None
        } else {
            Some(self.expected)
        }
    }

    /// XOR over every completed byte: zero across a complete frame means the
    /// checksum matched.
    #[must_use]
    pub const fn xor(&self) -> u8 {
        self.xor
    }

    #[must_use]
    pub const fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Sample indices at which each recorded pause ended.
    #[cfg(feature = "decode-stats")]
    #[must_use]
    pub fn pauses(&self) -> &[u16] {
        &self.pauses[..self.npauses]
    }

    /// Samples consumed so far (the resume point).
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }
}

#[cfg(test)]
#[expect(
    clippy::many_single_char_names,
    reason = "waveform tests transcribe v252's decoder, whose a/b/s/w names are the point of comparison"
)]
mod tests {
    extern crate std;
    use super::*;
    use std::format;
    use std::vec;
    use std::vec::Vec;

    use crate::test_wave::{block_frame, Wave, A, B};

    /// v252's decode (`vmu_diag::analyze` at 8c3063e), transcribed: the
    /// reference the fast decoder must match.
    struct Ref {
        bytes: Vec<u8>,
        bits: usize,
        gaps: u32,
        gap_total: usize,
        max_gap: usize,
        runs: [u32; 4],
        pauses: Vec<u16>,
        doubles: u32,
        last_edge: u32,
        ends: Vec<u16>,
    }

    /// `stop_bits`: the frame's length in bits, where the decoder under test
    /// stops; the reference stops there too so the stats compare like for like.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "mirrors the decoder: positions are stored as u16 and wrap past 65,535 samples on \
                  both sides, so the comparison holds even for the slow-rate waveforms that exceed it"
    )]
    fn reference(s: &[u8], stop_bits: usize) -> Option<Ref> {
        let start = wire::find_data_start_in(s, u32::from(A), u32::from(B))?.max(1);
        let mut r = Ref {
            bytes: vec![0; 640],
            bits: 0,
            gaps: 0,
            gap_total: 0,
            max_gap: 0,
            runs: [0; 4],
            pauses: Vec::new(),
            doubles: 0,
            last_edge: 0,
            ends: Vec::new(),
        };
        let mut last_a = s[start - 1] & A != 0;
        let mut last_b = s[start - 1] & B != 0;
        let (mut idle, mut quiet, mut seen) = (0usize, 0usize, false);
        for (at, &w) in s.iter().enumerate().skip(start) {
            let a = w & A != 0;
            let b = w & B != 0;
            if a == last_a && b == last_b {
                quiet += 1;
                if r.bits > 0 && quiet > END_IDLE_THRESHOLD as usize {
                    break;
                }
            } else {
                if r.bits > 0 {
                    if quiet > GAP_THRESHOLD as usize {
                        r.gaps += 1;
                        r.gap_total += quiet;
                        r.max_gap = r.max_gap.max(quiet);
                        r.pauses.push(at as u16);
                    } else {
                        r.runs[quiet.min(3)] += 1;
                    }
                }
                if a != last_a && b != last_b {
                    r.doubles += 1;
                }
                quiet = 0;
                r.last_edge = at as u32;
            }
            if a && !b {
                idle += 1;
            } else {
                if idle > GAP_THRESHOLD as usize {
                    seen = false;
                }
                idle = 0;
            }
            let bit = if last_a && !a {
                seen = true;
                Some(b)
            } else if last_b && !b {
                seen.then_some(a)
            } else {
                None
            };
            if let Some(bit) = bit {
                let idx = r.bits / 8;
                if idx < r.bytes.len() {
                    r.bytes[idx] = (r.bytes[idx] << 1) | u8::from(bit);
                }
                r.bits += 1;
                if r.bits.is_multiple_of(8) {
                    r.ends.push(at as u16);
                }
                if r.bits == stop_bits {
                    break;
                }
            }
            last_a = a;
            last_b = b;
        }
        Some(r)
    }

    fn decode(s: &[u8], quota: usize) -> (Option<Outcome>, BlockDecoder<A, B>, Vec<u8>) {
        let (o, d, out, _) = decode_pos(s, quota);
        (o, d, out)
    }

    fn decode_pos(
        s: &[u8],
        quota: usize,
    ) -> (Option<Outcome>, BlockDecoder<A, B>, Vec<u8>, Vec<u16>) {
        let mut d = BlockDecoder::<A, B>::new();
        let mut out = vec![0u8; 528];
        let mut pos = vec![0u16; 528];
        if let Some(o) = d.begin(s) {
            return (Some(o), d, out, pos);
        }
        let mut guard = 0;
        loop {
            if let Some(o) = d.step(s, &mut out, &mut pos, quota) {
                out.truncate(d.nbytes());
                pos.truncate(d.nbytes());
                return (Some(o), d, out, pos);
            }
            guard += 1;
            assert!(guard < 1_000_000, "decoder made no progress");
        }
    }

    #[test]
    fn a_clean_block_decodes_completely_with_a_zero_checksum() {
        let f = block_frame(255, 7);
        let mut w = Wave::new(1, 2, 3);
        w.frame(&f, (500, 800));
        let (o, d, out) = decode(&w.s, 4096);
        assert_eq!(o, Some(Outcome::Complete));
        assert_eq!(out, f);
        assert_eq!(d.xor(), 0);
        assert_eq!(d.stats().bits as usize, f.len() * 8);
        #[cfg(feature = "decode-stats")]
        {
            assert_eq!(d.stats().gaps, 32);
            assert_eq!(d.pauses().len(), 32);
            assert_eq!(d.stats().doubles, 0);
            assert!(d.stats().skipped > 0);
        }
    }

    #[test]
    fn matches_the_v252_reference_across_rates_seeds_and_slice_sizes() {
        for seed in 1..40u32 {
            for &(lo, hi) in &[(1, 1), (1, 2), (1, 3), (2, 4), (3, 6)] {
                let f = block_frame(seed.to_le_bytes()[0], seed * 31 + 3);
                let mut w = Wave::new(seed, lo, hi);
                w.frame(&f, (60, 900));
                let r = reference(&w.s, f.len() * 8).expect("reference found a start");
                for quota in [1, 3, 4, 7, 64, 4096, usize::MAX] {
                    let (o, d, out, pos) = decode_pos(&w.s, quota);
                    let tag = format!("seed {seed} rate {lo}-{hi} q {quota}");
                    assert_eq!(o, Some(Outcome::Complete), "{tag}");
                    assert_eq!(out[..], r.bytes[..f.len()], "{tag}");
                    assert_eq!(out, f);
                    assert_eq!(d.stats().bits as usize, f.len() * 8, "{tag}");
                    assert_eq!(pos, r.ends[..f.len()], "{tag}");
                    assert!(r.bits >= f.len() * 8);
                    #[cfg(feature = "decode-stats")]
                    {
                        assert_eq!(d.stats().gaps, r.gaps, "{tag}");
                        assert_eq!(d.stats().gap_total as usize, r.gap_total, "{tag}");
                        assert_eq!(d.stats().max_gap as usize, r.max_gap, "{tag}");
                        assert_eq!(d.stats().runs, r.runs, "{tag}");
                        assert_eq!(d.pauses(), &r.pauses[..], "{tag}");
                        assert_eq!(d.stats().doubles, r.doubles, "{tag}");
                        assert_eq!(d.stats().last_edge, r.last_edge, "{tag}");
                    }
                }
            }
        }
    }

    /// Host-side speed probe, relative only (x86 ≠ Cortex-M4): `cargo test
    /// --release -- --ignored --nocapture decode_speed`. The device figure is
    /// tag 13 of the read diagnostic (cycles per scanned sample × 100).
    #[test]
    #[ignore = "host timing probe, relative only; run by hand with --nocapture"]
    fn decode_speed() {
        let f = block_frame(255, 7);
        let mut w = Wave::new(1, 2, 2);
        w.frame(&f, (700, 900));
        let s = &w.s;
        let mut d = BlockDecoder::<A, B>::new();
        let mut out = vec![0u8; 528];
        let mut pos = vec![0u16; 528];
        let n = 2000;
        let t = std::time::Instant::now();
        for _ in 0..n {
            assert!(d.begin(s).is_none());
            while d.step(s, &mut out, &mut pos, 4096).is_none() {}
        }
        let per = t.elapsed().as_nanos() / n;
        std::println!(
            "decode: {} samples, {} scanned, stats {}, {} ns per block on this host",
            s.len(),
            d.stats().scanned,
            cfg!(feature = "decode-stats"),
            per
        );
    }

    #[cfg(feature = "decode-stats")]
    #[test]
    fn a_dropped_intermediate_state_is_counted_as_a_double() {
        let f = block_frame(3, 99);
        let mut w = Wave::new(5, 2, 2);
        w.frame(&f, (500, 500));
        // Find a sample run of (A=0,B=0) inside the data and delete it, so
        // the bus appears to jump from (1,0) straight to (0,1).
        let start = wire::find_data_start_in(&w.s, u32::from(A), u32::from(B)).unwrap() + 400;
        let mut k = start;
        while !(w.s[k] & (A | B) == 0 && w.s[k - 1] & (A | B) == A) {
            k += 1;
        }
        let mut end = k;
        while w.s[end] & (A | B) == 0 {
            end += 1;
        }
        let mut s = w.s.clone();
        s.drain(k..end);
        let (_, d, _) = decode(&s, 4096);
        assert_eq!(d.stats().doubles, 1);
        assert!(d.stats().first_double_byte.is_some());
    }

    /// v265 on the bench: the 22nd read never finished and every later read
    /// slot was deferred. A reply that stops early and then idles to the end
    /// of the capture has no edge after the idle run reaches the end
    /// threshold, and the slice loop only declared `Ended` on finding one.
    #[test]
    fn a_reply_that_stops_early_then_idles_ends_without_a_later_edge() {
        let f = block_frame(1, 5);
        let mut w = Wave::new(3, 2, 3);
        w.frame(&f[..200], (500, 800));
        for quota in [1, 7, 1024, 4096, usize::MAX] {
            let (o, d, _) = decode(&w.s, quota);
            assert_eq!(o, Some(Outcome::Ended), "quota {quota}");
            assert_eq!(d.nbytes(), 200, "quota {quota}");
        }
    }

    #[test]
    fn a_capture_cut_short_is_exhausted_not_complete() {
        let f = block_frame(1, 5);
        let mut w = Wave::new(3, 2, 3);
        w.frame(&f, (500, 800));
        let half = w.s.len() / 2;
        let (o, d, _) = decode(&w.s[..half], 4096);
        assert_eq!(o, Some(Outcome::Exhausted));
        assert!(d.nbytes() < f.len());
    }

    #[test]
    fn a_buffer_with_no_start_pattern_says_so() {
        let s = vec![A | B; 1000];
        let (o, _, _) = decode(&s, 4096);
        assert_eq!(o, Some(Outcome::NoStart));
    }

    #[test]
    fn a_header_longer_than_the_buffer_is_refused() {
        let mut f = block_frame(1, 5);
        f[0] = 200; // 4 + 800 + 1 bytes: over the 528-byte output
        let mut w = Wave::new(3, 2, 3);
        w.frame(&f, (500, 800));
        let (o, _, _) = decode(&w.s, 4096);
        assert_eq!(o, Some(Outcome::TooLong));
    }
}
