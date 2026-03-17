// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use pulsar_dreamcast_ble::ble::{get_connection_state, ConnectionState};
use pulsar_dreamcast_ble::maple::host::MapleResult;
use pulsar_dreamcast_ble::maple::{ControllerState, MapleBus, MapleHost};
use pulsar_dreamcast_ble::{ble, board, CONTROLLER_STATE};

#[cfg(feature = "board-xiao")]
use embassy_time::Instant;
use nrf_softdevice::Softdevice;
use panic_reset as _;
#[cfg(feature = "board-xiao")]
use pulsar_dreamcast_ble::SLEEP_TIMEOUT_MS;
use rtt_target::{rprintln, rtt_init_print};
use static_cell::StaticCell;

#[cfg(feature = "board-xiao")]
use pulsar_dreamcast_ble::BATTERY_LEVEL;

#[cfg(feature = "board-xiao")]
embassy_nrf::bind_interrupts!(struct SaadcIrqs {
    SAADC => embassy_nrf::saadc::InterruptHandler;
});

/// Maple Bus polling interval (~60Hz).
const POLL_INTERVAL_MS: u64 = 16;

/// Consecutive poll failures before declaring controller lost.
const CONTROLLER_LOST_THRESHOLD: u16 = 30;

/// Initial retry delay for controller detection (ms).
const INITIAL_RETRY_DELAY_MS: u64 = 100;

/// Maximum retry delay for controller detection (ms).
const MAX_RETRY_DELAY_MS: u64 = 1000;

/// How often to check BLE connection state while waiting (ms).
const BLE_WAIT_CHECK_MS: u64 = 100;

/// Timeout for initial controller detection (ms).
/// Enter System Off if no controller found within 60 seconds of BLE connecting.
#[cfg(feature = "board-xiao")]
const DETECT_TIMEOUT_MS: u64 = 60_000;

/// Timeout before entering sleep when controller is idle (ms).
/// 10 minutes with no input change triggers System Off.
#[cfg(feature = "board-xiao")]
const INACTIVITY_TIMEOUT_MS: u64 = 600_000;

/// Low battery cutoff voltage (mV). Enter System Off below this.
/// 3.2V gives ~5% margin above the 3.0V "empty" threshold.
#[cfg(feature = "board-xiao")]
const LOW_BATTERY_CUTOFF_MV: u32 = 3200;

