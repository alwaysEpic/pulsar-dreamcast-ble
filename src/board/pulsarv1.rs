// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Board support for the **Pulsar V1** carrier (MapleLink hardware).
//!
//! XIAO nRF52840 module on a carrier with IP5306-I²C power, 5× WS2812B, and a
//! rumble motor. Reconstructed from the board netlist.
//!
//! Pin assignments (XIAO pads → nRF):
//! - SDCKA: **P0.02 (D0)**, SDCKB: **P0.03 (D1)**  ← moved off D5 vs the discrete carrier
//! - Sync/select button: P1.15 (D10, active-low, wake source)
//! - Status: **5× WS2812 chain on P0.28 (D2)** via PWM2 (LED0 = status, LED1-4 = battery);
//!   sync LED on the onboard blue channel (P0.06)
//!
//! Deferred (not grabbed by `init` yet):
//! - **IP5306 INT** P1.11 (D6) — optional charge-event IRQ; battery is polled for now
//!
//! Implements the board contract documented in [`super`]. The XIAO-module silicon
//! (QSPI DPD, System Off) comes from [`super::xiao_common`]; status is a 5-LED WS2812
//! bar ([`super::ws2812`]); `Power` is the IP5306 I²C driver ([`super::ip5306`]).

use super::{
    ip5306::Ip5306,
    ws2812::{Rgb, Ws2812, LED_COUNT},
    xiao_common, ConfigRefresh, Observation, Reading,
};
use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::peripherals::PWM1;
use embassy_nrf::pwm::{DutyCycle, SimpleConfig, SimplePwm};
use embassy_nrf::{Peri, Peripherals};
use embassy_time::{Duration, Timer};

/// SDCKA bit position in P0 GPIO register.
pub const PIN_A_BIT: u32 = 2; // P0.02 (D0)

/// SDCKB bit position in P0 GPIO register.
pub const PIN_B_BIT: u32 = 3; // P0.03 (D1)

/// This board deep-sleeps via System Off.
pub const SUPPORTS_SLEEP: bool = true;

/// No USB passthrough: +5 V is the IP5306's own boost, up whether or not
/// anything is plugged into the USB-C port.
///
/// There is no external supply for the controller rail to fall back on, so
/// `main.rs` never gates `rail_on`/`rail_off` on VBUS here — but the pair is
/// real all the same: it switches the IP5306 boost on BLE connect/disconnect
/// (ADR-005), which is what lets a plugged-in unit charge while idle instead
/// of feeding the controller.
pub const HAS_USB_PASSTHROUGH: bool = false;

// ── Status indicator: 5-LED WS2812 bar with Xbox-style player number ─────────

/// Dim status colors (kept low to avoid glare through the shell window).
const C_SEARCHING: Rgb = Rgb::new(12, 0, 0); // dim red
const C_CONNECTED: Rgb = Rgb::new(0, 14, 0); // dim green
const C_SAVING: Rgb = Rgb::new(12, 6, 0); // dim amber
const C_STARTUP: Rgb = Rgb::new(0, 0, 14); // dim blue

/// Battery-gauge color. Deliberately **magenta**, not another red/green/blue:
/// the bar now carries two unrelated meanings, so the battery LEDs must not be
/// mistakable for a status color — especially at these low brightnesses, where
/// cyan and green are easy to confuse. One const to change for a different hue.
const C_BATTERY: Rgb = Rgb::new(10, 0, 10); // dim magenta
/// An empty gauge: the first battery LED alone, red. Red rather than a dimmer
/// magenta because it has to read as a warning beside either status color —
/// green when the link is fine, red when nothing answers.
const C_BATTERY_EMPTY: Rgb = Rgb::new(12, 0, 0); // dim red

