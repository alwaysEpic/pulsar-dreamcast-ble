// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright 2025-2026 alwaysEpic

//! Default-behaviour oracle for button remapping (remap design v2 §2.4,
//! gate G7). Pins today's `ControllerState` → `GamepadReport` conversion
//! three ways, all independent of the remap code path:
//!
//! 1. `legacy_to_gamepad_report` — a verbatim, test-only copy of the
//!    pre-remap conversion body (in `controller_state.rs`, next to the
//!    private constants it reads);
//! 2. golden 16-byte outputs for **both** serializers, captured from the
//!    pre-remap code at the base of the remap build — regenerate only via
//!    `print_goldens`, which runs the legacy reference, never production;
//! 3. [`legacy_guide_pipeline`] — the BLE task's post-hoc Guide-chord
//!    suppression exactly as `src/ble/task.rs` shipped it, which the remap
//!    moves to source level; `to_gamepad_report_with(&RemapTable::DEFAULT, ..)`
//!    must match it bit-for-bit.
//!
//! These tests must never delegate to the new code (story 002 step 1).

extern crate std;

use std::vec::Vec;

use crate::controller_state::{legacy_to_gamepad_report, ButtonState, ControllerState};
use crate::guide_chord::{GuideChord, GUIDE_HOLD_MS, GUIDE_PULSE_MS, GUIDE_TRIGGER_THRESHOLD};
use crate::xbox_hid::{buttons, GamepadReport};

/// The conversion + Guide handling as the BLE notify loop shipped it
/// pre-remap: post-hoc suppression of Start and both triggers on every frame
/// the chord is held, Guide OR-ed in during the pulse. Copied from
/// `src/ble/task.rs`; the remap build replaces the post-hoc block with
/// source-level exclusion and must stay equivalent under the default map.
pub fn legacy_guide_pipeline(
    state: &ControllerState,
    chord: &mut GuideChord,
    now_ms: u64,
) -> GamepadReport {
    let mut report = legacy_to_gamepad_report(*state);
    let out = chord.update(state, now_ms);
    if out.suppress {
        report.buttons &= !buttons::START;
        report.left_trigger = 0;
        report.right_trigger = 0;
    }
    if out.emit_guide {
        report.buttons |= buttons::GUIDE;
    }
    report
}

/// Build a state with the given buttons pressed, by `ButtonState::from_raw`
/// bit index (c=0, b=1, a=2, start=3, up=4, down=5, left=6, right=7, z=8,
/// y=9, x=10, d=11), everything else neutral.
pub fn buttons_pressed(bits: &[u16]) -> ControllerState {
    let mut raw: u16 = 0;
    for &bit in bits {
        raw |= 1 << bit;
    }
    ControllerState {
        buttons: ButtonState::from_raw(!raw),
        ..ControllerState::default()
    }
}

