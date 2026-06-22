// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! BLE advertising and connection handling task.

use crate::log;
use embassy_time::{Duration, Instant, Timer};
use nrf_softdevice::ble::gatt_server;
use nrf_softdevice::ble::security::SecurityHandler;
use nrf_softdevice::Softdevice;

use crate::ble::{
    advertise, get_connection_state, set_connection_state, AdvertiseMode, Bonder, ConnectionState,
    GamepadServer,
};
use crate::maple::ControllerState;
use crate::{CONTROLLER_STATE, PROFILE_CHANGE, SYNC_MODE, WAKE_REQUEST};
use maple_protocol::guide_chord::GuideChord;
use maple_protocol::xbox_hid::buttons;

#[cfg(feature = "board-xiao")]
use crate::BATTERY_LEVEL;

/// BLE advertising and connection handling task.
///
/// State machine:
/// - `Reconnecting` (60s): Try to connect to bonded device only
/// - `Idle`: Continue trying bonded device (not discoverable)
/// - `SyncMode` (60s): Discoverable to all, accepts new pairings
/// - `Connected`: Active connection
#[allow(clippy::items_after_statements, clippy::too_many_lines)]
#[embassy_executor::task]
pub async fn ble_task(
    sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
) {
    let mut flash = nrf_softdevice::Flash::take(sd);

    // Sync mode timeout: 60 seconds
    const SYNC_TIMEOUT_MS: u64 = 60_000;

    // Tracks whether we've completed at least one successful connection during
    // this power session. Boot does an initial reconnect burst; every later
    // disconnect is treated as user-intentional and stays silent until an
    // explicit WAKE_REQUEST or SYNC_MODE arrives. Without this, the Deck's
    // "Disconnect" button would just immediately re-pair, and that's exactly
    // the loop the user complained about.
    let mut had_connection = false;

    loop {
        // Check for profile switch request (non-blocking)
        if PROFILE_CHANGE.signaled() {
            let next = PROFILE_CHANGE.wait().await;
            log!(
                "PROFILE: Switching to {}",
                core::str::from_utf8(next.profile().vmu_label).unwrap_or("?")
            );
            let _ = crate::ble::flash_bond::save_profile(&mut flash, next).await;
            // Reset to bring up the SoftDevice with the new profile's descriptor.
            cortex_m::peripheral::SCB::sys_reset();
        }

        let state = get_connection_state();

        match state {
            ConnectionState::Reconnecting | ConnectionState::Idle => {
                // Reconnect strategy (matches Xbox / PS controller behavior):
                //  1. One fast-advertising burst (10s, configured in softdevice
                //     advertising config) to catch a brief disconnect or a host
                //     that's still awake.
                //  2. If that times out without a connection, go silent. Don't
                //     keep advertising — it'd repeatedly wake a sleeping host.
                //  3. Wait for an explicit wake signal (sync-button short press
                //     -> WAKE_REQUEST, or 3s hold -> SYNC_MODE).
                //  4. After SLEEP_TIMEOUT_MS total disconnected time, sleep
                //     (XIAO) or fall to Idle (DK).
                let total_start = Instant::now();
                let conn = if bonder.has_bond() {
                    // On boot, do one initial reconnect burst. After any
                    // successful connection in this session, a disconnect is
                    // treated as user-intentional — we go straight to silent
                    // wait until SYNC_MODE or WAKE_REQUEST arrives.
                    let mut wake_pending = !had_connection;
                    loop {
                        if wake_pending {
                            // Phase A: active reconnect window
                            let adv_future =
                                advertise(sd, server, bonder, AdvertiseMode::ReconnectFast);
                            let sync_future = SYNC_MODE.wait();
                            match embassy_futures::select::select(adv_future, sync_future).await {
                                embassy_futures::select::Either::First(Ok(c)) => break Some(c),
                                embassy_futures::select::Either::First(Err(_)) => {
                                    // Phase A timed out without a connection.
                                    // Fall through to the timeout check and
                                    // then Phase B (silent wait). We don't
                                    // clear `wake_pending` because Phase B's
                                    // wake-handler is what re-arms it when the
                                    // user explicitly asks to retry.
                                    log!("BLE: Reconnect window elapsed, going silent");
                                }
                                embassy_futures::select::Either::Second(()) => {
                                    log!("BLE: Sync mode requested");
                                    bonder.clear();
                                    let _ = crate::ble::flash_bond::clear_bond(&mut flash).await;
                                    set_connection_state(ConnectionState::SyncMode);
                                    break None;
                                }
                            }
                        }

                        // Total-disconnect timeout: bail to System Off (XIAO) or
                        // Idle (DK).
                        if total_start.elapsed().as_millis() >= crate::SLEEP_TIMEOUT_MS {
                            #[cfg(feature = "board-xiao")]
                            {
                                log!("BLE: Reconnect timeout, entering System Off");
                                unsafe {
                                    crate::board::enter_system_off();
                                }
                            }
                            #[cfg(not(feature = "board-xiao"))]
                            {
                                log!("BLE: Reconnect timeout, entering idle");
                                set_connection_state(ConnectionState::Idle);
                                break None;
                            }
                        }

                        // Phase B: silent, waiting for an explicit wake gesture.
                        // Drain any stale WAKE_REQUEST so we don't immediately
                        // re-trigger from a wake that happened during phase A.
                        if WAKE_REQUEST.signaled() {
                            WAKE_REQUEST.wait().await;
                        }
                        let wake_future = WAKE_REQUEST.wait();
                        let sync_future = SYNC_MODE.wait();
                        match embassy_futures::select::select(wake_future, sync_future).await {
                            embassy_futures::select::Either::First(()) => {
                                log!("BLE: Wake requested, advertising");
                                wake_pending = true;
                            }
                            embassy_futures::select::Either::Second(()) => {
                                log!("BLE: Sync mode requested");
                                bonder.clear();
                                let _ = crate::ble::flash_bond::clear_bond(&mut flash).await;
                                set_connection_state(ConnectionState::SyncMode);
                                break None;
                            }
                        }
                    }
                } else {
                    // No bonded device - go straight to sync mode
                    log!("BLE: No bond, auto-entering sync mode");
                    set_connection_state(ConnectionState::SyncMode);
                    None
                };

                if let Some(conn) = conn {
                    set_connection_state(ConnectionState::Connected);
                    let outcome = handle_connection(sd, server, bonder, &mut flash, conn).await;
                    match outcome {
                        DisconnectOutcome::SyncRequested => {
                            bonder.clear();
                            let _ = crate::ble::flash_bond::clear_bond(&mut flash).await;
                            set_connection_state(ConnectionState::SyncMode);
                        }
                        DisconnectOutcome::HostIntentional => {
                            // User wants the disconnect to stick (clicked
                            // Disconnect, Deck went to sleep, etc.). Skip the
                            // auto-reconnect burst — wait for an explicit
                            // wake gesture instead.
                            log!("BLE: Host-intentional disconnect, going silent");
                            had_connection = true;
                            transition_after_disconnect(bonder);
                        }
                        DisconnectOutcome::Lost => {
                            // Accidental — try to reconnect once. Leave
                            // had_connection unchanged so the next iteration
                            // runs Phase A.
                            log!("BLE: Connection lost, attempting reconnect");
                            transition_after_disconnect(bonder);
                        }
                    }
                }
            }

            ConnectionState::SyncMode => {
                // Drain any stale sync signal so it doesn't fire after disconnect
                if SYNC_MODE.signaled() {
                    SYNC_MODE.wait().await;
                }

                // Sync mode: discoverable to all for 60 seconds
                let start = Instant::now();

                let conn = loop {
                    if start.elapsed().as_millis() >= SYNC_TIMEOUT_MS {
                        log!("BLE: Sync mode timeout");
                        // Return to appropriate state
                        if bonder.has_bond() {
                            set_connection_state(ConnectionState::Reconnecting);
                        } else {
                            // No bond and sync timed out — sleep to save power.
                            // Wake via sync button → full reset → auto sync mode.
                            #[cfg(feature = "board-xiao")]
                            {
                                log!("BLE: No bond after sync timeout, entering System Off");
                                unsafe {
                                    crate::board::enter_system_off();
                                }
                            }
                            #[cfg(not(feature = "board-xiao"))]
                            set_connection_state(ConnectionState::Idle);
                        }
                        break None;
                    }

                    let adv_future = advertise(sd, server, bonder, AdvertiseMode::SyncMode);

                    if let Ok(Ok(c)) =
                        embassy_time::with_timeout(Duration::from_secs(5), adv_future).await
                    {
                        break Some(c);
                    }
                    // Timeout or error, keep trying
                };

                if let Some(conn) = conn {
                    set_connection_state(ConnectionState::Connected);
                    let outcome = handle_connection(sd, server, bonder, &mut flash, conn).await;
                    match outcome {
                        DisconnectOutcome::SyncRequested => {
                            bonder.clear();
                            let _ = crate::ble::flash_bond::clear_bond(&mut flash).await;
                            set_connection_state(ConnectionState::SyncMode);
                        }
                        DisconnectOutcome::HostIntentional => {
                            log!("BLE: Host-intentional disconnect, going silent");
                            had_connection = true;
                            transition_after_disconnect(bonder);
                        }
                        DisconnectOutcome::Lost => {
                            log!("BLE: Connection lost, attempting reconnect");
                            transition_after_disconnect(bonder);
                        }
                    }
                }
            }

            ConnectionState::Connected => {
                // Shouldn't get here, but handle it
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Why a session ended. Drives the "auto-reconnect or stay silent" decision
/// in the BLE task loop.
enum DisconnectOutcome {
    /// User invoked sync mode while connected — clear bond + advertise discoverable.
    SyncRequested,
    /// Host explicitly terminated (HCI 0x13 / 0x14 / 0x15) — user wants the
    /// disconnect to stick. Don't auto-advertise; wait for a wake gesture.
    HostIntentional,
    /// Connection was lost (timeout, range, error) — accidental, OK to retry.
    Lost,
}

/// Update connection state after a disconnection.
fn transition_after_disconnect(bonder: &Bonder) {
    // Drop the wire-level dedup cache — a new connection has no prior state
    // to compare against, so the first HID report must always go out.
    crate::ble::hid::reset_report_cache();
    if bonder.has_bond() {
        set_connection_state(ConnectionState::Reconnecting);
    } else {
        set_connection_state(ConnectionState::Idle);
    }
}

/// Handle an active BLE connection.
/// Returns the reason the session ended.
#[allow(clippy::too_many_lines)]
async fn handle_connection(
    _sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
    flash: &mut nrf_softdevice::Flash,
    conn: nrf_softdevice::ble::Connection,
) -> DisconnectOutcome {
    log!("BLE: Connected!");

    // If sync was requested before we got here, honor it immediately
    if SYNC_MODE.signaled() {
        SYNC_MODE.wait().await;
        log!("BLE: Sync requested during connection setup");
        return DisconnectOutcome::SyncRequested;
    }

    bonder.load_sys_attrs(&conn);
    Timer::after(Duration::from_millis(100)).await;
    let _ = conn.request_security();

    // Request Xbox-like connection parameters for ~100Hz polling
    Timer::after(Duration::from_millis(500)).await;
    if let Some(handle) = conn.handle() {
        let conn_params = nrf_softdevice::raw::ble_gap_conn_params_t {
            min_conn_interval: 7, // 8.75ms
            max_conn_interval: 9, // 11.25ms
            slave_latency: 0,
            conn_sup_timeout: 400, // 4000ms
        };
        // SAFETY: Connection handle is valid (checked above). conn_params is
        // a well-formed struct on the stack, passed as a const pointer.
        let rc = unsafe {
            nrf_softdevice::raw::sd_ble_gap_conn_param_update(
                handle,
                (&raw const conn_params).cast_mut(),
            )
        };
        if rc != 0 {
            log!("BLE: Conn param update failed: {}", rc);
        }
    }

    // Run GATT server while connected
    let gatt_future = gatt_server::run(&conn, server, |_| {});

    // Notification sender - sends HID reports at fixed 125Hz interval.
    // Reads state changes promptly (so the Signal is cleared before the next
    // poll overwrites it), but always waits for the timer before sending to
    // maintain a steady cadence that matches the BLE connection interval.
    let notify_future = async {
        // Wait for client to discover services and subscribe
        Timer::after(Duration::from_millis(crate::SERVICE_DISCOVERY_DELAY_MS)).await;

        let mut current_state = ControllerState::default();
        let mut notify_fails: u8 = 0;
        let mut guide_chord = GuideChord::default();

        loop {
            // Read any pending state change promptly, then wait for send timer.
            if CONTROLLER_STATE.signaled() {
                current_state = CONTROLLER_STATE.wait().await;
            }

            // Fixed-rate send at ~125Hz — matches Xbox cadence and BLE conn interval
            Timer::after(Duration::from_millis(crate::NOTIFY_INTERVAL_MS)).await;

            // Grab any state that arrived during the wait
            if CONTROLLER_STATE.signaled() {
                current_state = CONTROLLER_STATE.wait().await;
            }

            let mut report = current_state.to_gamepad_report();
            // Synthesize the Guide button from the L+R+Start hold-chord.
            // Detection runs before profile serialization, so both the Xbox and
            // the generic profile get it. State machine lives in maple-protocol
            // (host-tested); here we just feed it the monotonic clock and act.
            let chord = guide_chord.update(&current_state, Instant::now().as_millis());
            if chord.active {
                report.buttons = (report.buttons | buttons::GUIDE) & !buttons::START;
                report.left_trigger = 0;
                report.right_trigger = 0;
            }
            if chord.rising_edge {
                // Best-effort, fire-and-forget: ask the main loop to flash the
                // VMU home glyph. Single non-blocking atomic store; the main
                // loop may drop it. Never touches the controller path.
                crate::GUIDE_GLYPH_PENDING.store(true, core::sync::atomic::Ordering::Relaxed);
            }
            let report_bytes = GamepadServer::serialize_report_for_active_profile(&report);
            let _ = server.hid.report_set(&report_bytes);
            if server.send_report(&conn, &report).is_err() {
                notify_fails += 1;
                if notify_fails > crate::MAX_NOTIFY_FAILURES {
                    log!("BLE: Too many notify failures, disconnecting");
                    break;
                }
            } else {
                notify_fails = 0;
            }
        }
    };

    // Save bond early so it survives unexpected sleep/reset.
    // Polls until pairing completes and bond data is available, then saves once.
    let bond_save_future = async {
        // Wait for pairing to complete (typically 1-3 seconds)
        for _ in 0..10 {
            Timer::after(Duration::from_secs(1)).await;
            bonder.save_sys_attrs(&conn);
            if let Some((master_id, enc_info, peer_id)) = bonder.get_bond_data() {
                let sys_attrs = bonder.get_sys_attrs();
                let _ = crate::ble::flash_bond::save_bond(
                    flash, &master_id, &enc_info, &peer_id, &sys_attrs,
                )
                .await;
                log!("BLE: Bond saved");
                break;
            }
        }
        // Keep future alive, checking for profile switch requests
        loop {
            if PROFILE_CHANGE.signaled() {
                let next = PROFILE_CHANGE.wait().await;
                log!(
                    "PROFILE: Switching to {}",
                    core::str::from_utf8(next.profile().vmu_label).unwrap_or("?")
                );
                let _ = crate::ble::flash_bond::save_profile(flash, next).await;
                cortex_m::peripheral::SCB::sys_reset();
            }
            Timer::after(Duration::from_millis(100)).await;
        }
    };

    // Update battery level in BLE service when signaled (XIAO only).
    // 0xFF = charging (don't update percentage), otherwise 0-100%.
    #[cfg(feature = "board-xiao")]
    let battery_future = async {
        loop {
            let level = BATTERY_LEVEL.wait().await;
            if level != 0xFF {
                let _ = server.battery.battery_level_set(&level);
                let _ = server.battery.battery_level_notify(&conn, &level);
            }
        }
    };

    // Wait for sync mode request — disconnects active connection
    let sync_future = SYNC_MODE.wait();

    // Run all until one completes (connection drops or sync requested)
    #[cfg(feature = "board-xiao")]
    let sync_requested = {
        let main_futures =
            embassy_futures::select::select3(gatt_future, notify_future, battery_future);
        let combined =
            embassy_futures::select::select3(main_futures, bond_save_future, sync_future);
        match combined.await {
            embassy_futures::select::Either3::First(inner) => {
                match inner {
                    embassy_futures::select::Either3::First(_gatt_result) => {
                        log!("BLE: Disconnected (GATT: {:?})", _gatt_result);
                    }
                    embassy_futures::select::Either3::Second(()) => {
                        log!("BLE: Disconnected (notify failure)");
                    }
                    embassy_futures::select::Either3::Third(()) => {
                        log!("BLE: Disconnected (battery task ended)");
                    }
                }
                false
            }
            embassy_futures::select::Either3::Second(()) => unreachable!(),
            embassy_futures::select::Either3::Third(()) => {
                log!("BLE: Sync mode requested, disconnecting");
                true
            }
        }
    };
    #[cfg(not(feature = "board-xiao"))]
    let sync_requested = {
        let main_futures = embassy_futures::select::select(gatt_future, notify_future);
        let combined =
            embassy_futures::select::select3(main_futures, bond_save_future, sync_future);
        match combined.await {
            embassy_futures::select::Either3::First(inner) => {
                match inner {
                    embassy_futures::select::Either::First(_gatt_result) => {
                        log!("BLE: Disconnected (GATT: {:?})", _gatt_result);
                    }
                    embassy_futures::select::Either::Second(()) => {
                        log!("BLE: Disconnected (notify failure)");
                    }
                }
                false
            }
            embassy_futures::select::Either3::Second(()) => unreachable!(),
            embassy_futures::select::Either3::Third(()) => {
                log!("BLE: Sync mode requested, disconnecting");
                true
            }
        }
    };

    // Explicitly disconnect so the host sees a clean GAP termination
    // before we start advertising in sync mode.
    if sync_requested {
        let _ = conn.disconnect();
        // Give the host time to process the disconnect
        Timer::after(Duration::from_millis(1000)).await;
        return DisconnectOutcome::SyncRequested;
    }

    bonder.save_sys_attrs(&conn);
    if let Some((master_id, enc_info, peer_id)) = bonder.get_bond_data() {
        let sys_attrs = bonder.get_sys_attrs();
        let _ =
            crate::ble::flash_bond::save_bond(flash, &master_id, &enc_info, &peer_id, &sys_attrs)
                .await;
    }
    Timer::after(Duration::from_millis(500)).await;

    // Read the HCI disconnect reason from the (now-disconnected) Connection
    // and classify. Host-initiated termination (0x13 user, 0x14 low resources,
    // 0x15 power off) means the user wants the disconnect to stick. Anything
    // else (timeout, error) is treated as accidental — eligible for auto retry.
    use nrf_softdevice::ble::HciStatus;
    let reason = conn.disconnect_reason();
    log!("BLE: Disconnect reason = {:?}", reason);
    match reason {
        Some(HciStatus::REMOTE_USER_TERMINATED_CONNECTION)
        | Some(HciStatus::REMOTE_DEV_TERMINATION_DUE_TO_LOW_RESOURCES)
        | Some(HciStatus::REMOTE_DEV_TERMINATION_DUE_TO_POWER_OFF) => {
            DisconnectOutcome::HostIntentional
        }
        _ => DisconnectOutcome::Lost,
    }
}
