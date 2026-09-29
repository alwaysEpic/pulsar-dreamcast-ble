// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Hardware capture of a Maple reply: two SPIM instances as a one-bit logic
//! analyser each, sampling SDCKA and SDCKB into RAM by EasyDMA.
//!
//! The CPU is the sampler everywhere else in this firmware, and every cost on
//! the VMU read path follows from that: a 6.66 ms spin per block with the
//! app's interrupts masked, a byte-wide buffer eight times the data, and no
//! way to decode one block while the next is captured. The nRF52840 has no
//! programmable I/O; its substitute is SPIM in receive mode. With MOSI
//! disconnected and `TXD.MAXCNT` zero, a transfer is a free-running 8 MHz
//! clock and one MISO sample per clock, packed LSB-first into RAM — sample
//! `i` of a line is bit `i % 8` of byte `i / 8`
//! (`maple_protocol::packed::unpack_into` restores the byte-wide format the
//! decoder reads).
//!
//! **The trigger is the reply's own first edge, once.** A GPIOTE channel in
//! event mode watches SDCKA for a high-to-low transition; one PPI channel
//! forks that event into both instances' `TASKS_START`, so the two streams
//! begin in the same clock cycle without the CPU in the loop. The CPU's own
//! sampling loop starts from the same fall, which is what makes the two
//! captures comparable. The reply then carries thousands more SDCKA falls,
//! and a `START` on a running SPIM restarts its transfer: v270 measured
//! both DMAs stopped at ≈ 3,960 bytes, exactly 8 Mbit/s from the reply's
//! *last* edge (2026-09-15). So the trigger is made one-shot the
//! way Nordic documents: a second PPI channel on the same event whose task
//! is `TASKS_CHG[g].DIS` for the group holding both channels. The tasks of
//! one event all fire in that cycle; the group is disabled for the next.
//!
//! **Nothing is configured once.** Every register that matters is written
//! again in [`SpimCapture::arm`], with the instance disabled, before every
//! capture (pin selects take effect only while the instance is disabled, and
//! the rest of the firmware re-asserts peripheral state as policy). Outside
//! a capture both instances, the GPIOTE channel and the PPI channel are
//! disabled: an enabled SPIM and a GPIOTE channel in event mode each keep
//! the 16 MHz peripheral clock requested.
//!
//! **Sharing the bus pins.** MISO is routed by `PSEL` to the Maple lines the
//! bit-bang bus already owns as `Flex` GPIOs. The SPIM only listens, so the
//! GPIO keeps its direction; the instance is enabled only between the bus's
//! switch to input mode and the end of the reply, and disabled before the
//! next command is driven. The two clock outputs go to module pins the XIAO
//! nRF52840 routes to no pad (Seeed's schematic for the module names
//! P0.02–P0.05, P0.09, P0.10, P0.28, P0.29 and P1.11–P1.15 only); which pins is the
//! board's choice.
//!
//! **The buffers are the caller's.** [`SpimCapture::arm`] takes a
//! [`StreamBufs`] — a pointer to each stream and the bytes per stream — and
//! programs it into `RXD.PTR` and `RXD.MAXCNT`; this module owns no stream
//! storage at all. Each consumer declares its own pair as statics in
//! `.uninit`, which cortex-m-rt places after `.bss` and does not zero at boot
//! — so no existing RAM symbol moves (a RAM symbol moving is a timing
//! hazard) and EasyDMA, not the reset handler, fills them. On v266's layout
//! the streams land in RAM block 6 while the CPU capture buffer spans blocks
//! 4 and 5, so the DMA writes never contend with the pinned loop's stores for
//! a RAM AHB slave. Why one shared pair is not enough is [`SpimCapture`]'s
//! own doc: bench run #175. Nothing about a finished capture stays behind
//! either — [`SpimCapture::finish`] hands the caller a [`Captured`], which
//! names that caller's buffers and is the only way to read them.
//!
//! **PPI and the SoftDevice.** S140 owns PPI channels 17–31 and groups 4 and
//! 5 (`NRF_SOC_SD_PPI_CHANNELS_SD_ENABLED_MSK` and
//! `NRF_SOC_SD_PPI_GROUPS_SD_ENABLED_MSK` in the vendored S140 v7.3.0
//! `nrf_soc.h`); the channels and the group here are ones embassy hands out
//! from the app's range. S140 does not use GPIOTE.
//!
//! **Registers are written by address**, as `pwm_tx` writes PWM0's: embassy's
//! PAC re-export is behind its `unstable-pac` feature, and enabling it is a
//! Cargo change that alters embassy-nrf's crate hash — which reorders
//! symbols, which a placement check
//! cannot tell from a real shift. Offsets are the nRF52840 Product
//! Specification's (checked against nrf-pac 074935b). The `Peri` tokens are
//! still taken, so a second user of either instance or channel is a type
//! error rather than a convention.
//!
//! **Both clock pins are required.** With `PSEL.SCK` disconnected an
//! instance never completes a transfer: the v274 ran 1,040
//! captures on SPIM2 without its clock and `END` never fired (bench,
//! 2026-09-15). The pins carry nothing anyone reads, but the SPIM will not
//! clock its sampler without somewhere to put the clock.

