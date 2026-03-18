// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Board support for the Seeed XIAO nRF52840.
//!
//! Pin assignments:
//! - SDCKA: P0.05 (D5), SDCKB: P0.03 (D1)
//! - RGB LED: R=P0.26, G=P0.30, B=P0.06 (all active LOW, internal)
//! - Sync button: P1.15 (D10, wired to VMU MODE button, doubles as wake source)
//! - Boost SHDN: P0.28 (D2, HIGH=on, LOW=shutdown)
//! - Battery ADC: P0.31 (internal, via P0.14 enable — future)

use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pin, Pull};
use embassy_nrf::saadc::{self, Saadc};
use embassy_nrf::Peri;
use embassy_time::{Duration, Timer};

/// SDCKA bit position in P0 GPIO register.
pub const PIN_A_BIT: u32 = 5; // P0.05 (D5)

/// SDCKB bit position in P0 GPIO register.
pub const PIN_B_BIT: u32 = 3; // P0.03 (D1)

/// Whether this board supports System Off sleep mode.
#[allow(dead_code)] // Part of board abstraction API
pub const SUPPORTS_SLEEP: bool = true;

/// Status LEDs for the main task (R and G channels of the RGB LED).
///
/// The blue channel is owned by the sync button task.
///
/// Color mapping:
/// - Searching: Red solid
/// - Controller found / BLE connected: Green solid
pub struct StatusLeds {
    led_r: Output<'static>,
    led_g: Output<'static>,
}

impl StatusLeds {
    /// Blink green for startup indication.
    pub async fn startup_blink(&mut self) {
        for _ in 0..3 {
            self.led_g.set_low();
            Timer::after(Duration::from_millis(100)).await;
            self.led_g.set_high();
            Timer::after(Duration::from_millis(100)).await;
        }
    }

    /// Indicate controller search in progress (red solid).
    pub fn show_searching(&mut self) {
        self.led_g.set_high();
        self.led_r.set_low();
    }

    /// Indicate controller found (green solid).
    pub fn show_controller_found(&mut self) {
        self.led_r.set_high();
        self.led_g.set_low();
    }

    /// Turn off all status LEDs.
    pub fn off(&mut self) {
        self.led_r.set_high();
        self.led_g.set_high();
    }

    /// Turn on TX activity indicator (no-op on XIAO to avoid flicker).
    #[allow(clippy::unused_self)] // Must match DK API
    pub fn tx_activity_on(&mut self) {}

    /// Turn off TX activity indicator (no-op on XIAO).
    #[allow(clippy::unused_self)] // Must match DK API
    pub fn tx_activity_off(&mut self) {}
}

/// Initialized board pins, ready for use by the main task.
pub struct BoardPins {
    pub sdcka: Flex<'static>,
    pub sdckb: Flex<'static>,
    pub sync_button: Input<'static>,
    pub sync_led: Output<'static>,
    pub status: StatusLeds,
    pub charge_stat: Input<'static>,
}

