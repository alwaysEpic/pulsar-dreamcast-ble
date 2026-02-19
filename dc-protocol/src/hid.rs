//! Xbox One S BLE HID gamepad report types.
//!
//! Contains the `GamepadReport` struct and byte serialization.
//! GATT service definitions live in the main crate (require nrf-softdevice macros).

/// 10-bit trigger mask.
const TRIGGER_10BIT_MASK: u16 = 0x03FF;

/// Right stick center as little-endian bytes (32768 = 0x8000).
const RIGHT_STICK_CENTER_LE: [u8; 2] = [0x00, 0x80];

/// Xbox BLE stick center value (unsigned 16-bit).
const STICK_CENTER: u16 = 32768;

/// Xbox One S BLE gamepad report (Report ID 0x01, 16 bytes data).
///
/// NOTE: Report ID is NOT included in the byte array -- the Report Reference
/// descriptor on the characteristic identifies this as Report ID 1.
///
/// Byte layout matches real Xbox One S (Model 1708) exactly:
///   Bytes 0-1:   Left Stick X   (uint16 LE, 0-65535, center=32768)
///   Bytes 2-3:   Left Stick Y   (uint16 LE, 0-65535, center=32768)
///   Bytes 4-5:   Right Stick X  (uint16 LE, 0-65535, center=32768)
///   Bytes 6-7:   Right Stick Y  (uint16 LE, 0-65535, center=32768)
///   Bytes 8-9:   Left Trigger   (10-bit LE in low bits, 6 padding in high bits)
///   Bytes 10-11: Right Trigger  (10-bit LE in low bits, 6 padding in high bits)
///   Byte 12:     Hat Switch     (4-bit in low nibble, 4 padding in high nibble)
///   Bytes 13-14: Buttons 1-15   (15 bits, 1-bit padding)
///   Byte 15:     AC Back        (1 bit, 7-bit padding)
#[derive(Clone, Copy)]
pub struct GamepadReport {
    /// Left stick X (0=left, 32768=center, 65535=right)
    pub left_x: u16,
    /// Left stick Y (0=top, 32768=center, 65535=bottom)
    pub left_y: u16,
    /// Left trigger (0=released, 1023=fully pressed)
    pub left_trigger: u16,
    /// Right trigger (0=released, 1023=fully pressed)
    pub right_trigger: u16,
    /// Hat switch / D-pad (0=neutral, 1-8=directions)
    pub hat: u8,
    /// Button bitmask (bits 0-14 = buttons 1-15)
    pub buttons: u16,
}

impl Default for GamepadReport {
    fn default() -> Self {
        Self {
            left_x: STICK_CENTER,
            left_y: STICK_CENTER,
            left_trigger: 0,
            right_trigger: 0,
            hat: hat::NEUTRAL,
            buttons: 0,
        }
    }
}

/// Hat switch values (Xbox One convention: 1-8, 0=neutral/null).
pub mod hat {
    pub const NEUTRAL: u8 = 0;
    pub const NORTH: u8 = 1;
    pub const NORTH_EAST: u8 = 2;
    pub const EAST: u8 = 3;
    pub const SOUTH_EAST: u8 = 4;
    pub const SOUTH: u8 = 5;
    pub const SOUTH_WEST: u8 = 6;
    pub const WEST: u8 = 7;
    pub const NORTH_WEST: u8 = 8;
}

impl GamepadReport {
    /// Create a new report with neutral/centered values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Convert to 16-byte array for BLE transmission.
    ///
    /// Trigger packing: 10 data bits in low bits of `u16`, 6 zero padding in high bits.
    /// Byte 8 = `trigger[7:0]`, Byte 9 = `000000 | trigger[9:8]`
    #[must_use]
    pub fn to_bytes(self) -> [u8; 16] {
        let lx = self.left_x.to_le_bytes();
        let ly = self.left_y.to_le_bytes();
        // Triggers: mask to 10 bits, stored as LE u16 (padding is in high 6 bits)
        let lt = (self.left_trigger & TRIGGER_10BIT_MASK).to_le_bytes();
        let rt = (self.right_trigger & TRIGGER_10BIT_MASK).to_le_bytes();

        #[allow(clippy::cast_possible_truncation)]
        [
            // Left Stick X (bytes 0-1, uint16 LE)
            lx[0],
            lx[1],
            // Left Stick Y (bytes 2-3, uint16 LE)
            ly[0],
            ly[1],
            // Right Stick X (bytes 4-5) - Dreamcast has no right stick, center=32768
            RIGHT_STICK_CENTER_LE[0],
            RIGHT_STICK_CENTER_LE[1],
            // Right Stick Y (bytes 6-7)
            RIGHT_STICK_CENTER_LE[0],
            RIGHT_STICK_CENTER_LE[1],
            // Left Trigger (bytes 8-9, 10-bit + 6 padding)
            lt[0],
            lt[1],
            // Right Trigger (bytes 10-11, 10-bit + 6 padding)
            rt[0],
            rt[1],
            // Hat Switch (byte 12, low nibble = value, high nibble = padding)
            self.hat & 0x0F,
            // Buttons 1-8 (byte 13)
            (self.buttons & 0xFF) as u8,
            // Buttons 9-15 + 1-bit padding (byte 14)
            ((self.buttons >> 8) & 0x7F) as u8,
            // AC Back (byte 15, bit 0) - unused, always 0
            0x00,
        ]
    }
}

