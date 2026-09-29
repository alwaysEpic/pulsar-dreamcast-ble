#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["bleak>=0.22"]
# ///
"""Read blocks off a Pulsar's docked VMU over BLE — the reference reader for
the host-integration service (protocol v1).

    uv run scripts/vmu_read.py --connected                 # macOS, paired: root + FAT
    uv run scripts/vmu_read.py --connected --blocks 255,254,253
    uv run scripts/vmu_read.py --connected --pull          # the dongle's working set
    uv run scripts/vmu_read.py --connected --blocks 255 --hexdump
    uv run scripts/vmu_read.py --address <addr> --blocks 255

This is the sibling of `lcd_push.py` and shares its connection handling, which
is where all the macOS pain lives: **pair in System Settings first and then use
`--connected`** — a paired controller has stopped advertising, so `--scan` and
`--address` cannot find it. `lcd_push.py --scan` still lists one that is not
yet paired.

The wire contract, in full:

  service  7EDF0001-3536-4A03-82D0-8AB9122016C6

  down     7EDF0004-…, write without response
    01 READ     [01, blk]                  one outstanding at a time
    03 STATUS?  [03]

  up       7EDF0003-…, notify
    81 DATA     [81, blk, phase, result] + 128 B    four per READ, phase 0..3
                [81, blk, 0, result != 0]           one message, no data
    83 STATUS   [83, proto=1, flags, queue_free, epoch u32 LE, card u32 LE]

  flags: bit0 vmu_present, bit1 enabled, bit2 draining, bit3 idle

**Block bytes are image order** — the order the card's filesystem is laid out
in, which is the wire's words byte-reversed. The Pulsar converts once, on the
way out of its decoder, and this script checks the result by parsing the root
block's fields at the offsets they only live at in image order. That check is
the point of the run; a card that reports FAT 254 / directory 253 / 13
directory blocks / 200 user blocks is a card whose bytes arrived the right way
round.

It also recomputes `card` — FNV-1a 32 over block 255 then block 254 — and
compares it with the fingerprint STATUS reports, which is the same contract
checked from the other end.

Before blaming the firmware:

  - **Reads are served only while the pad is idle**: no stick or button
    movement for a second. Put the controller down before starting. STATUS's
    `idle` bit says what the firmware thinks, and a read that never answers
    while `idle` is 0 is the rule working, not a fault.
  - A read needs a VMU **docked and enumerated**. `vmu_present` says so.
  - This script only reads. Writes (`02 WRITE` → `82 ACK`, then `84 WRITTEN`) are
    served by the firmware and specified in `docs/host_integration.md`; there is
    no public reference writer yet.
"""
from __future__ import annotations

import argparse
import asyncio
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lcd_push import connected_device  # noqa: E402

VMU_UP = "7edf0003-3536-4a03-82d0-8ab9122016c6"
VMU_DOWN = "7edf0004-3536-4a03-82d0-8ab9122016c6"

OP_READ = 0x01
OP_STATUS_Q = 0x03
OP_DATA = 0x81
OP_STATUS = 0x83

BLOCK_BYTES = 512
PHASE_BYTES = 128
PHASES = BLOCK_BYTES // PHASE_BYTES

RESULT = {
    0: "OK",
    1: "FULL",
    2: "NO_VMU",
    3: "DISABLED",
    4: "FAILED",
    5: "DISCARDED",
    6: "SUPERSEDED",
    7: "BAD",
}

# Root block (255) fields, 16-bit little-endian, image order only.
ROOT_FAT_AT = 0x46
ROOT_DIR_AT = 0x4A
ROOT_DIR_BLOCKS_AT = 0x4C
ROOT_USER_BLOCKS_AT = 0x50

# A FAT entry for a block nothing has claimed.
FAT_FREE = 0xFFFC

FNV_OFFSET_BASIS = 0x811C9DC5
FNV_PRIME = 0x01000193


def fnv1a32(seed: int, data: bytes) -> int:
    h = seed
    for b in data:
        h ^= b
        h = (h * FNV_PRIME) & 0xFFFFFFFF
    return h


