// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Dreamcast controller state representation.
//!
//! Holds the parsed state from a `Get Condition` (`0x09`) response.

/// Maple Bus function code for standard controller.
pub const CONTROLLER_FUNCTION: u32 = 0x0000_0001;

/// Represents the state of a standard Dreamcast controller.
#[derive(Debug, Clone, Copy)]
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

/// Neutral controller state: no buttons, triggers released, sticks centered.
///
/// NOT the all-zero state — raw stick `0` is left + up (the upper-left
/// corner); the neutral position is center (`128` -> `32768`). The poll loop
/// signals this on controller disconnect so the host sees a centered stick
/// rather than a stuck corner (issue #6).
impl Default for ControllerState {
    fn default() -> Self {
        Self {
            buttons: ButtonState::default(),
            trigger_l: 0,
            trigger_r: 0,
            stick_x: DC_STICK_CENTER,
            stick_y: DC_STICK_CENTER,
        }
    }
}

/// Digital button states from a Dreamcast controller.
/// Note: In the Maple protocol, buttons are active LOW (0 = pressed).
/// We invert them here so true = pressed for easier use.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one field per physical Dreamcast button, mirroring the wire word 1:1; \
              a bitflags type would hide that mapping"
)]
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
    pub const fn from_raw(raw: u16) -> Self {
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
    pub const fn any_pressed(&self) -> bool {
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
    pub const fn to_raw(self) -> u16 {
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

/// Trigger noise floor — values at or below this are treated as "at rest"
/// for `state_changed`. Idle drift in this range won't generate a state
/// change, which prevents waking sleeping BLE hosts via phantom reports.
const TRIGGER_NOISE_FLOOR: u8 = 5;

/// Stick deadzone radius (matches the deadzone applied in `to_gamepad_report`).
/// `state_changed` treats axis pairs that are both inside the deadzone as
/// unchanged — the post-deadzone HID output is identical, so emitting a
/// notify would wake the host without any user intent.
const STICK_DEADZONE: u16 = 5;

/// D-pad/hat 16-way match (Xbox convention: 1-8, 0 = neutral). Opposing
/// pairs fall through to the `_ => NEUTRAL` arm — today's rule, kept
/// deliberately for remapped fan-in (design v2 §2.2).
const fn hat_match(directions: [bool; 4]) -> u8 {
    use crate::xbox_hid::hat;
    // [up, down, left, right]
    match directions {
        [true, false, false, false] => hat::NORTH,
        [true, false, false, true] => hat::NORTH_EAST,
        [false, false, false, true] => hat::EAST,
        [false, true, false, true] => hat::SOUTH_EAST,
        [false, true, false, false] => hat::SOUTH,
        [false, true, true, false] => hat::SOUTH_WEST,
        [false, false, true, false] => hat::WEST,
        [true, false, true, false] => hat::NORTH_WEST,
        _ => hat::NEUTRAL,
    }
}

/// Dreamcast trigger (0-255) to the 10-bit HID axis (0-1023), as always.
#[expect(
    clippy::cast_possible_truncation,
    reason = "u8 input scaled by XBOX_TRIGGER_MAX/DC_TRIGGER_MAX yields at most \
              1023, so the u32->u16 narrowing cannot lose bits"
)]
fn scale_trigger(value: u8) -> u16 {
    (u32::from(value) * XBOX_TRIGGER_MAX / DC_TRIGGER_MAX) as u16
}

/// Strict-inequality deadzone test around stick centre, matching the
/// pre-remap conversion: `|v - 128| < dz` clamps to centre.
fn in_deadzone(value: u8, deadzone: u16) -> bool {
    (i16::from(value) - i16::from(DC_STICK_CENTER)).unsigned_abs() < deadzone
}

/// Fan-in accumulator with the typed reducers of design v2 §2.2: OR for
/// Boolean buttons and Hat directions, **max** for trigger axes — bitwise OR
/// is undefined for an axis.
struct RemapAccum {
    buttons: u16,
    /// up, down, left, right — fed to [`hat_match`] after reduction.
    hat: [bool; 4],
    left_trigger: u16,
    right_trigger: u16,
}

impl RemapAccum {
    const fn new() -> Self {
        Self {
            buttons: 0,
            hat: [false; 4],
            left_trigger: 0,
            right_trigger: 0,
        }
    }

    /// One source's contribution to one destination. `pressed` is the
    /// source's digital reading (a digital source's state, or an analog
    /// source thresholded); `axis_value` is its analog reading (a digital
    /// source contributes full scale, an analog source its scaled value).
    /// Which one applies is decided by the destination's kind.
    fn contribute(&mut self, code: u8, pressed: bool, axis_value: u16) {
        use crate::remap::dest;
        match code {
            dest::A..=dest::GUIDE if pressed => self.buttons |= 1 << (code - 1),
            dest::HAT_UP..=dest::HAT_RIGHT if pressed => {
                self.hat[usize::from(code - dest::HAT_UP)] = true;
            }
            dest::LEFT_TRIGGER if axis_value > self.left_trigger => {
                self.left_trigger = axis_value;
            }
            dest::RIGHT_TRIGGER if axis_value > self.right_trigger => {
                self.right_trigger = axis_value;
            }
            // NONE, an unpressed Boolean contribution, an axis contribution
            // that loses the max — and, defensively, any code outside the
            // validated namespace — contribute nothing.
            _ => {}
        }
    }
}

impl ControllerState {
    /// Convert to the profile-neutral gamepad report under the default map.
    ///
    /// Since the remap build this is the remapped conversion under
    /// [`RemapTable::DEFAULT`] with no chord activity; the oracle tests pin
    /// it bit-for-bit to the pre-remap conversion (gate G7).
    #[must_use]
    pub fn to_gamepad_report(self) -> crate::xbox_hid::GamepadReport {
        self.remapped_report(&crate::remap::RemapTable::DEFAULT, false, false)
    }

    /// Convert under `map`, with the Guide chord folded in at source level
    /// (design v2 §2.2): while the chord is held, the contributions of Start
    /// and both triggers are excluded BEFORE fan-in — so a destination they
    /// share with an unrelated source still sees that source — and Guide is
    /// OR-ed into the report after reduction during the pulse.
    ///
    /// One conversion, two callers: the HID task serializes this report and
    /// the config personality notifies it as `LiveOutput`. There is no
    /// second implementation of the map anywhere.
    ///
    /// The chord output is returned alongside the report because the caller
    /// owns the one-shot side effects (the VMU home glyph on `rising_edge`).
    #[must_use]
    pub fn to_gamepad_report_with(
        self,
        map: &crate::remap::RemapTable,
        chord: &mut crate::guide_chord::GuideChord,
        now_ms: u64,
    ) -> (
        crate::xbox_hid::GamepadReport,
        crate::guide_chord::GuideChordOutput,
    ) {
        let out = chord.update(&self, now_ms);
        (self.remapped_report(map, out.suppress, out.emit_guide), out)
    }

    /// The remapped conversion core: gather every source contribution
    /// through the typed reducers, route the stick, then assemble.
    fn remapped_report(
        self,
        map: &crate::remap::RemapTable,
        chord_suppress: bool,
        emit_guide: bool,
    ) -> crate::xbox_hid::GamepadReport {
        use crate::remap::{flags, source, stick_dest};
        use crate::xbox_hid::{buttons, GamepadReport};

        let mut acc = RemapAccum::new();

        // Digital sources, `ButtonState::from_raw` bit order (= the map's
        // `buttons` index order).
        let pressed = [
            self.buttons.c,
            self.buttons.b,
            self.buttons.a,
            self.buttons.start,
            self.buttons.dpad_up,
            self.buttons.dpad_down,
            self.buttons.dpad_left,
            self.buttons.dpad_right,
            self.buttons.z,
            self.buttons.y,
            self.buttons.x,
            self.buttons.d,
        ];
        for (i, &is_pressed) in pressed.iter().enumerate() {
            // Source-level Guide exclusion: Start's contribution is removed
            // before fan-in while the chord is held.
            if !is_pressed || (chord_suppress && i == source::START) {
                continue;
            }
            // A pressed digital source contributes full scale to an axis
            // destination (§2.2).
            acc.contribute(map.buttons[i], true, scale_trigger(u8::MAX));
        }

        // Analog trigger sources — excluded entirely while the chord is
        // held, same source-level rule as Start.
        if !chord_suppress {
            acc.contribute(
                map.trigger_l,
                self.trigger_l >= map.trigger_threshold,
                scale_trigger(self.trigger_l),
            );
            acc.contribute(
                map.trigger_r,
                self.trigger_r >= map.trigger_threshold,
                scale_trigger(self.trigger_r),
            );
        }

        // Stick: normalize orientation first — the flags describe the stick
        // itself, so they apply whatever the destination — then route.
        let (mut sx, mut sy) = if map.flags & flags::SWAP_XY != 0 {
            (self.stick_y, self.stick_x)
        } else {
            (self.stick_x, self.stick_y)
        };
        if map.flags & flags::INVERT_X != 0 {
            sx = u8::MAX - sx;
        }
        if map.flags & flags::INVERT_Y != 0 {
            sy = u8::MAX - sy;
        }
        let dz = u16::from(map.stick_deadzone);
        let mut left_x = XBOX_STICK_CENTER;
        let mut left_y = XBOX_STICK_CENTER;
        match map.stick_dest {
            stick_dest::LEFT_STICK => {
                if !in_deadzone(sx, dz) {
                    left_x = u16::from(sx) * STICK_SCALE_FACTOR;
                }
                if !in_deadzone(sy, dz) {
                    left_y = u16::from(sy) * STICK_SCALE_FACTOR;
                }
            }
            stick_dest::HAT => {
                // Threshold each axis at the deadzone; direction by sign
                // (raw 0 is up/left). OR-ed into the same four Booleans as
                // every other Hat contribution, so opposing-pair fan-in
                // falls to Neutral like the D-pad does.
                if !in_deadzone(sy, dz) {
                    // false -> 0 (up), true -> 1 (down)
                    acc.hat[usize::from(sy >= DC_STICK_CENTER)] = true;
                }
                if !in_deadzone(sx, dz) {
                    // false -> 2 (left), true -> 3 (right)
                    acc.hat[2 + usize::from(sx >= DC_STICK_CENTER)] = true;
                }
            }
            // OFF (and, defensively, anything outside the validated range):
            // the stick contributes nothing, the left stick reads centred.
            _ => {}
        }

        let mut btns = acc.buttons;
        if emit_guide {
            btns |= buttons::GUIDE;
        }

        GamepadReport {
            left_x,
            left_y,
            left_trigger: acc.left_trigger,
            right_trigger: acc.right_trigger,
            hat: hat_match(acc.hat),
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
        //
        // Taken as little-endian bytes rather than shift-and-mask casts: the
        // wire format *is* a byte layout, so this states the same thing without
        // a narrowing cast to justify.
        let [trigger_l, trigger_r, btn_low, btn_high] = payload[1].to_le_bytes();

        // Buttons occupy the upper 16 bits with the two bytes swapped, which is
        // just a big-endian read of that byte pair.
        let buttons = ButtonState::from_raw(u16::from_be_bytes([btn_low, btn_high]));

        // Word 2: Analog sticks
        // Format: [unused, unused, stick_x, stick_y] (main stick in upper 16 bits)
        // Bytes 0-1 are for secondary stick (stays 0x80 on standard controller)
        let [_, _, stick_x, stick_y] = payload[2].to_le_bytes();

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
    /// Buttons use exact comparison. Triggers and sticks apply both a
    /// magnitude threshold (filters out polling noise) AND a "rest zone"
    /// gate: idle drift around 0 (triggers) or center (sticks) does not
    /// count as a change, since the post-deadzone HID output would be
    /// byte-identical anyway. This prevents phantom BLE notifies that
    /// would wake a sleeping host with no actual user input.
    #[must_use]
    pub fn state_changed(&self, other: &Self) -> bool {
        if self.buttons.to_raw() != other.buttons.to_raw() {
            return true;
        }

        if trigger_changed(self.trigger_l, other.trigger_l)
            || trigger_changed(self.trigger_r, other.trigger_r)
        {
            return true;
        }

        if stick_axis_changed(self.stick_x, other.stick_x)
            || stick_axis_changed(self.stick_y, other.stick_y)
        {
            return true;
        }

        false
    }
}

/// The pre-remap `to_gamepad_report` body, kept verbatim as the
/// default-behaviour oracle (remap design v2 §2.4, gate G7). It lives here,
/// not in the oracle module, because it needs this module's private
/// constants. `to_gamepad_report_with(&RemapTable::DEFAULT, ..)` must
/// reproduce it bit-for-bit; the oracle tests assert against THIS and the
/// golden bytes, never against the production path.
#[cfg(test)]
pub(crate) fn legacy_to_gamepad_report(state: ControllerState) -> crate::xbox_hid::GamepadReport {
    use crate::xbox_hid::{buttons, hat, GamepadReport};

    let mut btns: u16 = 0;
    if state.buttons.a {
        btns |= buttons::A;
    }
    if state.buttons.b {
        btns |= buttons::B;
    }
    if state.buttons.x {
        btns |= buttons::X;
    }
    if state.buttons.y {
        btns |= buttons::Y;
    }
    if state.buttons.start {
        btns |= buttons::START;
    }

    // D-pad -> Hat switch (Xbox convention: 1-8, 0=neutral)
    let hat_value = match (
        state.buttons.dpad_up,
        state.buttons.dpad_down,
        state.buttons.dpad_left,
        state.buttons.dpad_right,
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

    let raw_x = state.stick_y; // Dreamcast Y -> HID X
    let raw_y = state.stick_x; // Dreamcast X -> HID Y
    let left_x: u16 =
        if (i16::from(raw_x) - i16::from(DC_STICK_CENTER)).unsigned_abs() < STICK_DEADZONE {
            XBOX_STICK_CENTER
        } else {
            u16::from(raw_x) * STICK_SCALE_FACTOR
        };
    let left_y: u16 =
        if (i16::from(raw_y) - i16::from(DC_STICK_CENTER)).unsigned_abs() < STICK_DEADZONE {
            XBOX_STICK_CENTER
        } else {
            u16::from(raw_y) * STICK_SCALE_FACTOR
        };

    #[expect(
        clippy::cast_possible_truncation,
        reason = "u8 input scaled by XBOX_TRIGGER_MAX/DC_TRIGGER_MAX yields at most \
                  1023, so the u32->u16 narrowing cannot lose bits"
    )]
    let left_trigger = (u32::from(state.trigger_l) * XBOX_TRIGGER_MAX / DC_TRIGGER_MAX) as u16;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "u8 input scaled by XBOX_TRIGGER_MAX/DC_TRIGGER_MAX yields at most \
                  1023, so the u32->u16 narrowing cannot lose bits"
    )]
    let right_trigger = (u32::from(state.trigger_r) * XBOX_TRIGGER_MAX / DC_TRIGGER_MAX) as u16;

    GamepadReport {
        left_x,
        left_y,
        left_trigger,
        right_trigger,
        hat: hat_value,
        buttons: btns,
    }
}

