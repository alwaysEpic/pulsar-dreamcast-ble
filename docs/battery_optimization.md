# Battery Optimization Research

## Current Power Budget

| Component | Current Draw | Notes |
|-----------|-------------|-------|
| Boost converter + controller | ~60-80 mA | 5V rail via Pololu, powers Dreamcast controller |
| nRF52840 + BLE radio | ~5-15 mA | Depends on TX power and connection interval |
| Maple Bus polling | ~5-10 mA | Continuous GPIO sampling at 60 Hz |
| LEDs (each, when on) | ~5-10 mA | RGB active-low, turned off during sleep |
| **Total active** | **~120 mA** | Dominates battery life |
| System Off | ~2-5 uA | GPIO SENSE wake configured |

**Critical problem:** BQ25101 charges at 100 mA, but system draws ~120 mA. Battery drains
even when plugged into USB. Can only charge during System Off or with controller disconnected.

---

## Optimization Opportunities (Ordered by Impact)

### 1. Disable Boost When BLE Disconnected (~60-80 mA savings)

**Biggest single win.** The 5V boost converter + Dreamcast controller draws 60-80 mA and
serves no purpose when there's no BLE host to receive HID reports.

**Implementation:** Restructure boot flow:
1. Boot -> BLE advertise (boost OFF, no controller polling)
2. BLE connects -> enable boost -> detect controller -> start polling
3. BLE disconnects -> disable boost -> stop polling -> advertise again
4. Inactivity/timeout -> System Off

During advertising/reconnect, system draws only ~5-15 mA instead of ~120 mA.

### 2. Enable REG1 DCDC Converter (free efficiency gain)

The XIAO nRF52840 has the inductor for REG1 DCDC (confirmed via Zephyr devicetree
`xiao_ble_common.dtsi`). Currently running on the less efficient LDO.

**Implementation:** Add `config.dcdc.reg1 = true` in `main.rs` embassy init.

- Do NOT enable REG0 (no inductor on XIAO, VDDH tied to VDD)
- DCDC is most beneficial when radio is active (saves ~2-3 mA during TX/RX)
- At idle, the chip auto-switches to LDO refresh mode regardless

### 3. Put QSPI Flash into Deep Power Down (~2-5 mA savings)

The XIAO has a P25Q16H QSPI flash that draws several mA in standby. We don't use it.
Seeed forum users report this as the single biggest idle power reduction.

**Implementation:** Send DPD command (0xB9) to flash via QSPI at startup.
Flash supports DPD with 3 us entry time, 8 us exit time.

### 4. Reduce TX Power (up to ~3 mA savings)

| TX Power | Current (1 Mbps BLE) |
|----------|---------------------|
| +8 dBm | ~14 mA |
| +4 dBm | ~9 mA |
| 0 dBm | ~5 mA (default) |
| -4 dBm | ~4 mA |

For a gamepad adapter sitting near a console, 0 dBm (default) is probably fine.
Could drop to -4 dBm if range is never an issue.

### 5. BLE Slave Latency (potential ~5-10x idle savings)

Currently `slave_latency: 0` (respond to every connection event). Setting to 4-10 would
let the device skip events when no new data is available.

**Risk:** iBlueControlMod or other BLE hosts may not handle latency well. Xbox controller
uses slave_latency=0. **Deferred for now** — compatibility risk outweighs savings.

---

## Commercial Controller Comparison

| Controller | Battery | Life | Avg Draw |
|-----------|---------|------|----------|
| Xbox One S | 2x AA (~2400 mAh) | 40 hr | ~60 mA |
| Xbox Elite 2 | ~1800 mAh Li-ion | 40 hr | ~45 mA |
| DualShock 4 | 1000 mAh Li-ion | 4-8 hr | ~75-85 mA |
| DualSense | 1560 mAh Li-ion | 6-12 hr | ~100 mA |
| Switch Pro | 1300 mAh Li-ion | 40 hr | ~32 mA |
| 8BitDo SN30 Pro | 480 mAh Li-ion | 18 hr | ~27 mA |
| 8BitDo Pro 2 | 1000 mAh Li-ion | 20 hr | ~50 mA |

**Nintendo Switch Pro Controller is the gold standard:** 40 hours from 1300 mAh (~32 mA avg).
Achieves this with no analog triggers, no lightbar, no speaker, minimal features.

### Auto Power-Off Timeouts
- Xbox: 15 minutes
- PlayStation: 10/30/60 minutes (configurable)
- 8BitDo: 5-15 minutes
- Our project: 10 minutes (reasonable)

---

## Battery Life Projections for This Project

### Current State (boost always on): ~120 mA total
| Battery | Life |
|---------|------|
| 500 mAh | ~4 hours |

### After Optimization (boost off when disconnected): ~15 mA advertising, ~120 mA active
With typical usage (50% active, 50% idle/advertising):
| Battery | Life |
|---------|------|
| 500 mAh | ~7-8 hours |

### If Controller Powered Separately: ~10-20 mA active
| Battery | Life |
|---------|------|
| 500 mAh | ~25-50 hours |

---

## nRF52840 Power Modes Reference

| Mode | Current | Notes |
|------|---------|-------|
| System OFF | ~0.4 uA | No RAM retention |
| System OFF + full RAM | ~1.86 uA | All 256 KB retained |
| System ON idle (LDO) | ~1.5 uA min | LFCLK + RTC running |
| System ON idle (const latency) | ~500 uA+ | HFCLK kept running |
| BLE advertising | ~5-10 mA avg | Depends on interval |
| BLE connected (idle) | ~0.5-2 mA | With slave latency |
| BLE connected (active reports) | ~5-15 mA | Radio TX + MCU |

Embassy executor uses WFE between tasks, auto-entering System ON low-power sub-mode.
No additional code needed for idle power savings.

---

## XIAO-Specific Notes

- **REG1 DCDC: YES** (inductor present, confirmed via Zephyr DTS)
- **REG0 DCDC: NO** (VDDH and VDD tied together, no inductor)
- **QSPI Flash: P25Q16H** — put into DPD mode at startup
- **BQ25101 charger:** 100 mA charge rate, quiescent ~1 uA
- **Charge indicator:** STAT pin on P0.17 (LOW = charging, HIGH = done/not charging)
- **3.3V regulator:** Some quiescent current, unavoidable
- **Battery ADC:** P0.31 via 1M+510K divider, P0.14 enable (active LOW)

---

## Implementation Priority

1. **Disable boost when BLE disconnected** — biggest win, ~60-80 mA saved during idle
2. **Enable REG1 DCDC** — one line change, free efficiency
3. **QSPI flash DPD** — potentially ~2-5 mA savings
4. ~~Slave latency~~ — deferred for compatibility
5. ~~TX power reduction~~ — default 0 dBm is already reasonable
