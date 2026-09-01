// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Shared support for boards built on the **Seeed XIAO nRF52840 module**
//! (currently `xiao` and `pulsarv1`).
//!
//! These boards share the same silicon — P25Q16H QSPI flash, DC/DC regulator,
//! USB, and the System-Off wake path — so the housekeeping lives here once.
//! Board-specific differences (which pins to hold through System Off, whether a
//! pin must be skipped during pin-disconnect) are passed in as data rather than
//! duplicated. (Status LEDs are *not* shared — xiao uses the onboard RGB,
//! pulsarv1 a WS2812 bar — so each board owns its own `StatusIndicator`.)
//!
//! Only compiled for XIAO-module boards; see `mod.rs`.

/// Enable the DC/DC regulator (REG1) — same inductor on every XIAO-module board.
pub const fn configure_dcdc(config: &mut embassy_nrf::config::Config) {
    config.dcdc.reg1 = true;
}

// ── Silicon housekeeping ─────────────────────────────────────────────────────

/// Disconnect all GPIO pins to clear bootloader residue (Hi-Z, ~0µA).
///
/// `skip_p0` names a P0 pin to leave untouched — the discrete carrier passes
/// `Some(28)` to preserve boost-SHDN LOW (disconnecting it lets the Pololu's
/// pull-up momentarily enable 5V); boards without that hazard pass `None`.
///
/// # Safety
/// Writes directly to `PIN_CNF` registers; call before Embassy pin init.
pub unsafe fn disconnect_all_pins(skip_p0: Option<usize>) {
    const P0_PIN_CNF_BASE: *mut u32 = (0x5000_0000 + 0x700) as *mut u32;
    const P1_PIN_CNF_BASE: *mut u32 = (0x5000_0300 + 0x700) as *mut u32;
    const DISCONNECT: u32 = 0x0000_0002;

    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "one GPIO teardown sweep; every write is the same operation against \
                  a different pin index and shares a single justification"
    )]
    // SAFETY: P0 and P1 `PIN_CNF` are fixed, word-aligned register arrays in the
    // nRF52840 memory map. The loop bounds match the hardware — P0 has 32 pins
    // and P1 has 16 — so every `.add()` stays inside its own register block.
    // The caller's contract (see `# Safety` above) is that this runs before
    // Embassy pin init, so no driver holds a conflicting view of these pins.
    unsafe {
        for pin in 0..32 {
            if Some(pin) == skip_p0 {
                continue;
            }
            core::ptr::write_volatile(P0_PIN_CNF_BASE.add(pin), DISCONNECT);
        }
        for pin in 0..16 {
            core::ptr::write_volatile(P1_PIN_CNF_BASE.add(pin), DISCONNECT);
        }
    }
}

/// P1 pins carrying the **Sense**-only load-switch enables, both active-HIGH:
/// - `P1.08` — LSM6DS3TR-C IMU rail (also feeds that bus's I²C pull-ups)
/// - `P1.10` — MSM261D3526H1CPM PDM microphone rail
///
/// Neither is routed to a XIAO pad on *either* variant, so driving them is
/// always safe: on a plain XIAO the nets simply aren't populated.
const SENSE_ENABLE_P1: [usize; 2] = [8, 10];

/// Hold the XIAO **Sense**'s IMU and microphone rails off.
///
/// Both enables are active-HIGH into a load switch, and our teardown puts every
/// GPIO in DISCONNECT (Hi-Z, *no pull*) — which leaves those enables floating
/// rather than off, so whether the rails come up is down to whatever leakage
/// and stray coupling decide. Driving them LOW makes "off" deterministic. The
/// parts are ~0.55 mA (IMU, high-performance) and ~0.65 mA (mic) when powered,
/// against a ~5 µA System-Off budget, so a floating enable that latches on is
/// worth more than the entire sleep current.
///
/// Costs two register writes and applies to plain XIAOs harmlessly, so it runs
/// unconditionally rather than behind a board feature — the module variant is
/// not something the firmware can detect, and a Sense can be swapped in at any
/// time (they already have been).
///
/// # Safety
/// Writes directly to GPIO registers.
pub unsafe fn sense_peripherals_off() {
    const P1_OUTCLR: *mut u32 = 0x5000_080C as *mut u32;
    const P1_PIN_CNF_BASE: *mut u32 = (0x5000_0300 + 0x700) as *mut u32;
    const OUTPUT_CFG: u32 = 0x0000_0003; // DIR=output, INPUT=disconnected

    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "drive-LOW-then-configure-as-output is one sequence per pin, and the \
                  order is the point: configuring first would briefly drive the \
                  rail enable at whatever level OUT happened to hold"
    )]
    // SAFETY: `P1_OUTCLR` and the P1 `PIN_CNF` array are fixed, word-aligned
    // registers. `SENSE_ENABLE_P1` holds pins 8 and 10, both inside P1's 16-pin
    // block, so the shifts and `.add()` offsets are in range. As the doc comment
    // explains, neither net is routed to a pad on either module variant, so
    // driving them cannot fight an external source.
    unsafe {
        for pin in SENSE_ENABLE_P1 {
            core::ptr::write_volatile(P1_OUTCLR, 1 << pin);
            core::ptr::write_volatile(P1_PIN_CNF_BASE.add(pin), OUTPUT_CFG);
        }
    }
}

