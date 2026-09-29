// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! The host-facing VMU storage service: who is owed what, and what goes out
//! next (protocol v1).
//!
//! `…0004` in, `…0003` out. This is the whole state machine and none of the
//! plumbing: no statics, no mutex, no radio. The firmware's `ble::host_vmu`
//! owns one of these behind a `ThreadModeRawMutex`, hands it the writes the
//! GATT dispatch sees, hands it the blocks the poll loop decodes and the
//! outcome of the blocks it writes, and notifies whatever
//! [`HostIo::next_message`] offers.
//!
//! # Why it is here rather than in the firmware
//!
//! `read_pipeline`'s reason, and this module is the proof of it: the firmware
//! crate is `no_std` on-target and its tests do not run, so a state machine
//! left in it is a state machine nothing exercises. The first version of this
//! one lived there, and a case was found only by **extracting the source into
//! a harness by hand** — a generation change between a block being published
//! and its four phases going out deleted the reply and left the dongle waiting
//! for a fourth DATA that no longer existed (2026-09-18). That case is
//! `a_generation_change_mid_reply_still_answers` below.
//!
//! # The shape
//!
//! ```text
//!   dongle ──01 READ──▶ accept_write ──▶ take_request ──▶ BlockReader
//!                                                             │
//!   dongle ◀──81 DATA×4── next_message ◀── publish_block ◀─────┘
//!
//!   dongle ──02 WRITE×4─▶ accept_write ──▶ [queue] ──▶ take_write ──▶ writer
//!   dongle ◀──82 ACK────── next_message ◀────┘             │
//!   dongle ◀──84 WRITTEN── next_message ◀── publish_written ◀┘
//! ```
//!
//! **One outstanding read.** Protocol v1 allows the dongle one at a time: the
//! next waits for the fourth `81 DATA`, or for one with `result ≠ 0`. That is
//! enforced in [`HostIo::accept_write`] rather than trusted.
//!
//! It is unrelated to `read_pipeline`'s two-deep pipeline, which is an internal
//! overlap of one block's capture with the previous one's decode. A single
//! outstanding request never fills it, so nobody should read a throughput
//! number here as if it were the diagnostic's prefilled queue.
//!
//! **Reads are idle-only.** [`HostIo::take_request`] hands out nothing while
//! the pad is in use, and [`HostIo::recall`] takes an already-issued request
//! *back* when input resumes, so reads stop and the request survives. A capture
//! already committed to the bus is not recalled — it is microseconds from done.
//!
//! **Writes are staged, acked, then drained.** Four `02 WRITE` phases assemble
//! a block; on the fourth it is staged in the queue and `82 ACK`ed. The poll
//! loop takes staged blocks in the order they were first staged
//! ([`HostIo::take_write`]) and reports each one's fate
//! ([`HostIo::publish_written`], [`HostIo::publish_write_failed`]), which
//! becomes the block's one `84 WRITTEN` — the only message that means "on the
//! card". The protocol's write rules — duplicates re-acked,
//! a newer `seq` replacing a staged block in place, `FULL` past the room
//! reported in STATUS — are all in `commit`.
//!
//! **Every ended generation leaves a terminal answer.** A dock, an undock or a
//! forced shutdown ends one, and whatever the dongle was waiting for becomes
//! `DISCARDED` rather than silence: the outstanding read, and every staged
//! block that had not reached the card. See [`HostIo::end_generation`].
//!
//! **A link drop is not a generation change.** [`HostIo::link_down`] drops
//! only what belonged to the host that left — the read it asked for, the
//! ACKs it had not received, a block half-assembled — and keeps `epoch`,
//! `card`, the queue and every WRITTEN owed. A session start
//! ([`HostIo::reset`]) does not reseed. Whether the *port* was watched through
//! the gap is the firmware's knowledge, not this module's: if the rail went
//! down or the unit slept, it says so with [`HostIo::set_vmu_present`] (false),
//! and unobserved counts as changed.
//!
//! # Writes are open only when the firmware says so, and the card is known
//!
//! Until the firmware calls [`HostIo::open_writes`], `83 STATUS` reports
//! `queue_free` 0 — the protocol's own way of telling the dongle to send
//! nothing — and a WRITE that arrives regardless is answered `ACK FULL` rather
//! than left waiting. The firmware opens them at boot wherever its writer is
//! compiled and nowhere else: a queue with nothing to drain it is a queue of
//! saves that will be `DISCARDED` at the next generation change.
//!
//! `queue_free` is 0 for a second reason: **until `card` is known**. The
//! guard (`write_seq`) compares the docked card against
//! `card` before it writes, and `card` comes from the dongle's own pull of
//! blocks 255 and 254 — the protocol has it pull both before it exposes the
//! card, so a dongle that follows it never sees the 0. One that does not is
//! told `FULL`, which is exactly true.
//!
//! # The card's identity
//!
//! `card` is the fingerprint of what is *on the card now*, as far as this
//! side knows: FNV-1a over block 255 then 254 in image order, taken from the
//! pull and advanced whenever a write to either block is confirmed on the
//! card ([`HostIo::publish_written`]). The FAT's bytes are kept for that,
//! because the root can be rewritten — a format — and the hash cannot be
//! moved without the other half. A queued write advances nothing: the
//! guard compares against what the card holds, and a block is on the card
//! only when its read-back says so.
//!
//! Beside it, [`HostIo::card_verified`]: whether the guard has shown the
//! docked card to be the one `card` describes, and nothing has armed a
//! different one since. A guard pass sets it; an enumeration the firmware
//! sends outside the guard ([`HostIo::note_enumerated`]) or the end of the
//! generation clears it. The writer runs the guard before phase 0 whenever
//! it is clear, so a drain is guarded once, not once per block.
//!
//! `flags.enabled` reads 1. The save-transfer setting it describes is a
//! configuration-link item that does not exist yet; reporting 0 would be
//! accurate about the setting and would tell the dongle to expect nothing from
//! a build whose whole purpose is serving reads.

use crate::block_bytes::{fnv1a32, BLOCK_BYTES, FNV_OFFSET_BASIS, PHASE_BYTES};
use crate::write_seq::{phase_hashes, Expected, Landing, FAT_BLOCK, ROOT_BLOCK};

/// Op codes on `…0004`, the down characteristic.
pub mod down {
    pub const READ: u8 = 0x01;
    pub const WRITE: u8 = 0x02;
    pub const STATUS_Q: u8 = 0x03;
}

/// Op codes on `…0003`, the up characteristic.
pub mod up {
    pub const DATA: u8 = 0x81;
    pub const ACK: u8 = 0x82;
    pub const STATUS: u8 = 0x83;
    pub const WRITTEN: u8 = 0x84;
}

/// `result` bytes, protocol v1. `DISABLED` belongs to the save-transfer
/// setting, which does not exist yet, and is never produced here.
pub mod result {
    pub const OK: u8 = 0;
    /// ACK: `queue_free` was 0.
    pub const FULL: u8 = 1;
    pub const NO_VMU: u8 = 2;
    pub const DISABLED: u8 = 3;
    /// DATA: the capture failed every attempt it was allowed. WRITTEN: the
    /// card refused the block — and the generation ends.
    pub const FAILED: u8 = 4;
    /// The generation ended under it — an undock, or a forced shutdown.
    pub const DISCARDED: u8 = 5;
    /// WRITTEN: a newer `seq` for the same block replaced it before it
    /// drained. Nothing was written and nothing was lost.
    pub const SUPERSEDED: u8 = 6;
    /// ACK: a phase out of order or out of range, or `seq` changed mid-block.
    pub const BAD: u8 = 7;
}

/// `flags` bits of `83 STATUS`.
pub mod flags {
    pub const VMU_PRESENT: u8 = 1 << 0;
    pub const ENABLED: u8 = 1 << 1;
    /// Acked blocks not yet on the card.
    pub const DRAINING: u8 = 1 << 2;
    pub const IDLE: u8 = 1 << 3;
}

/// The protocol version `83 STATUS` reports.
pub const PROTO_VERSION: u8 = 1;

/// Header bytes before a phase payload, both directions.
const HEADER_LEN: usize = 4;

/// Longest message on either characteristic: a header and one 128-byte phase.
///
/// Deliberately not the MTU and deliberately not 512 — a GATTS event has to fit
/// `evt-max-size-256`, and it is the *registered maximum* that bounds one, not
/// `att_mtu`. Sizing a characteristic to the MTU is what panicked an earlier revision.
pub const MSG_MAX: usize = HEADER_LEN + PHASE_BYTES;
const _: () = assert!(MSG_MAX == 132);

/// Length of a `83 STATUS` message.
pub const STATUS_LEN: usize = 4 + 4 + 4;

/// Length of an `82 ACK` or `84 WRITTEN`: `[op, blk, seq, result]`.
pub const SHORT_LEN: usize = 4;

/// Phases in one block's `81 DATA` reply, and in one block's `02 WRITE`.
pub const PHASES: u8 = 4;
const _: () = assert!(PHASES as usize * PHASE_BYTES == BLOCK_BYTES);

/// Blocks the staging queue holds.
///
/// Derived from the other end, not asserted: the console-side sender stages
/// one block at a time and keeps at most 8 acked-unwritten, so room past 8 is
/// room it can never use. Eight entries are ≈ 4.2 KB of `.bss` — measured by
/// `an_entry_is_the_block_and_twelve_bytes` below — against the tens of
/// kilobytes of RAM left beside `SAMPLE_BUFFER`.
pub const QUEUE_DEPTH: usize = 8;
const _: () = assert!(QUEUE_DEPTH <= u8::MAX as usize);

/// Short messages waiting to go out: ACKs and the SUPERSEDED that a coalesce
/// owes. A WRITTEN for a staged block is *not* held here — it lives in the
/// block's own slot until it is delivered, so a full outbox can never lose
/// one. What this bounds is the dongle's own in-flight rule (one block staged
/// at a time, so two short messages at most); past it a WRITE is dropped
/// unanswered and the dongle's re-write timer sends it again.
const OUTBOX_LEN: usize = 8;

/// Counters, for the bench and for whatever reads them next.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// READs taken and answered, or being answered.
    pub reads_accepted: u32,
    /// READs dropped because one was already outstanding — a dongle breaking
    /// the one-outstanding rule, which is worth seeing rather than absorbing.
    pub reads_refused: u32,
    /// Blocks whose phases a generation change abandoned.
    pub replies_discarded: u32,
    /// Blocks staged and acked, coalesced replacements included.
    pub writes_staged: u32,
    /// WRITEs answered with anything but `OK`: `FULL`, `NO_VMU`, `BAD`.
    pub writes_refused: u32,
    /// Staged blocks a newer `seq` replaced before they drained.
    pub writes_superseded: u32,
    /// Staged blocks a generation change discarded before they drained.
    pub writes_discarded: u32,
    /// Blocks the writer confirmed on the card.
    pub blocks_written: u32,
    /// Blocks the card refused. Each one ended a generation.
    pub write_failures: u32,
    /// Blocks the guard would not write because the card in the port was
    /// not the one they were staged for, or could not be identified. Each
    /// one ended a generation.
    pub writes_unidentified: u32,
}

