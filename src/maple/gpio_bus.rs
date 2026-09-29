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

#[cfg(feature = "spim-capture")]
use crate::maple::spim_capture::{SpimCapture, StreamBufs};
use crate::maple::MaplePacket;
#[cfg(feature = "spim-capture")]
use core::mem::MaybeUninit;
#[cfg(feature = "spim-capture")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{compiler_fence, Ordering};
use embassy_nrf::gpio::{Flex, Pull};
use heapless::Vec;
#[cfg(feature = "spim-capture")]
use maple_protocol::packed;
use maple_protocol::wire;

/// Number of u32 samples in the bulk capture buffer.
const SAMPLE_BUFFER_LEN: usize = 24576;

/// NOP iterations for a half-bit delay (used by the timeslot TX fallback).
pub const HALF_BIT_NOPS: u32 = 32;

/// Cycle-counter target for one bit-banged Maple half-bit (DWT CYCCNT @ 64MHz).
///
/// The old `HALF_BIT_NOPS = 32` loop works on the production XIAO, but its
/// actual duration is a codegen artifact: with fat LTO, unrelated code changes
/// can re-inline the TX path and move the wire timing. POLLPHASE data from that
/// working build puts a `GET_CONDITION` TX around 395us. That frame contains 163
/// calls to `delay_half_bit` (idle + start + frame + one payload word + CRC +
/// end), so the known-good XIAO center is about 155 CPU cycles per half-bit.
const HALF_BIT_CYCLES: u32 = 155;

/// Ignore normal busy-wait/read overshoot, but do not let a real interrupt make
/// the next half-bit short while the schedule tries to catch up.
const TX_DEADLINE_GRACE_CYCLES: u32 = 8;

/// CPU clock in MHz — DWT cycles / this = microseconds (nRF52840, 64 MHz).
const CPU_MHZ: u32 = 64;

/// Bytes per SPIM stream for one controller capture: eight samples a byte, so
/// this covers the same `SAMPLE_BUFFER_LEN` window the CPU loop filled.
#[cfg(feature = "spim-capture")]
#[expect(
    clippy::cast_possible_truncation,
    reason = "SAMPLE_BUFFER_LEN is a 24,576 literal; the buffer cannot reach 2^35 bytes on this part"
)]
const SPIM_MAXCNT_BYTES: u32 = (SAMPLE_BUFFER_LEN / 8) as u32;

/// Cycles to wait for both streams' `END` after the trigger fired: the
/// transfer itself — one sample per 8 MHz clock, so `SAMPLE_BUFFER_LEN / 8`
/// microseconds, 3.072 ms — plus a millisecond. Nothing after the trigger is
/// the responder's to be late about, so this is a hang guard, not a budget:
/// blowing it means the DMA stopped, which is the `INCOMPLETE` counter.
#[cfg(feature = "spim-capture")]
#[expect(
    clippy::cast_possible_truncation,
    reason = "as for SPIM_MAXCNT_BYTES: a compile-time constant far inside u32"
)]
const SPIM_END_BUDGET_CYCLES: u32 = ((SAMPLE_BUFFER_LEN / 8) as u32 + 1_000) * CPU_MHZ;

/// Bytes per poll stream, as a length: `SPIM_MAXCNT_BYTES`.
#[cfg(feature = "spim-capture")]
const POLL_STREAM_BYTES: usize = SAMPLE_BUFFER_LEN / 8;

/// The controller poll's own two bit streams.
///
/// The capture owns no buffers: one `SpimCapture` serves the poll and the
/// diag read path, and a capture one of them has latched must survive the
/// other's next `arm` (`spim_capture::SpimCapture`, bench run #175). So the
/// poll brings its own pair, armed and read only here. `.uninit`, which
/// cortex-m-rt places after `.bss` and does not zero at boot, so no existing
/// RAM symbol moves and EasyDMA rather than the reset handler fills them.
#[cfg(feature = "spim-capture")]
#[link_section = ".uninit.poll_streams"]
static mut POLL_STREAM_A: MaybeUninit<[u8; POLL_STREAM_BYTES]> = MaybeUninit::uninit();
#[cfg(feature = "spim-capture")]
#[link_section = ".uninit.poll_streams"]
static mut POLL_STREAM_B: MaybeUninit<[u8; POLL_STREAM_BYTES]> = MaybeUninit::uninit();

/// The poll's stream buffers, for [`MapleBus::capture_on_spim`]'s `arm`.
#[cfg(feature = "spim-capture")]
fn poll_stream_bufs() -> StreamBufs {
    // The statics are named by address only; a raw borrow of a `static mut`
    // is safe and creates no reference to either.
    let a = core::ptr::addr_of_mut!(POLL_STREAM_A).cast::<u8>();
    let b = core::ptr::addr_of_mut!(POLL_STREAM_B).cast::<u8>();
    // SAFETY: `POLL_STREAM_A` and `POLL_STREAM_B` are two distinct statics of
    // exactly `POLL_STREAM_BYTES` bytes each, in RAM as EasyDMA requires, and
    // they live for the program. `capture_on_spim` is their only user: it is
    // the only caller of this function, it runs on the main task of a
    // single-core part, and within one call it arms, waits, finishes and
    // unpacks before returning — so nothing reads or writes either region
    // between the `arm` and its `finish`/`abort`, and no capture of anyone
    // else's is still holding them.
    unsafe { StreamBufs::new(a, b, SPIM_MAXCNT_BYTES) }
}

