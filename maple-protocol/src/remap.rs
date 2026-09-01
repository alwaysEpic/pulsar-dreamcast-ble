// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Button remap table: source-keyed, profile-neutral (ADR-016, remap design
//! v2 §2).
//!
//! The map is keyed by Dreamcast source and its values are stable
//! destination codes naming a profile-neutral gamepad meaning; it is applied
//! between `ControllerState` and profile serialization, never inside a
//! serializer, never keyed to HID wire bits.
//!
//! The persisted ABI is the versioned destination-code namespace and its
//! semantics — `GamepadReport` may change shape without a flash migration as
//! long as every code keeps its meaning.

/// The destination-code namespace, version 1 (ADR-016). Codes may never
/// change meaning; extend by appending, never renumber.
pub mod dest {
    /// No destination — the source is disconnected.
    pub const NONE: u8 = 0x00;
    // Boolean buttons: `xbox_hid::buttons` bit index + 1.
    pub const A: u8 = 0x01;
    pub const B: u8 = 0x02;
    pub const X: u8 = 0x03;
    pub const Y: u8 = 0x04;
    pub const LB: u8 = 0x05;
    pub const RB: u8 = 0x06;
    pub const BACK: u8 = 0x07;
    pub const START: u8 = 0x08;
    pub const L3: u8 = 0x09;
    pub const R3: u8 = 0x0A;
    pub const GUIDE: u8 = 0x0B;
    // Hat directions — Booleans that feed the 16-way hat match.
    pub const HAT_UP: u8 = 0x10;
    pub const HAT_DOWN: u8 = 0x11;
    pub const HAT_LEFT: u8 = 0x12;
    pub const HAT_RIGHT: u8 = 0x13;
    // Analog trigger axes.
    pub const LEFT_TRIGGER: u8 = 0x20;
    pub const RIGHT_TRIGGER: u8 = 0x21;

    /// True for every code in the v1 namespace. Boolean, Hat and Analog
    /// codes are all legal for both digital and analog sources — the
    /// reducers say what each combination does (design v2 §2.2).
    #[must_use]
    pub const fn is_valid(code: u8) -> bool {
        matches!(
            code,
            NONE | A..=GUIDE | HAT_UP..=HAT_RIGHT | LEFT_TRIGGER | RIGHT_TRIGGER
        )
    }
}

/// `RemapTable::flags` bits. Bits 3–7 must be 0.
pub mod flags {
    /// Transpose the stick axes.
    ///
    /// Set in [`super::RemapTable::DEFAULT`]: today's `raw_x = stick_y`
    /// transposition is preserved as data (C6); whether it is a correction
    /// or a latent bug is A/B'd on hardware before it is described to a
    /// user as either.
    pub const SWAP_XY: u8 = 1 << 0;
    /// Invert the (post-swap) horizontal stick axis.
    pub const INVERT_X: u8 = 1 << 1;
    /// Invert the (post-swap) vertical stick axis.
    pub const INVERT_Y: u8 = 1 << 2;
    /// Every defined flag bit; the rest must be zero.
    pub const VALID_MASK: u8 = SWAP_XY | INVERT_X | INVERT_Y;
}

/// `RemapTable::stick_dest` values.
pub mod stick_dest {
    pub const OFF: u8 = 0;
    pub const LEFT_STICK: u8 = 1;
    pub const HAT: u8 = 2;
}

/// Digital-source indices into `RemapTable::buttons`, in
/// `ButtonState::from_raw` bit order.
pub mod source {
    pub const C: usize = 0;
    pub const B: usize = 1;
    pub const A: usize = 2;
    pub const START: usize = 3;
    pub const UP: usize = 4;
    pub const DOWN: usize = 5;
    pub const LEFT: usize = 6;
    pub const RIGHT: usize = 7;
    pub const Z: usize = 8;
    pub const Y: usize = 9;
    pub const X: usize = 10;
    pub const D: usize = 11;
    /// Number of digital sources.
    pub const COUNT: usize = 12;
}