impl Stats {
    const ZERO: Self = Self {
        reads_accepted: 0,
        reads_refused: 0,
        replies_discarded: 0,
        writes_staged: 0,
        writes_refused: 0,
        writes_superseded: 0,
        writes_discarded: 0,
        blocks_written: 0,
        write_failures: 0,
        writes_unidentified: 0,
    };
}

/// A READ the host is owed an answer to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pending {
    block: u8,
    /// The generation it was accepted under. An answer produced under another
    /// one is about a different card and is never sent.
    epoch: u32,
    /// Handed to the block reader (true), or waiting for the pad to go idle.
    issued: bool,
}

/// The reply being notified out.
struct Reply {
    block: u8,
    result: u8,
    data: [u8; BLOCK_BYTES],
    /// Next phase to send; `phases` when done.
    next: u8,
    /// Four for a block, one for an error — an error is a single message with
    /// no data.
    phases: u8,
}

impl Reply {
    /// A single-message answer with no data.
    const fn terminal(block: u8, result: u8) -> Self {
        Self {
            block,
            result,
            data: [0; BLOCK_BYTES],
            next: 0,
            phases: 1,
        }
    }
}

/// Where a queue slot is in a block's life.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Free,
    /// Acked, waiting for the writer.
    Staged,
    /// In the writer's hands.
    Writing,
    /// Its WRITTEN, with this result, waiting to be delivered. The slot is
    /// held until it is: a WRITTEN is owed exactly once, ever, and the link
    /// may be down when it becomes due.
    Done(u8),
}

/// One staged block.
#[derive(Clone, Copy)]
struct Entry {
    slot: Slot,
    block: u8,
    seq: u8,
    /// The generation it was accepted under. Every live entry is of the
    /// current one — `end_generation` sees to that — so this is the check
    /// that the invariant held, not the mechanism that holds it.
    epoch: u32,
    /// Order of first staging, which is drain order and WRITTEN order.
    stamp: u32,
    data: [u8; BLOCK_BYTES],
}

impl Entry {
    const FREE: Self = Self {
        slot: Slot::Free,
        block: 0,
        seq: 0,
        epoch: 0,
        stamp: 0,
        data: [0; BLOCK_BYTES],
    };
}

/// A block being assembled from its four phases.
struct Inbox {
    block: u8,
    seq: u8,
    /// Phases seen so far, in order; `PHASES` completes it.
    next: u8,
    data: [u8; BLOCK_BYTES],
}

/// A short up message: `[op, blk, seq, result]`.
type Short = [u8; SHORT_LEN];

/// The service's whole state.
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent facts about the link and the port; an enum for each would be four enums"
)]
pub struct HostIo {
    outstanding: Option<Pending>,
    reply: Option<Reply>,
    status_due: bool,
    /// `(flags, queue_free)` as last notified. STATUS is due whenever the
    /// current pair differs: protocol v1 says "on any change", and the dongle
    /// paces its writes on `queue_free` from the last STATUS it saw.
    last_status: Option<(u8, u8)>,
    idle: bool,
    vmu_present: bool,
    /// Moves on any dock or undock, and on the seed at the first session of a
    /// power cycle. A `u32` because `83 STATUS` carries it as one.
    epoch: u32,
    /// The fingerprint of the docked card as it stands, or 0 until both of
    /// its blocks have been seen this generation — the protocol's "not read
    /// yet". Always `fnv1a32(root_seed, fat)` when both are known: see
    /// `recompute_card`.
    card: u32,
    /// FNV-1a state after block 255 — the root's half of the fingerprint.
    /// The root's bytes are not kept: only its per-phase hashes, which the
    /// guard needs to know a format torn mid-write (`write_seq::Landing`).
    root_seed: Option<u32>,
    /// `write_seq::phase_hashes` of block 255, set with `root_seed`.
    root_phases: Option<[u32; PHASES as usize]>,
    /// Block 254 as the card holds it, in image order. Kept whole because the
    /// fingerprint hashes it *after* the root, and a root rewritten by a
    /// format has to be hashed with the FAT that is still there.
    fat: Option<[u8; BLOCK_BYTES]>,
    /// The guard has shown the docked card to be the one `card` describes,
    /// and the port has not been enumerated outside the guard since.
    verified: bool,
    /// The firmware has a writer to drain the queue. Until then `queue_free`
    /// reads 0 and every WRITE is `FULL`.
    writes_open: bool,
    /// Admission closed for a gesture that ends the session — pairing or the
    /// update reboot — taken in the same step as its drain check, so no save
    /// can be acked between the check and the drop. Lifted by the next
    /// session's [`Self::reset`].
    writes_paused: bool,
    inbox: Option<Inbox>,
    queue: [Entry; QUEUE_DEPTH],
    /// Next `Entry::stamp`.
    stamp: u32,
    outbox: heapless::Vec<Short, OUTBOX_LEN>,
    stats: Stats,
}

impl Default for HostIo {
    fn default() -> Self {
        Self::new()
    }
}

