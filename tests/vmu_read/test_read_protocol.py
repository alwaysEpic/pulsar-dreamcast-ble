# SPDX-License-Identifier: GPL-3.0-or-later
"""The bench reader's half of protocol v1, and the block-byte contract it checks.

Two things are pinned here. The first is the contract itself: a root block in
image order parses, the same block in wire order does not, and the two
fingerprint differently — the same fixture the firmware's `block_bytes` tests
hold.

The second is the reader's state machine, which is where this script can go
wrong quietly. A `Status` is a snapshot, not a view: the one a run opens with
says `card == 0` by definition, because the firmware takes the fingerprint as
the pull passes. Comparing against that snapshot reported a mismatch on every
correct first pull, and
`a_status_snapshot_does_not_follow_later_notifications` is that bug.
"""
import asyncio

import pytest
import vmu_read as vr

# The first 88 bytes of a formatted card's root block, image order — the same
# literal table as maple-protocol's `block_bytes` tests.
ROOT_HEAD = bytes(
    [0x55] * 16
    + [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00]
    + [0x00] * 8
    + [0x00] * 16
    + [0x19, 0x99, 0x09, 0x09, 0x00, 0x00, 0x10, 0x00]
    + [0x00] * 8
    + [0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00, 0xFE, 0x00]
    + [0x01, 0x00, 0xFD, 0x00, 0x0D, 0x00, 0x00, 0x00]
    + [0xC8, 0x00, 0x1F, 0x00, 0x00, 0x00, 0x80, 0x00]
)


def block(head=ROOT_HEAD):
    return head + bytes(vr.BLOCK_BYTES - len(head))


def to_wire(image):
    """Each word's bytes reversed — what the card actually puts on the bus."""
    return bytes(image[(k & ~3) | (3 - (k & 3))] for k in range(len(image)))


def status_msg(*, flags=0x0B, queue_free=0, epoch=1, card=0):
    return bytes([vr.OP_STATUS, 1, flags, queue_free]) + epoch.to_bytes(
        4, "little"
    ) + card.to_bytes(4, "little")


def data_msg(blk, phase, payload):
    return bytes([vr.OP_DATA, blk, phase, 0]) + payload


class FakeClient:
    """A Pulsar that answers writes synchronously, inside the await."""

    def __init__(self):
        self.reader = None
        self.writes = []
        self.answer = lambda msg: []

    async def write_gatt_char(self, _uuid, data, response=False):
        self.writes.append(bytes(data))
        for msg in self.answer(bytes(data)):
            self.reader.on_notify(0, bytearray(msg))


def wired():
    client = FakeClient()
    reader = vr.Reader(client)
    client.reader = reader
    return client, reader


# --------------------------------------------------------- the contract --


def test_a_root_block_parses_in_image_order():
    ok, text = vr.check_root(block())
    assert ok
    assert "FAT 254" in text
    assert "directory 253" in text
    assert "directory blocks 13" in text
    assert "user blocks 200" in text


def test_the_same_block_in_wire_order_is_refused():
    ok, text = vr.check_root(to_wire(block()))
    assert not ok
    # Not merely "different": the fields are nonsense, which is what makes the
    # check worth running on the bench.
    assert "FAT 65280" in text


def test_the_fingerprint_matches_the_published_vectors():
    assert vr.fnv1a32(vr.FNV_OFFSET_BASIS, b"") == vr.FNV_OFFSET_BASIS
    assert vr.fnv1a32(vr.FNV_OFFSET_BASIS, b"a") == 0xE40C292C
    assert vr.fnv1a32(vr.FNV_OFFSET_BASIS, b"foobar") == 0xBF9CF968


def test_seeding_continues_the_hash_so_card_can_be_taken_in_two_calls():
    split = vr.fnv1a32(vr.fnv1a32(vr.FNV_OFFSET_BASIS, b"foo"), b"bar")
    assert split == vr.fnv1a32(vr.FNV_OFFSET_BASIS, b"foobar")


def test_the_two_byte_orders_fingerprint_differently():
    image = block()
    assert vr.fnv1a32(vr.FNV_OFFSET_BASIS, image) != vr.fnv1a32(
        vr.FNV_OFFSET_BASIS, to_wire(image)
    )


def test_used_blocks_reads_the_fat_in_the_pull_order():
    fat = bytearray(b"\xfc\xff" * 256)
    fat[0:2] = (0x0005).to_bytes(2, "little")
    fat[10:12] = (0xFFFA).to_bytes(2, "little")
    assert vr.used_blocks(bytes(fat), 200) == [0, 5]


def test_a_pull_is_one_card_while_the_generation_holds():
    taken = vr.Status(status_msg(epoch=4, card=0xAAAA))
    assert vr.one_card(taken, vr.Status(status_msg(epoch=4, card=0xAAAA)))


def test_a_swap_after_the_fingerprint_fails_the_pull():
    """The review's reproduction: root and FAT from card A, the rest from B.

    The generation moves on the swap even if B hashes the same as A, so the
    epoch alone has to fail it.
    """
    taken = vr.Status(status_msg(epoch=4, card=0xAAAA))
    assert not vr.one_card(taken, vr.Status(status_msg(epoch=6, card=0xAAAA)))
    assert not vr.one_card(taken, vr.Status(status_msg(epoch=6, card=0xBBBB)))


# ------------------------------------------------------ the read machine --


