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
use maple_protocol::sync_hold::{Release, SyncHold, Tick, DFU_MS};
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
    /// The DFU gesture was taken. Distinct from `ShortPress` on purpose: the
    /// caller must **not** count this as a press. `main`'s battery gate can
    /// refuse the update after the fact, and with the tap-tap-hold chord the
    /// press counter is already at 2 — counting the release would make it 3 and
    /// fire the profile toggle as a parting gift for a refused update.
    DfuRequested,
    /// The one-tap-then-hold configuration gesture was taken. Kept distinct
    /// so a failed marker write cannot fall through into pairing and erase the
    /// game-host bond.
    ConfigRequested,
}

/// Wait while button is held, blinking LED and checking for sync (2s), isolated
/// configuration (tap-hold at 3.5s), OTA DFU (3.5s, armed by controller Start
/// or the tap-tap-hold chord, with priority over configuration), or sleep (7s).
///
/// Returns `ShortPress` if released early, `SyncMode` if held past 2s but released
/// before 7s. If held 7s, enters System Off directly (never returns on XIAO).
/// If held past 3.5s while DFU is armed, requests the bootloader's BLE OTA DFU
/// mode. Sync mode is only signaled on release — holding through to sleep skips
/// sync so the bond is preserved and the device reconnects on wake.
///
/// `dfu_chord_armed` is the caller's tap-tap-hold determination. It is OR'd with
/// the controller's Start mirror, so either arms DFU — see
/// [`maple_protocol::sync_hold`] for why the controller-independent path exists.
async fn handle_button_hold(
    button: &Input<'static>,
    led: &mut Output<'static>,
    dfu_chord_armed: bool,
    config_chord_armed: bool,
) -> HoldResult {
    let press_start = Instant::now();
    let mut led_state = false;
    let mut last_blink = Instant::now();
    let mut gesture = SyncHold::default();
    let mut past_sync_threshold = false;
    let mut config_requested = false;

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

        let dfu_armed =
            dfu_chord_armed || crate::MAPLE_START_HELD.load(core::sync::atomic::Ordering::Relaxed);
        let tick = gesture.tick(elapsed, dfu_armed);

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

        // OTA DFU gesture: sync held past 3.5s with DFU armed — either by the
        // controller's Start button, or by the tap-tap-hold chord. Checked every
        // iteration of the 3.5s..7s window (not just at the crossing) so Start
        // pressed mid-hold still counts, and a transient failed poll clearing the
        // mirror only delays the trigger by one 20ms tick.
        //
        // The Start path cannot fire from sync mode or Phase 1, because
        // `MAPLE_START_HELD` is only ever true while a controller is being polled.
        // That was originally stated as a safety property; it is really a
        // limitation, and it locked out exactly the units that most needed an
        // update — no controller docked, or a Maple side that had stopped
        // answering. The chord reaches those, and needs two deliberate taps
        // immediately before a 3.5s hold, so it cannot fire by accident either.
        //
        // This only *requests* the reboot (see `DFU_PENDING`): the main task
        // owns the bus, so it lands a BOOT splash on the VMU and resets
        // between polls rather than mid-write — expected within one ~5ms poll
        // cycle, long before the 7s sleep commit. If the handoff is ever
        // dropped (BLE falls over in the same instant), holding through to
        // 7s still sleeps — no stuck state.
        if tick == Tick::RequestDfu {
            log!(
                "SYNC: DFU gesture at 3.5s (chord={}) — requesting OTA bootloader",
                dfu_chord_armed
            );
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

        // Browser configuration: one short tap followed by a hold through the
        // same 3.5 s threshold. DFU is evaluated above and wins whenever Start
        // or the tap-tap chord arms it. A failed marker write is latched as a
        // configuration attempt so release cannot enter sync mode and clear the
        // existing game-host bond.
        if elapsed >= DFU_MS
            && config_chord_armed
            && !dfu_armed
            && !gesture.dfu_requested()
            && !config_requested
        {
            config_requested = true;
            log!("SYNC: configuration gesture — requesting isolated personality");
            if !crate::reboot_into_config() {
                log!("SYNC: GPREGRET2 configuration marker write failed");
            }
        }

        Timer::after(Duration::from_millis(20)).await;
    }

    // Only signal sync mode on release — not if held through to sleep, and not
    // once the DFU gesture has been recognised. Sync clears the flash bond, and
    // the user asking to update firmware has not asked to be unpaired; without
    // this guard, a DFU request that the low-battery check refuses still costs
    // them their pairing on release.
    if config_requested {
        return HoldResult::ConfigRequested;
    }

    match gesture.release() {
        Release::SyncMode => {
            log!("SYNC: Entering pairing mode (60s)");
            SYNC_MODE.signal(());
            HoldResult::SyncMode
        }
        // Deliberately not SyncMode: pairing clears the bond, and asking for a
        // firmware update is not asking to be unpaired. Covered by
        // `sync_hold::tests::dfu_gesture_does_not_clear_the_bond`.
        Release::DfuRequested => HoldResult::DfuRequested,
        Release::ShortPress => HoldResult::ShortPress,
    }
}

