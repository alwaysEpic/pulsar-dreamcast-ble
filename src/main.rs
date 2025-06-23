#![no_std]
#![no_main]

mod maple;
use defmt_rtt as _;
use heapless::Vec;
use maple::MaplePacket;
use panic_probe as _;

use cortex_m_rt::entry;
// use nb::block;

// use panic_halt as _;

// use nrf52840_dk_bsp::{
//     Board,
//     hal::{
//         prelude::*,
//         timer::{self, Timer},
//     },
// };
// use rtt_target::{rprintln, rtt_init_print};

const MAX_DEVICES: usize = 1;

const MAPLE_HOST_ADDRESSES: u8 = 0x00;

#[entry]
fn main() -> ! {
    defmt::info!("Starting mock Maple bus cycle..");

    let packet = MaplePacket {
        sender: MapleDevice::Controller,
        recipient: MapleDevice::Console,
        command: MapleCommand::DeviceInfo,
        payload: Vec::from_slice(&[0xAABBCCDD]).unwrap(),
    };

    // defmt::info!("Built test packet: {:?}", packet);

    let mut bus = MockMapleBus::new();

    let now_us = 1000;
    let status = bus.process_events(now_us);

    defmt::info!("Bus status after processing: {:?}", status);

    loop {
        cortex_m::asm::wfi();
    }
}
