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
| 1 | Red | SDCKA (data/clock line A) |
| 2 | White | SDCKB (data/clock line B) |
| 3 | Green | GND / Sense |
| 4 | Blue | 5V power |
| 5 | — | Shield / frame ground |

Pin 3 (GND/Sense) must be connected to ground for the controller to power on.
