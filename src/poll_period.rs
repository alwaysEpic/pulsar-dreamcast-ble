// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Poll-loop period telemetry over the HID side channel (`poll-period-debug`).
//!
//! The 2026-08 post-OTA regression was the compiled-timing lottery's third
//! strike: rebuilds of unchanged source roll a poll-loop period anywhere from
//! the healthy ~13 ms to a stretched ~20 ms, and the only way to tell was a
//! full host-side capture per binary. This module makes the period — and its
//! attribution — readable in ONE capture: the loop measures itself with the
//! DWT cycle counter and publishes window means through HID report bytes 4-7
//! (the unused right-stick words), the same channel `maple-fail-debug` and
//! `gauge-debug` use, because pulsarv1 has no RTT probe.
//!
//! Static A/B disassembly of the good (v206/v209) vs bad (v207/v208) binaries
//! showed the entire Maple RX/decode region instruction-identical and
//! alignment-preserved (loop-for-loop, mod-4), with the nRF52840 icache off —
//! so the stretch is NOT slower code. The working theory this channel exists
//! to test: the poll loop and the BLE connection events are coupled
//! oscillators. `get_condition`'s ~3.5 ms TX+capture window colliding with a
//! connection event costs a ~4-5 ms retry, and with a *relative* sleep
//! (`Timer::after`) that retry shifts the phase of every subsequent poll —
//! body time feeds back into collision probability. µs-scale layout shifts in
//! base body time move that map between fast-sweeping (benign) and dwelling
//! (30% doubled intervals) attractors. If the theory holds, a bad roll shows
//! up here as a stretched `gc` span and a raised retry count, with the sleep
//! span unchanged.
//!
//! # Channel layout (bytes 4-7 of the 16-byte report)
//!
//! - `[4..6]` = value, LE u16, microseconds unless noted
//! - `[6]`    = low 8 bits of the window counter (groups values per window)
//! - `[7]`    = `0xB0 | k` tag:
//!   - `k=0` mean poll period over the window
//!   - `k=1` max poll period in the window
//!   - `k=2` mean `get_condition` span
//!   - `k=3` mean sleep span (the poll-cadence timer await)
//!   - `k=4` `get_condition` retries in the window (count, not µs)
//!   - `k=5` cumulative poll-cadence overruns (count, saturating)
//!   - `k=6` raw radio-notification count (wrapping u16; healthy ≈133/s)
//!
//! Values rotate through the tags **once per fresh controller sample**, not
//! per send, and the whole four-byte payload is snapshotted at that moment and
//! replayed byte-for-byte until the next one. Both halves matter, and both are
//! about `send_report`'s wire dedup (`ble::hid`), which drops a report
//! identical to the last:
//!
//! - Rotating per *send* made every report byte-distinct, so dedup never
//!   fired. Reports then left at the notify loop's fixed 125 Hz, throttled by
//!   the connection interval, and the host's arrival cadence stopped tracking
//!   the poll loop at all — measured on v297 run #200, where the host saw
//!   15.2 ms between reports while tag 0 reported a 16.1 ms poll period. A
//!   Control capture on such a build *cannot fail* Hz or IQR, which is worse
//!   than reading wrong.
//! - Re-reading the counters per send leaks the same way at a lower rate:
//!   tag 6 (radio notifications) and the window counter advance with no fresh
//!   sample at all, so a repeat send still differs in bytes 4-6. During a
//!   controller outage — exactly when reports *should* stop — that defeats
//!   dedup indefinitely. Snapshotting all four bytes closes it: between fresh
//!   samples there is no path that re-reads anything.
//!
//! A 30 s capture still collects hundreds of each tag. Read with
//! `hid_capture.py --pollperiod`.
//!
//! # No critical sections, single context
//!
//! Same discipline as [`crate::poll_timing`] (see its module docs for the
//! 2026-06 post-mortem): the accumulator is `static mut` touched only from
//! the main poll task; cross-task publication to the BLE task goes through
//! relaxed atomics only. Nothing here masks interrupts, ever.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8, Ordering};

