// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Low-battery policy, separated from the hardware that feeds it and the task
//! that acts on it.
//!
//! The firmware crate is `no_std` on-target and cannot run a test, and this is
//! the logic that decides when a unit powers itself off — so it lives here,
//! host-testable, with no dependencies at all. Three pieces, kept apart:
//!
//! - **Observation** — what one battery read established, field by field. A
//!   field that could not be read is `None`; it is never defaulted.
//! - **Policy** — [`Policy::observe`] folds an observation and a timestamp into
//!   a [`Decision`]. It owns all cutoff timing, so no phase of the firmware can
//!   shorten it by taking a reading of its own.
//! - **Action** — the caller's. This crate never sleeps, writes an LED or
//!   touches a bus.
//!
//! Time is plain milliseconds since boot (`u64`), so the crate needs no clock.
//! **Timestamps must be monotonic** — that is the contract, and the arithmetic
//! saturates rather than checks it. A `u64` of milliseconds does not wrap in
//! any lifetime that matters.

#![no_std]

/// What one battery read established. Each field stands alone: a charge flag
/// that read fine is still a fact when the gauge byte would not decode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// Whether the charger is running. `None` = the read failed.
    pub charging: Option<bool>,
    /// Whether the charger reports charge complete. `None` = the read failed,
    /// or the board's charger has no such state to report — so `None` is "not
    /// known to be complete", never evidence that the pack is not full.
    ///
    /// Presentation only: [`Snapshot::shown_percent`] reads it, the cutoff never
    /// does. If the bit stayed set while a plugged-in unit drained, a 100 fed to
    /// the empty-reading counter would reset it and suppress the cutoff.
    pub charge_complete: Option<bool>,
    /// State of charge, 0–100 — **as measured**. `None` = the read failed, or
    /// the gauge returned a code nobody can decode. The cutoff and the DFU gate
    /// read this; nothing may dress it up.
    pub percent: Option<u8>,
    /// Cell voltage. `None` on a gauge that reports no voltage (the IP5306).
    pub millivolts: Option<u32>,
}

/// A board's answer to "how is the battery?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    /// The board has no gauge at all (the dev kit). Not a failure.
    NoGauge,
    /// The board has a gauge; here is what it established this time, which may
    /// be nothing.
    Observed(Observation),
}

/// What the firmware is doing when it takes a reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// A controller is answering, or the unit is idle waiting for a host.
    Normal,
    /// The firmware is looking for a controller that is not answering.
    Searching,
}

/// Why the policy asked for a shutdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownReason {
    /// A percent-only gauge read at or below the cutoff, repeatedly, for long
    /// enough.
    EmptyGauge {
        /// The reading that completed the confirmation.
        percent: u8,
    },
    /// A voltage gauge read below the cutoff.
    Undervoltage {
        /// The reading that tripped it.
        millivolts: u32,
    },
}

/// What the caller should do about one observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing.
    Continue,
    /// The gauge reads empty and the cell is not known to be charging. Not yet
    /// a shutdown; worth showing.
    Warn,
    /// Power down now.
    Shutdown(ShutdownReason),
}

/// The tunable part of the policy. All times in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Whether the board can enter System Off at all. `false` disables every
    /// cutoff.
    pub can_sleep: bool,
    /// Voltage gauges: shut down below this.
    pub cutoff_millivolts: u32,
    /// Whether the voltage cutoff is evaluated while [`Context::Searching`].
    ///
    /// A separate decision from which confirmation span applies, though both
    /// turn on the context. `false` for now: the boost has just come up there
    /// and the cell is sagging under it, the XIAO has never been sampled in
    /// that state, and a single loaded dip must not sleep a healthy unit.
    /// Turning this on wants a settling policy for voltage readings first — not
    /// the IP5306's.
    pub voltage_cutoff_while_searching: bool,
    /// Percent-only gauges: a reading at or below this is "empty".
    pub cutoff_percent: u8,
    /// Counted empty readings required.
    pub empty_reads: u8,
    /// Time the counted readings must span while nothing answers on the bus.
    pub confirm_span_searching_ms: u64,
    /// Time the counted readings must span in normal operation. Separate from
    /// the searching span so that how *often* the battery is read never decides
    /// how *soon* a working unit is switched off.
    pub confirm_span_normal_ms: u64,
    /// Empty readings closer together than this count once.
    pub min_spacing_ms: u64,
    /// Longest gap between counted empty readings before confirmation starts
    /// over.
    pub max_gap_ms: u64,
    /// A retained fact older than this is no longer shown as current. Separate
    /// from `max_gap_ms`: one is about evidence, the other about presentation.
    pub display_expiry_ms: u64,
}

