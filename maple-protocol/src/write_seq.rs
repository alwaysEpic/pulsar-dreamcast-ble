// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! One block's write onto the VMU, and the swap guard around it.
//!
//! Which storage transaction goes out next, what each reply does to the
//! sequence, when the block is on the card or given up — and the guard that
//! identifies the card before the first phase and after any silence.
//!
//! The firmware's `maple::block_write` owns the bus, the bytes and the
//! read-back; this owns the rules. It is here for the reason `read_pipeline`
//! is: the firmware crate's tests do not run, and a retry rule left there is
//! a rule nothing exercises.
//!
//! # The sequence
//!
//! ```text
//!   Guard ─same─▶ Phase 0 ─ack─▶ Phase 1 ─ack─▶ Phase 2 ─ack─▶ Phase 3 ─ack─▶ Commit ─▶ Verify ─match─▶ Written
//!     │             │              │              │              │              │           │
//!     │             └──────────────┴── silent ────┴──────────────┘              │      mismatch, unread
//!     │                    │                                                    │           │
//!     │                    ▼                                                    │           │
//!     │                  Guard ─same─▶ (a try spent) ──▶ Recover ◀── refused ───┴───────────┘
//!     │                    │                               │
//!  different            different                       any reply
//!     ▼                    ▼                               ▼
//!  Unidentified        Unidentified                     Phase 0    (the budget → Failed)
//! ```
//!
//! A block is four `BLOCK_WRITE` phases of 128 bytes and then the commit, a
//! `GET_LAST_ERROR` addressed to phase 4 of the same block. Each is its own
//! reply-bearing transaction in its own quiet window: phase ACKs are read
//! until the fault-injection gate has shown that
//! a commit reliably reports an incomplete block, and that gate has not run.
//!
//! **A phase that goes unanswered runs the guard, then restarts the block
//! through [`Tx::Recover`]**, which is the same `GET_LAST_ERROR` frame: after a
//! failed phase the card answers nothing further for that sequence until it
//! sees one (dreamcast.wiki). The recovery's own reply is not judged — its job
//! is the card's reset, and phase 0 is what shows whether it worked. A phase
//! that is *refused* restarts without the guard: a card that answered is the
//! card that was enumerated, and enumeration is the guard's whole premise.
//!
//! **The commit's ACK is not what says "on the card". The read-back is.** A
//! reply capture can be lost to the radio with the bytes landed — every earlier
//! revision's LCD ACK `bool` has read `false` that way — so a silent commit is
//! not a failure here: it goes to [`Step::Verify`] like an acked one, and the
//! block read back through the ordinary read path decides. A refused commit
//! (a `FILE_ERROR` reply) restarts. A mismatch restarts. A card that cannot be
//! read back restarts, and the budget ends it.
//!
//! **The budget is per block, in attempts of the whole sequence**:
//! [`WRITE_TRIES`]. Past it the verdict is [`Verdict::Failed`], which the
//! service turns into `84 WRITTEN … FAILED` and the end of the generation
//! (`host_vmu_io`): a card that refuses writes is not one to keep draining
//! onto.
//!
//! # The swap guard
//!
//! Presence needs four missed probes at 5 s to end a generation, so a card
//! swapped inside that window is, to the service, the same card — and the
//! blocks the dongle staged for the old one would go onto the new one under
//! the same `epoch`. The guard, a fingerprint check, is what stops
//! that. It rests on one observed fact: **a
//! freshly docked VMU answers nothing on the bus until it has been sent a
//! device-info request** (bench, 2026-09-11). The firmware therefore stops
//! its periodic re-enumeration while the queue is draining, so a card swapped
//! mid-drain stays deaf, and the phase it ignores is the alarm.
//!
//! The guard itself is an enumeration of its own — the one place a possibly
//! new card is armed on purpose — followed by a read of block 255 and then
//! 254 through the ordinary read path, and their FNV-1a fingerprint compared
//! against the one the service holds for this generation (`HostIo::card`:
//! taken from the dongle's pull, advanced as writes to those blocks land).
//! **Same → the sequence goes on. Different → [`Verdict::Unidentified`]**,
//! which ends the generation: every staged block becomes `DISCARDED`, and the
//! dongle re-pulls before it exposes the card again.
//!
//! It runs at two moments, not before every block:
//!
//! - **Before phase 0 of a block, when the service cannot vouch for the card**
//!   ([`WriteSeq::new`] with `verified` false). The service vouches from the
//!   last guard pass until the firmware next enumerates the port outside the
//!   guard, or the generation ends — so a drain of many blocks is guarded
//!   once, and a card swapped while nothing was draining is caught at the
//!   next drain's first block.
//! - **After a silent phase, and after a read-back the read path gave up
//!   on.** The card may have power-cycled — the LCD path's recovery case,
//!   which suppressing re-enumeration took away — or it may be another card.
//!   The guard tells them apart before a single further byte is written. A
//!   silent *commit* is not a trigger: it goes to the read-back, as above,
//!   and the read-back's own silence is.
//!
//! **The block in hand may be half of the fingerprint.** A save rewrites the
//! FAT, a format the root, and either can land on the card with its
//! acknowledgement lost — any phase's ACK, the commit's, or every capture of
//! the read-back. The card then shows a fingerprint the pull never saw, and a
//! guard that knew only the old one would call the same card another,
//! discard every acknowledged save behind it, and leave the block as it
//! found it. Nor is "landed" one state: each phase puts its own 128 bytes on
//! the card, so a phase that goes silent after earlier ones were taken leaves
//! the block part new, part old. So the sequence carries both:
//! [`Expected::now`], what the pull and the confirmed writes say the card
//! holds, and [`Expected::landing`], the block in hand phase by phase, before
//! and after, with the other half of the fingerprint. Once a phase of it has
//! been put on the bus — never before, so the guard before phase 0 is as
//! strict as it was — a card whose other half is unchanged and whose block
//! in hand is, phase for phase, either the old bytes or the new is this card
//! mid-write; the retry then writes the whole block again and the read-back
//! confirms it.
//!
//! A guard that cannot get an answer — a silent enumeration, a fingerprint
//! block the read path gives up on — spends one of [`GUARD_TRIES`] and starts
//! again; past them the verdict is `Unidentified` too. **No identity, no
//! write** is the rule the guard is built on.
//!
//! What the guard does not cover, and accepts: two cards with the same root
//! and FAT (a clone) are the same card to it; and a swap that lands between
//! the guard's read of 254 and the phase that follows is caught only if the
//! new card is deaf, which is the premise above. The premise passed on
//! hardware on 2026-09-25 (v326): another card docked mid-drain was caught.
//! That was one run; its bus captures were not kept.

