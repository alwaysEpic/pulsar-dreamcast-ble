// SPDX-License-Identifier: GPL-3.0-or-later

//! SoftDevice Radio Timeslot-based Maple Bus TX.
//!
//! Long Maple Bus transmissions (e.g., VMU LCD writes, ~200 bytes / ~1.6ms)
//! get corrupted by SoftDevice BLE interrupts during CPU bit-banging. The
//! Radio Timeslot API grants guaranteed interrupt-free CPU time for the TX.
//!
//! # How it works
//!
//! 1. The caller pre-computes the entire GPIO waveform (pin states for every
//!    half-bit) into [`TX_WAVEFORM`].
//! 2. A radio session is opened once at init.
//! 3. When a TX is needed, [`request_timeslot_tx`] is called, which stores
//!    the waveform length and requests a timeslot.
//! 4. The SoftDevice calls [`timeslot_callback`] at priority 0 (no interrupts)
//!    with signal type START.
//! 5. The callback blasts the waveform out via direct GPIO register writes,
//!    then signals completion via an atomic flag.
//! 6. The caller polls [`is_tx_complete`] to know when it's done.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use nrf_softdevice_s140::{
    self as sd, NRF_RADIO_CALLBACK_SIGNAL_TYPE_NRF_RADIO_CALLBACK_SIGNAL_TYPE_START,
    NRF_RADIO_HFCLK_CFG_NRF_RADIO_HFCLK_CFG_NO_GUARANTEE,
    NRF_RADIO_PRIORITY_NRF_RADIO_PRIORITY_NORMAL,
    NRF_RADIO_REQUEST_TYPE_NRF_RADIO_REQ_TYPE_EARLIEST,
    NRF_RADIO_SIGNAL_CALLBACK_ACTION_NRF_RADIO_SIGNAL_CALLBACK_ACTION_END, NRF_SUCCESS,
};

use super::gpio_bus::{HALF_BIT_NOPS, LCD_PAYLOAD_WORDS, PIN_STABILIZE_NOPS};

/// P0 OUTSET register — write 1 to set pin high.
const P0_OUTSET: *mut u32 = 0x5000_0508 as *mut u32;
/// P0 OUTCLR register — write 1 to set pin low.
const P0_OUTCLR: *mut u32 = 0x5000_050C as *mut u32;
/// P0 DIR SET register — write 1 to set pin as output.
const P0_DIRSET: *mut u32 = 0x5000_0518 as *mut u32;
/// P0 DIR CLR register — write 1 to set pin as input.
const P0_DIRCLR: *mut u32 = 0x5000_051C as *mut u32;

const PIN_A_MASK: u32 = 1 << crate::board::PIN_A_BIT;
const PIN_B_MASK: u32 = 1 << crate::board::PIN_B_BIT;
const BOTH_MASK: u32 = PIN_A_MASK | PIN_B_MASK;

/// Maximum waveform entries.
///
/// VMU LCD write: start pattern (~20 half-bits) + 50 words × 32 bits × 2 half-bits +
/// CRC (8 bits × 2) + end pattern (~14 half-bits) ≈ 3250 entries.
///
/// Must fit within the shared `SAMPLE_BUFFER` (24,576 u32s = 12,288 `PinAction`s).
/// Each bit uses 3 waveform entries (data + clock + restore), so a 50-word
/// packet = 50×32×3 + start(11) + end(7) + crc(24) + idle(2) ≈ 4844 entries.
const MAX_WAVEFORM_LEN: usize = 5000;

/// Each waveform entry: (`set_mask`, `clr_mask`) for OUTSET/OUTCLR registers.
/// This avoids read-modify-write; we just write the masks directly.
#[repr(C)]
#[derive(Copy, Clone)]
struct PinAction {
    set: u32,
    clr: u32,
}

