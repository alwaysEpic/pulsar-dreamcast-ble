// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Sync-button hold gesture.
//!
//! One press of the sync button can mean four different things depending on how
//! long it is held and whether the controller's Start is down. This is the pure,
//! host-testable state machine — the firmware feeds it elapsed milliseconds and
//! the Start mirror, then performs the side effects (blink the LED, set the DFU
//! or sleep flag, signal pairing mode). Time is a parameter, not an embedded
//! `Instant`, so the threshold and latch behaviour can be unit-tested off-target.
//!
//! The thresholds are cumulative on a single hold:
//!
//! | Held past | With Start | Meaning |
//! |---|---|---|
//! | [`SYNC_MS`] | — | Release here to enter pairing mode |
//! | [`DFU_MS`] | yes | Request the OTA bootloader |
//! | [`SLEEP_MS`] | — | Commit to sleep; release does nothing further |
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
/// Hold duration to request OTA DFU. Requires the controller's Start held too.
pub const DFU_MS: u64 = 3_500;
/// Hold duration that commits to sleep.
pub const SLEEP_MS: u64 = 7_000;

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
    /// `start_held` mirrors the controller's Start button.
    ///
    /// Checked every step rather than only on the crossing, so Start pressed
    /// part-way through a hold still counts and a single failed controller poll
    /// only delays the trigger by one tick.
    pub fn tick(&mut self, elapsed_ms: u64, start_held: bool) -> Tick {
        if elapsed_ms >= SLEEP_MS {
            return Tick::CommitSleep;
        }
        if !self.dfu_requested && self.past_sync && elapsed_ms >= DFU_MS && start_held {
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
    pub fn release(&self) -> Release {
        if self.dfu_requested {
            Release::DfuRequested
        } else if self.past_sync {
            Release::SyncMode
        } else {
            Release::ShortPress
        }
    }

    /// Whether the DFU gesture was taken during this hold.
    pub fn dfu_requested(&self) -> bool {
        self.dfu_requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate is `no_std`; `heapless` is its only dependency, so tests use it
    /// rather than pulling in `std`.
    type Ticks = heapless::Vec<Tick, 8>;

    /// Drive a hold to `until_ms` in 20 ms steps, the firmware's poll cadence.
    fn hold_to(until_ms: u64, start_held: bool) -> (SyncHold, Ticks) {
        let mut g = SyncHold::default();
        let mut ticks = Ticks::new();
        let mut t = 0;
        while t <= until_ms {
            match g.tick(t, start_held) {
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
    /// not only on the DFU_MS crossing.
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

    #[test]
    fn thresholds_are_ordered() {
        assert!(SYNC_MS < DFU_MS && DFU_MS < SLEEP_MS);
    }
}