/// Polls per publication window. 32 polls ≈ 0.4 s at the ~13 ms period, so a
/// 30 s capture sees ~70 window updates per tag.
const WINDOW: u32 = 32;

/// CPU clock in MHz — cycles / this = microseconds (nRF52840 runs at 64 MHz).
const CPU_MHZ: u32 = 64;

/// Discontinuity guard: a "period" longer than this is a re-detect episode,
/// a goodbye hold, or some other non-poll stall — discard it and re-anchor
/// rather than let one multi-second gap dominate a window mean. The genuine
/// signal (13-30 ms stretches) sits far below.
const DISCONTINUITY_US: u32 = 65_000;

/// Published window means, µs saturated to u16 (65 ms ceiling, see
/// [`DISCONTINUITY_US`]). Written by the poll task, read by the BLE task.
static PERIOD_MEAN_US: AtomicU16 = AtomicU16::new(0);
static PERIOD_MAX_US: AtomicU16 = AtomicU16::new(0);
static GC_MEAN_US: AtomicU16 = AtomicU16::new(0);
static SLEEP_MEAN_US: AtomicU16 = AtomicU16::new(0);
/// Retries (attempts beyond the first) summed over the window — the
/// collision-rate half of the coupled-oscillator signature.
static RETRIES_IN_WINDOW: AtomicU16 = AtomicU16::new(0);
/// Window counter; low 8 bits ride in byte 6 so a capture can group values.
static WINDOW_SEQ: AtomicU16 = AtomicU16::new(0);

/// Controller polls since boot — **the denominator** for the capture-health
/// counters on tags 8 and 9, and the reason they are worth reading at all: a
/// no-trigger count without the polls it is out of says nothing.
///
/// `u32` because at ~66 Hz a `u16` wraps in under 17 minutes, inside one bench
/// session; tag 7 publishes it saturated to the channel's 16 bits, which covers
/// 10,000 polls six times over.
static POLLS: AtomicU32 = AtomicU32::new(0);

/// Of [`POLLS`], the ones that started outside an LCD frame's shadow, and the
/// no-trigger aborts among them.
///
/// **The gate is read on this pair, never on the pooled rate.** Pooled, the
/// no-trigger rate mostly measures how often a poll happens to follow a frame
/// — v296 had 58-60 of 64 no-triggers in the 0-2 ms post-frame bucket, which is
/// the v290/v292 frame effect and not the thing under test. The split is taken
/// on-device because the host cannot see when a frame went out.
#[cfg(feature = "spim-capture")]
static POLLS_CLEAR: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "spim-capture")]
static NO_TRIGGER_CLEAR: AtomicU32 = AtomicU32::new(0);

/// A poll is outside the frame shadow at this age or older, or with no frame
/// written yet. 15 ms is the diagnostic build's `15+/none` bucket boundary, so
/// the two readings are the same cut.
#[cfg(feature = "spim-capture")]
const FRAME_SHADOW_US: u64 = 15_000;

/// Which arm of an interleaved A/B this binary is, on tag 14.
///
/// Two arms of one comparison are the **same version** — downgrade prevention
/// refuses a lower one and same-version installs are accepted — so neither the
/// wire nor the version identifies which is running, and the v299 pair had
/// nothing but the staged payload sha to go on. One byte closes that.
///
/// **It is the only line the two arms' telemetry differs by**, it is read once
/// per published tag, and it is off every measured path.
const ARM_ID: u8 = 1;

/// Rotates the published tag, once per fresh controller sample — never
/// send-by-send, see the module docs. Lives on the BLE-task side of the
/// channel; relaxed is fine, worst case two sends carry the same tag.
static ROT: AtomicU8 = AtomicU8::new(0);

/// Set by the notify loop when it takes a fresh controller sample; cleared by
/// [`inject`], which re-snapshots only when it was set.
static FRESH: AtomicBool = AtomicBool::new(false);

/// The published payload, cached as one LE u32 = bytes `[4], [5], [6], [7]`.
///
/// Zero means "not yet primed" and cannot collide with a real payload: the top
/// byte is always the `0xB0 | k` tag, so a valid snapshot is never zero.
static SNAPSHOT: AtomicU32 = AtomicU32::new(0);

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Number of distinct tags [`inject`] rotates through.
const N_TAGS: u8 = 16;

