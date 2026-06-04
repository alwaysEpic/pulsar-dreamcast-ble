# BlueRetro mapping harness

Software regression test for how a **BlueRetro** adapter maps our controller —
no nRF hardware required. It exists to catch the class of bug in
[issue #2](https://github.com/alwaysEpic/pulsar-dreamcast-ble/issues/2)
(`START`/`X`/`Y` landing on the wrong Dreamcast functions) *before* flashing.

## How it works

BlueRetro ships a QEMU + pytest harness. Their firmware is built with a `qemu`
config and run under `qemu-system-xtensa -machine esp32`; a websocket
(`injector.py`) lets a test hand the firmware a HID report descriptor + report
bytes and read back every pipeline stage (`wireless_input` → `generic_input` →
`mapped_input` → `wired_output`).

`pytest_pulsar_dreamcast.py`:

1. advertises the name our `PROFILE_STD` uses (`Xbox Wireless Controller`),
2. sends BlueRetro the exact `HID_REPORT_DESCRIPTOR_STD` our firmware serves,
3. feeds the exact 16-byte reports `GamepadReport::to_bytes_ms` produces for each
   physical button,
4. asserts each one decodes to the same BlueRetro-generic button a genuine Xbox
   One S BLE controller produces (BlueRetro's own `device_data/xbox.py`).

If our descriptor doesn't parse the way the real Xbox descriptor does, the
buttons mis-map here exactly as they do on a real Dreamcast.

## Fixtures are generated, not hand-written

`fixtures.json` (descriptor + per-button report bytes) is emitted from the Rust
source by `maple-protocol/tests/blueretro_fixtures.rs`. That test runs in
`scripts/ci.sh`, so editing the descriptor or serializer without regenerating
the fixtures fails CI. Regenerate after an intentional change:

```sh
(cd maple-protocol && UPDATE_FIXTURES=1 cargo test --test blueretro_fixtures)
```

## Running it

In CI it runs via `.github/workflows/blueretro-pytest.yml`. Locally, use
BlueRetro's container (heavy; nested xtensa emulation is slow on Apple Silicon):

```sh
git clone --recursive https://github.com/darthcloud/BlueRetro
cd BlueRetro && git checkout e1a9831a875f5313a923160a1379a7ebbfaa2b11

docker run --rm -it -v "$PWD:/br" -v "/path/to/pulsar:/pulsar" \
  ghcr.io/darthcloud/idf-blueretro:v5.5.0_2024-12-02_gcovr bash -c '
    cd /br && . "$IDF_PATH/export.sh" && git config --global --add safe.directory "*"
    echo "harness qemu" > version.txt && cp configs/dbg/qemu sdkconfig && idf.py build
    cp /pulsar/tests/blueretro/pytest_pulsar_dreamcast.py /pulsar/tests/blueretro/fixtures.json tests/
    (cd build && esptool.py --chip esp32 merge_bin --fill-flash-size 4MB -o flash_image.bin @flash_args)
    qemu-system-xtensa -machine esp32 -drive file=build/flash_image.bin,if=mtd,format=raw \
      -serial file:serial_log.txt -serial file:gcov_data.gcfn -display none \
      -nic user,model=open_eth,id=lo0,hostfwd=tcp:127.0.0.1:8001-:80 -daemonize
    pip install --quiet websocket-client pytest numpy
    pytest tests/pytest_pulsar_dreamcast.py'
```

## Tests

- `test_pulsar_std_buttons_mapping` — every face/shoulder/system button (the issue).
- `test_pulsar_std_dpad_mapping` — hat switch → D-pad directions (`hat_to_ld_btns`).
- `test_pulsar_std_axes_scaling` — sticks (unsigned 0-65535, center 32768) and
  triggers (0-1023); guards the signed-`Logical Maximum` stick regression and
  confirms our Rx/Ry + Z/Rz usages land on the expected axes.
- `test_pulsar_ext_buttons_probe` — `xfail`-tolerant characterization of the EXT
  profile (contiguous layout, Steam/PC target) on BlueRetro. XFAIL = mis-maps as
  expected; XPASS = it happens to work, revisit.

All STD assertions are at the `generic_input` stage (descriptor-parse correctness,
system-independent). A red STD test means issue #2 is reproduced — the harness
working as intended; it goes green once the descriptor is fixed.

## Scope / TODO

- BlueRetro is pinned by commit in the workflow; bump deliberately and re-verify
  against the matching container tag.
- The EXT profile's real target (Steam Deck / kernel / xpadneo) is a *different*
  parser, better covered by a `hid-tools`/`uhid` virtual-device layer (future).
- No `dc.py` exists upstream, so DC *wired output* isn't modelled — we assert at
  `generic_input`, which is where mapping correctness is decided.
