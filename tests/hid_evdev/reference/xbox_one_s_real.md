# Ground-truth: real Xbox One S (Model 1708) Bluetooth descriptors

This is the reference the Generic/Xbox descriptors are measured against (plan Story 4).
The "shape" facts below are encoded as assertions in `test_descriptor_layout.py`.

## Firmware eras / PIDs (VID 0x045E)

| PID      | Firmware era            | Notes |
|----------|-------------------------|-------|
| `0x02E0` | original Xbox One S     | "Windows wireless" identity; what BlueRetro & retro stacks expect |
| `0x02FD` | original BLE firmware   | pre-update BLE PID |
| `0x0B20` | post-2021 FW (≥5.11.x)  | new BLE identity; falls under `hid-generic`. Our Generic/Dreamcast profile uses this recognized Xbox-family identity while serving our contiguous descriptor |

Source: SDL #3075, xpadneo #314, kernel `hid-microsoft`.

## Real raw report-1 layout (both 0x02E0 and 0x0B20 eras)

The real controller's RAW descriptor uses:
- Left stick:  **GD.X / GD.Y**          (16-bit)
- Right stick: **GD.Z / GD.Rz**          (16-bit)   ← note: Z/Rz, *not* Rx/Ry
- Triggers:    **Sim.Brake / Sim.Accelerator** (Simulation page 0x02, 10-bit)
- Hat, Buttons, Consumer AC button.

The kernel `hid-microsoft` / `xpadneo` drivers then **remap** this raw layout to the
canonical `Documentation/input/gamepad.rst` evdev layout (right stick → RX/RY,
triggers → RZ/Z). So "Z/Rz triggers, Rx/Ry right stick" is the *remapped output*,
not what a real controller advertises on the wire.

## Where our two profiles sit
- **Xbox** (`0x02E0`) matches the real RAW layout (Z/Rz right stick, Sim Brake/Accel
  triggers). ✅ faithful — `test_xbox_profile_matches_real_xbox_raw_layout`.
- **Generic/Dreamcast** ships the *already-remapped* layout (Rx/Ry right stick,
  GD.Z/Rz triggers) under the recognized `0x045E/0x0B20` identity —
  `test_generic_profile_diverges_from_real_xbox_raw_layout`. The neutral
  `0x1209/0xDC01` experiment avoided Xbox-family identity, but macOS/browser
  Gamepad API did not surface it; we keep `0x0B20` for host recognition and guard
  the intended layout with tests.

## Partial real 0x0B20 dump (provenance: xpadneo issue #314)

Captured under `hid-generic` at
`/sys/.../0005:045E:0B20.*/report_descriptor`. **Partial** (truncated in the issue):

```
05 01 09 05 a1 01 85 01 09 01 a1 00 09 30 09 31 15 00 27 ff ff 00 00 95 02
75 10 81 02 c0 09 01 a1 00 09 32 09 35 15 00 27 ff ff 00 00 95 02 75 10 81 02
c0 05 02 09 c5 15 00 26 ff 03 95 01 75 0a 81 02 15 00 25 00 75 06 95 01 81 03
...
```
Decoded so far: left stick X/Y, right stick **Z/Rz**, trigger **Sim.Brake** (10-bit).

## SDL / Flycast / Windows ground truth (HIDAPI hardcodes 0x0B20 — issue #7)

`SDL_hidapi_xboxone.c` decodes the Xbox BLE input report **by VID/PID, ignoring the
HID descriptor**. For `0x045E/0x0B20` (Report ID 0x01, ≥16 B) it expects this fixed
map (`data[]` includes the report-ID byte; our payload byte = `data[N] - 1`):

| `data[]` | our payload | field |
|----------|-------------|-------|
| 1-2  | 0-1   | Left stick X (u16, center 0x8000) |
| 3-4  | 2-3   | Left stick Y |
| 5-6  | 4-5   | Right stick X |
| 7-8  | 6-7   | Right stick Y |
| 9-10 | 8-9   | Left trigger (16-bit LE, 10-bit effective) |
| 11-12| 10-11 | Right trigger |
| 13   | 12    | Hat (1-8) |
| 14   | 13    | Face buttons **(gappy)**: A=bit0, B=1, X=**3**, Y=**4**, LB=**6**, RB=**7** |
| 15   | 14    | System: Back=bit2, Start=3, Guide=4, L3=5, R3=6 |

Sticks / triggers / hat offsets **match our `to_bytes`** — but the **button bits do
not**. SDL's gappy byte-14/15 layout is exactly what our **`to_bytes_ms`** (Xbox
profile) produces; the Generic **`to_bytes`** packs buttons *contiguously*
(A=0,B=1,X=2,Y=3,LB=4,RB=5,Back=6,Start=7), so SDL reads them shifted — which decodes
the issue #7 report against ground truth:

| we send (contiguous) | SDL reads (gappy) | result |
|----------------------|-------------------|--------|
| bit2 = X    | unused | **X dead** |
| bit3 = Y    | X      | **Y → X** |
| bit4 = LB   | Y      | LB → Y |
| bit6 = Back | LB     | Back → LB |
| bit7 = Start| RB     | **Start → RB (R1)** |
| byte14 bit0/1 = L3/R3 | unused | **L3/R3 dead** |

### The macOS-vs-SDL split (why no single Generic config wins)
- **macOS GameController is descriptor-driven** — it honors our *contiguous* descriptor,
  so `to_bytes` maps correctly there (confirmed on-device).
- **SDL / Flycast / Windows HIDAPI is PID-hardcoded** — ignores the descriptor and
  expects the *gappy* `0x0B20` layout, so the same payload mis-maps.

You can't satisfy both under one PID + one serializer (contiguous → macOS ✓ / SDL ✗;
gappy → SDL ✓ / macOS ✗ because the descriptor says contiguous). The clean answer is
the **two-profile split we already have**: Xbox profile (gappy) for
SDL/Steam/Flycast/Windows; Generic profile (contiguous) for macOS browser /
descriptor-honoring hosts — matching what the issue #7 reporter found (their STD/Xbox
mode mapped correctly in Flycast). So #7's *button* half is largely a **guidance** fix,
not a descriptor change. The open piece is **triggers on the "ARC" host** — SDL reads
LT/RT at payload bytes 8-9 / 10-11 (same as us, and the `*64 - 32768` scaling spans the
full axis), so that's host-specific and needs the real capture / per-host check, not a
button realignment.

Source: `SDL/src/joystick/hidapi/SDL_hidapi_xboxone.c` (Bluetooth state packet, fw ≥4.8 path).

## TODO for an *exact* byte-level diff
The dump above is partial. To assert the Generic profile aligns byte-for-byte (vs only at the
semantic layout level), capture a **complete** descriptor from a physical unit:

```
# on a Linux box with the real controller paired over BT:
hid-recorder            # pick the 045E:0B20 device, copy the descriptor line
# or:
xxd -c20 -g1 /sys/module/hid_generic/drivers/hid:hid-generic/0005:045E:0B20.*/report_descriptor
```
Drop the full hex here as `xbox_one_s_0b20.hex` and extend the diff test.