/// Trigger change detection: ignore drift when both values are at the noise
/// floor (the HID output would be 0 for both). Otherwise apply the standard
/// threshold.
fn trigger_changed(a: u8, b: u8) -> bool {
    if a <= TRIGGER_NOISE_FLOOR && b <= TRIGGER_NOISE_FLOOR {
        return false;
    }
    (i16::from(a) - i16::from(b)).abs() > TRIGGER_CHANGE_THRESHOLD
}

/// Stick axis change detection: ignore drift when both values are within
/// the deadzone (the HID output is clamped to center for both, so the wire
/// payload is byte-identical).
fn stick_axis_changed(a: u8, b: u8) -> bool {
    let a_in_dz = (i16::from(a) - i16::from(DC_STICK_CENTER)).unsigned_abs() < STICK_DEADZONE;
    let b_in_dz = (i16::from(b) - i16::from(DC_STICK_CENTER)).unsigned_abs() < STICK_DEADZONE;
    if a_in_dz && b_in_dz {
        return false;
    }
    (i16::from(a) - i16::from(b)).abs() > STICK_CHANGE_THRESHOLD
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
        use crate::xbox_hid::buttons;

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
            stick_x: 135, // diff=7 > threshold 2, out of deadzone
            ..Default::default()
        };
        assert!(a.state_changed(&b));

        let c = ControllerState {
            stick_x: 129, // diff=1 <= threshold 2
            ..Default::default()
        };
        assert!(!a.state_changed(&c));
    }

    #[test]
    fn state_changed_stick_drift_within_deadzone() {
        // Both values inside the ±5 deadzone — post-clamp HID output is
        // identical, so this should NOT count as a change even though the
        // raw diff (6) exceeds STICK_CHANGE_THRESHOLD (2).
        let a = ControllerState {
            stick_x: 125,
            ..Default::default()
        };
        let b = ControllerState {
            stick_x: 131,
            ..Default::default()
        };
        assert!(!a.state_changed(&b));
    }

    #[test]
    fn state_changed_stick_drift_crossing_deadzone() {
        // One inside deadzone, one outside — output WOULD differ, so this
        // should still count as a change.
        let a = ControllerState {
            stick_x: 128, // center
            ..Default::default()
        };
        let b = ControllerState {
            stick_x: 140, // outside deadzone
            ..Default::default()
        };
        assert!(a.state_changed(&b));
    }

    #[test]
    fn state_changed_trigger_drift_below_noise_floor() {
        // Both triggers below the noise floor — idle, post-clamp HID output
        // is 0 for both, no change.
        let a = ControllerState {
            trigger_l: 0,
            ..Default::default()
        };
        let b = ControllerState {
            trigger_l: 4, // diff=4 > threshold but both at rest
            ..Default::default()
        };
        assert!(!a.state_changed(&b));
    }

    // --- Remapped conversion: typed reducers, fan-in, source-level Guide
    // exclusion (remap design v2 §2.2, gates G5/G6/G7) ---

    use crate::remap::{dest, flags as rflags, stick_dest, RemapTable};

    /// Run the remapped conversion with no chord activity.
    fn remap(state: &ControllerState, map: &RemapTable) -> crate::xbox_hid::GamepadReport {
        state.remapped_report(map, false, false)
    }

    #[test]
    fn hat_all_16_combinations_exhaustive() {
        use crate::xbox_hid::hat;
        // (up, down, left, right) for every combination — opposing pairs and
        // 3-4 direction chords are Neutral, today's rule. Pinned here
        // independently of hat_match's own structure.
        let expected = [
            (false, false, false, false, hat::NEUTRAL),
            (true, false, false, false, hat::NORTH),
            (false, true, false, false, hat::SOUTH),
            (true, true, false, false, hat::NEUTRAL),
            (false, false, true, false, hat::WEST),
            (true, false, true, false, hat::NORTH_WEST),
            (false, true, true, false, hat::SOUTH_WEST),
            (true, true, true, false, hat::NEUTRAL),
            (false, false, false, true, hat::EAST),
            (true, false, false, true, hat::NORTH_EAST),
            (false, true, false, true, hat::SOUTH_EAST),
            (true, true, false, true, hat::NEUTRAL),
            (false, false, true, true, hat::NEUTRAL),
            (true, false, true, true, hat::NEUTRAL),
            (false, true, true, true, hat::NEUTRAL),
            (true, true, true, true, hat::NEUTRAL),
        ];
        for (up, down, left, right, want) in expected {
            let state = ControllerState {
                buttons: ButtonState {
                    dpad_up: up,
                    dpad_down: down,
                    dpad_left: left,
                    dpad_right: right,
                    ..ButtonState::default()
                },
                ..ControllerState::default()
            };
            let report = remap(&state, &RemapTable::DEFAULT);
            assert_eq!(
                report.hat, want,
                "dpad ({up},{down},{left},{right}) must be hat {want}"
            );
        }
    }

    #[test]
    fn fan_in_two_digital_sources_or_into_one_button() {
        use crate::xbox_hid::buttons;
        let mut map = RemapTable::DEFAULT;
        map.buttons[crate::remap::source::A] = dest::LB;
        map.buttons[crate::remap::source::B] = dest::LB;

        for (a, b) in [(true, false), (false, true), (true, true)] {
            let state = ControllerState {
                buttons: ButtonState {
                    b,
                    a,
                    ..ButtonState::default()
                },
                ..ControllerState::default()
            };
            let report = remap(&state, &map);
            assert_ne!(report.buttons & buttons::LB, 0, "a={a} b={b}");
            assert_eq!(report.buttons & buttons::A, 0);
            assert_eq!(report.buttons & buttons::B, 0);
        }
    }

    #[test]
    fn analog_source_thresholds_into_boolean_destination() {
        use crate::xbox_hid::buttons;
        let mut map = RemapTable::DEFAULT;
        map.trigger_l = dest::A;
        for (threshold, value, pressed) in [
            (128u8, 127u8, false),
            (128, 128, true),
            (1, 0, false),
            (1, 1, true),
            (255, 254, false),
            (255, 255, true),
        ] {
            map.trigger_threshold = threshold;
            let state = ControllerState {
                trigger_l: value,
                ..ControllerState::default()
            };
            let report = remap(&state, &map);
            assert_eq!(
                (report.buttons & buttons::A) != 0,
                pressed,
                "threshold {threshold}, value {value}"
            );
            assert_eq!(report.left_trigger, 0, "no analog leak to the old axis");
        }
    }

    #[test]
    fn two_analog_sources_reduce_by_max_on_one_axis() {
        let mut map = RemapTable::DEFAULT;
        map.trigger_r = dest::LEFT_TRIGGER; // both triggers -> left axis
        let state = ControllerState {
            trigger_l: 100,
            trigger_r: 200,
            ..ControllerState::default()
        };
        let report = remap(&state, &map);
        // max of the two scaled contributions, never an OR of their bits.
        assert_eq!(
            report.left_trigger,
            u16::try_from(200u32 * 1023 / 255).unwrap()
        );
        assert_eq!(report.right_trigger, 0);
    }

    #[test]
    fn digital_source_contributes_full_scale_to_axis() {
        let mut map = RemapTable::DEFAULT;
        map.buttons[crate::remap::source::A] = dest::RIGHT_TRIGGER;
        let state = ControllerState {
            buttons: ButtonState {
                a: true,
                ..ButtonState::default()
            },
            // The analog right trigger contributes too — max wins, so the
            // digital full-scale must dominate.
            trigger_r: 100,
            ..ControllerState::default()
        };
        let report = remap(&state, &map);
        assert_eq!(report.right_trigger, 1023);
    }

    #[test]
    fn opposing_hat_fan_in_is_neutral() {
        use crate::xbox_hid::hat;
        // An analog source driving HatDown against the D-pad's HatUp: the
        // opposing pair reduces to Neutral, exactly like a physical up+down.
        let mut map = RemapTable::DEFAULT;
        map.trigger_l = dest::HAT_DOWN;
        let state = ControllerState {
            buttons: ButtonState {
                dpad_up: true,
                ..ButtonState::default()
            },
            trigger_l: 255,
            ..ControllerState::default()
        };
        assert_eq!(remap(&state, &map).hat, hat::NEUTRAL);
    }

    #[test]
    fn stick_to_hat_ors_with_dpad_contributions() {
        use crate::xbox_hid::hat;
        let mut map = RemapTable::DEFAULT;
        map.stick_dest = stick_dest::HAT;
        map.flags = 0; // no swap: stick_x is horizontal, stick_y vertical

        // Stick hard up alone -> NORTH; left stick output stays centred.
        let state = ControllerState {
            stick_y: 0,
            ..ControllerState::default()
        };
        let report = remap(&state, &map);
        assert_eq!(report.hat, hat::NORTH);
        assert_eq!(report.left_x, 32768);
        assert_eq!(report.left_y, 32768);

        // Stick hard left + D-pad right -> opposing pair -> Neutral.
        let state = ControllerState {
            buttons: ButtonState {
                dpad_right: true,
                ..ButtonState::default()
            },
            stick_x: 0,
            ..ControllerState::default()
        };
        assert_eq!(remap(&state, &map).hat, hat::NEUTRAL);

        // Stick hard up + D-pad right -> NORTH_EAST, one boolean from each.
        let state = ControllerState {
            buttons: ButtonState {
                dpad_right: true,
                ..ButtonState::default()
            },
            stick_y: 0,
            ..ControllerState::default()
        };
        assert_eq!(remap(&state, &map).hat, hat::NORTH_EAST);

        // Inside the deadzone the stick contributes nothing.
        let state = ControllerState {
            stick_x: 130,
            stick_y: 126,
            ..ControllerState::default()
        };
        assert_eq!(remap(&state, &map).hat, hat::NEUTRAL);
    }

    #[test]
    fn guide_exclusion_is_source_level_not_post_hoc() {
        use crate::xbox_hid::buttons;
        // G6's scenario: Start and A share a destination (LB). While the
        // chord is held, Start's contribution is excluded BEFORE fan-in, so
        // LB still sees A. A post-hoc `buttons &= !LB` would wrongly clear it;
        // a post-hoc `buttons &= !START` would wrongly leave Start's LB.
        let mut map = RemapTable::DEFAULT;
        map.buttons[crate::remap::source::START] = dest::LB;
        map.buttons[crate::remap::source::A] = dest::LB;

        let chord_held = ControllerState {
            buttons: ButtonState {
                start: true,
                a: true,
                ..ButtonState::default()
            },
            trigger_l: 255,
            trigger_r: 255,
            ..ControllerState::default()
        };
        let report = chord_held.remapped_report(&map, true, false);
        assert_ne!(
            report.buttons & buttons::LB,
            0,
            "A's contribution must survive the exclusion"
        );
        assert_eq!(report.left_trigger, 0, "chord trigger excluded at source");
        assert_eq!(report.right_trigger, 0);

        // Same chord without A: Start was LB's only source, and its
        // contribution is excluded -> LB clear.
        let chord_only = ControllerState {
            buttons: ButtonState {
                start: true,
                ..ButtonState::default()
            },
            trigger_l: 255,
            trigger_r: 255,
            ..ControllerState::default()
        };
        let report = chord_only.remapped_report(&map, true, false);
        assert_eq!(
            report.buttons & buttons::LB,
            0,
            "Start's remapped contribution must not leak through the chord"
        );
    }

    #[test]
    fn remapped_triggers_can_cross_wire() {
        // L and R swapped: the left analog value lands on the right axis.
        let mut map = RemapTable::DEFAULT;
        map.trigger_l = dest::RIGHT_TRIGGER;
        map.trigger_r = dest::LEFT_TRIGGER;
        let state = ControllerState {
            trigger_l: 255,
            trigger_r: 0,
            ..ControllerState::default()
        };
        let report = remap(&state, &map);
        assert_eq!(report.right_trigger, 1023);
        assert_eq!(report.left_trigger, 0);
    }

    #[test]
    fn stick_dest_off_centres_left_stick() {
        let mut map = RemapTable::DEFAULT;
        map.stick_dest = stick_dest::OFF;
        let state = ControllerState {
            stick_x: 0,
            stick_y: 255,
            ..ControllerState::default()
        };
        let report = remap(&state, &map);
        assert_eq!(report.left_x, 32768);
        assert_eq!(report.left_y, 32768);
    }

    #[test]
    fn stick_invert_flags() {
        // No swap, invert X: hard-left reads as hard-right.
        let mut map = RemapTable::DEFAULT;
        map.flags = rflags::INVERT_X;
        let state = ControllerState {
            stick_x: 0,
            stick_y: 128,
            ..ControllerState::default()
        };
        let report = remap(&state, &map);
        assert_eq!(report.left_x, 65535);
        assert_eq!(report.left_y, 32768);

        // Inverting a centred axis lands on 127 — inside the default
        // deadzone, so it still reads centred.
        let centred = ControllerState::default();
        let report = remap(&centred, &map);
        assert_eq!(report.left_x, 32768);
        assert_eq!(report.left_y, 32768);
    }

    #[test]
    fn everything_mapped_to_none_is_legal_and_silent() {
        let map = RemapTable {
            buttons: [dest::NONE; 12],
            trigger_l: dest::NONE,
            trigger_r: dest::NONE,
            stick_dest: stick_dest::OFF,
            ..RemapTable::DEFAULT
        };
        assert!(map.is_valid());
        let state = ControllerState {
            buttons: ButtonState::from_raw(0), // everything pressed
            trigger_l: 255,
            trigger_r: 255,
            stick_x: 0,
            stick_y: 0,
        };
        let report = remap(&state, &map);
        assert_eq!(report.buttons, 0);
        assert_eq!(report.hat, 0);
        assert_eq!(report.left_trigger, 0);
        assert_eq!(report.right_trigger, 0);
        assert_eq!(report.left_x, 32768);
        assert_eq!(report.left_y, 32768);
    }

    // --- Issue #6: the neutral / disconnect state must center the stick ---
    // On controller disconnect (cable unplug) main.rs signals
    // `ControllerState::default()`. An all-zero state is NOT neutral: raw 0
    // means stick hard left + up (center is 128 -> 32768), so the host would
    // see the stick jammed into the upper-left corner. These guard that the
    // default state is a genuinely neutral controller.

    #[test]
    fn default_state_raw_sticks_are_centered() {
        let s = ControllerState::default();
        assert_eq!(
            s.stick_x, 128,
            "default stick_x must be centered (128), not 0"
        );
        assert_eq!(
            s.stick_y, 128,
            "default stick_y must be centered (128), not 0"
        );
        assert!(s.stick_centered(0), "default state must report as centered");
    }

    #[test]
    fn default_state_reports_centered_stick() {
        // Regression for issue #6: disconnect must map to a centered left stick
        // (32768, 32768), not the upper-left corner (0, 0).
        let report = ControllerState::default().to_gamepad_report();
        assert_eq!(
            report.left_x, 32768,
            "disconnect stick X must be centered, not upper-left"
        );
        assert_eq!(
            report.left_y, 32768,
            "disconnect stick Y must be centered, not upper-left"
        );
    }
}
