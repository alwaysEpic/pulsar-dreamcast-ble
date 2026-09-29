# Host Integration Service

A vendor GATT service that lets a host — a console-side dongle, an emulator,
or a script on a laptop — drive the VMU docked in a Pulsar controller: push
frames to its LCD, and read and write the physical memory card's 512-byte
blocks. It first shipped in firmware **0.6.0**; no earlier release carries it.

This is an *open* vendor characteristic set: anything malformed that arrives
on it is dropped in silence rather than erroring the connection. It exists so
a third party can write a sender without our hardware in the room.

## How to tell it is a Pulsar

Look for the service UUID below in the GATT table after connecting — it is
not advertised, so it will not appear in a scan result, only after connect
and discovery. It is present identically under **both** gamepad personalities
(the Xbox identity and the generic one): they share one GATT table, and the
personality only changes the GAP name, the PnP IDs and the HID report
descriptor. The service, not the identity, is what marks a Pulsar. The third
personality, `Pulsar Configure` (browser remapping), is a different GATT
table and does not carry this service.

## Service and characteristics

| Name | UUID | Properties | Security | Max length |
|---|---|---|---|---|
| `HostService` | `7EDF0001-3536-4A03-82D0-8AB9122016C6` | (primary service) | — | — |
| LCD frame | `7EDF0002-3536-4A03-82D0-8AB9122016C6` | Write Without Response | JustWorks | 192 B |
| VMU storage, up | `7EDF0003-3536-4A03-82D0-8AB9122016C6` | Notify | JustWorks | 132 B |
| VMU storage, down | `7EDF0004-3536-4A03-82D0-8AB9122016C6` | Write Without Response | JustWorks | 132 B |

Security is `JustWorks` — encrypted, unauthenticated — the same level as the
HID report characteristics. A host already bonded as a gamepad gets this for
free. Both down characteristics are Write Without Response, which by the ATT
spec never answers, so **a write on an unencrypted link is dropped in
silence** — there is no error to catch. **Pair the controller with the host
first.**

Only the bonded host may use the service. While a bond exists, a different
device that tries to pair outside the pairing window (the 2-second sync hold)
is disconnected, and nothing it writes to this service is acted on. Nothing
is notified on a link that is not encrypted.

This keeps out devices that merely happen to be in range. It is not
authentication: pairing is Just Works (there is no code to confirm), and the
bonded host is recognised by its Bluetooth address. A device that deliberately
impersonates that address can still pair.

The firmware requests an ATT MTU of 247 on connection; what a link actually
gets depends on the host's stack (some grant less). Two thresholds matter:

- **LCD:** a whole 192-byte frame needs a negotiated MTU of at least 195 to fit
  one write. Below that, use the four-write chunked shape described below.
  Both shapes are always accepted, told apart purely by the write's length —
  a sender never has to announce which one it is using.
- **VMU storage:** every `DATA` notification and every `WRITE` phase is 132
  bytes, so the storage protocol needs an MTU of **at least 135**. It has no
  chunked alternative; on a smaller MTU, do not use it.

## LCD frame channel

The VMU LCD is 48×32 pixels, 1 bit per pixel, 192 bytes total: 32 rows of 6
bytes each, MSB = leftmost pixel, and a set bit is a lit (dark) segment. This
is the same layout the Dreamcast itself puts on the Maple bus in a
`BLOCK_WRITE` to function `0x04` — the payload is exactly what a real console
would send to the VMU's LCD.

**Send the frame unrotated.** The VMU sits upside-down in the controller, and
Pulsar corrects for that in firmware before the frame reaches the physical
screen. A sender that pre-rotates ends up upside-down; frames arriving from a
Maple `BLOCK_WRITE` capture, or drawn upright, are already correct.

### Wire shapes

| Shape | Length | Contents |
|---|---|---|
| Whole frame | 192 bytes | The full frame, row-major, as above. |
| Chunk (×4) | 49 bytes | `[row_offset, chunk[48]]` — one 8-row band. |

`row_offset` is one of `0, 8, 16, 24` and must land on that boundary; a chunk
whose offset is not a multiple of 8, or out of range, is dropped. Offset `0`
starts a new frame — writing it clears whatever partial assembly was in
progress, so a sender that drops a chunk mid-frame resynchronises cleanly on
its next pass instead of ever producing a frame stitched from two sources.
The frame becomes visible only once all four chunks have arrived. Send the
chunks in order, `0` first: a set sent `8, 16, 24, 0` never completes, since the
final `0` starts over. A whole-frame write also discards any partial chunk set.

