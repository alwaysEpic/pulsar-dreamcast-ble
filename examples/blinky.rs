#![no_std]
#![no_main]

use cortex_m_rt::entry;
use nrf52840_dk_bsp::hal::{
    gpio::{Level, p0::Parts as P0Parts},
    pac,
    prelude::*,
};
use panic_halt as _;

#[entry]
fn main() -> ! {
    let periph = pac::Peripherals::take().unwrap();
    let p0 = P0Parts::new(periph.P0);

    // All 4 LEDs (active low)
    let mut led1 = p0.p0_13.into_push_pull_output(Level::High);
    let mut led2 = p0.p0_14.into_push_pull_output(Level::High);
    let mut led3 = p0.p0_15.into_push_pull_output(Level::High);
    let mut led4 = p0.p0_16.into_push_pull_output(Level::High);

    loop {
        // Cycle through all LEDs
        led1.set_low().ok();
        cortex_m::asm::delay(2_000_000);
        led1.set_high().ok();

        led2.set_low().ok();
        cortex_m::asm::delay(2_000_000);
        led2.set_high().ok();

        led3.set_low().ok();
        cortex_m::asm::delay(2_000_000);
        led3.set_high().ok();

        led4.set_low().ok();
        cortex_m::asm::delay(2_000_000);
        led4.set_high().ok();
    }
}
