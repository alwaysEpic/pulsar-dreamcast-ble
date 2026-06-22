# SPDX-License-Identifier: GPL-3.0-or-later
"""Descriptor-layout regression + known-standard alignment tests.

These run anywhere (pure-Python ``hid-tools`` parser, no kernel needed). They
prove two things the project cares about:

  1. REGRESSION: the meaningful field layout of each profile's main report does
     not change unexpectedly (complements the Rust byte-level golden snapshot in
     maple-protocol/tests/blueretro_fixtures.rs).
  2. STANDARD ALIGNMENT: the Xbox profile matches the real Xbox One S raw
     descriptor (right stick Z/Rz, triggers on the Simulation page
     Brake/Accelerator), and the Generic profile's divergence from that raw
     layout is documented (see issue #7).

The end-to-end "kernel actually maps this to BTN_SOUTH/ABS_Z" proof lives in
test_evdev_mapping.py (Linux/CI only).
"""
from __future__ import annotations

import hidlayout
import pytest

FIXTURES = hidlayout.load_fixtures()


def _buttons(start_byte: int = 13, count: int = 15) -> dict:
    """Expected (byte,bit) -> (size, usages) for contiguous Buttons 1..count."""
    out = {}
    for i in range(count):
        byte = start_byte + (i // 8)
        bit = i % 8
        out[(byte, bit)] = (1, (f"Button{i + 1}",))
    return out


# Expected semantic layout (named, non-padding fields keyed by byte/bit offset).
# Sourced from the decoded descriptors; this IS the regression lock + the
# documentation of each profile's intended shape.
EXPECTED_XBOX = {
    (0, 0): (16, ("GD.X",)),
    (2, 0): (16, ("GD.Y",)),
    (4, 0): (16, ("GD.Z",)),       # right stick X  -> Z   (real Xbox layout)
    (6, 0): (16, ("GD.Rz",)),      # right stick Y  -> Rz
    (8, 0): (10, ("Sim.Brake",)),         # left trigger  -> Simulation page
    (10, 0): (10, ("Sim.Accelerator",)),  # right trigger -> Simulation page
    (12, 0): (4, ("GD.Hat",)),
    **_buttons(),
    (15, 0): (1, ("Consumer.0x00b2",)),
}

EXPECTED_GENERIC = {
    (0, 0): (16, ("GD.X",)),
    (2, 0): (16, ("GD.Y",)),
    (4, 0): (16, ("GD.Rx",)),      # right stick X  -> Rx  (xpadneo-remapped)
    (6, 0): (16, ("GD.Ry",)),      # right stick Y  -> Ry
    (8, 0): (10, ("GD.Z",)),       # left trigger  -> Z    (NOT Simulation page)
    (10, 0): (10, ("GD.Rz",)),     # right trigger -> Rz
    (12, 0): (4, ("GD.Hat",)),
    **_buttons(),
    (15, 0): (1, ("Consumer.0x0224",)),
}


@pytest.mark.parametrize(
    "profile,expected", [("xbox", EXPECTED_XBOX), ("generic", EXPECTED_GENERIC)]
)
def test_report1_layout_is_stable(profile, expected):
    """Lock the meaningful field layout of each profile against drift."""
    got = hidlayout.semantic_summary(FIXTURES["descriptors"][profile])
    assert got == expected, (
        f"{profile} report-1 layout changed.\n"
        f"  got:      {sorted(got.items())}\n"
        f"  expected: {sorted(expected.items())}\n"
        "If this was intentional, update EXPECTED_* and regenerate fixtures.json."
    )


@pytest.mark.parametrize("profile", ["xbox", "generic"])
def test_triggers_are_two_separate_10bit_fields(profile):
    """Both profiles must keep LT and RT as two distinct 10-bit analog fields
    (the core property the combined-axis pitfall would violate)."""
    summary = hidlayout.semantic_summary(FIXTURES["descriptors"][profile])
    lt = summary.get((8, 0))
    rt = summary.get((10, 0))
    assert lt and lt[0] == 10, f"{profile}: left trigger not a 10-bit field at byte 8"
    assert rt and rt[0] == 10, f"{profile}: right trigger not a 10-bit field at byte 10"
    assert lt[1] != rt[1], f"{profile}: LT and RT share a usage (collapsed axis): {lt[1]}"


def test_xbox_profile_matches_real_xbox_raw_layout():
    """The Xbox profile is the faithful real-Xbox-One-S descriptor: right stick on
    Z/Rz and triggers on the Simulation page (Brake/Accelerator). This is what
    BlueRetro, the kernel hid-microsoft driver, and retro adapters expect."""
    s = hidlayout.semantic_summary(FIXTURES["descriptors"]["xbox"])
    assert s[(4, 0)][1] == ("GD.Z",) and s[(6, 0)][1] == ("GD.Rz",)
    assert s[(8, 0)][1] == ("Sim.Brake",)
    assert s[(10, 0)][1] == ("Sim.Accelerator",)


def test_generic_profile_diverges_from_real_xbox_raw_layout():
    """DOCUMENTED STATE (issue #7 history): the Generic profile uses the
    xpadneo-remapped convention (right stick Rx/Ry, triggers GD.Z/Rz) rather than
    the real raw Xbox layout (Z/Rz right stick, Simulation Brake/Accelerator).

    Generic now serves this descriptor under the recognized 0x045E/0x0B20 identity
    because macOS/browser Gamepad API did not surface the neutral 0x1209/0xDC01
    experiment. This test still locks the pre-remapped layout; if the Generic
    profile is ever realigned to the raw Xbox layout, THIS test will fail — the
    signal to revisit the alignment decision (plan Story 6)."""
    s = hidlayout.semantic_summary(FIXTURES["descriptors"]["generic"])
    assert s[(4, 0)][1] == ("GD.Rx",) and s[(6, 0)][1] == ("GD.Ry",), (
        "Generic right stick no longer Rx/Ry — it may now match the raw Xbox "
        "layout; revisit the Generic alignment decision."
    )
    assert s[(8, 0)][1] == ("GD.Z",) and s[(10, 0)][1] == ("GD.Rz",), (
        "Generic triggers no longer on GD.Z/Rz — revisit the alignment decision."
    )