/// Accumulator. SAFETY invariant: accessed exclusively from the main poll
/// task — no concurrent references can exist (see module docs).
static mut ACC: Acc = Acc {
    last_top: 0,
    have_top: false,
    n: 0,
    period_sum: 0,
    period_max: 0,
    gc_sum: 0,
    sleep_sum: 0,
    retries: 0,
    #[cfg(feature = "spim-capture")]
    last_frame_us: 0,
    #[cfg(feature = "spim-capture")]
    have_frame: false,
    #[cfg(feature = "spim-capture")]
    poll_clear: false,
    #[cfg(feature = "spim-capture")]
    nt_at_poll: 0,
};

struct Acc {
    /// Wall-clock µs ([`Instant::as_micros`]) of the previous loop top.
    last_top: u64,
    have_top: bool,
    n: u32,
    period_sum: u32,
    period_max: u32,
    gc_sum: u32,
    sleep_sum: u32,
    retries: u32,
    /// Wall-clock µs of the last LCD frame's return, for the shadow split.
    #[cfg(feature = "spim-capture")]
    last_frame_us: u64,
    #[cfg(feature = "spim-capture")]
    have_frame: bool,
    /// Was the poll now in flight outside the frame shadow?
    #[cfg(feature = "spim-capture")]
    poll_clear: bool,
    /// `NO_TRIGGER` as it stood when that poll started.
    #[cfg(feature = "spim-capture")]
    nt_at_poll: u32,
}

#[inline]
#[expect(
    clippy::multiple_unsafe_ops_per_block,
    reason = "taking the address of the static and dereferencing it are one operation in intent; splitting them into two blocks would duplicate the same safety argument"
)]
fn acc() -> &'static mut Acc {
    // SAFETY: single-context access per the module invariant above.
    unsafe { &mut *core::ptr::addr_of_mut!(ACC) }
}

#[inline]
fn cyccnt() -> u32 {
    // SAFETY: CYCCNT is a free-running read-only counter; reading is always safe.
    unsafe { (*cortex_m::peripheral::DWT::PTR).cyccnt.read() }
}

fn enable_dwt_once() {
    if !ENABLED.swap(true, Ordering::Relaxed) {
        // SAFETY: debug-only enable of the DWT cycle counter; DWT/DCB are not
        // managed by the SoftDevice. Idempotent with the enables in
        // `MapleBus::new` and `poll_timing::start`.
        let mut p = unsafe { cortex_m::Peripherals::steal() };
        p.DCB.enable_trace();
        p.DWT.enable_cycle_counter();
    }
}

#[inline]
fn sat16(us: u32) -> u16 {
    u16::try_from(us).unwrap_or(u16::MAX)
}

/// Start a DWT-timed span; pair with [`record_gc`].
///
/// DWT only — see the clock-domain note on [`mark_loop_top`]: this is only
/// valid around code that never sleeps the core, which `get_condition` (pure
/// blocking bit-bang/capture/decode) satisfies.
#[must_use]
pub fn stamp() -> u32 {
    enable_dwt_once();
    cyccnt()
}

/// Start a wall-clock span; pair with [`record_sleep`].
#[must_use]
pub fn stamp_wall() -> u64 {
    embassy_time::Instant::now().as_micros()
}

