// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Ingress for VMU LCD frames pushed by a console-side dongle over the vendor
//! GATT service.
//!
//! The BLE task hands every write on the frame characteristic to
//! [`accept_write`]; the main poll loop drains the newest complete frame with
//! [`take_frame`] and sends it down the same PWM/EasyDMA path the animation
//! uses. **Nothing here touches the Maple bus** — the quiet-window pacer in
//! `main` owns every transaction. A BLE-side write into the bus races that
//! pacer and corrupts the transaction in flight — a failure that has been paid
//! for once already and must not be reintroduced.
//!
//! # Two wire shapes, chosen by the sender
//!
//! A frame is 192 bytes and only fits one ATT write command when the negotiated
//! MTU is at least 195, so senders that got a smaller MTU cut it into four
//! writes of a leading row offset (0, 8, 16, 24) plus that quarter's 48 bytes.
//! Both shapes are accepted, told apart by length alone — 192 or 49 — so the
//! sender never has to announce which one it is using; a sender picks by the
//! negotiated MTU (`scripts/lcd_push.py` does).
//!
//! # Why the pending flag lives inside the cell
//!
//! The first design sketched a static double buffer plus an `AtomicBool`. The buffer
//! pair is real and load-bearing — chunked frames are assembled in `stage` and
//! only a *complete* frame is copied to `frame`, so the main loop can never
//! draw three quarters of one frame and one of the next. The flag, though, is
//! kept in the same cell as the buffer it describes rather than beside it: a
//! separate atomic would be a second thing to publish, and the ordering between
//! "the bytes are there" and "there is a frame" is the whole correctness
//! argument.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::blocking_mutex::Mutex;

use crate::vmu::{LCD_BYTES, LCD_WIDTH};

/// Bytes in one LCD row (48 px, 1 bpp).
const ROW_BYTES: usize = LCD_WIDTH / 8;
/// Rows carried by one chunk of the four-write shape.
pub const CHUNK_ROWS: usize = 8;
/// Frame bytes carried by one chunk.
pub const CHUNK_BYTES: usize = CHUNK_ROWS * ROW_BYTES;
/// A chunk write: the row offset byte, then the chunk.
pub const CHUNK_WRITE_LEN: usize = CHUNK_BYTES + 1;
/// Chunks in a frame.
const CHUNKS: usize = LCD_BYTES / CHUNK_BYTES;
/// All chunks of a frame seen.
const ALL_CHUNKS: u8 = (1 << CHUNKS) - 1;

/// The double buffer and its flag. See the module note for why they share a
/// cell.
struct Ingress {
    /// Partial frame under assembly (four-write shape only).
    stage: [u8; LCD_BYTES],
    /// Which quarters of `stage` have arrived since the frame started.
    have: u8,
    /// The newest *complete* frame. Only ever a whole frame from one sender
    /// pass — never a mix of two.
    frame: [u8; LCD_BYTES],
    /// `frame` holds something the main loop has not drawn yet.
    pending: bool,
}

impl Ingress {
    const fn new() -> Self {
        Self {
            stage: [0; LCD_BYTES],
            have: 0,
            frame: [0; LCD_BYTES],
            pending: false,
        }
    }
}

/// `ThreadModeRawMutex` for the same reason `RAW_CONTROLLER_STATE` uses it: the
/// GATT write callback runs inside `ble_task` and the consumer is the main poll
/// loop, both on Embassy's one thread-mode executor, so this is a handoff
/// between two tasks that cannot preempt each other — no interrupt masking on
/// a path that runs while the radio is live. It also asserts if that invariant
/// is ever broken by moving either side into an interrupt.
static INGRESS: Mutex<ThreadModeRawMutex, RefCell<Ingress>> =
    Mutex::new(RefCell::new(Ingress::new()));

/// Take one write on the frame characteristic.
///
/// Called from the BLE task's GATT event dispatch. A write that is neither
/// shape, or a chunk with an offset that is not a chunk boundary, is dropped
/// silently: this is an open vendor characteristic on a device that must keep
/// playing whatever a host writes to it.
///
/// Frames are *replaced*, never queued — a sender faster than the LCD slot
/// cadence simply loses the frames in between, which is what a display wants.
pub fn accept_write(data: &[u8]) {
    INGRESS.lock(|cell| {
        let mut ing = cell.borrow_mut();
        match data.len() {
            LCD_BYTES => {
                ing.frame.copy_from_slice(data);
                // A whole frame supersedes anything half-assembled.
                ing.have = 0;
                ing.pending = true;
            }
            CHUNK_WRITE_LEN => {
                let row = usize::from(data[0]);
                if row % CHUNK_ROWS != 0 {
                    return;
                }
                let index = row / CHUNK_ROWS;
                if index >= CHUNKS {
                    return;
                }
                let at = index * CHUNK_BYTES;
                ing.stage[at..at + CHUNK_BYTES].copy_from_slice(&data[1..]);
                // Offset 0 starts a frame, so a sender that drops a chunk
                // resynchronises on its next pass instead of publishing a
                // frame stitched from two.
                ing.have = if index == 0 {
                    1
                } else {
                    ing.have | (1 << index)
                };
                if ing.have == ALL_CHUNKS {
                    ing.frame = ing.stage;
                    ing.have = 0;
                    ing.pending = true;
                }
            }
            _ => {}
        }
    });
}

/// Drop anything not yet drawn. Call when a session *starts*.
///
/// `INGRESS` is a static and outlives the connection that filled it. Without
/// this, a frame written but not yet drained — or a part-assembled set of
/// chunks — crosses into the *next* session, so a new host's first poll draws
/// the previous host's art. That also latches screen ownership for the whole
/// session, because ownership is held until the link drops (see `main.rs`);
/// under the 10 s hold-off it replaced, stale art merely aged out.
///
/// Called on the way *in* rather than the way out, because a session has more
/// than one exit: the poll loop's disconnect is the obvious one, but Phase 2
/// leaves through `controller_found == false` when the link drops during
/// controller detection and reaches none of that teardown. One call at entry
/// covers every path, including any added later.
///
/// `frame` and `stage` are deliberately left alone: `pending == false` means
/// `frame` is never read, and `have == 0` means `stage` is not consulted until
/// a fresh offset-0 write starts a frame. Zeroing 384 bytes on a path that
/// already blocks on an LCD write buys nothing.
pub fn reset() {
    INGRESS.lock(|cell| {
        let mut ing = cell.borrow_mut();
        ing.have = 0;
        ing.pending = false;
    });
}

/// Move the newest complete frame into `frame`, if there is one.
///
/// Returns `false` and leaves `frame` untouched when nothing has arrived since
/// the last call.
pub fn take_frame(frame: &mut [u8; LCD_BYTES]) -> bool {
    INGRESS.lock(|cell| {
        let mut ing = cell.borrow_mut();
        if !ing.pending {
            return false;
        }
        *frame = ing.frame;
        ing.pending = false;
        true
    })
}
