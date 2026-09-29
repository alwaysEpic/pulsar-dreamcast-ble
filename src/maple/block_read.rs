// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! `BLOCK_READ` against the docked VMU's storage function, and the
//! incremental decode of its reply.
//!
//! One read is a `BLOCK_READ` command bit-banged onto the bus, the reply
//! captured by the SPIM/EasyDMA pair (ADR-018), and a
//! 525-byte frame decoded out of 53,248 samples in slices that yield between
//! them. The capture is hardware-timed; the decode is not, and at ~25 ms per
//! block it is what paces reads, not the cadence.
//!
//! # Where the parts come from
//!
//! This is the firmware half of [`maple_protocol::read_sched`], which decides
//! *when* a read runs and holds the LCD while one is owed. Both were built and
//! measured under the read diagnostic (v250–v299) and
//! neither carries any of it: the schedule came across as a host-tested module,
//! and this file is the read path rewritten from the diagnostic's — same bus
//! sequence, same pipeline shape, none of its modes, trials, telemetry or
//! placement pads.
//!
//! # The pipeline, and why it is two-deep
//!
//! `Empty → latched → decoding → ready → taken`. A read window latches its
//! [`Captured`] and returns; the decode runs afterwards in the slack of later
//! windows, and the *next* read window may capture another block while the
//! previous one is still decoding. That overlap is the throughput: a decode
//! spans several windows, and waiting for it before capturing again would put
//! reads one per decode instead of one per cadence slot.
//!
//! It is exactly two deep, and the scheduler enforces that: an unapplied latch
//! is [`BlockReader::pipeline_busy`], and a busy pipeline never gets a read
//! slot. Nothing here has to defend against a third.
//!
//! # What the block reply looks like
//!
//! A storage `DATA_TRANSFER`: the header word, the function word, the location
//! word, 512 data bytes and the checksum — 525 bytes, [`FRAME_BYTES`]. The
//! furthest reply edge the bench measured is at sample 46,155 of the 53,248
//! captured (v258); at 8 MHz that window is 6.66 ms.
//!
//! # Invariants that cost bench time to find
//!
//! - **The VMU must be enumerated before any storage command**, exactly as
//!   before an LCD write: an unenumerated card silently ignores the request.
//!   `main.rs` owns that; this path assumes it.
//! - **The read brings its own stream buffers.** One `SpimCapture` serves the
//!   controller poll and this path, and the poll arms it between this path's
//!   `finish` and its unpack one or more windows later. A latched `Captured`
//!   has to outlive the next `arm`, so the poll's pair cannot be borrowed —
//!   doing so unpacked the poll's bits at the poll's length (v287, run #175).
//! - **The capture needs no interrupt mask.** The CPU sampler did: 23–32 % of
//!   its captures were truncated by the app's own interrupts, and it took a
//!   SoftDevice-aware critical region to stop it (v263). EasyDMA does not care,
//!   and nothing here has masked since v277.
//!
//! # The byte order within a word, settled
//!
//! The wire sends each word least-significant byte first; the card's
//! filesystem, the console's memory and the dongle's image are all in the
//! reverse of that. [`Ready::data`] is **image order**: the block as the
//! filesystem lays it out, which is what the protocol carries and what
//! `maple_protocol::block_bytes` defines.
//!
//! The conversion happens exactly once, in [`BlockReader::finish`], on the way
//! out of the decoder — so this file is the only place in the firmware that
//! ever holds a block in wire order, and nothing downstream has to know the
//! wire had an opinion. The evidence for which way round it goes, and why the
//! link carries image order rather than wire order, is in that module's doc.

use heapless::Vec;
use maple_protocol::block_bytes::wire_to_image;
use maple_protocol::block_decode::{BlockDecoder, Outcome};
use maple_protocol::packed::unpack_into;
use maple_protocol::read_pipeline::{CapOutcome, Owner, ReadPipeline, Req, Stage, Stats};
use maple_protocol::read_sched::{ReadSched, ReadWork};