/// Call once per loop iteration at a fixed point (the top). Measures the
/// top-to-top period and publishes the window when it fills.
///
/// # Clock domains (field-learned, v210 run #40, 2026-08-05)
///
/// Period and sleep are measured on the RTC-backed [`embassy_time::Instant`]
/// (30.5 µs granularity), NOT the DWT cycle counter: **CYCCNT halts while
/// the core sleeps in WFE**, so a DWT top-to-top "period" is CPU-active
/// time only. The v210 capture read sleep mean = 446 µs for a 5 ms timer
/// await and a period ~4.6 ms shorter than the host-observed interval —
/// exactly the slept time going missing. `get_condition` keeps the DWT
/// (µs precision, and it never sleeps inside).
pub fn mark_loop_top() {
    let now = embassy_time::Instant::now().as_micros();
    let a = acc();
    if a.have_top {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the value is clamped with .min(u32::MAX) immediately before the cast, so the narrowing saturates by construction"
        )]
        let period_us = now.saturating_sub(a.last_top).min(u64::from(u32::MAX)) as u32;
        if period_us > DISCONTINUITY_US {
            // Re-detect / goodbye / other stall — drop the sample, keep the
            // window, re-anchor from here.
            a.last_top = now;
            return;
        }
        a.n += 1;
        a.period_sum += period_us;
        a.period_max = a.period_max.max(period_us);
        if a.n >= WINDOW {
            PERIOD_MEAN_US.store(sat16(a.period_sum / a.n), Ordering::Relaxed);
            PERIOD_MAX_US.store(sat16(a.period_max), Ordering::Relaxed);
            GC_MEAN_US.store(sat16(a.gc_sum / a.n), Ordering::Relaxed);
            SLEEP_MEAN_US.store(sat16(a.sleep_sum / a.n), Ordering::Relaxed);
            RETRIES_IN_WINDOW.store(sat16(a.retries), Ordering::Relaxed);
            WINDOW_SEQ.fetch_add(1, Ordering::Relaxed);
            a.n = 0;
            a.period_sum = 0;
            a.period_max = 0;
            a.gc_sum = 0;
            a.sleep_sum = 0;
            a.retries = 0;
        }
    }
    a.last_top = now;
    a.have_top = true;
}

/// Record a completed `get_condition` span.
pub fn record_gc(start: u32) {
    acc().gc_sum += cyccnt().wrapping_sub(start) / CPU_MHZ;
}

/// Record a completed poll-cadence sleep span (the bottom-of-loop timer
/// await). Wall clock, not DWT — the core sleeps in here, which is the
/// whole point (see [`mark_loop_top`]).
pub fn record_sleep(start: u64) {
    let now = embassy_time::Instant::now().as_micros();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is clamped with .min(u32::MAX) immediately before the cast, so the narrowing saturates by construction"
    )]
    let us = now.saturating_sub(start).min(u64::from(u32::MAX)) as u32;
    acc().sleep_sum += us;
}

/// Record that a `get_condition` call needed `attempts` bus transactions.
/// Everything beyond the first is a retry.
pub fn record_attempts(attempts: u32) {
    acc().retries += attempts.saturating_sub(1);
}

/// Note that the notify loop has taken a fresh controller sample.
///
/// This, not the send, is what advances the tag and refreshes the snapshot —
/// so two sends of the same controller sample carry identical bytes and the
/// wire dedup still collapses them. See the module docs for what rotating per
/// send cost, and why the snapshot covers all four bytes rather than the tag
/// alone.
pub fn note_fresh_sample() {
    FRESH.store(true, Ordering::Relaxed);
}

/// Saturate a boot-level counter into the channel's 16 bits.
fn sat_count(n: u32) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Count one controller poll, and classify it against the frame shadow.
///
/// Call at the poll's start, immediately before `get_condition` — the same
/// point the diagnostic build counted `vmu_diag`'s tag 4, so the no-trigger
/// ratio reads straight across to the runs taken on that harness.
///
/// A relaxed add from the poll task, published on tag 7 and read by the BLE
/// task; it is a denominator, not a synchronisation point.
pub fn note_poll() {
    POLLS.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "spim-capture")]
    {
        let a = acc();
        let clear = !a.have_frame
            || embassy_time::Instant::now()
                .as_micros()
                .saturating_sub(a.last_frame_us)
                >= FRAME_SHADOW_US;
        a.poll_clear = clear;
        if clear {
            POLLS_CLEAR.fetch_add(1, Ordering::Relaxed);
        }
        a.nt_at_poll = crate::maple::gpio_bus::NO_TRIGGER.load(Ordering::Relaxed);
    }
}

