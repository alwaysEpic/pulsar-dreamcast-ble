# Images

Photos of the Dreamcast BLE adapter build.

## Assembly Photos

| File | Description |
|------|-------------|
| `vmu_front.jpeg` | Enclosure front — controller cable exits bottom |
| `vmu_back.jpeg` | Enclosure back — screw holes visible |
| `vmu_opened.jpeg` | Enclosure opened — perfboard, Pololu boost, pull-up resistors |
| `vmu_opened2.jpeg` | Enclosure opened — alternate angle showing battery and wiring |
| `xiao_board_orientation.jpeg` | XIAO board seated in enclosure front — USB-C and sync button visible |
| `vmu_in_controller.jpeg` | Adapter installed in Dreamcast controller VMU slot — front view |
| `vmu_in_controller2.jpeg` | Adapter installed — side angle showing Dreamcast logo |
| `back_fit.jpeg` | Controller back shell — VMU enclosure fit with wiring routed |
| `vmu_slot_holes.jpeg` | VMU slot closeup — arrow showing cable routing |
| `controller_modification.jpeg` | Controller shell modification — arrows showing trimmed plastic for cable clearance |
| `larger_wiring_example.jpeg` | Perfboard wiring — XIAO, boost converter, battery, controller cable |
| `breadboard_wiring.jpeg` | DK development breadboard — pull-ups and controller cable connections |

## Install Photos

`install/` — the photo-by-photo sequence for
[fitting a Pulsar v1 into a controller](../build/pulsar-v1.md). Same exports the site's
install page uses.

| File | Description |
|------|-------------|
| `controller-back-screws.webp` | The six screw positions on the controller's back shell |
| `controller-open-cable-connected.webp` | Controller open, original cable still connected |
| `dreamcast-cable-removed.webp` | Original cable and strain relief lifted out |
| `second-accessory-slot.webp` | The empty second accessory slot |
| `pulsar-installed-overview.webp` | Pulsar seated in slot 2, cable routed to the right-hand opening |
| `pulsar-cable-connected.webp` | Pulsar's plug seated in the controller connector — plug orientation |
| `back-cover-cable-guard-before.webp` | Inside of the back shell, cable guard still present |
| `back-cover-cable-guard-removed.webp` | The same shell after a clean break |
| `cable-cover-and-routing.webp` | Printed cable cover fitted around the Pulsar cable |

## Wiring Diagrams

See [`../wiring/`](../wiring/) for the Fritzing diagram and breadboard export.

## Metadata

Every image here is published. **Before adding one, strip its metadata and verify:**

```bash
python3 scripts/image_metadata.py strip docs/images   # lossless — drops APP segments only
python3 scripts/image_metadata.py scan  docs          # exits 1 if any GPS remains
```

Stripping is lossless — JPEG scan data is untouched and only whole APP segments are
dropped, so there is no requantisation. A non-default `Orientation` is preserved; drop it
and the affected images rotate.

**Verify by parsing the file, not by asking an indexer.** On macOS, `mdls -name
kMDItemLatitude` reads the Spotlight index rather than the image and returns `(null)` for
anything Spotlight has not indexed — it will happily report a GPS-laden photo as clean.
The script above parses the containers directly (JPEG APP segments, PNG `eXIf`, WebP
`EXIF`/`XMP `) and exits non-zero when a GPS IFD is present.