/// LED 0 shows connection status; LEDs 1-4 are the battery gauge.
///
/// The host cannot drive a player number here: the Xbox One S BLE HID profile
/// has **no player-LED output report** (real hardware assigns players over the
/// proprietary GIP protocol, not BLE), and the only Output report we receive is
/// rumble (ID 0x03). The four spare LEDs show battery instead, which maps 1:1
/// onto the IP5306's four gauge levels.
const STATUS_LED: usize = 0;
const BATTERY_LED_0: usize = 1;
const BATTERY_LED_COUNT: usize = 4;

/// WS2812-backed status indicator.
///
/// **LED 0 = status, LEDs 1-4 = battery.**
///
/// - `searching` → LED 0 dim red
/// - `connected` → LED 0 dim green
/// - `set_battery(Some(pct))` → LEDs 1-4, one per gauge level, dim magenta;
///   an empty gauge lights LED 1 dim red instead. `None` hides the gauge.
/// - `off` → all dark
///
/// Battery level is retained across status changes, so connecting or losing the
/// controller doesn't blank the gauge.
pub struct StatusIndicator {
    leds: Ws2812,
    status: Rgb,
    /// `None` = gauge hidden; `Some(0)` = shown and reading empty.
    battery_bars: Option<u8>,
    lit: bool,
}

impl StatusIndicator {
    /// Build from the configured WS2812 driver.
    #[must_use]
    pub const fn new(leds: Ws2812) -> Self {
        Self {
            leds,
            status: Rgb::OFF,
            battery_bars: None,
            lit: false,
        }
    }

    /// Compose the whole strip: status on LED 0, battery on LEDs 1-4. One write
    /// for both meanings — the chain is a single DMA sequence, so there is no
    /// such thing as a partial update anyway.
    fn render(&mut self) {
        let mut frame = [Rgb::OFF; LED_COUNT];
        if self.lit {
            frame[STATUS_LED] = self.status;
            match self.battery_bars {
                None => {}
                // Shown and empty. Zero lit bars is also what a hidden gauge
                // looks like, so "charge me" was indistinguishable from "not
                // telling you" — the exact case `C_BATTERY_EMPTY` exists for.
                Some(0) => frame[BATTERY_LED_0] = C_BATTERY_EMPTY,
                Some(bars) => {
                    for i in 0..(bars as usize).min(BATTERY_LED_COUNT) {
                        frame[BATTERY_LED_0 + i] = C_BATTERY;
                    }
                }
            }
        }
        self.leds.write(&frame);
    }

    /// Blue sweep across the bar at startup.
    pub async fn startup(&mut self) {
        for i in 0..LED_COUNT {
            let mut frame = [Rgb::OFF; LED_COUNT];
            frame[i] = C_STARTUP;
            self.leds.write(&frame);
            Timer::after(Duration::from_millis(80)).await;
        }
        self.leds.write(&[Rgb::OFF; LED_COUNT]);
    }

    /// Controller search in progress — status LED dim red.
    pub fn searching(&mut self) {
        self.status = C_SEARCHING;
        self.lit = true;
        self.render();
    }

    /// Connected — status LED dim green.
    pub fn connected(&mut self) {
        self.status = C_CONNECTED;
        self.lit = true;
        self.render();
    }

    /// Acked saves draining onto the VMU — status LED dim amber, so the unit
    /// is visibly not to be undocked or held off. `connected` ends it.
    pub fn saving(&mut self) {
        self.status = C_SAVING;
        self.lit = true;
        self.render();
    }

    /// All LEDs off. Battery level is remembered for the next `render`.
    pub fn off(&mut self) {
        self.lit = false;
        self.render();
    }