use super::gpio_bus::MapleBus;
use super::host::{addressing, commands, functions};
use super::spim_capture::{Captured, StreamBufs};
use super::MaplePacket;
use core::mem::MaybeUninit;

/// SDCKA and SDCKB as masks of a captured byte.
///
/// `gpio_bus` has the same two constants and keeps them private. They are
/// restated rather than shared for the reason that module's own helpers are:
/// the bus code's compiled shape is load-bearing, and a read path reaching into
/// it for two `const`s is an invitation to reach in for more.
const PIN_A: u8 = 1 << crate::board::PIN_A_BIT;
const PIN_B: u8 = 1 << crate::board::PIN_B_BIT;
const _: () = assert!(crate::board::PIN_A_BIT < 8 && crate::board::PIN_B_BIT < 8);

const PIN_A_MASK: u32 = 1 << crate::board::PIN_A_BIT;
const PIN_B_MASK: u32 = 1 << crate::board::PIN_B_BIT;

/// P0's `IN` register.
const P0_IN: u32 = 0x5000_0510;
/// Core clock, for the cycle deadlines below.
const CPU_MHZ: u32 = 64;

/// Data bytes in a VMU storage block.
///
/// The protocol crate's, re-exported: the frame arithmetic below and the
/// contract in `block_bytes` have to be the same 512 or the copy out of the
/// decoder silently truncates.
pub use maple_protocol::block_bytes::BLOCK_BYTES;

/// Bytes in a `BLOCK_READ` reply frame: header, function, location, block,
/// checksum.
pub const FRAME_BYTES: usize = 4 + 4 + 4 + BLOCK_BYTES + 1;

/// The reply's header length field, in words: the function and location words
/// plus the block.
const FRAME_WORDS: u8 = 130;
const _: () = assert!(FRAME_WORDS as usize == 2 + BLOCK_BYTES / 4);

/// Where the block's data starts in a decoded frame.
const DATA_AT: usize = 12;

/// Decoder output buffers, `FRAME_BYTES` rounded up to a word.
///
/// [`BlockDecoder::step`] writes one byte and one sample position per decoded
/// byte and stops short if either runs out, so both are sized together.
const OUT_BYTES: usize = 528;
const _: () = assert!(OUT_BYTES >= FRAME_BYTES && OUT_BYTES.is_multiple_of(4));

/// Samples in the block capture window: 6.66 ms at 8 MHz.
///
/// Sized from the rate measured into a dedicated buffer (8.00 cycles/sample,
/// v253) rather than into `SAMPLE_BUFFER` (11.00) — v253 shipped 40,960, which
/// covers 5.12 ms of a 5.70 ms reply and would have truncated every read. The
/// furthest edge seen since is sample 46,155 (v258), ~0.9 ms of margin. Under
/// 65,536 so a sample index fits the decoder's `u16` position table.
pub const CAPTURE_SAMPLES: usize = 53_248;
const _: () = assert!(CAPTURE_SAMPLES.is_multiple_of(4) && CAPTURE_SAMPLES < 65_536);

/// Bytes per SPIM stream: eight samples to the byte.
const STREAM_BYTES: usize = CAPTURE_SAMPLES / 8;
const _: () = assert!(STREAM_BYTES * 8 == CAPTURE_SAMPLES);
/// `RXD.MAXCNT` is 16 bits; `SpimCapture::arm` would clamp silently.
const _: () = assert!(STREAM_BYTES <= 0xFFFF);

/// How long the reply may take to start, from the command's last edge. The
/// VMU's turnaround is tens of microseconds; this is a silence deadline, not a
/// budget to spend.
const READ_TIMEOUT_US: u32 = 2_000;

/// How long both `END`s may take from the trigger: the transfer's own 6.66 ms
/// and a millisecond.
const END_TIMEOUT_CYCLES: u32 = (6_660 + 1_000) * CPU_MHZ;