/// Close the bracket [`note_poll`] opened.
///
/// Call immediately after `get_condition` returns, before anything else can
/// drive the bus: this attributes a no-trigger abort to *this* poll by the
/// counter having moved, which only holds while nothing else has used the
/// capture in between.
#[cfg(feature = "spim-capture")]
pub fn note_poll_end() {
    let a = acc();
    if a.poll_clear && crate::maple::gpio_bus::NO_TRIGGER.load(Ordering::Relaxed) != a.nt_at_poll {
        NO_TRIGGER_CLEAR.fetch_add(1, Ordering::Relaxed);
    }
}

/// No capture, no aborts to attribute — kept so the call site needs no `cfg`.
#[cfg(not(feature = "spim-capture"))]
pub fn note_poll_end() {}

/// Record that an LCD frame has just gone out, opening the shadow
/// [`note_poll`] classifies the following polls against.
///
/// Stamped at the steady-state frame TX only, not at `phase1_vmu_splash`'s
/// boot/dock writes: those happen before the poll loop settles and outside this
/// task's cadence, so the handful of polls after one are counted clear. Against
/// the thousands a reading needs that is noise — but it is why tag 12 is not
/// exactly "polls with no frame in the preceding 15 ms".
#[cfg(feature = "spim-capture")]
pub fn note_frame() {
    let a = acc();
    a.last_frame_us = embassy_time::Instant::now().as_micros();
    a.have_frame = true;
}

/// No capture, so the frame/poll interaction is not what this build measures.
#[cfg(not(feature = "spim-capture"))]
pub fn note_frame() {}

/// Build the four published bytes for tag `k`: `[value LE u16, window, tag]`.
///
/// Every counter read lives here, and this runs only on a fresh sample — that
/// is the whole of the dedup guarantee.
fn payload(k: u8) -> [u8; 4] {
    let val = match k {
        0 => PERIOD_MEAN_US.load(Ordering::Relaxed),
        1 => PERIOD_MAX_US.load(Ordering::Relaxed),
        2 => GC_MEAN_US.load(Ordering::Relaxed),
        3 => SLEEP_MEAN_US.load(Ordering::Relaxed),
        4 => RETRIES_IN_WINDOW.load(Ordering::Relaxed),
        5 => {
            let o = crate::POLL_OVERRUNS.load(Ordering::Relaxed);
            u16::try_from(o).unwrap_or(u16::MAX)
        }
        // Raw SWI1 radio-notification count (wrapping u16) — the radio-quiet
        // gate's INPUT. Healthy ≈ 133/s (two edges per 15ms connection
        // event); a low or bursty rate means the gate is starving at the
        // source, upstream of any classification logic.
        6 => {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a debug telemetry counter; wrapping at 16 bits is intended, since the field is two bytes on the HID side-channel"
            )]
            let n = crate::maple::radio_notify::notification_count() as u16;
            n
        }
        // Capture health (ported from v297's
        // throwaway branch 2026-09-18). Tag 7 is the denominator every one of
        // the rest is a rate out of; 12/13 are the pair the gate is read on.
        7 => sat_count(POLLS.load(Ordering::Relaxed)),
        #[cfg(feature = "spim-capture")]
        8 => sat_count(crate::maple::gpio_bus::NO_TRIGGER.load(Ordering::Relaxed)),
        #[cfg(feature = "spim-capture")]
        9 => sat_count(crate::maple::gpio_bus::INCOMPLETE.load(Ordering::Relaxed)),
        // A CPU-sampling build has no SPIM capture and therefore no abort
        // counters to read. Published as 0 rather than dropped, so the tag
        // numbering is the same on every board and one host decoder serves all.
        #[cfg(not(feature = "spim-capture"))]
        8 | 9 => 0,
        10 => crate::MAPLE_FAIL_TOTAL.load(Ordering::Relaxed),
        #[cfg(feature = "spim-capture")]
        12 => sat_count(POLLS_CLEAR.load(Ordering::Relaxed)),
        #[cfg(feature = "spim-capture")]
        13 => sat_count(NO_TRIGGER_CLEAR.load(Ordering::Relaxed)),
        #[cfg(not(feature = "spim-capture"))]
        12 | 13 => 0,
        // Which arm of an interleaved A/B this is — see `ARM_ID`.
        14 => u16::from(ARM_ID),
        // Read out of the bootloader settings page, so it is what the DFU
        // actually recorded rather than a constant this build carries. It can
        // lag a flash by one — the failure mode the diagnostic build's tag 47
        // showed on v295 — and reads `None` on a UF2 board or a bare DK, which
        // publishes 0. Treat either as "settings not updated", not as the wrong
        // build.
        _ => sat_count(crate::installed_app_version().unwrap_or(0)),
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a debug window sequence counter; wrapping at 8 bits is intended, since the field is one byte on the HID side-channel"
    )]
    let window = WINDOW_SEQ.load(Ordering::Relaxed) as u8;
    let v = val.to_le_bytes();
    [v[0], v[1], window, 0xB0 | k]
}

