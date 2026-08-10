// SPDX-License-Identifier: GPL-3.0-or-later

//! SoftDevice radio notification gate for Maple Bus LCD writes.
//!
//! # ⚠ CURRENTLY UNUSED — preserved as a post-mortem (2026-06-10)
//!
//! `main.rs` no longer calls [`init`]: every write-scheduling gate built on
//! radio notifications produced SoftDevice assertion panics while VMU writes
//! were active — the alternating-flag `ON_BOTH` original, `INT_ON_INACTIVE`,
//! and the gap-classified `ON_BOTH` below all asserted within minutes, while
//! identical builds with writes disabled ran clean. The mechanism was never
//! identified (the write path makes no SD calls and masks no interrupts).
//! VMU writes are now fire-and-forget and unanchored; collided frames are
//! dropped by the VMU's CRC (~64% at the measured ~15ms connection interval,
//! giving ~2fps effective animation from 6fps attempted). Do not re-enable
//! without a soak test against the 2026-06-10 bench measurements.
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
//! INACTIVE notification follows its ACTIVE partner by ~1-3ms (800µs warning
//! + connection event); an ACTIVE notification follows ~12ms of quiet. A
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
//! 2. Before a long Maple TX, check [`idle_age_ms`]`.is_some_and(|a| a <= 3)`
//!    — start only in the fresh part of the quiet window. If stale, skip and
//!    retry next poll.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use nrf_softdevice_s140::{
    self as sd, NRF_RADIO_NOTIFICATION_DISTANCES_NRF_RADIO_NOTIFICATION_DISTANCE_800US,
    NRF_RADIO_NOTIFICATION_TYPES_NRF_RADIO_NOTIFICATION_TYPE_INT_ON_BOTH, NRF_SUCCESS,
};

/// DWT cycle count of the most recent notification (either edge).
static LAST_EDGE_CYC: AtomicU32 = AtomicU32::new(0);

/// DWT cycle count of the most recent INACTIVE-classified notification.
static LAST_INACTIVE_CYC: AtomicU32 = AtomicU32::new(0);

/// True once any INACTIVE-classified notification has been seen.
static INACTIVE_SEEN: AtomicBool = AtomicBool::new(false);

/// Total notifications since boot (BOTH edges — two per radio event).
static NOTIFY_COUNT: AtomicU32 = AtomicU32::new(0);

/// CPU clock in MHz — CYCCNT cycles / (this × 1000) = milliseconds.
const CPU_MHZ: u32 = 64;

/// Pre-gap threshold separating the two edges: ACTIVE→INACTIVE gaps are
/// ~1-3ms (800µs warning + connection event), INACTIVE→ACTIVE gaps are
/// ~12ms at the measured ~15ms connection interval. 5ms sits between.
const GAP_CLASSIFY_CYC: u32 = 5 * CPU_MHZ * 1000;

/// nRF52840 IRQ number for SWI1_EGU1.
const SWI1_EGU1_IRQN: u32 = 21;

#[inline]
fn cyccnt() -> u32 {
    // SAFETY: CYCCNT is a free-running read-only counter; reading is always safe.
    unsafe { (*cortex_m::peripheral::DWT::PTR).cyccnt.read() }
}

/// Initialize radio notifications. Call once after SoftDevice is enabled.
///
/// Configures `INT_ON_BOTH` with 800µs advance warning, enables the DWT
/// cycle counter used for edge timestamps, and enables the SWI1_EGU1
/// interrupt at priority 2 (same as Embassy, below SoftDevice).
///
/// Returns `true` on success.
#[allow(clippy::cast_possible_truncation)]
pub fn init() -> bool {
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

    // Enable the DWT cycle counter for edge timestamps. DWT/DCB are not
    // managed by the SoftDevice; stealing is safe here (single init at boot).
    {
        let mut p = unsafe { cortex_m::Peripherals::steal() };
        p.DCB.enable_trace();
        p.DWT.enable_cycle_counter();
    }

    // Enable SWI1_EGU1 in the NVIC at priority 2
    // nRF52840 uses 3 priority bits in the upper bits of the priority register
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
    let edge = LAST_INACTIVE_CYC.load(Ordering::Acquire);
    Some(cyccnt().wrapping_sub(edge) / (CPU_MHZ * 1000))
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
    let now = cyccnt();
    let prev = LAST_EDGE_CYC.swap(now, Ordering::AcqRel);
    // Short pre-gap ⇒ this notification ends a connection event (INACTIVE).
    // First-ever notification: prev=0 makes the gap effectively random/huge,
    // which classifies as ACTIVE — safe (we just wait for the next event).
    if now.wrapping_sub(prev) < GAP_CLASSIFY_CYC {
        LAST_INACTIVE_CYC.store(now, Ordering::Release);
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

#[allow(dead_code)]
#[export_name = "SWI1_EGU1"]
unsafe extern "C" fn old_swi1_irq_handler() {
    on_radio_notification();
}
