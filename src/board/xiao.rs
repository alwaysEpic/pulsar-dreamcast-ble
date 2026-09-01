// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Board support for the Seeed XIAO nRF52840 (discrete-power carrier).
//!
//! Pin assignments:
//! - SDCKA: P0.05 (D5), SDCKB: P0.03 (D1)
//! - RGB LED: R=P0.26, G=P0.30, B=P0.06 (all active LOW, internal)
//! - Sync button: P1.15 (D10, wired to VMU MODE button, doubles as wake source)
//! - Boost SHDN: P0.28 (D2, HIGH=on, LOW=shutdown)
//! - Battery ADC: P0.31 (AIN7, via P0.14 divider enable)
//! - Charge: P0.13 (ISET, LOW=100mA), P0.17 (BQ25101 STAT, LOW=charging)
//!
//! Implements the board contract documented in [`super`]. The onboard-RGB status
//! indicator and the XIAO-module silicon (QSPI DPD, System Off) come from
//! [`super::xiao_common`]; this file adds the discrete-power `Power` subsystem.

use super::{xiao_common, BatteryStatus};
use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::saadc::{self, Saadc};
use embassy_nrf::Peripherals;
use embassy_time::{Duration, Timer};

/// SDCKA bit position in P0 GPIO register.
pub const PIN_A_BIT: u32 = 5; // P0.05 (D5)

/// SDCKB bit position in P0 GPIO register.
pub const PIN_B_BIT: u32 = 3; // P0.03 (D1)

/// This board deep-sleeps via System Off.
pub const SUPPORTS_SLEEP: bool = true;

/// This board can run the controller rail straight off USB.
///
/// The discrete carrier feeds USB 5 V to the controller rail through a Schottky
/// diode, so while VBUS is present the boost can stay off. This is the *only*
/// board with that path.
pub const HAS_USB_PASSTHROUGH: bool = true;

/// Onboard-RGB status indicator (searching = red solid, connected = green solid).
pub struct StatusIndicator {
    led_r: Output<'static>,
    led_g: Output<'static>,
}

impl StatusIndicator {
    /// Build from the configured R and G channel outputs (active LOW).
    #[must_use]
    pub const fn new(led_r: Output<'static>, led_g: Output<'static>) -> Self {
        Self { led_r, led_g }
    }

    /// Blink green a few times at startup.
    pub async fn startup(&mut self) {
        for _ in 0..3 {
            self.led_g.set_low();
            Timer::after(Duration::from_millis(100)).await;
            self.led_g.set_high();
            Timer::after(Duration::from_millis(100)).await;
        }
    }

    /// Controller search in progress (red solid).
    pub fn searching(&mut self) {
        self.led_g.set_high();
        self.led_r.set_low();
    }

    /// Controller found / connected (green solid).
    pub fn connected(&mut self) {
        self.led_r.set_high();
        self.led_g.set_low();
    }

    /// All status LEDs off.
    pub fn off(&mut self) {
        self.led_r.set_high();
        self.led_g.set_high();
    }

    /// Battery gauge — no-op. Onboard RGB is a single LED — no room for a gauge.
    pub const fn set_battery(&mut self, _percent: Option<u8>) {}

    /// TX activity indicator — no-op on XIAO (avoids flicker).
    pub const fn tx_activity_on(&mut self) {}

    /// TX activity indicator off — no-op.
    pub const fn tx_activity_off(&mut self) {}
}

embassy_nrf::bind_interrupts!(struct SaadcIrqs {
    SAADC => embassy_nrf::saadc::InterruptHandler;
});

// ── Power: 5V boost rail + BQ25101 charge status + SAADC battery gauge ───────

/// Power subsystem: discrete boost rail, charge-status pin, SAADC fuel gauge.
pub struct Power {
    boost: Output<'static>,
    charge_stat: Input<'static>,
    battery: BatteryReader,
}

impl Power {
    /// Enable the 5V boost converter (on BLE connect).
    pub fn rail_on(&mut self) {
        self.boost.set_high();
    }

    /// Disable the 5V boost converter (on disconnect / before sleep).
    pub fn rail_off(&mut self) {
        self.boost.set_low();
    }

