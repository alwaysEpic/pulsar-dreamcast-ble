// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! When the HID notify loop gives up on a connection.
//!
//! Every 8 ms a report is due. The loop hangs up after a run of failures, and
//! hanging up is only right when retrying cannot help — counting the wrong
//! failures is what dropped working links after 88 ms. This module
//! is that rule, with the firmware's error type reduced to [`Outcome`] so it
//! can be tested here; `ble::task` owns the link and does what [`Verdict`] says.
//!
//! # The rule
//!
//! **Unencrypted link: nothing is sent, and every due report counts.** Every
//! CCCD in the pinned GATT server is `Open`, so a peer that never encrypted can
//! still subscribe, and a notify would then reach it in plaintext. A host we
//! serve has always encrypted first — in every saved `BlueZ` capture, fresh pair
//! or reconnect, encryption completed at least 0.66 s before the first report
//! reached the air. So a report due on an unencrypted link
//! is one owed to a peer that will not become a host, and 11 of them in a row
//! shed it rather than letting it hold the only connection.
//!
//! **Encrypted link: only a failure retrying cannot fix counts.**
//!
//!  - [`Outcome::QueueFull`] — the link is retransmitting. Nordic documents it
//!    as *wait and retry*; if the link is really dead, supervision ends it on
//!    the host's 4000 ms clock rather than our 88 ms one.
//!  - [`Outcome::NotSubscribed`] — the host has not written the CCCD yet, or an
//!    `ATT_MTU` exchange is in flight (`sd_ble_gatts_hvx`'s `INVALID_STATE`). A
//!    bonded host subscribes when it is ready; `BlueZ` on a fresh pairing does
//!    so ~2.6 s after connect, after our first send.
//!  - [`Outcome::AttrsMissing`] — the attribute table is not initialised yet.
//!    `Bonder::load_sys_attrs` answers it, and disconnects itself if it cannot.
//!
//! These hold the count where it is rather than resetting it: they are not
//! evidence the link works either.
//!
//! [`Outcome::Sent`] resets the count. It includes a report the dedup cache
//! skipped as unchanged, which is why the unencrypted rule above never asks —
//! on a quiet pad "sent" is true every tick without anything being delivered.

/// What became of one due report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Not offered to the SoftDevice: the link is not encrypted.
    Withheld,
    /// Accepted, or skipped by dedup as identical to the last accepted one.
    Sent,
    /// `NRF_ERROR_RESOURCES`: the notification queue is full.
    QueueFull,
    /// `NRF_ERROR_INVALID_STATE`: CCCD not enabled, or an MTU exchange running.
    NotSubscribed,
    /// `BLE_ERROR_GATTS_SYS_ATTR_MISSING`: attribute table not initialised.
    AttrsMissing,
    /// Anything else — a bad handle, a GATT timeout, `Disconnected`. Will not
    /// fix itself.
    Refused,
}

/// What the notify loop should do after a due report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    HangUp,
}

/// Consecutive counted failures on one connection.
#[derive(Clone, Copy, Debug)]
pub struct NotifyBudget {
    failures: u8,
    /// Failures tolerated; the next one hangs up.
    limit: u8,
}

impl NotifyBudget {
    /// A fresh budget for a new connection. `limit` failures in a row are
    /// tolerated and the one after hangs up — `MAX_NOTIFY_FAILURES`' meaning.
    #[must_use]
    pub const fn new(limit: u8) -> Self {
        Self { failures: 0, limit }
    }

    /// Whether a due report may be offered to the SoftDevice at all.
    #[must_use]
    pub const fn may_send(encrypted: bool) -> bool {
        encrypted
    }

