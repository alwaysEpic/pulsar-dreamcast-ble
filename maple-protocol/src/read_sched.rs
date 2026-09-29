// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Block-read scheduling with LCD deferral (the production candidate).
//!
//! A scheduled block read that falls due too soon after an LCD frame is left
//! **pending** instead of run; the window does a normal controller poll; LCD
//! writes are suppressed while it is pending; and it is issued at the first
//! later window whose gap is large enough and whose capture pipeline is free.
//!
//! Pure logic, time as a parameter — the firmware feeds it RTC1 ticks and the
//! window's facts, and performs the side effects. It depends on nothing in the
//! diagnostic it was written under and on no diagnostic type, so the lifecycle
//! below is unit-tested off-target. `maple::block_read` is the firmware side of
//! every call below.
//!
//! # The two commitments that are easy to get wrong
//!
//! **Selecting a read window is tentative; finding a block and committing it to
//! the bus is what consumes the slot.** [`Slot::Read`] is an *answer*, not an
//! event: the dispatcher can still return without issuing a command, and a
//! cadence slot consumed by a read that never went out is a read silently
//! dropped. So the cadence counter and the pending state both move in
//! [`ReadSched::note_read_issued`] and nowhere else, and the abandoned path
//! ([`ReadSched::note_read_abandoned`]) leaves both exactly as they were. An
//! abandoned dispatch may well have passed the gap check already — its problem
//! is that no command was issued, not an unsatisfied gap, so it is **not** a
//! gap deferral and is not counted as one.
//!
//! **Permanent absence of read work cancels; temporary exhaustion does not.**
//! A pipeline that is merely busy — a capture latched, a decode in flight, a
//! block not yet reachable — stays pending: the read is still owed and will be
//! served. Only [`ReadWork::Exhausted`], meaning no block will ever become due
//! again, cancels. Getting this backwards either latches the display off
//! forever (treating permanent as temporary) or drops owed reads on every busy
//! window (the reverse), which is why the caller must *name* which of the two
//! it is testing rather than let this module infer it from an empty pipeline.
//!
//! # Why the suppression is not optional
//!
//! [`ReadSched::frame_gate`] holds the display while a read is pending. Without
//! it a dirty frame in every window re-arms the gap and the read never becomes
//! eligible — the display starves the read. A held frame stays dirty and is
//! written at the first window whose gate opens, so nothing is lost; it is late.

/// The gap a scheduled read must have from the last LCD frame's **return**
/// before it may run: 492 RTC1 ticks at 32,768 Hz = 15.015 ms.
///
/// **An operating choice, not a measured minimum and not a bound.** 492 is the
/// eligibility edge the bench actually ran: it selected v294 run #192's
/// delayed-context population — 8,729 ordinary delayed first attempts plus 205
/// retries, 0 unsuccessful — against ~1.86 ms, where the same path failed
/// 200 of 200. Carrying the number forward is engineering continuity with the
/// tested configuration. No sharp boundary was measured, and nothing here
/// claims one exists.
///
/// Production owns this constant rather than importing the diagnostic's
/// `OCTX_GAP_TICKS`, which an experiment is free to re-tune.
pub const READ_LCD_GAP_TICKS: u32 = 492;

/// The gap reported when no frame has been written yet. Larger than any real
/// threshold, so a VMU that has never been drawn to is never gap-deferred.
pub const GAP_NONE: u32 = u32::MAX;

/// Whether any read work remains to be served.
///
/// The caller names which of the two it is testing; see the module note on
/// permanent versus temporary exhaustion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadWork {
    /// Work remains — a block is due now, or the pipeline is merely busy and
    /// will drain. A pending read stays pending.
    Remaining,
    /// No block will ever become due again: the pull is complete and does not
    /// restart. A pending read is cancelled, so the display is released.
    Exhausted,
}

/// What this window is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    /// The ordinary controller poll. A blocked read takes this too — the
    /// window is not wasted.
    Poll,
    /// A block read *may* run. Tentative until the dispatcher commits; see the
    /// module note.
    Read,
}