use core::sync::atomic::{compiler_fence, Ordering};

use embassy_nrf::gpio::{AnyPin, Level, Output, OutputDrive, Pin as _, Port};
use embassy_nrf::gpiote::Channel as _;
use embassy_nrf::peripherals::{GPIOTE_CH0, PPI_CH0, PPI_CH1, PPI_GROUP0, SPI2, TWISPI1};
use embassy_nrf::ppi::{Channel as _, Group as _};
use embassy_nrf::Peri;

/// The hardware ceiling on one transfer, and what [`arm`] clamps a caller's
/// length to: `RXD.MAXCNT` is 16 bits wide, so no capture is longer than this
/// however large the caller's buffers are. How long a capture actually runs
/// is the caller's [`StreamBufs::len`](StreamBufs), which is the size of the
/// buffers it brought.
///
/// [`arm`]: SpimCapture::arm
const STREAM_MAXCNT: u32 = 0xFFFF;

/// Where one capture goes: a pointer to each stream, and the bytes per
/// stream.
///
/// EasyDMA fills the two regions between [`SpimCapture::arm`] and the
/// matching [`finish`](SpimCapture::finish) or
/// [`abort`](SpimCapture::abort), and [`Captured::streams`] hands back `len`
/// bytes of each.
///
/// Built once per consumer, beside the statics it names, so the argument for
/// those two addresses lives with them rather than here.
#[derive(Clone, Copy)]
pub struct StreamBufs {
    a: *mut u8,
    b: *mut u8,
    len: u32,
}

impl StreamBufs {
    /// The two stream buffers, `len` bytes each.
    ///
    /// # Safety
    ///
    /// `a` and `b` must each point at `len` writable bytes in RAM (EasyDMA
    /// reaches nothing else), in two regions that overlap neither each other
    /// nor anything else live. Both must stay valid from the
    /// [`arm`](SpimCapture::arm) that takes them until the next `arm` **with
    /// these same buffers**, not merely until that capture's `finish`: a
    /// finished capture is read back through [`Captured::streams`], and a
    /// consumer may latch the [`Captured`] and unpack it much later — after
    /// any number of other consumers' captures, which touch their own buffers
    /// and nothing here. Nothing else may read or write either region between
    /// the `arm` and its `finish`/`abort` — the DMA is writing there. A pair
    /// of `.uninit` statics owned by one consumer satisfies all of this; a
    /// local does not.
    #[must_use]
    pub const unsafe fn new(a: *mut u8, b: *mut u8, len: u32) -> Self {
        Self { a, b, len }
    }
}

/// Cycles to wait for `EVENTS_STOPPED` after `TASKS_STOP` in [`disarm`]:
/// the stop completes within one SPI clock, so 100 µs at 64 MHz is a
/// safety net, not a budget.
///
/// [`disarm`]: SpimCapture::disarm
const STOP_BUDGET_CYCLES: u32 = 6_400;

