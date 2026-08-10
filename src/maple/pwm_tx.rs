// SPDX-License-Identifier: GPL-3.0-or-later

//! Hardware-timed Maple Bus TX via PWM + EasyDMA.
//!
//! ⚠ SCOPE (2026-06-13): currently used ONLY for VMU LCD writes
//! ([`write_lcd_dma`]). The controller-command path ([`write_packet_dma`]) is
//! built but NOT wired in — it works on the DK but bricks Maple I/O on the
//! XIAO, while this same engine drives the VMU LCD fine on both boards. So the
//! controller poll TX is bit-banged (`gpio_bus::write_packet`) for now. See
//! xiao_debug_log.md 2026-06-13 (bisect + two failed fix trials + punt).
//!
//! CPU bit-banging is wrong on this chip twice over:
//! 1. Radio interrupts stretch the driven waveform mid-frame (debug log
//!    2026-06-10/11) — corrupted frames, perturbed controller.
//! 2. The waveform's timing IS the speed of the compiled code, and under
//!    `lto = "fat"` + `codegen-units = 1` that code is re-rolled by ANY
//!    change anywhere in the crate (debug log 2026-06-12: toggling the
//!    `vmu` feature recompiled `write_packet` 225→161 instructions and the
//!    controller began garbling ~2/3 of commands).
//!
//! The nRF52 community hit the same wall driving WS2812 LEDs and converged
//! on the same fix: pre-compute the waveform in RAM and let a hardware
//! peripheral play it out via EasyDMA — timing immune to interrupts,
//! codegen, and layout, with zero CPU during TX.
//!
//! # Design
//!
//! - PWM0 in **grouped decoder mode**: each waveform step loads two
//!   halfwords — group 0 (channels 0/1) drives SDCKA, group 1 (channels 2/3)
//!   drives SDCKB. Channels 1 and 3 stay disconnected.
//! - `COUNTERTOP = 8` at 16MHz ⇒ one step per 0.5µs, matching the bit-bang
//!   `delay_half_bit` design timing. The full LCD frame is ~3.3k steps ≈
//!   **1.7ms on the wire** — less than a quarter of the bit-banged 7.6ms,
//!   because the CPU pin-op overhead is gone.
//! - Constant levels per step: compare `0x8000 | COUNTERTOP` = high for the
//!   whole period, `0x8000` = low for the whole period (falling-edge
//!   polarity). Verify edge behavior on the scope against a bit-banged
//!   reference capture in `signal_references/` if frames don't ACK.
//! - The waveform shares `SAMPLE_BUFFER` with RX bulk capture (96KB, only
//!   ~13KB used) — TX and RX never overlap, same exclusivity argument as
//!   `wait_and_sample`. The builder mirrors `gpio_bus`'s TX primitives
//!   step-for-step (start pattern, phase-alternating bits, end pattern); a
//!   pin write between two delays lands on the following step boundary,
//!   exactly like the real pins do.
//! - During playback the caller `await`s, so the executor (SoftDevice
//!   runner, HID notify task) runs freely — the TX no longer blocks
//!   anything.
//!
//! Fire-and-forget like the bit-bang path: no ACK read; a corrupted frame is
//! rejected by the VMU's CRC and replaced by the next refresh.

use embassy_time::{Duration, Timer};

use super::gpio_bus::MapleBus;
use super::MaplePacket;

/// PWM0 base address (nRF52840). PWM0 is reserved for this module; the
/// pulsarv1 WS2812 strip was moved to PWM2 after sharing it broke both
/// and is not a SoftDevice-reserved peripheral.
const PWM0_BASE: u32 = 0x4001_C000;

const TASKS_STOP: u32 = 0x004;
const TASKS_SEQSTART0: u32 = 0x008;
const EVENTS_SEQEND0: u32 = 0x110;
const REG_ENABLE: u32 = 0x500;
const REG_MODE: u32 = 0x504;
const REG_COUNTERTOP: u32 = 0x508;
const REG_PRESCALER: u32 = 0x50C;
const REG_DECODER: u32 = 0x510;
const REG_LOOP: u32 = 0x514;
const SEQ0_PTR: u32 = 0x520;
const SEQ0_CNT: u32 = 0x524;
const SEQ0_REFRESH: u32 = 0x528;
const SEQ0_ENDDELAY: u32 = 0x52C;
const PSEL_OUT0: u32 = 0x560;
const PSEL_OUT1: u32 = 0x564;
const PSEL_OUT2: u32 = 0x568;
const PSEL_OUT3: u32 = 0x56C;

