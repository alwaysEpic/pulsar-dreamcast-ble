# Pin Mapping

## XIAO nRF52840 (Primary Build)

| Function | Pin | Notes |
|----------|-----|-------|
| SDCKA (Red) | P0.05 (D5) | Maple Bus clock/data A |
| SDCKB (White) | P0.03 (D1) | Maple Bus clock/data B |
| Sync Button | P1.15 (D10) | Pairing, name toggle, wake from sleep |
| Boost SHDN | P0.28 (D2) | 5V boost converter enable |
| Battery ADC | P0.31 (AIN7) | Via P0.14 enable gate |
| RGB LED | P0.26/P0.30/P0.06 | R/G/B, active low |

Both data lines need 10kΩ pull-up resistors to 3.3V. The controller is powered at 5V — signals are 3.3V TTL.

## Pulsar V1 (MapleLink carrier — XIAO nRF52840)

The `board-pulsarv1` target. Same XIAO module, different carrier: SDCKA moved to **D0**, power is
the IP5306 over I²C (no discrete boost / charger / ADC-divider), status is a 5× WS2812 bar, plus a
rumble motor. Pins reconstructed from the board netlist.

| Function | Pin | Notes |
|----------|-----|-------|
| SDCKA (Red) | P0.02 (D0) | Maple Bus clock/data A — **moved from D5** |
| SDCKB (White) | P0.03 (D1) | Maple Bus clock/data B |
| Sync Button | P1.15 (D10) | Pairing, wake from sleep |
| I²C SDA | P0.04 (D4) | IP5306 (charge / boost / gauge) |
| I²C SCL | P0.05 (D5) | IP5306 |
| IP5306 INT | P1.11 (D6) | Charge-event IRQ (unused — battery is polled) |
| WS2812 data | P0.28 (D2) | 5-LED status bar (PWM2 — PWM0 is Maple TX) |
| Rumble EN | P0.29 (D3) | → R5 → Q1 → ERM motor (PWM1) |
| Status RGB | P0.26/P0.30/P0.06 | Onboard; blue (P0.06) is the sync LED |

Both Maple lines need 10 kΩ pull-ups; the I²C lines have 5.1 kΩ pull-ups (R1/R2). Power (charge,
5 V boost, coarse 25/50/75/100 % fuel gauge) is all over I²C — there are **no** boost-SHDN /
ADC-divider / BQ25101-STAT pins as on the discrete carrier.

## nRF52840 DK (Development)

| Function | Pin | Notes |
|----------|-----|-------|
| SDCKA (Red) | P0.05 | Maple Bus clock/data A |
| SDCKB (White) | P0.06 | Maple Bus clock/data B |
| Sync Button | P0.25 | Button 4, active low |
| Sync LED | P0.13 | LED1 |
| Status LEDs | P0.14-P0.16 | LED2-LED4 |

Same pull-up requirement. The DK needs an external 5V supply for the controller (the DK only outputs 3.3V).

## Dreamcast Controller Cable

The Dreamcast controller cable has 5 pins:

| Pin | Color | Function |
|-----|-------|----------|
| 1 | Red | SDCKA (data/clock A) → nRF P0.05 |
| 2 | Blue | +5V power |
| 3 | Black | GND (cable shield ties here) |
| 4 | Green | Sense — tied to GND |
| 5 | White | SDCKB (data/clock B) → nRF P0.03 |

Pinout per the canonical Maple-bus reference ([mc.pp.se](https://mc.pp.se/dc/controller.html));
matches the KiCad symbol. The Sense line (pin 4) is tied to GND. **Wire colors vary on
third-party cables — meter your actual cable before trusting the colors.**
