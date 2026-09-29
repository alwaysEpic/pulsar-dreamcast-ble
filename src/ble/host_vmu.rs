// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Where the host storage service lives on this device: one
//! [`HostIo`] behind a mutex, and the callers that reach it.
//!
//! the [`super::host_lcd`] is the pattern — a static behind a
//! `ThreadModeRawMutex`, written by the GATT dispatch inside `ble_task` and
//! read by the main poll loop, with **nothing here touching the Maple bus**.
//! This one carries traffic in both directions, so the poll loop hands it a
//! decoded block and the BLE task notifies it out phase by phase; the write
//! queue runs the other way, the BLE task filling it and the poll loop
//! draining it.
//!
//! The rules — one outstanding read, idle-only serving, terminal answers
//! across a generation change, the fingerprint, the queue and what survives a
//! link drop — are all in [`maple_protocol::host_vmu_io`], which is where they
//! can be tested. This file is the lock, the clock and the log line, and it
//! should stay that small: a rule that grows here is a rule nothing exercises.
//!
//! # The lifecycle, from this side
//!
//! The service outlives the connection and, while it is draining, the poll
//! loop too: `main` keeps the rail up through a link drop until the queue is
//! empty, so the generation the dongle knows is still the one it finds when
//! it reconnects. The poll loop says three things to make that true —
//! [`link_down`] the moment the link goes, [`unwatched`] the moment it stops
//! looking at the port, and [`draining`] whenever it decides whether to leave,
//! sleep or reboot.
//!
//! The writer is `maple::block_write`, driven from the same poll loop: it
//! takes a block with [`take_write`] — and with it [`expected`], the
//! fingerprint the guard compares against, and [`card_verified`], whether the
//! guard has to run first — asks [`writing`] at every window head whether that block is
//! still its own, and reports [`publish_written`], [`publish_write_failed`]
//! or [`publish_unidentified`]. A guard pass comes back as
//! [`note_card_verified`]; the poll loop's own enumeration of the port, when
//! it runs, as [`note_enumerated`]. The queue is opened to the dongle
//! ([`open_writes`]) at boot wherever the writer is compiled; on the DK it
//! stays closed — `queue_free` reads 0, which is the protocol's own way of
//! telling the dongle to send nothing. `main.rs` has why.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::Instant;

use maple_protocol::block_bytes::BLOCK_BYTES;
use maple_protocol::host_vmu_io::{HostIo, Stats, MSG_MAX};
use maple_protocol::read_pipeline::Stage;
use maple_protocol::write_seq::Expected;

/// `ThreadModeRawMutex` for `host_lcd`'s reason: the GATT callback and the main
/// poll loop are two tasks on one thread-mode executor, so this is a handoff
/// between things that cannot preempt each other — and it asserts if either
/// side is ever moved into an interrupt.
static IO: Mutex<ThreadModeRawMutex, RefCell<HostIo>> = Mutex::new(RefCell::new(HostIo::new()));

// ------------------------------------------------------------- the BLE task --

/// Take one write on the down characteristic (`…0004`).
pub fn accept_write(data: &[u8]) {
    IO.lock(|cell| cell.borrow_mut().accept_write(data));
}

/// The host subscribed to `…0003`.
pub fn note_subscribed() {
    IO.lock(|cell| cell.borrow_mut().note_subscribed());
}

/// Offer the next message to `f`, which notifies it and says whether that
/// worked. Returns whether a message was consumed.
///
/// Nothing advances unless `f` returns true — `HostIo::advance` has why a
/// dropped phase would strand the dongle.
///
/// # `f` runs while the cell is borrowed, and why that is not a re-entrancy bug
///
/// `f` notifies, and a re-entrant call back into this module would hit a
/// `RefCell` already borrowed — which on this target is a panic, not an error.
/// It cannot happen: `sd_ble_gatts_hvx` is synchronous and does not dispatch
/// events, GATT writes reach [`accept_write`] through `gatt_server::run` on
/// this same single-threaded executor, and there is no `await` inside the
/// borrow for that future to be polled at. Adding one here would be the bug.
pub fn with_next<F>(f: F) -> bool
where
    F: FnOnce(&[u8]) -> bool,
{
    IO.lock(|cell| {
        let mut io = cell.borrow_mut();
        let mut buf = [0u8; MSG_MAX];
        let Some(len) = io.next_message(&mut buf) else {
            return false;
        };
        if !f(&buf[..len]) {
            return false;
        }
        io.advance();
        true
    })
}

// ------------------------------------------------------------ the poll loop --

/// Open the queue to the dongle: report its room and take writes. Once, at
/// boot, wherever the writer is compiled — see the module doc.
pub fn open_writes() {
    IO.lock(|cell| cell.borrow_mut().open_writes());
}

/// Drop everything, at the *start* of a session.
///
/// The seed is read here rather than passed in: a caller should not have to
/// know how an epoch is seeded to start a session correctly. `HostIo::reset`
/// has what it is for.
pub fn reset() {
    let seed = u32::try_from(Instant::now().as_ticks() & u64::from(u32::MAX)).unwrap_or(1);
    IO.lock(|cell| cell.borrow_mut().reset(seed));
}

/// Tell the service whether the pad is being used.
pub fn set_idle(idle: bool) {
    IO.lock(|cell| cell.borrow_mut().set_idle(idle));
}

