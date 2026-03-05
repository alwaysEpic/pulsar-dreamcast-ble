// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Dreamcast controller state representation.
//!
//! Holds the parsed state from a `Get Condition` (`0x09`) response.

/// Maple Bus function code for standard controller.
pub const CONTROLLER_FUNCTION: u32 = 0x0000_0001;

/// Represents the state of a standard Dreamcast controller.
#[derive(Debug, Clone, Copy, Default)]
pub struct ControllerState {
    /// Digital button states (active LOW in protocol, but we store as active HIGH here).
    pub buttons: ButtonState,

    /// Left trigger analog value (0-255, 0 = released, 255 = fully pressed).
    pub trigger_l: u8,

    /// Right trigger analog value (0-255, 0 = released, 255 = fully pressed).
    pub trigger_r: u8,

    /// Analog stick X axis (0-255, 128 = center, 0 = left, 255 = right).
    pub stick_x: u8,

    /// Analog stick Y axis (0-255, 128 = center, 0 = up, 255 = down).
    pub stick_y: u8,
}

/// Digital button states from a Dreamcast controller.
/// Note: In the Maple protocol, buttons are active LOW (0 = pressed).
/// We invert them here so true = pressed for easier use.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ButtonState {
    pub c: bool,
    pub b: bool,
    pub a: bool,
    pub start: bool,
    pub dpad_up: bool,
    pub dpad_down: bool,
    pub dpad_left: bool,
    pub dpad_right: bool,
    pub z: bool,
    pub y: bool,
    pub x: bool,
    pub d: bool, // Second D button (rare)
                 // Bits 12-15 are typically unused on standard controllers
}

impl ButtonState {
    /// Parse button state from the first data word of a `Get Condition` response.
    /// The button bits are in the upper 16 bits of the first payload word.
    /// Buttons are active LOW in the protocol, so we invert.
    #[must_use]
    pub fn from_raw(raw: u16) -> Self {
        Self {
            c: (raw & (1 << 0)) == 0,
            b: (raw & (1 << 1)) == 0,
            a: (raw & (1 << 2)) == 0,
            start: (raw & (1 << 3)) == 0,
            dpad_up: (raw & (1 << 4)) == 0,
            dpad_down: (raw & (1 << 5)) == 0,
            dpad_left: (raw & (1 << 6)) == 0,
            dpad_right: (raw & (1 << 7)) == 0,
            z: (raw & (1 << 8)) == 0,
            y: (raw & (1 << 9)) == 0,
            x: (raw & (1 << 10)) == 0,
            d: (raw & (1 << 11)) == 0,
        }
    }

    /// Returns true if any button is currently pressed.
    #[must_use]
    pub fn any_pressed(&self) -> bool {
        self.c
            || self.b
            || self.a
            || self.start
            || self.dpad_up
            || self.dpad_down
            || self.dpad_left
            || self.dpad_right
            || self.z
            || self.y
            || self.x
            || self.d
    }

    /// Convert button state back to raw `u16` format for BLE transmission.
    /// Bit is set (1) when button is pressed (opposite of Maple protocol).
    #[must_use]
    pub fn to_raw(self) -> u16 {
        let mut raw: u16 = 0;
        if self.c {
            raw |= 1 << 0;
        }
        if self.b {
            raw |= 1 << 1;
        }
        if self.a {
            raw |= 1 << 2;
        }
        if self.start {
            raw |= 1 << 3;
        }
        if self.dpad_up {
            raw |= 1 << 4;
        }
        if self.dpad_down {
            raw |= 1 << 5;
        }
        if self.dpad_left {
            raw |= 1 << 6;
        }
        if self.dpad_right {
            raw |= 1 << 7;
        }
        if self.z {
            raw |= 1 << 8;
        }
        if self.y {
            raw |= 1 << 9;
        }
        if self.x {
            raw |= 1 << 10;
        }
        if self.d {
            raw |= 1 << 11;
        }
        raw
    }
}

/// Dreamcast analog stick center value (0-255 range).
const DC_STICK_CENTER: u8 = 128;

