// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

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

use crate::maple::MaplePacket;
use core::sync::atomic::{compiler_fence, Ordering};
use embassy_nrf::gpio::{Flex, Pull};
use heapless::Vec;

/// Number of u32 samples in the bulk capture buffer.
const SAMPLE_BUFFER_LEN: usize = 24576;

/// NOP iterations for a ~500ns half-bit delay at 64MHz.
const HALF_BIT_NOPS: u32 = 32;

/// NOP iterations for pin stabilization after output mode set.
const PIN_STABILIZE_NOPS: u32 = 100;

/// NOP iterations for pull-up stabilization after input mode set.
const PULLUP_STABILIZE_NOPS: u32 = 200;

/// Minimum B-line transitions required to accept a valid start pattern.
const MIN_START_TRANSITIONS: u32 = 3;

/// Maximum busy-loop iterations waiting for start pattern completion.
const START_PATTERN_TIMEOUT: u32 = 10_000;

/// Number of start pattern B-line toggles.
const START_TOGGLE_COUNT: u32 = 4;

/// Minimum bits required to attempt packet decode.
const MIN_DECODE_BITS: usize = 32;

/// Minimum bytes required for a valid frame header.
const MIN_FRAME_BYTES: usize = 4;

/// Static buffer for bulk sampling (96KB, 37% of RAM). Pre-allocated to avoid runtime delay.
///
/// This size is intentional: the entire controller response must be captured in one
/// uninterrupted burst at ~12.5MHz. On-the-fly processing would miss edges. The buffer
/// includes headroom for the idle/wait period before the response starts.
///
/// # Safety
/// Accessed only from `wait_and_sample()` and `receive_frame()`, which run
/// sequentially on a single-core Cortex-M4 with interrupts disabled during
/// the sampling window. No concurrent or overlapping references are possible.
static mut SAMPLE_BUFFER: [u32; SAMPLE_BUFFER_LEN] = [0; SAMPLE_BUFFER_LEN];

const PIN_A_MASK: u32 = 1 << crate::board::PIN_A_BIT;
const PIN_B_MASK: u32 = 1 << crate::board::PIN_B_BIT;

/// P0 GPIO base address for direct register access.
const P0_BASE: u32 = 0x5000_0000;
/// Offset to IN register within GPIO peripheral.
const GPIO_IN_OFFSET: u32 = 0x510;

/// Read P0 IN register directly.
#[inline]
fn read_p0_in() -> u32 {
    // MMIO register access requires integer-to-pointer cast
    unsafe { core::ptr::read_volatile((P0_BASE + GPIO_IN_OFFSET) as *const u32) }
}