use crate::block_bytes::{fnv1a32, BLOCK_BYTES, FNV_OFFSET_BASIS, PHASE_BYTES};

/// Attempts a block gets before it is reported failed.
///
/// Whole sequences, phase 0 to read-back. `KallistiOS` retries a block write
/// four times with 100 ms sleeps (`vmu.c`); four attempts here are ≈ 2.4 s
/// of windows at the production cadence, generous rather than tuned.
pub const WRITE_TRIES: u8 = 4;

/// Attempts the guard gets at identifying the card before it gives up on it.
///
/// Each is an enumeration and two block reads, and each read is already
/// retried by the read path (`read_pipeline::READ_TRIES`), so three here is
/// a bound on a card that answers nothing, not a rate to absorb.
pub const GUARD_TRIES: u8 = 3;

/// Phases in a block's `BLOCK_WRITE`.
pub const PHASES: u8 = 4;

/// The root block: the first half of the fingerprint.
pub const ROOT_BLOCK: u8 = 255;

/// The FAT: the second half of the fingerprint, and the block a save
/// rewrites.
pub const FAT_BLOCK: u8 = 254;

/// A transaction the writer has to put on the bus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tx {
    /// `BLOCK_WRITE`, phase `n` of the block: 128 bytes.
    Phase(u8),
    /// `GET_LAST_ERROR` addressed to phase 4: the commit.
    Commit,
    /// The same frame as [`Tx::Commit`], sent to reset a card that stopped
    /// answering after a failed phase. Its reply is not judged.
    Recover,
    /// The guard's device-info request to the card: what arms it to answer
    /// the reads that follow. [`Reply::Ack`] here means a device-info reply.
    Enumerate,
}

/// How a transaction's reply came back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reply {
    /// The card's `ACK` (`0x07`) — or, to [`Tx::Enumerate`], its device info.
    Ack,
    /// Nothing captured within the timeout.
    Silent,
    /// Any other reply — a `FILE_ERROR`, or something that was not for us.
    Error,
}

/// How the read-back came out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verify {
    /// The block read back byte-for-byte as written.
    Match,
    /// It read back, and differs.
    Mismatch,
    /// The read path gave up on it.
    Unread,
}

/// What became of the block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Read back as written: on the card.
    Written,
    /// The budget is spent. The service ends the generation on this.
    Failed,
    /// The guard found another card in the port, or could not identify the
    /// one there within its budget. Nothing of this block was written after
    /// the guard ran. The service ends the generation on this too, with the
    /// block `DISCARDED` rather than `FAILED`: the card did not refuse it.
    Unidentified,
}

/// Where the guard is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Guard {
    /// The enumeration is owed; the writer takes a window for it.
    Enumerate,
    /// One of the fingerprint blocks is to be read. No window of the writer's
    /// own: the read path serves it.
    Read(u8),
}

/// Where the sequence is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// The swap guard is identifying the card.
    Guard(Guard),
    /// A transaction is owed; the writer takes a window for it.
    Send(Tx),
    /// The block is to be read back. No window of the writer's own: the read
    /// path serves it, and the writer waits for the verdict.
    Verify,
    /// Finished, one way or the other.
    Done(Verdict),
}

/// A block the sequence is waiting on the read path for, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Read {
    /// A fingerprint block for the guard.
    Guard(u8),
    /// The block just written, for the read-back.
    Verify(u8),
}

/// The fingerprint the docked card has to show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Expected {
    /// What the card holds, as far as the service knows: the pull, advanced
    /// by every confirmed write to the root or the FAT.
    pub now: u32,
    /// The block in hand, when it is the root or the FAT; `None` for any
    /// other block. Consulted by the guard only after a phase has been sent.
    pub landing: Option<Landing>,
}

impl Expected {
    /// A block that touches neither half of the fingerprint.
    #[must_use]
    pub const fn unchanged(now: u32) -> Self {
        Self { now, landing: None }
    }
}

/// A fingerprint block in the writer's hands, as the guard needs it to know
/// the card mid-write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Landing {
    /// The half of the fingerprint the block in hand leaves alone, hashed on
    /// its own from [`FNV_OFFSET_BASIS`]: the root while the FAT is in hand,
    /// the FAT while the root is.
    pub other: u32,
    /// The block as the card held it, by [`phase_hashes`].
    pub before: [u32; PHASES as usize],
    /// The block being written, by [`phase_hashes`].
    pub after: [u32; PHASES as usize],
}

impl Landing {
    /// Whether a block, by its [`phase_hashes`], is phase for phase either
    /// what the card held or what is being written — any mix a silent phase
    /// can leave behind.
    #[must_use]
    pub fn admits(&self, shown: &[u32; PHASES as usize]) -> bool {
        shown
            .iter()
            .zip(self.before.iter().zip(self.after.iter()))
            .all(|(h, (b, a))| h == b || h == a)
    }
}

