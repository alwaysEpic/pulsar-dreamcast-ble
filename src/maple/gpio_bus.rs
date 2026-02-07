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
use core::sync::atomic::{Ordering, compiler_fence};
use heapless::Vec;
use nrf52840_dk_bsp::hal::gpio::{Input, Level, Output, Pin, PullUp, PushPull};
use nrf52840_dk_bsp::hal::pac::P0;
use nrf52840_dk_bsp::hal::prelude::*;
use rtt_target::rprintln;

/// Static buffer for bulk sampling (96KB). Pre-allocated to avoid runtime delay.
static mut SAMPLE_BUFFER: [u32; 24576] = [0; 24576];

const PIN_A_MASK: u32 = 1 << 5; // SDCKA on P0.05
const PIN_B_MASK: u32 = 1 << 6; // SDCKB on P0.06

/// ~500ns delay at 64MHz
#[inline(always)]
fn delay_half_bit() {
    for _ in 0..32 {
        cortex_m::asm::nop();
    }
    compiler_fence(Ordering::SeqCst);
}

/// GPIO-based Maple Bus driver.
pub struct MapleBusGpio<SDCKA, SDCKB> {
    sdcka: SDCKA,
    sdckb: SDCKB,
}

/// Type alias for output mode pins (push-pull for TX, switch to pull-up input for RX).
pub type MapleBusGpioOut = MapleBusGpio<Pin<Output<PushPull>>, Pin<Output<PushPull>>>;

impl MapleBusGpioOut {
    /// Create a new Maple Bus GPIO driver with pins in push-pull output mode.
    pub fn new(sdcka: Pin<Output<PushPull>>, sdckb: Pin<Output<PushPull>>) -> Self {
        Self { sdcka, sdckb }
    }

    /// Set bus to idle state (SDCKA high, SDCKB low).
    /// This is the state after transmission completes.
    #[inline(always)]
    pub fn set_idle(&mut self) {
        let _ = self.sdcka.set_high();
        let _ = self.sdckb.set_low();
    }

    /// Send the start/sync pattern.
    ///
    /// Pattern: SDCKA LOW, SDCKB pulsed 4 times, then SDCKB HIGH, SDCKA HIGH, SDCKB LOW.
    /// Final state: SDCKA=HIGH, SDCKB=LOW (ready for first bit).
    pub fn send_start_pattern(&mut self) {
        // 1. SDCKA immediately LOW (no initial HIGH state)
        let _ = self.sdcka.set_low();

        // 2. Toggle SDCKB 4 times: HIGH then LOW (while SDCKA stays LOW)
        for _ in 0..4 {
            let _ = self.sdckb.set_high();
            delay_half_bit();
            let _ = self.sdckb.set_low();
            delay_half_bit();
        }

        // 3. SDCKB HIGH
        let _ = self.sdckb.set_high();
        delay_half_bit();
        // SDCKA HIGH
        let _ = self.sdcka.set_high();
        delay_half_bit();
        // SDCKB LOW (final state)
        let _ = self.sdckb.set_low();
        delay_half_bit();
    }

    /// Send the end pattern to signal end of transmission.
    ///
    /// Per gmanmodz maple_protocol.cpp maple_terminate():
    /// A=1,B=1 → B=0 → A=0 → A=1 → A=0 → A=1 → B=1
    /// Final state: A=HIGH, B=HIGH (both HIGH)
    pub fn send_end_pattern(&mut self) {
        // A=HIGH, B=HIGH
        let _ = self.sdcka.set_high();
        let _ = self.sdckb.set_high();
        delay_half_bit();

        // B=LOW
        let _ = self.sdckb.set_low();
        delay_half_bit();

        // A=LOW
        let _ = self.sdcka.set_low();
        delay_half_bit();

        // A=HIGH
        let _ = self.sdcka.set_high();
        delay_half_bit();

        // A=LOW
        let _ = self.sdcka.set_low();
        delay_half_bit();

        // A=HIGH
        let _ = self.sdcka.set_high();
        delay_half_bit();

        // B=HIGH (final state)
        let _ = self.sdckb.set_high();
        delay_half_bit();
    }

