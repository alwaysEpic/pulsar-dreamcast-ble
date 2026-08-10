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
/// How long the synthesized Guide button stays asserted once the chord fires —
/// a short *tap*, not a hold. Some hosts (notably the Steam Deck) treat a *held*
/// Guide/Steam button as a shortcut-chord modifier (Steam+R1 = screenshot,
/// Steam+L2/R2 = mouse clicks, …); a brief tap just opens the guide menu. Kept
/// well under any host's hold/long-press threshold but long enough to register.
pub const GUIDE_PULSE_MS: u64 = 80;

/// Result of one [`GuideChord::update`] step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuideChordOutput {
    /// L+R+Start are all held right now (the chord is forming *or* has fired).
    /// The caller should suppress the constituents — zero the triggers, clear
    /// Start — on every such frame, starting the instant all three go down. This
    /// is asserted from the first frame (not just after firing) so the 300ms
    /// arming window can't leak a Start (or trigger) press to the host before the
    /// Guide tap.
    pub suppress: bool,
    /// Emit the Guide button this step. True only for the short [`GUIDE_PULSE_MS`]
    /// window right after the chord fires — a *tap*, not a hold (see
    /// [`GUIDE_PULSE_MS`]). After the window it goes false even while the chord is
    /// still physically held.
    pub emit_guide: bool,
    /// Rising edge of the pulse — true on exactly the first step Guide is
    /// emitted, for firing one-shot side effects (e.g. the VMU home glyph).
    pub rising_edge: bool,
}

/// Hold-chord detector. Construct with [`Default`], call [`update`] each poll.
///
/// [`update`]: GuideChord::update
#[derive(Debug, Default)]
pub struct GuideChord {
    /// Monotonic ms when L+R+Start first became all-held; `None` while not held.
    since_ms: Option<u64>,
    /// Monotonic ms when the Guide pulse started (the step the chord fired);
    /// `None` until it fires. Latches the one-shot `rising_edge` and bounds the
    /// [`GUIDE_PULSE_MS`] tap window. Cleared on release so a re-hold re-fires.
    fired_ms: Option<u64>,
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
            self.fired_ms = None;
            return GuideChordOutput::default();
        }
        let started = *self.since_ms.get_or_insert(now_ms);
        if now_ms.saturating_sub(started) < GUIDE_HOLD_MS {
            // Forming: suppress the constituents from the first frame so the
            // arming window can't leak a Start/trigger press, but don't fire yet.
            return GuideChordOutput {
                suppress: true,
                emit_guide: false,
                rising_edge: false,
            };
        }
        // Past the hold threshold. Keep suppressing, and *pulse* Guide for
        // GUIDE_PULSE_MS — a tap, not a hold.
        let rising_edge = self.fired_ms.is_none();
        let fired_at = *self.fired_ms.get_or_insert(now_ms);
        let emit_guide = now_ms.saturating_sub(fired_at) < GUIDE_PULSE_MS;
        GuideChordOutput {
            suppress: true,
            emit_guide,
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
    fn not_held_suppresses_nothing() {
        let mut gc = GuideChord::default();
        let out = gc.update(&chord_state(false), 0);
        assert!(!out.suppress);
        assert!(!out.emit_guide);
        assert!(!out.rising_edge);
    }

    #[test]
    fn suppresses_from_the_first_frame_before_firing() {
        let mut gc = GuideChord::default();
        // Suppression starts immediately — the whole point: the arming window
        // must not leak a Start (or trigger) press to the host. But Guide hasn't
        // fired yet.
        let out = gc.update(&chord_state(true), 0);
        assert!(out.suppress, "suppress the instant all three are held");
        assert!(!out.emit_guide);
        assert!(!out.rising_edge);
        // One ms short of the hold threshold: still just suppressing, not firing.
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS - 1);
        assert!(out.suppress);
        assert!(!out.emit_guide);
        assert!(!out.rising_edge);
    }

    #[test]
    fn held_past_hold_fires_and_edges_once() {
        let mut gc = GuideChord::default();
        gc.update(&chord_state(true), 0);
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS);
        assert!(out.suppress);
        assert!(out.rising_edge, "first activation is a rising edge");
        // Continued hold stays suppressed but the one-shot edge latches off.
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS + 500);
        assert!(out.suppress);
        assert!(!out.rising_edge, "edge fires only once per chord");
    }

    #[test]
    fn guide_is_a_tap_not_a_hold() {
        let mut gc = GuideChord::default();
        // Arming: already suppressing, but Guide not emitted yet.
        let out = gc.update(&chord_state(true), 0);
        assert!(out.suppress);
        assert!(!out.emit_guide);
        // Fires at the hold threshold: the Guide pulse starts.
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS);
        assert!(out.suppress);
        assert!(out.emit_guide, "Guide asserts at the start of the pulse");
        assert!(out.rising_edge);
        // Still inside the pulse window: Guide stays asserted, edge latched off.
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS + GUIDE_PULSE_MS - 1);
        assert!(out.emit_guide);
        assert!(!out.rising_edge);
        // Past the pulse window but STILL physically held: Guide releases (it's a
        // tap), yet the constituents stay suppressed. This is the Steam Deck fix —
        // a *held* Guide arms the Deck's shortcut layer (Steam+R1 = screenshot).
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS + GUIDE_PULSE_MS);
        assert!(out.suppress, "still suppressing triggers/Start while held");
        assert!(
            !out.emit_guide,
            "Guide is a tap — released even though chord still held"
        );
        assert!(!out.rising_edge);
        // A *sustained* hold never re-pulses Guide — the invariant that keeps us
        // out of the Deck's hold-shortcut layer (Steam+R1 = screenshot). Constituents
        // stay suppressed the whole time.
        let out = gc.update(&chord_state(true), GUIDE_HOLD_MS + 1000);
        assert!(out.suppress);
        assert!(
            !out.emit_guide,
            "Guide never re-asserts during one sustained hold"
        );
        assert!(!out.rising_edge);
    }

    #[test]
    fn release_resets_and_can_refire() {
        let mut gc = GuideChord::default();
        gc.update(&chord_state(true), 0);
        assert!(gc.update(&chord_state(true), GUIDE_HOLD_MS).rising_edge);
        // Release clears state — suppression stops.
        assert!(!gc.update(&chord_state(false), GUIDE_HOLD_MS + 10).suppress);
        // Re-hold from scratch: suppressing again, but the timer restarted so it
        // hasn't re-fired yet.
        let t = GUIDE_HOLD_MS + 20;
        let out = gc.update(&chord_state(true), t);
        assert!(out.suppress);
        assert!(!out.rising_edge);
        // Held past the threshold again: a fresh edge fires.
        let out = gc.update(&chord_state(true), t + GUIDE_HOLD_MS);
        assert!(out.suppress);
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