// SPIM1 and SPIM2 (nRF52840 PS § SPIM). SPIM0 is the IP5306's TWIM on
// pulsarv1; SPIM3 has anomaly 198.
const SPIM1_BASE: u32 = 0x4000_4000;
const SPIM2_BASE: u32 = 0x4002_3000;
const SPIM_TASKS_START: u32 = 0x010;
const SPIM_TASKS_STOP: u32 = 0x014;
const SPIM_EVENTS_STOPPED: u32 = 0x104;
const SPIM_EVENTS_END: u32 = 0x118;
const SPIM_EVENTS_STARTED: u32 = 0x14C;
const SPIM_SHORTS: u32 = 0x200;
const SPIM_INTENCLR: u32 = 0x308;
const SPIM_ENABLE: u32 = 0x500;
const SPIM_PSEL_SCK: u32 = 0x508;
const SPIM_PSEL_MOSI: u32 = 0x50C;
const SPIM_PSEL_MISO: u32 = 0x510;
const SPIM_PSEL_CSN: u32 = 0x514;
const SPIM_FREQUENCY: u32 = 0x524;
const SPIM_RXD_PTR: u32 = 0x534;
const SPIM_RXD_MAXCNT: u32 = 0x538;
const SPIM_RXD_AMOUNT: u32 = 0x53C;
const SPIM_TXD_MAXCNT: u32 = 0x548;
const SPIM_CONFIG: u32 = 0x554;

const SPIM_ENABLE_ENABLED: u32 = 7;
const SPIM_ENABLE_DISABLED: u32 = 0;
const SPIM_FREQUENCY_M8: u32 = 0x8000_0000;
/// `CONFIG`: ORDER bit 0 (1 = LSB first), CPHA bit 1 (0 = leading), CPOL
/// bit 2 (0 = active high). Mode 0, LSB first: one sample per rising clock
/// edge, bit `i % 8` of byte `i / 8` is sample `i`.
const SPIM_CONFIG_LSB_FIRST_MODE0: u32 = 0b001;

/// `PSEL.*`: pin bits 0–4, port bit 5, bit 31 set = disconnected.
const PSEL_PORT1: u32 = 1 << 5;
const PSEL_DISCONNECTED: u32 = 0xFFFF_FFFF;

// GPIOTE (PS § GPIOTE): `CONFIG[n]` has MODE in bits 0–1, PSEL in 8–12,
// PORT in 13, POLARITY in 16–17.
const GPIOTE_BASE: u32 = 0x4000_6000;
const GPIOTE_EVENTS_IN0: u32 = 0x100;
const GPIOTE_CONFIG0: u32 = 0x510;
const GPIOTE_MODE_DISABLED: u32 = 0;
const GPIOTE_MODE_EVENT: u32 = 1;
const GPIOTE_POLARITY_HI_TO_LO: u32 = 2 << 16;
const GPIOTE_PSEL_SHIFT: u32 = 8;

// PPI (PS § PPI): channel `n`'s event and task end points at
// `0x510 + 8n` and `0x514 + 8n`, its fork task at `0x910 + 4n`; group `g`'s
// member mask at `0x800 + 4g`, its disable task at `0x004 + 8g`.
const PPI_BASE: u32 = 0x4001_F000;
const PPI_TASKS_CHG0_DIS: u32 = 0x004;
const PPI_CHENSET: u32 = 0x504;
const PPI_CHENCLR: u32 = 0x508;
const PPI_CH0_EEP: u32 = 0x510;
const PPI_CH0_TEP: u32 = 0x514;
const PPI_CHG0: u32 = 0x800;
const PPI_FORK0_TEP: u32 = 0x910;

#[inline]
fn mmio_write(addr: u32, value: u32) {
    // SAFETY: every caller passes a register address of SPIM1, SPIM2, GPIOTE
    // or PPI built from the base and offset constants above, which the
    // nRF52840 memory map fixes; the `Peri` tokens `SpimCapture` holds make
    // it the only user of the two instances and the two channels, and the
    // SoftDevice touches none of them (PPI channels 17–31 are its; this one
    // is from the app's range). A volatile 32-bit write to such an address
    // is sound.
    unsafe { core::ptr::write_volatile(addr as *mut u32, value) }
}

