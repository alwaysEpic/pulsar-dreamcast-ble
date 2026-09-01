// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Wire-level Maple Bus framing over a captured sample stream.
//!
//! Bulk RX captures raw GPIO words and decodes them afterwards. Decoding is
//! only correct if it starts at the right sample, and *finding* that sample is
//! pure logic over the buffer — so it lives here, where it can be tested on a
//! host against synthetic waveforms, rather than in the register-level driver.
//!
//! # Why the start pattern must be validated, not estimated
//!
//! The Maple start pattern is fully specified and countable: SDCKA falls, SDCKB
//! is toggled **exactly 4 times** while SDCKA stays low, then SDCKA rises. The
//! toggle count is the frame *type* — 8 toggles is light-gun detect and 14 is
//! reset, neither of which carries a data frame ([`docs/maple_bus_protocol.md`],
//! "Special Sequences").
//!
//! The previous approach counted SDCKB transitions in a real-time busy loop and
//! then guessed the data offset from the index of the first captured edge
//! (`first_edge_idx > 100`). That threshold encodes one device's reply latency.
//! The Maple spec only puts a *floor* on response time — a peripheral answers
//! some time after 50us from the bus going neutral — so a sub-peripheral that
//! answers with different latency than the controller lands on the wrong side
//! of the threshold and every frame decodes as noise. That is exactly why the
//! VMU's reply to a direct `0x01` device-info request never decoded.
//!
//! [`find_data_start`] instead runs the same state machine the Raspberry Pi Pico
//! reference implementations put in PIO (OrangeFox86/DreamPicoPort `maple_in.pio`):
//! wait for A low, count B low->high toggles, and require A to stay low until
//! after B's final rise. Alignment then depends on the wire, not on who is
//! talking.
//!
//! One thing does **not** port from those implementations: they count toggles in
//! PIO at the pin, where every edge exists. This runs over a sampled buffer at
//! ~1.97 samples per peripheral phase, where edges can alias away — so the
//! accepted count is a window, not an equality. See [`MIN_TOGGLES`].
//!
//! [`docs/maple_bus_protocol.md`]: ../../../docs/maple_bus_protocol.md

/// SDCKB LOW->HIGH toggles a **data frame** start pattern is specified to carry.
/// TX sends exactly this many; RX accepts [`MIN_TOGGLES`]..=[`MAX_TOGGLES`].
pub const DATA_START_TOGGLES: u32 = 4;

/// Toggle counts RX accepts as a data frame.
///
/// The spec says exactly 4, and TX sends exactly 4 — but a receiver cannot
/// insist on it. Bulk capture runs ~7.9 Msamples/s against 250ns peripheral
/// phases: **1.97 samples per phase**, under two. A toggle can alias away
/// entirely, and the capture is triggered by SDCKA's own fall, so the first
/// toggle can also be clipped by trigger latency. Both cost at most one toggle.
///
/// Insisting on 4 does not make alignment safer, because the count is not what
/// carries alignment: the pattern ends at the same SDCKA rise whether 3 toggles
/// were resolved or 4, and that rise is what [`find_data_start`] returns. A
/// strict count only converts good frames into rejects, and a reject costs a
/// full ~3.1ms recapture and retry. Measured on hardware: strict `== 4` held
/// the link at 61.4 Hz with IQR 29.1 ms against a healthy 66.6 Hz / IQR 0.9 ms.
///
/// The count still does the job it is actually for — separating a data frame
/// from the only other patterns on the bus, light-gun detect (8) and reset
/// (14) — with the accept window ending well below either.
const MIN_TOGGLES: u32 = 3;
/// Upper bound of the accept window; leaves margin below light-gun detect (8)
/// while tolerating a spurious edge from ringing.
const MAX_TOGGLES: u32 = 5;

/// Outcome of scanning one candidate start pattern.
enum Scan {
    /// A valid 4-toggle data start pattern ended at this sample index.
    Match(usize),
    /// Not a data start pattern; resume the search from this sample index.
    Reject(usize),
    /// The buffer ended mid-pattern.
    Truncated,
}

