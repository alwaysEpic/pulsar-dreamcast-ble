#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Flex, Level, Output, OutputDrive};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use nrf_softdevice::ble::gatt_server;
use nrf_softdevice::ble::security::SecurityHandler;
use nrf_softdevice::Softdevice;
use panic_halt as _;
use rtt_target::{rprintln, rtt_init_print};
use static_cell::StaticCell;

mod ble;
mod board;
mod maple;

use crate::ble::{init_softdevice, Bonder, GamepadServer};
use crate::ble::hid::{GamepadServerEvent, HidServiceEvent};
use crate::maple::host::MapleResult;
use crate::maple::{ControllerState, MapleBus, MapleHost};

/// Shared controller state between maple and BLE tasks.
static CONTROLLER_STATE: Signal<CriticalSectionRawMutex, ControllerState> = Signal::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    rtt_init_print!();
    rprintln!("Dreamcast Controller Adapter Starting...");

    // Initialize Embassy with interrupt priorities that don't conflict with SoftDevice
    // SoftDevice uses priority 0-1, so we use P2 for our peripherals
    let mut config = embassy_nrf::config::Config::default();
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    let p = embassy_nrf::init(config);
    rprintln!("Embassy initialized");

    // Initialize SoftDevice
    rprintln!("Initializing SoftDevice...");
    let sd = init_softdevice();
    rprintln!("SoftDevice initialized");

    // Create HID Gamepad GATT server
    let server = match GamepadServer::new(sd) {
        Ok(s) => s,
        Err(e) => {
            rprintln!("Failed to create GATT server: {:?}", e);
            loop {
                cortex_m::asm::wfi();
            }
        }
    };
    static SERVER: StaticCell<GamepadServer> = StaticCell::new();
    let server = SERVER.init(server);

    // Initialize HID service values
    if let Err(e) = server.init() {
        rprintln!("Failed to init HID service: {:?}", e);
    }

    // Spawn the SoftDevice runner task
    match softdevice_task(sd) {
        Ok(token) => {
            spawner.spawn(token);
            rprintln!("SoftDevice task spawned");
        }
        Err(_) => rprintln!("Failed to create softdevice_task"),
    }

    // Create bonder for security/pairing
    static BONDER: StaticCell<Bonder> = StaticCell::new();
    let bonder = BONDER.init(Bonder::new());

    // Spawn BLE task
    match ble_task(sd, server, bonder) {
        Ok(token) => {
            spawner.spawn(token);
            rprintln!("BLE task spawned");
        }
        Err(_) => rprintln!("Failed to create ble_task"),
    }

    // LEDs (active low on DK)
    let mut led1 = Output::new(p.P0_13, Level::High, OutputDrive::Standard);
    let mut led2 = Output::new(p.P0_14, Level::High, OutputDrive::Standard);
    let mut led3 = Output::new(p.P0_15, Level::High, OutputDrive::Standard);
    let mut led4 = Output::new(p.P0_16, Level::High, OutputDrive::Standard);

    // Startup blink
    for _ in 0..3 {
        led1.set_low();
        Timer::after(Duration::from_millis(100)).await;
        led1.set_high();
        Timer::after(Duration::from_millis(100)).await;
    }

    // Set up Maple Bus using Flex pins
    let sdcka = Flex::new(p.P0_05);
    let sdckb = Flex::new(p.P0_06);
    let mut bus = MapleBus::new(sdcka, sdckb);
    let host = MapleHost::new();

    // Detect controller
    rprintln!("Detecting controller...");
    led2.set_low();

    let result = host.request_device_info(&mut bus);

    let controller_detected = match &result {
        MapleResult::Ok(info) => {
            rprintln!("Controller found! Functions: 0x{:08X}", info.functions);
            led2.set_high();
            led3.set_low();
            true
        }
        _ => {
            rprintln!("No controller detected");
            led2.set_high();
            led4.set_low();
            false
        }
    };

    if !controller_detected {
        rprintln!("Halting - no controller");
        loop {
            cortex_m::asm::wfi();
        }
    }

    // Controller polling loop
    rprintln!("");
    rprintln!("=== Polling Controller Input ===");
    rprintln!("Press buttons on the Dreamcast controller!");
    rprintln!("");

    let mut last_state: Option<ControllerState> = None;
    let mut poll_count: u32 = 0;

    loop {
        let result = host.get_condition(&mut bus);

        match result {
            MapleResult::Ok(state) => {
                // LED1 on when any button pressed
                if state.buttons.any_pressed() {
                    led1.set_low();
                } else {
                    led1.set_high();
                }

                // Only print and signal when state changes
                let changed = match &last_state {
                    None => true,
                    Some(prev) => state_changed(prev, &state),
                };

                if changed {
                    print_state(&state);
                    CONTROLLER_STATE.signal(state);
                    last_state = Some(state);
                }
            }
            MapleResult::Timeout => {
                if poll_count % 100 == 0 {
                    rprintln!("Poll timeout");
                }
            }
            MapleResult::CrcError => {
                rprintln!("CRC error");
            }
            MapleResult::UnexpectedResponse(cmd) => {
                rprintln!("Unexpected: 0x{:02X}", cmd);
            }
        }

        poll_count = poll_count.wrapping_add(1);
        Timer::after(Duration::from_millis(16)).await;
    }
}