    /// Write a single bit using the alternating clock/data scheme.
    ///
    /// Per gmanmodz maple_protocol.cpp:
    /// - Phase true: SDCKA = clock, SDCKB = data
    /// - Phase false: SDCKB = clock, SDCKA = data
    ///
    /// CRITICAL: After the clock falling edge, the DATA line returns HIGH
    /// (not the clock). This prepares the data line to become the clock
    /// in the next phase (it needs to be HIGH to generate a falling edge).
    #[inline(always)]
    pub fn write_bit(&mut self, bit: bool, phase: &mut bool) {
        if *phase {
            // Phase true: SDCKA = clock, SDCKB = data
            // Set data on SDCKB
            if bit {
                let _ = self.sdckb.set_high();
            } else {
                let _ = self.sdckb.set_low();
            }
            delay_half_bit();
            // Clock falling edge on SDCKA (triggers sampling)
            let _ = self.sdcka.set_low();
            delay_half_bit();
            // DATA line (SDCKB) returns HIGH (prepares to be clock next phase)
            let _ = self.sdckb.set_high();
        } else {
            // Phase false: SDCKB = clock, SDCKA = data
            // Set data on SDCKA
            if bit {
                let _ = self.sdcka.set_high();
            } else {
                let _ = self.sdcka.set_low();
            }
            delay_half_bit();
            // Clock falling edge on SDCKB (triggers sampling)
            let _ = self.sdckb.set_low();
            delay_half_bit();
            // DATA line (SDCKA) returns HIGH (prepares to be clock next phase)
            let _ = self.sdcka.set_high();
        }

        // Toggle phase for next bit
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

    /// Write a 32-bit word in Maple Bus byte order.
    /// "The last byte sends first" - LSB first.
    pub fn write_word(&mut self, word: u32, phase: &mut bool) {
        self.write_byte(word as u8, phase); // Byte 0 (LSB) - first
        self.write_byte((word >> 8) as u8, phase); // Byte 1
        self.write_byte((word >> 16) as u8, phase); // Byte 2
        self.write_byte((word >> 24) as u8, phase); // Byte 3 (MSB) - last
    }

    /// Write a complete packet (frame word + payload + CRC).
    pub fn write_packet(&mut self, packet: &MaplePacket) {
        let mut phase = true; // Start in phase 1

        // Send start pattern
        self.send_start_pattern();

        // Build the frame word
        let frame = packet.frame_word();

        // Calculate CRC as we go
        let mut crc: u8 = 0;

        // Write frame word
        self.write_word(frame, &mut phase);
        Self::update_crc(frame, &mut crc);

        // Write payload words
        for &word in packet.payload.iter() {
            self.write_word(word, &mut phase);
            Self::update_crc(word, &mut crc);
        }

        // Write CRC byte (as a word with padding)
        self.write_byte(crc, &mut phase);

        // Send end pattern
        self.send_end_pattern();
    }

    /// Update CRC with a word (bytewise XOR).
    fn update_crc(word: u32, crc: &mut u8) {
        *crc ^= (word & 0xFF) as u8;
        *crc ^= ((word >> 8) & 0xFF) as u8;
        *crc ^= ((word >> 16) & 0xFF) as u8;
        *crc ^= ((word >> 24) & 0xFF) as u8;
    }

    /// Convert pins to input mode for reading response.
    /// Returns a new MapleBusGpio with input pins.
    pub fn into_input(self) -> MapleBusGpio<Pin<Input<PullUp>>, Pin<Input<PullUp>>> {
        MapleBusGpio {
            sdcka: self.sdcka.into_pullup_input(),
            sdckb: self.sdckb.into_pullup_input(),
        }
    }
}

impl MapleBusGpio<Pin<Input<PullUp>>, Pin<Input<PullUp>>> {
    /// Read a single bit by detecting clock edges (phase-agnostic version).
    /// Watches for ANY falling edge and samples the other pin.
    /// Based on protocol: "a negative flank on any of the pins will always mean a valid bit on the other pin"
    #[inline(always)]
    pub fn read_bit_any_edge(&mut self, last_state: &mut u32, timeout: &mut u32) -> Option<bool> {
        let p0_in = unsafe { &(*P0::ptr()).in_ };

        loop {
            let val = p0_in.read().bits();
            let a = val & PIN_A_MASK;
            let b = val & PIN_B_MASK;
            let last_a = *last_state & PIN_A_MASK;
            let last_b = *last_state & PIN_B_MASK;

            // Check for falling edge on A (was HIGH, now LOW)
            if last_a != 0 && a == 0 {
                *last_state = val;
                return Some(b != 0); // Sample B
            }

            // Check for falling edge on B (was HIGH, now LOW)
            if last_b != 0 && b == 0 {
                *last_state = val;
                return Some(a != 0); // Sample A
            }

            *last_state = val;

            if *timeout == 0 {
                return None;
            }
            *timeout -= 1;
        }
    }