impl HostIo {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            outstanding: None,
            reply: None,
            status_due: false,
            last_status: None,
            idle: false,
            vmu_present: false,
            epoch: 0,
            card: 0,
            root_seed: None,
            root_phases: None,
            fat: None,
            verified: false,
            writes_open: false,
            writes_paused: false,
            inbox: None,
            queue: [Entry::FREE; QUEUE_DEPTH],
            stamp: 0,
            outbox: heapless::Vec::new(),
            stats: Stats::ZERO,
        }
    }

    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// The firmware has a writer: report the queue's room and take writes.
    ///
    /// Called once, at boot, by a build that drains the queue; never by one
    /// that cannot. See the module note.
    pub const fn open_writes(&mut self) {
        self.writes_open = true;
    }

    /// Close write admission unless acked saves are still draining.
    ///
    /// `false` means saves are draining and nothing changed: refuse the
    /// gesture. `true` means admission is closed — every WRITE is `FULL` and
    /// `queue_free` reads 0 — until the next session's [`Self::reset`]. One
    /// call, so a save cannot be acked between the check and the close.
    pub fn pause_writes_unless_draining(&mut self) -> bool {
        if self.draining() {
            return false;
        }
        self.writes_paused = true;
        true
    }

    // ------------------------------------------------------- from the host --

    /// Take one write on the down characteristic (`…0004`).
    ///
    /// Anything malformed is dropped in silence, as on the LCD characteristic:
    /// this is an open vendor endpoint on a device that must keep playing
    /// whatever a host writes at it. A WRITE phase that is the right length
    /// but the wrong phase is the one exception — it is answered `ACK BAD`,
    /// because the dongle is waiting on that block and silence would cost it
    /// a timeout.
    pub fn accept_write(&mut self, data: &[u8]) {
        match (data.first().copied(), data.len(), data.get(1).copied()) {
            (Some(down::READ), 2, Some(block)) => self.accept_read(block),
            (Some(down::STATUS_Q), 1, _) => self.status_due = true,
            (Some(down::WRITE), MSG_MAX, Some(block)) => {
                let Ok(payload) = <&[u8; PHASE_BYTES]>::try_from(&data[HEADER_LEN..]) else {
                    return;
                };
                self.accept_phase(block, data[2], data[3], payload);
            }
            _ => {}
        }
    }

    const fn accept_read(&mut self, block: u8) {
        // One outstanding, and a reply still draining counts: the fourth DATA
        // has not landed yet, so the dongle has not been answered.
        if self.outstanding.is_some() || self.reply.is_some() {
            self.stats.reads_refused = self.stats.reads_refused.saturating_add(1);
            return;
        }
        self.stats.reads_accepted = self.stats.reads_accepted.saturating_add(1);
        if self.vmu_present {
            self.outstanding = Some(Pending {
                block,
                epoch: self.epoch,
                issued: false,
            });
        } else {
            self.reply = Some(Reply::terminal(block, result::NO_VMU));
        }
    }

    /// One `02 WRITE` phase. Phase 0 opens a block; 1–3 must follow in order
    /// under the same `seq`, a repeat of a phase already seen overwriting it.
    /// The fourth commits.
    fn accept_phase(&mut self, block: u8, phase: u8, seq: u8, payload: &[u8; PHASE_BYTES]) {
        if phase >= PHASES {
            self.inbox = None;
            self.refuse(block, seq, result::BAD);
            return;
        }
        if phase == 0 {
            self.inbox = Some(Inbox {
                block,
                seq,
                next: 0,
                data: [0; BLOCK_BYTES],
            });
        }
        let Some(inbox) = self.inbox.as_mut() else {
            self.refuse(block, seq, result::BAD);
            return;
        };
        if inbox.block != block || inbox.seq != seq || phase > inbox.next {
            self.inbox = None;
            self.refuse(block, seq, result::BAD);
            return;
        }
        let at = usize::from(phase) * PHASE_BYTES;
        inbox.data[at..at + PHASE_BYTES].copy_from_slice(payload);
        if phase == inbox.next {
            inbox.next = phase.saturating_add(1);
        }
        if inbox.next == PHASES {
            if let Some(done) = self.inbox.take() {
                self.commit(&done);
            }
        }
    }

    /// A block's four phases are in. Stage it, or say why not.
    ///
    /// The order of the rules is the protocol's precedence: no card
    /// beats no writer beats no room. The duplicate and coalescing rules come
    /// before room because neither needs any — a resend after a lost ACK is
    /// re-acked from the slot it already has, and a newer `seq` takes the
    /// slot of the older one.
    fn commit(&mut self, inbox: &Inbox) {
        let (block, seq) = (inbox.block, inbox.seq);
        if !self.vmu_present {
            self.refuse(block, seq, result::NO_VMU);
            return;
        }
        if !self.writes_open || self.writes_paused || self.card == 0 {
            // `queue_free` is 0 in both cases, so FULL is the true answer.
            self.refuse(block, seq, result::FULL);
            return;
        }
        if self.queue.iter().any(|e| {
            e.slot != Slot::Free && e.epoch == self.epoch && e.block == block && e.seq == seq
        }) {
            // A resend after a lost ACK: ACK again, do not restage. Only
            // within the generation: a `Done` slot held over from an earlier
            // one wrote to a card that may not be this one. Today the card
            // gate above and WRITTEN-before-DATA ordering keep such a slot
            // from ever being matched; the epoch makes that not depend on them.
            self.send_short([up::ACK, block, seq, result::OK]);
            return;
        }
        if let Some(i) = self
            .queue
            .iter()
            .position(|e| e.slot == Slot::Staged && e.block == block)
        {
            // Coalesce in place: the block keeps its place in the drain
            // order, the older `seq` is told nothing was written under it.
            // Both messages or neither, so a full outbox drops the WRITE
            // rather than half-answering it.
            if self.outbox.len() + 2 > OUTBOX_LEN {
                return;
            }
            let old = self.queue[i].seq;
            self.queue[i].seq = seq;
            self.queue[i].data = inbox.data;
            self.send_short([up::WRITTEN, block, old, result::SUPERSEDED]);
            self.send_short([up::ACK, block, seq, result::OK]);
            self.stats.writes_superseded = self.stats.writes_superseded.saturating_add(1);
            self.stats.writes_staged = self.stats.writes_staged.saturating_add(1);
            return;
        }
        let Some(i) = self.queue.iter().position(|e| e.slot == Slot::Free) else {
            self.refuse(block, seq, result::FULL);
            return;
        };
        if self.outbox.is_full() {
            return;
        }
        self.queue[i] = Entry {
            slot: Slot::Staged,
            block,
            seq,
            epoch: self.epoch,
            stamp: self.stamp,
            data: inbox.data,
        };
        self.stamp = self.stamp.wrapping_add(1);
        self.send_short([up::ACK, block, seq, result::OK]);
        self.stats.writes_staged = self.stats.writes_staged.saturating_add(1);
    }

    fn refuse(&mut self, block: u8, seq: u8, code: u8) {
        self.stats.writes_refused = self.stats.writes_refused.saturating_add(1);
        self.send_short([up::ACK, block, seq, code]);
    }

    /// Queue a short message. A full outbox drops it: see `OUTBOX_LEN` for
    /// why that is never a WRITTEN owed for a staged block.
    fn send_short(&mut self, msg: Short) {
        let _ = self.outbox.push(msg);
    }

    /// The host subscribed to `…0003`. Protocol v1: STATUS on subscribe.
    pub const fn note_subscribed(&mut self) {
        self.status_due = true;
    }

    // --------------------------------------------------------- to the host --

    /// The next message to notify, written into `buf`, or `None` when nothing
    /// is waiting. Takes `&self`: deciding what to send changes nothing.
    ///
    /// STATUS goes first when anything else is waiting: it is one message,
    /// the dongle may be blocked on it at discovery, and everything else
    /// waits on it after a reconnect. Then the short messages — an ACK is
    /// what lets the dongle stage its next block — then WRITTENs in drain
    /// order, then a block's four phases, which are never urgent: the pad is
    /// idle by definition while one is being served.
    #[must_use]
    pub fn next_message(&self, buf: &mut [u8; MSG_MAX]) -> Option<usize> {
        if self.status_pending() {
            return Some(self.build_status(buf));
        }
        if let Some(msg) = self.outbox.first() {
            buf[..SHORT_LEN].copy_from_slice(msg);
            return Some(SHORT_LEN);
        }
        if let Some(i) = self.owed_written() {
            let e = &self.queue[i];
            let Slot::Done(code) = e.slot else {
                return None;
            };
            buf[..SHORT_LEN].copy_from_slice(&[up::WRITTEN, e.block, e.seq, code]);
            return Some(SHORT_LEN);
        }
        let reply = self.reply.as_ref()?;
        Some(build_data(reply, reply.next, buf))
    }

    /// The message [`Self::next_message`] offered was notified and accepted.
    ///
    /// **The backpressure rule lives in the split:** a caller that could not
    /// send does not call this, so nothing advances. A notification queue that
    /// is momentarily full is the normal case under load, and a phase dropped
    /// there would strand the dongle waiting for a fourth DATA that never
    /// comes.
    pub fn advance(&mut self) {
        if self.status_pending() {
            self.status_due = false;
            self.last_status = Some(self.snapshot());
            return;
        }
        if !self.outbox.is_empty() {
            self.outbox.remove(0);
            return;
        }
        if let Some(i) = self.owed_written() {
            // Delivered: the slot's last duty is done.
            self.queue[i].slot = Slot::Free;
            return;
        }
        let Some(reply) = self.reply.as_mut() else {
            return;
        };
        reply.next = reply.next.saturating_add(1);
        if reply.next >= reply.phases {
            self.reply = None;
        }
    }

    fn status_pending(&self) -> bool {
        self.status_due || self.last_status != Some(self.snapshot())
    }

    /// What STATUS carries beyond `epoch` and `card`, which move `status_due`
    /// themselves.
    fn snapshot(&self) -> (u8, u8) {
        (self.flags(), self.queue_free())
    }

    /// `[83, proto, flags, queue_free, epoch u32 LE, card u32 LE]`.
    fn build_status(&self, buf: &mut [u8; MSG_MAX]) -> usize {
        buf[0] = up::STATUS;
        buf[1] = PROTO_VERSION;
        buf[2] = self.flags();
        buf[3] = self.queue_free();
        buf[4..8].copy_from_slice(&self.epoch.to_le_bytes());
        buf[8..12].copy_from_slice(&self.card.to_le_bytes());
        STATUS_LEN
    }

    fn flags(&self) -> u8 {
        let mut f = flags::ENABLED;
        if self.vmu_present {
            f |= flags::VMU_PRESENT;
        }
        if self.idle {
            f |= flags::IDLE;
        }
        if self.draining() {
            f |= flags::DRAINING;
        }
        f
    }

    /// Room in the queue, in blocks. 0 until the firmware has a writer, and
    /// 0 until the card's fingerprint is known: the dongle sends nothing
    /// against 0, with no version check anywhere.
    fn queue_free(&self) -> u8 {
        if !self.writes_open || self.writes_paused || self.card == 0 {
            return 0;
        }
        let free = self.queue.iter().filter(|e| e.slot == Slot::Free).count();
        u8::try_from(free).unwrap_or(u8::MAX)
    }

    /// The earliest-staged slot whose WRITTEN is waiting to go out.
    fn owed_written(&self) -> Option<usize> {
        self.queue
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.slot, Slot::Done(_)))
            .min_by_key(|(_, e)| e.stamp)
            .map(|(i, _)| i)
    }

    // ------------------------------------------------------ from the loop --

    /// Drop what belonged to the host that left, at the *start* of a session.
    ///
    /// The static this lives in outlives the connection that filled it, and a
    /// read owed to a host that is gone must not be answered to the next one.
    /// The generation and the queue are not the host's — they are the card's —
    /// and survive: see [`Self::link_down`], which this is, plus the seed.
    ///
    /// # The seed, and the bug it avoids
    ///
    /// The *first* session of a power cycle starts its `epoch` at `seed` — the
    /// caller's tick count — instead of at 1. Counting from 1 every boot would
    /// make the epoch after a reboot equal one the dongle had already seen, and
    /// protocol v1 reads an unchanged `epoch` as "nothing was lost, the
    /// WRITTENs are still coming": precisely wrong after a reboot that lost the
    /// queue. A tick count differs between boots because a session never starts
    /// at the same moment twice.
    ///
    /// The real fix, when writes land, is a counter that survives power.
    pub fn reset(&mut self, seed: u32) {
        self.writes_paused = false;
        if self.epoch == 0 {
            self.epoch = if seed == 0 { 1 } else { seed };
        }
        self.link_down();
    }

    /// The link went away. Everything owed to that host goes with it; nothing
    /// about the card does.
    ///
    /// Dropped: the outstanding read and a reply half-sent (a DATA for a block
    /// nobody asked for, to the next host), a block half-assembled (the dongle
    /// takes it back on its own `link_down` and sends it again under a fresh
    /// `seq`), and the ACKs not yet delivered (the block they acknowledged is
    /// staged, and its WRITTEN will say so). Kept: `epoch`, `card`, every
    /// staged block, and every WRITTEN owed — including the `SUPERSEDED`s in
    /// the outbox, which are WRITTENs too.
    ///
    /// The firmware calls this at the moment of the drop, whether or not it
    /// then keeps the rail up to drain; the [`Self::reset`] at the next
    /// connection repeats it harmlessly.
    pub fn link_down(&mut self) {
        self.outstanding = None;
        self.reply = None;
        self.inbox = None;
        self.status_due = false;
        self.outbox.retain(|m| m[0] == up::WRITTEN);
    }

    /// Tell the service whether the pad is being used.
    pub const fn set_idle(&mut self, idle: bool) {
        if self.idle != idle {
            self.idle = idle;
            // `flags.idle` is part of STATUS and the dongle paces on it.
            self.status_due = true;
        }
    }

    /// Tell the service whether a VMU is docked. A change ends the generation:
    /// a card that has been out of the port is not the card that was pulled.
    ///
    /// "Not watched" is "not docked" for this purpose: the firmware says
    /// `false` when the rail goes down or the unit sleeps, because a port it
    /// was not looking at may have been emptied.
    pub fn set_vmu_present(&mut self, present: bool) {
        if self.vmu_present != present {
            self.vmu_present = present;
            self.end_generation();
        }
    }

    /// The card these answers were about is gone.
    ///
    /// # Every ended generation leaves a terminal answer behind
    ///
    /// A READ is outstanding in one of two places, never both — waiting to be
    /// issued (`outstanding`), or half-notified as a `reply` whose phases have
    /// not all gone out. `accept_write` refuses a new READ while either holds
    /// something, so the dongle is waiting on whichever it is.
    ///
    /// Both have to become `DISCARDED`. Taking the block only from
    /// `outstanding` left the other case with no remaining phases *and* no
    /// terminal message, so the dongle waited for a fourth DATA that had been
    /// deleted (2026-09-18). A generation change during the four
    /// notifications is not exotic: that is exactly when a card is pulled out.
    ///
    /// A `DISCARDED` already queued is re-made rather than dropped, so a second
    /// generation change before it is sent does not erase it either.
    ///
    /// # And every staged block becomes a `WRITTEN DISCARDED`
    ///
    /// Staged or in the writer's hands, a block that had not been confirmed on
    /// the card is not going to be: it was bound to this generation, and
    /// draining it onto whatever is docked next is the corruption the swap
    /// guard exists to prevent. Its slot holds the `DISCARDED` until it is
    /// delivered, which may be to the next session. A block already `Done`
    /// keeps its result — it did reach the card, or it did not, and neither
    /// changes with the card. The half-assembled block goes in silence: no
    /// ACK means the dongle sends it again, and the STATUS that follows tells
    /// it under which generation.
    pub fn end_generation(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.card = 0;
        self.root_seed = None;
        self.root_phases = None;
        self.fat = None;
        self.verified = false;
        self.status_due = true;
        self.inbox = None;

        let owed = match (self.outstanding.take(), &self.reply) {
            (Some(p), _) => Some(p.block),
            (None, Some(r)) => {
                if r.result == result::OK {
                    // A block's phases, abandoned. An error reply carried
                    // forward is not counted again.
                    self.stats.replies_discarded = self.stats.replies_discarded.saturating_add(1);
                }
                Some(r.block)
            }
            (None, None) => None,
        };
        self.reply = owed.map(|block| Reply::terminal(block, result::DISCARDED));

        for e in &mut self.queue {
            if matches!(e.slot, Slot::Staged | Slot::Writing) {
                e.slot = Slot::Done(result::DISCARDED);
                self.stats.writes_discarded = self.stats.writes_discarded.saturating_add(1);
            }
        }
    }

    /// The block to issue now, if there is one and the pad is idle.
    ///
    /// `None` while the pad is in use — that is the idle-only rule — and once a
    /// request has been issued, so a caller asking every window does not queue
    /// it twice.
    pub const fn take_request(&mut self) -> Option<u8> {
        if !self.idle || !self.vmu_present {
            return None;
        }
        let Some(p) = self.outstanding.as_mut() else {
            return None;
        };
        if p.issued {
            return None;
        }
        p.issued = true;
        Some(p.block)
    }

    /// Input resumed with a request in the reader's hands: take it back so the
    /// caller can clear the pipeline, and answer it when the pad is idle again.
    ///
    /// Returns whether anything was recalled. The request itself is kept — the
    /// dongle asked once and is owed one answer, and dropping it here would
    /// make every stick twitch cost the host a read it has to notice timing
    /// out.
    pub const fn recall(&mut self) -> bool {
        if self.idle {
            return false;
        }
        let Some(p) = self.outstanding.as_mut() else {
            return false;
        };
        if !p.issued {
            return false;
        }
        p.issued = false;
        true
    }

    /// A block came back. Queues its four `81 DATA` phases.
    ///
    /// Silently drops a block that is not the outstanding request, or one
    /// produced under a generation that has ended: either way it is an answer
    /// to a question nobody is waiting for.
    pub fn publish_block(&mut self, block: u8, data: &[u8; BLOCK_BYTES]) {
        let Some(p) = self.outstanding else {
            return;
        };
        if p.block != block || p.epoch != self.epoch {
            return;
        }
        self.outstanding = None;

        // The fingerprint, taken as the pull passes. Both halves are in
        // image order, as `card` is specified; whichever arrives second
        // completes it.
        match block {
            ROOT_BLOCK => self.set_root(data),
            FAT_BLOCK => self.fat = Some(*data),
            _ => {}
        }
        self.recompute_card();

        self.reply = Some(Reply {
            block,
            result: result::OK,
            data: *data,
            next: 0,
            phases: PHASES,
        });
    }

    /// A block failed every attempt. Answers the READ with a single
    /// `81 DATA … FAILED`, which is what ends the dongle's wait.
    pub const fn publish_failure(&mut self, block: u8) {
        self.answer_error(block, result::FAILED);
    }

    /// Queue an error answer to the outstanding READ, if it is still the one
    /// being asked about.
    const fn answer_error(&mut self, block: u8, code: u8) {
        let Some(p) = self.outstanding else {
            return;
        };
        if p.block != block {
            return;
        }
        self.outstanding = None;
        self.reply = Some(Reply::terminal(block, code));
    }

    // ------------------------------------------------------- the write drain --

    /// Acked blocks not yet on the card — the storage lease. While this is
    /// true the firmware keeps the rail up through a link drop, defers sleep
    /// and refuses the update gesture; a WRITTEN waiting to be delivered does
    /// not count, because nothing physical is left to do for it.
    #[must_use]
    pub fn draining(&self) -> bool {
        self.queue
            .iter()
            .any(|e| matches!(e.slot, Slot::Staged | Slot::Writing))
    }

    /// The next block for the writer, in the order the blocks were first
    /// staged, copied into `out`. `None` while one is already in the writer's
    /// hands: one physical write at a time, five bus transactions each.
    ///
    /// Not gated on the pad being idle, unlike reads. A save the console has
    /// already committed must land whether or not the player is still at the
    /// menu; how much of the poll's cadence the drain may take during play is
    /// the writer's pacing to decide, and it asks here only when it has a
    /// window to spend.
    pub fn take_write(&mut self, out: &mut [u8; BLOCK_BYTES]) -> Option<u8> {
        if !self.vmu_present || self.queue.iter().any(|e| e.slot == Slot::Writing) {
            return None;
        }
        let i = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, e)| e.slot == Slot::Staged && e.epoch == self.epoch)
            .min_by_key(|(_, e)| e.stamp)
            .map(|(i, _)| i)?;
        let e = &mut self.queue[i];
        e.slot = Slot::Writing;
        *out = e.data;
        Some(e.block)
    }

    /// The block in the writer's hands under the current generation, if any.
    ///
    /// The writer asks this at every window head and abandons a block that
    /// is no longer here: the generation ended under it — an undock, the
    /// absence streak, a refused write elsewhere — and the slot already says
    /// `DISCARDED`. Writing on regardless would put a stale block onto
    /// whatever card is docked now, which is the swap the generation exists
    /// to prevent.
    #[must_use]
    pub fn writing(&self) -> Option<u8> {
        self.queue
            .iter()
            .find(|e| e.slot == Slot::Writing && e.epoch == self.epoch)
            .map(|e| e.block)
    }

    /// The writer confirmed the block on the card. Its `WRITTEN OK` is now
    /// owed, and is delivered whenever the link next allows.
    ///
    /// Silently ignores a block that is not in the writer's hands under the
    /// current generation: the generation ended under it, and the slot already
    /// holds the `DISCARDED` that says so.
    pub fn publish_written(&mut self, block: u8) {
        if self.finish_write(block, result::OK) {
            self.stats.blocks_written = self.stats.blocks_written.saturating_add(1);
        }
    }

    /// The card refused the block within the writer's budget. Protocol v1:
    /// its WRITTEN says `FAILED`, and **the generation ends** — a card that
    /// refuses writes is not one to keep draining onto, and every other staged
    /// block becomes `DISCARDED` for the dongle to re-push or park.
    pub fn publish_write_failed(&mut self, block: u8) {
        if self.finish_write(block, result::FAILED) {
            self.stats.write_failures = self.stats.write_failures.saturating_add(1);
            self.end_generation();
        }
    }

    /// The guard could not show the block's card to be the one `card`
    /// describes — another card, or no answer within its budget. Nothing was
    /// written after the guard ran. The generation ends: the block and
    /// everything staged behind it become `DISCARDED`, `card` goes to 0, and
    /// the dongle re-pulls whatever is in the port before exposing it.
    ///
    /// Silently ignores a block that is not in the writer's hands under the
    /// current generation, as the other verdicts do.
    pub fn publish_unidentified(&mut self, block: u8) {
        if self.writing() == Some(block) {
            self.stats.writes_unidentified = self.stats.writes_unidentified.saturating_add(1);
            self.end_generation();
        }
    }

    /// The guard has read the card's fingerprint blocks and they are the ones
    /// `card` describes. The service vouches for the card from here until
    /// [`Self::note_enumerated`] or the end of the generation.
    pub const fn note_card_verified(&mut self) {
        self.verified = true;
    }

    /// The firmware sent the card a device-info request outside the guard —
    /// the periodic re-enumeration that keeps the LCD path alive. Whatever is
    /// in the port now answers the bus, so the guard has to look again before
    /// the next block.
    pub const fn note_enumerated(&mut self) {
        self.verified = false;
    }

    /// Whether the docked card has been shown to be the one `card` describes
    /// since the port was last enumerated outside the guard.
    #[must_use]
    pub const fn card_verified(&self) -> bool {
        self.verified
    }

    /// The fingerprint the docked card is expected to show: what the guard
    /// compares against. 0 until both halves are known.
    #[must_use]
    pub const fn card(&self) -> u32 {
        self.card
    }

    /// What the guard compares against for the block in the writer's hands:
    /// the card as it stands, and — when that block is the root or the FAT —
    /// the block before and after, with the half it leaves alone. Read in the
    /// same window as [`Self::take_write`]; `write_seq` has why the second is
    /// needed.
    #[must_use]
    pub fn expected(&self) -> Expected {
        let landing = self
            .queue
            .iter()
            .find(|e| e.slot == Slot::Writing && e.epoch == self.epoch)
            .and_then(
                |e| match (e.block, self.root_seed, self.root_phases, self.fat.as_ref()) {
                    (FAT_BLOCK, Some(seed), _, Some(fat)) => Some(Landing {
                        other: seed,
                        before: phase_hashes(fat),
                        after: phase_hashes(&e.data),
                    }),
                    (ROOT_BLOCK, _, Some(root), Some(fat)) => Some(Landing {
                        other: fnv1a32(FNV_OFFSET_BASIS, fat),
                        before: root,
                        after: phase_hashes(&e.data),
                    }),
                    _ => None,
                },
            );
        Expected {
            now: self.card,
            landing,
        }
    }

    /// The root's half of the fingerprint, and what the guard needs of it.
    fn set_root(&mut self, data: &[u8; BLOCK_BYTES]) {
        self.root_seed = Some(fnv1a32(FNV_OFFSET_BASIS, data));
        self.root_phases = Some(phase_hashes(data));
    }

    fn finish_write(&mut self, block: u8, code: u8) -> bool {
        let Some(e) = self
            .queue
            .iter_mut()
            .find(|e| e.slot == Slot::Writing && e.block == block && e.epoch == self.epoch)
        else {
            return false;
        };
        e.slot = Slot::Done(code);
        if code == result::OK {
            // The card's contents moved: the fingerprint moves with them.
            match block {
                ROOT_BLOCK => {
                    let data = e.data;
                    self.set_root(&data);
                }
                FAT_BLOCK => self.fat = Some(e.data),
                _ => {}
            }
            self.recompute_card();
        }
        true
    }

    /// `card` from its halves. STATUS is due when it moves: the protocol
    /// says "on any change", and the dongle reads it at reconnect.
    fn recompute_card(&mut self) {
        let card = match (self.root_seed, self.fat.as_ref()) {
            (Some(seed), Some(fat)) => fnv1a32(seed, fat),
            _ => 0,
        };
        if card != self.card {
            self.card = card;
            self.status_due = true;
        }
    }
}

