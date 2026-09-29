// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Who owns each slot of the block-read pipeline, and what may happen next.
//!
//! The firmware's `maple::block_read` owns the bus, the capture buffers and
//! the decoder; this owns the bookkeeping around them — the queue of requested
//! blocks, which request each of the three slots holds, the retry rule and the
//! counters. Time is not a parameter here because nothing in it is timed; the
//! *when* is [`crate::read_sched`]'s.
//!
//! It is split out for the reason that module is: the firmware crate is
//! `no_std` on-target and its tests do not run, so a state machine left in it
//! is a state machine nothing exercises. This one is exercised below.
//!
//! # The three slots
//!
//! ```text
//! queue ──issue──▶ latched ──apply──▶ decoding ──good──▶ ready ──take──▶ gone
//!   ▲                                     │
//!   └──────────────── retry ──────────────┘ (fail)
//! ```
//!
//! **`latched`** is a capture the hardware has finished with and the decoder
//! has not started on. **`decoding`** is the request under decode.
//! **`ready`** is a decoded block the consumer has not taken.
//!
//! The pipeline is two deep on purpose: a decode spans several windows, and a
//! read window may capture the next block while the previous one decodes.
//! [`Self::pipeline_busy`] is what stops it going three deep — the scheduler
//! refuses a read window while a latch is unapplied.
//!
//! # The liveness rule, which is easy to get wrong
//!
//! [`Self::take_applicable`] is the **only** way a latch becomes a decode, and
//! it refuses while `decoding` or `ready` is occupied. Both of those are
//! released by things the *caller* does — a decode ending, a consumer taking
//! its block — so a caller that only asks after those events can strand a
//! latch forever.
//!
//! That is not hypothetical: the first version of the firmware side called its
//! apply step from the read window and from the end of a decode, and `ready`
//! being occupied at the moment a decode ended left the next capture latched
//! with no path back — `decoding` false stopped the slack loop, `latched` true
//! stopped the scheduler, and reads stopped for good (2026-09-18).
//! `pipeline_stranded_latch_resumes_when_ready_is_taken` below is that case.
//!
//! **So the caller must ask unconditionally, once per window**, not only when
//! it believes something has changed. Every other call site is latency.

use heapless::Deque;

use crate::read_sched::ReadWork;

/// Outstanding block requests.
///
/// Enough for the fingerprint pair and a directory sweep without the host
/// having to pace itself. The sizing question that actually needs measuring is
/// the write queue's, against the stack high-water.
pub const QUEUE_LEN: usize = 32;

/// Attempts a requested block gets before it is reported failed.
///
/// The masked CPU capture measured ~1.3 % read failures and the DMA capture
/// removes their cause, so three is generous rather than tuned; a block that
/// burns all three is a fault to report, not a rate to absorb.
pub const READ_TRIES: u8 = 3;

/// Who asked for a block, and so who its answer goes to.
///
/// Two consumers share the one read path: the host service, whose reads are
/// the dongle's and end with the link, and the writer, whose read-back of a
/// block it has just written is part of the drain and survives a link drop.
/// A slot carries its owner so the firmware can hand each result to the right
/// one and clear one owner's work without the other's.
///
/// The writer's swap guard reads under `Writer` too, not under an owner of
/// its own: its fingerprint reads are asked for one at a time from the same
/// sequence, survive exactly what the read-back survives, and the sequence
/// knows which block it is waiting for. A third owner would add a failure
/// slot and an ordering for no distinction the firmware would act on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    /// A `01 READ` from the dongle.
    Host,
    /// The writer's read-back (`write_seq`, `Step::Verify`), or one of its
    /// guard's fingerprint blocks (`Step::Guard`).
    Writer,
}

/// One requested block, who asked, and how many attempts it has had.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Req {
    pub block: u8,
    pub tries: u8,
    pub owner: Owner,
}

/// How a capture came out, latched with its request until it is applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapOutcome {
    /// Nothing answered, or the bus never went idle to be armed on.
    NoReply,
    /// The capture triggered but did not deliver both whole streams.
    Incomplete,
    /// Both streams whole — the caller holds them.
    Streams,
}

