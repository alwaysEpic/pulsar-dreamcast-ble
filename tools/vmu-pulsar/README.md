# VMU pulsar animation

The rotating pulsar on the VMU LCD is rendered in Blender and baked to a 1-bit
frame table, `src/vmu/pulsar_frames.rs`. Nothing here runs at build time; the
generated file is committed.

```bash
cd tools/vmu-pulsar
/Applications/Blender.app/Contents/MacOS/Blender -b -P render_pulsar.py -- "$PWD/renders" 12
uv run --with pillow --with numpy python bake_frames.py renders ../../src/vmu/pulsar_frames.rs renders/preview.gif
```

`renders/` is ignored. `preview.gif` plays the frames at the firmware's rate
(~300 ms, `VMU_ANIM_INTERVAL` polls) in VMU LCD colours.

- **Frame count** is `ROTATION_FRAMES` in `src/vmu.rs`. It must match the render,
  and the firmware rate is fixed, so more frames means a slower spin.
- **Geometry** (tilt, beam width, fade) lives in `render_pulsar.py`.
- **Composition** (dither, star disc, sky, flash) lives in `bake_frames.py`.
