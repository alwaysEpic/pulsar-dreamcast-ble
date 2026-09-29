#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["bleak>=0.22", "pillow>=10"]
# ///
"""Push a VMU LCD frame to a Pulsar over BLE — the reference sender for the
host-integration service.

    uv run scripts/lcd_push.py --scan                     # find the controller
    uv run scripts/lcd_push.py --connected --pattern corner   # macOS, paired
    uv run scripts/lcd_push.py --address <addr> --pattern corner
    uv run scripts/lcd_push.py --address <addr> --png art.png --shape chunks
    uv run scripts/lcd_push.py --address <addr> --pattern bars --fps 10 --seconds 10
    uv run scripts/lcd_push.py --pattern corner --hex     # no radio; dump the bytes

This exists for the public spec page and for anyone writing a sender: it is a
known-good reference to compare a new sender's bytes and behaviour against.

The wire contract, in full:

  service         7EDF0001-3536-4A03-82D0-8AB9122016C6
  characteristic  7EDF0002-3536-4A03-82D0-8AB9122016C6, write without response

  Payload, one of two shapes, told apart by length:

    192 bytes  a whole 48x32 1-bpp frame. Needs an ATT MTU of at least 195.
    49 bytes   a row offset (0, 8, 16 or 24) then that quarter's 48 bytes.
               Four of them, offset 0 first, make a frame.

  Frame bytes are row-major, 6 bytes per row, MSB = leftmost pixel, a 1 bit =
  a lit (dark) LCD pixel — the same layout the Dreamcast puts on the wire in a
  BLOCK_WRITE to function 0x04, and exactly what Dreamwave pushes.

  The frame is UNROTATED. The VMU is mounted upside-down in the controller and
  Pulsar rotates 180 degrees itself; a sender that pre-rotates gets an upside-
  down screen.

Caveats worth knowing before blaming the firmware:

  - The characteristic is encrypted (JustWorks), so the controller must already
    be paired with this machine. An unpaired write fails with an insufficient-
    authentication error, not silence.
  - **On macOS, pair through System Settings first, and then use --connected.**
    Two things bite otherwise, and both look like firmware faults:
      * bleak cannot pair -- CoreBluetooth exposes no pairing API. On an
        unbonded link macOS reads an encrypted HID characteristic, gets ATT
        error 15 (insufficient encryption) and tears the whole session down
        mid-discovery. It looks like the controller hung up on you.
      * a sync hold clears the controller's bond but not the Mac's, after
        which macOS refuses to connect at all with CBError 14, "Peer removed
        pairing information". Forget the device and pair again.
    Once paired the controller is *connected* to the system and has stopped
    advertising, so --scan/--address cannot find it. --connected asks
    CoreBluetooth for it directly instead.
  - Writes are unacknowledged by design. A frame can be dropped in the radio,
    or on the Maple bus by the VMU's CRC, and nothing reports it. Send a
    recognisable pattern and look at the screen.
"""
from __future__ import annotations

import argparse
import asyncio
import sys
import time

LCD_SERVICE = "7edf0001-3536-4a03-82d0-8ab9122016c6"
LCD_FRAME = "7edf0002-3536-4a03-82d0-8ab9122016c6"

WIDTH = 48
HEIGHT = 32
ROW_BYTES = WIDTH // 8
FRAME_BYTES = WIDTH * HEIGHT // 8

# One chunk of the small-MTU shape: 8 rows, plus the leading row-offset byte.
CHUNK_ROWS = 8
CHUNK_BYTES = CHUNK_ROWS * ROW_BYTES
# An ATT write command spends three bytes on the opcode and the handle.
ATT_WRITE_OVERHEAD = 3


# --- frames ---------------------------------------------------------------


def blank() -> bytearray:
    return bytearray(FRAME_BYTES)