#[inline]
fn mmio_read(addr: u32) -> u32 {
    // SAFETY: see `mmio_write`.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// What a board hands over for the capture: the two instances, the trigger
/// and fork channels, and the two clock pins. `None` from a board with no
/// pins to spare for the clocks.
pub struct Parts {
    /// SPIM1. Samples SDCKA.
    pub spim_a: Peri<'static, TWISPI1>,
    /// SPIM2. Samples SDCKB.
    pub spim_b: Peri<'static, SPI2>,
    /// The channel that turns SDCKA's fall into an event.
    pub gpiote: Peri<'static, GPIOTE_CH0>,
    /// The channel that forks that event into both `TASKS_START`s.
    pub ppi: Peri<'static, PPI_CH0>,
    /// The channel that, on the same event, disables the group below, so
    /// the fork fires once per capture.
    pub ppi_oneshot: Peri<'static, PPI_CH1>,
    /// The group holding both channels.
    pub ppi_group: Peri<'static, PPI_GROUP0>,
    /// SPIM1's clock output: a pin routed nowhere.
    pub sck_a: Peri<'static, AnyPin>,
    /// SPIM2's clock output, likewise.
    pub sck_b: Peri<'static, AnyPin>,
}

/// What a capture came to: the streams themselves if both completed, and
/// enough about the ones that did not to say why.
#[derive(Clone, Copy, Default)]
pub struct Ended {
    /// SDCKA's stream: `EVENTS_END` fired and `RXD.AMOUNT` is the length
    /// [`SpimCapture::arm`] programmed.
    pub a: bool,
    /// SDCKB's stream, likewise.
    pub b: bool,
    /// `EVENTS_STARTED` had fired on each instance when `finish` looked: the
    /// trigger reached it.
    pub started: (bool, bool),
    /// `RXD.AMOUNT` of each instance once it had ended or been stopped: how
    /// far the DMA got, so a transfer that outran the budget still reports
    /// its rate.
    pub amount: (u32, u32),
    /// Cycles `finish` waited for both `END`s, up to its budget.
    pub wait_cycles: u32,
    /// The finished capture, `Some` exactly when [`both`](Self::both): the
    /// caller's own streams, to read now or to latch and read windows later.
    /// This is the only copy — [`SpimCapture`] keeps nothing about a capture
    /// it has finished.
    pub captured: Option<Captured>,
}

impl Ended {
    /// Nothing happened.
    pub const NONE: Self = Self {
        a: false,
        b: false,
        started: (false, false),
        amount: (0, 0),
        wait_cycles: 0,
        captured: None,
    };

    /// Both streams are complete, which is exactly when
    /// [`captured`](Self::captured) is `Some`.
    #[must_use]
    pub const fn both(self) -> bool {
        self.a && self.b
    }
}

/// One finished capture: both streams whole, and the buffers they are in.
///
/// [`SpimCapture::finish`] is the only constructor — the field is private
/// and nothing else builds one — so holding a `Captured` *is* the proof that
/// both instances reported `END` with `RXD.AMOUNT` at the armed length and
/// were disabled and fenced. It is `Copy` and self-contained: latch it, and
/// read it through [`streams`](Self::streams) any number of windows later.
/// What it points at is the caller's own [`StreamBufs`], which no other
/// consumer's `arm` touches; only that caller's next `arm` ends its life, and
/// `StreamBufs::new`'s contract is where that is promised.
#[derive(Clone, Copy)]
pub struct Captured {
    bufs: StreamBufs,
}

impl Captured {
    /// The capture's two streams, SDCKA's then SDCKB's: `len` bytes of each
    /// of the buffers its [`arm`](SpimCapture::arm) was given.
    #[must_use]
    pub fn streams(&self) -> (&[u8], &[u8]) {
        // The conversion cannot fail on a 32-bit target.
        let len = usize::try_from(self.bufs.len).unwrap_or(0);
        // SAFETY: this value exists only because `finish` observed both
        // `END`s with `RXD.AMOUNT == len` after the DMA had been disabled,
        // and fenced — so `len` bytes behind `bufs.a` are initialised, no
        // DMA is writing there, and these loads cannot be hoisted above that
        // check. The pointer and length are the ones `arm` programmed, and
        // `StreamBufs::new`'s contract keeps that region valid and untouched
        // by anything else until the consumer that owns it arms these same
        // buffers again — which cannot be while this borrow is alive,
        // because the borrow is of the value that consumer holds.
        let stream_a = unsafe { core::slice::from_raw_parts(self.bufs.a.cast_const(), len) };
        // SAFETY: as for `stream_a`, for the second buffer behind `bufs.b`.
        let stream_b = unsafe { core::slice::from_raw_parts(self.bufs.b.cast_const(), len) };
        (stream_a, stream_b)
    }
}

/// One instance's fixed addresses and pin selects.
#[derive(Clone, Copy)]
struct Line {
    base: u32,
    sck_psel: u32,
    miso_psel: u32,
}

impl Line {
    #[inline]
    fn write(self, offset: u32, value: u32) {
        mmio_write(self.base + offset, value);
    }

    #[inline]
    fn read(self, offset: u32) -> u32 {
        mmio_read(self.base + offset)
    }

    /// Disabled, every register written, then enabled and waiting for
    /// `TASKS_START`. `maxcnt` bytes is this capture's transfer length.
    fn program(self, buf: *mut u8, maxcnt: u32) {
        self.write(SPIM_ENABLE, SPIM_ENABLE_DISABLED);
        self.write(SPIM_INTENCLR, 0xFFFF_FFFF);
        // No END→START short: one transfer per trigger. Reset state, and no
        // in-tree owner sets it, but re-asserted so the residue policy above
        // is literal.
        self.write(SPIM_SHORTS, 0);
        self.write(SPIM_PSEL_SCK, self.sck_psel);
        self.write(SPIM_PSEL_MISO, self.miso_psel);
        self.write(SPIM_PSEL_MOSI, PSEL_DISCONNECTED);
        self.write(SPIM_PSEL_CSN, PSEL_DISCONNECTED);
        self.write(SPIM_CONFIG, SPIM_CONFIG_LSB_FIRST_MODE0);
        self.write(SPIM_FREQUENCY, SPIM_FREQUENCY_M8);
        self.write(SPIM_RXD_PTR, ram_addr(buf));
        self.write(SPIM_RXD_MAXCNT, maxcnt);
        // Nothing to transmit: the over-read character clocks out on a
        // disconnected MOSI for the whole transfer.
        self.write(SPIM_TXD_MAXCNT, 0);
        self.write(SPIM_EVENTS_STARTED, 0);
        self.write(SPIM_EVENTS_END, 0);
        self.write(SPIM_EVENTS_STOPPED, 0);
        self.write(SPIM_ENABLE, SPIM_ENABLE_ENABLED);
    }

    fn ended(self) -> bool {
        self.read(SPIM_EVENTS_END) != 0
    }

    /// `EVENTS_END` fired and the DMA wrote the whole transfer — `maxcnt`,
    /// the length this capture was armed with, not the buffer's size.
    fn complete(self, maxcnt: u32) -> bool {
        self.ended() && self.read(SPIM_RXD_AMOUNT) == maxcnt
    }

    /// Stop a transfer still running, wait for the stop, and disable.
    fn off(self) {
        if !self.ended() && self.read(SPIM_EVENTS_STARTED) != 0 {
            self.write(SPIM_EVENTS_STOPPED, 0);
            self.write(SPIM_TASKS_STOP, 1);
            let t0 = cyc();
            while self.read(SPIM_EVENTS_STOPPED) == 0 && cyc().wrapping_sub(t0) < STOP_BUDGET_CYCLES
            {
            }
        }
        self.write(SPIM_ENABLE, SPIM_ENABLE_DISABLED);
    }
}

/// The capture: owns the instances and channels for the program's lifetime.
///
/// One per program — [`new`](Self::new) consumes the peripheral tokens, so a
/// second cannot be built.
///
/// **Nothing about a finished capture survives in the instance.** The
/// hardware has one pair of DMA pointers and one `RXD.MAXCNT`, and this type
/// is the only owner of them, so every field here belongs to the capture that
/// is armed *now*: [`finish`](Self::finish) takes the buffers back out and
/// hands them to its caller as a [`Captured`], and there is no way to ask the
/// instance about a capture that is over. That is what lets several consumers
/// share one instance. Each brings its own [`StreamBufs`] — so the bytes
/// survive a foreign `arm` — and each carries its own `Captured`, so the
/// length and the completion verdict do too. A consumer may therefore latch a
/// capture and unpack it windows later; the diag read pipeline latches in
/// `Probe::pending` and unpacks in `apply_pending`, one or more controller
/// polls after its `finish`.
///
/// Bench run #175 is the bill for the other arrangement. v286 read 21 % of
/// blocks badly against v277's 0.13 %, with 410 captures "carried over",
/// because since v283 the controller poll armed this same instance — then a
/// single shared pair of streams, one `maxcnt` and one `done` — with 3,072
/// bytes between the read's `finish` and the `streams()` it then asked the
/// instance for. The read unpacked the poll's bits at the poll's length, or
/// was refused outright. Hence [`StreamBufs`], hence no stream storage in
/// this module, and hence no `streams()` on this type.
pub struct SpimCapture {
    _spim_a: Peri<'static, TWISPI1>,
    _spim_b: Peri<'static, SPI2>,
    _gpiote: Peri<'static, GPIOTE_CH0>,
    _ppi: Peri<'static, PPI_CH0>,
    _ppi_oneshot: Peri<'static, PPI_CH1>,
    _ppi_group: Peri<'static, PPI_GROUP0>,
    /// Held as outputs, low, so the pins are ours and idle at the clock's
    /// idle level; the SPIM takes them over while it is enabled.
    _sck_a: Output<'static>,
    _sck_b: Output<'static>,
    line_a: Line,
    line_b: Line,
    gpiote_ch: u32,
    ppi_ch: u32,
    oneshot_ch: u32,
    group: u32,
    /// SDCKA's P0 pin number: the trigger.
    trigger_pin: u32,
    /// The buffers the armed capture is writing, its length clamped to
    /// [`STREAM_MAXCNT`]: where the DMA is writing and what completion is
    /// judged against. `None` whenever no capture is armed — before the
    /// first [`arm`](Self::arm), and again the moment
    /// [`finish`](Self::finish) or [`abort`](Self::abort) ends one, because
    /// `finish` hands them on inside its [`Captured`] and nothing about a
    /// capture that is over stays here.
    bufs: Option<StreamBufs>,
    armed: bool,
}

/// A `PSEL` value connecting P0.`pin`.
const fn p0_psel(pin: u8) -> u32 {
    pin as u32
}

/// A `PSEL` value for `pin`, on P1 if `port1`.
const fn pin_psel(pin: u8, port1: bool) -> u32 {
    pin as u32 | if port1 { PSEL_PORT1 } else { 0 }
}

/// A RAM address, as EasyDMA's `PTR` takes it.
#[expect(
    clippy::cast_possible_truncation,
    reason = "addresses are 32-bit on this target"
)]
fn ram_addr(ptr: *mut u8) -> u32 {
    ptr as usize as u32
}