/// Samples a decode slice scans before yielding. 1,024 keeps a slice under
/// ~0.5 ms, so a slice started inside the capture wait cannot overrun the
/// `END` it is waiting for.
pub const SLICE_SAMPLES: usize = 1_024;

/// Maple command: read one storage block.
const BLOCK_READ: u8 = 0x0B;

/// A decoded block, borrowed until the reader is used again.
pub struct Ready<'a> {
    pub block: u8,
    /// Who asked: the host service, or the writer checking its own work.
    pub owner: Owner,
    /// The 512 data bytes in **image order**, ready for the link as they
    /// stand — `maple_protocol::block_bytes` for what that means.
    pub data: &'a [u8; BLOCK_BYTES],
}

#[repr(C, align(4))]
struct CaptureBuf([u8; CAPTURE_SAMPLES]);

/// The unpacked capture the decoder scans.
///
/// Written only by the unpack in [`BlockReader::apply_pending`] and read only
/// by the decoder's slices, both on the poll task, never at the same time: a
/// latched capture is unpacked only while no decode is in progress.
static mut CAPTURE: CaptureBuf = CaptureBuf([0; CAPTURE_SAMPLES]);

fn capture() -> &'static [u8] {
    let base = core::ptr::addr_of!(CAPTURE).cast::<u8>();
    // SAFETY: `CAPTURE` is a static of exactly `CAPTURE_SAMPLES` bytes, live
    // for the program. The only writer is `apply_pending`'s unpack, on this
    // same task, and it runs only while no decode holds a slice from here —
    // `decoding` is clear — so no `&mut` to the region can be live.
    unsafe { core::slice::from_raw_parts(base, CAPTURE_SAMPLES) }
}

fn capture_mut() -> &'static mut [u8] {
    let base = core::ptr::addr_of_mut!(CAPTURE).cast::<u8>();
    // SAFETY: as `capture`, and its one caller (`apply_pending`) runs only
    // while `decoding` is clear, holding no other slice of the buffer.
    unsafe { core::slice::from_raw_parts_mut(base, CAPTURE_SAMPLES) }
}

/// The read's own two bit streams.
///
/// Not `gpio_bus`'s poll pair: the controller poll arms the shared
/// `SpimCapture` between this path's `finish` and its unpack one or more
/// windows later, and a latched `Captured` has to outlive that arm. `.uninit`
/// for the reason `spim_capture`'s module doc gives — cortex-m-rt places it
/// after `.bss` and does not zero it at boot, so no existing RAM symbol moves
/// and EasyDMA rather than the reset handler fills them.
#[link_section = ".uninit.read_streams"]
static mut READ_STREAM_A: MaybeUninit<[u8; STREAM_BYTES]> = MaybeUninit::uninit();
#[link_section = ".uninit.read_streams"]
static mut READ_STREAM_B: MaybeUninit<[u8; STREAM_BYTES]> = MaybeUninit::uninit();

/// The read's transfer length. The controller poll arms the same instance with
/// its own, shorter length into its own pair; `finish` hands this read its
/// pointers and length back inside the `Captured`, so neither disturbs the
/// other.
#[expect(
    clippy::cast_possible_truncation,
    reason = "STREAM_BYTES is CAPTURE_SAMPLES / 8, asserted <= 0xFFFF above"
)]
const READ_MAXCNT_BYTES: u32 = STREAM_BYTES as u32;

/// P0's input register.
#[inline]
fn p0_in() -> u32 {
    // SAFETY: P0.IN is a fixed, word-aligned, read-only MMIO register on this
    // part; a volatile read of it has no side effects and races nothing.
    unsafe { core::ptr::read_volatile(P0_IN as *const u32) }
}

/// The DWT cycle counter. Enabled once in `MapleBus::new`.
#[inline]
fn cyc() -> u32 {
    // SAFETY: CYCCNT is a free-running read-only counter; reading is always
    // safe, and DWT is enabled before any bus transaction.
    unsafe { (*cortex_m::peripheral::DWT::PTR).cyccnt.read() }
}