/// Tell the service whether a VMU is docked. A change ends the generation.
pub fn set_vmu_present(present: bool) {
    IO.lock(|cell| cell.borrow_mut().set_vmu_present(present));
}

/// The link went away.
///
/// Drops what was owed to that host — the read, its half-sent reply,
/// undelivered ACKs — and keeps the generation, the queue and every WRITTEN
/// owed. Called at the drop itself, whether or not the poll loop then stays
/// to drain; `reset` at the next connection repeats it harmlessly.
pub fn link_down() {
    IO.lock(|cell| cell.borrow_mut().link_down());
}

/// The poll loop stopped watching the port.
///
/// The rail is going down, or the loop is leaving for a phase that does not
/// probe presence. A card that was not watched may have been swapped, so this
/// ends the generation exactly as an undock does — "unobserved counts as
/// changed". The next session's first probe reports
/// presence afresh.
pub fn unwatched() {
    IO.lock(|cell| cell.borrow_mut().set_vmu_present(false));
}

/// Acked blocks not yet on the card — the storage lease. While true, the poll
/// loop holds the rail through a link drop, defers the sleep hold and the
/// inactivity timeout, and refuses the update gesture.
#[must_use]
pub fn draining() -> bool {
    IO.lock(|cell| cell.borrow().draining())
}

/// Close write admission for a session-ending gesture unless saves are
/// draining. `false`: refuse the gesture. See
/// [`HostIo::pause_writes_unless_draining`].
pub fn pause_writes_unless_draining() -> bool {
    IO.lock(|cell| cell.borrow_mut().pause_writes_unless_draining())
}

/// The next block for the writer, in drain order, copied into `out`.
///
/// `None` while one is in the writer's hands, and while no card is docked.
pub fn take_write(out: &mut [u8; BLOCK_BYTES]) -> Option<u8> {
    IO.lock(|cell| cell.borrow_mut().take_write(out))
}

/// The block in the writer's hands under the current generation, if any. The
/// writer abandons a block this no longer names: the generation ended under
/// it and the slot already says `DISCARDED`.
#[must_use]
pub fn writing() -> Option<u8> {
    IO.lock(|cell| cell.borrow().writing())
}

/// The writer confirmed the block on the card: its `WRITTEN OK` is now owed.
pub fn publish_written(block: u8) {
    IO.lock(|cell| cell.borrow_mut().publish_written(block));
}

/// The card refused the block within the writer's budget. Its WRITTEN says
/// `FAILED`, and the generation ends.
pub fn publish_write_failed(block: u8) {
    crate::log!(
        "VMUIO: block {} refused by the card — generation ends",
        block
    );
    IO.lock(|cell| cell.borrow_mut().publish_write_failed(block));
}

/// The guard found another card in the port, or none it could identify.
/// Nothing was written after it ran; the generation ends with the block
/// `DISCARDED`.
pub fn publish_unidentified(block: u8) {
    crate::log!(
        "VMUIO: block {} — the card is not the one it was staged for; generation ends",
        block
    );
    IO.lock(|cell| cell.borrow_mut().publish_unidentified(block));
}

/// What the guard compares the docked card against for the block just taken.
///
/// The fingerprint as it stands (0 until the dongle has pulled both of its
/// blocks this generation) and, for the root or the FAT, that block before
/// and after, phase by phase, with the half it leaves alone. Read in the same
/// window as [`take_write`].
#[must_use]
pub fn expected() -> Expected {
    IO.lock(|cell| cell.borrow().expected())
}

/// Whether the guard has shown the docked card to be the one [`card`]
/// describes since the port was last enumerated outside the guard. The
/// writer runs the guard before phase 0 when this is false.
#[must_use]
pub fn card_verified() -> bool {
    IO.lock(|cell| cell.borrow().card_verified())
}

/// The writer's guard passed: the service vouches for the card from here.
pub fn note_card_verified() {
    IO.lock(|cell| cell.borrow_mut().note_card_verified());
}

/// The poll loop sent the card a device-info request outside the guard.
/// Whatever is in the port answers the bus now; the guard looks again
/// before the next block.
pub fn note_enumerated() {
    IO.lock(|cell| cell.borrow_mut().note_enumerated());
}

/// The block to issue now, if there is one and the pad is idle.
pub fn take_request() -> Option<u8> {
    IO.lock(|cell| cell.borrow_mut().take_request())
}

/// Input resumed with a request in the reader's hands: take it back so the
/// caller can clear the pipeline.
pub fn recall() -> bool {
    IO.lock(|cell| cell.borrow_mut().recall())
}

/// A block came back. Queues its four `81 DATA` phases.
pub fn publish_block(block: u8, data: &[u8; BLOCK_BYTES]) {
    IO.lock(|cell| cell.borrow_mut().publish_block(block, data));
}

/// A block failed every attempt.
///
/// The stage is not on the wire — protocol v1 has one failure code — but it is
/// the useful half of the event, so it is logged here rather than discarded at
/// the call site.
pub fn publish_failure(block: u8, stage: Stage) {
    let _ = stage;
    crate::log!("VMUIO: block {} failed at stage {:?}", block, stage);
    IO.lock(|cell| cell.borrow_mut().publish_failure(block));
}

/// The counters, for whatever reads them next — a telemetry tag, a bench
/// build's screen, the write path's own status.
#[must_use]
pub fn stats() -> Stats {
    IO.lock(|cell| cell.borrow().stats())
}
