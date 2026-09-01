// SPDX-License-Identifier: GPL-3.0-or-later

//! SoftDevice radio notification gate for Maple Bus transactions.
//!
//! # RE-ENABLED 2026-08-05 — now gates the controller poll loop (soak-gated)
//!
//! `main.rs` calls [`init`] again: the poll pacer starts each Maple
//! transaction at the head of a radio-quiet window ([`idle_age_ms`] fresh),
//! which removes the Maple/BLE collisions that field data proved were the
//! real mechanism of the compiled-timing lottery (board log 2026-08-05:
//! collision retries fed poll phase back into collision probability; gc mean
//! 14-17ms on bad rolls, ~1-1.3 retries/poll).
//!
//! ## The 2026-06-10 assert history, and its confound
//!
//! This banner used to say "every gate built on these asserted while VMU
//! writes were active; mechanism never identified — do not re-enable". A
//! day after that verdict, `poll_timing`'s critical-section bug was found:
//! its VMU-write measurement span masked interrupts and caused stochastic
//! SD asserts — a path that only executes when writes are active, exactly
//! reproducing the "writes-on asserts, writes-off clean" evidence that
//! condemned this module. The notification config itself (`INT_ON_BOTH`)
//! ran for hours with colliding writes and never asserted. Not proven
//! innocent: the re-enable is gated on a fresh soak (historical assert rate
//! ~1-2/min ⇒ a 30-60 min clean soak is decisive). If asserts return on a
//! build WITHOUT `poll-timing`, this module really is guilty — disable the
//! [`init`] call and fall back to the fixed-cadence pacer.
//!
//! ## ⚠ STALE NUMBERS REMOVED (2026-07-27)
//!
//! This banner used to quantify that loss as "~64% ... giving ~2fps effective
//! animation from 6fps attempted". **Both figures are wrong for the current
//! firmware and should not be quoted:**
//!
//! - The 6fps denominator contradicts the code. The animation advances every
//!   `VMU_ANIM_INTERVAL` = 20 polls (`main.rs`), which at the 15ms poll period
//!   is ~300ms — about **3.3fps attempted**, not 6.
//! - The ~64%/2fps loss figures predate [`super::pwm_tx::write_lcd_dma`]. They
//!   describe the old bit-bang LCD write, whose ~10ms radio-sensitive span was
//!   the thing causing the collisions; the DMA path cut that to ~6.3ms
//!   specifically to fit the BLE quiet gap. Nobody re-measured afterwards.
//!
//! The current effective rate is **unmeasured**. An attempt to recover it from
//! video failed and is worth not repeating: the VMU LCD's persistence is long
//! relative to the 300ms animation step, so successive rotation steps ghost
//! together on the glass and there is no crisp frame boundary to detect —
//! measured rates swung 0.61-2.01/s purely with the analysis threshold. Ground
//! truth only exists in firmware, via a diagnostic build that restores the ACK
//! read in `write_vmu_lcd` and counts it, accepting the timing cost for the
//! duration of the test.
//!
//! The BLE SoftDevice fires connection events at interrupt priority 0, which
//! corrupt long bit-bang Maple transactions (the ~7.6ms LCD TX, measured).
//! The quiet gap between connection events (~13ms at the measured ~15ms
//! interval) fits the TX, but only if the write starts at the *beginning* of
//! the gap — so this module timestamps the moment the radio goes idle and
//! exposes the age of the current quiet window.
//!
//! # Why `INT_ON_BOTH`, and why gap-classified edges
//!
//! Three revisions of this module taught two lessons (2026-06-10):
//!
//! 1. **Don't reconstruct the edge phase with an alternating flag.** SWI1
//!    carries no edge identity; if two notifications coalesce into one
//!    interrupt, an alternating flag inverts permanently and the "idle edge"
//!    becomes the ACTIVE warning fired 800µs *before* the radio starts —
//!    measured as a 100% VMU write-ACK failure rate.
//! 2. **Don't use `INT_ON_INACTIVE`.** It eliminates the phase problem, but
//!    every build combining it with active VMU writes produced SoftDevice
//!    assertion panics (reboot loops, ~1-2/min); with writes disabled it ran
//!    clean, and the original `INT_ON_BOTH` config ran for hours with
//!    colliding writes and never asserted. Mechanism unknown — decided on
//!    evidence.
//!
//! So: `INT_ON_BOTH` (the empirically assert-free config), with edges
//! classified by the gap *preceding* them instead of a persistent flag. An
//! INACTIVE notification follows its ACTIVE partner by ~1-3ms (800µs warning +
//! connection event); an ACTIVE notification follows ~12ms of quiet. A
//! pre-gap under [`GAP_CLASSIFY_CYC`] therefore marks an INACTIVE edge.
//! Classification is stateless per notification: a coalesced interrupt
//! misclassifies one edge (costing at most one corrupted frame, which the
//! VMU's CRC rejects) and the next notification classifies correctly again.
//!
//! The handler body is the minimum possible — DWT CYCCNT reads and atomic
//! stores. No timer-driver calls in interrupt context (an earlier revision
//! called `embassy_time::Instant::now()` here while the asserts were being
//! chased; keep it out of the suspect pool).
//!
//! # Usage
//!
//! 1. Call [`init`] once after the SoftDevice is enabled.
//! 2. Before a long Maple TX, check that [`idle_age_ms`] returns `Some(a)` with `a <= 3`
//!    — start only in the fresh part of the quiet window. If stale, skip and
//!    retry next poll.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use nrf_softdevice_s140::{
    self as sd, NRF_RADIO_NOTIFICATION_DISTANCES_NRF_RADIO_NOTIFICATION_DISTANCE_800US,
    NRF_RADIO_NOTIFICATION_TYPES_NRF_RADIO_NOTIFICATION_TYPE_INT_ON_BOTH, NRF_SUCCESS,
};