/// Arm the shared capture for the reply about to come, into this path's own
/// buffers.
fn spim_arm(bus: &mut MapleBus) {
    let a = core::ptr::addr_of_mut!(READ_STREAM_A).cast::<u8>();
    let b = core::ptr::addr_of_mut!(READ_STREAM_B).cast::<u8>();
    // SAFETY: `READ_STREAM_A` and `READ_STREAM_B` are two distinct statics of
    // exactly `STREAM_BYTES` bytes each, in RAM as EasyDMA requires, live for
    // the program, and this path is their only user — `spim_arm` the only arm,
    // `apply_pending`'s `Captured::streams` the only read — both on the poll
    // task of a single-core part, with the read after a `finish` that saw both
    // `END`s and fenced. The controller poll arms the same instance in
    // between, into `gpio_bus`'s own pair, touching neither region.
    let read_bufs = unsafe { StreamBufs::new(a, b, READ_MAXCNT_BYTES) };
    if let Some(cap) = bus.capture_mut() {
        cap.arm(read_bufs);
    }
}

/// Both `END`s have fired: disarm, fence, and hand back the capture. `None`
/// means a stream was short. The instance keeps nothing afterwards, which is
/// what lets this be latched across the controller poll's own captures.
fn spim_finish(bus: &mut MapleBus) -> Option<Captured> {
    bus.capture_mut().and_then(|cap| cap.finish(0).captured)
}

/// No reply, or not a whole one: nothing to wait for.
fn spim_abort(bus: &mut MapleBus) {
    if let Some(cap) = bus.capture_mut() {
        cap.abort();
    }
}

/// `BLOCK_READ` of `block`, the bus to input, the capture armed, then the wait
/// for the bus to go idle. `false` means it never did and the capture was
/// disarmed; the reply itself is waited for by [`BlockReader::capture_wait`],
/// which is where decode slices run meanwhile.
fn block_read_capture(bus: &mut MapleBus, block: u8) -> bool {
    let mut payload: Vec<u32, 32> = Vec::new();
    let _ = payload.push(functions::STORAGE);
    // Location word: partition << 24 | phase << 16 | block. Partition 0 and
    // phase 0 for a read of the docked card; confirmed on the v250/v252 bench.
    let _ = payload.push(u32::from(block));
    let packet = MaplePacket {
        sender: addressing::HOST,
        recipient: addressing::SUB_SLOT_1,
        command: BLOCK_READ,
        payload,
    };
    bus.write_packet(&packet);
    bus.set_input_mode();
    // Armed before the idle wait: the trigger is the reply's first SDCKA fall,
    // and arming after the bus has already gone idle would race it.
    spim_arm(bus);

    let t0 = cyc();
    let idle_budget = (READ_TIMEOUT_US / 2).saturating_mul(CPU_MHZ);
    loop {
        let v = p0_in();
        if v & PIN_A_MASK != 0 && v & PIN_B_MASK != 0 {
            return true;
        }
        if cyc().wrapping_sub(t0) > idle_budget {
            spim_abort(bus);
            return false;
        }
    }
}

/// The firmware half of the block-read pipeline: the bus, the capture buffers
/// and the decoder.
///
/// Every decision about *which* request owns *which* slot belongs to
/// [`ReadPipeline`], which is host-tested; this holds only what cannot leave
/// the target — the `Captured` the hardware handed back, the sample buffer, the
/// decoder and its frame.
pub struct BlockReader {
    /// The queue and the three slots. The one source of truth for what may
    /// happen next.
    pipe: ReadPipeline,
    /// The streams of the latched capture, written exactly when
    /// `ReadPipeline::note_latched` records one and read exactly when
    /// `take_applicable` hands it back.
    latched: Option<Captured>,
    decoder: BlockDecoder<PIN_A, PIN_B>,
    out: [u8; OUT_BYTES],
    /// Sample index of each decoded byte's last bit. The decoder writes one per
    /// byte and stops short without it; it is also the only thing that says
    /// *where* a truncated reply stopped, which is how the interrupt-hole
    /// question was answered and how a cadence regression would be.
    pos: [u16; OUT_BYTES],
    /// The decoded block, copied out of `out` so a decode starting behind the
    /// consumer's back cannot overwrite what it is reading.
    ready_data: [u8; BLOCK_BYTES],
    /// Samples the last unpack produced — what the decoder may scan.
    cap_len: usize,
}