    /// Read a single bit by detecting clock edges (original phase-tracking version).
    /// Uses fast direct register access for 2Mbps timing.
    /// Returns None on timeout.
    /// If debug_raw is provided, stores the raw register value when bit was sampled.
    #[inline(always)]
    pub fn read_bit_timeout_raw(
        &mut self,
        phase: &mut bool,
        timeout: &mut u32,
        debug_raw: Option<&mut u32>,
    ) -> Option<bool> {
        // Get register address once - P0 IN register is at base + 0x510
        // PAC uses VolatileCell internally, so reads are volatile
        let p0_in = unsafe { &(*P0::ptr()).in_ };
        let bit;

        if *phase {
            // Phase 1: SDCKA = clock, SDCKB = data
            // Wait for A to go LOW (falling edge)
            loop {
                let val = p0_in.read().bits();
                if (val & PIN_A_MASK) == 0 {
                    // A is LOW - sample B from same read
                    bit = (val & PIN_B_MASK) != 0;
                    if let Some(raw) = debug_raw {
                        *raw = val;
                    }
                    break;
                }
                if *timeout == 0 {
                    return None;
                }
                *timeout -= 1;
            }
        } else {
            // Phase 2: SDCKB = clock, SDCKA = data
            // Wait for B to go LOW (falling edge)
            loop {
                let val = p0_in.read().bits();
                if (val & PIN_B_MASK) == 0 {
                    // B is LOW - sample A from same read
                    bit = (val & PIN_A_MASK) != 0;
                    if let Some(raw) = debug_raw {
                        *raw = val;
                    }
                    break;
                }
                if *timeout == 0 {
                    return None;
                }
                *timeout -= 1;
            }
        }

        *phase = !*phase;
        Some(bit)
    }

    /// Read a single bit by detecting clock edges.
    #[inline(always)]
    pub fn read_bit_timeout(&mut self, phase: &mut bool, timeout: &mut u32) -> Option<bool> {
        self.read_bit_timeout_raw(phase, timeout, None)
    }

    /// Read a byte using phase-agnostic edge detection. MSB first.
    pub fn read_byte_any_edge(&mut self, last_state: &mut u32, timeout: &mut u32) -> Option<u8> {
        let mut byte: u8 = 0;
        for _ in 0..8 {
            let bit = self.read_bit_any_edge(last_state, timeout)?;
            byte = (byte << 1) | (bit as u8);
        }
        Some(byte)
    }

    /// Read a 32-bit word using phase-agnostic edge detection.
    /// LSB byte comes first.
    pub fn read_word_any_edge(&mut self, last_state: &mut u32, timeout: &mut u32) -> Option<u32> {
        let b0 = self.read_byte_any_edge(last_state, timeout)? as u32; // LSB first
        let b1 = self.read_byte_any_edge(last_state, timeout)? as u32;
        let b2 = self.read_byte_any_edge(last_state, timeout)? as u32;
        let b3 = self.read_byte_any_edge(last_state, timeout)? as u32; // MSB last
        Some((b3 << 24) | (b2 << 16) | (b1 << 8) | b0)
    }

    /// Read a byte, MSB first. Returns None on timeout.
    /// If debug_bits is provided, stores the individual bits read.
    pub fn read_byte_timeout_debug(
        &mut self,
        phase: &mut bool,
        timeout: &mut u32,
        mut debug_bits: Option<&mut [u8; 8]>,
    ) -> Option<u8> {
        let mut byte: u8 = 0;
        for i in 0..8 {
            let bit = self.read_bit_timeout(phase, timeout)?;
            if let Some(ref mut bits) = debug_bits {
                bits[i] = bit as u8;
            }
            byte = (byte << 1) | (bit as u8);
        }
        Some(byte)
    }

    /// Read a byte, MSB first. Returns None on timeout.
    pub fn read_byte_timeout(&mut self, phase: &mut bool, timeout: &mut u32) -> Option<u8> {
        self.read_byte_timeout_debug(phase, timeout, None)
    }