/// Locate the end of the first Maple **data frame** start pattern in `samples`.
///
/// `a_mask` / `b_mask` select the SDCKA / SDCKB bits within each captured GPIO
/// word. Returns the index of the sample at which SDCKA rose to close the
/// pattern — that is, the first sample of the post-pattern bus state, and the
/// index to begin bit decoding from. The first SDCKA falling edge at or after
/// it clocks data bit 0 (phase 1, sample SDCKB).
///
/// Returns `None` if the buffer holds no complete data start pattern, which is
/// the correct response to a capture triggered by noise: the caller retries
/// rather than decoding garbage.
///
/// # Capture that begins mid-pattern
///
/// The RX capture is triggered *by* SDCKA falling, so the buffer typically
/// opens with SDCKA already low and the edge itself outside it. Sample 0 is
/// therefore treated as a candidate pattern start.
///
/// This is also why toggles are counted on SDCKB's **rising** edges rather than
/// its falling ones: `last_b` seeds from whatever state the capture opened in,
/// so a first falling edge lost to trigger latency costs nothing.
///
/// Losing a *rise* costs nothing either — see [`MIN_TOGGLES`]. The returned
/// index is the pattern's closing SDCKA rise, which is in the same place
/// regardless of how many toggles were resolved before it.
#[must_use]
pub fn find_data_start(samples: &[u32], a_mask: u32, b_mask: u32) -> Option<usize> {
    let mut i = 1;

    if !samples.is_empty() && (samples[0] & a_mask) == 0 {
        match scan_pattern(samples, 0, a_mask, b_mask) {
            Scan::Match(end) => return Some(end),
            Scan::Reject(resume) => i = resume,
            Scan::Truncated => return None,
        }
    }

    while i < samples.len() {
        // SDCKA falling edge — a candidate start pattern begins here.
        if (samples[i - 1] & a_mask) != 0 && (samples[i] & a_mask) == 0 {
            match scan_pattern(samples, i, a_mask, b_mask) {
                Scan::Match(end) => return Some(end),
                // Resume at the rejected pattern's own A-rise. Always > i, so
                // the search strictly advances.
                Scan::Reject(resume) => {
                    i = resume;
                    continue;
                }
                Scan::Truncated => return None,
            }
        }
        i += 1;
    }
    None
}

/// Scan one candidate pattern starting at `fall`, the sample where SDCKA went low.
fn scan_pattern(samples: &[u32], fall: usize, a_mask: u32, b_mask: u32) -> Scan {
    let mut toggles: u32 = 0;
    let mut last_b = (samples[fall] & b_mask) != 0;

    for (offset, &sample) in samples[fall + 1..].iter().enumerate() {
        let a = (sample & a_mask) != 0;
        let b = (sample & b_mask) != 0;

        // Count the B rise before testing A: at ~4 samples per 500ns phase the
        // final B rise and the A rise can land in the same captured word, and
        // that coincidence must not lose the fourth toggle.
        if !last_b && b {
            toggles += 1;
        }

        if a {
            let idx = fall + 1 + offset;
            // A rose while B is high, after a data-frame toggle count.
            // B low here means A rose before B's final rise — malformed.
            return if (MIN_TOGGLES..=MAX_TOGGLES).contains(&toggles) && b {
                Scan::Match(idx)
            } else {
                Scan::Reject(idx)
            };
        }

        last_b = b;
    }

    Scan::Truncated
}

#[cfg(test)]
mod tests {
    // The crate is `no_std`; tests build waveforms with growable buffers.
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const A: u32 = 1 << 5;
    const B: u32 = 1 << 6;

    /// Samples per half-bit (phase). The real capture runs ~7.9 Msamples/s
    /// against 500ns host phases and 250ns peripheral phases, so 2 and 4 both
    /// occur on the wire; tests run both to keep the finder rate-independent.
    const FAST: usize = 2;
    const SLOW: usize = 4;

    /// Builds a sample stream the way the bus drives it, so tests describe
    /// waveforms rather than buffer indices.
    struct Wave {
        samples: Vec<u32>,
        a: bool,
        b: bool,
        per_phase: usize,
    }

    impl Wave {
        fn new(per_phase: usize) -> Self {
            // Idle: both lines pulled high.
            Self {
                samples: Vec::new(),
                a: true,
                b: true,
                per_phase,
            }
        }

        /// Hold the current line state for one phase.
        fn hold(&mut self) -> &mut Self {
            let word = (u32::from(self.a) * A) | (u32::from(self.b) * B);
            for _ in 0..self.per_phase {
                self.samples.push(word);
            }
            self
        }

        fn set_a(&mut self, level: bool) -> &mut Self {
            self.a = level;
            self.hold()
        }

        fn set_b(&mut self, level: bool) -> &mut Self {
            self.b = level;
            self.hold()
        }

        /// Idle bus: A high, B high, for `phases` phases.
        fn idle(&mut self, phases: usize) -> &mut Self {
            self.a = true;
            self.b = true;
            for _ in 0..phases {
                self.hold();
            }
            self
        }

