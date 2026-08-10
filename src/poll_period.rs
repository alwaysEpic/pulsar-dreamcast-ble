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
//! Values rotate through the tags send-by-send; a 30 s capture collects
//! hundreds of each. Read with `hid_capture.py --pollperiod`.
//!
//! # No critical sections, single context
//!
//! Same discipline as [`crate::poll_timing`] (see its module docs for the
//! 2026-06 post-mortem): the accumulator is `static mut` touched only from
//! the main poll task; cross-task publication to the BLE task goes through
//! relaxed atomics only. Nothing here masks interrupts, ever.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};

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

/// Rotates the published tag send-by-send. Lives on the BLE-task side of the
/// channel; relaxed is fine, worst case two sends carry the same tag.
static ROT: AtomicU8 = AtomicU8::new(0);

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Number of distinct tags [`inject`] rotates through.
const N_TAGS: u8 = 7;

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
}

#[inline]
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

/// Start a DWT-timed span; pair with [`record_gc`]. DWT only — see the
/// clock-domain note on [`mark_loop_top`]: this is only valid around code
/// that never sleeps the core, which `get_condition` (pure blocking
/// bit-bang/capture/decode) satisfies.
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
        #[allow(clippy::cast_possible_truncation)]
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
    #[allow(clippy::cast_possible_truncation)]
    let us = now.saturating_sub(start).min(u64::from(u32::MAX)) as u32;
    acc().sleep_sum += us;
}

/// Record that a `get_condition` call needed `attempts` bus transactions.
/// Everything beyond the first is a retry.
pub fn record_attempts(attempts: u32) {
    acc().retries += attempts.saturating_sub(1);
}

/// Overwrite report bytes 4-7 with the next rotating telemetry payload.
/// Called from the BLE task's `send_report`, pre-dedup, like the other
/// side-channel features.
pub fn inject(b: &mut [u8; 16]) {
    let k = ROT.fetch_add(1, Ordering::Relaxed) % N_TAGS;
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
        _ => {
            #[allow(clippy::cast_possible_truncation)]
            let n = crate::maple::radio_notify::notification_count() as u16;
            n
        }
    };
    b[4..6].copy_from_slice(&val.to_le_bytes());
    #[allow(clippy::cast_possible_truncation)]
    {
        b[6] = WINDOW_SEQ.load(Ordering::Relaxed) as u8;
    }
    b[7] = 0xB0 | k;
}
