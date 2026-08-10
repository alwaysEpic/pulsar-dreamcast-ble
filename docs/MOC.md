---
type: moc
created: 2026-08-06
tags: [index, public]
---

Map of content for the documentation tree. One line per doc — read this to locate a doc
rather than preloading the folder.

## Build and use

- [`users_guide.md`](users_guide.md) — what a customer needs: pairing, modes, charging
- [`flash-commands.md`](flash-commands.md) — UF2, DFU, and probe recipes; build variants
- [`bill_of_materials.md`](bill_of_materials.md) — parts for the XIAO and Pulsar v1 builds
- [`pin_mapping.md`](pin_mapping.md) — pin assignments across all three boards

## Protocol and firmware

- [`maple_bus_protocol.md`](maple_bus_protocol.md) — the Maple Bus reference, consolidated
  from three sources; the authoritative copy
- [`learnings.md`](learnings.md) — lessons from 2 Mbps bit-banged GPIO on the nRF52840,
  written to generalize beyond this project
- [`input_quality_testing.md`](input_quality_testing.md) — measuring packet loss and
  input latency; the issue #5 investigation end to end
- [`test_plan.md`](test_plan.md) — the full adapter test plan

## Power

- [`battery_optimization.md`](battery_optimization.md) — power strategy on a single-cell
  LiPo; measured active runtimes, calculated standby

## Assets

`images/`, `wiring/`, and `signal_references/` — photos, wiring diagrams, and captured
Maple Bus traces referenced by the docs above.