/// Where a read failed. Ordered from the wire inwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// No reply, or none that started the capture.
    NoReply,
    /// The capture or the reply stopped short.
    Truncated,
    /// No start pattern in the samples.
    NoStart,
    /// Decoded, but not a storage `DATA_TRANSFER` of the right length.
    BadFrame,
    /// The right frame, for another function or another block.
    BadLocation,
    /// The frame's checksum did not close.
    Crc,
}

/// How many [`Stage`]s there are, for [`Stats::failed`].
pub const N_STAGES: usize = 6;

/// Read counters, as plain totals.
///
/// Every consumer of these — a bench capture, the host service's status, a
/// later ADR-018 cadence re-check — wants a rate over a run, and a total is the
/// only shape all three can take.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// Commands put on the bus.
    pub issued: u32,
    /// Blocks decoded and accepted.
    pub good: u32,
    /// Attempts that failed, indexed by [`Stage`].
    pub failed: [u32; N_STAGES],
    /// Failed attempts that were re-queued.
    pub retries: u32,
    /// Blocks that exhausted [`READ_TRIES`] and were reported failed.
    pub dropped: u32,
    /// Requests refused because the queue was full.
    pub refused: u32,
}

impl Stats {
    pub const ZERO: Self = Self {
        issued: 0,
        good: 0,
        failed: [0; N_STAGES],
        retries: 0,
        dropped: 0,
        refused: 0,
    };

    fn note_fail(&mut self, stage: Stage) {
        if let Some(c) = self.failed.get_mut(stage as usize) {
            *c = c.saturating_add(1);
        }
    }
}

/// The queue and the three slots.
pub struct ReadPipeline {
    queue: Deque<Req, QUEUE_LEN>,
    latched: Option<(Req, CapOutcome)>,
    decoding: Option<Req>,
    ready: Option<(u8, Owner)>,
    /// One failure report per owner. A decode ending can fail the host's
    /// block and, in the same call, apply and fail a latched read-back of the
    /// writer's — a single slot lost one of the two, and the writer stranded
    /// in its verify with `draining()` true for good (2026-09-22).
    failed_host: Option<(u8, Stage)>,
    failed_writer: Option<(u8, Stage)>,
    stats: Stats,
}