/// FNV-1a of each phase's 128 bytes of a block, in image order.
///
/// Image order only reverses bytes within 4-byte words, so a phase's bytes
/// are the same 128 in either order and the split is the one the card was
/// written in.
#[must_use]
pub fn phase_hashes(block: &[u8; BLOCK_BYTES]) -> [u32; PHASES as usize] {
    let mut out = [0; PHASES as usize];
    for (h, phase) in out.iter_mut().zip(block.chunks_exact(PHASE_BYTES)) {
        *h = fnv1a32(FNV_OFFSET_BASIS, phase);
    }
    out
}

/// What the sequence does once the guard passes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum After {
    /// The guard ran before phase 0: begin.
    Start,
    /// The guard ran after a silent phase: that try is spent, and the next
    /// begins with a recovery — or the budget ends the block.
    Silence,
}

/// One block's sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WriteSeq {
    block: u8,
    step: Step,
    /// Attempts begun, this one included.
    tries: u8,
    /// The read for the current [`Step::Verify`] or [`Guard::Read`] has been
    /// handed to the read path. Cleared when the step is entered and when the
    /// read is lost.
    read_asked: bool,
    expected: Expected,
    /// A phase of this block has been put on the bus, in any attempt: its
    /// bytes may be on the card.
    touched: bool,
    /// FNV-1a state after the guard's read of [`ROOT_BLOCK`], waiting for
    /// [`FAT_BLOCK`].
    seed: u32,
    /// [`phase_hashes`] of the guard's read of [`ROOT_BLOCK`], for a root
    /// in hand. Waits for [`FAT_BLOCK`] with `seed`.
    root_phases: [u32; PHASES as usize],
    /// Guard attempts begun, this one included.
    guard_tries: u8,
    after: After,
    /// The guard passed and nobody has taken that yet.
    guard_passed: bool,
}

impl WriteSeq {
    /// A fresh sequence for `block`. `expected` is the fingerprint the card
    /// must show — the service's `card`, and the block before and after when
    /// it is half of it — and `verified` whether the service can already vouch
    /// for the card; when it cannot, the guard runs before phase 0.
    ///
    /// An `expected.now` of 0 is "not read yet" in the protocol and can
    /// match nothing, so a sequence begun with it ends `Unidentified` the
    /// moment the guard runs. The service does not hand out blocks in that
    /// state; this is what happens if it ever does.
    #[must_use]
    pub const fn new(block: u8, expected: Expected, verified: bool) -> Self {
        Self {
            block,
            step: if verified {
                Step::Send(Tx::Phase(0))
            } else {
                Step::Guard(Guard::Enumerate)
            },
            tries: 1,
            read_asked: false,
            expected,
            touched: false,
            seed: 0,
            root_phases: [0; PHASES as usize],
            guard_tries: 1,
            after: After::Start,
            guard_passed: false,
        }
    }

    #[must_use]
    pub const fn block(&self) -> u8 {
        self.block
    }

    #[must_use]
    pub const fn step(&self) -> Step {
        self.step
    }

    /// Attempts begun so far, the current one included.
    #[must_use]
    pub const fn tries(&self) -> u8 {
        self.tries
    }

    /// Guard attempts begun so far in the current guard run, the current one
    /// included.
    #[must_use]
    pub const fn guard_tries(&self) -> u8 {
        self.guard_tries
    }

    /// The transaction the writer should put on the bus now, if the step is
    /// one that needs the bus.
    #[must_use]
    pub const fn next_tx(&self) -> Option<Tx> {
        match self.step {
            Step::Send(tx) => Some(tx),
            Step::Guard(Guard::Enumerate) => Some(Tx::Enumerate),
            Step::Guard(Guard::Read(_)) | Step::Verify | Step::Done(_) => None,
        }
    }

    /// The read the step is waiting on the read path for, if any.
    #[must_use]
    pub const fn reading(&self) -> Option<Read> {
        match self.step {
            Step::Guard(Guard::Read(b)) => Some(Read::Guard(b)),
            Step::Verify => Some(Read::Verify(self.block)),
            Step::Guard(Guard::Enumerate) | Step::Send(_) | Step::Done(_) => None,
        }
    }

    /// The reply to the transaction [`Self::next_tx`] handed out. Ignored
    /// outside a step that sent one, so a late or duplicated report cannot
    /// move a sequence that has gone on to a read.
    pub const fn note_reply(&mut self, reply: Reply) {
        match self.step {
            Step::Guard(Guard::Enumerate) => match reply {
                Reply::Ack => self.enter_guard_read(ROOT_BLOCK),
                Reply::Silent | Reply::Error => self.spend_guard_try(),
            },
            Step::Send(tx) => match (tx, reply) {
                (Tx::Phase(_), _) if !self.touched => {
                    // The card may hold this block from here on, however the
                    // reply came back; then judge the reply as below.
                    self.touched = true;
                    self.note_reply(reply);
                }
                (Tx::Phase(n), Reply::Ack) => {
                    let next = n + 1;
                    self.step = if next < PHASES {
                        Step::Send(Tx::Phase(next))
                    } else {
                        Step::Send(Tx::Commit)
                    };
                }
                // A lost ACK to the commit is not a lost block: the read-back
                // decides. A lost ACK to a phase is — the card stops answering
                // the sequence — and it is also what a swapped card sounds
                // like, so the guard goes first.
                (Tx::Commit, Reply::Ack | Reply::Silent) => self.enter_verify(),
                (Tx::Phase(_), Reply::Silent) => self.enter_guard(After::Silence),
                (Tx::Phase(_) | Tx::Commit, Reply::Error) => self.spend_try(),
                // The recovery's reply is not judged; phase 0 is the test.
                (Tx::Recover, _) => self.step = Step::Send(Tx::Phase(0)),
                // Not a step that sends this; nothing to judge.
                (Tx::Enumerate, _) => {}
            },
            Step::Guard(Guard::Read(_)) | Step::Verify | Step::Done(_) => {}
        }
    }

