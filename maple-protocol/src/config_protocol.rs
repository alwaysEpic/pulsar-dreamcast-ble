// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Configuration-link protocol core (ADR-017; remap design v2 §4.2–§4.5).
//!
//! The single-flight, two-phase state machine behind the `Control` and `Map`
//! characteristics, pure and host-testable. The firmware's GATT callbacks
//! hand writes to [`ConfigProtocol`]; the config task executes the
//! [`Action`]s it returns — send a notification, perform the armed
//! preview/flash work then call [`ConfigProtocol::finish_work`], or reset
//! back to the HID personality. Nothing here touches hardware, so every row
//! of the state×command table is a unit test.
//!
//! Completion is real: `Complete` is only produced by `finish_work`, after
//! the preview is live or the flash write has been read back — never from
//! the synchronous write path.

use crate::controller_state::ControllerState;
use crate::remap::RemapTable;
use crate::xbox_hid::GamepadReport;

/// Protocol version spoken over `Info`.
pub const PROTO_MAJOR: u8 = 1;
pub const PROTO_MINOR: u8 = 0;

/// No `Map` write within this window of `Ready` releases the armed slot
/// (design v2 §4.3, status `ArmTimeout`).
pub const ARM_TIMEOUT_MS: u64 = 5_000;
/// Idle deadline, refreshed by any `Control` write, `Ping` included (§4.5).
pub const IDLE_DEADLINE_MS: u64 = 120_000;
/// Absolute session ceiling from connection; nothing extends it (§4.5).
pub const ABSOLUTE_CEILING_MS: u64 = 600_000;

/// Request/response opcodes (§4.3). A response echoes the request opcode
/// with [`opcode::RESPONSE_FLAG`] set; a `Map` write has no opcode of its
/// own and completes with [`opcode::COMPLETE`].
pub mod opcode {
    pub const BEGIN: u8 = 0x01;
    pub const ABORT: u8 = 0x02;
    pub const RESET_DEFAULTS: u8 = 0x03;
    pub const REVERT: u8 = 0x04;
    pub const PING: u8 = 0x05;
    pub const EXIT: u8 = 0x06;
    pub const RESPONSE_FLAG: u8 = 0x80;
    pub const COMPLETE: u8 = 0x90;
}

/// Status codes carried in the last response byte (§4.3).
pub mod status {
    pub const OK: u8 = 0x00;
    pub const BUSY: u8 = 0x01;
    pub const NOT_ARMED: u8 = 0x02;
    pub const INVALID: u8 = 0x03;
    pub const FLASH: u8 = 0x04;
    pub const BAD_LENGTH: u8 = 0x05;
    pub const BAD_OP: u8 = 0x06;
    pub const ARM_TIMEOUT: u8 = 0x07;
    pub const SEQ_MISMATCH: u8 = 0x08;
}

/// The operation a `Begin` arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Preview,
    Commit,
}

impl Op {
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Preview),
            0x02 => Some(Self::Commit),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Preview => 0x01,
            Self::Commit => 0x02,
        }
    }
}

/// A 5-byte `Control` notification:
/// `[opcode | 0x80, seq_lo, seq_hi, arg, status]`.
pub type Response = [u8; 5];

const fn response(op: u8, seq: u16, arg: u8, code: u8) -> Response {
    let seq = seq.to_le_bytes();
    [op, seq[0], seq[1], arg, code]
}

/// The work `finish_work` settles.
///
/// `Preview` has no external side effect — the task calls `finish_work`
/// once the `LiveOutput` producer will observe the candidate;
/// `Commit`/`ResetDefaults` append to the journal and read back first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    Preview,
    Commit,
    ResetDefaults,
}

/// What the state machine asks the driving task to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Send this `Control` notification; nothing else changes.
    Notify(Response),
    /// Enter the operation on [`ConfigProtocol::working_map`], then call
    /// [`ConfigProtocol::finish_work`]. No notification until then.
    StartWork(WorkKind),
    /// Send the notification, then reset to the HID personality after one
    /// connection event (~100 ms).
    NotifyThenReset(Response),
    /// An `Exit` arrived while an operation is in flight: nothing to send
    /// now — `finish_work` will carry the deferred `ExitAck` (§4.3, "Exit
    /// during Working lets the flash operation settle first").
    ExitDeferred,
}

/// `finish_work`'s outcome: the `Complete` (or `ResetDefaults`) response,
/// plus the deferred `ExitAck` if an `Exit` arrived mid-operation — send
/// both in order, then reset.
#[derive(Debug, PartialEq, Eq)]
pub struct WorkDone {
    pub complete: Response,
    pub exit_ack: Option<Response>,
}

