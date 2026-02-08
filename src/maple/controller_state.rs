//! Dreamcast controller state representation.
//!
//! Holds the parsed state from a Get Condition (0x09) response.

#![allow(dead_code)] // Used by get_condition (upcoming feature)

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
    /// Parse button state from the first data word of a Get Condition response.
    /// The button bits are in the upper 16 bits of the first payload word.
    /// Buttons are active LOW in the protocol, so we invert.
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

    /// Convert button state back to raw u16 format for BLE transmission.
    /// Bit is set (1) when button is pressed (opposite of Maple protocol).
    pub fn to_raw(&self) -> u16 {
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

impl ControllerState {
    /// Serialize controller state to 12-byte BLE characteristic format (legacy).
    #[allow(dead_code)]
    pub fn to_ble_bytes(&self) -> [u8; 12] {
        let buttons_raw = self.buttons.to_raw();
        [
            (buttons_raw & 0xFF) as u8,
            (buttons_raw >> 8) as u8,
            self.trigger_l,
            self.trigger_r,
            self.stick_x,
            self.stick_y,
            0, 0, 0, 0, 0, 0,
        ]
    }

    /// Convert to HID Gamepad report.
    ///
    /// Mapping:
    /// - Dreamcast A/B/X/Y → Buttons 0-3
    /// - Dreamcast Start → Button 7
    /// - Dreamcast D-pad → Buttons 12-15 (Up, Down, Left, Right)
    /// - Dreamcast analog stick → Left stick (signed, -127 to 127)
    /// - Dreamcast L/R triggers → Trigger axes (signed, -127 to 127)
    /// - Right stick always centered (Dreamcast doesn't have one)
    pub fn to_gamepad_report(&self) -> crate::ble::hid::GamepadReport {
        use crate::ble::hid::{buttons, GamepadReport};

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
        // D-pad as buttons 12-15
        if self.buttons.dpad_up {
            btns |= buttons::DPAD_UP;
        }
        if self.buttons.dpad_down {
            btns |= buttons::DPAD_DOWN;
        }
        if self.buttons.dpad_left {
            btns |= buttons::DPAD_LEFT;
        }
        if self.buttons.dpad_right {
            btns |= buttons::DPAD_RIGHT;
        }

        // Convert unsigned 0-255 (center=128) to signed -127 to 127 (center=0)
        let left_x = (self.stick_x as i16 - 128).clamp(-127, 127) as i8;
        let left_y = (self.stick_y as i16 - 128).clamp(-127, 127) as i8;

        // Convert triggers: 0-255 → -127 to 127
        // Dreamcast: 0=pressed, 255=released (after inversion in from_payload)
        // We need: -127=released, 127=pressed, so invert the mapping
        let left_trigger = (128i16 - self.trigger_l as i16).clamp(-127, 127) as i8;
        let right_trigger = (128i16 - self.trigger_r as i16).clamp(-127, 127) as i8;

        GamepadReport {
            buttons: btns,
            left_x,
            left_y,
            right_x: 0,  // Centered - Dreamcast has no right stick
            right_y: 0,
            left_trigger,
            right_trigger,
        }
    }

    /// Parse controller state from a Get Condition response payload.
    ///
    /// Expected payload format (from command 0x09 response):
    /// - Word 0: Function type (should be 0x00000001 for controller)
    /// - Word 1: Buttons (upper 16 bits) + unused (lower 16 bits)
    /// - Word 2: Triggers (R in upper byte, L in next) + Stick X, Y
    ///
    /// Returns None if payload is too short or function type is wrong.
    pub fn from_payload(payload: &[u32]) -> Option<Self> {
        if payload.len() < 3 {
            return None;
        }

        // Word 0: Function type - 0x00000001 = standard controller
        let func_type = payload[0];
        if func_type != 0x00000001 {
            return None; // Not a standard controller
        }

        // Word 1 format (bytes on wire): [trig_L, trig_R, btn_low, btn_high]
        // Assembled: trig_L | (trig_R << 8) | (btn_low << 16) | (btn_high << 24)
        // Triggers are inverted: 0xFF = released, 0x00 = fully pressed
        let word1 = payload[1];
        let trigger_l_raw = (word1 & 0xFF) as u8;
        let trigger_r_raw = ((word1 >> 8) & 0xFF) as u8;
        // Invert triggers: 0xFF->0, 0x00->255
        let trigger_l = 255 - trigger_l_raw;
        let trigger_r = 255 - trigger_r_raw;

        // Buttons in upper 16 bits, bytes swapped
        let buttons_word = ((word1 >> 16) & 0xFFFF) as u16;
        let buttons_raw = buttons_word.swap_bytes();
        let buttons = ButtonState::from_raw(buttons_raw);

        // Word 2: Analog sticks
        // Format: [unused, unused, stick_x, stick_y] (main stick in upper 16 bits)
        // Bytes 0-1 are for secondary stick (stays 0x80 on standard controller)
        let analog_word = payload[2];
        let stick_x = ((analog_word >> 16) & 0xFF) as u8;
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
    pub fn stick_centered(&self, deadzone: u8) -> bool {
        let dx = (self.stick_x as i16 - 128).unsigned_abs() as u8;
        let dy = (self.stick_y as i16 - 128).unsigned_abs() as u8;
        dx <= deadzone && dy <= deadzone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_button_parse_none_pressed() {
        // All bits high = no buttons pressed (active low)
        let buttons = ButtonState::from_raw(0xFFFF);
        assert!(!buttons.any_pressed());
    }

    #[test]
    fn test_button_parse_a_pressed() {
        // Bit 2 low = A pressed
        let buttons = ButtonState::from_raw(0xFFFB);
        assert!(buttons.a);
        assert!(!buttons.b);
        assert!(!buttons.start);
    }

    #[test]
    fn test_controller_state_parse() {
        // Word 1: [trig_L_raw, trig_R_raw, btn_low, btn_high]
        // Triggers inverted: 0xFF = released (becomes 0), 0x00 = pressed (becomes 255)
        // For trigger_l = 200 (after invert): raw = 255 - 200 = 55 = 0x37
        // For trigger_r = 100 (after invert): raw = 255 - 100 = 155 = 0x9B
        // Buttons: A pressed = bit 2 low. After swap_bytes on upper 16: need 0xFFFB
        // Upper 16 bits = btn_low | (btn_high << 8), so bytes [0xFB, 0xFF]
        // Word 1 = 0x37 | (0x9B << 8) | (0xFB << 16) | (0xFF << 24) = 0xFFFB9B37
        //
        // Word 2: [stick_x, stick_y, ...]
        // For stick_x = 64, stick_y = 200: Word 2 = 64 | (200 << 8) | ... = 0x????C840
        let payload = [
            0x00000001,  // Function type: controller
            0xFFFB9B37,  // trig_L_raw=0x37, trig_R_raw=0x9B, buttons=A pressed
            0x8080C840,  // stick_x=64, stick_y=200, unused
        ];

        let state = ControllerState::from_payload(&payload).unwrap();
        assert!(state.buttons.a);
        assert_eq!(state.trigger_l, 200);
        assert_eq!(state.trigger_r, 100);
        assert_eq!(state.stick_x, 64);
        assert_eq!(state.stick_y, 200);
    }
}
