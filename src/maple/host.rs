// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Maple Bus host controller.
//!
//! This module implements the host side of Maple Bus communication,
//! allowing the adapter to query Dreamcast controllers.

use crate::maple::gpio_bus::MapleBus;
use crate::maple::{ControllerState, MaplePacket};
use heapless::Vec;

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
}

/// Maple Bus function codes (device types).
pub mod functions {
    /// Standard controller.
    pub const CONTROLLER: u32 = maple_protocol::controller_state::CONTROLLER_FUNCTION;
}

/// Maple Bus addressing.
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
    /// Unexpected response command.
    UnexpectedResponse(#[allow(dead_code)] u8),
}

/// Device information returned by Device Info Request.
#[allow(dead_code)] // Fields populated but not yet consumed
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
pub struct MapleHost {
    /// Timeout for waiting for response (in busy-loop cycles).
    pub timeout_cycles: u32,
}

impl MapleHost {
    /// Create a new Maple Host with default timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            timeout_cycles: 64_000,
        }
    }

    /// Send a Device Info Request to discover what's connected.
    pub fn request_device_info(&self, bus: &mut MapleBus) -> MapleResult<DeviceInfo> {
        let packet = MaplePacket {
            sender: addressing::HOST,
            recipient: addressing::PORT_A_MAIN,
            command: commands::DEVICE_INFO_REQUEST,
            payload: Vec::new(),
        };

        bus.write_packet(&packet);

        // Read response using bulk sampling — no logging between TX and RX!
        // The controller responds within ~100µs; any rprintln here (~20ms via RTT)
        // causes us to miss the entire response.
        let response = bus.read_packet_bulk(self.timeout_cycles);

        let Some(pkt) = response else {
            return MapleResult::Timeout;
        };

        if pkt.command != commands::DEVICE_INFO_RESPONSE || pkt.payload.len() < 5 {
            return MapleResult::UnexpectedResponse(pkt.command);
        }

        #[allow(clippy::cast_possible_truncation)]
        let info = DeviceInfo {
            functions: pkt.payload[0],
            sub_functions: [pkt.payload[1], pkt.payload[2], pkt.payload[3]],
            region: (pkt.payload[4] >> 24) as u8,
            direction: (pkt.payload[4] >> 16) as u8,
            ..Default::default()
        };
        MapleResult::Ok(info)
    }

    /// Send a Get Condition request to read controller state.
    /// Retries up to 3 times on failure for resilience against BLE interference.
    pub fn get_condition(&self, bus: &mut MapleBus) -> MapleResult<ControllerState> {
        const MAX_RETRIES: u8 = 3;

        for _attempt in 0..MAX_RETRIES {
            let mut payload: Vec<u32, 32> = Vec::new();
            payload.push(functions::CONTROLLER).ok();

            let packet = MaplePacket {
                sender: addressing::HOST,
                recipient: addressing::PORT_A_MAIN,
                command: commands::GET_CONDITION,
                payload,
            };

            bus.write_packet(&packet);

            let response = bus.read_packet_bulk(self.timeout_cycles);

            let Some(pkt) = response else {
                // Retry on timeout/error
                continue;
            };

            if pkt.command != commands::CONDITION_RESPONSE {
                return MapleResult::UnexpectedResponse(pkt.command);
            }
            return match ControllerState::from_payload(&pkt.payload) {
                Some(state) => MapleResult::Ok(state),
                None => MapleResult::UnexpectedResponse(pkt.command),
            };
        }

        MapleResult::Timeout
    }

    /// Send a `DEVICE_INFO` request to the VMU sub-peripheral to enumerate it.
    ///
    /// The VMU will not accept `BLOCK_WRITE` until it has been enumerated.
    pub fn enumerate_vmu(&self, bus: &mut MapleBus) -> bool {
        let packet = MaplePacket {
            sender: addressing::HOST,
            recipient: 0x01, // SUB_PERIPHERAL_1
            command: commands::DEVICE_INFO_REQUEST,
            payload: Vec::new(),
        };

        bus.write_packet(&packet);

        let response = bus.read_packet_bulk(self.timeout_cycles);
        matches!(response, Some(pkt) if pkt.command == commands::DEVICE_INFO_RESPONSE)
    }

    /// Write a framebuffer to the VMU LCD in slot 1.
    ///
    /// Uses direct bit-bang TX (same as controller polling).
    /// May be corrupted by SoftDevice interrupts during the ~1.6ms TX,
    /// but avoids the BLE disruption caused by the timeslot API.
    pub fn write_vmu_lcd(&self, bus: &mut MapleBus, framebuffer: &[u8; 192]) -> bool {
        bus.write_lcd(
            addressing::HOST,
            0x01, // SUB_PERIPHERAL_1
            framebuffer,
        );

        let response = bus.read_packet_bulk(self.timeout_cycles);
        matches!(response, Some(pkt) if pkt.command == 0x07)
    }

    /// Write a framebuffer to the VMU LCD using the SoftDevice Radio Timeslot API.
    ///
    /// Guarantees interrupt-free TX but disrupts BLE connections.
    /// Kept for reference — use [`write_vmu_lcd`] for now.
    #[allow(dead_code)]
    pub fn write_vmu_lcd_timeslot(&self, bus: &mut MapleBus, framebuffer: &[u8; 192]) -> bool {
        use super::timeslot_tx;

        if !timeslot_tx::open_session() {
            return false;
        }

        if !timeslot_tx::request_lcd_tx(0x00, 0x01, framebuffer) {
            timeslot_tx::close_session();
            return false;
        }

        let mut timeout = 0u32;
        while !timeslot_tx::is_tx_complete() && !timeslot_tx::is_tx_failed() {
            cortex_m::asm::nop();
            timeout += 1;
            if timeout > 1_000_000 {
                timeslot_tx::close_session();
                return false;
            }
        }

        timeslot_tx::close_session();

        if timeslot_tx::is_tx_failed() {
            return false;
        }

        let response = bus.read_packet_bulk(self.timeout_cycles);
        matches!(response, Some(pkt) if pkt.command == 0x07)
    }
}

impl Default for MapleHost {
    fn default() -> Self {
        Self::new()
    }
}