const PSEL_DISCONNECTED: u32 = 0xFFFF_FFFF;

/// PWM ticks per waveform step: 8 ticks at 16MHz = 0.5µs, the Maple half-bit.
const COUNTERTOP: u16 = 8;

/// Constant-high for a whole step (falling-edge polarity, compare at top).
const HIGH: u16 = 0x8000 | COUNTERTOP;
/// Constant-low for a whole step (falling-edge polarity, compare at zero).
const LOW: u16 = 0x8000;

#[inline]
fn pwm_write(offset: u32, value: u32) {
    // SAFETY: PWM0 is reserved for this module. NOTE: this is enforced only by
    // convention — we take no `Peri` handle, so a second user is invisible to
    // the type system (the WS2812 driver did exactly that). (nothing else in the
    // firmware or the SoftDevice touches it); MMIO writes to it are sound.
    unsafe { core::ptr::write_volatile((PWM0_BASE + offset) as *mut u32, value) }
}

#[inline]
fn pwm_read(offset: u32) -> u32 {
    // SAFETY: see `pwm_write`.
    unsafe { core::ptr::read_volatile((PWM0_BASE + offset) as *const u32) }
}

/// Mirrors `gpio_bus`'s TX primitives into a PWM sample sequence: pin writes
/// update pending levels, each `delay_half_bit()` equivalent emits one step.
struct WaveformBuilder {
    buf: &'static mut [u16],
    n: usize,
    a: bool,
    b: bool,
}

impl WaveformBuilder {
    fn new(buf: &'static mut [u16]) -> Self {
        Self {
            buf,
            n: 0,
            a: true,
            b: false, // driven idle: SDCKA high, SDCKB low (set_idle)
        }
    }

    /// Emit one 0.5µs step at the current levels (= `delay_half_bit`).
    fn step(&mut self) {
        debug_assert!(self.n + 2 <= self.buf.len());
        self.buf[self.n] = if self.a { HIGH } else { LOW };
        self.buf[self.n + 1] = if self.b { HIGH } else { LOW };
        self.n += 2;
    }

    /// Mirrors `send_start_pattern`.
    fn start_pattern(&mut self) {
        self.a = false;
        for _ in 0..4 {
            self.b = true;
            self.step();
            self.b = false;
            self.step();
        }
        self.b = true;
        self.step();
        self.a = true;
        self.step();
        self.b = false;
        self.step();
    }

    /// Mirrors `send_end_pattern` (final state: both lines high).
    fn end_pattern(&mut self) {
        self.a = true;
        self.b = true;
        self.step();
        self.b = false;
        self.step();
        self.a = false;
        self.step();
        self.a = true;
        self.step();
        self.a = false;
        self.step();
        self.a = true;
        self.step();
        self.b = true;
        self.step();
    }

    /// Mirrors `write_bit` (phase-alternating clock/data, two steps per bit;
    /// the trailing data-line restore lands on the next step boundary).
    fn write_bit(&mut self, bit: bool, phase: &mut bool) {
        if *phase {
            self.b = bit;
            self.step();
            self.a = false;
            self.step();
            self.b = true;
        } else {
            self.a = bit;
            self.step();
            self.b = false;
            self.step();
            self.a = true;
        }
        *phase = !*phase;
    }

    /// Mirrors `write_byte` (MSB first).
    fn write_byte(&mut self, byte: u8, phase: &mut bool) {
        for i in (0..8).rev() {
            self.write_bit((byte >> i) & 1 == 1, phase);
        }
    }

    /// Mirrors `write_word` (LSB byte first).
    fn write_word(&mut self, word: u32, phase: &mut bool) {
        for &b in &word.to_le_bytes() {
            self.write_byte(b, phase);
        }
    }
}