impl Default for BlockReader {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockReader {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pipe: ReadPipeline::new(),
            latched: None,
            decoder: BlockDecoder::new(),
            out: [0; OUT_BYTES],
            pos: [0; OUT_BYTES],
            ready_data: [0; BLOCK_BYTES],
            cap_len: 0,
        }
    }

    #[must_use]
    pub const fn stats(&self) -> &Stats {
        self.pipe.stats()
    }

    /// The samples the current decode may scan.
    fn samples(&self) -> &'static [u8] {
        capture().get(..self.cap_len).unwrap_or(&[])
    }

    /// Ask for a block on the host's behalf. `false` means the queue is full
    /// and the request was not taken — the caller must retry it or report it,
    /// never assume it landed.
    pub fn request(&mut self, block: u8) -> bool {
        self.pipe.request(block)
    }

    /// Ask for a block on the writer's behalf — its read-back, or one of its
    /// guard's fingerprint blocks. Goes to the front of the queue; see
    /// [`ReadPipeline::request_verify`].
    pub fn request_verify(&mut self, block: u8) -> bool {
        self.pipe.request_verify(block)
    }

    #[must_use]
    pub const fn queued(&self) -> usize {
        self.pipe.queued()
    }

    /// Drop every outstanding request, any decode in flight and any latched
    /// capture. The card these reads were owed to is gone — an undock, a
    /// swapped card, a lost controller — and nothing captured under it may be
    /// served as an answer about its replacement. The writer's read-back goes
    /// with it; the writer learns that from the service, not from here.
    pub fn clear(&mut self) {
        self.pipe.clear();
        self.latched = None;
    }

    /// Drop the host's requests and slots and keep the writer's. The link
    /// dropped, or input resumed under an issued host read; the card is still
    /// there and a read-back in flight stays in flight. The streams go with
    /// the pipeline's latch: kept if the latch that remains is the writer's.
    pub fn clear_host(&mut self) {
        self.pipe.clear_host();
        if !self.pipe.pipeline_busy() {
            self.latched = None;
        }
    }

    /// What the schedule's [`Ctx::work`] should say this window.
    ///
    /// [`Ctx::work`]: maple_protocol::read_sched::Ctx::work
    #[must_use]
    pub fn work(&self) -> ReadWork {
        self.pipe.work()
    }

    /// What the schedule's [`Ctx::pipeline_busy`] should say: a capture is
    /// latched and not yet unpacked, so a second would overwrite it.
    ///
    /// [`Ctx::pipeline_busy`]: maple_protocol::read_sched::Ctx::pipeline_busy
    #[must_use]
    pub const fn pipeline_busy(&self) -> bool {
        self.pipe.pipeline_busy()
    }

    #[must_use]
    pub const fn decoding(&self) -> bool {
        self.pipe.decoding()
    }

    /// **Apply whatever can be applied.** Call this once at every window head,
    /// unconditionally — not only when something looks like it changed.
    ///
    /// This is the pipeline's liveness guarantee and the only place a latch
    /// becomes a decode. The two other calls to it, at the end of a read window
    /// and at the end of a decode, exist purely to save a window of latency;
    /// removing either would slow the pipeline and could not strand work.
    ///
    /// Leaving it out of the window head, on the other hand, *does* strand
    /// work, and the way it does so is silent. A decode that ends while the
    /// consumer still holds the previous block leaves the next capture latched:
    /// `decoding` is false so the slack loop does nothing, `pipeline_busy` is
    /// true so the scheduler grants no read window, and no other path calls
    /// this. Reads then stop for good. That was the shipped shape until the
    /// fix of 2026-09-18; `read_pipeline`'s
    /// `pipeline_stranded_latch_resumes_when_ready_is_taken` is the regression.
    pub fn service(&mut self) {
        let Some((req, outcome)) = self.pipe.take_applicable() else {
            return;
        };
        let captured = self.latched.take();
        match outcome {
            CapOutcome::NoReply => self.pipe.fail(req, Stage::NoReply),
            CapOutcome::Incomplete => self.pipe.fail(req, Stage::Truncated),
            CapOutcome::Streams => {
                let Some(c) = captured else {
                    // `Streams` is latched with a `Some`, so this is
                    // unreachable; kept so that were it ever reached it reads
                    // as a truncation rather than decoding the previous block's
                    // samples a second time.
                    self.pipe.fail(req, Stage::Truncated);
                    return;
                };
                let (a, b) = c.streams();
                // The unpack's count, not `CAPTURE_SAMPLES`: a `Captured`
                // promises both streams whole, so the two agree today, but
                // decoding past what was actually unpacked would scan the
                // previous block's samples and could decode a stale frame
                // whole. Short streams read as a truncation instead.
                self.cap_len = unpack_into(a, b, capture_mut(), PIN_A, PIN_B);
                if self.decoder.begin(self.samples()).is_some() {
                    self.pipe.fail(req, Stage::NoStart);
                    return;
                }
                self.pipe.note_decode_started(req);
            }
        }
    }

    /// Take the decoded block, if one is waiting.
    ///
    /// Taking it releases the backpressure it was holding, so the pipeline is
    /// serviced before the borrow is handed out — the capture waiting behind it
    /// starts decoding in this window rather than the next. The window head's
    /// `service` would pick it up anyway; this only saves the wait.
    ///
    /// The data is a copy, not the decoder's frame buffer, so the decode this
    /// starts cannot overwrite what the caller is reading.
    pub fn take_ready(&mut self) -> Option<Ready<'_>> {
        let (block, owner) = self.pipe.take_ready()?;
        self.service();
        Some(Ready {
            block,
            owner,
            data: &self.ready_data,
        })
    }

    /// Take the report of a block that exhausted its attempts, with who asked.
    pub const fn take_failed(&mut self) -> Option<(u8, Stage, Owner)> {
        self.pipe.take_failed()
    }

    /// The read window: one `BLOCK_READ` and its capture, with the decoder's
    /// slices running while the DMA fills, then the capture applied if nothing
    /// is in its way. Called at the head of a quiet window in place of the
    /// controller poll.
    ///
    /// `sched` and `now` are here rather than at the call site because
    /// *committing the command to the bus* is what consumes a cadence slot and
    /// ends a gap deferral — not the `Slot::Read` that sent us here. A dispatch
    /// that finds no block must leave both untouched, and keeping the two calls
    /// beside the decision is what makes that impossible to get wrong.
    pub fn read_window(&mut self, bus: &mut MapleBus, sched: &mut ReadSched, now: u64) {
        let Some(req) = self.pipe.take_for_issue() else {
            // Nothing sent. Cadence and pending state stay exactly as they
            // were, so the slot is due again next window. It may well have
            // passed the gap check — its problem is that no command went out,
            // so it is not a gap deferral and is not counted as one.
            sched.note_read_abandoned();
            return;
        };
        sched.note_read_issued(now);

        let armed = block_read_capture(bus, req.block);
        self.capture_wait(bus, req, armed);
        self.service();
    }

    /// Wait for the reply: the trigger within [`READ_TIMEOUT_US`], then both
    /// `END`s within [`END_TIMEOUT_CYCLES`], with decode slices running between
    /// the `END` checks while a decode is in flight. A slice is well under half
    /// a millisecond, so `END` is seen within one of it firing.
    ///
    /// The trigger phase does not slice: it is the VMU's turnaround, tens of
    /// microseconds, and a slice started there would be the whole of it.
    fn capture_wait(&mut self, bus: &mut MapleBus, req: Req, armed: bool) {
        if !armed {
            self.latch(req, CapOutcome::NoReply, None);
            return;
        }
        let t0 = cyc();
        let start_budget = READ_TIMEOUT_US.saturating_mul(CPU_MHZ);
        let t_trig = loop {
            if bus.capture_mut().is_some_and(|c| c.started()) {
                break cyc();
            }
            if cyc().wrapping_sub(t0) > start_budget {
                spim_abort(bus);
                self.latch(req, CapOutcome::NoReply, None);
                return;
            }
        };
        // The capture comes out of `finish` and goes straight into the latch;
        // it is never fetched back from the instance, which by then may have
        // been armed again by a controller poll.
        let (outcome, captured) = loop {
            if bus.capture_mut().is_some_and(|c| c.ended()) {
                break spim_finish(bus).map_or((CapOutcome::Incomplete, None), |c| {
                    (CapOutcome::Streams, Some(c))
                });
            }
            if cyc().wrapping_sub(t_trig) > END_TIMEOUT_CYCLES {
                spim_abort(bus);
                break (CapOutcome::Incomplete, None);
            }
            if self.decoding() {
                self.run_slice();
            }
        };
        self.latch(req, outcome, captured);
    }

    /// Record the capture against its request. The streams and the bookkeeping
    /// are set together, here and nowhere else, so `Some`-ness cannot drift
    /// apart from `ReadPipeline`'s view of the latch.
    const fn latch(&mut self, req: Req, outcome: CapOutcome, captured: Option<Captured>) {
        self.latched = captured;
        self.pipe.note_latched(req, outcome);
    }

    /// One decode slice, and the verdict if it ended. `false` when no decode is
    /// in flight.
    pub fn run_slice(&mut self) -> bool {
        if !self.decoding() {
            return false;
        }
        let r = self
            .decoder
            .step(self.samples(), &mut self.out, &mut self.pos, SLICE_SAMPLES);
        if let Some(o) = r {
            self.finish(o);
        }
        true
    }

    /// A word of the decoded frame's payload, `i` words past the header.
    const fn word(&self, i: usize) -> u32 {
        let at = 4 + i * 4;
        u32::from_le_bytes([
            self.out[at],
            self.out[at + 1],
            self.out[at + 2],
            self.out[at + 3],
        ])
    }

    /// The decode ended: judge the frame, publish or fail, then take whatever
    /// capture was latched while it ran.
    fn finish(&mut self, o: Outcome) {
        let Some(req) = self.pipe.decoding_req() else {
            return;
        };
        let stage = match o {
            Outcome::Complete => {
                if self.out[3] != commands::DATA_TRANSFER || self.out[0] != FRAME_WORDS {
                    Some(Stage::BadFrame)
                } else if self.word(0) != functions::STORAGE || self.word(1) != u32::from(req.block)
                {
                    Some(Stage::BadLocation)
                } else if self.decoder.xor() != 0 {
                    Some(Stage::Crc)
                } else {
                    None
                }
            }
            Outcome::TooLong => Some(Stage::BadFrame),
            Outcome::Ended | Outcome::Exhausted => Some(Stage::Truncated),
            Outcome::NoStart => Some(Stage::NoStart),
        };
        match stage {
            None => {
                self.ready_data
                    .copy_from_slice(&self.out[DATA_AT..DATA_AT + BLOCK_BYTES]);
                // The one conversion out of wire order, and the reason it is
                // here rather than at the consumer: a block leaves this file
                // in the order the protocol carries, so no later path can
                // forget. 128 word swaps against a ~13 ms decode.
                wire_to_image(&mut self.ready_data);
                self.pipe.note_decoded(req);
            }
            Some(s) => self.pipe.fail(req, s),
        }
        // Latency only: a capture that came in while this one decoded starts
        // decoding now rather than at the next window head.
        self.service();
    }
}