/// Access the shared `SAMPLE_BUFFER` as a TX waveform buffer.
///
/// The sample buffer (24,576 × u32 = 96KB) is reused for the TX waveform
/// since TX and RX never overlap — TX completes before RX begins.
/// Each `PinAction` is 2 × u32, so 24,576 u32s fits 12,288 `PinAction`s.
///
/// # Safety
/// Single-core Cortex-M4, no concurrent access. TX waveform is fully written
/// before the timeslot callback reads it, and the callback finishes before
/// any RX bulk capture uses the same memory.
#[inline]
unsafe fn tx_waveform() -> &'static mut [PinAction] {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "taking the address and building the slice are one derivation, \
                  justified by the single exclusivity argument above"
    )]
    // SAFETY: the caller's `# Safety` contract above establishes exclusivity —
    // the waveform is fully written before the timeslot callback reads it, and
    // that callback finishes before any RX capture reuses the buffer. The
    // reinterpretation fits: `MAX_WAVEFORM_LEN` entries of `PinAction` are sized
    // to stay within `SAMPLE_BUFFER`, and `addr_of_mut!` avoids forming an
    // intermediate reference to the static.
    unsafe {
        let ptr = core::ptr::addr_of_mut!(super::gpio_bus::SAMPLE_BUFFER).cast::<PinAction>();
        core::slice::from_raw_parts_mut(ptr, MAX_WAVEFORM_LEN)
    }
}

/// Length of the current waveform (number of valid entries in `TX_WAVEFORM`).
static TX_WAVEFORM_LEN: AtomicU32 = AtomicU32::new(0);

/// Set to true when a timeslot TX completes.
static TX_COMPLETE: AtomicBool = AtomicBool::new(false);

/// Set to true when a timeslot TX fails to schedule.
static TX_FAILED: AtomicBool = AtomicBool::new(false);

/// Whether the radio session is open.
static SESSION_OPEN: AtomicBool = AtomicBool::new(false);

/// Static return parameter for the signal callback.
///
/// Initialized at runtime in the callback since the union fields are
/// not const-constructible. We only use `ACTION_END`.
static mut RETURN_PARAM: core::mem::MaybeUninit<sd::nrf_radio_signal_callback_return_param_t> =
    core::mem::MaybeUninit::zeroed();

/// Open a radio session for timeslot requests.
///
/// Call once during initialization, after the SoftDevice is enabled.
pub fn open_session() -> bool {
    if SESSION_OPEN.load(Ordering::Relaxed) {
        return true;
    }
    // SAFETY: a SoftDevice SVC call taking a `'static` function pointer by
    // value. The SoftDevice must be enabled, which the doc comment above makes
    // the caller's contract, and the `SESSION_OPEN` guard above ensures we never
    // open a second session over a live one. The return code is checked below.
    let ret = unsafe { sd::sd_radio_session_open(Some(timeslot_callback)) };
    if ret == NRF_SUCCESS {
        SESSION_OPEN.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

/// Close the radio session. Call after the timeslot TX completes.
pub fn close_session() {
    if SESSION_OPEN.load(Ordering::Relaxed) {
        // SAFETY: a SoftDevice SVC call taking no arguments. The
        // `SESSION_OPEN` guard above guarantees a session is actually open, so
        // this cannot close one that was never opened.
        unsafe { sd::sd_radio_session_close() };
        SESSION_OPEN.store(false, Ordering::Release);
    }
}

/// Build a TX waveform for a Maple Bus LCD write and request a timeslot to send it.
///
/// This pre-computes the entire GPIO waveform, then asks the SoftDevice for
/// interrupt-free time to blast it out. Returns immediately; poll
/// [`is_tx_complete`] or [`is_tx_failed`] afterward.
///
/// # Arguments
/// * `sender` — Maple Bus sender address
/// * `dest` — Maple Bus destination address
/// * `framebuffer` — 192-byte VMU LCD data
#[expect(
    clippy::cast_possible_truncation,
    reason = "the request-type/hfclk/priority constants are small enum discriminants, and the waveform length is bounded by MAX_WAVEFORM_LEN"
)]
pub fn request_lcd_tx(sender: u8, dest: u8, framebuffer: &[u8; 192]) -> bool {
    if !SESSION_OPEN.load(Ordering::Relaxed) {
        return false;
    }

    TX_COMPLETE.store(false, Ordering::Release);
    TX_FAILED.store(false, Ordering::Release);

    // Build the waveform
    let len = build_lcd_waveform(sender, dest, framebuffer);
    TX_WAVEFORM_LEN.store(len as u32, Ordering::Release);

    // Request a timeslot (2500µs = 2.5ms, enough for ~1.6ms TX + margin)
    let request = sd::nrf_radio_request_t {
        request_type: NRF_RADIO_REQUEST_TYPE_NRF_RADIO_REQ_TYPE_EARLIEST as u8,
        params: sd::nrf_radio_request_t__bindgen_ty_1 {
            earliest: sd::nrf_radio_request_earliest_t {
                hfclk: NRF_RADIO_HFCLK_CFG_NRF_RADIO_HFCLK_CFG_NO_GUARANTEE as u8,
                priority: NRF_RADIO_PRIORITY_NRF_RADIO_PRIORITY_NORMAL as u8,
                length_us: 4000,    // 4ms — covers ~2.4ms waveform + margin
                timeout_us: 10_000, // 10ms max wait for timeslot
            },
        },
    };

    // SAFETY: a SoftDevice SVC call taking a pointer to `request`, which lives
    // on this stack frame and stays valid for the duration of the call — the
    // SoftDevice copies the descriptor before returning. A session is open
    // (checked by the caller), which is the call's precondition. The return code
    // is checked below.
    let ret = unsafe { sd::sd_radio_request(&raw const request) };
    if ret != NRF_SUCCESS {
        TX_FAILED.store(true, Ordering::Release);
        return false;
    }
    true
}