    /// The block to hand the read path, exactly once per read-bearing step
    /// entered — and again after [`Self::note_read_lost`]. `None` when the
    /// step is not waiting on a read or the read has already been asked for.
    pub const fn take_read_request(&mut self) -> Option<u8> {
        let block = match self.step {
            Step::Guard(Guard::Read(b)) => b,
            Step::Verify => self.block,
            Step::Guard(Guard::Enumerate) | Step::Send(_) | Step::Done(_) => return None,
        };
        if self.read_asked {
            return None;
        }
        self.read_asked = true;
        Some(block)
    }

    /// The read path dropped the writer's request before answering it — a
    /// clear that took it, or a queue that refused it. Ask again.
    pub const fn note_read_lost(&mut self) {
        self.read_asked = false;
    }

    /// The read-back's outcome. Ignored outside [`Step::Verify`].
    pub const fn note_verify(&mut self, v: Verify) {
        if !matches!(self.step, Step::Verify) {
            return;
        }
        match v {
            Verify::Match => self.step = Step::Done(Verdict::Written),
            // The card answered with the wrong bytes: it is the enumerated
            // one, and the guard has nothing to add.
            Verify::Mismatch => self.spend_try(),
            // Nothing answered: a swapped card sounds like this.
            Verify::Unread => self.enter_guard(After::Silence),
        }
    }

    /// A fingerprint block the read path delivered, in image order. Ignored
    /// unless the guard is waiting for exactly this block.
    pub fn note_guard_block(&mut self, block: u8, data: &[u8; BLOCK_BYTES]) {
        if self.step != Step::Guard(Guard::Read(block)) {
            return;
        }
        match block {
            ROOT_BLOCK => {
                self.seed = fnv1a32(FNV_OFFSET_BASIS, data);
                self.root_phases = phase_hashes(data);
                self.enter_guard_read(FAT_BLOCK);
            }
            FAT_BLOCK => {
                let shown = fnv1a32(self.seed, data);
                if shown == self.expected.now || self.mid_write(data) {
                    self.guard_passed = true;
                    match self.after {
                        After::Start => self.step = Step::Send(Tx::Phase(0)),
                        After::Silence => self.spend_try(),
                    }
                } else {
                    self.step = Step::Done(Verdict::Unidentified);
                }
            }
            _ => {}
        }
    }

    /// The read path gave up on a fingerprint block. Ignored unless the guard
    /// is waiting for exactly this block.
    pub const fn note_guard_unread(&mut self, block: u8) {
        if !matches!(self.step, Step::Guard(Guard::Read(b)) if b == block) {
            return;
        }
        self.spend_guard_try();
    }

    /// Whether the guard has passed since this was last asked. The caller
    /// tells the service, which vouches for the card from here until the
    /// port is next enumerated outside the guard.
    pub const fn take_guard_pass(&mut self) -> bool {
        let passed = self.guard_passed;
        self.guard_passed = false;
        passed
    }

    /// The card the guard just read is this one with the block in hand
    /// partly or wholly on it: the other half of the fingerprint unchanged,
    /// the block in hand admitted phase by phase. Only once a phase has gone
    /// out — before that the block cannot have moved.
    fn mid_write(&self, fat: &[u8; BLOCK_BYTES]) -> bool {
        let Some(landing) = self.expected.landing else {
            return false;
        };
        if !self.touched {
            return false;
        }
        match self.block {
            FAT_BLOCK => landing.other == self.seed && landing.admits(&phase_hashes(fat)),
            ROOT_BLOCK => {
                landing.admits(&self.root_phases) && landing.other == fnv1a32(FNV_OFFSET_BASIS, fat)
            }
            _ => false,
        }
    }

    const fn enter_guard(&mut self, after: After) {
        self.after = after;
        self.guard_tries = 1;
        self.step = Step::Guard(Guard::Enumerate);
    }

    const fn enter_guard_read(&mut self, block: u8) {
        self.step = Step::Guard(Guard::Read(block));
        self.read_asked = false;
    }

    const fn enter_verify(&mut self) {
        self.step = Step::Verify;
        self.read_asked = false;
    }

    /// This guard attempt got no identity. Another begins with a fresh
    /// enumeration, or the budget is spent and the block is unidentified.
    const fn spend_guard_try(&mut self) {
        if self.guard_tries >= GUARD_TRIES {
            self.step = Step::Done(Verdict::Unidentified);
        } else {
            self.guard_tries += 1;
            self.step = Step::Guard(Guard::Enumerate);
        }
    }

