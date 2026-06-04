''' Regression test: does a BlueRetro adapter map our STD profile correctly?

This reproduces issue #2 (https://github.com/alwaysEpic/pulsar-dreamcast-ble/issues/2)
in software, with no nRF hardware. It runs against BlueRetro's own firmware under
QEMU (qemu-system-xtensa), driven over the injector websocket:

  send_name      -> advertise as our STD profile does ("Xbox Wireless Controller")
  send_hid_desc  -> hand BlueRetro the exact HID report descriptor our firmware serves
  send_to_bridge -> feed the exact 16-byte report bytes our firmware serializes
                    (GamepadReport::to_bytes_ms) for each physical button

We then assert each button decodes to the same BlueRetro-*generic* button a real
Xbox One S BLE controller produces (modelled by device_data/xbox.py upstream). If
our descriptor does not parse the way the genuine Xbox descriptor does, the buttons
land on the wrong generic functions here exactly as they do on a real Dreamcast.

Fixtures (descriptor + report bytes) are generated from the Rust source and kept in
sync by `maple-protocol/tests/blueretro_fixtures.rs` (runs in scripts/ci.sh). This
file is copied into BlueRetro's tests/ dir at CI time so the imports below resolve
against their conftest/injector/device_data.
'''
import json
import os

import pytest

from bit_helper import bit
from device_data.br import system, dev_mode, bt_conn_type
from device_data.xbox import xbox_ble_btns_mask


# Matches PROFILE_STD.gap_name in src/ble/profile.rs.
DEVICE_NAME = 'Xbox Wireless Controller'

with open(os.path.join(os.path.dirname(__file__), 'fixtures.json'), encoding='utf-8') as _f:
    FIXTURES = json.load(_f)


def _wire_btns(report_hex):
    ''' Extract the 24-bit button field (bytes 13-15) from a 16-byte report. '''
    b = bytes.fromhex(report_hex)
    return b[13] | (b[14] << 8) | (b[15] << 16)


def _expected_generic(wire):
    ''' Mirror BlueRetro's own bit->generic mapping for the Xbox BLE controller,
        so this stays in lockstep with upstream device_data/xbox.py. '''
    generic = 0
    for gen_bit, src_mask in enumerate(xbox_ble_btns_mask):
        if src_mask and (wire & src_mask):
            generic |= bit(gen_bit)
    return generic


@pytest.mark.parametrize('blueretro', [[system.DC, dev_mode.PAD, bt_conn_type.BT_LE]], indirect=True)
def test_pulsar_std_buttons_mapping(blueretro):
    ''' Each physical button on the STD profile must decode to the right
        BlueRetro-generic button (the bug in issue #2). '''
    rsp = blueretro.send_name(DEVICE_NAME)
    assert rsp['type_update']['device_id'] == 0

    blueretro.send_hid_desc(bytes.fromhex(FIXTURES['descriptors']['std']))

    # Prime the adapter with a couple of neutral reports first.
    for _ in range(2):
        blueretro.send_to_bridge(0x01, FIXTURES['neutral']['std'])

    failures = []
    for name, reports in FIXTURES['buttons'].items():
        report = reports['std']
        rsp = blueretro.send_to_bridge(0x01, report)

        wire = _wire_btns(report)
        expected = _expected_generic(wire)
        got = rsp['generic_input']['btns'][0]

        if rsp['wireless_input']['btns'] & 0xFFFFFF != wire:
            failures.append(
                f"{name}: descriptor parse mismatch — "
                f"sent wire=0x{wire:06x}, BlueRetro read=0x{rsp['wireless_input']['btns'] & 0xFFFFFF:06x}")
        if got != expected:
            failures.append(
                f"{name}: wire=0x{wire:06x} expected generic=0x{expected:08x} got=0x{got:08x}")

    assert not failures, (
        "STD profile mis-maps on BlueRetro:\n  " + "\n  ".join(failures))
