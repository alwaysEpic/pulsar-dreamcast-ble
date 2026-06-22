// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Sync button monitoring task.

use crate::log;
use embassy_nrf::gpio::{Input, Output};
use embassy_time::{Duration, Instant, Timer};

use crate::ble::{get_connection_state, ConnectionState};
use crate::{PROFILE_CHANGE, SYNC_MODE};

const HOLD_SYNC_MS: u64 = 2000;
const HOLD_SLEEP_MS: u64 = 7000;
const BLINK_INTERVAL_MS: u64 = 100;
const TRIPLE_PRESS_WINDOW_MS: u64 = 2000;

/// Result of a button hold gesture.
enum HoldResult {
    /// Button released before any threshold.
    ShortPress,
    /// Held 3s — sync mode triggered.
    SyncMode,
    /// Held 10s — goodbye splash + System Off in progress.
    Goodbye,
}

/// Wait while button is held, blinking LED and checking for sync (3s) / sleep (10s).
///
/// Returns `ShortPress` if released early, `SyncMode` if held past 3s but released
/// before 10s. If held 10s, enters System Off directly (never returns on XIAO).
/// Sync mode is only signaled on release — holding through to sleep skips sync
/// so the bond is preserved and the device reconnects on wake.
async fn handle_button_hold(button: &Input<'static>, led: &mut Output<'static>) -> HoldResult {
    let press_start = Instant::now();
    let mut led_state = false;
    let mut last_blink = Instant::now();
    let mut past_sync_threshold = false;

    while button.is_low() {
        let elapsed = press_start.elapsed().as_millis();

        // Blink LED — faster after sync threshold to indicate sleep is coming
        let blink_rate = if past_sync_threshold {
            BLINK_INTERVAL_MS / 2
        } else {
            BLINK_INTERVAL_MS
        };
        if last_blink.elapsed().as_millis() >= blink_rate {
            led_state = !led_state;
            if led_state {
                led.set_low();
            } else {
                led.set_high();
            }
            last_blink = Instant::now();
        }

        if elapsed >= HOLD_SLEEP_MS {
            log!("SYNC: 10s hold — sleep committed, rendering goodbye");
            // Solid LED to confirm sleep is committed
            led.set_low();

            // Set the flag immediately (during the hold, not after release).
            // The main task — whether it's in Phase 1 (waiting for BLE) or
            // Phase 3 (connected) — picks this up and handles the BYE splash
            // + System Off transition. Both boards see this flag; on XIAO the
            // main task will actually sleep, on DK it halts after BYE so the
            // goodbye flow is testable on the dev kit too.
            crate::GOODBYE_PENDING.store(true, core::sync::atomic::Ordering::Relaxed);

            // Wait for release (LED stays solid as visual confirmation).
            while button.is_low() {
                Timer::after(Duration::from_millis(50)).await;
            }
            return HoldResult::Goodbye;
        }

        if !past_sync_threshold && elapsed >= HOLD_SYNC_MS {
            past_sync_threshold = true;
            log!("SYNC: Past sync threshold, release for pairing or keep holding for sleep");
        }

        Timer::after(Duration::from_millis(20)).await;
    }

    // Only signal sync mode on release — not if held through to sleep
    if past_sync_threshold {
        log!("SYNC: Entering pairing mode (60s)");
        SYNC_MODE.signal(());
        HoldResult::SyncMode
    } else {
        HoldResult::ShortPress
    }
}

/// Handle triple-press detection and profile toggle.
async fn handle_triple_press(led: &mut Output<'static>) {
    let current = crate::ble::flash_bond::load_profile();
    let next = current.next();
    log!(
        "PROFILE: Triple-press! Switching {} -> {}",
        core::str::from_utf8(current.profile().vmu_label).unwrap_or("?"),
        core::str::from_utf8(next.profile().vmu_label).unwrap_or("?"),
    );

    // LED confirmation: 5 rapid blinks
    for _ in 0..5 {
        led.set_low();
        Timer::after(Duration::from_millis(50)).await;
        led.set_high();
        Timer::after(Duration::from_millis(50)).await;
    }

    PROFILE_CHANGE.signal(next);
}

/// Sync button monitoring task.
///
/// - Hold 3 seconds: enter pairing/sync mode
/// - Hold 10 seconds: enter System Off (manual sleep)
/// - Triple-press within 2 seconds: toggle BLE profile (Xbox <-> Generic) and reset
///
/// LED behavior based on `ConnectionState`:
/// - `Idle`/`Reconnecting`: OFF
/// - `SyncMode`: Fast blink (200ms on/off)
/// - `Connected`: Solid ON
#[embassy_executor::task]
pub async fn sync_button_task(button: Input<'static>, mut led: Output<'static>) {
    // Let pull-up settle before reading button state
    Timer::after(Duration::from_millis(100)).await;

    let mut press_count: u8 = 0;
    let mut first_press_time = Instant::now();

    loop {
        let state = get_connection_state();

        // Update LED based on state
        match state {
            ConnectionState::Connected => {
                led.set_low(); // LED on (active low)
            }
            ConnectionState::SyncMode => {
                led.set_low();
                Timer::after(Duration::from_millis(200)).await;
                led.set_high();
                Timer::after(Duration::from_millis(200)).await;

                // Check for button press to cancel sync mode early
                if button.is_low() {
                    Timer::after(Duration::from_millis(100)).await;
                    while button.is_low() {
                        Timer::after(Duration::from_millis(50)).await;
                    }
                }
                continue;
            }
            ConnectionState::Idle | ConnectionState::Reconnecting => {
                led.set_high(); // LED off
            }
        }

        // Check for button press (active low)
        if button.is_high() {
            if press_count > 0 && first_press_time.elapsed().as_millis() >= TRIPLE_PRESS_WINDOW_MS {
                press_count = 0;
            }
            Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        // Button pressed — detect hold gesture
        match handle_button_hold(&button, &mut led).await {
            HoldResult::SyncMode | HoldResult::Goodbye => {
                press_count = 0;
            }
            HoldResult::ShortPress => {
                // Every short press also signals wake, matching the Xbox / PS
                // pattern where the wake button is also the home button.
                // While disconnected this brings the BLE task back from its
                // silent reconnect-wait. While connected it's a no-op.
                log!("BUTTON: Short press, requesting wake");
                crate::WAKE_REQUEST.signal(());

                if press_count == 0 {
                    first_press_time = Instant::now();
                }
                press_count += 1;

                if press_count >= 3
                    && first_press_time.elapsed().as_millis() < TRIPLE_PRESS_WINDOW_MS
                {
                    handle_triple_press(&mut led).await;
                    press_count = 0;
                }
            }
        }

        // Debounce
        Timer::after(Duration::from_millis(100)).await;
    }
}