/// Overwrite report bytes 4-7 with the current telemetry payload.
/// Called from the BLE task's `send_report`, pre-dedup, like the other
/// side-channel features.
pub fn inject(b: &mut [u8; 16]) {
    // Re-snapshot only on a fresh controller sample (or to prime the very
    // first send). Everything else replays the cached bytes verbatim, so no
    // counter can advance the payload between samples — see the module docs.
    if FRESH.swap(false, Ordering::Relaxed) || SNAPSHOT.load(Ordering::Relaxed) == 0 {
        let k = ROT.fetch_add(1, Ordering::Relaxed) % N_TAGS;
        SNAPSHOT.store(u32::from_le_bytes(payload(k)), Ordering::Relaxed);
    }
    b[4..8].copy_from_slice(&SNAPSHOT.load(Ordering::Relaxed).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dedup guarantee, which is the whole reason the payload is cached:
    /// between fresh controller samples the four published bytes do not move,
    /// however far the counters behind them advance.
    ///
    /// Rotating per send instead made every report byte-distinct, so
    /// `send_report`'s dedup never fired and the host's arrival cadence became
    /// a property of the BLE connection interval rather than the poll loop
    /// (v297 run #200). Snapshotting the tag alone would have left tag 6 and
    /// the window counter free to move with no fresh sample at all — the
    /// controller-outage case, where reports should stop.
    ///
    /// One test, not three: these statics are process-global, so separate
    /// `#[test]` functions would race each other.
    ///
    /// ⚠ **Nothing runs this yet.** `.cargo/config.toml` pins the build to
    /// `thumbv7em-none-eabihf`, which has no `test` crate, and `scripts/ci.sh`
    /// runs `cargo test` in `maple-protocol` only — the firmware crate has no
    /// host harness (`ble::config`'s test is in the same position). It was
    /// verified off-tree by copying this module's snapshot path verbatim into
    /// a host crate: it passes, and it fails on both regressions it guards —
    /// rotating per send, and snapshotting the tag alone. Giving the firmware
    /// crate a host harness is its own piece of work.
    #[test]
    fn counters_do_not_move_the_payload_between_fresh_samples() {
        let mut b = [0u8; 16];
        let mut c = [0u8; 16];

        // Prime, then read the same sample twice with every counter `payload`
        // can reach moved in between. The radio count (tag 6) is bumped only
        // from the SWI1 handler and cannot be driven from here; it is read in
        // the same function, under the same gate, as the two below.
        inject(&mut b);
        WINDOW_SEQ.fetch_add(7, Ordering::Relaxed);
        RETRIES_IN_WINDOW.fetch_add(3, Ordering::Relaxed);
        crate::POLL_OVERRUNS.fetch_add(5, Ordering::Relaxed);
        inject(&mut c);
        assert_eq!(b[4..8], c[4..8], "payload moved with no fresh sample");

        // A fresh sample advances the tag and re-reads the counters.
        note_fresh_sample();
        inject(&mut c);
        assert_ne!(b[7], c[7], "a fresh sample must advance the tag");

        // And the new payload is itself stable until the next fresh sample.
        let mut d = [0u8; 16];
        WINDOW_SEQ.fetch_add(1, Ordering::Relaxed);
        inject(&mut d);
        assert_eq!(c[4..8], d[4..8], "payload moved with no fresh sample");
    }
}
