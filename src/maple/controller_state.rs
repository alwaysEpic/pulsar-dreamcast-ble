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
}

impl ControllerState {
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

        // Word 1: Buttons in upper 16 bits
        let buttons_raw = ((payload[1] >> 16) & 0xFFFF) as u16;
        let buttons = ButtonState::from_raw(buttons_raw);

        // Word 2: Analog values
        // Byte layout: [R trigger] [L trigger] [Stick X] [Stick Y]
        let analog_word = payload[2];
        let trigger_r = ((analog_word >> 24) & 0xFF) as u8;
        let trigger_l = ((analog_word >> 16) & 0xFF) as u8;
        let stick_x = ((analog_word >> 8) & 0xFF) as u8;
        let stick_y = (analog_word & 0xFF) as u8;

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
        let payload = [
            0x00000001, // Function type: controller
            0xFFFB0000, // A button pressed (bit 2 = 0), lower 16 unused
            0x80804080, // R=128, L=64, X=64, Y=128
        ];

        let state = ControllerState::from_payload(&payload).unwrap();
        assert!(state.buttons.a);
        assert_eq!(state.trigger_r, 128);
        assert_eq!(state.trigger_l, 64);
        assert_eq!(state.stick_x, 64);
        assert_eq!(state.stick_y, 128);
    }
}
