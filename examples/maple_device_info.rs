//! Maple Bus Device Info Request Example
//!
//! Demonstrates reading from a Dreamcast controller using bulk sampling.
//! This example sends a Device Info Request and reads the controller's response.
//!
//! # Hardware Setup
//! - nRF52840 DK
//! - P0.05 = SDCKA (Red wire)
//! - P0.06 = SDCKB (White wire)
//! - 4.7kΩ pull-ups from both data lines to 3.3V
//! - Controller powered by 5V
//! - Controller GND/Sense pin connected to ground
//!
//! # Protocol Summary
//! Maple Bus is a 2-wire protocol at 2Mbps where lines alternate as clock/data:
//! - Phase 1: A falls -> sample B
//! - Phase 2: B falls -> sample A
//! - Idle state: A=HIGH, B=LOW
//!
//! # Key Implementation Details
//! - Uses bulk sampling (capture all GPIO samples, decode later) for reliability
//! - Static 96KB buffer avoids stack allocation delay
//! - Combined wait+sample function avoids function-call delay
//! - Phase enforcement: skip B edges until first A fall
//! - find_first_edges() uses 40 edges for late-start detection (not 10!)

#![no_std]
#![no_main]

use cortex_m_rt::entry;
use panic_halt as _;

use nb::block;
use nrf52840_dk_bsp::hal::{
    gpio::{Level, p0::Parts as P0Parts},
    pac,
    prelude::*,
    timer::{self, Timer},
};
use rtt_target::{rprintln, rtt_init_print};

// Import from parent crate (when built as example)
use embedded_rust_setup::maple::host::MapleResult;
use embedded_rust_setup::maple::{MapleBusGpio, MapleHost};

#[entry]
fn main() -> ! {
    rtt_init_print!();
    rprintln!("=== Maple Bus Device Info Example ===");
    rprintln!("");

    let periph = pac::Peripherals::take().unwrap();
    let p0 = P0Parts::new(periph.P0);
    let mut timer = Timer::new(periph.TIMER0);

    // Step 1: Read initial bus state to verify wiring
    rprintln!("Step 1: Checking initial bus state...");
    let test_a = p0.p0_05.into_pullup_input();
    let test_b = p0.p0_06.into_pullup_input();
    delay(&mut timer, 10_000);

    let a = test_a.is_high().unwrap_or(false);
    let b = test_b.is_high().unwrap_or(false);
    rprintln!("  Initial state: A={} B={}", a as u8, b as u8);

    // Verify correct idle state
    if a && !b {
        rprintln!("  OK - Correct idle state (A=1, B=0)");
    } else if !a && b {
        rprintln!("  ERROR - Wires are swapped! A should be HIGH, B should be LOW");
        rprintln!("  Fix: Swap the wires on P0.05 and P0.06");
        loop {} // Halt
    } else if !a && !b {
        rprintln!("  WARNING - Both lines LOW. Check controller power.");
    } else {
        rprintln!("  WARNING - Unexpected state. Check pull-ups.");
    }
    rprintln!("");

    // Step 2: Initialize Maple Bus
    rprintln!("Step 2: Initializing Maple Bus...");
    let sdcka = test_a.into_push_pull_output(Level::High).degrade();
    let sdckb = test_b.into_push_pull_output(Level::Low).degrade();
    let bus = MapleBusGpio::new(sdcka, sdckb);
    let host = MapleHost::new();
    rprintln!("  Maple Bus ready on P0.05 (SDCKA) and P0.06 (SDCKB)");
    rprintln!("");

    // Step 3: Send Device Info Request
    rprintln!("Step 3: Sending Device Info Request...");
    rprintln!("  This uses bulk sampling to capture the response:");
    rprintln!("  - 96KB static buffer (pre-allocated, no delay)");
    rprintln!("  - Combined wait+sample (no function-call delay)");
    rprintln!("  - Phase-aligned decoding (first edge must be A)");
    rprintln!("");

    let (bus, result) = host.request_device_info(bus);

    // Step 4: Display result
    rprintln!("Step 4: Result");
    match &result {
        MapleResult::Ok(info) => {
            rprintln!("  SUCCESS!");
            rprintln!("  Frame: cmd=0x05 (Device Info Response)");
            rprintln!("  Functions: 0x{:08X}", info.functions);
            if info.functions & 0x00000001 != 0 {
                rprintln!("    - Standard Controller");
            }
            if info.functions & 0x00000002 != 0 {
                rprintln!("    - Memory Card (VMU)");
            }
            if info.functions & 0x00000100 != 0 {
                rprintln!("    - Vibration (Rumble)");
            }
        }
        MapleResult::Timeout => {
            rprintln!("  TIMEOUT - No response from controller");
            rprintln!("  Check:");
            rprintln!("    - Controller is powered (5V)");
            rprintln!("    - GND/Sense pin is grounded");
            rprintln!("    - Pull-ups are installed (4.7k to 3.3V)");
        }
        MapleResult::CrcError => {
            rprintln!("  CRC ERROR - Data corruption");
            rprintln!("  Check signal integrity with oscilloscope");
        }
        MapleResult::UnexpectedResponse(cmd) => {
            rprintln!("  UNEXPECTED RESPONSE: cmd=0x{:02X}", cmd);
        }
    }

    rprintln!("");
    rprintln!("=== Example Complete ===");

    // Keep bus alive for inspection
    let _bus = bus;
    loop {
        cortex_m::asm::wfi();
    }
}

fn delay<T>(timer: &mut Timer<T>, cycles: u32)
where
    T: timer::Instance,
{
    timer.start(cycles);
    let _ = block!(timer.wait());
}
