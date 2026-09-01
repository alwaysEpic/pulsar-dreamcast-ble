// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Sync-button hold gesture.
//!
//! One press of the sync button can mean several things depending on how
//! long it is held and whether DFU is armed. This is the pure, host-testable
//! state machine — the firmware feeds it elapsed milliseconds and the armed flag,
//! then performs the side effects (blink the LED, set the DFU or sleep flag,
//! signal pairing mode). Time is a parameter, not an embedded `Instant`, so the
//! threshold and latch behaviour can be unit-tested off-target.
//!
//! The surrounding firmware also recognizes the owner-selected tap-then-hold
//! configuration chord at [`DFU_MS`]. Its press-count predicate lives here, but
//! its reboot side effect remains outside `SyncHold`; a Start-armed or tap-tap
//! DFU tick is evaluated first and takes priority.
//!
//! The thresholds are cumulative on a single hold:
//!
//! | Held past | DFU armed | Meaning |
//! |---|---|---|
//! | [`SYNC_MS`] | — | Release here to enter pairing mode |
//! | [`DFU_MS`] | yes | Request the OTA bootloader |
//! | [`SLEEP_MS`] | — | Commit to sleep; release does nothing further |
//!
//! **"Armed" has two sources, and this machine deliberately does not care which:**
//! the controller's **Start** held through the hold, or the **tap-tap-hold** chord
//! ([`dfu_chord_armed`]). Start alone was the original gesture, and it is
//! unreachable on a unit with no controller docked — or one whose Maple side is
//! failing, which is precisely when a firmware update is most needed — because the
//! Start mirror is only ever true while a controller is actively being polled.
//! The chord closes that hole without touching retail behaviour: a plain hold is
//! unchanged, so hold-to-sleep still works on a controller-less unit, which a
//! bare long-hold DFU entry would have broken.
//!
//! Two release rules are load-bearing and are the reason this is tested rather
//! than trusted:
//!
//! - **Holding through to sleep does not pair.** Sync clears the bond, and
//!   someone putting the device away has not asked to be unpaired.
//! - **Taking the DFU gesture does not pair either.** Asking for a firmware
//!   update is not asking to be unpaired. This matters most when the caller's
//!   battery gate then *refuses* the update: before this rule, a refused update
//!   still cost the user their pairing, with nothing on screen to explain it.

/// Hold duration to arm pairing mode.
pub const SYNC_MS: u64 = 2_000;
/// Hold duration to request OTA DFU. Requires DFU to be armed as well.
pub const DFU_MS: u64 = 3_500;
/// Hold duration that commits to sleep.
pub const SLEEP_MS: u64 = 7_000;

/// Short presses that must immediately precede the hold to arm DFU without a
/// controller — "tap, tap, hold".
///
/// Two, not three: the hold *is* the third press, so only two completed short
/// presses come before it. That also keeps it distinct from the profile toggle,
/// which needs three presses that are all short.
pub const DFU_CHORD_PRESSES: u8 = 2;

/// Window the chord's presses must land within, measured from the first.
///
/// Intentionally equal to the firmware's triple-press window — the two gestures
/// share a prefix, so a user learning one gets the same timing feel from the
/// other. Kept as its own constant so they can diverge without a silent surprise.
pub const DFU_CHORD_WINDOW_MS: u64 = 2_000;

/// Short presses before the hold for browser configuration: "tap, hold".
pub const CONFIG_CHORD_PRESSES: u8 = 1;

/// Whether the presses preceding a hold arm DFU without the controller.
///
/// `since_first_press_ms` is measured from the *first* press of the run, matching
/// how the firmware tracks its press counter. The window is exclusive, so a chord
/// that only just misses it is treated as an ordinary hold rather than ambiguously
/// firing.
#[must_use]
pub const fn dfu_chord_armed(press_count: u8, since_first_press_ms: u64) -> bool {
    press_count >= DFU_CHORD_PRESSES && since_first_press_ms < DFU_CHORD_WINDOW_MS
}

/// Whether the presses preceding a hold request the configuration personality.
///
/// This is an exact count, not a floor: two taps belong to controller-free DFU,
/// which has priority and must never be reinterpreted as configuration.
#[must_use]
pub const fn config_chord_armed(press_count: u8, since_first_press_ms: u64) -> bool {
    press_count == CONFIG_CHORD_PRESSES && since_first_press_ms < DFU_CHORD_WINDOW_MS
}

/// What the caller should do as a result of one [`SyncHold::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Nothing to do this step.
    None,
    /// Crossed [`SYNC_MS`] just now — speed the LED blink so the next
    /// threshold is visible before it is reached.
    PassedSync,
    /// Crossed [`DFU_MS`] with Start held — request the OTA bootloader.
    /// Emitted once per hold.
    RequestDfu,
    /// Reached [`SLEEP_MS`] — commit to sleep. The caller stops ticking.
    CommitSleep,
}

