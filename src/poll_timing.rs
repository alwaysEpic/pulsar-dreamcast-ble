//! Non-intrusive timing for the main controller-poll loop (`poll-timing` feature).
//!
//! Issue #5 traced the input-drop symptom to the *poll rate*: the loop completes
//! only ~12-17 times/sec where ~125 Hz was assumed. This module attributes where
//! the ~60-80 ms/iteration goes — `get_condition` vs the VMU LCD write vs the rest.
//!
//! Timing uses the DWT cycle counter (a register read, zero bus impact). Per
//! `docs/learnings.md` §2, NOTHING is logged between Maple TX and RX — durations
//! are accumulated and the summary is emitted once per ~60 polls from the bottom
//! of the loop, outside the hot window.
//!
//! # ABSOLUTELY NO CRITICAL SECTIONS IN HERE
//!
//! The first version kept the accumulator in a
//! `Mutex<CriticalSectionRawMutex, Cell<Acc>>` — a PRIMASK interrupt mask for
//! a few µs on every record, ~400×/sec at the 60Hz poll rate. The SoftDevice
//! tolerates almost all of them, and then one lands on a timing-critical
//! instant: stochastic SoftDevice assertion panics, ~1/min, in every build
//! carrying this feature across 2026-06-10/11 — two days were spent
//! attributing them to the VMU write path the instrumentation was built to
//! measure. (Nordic's wording: disabling interrupts "only a little bit ...
//! may appear to work, but you will get assertion failed errors after
//! hours.") The diagnostic harness was crashing the patient.
//!
//! No synchronization is needed at all: every caller (`start`, `record_*`,
//! `tick_and_log`) runs on the main poll task — single context, never from
//! an interrupt or another task. Keep it that way; if a second context ever
//! needs to record, use atomics, never a critical section.

use core::sync::atomic::{AtomicBool, Ordering};

/// Accumulator storage. SAFETY invariant: accessed exclusively from the main
/// poll task (see module docs) — no concurrent references can exist.
static mut ACC_STORAGE: Acc = EMPTY_ACC;

#[inline]
fn acc() -> &'static mut Acc {
    // SAFETY: single-context access per the module invariant above.
    unsafe { &mut *core::ptr::addr_of_mut!(ACC_STORAGE) }
}

/// CPU clock in MHz — cycles / this = microseconds (nRF52840 runs at 64 MHz).
const CPU_MHZ: u32 = 64;
/// Emit a summary roughly every this many polls.
const LOG_EVERY: u32 = 60;

#[inline]
fn cyccnt() -> u32 {
    // SAFETY: CYCCNT is a free-running read-only counter; reading is always safe.
    unsafe { (*cortex_m::peripheral::DWT::PTR).cyccnt.read() }
}

#[derive(Clone, Copy)]
struct Stat {
    sum: u32,
    min: u32,
    max: u32,
    n: u32,
}

const EMPTY_STAT: Stat = Stat {
    sum: 0,
    min: 0,
    max: 0,
    n: 0,
};

impl Stat {
    fn add(&mut self, us: u32) {
        if self.n == 0 || us < self.min {
            self.min = us;
        }
        if us > self.max {
            self.max = us;
        }
        self.sum = self.sum.saturating_add(us);
        self.n += 1;
    }
    fn avg(self) -> u32 {
        if self.n > 0 {
            self.sum / self.n
        } else {
            0
        }
    }
}

#[derive(Clone, Copy)]
struct Acc {
    last_top: u32,
    period: Stat,
    gc: Stat,
    vmu: Stat,
    tx: Stat,
    read: Stat,
    dec: Stat,
    tries: Stat,
    vmu_fail: u32,
}

const EMPTY_ACC: Acc = Acc {
    last_top: 0,
    period: EMPTY_STAT,
    gc: EMPTY_STAT,
    vmu: EMPTY_STAT,
    tx: EMPTY_STAT,
    read: EMPTY_STAT,
    dec: EMPTY_STAT,
    tries: EMPTY_STAT,
    vmu_fail: 0,
};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Start a timed span: enables the cycle counter on first use, returns the
/// current cycle count. Pair with [`record_gc`] / [`record_vmu`].
#[must_use]
pub fn start() -> u32 {
    if !ENABLED.swap(true, Ordering::Relaxed) {
        // SAFETY: debug-only enable of the DWT cycle counter; DWT/DCB are not
        // managed by the SoftDevice. Stealing is acceptable for this debug build.
        let mut p = unsafe { cortex_m::Peripherals::steal() };
        p.DCB.enable_trace();
        p.DWT.enable_cycle_counter();
    }
    cyccnt()
}