def u16(block: bytes, at: int) -> int:
    return int.from_bytes(block[at : at + 2], "little")


def describe_flags(f: int) -> str:
    names = [(1, "vmu_present"), (2, "enabled"), (4, "draining"), (8, "idle")]
    on = [n for bit, n in names if f & bit]
    return ",".join(on) if on else "none"


class Status:
    __slots__ = ("proto", "flags", "queue_free", "epoch", "card")

    def __init__(self, msg: bytes) -> None:
        self.proto = msg[1]
        self.flags = msg[2]
        self.queue_free = msg[3]
        self.epoch = int.from_bytes(msg[4:8], "little")
        self.card = int.from_bytes(msg[8:12], "little")

    def __str__(self) -> str:
        return (
            f"proto {self.proto}  flags {self.flags:#04x} ({describe_flags(self.flags)})"
            f"  queue_free {self.queue_free}  epoch {self.epoch}"
            f"  card {self.card:#010x}"
        )


class LinkLost(Exception):
    """The connection went away mid-run.

    Worth its own type because it is not a read failure and must not be
    retried: on a dropped link every subsequent call raises out of bleak's
    service cache instead, and the run has nothing left to say except how far
    it got. A Pulsar that is idle long enough sleeps (10 min, `main.rs`
    `INACTIVITY_TIMEOUT_MS`), and a pull is idle time by construction.
    """

    def __init__(self, block: int, cause: BaseException) -> None:
        super().__init__(f"link lost at block {block}: {cause}")
        self.block = block


class ReadFailed(Exception):
    def __init__(self, block: int, code: int) -> None:
        super().__init__(f"block {block}: {RESULT.get(code, code)}")
        self.block = block
        self.code = code


class Reader:
    """One outstanding READ at a time, as protocol v1 requires.

    The rule is the dongle's to keep, not the Pulsar's to police: a second READ
    sent before the fourth DATA is dropped by the firmware and counted, so a
    sender that pipelines simply hangs. This class is the shape that does not.
    """

    def __init__(self, client) -> None:
        self.client = client
        self.status: Status | None = None
        self.status_event = asyncio.Event()
        self._reset_block()

    def _reset_block(self) -> None:
        self.want: int | None = None
        self.parts: dict[int, bytes] = {}
        self.error: int | None = None
        self.done = asyncio.Event()

    def on_notify(self, _handle, data: bytearray) -> None:
        msg = bytes(data)
        if not msg:
            return
        if msg[0] == OP_STATUS and len(msg) >= 12:
            fresh = Status(msg)
            # A generation change is the interesting unprompted STATUS: it is
            # how a dock or an undock reaches the host, and it is what the
            # removal check on the bench is looking for. Printing it as it
            # lands means the operator sees the cause beside the effect.
            if self.status is not None and fresh.epoch != self.status.epoch:
                print(
                    f"  status: generation {self.status.epoch} -> {fresh.epoch}"
                    f"  ({describe_flags(fresh.flags)})  card {fresh.card:#010x}"
                )
            self.status = fresh
            self.status_event.set()
            return
        if msg[0] != OP_DATA or len(msg) < 4:
            return
        block, phase, result = msg[1], msg[2], msg[3]
        if self.want is None or block != self.want:
            # An answer to a request that is no longer ours — a stale reply
            # after a timeout. Ignoring it is right; counting it is useful.
            print(f"  note: DATA for block {block}, expecting {self.want}")
            return
        if result != 0:
            self.error = result
            self.done.set()
            return
        if len(msg) != 4 + PHASE_BYTES:
            print(f"  note: DATA phase {phase} is {len(msg)} bytes, expected {4 + PHASE_BYTES}")
            return
        self.parts[phase] = msg[4:]
        if len(self.parts) == PHASES:
            self.done.set()

    async def ask_status(self, timeout: float) -> Status:
        self.status_event.clear()
        await self.client.write_gatt_char(VMU_DOWN, bytes([OP_STATUS_Q]), response=False)
        await asyncio.wait_for(self.status_event.wait(), timeout)
        assert self.status is not None
        return self.status

    async def read_block(self, block: int, timeout: float) -> tuple[bytes, float]:
        self._reset_block()
        self.want = block
        started = time.perf_counter()
        await self.client.write_gatt_char(
            VMU_DOWN, bytes([OP_READ, block]), response=False
        )
        await asyncio.wait_for(self.done.wait(), timeout)
        elapsed = time.perf_counter() - started
        if self.error is not None:
            raise ReadFailed(block, self.error)
        data = b"".join(self.parts[p] for p in range(PHASES))
        return data, elapsed


