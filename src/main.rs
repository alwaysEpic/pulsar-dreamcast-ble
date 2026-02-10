#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pull};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use nrf_softdevice::ble::gatt_server;
use nrf_softdevice::ble::security::SecurityHandler;
use nrf_softdevice::Softdevice;
use panic_halt as _;
use rtt_target::{rprintln, rtt_init_print};
use static_cell::StaticCell;

mod ble;
mod board;
mod maple;

use crate::ble::{
    advertise, get_connection_state, init_softdevice, set_connection_state, set_name_mode,
    AdvertiseMode, Bonder, ConnectionState, GamepadServer,
};
use crate::maple::host::MapleResult;
use crate::maple::{ControllerState, MapleBus, MapleHost};

/// Shared controller state between maple and BLE tasks.
static CONTROLLER_STATE: Signal<CriticalSectionRawMutex, ControllerState> = Signal::new();

/// Signal to trigger sync/pairing mode (clears bonds).
static SYNC_MODE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Signal to toggle device name and reset. Carries new is_dreamcast value.
static NAME_TOGGLE: Signal<CriticalSectionRawMutex, bool> = Signal::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    rtt_init_print!();
    rprintln!("DC Adapter Starting");

    // Initialize Embassy with interrupt priorities that don't conflict with SoftDevice
    let mut config = embassy_nrf::config::Config::default();
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    let p = embassy_nrf::init(config);

    // Load name preference from flash (Xbox vs Dreamcast)
    let is_dreamcast = crate::ble::flash_bond::load_name_preference();
    if is_dreamcast {
        rprintln!("Name: Dreamcast Wireless Controller");
    } else {
        rprintln!("Name: Xbox Wireless Controller");
    }

    // Initialize SoftDevice with chosen name
    set_name_mode(is_dreamcast);
    let sd = init_softdevice(is_dreamcast);

    // Create HID Gamepad GATT server
    let server = match GamepadServer::new(sd) {
        Ok(s) => s,
        Err(_) => loop {
            cortex_m::asm::wfi();
        },
    };
    static SERVER: StaticCell<GamepadServer> = StaticCell::new();
    let server = SERVER.init(server);
    let _ = server.init();

    // Spawn the SoftDevice runner task
    if let Ok(token) = softdevice_task(sd) {
        spawner.spawn(token);
    }

    // Create bonder for security/pairing
    static BONDER: StaticCell<Bonder> = StaticCell::new();
    let bonder = BONDER.init(Bonder::new());

    // Load bonding data from flash if available
    if let Some((master_id, enc_info, peer_id, sys_attrs)) = crate::ble::flash_bond::load_bond() {
        bonder.load_from_flash(master_id, enc_info, peer_id, sys_attrs);
    }

    // Spawn BLE task
    if let Ok(token) = ble_task(sd, server, bonder) {
        spawner.spawn(token);
    }

    // LEDs (active low on DK)
    let led1 = Output::new(p.P0_13, Level::High, OutputDrive::Standard);
    let mut led2 = Output::new(p.P0_14, Level::High, OutputDrive::Standard);
    let mut led3 = Output::new(p.P0_15, Level::High, OutputDrive::Standard);
    let mut led4 = Output::new(p.P0_16, Level::High, OutputDrive::Standard);

    // Sync button (Button 4 on DK = P0.25, active low) with LED1 for feedback
    let sync_button = Input::new(p.P0_25, Pull::Up);
    if let Ok(token) = sync_button_task(sync_button, led1) {
        spawner.spawn(token);
    }

    // Startup blink (use LED2 since LED1 is owned by sync task)
    for _ in 0..3 {
        led2.set_low();
        Timer::after(Duration::from_millis(100)).await;
        led2.set_high();
        Timer::after(Duration::from_millis(100)).await;
    }

    // Set up Maple Bus using Flex pins
    let sdcka = Flex::new(p.P0_05);
    let sdckb = Flex::new(p.P0_06);
    let mut bus = MapleBus::new(sdcka, sdckb);
    let host = MapleHost::new();

    // Detect controller
    led2.set_low();
    let result = host.request_device_info(&mut bus);

    let controller_detected = match &result {
        MapleResult::Ok(_) => {
            led2.set_high();
            led3.set_low();
            true
        }
        _ => {
            led2.set_high();
            led4.set_low();
            false
        }
    };

    if !controller_detected {
        // Button 1 (P0.11) to retry - triggers system reset
        let reset_button = Input::new(p.P0_11, Pull::Up);
        loop {
            if reset_button.is_low() {
                // Debounce
                cortex_m::asm::delay(1_000_000);
                if reset_button.is_low() {
                    cortex_m::peripheral::SCB::sys_reset();
                }
            }
            cortex_m::asm::wfi();
        }
    }

    let mut last_state: Option<ControllerState> = None;

    loop {
        if let MapleResult::Ok(state) = host.get_condition(&mut bus) {
            // Only signal when state changes
            let changed = match &last_state {
                None => true,
                Some(prev) => state_changed(prev, &state),
            };

            if changed {
                CONTROLLER_STATE.signal(state);
                last_state = Some(state);
            }
        }

        Timer::after(Duration::from_millis(16)).await;
    }
}