/// Sleep up to `ms`, returning `true` early the moment the button goes down.
///
/// Sync mode blinks on a 200 ms cadence and used to sleep straight through both
/// halves, so a press was only noticed every ~400 ms. That is far too coarse for
/// the tap-tap-hold chord, whose whole run has to fit inside
/// `DFU_CHORD_WINDOW_MS` — two taps at 400 ms granularity can eat the entire
/// window before the hold even starts. Poll at the same 20 ms cadence the hold
/// machine uses.
async fn blink_wait(button: &Input<'static>, ms: u64) -> bool {
    let start = Instant::now();
    while start.elapsed().as_millis() < ms {
        if button.is_low() {
            return true;
        }
        Timer::after(Duration::from_millis(20)).await;
    }
    button.is_low()
}

/// Handle triple-press detection and profile toggle.
async fn handle_triple_press(led: &mut Output<'static>) {
    let current = crate::ble::prefs::load_prefs().profile_id;
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
/// - **Tap, tap, then hold 3.5 seconds: same OTA DFU mode, no controller needed**
/// - **Tap once, then hold 3.5 seconds: reboot into isolated browser configuration**
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
                let mut pressed = blink_wait(&button, 200).await;
                if !pressed {
                    led.set_high();
                    pressed = blink_wait(&button, 200).await;
                }

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
                // The press counter is maintained here too, so the DFU chord can
                // arm while advertising. It previously could not: this branch
                // discarded the result and passed `chord = false`, which made
                // "advertising, not yet paired" — a normal state to want to
                // reflash from — the one state the controller-free gesture was
                // unusable in. Found the hard way on 2026-08-17, when the chord
                // appeared not to work at all.
                if pressed {
                    if press_count > 0
                        && first_press_time.elapsed().as_millis() >= TRIPLE_PRESS_WINDOW_MS
                    {
                        press_count = 0;
                    }
                    let since_first = first_press_time.elapsed().as_millis();
                    let chord =
                        maple_protocol::sync_hold::dfu_chord_armed(press_count, since_first);
                    let config_chord =
                        maple_protocol::sync_hold::config_chord_armed(press_count, since_first);
                    match handle_button_hold(&button, &mut led, chord, config_chord).await {
                        HoldResult::ShortPress => {
                            if press_count == 0 {
                                first_press_time = Instant::now();
                            }
                            press_count += 1;
                        }
                        // Goodbye already set its flag, SyncMode re-signals a mode
                        // we are already in, and DfuRequested must not count as a
                        // press — all three just reset the run.
                        HoldResult::SyncMode
                        | HoldResult::Goodbye
                        | HoldResult::DfuRequested
                        | HoldResult::ConfigRequested => {
                            press_count = 0;
                        }
                    }
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

        // Button pressed — detect hold gesture. Any taps already counted in this
        // window arm the controller-free DFU chord: "tap, tap, hold" reaches here
        // with `press_count == 2`, because the hold itself is the third press and
        // so was never counted as a short one.
        let since_first = first_press_time.elapsed().as_millis();
        let chord = maple_protocol::sync_hold::dfu_chord_armed(press_count, since_first);
        let config_chord = maple_protocol::sync_hold::config_chord_armed(press_count, since_first);

        match handle_button_hold(&button, &mut led, chord, config_chord).await {
            // `DfuRequested` belongs here rather than with `ShortPress`: it must
            // reset the counter without signalling wake or counting a press.
            // `main` may still refuse on the battery gate, and with the chord the
            // counter is already at 2 — counting this release would make it 3 and
            // toggle the profile as a parting gift for a refused update.
            HoldResult::SyncMode
            | HoldResult::Goodbye
            | HoldResult::DfuRequested
            | HoldResult::ConfigRequested => {
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