    /// Update the battery gauge (LEDs 1-4). `None` hides it entirely.
    ///
    /// Hidden whenever a VMU is docked: the VMU already draws the same gauge, so
    /// lighting these too is duplicate information *and* wasted current (~1.6 mA
    /// per lit LED, ~6 mA at 4 bars). With no VMU the bar is the only battery
    /// readout the user has, so it comes on.
    ///
    /// Uses the **same** bucketing as the VMU icon (`vmu::bars_for_percent`), so
    /// the two can never disagree. No-op when nothing changed, so neither the
    /// 60s battery read nor the presence probe pushes a redundant WS2812 DMA
    /// sequence into the poll loop.
    pub fn set_battery(&mut self, percent: Option<u8>) {
        let bars = percent.map(crate::vmu::bars_for_percent);
        if bars != self.battery_bars {
            self.battery_bars = bars;
            self.render();
        }
    }

    /// TX activity — no-op (the bar shows status/player, not per-poll flicker).
    pub const fn tx_activity_on(&mut self) {}

    /// TX activity off — no-op.
    pub const fn tx_activity_off(&mut self) {}
}

// ── Power: IP5306 I²C (charge + boost + coarse fuel gauge) ────────────────────

/// Power subsystem backed by the IP5306-I²C IC.
///
/// The 5 V boost is switched over I²C — up only while a BLE host is connected
/// (`rail_on`/`rail_off`, ADR-005), re-asserted periodically while up, and
/// down again for System Off. Charge state and battery level are read over
/// the same bus.
pub struct Power {
    ip5306: Ip5306,
    /// Charge/power state cached from the last `battery()` read, so the frequent,
    /// synchronous `is_charging()` / `is_externally_powered()` never trigger an
    /// I²C transfer in the poll loop.
    charging: bool,
    powered: bool,
}

impl Power {
    /// Enable the IP5306 boost (on BLE connect, and ahead of the Phase 1 VMU
    /// splashes). Blocking RMW — a phase-transition write, never per poll.
    ///
    /// Until 2026-08-18 this was a no-op and the boost ran from `blocking_init`
    /// onward, so a plugged-in unit fed the controller instead of the pack and
    /// charging while awake was effectively broken. The rail is not USB-gated
    /// here (`HAS_USB_PASSTHROUGH` is false) — it is BLE-gated, per ADR-005.
    pub fn rail_on(&mut self) {
        self.ip5306.blocking_boost_on();
    }

    /// Disable the IP5306 boost (on BLE disconnect). Charger stays on, so an
    /// idle plugged-in unit charges the pack rather than the controller.
    ///
    /// The VMU is unpowered while the rail is down: any Phase 1 splash (BYE,
    /// DFU BOOT) has to `rail_on()` and settle first — see `main.rs`.
    pub fn rail_off(&mut self) {
        self.ip5306.blocking_boost_off();
    }

    /// Power the 5 V boost down before System Off so it can't drain the battery
    /// while asleep (the deep-discharge path). The XIAO keeps running off LDO1,
    /// so this only powers down the controller rail. Charger stays on for
    /// charge-while-asleep. Same write as `rail_off` — usually already landed
    /// (sleep is entered from Phase 1, or from Phase 3 after `rail_off`), but
    /// the Phase 1 goodbye path brings the rail up for the BYE splash, so this
    /// is not redundant.
    pub fn prepare_for_sleep(&mut self) {
        self.ip5306.blocking_boost_off();
    }

    /// Input power present (charging, or topped-off while plugged), cached from
    /// the last `battery()` read (no I²C on this path). Both underlying bits
    /// (`0x70[3]`, `0x71[3]`) are datasheet-confirmed.
    ///
    /// **Unconsumed on this board.** Both `main.rs` call sites now sit behind
    /// `HAS_USB_PASSTHROUGH`, which is `false` here, so this const-folds away.
    /// Kept for board-contract conformance. The `is_full()` I²C read that
    /// maintains `self.powered` is not only for this: it is also the
    /// `charge_complete` observation behind the on-cable 100 %.
    #[must_use]
    pub const fn is_externally_powered(&self) -> bool {
        self.powered
    }