/// Controller polls whose reply never triggered the capture.
///
/// `EVENTS_STARTED` was still clear `timeout_us` after the bus went neutral,
/// so the capture was aborted and the poll failed. The bench gate for the
/// hardware capture is that this stays ≈ 0 over ≥ 10,000 polls across
/// several controllers — the
/// trigger has to beat every pad's turnaround, not just the VMU's ≤ 260 µs.
/// Read out by a diagnostic build; nothing in production consumes it.
#[cfg(feature = "spim-capture")]
pub static NO_TRIGGER: AtomicU32 = AtomicU32::new(0);

/// Controller polls that triggered but did not deliver both whole streams.
///
/// An `END` missing at the deadline, or `RXD.AMOUNT` short of what was armed.
/// Counterpart to [`NO_TRIGGER`]; a diagnostic saw 0 of these in 2,333 read
/// captures, so anything but ≈ 0 here is a finding, not noise.
#[cfg(feature = "spim-capture")]
pub static INCOMPLETE: AtomicU32 = AtomicU32::new(0);

/// NOP iterations for pin stabilization after output mode set.
pub const PIN_STABILIZE_NOPS: u32 = 100;

/// Payload word count of a VMU LCD `BLOCK_WRITE` packet: one LCD function
/// word + one block/phase word + 48 words of pixel data. Lands in the low
/// byte of the frame word.
pub const LCD_PAYLOAD_WORDS: u32 = 50;

/// Payload word count of one storage `BLOCK_WRITE` phase: the function word,
/// the location word and 32 words — 128 bytes — of block data. Lands in the
/// low byte of the frame word.
pub const BLOCK_PHASE_WORDS: u32 = 34;
const _: () =
    assert!(BLOCK_PHASE_WORDS as usize == 2 + maple_protocol::block_bytes::PHASE_BYTES / 4);

/// Maple command: `BLOCK_WRITE`, the LCD's and the storage function's.
const BLOCK_WRITE: u8 = 0x0C;

/// NOP iterations for pull-up stabilization after input mode set.
const PULLUP_STABILIZE_NOPS: u32 = 200;

/// Number of start pattern B-line toggles.
const START_TOGGLE_COUNT: u32 = maple_protocol::wire::DATA_START_TOGGLES;

/// Minimum bits required to attempt packet decode.
const MIN_DECODE_BITS: usize = 32;

/// Capacity of the decoded-bit buffer.
///
/// **Not a tuning knob — a correctness bound.** `bits.push` is `let _ =`, so
/// once this fills every later bit is silently discarded: the frame's tail
/// vanishes, `byte_count` drops below the header's own length field, and the
/// packet is rejected as "incomplete" with nothing indicating a buffer ran out.
///
/// The previous value of 960 (120 bytes) sat 3 bytes above the 117 a 28-word
/// `DEVICE_INFO` response needs (`4 + 28*4 + 1`). That margin is now load-bearing:
/// VMU presence is read from the controller's device-info reply every 3s
/// ([`crate::maple::host::MapleHost::sub_peripheral_mask`]), so this path takes
/// a full-length response continuously rather than only at controller detect.
///
/// 2048 bits = 256 bytes, exactly the `bytes` array the decoder unpacks into —
/// it cannot be raised further without growing that too.
const MAX_DECODE_BITS: usize = 2048;

/// Minimum bytes required for a valid frame header.
const MIN_FRAME_BYTES: usize = 4;

/// Static buffer for bulk sampling (96KB, 37% of RAM). Pre-allocated to avoid runtime delay.
///
/// This size is intentional: the entire controller response must be captured in one
/// uninterrupted burst at ~12.5MHz. On-the-fly processing would miss edges. The buffer
/// includes headroom for the idle/wait period before the response starts.
///
/// # Safety
/// Accessed only from the capture stage (`wait_and_sample()`, or
/// `capture_on_spim()`'s unpack where the hardware capture is the sampler)
/// and `receive_frame()`, which run sequentially on a single-core Cortex-M4.
/// No concurrent or overlapping references are possible.
/// Shared bulk sample / TX waveform buffer.
///
/// Used by RX for bulk GPIO sampling and by TX (timeslot) for pre-computed
/// waveforms. TX and RX never overlap, so sharing is safe.
pub(crate) static mut SAMPLE_BUFFER: [u32; SAMPLE_BUFFER_LEN] = [0; SAMPLE_BUFFER_LEN];