/// A deadline fired (from [`ConfigProtocol::poll`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Expiry {
    /// The armed transaction timed out: send this `Complete(ArmTimeout)`;
    /// the slot is already released.
    ArmTimeout(Response),
    /// Idle deadline or absolute ceiling: end the session, reset to HID.
    /// The firmware lets any in-flight flash operation settle first.
    SessionEnd,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Idle,
    Armed {
        seq: u16,
        op: Op,
        deadline_ms: u64,
    },
    Working {
        seq: u16,
        kind: WorkKind,
        /// `COMPLETE` for a Map transaction, `RESET_DEFAULTS | 0x80` for a
        /// `ResetDefaults` (§4.3: it completes with `0x83`).
        complete_opcode: u8,
        /// The `arg` echoed in the completion (the armed op, or 0).
        complete_arg: u8,
    },
}

/// The protocol state machine plus the maps it arbitrates: `stored` (the
/// effective durable map `StoredMap` reports), `candidate` (an applied
/// preview) and the immutable in-flight snapshot.
pub struct ConfigProtocol {
    state: State,
    stored: RemapTable,
    candidate: Option<RemapTable>,
    working: Option<RemapTable>,
    /// Sequence of an `Exit` deferred behind an in-flight operation.
    exit_seq: Option<u16>,
    session_start_ms: u64,
    last_activity_ms: u64,
}

impl ConfigProtocol {
    /// `stored` is the journal's effective map at session start; `now_ms`
    /// anchors both session deadlines.
    #[must_use]
    pub const fn new(stored: RemapTable, now_ms: u64) -> Self {
        Self {
            state: State::Idle,
            stored,
            candidate: None,
            working: None,
            exit_seq: None,
            session_start_ms: now_ms,
            last_activity_ms: now_ms,
        }
    }

    /// The effective durable map — what `StoredMap` reads and the next HID
    /// boot will load. Never a preview.
    #[must_use]
    pub const fn stored_map(&self) -> &RemapTable {
        &self.stored
    }

    /// The map `LiveOutput` is computed under: the candidate while
    /// previewing, else the stored map.
    #[must_use]
    pub fn active_map(&self) -> &RemapTable {
        self.candidate.as_ref().unwrap_or(&self.stored)
    }

    /// The immutable snapshot of the armed transaction's map, present
    /// between a consumed `Map` write and `finish_work`.
    #[must_use]
    pub const fn working_map(&self) -> Option<&RemapTable> {
        self.working.as_ref()
    }