/// The oracle input corpus from design v2 §2.4: every digital button alone,
/// the chord-relevant combinations, all 16 D-pad combinations, the trigger
/// ladder (noise floor, digital-threshold ±1, midpoint, endpoints, asymmetry)
/// and the stick grid (centre, deadzone ±1 on both sides of both axes,
/// extrema, and an asymmetric probe that catches a lost axis swap).
///
/// Order is load-bearing: the golden arrays index into it. Append new cases
/// at the end and regenerate; never reorder.
pub fn corpus() -> Vec<ControllerState> {
    let mut v = Vec::new();

    // Neutral (also the disconnect state — issue #6).
    v.push(ControllerState::default());

    // Every digital button alone.
    for bit in 0..12 {
        v.push(buttons_pressed(&[bit]));
    }

    // Chord-relevant combinations as raw states (the chord *machine* is
    // time-driven and exercised in the pipeline tests below; these pin the
    // plain conversion of the constituent states).
    let chorded = |base: ControllerState, l: u8, r: u8| ControllerState {
        trigger_l: l,
        trigger_r: r,
        ..base
    };
    // L+R+Start fully pulled — the Guide chord.
    v.push(chorded(buttons_pressed(&[3]), 255, 255));
    // The chord plus an unrelated held button.
    v.push(chorded(buttons_pressed(&[2, 3]), 255, 255));
    // Start with a face button, no triggers.
    v.push(buttons_pressed(&[2, 3]));
    // A+B+X+Y+Start — the Dreamcast soft-reset chord, must pass through.
    v.push(buttons_pressed(&[1, 2, 3, 9, 10]));
    // Triggers exactly at / one below the chord threshold.
    v.push(chorded(
        buttons_pressed(&[3]),
        GUIDE_TRIGGER_THRESHOLD,
        GUIDE_TRIGGER_THRESHOLD,
    ));
    v.push(chorded(
        buttons_pressed(&[3]),
        GUIDE_TRIGGER_THRESHOLD - 1,
        255,
    ));

    // All 16 D-pad combinations (up=bit4 .. right=bit7).
    for dpad in 0..16u16 {
        v.push(ControllerState {
            buttons: ButtonState::from_raw(!(dpad << 4)),
            ..ControllerState::default()
        });
    }

    // Trigger ladder: noise floor edge (5/6), the default digital threshold
    // ±1 (127/128/129), midpoint-ish, and both endpoints.
    for t in [1u8, 5, 6, 127, 128, 129, 200, 254, 255] {
        v.push(ControllerState {
            trigger_l: t,
            trigger_r: t,
            ..ControllerState::default()
        });
    }
    // Asymmetric triggers — catches a swapped L/R.
    for (l, r) in [(200u8, 0u8), (0, 200), (255, 0), (0, 255)] {
        v.push(ControllerState {
            trigger_l: l,
            trigger_r: r,
            ..ControllerState::default()
        });
    }

    // Stick grid: centre; deadzone ±1 both sides of both axes (|v-128| < 5
    // clamps to centre, so 124/132 are inside, 123/133 the first values out);
    // extrema; on-axis extremes and an asymmetric probe (64, 200) — the
    // asymmetric cases catch a lost X↔Y transposition.
    for (x, y) in [
        (128u8, 128u8),
        (124, 128),
        (123, 128),
        (132, 128),
        (133, 128),
        (128, 124),
        (128, 123),
        (128, 132),
        (128, 133),
        (0, 0),
        (255, 255),
        (0, 255),
        (255, 0),
        (0, 128),
        (255, 128),
        (128, 0),
        (128, 255),
        (64, 200),
    ] {
        v.push(ControllerState {
            stick_x: x,
            stick_y: y,
            ..ControllerState::default()
        });
    }

    v
}

