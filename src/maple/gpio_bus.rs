//! GPIO-based Maple Bus implementation for nRF52840.
//!
//! This module implements the Maple Bus wire protocol using bit-banging.
//!
//! # Protocol Summary
//! - Two wires (SDCKA, SDCKB) alternate as clock/data
//! - Phase 1: SDCKA = clock, SDCKB = data
//! - Phase 2: SDCKB = clock, SDCKA = data
//! - 500ns per phase = 2Mbps
//! - Idle state: SDCKA HIGH, SDCKB LOW

#![allow(dead_code)] // Some methods for real-time edge detection (alternative to bulk sampling)

use crate::maple::MaplePacket;
use core::sync::atomic::{compiler_fence, Ordering};
use embassy_nrf::gpio::{Flex, Pull};
use heapless::Vec;
use rtt_target::rprintln;

/// Static buffer for bulk sampling (96KB). Pre-allocated to avoid runtime delay.
static mut SAMPLE_BUFFER: [u32; 24576] = [0; 24576];

const PIN_A_MASK: u32 = 1 << 5; // SDCKA on P0.05
const PIN_B_MASK: u32 = 1 << 6; // SDCKB on P0.06

/// P0 GPIO base address for direct register access.
const P0_BASE: u32 = 0x5000_0000;
/// Offset to IN register within GPIO peripheral.
const GPIO_IN_OFFSET: u32 = 0x510;

/// Read P0 IN register directly.
#[inline(always)]
fn read_p0_in() -> u32 {
    unsafe { core::ptr::read_volatile((P0_BASE + GPIO_IN_OFFSET) as *const u32) }
}

/// ~500ns delay at 64MHz
#[inline(always)]
fn delay_half_bit() {
    for _ in 0..32 {
        cortex_m::asm::nop();
    }
    compiler_fence(Ordering::SeqCst);
}

/// GPIO-based Maple Bus driver.
///
/// Uses Embassy Flex pins for dynamic input/output switching.
pub struct MapleBus {
    sdcka: Flex<'static>,
    sdckb: Flex<'static>,
}

impl MapleBus {
    /// Create a new Maple Bus GPIO driver.
    ///
    /// Initializes pins in idle state (SDCKA high, SDCKB low).
    pub fn new(mut sdcka: Flex<'static>, mut sdckb: Flex<'static>) -> Self {
        // Start in output mode with idle state
        sdcka.set_as_output(embassy_nrf::gpio::OutputDrive::Standard);
        sdckb.set_as_output(embassy_nrf::gpio::OutputDrive::Standard);
        sdcka.set_high();
        sdckb.set_low();

        // Small delay for pins to stabilize
        for _ in 0..100 {
            cortex_m::asm::nop();
        }

        Self { sdcka, sdckb }
    }

    /// Configure pins as outputs (push-pull).
    pub fn set_output_mode(&mut self) {
        self.sdcka.set_as_output(embassy_nrf::gpio::OutputDrive::Standard);
        self.sdckb.set_as_output(embassy_nrf::gpio::OutputDrive::Standard);
    }

    /// Configure pins as inputs with pull-up.
    pub fn set_input_mode(&mut self) {
        self.sdcka.set_as_input(Pull::Up);
        self.sdckb.set_as_input(Pull::Up);
        // Allow pull-ups to stabilize
        for _ in 0..200 {
            cortex_m::asm::nop();
        }
    }

    /// Set bus to idle state (SDCKA high, SDCKB low).
    #[inline(always)]
    pub fn set_idle(&mut self) {
        self.sdcka.set_high();
        self.sdckb.set_low();
    }

    /// Send the start/sync pattern.
    pub fn send_start_pattern(&mut self) {
        // SDCKA LOW
        self.sdcka.set_low();

        // Toggle SDCKB 4 times
        for _ in 0..4 {
            self.sdckb.set_high();
            delay_half_bit();
            self.sdckb.set_low();
            delay_half_bit();
        }

        // SDCKB HIGH
        self.sdckb.set_high();
        delay_half_bit();
        // SDCKA HIGH
        self.sdcka.set_high();
        delay_half_bit();
        // SDCKB LOW (final state)
        self.sdckb.set_low();
        delay_half_bit();
    }

    /// Send the end pattern.
    pub fn send_end_pattern(&mut self) {
        self.sdcka.set_high();
        self.sdckb.set_high();
        delay_half_bit();

        self.sdckb.set_low();
        delay_half_bit();

        self.sdcka.set_low();
        delay_half_bit();

        self.sdcka.set_high();
        delay_half_bit();

        self.sdcka.set_low();
        delay_half_bit();

        self.sdcka.set_high();
        delay_half_bit();

        self.sdckb.set_high();
        delay_half_bit();
    }