/// SoftDevice runner task - must run continuously.
#[embassy_executor::task]
async fn softdevice_task(sd: &'static Softdevice) {
    sd.run().await;
}

/// BLE advertising and connection handling task.
///
/// State machine:
/// - Reconnecting (60s): Try to connect to bonded device only
/// - Idle: Continue trying bonded device (not discoverable)
/// - SyncMode (60s): Discoverable to all, accepts new pairings
/// - Connected: Active connection
#[embassy_executor::task]
async fn ble_task(
    sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
) {
    let mut flash = nrf_softdevice::Flash::take(sd);

    // Reconnect timeout: 60 seconds
    const RECONNECT_TIMEOUT_MS: u64 = 60_000;
    // Sync mode timeout: 60 seconds
    const SYNC_TIMEOUT_MS: u64 = 60_000;

    loop {
        // Check for name toggle request (non-blocking)
        if NAME_TOGGLE.signaled() {
            let new_pref = NAME_TOGGLE.wait().await;
            rprintln!(
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
                        // Check for sync mode signal
                        let adv_future = advertise(sd, server, bonder, AdvertiseMode::Reconnect);
                        let sync_future = SYNC_MODE.wait();

                        match embassy_futures::select::select(adv_future, sync_future).await {
                            embassy_futures::select::Either::First(result) => {
                                match result {
                                    Ok(c) => break Some(c),
                                    Err(_) => {
                                        // Check timeout in Reconnecting state
                                        if get_connection_state() == ConnectionState::Reconnecting
                                            && start.elapsed().as_millis() >= RECONNECT_TIMEOUT_MS
                                        {
                                            rprintln!("BLE: Reconnect timeout, entering idle");
                                            set_connection_state(ConnectionState::Idle);
                                        }
                                        Timer::after(Duration::from_millis(500)).await;
                                        continue;
                                    }
                                }
                            }
                            embassy_futures::select::Either::Second(()) => {
                                // Sync mode triggered
                                rprintln!("BLE: Sync mode requested");
                                bonder.clear();
                                let _ = crate::ble::flash_bond::clear_bond(&mut flash).await;
                                set_connection_state(ConnectionState::SyncMode);
                                break None;
                            }
                        }
                    }
                } else {
                    // No bonded device - wait for sync mode
                    rprintln!("BLE: No bond, waiting for sync mode");
                    set_connection_state(ConnectionState::Idle);
                    SYNC_MODE.wait().await;
                    rprintln!("BLE: Sync mode requested (no prior bond)");
                    set_connection_state(ConnectionState::SyncMode);
                    None
                };

                if let Some(conn) = conn {
                    // Connected!
                    set_connection_state(ConnectionState::Connected);
                    handle_connection(sd, server, bonder, &mut flash, conn).await;
                    // After disconnect, go back to reconnecting
                    set_connection_state(ConnectionState::Reconnecting);
                }
            }

            ConnectionState::SyncMode => {
                // Sync mode: discoverable to all for 60 seconds
                let start = Instant::now();

                let conn = loop {
                    if start.elapsed().as_millis() >= SYNC_TIMEOUT_MS {
                        rprintln!("BLE: Sync mode timeout");
                        // Return to appropriate state
                        if bonder.has_bond() {
                            set_connection_state(ConnectionState::Reconnecting);
                        } else {
                            set_connection_state(ConnectionState::Idle);
                        }
                        break None;
                    }

                    let adv_future = advertise(sd, server, bonder, AdvertiseMode::SyncMode);

                    match embassy_time::with_timeout(Duration::from_secs(5), adv_future).await {
                        Ok(Ok(c)) => break Some(c),
                        Ok(Err(_)) | Err(_) => {
                            // Timeout or error, keep trying
                            continue;
                        }
                    }
                };

                if let Some(conn) = conn {
                    set_connection_state(ConnectionState::Connected);
                    handle_connection(sd, server, bonder, &mut flash, conn).await;
                    set_connection_state(ConnectionState::Reconnecting);
                }
            }

            ConnectionState::Connected => {
                // Shouldn't get here, but handle it
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Handle an active BLE connection.
async fn handle_connection(
    _sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
    flash: &mut nrf_softdevice::Flash,
    conn: nrf_softdevice::ble::Connection,
) {
    rprintln!("BLE: Connected!");

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
        unsafe {
            let _ = nrf_softdevice::raw::sd_ble_gap_conn_param_update(handle, &conn_params);
        }
    }

    // Run GATT server while connected
    let gatt_future = gatt_server::run(&conn, server, |_| {});

    // Notification sender - sends HID reports at fixed interval (like real controllers)
    let notify_future = async {
        // Wait for client to discover services and subscribe
        Timer::after(Duration::from_millis(5000)).await;

        let mut current_state = ControllerState::default();

        loop {
            // Check for new state (non-blocking via try semantics)
            // We use signaled() to check without blocking, then reset
            if CONTROLLER_STATE.signaled() {
                current_state = CONTROLLER_STATE.wait().await;
            }

            // Always send reports at fixed interval (8ms ≈ 125Hz)
            let report = current_state.to_gamepad_report();
            let report_bytes = report.to_bytes();
            let _ = server.hid.report_set(&report_bytes);
            let _ = server.send_report(&conn, &report);

            Timer::after(Duration::from_millis(8)).await;
        }
    };

    // Run both until one completes (connection drops)
    embassy_futures::select::select(gatt_future, notify_future).await;

    rprintln!("BLE: Disconnected");

    // Save system attributes and bond to flash
    bonder.save_sys_attrs(&conn);
    if let Some((master_id, enc_info, peer_id)) = bonder.get_bond_data() {
        let sys_attrs = bonder.get_sys_attrs();
        let _ =
            crate::ble::flash_bond::save_bond(flash, &master_id, &enc_info, &peer_id, &sys_attrs)
                .await;
    }

    Timer::after(Duration::from_millis(500)).await;
}

/// Sync button monitoring task.
///
/// - Hold 3 seconds: enter pairing/sync mode
/// - Triple-press within 2 seconds: toggle device name (Xbox <-> Dreamcast) and reset
///
/// LED1 behavior based on ConnectionState:
/// - Idle/Reconnecting: OFF
/// - SyncMode: Fast blink (200ms on/off)
/// - Connected: Solid ON
#[embassy_executor::task]
async fn sync_button_task(button: Input<'static>, mut led: Output<'static>) {
    const HOLD_DURATION_MS: u64 = 3000;
    const BLINK_INTERVAL_MS: u64 = 100;
    const TRIPLE_PRESS_WINDOW_MS: u64 = 2000;

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
                // Fast blink handled below when not checking button
                led.set_low();
                Timer::after(Duration::from_millis(200)).await;
                led.set_high();
                Timer::after(Duration::from_millis(200)).await;

                // Check for button press to cancel sync mode early
                if button.is_low() {
                    Timer::after(Duration::from_millis(100)).await;
                    if button.is_low() {
                        rprintln!("SYNC: Cancelled by button press");
                        while button.is_low() {
                            Timer::after(Duration::from_millis(50)).await;
                        }
                    }
                }
                continue; // Skip the button hold detection below
            }
            ConnectionState::Idle | ConnectionState::Reconnecting => {
                led.set_high(); // LED off
            }
        }

        // Check for button press (active low)
        if button.is_high() {
            // Reset triple-press counter if window expired
            if press_count > 0 && first_press_time.elapsed().as_millis() >= TRIPLE_PRESS_WINDOW_MS {
                press_count = 0;
            }
            Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        // Button pressed - start timing with LED feedback
        let press_start = Instant::now();
        let mut led_state = false;
        let mut last_blink = Instant::now();
        let mut held_long = false;

        // Wait for either release or hold duration
        while button.is_low() {
            // Blink LED while holding to indicate pending action
            if last_blink.elapsed().as_millis() >= BLINK_INTERVAL_MS {
                led_state = !led_state;
                if led_state {
                    led.set_low(); // LED on
                } else {
                    led.set_high(); // LED off
                }
                last_blink = Instant::now();
            }

            if press_start.elapsed().as_millis() >= HOLD_DURATION_MS {
                // Held long enough - trigger sync mode
                held_long = true;
                rprintln!("SYNC: Entering pairing mode (60s)");
                SYNC_MODE.signal(());
                press_count = 0; // Reset triple-press counter

                // Wait for button release
                while button.is_low() {
                    led.set_low();
                    Timer::after(Duration::from_millis(100)).await;
                    led.set_high();
                    Timer::after(Duration::from_millis(100)).await;
                }
                break;
            }
            Timer::after(Duration::from_millis(20)).await;
        }

        if !held_long {
            // Short press — count for triple-press detection
            if press_count == 0 {
                first_press_time = Instant::now();
            }
            press_count += 1;

            if press_count >= 3 && first_press_time.elapsed().as_millis() < TRIPLE_PRESS_WINDOW_MS {
                // Triple press detected! Toggle name preference.
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

                // Signal ble_task to save and reset
                NAME_TOGGLE.signal(new_pref);
                press_count = 0;
            }
        }

        // Debounce
        Timer::after(Duration::from_millis(100)).await;
    }
}

fn state_changed(prev: &ControllerState, curr: &ControllerState) -> bool {
    if prev.buttons.a != curr.buttons.a
        || prev.buttons.b != curr.buttons.b
        || prev.buttons.x != curr.buttons.x
        || prev.buttons.y != curr.buttons.y
        || prev.buttons.start != curr.buttons.start
        || prev.buttons.dpad_up != curr.buttons.dpad_up
        || prev.buttons.dpad_down != curr.buttons.dpad_down
        || prev.buttons.dpad_left != curr.buttons.dpad_left
        || prev.buttons.dpad_right != curr.buttons.dpad_right
    {
        return true;
    }

    if (prev.trigger_l as i16 - curr.trigger_l as i16).abs() > 10
        || (prev.trigger_r as i16 - curr.trigger_r as i16).abs() > 10
    {
        return true;
    }

    if (prev.stick_x as i16 - curr.stick_x as i16).abs() > 15
        || (prev.stick_y as i16 - curr.stick_y as i16).abs() > 15
    {
        return true;
    }

    false
}