    /// Read a 32-bit word in Maple Bus byte order. Returns None on timeout.
    /// LSB comes first.
    pub fn read_word_timeout(&mut self, phase: &mut bool, timeout: &mut u32) -> Option<u32> {
        let b0 = self.read_byte_timeout(phase, timeout)? as u32; // LSB first
        let b1 = self.read_byte_timeout(phase, timeout)? as u32;
        let b2 = self.read_byte_timeout(phase, timeout)? as u32;
        let b3 = self.read_byte_timeout(phase, timeout)? as u32; // MSB last
        Some((b3 << 24) | (b2 << 16) | (b1 << 8) | b0)
    }

    /// Wait for start pattern from peripheral (response).
    /// Returns (success, wait_cycles, b_transitions) for debugging after capture.
    /// CRITICAL: No prints in this function - they break timing!
    pub fn wait_for_start_silent(&mut self, timeout_cycles: u32) -> (bool, u32, u32) {
        let p0_in = unsafe { &(*P0::ptr()).in_ };

        // Step 1: Wait for bus to be IDLE (both A and B HIGH)
        let mut count = 0u32;
        loop {
            let val = p0_in.read().bits();
            let a_high = (val & PIN_A_MASK) != 0;
            let b_high = (val & PIN_B_MASK) != 0;
            if a_high && b_high {
                break;
            }
            count += 1;
            if count > timeout_cycles / 2 {
                return (false, count, 0);
            }
        }
        let idle_cycles = count;

        // Step 3: Wait for SDCKA to go LOW (controller starts its response)
        count = 0;
        loop {
            let val = p0_in.read().bits();
            if (val & PIN_A_MASK) == 0 {
                break;
            }
            count += 1;
            if count > timeout_cycles {
                return (false, idle_cycles + count, 0);
            }
        }
        let wait_cycles = idle_cycles + count;

        // Step 4: Start pattern - A stays LOW while B toggles 4 times (8 transitions)
        // Wait for A to go HIGH, counting B transitions
        let mut b_transitions = 0u32;
        let mut last_b = (p0_in.read().bits() & PIN_B_MASK) != 0;
        count = 0;

        loop {
            let val = p0_in.read().bits();
            let a = (val & PIN_A_MASK) != 0;
            let b = (val & PIN_B_MASK) != 0;

            if b != last_b {
                b_transitions += 1;
                last_b = b;
            }

            if a {
                // A went HIGH - check if this was a REAL start pattern
                // Real start pattern has ~8 B transitions (4 toggles)
                // If we only saw < 6 transitions, it's likely a false detection
                if b_transitions >= 3 {
                    // Valid start pattern
                    return (true, wait_cycles, b_transitions);
                } else {
                    // False start - keep waiting for real start pattern
                    // Go back to waiting for idle
                    let mut idle_count = 0u32;
                    loop {
                        let val2 = p0_in.read().bits();
                        let a2 = (val2 & PIN_A_MASK) != 0;
                        let b2 = (val2 & PIN_B_MASK) != 0;
                        if a2 && b2 {
                            idle_count += 1;
                            if idle_count > 50 {
                                break; // Found stable idle, continue to look for real start
                            }
                        } else {
                            idle_count = 0;
                        }
                        count += 1;
                        if count > timeout_cycles {
                            return (false, wait_cycles, b_transitions);
                        }
                    }
                    // Now look for A to go LOW again (real start pattern)
                    loop {
                        let val2 = p0_in.read().bits();
                        if (val2 & PIN_A_MASK) == 0 {
                            // Found A low - reset and count B transitions
                            b_transitions = 0;
                            last_b = (val2 & PIN_B_MASK) != 0;
                            break;
                        }
                        count += 1;
                        if count > timeout_cycles {
                            return (false, wait_cycles, b_transitions);
                        }
                    }
                    // Continue the outer loop to count B transitions
                }
            }

            count += 1;
            if count > 10000 {
                return (false, wait_cycles, b_transitions);
            }
        }
    }

    /// Wait for start pattern from peripheral (response).
    /// Returns true if start pattern detected, false on timeout.
    pub fn wait_for_start(&mut self, timeout_cycles: u32) -> bool {
        let p0_in = unsafe { &(*P0::ptr()).in_ };

        // Wait for SDCKA to go LOW (controller starts response)
        let mut count = 0u32;
        loop {
            if (p0_in.read().bits() & PIN_A_MASK) == 0 {
                break;
            }
            count += 1;
            if count > timeout_cycles {
                return false;
            }
        }

        // Wait for A to go HIGH (start pattern complete)
        count = 0;
        loop {
            if (p0_in.read().bits() & PIN_A_MASK) != 0 {
                // Small delay to let signals settle
                let _ = p0_in.read().bits();
                let _ = p0_in.read().bits();
                return true;
            }
            count += 1;
            if count > 10000 {
                return false;
            }
        }
    }