    /// Write a single bit using the alternating clock/data scheme.
    #[inline(always)]
    pub fn write_bit(&mut self, bit: bool, phase: &mut bool) {
        if *phase {
            // Phase true: SDCKA = clock, SDCKB = data
            if bit {
                self.sdckb.set_high();
            } else {
                self.sdckb.set_low();
            }
            delay_half_bit();
            self.sdcka.set_low();
            delay_half_bit();
            self.sdckb.set_high();
        } else {
            // Phase false: SDCKB = clock, SDCKA = data
            if bit {
                self.sdcka.set_high();
            } else {
                self.sdcka.set_low();
            }
            delay_half_bit();
            self.sdckb.set_low();
            delay_half_bit();
            self.sdcka.set_high();
        }
        *phase = !*phase;
    }

    /// Write a byte, MSB first.
    #[inline(always)]
    pub fn write_byte(&mut self, byte: u8, phase: &mut bool) {
        for i in (0..8).rev() {
            let bit = (byte >> i) & 1 == 1;
            self.write_bit(bit, phase);
        }
    }

    /// Write a 32-bit word in Maple Bus byte order (LSB first).
    pub fn write_word(&mut self, word: u32, phase: &mut bool) {
        self.write_byte(word as u8, phase);
        self.write_byte((word >> 8) as u8, phase);
        self.write_byte((word >> 16) as u8, phase);
        self.write_byte((word >> 24) as u8, phase);
    }

    /// Write a complete packet.
    pub fn write_packet(&mut self, packet: &MaplePacket) {
        self.set_output_mode();
        self.set_idle();
        delay_half_bit(); // Stabilize before start pattern
        let mut phase = true;

        self.send_start_pattern();

        let frame = packet.frame_word();
        let mut crc: u8 = 0;

        self.write_word(frame, &mut phase);
        Self::update_crc(frame, &mut crc);

        for &word in packet.payload.iter() {
            self.write_word(word, &mut phase);
            Self::update_crc(word, &mut crc);
        }

        self.write_byte(crc, &mut phase);
        self.send_end_pattern();
    }

    /// Update CRC with a word (bytewise XOR).
    fn update_crc(word: u32, crc: &mut u8) {
        *crc ^= (word & 0xFF) as u8;
        *crc ^= ((word >> 8) & 0xFF) as u8;
        *crc ^= ((word >> 16) & 0xFF) as u8;
        *crc ^= ((word >> 24) & 0xFF) as u8;
    }

    /// Wait for start pattern and bulk sample.
    /// Returns (success, wait_cycles, b_transitions, sample_count).
    pub fn wait_and_sample(&mut self, timeout_cycles: u32) -> (bool, u32, u32, usize) {
        self.set_input_mode();

        let samples = unsafe {
            core::ptr::addr_of_mut!(SAMPLE_BUFFER)
                .as_mut()
                .unwrap_unchecked()
        };

        // Wait for idle (both HIGH)
        let mut count = 0u32;
        loop {
            let val = read_p0_in();
            if (val & PIN_A_MASK) != 0 && (val & PIN_B_MASK) != 0 {
                break;
            }
            count += 1;
            if count > timeout_cycles / 2 {
                return (false, count, 0, 0);
            }
        }
        let idle_cycles = count;

        // Wait for A LOW (controller starts response)
        count = 0;
        loop {
            let val = read_p0_in();
            if (val & PIN_A_MASK) == 0 {
                break;
            }
            count += 1;
            if count > timeout_cycles {
                return (false, idle_cycles + count, 0, 0);
            }
        }
        let wait_cycles = idle_cycles + count;

        // Count B transitions while A is LOW
        let mut b_transitions = 0u32;
        let mut last_b = (read_p0_in() & PIN_B_MASK) != 0;
        count = 0;

        loop {
            let val = read_p0_in();
            let a = (val & PIN_A_MASK) != 0;
            let b = (val & PIN_B_MASK) != 0;

            if b != last_b {
                b_transitions += 1;
                last_b = b;
            }

            if a && b_transitions >= 3 {
                // Valid start pattern - sample immediately
                compiler_fence(Ordering::SeqCst);
                for sample in samples.iter_mut() {
                    *sample = read_p0_in();
                }
                compiler_fence(Ordering::SeqCst);
                return (true, wait_cycles, b_transitions, 24576);
            }

            count += 1;
            if count > 10000 {
                return (false, wait_cycles, b_transitions, 0);
            }
        }
    }

    /// Decode bits from bulk samples.
    pub fn decode_bulk_samples(
        &self,
        samples: &[u32],
        count: usize,
        start_sample: usize,
    ) -> (Vec<u8, 1024>, u32, u32, u32) {
        const GAP_THRESHOLD: usize = 50;

        let mut bits: Vec<u8, 1024> = Vec::new();
        let mut a_falls: u32 = 0;
        let mut b_falls: u32 = 0;
        let mut gaps_detected: u32 = 0;

        let start_idx = if start_sample > 0 && start_sample < count {
            start_sample
        } else {
            1
        };
        let init_idx = if start_idx > 0 { start_idx - 1 } else { 0 };
        let mut last_a = (samples[init_idx] & PIN_A_MASK) != 0;
        let mut last_b = (samples[init_idx] & PIN_B_MASK) != 0;
        let mut idle_count: usize = 0;
        let mut seen_first_a_fall = false;

        for &sample in &samples[start_idx..count] {
            let a = (sample & PIN_A_MASK) != 0;
            let b = (sample & PIN_B_MASK) != 0;

            // Gap detection: idle = A HIGH, B LOW
            if a && !b {
                idle_count += 1;
            } else {
                if idle_count > GAP_THRESHOLD {
                    gaps_detected += 1;
                    seen_first_a_fall = false;
                }
                idle_count = 0;
            }

            // A falls -> sample B (Phase 1)
            if last_a && !a {
                seen_first_a_fall = true;
                let _ = bits.push(b as u8);
                a_falls += 1;
            }
            // B falls -> sample A (Phase 2), but only after first A fall
            else if last_b && !b {
                if seen_first_a_fall {
                    let _ = bits.push(a as u8);
                }
                b_falls += 1;
            }

            last_a = a;
            last_b = b;
        }

        (bits, a_falls, b_falls, gaps_detected)
    }