A write of any other length is dropped and never reaches the screen.

**The host owns the screen from its first frame until the link drops.** The
adapter's own animation stops while a host is drawing and resumes after the
disconnect. One exception: while acked VMU writes are draining onto the card
(below), the firmware overlays a small disk icon on the top-left corner of
whatever is on screen, the host's frame included, and removes it when the
drain ends.

Frames **replace**, they are never queued: sending faster than the display
can draw simply loses the frames in between, which is the right behaviour for
a screen. There is no acknowledgement on this characteristic either way — a
frame can be dropped by the radio, or by the VMU's own CRC on the Maple bus,
and nothing reports it back. Send something recognisable and look at the
screen.

### Sample frame

A small asymmetric test pattern — a border plus a filled block in one
corner — so a rotation mistake anywhere in the chain is visible as the block
landing in the wrong corner. 32 rows of 6 bytes, whole-frame shape:

```
FF FF FF FF FF FF
80 00 00 00 00 01
BF C0 00 00 00 01
BF C0 00 00 00 01
BF C0 00 00 00 01
BF C0 00 00 00 01
BF C0 00 00 00 01
BF C0 00 00 00 01
BF C0 00 00 00 01
BF C0 00 00 00 01
80 00 00 00 00 01   (repeats through row 30)
FF FF FF FF FF FF
```

### Reference sender

`scripts/lcd_push.py` is a standalone `uv`-run script (`bleak` + `pillow`)
that connects, negotiates the shape from the actual negotiated MTU, and
streams either a built-in pattern or a 48×32 image:

```
uv run scripts/lcd_push.py --scan                        # find the controller
uv run scripts/lcd_push.py --connected --pattern corner   # macOS, already paired
uv run scripts/lcd_push.py --address <addr> --png art.png --shape chunks
uv run scripts/lcd_push.py --pattern corner --hex         # dump the bytes, no radio
```

On macOS specifically: pair through System Settings first, then use
`--connected` — a paired controller stops advertising, so `--scan`/`--address`
cannot see it; `--connected` asks the OS for the already-connected device by
service UUID instead.

## VMU storage protocol (v1)

Read and write access to the 512-byte blocks of
the VMU physically docked in the controller, over the `…0003`/`…0004` pair.
Block numbers are one byte, `0`–`255`; `255` is the card's root block and
`254` its FAT, in the standard VMU filesystem layout.

**Byte order.** Every payload on this protocol — a `81 DATA` reply, and a `02
WRITE` request where that exists — carries a block's bytes in **image
order**: the order the VMU's own filesystem lays them out in, which is *not*
the raw word order the Maple bus carries them in (the wire reverses each
32-bit word). The firmware converts once, on the way in and out; a reader
does not need to know the wire order exists.

### Down: `…0004`, Write Without Response