    /// Read a response packet using real-time edge detection.
    /// Note: Prefer read_packet_bulk() for more reliable reception.
    pub fn read_packet(&mut self, timeout_cycles: u32) -> Option<MaplePacket> {
        if !self.wait_for_start(timeout_cycles) {
            rprintln!("RX: No start pattern");
            return None;
        }

        // After start pattern: A=HIGH, B=LOW - Phase 1 (A=clock, B=data)
        let mut phase = true;
        let mut crc: u8 = 0;
        let mut timeout = timeout_cycles;

        // Read frame word
        let b0 = self.read_byte_timeout(&mut phase, &mut timeout)?;
        let b1 = self.read_byte_timeout(&mut phase, &mut timeout)?;
        let b2 = self.read_byte_timeout(&mut phase, &mut timeout)?;
        let b3 = self.read_byte_timeout(&mut phase, &mut timeout)?;
        let frame = ((b3 as u32) << 24) | ((b2 as u32) << 16) | ((b1 as u32) << 8) | (b0 as u32);

        let command = ((frame >> 24) & 0xFF) as u8;
        let recipient = ((frame >> 16) & 0xFF) as u8;
        let sender = ((frame >> 8) & 0xFF) as u8;
        let length = (frame & 0xFF) as usize;

        rprintln!(
            "RX: Frame=0x{:08X} cmd=0x{:02X} len={}",
            frame,
            command,
            length
        );

        if length > 255 {
            return None;
        }

        // Update CRC with frame
        crc ^= b0 ^ b1 ^ b2 ^ b3;

        // Read payload
        let mut payload: Vec<u32, 255> = Vec::new();
        for _ in 0..length {
            let word = self.read_word_timeout(&mut phase, &mut timeout)?;
            payload.push(word).ok()?;
            crc ^= (word & 0xFF) as u8;
            crc ^= ((word >> 8) & 0xFF) as u8;
            crc ^= ((word >> 16) & 0xFF) as u8;
            crc ^= ((word >> 24) & 0xFF) as u8;
        }

        // Verify CRC
        let received_crc = self.read_byte_timeout(&mut phase, &mut timeout)?;
        if crc != received_crc {
            rprintln!("RX: CRC error");
            return None;
        }

        rprintln!("RX: OK!");
        Some(MaplePacket {
            sender,
            recipient,
            command,
            payload,
        })
    }

    /// Wait for start pattern and immediately bulk sample.
    /// Combining these avoids the function-call delay that caused missed edges.
    pub fn wait_and_sample(
        &mut self,
        timeout_cycles: u32,
    ) -> (bool, u32, u32, usize, &'static [u32; 24576]) {
        let p0_in = unsafe { &(*P0::ptr()).in_ };
        let samples = unsafe {
            core::ptr::addr_of_mut!(SAMPLE_BUFFER)
                .as_mut()
                .unwrap_unchecked()
        };

        // Wait for idle (both HIGH)
        let mut count = 0u32;
        loop {
            let val = p0_in.read().bits();
            if (val & PIN_A_MASK) != 0 && (val & PIN_B_MASK) != 0 {
                break;
            }
            count += 1;
            if count > timeout_cycles / 2 {
                return (false, count, 0, 0, unsafe {
                    core::ptr::addr_of!(SAMPLE_BUFFER)
                        .as_ref()
                        .unwrap_unchecked()
                });
            }
        }
        let idle_cycles = count;

        // Wait for A LOW (controller starts response)
        count = 0;
        loop {
            let val = p0_in.read().bits();
            if (val & PIN_A_MASK) == 0 {
                break;
            }
            count += 1;
            if count > timeout_cycles {
                return (false, idle_cycles + count, 0, 0, unsafe {
                    core::ptr::addr_of!(SAMPLE_BUFFER)
                        .as_ref()
                        .unwrap_unchecked()
                });
            }
        }
        let wait_cycles = idle_cycles + count;

        // Count B transitions while A is LOW, wait for A HIGH
        let mut b_transitions = 0u32;
        let mut last_b = (p0_in.read().bits() & PIN_B_MASK) != 0;
        count = 0;

        loop {
            let val = p0_in.read().bits();
            let a = (val & PIN_A_MASK) != 0;
            let b = (val & PIN_B_MASK) != 0;

            if b != last_b {
                b_transitions += 1;
                last_b = b;
            }

            if a && b_transitions >= 3 {
                // Valid start pattern - sample IMMEDIATELY (no return delay!)
                compiler_fence(Ordering::SeqCst);
                for sample in samples.iter_mut() {
                    *sample = p0_in.read().bits();
                }
                compiler_fence(Ordering::SeqCst);
                return (true, wait_cycles, b_transitions, 24576, unsafe {
                    core::ptr::addr_of!(SAMPLE_BUFFER)
                        .as_ref()
                        .unwrap_unchecked()
                });
            }

            count += 1;
            if count > 10000 {
                return (false, wait_cycles, b_transitions, 0, unsafe {
                    core::ptr::addr_of!(SAMPLE_BUFFER)
                        .as_ref()
                        .unwrap_unchecked()
                });
            }
        }
    }

