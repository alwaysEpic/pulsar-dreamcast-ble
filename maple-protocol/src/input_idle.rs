// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Is the pad being used? The gate on serving block reads.
//!
//! Protocol v1: *"The Pulsar serves a READ only while the pad is idle — no
//! stick or button change for ≥ 1 s — and stops serving the moment input
//! resumes. Reads never cost the pad during play."* That sentence is this
//! module.
//!
//! It is here rather than in the firmware for the reason `read_sched` is: the
//! firmware crate's tests do not run on-target, and "idle" is a rule with
//! edges — first sample, drift, a button held down — that deserve to be
//! exercised.
//!
//! # Drift, and why the anchor is not the previous sample
//!
//! A stick at rest dithers by a couple of counts, so comparing each sample with
//! the one before it and a threshold would let a slow push across the whole
//! range read as idle the entire way: every step is under the threshold. The
//! comparison is against an **anchor** — the sample idleness was last measured
//! from — so movement accumulates. Exceeding the threshold re-anchors and
//! restarts the clock.
//!
//! The threshold itself is 6 counts, just above the 5-count rest deadzone
//! `remap::RemapTable::DEFAULT` uses for the same physical reason. Buttons and
//! triggers are compared exactly: a trigger is only analogue in the sense that
//! it reports 0 or a pressure, and a resting finger is not noise.

use crate::controller_state::ControllerState;

/// How long the pad must be untouched before a read may be served: 1 s of
/// RTC1 ticks at 32,768 Hz, the clock `read_sched` is fed from.
pub const IDLE_AFTER_TICKS: u64 = 32_768;

/// Stick movement from the anchor that counts as input, in raw counts.
///
/// Six, because the remap's rest deadzone is five: anything the stick does at
/// rest is already known to fit inside that, and one count of margin keeps a
/// pad whose centre sits on the boundary from flickering.
pub const STICK_MOVE: u8 = 6;

/// Trigger movement from the anchor that counts as input.
///
/// Also six rather than exact, for the same reason and the same part: the
/// triggers are analogue potentiometers on the same ADC.
pub const TRIGGER_MOVE: u8 = 6;

/// Tracks how long the pad has been untouched.
#[derive(Clone, Copy, Debug)]
pub struct IdleWatch {
    /// The sample idleness is measured from, and the tick it was taken at.
    anchor: ControllerState,
    since: u64,
    /// No sample has been seen yet, so `anchor` and `since` mean nothing.
    fresh: bool,
}

impl Default for IdleWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleWatch {
    #[must_use]
    pub fn new() -> Self {
        Self {
            // Ignored until the first sample lands: `fresh` is what says the
            // anchor means nothing yet.
            anchor: ControllerState::default(),
            since: 0,
            fresh: true,
        }
    }

    /// Take a fresh controller sample. Returns whether the pad is idle *now*.
    ///
    /// Called with every sample the poll loop gets, and only with fresh ones: a
    /// window that took no sample must not age the clock forward on stale
    /// input, which is why this takes the state rather than reading it.
    pub fn note(&mut self, state: &ControllerState, now: u64) -> bool {
        if self.fresh || moved(&self.anchor, state) {
            self.anchor = *state;
            self.since = now;
            self.fresh = false;
            return false;
        }
        now.saturating_sub(self.since) >= IDLE_AFTER_TICKS
    }

    /// Idleness without a new sample — what the read gate asks at a window that
    /// took none.
    ///
    /// Before the first sample this is `false`: nothing is known about the pad
    /// yet, and the conservative reading of "unknown" is "in use".
    #[must_use]
    pub const fn idle_at(&self, now: u64) -> bool {
        !self.fresh && now.saturating_sub(self.since) >= IDLE_AFTER_TICKS
    }

    /// Forget everything. The pad is gone, or the session is new, and the old
    /// anchor says nothing about the next one.
    pub const fn reset(&mut self) {
        self.fresh = true;
    }
}