/// Build a TX waveform for a Maple Bus `DEVICE_INFO` request and request a timeslot.
///
/// Much shorter than LCD write (~5 bytes), but using timeslot for consistency.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the request-type/hfclk/priority constants are small enum discriminants, and the waveform length is bounded by MAX_WAVEFORM_LEN"
)]
pub fn request_device_info_tx(sender: u8, dest: u8) -> bool {
    if !SESSION_OPEN.load(Ordering::Relaxed) {
        return false;
    }

    TX_COMPLETE.store(false, Ordering::Release);
    TX_FAILED.store(false, Ordering::Release);

    let len = build_short_packet_waveform(sender, dest, 0x01, &[]); // cmd=DEVICE_INFO, no payload
    TX_WAVEFORM_LEN.store(len as u32, Ordering::Release);

    let request = sd::nrf_radio_request_t {
        request_type: NRF_RADIO_REQUEST_TYPE_NRF_RADIO_REQ_TYPE_EARLIEST as u8,
        params: sd::nrf_radio_request_t__bindgen_ty_1 {
            earliest: sd::nrf_radio_request_earliest_t {
                hfclk: NRF_RADIO_HFCLK_CFG_NRF_RADIO_HFCLK_CFG_NO_GUARANTEE as u8,
                priority: NRF_RADIO_PRIORITY_NRF_RADIO_PRIORITY_NORMAL as u8,
                length_us: 500,
                timeout_us: 10_000,
            },
        },
    };

    // SAFETY: a SoftDevice SVC call taking a pointer to `request`, which lives
    // on this stack frame and stays valid for the duration of the call — the
    // SoftDevice copies the descriptor before returning. A session is open
    // (checked by the caller), which is the call's precondition. The return code
    // is checked below.
    let ret = unsafe { sd::sd_radio_request(&raw const request) };
    if ret != NRF_SUCCESS {
        TX_FAILED.store(true, Ordering::Release);
        return false;
    }
    true
}

/// Check if the timeslot TX has completed.
pub fn is_tx_complete() -> bool {
    TX_COMPLETE.load(Ordering::Acquire)
}

/// Check if the timeslot TX failed to schedule.
pub fn is_tx_failed() -> bool {
    TX_FAILED.load(Ordering::Acquire)
}

