//! Maple Bus host controller.
//!
//! This module implements the host side of Maple Bus communication,
//! allowing the adapter to query Dreamcast controllers.

#![allow(dead_code)] // Contains get_condition (upcoming feature)

use crate::maple::gpio_bus::MapleBusGpioOut;
use crate::maple::{ControllerState, MaplePacket};
use heapless::Vec;
use rtt_target::rprintln;

/// Maple Bus command codes.
pub mod commands {
    /// Request device info (identity).
    pub const DEVICE_INFO_REQUEST: u8 = 0x01;
    /// Device info response.
    pub const DEVICE_INFO_RESPONSE: u8 = 0x05;
    /// Get condition (read controller state).
    pub const GET_CONDITION: u8 = 0x09;
    /// Condition response.
    pub const CONDITION_RESPONSE: u8 = 0x08;
    /// No response / error.
    pub const NO_RESPONSE: u8 = 0xFF;
}

/// Maple Bus function codes (device types).
pub mod functions {
    /// Standard controller.
    pub const CONTROLLER: u32 = 0x00000001;
    /// Memory card (VMU).
    pub const MEMORY_CARD: u32 = 0x00000002;
    /// LCD display (VMU screen).
    pub const LCD: u32 = 0x00000004;
    /// Timer (VMU clock).
    pub const TIMER: u32 = 0x00000008;
    /// Vibration (rumble pack).
    pub const VIBRATION: u32 = 0x00000100;
}

/// Maple Bus addressing.
/// Port 0, Unit 0 = main controller in port A.
pub mod addressing {
    /// Host address (the adapter).
    pub const HOST: u8 = 0x00;
    /// Controller in port A, main unit.
    pub const PORT_A_MAIN: u8 = 0x20;
}

/// Result of a Maple Bus transaction.
#[derive(Debug)]
pub enum MapleResult<T> {
    /// Successful response with data.
    Ok(T),
    /// No response (timeout).
    Timeout,
    /// CRC error in response.
    CrcError,
    /// Unexpected response command.
    UnexpectedResponse(u8),
}

/// Device information returned by Device Info Request.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Function type bitmap.
    pub functions: u32,
    /// Sub-function data (3 words).
    pub sub_functions: [u32; 3],
    /// Region code.
    pub region: u8,
    /// Connection direction.
    pub direction: u8,
    /// Product name (up to 30 chars).
    pub product_name: [u8; 30],
    /// License string (up to 60 chars).
    pub license: [u8; 60],
    /// Standby power consumption (mW).
    pub standby_power: u16,
    /// Max power consumption (mW).
    pub max_power: u16,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            functions: 0,
            sub_functions: [0; 3],
            region: 0,
            direction: 0,
            product_name: [0; 30],
            license: [0; 60],
            standby_power: 0,
            max_power: 0,
        }
    }
}

/// Maple Bus host controller.
///
/// Manages communication with Dreamcast peripherals.
pub struct MapleHost {
    /// Timeout for waiting for response (in busy-loop cycles).
    pub timeout_cycles: u32,
}

impl MapleHost {
    /// Create a new Maple Host with default timeout.
    pub fn new() -> Self {
        Self {
            // ~1ms timeout at 64MHz
            timeout_cycles: 64_000,
        }
    }

    /// Create a new Maple Host with custom timeout.
    pub fn with_timeout(timeout_cycles: u32) -> Self {
        Self { timeout_cycles }
    }

    /// Send a Device Info Request to discover what's connected.
    ///
    /// Takes ownership of the bus and returns it along with the result.
    /// GPIO pins change type when switching modes, hence the ownership transfer.
    pub fn request_device_info(
        &self,
        mut bus: MapleBusGpioOut,
    ) -> (MapleBusGpioOut, MapleResult<DeviceInfo>) {
        let packet = MaplePacket {
            sender: addressing::HOST,
            recipient: addressing::PORT_A_MAIN,
            command: commands::DEVICE_INFO_REQUEST,
            payload: Vec::new(),
        };

        rprintln!("TX: DeviceInfoRequest");
        bus.write_packet(&packet);

        // Switch to input mode - controller will respond after ~60µs
        let mut bus_in = bus.into_input();

        // Use bulk sampling for more reliable reading
        let response = bus_in.read_packet_bulk(self.timeout_cycles);

        // Switch back to output mode
        let bus_out = bus_in.into_output();

        // Parse response
        let result = match response {
            None => MapleResult::Timeout,
            Some(pkt) => {
                if pkt.command != commands::DEVICE_INFO_RESPONSE || pkt.payload.len() < 5 {
                    MapleResult::UnexpectedResponse(pkt.command)
                } else {
                    let info = DeviceInfo {
                        functions: pkt.payload[0],
                        sub_functions: [pkt.payload[1], pkt.payload[2], pkt.payload[3]],
                        region: (pkt.payload[4] >> 24) as u8,
                        direction: (pkt.payload[4] >> 16) as u8,
                        ..Default::default()
                    };
                    MapleResult::Ok(info)
                }
            }
        };

        (bus_out, result)
    }

    /// Send a Get Condition request to read controller state.
    ///
    /// Takes ownership of the bus and returns it along with the result.
    pub fn get_condition(
        &self,
        mut bus: MapleBusGpioOut,
    ) -> (MapleBusGpioOut, MapleResult<ControllerState>) {
        let mut payload: Vec<u32, 255> = Vec::new();
        payload.push(functions::CONTROLLER).ok();

        let packet = MaplePacket {
            sender: addressing::HOST,
            recipient: addressing::PORT_A_MAIN,
            command: commands::GET_CONDITION,
            payload,
        };

        rprintln!("TX: GetCondition");
        bus.write_packet(&packet);

        // Switch to input mode and read response using bulk sampling
        let mut bus_in = bus.into_input();
        let response = bus_in.read_packet_bulk(self.timeout_cycles);

        // Switch back to output mode
        let bus_out = bus_in.into_output();

        // Parse response
        let result = match response {
            None => MapleResult::Timeout,
            Some(pkt) => {
                if pkt.command != commands::CONDITION_RESPONSE {
                    MapleResult::UnexpectedResponse(pkt.command)
                } else {
                    match ControllerState::from_payload(&pkt.payload) {
                        Some(state) => MapleResult::Ok(state),
                        None => MapleResult::UnexpectedResponse(pkt.command),
                    }
                }
            }
        };

        (bus_out, result)
    }
}

impl Default for MapleHost {
    fn default() -> Self {
        Self::new()
    }
}
