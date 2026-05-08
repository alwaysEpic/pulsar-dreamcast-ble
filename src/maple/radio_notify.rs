// SPDX-License-Identifier: GPL-3.0-or-later

//! SoftDevice radio notification gate for Maple Bus LCD writes.
//!
//! The BLE SoftDevice fires connection events every 8.75-11.25ms at interrupt
//! priority 0, which can corrupt long bit-bang transmissions (~1.65ms LCD writes).
//!
//! This module uses `sd_radio_notification_cfg_set()` to track when the radio
//! goes idle after each connection event. The ~6-10ms idle window between events
//! is more than enough for an LCD write. By only starting writes when the radio
//! is known to be idle, we avoid most SoftDevice interference without disrupting
//! BLE (unlike the timeslot API).
//!
//! # Usage
//!
//! 1. Call [`init`] once after the SoftDevice is enabled.
//! 2. Before starting a VMU LCD write, check [`is_radio_idle`].
//! 3. If idle, proceed with the write. If not, skip and retry next poll.

use core::sync::atomic::{AtomicBool, Ordering};
use nrf_softdevice_s140::{
    self as sd, NRF_RADIO_NOTIFICATION_DISTANCES_NRF_RADIO_NOTIFICATION_DISTANCE_800US,
    NRF_RADIO_NOTIFICATION_TYPES_NRF_RADIO_NOTIFICATION_TYPE_INT_ON_BOTH, NRF_SUCCESS,
};

/// True when the radio is idle (between connection events).
/// Set on the INACTIVE edge, cleared on the ACTIVE edge.
static RADIO_IDLE: AtomicBool = AtomicBool::new(false);

/// Tracks which edge we expect next.
/// With `INT_ON_BOTH`, the first interrupt after enabling is always the
/// ACTIVE edge (800µs before the next radio event). We track the phase
/// ourselves since SWI1 doesn't indicate which edge fired.
static EXPECTING_ACTIVE: AtomicBool = AtomicBool::new(true);

/// nRF52840 IRQ number for SWI1_EGU1.
const SWI1_EGU1_IRQN: u32 = 21;

/// Initialize radio notifications. Call once after SoftDevice is enabled.
///
/// Configures `INT_ON_BOTH` with 800µs advance warning, and enables the
/// SWI1_EGU1 interrupt at priority 2 (same as Embassy, below SoftDevice).
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

    // Start expecting the ACTIVE edge
    EXPECTING_ACTIVE.store(true, Ordering::Release);
    RADIO_IDLE.store(false, Ordering::Release);

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

/// Check if the radio is currently idle (safe to do a long Maple Bus TX).
pub fn is_radio_idle() -> bool {
    RADIO_IDLE.load(Ordering::Acquire)
}

/// SWI1 interrupt handler — called by the SoftDevice for radio notifications.
///
/// With `INT_ON_BOTH`, this fires twice per connection event:
/// - ACTIVE edge: ~800µs before the radio starts → mark radio as busy
/// - INACTIVE edge: radio just finished → mark radio as idle
fn on_radio_notification() {
    if EXPECTING_ACTIVE.load(Ordering::Acquire) {
        // ACTIVE edge: radio is about to start
        RADIO_IDLE.store(false, Ordering::Release);
        EXPECTING_ACTIVE.store(false, Ordering::Release);
    } else {
        // INACTIVE edge: radio just finished
        RADIO_IDLE.store(true, Ordering::Release);
        EXPECTING_ACTIVE.store(true, Ordering::Release);
    }
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