/// RTC1 tick count of the most recent notification (either edge).
static LAST_EDGE_TICKS: AtomicU32 = AtomicU32::new(0);

/// RTC1 tick count of the most recent INACTIVE-classified notification.
static LAST_INACTIVE_TICKS: AtomicU32 = AtomicU32::new(0);

/// True once any INACTIVE-classified notification has been seen.
static INACTIVE_SEEN: AtomicBool = AtomicBool::new(false);

/// Total notifications since boot (BOTH edges — two per radio event).
static NOTIFY_COUNT: AtomicU32 = AtomicU32::new(0);

/// # Clock choice: RTC1 COUNTER, never the DWT (field-learned, run #48)
///
/// The first version of this module timestamped edges with DWT CYCCNT.
/// **CYCCNT halts while the core sleeps in WFE**, so its inter-notification
/// "gap" is CPU-awake time, not wall time — which made edge classification
/// depend on how much CPU the rest of the firmware happened to burn per
/// connection interval. Layouts that ran the poll body in ~7-9ms of awake
/// time classified correctly (v216-v218); v219 rolled a faster body,
/// awake time between events fell toward the 5ms threshold, and ACTIVE
/// warnings (fired 800µs BEFORE radio activity) started classifying as
/// INACTIVE — the scheduler then aligned Maple transactions directly into
/// connection events (21% doubled intervals, 40-91ms stalls from align
/// caps). A build-dependent clock inside the scheduler is the compiled-
/// timing lottery all over again. RTC1 runs through sleep (it IS the
/// embassy time base, `time-driver-rtc1`); reading its COUNTER register
/// is one volatile load with no driver call, so it honors the "no
/// timer-driver calls in interrupt context" rule below.
const RTC1_COUNTER: *const u32 = 0x4001_1504 as *const u32;

/// RTC1 tick rate (32.768 kHz) and its 24-bit counter mask.
const RTC_HZ: u32 = 32_768;
const RTC_MASK: u32 = 0x00FF_FFFF;

/// Pre-gap threshold separating the two edges: ACTIVE→INACTIVE gaps are
/// ~1-3ms (800µs warning + connection event), INACTIVE→ACTIVE gaps are
/// ~12ms at the measured ~15ms connection interval. 5ms sits between.
const GAP_CLASSIFY_TICKS: u32 = 5 * RTC_HZ / 1000;

/// nRF52840 IRQ number for `SWI1_EGU1`.
const SWI1_EGU1_IRQN: u32 = 21;

#[inline]
fn rtc_ticks() -> u32 {
    // SAFETY: RTC1 COUNTER is a free-running read-only register (the
    // embassy time base keeps the peripheral started); a single volatile
    // read is always safe, including from interrupt context.
    unsafe { core::ptr::read_volatile(RTC1_COUNTER) }
}

/// 24-bit wrapping tick delta.
#[inline]
const fn tick_delta(now: u32, prev: u32) -> u32 {
    now.wrapping_sub(prev) & RTC_MASK
}

