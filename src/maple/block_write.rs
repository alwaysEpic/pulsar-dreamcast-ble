// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! `BLOCK_WRITE` of a staged block onto the docked VMU's storage function,
//! and the read-back that says it landed.
//!
//! The VMU writer: the poll loop's other storage consumer, beside
//! [`super::block_read`]. It takes one block at a time from the host service
//! (`ble::host_vmu::take_write`), puts it on the card as four 128-byte phases
//! and a commit, reads it back through the ordinary read path, and reports
//! the verdict (`publish_written` / `publish_write_failed`). Which transaction
//! goes next, and what a reply does to the sequence, is
//! [`maple_protocol::write_seq`], which is host-tested; this file is the bus,
//! the bytes and the glue to the reader.
//!
//! # The window rule
//!
//! Every transaction here is reply-bearing and takes a quiet window of its
//! own, in place of the controller poll — never beside it, and never two to
//! a window. Unacknowledged phases with a
//! single reply-bearing commit would be cheaper, but that depends on the
//! commit reliably reporting an incomplete block, which the fault-injection
//! gate has not yet shown. So a block is five windows, plus its read-back.
//!
//! The windows come from the read schedule (`read_sched`), shared with the
//! reader: the same cadence, the same 15 ms stand-off from an LCD frame's
//! return, and the same display hold while one is owed. The writer goes
//! first when both want the slot — a save the console has already committed
//! must land whether or not the player is at a menu, and host reads are
//! served at idle anyway.
//!
//! # The TX path
//!
//! Bit-banged, on the cycle-anchored half-bit (ADR-010), like the controller
//! poll and the `BLOCK_READ` command — not on PWM/EasyDMA like the LCD
//! frame. The DMA engine is the better transmitter, but the one thing this
//! path needs that the LCD frame does not is the *reply* after the handoff
//! back to GPIO, and that is exactly the shape that failed on the XIAO in
//! June with no root cause (`pwm_tx::write_packet_dma`). The bit-banged
//! `BLOCK_WRITE` + ACK capture is the shape every sync and goodbye splash has
//! used on this board since. 141 bytes ≈ 5.5 ms on the wire and a 3.1 ms
//! ACK capture fit the ≈ 10 ms the window guarantees; moving the phases to
//! DMA once the handoff is understood is the obvious saving.
//!
//! # What the ACK does and does not say
//!
//! An `ACK` to a phase means the card took it. An `ACK` to the commit means
//! the card is happy; a *lost* one means nothing either way, because the
//! capture can miss a reply the card sent. So the commit's ACK never decides
//! the block: the read-back does, byte for byte against the copy in hand.
//! A block that reads back as written is `Written`; anything else spends a
//! try, and the budget (`write_seq::WRITE_TRIES`) is what makes a block
//! `Failed`.
//!
//! # Invariants this path leans on
//!
//! - **The VMU is enumerated before any storage command** (`main.rs` owns
//!   that); an unenumerated card ignores a `BLOCK_WRITE` in silence.
//! - **A block is the writer's only while the service says so.** The
//!   generation can end under a write — undock, the absence streak, a
//!   refused write on another block — and the service marks the slot
//!   `DISCARDED` without telling anyone. The window head asks
//!   `host_vmu::writing()` and abandons the block if it is no longer named;
//!   writing on would put a stale block onto whatever card is docked now.
//! - **The swap guard runs here, on the writer's windows.** `write_seq`
//!   has the rule: before phase 0 of a block the service cannot vouch for,
//!   and after any silent phase, the sequence enumerates the card itself
//!   (`Tx::Enumerate`, a device-info request in a write window) and reads
//!   blocks 255 and 254 through the read path under `Owner::Writer`, as the
//!   read-back is; the compare against `host_vmu::card` is the sequence's.
//!   `main.rs` holds up its end by not re-enumerating the port while the
//!   queue drains, so a swapped card stays deaf until the guard looks at
//!   it. A guard pass is reported through [`BlockWriter::take_guard_pass`]
//!   so the service can vouch for the card until the port is next
//!   enumerated outside the guard.
//! - **The queue is opened to the dongle wherever this path is compiled**
//!   (`main.rs`, at boot). Opened in release builds on 2026-09-22; the
//!   guard's hardware gate passed on 2026-09-25 (v326).
//! - **The read-back survives what the host's reads do not.** A link drop
//!   or resumed input clears the host's requests from the reader
//!   (`BlockReader::clear_host`); the writer's request is tagged
//!   `Owner::Writer` and stays. A full clear (presence change, controller
//!   lost) takes it too, and the generation has ended in every such case —
//!   so the block is abandoned at the next window head, never re-asked.

