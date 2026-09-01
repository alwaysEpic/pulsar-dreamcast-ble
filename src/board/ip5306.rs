// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! IP5306-I²C power-management driver for the Pulsar V1 board.
//!
//! The IP5306 is an all-in-one power-bank IC — LiPo charger + synchronous 5 V
//! boost + coarse fuel gauge — controlled over I²C (addr `0x75`). On this board
//! it replaces the discrete boost + BQ25101 charger + SAADC divider of the XIAO
//! carrier. SDA = P0.04 (D4), SCL = P0.05 (D5); external 5.1 kΩ pull-ups (R1/R2).
//!
//! ✅ **Register map verified 2026-07-27** against the Injoinic *IP5306 寄存器文档*
//! V1.21 and the full IP5306-I²C datasheet V1.2 (which embeds the same register
//! section) — two independent sources, bit-for-bit agreement. The earlier map,
//! taken from the M5Stack / Arduino reference libraries, was wrong in one place
//! that mattered; see [`Ip5306::blocking_init`].
//!
//! **Charging is entirely a hardware function here.** Termination voltage lives
//! in `0x22[3:2]` (cell select, reset `00` = 4.2 V) with `0x22[1:0]` compensation
//! and `0x20[1:0]` full-stop; charge current is `0x24[4:0]`. This driver never
//! writes any of them, so termination stays at the part's factory default (plain
//! `IP5306` is the 4.20 V order code, datasheet V<sub>TRGT</sub> = 4.2 V). No
//! firmware bug can overcharge the pack.
//!
//! **Both source documents require read-modify-write.** Bits marked *Reserved*
//! "have special control functions; their original values must not be changed,
//! or unpredictable results will occur." Every access below reads first and
//! touches only the bits it owns — the previous blind writes clobbered the
//! reserved bits in `SYS_CTL0` (7:6 and 3) on every boot and every sleep.

use embassy_nrf::peripherals::TWISPI0;
use embassy_nrf::twim::{self, Twim};
use embassy_nrf::{bind_interrupts, gpio::Pin, Peri};
use static_cell::StaticCell;

/// IP5306 7-bit I²C address.
const ADDR: u8 = 0x75;

/// Boost/charger enable register.
const REG_SYS_CTL0: u8 = 0x00;
/// Charge in-progress register (bit3 = charging). ✅ Datasheet-confirmed.
const REG_READ0: u8 = 0x70;
/// Charge-complete register (bit3 = battery full). ✅ Datasheet-confirmed.
const REG_READ1: u8 = 0x71;
/// Coarse battery-level register (upper nibble = level). ⚠ **Undocumented** —
/// neither source lists a `0x78`, and the app notes state the IP5306 holds no
/// internal voltage or current information at all. Community reverse-engineering
/// only; see [`Ip5306::battery_percent`].
const REG_BAT_LEVEL: u8 = 0x78;

/// `SYS_CTL0` bit 5 — 5 V boost enable. Reset 1.
const SYS_CTL0_BOOST_EN: u8 = 1 << 5;
/// `SYS_CTL0` bit 4 — charger enable. Reset 1.
const SYS_CTL0_CHARGER_EN: u8 = 1 << 4;
/// `SYS_CTL0` bit 1 — *BOOST output always-on*. Reset 1.
///
/// Set, the 5 V output never auto-shuts-down; clear, it drops once the load
/// falls below the part's light-load threshold for the `SYS_CTL2[3:2]` dwell
/// (as short as 8 s). Both states are wanted, at different times — see
/// [`Ip5306::blocking_boost_off`].
const SYS_CTL0_BOOST_ALWAYS_ON: u8 = 1 << 1;
/// The bits this driver owns; everything else in `SYS_CTL0` is left as found.
///
/// Always-on is owned rather than merely preserved. Preserving it at the reset
/// value of 1 meant the rail could never be brought down at all: `boost_off`
/// cleared the enable bit while always-on kept the output up, so the controller
/// and VMU drew from the cell continuously — including through System Off. A
/// unit left plugged in then failed to charge, because the input current was
/// feeding that load instead of the pack (2026-08-17).
///
/// The rail has exactly two states, and every write lands one of them whole:
/// [`SYS_CTL0_RAIL_ON`] (all three set) or [`ctl0_rail_off`] (charger set,
/// boost and always-on clear). Nothing sets the enable without always-on, and
/// nothing clears one without the other.
const SYS_CTL0_OWNED: u8 = SYS_CTL0_BOOST_EN | SYS_CTL0_CHARGER_EN | SYS_CTL0_BOOST_ALWAYS_ON;
/// The owned bits as they read while the 5 V rail is up: boost enabled and held
/// always-on so the light-load timer cannot drop an idle-but-attached
/// controller mid-session, charger on.
const SYS_CTL0_RAIL_ON: u8 = SYS_CTL0_OWNED;
/// `SYS_CTL0` with the 5 V rail brought down: boost enable and always-on both
/// clear, charger kept on so a plugged-in unit charges whether it is idle in
/// Phase 1 or asleep. Clearing the enable alone is not enough — always-on holds
/// the output up regardless (see [`SYS_CTL0_OWNED`]).
const fn ctl0_rail_off(v: u8) -> u8 {
    (v & !(SYS_CTL0_BOOST_EN | SYS_CTL0_BOOST_ALWAYS_ON)) | SYS_CTL0_CHARGER_EN
}
/// Documented power-on reset value of `SYS_CTL0`: reserved 7:6 = `10`, boost,
/// charger, reserved bit 3, insert-load auto-power-on and boost-always-on all 1,
/// key-shutdown 0. Used only as the blind-write fallback in
/// [`Ip5306::blocking_boost_off`], so that even the degraded path lands the
/// documented reserved values instead of zeros.
const SYS_CTL0_RESET: u8 = 0xBE;

