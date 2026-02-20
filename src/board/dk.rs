//! Board support for the nRF52840-DK development kit.
//!
//! Pin assignments:
//! - SDCKA: P0.05, SDCKB: P0.06
//! - LEDs 1-4: P0.13-P0.16 (active LOW)
//! - Button 4 (sync): P0.25 (active LOW, internal pull-up)

use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pin, Pull};
use embassy_nrf::Peri;
use embassy_time::{Duration, Timer};

/// SDCKA bit position in P0 GPIO register.
pub const PIN_A_BIT: u32 = 5; // P0.05

/// SDCKB bit position in P0 GPIO register.
pub const PIN_B_BIT: u32 = 6; // P0.06

/// Whether this board supports System Off sleep mode.
#[allow(dead_code)] // Part of board abstraction API
pub const SUPPORTS_SLEEP: bool = false;

/// Status LEDs for the main task (LED2-LED4, active LOW on DK).
pub struct StatusLeds {
    led2: Output<'static>,
    led3: Output<'static>,
    led4: Output<'static>,
}

impl StatusLeds {
    /// Blink for startup indication.
    pub async fn startup_blink(&mut self) {
        for _ in 0..3 {
            self.led2.set_low();
            Timer::after(Duration::from_millis(100)).await;
            self.led2.set_high();
            Timer::after(Duration::from_millis(100)).await;
        }
    }

    /// Indicate controller search in progress (LED4 on).
    pub fn show_searching(&mut self) {
        self.led4.set_low();
    }

    /// Indicate controller found (LED4 off, LED3 on).
    pub fn show_controller_found(&mut self) {
        self.led4.set_high();
        self.led3.set_low();
    }

    /// Turn off all status LEDs.
    pub fn off(&mut self) {
        self.led2.set_high();
        self.led3.set_high();
        self.led4.set_high();
    }

    /// Turn on TX activity indicator (LED2).
    pub fn tx_activity_on(&mut self) {
        self.led2.set_low();
    }

    /// Turn off TX activity indicator (LED2).
    pub fn tx_activity_off(&mut self) {
        self.led2.set_high();
    }
}

/// Initialized board pins, ready for use by the main task.
pub struct BoardPins {
    pub sdcka: Flex<'static>,
    pub sdckb: Flex<'static>,
    pub sync_button: Input<'static>,
    pub sync_led: Output<'static>,
    pub status: StatusLeds,
}

/// Initialize all board-specific pins.
///
/// The sync LED (LED1) is separated so it can be moved into the sync button task.
#[allow(clippy::similar_names)]
pub fn init_pins(
    sdcka_pin: Peri<'static, impl Pin>,
    sdckb_pin: Peri<'static, impl Pin>,
    led1_pin: Peri<'static, impl Pin>,
    led2_pin: Peri<'static, impl Pin>,
    led3_pin: Peri<'static, impl Pin>,
    led4_pin: Peri<'static, impl Pin>,
    button_pin: Peri<'static, impl Pin>,
) -> BoardPins {
    let sdcka = Flex::new(sdcka_pin);
    let sdckb = Flex::new(sdckb_pin);
    let sync_button = Input::new(button_pin, Pull::Up);

    let sync_led = Output::new(led1_pin, Level::High, OutputDrive::Standard);
    let led2 = Output::new(led2_pin, Level::High, OutputDrive::Standard);
    let led3 = Output::new(led3_pin, Level::High, OutputDrive::Standard);
    let led4 = Output::new(led4_pin, Level::High, OutputDrive::Standard);

    let status = StatusLeds { led2, led3, led4 };

    BoardPins {
        sdcka,
        sdckb,
        sync_button,
        sync_led,
        status,
    }
}