/// What a release means, given how the hold went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Released before [`SYNC_MS`] — treat as a short press (wake / reconnect).
    ShortPress,
    /// Released after [`SYNC_MS`] without taking the DFU gesture — enter
    /// pairing mode. **This clears the bond.**
    SyncMode,
    /// Released after requesting DFU. Deliberately *not* `SyncMode`: the bond
    /// must survive, including when the caller refuses the update.
    DfuRequested,
}

/// Sync-button hold state machine. Construct with [`Default`] on press, call
/// [`tick`] while held, then [`release`] once.
///
/// [`tick`]: SyncHold::tick
/// [`release`]: SyncHold::release
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncHold {
    past_sync: bool,
    dfu_requested: bool,
}

impl SyncHold {
    /// Advance the gesture. `elapsed_ms` is time since the press began;
    /// `dfu_armed` is the controller's Start mirror **or** the tap-tap-hold chord
    /// (see [`dfu_chord_armed`]) — the caller ORs them, this machine only asks
    /// whether DFU is permitted.
    ///
    /// Checked every step rather than only on the crossing, so Start pressed
    /// part-way through a hold still counts and a single failed controller poll
    /// only delays the trigger by one tick.
    pub const fn tick(&mut self, elapsed_ms: u64, dfu_armed: bool) -> Tick {
        if elapsed_ms >= SLEEP_MS {
            return Tick::CommitSleep;
        }
        if !self.dfu_requested && self.past_sync && elapsed_ms >= DFU_MS && dfu_armed {
            self.dfu_requested = true;
            return Tick::RequestDfu;
        }
        if !self.past_sync && elapsed_ms >= SYNC_MS {
            self.past_sync = true;
            return Tick::PassedSync;
        }
        Tick::None
    }

    /// Resolve the release. Call once, after the button goes up.
    #[must_use]
    pub const fn release(&self) -> Release {
        if self.dfu_requested {
            Release::DfuRequested
        } else if self.past_sync {
            Release::SyncMode
        } else {
            Release::ShortPress
        }
    }

    /// Whether the DFU gesture was taken during this hold.
    #[must_use]
    pub const fn dfu_requested(&self) -> bool {
        self.dfu_requested
    }
}

