//! Bluetooth Low Energy module for Dreamcast controller adapter.
//!
//! Uses nRF SoftDevice S140 for BLE peripheral functionality.
//! Implements HID over GATT (HOG) for standard gamepad support.

pub mod hid;
pub mod security;
pub mod softdevice;

// Keep old gatt module for now but don't use it
#[allow(dead_code)]
pub mod gatt;

pub use hid::{GamepadReport, GamepadServer};
pub use security::Bonder;
pub use softdevice::init_softdevice;
