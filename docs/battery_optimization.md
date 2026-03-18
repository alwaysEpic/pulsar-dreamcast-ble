# Battery & Power Optimization

Power management strategy for the XIAO nRF52840 Dreamcast BLE adapter running on a single-cell LiPo (tested with 500mAh, recommended 1000mAh).

---

## Power Budget

| State | Current Draw | Source | Notes |
|-------|-------------|--------|-------|
| Active gaming | ~57-67 mA | Measured (FNB58) | Boost converter + controller + BLE radio |
| BLE advertising (slow) | ~0.5-2 mA | Estimated | 500ms interval, boost off |
| System Off | ~5-8 µA | Datasheet | QSPI flash in DPD, pins disconnected |

### Measured Battery Life (500mAh LiPo)

Tested with FNB58 USB power meter, active BLE connection with occasional controller input:

- **Idle connected draw:** ~57 mA
- **Active input draw:** ~67 mA
- **Sleep drain:** Negligible (~2% overnight, likely self-discharge)

Testing in progress — preliminary results suggest ~7-8 hours on 500mAh. Battery life estimates will be updated with more complete discharge data.

### Estimated Battery Life by Capacity

| Battery | Active Gaming | Sleep Standby |
|---------|--------------|---------------|
| 500 mAh | ~7-8 hours (preliminary) | Months |
| 1000 mAh | ~14-16 hours (estimated) | Months |

Note: The LiPo discharge curve is nonlinear. The battery spends a long time in the 3.7-3.9V plateau then drops quickly below 3.5V. Actual runtime may vary with controller usage intensity.

---

## Battery Percentage Estimation

Voltage-based percentage using a 10-point LiPo discharge curve lookup table with linear interpolation between entries. This is the industry standard approach for embedded devices — Xbox controllers only report 4 discrete levels, and projects like Meshtastic use similar tables.

### Lookup Table

| Voltage | Percentage |
|---------|-----------|
| 4200 mV | 100% |
| 4100 mV | 90% |
| 4000 mV | 80% |
| 3900 mV | 60% |
| 3800 mV | 40% |
| 3700 mV | 30% |
| 3600 mV | 20% |
| 3500 mV | 10% |
| 3400 mV | 5% |
| 3300 mV | 0% |

- **0% = 3300mV** — the battery protection circuit shuts down at this voltage under load (measured empirically: device dies at ~3.3V)
- **8x SAADC oversampling** — hardware-averaged ADC reads for noise reduction
- **Monotonic decrease** — reported percentage never increases unless USB charging is detected, eliminating confusing voltage-recovery bounces after sleep or load changes
- **60-second read interval** — uses `Instant` comparison for drift-free timing

### Accuracy

Realistic accuracy is +/-10-15% in the flat middle region (3.7-3.9V), better at extremes. This is consistent with other voltage-based approaches and adequate for a battery indicator. A fuel gauge IC (e.g., MAX17048 ~$1.50) would improve to +/-5% but only makes sense on a custom PCB.

---

## Optimizations Implemented

### 1. Boost Gating on BLE Connection (saves ~60-80 mA idle)

The 5V boost converter + Dreamcast controller draws 60-80 mA and serves no purpose when there's no BLE host connected. The boot flow now:

1. Boot → BLE advertise (boost OFF, no controller polling)
2. BLE connects → enable boost → detect controller → start polling
3. BLE disconnects → disable boost → stop polling → advertise again
4. Timeout → System Off

### 2. QSPI Flash Deep Power Down (saves ~2-5 mA in all states)

The XIAO's P25Q16H QSPI flash draws several mA in standby. We don't use it. At startup, a GPIO bit-bang SPI sequence sends the DPD command (0xB9), then disconnects the QSPI pins. CS (P0.25) stays driven HIGH to prevent accidental wake from bus noise.

This was the root cause of unexpectedly high System Off current (milliamps instead of microamps).

### 3. REG1 DCDC Enable (free efficiency gain)

The XIAO has the inductor for REG1 DCDC (confirmed via Zephyr devicetree). Enabling it (`config.dcdc.reg1 = true`) saves ~2-3 mA during radio TX/RX. REG0 is NOT enabled (VDDH tied to VDD, no inductor).

### 4. Pin Disconnect in Sleep States