def check_root(block: bytes) -> tuple[bool, str]:
    """The byte-order check, and the card's geometry.

    These four fields parse only in image order; in wire order each is half of
    a different field. A stock 128 KiB card answers 254 / 253 / 13 / 200.
    """
    fat = u16(block, ROOT_FAT_AT)
    directory = u16(block, ROOT_DIR_AT)
    dir_blocks = u16(block, ROOT_DIR_BLOCKS_AT)
    user = u16(block, ROOT_USER_BLOCKS_AT)
    sane = (
        fat < 256
        and directory < 256
        and 1 <= dir_blocks <= 32
        and dir_blocks <= directory + 1
        and 1 <= user <= 256
    )
    text = (
        f"FAT {fat}  directory {directory}  directory blocks {dir_blocks}  "
        f"user blocks {user}"
    )
    if block[:16] == b"\x55" * 16:
        text += "  [formatted]"
    return sane, text


def used_blocks(fat: bytes, user: int) -> list[int]:
    """User-area blocks the FAT says are claimed, in the dongle's pull order."""
    return [i for i in range(min(user, 256)) if u16(fat, 2 * i) != FAT_FREE]


def one_card(fingerprinted: Status, final: Status) -> bool:
    """Did a pull's blocks all come from the card its root and FAT came from?

    A dock or undock moves the generation, so the epoch taken with the
    fingerprint has to survive to the last block. The card is compared too,
    though a swap that kept the epoch would be a firmware bug.
    """
    return fingerprinted.epoch == final.epoch and fingerprinted.card == final.card


def hexdump(data: bytes, limit: int) -> str:
    out = []
    for at in range(0, min(len(data), limit), 16):
        row = data[at : at + 16]
        hexed = " ".join(f"{b:02X}" for b in row)
        text = "".join(chr(b) if 32 <= b < 127 else "." for b in row)
        out.append(f"  {at:04X}  {hexed:<47}  {text}")
    return "\n".join(out)