/// One window's facts, taken at its head before any transmission.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    /// Monotonic RTC1 ticks. The only clock this module has.
    pub now: u64,
    /// The cadence: a read falls due every `n` windows. 0 disables reads.
    pub n: u8,
    /// Is a VMU present to read from and draw to?
    pub vmu_present: bool,
    /// Ticks since the last LCD frame's return, or [`GAP_NONE`].
    pub gap_ticks: u32,
    /// Is the capture pipeline still holding an unapplied capture?
    pub pipeline_busy: bool,
    /// See [`ReadWork`] — the caller's explicit call, not an inference.
    pub work: ReadWork,
}

/// An unfinished stall's age, in windows and in RTC1 ticks.
///
/// Both halves are reported because neither alone is enough: a stalled loop can
/// show few windows and a long delay, and a brisk one many windows and a short
/// one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Age {
    pub windows: u32,
    pub ticks: u64,
}

/// A stall in progress: when and at which window it began.
#[derive(Clone, Copy, Debug)]
struct Stall {
    ticks: u64,
    windows: u32,
}

/// Completed stalls of **one outcome**, accumulated.
///
/// The count travels with the durations rather than sitting apart from them,
/// because a sum is only meaningful over the population it was taken from: a
/// mean is `ticks_sum / n` of the *same* span, and no consumer has to know
/// which count goes with which sum to get it right.
#[derive(Clone, Copy, Default, Debug)]
pub struct Span {
    /// Stalls recorded here.
    pub n: u32,
    pub windows_max: u32,
    pub windows_sum: u64,
    pub ticks_max: u64,
    pub ticks_sum: u64,
}

impl Span {
    pub const ZERO: Self = Self {
        n: 0,
        windows_max: 0,
        windows_sum: 0,
        ticks_max: 0,
        ticks_sum: 0,
    };

    fn record(&mut self, age: Age) {
        self.n = self.n.saturating_add(1);
        self.windows_max = self.windows_max.max(age.windows);
        self.windows_sum = self.windows_sum.saturating_add(u64::from(age.windows));
        self.ticks_max = self.ticks_max.max(age.ticks);
        self.ticks_sum = self.ticks_sum.saturating_add(age.ticks);
    }
}

/// Everything the bench gates read.
///
/// The two deferral causes never merge and the two stall outcomes are never
/// summed: a cancelled stall and a served stall mean different things, and a
/// read blocked by the gap rule is the thing under test while one blocked by a
/// busy pipeline is not.
#[derive(Clone, Copy, Default, Debug)]
pub struct Metrics {
    /// Read stalls begun. With `defer_served.n + defer_cancelled.n` (+ 1 for a
    /// stall still outstanding) the accounting closes.
    pub defer_starts: u32,
    /// Read stalls that ended in an actual issue — **the only span a
    /// served-stall mean may be taken from**.
    pub defer_served: Span,
    /// Read stalls ended by an explicit cancellation, durations and all.
    ///
    /// Held apart from [`Self::defer_served`] rather than merely counted
    /// apart: a cancelled stall was ended by a disconnect, a mode change or a
    /// VMU going away, so its length says nothing about how long the gap rule
    /// makes a read wait. Summed together, one 100-tick cancellation buries a
    /// 10-tick served stall and the mean describes neither population.
    pub defer_cancelled: Span,
    /// Blocked *windows* charged to the gap rule (not stalls).
    pub gap_deferred: u32,
    /// Blocked windows charged to a busy capture pipeline.
    pub pipe_deferred: u32,
    /// Dispatches that found no block and sent nothing. Not a gap deferral.
    pub abandoned: u32,
    /// LCD write opportunities suppressed.
    pub frames_held: u32,
    /// Frame holds begun. Closes against the two spans below the same way.
    pub hold_starts: u32,
    /// Frame holds that ended with the frame reaching the wire — the only
    /// span a display-staleness figure may be taken from.
    pub hold_written: Span,
    /// Frame holds ended by cancellation, durations kept separate for the same
    /// reason as [`Self::defer_cancelled`].
    pub hold_cancelled: Span,
}

impl Metrics {
    pub const ZERO: Self = Self {
        defer_starts: 0,
        defer_served: Span::ZERO,
        defer_cancelled: Span::ZERO,
        gap_deferred: 0,
        pipe_deferred: 0,
        abandoned: 0,
        frames_held: 0,
        hold_starts: 0,
        hold_written: Span::ZERO,
        hold_cancelled: Span::ZERO,
    };
}

