# 3D Models & Enclosure Files

This directory contains 3D models for a VMU-shaped enclosure for the Dreamcast BLE adapter.

## Contents

- **`blender_files/`** — Blender source files for the modified VMU enclosure (by alwaysEpic)
- **`Edited_VMU_stls/`** — Modified STL exports ready for printing (by alwaysEpic)
- **`VMU_ShellAssembly.step`** — STEP assembly file
- **`pulsar_pcb.stl`**, **`Pulsar.step`** — the carrier board as a mesh/solid, for fit-checking only. Not manufacturing data; no board can be fabricated from them.
- **`cable_hole_plug.stl`** — Cable hole plug by [byt3swap](https://github.com/byt3swap/dreamwave-enhanced) (used with permission)

The VMU scan archives this work was modelled against are not redistributed here. Get them from the original thread linked under Attribution.

## Printing Tips

The front and back halves print best at roughly a 45-degree angle with a few supports. This gives cleaner surfaces and avoids overhangs on the curved shell edges.

## Attribution

- **Modified enclosure** (Blender files, edited STLs) by [alwaysEpic](https://github.com/alwaysEpic) — modelled against the VMU scans below. If you use or remix these, please credit the author.
- **VMU 3D scans** by [Wesk](https://bitbuilt.net/forums/threads/dreamcast-vmu-scan.3988/) — the reference this enclosure was built from. Wesk published them as reference material without stating licence terms; they are not redistributed here.
- **Cable hole plug** by [byt3swap](https://github.com/byt3swap/dreamwave-enhanced) — used with permission

## License

These files are **not** covered by the project's GPL-3.0 licence.

The Blender sources and edited STLs are the author's own modelling work — the Pulsar cutouts, pockets, plunger, tolerances, and print splits — and are offered under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/): use, modify, and redistribute them with attribution.

That grant covers only the author's own modelling work. `cable_hole_plug.stl` is byt3swap's and is included by permission; its terms are theirs, not ours.