/// Initialize radio notifications. Call once after SoftDevice is enabled.
///
/// Configures `INT_ON_BOTH` with 800µs advance warning, enables the DWT
/// cycle counter used for edge timestamps, and enables the `SWI1_EGU1`
/// interrupt at priority 2 (same as Embassy, below SoftDevice).
///
/// Returns `true` on success.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the notification type and distance constants are small enum discriminants that fit u8"
)]
pub fn init() -> bool {
    // SAFETY: a SoftDevice SVC call taking two enum discriminants by value; it
    // dereferences nothing. The SoftDevice is enabled before any caller reaches
    // `init()`, and the return code is checked below rather than assumed.
    let ret = unsafe {
        sd::sd_radio_notification_cfg_set(
            NRF_RADIO_NOTIFICATION_TYPES_NRF_RADIO_NOTIFICATION_TYPE_INT_ON_BOTH as u8,
            NRF_RADIO_NOTIFICATION_DISTANCES_NRF_RADIO_NOTIFICATION_DISTANCE_800US as u8,
        )
    };

    if ret != NRF_SUCCESS {
        return false;
    }

    INACTIVE_SEEN.store(false, Ordering::Release);

    // Enable `SWI1_EGU1` in the NVIC at priority 2
    // nRF52840 uses 3 priority bits in the upper bits of the priority register
    //
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "priority-then-enable is one NVIC configuration sequence and the \
                  order matters: enabling before the priority is set could take an \
                  interrupt at the reset-default priority"
    )]
    // SAFETY: both writes target fixed Cortex-M NVIC registers at
    // architecturally-defined addresses (IPR base `0xE000_E400`, one byte per
    // IRQ; ISER0 base `0xE000_E100`), so the pointers are valid by construction
    // and correctly aligned — IPR is byte-addressed and ISER0 is word-aligned.
    // `SWI1_EGU1_IRQN` (21) is below the 32-IRQ span of ISER0, so the shift
    // cannot overflow. These registers belong to the application, not the
    // SoftDevice, which reserves only priorities 0 and 1 — we set 2, matching
    // Embassy. `write_volatile` is required so the writes are not reordered or
    // elided.
    unsafe {
        // Set priority: NVIC_IPR base = 0xE000_E400, each IRQ gets 1 byte
        let pri_reg = (0xE000_E400u32 + SWI1_EGU1_IRQN) as *mut u8;
        core::ptr::write_volatile(pri_reg, 2 << 5);

        // Enable: NVIC_ISER0 base = 0xE000_E100, bit 21
        let iser_reg = 0xE000_E100u32 as *mut u32;
        core::ptr::write_volatile(iser_reg, 1 << SWI1_EGU1_IRQN);
    }

    true
}

/// Milliseconds since the radio last finished a connection event (most
/// recent INACTIVE-classified notification), or `None` if none seen yet.
///
/// Long Maple transactions should start only when this is small — the fresh
/// part of the inter-event quiet window. A large age means either the next
/// connection event is imminent (~15ms cadence) or the radio is active right
/// now; both are bad times to start a ~7.6ms driven TX.
pub fn idle_age_ms() -> Option<u32> {
    if !INACTIVE_SEEN.load(Ordering::Acquire) {
        return None;
    }
    let edge = LAST_INACTIVE_TICKS.load(Ordering::Acquire);
    Some(tick_delta(rtc_ticks(), edge) * 1000 / RTC_HZ)
}

/// Total notifications since boot (both edges — two per radio event, so the
/// radio event cadence is half the delta).
pub fn notification_count() -> u32 {
    NOTIFY_COUNT.load(Ordering::Relaxed)
}

/// SWI1 interrupt handler — called by the SoftDevice for radio notifications.
/// Classifies the edge by the gap since the previous notification (see module
/// docs) and timestamps INACTIVE edges. Keep this minimal — it runs in
/// interrupt context between SoftDevice activities.
fn on_radio_notification() {
    let now = rtc_ticks();
    let prev = LAST_EDGE_TICKS.swap(now, Ordering::AcqRel);
    // Short pre-gap ⇒ this notification ends a connection event (INACTIVE).
    // First-ever notification: prev=0 makes the gap effectively random/huge,
    // which classifies as ACTIVE — safe (we just wait for the next event).
    if tick_delta(now, prev) < GAP_CLASSIFY_TICKS {
        LAST_INACTIVE_TICKS.store(now, Ordering::Release);
        INACTIVE_SEEN.store(true, Ordering::Release);
    }
    NOTIFY_COUNT.fetch_add(1, Ordering::Relaxed);
}

// Register the SWI1_EGU1 interrupt handler.
// Both symbol names are provided for PAC compatibility (same pattern as
// nrf-softdevice's SWI2 handler in events.rs).

#[export_name = "EGU1_SWI1"]
unsafe extern "C" fn swi1_irq_handler() {
    on_radio_notification();
}

#[expect(
    dead_code,
    reason = "no Rust caller: the NVIC dispatches through the exported symbol, which \
              is also what keeps the function in the binary. Kept under this second \
              PAC spelling of the IRQ name for compatibility (see comment above)"
)]
#[export_name = "SWI1_EGU1"]
unsafe extern "C" fn old_swi1_irq_handler() {
    on_radio_notification();
}
