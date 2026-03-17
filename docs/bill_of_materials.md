# Bill of Materials

## XIAO Build (Primary)

| Component | Quantity | Notes | Link |
|-----------|----------|-------|------|
| Seeed XIAO nRF52840 | 1 | Non-Sense version works | [Seeed Studio](https://www.seeedstudio.com/Seeed-XIAO-BLE-nRF52840-p-5201.html) |
| Dreamcast controller | 1 | Only tested with OEM controller | |
| Pololu U1V11F5 5V boost | 1 | Has SHDN pin for power gating | [Pololu](https://www.pololu.com/product/2562) |
| 1N5817 Schottky diodes | 2 | USB/boost OR circuit for 5V rail | [DigiKey](https://www.digikey.com/en/products/detail/diodes-incorporated/1N5817-T/22052) |
| 10kΩ resistors | 2 | Pull-ups for SDCKA and SDCKB | |
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
| 10kΩ resistors | 2 | Pull-ups for SDCKA and SDCKB | |
| Jumper wires | 4+ | 5V, GND, SDCKA, SDCKB | |
| 5V power supply | 1 | For controller (DK only outputs 3.3V) | |

## Optional

| Component | Notes | Link |
|-----------|-------|------|
| VMU enclosure (3D printed) | See print tips in 3d_files/ | [3d_files/README.md](../3d_files/README.md) |
