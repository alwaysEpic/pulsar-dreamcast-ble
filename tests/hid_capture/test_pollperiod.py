# SPDX-License-Identifier: GPL-3.0-or-later
"""The poll-period decoder's reset and saturation handling.

Two ways were reproduced in which this channel reported a healthy
run that had not happened:

  * a counter reset read as a wrap, turning 20,000 -> 100 into 45,636 polls of
    exposure with zero failures; and
  * a saturated counter printing a derived "0 of 12,000" *underneath* its own
    ceiling warning, which reads as a pass.

Both are regressions waiting to happen again — the arithmetic that produced
them is the obvious arithmetic. These tests hold the decoder to reporting
nothing rather than reporting those.
"""
import hid_capture as hc
import pytest


def report(tag, value, window=0):
    """One 16-byte HID report carrying `value` on `tag`."""
    p = bytearray(16)
    p[4:6] = int(value).to_bytes(2, "little")
    p[6] = window & 0xFF
    p[7] = hc.PP_MAGIC_BASE | tag
    return bytes(p)


def capture(tag, values, t0=0.0, step=0.01):
    """A capture of one tag's series, one report per value."""
    return [(t0 + i * step, report(tag, v, i)) for i, v in enumerate(values)]


def gate_output(samples, capsys):
    st = hc.analyze_pollperiod(samples)["stats"]
    hc._pp_route_a_gate(st)
    return st, capsys.readouterr().out


class TestClassification:
    def test_tag_10_wraps_and_is_not_read_as_saturating(self):
        """Finding 3: tag 10 is MAPLE_FAIL_TOTAL, a plain wrapping fetch_add.

        It was documented and read as cumulative-saturating, which makes a
        turnover look like a reset and a reset look like a huge delta.
        """
        assert 10 in hc.PP_WRAPPING
        assert 10 not in hc.PP_CUMULATIVE
        assert "WRAPPING" in hc.PP_TAGS[10][1]

    def test_a_decrease_on_a_wrapping_tag_is_not_a_reset(self):
        st = hc.analyze_pollperiod(capture(10, [65530, 65535, 3, 9]))["stats"]
        assert st[10]["kind"] == "wrapping"
        assert st[10]["reset"] is False
        assert st[10]["wrapped"] is True
        assert st[10]["delta"] == (9 - 65530) & 0xFFFF == 15

    def test_the_saturating_tags_are_classified_as_such(self):
        for tag in (5, 7, 8, 9, 12, 13):
            st = hc.analyze_pollperiod(capture(tag, [10, 20, 30]))["stats"]
            assert st[tag]["kind"] == "cumulative", tag
            assert st[tag]["delta"] == 20


class TestReset:
    def test_a_reset_suppresses_the_population(self):
        """The reproduced case: 20,000 -> 100 must not become 45,636."""
        st = hc.analyze_pollperiod(capture(7, [20000, 100]))["stats"]
        assert st[7]["reset"] is True
        assert st[7]["delta"] is None
        assert st[7]["rate"] is None
        assert hc.pp_unavailable(st, 7) is not None

    def test_a_reset_mid_capture_is_caught_though_the_ends_look_monotonic(self):
        """Why the scan is over the whole series, not first-versus-last."""
        st = hc.analyze_pollperiod(capture(7, [100, 20000, 50, 300]))["stats"]
        assert st[7]["drops"] == 1
        assert st[7]["reset"] is True
        assert st[7]["delta"] is None

    def test_the_gate_prints_no_figure_across_a_reset(self, capsys):
        st, out = gate_output(capture(7, [20000, 100]), capsys)
        assert "COUNTER RESET" in out
        assert "unavailable" in out
        assert "45636" not in out.replace(",", "")
        assert "No gate verdict" in out

    def test_a_reset_in_a_failure_tag_does_not_read_as_zero_failures(self, capsys):
        """The dangerous shape: polls fine, failures reset. 0 % is not the answer.

        The failure tag's own reset invalidates the run, so the exposure
        figure it would have been a percentage of is withheld too — see
        TestResetInvalidatesTheRun for why that is the whole run, not one tag.
        """
        samples = capture(7, [1000, 13000]) + capture(8, [900, 5], t0=0.5)
        st, out = gate_output(samples, capsys)
        assert st[8]["reset"] is True
        assert "polls in this capture: unavailable" in out
        assert "No gate verdict" in out
        assert "0.00 %" not in out


class TestSaturation:
    def test_a_pegged_counter_is_a_floor_not_a_count(self, capsys):
        samples = capture(7, [60000, 65535]) + capture(8, [0, 0], t0=0.5)
        st, out = gate_output(samples, capsys)
        assert st[7]["pegged"] is True
        assert "65535" in out or "ceiling" in out
        # The v297 failure: a derived exact figure printed under the warning.
        # A floor is printed as a floor. What must never appear is the
        # derived exact figure the v297 run printed under this warning.
        assert ">= 0 of 5535" in out
        assert "— 0.00 %" not in out
        assert "a floor (the counter saturated)" in out

    def test_a_pegged_counter_cannot_meet_the_gate(self, capsys):
        _, out = gate_output(capture(7, [0, 65535]), capsys)
        assert "cannot be judged against the gate" in out
        assert "meets the gate" not in out

    def test_saturation_is_not_claimed_for_a_wrapping_tag(self):
        st = hc.analyze_pollperiod(capture(10, [65535, 65535]))["stats"]
        assert st[10]["pegged"] is False