impl SpimCapture {
    /// Take the parts and leave every peripheral disabled. `pin_a` and
    /// `pin_b` are SDCKA's and SDCKB's P0 pin numbers (the board contract
    /// puts both on P0). The DWT cycle counter must already be running
    /// (`MapleBus::new` enables it); `finish` and `disarm` time their waits
    /// by it.
    #[must_use]
    pub fn new(parts: Parts, pin_a: u8, pin_b: u8) -> Self {
        let line_a = Line {
            base: SPIM1_BASE,
            sck_psel: pin_psel(parts.sck_a.pin(), matches!(parts.sck_a.port(), Port::Port1)),
            miso_psel: p0_psel(pin_a),
        };
        let line_b = Line {
            base: SPIM2_BASE,
            sck_psel: pin_psel(parts.sck_b.pin(), matches!(parts.sck_b.port(), Port::Port1)),
            miso_psel: p0_psel(pin_b),
        };
        let gpiote_ch = u32::try_from(parts.gpiote.number()).unwrap_or(0);
        let ppi_ch = u32::try_from(parts.ppi.number()).unwrap_or(0);
        let oneshot_ch = u32::try_from(parts.ppi_oneshot.number()).unwrap_or(1);
        let group = u32::try_from(parts.ppi_group.number()).unwrap_or(0);
        let mut this = Self {
            _spim_a: parts.spim_a,
            _spim_b: parts.spim_b,
            _gpiote: parts.gpiote,
            _ppi: parts.ppi,
            _ppi_oneshot: parts.ppi_oneshot,
            _ppi_group: parts.ppi_group,
            _sck_a: Output::new(parts.sck_a, Level::Low, OutputDrive::Standard),
            _sck_b: Output::new(parts.sck_b, Level::Low, OutputDrive::Standard),
            line_a,
            line_b,
            gpiote_ch,
            ppi_ch,
            oneshot_ch,
            group,
            trigger_pin: u32::from(pin_a),
            bufs: None,
            armed: false,
        };
        this.disarm();
        this
    }

