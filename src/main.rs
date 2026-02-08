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
use crate::maple::host::MapleResult;
use crate::maple::{ControllerState, MapleBus, MapleHost};

/// Shared controller state between maple and BLE tasks.
static CONTROLLER_STATE: Signal<CriticalSectionRawMutex, ControllerState> = Signal::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    rtt_init_print!();
    rprintln!("DC Adapter Starting");

    // Initialize Embassy with interrupt priorities that don't conflict with SoftDevice
    let mut config = embassy_nrf::config::Config::default();
    config.gpiote_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    let p = embassy_nrf::init(config);

    // Initialize SoftDevice
    let sd = init_softdevice();

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
        loop {
            cortex_m::asm::wfi();
        }
    }

    let mut last_state: Option<ControllerState> = None;

    loop {
        if let MapleResult::Ok(state) = host.get_condition(&mut bus) {
            // LED1 on when any button pressed
            if state.buttons.any_pressed() {
                led1.set_low();
            } else {
                led1.set_high();
            }

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
#[embassy_executor::task]
async fn ble_task(
    sd: &'static Softdevice,
    server: &'static GamepadServer,
    bonder: &'static Bonder,
) {
    let mut flash = nrf_softdevice::Flash::take(sd);

    loop {
        let conn = match ble::softdevice::advertise(sd, server, bonder).await {
            Ok(c) => c,
            Err(_) => {
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        };

        bonder.load_sys_attrs(&conn);
        Timer::after(Duration::from_millis(100)).await;
        let _ = conn.request_security();

        // Run GATT server while connected
        let gatt_future = gatt_server::run(&conn, server, |_| {});

        // Notification sender - polls CONTROLLER_STATE and sends HID reports
        let notify_future = async {
            // Wait for client to discover services and subscribe
            Timer::after(Duration::from_millis(5000)).await;

            let mut last_report: Option<[u8; 8]> = None;

            loop {
                let state = CONTROLLER_STATE.wait().await;
                let report = state.to_gamepad_report();
                let report_bytes = report.to_bytes();

                let should_notify = match &last_report {
                    None => true,
                    Some(prev) => prev != &report_bytes,
                };

                if should_notify {
                    let _ = server.hid.report_set(&report_bytes);
                    let _ = server.send_report(&conn, &report);
                    last_report = Some(report_bytes);
                }

                Timer::after(Duration::from_millis(8)).await;
            }
        };

        // Run both until one completes (connection drops)
        embassy_futures::select::select(gatt_future, notify_future).await;

        // Save system attributes and bond to flash
        bonder.save_sys_attrs(&conn);
        if let Some((master_id, enc_info, peer_id)) = bonder.get_bond_data() {
            let sys_attrs = bonder.get_sys_attrs();
            let _ = crate::ble::flash_bond::save_bond(
                &mut flash, &master_id, &enc_info, &peer_id, &sys_attrs,
            )
            .await;
        }

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