        /// A start pattern with an arbitrary toggle count: A low, B toggled
        /// `toggles` times low->high, B high, A high, B low. `toggles == 4` is
        /// a data frame; 8 is light-gun detect and 14 is reset.
        fn start_pattern(&mut self, toggles: u32) -> &mut Self {
            self.set_a(false);
            // B enters high. Each iteration drives one low->high toggle; the
            // last rise is emitted after the loop, before A rises — mirroring
            // `MapleBus::send_start_pattern`.
            for _ in 0..toggles - 1 {
                self.set_b(false);
                self.set_b(true);
            }
            self.set_b(false);
            self.set_b(true);
            self.set_a(true);
            self.set_b(false);
            self
        }

        /// Append data bits exactly as `MapleBus::write_bit` drives them.
        ///
        /// The phases ping-pong: phase 1 has A as clock and B as data, phase 2
        /// swaps them. The subtle part — and the reason a decoder can treat
        /// *every* A fall as phase 1 and *every* B fall as phase 2 without
        /// tracking phase — is that a clock line is left **low** at the end of
        /// its own phase and is raised again by the next one. So the data line
        /// always starts a phase low, and setting it can only raise it. Neither
        /// line ever falls except as the clock.
        fn data(&mut self, bits: &[u8]) -> &mut Self {
            // The start pattern exits at A high, B low — phase 1's entry state.
            let mut a_is_clock = true;
            for &bit in bits {
                if a_is_clock {
                    self.set_b(bit != 0); // data on B (low -> raise, or hold low)
                    self.set_a(false); // clock edge: receiver samples B
                    self.set_b(true); // raise B to be the next phase's clock
                } else {
                    self.set_a(bit != 0); // data on A (low -> raise, or hold low)
                    self.set_b(false); // clock edge: receiver samples A
                    self.set_a(true); // raise A to be the next phase's clock
                }
                a_is_clock = !a_is_clock;
            }
            self
        }

        fn build(&self) -> Vec<u32> {
            self.samples.clone()
        }
    }

    /// Decode bits from `start` using the same phase rules as the firmware
    /// decoder: A falling edge samples B, B falling edge samples A, and no
    /// phase-2 bit is accepted before the first phase-1 edge.
    fn decode_from(samples: &[u32], start: usize) -> Vec<u8> {
        let mut bits = Vec::new();
        let mut last_a = (samples[start - 1] & A) != 0;
        let mut last_b = (samples[start - 1] & B) != 0;
        let mut seen_first_a_fall = false;

        for &sample in &samples[start..] {
            let a = (sample & A) != 0;
            let b = (sample & B) != 0;

            if last_a && !a {
                seen_first_a_fall = true;
                bits.push(u8::from(b));
            } else if last_b && !b && seen_first_a_fall {
                bits.push(u8::from(a));
            }

            last_a = a;
            last_b = b;
        }
        bits
    }

    #[test]
    fn finds_a_clean_data_start_pattern() {
        for rate in [FAST, SLOW] {
            let samples = Wave::new(rate)
                .idle(8)
                .start_pattern(4)
                .data(&[1, 0])
                .build();
            let start = find_data_start(&samples, A, B).expect("pattern not found");

            // The returned index is the A rise that closes the pattern.
            assert!((samples[start] & A) != 0, "rate {rate}: A must be high");
            assert!(
                (samples[start - 1] & A) == 0,
                "rate {rate}: A must have just risen"
            );
        }
    }

    #[test]
    fn decoding_from_the_returned_index_recovers_the_payload_bits() {
        // The thesis: alignment is correct if the bits come back out.
        let bits = [1, 0, 1, 1, 0, 0, 0, 1, 1, 0];
        for rate in [FAST, SLOW] {
            let samples = Wave::new(rate).idle(8).start_pattern(4).data(&bits).build();
            let start = find_data_start(&samples, A, B).expect("pattern not found");
            assert_eq!(decode_from(&samples, start), bits, "rate {rate}");
        }
    }

    #[test]
    fn alignment_is_independent_of_response_latency() {
        // The `first_edge_idx > 100` heuristic this replaces was calibrated to
        // the controller's reply latency. A sub-peripheral answering sooner or
        // later must decode identically.
        let bits = [1, 1, 0, 1, 0, 0, 1, 0];
        let mut decoded = Vec::new();
        for idle_phases in [1, 5, 40, 200] {
            let samples = Wave::new(SLOW)
                .idle(idle_phases)
                .start_pattern(4)
                .data(&bits)
                .build();
            let start = find_data_start(&samples, A, B).expect("pattern not found");
            decoded.push(decode_from(&samples, start));
        }
        for got in &decoded {
            assert_eq!(got, &bits);
        }
    }

