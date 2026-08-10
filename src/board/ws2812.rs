// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! WS2812B (NeoPixel) driver for the Pulsar V1 5-LED status bar on P0.28 (D2),
//! driven by PWM2 + EasyDMA.
//!
//! Each `write` encodes the pixels into a bit buffer, clocks the ~150 µs
//! waveform out via EasyDMA, waits for the sequence to end, and hands the pin
//! back to GPIO. It blocks for that ~150 µs — acceptable because writes only
//! happen on a status change (`set_battery` and friends no-op when nothing
//! changed), not per poll. Between writes the pin is a driven LOW, which is both
//! the idle state and the >50 µs WS2812 reset.
//!
//! **Timing verified on hardware** (2026-07-27, Pulsar v1): COUNTERTOP 20 @
//! 16 MHz = 1.25 µs, duty 6/13, bit-15 polarity. A full 5-LED frame renders the
//! correct color across the whole chain, which also confirms the GRB byte order.

use embassy_nrf::gpio::Pin;
use embassy_nrf::peripherals::PWM2;
use embassy_nrf::Peri;

/// LEDs in the status bar.
pub const LED_COUNT: usize = 5;
const BITS: usize = LED_COUNT * 24;

/// PWM2 register block (nRF52840).
///
/// **Not PWM0.** `maple::pwm_tx` drives the VMU LCD through PWM0 with raw
/// register writes and no `Peri` handle, so the type system cannot see the
/// clash — it asserts exclusive ownership in a comment that was true before this
/// board existed. Sharing it meant every LCD write repointed `PSEL_OUT0` at a
/// Maple line, after which WS2812 writes clocked ~150 µs of pixel waveform onto
/// the bus instead of the strip: the LEDs froze and Maple traffic took the hit.
const PWM2_BASE: usize = 0x4002_2000;
const PWM_TASKS_SEQSTART0: *mut u32 = (PWM2_BASE + 0x008) as *mut u32;
const PWM_ENABLE: *mut u32 = (PWM2_BASE + 0x500) as *mut u32;
const PWM_MODE: *mut u32 = (PWM2_BASE + 0x504) as *mut u32;
const PWM_COUNTERTOP: *mut u32 = (PWM2_BASE + 0x508) as *mut u32;
const PWM_PRESCALER: *mut u32 = (PWM2_BASE + 0x50C) as *mut u32;
const PWM_DECODER: *mut u32 = (PWM2_BASE + 0x510) as *mut u32;
const PWM_LOOP: *mut u32 = (PWM2_BASE + 0x514) as *mut u32;
const PWM_SEQ0_PTR: *mut u32 = (PWM2_BASE + 0x520) as *mut u32;
const PWM_SEQ0_CNT: *mut u32 = (PWM2_BASE + 0x524) as *mut u32;
const PWM_SEQ0_REFRESH: *mut u32 = (PWM2_BASE + 0x528) as *mut u32;
const PWM_SEQ0_ENDDELAY: *mut u32 = (PWM2_BASE + 0x52C) as *mut u32;
const PWM_PSEL_OUT0: *mut u32 = (PWM2_BASE + 0x560) as *mut u32;
const PWM_EVENTS_SEQEND0: *mut u32 = (PWM2_BASE + 0x110) as *mut u32;

/// `PSEL.OUT[n].CONNECT = Disconnected`.
const PSEL_DISCONNECTED: u32 = 0x8000_0000;

/// P0.28 (D2) — the `NEOPIXEL_DIN` net. ⚠ pulsarv1-specific.
///
/// Doubles as the PSEL value (port 0, pin 28, CONNECT=0) and the index into the
/// GPIO `PIN_CNF` array, so the two can't drift apart.
const NEOPIXEL_PIN: u32 = 28;
const NEOPIXEL_PSEL: u32 = NEOPIXEL_PIN;