/// Initialize all board-specific pins.
///
/// The blue LED channel is passed out as `sync_led` for the sync button task.
/// The boost converter (SHDN pin) is enabled at init and stored in a static
/// for later shutdown during sleep. The charge current is set to 100mA (P0.13 LOW)
/// and the BQ25101 STAT pin (P0.17) is returned for charge status monitoring.
#[allow(clippy::similar_names)]
pub fn init_pins(
    sdcka_pin: Peri<'static, impl Pin>,
    sdckb_pin: Peri<'static, impl Pin>,
    led_r_pin: Peri<'static, impl Pin>,
    led_g_pin: Peri<'static, impl Pin>,
    led_b_pin: Peri<'static, impl Pin>,
    button_pin: Peri<'static, impl Pin>,
    boost_pin: Peri<'static, impl Pin>,
    charge_pin: Peri<'static, impl Pin>,
    charge_stat_pin: Peri<'static, impl Pin>,
) -> BoardPins {
    let sdcka = Flex::new(sdcka_pin);
    let sdckb = Flex::new(sdckb_pin);
    let sync_button = Input::new(button_pin, Pull::Up);

    let led_r = Output::new(led_r_pin, Level::High, OutputDrive::Standard);
    let led_g = Output::new(led_g_pin, Level::High, OutputDrive::Standard);
    let sync_led = Output::new(led_b_pin, Level::High, OutputDrive::Standard);

    // Start with boost converter OFF — enabled later when BLE connects
    let boost = Output::new(boost_pin, Level::Low, OutputDrive::Standard);
    // Store in static for sleep/shutdown access
    // SAFETY: Written once here, read only from main task during sleep entry
    unsafe {
        BOOST_CONTROL = Some(boost);
    }

    // Set charge current to 100mA (P0.13 LOW on XIAO BQ25101)
    // Pin config persists after drop — just need to set it once.
    let _charge = Output::new(charge_pin, Level::Low, OutputDrive::Standard);

    // BQ25101 STAT pin: LOW = charging, HIGH = not charging / full
    let charge_stat = Input::new(charge_stat_pin, Pull::Up);

    let status = StatusLeds { led_r, led_g };

    BoardPins {
        sdcka,
        sdckb,
        sync_button,
        sync_led,
        status,
        charge_stat,
    }
}

/// Static storage for the boost converter control pin.
/// Used during System Off entry to disable 5V output.
///
/// # Safety
/// Written once in `init_pins()`, read only from `disable_boost()` during
/// sleep entry. Both run on the main task of a single-core Cortex-M4 —
/// no concurrent access possible.
static mut BOOST_CONTROL: Option<Output<'static>> = None;

/// Enable the 5V boost converter (when BLE connects).
///
/// # Safety
/// Must only be called from the main task context, after `init_pins`.
pub unsafe fn enable_boost() {
    // SAFETY: See BOOST_CONTROL declaration — single writer, single reader, no concurrency.
    if let Some(ref mut boost) = BOOST_CONTROL {
        boost.set_high();
    }
}

/// Disable the 5V boost converter (on BLE disconnect or before System Off).
///
/// # Safety
/// Must only be called from the main task context, after `init_pins`.
pub unsafe fn disable_boost() {
    // SAFETY: See BOOST_CONTROL declaration — single writer, single reader, no concurrency.
    if let Some(ref mut boost) = BOOST_CONTROL {
        boost.set_low();
    }
}

/// Check if USB VBUS is present using the nRF52840 POWER peripheral.
///
/// When VBUS is detected, the controller can run from USB 5V through the
/// Schottky diode passthrough, so the boost converter is not needed.
pub fn is_usb_connected() -> bool {
    // POWER.USBREGSTATUS register, bit 0 = VBUSDETECT
    const POWER_USBREGSTATUS: *const u32 = 0x4000_0438 as *const u32;
    // SAFETY: Read-only register access, always valid on nRF52840
    (unsafe { core::ptr::read_volatile(POWER_USBREGSTATUS) } & 1) != 0
}

/// Battery voltage reader using SAADC on P0.31 (AIN7).
///
/// The XIAO has a 1M + 510K voltage divider on P0.31, with P0.14 as the low side.
/// P0.14 LOW = divider enabled (measuring), P0.14 HIGH = divider disabled (idle).
/// With the internal 0.6V reference and 1/6 gain, the SAADC input range is 0-3.6V.
/// Battery voltage = ADC voltage * (1M + 510K) / 510K ≈ ADC * 2.96.
pub struct BatteryReader<'d> {
    saadc: Saadc<'d, 1>,
    enable: Output<'d>,
    last_percent: u8,
}

