// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Guide-button chord detection.
//!
//! The Dreamcast pad has no Guide/Xbox button, so it's synthesized from a
//! hold-chord: Left Trigger + Right Trigger fully pulled while Start is held.
//! This is the pure, host-testable state machine — the firmware feeds it a
//! monotonic millisecond clock and acts on the output (set the Guide bit,
//! flash the VMU home glyph). Time is a parameter, not an embedded `Instant`,
//! so the threshold / hold / edge-latch behavior can be unit-tested off-target.
//!
//! It matches ONLY L+R+Start — never A+B+X+Y+Start, which is the
//! Dreamcast/GDEMU soft-reset chord and must pass through untouched.

use crate::controller_state::ControllerState;

/// Analog-trigger value (0-255) that counts as "fully pulled" for the chord.
pub const GUIDE_TRIGGER_THRESHOLD: u8 = 200;
/// How long L+R+Start must be held continuously before Guide fires (ms).
pub const GUIDE_HOLD_MS: u64 = 300;

/// Result of one [`GuideChord::update`] step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuideChordOutput {
    /// The chord has been held past [`GUIDE_HOLD_MS`]; the caller should emit
    /// the Guide button and suppress its constituents (triggers + Start).
    pub active: bool,
    /// Rising edge of `active` — true on exactly the first step it becomes
    /// active, for firing one-shot side effects (e.g. the VMU home glyph).
    pub rising_edge: bool,
}

/// Hold-chord detector. Construct with [`Default`], call [`update`] each poll.
///
/// [`update`]: GuideChord::update
#[derive(Debug, Default)]
pub struct GuideChord {
    /// Monotonic ms when L+R+Start first became all-held; `None` while not held.
    since_ms: Option<u64>,
    /// Latches once `active` fires so `rising_edge` is reported only once.
    fired: bool,
}

impl GuideChord {
    /// True when the trigger values + Start constitute the chord being held.
    #[must_use]
    pub fn is_held(state: &ControllerState) -> bool {
        state.trigger_l >= GUIDE_TRIGGER_THRESHOLD
            && state.trigger_r >= GUIDE_TRIGGER_THRESHOLD
            && state.buttons.start
    }

    /// Advance the state machine. `now_ms` is a monotonic millisecond clock.
    /// Releasing the chord resets it, so a later re-hold fires a fresh edge.
    pub fn update(&mut self, state: &ControllerState, now_ms: u64) -> GuideChordOutput {
        if !Self::is_held(state) {
            self.since_ms = None;
            self.fired = false;
            return GuideChordOutput::default();
        }
        let started = *self.since_ms.get_or_insert(now_ms);
        let active = now_ms.saturating_sub(started) >= GUIDE_HOLD_MS;
        let rising_edge = active && !self.fired;
        if rising_edge {
            self.fired = true;
        }
        GuideChordOutput {
            active,
            rising_edge,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A controller state that is (or isn't) holding the L+R+Start chord.
    fn chord_state(held: bool) -> ControllerState {
        let mut s = ControllerState::default();
        if held {
            s.trigger_l = 255;
            s.trigger_r = 255;
            s.buttons.start = true;
        }
        s
    }

    #[test]
    fn not_held_is_inactive() {
        let mut gc = GuideChord::default();
        let out = gc.update(&chord_state(false), 0);
        assert!(!out.active);
        assert!(!out.rising_edge);
    }

    #[test]
    fn held_below_hold_is_inactive() {
        let mut gc = GuideChord::default();
        assert!(!gc.update(&chord_state(true), 0).active);
        // One ms short of the hold threshold.
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS - 1);
        assert!(!out.active);
        assert!(!out.rising_edge);
    }

    #[test]
    fn held_past_hold_activates_and_edges_once() {
        let mut gc = GuideChord::default();
        gc.update(&chord_state(true), 0);
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS);
        assert!(out.active);
        assert!(out.rising_edge, "first activation is a rising edge");
        // Continued hold stays active but the one-shot edge latches off.
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS + 500);
        assert!(out.active);
        assert!(!out.rising_edge, "edge fires only once per chord");
    }

    #[test]
    fn release_resets_and_can_refire() {
        let mut gc = GuideChord::default();
        gc.update(&chord_state(true), 0);
        assert!(gc.update(&chord_state(true), GUIDE_HOLD_MS).rising_edge);
        // Release clears state.
        assert!(!gc.update(&chord_state(false), GUIDE_HOLD_MS + 10).active);
        // Re-hold from scratch: the timer restarts and a fresh edge fires.
        let t = GUIDE_HOLD_MS + 20;
        assert!(!gc.update(&chord_state(true), t).active);
        let out = gc.update(&chord_state(true), t + GUIDE_HOLD_MS);
        assert!(out.active);
        assert!(out.rising_edge);
    }

    #[test]
    fn is_held_requires_both_triggers_and_start() {
        let mut s = ControllerState::default();
        assert!(!GuideChord::is_held(&s));
        s.trigger_l = GUIDE_TRIGGER_THRESHOLD;
        s.trigger_r = GUIDE_TRIGGER_THRESHOLD;
        assert!(!GuideChord::is_held(&s), "needs Start too");
        s.buttons.start = true;
        assert!(GuideChord::is_held(&s));
        // One trigger a hair below threshold breaks the chord.
        s.trigger_l = GUIDE_TRIGGER_THRESHOLD - 1;
        assert!(!GuideChord::is_held(&s));
    }
}
