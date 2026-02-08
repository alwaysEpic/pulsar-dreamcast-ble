//! Board-specific pin configurations.
//!
//! Abstracts hardware differences between development boards.

/// Pin configuration for nRF52840 DK.
pub mod nrf52840_dk {
    /// SDCKA (Maple Bus line A) - P0.05
    pub const SDCKA_PIN: u8 = 5;
    /// SDCKB (Maple Bus line B) - P0.06
    pub const SDCKB_PIN: u8 = 6;

    /// LED1 - P0.13 (active low)
    pub const LED1_PIN: u8 = 13;
    /// LED2 - P0.14 (active low)
    pub const LED2_PIN: u8 = 14;
    /// LED3 - P0.15 (active low)
    pub const LED3_PIN: u8 = 15;
    /// LED4 - P0.16 (active low)
    pub const LED4_PIN: u8 = 16;

    /// Button 1 - P0.11 (active low)
    pub const BUTTON1_PIN: u8 = 11;
}

/// Pin configuration for Adafruit nRF52840 Feather (future).
#[allow(dead_code)]
pub mod nrf52840_feather {
    /// SDCKA (Maple Bus line A) - TBD
    pub const SDCKA_PIN: u8 = 0; // TODO: Assign when hardware ready
    /// SDCKB (Maple Bus line B) - TBD
    pub const SDCKB_PIN: u8 = 0; // TODO: Assign when hardware ready

    /// Blue LED - P1.10
    pub const LED_PIN: u8 = 10; // On P1, not P0

    /// User button - TBD (Feather has no built-in button)
    pub const BUTTON_PIN: u8 = 0;
}

/// LED state abstraction (handles active-low vs active-high).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LedState {
    Off,
    On,
    Blink,
    FastBlink,
}

/// Board configuration trait for abstracting hardware differences.
pub trait BoardConfig {
    /// Returns true if LEDs are active-low (DK) vs active-high (Feather).
    fn leds_active_low() -> bool;
}

/// nRF52840 DK board configuration.
pub struct Nrf52840Dk;

impl BoardConfig for Nrf52840Dk {
    fn leds_active_low() -> bool {
        true
    }
}