async def run(args: argparse.Namespace) -> int:
    from bleak import BleakClient
    from bleak.exc import BleakError

    target = args.address
    if args.connected:
        target = await connected_device()
        if target is None:
            return 2

    async with BleakClient(target, timeout=args.timeout) as client:
        if client.services.get_characteristic(VMU_DOWN) is None:
            print(
                f"{target} has no {VMU_DOWN} characteristic — this is not a\n"
                "Pulsar running firmware 0.6.0 or later.",
                file=sys.stderr,
            )
            return 2

        reader = Reader(client)
        await client.start_notify(VMU_UP, reader.on_notify)

        try:
            status = await reader.ask_status(args.timeout)
        except asyncio.TimeoutError:
            print("no STATUS within the timeout", file=sys.stderr)
            return 2
        print(f"status: {status}")
        if not status.flags & 0x01:
            print("no VMU docked — dock one and re-run", file=sys.stderr)
            return 2
        if not status.flags & 0x08:
            print("pad is not idle yet: put the controller down (reads need 1 s of stillness)")

        if args.watch:
            # The idle rule, watched rather than inferred. Reads are refused
            # while the pad is in use, so a run that times out every block and a
            # run against a pad someone is holding look identical from outside
            # — this is what tells them apart, and it is the instrument for the
            # stick-movement check too.
            print(f"watching STATUS for {args.watch:.0f} s (Ctrl-C to stop)")
            until = time.perf_counter() + args.watch
            last = None
            while time.perf_counter() < until:
                try:
                    st = await reader.ask_status(args.timeout)
                except asyncio.TimeoutError:
                    print("  no STATUS within the timeout")
                    break
                now_flags = describe_flags(st.flags)
                mark = "" if now_flags == last else "   <-- changed"
                print(f"  {time.strftime('%H:%M:%S')}  flags {st.flags:#04x} ({now_flags}){mark}")
                last = now_flags
                await asyncio.sleep(1.0)
            await client.stop_notify(VMU_UP)
            return 0

        wanted = [255, 254] if args.pull else args.blocks
        blocks: dict[int, bytes] = {}
        took_ms: list[float] = []
        failures = 0
        started = time.perf_counter()

        async def fetch(n: int) -> bytes | None:
            nonlocal failures
            for attempt in range(1, args.retries + 1):
                try:
                    data, took = await reader.read_block(n, args.read_timeout)
                except asyncio.TimeoutError:
                    print(f"block {n:3}: timed out (attempt {attempt})")
                    continue
                except ReadFailed as e:
                    print(f"block {n:3}: {RESULT.get(e.code, e.code)} (attempt {attempt})")
                    continue
                except BleakError as e:
                    raise LinkLost(n, e) from e
                took_ms.append(took * 1000)
                print(f"block {n:3}: {len(data)} bytes in {took * 1000:6.1f} ms")
                return data
            failures += 1
            return None

        async def fetch_all(numbers) -> bool:
            """Read each block. False once the link is gone — not an exception,
            because everything already pulled is still worth reporting."""
            for n in numbers:
                try:
                    data = await fetch(n)
                except LinkLost as lost:
                    print(f"\nLINK LOST at block {lost.block}, after {len(blocks)} blocks.")
                    print(
                        "  If the pad was untouched throughout, the likely cause is the\n"
                        "  inactivity timeout: reads are idle-only, so a pull is ten minutes\n"
                        "  of 'no input' by construction and the unit powers off mid-pull.\n"
                        "  Wake it with a button and check whether it is advertising again."
                    )
                    return False
                if data is not None:
                    blocks[n] = data
            return True

        live = await fetch_all(wanted)

        # The contract check, and the geometry the rest of a pull needs.
        root = blocks.get(255)
        if root is not None:
            sane, text = check_root(root)
            print(f"root:   {text}")
            if not sane:
                print(
                    "root block does not parse in image order — the block-byte\n"
                    "contract is broken somewhere between the decoder and here.",
                    file=sys.stderr,
                )
                return 1

        fat = blocks.get(254)
        if live and root is not None and fat is not None:
            # Against a *fresh* STATUS, not the one this run opened with: the
            # firmware takes `card` as the pull passes, and the protocol says
            # it reads 0 until both blocks have been read. Comparing with the
            # opening STATUS reports a mismatch on every correct first pull.
            try:
                now = await reader.ask_status(args.timeout)
            except asyncio.TimeoutError:
                print("no STATUS after the fingerprint blocks", file=sys.stderr)
                return 2
            if now.epoch != status.epoch:
                print(
                    f"generation changed during the read ({status.epoch} -> {now.epoch}):\n"
                    "the card was docked or undocked, so these two blocks may not be\n"
                    "from the same one. Re-run without touching the VMU.",
                    file=sys.stderr,
                )
                return 1
            card = fnv1a32(fnv1a32(FNV_OFFSET_BASIS, root), fat)
            agree = "matches" if card == now.card else "DIFFERS FROM"
            print(f"card:   {card:#010x} {agree} the firmware's {now.card:#010x}")
            if card != now.card:
                print(
                    "the fingerprint disagrees: the two ends are hashing different\n"
                    "bytes, which is the block-byte contract failing quietly.",
                    file=sys.stderr,
                )
                return 1

        if live and args.pull and root is not None and fat is not None:
            user = u16(root, ROOT_USER_BLOCKS_AT)
            directory = u16(root, ROOT_DIR_AT)
            dir_blocks = u16(root, ROOT_DIR_BLOCKS_AT)
            rest = [directory - i for i in range(dir_blocks)]
            rest += used_blocks(fat, user)
            print(f"pull:   {len(rest)} more blocks (directory {dir_blocks}, used {len(rest) - dir_blocks})")
            live = await fetch_all(rest)
            # The fingerprint check above only covers root and FAT. A card
            # swapped after it would hand over the rest of the pull, and the
            # backup would mix two cards and still report success.
            if live:
                try:
                    final = await reader.ask_status(args.timeout)
                except asyncio.TimeoutError:
                    print("no STATUS after the pull; not writing it", file=sys.stderr)
                    return 2
                if not one_card(now, final):
                    print(
                        f"generation changed during the pull ({now.epoch} -> {final.epoch}):\n"
                        "the blocks may come from more than one card; not writing them.\n"
                        "Re-run without touching the VMU.",
                        file=sys.stderr,
                    )
                    return 1

        elapsed = time.perf_counter() - started
        print(
            f"done:   {len(blocks)} blocks in {elapsed:.2f} s"
            f"  ({failures} failed)"
        )
        # The cadence, which is the number that matters here: one
        # outstanding read against a read window every `READ_EVERY_N` polls, not
        # the diagnostic's prefilled queue.
        if took_ms:
            ordered = sorted(took_ms)
            p50 = ordered[len(ordered) // 2]
            print(
                f"cadence: {p50:.0f} ms median, {min(ordered):.0f}-{max(ordered):.0f} ms"
                f"  over {len(ordered)} blocks"
            )

        if args.hexdump:
            for n, data in blocks.items():
                print(f"block {n}:")
                print(hexdump(data, args.hexdump_bytes))

        if args.out:
            out = Path(args.out)
            if out.is_dir():
                for n, data in blocks.items():
                    (out / f"block_{n:03}.bin").write_bytes(data)
            else:
                # One file, image order, blocks in the order they arrived.
                # Replaced, not appended to: appending stacked a second run
                # onto the first.
                out.write_bytes(b"".join(blocks.values()))
            print(f"wrote:  {len(blocks)} blocks to {out}")

        if live:
            await client.stop_notify(VMU_UP)
        return 1 if (failures or not live) else 0


def parse_blocks(text: str) -> list[int]:
    out = []
    for part in text.split(","):
        part = part.strip()
        if not part:
            continue
        n = int(part, 0)
        if not 0 <= n <= 255:
            raise argparse.ArgumentTypeError(f"block {n} out of range 0-255")
        out.append(n)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Read VMU blocks off a Pulsar",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    ap.add_argument("--address", help="BLE address, if the controller is unpaired")
    ap.add_argument(
        "--connected",
        action="store_true",
        help="use the controller macOS already has paired and connected",
    )
    ap.add_argument(
        "--blocks",
        type=parse_blocks,
        default=[255, 254],
        help="blocks to read (default: 255,254 — root and FAT)",
    )
    ap.add_argument(
        "--pull",
        action="store_true",
        help="read the dongle's working set: root, FAT, directory, then used blocks",
    )
    ap.add_argument("--retries", type=int, default=3, help="attempts per block (default 3)")
    ap.add_argument(
        "--read-timeout",
        type=float,
        default=5.0,
        help="seconds to wait for one block's four DATA phases (default 5)",
    )
    ap.add_argument("--timeout", type=float, default=15.0, help="connection timeout")
    ap.add_argument(
        "--watch",
        type=float,
        default=0.0,
        help="poll STATUS for N seconds and print the flags — the idle rule, watched",
    )
    ap.add_argument("--hexdump", action="store_true", help="dump each block")
    ap.add_argument(
        "--hexdump-bytes", type=int, default=128, help="bytes per block to dump (default 128)"
    )
    ap.add_argument("--out", help="write blocks to this directory (or replace this file with them)")
    args = ap.parse_args()

    if not args.address and not args.connected:
        print(
            "--address is required, or --connected on a macOS machine the\n"
            "controller is already paired to.",
            file=sys.stderr,
        )
        return 2

    try:
        return asyncio.run(run(args))
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