const GOLDEN_CONTIGUOUS: &[[u8; 16]] = &[
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 2, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 1, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 128, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 1, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 5, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 7, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 3, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 8, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 4, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 255, 3, 255, 3, 0, 128, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 255, 3, 255, 3, 0, 129, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 129, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 143, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 34, 3, 34, 3, 0, 128, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 30, 3, 255, 3, 0, 128, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 1, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 5, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 7, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 8, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 6, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 3, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 2, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 4, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 4, 0, 4, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 20, 0, 20, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 24, 0, 24, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 253, 1, 253, 1, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 1, 2, 1, 2, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 5, 2, 5, 2, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 34, 3, 34, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 250, 3, 250, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 255, 3, 255, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 34, 3, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 34, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 255, 3, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 255, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 123, 123, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 133, 133, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [123, 123, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [133, 133, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [255, 255, 255, 255, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [255, 255, 0, 0, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 255, 255, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 0, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 255, 255, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [255, 255, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [200, 200, 64, 64, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
];

const GOLDEN_XBOX_WIRE: &[[u8; 16]] = &[
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 2, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 1, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 8, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 1, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 5, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 7, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 3, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 16, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 8, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 255, 3, 255, 3, 0, 0, 8, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 255, 3, 255, 3, 0, 1, 8, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 1, 8, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 27, 8, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 34, 3, 34, 3, 0, 0, 8, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 30, 3, 255, 3, 0, 0, 8, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 1, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 5, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 7, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 8, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 6, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 3, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 2, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 4, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 4, 0, 4, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 20, 0, 20, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 24, 0, 24, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 253, 1, 253, 1, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 1, 2, 1, 2, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 5, 2, 5, 2, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 34, 3, 34, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 250, 3, 250, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 255, 3, 255, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 34, 3, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 34, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 255, 3, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 255, 3, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 123, 123, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 133, 133, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [123, 123, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [133, 133, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [255, 255, 255, 255, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [255, 255, 0, 0, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 255, 255, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 0, 0, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 128, 255, 255, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [255, 255, 0, 128, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
    [200, 200, 64, 64, 0, 128, 0, 128, 0, 0, 0, 0, 0, 0, 0, 0],
];

/// Regenerates the golden arrays from the LEGACY reference — never from the
/// production path, so a regeneration cannot launder a behaviour change:
/// `cargo test -p maple-protocol print_goldens -- --ignored --nocapture`
#[test]
#[ignore = "golden generator, not a check — prints the const arrays to paste above"]
fn print_goldens() {
    let states = corpus();
    std::println!("const GOLDEN_CONTIGUOUS: &[[u8; 16]] = &[");
    for s in &states {
        std::println!("    {:?},", legacy_to_gamepad_report(*s).to_bytes());
    }
    std::println!("];");
    std::println!();
    std::println!("const GOLDEN_XBOX_WIRE: &[[u8; 16]] = &[");
    for s in &states {
        std::println!("    {:?},", legacy_to_gamepad_report(*s).to_bytes_ms());
    }
    std::println!("];");
}

/// The production conversion matches the goldens on both serializers. After
/// the remap lands, this is the assertion that `to_gamepad_report()` becoming
/// `to_gamepad_report_with(&DEFAULT, ..)` changed nothing.
#[test]
fn production_conversion_matches_goldens() {
    let states = corpus();
    assert_eq!(
        states.len(),
        GOLDEN_CONTIGUOUS.len(),
        "corpus/golden length mismatch — regenerate via print_goldens"
    );
    assert_eq!(states.len(), GOLDEN_XBOX_WIRE.len());
    for (i, s) in states.iter().enumerate() {
        let r = s.to_gamepad_report();
        assert_eq!(
            r.to_bytes(),
            GOLDEN_CONTIGUOUS[i],
            "contiguous serializer diverged at corpus[{i}]: {s:?}"
        );
        assert_eq!(
            r.to_bytes_ms(),
            GOLDEN_XBOX_WIRE[i],
            "Xbox wire serializer diverged at corpus[{i}]: {s:?}"
        );
    }
}

/// The verbatim legacy copy matches the goldens too — pins the copy itself
/// against drift, so the equivalence tests have a trustworthy reference.
#[test]
fn legacy_reference_matches_goldens() {
    let states = corpus();
    assert_eq!(states.len(), GOLDEN_CONTIGUOUS.len());
    for (i, s) in states.iter().enumerate() {
        let r = legacy_to_gamepad_report(*s);
        assert_eq!(
            r.to_bytes(),
            GOLDEN_CONTIGUOUS[i],
            "legacy reference diverged at corpus[{i}]: {s:?}"
        );
        assert_eq!(r.to_bytes_ms(), GOLDEN_XBOX_WIRE[i]);
    }
}

/// While production still routes through the legacy body, the two must agree
/// on the whole corpus. Kept after the remap lands as the direct
/// function-vs-reference oracle (goldens catch drift; this catches it with a
/// readable diff).
#[test]
fn production_matches_legacy_reference() {
    for (i, s) in corpus().iter().enumerate() {
        let prod = s.to_gamepad_report();
        let legacy = legacy_to_gamepad_report(*s);
        assert_eq!(prod.to_bytes(), legacy.to_bytes(), "corpus[{i}]: {s:?}");
        assert_eq!(
            prod.to_bytes_ms(),
            legacy.to_bytes_ms(),
            "corpus[{i}]: {s:?}"
        );
    }
}

fn guide_chord_with_a() -> ControllerState {
    ControllerState {
        trigger_l: 255,
        trigger_r: 255,
        ..buttons_pressed(&[2, 3]) // A + Start
    }
}

#[test]
fn pipeline_suppresses_constituents_from_first_frame() {
    let mut chord = GuideChord::default();
    // Chord held with A also down: A survives; Start and both triggers are
    // suppressed from the very first frame, before Guide fires.
    let r = legacy_guide_pipeline(&guide_chord_with_a(), &mut chord, 0);
    assert_eq!(r.buttons & buttons::START, 0, "Start must not leak");
    assert_ne!(r.buttons & buttons::A, 0, "unrelated button passes through");
    assert_eq!(r.left_trigger, 0);
    assert_eq!(r.right_trigger, 0);
    assert_eq!(r.buttons & buttons::GUIDE, 0, "Guide has not fired yet");
}

#[test]
fn pipeline_guide_is_a_tap_then_keeps_suppressing() {
    let mut chord = GuideChord::default();
    let s = guide_chord_with_a();
    legacy_guide_pipeline(&s, &mut chord, 0);
    // At the hold threshold the pulse starts.
    let r = legacy_guide_pipeline(&s, &mut chord, GUIDE_HOLD_MS);
    assert_ne!(
        r.buttons & buttons::GUIDE,
        0,
        "Guide fires at the threshold"
    );
    assert_eq!(r.buttons & buttons::START, 0);
    // Past the pulse window, still held: Guide releases (a tap), the
    // constituents stay suppressed.
    let r = legacy_guide_pipeline(&s, &mut chord, GUIDE_HOLD_MS + GUIDE_PULSE_MS);
    assert_eq!(r.buttons & buttons::GUIDE, 0, "Guide is a tap, not a hold");
    assert_eq!(r.buttons & buttons::START, 0);
    assert_eq!(r.left_trigger, 0);
    assert_eq!(r.right_trigger, 0);
    assert_ne!(r.buttons & buttons::A, 0);
}

#[test]
fn pipeline_leaves_soft_reset_chord_untouched() {
    let mut chord = GuideChord::default();
    // A+B+X+Y+Start with no triggers is the Dreamcast/GDEMU soft-reset chord
    // and must pass through unsuppressed even under sustained hold.
    let s = buttons_pressed(&[1, 2, 3, 9, 10]);
    for t in [0, GUIDE_HOLD_MS, GUIDE_HOLD_MS * 2] {
        let r = legacy_guide_pipeline(&s, &mut chord, t);
        assert_ne!(r.buttons & buttons::START, 0);
        assert_ne!(r.buttons & buttons::A, 0);
        assert_ne!(r.buttons & buttons::B, 0);
        assert_ne!(r.buttons & buttons::X, 0);
        assert_ne!(r.buttons & buttons::Y, 0);
        assert_eq!(r.buttons & buttons::GUIDE, 0);
    }
}

/// The remapped conversion under `DEFAULT` must match the legacy pipeline —
/// post-hoc suppression replaced by source-level exclusion — over the whole
/// corpus and the chord timeline, on both serializers (gates G6/G7).
#[test]
fn remap_default_matches_legacy_pipeline_over_time() {
    use crate::remap::RemapTable;
    for (i, s) in corpus().iter().enumerate() {
        let mut legacy_chord = GuideChord::default();
        let mut new_chord = GuideChord::default();
        for t in [
            0,
            100,
            GUIDE_HOLD_MS - 1,
            GUIDE_HOLD_MS,
            GUIDE_HOLD_MS + GUIDE_PULSE_MS - 1,
            GUIDE_HOLD_MS + GUIDE_PULSE_MS,
            GUIDE_HOLD_MS + 1000,
        ] {
            let legacy = legacy_guide_pipeline(s, &mut legacy_chord, t);
            let (new, _) = s.to_gamepad_report_with(&RemapTable::DEFAULT, &mut new_chord, t);
            assert_eq!(
                new.to_bytes(),
                legacy.to_bytes(),
                "corpus[{i}] at t={t}: {s:?}"
            );
            assert_eq!(
                new.to_bytes_ms(),
                legacy.to_bytes_ms(),
                "corpus[{i}] at t={t}"
            );
        }
    }
}