bind_interrupts!(struct Irqs {
    TWISPI0 => twim::InterruptHandler<TWISPI0>;
});

/// IP5306 power IC over I²C. Owns the TWIM peripheral.
pub struct Ip5306 {
    twim: Twim<'static>,
}

impl Ip5306 {
    /// Bring up the I²C peripheral on the IP5306 lines (SDA, SCL).
    #[expect(
        clippy::items_after_statements,
        reason = "function-local StaticCell for the DMA buffer"
    )]
    pub fn new(
        twspi: Peri<'static, TWISPI0>,
        sda: Peri<'static, impl Pin>,
        scl: Peri<'static, impl Pin>,
    ) -> Self {
        let config = twim::Config::default(); // 100 kHz; external pull-ups present
                                              // EasyDMA can't read flash, so const register-write slices stage through
                                              // this RAM buffer (sized for our largest transfer — 2 bytes).
        static RAM_BUF: StaticCell<[u8; 8]> = StaticCell::new();
        let twim = Twim::new(twspi, Irqs, sda, scl, config, RAM_BUF.init([0; 8]));
        Self { twim }
    }

    /// Read-modify-write `SYS_CTL0`, applying `f` to the current value. Returns
    /// `false` if either half of the exchange failed.
    ///
    /// The datasheet mandates this pattern: `SYS_CTL0` bits 7:6 and 3 are
    /// Reserved with non-zero reset values (`10` and `1`), and overwriting them
    /// is documented as producing undefined behaviour.
    fn blocking_update_ctl0(&mut self, f: impl FnOnce(u8) -> u8) -> bool {
        let mut cur = [0u8; 1];
        if self
            .twim
            .blocking_write_read(ADDR, &[REG_SYS_CTL0], &mut cur)
            .is_err()
        {
            return false;
        }
        self.twim
            .blocking_write(ADDR, &[REG_SYS_CTL0, f(cur[0])])
            .is_ok()
    }

    /// One-time boot configuration (blocking, so it runs immediately at startup):
    /// charger enabled, **5 V boost off**, preserving every other bit. The rail
    /// comes up only when a BLE host connects — [`Ip5306::blocking_boost_on`]
    /// from `Power::rail_on` — and goes back down on disconnect, which is
    /// ADR-005 ("boot → advertise with boost OFF") applied to this carrier. It
    /// matters here for charging rather than for idle draw: with the boost held
    /// up the whole time the board runs, a plugged-in unit feeds the controller
    /// instead of the pack, so charging while awake was effectively broken
    /// until the rail was gated (2026-08-18).
    ///
    /// The power-on reset value has the boost *on*, so on a cold boot (cell
    /// insertion) this genuinely brings the rail down; on a wake from System
    /// Off it is a no-op re-assertion of what [`Ip5306::blocking_boost_off`]
    /// already left. Best-effort — I²C errors are swallowed (nothing to recover
    /// to before the board exists), but they are reported rather than papered
    /// over, and the Phase 2 `rail_on` + `refresh_config` pair retries the
    /// write that actually matters at the first moment it can land.
    ///
    /// **No longer touches `SYS_CTL1`.** It used to clear `0x01` bit 1, believed
    /// to be the light-load (~<45 mA, ~32 s) auto-shutdown enable. Both source
    /// documents show `0x01` bit 1 is *Reserved, reset 0* — so that write was
    /// clearing an already-clear reserved bit: a no-op dressed up as the one
    /// setting that "must be right for the device to stay powered". Light-load
    /// behaviour is actually governed by `SYS_CTL0` bit 1 (*BOOST output
    /// always-on*, reset **1**) with the dwell in `SYS_CTL2[3:2]`; it is now
    /// owned outright, set with the enable and cleared with it. `SYS_CTL1`
    /// bit 0 — the Batlow 3.0 V low-battery shutdown — is reset-enabled and
    /// deliberately left alone.
    pub fn blocking_init(&mut self) {
        // Retry: the XIAO runs off LDO1 straight from +BATT, so it boots the
        // instant a battery is connected — potentially before the IP5306 has
        // finished its own power-on and can answer I2C. The previous version
        // swallowed that error and logged success anyway, leaving the chip in
        // whatever state it woke in and the log claiming otherwise. Observed
        // 2026-07-25 on a battery hot-plug (back when this write was the one
        // that enabled the boost; a silent failure then meant no rail at all).
        const ATTEMPTS: usize = 10;
        let mut ok = false;
        for _ in 0..ATTEMPTS {
            if self.blocking_update_ctl0(ctl0_rail_off) {
                ok = true;
                break;
            }
            // ~1ms of spin at 64MHz; no executor yet at this point in boot.
            cortex_m::asm::delay(64_000);
        }

        // Report what actually happened. A failure here is not fatal — the
        // Phase 2 `rail_on` and the periodic `refresh_config` will keep trying —
        // but silently claiming success cost an evening of debugging.
        if ok {
            crate::log!("IP5306: init OK (charger on, boost off until BLE connects)");
        } else {
            crate::log!("IP5306: init FAILED after {} attempts — no I2C", ATTEMPTS);
        }
    }

    /// Bring the 5 V rail up: boost enabled and always-on, charger on. Blocking
    /// RMW, so it can sit behind the synchronous `Power::rail_on` in the board
    /// contract; it fires at phase transitions (BLE connect, the Phase 1 VMU
    /// splashes), never per poll, so a few tens of µs on the I²C bus is fine.
    ///
    /// Best-effort: on I²C failure the log says so. On the connect path the
    /// Phase 2 `refresh_config` — which asserts the same bits — retries within
    /// the detect loop, so a transient NAK there does not strand a connection
    /// with no rail; the Phase 1 splash paths have no such retry and simply
    /// fail to render, which is their contract.
    pub fn blocking_boost_on(&mut self) {
        if self.blocking_update_ctl0(|v| v | SYS_CTL0_RAIL_ON) {
            crate::log!("IP5306: 5V boost on");
        } else {
            crate::log!("IP5306: 5V boost on FAILED — I2C error");
        }
    }

    /// Bring the 5 V rail down, keeping the charger enabled so a plugged-in
    /// board charges — whether it is idle in Phase 1 (`Power::rail_off` on BLE
    /// disconnect) or asleep (`Power::prepare_for_sleep` ahead of System Off).
    /// Blocking, best-effort (I²C errors swallowed; a failed disconnect-time
    /// write is retried by the next transition, and on the sleep path we're
    /// about to power off regardless).
    ///
    /// Safe to drop the 5 V rail: it only feeds the Dreamcast controller + rumble
    /// motor; the XIAO itself runs off LDO1 straight from the battery, so cutting
    /// the boost never removes MCU power. Re-enabled by [`Ip5306::blocking_boost_on`]
    /// when the next BLE host connects.
    ///
    /// **Clears [`SYS_CTL0_BOOST_ALWAYS_ON`] as well as the enable bit**, and that
    /// is the whole point. Clearing the enable alone did not bring the rail down:
    /// always-on holds the output up regardless, so the controller and VMU kept
    /// drawing from the cell right through System Off. A unit left plugged in
    /// then charged nothing, because the input current went to that load instead
    /// of the pack — while an older build, which blind-wrote `0x35` and so
    /// happened to leave always-on clear, charged normally. Diagnosed 2026-08-17
    /// by comparing the two units directly.
    ///
    /// `blocking_boost_on` and `refresh_config` set it again with the enable
    /// (both assert [`SYS_CTL0_RAIL_ON`]), so the rail is always-on whenever it
    /// is up at all — which is what keeps an idle-but-attached controller from
    /// being dropped by the light-load timer mid-session.
    pub fn blocking_boost_off(&mut self) {
        if self.blocking_update_ctl0(ctl0_rail_off) {
            crate::log!("IP5306: 5V boost off");
            return;
        }
        // The read failed. On the sleep path that means going into System Off
        // with a live boost draining the pack for however long the board
        // sleeps; on the disconnect path it means charging keeps competing with
        // the controller until the next transition. Both are worse than a blind
        // write, so fall back to one — but to the documented reset value with
        // the boost bits cleared, which at least lands the correct reserved
        // bits rather than zeros.
        let _ = self
            .twim
            .blocking_write(ADDR, &[REG_SYS_CTL0, ctl0_rail_off(SYS_CTL0_RESET)]);
        crate::log!("IP5306: 5V boost off — blind fallback, I2C read failed");
    }

    /// Re-assert the rail-up configuration, and report whether it had drifted.
    ///
    /// **Only meaningful while the rail should be up** — Phase 2 and Phase 3,
    /// which is where `main.rs` calls it. It asserts [`SYS_CTL0_RAIL_ON`], so a
    /// call from Phase 1 would silently undo `rail_off` and put the boost back
    /// in competition with the charger.
    ///
    /// [`Ip5306::blocking_boost_on`] writes `SYS_CTL0` once at BLE connect and
    /// nothing ever verifies it again. Anything that perturbs the chip —
    /// VIN insertion or removal being the obvious candidate — then leaves us
    /// running with unknown config indefinitely, with no path back to a good
    /// state. That is the failure mode this exists to close.
    ///
    /// Returns `true` if any of the boost, always-on or charger bits had actually been cleared —
    /// i.e. the config really had drifted. That distinguishes "this fixed it"
    /// from "this was never the problem", which matters when the alternative is
    /// guessing.
    ///
    /// The previous version tested `SYS_CTL1` bit 1, which both source documents
    /// show is Reserved with reset 0. Nothing sets it, so the check could only
    /// ever report "no drift" — the telemetry added specifically to tell those
    /// two cases apart was measuring a bit that never moves. It now watches the
    /// two bits whose loss actually kills the rail.
    pub async fn refresh_config(&mut self) -> bool {
        let mut cur = [0u8; 1];
        if self
            .twim
            .write_read(ADDR, &[REG_SYS_CTL0], &mut cur)
            .await
            .is_err()
        {
            return false;
        }
        if cur[0] & SYS_CTL0_RAIL_ON == SYS_CTL0_RAIL_ON {
            return false; // nothing to do; leave the register untouched
        }
        let _ = self
            .twim
            .write(ADDR, &[REG_SYS_CTL0, cur[0] | SYS_CTL0_RAIL_ON])
            .await;
        true
    }

    /// Coarse battery percentage (25/50/75/100) **and the raw `0x78` byte it was
    /// decoded from**. `None` on I²C error. ⚠ **Permanently unverifiable.**
    ///
    /// The decode treats bits 7:4 as the chip's 4 gauge LEDs, active-low
    /// (`0xF0` = none lit, `0x00` = all four), which is what the M5Stack /
    /// Arduino reference libraries do. The 2026-07-27 datasheet review
    /// established that this can never be confirmed: neither the Injoinic
    /// register document nor the full datasheet lists a `0x78` at all (they stop
    /// at `0x77`), and the app notes state outright that the IP5306 holds no
    /// internal voltage or current information — an external ADC-equipped MCU is
    /// the vendor's answer for battery management. So this is community
    /// reverse-engineering with no spec behind it, and a full LiPo reading 75 %
    /// has no authoritative explanation available.
    ///
    /// This is why `LOW_BATTERY_CUTOFF_PCT` (main.rs) sits at 0: it is the one
    /// bucket whose meaning survives any plausible decode. The raw byte comes
    /// back with the percentage so the caller can log it and characterize the
    /// map empirically across a charge/discharge — that remains the only route.
    pub async fn battery_percent(&mut self) -> Option<(u8, u8)> {
        let mut buf = [0u8; 1];
        self.twim
            .write_read(ADDR, &[REG_BAT_LEVEL], &mut buf)
            .await
            .ok()?;
        let percent = match buf[0] & 0xF0 {
            0x00 => 100,
            0x80 => 75,
            0xC0 => 50,
            0xE0 => 25,
            _ => 0,
        };
        Some((percent, buf[0]))
    }

    /// True while charging (charge in progress). `false` on error.
    /// ✅ `0x70` bit 3 — datasheet-confirmed, and named in its app notes as the
    /// intended way to tell charging from discharging.
    pub async fn is_charging(&mut self) -> bool {
        self.read_flag(REG_READ0, 0x08).await
    }

    /// True once charging has completed (battery full). `false` on error.
    /// ✅ `0x71` bit 3 — datasheet-confirmed.
    pub async fn is_full(&mut self) -> bool {
        self.read_flag(REG_READ1, 0x08).await
    }

    /// Read one register and test `mask`; `false` on I²C error.
    async fn read_flag(&mut self, reg: u8, mask: u8) -> bool {
        let mut buf = [0u8; 1];
        self.twim.write_read(ADDR, &[reg], &mut buf).await.is_ok() && (buf[0] & mask) != 0
    }
}