/// Fixed-size remap table keyed by Dreamcast source (design v2 §2.1).
///
/// 20 bytes, no implicit padding — this layout IS the GATT `Map`/`StoredMap`
/// payload and the flash record body, so field order and width are ABI.
#[repr(C, align(4))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemapTable {
    /// Schema version; must be [`Self::VERSION`].
    pub version: u8,
    /// Orientation flags, see [`flags`]; bits 3–7 must be 0.
    pub flags: u8,
    /// Where the analog stick routes, see [`stick_dest`].
    pub stick_dest: u8,
    /// Stick deadzone radius in raw Dreamcast counts from centre.
    pub stick_deadzone: u8,
    /// Destination code per digital source, [`source`] order:
    /// c, b, a, start, up, down, left, right, z, y, x, d.
    pub buttons: [u8; source::COUNT],
    /// Destination code for the left analog trigger.
    pub trigger_l: u8,
    /// Destination code for the right analog trigger.
    pub trigger_r: u8,
    /// Analog→digital threshold (`value >= threshold`); valid 1–255. A
    /// threshold of 0 would permanently assert every Boolean destination of
    /// an analog source — not a remap but an always-on output, rejected
    /// (design v2 §4.4).
    pub trigger_threshold: u8,
    /// Must be 0 — a nonzero pad is a schema this firmware does not know,
    /// rejected rather than canonicalized.
    #[expect(
        clippy::pub_underscore_fields,
        reason = "design v2 §2.1 names the field _pad; it is explicit ABI padding, \
                  validated to zero, not an unused field"
    )]
    pub _pad: u8,
}

const _: () = assert!(core::mem::size_of::<RemapTable>() == RemapTable::LEN);
const _: () = assert!(core::mem::align_of::<RemapTable>() == 4);

impl RemapTable {
    /// The schema version this firmware speaks.
    pub const VERSION: u8 = 1;
    /// Serialized length in bytes.
    pub const LEN: usize = 20;

    /// Reproduces the pre-remap conversion bit-for-bit (pinned by the
    /// oracle tests, gate G7): A, B, X, Y and Start to their buttons, D-pad to hat
    /// directions, triggers to their axes, c/z/d unmapped, stick to
    /// `LeftStick` with `swap_xy` set.
    pub const DEFAULT: Self = Self {
        version: Self::VERSION,
        flags: flags::SWAP_XY,
        stick_dest: stick_dest::LEFT_STICK,
        stick_deadzone: 5,
        buttons: [
            dest::NONE, // c
            dest::B,
            dest::A,
            dest::START,
            dest::HAT_UP,
            dest::HAT_DOWN,
            dest::HAT_LEFT,
            dest::HAT_RIGHT,
            dest::NONE, // z
            dest::Y,
            dest::X,
            dest::NONE, // d — semantics untested across the accessory matrix
        ],
        trigger_l: dest::LEFT_TRIGGER,
        trigger_r: dest::RIGHT_TRIGGER,
        trigger_threshold: 128,
        _pad: 0,
    };