    /// Re-assert the rail-up IP5306 configuration and say what happened. See [`Ip5306::refresh_config`] — the register is otherwise
    /// written at connect/disconnect and never checked between. **Phase 2/3
    /// only**: it asserts the boost, so it would undo `rail_off` in Phase 1.
    pub async fn refresh_config(&mut self) -> ConfigRefresh {
        self.ip5306.refresh_config().await
    }

    /// Charge state, cached from the last `battery()` read (no I²C on this path).
    #[must_use]
    pub const fn is_charging(&self) -> bool {
        self.charging
    }

    /// Sample the IP5306 fuel gauge. Coarse (25/50/75/100 %); no millivolts.
    ///
    /// Publishes the raw `0x78` byte alongside the decode and the charge flags
    /// so the gauge map (still ⚠ UNVERIFIED — see [`Ip5306::battery_percent`])
    /// can be characterized across a full charge/discharge.
    ///
    /// Two channels, because **RTT does not work on this board** — the XIAO has
    /// no onboard debugger and there is no SWD probe for it:
    /// - `log!` — useful only on a DK or with a probe attached. Free otherwise
    ///   (compiles out without `rtt`).
    /// - `gauge-debug` feature — smuggles the sample out in the HID report's
    ///   unused right-stick bytes, which is the only channel that works
    ///   untethered on battery, i.e. in the condition being characterized.
    ///
    /// Reads are 60 s apart, so neither costs anything in the poll loop.
    pub async fn battery(&mut self) -> Reading {
        // Three independent reads, three independent facts. Each is reported
        // on its own: a failed charge-flag read must not become "not
        // charging", an unexplained gauge byte must not become "0 %" — and a
        // failed read must not throw away the ones that landed, which is how a
        // level reading "empty" once vanished from the DFU gate (2026-09-21).
        // The cached flags move only on a read that landed.
        let charging = self.ip5306.is_charging().await;
        let full = self.ip5306.is_full().await;
        let gauge = self.ip5306.battery_percent().await;
        if let Some(charging) = charging {
            self.charging = charging;
        }
        // "Externally powered" = plugged in: either charging, or topped-off
        // (full) with input present. Good enough without a dedicated VIN bit.
        if let (Some(charging), Some(full)) = (charging, full) {
            self.powered = charging || full;
        }
        crate::log!(
            "IP5306: bat 0x78={:?} (chg={:?} full={:?})",
            gauge,
            charging,
            full
        );
        // Published *before* anything is filtered: an unrecognized byte is
        // exactly what a characterization run is looking for.
        #[cfg(feature = "gauge-debug")]
        crate::publish_gauge_sample(gauge.map(|g| (g.raw, g.percent)), charging, full);
        // `percent` stays the decoded gauge level. `full` (`0x71[3]`, the
        // datasheet-confirmed charge-complete bit) goes out beside it rather
        // than being folded in, so the cutoff keeps the measurement and only
        // the displays apply `Snapshot::shown_percent`. A failed read is
        // `None` — "not known to be complete", never "not full".
        Reading::Observed(Observation {
            charging,
            charge_complete: full,
            percent: gauge.and_then(|g| g.percent),
            millivolts: None,
        })
    }
}

// ── Rumble: ERM motor on P0.29 (D3 → R5 → Q1 → CN3 motor), PWM1 ──────────────

/// Rumble motor driver — PWM1 sets the motor duty (intensity).
///
/// ⚠ PWM polarity/frequency to verify on hardware: the motor is low-side driven
/// by Q1, so a higher duty on `VIBRATOR_EN` = stronger buzz.
pub struct Rumble {
    pwm: SimplePwm<'static>,
    max_duty: u16,
}

impl Rumble {
    /// Configure PWM1 on the rumble pin (starts off).
    #[must_use]
    pub fn new(
        pwm: Peri<'static, PWM1>,
        pin: Peri<'static, embassy_nrf::peripherals::P0_29>,
    ) -> Self {
        let config = SimpleConfig::default();
        let max_duty = config.max_duty;
        Self {
            pwm: SimplePwm::new_1ch(pwm, pin, &config),
            max_duty,
        }
    }