/// Put the onboard P25Q16H QSPI flash into Deep Power Down (~3µA vs 2-5mA).
///
/// Bit-bangs the DPD command (0xB9); CS (P0.25) is then held HIGH.
///
/// # Safety
/// Writes directly to GPIO registers.
pub unsafe fn qspi_flash_deep_power_down() {
    const P0_OUTSET: *mut u32 = 0x5000_0508 as *mut u32;
    const P0_OUTCLR: *mut u32 = 0x5000_050C as *mut u32;
    const P0_PIN_CNF_BASE: *mut u32 = (0x5000_0000 + 0x700) as *mut u32;

    const CS: u32 = 25; // P0.25
    const SCK: u32 = 21; // P0.21
    const IO0: u32 = 20; // P0.20 (MOSI)
    const CNF_OUTPUT: u32 = 0x0000_0003;

    const DPD_CMD: u8 = 0xB9;
    const DISCONNECT: u32 = 0x0000_0002;

    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "one bit-banged SPI transaction: the whole point is the ordering of \
                  CS, SCK and IO0 edges, so the writes are only sound as a sequence \
                  and cannot be justified individually"
    )]
    // SAFETY: every address here is a fixed, word-aligned P0 GPIO register.
    // CS/SCK/IO0 are compile-time pin numbers below 32, so the shifts and the
    // `PIN_CNF` offsets stay inside P0's register block. The caller's contract
    // (`# Safety` above) is that this runs before Embassy claims these pins, so
    // no driver is concurrently driving the QSPI lines.
    unsafe {
        core::ptr::write_volatile(P0_OUTSET, 1 << CS);
        core::ptr::write_volatile(P0_OUTCLR, 1 << SCK);
        core::ptr::write_volatile(P0_PIN_CNF_BASE.add(CS as usize), CNF_OUTPUT);
        core::ptr::write_volatile(P0_PIN_CNF_BASE.add(SCK as usize), CNF_OUTPUT);
        core::ptr::write_volatile(P0_PIN_CNF_BASE.add(IO0 as usize), CNF_OUTPUT);

        core::ptr::write_volatile(P0_OUTCLR, 1 << CS); // assert CS

        for i in (0..8).rev() {
            if (DPD_CMD >> i) & 1 == 1 {
                core::ptr::write_volatile(P0_OUTSET, 1 << IO0);
            } else {
                core::ptr::write_volatile(P0_OUTCLR, 1 << IO0);
            }
            cortex_m::asm::nop();
            cortex_m::asm::nop();
            core::ptr::write_volatile(P0_OUTSET, 1 << SCK);
            cortex_m::asm::nop();
            cortex_m::asm::nop();
            core::ptr::write_volatile(P0_OUTCLR, 1 << SCK);
        }

        core::ptr::write_volatile(P0_OUTSET, 1 << CS); // deassert → enter DPD

        for pin in [SCK, IO0, 22, 23, 24] {
            core::ptr::write_volatile(P0_PIN_CNF_BASE.add(pin as usize), DISCONNECT);
        }
    }
    log!("QSPI: Flash in Deep Power Down");
}

