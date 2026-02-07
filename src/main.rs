#![no_std]
#![no_main]

use cortex_m::asm::nop;
use cortex_m_rt::entry;
use nb::block;

use panic_halt as _;

use nrf52840_dk_bsp::hal::{
    gpio::{Level, p0::Parts as P0Parts},
    pac,
    prelude::*,
    timer::{self, Timer},
};

use rtt_target::{rprintln, rtt_init_print};

// Maple Dependencies
mod maple;

use crate::maple::host::MapleResult;
use crate::maple::{MapleBusGpio, MapleHost};

#[entry]
fn main() -> ! {
    rtt_init_print!();
    rprintln!("Dreamcast Controller Adapter Starting...");

    // Take peripherals
    let periph = pac::Peripherals::take().unwrap();

    // Set up GPIO
    let p0 = P0Parts::new(periph.P0);

    // Set up timer
    let mut timer = Timer::new(periph.TIMER0);

    // Set up LEDs (accent active low on DK)
    let mut led1 = p0.p0_13.into_push_pull_output(Level::High).degrade();
    let mut led2 = p0.p0_14.into_push_pull_output(Level::High).degrade();
    let mut led3 = p0.p0_15.into_push_pull_output(Level::High).degrade();
    let mut led4 = p0.p0_16.into_push_pull_output(Level::High).degrade();

    // Blink LED1 to show we're starting
    for _ in 0..3 {
        let _ = led1.set_low(); // LED on
        delay(&mut timer, 100_000);
        let _ = led1.set_high(); // LED off
        delay(&mut timer, 100_000);
    }

    rprintln!("Setting up Maple Bus on P0.05 (SDCKA) and P0.06 (SDCKB)...");

    // First, read the lines as inputs to see natural state from controller
    let test_a = p0.p0_05.into_pullup_input();
    let test_b = p0.p0_06.into_pullup_input();
    delay(&mut timer, 10_000); // Let pull-ups settle
    let a = test_a.is_high().unwrap_or(false);
    let b = test_b.is_high().unwrap_or(false);
    rprintln!("Initial bus state (as inputs): A={} B={}", a as u8, b as u8);

    // Set up Maple Bus GPIO pins as PUSH-PULL output
    // SDCKA (Pin 1/Red) on P0.05 - idle HIGH
    // SDCKB (Pin 5/White) on P0.06 - idle LOW
    let sdcka = test_a.into_push_pull_output(Level::High).degrade();
    let sdckb = test_b.into_push_pull_output(Level::Low).degrade();

    let mut bus = MapleBusGpio::new(sdcka, sdckb);
    let host = MapleHost::new();

    // Debug: Toggle pins to verify wiring
    // If you have a multimeter/scope, check P0.05 and P0.06 are toggling
    rprintln!("Debug: Toggling Maple Bus pins 5 times...");
    for i in 0..5 {
        bus.set_idle(); // SDCKA high, SDCKB low
        delay(&mut timer, 500_000); // 500ms

        // Swap state
        bus.send_start_pattern(); // This will toggle the lines
        delay(&mut timer, 500_000); // 500ms
        rprintln!("  Toggle {}", i);
    }
    bus.set_idle();

    rprintln!("Maple Bus initialized. Attempting to detect controller...");
    let _ = led2.set_low(); // LED2 on = trying to communicate

    // Try to get device info
    let (mut bus, result) = host.request_device_info(bus);

    match &result {
        MapleResult::Ok(info) => {
            rprintln!("Controller detected!");
            rprintln!("  Functions: 0x{:08X}", info.functions);
            let _ = led2.set_high(); // LED2 off
            let _ = led3.set_low(); // LED3 on = success
        }
        MapleResult::Timeout => {
            rprintln!("No response from controller (timeout)");
            let _ = led2.set_high();
            let _ = led4.set_low(); // LED4 on = error
        }
        MapleResult::CrcError => {
            rprintln!("CRC error in response");
            let _ = led4.set_low();
        }
        MapleResult::UnexpectedResponse(cmd) => {
            rprintln!("Unexpected response: 0x{:02X}", cmd);
            let _ = led4.set_low();
        }
    }

    // Set up button for manual trigger
    let button1 = p0.p0_11.into_pullup_input();

    rprintln!("Entering main loop...");
    rprintln!("Press Button 1 on DK to send Device Info Request");

    let mut poll_count = 0u32;
    let mut last_button_state = false;

    loop {
        // Check if button 1 is pressed (active low)
        let button_pressed = button1.is_low().unwrap_or(false);

        // Detect button press (falling edge)
        if button_pressed && !last_button_state {
            rprintln!("Button pressed - sending Device Info Request...");
            let _ = led2.set_low(); // LED2 on

            let (new_bus, result) = host.request_device_info(bus);
            bus = new_bus;

            match &result {
                MapleResult::Ok(info) => {
                    rprintln!("SUCCESS! Controller detected!");
                    rprintln!("  Functions: 0x{:08X}", info.functions);
                    let _ = led2.set_high();
                    let _ = led3.set_low(); // LED3 on = success
                    let _ = led4.set_high();
                }
                MapleResult::Timeout => {
                    rprintln!("TIMEOUT - no response");
                    let _ = led2.set_high();
                    let _ = led3.set_high();
                    let _ = led4.set_low(); // LED4 on = error
                }
                MapleResult::UnexpectedResponse(cmd) => {
                    rprintln!("Got response but unexpected command: 0x{:02X}", cmd);
                    let _ = led2.set_high();
                    let _ = led4.set_low();
                }
                MapleResult::CrcError => {
                    rprintln!("CRC error in response");
                    let _ = led4.set_low();
                }
            }
        }
        last_button_state = button_pressed;

        poll_count = poll_count.wrapping_add(1);
        delay(&mut timer, 10_000); // 10ms debounce
        nop();
    }
}

fn delay<T>(timer: &mut Timer<T>, cycles: u32)
where
    T: timer::Instance,
{
    timer.start(cycles);
    let _ = block!(timer.wait());
}