/// Xbox BLE stick center value (unsigned 16-bit, 0-65535 range).
const XBOX_STICK_CENTER: u16 = 32768;

/// Scale factor to convert Dreamcast stick (0-255) to Xbox stick (0-65535).
/// Maps 0->0, 128->32896~=32768, 255->65535.
const STICK_SCALE_FACTOR: u16 = 257;

/// Maximum Xbox trigger value (10-bit).
const XBOX_TRIGGER_MAX: u32 = 1023;

/// Maximum Dreamcast trigger value (8-bit).
const DC_TRIGGER_MAX: u32 = 255;

/// Trigger change threshold for `state_changed` detection.
const TRIGGER_CHANGE_THRESHOLD: i16 = 2;

/// Stick change threshold for `state_changed` detection.
const STICK_CHANGE_THRESHOLD: i16 = 2;

impl ControllerState {
    /// Convert to Xbox One S BLE HID gamepad report.
    ///
    /// Mapping:
    /// - Dreamcast A/B/X/Y -> Xbox Buttons 1-4
    /// - Dreamcast Start -> Xbox Button 8 (Menu)
    /// - Dreamcast D-pad -> Hat switch (1-8, 0=neutral)
    /// - Dreamcast analog stick -> Left stick (uint16, 0-65535, center=32768)
    /// - Dreamcast L/R triggers -> Brake/Accelerator (10-bit, 0-1023)
    #[must_use]
    #[allow(clippy::items_after_statements)]
    pub fn to_gamepad_report(self) -> crate::xbox_hid::GamepadReport {
        use crate::xbox_hid::{buttons, hat, GamepadReport};

        let mut btns: u16 = 0;
        if self.buttons.a {
            btns |= buttons::A;
        }
        if self.buttons.b {
            btns |= buttons::B;
        }
        if self.buttons.x {
            btns |= buttons::X;
        }
        if self.buttons.y {
            btns |= buttons::Y;
        }
        if self.buttons.start {
            btns |= buttons::START;
        }

        // D-pad -> Hat switch (Xbox convention: 1-8, 0=neutral)
        let hat_value = match (
            self.buttons.dpad_up,
            self.buttons.dpad_down,
            self.buttons.dpad_left,
            self.buttons.dpad_right,
        ) {
            (true, false, false, false) => hat::NORTH,
            (true, false, false, true) => hat::NORTH_EAST,
            (false, false, false, true) => hat::EAST,
            (false, true, false, true) => hat::SOUTH_EAST,
            (false, true, false, false) => hat::SOUTH,
            (false, true, true, false) => hat::SOUTH_WEST,
            (false, false, true, false) => hat::WEST,
            (true, false, true, false) => hat::NORTH_WEST,
            _ => hat::NEUTRAL,
        };

        // Convert Dreamcast stick (u8, 0-255, center=128) to Xbox (u16, 0-65535, center=32768)
        // Scale: multiply by 257 (maps 0->0, 128->32896~=32768, 255->65535)
        // Apply deadzone around center
        const DEADZONE: u16 = 5;
        let raw_x = self.stick_y; // Dreamcast Y -> HID X
        let raw_y = self.stick_x; // Dreamcast X -> HID Y
        let left_x: u16 =
            if (i16::from(raw_x) - i16::from(DC_STICK_CENTER)).unsigned_abs() < DEADZONE {
                XBOX_STICK_CENTER
            } else {
                u16::from(raw_x) * STICK_SCALE_FACTOR
            };
        let left_y: u16 =
            if (i16::from(raw_y) - i16::from(DC_STICK_CENTER)).unsigned_abs() < DEADZONE {
                XBOX_STICK_CENTER
            } else {
                u16::from(raw_y) * STICK_SCALE_FACTOR
            };

        // Convert triggers: 0-255 -> 0-1023 (10-bit)
        #[allow(clippy::cast_possible_truncation)]
        let left_trigger = (u32::from(self.trigger_l) * XBOX_TRIGGER_MAX / DC_TRIGGER_MAX) as u16;
        #[allow(clippy::cast_possible_truncation)]
        let right_trigger = (u32::from(self.trigger_r) * XBOX_TRIGGER_MAX / DC_TRIGGER_MAX) as u16;

        GamepadReport {
            left_x,
            left_y,
            left_trigger,
            right_trigger,
            hat: hat_value,
            buttons: btns,
        }
    }