/// SoftDevice radio signal callback. Runs at priority 0 (highest, no interrupts).
///
/// # Safety
/// Called by the SoftDevice. Must not use Embassy async, RTT, or anything
/// requiring lower-priority interrupts.
#[expect(
    clippy::cast_possible_truncation,
    reason = "SoftDevice callback-action and signal-type constants are small enum discriminants that fit u8"
)]
unsafe extern "C" fn timeslot_callback(
    signal_type: u8,
) -> *mut sd::nrf_radio_signal_callback_return_param_t {
    if signal_type == NRF_RADIO_CALLBACK_SIGNAL_TYPE_NRF_RADIO_CALLBACK_SIGNAL_TYPE_START as u8 {
        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "this is the timeslot TX blast: direction-set, the waveform \
                      writes, and direction-clear are one bus transaction whose \
                      ordering IS the wire protocol. Splitting it would invite \
                      reordering the very thing that must not move"
        )]
        // SAFETY: P0 DIRSET/DIRCLR/OUTSET/OUTCLR are fixed, word-aligned GPIO
        // registers. We hold the bus exclusively here: the SoftDevice has
        // granted a timeslot and this runs at priority 0, so no interrupt and no
        // other Maple code can touch these lines mid-frame. `tx_waveform()` is
        // sound for the same reason — its `# Safety` contract requires the
        // waveform to be fully written before the callback reads it, which the
        // request path guarantees — and `len` comes from `TX_WAVEFORM_LEN`,
        // which the builder stores only after filling that many entries, so the
        // slice cannot exceed the initialised prefix.
        unsafe {
            // Set pins as output
            core::ptr::write_volatile(P0_DIRSET, BOTH_MASK);

            // Small stabilization delay
            for _ in 0..PIN_STABILIZE_NOPS {
                cortex_m::asm::nop();
            }

            // Blast the pre-computed waveform
            let len = TX_WAVEFORM_LEN.load(Ordering::Acquire) as usize;
            let waveform = &tx_waveform()[..len];

            for action in waveform {
                if action.set != 0 {
                    core::ptr::write_volatile(P0_OUTSET, action.set);
                }
                if action.clr != 0 {
                    core::ptr::write_volatile(P0_OUTCLR, action.clr);
                }
                for _ in 0..HALF_BIT_NOPS {
                    cortex_m::asm::nop();
                }
            }

            // Set pins back to input (release bus)
            core::ptr::write_volatile(P0_DIRCLR, BOTH_MASK);
        }

        TX_COMPLETE.store(true, Ordering::Release);
    }

    // End the timeslot
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "taking the address and writing through it are one initialisation \
                  of the return parameter the SoftDevice is about to read"
    )]
    // SAFETY: `RETURN_PARAM` is a zeroed static this callback alone writes, and
    // the SoftDevice calls us non-reentrantly, so there is no aliasing.
    // `addr_of_mut!` avoids forming a reference to the `MaybeUninit` static;
    // writing `callback_action` initialises the only field the SoftDevice reads
    // for an END action. The pointer stays valid after return because the static
    // has `'static` lifetime.
    unsafe {
        let ret = core::ptr::addr_of_mut!(RETURN_PARAM)
            .cast::<sd::nrf_radio_signal_callback_return_param_t>();
        (*ret).callback_action =
            NRF_RADIO_SIGNAL_CALLBACK_ACTION_NRF_RADIO_SIGNAL_CALLBACK_ACTION_END as u8;
        ret
    }
}

// ── Waveform builders ───────────────────────────────────────────────────────

/// Waveform builder helper — tracks position in the waveform buffer.
struct WaveformBuilder {
    pos: usize,
    phase: bool,
    crc: u8,
}

impl WaveformBuilder {
    const fn new() -> Self {
        Self {
            pos: 0,
            phase: true,
            crc: 0,
        }
    }

    /// Emit a single pin action (one half-bit period).
    #[inline]
    fn emit(&mut self, set: u32, clr: u32) {
        if self.pos < MAX_WAVEFORM_LEN {
            // SAFETY: `tx_waveform()` reinterprets the shared sample buffer,
            // which is exclusively ours while a TX waveform is being built — TX
            // and RX never overlap (see `gpio_bus::tx_waveform_buf`). The index
            // is bounded by the `self.pos < MAX_WAVEFORM_LEN` test above.
            unsafe {
                tx_waveform()[self.pos] = PinAction { set, clr };
            }
            self.pos += 1;
        }
    }

    /// Emit the idle state (A high, B low).
    fn emit_idle(&mut self) {
        self.emit(PIN_A_MASK, PIN_B_MASK);
    }

    /// Emit the start pattern.
    fn emit_start_pattern(&mut self) {
        // SDCKA LOW
        self.emit(0, PIN_A_MASK);

        // Toggle SDCKB 4 times
        for _ in 0..4 {
            self.emit(PIN_B_MASK, 0); // B HIGH
            self.emit(0, PIN_B_MASK); // B LOW
        }

        // SDCKB HIGH
        self.emit(PIN_B_MASK, 0);
        // SDCKA HIGH
        self.emit(PIN_A_MASK, 0);
        // SDCKB LOW
        self.emit(0, PIN_B_MASK);
    }