/// Empty observations with bounded gaps — not an "unbroken run". Unknown
/// intervals are tolerated by design: failed reads between two empty readings
/// change nothing, provided the gap stays within `max_gap_ms`.
#[derive(Clone, Copy, Debug)]
struct EmptyReadings {
    first_counted_ms: u64,
    last_counted_ms: u64,
    counted: u8,
}

/// A retained value and when it was established.
#[derive(Clone, Copy, Debug)]
struct Fact<T> {
    value: T,
    at_ms: u64,
}

/// What is currently fit to show. A field is `None` when it was never
/// established or has aged past [`Config::display_expiry_ms`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Last known state of charge, if still current.
    pub percent: Option<u8>,
    /// Last known charge state, if still current.
    pub charging: Option<bool>,
    /// Last known charge-complete report, if still current.
    pub charge_complete: Option<bool>,
}

impl Snapshot {
    /// The level to *show* — BLE, the LED bar, the VMU icon — given whether USB
    /// power is present: 100 when the charger reports charge complete on the
    /// cable, otherwise the measured level unchanged (which may be unknown).
    ///
    /// Presentation only. It exists because pulsarv1's gauge reads 100 only
    /// while the charger is actively topping and a rested-full cell decodes to
    /// 75, so a unit that had finished charging showed three bars on the cable
    /// (field report, 2026-09-21). It does **not** make the level diagnostic:
    /// 75 on the cable can still be a pack mid-charge, or a failed read of the
    /// complete bit. Neither the percentage nor VBUS says whether current is
    /// flowing.
    ///
    /// `vbus` gates it because the complete bit's behaviour once unplugged has
    /// never been verified on hardware: if it latches, an ungated 100 would hide
    /// the discharge on battery. And it is kept off [`Snapshot::percent`] for the
    /// same reason — see [`Observation::charge_complete`].
    #[must_use]
    pub const fn shown_percent(&self, vbus: bool) -> Option<u8> {
        if vbus && matches!(self.charge_complete, Some(true)) {
            Some(100)
        } else {
            self.percent
        }
    }
}

/// The one owner of low-battery state.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    config: Config,
    percent: Option<Fact<u8>>,
    charging: Option<Fact<bool>>,
    charge_complete: Option<Fact<bool>>,
    empty: Option<EmptyReadings>,
}

impl Policy {
    /// A policy that has observed nothing.
    #[must_use]
    pub const fn new(config: Config) -> Self {
        Self {
            config,
            percent: None,
            charging: None,
            charge_complete: None,
            empty: None,
        }
    }