    /// Parse controller state from a `Get Condition` response payload.
    ///
    /// Expected payload format (from command `0x09` response):
    /// - Word 0: Function type (should be `0x0000_0001` for controller)
    /// - Word 1: Buttons (upper 16 bits) + unused (lower 16 bits)
    /// - Word 2: Triggers (R in upper byte, L in next) + Stick X, Y
    ///
    /// Returns `None` if payload is too short or function type is wrong.
    #[must_use]
    pub fn from_payload(payload: &[u32]) -> Option<Self> {
        if payload.len() < 3 {
            return None;
        }

        // Word 0: Function type - must be standard controller
        let func_type = payload[0];
        if func_type != CONTROLLER_FUNCTION {
            return None; // Not a standard controller
        }

        // Word 1 format (bytes on wire): [trig_L, trig_R, btn_low, btn_high]
        // Assembled: trig_L | (trig_R << 8) | (btn_low << 16) | (btn_high << 24)
        // Raw values: 0x00 = released, 0xFF = fully pressed (no inversion needed)
        let word1 = payload[1];
        #[allow(clippy::cast_possible_truncation)]
        let trigger_l = (word1 & 0xFF) as u8;
        #[allow(clippy::cast_possible_truncation)]
        let trigger_r = ((word1 >> 8) & 0xFF) as u8;

        // Buttons in upper 16 bits, bytes swapped
        #[allow(clippy::cast_possible_truncation)]
        let buttons_word = ((word1 >> 16) & 0xFFFF) as u16;
        let buttons_raw = buttons_word.swap_bytes();
        let buttons = ButtonState::from_raw(buttons_raw);

        // Word 2: Analog sticks
        // Format: [unused, unused, stick_x, stick_y] (main stick in upper 16 bits)
        // Bytes 0-1 are for secondary stick (stays 0x80 on standard controller)
        let analog_word = payload[2];
        #[allow(clippy::cast_possible_truncation)]
        let stick_x = ((analog_word >> 16) & 0xFF) as u8;
        #[allow(clippy::cast_possible_truncation)]
        let stick_y = ((analog_word >> 24) & 0xFF) as u8;

        Some(Self {
            buttons,
            trigger_l,
            trigger_r,
            stick_x,
            stick_y,
        })
    }

    /// Check if the stick is roughly centered (within deadzone).
    #[must_use]
    pub fn stick_centered(&self, deadzone: u8) -> bool {
        let dx = (i16::from(self.stick_x) - i16::from(DC_STICK_CENTER)).unsigned_abs();
        let dy = (i16::from(self.stick_y) - i16::from(DC_STICK_CENTER)).unsigned_abs();
        dx <= u16::from(deadzone) && dy <= u16::from(deadzone)
    }

