# SPDX-License-Identifier: GPL-3.0-or-later
"""End-to-end mapping proof via UHID + evdev (Linux/CI only).

Instantiates a REAL kernel HID device from each profile's descriptor (UHID),
injects our serialized report bytes, and reads the resulting evdev events from
the kernel input node — the same path a real Bluetooth controller takes. This is
the positive end-to-end mapping proof the Generic profile never had (plan
Stories 1-3; issue #7).

Runs only on Linux with /dev/uhid + libevdev (see requirements-linux.txt); the
CI job (.github/workflows/hid-evdev.yml) provides them. Skips elsewhere so the
macOS/Windows descriptor-layout tests still run.
"""
from __future__ import annotations

import contextlib
import os
import sys
import time

import pytest

import hidlayout

_linux = sys.platform.startswith("linux")
try:
    import libevdev
    from hidtools.uhid import UHIDDevice

    _have_deps = True
    _import_err = ""
except Exception as exc:  # pragma: no cover - exercised only off-Linux
    _have_deps = False
    _import_err = repr(exc)

pytestmark = pytest.mark.skipif(
    not (_linux and _have_deps),
    reason=f"UHID/evdev is Linux-only (linux={_linux}, deps={_have_deps} {_import_err})",
)

FIXTURES = hidlayout.load_fixtures()
REPORT_ID = 0x01
BUS_BLUETOOTH = 0x05
# (vid, pid) per profile — must match src/ble/profile.rs.
PROFILE_IDENTITY = {"xbox": (0x045E, 0x0B20), "generic": (0x1209, 0xDC01)}
PROFILES = list(PROFILE_IDENTITY)

# Left trigger = bytes 8-9, right trigger = bytes 10-11 (10-bit LE), per xbox_hid.rs.
LT_OFFSET, RT_OFFSET = 8, 10


def _payload(hexbytes: str) -> list[int]:
    return list(bytes.fromhex(hexbytes))


def _with_trigger(profile: str, offset: int, value: int = 0x03FF) -> list[int]:
    data = _payload(FIXTURES["neutral"][profile])
    data[offset] = value & 0xFF
    data[offset + 1] = (value >> 8) & 0x03
    return data


@contextlib.contextmanager
def gamepad(profile: str):
    """Create a UHID gamepad for `profile`, yield (uhid, libevdev.Device)."""
    dev = UHIDDevice()
    dev.rdesc = list(bytes.fromhex(FIXTURES["descriptors"][profile]))
    vid, pid = PROFILE_IDENTITY[profile]
    # bus/vid/pid are read-only; set them together via the info tuple.
    dev.info = (BUS_BLUETOOTH, vid, pid)
    dev.name = f"pulsar-dreamcast-{profile}"
    dev.create_kernel_device()

    evdev = None
    try:
        deadline = time.time() + 5
        while time.time() < deadline:
            dev.dispatch(0.05)
            if dev.device_nodes:
                break
        assert dev.device_nodes, f"{profile}: no evdev node appeared"
        fd = open(dev.device_nodes[0], "rb")
        os.set_blocking(fd.fileno(), False)
        evdev = libevdev.Device(fd)
        _drain(dev, evdev)
        yield dev, evdev
    finally:
        if evdev is not None:
            evdev.fd.close()
        dev.destroy()


def _drain(dev, evdev) -> None:
    for _ in range(3):
        dev.dispatch(0.02)
        for _ in evdev.events():
            pass


def _inject(dev, evdev, payload: list[int]) -> list:
    dev.call_input_event([REPORT_ID] + payload)
    events = []
    deadline = time.time() + 0.5
    while time.time() < deadline:
        dev.dispatch(0.02)
        batch = [e for e in evdev.events()]
        events += batch
        if any(e.matches(libevdev.EV_SYN) for e in batch):
            break
    return events


def _keys_pressed(events) -> set:
    return {e.code for e in events if e.matches(libevdev.EV_KEY) and e.value == 1}


def _abs_changes(events) -> dict:
    return {e.code: e.value for e in events if e.matches(libevdev.EV_ABS)}


@pytest.mark.parametrize("profile", PROFILES)
def test_device_enumerates_as_gamepad(profile):
    with gamepad(profile) as (_dev, evdev):
        assert evdev.has(libevdev.EV_KEY), f"{profile}: no buttons (EV_KEY)"
        assert evdev.has(libevdev.EV_ABS), f"{profile}: no axes (EV_ABS)"


@pytest.mark.parametrize("profile", PROFILES)
def test_each_button_maps_to_a_distinct_key(profile):
    """Every fixture button must produce at least one EV_KEY, and no two
    different fixture buttons may collapse onto the same key."""
    seen = {}
    with gamepad(profile) as (dev, evdev):
        for name, payloads in FIXTURES["buttons"].items():
            pressed = _keys_pressed(_inject(dev, evdev, _payload(payloads[profile])))
            _inject(dev, evdev, _payload(FIXTURES["neutral"][profile]))  # release
            assert pressed, f"{profile}/{name}: no button event fired"
            for code in pressed:
                assert code not in seen or seen[code] == name, (
                    f"{profile}: '{name}' and '{seen[code]}' both map to {code}"
                )
                seen[code] = name


@pytest.mark.parametrize("profile", PROFILES)
def test_triggers_are_independent_axes(profile):
    """The core issue-#7 property: LT and RT must drive two DIFFERENT analog
    axes, and pressing one must not move the other's axis."""
    with gamepad(profile) as (dev, evdev):
        lt = _abs_changes(_inject(dev, evdev, _with_trigger(profile, LT_OFFSET)))
        _inject(dev, evdev, _payload(FIXTURES["neutral"][profile]))
        rt = _abs_changes(_inject(dev, evdev, _with_trigger(profile, RT_OFFSET)))

        lt_axes = {c for c, v in lt.items() if v != 0}
        rt_axes = {c for c, v in rt.items() if v != 0}
        assert lt_axes, f"{profile}: left trigger moved no axis"
        assert rt_axes, f"{profile}: right trigger moved no axis"
        assert lt_axes.isdisjoint(rt_axes), (
            f"{profile}: triggers collapse onto a shared axis "
            f"(LT={lt_axes}, RT={rt_axes}) — the combined-axis bug"
        )