def test_four_phases_reassemble_into_the_block_they_were_cut_from():
    client, reader = wired()
    image = block()
    client.answer = lambda msg: [
        data_msg(255, p, image[p * vr.PHASE_BYTES : (p + 1) * vr.PHASE_BYTES])
        for p in range(vr.PHASES)
    ]
    data, _took = asyncio.run(reader.read_block(255, 1.0))
    assert data == image


def test_phases_out_of_order_still_reassemble_in_order():
    client, reader = wired()
    image = block()
    client.answer = lambda msg: [
        data_msg(255, p, image[p * vr.PHASE_BYTES : (p + 1) * vr.PHASE_BYTES])
        for p in (2, 0, 3, 1)
    ]
    data, _took = asyncio.run(reader.read_block(255, 1.0))
    assert data == image


def test_an_error_result_ends_the_read_with_that_code():
    client, reader = wired()
    client.answer = lambda msg: [bytes([vr.OP_DATA, 7, 0, 4])]
    with pytest.raises(vr.ReadFailed) as caught:
        asyncio.run(reader.read_block(7, 1.0))
    assert caught.value.code == 4


def test_data_for_another_block_is_ignored_and_the_read_times_out():
    client, reader = wired()
    client.answer = lambda msg: [data_msg(9, 0, bytes(vr.PHASE_BYTES))]
    with pytest.raises(asyncio.TimeoutError):
        asyncio.run(reader.read_block(255, 0.05))


def test_a_short_phase_is_not_stored():
    client, reader = wired()
    client.answer = lambda msg: [data_msg(255, 0, bytes(vr.PHASE_BYTES - 1))]
    with pytest.raises(asyncio.TimeoutError):
        asyncio.run(reader.read_block(255, 0.05))


# ------------------------------------------------------------- the STATUS --


def test_status_carries_every_field_of_the_wire_shape():
    client, reader = wired()
    client.answer = lambda msg: [status_msg(flags=0x0B, epoch=7, card=0xDEADBEEF)]
    s = asyncio.run(reader.ask_status(1.0))
    assert (s.proto, s.flags, s.epoch, s.card) == (1, 0x0B, 7, 0xDEADBEEF)
    assert "vmu_present" in vr.describe_flags(s.flags)
    assert "idle" in vr.describe_flags(s.flags)


def test_a_status_snapshot_does_not_follow_later_notifications():
    """The bug: `card` is 0 until both fingerprint blocks have been read.

    A run that keeps the STATUS it opened with compares a real fingerprint
    against that 0 and reports the contract broken on a perfectly good pull.
    """
    client, reader = wired()
    client.answer = lambda msg: [status_msg(epoch=3, card=0)]
    opening = asyncio.run(reader.ask_status(1.0))
    assert opening.card == 0

    # The firmware finishes the fingerprint and says so, unprompted.
    reader.on_notify(0, bytearray(status_msg(epoch=3, card=0x1234_5678)))
    assert opening.card == 0, "a snapshot must not mutate under the caller"
    assert reader.status.card == 0x1234_5678

    # Asking again is what the run does, rather than trusting that the
    # unprompted one arrived: `03 STATUS?` is always answered, so this is the
    # deterministic way to read the fingerprint the firmware now holds.
    client.answer = lambda msg: [status_msg(epoch=3, card=0x1234_5678)]
    fresh = asyncio.run(reader.ask_status(1.0))
    assert fresh.card == 0x1234_5678
    assert fresh.epoch == opening.epoch, "same generation, so comparable"


def test_ask_status_waits_for_an_answer_rather_than_reusing_the_last_one():
    """Why the run asks instead of reading `reader.status`.

    A STATUS held from before is a fact about before. `ask_status` clears its
    event first, so what it returns was notified after the question.
    """
    client, reader = wired()
    client.answer = lambda msg: [status_msg(epoch=1, card=0xAAAA)]
    assert asyncio.run(reader.ask_status(1.0)).card == 0xAAAA

    client.answer = lambda msg: []
    with pytest.raises(asyncio.TimeoutError):
        asyncio.run(reader.ask_status(0.05))


def test_an_unprompted_generation_change_is_reported_to_the_operator(capsys):
    """The removal check on the bench reads this line.

    A dock or an undock reaches the host only as a STATUS with a new epoch. It
    arrives unprompted, in the middle of a pull, so the run has to say so where
    the operator can see it beside the block that was lost.
    """
    _client, reader = wired()
    reader.on_notify(0, bytearray(status_msg(epoch=4, card=0xAAAA)))
    capsys.readouterr()  # the first STATUS is not a change

    reader.on_notify(0, bytearray(status_msg(epoch=5, flags=0x02, card=0)))
    printed = capsys.readouterr().out
    assert "generation 4 -> 5" in printed
    assert "card 0x00000000" in printed
    assert "vmu_present" not in printed, "the card is out; say so"


def test_a_discarded_reply_ends_the_read_rather_than_hanging():
    """What an undock mid-reply looks like from here: one terminal DATA.

    The firmware owes a terminal answer for whatever the dongle was waiting on
    — so a pull interrupted by a removal fails that block
    and moves on instead of waiting out the timeout.
    """
    client, reader = wired()
    client.answer = lambda msg: [
        data_msg(255, 0, bytes(vr.PHASE_BYTES)),
        bytes([vr.OP_DATA, 255, 0, 5]),  # DISCARDED, mid-reply
    ]
    with pytest.raises(vr.ReadFailed) as caught:
        asyncio.run(reader.read_block(255, 1.0))
    assert caught.value.code == 5
    assert vr.RESULT[caught.value.code] == "DISCARDED"