    /// The 10-byte `Info` payload (§4.2). `profile_id` and `source` (where
    /// the stored record came from, `prefs_journal::source`) are the
    /// firmware's to supply.
    #[must_use]
    pub fn info(&self, profile_id: u8, source: u8) -> [u8; 10] {
        let mut flags = 0u8;
        if !matches!(self.state, State::Idle) {
            flags |= 0x01; // transaction armed (or in flight)
        }
        if self.candidate.is_some_and(|c| c != self.stored) {
            flags |= 0x02; // dirty: candidate differs from stored
        }
        if self.stored == RemapTable::DEFAULT {
            flags |= 0x04;
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "120_000 ms / 1000 = 120 and 600_000 ms / 60_000 = 10 both fit u8; \
                      the constants are protocol ABI"
        )]
        [
            PROTO_MAJOR,
            PROTO_MINOR,
            flags,
            profile_id,
            RemapTable::LEN as u8,
            RemapTable::LEN as u8, // att_payload: every payload fits MTU 23
            crate::prefs_journal::SCHEMA,
            source,
            (IDLE_DEADLINE_MS / 1000) as u8,
            (ABSOLUTE_CEILING_MS / 60_000) as u8,
        ]
    }

    /// A write to the `Control` characteristic. Any such write refreshes
    /// the idle deadline (§4.5); so does a `Map` write — activity is
    /// activity, and every `Map` is preceded by a `Begin` anyway.
    pub fn on_control(&mut self, payload: &[u8], now_ms: u64) -> Action {
        self.last_activity_ms = now_ms;

        // Length-preserving values make this reachable: a short write is a
        // short slice, never zero-padded into a valid command (§4.2 — a
        // padded [0x05] would be a valid Ping(0)).
        if payload.len() != 4 {
            return Action::Notify(response(opcode::COMPLETE, 0, 0, status::BAD_LENGTH));
        }
        let seq = u16::from_le_bytes([payload[1], payload[2]]);
        let arg = payload[3];

        match payload[0] {
            opcode::BEGIN => self.begin(seq, arg, now_ms),
            opcode::ABORT => self.abort(seq),
            opcode::RESET_DEFAULTS => self.reset_defaults(seq),
            opcode::REVERT => self.revert(seq),
            // Ping answers in every state (§4.3) and has already refreshed
            // the idle deadline above.
            opcode::PING => Action::Notify(response(
                opcode::PING | opcode::RESPONSE_FLAG,
                seq,
                0,
                status::OK,
            )),
            opcode::EXIT => self.exit(seq),
            other => Action::Notify(response(
                other | opcode::RESPONSE_FLAG,
                seq,
                arg,
                status::BAD_OP,
            )),
        }
    }

    const fn begin(&mut self, seq: u16, arg: u8, now_ms: u64) -> Action {
        let ready = opcode::BEGIN | opcode::RESPONSE_FLAG;
        match self.state {
            State::Idle => match Op::from_byte(arg) {
                None => Action::Notify(response(ready, seq, arg, status::BAD_OP)),
                Some(op) => {
                    self.state = State::Armed {
                        seq,
                        op,
                        deadline_ms: now_ms.saturating_add(ARM_TIMEOUT_MS),
                    };
                    Action::Notify(response(ready, seq, arg, status::OK))
                }
            },
            // A second Begin never disturbs the armed slot (§4.3).
            State::Armed { .. } | State::Working { .. } => {
                Action::Notify(response(ready, seq, arg, status::BUSY))
            }
        }
    }

    const fn abort(&mut self, seq: u16) -> Action {
        let ack = opcode::ABORT | opcode::RESPONSE_FLAG;
        match self.state {
            State::Idle => Action::Notify(response(ack, seq, 0, status::NOT_ARMED)),
            State::Armed { seq: armed, .. } => {
                if seq == armed {
                    self.state = State::Idle;
                    Action::Notify(response(ack, seq, 0, status::OK))
                } else {
                    Action::Notify(response(ack, seq, 0, status::SEQ_MISMATCH))
                }
            }
            State::Working { .. } => Action::Notify(response(ack, seq, 0, status::BUSY)),
        }
    }

    const fn reset_defaults(&mut self, seq: u16) -> Action {
        let ack = opcode::RESET_DEFAULTS | opcode::RESPONSE_FLAG;
        match self.state {
            State::Idle => {
                // Occupies the same single-flight slot through its
                // read-back (§4.3); completes with 0x83, arg 0.
                self.state = State::Working {
                    seq,
                    kind: WorkKind::ResetDefaults,
                    complete_opcode: ack,
                    complete_arg: 0,
                };
                self.working = Some(RemapTable::DEFAULT);
                Action::StartWork(WorkKind::ResetDefaults)
            }
            State::Armed { .. } | State::Working { .. } => {
                Action::Notify(response(ack, seq, 0, status::BUSY))
            }
        }
    }

    const fn revert(&mut self, seq: u16) -> Action {
        let ack = opcode::REVERT | opcode::RESPONSE_FLAG;
        match self.state {
            State::Idle => {
                // Idempotent: dropping no candidate is still Ok (§4.3).
                self.candidate = None;
                Action::Notify(response(ack, seq, 0, status::OK))
            }
            State::Armed { .. } | State::Working { .. } => {
                Action::Notify(response(ack, seq, 0, status::BUSY))
            }
        }
    }

    const fn exit(&mut self, seq: u16) -> Action {
        match self.state {
            // A commit is never abandoned half-done by the reset: the ack
            // and the reset wait for finish_work.
            State::Working { .. } => {
                self.exit_seq = Some(seq);
                Action::ExitDeferred
            }
            State::Idle | State::Armed { .. } => Action::NotifyThenReset(response(
                opcode::EXIT | opcode::RESPONSE_FLAG,
                seq,
                0,
                status::OK,
            )),
        }
    }

    /// A write to the `Map` characteristic: the candidate for the armed
    /// transaction only. Any `Map` write consumes the armed slot — a
    /// rejected one completes the transaction with the reason and changes
    /// nothing else the protocol can observe (§4.3).
    pub fn on_map_write(&mut self, payload: &[u8], now_ms: u64) -> Action {
        self.last_activity_ms = now_ms;
        match self.state {
            State::Idle => Action::Notify(response(opcode::COMPLETE, 0, 0, status::NOT_ARMED)),
            State::Armed { seq, op, .. } => {
                if payload.len() != RemapTable::LEN {
                    self.state = State::Idle;
                    return Action::Notify(response(
                        opcode::COMPLETE,
                        seq,
                        op.as_byte(),
                        status::BAD_LENGTH,
                    ));
                }
                match RemapTable::from_bytes(payload) {
                    None => {
                        self.state = State::Idle;
                        Action::Notify(response(
                            opcode::COMPLETE,
                            seq,
                            op.as_byte(),
                            status::INVALID,
                        ))
                    }
                    Some(map) => {
                        // Immutable from here (§4.3).
                        self.working = Some(map);
                        let kind = match op {
                            Op::Preview => WorkKind::Preview,
                            Op::Commit => WorkKind::Commit,
                        };
                        self.state = State::Working {
                            seq,
                            kind,
                            complete_opcode: opcode::COMPLETE,
                            complete_arg: op.as_byte(),
                        };
                        Action::StartWork(kind)
                    }
                }
            }
            State::Working { .. } => {
                // Seq 0 on purpose: echoing the in-flight (seq, op) here
                // would let the browser mistake this for its completion.
                Action::Notify(response(opcode::COMPLETE, 0, 0, status::BUSY))
            }
        }
    }

    /// The task calls this once the operation settled: for `Preview`, once
    /// the `LiveOutput` producer observes the candidate; for
    /// `Commit`/`ResetDefaults`, after the journal append was read back and
    /// compared (`flash_ok`). Returns `None` if nothing was in flight.
    pub fn finish_work(&mut self, flash_ok: bool) -> Option<WorkDone> {
        let State::Working {
            seq,
            kind,
            complete_opcode,
            complete_arg,
        } = self.state
        else {
            return None;
        };
        let map = self.working.take();
        let code = match (kind, flash_ok, map) {
            (WorkKind::Preview, _, Some(map)) => {
                self.candidate = Some(map);
                status::OK
            }
            (WorkKind::Commit | WorkKind::ResetDefaults, true, Some(map)) => {
                // The append is verified: the journal's newest valid slot IS
                // this map, so stored follows it and any preview is spent.
                self.stored = map;
                self.candidate = None;
                status::OK
            }
            // An ordinary append failure leaves the previous slot effective
            // (§3): stored is untouched and StoredMap proves it.
            _ => status::FLASH,
        };
        self.state = State::Idle;
        let exit_ack = self.exit_seq.take().map(|exit_seq| {
            response(
                opcode::EXIT | opcode::RESPONSE_FLAG,
                exit_seq,
                0,
                status::OK,
            )
        });
        Some(WorkDone {
            complete: response(complete_opcode, seq, complete_arg, code),
            exit_ack,
        })
    }

    /// Deadline check; call on a timer. Arm timeout releases the slot and
    /// yields the `Complete(ArmTimeout)` to notify; idle/absolute expiry
    /// ends the session (§4.5).
    pub const fn poll(&mut self, now_ms: u64) -> Option<Expiry> {
        if let State::Armed {
            seq,
            op,
            deadline_ms,
        } = self.state
        {
            if now_ms >= deadline_ms {
                self.state = State::Idle;
                return Some(Expiry::ArmTimeout(response(
                    opcode::COMPLETE,
                    seq,
                    op.as_byte(),
                    status::ARM_TIMEOUT,
                )));
            }
        }
        if now_ms.saturating_sub(self.session_start_ms) >= ABSOLUTE_CEILING_MS
            || now_ms.saturating_sub(self.last_activity_ms) >= IDLE_DEADLINE_MS
        {
            return Some(Expiry::SessionEnd);
        }
        None
    }
}

