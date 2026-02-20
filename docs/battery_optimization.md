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

1. **Disable boost when BLE disconnected** — biggest win, ~60-80 mA saved during idle — **DONE**
2. **Enable REG1 DCDC** — one line change, free efficiency — **DONE**
3. **QSPI flash DPD** — potentially ~2-5 mA savings
4. ~~Slave latency~~ — deferred for compatibility
5. ~~TX power reduction~~ — default 0 dBm is already reasonable

---

## Decisions Log

### Pull-Up Resistors — 2026-02-20

**Current:** External 4.7kΩ on SDCKA and SDCKB, internal pull-ups disabled.

**Problem:** SDCKB idle state is LOW, so 3.3V/4.7kΩ = 0.70 mA wasted continuously through
pull-up. This persists during BLE advertising (~5% of idle draw) and even during System Off
(GPIO state survives System Off on nRF52840 — 140x more than the ~5 uA sleep target).

**Decisions:**
- **Disconnect Maple Bus pins when not polling** — set to disconnected input via
  `set_as_disconnected()`. Pull-ups hold both lines at 3.3V, zero current. Saves 0.70 mA
  during advertising and System Off. **DO THIS (firmware).**
- **Disconnect pins before System Off** — add raw register writes to `enter_system_off()`
  for SDCKA (P0.05), SDCKB (P0.03), and charge STAT (P0.17, saves 0.25 mA if charging).
  MapleBus objects aren't accessible there so use raw PIN_CNF writes (value 0x00000002 =
  disconnected input, no pull). **DO THIS (firmware).**
- **Switch TX to HighDrive mode** — `OutputDrive::HighDrive` gives ~121ns rise time vs ~363ns
  standard. Free signal quality improvement, no power cost. **DO THIS (firmware).**
- **Test 6.8kΩ and 10kΩ on DK** — could save 0.21-0.37 mA during active polling. Time to VIH
  at 10kΩ is ~1.2 µs, which should fit in the 50-160 µs turnaround gap but needs real testing.
  **TODO: hardware test on DK breadboard.**
- ~~Internal pull-ups only~~ — tested during initial development, inconsistent results.
  13kΩ is too weak and varies across temperature. **REJECTED.**

### USB 5V Passthrough — 2026-02-20

**Problem:** When USB is connected, power path is USB→charger→battery→boost→controller
(~74% combined efficiency). Charger puts in 100 mA, boost pulls out ~124 mA. Battery drains
even on USB.

**Solution:** OR the USB VBUS (5V) directly to the controller 5V rail via Schottky diodes.
When USB is present, controller runs from VBUS (~4.7V after diode drop). Boost shuts down.
Battery charges at 100 mA with nothing drawing from it.

**Perfboard version (now):**
- 2x 1N5817 (1A through-hole Schottky, ~0.3-0.4V drop at 80mA)
- Cathodes joined → 5V controller rail
- Anode 1 ← USB VBUS (5V from USB-C connector)
- Anode 2 ← Pololu boost output
- Dreamcast controller should tolerate 4.7V (5V spec, 3.3V logic) — **needs testing**

**PCB version (future board):**
- 2x BAT54 or PMEG3010 (SOT-23 SMD Schottky, ~0.23-0.3V drop)
- Same OR topology, smaller footprint

**Firmware:**
- Detect USB presence — can infer from BQ25101 STAT pin (LOW = charging = USB present)
- Caveat: STAT goes HIGH when battery is full even with USB connected. May need a separate
  VBUS sense GPIO (voltage divider on VBUS → ADC pin) for reliable detection.
- When USB detected: shut down boost (SHDN LOW), controller runs from VBUS passthrough
- When USB removed: enable boost normally

**Impact:** Tethered play is free — battery charges while playing. Battery-only mode unchanged.
**TODO: hardware mod (diodes) + firmware VBUS detection + test controller at 4.7V.**

### Slow Reconnect Advertising — 2026-02-20

**Current:** After 5s fast reconnect (20ms), slow advertising runs at 100ms (~30 uA) until
connection or 60s timeout.

**Change:** Increase slow reconnect interval from 100ms (160 units) to 500ms (800 units).
Bonded hosts still reconnect within 2-4 seconds. SyncMode (active pairing) stays at 20ms.

**Impact:** ~22 uA savings during slow reconnect phase. Small but free.
**DO THIS (firmware, one-line change).**

### HighDrive Mode for Maple Bus TX — 2026-02-20

**Change:** Switch `OutputDrive::Standard` to `OutputDrive::HighDrive` in `gpio_bus.rs` for
all SDCKA/SDCKB output mode calls. Drops output impedance from ~1.65kΩ to ~550Ω, giving
~121ns rise time vs ~363ns (at 100pF cable load). Well within 250ns phase window.

**Impact:** No power savings — purely signal quality. Cleaner edges, more reliable comms,
and enables future move to higher-resistance pull-ups.
**DO THIS (firmware, trivial change).**