    /// No configuration to re-assert. Discrete boost has no I²C config to drift.
    #[expect(
        clippy::unused_async,
        reason = "the board contract (ADR-013) fixes this signature so all three boards expose \
              one API; this board answers without awaiting"
    )]
    pub async fn refresh_config(&mut self) -> bool {
        false
    }

    /// Boost-off for sleep is handled inside [`enter_sleep`] (P0.28 SHDN held LOW
    /// through System Off), so there is nothing extra to do here.
    pub const fn prepare_for_sleep(&mut self) {}

    /// USB VBUS present → controller can run off USB 5V (boost not needed).
    #[must_use]
    pub fn is_externally_powered(&self) -> bool {
        is_usb_connected()
    }

    /// BQ25101 STAT: LOW = charging.
    #[must_use]
    pub fn is_charging(&self) -> bool {
        self.charge_stat.is_low()
    }

    /// Sample the battery. Returns voltage, `SoC` %, and charge state.
    pub async fn battery(&mut self) -> Option<BatteryStatus> {
        let charging = self.charge_stat.is_low();
        let (millivolts, percent) = self.battery.read(charging).await;
        Some(BatteryStatus {
            millivolts,
            percent,
            charging,
        })
    }
}

/// Rumble motor — the discrete carrier has none; a no-op.
pub struct Rumble;

impl Rumble {
    /// No motor on the XIAO carrier.
    pub const fn set(&mut self, _intensity: u8) {}
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
}

/// Board-specific Embassy config: enable the DC/DC regulator (REG1).
pub const fn configure_embassy(config: &mut embassy_nrf::config::Config) {
    xiao_common::configure_dcdc(config);
}

/// Pre-Embassy silicon housekeeping: clear bootloader pin residue (skipping the
/// boost-SHDN pin), then park the onboard QSPI flash in Deep Power Down.
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
    // above — so the obligation passes straight through to our caller. P0.28 is
    // skipped deliberately: a Hi-Z there lets the Pololu's pull-up re-enable 5V.
    unsafe {
        // Skip P0.28 (boost SHDN) so its LOW state survives — a Hi-Z here lets the
        // Pololu's pull-up momentarily enable 5V.
        xiao_common::disconnect_all_pins(Some(28));
        xiao_common::sense_peripherals_off();
        xiao_common::qspi_flash_deep_power_down();
    }
}

/// Initialize all board pins and peripherals from the HAL singletons.
///
/// The boost starts OFF (enabled later on BLE connect). Charge current is set
/// to 100 mA (P0.13 LOW). The blue LED channel becomes `sync_led`.
#[expect(
    clippy::similar_names,
    reason = "sdcka/sdckb are the Maple Bus signal names from the protocol spec; renaming them would break the correspondence to the wiring"
)]
#[must_use]
pub fn init(p: Peripherals) -> BoardPins {
    let sdcka = Flex::new(p.P0_05);
    let sdckb = Flex::new(p.P0_03);
    let sync_button = Input::new(p.P1_15, Pull::Up);

    let led_r = Output::new(p.P0_26, Level::High, OutputDrive::Standard);
    let led_g = Output::new(p.P0_30, Level::High, OutputDrive::Standard);
    let sync_led = Output::new(p.P0_06, Level::High, OutputDrive::Standard);

    // Boost OFF at boot — enabled when BLE connects.
    let boost = Output::new(p.P0_28, Level::Low, OutputDrive::Standard);

    // Charge current 100 mA (P0.13 LOW). Config persists after drop.
    let _charge = Output::new(p.P0_13, Level::Low, OutputDrive::Standard);
    let charge_stat = Input::new(p.P0_17, Pull::Up);

    let battery = BatteryReader::new(p.P0_14, p.P0_31, p.SAADC);

    BoardPins {
        sdcka,
        sdckb,
        sync_button,
        sync_led,
        status: StatusIndicator::new(led_r, led_g),
        power: Power {
            boost,
            charge_stat,
            battery,
        },
        rumble: Rumble,
    }
}

/// Enter System Off. Kills the boost rail early and holds boost SHDN (P0.28) +
/// charge ISET (P0.13) LOW through sleep. Does not return.
///
/// # Safety
/// Does not return. The `SoftDevice` must be initialized.
pub unsafe fn enter_sleep() -> ! {
    // SAFETY: `enter_system_off` requires an initialised SoftDevice and pin lists
    // valid for this board — the former is this function's own `# Safety`
    // contract, and P0.28 (boost) / P0.13 are the carrier's boost-SHDN and
    // charge-ISET nets, which must be driven LOW across sleep.
    unsafe {
        // early_off: P0.28 boost off before teardown; hold: P0.28 + P0.13 LOW.
        xiao_common::enter_system_off(&[28], &[(28, false), (13, false)])
    }
}