fn update_crc(word: u32, crc: &mut u8) {
    for &b in &word.to_le_bytes() {
        *crc ^= b;
    }
}

/// Build the LCD BLOCK_WRITE waveform (mirrors `gpio_bus::write_lcd` exactly,
/// including the pixel byte-swap) and play it via PWM0 + EasyDMA.
///
/// Fire-and-forget: returns after the waveform has finished on the wire and
/// the bus has been released to input mode (VMU drives its unobserved ACK).
pub async fn write_lcd_dma(bus: &mut MapleBus, sender: u8, dest: u8, framebuffer: &[u8; 192]) {
    // --- Build the waveform ---------------------------------------------
    let buf = super::gpio_bus::tx_waveform_buf();
    let mut w = WaveformBuilder::new(buf);

    // Driven idle for a few steps before the start pattern (the bit-bang
    // path stabilizes with one delay; extra margin is free here).
    for _ in 0..4 {
        w.step();
    }
    w.start_pattern();

    let mut phase = true;
    let mut crc: u8 = 0;

    let frame: u32 = (0x0C_u32 << 24) | (u32::from(dest) << 16) | (u32::from(sender) << 8) | 50;
    w.write_word(frame, &mut phase);
    update_crc(frame, &mut crc);

    let func: u32 = 0x0000_0004;
    w.write_word(func, &mut phase);
    update_crc(func, &mut crc);

    let loc: u32 = 0x0000_0000;
    w.write_word(loc, &mut phase);
    update_crc(loc, &mut crc);

    for chunk in framebuffer.chunks_exact(4) {
        let word = u32::from_le_bytes([chunk[3], chunk[2], chunk[1], chunk[0]]);
        w.write_word(word, &mut phase);
        update_crc(word, &mut crc);
    }

    w.write_byte(crc, &mut phase);
    w.end_pattern();

    let halfwords = w.n;
    let steps = halfwords / 2;
    let buf_ptr = w.buf.as_ptr() as u32;
    // Wire time: steps × 0.5µs.
    #[allow(clippy::cast_possible_truncation)]
    let duration_us = (steps as u32) / 2;

    start_playback(bus, buf_ptr, halfwords);

    // --- Hardware plays; the executor runs (SoftDevice, HID notify) ------
    Timer::after(Duration::from_micros(u64::from(duration_us) + 100)).await;
    let mut guard = 0u32;
    while pwm_read(EVENTS_SEQEND0) == 0 {
        guard += 1;
        if guard > 10 {
            // Should not happen (sequence is self-terminating); stop and bail.
            pwm_write(TASKS_STOP, 1);
            break;
        }
        Timer::after(Duration::from_micros(100)).await;
    }

    finish_playback(bus);
    // Release the bus for the VMU's ACK (unobserved).
    bus.set_input_mode();
}

