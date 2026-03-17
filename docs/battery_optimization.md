# Battery & Power Optimization

Power management strategy for the XIAO nRF52840 Dreamcast BLE adapter running on a 1000mAh LiPo.

---

## Power Budget

| State | Current Draw | Notes |
|-------|-------------|-------|
| Active gaming | ~120 mA | Boost converter + controller + BLE radio | - non tested
| BLE advertising (slow) | ~0.5-2 mA | 500ms interval, boost off | - non tested
| System Off | ~5-8 µA | QSPI flash in DPD, pins disconnected | - from specs

With a 1000mAh battery, expect ~8 hours of active gaming at the estimated ~120 mA draw. System Off standby lasts months.

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
| **This project** | **1000 mAh** | **~8 hr** | **~120 mA** |

Our draw is dominated by the 5V boost converter + Dreamcast controller (~60-80 mA). The nRF52840 + BLE radio is only ~15 mA of the total.

---

## Possible Next Steps

- **Slave latency** — Setting BLE slave_latency to 2-4 could save ~200-300 µA during idle connected periods. Deferred due to compatibility risk with iBlueControlMod.
- **Dedicated PCB** — Eliminates perfboard losses and enables SMD components: TPS61099x50 boost converter (~90% efficiency, 5µA quiescent vs 50µA on current Pololu), BAT54 Schottky diodes (~0.23V drop vs 0.3-0.4V), and integrated charging circuit.