    /// Fold one observation in.
    ///
    /// The rule for a percent-only gauge: shut down when a reading that
    /// *counts* brings the counted empty readings to `empty_reads`, with the
    /// first and last of them at least the confirmation span apart. A reading
    /// counts when it is at least `min_spacing_ms` after the last counted one.
    /// Only a counted reading can confirm — an uncounted one carries no new
    /// evidence, so it must not be able to finish a span the counted readings
    /// have not covered. A gap longer than `max_gap_ms`
    /// starts over. A known-charging or known-healthy reading clears the
    /// evidence; an unknown one leaves it alone.
    ///
    /// The span is the one for the context of the reading that would confirm.
    /// Evidence is never discarded because the context changed: a properly
    /// spaced third reading taken on entering a search confirms at once under
    /// the shorter span, and readings gathered while searching go on counting
    /// toward the longer span once a controller answers.
    ///
    /// The voltage rule is a single reading below the cutoff, with the cell
    /// known not to be charging. Whether it runs while searching is
    /// [`Config::voltage_cutoff_while_searching`].
    pub fn observe(&mut self, obs: Observation, now_ms: u64, context: Context) -> Decision {
        if let Some(value) = obs.charging {
            self.charging = Some(Fact {
                value,
                at_ms: now_ms,
            });
        }
        if let Some(value) = obs.charge_complete {
            self.charge_complete = Some(Fact {
                value,
                at_ms: now_ms,
            });
        }
        if let Some(value) = obs.percent {
            self.percent = Some(Fact {
                value,
                at_ms: now_ms,
            });
        }

        // `charge_complete` is retained above and read by no rule below: a
        // complete bit that outlived the cable must not be able to exempt a
        // draining cell the way a live `charging` does.
        if !self.config.can_sleep || obs.charging == Some(true) {
            self.empty = None;
            return Decision::Continue;
        }

        if let Some(millivolts) = obs.millivolts {
            self.empty = None;
            let evaluated =
                context == Context::Normal || self.config.voltage_cutoff_while_searching;
            let tripped = evaluated
                && obs.charging == Some(false)
                && millivolts < self.config.cutoff_millivolts;
            return if tripped {
                Decision::Shutdown(ShutdownReason::Undervoltage { millivolts })
            } else {
                Decision::Continue
            };
        }

        let Some(percent) = obs.percent else {
            return Decision::Continue;
        };
        if percent > self.config.cutoff_percent {
            self.empty = None;
            return Decision::Continue;
        }
        // Empty, but is it *discharging*? Without the charge flag there is no
        // telling, and a charging cell is exempt — so it is shown, not counted.
        if obs.charging.is_none() {
            return Decision::Warn;
        }

        let (empty, counted) = match self.empty {
            Some(e) if now_ms.saturating_sub(e.last_counted_ms) <= self.config.max_gap_ms => {
                if now_ms.saturating_sub(e.last_counted_ms) < self.config.min_spacing_ms {
                    (e, false)
                } else {
                    let e = EmptyReadings {
                        last_counted_ms: now_ms,
                        counted: e.counted.saturating_add(1),
                        ..e
                    };
                    (e, true)
                }
            }
            _ => {
                let e = EmptyReadings {
                    first_counted_ms: now_ms,
                    last_counted_ms: now_ms,
                    counted: 1,
                };
                (e, true)
            }
        };
        self.empty = Some(empty);

        let span = match context {
            Context::Searching => self.config.confirm_span_searching_ms,
            Context::Normal => self.config.confirm_span_normal_ms,
        };
        let confirmed = counted
            && empty.counted >= self.config.empty_reads
            && empty.last_counted_ms.saturating_sub(empty.first_counted_ms) >= span;
        if confirmed {
            Decision::Shutdown(ShutdownReason::EmptyGauge { percent })
        } else {
            Decision::Warn
        }
    }

    /// What is fit to show right now.
    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> Snapshot {
        let expiry = self.config.display_expiry_ms;
        Snapshot {
            percent: current(self.percent, now_ms, expiry),
            charging: current(self.charging, now_ms, expiry),
            charge_complete: current(self.charge_complete, now_ms, expiry),
        }
    }

    /// Counted empty readings so far and the time they span, for logging.
    #[must_use]
    pub fn empty_progress(&self) -> Option<(u8, u64)> {
        self.empty.map(|e| {
            (
                e.counted,
                e.last_counted_ms.saturating_sub(e.first_counted_ms),
            )
        })
    }
}

fn current<T: Copy>(fact: Option<Fact<T>>, now_ms: u64, expiry_ms: u64) -> Option<T> {
    let fact = fact?;
    (now_ms.saturating_sub(fact.at_ms) <= expiry_ms).then_some(fact.value)
}