/// Button bit positions in the 15-bit button field.
/// Xbox One S layout (matches HID Button Usage 1-15):
///   Bit 0  = Button 1  = A
///   Bit 1  = Button 2  = B
///   Bit 2  = Button 3  = X
///   Bit 3  = Button 4  = Y
///   Bit 4  = Button 5  = LB (Left Bumper)
///   Bit 5  = Button 6  = RB (Right Bumper)
///   Bit 6  = Button 7  = Back/View
///   Bit 7  = Button 8  = Menu/Start
///   Bit 8  = Button 9  = Left Stick Click (L3)
///   Bit 9  = Button 10 = Right Stick Click (R3)
///   Bits 10-14 = reserved
pub mod buttons {
    pub const A: u16 = 1 << 0;
    pub const B: u16 = 1 << 1;
    pub const X: u16 = 1 << 2;
    pub const Y: u16 = 1 << 3;
    pub const LB: u16 = 1 << 4;
    pub const RB: u16 = 1 << 5;
    pub const BACK: u16 = 1 << 6;
    pub const START: u16 = 1 << 7;
    pub const L3: u16 = 1 << 8;
    pub const R3: u16 = 1 << 9;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_bytes_default() {
        let report = GamepadReport::default();
        let bytes = report.to_bytes();
        // Sticks centered at 32768 = 0x8000 LE = [0x00, 0x80]
        assert_eq!(bytes[0], 0x00); // left_x low
        assert_eq!(bytes[1], 0x80); // left_x high
        assert_eq!(bytes[2], 0x00); // left_y low
        assert_eq!(bytes[3], 0x80); // left_y high
        assert_eq!(bytes[4], 0x00); // right_x low (center)
        assert_eq!(bytes[5], 0x80); // right_x high
        assert_eq!(bytes[6], 0x00); // right_y low
        assert_eq!(bytes[7], 0x80); // right_y high
        assert_eq!(bytes[8], 0x00); // left trigger low
        assert_eq!(bytes[9], 0x00); // left trigger high
        assert_eq!(bytes[10], 0x00); // right trigger low
        assert_eq!(bytes[11], 0x00); // right trigger high
        assert_eq!(bytes[12], 0x00); // hat (neutral)
        assert_eq!(bytes[13], 0x00); // buttons low
        assert_eq!(bytes[14], 0x00); // buttons high
        assert_eq!(bytes[15], 0x00); // AC back
    }

    #[test]
    fn to_bytes_buttons() {
        let report = GamepadReport {
            buttons: buttons::A | buttons::Y | buttons::START, // 0x01 | 0x08 | 0x80 = 0x89
            ..Default::default()
        };
        let bytes = report.to_bytes();
        assert_eq!(bytes[13], 0x89); // buttons low byte
        assert_eq!(bytes[14], 0x00); // buttons high byte
    }

    #[test]
    fn to_bytes_triggers() {
        let report = GamepadReport {
            left_trigger: 1023,  // max 10-bit = 0x03FF
            right_trigger: 512,  // 0x0200
            ..Default::default()
        };
        let bytes = report.to_bytes();
        assert_eq!(bytes[8], 0xFF);  // left trigger low
        assert_eq!(bytes[9], 0x03);  // left trigger high (10-bit)
        assert_eq!(bytes[10], 0x00); // right trigger low
        assert_eq!(bytes[11], 0x02); // right trigger high
    }

    #[test]
    fn to_bytes_hat_values() {
        for hat_val in 0..=8 {
            let report = GamepadReport {
                hat: hat_val,
                ..Default::default()
            };
            let bytes = report.to_bytes();
            assert_eq!(bytes[12], hat_val & 0x0F);
        }
    }

    #[test]
    fn to_bytes_max_values() {
        let report = GamepadReport {
            left_x: 65535,
            left_y: 65535,
            left_trigger: 1023,
            right_trigger: 1023,
            hat: 8,
            buttons: 0x7FFF, // all 15 buttons
        };
        let bytes = report.to_bytes();
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1], 0xFF);
        assert_eq!(bytes[2], 0xFF);
        assert_eq!(bytes[3], 0xFF);
        assert_eq!(bytes[8], 0xFF);
        assert_eq!(bytes[9], 0x03);
        assert_eq!(bytes[10], 0xFF);
        assert_eq!(bytes[11], 0x03);
        assert_eq!(bytes[12], 8);
        assert_eq!(bytes[13], 0xFF);
        assert_eq!(bytes[14], 0x7F);
    }
}