### Boost Converter Upgrade — 2026-02-20

**Current:** Pololu U1V11F5 (TPS61201), ~87% efficiency, 50 uA quiescent, 1 uA shutdown.

**Better option for PCB:** TPS61099x50 (TI), SOT-23-5, ~90% efficiency, 5 uA quiescent,
0.01 uA shutdown. Auto PFM/PWM mode switching. ~$1.50.

**Impact:** ~4 mA savings during active gaming (124 mA → 120 mA from battery).
Not worth reworking perfboard. **DEFERRED to PCB build.**

### QSPI Flash Deep Power Down — 2026-02-20

**Current:** P25Q16H 2MB flash on XIAO is hardwired to 3.3V, sitting in standby drawing
2-5 mA. We never use it (bonds/prefs use internal flash). This is likely the reason System Off
current is milliamps instead of the expected ~5 uA.

**Solution:** Send DPD command (0xB9) via QSPI at startup, then shut down the peripheral.
Keep CS (P0.25) driven HIGH to prevent the flash from accidentally waking up (known Zephyr
issue — floating CS can glitch LOW and the flash interprets bus noise as a Release command).
Disconnect the other 5 QSPI pins (P0.20-P0.24, P0.21).

**Implementation:**
- Raw register writes (no embassy `qspi` feature needed)
- Called once at startup: after `embassy_nrf::init()`, before SoftDevice init
- CS stays driven HIGH for the rest of the session (zero current, both ends at 3.3V)
- Flash stays in DPD through System Off (latched inside flash chip)
- On fresh boot after wake, DPD is sent again (flash resets to standby on power cycle)

**Impact:** Saves 2-5 mA in ALL power states. System Off drops from ~2-5 mA to ~5-8 uA.
**DO THIS (firmware).**

---

## Future PCB Build — Parts List

Components for a dedicated PCB replacing the XIAO perfboard setup.

| Component | Part | Package | Purpose | Notes |
|-----------|------|---------|---------|-------|
| MCU | nRF52840 | QFN-73 | Main controller + BLE | Or use nRF52840 module (E73, MDBT50Q) |
| Boost converter | TPS61099x50 | SOT-23-5 | 3.7V→5V, 200mA max | 5µA Iq, 90% eff, ~$1.50 |
| Schottky diodes (x2) | BAT54 or PMEG3010 | SOT-23 | USB/boost OR for 5V rail | 0.23-0.3V drop |
| LiPo charger | BQ25101 | - | 100mA charge, keep current design | Or MCP73831 for simpler layout |
| Pull-ups (x2) | TBD (4.7kΩ-10kΩ) | 0402/0603 | Maple Bus data lines | Pending DK resistance testing |
| DCDC inductor | 10µH | 0603+ | REG1 DCDC (if bare nRF52840) | Not needed if using module with inductor |

---

## Slave Latency — 2026-02-20

**Current:** slave_latency=0, radio wakes every ~10ms even when idle. Costs ~300-500 uA.

**Opportunity:** slave_latency=2-4 could save ~200-300 uA during idle connected periods.
Device skips connection events when no input changes, but responds instantly when buttons
are pressed.

**Risk:** iBlueControlMod may not handle skipped events. Xbox controller uses latency=0.

**Decision:** **DEFERRED.** Test with iBlueControlMod when available. If compatible,
slave_latency=2 is a safe starting point.

---

## Implementation Summary — 2026-02-20

### Firmware changes (no hardware required):
1. **QSPI flash DPD** — raw register writes at startup, saves 2-5 mA always
2. **Pin disconnect when not polling** — `MapleBus::set_low_power()`, saves 0.7 mA idle
3. **Pin disconnect before System Off** — raw writes in `enter_system_off()`, saves 0.7 mA sleep
4. **HighDrive mode for TX** — `OutputDrive::HighDrive` in gpio_bus.rs, signal quality
5. **Slow reconnect advertising** — 100ms → 500ms interval, saves ~22 uA

Already done:
- Boost gating on BLE connection (saves ~60-80 mA idle)
- REG1 DCDC enable (free efficiency)

### Hardware changes (pending):
- **USB 5V passthrough** — 2x 1N5817 Schottky diodes + firmware VBUS detection
- **Pull-up resistance testing** — test 6.8kΩ and 10kΩ on DK breadboard

### Deferred to PCB build:
- Boost converter upgrade (TPS61099x50)
- Slave latency testing (needs iBlueControlMod)

### Projected Power Budget After All Firmware Changes

| State | Before | After | Savings |
|-------|--------|-------|---------|
| Active gaming | ~122 mA | ~118 mA | ~4 mA (QSPI) |
| BLE advertising | ~17 mA | ~12 mA | ~5 mA (QSPI + pin disconnect) |
| System Off | ~2-5 mA (!) | ~5-8 uA | ~2-5 mA (QSPI was the hidden drain) |