/// Build a command-packet waveform (mirrors `gpio_bus::write_packet` exactly)
/// and play it via PWM0 + EasyDMA, busy-waiting for completion.
///
/// ⚠ NOT CURRENTLY WIRED IN — the controller-poll TX is bit-banged
/// (`gpio_bus::write_packet`) instead. This path works on the DK but on the
/// XIAO the controller never returns a valid response, even though the SAME
/// PWM/EasyDMA engine drives the VMU LCD correctly on both boards via
/// [`write_lcd_dma`]. So the fault is specific to THIS function (command TX
/// + its busy-wait/RX hand-off), not the DMA engine. Root cause open (debug
/// log 2026-06-13). Kept for the in-progress fix; re-wire the `host.rs` call
/// sites once it's confirmed working on the XIAO.
///
/// This was the controller-poll TX path. It exists because the bit-banged
/// `write_packet`'s waveform timing IS the speed of its compiled code, and
/// under fat LTO that code is re-rolled by any unrelated change anywhere in
/// the crate (debug log 2026-06-12: enabling the `vmu` feature alone
/// recompiled `write_packet` from 225 to 161 instructions and the controller
/// began garbling ~2/3 of commands). Hardware-timed playback is immune to
/// codegen, layout, and interrupts permanently.
///
/// Blocking, not async: the controller starts answering ~50µs after the end
/// pattern, so the caller must be sampling immediately — timer-granularity
/// wakeups (~30µs + executor latency) could miss the response start. The
/// spin is short: a GET_CONDITION command is ~170 steps ≈ 85µs on the wire,
/// less than a quarter of the old ~390µs bit-bang.
///
/// Returns with the bus in output mode, both lines driven high (the end
/// pattern's final state) — identical to the bit-bang post-TX state; the
/// subsequent `wait_and_sample` switches to input mode as before.
pub fn write_packet_dma(bus: &mut MapleBus, packet: &MaplePacket) {
    let buf = super::gpio_bus::tx_waveform_buf();
    let mut w = WaveformBuilder::new(buf);

    // Driven idle before the start pattern (= the bit-bang stabilize delay).
    for _ in 0..4 {
        w.step();
    }
    w.start_pattern();

    let mut phase = true;
    let mut crc: u8 = 0;

    let frame = packet.frame_word();
    w.write_word(frame, &mut phase);
    update_crc(frame, &mut crc);

    for &word in &packet.payload {
        w.write_word(word, &mut phase);
        update_crc(word, &mut crc);
    }

    w.write_byte(crc, &mut phase);
    w.end_pattern();

    let halfwords = w.n;
    let buf_ptr = w.buf.as_ptr() as u32;

    start_playback(bus, buf_ptr, halfwords);

    // Busy-wait for sequence end. Bound: a full 32-word packet is ~2.2ms ≈
    // 140k cycles; 4M iterations (≥ 4M cycles) is far past any legal frame.
    let mut guard = 0u32;
    while pwm_read(EVENTS_SEQEND0) == 0 {
        guard += 1;
        if guard > 4_000_000 {
            pwm_write(TASKS_STOP, 1);
            break;
        }
    }

    finish_playback(bus);
}

/// Pin handoff GPIO → PWM and start the sequence. GPIO holds the driven
/// idle; PSEL is connected while the PWM is still disabled (pins stay under
/// GPIO control until ENABLE).
fn start_playback(bus: &mut MapleBus, buf_ptr: u32, halfwords: usize) {
    debug_assert!(halfwords <= 0x7FFF, "SEQ.CNT is 15 bits");

    bus.set_output_mode();
    bus.set_idle();

    pwm_write(REG_ENABLE, 0);
    pwm_write(PSEL_OUT0, crate::board::PIN_A_BIT); // SDCKA, P0
    pwm_write(PSEL_OUT1, PSEL_DISCONNECTED);
    pwm_write(PSEL_OUT2, crate::board::PIN_B_BIT); // SDCKB, P0
    pwm_write(PSEL_OUT3, PSEL_DISCONNECTED);

    pwm_write(REG_MODE, 0); // up counter
    pwm_write(REG_COUNTERTOP, u32::from(COUNTERTOP));
    pwm_write(REG_PRESCALER, 0); // 16MHz
    pwm_write(REG_DECODER, 1); // LOAD=Grouped, MODE=RefreshCount
    pwm_write(REG_LOOP, 0);
    pwm_write(SEQ0_PTR, buf_ptr);
    #[allow(clippy::cast_possible_truncation)]
    pwm_write(SEQ0_CNT, halfwords as u32);
    pwm_write(SEQ0_REFRESH, 0); // new sample every period
    pwm_write(SEQ0_ENDDELAY, 0);

    pwm_write(EVENTS_SEQEND0, 0);
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    pwm_write(REG_ENABLE, 1);
    pwm_write(TASKS_SEQSTART0, 1);
}

/// Pin handoff PWM → GPIO. Match the GPIO OUT levels to the waveform's final
/// state (end pattern leaves both lines high) before disconnecting, so the
/// handoff produces no edges. Leaves the bus in output mode, both high.
fn finish_playback(bus: &mut MapleBus) {
    bus.set_both_high();
    pwm_write(PSEL_OUT0, PSEL_DISCONNECTED);
    pwm_write(PSEL_OUT2, PSEL_DISCONNECTED);
    pwm_write(REG_ENABLE, 0);
}
