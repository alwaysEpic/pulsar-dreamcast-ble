#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Input, Output};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use nrf_softdevice::ble::gatt_server;
use nrf_softdevice::ble::security::SecurityHandler;
use nrf_softdevice::Softdevice;
use panic_reset as _;
use rtt_target::{rprintln, rtt_init_print};
use static_cell::StaticCell;

mod ble;
mod board;
mod maple;

#[cfg(feature = "board-xiao")]
embassy_nrf::bind_interrupts!(struct SaadcIrqs {
    SAADC => embassy_nrf::saadc::InterruptHandler;
});

use crate::ble::{
    advertise, get_connection_state, init_softdevice, set_connection_state, set_name_mode,
    AdvertiseMode, Bonder, ConnectionState, GamepadServer,
};
use crate::maple::host::MapleResult;
use crate::maple::{ControllerState, MapleBus, MapleHost};

/// Maple Bus polling interval (~60Hz).
const POLL_INTERVAL_MS: u64 = 16;

/// BLE HID notification interval (~125Hz, matches Xbox One S).
const NOTIFY_INTERVAL_MS: u64 = 8;

/// Consecutive poll failures before declaring controller lost.
const CONTROLLER_LOST_THRESHOLD: u16 = 30;

/// Initial retry delay for controller detection (ms).
const INITIAL_RETRY_DELAY_MS: u64 = 100;

/// Maximum retry delay for controller detection (ms).
const MAX_RETRY_DELAY_MS: u64 = 1000;

/// Delay for BLE client to discover services and subscribe (ms).
const SERVICE_DISCOVERY_DELAY_MS: u64 = 5000;

/// Max consecutive BLE notify failures before disconnecting.
const MAX_NOTIFY_FAILURES: u8 = 10;

/// Timeout before entering sleep when disconnected (ms).
const SLEEP_TIMEOUT_MS: u64 = 60_000;

/// Timeout before entering sleep when controller is idle (ms).
/// 10 minutes with no input change triggers System Off.
#[cfg(feature = "board-xiao")]
const INACTIVITY_TIMEOUT_MS: u64 = 600_000;

/// Shared controller state between maple and BLE tasks.
static CONTROLLER_STATE: Signal<CriticalSectionRawMutex, ControllerState> = Signal::new();

/// Signal to trigger sync/pairing mode (clears bonds).
static SYNC_MODE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Signal to toggle device name and reset. Carries new `is_dreamcast` value.
static NAME_TOGGLE: Signal<CriticalSectionRawMutex, bool> = Signal::new();

/// Signal to trigger System Off sleep (XIAO only).
#[cfg(feature = "board-xiao")]
static SLEEP_REQUEST: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Battery level percentage (0-100), updated periodically on XIAO.
#[cfg(feature = "board-xiao")]
static BATTERY_LEVEL: Signal<CriticalSectionRawMutex, u8> = Signal::new();

