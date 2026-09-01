# 3D Models & Enclosure Files

This directory contains 3D models for a VMU-shaped enclosure for the Dreamcast BLE adapter.

## Which shell do I print?

Two different builds live in `Edited_VMU_stls/`, and they are not interchangeable — the
hand-wired XIAO build and the Pulsar v1 carrier are different shapes inside.

| Folder | For | Notes |
|---|---|---|
| `dreamcast_vmu_*.stl` (loose, at the top of `Edited_VMU_stls/`) | **[The hand-wired XIAO build](../docs/build/xiao.md)** | Front plus three back variants — plain, `_cutout`, and `_w_cutout` — differing in the opening for your wiring. Print the front and whichever back suits how you routed the cable |
| `PulsarFit_703035/` | **Pulsar v1 carrier**, 703035 cell | The baseline fit: a 7.0 × 30 × 35 mm, ~800 mAh pouch. Clearance at the dome crown is ~0.5 mm, so it wants that cell. Ships a `PRINT_PLATE.stl` with the parts laid out |
| `PulsarFit_bigcell/` | **Pulsar v1 carrier**, larger cells | Same rear and plunger; the **front** hollows the dome to a constant 1.0 mm wall, raising crown clearance to ~1.5 mm so taller pouches fit. Includes `pulsar_bigcell.3mf` — the slicer project with orientation, supports and settings the STLs alone do not carry |
| `PulsarFit_bigcell_cable_relief/` | **Pulsar v1 carrier**, big cell, cable-corner relief | ⚠️ **Experimental — not yet fit-tested.** Adds a filleted relief cut at the cable corner of *both* halves, for a pinch between the shell corner and the controller's connector housing. Print it only if you hit that pinch. Includes `pulsar_bigcell_cable_relief.3mf` |

> The cable-relief variant was cut in a live modelling session rather than by the build
> scripts, so re-running `blender_files/build_pulsar_vmu*.py` reproduces the shells
> **without** it. Treat those STLs as the source of truth for that variant.

Every Pulsar-fit family shares the same `pulsar_vmu_button_plunger.stl`.

## Contents

- **`blender_files/`** — Blender source files for the modified VMU enclosure (by alwaysEpic)
- **`Edited_VMU_stls/`** — Modified STL exports ready for printing (by alwaysEpic); see the
  table above for which folder matches your build
- **`VMU_ShellAssembly.step`** — STEP assembly file
- **`pulsar_pcb.stl`**, **`Pulsar.step`** — the carrier board as a mesh/solid, for fit-checking only. Not manufacturing data; no board can be fabricated from them.
- **`cable_hole_plug.stl`** — Cable hole plug by [byt3swap](https://github.com/byt3swap/dreamwave-enhanced) (used with permission)

## Printing Tips

The front and back halves print best at roughly a 45-degree angle with a few supports. This gives cleaner surfaces and avoids overhangs on the curved shell edges.

## Attribution

- **Modified enclosure** (Blender files, edited STLs) by [alwaysEpic](https://github.com/alwaysEpic) — modelled against the VMU scans below. If you use or remix these, please credit the author.
- **VMU 3D scans** by [Wesk](https://bitbuilt.net/forums/threads/dreamcast-vmu-scan.3988/) — the reference this enclosure was built from.
- **Cable hole plug** by [byt3swap](https://github.com/byt3swap/dreamwave-enhanced) — used with permission

## License

These files are **not** covered by the project's GPL-3.0 licence.

The Blender sources and edited STLs are the author's own modelling work — the Pulsar cutouts, pockets, plunger, tolerances, and print splits — and are offered under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/): use, modify, and redistribute them with attribution.

That grant covers only the author's own modelling work. `cable_hole_plug.stl` is byt3swap's and is included by permission; its terms are theirs, not ours.