    #[test]
    fn decodes_when_the_capture_opens_mid_start_pattern() {
        // The real capture is triggered by SDCKA falling, so the buffer opens
        // with A already low and some leading samples lost to trigger latency.
        // Everything up to and including the first B *rise* is expendable-minus-
        // one: losing the first rise must reject, not misalign.
        let bits = [1, 0, 0, 1, 1, 1, 0, 1];
        let full = Wave::new(SLOW).idle(8).start_pattern(4).data(&bits).build();
        let a_fall = full
            .windows(2)
            .position(|w| (w[0] & A) != 0 && (w[1] & A) == 0)
            .expect("no A fall")
            + 1;

        // Trim 0..=SLOW samples off the front: the trigger fires within one
        // phase of the edge, well before the first rise at ~2 phases.
        for lost in 0..=SLOW {
            let samples = &full[a_fall + lost..];
            let start = find_data_start(samples, A, B).expect("pattern not found");
            assert_eq!(decode_from(samples, start), bits, "lost {lost} samples");
        }
    }

    #[test]
    fn a_capture_that_loses_a_toggle_still_aligns() {
        // At 1.97 samples per 250ns peripheral phase a toggle can alias away,
        // and trigger latency can clip the first one. The pattern still ends at
        // the same A rise, so the frame must decode rather than cost a retry —
        // rejecting it was measured at IQR 29.1 ms against a healthy 0.9 ms.
        let bits = [1, 0, 1, 0, 1, 1, 0, 0];
        let full = Wave::new(SLOW).idle(8).start_pattern(4).data(&bits).build();
        let a_fall = full
            .windows(2)
            .position(|w| (w[0] & A) != 0 && (w[1] & A) == 0)
            .expect("no A fall")
            + 1;
        let first_rise = a_fall
            + full[a_fall..]
                .windows(2)
                .position(|w| (w[0] & B) == 0 && (w[1] & B) != 0)
                .expect("no B rise")
            + 1;

        let samples = &full[first_rise..];
        let start = find_data_start(samples, A, B).expect("frame lost to a clipped toggle");
        assert_eq!(decode_from(samples, start), bits);
    }

    #[test]
    fn accepts_the_whole_alias_tolerance_window() {
        // 3 and 5 are what a lost or spurious edge looks like; all must align.
        let bits = [1, 1, 0, 0, 1, 0];
        for toggles in [3, 4, 5] {
            let samples = Wave::new(SLOW)
                .idle(8)
                .start_pattern(toggles)
                .data(&bits)
                .build();
            let start = find_data_start(&samples, A, B)
                .unwrap_or_else(|| panic!("{toggles}-toggle pattern rejected"));
            assert_eq!(decode_from(&samples, start), bits, "{toggles} toggles");
        }
    }

    #[test]
    fn rejects_non_data_start_patterns() {
        // 8 toggles = light-gun detect, 14 = reset. Neither carries a frame,
        // and the accept window has to end well below both.
        for toggles in [1, 2, 8, 14] {
            let samples = Wave::new(SLOW).idle(8).start_pattern(toggles).build();
            assert_eq!(
                find_data_start(&samples, A, B),
                None,
                "{toggles}-toggle pattern must not decode as a data frame"
            );
        }
    }

    #[test]
    fn skips_a_rejected_pattern_and_finds_the_data_frame_behind_it() {
        let bits = [0, 1, 1, 0];
        let samples = Wave::new(SLOW)
            .idle(4)
            .start_pattern(14) // reset
            .idle(4)
            .start_pattern(4) // the real frame
            .data(&bits)
            .build();
        let start = find_data_start(&samples, A, B).expect("pattern not found");
        assert_eq!(decode_from(&samples, start), bits);
    }

    #[test]
    fn returns_none_on_a_truncated_pattern() {
        // Capture that ended mid-start-pattern: A low, two toggles, no A rise.
        let mut wave = Wave::new(SLOW);
        wave.idle(8).set_a(false);
        for _ in 0..2 {
            wave.set_b(false);
            wave.set_b(true);
        }
        assert_eq!(find_data_start(&wave.build(), A, B), None);
    }

    #[test]
    fn returns_none_on_an_idle_buffer() {
        let samples = Wave::new(SLOW).idle(64).build();
        assert_eq!(find_data_start(&samples, A, B), None);
    }

    #[test]
    fn rejects_a_pattern_whose_a_rises_before_b() {
        // "A must not transition HIGH until after B transitions HIGH." Four
        // toggles are present but A rises while B is still low.
        let mut wave = Wave::new(SLOW);
        wave.idle(8).set_a(false);
        for _ in 0..4 {
            wave.set_b(false);
            wave.set_b(true);
        }
        wave.set_b(false).set_a(true).idle(8);
        assert_eq!(find_data_start(&wave.build(), A, B), None);
    }
}
