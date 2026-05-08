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

use embassy_time::Instant;
use nrf_softdevice::Softdevice;
// Panic handler is registered via #[panic_handler] in pulsar_dreamcast_ble::panic_handler
#[cfg(feature = "board-xiao")]
use pulsar_dreamcast_ble::SLEEP_TIMEOUT_MS;
use pulsar_dreamcast_ble::{log, log_init};
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
    log_init!();
    pulsar_dreamcast_ble::panic_handler::check_panic_log();
    log!("DC Adapter Starting");

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

    // Load active profile from flash; defaults to STD on first boot.
    let profile_id = ble::flash_bond::load_profile();
    let profile = profile_id.profile();
    log!(
        "PROFILE: {} (PID {:#06x})",
        core::str::from_utf8(profile.vmu_label).unwrap_or("?"),
        profile.pid
    );

    // Initialize SoftDevice with chosen profile
    ble::softdevice::set_profile(profile);
    let sd = ble::softdevice::init_softdevice(profile);

    // Enable radio notifications for VMU write scheduling
    if pulsar_dreamcast_ble::maple::radio_notify::init() {
        log!("RADIO: Notifications enabled");
    } else {
        log!("RADIO: Notification setup failed");
    }

    // Create HID Gamepad GATT server
    let Ok(server) = ble::GamepadServer::new(sd) else {
        loop {
            cortex_m::asm::wfi();
        }
    };
    static SERVER: StaticCell<ble::GamepadServer> = StaticCell::new();
    let server = SERVER.init(server);
    let _ = server.init(profile);

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
        log!(
            "PWR: {}",
            if charging { "Charging" } else { "Not charging" }
        );
        charging
    };

    // Set up Maple Bus using Flex pins
    let mut bus = MapleBus::new(sdcka, sdckb);
    let host = MapleHost::new();

    #[cfg(feature = "board-xiao")]
    const BATTERY_READ_INTERVAL: Duration = Duration::from_secs(60);
    #[cfg(feature = "board-xiao")]
    let mut last_battery_read: Instant = Instant::now();

    // Initial battery read at startup
    #[cfg(feature = "board-xiao")]
    {
        let charging = charge_stat.is_low();
        let (mv, percent) = battery_reader.read(charging).await;
        BATTERY_LEVEL.signal(if charging { 0xFF } else { percent });
        if !charging && mv < LOW_BATTERY_CUTOFF_MV {
            log!("PWR: Low battery ({}mV), entering System Off", mv);
            unsafe {
                board::enter_system_off();
            }
        }
    }

    // Outer loop: wait for BLE connection, then poll controller
    loop {
        // --- Phase 1: Wait for BLE connection ---
        log!("MAIN: Waiting for BLE connection...");
        bus.set_low_power();
        status.off();
        // Wake-on-controller-input: poll the Maple bus periodically while
        // disconnected. If any face button / trigger / stick deflection is
        // detected, fire WAKE_REQUEST so the BLE task starts advertising and
        // the host can reconnect. Matches Xbox/PS controller behavior.
        //
        // Two intervals: base when a controller is responding, longer backoff
        // when the bus keeps timing out (no controller plugged in). With
        // button-mashing the worst-case latency is the base interval since
        // any one of the mash presses lands in a poll window.
        const WAKE_POLL_BASE_MS: u64 = 1500;
        const WAKE_POLL_BACKOFF_MS: u64 = 5000;
        const TIMEOUT_BACKOFF_THRESHOLD: u8 = 3;
        const TRIGGER_WAKE_THRESHOLD: u8 = 20;
        const STICK_WAKE_DEFLECTION: i16 = 30;
        let mut last_wake_poll = Instant::now();
        let mut consecutive_poll_failures: u8 = 0;
        loop {
            if get_connection_state() == ConnectionState::Connected {
                break;
            }

            // Goodbye splash from disconnected state. Wake the bus, try to
            // write BYE to the VMU (may silently fail if no controller is
            // plugged in), hold briefly so the user sees it, then sleep.
            // Runs on both boards so the flow is testable on the dev kit;
            // XIAO actually enters System Off, DK halts via WFI.
            if pulsar_dreamcast_ble::GOODBYE_PENDING.load(core::sync::atomic::Ordering::Relaxed) {
                log!("MAIN: Phase 1 goodbye, rendering BYE");
                bus.set_output_mode();
                Timer::after(Duration::from_millis(20)).await;
                let mut send_buf = pulsar_dreamcast_ble::vmu::build_message_splash(b"BYE");
                pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                log!("MAIN: VMU enumerate = {}", host.enumerate_vmu(&mut bus));
                let mut wrote = false;
                for _ in 0..5 {
                    if host.write_vmu_lcd(&mut bus, &send_buf) {
                        log!("MAIN: BYE write OK");
                        wrote = true;
                        break;
                    }
                    Timer::after(Duration::from_millis(50)).await;
                }
                if !wrote {
                    log!("MAIN: BYE write failed (no controller?)");
                }
                Timer::after(Duration::from_millis(1000)).await;
                #[cfg(feature = "board-xiao")]
                {
                    log!("MAIN: calling enter_system_off");
                    unsafe {
                        board::enter_system_off();
                    }
                }
                #[cfg(not(feature = "board-xiao"))]
                {
                    log!("MAIN: DK halting (no System Off on this board)");
                    loop {
                        cortex_m::asm::wfi();
                    }
                }
            }

            // Wake-on-controller-input poll. Briefly wake the bus, read state,
            // fire WAKE_REQUEST if anything's pressed, then back to low power.
            // Adaptive interval: backs off to 5s after repeated bus timeouts
            // (no controller plugged in) to save power.
            let poll_interval_ms = if consecutive_poll_failures >= TIMEOUT_BACKOFF_THRESHOLD {
                WAKE_POLL_BACKOFF_MS
            } else {
                WAKE_POLL_BASE_MS
            };
            if last_wake_poll.elapsed() >= Duration::from_millis(poll_interval_ms) {
                bus.set_output_mode();
                Timer::after(Duration::from_millis(2)).await;
                if let MapleResult::Ok(state) = host.get_condition(&mut bus) {
                    consecutive_poll_failures = 0;
                    let stick_dx = i16::from(state.stick_x) - 128;
                    let stick_dy = i16::from(state.stick_y) - 128;
                    let any_input = state.buttons.any_pressed()
                        || state.trigger_l > TRIGGER_WAKE_THRESHOLD
                        || state.trigger_r > TRIGGER_WAKE_THRESHOLD
                        || stick_dx.abs() > STICK_WAKE_DEFLECTION
                        || stick_dy.abs() > STICK_WAKE_DEFLECTION;
                    if any_input {
                        log!("MAIN: Controller input while disconnected, requesting wake");
                        pulsar_dreamcast_ble::WAKE_REQUEST.signal(());
                    }
                } else {
                    consecutive_poll_failures = consecutive_poll_failures.saturating_add(1);
                }
                bus.set_low_power();
                last_wake_poll = Instant::now();
            }

            #[cfg(feature = "board-xiao")]
            {
                // Battery/charge monitoring while waiting for BLE
                let charging = charge_stat.is_low();
                if charging != was_charging {
                    log!(
                        "CHG: {}",
                        if charging {
                            "Charging started"
                        } else {
                            "Charging stopped"
                        }
                    );
                    was_charging = charging;
                }

                if last_battery_read.elapsed() >= BATTERY_READ_INTERVAL {
                    let (mv, percent) = battery_reader.read(charging).await;
                    BATTERY_LEVEL.signal(if charging { 0xFF } else { percent });
                    last_battery_read = Instant::now();

                    if !charging && mv < LOW_BATTERY_CUTOFF_MV {
                        log!("PWR: Low battery ({}mV), entering System Off", mv);
                        unsafe {
                            board::enter_system_off();
                        }
                    }
                }
            }

            Timer::after(Duration::from_millis(BLE_WAIT_CHECK_MS)).await;
        }
        log!("MAIN: BLE connected, enabling controller");

        // --- Phase 2: Enable boost and detect controller ---
        // Skip boost if USB is providing 5V through Schottky diode passthrough
        #[cfg(feature = "board-xiao")]
        let mut usb_powered = board::is_usb_connected();
        #[cfg(feature = "board-xiao")]
        if usb_powered {
            log!("PWR: USB detected, boost off (passthrough)");
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
                log!(
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
                    log!("MAPLE: Controller detected");
                    break true;
                }
                MapleResult::Timeout => {
                    if !timeout_logged {
                        log!("MAPLE: Timeout (retrying...)");
                        bus.diagnose_bus();
                        timeout_logged = true;
                    }
                }
                MapleResult::UnexpectedResponse(_cmd) => {
                    log!("MAPLE: Unexpected cmd=0x{:02X}", _cmd);
                }
            }

            Timer::after(Duration::from_millis(retry_delay_ms)).await;
            retry_delay_ms = (retry_delay_ms * 2).min(MAX_RETRY_DELAY_MS);
        };

        if !controller_found {
            log!("MAIN: BLE disconnected during detection, disabling boost");
            #[cfg(feature = "board-xiao")]
            unsafe {
                board::disable_boost();
            }
            continue;
        }

        // --- Phase 3: Poll loop (active gaming) ---
        let mut vmu_delay: u16 = 180; // ~3s delay before VMU attempt
        let mut vmu_enumerated = false;
        let mut vmu_frame_dirty = true;
        let mut vmu_framebuf =
            pulsar_dreamcast_ble::vmu::build_profile_splash(profile.vmu_glyph, profile.vmu_label);
        let mut vmu_anim_step: u8 = 0;
        let mut vmu_anim_counter: u16 = 0;
        let mut vmu_idle_wait: u16 = 0;
        // ~16ms per poll, splash holds 30s before transitioning to the pulsar.
        let mut vmu_splash_polls: u16 = 30 * 60;
        const VMU_ANIM_INTERVAL: u16 = 10; // Advance animation every 10 polls (~160ms, ~6fps)
        const VMU_IDLE_FALLBACK: u16 = 30; // Write anyway after 30 polls (~480ms) without radio idle
        #[cfg_attr(not(feature = "board-xiao"), allow(unused_mut))]
        let mut vmu_battery_percent: u8 = 100;
        let mut last_state: Option<ControllerState> = None;
        let mut fail_count: u16 = 0;
        #[cfg(feature = "board-xiao")]
        let mut last_activity = Instant::now();

        // Goodbye state machine. Activated when GOODBYE_PENDING is set (button
        // task signals at the 7s hold mark, *during* the hold). We render BYE
        // through the existing dirty-flag path so it gets the same radio-idle
        // waiting and retry behavior as the regular pulsar/splash writes.
        // Once the write lands, we hold for ≥1s before triggering System Off
        // (XIAO) or halting via WFI (DK) so the user actually sees BYE.
        #[derive(Clone, Copy)]
        enum GoodbyeState {
            Render,        // need to swap framebuffer to BYE
            Wait,          // BYE in framebuffer, waiting for write to land
            Hold(Instant), // BYE on LCD, holding before sleep
        }
        let mut goodbye_state: Option<GoodbyeState> = None;

        loop {
            {
                let pending = pulsar_dreamcast_ble::GOODBYE_PENDING
                    .load(core::sync::atomic::Ordering::Relaxed);
                if pending && goodbye_state.is_none() {
                    log!("MAIN: Goodbye, rendering BYE");
                    goodbye_state = Some(GoodbyeState::Render);
                }
                match goodbye_state {
                    Some(GoodbyeState::Render) => {
                        vmu_framebuf = pulsar_dreamcast_ble::vmu::build_message_splash(b"BYE");
                        vmu_frame_dirty = true;
                        goodbye_state = Some(GoodbyeState::Wait);
                    }
                    Some(GoodbyeState::Wait) => {
                        if !vmu_frame_dirty {
                            // The standard write path cleared the dirty flag,
                            // which means BYE has actually landed on the LCD.
                            log!("MAIN: BYE landed, holding then System Off");
                            goodbye_state = Some(GoodbyeState::Hold(Instant::now()));
                        }
                    }
                    Some(GoodbyeState::Hold(start)) => {
                        if start.elapsed() >= Duration::from_millis(1000) {
                            #[cfg(feature = "board-xiao")]
                            {
                                log!("MAIN: calling enter_system_off");
                                unsafe {
                                    board::enter_system_off();
                                }
                            }
                            #[cfg(not(feature = "board-xiao"))]
                            {
                                log!("MAIN: DK halting (no System Off)");
                                loop {
                                    cortex_m::asm::wfi();
                                }
                            }
                        }
                    }
                    None => {}
                }
            }

            // Check for BLE disconnect
            let conn_state = get_connection_state();
            if conn_state != ConnectionState::Connected {
                // If the disconnect is because we just entered sync mode, write
                // a SYNC splash to the VMU so it persists through Phase 1.
                if conn_state == ConnectionState::SyncMode {
                    log!("MAIN: Sync mode entered, writing SYNC splash");
                    let mut send_buf = pulsar_dreamcast_ble::vmu::build_message_splash(b"SYNC");
                    pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                    if !vmu_enumerated {
                        let _ = host.enumerate_vmu(&mut bus);
                    }
                    let _ = host.write_vmu_lcd(&mut bus, &send_buf);
                }
                log!("MAIN: BLE disconnected, disabling boost");
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
                    log!("MAPLE: Controller reconnected");
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

                // VMU content: profile splash for the first 30s of every boot,
                // then the rotating pulsar (with battery overlay) for the rest.
                // Skipped entirely while goodbye is active so the BYE frame
                // doesn't get overwritten before it lands.
                let vmu_busy = goodbye_state.is_some();
                if vmu_busy {
                    // Goodbye in flight — leave vmu_framebuf alone.
                } else if vmu_delay > 0 {
                    vmu_delay -= 1;
                } else if vmu_splash_polls > 0 {
                    vmu_splash_polls -= 1;
                    if vmu_splash_polls == 0 {
                        // Transition out of splash: prime the animation so the
                        // first pulsar frame renders on the next interval.
                        vmu_anim_counter = VMU_ANIM_INTERVAL;
                    }
                } else {
                    vmu_anim_counter += 1;
                    if vmu_anim_counter >= VMU_ANIM_INTERVAL {
                        vmu_framebuf =
                            pulsar_dreamcast_ble::vmu::build_animated_frame(vmu_anim_step);
                        vmu_anim_step =
                            (vmu_anim_step + 1) % pulsar_dreamcast_ble::vmu::ROTATION_FRAMES;
                        vmu_anim_counter = 0;
                        vmu_frame_dirty = true;
                    }
                }

                // VMU write: send current framebuffer when radio is idle.
                // Battery overlay is composited here so every frame gets it
                // regardless of content source (animation, dongle, game icon).
                // Falls back to writing anyway if radio idle is never signaled.
                if vmu_frame_dirty {
                    let radio_ok = pulsar_dreamcast_ble::maple::radio_notify::is_radio_idle();
                    if radio_ok || vmu_idle_wait >= VMU_IDLE_FALLBACK {
                        if !vmu_enumerated {
                            let _ = host.enumerate_vmu(&mut bus);
                            vmu_enumerated = true;
                        }
                        let mut send_buf = vmu_framebuf;
                        pulsar_dreamcast_ble::vmu::composite_battery(
                            &mut send_buf,
                            vmu_battery_percent,
                            true,
                        );
                        pulsar_dreamcast_ble::vmu::rotate_180(&mut send_buf);
                        if host.write_vmu_lcd(&mut bus, &send_buf) {
                            vmu_frame_dirty = false;
                        }
                        vmu_idle_wait = 0;
                    } else {
                        vmu_idle_wait += 1;
                    }
                }
            } else {
                fail_count = fail_count.saturating_add(1);
                if fail_count == CONTROLLER_LOST_THRESHOLD {
                    log!("MAPLE: Controller lost, re-detecting...");
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
                            log!("MAPLE: Re-detect timeout, entering System Off");
                            unsafe {
                                board::enter_system_off();
                            }
                        }

                        let result = host.request_device_info(&mut bus);
                        if let MapleResult::Ok(_) = &result {
                            log!("MAPLE: Controller re-detected");
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
                        log!("MAIN: BLE disconnected during re-detect, disabling boost");
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
                        log!("PWR: USB connected, disabling boost (passthrough)");
                        unsafe {
                            board::disable_boost();
                        }
                    } else {
                        log!("PWR: USB removed, enabling boost");
                        unsafe {
                            board::enable_boost();
                        }
                    }
                }

                let charging = charge_stat.is_low();
                if charging != was_charging {
                    log!(
                        "CHG: {}",
                        if charging {
                            "Charging started"
                        } else {
                            "Charging stopped"
                        }
                    );
                    was_charging = charging;
                }

                if last_battery_read.elapsed() >= BATTERY_READ_INTERVAL {
                    let (mv, percent) = battery_reader.read(charging).await;
                    BATTERY_LEVEL.signal(if charging { 0xFF } else { percent });
                    vmu_battery_percent = if charging { 100 } else { percent };
                    last_battery_read = Instant::now();

                    if !charging && mv < LOW_BATTERY_CUTOFF_MV {
                        log!("PWR: Low battery ({}mV), entering System Off", mv);
                        unsafe {
                            board::enter_system_off();
                        }
                    }
                }
            }

            #[cfg(feature = "board-xiao")]
            if last_activity.elapsed().as_millis() >= INACTIVITY_TIMEOUT_MS {
                log!("MAIN: Inactivity timeout (10 min), entering System Off");
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