    /// Account for one due report.
    pub const fn record(&mut self, encrypted: bool, outcome: Outcome) -> Verdict {
        let counts = if encrypted {
            match outcome {
                Outcome::Sent => {
                    self.failures = 0;
                    false
                }
                Outcome::QueueFull | Outcome::NotSubscribed | Outcome::AttrsMissing => false,
                // `Withheld` on an encrypted link means the caller skipped a
                // send it was allowed to make; nothing was delivered.
                Outcome::Withheld | Outcome::Refused => true,
            }
        } else {
            // Whatever the caller did, nothing reached a host we serve.
            true
        };

        if counts {
            self.failures = self.failures.saturating_add(1);
        }
        if self.failures > self.limit {
            Verdict::HangUp
        } else {
            Verdict::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The firmware's `MAX_NOTIFY_FAILURES`.
    const LIMIT: u8 = 10;

    /// Feed `n` identical due reports; return the 1-based report that hung up.
    fn hang_up_at(encrypted: bool, outcome: Outcome, n: usize) -> Option<usize> {
        let mut b = NotifyBudget::new(LIMIT);
        (1..=n).find(|_| b.record(encrypted, outcome) == Verdict::HangUp)
    }

    #[test]
    fn only_an_encrypted_link_is_sent_to() {
        assert!(NotifyBudget::may_send(true));
        assert!(!NotifyBudget::may_send(false));
    }

    // --- encrypted: the host we serve --------------------------------------

    #[test]
    fn encrypted_transients_never_hang_up() {
        // A marginal link stalls for seconds; a host subscribes when it is
        // ready. 10 000 ticks is 80 s of either.
        for o in [
            Outcome::QueueFull,
            Outcome::NotSubscribed,
            Outcome::AttrsMissing,
        ] {
            assert_eq!(hang_up_at(true, o, 10_000), None, "{o:?}");
        }
    }

    #[test]
    fn encrypted_refusals_hang_up_on_the_eleventh() {
        assert_eq!(hang_up_at(true, Outcome::Refused, 100), Some(11));
    }

    #[test]
    fn a_skipped_send_on_an_encrypted_link_counts() {
        assert_eq!(hang_up_at(true, Outcome::Withheld, 100), Some(11));
    }

    #[test]
    fn a_sent_report_resets_the_count() {
        let mut b = NotifyBudget::new(LIMIT);
        for _ in 0..LIMIT {
            assert_eq!(b.record(true, Outcome::Refused), Verdict::Continue);
        }
        assert_eq!(b.record(true, Outcome::Sent), Verdict::Continue);
        for _ in 0..LIMIT {
            assert_eq!(b.record(true, Outcome::Refused), Verdict::Continue);
        }
        assert_eq!(b.record(true, Outcome::Refused), Verdict::HangUp);
    }

    #[test]
    fn transients_hold_the_count_rather_than_reset_it() {
        // Ten refusals, a long stall, then one more refusal: the stall is not
        // evidence the link works, so the eleventh still hangs up.
        let mut b = NotifyBudget::new(LIMIT);
        for _ in 0..LIMIT {
            b.record(true, Outcome::Refused);
        }
        for _ in 0..500 {
            assert_eq!(b.record(true, Outcome::QueueFull), Verdict::Continue);
        }
        assert_eq!(b.record(true, Outcome::Refused), Verdict::HangUp);
    }

    /// The 2026-09-20 second trigger: a fresh `BlueZ` pairing, encrypted at
    /// +1.13 s, first report due ~+2.1 s, CCCD written +2.6 s. v304 counted
    /// the not-yet-subscribed sends and hung up at +3.3 s.
    #[test]
    fn fresh_pair_that_subscribes_late_is_kept() {
        let mut b = NotifyBudget::new(LIMIT);
        // ~0.5 s of reports due before the CCCD write, at 8 ms.
        for _ in 0..63 {
            assert_eq!(b.record(true, Outcome::NotSubscribed), Verdict::Continue);
        }
        assert_eq!(b.record(true, Outcome::Sent), Verdict::Continue);
    }

    // --- unencrypted: a peer that is not a host we serve --------------------

    /// An unbonded peer connects on reconnect advertising and
    /// never subscribes. v305 held the only connection for it indefinitely.
    #[test]
    fn unencrypted_peer_is_shed_on_the_eleventh_due_report() {
        assert_eq!(hang_up_at(false, Outcome::Withheld, 100), Some(11));
    }

    /// The CCCDs are `Open`, so the same peer *can*
    /// subscribe. Were a send attempted and "succeed" — including a dedup skip
    /// on a quiet pad — it must still not keep the peer.
    #[test]
    fn unencrypted_success_does_not_keep_a_peer() {
        assert_eq!(hang_up_at(false, Outcome::Sent, 100), Some(11));
    }

    #[test]
    fn unencrypted_transients_count_too() {
        for o in [
            Outcome::QueueFull,
            Outcome::NotSubscribed,
            Outcome::AttrsMissing,
            Outcome::Refused,
        ] {
            assert_eq!(hang_up_at(false, o, 100), Some(11), "{o:?}");
        }
    }

    /// A bonded reconnect: unencrypted for the first ~120-280 ms, encrypted
    /// long before a report is due. If a slow host were ever still unencrypted
    /// when reports started, encryption landing within the budget must clear
    /// the debt at its first delivered report.
    #[test]
    fn encryption_inside_the_budget_is_kept() {
        let mut b = NotifyBudget::new(LIMIT);
        for _ in 0..LIMIT {
            assert_eq!(b.record(false, Outcome::Withheld), Verdict::Continue);
        }
        assert_eq!(b.record(true, Outcome::NotSubscribed), Verdict::Continue);
        assert_eq!(b.record(true, Outcome::Sent), Verdict::Continue);
        for _ in 0..LIMIT {
            assert_eq!(b.record(true, Outcome::Refused), Verdict::Continue);
        }
    }

    #[test]
    fn the_count_cannot_wrap_back_under_the_limit() {
        // A caller that ignores `HangUp` keeps getting it, 1 000 ticks on.
        let mut b = NotifyBudget::new(LIMIT);
        for _ in 0..1_000 {
            b.record(false, Outcome::Withheld);
        }
        assert_eq!(b.record(false, Outcome::Withheld), Verdict::HangUp);
    }
}