/// View of `SAMPLE_BUFFER` as a PWM waveform halfword buffer for DMA TX.
///
/// SAFETY: single-core, and TX waveform building/playback never overlaps RX
/// bulk sampling — the same exclusivity argument as `wait_and_sample`. The
/// buffer is in RAM, as EasyDMA requires.
pub(crate) fn tx_waveform_buf() -> &'static mut [u16] {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "taking the address and building the slice are one derivation, \
                  justified by the single exclusivity argument above"
    )]
    // SAFETY: as the doc comment above states — single-core, and TX waveform
    // building/playback never overlaps RX bulk sampling, so no second live
    // reference to `SAMPLE_BUFFER` can exist. `addr_of_mut!` avoids creating an
    // intermediate reference to the static. Reinterpreting as `u16` is sound:
    // it has weaker alignment than `u32`, and the doubled length covers exactly
    // the same bytes. The buffer is in RAM, as EasyDMA requires.
    unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(SAMPLE_BUFFER).cast::<u16>(),
            SAMPLE_BUFFER_LEN * 2,
        )
    }
}

const PIN_A_MASK: u32 = 1 << crate::board::PIN_A_BIT;
const PIN_B_MASK: u32 = 1 << crate::board::PIN_B_BIT;

/// P0 GPIO base address for direct register access.
const P0_BASE: u32 = 0x5000_0000;
/// Offset to IN register within GPIO peripheral.
const GPIO_IN_OFFSET: u32 = 0x510;

/// Read P0 IN register directly.
#[inline]
fn read_p0_in() -> u32 {
    // SAFETY: MMIO register access requires an integer-to-pointer cast. P0's IN
    // register sits at a fixed, word-aligned address defined by the nRF52840
    // memory map, so the pointer is always valid. IN is read-only and
    // side-effect free, so a volatile read cannot disturb the peripheral or
    // race anything.
    unsafe { core::ptr::read_volatile((P0_BASE + GPIO_IN_OFFSET) as *const u32) }
}

/// Read the DWT cycle counter. Enabled once in `MapleBus::new`.
#[expect(
    clippy::inline_always,
    reason = "the half-bit schedule is measured in CPU cycles, so a call/return in this path is a timing error, not \
              an optimiser preference (see HALF_BIT_CYCLES)"
)]
#[inline(always)]
fn cyccnt() -> u32 {
    // SAFETY: CYCCNT is a free-running read-only counter; reading is always safe.
    unsafe { (*cortex_m::peripheral::DWT::PTR).cyccnt.read() }
}

/// Absolute cycle deadline for the next bit-banged half-bit edge.
///
/// Accessed only from the single poll-task TX path (`write_packet`/`write_lcd`
/// and their pattern/bit helpers). TX never overlaps RX or re-enters, so this
/// has the same single-context invariant as `SAMPLE_BUFFER`.
static mut TX_DEADLINE: u32 = 0;

/// Reset the half-bit schedule to "now" at the start of each TX frame.
#[expect(
    clippy::inline_always,
    reason = "the half-bit schedule is measured in CPU cycles, so a call/return in this path is a timing error, not \
              an optimiser preference (see HALF_BIT_CYCLES)"
)]
#[inline(always)]
fn tx_timing_reset() {
    // SAFETY: single TX context (see `TX_DEADLINE`).
    unsafe { TX_DEADLINE = cyccnt() };
}