impl<'d> BatteryReader<'d> {
    /// Create a new battery reader.
    ///
    /// `enable_pin` is P0.14 (LOW = enable divider, HIGH = disable).
    /// `adc_pin` is P0.31 (AIN7, battery voltage through divider).
    /// `saadc_peri` is the SAADC peripheral.
    pub fn new(
        enable_pin: Peri<'d, impl Pin>,
        adc_pin: impl saadc::Input + 'd,
        saadc_peri: Peri<'d, embassy_nrf::peripherals::SAADC>,
        irq: impl embassy_nrf::interrupt::typelevel::Binding<
                embassy_nrf::interrupt::typelevel::SAADC,
                saadc::InterruptHandler,
            > + 'd,
    ) -> Self {
        // Start with divider disabled (HIGH) to avoid current leak
        let enable = Output::new(enable_pin, Level::High, OutputDrive::Standard);
        let channel = saadc::ChannelConfig::single_ended(adc_pin);
        let mut config = saadc::Config::default();
        config.oversample = saadc::Oversample::OVER8X;
        let saadc = Saadc::new(saadc_peri, irq, config, [channel]);
        Self {
            saadc,
            enable,
            last_percent: 100,
        }
    }

    /// Read battery voltage and return `(millivolts, percentage)`.
    ///
    /// Enables the voltage divider (P0.14 LOW), takes a sample, disables (P0.14 HIGH).
    /// Uses 8x hardware oversampling for stable readings.
    ///
    /// 12-bit SAADC with 0.6V internal ref, 1/6 gain gives 0-3.6V range.
    /// Divider: 1M + 510K → V_adc = V_bat * 510 / 1510.
    ///
    /// When `charging` is false, percentage never increases (monotonic decrease)
    /// to avoid confusing voltage-recovery bounces. Resets when charging.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub async fn read(&mut self, charging: bool) -> (u32, u8) {
        self.enable.set_low(); // Enable divider
        Timer::after(Duration::from_micros(100)).await;

        let mut buf = [0i16; 1];
        self.saadc.sample(&mut buf).await;

        self.enable.set_high(); // Disable divider

        let raw = buf[0].max(0) as u32;
        // 12-bit resolution (0-4095), internal ref 0.6V, gain 1/6 → full scale 3.6V
        // ADC voltage (mV) = raw * 3600 / 4095
        // Battery voltage = ADC voltage * (1M + 510K) / 510K = ADC * 1510 / 510
        // Combined: v_bat_mv = raw * 3600 * 1510 / (4095 * 510) ≈ raw * 10663 / 4095
        let v_bat_mv = (u64::from(raw) * 10_663 / 4095) as u32;

        let mut percent = lipo_voltage_to_percent(v_bat_mv);

        if charging {
            self.last_percent = percent;
        } else {
            // Never report higher than previous reading when discharging
            percent = percent.min(self.last_percent);
            self.last_percent = percent;
        }

        crate::log!("BAT: {}mV {}%", v_bat_mv, percent);

        (v_bat_mv, percent)
    }
}