| Op | Bytes | Meaning |
|---|---|---|
| `0x01` READ | `[01, blk]` | Request one block. One outstanding at a time — see below. |
| `0x03` STATUS? | `[03]` | Ask for a `STATUS` notification on demand. |
| `0x02` WRITE | `[02, blk, phase, seq]` + 128 B | One of four phases of a block write — see [Writing a block](#writing-a-block). |

Lengths are exact: `READ` is 2 bytes, `STATUS?` 1 byte, `WRITE` 132 bytes.
Anything else is dropped in silence.

A `READ` sent while one is already outstanding (including while its `DATA`
phases are still going out) is silently refused — nothing is sent back, not
even an error. The rule is the sender's to keep: wait for the fourth `DATA`
(or a `DATA` carrying a non-zero result) before issuing the next `READ`.

A `READ` accepted while the pad is in use is **held, not refused**: it is
served once the pad goes idle, and if input resumes mid-read the firmware
recalls it and serves it again at the next idle. Only a `READ` with no VMU
docked is answered at once (`DATA … NO_VMU`).

### Up: `…0003`, Notify

| Op | Bytes | When |
|---|---|---|
| `0x81` DATA | `[81, blk, phase, result]` + 128 B, or just the 4-byte header when `result != 0` | Four per successful `READ`, phases `0`–`3`; one message with no data on failure. |
| `0x83` STATUS | `[83, proto=1, flags, queue_free, epoch u32 LE, card u32 LE]` (12 bytes) | On subscribe, on request (`03`), and whenever `flags`, `queue_free`, `epoch` or `card` change while connected. On reconnect, a change to `flags` or `queue_free` made while the link was down is announced, but a moved `epoch` or `card` is not — send `03`. |
| `0x82` ACK | `[82, blk, seq, result]` (4 bytes) | Answers a completed four-phase `WRITE`, or a phase that broke the rules (`BAD`). |
| `0x84` WRITTEN | `[84, blk, seq, result]` (4 bytes) | Once per acked `seq`, when its fate is known. One in flight as the link drops can be lost: see [Sessions and reconnects](#sessions-and-reconnects). |

Messages go out in a fixed priority: `STATUS` first, then `ACK`s (and
`WRITTEN … SUPERSEDED`), then other `WRITTEN`s in drain order, then `DATA`.
`STATUS` leads because it is one message and a host may be blocked on it
right after connecting; `DATA` is last because a block's phases wait on
nothing, and the pad is idle by definition while a read is being served.

### `flags` (byte 2 of `STATUS`)

| Bit | Name | Meaning |
|---|---|---|
| 0 | `vmu_present` | A VMU is docked and enumerated. |
| 1 | `enabled` | Always `1` at this revision. Reserved for a future setting that turns the service off; until one exists it carries no information. |
| 2 | `draining` | One or more acked writes are staged or being written and not yet confirmed on the card. Always `0` on builds without the write path. |
| 3 | `idle` | The pad has gone at least 1 second with no button change and no stick or trigger movement past a small deadzone. Reads are served only while this is set. |

### `result` codes

| Code | Name | Meaning |
|---|---|---|
| `0` | OK | |
| `1` | FULL | The `WRITE` was refused: no free queue slot, or the card's fingerprint is not yet known (read blocks `255` and `254` first). |
| `2` | NO_VMU | No VMU is docked. |
| `3` | DISABLED | Reserved for the same future setting as `enabled`; never produced at this revision. |
| `4` | FAILED | `DATA`: the block read failed every attempt the firmware allows. `WRITTEN`: the write could not be confirmed within the firmware's attempts — the card refused a phase or the commit, or the read-back did not match or could not be read. This also ends the current generation (below). |
| `5` | DISCARDED | The generation ended (below) before this READ or WRITE could be answered normally. |
| `6` | SUPERSEDED | A different `seq` for the same block replaced this one while it was still staged; nothing was written under the superseded `seq`. |
| `7` | BAD | A `WRITE` phase broke the assembly rules ([Writing a block](#writing-a-block)). |

### Reading a block

1. Subscribe to `…0003`. A `STATUS` notification follows immediately.
2. Check `flags`: bit 0 (`vmu_present`) must be set, and reads only progress
   while bit 3 (`idle`) is set — put the pad down and leave it alone.
3. Write `[0x01, blk]` to `…0004`.
4. Collect four `81 DATA` notifications, `phase` `0`–`3`, each `result == 0`
   with 128 bytes of payload; concatenate `phase * 128 .. +128` in order to
   reassemble the 512-byte block. A single `DATA` with `result != 0` and no
   payload ends the request instead — inspect the `result` code.
5. Only after that (or after a non-zero-result `DATA`) may the next `READ` be
   issued.

**Generations.** `epoch` moves whenever the docked card is no longer known to
be the one being served: a dock or an undock, the controller being lost, the
link dropping (see [Sessions and reconnects](#sessions-and-reconnects)), the
controller's power going down (including sleep), a write that `FAILED`, or the
firmware being unable to confirm the card is the same one after a write was
interrupted. Its first value after boot is arbitrary. If a generation ends
while a `READ` is outstanding or its `DATA` phases are still going out,
whatever was owed becomes a single `DATA … DISCARDED` rather than silence, so a
sender is never left waiting for a fourth phase that will not come. Staged
writes become `WRITTEN … DISCARDED` the same way.

**The card fingerprint.** `card` in `STATUS` is an FNV-1a 32-bit hash, seed
`0x811C9DC5`, taken over block `255`'s 512 bytes and then block `254`'s, both
in image order — it reads `0` until both blocks have been read (or
written) at least once in the current generation. A
host that wants to know it is talking about the same card it already knows
should compare this against its own hash of the same two blocks, computed
the same way.

### Writing a block

**Which builds write.** Pulsar v1 and XIAO builds serve both reads and
writes. The DK build has no VMU storage path at all: it reports no VMU
(`flags` reads `0x02`), answers every `READ` with `DATA … NO_VMU`, keeps
`queue_free` at `0`, and answers every completed `WRITE` with `ACK … NO_VMU`.
A sender should honour `STATUS` rather than assume either.

To write a block:

1. Send four `02 WRITE` phases for the same block and the same `seq` (a
   sender-chosen byte, one value per block write), phase `0` first and in
   order. Phase `0` always starts a fresh assembly, discarding any half-sent
   one. Resending a phase already received overwrites it. Answered
   `ACK … BAD` and discarded: a phase `≥ 4`; phase `1`–`3` with no assembly
   open, or for a different block or `seq`; a phase that skips ahead.
2. Once all four phases are in, the block is staged and answered
   `ACK … OK` — or refused: `NO_VMU` if nothing is docked, `FULL` if the
   queue has no room, is not open, or the card's fingerprint is not yet known
   (read blocks `255` and `254` first).
3. A resend of a `(block, seq)` the firmware still holds — a retry after a lost
   `ACK` — is acknowledged again without being staged twice. A `WRITE` for a
   block that is still staged under a different `seq` replaces it in place,
   keeping its position in the drain order; the replaced `seq` receives
   `WRITTEN … SUPERSEDED` immediately, and the new one its own `ACK` and,
   later, its own `WRITTEN`. If the earlier write has already started going to
   the card, the new one is staged separately and drains after it.
4. Staged blocks drain to the physical card in the order they were first
   staged. Each one receives one `WRITTEN` (unless the link drops as it goes
   out; see below): `OK` once the firmware has
   read the block back from the card and it matched, `FAILED` if that could not
   be achieved (which also ends the generation — every other block still
   staged becomes `DISCARDED`), or `DISCARDED` if the generation ends first for
   any other reason.

The queue holds **8** blocks. `queue_free` in `STATUS` is the number of free
slots and is the flow-control signal: never have more acked-but-unwritten
writes in flight than the last `queue_free` allowed. A slot stays occupied
until its `WRITTEN` has been sent.

**Keep a retry timer.** If the firmware's outgoing buffer is full, a completed
`WRITE` can be dropped with no `ACK` at all. Resend the same `(block, seq)`
until it is acked; step 3 makes that safe while the firmware still holds the
block. Once the `ACK` arrives, stop resending: a resend that lands after the
block's `WRITTEN` has gone out is staged again, written a second time (the same
bytes, so the card is unharmed), and answered with a second `ACK` and
`WRITTEN`. A sender should tolerate that pair rather than rely on never
seeing it. The `WRITTEN` is always the answer that counts: an `ACK … OK` for
a resend after a reconnect can be followed by `WRITTEN … DISCARDED`.

### Sessions and reconnects

A dropped link immediately loses what was in flight: an outstanding `READ`, a
half-sent `DATA`, a half-assembled `WRITE` and any undelivered `ACK`s.

Acked writes keep draining onto the card while the link is down, for up to
30 seconds. Once nothing is left to drain (at once, if the queue was empty), or
when the 30 seconds run out, the generation ends: `epoch` moves, `card` reads
`0`, and anything still staged becomes `WRITTEN … DISCARDED`. Every `WRITTEN`
still owed at the drop, whatever its result, is delivered on the next
connection. The exception is one already handed to the radio: a `WRITTEN` in
flight as the link drops can be lost, and it is not sent again.

So on reconnect, read `STATUS` (send `03`, below). An unchanged `epoch` means
the host reconnected while its writes were still draining, and the generation
carried on. A moved `epoch` means read blocks `255` and `254` again before
writing, then resend whatever came back `DISCARDED`, and anything never acked.

Either way, do not wait on `WRITTEN`s for writes acked before the drop. Once
`STATUS` shows `draining` clear, a write that still has no `WRITTEN` has an
unknown fate: read the block back and compare, or resend it. A resend is safe
(step 3 above): the same bytes are written again and answered with a fresh
`WRITTEN`.

`STATUS` is sent when the host *writes* the CCCD to enable notifications. A
host that relies on a subscription restored from the bond may get no `STATUS`
until something changes, so **send `[03]` after every connect**.

## Reference scripts

Both are standalone `uv`-run Python (no project virtualenv needed):

```
uv run scripts/lcd_push.py --connected --pattern corner
uv run scripts/vmu_read.py --connected                      # root + FAT
uv run scripts/vmu_read.py --connected --pull                # the whole used working set
uv run scripts/vmu_read.py --connected --blocks 255 --hexdump
```

`vmu_read.py` shares `lcd_push.py`'s connection handling (including the
macOS `--connected` pairing workaround above), decodes `STATUS`/`DATA`,
verifies the root block parses correctly in image order, and cross-checks its
own computed `card` fingerprint against the firmware's. `--pull` walks the
root block, FAT and directory to fetch those plus every user-area block the
FAT marks as used. Neither script exercises the write path (`02 WRITE`);
there is no public reference writer yet, and the write section above is
specified from the firmware's code and tests.