use heapless::Vec;
use maple_protocol::block_bytes::{BLOCK_BYTES, PHASE_BYTES};
use maple_protocol::read_sched::ReadSched;
use maple_protocol::write_seq::{Expected, Read, Reply, Step, Tx, Verdict, Verify, WriteSeq};

use super::block_read::BlockReader;
use super::gpio_bus::MapleBus;
use super::host::{addressing, commands, functions};
use super::MaplePacket;

/// The location word's phase field for the commit: one past the four data
/// phases.
const COMMIT_PHASE: u32 = 4;

/// How long a reply may take to start, from the frame's last edge. The
/// host's own reply timeout; the LCD ACK arrives inside it on this board.
/// A card that takes longer to acknowledge a phase would read as silent
/// here — if the bench shows every phase silent, this is the first suspect.
const REPLY_TIMEOUT_US: u32 = 2_000;

/// Counters, as plain totals, for whatever reads them next.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// Transactions put on the bus, phases, commits and recoveries alike.
    pub issued: u32,
    /// Transactions that got no reply.
    pub silent: u32,
    /// Transactions answered with something other than `ACK`.
    pub refused: u32,
    /// Read-backs that differed from the block written.
    pub mismatched: u32,
    /// Read-backs the read path gave up on.
    pub unread: u32,
    /// Blocks confirmed on the card.
    pub written: u32,
    /// Blocks that spent their budget.
    pub failed: u32,
    /// Blocks the guard would not write: another card, or no identity.
    pub unidentified: u32,
    /// Guard enumerations put on the bus.
    pub guarded: u32,
}

/// The firmware half of the writer: the block in hand and the bus.
pub struct BlockWriter {
    seq: Option<WriteSeq>,
    /// The block in hand, in **image order** — what `take_write` copied out,
    /// what each phase is cut from, and what the read-back is compared to.
    data: [u8; BLOCK_BYTES],
    stats: Stats,
}