/// One Maple half-bit, anchored to the DWT cycle counter.
///
/// The period is the measured XIAO-good center, not the compiled duration of a
/// Rust `for` loop. If the wait is late by more than normal read/branch
/// overshoot, resync to the delivered edge so we stretch one half-bit rather
/// than compressing the next one.
#[expect(
    clippy::inline_always,
    reason = "the half-bit schedule is measured in CPU cycles, so a call/return in this path is a timing error, not \
              an optimiser preference (see HALF_BIT_CYCLES)"
)]
#[inline(always)]
#[expect(
    clippy::cast_possible_wrap,
    reason = "the wrap is the point: the signed difference of two wrapping cycle counters is how the deadline comparison stays correct across CYCCNT rollover"
)]
fn delay_half_bit() {
    compiler_fence(Ordering::SeqCst);
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "read-modify-write of TX_DEADLINE is one update; splitting it would \
                  imply the halves can be reasoned about independently"
    )]
    // SAFETY: single TX context (see `TX_DEADLINE`). Only one transmit runs at a
    // time and TX is never re-entered from an interrupt, so this
    // read-modify-write of the static cannot race.
    let deadline = unsafe {
        let d = TX_DEADLINE.wrapping_add(HALF_BIT_CYCLES);
        TX_DEADLINE = d;
        d
    };

    while (cyccnt().wrapping_sub(deadline) as i32) < 0 {}

    let late = cyccnt().wrapping_sub(deadline);
    if late > TX_DEADLINE_GRACE_CYCLES {
        // SAFETY: single TX context.
        unsafe { TX_DEADLINE = deadline.wrapping_add(late) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// GPIO-based Maple Bus driver.
///
/// Uses Embassy Flex pins for dynamic input/output switching.
pub struct MapleBus {
    sdcka: Flex<'static>,
    sdckb: Flex<'static>,
    /// The hardware capture that samples every reply on these builds, once
    /// `main` has handed over the board's parts ([`attach_capture`]). `None`
    /// until then, and `None` for good on a board whose `spim_capture` parts
    /// are `None` — see [`read_packet_bulk`].
    ///
    /// [`attach_capture`]: MapleBus::attach_capture
    /// [`read_packet_bulk`]: MapleBus::read_packet_bulk
    #[cfg(feature = "spim-capture")]
    capture: Option<SpimCapture>,
}

impl MapleBus {
    /// Create a new Maple Bus GPIO driver.
    ///
    /// Initializes pins in idle state (SDCKA high, SDCKB low).
    #[must_use]
    #[expect(clippy::similar_names, reason = "sdcka/sdckb are protocol names")]
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

        // Enable the DWT cycle counter for cycle-anchored bit-bang TX timing
        // (`delay_half_bit`). Idempotent one-time enable; harmless if another
        // diagnostic feature has already enabled it.
        // SAFETY: DWT/DCB are core debug peripherals not owned elsewhere at
        // run time; this only sets the CYCCNT-enable bits.
        unsafe {
            let mut core = cortex_m::Peripherals::steal();
            core.DCB.enable_trace();
            core.DWT.enable_cycle_counter();
        }

        Self {
            sdcka,
            sdckb,
            #[cfg(feature = "spim-capture")]
            capture: None,
        }
    }

    /// Hand the bus the board's hardware capture; it samples every reply from
    /// here on.
    ///
    /// Called once, from `main`, before the first poll. The capture and the
    /// bit-bang TX share the two Maple pins but never the wire: the SPIM only
    /// listens, and it is enabled only between [`set_input_mode`] and the end
    /// of the reply.
    ///
    /// [`set_input_mode`]: MapleBus::set_input_mode
    #[cfg(feature = "spim-capture")]
    pub fn attach_capture(&mut self, cap: SpimCapture) {
        self.capture = Some(cap);
    }

    /// The bus's capture, for a consumer that drives it itself.
    ///
    /// There is one `SpimCapture` in the program because there is one set of
    /// board parts, and the bus owns it. `maple::block_read` arms and waits on
    /// the same instance for its `BLOCK_READ` window: the poll and a read never
    /// overlap — one poll task, and a read window *replaces* the controller
    /// poll in its window rather than running beside it — so a single instance
    /// serving both is sound. `arm` sets the transfer length each time, and
    /// each consumer brings its own stream buffers, so a poll between a read's
    /// `finish` and its unpack disturbs neither.
    #[cfg(feature = "spim-capture")]
    pub const fn capture_mut(&mut self) -> Option<&mut SpimCapture> {
        self.capture.as_mut()
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
        self.set_input_mode();

        // Quick sample: 1000 reads
        let mut _a_low_count: u32 = 0;
        let mut _b_low_count: u32 = 0;
        let mut _transitions: u32 = 0;
        let mut last = read_p0_in();

        for _ in 0..1000 {
            let val = read_p0_in();
            if val & PIN_A_MASK == 0 {
                _a_low_count += 1;
            }
            if val & PIN_B_MASK == 0 {
                _b_low_count += 1;
            }
            if (val ^ last) & (PIN_A_MASK | PIN_B_MASK) != 0 {
                _transitions += 1;
            }
            last = val;
        }

        let _final_val = read_p0_in();
        // Computed inside `log!` rather than bound first: `log!` compiles to
        // nothing without `rtt`, so bindings here would be dead in production.
        log!(
            "DIAG: A_low={}/1000 B_low={}/1000 trans={} final A={} B={}",
            _a_low_count,
            _b_low_count,
            _transitions,
            u8::from((_final_val & PIN_A_MASK) != 0),
            u8::from((_final_val & PIN_B_MASK) != 0)
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

    /// Drive both lines high — the end-pattern's final state. Used to match
    /// GPIO OUT levels to the waveform tail for a glitch-free PWM→GPIO
    /// handoff after a DMA TX (`pwm_tx`).
    #[inline]
    pub fn set_both_high(&mut self) {
        self.sdcka.set_high();
        self.sdckb.set_high();
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
        tx_timing_reset();
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

    /// Write a VMU LCD frame via direct bit-bang (no timeslot).
    pub fn write_lcd(&mut self, sender: u8, dest: u8, framebuffer: &[u8; 192]) {
        self.set_output_mode();
        self.set_idle();
        tx_timing_reset();
        delay_half_bit();
        let mut phase = true;
        self.send_start_pattern();

        let mut crc: u8 = 0;

        let frame: u32 = (u32::from(BLOCK_WRITE) << 24)
            | (u32::from(dest) << 16)
            | (u32::from(sender) << 8)
            | LCD_PAYLOAD_WORDS;
        self.write_word(frame, &mut phase);
        Self::update_crc(frame, &mut crc);

        let func: u32 = 0x0000_0004;
        self.write_word(func, &mut phase);
        Self::update_crc(func, &mut crc);

        let loc: u32 = 0x0000_0000;
        self.write_word(loc, &mut phase);
        Self::update_crc(loc, &mut crc);

        for chunk in framebuffer.chunks_exact(4) {
            let word = u32::from_le_bytes([chunk[3], chunk[2], chunk[1], chunk[0]]);
            self.write_word(word, &mut phase);
            Self::update_crc(word, &mut crc);
        }

        self.write_byte(crc, &mut phase);
        self.send_end_pattern();
    }

    /// Write one 128-byte phase of a storage `BLOCK_WRITE` to the VMU: the
    /// LCD frame's shape on function `0x02`, with `location` carrying the
    /// phase and the block (`phase << 16 | block`). Bit-banged like
    /// [`Self::write_lcd`], on the cycle-anchored half-bit (ADR-010), in the
    /// quiet window the caller has aligned to — 141 bytes ≈ 5.5 ms on the
    /// wire, so the ACK capture that follows still fits the window.
    ///
    /// `data` is in **image order**, as the link carries it; each word goes
    /// out as `u32::from_be_bytes` of its four bytes, which is the LCD path's
    /// "pixel byte-swap" and `block_bytes::image_to_wire` in one step — the
    /// wire sends a word least-significant byte first, and the card's
    /// filesystem is laid out in the reverse of that.
    pub fn write_block_phase(
        &mut self,
        sender: u8,
        dest: u8,
        location: u32,
        data: &[u8; maple_protocol::block_bytes::PHASE_BYTES],
    ) {
        self.set_output_mode();
        self.set_idle();
        tx_timing_reset();
        delay_half_bit();
        let mut phase = true;
        self.send_start_pattern();

        let mut crc: u8 = 0;

        let frame: u32 = (u32::from(BLOCK_WRITE) << 24)
            | (u32::from(dest) << 16)
            | (u32::from(sender) << 8)
            | BLOCK_PHASE_WORDS;
        self.write_word(frame, &mut phase);
        Self::update_crc(frame, &mut crc);

        let func: u32 = super::host::functions::STORAGE;
        self.write_word(func, &mut phase);
        Self::update_crc(func, &mut crc);

        self.write_word(location, &mut phase);
        Self::update_crc(location, &mut crc);

        for chunk in data.chunks_exact(4) {
            let word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
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

    /// Wait for a peripheral response and bulk sample it.
    ///
    /// Capture is triggered by the **first SDCKA falling edge** after the bus
    /// goes neutral — the opening edge of the start pattern itself — so the
    /// pattern lands inside the buffer and is validated against the spec by
    /// [`maple_protocol::wire::find_data_start`] rather than counted here at
    /// wire speed.
    ///
    /// The previous version counted SDCKB transitions in this loop
    /// (`b_transitions >= 3`) and started the capture *after* the pattern. That
    /// loop reads P0 every ~100ns against 250ns peripheral phases, so it can
    /// undercount toggles, fall through into the data phase, and trigger on an
    /// arbitrary edge — which left the decoder guessing alignment from a sample
    /// index. Moving the check into software costs nothing on the wire: the asm
    /// loop already fills the whole buffer on every call (~3.1ms), so triggering
    /// ~4us earlier is free, and it buys a real conformance check — a non-data
    /// pattern (8 toggles = light gun, 14 = reset) is now rejected instead of
    /// clearing a `>= 3` threshold.
    ///
    /// # Timeouts are wall-clock (DWT), not iteration counts (2026-08-05)
    ///
    /// Both wait loops here used to time out on ITERATION counts
    /// (`timeout_cycles = 64_000`, believed to be ~1ms but actually ~8-12ms
    /// of compiled loop time) — the last layout-dependent duration in the
    /// firmware, and the actual variable behind "pattern B" of the layout
    /// lottery: an embassy RTC alarm ISR landing inside the ~395µs bit-bang
    /// TX (~3.3/s, Poisson) stretches one half-bit, the controller rejects
    /// the frame and stays silent, and the silent-bus retry burst then
    /// costs 3 × the compiled timeout — 20-25ms on a lucky layout (rare
    /// 30ms delivery gaps) vs 36ms+ on an unlucky one (the 45-60ms
    /// quantized stalls, 18-23% doubled intervals, runs #48/#50/#52/#53).
    /// Exact-binary A/B with the VMU removed pinned it to this path.
    /// DWT deadlines make the timeout a designed constant on every layout.
    /// (DWT is valid here: this is blocking code, the core never sleeps.)
    ///
    /// Returns `(success, waited_us, sample_count)`.
    ///
    /// # The CPU sampler is not built where the hardware capture is
    ///
    /// With `spim-capture` this function, and the pinned loop with it, is not
    /// compiled at all: the carrier and the XIAO capture every reply on the
    /// SPIM pair by EasyDMA ([`capture_on_spim`]), so the sample rate stops
    /// being a property of where the linker put a loop.
    /// The DK keeps this path. There is deliberately no fallback in either
    /// direction — a build has one sampler, and `check_timing_invariants.sh`
    /// checks for the right one.
    ///
    /// [`capture_on_spim`]: MapleBus::capture_on_spim
    #[cfg(not(feature = "spim-capture"))]
    pub fn wait_and_sample(&mut self, timeout_us: u32) -> (bool, u32, usize) {
        self.set_input_mode();

        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "the reborrow and the never-None unwrap are one derivation of the \
                      buffer reference, sound for the same reason"
        )]
        // SAFETY: single-core, and interrupts are disabled for the duration of
        // sampling, so only one mutable reference to `SAMPLE_BUFFER` is live at
        // a time — no concurrent access is possible. `addr_of_mut!` on a static
        // always yields a non-null, well-aligned, initialised pointer, so
        // `as_mut()` cannot return `None` and `unwrap_unchecked` is discharged.
        let samples = unsafe {
            core::ptr::addr_of_mut!(SAMPLE_BUFFER)
                .as_mut()
                .unwrap_unchecked()
        };

        // Wait for idle (both HIGH)
        let t0 = cyccnt();
        let idle_budget = (timeout_us / 2).saturating_mul(CPU_MHZ);
        loop {
            let val = read_p0_in();
            if (val & PIN_A_MASK) != 0 && (val & PIN_B_MASK) != 0 {
                break;
            }
            if cyccnt().wrapping_sub(t0) > idle_budget {
                return (false, cyccnt().wrapping_sub(t0) / CPU_MHZ, 0);
            }
        }

        // Wait for SDCKA LOW — the opening edge of the responder's start
        // pattern — then capture immediately. This loop is the fast path for a
        // silent bus: with no peripheral answering it times out here instead of
        // spending a full ~3.1ms capture on nothing.
        let start_budget = timeout_us.saturating_mul(CPU_MHZ);
        let t1 = cyccnt();
        loop {
            if (read_p0_in() & PIN_A_MASK) == 0 {
                // Start pattern detected - sample immediately.
                //
                // The sample loop is inline asm, not `for s in samples`: the
                // compiled loop's cycle count IS the sample rate, and every
                // decode threshold (GAP_THRESHOLD, end-of-response quiet run)
                // is calibrated in sample counts at ~7.9 Msamples/s. Under
                // fat LTO any unrelated code change can move a Rust loop to
                // a non-word-aligned address, which costs +1 fetch cycle per
                // iteration on Cortex-M4 (-12.6% sample rate, doubled read
                // retries — debug log 2026-06-11). Registers are pinned so
                // the encoding is byte-identical (14 bytes) in every build,
                // and `.p2align 2` pins the branch target word-aligned.
                compiler_fence(Ordering::SeqCst);
                // SAFETY: the asm only reads P0 IN through r12 and writes words
                // into the `SAMPLE_BUFFER` region addressed by r2, bounded by
                // the `end` operand — it stays inside the buffer proved
                // exclusive above and touches no other memory. All registers it
                // clobbers are declared in the operand list, and it neither
                // calls nor branches outside its own loop label. Do not edit the
                // body: the exact 5-instruction encoding and the `.p2align 2`
                // branch target are load-bearing (see the comment above and
                // scripts/check_timing_invariants.sh).
                unsafe {
                    core::arch::asm!(
                        ".p2align 2",
                        "2:",
                        "ldr.w r0, [r12]",      // sample P0 IN
                        "str r0, [r2, r1]",
                        "adds r1, #4",
                        "cmp.w r1, {end}",
                        "bne 2b",
                        end = const SAMPLE_BUFFER_LEN * 4,
                        in("r12") P0_BASE + GPIO_IN_OFFSET,
                        in("r2") samples.as_mut_ptr(),
                        inout("r1") 0u32 => _,
                        out("r0") _,
                        options(nostack),
                    );
                }
                compiler_fence(Ordering::SeqCst);
                return (true, cyccnt().wrapping_sub(t0) / CPU_MHZ, SAMPLE_BUFFER_LEN);
            }

            if cyccnt().wrapping_sub(t1) > start_budget {
                return (false, cyccnt().wrapping_sub(t0) / CPU_MHZ, 0);
            }
        }
    }

    /// Capture a reply on the SPIM pair and unpack it into `SAMPLE_BUFFER`.
    ///
    /// The hardware half of [`wait_and_sample`](Self::wait_and_sample), and
    /// its replacement on the boards that have the capture: two SPIM
    /// instances sample SDCKA and SDCKB into RAM at 8 MHz by EasyDMA, both
    /// started in the same cycle by the reply's own first SDCKA fall through
    /// GPIOTE → PPI, with no CPU in the loop (`maple::spim_capture`). The
    /// unpack then writes exactly the word-per-sample buffer the CPU loop
    /// wrote, so `find_data_start`, `decode_bulk_samples` and the frame parse
    /// below are the same code reading the same shape.
    ///
    /// The order is the one measured over 2,333 read captures:
    /// input mode, **then** arm — the trigger is one-shot and armed only
    /// after the command's TX, or the command's own SDCKA falls would fire
    /// it — then the idle wait, then the two spins. Every wait is a plain
    /// spin on an event register against a DWT deadline: no PRIMASK, no
    /// critical section, nothing that could hold off the SoftDevice, and
    /// nothing whose duration depends on codegen.
    ///
    /// The CPU's 3.07 ms of spinning becomes the DMA's, and the unpack — the
    /// price of feeding a word-per-sample decoder from packed bits — lands
    /// inside the caller's `poll-timing` *read* span, not its decode span, so
    /// a bench run compares against the CPU capture's read span directly.
    ///
    /// Returns `(success, sample_count)`.
    #[cfg(feature = "spim-capture")]
    fn capture_on_spim(&mut self, timeout_us: u32) -> (bool, usize) {
        self.set_input_mode();

        let Some(capture) = self.capture.as_mut() else {
            // No capture on this board, and no CPU sampler in this build to
            // fall back to: the pinned loop is compiled out wherever
            // `spim-capture` is on. The board contract makes this
            // unreachable — the two boards that enable the feature both hand
            // over their `Parts` — so failing the poll is the honest answer
            // rather than a second sampler kept alive for a case that cannot
            // happen.
            return (false, 0);
        };
        capture.arm(poll_stream_bufs());

        // Wait for the bus to go neutral (both HIGH), on the CPU path's own
        // budget and deadline: half the timeout, DWT cycles, never an
        // iteration count (2026-08-05, see `wait_and_sample`).
        let t0 = cyccnt();
        let idle_budget = (timeout_us / 2).saturating_mul(CPU_MHZ);
        loop {
            let val = read_p0_in();
            if (val & PIN_A_MASK) != 0 && (val & PIN_B_MASK) != 0 {
                break;
            }
            if cyccnt().wrapping_sub(t0) > idle_budget {
                capture.abort();
                return (false, 0);
            }
        }

        // The reply's first SDCKA fall starts both transfers in hardware; all
        // the CPU does is watch one `EVENTS_STARTED`. A silent bus times out
        // here, as it did in the CPU path, and costs no transfer.
        let start_budget = timeout_us.saturating_mul(CPU_MHZ);
        let t1 = cyccnt();
        while !capture.started() {
            if cyccnt().wrapping_sub(t1) > start_budget {
                capture.abort();
                NO_TRIGGER.fetch_add(1, Ordering::Relaxed);
                return (false, 0);
            }
        }

        // Both `END`s, within the transfer's own length plus a millisecond.
        let t2 = cyccnt();
        while !capture.ended() {
            if cyccnt().wrapping_sub(t2) > SPIM_END_BUDGET_CYCLES {
                capture.abort();
                INCOMPLETE.fetch_add(1, Ordering::Relaxed);
                return (false, 0);
            }
        }

        // Both ends are already in, so `finish` only checks `RXD.AMOUNT`,
        // disarms and fences — a zero budget, per its contract. It hands
        // back the capture itself; a short stream leaves `captured` `None`
        // and nothing about it in the instance.
        let Some(cap) = capture.finish(0).captured else {
            INCOMPLETE.fetch_add(1, Ordering::Relaxed);
            return (false, 0);
        };
        let (stream_a, stream_b) = cap.streams();

        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "the reborrow and the never-None unwrap are one derivation of the \
                      buffer reference, sound for the same reason"
        )]
        // SAFETY: single-core, and this is the only reference to
        // `SAMPLE_BUFFER` that exists while the unpack runs — the decode
        // below takes its shared reference only after this one is dropped,
        // TX's view of the same buffer never overlaps RX, and the two stream
        // slices are the capture's statics, not this one. `addr_of_mut!` on a
        // static always yields a non-null, well-aligned, initialised pointer,
        // so `as_mut()` cannot return `None` and `unwrap_unchecked` is
        // discharged.
        let samples = unsafe {
            core::ptr::addr_of_mut!(SAMPLE_BUFFER)
                .as_mut()
                .unwrap_unchecked()
        };
        let count = packed::unpack_words_into(
            stream_a,
            stream_b,
            samples,
            crate::board::PIN_A_BIT,
            crate::board::PIN_B_BIT,
        );
        (true, count)
    }

    /// Decode bits from bulk samples.
    #[must_use]
    pub fn decode_bulk_samples(
        &self,
        samples: &[u32],
        count: usize,
        start_sample: usize,
    ) -> (Vec<u8, MAX_DECODE_BITS>, u32, u32, u32) {
        const GAP_THRESHOLD: usize = 50;
        // A sustained run with no edge on either line, longer than any
        // inter-chunk gap (~900 samples at the measured ~7 M samples/s), marks
        // the end of the response. Stop decoding there instead of scanning the
        // rest of the 24k-sample buffer — that trailing scan was the dominant
        // cost of get_condition (issue #5, POLLPHASE `dec` ≈ 10 ms of ~14).
        //
        // Quiet is detected by *absence of edges*, not by line state: after the
        // response the bus floats with BOTH lines pulled high, so the driven
        // idle state (A high, B low) that `GAP_THRESHOLD` matches never occurs
        // and an A-high/B-low check would never fire (the first version of this
        // cut made exactly that mistake and scanned the full buffer anyway).
        const END_IDLE_THRESHOLD: usize = 3000;

        let mut bits: Vec<u8, MAX_DECODE_BITS> = Vec::new();
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
        let mut quiet_count: usize = 0;
        let mut seen_first_a_fall = false;

        for &sample in &samples[start_idx..count] {
            let a = (sample & PIN_A_MASK) != 0;
            let b = (sample & PIN_B_MASK) != 0;

            // End of response: once some bits are decoded, a long edge-free run
            // (beyond any inter-chunk gap) means the packet is done. Stop rather
            // than scan trailing samples. A too-early cut just fails the
            // frame-length/CRC check below and triggers a retry — it can never
            // produce a corrupt packet, which is what makes this safe to tune.
            if a == last_a && b == last_b {
                quiet_count += 1;
                if a_falls > 0 && quiet_count > END_IDLE_THRESHOLD {
                    break;
                }
            } else {
                quiet_count = 0;
            }

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

    /// Read a complete response packet using bulk sampling.
    #[expect(
        clippy::option_if_let_else,
        reason = "the if/let reads as the decode fallback it is; map_or_else would bury both branches in closures"
    )]
    pub fn read_packet_bulk(&mut self, timeout_us: u32) -> Option<MaplePacket> {
        // poll-timing spans are taken here, around whole calls — never inside
        // the capture stage, whose wait/capture loops are timing-critical.
        #[cfg(feature = "poll-timing")]
        let _pt_read = crate::poll_timing::start();
        // The capture stage is the board's: the CPU sampling loop where there
        // is no hardware capture (the DK), the SPIM pair by EasyDMA where
        // there is. Everything below — the start-pattern
        // search, the decode and the frame parse — reads the same
        // `SAMPLE_BUFFER` words either way, which is why all six `host.rs`
        // callers move through this one entry.
        #[cfg(not(feature = "spim-capture"))]
        let (success, _waited_us, count) = self.wait_and_sample(timeout_us);
        #[cfg(feature = "spim-capture")]
        let (success, count) = self.capture_on_spim(timeout_us);
        #[cfg(feature = "poll-timing")]
        crate::poll_timing::record_read(_pt_read);

        if !success {
            // Bus never went neutral, or nothing answered before the timeout.
            // Whether what did answer was a *conforming* start pattern is
            // decided below, against the captured samples.
            return None;
        }

        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "the reborrow and the never-None unwrap are one derivation of the \
                      buffer reference, sound for the same reason"
        )]
        // SAFETY: single-core, and the mutable reference the capture stage
        // held has already been dropped, so this shared reference
        // to `SAMPLE_BUFFER` cannot alias a live `&mut`. `addr_of!` on a static
        // always yields a non-null, well-aligned, initialised pointer, so
        // `as_ref()` cannot return `None` and `unwrap_unchecked` is discharged.
        let samples = unsafe {
            core::ptr::addr_of!(SAMPLE_BUFFER)
                .as_ref()
                .unwrap_unchecked()
        };

        #[cfg(feature = "poll-timing")]
        let _pt_dec = crate::poll_timing::start();
        // Validate the captured start pattern and take the decode offset from
        // it. This replaces a `first_edge_idx > 100` heuristic that inferred the
        // offset from *when* the first edge landed — a proxy for the responder's
        // reply latency, and so calibrated to the controller. The Maple spec puts
        // only a floor on reply time (a peripheral answers some time after 50us
        // from the bus going neutral), so a sub-peripheral answering with
        // different latency fell on the wrong side of the threshold and decoded
        // as noise. Alignment now comes from the wire, not from who is talking.
        //
        // A missing pattern means the capture was noise, or the responder sent a
        // non-data pattern. Fall through to the empty-bits check so the decode
        // span is still recorded on this path.
        let bits = match wire::find_data_start(&samples[..count], PIN_A_MASK, PIN_B_MASK) {
            Some(data_start) => self.decode_bulk_samples(samples, count, data_start).0,
            None => Vec::new(),
        };
        #[cfg(feature = "poll-timing")]
        crate::poll_timing::record_decode(_pt_dec);

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

        let command = ((frame >> 24) & 0xFF) as u8;
        let recipient = ((frame >> 16) & 0xFF) as u8;
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