    /// Bulk sample GPIO register. Prefer wait_and_sample() for RX.
    pub fn bulk_sample(&mut self) -> (usize, &'static [u32; 24576]) {
        let p0_in = unsafe { &(*P0::ptr()).in_ };
        let samples = unsafe {
            core::ptr::addr_of_mut!(SAMPLE_BUFFER)
                .as_mut()
                .unwrap_unchecked()
        };

        compiler_fence(Ordering::SeqCst);
        for sample in samples.iter_mut() {
            *sample = p0_in.read().bits();
        }
        compiler_fence(Ordering::SeqCst);

        (24576, unsafe {
            core::ptr::addr_of!(SAMPLE_BUFFER)
                .as_ref()
                .unwrap_unchecked()
        })
    }

    /// Decode bits from bulk samples. Detects falling edges and extracts data.
    /// Enforces phase alignment: skips B edges until first A fall.
    /// Detects inter-chunk gaps and resets phase after each.
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
                    seen_first_a_fall = false; // Reset phase after gap
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

    /// Debug: Find first N edges and their sample indices
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

            // Falling edge on A (i+1 because we started at index 1)
            if last_a && !a {
                let _ = edges.push((i + 1, 'A', b));
            }
            // Falling edge on B
            if last_b && !b {
                let _ = edges.push((i + 1, 'B', a));
            }

            last_a = a;
            last_b = b;
        }

        edges
    }

    /// Read a complete response packet using bulk sampling.
    /// More reliable than real-time edge detection at 2Mbps.
    pub fn read_packet_bulk(&mut self, timeout_cycles: u32) -> Option<MaplePacket> {
        let (success, _wait_cycles, b_trans, count, samples) = self.wait_and_sample(timeout_cycles);

        if !success {
            rprintln!("RX: No start pattern (b_trans={})", b_trans);
            return None;
        }

        // Check first edge to detect late start (false start pattern)
        let edges = self.find_first_edges(samples, count, 40);
        let first_edge_idx = edges.first().map(|(idx, _, _)| *idx).unwrap_or(0);
        let skip_samples = if first_edge_idx > 100 {
            first_edge_idx.saturating_sub(10)
        } else {
            0
        };

        // Decode bits from samples
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

        rprintln!(
            "RX: Frame=0x{:08X} cmd=0x{:02X} len={}",
            frame,
            command,
            length
        );

        // Calculate CRC over frame
        let mut crc: u8 = bytes[0] ^ bytes[1] ^ bytes[2] ^ bytes[3];

        // Verify we have enough data
        let expected_bytes = 4 + (length * 4) + 1;
        if byte_count < expected_bytes {
            rprintln!(
                "RX: Incomplete (need {} bytes, got {})",
                expected_bytes,
                byte_count
            );
            return None;
        }

        // Parse payload and update CRC
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

        // Verify CRC
        let received_crc = bytes[4 + (length * 4)];
        if crc != received_crc {
            rprintln!(
                "RX: CRC error (calc=0x{:02X} recv=0x{:02X})",
                crc,
                received_crc
            );
            return None;
        }

        rprintln!("RX: OK!");
        Some(MaplePacket {
            sender,
            recipient,
            command,
            payload,
        })
    }

    /// Convert back to output mode (push-pull).
    pub fn into_output(self) -> MapleBusGpioOut {
        MapleBusGpio {
            sdcka: self.sdcka.into_push_pull_output(Level::High),
            sdckb: self.sdckb.into_push_pull_output(Level::Low),
        }
    }
}