/// The 8-byte `LiveInput` notification: raw source state
/// `[buttons_le16, L, R, stick_x, stick_y, seq_le16]` (§4.2).
#[must_use]
pub const fn encode_live_input(state: &ControllerState, seq: u16) -> [u8; 8] {
    let buttons = state.buttons.to_raw().to_le_bytes();
    let seq = seq.to_le_bytes();
    [
        buttons[0],
        buttons[1],
        state.trigger_l,
        state.trigger_r,
        state.stick_x,
        state.stick_y,
        seq[0],
        seq[1],
    ]
}

/// The 13-byte `LiveOutput` notification: the logical report computed
/// on-device under the active map,
/// `[buttons_le16, hat, lt_le16, rt_le16, lx_le16, ly_le16, seq_le16]`.
///
/// The browser's pad picture lights from this, never from a JavaScript
/// re-implementation of the map (§4.2).
#[must_use]
pub const fn encode_live_output(report: &GamepadReport, seq: u16) -> [u8; 13] {
    let buttons = report.buttons.to_le_bytes();
    let lt = report.left_trigger.to_le_bytes();
    let rt = report.right_trigger.to_le_bytes();
    let lx = report.left_x.to_le_bytes();
    let ly = report.left_y.to_le_bytes();
    let seq = seq.to_le_bytes();
    [
        buttons[0], buttons[1], report.hat, lt[0], lt[1], rt[0], rt[1], lx[0], lx[1], ly[0], ly[1],
        seq[0], seq[1],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remap::dest;

    const T0: u64 = 1_000;

    fn proto() -> ConfigProtocol {
        ConfigProtocol::new(RemapTable::DEFAULT, T0)
    }

    fn a_b_swapped() -> RemapTable {
        let mut map = RemapTable::DEFAULT;
        map.buttons[crate::remap::source::A] = dest::B;
        map.buttons[crate::remap::source::B] = dest::A;
        map
    }

    fn control(op: u8, seq: u16, arg: u8) -> [u8; 4] {
        let seq = seq.to_le_bytes();
        [op, seq[0], seq[1], arg]
    }

    /// Drive Begin+Map into Working.
    fn arm_and_map(p: &mut ConfigProtocol, seq: u16, op_arg: u8, map: &RemapTable) -> Action {
        assert_eq!(
            p.on_control(&control(opcode::BEGIN, seq, op_arg), T0),
            Action::Notify(response(0x81, seq, op_arg, status::OK))
        );
        p.on_map_write(&map.to_bytes(), T0)
    }

    #[test]
    #[expect(
        clippy::assertions_on_constants,
        reason = "the constants ARE the claim: every payload fits ATT MTU 23"
    )]
    fn every_payload_fits_minimum_mtu() {
        // ATT MTU 23 -> 20-byte payloads (design v2 §4.2).
        assert!(core::mem::size_of::<Response>() <= 20);
        assert!(RemapTable::LEN <= 20);
        let p = proto();
        assert!(p.info(0, 0).len() <= 20);
        assert!(encode_live_input(&ControllerState::default(), 0).len() <= 20);
        let report = GamepadReport::default();
        assert!(encode_live_output(&report, 0).len() <= 20);
    }

    #[test]
    fn map_without_begin_is_not_armed() {
        let mut p = proto();
        assert_eq!(
            p.on_map_write(&RemapTable::DEFAULT.to_bytes(), T0),
            Action::Notify([opcode::COMPLETE, 0, 0, 0, status::NOT_ARMED])
        );
    }

    #[test]
    fn preview_flow_applies_candidate_only() {
        let mut p = proto();
        let map = a_b_swapped();
        assert_eq!(
            arm_and_map(&mut p, 7, 0x01, &map),
            Action::StartWork(WorkKind::Preview)
        );
        assert_eq!(p.working_map(), Some(&map), "immutable snapshot armed");
        // Nothing observable until completion.
        assert_eq!(*p.active_map(), RemapTable::DEFAULT);

        let done = p.finish_work(true).unwrap();
        assert_eq!(done.complete, [opcode::COMPLETE, 7, 0, 0x01, status::OK]);
        assert_eq!(done.exit_ack, None);
        assert_eq!(*p.active_map(), map, "LiveOutput follows the candidate");
        assert_eq!(
            *p.stored_map(),
            RemapTable::DEFAULT,
            "StoredMap never shows a preview"
        );
        assert_eq!(p.info(0, 0)[2] & 0x02, 0x02, "dirty flag set");
    }

    #[test]
    fn commit_flow_updates_stored() {
        let mut p = proto();
        let map = a_b_swapped();
        assert_eq!(
            arm_and_map(&mut p, 9, 0x02, &map),
            Action::StartWork(WorkKind::Commit)
        );
        let done = p.finish_work(true).unwrap();
        assert_eq!(done.complete, [opcode::COMPLETE, 9, 0, 0x02, status::OK]);
        assert_eq!(*p.stored_map(), map);
        assert_eq!(*p.active_map(), map);
        assert_eq!(p.info(0, 0)[2] & 0x02, 0, "not dirty after commit");
        assert_eq!(p.info(0, 0)[2] & 0x04, 0, "stored is no longer DEFAULT");
    }

    #[test]
    fn flash_failure_keeps_previous_map_and_reports_it() {
        // The ledger of what the browser was told: response only after
        // completion, carrying Flash; StoredMap proves nothing was lost.
        let mut p = proto();
        let map = a_b_swapped();
        arm_and_map(&mut p, 3, 0x02, &map);
        let done = p.finish_work(false).unwrap();
        assert_eq!(done.complete, [opcode::COMPLETE, 3, 0, 0x02, status::FLASH]);
        assert_eq!(
            *p.stored_map(),
            RemapTable::DEFAULT,
            "previous slot effective"
        );
        assert_eq!(*p.active_map(), RemapTable::DEFAULT);
    }

    #[test]
    fn second_begin_is_busy_and_does_not_disturb_the_armed_slot() {
        let mut p = proto();
        assert_eq!(
            p.on_control(&control(opcode::BEGIN, 7, 0x02), T0),
            Action::Notify(response(0x81, 7, 0x02, status::OK))
        );
        assert_eq!(
            p.on_control(&control(opcode::BEGIN, 8, 0x01), T0),
            Action::Notify(response(0x81, 8, 0x01, status::BUSY))
        );
        // The original transaction is intact: the Map completes as seq 7,
        // op Commit.
        assert_eq!(
            p.on_map_write(&a_b_swapped().to_bytes(), T0),
            Action::StartWork(WorkKind::Commit)
        );
        let done = p.finish_work(true).unwrap();
        assert_eq!(done.complete, [opcode::COMPLETE, 7, 0, 0x02, status::OK]);
    }

    #[test]
    fn commands_during_working_are_busy_never_lost_never_reordered() {
        let mut p = proto();
        arm_and_map(&mut p, 5, 0x02, &a_b_swapped());

        assert_eq!(
            p.on_control(&control(opcode::BEGIN, 6, 0x01), T0),
            Action::Notify(response(0x81, 6, 0x01, status::BUSY))
        );
        assert_eq!(
            p.on_control(&control(opcode::ABORT, 5, 0), T0),
            Action::Notify(response(0x82, 5, 0, status::BUSY))
        );
        assert_eq!(
            p.on_control(&control(opcode::RESET_DEFAULTS, 6, 0), T0),
            Action::Notify(response(0x83, 6, 0, status::BUSY))
        );
        assert_eq!(
            p.on_control(&control(opcode::REVERT, 6, 0), T0),
            Action::Notify(response(0x84, 6, 0, status::BUSY))
        );
        // A Map write during Working is Busy with seq 0 — echoing the
        // in-flight pair would fake its completion.
        assert_eq!(
            p.on_map_write(&a_b_swapped().to_bytes(), T0),
            Action::Notify([opcode::COMPLETE, 0, 0, 0, status::BUSY])
        );
        // Ping answers in every state.
        assert_eq!(
            p.on_control(&control(opcode::PING, 6, 0), T0),
            Action::Notify(response(0x85, 6, 0, status::OK))
        );
        // The in-flight operation still completes as itself.
        let done = p.finish_work(true).unwrap();
        assert_eq!(done.complete, [opcode::COMPLETE, 5, 0, 0x02, status::OK]);
    }

    #[test]
    fn abort_semantics() {
        let mut p = proto();
        assert_eq!(
            p.on_control(&control(opcode::ABORT, 1, 0), T0),
            Action::Notify(response(0x82, 1, 0, status::NOT_ARMED))
        );
        p.on_control(&control(opcode::BEGIN, 7, 0x01), T0);
        // A stale sequence does not release the slot.
        assert_eq!(
            p.on_control(&control(opcode::ABORT, 6, 0), T0),
            Action::Notify(response(0x82, 6, 0, status::SEQ_MISMATCH))
        );
        assert_eq!(
            p.on_map_write(&RemapTable::DEFAULT.to_bytes(), T0),
            Action::StartWork(WorkKind::Preview),
            "armed slot survived the mismatched Abort"
        );
        p.finish_work(true).unwrap();
        // A matching sequence releases it.
        p.on_control(&control(opcode::BEGIN, 8, 0x01), T0);
        assert_eq!(
            p.on_control(&control(opcode::ABORT, 8, 0), T0),
            Action::Notify(response(0x82, 8, 0, status::OK))
        );
        assert_eq!(
            p.on_map_write(&RemapTable::DEFAULT.to_bytes(), T0),
            Action::Notify([opcode::COMPLETE, 0, 0, 0, status::NOT_ARMED])
        );
    }

    #[test]
    fn control_wrong_length_is_bad_length_never_a_padded_ping() {
        let mut p = proto();
        // The stray one-byte [0x05] of §4.2: with zero-padding it would be
        // a valid Ping(0); with length-preserving values it is BadLength.
        for bad in [
            &[0x05u8][..],
            &[0x01, 0x07],
            &[0x01, 0x07, 0x00],
            &[0x01, 0x07, 0x00, 0x01, 0x00],
        ] {
            assert_eq!(
                p.on_control(bad, T0),
                Action::Notify([opcode::COMPLETE, 0, 0, 0, status::BAD_LENGTH]),
                "len {}",
                bad.len()
            );
        }
    }

    #[test]
    fn map_wrong_length_completes_bad_length_and_releases() {
        let mut p = proto();
        p.on_control(&control(opcode::BEGIN, 4, 0x02), T0);
        let short = [0u8; 19];
        assert_eq!(
            p.on_map_write(&short, T0),
            Action::Notify([opcode::COMPLETE, 4, 0, 0x02, status::BAD_LENGTH])
        );
        // The transaction completed; a retry needs a fresh Begin.
        assert_eq!(
            p.on_map_write(&RemapTable::DEFAULT.to_bytes(), T0),
            Action::Notify([opcode::COMPLETE, 0, 0, 0, status::NOT_ARMED])
        );
    }

    #[test]
    fn invalid_map_changes_nothing_observable() {
        let mut p = proto();
        p.on_control(&control(opcode::BEGIN, 4, 0x01), T0);
        let mut bad = RemapTable::DEFAULT.to_bytes();
        bad[18] = 0; // trigger_threshold 0
        assert_eq!(
            p.on_map_write(&bad, T0),
            Action::Notify([opcode::COMPLETE, 4, 0, 0x01, status::INVALID])
        );
        assert_eq!(*p.active_map(), RemapTable::DEFAULT);
        assert_eq!(*p.stored_map(), RemapTable::DEFAULT);
        assert_eq!(p.working_map(), None);
        assert_eq!(p.info(0, 0)[2] & 0x02, 0, "not dirty");
    }

    #[test]
    fn arm_timeout_releases_the_slot() {
        let mut p = proto();
        p.on_control(&control(opcode::BEGIN, 7, 0x02), T0);
        assert_eq!(p.poll(T0 + ARM_TIMEOUT_MS - 1), None);
        assert_eq!(
            p.poll(T0 + ARM_TIMEOUT_MS),
            Some(Expiry::ArmTimeout([
                opcode::COMPLETE,
                7,
                0,
                0x02,
                status::ARM_TIMEOUT
            ]))
        );
        // Slot released: a Map is NotArmed, a fresh Begin works.
        assert_eq!(
            p.on_map_write(&RemapTable::DEFAULT.to_bytes(), T0 + ARM_TIMEOUT_MS),
            Action::Notify([opcode::COMPLETE, 0, 0, 0, status::NOT_ARMED])
        );
        assert_eq!(
            p.on_control(&control(opcode::BEGIN, 8, 0x01), T0 + ARM_TIMEOUT_MS),
            Action::Notify(response(0x81, 8, 0x01, status::OK))
        );
    }

    #[test]
    fn idle_deadline_refreshed_by_ping() {
        let mut p = proto();
        assert_eq!(p.poll(T0 + IDLE_DEADLINE_MS - 1), None);
        p.on_control(&control(opcode::PING, 1, 0), T0 + 100_000);
        assert_eq!(p.poll(T0 + IDLE_DEADLINE_MS), None, "Ping refreshed idle");
        assert_eq!(
            p.poll(T0 + 100_000 + IDLE_DEADLINE_MS),
            Some(Expiry::SessionEnd)
        );
    }

    #[test]
    fn absolute_ceiling_is_not_extended_by_activity() {
        let mut p = proto();
        let mut t = T0;
        while t < T0 + ABSOLUTE_CEILING_MS {
            p.on_control(&control(opcode::PING, 1, 0), t);
            t += 60_000;
        }
        assert_eq!(
            p.poll(T0 + ABSOLUTE_CEILING_MS),
            Some(Expiry::SessionEnd),
            "nothing extends the ceiling"
        );
    }

    #[test]
    fn reset_defaults_commits_default_and_completes_with_0x83() {
        let mut p = proto();
        // Make stored non-default first.
        arm_and_map(&mut p, 1, 0x02, &a_b_swapped());
        p.finish_work(true).unwrap();

        assert_eq!(
            p.on_control(&control(opcode::RESET_DEFAULTS, 2, 0), T0),
            Action::StartWork(WorkKind::ResetDefaults)
        );
        assert_eq!(p.working_map(), Some(&RemapTable::DEFAULT));
        let done = p.finish_work(true).unwrap();
        assert_eq!(done.complete, [0x83, 2, 0, 0, status::OK]);
        assert_eq!(*p.stored_map(), RemapTable::DEFAULT);

        // From Armed it is Busy — the single-flight slot is taken.
        p.on_control(&control(opcode::BEGIN, 3, 0x01), T0);
        assert_eq!(
            p.on_control(&control(opcode::RESET_DEFAULTS, 4, 0), T0),
            Action::Notify(response(0x83, 4, 0, status::BUSY))
        );
    }

    #[test]
    fn revert_drops_the_candidate_and_is_idempotent() {
        let mut p = proto();
        arm_and_map(&mut p, 1, 0x01, &a_b_swapped());
        p.finish_work(true).unwrap();
        assert_ne!(*p.active_map(), RemapTable::DEFAULT);

        assert_eq!(
            p.on_control(&control(opcode::REVERT, 2, 0), T0),
            Action::Notify(response(0x84, 2, 0, status::OK))
        );
        assert_eq!(
            *p.active_map(),
            RemapTable::DEFAULT,
            "LiveOutput follows stored"
        );
        assert_eq!(
            p.on_control(&control(opcode::REVERT, 3, 0), T0),
            Action::Notify(response(0x84, 3, 0, status::OK)),
            "idempotent"
        );
    }

    #[test]
    fn exit_from_idle_acks_then_resets() {
        let mut p = proto();
        assert_eq!(
            p.on_control(&control(opcode::EXIT, 1, 0), T0),
            Action::NotifyThenReset(response(0x86, 1, 0, status::OK))
        );
    }

    #[test]
    fn exit_during_working_waits_for_the_operation() {
        let mut p = proto();
        arm_and_map(&mut p, 5, 0x02, &a_b_swapped());
        assert_eq!(
            p.on_control(&control(opcode::EXIT, 6, 0), T0),
            Action::ExitDeferred,
            "a commit is never abandoned half-done by the reset"
        );
        let done = p.finish_work(true).unwrap();
        assert_eq!(done.complete, [opcode::COMPLETE, 5, 0, 0x02, status::OK]);
        assert_eq!(done.exit_ack, Some(response(0x86, 6, 0, status::OK)));
        assert_eq!(*p.stored_map(), a_b_swapped(), "the commit landed first");
    }

    #[test]
    fn bad_ops_are_rejected_without_arming() {
        let mut p = proto();
        for bad_arg in [0x00, 0x03, 0xFF] {
            assert_eq!(
                p.on_control(&control(opcode::BEGIN, 1, bad_arg), T0),
                Action::Notify(response(0x81, 1, bad_arg, status::BAD_OP))
            );
        }
        assert_eq!(
            p.on_map_write(&RemapTable::DEFAULT.to_bytes(), T0),
            Action::Notify([opcode::COMPLETE, 0, 0, 0, status::NOT_ARMED]),
            "a BadOp Begin must not arm"
        );
        // Unknown opcode.
        assert_eq!(
            p.on_control(&control(0x07, 2, 0), T0),
            Action::Notify(response(0x87, 2, 0, status::BAD_OP))
        );
    }

    #[test]
    fn live_encodings_are_exact() {
        let state = ControllerState {
            buttons: crate::controller_state::ButtonState::from_raw(!(1 << 2)), // A
            trigger_l: 10,
            trigger_r: 20,
            stick_x: 30,
            stick_y: 40,
        };
        assert_eq!(
            encode_live_input(&state, 0x1234),
            [0x04, 0x00, 10, 20, 30, 40, 0x34, 0x12]
        );
        let report = GamepadReport {
            left_x: 0x1122,
            left_y: 0x3344,
            left_trigger: 0x0102,
            right_trigger: 0x0203,
            hat: 5,
            buttons: 0x8001,
        };
        assert_eq!(
            encode_live_output(&report, 0x5566),
            [0x01, 0x80, 5, 0x02, 0x01, 0x03, 0x02, 0x22, 0x11, 0x44, 0x33, 0x66, 0x55]
        );
    }

    #[test]
    fn info_payload_shape() {
        let p = proto();
        assert_eq!(p.info(1, 0), [1, 0, 0x04, 1, 20, 20, 1, 0, 120, 10]);
    }
}