def set_pixel(frame: bytearray, x: int, y: int) -> None:
    if 0 <= x < WIDTH and 0 <= y < HEIGHT:
        frame[y * ROW_BYTES + x // 8] |= 0x80 >> (x % 8)


def pattern(name: str, phase: int = 0) -> bytearray:
    """A test frame. `phase` shifts it horizontally so a stream visibly moves —
    a still image proves the first write landed and nothing after it."""
    frame = blank()
    if name == "corner":
        # Deliberately asymmetric: a border plus a filled top-left block. If
        # rotation is wrong anywhere in the chain the block is bottom-right,
        # which no symmetric pattern would show.
        for x in range(WIDTH):
            set_pixel(frame, x, 0)
            set_pixel(frame, x, HEIGHT - 1)
        for y in range(HEIGHT):
            set_pixel(frame, 0, y)
            set_pixel(frame, WIDTH - 1, y)
        for y in range(2, 10):
            for x in range(2, 10):
                set_pixel(frame, x, y)
    elif name == "border":
        for x in range(WIDTH):
            set_pixel(frame, x, 0)
            set_pixel(frame, x, HEIGHT - 1)
        for y in range(HEIGHT):
            set_pixel(frame, 0, y)
            set_pixel(frame, WIDTH - 1, y)
        for i in range(min(WIDTH, HEIGHT)):
            set_pixel(frame, i, i)
    elif name == "checker":
        for y in range(HEIGHT):
            for x in range(WIDTH):
                if ((x + phase) // 4 + y // 4) % 2 == 0:
                    set_pixel(frame, x, y)
    elif name == "bars":
        for y in range(HEIGHT):
            for x in range(WIDTH):
                if ((x + phase) // 3) % 2 == 0:
                    set_pixel(frame, x, y)
    elif name == "sweep":
        # A single moving column: the cheapest way to read the delivered frame
        # rate off the screen by eye.
        for y in range(HEIGHT):
            set_pixel(frame, phase % WIDTH, y)
            set_pixel(frame, (phase + 1) % WIDTH, y)
    else:  # pragma: no cover - argparse constrains this
        raise SystemExit(f"unknown pattern {name}")
    return frame


def from_png(path: str) -> bytearray:
    from PIL import Image

    img = Image.open(path).convert("L")
    if img.size != (WIDTH, HEIGHT):
        raise SystemExit(f"{path} is {img.size[0]}x{img.size[1]}, need {WIDTH}x{HEIGHT}")
    frame = blank()
    for y in range(HEIGHT):
        for x in range(WIDTH):
            # Dark pixel = lit LCD segment, so a black-on-white drawing looks
            # the same on the VMU as it does in an image viewer.
            if img.getpixel((x, y)) < 128:
                set_pixel(frame, x, y)
    return frame


def render(frame: bytes) -> str:
    rows = []
    for y in range(HEIGHT):
        row = "".join(
            "#" if frame[y * ROW_BYTES + x // 8] & (0x80 >> (x % 8)) else "."
            for x in range(WIDTH)
        )
        rows.append(row)
    return "\n".join(rows)


def chunks(frame: bytes) -> list[bytes]:
    """The four-write shape: row offset, then that quarter's bytes."""
    out = []
    for i in range(FRAME_BYTES // CHUNK_BYTES):
        at = i * CHUNK_BYTES
        out.append(bytes([i * CHUNK_ROWS]) + frame[at : at + CHUNK_BYTES])
    return out


# --- radio ----------------------------------------------------------------


async def scan(seconds: float) -> None:
    from bleak import BleakScanner

    print(f"scanning {seconds:.0f}s ...")
    devices = await BleakScanner.discover(timeout=seconds)
    if not devices:
        print("nothing found")
        return
    for d in sorted(devices, key=lambda d: (d.name or "").lower()):
        print(f"  {d.address}  {d.name or '(no name)'}")
    print(
        "\nPulsar advertises as a gamepad under its profile name (Xbox Wireless\n"
        "Controller, or Pulsar in the Generic profile). The frame service is not\n"
        "in the advert — connect and it will be in the service list."
    )


def _looks_like_pulsar(name: str | None) -> bool:
    """Name test for the fallback path only, never for the frame-service hit."""
    return any(
        n in (name or "").lower()
        for n in ("xbox wireless controller", "dreamcast wireless controller", "pulsar")
    )


async def connected_device():
    """The controller as macOS already has it: paired, connected, not advertising.

    bleak has no public API for this. CoreBluetooth does --
    retrieveConnectedPeripheralsWithServices -- and a BLEDevice carrying the
    (peripheral, delegate) pair is what the CoreBluetooth backend expects, so
    the result drops straight into BleakClient.
    """
    try:
        from bleak.backends.corebluetooth.CentralManagerDelegate import (
            CentralManagerDelegate,
        )
        from bleak.backends.device import BLEDevice
        from CoreBluetooth import CBUUID
    except ImportError:
        print("--connected is macOS only (needs CoreBluetooth)", file=sys.stderr)
        return None

    cm = CentralManagerDelegate()
    await cm.wait_until_ready()

    # Ask for the frame service alone. retrieveConnectedPeripherals returns
    # everything carrying *any* of the UUIDs given, so including the generic
    # HID/Device-Information/Battery UUIDs would make a connected mouse or
    # keyboard a candidate -- and the first result is not sorted in our favour.
    found = list(
        cm.central_manager.retrieveConnectedPeripheralsWithServices_(
            [CBUUID.UUIDWithString_(LCD_SERVICE)]
        )
        or []
    )

    if not found:
        # The vendor service is only known to CoreBluetooth once it has
        # discovered it on this host, so a freshly paired controller can be
        # connected and still not match. Fall back to the gamepad UUID, but
        # filter by name instead of taking whatever comes first.
        candidates = list(
            cm.central_manager.retrieveConnectedPeripheralsWithServices_(
                [CBUUID.UUIDWithString_("1812")]
            )
            or []
        )
        found = [p for p in candidates if _looks_like_pulsar(p.name())]
        if candidates and not found:
            names = ", ".join(repr(p.name()) for p in candidates)
            print(
                f"connected HID devices, none of them a Pulsar: {names}",
                file=sys.stderr,
            )

    if not found:
        print(
            "no connected controller. Pair it in System Settings > Bluetooth and\n"
            "make sure it is connected, then try again.",
            file=sys.stderr,
        )
        return None

    if len(found) > 1:
        print("more than one candidate; pass --address to choose:", file=sys.stderr)
        for p in found:
            print(f"  {p.identifier().UUIDString()}  {p.name()}", file=sys.stderr)
        return None

    per = found[0]
    print(f"using connected {per.identifier().UUIDString()}  {per.name()}")
    return BLEDevice(per.identifier().UUIDString(), per.name(), (per, cm))


async def push(args: argparse.Namespace, frames: list[bytes]) -> int:
    from bleak import BleakClient

    target = args.address
    if args.connected:
        target = await connected_device()
        if target is None:
            return 2

    async with BleakClient(target, timeout=args.timeout) as client:
        char = client.services.get_characteristic(LCD_FRAME)
        if char is None:
            print(
                f"{target} has no {LCD_FRAME} characteristic — this is not a\n"
                "Pulsar running firmware 0.6.0 or later.",
                file=sys.stderr,
            )
            return 2

        # bleak reports the negotiated ATT MTU on every backend that can know
        # it. The choice is the same one the dongle makes at discovery: whole
        # frames when 195 fits, four row writes otherwise.
        mtu = getattr(client, "mtu_size", 0) or 0
        shape = args.shape
        if shape == "auto":
            shape = "whole" if mtu >= FRAME_BYTES + ATT_WRITE_OVERHEAD else "chunks"
            print(f"att mtu {mtu} -> {shape}")
        elif shape == "whole" and mtu and mtu < FRAME_BYTES + ATT_WRITE_OVERHEAD:
            print(
                f"warning: att mtu {mtu} cannot carry a 192-byte write; the stack\n"
                "         will truncate or reject it. --shape chunks is the fallback.",
                file=sys.stderr,
            )

        period = 1.0 / args.fps if args.fps else 0.0
        sent = 0
        started = time.monotonic()
        for frame in frames:
            due = started + sent * period
            now = time.monotonic()
            if now < due:
                await asyncio.sleep(due - now)
            if shape == "whole":
                await client.write_gatt_char(char, frame, response=False)
            else:
                for part in chunks(frame):
                    await client.write_gatt_char(char, part, response=False)
            sent += 1
        elapsed = time.monotonic() - started
        rate = sent / elapsed if elapsed > 0 else 0.0
        print(f"sent {sent} frame(s) in {elapsed:.2f}s ({rate:.1f} fps offered)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Push VMU LCD frames to a Pulsar (host service)."
    )
    ap.add_argument("--scan", action="store_true", help="list nearby BLE devices and exit")
    ap.add_argument("--address", help="BLE address (or macOS UUID) of the controller")
    ap.add_argument(
        "--connected",
        action="store_true",
        help="macOS: use the controller already paired and connected to this Mac, "
        "which --scan cannot see because it has stopped advertising",
    )
    ap.add_argument(
        "--pattern",
        default="corner",
        choices=["corner", "border", "checker", "bars", "sweep"],
        help="built-in test frame (default: corner)",
    )
    ap.add_argument("--png", help="48x32 image to send instead of a built-in pattern")
    ap.add_argument(
        "--shape",
        default="auto",
        choices=["auto", "whole", "chunks"],
        help="one 192-byte write, four 49-byte writes, or pick by negotiated MTU",
    )
    ap.add_argument("--fps", type=float, default=0.0, help="stream at this rate")
    ap.add_argument("--seconds", type=float, default=0.0, help="stream for this long")
    ap.add_argument("--timeout", type=float, default=20.0, help="connect timeout")
    ap.add_argument("--show", action="store_true", help="print the frame as ASCII art")
    ap.add_argument("--hex", action="store_true", help="print the frame as hex and exit")
    args = ap.parse_args()

    if args.scan:
        asyncio.run(scan(5.0))
        return 0

    still = from_png(args.png) if args.png else pattern(args.pattern)
    if args.show or args.hex:
        if args.show:
            print(render(still))
        if args.hex:
            for i in range(0, FRAME_BYTES, ROW_BYTES):
                print(" ".join(f"{b:02X}" for b in still[i : i + ROW_BYTES]))
            return 0
        # --show alongside a target prints what is about to go out, which is
        # worth keeping. On its own there is nothing to send to, and it is
        # documented as needing no radio, so it must not fall through to the
        # "--address is required" error after having already printed the frame.
        if not args.address and not args.connected:
            return 0

    if not args.address and not args.connected:
        ap.error(
            "--address is required (find it with --scan), or --connected on a macOS "
            "host where the controller is already paired -- unless --hex"
        )

    if args.fps and args.seconds:
        count = max(1, round(args.fps * args.seconds))
        # A PNG has nothing to animate, so a stream of it is a repeat send —
        # still the right test for "does the link hold at N fps".
        frames = [
            bytes(still if args.png else pattern(args.pattern, phase=i))
            for i in range(count)
        ]
    else:
        frames = [bytes(still)]

    return asyncio.run(push(args, frames))


if __name__ == "__main__":
    sys.exit(main())
