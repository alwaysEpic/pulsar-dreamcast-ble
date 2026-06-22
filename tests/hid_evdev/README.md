# HID descriptor → mapping tests

Proves our two BLE gamepad profiles (Xbox `0x02E0`, Generic/Dreamcast `0x0B20`)
keep a stable, correct HID layout and map the way real hosts expect. Complements the existing
Rust byte-golden (`maple-protocol/tests/blueretro_fixtures.rs`) and the BlueRetro
QEMU harness (`tests/blueretro/`).

Both layers consume the **same** Rust-generated `tests/blueretro/fixtures.json`
(descriptors + per-button report bytes) — one source of truth.

## Two layers

| File | Proves | Runs on |
|------|--------|---------|
| `test_descriptor_layout.py` | layout doesn't regress; the Xbox profile matches the real Xbox raw descriptor; the Generic profile's divergence is documented (issue #7) | **any OS** (pure-Python `hid-tools` parser) |
| `test_evdev_mapping.py` | the real Linux kernel maps each profile to the right buttons/axes, and LT/RT are independent axes | **Linux only** (UHID + libevdev) |

`reference/xbox_one_s_real.md` documents the ground-truth real-controller layout
the descriptors are measured against.

## Run locally

```bash
cd tests/hid_evdev
uv venv .venv && uv pip install --python .venv/bin/python -r requirements.txt
.venv/bin/python -m pytest test_descriptor_layout.py -v
```

The evdev layer needs Linux (`/dev/uhid` + libevdev); on macOS/Windows it skips.
On Linux:

```bash
uv pip install --python .venv/bin/python -r requirements-linux.txt
sudo modprobe uhid
sudo .venv/bin/python -m pytest test_evdev_mapping.py -v
```

CI runs both (`.github/workflows/hid-evdev.yml`).

## Key finding (issue #7)

`test_xbox_profile_matches_real_xbox_raw_layout` /
`test_generic_profile_diverges_from_real_xbox_raw_layout`: the **real** Xbox One S
puts the right stick on `Z`/`Rz` and the triggers on the Simulation page
(`Brake`/`Accelerator`). The **Xbox** profile matches this; the **Generic** profile
ships the *xpadneo-remapped* layout (right stick `Rx`/`Ry`, triggers `Z`/`Rz`).
The Generic profile serves this layout under the **recognized Xbox One S 1708 BLE
identity** (Microsoft 0x045E / PID **0x0B20**, post-FW-5.11). An earlier neutral
pid.codes PID `0x1209/0xDC01` was tried to sidestep Xbox-family identity, but it
regressed the browser Gamepad API on macOS: Apple's GameController framework
seized the device but did not surface it to browsers. The current profile keeps
`0x0B20` for host recognition and relies on these tests to guard the intended
contiguous descriptor/layout. See plan Story 6 for the remaining alignment
question.
