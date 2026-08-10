// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Sync button monitoring task.

use embassy_nrf::gpio::{Input, Output};
use embassy_time::{Duration, Instant, Timer};

use crate::ble::{get_connection_state, ConnectionState};
use crate::{PROFILE_CHANGE, SYNC_MODE};

// The hold gesture is a pure state machine in maple-protocol, where the
// thresholds live and where the release rules are unit-tested off-target. This
// file supplies the clock and performs the side effects; it does not re-derive
// the decision. See `maple_protocol::sync_hold`.
use maple_protocol::sync_hold::{Release, SyncHold, Tick};
const BLINK_INTERVAL_MS: u64 = 100;
const TRIPLE_PRESS_WINDOW_MS: u64 = 2000;

/// Result of a button hold gesture.
enum HoldResult {
    /// Button released before any threshold.
    ShortPress,
    /// Held 2s — sync mode triggered.
    SyncMode,
    /// Held 7s — goodbye splash + System Off in progress.
    Goodbye,
}

/// Wait while button is held, blinking LED and checking for sync (2s) /
/// OTA DFU (3.5s + controller Start) / sleep (7s).
///
/// Returns `ShortPress` if released early, `SyncMode` if held past 2s but released
/// before 7s. If held 7s, enters System Off directly (never returns on XIAO).
/// If held past 3.5s while the controller's Start button is down, reboots into
/// the bootloader's BLE OTA DFU mode (never returns). Sync mode is only
/// signaled on release — holding through to sleep skips sync so the bond is
/// preserved and the device reconnects on wake.
async fn handle_button_hold(button: &Input<'static>, led: &mut Output<'static>) -> HoldResult {
    let press_start = Instant::now();
    let mut led_state = false;
    let mut last_blink = Instant::now();
    let mut gesture = SyncHold::default();
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

        let tick = gesture.tick(
            elapsed,
            crate::MAPLE_START_HELD.load(core::sync::atomic::Ordering::Relaxed),
        );

        if tick == Tick::CommitSleep {
            log!("SYNC: 7s hold — sleep committed, rendering goodbye");
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

        if tick == Tick::PassedSync {
            past_sync_threshold = true;
            log!("SYNC: Past sync threshold, release for pairing or keep holding for sleep");
        }

        // OTA DFU gesture: sync held past 3.5s WITH the controller's Start button
        // down. Checked every iteration of the 3.5s..7s window (not just at the
        // crossing) so Start pressed mid-hold still counts, and a transient
        // failed poll clearing the mirror only delays the trigger by one 20ms
        // tick. `MAPLE_START_HELD` is only ever true while a controller is
        // being polled, so this can't fire from sync mode or Phase 1 — and a
        // 3.5s two-board chord can't fire by accident.
        //
        // This only *requests* the reboot (see `DFU_PENDING`): the main task
        // owns the bus, so it lands a BOOT splash on the VMU and resets
        // between polls rather than mid-write — expected within one ~5ms poll
        // cycle, long before the 7s sleep commit. If the handoff is ever
        // dropped (BLE falls over in the same instant), holding through to
        // 7s still sleeps — no stuck state.
        if tick == Tick::RequestDfu {
            log!("SYNC: DFU gesture (sync+Start 3.5s) — requesting OTA bootloader");
            crate::DFU_PENDING.store(true, core::sync::atomic::Ordering::Relaxed);
            // Distinct confirmation: a fast triple flash (the reset usually
            // lands mid-flash — that's fine, the flag is already set).
            for _ in 0..3 {
                led.set_low();
                Timer::after(Duration::from_millis(40)).await;
                led.set_high();
                Timer::after(Duration::from_millis(40)).await;
            }
        }

        Timer::after(Duration::from_millis(20)).await;
    }

    // Only signal sync mode on release — not if held through to sleep, and not
    // once the DFU gesture has been recognised. Sync clears the flash bond, and
    // the user asking to update firmware has not asked to be unpaired; without
    // this guard, a DFU request that the low-battery check refuses still costs
    // them their pairing on release.
    match gesture.release() {
        Release::SyncMode => {
            log!("SYNC: Entering pairing mode (60s)");
            SYNC_MODE.signal(());
            HoldResult::SyncMode
        }
        // Deliberately not SyncMode: pairing clears the bond, and asking for a
        // firmware update is not asking to be unpaired. Covered by
        // `sync_hold::tests::dfu_gesture_does_not_clear_the_bond`.
        Release::DfuRequested | Release::ShortPress => HoldResult::ShortPress,
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
/// - Hold 2 seconds: enter pairing/sync mode
/// - Hold 3.5 seconds with controller Start held: reboot into OTA DFU mode
/// - Hold 7 seconds: enter System Off (manual sleep)
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

                // A press while advertising must still be measured as a hold
                // gesture. This previously just spun until release and threw the
                // press away, so `handle_button_hold` never ran in sync mode:
                // the 7s threshold was never reached, `GOODBYE_PENDING` was never
                // set, and **hold-to-sleep was impossible while advertising**.
                // The only way out was to reconnect first.
                //
                // The result is discarded because each variant already did its
                // work inside: `Goodbye` set the flag (Phase 1 renders BYE and
                // sleeps), `SyncMode` re-signals a mode we are already in, and
                // `ShortPress` is a no-op here — matching the old drain.
                if button.is_low() {
                    let _ = handle_button_hold(&button, &mut led).await;
                    Timer::after(Duration::from_millis(100)).await;
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