    /// Returns true if the controller state has changed meaningfully.
    ///
    /// Buttons use exact comparison, while triggers and sticks use
    /// thresholds to avoid noise-triggered updates.
    #[must_use]
    pub fn state_changed(&self, other: &Self) -> bool {
        if self.buttons.to_raw() != other.buttons.to_raw() {
            return true;
        }

        if (i16::from(self.trigger_l) - i16::from(other.trigger_l)).abs() > TRIGGER_CHANGE_THRESHOLD
            || (i16::from(self.trigger_r) - i16::from(other.trigger_r)).abs()
                > TRIGGER_CHANGE_THRESHOLD
        {
            return true;
        }

        if (i16::from(self.stick_x) - i16::from(other.stick_x)).abs() > STICK_CHANGE_THRESHOLD
            || (i16::from(self.stick_y) - i16::from(other.stick_y)).abs() > STICK_CHANGE_THRESHOLD
        {
            return true;
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_parse_none_pressed() {
        let buttons = ButtonState::from_raw(0xFFFF);
        assert!(!buttons.any_pressed());
    }

    #[test]
    fn button_parse_a_pressed() {
        let buttons = ButtonState::from_raw(0xFFFB);
        assert!(buttons.a);
        assert!(!buttons.b);
        assert!(!buttons.start);
    }

    #[test]
    fn controller_state_parse() {
        // Word 1: [trig_L, trig_R, btn_low, btn_high]
        // trig_L = 0xC8 (200), trig_R = 0x64 (100)
        // Buttons: A pressed (bit 2 low in active-low). After swap_bytes:
        //   upper 16 = btn_low | (btn_high << 8). We need after swap => 0xFFFB.
        //   So before swap: 0xFBFF. Upper 16 bits of word1 = 0xFBFF.
        // Word 1 = 0xFBFF_64C8
        //
        // Word 2: stick_x at >>16, stick_y at >>24
        // stick_x=64 (0x40), stick_y=200 (0xC8)
        // Word 2 = 0xC840_8080
        let payload = [
            0x0000_0001, // Function type: controller
            0xFBFF_64C8, // trig_L=200, trig_R=100, buttons=A pressed
            0xC840_8080, // stick_x=64, stick_y=200
        ];

        let state = ControllerState::from_payload(&payload).unwrap();
        assert!(state.buttons.a);
        assert!(!state.buttons.b);
        assert_eq!(state.trigger_l, 200);
        assert_eq!(state.trigger_r, 100);
        assert_eq!(state.stick_x, 64);
        assert_eq!(state.stick_y, 200);
    }

    #[test]
    fn button_roundtrip() {
        // Set some buttons
        let original = ButtonState::from_raw(0xFFF0); // up/down/left/right pressed
        let raw = original.to_raw();
        let restored = ButtonState::from_raw(!raw); // to_raw uses active-high, from_raw uses active-low
        assert_eq!(original.dpad_up, restored.dpad_up);
        assert_eq!(original.dpad_down, restored.dpad_down);
        assert_eq!(original.a, restored.a);
        assert_eq!(original.start, restored.start);
    }

    #[test]
    fn button_roundtrip_all_pressed() {
        let original = ButtonState::from_raw(0x0000); // all pressed (active low)
        assert!(original.any_pressed());
        let raw = original.to_raw();
        assert_eq!(raw, 0x0FFF); // 12 buttons all set
                                 // Invert back to active-low encoding
        let restored = ButtonState::from_raw(!raw);
        assert_eq!(original.a, restored.a);
        assert_eq!(original.b, restored.b);
        assert_eq!(original.x, restored.x);
        assert_eq!(original.y, restored.y);
    }

    #[test]
    fn from_payload_too_short() {
        assert!(ControllerState::from_payload(&[0x0000_0001, 0x0000_0000]).is_none());
        assert!(ControllerState::from_payload(&[]).is_none());
    }

    #[test]
    fn from_payload_wrong_function() {
        let payload = [
            0x0000_0002, // Not a controller
            0x0000_0000,
            0x0000_0000,
        ];
        assert!(ControllerState::from_payload(&payload).is_none());
    }

    #[test]
    fn stick_centered_in_deadzone() {
        let state = ControllerState {
            stick_x: 130, // 2 away from center
            stick_y: 126, // 2 away from center
            ..Default::default()
        };
        assert!(state.stick_centered(5));
    }

    #[test]
    fn stick_centered_outside_deadzone() {
        let state = ControllerState {
            stick_x: 200,
            stick_y: 128,
            ..Default::default()
        };
        assert!(!state.stick_centered(5));
    }

    #[test]
    fn to_gamepad_report_buttons() {
        let state = ControllerState {
            buttons: ButtonState {
                a: true,
                b: true,
                x: true,
                y: true,
                start: true,
                ..Default::default()
            },
            stick_x: 128,
            stick_y: 128,
            ..Default::default()
        };
        let report = state.to_gamepad_report();
        use crate::xbox_hid::buttons;
        assert_ne!(report.buttons & buttons::A, 0);
        assert_ne!(report.buttons & buttons::B, 0);
        assert_ne!(report.buttons & buttons::X, 0);
        assert_ne!(report.buttons & buttons::Y, 0);
        assert_ne!(report.buttons & buttons::START, 0);
        assert_eq!(report.buttons & buttons::LB, 0);
    }

    #[test]
    fn to_gamepad_report_dpad_all_directions() {
        use crate::xbox_hid::hat;

        let directions = [
            (true, false, false, false, hat::NORTH),
            (true, false, false, true, hat::NORTH_EAST),
            (false, false, false, true, hat::EAST),
            (false, true, false, true, hat::SOUTH_EAST),
            (false, true, false, false, hat::SOUTH),
            (false, true, true, false, hat::SOUTH_WEST),
            (false, false, true, false, hat::WEST),
            (true, false, true, false, hat::NORTH_WEST),
            (false, false, false, false, hat::NEUTRAL),
        ];

        for (up, down, left, right, expected_hat) in directions {
            let state = ControllerState {
                buttons: ButtonState {
                    dpad_up: up,
                    dpad_down: down,
                    dpad_left: left,
                    dpad_right: right,
                    ..Default::default()
                },
                stick_x: 128,
                stick_y: 128,
                ..Default::default()
            };
            let report = state.to_gamepad_report();
            assert_eq!(
                report.hat, expected_hat,
                "dpad ({up},{down},{left},{right}) should be hat {expected_hat}"
            );
        }
    }

    #[test]
    fn to_gamepad_report_triggers() {
        // 0 -> 0
        let state = ControllerState::default();
        let report = state.to_gamepad_report();
        assert_eq!(report.left_trigger, 0);
        assert_eq!(report.right_trigger, 0);

        // 255 -> 1023
        let state = ControllerState {
            trigger_l: 255,
            trigger_r: 255,
            stick_x: 128,
            stick_y: 128,
            ..Default::default()
        };
        let report = state.to_gamepad_report();
        assert_eq!(report.left_trigger, 1023);
        assert_eq!(report.right_trigger, 1023);
    }

    #[test]
    fn to_gamepad_report_sticks() {
        // Center -> 32768 (deadzone applied)
        let state = ControllerState {
            stick_x: 128,
            stick_y: 128,
            ..Default::default()
        };
        let report = state.to_gamepad_report();
        assert_eq!(report.left_x, 32768);
        assert_eq!(report.left_y, 32768);

        // Outside deadzone
        let state = ControllerState {
            stick_x: 0,
            stick_y: 255,
            ..Default::default()
        };
        let report = state.to_gamepad_report();
        // stick_y->left_x, stick_x->left_y (axis swap)
        assert_eq!(report.left_x, 255 * 257); // 65535
        assert_eq!(report.left_y, 0);
    }

    #[test]
    fn state_changed_buttons() {
        let a = ControllerState::default();
        let mut b = ControllerState::default();
        assert!(!a.state_changed(&b));

        b.buttons.a = true;
        assert!(a.state_changed(&b));
    }

    #[test]
    fn state_changed_trigger_within_threshold() {
        let a = ControllerState {
            trigger_l: 100,
            ..Default::default()
        };
        let b = ControllerState {
            trigger_l: 101, // within threshold of 2
            ..Default::default()
        };
        assert!(!a.state_changed(&b));
    }

    #[test]
    fn state_changed_trigger_outside_threshold() {
        let a = ControllerState {
            trigger_l: 100,
            ..Default::default()
        };
        let b = ControllerState {
            trigger_l: 105, // diff=5 > threshold 2
            ..Default::default()
        };
        assert!(a.state_changed(&b));
    }

    #[test]
    fn state_changed_stick() {
        let a = ControllerState {
            stick_x: 128,
            ..Default::default()
        };
        let b = ControllerState {
            stick_x: 135, // diff=7 > threshold 2
            ..Default::default()
        };
        assert!(a.state_changed(&b));

        let c = ControllerState {
            stick_x: 129, // diff=1 <= threshold 2
            ..Default::default()
        };
        assert!(!a.state_changed(&c));
    }
}