#[allow(clippy::items_after_statements)] // StaticCell pattern requires inline statics
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
    let Ok(server) = GamepadServer::new(sd) else {
        loop {
            cortex_m::asm::wfi();
        }
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

    // Initialize board-specific pins
    #[cfg(feature = "board-dk")]
    let (sdcka, sdckb, sync_button, sync_led, mut status) = board::init_pins(
        p.P0_05, p.P0_06, p.P0_13, p.P0_14, p.P0_15, p.P0_16, p.P0_25,
    );
    #[cfg(feature = "board-xiao")]
    let (sdcka, sdckb, sync_button, sync_led, mut status) = board::init_pins(
        p.P0_05, p.P0_03, p.P0_26, p.P0_30, p.P0_06, p.P1_12, p.P0_28, p.P0_13,
    );

    #[cfg(feature = "board-xiao")]
    let mut battery_reader = board::BatteryReader::new(p.P0_14, p.P0_31, p.SAADC, SaadcIrqs);

    if let Ok(token) = sync_button_task(sync_button, sync_led) {
        spawner.spawn(token);
    }

    status.startup_blink().await;

    // Set up Maple Bus using Flex pins
    let mut bus = MapleBus::new(sdcka, sdckb);
    let host = MapleHost::new();

    // Check initial bus state (should be A=1, B=0 with external pull-ups)
    let (a, b) = bus.read_pins();
    rprintln!("BUS: Initial state A={} B={}", a as u8, b as u8);

    // Detect controller (retry with backoff until found)
    status.show_searching();
    let mut retry_delay_ms: u64 = INITIAL_RETRY_DELAY_MS;
    loop {
        status.tx_activity_on();
        let result = host.request_device_info(&mut bus);
        status.tx_activity_off();

        match &result {
            MapleResult::Ok(_) => {
                status.show_controller_found();
                rprintln!("MAPLE: Controller detected");
                break;
            }
            MapleResult::Timeout => rprintln!("MAPLE: Timeout"),
            MapleResult::UnexpectedResponse(cmd) => {
                rprintln!("MAPLE: Unexpected cmd=0x{:02X}", cmd);
            }
        }

        // Diagnostic: check bus state after failed attempt (not in hot path)
        bus.diagnose_bus();

        Timer::after(Duration::from_millis(retry_delay_ms)).await;
        // Back off up to max delay between retries
        retry_delay_ms = (retry_delay_ms * 2).min(MAX_RETRY_DELAY_MS);
    }

    let mut last_state: Option<ControllerState> = None;
    let mut fail_count: u16 = 0;
    #[cfg(feature = "board-xiao")]
    let mut last_activity = Instant::now();
    #[cfg(feature = "board-xiao")]
    const BATTERY_READ_INTERVAL_MS: u64 = 60_000;
    #[cfg(feature = "board-xiao")]
    let mut battery_read_countdown: u64 = 0; // Force immediate first read

    loop {
        // Check for sleep request (XIAO only)
        #[cfg(feature = "board-xiao")]
        if SLEEP_REQUEST.signaled() {
            SLEEP_REQUEST.wait().await;
            rprintln!("MAIN: Sleep requested, entering System Off");
            // SAFETY: SoftDevice is initialized. Sync button pin already
            // configured as input — enter_system_off adds SENSE for wake.
            unsafe {
                board::enter_system_off();
            }
        }

        if let MapleResult::Ok(state) = host.get_condition(&mut bus) {
            if fail_count >= CONTROLLER_LOST_THRESHOLD {
                rprintln!("MAPLE: Controller reconnected");
            }
            fail_count = 0;

            // Only signal when state changes
            let changed = match &last_state {
                None => true,
                Some(prev) => prev.state_changed(&state),
            };

            if changed {
                CONTROLLER_STATE.signal(state);
                last_state = Some(state);
                #[cfg(feature = "board-xiao")]
                {
                    last_activity = Instant::now();
                }
            }
        } else {
            fail_count = fail_count.saturating_add(1);
            if fail_count == CONTROLLER_LOST_THRESHOLD {
                rprintln!("MAPLE: Controller lost, re-detecting...");
                CONTROLLER_STATE.signal(ControllerState::default());
                last_state = None;
                status.show_searching();

                // Re-detect controller before resuming polling
                let mut retry_delay_ms: u64 = INITIAL_RETRY_DELAY_MS;
                let redetect_start = Instant::now();
                loop {
                    // Sleep after 60s of failed re-detection (XIAO only)
                    #[cfg(feature = "board-xiao")]
                    if redetect_start.elapsed().as_millis() >= SLEEP_TIMEOUT_MS {
                        rprintln!("MAPLE: Re-detect timeout, entering sleep");
                        SLEEP_REQUEST.signal(());
                        Timer::after(Duration::from_secs(5)).await;
                    }

                    let result = host.request_device_info(&mut bus);
                    if let MapleResult::Ok(_) = &result {
                        rprintln!("MAPLE: Controller re-detected");
                        status.show_controller_found();
                        fail_count = 0;
                        #[cfg(feature = "board-xiao")]
                        {
                            last_activity = Instant::now();
                        }
                        break;
                    }
                    Timer::after(Duration::from_millis(retry_delay_ms)).await;
                    retry_delay_ms = (retry_delay_ms * 2).min(MAX_RETRY_DELAY_MS);
                }
            }
        }

        #[cfg(feature = "board-xiao")]
        {
            battery_read_countdown = battery_read_countdown.saturating_sub(POLL_INTERVAL_MS);
            if battery_read_countdown == 0 {
                let percent = battery_reader.read_percent().await;
                BATTERY_LEVEL.signal(percent);
                battery_read_countdown = BATTERY_READ_INTERVAL_MS;
            }
        }

        #[cfg(feature = "board-xiao")]
        if last_activity.elapsed().as_millis() >= INACTIVITY_TIMEOUT_MS {
            rprintln!("MAIN: Inactivity timeout (10 min), entering sleep");
            SLEEP_REQUEST.signal(());
            Timer::after(Duration::from_secs(5)).await;
        }

        Timer::after(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

/// `SoftDevice` runner task - must run continuously.
#[embassy_executor::task]
async fn softdevice_task(sd: &'static Softdevice) {
    sd.run().await;
}

/// BLE advertising and connection handling task.
///
/// State machine:
/// - `Reconnecting` (60s): Try to connect to bonded device only
/// - `Idle`: Continue trying bonded device (not discoverable)
/// - `SyncMode` (60s): Discoverable to all, accepts new pairings
/// - `Connected`: Active connection
#[allow(clippy::items_after_statements, clippy::too_many_lines)]
#[embassy_executor::task]
async fn ble_task(
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
                                    && start.elapsed().as_millis() >= SLEEP_TIMEOUT_MS
                                {
                                    #[cfg(feature = "board-xiao")]
                                    {
                                        rprintln!("BLE: Reconnect timeout, entering sleep");
                                        SLEEP_REQUEST.signal(());
                                        // Wait for sleep to take effect (main task handles it)
                                        Timer::after(Duration::from_secs(5)).await;
                                    }
                                    #[cfg(not(feature = "board-xiao"))]
                                    {
                                        rprintln!("BLE: Reconnect timeout, entering idle");
                                        set_connection_state(ConnectionState::Idle);
                                    }
                                }
                                Timer::after(Duration::from_millis(500)).await;
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
                    // No bonded device - go straight to sync mode
                    rprintln!("BLE: No bond, auto-entering sync mode");
                    set_connection_state(ConnectionState::SyncMode);
                    None
                };

                if let Some(conn) = conn {
                    set_connection_state(ConnectionState::Connected);
                    handle_connection(sd, server, bonder, &mut flash, conn).await;
                    if bonder.has_bond() {
                        set_connection_state(ConnectionState::Reconnecting);
                    } else {
                        set_connection_state(ConnectionState::Idle);
                    }
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
                            // No bond and sync timed out — sleep to save power.
                            // Wake via sync button → full reset → auto sync mode.
                            #[cfg(feature = "board-xiao")]
                            {
                                rprintln!("BLE: No bond after sync timeout, entering sleep");
                                SLEEP_REQUEST.signal(());
                                Timer::after(Duration::from_secs(5)).await;
                            }
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
                    handle_connection(sd, server, bonder, &mut flash, conn).await;
                    if bonder.has_bond() {
                        set_connection_state(ConnectionState::Reconnecting);
                    } else {
                        set_connection_state(ConnectionState::Idle);
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
        // SAFETY: Connection handle is valid (checked above). conn_params is
        // a well-formed struct on the stack, passed as a const pointer.
        let rc = unsafe {
            nrf_softdevice::raw::sd_ble_gap_conn_param_update(
                handle,
                (&raw const conn_params).cast_mut(),
            )
        };
        if rc != 0 {
            rprintln!("BLE: Conn param update failed: {}", rc);
        }
    }

    // Run GATT server while connected
    let gatt_future = gatt_server::run(&conn, server, |_| {});

    // Notification sender - sends HID reports at fixed interval (like real controllers)
    let notify_future = async {
        // Wait for client to discover services and subscribe
        Timer::after(Duration::from_millis(SERVICE_DISCOVERY_DELAY_MS)).await;

        let mut current_state = ControllerState::default();
        let mut notify_fails: u8 = 0;

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
            if server.send_report(&conn, &report).is_err() {
                notify_fails += 1;
                if notify_fails > MAX_NOTIFY_FAILURES {
                    rprintln!("BLE: Too many notify failures, disconnecting");
                    break;
                }
            } else {
                notify_fails = 0;
            }

            Timer::after(Duration::from_millis(NOTIFY_INTERVAL_MS)).await;
        }
    };

    // Update battery level in BLE service when signaled (XIAO only)
    #[cfg(feature = "board-xiao")]
    let battery_future = async {
        loop {
            let level = BATTERY_LEVEL.wait().await;
            let _ = server.battery.battery_level_set(&level);
            let _ = server.battery.battery_level_notify(&conn, &level);
        }
    };

    // Run all until one completes (connection drops)
    #[cfg(feature = "board-xiao")]
    let result = embassy_futures::select::select3(gatt_future, notify_future, battery_future).await;
    #[cfg(not(feature = "board-xiao"))]
    let result = embassy_futures::select::select(gatt_future, notify_future).await;

    #[cfg(feature = "board-xiao")]
    match result {
        embassy_futures::select::Either3::First(gatt_result) => {
            rprintln!("BLE: Disconnected (GATT: {:?})", gatt_result);
        }
        embassy_futures::select::Either3::Second(()) => {
            rprintln!("BLE: Disconnected (notify failure)");
        }
        embassy_futures::select::Either3::Third(()) => {
            rprintln!("BLE: Disconnected (battery task ended)");
        }
    }
    #[cfg(not(feature = "board-xiao"))]
    match result {
        embassy_futures::select::Either::First(gatt_result) => {
            rprintln!("BLE: Disconnected (GATT: {:?})", gatt_result);
        }
        embassy_futures::select::Either::Second(()) => {
            rprintln!("BLE: Disconnected (notify failure)");
        }
    }

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
/// LED1 behavior based on `ConnectionState`:
/// - `Idle`/`Reconnecting`: OFF
/// - `SyncMode`: Fast blink (200ms on/off)
/// - `Connected`: Solid ON
#[allow(clippy::items_after_statements)]
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
