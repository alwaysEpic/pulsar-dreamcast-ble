// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Sync button monitoring task.

use embassy_nrf::gpio::{Input, Output};
use embassy_time::{Duration, Instant, Timer};
use rtt_target::rprintln;

use crate::ble::{get_connection_state, ConnectionState};
use crate::{NAME_TOGGLE, SYNC_MODE};

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
}

/// Wait while button is held, blinking LED and checking for sync (3s) / sleep (10s).
///
/// Returns `ShortPress` if released early, `SyncMode` if held past 3s.
/// If held 10s, enters System Off directly (never returns on XIAO).
async fn handle_button_hold(button: &Input<'static>, led: &mut Output<'static>) -> HoldResult {
    let press_start = Instant::now();
    let mut led_state = false;
    let mut last_blink = Instant::now();
    let mut sync_triggered = false;

    while button.is_low() {
        let elapsed = press_start.elapsed().as_millis();

        // Blink LED — faster after sync triggers to indicate sleep is coming
        let blink_rate = if sync_triggered {
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
            rprintln!("SYNC: 10s hold — waiting for release, then System Off");
            // Solid LED to confirm sleep is committed
            led.set_low();
            while button.is_low() {
                Timer::after(Duration::from_millis(50)).await;
            }
            #[cfg(feature = "board-xiao")]
            unsafe {
                crate::board::enter_system_off();
            }
        }

        if !sync_triggered && elapsed >= HOLD_SYNC_MS {
            sync_triggered = true;
            rprintln!("SYNC: Entering pairing mode (60s)");
            SYNC_MODE.signal(());
        }

        Timer::after(Duration::from_millis(20)).await;
    }

    if sync_triggered {
        HoldResult::SyncMode
    } else {
        HoldResult::ShortPress
    }
}

/// Handle triple-press detection and name toggle.
async fn handle_triple_press(led: &mut Output<'static>) {
    let current = crate::ble::flash_bond::load_name_preference();
    let new_pref = !current;
    rprintln!(
        "NAME: Triple-press! Switching to {}",
        if new_pref { "Dreamcast" } else { "Xbox" }
    );

    // LED confirmation: 5 rapid blinks
    for _ in 0..5 {
        led.set_low();
        Timer::after(Duration::from_millis(50)).await;
        led.set_high();
        Timer::after(Duration::from_millis(50)).await;
    }

    NAME_TOGGLE.signal(new_pref);
}

/// Sync button monitoring task.
///
/// - Hold 3 seconds: enter pairing/sync mode
/// - Hold 10 seconds: enter System Off (manual sleep)
/// - Triple-press within 2 seconds: toggle device name (Xbox <-> Dreamcast) and reset
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
            HoldResult::SyncMode => {
                press_count = 0;
            }
            HoldResult::ShortPress => {
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