/// ~500ns delay at 64MHz
#[inline]
fn delay_half_bit() {
    for _ in 0..HALF_BIT_NOPS {
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
    #[must_use]
    #[allow(clippy::similar_names)] // sdcka/sdckb are protocol names
    pub fn new(mut sdcka: Flex<'static>, mut sdckb: Flex<'static>) -> Self {
        // Start in output mode with idle state
        sdcka.set_as_output(embassy_nrf::gpio::OutputDrive::HighDrive);
        sdckb.set_as_output(embassy_nrf::gpio::OutputDrive::HighDrive);
        sdcka.set_high();
        sdckb.set_low();

        // Small delay for pins to stabilize
        for _ in 0..PIN_STABILIZE_NOPS {
            cortex_m::asm::nop();
        }

        Self { sdcka, sdckb }
    }

    /// Set pins to lowest-power disconnected state.
    ///
    /// Call when not polling (BLE disconnected). External pull-ups hold both
    /// lines at 3.3V with zero current flow. Saves ~0.7 mA vs idle state
    /// where SDCKB is driven LOW against its pull-up.
    pub fn set_low_power(&mut self) {
        self.sdcka.set_as_disconnected();
        self.sdckb.set_as_disconnected();
    }

    /// Configure pins as outputs (push-pull).
    pub fn set_output_mode(&mut self) {
        self.sdcka
            .set_as_output(embassy_nrf::gpio::OutputDrive::HighDrive);
        self.sdckb
            .set_as_output(embassy_nrf::gpio::OutputDrive::HighDrive);
    }

    /// Read current pin states (for diagnostics).
    pub fn read_pins(&mut self) -> (bool, bool) {
        self.sdcka.set_as_input(Pull::None);
        self.sdckb.set_as_input(Pull::None);
        for _ in 0..PULLUP_STABILIZE_NOPS {
            cortex_m::asm::nop();
        }
        let a = self.sdcka.is_high();
        let b = self.sdckb.is_high();
        // Restore output mode
        self.sdcka
            .set_as_output(embassy_nrf::gpio::OutputDrive::HighDrive);
        self.sdckb
            .set_as_output(embassy_nrf::gpio::OutputDrive::HighDrive);
        self.sdcka.set_high();
        self.sdckb.set_low();
        (a, b)
    }

    /// Diagnostic: sample the bus briefly and report what we see.
    /// Call this after TX to check if any activity is present.
    pub fn diagnose_bus(&mut self) {
        use crate::log;
        self.set_input_mode();

        // Quick sample: 1000 reads
        let mut a_low_count: u32 = 0;
        let mut b_low_count: u32 = 0;
        let mut transitions: u32 = 0;
        let mut last = read_p0_in();

        for _ in 0..1000 {
            let val = read_p0_in();
            if val & PIN_A_MASK == 0 {
                a_low_count += 1;
            }
            if val & PIN_B_MASK == 0 {
                b_low_count += 1;
            }
            if (val ^ last) & (PIN_A_MASK | PIN_B_MASK) != 0 {
                transitions += 1;
            }
            last = val;
        }

        let final_val = read_p0_in();
        let final_a = (final_val & PIN_A_MASK) != 0;
        let final_b = (final_val & PIN_B_MASK) != 0;
        log!(
            "DIAG: A_low={}/1000 B_low={}/1000 trans={} final A={} B={}",
            a_low_count,
            b_low_count,
            transitions,
            u8::from(final_a),
            u8::from(final_b)
        );

        // Restore output idle
        self.set_output_mode();
        self.sdcka.set_high();
        self.sdckb.set_low();
    }

    /// Configure pins as inputs without pull-up.
    pub fn set_input_mode(&mut self) {
        self.sdcka.set_as_input(Pull::None);
        self.sdckb.set_as_input(Pull::None);
        // Allow pull-ups to stabilize
        for _ in 0..PULLUP_STABILIZE_NOPS {
            cortex_m::asm::nop();
        }
    }

    /// Set bus to idle state (SDCKA high, SDCKB low).
    #[inline]
    pub fn set_idle(&mut self) {
        self.sdcka.set_high();
        self.sdckb.set_low();
    }

    /// Send the start/sync pattern.
    pub fn send_start_pattern(&mut self) {
        // SDCKA LOW
        self.sdcka.set_low();

        // Toggle SDCKB 4 times
        for _ in 0..START_TOGGLE_COUNT {
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
    #[inline]
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
    #[inline]
    pub fn write_byte(&mut self, byte: u8, phase: &mut bool) {
        for i in (0..8).rev() {
            let bit = (byte >> i) & 1 == 1;
            self.write_bit(bit, phase);
        }
    }

    /// Write a 32-bit word in Maple Bus byte order (LSB first).
    pub fn write_word(&mut self, word: u32, phase: &mut bool) {
        let bytes = word.to_le_bytes();
        for &b in &bytes {
            self.write_byte(b, phase);
        }
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

        for &word in &packet.payload {
            self.write_word(word, &mut phase);
            Self::update_crc(word, &mut crc);
        }

        self.write_byte(crc, &mut phase);
        self.send_end_pattern();
    }

    /// Update CRC with a word (bytewise XOR).
    fn update_crc(word: u32, crc: &mut u8) {
        for &b in &word.to_le_bytes() {
            *crc ^= b;
        }
    }

    /// Wait for start pattern and bulk sample.
    /// Returns `(success, wait_cycles, b_transitions, sample_count)`.
    pub fn wait_and_sample(&mut self, timeout_cycles: u32) -> (bool, u32, u32, usize) {
        self.set_input_mode();

        // SAFETY: Single-core, interrupts disabled during sampling. Only one
        // mutable reference exists at a time — no concurrent access possible.
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

            if a && b_transitions >= MIN_START_TRANSITIONS {
                // Valid start pattern - sample immediately
                compiler_fence(Ordering::SeqCst);
                for sample in samples.iter_mut() {
                    *sample = read_p0_in();
                }
                compiler_fence(Ordering::SeqCst);
                return (true, wait_cycles, b_transitions, SAMPLE_BUFFER_LEN);
            }

            count += 1;
            if count > START_PATTERN_TIMEOUT {
                return (false, wait_cycles, b_transitions, 0);
            }
        }
    }

    /// Decode bits from bulk samples.
    #[must_use]
    #[allow(clippy::unused_self)] // Method on bus for API consistency
    pub fn decode_bulk_samples(
        &self,
        samples: &[u32],
        count: usize,
        start_sample: usize,
    ) -> (Vec<u8, 960>, u32, u32, u32) {
        const GAP_THRESHOLD: usize = 50;

        let mut bits: Vec<u8, 960> = Vec::new();
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
                let _ = bits.push(u8::from(b));
                a_falls += 1;
            }
            // B falls -> sample A (Phase 2), but only after first A fall
            else if last_b && !b {
                if seen_first_a_fall {
                    let _ = bits.push(u8::from(a));
                }
                b_falls += 1;
            }

            last_a = a;
            last_b = b;
        }

        (bits, a_falls, b_falls, gaps_detected)
    }

    /// Find first N edges and their sample indices.
    #[must_use]
    #[allow(clippy::unused_self)] // Method on bus for API consistency
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
        let (success, _wait_cycles, _b_trans, count) = self.wait_and_sample(timeout_cycles);

        if !success {
            // No start pattern detected
            return None;
        }

        // SAFETY: Single-core, interrupts disabled during sampling. The mutable
        // reference from `wait_and_sample` has been dropped before this read.
        let samples = unsafe {
            core::ptr::addr_of!(SAMPLE_BUFFER)
                .as_ref()
                .unwrap_unchecked()
        };

        let edges = self.find_first_edges(samples, count, 40);
        let first_edge_idx = edges.first().map_or(0, |(idx, _, _)| *idx);
        let skip_samples = if first_edge_idx > 100 {
            first_edge_idx.saturating_sub(10)
        } else {
            0
        };

        let (bits, _a_falls, _b_falls, _gaps) =
            self.decode_bulk_samples(samples, count, skip_samples);

        if bits.len() < MIN_DECODE_BITS {
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

        if byte_count < MIN_FRAME_BYTES {
            // Not enough bytes for frame
            return None;
        }

        // Parse frame word (LSB byte first)
        let frame = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);

        #[allow(clippy::cast_possible_truncation)]
        let command = ((frame >> 24) & 0xFF) as u8;
        #[allow(clippy::cast_possible_truncation)]
        let recipient = ((frame >> 16) & 0xFF) as u8;
        #[allow(clippy::cast_possible_truncation)]
        let sender = ((frame >> 8) & 0xFF) as u8;
        let length = (frame & 0xFF) as usize;

        let mut crc: u8 = bytes[0] ^ bytes[1] ^ bytes[2] ^ bytes[3];

        let expected_bytes = 4 + (length * 4) + 1;
        if byte_count < expected_bytes {
            // Incomplete packet
            return None;
        }

        let mut payload: Vec<u32, 32> = Vec::new();
        for i in 0..length {
            let offset = 4 + (i * 4);
            let word = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            payload.push(word).ok()?;
            crc ^= bytes[offset] ^ bytes[offset + 1] ^ bytes[offset + 2] ^ bytes[offset + 3];
        }

        let received_crc = bytes[4 + (length * 4)];
        if crc != received_crc {
            // CRC error
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