/// The every-Nth read schedule, its LCD-gap deferral and the display hold.
#[derive(Clone, Copy, Debug)]
pub struct ReadSched {
    /// Windows since the last **issued** read — the cadence counter.
    windows: u32,
    /// Windows since construction, monotonic. Stall ages are differences of
    /// this, so a cadence reset cannot shorten a stall.
    elapsed: u32,
    /// The outstanding read stall, if any. `Some` *is* "read pending".
    defer: Option<Stall>,
    /// The outstanding frame hold, if any.
    hold: Option<Stall>,
    m: Metrics,
}

impl Default for ReadSched {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadSched {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            windows: 0,
            elapsed: 0,
            defer: None,
            hold: None,
            m: Metrics::ZERO,
        }
    }

    #[must_use]
    pub const fn metrics(&self) -> &Metrics {
        &self.m
    }

    /// Is a read owed but not yet issued? This, and only this, holds the
    /// display.
    #[must_use]
    pub const fn read_pending(&self) -> bool {
        self.defer.is_some()
    }

    /// The outstanding read stall's age, or `None` if none is running.
    ///
    /// A snapshot that reports a healthy maximum while the longest wait is
    /// still running reads as a pass and is worse than no number, so this is
    /// reported beside the completed statistics and never folded into them.
    #[must_use]
    pub fn defer_age(&self, now: u64) -> Option<Age> {
        self.defer.map(|s| self.age(s, now))
    }

    /// The outstanding frame hold's age, or `None`.
    #[must_use]
    pub fn hold_age(&self, now: u64) -> Option<Age> {
        self.hold.map(|s| self.age(s, now))
    }

    const fn age(&self, s: Stall, now: u64) -> Age {
        Age {
            windows: self.elapsed.saturating_sub(s.windows),
            ticks: now.saturating_sub(s.ticks),
        }
    }

    /// Decide what this window is for. Called once at every window head, before
    /// any transmission.
    ///
    /// The order of the arms is load-bearing. Cancellation runs first, so a
    /// read that can never be served releases the display in the same window;
    /// the gap arm sits **after** the due test, so an undue window is not
    /// charged to the gap rule, and **before** the pipeline test, so the cause
    /// recorded is the one under test.
    pub fn window(&mut self, ctx: &Ctx) -> Slot {
        self.elapsed = self.elapsed.saturating_add(1);
        self.windows = self.windows.saturating_add(1);

        // No read can ever become due from here, so nothing is owed and the
        // display must not stay suppressed. Missing any one of these three is
        // what leaves LCD writes held indefinitely after a mode change.
        if ctx.n == 0 || !ctx.vmu_present || ctx.work == ReadWork::Exhausted {
            self.cancel(ctx.now);
            return Slot::Poll;
        }
        if self.windows < u32::from(ctx.n) {
            return Slot::Poll;
        }
        // v294 run #192: the shared read path was clean at >= 15 ms after a
        // frame (8,729 delayed first attempts + 205 retries, 0 unsuccessful)
        // and failed at ~1.86 ms (200 of 200 no-trigger). An operating choice
        // matching the tested configuration, not a measured minimum.
        if ctx.gap_ticks < READ_LCD_GAP_TICKS {
            self.m.gap_deferred = self.m.gap_deferred.saturating_add(1);
            self.defer_start(ctx.now);
            return Slot::Poll;
        }
        // Temporary: the read is still owed, so any pending stall continues and
        // no new one begins — the pipeline is not the display's fault.
        if ctx.pipeline_busy {
            self.m.pipe_deferred = self.m.pipe_deferred.saturating_add(1);
            return Slot::Poll;
        }
        Slot::Read
    }

    /// Begin a stall, or continue the one already running.
    ///
    /// Idempotent by design: a second and third blocked window are the *same*
    /// stall continuing, and restarting the age there would report every long
    /// starvation as a one-window wait.
    const fn defer_start(&mut self, now: u64) {
        if self.defer.is_none() {
            self.m.defer_starts = self.m.defer_starts.saturating_add(1);
            self.defer = Some(Stall {
                ticks: now,
                windows: self.elapsed,
            });
        }
    }

    /// The dispatcher has its block and is committing the command to the bus.
    ///
    /// This is what consumes the cadence slot and ends the stall — not the
    /// return of [`Slot::Read`]. Calling it twice without an intervening due
    /// window is harmless: the cadence is already 0 and the stall already
    /// ended, so neither is double-counted.
    pub fn note_read_issued(&mut self, now: u64) {
        self.windows = 0;
        if let Some(s) = self.defer.take() {
            self.m.defer_served.record(self.age(s, now));
        }
    }

    /// The dispatcher found no block and sent nothing.
    ///
    /// Cadence and pending state are left exactly as they were, so the slot is
    /// due again at the very next window. Deliberately not a gap deferral: this
    /// dispatch may well have passed the gap check, and its problem is that no
    /// command was issued.
    pub const fn note_read_abandoned(&mut self) {
        self.m.abandoned = self.m.abandoned.saturating_add(1);
    }

    /// May this window's already-dirty LCD frame be written?
    ///
    /// `false` holds it. A read owed but blocked by the gap takes priority over
    /// the display: otherwise a dirty frame every window re-arms the gap and
    /// the read never becomes eligible. The frame stays dirty and goes out at
    /// the first window whose gate opens.
    ///
    /// Called only when a frame is actually waiting, so every `false` here is a
    /// real suppressed write opportunity.
    pub const fn frame_gate(&mut self, now: u64) -> bool {
        if self.defer.is_none() {
            return true;
        }
        self.m.frames_held = self.m.frames_held.saturating_add(1);
        // The hold's age runs from the *first* suppression. A later framebuffer
        // update must not restart it: staleness is measured from when the
        // display first could not be drawn, not from the newest content it
        // wanted to draw.
        if self.hold.is_none() {
            self.m.hold_starts = self.m.hold_starts.saturating_add(1);
            self.hold = Some(Stall {
                ticks: now,
                windows: self.elapsed,
            });
        }
        false
    }

    /// A frame reached the wire. Ends any hold; harmless when none is running.
    pub fn note_lcd_written(&mut self, now: u64) {
        if let Some(s) = self.hold.take() {
            self.m.hold_written.record(self.age(s, now));
        }
    }

    /// The controller went away. Cancels any owed read: it was owed to a
    /// controller that is gone, not to its replacement.
    pub fn note_disconnect(&mut self, now: u64) {
        self.cancel(now);
    }

    /// The mode changed — cadence, and everything owed under the old one, go
    /// with it.
    pub fn note_mode_reset(&mut self, now: u64) {
        self.windows = 0;
        self.cancel(now);
    }

    /// Zero the counters without touching the lifecycle, for a caller whose
    /// measurements are scoped to something shorter than the schedule's life —
    /// the diagnostic harness restarts them at every mode change.
    pub const fn reset_metrics(&mut self) {
        self.m = Metrics::ZERO;
    }

    /// End an outstanding stall and hold without serving either, and re-open
    /// the gate in this same window.
    ///
    /// Cancellations are counted apart from issues throughout: a stall that was
    /// abandoned says nothing about how long the rule makes a read wait.
    fn cancel(&mut self, now: u64) {
        if let Some(s) = self.defer.take() {
            self.m.defer_cancelled.record(self.age(s, now));
        }
        if let Some(s) = self.hold.take() {
            self.m.hold_cancelled.record(self.age(s, now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window with a gap well past the threshold, a free pipeline, work to
    /// do and the default `N = 2` cadence. Tests vary one field at a time.
    fn ctx(now: u64) -> Ctx {
        Ctx {
            now,
            n: 2,
            vmu_present: true,
            gap_ticks: READ_LCD_GAP_TICKS,
            pipeline_busy: false,
            work: ReadWork::Remaining,
        }
    }

    /// Run `n` identical windows at `c.now`, returning the last slot. The clock
    /// deliberately does not advance, so a stall opened here starts at `c.now`
    /// and every age below is a plain subtraction from it.
    fn run(s: &mut ReadSched, c: &Ctx, n: u32) -> Slot {
        let mut slot = Slot::Poll;
        for _ in 0..n {
            slot = s.window(c);
        }
        slot
    }

    #[test]
    fn cadence_issues_every_nth_window() {
        let mut s = ReadSched::new();
        assert_eq!(s.window(&ctx(0)), Slot::Poll);
        assert_eq!(s.window(&ctx(1)), Slot::Read);
        s.note_read_issued(1);
        assert_eq!(s.window(&ctx(2)), Slot::Poll);
        assert_eq!(s.window(&ctx(3)), Slot::Read);
    }

    // ---- §2a, the boundary -------------------------------------------------

    #[test]
    fn defers_one_tick_under_the_threshold() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = READ_LCD_GAP_TICKS - 1;
        assert_eq!(run(&mut s, &c, 2), Slot::Poll);
        assert!(s.read_pending());
        assert_eq!(s.metrics().gap_deferred, 1);
    }

    #[test]
    fn does_not_defer_at_the_threshold() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = READ_LCD_GAP_TICKS;
        assert_eq!(run(&mut s, &c, 2), Slot::Read);
        assert!(!s.read_pending());
        assert_eq!(s.metrics().gap_deferred, 0);
    }

    #[test]
    fn gap_none_never_defers() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = GAP_NONE;
        assert_eq!(run(&mut s, &c, 2), Slot::Read);
        assert!(!s.read_pending());
    }

    // ---- §0a.6, what may set pending ---------------------------------------

    #[test]
    fn an_undue_window_never_sets_pending() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = 0;
        // Window 1 of a 2-window cadence: blocked by the gap, but not due.
        assert_eq!(s.window(&c), Slot::Poll);
        assert!(!s.read_pending());
        assert_eq!(s.metrics().gap_deferred, 0);
        assert_eq!(s.metrics().defer_starts, 0);
    }

    #[test]
    fn a_busy_pipeline_never_sets_pending() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.pipeline_busy = true;
        assert_eq!(run(&mut s, &c, 2), Slot::Poll);
        assert!(!s.read_pending());
        assert_eq!(s.metrics().pipe_deferred, 1);
        assert_eq!(s.metrics().gap_deferred, 0);
    }

    #[test]
    fn a_busy_pipeline_leaves_an_existing_stall_pending() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = 0;
        run(&mut s, &c, 2);
        assert!(s.read_pending());
        // The gap opens but the pipeline is busy: still owed, still pending.
        c.gap_ticks = GAP_NONE;
        c.pipeline_busy = true;
        c.now = 10;
        assert_eq!(s.window(&c), Slot::Poll);
        assert!(s.read_pending());
        assert_eq!(s.metrics().defer_starts, 1);
    }

    #[test]
    fn pending_start_is_idempotent() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = 0;
        run(&mut s, &c, 2);
        c.now = 100;
        s.window(&c);
        c.now = 200;
        s.window(&c);
        assert_eq!(s.metrics().defer_starts, 1);
        assert_eq!(s.metrics().gap_deferred, 3);
        // The age is measured from the first blocked window, not the last.
        assert_eq!(
            s.defer_age(200),
            Some(Age {
                windows: 2,
                ticks: 200
            })
        );
    }

    // ---- §2c, the commit ---------------------------------------------------

    #[test]
    fn an_abandoned_dispatch_preserves_cadence_and_pending() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = 0;
        run(&mut s, &c, 2);
        assert!(s.read_pending());

        // The gap opens, the slot is selected, and the dispatcher then finds no
        // block. It had already passed the gap check, so this is not a gap
        // deferral — and the slot must still be due next window.
        c.gap_ticks = GAP_NONE;
        c.now = 10;
        assert_eq!(s.window(&c), Slot::Read);
        s.note_read_abandoned();
        assert!(s.read_pending());
        assert_eq!(s.metrics().abandoned, 1);
        // Only the due window was charged to the gap; the selected-then-
        // abandoned one had already passed the check.
        assert_eq!(s.metrics().gap_deferred, 1);
        assert_eq!(s.metrics().defer_served.n, 0);

        c.now = 11;
        assert_eq!(s.window(&c), Slot::Read);
    }

    #[test]
    fn an_actual_issue_clears_both_exactly_once() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = 0;
        run(&mut s, &c, 2);
        c.gap_ticks = GAP_NONE;
        c.now = 50;
        assert_eq!(s.window(&c), Slot::Read);
        s.note_read_issued(50);
        assert!(!s.read_pending());
        assert_eq!(s.metrics().defer_served.n, 1);
        assert_eq!(s.metrics().defer_served.ticks_max, 50);
        assert_eq!(s.metrics().defer_served.windows_max, 1);

        // A second commit with no window between must not double-count, and
        // must not restart the cadence a second time.
        s.note_read_issued(60);
        assert_eq!(s.metrics().defer_served.n, 1);
        assert_eq!(s.metrics().defer_served.ticks_max, 50);
        c.now = 51;
        assert_eq!(s.window(&c), Slot::Poll);
    }

    // ---- §2c, the clear table ----------------------------------------------

    /// Drive a stall, then apply `f`, and assert it cancelled rather than
    /// served — and that the display is released in the same window.
    fn cancels_by(f: impl FnOnce(&mut ReadSched, &mut Ctx)) {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = 0;
        run(&mut s, &c, 2);
        assert!(!s.frame_gate(2), "a pending read holds the frame");
        assert!(s.read_pending());

        c.now = 10;
        f(&mut s, &mut c);
        assert!(!s.read_pending());
        assert_eq!(s.metrics().defer_cancelled.n, 1);
        assert_eq!(s.metrics().defer_served.n, 0);
        assert_eq!(s.metrics().hold_cancelled.n, 1);
        assert!(s.frame_gate(11), "the gate re-opens with the cancellation");
    }

    #[test]
    fn disconnect_cancels() {
        cancels_by(|s, c| s.note_disconnect(c.now));
    }

    #[test]
    fn mode_reset_cancels() {
        cancels_by(|s, c| s.note_mode_reset(c.now));
    }

    #[test]
    fn read_disable_cancels() {
        cancels_by(|s, c| {
            c.n = 0;
            assert_eq!(s.window(c), Slot::Poll);
        });
    }

    #[test]
    fn vmu_removal_cancels() {
        cancels_by(|s, c| {
            c.vmu_present = false;
            assert_eq!(s.window(c), Slot::Poll);
        });
    }

    #[test]
    fn permanent_exhaustion_cancels() {
        cancels_by(|s, c| {
            c.work = ReadWork::Exhausted;
            assert_eq!(s.window(c), Slot::Poll);
        });
    }

    #[test]
    fn read_pending_implies_a_read_can_still_become_due() {
        // The invariant, driven over every state that ends the schedule.
        for end in [
            &mut (|c: &mut Ctx| c.n = 0) as &mut dyn FnMut(&mut Ctx),
            &mut |c: &mut Ctx| c.vmu_present = false,
            &mut |c: &mut Ctx| c.work = ReadWork::Exhausted,
        ] {
            let mut s = ReadSched::new();
            let mut c = ctx(0);
            c.gap_ticks = 0;
            run(&mut s, &c, 4);
            assert!(s.read_pending());
            end(&mut c);
            c.now = 20;
            s.window(&c);
            assert!(
                !s.read_pending(),
                "pending outlived the possibility of a read"
            );
        }
    }

    // ---- §3, the frame hold ------------------------------------------------

    #[test]
    fn frame_hold_age_does_not_restart_on_a_later_update() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = 0;
        run(&mut s, &c, 2);

        assert!(!s.frame_gate(10));
        // A new framebuffer update arrives and is suppressed in turn. The
        // display has been stale since tick 10, not since tick 30.
        c.now = 20;
        s.window(&c);
        assert!(!s.frame_gate(30));
        assert_eq!(s.metrics().hold_starts, 1);
        assert_eq!(s.metrics().frames_held, 2);
        assert_eq!(
            s.hold_age(30),
            Some(Age {
                windows: 1,
                ticks: 20
            })
        );
    }

    #[test]
    fn the_hold_outlives_the_issue_and_ends_at_the_write() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        c.gap_ticks = 0;
        run(&mut s, &c, 2);
        assert!(!s.frame_gate(5));

        // The read goes out in the next window; the frame it held is written in
        // the window after that, which is when the display stops being stale.
        c.gap_ticks = GAP_NONE;
        c.now = 10;
        assert_eq!(s.window(&c), Slot::Read);
        s.note_read_issued(10);
        assert!(s.hold_age(10).is_some(), "the frame is still unwritten");

        c.now = 20;
        s.window(&c);
        assert!(s.frame_gate(20));
        s.note_lcd_written(20);
        assert!(s.hold_age(20).is_none());
        assert_eq!(s.metrics().hold_written.n, 1);
        assert_eq!(s.metrics().hold_cancelled.n, 0);
        assert_eq!(s.metrics().hold_written.ticks_max, 15);
    }

    #[test]
    fn no_hold_without_a_pending_read() {
        let mut s = ReadSched::new();
        assert!(s.frame_gate(0));
        s.note_lcd_written(0);
        assert_eq!(s.metrics().frames_held, 0);
        assert_eq!(s.metrics().hold_starts, 0);
        assert_eq!(s.metrics().hold_written.n, 0);
    }

    // ---- served and cancelled durations never mix --------------------------

    /// A short served stall followed by a long cancelled one. Sharing a span
    /// would leave one issued count against 110 ticks, from which no served
    /// mean can be recovered — the cancellation is not a wait the gap rule
    /// imposed, and it is ten times the size of the one that was.
    #[test]
    fn served_and_cancelled_durations_are_kept_apart() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);

        // A 10-tick stall, served.
        c.gap_ticks = 0;
        run(&mut s, &c, 2);
        assert!(!s.frame_gate(2));
        c.gap_ticks = GAP_NONE;
        c.now = 10;
        assert_eq!(s.window(&c), Slot::Read);
        s.note_read_issued(10);
        c.now = 12;
        s.window(&c);
        assert!(s.frame_gate(12));
        s.note_lcd_written(12);

        // A 100-tick stall spanning four windows, cancelled by a disconnect.
        c.gap_ticks = 0;
        c.now = 20;
        run(&mut s, &c, 4);
        assert!(s.read_pending());
        assert!(!s.frame_gate(22));
        s.note_disconnect(120);

        let m = s.metrics();
        assert_eq!(m.defer_served.n, 1);
        assert_eq!(
            m.defer_served.ticks_sum, 10,
            "the served mean is 10, not 55"
        );
        assert_eq!(
            m.defer_served.ticks_max, 10,
            "the cancellation is not a wait"
        );
        assert_eq!(m.defer_cancelled.n, 1);
        assert_eq!(m.defer_cancelled.ticks_sum, 100);
        assert_eq!(m.defer_cancelled.ticks_max, 100);

        // The display's staleness splits the same way: 10 ticks written, 98
        // abandoned when the controller went.
        assert_eq!(m.hold_written.n, 1);
        assert_eq!(m.hold_written.ticks_sum, 10);
        assert_eq!(m.hold_written.ticks_max, 10);
        assert_eq!(m.hold_cancelled.n, 1);
        assert_eq!(m.hold_cancelled.ticks_sum, 98);
        assert_eq!(m.hold_cancelled.ticks_max, 98);

        // And the window halves stay split too, not only the tick halves.
        assert_eq!(m.defer_served.windows_max, 1);
        assert_eq!(m.defer_cancelled.windows_max, 3);
    }

    // ---- §4, the accounting closes -----------------------------------------

    #[test]
    fn stall_accounting_closes() {
        let mut s = ReadSched::new();
        let mut c = ctx(0);
        let mut now = 0u64;

        // A mixed run: some windows blocked by the gap, some clear, a
        // disconnect part way, and a frame waiting throughout.
        for i in 0..200u64 {
            now = i * 7;
            c.now = now;
            c.gap_ticks = if i % 5 < 3 { 0 } else { GAP_NONE };
            c.pipeline_busy = i % 11 == 0;
            match s.window(&c) {
                Slot::Read => {
                    if i % 17 == 0 {
                        s.note_read_abandoned();
                    } else {
                        s.note_read_issued(now);
                    }
                }
                Slot::Poll => {
                    if s.frame_gate(now) {
                        s.note_lcd_written(now);
                    }
                }
            }
            if i == 120 {
                s.note_disconnect(now);
            }
        }

        let m = s.metrics();
        let outstanding = u32::from(s.read_pending());
        assert_eq!(
            m.defer_starts,
            m.defer_served.n + m.defer_cancelled.n + outstanding
        );
        let held = u32::from(s.hold_age(now).is_some());
        assert_eq!(m.hold_starts, m.hold_written.n + m.hold_cancelled.n + held);
        // Both causes were exercised and neither absorbed the other.
        assert!(m.gap_deferred > 0);
        assert!(m.pipe_deferred > 0);
        assert!(m.abandoned > 0);
        assert!(m.defer_served.n > 0);
        assert!(m.defer_cancelled.n > 0);
    }
}