/// USB VBUS presence via the nRF52840 POWER peripheral.
fn is_usb_connected() -> bool {
    // POWER.USBREGSTATUS register, bit 0 = VBUSDETECT
    const POWER_USBREGSTATUS: *const u32 = 0x4000_0438 as *const u32;
    // SAFETY: Read-only register access, always valid on nRF52840
    (unsafe { core::ptr::read_volatile(POWER_USBREGSTATUS) } & 1) != 0
}

// ── Battery gauge (SAADC on P0.31 via P0.14 divider enable) ──────────────────

/// Battery voltage reader using SAADC on P0.31 (AIN7).
///
/// 1M + 510K divider on P0.31, P0.14 as low-side enable (LOW = measuring).
/// Internal 0.6V ref, 1/6 gain → 0-3.6V input range;
/// battery mV = ADC * (1M + 510K) / 510K ≈ ADC * 2.96.
struct BatteryReader {
    saadc: Saadc<'static, 1>,
    enable: Output<'static>,
    last_percent: u8,
}

impl BatteryReader {
    fn new(
        enable_pin: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P0_14>,
        adc_pin: embassy_nrf::Peri<'static, embassy_nrf::peripherals::P0_31>,
        saadc_peri: embassy_nrf::Peri<'static, embassy_nrf::peripherals::SAADC>,
    ) -> Self {
        // Divider disabled (HIGH) at rest to avoid current leak.
        let enable = Output::new(enable_pin, Level::High, OutputDrive::Standard);
        let channel = saadc::ChannelConfig::single_ended(adc_pin);
        let mut config = saadc::Config::default();
        config.oversample = saadc::Oversample::OVER8X;
        let saadc = Saadc::new(saadc_peri, SaadcIrqs, config, [channel]);
        Self {
            saadc,
            enable,
            last_percent: 100,
        }
    }

    /// Read battery voltage, return `(millivolts, percent)`.
    ///
    /// When `charging` is false the percentage is monotonic-decreasing to hide
    /// voltage-recovery bounces; it resets while charging.
    #[expect(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "SAADC counts are non-negative once the divider settles, and the percentage returned by lipo_voltage_to_percent is 0-100 by construction"
    )]
    async fn read(&mut self, charging: bool) -> (u32, u8) {
        self.enable.set_low(); // enable divider
        Timer::after(Duration::from_micros(100)).await;

        let mut buf = [0i16; 1];
        self.saadc.sample(&mut buf).await;

        self.enable.set_high(); // disable divider

        let raw = buf[0].max(0) as u32;
        // v_bat_mv = raw * 3600 * 1510 / (4095 * 510) ≈ raw * 10663 / 4095
        let v_bat_mv = (u64::from(raw) * 10_663 / 4095) as u32;

        let mut percent = lipo_voltage_to_percent(v_bat_mv);
        if !charging {
            // Discharge is monotonic: never let the reported level climb back up
            // on a noisy read. Charging is allowed to move it either way.
            percent = percent.min(self.last_percent);
        }
        self.last_percent = percent;

        crate::log!("BAT: {}mV {}%", v_bat_mv, percent);
        (v_bat_mv, percent)
    }
}

/// LiPo voltage (mV) → percentage. 100% ≥ 4100mV, 0% ≤ 3300mV (measured cutoff).
#[expect(
    clippy::cast_possible_truncation,
    reason = "every percentage in TABLE is 0-100, so the interpolated result fits u8"
)]
fn lipo_voltage_to_percent(mv: u32) -> u8 {
    const TABLE: [(u32, u8); 9] = [
        (4100, 100),
        (4000, 80),
        (3900, 60),
        (3800, 40),
        (3700, 30),
        (3600, 20),
        (3500, 10),
        (3400, 5),
        (3300, 0),
    ];
    if mv >= TABLE[0].0 {
        return 100;
    }
    if mv <= TABLE[TABLE.len() - 1].0 {
        return 0;
    }
    for i in 0..TABLE.len() - 1 {
        let (v_hi, p_hi) = TABLE[i];
        let (v_lo, p_lo) = TABLE[i + 1];
        if mv >= v_lo {
            let range_mv = v_hi - v_lo;
            let range_pct = u32::from(p_hi) - u32::from(p_lo);
            let offset = mv - v_lo;
            return (u32::from(p_lo) + offset * range_pct / range_mv) as u8;
        }
    }
    0
}
