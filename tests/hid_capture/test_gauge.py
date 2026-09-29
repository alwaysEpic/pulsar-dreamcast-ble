# SPDX-License-Identifier: GPL-3.0-or-later
"""The gauge decoder keeps "unknown" unknown, and keeps returns to earlier values.

The firmware reports each of its three battery reads separately, because a
failed read is not `false` and an undecodable byte is not 0 %. A check of
the low-cell fix fed the old decoder an all-reads-failed sample and got
"255 % discharging" — the interpretation the firmware had just been changed to
refuse. The same check found the timeline's global `seen` set dropping every
return to an earlier value, so 25 % -> 0 % -> 25 % lost its rebound: the one
thing a discharge run is for.

The layout under test is the contract with `src/lib.rs::publish_gauge_sample`.
"""
import hid_capture as hc

CHARGING, FULL = hc.GAUGE_FLAG_CHARGING, hc.GAUGE_FLAG_FULL
CHG_UNREAD, FULL_UNREAD = hc.GAUGE_FLAG_CHARGING_UNREAD, hc.GAUGE_FLAG_FULL_UNREAD
GAUGE_UNREAD = hc.GAUGE_FLAG_GAUGE_UNREAD


def report(raw, pct, flags, seq=0, magic=hc.GAUGE_MAGIC):
    """One 16-byte HID report carrying a gauge sample in bytes 4-7."""
    p = bytearray(16)
    p[4], p[5], p[6], p[7] = raw, pct, flags | (seq << hc.GAUGE_SEQ_SHIFT), magic
    return bytes(p)


def timeline(*reports):
    return hc.analyze_gauge([(float(i), r) for i, r in enumerate(reports)])["timeline"]


def test_a_clean_sample_decodes_as_before():
    (e,) = timeline(report(0xE0, 25, 0))
    assert (e["gauge"], e["raw"], e["percent"]) == ("ok", 0xE0, 25)
    assert (e["charging"], e["full"]) == (False, False)
    assert hc.gauge_state(e) == "discharging"


def test_all_reads_failed_is_not_255_percent_discharging():
    (e,) = timeline(report(0x00, 0xFF, GAUGE_UNREAD | CHG_UNREAD | FULL_UNREAD))
    assert e["gauge"] == "unread"
    assert e["raw"] is None and e["percent"] is None
    assert e["charging"] is None and e["full"] is None
    assert hc.gauge_state(e) == "charge state unknown"


def test_an_undecodable_byte_keeps_its_raw_value():
    (e,) = timeline(report(0x70, 0xFF, 0))
    assert (e["gauge"], e["raw"], e["percent"]) == ("undecodable", 0x70, None)


def test_a_real_zero_percent_is_not_confused_with_unknown():
    (e,) = timeline(report(0xF0, 0, 0))
    assert (e["gauge"], e["percent"]) == ("ok", 0)


def test_unknown_charging_is_not_discharging():
    (e,) = timeline(report(0xF0, 0, CHG_UNREAD))
    assert e["charging"] is None and e["full"] is False
    assert hc.gauge_state(e) == "charging unknown, not full"


def test_unknown_full_is_said_so():
    (e,) = timeline(report(0x00, 100, FULL_UNREAD))
    assert e["charging"] is False and e["full"] is None
    assert hc.gauge_state(e) == "not charging, full unknown"


def test_charging_and_full_still_read():
    chg, full = timeline(report(0x80, 75, CHARGING), report(0x00, 100, FULL))
    assert hc.gauge_state(chg) == "charging"
    assert hc.gauge_state(full) == "full"


def test_a_return_to_an_earlier_value_is_kept():
    levels = [e["percent"] for e in timeline(
        report(0xE0, 25, 0), report(0xF0, 0, 0), report(0xE0, 25, 0))]
    assert levels == [25, 0, 25]


def test_one_measurement_in_many_reports_is_one_entry():
    assert len(timeline(*[report(0xE0, 25, 0, seq=1)] * 50)) == 1


def test_the_sequence_counter_separates_identical_measurements():
    entries = timeline(
        report(0xF0, 0, 0, seq=0), report(0xF0, 0, 0, seq=0),
        report(0xF0, 0, 0, seq=1), report(0xF0, 0, 0, seq=2))
    assert [e["seq"] for e in entries] == [0, 1, 2]


def test_a_build_without_the_counter_still_decodes():
    # Older gauge-debug builds leave bits 3:2 clear: repeats collapse, changes do not.
    levels = [e["percent"] for e in timeline(
        report(0xE0, 25, 0), report(0xE0, 25, 0), report(0xF0, 0, 0))]
    assert levels == [25, 0]


def test_reports_without_the_magic_byte_are_counted_not_decoded():
    g = hc.analyze_gauge([(0.0, report(0xE0, 25, 0, magic=0x80)), (1.0, report(0xE0, 25, 0))])
    assert g["non_magic"] == 1 and len(g["timeline"]) == 1