impl Default for BlockWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockWriter {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            seq: None,
            data: [0; BLOCK_BYTES],
            stats: Stats {
                issued: 0,
                silent: 0,
                refused: 0,
                mismatched: 0,
                unread: 0,
                written: 0,
                failed: 0,
                unidentified: 0,
                guarded: 0,
            },
        }
    }

    #[must_use]
    pub const fn stats(&self) -> &Stats {
        &self.stats
    }

    /// A block is in hand, in any step.
    #[must_use]
    pub const fn busy(&self) -> bool {
        self.seq.is_some()
    }

    /// The block in hand.
    #[must_use]
    pub fn block(&self) -> Option<u8> {
        self.seq.as_ref().map(WriteSeq::block)
    }

    /// Where the next block goes before [`Self::begin`]. Only meaningful
    /// while nothing is in hand: the caller fills it (`take_write`) and then
    /// begins, in the same window.
    pub const fn data_mut(&mut self) -> &mut [u8; BLOCK_BYTES] {
        &mut self.data
    }

    /// Start on the block now in [`Self::data_mut`]. `expected` is the
    /// fingerprint the card must show — and becomes, if this block is half
    /// of it — and `verified` whether the service already vouches for the
    /// card; both from `host_vmu`, read in the same window as `take_write`.
    /// A block already in hand is dropped: the caller checks [`Self::busy`]
    /// first, and the service hands out one at a time anyway.
    pub const fn begin(&mut self, block: u8, expected: Expected, verified: bool) {
        self.seq = Some(WriteSeq::new(block, expected, verified));
    }

    /// Whether the guard has passed since this was last asked — the caller
    /// tells the service, which vouches for the card from then on.
    pub fn take_guard_pass(&mut self) -> bool {
        self.seq.as_mut().is_some_and(WriteSeq::take_guard_pass)
    }

    /// Drop the block in hand without a verdict. The generation ended under
    /// it; the service already holds its `DISCARDED`.
    pub const fn abandon(&mut self) {
        self.seq = None;
    }

    /// A transaction is owed: the next scheduled slot should go to
    /// [`Self::write_window`] rather than the reader.
    #[must_use]
    pub fn wants_window(&self) -> bool {
        self.seq.as_ref().is_some_and(|s| s.next_tx().is_some())
    }

    /// The window head. Hands a due read — the read-back, or one of the
    /// guard's fingerprint blocks — to the reader, and returns the verdict on
    /// the block in hand once there is one, after which nothing is in hand.
    pub fn service(&mut self, reader: &mut BlockReader) -> Option<(u8, Verdict)> {
        let seq = self.seq.as_mut()?;
        if let Some(block) = seq.take_read_request() {
            if !reader.request_verify(block) {
                // A full queue cannot happen with one writer's read at a time
                // and the host's one outstanding read; if it ever does, ask
                // again next window rather than wait for an answer that
                // never comes.
                seq.note_read_lost();
            }
        }
        let Step::Done(verdict) = seq.step() else {
            return None;
        };
        let block = seq.block();
        self.seq = None;
        match verdict {
            Verdict::Written => self.stats.written = self.stats.written.saturating_add(1),
            Verdict::Failed => self.stats.failed = self.stats.failed.saturating_add(1),
            Verdict::Unidentified => {
                self.stats.unidentified = self.stats.unidentified.saturating_add(1);
            }
        }
        Some((block, verdict))
    }

    /// The reader decoded a block of the writer's. Which one it was waiting
    /// for says what it means: the read-back is compared byte for byte
    /// against the copy in hand, a fingerprint block goes to the guard. A
    /// block the sequence is not waiting for is ignored — the read-back of a
    /// block whose step has moved on, a fingerprint block after the guard
    /// gave up on it.
    pub fn note_read_back(&mut self, block: u8, data: &[u8; BLOCK_BYTES]) {
        let Some(seq) = self.seq.as_mut() else {
            return;
        };
        match seq.reading() {
            Some(Read::Guard(b)) if b == block => seq.note_guard_block(block, data),
            Some(Read::Verify(b)) if b == block => {
                if *data == self.data {
                    seq.note_verify(Verify::Match);
                } else {
                    self.stats.mismatched = self.stats.mismatched.saturating_add(1);
                    seq.note_verify(Verify::Mismatch);
                }
            }
            _ => {}
        }
    }

    /// The reader gave up on a block of the writer's.
    pub const fn note_read_back_failed(&mut self, block: u8) {
        let Some(seq) = self.seq.as_mut() else {
            return;
        };
        match seq.reading() {
            Some(Read::Guard(b)) if b == block => seq.note_guard_unread(block),
            Some(Read::Verify(b)) if b == block => {
                self.stats.unread = self.stats.unread.saturating_add(1);
                seq.note_verify(Verify::Unread);
            }
            _ => {}
        }
    }

    /// The write window: the sequence's next transaction on the bus and its
    /// reply, in place of the controller poll. Called only when
    /// [`Self::wants_window`] said so at the head of this same window.
    ///
    /// `sched` and `now` are here for `read_window`'s reason: committing the
    /// command to the bus is what consumes the cadence slot, and a dispatch
    /// that finds nothing to send must leave the schedule untouched.
    pub fn write_window(&mut self, bus: &mut MapleBus, sched: &mut ReadSched, now: u64) {
        let Some(seq) = self.seq.as_mut() else {
            sched.note_read_abandoned();
            return;
        };
        let Some(tx) = seq.next_tx() else {
            sched.note_read_abandoned();
            return;
        };
        sched.note_read_issued(now);
        self.stats.issued = self.stats.issued.saturating_add(1);
        let block = seq.block();
        let reply = match tx {
            Tx::Phase(n) => {
                let at = usize::from(n) * PHASE_BYTES;
                match self
                    .data
                    .get(at..)
                    .and_then(|rest| rest.split_first_chunk::<PHASE_BYTES>())
                {
                    Some((chunk, _)) => send_phase(bus, block, n, chunk),
                    // Unreachable — `n` is below `write_seq::PHASES` and four
                    // phases are the block — but a phase not sent is a phase
                    // refused, not a panic.
                    None => Reply::Error,
                }
            }
            Tx::Commit | Tx::Recover => send_commit(bus, block),
            Tx::Enumerate => {
                self.stats.guarded = self.stats.guarded.saturating_add(1);
                send_enumerate(bus)
            }
        };
        match reply {
            Reply::Ack => {}
            Reply::Silent => self.stats.silent = self.stats.silent.saturating_add(1),
            Reply::Error => self.stats.refused = self.stats.refused.saturating_add(1),
        }
        seq.note_reply(reply);
    }
}

