# SPDX-License-Identifier: GPL-3.0-or-later
"""Shared helpers for the HID descriptor / evdev mapping tests.

Loads the Rust-generated fixtures (the single source of truth — see
``maple-protocol/tests/blueretro_fixtures.rs``) and decodes a profile's main
input report (Report ID 1) into a human-readable field layout using the
freedesktop ``hid-tools`` parser. The parser is pure-Python and runs on any OS;
only the UHID/evdev layer (``test_evdev_mapping.py``) needs Linux.
"""
from __future__ import annotations

import json
import pathlib

from hidtools.hid import ReportDescriptor

# fixtures.json lives next to the BlueRetro harness; both consume the same file.
FIXTURES_PATH = pathlib.Path(__file__).resolve().parents[1] / "blueretro" / "fixtures.json"

# HID usage names we care about (page << 16 | id).
_GENERIC_DESKTOP = {
    0x30: "X", 0x31: "Y", 0x32: "Z", 0x33: "Rx", 0x34: "Ry", 0x35: "Rz", 0x39: "Hat",
}
_SIMULATION = {0xC4: "Accelerator", 0xC5: "Brake"}


def usage_name(usage: int) -> str:
    """Render a 32-bit HID usage (page<<16 | id) as a short stable name."""
    usage = int(usage)
    page, uid = (usage >> 16) & 0xFFFF, usage & 0xFFFF
    if page == 0x01 and uid in _GENERIC_DESKTOP:
        return f"GD.{_GENERIC_DESKTOP[uid]}"
    if page == 0x02 and uid in _SIMULATION:
        return f"Sim.{_SIMULATION[uid]}"
    if page == 0x09:
        return f"Button{uid}"
    if page == 0x0C:
        return f"Consumer.0x{uid:04x}"
    if page == 0x0F:
        return f"PID.0x{uid:04x}"
    return f"0x{page:02x}.0x{uid:04x}"


def load_fixtures() -> dict:
    return json.loads(FIXTURES_PATH.read_text())


def _field_usages(field) -> tuple[str, ...]:
    raw = getattr(field, "usages", None)
    if not raw:
        single = getattr(field, "usage", None)
        raw = [single] if single else []
    return tuple(usage_name(u) for u in raw)


def report1_layout(descriptor_hex: str) -> list[tuple[int, int, int, tuple[str, ...]]]:
    """Decode Report ID 1 into ``(byte, bit, size_bits, usage_names)`` rows.

    Offsets are relative to the start of the 16-byte report payload (the 8-bit
    Report ID prefix is subtracted out), so they line up with the byte layout
    documented in ``xbox_hid.rs``. Constant/padding fields decode to an empty
    usage tuple.
    """
    rdesc = ReportDescriptor.from_bytes(list(bytes.fromhex(descriptor_hex)))
    rows = []
    for field in rdesc.input_reports[1]:
        data_bit = field.start - 8  # drop the Report ID byte
        rows.append((data_bit // 8, data_bit % 8, field.size, _field_usages(field)))
    return rows


def semantic_summary(descriptor_hex: str) -> dict:
    """Collapse the report-1 layout to the parts that define the controller's
    'shape': the named non-padding fields keyed by (byte, bit)."""
    summary = {}
    for byte, bit, size, usages in report1_layout(descriptor_hex):
        if usages:
            summary[(byte, bit)] = (size, usages)
    return summary