    /// Program both instances, the trigger and the fork, and wait for the
    /// edge. Call after the command has gone out and the bus is in input
    /// mode, before the CPU's own edge wait — never between the edge test and
    /// the sampling loop. About a microsecond of register writes.
    ///
    /// `bufs` says both where this capture goes and how long it runs:
    /// `bufs.len` bytes per stream, 8 samples each, so 8 × `len` / 8 MHz of
    /// bus time. The read path brings 6,656-byte buffers, the controller poll
    /// its own 3,072-byte pair  — and they must be its own; see
    /// the type doc, and run #175. A length above [`STREAM_MAXCNT`], which
    /// `RXD.MAXCNT` cannot hold, is **clamped**, not rejected: there is no
    /// unwind on this target to make a panic the safer answer.
    ///
    /// A capture that was still armed is disarmed first; its streams are lost.
    pub fn arm(&mut self, bufs: StreamBufs) {
        if self.armed {
            self.disarm();
        }
        let bufs = StreamBufs {
            len: bufs.len.min(STREAM_MAXCNT),
            ..bufs
        };
        self.bufs = Some(bufs);
        // Nothing the CPU wrote has to reach the DMA (the buffers are its
        // output), but the fence keeps every earlier read of the streams
        // ahead of the hardware's next writes — embassy's SPIM does the same
        // on both sides of a transfer.
        compiler_fence(Ordering::SeqCst);

        // The caller's buffers reach EasyDMA by address only; no reference
        // to either is created here. The only reader is `Captured::streams`,
        // and a `Captured` exists only after the `finish` that disabled both
        // instances, so the DMA's writes and any read never overlap.
        // `StreamBufs::new`'s contract puts both in RAM, as EasyDMA requires,
        // and keeps them unaliased until this consumer's next arm.
        self.line_a.program(bufs.a, bufs.len);
        self.line_b.program(bufs.b, bufs.len);

        // The trigger: SDCKA high-to-low. Configured before the event is
        // cleared, so a transition latched by the mode change itself (the
        // pin is high here, but the order costs nothing) cannot fire the
        // fork the moment PPI is enabled.
        let events_in = GPIOTE_BASE + GPIOTE_EVENTS_IN0 + 4 * self.gpiote_ch;
        mmio_write(
            GPIOTE_BASE + GPIOTE_CONFIG0 + 4 * self.gpiote_ch,
            GPIOTE_MODE_EVENT | (self.trigger_pin << GPIOTE_PSEL_SHIFT) | GPIOTE_POLARITY_HI_TO_LO,
        );
        mmio_write(events_in, 0);

        // The fork, re-asserted: the event into SPIM1's START, and SPIM2's.
        mmio_write(PPI_BASE + PPI_CH0_EEP + 8 * self.ppi_ch, events_in);
        mmio_write(
            PPI_BASE + PPI_CH0_TEP + 8 * self.ppi_ch,
            self.line_a.base + SPIM_TASKS_START,
        );
        mmio_write(
            PPI_BASE + PPI_FORK0_TEP + 4 * self.ppi_ch,
            self.line_b.base + SPIM_TASKS_START,
        );
        // One shot: the same event also disables the group holding both
        // channels, after this cycle's tasks have fired.
        mmio_write(PPI_BASE + PPI_CH0_EEP + 8 * self.oneshot_ch, events_in);
        mmio_write(
            PPI_BASE + PPI_CH0_TEP + 8 * self.oneshot_ch,
            PPI_BASE + PPI_TASKS_CHG0_DIS + 8 * self.group,
        );
        let both = (1 << self.ppi_ch) | (1 << self.oneshot_ch);
        mmio_write(PPI_BASE + PPI_CHG0 + 4 * self.group, both);
        mmio_write(PPI_BASE + PPI_CHENSET, both);

        self.armed = true;
    }