/// SoftDevice runner task - must run continuously.
#[embassy_executor::task]
async fn softdevice_task(sd: &'static Softdevice) {
    sd.run().await;
}

/// BLE advertising and connection handling task.
#[embassy_executor::task]
async fn ble_task(sd: &'static Softdevice, server: &'static GamepadServer, bonder: &'static Bonder) {
    loop {
        // Advertise and wait for connection (with pairing support)
        let conn = match ble::softdevice::advertise(sd, server, bonder).await {
            Ok(c) => c,
            Err(e) => {
                rprintln!("BLE: Advertise error: {:?}", e);
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        };

        rprintln!("BLE: Client connected (HID Gamepad)");

        // Load sys_attrs for returning bonded devices (required for GATT to work on reconnect)
        bonder.load_sys_attrs(&conn);

        // Small delay to let connection stabilize
        Timer::after(Duration::from_millis(100)).await;

        // Request security/pairing from the central (required for HID over GATT)
        // This triggers "Just Works" pairing to establish an encrypted link
        rprintln!("BLE: Requesting security...");
        if let Err(e) = conn.request_security() {
            rprintln!("BLE: Security request failed: {:?}", e);
            // Continue anyway - the central might initiate on its own
        }

        // Run GATT server while connected, also sending notifications
        let gatt_future = gatt_server::run(&conn, server, |event| {
            match event {
                GamepadServerEvent::Hid(e) => {
                    match e {
                        HidServiceEvent::ProtocolModeWrite(val) => {
                            rprintln!("BLE: Protocol mode set to {}", val);
                        }
                        HidServiceEvent::ReportCccdWrite { notifications } => {
                            rprintln!("BLE: Client {} notifications",
                                if notifications { "enabled" } else { "disabled" });
                        }
                        HidServiceEvent::ControlPointWrite(val) => {
                            rprintln!("BLE: Control point: {}",
                                if val == 0 { "suspend" } else { "exit suspend" });
                        }
                    }
                }
                GamepadServerEvent::DeviceInfo(_) => {
                    rprintln!("BLE: DevInfo read");
                }
                GamepadServerEvent::Battery(_) => {
                    rprintln!("BLE: Battery read");
                }
            }
        });

        // Notification sender - polls CONTROLLER_STATE and sends HID reports
        let notify_future = async {
            // Wait for client to discover services and subscribe to notifications
            // macOS needs time for: pairing, service discovery, CCCD write
            rprintln!("BLE: Waiting for client to subscribe...");
            Timer::after(Duration::from_millis(5000)).await;
            rprintln!("BLE: Starting HID reports");

            let mut last_report: Option<[u8; 8]> = None;
            let mut success_count = 0u32;
            let mut error_count = 0u32;

            loop {
                let state = CONTROLLER_STATE.wait().await;
                let report = state.to_gamepad_report();
                let report_bytes = report.to_bytes();

                let should_notify = match &last_report {
                    None => true,
                    Some(prev) => prev != &report_bytes,
                };

                if should_notify {
                    // First just set the value (client can read it)
                    let _ = server.hid.report_set(&report_bytes);

                    // Then try to notify
                    match server.send_report(&conn, &report) {
                        Ok(_) => {
                            success_count += 1;
                            error_count = 0;
                            last_report = Some(report_bytes);
                            if success_count == 1 {
                                rprintln!("BLE: First HID report sent successfully!");
                            }
                        }
                        Err(e) => {
                            error_count += 1;
                            // Log first few errors with details, then less frequently
                            if error_count <= 3 {
                                rprintln!("BLE: Notify error {:?} (count={})", e, error_count);
                            } else if error_count % 100 == 0 {
                                rprintln!("BLE: Notify still failing (count={})", error_count);
                            }
                            // Still update last_report so we don't spam
                            last_report = Some(report_bytes);
                        }
                    }
                }

                // Small delay between reports to reduce radio contention
                Timer::after(Duration::from_millis(8)).await;
            }
        };

        // Run both until one completes (connection drops)
        embassy_futures::select::select(gatt_future, notify_future).await;

        // Save system attributes before disconnect (for reconnection)
        bonder.save_sys_attrs(&conn);

        rprintln!("BLE: Client disconnected");
        Timer::after(Duration::from_millis(500)).await;
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

fn print_state(state: &ControllerState) {
    let b = &state.buttons;

    let mut btns: heapless::String<32> = heapless::String::new();
    if b.a {
        let _ = btns.push_str("A ");
    }
    if b.b {
        let _ = btns.push_str("B ");
    }
    if b.x {
        let _ = btns.push_str("X ");
    }
    if b.y {
        let _ = btns.push_str("Y ");
    }
    if b.start {
        let _ = btns.push_str("ST ");
    }
    if b.dpad_up {
        let _ = btns.push_str("U ");
    }
    if b.dpad_down {
        let _ = btns.push_str("D ");
    }
    if b.dpad_left {
        let _ = btns.push_str("L ");
    }
    if b.dpad_right {
        let _ = btns.push_str("R ");
    }

    if btns.is_empty() {
        rprintln!(
            "Stick({},{}) Trig({},{})",
            state.stick_x,
            state.stick_y,
            state.trigger_l,
            state.trigger_r
        );
    } else {
        rprintln!(
            "[{}] Stick({},{}) Trig({},{})",
            btns.trim_end(),
            state.stick_x,
            state.stick_y,
            state.trigger_l,
            state.trigger_r
        );
    }
}