/// GPIO P0 registers. Connecting `PSEL_OUT0` is *not* enough to drive a pad: the
/// PWM block drives its internal OUT signal, but a pin left at the reset value
/// (`PIN_CNF = 0x02`, DIR=Input) has its output buffer disabled, so the waveform
/// never reaches the wire. Every working PWM path on this board configures the
/// pin as an output first — `maple::pwm_tx` via `bus.set_output_mode()`, `Rumble`
/// via embassy's `SimplePwm`. This driver has to do it by hand.
const P0_OUTCLR: *mut u32 = 0x5000_050C as *mut u32;
const P0_PIN_CNF: *mut u32 = 0x5000_0700 as *mut u32;

/// `DIR=Output, INPUT=Disconnect, PULL=Disabled, DRIVE=S0S1, SENSE=Disabled` —
/// the same configuration embassy applies to a PWM output pin.
const PIN_CNF_OUTPUT: u32 = 0x0000_0003;

// 1.25 µs bit period at 16 MHz. Duty encodes the WS2812 0/1 high-time; bit15 is
// the polarity so the pulse is HIGH-at-start-of-period.
//
// ✅ Confirmed 2026-07-27 without a scope. embassy-nrf's `DutyCycle` documents
// this exact hardware bit: `inverted` (bit15 set) means "output is set high if
// the counter is **below** the duty value" — i.e. HIGH from the start of the
// period for `value` ticks. At 16 MHz that gives T0H = 6 ticks = 375 ns and
// T1H = 13 ticks = 812.5 ns over a 20-tick (1.25 µs) period, matching the
// comments below.
//
// Independently corroborated by the strip simply working: WS2812 decodes purely
// on high-time, so the opposite polarity would invert every bit *and* idle the
// line high (destroying the reset latch). A correct 5-LED bar with per-LED
// addressing is not reachable with the polarity wrong.
const COUNTERTOP: u32 = 20;
const T0H: u16 = 0x8000 | 6; // ~0.375 µs high
const T1H: u16 = 0x8000 | 13; // ~0.81 µs high

/// A pixel color. WS2812 wire order is GRB; conversion happens in `encode`.
#[derive(Clone, Copy)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const OFF: Rgb = Rgb { r: 0, g: 0, b: 0 };

    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// WS2812 status bar. Owns PWM2 + the LED pin and the DMA bit buffer.
pub struct Ws2812 {
    buf: [u16; BITS],
    // Held to keep the peripherals reserved for this driver's lifetime.
    _pwm: Peri<'static, PWM2>,
}

impl Ws2812 {
    /// Configure PWM2 for WS2812 on the given pin (must be P0.28 on this board).
    pub fn new(pwm: Peri<'static, PWM2>, _pin: Peri<'static, impl Pin>) -> Self {
        // SAFETY: one-time configuration of the PWM2 peripheral and the LED pin,
        // both of which we now own.
        unsafe {
            // Drive the pin LOW *before* enabling the output buffer, so the line
            // comes up in the WS2812 idle/reset state with no start-up glitch.
            // With the PWM disabled between sequences the pad falls back to this
            // GPIO level, which is what makes the >50 µs inter-write reset real.
            core::ptr::write_volatile(P0_OUTCLR, 1 << NEOPIXEL_PIN);
            core::ptr::write_volatile(P0_PIN_CNF.add(NEOPIXEL_PIN as usize), PIN_CNF_OUTPUT);

            core::ptr::write_volatile(PWM_PSEL_OUT0, NEOPIXEL_PSEL);
            core::ptr::write_volatile(PWM_ENABLE, 1);
            core::ptr::write_volatile(PWM_MODE, 0); // Up counter
            core::ptr::write_volatile(PWM_PRESCALER, 0); // Div1 → 16 MHz
            core::ptr::write_volatile(PWM_COUNTERTOP, COUNTERTOP);
            core::ptr::write_volatile(PWM_LOOP, 0); // play once
            core::ptr::write_volatile(PWM_DECODER, 0); // LOAD=Common, MODE=RefreshCount
            core::ptr::write_volatile(PWM_SEQ0_REFRESH, 0);
            core::ptr::write_volatile(PWM_SEQ0_ENDDELAY, 0);
        }
        Self {
            buf: [0; BITS],
            _pwm: pwm,
        }
    }