/// One phase on the bus and its reply.
fn send_phase(bus: &mut MapleBus, block: u8, phase: u8, data: &[u8; PHASE_BYTES]) -> Reply {
    // Location word: partition << 24 | phase << 16 | block. Partition 0.
    let location = (u32::from(phase) << 16) | u32::from(block);
    bus.write_block_phase(addressing::HOST, addressing::SUB_SLOT_1, location, data);
    judge(bus.read_packet_bulk(REPLY_TIMEOUT_US))
}

/// The commit — `GET_LAST_ERROR` addressed to phase 4 of the block — and its
/// reply. The same frame serves as the recovery after a failed phase.
fn send_commit(bus: &mut MapleBus, block: u8) -> Reply {
    let mut payload: Vec<u32, 32> = Vec::new();
    let _ = payload.push(functions::STORAGE);
    let _ = payload.push((COMMIT_PHASE << 16) | u32::from(block));
    let packet = MaplePacket {
        sender: addressing::HOST,
        recipient: addressing::SUB_SLOT_1,
        command: commands::GET_LAST_ERROR,
        payload,
    };
    bus.write_packet(&packet);
    judge(bus.read_packet_bulk(REPLY_TIMEOUT_US))
}

/// The guard's device-info request to the card, and whether it answered as
/// one. The same frame as `MapleHost::enumerate_vmu`, sent from here because
/// it is the writer's window and the writer's rule; the reply is judged for
/// the guard (`write_seq`: an answer arms the reads, anything else spends a
/// guard try), where `enumerate_vmu`'s is sent for its side effect.
fn send_enumerate(bus: &mut MapleBus) -> Reply {
    let packet = MaplePacket {
        sender: addressing::HOST,
        recipient: addressing::SUB_SLOT_1,
        command: commands::DEVICE_INFO_REQUEST,
        payload: Vec::new(),
    };
    bus.write_packet(&packet);
    match bus.read_packet_bulk(REPLY_TIMEOUT_US) {
        None => Reply::Silent,
        Some(pkt) if pkt.command == commands::DEVICE_INFO_RESPONSE => Reply::Ack,
        Some(_) => Reply::Error,
    }
}

/// What a captured reply, or its absence, says.
fn judge(reply: Option<MaplePacket>) -> Reply {
    match reply {
        None => Reply::Silent,
        Some(pkt) if pkt.command == commands::ACK => Reply::Ack,
        Some(_) => Reply::Error,
    }
}