impl Default for ReadPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadPipeline {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: Deque::new(),
            latched: None,
            decoding: None,
            ready: None,
            failed_host: None,
            failed_writer: None,
            stats: Stats::ZERO,
        }
    }

    #[must_use]
    pub const fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Ask for a block on the host's behalf. `false` means the queue is full
    /// and the request was not taken — the caller must retry it or report it,
    /// never assume it landed.
    pub fn request(&mut self, block: u8) -> bool {
        let req = Req {
            block,
            tries: 0,
            owner: Owner::Host,
        };
        if self.queue.push_back(req).is_ok() {
            true
        } else {
            self.stats.refused = self.stats.refused.saturating_add(1);
            false
        }
    }

    /// Ask for a block on the writer's behalf: its read-back of a block just
    /// written. Goes to the **front** — it is the drain's critical path, and a
    /// host read waiting one more slot costs the dongle nothing it notices.
    pub fn request_verify(&mut self, block: u8) -> bool {
        let req = Req {
            block,
            tries: 0,
            owner: Owner::Writer,
        };
        if self.queue.push_front(req).is_ok() {
            true
        } else {
            self.stats.refused = self.stats.refused.saturating_add(1);
            false
        }
    }

    #[must_use]
    pub const fn queued(&self) -> usize {
        self.queue.len()
    }

    /// What the schedule's `Ctx::work` should say this window.
    ///
    /// `Exhausted` the moment nothing is queued, which cancels an owed read and
    /// releases the display. That is the right reading of a host-driven queue,
    /// and it is the opposite of what a cyclic pull passes: a pull that
    /// restarts is never exhausted, whereas here "nothing is owed" is exactly
    /// what an empty queue means. A later request starts a new stall.
    ///
    /// Deliberately **not** `Remaining` while a decode finishes with an empty
    /// queue: that hands out read slots with nothing to issue, and each one
    /// costs a controller poll.
    #[must_use]
    pub fn work(&self) -> ReadWork {
        if self.queue.is_empty() {
            ReadWork::Exhausted
        } else {
            ReadWork::Remaining
        }
    }

    /// What the schedule's `Ctx::pipeline_busy` should say: a capture is
    /// latched and not yet applied, so a second would overwrite it.
    #[must_use]
    pub const fn pipeline_busy(&self) -> bool {
        self.latched.is_some()
    }

    #[must_use]
    pub const fn decoding(&self) -> bool {
        self.decoding.is_some()
    }

    /// The request under decode, for judging the frame it produced.
    #[must_use]
    pub const fn decoding_req(&self) -> Option<Req> {
        self.decoding
    }

    #[must_use]
    pub const fn ready_pending(&self) -> bool {
        self.ready.is_some()
    }

    /// The next block to issue. The caller commits it to the bus and must
    /// follow with [`Self::note_latched`], whatever the capture did.
    pub fn take_for_issue(&mut self) -> Option<Req> {
        let req = self.queue.pop_front()?;
        self.stats.issued = self.stats.issued.saturating_add(1);
        Some(req)
    }

    /// Record how the capture came out. The caller holds the samples.
    pub const fn note_latched(&mut self, req: Req, outcome: CapOutcome) {
        self.latched = Some((req, outcome));
    }

    /// The latched capture, if it may be applied **now**.
    ///
    /// `None` while a decode is in flight or a decoded block is unclaimed — the
    /// consumer's backpressure runs through the same latch the scheduler reads,
    /// rather than through a second mechanism. See the module's liveness rule:
    /// the caller must ask every window, not only when something changed.
    pub const fn take_applicable(&mut self) -> Option<(Req, CapOutcome)> {
        if self.decoding.is_some() || self.ready.is_some() {
            return None;
        }
        self.latched.take()
    }

    /// The decoder has been primed on the applied capture.
    pub const fn note_decode_started(&mut self, req: Req) {
        self.decoding = Some(req);
    }

    /// The decode ended and the frame was accepted. The block goes to the ready
    /// slot with its owner; the caller copies its bytes.
    pub const fn note_decoded(&mut self, req: Req) {
        self.decoding = None;
        self.ready = Some((req.block, req.owner));
        self.stats.good = self.stats.good.saturating_add(1);
    }

    /// An attempt failed: re-queue the block if it has attempts left, otherwise
    /// report it. Re-queued at the **front** — a block the host asked for first
    /// stays first, and a retry sent to the back would reorder a sweep every
    /// time a capture missed.
    ///
    /// Clears `decoding` when the failure is the decoding request's, so a
    /// caller does not have to remember which of the two paths it is on.
    pub fn fail(&mut self, req: Req, stage: Stage) {
        if self.decoding == Some(req) {
            self.decoding = None;
        }
        self.stats.note_fail(stage);
        let tries = req.tries.saturating_add(1);
        if tries < READ_TRIES
            && self
                .queue
                .push_front(Req {
                    block: req.block,
                    tries,
                    owner: req.owner,
                })
                .is_ok()
        {
            self.stats.retries = self.stats.retries.saturating_add(1);
        } else {
            self.stats.dropped = self.stats.dropped.saturating_add(1);
            let slot = match req.owner {
                Owner::Host => &mut self.failed_host,
                Owner::Writer => &mut self.failed_writer,
            };
            // A second failure of the same owner before the first is taken
            // cannot happen — one outstanding host read, one read-back at a
            // time — and if it ever does, the first report is the one owed;
            // `Stats::dropped` stays honest about the count.
            if slot.is_none() {
                *slot = Some((req.block, stage));
            }
        }
    }

    /// Take the decoded block's number and owner, releasing the backpressure
    /// it holds.
    ///
    /// Releasing it is all this does — it does **not** resume the latch. The
    /// caller's unconditional per-window [`Self::take_applicable`] is what does
    /// that, which is the whole of the module's liveness rule.
    pub const fn take_ready(&mut self) -> Option<(u8, Owner)> {
        self.ready.take()
    }

    /// Take a report of a block that exhausted its attempts, with who asked.
    /// The writer's first, then the host's; the caller drains until `None`,
    /// since both can be due in one window.
    pub const fn take_failed(&mut self) -> Option<(u8, Stage, Owner)> {
        if let Some((b, s)) = self.failed_writer.take() {
            return Some((b, s, Owner::Writer));
        }
        if let Some((b, s)) = self.failed_host.take() {
            return Some((b, s, Owner::Host));
        }
        None
    }

    /// Drop every outstanding request and every slot. The card these reads were
    /// owed to is gone — an undock, a swapped card, a controller that went
    /// away — and nothing captured under it may be served as an answer about
    /// its replacement. The writer's read-back goes too: the block it was
    /// checking is `DISCARDED` with the generation.
    ///
    /// Counters survive: they describe the run, not the connection.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.latched = None;
        self.decoding = None;
        self.ready = None;
        self.failed_host = None;
        self.failed_writer = None;
    }

    /// Drop the host's requests and slots and keep the writer's. The link
    /// went, or input resumed under an issued host read: what the dongle was
    /// owed has no consumer, but the card is still there and the drain is
    /// still running, so a read-back in flight stays in flight.
    pub fn clear_host(&mut self) {
        // `Deque` has no `retain`: rebuild it, front to back, keeping order.
        let n = self.queue.len();
        for _ in 0..n {
            if let Some(req) = self.queue.pop_front() {
                if req.owner == Owner::Writer {
                    // Cannot fail: the queue has just given up at least one slot.
                    let _ = self.queue.push_back(req);
                }
            }
        }
        if self.latched.is_some_and(|(r, _)| r.owner == Owner::Host) {
            self.latched = None;
        }
        if self.decoding.is_some_and(|r| r.owner == Owner::Host) {
            self.decoding = None;
        }
        if self.ready.is_some_and(|(_, o)| o == Owner::Host) {
            self.ready = None;
        }
        self.failed_host = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u8 = 10;
    const B: u8 = 20;

    /// Drive one block from request to ready, the way the firmware does.
    fn run_one(p: &mut ReadPipeline, block: u8) {
        assert!(p.request(block));
        let req = p.take_for_issue().expect("a queued block");
        p.note_latched(req, CapOutcome::Streams);
        let (r, o) = p.take_applicable().expect("nothing in the way");
        assert_eq!(o, CapOutcome::Streams);
        p.note_decode_started(r);
        p.note_decoded(r);
    }

    #[test]
    fn empty_queue_is_exhausted_and_a_request_makes_it_remaining() {
        let mut p = ReadPipeline::new();
        assert_eq!(p.work(), ReadWork::Exhausted);
        assert!(p.request(A));
        assert_eq!(p.work(), ReadWork::Remaining);
    }

    /// A decode in flight does not by itself make work remain: the slot would
    /// be handed out with nothing to issue, and each one costs a poll.
    #[test]
    fn decoding_with_an_empty_queue_is_still_exhausted() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        let req = p.take_for_issue().unwrap();
        p.note_latched(req, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);
        assert!(p.decoding());
        assert_eq!(p.work(), ReadWork::Exhausted);
    }

    #[test]
    fn a_latched_capture_makes_the_pipeline_busy() {
        let mut p = ReadPipeline::new();
        assert!(!p.pipeline_busy());
        assert!(p.request(A));
        let req = p.take_for_issue().unwrap();
        p.note_latched(req, CapOutcome::Streams);
        assert!(p.pipeline_busy());
        assert!(p.take_applicable().is_some());
        assert!(!p.pipeline_busy());
    }

    #[test]
    fn a_decode_in_flight_holds_the_latch() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        assert!(p.request(B));
        let ra = p.take_for_issue().unwrap();
        p.note_latched(ra, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);
        let rb = p.take_for_issue().unwrap();
        p.note_latched(rb, CapOutcome::Streams);
        assert!(p.take_applicable().is_none(), "B must wait for A's decode");
        assert!(p.pipeline_busy());
    }

    /// The regression this module was split out for (2026-09-18).
    ///
    /// A finishes while B is latched, so B cannot be applied — `ready` is
    /// occupied. Taking A must leave B reachable: the next unconditional ask
    /// gets it. The firmware bug was that nothing asked again.
    #[test]
    fn pipeline_stranded_latch_resumes_when_ready_is_taken() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        assert!(p.request(B));

        // A is captured and decoding.
        let ra = p.take_for_issue().unwrap();
        p.note_latched(ra, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);

        // B is captured while A decodes — the two-deep pipeline.
        let rb = p.take_for_issue().unwrap();
        p.note_latched(rb, CapOutcome::Streams);

        // A decodes cleanly and sits in the ready slot, so B stays latched.
        p.note_decoded(r);
        assert!(p.ready_pending());
        assert!(p.take_applicable().is_none(), "ready blocks B");
        assert!(p.pipeline_busy());

        // The consumer drains A. B must now be applicable.
        assert_eq!(p.take_ready(), Some((A, Owner::Host)));
        let (got, outcome) = p
            .take_applicable()
            .expect("B must progress once ready is drained");
        assert_eq!(got.block, B);
        assert_eq!(outcome, CapOutcome::Streams);
        assert!(!p.pipeline_busy());
    }

    /// The same stall, reached the other way: the consumer never drains, so B
    /// stays latched and the scheduler keeps refusing read windows. That is
    /// backpressure working, not the bug above.
    #[test]
    fn an_undrained_block_holds_the_pipeline_indefinitely() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        assert!(p.request(B));
        let ra = p.take_for_issue().unwrap();
        p.note_latched(ra, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);
        let rb = p.take_for_issue().unwrap();
        p.note_latched(rb, CapOutcome::Streams);
        p.note_decoded(r);
        for _ in 0..100 {
            assert!(p.take_applicable().is_none());
            assert!(p.pipeline_busy());
        }
    }

    #[test]
    fn a_failed_attempt_is_requeued_at_the_front() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        assert!(p.request(B));
        let ra = p.take_for_issue().unwrap();
        p.fail(ra, Stage::NoReply);
        assert_eq!(p.stats().retries, 1);
        assert_eq!(p.stats().failed[Stage::NoReply as usize], 1);
        // A keeps its place ahead of B.
        assert_eq!(p.take_for_issue().unwrap().block, A);
    }

    #[test]
    fn a_block_is_dropped_after_its_attempts_and_reported_once() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        for _ in 0..READ_TRIES {
            let r = p.take_for_issue().expect("still queued");
            p.fail(r, Stage::Truncated);
        }
        assert!(p.take_for_issue().is_none(), "no attempts left");
        assert_eq!(p.stats().dropped, 1);
        assert_eq!(p.stats().retries, u32::from(READ_TRIES) - 1);
        assert_eq!(p.take_failed(), Some((A, Stage::Truncated, Owner::Host)));
        assert_eq!(p.take_failed(), None);
    }

    /// A failure on the decoding request must clear `decoding`, or the
    /// pipeline holds a decode that ended and nothing can be applied again.
    #[test]
    fn failing_the_decoding_request_releases_the_decode_slot() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        let ra = p.take_for_issue().unwrap();
        p.note_latched(ra, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);
        assert!(p.decoding());
        p.fail(r, Stage::Crc);
        assert!(!p.decoding(), "the decode slot must be free");
        assert!(p.take_applicable().is_none(), "nothing latched");
    }

    /// A failure on a *latched* request while another decodes must not touch
    /// the decode slot.
    #[test]
    fn failing_a_latched_request_leaves_a_running_decode_alone() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        assert!(p.request(B));
        let ra = p.take_for_issue().unwrap();
        p.note_latched(ra, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);
        let rb = p.take_for_issue().unwrap();
        p.fail(rb, Stage::NoReply);
        assert_eq!(p.decoding_req().map(|q| q.block), Some(A));
    }

    #[test]
    fn a_full_queue_refuses_and_counts() {
        let mut p = ReadPipeline::new();
        for i in 0..QUEUE_LEN {
            assert!(p.request(u8::try_from(i).unwrap()));
        }
        assert!(!p.request(200));
        assert_eq!(p.stats().refused, 1);
        assert_eq!(p.queued(), QUEUE_LEN);
    }

    /// A drop rather than a retry when the queue is full: the block cannot be
    /// re-queued, so it is reported instead of vanishing.
    #[test]
    fn a_failure_that_cannot_be_requeued_is_dropped_not_lost() {
        let mut p = ReadPipeline::new();
        for i in 0..QUEUE_LEN {
            assert!(p.request(u8::try_from(i).unwrap()));
        }
        let r = p.take_for_issue().unwrap();
        // Refill the slot the issue freed, so the retry has nowhere to go.
        assert!(p.request(200));
        p.fail(r, Stage::NoStart);
        assert_eq!(p.stats().dropped, 1);
        assert_eq!(p.stats().retries, 0);
        assert_eq!(
            p.take_failed(),
            Some((r.block, Stage::NoStart, Owner::Host))
        );
    }

    /// The writer's read-back goes to the front and comes back tagged as its
    /// own, ahead of a host read that was queued first.
    #[test]
    fn a_verify_read_goes_first_and_comes_back_to_the_writer() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        assert!(p.request_verify(B));
        let req = p.take_for_issue().unwrap();
        assert_eq!((req.block, req.owner), (B, Owner::Writer));
        p.note_latched(req, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);
        p.note_decoded(r);
        assert_eq!(p.take_ready(), Some((B, Owner::Writer)));
        let host = p.take_for_issue().unwrap();
        assert_eq!((host.block, host.owner), (A, Owner::Host));
    }

    /// A read-back keeps its owner through the retry rule, so its failure
    /// report reaches the writer and not the host.
    #[test]
    fn a_verify_reads_failure_is_reported_to_the_writer() {
        let mut p = ReadPipeline::new();
        assert!(p.request_verify(B));
        for _ in 0..READ_TRIES {
            let req = p.take_for_issue().unwrap();
            assert_eq!(req.owner, Owner::Writer);
            p.note_latched(req, CapOutcome::NoReply);
            let (r, _) = p.take_applicable().unwrap();
            p.fail(r, Stage::NoReply);
        }
        assert_eq!(p.take_failed(), Some((B, Stage::NoReply, Owner::Writer)));
    }

    /// A link drop clears what the host was owed and nothing of the writer's:
    /// a queued read-back stays queued, a decoding one keeps decoding, a
    /// ready one is still there to take.
    #[test]
    fn clearing_the_host_keeps_the_writers_read_back() {
        // Queued behind a host read, with a host block decoding.
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        let ra = p.take_for_issue().unwrap();
        p.note_latched(ra, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);
        assert!(p.request(A));
        assert!(p.request_verify(B));
        p.clear_host();
        assert!(!p.decoding(), "the host's decode is dropped");
        assert_eq!(p.queued(), 1);
        let req = p.take_for_issue().unwrap();
        assert_eq!((req.block, req.owner), (B, Owner::Writer));

        // Decoding: the writer's decode survives, a host latch behind it does not.
        p.note_latched(req, CapOutcome::Streams);
        let (r, _) = p.take_applicable().unwrap();
        p.note_decode_started(r);
        assert!(p.request(A));
        let ra = p.take_for_issue().unwrap();
        p.note_latched(ra, CapOutcome::Streams);
        p.clear_host();
        assert!(p.decoding());
        assert!(!p.pipeline_busy());
        p.note_decoded(r);

        // Ready: still the writer's to take after the clear.
        p.clear_host();
        assert_eq!(p.take_ready(), Some((B, Owner::Writer)));
    }

    /// The case found 2026-09-22: the writer's read-back exhausts its
    /// tries, and in the same servicing a latched host capture exhausts its
    /// own. Both reports must reach their owners — with one slot the host's
    /// overwrote the writer's, and the writer waited in its verify forever.
    #[test]
    fn a_host_failure_does_not_overwrite_the_writers() {
        let mut p = ReadPipeline::new();
        assert!(p.request_verify(B));
        for _ in 0..READ_TRIES {
            let r = p.take_for_issue().unwrap();
            p.note_latched(r, CapOutcome::NoReply);
            let (r, _) = p.take_applicable().unwrap();
            p.fail(r, Stage::NoReply);
        }
        assert!(p.request(A));
        for _ in 0..READ_TRIES {
            let r = p.take_for_issue().unwrap();
            p.note_latched(r, CapOutcome::Incomplete);
            let (r, _) = p.take_applicable().unwrap();
            p.fail(r, Stage::Truncated);
        }
        assert_eq!(p.take_failed(), Some((B, Stage::NoReply, Owner::Writer)));
        assert_eq!(p.take_failed(), Some((A, Stage::Truncated, Owner::Host)));
        assert_eq!(p.take_failed(), None);
        assert_eq!(p.stats().dropped, 2);
    }

    #[test]
    fn clearing_the_host_drops_its_failure_and_keeps_the_writers() {
        let mut p = ReadPipeline::new();
        for (owner_is_writer, block) in [(true, B), (false, A)] {
            assert!(if owner_is_writer {
                p.request_verify(block)
            } else {
                p.request(block)
            });
            for _ in 0..READ_TRIES {
                let r = p.take_for_issue().unwrap();
                p.note_latched(r, CapOutcome::NoReply);
                let (r, _) = p.take_applicable().unwrap();
                p.fail(r, Stage::NoReply);
            }
        }
        p.clear_host();
        assert_eq!(p.take_failed(), Some((B, Stage::NoReply, Owner::Writer)));
        assert_eq!(p.take_failed(), None);
    }

    #[test]
    fn clearing_the_host_drops_a_host_result_that_was_ready() {
        let mut p = ReadPipeline::new();
        run_one(&mut p, A);
        assert!(p.ready_pending());
        p.clear_host();
        assert!(!p.ready_pending());
        assert_eq!(p.take_ready(), None);
    }

    #[test]
    fn clear_drops_every_slot_but_keeps_the_counters() {
        let mut p = ReadPipeline::new();
        run_one(&mut p, A);
        assert!(p.request(B));
        let rb = p.take_for_issue().unwrap();
        p.note_latched(rb, CapOutcome::Streams);
        p.clear();
        assert_eq!(p.queued(), 0);
        assert!(!p.pipeline_busy());
        assert!(!p.decoding());
        assert!(!p.ready_pending());
        assert_eq!(p.work(), ReadWork::Exhausted);
        assert_eq!(p.stats().good, 1);
        assert_eq!(p.stats().issued, 2);
    }

    #[test]
    fn a_no_reply_latch_carries_its_outcome_through() {
        let mut p = ReadPipeline::new();
        assert!(p.request(A));
        let r = p.take_for_issue().unwrap();
        p.note_latched(r, CapOutcome::NoReply);
        let (got, outcome) = p.take_applicable().unwrap();
        assert_eq!(got, r);
        assert_eq!(outcome, CapOutcome::NoReply);
    }

    /// Three blocks end to end, asking unconditionally every step, the way the
    /// firmware's per-window service does. Nothing strands.
    #[test]
    fn a_sweep_of_three_blocks_completes() {
        let mut p = ReadPipeline::new();
        for b in [1u8, 2, 3] {
            assert!(p.request(b));
        }
        let mut taken = heapless::Vec::<u8, 4>::new();
        for _ in 0..64 {
            // The unconditional service step.
            if let Some((r, o)) = p.take_applicable() {
                assert_eq!(o, CapOutcome::Streams);
                p.note_decode_started(r);
                p.note_decoded(r);
            }
            if let Some((b, _)) = p.take_ready() {
                let _ = taken.push(b);
            }
            if !p.pipeline_busy() {
                if let Some(r) = p.take_for_issue() {
                    p.note_latched(r, CapOutcome::Streams);
                }
            }
        }
        assert_eq!(&taken[..], &[1, 2, 3]);
        assert_eq!(p.stats().good, 3);
        assert_eq!(p.stats().issued, 3);
    }
}