    /// Encode the pixels (GRB, MSB-first) into the DMA bit buffer.
    fn encode(&mut self, colors: &[Rgb; LED_COUNT]) {
        let mut i = 0;
        for c in colors {
            for byte in [c.g, c.r, c.b] {
                for bit in (0..8).rev() {
                    self.buf[i] = if (byte >> bit) & 1 == 1 { T1H } else { T0H };
                    i += 1;
                }
            }
        }
    }

    /// Push `colors` to the strip. Blocks for the ~150 µs the sequence takes.
    ///
    /// The buffer lives in `self` (RAM) and EasyDMA reads it for the duration of
    /// the sequence, which this call waits out — so the buffer is never reused
    /// while the DMA is still walking it.
    pub fn write(&mut self, colors: &[Rgb; LED_COUNT]) {
        self.encode(colors);
        // Full re-arm on every write, mirroring `maple::pwm_tx` — the one PWM
        // sequence driver in this firmware that is known to work repeatedly.
        // Setup is only half of it; the matching teardown below is what actually
        // makes a second write possible. Re-arming the setup alone was tried and
        // changed nothing.
        //
        // SAFETY: buf is a RAM field; EasyDMA reads it while the sequence plays.
        unsafe {
            core::ptr::write_volatile(PWM_ENABLE, 0);
            core::ptr::write_volatile(PWM_PSEL_OUT0, NEOPIXEL_PSEL);
            core::ptr::write_volatile(PWM_MODE, 0); // up counter
            core::ptr::write_volatile(PWM_COUNTERTOP, COUNTERTOP);
            core::ptr::write_volatile(PWM_PRESCALER, 0); // div1 → 16 MHz
            core::ptr::write_volatile(PWM_DECODER, 0); // LOAD=Common, MODE=RefreshCount
            core::ptr::write_volatile(PWM_LOOP, 0); // play once
            core::ptr::write_volatile(PWM_SEQ0_PTR, self.buf.as_ptr() as u32);
            core::ptr::write_volatile(PWM_SEQ0_CNT, BITS as u32);
            core::ptr::write_volatile(PWM_SEQ0_REFRESH, 0);
            core::ptr::write_volatile(PWM_SEQ0_ENDDELAY, 0);
            core::ptr::write_volatile(PWM_EVENTS_SEQEND0, 0);
            // Keep the pixel encoding from sinking below the SEQSTART that hands
            // the buffer to EasyDMA. `maple::pwm_tx` fences here for the same
            // reason (`start_playback`).
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
            core::ptr::write_volatile(PWM_ENABLE, 1);
            core::ptr::write_volatile(PWM_TASKS_SEQSTART0, 1);
        }

        // Wait out the sequence, then park the peripheral exactly the way
        // `maple::pwm_tx::finish_playback` does — disconnect PSEL, then disable.
        //
        // Leaving the block enabled and still connected to the pin after a
        // one-shot sequence is what broke every write after the first: the strip
        // froze on frame 1. Handing the pin back to GPIO is only safe because
        // `new` configured it as a driven LOW output, so the line parks in the
        // WS2812 reset state instead of floating.
        //
        // Bound: 120 bits × 1.25 µs ≈ 150 µs ≈ 9.6k cycles at 64 MHz. The guard
        // is far past that, so a wedged peripheral can't hang the poll loop.
        let mut guard = 0u32;
        while unsafe { core::ptr::read_volatile(PWM_EVENTS_SEQEND0) } == 0 {
            guard += 1;
            if guard > 1_000_000 {
                break;
            }
        }

        // SAFETY: the sequence is finished; releasing the peripheral we own.
        unsafe {
            core::ptr::write_volatile(PWM_PSEL_OUT0, PSEL_DISCONNECTED);
            core::ptr::write_volatile(PWM_ENABLE, 0);
        }
    }
}