    /// Find first N edges and their sample indices.
    pub fn find_first_edges(
        &self,
        samples: &[u32],
        count: usize,
        max_edges: usize,
    ) -> Vec<(usize, char, bool), 64> {
        let mut edges: Vec<(usize, char, bool), 64> = Vec::new();
        let mut last_a = (samples[0] & PIN_A_MASK) != 0;
        let mut last_b = (samples[0] & PIN_B_MASK) != 0;

        for (i, &sample) in samples[1..count].iter().enumerate() {
            if edges.len() >= max_edges {
                break;
            }
            let a = (sample & PIN_A_MASK) != 0;
            let b = (sample & PIN_B_MASK) != 0;

            if last_a && !a {
                let _ = edges.push((i + 1, 'A', b));
            }
            if last_b && !b {
                let _ = edges.push((i + 1, 'B', a));
            }

            last_a = a;
            last_b = b;
        }

        edges
    }

    /// Read a complete response packet using bulk sampling.
    pub fn read_packet_bulk(&mut self, timeout_cycles: u32) -> Option<MaplePacket> {
        let (success, _wait_cycles, b_trans, count) = self.wait_and_sample(timeout_cycles);

        if !success {
            rprintln!("RX: No start pattern (b_trans={})", b_trans);
            return None;
        }

        let samples = unsafe {
            core::ptr::addr_of!(SAMPLE_BUFFER)
                .as_ref()
                .unwrap_unchecked()
        };

        let edges = self.find_first_edges(samples, count, 40);
        let first_edge_idx = edges.first().map(|(idx, _, _)| *idx).unwrap_or(0);
        let skip_samples = if first_edge_idx > 100 {
            first_edge_idx.saturating_sub(10)
        } else {
            0
        };

        let (bits, _a_falls, _b_falls, _gaps) =
            self.decode_bulk_samples(samples, count, skip_samples);

        if bits.len() < 32 {
            rprintln!("RX: Not enough bits ({})", bits.len());
            return None;
        }

        // Convert bits to bytes (MSB first per byte)
        let mut bytes: [u8; 256] = [0; 256];
        let byte_count = bits.len() / 8;
        for byte_idx in 0..byte_count {
            let mut byte_val: u8 = 0;
            for bit_idx in 0..8 {
                byte_val = (byte_val << 1) | bits[byte_idx * 8 + bit_idx];
            }
            bytes[byte_idx] = byte_val;
        }

        if byte_count < 4 {
            rprintln!("RX: Not enough bytes for frame");
            return None;
        }

        // Parse frame word (LSB byte first)
        let frame = (bytes[0] as u32)
            | ((bytes[1] as u32) << 8)
            | ((bytes[2] as u32) << 16)
            | ((bytes[3] as u32) << 24);

        let command = ((frame >> 24) & 0xFF) as u8;
        let recipient = ((frame >> 16) & 0xFF) as u8;
        let sender = ((frame >> 8) & 0xFF) as u8;
        let length = (frame & 0xFF) as usize;

        let mut crc: u8 = bytes[0] ^ bytes[1] ^ bytes[2] ^ bytes[3];

        let expected_bytes = 4 + (length * 4) + 1;
        if byte_count < expected_bytes {
            rprintln!(
                "RX: Incomplete (need {} bytes, got {})",
                expected_bytes,
                byte_count
            );
            return None;
        }

        let mut payload: Vec<u32, 255> = Vec::new();
        for i in 0..length {
            let offset = 4 + (i * 4);
            let word = (bytes[offset] as u32)
                | ((bytes[offset + 1] as u32) << 8)
                | ((bytes[offset + 2] as u32) << 16)
                | ((bytes[offset + 3] as u32) << 24);
            payload.push(word).ok()?;
            crc ^= bytes[offset] ^ bytes[offset + 1] ^ bytes[offset + 2] ^ bytes[offset + 3];
        }

        let received_crc = bytes[4 + (length * 4)];
        if crc != received_crc {
            rprintln!(
                "RX: CRC error (calc=0x{:02X} recv=0x{:02X})",
                crc,
                received_crc
            );
            return None;
        }

        Some(MaplePacket {
            sender,
            recipient,
            command,
            payload,
        })
    }
}

// Type alias for backwards compatibility
pub type MapleBusGpioOut = MapleBus;
