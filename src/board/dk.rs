// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Board support for the nRF52840-DK development kit.
//!
//! Pin assignments:
//! - SDCKA: P0.05, SDCKB: P0.06
//! - Sync LED (LED1): P0.13; Status LEDs (LED2-4): P0.14-P0.16 (active LOW)
//! - Button 4 (sync): P0.25 (active LOW, internal pull-up)
//!
//! Bench board: no boost rail, no battery gauge, no sleep. Implements the board
//! contract documented in [`super`] with no-op / `None` power and a WFI-halt
//! `enter_sleep`.

use super::BatteryStatus;
use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::Peripherals;
use embassy_time::{Duration, Timer};

/// SDCKA bit position in P0 GPIO register.
pub const PIN_A_BIT: u32 = 5; // P0.05

/// SDCKB bit position in P0 GPIO register.
pub const PIN_B_BIT: u32 = 6; // P0.06

/// The dev kit has no battery/power-off path.
pub const SUPPORTS_SLEEP: bool = false;

/// No switchable rail on the DK, so nothing for a passthrough to replace.
pub const HAS_USB_PASSTHROUGH: bool = false;

/// Logical status indicator backed by DK LED2-LED4 (active LOW).
pub struct StatusIndicator {
    led2: Output<'static>,
    led3: Output<'static>,
    led4: Output<'static>,
}

impl StatusIndicator {
    /// Blink LED2 a few times at startup.
    pub async fn startup(&mut self) {
        for _ in 0..3 {
            self.led2.set_low();
            Timer::after(Duration::from_millis(100)).await;
            self.led2.set_high();
            Timer::after(Duration::from_millis(100)).await;
        }
    }

    /// Controller search in progress (LED4 on).
    pub fn searching(&mut self) {
        self.led4.set_low();
    }

    /// Controller found / connected (LED4 off, LED3 on).
    pub fn connected(&mut self) {
        self.led4.set_high();
        self.led3.set_low();
    }

    /// All status LEDs off.
    pub fn off(&mut self) {
        self.led2.set_high();
        self.led3.set_high();
        self.led4.set_high();
    }

    /// Battery gauge — no-op. DK LEDs are discrete status only; no battery gauge.
    #[allow(clippy::unused_self)] // uniform contract API
    pub fn set_battery(&mut self, _percent: Option<u8>) {}

    /// TX activity indicator on (LED2).
    pub fn tx_activity_on(&mut self) {
        self.led2.set_low();
    }

    /// TX activity indicator off (LED2).
    pub fn tx_activity_off(&mut self) {
        self.led2.set_high();
    }
}

/// Power subsystem — the DK has none; every method is a no-op / `None`.
pub struct Power;

impl Power {
    /// No boost rail on the DK.
    #[allow(clippy::unused_self)] // uniform contract API
    pub fn rail_on(&mut self) {}

    /// No boost rail on the DK.
    #[allow(clippy::unused_self)] // uniform contract API
    pub fn rail_off(&mut self) {}

    /// No configuration to re-assert. No power IC on the DK.
    #[allow(clippy::unused_self, clippy::unused_async)] // uniform contract API
    pub async fn refresh_config(&mut self) -> bool {
        false
    }

    /// No boost rail on the DK — nothing to power down for sleep.
    #[allow(clippy::unused_self)] // uniform contract API
    pub fn prepare_for_sleep(&mut self) {}

    /// The DK runs from the debugger USB; report not externally battery-gated.
    #[allow(clippy::unused_self)] // uniform contract API
    pub fn is_externally_powered(&self) -> bool {
        false
    }

    /// No charge circuit on the DK.
    #[allow(clippy::unused_self)] // uniform contract API
    pub fn is_charging(&self) -> bool {
        false
    }

    /// No battery gauge on the DK.
    #[allow(clippy::unused_self, clippy::unused_async)] // uniform contract API
    pub async fn battery(&mut self) -> Option<BatteryStatus> {
        None
    }
}

/// Rumble motor — the DK has none; a no-op.
pub struct Rumble;

impl Rumble {
    /// No motor on the DK.
    #[allow(clippy::unused_self)] // uniform contract API
    pub fn set(&mut self, _intensity: u8) {}
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

/// No board-specific Embassy config on the DK.
#[allow(clippy::missing_const_for_fn)] // uniform contract API (other boards mutate config)
pub fn configure_embassy(_config: &mut embassy_nrf::config::Config) {}

/// No silicon housekeeping needed on the DK.
///
/// # Safety
/// No-op; the signature mirrors the board contract.
pub unsafe fn early_init() {}

/// Initialize all board pins from the HAL singletons.
///
/// The sync LED (LED1) is separated so it can be moved into the sync button task.
#[allow(clippy::similar_names)]
pub fn init(p: Peripherals) -> BoardPins {
    let sdcka = Flex::new(p.P0_05);
    let sdckb = Flex::new(p.P0_06);
    let sync_button = Input::new(p.P0_25, Pull::Up);

    let sync_led = Output::new(p.P0_13, Level::High, OutputDrive::Standard);
    let led2 = Output::new(p.P0_14, Level::High, OutputDrive::Standard);
    let led3 = Output::new(p.P0_15, Level::High, OutputDrive::Standard);
    let led4 = Output::new(p.P0_16, Level::High, OutputDrive::Standard);

    BoardPins {
        sdcka,
        sdckb,
        sync_button,
        sync_led,
        status: StatusIndicator { led2, led3, led4 },
        power: Power,
        rumble: Rumble,
    }
}

/// The DK has no System Off path — halt via WFI.
///
/// Only reached on the explicit goodbye flow (`SUPPORTS_SLEEP` gates the
/// automatic timeouts off), matching the dev kit's original behavior.
///
/// # Safety
/// Does not return.
pub unsafe fn enter_sleep() -> ! {
    loop {
        cortex_m::asm::wfi();
    }
}