    /// Whether the trigger has fired: line A's `EVENTS_STARTED`, one
    /// volatile load, nothing else. Meaningful only while armed.
    #[inline]
    #[must_use]
    pub fn started(&self) -> bool {
        self.line_a.read(SPIM_EVENTS_STARTED) != 0
    }

    /// Whether both transfers have ended: both `EVENTS_END`s, two volatile
    /// loads, nothing else. A caller that polls this and sees `true` then
    /// calls [`finish`](Self::finish) with a budget of zero, which is the
    /// disarm-and-fence path; the streams arrive only from there, because
    /// only `finish` checks `RXD.AMOUNT` and orders the buffers' reads
    /// behind the DMA's writes.
    #[inline]
    #[must_use]
    pub fn ended(&self) -> bool {
        self.line_a.ended() && self.line_b.ended()
    }

    /// Wait up to `budget_cycles` for both streams to end, then disarm.
    /// Returns which ended, and — if both did — the capture itself in
    /// [`Ended::captured`]: this is where a finished capture leaves the
    /// instance, and the only place it can be had from.
    /// The streams end one microsecond per armed byte after the trigger —
    /// 6.66 ms for the read's 6,656, 3.07 ms for the controller poll's 3,072
    /// — so the caller's budget is what remains of that after its own
    /// capture; a caller that has already seen [`ended`](Self::ended) passes
    /// zero.
    pub fn finish(&mut self, budget_cycles: u32) -> Ended {
        let t0 = cyc();
        while !self.ended() && cyc().wrapping_sub(t0) <= budget_cycles {}
        let wait_cycles = cyc().wrapping_sub(t0);
        let started = (
            self.line_a.read(SPIM_EVENTS_STARTED) != 0,
            self.line_b.read(SPIM_EVENTS_STARTED) != 0,
        );
        // Out of the instance here, not read from it: the capture ends in
        // this call whatever the verdict, so the buffers go with the
        // `Captured` below or nowhere. `None` only before the first `arm`,
        // when neither instance has run and so neither can report `END`.
        let bufs = self.bufs.take();
        let maxcnt = bufs.map_or(0, |bufs| bufs.len);
        let (a, b) = (self.line_a.complete(maxcnt), self.line_b.complete(maxcnt));
        self.disarm();
        // `AMOUNT` is valid after `END` or `STOPPED`, and `disarm` waited
        // for the stop; read while still holding the instance's registers.
        let ended = Ended {
            a,
            b,
            started,
            amount: (
                self.line_a.read(SPIM_RXD_AMOUNT),
                self.line_b.read(SPIM_RXD_AMOUNT),
            ),
            wait_cycles,
            // `Some` exactly on `both()`: a complete stream is one whose
            // `RXD.AMOUNT` matched `maxcnt`, which is `bufs`'s own length, so
            // `a && b` cannot hold with no buffers to report.
            captured: match (a && b, bufs) {
                (true, Some(bufs)) => Some(Captured { bufs }),
                _ => None,
            },
        };
        // The `END`/`AMOUNT` reads and the disable above are volatile, but
        // that alone does not order the ordinary loads `Captured::streams`
        // will make against the hardware's writes: without this fence the
        // compiler may hoist them above the completion check. No cache on
        // this core, so a compiler fence is the whole requirement.
        compiler_fence(Ordering::SeqCst);
        ended
    }