/// Whether `now` differs from `anchor` by more than rest noise.
fn moved(anchor: &ControllerState, now: &ControllerState) -> bool {
    anchor.buttons != now.buttons
        || anchor.stick_x.abs_diff(now.stick_x) > STICK_MOVE
        || anchor.stick_y.abs_diff(now.stick_y) > STICK_MOVE
        || anchor.trigger_l.abs_diff(now.trigger_l) > TRIGGER_MOVE
        || anchor.trigger_r.abs_diff(now.trigger_r) > TRIGGER_MOVE
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = IDLE_AFTER_TICKS;

    fn rest() -> ControllerState {
        ControllerState::default()
    }

    #[test]
    fn the_first_sample_is_never_idle() {
        let mut w = IdleWatch::new();
        assert!(!w.note(&rest(), 0));
        assert!(!w.idle_at(0));
    }

    #[test]
    fn a_second_of_rest_is_idle_and_not_a_tick_less() {
        let mut w = IdleWatch::new();
        assert!(!w.note(&rest(), 100));
        assert!(!w.note(&rest(), 100 + SECOND - 1));
        assert!(w.note(&rest(), 100 + SECOND));
        assert!(w.idle_at(100 + SECOND));
    }

    #[test]
    fn a_button_ends_idle_at_once_and_restarts_the_clock() {
        let mut w = IdleWatch::new();
        assert!(!w.note(&rest(), 0));
        assert!(w.note(&rest(), SECOND));

        let mut pressed = rest();
        pressed.buttons.a = true;
        assert!(!w.note(&pressed, SECOND));
        // Held is not moving, but the clock restarted when it went down.
        assert!(!w.note(&pressed, SECOND + SECOND - 1));
        assert!(w.note(&pressed, SECOND * 2));
        // And releasing it is input again.
        assert!(!w.note(&rest(), SECOND * 2));
    }

    #[test]
    fn rest_dither_does_not_end_idle() {
        let mut w = IdleWatch::new();
        let mut s = rest();
        assert!(!w.note(&s, 0));
        for (i, dx) in [1u8, 0, 2, 1, 3, 0].into_iter().enumerate() {
            s.stick_x = 128 + dx;
            let now = (i as u64 + 1) * 1_000;
            assert!(!w.note(&s, now), "dither must not restart the clock");
        }
        s.stick_x = 128 + STICK_MOVE;
        assert!(w.note(&s, SECOND), "still inside the threshold");
    }

    /// The reason the anchor is not the previous sample: a push that moves one
    /// count per poll never takes a *step* bigger than the threshold, so
    /// against the previous sample it would read as rest the whole way across.
    ///
    /// One poll is 492 ticks (15.015 ms, `read_sched::READ_LCD_GAP_TICKS`), so
    /// these twenty polls span 0.3 s — a third of the idle window, with the
    /// stick crossing twenty counts inside it.
    #[test]
    fn a_slow_push_is_input_even_though_no_step_exceeds_the_threshold() {
        const POLL: u64 = 492;
        let mut w = IdleWatch::new();
        let mut s = rest();
        assert!(!w.note(&s, 0));
        let mut idle_seen = false;
        for step in 1..=20u64 {
            s.stick_x = 128 + u8::try_from(step).unwrap_or(u8::MAX);
            idle_seen |= w.note(&s, POLL * step);
        }
        assert!(!idle_seen, "a stick crossing 20 counts is being used");
        // And the clock is still restarting: the last re-anchor was recent.
        assert!(!w.idle_at(POLL * 20));
    }

    /// The flip side, and the honest limit of the rule: a *single* count of
    /// drift per second is rest, and is read as rest. The gate is "untouched
    /// for a second", not "identical for a second".
    #[test]
    fn one_count_of_drift_a_second_is_still_rest() {
        let mut w = IdleWatch::new();
        let mut s = rest();
        assert!(!w.note(&s, 0));
        s.stick_x = 129;
        assert!(w.note(&s, SECOND));
    }

    #[test]
    fn a_trigger_squeeze_is_input() {
        let mut w = IdleWatch::new();
        let mut s = rest();
        assert!(!w.note(&s, 0));
        assert!(w.note(&s, SECOND));
        s.trigger_r = TRIGGER_MOVE + 1;
        assert!(!w.note(&s, SECOND));
    }

    #[test]
    fn reset_makes_the_next_sample_the_first_one_again() {
        let mut w = IdleWatch::new();
        assert!(!w.note(&rest(), 0));
        assert!(w.note(&rest(), SECOND));
        w.reset();
        assert!(!w.idle_at(SECOND));
        assert!(!w.note(&rest(), SECOND));
        assert!(w.note(&rest(), SECOND * 2));
    }
}