/// Whether a firmware update may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DfuVerdict {
    /// Go ahead.
    Allowed,
    /// The battery reads below the minimum and is not known to be charging.
    RefusedLow {
        /// The reading.
        percent: u8,
    },
    /// The board has a gauge and it could not say how full the battery is.
    RefusedUnknown,
}

/// The update gate.
///
/// A flash interrupted by a brown-out is the failure this exists to prevent,
/// so off the cable the battery has to be *shown* adequate:
/// a gauge that could not be read is a refusal, not a pass. It used to be a
/// pass, and a partial read failure could discard a level that said "too low".
///
/// VBUS is checked first and always allows — that is the recovery route, and
/// the one that fixed the 2026-08-17 lockout of a unit sitting on a charger.
/// A board with no gauge has nothing to be refused on.
#[must_use]
pub const fn dfu_verdict(reading: Reading, vbus_present: bool, min_percent: u8) -> DfuVerdict {
    if vbus_present {
        return DfuVerdict::Allowed;
    }
    let obs = match reading {
        Reading::NoGauge => return DfuVerdict::Allowed,
        Reading::Observed(obs) => obs,
    };
    if matches!(obs.charging, Some(true)) {
        return DfuVerdict::Allowed;
    }
    match obs.percent {
        None => DfuVerdict::RefusedUnknown,
        Some(percent) if percent < min_percent => DfuVerdict::RefusedLow { percent },
        Some(_) => DfuVerdict::Allowed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000;

    /// The firmware's configuration for pulsarv1.
    const CONFIG: Config = Config {
        can_sleep: true,
        cutoff_millivolts: 3200,
        voltage_cutoff_while_searching: false,
        cutoff_percent: 0,
        empty_reads: 3,
        confirm_span_searching_ms: 20 * S,
        confirm_span_normal_ms: 120 * S,
        min_spacing_ms: 8 * S,
        max_gap_ms: 150 * S,
        display_expiry_ms: 150 * S,
    };

    const fn gauge(percent: u8) -> Observation {
        Observation {
            charging: Some(false),
            charge_complete: Some(false),
            percent: Some(percent),
            millivolts: None,
        }
    }

    const EMPTY: Observation = gauge(0);
    const UNKNOWN: Observation = Observation {
        charging: None,
        charge_complete: None,
        percent: None,
        millivolts: None,
    };
    const SHUTDOWN: Decision = Decision::Shutdown(ShutdownReason::EmptyGauge { percent: 0 });

    const fn volts(millivolts: u32) -> Observation {
        Observation {
            charging: Some(false),
            charge_complete: Some(false),
            percent: Some(50),
            millivolts: Some(millivolts),
        }
    }

    /// Feed `(seconds, observation)` pairs while searching; return each decision.
    fn searching<const N: usize>(steps: [(u64, Observation); N]) -> [Decision; N] {
        let mut policy = Policy::new(CONFIG);
        steps.map(|(t, obs)| policy.observe(obs, t * S, Context::Searching))
    }

    #[test]
    fn searching_confirms_at_0_10_20() {
        let d = searching([(0, EMPTY), (10, EMPTY), (20, EMPTY)]);
        assert_eq!(d, [Decision::Warn, Decision::Warn, SHUTDOWN]);
    }

    /// The defect found by running `record()`: 0/8/16 count
    /// but span only 16 s; the 20 s reading is too close to count, and must not
    /// confirm on the strength of `now`.
    #[test]
    fn an_uncounted_reading_cannot_confirm() {
        let d = searching([(0, EMPTY), (8, EMPTY), (16, EMPTY), (20, EMPTY)]);
        assert_eq!(d, [Decision::Warn; 4]);
    }

    #[test]
    fn the_next_counted_reading_after_a_short_span_confirms() {
        let d = searching([
            (0, EMPTY),
            (8, EMPTY),
            (16, EMPTY),
            (20, EMPTY),
            (24, EMPTY),
        ]);
        assert_eq!(d[4], SHUTDOWN);
    }

    /// Phase changes take readings of their own. A boot reading, a Phase 1
    /// reading and a detect-entry reading seconds apart are one reading.
    #[test]
    fn repeated_entry_readings_do_not_accelerate_the_cutoff() {
        let d = searching([(0, EMPTY), (1, EMPTY), (2, EMPTY), (3, EMPTY), (7, EMPTY)]);
        assert_eq!(d, [Decision::Warn; 5]);
    }

    /// An entry reading may be the third — if the earlier two are honestly old.
    #[test]
    fn an_entry_reading_can_be_the_third() {
        let mut policy = Policy::new(CONFIG);
        assert_eq!(policy.observe(EMPTY, 0, Context::Normal), Decision::Warn);
        assert_eq!(
            policy.observe(EMPTY, 60 * S, Context::Normal),
            Decision::Warn
        );
        assert_eq!(policy.observe(EMPTY, 90 * S, Context::Searching), SHUTDOWN);
    }

    /// The reverse direction: evidence gathered while searching is kept when a
    /// controller answers, and then has to cover the longer span.
    #[test]
    fn search_evidence_carries_into_normal_operation_under_the_longer_span() {
        let mut policy = Policy::new(CONFIG);
        assert_eq!(policy.observe(EMPTY, 0, Context::Searching), Decision::Warn);
        assert_eq!(
            policy.observe(EMPTY, 10 * S, Context::Searching),
            Decision::Warn
        );
        // Would have confirmed while searching (three readings over 20 s)…
        assert_eq!(
            policy.observe(EMPTY, 20 * S, Context::Normal),
            Decision::Warn
        );
        assert_eq!(policy.empty_progress(), Some((3, 20 * S)));
        // …and does once the same evidence spans the normal 120 s.
        assert_eq!(
            policy.observe(EMPTY, 80 * S, Context::Normal),
            Decision::Warn
        );
        assert_eq!(policy.observe(EMPTY, 120 * S, Context::Normal), SHUTDOWN);
    }

    /// A reading taken on entering a search, too soon after the last counted
    /// one, neither counts nor confirms — even though the span is already met.
    #[test]
    fn a_too_close_reading_on_a_context_change_cannot_confirm() {
        let mut policy = Policy::new(CONFIG);
        assert_eq!(policy.observe(EMPTY, 0, Context::Normal), Decision::Warn);
        assert_eq!(
            policy.observe(EMPTY, 60 * S, Context::Normal),
            Decision::Warn
        );
        assert_eq!(
            policy.observe(EMPTY, 63 * S, Context::Searching),
            Decision::Warn
        );
        assert_eq!(policy.empty_progress(), Some((2, 60 * S)));
        assert_eq!(policy.observe(EMPTY, 70 * S, Context::Searching), SHUTDOWN);
    }

    #[test]
    fn normal_operation_takes_its_own_longer_span() {
        let mut policy = Policy::new(CONFIG);
        assert_eq!(policy.observe(EMPTY, 0, Context::Normal), Decision::Warn);
        assert_eq!(
            policy.observe(EMPTY, 10 * S, Context::Normal),
            Decision::Warn
        );
        assert_eq!(
            policy.observe(EMPTY, 20 * S, Context::Normal),
            Decision::Warn
        );
        assert_eq!(
            policy.observe(EMPTY, 60 * S, Context::Normal),
            Decision::Warn
        );
        assert_eq!(policy.observe(EMPTY, 120 * S, Context::Normal), SHUTDOWN);
    }

    #[test]
    fn a_healthy_reading_clears_the_evidence() {
        let d = searching([
            (0, EMPTY),
            (10, EMPTY),
            (15, gauge(25)),
            (20, EMPTY),
            (30, EMPTY),
        ]);
        assert_eq!(d[2], Decision::Continue);
        assert_eq!(d[4], Decision::Warn);
    }

    #[test]
    fn a_charging_reading_clears_the_evidence() {
        let charging = Observation {
            charging: Some(true),
            ..EMPTY
        };
        let d = searching([
            (0, EMPTY),
            (10, EMPTY),
            (15, charging),
            (20, EMPTY),
            (30, EMPTY),
        ]);
        assert_eq!(d[2], Decision::Continue);
        assert_eq!(d[4], Decision::Warn);
    }

    #[test]
    fn failed_reads_neither_count_nor_clear() {
        let d = searching([
            (0, EMPTY),
            (10, UNKNOWN),
            (20, UNKNOWN),
            (30, EMPTY),
            (40, EMPTY),
        ]);
        assert_eq!(d[1], Decision::Continue);
        assert_eq!(d[4], SHUTDOWN);
    }

    /// Bounded gaps, not an unbroken run: 0/120/240 s confirms.
    #[test]
    fn sparse_empty_readings_within_the_gap_confirm() {
        let d = searching([(0, EMPTY), (120, EMPTY), (240, EMPTY)]);
        assert_eq!(d[2], SHUTDOWN);
    }

    #[test]
    fn the_gap_boundary_is_inclusive() {
        // Exactly max_gap_ms apart still belongs to the same evidence…
        let d = searching([(0, EMPTY), (150, EMPTY), (300, EMPTY)]);
        assert_eq!(d[2], SHUTDOWN);
        // …one millisecond more starts over.
        let mut policy = Policy::new(CONFIG);
        assert_eq!(policy.observe(EMPTY, 0, Context::Searching), Decision::Warn);
        assert_eq!(
            policy.observe(EMPTY, 150 * S, Context::Searching),
            Decision::Warn
        );
        assert_eq!(
            policy.observe(EMPTY, 300 * S + 1, Context::Searching),
            Decision::Warn
        );
        assert_eq!(policy.empty_progress(), Some((1, 0)));
    }

    /// Empty level, charge flag unreadable: a charging cell is exempt, so this
    /// cannot count — but it is worth showing.
    #[test]
    fn empty_with_unknown_charge_state_warns_without_counting() {
        let obs = Observation {
            charging: None,
            ..EMPTY
        };
        let mut policy = Policy::new(CONFIG);
        for t in [0, 10, 20, 30] {
            assert_eq!(
                policy.observe(obs, t * S, Context::Searching),
                Decision::Warn
            );
        }
        assert_eq!(policy.empty_progress(), None);
    }

    #[test]
    fn a_board_that_cannot_sleep_never_shuts_down() {
        let mut policy = Policy::new(Config {
            can_sleep: false,
            ..CONFIG
        });
        for t in [0, 10, 20, 30] {
            assert_eq!(
                policy.observe(EMPTY, t * S, Context::Searching),
                Decision::Continue
            );
        }
        assert_eq!(
            policy.observe(volts(3000), 40 * S, Context::Normal),
            Decision::Continue
        );
    }

    #[test]
    fn one_low_voltage_reading_in_normal_operation_shuts_down() {
        let mut policy = Policy::new(CONFIG);
        assert_eq!(
            policy.observe(volts(3700), 0, Context::Normal),
            Decision::Continue
        );
        assert_eq!(
            policy.observe(volts(3100), 60 * S, Context::Normal),
            Decision::Shutdown(ShutdownReason::Undervoltage { millivolts: 3100 })
        );
    }

    /// The XIAO has never been sampled while searching. A dip under the
    /// boost's start-up load must not sleep it.
    #[test]
    fn a_voltage_dip_while_searching_is_ignored() {
        let mut policy = Policy::new(CONFIG);
        for t in [0, 10, 20, 30] {
            assert_eq!(
                policy.observe(volts(3100), t * S, Context::Searching),
                Decision::Continue
            );
        }
    }

    #[test]
    fn the_voltage_cutoff_while_searching_is_a_config_choice() {
        let mut policy = Policy::new(Config {
            voltage_cutoff_while_searching: true,
            ..CONFIG
        });
        assert_eq!(
            policy.observe(volts(3100), 0, Context::Searching),
            Decision::Shutdown(ShutdownReason::Undervoltage { millivolts: 3100 })
        );
    }

    /// Low voltage, charge flag unreadable: a charging cell is exempt, so this
    /// must not shut down — but the facts that did read are kept for display.
    #[test]
    fn low_voltage_with_unknown_charge_state_continues_and_keeps_its_facts() {
        let obs = Observation {
            charging: None,
            ..volts(3000)
        };
        let mut policy = Policy::new(CONFIG);
        assert_eq!(policy.observe(obs, 0, Context::Normal), Decision::Continue);
        assert_eq!(
            policy.snapshot(0),
            Snapshot {
                percent: Some(50),
                charging: None,
                charge_complete: Some(false),
            }
        );
    }

    /// A partial observation — the gauge byte failed, the charge flag read
    /// "charging" — still clears the evidence. One fact is enough when it is
    /// the exempting one.
    #[test]
    fn a_partial_charging_observation_clears_the_evidence() {
        let partial = Observation {
            charging: Some(true),
            ..UNKNOWN
        };
        let d = searching([
            (0, EMPTY),
            (10, EMPTY),
            (15, partial),
            (20, EMPTY),
            (30, EMPTY),
        ]);
        assert_eq!(d[2], Decision::Continue);
        assert_eq!(d[4], Decision::Warn);
    }

    #[test]
    fn a_charging_cell_is_exempt_from_the_voltage_cutoff() {
        let obs = Observation {
            charging: Some(true),
            ..volts(3000)
        };
        let mut policy = Policy::new(CONFIG);
        assert_eq!(policy.observe(obs, 0, Context::Normal), Decision::Continue);
    }

    #[test]
    fn facts_are_retained_independently() {
        let mut policy = Policy::new(CONFIG);
        policy.observe(gauge(25), 0, Context::Normal);
        // The gauge byte will not decode, but the charge flag read fine.
        let partial = Observation {
            charging: Some(true),
            ..UNKNOWN
        };
        policy.observe(partial, 60 * S, Context::Normal);
        assert_eq!(
            policy.snapshot(60 * S),
            Snapshot {
                percent: Some(25),
                charging: Some(true),
                charge_complete: Some(false),
            }
        );
    }

    #[test]
    fn a_stale_fact_is_not_shown_as_current() {
        let mut policy = Policy::new(CONFIG);
        policy.observe(gauge(75), 0, Context::Normal);
        assert_eq!(policy.snapshot(150 * S).percent, Some(75));
        assert_eq!(policy.snapshot(150 * S + 1).percent, None);
    }

    /// A rested-full cell decodes to 75 on the IP5306; the complete bit is what
    /// says it finished. Shown as 100 on the cable only.
    #[test]
    fn charge_complete_on_the_cable_is_shown_as_100() {
        let complete = Observation {
            charge_complete: Some(true),
            ..gauge(75)
        };
        let mut policy = Policy::new(CONFIG);
        policy.observe(complete, 0, Context::Normal);
        let snap = policy.snapshot(0);
        assert_eq!(snap.percent, Some(75));
        assert_eq!(snap.shown_percent(true), Some(100));
        assert_eq!(snap.shown_percent(false), Some(75));
    }

    /// The complete bit is a reading in its own right: it stands even when the
    /// gauge byte did not decode — on the cable. Off it, nothing is shown.
    #[test]
    fn charge_complete_is_shown_without_a_decoded_level() {
        let complete = Observation {
            charge_complete: Some(true),
            ..UNKNOWN
        };
        let mut policy = Policy::new(CONFIG);
        policy.observe(complete, 0, Context::Normal);
        let snap = policy.snapshot(0);
        assert_eq!(snap.shown_percent(true), Some(100));
        assert_eq!(snap.shown_percent(false), None);
    }

    /// An unknown or false complete bit changes nothing about what is shown.
    #[test]
    fn an_unknown_complete_bit_shows_the_measured_level() {
        let mut policy = Policy::new(CONFIG);
        policy.observe(gauge(75), 0, Context::Normal);
        assert_eq!(policy.snapshot(0).shown_percent(true), Some(75));
        policy.observe(
            Observation {
                charge_complete: None,
                ..gauge(50)
            },
            60 * S,
            Context::Normal,
        );
        // The earlier `Some(false)` is still current; the level moved.
        assert_eq!(policy.snapshot(60 * S).shown_percent(true), Some(50));
    }

    /// The flaw in the first cut of this fix: folded into the
    /// measured level, a complete bit that stayed set on a draining, plugged-in
    /// unit reset the empty-reading counter. Here it is not a measurement and
    /// cannot touch the cutoff.
    #[test]
    fn charge_complete_does_not_exempt_an_empty_cell() {
        let complete_but_empty = Observation {
            charge_complete: Some(true),
            ..EMPTY
        };
        let d = searching([
            (0, complete_but_empty),
            (10, complete_but_empty),
            (20, complete_but_empty),
        ]);
        assert_eq!(d, [Decision::Warn, Decision::Warn, SHUTDOWN]);
    }

    #[test]
    fn charge_complete_is_retained_and_expires_like_any_fact() {
        let mut policy = Policy::new(CONFIG);
        policy.observe(
            Observation {
                charge_complete: Some(true),
                ..gauge(75)
            },
            0,
            Context::Normal,
        );
        policy.observe(UNKNOWN, 60 * S, Context::Normal);
        assert_eq!(policy.snapshot(60 * S).charge_complete, Some(true));
        assert_eq!(policy.snapshot(150 * S).shown_percent(true), Some(100));
        assert_eq!(policy.snapshot(150 * S + 1).charge_complete, None);
        assert_eq!(policy.snapshot(150 * S + 1).shown_percent(true), None);
    }

    const fn observed(charging: Option<bool>, percent: Option<u8>) -> Reading {
        Reading::Observed(Observation {
            charging,
            charge_complete: None,
            percent,
            millivolts: None,
        })
    }

    #[test]
    fn dfu_on_the_cable_is_always_allowed() {
        assert_eq!(
            dfu_verdict(observed(None, None), true, 50),
            DfuVerdict::Allowed
        );
        assert_eq!(
            dfu_verdict(observed(Some(false), Some(0)), true, 50),
            DfuVerdict::Allowed
        );
    }

    #[test]
    fn dfu_without_a_gauge_is_allowed() {
        assert_eq!(
            dfu_verdict(Reading::NoGauge, false, 50),
            DfuVerdict::Allowed
        );
    }

    /// The failure this guards: the full-status read fails, and with it went a
    /// level that said "empty" — which then read as permission.
    #[test]
    fn dfu_is_refused_on_a_low_level_even_when_other_reads_failed() {
        assert_eq!(
            dfu_verdict(observed(None, Some(0)), false, 50),
            DfuVerdict::RefusedLow { percent: 0 }
        );
    }

    #[test]
    fn dfu_is_refused_when_the_level_is_unknown() {
        assert_eq!(
            dfu_verdict(observed(Some(false), None), false, 50),
            DfuVerdict::RefusedUnknown
        );
        assert_eq!(
            dfu_verdict(observed(None, None), false, 50),
            DfuVerdict::RefusedUnknown
        );
    }

    #[test]
    fn dfu_is_allowed_when_charging_or_adequate() {
        assert_eq!(
            dfu_verdict(observed(Some(true), None), false, 50),
            DfuVerdict::Allowed
        );
        assert_eq!(
            dfu_verdict(observed(Some(false), Some(50)), false, 50),
            DfuVerdict::Allowed
        );
        assert_eq!(
            dfu_verdict(observed(None, Some(75)), false, 50),
            DfuVerdict::Allowed
        );
    }
}