// The gesture thresholds must stay strictly ordered: each longer hold has to
// pass through the shorter one's window first. Asserted at compile time rather
// than in a test, so reordering them fails the build instead of only failing
// `cargo test` (same idiom as vmu.rs's layout bounds).
const _: () = assert!(SYNC_MS < DFU_MS);
const _: () = assert!(DFU_MS < SLEEP_MS);

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate is `no_std`; `heapless` is its only dependency, so tests use it
    /// rather than pulling in `std`.
    type Ticks = heapless::Vec<Tick, 8>;

    /// Drive a hold to `until_ms` in 20 ms steps, the firmware's poll cadence.
    fn hold_to(until_ms: u64, dfu_armed: bool) -> (SyncHold, Ticks) {
        let mut g = SyncHold::default();
        let mut ticks = Ticks::new();
        let mut t = 0;
        while t <= until_ms {
            match g.tick(t, dfu_armed) {
                Tick::None => {}
                other => ticks.push(other).expect("tick overflow"),
            }
            if ticks.last() == Some(&Tick::CommitSleep) {
                break;
            }
            t += 20;
        }
        (g, ticks)
    }

    #[test]
    fn short_press_releases_without_pairing() {
        let (g, ticks) = hold_to(SYNC_MS - 100, false);
        assert_eq!(g.release(), Release::ShortPress);
        assert!(ticks.is_empty());
    }

    #[test]
    fn past_sync_releases_into_pairing() {
        let (g, ticks) = hold_to(SYNC_MS + 100, false);
        assert_eq!(ticks.as_slice(), &[Tick::PassedSync]);
        assert_eq!(g.release(), Release::SyncMode);
    }

    /// The regression this module exists for: a hold that takes the DFU gesture
    /// must not also pair, because pairing clears the bond.
    #[test]
    fn dfu_gesture_does_not_clear_the_bond() {
        let (g, ticks) = hold_to(DFU_MS + 100, true);
        assert!(ticks.contains(&Tick::RequestDfu));
        assert!(g.dfu_requested());
        assert_eq!(
            g.release(),
            Release::DfuRequested,
            "releasing after the DFU gesture must not enter pairing mode"
        );
    }

    /// The caller's battery gate may refuse the update *after* the gesture is
    /// recognised. The bond must survive that too — the state machine cannot
    /// know the outcome, so the release must not depend on it.
    #[test]
    fn refused_dfu_still_preserves_the_bond() {
        let (g, _) = hold_to(DFU_MS + 500, true);
        assert_eq!(g.release(), Release::DfuRequested);
    }

    #[test]
    fn dfu_needs_start_held() {
        let (g, ticks) = hold_to(DFU_MS + 500, false);
        assert!(!ticks.contains(&Tick::RequestDfu));
        assert!(!g.dfu_requested());
        assert_eq!(
            g.release(),
            Release::SyncMode,
            "without Start it is a normal sync hold"
        );
    }

    #[test]
    fn dfu_fires_once_per_hold() {
        let (_, ticks) = hold_to(SLEEP_MS - 100, true);
        assert_eq!(ticks.iter().filter(|t| **t == Tick::RequestDfu).count(), 1);
    }

    /// Start pressed part-way through still counts — the check runs every step,
    /// not only on the `DFU_MS` crossing.
    #[test]
    fn start_pressed_mid_hold_still_requests_dfu() {
        let mut g = SyncHold::default();
        let mut fired = false;
        let mut t = 0;
        while t < SLEEP_MS {
            // Start goes down well after the DFU threshold has passed.
            let start = t >= DFU_MS + 400;
            if g.tick(t, start) == Tick::RequestDfu {
                fired = true;
            }
            t += 20;
        }
        assert!(fired);
    }

    #[test]
    fn holding_through_to_sleep_does_not_pair() {
        let (g, ticks) = hold_to(SLEEP_MS + 100, false);
        assert_eq!(ticks.last(), Some(&Tick::CommitSleep));
        assert_eq!(
            g.release(),
            Release::SyncMode,
            "state after commit is SyncMode, but the caller never releases — it sleeps"
        );
    }

    #[test]
    fn sleep_takes_priority_over_a_late_dfu() {
        let mut g = SyncHold::default();
        g.tick(SYNC_MS, false);
        assert_eq!(g.tick(SLEEP_MS, true), Tick::CommitSleep);
        assert!(!g.dfu_requested());
    }

    // ── The tap-tap-hold chord: DFU without a controller ────────────────────

    #[test]
    fn chord_arms_on_two_presses_inside_the_window() {
        assert!(dfu_chord_armed(DFU_CHORD_PRESSES, 0));
        assert!(dfu_chord_armed(DFU_CHORD_PRESSES, DFU_CHORD_WINDOW_MS - 1));
        assert!(
            dfu_chord_armed(DFU_CHORD_PRESSES + 1, 10),
            "more presses than needed still arms — the count is a floor"
        );
    }

    #[test]
    fn one_tap_is_not_the_chord() {
        assert!(!dfu_chord_armed(1, 0));
        assert!(
            !dfu_chord_armed(0, 0),
            "a plain hold must stay a plain hold — this is what keeps \
             hold-to-sleep working on a controller-less unit"
        );
    }

    #[test]
    fn chord_window_is_exclusive() {
        assert!(!dfu_chord_armed(DFU_CHORD_PRESSES, DFU_CHORD_WINDOW_MS));
        assert!(!dfu_chord_armed(DFU_CHORD_PRESSES, DFU_CHORD_WINDOW_MS + 1));
    }

    #[test]
    fn one_tap_arms_config_but_two_belong_to_dfu() {
        assert!(config_chord_armed(CONFIG_CHORD_PRESSES, 100));
        assert!(!config_chord_armed(DFU_CHORD_PRESSES, 100));
        assert!(dfu_chord_armed(DFU_CHORD_PRESSES, 100));
    }

    #[test]
    fn config_chord_uses_the_same_exclusive_window() {
        assert!(config_chord_armed(1, DFU_CHORD_WINDOW_MS - 1));
        assert!(!config_chord_armed(1, DFU_CHORD_WINDOW_MS));
        assert!(!config_chord_armed(0, 0));
    }

    /// The point of the whole change: with the chord arming it, the DFU gesture
    /// fires on a unit where Start can never be read because no controller is
    /// being polled. Same machine, same release rule — only the arming differs.
    #[test]
    fn chord_reaches_dfu_with_start_never_held() {
        let armed = dfu_chord_armed(DFU_CHORD_PRESSES, 100);
        assert!(armed);

        let mut g = SyncHold::default();
        let mut fired = false;
        let mut t = 0;
        while t < SLEEP_MS {
            // The Start mirror is permanently false on a controller-less unit,
            // so `armed` is the only thing arming this hold.
            if g.tick(t, armed) == Tick::RequestDfu {
                fired = true;
            }
            t += 20;
        }
        assert!(
            fired,
            "chord-armed hold must reach RequestDfu without Start"
        );
        assert_eq!(
            g.release(),
            Release::DfuRequested,
            "and must not pair on release, same as the Start-armed path"
        );
    }

    /// A chord-armed hold carried all the way to 7 s still sleeps rather than
    /// silently doing both — `SLEEP_MS` is checked first. Documents that adding
    /// the chord did not reorder the ladder.
    #[test]
    fn chord_does_not_outrank_sleep() {
        let (g, ticks) = hold_to(SLEEP_MS + 100, true);
        assert_eq!(ticks.last(), Some(&Tick::CommitSleep));
        assert!(
            g.dfu_requested(),
            "DFU was requested on the way past 3.5s; the caller reboots long \
             before 7s, so this only documents the ordering"
        );
    }
}