class TestHealthyRun:
    def test_a_clean_capture_still_reads_normally(self, capsys):
        samples = (capture(7, [1000, 13000])
                   + capture(8, [100, 220], t0=0.5)
                   + capture(12, [900, 11900], t0=1.0)
                   + capture(13, [10, 22], t0=1.5))
        st, out = gate_output(samples, capsys)
        assert st[7]["delta"] == 12000
        assert "meets the gate's >= 10,000" in out
        assert "pooled no-trigger    120 of 12000 — 1.00 %" in out
        assert "no-trigger 12 of 11000 polls — 0.109 %" in out
        assert "COUNTER RESET" not in out
        assert "ceiling" not in out

    def test_first_last_and_timestamps_are_kept_for_the_record(self):
        st = hc.analyze_pollperiod(capture(7, [5, 9, 30], t0=2.0, step=0.5))["stats"]
        assert st[7]["first"] == 5 and st[7]["last"] == 30
        assert st[7]["first_t"] == pytest.approx(2.0)
        assert st[7]["last_t"] == pytest.approx(3.0)


class TestResetInvalidatesTheRun:
    """A reset is a property of the unit, not of the one counter that showed it.

    A reboot can leave a visible decrease in a densely
    sampled failure tag while the sparsely sampled exposure tags stay
    monotonic and a zero-valued tag shows nothing at all. Suppressing only the
    tag that dropped then announces the reset and prints a passing gate
    underneath it — the same shape as the saturation bug this channel already
    had.
    """

    @staticmethod
    def reset_hidden_in_one_tag():
        # Tags 7/12 sampled at the ends only, so their two reports straddle the
        # reboot without ever decreasing; tag 13 is zero throughout; tag 8 is
        # sampled densely enough to catch the drop.
        return (capture(7, [1000, 24000])
                + capture(12, [500, 23500], t0=0.5)
                + capture(13, [0, 0], t0=1.0)
                + capture(8, [900, 5, 40], t0=1.5))

    def test_a_reset_in_any_tag_suppresses_every_cumulative_figure(self):
        st = hc.analyze_pollperiod(self.reset_hidden_in_one_tag())["stats"]
        assert st[8]["reset"] is True
        # These never decreased, but they span the same reboot.
        assert st[7]["reset"] is False
        assert st[12]["reset"] is False
        for tag in (7, 8, 12, 13):
            assert hc.pp_unavailable(st, tag) is not None, tag

    def test_a_reset_in_any_tag_blocks_the_gate_verdict(self, capsys):
        _, out = gate_output(self.reset_hidden_in_one_tag(), capsys)
        assert "COUNTER RESET" in out
        assert "meets the gate" not in out
        assert "0.000 %" not in out
        assert "0.00 %" not in out
        assert "No gate verdict" in out

    def test_a_reset_also_invalidates_the_wrapping_tags(self):
        """A reboot zeroes MAPLE_FAIL_TOTAL too; its span is just as broken."""
        samples = self.reset_hidden_in_one_tag() + capture(10, [7, 9], t0=2.0)
        st = hc.analyze_pollperiod(samples)["stats"]
        assert hc.pp_unavailable(st, 10) is not None


class TestWrapAccumulation:
    """Endpoint-only modulo subtraction throws away whole revolutions.

    100 -> 65000 -> 100 differences to zero at the ends
    while the reports in between show 65,536 increments.
    """

    def test_a_full_revolution_is_accumulated_not_discarded(self):
        st = hc.analyze_pollperiod(capture(10, [100, 65000, 100]))["stats"]
        assert st[10]["delta"] == 65536
        assert st[10]["wrapped"] is True

    def test_accumulation_matches_endpoints_when_nothing_wraps(self):
        st = hc.analyze_pollperiod(capture(10, [100, 400, 900]))["stats"]
        assert st[10]["delta"] == 800
        assert st[10]["wrapped"] is False

    def test_a_wrapped_count_is_not_printed_as_an_exact_percentage(self, capsys):
        samples = capture(7, [1000, 25000]) + capture(10, [100, 65000, 100], t0=0.5)
        _, out = gate_output(samples, capsys)
        assert "0.00 %" not in out
        assert "a floor (it turned over during this capture)" in out
        # The qualification travels with the figure, not after it.
        assert out.index("floor") < out.index("no rate from this run")


class TestPersistence:
    """The JSONL must be able to reconstruct its own delta.

    `last` came from the window-keyed dict, whose insertion
    order survives an 8-bit window ID rolling over, so it was paired with the
    chronological final timestamp.
    """

    def test_last_is_the_chronological_last_across_a_window_rollover(self):
        # 257 windows: the 257th reuses window ID 0 and lands back at the front
        # of the dict, so insertion order ends on window 255's value.
        st = hc.analyze_pollperiod(capture(7, list(range(257))))["stats"]
        assert st[7]["first"] == 0
        assert st[7]["last"] == 256
        assert st[7]["last"] - st[7]["first"] == st[7]["delta"] == 256

    def test_the_persisted_pair_reconstructs_the_delta(self):
        st = hc.analyze_pollperiod(capture(7, list(range(257))))["stats"]
        assert st[7]["last_t"] > st[7]["first_t"]
        assert st[7]["delta"] == st[7]["last"] - st[7]["first"]