    /// Full schema validation (design v2 §4.4), applied to every GATT `Map`
    /// write and to the flash record at boot. Any failure rejects the whole
    /// table; nothing is partially applied.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        if self.version != Self::VERSION
            || self.flags & !flags::VALID_MASK != 0
            || self._pad != 0
            || self.stick_dest > stick_dest::HAT
            || self.trigger_threshold == 0
        {
            return false;
        }
        let mut i = 0;
        while i < source::COUNT {
            if !dest::is_valid(self.buttons[i]) {
                return false;
            }
            i += 1;
        }
        dest::is_valid(self.trigger_l) && dest::is_valid(self.trigger_r)
    }

    /// Serialize to the 20-byte wire/flash layout.
    #[must_use]
    pub const fn to_bytes(&self) -> [u8; Self::LEN] {
        let b = &self.buttons;
        [
            self.version,
            self.flags,
            self.stick_dest,
            self.stick_deadzone,
            b[0],
            b[1],
            b[2],
            b[3],
            b[4],
            b[5],
            b[6],
            b[7],
            b[8],
            b[9],
            b[10],
            b[11],
            self.trigger_l,
            self.trigger_r,
            self.trigger_threshold,
            self._pad,
        ]
    }

    /// Parse and validate a serialized table. `None` for any length other
    /// than exactly [`Self::LEN`] or any invalid field (§4.4).
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::LEN {
            return None;
        }
        let mut buttons = [0u8; source::COUNT];
        buttons.copy_from_slice(&bytes[4..16]);
        let table = Self {
            version: bytes[0],
            flags: bytes[1],
            stick_dest: bytes[2],
            stick_deadzone: bytes[3],
            buttons,
            trigger_l: bytes[16],
            trigger_r: bytes[17],
            trigger_threshold: bytes[18],
            _pad: bytes[19],
        };
        table.is_valid().then_some(table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        assert!(RemapTable::DEFAULT.is_valid());
    }

    /// The exact wire bytes of the default map — the ABI clients hardcode
    /// (the bench client's `MAP_DEFAULT`, story 003's fixtures). A change
    /// here is a protocol change, not a refactor.
    #[test]
    fn default_serializes_to_the_documented_wire_bytes() {
        assert_eq!(
            RemapTable::DEFAULT.to_bytes(),
            [1, 1, 1, 5, 0, 2, 1, 8, 16, 17, 18, 19, 0, 4, 3, 0, 0x20, 0x21, 128, 0]
        );
    }

    #[test]
    fn roundtrip_default() {
        let bytes = RemapTable::DEFAULT.to_bytes();
        assert_eq!(RemapTable::from_bytes(&bytes), Some(RemapTable::DEFAULT));
    }

    #[test]
    fn wrong_length_rejected() {
        let bytes = RemapTable::DEFAULT.to_bytes();
        assert!(RemapTable::from_bytes(&bytes[..19]).is_none());
        assert!(RemapTable::from_bytes(&[]).is_none());
        let mut long = [0u8; 21];
        long[..20].copy_from_slice(&bytes);
        assert!(RemapTable::from_bytes(&long).is_none());
    }

    #[test]
    fn every_valid_code_accepted_in_every_slot() {
        let codes = [
            dest::NONE,
            dest::A,
            dest::B,
            dest::X,
            dest::Y,
            dest::LB,
            dest::RB,
            dest::BACK,
            dest::START,
            dest::L3,
            dest::R3,
            dest::GUIDE,
            dest::HAT_UP,
            dest::HAT_DOWN,
            dest::HAT_LEFT,
            dest::HAT_RIGHT,
            dest::LEFT_TRIGGER,
            dest::RIGHT_TRIGGER,
        ];
        for &code in &codes {
            let mut t = RemapTable::DEFAULT;
            for i in 0..source::COUNT {
                t.buttons[i] = code;
            }
            t.trigger_l = code;
            t.trigger_r = code;
            assert!(t.is_valid(), "code {code:#04x} must be legal everywhere");
        }
    }

    #[test]
    fn invalid_codes_rejected() {
        for bad in [0x0Cu8, 0x0F, 0x14, 0x1F, 0x22, 0x30, 0x80, 0xFF] {
            let mut t = RemapTable::DEFAULT;
            t.buttons[0] = bad;
            assert!(!t.is_valid(), "button code {bad:#04x} must be rejected");

            let mut t = RemapTable::DEFAULT;
            t.trigger_l = bad;
            assert!(!t.is_valid(), "trigger code {bad:#04x} must be rejected");
        }
    }

    #[test]
    fn schema_fields_validated() {
        let mut t = RemapTable::DEFAULT;
        t.version = 2;
        assert!(!t.is_valid(), "unknown version");

        let mut t = RemapTable::DEFAULT;
        t.flags = flags::VALID_MASK + 1;
        assert!(!t.is_valid(), "reserved flag bits must be zero");

        let mut t = RemapTable::DEFAULT;
        t._pad = 1;
        assert!(!t.is_valid(), "nonzero pad is an unknown schema, rejected");

        let mut t = RemapTable::DEFAULT;
        t.stick_dest = 3;
        assert!(!t.is_valid(), "unknown stick destination");
    }

    #[test]
    fn trigger_threshold_range() {
        let mut t = RemapTable::DEFAULT;
        t.trigger_threshold = 0;
        assert!(
            !t.is_valid(),
            "threshold 0 is an always-on output, rejected"
        );
        t.trigger_threshold = 1;
        assert!(t.is_valid());
        t.trigger_threshold = 255;
        assert!(t.is_valid());
    }

    #[test]
    fn stick_deadzone_unrestricted() {
        for dz in [0u8, 5, 128, 255] {
            let mut t = RemapTable::DEFAULT;
            t.stick_deadzone = dz;
            assert!(t.is_valid());
        }
    }
}