GPIO state survives System Off on the nRF52840. Every unused pin is explicitly disconnected (input, no pull, Hi-Z) before entering System Off to prevent current leakage. Eight pins are disconnected; three are kept driven (QSPI CS HIGH, boost SHDN LOW, charge ISET LOW).

During BLE advertising (boost off), Maple Bus pins are also disconnected via `MapleBus::set_low_power()`. The external 10kΩ pull-ups hold both lines at 3.3V with zero current.

### 5. HighDrive Mode for TX

`OutputDrive::HighDrive` gives ~121ns rise time vs ~363ns standard drive. No power cost — purely signal quality improvement that enables reliable communication and potentially allows higher-resistance pull-ups in the future.

### 6. USB VBUS Detection

When USB power is present, the controller runs directly from USB 5V via Schottky diode OR circuit (2x 1N5817). The boost converter stays off and the battery charges at 100mA with nothing drawing from it. Tethered play is effectively free.

### 7. Tiered Advertising

| Phase | Interval | Duration | Purpose |
|-------|----------|----------|---------|
| Fast reconnect | 20 ms | 5 seconds | Instant reconnect to bonded host |
| Slow reconnect | 500 ms | 55 seconds | Low-power reconnect window |
| Sync mode | 20 ms | 60 seconds | Active pairing (user-initiated) |
| Timeout | — | — | System Off after all phases expire |

### 8. Controller Detection Timeout

If BLE connects but no controller is found within 60 seconds, the device enters System Off. This prevents the overnight drain scenario where a host auto-reconnects via BLE with no controller plugged in, leaving the boost converter running indefinitely.

### 9. Inactivity Sleep

After 10 minutes with no BLE connection (controller disconnected, host gone), the device enters System Off. Wake via sync button press (GPIO SENSE).

---

## Hardware: USB 5V Passthrough

Two 1N5817 Schottky diodes in an OR configuration route either USB VBUS or boost output to the controller 5V rail. When USB is present, the higher voltage wins (~4.7V after diode drop) and the boost shuts down. Firmware detects USB via the nRF52840 `POWER.USBREGSTATUS` register.

---

## Commercial Comparison

| Controller | Battery | Life | Avg Draw |
|-----------|---------|------|----------|
| Xbox One S | 2x AA (~2400 mAh) | 40 hr | ~60 mA |
| DualSense | 1560 mAh | 6-12 hr | ~100 mA |
| Switch Pro | 1300 mAh | 40 hr | ~32 mA |
| 8BitDo Pro 2 | 1000 mAh | 20 hr | ~50 mA |
| **This project** | **500 mAh** | **~7-8 hr** | **~60 mA** |
| **This project** | **1000 mAh** | **~14-16 hr** | **~60 mA** |

Our draw is dominated by the 5V boost converter + Dreamcast controller (~45 mA). The nRF52840 + BLE radio is only ~15 mA of the total.

---

### 10. RTT Feature Gate (saves flash size + minor power)

RTT debug logging is gated behind an `rtt` Cargo feature. DK builds always include it. XIAO production builds omit it — all `log!()` calls compile to nothing, reducing binary size and eliminating string formatting overhead. Development builds opt in with `--features board-xiao,rtt`.

### 11. Flash-Based Panic Logging

On panic, the firmware writes the panic message to a dedicated flash page (`0xFC000`) using raw NVMC register writes (no SoftDevice dependency), then resets. On the next boot with RTT enabled, the stored panic is printed and the page is cleared. This replaces the silent `panic-reset` behavior with something debuggable.

---

## Possible Next Steps

- **Slave latency** — Setting BLE slave_latency to 2-4 could save ~200-300 µA during idle connected periods. Deferred due to compatibility risk with iBlueControlMod. Only effective if combined with skipping notifications when state is unchanged, which may break Xbox HID compatibility.
- **Dedicated PCB** — Eliminates perfboard losses and enables SMD components: TPS61099x50 boost converter (~90% efficiency, 5µA quiescent vs 50µA on current Pololu), BAT54 Schottky diodes (~0.23V drop vs 0.3-0.4V), and integrated charging circuit.
- **Fuel gauge IC** — MAX17048 (~$1.50, I2C) for +/-5% battery accuracy vs current +/-10-15%. Only practical on a custom PCB.
- **TX power reduction** — Lowering BLE TX from 0dBm to -4dBm would save ~1mA with no perceptible range impact at gamepad distances. Deferred pending range testing through plastic enclosure.
