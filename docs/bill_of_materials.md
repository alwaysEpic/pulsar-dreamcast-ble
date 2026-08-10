# Bill of Materials

The firmware supports three boards. Two of them you can build yourself and this document
lists what to buy; the third is the designed carrier, described here so you can understand
how it works and what it changes.

| Board | Build it yourself? | Section |
|---|---|---|
| **XIAO** | Yes — the primary DIY build | [XIAO Build](#xiao-build-primary) |
| **DK** | Yes — bench/development only | [DK Build](#dk-build-development) |
| **Pulsar v1** | No — the designed carrier | [Pulsar v1](#pulsar-v1-designed-carrier) |

## XIAO Build (Primary)

| Component | Quantity | Notes | Link |
|-----------|----------|-------|------|
| Seeed XIAO nRF52840 | 1 | Non-Sense version works | [Seeed Studio](https://www.seeedstudio.com/Seeed-XIAO-BLE-nRF52840-p-5201.html) |
| Dreamcast controller | 1 | Only tested with OEM controller | |
| Pololu U1V11F5 5V boost | 1 | Has SHDN pin for power gating | [Pololu](https://www.pololu.com/product/2562) |
| 1N5817 Schottky diodes | 2 | USB/boost OR circuit for 5V rail | [DigiKey](https://www.digikey.com/en/products/detail/diodes-incorporated/1N5817-T/22052) |
| 10kΩ resistors | 2 | Pull-ups for SDCKA and SDCKB (4.7kΩ also works) | |
| LiPo battery (1000mAh) | 1 | 3.7V, JST PH 2.0mm, ~8 hr runtime | [DigiKey](https://www.digikey.com/en/products/filter/battery-packs/89) |
| Perfboard | 1 | Used: Adafruit Perma-Proto quarter-size | [Adafruit](https://www.adafruit.com/product/589) |
| Wire | — | 30 AWG or similar | |
| Dreamcast controller cable | 1 | For tapping Maple Bus lines | |

## Programming / Debug (Optional)

The XIAO can be flashed via USB using the built-in UF2 bootloader — no debug probe needed. See [flash commands](flash-commands.md) for the UF2 workflow.

The hardware below is only needed for RTT debug logging or if you need to restore the bootloader:

| Component | Quantity | Notes | Link |
|-----------|----------|-------|------|
| nRF52840 DK | 1 | Used as J-Link programmer | [Nordic](https://www.nordicsemi.com/Products/Development-hardware/nrf52840-dk) |
| SWD breakout board | 1 | 2x5 1.27mm to breadboard-friendly pins | [Adafruit](https://www.adafruit.com/product/2743) |
| SWD cable (10-pin 1.27mm) | 1 | 150mm, connects DK to breakout | [Adafruit](https://www.adafruit.com/product/1675) |

## DK Build (Development)

| Component | Quantity | Notes | Link |
|-----------|----------|-------|------|
| nRF52840 DK | 1 | Built-in J-Link, no extra programmer needed | [Nordic](https://www.nordicsemi.com/Products/Development-hardware/nrf52840-dk) |
| Dreamcast controller | 1 | | |
| 10kΩ resistors | 2 | Pull-ups for SDCKA and SDCKB (4.7kΩ also works) | |
| Jumper wires | 4+ | 5V, GND, SDCKA, SDCKB | |
| 5V power supply | 1 | For controller (DK only outputs 3.3V) | |

## Pulsar v1 (Designed Carrier)

Pulsar v1 is a purpose-built carrier PCB with a **Seeed XIAO nRF52840 mounted on it** — the
same module as the DIY build, so both run identical silicon. The carrier replaces the
hand-wired power section with integrated parts and adds two things the DIY build has no room
for. It is described here to explain the design; the fabrication package is not published, so
this is a reference rather than a shopping list.

| Function | Part | What it replaces / adds vs. the DIY build |
|---|---|---|
| MCU | Seeed XIAO nRF52840 | Same module, socketed onto the carrier instead of perfboard |
| Charge, boost, gauge | IP5306 PMIC | Replaces the Pololu boost + BQ25101 charger + ADC divider with one part. Provides the 5 V controller rail and a four-level fuel gauge over I²C |
| Status and battery | 5× WS2812 chain | Replaces the single onboard RGB. LED 0 is status, LEDs 1–4 are the battery gauge |
| Haptics | ERM motor via transistor | New — the DIY build has no rumble |
| Controller | JST PH 2.0 mm, 5-pin | Maple cable: SDCKA, SDCKB, 5 V, ground |
| Battery | JST PH 2.0 mm, 2-pin | LiPo cell. PH rather than a smaller series because charge and boost currents exceed the 1 A rating of the finer pitches |
| Rumble motor | JST PH 2.0 mm, 2-pin | Motor lead, with a flyback diode |

What this changes in practice, versus the XIAO build: a four-level gauge instead of a
continuous voltage estimate, a battery bar instead of a status-only LED, working rumble, and
wireless firmware updates instead of USB drag-and-drop. Everything about how it plays is the
same. Pin assignments for all three boards are in [pin mapping](pin_mapping.md).

## Tools & Supplies

| Item | Notes |
|------|-------|
| Soldering iron + solder | For perfboard assembly and wire connections |
| Wire strippers | For 30 AWG wire |
| Hi-temp masking tape (Kapton) | Useful for insulating connections and holding parts during assembly |
| Multimeter | For verifying connections and checking voltages |
| Heat shrink tubing | For insulating solder joints on the controller cable |

## Optional

| Component | Notes | Link |
|-----------|-------|------|
| VMU enclosure (3D printed) | See print tips in 3d_files/ | [3d_files/README.md](../3d_files/README.md) |
