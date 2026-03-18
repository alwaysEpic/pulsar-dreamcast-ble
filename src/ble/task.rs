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
use crate::{CONTROLLER_STATE, NAME_TOGGLE, SYNC_MODE};

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

    loop {
        // Check for name toggle request (non-blocking)
        if NAME_TOGGLE.signaled() {
            let new_pref = NAME_TOGGLE.wait().await;
            log!(
                "NAME: Toggling to {}",
                if new_pref { "Dreamcast" } else { "Xbox" }
            );
            let _ = crate::ble::flash_bond::save_name_preference(&mut flash, new_pref).await;
            // Reset to apply new name (set at SoftDevice init)
            cortex_m::peripheral::SCB::sys_reset();
        }

        let state = get_connection_state();

        match state {
            ConnectionState::Reconnecting | ConnectionState::Idle => {
                // Try to reconnect to bonded device (not discoverable)
                let conn = if bonder.has_bond() {
                    // Have a bonded device - advertise in reconnect mode (not discoverable)
                    let start = Instant::now();

                    loop {
                        // Use fast advertising (20ms) for first 5s, then slow (100ms)
                        let mode = if start.elapsed().as_millis() < 5000 {
                            AdvertiseMode::ReconnectFast
                        } else {
                            AdvertiseMode::Reconnect
                        };
                        let adv_future = advertise(sd, server, bonder, mode);
                        let sync_future = SYNC_MODE.wait();

                        match embassy_futures::select::select(adv_future, sync_future).await {
                            embassy_futures::select::Either::First(result) => {
                                if let Ok(c) = result {
                                    break Some(c);
                                }
                                // Check timeout in Reconnecting state
                                if get_connection_state() == ConnectionState::Reconnecting
                                    && start.elapsed().as_millis() >= crate::SLEEP_TIMEOUT_MS
                                {
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
                                    }
                                }
                                Timer::after(Duration::from_millis(500)).await;
                            }
                            embassy_futures::select::Either::Second(()) => {
                                // Sync mode triggered
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
                    let sync = handle_connection(sd, server, bonder, &mut flash, conn).await;
                    if sync {
                        bonder.clear();
                        let _ = crate::ble::flash_bond::clear_bond(&mut flash).await;
                        set_connection_state(ConnectionState::SyncMode);
                    } else {
                        transition_after_disconnect(bonder);
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
                    let sync = handle_connection(sd, server, bonder, &mut flash, conn).await;
                    if sync {
                        bonder.clear();
                        let _ = crate::ble::flash_bond::clear_bond(&mut flash).await;
                        set_connection_state(ConnectionState::SyncMode);
                    } else {
                        transition_after_disconnect(bonder);
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

/// Update connection state after a disconnection.
fn transition_after_disconnect(bonder: &Bonder) {
    if bonder.has_bond() {
        set_connection_state(ConnectionState::Reconnecting);
    } else {
        set_connection_state(ConnectionState::Idle);
    }
}

/// Handle an active BLE connection.
/// Returns `true` if disconnected due to sync mode request.
#[allow(clippy::too_many_lines)]
async fn handle_connection(
    _sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
    flash: &mut nrf_softdevice::Flash,
    conn: nrf_softdevice::ble::Connection,
) -> bool {
    log!("BLE: Connected!");

    // If sync was requested before we got here, honor it immediately
    if SYNC_MODE.signaled() {
        SYNC_MODE.wait().await;
        log!("BLE: Sync requested during connection setup");
        return true;
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

            let report = current_state.to_gamepad_report();
            let report_bytes = report.to_bytes();
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
        // Keep future alive, checking for name toggle requests
        loop {
            if NAME_TOGGLE.signaled() {
                let new_pref = NAME_TOGGLE.wait().await;
                log!(
                    "NAME: Toggling to {}",
                    if new_pref { "Dreamcast" } else { "Xbox" }
                );
                let _ = crate::ble::flash_bond::save_name_preference(flash, new_pref).await;
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
                    embassy_futures::select::Either3::First(gatt_result) => {
                        log!("BLE: Disconnected (GATT: {:?})", gatt_result);
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
                    embassy_futures::select::Either::First(gatt_result) => {
                        log!("BLE: Disconnected (GATT: {:?})", gatt_result);
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
    } else {
        bonder.save_sys_attrs(&conn);
        if let Some((master_id, enc_info, peer_id)) = bonder.get_bond_data() {
            let sys_attrs = bonder.get_sys_attrs();
            let _ = crate::ble::flash_bond::save_bond(
                flash, &master_id, &enc_info, &peer_id, &sys_attrs,
            )
            .await;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    sync_requested
}