    /// This attempt is lost. Another begins with a recovery, or the budget is
    /// spent and the block is failed.
    const fn spend_try(&mut self) {
        if self.tries >= WRITE_TRIES {
            self.step = Step::Done(Verdict::Failed);
        } else {
            self.tries += 1;
            self.step = Step::Send(Tx::Recover);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLK: u8 = 42;

    fn root() -> [u8; BLOCK_BYTES] {
        [0x10; BLOCK_BYTES]
    }

    fn fat() -> [u8; BLOCK_BYTES] {
        [0x20; BLOCK_BYTES]
    }

    /// The fingerprint of the card `root` and `fat` describe, as the service
    /// computes it from the dongle's pull.
    fn card() -> u32 {
        fnv1a32(fnv1a32(FNV_OFFSET_BASIS, &root()), &fat())
    }

    /// A sequence whose card the service can vouch for: no guard first.
    fn verified() -> WriteSeq {
        WriteSeq::new(BLK, Expected::unchanged(card()), true)
    }

    /// A sequence for `block` on an unvouched card.
    fn unvouched(block: u8, now: u32) -> WriteSeq {
        WriteSeq::new(block, Expected::unchanged(now), false)
    }

    /// The FAT a save is writing. Each phase differs from the others, so a
    /// test that tears it catches a phase taken from the wrong place.
    fn new_fat() -> [u8; BLOCK_BYTES] {
        let mut b = [0u8; BLOCK_BYTES];
        for (n, phase) in b.chunks_exact_mut(PHASE_BYTES).enumerate() {
            phase.fill(0x21 + u8::try_from(n).unwrap_or(0));
        }
        b
    }

    /// The FAT in hand, as the service describes it to the guard.
    fn fat_landing() -> Landing {
        Landing {
            other: fnv1a32(FNV_OFFSET_BASIS, &root()),
            before: phase_hashes(&fat()),
            after: phase_hashes(&new_fat()),
        }
    }

    /// A vouched-for card, writing the FAT.
    fn writing_fat() -> WriteSeq {
        WriteSeq::new(
            FAT_BLOCK,
            Expected {
                now: card(),
                landing: Some(fat_landing()),
            },
            true,
        )
    }

    /// `new` over `old` through phase `k`: what the card holds once phases
    /// `0..=k` have landed and the rest have not.
    fn torn(old: &[u8; BLOCK_BYTES], new: &[u8; BLOCK_BYTES], k: u8) -> [u8; BLOCK_BYTES] {
        let mut out = *old;
        let end = (usize::from(k) + 1) * PHASE_BYTES;
        out[..end].copy_from_slice(&new[..end]);
        out
    }

    /// Enumerate and serve the guard's reads with `root` and `fat`.
    fn pass_guard_with_both(s: &mut WriteSeq, root: &[u8; BLOCK_BYTES], fat: &[u8; BLOCK_BYTES]) {
        assert_eq!(s.next_tx(), Some(Tx::Enumerate));
        s.note_reply(Reply::Ack);
        assert_eq!(s.take_read_request(), Some(ROOT_BLOCK));
        s.note_guard_block(ROOT_BLOCK, root);
        assert_eq!(s.take_read_request(), Some(FAT_BLOCK));
        s.note_guard_block(FAT_BLOCK, fat);
    }

    /// Enumerate and serve the guard's reads with `root()` and `fat`.
    fn pass_guard_with(s: &mut WriteSeq, fat: &[u8; BLOCK_BYTES]) {
        assert_eq!(s.next_tx(), Some(Tx::Enumerate));
        s.note_reply(Reply::Ack);
        assert_eq!(s.take_read_request(), Some(ROOT_BLOCK));
        s.note_guard_block(ROOT_BLOCK, &root());
        assert_eq!(s.take_read_request(), Some(FAT_BLOCK));
        s.note_guard_block(FAT_BLOCK, fat);
    }

    /// Drive a fresh attempt through its four phases and the commit.
    fn ack_through_commit(s: &mut WriteSeq) {
        for n in 0..PHASES {
            assert_eq!(s.next_tx(), Some(Tx::Phase(n)));
            s.note_reply(Reply::Ack);
        }
        assert_eq!(s.next_tx(), Some(Tx::Commit));
        s.note_reply(Reply::Ack);
    }

    /// Serve the guard's reads with the blocks of `card()`, as the read path
    /// would, checking it asks for each exactly once and in order.
    fn serve_guard_reads(s: &mut WriteSeq) {
        assert_eq!(s.reading(), Some(Read::Guard(ROOT_BLOCK)));
        assert_eq!(s.take_read_request(), Some(ROOT_BLOCK));
        assert_eq!(s.take_read_request(), None, "asked once");
        s.note_guard_block(ROOT_BLOCK, &root());
        assert_eq!(s.reading(), Some(Read::Guard(FAT_BLOCK)));
        assert_eq!(s.take_read_request(), Some(FAT_BLOCK));
        assert_eq!(s.take_read_request(), None);
        s.note_guard_block(FAT_BLOCK, &fat());
    }

    /// Enumerate and serve the reads: the whole guard, passing.
    fn pass_guard(s: &mut WriteSeq) {
        assert_eq!(s.next_tx(), Some(Tx::Enumerate));
        s.note_reply(Reply::Ack);
        serve_guard_reads(s);
    }

    // ------------------------------------------------------------ the write --

    #[test]
    fn four_acked_phases_and_an_acked_commit_lead_to_the_read_back() {
        let mut s = verified();
        ack_through_commit(&mut s);
        assert_eq!(s.step(), Step::Verify);
        assert_eq!(s.next_tx(), None, "the read path serves the verify");
        assert_eq!(s.reading(), Some(Read::Verify(BLK)));
        assert_eq!(s.take_read_request(), Some(BLK));
        assert_eq!(s.take_read_request(), None, "asked once");
    }

    #[test]
    fn a_matching_read_back_is_written() {
        let mut s = verified();
        ack_through_commit(&mut s);
        s.note_verify(Verify::Match);
        assert_eq!(s.step(), Step::Done(Verdict::Written));
        assert_eq!(s.tries(), 1);
    }

    #[test]
    fn a_refused_phase_starts_over_without_the_guard() {
        let mut s = verified();
        s.note_reply(Reply::Error);
        assert_eq!(
            s.next_tx(),
            Some(Tx::Recover),
            "a card that answered is the enumerated one"
        );
        assert_eq!(s.tries(), 2);
        s.note_reply(Reply::Ack);
        assert_eq!(s.next_tx(), Some(Tx::Phase(0)));
    }

    /// The commit's ACK can be lost with the bytes landed, so silence there
    /// is not a failure: the read-back says.
    #[test]
    fn a_silent_commit_is_decided_by_the_read_back() {
        let mut s = verified();
        for _ in 0..PHASES {
            s.note_reply(Reply::Ack);
        }
        assert_eq!(s.next_tx(), Some(Tx::Commit));
        s.note_reply(Reply::Silent);
        assert_eq!(s.step(), Step::Verify, "not the guard");
        s.note_verify(Verify::Match);
        assert_eq!(s.step(), Step::Done(Verdict::Written));
        assert_eq!(s.tries(), 1, "no try was spent on the lost ACK");
    }

    #[test]
    fn a_refused_commit_starts_over() {
        let mut s = verified();
        for _ in 0..PHASES {
            s.note_reply(Reply::Ack);
        }
        s.note_reply(Reply::Error);
        assert_eq!(s.next_tx(), Some(Tx::Recover));
        assert_eq!(s.tries(), 2);
    }

    #[test]
    fn a_mismatching_read_back_starts_over() {
        let mut s = verified();
        ack_through_commit(&mut s);
        s.note_verify(Verify::Mismatch);
        assert_eq!(s.next_tx(), Some(Tx::Recover));
        assert_eq!(s.tries(), 2);
        s.note_reply(Reply::Ack);
        ack_through_commit(&mut s);
        assert_eq!(s.take_read_request(), Some(BLK), "a new verify asks again");
        s.note_verify(Verify::Match);
        assert_eq!(s.step(), Step::Done(Verdict::Written));
    }

    /// An unreadable read-back is a silence: a swapped card sounds like it.
    #[test]
    fn a_card_that_cannot_be_read_back_is_fingerprinted_before_starting_over() {
        let mut s = verified();
        ack_through_commit(&mut s);
        s.note_verify(Verify::Unread);
        assert_eq!(s.next_tx(), Some(Tx::Enumerate));
        assert_eq!(s.tries(), 1);
        pass_guard(&mut s);
        assert_eq!(s.next_tx(), Some(Tx::Recover));
        assert_eq!(s.tries(), 2);
    }

    #[test]
    fn the_budget_ends_in_failed() {
        let mut s = verified();
        for attempt in 1..=WRITE_TRIES {
            assert_eq!(s.tries(), attempt);
            assert_eq!(s.next_tx(), Some(Tx::Phase(0)));
            s.note_reply(Reply::Error);
            if attempt < WRITE_TRIES {
                assert_eq!(s.next_tx(), Some(Tx::Recover));
                s.note_reply(Reply::Silent);
            }
        }
        assert_eq!(s.step(), Step::Done(Verdict::Failed));
        assert_eq!(s.next_tx(), None);
        assert_eq!(s.tries(), WRITE_TRIES);
    }

    #[test]
    fn a_lost_verify_read_is_asked_for_again() {
        let mut s = verified();
        ack_through_commit(&mut s);
        assert_eq!(s.take_read_request(), Some(BLK));
        assert_eq!(s.take_read_request(), None);
        s.note_read_lost();
        assert_eq!(s.take_read_request(), Some(BLK));
    }

    #[test]
    fn reports_outside_their_step_are_ignored() {
        let mut s = verified();
        // A verify verdict before any verify, and guard reports with no guard.
        s.note_verify(Verify::Mismatch);
        s.note_guard_block(ROOT_BLOCK, &root());
        s.note_guard_unread(FAT_BLOCK);
        assert_eq!(s.step(), Step::Send(Tx::Phase(0)));
        assert_eq!(s.take_read_request(), None);
        ack_through_commit(&mut s);
        // A late reply while the read-back is pending.
        s.note_reply(Reply::Error);
        assert_eq!(s.step(), Step::Verify);
        s.note_verify(Verify::Match);
        // Anything after Done.
        s.note_reply(Reply::Silent);
        s.note_verify(Verify::Unread);
        assert_eq!(s.step(), Step::Done(Verdict::Written));
    }

    // ------------------------------------------------------------ the guard --

    #[test]
    fn an_unvouched_card_is_fingerprinted_before_phase_0() {
        let mut s = unvouched(BLK, card());
        assert_eq!(s.step(), Step::Guard(Guard::Enumerate));
        assert_eq!(s.reading(), None);
        assert_eq!(s.take_read_request(), None, "the enumeration comes first");
        pass_guard(&mut s);
        assert_eq!(s.next_tx(), Some(Tx::Phase(0)));
        assert_eq!(s.tries(), 1, "the guard spends no write try");
        assert!(s.take_guard_pass());
        assert!(!s.take_guard_pass(), "taken once");
    }

    #[test]
    fn a_vouched_card_skips_the_guard() {
        let mut s = verified();
        assert_eq!(s.next_tx(), Some(Tx::Phase(0)));
        assert!(!s.take_guard_pass());
    }

    #[test]
    fn a_different_card_is_unidentified() {
        let mut s = unvouched(BLK, card() ^ 1);
        pass_guard(&mut s);
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
        assert_eq!(s.next_tx(), None, "nothing goes onto it");
        assert!(!s.take_guard_pass());
    }

    #[test]
    fn a_silent_phase_fingerprints_before_recovering() {
        let mut s = verified();
        s.note_reply(Reply::Ack);
        s.note_reply(Reply::Ack);
        assert_eq!(s.next_tx(), Some(Tx::Phase(2)));
        s.note_reply(Reply::Silent);
        assert_eq!(s.step(), Step::Guard(Guard::Enumerate));
        assert_eq!(
            s.tries(),
            1,
            "not spent until the card is known to be the same"
        );
        pass_guard(&mut s);
        assert!(s.take_guard_pass());
        assert_eq!(
            s.next_tx(),
            Some(Tx::Recover),
            "the same card: the try is spent, recover"
        );
        assert_eq!(s.tries(), 2);
        s.note_reply(Reply::Silent);
        assert_eq!(s.next_tx(), Some(Tx::Phase(0)));
    }

    #[test]
    fn a_silent_phase_on_another_card_is_unidentified_not_failed() {
        let mut s = verified();
        // Spend every try but the last on refusals, so a plain silence would
        // be the budget's end.
        for _ in 1..WRITE_TRIES {
            s.note_reply(Reply::Error);
            s.note_reply(Reply::Ack);
        }
        assert_eq!(s.tries(), WRITE_TRIES);
        assert_eq!(s.next_tx(), Some(Tx::Phase(0)));
        s.note_reply(Reply::Silent);
        assert_eq!(s.next_tx(), Some(Tx::Enumerate));
        s.note_reply(Reply::Ack);
        s.note_guard_block(ROOT_BLOCK, &root());
        s.note_guard_block(FAT_BLOCK, &[0x21; BLOCK_BYTES]);
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
    }

    #[test]
    fn a_silent_phase_on_the_last_try_of_the_same_card_is_failed() {
        let mut s = verified();
        for _ in 1..WRITE_TRIES {
            s.note_reply(Reply::Error);
            s.note_reply(Reply::Ack);
        }
        s.note_reply(Reply::Silent);
        pass_guard(&mut s);
        assert_eq!(s.step(), Step::Done(Verdict::Failed));
    }

    #[test]
    fn a_silent_enumeration_spends_a_guard_try() {
        let mut s = unvouched(BLK, card());
        for attempt in 1..=GUARD_TRIES {
            assert_eq!(s.guard_tries(), attempt);
            assert_eq!(s.next_tx(), Some(Tx::Enumerate));
            s.note_reply(Reply::Silent);
        }
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
        assert_eq!(s.tries(), 1, "no write try was spent");
    }

    #[test]
    fn a_reply_that_is_not_device_info_spends_a_guard_try_too() {
        let mut s = unvouched(BLK, card());
        s.note_reply(Reply::Error);
        assert_eq!(s.guard_tries(), 2);
        assert_eq!(s.next_tx(), Some(Tx::Enumerate));
    }

    #[test]
    fn an_unreadable_fingerprint_block_spends_a_guard_try() {
        let mut s = unvouched(BLK, card());
        s.note_reply(Reply::Ack);
        assert_eq!(s.take_read_request(), Some(ROOT_BLOCK));
        s.note_guard_unread(ROOT_BLOCK);
        assert_eq!(s.guard_tries(), 2);
        assert_eq!(s.next_tx(), Some(Tx::Enumerate), "a fresh enumeration");
        s.note_reply(Reply::Ack);
        s.note_guard_block(ROOT_BLOCK, &root());
        assert_eq!(s.take_read_request(), Some(FAT_BLOCK));
        s.note_guard_unread(FAT_BLOCK);
        assert_eq!(s.guard_tries(), 3);
        s.note_reply(Reply::Ack);
        s.note_guard_block(ROOT_BLOCK, &root());
        s.note_guard_unread(FAT_BLOCK);
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
    }

    #[test]
    fn a_silent_phase_after_a_passed_guard_runs_it_again_from_one() {
        let mut s = unvouched(BLK, card());
        s.note_reply(Reply::Silent);
        assert_eq!(s.guard_tries(), 2);
        s.note_reply(Reply::Ack);
        serve_guard_reads(&mut s);
        assert_eq!(s.next_tx(), Some(Tx::Phase(0)));
        s.note_reply(Reply::Silent);
        assert_eq!(s.guard_tries(), 1, "a new guard run");
    }

    #[test]
    fn a_lost_guard_read_is_asked_for_again() {
        let mut s = unvouched(BLK, card());
        s.note_reply(Reply::Ack);
        assert_eq!(s.take_read_request(), Some(ROOT_BLOCK));
        assert_eq!(s.take_read_request(), None);
        s.note_read_lost();
        assert_eq!(s.take_read_request(), Some(ROOT_BLOCK));
    }

    #[test]
    fn guard_blocks_out_of_order_or_out_of_step_are_ignored() {
        let mut s = unvouched(BLK, card());
        s.note_reply(Reply::Ack);
        // The FAT before the root, and a block that is neither.
        s.note_guard_block(FAT_BLOCK, &fat());
        s.note_guard_block(BLK, &fat());
        s.note_guard_unread(FAT_BLOCK);
        assert_eq!(s.step(), Step::Guard(Guard::Read(ROOT_BLOCK)));
        // A phase reply while a read is pending.
        s.note_reply(Reply::Silent);
        assert_eq!(s.step(), Step::Guard(Guard::Read(ROOT_BLOCK)));
        serve_guard_reads(&mut s);
        assert_eq!(s.next_tx(), Some(Tx::Phase(0)));
    }

    #[test]
    fn an_expected_fingerprint_of_zero_matches_nothing() {
        let mut s = unvouched(BLK, 0);
        pass_guard(&mut s);
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
    }

    // ------------------------------------- the block in hand is the FAT --

    /// The case found 2026-09-22: the FAT lands, every capture of the
    /// read-back is lost, the retry's first phase is silent, and the card —
    /// the same card — now shows the new fingerprint.
    #[test]
    fn a_landed_fat_whose_read_back_was_lost_is_still_this_card() {
        let mut s = writing_fat();
        ack_through_commit(&mut s);
        s.note_verify(Verify::Unread);
        // The guard after the unreadable read-back sees the FAT it wrote.
        pass_guard_with(&mut s, &new_fat());
        assert_eq!(
            s.next_tx(),
            Some(Tx::Recover),
            "this card, with the block on it"
        );
        assert_eq!(s.tries(), 2);
        // And again after a silent phase of the retry.
        s.note_reply(Reply::Ack);
        s.note_reply(Reply::Silent);
        pass_guard_with(&mut s, &new_fat());
        assert_eq!(s.next_tx(), Some(Tx::Recover));
        assert_eq!(s.tries(), 3);
        // The retry writes the same bytes; the read-back confirms them.
        s.note_reply(Reply::Ack);
        ack_through_commit(&mut s);
        s.note_verify(Verify::Match);
        assert_eq!(s.step(), Step::Done(Verdict::Written));
    }

    /// Phase 3's ACK lost with the block flashed: the guard runs before any
    /// commit, and the card already shows the new FAT.
    #[test]
    fn a_fat_landed_on_a_silent_last_phase_is_still_this_card() {
        let mut s = writing_fat();
        for _ in 0..PHASES - 1 {
            s.note_reply(Reply::Ack);
        }
        assert_eq!(s.next_tx(), Some(Tx::Phase(PHASES - 1)));
        s.note_reply(Reply::Silent);
        pass_guard_with(&mut s, &new_fat());
        assert_eq!(s.next_tx(), Some(Tx::Recover));
    }

    /// The old fingerprint still passes while the FAT is in hand: the block
    /// may not have landed.
    #[test]
    fn an_unlanded_fat_leaves_the_card_as_it_was() {
        let mut s = writing_fat();
        s.note_reply(Reply::Silent);
        pass_guard_with(&mut s, &fat());
        assert_eq!(s.next_tx(), Some(Tx::Recover));
    }

    /// Before a phase has gone out, only the pulled fingerprint identifies
    /// the card: a card that already shows the new FAT is not this one.
    #[test]
    fn the_landed_fingerprint_is_not_accepted_before_a_phase_is_sent() {
        let mut s = WriteSeq::new(
            FAT_BLOCK,
            Expected {
                now: card(),
                landing: Some(fat_landing()),
            },
            false,
        );
        pass_guard_with(&mut s, &new_fat());
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
    }

    #[test]
    fn a_card_that_shows_neither_fingerprint_is_unidentified_with_the_fat_in_hand() {
        let mut s = writing_fat();
        s.note_reply(Reply::Silent);
        pass_guard_with(&mut s, &[0x22; BLOCK_BYTES]);
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
    }

    /// A phase's ACK lost after earlier phases were taken: the card holds the
    /// FAT part new, part old — neither whole fingerprint. It is still this
    /// card, and the retry rewrites the whole block.
    #[test]
    fn a_fat_torn_by_a_silent_phase_is_still_this_card() {
        for k in 0..PHASES - 1 {
            let mut s = writing_fat();
            for _ in 0..k {
                s.note_reply(Reply::Ack);
            }
            assert_eq!(s.next_tx(), Some(Tx::Phase(k)));
            s.note_reply(Reply::Silent);
            pass_guard_with(&mut s, &torn(&fat(), &new_fat(), k));
            assert_eq!(s.next_tx(), Some(Tx::Recover), "torn through phase {k}");
            s.note_reply(Reply::Ack);
            ack_through_commit(&mut s);
            s.note_verify(Verify::Match);
            assert_eq!(s.step(), Step::Done(Verdict::Written));
        }
    }

    /// The same phase mix under a different root is another card.
    #[test]
    fn a_torn_fat_under_another_root_is_unidentified() {
        let mut s = writing_fat();
        s.note_reply(Reply::Ack);
        s.note_reply(Reply::Silent);
        pass_guard_with_both(&mut s, &[0x11; BLOCK_BYTES], &torn(&fat(), &new_fat(), 1));
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
    }

    /// A phase that is neither the old bytes nor the new is not a tear.
    #[test]
    fn a_fat_with_a_foreign_phase_is_unidentified() {
        let mut s = writing_fat();
        s.note_reply(Reply::Ack);
        s.note_reply(Reply::Silent);
        let mut shown = torn(&fat(), &new_fat(), 0);
        shown[3 * PHASE_BYTES] ^= 1;
        pass_guard_with(&mut s, &shown);
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
    }

    /// The root in hand: a format torn by a silent phase, the FAT untouched.
    #[test]
    fn a_root_torn_by_a_silent_phase_is_still_this_card() {
        let new_root = [0x11; BLOCK_BYTES];
        let expected = Expected {
            now: card(),
            landing: Some(Landing {
                other: fnv1a32(FNV_OFFSET_BASIS, &fat()),
                before: phase_hashes(&root()),
                after: phase_hashes(&new_root),
            }),
        };
        let mut s = WriteSeq::new(ROOT_BLOCK, expected, true);
        s.note_reply(Reply::Ack);
        s.note_reply(Reply::Silent);
        pass_guard_with_both(&mut s, &torn(&root(), &new_root, 1), &fat());
        assert_eq!(s.next_tx(), Some(Tx::Recover));

        // The same torn root over another FAT is another card.
        let mut s = WriteSeq::new(ROOT_BLOCK, expected, true);
        s.note_reply(Reply::Silent);
        pass_guard_with_both(&mut s, &torn(&root(), &new_root, 0), &new_fat());
        assert_eq!(s.step(), Step::Done(Verdict::Unidentified));
    }
}