    /// Drive the motor at `intensity` (0 = off, 255 = full). Inverted polarity so
    /// on-time ∝ intensity.
    ///
    /// ✅ **Polarity confirmed 2026-07-27 from the netlist + API, no scope.**
    /// Board netlist: P0.29 → R5 → Q1 pin 1 (base); Q1 pin 2 (emitter) →
    /// `GND`; Q1 pin 3 (collector) → CN3 pin 2 and D4 anode; D4 cathode → `+5V`
    /// (flyback); CN3 pin 1 → `+5V`. An NPN low-side switch with the motor across
    /// +5 V and the collector, so **base HIGH = motor ON**. embassy's
    /// `DutyCycle::inverted` sets the output high while the counter is *below*
    /// the duty value, making high-time = duty ∝ intensity. Ends check out:
    /// 255 → duty = `max_duty` → 100 %; 0 → duty 0 → line always low → off.
    ///
    /// ⚠ **Never run on hardware** — no motor has been connected to CN3 (open
    /// since Phase 4, 2026-07-02; deliberately deferred, rumble is a "maybe"
    /// feature behind core stability). The reasoning above proves the firmware
    /// matches the *intended* wiring, not that the board was assembled to match —
    /// that is a multimeter continuity check, not a scope capture.
    ///
    /// ⚠ **Frequency is 1 kHz and nobody chose it.** `SimpleConfig::default()` is
    /// `Prescaler::Div16` with `max_duty: 1000` → 16 MHz / 16 / 1000. That is
    /// audible; small ERMs whine at 1 kHz, and rumble drivers normally sit at
    /// 20–30 kHz to stay above hearing. Decide this deliberately before shipping
    /// a motor. (`ch0_idle_level: Level::Low` is correct — motor off when idle.)
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the product is bounded by max_duty, which the PWM peripheral caps at 15 bits"
    )]
    pub fn set(&mut self, intensity: u8) {
        let duty = (u32::from(intensity) * u32::from(self.max_duty) / 255) as u16;
        self.pwm.set_duty(0, DutyCycle::inverted(duty));
    }
}

/// Initialized board pins, ready for use by the main task.
pub struct BoardPins {
    pub sdcka: Flex<'static>,
    pub sdckb: Flex<'static>,
    pub sync_button: Input<'static>,
    pub sync_led: Output<'static>,
    pub status: StatusIndicator,
    pub power: Power,
    pub rumble: Rumble,
    /// The SPIM pair, trigger and clock pins for the hardware reply capture
    /// (`maple::spim_capture`). This board's only RX capture backend, so
    /// always `Some` — `board-pulsarv1` implies `spim-capture`. `None`
    /// only on a board with no pins to spare for the clocks, which reads
    /// replies with the CPU sampling loop instead (dk).
    #[cfg(feature = "spim-capture")]
    pub spim_capture: Option<crate::maple::spim_capture::Parts>,
}

/// Board-specific Embassy config: enable the DC/DC regulator (REG1).
pub const fn configure_embassy(config: &mut embassy_nrf::config::Config) {
    xiao_common::configure_dcdc(config);
}

/// Pre-Embassy silicon housekeeping: clear bootloader pin residue, then park
/// the onboard QSPI flash in Deep Power Down. No boost-EN pin to preserve.
///
/// # Safety
/// Writes directly to `PIN_CNF/GPIO` registers; call once at early boot before
/// any Embassy pin peripherals are configured.
pub unsafe fn early_init() {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "one early-boot housekeeping sequence, ordered: pins are parked \
                  before the QSPI lines are bit-banged into deep power down"
    )]
    // SAFETY: each callee requires that it run at early boot before Embassy
    // claims any pin, which is exactly this function's own `# Safety` contract
    // (above) — so the obligation is passed straight through to our caller.
    unsafe {
        xiao_common::disconnect_all_pins(None);
        xiao_common::sense_peripherals_off();
        xiao_common::qspi_flash_deep_power_down();
    }
}