    /// Emit the end pattern.
    fn emit_end_pattern(&mut self) {
        self.emit(BOTH_MASK, 0); // A HIGH, B HIGH
        self.emit(0, PIN_B_MASK); // B LOW
        self.emit(0, PIN_A_MASK); // A LOW
        self.emit(PIN_A_MASK, 0); // A HIGH
        self.emit(0, PIN_A_MASK); // A LOW
        self.emit(PIN_A_MASK, 0); // A HIGH
        self.emit(PIN_B_MASK, 0); // B HIGH
    }

    /// Emit a single data bit using the alternating phase scheme.
    #[inline]
    fn emit_bit(&mut self, bit: bool) {
        if self.phase {
            // Phase true: SDCKA = clock, SDCKB = data
            if bit {
                self.emit(PIN_B_MASK, 0); // B = data HIGH
            } else {
                self.emit(0, PIN_B_MASK); // B = data LOW
            }
            self.emit(0, PIN_A_MASK); // A falls (clock)
                                      // Data pin returns high after clock
            self.emit(PIN_B_MASK, 0);
        } else {
            // Phase false: SDCKB = clock, SDCKA = data
            if bit {
                self.emit(PIN_A_MASK, 0); // A = data HIGH
            } else {
                self.emit(0, PIN_A_MASK); // A = data LOW
            }
            self.emit(0, PIN_B_MASK); // B falls (clock)
                                      // Data pin returns high after clock
            self.emit(PIN_A_MASK, 0);
        }
        self.phase = !self.phase;
    }

    /// Emit a byte (MSB first).
    fn emit_byte(&mut self, byte: u8) {
        for i in (0..8).rev() {
            self.emit_bit((byte >> i) & 1 == 1);
        }
    }

    /// Emit a 32-bit word in Maple Bus byte order (LSB first) and update CRC.
    fn emit_word(&mut self, word: u32) {
        let bytes = word.to_le_bytes();
        for &b in &bytes {
            self.emit_byte(b);
            self.crc ^= b;
        }
    }

    const fn finish(self) -> usize {
        self.pos
    }
}

/// Build the waveform for a VMU LCD `BLOCK_WRITE` packet.
fn build_lcd_waveform(sender: u8, dest: u8, framebuffer: &[u8; 192]) -> usize {
    let mut wb = WaveformBuilder::new();

    wb.emit_idle();
    wb.emit_start_pattern();

    // Frame word: payload word count, sender, dest, command=0x0C
    let frame: u32 =
        (0x0C_u32 << 24) | (u32::from(dest) << 16) | (u32::from(sender) << 8) | LCD_PAYLOAD_WORDS;
    wb.emit_word(frame);

    // Function type: FUNC_LCD = 0x0000_0004
    wb.emit_word(0x0000_0004);

    // Location word: partition=0, phase=0, block=0
    wb.emit_word(0x0000_0000);

    // 192 bytes of LCD data as 48 words.
    // Bytes are reversed within each word — the VMU interprets each 32-bit
    // word as big-endian, so byte 3 maps to the leftmost pixels.
    for chunk in framebuffer.chunks_exact(4) {
        let word = u32::from_le_bytes([chunk[3], chunk[2], chunk[1], chunk[0]]);
        wb.emit_word(word);
    }

    // CRC byte (not included in CRC calculation itself)
    let crc = wb.crc;
    wb.emit_byte(crc);

    wb.emit_end_pattern();
    wb.emit_idle();

    wb.finish()
}

/// Build the waveform for a short Maple Bus packet (`DEVICE_INFO`, `GET_CONDITION`, etc.).
fn build_short_packet_waveform(sender: u8, dest: u8, command: u8, payload_words: &[u32]) -> usize {
    let mut wb = WaveformBuilder::new();

    wb.emit_idle();
    wb.emit_start_pattern();

    #[expect(
        clippy::cast_possible_truncation,
        reason = "payload length is masked to & 0xFF in the same expression, so the discarded bits are provably zero"
    )]
    let frame: u32 = (u32::from(command) << 24)
        | (u32::from(dest) << 16)
        | (u32::from(sender) << 8)
        | (payload_words.len() as u32 & 0xFF);
    wb.emit_word(frame);

    for &word in payload_words {
        wb.emit_word(word);
    }

    let crc = wb.crc;
    wb.emit_byte(crc);

    wb.emit_end_pattern();
    wb.emit_idle();

    wb.finish()
}