    /// Give up on an armed capture without waiting — the reply never came.
    /// Its streams are lost: nothing hands them out but `finish`.
    pub fn abort(&mut self) {
        self.disarm();
    }

    /// Everything off: the fork, the trigger, both instances. A transfer
    /// still running is stopped first, and the stop waited for, before the
    /// instance is disabled.
    fn disarm(&mut self) {
        mmio_write(
            PPI_BASE + PPI_CHENCLR,
            (1 << self.ppi_ch) | (1 << self.oneshot_ch),
        );
        mmio_write(PPI_BASE + PPI_CHG0 + 4 * self.group, 0);
        mmio_write(
            GPIOTE_BASE + GPIOTE_CONFIG0 + 4 * self.gpiote_ch,
            GPIOTE_MODE_DISABLED,
        );
        self.line_a.off();
        self.line_b.off();
        self.armed = false;
        // No capture is armed, so the instance names no buffers. `finish`
        // has already taken them for its `Captured`; every other path drops
        // them.
        self.bufs = None;
    }

    /// Whether a capture is armed and waiting for, or in, its transfer.
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }
}

/// DWT cycle count; enabled by `MapleBus::new`.
#[inline]
fn cyc() -> u32 {
    cortex_m::peripheral::DWT::cycle_count()
}