fn elapsed_us(start: u32) -> u32 {
    cyccnt().wrapping_sub(start) / CPU_MHZ
}

fn add_to(sel: fn(&mut Acc) -> &mut Stat, val: u32) {
    sel(acc()).add(val);
}

/// Record a completed `get_condition` span (microseconds).
pub fn record_gc(start: u32) {
    add_to(|a| &mut a.gc, elapsed_us(start));
}

/// Record a completed `write_vmu_lcd` span (microseconds) and whether the
/// VMU acknowledged it. `n` vs `fail` in the summary exposes the write
/// failure rate (a failed write stays dirty and is re-attempted).
pub fn record_vmu(start: u32, ok: bool) {
    let us = elapsed_us(start);
    let a = acc();
    a.vmu.add(us);
    if !ok {
        a.vmu_fail += 1;
    }
}

/// Record a completed `write_packet` (command TX) span (microseconds).
pub fn record_tx(start: u32) {
    add_to(|a| &mut a.tx, elapsed_us(start));
}

/// Record a completed `wait_and_sample` span — wait-for-response plus the
/// bulk capture loop (microseconds). The wait portion is bounded by the
/// controller's ~µs response latency, so this span ≈ capture cost.
pub fn record_read(start: u32) {
    add_to(|a| &mut a.read, elapsed_us(start));
}

/// Record a completed decode span — edge scan plus bit decode (microseconds).
pub fn record_decode(start: u32) {
    add_to(|a| &mut a.dec, elapsed_us(start));
}

/// Record how many bus transactions one `get_condition` call needed before a
/// packet came back (1 = first try; `MAX_RETRIES` is also recorded on total
/// failure). `sum` over a window vs the poll count exposes routine retries.
pub fn record_tries(n: u32) {
    add_to(|a| &mut a.tries, n);
}

/// Call once per loop iteration, at the bottom (outside the TX/RX window).
/// Measures the full loop period and emits a summary every `LOG_EVERY` polls.
pub fn tick_and_log() {
    let now = cyccnt();
    let mut summary = None;
    {
        let a = acc();
        if a.last_top != 0 {
            a.period.add(now.wrapping_sub(a.last_top) / CPU_MHZ);
        }
        a.last_top = now;
        if a.period.n >= LOG_EVERY {
            summary = Some((
                a.period, a.gc, a.vmu, a.tx, a.read, a.dec, a.tries, a.vmu_fail,
            ));
            a.period = EMPTY_STAT;
            a.gc = EMPTY_STAT;
            a.vmu = EMPTY_STAT;
            a.tx = EMPTY_STAT;
            a.read = EMPTY_STAT;
            a.dec = EMPTY_STAT;
            a.tries = EMPTY_STAT;
            a.vmu_fail = 0;
        }
    }
    if let Some((period, gc, vmu, tx, read, dec, tries, vmu_fail)) = summary {
        // Outside the hot path: safe to log here.
        crate::log!(
            "POLLTIME us | period avg={} min={} max={} | get_cond avg={} max={} | vmu avg={} max={} n={} fail={} | n={}",
            period.avg(),
            period.min,
            period.max,
            gc.avg(),
            gc.max,
            vmu.avg(),
            vmu.max,
            vmu.n,
            vmu_fail,
            period.n
        );
        // get_cond sub-phases, per bus transaction (a retried get_cond records
        // tx/read/dec once per attempt). tries: sum vs n shows the retry rate
        // (sum == n ⇒ every poll succeeded first try).
        crate::log!(
            "POLLPHASE us | tx avg={} max={} | read avg={} min={} max={} | dec avg={} min={} max={} | tries sum={} max={} n={}",
            tx.avg(),
            tx.max,
            read.avg(),
            read.min,
            read.max,
            dec.avg(),
            dec.min,
            dec.max,
            tries.sum,
            tries.max,
            tries.n
        );
    }
}