/// `[81, blk, phase, result]` and, when `result == 0`, that phase's 128 bytes.
fn build_data(reply: &Reply, phase: u8, buf: &mut [u8; MSG_MAX]) -> usize {
    buf[0] = up::DATA;
    buf[1] = reply.block;
    buf[2] = phase;
    buf[3] = reply.result;
    if reply.result != result::OK {
        return HEADER_LEN;
    }
    let at = usize::from(phase) * PHASE_BYTES;
    buf[HEADER_LEN..].copy_from_slice(&reply.data[at..at + PHASE_BYTES]);
    MSG_MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLK: u8 = 7;

    /// A service with a card docked, the pad idle, a session open, a writer
    /// present and the card's fingerprint pulled — the state a dongle that
    /// follows the protocol leaves it in before its first WRITE.
    fn serving() -> HostIo {
        let mut io = unpulled();
        pull_fingerprint(&mut io);
        io
    }

    /// As [`serving`], before the dongle has pulled anything.
    fn unpulled() -> HostIo {
        let mut io = HostIo::new();
        io.reset(1_000);
        io.open_writes();
        io.set_vmu_present(true);
        io.set_idle(true);
        drain(&mut io);
        io
    }

    /// The fingerprint of the card [`pull_fingerprint`] serves.
    fn pulled_card() -> u32 {
        fnv1a32(fnv1a32(FNV_OFFSET_BASIS, &block_of(0x10)), &block_of(0x20))
    }

    type Msg = heapless::Vec<u8, MSG_MAX>;
    type Sent = heapless::Vec<Msg, 32>;

    /// Send everything waiting, as a caller whose notifications all land.
    fn drain(io: &mut HostIo) -> Sent {
        let mut out = heapless::Vec::new();
        let mut buf = [0u8; MSG_MAX];
        while let Some(len) = io.next_message(&mut buf) {
            let mut msg = heapless::Vec::new();
            let _ = msg.extend_from_slice(&buf[..len]);
            let _ = out.push(msg);
            io.advance();
        }
        out
    }

    /// The messages of one op code, in order.
    fn only(sent: &Sent, op: u8) -> heapless::Vec<&Msg, 32> {
        sent.iter().filter(|m| m[0] == op).collect()
    }

    /// The block numbers of one op code, in order.
    fn blocks_of(sent: &Sent, op: u8) -> heapless::Vec<u8, 32> {
        only(sent, op).iter().map(|m| m[1]).collect()
    }

    fn block_of(fill: u8) -> [u8; BLOCK_BYTES] {
        let mut b = [fill; BLOCK_BYTES];
        // Something that differs per phase, so a reassembly that drops or
        // repeats one is visible.
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = u8::try_from(i / PHASE_BYTES).unwrap_or(0) ^ fill;
        }
        b
    }

    fn read(block: u8) -> [u8; 2] {
        [down::READ, block]
    }

    /// One `02 WRITE` phase of `data`.
    fn phase(block: u8, phase: u8, seq: u8, data: &[u8; BLOCK_BYTES]) -> [u8; MSG_MAX] {
        let mut msg = [0u8; MSG_MAX];
        msg[..4].copy_from_slice(&[down::WRITE, block, phase, seq]);
        let at = usize::from(phase) * PHASE_BYTES;
        msg[4..].copy_from_slice(&data[at..at + PHASE_BYTES]);
        msg
    }

    /// All four phases, as the dongle sends them.
    fn write(io: &mut HostIo, block: u8, seq: u8, data: &[u8; BLOCK_BYTES]) {
        for p in 0..PHASES {
            io.accept_write(&phase(block, p, seq, data));
        }
    }

    /// The writer's turn: take the next block, if any, and confirm it.
    fn write_one(io: &mut HostIo) -> Option<(u8, [u8; BLOCK_BYTES])> {
        let mut out = [0u8; BLOCK_BYTES];
        let block = io.take_write(&mut out)?;
        io.publish_written(block);
        Some((block, out))
    }

    /// The block the writer is handed next, if any.
    fn next_written(io: &mut HostIo) -> Option<u8> {
        write_one(io).map(|(b, _)| b)
    }

    /// Ask for a STATUS and return the last one sent.
    fn status_of(io: &mut HostIo) -> Msg {
        io.accept_write(&[down::STATUS_Q]);
        let sent = drain(io);
        let st = only(&sent, up::STATUS);
        assert!(!st.is_empty(), "a STATUS question is always answered");
        st[st.len() - 1].clone()
    }

    fn epoch_in(st: &Msg) -> u32 {
        u32::from_le_bytes([st[4], st[5], st[6], st[7]])
    }

    fn card_in(st: &Msg) -> u32 {
        u32::from_le_bytes([st[8], st[9], st[10], st[11]])
    }

    fn epoch_of(io: &mut HostIo) -> u32 {
        epoch_in(&status_of(io))
    }

    fn card_of(io: &mut HostIo) -> u32 {
        card_in(&status_of(io))
    }

    /// Serve the dongle's fingerprint pair, as a pull does.
    fn pull_fingerprint(io: &mut HostIo) {
        for (block, data) in [(255u8, block_of(0x10)), (254u8, block_of(0x20))] {
            io.accept_write(&read(block));
            assert_eq!(io.take_request(), Some(block));
            io.publish_block(block, &data);
            drain(io);
        }
    }

    // ------------------------------------------------------- the read path --

    #[test]
    fn a_read_becomes_a_request_and_its_block_becomes_four_phases() {
        let mut io = unpulled();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        assert_eq!(io.take_request(), None, "issued once, not once per window");

        let data = block_of(0xA5);
        io.publish_block(BLK, &data);
        let sent = drain(&mut io);
        assert_eq!(sent.len(), PHASES as usize);

        let mut rebuilt = [0u8; BLOCK_BYTES];
        for (phase, msg) in sent.iter().enumerate() {
            assert_eq!(msg[0], up::DATA);
            assert_eq!(msg[1], BLK);
            assert_eq!(usize::from(msg[2]), phase);
            assert_eq!(msg[3], result::OK);
            assert_eq!(msg.len(), MSG_MAX);
            rebuilt[phase * PHASE_BYTES..(phase + 1) * PHASE_BYTES].copy_from_slice(&msg[4..]);
        }
        assert_eq!(rebuilt, data, "the host reassembles exactly what was read");
        assert_eq!(io.stats().reads_accepted, 1);
    }

    #[test]
    fn a_second_read_before_the_fourth_phase_is_refused_not_queued() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        io.publish_block(BLK, &block_of(1));

        // Mid-reply: one phase out, three to go.
        let mut buf = [0u8; MSG_MAX];
        assert!(io.next_message(&mut buf).is_some());
        io.advance();

        io.accept_write(&read(9));
        assert_eq!(io.stats().reads_refused, 1);
        assert_eq!(io.take_request(), None);

        // And the first block's remaining phases are untouched.
        let rest = drain(&mut io);
        assert_eq!(rest.len(), 3);
        assert!(rest.iter().all(|m| m[1] == BLK));
    }

    #[test]
    fn a_failed_block_ends_the_wait_with_one_message_and_no_data() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        io.publish_failure(BLK);

        let sent = drain(&mut io);
        assert_eq!(sent.len(), 1);
        assert_eq!(&sent[0][..], &[up::DATA, BLK, 0, result::FAILED]);
    }

    #[test]
    fn a_read_with_no_card_is_answered_no_vmu_without_touching_the_bus() {
        let mut io = HostIo::new();
        io.reset(1);
        io.set_idle(true);
        drain(&mut io);

        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), None, "nothing to ask the bus for");
        let sent = drain(&mut io);
        assert_eq!(&sent[0][..], &[up::DATA, BLK, 0, result::NO_VMU]);
    }

    #[test]
    fn a_block_that_was_not_asked_for_is_dropped() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        io.publish_block(BLK + 1, &block_of(2));
        assert!(drain(&mut io).is_empty());
    }

    // ---------------------------------------------------------- generations --

    /// The case the harness found: a card pulled out between the block and its phases.
    #[test]
    fn a_generation_change_mid_reply_still_answers() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        io.publish_block(BLK, &block_of(3));

        let mut buf = [0u8; MSG_MAX];
        assert!(io.next_message(&mut buf).is_some());
        io.advance(); // one phase out, three owed

        io.set_vmu_present(false); // undocked mid-reply

        let sent = drain(&mut io);
        let data = only(&sent, up::DATA);
        assert_eq!(data.len(), 1, "exactly one terminal answer");
        assert_eq!(&data[0][..], &[up::DATA, BLK, 0, result::DISCARDED]);
        assert_eq!(io.stats().replies_discarded, 1);
    }

    /// And the second half of it: the terminal answer survives another change
    /// before it has been sent.
    #[test]
    fn a_second_generation_change_does_not_erase_the_unsent_discarded() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        io.publish_block(BLK, &block_of(4));

        io.set_vmu_present(false);
        io.set_vmu_present(true); // docked again before anything went out

        let sent = drain(&mut io);
        let data = only(&sent, up::DATA);
        assert_eq!(data.len(), 1);
        assert_eq!(&data[0][..], &[up::DATA, BLK, 0, result::DISCARDED]);
        // Counted once: the carried-forward error is not a second lost block.
        assert_eq!(io.stats().replies_discarded, 1);
    }

    #[test]
    fn an_unissued_read_is_discarded_too() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        io.set_vmu_present(false);
        let sent = drain(&mut io);
        let data = only(&sent, up::DATA);
        assert_eq!(&data[0][..], &[up::DATA, BLK, 0, result::DISCARDED]);
    }

    #[test]
    fn a_block_decoded_under_the_old_generation_is_never_served() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        io.set_vmu_present(false);
        io.set_vmu_present(true);
        drain(&mut io); // the DISCARDED and its STATUSes

        // The capture from before the swap finally decodes.
        io.publish_block(BLK, &block_of(5));
        assert!(drain(&mut io).is_empty(), "it is about a card that is gone");
    }

    #[test]
    fn a_session_reset_answers_nobody_and_keeps_the_generation() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        let before = epoch_of(&mut io);

        io.reset(99);
        assert!(drain(&mut io).is_empty(), "the host it was owed to is gone");
        assert_eq!(io.take_request(), None, "and so is its read");
        assert_eq!(epoch_of(&mut io), before, "the card did not change");
    }

    /// The first session of a power cycle takes its epoch from the seed, so a
    /// reboot cannot hand the dongle a value it has already seen. Later
    /// sessions do not reseed: the generation is the card's, not the host's.
    #[test]
    fn the_first_session_seeds_its_epoch_and_later_ones_keep_it() {
        let mut fresh = HostIo::new();
        fresh.reset(4_242);
        assert_eq!(epoch_of(&mut fresh), 4_242);

        fresh.reset(9_999);
        assert_eq!(epoch_of(&mut fresh), 4_242);

        fresh.set_vmu_present(true);
        assert_eq!(epoch_of(&mut fresh), 4_243, "a dock is what moves it");
    }

    // --------------------------------------------------------- the idle gate --

    #[test]
    fn nothing_is_issued_while_the_pad_is_in_use() {
        let mut io = serving();
        io.set_idle(false);
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), None);
        io.set_idle(true);
        assert_eq!(io.take_request(), Some(BLK), "and it was not lost");
    }

    #[test]
    fn input_recalls_an_issued_request_once_and_it_is_reissued_when_idle() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));

        io.set_idle(false);
        assert!(io.recall());
        assert!(!io.recall(), "recalled once, not once per window");
        assert_eq!(io.take_request(), None);

        io.set_idle(true);
        assert!(!io.recall());
        assert_eq!(io.take_request(), Some(BLK));
    }

    #[test]
    fn recall_says_no_when_there_is_nothing_in_the_readers_hands() {
        let mut io = serving();
        io.set_idle(false);
        assert!(!io.recall());
        io.accept_write(&read(BLK));
        assert!(!io.recall(), "accepted but never issued");
    }

    // -------------------------------------------------------------- STATUS --

    #[test]
    fn status_reports_the_state_the_dongle_paces_on() {
        let mut io = serving();
        let mut buf = [0u8; MSG_MAX];
        io.note_subscribed();
        let len = io.next_message(&mut buf).unwrap_or(0);
        assert_eq!(len, STATUS_LEN);
        assert_eq!(buf[0], up::STATUS);
        assert_eq!(buf[1], PROTO_VERSION);
        assert_eq!(buf[2] & flags::VMU_PRESENT, flags::VMU_PRESENT);
        assert_eq!(buf[2] & flags::ENABLED, flags::ENABLED);
        assert_eq!(buf[2] & flags::IDLE, flags::IDLE);
        assert_eq!(buf[2] & flags::DRAINING, 0, "nothing staged");
        assert_eq!(
            usize::from(buf[3]),
            QUEUE_DEPTH,
            "queue_free: the whole queue"
        );
    }

    #[test]
    fn a_status_question_is_always_answered() {
        let mut io = serving();
        io.accept_write(&[down::STATUS_Q]);
        let sent = drain(&mut io);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0][0], up::STATUS);
    }

    #[test]
    fn idle_changing_tells_the_dongle_without_being_asked() {
        let mut io = serving();
        io.set_idle(false);
        let sent = drain(&mut io);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0][2] & flags::IDLE, 0);
    }

    #[test]
    fn status_outranks_a_phase_so_discovery_is_never_blocked_behind_a_block() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        io.publish_block(BLK, &block_of(6));
        io.note_subscribed();

        let mut buf = [0u8; MSG_MAX];
        let len = io.next_message(&mut buf).unwrap_or(0);
        assert_eq!(buf[0], up::STATUS);
        assert_eq!(len, STATUS_LEN);
    }

    // -------------------------------------------------------- backpressure --

    #[test]
    fn a_notification_that_did_not_land_repeats_rather_than_skipping() {
        let mut io = serving();
        io.accept_write(&read(BLK));
        assert_eq!(io.take_request(), Some(BLK));
        io.publish_block(BLK, &block_of(7));
        // Nothing else is due: `serving` drained, and a block that is not part
        // of the fingerprint pair does not move STATUS.

        let mut first = [0u8; MSG_MAX];
        let a = io.next_message(&mut first).unwrap_or(0);
        // The caller could not send: no `advance`.
        let mut second = [0u8; MSG_MAX];
        let b = io.next_message(&mut second).unwrap_or(0);
        assert_eq!((a, first), (b, second), "the same phase is offered again");

        io.advance();
        let mut third = [0u8; MSG_MAX];
        let _ = io.next_message(&mut third).unwrap_or(0);
        assert_eq!(third[2], 1, "only a landed notification moves on");
    }

    #[test]
    fn nothing_is_offered_when_nothing_is_owed() {
        let mut io = serving();
        let mut buf = [0u8; MSG_MAX];
        assert_eq!(io.next_message(&mut buf), None);
        // And advancing on nothing is harmless.
        io.advance();
        assert_eq!(io.next_message(&mut buf), None);
    }

    // ----------------------------------------------------- the fingerprint --

    #[test]
    fn card_is_the_two_blocks_hashed_in_pull_order() {
        let mut io = unpulled();
        pull_fingerprint(&mut io);
        assert_eq!(card_of(&mut io), pulled_card());
    }

    #[test]
    fn card_stays_zero_until_both_blocks_have_been_read() {
        let mut io = unpulled();
        io.accept_write(&read(255));
        assert_eq!(io.take_request(), Some(255));
        io.publish_block(255, &block_of(0x10));
        drain(&mut io);

        assert_eq!(card_of(&mut io), 0, "the protocol's 'not read yet'");
    }

    #[test]
    fn a_generation_change_forgets_the_half_taken_fingerprint() {
        let mut io = unpulled();
        io.accept_write(&read(255));
        assert_eq!(io.take_request(), Some(255));
        io.publish_block(255, &block_of(0x10));
        drain(&mut io);

        io.set_vmu_present(false);
        io.set_vmu_present(true);
        drain(&mut io);

        // 254 alone cannot complete a fingerprint whose first half was another
        // card's.
        io.accept_write(&read(254));
        assert_eq!(io.take_request(), Some(254));
        io.publish_block(254, &block_of(0x20));
        drain(&mut io);
        assert_eq!(card_of(&mut io), 0);
    }

    #[test]
    fn the_halves_complete_the_fingerprint_in_either_order() {
        let mut io = unpulled();
        for (block, data) in [(254u8, block_of(0x20)), (255u8, block_of(0x10))] {
            io.accept_write(&read(block));
            assert_eq!(io.take_request(), Some(block));
            io.publish_block(block, &data);
            drain(&mut io);
        }
        assert_eq!(
            card_of(&mut io),
            pulled_card(),
            "the hash order is fixed, the pull order is not"
        );
    }

    /// Protocol v1: the dongle pulls 255 and 254 before it exposes the card,
    /// so a WRITE before then is off-protocol — and it is answered as the
    /// `queue_free` it was told (0) says.
    #[test]
    fn no_room_is_reported_and_no_write_is_taken_until_the_card_is_known() {
        let mut io = unpulled();
        let st = status_of(&mut io);
        assert_eq!(st[3], 0, "queue_free");
        write(&mut io, BLK, 1, &block_of(1));
        let sent = drain(&mut io);
        let acks = only(&sent, up::ACK);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0][3], result::FULL);
        assert!(!io.draining());

        pull_fingerprint(&mut io);
        let st = status_of(&mut io);
        assert_eq!(usize::from(st[3]), QUEUE_DEPTH, "known: the whole queue");
        write(&mut io, BLK, 2, &block_of(1));
        let sent = drain(&mut io);
        assert_eq!(only(&sent, up::ACK)[0][3], result::OK);
    }

    #[test]
    fn a_written_fat_advances_the_fingerprint() {
        let mut io = serving();
        let new_fat = block_of(0x21);
        write(&mut io, 254, 1, &new_fat);
        drain(&mut io);
        assert_eq!(next_written(&mut io), Some(254));
        let sent = drain(&mut io);
        assert_eq!(
            card_in(only(&sent, up::STATUS)[0]),
            fnv1a32(fnv1a32(FNV_OFFSET_BASIS, &block_of(0x10)), &new_fat),
            "STATUS carries the moved fingerprint, unasked"
        );
    }

    #[test]
    fn a_written_root_advances_the_fingerprint_with_the_fat_still_there() {
        let mut io = serving();
        let new_root = block_of(0x11);
        write(&mut io, 255, 1, &new_root);
        drain(&mut io);
        assert_eq!(next_written(&mut io), Some(255));
        assert_eq!(
            card_of(&mut io),
            fnv1a32(fnv1a32(FNV_OFFSET_BASIS, &new_root), &block_of(0x20))
        );
    }

    #[test]
    fn the_writer_is_told_what_the_card_becomes_when_its_block_is_half_the_fingerprint() {
        let mut io = serving();
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(
            io.expected(),
            Expected::unchanged(pulled_card()),
            "nothing in hand"
        );

        write(&mut io, BLK, 1, &block_of(1));
        drain(&mut io);
        assert_eq!(io.take_write(&mut out), Some(BLK));
        assert_eq!(io.expected().landing, None, "a data block moves nothing");
        io.publish_written(BLK);

        let new_fat = block_of(0x21);
        write(&mut io, 254, 2, &new_fat);
        drain(&mut io);
        assert_eq!(io.take_write(&mut out), Some(254));
        let want = fnv1a32(fnv1a32(FNV_OFFSET_BASIS, &block_of(0x10)), &new_fat);
        assert_eq!(
            io.expected(),
            Expected {
                now: pulled_card(),
                landing: Some(Landing {
                    other: fnv1a32(FNV_OFFSET_BASIS, &block_of(0x10)),
                    before: phase_hashes(&block_of(0x20)),
                    after: phase_hashes(&new_fat),
                })
            }
        );
        io.publish_written(254);
        assert_eq!(
            io.expected(),
            Expected::unchanged(want),
            "landed: now it is the card"
        );

        let new_root = block_of(0x11);
        write(&mut io, 255, 3, &new_root);
        drain(&mut io);
        assert_eq!(io.take_write(&mut out), Some(255));
        assert_eq!(
            io.expected().landing,
            Some(Landing {
                other: fnv1a32(FNV_OFFSET_BASIS, &new_fat),
                before: phase_hashes(&block_of(0x10)),
                after: phase_hashes(&new_root),
            }),
            "the root against the FAT as it now stands"
        );
    }

    #[test]
    fn a_staged_write_advances_nothing_and_a_failed_one_neither() {
        let mut io = serving();
        write(&mut io, 254, 1, &block_of(0x21));
        drain(&mut io);
        assert_eq!(card_of(&mut io), pulled_card(), "staged is not on the card");
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), Some(254));
        assert_eq!(
            io.card(),
            pulled_card(),
            "in the writer's hands is not on the card"
        );
        io.publish_write_failed(254);
        assert_eq!(io.card(), 0, "the generation ended");
    }

    // ------------------------------------------------------- the swap guard --

    #[test]
    fn the_card_is_vouched_for_from_a_guard_pass_until_the_port_is_enumerated() {
        let mut io = serving();
        assert!(!io.card_verified(), "nothing has looked yet");
        io.note_card_verified();
        assert!(io.card_verified());
        io.note_enumerated();
        assert!(
            !io.card_verified(),
            "whatever is in the port answers the bus now"
        );
        io.note_card_verified();
        io.set_vmu_present(false);
        assert!(!io.card_verified(), "the generation ended");
    }

    #[test]
    fn an_unidentified_card_ends_the_generation_with_the_block_discarded() {
        let mut io = serving();
        io.note_card_verified();
        write(&mut io, BLK, 1, &block_of(1));
        write(&mut io, BLK + 1, 2, &block_of(2));
        drain(&mut io);
        let epoch = epoch_of(&mut io);
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), Some(BLK));

        io.publish_unidentified(BLK);
        assert!(
            !io.draining(),
            "nothing left to drain onto a card that is not this one"
        );
        assert!(!io.card_verified());
        let sent = drain(&mut io);
        let st = only(&sent, up::STATUS);
        assert_ne!(epoch_in(st[0]), epoch);
        assert_eq!(
            card_in(st[0]),
            0,
            "the dongle re-pulls before exposing the card"
        );
        let written = only(&sent, up::WRITTEN);
        assert_eq!(written.len(), 2);
        assert!(
            written.iter().all(|m| m[3] == result::DISCARDED),
            "not FAILED: the card did not refuse it"
        );
        assert_eq!(io.stats().writes_unidentified, 1);
        assert_eq!(io.stats().write_failures, 0);
        assert_eq!(io.writing(), None);
    }

    #[test]
    fn a_late_unidentified_verdict_is_ignored() {
        let mut io = serving();
        write(&mut io, BLK, 1, &block_of(1));
        drain(&mut io);
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), Some(BLK));
        io.set_vmu_present(false);
        let epoch = epoch_of(&mut io);
        io.publish_unidentified(BLK);
        assert_eq!(
            epoch_of(&mut io),
            epoch,
            "the generation had already ended; not twice"
        );
        assert_eq!(io.stats().writes_unidentified, 0);
    }

    // ------------------------------------------------------- malformed writes --

    #[test]
    fn nonsense_is_dropped_in_silence() {
        let mut io = unpulled();
        for junk in [
            &[][..],
            &[down::READ][..],
            &[down::READ, 1, 2][..],
            &[0x7F][..],
            &[down::WRITE, BLK, 0, 1][..],
            &[0u8; MSG_MAX + 1][..],
        ] {
            io.accept_write(junk);
        }
        assert_eq!(io.stats().reads_accepted, 0);
        assert_eq!(io.stats().writes_refused, 0);
        assert!(drain(&mut io).is_empty());
    }

    // ------------------------------------------------------- the write path --

    #[test]
    fn four_phases_become_a_staged_block_that_is_acked_drained_and_written() {
        let mut io = serving();
        let data = block_of(0x5A);
        write(&mut io, BLK, 1, &data);

        let sent = drain(&mut io);
        let acks = only(&sent, up::ACK);
        assert_eq!(acks.len(), 1);
        assert_eq!(&acks[0][..], &[up::ACK, BLK, 1, result::OK]);
        let st = only(&sent, up::STATUS);
        assert!(
            !st.is_empty(),
            "queue_free moved, so STATUS went out unasked"
        );
        let last = st[st.len() - 1];
        assert_eq!(last[2] & flags::DRAINING, flags::DRAINING);
        assert_eq!(usize::from(last[3]), QUEUE_DEPTH - 1);
        assert!(io.draining());

        assert_eq!(
            write_one(&mut io),
            Some((BLK, data)),
            "the writer gets the bytes as sent"
        );
        assert!(!io.draining(), "on the card: nothing physical is left");

        let sent = drain(&mut io);
        let written = only(&sent, up::WRITTEN);
        assert_eq!(written.len(), 1);
        assert_eq!(&written[0][..], &[up::WRITTEN, BLK, 1, result::OK]);
        let st = only(&sent, up::STATUS);
        let last = st[st.len() - 1];
        assert_eq!(last[2] & flags::DRAINING, 0);
        assert_eq!(usize::from(last[3]), QUEUE_DEPTH, "the slot is free again");
        assert_eq!(io.stats().writes_staged, 1);
        assert_eq!(io.stats().blocks_written, 1);
    }

    #[test]
    fn the_writer_is_handed_one_block_at_a_time() {
        let mut io = serving();
        write(&mut io, 1, 1, &block_of(1));
        write(&mut io, 2, 2, &block_of(2));
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), Some(1));
        assert_eq!(io.take_write(&mut out), None, "one in the writer's hands");
        io.publish_written(1);
        assert_eq!(io.take_write(&mut out), Some(2));
    }

    #[test]
    fn with_no_writer_status_says_no_room_and_a_write_is_full() {
        let mut io = HostIo::new();
        io.reset(1);
        io.set_vmu_present(true);
        drain(&mut io);
        assert_eq!(status_of(&mut io)[3], 0, "queue_free: nothing may be sent");

        write(&mut io, BLK, 1, &block_of(0));
        let sent = drain(&mut io);
        let acks = only(&sent, up::ACK);
        assert_eq!(&acks[0][..], &[up::ACK, BLK, 1, result::FULL]);
        assert!(!io.draining());
        assert_eq!(io.stats().writes_refused, 1);
    }

    #[test]
    fn a_write_with_no_card_is_answered_no_vmu() {
        let mut io = serving();
        io.set_vmu_present(false);
        drain(&mut io);
        write(&mut io, BLK, 1, &block_of(0));
        let sent = drain(&mut io);
        let acks = only(&sent, up::ACK);
        assert_eq!(&acks[0][..], &[up::ACK, BLK, 1, result::NO_VMU]);
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), None);
    }

    #[test]
    fn a_phase_out_of_order_is_bad_and_the_block_starts_over() {
        let mut io = serving();
        let data = block_of(3);
        io.accept_write(&phase(BLK, 0, 1, &data));
        io.accept_write(&phase(BLK, 2, 1, &data)); // skipped 1
        let sent = drain(&mut io);
        assert_eq!(
            &only(&sent, up::ACK)[0][..],
            &[up::ACK, BLK, 1, result::BAD]
        );

        // The remaining phases of that attempt find nothing open.
        io.accept_write(&phase(BLK, 3, 1, &data));
        let sent = drain(&mut io);
        assert_eq!(
            &only(&sent, up::ACK)[0][..],
            &[up::ACK, BLK, 1, result::BAD]
        );
        assert!(!io.draining(), "nothing was staged");

        // A fresh attempt from phase 0 stages.
        write(&mut io, BLK, 2, &data);
        let sent = drain(&mut io);
        assert_eq!(&only(&sent, up::ACK)[0][..], &[up::ACK, BLK, 2, result::OK]);
    }

    #[test]
    fn seq_changing_mid_block_is_bad() {
        let mut io = serving();
        let data = block_of(4);
        io.accept_write(&phase(BLK, 0, 1, &data));
        io.accept_write(&phase(BLK, 1, 2, &data));
        let sent = drain(&mut io);
        assert_eq!(
            &only(&sent, up::ACK)[0][..],
            &[up::ACK, BLK, 2, result::BAD]
        );
    }

    #[test]
    fn a_repeated_phase_overwrites_that_phase() {
        let mut io = serving();
        let wrong = block_of(0xFF);
        let right = block_of(0x0F);
        io.accept_write(&phase(BLK, 0, 1, &right));
        io.accept_write(&phase(BLK, 1, 1, &wrong));
        io.accept_write(&phase(BLK, 1, 1, &right)); // again, corrected
        io.accept_write(&phase(BLK, 2, 1, &right));
        io.accept_write(&phase(BLK, 3, 1, &right));
        assert_eq!(write_one(&mut io), Some((BLK, right)));
    }

    #[test]
    fn a_resend_after_a_lost_ack_is_acked_again_not_restaged() {
        let mut io = serving();
        let data = block_of(5);
        write(&mut io, BLK, 1, &data);
        drain(&mut io);
        write(&mut io, BLK, 1, &data);
        let sent = drain(&mut io);
        assert_eq!(&only(&sent, up::ACK)[0][..], &[up::ACK, BLK, 1, result::OK]);
        assert!(
            only(&sent, up::WRITTEN).is_empty(),
            "nothing was superseded"
        );
        assert_eq!(usize::from(status_of(&mut io)[3]), QUEUE_DEPTH - 1);
        assert_eq!(io.stats().writes_staged, 1);
    }

    #[test]
    fn a_pause_is_refused_while_saves_drain() {
        let mut io = serving();
        write(&mut io, BLK, 1, &block_of(5));
        drain(&mut io);
        assert!(!io.pause_writes_unless_draining());
        write(&mut io, 3, 2, &block_of(3));
        let sent = drain(&mut io);
        assert_eq!(&only(&sent, up::ACK)[0][..], &[up::ACK, 3, 2, result::OK]);
    }

    #[test]
    fn a_paused_service_refuses_writes_and_reports_no_room() {
        let mut io = serving();
        assert!(io.pause_writes_unless_draining());
        write(&mut io, BLK, 1, &block_of(5));
        let sent = drain(&mut io);
        assert_eq!(
            &only(&sent, up::ACK)[0][..],
            &[up::ACK, BLK, 1, result::FULL]
        );
        assert_eq!(status_of(&mut io)[3], 0);
        assert!(!io.draining());
    }

    #[test]
    fn the_next_session_lifts_a_pause() {
        let mut io = serving();
        assert!(io.pause_writes_unless_draining());
        io.reset(1_000);
        io.set_idle(true);
        drain(&mut io);
        write(&mut io, BLK, 1, &block_of(5));
        let sent = drain(&mut io);
        assert_eq!(&only(&sent, up::ACK)[0][..], &[up::ACK, BLK, 1, result::OK]);
    }

    #[test]
    fn a_resend_does_not_match_a_done_slot_from_an_earlier_generation() {
        let mut io = serving();
        let data = block_of(5);
        write(&mut io, BLK, 1, &data);
        drain(&mut io);
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), Some(BLK));
        io.publish_written(BLK);
        // The WRITTEN is still owed, and the slot is made to predate the
        // current generation — what the card gate and delivery order
        // otherwise keep a resend from ever seeing.
        let held = io
            .queue
            .iter()
            .position(|e| e.block == BLK && e.slot != Slot::Free);
        let Some(i) = held else {
            panic!("the written block's slot is held until its WRITTEN is delivered");
        };
        io.queue[i].epoch = io.epoch.wrapping_sub(1);

        write(&mut io, BLK, 1, &data);
        assert_eq!(io.stats().writes_staged, 2, "staged fresh, not re-acked");
        assert!(io
            .queue
            .iter()
            .any(|e| e.slot == Slot::Staged && e.block == BLK));
    }

    #[test]
    fn a_newer_seq_replaces_a_staged_block_in_place_and_supersedes_the_old() {
        let mut io = serving();
        write(&mut io, 1, 1, &block_of(1));
        write(&mut io, BLK, 2, &block_of(0xAA));
        write(&mut io, 3, 3, &block_of(3));
        drain(&mut io);

        let newer = block_of(0xBB);
        write(&mut io, BLK, 4, &newer);
        let sent = drain(&mut io);
        assert_eq!(
            &only(&sent, up::WRITTEN)[0][..],
            &[up::WRITTEN, BLK, 2, result::SUPERSEDED]
        );
        assert_eq!(&only(&sent, up::ACK)[0][..], &[up::ACK, BLK, 4, result::OK]);
        assert_eq!(
            usize::from(status_of(&mut io)[3]),
            QUEUE_DEPTH - 3,
            "one slot, not two"
        );

        // It kept its place in the drain order, and the writer sees the new bytes.
        assert_eq!(next_written(&mut io), Some(1));
        assert_eq!(write_one(&mut io), Some((BLK, newer)));
        assert_eq!(next_written(&mut io), Some(3));
        let sent = drain(&mut io);
        let written = only(&sent, up::WRITTEN);
        assert_eq!(&written[1][..], &[up::WRITTEN, BLK, 4, result::OK]);
        assert_eq!(io.stats().writes_superseded, 1);
    }

    #[test]
    fn a_block_in_the_writers_hands_is_not_replaced() {
        let mut io = serving();
        write(&mut io, BLK, 1, &block_of(1));
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), Some(BLK));

        let newer = block_of(2);
        write(&mut io, BLK, 2, &newer);
        drain(&mut io);
        io.publish_written(BLK);
        let sent = drain(&mut io);
        let written = only(&sent, up::WRITTEN);
        assert_eq!(
            &written[0][..],
            &[up::WRITTEN, BLK, 1, result::OK],
            "the old one landed"
        );
        assert_eq!(
            write_one(&mut io),
            Some((BLK, newer)),
            "and the new one follows"
        );
    }

    #[test]
    fn drain_order_is_first_staging_order() {
        let mut io = serving();
        for b in [200u8, 3, 254, 255] {
            write(&mut io, b, b, &block_of(b));
        }
        let mut order = heapless::Vec::<u8, 4>::new();
        while let Some(b) = next_written(&mut io) {
            let _ = order.push(b);
        }
        assert_eq!(&order[..], &[200, 3, 254, 255], "the console's own order");
        let sent = drain(&mut io);
        assert_eq!(
            &blocks_of(&sent, up::WRITTEN)[..],
            &[200, 3, 254, 255],
            "and WRITTENs in the same order"
        );
    }

    #[test]
    fn a_full_queue_answers_full_and_a_written_makes_room() {
        let mut io = serving();
        for b in 0..QUEUE_DEPTH {
            let b = u8::try_from(b).unwrap();
            write(&mut io, b, b, &block_of(b));
        }
        drain(&mut io);
        assert_eq!(status_of(&mut io)[3], 0);

        write(&mut io, 100, 100, &block_of(100));
        let sent = drain(&mut io);
        assert_eq!(
            &only(&sent, up::ACK)[0][..],
            &[up::ACK, 100, 100, result::FULL]
        );

        // The slot is not free at the write — it is free when the WRITTEN
        // has been delivered, which is when the dongle stops counting it.
        assert!(write_one(&mut io).is_some());
        let sent = drain(&mut io);
        assert_eq!(only(&sent, up::WRITTEN).len(), 1);
        let st = only(&sent, up::STATUS);
        assert_eq!(st[st.len() - 1][3], 1, "STATUS says there is room again");
    }

    #[test]
    fn a_refused_write_ends_the_generation_and_discards_the_rest() {
        let mut io = serving();
        write(&mut io, 1, 1, &block_of(1));
        write(&mut io, 2, 2, &block_of(2));
        drain(&mut io);
        let before = epoch_of(&mut io);

        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), Some(1));
        io.publish_write_failed(1);

        let sent = drain(&mut io);
        let written = only(&sent, up::WRITTEN);
        assert_eq!(&written[0][..], &[up::WRITTEN, 1, 1, result::FAILED]);
        assert_eq!(&written[1][..], &[up::WRITTEN, 2, 2, result::DISCARDED]);
        assert_ne!(epoch_of(&mut io), before);
        assert!(!io.draining());
        assert_eq!(io.take_write(&mut out), None);
    }

    #[test]
    fn the_writer_can_see_whether_its_block_is_still_its_own() {
        let mut io = serving();
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.writing(), None);
        write(&mut io, BLK, 1, &block_of(0xAA));
        assert_eq!(io.writing(), None, "staged, not yet taken");
        assert_eq!(io.take_write(&mut out), Some(BLK));
        assert_eq!(io.writing(), Some(BLK));
        // The card leaves: the generation ends, and the block is no longer
        // the writer's to finish.
        io.set_vmu_present(false);
        assert_eq!(io.writing(), None);
        io.set_vmu_present(true);
        assert_eq!(io.writing(), None, "not revived by the next card");
    }

    #[test]
    fn an_undock_mid_write_discards_the_block_and_ignores_its_late_result() {
        let mut io = serving();
        write(&mut io, BLK, 1, &block_of(1));
        drain(&mut io);
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), Some(BLK));

        io.set_vmu_present(false);
        io.publish_written(BLK); // the transaction finished as the card left
        let sent = drain(&mut io);
        let written = only(&sent, up::WRITTEN);
        assert_eq!(written.len(), 1);
        assert_eq!(&written[0][..], &[up::WRITTEN, BLK, 1, result::DISCARDED]);
        assert_eq!(io.stats().blocks_written, 0);
    }

    #[test]
    fn a_half_assembled_block_does_not_survive_a_generation_change() {
        let mut io = serving();
        let data = block_of(9);
        io.accept_write(&phase(BLK, 0, 1, &data));
        io.accept_write(&phase(BLK, 1, 1, &data));
        io.set_vmu_present(false);
        io.set_vmu_present(true);
        drain(&mut io);
        io.accept_write(&phase(BLK, 2, 1, &data));
        io.accept_write(&phase(BLK, 3, 1, &data));
        let sent = drain(&mut io);
        assert_eq!(
            &only(&sent, up::ACK)[0][..],
            &[up::ACK, BLK, 1, result::BAD]
        );
        assert!(!io.draining());
    }

    // ------------------------------------- the generation across the link --

    /// (i) A session reset with the card still docked: `epoch` and `card`
    /// unchanged.
    #[test]
    fn a_reconnect_with_the_card_docked_keeps_epoch_and_card() {
        let mut io = serving();
        pull_fingerprint(&mut io);
        let (epoch, card) = (epoch_of(&mut io), card_of(&mut io));
        assert_ne!(card, 0);

        io.link_down();
        io.reset(77); // the next connection
        io.note_subscribed();
        let sent = drain(&mut io);
        let st = only(&sent, up::STATUS);
        assert_eq!(st.len(), 1, "STATUS on subscribe, and nothing else owed");
        assert_eq!(epoch_in(st[0]), epoch);
        assert_eq!(card_in(st[0]), card);
    }

    /// (ii) Reconnect mid-drain: blocks staged and acked, the link drops, the
    /// drain carries on, the link returns — same `epoch`, a WRITTEN for every
    /// block drained in the gap, the rest following in order.
    #[test]
    fn a_reconnect_mid_drain_delivers_the_gaps_writtens_under_the_same_epoch() {
        let mut io = serving();
        for b in [10u8, 11, 12, 13] {
            write(&mut io, b, b, &block_of(b));
        }
        let sent = drain(&mut io);
        assert_eq!(only(&sent, up::ACK).len(), 4, "all acked before the drop");
        let epoch = epoch_of(&mut io);

        io.link_down();
        assert!(io.draining(), "the lease holds: the rail stays up for this");
        // Two blocks drain while nobody is listening.
        assert_eq!(next_written(&mut io), Some(10));
        assert_eq!(next_written(&mut io), Some(11));
        let mut buf = [0u8; MSG_MAX];
        assert!(
            io.next_message(&mut buf).is_some(),
            "owed, and held until it can go"
        );

        io.reset(5_555); // the link returns
        io.note_subscribed();
        let sent = drain(&mut io);
        assert_eq!(sent[0][0], up::STATUS, "STATUS first");
        assert_eq!(epoch_in(&sent[0]), epoch);
        assert_eq!(
            &blocks_of(&sent, up::WRITTEN)[..],
            &[10, 11],
            "the gap's WRITTENs, in drain order"
        );
        assert!(only(&sent, up::WRITTEN).iter().all(|m| m[3] == result::OK));

        // The rest follow in order.
        assert_eq!(next_written(&mut io), Some(12));
        assert_eq!(next_written(&mut io), Some(13));
        let sent = drain(&mut io);
        assert_eq!(&blocks_of(&sent, up::WRITTEN)[..], &[12, 13]);
        assert!(!io.draining());
        assert_eq!(io.stats().blocks_written, 4);
        assert_eq!(io.stats().writes_discarded, 0);
    }

    /// (iii) The card pulled during the gap: new `epoch`, `card` 0, the queue
    /// discarded, and no `WRITTEN OK` for anything that had not reached the
    /// card.
    #[test]
    fn a_card_pulled_during_the_gap_discards_the_queue_and_moves_the_epoch() {
        let mut io = serving();
        pull_fingerprint(&mut io);
        for b in [20u8, 21, 22] {
            write(&mut io, b, b, &block_of(b));
        }
        drain(&mut io);
        let (epoch, card) = (epoch_of(&mut io), card_of(&mut io));
        assert_ne!(card, 0);

        io.link_down();
        assert_eq!(next_written(&mut io), Some(20), "one reached the card");
        io.set_vmu_present(false); // pulled, or simply unobserved
        assert!(!io.draining(), "nothing left to hold the rail for");

        io.reset(1); // the link returns
        io.note_subscribed();
        let sent = drain(&mut io);
        let st = only(&sent, up::STATUS);
        assert_ne!(epoch_in(st[0]), epoch);
        assert_eq!(card_in(st[0]), 0, "card: not read yet");
        assert_eq!(st[0][2] & flags::VMU_PRESENT, 0);

        let written = only(&sent, up::WRITTEN);
        assert_eq!(written.len(), 3, "every acked block is answered, once");
        assert_eq!(&written[0][..], &[up::WRITTEN, 20, 20, result::OK]);
        assert_eq!(&written[1][..], &[up::WRITTEN, 21, 21, result::DISCARDED]);
        assert_eq!(&written[2][..], &[up::WRITTEN, 22, 22, result::DISCARDED]);
        let mut out = [0u8; BLOCK_BYTES];
        assert_eq!(io.take_write(&mut out), None, "the queue is gone");
        assert_eq!(io.stats().writes_discarded, 2);
    }

    /// The outbox across a link drop: an ACK the host never saw is for a host
    /// that is gone; a SUPERSEDED is a WRITTEN and is owed for ever.
    #[test]
    fn a_link_drop_keeps_owed_writtens_and_drops_stale_acks() {
        let mut io = serving();
        write(&mut io, BLK, 1, &block_of(1));
        write(&mut io, BLK, 2, &block_of(2)); // supersedes 1, acks 2; nothing sent yet
        io.link_down();
        io.reset(3);
        io.note_subscribed();
        let sent = drain(&mut io);
        assert!(
            only(&sent, up::ACK).is_empty(),
            "the ACKs were for the old session"
        );
        let written = only(&sent, up::WRITTEN);
        assert_eq!(written.len(), 1);
        assert_eq!(&written[0][..], &[up::WRITTEN, BLK, 1, result::SUPERSEDED]);
        assert!(io.draining(), "and the staged block is still to be written");
    }

    /// The link drop is not a generation change even with a read in flight:
    /// the read is dropped, the queue is not.
    #[test]
    fn a_link_drop_drops_the_read_and_keeps_the_queue() {
        let mut io = serving();
        write(&mut io, BLK, 1, &block_of(1));
        drain(&mut io);
        io.accept_write(&read(9));
        assert_eq!(io.take_request(), Some(9));
        let epoch = epoch_of(&mut io);

        io.link_down();
        io.publish_block(9, &block_of(9)); // the capture decodes after the drop
        assert!(drain(&mut io).is_empty(), "nobody to answer");
        assert_eq!(epoch_of(&mut io), epoch);
        assert_eq!(next_written(&mut io), Some(BLK));
    }

    // ------------------------------------------------------------- sizing --

    /// The number that sizes the budget: the measured entry size, so the queue's
    /// capacity is derived rather than asserted.
    #[test]
    fn an_entry_is_the_block_and_twelve_bytes() {
        // Slot (2), block, seq, epoch (4), stamp (4): 524, word-aligned.
        assert_eq!(core::mem::size_of::<Entry>(), BLOCK_BYTES + 12);
        assert_eq!(
            core::mem::size_of::<[Entry; QUEUE_DEPTH]>(),
            QUEUE_DEPTH * (BLOCK_BYTES + 12)
        );
        // And the whole service, for the `.bss` line in `## Built`.
        assert!(core::mem::size_of::<HostIo>() < 6 * 1024);
    }
}