/// Convert battery voltage (mV) to percentage using a LiPo discharge curve.
///
/// Based on a typical single-cell LiPo discharge profile. The curve is
/// relatively flat from 3.7-3.9V and drops steeply below 3.5V.
/// Cutoff at 3300mV — the battery protection circuit shuts down around
/// this voltage (measured: device dies at ~3.3V under load).
/// Top clamped at 4100mV — charger often terminates before reaching
/// 4200mV, so anything above 4.1V reports as 100% to match user
/// expectations (same approach as phones/laptops).
/// Table entries: (millivolts, percentage).
#[allow(clippy::cast_possible_truncation)]
fn lipo_voltage_to_percent(mv: u32) -> u8 {
    // Voltage-to-percent lookup based on typical LiPo discharge curve.
    // 100% = 4100mV (charger terminates near here, report "full" early).
    // 0% = 3300mV (protection circuit cutoff, measured empirically).
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

    // Linear interpolation between table entries
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

/// Disconnect all GPIO pins to clear bootloader residue.
///
/// The UF2 bootloader may leave QSPI, NeoPixel, or LED pins configured,
/// which can draw current. This resets all P0 and P1 pins to input
/// disconnected (Hi-Z). Call once at early boot, before pin init.
///
/// Skips P0.28 (boost SHDN) to preserve its LOW state from System Off —
/// disconnecting it would let the Pololu's pull-up momentarily enable 5V.
///
/// # Safety
/// Must be called before any Embassy pin peripherals are configured, since
/// it writes directly to PIN_CNF registers.
pub unsafe fn disconnect_all_pins() {
    const P0_PIN_CNF_BASE: *mut u32 = (0x5000_0000 + 0x700) as *mut u32;
    const P1_PIN_CNF_BASE: *mut u32 = (0x5000_0300 + 0x700) as *mut u32;
    const DISCONNECT: u32 = 0x0000_0002;
    for pin in 0..32 {
        // Skip P0.28 (boost SHDN) — preserve LOW state from System Off.
        // Disconnecting it lets the Pololu's internal pull-up enable the
        // boost converter, causing a momentary 5V spike on wake.
        if pin == 28 {
            continue;
        }
        core::ptr::write_volatile(P0_PIN_CNF_BASE.add(pin), DISCONNECT);
    }
    for pin in 0..16 {
        core::ptr::write_volatile(P1_PIN_CNF_BASE.add(pin), DISCONNECT);
    }
}

/// Put the onboard P25Q16H QSPI flash into Deep Power Down mode.
///
/// Bit-bangs SPI to send the DPD command (0xB9) — avoids QSPI peripheral
/// configuration complexity. The flash draws 2-5 mA in standby; DPD drops
/// it to ~3 µA. Call once at startup.
///
/// After DPD, CS (P0.25) is kept driven HIGH to prevent accidental wake-up.
/// All other QSPI pins (SCK, IO0-IO3) are disconnected.
///
/// # Safety
/// Writes directly to GPIO peripheral registers.
pub unsafe fn qspi_flash_deep_power_down() {
    use crate::log;

    const P0_OUTSET: *mut u32 = 0x5000_0508 as *mut u32;
    const P0_OUTCLR: *mut u32 = 0x5000_050C as *mut u32;
    const P0_PIN_CNF_BASE: *mut u32 = (0x5000_0000 + 0x700) as *mut u32;

    // Pin assignments (XIAO nRF52840 QSPI flash)
    const CS: u32 = 25; // P0.25
    const SCK: u32 = 21; // P0.21
    const IO0: u32 = 20; // P0.20 (MOSI)

    // PIN_CNF: output, input disconnected, no pull, standard drive
    const CNF_OUTPUT: u32 = 0x0000_0003; // DIR=output, INPUT=disconnected

    // Configure CS, SCK, IO0 as outputs
    // CS starts HIGH (deasserted), SCK starts LOW (SPI mode 0)
    core::ptr::write_volatile(P0_OUTSET, 1 << CS);
    core::ptr::write_volatile(P0_OUTCLR, 1 << SCK);
    core::ptr::write_volatile(P0_PIN_CNF_BASE.add(CS as usize), CNF_OUTPUT);
    core::ptr::write_volatile(P0_PIN_CNF_BASE.add(SCK as usize), CNF_OUTPUT);
    core::ptr::write_volatile(P0_PIN_CNF_BASE.add(IO0 as usize), CNF_OUTPUT);

    // Assert CS (LOW)
    core::ptr::write_volatile(P0_OUTCLR, 1 << CS);

    // Clock out 0xB9 (DPD command), MSB first, SPI mode 0
    // Mode 0: data set on falling edge, sampled on rising edge
    const DPD_CMD: u8 = 0xB9;
    for i in (0..8).rev() {
        // Set MOSI
        if (DPD_CMD >> i) & 1 == 1 {
            core::ptr::write_volatile(P0_OUTSET, 1 << IO0);
        } else {
            core::ptr::write_volatile(P0_OUTCLR, 1 << IO0);
        }
        // Brief setup time
        cortex_m::asm::nop();
        cortex_m::asm::nop();
        // SCK HIGH (flash samples data)
        core::ptr::write_volatile(P0_OUTSET, 1 << SCK);
        cortex_m::asm::nop();
        cortex_m::asm::nop();
        // SCK LOW
        core::ptr::write_volatile(P0_OUTCLR, 1 << SCK);
    }

    // Deassert CS (HIGH) — flash enters DPD on CS rising edge
    core::ptr::write_volatile(P0_OUTSET, 1 << CS);

    // Keep CS driven HIGH to prevent accidental wake-up
    // Disconnect SCK, IO0, and the unused IO1-IO3 pins
    const DISCONNECT: u32 = 0x0000_0002; // input disconnected, no pull
    for pin in [SCK, IO0, 22, 23, 24] {
        core::ptr::write_volatile(P0_PIN_CNF_BASE.add(pin as usize), DISCONNECT);
    }

    log!("QSPI: Flash in Deep Power Down");
}

/// Enter System Off mode (deep sleep, ~5µA draw).
///
/// Configures the sync button (D10/P1.15) with GPIO SENSE for wake-on-press,
/// disables the 5V boost converter, then enters System Off via `SoftDevice`.
/// The sync button pin is already configured as input with pull-up by the
/// sync button task — we just add SENSE to it.
///
/// On wake, the device performs a full reset (boots fresh).
///
/// # Safety
/// This function does not return. The `SoftDevice` must be initialized.
pub unsafe fn enter_system_off() -> ! {
    use crate::log;

    // Turn off all LEDs (active low: HIGH = off)
    // P0 OUTSET register: set P0.26 (R), P0.30 (G), P0.06 (B)
    const P0_OUTSET: *mut u32 = 0x5000_0508 as *mut u32;
    core::ptr::write_volatile(P0_OUTSET, (1 << 26) | (1 << 30) | (1 << 6));

    log!("SLEEP: Entering System Off");
    disable_boost();

    // Disconnect ALL GPIO pins to minimize current draw in System Off.
    // GPIO state survives System Off — any pin left configured (by Embassy,
    // SoftDevice, or bootloader) could leak through external components.
    // Disconnect everything first, then re-configure only the pins we need.
    // Value 0x00000002 = input disconnected, no pull (Hi-Z, ~0 µA).
    const P0_PIN_CNF_BASE: *mut u32 = (0x5000_0000 + 0x700) as *mut u32;
    const P1_PIN_CNF_BASE: *mut u32 = (0x5000_0300 + 0x700) as *mut u32;
    const DISCONNECT: u32 = 0x0000_0002;
    for pin in 0..32 {
        core::ptr::write_volatile(P0_PIN_CNF_BASE.add(pin), DISCONNECT);
    }
    for pin in 0..16 {
        core::ptr::write_volatile(P1_PIN_CNF_BASE.add(pin), DISCONNECT);
    }

    // Re-configure the 4 pins that must maintain state:
    // P0.25: QSPI CS — output HIGH (keeps flash in Deep Power Down)
    const OUTPUT_HIGH: u32 = 0x0000_0003; // DIR=output, INPUT=disconnected
    core::ptr::write_volatile(P0_OUTSET, 1 << 25);
    core::ptr::write_volatile(P0_PIN_CNF_BASE.add(25), OUTPUT_HIGH);

    // P0.28: Boost SHDN — output LOW (keeps boost converter off)
    const P0_OUTCLR: *mut u32 = 0x5000_050C as *mut u32;
    core::ptr::write_volatile(P0_OUTCLR, 1 << 28);
    core::ptr::write_volatile(P0_PIN_CNF_BASE.add(28), OUTPUT_HIGH);

    // P0.13: Charge ISET — output LOW (BQ25101 at 100mA rate)
    core::ptr::write_volatile(P0_OUTCLR, 1 << 13);
    core::ptr::write_volatile(P0_PIN_CNF_BASE.add(13), OUTPUT_HIGH);

    // P1.15: Wake button — input with pull-up + SENSE LOW
    // 0x0003_000C = INPUT=connected, PULL=pullup(11), SENSE=low(11)
    const WAKE_INPUT_SENSE: u32 = 0x0003_000C;
    core::ptr::write_volatile(P1_PIN_CNF_BASE.add(15), WAKE_INPUT_SENSE);

    nrf_softdevice::raw::sd_power_system_off();

    // Should never reach here
    loop {
        cortex_m::asm::wfi();
    }
}