/// Initialize the Phase-1 board pins from the HAL singletons.
///
/// Maple on D0/D1, sync on D10, status on the onboard RGB. The I²C / NeoPixel /
/// rumble pins are left in `p` for their drivers to claim in later phases.
#[expect(
    clippy::similar_names,
    reason = "sdcka/sdckb are the Maple Bus signal names from the protocol spec; renaming them would break the correspondence to the wiring"
)]
#[must_use]
pub fn init(p: Peripherals) -> BoardPins {
    let sdcka = Flex::new(p.P0_02);
    let sdckb = Flex::new(p.P0_03);
    let sync_button = Input::new(p.P1_15, Pull::Up);

    let sync_led = Output::new(p.P0_06, Level::High, OutputDrive::Standard);

    // 5-LED WS2812 status bar on P0.28 (D2), driven by PWM2 (PWM0 is the Maple TX).
    let leds = Ws2812::new(p.PWM2, p.P0_28);

    // IP5306 power IC on I²C (SDA P0.04, SCL P0.05). Charger on, 5 V boost off
    // until a BLE host connects (`rail_on`), preserving every other bit (the
    // datasheet requires read-modify-write; `SYS_CTL0` has reserved bits with
    // non-zero resets).
    let mut ip5306 = Ip5306::new(p.TWISPI0, p.P0_04, p.P0_05);
    ip5306.blocking_init();

    // Rumble motor on P0.29 (D3), driven by PWM1.
    let rumble = Rumble::new(p.PWM1, p.P0_29);

    // Hardware reply capture : SPIM1/SPIM2 listen on the Maple
    // lines by PSEL; their clocks go out on P1.01 and P1.02, which the XIAO
    // nRF52840 module routes to no pad (Seeed's schematic for the module lists
    // P1.11–P1.15 as the only port-1 pins, and the Sense variant's mic and
    // IMU sit on P1.00, P0.16, P0.07, P0.27, P0.11 and P1.08; the Plus, whose
    // pads do include P1.01, is not this module). SPIM0 is the IP5306's I²C,
    // SPIM3 has anomaly 198.
    #[cfg(feature = "spim-capture")]
    let spim_capture = Some(crate::maple::spim_capture::Parts {
        spim_a: p.TWISPI1,
        spim_b: p.SPI2,
        gpiote: p.GPIOTE_CH0,
        ppi: p.PPI_CH0,
        ppi_oneshot: p.PPI_CH1,
        ppi_group: p.PPI_GROUP0,
        sck_a: p.P1_01.into(),
        sck_b: p.P1_02.into(),
    });

    BoardPins {
        sdcka,
        sdckb,
        sync_button,
        sync_led,
        status: StatusIndicator::new(leds),
        power: Power {
            ip5306,
            charging: false,
            powered: false,
        },
        rumble,
        #[cfg(feature = "spim-capture")]
        spim_capture,
    }
}

/// Enter System Off.
///
/// No boost/charge pins to hold — the 5 V boost is powered
/// down over I²C by [`Power::prepare_for_sleep`] just before this is called, and
/// the IP5306 keeps managing charge/battery independently. Does not return.
///
/// # Safety
/// Does not return. The `SoftDevice` must be initialized.
pub unsafe fn enter_sleep() -> ! {
    // SAFETY: `enter_system_off` requires an initialised SoftDevice and pin
    // lists valid for this board — the former is this function's own `# Safety`
    // contract, and pulsarv1 has no pins to force off or hold (the 5 V boost is
    // powered down over I2C by `Power::prepare_for_sleep` beforehand), so the
    // empty slices are correct rather than merely convenient.
    unsafe { xiao_common::enter_system_off(&[], &[]) }
}