/// Enter System Off (deep sleep, ~5µA). Does not return; wake resets the chip.
///
/// The common sequence: wait for the wake button (P1.15) release, turn the
/// onboard RGB off, disconnect all GPIO, hold QSPI CS (P0.25) HIGH, re-arm the
/// wake button with SENSE LOW, clear LATCH, and power off.
///
/// Board-specific pins are passed as data:
/// - `early_off_p0`: P0 pins to drive LOW *before* teardown (e.g. boost SHDN, to
///   kill 5V before the disconnect Hi-Z window). Each must already be an output.
/// - `hold_p0`: P0 pins to reconfigure as outputs *after* teardown, as
///   `(pin, drive_high)` — e.g. boost SHDN and charge ISET held LOW.
///
/// # Safety
/// Does not return. The `SoftDevice` must be initialized. The named pins must be
/// valid for the board.
pub unsafe fn enter_system_off(early_off_p0: &[usize], hold_p0: &[(usize, bool)]) -> ! {
    const P0_OUTSET: *mut u32 = 0x5000_0508 as *mut u32;
    const P0_OUTCLR: *mut u32 = 0x5000_050C as *mut u32;
    const P0_PIN_CNF_BASE: *mut u32 = (0x5000_0000 + 0x700) as *mut u32;
    const P1_PIN_CNF_BASE: *mut u32 = (0x5000_0300 + 0x700) as *mut u32;
    const P1_IN: *const u32 = 0x5000_0810 as *const u32;

    const DISCONNECT: u32 = 0x0000_0002;
    const OUTPUT_CFG: u32 = 0x0000_0003; // DIR=output, INPUT=disconnected
    const WAKE_INPUT_SENSE: u32 = 0x0003_000C;
    const P0_LATCH: *mut u32 = 0x5000_0520 as *mut u32;
    const P1_LATCH: *mut u32 = 0x5000_0820 as *mut u32;

    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "the System Off teardown is one ordered sequence — kill 5V, tear \
                  down GPIO, re-establish only the held pins, clear LATCH, then \
                  power off. Each step is sound only in this order, so they share \
                  one justification rather than admitting individual ones"
    )]
    // SAFETY: every address is a fixed, word-aligned P0/P1 GPIO register, and
    // all pin indices are in range — the sweeps use the hardware's own 32/16 pin
    // counts, and `early_off_p0`/`hold_p0` are board constants supplied by the
    // caller, whose `# Safety` contract requires them to be valid for the board.
    // The caller also guarantees the SoftDevice is initialised, which is what
    // makes the `sd_power_system_off` SVC legal. This diverges, so no later code
    // can observe the torn-down pin state.
    unsafe {
        // Wait for the wake button release so re-arming SENSE LOW doesn't latch
        // immediately and refuse System Off.
        while core::ptr::read_volatile(P1_IN) & (1 << 15) == 0 {
            cortex_m::asm::nop();
        }

        // Drive board-specific pins LOW early (e.g. boost SHDN — kill 5V before
        // the disconnect Hi-Z window where a pull-up could re-enable it).
        for &pin in early_off_p0 {
            core::ptr::write_volatile(P0_OUTCLR, 1 << pin);
        }

        // Onboard RGB off (active low: HIGH = off): P0.26 (R), P0.30 (G), P0.06 (B)
        core::ptr::write_volatile(P0_OUTSET, (1 << 26) | (1 << 30) | (1 << 6));

        log!("SLEEP: Entering System Off");

        // Disconnect ALL GPIO, then reconfigure only the pins that must hold state.
        for pin in 0..32 {
            core::ptr::write_volatile(P0_PIN_CNF_BASE.add(pin), DISCONNECT);
        }
        for pin in 0..16 {
            core::ptr::write_volatile(P1_PIN_CNF_BASE.add(pin), DISCONNECT);
        }

        // P0.25: QSPI CS — output HIGH (keeps flash in Deep Power Down)
        core::ptr::write_volatile(P0_OUTSET, 1 << 25);
        core::ptr::write_volatile(P0_PIN_CNF_BASE.add(25), OUTPUT_CFG);

        // Board-specific held pins (e.g. boost SHDN LOW, charge ISET LOW).
        for &(pin, drive_high) in hold_p0 {
            if drive_high {
                core::ptr::write_volatile(P0_OUTSET, 1 << pin);
            } else {
                core::ptr::write_volatile(P0_OUTCLR, 1 << pin);
            }
            core::ptr::write_volatile(P0_PIN_CNF_BASE.add(pin), OUTPUT_CFG);
        }

        // Re-assert the Sense IMU/mic enables LOW: the disconnect loop above just
        // put them back to Hi-Z, and System Off is the longest stretch we'd be
        // paying for a rail that floated on.
        sense_peripherals_off();

        // P1.15: Wake button — input + pullup + SENSE LOW (0x0003_000C).
        core::ptr::write_volatile(P1_PIN_CNF_BASE.add(15), WAKE_INPUT_SENSE);

        // Clear LATCH bits — sd_power_system_off() refuses if any are pending.
        core::ptr::write_volatile(P0_LATCH, 0xFFFF_FFFF);
        core::ptr::write_volatile(P1_LATCH, 0xFFFF_FFFF);

        nrf_softdevice::raw::sd_power_system_off();
    }

    loop {
        cortex_m::asm::wfi();
    }
}