#[allow(clippy::items_after_statements)] // StaticCell pattern requires inline statics
#[embassy_executor::main]
async fn main(spawner: Spawner) {
    rtt_init_print!();
    rprintln!("DC Adapter Starting");

    // Initialize Embassy with interrupt priorities that don't conflict with SoftDevice
    let mut config = embassy_nrf::config::Config::default();
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    #[cfg(feature = "board-xiao")]
    {
        config.dcdc.reg1 = true;
    }
    let p = embassy_nrf::init(config);

    // Disconnect all GPIO pins to clear any bootloader residue.
    // After reset the nRF52840 defaults pins to disconnected, but the UF2
    // bootloader may leave QSPI, NeoPixel, or LED pins configured.
    #[cfg(feature = "board-xiao")]
    unsafe {
        board::disconnect_all_pins();
    }

    // Put onboard QSPI flash into Deep Power Down (saves 2-5 mA)
    #[cfg(feature = "board-xiao")]
    unsafe {
        board::qspi_flash_deep_power_down();
    }

    // Load name preference from flash (Xbox vs Dreamcast)
    let is_dreamcast = ble::flash_bond::load_name_preference();
    if is_dreamcast {
        rprintln!("Name: Dreamcast Wireless Controller");
    } else {
        rprintln!("Name: Xbox Wireless Controller");
    }

    // Initialize SoftDevice with chosen name
    ble::softdevice::set_name_mode(is_dreamcast);
    let sd = ble::softdevice::init_softdevice(is_dreamcast);

    // Create HID Gamepad GATT server
    let Ok(server) = ble::GamepadServer::new(sd) else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    static SERVER: StaticCell<ble::GamepadServer> = StaticCell::new();
    let server = SERVER.init(server);
    let _ = server.init();

    // Spawn the SoftDevice runner task
    if let Ok(token) = softdevice_task(sd) {
        spawner.spawn(token);
    }

    // Create bonder for security/pairing
    static BONDER: StaticCell<ble::Bonder> = StaticCell::new();
    let bonder = BONDER.init(ble::Bonder::new());

    // Load bonding data from flash if available
    if let Some((master_id, enc_info, peer_id, sys_attrs)) = ble::flash_bond::load_bond() {
        bonder.load_from_flash(master_id, enc_info, peer_id, sys_attrs);
    }

    // Spawn BLE task
    if let Ok(token) = ble::task::ble_task(sd, server, bonder) {
        spawner.spawn(token);
    }

    // Initialize board-specific pins
    #[cfg(feature = "board-dk")]
    let board::BoardPins {
        sdcka,
        sdckb,
        sync_button,
        sync_led,
        mut status,
    } = board::init_pins(
        p.P0_05, p.P0_06, p.P0_13, p.P0_14, p.P0_15, p.P0_16, p.P0_25,
    );
    #[cfg(feature = "board-xiao")]
    let board::BoardPins {
        sdcka,
        sdckb,
        sync_button,
        sync_led,
        mut status,
        charge_stat,
    } = board::init_pins(
        p.P0_05, p.P0_03, p.P0_26, p.P0_30, p.P0_06, p.P1_15, p.P0_28, p.P0_13, p.P0_17,
    );

    #[cfg(feature = "board-xiao")]
    let mut battery_reader = board::BatteryReader::new(p.P0_14, p.P0_31, p.SAADC, SaadcIrqs);

    if let Ok(token) = pulsar_dreamcast_ble::button::sync_button_task(sync_button, sync_led) {
        spawner.spawn(token);
    }

    status.startup_blink().await;

    // Log initial charge status
    #[cfg(feature = "board-xiao")]
    let mut was_charging = {
        let charging = charge_stat.is_low();
        rprintln!(
            "PWR: {}",
            if charging { "Charging" } else { "Not charging" }
        );
        charging
    };

    // Set up Maple Bus using Flex pins
    let mut bus = MapleBus::new(sdcka, sdckb);
    let host = MapleHost::new();

    #[cfg(feature = "board-xiao")]
    const BATTERY_READ_INTERVAL_MS: u64 = 60_000;
    #[cfg(feature = "board-xiao")]
    let mut battery_read_countdown: u64 = BATTERY_READ_INTERVAL_MS;

    // Initial battery read at startup
    #[cfg(feature = "board-xiao")]
    {
        let (mv, percent) = battery_reader.read().await;
        let charging = charge_stat.is_low();
        BATTERY_LEVEL.signal(if charging { 0xFF } else { percent });
        if !charging && mv < LOW_BATTERY_CUTOFF_MV {
            rprintln!("PWR: Low battery ({}mV), entering System Off", mv);
            unsafe {
                board::enter_system_off();
            }
        }
    }

    // Outer loop: wait for BLE connection, then poll controller
    loop {
        // --- Phase 1: Wait for BLE connection ---
        rprintln!("MAIN: Waiting for BLE connection...");
        bus.set_low_power();
        status.off();
        loop {
            if get_connection_state() == ConnectionState::Connected {
                break;
            }

            #[cfg(feature = "board-xiao")]
            {
                // Battery/charge monitoring while waiting for BLE
                let charging = charge_stat.is_low();
                if charging != was_charging {
                    rprintln!(
                        "CHG: {}",
                        if charging {
                            "Charging started"
                        } else {
                            "Charging stopped"
                        }
                    );
                    was_charging = charging;
                }

                battery_read_countdown = battery_read_countdown.saturating_sub(BLE_WAIT_CHECK_MS);
                if battery_read_countdown == 0 {
                    let (mv, percent) = battery_reader.read().await;
                    BATTERY_LEVEL.signal(if charging { 0xFF } else { percent });
                    battery_read_countdown = BATTERY_READ_INTERVAL_MS;

                    if !charging && mv < LOW_BATTERY_CUTOFF_MV {
                        rprintln!("PWR: Low battery ({}mV), entering System Off", mv);
                        unsafe {
                            board::enter_system_off();
                        }
                    }
                }
            }

            Timer::after(Duration::from_millis(BLE_WAIT_CHECK_MS)).await;
        }
        rprintln!("MAIN: BLE connected, enabling controller");

        // --- Phase 2: Enable boost and detect controller ---
        // Skip boost if USB is providing 5V through Schottky diode passthrough
        #[cfg(feature = "board-xiao")]
        let mut usb_powered = board::is_usb_connected();
        #[cfg(feature = "board-xiao")]
        if usb_powered {
            rprintln!("PWR: USB detected, boost off (passthrough)");
        } else {
            unsafe {
                board::enable_boost();
            }
        }
        #[cfg(not(feature = "board-xiao"))]
        {
            // DK has no boost — nothing to do
        }
        // Brief delay for power source startup
        Timer::after(Duration::from_millis(50)).await;

        status.show_searching();
        let mut retry_delay_ms: u64 = INITIAL_RETRY_DELAY_MS;
        let mut timeout_logged = false;
        #[cfg(feature = "board-xiao")]
        let detect_start = Instant::now();
        let controller_found = loop {
            // Abort detection if BLE disconnects
            if get_connection_state() != ConnectionState::Connected {
                break false;
            }

            // Enter System Off if no controller found within timeout
            #[cfg(feature = "board-xiao")]
            if detect_start.elapsed().as_millis() >= DETECT_TIMEOUT_MS {
                rprintln!(
                    "MAPLE: Detect timeout ({}s), entering System Off",
                    DETECT_TIMEOUT_MS / 1000
                );
                unsafe {
                    board::enter_system_off();
                }
            }

            status.tx_activity_on();
            let result = host.request_device_info(&mut bus);
            status.tx_activity_off();

            match &result {
                MapleResult::Ok(_) => {
                    status.show_controller_found();
                    rprintln!("MAPLE: Controller detected");
                    break true;
                }
                MapleResult::Timeout => {
                    if !timeout_logged {
                        rprintln!("MAPLE: Timeout (retrying...)");
                        bus.diagnose_bus();
                        timeout_logged = true;
                    }
                }
                MapleResult::UnexpectedResponse(cmd) => {
                    rprintln!("MAPLE: Unexpected cmd=0x{:02X}", cmd);
                }
            }

            Timer::after(Duration::from_millis(retry_delay_ms)).await;
            retry_delay_ms = (retry_delay_ms * 2).min(MAX_RETRY_DELAY_MS);
        };

        if !controller_found {
            rprintln!("MAIN: BLE disconnected during detection, disabling boost");
            #[cfg(feature = "board-xiao")]
            unsafe {
                board::disable_boost();
            }
            continue;
        }

        // --- Phase 3: Poll loop (active gaming) ---
        let mut last_state: Option<ControllerState> = None;
        let mut fail_count: u16 = 0;
        #[cfg(feature = "board-xiao")]
        let mut last_activity = Instant::now();

        loop {
            // Check for BLE disconnect
            if get_connection_state() != ConnectionState::Connected {
                rprintln!("MAIN: BLE disconnected, disabling boost");
                #[cfg(feature = "board-xiao")]
                unsafe {
                    board::disable_boost();
                }
                status.off();
                CONTROLLER_STATE.signal(ControllerState::default());
                break;
            }

            if let MapleResult::Ok(state) = host.get_condition(&mut bus) {
                if fail_count >= CONTROLLER_LOST_THRESHOLD {
                    rprintln!("MAPLE: Controller reconnected");
                }
                fail_count = 0;

                let changed = match &last_state {
                    None => true,
                    Some(prev) => prev.state_changed(&state),
                };

                // Only signal on change — avoids overwriting a real button
                // press with identical idle-state data from the next poll.
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

                    let mut retry_delay_ms: u64 = INITIAL_RETRY_DELAY_MS;
                    #[cfg(feature = "board-xiao")]
                    let redetect_start = Instant::now();
                    loop {
                        // Abort re-detection if BLE disconnects
                        if get_connection_state() != ConnectionState::Connected {
                            break;
                        }

                        #[cfg(feature = "board-xiao")]
                        if redetect_start.elapsed().as_millis() >= SLEEP_TIMEOUT_MS {
                            rprintln!("MAPLE: Re-detect timeout, entering System Off");
                            unsafe {
                                board::enter_system_off();
                            }
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

                    // If BLE disconnected during re-detection, break to outer loop
                    if get_connection_state() != ConnectionState::Connected {
                        rprintln!("MAIN: BLE disconnected during re-detect, disabling boost");
                        #[cfg(feature = "board-xiao")]
                        unsafe {
                            board::disable_boost();
                        }
                        status.off();
                        CONTROLLER_STATE.signal(ControllerState::default());
                        break;
                    }
                }
            }

            #[cfg(feature = "board-xiao")]
            {
                // Monitor USB state changes — toggle boost accordingly
                let usb_now = board::is_usb_connected();
                if usb_now != usb_powered {
                    usb_powered = usb_now;
                    if usb_now {
                        rprintln!("PWR: USB connected, disabling boost (passthrough)");
                        unsafe {
                            board::disable_boost();
                        }
                    } else {
                        rprintln!("PWR: USB removed, enabling boost");
                        unsafe {
                            board::enable_boost();
                        }
                    }
                }

                let charging = charge_stat.is_low();
                if charging != was_charging {
                    rprintln!(
                        "CHG: {}",
                        if charging {
                            "Charging started"
                        } else {
                            "Charging stopped"
                        }
                    );
                    was_charging = charging;
                }

                battery_read_countdown = battery_read_countdown.saturating_sub(POLL_INTERVAL_MS);
                if battery_read_countdown == 0 {
                    let (mv, percent) = battery_reader.read().await;
                    BATTERY_LEVEL.signal(if charging { 0xFF } else { percent });
                    battery_read_countdown = BATTERY_READ_INTERVAL_MS;

                    if !charging && mv < LOW_BATTERY_CUTOFF_MV {
                        rprintln!("PWR: Low battery ({}mV), entering System Off", mv);
                        unsafe {
                            board::enter_system_off();
                        }
                    }
                }
            }

            #[cfg(feature = "board-xiao")]
            if last_activity.elapsed().as_millis() >= INACTIVITY_TIMEOUT_MS {
                rprintln!("MAIN: Inactivity timeout (10 min), entering System Off");
                unsafe {
                    board::enter_system_off();
                }
            }

            Timer::after(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }
}

/// `SoftDevice` runner task - must run continuously.
#[embassy_executor::task]
async fn softdevice_task(sd: &'static Softdevice) {
    sd.run().await;
}
